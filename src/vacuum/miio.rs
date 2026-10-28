//! Protocole miIO (Xiaomi, Roborock, Dreame fabriqués avant 2022) : UDP 54321, en local.
//!
//! Un paquet = un en-tête de 32 octets (0x2131, longueur, zéro, identifiant de l'appareil,
//! horodatage de l'appareil, somme MD5) suivi du JSON chiffré en AES-128-CBC avec bourrage PKCS#7 :
//! clé = MD5(jeton), IV = MD5(clé ‖ jeton). La somme MD5 couvre l'en-tête (somme remplacée par le
//! jeton) et le message chiffré. Une poignée de main (« hello », 32 octets à 0xff) donne
//! l'identifiant et l'horodatage à reprendre.
//!
//! Le jeton (16 octets, 32 caractères hexadécimaux) est propre à chaque appareil et ne circule pas
//! sur le réseau une fois l'appareil appairé : il faut le récupérer une fois (outil d'extraction
//! depuis le compte Xiaomi, ancienne application Mi Home…). Les Dreame vendus avec l'application
//! Dreamehome (depuis 2022) n'ont plus d'API locale, quel que soit le jeton.
//!
//! Deux dialectes de robots :
//! - Roborock (`roborock.vacuum.*`, `rockrobo.vacuum.*`) : `get_status`, `app_start`, `app_charge`…
//! - MIoT (Dreame et la plupart des autres) : propriétés `siid`/`piid` et actions `siid`/`aiid`.

use super::{mode_label, Activity, Caps, Choice, Command, Consumable, ErrorKind, Status, VacError};
use crate::protocol::crypto::md5;
use aes::cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

pub const PORT: u16 = 54321;
const MAGIC: [u8; 2] = [0x21, 0x31];

/// Jeton écrit en hexadécimal (32 caractères).
pub fn parse_token(s: &str) -> Result<[u8; 16]> {
    let s = s.trim();
    if s.len() != 32 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("jeton invalide : il faut 32 caractères hexadécimaux (0-9, a-f)");
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)?;
    }
    Ok(out)
}

fn keys(token: &[u8; 16]) -> ([u8; 16], [u8; 16]) {
    let key = md5(token);
    let mut kt = key.to_vec();
    kt.extend_from_slice(token);
    (key, md5(&kt))
}

/// AES-128-CBC avec bourrage PKCS#7.
pub fn encrypt(token: &[u8; 16], plain: &[u8]) -> Vec<u8> {
    let (key, iv) = keys(token);
    let cipher = Aes128::new(GenericArray::from_slice(&key));
    let mut buf = plain.to_vec();
    let n = 16 - buf.len() % 16;
    buf.extend(std::iter::repeat(n as u8).take(n));
    let mut prev = iv;
    for block in buf.chunks_exact_mut(16) {
        for (b, p) in block.iter_mut().zip(prev.iter()) {
            *b ^= p;
        }
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
        prev.copy_from_slice(block);
    }
    buf
}

pub fn decrypt(token: &[u8; 16], data: &[u8]) -> Result<Vec<u8>> {
    if data.is_empty() || data.len() % 16 != 0 {
        bail!("message chiffré de taille invalide ({} octets)", data.len());
    }
    let (key, iv) = keys(token);
    let cipher = Aes128::new(GenericArray::from_slice(&key));
    let mut buf = data.to_vec();
    let mut prev = iv;
    for block in buf.chunks_exact_mut(16) {
        let saved: [u8; 16] = block.try_into().unwrap();
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
        for (b, p) in block.iter_mut().zip(prev.iter()) {
            *b ^= p;
        }
        prev = saved;
    }
    let n = *buf.last().unwrap() as usize;
    if n == 0 || n > 16 || !buf[buf.len() - n..].iter().all(|&b| b as usize == n) {
        bail!("déchiffrement impossible : jeton incorrect ?");
    }
    buf.truncate(buf.len() - n);
    Ok(buf)
}

