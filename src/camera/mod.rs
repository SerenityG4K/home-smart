//! Caméras IP en local : Imou / Dahua d'abord, et toute caméra RTSP + ONVIF.
//!
//! Rien ne passe par le cloud : la vidéo arrive en RTSP directement depuis la caméra,
//! elle est décodée sur le PC, et l'orientation se pilote en ONVIF. Le flux n'est ouvert
//! que pendant qu'on le regarde (ou pour une capture) : au repos, rien n'est décodé.
//!
//! Les caméras HomeSmart (fabriquées soi-même, agent `homesmart-cam`) se pilotent par leur
//! agent au lieu d'ONVIF : voir `agent`.

pub mod agent;
pub mod audio;
pub mod discover;
pub mod enhance;
pub mod ffmpeg;
pub mod host_wifi;
pub mod onvif;
pub mod playback;
pub mod sim;
pub mod stream;
mod xml;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const RTSP_PORT: u16 = 554;
pub const ONVIF_PORT: u16 = 80;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Brand {
    #[default]
    Imou,
    Dahua,
    Tapo,
    Reolink,
    Hikvision,
    Amcrest,
    Axis,
    Foscam,
    /// Caméra HomeSmart, fabriquée soi-même (agent `homesmart-cam`, appairage par code).
    #[serde(rename = "homesmart")]
    HomeSmart,
    Generic,
}

impl Brand {
    pub const ALL: [Brand; 10] = [Brand::Imou, Brand::Dahua, Brand::Tapo, Brand::Reolink, Brand::Hikvision, Brand::Amcrest, Brand::Axis, Brand::Foscam, Brand::HomeSmart, Brand::Generic];

    pub fn label(self) -> &'static str {
        match self {
            Brand::Imou => "Imou",
            Brand::Dahua => "Dahua",
            Brand::Tapo => "TP-Link Tapo",
            Brand::Reolink => "Reolink",
            Brand::Hikvision => "Hikvision",
            Brand::Amcrest => "Amcrest",
            Brand::Axis => "Axis",
            Brand::Foscam => "Foscam",
            Brand::HomeSmart => "Caméra HomeSmart (fait maison)",
            Brand::Generic => "Autre (RTSP / ONVIF)",
        }
    }

    /// Chemins RTSP habituels de la marque : (flux principal, flux secondaire).
    /// `None` pour « Autre » : le chemin est demandé à la caméra en ONVIF.
    pub fn default_paths(self) -> Option<(&'static str, &'static str)> {
        Some(match self {
            Brand::Imou | Brand::Dahua | Brand::Amcrest => ("/cam/realmonitor?channel=1&subtype=0", "/cam/realmonitor?channel=1&subtype=1"),
            Brand::Tapo => ("/stream1", "/stream2"),
            Brand::Reolink => ("/h264Preview_01_main", "/h264Preview_01_sub"),
            Brand::Hikvision => ("/Streaming/Channels/101", "/Streaming/Channels/102"),
            Brand::Axis => ("/axis-media/media.amp", "/axis-media/media.amp?resolution=640x360"),
            Brand::Foscam => ("/videoMain", "/videoSub"),
            Brand::HomeSmart => (homesmart_cam_proto::MAIN_PATH, homesmart_cam_proto::SUB_PATH),
            Brand::Generic => return None,
        })
    }

    /// Utilisateur par défaut de la marque (vide : choisi par l'utilisateur, comme le « compte
    /// de la caméra » des Tapo).
    pub fn default_user(self) -> &'static str {
        match self {
            Brand::Axis => "root",
            // Caméra HomeSmart : l'utilisateur et le mot de passe naissent de l'appairage.
            Brand::Tapo | Brand::HomeSmart => "",
            _ => "admin",
        }
    }

    /// Port RTSP habituel : 8554 sur une caméra HomeSmart (MediaMTX), 554 ailleurs.
    pub fn default_rtsp_port(self) -> u16 {
        match self {
            Brand::HomeSmart => homesmart_cam_proto::RTSP_PORT,
            _ => RTSP_PORT,
        }
    }

    /// Pilotage en ONVIF (orientation, réglages d'image, positions) : pas sur une caméra
    /// HomeSmart, réglée par son agent.
    pub fn has_onvif(self) -> bool {
        self != Brand::HomeSmart
    }

    /// Port ONVIF habituel : 2020 sur les Tapo, 80 ailleurs.
    pub fn default_onvif_port(self) -> u16 {
        match self {
            Brand::Tapo => 2020,
            _ => ONVIF_PORT,
        }
    }

    /// Marque d'après un fabricant ou un nom annoncé (ONVIF, découverte), sans tenir compte de la casse.
    pub fn from_vendor(name: &str) -> Option<Brand> {
        let n = name.to_ascii_lowercase();
        let has = |k: &str| n.contains(k);
        Some(if has("tp-link") || has("tplink") || has("tapo") || has("tp-ipc") {
            Brand::Tapo
        } else if has("imou") || n.trim() == "lc" {
            Brand::Imou
        } else if has("dahua") {
            Brand::Dahua
        } else if has("reolink") {
            Brand::Reolink
        } else if has("hikvision") {
            Brand::Hikvision
        } else if has("amcrest") {
            Brand::Amcrest
        } else if has("axis") {
            Brand::Axis
        } else if has("foscam") {
            Brand::Foscam
        } else if has("homesmart") || has("fait maison") {
            Brand::HomeSmart
        } else {
            return None;
        })
    }

    /// Comment obtenir les identifiants de la caméra, si la marque a une particularité.
    pub fn account_help(self) -> Option<&'static str> {
        match self {
            Brand::Imou => Some(
                "Sur l'étiquette collée sous la caméra (ou sur la boîte) : 8 caractères après « SC ». \
                 S'il a été changé dans Imou Life, c'est le nouveau mot de passe de l'appareil. \
                 L'utilisateur est « admin ».",
            ),
            Brand::Tapo => Some(
                "Une Tapo n'accepte pas le mot de passe de ton compte TP-Link en local : il lui faut un \
                 « compte de la caméra ». Crée-le une fois dans l'application Tapo : la caméra, \
                 Paramètres (roue dentée), Paramètres avancés, Compte de la caméra. Choisis un nom \
                 d'utilisateur et un mot de passe, puis recopie-les ici. Ensuite, tout passe en local \
                 (vidéo RTSP sur le port 554, orientation ONVIF sur le port 2020), même sans Internet. \
                 Si une mise à jour coupe l'accès, active « Compatibilité avec les services tiers » \
                 dans l'application (Moi, Tapo Lab) si ton modèle la propose.",
            ),
            Brand::HomeSmart => Some(
                "Le code d'appairage (8 caractères, « ABCD-EFGH ») s'affiche à l'installation de \
                 l'agent sur la caméra ; pour le revoir : `sudo -u homesmart-cam homesmart-cam code` \
                 sur la caméra. Il ne passe jamais sur le réseau : HomeSmart et la caméra se le \
                 prouvent mutuellement (SPAKE2), puis la caméra ne répond plus qu'à ce HomeSmart. \
                 Après 5 codes faux, elle refuse les essais pendant 10 minutes.",
            ),
            _ => None,
        }
    }
}

