//! Garde-fous (sécurité des appareils à risque), programmes horaires, et scénarios déclenchés
//! par ce que voient et entendent les caméras.
//!
//! Les décisions sont des fonctions pures, testées sans réseau ; le hub les
//! applique à chaque changement d'état et à chaque battement de cœur.

use chrono::{Datelike, Duration as ChronoDuration, NaiveDateTime, NaiveTime, Timelike};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

// ---- Heures --------------------------------------------------------------------

pub fn parse_hm(s: &str) -> Option<NaiveTime> {
    let (h, m) = s.trim().split_once(':')?;
    NaiveTime::from_hms_opt(h.trim().parse().ok()?, m.trim().parse().ok()?, 0)
}

pub fn fmt_hm(t: NaiveTime) -> String {
    format!("{:02}:{:02}", t.hour(), t.minute())
}

/// Plage horaire, éventuellement à cheval sur minuit (22:00 → 06:00).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TimeRange {
    pub start: String,
    pub end: String,
}

impl TimeRange {
    pub fn contains(&self, t: NaiveTime) -> bool {
        let (Some(a), Some(b)) = (parse_hm(&self.start), parse_hm(&self.end)) else { return false };
        if a <= b {
            t >= a && t < b
        } else {
            t >= a || t < b
        }
    }

    pub fn label(&self) -> String {
        format!("{} – {}", self.start, self.end)
    }
}

// ---- Garde-fous ------------------------------------------------------------------

/// Garde-fous d'un appareil. Ils ne s'appliquent que si `enabled` est vrai.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Safety {
    #[serde(default)]
    pub enabled: bool,
    /// Demander confirmation avant d'allumer depuis HomeSmart.
    #[serde(default)]
    pub confirm_on: bool,
    /// Extinction automatique après N minutes allumé (minuterie de l'appareil si possible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_on_minutes: Option<u32>,
    /// Coupure si la puissance dépasse ce seuil (W) sur deux mesures consécutives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_power_w: Option<f64>,
    /// Allumage interdit (et extinction) sur cette plage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forbidden: Option<TimeRange>,
    /// Couper dès que l'appareil signale un défaut (surintensité, surtension…).
    #[serde(default)]
    pub off_on_fault: bool,
}

impl Safety {
    pub fn is_default(&self) -> bool {
        *self == Safety::default()
    }

    /// Réglages conseillés pour un chauffage branché sur une prise connectée.
    pub fn heater_preset() -> Safety {
        Safety {
            enabled: true,
            confirm_on: true,
            max_on_minutes: Some(240),
            max_power_w: Some(3000.0),
            forbidden: None,
            off_on_fault: true,
        }
    }

    pub fn forbids_now(&self, now: NaiveTime) -> bool {
        self.enabled && self.forbidden.as_ref().is_some_and(|r| r.contains(now))
    }
}

/// Le nom suggère-t-il un appareil à risque (chauffage, fer, four…) ?
pub fn looks_risky(name: &str) -> bool {
    let n = name.to_lowercase();
    [
        "chauff", "radiat", "convect", "heater", "four", "fer ", "fer à", "bouilloire", "plaque", "sèche", "seche", "grill", "friteuse",
        "kettle", "iron", "oven", "poêle", "poele", "cuiseur", "plancha",
    ]
    .iter()
    .any(|k| n.contains(k))
        || n == "fer"
}

/// Mémoire des garde-fous pour un appareil (persiste entre reconnexions).
#[derive(Default, Debug)]
pub struct GuardState {
    pub on_since: Option<Instant>,
    pub over_power: u32,
    /// Dernière coupure envoyée (anti-répétition le temps que l'appareil confirme).
    pub last_off: Option<Instant>,
}

#[derive(Debug, PartialEq)]
pub enum GuardAction {
    /// Éteindre, avec la raison.
    TurnOff(String),
    /// Programmer la minuterie de l'appareil (secondes).
    SetCountdown(u64),
}

