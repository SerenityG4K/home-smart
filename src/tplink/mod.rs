//! Appareils TP-Link en local : Tapo (prises, ampoules, caméras) et Kasa.
//!
//! - découverte (`discover`) : UDP 20002 pour les appareils récents, UDP 9999 pour les anciennes Kasa ;
//! - prises et ampoules Tapo, Kasa récentes : protocole KLAP (`klap`), chiffré, avec l'e-mail et
//!   le mot de passe du compte TP-Link. L'appareil ne contacte pas le cloud pour autant : il garde
//!   une empreinte de ces identifiants depuis son installation dans l'application ;
//! - anciennes prises Kasa : protocole TCP 9999 à XOR (`kasa`), sans identifiants ;
//! - caméras Tapo : la vidéo et l'orientation passent par RTSP et ONVIF (`crate::camera`, marque
//!   Tapo), avec le « compte de la caméra » créé dans l'application ; leurs autres réglages
//!   (alarme, suivi des mouvements, carte SD) par leur API locale chiffrée (`camera`), avec le
//!   mot de passe du compte TP-Link, vérifié par la caméra elle-même sans Internet.

pub mod camera;
pub mod camera_sim;
pub mod discover;
pub mod kasa;
pub mod klap;
pub mod sim;

use anyhow::{anyhow, bail, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Value};

/// Identifiants du compte TP-Link (pour KLAP).
#[derive(Clone, Default)]
pub struct Credentials {
    pub email: String,
    pub password: String,
}

/// État d'un appareil après une lecture.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Info {
    pub model: String,
    /// Nom donné dans l'application.
    pub name: String,
    pub mac: String,
    /// Allumé ou éteint (`None` : pas d'interrupteur).
    pub on: Option<bool>,
    /// Puissance instantanée (W), prises à mesure (P110, P115, HS110…).
    pub power_w: Option<f64>,
    /// Énergie du jour (Wh), si l'appareil la donne.
    pub today_wh: Option<f64>,
    /// Luminosité (%), ampoules.
    pub brightness: Option<u8>,
    pub firmware: String,
    /// Qualité du Wi-Fi (dBm).
    pub rssi: Option<i32>,
}

enum Link {
    Klap(klap::Klap),
    Kasa { host: String, port: u16 },
}

/// Un appareil Tapo ou Kasa joint en local.
pub struct Device {
    link: Link,
}

impl Device {
    /// KLAP sur le port HTTP (Tapo, Kasa récentes), sinon l'ancien protocole Kasa (port 9999).
    pub fn connect(host: &str, creds: &Credentials) -> Result<Device> {
        Self::connect_at(host, 80, kasa::PORT, creds)
    }

    pub fn connect_at(host: &str, http_port: u16, kasa_port: u16, creds: &Credentials) -> Result<Device> {
        match klap::Klap::connect(host, http_port, &creds.email, &creds.password) {
            Ok(k) => Ok(Device { link: Link::Klap(k) }),
            Err(e) => {
                // Ancienne Kasa : pas de KLAP, mais une réponse en clair (ou presque) sur le port 9999.
                if kasa::request_at(host, kasa_port, &json!({ "system": { "get_sysinfo": {} } })).is_ok() {
                    return Ok(Device { link: Link::Kasa { host: host.to_string(), port: kasa_port } });
                }
                Err(e)
            }
        }
    }

