//! KLAP : le protocole local chiffré des Tapo (et des Kasa récentes), sur HTTP.
//!
//! Poignée de main en deux temps : `POST /app/handshake1` avec 16 octets aléatoires (graine
//! locale) ; l'appareil répond sa graine (16 octets) et SHA-256(graine locale ‖ graine distante ‖
//! empreinte), ce qui prouve qu'il connaît les identifiants. Puis `POST /app/handshake2` avec
//! SHA-256(graine distante ‖ graine locale ‖ empreinte), et le cookie `TP_SESSIONID` reçu.
//!
//! Empreinte des identifiants : SHA-256(SHA-1(e-mail) ‖ SHA-1(mot de passe)) ; les premiers
//! firmwares (v1) prennent MD5(MD5(e-mail) ‖ MD5(mot de passe)). L'appareil ne contacte pas le
//! cloud pour autant : il garde cette empreinte depuis son installation dans l'application.
//!
//! Ensuite chaque requête JSON est chiffrée en AES-128-CBC (bourrage PKCS#7), précédée d'une
//! signature SHA-256 de 32 octets et numérotée (`/app/request?seq=…`). Clé, vecteur et numéro de
//! départ se déduisent des deux graines et de l'empreinte (`Cipher::new`).

use aes::cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use anyhow::{anyhow, bail, Context, Result};
use md5::Md5;
use rand::RngCore;
use serde_json::{json, Value};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::time::Duration;

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// Empreinte des identifiants, firmwares actuels (v2).
pub fn auth_hash_v2(user: &str, password: &str) -> Vec<u8> {
    let (u, p) = (Sha1::digest(user.as_bytes()), Sha1::digest(password.as_bytes()));
    sha256(&[&u, &p]).to_vec()
}

/// Empreinte des identifiants, premiers firmwares (v1).
pub fn auth_hash_v1(user: &str, password: &str) -> Vec<u8> {
    let (u, p) = (Md5::digest(user.as_bytes()), Md5::digest(password.as_bytes()));
    let mut h = Md5::new();
    h.update(u);
    h.update(p);
    h.finalize().to_vec()
}

/// Ce que l'appareil renvoie à `handshake1` pour prouver qu'il connaît l'empreinte.
pub fn server_proof(local: &[u8], remote: &[u8], auth: &[u8]) -> [u8; 32] {
    sha256(&[local, remote, auth])
}

/// Ce qu'on envoie à `handshake2` pour prouver qu'on la connaît aussi.
pub fn client_proof(local: &[u8], remote: &[u8], auth: &[u8]) -> [u8; 32] {
    sha256(&[remote, local, auth])
}

/// Chiffrement d'une session KLAP.
#[derive(Clone)]
pub struct Cipher {
    key: [u8; 16],
    iv: [u8; 12],
    seq: i32,
    sig: [u8; 28],
}

impl Cipher {
    pub fn new(local: &[u8], remote: &[u8], auth: &[u8]) -> Cipher {
        let lsk = sha256(&[b"lsk", local, remote, auth]);
        let iv = sha256(&[b"iv", local, remote, auth]);
        let ldk = sha256(&[b"ldk", local, remote, auth]);
        let mut c = Cipher { key: [0; 16], iv: [0; 12], seq: i32::from_be_bytes([iv[28], iv[29], iv[30], iv[31]]), sig: [0; 28] };
        c.key.copy_from_slice(&lsk[..16]);
        c.iv.copy_from_slice(&iv[..12]);
        c.sig.copy_from_slice(&ldk[..28]);
        c
    }

    fn iv_for(&self, seq: i32) -> [u8; 16] {
        let mut iv = [0u8; 16];
        iv[..12].copy_from_slice(&self.iv);
        iv[12..].copy_from_slice(&seq.to_be_bytes());
        iv
    }

    /// Chiffre et signe un message avec ce numéro : signature (32 octets) puis données.
    pub fn seal(&self, seq: i32, plain: &[u8]) -> Vec<u8> {
        let ct = cbc_encrypt(&self.key, &self.iv_for(seq), plain);
        let sig = sha256(&[&self.sig, &seq.to_be_bytes(), &ct]);
        let mut out = sig.to_vec();
        out.extend_from_slice(&ct);
        out
    }

    /// Message suivant : numéro incrémenté, données scellées.
    pub fn encrypt(&mut self, plain: &[u8]) -> (i32, Vec<u8>) {
        self.seq = self.seq.wrapping_add(1);
        (self.seq, self.seal(self.seq, plain))
    }

    /// Ouvre un message (la signature est ignorée, comme le fait l'application).
    pub fn decrypt(&self, seq: i32, body: &[u8]) -> Result<Vec<u8>> {
        if body.len() < 32 + 16 {
            bail!("réponse chiffrée trop courte ({} octets)", body.len());
        }
        cbc_decrypt(&self.key, &self.iv_for(seq), &body[32..])
    }
}

pub(crate) fn cbc_encrypt(key: &[u8; 16], iv: &[u8; 16], plain: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let pad = 16 - plain.len() % 16;
    let mut data = plain.to_vec();
    data.extend(std::iter::repeat_n(pad as u8, pad));
    let mut prev = *iv;
    for block in data.chunks_exact_mut(16) {
        for (b, p) in block.iter_mut().zip(prev) {
            *b ^= p;
        }
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
        prev.copy_from_slice(block);
    }
    data
}

