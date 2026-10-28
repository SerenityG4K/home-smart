//! Recherche des caméras sur le réseau local, sans cloud.
//!
//! Quatre méthodes, lancées en parallèle puis fusionnées par adresse IP :
//! - la découverte Dahua / Imou (`DHDiscover.search`, UDP 37810, multicast 239.255.255.251) :
//!   modèle, n° de série, adresse MAC, firmware ;
//! - la découverte TP-Link (UDP 20002, `tplink::discover`) : les caméras Tapo et leur modèle,
//!   même avant la création de leur compte caméra (sans lequel elles ne répondent pas en ONVIF) ;
//! - WS-Discovery ONVIF (UDP 3702, multicast 239.255.255.250) : toutes les caméras ONVIF ;
//! - mDNS (`_homesmart-cam._tcp`) : les caméras HomeSmart fabriquées soi-même ;
//! - un sondage du port RTSP 554 sur le sous-réseau, pour les caméras qui ne s'annoncent pas.

use super::Brand;
use serde::Serialize;
use serde_json::Value;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket as StdUdp};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::{timeout, Instant};

const DH_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 251);
const DH_PORT: u16 = 37810;
const WSD_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const WSD_PORT: u16 = 3702;

/// Caméra trouvée sur le réseau.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Found {
    pub ip: String,
    pub brand: Brand,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub mac: Option<String>,
    pub firmware: Option<String>,
    /// Nom annoncé (ONVIF) ou fabricant.
    pub vendor: Option<String>,
    pub onvif: bool,
    /// Port ONVIF annoncé, s'il n'est pas 80 (2020 sur les Tapo).
    pub onvif_port: Option<u16>,
    pub rtsp: bool,
    /// Méthodes qui l'ont trouvée.
    pub via: Vec<&'static str>,
    /// Autres adresses de la même caméra (les Dahua / Imou gardent aussi leur IP d'usine 192.168.1.108).
    pub aliases: Vec<String>,
    /// « realm » de l'authentification RTSP : propre à chaque caméra, il sert à reconnaître les alias.
    #[serde(skip)]
    pub realm: Option<String>,
    /// Caméra HomeSmart : port de l'API de son agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_port: Option<u16>,
    /// Port RTSP annoncé, s'il n'est pas 554 (8554 sur une caméra HomeSmart).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtsp_port: Option<u16>,
}

impl Found {
    /// Tapo trouvée par la découverte TP-Link, mais ni vidéo ni ONVIF : son compte caméra
    /// n'existe pas encore (elle n'ouvre RTSP et ONVIF qu'ensuite).
    pub fn needs_tapo_account(&self) -> bool {
        self.brand == Brand::Tapo && !self.rtsp && !self.onvif
    }

    pub fn title(&self) -> String {
        if self.brand == Brand::HomeSmart {
            // Son nom (`vendor`) dit mieux de quelle caméra il s'agit que la carte (`model`).
            return match &self.vendor {
                Some(n) => format!("Caméra HomeSmart « {n} »"),
                None => "Caméra HomeSmart".into(),
            };
        }
        match (&self.model, self.brand) {
            (Some(m), Brand::Generic) => m.clone(),
            (Some(m), b) => format!("{} {m}", b.label()),
            (None, Brand::Generic) if self.onvif => "Caméra ONVIF".into(),
            (None, Brand::Generic) => match &self.vendor {
                Some(v) => format!("Serveur vidéo RTSP ({v})"),
                None => "Serveur vidéo RTSP".into(),
            },
            (None, b) => format!("Caméra {}", b.label()),
        }
    }