/// Paquet de poignée de main.
pub fn hello_packet() -> [u8; 32] {
    let mut p = [0xffu8; 32];
    p[..4].copy_from_slice(&[0x21, 0x31, 0x00, 0x20]);
    p
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Header {
    pub device_id: u32,
    pub stamp: u32,
}

pub fn parse_header(data: &[u8]) -> Result<Header> {
    if data.len() < 32 || data[..2] != MAGIC {
        bail!("ce n'est pas un paquet miIO");
    }
    let len = u16::from_be_bytes([data[2], data[3]]) as usize;
    if len != data.len() {
        bail!("paquet miIO tronqué ({} octets sur {len})", data.len());
    }
    Ok(Header { device_id: u32::from_be_bytes(data[8..12].try_into().unwrap()), stamp: u32::from_be_bytes(data[12..16].try_into().unwrap()) })
}

/// Paquet chiffré (`payload` : JSON en clair).
pub fn encode(token: &[u8; 16], h: Header, payload: &[u8]) -> Vec<u8> {
    let body = encrypt(token, payload);
    let mut p = Vec::with_capacity(32 + body.len());
    p.extend_from_slice(&MAGIC);
    p.extend_from_slice(&((32 + body.len()) as u16).to_be_bytes());
    p.extend_from_slice(&[0, 0, 0, 0]);
    p.extend_from_slice(&h.device_id.to_be_bytes());
    p.extend_from_slice(&h.stamp.to_be_bytes());
    p.extend_from_slice(token);
    p.extend_from_slice(&body);
    let sum = md5(&p);
    p[16..32].copy_from_slice(&sum);
    p
}

/// En-tête et JSON en clair d'un paquet (somme vérifiée).
pub fn decode(token: &[u8; 16], data: &[u8]) -> Result<(Header, Vec<u8>)> {
    let h = parse_header(data)?;
    let mut check = data.to_vec();
    check[16..32].copy_from_slice(token);
    if md5(&check)[..] != data[16..32] {
        bail!("somme de contrôle fausse : jeton incorrect ?");
    }
    if data.len() == 32 {
        return Ok((h, Vec::new()));
    }
    let mut plain = decrypt(token, &data[32..])?;
    while plain.last() == Some(&0) {
        plain.pop();
    }
    Ok((h, plain))
}

/// Liaison avec un appareil miIO.
pub struct Client {
    sock: UdpSocket,
    addr: SocketAddr,
    token: [u8; 16],
    device_id: u32,
    /// Horodatage de l'appareil à la poignée de main, et quand.
    stamp: (u32, Instant),
    next_id: u64,
}

impl Client {
    pub async fn connect(host: &str, port: u16, token: [u8; 16]) -> Result<Client> {
        let addr: SocketAddr = tokio::net::lookup_host((host.trim(), port)).await?.next().ok_or_else(|| anyhow!("adresse introuvable : {host}"))?;
        let sock = UdpSocket::bind(if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).await?;
        sock.connect(addr).await?;
        let mut c = Client { sock, addr, token, device_id: 0, stamp: (0, Instant::now()), next_id: rand::random::<u16>() as u64 + 1 };
        c.handshake().await?;
        Ok(c)
    }

    pub fn device_id(&self) -> u32 {
        self.device_id
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    async fn recv_timeout(&self, d: Duration) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 4096];
        let n = tokio::time::timeout(d, self.sock.recv(&mut buf)).await.map_err(|_| anyhow!("le robot ne répond pas (UDP {})", self.addr))??;
        buf.truncate(n);
        Ok(buf)
    }

    async fn handshake(&mut self) -> Result<()> {
        for _ in 0..3 {
            self.sock.send(&hello_packet()).await?;
            if let Ok(data) = self.recv_timeout(Duration::from_millis(1500)).await {
                let h = parse_header(&data)?;
                (self.device_id, self.stamp) = (h.device_id, (h.stamp, Instant::now()));
                return Ok(());
            }
        }
        bail!("le robot ne répond pas à la poignée de main miIO ({}) : adresse fausse, ou robot sans API locale", self.addr)
    }

    /// Appel d'une méthode : `{"id", "method", "params"}` → `result`.
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let mut last = anyhow!("aucun essai");
        for attempt in 0..3 {
            if attempt > 0 {
                // L'appareil a pu redémarrer : nouvelle poignée de main.
                let _ = self.handshake().await;
            }
            self.next_id += 1;
            let id = self.next_id;
            let msg = json!({ "id": id, "method": method, "params": params }).to_string();
            let stamp = self.stamp.0.wrapping_add(self.stamp.1.elapsed().as_secs() as u32 + 1);
            let pkt = encode(&self.token, Header { device_id: self.device_id, stamp }, msg.as_bytes());
            self.sock.send(&pkt).await?;
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    last = anyhow!("le robot ne répond pas à « {method} »");
                    break;
                }
                let data = match self.recv_timeout(left).await {
                    Ok(d) => d,
                    Err(e) => {
                        last = e;
                        break;
                    }
                };
                let (_, plain) = match decode(&self.token, &data) {
                    Ok(x) => x,
                    Err(e) => {
                        last = e;
                        break;
                    }
                };
                let v: Value = serde_json::from_slice(&plain).context("réponse miIO illisible")?;
                if v.get("id").and_then(Value::as_u64) != Some(id) {
                    continue; // réponse à une demande précédente
                }
                if let Some(e) = v.get("error") {
                    bail!("le robot refuse « {method} » : {}", e.get("message").and_then(Value::as_str).unwrap_or(&e.to_string()));
                }
                return Ok(v.get("result").cloned().unwrap_or(Value::Null));
            }
        }
        Err(last)
    }
}

