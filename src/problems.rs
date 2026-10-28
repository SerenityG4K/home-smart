//! Détection des problèmes : ce qui arrive aux appareils, aux caméras et à ce PC sans qu'on
//! l'ait demandé.
//!
//! - prises : extinction ou allumage non demandés, avec la cause probable (bouton de la prise,
//!   autre application, minuterie de la prise, coupure de courant, protection) ; hors ligne ;
//!   redémarrage ; défauts signalés (DP 26) ; tension anormale ; consommation inhabituelle ;
//! - caméras : injoignable, flux vidéo perdu, analyse IA ou écoute arrêtées sur erreur ;
//! - aspirateurs robots : erreurs signalées par le robot (brosse bloquée, bac plein, robot
//!   coincé…), avec ce qu'il faut faire ;
//! - ce PC : arrêt brutal (coupure de courant) pendant que HomeSmart tournait. HomeSmart tourne
//!   souvent sur un PC branché sur une des prises : quand elle coupe, il meurt avec lui. Au
//!   démarrage suivant, la marque de session (avec la dernière minute de mesures des prises), le
//!   journal de Windows et les dernières lignes du journal (creux de tension signalés juste
//!   avant) disent ce qui s'est passé, y compris les coupures survenues HomeSmart fermé.
//!
//! Les règles sont des fonctions pures, testées sans réseau. Le hub les applique et inscrit chaque
//! problème au journal avec sa source et sa gravité ; les critiques déclenchent une notification
//! du système.

use crate::journal::{Entry, Level, Source};
use crate::kinds::{fmt_secs, fr};
use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

/// Hors ligne pendant ce délai avant d'être signalé (une reconnexion rapide n'est pas un problème).
pub const OFFLINE_AFTER: Duration = Duration::from_secs(60);
/// Allumé sans consommer comme d'habitude pendant ce délai avant d'être signalé.
pub const LOW_POWER_AFTER: Duration = Duration::from_secs(15 * 60);
/// Caméra sans image, ou analyse en erreur, pendant ce délai avant d'être signalée.
pub const CAMERA_AFTER: Duration = Duration::from_secs(20);
pub const ANALYSIS_AFTER: Duration = Duration::from_secs(30);
/// Un événement (extinction non demandée…) reste « en cours » au plus ce temps, si personne ne l'a vu.
pub const EVENT_TTL_H: i64 = 24;
/// Préfixe de la clé des entrées du journal qui décrivent un problème.
pub const KEY_PREFIX: &str = "probleme:";

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ProblemKind {
    UnexpectedOff,
    UnexpectedOn,
    Offline,
    Restarted,
    Fault,
    Voltage,
    LowPower,
    HostShutdown,
    CameraOffline,
    StreamLost,
    VisionStopped,
    ListenStopped,
    VacuumError,
    CameraTampered,
}

impl ProblemKind {
    pub const ALL: [ProblemKind; 14] = [
        ProblemKind::UnexpectedOff,
        ProblemKind::UnexpectedOn,
        ProblemKind::Offline,
        ProblemKind::Restarted,
        ProblemKind::Fault,
        ProblemKind::Voltage,
        ProblemKind::LowPower,
        ProblemKind::HostShutdown,
        ProblemKind::CameraOffline,
        ProblemKind::StreamLost,
        ProblemKind::VisionStopped,
        ProblemKind::ListenStopped,
        ProblemKind::VacuumError,
        ProblemKind::CameraTampered,
    ];

    /// Nom court dans les clés du journal.
    pub fn slug(self) -> &'static str {
        match self {
            ProblemKind::UnexpectedOff => "extinction",
            ProblemKind::UnexpectedOn => "allumage",
            ProblemKind::Offline => "hors-ligne",
            ProblemKind::Restarted => "redemarrage",
            ProblemKind::Fault => "defaut",
            ProblemKind::Voltage => "tension",
            ProblemKind::LowPower => "consommation",
            ProblemKind::HostShutdown => "arret-pc",
            ProblemKind::CameraOffline => "camera",
            ProblemKind::StreamLost => "flux",
            ProblemKind::VisionStopped => "analyse",
            ProblemKind::ListenStopped => "ecoute",
            ProblemKind::VacuumError => "aspirateur",
            ProblemKind::CameraTampered => "sabotage",
        }
    }

    pub fn from_slug(s: &str) -> Option<ProblemKind> {
        ProblemKind::ALL.into_iter().find(|k| k.slug() == s)
    }

    pub fn title(self) -> &'static str {
        match self {
            ProblemKind::UnexpectedOff => "Extinction non demandée",
            ProblemKind::UnexpectedOn => "Allumage non demandé",
            ProblemKind::Offline => "Hors ligne",
            ProblemKind::Restarted => "Redémarrage de la prise",
            ProblemKind::Fault => "Défaut signalé",
            ProblemKind::Voltage => "Tension anormale",
            ProblemKind::LowPower => "Consommation inhabituelle",
            ProblemKind::HostShutdown => "Arrêt brutal de ce PC",
            ProblemKind::CameraOffline => "Caméra injoignable",
            ProblemKind::StreamLost => "Flux vidéo perdu",
            ProblemKind::VisionStopped => "Analyse IA arrêtée",
            ProblemKind::ListenStopped => "Écoute arrêtée",
            ProblemKind::VacuumError => "Aspirateur en difficulté",
            ProblemKind::CameraTampered => "Sabotage d'une caméra",
        }
    }

    /// Événement ponctuel (il reste affiché jusqu'à ce qu'on l'ait vu), plutôt qu'un état qui
    /// dure et disparaît de lui-même quand tout redevient normal.
    pub fn is_event(self) -> bool {
        matches!(self, ProblemKind::UnexpectedOff | ProblemKind::UnexpectedOn | ProblemKind::Restarted | ProblemKind::HostShutdown)
    }

    /// Origine inscrite au journal.
    pub fn source(self) -> Source {
        match self {
            ProblemKind::UnexpectedOff | ProblemKind::UnexpectedOn | ProblemKind::Restarted | ProblemKind::Fault | ProblemKind::Voltage | ProblemKind::VacuumError => {
                Source::Device
            }
            ProblemKind::VisionStopped => Source::Vision,
            ProblemKind::ListenStopped => Source::Listen,
            _ => Source::System,
        }
    }
}

/// Clé du journal d'un problème (`fin` : l'entrée qui le clôt).
pub fn key(kind: ProblemKind, device: &str, end: bool) -> String {
    format!("{KEY_PREFIX}{}:{device}{}", kind.slug(), if end { ":fin" } else { "" })
}

/// (sorte, appareil, entrée de fin) d'après la clé d'une entrée du journal.
pub fn parse_key(k: &str) -> Option<(ProblemKind, &str, bool)> {
    let rest = k.strip_prefix(KEY_PREFIX)?;
    let (slug, device) = rest.split_once(':').unwrap_or((rest, ""));
    let (device, end) = match device.strip_suffix(":fin") {
        Some(d) => (d, true),
        None => (device, false),
    };
    Some((ProblemKind::from_slug(slug)?, device, end))
}

fn rank(l: Level) -> u8 {
    match l {
        Level::Info => 0,
        Level::Warning => 1,
        Level::Critical => 2,
    }
}

/// Un problème en cours.
#[derive(Clone, Debug)]
pub struct Problem {
    pub id: u64,
    pub kind: ProblemKind,
    /// Appareil ou caméra concerné (vide : ce PC).
    pub device: String,
    pub device_name: String,
    pub level: Level,
    /// Ce qui se passe et la cause probable.
    pub detail: String,
    pub since: DateTime<Local>,
    pub last: DateTime<Local>,
    /// Nombre de fois (un événement qui se répète).
    pub count: u32,
}

impl Problem {
    /// Texte du journal et de la notification : « Extinction non demandée — … ».
    pub fn message(&self) -> String {
        format!("{} — {}", self.kind.title(), self.detail)
    }
}

/// Problèmes en cours.
#[derive(Default)]
pub struct Board {
    active: Vec<Problem>,
    next: u64,
}

