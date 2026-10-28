//! Robots aspirateurs Tuya / Smart Life (catégorie « sd ») : leurs DP traduits dans le modèle commun.
//!
//! Les DP sont reconnus par leur code (description importée de Smart Life) : `power_go` (lancer /
//! arrêter), `pause`, `switch_charge` (retour à la base), `mode` (façon de nettoyer, `chargego` =
//! retour), `status`, `electricity_left` (batterie), `suction`, `cistern` (eau), `seek` (bip),
//! `fault` (défauts), `clean_area`, `clean_time`, `edge_brush` / `roll_brush` / `filter` (usure).
//! Sans description, HomeSmart prend la numérotation standard de la catégorie (2 power_go, 3 mode,
//! 5 status, 6 electricity_left, 7‥9 usure, 13 seek, 14 suction, 16 clean_area, 17 clean_time,
//! 18 fault) quand les valeurs reçues lui ressemblent.

use super::{fan_label, mode_label, water_label, Activity, Caps, Choice, Command, Consumable, ErrorKind, Status, VacError};
use serde_json::{json, Map, Value};

/// États « status » connus (pour reconnaître un robot sans description).
pub const STATUSES: &[&str] = &[
    "standby", "smart_clean", "smart", "cleaning", "wall_clean", "wall_follow", "spot_clean", "mop_clean", "part_clean", "zone_clean",
    "pick_zone_clean", "random", "goto_pos", "pos_arrived", "pos_unarrive", "goto_charge", "docking", "chargego", "charging", "charge_done",
    "chargecompleted", "paused", "sleep", "fault",
];

/// Façons de nettoyer qu'on peut proposer (valeurs de « mode »), les autres étant des états ou des
/// commandes internes (retour à la base, veille, aller à un point…).
const PATTERNS: &[&str] = &["smart", "auto", "wall_follow", "edge", "spiral", "left_spiral", "right_spiral", "random", "mop", "partial_bow", "spot"];

/// Rôle des DP d'un robot Tuya.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Roles {
    pub power: Option<String>,
    pub go: Option<String>,
    pub pause: Option<String>,
    pub charge: Option<String>,
    pub mode: Option<String>,
    pub status: Option<String>,
    pub battery: Option<String>,
    pub suction: Option<String>,
    pub water: Option<String>,
    pub seek: Option<String>,
    pub fault: Option<String>,
    pub area: Option<String>,
    pub time: Option<String>,
    pub edge_brush: Option<String>,
    pub roll_brush: Option<String>,
    pub filter: Option<String>,
}

fn code_of<'a>(schema: &'a Map<String, Value>, dp: &str) -> Option<&'a str> {
    schema.get(dp).and_then(|s| s.get("code")).and_then(Value::as_str)
}

fn by_code(schema: &Map<String, Value>, codes: &[&str]) -> Option<String> {
    codes.iter().find_map(|c| schema.iter().find(|(_, s)| s.get("code").and_then(Value::as_str) == Some(c)).map(|(k, _)| k.clone()))
}

fn is_bool(dps: &Map<String, Value>, k: &str) -> bool {
    matches!(dps.get(k), Some(Value::Bool(_)))
}

fn is_num(dps: &Map<String, Value>, k: &str) -> bool {
    matches!(dps.get(k), Some(Value::Number(_)))
}

fn is_str(dps: &Map<String, Value>, k: &str) -> bool {
    matches!(dps.get(k), Some(Value::String(_)))
}

/// Les DP ressemblent-ils à ceux d'un robot aspirateur (sans description) ?
pub fn looks_like(dps: &Map<String, Value>) -> bool {
    matches!(dps.get("5"), Some(Value::String(s)) if STATUSES.contains(&s.as_str())) && is_bool(dps, "2")
}

/// Le schéma décrit-il un robot aspirateur ?
pub fn schema_is_vacuum(schema: &Map<String, Value>) -> bool {
    let has = |c: &str| by_code(schema, &[c]).is_some();
    has("power_go") || (has("status") && has("electricity_left")) || (has("suction") && has("seek"))
}

