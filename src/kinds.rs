//! Reconnaissance du type d'appareil à partir de ses DP, et formats Tuya usuels.
//!
//! Conventions Tuya les plus répandues :
//! - ampoule « v2 » : 20 marche, 21 mode, 22 luminosité 10‥1000, 23 teinte du blanc 0‥1000,
//!   24 couleur « hhhhssssvvvv » (teinte 0‥360, saturation et valeur 0‥1000) ;
//! - ampoule « v1 » : 1 marche, 2 mode, 3 luminosité 25‥255, 4 teinte du blanc 0‥255,
//!   5 couleur « rrggbb0hhhssvv » ;
//! - prise : 1 marche, 18 courant (mA), 19 puissance (dW), 20 tension (dV) ;
//! - volet : 1 commande open/stop/close, 2 position voulue (%), 3 position actuelle (%).

use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    LightV2,
    LightV1,
    Cover,
    Plug { metering: bool },
    Switch { gangs: Vec<String> },
    /// Aspirateur robot (catégorie Tuya « sd ») : voir `crate::vacuum::tuya`.
    Vacuum,
    Generic,
}

fn is_bool(dps: &Map<String, Value>, k: &str) -> bool {
    matches!(dps.get(k), Some(Value::Bool(_)))
}

fn is_num(dps: &Map<String, Value>, k: &str) -> bool {
    matches!(dps.get(k), Some(Value::Number(_)))
}

fn str_in(dps: &Map<String, Value>, k: &str, set: &[&str]) -> bool {
    matches!(dps.get(k), Some(Value::String(s)) if set.contains(&s.as_str()))
}

/// DP rencontrés sur les prises : marche, minuterie, mesures, étalonnage, voyant, état au démarrage…
const PLUG_DPS: &[u32] = &[1, 7, 9, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 38, 39, 40, 41, 42, 43, 44, 46, 47, 51];
const COVER_CMDS: &[&str] = &["open", "close", "stop", "continue", "on", "off"];
const LIGHT_MODES: &[&str] = &["white", "colour", "scene", "music", "scene_1", "scene_2", "scene_3", "scene_4"];

/// Sorties d'une multiprise / d'un interrupteur multiple (DP booléens 1‥9, + USB éventuels).
fn gangs(dps: &Map<String, Value>) -> Vec<String> {
    let mut g: Vec<String> = dps
        .iter()
        .filter(|(k, v)| v.is_boolean() && k.parse::<u32>().is_ok_and(|n| (1..=9).contains(&n)))
        .map(|(k, _)| k.clone())
        .collect();
    g.sort_by_key(|k| k.parse::<u32>().unwrap_or(0));
    g
}

pub fn detect(hint: Option<&str>, dps: &Map<String, Value>) -> Kind {
    let light = || {
        if is_bool(dps, "20") && (dps.contains_key("22") || dps.contains_key("23") || dps.contains_key("24")) {
            Some(Kind::LightV2)
        } else if is_bool(dps, "1") && str_in(dps, "2", LIGHT_MODES) && is_num(dps, "3") {
            Some(Kind::LightV1)
        } else {
            None
        }
    };
    let cover = || str_in(dps, "1", COVER_CMDS).then_some(Kind::Cover);
    let plug = || {
        let plug_like = dps.keys().all(|k| k.parse::<u32>().is_ok_and(|n| PLUG_DPS.contains(&n)));
        (is_bool(dps, "1") && (plug_like || hint.is_some())).then(|| Kind::Plug { metering: is_num(dps, "19") })
    };
    let switch = || {
        let g = gangs(dps);
        (g.len() >= 2).then_some(Kind::Switch { gangs: g })
    };
    let vacuum = || crate::vacuum::tuya::looks_like(dps).then_some(Kind::Vacuum);
    let found = match hint {
        Some("light") => light(),
        Some("cover") => cover(),
        Some("plug") => switch().or_else(plug),
        Some("switch") => switch().or_else(plug),
        Some("vacuum") => Some(Kind::Vacuum),
        Some("generic") => Some(Kind::Generic),
        _ => vacuum().or_else(light).or_else(cover).or_else(switch).or_else(plug),
    };
    found.unwrap_or(Kind::Generic)
}