/// Appareils miIO qui répondent à une poignée de main en diffusion : (adresse, identifiant).
pub async fn discover(timeout: Duration) -> Result<Vec<(String, u32)>> {
    let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    sock.set_broadcast(true)?;
    sock.set_nonblocking(true)?;
    sock.bind(&"0.0.0.0:0".parse::<SocketAddr>()?.into())?;
    let sock = UdpSocket::from_std(sock.into())?;
    for _ in 0..2 {
        let _ = sock.send_to(&hello_packet(), ("255.255.255.255", PORT)).await;
    }
    let mut found: Vec<(String, u32)> = Vec::new();
    let end = tokio::time::Instant::now() + timeout;
    let mut buf = [0u8; 512];
    while let Ok(Ok((n, from))) = tokio::time::timeout_at(end, sock.recv_from(&mut buf)).await {
        if let Ok(h) = parse_header(&buf[..n]) {
            let ip = from.ip().to_string();
            if !found.iter().any(|(a, _)| *a == ip) {
                found.push((ip, h.device_id));
            }
        }
    }
    Ok(found)
}

// ---- Robots -------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Roborock,
    Miot,
}

pub fn dialect_of(model: &str) -> Dialect {
    if model.starts_with("roborock.") || model.starts_with("rockrobo.") {
        Dialect::Roborock
    } else {
        Dialect::Miot
    }
}

/// Robot miIO : liaison + dialecte.
pub struct Robot {
    client: Client,
    pub model: String,
    pub dialect: Dialect,
    pub caps: Caps,
    /// Propriétés MIoT absentes de ce modèle (non relues).
    missing: Vec<(u32, u32)>,
}

impl Robot {
    pub async fn connect(host: &str, port: Option<u16>, token: &str) -> Result<Robot> {
        let token = parse_token(token)?;
        let mut client = Client::connect(host, port.unwrap_or(PORT), token).await?;
        let info = client.call("miIO.info", json!([])).await.context("le robot ne donne pas son modèle (jeton incorrect ?)")?;
        let model = info.get("model").and_then(Value::as_str).unwrap_or("").to_string();
        let dialect = dialect_of(&model);
        Ok(Robot { client, model, dialect, caps: Caps::default(), missing: Vec::new() })
    }

    fn did(&self) -> String {
        self.client.device_id().to_string()
    }

