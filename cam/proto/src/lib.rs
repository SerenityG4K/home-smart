//! Protocole entre HomeSmart et une caméra HomeSmart (caméra fabriquée soi-même).
//!
//! L'agent de la caméra (`homesmart-cam`) s'annonce en mDNS (`SERVICE_TYPE`), se pilote en
//! HTTP + JSON et diffuse deux flux RTSP : `/main` (4K, pour regarder) et `/sub` (léger, pour
//! l'analyse et les vignettes), avec le micro en PCM 16 bits à 16 kHz (L16).
//!
//! Sécurité sans TLS ni compte :
//! - appairage par code (8 caractères, donné à l'installation) en SPAKE2 : le code ne passe
//!   jamais sur le réseau et un espion ne peut pas le deviner hors ligne ; chaque essai coûte
//!   une requête à la caméra, qui se verrouille après quelques échecs ;
//! - ensuite, chaque requête est signée (HMAC-SHA256 du secret partagé, de la méthode, du
//!   chemin, de l'heure, d'un nonce et du corps) : ni falsification ni rejeu ;
//! - le mot de passe RTSP est dérivé du même secret : il ne circule jamais.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};

type HmacSha256 = Hmac<Sha256>;

/// Type de service mDNS annoncé par les caméras.
pub const SERVICE_TYPE: &str = "_homesmart-cam._tcp.local.";
/// Version du protocole (annoncée dans `Info` et dans l'annonce mDNS).
pub const PROTOCOL: u32 = 1;
/// Port HTTP par défaut de l'agent.
pub const API_PORT: u16 = 8780;
/// Port RTSP par défaut (serveur MediaMTX de la caméra).
pub const RTSP_PORT: u16 = 8554;
pub const MAIN_PATH: &str = "/main";
pub const SUB_PATH: &str = "/sub";
/// Écart d'horloge toléré entre HomeSmart et la caméra pour une requête signée.
pub const MAX_CLOCK_SKEW_S: i64 = 300;

/// En-têtes d'une requête signée.
pub const H_CLIENT: &str = "x-hs-client";
pub const H_TIME: &str = "x-hs-time";
pub const H_NONCE: &str = "x-hs-nonce";
pub const H_SIG: &str = "x-hs-sig";

// ---- Réglages et état ------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    H264,
    H265,
}

impl VideoCodec {
    pub fn label(self) -> &'static str {
        match self {
            VideoCodec::H264 => "H.264",
            VideoCodec::H265 => "H.265",
        }
    }
}

/// Réglage d'un flux vidéo.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct StreamSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Débit visé, en kbit/s.
    pub kbps: u32,
    pub codec: VideoCodec,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct AudioSettings {
    /// Micro actif (coupé : aucune piste son dans les flux).
    pub enabled: bool,
    /// Gain appliqué au micro, en dB (−20 … +30).
    #[serde(default)]
    pub gain_db: f32,
}

/// Tout ce que HomeSmart peut régler sur la caméra.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Settings {
    pub name: String,
    pub main: StreamSettings,
    pub sub: StreamSettings,
    pub audio: AudioSettings,
    /// Image tournée d'un demi-tour (caméra fixée au plafond).
    #[serde(default)]
    pub rotate_180: bool,
}

/// Résolutions proposées dans HomeSmart (largeur, hauteur, nom).
pub const RESOLUTIONS: [(u32, u32, &str); 5] = [(3840, 2160, "4K"), (2560, 1440, "1440p"), (1920, 1080, "1080p"), (1280, 720, "720p"), (640, 360, "360p")];

/// Ce que le matériel sait faire.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Caps {
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
    pub codecs: Vec<VideoCodec>,
    /// Un micro est branché.
    pub audio: bool,
    /// Capteur d'image, s'il est connu (« Sony IMX415 »).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensor: Option<String>,
}

impl Caps {
    /// Débit conseillé pour une image de `w`×`h` à `fps` (≈ 8 Mbit/s en 4K à 30 i/s en H.265,
    /// le double en H.264).
    pub fn suggested_kbps(w: u32, h: u32, fps: u32, codec: VideoCodec) -> u32 {
        let per_pixel_frame = match codec {
            VideoCodec::H265 => 0.032,
            VideoCodec::H264 => 0.064,
        };
        let kbps = w as f64 * h as f64 * fps.max(1) as f64 * per_pixel_frame / 1000.0;
        (kbps.round() as u32).clamp(300, 40_000)
    }

