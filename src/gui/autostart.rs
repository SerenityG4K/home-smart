//! Lancement automatique à l'ouverture de session (programmes et garde-fous actifs en continu).
//!
//! - Windows : valeur « HomeSmart » sous `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` ;
//! - Linux : fichier `~/.config/autostart/homesmart.desktop`.
//! Dans les deux cas, l'application démarre réduite (`--minimized`).

use anyhow::{Context, Result};

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(windows)]
fn reg(args: &[&str]) -> std::io::Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("reg").args(args).creation_flags(CREATE_NO_WINDOW).output()
}

#[cfg(windows)]
pub fn is_enabled() -> bool {
    reg(&["query", RUN_KEY, "/v", "HomeSmart"]).is_ok_and(|o| o.status.success())
}

#[cfg(windows)]
pub fn set(enabled: bool) -> Result<()> {
    let out = if enabled {
        let exe = std::env::current_exe().context("chemin de l'exécutable")?;
        let cmd = format!("\"{}\" --minimized", exe.display());
        reg(&["add", RUN_KEY, "/v", "HomeSmart", "/t", "REG_SZ", "/d", &cmd, "/f"])?
    } else {
        reg(&["delete", RUN_KEY, "/v", "HomeSmart", "/f"])?
    };
    if !out.status.success() && enabled {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(not(windows))]
fn desktop_file() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(base.join("autostart").join("homesmart.desktop"))
}

#[cfg(not(windows))]
pub fn is_enabled() -> bool {
    desktop_file().is_some_and(|p| p.exists())
}

#[cfg(not(windows))]
pub fn set(enabled: bool) -> Result<()> {
    let path = desktop_file().context("dossier de configuration introuvable")?;
    if enabled {
        let exe = std::env::current_exe().context("chemin de l'exécutable")?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(
            &path,
            format!(
                "[Desktop Entry]\nType=Application\nName=HomeSmart\nComment=Pilotage local des appareils Tuya\nExec=\"{}\" --minimized\nX-GNOME-Autostart-enabled=true\n",
                exe.display()
            ),
        )?;
    } else if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}