pub(crate) fn cbc_decrypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Result<Vec<u8>> {
    if data.is_empty() || data.len() % 16 != 0 {
        bail!("données chiffrées de taille invalide");
    }
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = data.to_vec();
    let mut prev = *iv;
    for block in out.chunks_exact_mut(16) {
        let saved: [u8; 16] = block.try_into().unwrap();
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
        for (b, p) in block.iter_mut().zip(prev) {
            *b ^= p;
        }
        prev = saved;
    }
    let pad = *out.last().unwrap() as usize;
    if pad == 0 || pad > 16 || out[out.len() - pad..].iter().any(|&b| b as usize != pad) {
        bail!("déchiffrement impossible (mauvaise clé ?)");
    }
    out.truncate(out.len() - pad);
    Ok(out)
}

/// Codes d'erreur des appareils Tapo, en clair.
fn error_text(code: i64) -> String {
    match code {
        -1501 => "identifiants refusés par l'appareil".into(),
        -1002 | -1003 | -1008 | -1010 => format!("requête refusée par l'appareil (code {code})"),
        -1 => "fonction absente de cet appareil".into(),
        c => format!("l'appareil refuse (code {c})"),
    }
}

/// Session KLAP ouverte avec un appareil.
pub struct Klap {
    base: String,
    cookie: String,
    cipher: Cipher,
    agent: ureq::Agent,
    terminal: String,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(6))).http_status_as_error(false).build().into()
}

impl Klap {
    /// Poignée de main avec l'appareil (`port` : son port HTTP, 80 d'habitude).
    pub fn connect(host: &str, port: u16, user: &str, password: &str) -> Result<Klap> {
        let base = if port == 80 { format!("http://{host}/app") } else { format!("http://{host}:{port}/app") };
        let agent = agent();
        let mut local = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut local);
        let mut r = agent.post(format!("{base}/handshake1")).send(&local[..]).with_context(|| format!("connexion à {host}"))?;
        let status = r.status().as_u16();
        if status != 200 {
            bail!("l'appareil ne parle pas KLAP (HTTP {status})");
        }
        let cookie = r
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|v| v.split(';').map(str::trim).find(|p| p.starts_with("TP_SESSIONID=")).map(str::to_string))
            .unwrap_or_default();
        let body = r.body_mut().read_to_vec()?;
        if body.len() < 48 {
            bail!("réponse de poignée de main trop courte ({} octets)", body.len());
        }
        let (remote, proof) = (&body[..16], &body[16..48]);
        let auth = [auth_hash_v2(user, password), auth_hash_v1(user, password)]
            .into_iter()
            .find(|a| server_proof(&local, remote, a) == proof)
            .ok_or_else(|| anyhow!("identifiants TP-Link refusés par l'appareil (e-mail ou mot de passe du compte Tapo)"))?;
        let mut req = agent.post(format!("{base}/handshake2"));
        if !cookie.is_empty() {
            req = req.header("Cookie", &cookie);
        }
        let status = req.send(&client_proof(&local, remote, &auth)[..]).context("poignée de main")?.status().as_u16();
        if status != 200 {
            bail!("poignée de main refusée par l'appareil (HTTP {status})");
        }
        let mut id = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut id);
        let terminal = id.iter().map(|b| format!("{b:02X}")).collect();
        Ok(Klap { base, cookie, cipher: Cipher::new(&local, remote, &auth), agent, terminal })
    }

    /// Envoie une méthode (`get_device_info`, `set_device_info`…) et renvoie son `result`.
    pub fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let mut req = json!({ "method": method, "requestTimeMils": now, "terminalUUID": self.terminal });
        if let Some(p) = params {
            req["params"] = p;
        }
        let (seq, body) = self.cipher.encrypt(req.to_string().as_bytes());
        let mut r = self.agent.post(format!("{}/request?seq={seq}", self.base));
        if !self.cookie.is_empty() {
            r = r.header("Cookie", &self.cookie);
        }
        let mut resp = r.send(&body[..]).context("requête à l'appareil")?;
        let status = resp.status().as_u16();
        if status != 200 {
            bail!("l'appareil refuse la requête (HTTP {status}{})", if status == 403 { ", session expirée" } else { "" });
        }
        let data = resp.body_mut().read_to_vec()?;
        let plain = self.cipher.decrypt(seq, &data)?;
        let v: Value = serde_json::from_slice(&plain).context("réponse illisible")?;
        match v.get("error_code").and_then(Value::as_i64).unwrap_or(0) {
            0 => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
            c => Err(anyhow!("{}", error_text(c))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cipher_round_trip_and_numbering() {
        let (local, remote) = ([1u8; 16], [2u8; 16]);
        let auth = auth_hash_v2("moi@exemple.fr", "secret");
        assert_eq!(auth.len(), 32);
        assert_eq!(auth_hash_v1("moi@exemple.fr", "secret").len(), 16);
        let mut a = Cipher::new(&local, &remote, &auth);
        let b = Cipher::new(&local, &remote, &auth);
        let start = a.seq;
        let (seq, sealed) = a.encrypt(br#"{"method":"get_device_info"}"#);
        assert_eq!(seq, start.wrapping_add(1));
        assert_eq!(b.decrypt(seq, &sealed).unwrap(), br#"{"method":"get_device_info"}"#);
        // Un autre numéro, ou d'autres graines : illisible.
        assert!(b.decrypt(seq + 1, &sealed).map(|p| p != br#"{"method":"get_device_info"}"#).unwrap_or(true));
        let other = Cipher::new(&local, &[3u8; 16], &auth);
        assert!(other.decrypt(seq, &sealed).map(|p| p != br#"{"method":"get_device_info"}"#).unwrap_or(true));
        // Les deux preuves de la poignée de main ne se confondent pas.
        assert_ne!(server_proof(&local, &remote, &auth), client_proof(&local, &remote, &auth));
    }
}