impl Board {
    /// Ouvre un problème, ou met à jour celui de même sorte sur le même appareil. Renvoie ce qu'il
    /// faut inscrire au journal : un problème nouveau, un événement qui se répète, ou un état qui
    /// s'aggrave (un état qui dure n'est pas réinscrit).
    pub fn open(&mut self, kind: ProblemKind, device: &str, name: &str, level: Level, detail: String, now: DateTime<Local>) -> Option<Problem> {
        if let Some(p) = self.active.iter_mut().find(|p| p.kind == kind && p.device == device) {
            let worse = rank(level) > rank(p.level);
            let repeat = kind.is_event();
            (p.detail, p.last, p.device_name) = (detail, now, name.to_string());
            if repeat {
                (p.count, p.level) = (p.count + 1, level);
            } else if worse {
                p.level = level;
            }
            return (worse || repeat).then(|| p.clone());
        }
        self.next += 1;
        let p = Problem { id: self.next, kind, device: device.to_string(), device_name: name.to_string(), level, detail, since: now, last: now, count: 1 };
        self.active.push(p.clone());
        Some(p)
    }

    /// Clôt le problème s'il est en cours.
    pub fn close(&mut self, kind: ProblemKind, device: &str) -> Option<Problem> {
        let i = self.active.iter().position(|p| p.kind == kind && p.device == device)?;
        Some(self.active.remove(i))
    }

    /// Retire un problème (vu par l'utilisateur).
    pub fn take(&mut self, id: u64) -> Option<Problem> {
        let i = self.active.iter().position(|p| p.id == id)?;
        Some(self.active.remove(i))
    }

    /// Événements plus vieux que `EVENT_TTL_H` : retirés.
    pub fn expire(&mut self, now: DateTime<Local>) -> Vec<Problem> {
        let old = |p: &Problem| p.kind.is_event() && now - p.last > chrono::Duration::hours(EVENT_TTL_H);
        let out: Vec<Problem> = self.active.iter().filter(|p| old(p)).cloned().collect();
        self.active.retain(|p| !old(p));
        out
    }

    pub fn forget(&mut self, device: &str) {
        self.active.retain(|p| p.device != device);
    }

    pub fn is_open(&self, kind: ProblemKind, device: &str) -> bool {
        self.active.iter().any(|p| p.kind == kind && p.device == device)
    }

    /// Les problèmes en cours, les plus graves puis les plus récents d'abord.
    pub fn active(&self) -> Vec<Problem> {
        let mut v = self.active.clone();
        v.sort_by(|a, b| rank(b.level).cmp(&rank(a.level)).then(b.last.cmp(&a.last)));
        v
    }
}

/// Résultat d'une surveillance continue (tension, consommation).
#[derive(Debug, PartialEq)]
pub enum Finding {
    Open(Level, String),
    Close(String),
}

fn watts(w: f64) -> String {
    if w.abs() >= 1000.0 {
        format!("{} kW", fr(w / 1000.0, 1))
    } else {
        format!("{} W", fr(w, 0))
    }
}

fn secs(d: Duration) -> String {
    fmt_secs(d.as_secs())
}

// ---- Marche / arrêt -----------------------------------------------------------------------

/// Ce qu'on sait d'un changement de marche/arrêt que HomeSmart n'a pas demandé.
#[derive(Clone, Debug, Default)]
pub struct SwitchChange {
    pub on: bool,
    /// La minuterie de la prise arrivait à échéance.
    pub by_timer: bool,
    /// Cette minuterie avait été réglée par HomeSmart (toi, un programme, un garde-fou).
    pub timer_ours: bool,
    /// Premier état reçu après une perte de liaison de cette durée.
    pub after_outage: Option<Duration>,
    /// Comportement au retour du courant (`kinds::after_power_cut`).
    pub after_cut: Option<&'static str>,
    /// Défauts signalés en même temps (libellés).
    pub faults: Vec<String>,
    /// Appareil sous garde-fous (chauffage…) : un allumage imprévu est grave.
    pub guarded: bool,
}

/// Un changement de marche/arrêt venu de l'appareil est-il un problème ? `None` : c'est HomeSmart
/// qui l'a demandé (sa propre minuterie). Sinon la sorte, la gravité et la cause probable.
pub fn classify_switch(c: &SwitchChange) -> Option<(ProblemKind, Level, String)> {
    if c.by_timer && c.timer_ours {
        return None;
    }
    let outage = c.after_outage.map(secs);
    if !c.on {
        let (level, why) = if !c.faults.is_empty() {
            let (level, advice) = fault_advice(&c.faults);
            (level, format!("coupée par sa protection ({}){advice}", c.faults.join(", ")))
        } else if c.by_timer {
            (Level::Info, "minuterie de la prise, réglée hors de HomeSmart (application Smart Life ?)".to_string())
        } else if let (Some(d), Some("off")) = (&outage, c.after_cut) {
            (Level::Warning, format!("coupure de courant probable (hors ligne {d}) : au retour du courant, cette prise reste éteinte (réglage « Après une coupure de courant : Rester éteinte »)"))
        } else if let Some(d) = &outage {
            (Level::Warning, format!("éteinte pendant qu'elle était hors ligne ({d}) : coupure de courant, bouton de la prise ou application Smart Life"))
        } else {
            (Level::Warning, "aucune commande de HomeSmart : bouton de la prise, ou application Smart Life (ou une autre application)".to_string())
        };
        return Some((ProblemKind::UnexpectedOff, level, why));
    }
    let serious = if c.guarded { Level::Critical } else { Level::Warning };
    let (level, mut why) = if c.by_timer {
        (if c.guarded { Level::Warning } else { Level::Info }, "minuterie de la prise, réglée hors de HomeSmart (application Smart Life ?)".to_string())
    } else if let (Some(d), Some("on")) = (&outage, c.after_cut) {
        (serious, format!("rallumée seule au retour du courant (hors ligne {d}, réglage « Après une coupure de courant : Se rallumer »)"))
    } else if let Some(d) = &outage {
        (serious, format!("allumée pendant qu'elle était hors ligne ({d}) : bouton de la prise ou application Smart Life"))
    } else {
        (serious, "aucune commande de HomeSmart : bouton de la prise, ou application Smart Life (ou une autre application)".to_string())
    };
    if c.guarded {
        why.push_str(" ; ses garde-fous restent actifs");
    }
    Some((ProblemKind::UnexpectedOn, level, why))
}

/// La prise a-t-elle redémarré pendant la perte de liaison ? Sa minuterie, qui avait encore
/// `left_before` secondes à courir, est revenue à zéro.
pub fn restarted(left_before: Option<u64>, countdown_now: Option<u64>) -> bool {
    left_before.is_some_and(|l| l > 30) && countdown_now == Some(0)
}

pub fn restart_detail(outage: Duration, rearmed: bool) -> String {
    let again = if rearmed { " ; HomeSmart la reprogramme" } else { "" };
    format!("hors ligne {}, revenue avec sa minuterie à zéro : elle a redémarré (coupure de courant probable){again}", secs(outage))
}

// ---- Défauts (DP 26) ----------------------------------------------------------------------

/// Gravité et conseil selon les défauts signalés.
fn fault_advice(labels: &[String]) -> (Level, &'static str) {
    let has = |w: &str| labels.iter().any(|l| l == w);
    if has("surintensité") || has("surpuissance") {
        (Level::Critical, " : l'appareil branché tire trop ; débranche-le et vérifie-le avant de rallumer")
    } else if has("surtension") {
        (Level::Critical, " : la tension du secteur est trop haute, dangereuse pour les appareils")
    } else if has("sous-tension") {
        (Level::Warning, " : la tension du secteur a chuté (creux de tension) ; si des appareils se coupent en même temps, vérifie la multiprise ou la prise murale (faux contact) et ce qui tourne sur le même circuit")
    } else if labels.iter().all(|l| l == "sous-intensité" || l == "sous-puissance") {
        (Level::Info, " : l'appareil branché ne consomme presque plus")
    } else {
        (Level::Warning, "")
    }
}

/// Défauts signalés par la prise (DP 26) : gravité et texte.
pub fn fault_problem(labels: &[String], guard_cuts: bool) -> (Level, String) {
    let (level, advice) = fault_advice(labels);
    let cut = if guard_cuts { " — garde-fou : coupure demandée" } else { "" };
    let what: Vec<String> = labels.iter().map(|l| if l.starts_with("défaut") { format!("un {l}") } else { format!("une {l}") }).collect();
    (level, format!("la prise signale {}{advice}{cut}", what.join(", ")))
}

