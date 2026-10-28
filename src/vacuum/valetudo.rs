//! Robots sous Valetudo (micrologiciel libre, https://valetudo.cloud) : API REST locale, en HTTP,
//! sans compte ni cloud. Valetudo remplace le micrologiciel d'origine de nombreux Dreame, Roborock
//! et Xiaomi (il faut l'installer soi-même, souvent en ouvrant le robot).
//!
//! Points utilisés (`/api/v2/robot`) : `GET /` (modèle), `GET /capabilities`, `GET /state/attributes`
//! (état, batterie, réglages, erreur), `PUT /capabilities/BasicControlCapability` (start, stop, pause,
//! home), `PUT /capabilities/LocateCapability`, `GET|PUT …ControlCapability/preset(s)` (puissance,
//! eau, mode), `GET /capabilities/ConsumableMonitoringCapability`,
//! `GET /capabilities/CurrentStatisticsCapability`. Les appels sont bloquants (à lancer hors du
//! moteur tokio).

use super::{fan_label, mode_label, water_label, Activity, Caps, Choice, Command, Consumable, ErrorKind, Status, VacError};
use crate::journal::Level;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::time::Duration;

const FAN: &str = "FanSpeedControlCapability";
const WATER: &str = "WaterUsageControlCapability";
const MODE: &str = "OperationModeControlCapability";
const BASIC: &str = "BasicControlCapability";
const LOCATE: &str = "LocateCapability";
const CONSUMABLES: &str = "ConsumableMonitoringCapability";
const STATS: &str = "CurrentStatisticsCapability";

/// Client d'un robot Valetudo.
pub struct Valetudo {
    base: String,
    auth: Option<String>,
    agent: ureq::Agent,
    /// Ce que le robot sait faire (lu une fois).
    pub capabilities: Vec<String>,
    pub caps: Caps,
}

/// Modèle annoncé par Valetudo.
#[derive(Clone, Debug, PartialEq)]
pub struct RobotInfo {
    pub manufacturer: String,
    pub model: String,
}

impl RobotInfo {
    pub fn label(&self) -> String {
        format!("{} {}", self.manufacturer, self.model).trim().to_string()
    }
}