    /// État du robot ; met aussi à jour ce qu'il sait faire.
    pub async fn read(&mut self) -> Result<Status> {
        match self.dialect {
            Dialect::Roborock => {
                let st = self.client.call("get_status", json!([])).await?;
                let st = st.as_array().and_then(|a| a.first()).cloned().unwrap_or(st);
                let mut status = roborock_status(&st);
                if let Ok(c) = self.client.call("get_consumable", json!([])).await {
                    status.consumables = roborock_consumables(c.as_array().and_then(|a| a.first()).unwrap_or(&c));
                }
                self.caps = roborock_caps(st.get("fan_power").and_then(Value::as_i64));
                Ok(status)
            }
            Dialect::Miot => {
                let did = self.did();
                let wanted: Vec<(u32, u32)> = MIOT_PROPS.iter().map(|p| (p.0, p.1)).filter(|p| !self.missing.contains(p)).collect();
                let params: Vec<Value> = wanted.iter().map(|(s, p)| json!({ "did": did, "siid": s, "piid": p })).collect();
                let res = self.client.call("get_properties", Value::Array(params)).await?;
                let mut props = Vec::new();
                for r in res.as_array().into_iter().flatten() {
                    let (Some(s), Some(p)) = (r.get("siid").and_then(Value::as_u64), r.get("piid").and_then(Value::as_u64)) else { continue };
                    let key = (s as u32, p as u32);
                    if r.get("code").and_then(Value::as_i64).unwrap_or(0) != 0 {
                        self.missing.push(key);
                        continue;
                    }
                    if let Some(v) = r.get("value") {
                        props.push((key, v.clone()));
                    }
                }
                self.caps = miot_caps(&props);
                Ok(miot_status(&props))
            }
        }
    }

    pub async fn run(&mut self, cmd: &Command) -> Result<()> {
        match self.dialect {
            Dialect::Roborock => {
                let (method, params) = match cmd {
                    Command::Start => ("app_start", json!([])),
                    Command::Pause => ("app_pause", json!([])),
                    Command::Stop => ("app_stop", json!([])),
                    Command::Dock => {
                        // Les Roborock rentrent mieux à l'arrêt : pause d'abord (sans effet s'il l'est déjà).
                        let _ = self.client.call("app_pause", json!([])).await;
                        ("app_charge", json!([]))
                    }
                    Command::Locate => ("find_me", json!([""])),
                    Command::Fan(v) => ("set_custom_mode", json!([v.parse::<i64>().map_err(|_| anyhow!("puissance inconnue : {v}"))?])),
                    _ => bail!("ce réglage n'existe pas sur les Roborock en miIO"),
                };
                let r = self.client.call(method, params).await?;
                if r.as_array().and_then(|a| a.first()).and_then(Value::as_str).is_some_and(|s| s != "ok") {
                    bail!("le robot refuse « {method} » : {r}");
                }
                Ok(())
            }
            Dialect::Miot => {
                let did = self.did();
                let action = |siid: u32, aiid: u32| ("action", json!({ "did": did, "siid": siid, "aiid": aiid, "in": [] }));
                let set = |siid: u32, piid: u32, v: &str| -> Result<(&'static str, Value)> {
                    let n: i64 = v.parse().map_err(|_| anyhow!("valeur inconnue : {v}"))?;
                    Ok(("set_properties", json!([{ "did": did, "siid": siid, "piid": piid, "value": n }])))
                };
                let (method, params) = match cmd {
                    Command::Start => action(2, 1),
                    Command::Pause => action(2, 2),
                    Command::Dock => action(3, 1),
                    Command::Stop => action(4, 2),
                    Command::Locate => action(7, 1),
                    Command::Fan(v) => set(4, 4, v)?,
                    Command::Water(v) => set(4, 5, v)?,
                    Command::Mode(v) => set(4, 23, v)?,
                    Command::Pattern(_) => bail!("ce robot ne propose pas de façon de nettoyer à choisir"),
                };
                let r = self.client.call(method, params).await?;
                let code = r.get("code").or_else(|| r.as_array().and_then(|a| a.first()).and_then(|x| x.get("code"))).and_then(Value::as_i64).unwrap_or(0);
                if code != 0 {
                    bail!("le robot refuse la commande (code {code})");
                }
                Ok(())
            }
        }
    }
}

// ---- Roborock -------------------------------------------------------------------------------------

/// Durées de vie des pièces d'usure Roborock (heures) : brosse principale, latérale, filtre, capteurs.
const ROBOROCK_LIFE: [(&str, &str, f64); 4] =
    [("main_brush_work_time", "Brosse principale", 300.0), ("side_brush_work_time", "Brosse latérale", 200.0), ("filter_work_time", "Filtre", 150.0), ("sensor_dirty_time", "Capteurs", 30.0)];