// ---- Hors ligne -----------------------------------------------------------------------------

/// Ce qu'on sait d'un appareil qui ne répond plus.
#[derive(Clone, Debug, Default)]
pub struct OfflineInfo {
    pub for_: Duration,
    pub error: String,
    pub guarded: bool,
    /// Allumé quand la liaison a été perdue ?
    pub was_on: Option<bool>,
    /// Minuterie de la prise qui court encore (s).
    pub countdown_left: Option<u64>,
    /// Tous les appareils sont perdus en même temps.
    pub all_lost: bool,
    /// C'est un aspirateur robot (pas une prise).
    pub robot: bool,
}

/// Le réseau de ce PC lui-même est coupé (et pas l'appareil) ?
pub fn network_down(error: &str) -> bool {
    ["10051", "os error 101)", "Network is unreachable", "réseau impossible à atteindre"].iter().any(|k| error.contains(k))
}

pub fn offline_problem(o: &OfflineInfo) -> (Level, String) {
    let cause = if network_down(&o.error) {
        "le réseau de ce PC semble coupé (Wi-Fi ou câble)"
    } else if o.all_lost {
        "tous les appareils sont perdus en même temps (box ou Wi-Fi ?)"
    } else if o.robot {
        "robot éteint, batterie vide, ou hors de portée du Wi-Fi"
    } else {
        "prise débranchée ou sans courant, ou trop loin du Wi-Fi"
    };
    let mut level = Level::Warning;
    let mut text = format!("ne répond plus depuis {} : {cause}", secs(o.for_));
    if o.guarded {
        match (o.was_on, o.countdown_left) {
            (Some(true), Some(c)) => text += &format!(" ; elle était allumée, sa minuterie de sécurité l'éteindra dans {}", fmt_secs(c)),
            (Some(true), None) => {
                level = Level::Critical;
                text += " ; elle était allumée et ses garde-fous ne peuvent plus agir";
            }
            _ => text += " ; ses garde-fous ne peuvent plus agir",
        }
    }
    if !o.error.is_empty() {
        text += &format!(" (détail : {})", o.error);
    }
    (level, text)
}

/// Erreurs signalées par un aspirateur robot : la plus grave donne la gravité ; chacune avec ce
/// qu'il faut faire.
pub fn vacuum_problem(errors: &[crate::vacuum::VacError]) -> (Level, String) {
    let level = errors.iter().map(|e| e.level).max_by_key(|l| rank(*l)).unwrap_or(Level::Warning);
    let parts: Vec<String> = errors
        .iter()
        .map(|e| {
            let code = if e.code.is_empty() { String::new() } else { format!(" (code {})", e.code) };
            format!("« {} » : {}{code}", e.label, e.advice)
        })
        .collect();
    (level, format!("le robot signale {}", if parts.is_empty() { "une erreur".into() } else { parts.join(" ; ") }))
}

// ---- Tension ------------------------------------------------------------------------------------

/// Surveillance de la tension du secteur mesurée par une prise (DP 20). La tension nominale
/// (230 V ou 120 V) est reconnue à la première mesure plausible.
#[derive(Default, Debug)]
pub struct VoltageWatch {
    nominal: Option<f64>,
    bad: u32,
    good: u32,
    raised: bool,
}

impl VoltageWatch {
    /// Nouvelle mesure (V). Un écart fort (± 15 %) est signalé tout de suite, un écart de ± 10 %
    /// après trois mesures de suite ; le retour à la normale (± 8 %) après trois mesures.
    pub fn update(&mut self, v: f64) -> Option<Finding> {
        if !(50.0..=400.0).contains(&v) {
            return None;
        }
        if self.nominal.is_none() {
            self.nominal = if (200.0..=255.0).contains(&v) {
                Some(230.0)
            } else if (105.0..=135.0).contains(&v) {
                Some(120.0)
            } else {
                None
            };
        }
        let n = self.nominal?;
        let (lo, hi) = (n * 0.9, n * 1.1);
        if v < lo || v > hi {
            (self.good, self.bad) = (0, self.bad + 1);
            let severe = v < n * 0.85 || v > n * 1.15;
            if !self.raised && (severe || self.bad >= 3) {
                self.raised = true;
                let range = format!("normale : {} à {} V", fr(lo, 0), fr(hi, 0));
                return Some(if v < lo {
                    Finding::Open(
                        Level::Warning,
                        format!("{} V mesurés ({range}) : creux de tension du secteur ; si des appareils se coupent en même temps, vérifie la multiprise ou la prise murale (faux contact)", fr(v, 0)),
                    )
                } else {
                    let level = if v > n * 1.15 { Level::Critical } else { Level::Warning };
                    Finding::Open(level, format!("{} V mesurés ({range}) : surtension du secteur, dangereuse pour les appareils branchés", fr(v, 0)))
                });
            }
        } else if (n * 0.92..=n * 1.08).contains(&v) {
            (self.bad, self.good) = (0, self.good + 1);
            if self.raised && self.good >= 3 {
                self.raised = false;
                return Some(Finding::Close(format!("tension revenue à la normale ({} V)", fr(v, 0))));
            }
        }
        None
    }
}

// ---- Consommation ---------------------------------------------------------------------------------

/// Appareil allumé qui ne consomme pas comme d'habitude. Pour ne pas crier au loup, seul compte
/// le cas où, depuis l'allumage, il n'a jamais atteint sa puissance habituelle (un radiateur dont
/// le thermostat coupe après avoir chauffé, c'est normal) ; et c'est une simple information.
#[derive(Default, Debug)]
pub struct PowerWatch {
    on_since: Option<Instant>,
    reached: bool,
    low_since: Option<Instant>,
    raised: bool,
}

impl PowerWatch {
    /// `typical` : puissance habituelle en marche (`EnergyStore::typical_active_w`) ; `heater` :
    /// appareil de chauffage (thermostat).
    pub fn update(&mut self, now: Instant, on: bool, power: f64, typical: Option<f64>, heater: bool) -> Option<Finding> {
        self.update_after(now, on, power, typical, heater, LOW_POWER_AFTER)
    }

    /// Comme `update`, avec le délai avant de le signaler (démo : quelques secondes).
    pub fn update_after(&mut self, now: Instant, on: bool, power: f64, typical: Option<f64>, heater: bool, after: Duration) -> Option<Finding> {
        if !on {
            let was = self.raised;
            *self = PowerWatch::default();
            return was.then(|| Finding::Close("appareil éteint".into()));
        }
        let on_since = *self.on_since.get_or_insert(now);
        let typ = typical?;
        if power >= typ * 0.5 {
            (self.reached, self.low_since) = (true, None);
            if self.raised {
                self.raised = false;
                return Some(Finding::Close(format!("consommation revenue à la normale ({})", watts(power))));
            }
            return None;
        }
        if self.reached || power >= typ * 0.2 {
            self.low_since = None;
            return None;
        }
        let low_since = *self.low_since.get_or_insert(now);
        if self.raised || now.duration_since(low_since) < after {
            return None;
        }
        self.raised = true;
        let why = if heater {
            "son thermostat a peut-être déjà coupé (pièce chaude : c'est normal), ou il est arrêté par son propre interrupteur"
        } else {
            "l'appareil branché est peut-être arrêté, en veille ou débranché"
        };
        Some(Finding::Open(
            Level::Info,
            format!("allumé depuis {}, mais ne consomme que {} (d'habitude {} en marche) : {why}", secs(now.duration_since(on_since)), watts(power), watts(typ)),
        ))
    }
}

// ---- Caméras --------------------------------------------------------------------------------------

/// Depuis combien de temps chaque condition (caméra sans image, analyse en erreur…) dure.
#[derive(Default)]
pub struct Holds(HashMap<(ProblemKind, String), Instant>);

impl Holds {
    /// `Some(durée)` tant que `cond` est vraie (depuis la première fois), `None` sinon.
    pub fn hold(&mut self, kind: ProblemKind, id: &str, cond: bool, now: Instant) -> Option<Duration> {
        let k = (kind, id.to_string());
        if !cond {
            self.0.remove(&k);
            return None;
        }
        Some(now.duration_since(*self.0.entry(k).or_insert(now)))
    }

