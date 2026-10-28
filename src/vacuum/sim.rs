//! Robots aspirateurs simulés (démo et essais) : un « cerveau » commun qui nettoie, se décharge,
//! rentre à sa base et se recharge, exposé avec les vrais protocoles : Tuya (par `crate::sim`),
//! Valetudo (petit serveur HTTP) et miIO (serveur UDP chiffré, dialectes Roborock et MIoT).

use super::{miio, Activity, Command, ErrorKind};
use anyhow::Result;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UdpSocket};

/// État d'un robot simulé.
#[derive(Clone, Debug)]
pub struct SimRobot {
    pub activity: Activity,
    pub battery: f64,
    /// Puissance, eau et mode, dans le vocabulaire du protocole exposé.
    pub fan: String,
    pub water: String,
    pub mode: String,
    /// Nettoyage en cours : surface (m²) et durée (s).
    pub area: f64,
    pub secs: f64,
    pub error: Option<ErrorKind>,
    /// Signal sonore en cours (s).
    pub locating: f64,
    /// Accélération du temps (1 = temps réel).
    pub speed: f64,
    returning_left: f64,
    /// Nombre de changements (les serveurs poussent l'état quand il bouge).
    pub version: u64,
}

pub type Shared = Arc<Mutex<SimRobot>>;

/// Durée du retour à la base (s).
const RETURN_S: f64 = 8.0;

impl SimRobot {
    pub fn new(fan: &str) -> SimRobot {
        SimRobot {
            activity: Activity::Docked,
            battery: 100.0,
            fan: fan.into(),
            water: String::new(),
            mode: String::new(),
            area: 0.0,
            secs: 0.0,
            error: None,
            locating: 0.0,
            speed: 1.0,
            returning_left: 0.0,
            version: 0,
        }
    }

    fn bump(&mut self) {
        self.version += 1;
    }

    pub fn command(&mut self, cmd: &Command) -> Result<(), String> {
        match cmd {
            Command::Start | Command::Pattern(_) => {
                if self.error.is_some() {
                    return Err("robot en erreur".into());
                }
                if self.activity != Activity::Paused {
                    (self.area, self.secs) = (0.0, 0.0);
                }
                self.activity = Activity::Cleaning;
            }
            Command::Pause => {
                if self.activity.is_active() {
                    self.activity = Activity::Paused;
                }
            }
            Command::Stop => {
                if !self.activity.at_dock() && self.activity != Activity::Error {
                    self.activity = Activity::Idle;
                }
            }
            Command::Dock => {
                if !self.activity.at_dock() {
                    self.activity = Activity::Returning;
                    self.returning_left = RETURN_S;
                }
            }
            Command::Locate => self.locating = 3.0,
            Command::Fan(v) => self.fan = v.clone(),
            Command::Water(v) => self.water = v.clone(),
            Command::Mode(v) => self.mode = v.clone(),
        }
        self.bump();
        Ok(())
    }

    /// Erreur simulée (`None` : réparée, le robot attend sur place).
    pub fn set_error(&mut self, kind: Option<ErrorKind>) {
        self.error = kind;
        self.activity = if kind.is_some() { Activity::Error } else if self.activity == Activity::Error { Activity::Idle } else { self.activity };
        self.bump();
    }

