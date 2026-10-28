//! Configuration de l'agent (JSON) : code d'appairage, HomeSmart appairés, réglages, matériel.

use anyhow::{Context, Result};
use homesmart_cam_proto::{self as proto, Settings};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Comment l'image et le son sont produits.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// Carte Rockchip (Radxa ROCK 4D…) : capteur via l'ISP, encodage matériel (GStreamer + MPP).
    Rockchip,
    /// Toute machine Linux avec une caméra V4L2 (webcam USB) : encodage logiciel (FFmpeg).
    V4l2,
    /// Mire de test et la à 440 Hz (FFmpeg) : pour essayer sans matériel, même sous Windows.
    Test,
    /// Commande donnée dans `custom` (pour une carte qui n'est pas encore prise en charge).
    Custom,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::Rockchip => "Rockchip (encodage matériel)",
            Backend::V4l2 => "V4L2 (encodage logiciel)",
            Backend::Test => "mire de test",
            Backend::Custom => "commande personnalisée",
        }
    }
}

/// Un HomeSmart appairé.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Client {
    /// Identifiant (aussi l'utilisateur RTSP), « hs-xxxxxxxx ».
    pub id: String,
    pub name: String,
    /// Secret partagé, en hexadécimal.
    pub secret: String,
    /// Date de l'appairage (secondes Unix).
    pub paired_at: u64,
}

impl Client {
    pub fn secret_bytes(&self) -> Option<[u8; 32]> {
        proto::secret_from_hex(&self.secret)
    }
}

/// Outils externes (sinon cherchés à côté de l'agent, puis dans le PATH).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Tools {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mediamtx: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ffmpeg: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gst_launch: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AgentConfig {
    /// Identifiant stable de la caméra (adresse MAC, ou tiré au hasard à la création).
    pub id: String,
    /// Code d'appairage (forme canonique, 8 caractères).
    pub setup_code: String,
    #[serde(default)]
    pub clients: Vec<Client>,
    pub backend: Backend,
    pub settings: Settings,
    /// Périphérique vidéo (sinon celui du backend : `/dev/video-camera0` sur Rockchip, `/dev/video0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_device: Option<String>,
    /// Micro ALSA (`hw:1,0`, `plughw:CARD=Mic`…) ; sinon `default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_device: Option<String>,
    /// Micro présent : sinon détecté (une carte son qui enregistre).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_mic: Option<bool>,
    /// Ce que sait le matériel, si la détection se trompe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caps: Option<proto::Caps>,
    #[serde(default = "default_api_port")]
    pub api_port: u16,
    #[serde(default = "default_rtsp_port")]
    pub rtsp_port: u16,
    #[serde(default)]
    pub tools: Tools,
    /// Commande du backend `custom` (un argument par ligne), avec des repères remplacés au
    /// lancement : {main_url} {sub_url} {width} {height} {fps} {kbps} {codec} {sub_width}
    /// {sub_height} {sub_fps} {sub_kbps} {sub_codec} {video_device} {audio_device} {gain_db}.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom: Vec<String>,
}

fn default_api_port() -> u16 {
    proto::API_PORT
}

fn default_rtsp_port() -> u16 {
    proto::RTSP_PORT
}

/// Emplacement par défaut : `HOMESMART_CAM_CONFIG`, sinon `/var/lib/homesmart-cam/config.json`
/// (service systemd), `~/.local/state/homesmart-cam/config.json` (Linux, sans le service) ou
/// `%APPDATA%\HomeSmart\cam-agent\config.json` (Windows, essais).
pub fn default_path() -> PathBuf {
    if let Some(p) = std::env::var_os("HOMESMART_CAM_CONFIG").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    #[cfg(windows)]
    {
        let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default();
        base.join("HomeSmart").join("cam-agent").join("config.json")
    }
    #[cfg(not(windows))]
    {
        let system = Path::new("/var/lib/homesmart-cam");
        if system.is_dir() {
            return system.join("config.json");
        }
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.join(".local").join("state").join("homesmart-cam").join("config.json")
    }
}

impl AgentConfig {
    pub fn load(path: &Path) -> Result<Option<AgentConfig>> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(Some(serde_json::from_str(&s).with_context(|| format!("lecture de {}", path.display()))?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("ouverture de {}", path.display())),
        }
    }

    /// Écriture atomique, lisible par le seul propriétaire (le fichier contient des secrets).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("création de {}", dir.display()))?;
        }
        write_private(path, serde_json::to_string_pretty(self)?.as_bytes())
    }

    pub fn client(&self, id: &str) -> Option<&Client> {
        self.clients.iter().find(|c| c.id == id)
    }
}

/// Écrit un fichier privé (droits 600 sous Linux) par fichier temporaire puis renommage.
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).with_context(|| format!("écriture de {}", tmp.display()))?;
        f.write_all(data)?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, data).with_context(|| format!("écriture de {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("écriture de {}", path.display()))?;
    Ok(())
}

/// Identifiant d'un nouveau HomeSmart appairé.
pub fn new_client_id() -> String {
    format!("hs-{}", hex::encode(rand::random::<[u8; 4]>()))
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_keeps_secrets_private() {
        let dir = std::env::temp_dir().join(format!("hscam-cfg-{}", rand::random::<u32>()));
        let path = dir.join("config.json");
        let caps = proto::Caps { max_width: 1920, max_height: 1080, max_fps: 30, codecs: vec![proto::VideoCodec::H264], audio: false, sensor: None };
        let cfg = AgentConfig {
            id: "aabbccddeeff".into(),
            setup_code: proto::new_code(),
            clients: vec![Client { id: new_client_id(), name: "PC".into(), secret: hex::encode([1u8; 32]), paired_at: unix_now() }],
            backend: Backend::Test,
            settings: caps.default_settings("Essai"),
            video_device: None,
            audio_device: None,
            has_mic: None,
            caps: None,
            api_port: default_api_port(),
            rtsp_port: default_rtsp_port(),
            tools: Tools::default(),
            custom: Vec::new(),
        };
        cfg.save(&path).unwrap();
        let back = AgentConfig::load(&path).unwrap().unwrap();
        assert_eq!(back.clients, cfg.clients);
        assert_eq!(back.settings, cfg.settings);
        assert_eq!(back.client(&cfg.clients[0].id).unwrap().secret_bytes(), Some([1u8; 32]));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(AgentConfig::load(&dir.join("absent.json")).unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}
