//! Aspirateurs robots : modèle commun (état, commandes, erreurs) et pilotes locaux.
//!
//! Familles prises en charge, de la plus simple à la plus exigeante :
//! - **Tuya / Smart Life** (catégorie « sd ») : le protocole local de HomeSmart, avec la clé
//!   locale importée une fois depuis Smart Life (`tuya`) ;
//! - **Valetudo** (micrologiciel libre installé sur des robots Dreame, Roborock, Xiaomi…) : API REST
//!   locale, sans compte ni cloud (`valetudo`) ;
//! - **miIO** (Xiaomi, Roborock, Dreame fabriqués avant 2022) : UDP 54321 en local, chiffré avec le
//!   jeton de l'appareil, qu'il faut obtenir une fois (`miio`).
//!
//! Les Dreame récents (application Dreamehome) n'ont aucune API locale : seul leur cloud, ou
//! Valetudo après remplacement du micrologiciel, permet de les piloter.
//!
//! Chaque pilote traduit l'état de son robot dans `Status` et les commandes de `Command` : le
//! hub, l'interface, la TUI, la ligne de commande, les programmes et les scénarios ne voient que
//! ce modèle commun.

pub mod miio;
pub mod scan;
pub mod sim;
pub mod tuya;
pub mod valetudo;

use crate::journal::Level;
use serde::{Deserialize, Serialize};

/// Ce que fait le robot.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    Cleaning,
    Paused,
    Returning,
    Charging,
    /// Sur sa base, batterie pleine.
    Docked,
    /// À l'arrêt, hors de sa base.
    Idle,
    /// Entretien à la station (vidage du bac, lavage ou séchage des serpillières).
    Station,
    Error,
    #[default]
    Unknown,
}

impl Activity {
    pub fn label(self) -> &'static str {
        match self {
            Activity::Cleaning => "Nettoyage en cours",
            Activity::Paused => "En pause",
            Activity::Returning => "Retour à la base",
            Activity::Charging => "En charge",
            Activity::Docked => "Sur sa base",
            Activity::Idle => "À l'arrêt",
            Activity::Station => "Entretien à la station",
            Activity::Error => "En erreur",
            Activity::Unknown => "État inconnu",
        }
    }

    /// En mouvement (nettoyage ou retour) : l'état est relu plus souvent.
    pub fn is_active(self) -> bool {
        matches!(self, Activity::Cleaning | Activity::Returning)
    }

    /// Sur sa base (en charge, chargé, ou entretien à la station).
    pub fn at_dock(self) -> bool {
        matches!(self, Activity::Charging | Activity::Docked | Activity::Station)
    }
}

/// Sorte d'erreur signalée, commune à toutes les familles de robots.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    MainBrush,
    SideBrush,
    Wheel,
    Stuck,
    BinMissing,
    BinFull,
    Cliff,
    Bumper,
    Lidar,
    Lifted,
    BatteryFault,
    LowBattery,
    Charging,
    Dock,
    WaterTank,
    Mop,
    Filter,
    Fan,
    Sensor,
    Zone,
    Other,
}

