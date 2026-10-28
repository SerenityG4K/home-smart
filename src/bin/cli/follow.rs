//! `homesmart-cli cam follow …` : essai du suivi automatique d'une personne par une caméra
//! motorisée (`homesmart::vision::autotrack`). Chiffres seulement : boîte, commande envoyée,
//! écart au centre. Chaque mouvement est inscrit au journal (source « Ligne de commande ») et,
//! à la fin, la caméra revient à sa position de départ (sauf `--stay`).
//!
//! - vraie caméra : RTMO et suivi des pistes sur le flux fluide, comme l'application ;
//! - `--sim` : caméra simulée (vrai serveur ONVIF) et personne virtuelle, pour mesurer le temps
//!   de centrage, le dépassement et l'absence d'oscillation ;
//! - `--virtual X` : vraie caméra, personne virtuelle immobile dans la pièce, placée dans l'image
//!   d'après le glissement mesuré de l'image : le vrai retard et le vrai moteur, sans personne ;
//! - `--calibrate` : réponse de la caméra, sans personne (retard, glissement de l'image à une
//!   vitesse donnée, sens des axes), mesurée sur le glissement de l'image entière ;
//! - `--shift-px N` : petit recadrage mesuré sur l'image (rattrape un décalage laissé par des essais).
//!
//! Sur l'Imou Ranger 2C de la chambre, `--no-tilt` est indispensable : tout mouvement qui
//! comporte une part d'inclinaison y est ignoré (même l'orientation ne bouge plus).

use super::jlog;
use anyhow::{anyhow, bail, Result};
use clap::Args;
use homesmart::camera::{onvif::Onvif, sim, stream, CameraConfig, Frame, Quality};
use homesmart::journal::Level;
use homesmart::vision::autotrack::{direction, home_nudge, AutoTrackEvent, AutoTrackSettings, AutoTracker, Phase, Profiles, PtzCommand};
use homesmart::vision::engine::Accel;
use homesmart::vision::geom::BBox;
use homesmart::vision::onnx::RtmoPersons;
use homesmart::vision::pipeline::{PersonDetector, Pipeline};
use homesmart::vision::track::{Detection, Tracker};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

#[derive(Args, Clone)]
pub struct FollowArgs {
    /// Caméra (nom, IP ou identifiant) ; sans objet avec --sim
    pub camera: Option<String>,
    /// Durée de l'essai (secondes)
    #[arg(long, default_value_t = 30)]
    pub secs: u64,
    /// Vitesse maximale envoyée (0.1 à 1)
    #[arg(long, default_value_t = 0.3)]
    pub speed: f32,
    /// Caméra simulée et personne virtuelle (aucun matériel)
    #[arg(long)]
    pub sim: bool,
    /// Vraie caméra, personne virtuelle immobile dans la pièce à X largeur(s) d'image du centre
    /// (ex. 0.25) : sa place dans l'image suit le glissement mesuré de l'image. Présente de 1 s
    /// à la moitié de l'essai ; éprouve le vrai retard et le vrai moteur sans personne devant
    #[arg(long, allow_hyphen_values = true)]
    pub r#virtual: Option<f32>,
    /// Mesure la réponse de la caméra, sans personne : retard, glissement de l'image, sens des axes
    #[arg(long)]
    pub calibrate: bool,
    /// Axe mesuré par --calibrate : pan, tilt, both (les deux) ou diag (les deux à la fois)
    #[arg(long, default_value = "both")]
    pub axis: String,
    /// Retour à la position de départ N s après la perte de la personne (10 s ; 3 s avec --sim)
    #[arg(long)]
    pub return_after: Option<f64>,
    /// Laisser la caméra où elle est à la fin (sinon : retour à la position de départ)
    #[arg(long)]
    pub stay: bool,
    /// Sans suivi : fait glisser le décor de N pixels en largeur (flux fluide, N < 0 : vers la
    /// gauche), mesuré sur l'image ; rattrape un petit décalage laissé par des essais
    #[arg(long, allow_hyphen_values = true)]
    pub shift_px: Option<f32>,
    /// Suivre seulement en largeur (pas d'inclinaison ; indispensable sur la Ranger 2C, qui ignore
    /// tout mouvement comportant une inclinaison)
    #[arg(long)]
    pub no_tilt: bool,
    /// Glissement de l'image à la vitesse 1, en largeurs d'image par seconde (voir --calibrate)
    #[arg(long)]
    pub pan_rate: Option<f32>,
    /// Glissement de l'image à la vitesse 1, en hauteurs d'image par seconde (voir --calibrate)
    #[arg(long)]
    pub tilt_rate: Option<f32>,
    /// Retard de toute la boucle, en secondes (voir --calibrate)
    #[arg(long)]
    pub latency: Option<f32>,
    /// Analyses par seconde
    #[arg(long, default_value_t = 8.0)]
    pub fps: f32,
    /// Processeur seulement (sinon la carte graphique si possible)
    #[arg(long)]
    pub cpu: bool,
}

pub async fn run(a: FollowArgs, path: &Path) -> Result<()> {
    if a.sim {
        return simulated(a).await;
    }
    let query = a.camera.as_deref().ok_or_else(|| anyhow!("précise la caméra (voir `homesmart-cli cam list`), ou --sim"))?;
    let c = super::cam::find(path, query)?;
    if a.calibrate {
        calibrate(a, path, c).await
    } else if let Some(px) = a.shift_px {
        shift(a.clone(), path, c, px).await
    } else {
        live(a, path, c).await
    }
}

fn settings(a: &FollowArgs, return_after: f64) -> AutoTrackSettings {
    let d = AutoTrackSettings::default();
    AutoTrackSettings {
        max_speed: a.speed.clamp(0.1, 1.0),
        pan_rate: a.pan_rate.unwrap_or(d.pan_rate),
        tilt_rate: a.tilt_rate.unwrap_or(d.tilt_rate),
        latency: a.latency.unwrap_or(d.latency),
        tilt: !a.no_tilt,
        return_after: Some(a.return_after.unwrap_or(return_after).max(0.0)),
        ..d
    }
}

fn connect(c: &CameraConfig) -> Result<Onvif> {
    let o = Onvif::connect(&c.host, c.onvif_port, &c.username, &c.password)?;
    if !o.ptz_supported() {
        bail!("cette caméra n'est pas orientable (pas de PTZ ONVIF)");
    }
    Ok(o)
}

/// Nombre à deux décimales, à la française (journal).
fn fr(v: f32) -> String {
    format!("{v:.2}").replace('.', ",")
}

fn fmt_cmd(c: PtzCommand) -> String {
    match c {
        PtzCommand::Move { pan, tilt } => format!("ContinuousMove pan {pan:+.2} tilt {tilt:+.2}"),
        PtzCommand::Stop => "Stop".into(),
    }
}