/// Ce que le garde-fou sait de l'appareil à un instant donné.
pub struct Observation {
    pub now_local: NaiveTime,
    pub on: Option<bool>,
    pub power_w: Option<f64>,
    pub faults: u32,
    pub fault_labels: Vec<String>,
    pub has_countdown: bool,
    pub countdown_left: Option<u64>,
}

/// Évalue les garde-fous. `fresh_power` indique qu'une nouvelle mesure de puissance vient d'arriver.
/// (Les défauts signalés sont aussi des problèmes : `problems` les inscrit au journal.)
pub fn evaluate(s: &Safety, st: &mut GuardState, o: &Observation, fresh_power: bool, now: Instant) -> Vec<GuardAction> {
    let mut out = Vec::new();
    let on = o.on.unwrap_or(false);
    if !on {
        st.on_since = None;
        st.over_power = 0;
    }
    if !s.enabled || !on {
        return out;
    }
    let just_on = st.on_since.is_none();
    if just_on {
        st.on_since = Some(now);
    }
    if let Some(r) = s.forbidden.as_ref().filter(|r| r.contains(o.now_local)) {
        out.push(GuardAction::TurnOff(format!("allumage interdit de {}", r.label())));
        return out;
    }
    if s.off_on_fault && o.faults != 0 {
        out.push(GuardAction::TurnOff(format!("défaut : {}", o.fault_labels.join(", "))));
        return out;
    }
    if let (Some(limit), Some(p)) = (s.max_power_w, o.power_w) {
        if fresh_power {
            st.over_power = if p > limit { st.over_power + 1 } else { 0 };
        }
        if st.over_power >= 2 {
            st.over_power = 0;
            out.push(GuardAction::TurnOff(format!("puissance de {p:.0} W au-delà de la limite de {limit:.0} W")));
            return out;
        }
    }
    if let Some(max) = s.max_on_minutes.filter(|m| *m > 0) {
        let max_s = max as u64 * 60;
        let elapsed = st.on_since.map(|t| now.duration_since(t)).unwrap_or_default();
        if elapsed > Duration::from_secs(max_s + 30) {
            out.push(GuardAction::TurnOff(format!("durée maximale de {} atteinte", fmt_minutes(max))));
            return out;
        }
        // Filet côté appareil : la minuterie l'éteindra même si HomeSmart est fermé.
        let remaining = max_s.saturating_sub(elapsed.as_secs()).max(60);
        if o.has_countdown && o.countdown_left.is_none_or(|c| c == 0 || c > remaining + 60) {
            out.push(GuardAction::SetCountdown(remaining));
        }
    }
    out
}

pub fn fmt_minutes(m: u32) -> String {
    match (m / 60, m % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m:02}"),
    }
}

// ---- Programmes ------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    On,
    Off,
    /// Aspirateur robot : lancer un nettoyage.
    Clean,
    /// Aspirateur robot : retour à la base.
    Dock,
}

impl Action {
    /// L'action qui termine une plage : éteindre après allumer, rentrer après nettoyer.
    pub fn inverse(self) -> Action {
        match self {
            Action::On => Action::Off,
            Action::Off => Action::On,
            Action::Clean => Action::Dock,
            Action::Dock => Action::Clean,
        }
    }

    /// Réservée aux aspirateurs robots ?
    pub fn for_vacuum(self) -> bool {
        matches!(self, Action::Clean | Action::Dock)
    }

    /// Verbe à l'infinitif : « allumer », « nettoyer avec »…
    pub fn verb(self) -> &'static str {
        match self {
            Action::On => "allumer",
            Action::Off => "éteindre",
            Action::Clean => "nettoyer avec",
            Action::Dock => "renvoyer à sa base",
        }
    }

    /// Nom de l'action : « allumage », « nettoyage »…
    pub fn noun(self) -> &'static str {
        match self {
            Action::On => "allumage",
            Action::Off => "extinction",
            Action::Clean => "nettoyage",
            Action::Dock => "retour à la base",
        }
    }
}