impl ErrorKind {
    /// (intitulé, conseil, gravité).
    pub fn describe(self) -> (&'static str, &'static str, Level) {
        use ErrorKind::*;
        match self {
            MainBrush => ("Brosse principale bloquée", "retire les cheveux et les fils enroulés autour de la brosse", Level::Warning),
            SideBrush => ("Brosse latérale bloquée", "dégage la brosse latérale (cheveux, fils, franges de tapis)", Level::Warning),
            Wheel => ("Roue bloquée", "vérifie qu'aucun objet ne coince une roue", Level::Warning),
            Stuck => ("Robot coincé", "il ne peut plus avancer : dégage-le, puis relance le nettoyage", Level::Warning),
            BinMissing => ("Bac à poussière absent", "remets le bac à poussière en place", Level::Warning),
            BinFull => ("Bac à poussière plein", "vide le bac à poussière", Level::Info),
            Cliff => ("Capteurs de vide", "essuie les capteurs sous le robot, ou éloigne-le du bord d'une marche", Level::Warning),
            Bumper => ("Pare-chocs bloqué", "vérifie que le pare-chocs n'est pas coincé", Level::Warning),
            Lidar => ("Laser de navigation bloqué", "vérifie que rien n'obstrue la tourelle du laser", Level::Warning),
            Lifted => ("Robot soulevé", "une roue est dans le vide : repose le robot à plat", Level::Warning),
            BatteryFault => ("Batterie défaillante", "débranche la base et fais vérifier la batterie avant de relancer le robot", Level::Critical),
            LowBattery => ("Batterie trop faible", "replace le robot sur sa base pour qu'il se recharge", Level::Warning),
            Charging => ("Problème de charge", "nettoie les contacts du robot et de la base, et vérifie que la base est alimentée", Level::Warning),
            Dock => ("Problème de station", "vérifie la station (réservoirs, sac, alimentation)", Level::Warning),
            WaterTank => ("Réservoir d'eau", "remplis ou remets en place le réservoir d'eau", Level::Info),
            Mop => ("Serpillière", "vérifie la serpillière (absente, mal fixée ou à retirer)", Level::Info),
            Filter => ("Filtre bouché", "nettoie ou remplace le filtre", Level::Warning),
            Fan => ("Aspiration en panne", "vérifie le bac et le filtre ; si l'erreur revient, le moteur d'aspiration est en cause", Level::Warning),
            Sensor => ("Capteur en défaut", "essuie les capteurs du robot", Level::Warning),
            Zone => ("Zone interdite ou mur virtuel", "le robot est bloqué par une zone interdite : déplace-le", Level::Info),
            Other => ("Erreur du robot", "vérifie le robot", Level::Warning),
        }
    }

    /// Reconnaît une erreur d'après un nom ou un message (anglais, comme les envoient les robots).
    pub fn guess(text: &str) -> ErrorKind {
        let t = text.to_lowercase();
        let has = |words: &[&str]| words.iter().any(|w| t.contains(w));
        // Du plus précis au plus vague : « side brush » avant « brush », « filter blocked » avant
        // « blocked », « water box » avant « box ».
        let table: [(&[&str], ErrorKind); 20] = [
            (&["side brush", "side_brush", "sidebrush", "edge_sweep", "edge brush", "edge_brush"], ErrorKind::SideBrush),
            (&["main brush", "main_brush", "middle_sweep", "roll_brush", "rolling brush", "roller brush", "brush"], ErrorKind::MainBrush),
            (&["wheel suspend", "suspend", "lifted", "wheel_hang", "hanging", "picked up"], ErrorKind::Lifted),
            (&["battery fault", "battery failure", "battery_fault", "battery temperature", "overheat"], ErrorKind::BatteryFault),
            (&["low battery", "battery low", "low_power", "battery_low", "low_bat"], ErrorKind::LowBattery),
            (&["dock", "station", "dust bag", "clean water", "dirty water"], ErrorKind::Dock),
            (&["water", "tank", "cistern"], ErrorKind::WaterTank),
            (&["full"], ErrorKind::BinFull),
            (&["dust bin", "dustbin", "garbage", "bin missing", "dust box", "box"], ErrorKind::BinMissing),
            (&["wheel"], ErrorKind::Wheel),
            (&["cliff", "land_check", "drop"], ErrorKind::Cliff),
            (&["bumper", "collision", "bump"], ErrorKind::Bumper),
            (&["laser", "lidar", "lds"], ErrorKind::Lidar),
            (&["filter"], ErrorKind::Filter),
            (&["fan", "suction"], ErrorKind::Fan),
            (&["stuck", "trapped", "trap", "suffocate", "blocked", "jam"], ErrorKind::Stuck),
            (&["charg"], ErrorKind::Charging),
            (&["mop"], ErrorKind::Mop),
            (&["no-go", "no go", "nogo", "virtual wall", "restricted", "magnet", "zone"], ErrorKind::Zone),
            (&["sensor", "camera", "optical", "infrared", "ultrasonic"], ErrorKind::Sensor),
        ];
        table.iter().find(|(words, _)| has(words)).map(|(_, k)| *k).unwrap_or(ErrorKind::Other)
    }
}