    /// Réglages de départ : flux principal à la définition maximale, flux secondaire en 720p H.264.
    pub fn default_settings(&self, name: &str) -> Settings {
        let fps = self.max_fps.clamp(1, 30);
        let codec = if self.codecs.contains(&VideoCodec::H265) { VideoCodec::H265 } else { VideoCodec::H264 };
        let (w, h) = (self.max_width, self.max_height);
        // Flux secondaire : 1280 de large, même proportion que le principal (hauteur paire).
        let (sw, sh) = if w > 1280 { (1280, ((1280 * h as u64 / w.max(1) as u64) as u32) & !1) } else { (w, h) };
        Settings {
            name: name.to_string(),
            main: StreamSettings { width: w, height: h, fps, kbps: Caps::suggested_kbps(w, h, fps, codec), codec },
            sub: StreamSettings { width: sw, height: sh, fps, kbps: Caps::suggested_kbps(sw, sh, fps, VideoCodec::H264), codec: VideoCodec::H264 },
            audio: AudioSettings { enabled: self.audio, gain_db: 0.0 },
            rotate_180: false,
        }
    }
}

impl Settings {
    /// Vérifie que le matériel peut tenir ces réglages ; le message dit ce qui ne va pas.
    pub fn validate(&self, caps: &Caps) -> Result<(), String> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            return Err("le nom doit faire de 1 à 64 caractères".into());
        }
        for (label, s, kbps_max) in [("principal", &self.main, 40_000), ("secondaire", &self.sub, 8_000)] {
            if s.width < 320 || s.height < 180 || s.width % 2 != 0 || s.height % 2 != 0 {
                return Err(format!("flux {label} : définition {}×{} invalide (paire, au moins 320×180)", s.width, s.height));
            }
            if s.width > caps.max_width || s.height > caps.max_height {
                return Err(format!("flux {label} : {}×{} dépasse ce que la caméra sait faire ({}×{})", s.width, s.height, caps.max_width, caps.max_height));
            }
            if s.fps == 0 || s.fps > caps.max_fps {
                return Err(format!("flux {label} : {} images/s hors limites (1 à {})", s.fps, caps.max_fps));
            }
            if !(100..=kbps_max).contains(&s.kbps) {
                return Err(format!("flux {label} : débit de {} kbit/s hors limites (100 à {kbps_max})", s.kbps));
            }
            if !caps.codecs.contains(&s.codec) {
                return Err(format!("flux {label} : la caméra n'encode pas en {}", s.codec.label()));
            }
        }
        if self.sub.width > self.main.width || self.sub.height > self.main.height {
            return Err("le flux secondaire doit être plus petit que le principal".into());
        }
        if !(-20.0..=30.0).contains(&self.audio.gain_db) {
            return Err("gain du micro hors limites (−20 à +30 dB)".into());
        }
        if self.audio.enabled && !caps.audio {
            return Err("aucun micro n'est branché sur la caméra".into());
        }
        Ok(())
    }
}

/// Présentation de la caméra (publique : lisible sans appairage).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Info {
    /// Identifiant stable (adresse MAC sans séparateurs, sinon nom de la machine).
    pub id: String,
    pub name: String,
    pub model: String,
    /// Carte (« Radxa ROCK 4D »), lue dans l'arbre des périphériques.
    pub board: String,
    /// Version de l'agent.
    pub version: String,
    pub protocol: u32,
    /// Au moins un HomeSmart est appairé.
    pub paired: bool,
    pub rtsp_port: u16,
    pub caps: Caps,
}

/// État d'un processus surveillé par l'agent (serveur RTSP, chaîne d'encodage).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct ProcStatus {
    pub running: bool,
    /// Redémarrages depuis le lancement de l'agent.
    pub restarts: u32,
    /// Secondes depuis le dernier démarrage, s'il tourne.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_s: Option<u64>,
    /// Dernière erreur (fin de la sortie d'erreur du processus).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Status {
    pub uptime_s: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_temp_c: Option<f32>,
    /// Charge moyenne sur une minute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<f32>,
    /// Chaîne capteur → encodeur → flux.
    pub pipeline: ProcStatus,
    /// Serveur RTSP.
    pub server: ProcStatus,
    /// HomeSmart appairés.
    pub clients: usize,
}