/// Programme horaire : `action` à `start` les jours cochés ; si `end` est donné,
/// l'action inverse est faite à `end` (plage, éventuellement après minuit).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Schedule {
    pub id: u64,
    pub device: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Lundi = 0 … dimanche = 6.
    pub days: [bool; 7],
    pub start: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    pub action: Action,
}

fn yes() -> bool {
    true
}

pub const DAY_LETTERS: [&str; 7] = ["L", "M", "M", "J", "V", "S", "D"];
pub const DAY_NAMES: [&str; 7] = ["lundi", "mardi", "mercredi", "jeudi", "vendredi", "samedi", "dimanche"];

/// Un déclenchement concret.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub schedule: u64,
    pub device: String,
    /// Allumer (ou nettoyer) plutôt qu'éteindre (ou rentrer).
    pub on: bool,
    pub action: Action,
    pub at: NaiveDateTime,
}

impl Schedule {
    pub fn validate(&self) -> Result<(), String> {
        if !self.days.iter().any(|d| *d) {
            return Err("choisis au moins un jour".into());
        }
        let start = parse_hm(&self.start).ok_or("heure de début invalide")?;
        if let Some(e) = &self.end {
            let end = parse_hm(e).ok_or("heure de fin invalide")?;
            if end == start {
                return Err("le début et la fin sont identiques".into());
            }
        }
        Ok(())
    }

    fn day(dt: NaiveDateTime) -> usize {
        dt.weekday().num_days_from_monday() as usize
    }

    /// Événements de ce programme exactement à la minute `minute`.
    pub fn events_at(&self, minute: NaiveDateTime) -> Vec<Event> {
        let mut out = Vec::new();
        if !self.enabled {
            return out;
        }
        let hm = minute.time().with_second(0).unwrap_or(minute.time());
        let ev = |action: Action| Event { schedule: self.id, device: self.device.clone(), on: matches!(action, Action::On | Action::Clean), action, at: minute };
        let Some(start) = parse_hm(&self.start) else { return out };
        if self.days[Self::day(minute)] && hm == start {
            out.push(ev(self.action));
        }
        if let Some(end) = self.end.as_deref().and_then(parse_hm) {
            // Une plage qui passe minuit se termine le lendemain du jour coché.
            let start_day = if end > start { minute } else { minute - ChronoDuration::days(1) };
            if self.days[Self::day(start_day)] && hm == end {
                out.push(ev(self.action.inverse()));
            }
        }
        out
    }

    /// Prochain déclenchement après `now` (dans les 8 jours).
    pub fn next_event(&self, now: NaiveDateTime) -> Option<Event> {
        let base = now.date().and_hms_opt(now.hour(), now.minute(), 0)?;
        let mut cands: Vec<NaiveDateTime> = Vec::new();
        for d in 0..=8 {
            let date = base.date() + ChronoDuration::days(d);
            for t in [Some(&self.start), self.end.as_ref()].into_iter().flatten() {
                if let Some(t) = parse_hm(t) {
                    let dt = date.and_time(t);
                    if dt > base {
                        cands.push(dt);
                    }
                }
            }
        }
        cands.sort();
        cands.into_iter().find_map(|dt| self.events_at(dt).into_iter().next())
    }

    pub fn days_label(&self) -> String {
        let n = self.days.iter().filter(|d| **d).count();
        match n {
            7 => "tous les jours".into(),
            _ if self.days == [true, true, true, true, true, false, false] => "en semaine".into(),
            _ if self.days == [false, false, false, false, false, true, true] => "le week-end".into(),
            _ => self
                .days
                .iter()
                .enumerate()
                .filter(|(_, d)| **d)
                .map(|(i, _)| &DAY_NAMES[i][..3])
                .collect::<Vec<_>>()
                .join(" "),
        }
    }