/// Envoi des commandes (axes inversés selon la caméra), journal, relevé des commandes.
struct Driver {
    onvif: Onvif,
    invert: (bool, bool),
    /// Configuration (le journal est à côté).
    config: PathBuf,
    id: String,
    name: String,
    /// Commandes envoyées (instant, commande dans le sens de l'image).
    log: Vec<(f64, PtzCommand)>,
}

impl Driver {
    fn new(onvif: Onvif, c: &CameraConfig, config: &Path) -> Driver {
        Driver { onvif, invert: (c.invert_pan, c.invert_tilt), config: config.to_path_buf(), id: c.id.clone(), name: c.display_name().to_string(), log: Vec::new() }
    }

    fn jlog(&self, level: Level, msg: impl Into<String>) {
        jlog(&self.config, level, &self.id, &self.name, msg);
    }

    fn send(&mut self, now: f64, cmd: PtzCommand) -> Result<()> {
        let r = match cmd.inverted(self.invert.0, self.invert.1) {
            PtzCommand::Move { pan, tilt } => self.onvif.continuous_move(pan, tilt),
            PtzCommand::Stop => self.onvif.stop(),
        };
        self.log.push((now, cmd));
        println!("  {now:7.2} s  → {}", fmt_cmd(cmd));
        if let Err(e) = &r {
            self.jlog(Level::Warning, format!("Suivi automatique : commande refusée ({}) : {e:#}", fmt_cmd(cmd)));
        }
        r
    }

    /// Événements du régulateur → journal (chaque mouvement : son départ et son arrêt).
    fn events(&self, now: f64, events: Vec<AutoTrackEvent>) {
        for e in events {
            println!("  {now:7.2} s  · {}", e.describe());
            self.jlog(Level::Info, format!("Suivi automatique : {}", e.describe()));
        }
    }

    /// Commandes envoyées, mouvements, temps en mouvement, changements de sens (orientation, inclinaison).
    fn summary(&self) -> (usize, usize, f64, [usize; 2]) {
        let (mut moves, mut moving, mut since, mut prev, mut rev) = (0, 0.0, None::<f64>, [0f32; 2], [0usize; 2]);
        for &(t, c) in &self.log {
            match c {
                PtzCommand::Move { pan, tilt } => {
                    if since.is_none() {
                        moves += 1;
                        since = Some(t);
                    }
                    for (i, u) in [pan, tilt].into_iter().enumerate() {
                        if u != 0.0 {
                            if prev[i] != 0.0 && u.signum() != prev[i].signum() {
                                rev[i] += 1;
                            }
                            prev[i] = u;
                        }
                    }
                }
                PtzCommand::Stop => {
                    if let Some(s) = since.take() {
                        moving += t - s;
                    }
                }
            }
        }
        (self.log.len(), moves, moving, rev)
    }

    /// Plus grand nombre de commandes envoyées sur une seconde.
    fn max_rate(&self) -> usize {
        self.log.iter().map(|(t, _)| self.log.iter().filter(|(u, _)| *u >= *t && *u < *t + 1.0).count()).max().unwrap_or(0)
    }
}

/// Écart au centre de la personne suivie, au fil des analyses.
#[derive(Default)]
struct Centering {
    followed: f64,
    centered: f64,
    sum: [f64; 2],
    max: [f32; 2],
    n: u32,
    last: Option<f64>,
    acquired: Option<f64>,
    first_centered: Option<f64>,
    /// Côté de la première mesure, et plus grand écart de l'autre côté (dépassement), en largeur.
    first_side: f32,
    overshoot: f32,
}

impl Centering {
    fn observe(&mut self, now: f64, error: Option<(f32, f32)>, dz: (f32, f32)) {
        let dt = self.last.map_or(0.0, |l| (now - l).min(0.5));
        self.last = Some(now);
        let Some((ex, ey)) = error else { return };
        self.acquired.get_or_insert(now);
        if self.first_side == 0.0 {
            self.first_side = ex.signum();
        }
        self.overshoot = self.overshoot.max(-self.first_side * ex);
        self.followed += dt;
        let centered = ex.abs() < dz.0 && ey.abs() < dz.1;
        if centered {
            self.centered += dt;
            self.first_centered.get_or_insert(now);
        }
        self.sum[0] += ex.abs() as f64;
        self.sum[1] += ey.abs() as f64;
        self.max = [self.max[0].max(ex.abs()), self.max[1].max(ey.abs())];
        self.n += 1;
    }

    /// `still` : la personne ne bouge pas (personne virtuelle) ; le dépassement du centre a un sens.
    fn print(&self, still: bool) {
        if self.n == 0 {
            println!("— personne suivie : aucune");
            return;
        }
        let n = self.n as f64;
        println!(
            "— personne suivie {:.1} s : centrée {:.0} % du temps (première fois {:.1} s après l'avoir vue), écart moyen x {:.3} y {:.3}, max x {:.3} y {:.3}{}",
            self.followed,
            if self.followed > 0.0 { self.centered / self.followed * 100.0 } else { 0.0 },
            self.first_centered.zip(self.acquired).map_or(f64::NAN, |(c, a)| c - a),
            self.sum[0] / n,
            self.sum[1] / n,
            self.max[0],
            self.max[1],
            if still { format!(", dépassement x {:.3}", self.overshoot) } else { String::new() }
        );
    }
}

fn print_commands(drv: &Driver) {
    let (n, moves, moving, rev) = drv.summary();
    println!(
        "— {n} commande(s) envoyée(s) (au plus {} par seconde), {moves} mouvement(s), {moving:.1} s en mouvement, changements de sens : {} en orientation, {} en inclinaison",
        drv.max_rate(),
        rev[0],
        rev[1]
    );
}

/// Écart entre l'image du départ et celle de la fin (après le retour), en pixels.
fn print_return(label: &str, first: Option<&(usize, usize, Profiles)>, last: Option<&(usize, usize, Profiles)>) {
    let (Some((w, h, a)), Some((_, _, b))) = (first, last) else {
        println!("— {label} : pas d'image pour le vérifier");
        return;
    };
    let ((dx, dy), q) = a.shift_to(b, w.max(h) / 4);
    println!(
        "— {label} : l'image a glissé de {dx:+.1} px en largeur et {dy:+.1} px en hauteur ({:+.1} % / {:+.1} % de l'image, ressemblance {q:.2})",
        dx / *w as f32 * 100.0,
        dy / *h as f32 * 100.0
    );
}

/// Laisse passer `secs` secondes d'images et rend les profils de la dernière.
fn settle(rx: &mpsc::Receiver<Frame>, secs: f64) -> Option<(usize, usize, Profiles)> {
    let until = Instant::now() + Duration::from_secs_f64(secs);
    let mut last = None;
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(f) => last = Some(f),
            Err(_) => break,
        }
    }
    last.map(|f| (f.width, f.height, Profiles::from_rgba(f.width, f.height, &f.rgba)))
}