/// Sépare une URL RTSP annoncée par la caméra en (port, chemin avec requête).
pub fn split_rtsp_uri(uri: &str) -> Option<(Option<u16>, String)> {
    let rest = uri.trim().strip_prefix("rtsp://").or_else(|| uri.trim().strip_prefix("rtsps://"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let port = host.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok());
    Some((port, path.replace("&amp;", "&")))
}

/// Flux principal (HD) ou secondaire (léger, idéal pour les vignettes).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    #[default]
    Sub,
    Main,
}

impl Quality {
    pub fn label(self) -> &'static str {
        match self {
            Quality::Sub => "fluide",
            Quality::Main => "HD",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CameraConfig {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// Adresse IP (ou nom) de la caméra.
    pub host: String,
    #[serde(default = "default_rtsp")]
    pub rtsp_port: u16,
    #[serde(default = "default_onvif")]
    pub onvif_port: u16,
    #[serde(default = "default_user")]
    pub username: String,
    /// Imou : le code de sécurité imprimé sous la caméra (« SC »), sauf s'il a été changé.
    /// Rangé dans les secrets (`config::Secrets`), pas dans `homesmart.json`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub password: String,
    #[serde(default)]
    pub brand: Brand,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    /// Chemins RTSP personnalisés (caméras d'autres marques) : flux principal et secondaire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_path: Option<String>,
    /// Orientation inversée (caméra posée à l'envers, ou firmware qui inverse les axes ONVIF).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub invert_tilt: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub invert_pan: bool,
    /// Caméra HomeSmart : lien avec son agent, établi à l'appairage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentLink>,
    /// Caméra Tapo : empreintes du mot de passe du compte TP-Link (jamais le mot de passe), pour
    /// son API locale (alarme, suivi des mouvements, carte SD). Rangées dans les secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tapo: Option<crate::tplink::camera::TapoSecret>,
    /// Détection de sabotage (objectif masqué, caméra détournée, image floutée).
    #[serde(default, skip_serializing_if = "TamperConfig::is_default")]
    pub tamper: TamperConfig,
}

/// Sensibilité de la détection de sabotage.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum TamperSensitivity {
    Low,
    #[default]
    Medium,
    High,
}

impl TamperSensitivity {
    pub const ALL: [TamperSensitivity; 3] = [TamperSensitivity::Low, TamperSensitivity::Medium, TamperSensitivity::High];