impl Roles {
    pub fn of(dps: &Map<String, Value>, schema: &Map<String, Value>) -> Roles {
        if !schema.is_empty() && schema_is_vacuum(schema) {
            return Roles {
                power: by_code(schema, &["power"]),
                go: by_code(schema, &["power_go", "switch_go"]),
                pause: by_code(schema, &["pause"]),
                charge: by_code(schema, &["switch_charge"]),
                mode: by_code(schema, &["mode", "work_mode"]),
                status: by_code(schema, &["status", "work_status"]),
                battery: by_code(schema, &["electricity_left", "battery_percentage", "residual_electricity"]),
                suction: by_code(schema, &["suction", "suction_level"]),
                water: by_code(schema, &["cistern", "water_level"]),
                seek: by_code(schema, &["seek"]),
                fault: by_code(schema, &["fault"]),
                area: by_code(schema, &["clean_area"]),
                time: by_code(schema, &["clean_time"]),
                edge_brush: by_code(schema, &["edge_brush", "edge_brush_life"]),
                roll_brush: by_code(schema, &["roll_brush", "roll_brush_life"]),
                filter: by_code(schema, &["filter", "filter_life"]),
            };
        }
        // Numérotation standard de la catégorie, seulement là où la valeur a le bon type.
        let pick = |k: &str, ok: bool| ok.then(|| k.to_string());
        Roles {
            power: pick("1", is_bool(dps, "1")),
            go: pick("2", is_bool(dps, "2")),
            pause: None,
            charge: None,
            mode: pick("3", is_str(dps, "3")),
            status: pick("5", is_str(dps, "5")),
            battery: pick("6", dps.get("6").and_then(Value::as_u64).is_some_and(|b| b <= 100)),
            suction: pick("14", is_str(dps, "14")),
            water: None,
            seek: pick("13", is_bool(dps, "13")),
            fault: pick("18", is_num(dps, "18")),
            area: pick("16", is_num(dps, "16")),
            time: pick("17", is_num(dps, "17")),
            edge_brush: pick("7", is_num(dps, "7")),
            roll_brush: pick("8", is_num(dps, "8")),
            filter: pick("9", is_num(dps, "9")),
        }
    }
}

fn values<'a>(schema: &'a Map<String, Value>, dp: &str) -> Option<&'a Value> {
    schema.get(dp).and_then(|s| s.get("values"))
}