    /// Avance le temps de `dt` secondes.
    pub fn tick(&mut self, dt: f64) {
        let dt = dt * self.speed;
        let before = (self.activity, self.battery.round() as i64, self.area.round() as i64, (self.secs / 60.0) as i64, self.locating > 0.0);
        self.locating = (self.locating - dt).max(0.0);
        match self.activity {
            Activity::Cleaning => {
                self.battery = (self.battery - dt * 0.25).max(0.0);
                self.area += dt * 0.2;
                self.secs += dt;
                if self.battery <= 15.0 {
                    self.activity = Activity::Returning;
                    self.returning_left = RETURN_S;
                }
            }
            Activity::Returning => {
                self.battery = (self.battery - dt * 0.1).max(0.0);
                self.returning_left -= dt;
                if self.returning_left <= 0.0 {
                    self.activity = Activity::Charging;
                }
            }
            Activity::Charging => {
                self.battery = (self.battery + dt * 1.5).min(100.0);
                if self.battery >= 100.0 {
                    self.activity = Activity::Docked;
                }
            }
            Activity::Docked if self.battery < 98.0 => self.activity = Activity::Charging,
            Activity::Paused | Activity::Idle => self.battery = (self.battery - dt * 0.01).max(0.0),
            _ => {}
        }
        let after = (self.activity, self.battery.round() as i64, self.area.round() as i64, (self.secs / 60.0) as i64, self.locating > 0.0);
        if before != after {
            self.bump();
        }
    }
}

/// Fait vivre un robot : un tic par seconde, puis `changed` si son état a bougé.
pub fn spawn_clock(robot: Shared, changed: impl Fn() + Send + 'static) {
    tokio::spawn(async move {
        let mut seen = robot.lock().unwrap().version;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if Arc::strong_count(&robot) == 1 {
                return; // plus personne ne s'en sert
            }
            let v = {
                let mut r = robot.lock().unwrap();
                r.tick(1.0);
                r.version
            };
            if v != seen {
                seen = v;
                changed();
            }
        }
    });
}

fn registry() -> &'static Mutex<HashMap<String, Shared>> {
    static R: OnceLock<Mutex<HashMap<String, Shared>>> = OnceLock::new();
    R.get_or_init(Mutex::default)
}

/// Robot simulé par son identifiant (démo : pannes provoquées).
pub fn robot(id: &str) -> Option<Shared> {
    registry().lock().unwrap().get(id).cloned()
}

pub fn register(id: &str, robot: Shared) {
    registry().lock().unwrap().insert(id.to_string(), robot);
}

// ---- Tuya ----------------------------------------------------------------------------------------

/// Bit du DP « fault » (description standard de `super::tuya`) pour une erreur.
fn tuya_fault_bit(k: ErrorKind) -> u32 {
    match k {
        ErrorKind::SideBrush => 0,
        ErrorKind::MainBrush => 1,
        ErrorKind::Wheel => 2,
        ErrorKind::BinMissing => 4,
        ErrorKind::Cliff => 5,
        ErrorKind::Bumper => 6,
        ErrorKind::Lifted => 7,
        ErrorKind::LowBattery => 8,
        ErrorKind::BinFull => 9,
        _ => 6,
    }
}

impl SimRobot {
    /// DP Tuya (numérotation standard) de l'état actuel.
    pub fn tuya_dps(&self) -> Map<String, Value> {
        let status = match self.activity {
            Activity::Cleaning => "smart_clean",
            Activity::Paused => "paused",
            Activity::Returning => "goto_charge",
            Activity::Charging => "charging",
            Activity::Docked | Activity::Station => "charge_done",
            Activity::Idle | Activity::Unknown => "standby",
            Activity::Error => "fault",
        };
        let mode = match self.activity {
            Activity::Cleaning | Activity::Paused => "smart",
            Activity::Returning => "chargego",
            _ => "standby",
        };
        let fault = self.error.map(|k| 1u32 << tuya_fault_bit(k)).unwrap_or(0);
        let mut m = Map::new();
        for (k, v) in [
            ("2", json!(matches!(self.activity, Activity::Cleaning | Activity::Paused))),
            ("3", json!(mode)),
            ("5", json!(status)),
            ("6", json!(self.battery.round() as i64)),
            ("13", json!(self.locating > 0.0)),
            ("14", json!(self.fan)),
            ("15", json!(if self.water.is_empty() { "closed" } else { &self.water })),
            ("16", json!(self.area.round() as i64)),
            ("17", json!((self.secs / 60.0).round() as i64)),
            ("18", json!(fault)),
            ("25", json!(self.activity == Activity::Paused)),
            ("26", json!(self.activity == Activity::Returning)),
        ] {
            m.insert(k.into(), v);
        }
        m
    }