pub fn roborock_activity(state: i64, battery: Option<u8>) -> Activity {
    match state {
        1 | 4 | 5 | 7 | 11 | 16 | 17 | 18 => Activity::Cleaning,
        2 | 3 | 13 | 14 => Activity::Idle,
        6 | 15 | 26 => Activity::Returning,
        8 if battery == Some(100) => Activity::Docked,
        8 => Activity::Charging,
        100 => Activity::Docked,
        9 | 12 => Activity::Error,
        10 => Activity::Paused,
        22 | 23 => Activity::Station,
        _ => Activity::Unknown,
    }
}

pub fn roborock_error(code: i64) -> Option<VacError> {
    use ErrorKind::*;
    let kind = match code {
        0 => return None,
        1 | 21 => Lidar,
        2 => Bumper,
        3 => Lifted,
        4 => Cliff,
        5 => MainBrush,
        6 | 17 => SideBrush,
        7 => Wheel,
        8 | 16 => Stuck,
        9 => BinMissing,
        10 => Filter,
        11 | 24 => Zone,
        12 => LowBattery,
        13 | 22 => Charging,
        14 => BatteryFault,
        15 => Sensor,
        18 => Fan,
        19 | 23 => Dock,
        254 => BinFull,
        _ => Other,
    };
    Some(VacError::new(code.to_string(), kind))
}

pub fn roborock_status(st: &Value) -> Status {
    let n = |k: &str| st.get(k).and_then(Value::as_i64);
    let battery = n("battery").map(|b| b.clamp(0, 100) as u8);
    let state = n("state").unwrap_or(-1);
    let mut errors: Vec<VacError> = n("error_code").and_then(roborock_error).into_iter().collect();
    let activity = roborock_activity(state, battery);
    if activity == Activity::Error && errors.is_empty() {
        errors.push(VacError::new(state.to_string(), if state == 9 { ErrorKind::Charging } else { ErrorKind::Other }));
    }
    Status {
        activity,
        battery,
        charging: activity == Activity::Charging,
        fan: n("fan_power").map(|f| f.to_string()),
        mode: None,
        water: None,
        area_m2: n("clean_area").map(|mm2| (mm2 as f64 / 1_000_000.0 * 10.0).round() / 10.0),
        clean_min: n("clean_time").map(|s| (s as f64 / 60.0).round() as u32),
        errors,
        consumables: Vec::new(),
    }
}

fn roborock_consumables(c: &Value) -> Vec<Consumable> {
    ROBOROCK_LIFE
        .iter()
        .filter_map(|(k, label, life)| {
            let used_h = c.get(*k).and_then(Value::as_f64)? / 3600.0;
            let left = (life - used_h).max(0.0);
            Some(Consumable { label: label.to_string(), percent: Some((left / life * 100.0).clamp(0.0, 100.0)), hours: Some(left) })
        })
        .collect()
}

/// Puissances : 101‥105 sur les modèles récents, 38/60/75/100 sur les premiers.
pub fn roborock_caps(current_fan: Option<i64>) -> Caps {
    let fans: Vec<Choice> = if current_fan.is_some_and(|f| f <= 100) {
        [(38, "Silencieux"), (60, "Standard"), (75, "Fort"), (100, "Maximum")].iter().map(|(v, l)| Choice::new(v.to_string(), *l)).collect()
    } else {
        [(101, "Silencieux"), (102, "Standard"), (103, "Fort"), (104, "Maximum"), (105, "Sans aspiration (serpillière)")].iter().map(|(v, l)| Choice::new(v.to_string(), *l)).collect()
    };
    Caps { pause: true, stop: true, dock: true, locate: true, fans, ..Default::default() }
}

// ---- MIoT (Dreame…) ---------------------------------------------------------------------------------

/// Propriétés relues : état (2.1), erreur (2.2), batterie (3.1), charge (3.2), durée (4.2),
/// surface (4.3), puissance (4.4), eau (4.5), mode (4.23), usure (9.2, 10.2, 11.1).
const MIOT_PROPS: [(u32, u32, &str); 12] = [
    (2, 1, "state"),
    (2, 2, "error"),
    (3, 1, "battery"),
    (3, 2, "charging"),
    (4, 2, "time"),
    (4, 3, "area"),
    (4, 4, "suction"),
    (4, 5, "water"),
    (4, 23, "mode"),
    (9, 2, "Brosse principale"),
    (10, 2, "Brosse latérale"),
    (11, 1, "Filtre"),
];