impl Valetudo {
    pub fn new(host: &str, port: Option<u16>, user: Option<&str>, password: Option<&str>) -> Valetudo {
        let host = host.trim().trim_start_matches("http://").trim_end_matches('/');
        let base = match port {
            Some(p) if p != 80 => format!("http://{host}:{p}/api/v2/robot"),
            _ => format!("http://{host}/api/v2/robot"),
        };
        let auth = user.filter(|u| !u.is_empty()).map(|u| {
            let raw = format!("{u}:{}", password.unwrap_or(""));
            format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw))
        });
        let agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(6))).http_status_as_error(false).build().into();
        Valetudo { base, auth, agent, capabilities: Vec::new(), caps: Caps::default() }
    }

    fn check(status: u16, what: &str) -> Result<()> {
        match status {
            200..=299 => Ok(()),
            401 | 403 => bail!("identifiants refusés par Valetudo (authentification HTTP activée sur le robot ?)"),
            404 => bail!("{what} : inconnu de ce robot (HTTP 404)"),
            s => bail!("{what} : le robot répond HTTP {s}"),
        }
    }

    /// Adresse du robot (pour les messages).
    fn host(&self) -> &str {
        self.base.trim_start_matches("http://").split('/').next().unwrap_or("")
    }

    fn get(&self, path: &str) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let mut req = self.agent.get(&url);
        if let Some(a) = &self.auth {
            req = req.header("Authorization", a);
        }
        let mut resp = req.call().with_context(|| format!("pas de réponse Valetudo de {} (robot éteint, adresse fausse, ou micrologiciel d'origine sans Valetudo)", self.host()))?;
        Self::check(resp.status().as_u16(), path)?;
        resp.body_mut().read_json().with_context(|| format!("réponse illisible de {path}"))
    }

    fn put(&self, path: &str, body: Value) -> Result<()> {
        let url = format!("{}{path}", self.base);
        let mut req = self.agent.put(&url);
        if let Some(a) = &self.auth {
            req = req.header("Authorization", a);
        }
        let resp = req.send_json(&body).with_context(|| format!("pas de réponse Valetudo de {}", self.host()))?;
        Self::check(resp.status().as_u16(), path)
    }

    pub fn robot(&self) -> Result<RobotInfo> {
        let v = self.get("")?;
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        if v.get("modelName").is_none() && v.get("implementation").is_none() {
            bail!("ce n'est pas un robot Valetudo (réponse inattendue)");
        }
        Ok(RobotInfo { manufacturer: s("manufacturer"), model: s("modelName") })
    }

    fn has(&self, cap: &str) -> bool {
        self.capabilities.iter().any(|c| c == cap)
    }

    /// Première connexion : modèle, capacités et listes de réglages.
    pub fn connect(&mut self) -> Result<RobotInfo> {
        let info = self.robot()?;
        let caps: Value = self.get("/capabilities")?;
        self.capabilities = caps.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
        let presets = |cap: &str| -> Vec<String> {
            if !self.capabilities.iter().any(|c| c == cap) {
                return Vec::new();
            }
            self.get(&format!("/capabilities/{cap}/presets")).ok().and_then(|v| v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())).unwrap_or_default()
        };
        let (fans, waters, modes) = (presets(FAN), presets(WATER), presets(MODE));
        self.caps = caps_from(&self.capabilities, &fans, &waters, &modes);
        Ok(info)
    }

    /// État complet : attributs, statistiques du nettoyage en cours, pièces d'usure.
    pub fn read(&self) -> Result<Status> {
        let attrs = self.get("/state/attributes")?;
        let mut st = status_from(&attrs);
        if self.has(STATS) {
            if let Ok(v) = self.get(&format!("/capabilities/{STATS}")) {
                stats_into(&v, &mut st);
            }
        }
        if self.has(CONSUMABLES) {
            if let Ok(v) = self.get(&format!("/capabilities/{CONSUMABLES}")) {
                st.consumables = consumables_from(&v);
            }
        }
        Ok(st)
    }

    pub fn run(&self, cmd: &Command) -> Result<()> {
        let basic = |action: &str| -> Result<()> {
            if !self.has(BASIC) {
                bail!("ce robot ne se pilote pas par Valetudo (BasicControlCapability absente)");
            }
            self.put(&format!("/capabilities/{BASIC}"), json!({ "action": action }))
        };
        let preset = |cap: &str, v: &str| self.put(&format!("/capabilities/{cap}/preset"), json!({ "name": v }));
        match cmd {
            Command::Start => basic("start"),
            Command::Pause => basic("pause"),
            Command::Stop => basic("stop"),
            Command::Dock => basic("home"),
            Command::Locate => self.put(&format!("/capabilities/{LOCATE}"), json!({ "action": "locate" })),
            Command::Fan(v) => preset(FAN, v),
            Command::Water(v) => preset(WATER, v),
            Command::Mode(v) => preset(MODE, v),
            Command::Pattern(_) => Err(anyhow!("Valetudo ne propose pas de façon de nettoyer à choisir")),
        }
    }
}

/// Ce que le robot sait faire, d'après ses capacités et ses listes de réglages.
pub fn caps_from(capabilities: &[String], fans: &[String], waters: &[String], modes: &[String]) -> Caps {
    let has = |c: &str| capabilities.iter().any(|x| x == c);
    let basic = has(BASIC);
    Caps {
        pause: basic,
        stop: basic,
        dock: basic,
        locate: has(LOCATE),
        fans: fans.iter().map(|v| Choice::new(v.clone(), intensity_label(v, false))).collect(),
        waters: waters.iter().map(|v| Choice::new(v.clone(), intensity_label(v, true))).collect(),
        modes: modes.iter().map(|v| Choice::new(v.clone(), mode_label(v))).collect(),
        patterns: Vec::new(),
    }
}

/// Niveaux de Valetudo : off, min, low, medium, high, max, turbo, custom.
fn intensity_label(v: &str, water: bool) -> String {
    match v {
        "low" if !water => "Faible".into(),
        "medium" if !water => "Moyen".into(),
        "high" if !water => "Fort".into(),
        "turbo" => "Turbo".into(),
        "max" if water => "Maximum".into(),
        "off" if !water => "Sans aspiration".into(),
        _ if water => water_label(v),
        _ => fan_label(v),
    }
}

