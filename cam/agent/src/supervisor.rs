//! Surveillance des processus (serveur RTSP, chaîne d'encodage) : lancement, fin de la sortie
//! d'erreur gardée pour l'état, redémarrage avec un délai croissant, relance sur demande.

use anyhow::Result;
use homesmart_cam_proto::ProcStatus;
use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Notify;

/// Commande à lancer, recalculée à chaque démarrage (les réglages ont pu changer).
/// `Ok(None)` : rien à lancer pour l'instant.
pub type Build = Arc<dyn Fn() -> Result<Option<Vec<String>>> + Send + Sync>;

/// Lignes de sortie gardées pour expliquer un arrêt.
const TAIL: usize = 12;

#[derive(Default)]
struct State {
    running: bool,
    restarts: u32,
    started: Option<Instant>,
    last_error: Option<String>,
}

/// Un processus surveillé.
pub struct Proc {
    pub name: &'static str,
    state: Mutex<State>,
    restart: Notify,
}

impl Proc {
    pub fn status(&self) -> ProcStatus {
        let s = self.state.lock().unwrap();
        ProcStatus { running: s.running, restarts: s.restarts, since_s: s.started.filter(|_| s.running).map(|t| t.elapsed().as_secs()), last_error: s.last_error.clone() }
    }

    /// Arrête le processus et le relance aussitôt (nouveaux réglages).
    pub fn restart(&self) {
        self.restart.notify_one();
    }
}

/// Lance la surveillance ; `wait_port` : port local à attendre avant chaque démarrage.
pub fn spawn(name: &'static str, build: Build, wait_port: Option<u16>) -> Arc<Proc> {
    let proc = Arc::new(Proc { name, state: Mutex::default(), restart: Notify::new() });
    let p = proc.clone();
    tokio::spawn(async move { supervise(p, build, wait_port).await });
    proc
}

async fn supervise(p: Arc<Proc>, build: Build, wait_port: Option<u16>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let argv = match build() {
            Ok(Some(a)) if !a.is_empty() => a,
            Ok(_) => {
                p.restart.notified().await;
                continue;
            }
            Err(e) => {
                p.state.lock().unwrap().last_error = Some(format!("{e:#}"));
                tracing::warn!("{} : {e:#}", p.name);
                wait_or_restart(&p, Duration::from_secs(10)).await;
                continue;
            }
        };
        if let Some(port) = wait_port {
            wait_for_port(port, Duration::from_secs(15)).await;
        }
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("lancement de {} impossible : {e}", argv[0]);
                tracing::warn!("{} : {msg}", p.name);
                p.state.lock().unwrap().last_error = Some(msg);
                wait_or_restart(&p, backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        tracing::info!("{} démarré : {}", p.name, argv.join(" "));
        let tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
        for out in [child.stdout.take().map(|o| Box::new(o) as Box<dyn tokio::io::AsyncRead + Send + Unpin>), child.stderr.take().map(|e| Box::new(e) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)].into_iter().flatten() {
            let (tail, name) = (tail.clone(), p.name);
            tokio::spawn(async move {
                let mut lines = BufReader::new(out).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!("{name} : {line}");
                    let mut t = tail.lock().unwrap();
                    if t.len() == TAIL {
                        t.pop_front();
                    }
                    t.push_back(line);
                }
            });
        }
        let started = Instant::now();
        {
            let mut s = p.state.lock().unwrap();
            (s.running, s.started) = (true, Some(started));
        }
        let asked = tokio::select! {
            r = child.wait() => {
                let code = r.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
                // Laisse les lecteurs vider les dernières lignes.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let last = tail.lock().unwrap().iter().rev().find(|l| !l.trim().is_empty()).cloned();
                let msg = match last {
                    Some(l) => format!("arrêté ({code}) : {l}"),
                    None => format!("arrêté ({code})"),
                };
                tracing::warn!("{} {msg}", p.name);
                let mut s = p.state.lock().unwrap();
                (s.running, s.last_error) = (false, Some(msg));
                s.restarts += 1;
                false
            }
            _ = p.restart.notified() => {
                let _ = child.kill().await;
                let mut s = p.state.lock().unwrap();
                (s.running, s.last_error) = (false, None);
                true
            }
        };
        if asked {
            backoff = Duration::from_secs(1);
            continue;
        }
        backoff = if started.elapsed() > Duration::from_secs(60) { Duration::from_secs(1) } else { (backoff * 2).min(Duration::from_secs(30)) };
        wait_or_restart(&p, backoff).await;
    }
}

async fn wait_or_restart(p: &Proc, d: Duration) {
    tokio::select! {
        _ = tokio::time::sleep(d) => {}
        _ = p.restart.notified() => {}
    }
}

/// Attend qu'un port local accepte les connexions (le serveur RTSP avant la chaîne d'encodage).
pub async fn wait_for_port(port: u16, max: Duration) -> bool {
    let until = Instant::now() + max;
    while Instant::now() < until {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un processus qui s'arrête est relancé, et son dernier message est gardé.
    #[tokio::test]
    async fn restarts_and_keeps_the_last_error() {
        let argv: Vec<String> = if cfg!(windows) { vec!["cmd".into(), "/C".into(), "echo plus de capteur 1>&2 & exit 3".into()] } else { vec!["sh".into(), "-c".into(), "echo plus de capteur >&2; exit 3".into()] };
        let p = spawn("essai", Arc::new(move || Ok(Some(argv.clone()))), None);
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let s = p.status();
            if s.restarts >= 2 {
                let e = s.last_error.unwrap();
                assert!(e.contains("plus de capteur") && e.contains('3'), "{e}");
                break;
            }
            assert!(Instant::now() < until, "pas relancé : {s:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn a_missing_program_is_reported() {
        let p = spawn("absent", Arc::new(|| Ok(Some(vec!["programme-qui-n-existe-pas-hscam".into()]))), None);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let s = p.status();
        assert!(!s.running && s.last_error.unwrap().contains("lancement"));
    }
}