fn prop<'a>(props: &'a [((u32, u32), Value)], s: u32, p: u32) -> Option<&'a Value> {
    props.iter().find(|(k, _)| *k == (s, p)).map(|(_, v)| v)
}

pub fn miot_activity(state: i64, charging: Option<i64>) -> Activity {
    match state {
        1 | 7 | 11 | 12 | 15 | 23 | 25 | 26 | 27 | 37 | 38 => Activity::Cleaning,
        2 | 14 | 29 => match charging {
            Some(1) => Activity::Charging,
            Some(3) => Activity::Docked,
            _ => Activity::Idle,
        },
        3 | 21 => Activity::Paused,
        4 => Activity::Error,
        5 | 10 | 17 | 18 | 28 | 31 => Activity::Returning,
        6 | 24 => Activity::Charging,
        13 => Activity::Docked,
        8 | 9 | 16 | 19 | 20 | 22 | 30 | 32 | 33 | 34 | 35 | 36 => Activity::Station,
        _ => Activity::Unknown,
    }
}

/// Codes d'erreur des Dreame (MIoT 2.2).
pub fn miot_error(code: i64) -> Option<VacError> {
    use ErrorKind::*;
    let kind = match code {
        0 => return None,
        1 | 4 | 6 => Lifted,
        2 => Cliff,
        3 | 5 => Bumper,
        7 | 24 | 26 | 27 | 33 | 34 | 35 | 38 | 39 | 40 | 46 | 58 => Sensor,
        8 => BinMissing,
        9 | 10 | 42 | 50 => WaterTank,
        11 => BinFull,
        12 => MainBrush,
        13 => SideBrush,
        14 | 30 => Fan,
        15 | 16 | 31 | 32 => Wheel,
        17 | 18 | 25 | 47 | 54 | 57 | 61 | 62 | 63 | 64 => Stuck,
        19 | 28 => Dock,
        20 => LowBattery,
        21 => Charging,
        22 | 29 => BatteryFault,
        36 | 37 | 41 | 59 | 65 | 66 | 67 => Zone,
        48 | 49 | 56 => Lidar,
        51 => Filter,
        55 | 68 | 69 | 70 => Mop,
        _ => Other,
    };
    Some(VacError::new(code.to_string(), kind))
}

pub fn miot_status(props: &[((u32, u32), Value)]) -> Status {
    let n = |s: u32, p: u32| prop(props, s, p).and_then(Value::as_i64);
    let charging = n(3, 2);
    let activity = n(2, 1).map(|s| miot_activity(s, charging)).unwrap_or(Activity::Unknown);
    let mut errors: Vec<VacError> = n(2, 2).and_then(miot_error).into_iter().collect();
    if activity == Activity::Error && errors.is_empty() {
        errors.push(VacError::new("", ErrorKind::Other));
    }
    let consumables = MIOT_PROPS[9..]
        .iter()
        .filter_map(|(s, p, label)| n(*s, *p).map(|pc| Consumable { label: label.to_string(), percent: Some(pc.clamp(0, 100) as f64), hours: None }))
        .collect();
    Status {
        activity,
        battery: n(3, 1).map(|b| b.clamp(0, 100) as u8),
        charging: charging == Some(1),
        fan: n(4, 4).map(|v| v.to_string()),
        water: n(4, 5).map(|v| v.to_string()),
        mode: n(4, 23).filter(|m| (0..=3).contains(m)).map(|v| v.to_string()),
        area_m2: n(4, 3).map(|a| a as f64),
        clean_min: n(4, 2).map(|m| m.max(0) as u32),
        errors,
        consumables,
    }
}