    fn merge(&mut self, o: Found) {
        if self.brand == Brand::Generic {
            self.brand = o.brand;
        }
        self.model = self.model.take().or(o.model);
        self.serial = self.serial.take().or(o.serial);
        self.mac = self.mac.take().or(o.mac);
        self.firmware = self.firmware.take().or(o.firmware);
        self.vendor = self.vendor.take().or(o.vendor);
        self.realm = self.realm.take().or(o.realm);
        self.onvif |= o.onvif;
        self.onvif_port = self.onvif_port.or(o.onvif_port);
        self.agent_port = self.agent_port.or(o.agent_port);
        self.rtsp_port = self.rtsp_port.or(o.rtsp_port);
        self.rtsp |= o.rtsp;
        for v in o.via {
            if !self.via.contains(&v) {
                self.via.push(v);
            }
        }
    }
}

// ---- Dahua / Imou -----------------------------------------------------------------

/// Paquet DHIP : en-tête de 32 octets puis JSON.
pub fn dhip_packet(json: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(32 + json.len());
    p.extend_from_slice(&0x20u32.to_le_bytes());
    p.extend_from_slice(b"DHIP");
    p.extend_from_slice(&0u32.to_le_bytes()); // session
    p.extend_from_slice(&0u32.to_le_bytes()); // requête
    p.extend_from_slice(&(json.len() as u32).to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&(json.len() as u32).to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(json);
    p
}

pub fn dh_search_packet() -> Vec<u8> {
    dhip_packet(br#"{"method":"DHDiscover.search","params":{"mac":"","uni":1}}"#)
}

/// Réponse à `DHDiscover.search` (les autres messages, dont notre propre requête, sont ignorés).
pub fn parse_dhip(data: &[u8]) -> Option<Found> {
    let json = if data.len() > 32 && &data[4..8] == b"DHIP" { &data[32..] } else { data };
    let start = json.iter().position(|&b| b == b'{')?;
    let end = json.iter().rposition(|&b| b == b'}')?;
    let v: Value = serde_json::from_slice(&json[start..=end]).ok()?;
    let info = v.get("params")?.get("deviceInfo")?;
    let s = |k: &str| info.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let ip = info.get("IPv4Address").and_then(|a| a.get("IPAddress")).and_then(Value::as_str)?.to_string();
    let vendor = s("Vendor");
    // « LC » = LeChange, l'ancien nom d'Imou ; « Private » est ce qu'annoncent les Imou.
    let brand = match vendor.as_deref() {
        Some("LC") | Some("Imou") | Some("IMOU") => Brand::Imou,
        _ if s("Manufacturer").as_deref() == Some("Private") => Brand::Imou,
        _ => Brand::Dahua,
    };
    let mac = v.get("mac").and_then(Value::as_str).map(str::to_string).or_else(|| s("Mac"));
    Some(Found {
        ip,
        brand,
        model: s("DeviceType").or_else(|| s("DeviceClass")),
        serial: s("SerialNo"),
        mac: mac.map(|m| m.to_uppercase()),
        firmware: s("Version"),
        vendor,
        via: vec!["Dahua/Imou"],
        ..Found::default()
    })
}

// ---- ONVIF WS-Discovery ----------------------------------------------------------------

pub fn wsd_probe(message_id: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope" xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" xmlns:dn="http://www.onvif.org/ver10/network/wsdl"><e:Header><w:MessageID>uuid:{message_id}</w:MessageID><w:To e:mustUnderstand="true">urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To><w:Action e:mustUnderstand="true">http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action></e:Header><e:Body><d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe></e:Body></e:Envelope>"#
    )
}

/// Réponses `ProbeMatch` des émetteurs vidéo (les imprimantes et autres appareils WSD sont écartés).
pub fn parse_probe_matches(body: &str) -> Vec<Found> {
    use super::xml;
    xml::elements(body, "ProbeMatch")
        .into_iter()
        .filter(|m| m.find_text("Types").is_some_and(|t| t.contains("NetworkVideoTransmitter")))
        .filter_map(|m| {
            let xaddrs = m.find_text("XAddrs")?;
            let ip = xaddrs.split_whitespace().find_map(host_of)?;
            let onvif_port = xaddrs.split_whitespace().find(|u| host_of(u).as_deref() == Some(ip.as_str())).and_then(port_of).filter(|&p| p != 80);
            let scopes = m.find_text("Scopes").unwrap_or_default();
            let scope = |key: &str| {
                scopes.split_whitespace().find_map(|s| s.strip_prefix(&format!("onvif://www.onvif.org/{key}/"))).map(|v| {
                    v.replace("%20", " ")
                })
            };
            let name = scope("name");
            // Imou annonce « hardware/<modèle>_<n° de série> ».
            let (model, serial) = match scope("hardware") {
                Some(h) => match h.split_once('_') {
                    Some((m, s)) => (Some(m.to_string()), Some(s.to_string())),
                    None => (Some(h), None),
                },
                None => (None, None),
            };
            // Les Tapo s'annoncent « TP-IPC » et servent ONVIF sur le port 2020, une signature à elles.
            let brand = name
                .as_deref()
                .and_then(Brand::from_vendor)
                .or_else(|| scope("manufacturer").as_deref().and_then(Brand::from_vendor))
                .or_else(|| (onvif_port == Some(2020)).then_some(Brand::Tapo))
                .unwrap_or(Brand::Generic);
            // Le nom « LC » (LeChange) d'Imou n'apprend rien : seul le modèle sert au titre.
            Some(Found { ip, brand, model, serial, onvif: true, onvif_port, via: vec!["ONVIF"], ..Found::default() })
        })
        .collect()
}

fn port_of(url: &str) -> Option<u16> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split('/').next()?;
    authority.rsplit_once(':').and_then(|(_, p)| p.parse().ok())
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let host = rest.split(['/', ':']).next()?;
    host.parse::<Ipv4Addr>().ok().map(|ip| ip.to_string())
}