/// Amène le décor à `target` pixels de l'image `reference` par de petites impulsions mesurées
/// (six au plus), à la vitesse la plus lente ; rend les profils de la dernière image. Une
/// impulsion courte déplace la caméra plus que sa durée ne le laisse croire (l'arrêt ONVIF met
/// 0,13 s à partir, le moteur a de l'inertie) : la première ne fait que la moitié du chemin
/// prévu, les suivantes sont recalées sur ce que la précédente a vraiment fait. On s'arrête
/// quand l'écart restant est plus petit que le plus petit pas que la caméra a su faire, ou dès
/// qu'une impulsion n'a rien amélioré.
fn align(drv: &mut Driver, rx: &mpsc::Receiver<Frame>, reference: &(usize, usize, Profiles), target: (f32, f32), s: &AutoTrackSettings, t0: Instant, why: &str) -> Result<Option<(usize, usize, Profiles)>> {
    let (w, h, r) = reference;
    let mut last = settle(rx, 1.0);
    // Pixels réellement parcourus par impulsion, rapportés à ceux que prévoit le réglage
    // (première idée : deux fois plus).
    let mut scale = 2.0f64;
    let mut pending: Option<(usize, f32, f32, f32, f64)> = None;
    let (speed, tolerance) = (s.min_speed, (0.0125 * *w as f32).max(3.0));
    let mut min_step = f32::MAX;
    for _ in 0..6 {
        let Some((_, _, p)) = last.as_ref() else { break };
        let ((dx, dy), q) = r.shift_to(p, w.max(h) / 4);
        if q < 0.6 {
            println!("  correction : ressemblance {q:.2} trop faible avec l'image de référence, pas de correction");
            break;
        }
        let err = [dx - target.0, dy - target.1];
        println!("  écart au but {:+.1} px / {:+.1} px (ressemblance {q:.2})", err[0], err[1]);
        if let Some((axis, e0, model_px, u, secs)) = pending.take() {
            let done = e0 - err[axis];
            if done.abs() <= 1.0 {
                // Impulsion sans effet (trop courte pour démarrer le moteur) : deux fois plus longue.
                scale /= 2.0;
            } else if done.signum() == e0.signum() {
                scale = (done.abs() / model_px).clamp(0.3, 20.0) as f64;
                min_step = min_step.min(done.abs());
            }
            if done.abs() > 1.0 && err[axis].abs() >= e0.abs() {
                // Trop loin : la même impulsion à l'envers ramène au plus près, puis on s'arrête.
                println!("  correction : l'impulsion a dépassé le but, retour d'autant, puis arrêt");
                let mut back = [0.0; 2];
                back[axis] = -u;
                drv.send(t0.elapsed().as_secs_f64(), PtzCommand::Move { pan: back[0], tilt: back[1] })?;
                std::thread::sleep(Duration::from_secs_f64(secs));
                drv.send(t0.elapsed().as_secs_f64(), PtzCommand::Stop)?;
                drv.jlog(Level::Info, format!("Suivi automatique : {why} — {} pendant {secs:.2} s (retour du dépassement)", direction(back[0], back[1])).replace('.', ","));
                last = settle(rx, 1.0);
                break;
            }
            if min_step < f32::MAX && err[axis].abs() < 0.6 * min_step {
                println!("  correction : écart plus petit que le plus petit pas de la caméra ({min_step:.0} px), on s'arrête là");
                break;
            }
        }
        let Some((cmd, secs)) = home_nudge((err[0], err[1]), (*w, *h), s, speed, tolerance) else { break };
        let PtzCommand::Move { pan, tilt } = cmd else { break };
        let axis = if pan != 0.0 { 0 } else { 1 };
        // 70 % du chemin seulement : une impulsion plus courte va plus loin, toutes proportions gardées.
        let secs = (0.7 * secs / scale).clamp(0.02, 1.0);
        let (size, rate) = if axis == 0 { (*w, s.pan_rate) } else { (*h, s.tilt_rate) };
        pending = Some((axis, err[axis], secs as f32 * rate * speed * size as f32, if axis == 0 { pan } else { tilt }, secs));
        drv.send(t0.elapsed().as_secs_f64(), cmd)?;
        std::thread::sleep(Duration::from_secs_f64(secs));
        drv.send(t0.elapsed().as_secs_f64(), PtzCommand::Stop)?;
        let msg = format!("{} pendant {secs:.2} s (écart {:+.0} px / {:+.0} px)", direction(pan, tilt), err[0], err[1]).replace('.', ",");
        drv.jlog(Level::Info, format!("Suivi automatique : {why} — {msg}"));
        last = settle(rx, 1.0);
    }
    Ok(last)
}

// ---- Vraie caméra --------------------------------------------------------------------------

async fn live(a: FollowArgs, path: &Path, c: CameraConfig) -> Result<()> {
    let s = settings(&a, 10.0);
    println!(
        "{} — suivi automatique pendant {} s{}, vitesse max {:.2}, flux fluide, {} analyses/s ; axes inversés : orientation {}, inclinaison {} ; chiffres seulement",
        c.display_name(),
        a.secs,
        if s.tilt { "" } else { " (orientation seulement)" },
        s.max_speed,
        a.fps,
        if c.invert_pan { "oui" } else { "non" },
        if c.invert_tilt { "oui" } else { "non" }
    );
    println!("  réglages : glissement de l'image {:.2} / {:.2} par seconde à la vitesse 1, retard {:.2} s, zone morte {:.2} / {:.2}", s.pan_rate, s.tilt_rate, s.latency, s.dead_zone.0, s.dead_zone.1);
    let c2 = c.clone();
    let onvif = tokio::task::spawn_blocking(move || connect(&c2)).await??;
    let who = a.r#virtual.map_or(String::new(), |x| format!(", personne virtuelle à {} largeur d'image", fr(x)));
    if let Some(x) = a.r#virtual {
        println!("  personne virtuelle immobile à {x:+.2} largeur d'image du centre, de 1 s à {:.0} s (place dans l'image : glissement mesuré de l'image)", a.secs as f64 / 2.0);
    }
    jlog(path, Level::Info, &c.id, c.display_name(), format!("Suivi automatique : essai de {} s (vitesse max {}{who}) — vidéo en direct ouverte (flux fluide, chiffres seulement)", a.secs, fr(s.max_speed)));
    let (tx, rx) = mpsc::sync_channel::<Frame>(2);
    let sink: stream::FrameSink = Arc::new(move |f| {
        let _ = tx.try_send(f);
    });
    let stop = Arc::new(AtomicBool::new(false));
    let (stop2, cfg, a2, c2) = (stop.clone(), path.to_path_buf(), a.clone(), c.clone());
    let worker = tokio::task::spawn_blocking(move || live_worker(&a2, s, Driver::new(onvif, &c2, &cfg), rx, &stop2));
    tokio::pin!(worker);
    let url = c.rtsp_url(Quality::Sub);
    let run = stream::run(&url, &c.username, &c.password, sink, |_| {});
    let r = tokio::select! {
        r = &mut worker => r,
        r = run => {
            if let Err(e) = r {
                eprintln!("flux interrompu : {}", stream::describe_error(&e));
            }
            worker.await
        }
        _ = tokio::signal::ctrl_c() => {
            stop.store(true, Ordering::Relaxed);
            worker.await
        }
    };
    let r = r.map_err(|e| anyhow!("analyse interrompue : {e}")).and_then(|r| r);
    let level = if r.is_ok() { Level::Info } else { Level::Warning };
    jlog(path, level, &c.id, c.display_name(), "Suivi automatique : essai terminé — vidéo en direct fermée");
    r
}

