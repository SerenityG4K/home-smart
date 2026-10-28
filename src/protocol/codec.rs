//! Chiffrement des charges utiles selon la version du protocole, négociation
//! de la clé de session (3.4/3.5) et construction des commandes JSON.

use super::cmd;
use super::crypto::{self, Key};
use super::frame::{self, DecodeOpts, Frame, Integrity};
use super::Version;
use anyhow::{bail, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Map, Value};

#[derive(Clone, Debug)]
pub struct Codec {
    pub version: Version,
    pub dev_id: String,
    pub local_key: Key,
    /// Clé effectivement utilisée : clé locale, ou clé de session négociée en 3.4/3.5.
    pub session_key: Key,
    /// Appareils « device22 » (3.2, ou 3.3 à identifiant de 22 caractères) :
    /// ils exigent une requête d'état explicite listant les DP.
    pub dev22: bool,
}

pub fn parse_key(s: &str) -> Result<Key> {
    let b = s.as_bytes();
    if b.len() != 16 {
        bail!("la clé locale doit faire exactement 16 caractères (reçu {})", b.len());
    }
    let mut k = [0u8; 16];
    k.copy_from_slice(b);
    Ok(k)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Retire l'en-tête « 3.x » + 12 octets s'il est présent.
fn strip_version_header(p: &[u8]) -> &[u8] {
    if p.len() >= 15 && p[0] == b'3' && p[1] == b'.' && p[2].is_ascii_digit() {
        &p[15..]
    } else {
        p
    }
}

impl Codec {
    pub fn new(version: Version, dev_id: &str, local_key: Key) -> Self {
        Codec {
            version,
            dev_id: dev_id.to_string(),
            local_key,
            session_key: local_key,
            dev22: version == Version::V32,
        }
    }

    fn version_header(&self) -> Vec<u8> {
        let mut h = self.version.as_str().as_bytes().to_vec();
        h.extend_from_slice(&[0u8; 12]);
        h
    }

    /// Options de lecture des trames pour cette session.
    pub fn decode_opts(&self, detect_retcode: bool) -> DecodeOpts<'_> {
        DecodeOpts {
            key: if self.version.needs_session() { Some(&self.session_key) } else { None },
            hmac: self.version.needs_session(),
            detect_retcode,
        }
    }

    /// Construit une trame complète. `retcode` n'est utilisé que côté appareil.
    pub fn encode(&self, seq: u32, command: u32, payload: &[u8], retcode: Option<u32>) -> Vec<u8> {
        let with_header = !cmd::no_version_header(command);
        match self.version {
            Version::V31 => {
                let body = if !payload.is_empty() && (command == cmd::CONTROL || command == cmd::STATUS) {
                    let b64 = B64.encode(crypto::ecb_encrypt(&self.local_key, payload, true));
                    let mut pre = Vec::with_capacity(b64.len() + 32);
                    pre.extend_from_slice(b"data=");
                    pre.extend_from_slice(b64.as_bytes());
                    pre.extend_from_slice(b"||lpv=3.1||");
                    pre.extend_from_slice(&self.local_key);
                    let digest = crypto::hex(&crypto::md5(&pre));
                    format!("3.1{}{}", &digest[8..24], b64).into_bytes()
                } else {
                    payload.to_vec()
                };
                frame::pack_55aa(seq, command, retcode, &body, Integrity::Crc)
            }
            Version::V32 | Version::V33 => {
                let mut body = Vec::new();
                if !payload.is_empty() {
                    if with_header {
                        body.extend(self.version_header());
                    }
                    body.extend(crypto::ecb_encrypt(&self.local_key, payload, true));
                }
                frame::pack_55aa(seq, command, retcode, &body, Integrity::Crc)
            }
            Version::V34 => {
                let mut body = Vec::new();
                if !payload.is_empty() {
                    let mut plain = if with_header { self.version_header() } else { Vec::new() };
                    plain.extend_from_slice(payload);
                    body = crypto::ecb_encrypt(&self.session_key, &plain, true);
                }
                frame::pack_55aa(seq, command, retcode, &body, Integrity::Hmac(&self.session_key))
            }
            Version::V35 => {
                let mut plain = if with_header && !payload.is_empty() { self.version_header() } else { Vec::new() };
                plain.extend_from_slice(payload);
                frame::pack_6699(seq, command, retcode, &plain, &self.session_key)
            }
        }
    }

    /// Déchiffre la charge utile d'une trame JSON (en-tête de version retiré).
    pub fn decode_payload(&self, f: &Frame) -> Result<Vec<u8>> {
        let p = &f.payload[..];
        if p.is_empty() {
            return Ok(Vec::new());
        }
        match self.version {
            Version::V31 => {
                if p.starts_with(b"3.1") && p.len() >= 19 {
                    let raw = B64.decode(&p[19..])?;
                    crypto::ecb_decrypt(&self.local_key, &raw, true)
                } else {
                    Ok(p.to_vec())
                }
            }
            Version::V32 | Version::V33 => {
                let body = strip_version_header(p);
                if body.is_empty() || body[0] == b'{' || body.len() % 16 != 0 {
                    return Ok(body.to_vec());
                }
                crypto::ecb_decrypt(&self.local_key, body, true)
            }
            Version::V34 => {
                if p.len() % 16 != 0 {
                    return Ok(strip_version_header(p).to_vec());
                }
                let plain = crypto::ecb_decrypt(&self.session_key, p, true)?;
                Ok(strip_version_header(&plain).to_vec())
            }
            Version::V35 => Ok(strip_version_header(p).to_vec()),
        }
    }

    /// Déchiffre une charge non-JSON (négociation), sans retirer d'en-tête.
    pub fn decrypt_raw(&self, f: &Frame) -> Result<Vec<u8>> {
        match self.version {
            Version::V34 => {
                let p = if f.payload.len() % 16 == 4 { &f.payload[4..] } else { &f.payload[..] };
                crypto::ecb_decrypt(&self.session_key, p, true)
            }
            _ => Ok(f.payload.clone()),
        }
    }

    // ---- Négociation de la clé de session (3.4 / 3.5) -------------------------

    /// Vérifie la réponse SESS_KEY_NEG_RESP et renvoie le nonce de l'appareil.
    pub fn check_negotiation(&self, resp: &Frame, local_nonce: &[u8; 16]) -> Result<[u8; 16]> {
        let data = self
            .decrypt_raw(resp)
            .map_err(|_| anyhow::anyhow!("réponse de négociation illisible : clé locale incorrecte ?"))?;
        if data.len() < 48 {
            bail!("réponse de négociation trop courte ({} octets)", data.len());
        }
        let d = &data[data.len() - 48..];
        if d[16..48] != crypto::hmac_sha256(&self.local_key, local_nonce) {
            bail!("clé locale incorrecte (l'appareil n'a pas validé la négociation)");
        }
        let mut remote = [0u8; 16];
        remote.copy_from_slice(&d[..16]);
        Ok(remote)
    }

    /// Charge utile de SESS_KEY_NEG_FINISH.
    pub fn negotiation_finish(&self, remote_nonce: &[u8; 16]) -> Vec<u8> {
        crypto::hmac_sha256(&self.local_key, remote_nonce).to_vec()
    }

    /// Réponse côté appareil à SESS_KEY_NEG_START (pour le simulateur).
    pub fn negotiation_response(&self, client_nonce: &[u8; 16], remote_nonce: &[u8; 16]) -> Vec<u8> {
        let mut out = remote_nonce.to_vec();
        out.extend_from_slice(&crypto::hmac_sha256(&self.local_key, client_nonce));
        out
    }

    /// Dérive la clé de session : AES(clé_locale, nonce_local XOR nonce_appareil).
    pub fn set_session(&mut self, local_nonce: &[u8; 16], remote_nonce: &[u8; 16]) {
        let x = crypto::xor16(local_nonce, remote_nonce);
        let mut key = [0u8; 16];
        match self.version {
            Version::V35 => {
                let mut buf = x.to_vec();
                let mut iv = [0u8; 12];
                iv.copy_from_slice(&local_nonce[..12]);
                crypto::gcm_encrypt(&self.local_key, &iv, &[], &mut buf);
                key.copy_from_slice(&buf[..16]);
            }
            _ => key.copy_from_slice(&crypto::ecb_encrypt(&self.local_key, &x, false)),
        }
        self.session_key = key;
    }

    // ---- Commandes ------------------------------------------------------------

    /// Requête d'état complète. `dps_hint` sert aux appareils device22.
    pub fn query(&self, dps_hint: &[String]) -> (u32, Vec<u8>) {
        let id = &self.dev_id;
        if self.version.needs_session() {
            // Même charge que localtuya / tinytuya pour DP_QUERY_NEW.
            let body = json!({ "gwId": id, "devId": id, "uid": id, "t": now_secs().to_string() });
            return (cmd::DP_QUERY_NEW, body.to_string().into_bytes());
        }
        if self.dev22 {
            let mut dps = Map::new();
            dps.insert("1".into(), Value::Null);
            for k in dps_hint {
                dps.insert(k.clone(), Value::Null);
            }
            return (cmd::CONTROL_NEW, self.legacy_body(Some(Value::Object(dps))));
        }
        let body = json!({ "gwId": id, "devId": id, "uid": id, "t": now_secs().to_string() });
        (cmd::DP_QUERY, body.to_string().into_bytes())
    }

    /// Requêtes de détection des DP pour un appareil device22 (par tranches, comme tinytuya).
    pub fn dev22_probe(&self) -> Vec<(u32, Vec<u8>)> {
        [(2, 11), (11, 21), (21, 31), (100, 111)]
            .into_iter()
            .map(|(a, b)| {
                let hint: Vec<String> = (a..b).map(|i: u32| i.to_string()).collect();
                self.query(&hint)
            })
            .collect()
    }

    fn legacy_body(&self, dps: Option<Value>) -> Vec<u8> {
        let id = &self.dev_id;
        let mut body = json!({ "devId": id, "uid": id, "t": now_secs().to_string() });
        if let Some(d) = dps {
            body["dps"] = d;
        }
        body.to_string().into_bytes()
    }

    pub fn control(&self, dps: &Map<String, Value>) -> (u32, Vec<u8>) {
        if self.version.needs_session() {
            let body = json!({ "protocol": 5, "t": now_secs(), "data": { "dps": dps } });
            (cmd::CONTROL_NEW, body.to_string().into_bytes())
        } else {
            (cmd::CONTROL, self.legacy_body(Some(Value::Object(dps.clone()))))
        }
    }

    pub fn heartbeat(&self) -> (u32, Vec<u8>) {
        let id = &self.dev_id;
        (cmd::HEART_BEAT, json!({ "gwId": id, "devId": id }).to_string().into_bytes())
    }

    /// Demande à l'appareil de rafraîchir des DP (mesures de consommation…).
    pub fn update_dps(&self, ids: &[u32]) -> (u32, Vec<u8>) {
        (cmd::UPDATEDPS, json!({ "dpId": ids }).to_string().into_bytes())
    }
}