    pub fn summary(&self) -> String {
        match (&self.end, self.action) {
            (Some(end), Action::On) => format!("Allumé de {} à {}, {}", self.start, end, self.days_label()),
            (Some(end), Action::Off) => format!("Éteint de {} à {}, {}", self.start, end, self.days_label()),
            (Some(end), Action::Clean) => format!("Nettoyage de {} à {} (puis retour à la base), {}", self.start, end, self.days_label()),
            (Some(end), Action::Dock) => format!("Sur sa base de {} à {}, {}", self.start, end, self.days_label()),
            (None, Action::On) => format!("Allumer à {}, {}", self.start, self.days_label()),
            (None, Action::Off) => format!("Éteindre à {}, {}", self.start, self.days_label()),
            (None, Action::Clean) => format!("Nettoyer à {}, {}", self.start, self.days_label()),
            (None, Action::Dock) => format!("Retour à la base à {}, {}", self.start, self.days_label()),
        }
    }
}

/// Événements dus entre `from` (exclu) et `to` (inclus), minute par minute.
/// Au-delà de 3 minutes de retard (PC en veille…), on n'exécute rien en rattrapage.
pub fn due_events(schedules: &[Schedule], from: NaiveDateTime, to: NaiveDateTime) -> Vec<Event> {
    let mut out = Vec::new();
    let to = to.date().and_hms_opt(to.hour(), to.minute(), 0).unwrap_or(to);
    let mut m = from.date().and_hms_opt(from.hour(), from.minute(), 0).unwrap_or(from) + ChronoDuration::minutes(1);
    if to - m > ChronoDuration::minutes(3) {
        m = to;
    }
    while m <= to {
        for s in schedules {
            out.extend(s.events_at(m));
        }
        m += ChronoDuration::minutes(1);
    }
    out
}

// ---- Scénarios : réactions à ce que voient et entendent les caméras --------------------------

/// Ce qui déclenche un scénario (sur une caméra dont l'analyse IA est active).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// Quelqu'un arrive dans le champ.
    Arrival,
    /// Personne n'a été vu depuis tant de minutes (pièce vide).
    Absence { minutes: u32 },
    /// Chute (et personne restée au sol).
    Fall,
    /// Appel à l'aide : deux bras levés, ou « au secours » entendu.
    HelpCall,
    /// Alarme incendie entendue (détecteur de fumée).
    FireAlarm,
    /// Bruit de verre brisé.
    GlassBreak,
    /// Quelqu'un arrive alors que le mode absence est activé.
    Intrusion,
    /// Coups répétés entre deux personnes.
    Violence,
    /// Sabotage d'une caméra : objectif masqué, caméra détournée ou image floutée.
    Tamper,
    /// Le mode absence vient d'être activé (maison vide).
    AwayOn,
    /// Le mode absence vient d'être désactivé (retour à la maison).
    AwayOff,
}

impl Trigger {
    /// Les déclencheurs proposés, dans l'ordre de l'interface (absence : 30 min par défaut).
    pub const CHOICES: [Trigger; 9] = [Trigger::Arrival, Trigger::Absence { minutes: 30 }, Trigger::Intrusion, Trigger::Fall, Trigger::HelpCall, Trigger::Violence, Trigger::Tamper, Trigger::FireAlarm, Trigger::GlassBreak];

    pub fn label(self) -> String {
        match self {
            Trigger::Arrival => "quelqu'un arrive".into(),
            Trigger::Absence { minutes } => format!("personne depuis {}", fmt_minutes(minutes)),
            Trigger::Fall => "une chute".into(),
            Trigger::HelpCall => "un appel à l'aide".into(),
            Trigger::FireAlarm => "une alarme incendie".into(),
            Trigger::GlassBreak => "du verre brisé".into(),
            Trigger::Intrusion => "une intrusion (mode absence)".into(),
            Trigger::Violence => "des coups (violence)".into(),
            Trigger::Tamper => "un sabotage de caméra".into(),
            Trigger::AwayOn => "mode absence activé".into(),
            Trigger::AwayOff => "mode absence désactivé".into(),
        }
    }

    /// Déclencheur de la maison entière (sans caméra) ?
    pub fn house_wide(self) -> bool {
        matches!(self, Trigger::AwayOn | Trigger::AwayOff)
    }

