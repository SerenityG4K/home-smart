//! Découverte des appareils par leurs annonces UDP.
//!
//! - port 6666 : annonces en clair (3.1) ;
//! - port 6667 : annonces chiffrées AES-ECB avec la clé UDP fixe (3.3/3.4) ;
//! - port 7000 : appareils 3.5, qui répondent à une requête « REQ_DEVINFO ».

use crate::protocol::crypto::{self, Key};
use crate::protocol::frame::{self, DecodeOpts};
use crate::protocol::cmd;
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;

pub const PORTS: [u16; 3] = [6666, 6667, 7000];
const APP_PORT: u16 = 7000;

#[derive(Clone, Debug, Serialize)]
pub struct Discovered {
    pub id: String,
    pub ip: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_key: Option<String>,
    /// Horodatage (ms) de la dernière annonce.
    pub seen: u64,
}

fn udp_key() -> &'static Key {
    static K: OnceLock<Key> = OnceLock::new();
    K.get_or_init(crypto::udp_key)
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Décode une annonce UDP. `None` si ce n'est pas une annonce d'appareil.
pub fn parse_datagram(data: &[u8], from: Option<SocketAddr>) -> Option<Discovered> {
    let opts = DecodeOpts { key: Some(udp_key()), hmac: false, detect_retcode: true };
    let payload = match frame::decode_one(data, opts) {
        Ok(f) => f.payload,
        // Certains firmwares envoient le JSON chiffré sans trame.
        Err(_) => data.to_vec(),
    };
    let mut plain = if payload.first() == Some(&b'{') {
        payload
    } else {
        crypto::ecb_decrypt(udp_key(), &payload, true).ok()?
    };
    while plain.last() == Some(&0) {
        plain.pop();
    }
    let v: Value = serde_json::from_slice(&plain).ok()?;
    let id = v.get("gwId").or_else(|| v.get("devId")).and_then(Value::as_str)?.to_string();
    let ip = v
        .get("ip")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| from.map(|a| a.ip().to_string()))?;
    let version = match v.get("version") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => "3.3".into(),
    };
    let product_key = v.get("productKey").and_then(Value::as_str).map(str::to_string);
    Some(Discovered { id, ip, version, product_key, seen: now_ms() })
}

fn bind_udp(port: u16) -> Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    s.set_broadcast(true)?;
    s.set_nonblocking(true)?;
    s.bind(&SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)).into())?;
    Ok(UdpSocket::from_std(s.into())?)
}

/// Adresse IPv4 locale de l'interface par défaut (aucun paquet n'est émis).
pub fn local_ipv4() -> Option<Ipv4Addr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("10.254.254.254:1").ok()?;
    match s.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// Diffuse une requête « REQ_DEVINFO » : les appareils 3.5 n'annoncent qu'en réponse.
pub async fn send_discovery_request() {
    let Ok(sock) = bind_udp(0) else { return };
    let ip = local_ipv4().unwrap_or(Ipv4Addr::UNSPECIFIED);
    let body = json!({ "from": "app", "ip": ip.to_string() }).to_string();
    let msg = frame::pack_6699(0, cmd::REQ_DEVINFO, None, body.as_bytes(), udp_key());
    let mut targets = vec![Ipv4Addr::BROADCAST];
    if !ip.is_unspecified() {
        let o = ip.octets();
        targets.push(Ipv4Addr::new(o[0], o[1], o[2], 255));
    }
    for t in targets {
        let _ = sock.send_to(&msg, (t, APP_PORT)).await;
    }
}

/// Écoute en continu les annonces et les pousse dans `tx`.
/// Renvoie le nombre de ports effectivement écoutés.
pub fn spawn_listeners(tx: mpsc::UnboundedSender<Discovered>) -> usize {
    let mut ok = 0;
    for port in PORTS {
        match bind_udp(port) {
            Ok(sock) => {
                ok += 1;
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 2048];
                    loop {
                        let Ok((n, from)) = sock.recv_from(&mut buf).await else { continue };
                        if let Some(d) = parse_datagram(&buf[..n], Some(from)) {
                            if tx.send(d).is_err() {
                                return;
                            }
                        }
                    }
                });
            }
            Err(e) => tracing::warn!("écoute UDP {port} impossible : {e}"),
        }
    }
    ok
}

/// Balayage ponctuel du réseau pendant `dur`, ou jusqu'à trouver `stop_at`.
pub async fn scan(dur: Duration, stop_at: Option<&str>) -> Result<Vec<Discovered>> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    if spawn_listeners(tx) == 0 {
        anyhow::bail!("aucun port UDP de découverte n'a pu être ouvert (6666/6667/7000)");
    }
    send_discovery_request().await;
    let deadline = Instant::now() + dur;
    let mut found: HashMap<String, Discovered> = HashMap::new();
    let mut next_ping = Instant::now() + Duration::from_secs(2);
    loop {
        let wake = deadline.min(next_ping);
        match tokio::time::timeout_at(wake, rx.recv()).await {
            Ok(Some(d)) => {
                let done = stop_at == Some(d.id.as_str());
                found.insert(d.id.clone(), d);
                if done {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) if Instant::now() >= deadline => break,
            Err(_) => {
                send_discovery_request().await;
                next_ping = Instant::now() + Duration::from_secs(2);
            }
        }
    }
    let mut list: Vec<_> = found.into_values().collect();
    list.sort_by(|a, b| a.ip.cmp(&b.ip));
    Ok(list)
}

/// Construit une annonce 3.3 (pour le simulateur).
pub fn build_announce(id: &str, ip: &str, version: &str) -> Vec<u8> {
    let body = json!({
        "ip": ip, "gwId": id, "active": 2, "ablilty": 0, "encrypt": true,
        "productKey": "demoProductKey00", "version": version
    })
    .to_string();
    let enc = crypto::ecb_encrypt(udp_key(), body.as_bytes(), true);
    frame::pack_55aa(0, cmd::UDP_NEW, Some(0), &enc, frame::Integrity::Crc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announce_roundtrip() {
        let raw = build_announce("bf1234", "192.168.1.42", "3.4");
        let d = parse_datagram(&raw, None).unwrap();
        assert_eq!((d.id.as_str(), d.ip.as_str(), d.version.as_str()), ("bf1234", "192.168.1.42", "3.4"));
    }

    #[test]
    fn announce_6699_roundtrip() {
        let body = br#"{"ip":"10.0.0.5","gwId":"abc","version":"3.5"}"#;
        let raw = frame::pack_6699(0, 0x44, Some(0), body, udp_key());
        let d = parse_datagram(&raw, None).unwrap();
        assert_eq!((d.id.as_str(), d.version.as_str()), ("abc", "3.5"));
    }

    #[test]
    fn own_request_is_ignored() {
        let msg = frame::pack_6699(0, cmd::REQ_DEVINFO, None, br#"{"from":"app","ip":"1.2.3.4"}"#, udp_key());
        assert!(parse_datagram(&msg, None).is_none());
    }
}