/// Récupère l'objet `dps` d'une réponse (3.3 : à la racine, 3.4+ : sous `data`).
pub fn extract_dps(v: &Value) -> Option<&Map<String, Value>> {
    v.get("dps")
        .or_else(|| v.get("data").and_then(|d| d.get("dps")))
        .and_then(Value::as_object)
}

/// Choisit le DP « marche/arrêt » principal d'un appareil.
pub fn main_switch_dp(dps: &Map<String, Value>) -> Option<String> {
    let is_bool = |k: &str| matches!(dps.get(k), Some(Value::Bool(_)));
    if is_bool("20") && !is_bool("1") {
        return Some("20".into());
    }
    if is_bool("1") {
        return Some("1".into());
    }
    dps.iter()
        .filter(|(_, v)| v.is_boolean())
        .min_by_key(|(k, _)| k.parse::<u32>().unwrap_or(u32::MAX))
        .map(|(k, _)| k.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: Key = *b"0123456789abcdef";

    /// Ce que le client envoie doit être relu à l'identique côté appareil, pour chaque version.
    #[test]
    fn payload_roundtrip_all_versions() {
        for v in [Version::V31, Version::V32, Version::V33, Version::V34, Version::V35] {
            let c = Codec::new(v, "bf0123456789abcdef01", KEY);
            let mut dps = Map::new();
            dps.insert("1".into(), Value::Bool(true));
            let (command, body) = c.control(&dps);
            let raw = c.encode(1, command, &body, None);
            let f = frame::decode_one(&raw, c.decode_opts(false)).unwrap();
            assert_eq!(f.cmd, command);
            let plain = c.decode_payload(&f).unwrap();
            let v2: Value = serde_json::from_slice(&plain).unwrap_or_else(|e| panic!("{v}: {e}"));
            assert_eq!(extract_dps(&v2).unwrap()["1"], Value::Bool(true), "version {v}");
        }
    }

    #[test]
    fn device_status_with_retcode() {
        for v in [Version::V33, Version::V34, Version::V35] {
            let c = Codec::new(v, "x", KEY);
            let raw = c.encode(9, cmd::STATUS, br#"{"dps":{"2":42}}"#, Some(0));
            let f = frame::decode_one(&raw, c.decode_opts(true)).unwrap();
            assert_eq!(f.retcode, Some(0), "version {v}");
            let plain = c.decode_payload(&f).unwrap();
            assert_eq!(plain, br#"{"dps":{"2":42}}"#, "version {v}");
        }
    }

    #[test]
    fn negotiation_both_sides_agree() {
        for v in [Version::V34, Version::V35] {
            let mut client = Codec::new(v, "x", KEY);
            let mut device = Codec::new(v, "x", KEY);
            let ln = *b"0123456789abcdef";
            let rn = *b"fedcba9876543210";
            let resp = device.encode(1, cmd::SESS_KEY_NEG_RESP, &device.negotiation_response(&ln, &rn), Some(0));
            let f = frame::decode_one(&resp, client.decode_opts(true)).unwrap();
            assert_eq!(client.check_negotiation(&f, &ln).unwrap(), rn);
            client.set_session(&ln, &rn);
            device.set_session(&ln, &rn);
            assert_eq!(client.session_key, device.session_key);
            assert_ne!(client.session_key, KEY);
        }
    }

    #[test]
    fn wrong_key_detected_in_negotiation() {
        let device = Codec::new(Version::V34, "x", KEY);
        let client = Codec::new(Version::V34, "x", *b"AAAAAAAAAAAAAAAA");
        let ln = [1u8; 16];
        let resp = device.encode(1, cmd::SESS_KEY_NEG_RESP, &device.negotiation_response(&ln, &[2u8; 16]), None);
        // Signature HMAC invalide dès la trame.
        assert!(frame::decode_one(&resp, client.decode_opts(true)).is_err());
    }

    #[test]
    fn switch_dp_detection() {
        let m = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(main_switch_dp(&m(json!({"20": true, "22": 500}))).as_deref(), Some("20"));
        assert_eq!(main_switch_dp(&m(json!({"1": false, "19": 0}))).as_deref(), Some("1"));
        assert_eq!(main_switch_dp(&m(json!({"101": true, "7": false}))).as_deref(), Some("7"));
    }
}
