//! Caméras Tapo : leur API locale chiffrée (HTTPS, port 443).
//!
//! Le « compte de la caméra » (RTSP, ONVIF) ne suffit pas ici : l'API locale n'accepte que
//! l'utilisateur `admin` avec le mot de passe du **compte TP-Link**, que la caméra garde (en
//! empreinte) depuis son installation dans l'application. Elle le vérifie elle-même, sans
//! Internet : rien ne passe par le cloud. HomeSmart ne garde que ses empreintes (MD5 et SHA-256),
//! jamais le mot de passe lui-même.
//!
//! Connexion des firmwares « encrypt_type 3 » (C200, C210 et apparentés) :
//! 1. `login` avec un nonce client (`cnonce`) : la caméra répond son nonce et une preuve
//!    `device_confirm` = SHA-256(cnonce ‖ empreinte ‖ nonce) ‖ nonce ‖ cnonce. Elle se vérifie
//!    ici : si elle ne correspond pas, le mot de passe est faux et on s'arrête là, sans second
//!    essai. **Chaque tentative compte** : au bout de 10 (`max_time`), la caméra refuse toute
//!    connexion pendant un moment (`sec_left`) ;
//! 2. `login` avec `digest_passwd` = SHA-256(empreinte ‖ cnonce ‖ nonce) ‖ cnonce ‖ nonce : la
//!    caméra rend un jeton de session (`stok`) et un numéro de départ (`start_seq`) ;
//! 3. chaque requête part chiffrée (AES-128-CBC, clé `lsk` et vecteur `ivb` dérivés des deux
//!    nonces) dans `securePassthrough`, numérotée (`Seq`) et signée (`Tapo_tag`).
//!
//! Les très anciens firmwares (sans `encrypt_type 3`) prennent l'empreinte MD5 en clair dans le
//! `login`. Les plus récents (« TPAP », SPAKE2+) ne sont pas encore gérés : c'est dit clairement.
//!
//! Protocole décrit d'après pytapo et go2rtc (licences MIT).

use super::klap::{cbc_decrypt, cbc_encrypt};
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chrono::NaiveDate;
use md5::Md5;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const API_PORT: u16 = 443;
/// Utilisateur de l'API locale : toujours `admin` (le compte TP-Link), jamais le compte de la caméra.
pub const USER: &str = "admin";
/// Après un refus du mot de passe, aucun nouvel essai automatique pendant ce délai : chaque
/// tentative rapproche la caméra de son verrouillage.
const REFUSAL_HOLD: Duration = Duration::from_secs(15 * 60);

/// Empreintes du mot de passe du compte TP-Link (hexadécimal en majuscules) : c'est tout ce que
/// HomeSmart garde. La caméra attend l'une ou l'autre selon son firmware.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct TapoSecret {
    pub md5: String,
    pub sha256: String,
}

impl TapoSecret {
    pub fn from_password(password: &str) -> TapoSecret {
        TapoSecret { md5: hex_upper(&Md5::digest(password.as_bytes())), sha256: hex_upper(&Sha256::digest(password.as_bytes())) }
    }

    pub fn is_set(&self) -> bool {
        !self.md5.is_empty() || !self.sha256.is_empty()
    }
}

pub fn hex_upper(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

fn sha256_upper(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    hex_upper(&h.finalize())
}

/// Nonce aléatoire en hexadécimal majuscule (`n` octets).
pub fn nonce(n: usize) -> String {
    let mut b = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut b);
    hex_upper(&b)
}

/// Preuve que la caméra connaît l'empreinte (`device_confirm` attendu).
pub fn device_confirm(cnonce: &str, hashed: &str, nonce: &str) -> String {
    format!("{}{nonce}{cnonce}", sha256_upper(&[cnonce.as_bytes(), hashed.as_bytes(), nonce.as_bytes()]))
}