/// Une erreur signalée par le robot.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct VacError {
    /// Code du constructeur (numéro, nom du bit…), pour s'y retrouver dans sa documentation.
    pub code: String,
    pub kind: ErrorKind,
    /// Intitulé en français.
    pub label: String,
    /// Ce qu'il faut faire.
    pub advice: String,
    pub level: Level,
}

impl VacError {
    pub fn new(code: impl Into<String>, kind: ErrorKind) -> VacError {
        let (label, advice, level) = kind.describe();
        VacError { code: code.into(), kind, label: label.into(), advice: advice.into(), level }
    }

    /// Erreur inconnue de HomeSmart : on garde le texte du robot.
    pub fn other(code: impl Into<String>, text: &str) -> VacError {
        let mut e = VacError::new(code, ErrorKind::Other);
        if !text.trim().is_empty() {
            e.label = format!("Erreur du robot : {}", text.trim());
        }
        e
    }

    /// « Brosse principale bloquée : retire les cheveux… (code 5) »
    pub fn sentence(&self) -> String {
        let code = if self.code.is_empty() { String::new() } else { format!(" (code {})", self.code) };
        format!("{} : {}{code}", self.label, self.advice)
    }
}

/// Pièce d'usure (brosses, filtre…) et ce qu'il lui reste.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Consumable {
    pub label: String,
    /// Durée de vie restante, en %.
    pub percent: Option<f64>,
    /// Heures restantes, quand le robot les donne.
    pub hours: Option<f64>,
}

/// Un choix d'un réglage (puissance, mode, débit d'eau) : valeur du robot et libellé.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Choice {
    pub value: String,
    pub label: String,
}

impl Choice {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Choice {
        Choice { value: value.into(), label: label.into() }
    }
}

/// Ce que le robot sait faire (d'après son pilote et ce qu'il annonce).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Caps {
    pub pause: bool,
    pub stop: bool,
    pub dock: bool,
    pub locate: bool,
    /// Puissances d'aspiration.
    pub fans: Vec<Choice>,
    /// Modes de nettoyage, réglage gardé (aspiration seule, lavage…).
    pub modes: Vec<Choice>,
    /// Débits d'eau (serpillière).
    pub waters: Vec<Choice>,
    /// Façons de lancer un nettoyage (automatique, bordures, spirale…), qui démarrent le robot.
    pub patterns: Vec<Choice>,
}

impl Caps {
    fn label_in(list: &[Choice], value: &str) -> String {
        list.iter().find(|c| c.value == value).map(|c| c.label.clone()).unwrap_or_else(|| value.replace('_', " "))
    }

    pub fn fan_label(&self, value: &str) -> String {
        Self::label_in(&self.fans, value)
    }

    pub fn mode_label(&self, value: &str) -> String {
        Self::label_in(&self.modes, value)
    }

    pub fn water_label(&self, value: &str) -> String {
        Self::label_in(&self.waters, value)
    }

    pub fn pattern_label(&self, value: &str) -> String {
        Self::label_in(&self.patterns, value)
    }
}

/// État d'un robot, dans le vocabulaire commun.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Status {
    pub activity: Activity,
    /// Batterie (%).
    pub battery: Option<u8>,
    pub charging: bool,
    /// Puissance d'aspiration (valeur du robot, voir `Caps::fans`).
    pub fan: Option<String>,
    pub mode: Option<String>,
    pub water: Option<String>,
    /// Surface nettoyée pendant le nettoyage en cours (ou le dernier), en m².
    pub area_m2: Option<f64>,
    /// Durée du nettoyage en cours (ou du dernier), en minutes.
    pub clean_min: Option<u32>,
    pub errors: Vec<VacError>,
    pub consumables: Vec<Consumable>,
}

impl Status {
    /// Phrase courte : « Nettoyage en cours · 64 % », « En charge · 35 % ».
    pub fn summary(&self) -> String {
        let mut s = match self.errors.first() {
            Some(e) => e.label.clone(),
            None => self.activity.label().to_string(),
        };
        if let Some(b) = self.battery {
            s += &format!(" · {b} %");
        }
        s
    }
}