    pub fn forget(&mut self, id: &str) {
        self.0.retain(|(_, d), _| d != id);
    }
}

pub fn camera_offline(host: &str, port: u16, for_: Duration) -> (Level, String) {
    (Level::Warning, format!("ne répond plus depuis {} ({host}:{port}) : caméra débranchée, hors du Wi-Fi, ou adresse IP changée", secs(for_)))
}

pub fn stream_lost(error: &str, for_: Duration, waiting_retry: bool) -> (Level, String) {
    let wait = if waiting_retry { " — HomeSmart attend « Réessayer » dans les réglages de la caméra" } else { "" };
    (Level::Warning, format!("pas d'image depuis {} : {error}{wait}", secs(for_)))
}

/// L'analyse d'une caméra est en erreur ; `watching` : ce qu'elle surveillait (chutes…).
pub fn vision_stopped(error: &str, watching: &[&str]) -> (Level, String) {
    if watching.is_empty() {
        return (Level::Warning, format!("{error} : plus rien n'est analysé sur cette caméra"));
    }
    // Une alerte de sécurité (chute, appel à l'aide, mode absence) ne fonctionne plus : grave,
    // sauf s'il manque seulement les modèles (réglage à terminer, pas une panne).
    let level = if error.contains("non installés") { Level::Warning } else { Level::Critical };
    (level, format!("{error} : {} ne sont plus surveillés", watching.join(", ")))
}

pub fn listen_stopped(error: &str) -> (Level, String) {
    (Level::Warning, format!("{error} : les sons (alarme incendie, verre brisé, appel à l'aide) ne sont plus écoutés"))
}

// ---- Arrêt brutal de ce PC --------------------------------------------------------------------------

/// Mesures récentes d'une prise : la « boîte noire » de la dernière minute, gardée dans la marque
/// de session pour expliquer un arrêt brutal au démarrage suivant.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Trace {
    pub device: String,
    pub name: String,
    /// (heure, puissance W, tension V), la plus ancienne d'abord.
    pub samples: Vec<(DateTime<Local>, Option<f64>, Option<f64>)>,
}

/// Durée gardée par la boîte noire.
pub const TRACE_SPAN_S: i64 = 60;

impl Trace {
    pub fn push(&mut self, at: DateTime<Local>, watts: Option<f64>, volts: Option<f64>) {
        if watts.is_none() && volts.is_none() {
            return;
        }
        self.samples.push((at, watts.map(|w| (w * 10.0).round() / 10.0), volts.map(|v| (v * 10.0).round() / 10.0)));
        let oldest = at - chrono::Duration::seconds(TRACE_SPAN_S);
        self.samples.retain(|s| s.0 >= oldest);
    }

    /// « 276 W et 240 V à 20:42:28 (sur la dernière minute : 270 à 290 W, 238 V au plus bas) ».
    pub fn summary(&self) -> Option<String> {
        let (at, _, _) = self.samples.last()?;
        let w = self.samples.iter().rev().find_map(|s| s.1);
        let v = self.samples.iter().rev().find_map(|s| s.2);
        let mut now = Vec::new();
        now.extend(w.map(watts));
        now.extend(v.map(|v| format!("{} V", fr(v, 0))));
        let mut text = format!("{} à {}", now.join(" et "), at.format("%H:%M:%S"));
        let ws: Vec<f64> = self.samples.iter().filter_map(|s| s.1).collect();
        let vs: Vec<f64> = self.samples.iter().filter_map(|s| s.2).collect();
        let mut minute = Vec::new();
        if ws.len() >= 2 {
            let (lo, hi) = (ws.iter().copied().fold(f64::MAX, f64::min), ws.iter().copied().fold(0.0, f64::max));
            minute.push(if hi - lo < 1.0 { watts(hi) } else { format!("{} à {}", fr(lo, 0), watts(hi)) });
        }
        let low = vs.iter().copied().fold(f64::MAX, f64::min);
        if vs.len() >= 2 {
            minute.push(format!("{} V au plus bas", fr(low, 0)));
        }
        if !minute.is_empty() {
            text += &format!(" (sur la dernière minute : {})", minute.join(", "));
        }
        if low < 207.0 && low > 50.0 {
            text += " — la tension avait baissé";
        }
        Some(text)
    }
}

/// Marque de session : écrite au démarrage, rafraîchie toutes les 5 s (avec la boîte noire des
/// prises), `clean` à la fermeture. Au démarrage suivant, elle dit quand HomeSmart vivait encore.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionMark {
    pub pid: u32,
    pub started: DateTime<Local>,
    pub alive: DateTime<Local>,
    #[serde(default)]
    pub clean: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub traces: Vec<Trace>,
}

impl SessionMark {
    pub fn new(now: DateTime<Local>) -> Self {
        SessionMark { pid: std::process::id(), started: now, alive: now, clean: false, traces: Vec::new() }
    }

    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(text.trim_matches(|c: char| c == '\0' || c.is_whitespace())).ok()
    }

    /// Écriture forcée sur le disque, par un fichier temporaire renommé : une coupure pendant
    /// l'écriture laisse l'ancienne marque intacte.
    pub fn save(&self, path: &Path) {
        use std::io::Write;
        let tmp = path.with_extension("json.tmp");
        let res = serde_json::to_vec(self).map_err(std::io::Error::other).and_then(|data| {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&data)?;
            f.sync_data()?;
            drop(f);
            std::fs::rename(&tmp, path)
        });
        if let Err(e) = res {
            tracing::warn!("marque de session impossible à écrire ({}) : {e}", path.display());
        }
    }
}

/// Redémarrage de Windows après un arrêt non prévu (événement Kernel-Power 41).
#[derive(Clone, Debug, PartialEq)]
pub struct UnexpectedBoot {
    /// Heure du redémarrage.
    pub at: DateTime<Local>,
    /// Code d'écran bleu (0 : aucun).
    pub bugcheck: u64,
    /// Appui long sur le bouton d'alimentation.
    pub power_button: bool,
    /// Heure exacte de l'arrêt (événement 6008), si Windows l'a notée.
    pub cut_at: Option<DateTime<Local>>,
}

impl UnexpectedBoot {
    /// Perte d'alimentation nette : ni écran bleu, ni bouton.
    pub fn power_lost(&self) -> bool {
        self.bugcheck == 0 && !self.power_button
    }
}

/// Ce que le journal de Windows dit des démarrages (7 derniers jours ; vide ailleurs).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SystemLog {
    /// Tous les démarrages (événement 12), dans l'ordre.
    pub boots: Vec<DateTime<Local>>,
    /// Ceux qui suivaient un arrêt non prévu.
    pub unexpected: Vec<UnexpectedBoot>,
}

/// SYSTEMTIME (8 entiers de 16 bits, petit-boutiste) en heure locale.
fn systemtime(hex: &str, utc: bool) -> Option<DateTime<Local>> {
    let b: Vec<u16> = (0..8).map(|i| hex.get(i * 4..i * 4 + 4).and_then(|h| u16::from_str_radix(h, 16).ok()).map(u16::swap_bytes)).collect::<Option<_>>()?;
    let t = chrono::NaiveDate::from_ymd_opt(b[0] as i32, b[1] as u32, b[3] as u32)?.and_hms_milli_opt(b[4] as u32, b[5] as u32, b[6] as u32, b[7] as u32)?;
    if utc {
        Some(chrono::Utc.from_utc_datetime(&t).with_timezone(&Local))
    } else {
        Local.from_local_datetime(&t).earliest()
    }
}

