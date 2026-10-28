//! Recherche des appareils TP-Link sur le réseau local, sans cloud.
//!
//! - UDP 20002 (Tapo, Kasa récentes) : requête « TDP », un en-tête de 16 octets (version 2,
//!   sonde, longueur, drapeaux 17, n° aléatoire, CRC-32 du tout calculé avec 0x5A6B7C8D à sa
//!   place) suivi d'un JSON qui porte une clé publique RSA. La réponse a le même en-tête, puis un
//!   JSON en clair : type, modèle, adresse MAC, chiffrement et port HTTP de l'appareil ;
//! - UDP 9999 (anciennes Kasa) : `get_sysinfo` chiffré par XOR (`kasa`).

use super::kasa;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::{timeout, Instant};

pub const TDP_PORT: u16 = 20002;

/// Clé publique fixe envoyée dans la sonde. L'appareil s'en sert pour chiffrer une clé de session
/// dont on n'a pas besoin : ce qu'on lit de la réponse (type, modèle, adresse) est en clair.
const RSA_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDM+uRjTXjXcY2BOJ9x6NVgfy63\nPk3w3u1svz9BbPI9xh0bhKvvjfMngfoOmdMg4sOZCqPIAEAfUZnnxQNYSrbdET4p\nm8msd1BAznpa+/UP8+e8UT9+P3P2OXy99ystsBf0sOayvAGTv7+LliUWoxrvf7LC\n3N18q//VM//i2bsSnQIDAQAB\n-----END PUBLIC KEY-----\n";

/// Sorte d'appareil TP-Link.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Default)]
pub enum TpKind {
    Plug,
    Strip,
    Bulb,
    Switch,
    Camera,
    Hub,
    #[default]
    Other,
}

impl TpKind {
    /// D'après le type annoncé (`SMART.TAPOPLUG`, `SMART.IPCAMERA`, `IOT.SMARTPLUGSWITCH`…).
    pub fn from_type(t: &str) -> TpKind {
        let t = t.to_ascii_uppercase();
        if t.contains("IPCAMERA") || t.contains("CAMERA") {
            TpKind::Camera
        } else if t.contains("HUB") {
            TpKind::Hub
        } else if t.contains("STRIP") {
            TpKind::Strip
        } else if t.contains("BULB") || t.contains("LIGHT") {
            TpKind::Bulb
        } else if t.contains("PLUG") {
            TpKind::Plug
        } else if t.contains("SWITCH") {
            TpKind::Switch
        } else {
            TpKind::Other
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TpKind::Plug => "prise",
            TpKind::Strip => "multiprise",
            TpKind::Bulb => "ampoule",
            TpKind::Switch => "interrupteur",
            TpKind::Camera => "caméra",
            TpKind::Hub => "passerelle",
            TpKind::Other => "appareil",
        }
    }
}

/// Gamme : Tapo (application Tapo) ou Kasa.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Default)]
pub enum Family {
    #[default]
    Tapo,
    Kasa,
}

/// Appareil TP-Link trouvé sur le réseau.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Found {
    pub ip: String,
    pub family: Family,
    pub kind: TpKind,
    /// Modèle sans la région (« P110 », « C200 », « HS110 »).
    pub model: Option<String>,
    pub mac: Option<String>,
    /// Nom donné dans l'application (anciennes Kasa seulement : les Tapo ne le disent qu'après connexion).
    pub name: Option<String>,
    /// « KLAP », « AES » ou « XOR » (ancien protocole Kasa, sans identifiants).
    pub encryption: String,
    pub http_port: u16,
    pub https: bool,
    /// Jamais installé dans l'application (réglages d'usine).
    pub factory_default: bool,
    /// Allumé (anciennes Kasa seulement).
    pub on: Option<bool>,
}