    /// Écriture de DP Tuya : `true` si elle concernait le robot.
    pub fn apply_tuya(&mut self, req: &Map<String, Value>) -> bool {
        let mut handled = false;
        for (k, v) in req {
            let cmd = match (k.as_str(), v) {
                ("2", Value::Bool(true)) => Some(Command::Start),
                ("2", Value::Bool(false)) => Some(Command::Stop),
                ("25", Value::Bool(true)) => Some(Command::Pause),
                ("26", Value::Bool(true)) => Some(Command::Dock),
                ("3", Value::String(m)) if m == "chargego" => Some(Command::Dock),
                ("3", Value::String(m)) if m != "standby" => Some(Command::Pattern(m.clone())),
                ("13", Value::Bool(true)) => Some(Command::Locate),
                ("14", Value::String(f)) => Some(Command::Fan(f.clone())),
                ("15", Value::String(w)) => Some(Command::Water(w.clone())),
                _ => None,
            };
            if let Some(c) = cmd {
                handled = true;
                let _ = self.command(&c);
            }
        }
        handled
    }
}

// ---- Valetudo (HTTP) ---------------------------------------------------------------------------------

const V_FANS: [&str; 4] = ["low", "medium", "high", "max"];
const V_WATERS: [&str; 3] = ["low", "medium", "high"];
const V_MODES: [&str; 3] = ["vacuum", "mop", "vacuum_and_mop"];

fn english(k: ErrorKind) -> (&'static str, &'static str) {
    match k {
        ErrorKind::MainBrush => ("motors", "Main brush jammed"),
        ErrorKind::SideBrush => ("motors", "Side brush jammed"),
        ErrorKind::Wheel => ("motors", "Wheel jammed"),
        ErrorKind::Stuck => ("navigation", "Robot stuck"),
        ErrorKind::BinMissing => ("attachments", "Dustbin missing"),
        ErrorKind::BinFull => ("attachments", "Dustbin full"),
        ErrorKind::Cliff => ("sensors", "Cliff sensor dirty or robot on the verge of falling"),
        ErrorKind::Lifted => ("sensors", "Wheel suspended"),
        ErrorKind::BatteryFault => ("power", "Battery temperature out of range"),
        _ => ("core", "Unknown error"),
    }
}

impl SimRobot {
    pub fn valetudo_attributes(&self) -> Value {
        let status = match self.activity {
            Activity::Cleaning => "cleaning",
            Activity::Paused => "paused",
            Activity::Returning => "returning",
            Activity::Charging | Activity::Docked | Activity::Station => "docked",
            Activity::Idle | Activity::Unknown => "idle",
            Activity::Error => "error",
        };
        let mut st = json!({ "__class": "StatusStateAttribute", "value": status, "flag": "none" });
        if let Some(k) = self.error {
            let (subsystem, message) = english(k);
            st["error"] = json!({ "severity": { "kind": "transient", "level": "error" }, "subsystem": subsystem, "message": message, "vendorErrorCode": "sim" });
        }
        let flag = match self.activity {
            Activity::Charging => "charging",
            Activity::Docked => "charged",
            _ => "discharging",
        };
        json!([
            st,
            { "__class": "BatteryStateAttribute", "level": self.battery.round(), "flag": flag },
            { "__class": "PresetSelectionStateAttribute", "type": "fan_speed", "value": self.fan },
            { "__class": "PresetSelectionStateAttribute", "type": "water_grade", "value": self.water },
            { "__class": "PresetSelectionStateAttribute", "type": "operation_mode", "value": self.mode },
            { "__class": "AttachmentStateAttribute", "type": "dustbin", "attached": self.error != Some(ErrorKind::BinMissing) }
        ])
    }
}