fn live_worker(a: &FollowArgs, s: AutoTrackSettings, mut drv: Driver, rx: mpsc::Receiver<Frame>, stop: &AtomicBool) -> Result<()> {
    let r = live_loop(a, s, &mut drv, rx, stop);
    // Quoi qu'il arrive, la caméra ne doit pas rester en mouvement.
    if r.is_err() || drv.log.last().is_some_and(|(_, c)| *c != PtzCommand::Stop) {
        let _ = drv.onvif.stop();
        drv.jlog(Level::Info, "Suivi automatique : arrêt de sécurité de la caméra");
    }
    r
}

fn live_loop(a: &FollowArgs, s: AutoTrackSettings, drv: &mut Driver, rx: mpsc::Receiver<Frame>, stop: &AtomicBool) -> Result<()> {
    let mut persons = RtmoPersons::new(if a.cpu { Accel::Cpu } else { Accel::Auto })?;
    persons.threshold = 0.25;
    println!("  personnes : RTMO ({})", persons.backend);
    let mut tracker = Tracker::default();
    tracker.high = Pipeline::NEW_PERSON;
    let dz = s.dead_zone;
    let settings = s.clone();
    let mut at = AutoTracker::new(s);
    // Une image arrive toutes les 40 ms environ : on analyse celle qui tombe au plus près.
    let every = Duration::from_secs_f32(1.0 / a.fps.clamp(0.5, 30.0)).saturating_sub(Duration::from_millis(20));
    let t0 = Instant::now();
    let (mut last_analysis, mut last_print) = (None::<Instant>, -1i64);
    let (mut first, mut last) = (None, None);
    let mut centering = Centering::default();
    let (mut finishing, mut still_since, mut disconnected) = (None::<f64>, None::<f64>, false);
    let mut t_detect = (0f64, 0u32);
    // Personne virtuelle : glissement cumulé de l'image (pixels) depuis le départ.
    let (mut slid, mut prev) = (0f32, None::<Profiles>);
    loop {
        let now = t0.elapsed().as_secs_f64();
        // Fin de l'essai : retour au départ (ou arrêt sur place), puis encore 1,5 s d'images
        // pour mesurer l'écart avec l'image du départ.
        if finishing.is_none() && (now >= a.secs as f64 || stop.load(Ordering::Relaxed)) {
            finishing = Some(now);
            let cmd = if a.stay { at.stop(now) } else { at.go_home(now) };
            if let Some(cmd) = cmd {
                drv.send(now, cmd)?;
            }
            drv.events(now, at.take_events());
        }
        if let Some(f) = finishing {
            if at.is_moving() || at.phase() == Phase::Returning {
                still_since = None;
            } else if now - *still_since.get_or_insert(now) >= 1.5 || disconnected {
                break;
            }
            if now - f > 180.0 {
                bail!("retour au départ trop long");
            }
        }
        let wait = at.wake_at().map_or(0.1, |w| (w - now).clamp(0.0, 0.1));
        let frame = if disconnected {
            std::thread::sleep(Duration::from_secs_f64(wait));
            None
        } else {
            match rx.recv_timeout(Duration::from_secs_f64(wait)) {
                Ok(f) => Some(f),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    None
                }
            }
        };
        let mut cmd = None;
        if let Some(frame) = frame.filter(|_| last_analysis.is_none_or(|l| l.elapsed() >= every)) {
            last_analysis = Some(Instant::now());
            let p = (frame.width, frame.height, Profiles::from_rgba(frame.width, frame.height, &frame.rgba));
            let p_now = p.2.clone();
            if first.is_none() {
                first = Some(p.clone());
            }
            last = Some(p);
            if let Some(prev) = prev.replace(p_now.clone()) {
                slid += prev.shift_to(&p_now, 64).0 .0;
            }
            if finishing.is_none() {
                let t1 = Instant::now();
                let now = t0.elapsed().as_secs_f64();
                let dets = match a.r#virtual {
                    Some(x) => {
                        let cx = 0.5 + x + slid / frame.width as f32;
                        let here = now >= 1.0 && now < a.secs as f64 / 2.0 && (0.02..0.98).contains(&cx);
                        here.then(|| Detection { bbox: BBox::new(cx - 0.075, 0.25, cx + 0.075, 0.75), score: 0.9, pose: None }).into_iter().collect()
                    }
                    None => persons.detect(&frame)?,
                };
                t_detect = (t_detect.0 + t1.elapsed().as_secs_f64(), t_detect.1 + 1);
                let now = t0.elapsed().as_secs_f64();
                tracker.update(now, &dets);
                cmd = at.update(now, tracker.tracks());
                // Écart de la personne suivie, sur les images où elle est détectée.
                let aim = at.aim().filter(|m| tracker.track(m.track).is_some_and(|t| t.last_seen >= now - 1e-9));
                centering.observe(now, aim.map(|m| m.error), dz);
                if now as i64 != last_print {
                    last_print = now as i64;
                    let (pan, tilt) = at.command();
                    match aim {
                        Some(m) => println!(
                            "  {now:7.2} s  #{} boîte [{:.2} {:.2} {:.2} {:.2}] écart x {:+.3} y {:+.3} · prévu x {:+.3} y {:+.3} · vitesse {:+.2} / {:+.2} · commande pan {pan:+.2} tilt {tilt:+.2} · freinage {:.2}",
                            m.track, m.bbox.x0, m.bbox.y0, m.bbox.x1, m.bbox.y1, m.error.0, m.error.1, m.predicted.0, m.predicted.1, m.velocity.0, m.velocity.1, at.damping().0
                        ),
                        None => println!("  {now:7.2} s  {} personne(s) détectée(s), {:?} · commande pan {pan:+.2} tilt {tilt:+.2}", dets.len(), at.phase()),
                    }
                }
            }
        }
        let now = t0.elapsed().as_secs_f64();
        if cmd.is_none() && at.wake_at().is_some_and(|w| w <= now) {
            cmd = at.update(now, if finishing.is_some() { &[] } else { tracker.tracks() });
        }
        if let Some(cmd) = cmd {
            drv.send(now, cmd)?;
        }
        drv.events(now, at.take_events());
    }
    if t_detect.1 > 0 {
        let what = if a.r#virtual.is_some() { "personne virtuelle" } else { "RTMO" };
        println!("— {} analyse(s) en {:.0} s, {:.0} ms par image ({what})", t_detect.1, t0.elapsed().as_secs_f64(), t_detect.0 * 1000.0 / t_detect.1 as f64);
    }
    centering.print(a.r#virtual.is_some());
    if a.stay {
        print_commands(drv);
        println!("— caméra laissée où elle est (--stay)");
        return Ok(());
    }
    // Les mouvements refaits à l'envers ne ramènent pas tout à fait au départ (moteur pas
    // parfaitement fidèle) : comparaison avec l'image du départ, petite correction si besoin.
    if !drv.log.is_empty() {
        print_return("retour, mouvements refaits à l'envers", first.as_ref(), last.as_ref());
        if let Some(f) = first.as_ref().filter(|_| !disconnected) {
            last = align(drv, &rx, f, (0.0, 0.0), &settings, t0, "correction du retour au départ")?.or(last);
        }
    }
    print_commands(drv);
    print_return("à la fin, par rapport au départ", first.as_ref(), last.as_ref());
    Ok(())
}