/// Commande d'un robot.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case", tag = "do", content = "value")]
pub enum Command {
    /// Lancer (ou reprendre) un nettoyage complet.
    Start,
    Pause,
    Stop,
    /// Retour à la base.
    Dock,
    /// Le robot émet un son pour qu'on le trouve.
    Locate,
    Fan(String),
    Mode(String),
    Water(String),
    /// Lancer un nettoyage d'une certaine façon (bordures, spirale…).
    Pattern(String),
}

impl Command {
    /// Description pour le journal : « Nettoyage lancé », « Puissance d'aspiration : Fort ».
    pub fn describe(&self, caps: &Caps) -> String {
        match self {
            Command::Start => "Nettoyage lancé".into(),
            Command::Pause => "Nettoyage mis en pause".into(),
            Command::Stop => "Nettoyage arrêté".into(),
            Command::Dock => "Retour à la base demandé".into(),
            Command::Locate => "Recherche du robot (signal sonore)".into(),
            Command::Fan(v) => format!("Puissance d'aspiration : {}", caps.fan_label(v)),
            Command::Mode(v) => format!("Mode de nettoyage : {}", caps.mode_label(v)),
            Command::Water(v) => format!("Débit d'eau : {}", caps.water_label(v)),
            Command::Pattern(v) => format!("Nettoyage lancé ({})", caps.pattern_label(v).to_lowercase()),
        }
    }

    /// Vérifie que le robot sait le faire.
    pub fn check(&self, caps: &Caps) -> Result<(), String> {
        let in_list = |list: &[Choice], v: &str, what: &str| {
            if list.iter().any(|c| c.value == v) {
                Ok(())
            } else if list.is_empty() {
                Err(format!("ce robot ne permet pas de choisir {what}"))
            } else {
                Err(format!("« {v} » inconnu ({})", list.iter().map(|c| c.value.as_str()).collect::<Vec<_>>().join(", ")))
            }
        };
        match self {
            Command::Pause if !caps.pause => Err("ce robot ne sait pas se mettre en pause".into()),
            Command::Stop if !caps.stop => Err("ce robot ne sait pas s'arrêter sur place".into()),
            Command::Dock if !caps.dock => Err("ce robot ne sait pas rentrer à sa base sur commande".into()),
            Command::Locate if !caps.locate => Err("ce robot ne sait pas émettre de signal sonore".into()),
            Command::Fan(v) => in_list(&caps.fans, v, "la puissance d'aspiration"),
            Command::Mode(v) => in_list(&caps.modes, v, "le mode de nettoyage"),
            Command::Water(v) => in_list(&caps.waters, v, "le débit d'eau"),
            Command::Pattern(v) => in_list(&caps.patterns, v, "la façon de nettoyer"),
            _ => Ok(()),
        }
    }

    /// Lit une commande écrite à la main (ligne de commande) : start, pause, stop, dock, locate,
    /// fan:<valeur>, mode:<valeur>, water:<valeur>, pattern:<valeur> (noms français acceptés).
    pub fn parse(s: &str) -> Option<Command> {
        let (verb, arg) = match s.split_once(':') {
            Some((a, b)) => (a.trim().to_lowercase(), Some(b.trim().to_string())),
            None => (s.trim().to_lowercase(), None),
        };
        Some(match (verb.as_str(), arg) {
            ("start" | "nettoyer" | "go", None) => Command::Start,
            ("pause", None) => Command::Pause,
            ("stop" | "arreter" | "arrêter", None) => Command::Stop,
            ("dock" | "home" | "base", None) => Command::Dock,
            ("locate" | "find" | "trouver" | "bip", None) => Command::Locate,
            ("fan" | "puissance" | "suction", Some(v)) => Command::Fan(v),
            ("mode", Some(v)) => Command::Mode(v),
            ("water" | "eau", Some(v)) => Command::Water(v),
            ("pattern" | "facon" | "façon", Some(v)) => Command::Pattern(v),
            _ => return None,
        })
    }
}