// ---- Réseau -------------------------------------------------------------------------

/// Adresse IPv4 locale utilisée pour sortir vers le réseau (aucun paquet n'est envoyé).
pub fn local_ipv4() -> Option<Ipv4Addr> {
    let s = StdUdp::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    match s.local_addr().ok()? {
        SocketAddr::V4(a) if !a.ip().is_loopback() && !a.ip().is_unspecified() => Some(*a.ip()),
        _ => None,
    }
}

fn udp_socket(port: u16, join: Option<(Ipv4Addr, Ipv4Addr)>) -> std::io::Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    s.set_broadcast(true)?;
    s.set_multicast_ttl_v4(2)?;
    s.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())?;
    if let Some((group, iface)) = join {
        let _ = s.join_multicast_v4(&group, &iface);
        let _ = s.set_multicast_if_v4(&iface);
    }
    s.set_nonblocking(true)?;
    UdpSocket::from_std(s.into())
}

async fn collect(sock: &UdpSocket, until: Instant, mut on: impl FnMut(&[u8], SocketAddr)) {
    let mut buf = vec![0u8; 65536];
    while let Ok(Ok((n, from))) = timeout(until.saturating_duration_since(Instant::now()), sock.recv_from(&mut buf)).await {
        on(&buf[..n], from);
    }
}

pub async fn search_dahua(iface: Ipv4Addr, wait: Duration) -> Vec<Found> {
    // Les caméras Dahua répondent sur le groupe multicast, port 37810 : il faut l'écouter.
    let sock = match udp_socket(DH_PORT, Some((DH_GROUP, iface))).or_else(|_| udp_socket(0, Some((DH_GROUP, iface)))) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("découverte Dahua impossible : {e}");
            return Vec::new();
        }
    };
    let pkt = dh_search_packet();
    let _ = sock.send_to(&pkt, (DH_GROUP, DH_PORT)).await;
    let _ = sock.send_to(&pkt, (Ipv4Addr::BROADCAST, DH_PORT)).await;
    let mut out = Vec::new();
    collect(&sock, Instant::now() + wait, |data, _| {
        if let Some(f) = parse_dhip(data) {
            out.push(f);
        }
    })
    .await;
    out
}