/// Fait glisser le décor de `px` pixels en largeur, mesuré sur l'image (sans suivi).
async fn shift(a: FollowArgs, path: &Path, c: CameraConfig, px: f32) -> Result<()> {
    let s = AutoTrackSettings { tilt: false, ..settings(&a, 10.0) };
    let c2 = c.clone();
    let onvif = tokio::task::spawn_blocking(move || connect(&c2)).await??;
    println!("{} — décor à faire glisser de {px:+.0} px en largeur (flux fluide), chiffres seulement", c.display_name());
    jlog(path, Level::Info, &c.id, c.display_name(), format!("Recadrage mesuré de {px:+.0} px — vidéo en direct ouverte (flux fluide, chiffres seulement)"));
    let (tx, rx) = mpsc::sync_channel::<Frame>(2);
    let sink: stream::FrameSink = Arc::new(move |f| {
        let _ = tx.try_send(f);
    });
    let (cfg, c2) = (path.to_path_buf(), c.clone());
    let worker = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut drv = Driver::new(onvif, &c2, &cfg);
        let t0 = Instant::now();
        let reference = settle(&rx, 1.5).ok_or_else(|| anyhow!("pas d'image de la caméra"))?;
        let r = align(&mut drv, &rx, &reference, (px, 0.0), &s, t0, &format!("recadrage de {px:+.0} px"));
        if r.is_err() {
            let _ = drv.onvif.stop();
        }
        if let Some((_, _, p)) = r?.as_ref() {
            let ((dx, dy), q) = reference.2.shift_to(p, reference.0.max(reference.1) / 4);
            println!("— décor glissé de {dx:+.1} px / {dy:+.1} px (demandé {px:+.0} px, ressemblance {q:.2}), {} commande(s)", drv.log.len());
        }
        Ok(())
    });
    tokio::pin!(worker);
    let url = c.rtsp_url(Quality::Sub);
    let run = stream::run(&url, &c.username, &c.password, sink, |_| {});
    let r = tokio::select! {
        r = &mut worker => r,
        r = run => {
            if let Err(e) = r {
                eprintln!("flux interrompu : {}", stream::describe_error(&e));
            }
            worker.await
        }
    };
    let r = r.map_err(|e| anyhow!("recadrage interrompu : {e}")).and_then(|r| r);
    jlog(path, if r.is_ok() { Level::Info } else { Level::Warning }, &c.id, c.display_name(), "Recadrage mesuré terminé — vidéo en direct fermée");
    r
}

// ---- Caméra simulée ------------------------------------------------------------------------

/// Image de la caméra simulée (voir `render_region` dans `camera/sim.rs`) : largeurs d'image par
/// unité d'orientation, hauteurs d'image par unité d'inclinaison (flux fluide 640×360).
const SIM_PAN_IMAGE: f32 = 0.75 / 1.2;
const SIM_TILT_IMAGE: f32 = 0.35 / (1.2 * 360.0 / 640.0);
/// Vitesse de la caméra simulée à la vitesse 1 (unités par seconde).
const SIM_SPEED: f32 = 0.6;
/// Retard de l'analyse simulé : flux et calcul (secondes).
const SIM_ANALYSIS_DELAY: f64 = 0.3;

/// La personne virtuelle : écart au centre de l'image de départ (`None` : pas là). Apparaît en
/// bas à droite, attend, traverse vers la gauche, attend, puis sort.
fn sim_person(t: f64) -> Option<[f32; 2]> {
    match t {
        t if t < 0.5 => None,
        t if t < 9.0 => Some([0.30, 0.18]),
        t if t < 14.0 => Some([0.30 - 0.12 * (t - 9.0) as f32, 0.18]),
        t if t < 17.0 => Some([-0.30, 0.18]),
        _ => None,
    }
}

async fn simulated(a: FollowArgs) -> Result<()> {
    let s = settings(&a, 3.0);
    let cam = sim::start("SIMSUIVI1", 0).await?;
    let mut c = CameraConfig::new("127.0.0.1");
    (c.id, c.name, c.onvif_port, c.rtsp_port, c.password) = ("cam-sim-suivi".into(), "Caméra simulée".into(), cam.onvif_port, cam.rtsp_port, "SIMSUIVI1".into());
    // Journal à part, comme la démo : l'essai simulé ne se mêle pas au journal de la maison.
    let dir = std::env::temp_dir().join("homesmart-follow-sim");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let config = dir.join("homesmart.json");
    println!(
        "Caméra simulée — personne virtuelle, analyse rendue {SIM_ANALYSIS_DELAY} s après l'image, {} analyses/s ; vraie vitesse de l'image {:.3} / {:.3} par seconde à la vitesse 1, réglage {:.2} / {:.2}, retard réglé {:.2} s",
        a.fps,
        SIM_SPEED * SIM_PAN_IMAGE,
        SIM_SPEED * SIM_TILT_IMAGE,
        s.pan_rate,
        s.tilt_rate,
        s.latency
    );
    println!("  journal de l'essai : {}", config.with_file_name("journal.jsonl").display());
    let c2 = c.clone();
    let onvif = tokio::task::spawn_blocking(move || connect(&c2)).await??;
    let drv = Driver::new(onvif, &c, &config);
    drv.jlog(Level::Info, format!("Suivi automatique : essai simulé de {} s (vitesse max {})", a.secs, fr(s.max_speed)));
    let state = cam.state.clone();
    tokio::task::spawn_blocking(move || sim_worker(&a, s, drv, &state)).await?
}

fn sim_worker(a: &FollowArgs, s: AutoTrackSettings, mut drv: Driver, state: &sim::SimState) -> Result<()> {
    let r = sim_loop(a, s, &mut drv, state);
    let _ = drv.onvif.stop();
    r
}

