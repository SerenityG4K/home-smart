//! Wi-Fi du PC : rejoindre quelques instants le point d'accès d'une caméra en appairage
//! (`DAP-XXXXXXXX` chez Imou), puis revenir sur le Wi-Fi de la maison.
//!
//! Windows : `netsh wlan` avec un profil temporaire en connexion manuelle (il est supprimé
//! ensuite) ; Linux : `nmcli`. Rien n'est lu dans les profils Wi-Fi enregistrés : le mot de
//! passe du Wi-Fi de la maison, c'est l'utilisateur qui le saisit.

use anyhow::{anyhow, bail, Context, Result};
use std::time::{Duration, Instant};

/// Réseau Wi-Fi visible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub ssid: String,
    /// Qualité du signal, en %.
    pub signal: Option<u8>,
}

/// Point d'accès ouvert par une caméra Imou / Dahua en attente de configuration.
pub fn is_camera_ap(ssid: &str) -> bool {
    let s = ssid.to_ascii_uppercase();
    ["DAP-", "IMOU-", "IMOU_", "LC-", "DH-"].iter().any(|p| s.starts_with(p))
}

/// Code de la caméra annoncé dans le nom du point d'accès (`DAP-1A2B3C4D` → `1A2B3C4D`).
pub fn ap_suffix(ssid: &str) -> Option<&str> {
    ssid.split_once(['-', '_']).map(|(_, s)| s).filter(|s| !s.is_empty())
}

fn run(program: &str, args: &[&str]) -> Result<String> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // pas de fenêtre console
    }
    let out = cmd.output().with_context(|| format!("lancement de {program}"))?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = if err.trim().is_empty() { text.trim() } else { err.trim() };
        bail!("{program} : {}", msg.lines().next().unwrap_or("échec"));
    }
    Ok(text)
}

// ---- Analyse des sorties (fonctions pures, testées) ---------------------------------------

/// `clé : valeur` d'une ligne de netsh (les clés sont traduites, les espaces insécables tolérés).
fn kv(line: &str) -> Option<(&str, &str)> {
    let (k, v) = line.split_once(':')?;
    Some((k.trim(), v.trim()))
}

/// SSID du réseau connecté, d'après `netsh wlan show interfaces`.
pub fn parse_netsh_current(text: &str) -> Option<String> {
    text.lines().filter_map(kv).find(|(k, _)| *k == "SSID").map(|(_, v)| v.to_string()).filter(|s| !s.is_empty())
}

/// Profil Windows du réseau connecté (« Profil » en français, « Profile » en anglais).
pub fn parse_netsh_profile(text: &str) -> Option<String> {
    text.lines().filter_map(kv).find(|(k, _)| k.starts_with("Profil")).map(|(_, v)| v.to_string()).filter(|s| !s.is_empty())
}

/// Réseaux de `netsh wlan show networks mode=bssid` (le meilleur signal de chaque SSID).
pub fn parse_netsh_networks(text: &str) -> Vec<Network> {
    let mut out: Vec<Network> = Vec::new();
    for (k, v) in text.lines().filter_map(kv) {
        let is_ssid = k.strip_prefix("SSID").is_some_and(|n| n.trim().chars().all(|c| c.is_ascii_digit()) && !n.trim().is_empty());
        if is_ssid {
            out.push(Network { ssid: v.to_string(), signal: None });
        } else if k == "Signal" {
            let pct = v.trim_end_matches('%').trim().parse::<u8>().ok();
            if let Some(n) = out.last_mut() {
                n.signal = n.signal.max(pct);
            }
        }
    }
    out.retain(|n| !n.ssid.is_empty());
    out
}

/// Lignes `nmcli -t` : les `:` des valeurs y sont échappés en `\:`.
fn nmcli_fields(line: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    fields.last_mut().unwrap().push(n);
                }
            }
            ':' => fields.push(String::new()),
            c => fields.last_mut().unwrap().push(c),
        }
    }
    fields
}