/// Preuve qu'on la connaît aussi (`digest_passwd` envoyé au second `login`).
pub fn digest_passwd(hashed: &str, cnonce: &str, nonce: &str) -> String {
    format!("{}{cnonce}{nonce}", sha256_upper(&[hashed.as_bytes(), cnonce.as_bytes(), nonce.as_bytes()]))
}

/// Clé (`lsk`) ou vecteur (`ivb`) de la session chiffrée.
pub fn session_key(kind: &str, cnonce: &str, nonce: &str, hashed: &str) -> [u8; 16] {
    let hashed_key = sha256_upper(&[cnonce.as_bytes(), hashed.as_bytes(), nonce.as_bytes()]);
    let d = Sha256::digest([kind.as_bytes(), cnonce.as_bytes(), nonce.as_bytes(), hashed_key.as_bytes()].concat());
    let mut k = [0u8; 16];
    k.copy_from_slice(&d[..16]);
    k
}

/// Étiquette `Tapo_tag` d'une requête chiffrée (corps exact envoyé, numéro `Seq`).
pub fn tag(hashed: &str, cnonce: &str, body: &str, seq: i64) -> String {
    let t = sha256_upper(&[hashed.as_bytes(), cnonce.as_bytes()]);
    sha256_upper(&[t.as_bytes(), body.as_bytes(), seq.to_string().as_bytes()])
}

/// Codes d'erreur de l'API, en clair.
pub fn error_text(code: i64) -> String {
    match code {
        -40401 => "session expirée".into(),
        -40210 | -40106 | -40105 | -40101 | -1 => format!("fonction absente de cette caméra (code {code})"),
        -40209 | -40411 => "mot de passe du compte TP-Link refusé par la caméra".into(),
        -40404 => "caméra verrouillée après trop d'essais de mot de passe".into(),
        -64303 => "impossible pendant la ronde automatique de la caméra".into(),
        -64304 => "réglage refusé par la caméra (valeur hors limites)".into(),
        -71101 | -71103 | -71105 => "identifiant de lecture expiré (relecture de la carte SD)".into(),
        -71106 => "carte SD absente ou illisible".into(),
        c => format!("la caméra refuse (code {c})"),
    }
}

/// Verrouillage en cours (`sec_left` > 0) dans une réponse de `login`.
fn suspension(r: &Value) -> Option<i64> {
    [r.pointer("/result/data/sec_left"), r.pointer("/data/sec_left")].into_iter().flatten().filter_map(Value::as_i64).find(|s| *s > 0)
}

/// Essais restants avant verrouillage, si la caméra le dit (`max_time` − tentatives faites).
fn attempts_left(r: &Value) -> Option<i64> {
    let d = r.pointer("/result/data")?;
    let (t, max) = (d.get("time")?.as_i64()?, d.get("max_time")?.as_i64()?);
    // Selon le firmware, `time` compte les essais restants ou déjà faits.
    Some(if t <= max { t } else { 0 })
}

/// Session chiffrée ouverte.
struct Secure {
    lsk: [u8; 16],
    ivb: [u8; 16],
    seq: i64,
    cnonce: String,
}

struct Session {
    stok: String,
    /// Empreinte reconnue par la caméra (MD5 ou SHA-256).
    hashed: String,
    secure: Option<Secure>,
}

/// Connexion à l'API locale d'une caméra Tapo (bloquante : à utiliser hors de l'interface).
pub struct TapoCam {
    base: String,
    host_header: String,
    secret: TapoSecret,
    agent: ureq::Agent,
    session: Option<Session>,
    user_id: Option<i64>,
    /// Horloge du PC − horloge de la caméra (secondes).
    clock_offset: Option<i64>,
    /// Dernier refus du mot de passe (pas de nouvel essai avant `REFUSAL_HOLD`).
    refused: Option<(Instant, String)>,
}

fn agent(tls: bool) -> ureq::Agent {
    let mut b = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(8))).http_status_as_error(false);
    if tls {
        // Certificat auto-signé de la caméra (« TPRI-DEVICE ») : pas de chaîne de confiance possible.
        b = b.tls_config(ureq::tls::TlsConfig::builder().disable_verification(true).build());
    }
    b.build().into()
}