fn sim_loop(a: &FollowArgs, s: AutoTrackSettings, drv: &mut Driver, state: &sim::SimState) -> Result<()> {
    let mut tracker = Tracker::default();
    tracker.high = Pipeline::NEW_PERSON;
    let (aim_y, dz) = (s.aim_y, s.dead_zone);
    let mut at = AutoTracker::new(s);
    let period = 1.0 / a.fps.clamp(0.5, 30.0) as f64;
    let mut pending: VecDeque<(f64, Option<BBox>)> = VecDeque::new();
    let (mut next_frame, mut noise, mut last_print) = (0.0, 7u32, -1i64);
    // (instant, écart réel au centre si la personne est là, position de la caméra)
    let mut log: Vec<(f64, Option<[f32; 2]>, (f32, f32))> = Vec::new();
    let mut centering = Centering::default();
    let (mut finishing, mut still_since, mut last_detection) = (false, None::<f64>, None::<f64>);
    let t0 = Instant::now();
    loop {
        let now = t0.elapsed().as_secs_f64();
        if !finishing && now >= a.secs as f64 {
            finishing = true;
            let cmd = if a.stay { at.stop(now) } else { at.go_home(now) };
            if let Some(cmd) = cmd {
                drv.send(now, cmd)?;
            }
            drv.events(now, at.take_events());
        }
        if finishing {
            if at.is_moving() || at.phase() == Phase::Returning {
                still_since = None;
            } else if now - *still_since.get_or_insert(now) >= 0.5 {
                break;
            }
        }
        let (x, y) = state.position();
        let cam = [x * SIM_PAN_IMAGE, -y * SIM_TILT_IMAGE];
        let seen = sim_person(now).filter(|_| !finishing).map(|p| [p[0] - cam[0], p[1] + cam[1]]);
        log.push((now, seen, (x, y)));
        if now >= next_frame {
            next_frame += period;
            let mut jitter = || {
                noise = noise.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (((noise >> 16) % 1000) as f32 / 1000.0 - 0.5) * 0.008
            };
            let bbox = seen.filter(|e| e[0].abs() < 0.5 && e[1].abs() < 0.5).map(|e| {
                let (w, h, cx, ay) = (0.15, 0.5, 0.5 + e[0] + jitter(), 0.5 + e[1] + jitter());
                BBox::new(cx - w / 2.0, ay - aim_y * h, cx + w / 2.0, ay + (1.0 - aim_y) * h).clamp()
            });
            pending.push_back((now + SIM_ANALYSIS_DELAY, bbox));
        }
        let mut cmd = None;
        while pending.front().is_some_and(|(at, _)| *at <= now) {
            let (_, bbox) = pending.pop_front().unwrap();
            if bbox.is_some() {
                last_detection = Some(now);
            }
            let dets: Vec<Detection> = bbox.into_iter().map(|b| Detection { bbox: b, score: 0.9, pose: None }).collect();
            tracker.update(now, &dets);
            if let Some(c) = at.update(now, if finishing { &[] } else { tracker.tracks() }) {
                cmd = Some(c);
            }
            let aim = at.aim().filter(|m| tracker.track(m.track).is_some_and(|t| t.last_seen >= now - 1e-9));
            centering.observe(now, aim.map(|m| m.error), dz);
            if now as i64 != last_print {
                last_print = now as i64;
                let (pan, tilt) = at.command();
                let truth = seen.map_or("absente".to_string(), |e| format!("x {:+.3} y {:+.3}", e[0], e[1]));
                match aim {
                    Some(m) => println!(
                        "  {now:7.2} s  #{} boîte [{:.2} {:.2} {:.2} {:.2}] écart vu x {:+.3} y {:+.3} (réel {truth}) · prévu x {:+.3} y {:+.3} · commande pan {pan:+.2} tilt {tilt:+.2} · caméra ({x:+.3}, {y:+.3})",
                        m.track, m.bbox.x0, m.bbox.y0, m.bbox.x1, m.bbox.y1, m.error.0, m.error.1, m.predicted.0, m.predicted.1
                    ),
                    None => println!("  {now:7.2} s  personne {truth}, {:?} · commande pan {pan:+.2} tilt {tilt:+.2} · caméra ({x:+.3}, {y:+.3})", at.phase()),
                }
            }
        }
        if cmd.is_none() && at.wake_at().is_some_and(|w| w <= now) {
            cmd = at.update(now, if finishing { &[] } else { tracker.tracks() });
        }
        if let Some(cmd) = cmd {
            drv.send(now, cmd)?;
        }
        drv.events(now, at.take_events());
        std::thread::sleep(Duration::from_millis(5));
    }

    println!("Mesures (écart réel au centre, en fraction de l'image ; zone morte {:.2} / {:.2}) :", dz.0, dz.1);
    let window = |from: f64, to: f64| log.iter().filter(move |(t, e, _)| *t >= from && *t < to && e.is_some()).map(|(t, e, _)| (*t, e.unwrap()));
    // 1. Personne immobile apparue loin du centre.
    let step: Vec<(f64, [f32; 2])> = window(0.5, 9.0).collect();
    if let Some(&(t_first, e0)) = step.first() {
        let settle = step.iter().rposition(|(_, e)| e[0].abs() >= dz.0 || e[1].abs() >= dz.1).map_or(Some(0.0), |i| step.get(i + 1).map(|p| p.0 - t_first));
        let over = |i: usize| step.iter().map(|(_, e)| -e0[i].signum() * e[i]).fold(0.0f32, f32::max);
        let last = step.last().unwrap().1;
        let rev = reversals(&drv.log, 0.5, 9.0);
        println!(
            "  apparition à x {:+.2} y {:+.2} : centrée en {} (premier mouvement {}), dépassement x {:.3} y {:.3}, écart à 9 s x {:+.3} y {:+.3}, changements de sens {} / {}",
            e0[0],
            e0[1],
            settle.map_or("—".into(), |s| format!("{s:.1} s")),
            first_move(&drv.log, 0.5).map_or("—".into(), |t| format!("{:.2} s après l'apparition", t - t_first)),
            over(0),
            over(1),
            last[0],
            last[1],
            rev[0],
            rev[1]
        );
    }
    // 2. Traversée de la pièce (0,12 largeur d'image par seconde pendant 5 s), puis arrêt.
    let walk: Vec<f32> = window(10.0, 14.0).map(|(_, e)| e[0].abs()).collect();
    if !walk.is_empty() {
        let after: Vec<(f64, [f32; 2])> = window(14.0, 17.0).collect();
        let last = after.last().map_or([f32::NAN; 2], |p| p.1);
        let rev = reversals(&drv.log, 14.0, 17.0);
        println!(
            "  traversée : écart moyen x {:.3}, max {:.3} ; arrêtée : écart à 17 s x {:+.3} y {:+.3}, changements de sens {} / {}",
            walk.iter().sum::<f32>() / walk.len() as f32,
            walk.iter().cloned().fold(0.0, f32::max),
            last[0],
            last[1],
            rev[0],
            rev[1]
        );
    }
    // 3. Sortie de la pièce : arrêt, attente, retour au départ.
    if let Some(gone) = last_detection.filter(|_| a.secs > 17) {
        let stop = drv.log.iter().find(|(t, c)| *t >= gone - 1e-9 && *c == PtzCommand::Stop).map(|(t, _)| t - gone);
        let moving_then = drv.log.iter().rev().find(|(t, _)| *t < gone).is_some_and(|(_, c)| *c != PtzCommand::Stop);
        println!(
            "  sortie : {} ; dernière détection à {gone:.1} s",
            match (moving_then, stop) {
                (true, Some(s)) => format!("caméra arrêtée {s:.2} s après la dernière détection"),
                (true, None) => "caméra jamais arrêtée !".into(),
                (false, _) => "caméra déjà immobile".into(),
            }
        );
    }
    let (x, y) = state.position();
    println!(
        "  position finale de la caméra ({x:+.4}, {y:+.4}) pour un départ en (0, 0) : écart {:+.2} % de la largeur, {:+.2} % de la hauteur",
        x * SIM_PAN_IMAGE * 100.0,
        -y * SIM_TILT_IMAGE * 100.0
    );
    centering.print(false);
    print_commands(drv);
    Ok(())
}

