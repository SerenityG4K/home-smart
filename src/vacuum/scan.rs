//! Recherche des aspirateurs robots sur le réseau local, sans rien modifier : robots Valetudo
//! (annonce mDNS `_valetudo._tcp`, confirmée par leur API) et appareils miIO (poignée de main en
//! diffusion sur UDP 54321). Les robots Tuya, eux, sont trouvés par la découverte habituelle.

use super::{miio, valetudo::Valetudo, Family};
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;

/// Un robot trouvé.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub family: Family,
    pub host: String,
    /// Modèle ou description.
    pub label: String,
}

/// Requête mDNS (PTR, réponse directe demandée) pour un service.
fn mdns_query(service: &str) -> Vec<u8> {
    let mut q = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    for part in service.split('.').filter(|p| !p.is_empty()) {
        q.push(part.len() as u8);
        q.extend_from_slice(part.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 12, 0x80, 1]);
    q
}

/// Adresses des machines qui répondent au service mDNS `_valetudo._tcp`.
async fn valetudo_hosts(timeout: Duration) -> Vec<String> {
    let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await else { return Vec::new() };
    let _ = sock.set_multicast_ttl_v4(2);
    let target = SocketAddr::from((Ipv4Addr::new(224, 0, 0, 251), 5353));
    for _ in 0..2 {
        let _ = sock.send_to(&mdns_query("_valetudo._tcp.local"), target).await;
    }
    let mut hosts: Vec<String> = Vec::new();
    let end = tokio::time::Instant::now() + timeout;
    let mut buf = vec![0u8; 4096];
    while let Ok(Ok((n, from))) = tokio::time::timeout_at(end, sock.recv_from(&mut buf)).await {
        let ip = from.ip().to_string();
        if buf[..n].windows(9).any(|w| w == b"_valetudo") && !hosts.contains(&ip) {
            hosts.push(ip);
        }
    }
    hosts
}

/// Cherche pendant `timeout` (les deux recherches en même temps).
pub async fn scan(timeout: Duration) -> Vec<Found> {
    let (valetudo, miio) = tokio::join!(valetudo_hosts(timeout), miio::discover(timeout));
    let mut out = Vec::new();
    for host in valetudo {
        let h = host.clone();
        let info = tokio::task::spawn_blocking(move || Valetudo::new(&h, None, None, None).robot()).await.ok().and_then(Result::ok);
        let label = info.map(|i| i.label()).unwrap_or_else(|| "Robot Valetudo".into());
        out.push(Found { family: Family::Valetudo, host, label });
    }
    for (host, id) in miio.unwrap_or_default() {
        if out.iter().any(|f| f.host == host) {
            continue;
        }
        out.push(Found { family: Family::Miio, host, label: format!("Appareil miIO n° {id} (jeton requis)") });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mdns_query_format() {
        let q = mdns_query("_valetudo._tcp.local");
        assert_eq!(&q[..6], &[0, 0, 0, 0, 0, 1]);
        assert_eq!(q[12], 9);
        assert_eq!(&q[13..22], b"_valetudo");
        // Type PTR, classe IN avec le bit « réponse directe ».
        assert_eq!(&q[q.len() - 4..], &[0, 12, 0x80, 1]);
    }
}