/// Démarre un robot Valetudo simulé (HTTP sur 127.0.0.1, port libre). `auth` : identifiants exigés.
pub async fn start_valetudo(model: &str, auth: Option<(&str, &str)>) -> Result<(u16, Shared)> {
    let mut r = SimRobot::new("medium");
    (r.water, r.mode) = ("low".into(), "vacuum".into());
    let robot: Shared = Arc::new(Mutex::new(r));
    spawn_clock(robot.clone(), || {});
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let model = model.to_string();
    let auth = auth.map(|(u, p)| {
        use base64::Engine;
        format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}")))
    });
    let r = robot.clone();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            let (r, model, auth) = (r.clone(), model.clone(), auth.clone());
            tokio::spawn(async move {
                let _ = valetudo_conn(sock, r, &model, auth.as_deref()).await;
            });
        }
    });
    Ok((port, robot))
}

async fn valetudo_conn(sock: tokio::net::TcpStream, robot: Shared, model: &str, auth: Option<&str>) -> Result<()> {
    let (rd, mut wr) = sock.into_split();
    let mut rd = BufReader::new(rd);
    loop {
        let mut line = String::new();
        if rd.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let mut parts = line.split_whitespace();
        let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
        let mut len = 0usize;
        let mut authorization = String::new();
        loop {
            let mut h = String::new();
            rd.read_line(&mut h).await?;
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                match k.trim().to_lowercase().as_str() {
                    "content-length" => len = v.trim().parse().unwrap_or(0),
                    "authorization" => authorization = v.trim().to_string(),
                    _ => {}
                }
            }
        }
        let mut body = vec![0u8; len];
        rd.read_exact(&mut body).await?;
        let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let (code, answer) = if auth.is_some_and(|a| a != authorization) {
            (401, json!("Unauthorized"))
        } else {
            valetudo_route(&robot, model, &method, &path, &body)
        };
        let text = answer.to_string();
        let reason = match code {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            _ => "Not Found",
        };
        wr.write_all(format!("HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{text}", text.len()).as_bytes()).await?;
    }
}

fn valetudo_route(robot: &Shared, model: &str, method: &str, path: &str, body: &Value) -> (u16, Value) {
    let path = path.strip_prefix("/api/v2/robot").unwrap_or("?");
    let mut r = robot.lock().unwrap();
    let preset = |list: &[&str], r: &mut SimRobot, set: fn(&mut SimRobot, String)| -> (u16, Value) {
        match body.get("name").and_then(Value::as_str).filter(|n| list.contains(n)) {
            Some(n) => {
                set(r, n.to_string());
                r.version += 1;
                (200, json!("OK"))
            }
            None => (400, json!("Invalid preset")),
        }
    };
    match (method, path) {
        ("GET", "" | "/") => (200, json!({ "manufacturer": "Dreame", "modelName": model, "implementation": "DreameValetudoRobot" })),
        ("GET", "/capabilities") => (
            200,
            json!([
                "BasicControlCapability", "LocateCapability", "FanSpeedControlCapability", "WaterUsageControlCapability",
                "OperationModeControlCapability", "ConsumableMonitoringCapability", "CurrentStatisticsCapability"
            ]),
        ),
        ("GET", "/state/attributes") => (200, r.valetudo_attributes()),
        ("GET", "/capabilities/FanSpeedControlCapability/presets") => (200, json!(V_FANS)),
        ("GET", "/capabilities/WaterUsageControlCapability/presets") => (200, json!(V_WATERS)),
        ("GET", "/capabilities/OperationModeControlCapability/presets") => (200, json!(V_MODES)),
        ("PUT", "/capabilities/FanSpeedControlCapability/preset") => preset(&V_FANS, &mut r, |r, v| r.fan = v),
        ("PUT", "/capabilities/WaterUsageControlCapability/preset") => preset(&V_WATERS, &mut r, |r, v| r.water = v),
        ("PUT", "/capabilities/OperationModeControlCapability/preset") => preset(&V_MODES, &mut r, |r, v| r.mode = v),
        ("PUT", "/capabilities/BasicControlCapability") => {
            let cmd = match body.get("action").and_then(Value::as_str) {
                Some("start") => Command::Start,
                Some("pause") => Command::Pause,
                Some("stop") => Command::Stop,
                Some("home") => Command::Dock,
                _ => return (400, json!("Invalid action")),
            };
            match r.command(&cmd) {
                Ok(()) => (200, json!("OK")),
                Err(e) => (400, json!(e)),
            }
        }
        ("PUT", "/capabilities/LocateCapability") if body.get("action").and_then(Value::as_str) == Some("locate") => {
            let _ = r.command(&Command::Locate);
            (200, json!("OK"))
        }
        ("GET", "/capabilities/CurrentStatisticsCapability") => (200, json!([{ "type": "time", "value": r.secs.round() }, { "type": "area", "value": (r.area * 10_000.0).round() }])),
        ("GET", "/capabilities/ConsumableMonitoringCapability") => (
            200,
            json!([
                { "type": "brush", "subType": "main", "remaining": { "value": 14_400, "unit": "minutes" } },
                { "type": "brush", "subType": "side_right", "remaining": { "value": 9_000, "unit": "minutes" } },
                { "type": "filter", "subType": "main", "remaining": { "value": 62, "unit": "percent" } }
            ]),
        ),
        _ => (404, json!("Not Found")),
    }
}