    /// Même sorte de déclencheur (l'absence, quelle que soit sa durée).
    pub fn same_kind(self, other: Trigger) -> bool {
        std::mem::discriminant(&self) == std::mem::discriminant(&other)
    }

    /// Le son ou l'image doit-il être écouté pour ce déclencheur ?
    pub fn needs_listening(self) -> bool {
        matches!(self, Trigger::FireAlarm | Trigger::GlassBreak)
    }
}

/// « Quand [déclencheur] sur [caméra] : [allumer / éteindre / nettoyer…] [appareil] »,
/// éventuellement seulement sur une plage horaire. Sans caméra (`camera` vide) : n'importe quelle
/// caméra analysée (absence : personne sur aucune, la maison est vide), ou la maison entière
/// (mode absence).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Scenario {
    pub id: u64,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub camera: String,
    pub trigger: Trigger,
    pub device: String,
    pub action: Action,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub during: Option<TimeRange>,
}

/// Délai entre deux déclenchements d'un même scénario sur événement (arrivée…) : une personne
/// qui entre et sort ne fait pas clignoter la lampe.
pub const SCENARIO_COOLDOWN: Duration = Duration::from_secs(60);

impl Scenario {
    pub fn validate(&self) -> Result<(), String> {
        if self.device.is_empty() {
            return Err("choisis un appareil".into());
        }
        if let Trigger::Absence { minutes } = self.trigger {
            if minutes == 0 {
                return Err("durée d'absence nulle".into());
            }
        }
        if let Some(r) = &self.during {
            let (a, b) = (parse_hm(&r.start).ok_or("heure de début invalide")?, parse_hm(&r.end).ok_or("heure de fin invalide")?);
            if a == b {
                return Err("le début et la fin sont identiques".into());
            }
        }
        Ok(())
    }

    /// Actif à cette heure (activé, et dans sa plage s'il en a une).
    pub fn active_at(&self, t: NaiveTime) -> bool {
        self.enabled && self.during.as_ref().is_none_or(|r| r.contains(t))
    }

    /// « personne depuis 30 min → éteindre Radiateur (de 22:00 à 07:00) »
    pub fn summary(&self, device_name: &str) -> String {
        let when = self.during.as_ref().map(|r| format!(" (de {} à {})", r.start, r.end)).unwrap_or_default();
        format!("{} → {} « {device_name} »{when}", self.trigger.label(), self.action.verb())
    }

    /// Où se produit le déclencheur : « Salon », « Toutes les caméras », « Maison ».
    pub fn place(&self, camera_name: Option<String>) -> String {
        if self.trigger.house_wide() {
            "Maison".into()
        } else if self.camera.is_empty() {
            "Toutes les caméras".into()
        } else {
            camera_name.unwrap_or_else(|| self.camera.clone())
        }
    }
}

