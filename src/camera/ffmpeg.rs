//! FFmpeg, utilisé seulement pour décoder les flux H.265 (le H.264 est décodé en interne).
//!
//! Recherche : à côté de l'exécutable, dans le PATH, puis (Windows) aux emplacements où
//! `winget` installe les paquets portables, ce qui permet de l'utiliser juste après
//! l'avoir installé, sans redémarrer HomeSmart. Installation en un clic sous Windows
//! via `winget` (paquet officiel `Gyan.FFmpeg`, sans droits administrateur) ; sous Linux,
//! la commande du gestionnaire de paquets est proposée.

use anyhow::{anyhow, bail, Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Paquet winget installé par le bouton « Installer FFmpeg ».
pub const WINGET_ID: &str = "Gyan.FFmpeg";

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }
}

/// Commande sans fenêtre console (Windows).
fn quiet(mut cmd: Command) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

/// Emplacements de winget : liens des paquets portables, puis dossiers des paquets Gyan.
#[cfg(windows)]
fn winget_candidates() -> Vec<PathBuf> {
    let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) else { return Vec::new() };
    let winget = local.join("Microsoft").join("WinGet");
    let mut out = vec![winget.join("Links").join(exe_name())];
    if let Ok(dirs) = std::fs::read_dir(winget.join("Packages")) {
        for pkg in dirs.flatten().filter(|d| d.file_name().to_string_lossy().starts_with(WINGET_ID)) {
            // …\Packages\Gyan.FFmpeg_…\ffmpeg-9.0.2-full_build\bin\ffmpeg.exe
            if let Ok(inner) = std::fs::read_dir(pkg.path()) {
                out.extend(inner.flatten().map(|d| d.path().join("bin").join(exe_name())));
            }
        }
    }
    out
}

#[cfg(not(windows))]
fn winget_candidates() -> Vec<PathBuf> {
    Vec::new()
}

/// Chemin de FFmpeg s'il est disponible.
pub fn find() -> Option<PathBuf> {
    let beside = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(exe_name())));
    let path = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).map(|d| d.join(exe_name())).collect::<Vec<_>>()).unwrap_or_default();
    beside.into_iter().chain(path).chain(winget_candidates()).find(|p| p.is_file())
}

/// Première ligne de `ffmpeg -version`, ex. « ffmpeg version 9.0.2-full_build-www.gyan.dev ».
pub fn version(path: &Path) -> Option<String> {
    let mut cmd = quiet(Command::new(path));
    let out = cmd.arg("-version").stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next()?.trim();
    let v = first.strip_prefix("ffmpeg version ")?.split_whitespace().next()?;
    Some(v.to_string())
}

/// FFmpeg peut-il être installé en un clic ici ? (Windows avec winget)
pub fn can_install() -> bool {
    cfg!(windows) && quiet(Command::new("winget")).arg("--version").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// Commande à proposer quand l'installation automatique n'est pas possible.
pub fn manual_command() -> String {
    if cfg!(windows) {
        return format!("winget install --id {WINGET_ID} --exact");
    }
    let has = |bin: &str| std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()));
    if has("apt") {
        "sudo apt install ffmpeg".into()
    } else if has("dnf") {
        "sudo dnf install ffmpeg".into()
    } else if has("pacman") {
        "sudo pacman -S ffmpeg".into()
    } else if has("zypper") {
        "sudo zypper install ffmpeg".into()
    } else {
        "installe le paquet « ffmpeg » de ta distribution".into()
    }
}

