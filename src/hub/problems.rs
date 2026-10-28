//! Détection des problèmes dans le hub : les règles de `crate::problems` appliquées aux
//! appareils (acteurs), aux caméras (tâche de surveillance toutes les 5 s) et à la session
//! précédente (arrêt brutal du PC). Chaque problème va au journal avec sa source, sa gravité et
//! sa cause probable, écrit tout de suite sur le disque ; les critiques déclenchent une
//! notification du système (interface). Sa fin est inscrite aussi (« Résolu — … »).

use super::{DeviceHandle, Hub, Level, Source};
use crate::journal::Entry;
use crate::kinds::{self, Kind};
use crate::problems::{self, Board, Finding, Holds, OfflineInfo, PowerWatch, Problem, ProblemKind, SessionMark, SwitchChange, SystemLog, Trace, VoltageWatch};
use chrono::{DateTime, Local};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Délai avant de juger un changement fait hors de HomeSmart : la ligne de commande inscrit
/// son action au journal juste après l'avoir faite.
const EXTERNAL_VERDICT: Duration = Duration::from_secs(4);
/// Puissance habituelle relue dans l'historique au plus toutes les 15 min.
const TYPICAL_REFRESH: Duration = Duration::from_secs(15 * 60);

/// Ce que le hub sait des problèmes.
#[derive(Default)]
pub(super) struct ProblemState {
    board: Board,
    holds: Holds,
    /// Marque de la session en cours (`session.json`).
    session: Option<(PathBuf, SessionMark)>,
    /// Boîte noire : dernière minute de mesures de chaque prise (écrite avec la marque).
    traces: HashMap<String, Trace>,
}

/// Surveillance d'un appareil (garde d'une connexion à l'autre).
#[derive(Default)]
pub(super) struct DeviceWatch {
    /// Liaison perdue depuis (au lancement de l'acteur : pas encore établie).
    lost_at: Option<Instant>,
    last_error: String,
    was_on: Option<bool>,
    /// Échéance de la minuterie de la prise au moment de la perte.
    countdown_until: Option<Instant>,
    /// Reconnecté après une perte de liaison : (quand, durée de la perte, minuterie qui restait).
    reconnected: Option<(Instant, Duration, Option<u64>)>,
    voltage: VoltageWatch,
    power: PowerWatch,
    typical: Option<(Option<f64>, Instant)>,
    faults: u32,
    /// La minuterie qui court a été réglée par HomeSmart (toi, un programme, un garde-fou).
    pub(super) timer_ours: bool,
}

impl DeviceWatch {
    pub(super) fn starting() -> Self {
        DeviceWatch { lost_at: Some(Instant::now()), ..Default::default() }
    }
}

/// Changement fait hors de HomeSmart, en attente du verdict (était-ce la ligne de commande ?).
pub(super) struct ExternalChange {
    pub at: DateTime<Local>,
    /// Description de chaque DP changé, pour le journal.
    pub parts: Vec<String>,
    pub suffix: &'static str,
    /// Changement de marche/arrêt : les sorties concernées (multiprise) et le contexte.
    pub switch: Option<(Vec<String>, SwitchChange)>,
}

impl Hub {
    /// Délais raccourcis (démo, essais) : hors ligne après 5 s, consommation après 20 s, caméra après 5 s.
    pub fn set_fast_detection(&self, fast: bool) {
        self.fast_detection.store(fast, Ordering::Relaxed);
    }

    fn delays(&self) -> (Duration, Duration, Duration, Duration) {
        if self.fast_detection.load(Ordering::Relaxed) {
            (Duration::from_secs(5), Duration::from_secs(20), Duration::from_secs(5), Duration::from_secs(5))
        } else {
            (problems::OFFLINE_AFTER, problems::LOW_POWER_AFTER, problems::CAMERA_AFTER, problems::ANALYSIS_AFTER)
        }
    }

    // ---- Lecture -----------------------------------------------------------------------------

    /// Problèmes en cours, les plus graves d'abord.
    pub fn problems(&self) -> Vec<Problem> {
        self.problems.lock().unwrap().board.active()
    }

    /// Problèmes en cours d'un appareil ou d'une caméra.
    pub fn problems_of(&self, device: &str) -> Vec<Problem> {
        self.problems().into_iter().filter(|p| p.device == device).collect()
    }

    /// Entrées du journal sur les problèmes (début et fin) des `days` derniers jours, la plus récente d'abord.
    pub fn recent_problems(&self, limit: usize, days: i64) -> Vec<super::Alert> {
        let since = Local::now() - chrono::Duration::days(days);
        self.journal_query(limit, |e| e.at >= since && problems::parse_key(&e.key).is_some())
    }

    /// Un problème vu par l'utilisateur : il quitte la liste des problèmes en cours.
    pub fn dismiss_problem(&self, id: u64, source: Source) {
        let taken = self.problems.lock().unwrap().board.take(id);
        if let Some(p) = taken {
            let e = Entry::new(Level::Info, source, &p.device, &p.device_name, format!("Problème vu : {}", p.kind.title().to_lowercase()))
                .with_key(problems::key(p.kind, &p.device, true));
            self.record(e);
        }
    }

    // ---- Journal -----------------------------------------------------------------------------

