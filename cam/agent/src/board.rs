//! Matériel de la caméra : carte, capteur, micro, outils, et mesures du système.

use crate::config::{AgentConfig, Backend};
use homesmart_cam_proto::{Caps, VideoCodec};
use std::path::{Path, PathBuf};

/// Nom de la carte (« Radxa ROCK 4D »), lu dans l'arbre des périphériques.
pub fn board_name() -> String {
    read_trim("/proc/device-tree/model").unwrap_or_else(|| if cfg!(windows) { "PC Windows (essai)".into() } else { "Linux".into() })
}

fn read_trim(path: impl AsRef<Path>) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let s = s.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    (!s.is_empty()).then(|| s.to_string())
}

/// Carte à puce Rockchip (compatible « rockchip,… » dans l'arbre des périphériques).
pub fn is_rockchip() -> bool {
    std::fs::read("/proc/device-tree/compatible").is_ok_and(|b| String::from_utf8_lossy(&b).contains("rockchip"))
}

/// Backend à utiliser sur cette machine, à la création de la configuration.
pub fn detect_backend() -> Backend {
    if is_rockchip() {
        Backend::Rockchip
    } else if Path::new("/dev/video0").exists() {
        Backend::V4l2
    } else {
        Backend::Test
    }
}

/// Capteur d'image déclaré par le noyau (sous-périphérique V4L2 « m00_b_imx415 … »).
pub fn sensor_name() -> Option<String> {
    let dir = std::fs::read_dir("/sys/class/video4linux").ok()?;
    for e in dir.flatten() {
        let name = read_trim(e.path().join("name")).unwrap_or_default().to_ascii_lowercase();
        for (key, label) in [("imx415", "Sony IMX415"), ("imx678", "Sony IMX678"), ("imx577", "Sony IMX577"), ("imx708", "Sony IMX708"), ("imx219", "Sony IMX219"), ("ov13850", "OmniVision OV13850"), ("ov5647", "OmniVision OV5647")] {
            if name.contains(key) {
                return Some(label.into());
            }
        }
    }
    None
}

/// Une carte son sait-elle enregistrer ? (`/proc/asound/pcm` : « … : capture 1 »)
pub fn mic_present() -> bool {
    std::fs::read_to_string("/proc/asound/pcm").is_ok_and(|s| s.lines().any(|l| l.contains("capture")))
}

/// Ce que sait le matériel : la configuration l'emporte, sinon d'après le backend.
pub fn caps(cfg: &AgentConfig, ffmpeg_has_x265: bool) -> Caps {
    if let Some(c) = &cfg.caps {
        return c.clone();
    }
    let audio = cfg.has_mic.unwrap_or_else(|| match cfg.backend {
        Backend::Test => true,
        _ => cfg.audio_device.is_some() || mic_present(),
    });
    let x265 = if ffmpeg_has_x265 { vec![VideoCodec::H264, VideoCodec::H265] } else { vec![VideoCodec::H264] };
    match cfg.backend {
        // RK3576 : H.264 / H.265 jusqu'en 4K à 60 i/s ; l'IMX415 plafonne à 30 i/s en 4K.
        Backend::Rockchip => Caps { max_width: 3840, max_height: 2160, max_fps: 30, codecs: vec![VideoCodec::H264, VideoCodec::H265], audio, sensor: sensor_name() },
        // Webcam USB courante, encodée par le processeur.
        Backend::V4l2 => Caps { max_width: 1920, max_height: 1080, max_fps: 30, codecs: x265, audio, sensor: None },
        Backend::Test => Caps { max_width: 3840, max_height: 2160, max_fps: 30, codecs: x265, audio, sensor: Some("mire de test".into()) },
        Backend::Custom => Caps { max_width: 3840, max_height: 2160, max_fps: 30, codecs: vec![VideoCodec::H264, VideoCodec::H265], audio, sensor: sensor_name() },
    }
}

/// Identifiant stable : adresse MAC de la première interface réseau physique, sinon au hasard.
pub fn machine_id() -> String {
    let mut names: Vec<String> = std::fs::read_dir("/sys/class/net").map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    names.sort();
    for n in names.iter().filter(|n| n.as_str() != "lo" && !n.starts_with("docker") && !n.starts_with("veth")) {
        if let Some(mac) = read_trim(format!("/sys/class/net/{n}/address")) {
            let clean: String = mac.chars().filter(|c| c.is_ascii_hexdigit()).collect::<String>().to_ascii_lowercase();
            if clean.len() == 12 && clean != "000000000000" {
                return clean;
            }
        }
    }
    format!("x{}", hex::encode(rand::random::<[u8; 6]>()))
}

/// Nom de la machine sur le réseau (pour l'annonce mDNS).
pub fn hostname() -> String {
    let raw = read_trim("/etc/hostname").or_else(|| std::env::var("COMPUTERNAME").ok()).or_else(|| std::env::var("HOSTNAME").ok()).unwrap_or_else(|| "homesmart-cam".into());
    let clean: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c.to_ascii_lowercase() } else { '-' }).collect();
    if clean.trim_matches('-').is_empty() {
        "homesmart-cam".into()
    } else {
        clean
    }
}

/// Temps depuis le démarrage du système (sinon depuis celui de l'agent).
pub fn uptime_s(agent_started: std::time::Instant) -> u64 {
    read_trim("/proc/uptime").and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok()).map(|s| s as u64).unwrap_or_else(|| agent_started.elapsed().as_secs())
}

pub fn load_1min() -> Option<f32> {
    read_trim("/proc/loadavg")?.split_whitespace().next()?.parse().ok()
}

/// Température la plus haute des zones thermiques (°C).
pub fn cpu_temp_c() -> Option<f32> {
    let dir = std::fs::read_dir("/sys/class/thermal").ok()?;
    dir.flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("thermal_zone"))
        .filter_map(|e| read_trim(e.path().join("temp"))?.parse::<f32>().ok())
        .map(|milli| milli / 1000.0)
        .fold(None, |m: Option<f32>, t| Some(m.map_or(t, |m| m.max(t))))
}

/// Cherche un outil : chemin configuré, à côté de l'agent, dans /usr/local/bin, puis le PATH.
pub fn find_tool(configured: Option<&PathBuf>, name: &str) -> Option<PathBuf> {
    if let Some(p) = configured {
        return Some(p.clone());
    }
    let exe = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        dirs.push(d);
    }
    dirs.push(PathBuf::from("/usr/local/bin"));
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    // FFmpeg installé par HomeSmart (winget) sous Windows.
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local).join("Microsoft").join("WinGet").join("Links"));
    }
    dirs.into_iter().map(|d| d.join(&exe)).find(|p| p.is_file())
}

/// FFmpeg sait-il encoder en H.265 (libx265) ?
pub fn ffmpeg_has_x265(ffmpeg: &Path) -> bool {
    std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("libx265"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_is_dns_safe() {
        let h = hostname();
        assert!(!h.is_empty() && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'), "{h}");
        let id = machine_id();
        assert!(id.len() >= 12, "{id}");
    }
}