pub fn miot_caps(props: &[((u32, u32), Value)]) -> Caps {
    let has = |s, p| prop(props, s, p).is_some();
    let list = |items: &[(i64, &str)]| items.iter().map(|(v, l)| Choice::new(v.to_string(), *l)).collect::<Vec<_>>();
    Caps {
        pause: true,
        stop: true,
        dock: true,
        locate: true,
        fans: if has(4, 4) { list(&[(0, "Silencieux"), (1, "Standard"), (2, "Fort"), (3, "Maximum")]) } else { Vec::new() },
        waters: if has(4, 5) { list(&[(1, "Faible"), (2, "Moyen"), (3, "Fort")]) } else { Vec::new() },
        modes: if prop(props, 4, 23).and_then(Value::as_i64).is_some_and(|m| (0..=3).contains(&m)) {
            [(0, "sweeping"), (1, "mopping"), (2, "sweeping_and_mopping"), (3, "mopping_after_sweeping")].iter().map(|(v, k)| Choice::new(v.to_string(), mode_label(k))).collect()
        } else {
            Vec::new()
        },
        patterns: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_roundtrip_and_reject_wrong_token() {
        let token = parse_token("00112233445566778899aabbccddeeff").unwrap();
        assert!(parse_token("1234").is_err() && parse_token(&"zz".repeat(16)).is_err());
        let body = br#"{"id":1,"method":"miIO.info","params":[]}"#;
        let pkt = encode(&token, Header { device_id: 0x0123_4567, stamp: 42 }, body);
        assert_eq!(pkt.len() % 16, 0);
        assert_eq!(&pkt[..2], &MAGIC);
        let (h, plain) = decode(&token, &pkt).unwrap();
        assert_eq!((h.device_id, h.stamp), (0x0123_4567, 42));
        assert_eq!(plain, body);
        let other = parse_token("ffeeddccbbaa99887766554433221100").unwrap();
        assert!(decode(&other, &pkt).is_err());
        assert_eq!(parse_header(&hello_packet()).unwrap().device_id, u32::MAX);
        // Message vide : un seul bloc de bourrage, aller-retour.
        let zero = [0u8; 16];
        assert_eq!(decrypt(&zero, &encrypt(&zero, b"")).unwrap(), b"");
    }

    #[test]
    fn roborock_and_miot_statuses() {
        let st = roborock_status(&json!({ "state": 5, "battery": 72, "fan_power": 102, "clean_time": 1260, "clean_area": 18_500_000, "error_code": 0 }));
        assert_eq!((st.activity, st.battery, st.fan.as_deref(), st.clean_min, st.area_m2), (Activity::Cleaning, Some(72), Some("102"), Some(21), Some(18.5)));
        let st = roborock_status(&json!({ "state": 12, "battery": 50, "error_code": 5 }));
        assert_eq!((st.activity, st.errors[0].kind), (Activity::Error, ErrorKind::MainBrush));
        assert_eq!(roborock_activity(8, Some(100)), Activity::Docked);
        assert_eq!(roborock_caps(Some(60)).fans[0].value, "38");
        let c = roborock_consumables(&json!({ "main_brush_work_time": 540_000, "filter_work_time": 0 }));
        assert_eq!((c[0].hours, c[1].percent), (Some(150.0), Some(100.0)));

        let props = vec![((2, 1), json!(6)), ((2, 2), json!(0)), ((3, 1), json!(55)), ((3, 2), json!(1)), ((4, 4), json!(2)), ((4, 23), json!(2)), ((9, 2), json!(81))];
        let st = miot_status(&props);
        assert_eq!((st.activity, st.battery, st.charging, st.fan.as_deref(), st.mode.as_deref()), (Activity::Charging, Some(55), true, Some("2"), Some("2")));
        assert_eq!(st.consumables[0].percent, Some(81.0));
        let caps = miot_caps(&props);
        assert_eq!((caps.fans.len(), caps.waters.len(), caps.modes[2].label.as_str()), (4, 0, "Aspiration et lavage"));
        let st = miot_status(&[((2, 1), json!(4)), ((2, 2), json!(13))]);
        assert_eq!((st.activity, st.errors[0].kind), (Activity::Error, ErrorKind::SideBrush));
        assert_eq!(miot_activity(2, Some(3)), Activity::Docked);
        assert_eq!(dialect_of("roborock.vacuum.s5"), Dialect::Roborock);
        assert_eq!(dialect_of("dreame.vacuum.p2008"), Dialect::Miot);
    }
}