fn range(schema: &Map<String, Value>, dp: &str) -> Vec<String> {
    values(schema, dp).and_then(|v| v.get("range")).and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

/// Nombre à l'échelle du schéma.
fn scaled(dps: &Map<String, Value>, schema: &Map<String, Value>, dp: &str) -> Option<f64> {
    let raw = dps.get(dp).and_then(Value::as_f64)?;
    let scale = values(schema, dp).and_then(|v| v.get("scale")).and_then(Value::as_i64).unwrap_or(0).clamp(0, 6);
    Some(raw / 10f64.powi(scale as i32))
}

fn activity_of(status: &str) -> Activity {
    match status {
        "smart_clean" | "smart" | "cleaning" | "wall_clean" | "wall_follow" | "spot_clean" | "mop_clean" | "part_clean" | "zone_clean"
        | "pick_zone_clean" | "random" | "goto_pos" | "pos_arrived" | "pos_unarrive" | "spiral" | "edge" | "spot" | "auto" | "sweep" => Activity::Cleaning,
        "goto_charge" | "docking" | "chargego" | "recharge" => Activity::Returning,
        "charging" => Activity::Charging,
        "charge_done" | "chargecompleted" | "completed" | "full" => Activity::Docked,
        "paused" | "pause" => Activity::Paused,
        "standby" | "sleep" | "idle" | "sleeping" => Activity::Idle,
        "fault" | "error" => Activity::Error,
        _ => Activity::Unknown,
    }
}

/// Défauts signalés (bitmap « fault »), avec les noms du schéma.
fn errors(dps: &Map<String, Value>, schema: &Map<String, Value>, r: &Roles) -> Vec<VacError> {
    let Some(dp) = &r.fault else { return Vec::new() };
    let bits = dps.get(dp).and_then(Value::as_u64).unwrap_or(0);
    let names: Vec<String> =
        values(schema, dp).and_then(|v| v.get("label")).and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
    (0..32)
        .filter(|i| bits & (1 << i) != 0)
        .map(|i| match names.get(i as usize) {
            Some(n) => match ErrorKind::guess(n) {
                ErrorKind::Other => VacError::other(n.clone(), &n.replace('_', " ")),
                k => VacError::new(n.clone(), k),
            },
            None => VacError::new(format!("bit {i}"), ErrorKind::Other),
        })
        .collect()
}

fn consumable(dps: &Map<String, Value>, schema: &Map<String, Value>, dp: &Option<String>, label: &str) -> Option<Consumable> {
    let dp = dp.as_ref()?;
    let v = scaled(dps, schema, dp)?;
    let vals = values(schema, dp);
    let unit = vals.and_then(|x| x.get("unit")).and_then(Value::as_str).unwrap_or("");
    let max = vals.and_then(|x| x.get("max")).and_then(Value::as_f64).filter(|m| *m > 0.0);
    let (percent, hours) = match unit {
        "%" => (Some(v.clamp(0.0, 100.0)), None),
        "min" | "minute" | "minutes" => (max.map(|m| (v / m * 100.0).clamp(0.0, 100.0)), Some(v / 60.0)),
        "h" | "hour" | "hours" => (max.map(|m| (v / m * 100.0).clamp(0.0, 100.0)), Some(v)),
        // Sans unité : une valeur de 0 à 100 est un pourcentage.
        _ if v <= 100.0 && max.is_none_or(|m| m <= 100.0) => (Some(v), None),
        _ => (max.map(|m| (v / m * 100.0).clamp(0.0, 100.0)), None),
    };
    Some(Consumable { label: label.into(), percent, hours })
}

/// État du robot d'après ses DP.
pub fn status(dps: &Map<String, Value>, schema: &Map<String, Value>) -> Status {
    let r = Roles::of(dps, schema);
    let get_str = |dp: &Option<String>| dp.as_ref().and_then(|d| dps.get(d)).and_then(Value::as_str).map(str::to_string);
    let get_bool = |dp: &Option<String>| dp.as_ref().and_then(|d| dps.get(d)).and_then(Value::as_bool);
    let mut activity = match get_str(&r.status) {
        Some(s) => activity_of(&s),
        None => match get_bool(&r.go) {
            Some(true) => Activity::Cleaning,
            Some(false) => Activity::Idle,
            None => Activity::Unknown,
        },
    };
    if get_bool(&r.pause) == Some(true) && !activity.at_dock() && activity != Activity::Error {
        activity = Activity::Paused;
    }
    let errs = errors(dps, schema, &r);
    if activity == Activity::Unknown && !errs.is_empty() {
        activity = Activity::Error;
    }
    let mut errs = errs;
    if activity == Activity::Error && errs.is_empty() {
        errs.push(VacError::new("", ErrorKind::Other));
    }
    let battery = r.battery.as_ref().and_then(|d| scaled(dps, schema, d)).map(|b| b.round().clamp(0.0, 100.0) as u8);
    let consumables =
        [(&r.roll_brush, "Brosse principale"), (&r.edge_brush, "Brosse latérale"), (&r.filter, "Filtre")].into_iter().filter_map(|(dp, l)| consumable(dps, schema, dp, l)).collect();
    Status {
        activity,
        battery,
        charging: activity == Activity::Charging,
        fan: get_str(&r.suction),
        mode: None,
        water: get_str(&r.water),
        area_m2: r.area.as_ref().and_then(|d| scaled(dps, schema, d)),
        clean_min: r.time.as_ref().and_then(|d| scaled(dps, schema, d)).map(|m| m.round().max(0.0) as u32),
        errors: errs,
        consumables,
    }
}

/// Ce que le robot sait faire, d'après ses DP et sa description.
pub fn caps(dps: &Map<String, Value>, schema: &Map<String, Value>) -> Caps {
    let r = Roles::of(dps, schema);
    let choices = |dp: &Option<String>, fallback: &[&str], label: fn(&str) -> String| -> Vec<Choice> {
        let Some(dp) = dp else { return Vec::new() };
        let mut list = range(schema, dp);
        if list.is_empty() {
            // Sans description : la liste standard, si la valeur actuelle en fait partie.
            let cur = dps.get(dp).and_then(Value::as_str).unwrap_or("");
            if fallback.contains(&cur) {
                list = fallback.iter().map(|s| s.to_string()).collect();
            }
        }
        list.iter().map(|v| Choice::new(v.clone(), label(v))).collect()
    };
    let modes = r.mode.as_ref().map(|d| range(schema, d)).unwrap_or_default();
    Caps {
        pause: r.pause.is_some() || r.go.is_some(),
        stop: r.go.is_some(),
        dock: r.charge.is_some() || modes.iter().any(|m| m == "chargego") || (r.mode.is_some() && schema.is_empty()),
        locate: r.seek.is_some(),
        fans: choices(&r.suction, &["gentle", "normal", "strong", "max"], fan_label),
        modes: Vec::new(),
        waters: choices(&r.water, &["closed", "low", "middle", "high"], water_label),
        patterns: modes.iter().filter(|m| PATTERNS.contains(&m.as_str())).map(|m| Choice::new(m.clone(), mode_label(m))).collect(),
    }
}

/// DP à écrire pour une commande.
pub fn command(cmd: &Command, dps: &Map<String, Value>, schema: &Map<String, Value>) -> Result<Map<String, Value>, String> {
    let r = Roles::of(dps, schema);
    let mut m = Map::new();
    let missing = || "ce robot n'annonce pas ce réglage (description des DP absente ?)".to_string();
    match cmd {
        Command::Start => match (&r.go, &r.mode) {
            (Some(go), _) => {
                // Repartir d'une pause : la pause est levée en même temps.
                if let Some(p) = r.pause.as_ref().filter(|p| dps.get(*p) == Some(&json!(true))) {
                    m.insert(p.clone(), json!(false));
                }
                m.insert(go.clone(), json!(true));
            }
            (None, Some(mode)) => {
                m.insert(mode.clone(), json!("smart"));
            }
            _ => return Err(missing()),
        },
        Command::Pause => match (&r.pause, &r.go) {
            (Some(p), _) => {
                m.insert(p.clone(), json!(true));
            }
            (None, Some(go)) => {
                m.insert(go.clone(), json!(false));
            }
            _ => return Err(missing()),
        },
        Command::Stop => {
            m.insert(r.go.clone().ok_or_else(missing)?, json!(false));
        }
        Command::Dock => match (&r.charge, &r.mode) {
            (Some(c), _) => {
                m.insert(c.clone(), json!(true));
            }
            (None, Some(mode)) => {
                m.insert(mode.clone(), json!("chargego"));
            }
            _ => return Err(missing()),
        },
        Command::Locate => {
            m.insert(r.seek.clone().ok_or_else(missing)?, json!(true));
        }
        Command::Fan(v) => {
            m.insert(r.suction.clone().ok_or_else(missing)?, json!(v));
        }
        Command::Water(v) => {
            m.insert(r.water.clone().ok_or_else(missing)?, json!(v));
        }
        Command::Pattern(v) => {
            m.insert(r.mode.clone().ok_or_else(missing)?, json!(v));
            if let Some(go) = &r.go {
                m.insert(go.clone(), json!(true));
            }
        }
        Command::Mode(_) => return Err("ce robot n'a pas de mode de nettoyage à régler (choisis une façon de nettoyer)".into()),
    }
    Ok(m)
}

/// Description française d'un DP écrit ou changé, pour le journal (`None` : DP sans rôle connu).
pub fn describe(dp: &str, v: &Value, dps: &Map<String, Value>, schema: &Map<String, Value>) -> Option<String> {
    let r = Roles::of(dps, schema);
    let is = |role: &Option<String>| role.as_deref() == Some(dp);
    let b = v.as_bool();
    let s = v.as_str().unwrap_or("");
    Some(if is(&r.go) {
        if b == Some(true) { "Nettoyage lancé" } else { "Nettoyage arrêté" }.to_string()
    } else if is(&r.pause) {
        if b == Some(true) { "Nettoyage mis en pause" } else { "Nettoyage repris" }.to_string()
    } else if is(&r.charge) {
        if b == Some(true) { "Retour à la base demandé" } else { "Retour à la base annulé" }.to_string()
    } else if is(&r.mode) {
        match s {
            "chargego" => "Retour à la base demandé".to_string(),
            "standby" => "Robot mis en veille".to_string(),
            _ => format!("Nettoyage lancé ({})", mode_label(s).to_lowercase()),
        }
    } else if is(&r.seek) {
        if b == Some(true) { "Recherche du robot (signal sonore)" } else { "Fin du signal sonore" }.to_string()
    } else if is(&r.suction) {
        format!("Puissance d'aspiration : {}", fan_label(s))
    } else if is(&r.water) {
        format!("Débit d'eau : {}", water_label(s))
    } else if is(&r.power) {
        if b == Some(true) { "Robot allumé" } else { "Robot éteint" }.to_string()
    } else {
        return None;
    })
}

/// DP qui suivent l'état du robot (marche, retour, batterie, défauts, usure…) : ce ne sont pas des
/// actions à inscrire une à une ; le hub inscrit les changements d'activité eux-mêmes. Seuls les
/// réglages (puissance, eau) faits ailleurs restent des actions.
pub fn is_state_dp(dp: &str, dps: &Map<String, Value>, schema: &Map<String, Value>) -> bool {
    let r = Roles::of(dps, schema);
    [&r.go, &r.pause, &r.charge, &r.mode, &r.status, &r.battery, &r.seek, &r.fault, &r.area, &r.time, &r.edge_brush, &r.roll_brush, &r.filter]
        .iter()
        .any(|role| role.as_deref() == Some(dp))
        || code_of(schema, dp).is_some_and(|c| c == "clean_record" || c.starts_with("path_data") || c == "command_trans" || c == "request")
}

/// Description standard d'un robot Tuya (simulateur, essais) : codes et plages de la catégorie « sd ».
pub fn standard_schema() -> Value {
    json!({
        "1": { "code": "power", "type": "Boolean", "values": {} },
        "2": { "code": "power_go", "type": "Boolean", "values": {} },
        "3": { "code": "mode", "type": "Enum", "values": { "range": ["standby", "smart", "wall_follow", "spiral", "chargego"] } },
        "4": { "code": "direction_control", "type": "Enum", "values": { "range": ["forward", "backward", "turn_left", "turn_right", "stop"] } },
        "5": { "code": "status", "type": "Enum", "values": { "range": ["standby", "smart_clean", "wall_clean", "spot_clean", "goto_charge", "charging", "charge_done", "paused", "sleep", "fault"] } },
        "6": { "code": "electricity_left", "type": "Integer", "values": { "unit": "%", "min": 0, "max": 100, "scale": 0, "step": 1 } },
        "7": { "code": "edge_brush", "type": "Integer", "values": { "unit": "min", "min": 0, "max": 9000, "scale": 0, "step": 1 } },
        "8": { "code": "roll_brush", "type": "Integer", "values": { "unit": "min", "min": 0, "max": 18000, "scale": 0, "step": 1 } },
        "9": { "code": "filter", "type": "Integer", "values": { "unit": "min", "min": 0, "max": 9000, "scale": 0, "step": 1 } },
        "13": { "code": "seek", "type": "Boolean", "values": {} },
        "14": { "code": "suction", "type": "Enum", "values": { "range": ["gentle", "normal", "strong", "max"] } },
        "15": { "code": "cistern", "type": "Enum", "values": { "range": ["closed", "low", "middle", "high"] } },
        "16": { "code": "clean_area", "type": "Integer", "values": { "unit": "㎡", "min": 0, "max": 9999, "scale": 0, "step": 1 } },
        "17": { "code": "clean_time", "type": "Integer", "values": { "unit": "min", "min": 0, "max": 9999, "scale": 0, "step": 1 } },
        "18": { "code": "fault", "type": "Bitmap", "values": { "label": ["edge_sweep", "middle_sweep", "left_wheel", "right_wheel", "garbage_box", "land_check", "collision", "wheel_suspend", "low_power", "box_full"] } },
        "25": { "code": "pause", "type": "Boolean", "values": {} },
        "26": { "code": "switch_charge", "type": "Boolean", "values": {} }
    })
}

/// DP d'un robot Tuya sur sa base, chargé (simulateur, essais).
pub fn standard_dps() -> Value {
    json!({
        "1": true, "2": false, "3": "standby", "4": "stop", "5": "charge_done", "6": 100, "7": 7800, "8": 15600, "9": 2100,
        "13": false, "14": "normal", "15": "closed", "16": 0, "17": 0, "18": 0, "25": false, "26": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn standard_robot_with_description() {
        let schema = m(standard_schema());
        let mut dps = m(standard_dps());
        let st = status(&dps, &schema);
        assert_eq!(st.activity, Activity::Docked);
        assert_eq!(st.battery, Some(100));
        assert_eq!(st.fan.as_deref(), Some("normal"));
        assert_eq!(st.consumables.len(), 3);
        assert_eq!(st.consumables[0].label, "Brosse principale");
        assert!((st.consumables[0].percent.unwrap() - 86.666).abs() < 0.01);
        let caps = caps(&dps, &schema);
        assert!(caps.pause && caps.stop && caps.dock && caps.locate);
        assert_eq!(caps.fans.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["Silencieux", "Standard", "Fort", "Maximum"]);
        assert_eq!(caps.patterns.iter().map(|c| c.value.as_str()).collect::<Vec<_>>(), ["smart", "wall_follow", "spiral"]);
        // Commandes → DP.
        assert_eq!(command(&Command::Start, &dps, &schema).unwrap(), m(json!({ "2": true })));
        assert_eq!(command(&Command::Pause, &dps, &schema).unwrap(), m(json!({ "25": true })));
        assert_eq!(command(&Command::Dock, &dps, &schema).unwrap(), m(json!({ "26": true })));
        assert_eq!(command(&Command::Locate, &dps, &schema).unwrap(), m(json!({ "13": true })));
        assert_eq!(command(&Command::Fan("max".into()), &dps, &schema).unwrap(), m(json!({ "14": "max" })));
        assert_eq!(command(&Command::Pattern("spiral".into()), &dps, &schema).unwrap(), m(json!({ "3": "spiral", "2": true })));
        assert!(command(&Command::Mode("mop".into()), &dps, &schema).is_err());
        // En pause : reprendre lève la pause.
        dps.insert("25".into(), json!(true));
        dps.insert("5".into(), json!("smart_clean"));
        assert_eq!(status(&dps, &schema).activity, Activity::Paused);
        assert_eq!(command(&Command::Start, &dps, &schema).unwrap(), m(json!({ "25": false, "2": true })));
        // Défauts : brosse principale et bac plein.
        dps.insert("18".into(), json!((1 << 1) | (1 << 9)));
        let st = status(&dps, &schema);
        assert_eq!(st.errors.iter().map(|e| e.kind).collect::<Vec<_>>(), [ErrorKind::MainBrush, ErrorKind::BinFull]);
        assert_eq!(st.errors[0].code, "middle_sweep");
        // Journal.
        assert_eq!(describe("2", &json!(true), &dps, &schema).as_deref(), Some("Nettoyage lancé"));
        assert_eq!(describe("26", &json!(true), &dps, &schema).as_deref(), Some("Retour à la base demandé"));
        assert_eq!(describe("14", &json!("strong"), &dps, &schema).as_deref(), Some("Puissance d'aspiration : Fort"));
        assert_eq!(describe("3", &json!("wall_follow"), &dps, &schema).as_deref(), Some("Nettoyage lancé (bordures)"));
        assert!(describe("4", &json!("stop"), &dps, &schema).is_none());
        assert!(is_state_dp("5", &dps, &schema) && is_state_dp("2", &dps, &schema) && !is_state_dp("14", &dps, &schema));
    }

    #[test]
    fn standard_robot_without_description() {
        let dps = m(json!({ "1": true, "2": true, "3": "smart", "5": "smart_clean", "6": 64, "13": false, "14": "strong" }));
        assert!(looks_like(&dps));
        let st = status(&dps, &Map::new());
        assert_eq!((st.activity, st.battery, st.fan.as_deref()), (Activity::Cleaning, Some(64), Some("strong")));
        let caps = caps(&dps, &Map::new());
        assert_eq!(caps.fans.len(), 4, "liste standard reconnue d'après la valeur actuelle");
        assert!(caps.dock, "retour par le mode « chargego »");
        assert_eq!(command(&Command::Dock, &dps, &Map::new()).unwrap(), m(json!({ "3": "chargego" })));
        assert!(!looks_like(&m(json!({ "1": true, "19": 120 }))), "une prise n'est pas un robot");
    }

    #[test]
    fn statuses_map_to_activities() {
        for (s, a) in [("goto_charge", Activity::Returning), ("charging", Activity::Charging), ("charge_done", Activity::Docked), ("sleep", Activity::Idle), ("wall_clean", Activity::Cleaning)] {
            assert_eq!(activity_of(s), a, "{s}");
        }
        // Robot en défaut sans bit connu : une erreur générique.
        let schema = m(standard_schema());
        let st = status(&m(json!({ "2": false, "5": "fault", "18": 0 })), &schema);
        assert_eq!((st.activity, st.errors.len()), (Activity::Error, 1));
    }
}
