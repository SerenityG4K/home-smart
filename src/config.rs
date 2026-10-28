//! Fichier de configuration local (JSON) : la liste des appareils, des caméras, des réglages.
//!
//! Les **secrets** (clés locales, mots de passe, jetons) n'y sont jamais écrits : ils sont rangés
//! à part, chiffrés (`crate::protect`), dans `homesmart.secrets.json` à côté de lui. En mémoire,
//! `Config` les porte comme avant ; seule l'écriture les sépare, et la lecture les réunit.

use crate::automation::{Safety, Schedule};
use crate::camera::CameraConfig;
use crate::house::HousePlan;
use crate::journal::{self, Entry, Level, Source};
use crate::protect::{self, Stored};
use crate::tplink::camera::TapoSecret;
use crate::vacuum::Link;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
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
    /// Clé locale (16 caractères). Rangée dans les secrets, pas dans `homesmart.json`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
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

/// Nom des secrets pour `protect` (clé du trousseau, entropie DPAPI).
pub const SECRETS_AS: &str = "secrets";

/// Fichier des secrets d'une configuration : `homesmart.json` → `homesmart.secrets.json`.
pub fn secrets_path(config: &Path) -> PathBuf {
    config.with_extension("secrets.json")
}

/// Clés et mots de passe de la configuration, rangés par identifiant (appareil, caméra,
/// aspirateur) dans le fichier des secrets.
#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
pub struct Secrets {
    /// Clé locale de chaque appareil Tuya.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub devices: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cameras: BTreeMap<String, CameraSecrets>,
    /// Mot de passe (Valetudo) ou jeton (miIO) de chaque aspirateur hors Tuya.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vacuums: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
pub struct CameraSecrets {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub password: String,
    /// Caméra HomeSmart : secret né de l'appairage.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    /// Caméra Tapo : empreintes du mot de passe du compte TP-Link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tapo: Option<TapoSecret>,
}

impl CameraSecrets {
    fn is_empty(&self) -> bool {
        self.password.is_empty() && self.agent.is_empty() && self.tapo.is_none()
    }
}

impl Secrets {
    /// Nombre de secrets (une caméra peut en avoir plusieurs).
    pub fn count(&self) -> usize {
        let cams: usize = self.cameras.values().map(|c| usize::from(!c.password.is_empty()) + usize::from(!c.agent.is_empty()) + usize::from(c.tapo.is_some())).sum();
        self.devices.len() + cams + self.vacuums.len()
    }
}

/// Identifiant sous lequel une caméra range ses secrets (l'adresse tant qu'elle n'en a pas).
fn camera_key(c: &CameraConfig) -> String {
    if c.id.is_empty() {
        c.host.clone()
    } else {
        c.id.clone()
    }
}

impl Config {
    /// Lit la configuration et ses secrets. Des secrets encore écrits dans `homesmart.json` (ancien
    /// format, ou ajoutés à la main) y sont laissés en mémoire, puis déplacés aussitôt dans le
    /// fichier des secrets ; le journal le dit. Un fichier des secrets illisible (abîmé, chiffré par
    /// un autre compte ou sur un autre PC) est une erreur : jamais une configuration sans ses clés
    /// qui l'écraserait.
    pub fn load(path: &Path) -> Result<Self> {
        let mut cfg: Config = match std::fs::read_to_string(path) {
            Ok(s) if s.trim().is_empty() => Config::default(),
            Ok(s) => serde_json::from_str(&s).with_context(|| format!("lecture de {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e).with_context(|| format!("ouverture de {}", path.display())),
        };
        let in_json = cfg.clone().take_secrets().count();
        let vault = secrets_path(path);
        let help = || {
            format!(
                "lecture de {} (clés et mots de passe des appareils) ; s'il vient d'un autre compte ou d'un autre PC, renomme-le pour repartir sans lui : il faudra ressaisir ces secrets",
                vault.display()
            )
        };
        if let Some((plain, _)) = protect::read_file(&vault, SECRETS_AS).with_context(help)? {
            let secrets: Secrets = serde_json::from_slice(&plain).context("secrets illisibles").with_context(help)?;
            cfg.put_secrets(secrets);
        }
        if in_json > 0 {
            cfg.move_secrets_out(path, in_json);
        }
        Ok(cfg)
    }

    /// Range les secrets trouvés dans `homesmart.json` dans le fichier des secrets, et le note au
    /// journal. En cas d'échec, ils restent où ils étaient : rien n'est perdu.
    fn move_secrets_out(&self, path: &Path, n: usize) {
        let (level, msg) = match self.save_with_secrets(path) {
            Ok(Stored::Sealed(p)) => (Level::Info, format!("{n} clé(s) et mot(s) de passe sortis de homesmart.json : rangés à part, chiffrés ({})", p.describe())),
            Ok(_) => (
                Level::Warning,
                format!("{n} clé(s) et mot(s) de passe sortis de homesmart.json : rangés à part mais en clair, aucun trousseau (Secret Service) pour les chiffrer ; seul ton compte peut les lire"),
            ),
            Err(e) => (Level::Warning, format!("Clés et mots de passe laissés dans homesmart.json : rangement à part impossible ({e:#})")),
        };
        journal::append_to_file(&path.with_file_name("journal.jsonl"), &Entry::new(level, Source::System, "", "", msg));
    }

    /// Écriture atomique (fichier temporaire puis renommage) : les secrets dans leur fichier, chiffré
    /// si le système le permet, le reste dans `path`.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.save_with_secrets(path).map(drop)
    }

    /// Comme `save` ; renvoie comment les secrets sont rangés. Les secrets d'abord : si l'écriture
    /// s'arrête entre les deux, `path` n'annonce jamais un appareil dont la clé n'est nulle part.
    pub fn save_with_secrets(&self, path: &Path) -> Result<Stored> {
        let mut public = self.clone();
        let secrets = public.take_secrets();
        let vault = secrets_path(path);
        let stored = if secrets.count() == 0 && !vault.exists() {
            Stored::Nothing
        } else {
            protect::write_file(&vault, SECRETS_AS, &serde_json::to_vec_pretty(&secrets)?)?
        };
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&public)?)?;
        std::fs::rename(&tmp, path).with_context(|| format!("écriture de {}", path.display()))?;
        Ok(stored)
    }