impl Kind {
    pub fn label(&self) -> &'static str {
        match self {
            Kind::LightV2 | Kind::LightV1 => "Lumière",
            Kind::Cover => "Volet",
            Kind::Plug { .. } => "Prise",
            Kind::Switch { .. } => "Multiprise",
            Kind::Vacuum => "Aspirateur",
            Kind::Generic => "Appareil",
        }
    }

    /// DP marche/arrêt principal.
    pub fn switch_dp(&self, dps: &Map<String, Value>) -> Option<String> {
        match self {
            Kind::LightV2 => Some("20".into()),
            // Un robot ne s'allume pas comme une prise : on le lance ou on le renvoie à sa base.
            Kind::Cover | Kind::Vacuum => None,
            _ => crate::protocol::codec::main_switch_dp(dps),
        }
    }
}

/// Couleur en TSV normalisée (teinte 0‥1, saturation 0‥1, valeur 0‥1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hsv {
    pub h: f32,
    pub s: f32,
    pub v: f32,
}

pub fn parse_colour_v2(s: &str) -> Option<Hsv> {
    if s.len() < 12 || !s.is_ascii() {
        return None;
    }
    let h = u16::from_str_radix(&s[0..4], 16).ok()?;
    let sat = u16::from_str_radix(&s[4..8], 16).ok()?;
    let v = u16::from_str_radix(&s[8..12], 16).ok()?;
    Some(Hsv { h: (h % 360) as f32 / 360.0, s: sat.min(1000) as f32 / 1000.0, v: v.min(1000) as f32 / 1000.0 })
}

pub fn encode_colour_v2(c: Hsv) -> String {
    let h = ((c.h.rem_euclid(1.0)) * 360.0).round() as u16 % 360;
    let s = (c.s.clamp(0.0, 1.0) * 1000.0).round() as u16;
    let v = (c.v.clamp(0.01, 1.0) * 1000.0).round().max(10.0) as u16;
    format!("{h:04x}{s:04x}{v:04x}")
}

pub fn parse_colour_v1(s: &str) -> Option<Hsv> {
    // rrggbb0hhhssvv : on lit la partie TSV.
    if s.len() < 14 || !s.is_ascii() {
        return None;
    }
    let h = u16::from_str_radix(&s[7..10], 16).ok()?;
    let sat = u8::from_str_radix(&s[10..12], 16).ok()?;
    let v = u8::from_str_radix(&s[12..14], 16).ok()?;
    Some(Hsv { h: (h % 360) as f32 / 360.0, s: sat as f32 / 255.0, v: v as f32 / 255.0 })
}

pub fn encode_colour_v1(c: Hsv) -> String {
    let (r, g, b) = hsv_to_rgb(c);
    let h = ((c.h.rem_euclid(1.0)) * 360.0).round() as u16 % 360;
    let s = (c.s.clamp(0.0, 1.0) * 255.0).round() as u8;
    let v = (c.v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("{r:02x}{g:02x}{b:02x}0{h:03x}{s:02x}{v:02x}")
}

pub fn hsv_to_rgb(c: Hsv) -> (u8, u8, u8) {
    let h = c.h.rem_euclid(1.0) * 6.0;
    let (s, v) = (c.s.clamp(0.0, 1.0), c.v.clamp(0.0, 1.0));
    let i = h.floor();
    let f = h - i;
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - s * f), v * (1.0 - s * (1.0 - f)));
    let (r, g, b) = match i as i32 % 6 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    ((r * 255.0).round() as u8, (g * 255.0).round() as u8, (b * 255.0).round() as u8)
}

/// Mesures d'une prise : (puissance W, courant A, tension V).
pub fn metering(dps: &Map<String, Value>) -> Option<(f64, f64, f64)> {
    let n = |k: &str| dps.get(k).and_then(Value::as_f64);
    let p = n("19")?;
    Some((p / 10.0, n("18").unwrap_or(0.0) / 1000.0, n("20").unwrap_or(0.0) / 10.0))
}