fn class(v: &Value) -> &str {
    v.get("__class").and_then(Value::as_str).unwrap_or("")
}

/// État d'après `/state/attributes`.
pub fn status_from(attrs: &Value) -> Status {
    let list = attrs.as_array().cloned().unwrap_or_default();
    let mut st = Status::default();
    let mut raw_status = "";
    let mut battery_flag = "";
    for a in &list {
        match class(a) {
            "StatusStateAttribute" => {
                raw_status = a.get("value").and_then(Value::as_str).unwrap_or("");
                if let Some(e) = a.get("error").filter(|e| e.is_object()) {
                    st.errors.push(error_from(e));
                }
            }
            "BatteryStateAttribute" => {
                st.battery = a.get("level").and_then(Value::as_f64).map(|l| l.round().clamp(0.0, 100.0) as u8);
                battery_flag = a.get("flag").and_then(Value::as_str).unwrap_or("");
            }
            "PresetSelectionStateAttribute" => {
                let value = a.get("value").and_then(Value::as_str).map(str::to_string);
                match a.get("type").and_then(Value::as_str) {
                    Some("fan_speed") => st.fan = value,
                    Some("water_grade") => st.water = value,
                    Some("operation_mode") => st.mode = value,
                    _ => {}
                }
            }
            _ => {}
        }
    }
    st.charging = battery_flag == "charging";
    st.activity = match raw_status {
        "cleaning" | "moving" | "manual_control" => Activity::Cleaning,
        "paused" => Activity::Paused,
        "returning" => Activity::Returning,
        "docked" if st.charging => Activity::Charging,
        "docked" => Activity::Docked,
        "idle" => Activity::Idle,
        "error" => Activity::Error,
        _ => Activity::Unknown,
    };
    if st.activity == Activity::Error && st.errors.is_empty() {
        st.errors.push(VacError::new("", ErrorKind::Other));
    }
    st
}

/// Erreur de Valetudo : `{severity: {kind, level}, subsystem, message, vendorErrorCode}`.
fn error_from(e: &Value) -> VacError {
    let message = e.get("message").and_then(Value::as_str).unwrap_or("");
    let code = match e.get("vendorErrorCode") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let subsystem = e.get("subsystem").and_then(Value::as_str).unwrap_or("");
    let mut kind = ErrorKind::guess(message);
    if kind == ErrorKind::Other {
        kind = match subsystem {
            "power" => ErrorKind::Charging,
            "dock" => ErrorKind::Dock,
            "attachments" => ErrorKind::BinMissing,
            "navigation" => ErrorKind::Stuck,
            "sensors" => ErrorKind::Sensor,
            _ => ErrorKind::Other,
        };
    }
    let mut err = if kind == ErrorKind::Other { VacError::other(code, message) } else { VacError::new(code, kind) };
    match e.get("severity").and_then(|s| s.get("level")).and_then(Value::as_str) {
        Some("catastrophic") => err.level = Level::Critical,
        Some("info") => err.level = Level::Info,
        _ => {}
    }
    err
}

/// Statistiques du nettoyage en cours : durée (s) et surface (cm²).
fn stats_into(v: &Value, st: &mut Status) {
    for p in v.as_array().into_iter().flatten() {
        let n = p.get("value").and_then(Value::as_f64);
        match p.get("type").and_then(Value::as_str) {
            Some("time") => st.clean_min = n.map(|s| (s / 60.0).round() as u32),
            Some("area") => st.area_m2 = n.map(|cm2| (cm2 / 10_000.0 * 10.0).round() / 10.0),
            _ => {}
        }
    }
}

