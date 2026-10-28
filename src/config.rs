//! Fichier de configuration local (JSON) : la liste des appareils et leurs clés.

use crate::automation::{Safety, Schedule};
use crate::camera::CameraConfig;
use crate::house::HousePlan;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// Emplacement par défaut : `HOMESMART_CONFIG`, sinon
/// `%APPDATA%\HomeSmart\homesmart.json` (Windows) ou `~/.config/homesmart/homesmart.json` (Linux).
pub fn default_path() -> PathBuf {
    if let Some(p) = std::env::var_os("HOMESMART_CONFIG").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    #[cfg(windows)]
    let dir = std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("HomeSmart"));
    #[cfg(not(windows))]
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|d| d.join("homesmart"));
    dir.unwrap_or_default().join("homesmart.json")
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Config {
    #[serde(default)]
    pub devices: Vec<DeviceConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedules: Vec<Schedule>,
    /// Scénarios : réactions des appareils à ce que voient et entendent les caméras.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scenarios: Vec<crate::automation::Scenario>,
    #[serde(default)]
    pub prefs: Prefs,
    /// Plan de la maison (vue 3D).
    #[serde(default, skip_serializing_if = "HousePlan::is_empty")]
    pub house: HousePlan,
    /// Caméras IP (RTSP / ONVIF).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cameras: Vec<CameraConfig>,
    /// Vision par IA (analyse des caméras, en local).
    #[serde(default, skip_serializing_if = "crate::vision::VisionSettings::is_empty")]
    pub vision: crate::vision::VisionSettings,
    /// Aspirateurs robots hors Tuya (Valetudo, miIO) ; les robots Tuya sont dans `devices`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vacuums: Vec<crate::vacuum::VacuumConfig>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Prefs {
    /// Prix du kWh (€ TTC), pour estimer le coût.
    #[serde(default = "default_price")]
    pub price_kwh: f64,
    /// Démarrer réduite (lancement automatique avec la session).
    #[serde(default)]
    pub start_minimized: bool,
    /// Intervalle des mesures de consommation, en secondes.
    #[serde(default = "default_meter")]
    pub meter_interval_s: f64,
    /// Notifications du système pour les alertes critiques (chute, appel à l'aide, alarme…).
    #[serde(default = "default_true")]
    pub os_notifications: bool,
    /// Son des caméras amélioré à l'écoute (filtre des basses, volume automatique, limiteur).
    #[serde(default = "default_true")]
    pub audio_enhance: bool,
    /// Thème clair de l'application (sombre sinon).
    #[serde(default)]
    pub light_theme: bool,
}

fn default_true() -> bool {
    true
}

fn default_meter() -> f64 {
    2.0
}