/// Changements de sens des commandes (orientation, inclinaison) entre `from` et `to`.
fn reversals(log: &[(f64, PtzCommand)], from: f64, to: f64) -> [usize; 2] {
    let (mut prev, mut rev) = ([0f32; 2], [0usize; 2]);
    for (_, c) in log.iter().filter(|(t, _)| *t >= from && *t < to) {
        if let PtzCommand::Move { pan, tilt } = c {
            for (i, u) in [*pan, *tilt].into_iter().enumerate() {
                if u != 0.0 {
                    if prev[i] != 0.0 && u.signum() != prev[i].signum() {
                        rev[i] += 1;
                    }
                    prev[i] = u;
                }
            }
        }
    }
    rev
}

fn first_move(log: &[(f64, PtzCommand)], from: f64) -> Option<f64> {
    log.iter().find(|(t, c)| *t >= from && matches!(c, PtzCommand::Move { .. })).map(|(t, _)| *t)
}

// ---- Étalonnage ----------------------------------------------------------------------------

async fn calibrate(a: FollowArgs, path: &Path, c: CameraConfig) -> Result<()> {
    let hi = (a.speed.clamp(0.1, 1.0) / 0.05).round() * 0.05;
    let lo = ((hi / 2.0) / 0.05).round().max(2.0) * 0.05;
    let (pan, tilt, diag) = match a.axis.as_str() {
        "pan" => (true, false, false),
        "tilt" => (false, true, false),
        "both" => (true, true, false),
        "diag" => (false, false, true),
        other => bail!("axe inconnu : {other} (pan, tilt, both ou diag)"),
    };
    println!(
        "{} — étalonnage du suivi automatique : {}{}chaque mouvement suivi du même à l'envers ; axes inversés : orientation {}, inclinaison {} ; chiffres seulement",
        c.display_name(),
        if pan { format!("orientation à {lo:.2} puis {hi:.2}, ") } else { String::new() },
        if tilt { format!("inclinaison à {lo:.2} puis {hi:.2}, ") } else { String::new() } + if diag { format!("diagonale (orientation {lo:.2}, inclinaison {hi:.2}), ") } else { String::new() }.as_str(),
        if c.invert_pan { "oui" } else { "non" },
        if c.invert_tilt { "oui" } else { "non" }
    );
    let c2 = c.clone();
    let onvif = tokio::task::spawn_blocking(move || connect(&c2)).await??;
    jlog(path, Level::Info, &c.id, c.display_name(), "Étalonnage du suivi automatique — vidéo en direct ouverte (flux fluide, chiffres seulement)");
    let (tx, rx) = mpsc::sync_channel::<(Instant, Frame)>(16);
    let sink: stream::FrameSink = Arc::new(move |f| {
        let _ = tx.try_send((Instant::now(), f));
    });
    // Mouvements : (début, vitesses dans le sens de l'image, durée).
    let mut plan: Vec<(f64, [f32; 2], f64)> = Vec::new();
    let mut t = 2.0;
    let pans = [([hi, 0.0], 1.0), ([-hi, 0.0], 1.0), ([lo, 0.0], 1.0), ([-lo, 0.0], 1.0)];
    let tilts = [([0.0, hi], 1.2), ([0.0, -hi], 1.2), ([0.0, lo], 1.2), ([0.0, -lo], 1.2)];
    let diags = [([lo, hi], 1.0), ([-lo, -hi], 1.0)];
    for (u, d) in pans.into_iter().filter(|_| pan).chain(tilts.into_iter().filter(|_| tilt)).chain(diags.into_iter().filter(|_| diag)) {
        plan.push((t, u, d));
        t += d + 1.6;
    }
    let end = t + 0.5;
    let (cfg, c2) = (path.to_path_buf(), c.clone());
    let worker = tokio::task::spawn_blocking(move || calibrate_worker(Driver::new(onvif, &c2, &cfg), rx, plan, end));
    tokio::pin!(worker);
    let url = c.rtsp_url(Quality::Sub);
    let run = stream::run(&url, &c.username, &c.password, sink, |_| {});
    let r = tokio::select! {
        r = &mut worker => r,
        r = run => {
            if let Err(e) = r {
                eprintln!("flux interrompu : {}", stream::describe_error(&e));
            }
            worker.await
        }
    };
    let r = r.map_err(|e| anyhow!("mesure interrompue : {e}")).and_then(|r| r);
    jlog(path, if r.is_ok() { Level::Info } else { Level::Warning }, &c.id, c.display_name(), "Étalonnage du suivi automatique terminé — vidéo en direct fermée");
    r
}

fn calibrate_worker(mut drv: Driver, rx: mpsc::Receiver<(Instant, Frame)>, plan: Vec<(f64, [f32; 2], f64)>, end: f64) -> Result<()> {
    let r = calibrate_loop(&mut drv, rx, &plan, end);
    if r.is_err() {
        let _ = drv.onvif.stop();
        drv.jlog(Level::Info, "Étalonnage : arrêt de sécurité de la caméra");
    }
    r
}