// ---- miIO (UDP) ------------------------------------------------------------------------------------

/// Robot miIO simulé : `model` choisit le dialecte (`roborock.vacuum.*` ou MIoT).
pub struct SimMiio {
    pub port: u16,
    pub device_id: u32,
    pub robot: Shared,
}

pub async fn start_miio(model: &str, token: [u8; 16]) -> Result<SimMiio> {
    let dialect = miio::dialect_of(model);
    let robot: Shared = Arc::new(Mutex::new(SimRobot::new(if dialect == miio::Dialect::Roborock { "102" } else { "1" })));
    {
        let mut r = robot.lock().unwrap();
        (r.water, r.mode) = ("2".into(), "0".into());
    }
    spawn_clock(robot.clone(), || {});
    let sock = UdpSocket::bind("127.0.0.1:0").await?;
    let port = sock.local_addr()?.port();
    let device_id: u32 = 0x1a2b_0000 | (port as u32);
    let (r, model) = (robot.clone(), model.to_string());
    tokio::spawn(async move {
        let start = std::time::Instant::now();
        let mut buf = vec![0u8; 4096];
        while let Ok((n, from)) = sock.recv_from(&mut buf).await {
            let data = &buf[..n];
            let stamp = 1_000 + start.elapsed().as_secs() as u32;
            if n == 32 && data[4..].iter().all(|b| *b == 0xff) {
                // Poignée de main : identifiant et horodatage, somme à 0xff.
                let mut p = vec![0x21, 0x31, 0x00, 0x20, 0, 0, 0, 0];
                p.extend_from_slice(&device_id.to_be_bytes());
                p.extend_from_slice(&stamp.to_be_bytes());
                p.extend_from_slice(&[0xff; 16]);
                let _ = sock.send_to(&p, from).await;
                continue;
            }
            let Ok((_, plain)) = miio::decode(&token, data) else { continue }; // mauvais jeton : silence
            let Ok(req) = serde_json::from_slice::<Value>(&plain) else { continue };
            let id = req.get("id").cloned().unwrap_or(json!(0));
            let method = req.get("method").and_then(Value::as_str).unwrap_or("");
            let params = req.get("params").cloned().unwrap_or(Value::Null);
            let result = miio_answer(&r, &model, dialect, method, &params);
            let msg = match result {
                Ok(v) => json!({ "id": id, "result": v }),
                Err(e) => json!({ "id": id, "error": { "code": -1, "message": e } }),
            };
            let pkt = miio::encode(&token, miio::Header { device_id, stamp }, msg.to_string().as_bytes());
            let _ = sock.send_to(&pkt, from).await;
        }
    });
    Ok(SimMiio { port, device_id, robot })
}