/// Famille de robot (et donc son pilote).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    Tuya,
    Valetudo,
    Miio,
}

impl Family {
    pub fn label(self) -> &'static str {
        match self {
            Family::Tuya => "Tuya / Smart Life (local)",
            Family::Valetudo => "Valetudo (local, sans compte)",
            Family::Miio => "miIO (local, jeton)",
        }
    }
}

/// Connexion d'un robot qui n'est pas un appareil Tuya (les robots Tuya sont des appareils
/// comme les autres, dans `Config::devices`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case", tag = "family")]
pub enum Link {
    Valetudo {
        host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        /// Identifiants HTTP (si l'authentification de Valetudo est activée).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
    },
    Miio {
        host: String,
        /// Jeton de l'appareil (32 caractères hexadécimaux). Rangé dans les secrets
        /// (`config::Secrets`), comme le mot de passe Valetudo.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        token: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
    },
}

impl Link {
    pub fn family(&self) -> Family {
        match self {
            Link::Valetudo { .. } => Family::Valetudo,
            Link::Miio { .. } => Family::Miio,
        }
    }

    pub fn host(&self) -> &str {
        match self {
            Link::Valetudo { host, .. } | Link::Miio { host, .. } => host,
        }
    }

    pub fn port(&self) -> Option<u16> {
        match self {
            Link::Valetudo { port, .. } | Link::Miio { port, .. } => *port,
        }
    }
}

/// Aspirateur robot hors Tuya, enregistré dans la configuration.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct VacuumConfig {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    pub link: Link,
    /// Modèle annoncé par le robot (rempli à la première connexion).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl VacuumConfig {
    /// Identifiant stable tiré de l'adresse (« vac-192-168-1-59 »).
    pub fn make_id(host: &str) -> String {
        let clean: String = host.trim().chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
        format!("vac-{clean}")
    }

    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.id
        } else {
            &self.name
        }
    }
}

/// Libellé français d'une puissance d'aspiration, quel que soit le vocabulaire du robot.
pub fn fan_label(v: &str) -> String {
    let l = match v.to_lowercase().as_str() {
        "closed" | "off" | "no_suction" | "none" => "Sans aspiration",
        "min" | "minimum" => "Minimum",
        "gentle" | "quiet" | "silent" | "low" | "eco" | "mute" => "Silencieux",
        "normal" | "standard" | "medium" | "middle" | "auto" => "Standard",
        "strong" | "high" | "boost" | "boost_iq" => "Fort",
        "max" | "maximum" | "turbo" | "max+" | "strongest" => "Maximum",
        "custom" => "Personnalisé",
        _ => return v.replace('_', " "),
    };
    l.to_string()
}

/// Libellé français d'un débit d'eau.
pub fn water_label(v: &str) -> String {
    let l = match v.to_lowercase().as_str() {
        "closed" | "off" | "none" => "Sans eau",
        "min" | "low" | "small" => "Faible",
        "middle" | "medium" | "normal" | "standard" => "Moyen",
        "high" | "large" | "big" | "max" => "Fort",
        "custom" => "Personnalisé",
        _ => return v.replace('_', " "),
    };
    l.to_string()
}