async fn search_onvif(iface: Ipv4Addr, wait: Duration) -> Vec<Found> {
    let sock = match udp_socket(0, None) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let s2 = socket2::SockRef::from(&sock);
    let _ = s2.set_multicast_if_v4(&iface);
    let id = format!("{:032x}", rand::random::<u128>());
    let id = format!("{}-{}-{}-{}-{}", &id[0..8], &id[8..12], &id[12..16], &id[16..20], &id[20..32]);
    let _ = sock.send_to(wsd_probe(&id).as_bytes(), (WSD_GROUP, WSD_PORT)).await;
    let mut out = Vec::new();
    collect(&sock, Instant::now() + wait, |data, _| {
        out.extend(parse_probe_matches(&String::from_utf8_lossy(data)));
    })
    .await;
    out
}

/// Le port est-il ouvert ? (connexion TCP simple, refermée aussitôt)
pub async fn port_open(ip: &str, port: u16, wait: Duration) -> bool {
    matches!(timeout(wait, TcpStream::connect((ip, port))).await, Ok(Ok(_)))
}

/// Réponse d'un serveur RTSP à OPTIONS : en-tête `Server` et « realm » de l'authentification.
pub async fn rtsp_hello(ip: &str, port: u16) -> Option<(Option<String>, Option<String>)> {
    let mut s = timeout(Duration::from_secs(2), TcpStream::connect((ip, port))).await.ok()?.ok()?;
    let req = format!("OPTIONS rtsp://{ip}:{port}/ RTSP/1.0\r\nCSeq: 1\r\nUser-Agent: HomeSmart\r\n\r\n");
    s.write_all(req.as_bytes()).await.ok()?;
    let mut buf = vec![0u8; 2048];
    let n = timeout(Duration::from_secs(2), s.read(&mut buf)).await.ok()?.ok()?;
    let text = String::from_utf8_lossy(&buf[..n]).to_string();
    if !text.starts_with("RTSP/") {
        return None;
    }
    let header = |name: &str| text.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_string()));
    let realm = header("www-authenticate").and_then(|a| {
        let i = a.find("realm=\"")? + 7;
        a[i..].find('"').map(|j| a[i..i + j].to_string())
    });
    Some((header("server").filter(|s| !s.is_empty()), realm))
}

async fn sweep_rtsp(iface: Ipv4Addr) -> Vec<Found> {
    let o = iface.octets();
    let mut tasks = Vec::new();
    for last in 1..=254u8 {
        if last == o[3] {
            continue;
        }
        let ip = Ipv4Addr::new(o[0], o[1], o[2], last).to_string();
        tasks.push(tokio::spawn(async move { port_open(&ip, 554, Duration::from_millis(700)).await.then_some(ip) }));
    }
    let mut out = Vec::new();
    for t in tasks {
        if let Ok(Some(ip)) = t.await {
            let (server, realm) = rtsp_hello(&ip, 554).await.unwrap_or_default();
            out.push(Found { ip, brand: Brand::Generic, rtsp: true, vendor: server, realm, via: vec!["RTSP"], ..Found::default() });
        }
    }
    out
}

// ---- Caméras HomeSmart (mDNS) ------------------------------------------------------------

/// Caméras HomeSmart qui s'annoncent sur le réseau pendant `wait`.
pub async fn search_homesmart(wait: Duration) -> Vec<Found> {
    tokio::task::spawn_blocking(move || browse_homesmart(wait)).await.unwrap_or_default()
}