    /// Retire les secrets de la configuration et les rend.
    pub fn take_secrets(&mut self) -> Secrets {
        let mut s = Secrets::default();
        for d in &mut self.devices {
            let key = std::mem::take(&mut d.key);
            if !key.is_empty() {
                s.devices.insert(d.id.clone(), key);
            }
        }
        for c in &mut self.cameras {
            let cs = CameraSecrets {
                password: std::mem::take(&mut c.password),
                agent: c.agent.as_mut().map(|a| std::mem::take(&mut a.secret)).unwrap_or_default(),
                tapo: c.tapo.take(),
            };
            if !cs.is_empty() {
                s.cameras.insert(camera_key(c), cs);
            }
        }
        for v in &mut self.vacuums {
            let secret = match &mut v.link {
                Link::Valetudo { password, .. } => password.take().unwrap_or_default(),
                Link::Miio { token, .. } => std::mem::take(token),
            };
            if !secret.is_empty() {
                s.vacuums.insert(v.id.clone(), secret);
            }
        }
        s
    }

    /// Rend à la configuration les secrets qui lui manquent (ceux déjà présents l'emportent : écrits
    /// à la main dans `homesmart.json`, ils sont les plus récents).
    pub fn put_secrets(&mut self, mut s: Secrets) {
        for d in &mut self.devices {
            if let Some(key) = s.devices.remove(&d.id).filter(|_| d.key.is_empty()) {
                d.key = key;
            }
        }
        for c in &mut self.cameras {
            let Some(cs) = s.cameras.remove(&camera_key(c)) else { continue };
            if c.password.is_empty() {
                c.password = cs.password;
            }
            if let Some(a) = c.agent.as_mut().filter(|a| a.secret.is_empty()) {
                a.secret = cs.agent;
            }
            if c.tapo.is_none() {
                c.tapo = cs.tapo;
            }
        }
        for v in &mut self.vacuums {
            let Some(secret) = s.vacuums.remove(&v.id) else { continue };
            match &mut v.link {
                Link::Valetudo { password, .. } if password.is_none() => *password = Some(secret),
                Link::Miio { token, .. } if token.is_empty() => *token = secret,
                _ => {}
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::AgentLink;
    use crate::vacuum::VacuumConfig;

    const DEVICE_KEY: &str = "k3y-l0cale-T3st!";
    const CAM_PASSWORD: &str = "c0de-SC-t3st";
    const AGENT_SECRET: &str = "a9e7c0ffee00112233445566778899aabbccddeeff00112233445566778899aa";
    const VALETUDO_PASSWORD: &str = "v4letudo-t3st";
    const MIIO_TOKEN: &str = "0f0e0d0c0b0a09080706050403020100";

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("homesmart-secrets-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("homesmart.json")
    }

    fn sample() -> Config {
        let mut cfg = Config::default();
        cfg.devices.push(DeviceConfig { id: "bf00000000000000test".into(), key: DEVICE_KEY.into(), name: "Prise".into(), ..Default::default() });
        let mut cam = CameraConfig::new("10.0.0.5");
        cam.id = "cam-test".into();
        cam.password = CAM_PASSWORD.into();
        cam.agent = Some(AgentLink { api_port: 8780, client_id: "hub-test".into(), secret: AGENT_SECRET.into() });
        cam.tapo = Some(TapoSecret::from_password("Sim-pass1!"));
        cfg.cameras.push(cam);
        let valetudo = Link::Valetudo { host: "10.0.0.2".into(), port: None, user: Some("admin".into()), password: Some(VALETUDO_PASSWORD.into()) };
        cfg.vacuums.push(VacuumConfig { id: "vac-a".into(), name: "Robot".into(), room: None, link: valetudo, model: None });
        let miio = Link::Miio { host: "10.0.0.3".into(), token: MIIO_TOKEN.into(), port: None };
        cfg.vacuums.push(VacuumConfig { id: "vac-b".into(), name: "Robot 2".into(), room: None, link: miio, model: None });
        cfg
    }

    fn secret_values() -> Vec<String> {
        let tapo = TapoSecret::from_password("Sim-pass1!");
        [DEVICE_KEY, CAM_PASSWORD, AGENT_SECRET, VALETUDO_PASSWORD, MIIO_TOKEN, &tapo.md5, &tapo.sha256].map(String::from).to_vec()
    }

    fn assert_all_secrets(cfg: &Config) {
        let mut copy = cfg.clone();
        let s = copy.take_secrets();
        assert_eq!(s.count(), 6, "{s:?}");
        assert_eq!(s, sample().take_secrets());
    }

    #[test]
    fn secrets_are_kept_apart_and_come_back() {
        let path = temp("apart");
        let stored = sample().save_with_secrets(&path).unwrap();
        let json = std::fs::read_to_string(&path).unwrap();
        for v in secret_values() {
            assert!(!json.contains(&v), "secret dans homesmart.json : {json}");
        }
        // Le reste de la configuration est toujours là, lisible.
        assert!(json.contains("bf00000000000000test") && json.contains("hub-test") && json.contains("vac-b"), "{json}");
        let vault = std::fs::read_to_string(secrets_path(&path)).unwrap();
        match stored {
            Stored::Sealed(p) => {
                assert!(vault.contains(&format!("\"protection\":\"{}\"", p.id())));
                for v in secret_values() {
                    assert!(!vault.contains(&v), "secret lisible dans le fichier chiffré");
                }
            }
            _ => assert!(!cfg!(windows), "DPAPI existe toujours sous Windows"),
        }
        assert_all_secrets(&Config::load(&path).unwrap());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn secrets_in_old_json_are_moved_out_and_logged() {
        let path = temp("migrate");
        // Ancien format : tout dans homesmart.json.
        std::fs::write(&path, serde_json::to_string_pretty(&sample()).unwrap()).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains(DEVICE_KEY));
        let cfg = Config::load(&path).unwrap();
        assert_all_secrets(&cfg);
        let json = std::fs::read_to_string(&path).unwrap();
        for v in secret_values() {
            assert!(!json.contains(&v), "secret resté dans homesmart.json : {json}");
        }
        assert!(secrets_path(&path).exists());
        let log = std::fs::read_to_string(path.with_file_name("journal.jsonl")).unwrap();
        assert!(log.contains("6 clé(s) et mot(s) de passe sortis de homesmart.json"), "{log}");
        assert!(log.contains("\"System\""), "source Système attendue : {log}");
        // Relu : plus rien à déplacer, le journal n'en dit pas plus.
        assert_all_secrets(&Config::load(&path).unwrap());
        assert_eq!(std::fs::read_to_string(path.with_file_name("journal.jsonl")).unwrap(), log);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn key_written_by_hand_wins_and_removed_device_leaves_nothing() {
        let path = temp("hand");
        sample().save(&path).unwrap();
        // Clé corrigée à la main dans homesmart.json.
        let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        v["devices"][0]["key"] = Value::String("n0uvelle-cle-123".into());
        std::fs::write(&path, v.to_string()).unwrap();
        let mut cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.devices[0].key, "n0uvelle-cle-123");
        assert!(!std::fs::read_to_string(&path).unwrap().contains("n0uvelle-cle-123"));
        // Appareil supprimé : sa clé quitte aussi le fichier des secrets.
        cfg.devices.clear();
        cfg.save(&path).unwrap();
        let (plain, _) = protect::read_file(&secrets_path(&path), SECRETS_AS).unwrap().unwrap();
        let s: Secrets = serde_json::from_slice(&plain).unwrap();
        assert!(s.devices.is_empty() && s.cameras.len() == 1 && s.vacuums.len() == 2, "{s:?}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn unreadable_secrets_are_refused_and_left_alone() {
        let path = temp("damaged");
        sample().save(&path).unwrap();
        let json = std::fs::read_to_string(&path).unwrap();
        let vault = secrets_path(&path);
        for content in ["\u{0}pas du JSON", r#"{"protection":"dpapi","data":"%%%"}"#, r#"{"protection":"rot13","data":""}"#] {
            std::fs::write(&vault, content).unwrap();
            let e = format!("{:#}", Config::load(&path).unwrap_err());
            assert!(e.contains("renomme-le"), "{e}");
            assert_eq!(std::fs::read_to_string(&vault).unwrap(), content, "le fichier des secrets doit rester intact");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), json, "la configuration doit rester intacte");
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn no_secret_no_file() {
        let path = temp("none");
        let mut cfg = Config::default();
        cfg.prefs.price_kwh = 0.25;
        assert_eq!(cfg.save_with_secrets(&path).unwrap(), Stored::Nothing);
        assert!(!secrets_path(&path).exists());
        assert_eq!(Config::load(&path).unwrap().prefs.price_kwh, 0.25);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