/// Résume une ligne de sortie de winget (barres de progression, messages en anglais ou en français).
pub fn progress_label(line: &str) -> Option<String> {
    let l = line.trim().trim_matches(|c: char| !c.is_alphanumeric() && c != '%' && c != '.');
    if l.is_empty() {
        return None;
    }
    // Pourcentage d'une barre de progression : « ██████▒▒▒  42% » ou « 35.2 MB / 112 MB ».
    if let Some(pct) = l.split_whitespace().find(|w| w.ends_with('%')).and_then(|w| w.trim_end_matches('%').parse::<f32>().ok()) {
        return Some(format!("Téléchargement… {pct:.0} %"));
    }
    if l.contains(" MB / ") || l.contains(" Mo / ") || l.contains(" KB / ") {
        let sizes: String = l.chars().skip_while(|c| !c.is_ascii_digit()).collect();
        return Some(format!("Téléchargement… {}", sizes.trim()));
    }
    let low = l.to_lowercase();
    let label = if low.contains("download") || low.contains("téléchargement") {
        "Téléchargement…"
    } else if low.contains("hash") {
        "Vérification du fichier téléchargé…"
    } else if low.contains("extract") || low.contains("extraction") || low.contains("starting package install") || low.contains("démarrage de l") {
        "Installation…"
    } else if low.contains("successfully installed") || low.contains("installé correctement") || low.contains("installation réussie") {
        "Installé"
    } else if low.contains("already installed") || low.contains("déjà installé") {
        "Déjà installé"
    } else if low.contains("found ") || low.contains("trouvé ") {
        "Paquet trouvé…"
    } else {
        return None;
    };
    Some(label.to_string())
}

/// Installe FFmpeg avec winget (bloquant, ~1 min) ; `progress` reçoit l'étape en cours.
/// Renvoie le chemin de FFmpeg une fois installé.
pub fn install(progress: impl Fn(String)) -> Result<PathBuf> {
    if !cfg!(windows) {
        bail!("installation automatique disponible seulement sous Windows : {}", manual_command());
    }
    progress("Préparation…".into());
    let mut cmd = quiet(Command::new("winget"));
    cmd.args([
        "install",
        "--id",
        WINGET_ID,
        "--exact",
        "--source",
        "winget",
        "--silent",
        "--accept-source-agreements",
        "--accept-package-agreements",
        "--disable-interactivity",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    let mut child = cmd.spawn().context("lancement de winget")?;
    let mut out = child.stdout.take().ok_or_else(|| anyhow!("winget : sortie indisponible"))?;
    let mut buf = [0u8; 4096];
    let mut line = Vec::new();
    let mut last_lines: Vec<String> = Vec::new();
    loop {
        let n = out.read(&mut buf).unwrap_or(0);
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            if b == b'\n' || b == b'\r' {
                let text = String::from_utf8_lossy(&line).to_string();
                if let Some(p) = progress_label(&text) {
                    progress(p);
                }
                if !text.trim().is_empty() {
                    last_lines.push(text.trim().to_string());
                    if last_lines.len() > 6 {
                        last_lines.remove(0);
                    }
                }
                line.clear();
            } else {
                line.push(b);
            }
        }
    }
    let status = child.wait().context("winget")?;
    // « Déjà installé » renvoie un code d'erreur : seul compte le fait de trouver FFmpeg ensuite.
    match find() {
        Some(p) => Ok(p),
        None if status.success() => bail!("winget a terminé, mais FFmpeg reste introuvable (redémarre HomeSmart)"),
        None => {
            let detail = last_lines.iter().rev().find(|l| l.chars().any(|c| c.is_alphabetic())).cloned().unwrap_or_default();
            bail!("échec de winget (code {}) {detail}", status.code().unwrap_or(-1))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winget_progress() {
        assert_eq!(progress_label("  ██████████▒▒▒▒▒▒▒▒▒▒  42%").as_deref(), Some("Téléchargement… 42 %"));
        assert_eq!(progress_label("Téléchargement de https://github.com/GyanD/codexffmpeg/releases/…").as_deref(), Some("Téléchargement…"));
        assert_eq!(progress_label("Successfully verified installer hash").as_deref(), Some("Vérification du fichier téléchargé…"));
        assert_eq!(progress_label("Extracting archive...").as_deref(), Some("Installation…"));
        assert_eq!(progress_label("Installé correctement").as_deref(), Some("Installé"));
        assert_eq!(progress_label("   -   ").as_deref(), None);
        assert!(!manual_command().is_empty());
    }
}