/// Libellé français d'un code DP Tuya.
pub fn label_for_code(code: &str) -> String {
    let known = match code {
        "switch" | "switch_1" | "switch_led" => "Marche",
        "switch_2" => "Sortie 2",
        "switch_3" => "Sortie 3",
        "switch_4" => "Sortie 4",
        "switch_5" => "Sortie 5",
        "switch_6" => "Sortie 6",
        "switch_usb1" => "USB",
        "switch_usb2" => "USB 2",
        "temp_set" => "Consigne",
        "temp_current" => "Température",
        "humidity_current" | "va_humidity" => "Humidité",
        "va_temperature" => "Température",
        "battery_percentage" => "Batterie",
        "mode" | "work_mode" => "Mode",
        "bright_value" | "bright_value_v2" => "Luminosité",
        "temp_value" | "temp_value_v2" => "Teinte du blanc",
        "colour_data" | "colour_data_v2" => "Couleur",
        "countdown" | "countdown_1" => "Minuterie (s)",
        "child_lock" => "Sécurité enfant",
        "relay_status" => "Après une coupure de courant",
        "light_mode" => "Voyant lumineux",
        "cur_current" => "Courant",
        "cur_power" => "Puissance",
        "cur_voltage" => "Tension",
        "add_ele" => "Énergie ajoutée",
        "control" => "Commande",
        "percent_control" => "Position voulue",
        "percent_state" => "Position",
        "fan_speed" | "fan_speed_enum" => "Vitesse",
        "eco" => "Éco",
        "window_check" => "Fenêtre ouverte",
        "fault" => "Défauts détectés",
        "voltage_coe" => "Étalonnage tension",
        "electric_coe" => "Étalonnage courant",
        "power_coe" => "Étalonnage puissance",
        "electricity_coe" => "Étalonnage énergie",
        "test_bit" => "Bit de test usine",
        "overcharge_switch" => "Protection surcharge",
        "cycle_time" => "Programmes internes",
        "random_time" => "Simulation de présence",
        "switch_inching" => "Mode impulsion",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    let s = code.replace('_', " ");
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Libellé français d'une valeur d'énumération.
pub fn enum_label(code: &str, value: &str) -> String {
    let l = match (code, value) {
        ("relay_status", "power_off" | "off" | "0") => "Rester éteinte",
        ("relay_status", "power_on" | "on" | "1") => "Se rallumer",
        ("relay_status", "last" | "memory" | "2") => "Reprendre l'état précédent",
        ("light_mode", "relay") => "Suit la prise",
        ("light_mode", "pos") => "Inversé (repère la nuit)",
        ("light_mode", "none") => "Toujours éteint",
        (_, "auto") => "Automatique",
        (_, "manual") => "Manuel",
        (_, "eco") => "Éco",
        (_, "white") => "Blanc",
        (_, "colour") => "Couleur",
        (_, "scene") => "Scène",
        (_, "open") => "Ouvrir",
        (_, "close") => "Fermer",
        (_, "stop") => "Stop",
        _ => return value.replace('_', " "),
    };
    l.to_string()
}

/// Paramètres d'usine qu'il ne faut jamais modifier (étalonnage des mesures).
pub fn is_protected_code(code: &str) -> bool {
    code.ends_with("_coe") || code.ends_with("_calibration") || code.contains("calibrat") || matches!(code, "test_bit")
}

/// Prise à mesure d'énergie : courant, puissance et tension présents (DP 18, 19, 20).
fn is_metering_plug(dps: &Map<String, Value>) -> bool {
    ["18", "19", "20"].iter().all(|k| is_num(dps, k))
}

/// DP qu'on ne doit jamais écrire : réglages d'usine (étalonnage de la tension, du courant,
/// de la puissance et de l'énergie, bit de test). Reconnus par leur code dans le schéma ;
/// pour une prise à mesure sans schéma, les DP 21 à 25 sont protégés par prudence.
pub fn protected_dps(dps: &Map<String, Value>, schema: &Map<String, Value>) -> Vec<String> {
    let mut out: Vec<String> =
        schema.iter().filter(|(_, s)| s.get("code").and_then(Value::as_str).is_some_and(is_protected_code)).map(|(k, _)| k.clone()).collect();
    if is_metering_plug(dps) {
        for k in 21..=25 {
            let k = k.to_string();
            if code_of(schema, &k).is_none() && !out.contains(&k) {
                out.push(k);
            }
        }
    }
    out
}

/// Valeurs possibles d'une énumération d'après le schéma.
pub fn enum_range(schema: &Map<String, Value>, dp: &str) -> Vec<String> {
    schema
        .get(dp)
        .and_then(|s| s.get("values"))
        .and_then(|v| v.get("range"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn cloud_to_local(v: &str) -> &str {
    match v {
        "power_off" => "off",
        "power_on" => "on",
        "last" => "memory",
        other => other,
    }
}

/// Valeurs réellement acceptées par l'appareil pour une énumération. Certains firmwares
/// n'utilisent pas en local les noms du cloud (« Après une coupure » : off / on / memory au
/// lieu de power_off / power_on / last). `None` : on ne sait pas lesquelles (ne pas écrire).
pub fn local_enum_range(current: Option<&str>, range: &[String]) -> Option<Vec<String>> {
    let Some(cur) = current else { return Some(range.to_vec()) };
    if range.iter().any(|r| r == cur) {
        return Some(range.to_vec());
    }
    let mapped: Vec<String> = range.iter().map(|v| cloud_to_local(v).to_string()).collect();
    mapped.iter().any(|r| r == cur).then_some(mapped)
}

/// Garde-fous d'écriture : réglages d'usine intouchables, énumérations limitées aux valeurs
/// que l'appareil connaît, nombres limités à la plage du schéma.
pub fn check_writable(req: &Map<String, Value>, dps: &Map<String, Value>, schema: &Map<String, Value>) -> Result<(), String> {
    let protected = protected_dps(dps, schema);
    let label = |dp: &str| code_of(schema, dp).map(label_for_code).unwrap_or_else(|| format!("DP {dp}"));
    if let Some(dp) = req.keys().find(|k| protected.contains(k)) {
        return Err(format!("« {} » (DP {dp}) est un réglage d'usine protégé : écriture refusée", label(dp)));
    }
    for (dp, v) in req {
        let range = enum_range(schema, dp);
        if !range.is_empty() {
            let Some(sv) = v.as_str() else { return Err(format!("« {} » attend un texte", label(dp))) };
            match local_enum_range(dps.get(dp).and_then(Value::as_str), &range) {
                Some(list) if list.iter().any(|x| x == sv) => {}
                Some(list) => return Err(format!("« {sv} » n'est pas une valeur de « {} » ({})", label(dp), list.join(", "))),
                None => return Err(format!("« {} » : valeurs de l'appareil inconnues, modification bloquée par prudence", label(dp))),
            }
        }
        let values = schema.get(dp).and_then(|s| s.get("values"));
        let bound = |k: &str| values.and_then(|x| x.get(k)).and_then(Value::as_f64);
        if let (Some(n), Some(min), Some(max)) = (v.as_f64(), bound("min"), bound("max")) {
            if n < min || n > max {
                return Err(format!("« {} » : {n} est hors de la plage autorisée ({min} à {max})", label(dp)));
            }
        }
    }
    Ok(())
}

/// Code d'un DP d'après le schéma.
pub fn code_of<'a>(schema: &'a Map<String, Value>, dp: &str) -> Option<&'a str> {
    schema.get(dp).and_then(|s| s.get("code")).and_then(Value::as_str)
}

/// DP de minuterie (compte à rebours en secondes) : par le schéma, sinon le DP 9 des prises.
pub fn countdown_dp(kind: &Kind, dps: &Map<String, Value>, schema: &Map<String, Value>) -> Option<String> {
    if let Some((dp, _)) = schema.iter().find(|(_, s)| {
        s.get("code").and_then(Value::as_str).is_some_and(|c| c == "countdown" || c == "countdown_1")
    }) {
        return Some(dp.clone());
    }
    match kind {
        Kind::Plug { .. } if is_num(dps, "9") && schema.is_empty() => Some("9".into()),
        _ => None,
    }
}

/// Défauts signalés (bitmap) : masque brut + libellés français.
pub fn faults(dps: &Map<String, Value>, schema: &Map<String, Value>) -> (u32, Vec<String>) {
    let Some((dp, def)) = schema.iter().find(|(_, s)| s.get("code").and_then(Value::as_str) == Some("fault")) else {
        return (0, Vec::new());
    };
    let bits = dps.get(dp).and_then(Value::as_u64).unwrap_or(0) as u32;
    let names: Vec<String> = def
        .get("values")
        .and_then(|v| v.get("label"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let labels = (0..32)
        .filter(|i| bits & (1 << i) != 0)
        .map(|i| {
            let n = names.get(i).map(String::as_str).unwrap_or("");
            match n {
                "ov_cr" => "surintensité".to_string(),
                "ov_vol" => "surtension".to_string(),
                "ov_pwr" => "surpuissance".to_string(),
                "ls_cr" => "sous-intensité".to_string(),
                "ls_vol" => "sous-tension".to_string(),
                "ls_pow" => "sous-puissance".to_string(),
                "" => format!("défaut n°{i}"),
                other => other.replace('_', " "),
            }
        })
        .collect();
    (bits, labels)
}

/// Puissance instantanée en W (DP 19, échelle du schéma, 1 décimale par défaut).
pub fn power_w(dps: &Map<String, Value>, schema: &Map<String, Value>) -> Option<f64> {
    let raw = dps.get("19").and_then(Value::as_f64)?;
    let scale = schema.get("19").and_then(|s| s.get("values")).and_then(|v| v.get("scale")).and_then(Value::as_i64).unwrap_or(1);
    Some(raw / 10f64.powi(scale.clamp(0, 6) as i32))
}

/// Tension du secteur en V mesurée par une prise (DP 20, échelle du schéma, 1 décimale par défaut).
pub fn voltage_v(dps: &Map<String, Value>, schema: &Map<String, Value>) -> Option<f64> {
    if !(is_metering_plug(dps) || code_of(schema, "20") == Some("cur_voltage")) {
        return None;
    }
    let raw = dps.get("20").and_then(Value::as_f64)?;
    let scale = schema.get("20").and_then(|s| s.get("values")).and_then(|v| v.get("scale")).and_then(Value::as_i64).unwrap_or(1);
    Some(raw / 10f64.powi(scale.clamp(0, 6) as i32))
}

/// Comportement de la prise au retour du courant (« relay_status », DP 38 sans schéma) :
/// `Some("off")` reste éteinte, `Some("on")` se rallume, `Some("memory")` reprend l'état d'avant.
pub fn after_power_cut(dps: &Map<String, Value>, schema: &Map<String, Value>) -> Option<&'static str> {
    let dp = schema.iter().find(|(_, s)| s.get("code").and_then(Value::as_str) == Some("relay_status")).map(|(k, _)| k.as_str()).unwrap_or("38");
    match dps.get(dp)? {
        Value::String(s) => match s.as_str() {
            "off" | "power_off" | "0" => Some("off"),
            "on" | "power_on" | "1" => Some("on"),
            "memory" | "last" | "2" => Some("memory"),
            _ => None,
        },
        _ => None,
    }
}

/// Nombre à la française (virgule décimale).
pub fn fr(x: f64, decimals: usize) -> String {
    format!("{x:.decimals$}").replace('.', ",")
}

/// Durée lisible : « 45 s », « 30 min », « 2 h 05 ».
pub fn fmt_secs(secs: u64) -> String {
    match (secs / 3600, (secs % 3600) / 60, secs % 60) {
        (0, 0, s) => format!("{s} s"),
        (0, m, 0) => format!("{m} min"),
        (0, m, s) => format!("{m} min {s:02} s"),
        (h, 0, _) => format!("{h} h"),
        (h, m, _) => format!("{h} h {m:02}"),
    }
}

/// Valeur lisible d'un DP selon le schéma (échelle, unité, libellés des choix).
pub fn format_value(dp: &str, v: &Value, schema: &Map<String, Value>) -> String {
    let code = code_of(schema, dp).unwrap_or("");
    let values = schema.get(dp).and_then(|s| s.get("values"));
    match v {
        Value::Bool(b) => (if *b { "oui" } else { "non" }).into(),
        Value::Number(n) => {
            let scale = values.and_then(|x| x.get("scale")).and_then(Value::as_i64).unwrap_or(0).clamp(0, 6) as i32;
            let unit = values.and_then(|x| x.get("unit")).and_then(Value::as_str).unwrap_or("");
            let s = fr(n.as_f64().unwrap_or(0.0) / 10f64.powi(scale), scale as usize);
            if unit.is_empty() {
                s
            } else {
                format!("{s} {unit}")
            }
        }
        Value::String(s) => enum_label(code, s),
        Value::Null => "—".into(),
        other => other.to_string(),
    }
}

fn switch_dps(kind: &Kind, dps: &Map<String, Value>) -> Vec<String> {
    match kind {
        Kind::Switch { gangs } => gangs.clone(),
        k => k.switch_dp(dps).into_iter().collect(),
    }
}

/// Description française d'une écriture (ou d'un changement) de DP, pour le journal.
pub fn describe_write(dp: &str, v: &Value, kind: &Kind, dps: &Map<String, Value>, schema: &Map<String, Value>) -> String {
    if *kind == Kind::Vacuum {
        if let Some(text) = crate::vacuum::tuya::describe(dp, v, dps, schema) {
            return text;
        }
    }
    let switches = switch_dps(kind, dps);
    if let (Some(i), Some(on)) = (switches.iter().position(|s| s == dp), v.as_bool()) {
        let word = if on { "allumé" } else { "éteint" };
        return if switches.len() > 1 {
            let name = code_of(schema, dp).map(label_for_code).filter(|l| l != "Marche").unwrap_or_else(|| format!("Sortie {}", i + 1));
            format!("{name} : {word}e")
        } else {
            let mut c = word.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        };
    }
    if countdown_dp(kind, dps, schema).as_deref() == Some(dp) {
        return match v.as_u64() {
            Some(0) | None => "Minuterie annulée".into(),
            Some(s) => {
                let on = switches.first().and_then(|d| dps.get(d)).and_then(Value::as_bool).unwrap_or(false);
                format!("Minuterie : {} dans {}", if on { "extinction" } else { "allumage" }, fmt_secs(s))
            }
        };
    }
    let label = code_of(schema, dp).map(label_for_code).unwrap_or_else(|| format!("DP {dp}"));
    format!("{label} : {}", format_value(dp, v, schema))
}

/// DP qui changent tout seuls (mesures, défauts, décompte) : ce ne sont pas des actions.
pub fn is_measurement(dp: &str, kind: &Kind, dps: &Map<String, Value>, schema: &Map<String, Value>) -> bool {
    if countdown_dp(kind, dps, schema).as_deref() == Some(dp) {
        return true;
    }
    // Aspirateur : marche, retour, batterie, défauts… suivent son activité, inscrite à part.
    if *kind == Kind::Vacuum && crate::vacuum::tuya::is_state_dp(dp, dps, schema) {
        return true;
    }
    match code_of(schema, dp) {
        Some(code) => {
            code.starts_with("cur_")
                || code.starts_with("va_")
                || code.ends_with("_current")
                || matches!(code, "add_ele" | "fault" | "battery_percentage" | "temp_current")
        }
        // Sans description : une prise qui mesure sa puissance (DP 19) a ses mesures en 17 à 20 et ses défauts en 26.
        None => is_num(dps, "19") && !matches!(kind, Kind::LightV1 | Kind::LightV2) && matches!(dp, "17" | "18" | "19" | "20" | "26"),
    }
}

/// DP en lecture seule (mesures) : on les affiche sans contrôle.
pub fn is_readonly_code(code: &str) -> bool {
    is_protected_code(code)
        || code.starts_with("cur_")
        || code.starts_with("va_")
        || code.ends_with("_current")
        || code.ends_with("_state")
        || matches!(code, "battery_percentage" | "add_ele" | "fault" | "temp_current" | "cycle_time" | "random_time")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn m(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn detects_common_devices() {
        assert_eq!(detect(None, &m(json!({"20": true, "22": 500, "24": "00f003e803e8"}))), Kind::LightV2);
        assert_eq!(detect(None, &m(json!({"1": true, "2": "white", "3": 255}))), Kind::LightV1);
        assert_eq!(detect(None, &m(json!({"1": "stop", "2": 60}))), Kind::Cover);
        assert_eq!(detect(None, &m(json!({"1": true, "19": 120}))), Kind::Plug { metering: true });
        assert_eq!(
            detect(None, &m(json!({"1": true, "2": false, "3": true}))),
            Kind::Switch { gangs: vec!["1".into(), "2".into(), "3".into()] }
        );
        assert_eq!(detect(None, &m(json!({"101": 3}))), Kind::Generic);
        // Radiateur : DP 1 booléen mais consigne / mode → pas une prise.
        assert_eq!(detect(None, &m(json!({"1": true, "2": 20, "4": "auto"}))), Kind::Generic);
        assert_eq!(detect(Some("generic"), &m(json!({"1": true}))), Kind::Generic);
        // Robot aspirateur, reconnu par ses DP standard ou par sa catégorie.
        assert_eq!(detect(None, &m(json!({"1": true, "2": false, "5": "charge_done", "6": 100}))), Kind::Vacuum);
        assert_eq!(detect(Some("vacuum"), &m(json!({}))), Kind::Vacuum);
        assert_eq!(Kind::Vacuum.switch_dp(&m(json!({"1": true}))), None);
    }

    #[test]
    fn colour_roundtrip() {
        let c = parse_colour_v2("00f003e803e8").unwrap();
        assert!((c.h - 240.0 / 360.0).abs() < 1e-3 && c.s == 1.0 && c.v == 1.0);
        assert_eq!(encode_colour_v2(c), "00f003e803e8");
        let v1 = encode_colour_v1(Hsv { h: 0.0, s: 1.0, v: 1.0 });
        assert_eq!(v1, "ff00000000ffff");
        assert!(parse_colour_v1(&v1).is_some());
    }

    #[test]
    fn faults_and_countdown_from_schema() {
        let schema = m(json!({
            "9": { "code": "countdown_1", "type": "Integer", "values": {} },
            "26": { "code": "fault", "type": "Bitmap", "values": { "label": ["ov_cr", "ov_vol", "ov_pwr"] } }
        }));
        let dps = m(json!({ "1": true, "9": 0, "26": 5 }));
        assert_eq!(countdown_dp(&Kind::Plug { metering: false }, &dps, &schema).as_deref(), Some("9"));
        assert_eq!(faults(&dps, &schema), (5, vec!["surintensité".to_string(), "surpuissance".to_string()]));
        assert!(is_readonly_code("power_coe"));
        assert_eq!(enum_label("relay_status", "power_off"), "Rester éteinte");
    }

    #[test]
    fn calibration_is_protected_with_or_without_schema() {
        let dps = m(json!({ "1": true, "18": 100, "19": 230, "20": 2300, "22": 626, "23": 30683, "24": 17264, "25": 1048 }));
        let schema = m(json!({
            "1": { "code": "switch_1" },
            "22": { "code": "voltage_coe" }, "23": { "code": "electric_coe" },
            "24": { "code": "power_coe" }, "25": { "code": "electricity_coe" }
        }));
        let req = |k: &str| m(json!({ k: 1 }));
        for k in ["22", "23", "24", "25"] {
            assert!(check_writable(&req(k), &dps, &schema).is_err(), "avec schéma : {k}");
            assert!(check_writable(&req(k), &dps, &Map::new()).is_err(), "sans schéma : {k}");
        }
        assert!(check_writable(&m(json!({ "1": false })), &dps, &schema).is_ok());
        // Hors prise à mesure et sans schéma, le DP 22 peut être autre chose (luminosité d'une lampe).
        assert!(check_writable(&req("22"), &m(json!({ "20": true, "22": 500 })), &Map::new()).is_ok());
    }

    #[test]
    fn enum_values_follow_the_device_not_the_cloud() {
        let schema = m(json!({
            "38": { "code": "relay_status", "type": "Enum", "values": { "range": ["power_off", "power_on", "last"] } },
            "9": { "code": "countdown_1", "type": "Integer", "values": { "min": 0, "max": 86400 } }
        }));
        // La prise réelle répond « off » : elle attend off / on / memory.
        let dps = m(json!({ "38": "off", "9": 0 }));
        assert_eq!(local_enum_range(Some("off"), &enum_range(&schema, "38")).unwrap(), ["off", "on", "memory"]);
        assert!(check_writable(&m(json!({ "38": "memory" })), &dps, &schema).is_ok());
        assert!(check_writable(&m(json!({ "38": "power_off" })), &dps, &schema).is_err());
        // Valeur actuelle inconnue : on ne devine pas.
        assert!(check_writable(&m(json!({ "38": "on" })), &m(json!({ "38": "bizarre" })), &schema).is_err());
        // Plage numérique.
        assert!(check_writable(&m(json!({ "9": 90000 })), &dps, &schema).is_err());
        assert!(check_writable(&m(json!({ "9": 3600 })), &dps, &schema).is_ok());
    }

    #[test]
    fn journal_descriptions() {
        let schema = m(json!({
            "1": { "code": "switch_1" },
            "9": { "code": "countdown_1", "values": { "min": 0, "max": 86400 } },
            "38": { "code": "relay_status", "type": "Enum", "values": { "range": ["power_off", "power_on", "last"] } },
            "19": { "code": "cur_power", "values": { "unit": "W", "scale": 1 } }
        }));
        let dps = m(json!({ "1": true, "9": 0, "18": 1, "19": 2300, "20": 2300, "38": "off" }));
        let kind = Kind::Plug { metering: true };
        assert_eq!(describe_write("1", &json!(false), &kind, &dps, &schema), "Éteint");
        assert_eq!(describe_write("9", &json!(1800), &kind, &dps, &schema), "Minuterie : extinction dans 30 min");
        assert_eq!(describe_write("9", &json!(0), &kind, &dps, &schema), "Minuterie annulée");
        assert_eq!(describe_write("38", &json!("memory"), &kind, &dps, &schema), "Après une coupure de courant : Reprendre l'état précédent");
        assert!(is_measurement("19", &kind, &dps, &schema));
        assert!(is_measurement("9", &kind, &dps, &schema));
        assert!(!is_measurement("1", &kind, &dps, &schema));
        let strip = Kind::Switch { gangs: vec!["1".into(), "2".into()] };
        assert_eq!(describe_write("2", &json!(true), &strip, &m(json!({"1": true, "2": false})), &Map::new()), "Sortie 2 : allumée");
    }

    #[test]
    fn plug_metering_units() {
        let (p, a, v) = metering(&m(json!({"18": 210, "19": 486, "20": 2304}))).unwrap();
        assert_eq!((p, a, v), (48.6, 0.21, 230.4));
        // Valeurs lues sur la vraie « Multiprise - Bureau » (25/10/2026).
        let real = m(json!({ "1": true, "9": 0, "18": 1372, "19": 2911, "20": 2407, "26": 0, "38": "off" }));
        assert_eq!(voltage_v(&real, &Map::new()), Some(240.7));
        assert_eq!(after_power_cut(&real, &Map::new()), Some("off"));
        assert_eq!(after_power_cut(&m(json!({ "38": "memory" })), &Map::new()), Some("memory"));
        // Une ampoule (DP 20 = marche) n'a pas de tension.
        assert_eq!(voltage_v(&m(json!({ "20": true, "22": 500 })), &Map::new()), None);
    }
}
