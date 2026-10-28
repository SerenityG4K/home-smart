//! Import unique des clés locales depuis un compte Smart Life / Tuya Smart.
//!
//! Même mécanisme que l'intégration Tuya officielle de Home Assistant
//! (SDK `tuya-device-sharing-sdk`) : connexion par QR code scanné dans l'app
//! Smart Life, sans compte développeur. On lit la liste des appareils (avec
//! leur `local_key`), puis on ferme la session : le pilotage reste 100 % local.
//!
//! Requêtes : paramètres chiffrés en AES-128-GCM avec une clé dérivée par requête,
//! en-têtes signés en HMAC-SHA256, réponse chiffrée de la même façon.

use crate::config::{self, DeviceConfig};
use crate::protocol::crypto;
use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Map, Value};

/// Identifiant client public de l'intégration Tuya de Home Assistant.
pub const CLIENT_ID: &str = "HA_3y9q4ak7g4ephrvke";
const SCHEMA: &str = "haauthorize";
const LOGIN_HOST: &str = "https://apigw.iotbing.com";

#[derive(Clone, Debug)]
pub struct QrLogin {
    pub user_code: String,
    pub token: String,
}

impl QrLogin {
    /// Contenu du QR code à scanner avec l'app Smart Life.
    pub fn qr_content(&self) -> String {
        format!("tuyaSmart--qrLogin?token={}", self.token)
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    endpoint: String,
    access_token: String,
    refresh_token: String,
    terminal_id: String,
}

#[derive(Clone, Debug)]
pub struct CloudDevice {
    pub id: String,
    pub name: String,
    pub local_key: String,
    pub category: String,
    pub product_name: String,
    pub home: String,
    /// Sous-appareil (Zigbee, Bluetooth…) joint via une passerelle.
    pub sub: bool,
    pub schema: Map<String, Value>,
}

impl CloudDevice {
    pub fn to_config(&self, ip: Option<String>, version: Option<String>) -> DeviceConfig {
        DeviceConfig {
            id: self.id.clone(),
            key: self.local_key.clone(),
            name: self.name.clone(),
            ip,
            port: None,
            version,
            room: None,
            kind: config::kind_from_category(&self.category).map(str::to_string),
            schema: self.schema.clone(),
            safety: Default::default(),
            powers_pc: false,
        }
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn str_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Étape 1 : demande un QR code de connexion pour ce « code utilisateur ».
pub fn start_login(user_code: &str) -> Result<QrLogin> {
    let user_code = user_code.trim();
    if user_code.is_empty() {
        bail!("code utilisateur vide");
    }
    let resp: Value = agent()
        .post(format!("{LOGIN_HOST}/v1.0/m/life/home-assistant/qrcode/tokens"))
        .query("clientid", CLIENT_ID)
        .query("usercode", user_code)
        .query("schema", SCHEMA)
        .send_empty()
        .context("connexion à Smart Life impossible (Internet ?)")?
        .body_mut()
        .read_json()?;
    if resp["success"].as_bool() != Some(true) {
        bail!("Smart Life refuse le code utilisateur : {}", resp["msg"].as_str().unwrap_or("erreur inconnue"));
    }
    let token = resp["result"]["qrcode"].as_str().ok_or_else(|| anyhow!("réponse sans QR code"))?;
    Ok(QrLogin { user_code: user_code.to_string(), token: token.to_string() })
}

/// Étape 2 : `Some(session)` une fois le QR code scanné et validé dans l'app.
pub fn poll_login(login: &QrLogin) -> Result<Option<Session>> {
    let resp: Value = agent()
        .get(format!("{LOGIN_HOST}/v1.0/m/life/home-assistant/qrcode/tokens/{}", login.token))
        .query("clientid", CLIENT_ID)
        .query("usercode", &login.user_code)
        .call()?
        .body_mut()
        .read_json()?;
    if resp["success"].as_bool() != Some(true) {
        return Ok(None);
    }
    let r = &resp["result"];
    let field = |k: &str| r[k].as_str().map(str::to_string).ok_or_else(|| anyhow!("réponse de connexion incomplète ({k})"));
    Ok(Some(Session {
        endpoint: field("endpoint")?,
        access_token: field("access_token")?,
        refresh_token: field("refresh_token")?,
        terminal_id: field("terminal_id")?,
    }))
}

// ---- Requêtes signées -------------------------------------------------------

fn random_nonce12() -> String {
    use rand::Rng;
    const ALPHA: &[u8] = b"ABCDEFGHJKMNPQRSTWXYZabcdefhijkmnprstwxyz2345678";
    let mut rng = rand::thread_rng();
    (0..12).map(|_| ALPHA[rng.gen_range(0..ALPHA.len())] as char).collect()
}

fn uuid4() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = crypto::hex(&b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Clé AES de la requête : les 16 premiers caractères hexadécimaux de HMAC(rid, hash_key).
fn request_secret(rid: &str, hash_key: &str) -> [u8; 16] {
    let hex = crypto::hex(&crypto::hmac_sha256(rid.as_bytes(), hash_key.as_bytes()));
    let mut k = [0u8; 16];
    k.copy_from_slice(&hex.as_bytes()[..16]);
    k
}

/// base64(nonce) + base64(chiffré‖tag), comme le SDK.
fn encrypt(plain: &str, secret: &[u8; 16]) -> String {
    let nonce = random_nonce12();
    let mut iv = [0u8; 12];
    iv.copy_from_slice(nonce.as_bytes());
    let mut data = plain.as_bytes().to_vec();
    let tag = crypto::gcm_encrypt(secret, &iv, &[], &mut data);
    data.extend_from_slice(&tag);
    format!("{}{}", B64.encode(nonce.as_bytes()), B64.encode(&data))
}

fn decrypt(cipher: &str, secret: &[u8; 16]) -> Result<String> {
    let raw = B64.decode(cipher.trim())?;
    if raw.len() < 12 + 16 {
        bail!("réponse chiffrée trop courte");
    }
    let (iv, rest) = raw.split_at(12);
    let (ct, tag) = rest.split_at(rest.len() - 16);
    let mut data = ct.to_vec();
    crypto::gcm_decrypt(secret, iv, &[], &mut data, tag)?;
    Ok(String::from_utf8(data)?)
}

impl Session {
    fn request(&self, method: &str, path: &str, params: Option<Value>, body: Option<Value>) -> Result<Value> {
        let rid = uuid4();
        let hash_key = crypto::hex(&crypto::md5(format!("{rid}{}", self.refresh_token).as_bytes()));
        let secret = request_secret(&rid, &hash_key);
        let query_enc = params.filter(|p| p.as_object().is_some_and(|m| !m.is_empty())).map(|p| encrypt(&p.to_string(), &secret));
        let body_enc = body.filter(|b| b.as_object().is_some_and(|m| !m.is_empty())).map(|b| encrypt(&b.to_string(), &secret));
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis().to_string();

        // Signature : en-têtes non vides « clé=valeur » joints par « || », puis les données chiffrées.
        let mut sign_str = [("X-appKey", CLIENT_ID), ("X-requestId", rid.as_str()), ("X-time", t.as_str()), ("X-token", self.access_token.as_str())]
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("||");
        sign_str.push_str(query_enc.as_deref().unwrap_or(""));
        sign_str.push_str(body_enc.as_deref().unwrap_or(""));
        let sign = crypto::hex(&crypto::hmac_sha256(hash_key.as_bytes(), sign_str.as_bytes()));

        let url = format!("{}{path}", self.endpoint.trim_end_matches('/'));
        let headers = [
            ("X-appKey", CLIENT_ID),
            ("X-requestId", rid.as_str()),
            ("X-sid", ""),
            ("X-time", t.as_str()),
            ("X-token", self.access_token.as_str()),
            ("X-sign", sign.as_str()),
        ];
        let agent = agent();
        let mut resp = match method {
            "GET" => {
                let mut req = agent.get(&url);
                for (k, v) in headers {
                    req = req.header(k, v);
                }
                if let Some(q) = &query_enc {
                    req = req.query("encdata", q);
                }
                req.call()?
            }
            _ => {
                let mut req = agent.post(&url);
                for (k, v) in headers {
                    req = req.header(k, v);
                }
                if let Some(q) = &query_enc {
                    req = req.query("encdata", q);
                }
                match &body_enc {
                    Some(b) => req.send_json(json!({ "encdata": b }))?,
                    None => req.send_empty()?,
                }
            }
        };
        let ret: Value = resp.body_mut().read_json().with_context(|| format!("réponse illisible ({})", resp.status()))?;
        if ret["success"].as_bool() != Some(true) {
            bail!("Smart Life : {} ({})", ret["msg"].as_str().unwrap_or("erreur"), str_of(&ret["code"]));
        }
        match &ret["result"] {
            Value::String(s) => {
                let plain = decrypt(s, &secret)?;
                Ok(serde_json::from_str(&plain).unwrap_or(Value::String(plain)))
            }
            other => Ok(other.clone()),
        }
    }

    /// Étape 3 : tous les appareils du compte, avec clé locale et description des DP.
    pub fn fetch_devices(&self) -> Result<Vec<CloudDevice>> {
        let homes = self.request("GET", "/v1.0/m/life/users/homes", None, None)?;
        let mut out: Vec<CloudDevice> = Vec::new();
        for home in homes.as_array().into_iter().flatten() {
            let home_id = str_of(&home["ownerId"]);
            let home_name = str_of(&home["name"]);
            let list = self.request("GET", "/v1.0/m/life/ha/home/devices", Some(json!({ "homeId": home_id })), None)?;
            for d in list.as_array().into_iter().flatten() {
                let id = str_of(&d["id"]);
                if id.is_empty() || out.iter().any(|o| o.id == id) {
                    continue;
                }
                out.push(CloudDevice {
                    schema: self.dp_schema(&id).unwrap_or_default(),
                    name: str_of(&d["name"]),
                    local_key: str_of(&d["local_key"]),
                    category: str_of(&d["category"]),
                    product_name: str_of(&d["product_name"]),
                    sub: d["sub"].as_bool().unwrap_or(false),
                    home: home_name.clone(),
                    id,
                });
            }
        }
        Ok(out)
    }

    /// Numéro de DP → code de fonction, type et plage de valeurs.
    fn dp_schema(&self, id: &str) -> Result<Map<String, Value>> {
        let r = self.request("GET", &format!("/v1.0/m/life/devices/{id}/status"), None, None)?;
        let mut schema = Map::new();
        for rel in r["dpStatusRelationDTOS"].as_array().into_iter().flatten() {
            let dp = str_of(&rel["dpId"]);
            let code = str_of(&rel["statusCode"]);
            if dp.is_empty() || code.is_empty() {
                continue;
            }
            let values = match &rel["valueDesc"] {
                Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Object(Map::new())),
                Value::Object(o) => Value::Object(o.clone()),
                _ => Value::Object(Map::new()),
            };
            schema.insert(dp, json!({ "code": code, "type": str_of(&rel["valueType"]), "values": values }));
        }
        Ok(schema)
    }

    /// Ferme la session côté Tuya : plus rien n'y reste ouvert après l'import.
    pub fn logout(&self) {
        let body = json!({ "accessToken": self.access_token, "terminalId": self.terminal_id });
        if let Err(e) = self.request("POST", "/v1.0/m/token/terminal/expire", None, Some(body)) {
            tracing::debug!("fermeture de session Smart Life : {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_crypto_roundtrip() {
        let secret = request_secret(&uuid4(), "0123456789abcdef0123456789abcdef");
        assert!(secret.iter().all(|b| b.is_ascii_hexdigit()));
        let enc = encrypt(r#"{"homeId":"42"}"#, &secret);
        // Le serveur renvoie base64(nonce‖chiffré‖tag) : on reconstruit ce format.
        let (n, c) = enc.split_at(16);
        let mut raw = B64.decode(n).unwrap();
        raw.extend(B64.decode(c).unwrap());
        assert_eq!(decrypt(&B64.encode(raw), &secret).unwrap(), r#"{"homeId":"42"}"#);
    }

    #[test]
    fn uuid_format() {
        let u = uuid4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
    }
}