    /// Ouvre (ou met à jour) un problème ; inscrit au journal s'il est nouveau ou s'aggrave.
    pub(super) fn raise(&self, kind: ProblemKind, device: &str, level: Level, detail: String) {
        let name = self.name_of(device);
        let p = self.problems.lock().unwrap().board.open(kind, device, &name, level, detail, Local::now());
        if let Some(p) = p {
            let e = Entry::new(p.level, kind.source(), device, &name, p.message()).with_key(problems::key(kind, device, false));
            self.record_synced(e);
        }
    }

    /// Clôt un problème en cours (rien s'il ne l'est pas) : « Résolu — Hors ligne : … ».
    pub(super) fn settle(&self, kind: ProblemKind, device: &str, detail: &str) {
        let closed = self.problems.lock().unwrap().board.close(kind, device);
        if let Some(p) = closed {
            let e = Entry::new(Level::Info, kind.source(), device, &p.device_name, format!("Résolu — {} : {detail}", kind.title())).with_key(problems::key(kind, device, true));
            self.record(e);
        }
    }

    fn is_open(&self, kind: ProblemKind, device: &str) -> bool {
        self.problems.lock().unwrap().board.is_open(kind, device)
    }

    pub(super) fn forget_problems(&self, device: &str) {
        let mut st = self.problems.lock().unwrap();
        st.board.forget(device);
        st.holds.forget(device);
        st.traces.remove(device);
    }

    // ---- Appareils : liaison ----------------------------------------------------------------------

    /// La liaison vient d'être perdue (appareil jusque-là en ligne).
    pub(super) fn note_lost(&self, dev: &DeviceHandle, error: &str) {
        let (on, until) = {
            let cfg = dev.cfg.read().unwrap();
            let st = dev.state.read().unwrap();
            let kind = kinds::detect(cfg.kind.as_deref(), &st.dps);
            let on = kind.switch_dp(&st.dps).and_then(|d| st.dps.get(&d)).and_then(Value::as_bool);
            let until = super::countdown_left(&st).map(|l| Instant::now() + Duration::from_secs(l));
            (on, until)
        };
        let mut w = dev.watch.lock().unwrap();
        (w.lost_at, w.last_error, w.was_on, w.countdown_until, w.reconnected) = (Some(Instant::now()), error.to_string(), on, until, None);
    }

    /// Nouvelle erreur pendant que l'appareil est hors ligne.
    pub(super) fn note_error(&self, dev: &DeviceHandle, error: &str) {
        let mut w = dev.watch.lock().unwrap();
        w.last_error = error.to_string();
        w.lost_at.get_or_insert_with(Instant::now);
    }

    /// Connexion établie : fin de la perte de liaison.
    pub(super) fn note_connected(&self, dev: &DeviceHandle) {
        let id = dev.cfg.read().unwrap().id.clone();
        let known = !dev.state.read().unwrap().dps.is_empty();
        let outage = {
            let mut w = dev.watch.lock().unwrap();
            let lost = w.lost_at.take();
            let left = w.countdown_until.take().map(|u| u.saturating_duration_since(Instant::now()).as_secs());
            let outage = lost.map(|t| t.elapsed());
            // Le premier état reçu dira ce qui a changé pendant la perte (seulement si on connaissait l'état d'avant).
            w.reconnected = match (outage, known) {
                (Some(d), true) => Some((Instant::now(), d, left)),
                _ => None,
            };
            outage
        };
        if let Some(d) = outage {
            self.settle(ProblemKind::Offline, &id, &format!("de nouveau en ligne (hors ligne {})", kinds::fmt_secs(d.as_secs())));
        }
    }

    /// Premier état complet reçu après une reconnexion : (durée de la perte, minuterie qui restait).
    pub(super) fn take_reconnected(&self, dev: &DeviceHandle, update: &Map<String, Value>, switch_dp: Option<&str>) -> Option<(Duration, Option<u64>)> {
        let mut w = dev.watch.lock().unwrap();
        let (at, d, left) = w.reconnected?;
        if at.elapsed() > Duration::from_secs(30) {
            w.reconnected = None;
            return None;
        }
        if !switch_dp.is_some_and(|s| update.contains_key(s)) {
            return None;
        }
        w.reconnected = None;
        Some((d, left))
    }

    /// Redémarrage de la prise pendant la perte de liaison : sa minuterie est revenue à zéro.
    pub(super) fn check_restart(&self, dev: &DeviceHandle, outage: (Duration, Option<u64>), update: &Map<String, Value>, countdown_dp: Option<&str>, switch_changed: bool) {
        let now = countdown_dp.and_then(|d| update.get(d)).and_then(Value::as_u64);
        if switch_changed || !problems::restarted(outage.1, now) {
            return;
        }
        let cfg = dev.cfg.read().unwrap().clone();
        let rearm = cfg.safety.enabled && cfg.safety.max_on_minutes.is_some();
        self.raise(ProblemKind::Restarted, &cfg.id, Level::Warning, problems::restart_detail(outage.0, rearm));
    }

    // ---- Appareils : changements faits ailleurs -----------------------------------------------------