    pub fn label(self) -> &'static str {
        match self {
            TamperSensitivity::Low => "faible",
            TamperSensitivity::Medium => "moyenne",
            TamperSensitivity::High => "élevée",
        }
    }

    pub fn parse(s: &str) -> Option<TamperSensitivity> {
        match s.trim().to_lowercase().as_str() {
            "low" | "faible" | "basse" => Some(TamperSensitivity::Low),
            "medium" | "moyenne" | "normale" | "normal" => Some(TamperSensitivity::Medium),
            "high" | "élevée" | "elevee" | "haute" => Some(TamperSensitivity::High),
            _ => None,
        }
    }
}

/// Détection de sabotage d'une caméra : par l'analyse de l'image dans HomeSmart, et par les
/// alertes de la caméra elle-même (ONVIF), si elle en a.
/// Désactivée par défaut : rien ne tourne (ni analyse d'image, ni abonnement ONVIF) tant que
/// l'utilisateur ne l'active pas.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TamperConfig {
    /// Analyse de l'image par HomeSmart (le flux fluide reste ouvert : un peu de processeur).
    #[serde(default)]
    pub image: bool,
    /// Écoute des alertes de sabotage de la caméra (ONVIF).
    #[serde(default)]
    pub camera: bool,
    #[serde(default)]
    pub sensitivity: TamperSensitivity,
}

impl TamperConfig {
    pub fn is_default(&self) -> bool {
        *self == TamperConfig::default()
    }
}

/// Lien avec l'agent d'une caméra HomeSmart (voir `agent`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AgentLink {
    /// Port de son API HTTP.
    pub api_port: u16,
    /// Identifiant de ce HomeSmart sur la caméra (aussi l'utilisateur RTSP).
    pub client_id: String,
    /// Secret partagé né de l'appairage (hexadécimal) : signe les requêtes, donne le mot de passe RTSP.
    /// Rangé dans les secrets (`config::Secrets`), pas dans `homesmart.json`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub secret: String,
}

fn default_rtsp() -> u16 {
    RTSP_PORT
}

fn default_onvif() -> u16 {
    ONVIF_PORT
}

fn default_user() -> String {
    "admin".into()
}

impl CameraConfig {
    pub fn new(host: &str) -> Self {
        CameraConfig {
            id: String::new(),
            name: String::new(),
            host: host.trim().to_string(),
            rtsp_port: RTSP_PORT,
            onvif_port: ONVIF_PORT,
            username: default_user(),
            password: String::new(),
            brand: Brand::Imou,
            model: None,
            serial: None,
            mac: None,
            room: None,
            main_path: None,
            sub_path: None,
            invert_tilt: false,
            invert_pan: false,
            agent: None,
            tapo: None,
            tamper: TamperConfig::default(),
        }
    }

    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.host
        } else {
            &self.name
        }
    }

    /// Identifiant stable : n° de série si connu, sinon l'adresse.
    pub fn make_id(&self) -> String {
        let base = self.serial.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| self.host.clone());
        let clean: String = base.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
        format!("cam-{clean}")
    }

    /// Chemin RTSP du flux (Imou et Dahua : `/cam/realmonitor`, `subtype` 0 = principal, 1 = secondaire).
    pub fn rtsp_path(&self, q: Quality) -> String {
        let custom = match q {
            Quality::Main => self.main_path.as_deref(),
            Quality::Sub => self.sub_path.as_deref().or(self.main_path.as_deref()),
        };
        // Sans chemin personnalisé : celui de la marque (Imou / Dahua pour « Autre », en attendant ONVIF).
        match custom.map(str::trim).filter(|p| !p.is_empty()) {
            Some(p) if p.starts_with('/') => p.to_string(),
            Some(p) => format!("/{p}"),
            None => {
                let (main, sub) = self.brand.default_paths().unwrap_or(("/cam/realmonitor?channel=1&subtype=0", "/cam/realmonitor?channel=1&subtype=1"));
                (if q == Quality::Main { main } else { sub }).to_string()
            }
        }
    }

    /// URL RTSP sans identifiants (ils sont transmis à part, en Digest).
    pub fn rtsp_url(&self, q: Quality) -> String {
        format!("rtsp://{}:{}{}", self.host, self.rtsp_port, self.rtsp_path(q))
    }
}