/// Libellé français d'un mode ou d'une façon de nettoyer.
pub fn mode_label(v: &str) -> String {
    let l = match v.to_lowercase().as_str() {
        "smart" | "auto" | "smart_clean" => "Automatique",
        "wall_follow" | "edge" | "wall" => "Bordures",
        "spiral" | "spot" | "left_spiral" | "right_spiral" => "Spirale (un endroit)",
        "random" => "Aléatoire",
        "partial_bow" | "zone" | "part" => "Zone",
        "mop" => "Serpillière",
        "vacuum" | "sweeping" | "sweep" => "Aspiration",
        "vacuum_and_mop" | "sweeping_and_mopping" | "sweep_and_mop" => "Aspiration et lavage",
        "vacuum_then_mop" | "mopping_after_sweeping" => "Aspiration puis lavage",
        "mopping" => "Lavage seul",
        _ => return v.replace('_', " "),
    };
    l.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_recognised_and_described() {
        assert_eq!(ErrorKind::guess("Main brush jammed"), ErrorKind::MainBrush);
        assert_eq!(ErrorKind::guess("edge_sweep"), ErrorKind::SideBrush);
        assert_eq!(ErrorKind::guess("Dustbin full"), ErrorKind::BinFull);
        assert_eq!(ErrorKind::guess("garbage_box"), ErrorKind::BinMissing);
        assert_eq!(ErrorKind::guess("Wheel suspended"), ErrorKind::Lifted);
        assert_eq!(ErrorKind::guess("left_wheel"), ErrorKind::Wheel);
        assert_eq!(ErrorKind::guess("Robot stuck"), ErrorKind::Stuck);
        assert_eq!(ErrorKind::guess("LDS jammed"), ErrorKind::Lidar);
        assert_eq!(ErrorKind::guess("Battery temperature too high"), ErrorKind::BatteryFault);
        assert_eq!(ErrorKind::guess("something odd"), ErrorKind::Other);
        let e = VacError::new("5", ErrorKind::MainBrush);
        assert_eq!(e.sentence(), "Brosse principale bloquée : retire les cheveux et les fils enroulés autour de la brosse (code 5)");
        assert_eq!(e.level, Level::Warning);
        assert_eq!(VacError::new("", ErrorKind::BinFull).level, Level::Info);
        assert_eq!(VacError::new("", ErrorKind::BatteryFault).level, Level::Critical);
        assert_eq!(VacError::other("77", "Strange thing").label, "Erreur du robot : Strange thing");
    }

    #[test]
    fn commands_parse_check_and_describe() {
        let caps = Caps { pause: true, stop: false, dock: true, locate: true, fans: vec![Choice::new("strong", "Fort")], ..Default::default() };
        assert_eq!(Command::parse("start"), Some(Command::Start));
        assert_eq!(Command::parse("Nettoyer"), Some(Command::Start));
        assert_eq!(Command::parse("fan:strong"), Some(Command::Fan("strong".into())));
        assert_eq!(Command::parse("fan"), None);
        assert!(Command::Stop.check(&caps).is_err());
        assert!(Command::Fan("strong".into()).check(&caps).is_ok());
        assert!(Command::Fan("max".into()).check(&caps).unwrap_err().contains("strong"));
        assert!(Command::Mode("mop".into()).check(&caps).unwrap_err().contains("ne permet pas"));
        assert_eq!(Command::Fan("strong".into()).describe(&caps), "Puissance d'aspiration : Fort");
        assert_eq!(Command::Dock.describe(&caps), "Retour à la base demandé");
        // Format enregistré (programmes, scénarios, ligne de commande).
        assert_eq!(serde_json::to_string(&Command::Fan("max".into())).unwrap(), r#"{"do":"fan","value":"max"}"#);
        assert_eq!(serde_json::to_string(&Command::Start).unwrap(), r#"{"do":"start"}"#);
    }

    #[test]
    fn labels_and_config() {
        assert_eq!(fan_label("gentle"), "Silencieux");
        assert_eq!(fan_label("max"), "Maximum");
        assert_eq!(fan_label("Boost_IQ"), "Fort");
        assert_eq!(water_label("middle"), "Moyen");
        assert_eq!(mode_label("vacuum_and_mop"), "Aspiration et lavage");
        assert_eq!(VacuumConfig::make_id("192.168.1.59"), "vac-192-168-1-59");
        let c = VacuumConfig { id: "vac-x".into(), name: "Robot".into(), room: None, link: Link::Miio { host: "10.0.0.2".into(), token: "00".repeat(16), port: None }, model: None };
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains(r#""family":"miio""#), "{json}");
        assert_eq!(serde_json::from_str::<VacuumConfig>(&json).unwrap(), c);
        let s = Status { activity: Activity::Charging, battery: Some(35), ..Default::default() };
        assert_eq!(s.summary(), "En charge · 35 %");
    }
}