    /// Un changement venu de l'appareil, que HomeSmart n'a pas envoyé : jugé un peu plus tard.
    pub(super) fn defer_external(self: &Arc<Self>, dev: &Arc<DeviceHandle>, change: ExternalChange) {
        let (hub, dev) = (self.clone(), dev.clone());
        tokio::spawn(async move {
            tokio::time::sleep(EXTERNAL_VERDICT).await;
            hub.external_verdict(&dev, change);
        });
    }

    fn external_verdict(&self, dev: &DeviceHandle, c: ExternalChange) {
        let id = dev.cfg.read().unwrap().id.clone();
        // La ligne de commande l'a-t-elle fait ? (elle l'inscrit elle-même au journal)
        let me = std::process::id();
        let since = c.at - chrono::Duration::seconds(10);
        let (by_cli, fresh) = {
            let mut j = self.journal.lock().unwrap();
            let fresh = j.poll_external();
            (!j.query(1, |e| e.device == id && e.pid != me && e.at >= since).is_empty(), fresh)
        };
        if let Some(e) = fresh.last() {
            self.emit(super::Event::Alert(e.id));
        }
        if by_cli {
            return;
        }
        let problem = c.switch.as_ref().and_then(|(outputs, s)| problems::classify_switch(s).map(|(k, l, why)| (k, l, why, outputs.clone())));
        match problem {
            Some((kind, level, why, outputs)) => {
                // Le problème décrit lui-même le changement ; les autres DP changés sont inscrits à part.
                let rest: Vec<&String> = c.parts.iter().filter(|p| !outputs.contains(p)).collect();
                if !rest.is_empty() {
                    self.log(Level::Info, Source::Device, &id, rest.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" · "));
                }
                let detail = if outputs.len() == 1 && !outputs[0].contains(':') { why } else { format!("{} ; {why}", outputs.join(" · ")) };
                self.raise(kind, &id, level, detail);
            }
            None => self.log(Level::Info, Source::Device, &id, format!("{}{}", c.parts.join(" · "), c.suffix)),
        }
    }

    // ---- Appareils : mesures et défauts -------------------------------------------------------------

    /// Après chaque mise à jour d'état : défauts (DP 26), tension (DP 20), consommation.
    pub(super) fn watch_measures(&self, dev: &DeviceHandle, update: &Map<String, Value>, on: Option<bool>, power: Option<f64>, fresh_power: bool) {
        let (cfg, dps) = (dev.cfg.read().unwrap().clone(), dev.state.read().unwrap().dps.clone());
        let (bits, labels) = kinds::faults(&dps, &cfg.schema);
        let fault = {
            let mut w = dev.watch.lock().unwrap();
            (bits != w.faults).then(|| std::mem::replace(&mut w.faults, bits))
        };
        if let Some(before) = fault {
            if bits != 0 {
                let cuts = cfg.safety.enabled && cfg.safety.off_on_fault && on == Some(true);
                let (level, text) = problems::fault_problem(&labels, cuts);
                self.raise(ProblemKind::Fault, &cfg.id, level, text);
            } else if before != 0 {
                self.settle(ProblemKind::Fault, &cfg.id, "plus aucun défaut signalé par la prise");
            }
        }
        // Boîte noire : chaque mesure de puissance ou de tension d'une prise à mesure.
        let volts = kinds::voltage_v(&dps, &cfg.schema);
        if volts.is_some() && (update.contains_key("19") || update.contains_key("20")) {
            let watts = power.or_else(|| kinds::power_w(&dps, &cfg.schema));
            let mut st = self.problems.lock().unwrap();
            let t = st.traces.entry(cfg.id.clone()).or_default();
            (t.device, t.name) = (cfg.id.clone(), cfg.display_name().to_string());
            t.push(Local::now(), watts, volts);
        }
        if update.contains_key("20") {
            if let Some(v) = kinds::voltage_v(&dps, &cfg.schema) {
                let found = dev.watch.lock().unwrap().voltage.update(v);
                self.apply_finding(ProblemKind::Voltage, &cfg.id, found);
            }
        }
        if let (true, Some(w), Some(on)) = (fresh_power, power, on) {
            let stale = dev.watch.lock().unwrap().typical.is_none_or(|(_, t)| t.elapsed() > TYPICAL_REFRESH);
            if stale {
                let typ = self.energy.lock().unwrap().typical_active_w(&cfg.id, Local::now().date_naive());
                dev.watch.lock().unwrap().typical = Some((typ, Instant::now()));
            }
            let (_, after, _, _) = self.delays();
            let found = {
                let mut wt = dev.watch.lock().unwrap();
                let typ = wt.typical.and_then(|t| t.0);
                wt.power.update_after(Instant::now(), on, w, typ, crate::automation::looks_risky(cfg.display_name()), after)
            };
            self.apply_finding(ProblemKind::LowPower, &cfg.id, found);
        }
    }

    fn apply_finding(&self, kind: ProblemKind, id: &str, f: Option<Finding>) {
        match f {
            Some(Finding::Open(level, text)) => self.raise(kind, id, level, text),
            Some(Finding::Close(text)) => self.settle(kind, id, &text),
            None => {}
        }
    }

    // ---- Surveillance périodique ------------------------------------------------------------------------

    /// Toutes les 5 s : appareils hors ligne, caméras, analyse et écoute, événements trop vieux.
    pub(super) fn problems_tick(self: &Arc<Self>) {
        let (offline_after, _, camera_after, analysis_after) = self.delays();
        let devices: Vec<Arc<DeviceHandle>> = self.devices.read().unwrap().values().cloned().collect();
        let lost: Vec<&Arc<DeviceHandle>> = devices.iter().filter(|d| !d.state.read().unwrap().online).collect();
        let all_lost = devices.len() >= 2 && lost.len() == devices.len();
        for d in lost {
            let cfg = d.cfg.read().unwrap().clone();
            let info = {
                let w = d.watch.lock().unwrap();
                let Some(t) = w.lost_at.filter(|t| t.elapsed() >= offline_after) else { continue };
                OfflineInfo {
                    for_: t.elapsed(),
                    error: w.last_error.clone(),
                    guarded: cfg.safety.enabled,
                    was_on: w.was_on,
                    countdown_left: w.countdown_until.map(|u| u.saturating_duration_since(Instant::now()).as_secs()).filter(|l| *l > 0),
                    all_lost,
                    robot: kinds::detect(cfg.kind.as_deref(), &d.state.read().unwrap().dps) == Kind::Vacuum,
                }
            };
            let (level, text) = problems::offline_problem(&info);
            self.raise(ProblemKind::Offline, &cfg.id, level, text);
        }
        self.camera_problems_tick(offline_after, camera_after);
        self.vacuum_problems_tick(offline_after);
        self.analysis_problems_tick(analysis_after);
        self.problems.lock().unwrap().board.expire(Local::now());
    }

    /// Analyse IA et écoute arrêtées sur erreur.
    fn analysis_problems_tick(&self, after: Duration) {
        let now = Instant::now();
        let armed = self.vision_armed();
        for c in self.cameras() {
            let cv = self.camera_vision(&c.id);
            let a = self.camera_analysis(&c.id);
            let err = a.as_ref().filter(|a| cv.enabled && !a.starting).and_then(|a| a.error.clone());
            let held = self.problems.lock().unwrap().holds.hold(ProblemKind::VisionStopped, &c.id, err.is_some(), now);
            match (held.filter(|d| *d >= after), err) {
                (Some(_), Some(e)) => {
                    let mut watching = Vec::new();
                    if cv.falls {
                        watching.push("les chutes");
                    }
                    if cv.gestures {
                        watching.push("les appels à l'aide");
                    }
                    if armed {
                        watching.push("les intrusions (mode absence)");
                    }
                    let (level, text) = problems::vision_stopped(&e, &watching);
                    self.raise(ProblemKind::VisionStopped, &c.id, level, text);
                }
                (None, _) if self.is_open(ProblemKind::VisionStopped, &c.id) => {
                    let why = if cv.enabled { "l'analyse fonctionne de nouveau" } else { "analyse désactivée" };
                    self.settle(ProblemKind::VisionStopped, &c.id, why);
                }
                _ => {}
            }
            let listen = cv.enabled && (cv.sounds || cv.speech);
            let l = self.camera_listening(&c.id);
            let err = l.filter(|_| listen).and_then(|l| l.error);
            let held = self.problems.lock().unwrap().holds.hold(ProblemKind::ListenStopped, &c.id, err.is_some(), now);
            match (held.filter(|d| *d >= after), err) {
                (Some(_), Some(e)) => {
                    let (level, text) = problems::listen_stopped(&e);
                    self.raise(ProblemKind::ListenStopped, &c.id, level, text);
                }
                (None, _) if self.is_open(ProblemKind::ListenStopped, &c.id) => {
                    let why = if listen { "l'écoute fonctionne de nouveau" } else { "écoute désactivée" };
                    self.settle(ProblemKind::ListenStopped, &c.id, why);
                }
                _ => {}
            }
        }
    }

    pub(super) fn problem_open(&self, kind: ProblemKind, id: &str) -> bool {
        self.is_open(kind, id)
    }

    // ---- Session précédente : arrêt brutal de ce PC ----------------------------------------------------------

    /// Au démarrage : lit la marque de la session précédente, écrit celle-ci, la rafraîchit toutes
    /// les 5 s avec la boîte noire des prises ; 20 s plus tard (les prises ont répondu), dit comment
    /// la précédente s'est terminée et si ce PC a perdu son alimentation depuis.
    pub(super) fn start_session_watch(self: &Arc<Self>) {
        let path = self.config_path.with_file_name("session.json");
        let previous = SessionMark::load(&path);
        let mark = SessionMark::new(Local::now());
        mark.save(&path);
        self.problems.lock().unwrap().session = Some((path, mark));
        let hub = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let st = {
                    let mut st = hub.problems.lock().unwrap();
                    let mut traces: Vec<Trace> = st.traces.values().filter(|t| !t.samples.is_empty()).cloned().collect();
                    traces.sort_by(|a, b| a.name.cmp(&b.name));
                    st.session.as_mut().filter(|(_, m)| !m.clean).map(|(p, m)| {
                        (m.alive, m.traces) = (Local::now(), traces);
                        (p.clone(), m.clone())
                    })
                };
                if let Some((path, mark)) = st {
                    mark.save(&path);
                }
            }
        });
        // Même après une fermeture propre : ce PC a pu perdre son alimentation depuis, HomeSmart fermé.
        let Some(prev) = previous else { return };
        let hub = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(20)).await;
            let log = tokio::task::spawn_blocking(problems::system_log).await.unwrap_or_default();
            hub.judge_previous_session(&prev, &log, problems::system_boot());
        });
    }

    /// Fermeture normale : la marque le dit.
    pub(super) fn end_session_watch(&self) {
        let st = self.problems.lock().unwrap().session.as_mut().map(|(p, m)| {
            (m.alive, m.clean) = (Local::now(), true);
            (p.clone(), m.clone())
        });
        if let Some((path, mark)) = st {
            mark.save(&path);
        }
    }

    /// Démo : ce que HomeSmart dirait au démarrage qui suit une coupure de courant de ce PC (avec la
    /// boîte noire des prises simulées).
    pub fn demo_host_shutdown(&self) {
        let now = Local::now();
        let traces: Vec<Trace> = self.problems.lock().unwrap().traces.values().cloned().collect();
        let prev = SessionMark { pid: 0, started: now - chrono::Duration::minutes(30), alive: now - chrono::Duration::seconds(3), clean: false, traces };
        let cut = problems::UnexpectedBoot { at: now, bugcheck: 0, power_button: false, cut_at: Some(now - chrono::Duration::seconds(1)) };
        self.judge_previous_session(&prev, &SystemLog { boots: vec![now], unexpected: vec![cut] }, Some(now));
    }

    pub(super) fn judge_previous_session(&self, prev: &SessionMark, log: &SystemLog, boot: Option<DateTime<Local>>) {
        let (from, to) = (prev.alive - chrono::Duration::minutes(3), prev.alive + chrono::Duration::seconds(15));
        let clues: Vec<problems::Clue> = self.journal_query(50, |e| e.at >= from && e.at <= to).iter().filter_map(problems::clue_of).collect();
        let list = self.list();
        let stays_off = |d: &super::DeviceView| kinds::after_power_cut(&d.dps, &d.schema) == Some("off");
        let host = self.config.lock().unwrap().devices.iter().find(|d| d.powers_pc).map(|d| d.id.clone());
        let host_view = host.as_ref().and_then(|id| list.iter().find(|d| &d.id == id));
        let powers_pc = host_view.map(|d| (d.name.as_str(), kinds::after_power_cut(&d.dps, &d.schema)));
        // Sans prise marquée « alimente ce PC » : celles qui restent éteintes après une coupure (sauf
        // les appareils sous garde-fous, un chauffage, pour qui rester éteint est conseillé).
        let others: Vec<String> = list
            .iter()
            .filter(|d| host.is_none() && !d.safety.enabled && matches!(kinds::detect(d.kind.as_deref(), &d.dps), Kind::Plug { .. } | Kind::Switch { .. }) && stays_off(d))
            .map(|d| d.name.clone())
            .collect();
        let s = problems::PreviousSession { mark: prev, log, boot, clues: &clues, powers_pc, stays_off: &others, now: Local::now() };
        if let Some((level, text)) = problems::host_shutdown(&s) {
            self.raise(ProblemKind::HostShutdown, host.as_deref().unwrap_or(""), level, text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::{self, CameraConfig, Quality};
    use crate::config::DeviceConfig;
    use crate::protocol::Version;
    use crate::sim::{self, SimCtl, SimSpec};
    use serde_json::json;

    /// Hub sur une configuration temporaire, délais de détection raccourcis.
    fn hub(tag: &str) -> (Arc<Hub>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("homesmart-problems-{tag}-{}", rand::random::<u32>()));
        let hub = Hub::new(dir.join("homesmart.json")).unwrap();
        hub.set_fast_detection(true);
        (hub, dir)
    }

    /// Prise simulée (même description que les vraies), ajoutée au hub.
    async fn plug(hub: &Arc<Hub>, id: &str, name: &str, dps: Value, watts: u32, safety: crate::automation::Safety) -> SimCtl {
        let key = *b"0123456789abcdef";
        let (port, ctl) = sim::spawn_ctl(SimSpec { id: id.into(), key, version: Version::V34, dps, watts }, "127.0.0.1:0").await.unwrap();
        hub.add(DeviceConfig {
            id: id.into(),
            key: "0123456789abcdef".into(),
            name: name.into(),
            ip: Some("127.0.0.1".into()),
            port: Some(port),
            version: Some("3.4".into()),
            kind: Some("plug".into()),
            schema: sim::plug_schema().as_object().cloned().unwrap(),
            safety,
            ..Default::default()
        })
        .unwrap();
        until(hub, 10, |h| h.view(id).is_some_and(|v| v.online && v.dps.contains_key("26"))).await;
        ctl
    }

    async fn until(hub: &Hub, secs: u64, pred: impl Fn(&Hub) -> bool) {
        let t0 = Instant::now();
        while !pred(hub) {
            let log: Vec<String> = hub.alerts(25).iter().map(|a| format!("{} [{:?}] {}", a.device_name, a.source, a.message)).collect();
            assert!(t0.elapsed() < Duration::from_secs(secs), "délai dépassé ; journal : {log:#?}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn problem(hub: &Hub, kind: ProblemKind, id: &str) -> Option<Problem> {
        hub.problems_of(id).into_iter().find(|p| p.kind == kind)
    }

    fn logged(hub: &Hub, source: Source, level: Level, text: &str) -> bool {
        hub.alerts(200).iter().any(|a| a.source == source && a.level == level && a.message.contains(text))
    }

    const OFFICE: &str = "bf00000000000office01";
    const HEATER: &str = "bf00000000000heater01";

    fn office_dps() -> Value {
        // Valeurs lues sur la vraie « Multiprise - Bureau » le 25/10/2026.
        json!({ "1": true, "9": 0, "18": 1372, "19": 2911, "20": 2407, "26": 0, "38": "off", "40": "relay" })
    }

    fn heater_dps() -> Value {
        json!({ "1": false, "9": 0, "18": 0, "19": 0, "20": 2392, "26": 0, "38": "memory", "40": "relay" })
    }

    /// Bouton de la prise, commande de HomeSmart, commande de la ligne de commande, allumage
    /// imprévu d'un chauffage : seuls les changements faits hors de HomeSmart sont des problèmes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn unrequested_switching() {
        let (hub, dir) = hub("switch");
        hub.start(false);
        let office = plug(&hub, OFFICE, "Multiprise - Bureau", office_dps(), 2911, Default::default()).await;
        let heater = plug(&hub, HEATER, "Chauffage", heater_dps(), 12_000, crate::automation::Safety::heater_preset()).await;

        // Bouton de la prise : extinction non demandée, inscrite par l'appareil, avec sa cause.
        office.press_button();
        until(&hub, 12, |h| problem(h, ProblemKind::UnexpectedOff, OFFICE).is_some()).await;
        let p = problem(&hub, ProblemKind::UnexpectedOff, OFFICE).unwrap();
        assert_eq!(p.level, Level::Warning);
        assert!(p.detail.contains("bouton de la prise"), "{}", p.detail);
        assert!(logged(&hub, Source::Device, Level::Warning, "Extinction non demandée — aucune commande de HomeSmart"));
        // Une seule entrée pour ce changement (pas d'« Éteint » en plus).
        assert!(!hub.alerts(200).iter().any(|a| a.device == OFFICE && a.message == "Éteint"), "{:#?}", hub.alerts(20));

        // Rallumée par HomeSmart : demandé, pas de problème.
        hub.power(OFFICE, true, None, Source::User).await.unwrap();
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert!(problem(&hub, ProblemKind::UnexpectedOn, OFFICE).is_none());

        // La ligne de commande (autre processus) l'éteint : elle l'inscrit elle-même, pas de problème.
        office.press_button();
        let mut cli = Entry::new(Level::Info, Source::Cli, OFFICE, "Multiprise - Bureau", "Éteint");
        cli.pid = std::process::id().wrapping_add(7);
        crate::journal::append_to_file(&hub.journal_path(), &cli);
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(problem(&hub, ProblemKind::UnexpectedOff, OFFICE).unwrap().count, 1, "commande de la ligne de commande comptée comme un problème");

        // Chauffage allumé par son bouton : grave (notification), ses garde-fous restent actifs.
        heater.press_button();
        until(&hub, 12, |h| problem(h, ProblemKind::UnexpectedOn, HEATER).is_some()).await;
        let p = problem(&hub, ProblemKind::UnexpectedOn, HEATER).unwrap();
        assert_eq!(p.level, Level::Critical);
        assert!(p.message().ends_with("ses garde-fous restent actifs"), "{}", p.message());
        // Le garde-fou pose sa minuterie de sécurité.
        until(&hub, 8, |h| h.view(HEATER).is_some_and(|v| v.countdown_left.is_some())).await;

        // Vu : quitte les problèmes en cours (et c'est inscrit).
        hub.dismiss_problem(p.id, Source::User);
        assert!(problem(&hub, ProblemKind::UnexpectedOn, HEATER).is_none());
        assert!(logged(&hub, Source::User, Level::Info, "Problème vu : allumage non demandé"));
        hub.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Minuterie : celle que pose HomeSmart (garde-fou) coupe « sur demande » ; celle réglée
    /// ailleurs (application Smart Life) est une extinction non demandée, sans gravité.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn timers_ours_and_elsewhere() {
        let (hub, dir) = hub("timer");
        hub.start(false);
        let office = plug(&hub, OFFICE, "Multiprise - Bureau", office_dps(), 2911, Default::default()).await;
        // Minuterie de 3 s réglée par HomeSmart.
        hub.set_countdown(OFFICE, 3, Source::User).await.unwrap();
        until(&hub, 12, |h| h.alerts(50).iter().any(|a| a.message == "Éteint (minuterie réglée par HomeSmart)")).await;
        assert!(problem(&hub, ProblemKind::UnexpectedOff, OFFICE).is_none());
        // Rallumée, puis minuterie de 3 s réglée ailleurs.
        hub.power(OFFICE, true, None, Source::User).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        let mut m = Map::new();
        m.insert("9".into(), json!(4));
        office.set(m);
        until(&hub, 15, |h| problem(h, ProblemKind::UnexpectedOff, OFFICE).is_some()).await;
        let p = problem(&hub, ProblemKind::UnexpectedOff, OFFICE).unwrap();
        assert_eq!(p.level, Level::Info);
        assert!(p.detail.contains("minuterie de la prise, réglée hors de HomeSmart"), "{}", p.detail);
        hub.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Coupure de courant : la prise du bureau revient éteinte (elle est réglée pour rester
    /// éteinte), celle du chauffage revient allumée mais sans sa minuterie (redémarrage).
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn power_cut_and_restart() {
        let (hub, dir) = hub("cut");
        hub.start(false);
        let office = plug(&hub, OFFICE, "Multiprise - Bureau", office_dps(), 2911, Default::default()).await;
        let heater = plug(&hub, HEATER, "Chauffage", heater_dps(), 12_000, crate::automation::Safety::heater_preset()).await;
        hub.power(HEATER, true, None, Source::User).await.unwrap();
        until(&hub, 8, |h| h.view(HEATER).is_some_and(|v| v.countdown_left.is_some_and(|c| c > 600))).await;

        let (o, h2) = (office.clone(), heater.clone());
        tokio::spawn(async move { o.power_cut(9.0).await });
        tokio::spawn(async move { h2.power_cut(9.0).await });
        // Hors ligne au bout de 5 s (délais de la démo), pour les deux.
        until(&hub, 15, |h| problem(h, ProblemKind::Offline, OFFICE).is_some() && problem(h, ProblemKind::Offline, HEATER).is_some()).await;
        let p = problem(&hub, ProblemKind::Offline, OFFICE).unwrap();
        assert!(p.detail.contains("tous les appareils sont perdus en même temps"), "{}", p.detail);
        // Chauffage allumé, sa minuterie de sécurité court encore : avertissement, pas critique.
        let p = problem(&hub, ProblemKind::Offline, HEATER).unwrap();
        assert_eq!(p.level, Level::Warning);
        assert!(p.detail.contains("sa minuterie de sécurité l'éteindra dans"), "{}", p.detail);

        // Retour du courant.
        until(&hub, 40, |h| problem(h, ProblemKind::UnexpectedOff, OFFICE).is_some() && problem(h, ProblemKind::Restarted, HEATER).is_some()).await;
        let p = problem(&hub, ProblemKind::UnexpectedOff, OFFICE).unwrap();
        assert!(p.detail.starts_with("coupure de courant probable (hors ligne") && p.detail.contains("Rester éteinte"), "{}", p.detail);
        let p = problem(&hub, ProblemKind::Restarted, HEATER).unwrap();
        assert!(p.detail.contains("minuterie à zéro") && p.detail.contains("HomeSmart la reprogramme"), "{}", p.detail);
        assert!(problem(&hub, ProblemKind::Offline, OFFICE).is_none(), "hors ligne résolu");
        assert!(logged(&hub, Source::System, Level::Info, "Résolu — Hors ligne : de nouveau en ligne (hors ligne"));
        // Le garde-fou reprogramme la minuterie perdue.
        until(&hub, 10, |h| h.view(HEATER).is_some_and(|v| v.countdown_left.is_some_and(|c| c > 600))).await;
        hub.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Défaut « sous-tension » (DP 26), creux de tension (DP 20), puis retour à la normale.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn fault_and_voltage() {
        let (hub, dir) = hub("volt");
        hub.start(false);
        hub.set_prefs(crate::config::Prefs { meter_interval_s: 1.0, ..Default::default() });
        let office = plug(&hub, OFFICE, "Multiprise - Bureau", office_dps(), 2911, Default::default()).await;

        office.set_fault(1 << 4);
        until(&hub, 8, |h| problem(h, ProblemKind::Fault, OFFICE).is_some()).await;
        let p = problem(&hub, ProblemKind::Fault, OFFICE).unwrap();
        assert_eq!(p.level, Level::Warning);
        assert!(p.message().starts_with("Défaut signalé — la prise signale une sous-tension : la tension du secteur a chuté"), "{}", p.message());
        office.set_fault(0);
        until(&hub, 8, |h| problem(h, ProblemKind::Fault, OFFICE).is_none()).await;
        assert!(logged(&hub, Source::Device, Level::Info, "Résolu — Défaut signalé : plus aucun défaut"));

        office.set_voltage(180.0);
        until(&hub, 8, |h| problem(h, ProblemKind::Voltage, OFFICE).is_some()).await;
        let p = problem(&hub, ProblemKind::Voltage, OFFICE).unwrap();
        assert!(p.detail.starts_with("180 V mesurés (normale : 207 à 253 V)"), "{}", p.detail);
        office.set_voltage(231.0);
        until(&hub, 12, |h| problem(h, ProblemKind::Voltage, OFFICE).is_none()).await;
        assert!(logged(&hub, Source::Device, Level::Info, "Résolu — Tension anormale : tension revenue à la normale"));
        // Les problèmes sont écrits sur le disque tout de suite (ils survivent à une coupure).
        let text = std::fs::read_to_string(hub.journal_path()).unwrap();
        assert!(text.contains("probleme:tension:") && text.contains("probleme:defaut:"));
        hub.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Chauffage allumé qui ne tire que 25 W alors qu'il chauffe d'habitude à 1 200 W : simple
    /// information, et seulement s'il n'a jamais chauffé depuis l'allumage.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn heater_that_does_not_heat() {
        let (hub, dir) = hub("low");
        hub.seed_demo_energy(HEATER, 1200.0);
        hub.start(false);
        hub.set_prefs(crate::config::Prefs { meter_interval_s: 1.0, ..Default::default() });
        let heater = plug(&hub, HEATER, "Chauffage", heater_dps(), 12_000, Default::default()).await;
        heater.set_load(25.0);
        hub.power(HEATER, true, None, Source::User).await.unwrap();
        until(&hub, 40, |h| problem(h, ProblemKind::LowPower, HEATER).is_some()).await;
        let p = problem(&hub, ProblemKind::LowPower, HEATER).unwrap();
        assert_eq!(p.level, Level::Info);
        assert!(p.detail.contains("ne consomme que 2") && p.detail.contains("en marche) : son thermostat"), "{}", p.detail);
        // Il se met à chauffer : résolu.
        heater.set_load(1200.0);
        until(&hub, 10, |h| problem(h, ProblemKind::LowPower, HEATER).is_none()).await;
        hub.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Démarrage après une coupure de courant de ce PC : la marque de session n'est pas « propre »,
    /// Windows a redémarré sans arrêt normal, et une prise a signalé une sous-tension juste avant.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn previous_session_cut_by_the_office_plug() {
        let (hub, dir) = hub("host");
        hub.start(false);
        plug(&hub, OFFICE, "Multiprise - Bureau", office_dps(), 2911, Default::default()).await;
        hub.update(OFFICE, super::super::DevicePatch { powers_pc: Some(true), ..Default::default() }).unwrap();
        until(&hub, 10, |h| h.view(OFFICE).is_some_and(|v| v.online && v.powers_pc && v.dps.contains_key("38"))).await;
        let now = Local::now();
        hub.record(Entry::new(Level::Warning, Source::Device, HEATER, "Chauffage", "défaut signalé par l'appareil : sous-tension"));
        // Boîte noire de cette session (la prise a envoyé ses mesures).
        until(&hub, 10, |h| h.problems.lock().unwrap().traces.get(OFFICE).is_some_and(|t| !t.samples.is_empty())).await;
        let traces: Vec<Trace> = hub.problems.lock().unwrap().traces.values().cloned().collect();
        let prev = SessionMark { pid: 1, started: now - chrono::Duration::minutes(10), alive: now + chrono::Duration::seconds(5), clean: false, traces };
        let boot = now + chrono::Duration::minutes(2);
        let cut = problems::UnexpectedBoot { at: boot, bugcheck: 0, power_button: false, cut_at: Some(now + chrono::Duration::seconds(7)) };
        hub.judge_previous_session(&prev, &SystemLog { boots: vec![boot], unexpected: vec![cut] }, Some(boot));
        let p = problem(&hub, ProblemKind::HostShutdown, OFFICE).expect("arrêt brutal attaché à la prise qui alimente ce PC");
        assert_eq!(p.level, Level::Warning);
        assert!(p.detail.starts_with("ce PC a perdu son alimentation"), "{}", p.detail);
        assert!(p.detail.contains("« Chauffage » a signalé une sous-tension"), "{}", p.detail);
        assert!(p.detail.contains("la prise « Multiprise - Bureau » qui l'alimente revient éteinte après une coupure"), "{}", p.detail);
        assert!(p.detail.contains("dernières mesures : « Multiprise - Bureau » 29"), "{}", p.detail);
        assert!(logged(&hub, Source::System, Level::Warning, "Arrêt brutal de ce PC — ce PC a perdu son alimentation à"));
        // La marque de session de ce démarrage est écrite, avec la boîte noire ; fermée proprement à l'arrêt.
        let path = dir.join("session.json");
        until(&hub, 12, |_| SessionMark::load(&path).is_some_and(|m| m.traces.iter().any(|t| t.name == "Multiprise - Bureau"))).await;
        assert!(!SessionMark::load(&path).unwrap().clean);
        hub.shutdown();
        assert!(SessionMark::load(&path).unwrap().clean);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Caméra simulée dont le flux est coupé : « Flux vidéo perdu », puis résolu.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn camera_stream_lost() {
        let (hub, dir) = hub("cam");
        std::env::set_var("HOMESMART_CAPTURES", dir.join("captures"));
        hub.start(false);
        let sim = camera::sim::start("CODE1234", 0).await.unwrap();
        let mut cfg = CameraConfig::new("127.0.0.1");
        (cfg.rtsp_port, cfg.onvif_port, cfg.password, cfg.name) = (sim.rtsp_port, sim.onvif_port, "CODE1234".into(), "Salon".into());
        let id = hub.add_camera(cfg, Source::User).unwrap().id;
        // Flux demandé en continu (comme pour l'analyse IA).
        hub.camera_vision_want(&id, Some(Quality::Sub));
        until(&hub, 20, |h| h.camera_frame(&id).is_some()).await;
        sim.state.down.store(true, std::sync::atomic::Ordering::Relaxed);
        until(&hub, 40, |h| problem(h, ProblemKind::StreamLost, &id).is_some()).await;
        assert!(logged(&hub, Source::System, Level::Warning, "Flux vidéo perdu — pas d'image depuis"));
        sim.state.down.store(false, std::sync::atomic::Ordering::Relaxed);
        until(&hub, 60, |h| problem(h, ProblemKind::StreamLost, &id).is_none()).await;
        assert!(logged(&hub, Source::System, Level::Info, "Résolu — Flux vidéo perdu"));
        hub.camera_vision_want(&id, None);
        hub.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }
}