/// Dossier des captures : `HOMESMART_CAPTURES`, sinon `Images/HomeSmart` de l'utilisateur.
pub fn captures_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("HOMESMART_CAPTURES").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_default();
    #[cfg(not(windows))]
    if let Some(p) = std::env::var_os("XDG_PICTURES_DIR").filter(|p| !p.is_empty()) {
        return PathBuf::from(p).join("HomeSmart");
    }
    home.join("Pictures").join("HomeSmart")
}

/// Enregistre une image en PNG (compressé, sans canal alpha).
pub fn save_png(frame: &Frame, path: &Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("création de {}", dir.display()))?;
    }
    let file = std::fs::File::create(path).with_context(|| format!("écriture de {}", path.display()))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), frame.width as u32, frame.height as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let rgb: Vec<u8> = frame.rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
    enc.write_header()?.write_image_data(&rgb)?;
    Ok(())
}

/// Nom de fichier d'une capture : `<caméra> 2026-10-20 14-10-24.png`.
pub fn capture_name(camera: &str) -> String {
    let clean: String = camera.chars().map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { '_' }).collect();
    format!("{} {}.png", clean.trim(), chrono::Local::now().format("%Y-%m-%d %H-%M-%S"))
}

/// Image décodée, en RGBA.
#[derive(Clone, Debug)]
pub struct Frame {
    /// Numéro croissant dans le flux en cours.
    pub seq: u64,
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// Mesures d'un flux en cours.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamStats {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub kbps: f32,
    /// Temps moyen de décodage d'une image (ms), conversion en couleurs comprise.
    pub decode_ms: f32,
    /// Images sautées faute de temps (décodeur en retard).
    pub dropped: u64,
    /// Son reçu (ex. « AAC 16 kHz »), vide si le son n'est pas demandé.
    pub audio: String,
}

impl StreamStats {
    pub fn summary(&self) -> String {
        if self.width == 0 {
            return self.codec.clone();
        }
        let mut s = format!("{} · {}×{}", self.codec, self.width, self.height);
        if self.fps > 0.0 {
            s.push_str(&format!(" · {:.0} i/s · {:.0} kbit/s", self.fps, self.kbps));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_paths_and_uri_split() {
        let mut c = CameraConfig::new("10.0.0.5");
        c.brand = Brand::Tapo;
        assert_eq!(c.rtsp_url(Quality::Main), "rtsp://10.0.0.5:554/stream1");
        assert_eq!(c.rtsp_url(Quality::Sub), "rtsp://10.0.0.5:554/stream2");
        c.brand = Brand::Hikvision;
        assert_eq!(c.rtsp_path(Quality::Main), "/Streaming/Channels/101");
        assert_eq!(split_rtsp_uri("rtsp://192.168.1.20:8554/live/ch0?x=1&amp;y=2"), Some((Some(8554), "/live/ch0?x=1&y=2".to_string())));
        assert_eq!(split_rtsp_uri("rtsp://admin:pw@cam/stream1"), Some((None, "/stream1".to_string())));
        assert_eq!(split_rtsp_uri("http://x/"), None);
    }

    #[test]
    fn urls_and_ids() {
        let mut c = CameraConfig::new("192.168.1.164");
        assert_eq!(c.rtsp_url(Quality::Main), "rtsp://192.168.1.164:554/cam/realmonitor?channel=1&subtype=0");
        assert_eq!(c.rtsp_url(Quality::Sub), "rtsp://192.168.1.164:554/cam/realmonitor?channel=1&subtype=1");
        assert_eq!(c.make_id(), "cam-192-168-1-164");
        c.serial = Some("A1B2C3D4E5F6789".into());
        assert_eq!(c.make_id(), "cam-a1b2c3d4e5f6789");
        c.brand = Brand::Generic;
        c.main_path = Some("live/ch0".into());
        assert_eq!(c.rtsp_url(Quality::Main), "rtsp://192.168.1.164:554/live/ch0");
        assert_eq!(c.rtsp_url(Quality::Sub), "rtsp://192.168.1.164:554/live/ch0");
        // Un ancien fichier sans ports ni utilisateur reprend les valeurs Imou.
        let c: CameraConfig = serde_json::from_str(r#"{"id":"cam-x","host":"10.0.0.2"}"#).unwrap();
        assert_eq!((c.rtsp_port, c.onvif_port, c.username.as_str(), c.brand), (554, 80, "admin", Brand::Imou));
    }
}