fn browse_homesmart(wait: Duration) -> Vec<Found> {
    let Ok(daemon) = mdns_sd::ServiceDaemon::new() else { return Vec::new() };
    let mut out: Vec<Found> = Vec::new();
    if let Ok(rx) = daemon.browse(homesmart_cam_proto::SERVICE_TYPE) {
        let deadline = std::time::Instant::now() + wait;
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(mdns_sd::ServiceEvent::ServiceResolved(s)) => {
                    let Some(f) = from_mdns(&s) else { continue };
                    // Une même caméra peut répondre sur plusieurs interfaces (dont la boucle
                    // locale) : une entrée par identifiant, avec la meilleure adresse.
                    match out.iter_mut().find(|o| o.ip == f.ip || (f.serial.is_some() && o.serial == f.serial)) {
                        Some(o) if ip_rank(&f.ip) < ip_rank(&o.ip) => *o = f,
                        Some(_) => {}
                        None => out.push(f),
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
    let _ = daemon.shutdown();
    out
}

/// Préférence d'une adresse annoncée (plus petit = meilleur) : réseau local, puis lien local,
/// puis boucle locale (la caméra tourne sur ce PC, en essai).
fn ip_rank(ip: &str) -> (u8, String) {
    let rank = match ip.parse::<Ipv4Addr>() {
        Ok(a) if a.is_loopback() => 2,
        Ok(a) if a.is_link_local() => 1,
        _ => 0,
    };
    (rank, ip.to_string())
}

/// Annonce mDNS d'un agent `homesmart-cam` → caméra trouvée.
pub fn from_mdns(s: &mdns_sd::ResolvedService) -> Option<Found> {
    let mut ips: Vec<String> = s.get_addresses_v4().into_iter().map(|ip| ip.to_string()).collect();
    ips.sort_by_key(|ip| ip_rank(ip));
    let ip = ips.into_iter().next()?;
    let prop = |k: &str| s.txt_properties.get_property_val_str(k).map(str::to_string).filter(|v| !v.is_empty());
    Some(Found {
        ip,
        brand: Brand::HomeSmart,
        model: prop("board"),
        serial: prop("id"),
        firmware: prop("ver"),
        vendor: prop("name"),
        rtsp: true,
        rtsp_port: prop("rtsp").and_then(|p| p.parse().ok()),
        agent_port: Some(s.get_port()),
        via: vec!["mDNS"],
        ..Found::default()
    })
}

/// Recherche complète (≈ 3 s). Résultats triés : caméras identifiées d'abord.
pub async fn scan(wait: Duration) -> Vec<Found> {
    let Some(iface) = local_ipv4() else { return Vec::new() };
    let (dh, onvif, rtsp, tp, hs) = tokio::join!(search_dahua(iface, wait), search_onvif(iface, wait), sweep_rtsp(iface), crate::tplink::discover::scan(wait), search_homesmart(wait));
    let tapo = tp.into_iter().filter(|t| t.kind == crate::tplink::discover::TpKind::Camera).map(|t| Found {
        ip: t.ip,
        brand: Brand::Tapo,
        model: t.model,
        mac: t.mac,
        via: vec!["TP-Link"],
        ..Found::default()
    });
    let mut by_ip: BTreeMap<Ipv4Addr, Found> = BTreeMap::new();
    for f in hs.into_iter().chain(dh).chain(tapo).chain(onvif).chain(rtsp) {
        let Ok(key) = f.ip.parse::<Ipv4Addr>() else { continue };
        match by_ip.get_mut(&key) {
            Some(e) => e.merge(f),
            None => {
                by_ip.insert(key, f);
            }
        }
    }
    let mut list: Vec<Found> = by_ip.into_values().collect();
    // Port RTSP des caméras identifiées par annonce (le balayage a pu les manquer).
    for f in list.iter_mut().filter(|f| !f.rtsp) {
        f.rtsp = port_open(&f.ip, 554, Duration::from_millis(800)).await;
    }
    for f in list.iter_mut().filter(|f| f.rtsp && f.realm.is_none() && f.agent_port.is_none()) {
        f.realm = rtsp_hello(&f.ip, 554).await.and_then(|(_, r)| r);
    }
    let list = fold_aliases(list);
    let mut list = list;
    list.sort_by_key(|f| (f.model.is_none(), !f.onvif));
    list
}

/// Une adresse qui ne fait que répondre en RTSP avec le même « realm » qu'une caméra identifiée
/// est la même caméra : elle est rangée dans ses alias au lieu d'apparaître deux fois.
fn fold_aliases(list: Vec<Found>) -> Vec<Found> {
    let (named, anonymous): (Vec<Found>, Vec<Found>) = list.into_iter().partition(|f| f.model.is_some() || f.onvif || f.agent_port.is_some());
    let mut named = named;
    let mut out_anon = Vec::new();
    for a in anonymous {
        match named.iter_mut().find(|n| a.realm.is_some() && n.realm == a.realm) {
            Some(n) => n.aliases.push(a.ip),
            None => out_anon.push(a),
        }
    }
    named.extend(out_anon);
    named
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dahua_answer() {
        let json = br#"{"mac":"a8:31:62:00:00:01","method":"client.notifyDevInfo","params":{"deviceInfo":{"DeviceClass":"IPC","DeviceType":"IPC-K2EC-3H1W","HttpPort":80,"Port":37777,"IPv4Address":{"DefaultGateway":"192.168.1.254","DhcpEnable":true,"IPAddress":"192.168.1.164","SubnetMask":"255.255.255.0"},"Manufacturer":"Private","SerialNo":"A1B2C3D4E5F6789","Vendor":"LC","Version":"2.800.0000000.22.R"}}}"#;
        let f = parse_dhip(&dhip_packet(json)).unwrap();
        assert_eq!(f.ip, "192.168.1.164");
        assert_eq!(f.brand, Brand::Imou);
        assert_eq!(f.model.as_deref(), Some("IPC-K2EC-3H1W"));
        assert_eq!(f.serial.as_deref(), Some("A1B2C3D4E5F6789"));
        assert_eq!(f.mac.as_deref(), Some("A8:31:62:00:00:01"));
        // Notre propre requête, renvoyée par le multicast, est ignorée.
        assert!(parse_dhip(&dh_search_packet()).is_none());
        assert_eq!(dh_search_packet().len(), 32 + 58);
    }

    /// Un agent `homesmart-cam` qui s'annonce (vraie annonce mDNS sur cette machine) est trouvé.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn homesmart_camera_found_by_mdns() {
        use homesmart_cam::api::Shared;
        use homesmart_cam::config::{AgentConfig, Backend, Tools};
        use homesmart_cam_proto::{Caps, VideoCodec};
        let id = format!("{:012x}", rand::random::<u64>() & 0xffff_ffff_ffff);
        let caps = Caps { max_width: 3840, max_height: 2160, max_fps: 30, codecs: vec![VideoCodec::H265], audio: true, sensor: None };
        let cfg = AgentConfig {
            id: id.clone(),
            setup_code: "ABCDEFGH".into(),
            clients: Vec::new(),
            backend: Backend::Test,
            settings: caps.default_settings("Couloir"),
            video_device: None,
            audio_device: None,
            has_mic: None,
            caps: None,
            api_port: 18780,
            rtsp_port: 18554,
            tools: Tools::default(),
            custom: Vec::new(),
        };
        let shared = Shared { cfg: std::sync::Mutex::new(cfg), path: std::env::temp_dir().join("hs-mdns-test.json"), caps, board: "Radxa ROCK 4D".into(), started: std::time::Instant::now() };
        let announcer = homesmart_cam::mdns::Announcer::new().unwrap();
        announcer.publish(&shared).unwrap();
        let found = search_homesmart(Duration::from_secs(4)).await;
        let f = found.iter().find(|f| f.serial.as_deref() == Some(id.as_str())).unwrap_or_else(|| panic!("pas trouvée : {found:?}"));
        assert_eq!((f.brand, f.agent_port, f.rtsp_port, f.rtsp), (Brand::HomeSmart, Some(18780), Some(18554), true));
        assert_eq!(f.title(), "Caméra HomeSmart « Couloir »");
        assert_eq!(f.model.as_deref(), Some("Radxa ROCK 4D"));
        drop(announcer);
    }

    #[test]
    fn aliases_folded_by_realm() {
        let cam = Found { ip: "192.168.1.164".into(), model: Some("IPC-K2EC-3H1W".into()), rtsp: true, realm: Some("Login to 0F84".into()), ..Found::default() };
        let alias = Found { ip: "192.168.1.108".into(), brand: Brand::Generic, rtsp: true, realm: Some("Login to 0F84".into()), ..Found::default() };
        let other = Found { ip: "192.168.1.254".into(), brand: Brand::Generic, rtsp: true, vendor: Some("Freebox".into()), ..Found::default() };
        let l = fold_aliases(vec![alias, cam, other]);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].aliases, vec!["192.168.1.108".to_string()]);
        assert_eq!(l[1].title(), "Serveur vidéo RTSP (Freebox)");
    }

    #[test]
    fn onvif_probe_match() {
        let x = r#"<s:Envelope><s:Body><d:ProbeMatches><d:ProbeMatch><a:EndpointReference><a:Address>uuid:x</a:Address></a:EndpointReference><d:Types>dn:NetworkVideoTransmitter tds:Device</d:Types><d:Scopes>onvif://www.onvif.org/location/country/china onvif://www.onvif.org/name/LC onvif://www.onvif.org/hardware/IPC-K2EC-3H1W_A1B2C3D4E5F6789 onvif://www.onvif.org/Profile/Streaming</d:Scopes><d:XAddrs>http://192.168.1.164/onvif/device_service</d:XAddrs></d:ProbeMatch></d:ProbeMatches></s:Body></s:Envelope>"#;
        let f = parse_probe_matches(x);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].ip, "192.168.1.164");
        assert_eq!(f[0].brand, Brand::Imou);
        assert_eq!(f[0].model.as_deref(), Some("IPC-K2EC-3H1W"));
        assert_eq!(f[0].serial.as_deref(), Some("A1B2C3D4E5F6789"));
        // Une Tapo : nom « TP-IPC », ONVIF sur le port 2020…
        let tapo = r#"<d:ProbeMatch><d:Types>dn:NetworkVideoTransmitter</d:Types><d:Scopes>onvif://www.onvif.org/name/TP-IPC onvif://www.onvif.org/hardware/C200 onvif://www.onvif.org/Profile/Streaming</d:Scopes><d:XAddrs>http://192.168.1.50:2020/onvif/device_service</d:XAddrs></d:ProbeMatch>"#;
        let f = parse_probe_matches(tapo);
        assert_eq!((f[0].brand, f[0].onvif_port, f[0].model.as_deref()), (Brand::Tapo, Some(2020), Some("C200")));
        assert_eq!(f[0].title(), "TP-Link Tapo C200");
        // … reconnue au port seul si elle ne dit pas son nom.
        let quiet = tapo.replace("onvif://www.onvif.org/name/TP-IPC ", "");
        assert_eq!(parse_probe_matches(&quiet)[0].brand, Brand::Tapo);
        // Le port 80 n'est pas retenu (valeur par défaut).
        assert_eq!(parse_probe_matches(x)[0].onvif_port, None);
        // Une imprimante qui répond aussi à WS-Discovery n'est pas une caméra.
        let printer = r#"<wsd:ProbeMatch><wsd:Types>wsdp:Device wprt:PrintDeviceType</wsd:Types><wsd:XAddrs>http://192.168.1.56:3911/</wsd:XAddrs></wsd:ProbeMatch>"#;
        assert!(parse_probe_matches(printer).is_empty());
    }
}