fn consumables_from(v: &Value) -> Vec<Consumable> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let t = c.get("type").and_then(Value::as_str).unwrap_or("");
            let sub = c.get("subType").and_then(Value::as_str).unwrap_or("");
            let label = match (t, sub) {
                ("brush", "main") => "Brosse principale",
                ("brush", "side_right") => "Brosse latérale droite",
                ("brush", "side_left") => "Brosse latérale gauche",
                ("brush", _) => "Brosse latérale",
                ("filter", _) => "Filtre",
                ("sensor", _) => "Capteurs",
                ("mop", _) => "Serpillière",
                ("detergent", _) => "Détergent",
                ("dustbag", _) => "Sac à poussière",
                ("cleaning", "wheel") => "Roue",
                _ => "Pièce d'usure",
            };
            let remaining = c.get("remaining");
            let value = remaining.and_then(|r| r.get("value")).and_then(Value::as_f64);
            let unit = remaining.and_then(|r| r.get("unit")).and_then(Value::as_str).unwrap_or("");
            let (percent, hours) = match unit {
                "percent" => (value, None),
                "minutes" => (None, value.map(|m| m / 60.0)),
                _ => (None, None),
            };
            Consumable { label: label.into(), percent, hours }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_to_status() {
        let attrs = json!([
            { "__class": "StatusStateAttribute", "value": "docked", "flag": "none" },
            { "__class": "BatteryStateAttribute", "level": 87, "flag": "charging" },
            { "__class": "PresetSelectionStateAttribute", "type": "fan_speed", "value": "high" },
            { "__class": "PresetSelectionStateAttribute", "type": "water_grade", "value": "low" },
            { "__class": "PresetSelectionStateAttribute", "type": "operation_mode", "value": "vacuum_and_mop" },
            { "__class": "AttachmentStateAttribute", "type": "dustbin", "attached": true }
        ]);
        let st = status_from(&attrs);
        assert_eq!((st.activity, st.battery, st.charging), (Activity::Charging, Some(87), true));
        assert_eq!((st.fan.as_deref(), st.water.as_deref(), st.mode.as_deref()), (Some("high"), Some("low"), Some("vacuum_and_mop")));
        let err = json!([
            { "__class": "StatusStateAttribute", "value": "error", "flag": "none",
              "error": { "severity": { "kind": "transient", "level": "error" }, "subsystem": "motors", "message": "Main brush jammed", "vendorErrorCode": "5" } },
            { "__class": "BatteryStateAttribute", "level": 40, "flag": "discharging" }
        ]);
        let st = status_from(&err);
        assert_eq!(st.activity, Activity::Error);
        assert_eq!((st.errors[0].kind, st.errors[0].code.as_str(), st.errors[0].level), (ErrorKind::MainBrush, "5", Level::Warning));
        let odd = json!([{ "__class": "StatusStateAttribute", "value": "error",
              "error": { "severity": { "level": "catastrophic" }, "subsystem": "core", "message": "Something weird", "vendorErrorCode": 42 } }]);
        let e = &status_from(&odd).errors[0];
        assert_eq!((e.label.as_str(), e.level, e.code.as_str()), ("Erreur du robot : Something weird", Level::Critical, "42"));
    }

    #[test]
    fn capabilities_and_extras() {
        let caps = caps_from(&["BasicControlCapability".into(), "LocateCapability".into()], &["low".into(), "max".into()], &["low".into(), "high".into()], &["vacuum".into(), "mop".into()]);
        assert!(caps.pause && caps.dock && caps.locate);
        assert_eq!(caps.fans.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["Faible", "Maximum"]);
        assert_eq!(caps.waters.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["Faible", "Fort"]);
        assert_eq!(caps.modes[1].label, "Serpillière");
        let mut st = Status::default();
        stats_into(&json!([{ "type": "time", "value": 1830 }, { "type": "area", "value": 235000 }]), &mut st);
        assert_eq!((st.clean_min, st.area_m2), (Some(31), Some(23.5)));
        let c = consumables_from(&json!([{ "type": "brush", "subType": "main", "remaining": { "value": 6000, "unit": "minutes" } }, { "type": "filter", "subType": "none", "remaining": { "value": 40, "unit": "percent" } }]));
        assert_eq!((c[0].label.as_str(), c[0].hours), ("Brosse principale", Some(100.0)));
        assert_eq!((c[1].label.as_str(), c[1].percent), ("Filtre", Some(40.0)));
    }
}