impl TapoCam {
    /// Caméra sur le réseau (HTTPS, port 443). La connexion se fait à la première requête.
    pub fn new(host: &str, secret: &TapoSecret) -> TapoCam {
        Self::with_base(&format!("https://{host}:{API_PORT}"), secret)
    }

    /// Adresse de base explicite (`http://127.0.0.1:…` pour la caméra simulée des tests).
    pub fn with_base(base: &str, secret: &TapoSecret) -> TapoCam {
        let base = base.trim_end_matches('/').to_string();
        let host_header = base.split_once("://").map(|(_, h)| h.to_string()).unwrap_or_default();
        TapoCam {
            agent: agent(base.starts_with("https")),
            base,
            host_header,
            secret: secret.clone(),
            session: None,
            user_id: None,
            clock_offset: None,
            refused: None,
        }
    }

    fn post(&self, url: &str, body: &str, extra: &[(&str, String)]) -> Result<Value> {
        let mut req = self
            .agent
            .post(url)
            .header("Host", &self.host_header)
            .header("Referer", &format!("https://{}", self.host_header))
            .header("Accept", "application/json")
            .header("User-Agent", "Tapo CameraClient Android")
            .header("requestByApp", "true")
            .header("Content-Type", "application/json; charset=UTF-8");
        for (k, v) in extra {
            req = req.header(*k, v);
        }
        let mut resp = req.send(body).with_context(|| format!("connexion à l'API locale de la caméra ({})", self.host_header))?;
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        serde_json::from_str(&text).with_context(|| format!("réponse illisible de la caméra (HTTP {})", resp.status().as_u16()))
    }

