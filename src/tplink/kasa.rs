//! Ancien protocole local des prises Kasa : JSON chiffré par XOR « à clé roulante » (clé de
//! départ 171, chaque octet chiffré sert de clé au suivant). En TCP sur le port 9999, la longueur
//! (4 octets, gros-boutiste) précède le message ; en UDP (découverte), il est seul. Aucun
//! identifiant : ces prises répondent à tout le réseau local.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

pub const PORT: u16 = 9999;

pub fn encrypt(plain: &[u8]) -> Vec<u8> {
    let mut key = 171u8;
    plain
        .iter()
        .map(|&b| {
            key ^= b;
            key
        })
        .collect()
}

pub fn decrypt(data: &[u8]) -> Vec<u8> {
    let mut key = 171u8;
    data.iter()
        .map(|&c| {
            let b = key ^ c;
            key = c;
            b
        })
        .collect()
}

/// Requête JSON à une prise Kasa (port 9999).
pub fn request(host: &str, req: &Value) -> Result<Value> {
    request_at(host, PORT, req)
}

pub fn request_at(host: &str, port: u16, req: &Value) -> Result<Value> {
    let addr = (host, port).to_socket_addrs()?.next().ok_or_else(|| anyhow!("adresse {host} invalide"))?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).with_context(|| format!("connexion à {host}:{port}"))?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let msg = encrypt(req.to_string().as_bytes());
    s.write_all(&(msg.len() as u32).to_be_bytes())?;
    s.write_all(&msg)?;
    let mut len = [0u8; 4];
    s.read_exact(&mut len).context("pas de réponse de la prise")?;
    let len = u32::from_be_bytes(len) as usize;
    if len > 1 << 20 {
        bail!("réponse démesurée ({len} octets)");
    }
    let mut data = vec![0u8; len];
    s.read_exact(&mut data)?;
    serde_json::from_slice(&decrypt(&data)).context("réponse illisible")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xor_round_trip() {
        let m = br#"{"system":{"get_sysinfo":{}}}"#;
        let e = encrypt(m);
        assert_eq!(e[0], 171 ^ b'{');
        assert_ne!(&e[..], &m[..]);
        assert_eq!(decrypt(&e), m);
    }
}