fn calibrate_loop(drv: &mut Driver, rx: mpsc::Receiver<(Instant, Frame)>, plan: &[(f64, [f32; 2], f64)], end: f64) -> Result<()> {
    let t0 = Instant::now();
    // Profils de chaque image, à son arrivée.
    let mut frames: Vec<(f64, Profiles)> = Vec::new();
    let mut size = (0usize, 0usize);
    // Commandes faites : (instant de départ, vitesses, instant d'arrêt).
    let mut done: Vec<(f64, [f32; 2], f64)> = Vec::new();
    let mut step = 0usize;
    let mut moving: Option<(f64, [f32; 2], f64)> = None;
    loop {
        let now = t0.elapsed().as_secs_f64();
        if let Some((start, u, d)) = moving {
            if now >= start + d {
                drv.send(now, PtzCommand::Stop)?;
                done.push((start, u, now));
                moving = None;
            }
        } else if let Some(&(at, u, d)) = plan.get(step) {
            if now >= at {
                step += 1;
                drv.send(now, PtzCommand::Move { pan: u[0], tilt: u[1] })?;
                let dir = direction(u[0], u[1]);
                drv.jlog(Level::Info, format!("Étalonnage du suivi automatique : {dir} à la vitesse {:.2} pendant {:.1} s", u[0].abs().max(u[1].abs()), d).replace('.', ","));
                moving = Some((now, u, d));
            }
        } else if now >= end {
            break;
        }
        if now > end + 10.0 {
            bail!("pas d'image de la caméra");
        }
        match rx.recv_timeout(Duration::from_millis(5)) {
            Ok((arrived, f)) => {
                size = (f.width, f.height);
                frames.push(((arrived - t0).as_secs_f64(), Profiles::from_rgba(f.width, f.height, &f.rgba)));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("flux vidéo interrompu"),
        }
    }
    let (w, h) = size;
    if frames.len() < 20 {
        bail!("trop peu d'images ({})", frames.len());
    }
    // Position du décor (pixels) image après image : somme des glissements entre voisines.
    let mut pos: Vec<(f64, [f32; 2])> = vec![(frames[0].0, [0.0, 0.0])];
    let mut weak = 0;
    for pair in frames.windows(2) {
        let ((dx, dy), q) = pair[0].1.shift_to(&pair[1].1, 48);
        if q < 0.5 {
            weak += 1;
        }
        let p = pos.last().unwrap().1;
        pos.push((pair[1].0, [p[0] + dx, p[1] + dy]));
    }
    let span = frames.last().unwrap().0 - frames[0].0;
    println!("— {} images en {span:.1} s ({:.1} images/s), {w}×{h}, {weak} glissement(s) douteux", frames.len(), frames.len() as f64 / span);
    let at = |t: f64| pos.iter().rev().find(|(pt, _)| *pt <= t).map_or(pos[0].1, |p| p.1);
    // Bruit au repos (avant le premier mouvement).
    let rest: Vec<[f32; 2]> = pos.iter().filter(|(t, _)| *t < plan[0].0).map(|p| p.1).collect();
    let jitter = rest.iter().map(|p| p[0].abs().max(p[1].abs())).fold(0.0, f32::max);
    println!("— au repos : le décor bouge d'au plus {jitter:.1} px (bruit de mesure)");
    let mut rates: Vec<(usize, f32, f32)> = Vec::new();
    let mut latencies = Vec::new();
    for (k, &(start, u, stop)) in done.iter().enumerate() {
        let axis = if u[0] != 0.0 { 0 } else { 1 };
        let speed = u[axis].abs();
        let next = done.get(k + 1).map_or(f64::MAX, |n| n.0);
        let settled_at = next.min(stop + 1.5);
        let p0 = at(start);
        let disp = at(settled_at)[axis] - p0[axis];
        // Début : premier glissement net du décor ; fin : dernier glissement avant le mouvement suivant.
        let onset = pos.iter().find(|(t, p)| *t >= start && (p[axis] - p0[axis]).abs() >= 1.5).map(|(t, _)| t - start);
        let p_end = at(settled_at)[axis];
        let still = pos.iter().filter(|(t, _)| *t >= stop && *t <= settled_at).find(|(_, p)| (p[axis] - p_end).abs() < 1.0).map(|(t, _)| t - stop);
        // Sens attendu : orienter à droite fait glisser le décor vers la gauche, incliner vers le
        // haut le fait glisser vers le bas.
        let expected = if axis == 0 { -u[0].signum() } else { u[1].signum() };
        let ok = disp.signum() == expected && disp.abs() > 2.0;
        let size = (if axis == 0 { w } else { h }) as f32;
        let rate = disp.abs() / size / (speed * (stop - start) as f32);
        let other = at(settled_at)[1 - axis] - p0[1 - axis];
        println!(
            "  {:<10} vitesse {speed:.2} pendant {:.2} s : décor glissé de {disp:+.1} px ({:.1} % de l'image ; autre axe {other:+.1} px) → {:.3} {} par seconde à la vitesse 1 · départ visible après {} · immobile {} après l'arrêt · sens {}",
            direction(u[0], u[1]),
            stop - start,
            disp.abs() / size * 100.0,
            rate,
            if axis == 0 { "largeur(s)" } else { "hauteur(s)" },
            onset.map_or("—".into(), |o| format!("{o:.2} s")),
            still.map_or("—".into(), |s| format!("{s:.2} s")),
            if disp.abs() <= 2.0 { "non mesuré (pas de glissement)" } else if ok { "correct" } else { "INVERSÉ" }
        );
        if disp.abs() > 2.0 {
            rates.push((axis, speed, rate));
        }
        if let Some(o) = onset {
            latencies.push(o);
        }
    }
    let mean = |v: Vec<f32>| if v.is_empty() { None } else { Some(v.iter().sum::<f32>() / v.len() as f32) };
    let pan = mean(rates.iter().filter(|r| r.0 == 0).map(|r| r.2).collect());
    let tilt = mean(rates.iter().filter(|r| r.0 == 1).map(|r| r.2).collect());
    latencies.sort_by(f64::total_cmp);
    let lat = latencies.get(latencies.len() / 2).copied();
    // Retour : image de la fin comparée à celle du départ.
    let ((dx, dy), q) = frames[0].1.shift_to(&frames.last().unwrap().1, w.max(h) / 4);
    println!("— fin, chaque mouvement refait à l'envers : décor glissé de {dx:+.1} px / {dy:+.1} px par rapport au départ (ressemblance {q:.2}) ; somme des glissements {:+.1} / {:+.1} px", pos.last().unwrap().1[0], pos.last().unwrap().1[1]);
    let by_speed: Vec<String> = rates.iter().filter(|r| r.0 == 0).map(|r| format!("{:.3} à {:.2}", r.2, r.1)).collect();
    println!("— orientation : {} (proportionnelle à la vitesse si les valeurs se ressemblent)", by_speed.join(", "));
    // Le retard de la boucle compte aussi l'analyse (≈ 0,05 s) après l'arrivée de l'image.
    println!(
        "— réglages mesurés : --pan-rate {} --tilt-rate {} --latency {}",
        pan.map_or("?".into(), |p| format!("{p:.3}")),
        tilt.map_or("?".into(), |t| format!("{t:.3}")),
        lat.map_or("?".into(), |l| format!("{:.2}", l + 0.05))
    );
    Ok(())
}