/// Lit la sortie XML de `wevtutil qe System` (événements 12, 41 et 6008).
pub fn parse_system_log(xml: &str) -> SystemLog {
    let field = |ev: &str, name: &str| -> Option<String> {
        let tag = format!("Name='{name}'>");
        let start = ev.find(&tag)? + tag.len();
        Some(ev[start..].split('<').next()?.trim().to_string())
    };
    let num = |s: Option<String>| -> u64 {
        let s = s.unwrap_or_default();
        match s.strip_prefix("0x") {
            Some(h) => u64::from_str_radix(h, 16).unwrap_or(0),
            None => s.parse().unwrap_or(0),
        }
    };
    let created = |ev: &str| -> Option<DateTime<Local>> {
        let t = ev.split("SystemTime=").nth(1)?;
        let t = t.trim_start_matches(['\'', '"']).split(['\'', '"']).next()?;
        Some(DateTime::parse_from_rfc3339(t).ok()?.with_timezone(&Local))
    };
    let mut log = SystemLog::default();
    let mut cuts: Vec<(DateTime<Local>, DateTime<Local>)> = Vec::new();
    for ev in xml.split("<Event ").skip(1) {
        let Some(at) = created(ev) else { continue };
        let id = ev.split("<EventID").nth(1).and_then(|s| s.split('>').nth(1)).and_then(|s| s.split('<').next()).unwrap_or("");
        match id {
            "12" => {
                let start = field(ev, "StartTime").and_then(|s| DateTime::parse_from_rfc3339(&s).ok()).map(|t| t.with_timezone(&Local));
                log.boots.push(start.unwrap_or(at));
            }
            "41" => {
                let button = num(field(ev, "PowerButtonTimestamp")) != 0 || field(ev, "LongPowerButtonPressDetected").as_deref() == Some("true");
                log.unexpected.push(UnexpectedBoot { at, bugcheck: num(field(ev, "BugcheckCode")), power_button: button, cut_at: None });
            }
            "6008" => {
                // Deux SYSTEMTIME : heure locale puis heure universelle de l'arrêt non prévu.
                let bin = ev.split("<Binary>").nth(1).and_then(|s| s.split('<').next()).unwrap_or("");
                if let Some(t) = systemtime(bin.get(32..64).unwrap_or(""), true).or_else(|| systemtime(bin, false)) {
                    cuts.push((at, t));
                }
            }
            _ => {}
        }
    }
    log.boots.sort();
    for u in log.unexpected.iter_mut() {
        // L'événement 41 est noté au démarrage : l'heure du démarrage est celle de l'événement 12 juste avant.
        if let Some(b) = log.boots.iter().rev().find(|b| **b <= u.at && u.at - **b < chrono::Duration::minutes(5)) {
            u.at = *b;
        }
        // L'événement 6008 est noté quelques secondes après ce démarrage-là (pas après le précédent).
        u.cut_at = cuts.iter().filter(|(at, t)| *at >= u.at && *at - u.at < chrono::Duration::minutes(5) && *t < u.at).min_by_key(|(at, _)| *at - u.at).map(|(_, t)| *t);
    }
    log.unexpected.sort_by_key(|u| u.at);
    log
}

/// Démarrages et arrêts non prévus de Windows, ces 7 derniers jours (ailleurs : rien de connu).
pub fn system_log() -> SystemLog {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let query = "/q:*[System[((Provider[@Name='Microsoft-Windows-Kernel-General'] and EventID=12) or EventID=41 or EventID=6008) and TimeCreated[timediff(@SystemTime) <= 604800000]]]";
        let out = std::process::Command::new("wevtutil").args(["qe", "System", query, "/f:xml"]).creation_flags(CREATE_NO_WINDOW).output();
        match out {
            Ok(o) if o.status.success() => parse_system_log(&String::from_utf8_lossy(&o.stdout)),
            Ok(o) => {
                tracing::debug!("wevtutil : {}", String::from_utf8_lossy(&o.stderr));
                SystemLog::default()
            }
            Err(e) => {
                tracing::debug!("wevtutil impossible : {e}");
                SystemLog::default()
            }
        }
    }
    #[cfg(not(windows))]
    {
        SystemLog::default()
    }
}

/// Démarrage du système d'exploitation.
pub fn system_boot() -> Option<DateTime<Local>> {
    #[cfg(windows)]
    let up = {
        // SAFETY : fonction sans argument ni effet de bord.
        let ms = unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() };
        Some(Duration::from_millis(ms))
    };
    #[cfg(not(windows))]
    let up = std::fs::read_to_string("/proc/uptime").ok().and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok()).map(Duration::from_secs_f64);
    Some(Local::now() - chrono::Duration::from_std(up?).ok()?)
}

/// Indice trouvé au journal juste avant l'arrêt (creux de tension, défaut signalé…).
#[derive(Clone, Debug, PartialEq)]
pub struct Clue {
    pub at: DateTime<Local>,
    pub device_name: String,
    pub what: String,
}

/// Une entrée du journal est-elle un indice d'un problème d'alimentation ?
pub fn clue_of(e: &Entry) -> Option<Clue> {
    let tagged = matches!(parse_key(&e.key), Some((ProblemKind::Voltage | ProblemKind::Fault, _, false)));
    let m = e.message.to_lowercase();
    let what = if m.contains("sous-tension") {
        "une sous-tension"
    } else if m.contains("surtension") {
        "une surtension"
    } else if m.contains("creux de tension") || (tagged && m.contains("v mesurés")) {
        "une tension trop basse"
    } else if m.contains("surintensité") {
        "une surintensité"
    } else if tagged {
        "un défaut"
    } else {
        return None;
    };
    (e.source == Source::Device && !e.key.ends_with(":fin")).then(|| Clue { at: e.at, device_name: e.device_name.clone(), what: what.into() })
}

/// « 20:42 », ou « le 24/10 à 20:42 » si ce n'est pas aujourd'hui (`secs` : avec les secondes).
pub fn when(at: DateTime<Local>, now: DateTime<Local>) -> String {
    when_fmt(at, now, false)
}

fn when_fmt(at: DateTime<Local>, now: DateTime<Local>, secs: bool) -> String {
    let hm = if secs { "%H:%M:%S" } else { "%H:%M" };
    if at.date_naive() == now.date_naive() {
        at.format(hm).to_string()
    } else {
        at.format(&format!("le %d/%m à {hm}")).to_string()
    }
}

/// « 20:25:53 et 20:31:01 », « 20:25:53, 20:31:01 et 20:42:31 ».
fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} et {last}", rest.join(", ")),
    }
}

/// Ce que l'on sait au démarrage de la façon dont la session précédente s'est terminée.
pub struct PreviousSession<'a> {
    pub mark: &'a SessionMark,
    /// Journal de Windows (démarrages, arrêts non prévus) ; vide sous Linux.
    pub log: &'a SystemLog,
    /// Démarrage du système actuel.
    pub boot: Option<DateTime<Local>>,
    /// Indices au journal dans les minutes avant la dernière trace de vie.
    pub clues: &'a [Clue],
    /// Prise marquée « alimente ce PC », et son comportement au retour du courant (DP 38).
    pub powers_pc: Option<(&'a str, Option<&'static str>)>,
    /// Autres prises qui restent éteintes après une coupure de courant.
    pub stays_off: &'a [String],
    pub now: DateTime<Local>,
}