/// Réponse d'erreur de l'API.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ApiError {
    pub error: String,
}

// ---- Appairage -------------------------------------------------------------------------

/// Alphabet des codes : sans 0/O, 1/I/L (dictés ou recopiés sans erreur).
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
pub const CODE_LEN: usize = 8;

/// Nouveau code d'appairage (8 caractères, ≈ 40 bits).
pub fn new_code() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..CODE_LEN).map(|_| CODE_ALPHABET[rng.gen_range(0..CODE_ALPHABET.len())] as char).collect()
}

/// Code tel qu'on l'écrit : « ABCD-EFGH ».
pub fn format_code(code: &str) -> String {
    let c = normalize_code(code);
    if c.len() == CODE_LEN {
        format!("{}-{}", &c[..4], &c[4..])
    } else {
        c
    }
}

/// Code tapé par l'utilisateur → forme canonique (majuscules, sans tirets ni espaces).
pub fn normalize_code(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

const ID_HUB: &[u8] = b"homesmart";
const ID_CAM: &[u8] = b"homesmart-cam";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PairStart {
    /// Nom du HomeSmart qui s'appaire (affiché sur la caméra).
    pub client_name: String,
    /// Message SPAKE2 (côté A), en hexadécimal.
    pub msg: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PairChallenge {
    pub pair_id: String,
    /// Message SPAKE2 (côté B), en hexadécimal.
    pub msg: String,
    /// Preuve que la caméra connaît le code.
    pub confirm: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PairFinish {
    pub pair_id: String,
    /// Preuve que HomeSmart connaît le code.
    pub confirm: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Paired {
    /// Identifiant de ce HomeSmart sur la caméra (et utilisateur RTSP).
    pub client_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairError {
    /// Message illisible.
    Malformed,
    /// Le code ne correspond pas (preuve fausse).
    WrongCode,
}

impl std::fmt::Display for PairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PairError::Malformed => "réponse d'appairage illisible",
            PairError::WrongCode => "code d'appairage incorrect",
        })
    }
}

impl std::error::Error for PairError {}

fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC accepte toute clé");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

fn hmac_matches(key: &[u8], parts: &[&[u8]], tag_hex: &str) -> bool {
    let Ok(tag) = hex::decode(tag_hex) else { return false };
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC accepte toute clé");
    for p in parts {
        m.update(p);
    }
    m.verify_slice(&tag).is_ok()
}

/// Clé de session SPAKE2 → (preuve de la caméra, preuve de HomeSmart, secret durable).
fn derive(key: &[u8], msg_a: &[u8], msg_b: &[u8]) -> ([u8; 32], [u8; 32], [u8; 32]) {
    (hmac(key, &[b"cam-confirm", msg_a, msg_b]), hmac(key, &[b"hub-confirm", msg_a, msg_b]), hmac(key, &[b"homesmart-cam secret v1", msg_a, msg_b]))
}

/// Côté HomeSmart : appairage en cours.
pub struct HubPairing {
    state: Spake2<Ed25519Group>,
    msg_a: Vec<u8>,
}

impl HubPairing {
    /// Démarre avec le code tapé ; renvoie la requête à envoyer à la caméra.
    pub fn start(code: &str, client_name: &str) -> (HubPairing, PairStart) {
        let (state, msg_a) = Spake2::<Ed25519Group>::start_a(&Password::new(normalize_code(code).as_bytes()), &Identity::new(ID_HUB), &Identity::new(ID_CAM));
        let req = PairStart { client_name: client_name.to_string(), msg: hex::encode(&msg_a) };
        (HubPairing { state, msg_a }, req)
    }

    /// Vérifie la réponse de la caméra : renvoie le secret durable et la preuve à renvoyer.
    pub fn finish(self, ch: &PairChallenge) -> Result<([u8; 32], PairFinish), PairError> {
        let msg_b = hex::decode(&ch.msg).map_err(|_| PairError::Malformed)?;
        let key = self.state.finish(&msg_b).map_err(|_| PairError::Malformed)?;
        let (cam, hub, secret) = derive(&key, &self.msg_a, &msg_b);
        if !hmac_matches(&key, &[b"cam-confirm", &self.msg_a, &msg_b], &ch.confirm) {
            return Err(PairError::WrongCode);
        }
        debug_assert_eq!(hex::encode(cam), ch.confirm);
        Ok((secret, PairFinish { pair_id: ch.pair_id.clone(), confirm: hex::encode(hub) }))
    }
}

/// Côté caméra : réponse à une demande d'appairage.
pub struct CamAnswer {
    /// Message et preuve à renvoyer (le `pair_id` est fixé par l'agent).
    pub msg: String,
    pub confirm: String,
    /// Secret durable, à retenir si HomeSmart prouve à son tour qu'il connaît le code.
    pub secret: [u8; 32],
    msg_a: Vec<u8>,
    msg_b: Vec<u8>,
    key: Vec<u8>,
}

impl CamAnswer {
    pub fn new(code: &str, req: &PairStart) -> Result<CamAnswer, PairError> {
        let msg_a = hex::decode(&req.msg).map_err(|_| PairError::Malformed)?;
        let (state, msg_b) = Spake2::<Ed25519Group>::start_b(&Password::new(normalize_code(code).as_bytes()), &Identity::new(ID_HUB), &Identity::new(ID_CAM));
        let key = state.finish(&msg_a).map_err(|_| PairError::Malformed)?;
        let (cam, _, secret) = derive(&key, &msg_a, &msg_b);
        Ok(CamAnswer { msg: hex::encode(&msg_b), confirm: hex::encode(cam), secret, msg_a, msg_b, key })
    }

    /// La preuve de HomeSmart est-elle juste (même code des deux côtés) ?
    pub fn check(&self, fin: &PairFinish) -> bool {
        hmac_matches(&self.key, &[b"hub-confirm", &self.msg_a, &self.msg_b], &fin.confirm)
    }
}

// ---- Requêtes signées ------------------------------------------------------------------

/// Signature d'une requête (hexadécimal).
pub fn signature(secret: &[u8], method: &str, path: &str, time: i64, nonce: &str, body: &[u8]) -> String {
    hex::encode(hmac(secret, &[signed_text(method, path, time, nonce, body).as_bytes()]))
}

/// La signature reçue est-elle celle de cette requête ? (comparaison à temps constant)
pub fn verify_signature(secret: &[u8], method: &str, path: &str, time: i64, nonce: &str, body: &[u8], sig: &str) -> bool {
    hmac_matches(secret, &[signed_text(method, path, time, nonce, body).as_bytes()], sig)
}

fn signed_text(method: &str, path: &str, time: i64, nonce: &str, body: &[u8]) -> String {
    format!("v1\n{}\n{path}\n{time}\n{nonce}\n{}", method.to_ascii_uppercase(), hex::encode(Sha256::digest(body)))
}

/// Nonce aléatoire d'une requête signée.
pub fn new_nonce() -> String {
    hex::encode(rand::random::<[u8; 12]>())
}

/// Mot de passe RTSP de ce HomeSmart, dérivé du secret (32 caractères hexadécimaux).
pub fn rtsp_password(secret: &[u8]) -> String {
    hex::encode(&hmac(secret, &[b"rtsp"])[..16])
}

pub fn secret_to_hex(secret: &[u8; 32]) -> String {
    hex::encode(secret)
}

pub fn secret_from_hex(s: &str) -> Option<[u8; 32]> {
    hex::decode(s.trim()).ok()?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> Caps {
        Caps { max_width: 3840, max_height: 2160, max_fps: 30, codecs: vec![VideoCodec::H264, VideoCodec::H265], audio: true, sensor: Some("Sony IMX415".into()) }
    }

    #[test]
    fn pairing_with_the_right_code() {
        let (hub, start) = HubPairing::start("abcd-efgh", "PC du salon");
        let cam = CamAnswer::new("ABCDEFGH", &start).unwrap();
        let ch = PairChallenge { pair_id: "p1".into(), msg: cam.msg.clone(), confirm: cam.confirm.clone() };
        let (secret, fin) = hub.finish(&ch).unwrap();
        assert!(cam.check(&fin));
        assert_eq!(secret, cam.secret);
        assert_eq!(fin.pair_id, "p1");
        assert_eq!(rtsp_password(&secret).len(), 32);
        assert_eq!(secret_from_hex(&secret_to_hex(&secret)), Some(secret));
    }

    #[test]
    fn pairing_with_a_wrong_code_fails_on_both_sides() {
        let (hub, start) = HubPairing::start("ABCDEFGH", "PC");
        let cam = CamAnswer::new("ABCDEFGX", &start).unwrap();
        let ch = PairChallenge { pair_id: "p".into(), msg: cam.msg.clone(), confirm: cam.confirm.clone() };
        assert_eq!(hub.finish(&ch).unwrap_err(), PairError::WrongCode);
        // Un HomeSmart qui mentirait sur sa preuve est refusé par la caméra.
        let (hub2, start2) = HubPairing::start("ABCDEFGH", "PC");
        let cam2 = CamAnswer::new("ABCDEFGX", &start2).unwrap();
        let forged = PairFinish { pair_id: "p".into(), confirm: hex::encode([0u8; 32]) };
        assert!(!cam2.check(&forged));
        drop(hub2);
        assert!(CamAnswer::new("ABCDEFGH", &PairStart { client_name: "x".into(), msg: "zz".into() }).is_err());
    }

    #[test]
    fn codes() {
        let c = new_code();
        assert_eq!(c.len(), CODE_LEN);
        assert!(c.bytes().all(|b| CODE_ALPHABET.contains(&b)));
        assert_eq!(format_code("abcdefgh"), "ABCD-EFGH");
        assert_eq!(normalize_code(" abcd-EFGH "), "ABCDEFGH");
    }

    #[test]
    fn signed_requests() {
        let secret = [7u8; 32];
        let sig = signature(&secret, "put", "/api/settings", 1_800_000_000, "n1", b"{}");
        assert!(verify_signature(&secret, "PUT", "/api/settings", 1_800_000_000, "n1", b"{}", &sig));
        // Tout changement casse la signature : corps, chemin, heure, nonce, clé.
        assert!(!verify_signature(&secret, "PUT", "/api/settings", 1_800_000_000, "n1", b"{ }", &sig));
        assert!(!verify_signature(&secret, "PUT", "/api/status", 1_800_000_000, "n1", b"{}", &sig));
        assert!(!verify_signature(&secret, "PUT", "/api/settings", 1_800_000_001, "n1", b"{}", &sig));
        assert!(!verify_signature(&secret, "PUT", "/api/settings", 1_800_000_000, "n2", b"{}", &sig));
        assert!(!verify_signature(&[8u8; 32], "PUT", "/api/settings", 1_800_000_000, "n1", b"{}", &sig));
        assert!(!verify_signature(&secret, "PUT", "/api/settings", 1_800_000_000, "n1", b"{}", "pas-hex"));
    }

    #[test]
    fn default_settings_fit_the_hardware() {
        let s = caps().default_settings("Salon");
        assert_eq!((s.main.width, s.main.height, s.main.fps, s.main.codec), (3840, 2160, 30, VideoCodec::H265));
        assert_eq!((s.sub.width, s.sub.height, s.sub.codec), (1280, 720, VideoCodec::H264));
        assert!((7_000..=9_000).contains(&s.main.kbps), "{}", s.main.kbps);
        s.validate(&caps()).unwrap();
        // Webcam 1080p H.264 seulement, sans micro.
        let small = Caps { max_width: 1920, max_height: 1080, max_fps: 30, codecs: vec![VideoCodec::H264], audio: false, sensor: None };
        let s = small.default_settings("Bureau");
        assert_eq!((s.main.codec, s.audio.enabled, s.sub.width, s.sub.height), (VideoCodec::H264, false, 1280, 720));
        s.validate(&small).unwrap();
    }

    #[test]
    fn validation_explains_what_is_wrong() {
        let c = caps();
        let ok = c.default_settings("Salon");
        let mut s = ok.clone();
        s.main.width = 7680;
        assert!(s.validate(&c).unwrap_err().contains("dépasse"));
        let mut s = ok.clone();
        s.main.fps = 60;
        assert!(s.validate(&c).unwrap_err().contains("images/s"));
        let mut s = ok.clone();
        s.sub = s.main;
        s.sub.kbps = 4000;
        s.sub.width = 3840;
        s.main.width = 1920;
        assert!(s.validate(&c).is_err());
        let mut s = ok.clone();
        s.name = " ".into();
        assert!(s.validate(&c).is_err());
        let mut s = ok;
        s.audio.enabled = true;
        let mut no_mic = c.clone();
        no_mic.audio = false;
        assert!(s.validate(&no_mic).unwrap_err().contains("micro"));
        // Les réglages voyagent en JSON tels quels.
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"codec\":\"h265\""));
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), s);
    }
}