fn default_price() -> f64 {
    0.20
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { price_kwh: default_price(), start_minimized: false, meter_interval_s: default_meter(), os_notifications: true, audio_enhance: true, light_theme: false }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DeviceConfig {
    pub id: String,
    /// Clé locale (16 caractères).
    pub key: String,
    #[serde(default)]
    pub name: String,
    /// IP fixe ; sinon on prend celle annoncée sur le réseau.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// "3.1" … "3.5" ; sinon la version annoncée, ou 3.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    /// light | plug | switch | cover | vacuum | generic ; sinon détection automatique.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Description des DP (format `mapping` de tinytuya), facultative.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub schema: Map<String, Value>,
    /// Garde-fous (appareils à risque).
    #[serde(default, skip_serializing_if = "Safety::is_default")]
    pub safety: Safety,
    /// Cette prise alimente le PC où tourne HomeSmart : quand elle coupe, HomeSmart s'arrête avec
    /// lui ; au redémarrage, la détection des problèmes le dit et nomme la prise.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub powers_pc: bool,
}

impl DeviceConfig {
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) if s.trim().is_empty() => Ok(Config::default()),
            Ok(s) => serde_json::from_str(&s).with_context(|| format!("lecture de {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("ouverture de {}", path.display())),
        }
    }

    /// Écriture atomique (fichier temporaire puis renommage).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path).with_context(|| format!("écriture de {}", path.display()))?;
        Ok(())
    }

    /// Recherche par identifiant exact, puis par nom (insensible à la casse).
    pub fn find(&self, query: &str) -> Option<&DeviceConfig> {
        let q = query.trim();
        self.devices
            .iter()
            .find(|d| d.id == q)
            .or_else(|| self.devices.iter().find(|d| d.name.eq_ignore_ascii_case(q)))
            .or_else(|| {
                let ql = q.to_lowercase();
                let mut hits = self.devices.iter().filter(|d| d.name.to_lowercase().contains(&ql));
                match (hits.next(), hits.next()) {
                    (Some(d), None) => Some(d),
                    _ => None,
                }
            })
    }

    /// Caméra par identifiant, nom (insensible à la casse, ou début de nom unique) ou adresse.
    pub fn find_camera(&self, query: &str) -> Option<&CameraConfig> {
        let q = query.trim();
        let ql = q.to_lowercase();
        self.cameras
            .iter()
            .find(|c| c.id == q || c.host == q)
            .or_else(|| self.cameras.iter().find(|c| c.name.eq_ignore_ascii_case(q)))
            .or_else(|| {
                let mut hits = self.cameras.iter().filter(|c| c.name.to_lowercase().contains(&ql));
                match (hits.next(), hits.next()) {
                    (Some(c), None) => Some(c),
                    _ => None,
                }
            })
    }

    /// Ajoute l'appareil, ou met à jour celui de même identifiant.
    pub fn upsert(&mut self, dev: DeviceConfig) -> bool {
        match self.devices.iter_mut().find(|d| d.id == dev.id) {
            Some(existing) => {
                *existing = dev;
                false
            }
            None => {
                self.devices.push(dev);
                true
            }
        }
    }
}

/// Type d'appareil déduit de la catégorie Tuya.
pub fn kind_from_category(cat: &str) -> Option<&'static str> {
    Some(match cat {
        "dj" | "dd" | "fwd" | "dc" | "xdd" | "fsd" | "tgq" | "tgkg" | "gyd" | "tyndj" => "light",
        "cz" | "pc" => "plug",
        "kg" | "tdq" | "qjdcz" => "switch",
        "cl" | "clkg" | "ckmkzq" => "cover",
        "sd" => "vacuum",
        _ => return None,
    })
}

fn as_string(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Importe un `devices.json` (ou `snapshot.json`) produit par tinytuya.
pub fn import_tinytuya(path: &Path) -> Result<Vec<DeviceConfig>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("lecture de {}", path.display()))?;
    let root: Value = serde_json::from_str(&text)?;
    let list = match &root {
        Value::Array(a) => a.clone(),
        Value::Object(o) => o.get("devices").and_then(Value::as_array).cloned().unwrap_or_default(),
        _ => bail!("format non reconnu"),
    };
    let mut out = Vec::new();
    for d in list {
        let (Some(id), Some(key)) = (as_string(d.get("id")), as_string(d.get("key"))) else { continue };
        if key.len() != 16 {
            tracing::warn!("{id} ignoré : clé locale absente ou invalide");
            continue;
        }
        let mut schema = Map::new();
        if let Some(Value::Object(mapping)) = d.get("mapping") {
            for (dp, def) in mapping {
                let mut def = def.clone();
                // `values` est parfois une chaîne JSON.
                if let Some(Value::String(s)) = def.get("values") {
                    if let Ok(v) = serde_json::from_str::<Value>(s) {
                        def["values"] = v;
                    }
                }
                schema.insert(dp.clone(), def);
            }
        }
        out.push(DeviceConfig {
            id,
            key,
            name: as_string(d.get("name")).unwrap_or_default(),
            ip: as_string(d.get("ip")),
            port: None,
            version: as_string(d.get("version")).or_else(|| as_string(d.get("ver"))),
            room: None,
            kind: as_string(d.get("category")).and_then(|c| kind_from_category(&c)).map(str::to_string),
            schema,
            safety: Safety::default(),
            powers_pc: false,
        });
    }
    Ok(out)
}