/// La session précédente s'est-elle mal terminée, et ce PC a-t-il perdu son alimentation depuis
/// (même HomeSmart fermé) ? (gravité, texte)
pub fn host_shutdown(s: &PreviousSession) -> Option<(Level, String)> {
    let alive = s.mark.alive;
    let after: Vec<&UnexpectedBoot> = s.log.unexpected.iter().filter(|u| u.at > alive && u.at - alive < chrono::Duration::days(7)).collect();
    // HomeSmart est mort avec le PC si le premier démarrage qui suit sa dernière trace est un
    // redémarrage après un arrêt non prévu (sinon : arrêt normal de Windows, HomeSmart fermé avec lui).
    let first_boot = s.log.boots.iter().find(|b| **b > alive);
    let died = (!s.mark.clean)
        .then(|| match first_boot {
            Some(b) => after.iter().find(|u| (u.at - *b).num_minutes().abs() < 5).copied(),
            None => after.first().copied(),
        })
        .flatten();
    // Coupures pendant que HomeSmart était fermé.
    let closed: Vec<&UnexpectedBoot> = after.iter().filter(|u| Some(**u) != died && u.power_lost()).copied().collect();
    let cut_time = |u: &UnexpectedBoot| u.cut_at.map(|t| when_fmt(t, s.now, true)).unwrap_or_else(|| format!("avant {}", when(u.at, s.now)));
    let closed_text = |also: bool| {
        let times: Vec<String> = closed.iter().map(|u| cut_time(u)).collect();
        format!("pendant que HomeSmart était fermé, ce PC a {}perdu son alimentation à {} (ni écran bleu, ni bouton)", if also { "aussi " } else { "" }, and_list(&times))
    };
    let n24 = s.log.unexpected.iter().filter(|u| u.power_lost() && s.now - u.at <= chrono::Duration::hours(24)).count();
    let count = if n24 >= 2 { format!(" ({n24} coupures en 24 h)") } else { String::new() };
    let last = when_fmt(alive, s.now, true);
    let clue = s.clues.iter().filter(|c| c.at <= alive + chrono::Duration::seconds(15)).max_by_key(|c| c.at).map(|c| {
        let before = (alive - c.at).num_seconds().max(0);
        format!(" ; juste avant, « {} » a signalé {} à {} ({before} s avant la dernière trace de HomeSmart) : creux de tension du secteur probable", c.device_name, c.what, c.at.format("%H:%M:%S"))
    });
    // Boîte noire : dernières mesures de la prise qui alimente ce PC (sinon de toutes les prises).
    let traces: Vec<&Trace> = match s.powers_pc {
        Some((plug, _)) => s.mark.traces.iter().filter(|t| t.name == plug).collect(),
        None => s.mark.traces.iter().filter(|t| !t.samples.is_empty()).take(2).collect(),
    };
    let measures: Vec<String> = traces.iter().filter_map(|t| t.summary().map(|m| format!("« {} » {m}", t.name))).collect();
    // Si la prise ne s'est pas éteinte (son voyant suit le relais), c'est le PC lui-même.
    const PC_ALONE: &str = " ; si son voyant est resté allumé, la prise n'a pas coupé : c'est ce PC lui-même qui s'est arrêté (son alimentation, ou son câble)";
    let plug = match s.powers_pc {
        Some((plug, Some("off"))) => format!(
            " ; la prise « {plug} » qui l'alimente revient éteinte après une coupure : s'il a fallu appuyer sur son bouton, impossible de savoir si elle a redémarré (micro-coupure du secteur, plantage) ou reçu un ordre d'extinction (application Smart Life : minuterie, automatisme) ; réglée sur « Reprendre l'état précédent », elle se rallumerait seule après un redémarrage, ce qui les distinguerait{PC_ALONE}"
        ),
        Some((plug, Some(_))) => format!(
            " ; si la prise « {plug} » qui l'alimente s'est éteinte puis rallumée seule, elle a redémarré (micro-coupure du secteur ou plantage) ; s'il a fallu appuyer sur son bouton, elle a reçu un ordre d'extinction (application Smart Life : minuterie, automatisme, ou un autre appareil du réseau){PC_ALONE}"
        ),
        Some((plug, None)) => format!(" ; vérifie la prise « {plug} » qui l'alimente, et ce qui est branché sur le même circuit{PC_ALONE}"),
        None if !s.stays_off.is_empty() => format!(
            " ; « {} » reste éteinte après une coupure de courant : si ce PC y est branché, c'est pourquoi il faut la rallumer à la main (« Reprendre l'état précédent » la rallumerait seule)",
            s.stays_off.join(" », « ")
        ),
        None => String::new(),
    };
    match died {
        Some(b) if b.bugcheck != 0 => {
            let mut t = format!("ce PC a planté vers {last} (écran bleu, code 0x{:X}) ; Windows a redémarré à {}", b.bugcheck, when(b.at, s.now));
            if !closed.is_empty() {
                t += &format!(" ; {}", closed_text(true));
            }
            Some((Level::Warning, t))
        }
        Some(b) if b.power_button => Some((Level::Info, format!("ce PC a été éteint de force vers {last} (appui long sur son bouton) ; Windows a redémarré à {}", when(b.at, s.now)))),
        Some(b) => {
            let at = b.cut_at.map(|t| format!("à {}", when_fmt(t, s.now, true))).unwrap_or_else(|| format!("vers {last}"));
            let mut text = format!(
                "ce PC a perdu son alimentation {at} pendant que HomeSmart tournait (dernière trace à {last}) : Windows a redémarré à {} sans arrêt normal, ni écran bleu, ni appui sur le bouton{count}",
                when(b.at, s.now)
            );
            text += &clue.unwrap_or_default();
            if !measures.is_empty() {
                text += &format!(" ; dernières mesures : {}", measures.join(" ; "));
            }
            if !closed.is_empty() {
                text += &format!(" ; {}", closed_text(true));
            }
            text += &plug;
            Some((Level::Warning, text))
        }
        None if !closed.is_empty() => Some((Level::Warning, format!("{}{count}{plug}", closed_text(false)))),
        // Pas d'arrêt non prévu connu : arrêt normal du système, ou HomeSmart lui-même arrêté net.
        None if s.mark.clean => None,
        None => match s.boot {
            Some(boot) if boot < alive => Some((Level::Info, format!("HomeSmart s'est arrêté net vers {} (plantage ou fin forcée) ; ce PC, lui, est resté allumé", when(alive, s.now)))),
            // Sans journal du système (Linux) : seulement si un creux de tension a été vu juste avant.
            Some(_) if s.log.boots.is_empty() && s.log.unexpected.is_empty() && clue.is_some() => {
                Some((Level::Warning, format!("ce PC s'est arrêté vers {} sans que HomeSmart soit fermé{}", when(alive, s.now), clue.unwrap_or_default())))
            }
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32, s: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 25, h, m, s).unwrap()
    }

    /// Heure universelle du 25/10/2026 (le journal de Windows est en UTC ; la France est alors à UTC+1).
    fn utc(h: u32, m: u32, s: u32) -> DateTime<Local> {
        chrono::Utc.with_ymd_and_hms(2026, 10, 25, h, m, s).unwrap().with_timezone(&Local)
    }

    #[test]
    fn switch_changes_and_their_causes() {
        let off = |f: fn(&mut SwitchChange)| {
            let mut c = SwitchChange::default();
            f(&mut c);
            classify_switch(&c)
        };
        // Minuterie réglée par HomeSmart (garde-fou) : demandé, pas un problème, même si elle a
        // échu pendant une perte de liaison (PC en veille).
        assert_eq!(off(|c| (c.by_timer, c.timer_ours) = (true, true)), None);
        assert_eq!(off(|c| (c.by_timer, c.timer_ours, c.after_outage) = (true, true, Some(Duration::from_secs(3600)))), None);
        // Minuterie réglée ailleurs : simple information.
        let (k, l, why) = off(|c| c.by_timer = true).unwrap();
        assert_eq!((k, l), (ProblemKind::UnexpectedOff, Level::Info));
        assert!(why.contains("hors de HomeSmart"), "{why}");
        // Bouton ou autre application.
        let (_, l, why) = off(|_| {}).unwrap();
        assert_eq!(l, Level::Warning);
        assert!(why.contains("bouton de la prise"), "{why}");
        // Retour après coupure, prise réglée pour rester éteinte (cas de la Multiprise - Bureau).
        let (_, l, why) = off(|c| (c.after_outage, c.after_cut) = (Some(Duration::from_secs(95)), Some("off"))).unwrap();
        assert_eq!(l, Level::Warning);
        assert!(why.contains("coupure de courant probable (hors ligne 1 min 35 s)") && why.contains("Rester éteinte"), "{why}");
        // Protection : surintensité grave, sous-tension avertissement.
        let (_, l, why) = off(|c| c.faults = vec!["surintensité".into()]).unwrap();
        assert_eq!(l, Level::Critical);
        assert!(why.starts_with("coupée par sa protection (surintensité)"), "{why}");
        let (_, l, _) = off(|c| c.faults = vec!["sous-tension".into()]).unwrap();
        assert_eq!(l, Level::Warning);
        // Allumage : grave pour un appareil sous garde-fous (chauffage).
        let (k, l, why) = classify_switch(&SwitchChange { on: true, guarded: true, ..Default::default() }).unwrap();
        assert_eq!((k, l), (ProblemKind::UnexpectedOn, Level::Critical));
        assert!(why.ends_with("ses garde-fous restent actifs"), "{why}");
        let (_, l, why) = classify_switch(&SwitchChange { on: true, after_outage: Some(Duration::from_secs(20)), after_cut: Some("on"), ..Default::default() }).unwrap();
        assert_eq!(l, Level::Warning);
        assert!(why.contains("rallumée seule au retour du courant"), "{why}");
    }

    #[test]
    fn restart_is_seen_by_the_lost_countdown() {
        assert!(restarted(Some(1500), Some(0)));
        assert!(!restarted(Some(10), Some(0)), "minuterie qui arrivait à échéance");
        assert!(!restarted(None, Some(0)));
        assert!(!restarted(Some(1500), Some(1440)));
        assert!(restart_detail(Duration::from_secs(40), true).contains("HomeSmart la reprogramme"));
    }

    #[test]
    fn faults_offline_and_messages() {
        let (l, t) = fault_problem(&["sous-tension".into()], false);
        assert_eq!(l, Level::Warning);
        assert!(t.starts_with("la prise signale une sous-tension : la tension du secteur a chuté"), "{t}");
        assert_eq!(fault_problem(&["surintensité".into()], true).0, Level::Critical);
        assert!(fault_problem(&["surintensité".into()], true).1.ends_with("coupure demandée"));
        assert_eq!(fault_problem(&["sous-puissance".into()], false).0, Level::Info);

        // Réseau du PC coupé (vrai message du 24/10, os error 10051).
        let err = "connexion à 192.168.1.112:6668: Une opération a été tentée sur un réseau impossible à atteindre. (os error 10051)";
        let (l, t) = offline_problem(&OfflineInfo { for_: Duration::from_secs(60), error: err.into(), ..Default::default() });
        assert_eq!(l, Level::Warning);
        assert!(t.starts_with("ne répond plus depuis 1 min : le réseau de ce PC semble coupé"), "{t}");
        // Chauffage allumé, plus de liaison, pas de minuterie : grave.
        let o = OfflineInfo { for_: Duration::from_secs(90), guarded: true, was_on: Some(true), ..Default::default() };
        assert_eq!(offline_problem(&o).0, Level::Critical);
        let o = OfflineInfo { countdown_left: Some(1200), ..o };
        let (l, t) = offline_problem(&o);
        assert_eq!(l, Level::Warning);
        assert!(t.contains("l'éteindra dans 20 min"), "{t}");
        assert!(offline_problem(&OfflineInfo { all_lost: true, ..Default::default() }).1.contains("box ou Wi-Fi"));
    }

    #[test]
    fn voltage_dips_and_recovery() {
        let mut w = VoltageWatch::default();
        for v in [240.7, 239.0, 241.0] {
            assert_eq!(w.update(v), None);
        }
        // Léger creux (205 V) : trois mesures de suite.
        assert_eq!(w.update(205.0), None);
        assert_eq!(w.update(205.0), None);
        match w.update(204.0) {
            Some(Finding::Open(Level::Warning, t)) => assert!(t.starts_with("204 V mesurés (normale : 207 à 253 V) : creux de tension"), "{t}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(w.update(190.0), None, "déjà signalé");
        assert_eq!(w.update(230.0), None);
        assert_eq!(w.update(231.0), None);
        assert_eq!(w.update(232.0), Some(Finding::Close("tension revenue à la normale (232 V)".into())));
        // Creux fort : tout de suite.
        assert!(matches!(w.update(180.0), Some(Finding::Open(Level::Warning, _))));
        // Réseau 120 V (Amérique du Nord) : pas d'alerte.
        let mut us = VoltageWatch::default();
        for _ in 0..5 {
            assert_eq!(us.update(121.0), None);
        }
        // Surtension forte : critique.
        let mut hi = VoltageWatch::default();
        hi.update(230.0);
        assert!(matches!(hi.update(270.0), Some(Finding::Open(Level::Critical, _))));
        // Mesures absurdes ignorées.
        assert_eq!(VoltageWatch::default().update(0.0), None);
    }

    #[test]
    fn low_power_only_when_it_never_heated() {
        let t0 = Instant::now();
        let min = |m: u64| t0 + Duration::from_secs(m * 60);
        // Chauffage allumé qui ne tire que 25 W (cas vécu), d'habitude 1 200 W.
        let mut w = PowerWatch::default();
        assert_eq!(w.update(t0, true, 25.0, Some(1200.0), true), None);
        assert_eq!(w.update(min(14), true, 25.0, Some(1200.0), true), None);
        match w.update(min(15), true, 25.0, Some(1200.0), true) {
            Some(Finding::Open(Level::Info, t)) => {
                assert!(t.starts_with("allumé depuis 15 min, mais ne consomme que 25 W (d'habitude 1,2 kW en marche) : son thermostat"), "{t}")
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(w.update(min(30), true, 25.0, Some(1200.0), true), None, "une seule fois");
        assert_eq!(w.update(min(31), true, 1210.0, Some(1200.0), true), Some(Finding::Close("consommation revenue à la normale (1,2 kW)".into())));
        // Il a chauffé, puis le thermostat coupe : normal, rien.
        assert_eq!(w.update(min(40), true, 0.0, Some(1200.0), true), None);
        assert_eq!(w.update(min(90), true, 0.0, Some(1200.0), true), None);
        // Éteint : tout repart de zéro.
        assert_eq!(w.update(min(91), false, 0.0, Some(1200.0), true), None);
        // Pas d'historique : on ne sait pas, on ne dit rien.
        let mut new = PowerWatch::default();
        assert_eq!(new.update(t0, true, 2.0, None, false), None);
        assert_eq!(new.update(min(60), true, 2.0, None, false), None);
    }

    #[test]
    fn board_open_update_close() {
        let mut b = Board::default();
        let now = at(20, 0, 0);
        assert!(b.open(ProblemKind::Offline, "p", "Prise", Level::Warning, "a".into(), now).is_some());
        // Même état qui dure : pas réinscrit ; s'il s'aggrave : réinscrit.
        assert!(b.open(ProblemKind::Offline, "p", "Prise", Level::Warning, "b".into(), now).is_none());
        assert!(b.open(ProblemKind::Offline, "p", "Prise", Level::Critical, "c".into(), now).is_some());
        // Un événement qui se répète est réinscrit et compté.
        b.open(ProblemKind::UnexpectedOff, "p", "Prise", Level::Warning, "x".into(), now);
        let again = b.open(ProblemKind::UnexpectedOff, "p", "Prise", Level::Warning, "y".into(), now).unwrap();
        assert_eq!(again.count, 2);
        assert_eq!(b.active()[0].kind, ProblemKind::Offline, "le plus grave d'abord");
        assert!(b.close(ProblemKind::Offline, "p").is_some());
        assert!(b.close(ProblemKind::Offline, "p").is_none());
        // Un événement non vu expire au bout de 24 h.
        assert!(b.expire(now + chrono::Duration::hours(25)).len() == 1 && b.active().is_empty());
        // Clés du journal.
        assert_eq!(key(ProblemKind::Voltage, "bf1", false), "probleme:tension:bf1");
        assert_eq!(parse_key("probleme:tension:bf1:fin"), Some((ProblemKind::Voltage, "bf1", true)));
        assert_eq!(parse_key("probleme:arret-pc:"), Some((ProblemKind::HostShutdown, "", false)));
        assert_eq!(parse_key("vision:x"), None);
    }

    /// Le 25/10/2026, journal de Windows réel (heures UTC ; la France est alors à UTC+1) : coupures
    /// à 19:25:53, 19:31:01 et 19:42:31, redémarrages à 19:27:51, 19:32:22 et 19:50:25, une autre
    /// coupure le 20/10. HomeSmart tournait depuis 19:33 ; « Chauffage » signale une sous-tension
    /// à 19:42:09.
    #[test]
    fn host_power_loss_like_the_office_plug() {
        let xml = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/problems/system_log.xml")).unwrap();
        let log = parse_system_log(&xml);
        assert_eq!(log.boots.len(), 11, "{:?}", log.boots);
        assert_eq!(log.unexpected.len(), 4, "{:?}", log.unexpected);
        assert!(log.unexpected.iter().all(UnexpectedBoot::power_lost));
        let utc_fmt = |t: DateTime<Local>, f: &str| t.with_timezone(&chrono::Utc).format(f).to_string();
        let cuts: Vec<String> = log.unexpected.iter().filter_map(|u| u.cut_at).map(|t| utc_fmt(t, "%d/%m %H:%M:%S")).collect();
        assert_eq!(cuts, ["20/10 01:27:04", "25/10 19:25:53", "25/10 19:31:01", "25/10 19:42:31"]);
        assert_eq!(utc_fmt(log.unexpected[3].at, "%H:%M:%S"), "19:50:25", "heure du démarrage (événement 12)");

        let mut e = Entry::new(Level::Warning, Source::Device, "bf77", "Chauffage", "défaut signalé par l'appareil : sous-tension");
        e.at = utc(19, 42, 9);
        let clues: Vec<Clue> = clue_of(&e).into_iter().collect();
        assert_eq!(clues.len(), 1);
        assert!(clue_of(&Entry::new(Level::Info, Source::Vision, "c", "Chambre", "Personne détectée")).is_none());

        // Boîte noire : la dernière minute de la prise du bureau.
        let mut trace = Trace { device: "bf00".into(), name: "Multiprise - Bureau".into(), samples: Vec::new() };
        for i in 0..40 {
            trace.push(utc(19, 41, 8) + chrono::Duration::seconds(2 * i), Some(276.0 + (i % 8) as f64 * 2.0), Some(240.4));
        }
        assert_eq!(trace.samples.len(), 31, "une minute gardée");
        let hm = |t: DateTime<Local>| t.format("%H:%M").to_string();
        let hms = |t: DateTime<Local>| t.format("%H:%M:%S").to_string();
        let mark = SessionMark { pid: 21496, started: utc(19, 33, 20), alive: utc(19, 42, 28), clean: false, traces: vec![trace] };
        let stays_off = vec!["Multiprise - Bureau".to_string()];
        let mut s = PreviousSession { mark: &mark, log: &log, boot: Some(utc(19, 50, 25)), clues: &clues, powers_pc: None, stays_off: &stays_off, now: utc(19, 55, 0) };
        let (level, text) = host_shutdown(&s).unwrap();
        assert_eq!(level, Level::Warning);
        let start = format!(
            "ce PC a perdu son alimentation à {} pendant que HomeSmart tournait (dernière trace à {}) : Windows a redémarré à {} sans arrêt normal",
            hms(utc(19, 42, 31)),
            hms(utc(19, 42, 28)),
            hm(utc(19, 50, 25))
        );
        assert!(text.starts_with(&start), "{text}");
        assert!(text.contains("(3 coupures en 24 h)"), "{text}");
        let clue = format!("« Chauffage » a signalé une sous-tension à {} (19 s avant la dernière trace de HomeSmart)", hms(utc(19, 42, 9)));
        assert!(text.contains(&clue), "{text}");
        assert!(text.contains("dernières mesures : « Multiprise - Bureau » 290 W et 240 V à") && text.contains("(sur la dernière minute : 276 à 290 W, 240 V au plus bas)"), "{text}");
        assert!(!text.contains("HomeSmart était fermé"), "les coupures d'avant 19:33 ont déjà été dites : {text}");
        assert!(text.contains("« Multiprise - Bureau » reste éteinte après une coupure de courant : si ce PC y est branché"), "{text}");
        // La prise est marquée « alimente ce PC » : réglée pour rester éteinte, on ne peut pas trancher…
        s.powers_pc = Some(("Multiprise - Bureau", Some("off")));
        let t = host_shutdown(&s).unwrap().1;
        assert!(t.contains("la prise « Multiprise - Bureau » qui l'alimente revient éteinte après une coupure : s'il a fallu appuyer sur son bouton, impossible de savoir si elle a redémarré"), "{t}");
        assert!(t.ends_with("si son voyant est resté allumé, la prise n'a pas coupé : c'est ce PC lui-même qui s'est arrêté (son alimentation, ou son câble)"), "{t}");
        // … réglée sur « Reprendre l'état précédent », son voyant tranche.
        s.powers_pc = Some(("Multiprise - Bureau", Some("memory")));
        let t = host_shutdown(&s).unwrap().1;
        assert!(t.contains("si la prise « Multiprise - Bureau » qui l'alimente s'est éteinte puis rallumée seule, elle a redémarré"), "{t}");
        assert!(t.contains("s'il a fallu appuyer sur son bouton, elle a reçu un ordre d'extinction"), "{t}");
        assert!(t.ends_with("c'est ce PC lui-même qui s'est arrêté (son alimentation, ou son câble)"), "{t}");

        // HomeSmart fermé normalement la veille (24/10, 20:20 UTC), démarré le 25 à 19:33 : les deux
        // coupures de 19:25 et 19:31 ont eu lieu pendant qu'il était fermé.
        let now = utc(19, 33, 40);
        let earlier = SystemLog {
            boots: log.boots.iter().filter(|b| **b <= now).copied().collect(),
            unexpected: log.unexpected.iter().filter(|u| u.at <= now).cloned().collect(),
        };
        let yesterday = chrono::Utc.with_ymd_and_hms(2026, 10, 24, 20, 20, 15).unwrap().with_timezone(&Local);
        let closed = SessionMark { alive: yesterday, clean: true, traces: Vec::new(), ..mark.clone() };
        let s2 = PreviousSession { mark: &closed, log: &earlier, boot: Some(utc(19, 32, 22)), clues: &[], powers_pc: None, stays_off: &[], now };
        let (l, t) = host_shutdown(&s2).unwrap();
        assert_eq!(l, Level::Warning);
        let expected = format!("pendant que HomeSmart était fermé, ce PC a perdu son alimentation à {} et {} (ni écran bleu, ni bouton) (2 coupures en 24 h)", hms(utc(19, 25, 53)), hms(utc(19, 31, 1)));
        assert_eq!(t, expected);
        // Même chose si Windows a été arrêté normalement avec HomeSmart ouvert (la marque n'est
        // alors pas « propre ») : ce n'est pas cet arrêt-là qui est mis en cause.
        let killed = SessionMark { alive: chrono::Utc.with_ymd_and_hms(2026, 10, 24, 1, 28, 20).unwrap().with_timezone(&Local), clean: false, ..closed.clone() };
        assert_eq!(host_shutdown(&PreviousSession { mark: &killed, ..s2 }).unwrap().1, expected);

        // Fermeture normale, rien depuis : rien.
        let clean = SessionMark { clean: true, alive: utc(19, 51, 0), ..mark.clone() };
        assert!(host_shutdown(&PreviousSession { mark: &clean, ..s }).is_none());
        // HomeSmart arrêté net sans redémarrage du PC : information.
        let alone = SessionMark { alive: utc(19, 51, 0), ..mark.clone() };
        let (l, t) = host_shutdown(&PreviousSession { mark: &alone, boot: Some(utc(7, 0, 0)), clues: &[], ..s }).unwrap();
        assert_eq!(l, Level::Info);
        assert!(t.starts_with(&format!("HomeSmart s'est arrêté net vers {}", hm(utc(19, 51, 0)))), "{t}");
    }

    /// Sur ce PC (journal de Windows réel) :
    /// `cargo test --release --lib -- --ignored live_system_log --nocapture`.
    #[test]
    #[ignore]
    fn live_system_log() {
        let log = system_log();
        println!("{} démarrage(s) en 7 jours", log.boots.len());
        for u in log.unexpected {
            println!("arrêt non prévu à {:?}, redémarrage à {} (écran bleu 0x{:X}, bouton : {})", u.cut_at, u.at, u.bugcheck, u.power_button);
        }
        println!("démarrage du système : {:?}", system_boot());
    }

    #[test]
    fn session_mark_survives_nul_bytes() {
        let p = std::env::temp_dir().join(format!("hs-session-{}.json", rand::random::<u32>()));
        let mut m = SessionMark::new(at(20, 33, 20));
        m.traces.push(Trace { device: "a".into(), name: "Prise".into(), samples: vec![(at(20, 33, 18), Some(291.1), Some(240.7))] });
        m.save(&p);
        let back = SessionMark::load(&p).unwrap();
        assert_eq!((back.started, back.traces.clone()), (at(20, 33, 20), m.traces.clone()));
        std::fs::write(&p, [&[0u8; 40][..], serde_json::to_string(&m).unwrap().as_bytes()].concat()).unwrap();
        assert!(SessionMark::load(&p).is_some());
        let _ = std::fs::remove_file(p);
    }
}