    /// Se connecte (deux requêtes `login`), sauf refus récent du même mot de passe.
    pub fn login(&mut self) -> Result<()> {
        if let Some((at, why)) = &self.refused {
            if at.elapsed() < REFUSAL_HOLD {
                bail!("{why} (pas de nouvel essai avant {} min, pour ne pas verrouiller la caméra)", (REFUSAL_HOLD - at.elapsed()).as_secs() / 60 + 1);
            }
        }
        if !self.secret.is_set() {
            bail!("mot de passe du compte TP-Link non renseigné");
        }
        self.session = None;
        let cnonce = nonce(8);
        let r = self.post(&self.base, &json!({ "method": "login", "params": { "cnonce": cnonce, "encrypt_type": "3", "username": USER } }).to_string(), &[])?;
        if let Some(s) = suspension(&r) {
            bail!("caméra verrouillée après trop d'essais de mot de passe : réessaie dans {} min", s / 60 + 1);
        }
        let code = r.get("error_code").and_then(Value::as_i64).unwrap_or(0);
        if r.pointer("/result/tpap").is_some() || code == -40211 {
            bail!("ce firmware Tapo utilise la nouvelle connexion « TPAP », pas encore gérée par HomeSmart");
        }
        let data = r.pointer("/result/data").cloned().unwrap_or(Value::Null);
        let secure = code == -40413 && data.get("encrypt_type").and_then(Value::as_array).is_some_and(|a| a.iter().any(|v| v.as_str() == Some("3")));
        if !secure {
            // Ancien firmware : l'empreinte MD5 part dans la requête (sur HTTPS).
            let r = self.post(&self.base, &json!({ "method": "login", "params": { "hashed": true, "password": self.secret.md5, "username": USER } }).to_string(), &[])?;
            if let Some(s) = suspension(&r) {
                bail!("caméra verrouillée après trop d'essais de mot de passe : réessaie dans {} min", s / 60 + 1);
            }
            let stok = r.pointer("/result/stok").and_then(Value::as_str).filter(|s| !s.is_empty());
            let Some(stok) = stok else {
                let why = "mot de passe du compte TP-Link refusé par la caméra".to_string();
                self.refused = Some((Instant::now(), why.clone()));
                bail!(why);
            };
            self.session = Some(Session { stok: stok.to_string(), hashed: self.secret.md5.clone(), secure: None });
            self.refused = None;
            return Ok(());
        }
        let nonce_dev = data.get("nonce").and_then(Value::as_str).unwrap_or_default().to_string();
        let confirm = data.get("device_confirm").and_then(Value::as_str).unwrap_or_default();
        let hashed = [&self.secret.sha256, &self.secret.md5].into_iter().find(|h| !h.is_empty() && device_confirm(&cnonce, h, &nonce_dev) == confirm).cloned();
        let Some(hashed) = hashed else {
            // Vérifié ici : on n'envoie pas de second `login` voué à l'échec.
            let left = attempts_left(&r).map(|n| format!(" ; la caméra se verrouille après encore {n} essai(s)")).unwrap_or_default();
            let why = format!("mot de passe du compte TP-Link refusé : la caméra ne le reconnaît pas{left}");
            self.refused = Some((Instant::now(), why.clone()));
            bail!(why);
        };
        let r = self.post(
            &self.base,
            &json!({ "method": "login", "params": { "cnonce": cnonce, "encrypt_type": "3", "digest_passwd": digest_passwd(&hashed, &cnonce, &nonce_dev), "username": USER } }).to_string(),
            &[],
        )?;
        if let Some(s) = suspension(&r) {
            bail!("caméra verrouillée après trop d'essais de mot de passe : réessaie dans {} min", s / 60 + 1);
        }
        let res = r.get("result").cloned().unwrap_or(Value::Null);
        if let Some(g) = res.get("user_group").and_then(Value::as_str) {
            if g != "root" {
                bail!("la caméra n'accorde pas le pilotage à ce compte (groupe « {g} ») : il faut le mot de passe du compte TP-Link, pas celui du compte de la caméra");
            }
        }
        let (Some(stok), Some(seq)) = (res.get("stok").and_then(Value::as_str), res.get("start_seq").and_then(Value::as_i64)) else {
            let code = r.get("error_code").and_then(Value::as_i64).unwrap_or(0);
            bail!("connexion refusée par la caméra : {}", error_text(code));
        };
        self.session = Some(Session {
            stok: stok.to_string(),
            secure: Some(Secure { lsk: session_key("lsk", &cnonce, &nonce_dev, &hashed), ivb: session_key("ivb", &cnonce, &nonce_dev, &hashed), seq, cnonce }),
            hashed,
        });
        self.refused = None;
        Ok(())
    }

    /// Connecté (session ouverte) ?
    pub fn connected(&self) -> bool {
        self.session.is_some()
    }

    /// Envoie une requête brute et renvoie la réponse complète (déchiffrée).
    fn send_once(&mut self, req: &Value) -> Result<Value> {
        if self.session.is_none() {
            self.login()?;
        }
        let base = self.base.clone();
        let s = self.session.as_mut().ok_or_else(|| anyhow!("pas de session"))?;
        let url = format!("{base}/stok={}/ds", s.stok);
        let Some(sec) = s.secure.as_mut() else {
            let body = req.to_string();
            return self.post(&url, &body, &[]);
        };
        let inner = req.to_string();
        let body = json!({ "method": "securePassthrough", "params": { "request": B64.encode(cbc_encrypt(&sec.lsk, &sec.ivb, inner.as_bytes())) } }).to_string();
        let seq = sec.seq;
        let tag = tag(&s.hashed, &sec.cnonce, &body, seq);
        sec.seq += 1;
        let (lsk, ivb) = (sec.lsk, sec.ivb);
        let r = self.post(&url, &body, &[("Seq", seq.to_string()), ("Tapo_tag", tag)])?;
        match r.pointer("/result/response").and_then(Value::as_str) {
            Some(enc) => {
                let plain = cbc_decrypt(&lsk, &ivb, &B64.decode(enc).context("réponse chiffrée illisible")?)?;
                serde_json::from_slice(&plain).context("réponse déchiffrée illisible")
            }
            None => Ok(r),
        }
    }