/// `nmcli -t -f SSID,SIGNAL dev wifi list`.
pub fn parse_nmcli_networks(text: &str) -> Vec<Network> {
    let mut out: Vec<Network> = Vec::new();
    for line in text.lines() {
        let f = nmcli_fields(line);
        let (Some(ssid), signal) = (f.first().filter(|s| !s.is_empty()), f.get(1).and_then(|s| s.parse::<u8>().ok())) else { continue };
        match out.iter_mut().find(|n| &n.ssid == ssid) {
            Some(n) => n.signal = n.signal.max(signal),
            None => out.push(Network { ssid: ssid.clone(), signal }),
        }
    }
    out
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// Profil Windows d'un réseau, en connexion manuelle (Windows n'y reviendra pas tout seul).
pub fn profile_xml(ssid: &str, key: Option<&str>) -> String {
    let hex: String = ssid.bytes().map(|b| format!("{b:02X}")).collect();
    let security = match key {
        None => "<authEncryption><authentication>open</authentication><encryption>none</encryption><useOneX>false</useOneX></authEncryption>".to_string(),
        Some(k) => format!(
            "<authEncryption><authentication>WPA2PSK</authentication><encryption>AES</encryption><useOneX>false</useOneX></authEncryption><sharedKey><keyType>passPhrase</keyType><protected>false</protected><keyMaterial>{}</keyMaterial></sharedKey>",
            xml_escape(k)
        ),
    };
    format!(
        r#"<?xml version="1.0"?><WLANProfile xmlns="http://www.microsoft.com/networking/WLAN/profile/v1"><name>{name}</name><SSIDConfig><SSID><hex>{hex}</hex><name>{name}</name></SSID></SSIDConfig><connectionType>ESS</connectionType><connectionMode>manual</connectionMode><MSM><security>{security}</security></MSM></WLANProfile>"#,
        name = xml_escape(ssid)
    )
}

// ---- Windows --------------------------------------------------------------------------------

#[cfg(windows)]
mod wlanapi {
    //! Juste de quoi demander une nouvelle recherche des réseaux (netsh ne sait pas le faire).
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Guid {
        pub data: [u8; 16],
    }

    #[repr(C)]
    pub struct InterfaceInfo {
        pub guid: Guid,
        pub description: [u16; 256],
        pub state: u32,
    }

    #[repr(C)]
    pub struct InterfaceList {
        pub count: u32,
        pub index: u32,
        pub items: [InterfaceInfo; 1],
    }

    #[link(name = "wlanapi")]
    extern "system" {
        pub fn WlanOpenHandle(version: u32, reserved: *mut c_void, negotiated: *mut u32, handle: *mut *mut c_void) -> u32;
        pub fn WlanCloseHandle(handle: *mut c_void, reserved: *mut c_void) -> u32;
        pub fn WlanEnumInterfaces(handle: *mut c_void, reserved: *mut c_void, list: *mut *mut InterfaceList) -> u32;
        pub fn WlanScan(handle: *mut c_void, guid: *const Guid, ssid: *const c_void, ie: *const c_void, reserved: *mut c_void) -> u32;
        pub fn WlanFreeMemory(memory: *mut c_void);
    }

    /// Lance une recherche sur toutes les cartes Wi-Fi (les résultats arrivent en 2 à 4 s).
    pub fn rescan() {
        // SAFETY : appels documentés de wlanapi ; la liste renvoyée est lue dans ses bornes puis libérée.
        unsafe {
            let (mut negotiated, mut handle) = (0u32, std::ptr::null_mut());
            if WlanOpenHandle(2, std::ptr::null_mut(), &mut negotiated, &mut handle) != 0 {
                return;
            }
            let mut list: *mut InterfaceList = std::ptr::null_mut();
            if WlanEnumInterfaces(handle, std::ptr::null_mut(), &mut list) == 0 && !list.is_null() {
                let count = (*list).count as usize;
                let first = std::ptr::addr_of!((*list).items) as *const InterfaceInfo;
                for i in 0..count {
                    let guid = (*first.add(i)).guid;
                    WlanScan(handle, &guid, std::ptr::null(), std::ptr::null(), std::ptr::null_mut());
                }
                WlanFreeMemory(list as *mut c_void);
            }
            WlanCloseHandle(handle, std::ptr::null_mut());
        }
    }
}

/// SSID du Wi-Fi auquel le PC est connecté.
pub fn current_ssid() -> Option<String> {
    #[cfg(windows)]
    {
        run("netsh", &["wlan", "show", "interfaces"]).ok().as_deref().and_then(parse_netsh_current)
    }
    #[cfg(not(windows))]
    {
        let text = run("nmcli", &["-t", "-f", "ACTIVE,SSID", "dev", "wifi"]).ok()?;
        text.lines().map(nmcli_fields).find(|f| f.first().map(String::as_str) == Some("yes")).and_then(|f| f.get(1).cloned()).filter(|s| !s.is_empty())
    }
}

/// Nom sous lequel le système connaît le réseau connecté (pour y revenir), à défaut son SSID.
pub fn current_profile() -> Option<String> {
    #[cfg(windows)]
    {
        let text = run("netsh", &["wlan", "show", "interfaces"]).ok()?;
        parse_netsh_profile(&text).or_else(|| parse_netsh_current(&text))
    }
    #[cfg(not(windows))]
    {
        let text = run("nmcli", &["-t", "-f", "NAME,TYPE", "con", "show", "--active"]).ok()?;
        text.lines().map(nmcli_fields).find(|f| f.get(1).is_some_and(|t| t.contains("wireless"))).and_then(|f| f.first().cloned()).or_else(current_ssid)
    }
}

/// Réseaux visibles, après une nouvelle recherche (≈ 4 s).
pub fn networks() -> Result<Vec<Network>> {
    #[cfg(windows)]
    {
        wlanapi::rescan();
        std::thread::sleep(Duration::from_secs(4));
        let mut list = parse_netsh_networks(&run("netsh", &["wlan", "show", "networks", "mode=bssid"])?);
        list.sort_by_key(|n| std::cmp::Reverse(n.signal));
        Ok(list)
    }
    #[cfg(not(windows))]
    {
        let mut list = parse_nmcli_networks(&run("nmcli", &["-t", "-f", "SSID,SIGNAL", "dev", "wifi", "list", "--rescan", "yes"])?);
        list.sort_by_key(|n| std::cmp::Reverse(n.signal));
        Ok(list)
    }
}

/// Attend que le PC soit connecté à `ssid`.
fn wait_for(ssid: &str, wait: Duration) -> Result<()> {
    let t0 = Instant::now();
    while t0.elapsed() < wait {
        if current_ssid().as_deref() == Some(ssid) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(anyhow!("le PC ne s'est pas connecté à « {ssid} » en {} s", wait.as_secs()))
}

/// Connecte le PC à `ssid` (réseau ouvert si `key` est `None`).
pub fn join(ssid: &str, key: Option<&str>, wait: Duration) -> Result<()> {
    #[cfg(windows)]
    {
        // Profil temporaire : le fichier (qui peut contenir la clé) est effacé aussitôt importé.
        let path = std::env::temp_dir().join(format!("homesmart-wlan-{}.xml", std::process::id()));
        std::fs::write(&path, profile_xml(ssid, key)).context("profil Wi-Fi temporaire")?;
        let file = format!("filename={}", path.display());
        let added = run("netsh", &["wlan", "add", "profile", &file, "user=current"]);
        let _ = std::fs::remove_file(&path);
        added.context("ajout du profil Wi-Fi")?;
        run("netsh", &["wlan", "connect", &format!("name={ssid}"), &format!("ssid={ssid}")]).context("connexion Wi-Fi")?;
    }
    #[cfg(not(windows))]
    {
        let mut args = vec!["dev", "wifi", "connect", ssid];
        if let Some(k) = key {
            args.extend(["password", k]);
        }
        run("nmcli", &args).context("connexion Wi-Fi")?;
    }
    wait_for(ssid, wait)
}

/// Revient sur un réseau déjà connu du système (le Wi-Fi de la maison) : `profile` est le nom
/// du profil (`current_profile`), `ssid` le réseau attendu.
pub fn rejoin(profile: &str, ssid: &str, wait: Duration) -> Result<()> {
    if current_ssid().as_deref() == Some(ssid) {
        return Ok(());
    }
    #[cfg(windows)]
    run("netsh", &["wlan", "connect", &format!("name={profile}")]).context("retour sur le Wi-Fi")?;
    #[cfg(not(windows))]
    run("nmcli", &["con", "up", "id", profile]).context("retour sur le Wi-Fi")?;
    wait_for(ssid, wait)
}

/// Supprime le profil temporaire créé par `join`.
pub fn forget(ssid: &str) {
    #[cfg(windows)]
    let _ = run("netsh", &["wlan", "delete", "profile", &format!("name={ssid}")]);
    #[cfg(not(windows))]
    let _ = run("nmcli", &["con", "delete", "id", ssid]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_access_points() {
        assert!(is_camera_ap("DAP-1A2B3C4D"));
        assert!(is_camera_ap("Imou-K2EC"));
        assert!(!is_camera_ap("Freebox-A1B2C3"));
        assert_eq!(ap_suffix("DAP-1A2B3C4D"), Some("1A2B3C4D"));
    }

    #[test]
    fn netsh_output() {
        // Sortie française réelle (espaces insécables avant les deux-points).
        let iface = "\r\n    Nom                   : Wi-Fi\r\n    SSID                  : Freebox-A1B2C3\r\n    Point d'accès BSSID   : 3a:00:00:00:00:01\r\n    Profil               \u{a0}: Freebox-A1B2C3 \r\n";
        assert_eq!(parse_netsh_current(iface).as_deref(), Some("Freebox-A1B2C3"));
        assert_eq!(parse_netsh_profile(iface).as_deref(), Some("Freebox-A1B2C3"));
        assert_eq!(parse_netsh_profile("    Profile : Home\r\n").as_deref(), Some("Home"));
        let nets = "SSID 1\u{a0}: Freebox-A1B2C3\r\n     HESSID   \u{a0}: 38:07\r\n    BSSID 1   : 3a:07\r\n         Signal             : 83%  \r\n    BSSID 2   : 3a:08\r\n         Signal  : 91%\r\n\r\nSSID 2 : DAP-1A2B3C4D\r\n    Authentification : Ouvrir\r\n    BSSID 1 : a8:31\r\n         Signal : 99%\r\nSSID 3 : \r\n";
        let l = parse_netsh_networks(nets);
        assert_eq!(l, vec![Network { ssid: "Freebox-A1B2C3".into(), signal: Some(91) }, Network { ssid: "DAP-1A2B3C4D".into(), signal: Some(99) }]);
    }

    #[test]
    fn nmcli_output() {
        let l = parse_nmcli_networks("DAP-1A2B3C4D:99\nMa\\:Box:40\nMa\\:Box:62\n:30\n");
        assert_eq!(l, vec![Network { ssid: "DAP-1A2B3C4D".into(), signal: Some(99) }, Network { ssid: "Ma:Box".into(), signal: Some(62) }]);
    }

    #[test]
    fn windows_profile() {
        let x = profile_xml("DAP-1A", None);
        assert!(x.contains("<hex>4441502D3141</hex>") && x.contains("<connectionMode>manual</connectionMode>") && x.contains("<authentication>open</authentication>"));
        let x = profile_xml("Chez <moi>", Some("a&b"));
        assert!(x.contains("<name>Chez &lt;moi&gt;</name>") && x.contains("<keyMaterial>a&amp;b</keyMaterial>"));
    }
}