impl Found {
    pub fn title(&self) -> String {
        let brand = match self.family {
            Family::Tapo => "Tapo",
            Family::Kasa => "Kasa",
        };
        match &self.model {
            Some(m) => format!("{brand} {m} ({})", self.kind.label()),
            None => format!("{} {brand}", capitalize(self.kind.label())),
        }
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn clean_model(m: &str) -> String {
    m.split('(').next().unwrap_or(m).trim().to_string()
}

/// Sonde TDP (port 20002), avec ce n° de requête.
pub fn tdp_query(serial: u32) -> Vec<u8> {
    let payload = json!({ "params": { "rsa_key": RSA_KEY } }).to_string();
    let mut p = Vec::with_capacity(16 + payload.len());
    p.extend_from_slice(&[2, 0]);
    p.extend_from_slice(&1u16.to_be_bytes());
    p.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    p.extend_from_slice(&[17, 0]);
    p.extend_from_slice(&serial.to_be_bytes());
    p.extend_from_slice(&0x5A6B_7C8Du32.to_be_bytes());
    p.extend_from_slice(payload.as_bytes());
    let crc = crc32fast::hash(&p);
    p[12..16].copy_from_slice(&crc.to_be_bytes());
    p
}

/// Réponse TDP (notre propre sonde, renvoyée par la diffusion, est ignorée).
pub fn parse_tdp(data: &[u8], from: Option<Ipv4Addr>) -> Option<Found> {
    let json = data.get(16..)?;
    let v: Value = serde_json::from_slice(json).ok()?;
    let r = v.get("result")?;
    let s = |k: &str| r.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let ip = s("ip").or_else(|| from.map(|a| a.to_string()))?;
    let kind_text = s("device_type").unwrap_or_default();
    let schm = r.get("mgt_encrypt_schm");
    let sv = |k: &str| schm.and_then(|m| m.get(k));
    Some(Found {
        ip,
        family: if kind_text.to_ascii_uppercase().contains("KASA") { Family::Kasa } else { Family::Tapo },
        kind: TpKind::from_type(&kind_text),
        model: s("device_model").map(|m| clean_model(&m)),
        mac: s("mac").map(|m| m.replace('-', ":").to_uppercase()),
        name: None,
        encryption: sv("encrypt_type").and_then(Value::as_str).unwrap_or("KLAP").to_string(),
        http_port: sv("http_port").and_then(Value::as_u64).map(|p| p as u16).unwrap_or(80),
        https: sv("is_support_https").and_then(Value::as_bool).unwrap_or(false),
        factory_default: r.get("factory_default").and_then(Value::as_bool).unwrap_or(false),
        on: None,
    })
}

/// Réponse `get_sysinfo` d'une ancienne Kasa (UDP 9999, chiffrée par XOR).
pub fn parse_legacy(data: &[u8], from: Ipv4Addr) -> Option<Found> {
    let v: Value = serde_json::from_slice(&kasa::decrypt(data)).ok()?;
    let si = v.pointer("/system/get_sysinfo")?;
    let s = |k: &str| si.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let kind_text = s("type").or_else(|| s("mic_type")).unwrap_or_default();
    let on = si.get("relay_state").and_then(Value::as_u64).or_else(|| si.pointer("/light_state/on_off").and_then(Value::as_u64)).map(|v| v == 1);
    Some(Found {
        ip: from.to_string(),
        family: Family::Kasa,
        kind: TpKind::from_type(&kind_text),
        model: s("model").map(|m| clean_model(&m)),
        mac: s("mac").or_else(|| s("mic_mac")).map(|m| m.replace('-', ":").to_uppercase()),
        name: s("alias"),
        encryption: "XOR".into(),
        http_port: kasa::PORT,
        https: false,
        factory_default: false,
        on,
    })
}

/// Recherche (≈ `wait`) : sondes TDP et Kasa en diffusion, réponses fusionnées par adresse.
pub async fn scan(wait: Duration) -> Vec<Found> {
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0").await else { return Vec::new() };
    let _ = sock.set_broadcast(true);
    let tdp = tdp_query(rand::random());
    let legacy = kasa::encrypt(br#"{"system":{"get_sysinfo":{}}}"#);
    let mut targets = vec![Ipv4Addr::BROADCAST];
    if let Some(ip) = crate::camera::discover::local_ipv4() {
        let o = ip.octets();
        targets.push(Ipv4Addr::new(o[0], o[1], o[2], 255));
    }
    for t in &targets {
        let _ = sock.send_to(&tdp, (*t, TDP_PORT)).await;
        let _ = sock.send_to(&legacy, (*t, kasa::PORT)).await;
    }
    let until = Instant::now() + wait;
    let mut by_ip: BTreeMap<Ipv4Addr, Found> = BTreeMap::new();
    let mut buf = vec![0u8; 8192];
    while let Ok(Ok((n, from))) = timeout(until.saturating_duration_since(Instant::now()), sock.recv_from(&mut buf)).await {
        let SocketAddr::V4(from) = from else { continue };
        let found = match from.port() {
            TDP_PORT => parse_tdp(&buf[..n], Some(*from.ip())),
            kasa::PORT => parse_legacy(&buf[..n], *from.ip()),
            _ => None,
        };
        if let Some(f) = found {
            let key = f.ip.parse().unwrap_or(*from.ip());
            by_ip.entry(key).and_modify(|e| merge(e, &f)).or_insert(f);
        }
    }
    by_ip.into_values().collect()
}

fn merge(e: &mut Found, o: &Found) {
    e.model = e.model.take().or(o.model.clone());
    e.mac = e.mac.take().or(o.mac.clone());
    e.name = e.name.take().or(o.name.clone());
    e.on = e.on.or(o.on);
    if e.kind == TpKind::Other {
        e.kind = o.kind;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tdp_probe_layout() {
        let q = tdp_query(0x0102_0304);
        assert_eq!(&q[..4], &[2, 0, 0, 1]);
        let len = u16::from_be_bytes([q[4], q[5]]) as usize;
        assert_eq!(q.len(), 16 + len);
        assert_eq!(&q[6..12], &[17, 0, 1, 2, 3, 4]);
        // CRC-32 calculé avec la valeur de départ à sa place.
        let mut check = q.clone();
        check[12..16].copy_from_slice(&0x5A6B_7C8Du32.to_be_bytes());
        assert_eq!(&q[12..16], &crc32fast::hash(&check).to_be_bytes());
        let v: Value = serde_json::from_slice(&q[16..]).unwrap();
        assert!(v.pointer("/params/rsa_key").and_then(Value::as_str).unwrap().starts_with("-----BEGIN PUBLIC KEY-----"));
        // Notre propre sonde n'est pas une réponse.
        assert!(parse_tdp(&q, None).is_none());
    }

    #[test]
    fn tdp_answers() {
        let answer = |json: &str| {
            let mut p = vec![2, 0, 0, 1, 0, 0, 17, 0, 0, 0, 0, 1, 0, 0, 0, 0];
            p.extend_from_slice(json.as_bytes());
            p
        };
        let plug = answer(r#"{"error_code":0,"result":{"device_id":"8022","device_type":"SMART.TAPOPLUG","device_model":"P110(EU)","ip":"192.168.1.42","mac":"AA-BB-CC-DD-EE-01","factory_default":false,"mgt_encrypt_schm":{"is_support_https":false,"encrypt_type":"KLAP","http_port":80,"lv":2}}}"#);
        let f = parse_tdp(&plug, None).unwrap();
        assert_eq!((f.ip.as_str(), f.kind, f.family, f.model.as_deref()), ("192.168.1.42", TpKind::Plug, Family::Tapo, Some("P110")));
        assert_eq!((f.mac.as_deref(), f.encryption.as_str(), f.http_port), (Some("AA:BB:CC:DD:EE:01"), "KLAP", 80));
        assert_eq!(f.title(), "Tapo P110 (prise)");
        let cam = answer(r#"{"error_code":0,"result":{"device_type":"SMART.IPCAMERA","device_model":"C200","mac":"AA-BB-CC-DD-EE-02","mgt_encrypt_schm":{"is_support_https":true,"encrypt_type":"AES","http_port":443}}}"#);
        let f = parse_tdp(&cam, Some(Ipv4Addr::new(192, 168, 1, 50))).unwrap();
        assert_eq!((f.ip.as_str(), f.kind, f.https), ("192.168.1.50", TpKind::Camera, true));
        let kasa = answer(r#"{"result":{"device_type":"SMART.KASAPLUG","device_model":"KP125M(US)","ip":"192.168.1.43"}}"#);
        assert_eq!(parse_tdp(&kasa, None).unwrap().family, Family::Kasa);
    }

    #[test]
    fn legacy_kasa_answer() {
        let json = r#"{"system":{"get_sysinfo":{"alias":"Lampe du salon","model":"HS110(EU)","mac":"50:C7:BF:00:00:01","type":"IOT.SMARTPLUGSWITCH","relay_state":1,"sw_ver":"1.5.4"}}}"#;
        let f = parse_legacy(&kasa::encrypt(json.as_bytes()), Ipv4Addr::new(192, 168, 1, 44)).unwrap();
        assert_eq!((f.kind, f.family, f.model.as_deref(), f.name.as_deref(), f.on), (TpKind::Plug, Family::Kasa, Some("HS110"), Some("Lampe du salon"), Some(true)));
        assert_eq!(f.encryption, "XOR");
    }
}