    /// Requête brute ; une session expirée est rouverte une fois.
    pub fn raw(&mut self, req: &Value) -> Result<Value> {
        let r = self.send_once(req)?;
        if r.get("error_code").and_then(Value::as_i64) == Some(-40401) {
            self.session = None;
            return self.send_once(req);
        }
        Ok(r)
    }

    /// Requête « simple » (`set` / `do`) : vérifie son code d'erreur.
    pub fn exec(&mut self, req: &Value) -> Result<Value> {
        let r = self.raw(req)?;
        match r.get("error_code").and_then(Value::as_i64).unwrap_or(0) {
            0 => Ok(r.get("result").cloned().unwrap_or(r)),
            c => Err(anyhow!("{}", error_text(c))),
        }
    }

    /// Méthode nommée (enveloppée dans `multipleRequest`, comme l'application) : son `result`.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let mut one = json!({ "method": method });
        if !params.is_null() {
            one["params"] = params;
        }
        let r = self.raw(&json!({ "method": "multipleRequest", "params": { "requests": [one] } }))?;
        if let Some(c) = r.get("error_code").and_then(Value::as_i64).filter(|c| *c != 0) {
            bail!("{}", error_text(c));
        }
        let first = r.pointer("/result/responses/0").cloned().ok_or_else(|| anyhow!("réponse sans résultat"))?;
        match first.get("error_code").and_then(Value::as_i64).unwrap_or(0) {
            0 => Ok(first.get("result").cloned().unwrap_or(first)),
            c => Err(anyhow!("{}", error_text(c))),
        }
    }

    // ---- Informations ----------------------------------------------------------------------

    pub fn device_info(&mut self) -> Result<DeviceInfo> {
        let r = self.call("getDeviceInfo", json!({ "device_info": { "name": ["basic_info"] } }))?;
        let b = r.pointer("/device_info/basic_info").cloned().unwrap_or(Value::Null);
        let s = |k: &str| b.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        Ok(DeviceInfo { model: s("device_model"), firmware: s("sw_version"), hardware: s("hw_version"), name: s("device_alias"), mac: s("mac") })
    }

    /// Décalage d'horloge PC − caméra (secondes), lu une fois.
    pub fn clock_offset(&mut self) -> Result<i64> {
        if let Some(o) = self.clock_offset {
            return Ok(o);
        }
        let r = self.call("getClockStatus", json!({ "system": { "name": "clock_status" } }))?;
        let cam = r.pointer("/system/clock_status/seconds_from_1970").and_then(Value::as_i64).ok_or_else(|| anyhow!("heure de la caméra absente"))?;
        let o = chrono::Utc::now().timestamp() - cam;
        self.clock_offset = Some(o);
        Ok(o)
    }

    // ---- Alarme (sirène et lumière) ----------------------------------------------------------

    pub fn alarm(&mut self) -> Result<AlarmConfig> {
        let r = self.call("getLastAlarmInfo", json!({ "msg_alarm": { "name": ["chn1_msg_alarm_info"] } }))?;
        let a = r.pointer("/msg_alarm/chn1_msg_alarm_info").cloned().ok_or_else(|| anyhow!("pas d'alarme sur cette caméra"))?;
        Ok(AlarmConfig::from_json(&a))
    }

    pub fn set_alarm(&mut self, a: &AlarmConfig) -> Result<()> {
        if !a.sound && !a.light {
            bail!("choisis au moins le son ou la lumière");
        }
        let mut info = json!({ "alarm_type": "0", "light_type": "0", "enabled": on_off(a.enabled), "alarm_mode": a.modes() });
        if let Some(v) = &a.volume {
            info["alarm_volume"] = json!(v);
        }
        if let Some(d) = a.duration {
            info["alarm_duration"] = json!(d);
        }
        self.exec(&json!({ "method": "set", "msg_alarm": { "chn1_msg_alarm_info": info } })).map(|_| ())
    }

    /// Déclenche (ou arrête) l'alarme tout de suite, quels que soient les réglages de détection.
    pub fn manual_alarm(&mut self, on: bool) -> Result<()> {
        self.exec(&json!({ "method": "do", "msg_alarm": { "manual_msg_alarm": { "action": if on { "start" } else { "stop" } } } })).map(|_| ())
    }

    // ---- Suivi des mouvements (intégré à la caméra) -------------------------------------------

    pub fn auto_track(&mut self) -> Result<bool> {
        let r = self.call("getTargetTrackConfig", json!({ "target_track": { "name": ["target_track_info"] } }))?;
        Ok(r.pointer("/target_track/target_track_info/enabled").and_then(Value::as_str) == Some("on"))
    }

    pub fn set_auto_track(&mut self, on: bool) -> Result<()> {
        self.call("setTargetTrackConfig", json!({ "target_track": { "target_track_info": { "enabled": on_off(on) } } })).map(|_| ())
    }

    // ---- Son ----------------------------------------------------------------------------------

    /// Volume du haut-parleur (0…100).
    pub fn speaker_volume(&mut self) -> Result<u8> {
        let r = self.call("getAudioConfig", json!({ "method": "get", "audio_config": { "name": ["speaker", "microphone", "record_audio"] } }))?;
        let v = r.pointer("/audio_config/speaker/volume").ok_or_else(|| anyhow!("pas de haut-parleur sur cette caméra"))?;
        Ok(v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0).min(100) as u8)
    }

    pub fn set_speaker_volume(&mut self, v: u8) -> Result<()> {
        self.call("setSpeakerVolume", json!({ "method": "set", "audio_config": { "speaker": { "volume": v.min(100) } } })).map(|_| ())
    }

    // ---- Qualité vidéo --------------------------------------------------------------------------

    /// Réglage du flux principal tel que la caméra le décrit (résolution, débit, codec…).
    pub fn video_quality(&mut self) -> Result<VideoQuality> {
        let r = self.call("getVideoQualities", json!({ "video": { "name": ["main"] } }))?;
        let m = r.pointer("/video/main").cloned().unwrap_or(Value::Null);
        let s = |k: &str| m.get(k).map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).filter(|s| !s.is_empty());
        Ok(VideoQuality { resolution: s("resolution").map(|r| r.replace('*', "×")), bitrate_kbps: s("bitrate").and_then(|b| b.parse().ok()), bitrate_type: s("bitrate_type"), encode_type: s("encode_type"), quality: s("quality"), smart_codec: s("smart_codec") })
    }

    // ---- Carte SD -------------------------------------------------------------------------------

    pub fn sd_card(&mut self) -> Result<Vec<SdCard>> {
        let r = self.call("getSdCardStatus", json!({ "harddisk_manage": { "table": ["hd_info"] } }))?;
        let mut out = Vec::new();
        for item in r.pointer("/harddisk_manage/hd_info").and_then(Value::as_array).into_iter().flatten() {
            // Chaque élément : { "hd_info_1": { … } }.
            for v in item.as_object().into_iter().flat_map(|o| o.values()) {
                let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
                out.push(SdCard { status: s("status"), total: s("total_space"), free: s("free_space"), video_total: s("video_total_space"), video_free: s("video_free_space") });
            }
        }
        Ok(out)
    }

    /// Identifiant de lecture demandé par la relecture.
    pub fn user_id(&mut self) -> Result<i64> {
        if let Some(u) = self.user_id {
            return Ok(u);
        }
        let r = self.call("getUserID", json!({ "system": { "get_user_id": "null" } }))?;
        let u = r.get("user_id").and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).ok_or_else(|| anyhow!("la caméra ne donne pas d'identifiant de lecture"))?;
        self.user_id = Some(u);
        Ok(u)
    }

    /// Jours qui ont des enregistrements entre deux dates.
    pub fn recording_days(&mut self, from: NaiveDate, to: NaiveDate) -> Result<Vec<NaiveDate>> {
        let r = self.call(
            "searchDateWithVideo",
            json!({ "playback": { "search_year_utility": { "channel": [0], "start_date": from.format("%Y%m%d").to_string(), "end_date": to.format("%Y%m%d").to_string() } } }),
        )?;
        let list = r.pointer("/playback/search_results").ok_or_else(|| anyhow!("pas de relecture sur cette caméra (carte SD absente ?)"))?;
        let mut days: Vec<NaiveDate> = Vec::new();
        collect(list, &mut |o| {
            if let Some(d) = o.get("date").and_then(Value::as_str).and_then(|d| NaiveDate::parse_from_str(d, "%Y%m%d").ok()) {
                days.push(d);
            }
        });
        days.sort();
        days.dedup();
        Ok(days)
    }

    /// Séquences enregistrées un jour (bornes en secondes Unix, horloge de la caméra).
    pub fn recordings(&mut self, date: NaiveDate) -> Result<Vec<(i64, i64)>> {
        let mut out = Vec::new();
        let mut retried = false;
        let mut start = 0usize;
        loop {
            let id = self.user_id()?;
            let r = self.call(
                "searchVideoOfDay",
                json!({ "playback": { "search_video_utility": { "channel": 0, "date": date.format("%Y%m%d").to_string(), "start_index": start, "end_index": start + 99, "id": id } } }),
            );
            let r = match r {
                // Identifiant de lecture expiré : on en redemande un, une fois.
                Err(e) if !retried && format!("{e:#}").contains("identifiant de lecture") => {
                    retried = true;
                    self.user_id = None;
                    continue;
                }
                r => r?,
            };
            let list = r.pointer("/playback/search_video_results").ok_or_else(|| anyhow!("pas de relecture sur cette caméra (carte SD absente ?)"))?;
            let before = out.len();
            collect(list, &mut |o| {
                let n = |k: &str| o.get(k).and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
                if let (Some(a), Some(b)) = (n("startTime"), n("endTime")) {
                    if b > a {
                        out.push((a, b));
                    }
                }
            });
            // Pages de 100 séquences.
            if out.len() - before < 100 {
                break;
            }
            start += 100;
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}

/// Parcourt un JSON et appelle `f` sur chaque objet (les réponses de la relecture imbriquent
/// leurs éléments sous des clés numérotées : `video0`, `search_results_1`…).
fn collect(v: &Value, f: &mut dyn FnMut(&serde_json::Map<String, Value>)) {
    match v {
        Value::Object(o) => {
            f(o);
            for x in o.values() {
                collect(x, f);
            }
        }
        Value::Array(a) => {
            for x in a {
                collect(x, f);
            }
        }
        _ => {}
    }
}

fn on_off(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "off"
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceInfo {
    pub model: String,
    pub firmware: String,
    pub hardware: String,
    pub name: String,
    pub mac: String,
}

/// Alarme de la caméra : sirène (son) et lumière, déclenchées par sa propre détection.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AlarmConfig {
    /// Alarme déclenchée par la détection de la caméra.
    pub enabled: bool,
    pub sound: bool,
    pub light: bool,
    /// Volume de la sirène (« low », « normal », « high », ou un nombre selon le modèle).
    pub volume: Option<String>,
    /// Durée d'une alarme, en secondes.
    pub duration: Option<u32>,
}

impl AlarmConfig {
    pub fn from_json(a: &Value) -> AlarmConfig {
        let modes: Vec<&str> = a.get("alarm_mode").and_then(Value::as_array).map(|m| m.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        AlarmConfig {
            enabled: a.get("enabled").and_then(Value::as_str) == Some("on"),
            sound: modes.iter().any(|m| *m == "sound" || *m == "siren"),
            light: modes.contains(&"light"),
            volume: a.get("alarm_volume").map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())),
            duration: a.get("alarm_duration").and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).map(|d| d as u32),
        }
    }

    pub fn modes(&self) -> Vec<&'static str> {
        let mut m = Vec::new();
        if self.sound {
            m.push("sound");
        }
        if self.light {
            m.push("light");
        }
        m
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct VideoQuality {
    /// « 2304×1296 »
    pub resolution: Option<String>,
    pub bitrate_kbps: Option<u32>,
    /// « vbr » / « cbr »
    pub bitrate_type: Option<String>,
    pub encode_type: Option<String>,
    pub quality: Option<String>,
    pub smart_codec: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SdCard {
    /// « normal », « unformatted », « offline »…
    pub status: String,
    pub total: String,
    pub free: String,
    pub video_total: String,
    pub video_free: String,
}

/// Connexions partagées par caméra (adresse et empreinte) : une seule session, réutilisée par
/// toutes les fonctions, pour ne pas multiplier les `login` (chacun compte pour le verrouillage).
pub fn shared(host: &str, secret: &TapoSecret) -> Arc<Mutex<TapoCam>> {
    shared_at(&format!("https://{host}:{API_PORT}"), secret)
}

pub fn shared_at(base: &str, secret: &TapoSecret) -> Arc<Mutex<TapoCam>> {
    static POOL: OnceLock<Mutex<HashMap<(String, String), Arc<Mutex<TapoCam>>>>> = OnceLock::new();
    let key = (base.to_string(), format!("{}{}", secret.md5, secret.sha256));
    POOL.get_or_init(Default::default).lock().unwrap().entry(key).or_insert_with(|| Arc::new(Mutex::new(TapoCam::with_base(base, secret)))).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proofs_and_keys() {
        let s = TapoSecret::from_password("secret");
        assert_eq!(s.md5, "5EBE2294ECD0E0F08EAB7690D2A6EE69");
        assert_eq!(s.sha256, "2BB80D537B1DA3E38BD30361AA855686BDE0EACD7162FEF6A25FE97BF527A25B");
        let (c, n) = ("0123456789ABCDEF", "FEDCBA9876543210");
        let confirm = device_confirm(c, &s.sha256, n);
        assert!(confirm.ends_with(&format!("{n}{c}")) && confirm.len() == 64 + 32);
        assert_ne!(confirm, device_confirm(c, &s.md5, n));
        let d = digest_passwd(&s.sha256, c, n);
        assert!(d.ends_with(&format!("{c}{n}")));
        assert_ne!(session_key("lsk", c, n, &s.sha256), session_key("ivb", c, n, &s.sha256));
        assert_eq!(tag(&s.sha256, c, "{}", 1).len(), 64);
        assert_ne!(tag(&s.sha256, c, "{}", 1), tag(&s.sha256, c, "{}", 2));
        // Verrouillage annoncé, essais restants.
        assert_eq!(suspension(&json!({ "result": { "data": { "sec_left": 1500 } } })), Some(1500));
        assert_eq!(suspension(&json!({ "result": { "data": { "code": -40401, "time": 9, "max_time": 10 } } })), None);
        assert_eq!(attempts_left(&json!({ "result": { "data": { "time": 8, "max_time": 10 } } })), Some(8));
    }

    #[test]
    fn alarm_and_recordings_parsing() {
        let a = AlarmConfig::from_json(&json!({ "enabled": "on", "alarm_type": "0", "light_type": "0", "alarm_mode": ["sound", "light"], "alarm_volume": "high", "alarm_duration": "30" }));
        assert_eq!(a, AlarmConfig { enabled: true, sound: true, light: true, volume: Some("high".into()), duration: Some(30) });
        assert_eq!(AlarmConfig::from_json(&json!({ "enabled": "off", "alarm_mode": ["siren"] })).modes(), vec!["sound"]);
        let mut found = Vec::new();
        collect(&json!([{ "search_video_results_0": { "startTime": 100, "endTime": 160 } }, { "video1": { "startTime": "200", "endTime": "230" } }]), &mut |o| {
            if let (Some(a), Some(b)) = (o.get("startTime"), o.get("endTime")) {
                found.push((a.clone(), b.clone()));
            }
        });
        assert_eq!(found.len(), 2);
    }
}