/// Une absence est-elle due ? `empty_for` : depuis combien de temps personne n'est en vue (ou
/// depuis le début de l'analyse, si personne n'a encore été vu) ; `fired` : déjà déclenchée pour
/// cette absence-ci.
pub fn absence_due(minutes: u32, empty_for: Duration, fired: bool) -> bool {
    !fired && minutes > 0 && empty_for >= Duration::from_secs(minutes as u64 * 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenarios_labels_hours_and_absence() {
        let s = Scenario {
            id: 1,
            enabled: true,
            camera: "cam-1".into(),
            trigger: Trigger::Absence { minutes: 30 },
            device: "radiateur".into(),
            action: Action::Off,
            during: Some(TimeRange { start: "22:00".into(), end: "07:00".into() }),
        };
        assert_eq!(s.summary("Radiateur"), "personne depuis 30 min → éteindre « Radiateur » (de 22:00 à 07:00)");
        assert!(s.validate().is_ok());
        assert!(s.active_at(parse_hm("23:30").unwrap()) && s.active_at(parse_hm("06:59").unwrap()));
        assert!(!s.active_at(parse_hm("12:00").unwrap()));
        assert!(!Scenario { enabled: false, ..s.clone() }.active_at(parse_hm("23:30").unwrap()));
        assert!(Scenario { device: String::new(), ..s.clone() }.validate().is_err());
        // Sans caméra : toutes les caméras (maison vide), ou la maison entière (mode absence).
        let vac = Scenario { camera: String::new(), action: Action::Clean, trigger: Trigger::AwayOn, during: None, ..s.clone() };
        assert!(vac.validate().is_ok());
        assert_eq!(vac.summary("Robot"), "mode absence activé → nettoyer avec « Robot »");
        assert_eq!(vac.place(None), "Maison");
        assert_eq!(Scenario { trigger: Trigger::Absence { minutes: 30 }, ..vac.clone() }.place(None), "Toutes les caméras");
        assert_eq!(serde_json::to_string(&Trigger::AwayOn).unwrap(), r#""away_on""#);
        assert!(Scenario { trigger: Trigger::Absence { minutes: 0 }, ..s.clone() }.validate().is_err());
        // L'absence ne se déclenche qu'une fois, et seulement après la durée.
        assert!(!absence_due(30, Duration::from_secs(29 * 60), false));
        assert!(absence_due(30, Duration::from_secs(30 * 60), false));
        assert!(!absence_due(30, Duration::from_secs(90 * 60), true));
        assert!(Trigger::Absence { minutes: 5 }.same_kind(Trigger::Absence { minutes: 60 }));
        assert!(!Trigger::Fall.same_kind(Trigger::Arrival));
        // Format enregistré dans la configuration.
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""trigger":{"absence":{"minutes":30}}"#), "{json}");
        assert_eq!(serde_json::from_str::<Scenario>(&json).unwrap(), s);
        assert_eq!(serde_json::to_string(&Trigger::HelpCall).unwrap(), r#""help_call""#);
    }
    use chrono::NaiveDate;

    fn at(d: u32, h: u32, m: u32) -> NaiveDateTime {
        // Le 19/10/2026 est un lundi.
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap().and_hms_opt(h, m, 0).unwrap()
    }

    fn sched(days: [bool; 7], start: &str, end: Option<&str>) -> Schedule {
        Schedule { id: 1, device: "x".into(), enabled: true, days, start: start.into(), end: end.map(str::to_string), action: Action::On }
    }

    #[test]
    fn range_over_midnight() {
        let r = TimeRange { start: "22:00".into(), end: "06:00".into() };
        assert!(r.contains(parse_hm("23:30").unwrap()));
        assert!(r.contains(parse_hm("05:59").unwrap()));
        assert!(!r.contains(parse_hm("06:00").unwrap()));
        assert!(!r.contains(parse_hm("12:00").unwrap()));
    }

    #[test]
    fn weekday_plage() {
        let s = sched([true, true, true, true, true, false, false], "06:30", Some("08:00"));
        assert_eq!(s.events_at(at(19, 6, 30)).len(), 1);
        assert!(s.events_at(at(19, 6, 30))[0].on);
        assert!(!s.events_at(at(19, 8, 0))[0].on);
        assert!(s.events_at(at(24, 6, 30)).is_empty(), "samedi");
        assert_eq!(s.days_label(), "en semaine");
    }

    #[test]
    fn vacuum_schedules() {
        let mut s = sched([true; 7], "10:00", Some("11:30"));
        s.action = Action::Clean;
        assert_eq!(s.summary(), "Nettoyage de 10:00 à 11:30 (puis retour à la base), tous les jours");
        let start = s.events_at(at(19, 10, 0));
        assert_eq!((start[0].action, start[0].on), (Action::Clean, true));
        let end = s.events_at(at(19, 11, 30));
        assert_eq!((end[0].action, end[0].on), (Action::Dock, false));
        s.end = None;
        assert_eq!(s.summary(), "Nettoyer à 10:00, tous les jours");
        assert_eq!(serde_json::to_string(&Action::Clean).unwrap(), r#""clean""#);
        assert!(Action::Dock.for_vacuum() && !Action::On.for_vacuum());
    }

    #[test]
    fn overnight_end_is_next_day() {
        // Coché le dimanche seulement : 22:00 dimanche → 06:00 lundi.
        let mut days = [false; 7];
        days[6] = true;
        let s = sched(days, "22:00", Some("06:00"));
        assert_eq!(s.events_at(at(25, 22, 0)).len(), 1);
        let end = s.events_at(at(26, 6, 0));
        assert_eq!(end.len(), 1);
        assert!(!end[0].on);
        assert!(s.events_at(at(25, 6, 0)).is_empty());
    }

    #[test]
    fn due_events_no_late_catch_up() {
        let s = sched([true; 7], "07:00", None);
        assert_eq!(due_events(&[s.clone()], at(19, 6, 59), at(19, 7, 0)).len(), 1);
        assert_eq!(due_events(&[s.clone()], at(19, 6, 58), at(19, 7, 1)).len(), 1);
        // Réveil 2 h après : rien n'est rattrapé.
        assert!(due_events(&[s], at(19, 5, 0), at(19, 7, 30)).is_empty());
    }

    #[test]
    fn next_event_skips_unchecked_days() {
        let mut days = [false; 7];
        days[2] = true; // mercredi
        let s = sched(days, "07:00", None);
        let e = s.next_event(at(19, 12, 0)).unwrap();
        assert_eq!(e.at, at(21, 7, 0));
    }

    fn obs(on: bool, power: Option<f64>) -> Observation {
        Observation {
            now_local: parse_hm("12:00").unwrap(),
            on: Some(on),
            power_w: power,
            faults: 0,
            fault_labels: vec![],
            has_countdown: true,
            countdown_left: Some(0),
        }
    }

    #[test]
    fn guard_sets_countdown_then_turns_off() {
        let s = Safety { enabled: true, max_on_minutes: Some(60), ..Default::default() };
        let mut st = GuardState::default();
        let t0 = Instant::now();
        assert_eq!(evaluate(&s, &mut st, &obs(true, None), false, t0), vec![GuardAction::SetCountdown(3600)]);
        let mut o = obs(true, None);
        o.countdown_left = Some(3500);
        assert!(evaluate(&s, &mut st, &o, false, t0 + Duration::from_secs(100)).is_empty());
        let late = evaluate(&s, &mut st, &o, false, t0 + Duration::from_secs(3700));
        assert!(matches!(late[0], GuardAction::TurnOff(_)));
    }

    #[test]
    fn guard_power_limit_needs_two_readings() {
        let s = Safety { enabled: true, max_power_w: Some(2000.0), ..Default::default() };
        let mut st = GuardState::default();
        let now = Instant::now();
        assert!(evaluate(&s, &mut st, &obs(true, Some(2500.0)), true, now).is_empty());
        assert!(matches!(evaluate(&s, &mut st, &obs(true, Some(2500.0)), true, now)[0], GuardAction::TurnOff(_)));
        // Une valeur répétée sans nouvelle mesure ne compte pas.
        assert!(evaluate(&s, &mut st, &obs(true, Some(2500.0)), false, now).is_empty());
    }

    #[test]
    fn guard_forbidden_window_and_disabled() {
        let mut s = Safety { enabled: true, forbidden: Some(TimeRange { start: "11:00".into(), end: "13:00".into() }), ..Default::default() };
        let mut st = GuardState::default();
        assert!(matches!(evaluate(&s, &mut st, &obs(true, None), false, Instant::now())[0], GuardAction::TurnOff(_)));
        s.enabled = false;
        let mut st = GuardState::default();
        assert!(evaluate(&s, &mut st, &obs(true, None), false, Instant::now()).is_empty());
    }

    #[test]
    fn risky_names() {
        assert!(looks_risky("Chauffage"));
        assert!(looks_risky("Radiateur salle de bain"));
        assert!(!looks_risky("Multiprise - Bureau"));
    }
}