    /// Protocole utilisé, pour l'affichage.
    pub fn protocol(&self) -> &'static str {
        match self.link {
            Link::Klap(_) => "KLAP",
            Link::Kasa { .. } => "Kasa (XOR)",
        }
    }

    pub fn info(&mut self) -> Result<Info> {
        match &mut self.link {
            Link::Klap(k) => {
                let r = k.request("get_device_info", None)?;
                let s = |key: &str| r.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
                // Les Tapo donnent leur nom en Base64.
                let name = B64.decode(s("nickname")).ok().and_then(|v| String::from_utf8(v).ok()).unwrap_or_else(|| s("nickname"));
                let mut info = Info {
                    model: s("model"),
                    name,
                    mac: s("mac").replace('-', ":").to_uppercase(),
                    on: r.get("device_on").and_then(Value::as_bool),
                    brightness: r.get("brightness").and_then(Value::as_u64).map(|b| b.min(100) as u8),
                    firmware: s("fw_ver"),
                    rssi: r.get("rssi").and_then(Value::as_i64).map(|v| v as i32),
                    ..Info::default()
                };
                // Prises à mesure : puissance (en mW) et énergie du jour ; les autres refusent.
                if let Ok(e) = k.request("get_energy_usage", None) {
                    info.power_w = e.get("current_power").and_then(Value::as_f64).map(|mw| mw / 1000.0);
                    info.today_wh = e.get("today_energy").and_then(Value::as_f64);
                }
                Ok(info)
            }
            Link::Kasa { host, port } => {
                let r = kasa::request_at(host, *port, &json!({ "system": { "get_sysinfo": {} } }))?;
                let si = r.pointer("/system/get_sysinfo").ok_or_else(|| anyhow!("réponse sans get_sysinfo"))?;
                let s = |key: &str| si.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
                let on = si.get("relay_state").and_then(Value::as_u64).or_else(|| si.pointer("/light_state/on_off").and_then(Value::as_u64)).map(|v| v == 1);
                let mut info = Info {
                    model: s("model").split('(').next().unwrap_or_default().trim().to_string(),
                    name: s("alias"),
                    mac: if s("mac").is_empty() { s("mic_mac") } else { s("mac") }.replace('-', ":").to_uppercase(),
                    on,
                    firmware: s("sw_ver"),
                    rssi: si.get("rssi").and_then(Value::as_i64).map(|v| v as i32),
                    ..Info::default()
                };
                if s("feature").contains("ENE") {
                    if let Ok(e) = kasa::request_at(host, *port, &json!({ "emeter": { "get_realtime": {} } })) {
                        let rt = e.pointer("/emeter/get_realtime");
                        // Selon le firmware : en mW (`power_mw`) ou en W (`power`).
                        info.power_w = rt.and_then(|v| v.get("power_mw").and_then(Value::as_f64).map(|mw| mw / 1000.0).or_else(|| v.get("power").and_then(Value::as_f64)));
                    }
                }
                Ok(info)
            }
        }
    }

    pub fn set_on(&mut self, on: bool) -> Result<()> {
        match &mut self.link {
            Link::Klap(k) => k.request("set_device_info", Some(json!({ "device_on": on }))).map(|_| ()),
            Link::Kasa { host, port } => {
                let r = kasa::request_at(host, *port, &json!({ "system": { "set_relay_state": { "state": on as u8 } } }))?;
                match r.pointer("/system/set_relay_state/err_code").and_then(Value::as_i64) {
                    Some(0) => Ok(()),
                    c => bail!("la prise refuse (code {})", c.map(|c| c.to_string()).unwrap_or_else(|| "inconnu".into())),
                }
            }
        }
    }

    /// Luminosité (1 à 100 %), ampoules Tapo ; l'ampoule s'allume.
    pub fn set_brightness(&mut self, pct: u8) -> Result<()> {
        match &mut self.link {
            Link::Klap(k) => k.request("set_device_info", Some(json!({ "brightness": pct.clamp(1, 100), "device_on": true }))).map(|_| ()),
            Link::Kasa { .. } => bail!("luminosité non gérée sur les anciennes Kasa"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(email: &str, password: &str) -> Credentials {
        Credentials { email: email.into(), password: password.into() }
    }

    /// Prise Tapo simulée : poignée de main KLAP, lecture, extinction, et identifiants refusés.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tapo_plug_over_klap() {
        let sim = sim::start_tapo("moi@exemple.fr", "secret", "Lampe du salon").await.unwrap();
        let port = sim.port;
        let info = tokio::task::spawn_blocking(move || -> Result<(Info, Info, &'static str)> {
            let mut d = Device::connect_at("127.0.0.1", port, 1, &creds("moi@exemple.fr", "secret"))?;
            let before = d.info()?;
            d.set_on(false)?;
            Ok((before, d.info()?, d.protocol()))
        })
        .await
        .unwrap()
        .unwrap();
        let (before, after, protocol) = info;
        assert_eq!(protocol, "KLAP");
        assert_eq!((before.model.as_str(), before.name.as_str(), before.mac.as_str()), ("P110", "Lampe du salon", "AA:BB:CC:00:01:10"));
        assert_eq!((before.on, before.power_w, before.today_wh), (Some(true), Some(42.5), Some(310.0)));
        assert_eq!((after.on, after.power_w), (Some(false), Some(0.0)));
        assert_eq!(sim.state.lock().unwrap().get("device_on"), Some(&json!(false)));
        // Mauvais mot de passe : refus clair, sans essayer de deviner.
        let err = tokio::task::spawn_blocking(move || Device::connect_at("127.0.0.1", port, 1, &creds("moi@exemple.fr", "faux")).err().map(|e| format!("{e:#}"))).await.unwrap().unwrap();
        assert!(err.contains("identifiants TP-Link refusés"), "{err}");
    }

    /// Ancienne Kasa : pas de KLAP, le protocole XOR prend le relais.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn legacy_kasa_plug() {
        let sim = sim::start_kasa("Radiateur").await.unwrap();
        let port = sim.port;
        let (before, after, protocol) = tokio::task::spawn_blocking(move || -> Result<(Info, Info, &'static str)> {
            // Rien n'écoute en HTTP sur le port 1 : KLAP échoue, la Kasa répond.
            let mut d = Device::connect_at("127.0.0.1", 1, port, &Credentials::default())?;
            let before = d.info()?;
            d.set_on(false)?;
            Ok((before, d.info()?, d.protocol()))
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(protocol, "Kasa (XOR)");
        assert_eq!((before.model.as_str(), before.name.as_str(), before.on, before.power_w), ("HS110", "Radiateur", Some(true), Some(18.2)));
        assert_eq!((after.on, after.power_w), (Some(false), Some(0.0)));
        assert_eq!(sim.sysinfo.lock().unwrap().get("relay_state"), Some(&json!(0)));
    }
}