fn miio_answer(robot: &Shared, model: &str, dialect: miio::Dialect, method: &str, params: &Value) -> Result<Value, String> {
    let mut r = robot.lock().unwrap();
    if method == "miIO.info" {
        return Ok(json!({ "model": model, "fw_ver": "sim", "hw_ver": "Linux" }));
    }
    match dialect {
        miio::Dialect::Roborock => {
            let state = match r.activity {
                Activity::Cleaning => 5,
                Activity::Paused => 10,
                Activity::Returning => 6,
                Activity::Charging | Activity::Docked => 8,
                Activity::Station => 23,
                Activity::Idle | Activity::Unknown => 3,
                Activity::Error => 12,
            };
            let error_code = match r.error {
                None => 0,
                Some(ErrorKind::MainBrush) => 5,
                Some(ErrorKind::SideBrush) => 6,
                Some(ErrorKind::Wheel) => 7,
                Some(ErrorKind::Stuck) => 8,
                Some(ErrorKind::BinMissing) => 9,
                Some(ErrorKind::Cliff) => 4,
                Some(ErrorKind::BinFull) => 254,
                Some(_) => 255,
            };
            let ok = || Ok(json!(["ok"]));
            match method {
                "get_status" => Ok(json!([{
                    "state": state, "battery": r.battery.round() as i64, "fan_power": r.fan.parse::<i64>().unwrap_or(102),
                    "clean_time": r.secs.round() as i64, "clean_area": (r.area * 1_000_000.0).round() as i64, "error_code": error_code,
                    "in_cleaning": i64::from(r.activity == Activity::Cleaning), "map_present": 1
                }])),
                "get_consumable" => Ok(json!([{ "main_brush_work_time": 360_000, "side_brush_work_time": 180_000, "filter_work_time": 270_000, "sensor_dirty_time": 36_000 }])),
                "app_start" => r.command(&Command::Start).and_then(|_| ok()),
                "app_pause" => r.command(&Command::Pause).and_then(|_| ok()),
                "app_stop" => r.command(&Command::Stop).and_then(|_| ok()),
                "app_charge" => r.command(&Command::Dock).and_then(|_| ok()),
                "find_me" => r.command(&Command::Locate).and_then(|_| ok()),
                "set_custom_mode" => match params.get(0).and_then(Value::as_i64).filter(|v| (101..=105).contains(v)) {
                    Some(v) => r.command(&Command::Fan(v.to_string())).and_then(|_| ok()),
                    None => Err("invalid fan".into()),
                },
                _ => Err(format!("unknown method {method}")),
            }
        }
        miio::Dialect::Miot => {
            let state = match r.activity {
                Activity::Cleaning => 1,
                Activity::Paused => 3,
                Activity::Returning => 5,
                Activity::Charging => 6,
                Activity::Docked => 13,
                Activity::Station => 22,
                Activity::Idle | Activity::Unknown => 2,
                Activity::Error => 4,
            };
            let error = match r.error {
                None => 0,
                Some(ErrorKind::MainBrush) => 12,
                Some(ErrorKind::SideBrush) => 13,
                Some(ErrorKind::BinFull) => 11,
                Some(ErrorKind::BinMissing) => 8,
                Some(ErrorKind::Stuck) => 47,
                Some(ErrorKind::Cliff) => 2,
                Some(ErrorKind::Lifted) => 1,
                Some(_) => 23,
            };
            let charging = match r.activity {
                Activity::Charging => 1,
                Activity::Docked => 3,
                _ => 2,
            };
            let value = |s: i64, p: i64, r: &SimRobot| -> Option<Value> {
                Some(match (s, p) {
                    (2, 1) => json!(state),
                    (2, 2) => json!(error),
                    (3, 1) => json!(r.battery.round() as i64),
                    (3, 2) => json!(charging),
                    (4, 2) => json!((r.secs / 60.0).round() as i64),
                    (4, 3) => json!(r.area.round() as i64),
                    (4, 4) => json!(r.fan.parse::<i64>().unwrap_or(1)),
                    (4, 5) => json!(r.water.parse::<i64>().unwrap_or(2)),
                    (4, 23) => json!(r.mode.parse::<i64>().unwrap_or(0)),
                    (9, 2) => json!(83),
                    (10, 2) => json!(64),
                    (11, 1) => json!(71),
                    _ => return None,
                })
            };
            match method {
                "get_properties" => Ok(Value::Array(
                    params
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|p| {
                            let (s, pi) = (p["siid"].as_i64().unwrap_or(0), p["piid"].as_i64().unwrap_or(0));
                            match value(s, pi, &r) {
                                Some(v) => json!({ "did": p["did"], "siid": s, "piid": pi, "code": 0, "value": v }),
                                None => json!({ "did": p["did"], "siid": s, "piid": pi, "code": -4001 }),
                            }
                        })
                        .collect(),
                )),
                "set_properties" => {
                    let mut out = Vec::new();
                    for p in params.as_array().into_iter().flatten() {
                        let (s, pi, v) = (p["siid"].as_i64().unwrap_or(0), p["piid"].as_i64().unwrap_or(0), p["value"].as_i64().unwrap_or(-1));
                        let cmd = match (s, pi) {
                            (4, 4) if (0..=3).contains(&v) => Some(Command::Fan(v.to_string())),
                            (4, 5) if (1..=3).contains(&v) => Some(Command::Water(v.to_string())),
                            (4, 23) if (0..=3).contains(&v) => Some(Command::Mode(v.to_string())),
                            _ => None,
                        };
                        let code = match cmd {
                            Some(c) => r.command(&c).map(|_| 0).unwrap_or(-1),
                            None => -4004,
                        };
                        out.push(json!({ "did": p["did"], "siid": s, "piid": pi, "code": code }));
                    }
                    Ok(Value::Array(out))
                }
                "action" => {
                    let cmd = match (params["siid"].as_i64(), params["aiid"].as_i64()) {
                        (Some(2), Some(1)) => Command::Start,
                        (Some(2), Some(2)) => Command::Pause,
                        (Some(3), Some(1)) => Command::Dock,
                        (Some(4), Some(2)) => Command::Stop,
                        (Some(7), Some(1)) => Command::Locate,
                        _ => return Ok(json!({ "code": -4004 })),
                    };
                    let code = if r.command(&cmd).is_ok() { 0 } else { -4005 };
                    Ok(json!({ "did": params["did"], "siid": params["siid"], "aiid": params["aiid"], "code": code, "out": [] }))
                }
                _ => Err(format!("unknown method {method}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vacuum::{miio::Robot, valetudo::Valetudo};

    #[test]
    fn brain_cleans_returns_and_charges() {
        let mut r = SimRobot::new("normal");
        r.command(&Command::Start).unwrap();
        assert_eq!(r.activity, Activity::Cleaning);
        r.speed = 10.0;
        for _ in 0..30 {
            r.tick(1.0);
        }
        assert!(r.area > 50.0 && r.battery < 100.0 && r.activity == Activity::Cleaning, "{r:?}");
        r.battery = 16.0;
        r.tick(1.0);
        assert_eq!(r.activity, Activity::Returning, "batterie faible : retour");
        r.tick(1.0);
        assert_eq!(r.activity, Activity::Charging);
        for _ in 0..10 {
            r.tick(1.0);
        }
        assert_eq!(r.activity, Activity::Docked);
        r.set_error(Some(ErrorKind::MainBrush));
        assert!(r.command(&Command::Start).is_err());
        r.set_error(None);
        assert_eq!(r.activity, Activity::Idle);
        let mut tuya = SimRobot::new("normal");
        assert!(tuya.apply_tuya(&json!({ "2": true }).as_object().unwrap().clone()));
        assert_eq!(tuya.tuya_dps()["5"], json!("smart_clean"));
        assert!(tuya.apply_tuya(&json!({ "26": true }).as_object().unwrap().clone()));
        assert_eq!(tuya.tuya_dps()["5"], json!("goto_charge"));
    }

    /// Le client Valetudo contre le robot simulé : modèle, état, commandes, réglages, identifiants.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn valetudo_client_against_simulator() {
        let (port, robot) = start_valetudo("L10s Ultra", Some(("valetudo", "secret"))).await.unwrap();
        let st = tokio::task::spawn_blocking(move || {
            let mut bad = Valetudo::new("127.0.0.1", Some(port), Some("valetudo"), Some("faux"));
            assert!(format!("{:#}", bad.connect().unwrap_err()).contains("identifiants"));
            let mut v = Valetudo::new("127.0.0.1", Some(port), Some("valetudo"), Some("secret"));
            let info = v.connect().unwrap();
            assert_eq!(info.label(), "Dreame L10s Ultra");
            assert!(v.caps.locate && v.caps.fans.len() == 4 && v.caps.modes.len() == 3);
            v.run(&Command::Start).unwrap();
            v.run(&Command::Fan("max".into())).unwrap();
            assert!(v.run(&Command::Fan("turbo".into())).is_err(), "puissance inconnue refusée");
            v.run(&Command::Mode("vacuum_and_mop".into())).unwrap();
            v.read().unwrap()
        })
        .await
        .unwrap();
        assert_eq!((st.activity, st.fan.as_deref(), st.mode.as_deref()), (Activity::Cleaning, Some("max"), Some("vacuum_and_mop")));
        assert_eq!(st.consumables.len(), 3);
        robot.lock().unwrap().set_error(Some(ErrorKind::SideBrush));
        let st = tokio::task::spawn_blocking(move || {
            let mut v = Valetudo::new("127.0.0.1", Some(port), Some("valetudo"), Some("secret"));
            v.connect().unwrap();
            v.read().unwrap()
        })
        .await
        .unwrap();
        assert_eq!((st.activity, st.errors[0].kind), (Activity::Error, ErrorKind::SideBrush));
    }

    /// Le client miIO contre les robots simulés, dans les deux dialectes, et avec un mauvais jeton.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn miio_client_against_simulators() {
        let token_hex = "8f2f7a1b3c4d5e6f708192a3b4c5d6e7";
        let token = miio::parse_token(token_hex).unwrap();
        for model in ["roborock.vacuum.s5", "dreame.vacuum.p2008"] {
            let sim = start_miio(model, token).await.unwrap();
            let mut robot = Robot::connect("127.0.0.1", Some(sim.port), token_hex).await.unwrap_or_else(|e| panic!("{model} : {e:#}"));
            assert_eq!(robot.model, model);
            let st = robot.read().await.unwrap();
            assert_eq!((st.activity, st.battery), (Activity::Docked, Some(100)), "{model}");
            assert!(!robot.caps.fans.is_empty() && !st.consumables.is_empty(), "{model}");
            robot.run(&Command::Start).await.unwrap();
            assert_eq!(robot.read().await.unwrap().activity, Activity::Cleaning, "{model}");
            let fan = robot.caps.fans.last().unwrap().value.clone();
            robot.run(&Command::Fan(fan.clone())).await.unwrap();
            robot.run(&Command::Dock).await.unwrap();
            let st = robot.read().await.unwrap();
            assert_eq!((st.activity, st.fan), (Activity::Returning, Some(fan)), "{model}");
            sim.robot.lock().unwrap().set_error(Some(ErrorKind::MainBrush));
            let st = robot.read().await.unwrap();
            assert_eq!((st.activity, st.errors[0].kind), (Activity::Error, ErrorKind::MainBrush), "{model}");
            // Mauvais jeton : la poignée de main passe, puis plus rien ne répond.
            let wrong = "00000000000000000000000000000000";
            let err = Robot::connect("127.0.0.1", Some(sim.port), wrong).await.err().expect("jeton faux accepté");
            assert!(format!("{err:#}").contains("jeton"), "{err:#}");
        }
    }
}
