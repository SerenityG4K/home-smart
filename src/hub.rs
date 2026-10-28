//! Registre des appareils et moteur d'automatisation.
//!
//! Chaque appareil a sa propre tâche (« acteur ») qui garde une connexion TCP
//! ouverte : les commandes partent immédiatement, les changements d'état
//! poussés par l'appareil sont relayés en direct à l'interface. L'acteur applique
//! aussi les garde-fous et enregistre la consommation ; une tâche à part exécute
//! les programmes horaires. La détection des problèmes (`problems`) suit chaque appareil et
//! chaque caméra, et inscrit au journal ce qui arrive sans avoir été demandé. Les aspirateurs
//! robots (`vacuums`) ont leur propre acteur quand ils ne sont pas Tuya.

use crate::automation::{self, GuardAction, GuardState, Observation, Safety, Schedule};
use crate::config::{Config, DeviceConfig, Prefs};
use crate::device::{self, Target, Writer};
use crate::discovery::{self, now_ms, Discovered};
use crate::energy::EnergyStore;
use crate::journal::{Entry, Journal};
pub use crate::journal::{Level, Source};
use crate::kinds::{self, Kind};
use crate::protocol::{codec, Version, PORT_TCP};
use anyhow::{anyhow, bail, Result};
use chrono::{Local, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::{sleep, sleep_until, Instant, MissedTickBehavior};

mod analysis;
mod listening;
mod problems;
mod scenarios;
mod cameras;
mod vacuums;
pub use analysis::{Analysis, KnownFaceView, ModelInstall, VisionStatus};
pub use listening::{calls_for_help, missing_mb as listen_missing_mb, Listening, HELP_WORDS};
pub use cameras::{CameraPatch, CameraView, FfmpegStatus, PlaybackView, TestReport};
pub use vacuums::{read_link as read_vacuum_link, run_link as run_vacuum_link, test_link as test_vacuum_link, VacuumPatch, VacuumView};
pub use crate::problems::{Problem, ProblemKind};
pub use crate::camera::onvif::{EncoderConfig, EncoderOptions};

const HEARTBEAT: Duration = Duration::from_secs(10);
/// Intervalle des mesures (courant, puissance, tension) par défaut.
pub const DEFAULT_METER_INTERVAL: Duration = Duration::from_secs(2);
const SILENCE_LIMIT: Duration = Duration::from_secs(35);
const CONFIRM_DELAY: Duration = Duration::from_millis(1500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Une valeur envoyée puis confirmée par l'appareil dans ce délai est « la nôtre ».
const OWN_ECHO: Duration = Duration::from_secs(10);
/// DP de mesure (courant, puissance, tension) des prises à suivi de consommation.
const METERING_DPS: [u32; 3] = [18, 19, 20];

/// Changement signalé à l'interface, qui relit ensuite l'état du hub.
/// (Volontairement léger : rien n'est sérialisé ni copié à chaque mesure.)
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Device(String),
    Removed(String),
    Discovered,
    Alert(u64),
    AlertsCleared,
    Schedules,
    /// Liste ou état des caméras (les images, elles, passent par `set_frame_waker`).
    Cameras,
}

pub enum Command {
    Set(Map<String, Value>, oneshot::Sender<Result<()>>),
    Refresh,
    Shutdown,
}

#[derive(Clone, Default, Serialize)]
pub struct DeviceState {
    pub online: bool,
    pub dps: Map<String, Value>,
    pub error: Option<String>,
    pub ip: Option<String>,
    pub version: Option<String>,
    pub updated: u64,
    /// Délai mesuré entre l'envoi d'une commande et la confirmation de l'appareil.
    pub latency_ms: Option<u64>,
    /// Dernière valeur de minuterie reçue, et quand.
    #[serde(skip)]
    pub countdown: Option<(u64, std::time::Instant)>,
}

pub struct DeviceHandle {
    pub cfg: RwLock<DeviceConfig>,
    pub state: RwLock<DeviceState>,
    guard: Mutex<GuardState>,
    /// Dernières valeurs envoyées par HomeSmart (pour reconnaître les changements faits ailleurs).
    sent: Mutex<HashMap<String, (Value, std::time::Instant)>>,
    /// Détection des problèmes : liaison, tension, consommation, défauts, origine de la minuterie.
    watch: Mutex<problems::DeviceWatch>,
    tx: mpsc::Sender<Command>,
}

impl DeviceHandle {
    fn note_sent(&self, dps: &Map<String, Value>) {
        let now = std::time::Instant::now();
        let mut sent = self.sent.lock().unwrap();
        for (k, v) in dps {
            sent.insert(k.clone(), (v.clone(), now));
        }
    }

    fn name(&self) -> String {
        self.cfg.read().unwrap().display_name().to_string()
    }
}

#[derive(Serialize, Clone)]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    pub room: Option<String>,
    pub kind: Option<String>,
    pub ip: Option<String>,
    pub version: Option<String>,
    pub online: bool,
    pub error: Option<String>,
    pub updated: u64,
    pub latency_ms: Option<u64>,
    pub dps: Map<String, Value>,
    pub schema: Map<String, Value>,
    pub safety: Safety,
    /// Secondes restantes avant que la minuterie de l'appareil ne bascule.
    pub countdown_left: Option<u64>,
    /// Allumé depuis (s), d'après les garde-fous.
    pub on_for_s: Option<u64>,
    pub faults: Vec<String>,
    /// Cette prise alimente le PC où tourne HomeSmart.
    pub powers_pc: bool,
}

#[derive(Serialize)]
pub struct DiscoveredView {
    #[serde(flatten)]
    pub device: Discovered,
    pub known: bool,
}

/// Une entrée du journal (action ou alerte).
pub type Alert = Entry;

/// Modification partielle d'un appareil (une chaîne vide efface le champ).
#[derive(Deserialize, Default)]
pub struct DevicePatch {
    pub name: Option<String>,
    pub room: Option<String>,
    pub kind: Option<String>,
    pub ip: Option<String>,
    pub version: Option<String>,
    pub key: Option<String>,
    pub powers_pc: Option<bool>,
}

pub struct Hub {
    config_path: PathBuf,
    config: Mutex<Config>,
    devices: RwLock<HashMap<String, Arc<DeviceHandle>>>,
    discovered: RwLock<HashMap<String, Discovered>>,
    energy: Mutex<EnergyStore>,
    journal: Mutex<Journal>,
    pub events: broadcast::Sender<Event>,
    /// Intervalle des mesures, en millisecondes (modifiable à chaud).
    meter_ms: AtomicU64,
    cameras: RwLock<HashMap<String, Arc<cameras::CameraHandle>>>,
    frame_waker: RwLock<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// FFmpeg (décodage H.265) : présence et installation en cours.
    ffmpeg: Mutex<cameras::FfmpegStatus>,
    /// Vision par IA : analyses en cours, installation des modèles, visages connus.
    analysis: Mutex<analysis::VisionState>,
    /// Scénarios : caméras analysées, absences en cours.
    scenes: Mutex<scenarios::SceneState>,
    /// Problèmes en cours, conditions qui durent, marque de session.
    problems: Mutex<problems::ProblemState>,
    /// Délais de détection raccourcis (démo, essais).
    fast_detection: AtomicBool,
    /// Aspirateurs robots hors Tuya (Valetudo, miIO).
    vacuums: RwLock<HashMap<String, Arc<vacuums::VacuumHandle>>>,
    /// Suivi de l'activité de tous les aspirateurs (Tuya ou non).
    vac_watch: Mutex<HashMap<String, vacuums::VacWatch>>,
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

impl Hub {
    pub fn new(config_path: PathBuf) -> Result<Arc<Hub>> {
        let config = Config::load(&config_path)?;
        let (events, _) = broadcast::channel(256);
        let energy = EnergyStore::load(config_path.with_file_name("energy.json"));
        let meter_ms = AtomicU64::new(interval_ms(config.prefs.meter_interval_s));
        let journal = Journal::open(config_path.with_file_name("journal.jsonl"));
        Ok(Arc::new(Hub {
            config_path,
            config: Mutex::new(config),
            devices: RwLock::new(HashMap::new()),
            discovered: RwLock::new(HashMap::new()),
            energy: Mutex::new(energy),
            journal: Mutex::new(journal),
            events,
            meter_ms,
            cameras: RwLock::default(),
            frame_waker: RwLock::default(),
            ffmpeg: Mutex::default(),
            analysis: Mutex::default(),
            scenes: Mutex::default(),
            problems: Mutex::default(),
            fast_detection: AtomicBool::new(false),
            vacuums: RwLock::default(),
            vac_watch: Mutex::default(),
        }))
    }

    /// Lance un acteur par appareil, le planificateur et, si demandé, l'écoute réseau.
    pub fn start(self: &Arc<Self>, discovery: bool) {
        let list = self.config.lock().unwrap().devices.clone();
        let cams = self.config.lock().unwrap().cameras.len();
        let cams = if cams > 0 { format!(", {cams} caméra(s)") } else { String::new() };
        let vacs = self.config.lock().unwrap().vacuums.len();
        let vacs = if vacs > 0 { format!(", {vacs} aspirateur(s) hors Tuya") } else { String::new() };
        self.log(Level::Info, Source::System, "", format!("HomeSmart démarré ({} appareil(s){cams}{vacs})", list.len()));
        // Marque de session : dit au prochain démarrage si celui-ci s'est terminé net (coupure).
        self.start_session_watch();
        for cfg in list {
            if let Err(e) = codec::parse_key(&cfg.key) {
                tracing::error!("{} ignoré : {e}", cfg.display_name());
                continue;
            }
            self.spawn_actor(cfg, DeviceState::default());
        }
        // Les scénarios commandent les appareils depuis les fils d'analyse : il leur faut le moteur tokio.
        self.scenes.lock().unwrap().set_runtime();
        self.start_cameras();
        self.start_vacuums();
        self.start_vision();
        if discovery {
            self.start_discovery();
        }
        self.start_scheduler();
        let hub = self.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(60)).await;
                hub.save_energy();
                hub.save_presence();
            }
        });
        // Journal : écrire les entrées regroupées, et relire les actions de la ligne de commande.
        let hub = self.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(3)).await;
                let new = {
                    let mut j = hub.journal.lock().unwrap();
                    j.flush_stale();
                    j.poll_external()
                };
                if let Some(e) = new.last() {
                    hub.emit(Event::Alert(e.id));
                }
            }
        });
    }

    /// À appeler à la fermeture : journal et historique d'énergie sur disque.
    pub fn shutdown(&self) {
        self.log(Level::Info, Source::System, "", "HomeSmart arrêté");
        self.journal.lock().unwrap().flush();
        self.end_session_watch();
        self.save_energy();
        self.save_presence();
    }

    fn save_energy(&self) {
        if let Err(e) = self.energy.lock().unwrap().save_if_dirty() {
            tracing::error!("sauvegarde de l'historique d'énergie impossible : {e:#}");
        }
    }

    fn spawn_actor(self: &Arc<Self>, cfg: DeviceConfig, state: DeviceState) -> Arc<DeviceHandle> {
        let (tx, rx) = mpsc::channel(32);
        let id = cfg.id.clone();
        let handle = Arc::new(DeviceHandle {
            cfg: RwLock::new(cfg),
            state: RwLock::new(state),
            guard: Mutex::default(),
            sent: Mutex::default(),
            watch: Mutex::new(problems::DeviceWatch::starting()),
            tx,
        });
        if let Some(old) = self.devices.write().unwrap().insert(id, handle.clone()) {
            let _ = old.tx.try_send(Command::Shutdown);
        }
        tokio::spawn(run_actor(self.clone(), handle.clone(), rx));
        handle
    }

    fn handle(&self, id: &str) -> Option<Arc<DeviceHandle>> {
        self.devices.read().unwrap().get(id).cloned()
    }

    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    fn emit_device(&self, h: &DeviceHandle) {
        self.emit(Event::Device(h.cfg.read().unwrap().id.clone()));
    }

    fn meter_interval(&self) -> Duration {
        Duration::from_millis(self.meter_ms.load(Ordering::Relaxed))
    }

    fn persist(&self) {
        let snapshot = {
            let devices = self.devices.read().unwrap();
            let mut cfg = self.config.lock().unwrap();
            for d in cfg.devices.iter_mut() {
                if let Some(h) = devices.get(&d.id) {
                    *d = h.cfg.read().unwrap().clone();
                }
            }
            cfg.clone()
        };
        if let Err(e) = snapshot.save(&self.config_path) {
            tracing::error!("sauvegarde de la configuration impossible : {e:#}");
        }
    }

    /// Inscrit une entrée au journal (toutes les actions y passent, sans exception).
    pub fn log(&self, level: Level, source: Source, device: &str, message: impl Into<String>) {
        let name = self.name_of(device);
        self.record(Entry::new(level, source, device, &name, message));
    }

    /// Comme `log`, mais les entrées de même clé rapprochées sont regroupées (curseur que l'on fait glisser).
    pub fn log_keyed(&self, level: Level, source: Source, device: &str, key: &str, message: impl Into<String>) {
        let name = self.name_of(device);
        self.record(Entry::new(level, source, device, &name, message).with_key(key));
    }

    /// Nom affiché d'un appareil ou d'une caméra.
    fn name_of(&self, id: &str) -> String {
        if id.is_empty() {
            return String::new();
        }
        self.handle(id).map(|h| h.name()).or_else(|| self.camera_name(id)).or_else(|| self.vacuum_name(id)).unwrap_or_default()
    }

    fn record(&self, e: Entry) {
        let who = if e.device_name.is_empty() { String::new() } else { format!("{} : ", e.device_name) };
        match e.level {
            Level::Info => tracing::info!("[{}] {who}{}", e.source.label(), e.message),
            _ => tracing::warn!("[{}] {who}{}", e.source.label(), e.message),
        }
        let id = self.journal.lock().unwrap().append(e);
        self.emit(Event::Alert(id));
    }

    /// Comme `record`, écrit tout de suite et forcé sur le disque (problèmes : ils doivent survivre
    /// à la coupure qu'ils annoncent parfois).
    fn record_synced(&self, e: Entry) {
        let who = if e.device_name.is_empty() { String::new() } else { format!("{} : ", e.device_name) };
        tracing::warn!("[{}] {who}{}", e.source.label(), e.message);
        let id = self.journal.lock().unwrap().append_synced(e);
        self.emit(Event::Alert(id));
    }

    // ---- Lecture -------------------------------------------------------------

    pub fn list(&self) -> Vec<DeviceView> {
        let order: Vec<String> = self.config.lock().unwrap().devices.iter().map(|d| d.id.clone()).collect();
        let devices = self.devices.read().unwrap();
        order.iter().filter_map(|id| devices.get(id)).map(|h| view_of(h)).collect()
    }

    /// Configuration complète (clé locale comprise) d'un appareil.
    pub fn config_of(&self, id: &str) -> Option<DeviceConfig> {
        self.handle(id).map(|h| h.cfg.read().unwrap().clone())
    }

    pub fn view(&self, id: &str) -> Option<DeviceView> {
        self.handle(id).map(|h| view_of(&h))
    }

    /// Identifiant à partir d'un identifiant ou d'un nom.
    pub fn resolve(&self, query: &str) -> Option<String> {
        if self.devices.read().unwrap().contains_key(query) {
            return Some(query.to_string());
        }
        self.config.lock().unwrap().find(query).map(|d| d.id.clone())
    }

    pub fn discovered(&self) -> Vec<DiscoveredView> {
        let devices = self.devices.read().unwrap();
        let mut list: Vec<_> = self
            .discovered
            .read()
            .unwrap()
            .values()
            .map(|d| DiscoveredView { device: d.clone(), known: devices.contains_key(&d.id) })
            .collect();
        list.sort_by(|a, b| a.known.cmp(&b.known).then(a.device.ip.cmp(&b.device.ip)));
        list
    }

    /// Les `limit` dernières entrées du journal, la plus récente d'abord.
    pub fn alerts(&self, limit: usize) -> Vec<Alert> {
        self.journal.lock().unwrap().latest(limit)
    }

    /// Recherche dans le journal, la plus récente d'abord.
    pub fn journal_query(&self, limit: usize, pred: impl Fn(&Alert) -> bool) -> Vec<Alert> {
        self.journal.lock().unwrap().query(limit, pred)
    }

    pub fn journal_len(&self) -> usize {
        self.journal.lock().unwrap().len()
    }

    /// Entrées postérieures à `since`, la plus récente d'abord.
    pub fn alerts_after(&self, since: u64) -> Vec<Alert> {
        self.journal.lock().unwrap().after(since)
    }

    /// (numéro de la dernière entrée, nombre d'avertissements et alertes de sécurité après `since`).
    pub fn alert_stats(&self, since: u64) -> (u64, usize) {
        let j = self.journal.lock().unwrap();
        (j.last_id(), j.unread_important(since))
    }

    pub fn journal_path(&self) -> PathBuf {
        self.journal.lock().unwrap().path().to_path_buf()
    }

    /// Lecture de l'historique d'énergie.
    pub fn energy<R>(&self, f: impl FnOnce(&EnergyStore) -> R) -> R {
        f(&self.energy.lock().unwrap())
    }

    /// Remplit un historique fictif (mode démo).
    pub fn seed_demo_energy(&self, id: &str, base_w: f64) {
        self.energy.lock().unwrap().seed_demo(id, base_w, Local::now());
    }

    pub fn house(&self) -> crate::house::HousePlan {
        self.config.lock().unwrap().house.clone()
    }

    /// Enregistre le plan de la maison ; `what` décrit la modification pour le journal
    /// (les déplacements successifs d'un même élément sont regroupés).
    pub fn set_house(&self, plan: crate::house::HousePlan, what: &str, key: &str) {
        if self.config.lock().unwrap().house == plan {
            return;
        }
        self.config.lock().unwrap().house = plan;
        self.log_keyed(Level::Info, Source::User, "", &format!("house:{key}"), format!("Plan de la maison : {what}"));
        self.persist();
    }

    pub fn prefs(&self) -> Prefs {
        self.config.lock().unwrap().prefs.clone()
    }

    pub fn set_prefs(&self, prefs: Prefs) {
        self.meter_ms.store(interval_ms(prefs.meter_interval_s), Ordering::Relaxed);
        let old = self.config.lock().unwrap().prefs.clone();
        if old == prefs {
            return;
        }
        if old.meter_interval_s != prefs.meter_interval_s {
            let msg = format!("Mesures toutes les {} s (avant : {} s)", kinds::fr(prefs.meter_interval_s, 0), kinds::fr(old.meter_interval_s, 0));
            self.log_keyed(Level::Info, Source::User, "", "prefs:meter", msg);
        }
        if old.audio_enhance != prefs.audio_enhance {
            let msg = if prefs.audio_enhance { "Son des caméras amélioré (volume automatique, filtre des basses)" } else { "Son des caméras brut" };
            self.log_keyed(Level::Info, Source::User, "", "prefs:audio", msg);
        }
        if old.light_theme != prefs.light_theme {
            let msg = if prefs.light_theme { "Thème clair de l'application" } else { "Thème sombre de l'application" };
            self.log_keyed(Level::Info, Source::User, "", "prefs:theme", msg);
        }
        if old.os_notifications != prefs.os_notifications {
            let msg = if prefs.os_notifications { "Notifications du système activées (alertes critiques)" } else { "Notifications du système désactivées" };
            self.log_keyed(Level::Info, Source::User, "", "prefs:notify", msg);
        }
        if old.price_kwh != prefs.price_kwh {
            let msg = format!("Prix du kWh : {} € (avant : {} €)", kinds::fr(prefs.price_kwh, 4), kinds::fr(old.price_kwh, 4));
            self.log_keyed(Level::Info, Source::User, "", "prefs:price", msg);
        }
        if old.start_minimized != prefs.start_minimized {
            self.log(Level::Info, Source::User, "", format!("Démarrage réduit : {}", if prefs.start_minimized { "oui" } else { "non" }));
        }
        self.config.lock().unwrap().prefs = prefs;
        self.persist();
    }

    // ---- Commandes -----------------------------------------------------------

    /// Écrit des DP. Chaque écriture, réussie ou refusée, est inscrite au journal avec son origine.
    pub async fn set_dps(&self, id: &str, dps: Map<String, Value>, source: Source) -> Result<()> {
        self.set_dps_as(id, dps, source, None).await
    }

    /// Comme `set_dps`, avec la description à inscrire au journal (commande d'un aspirateur :
    /// « Nettoyage lancé » plutôt que la liste des DP écrits).
    pub(crate) async fn set_dps_as(&self, id: &str, dps: Map<String, Value>, source: Source, described: Option<String>) -> Result<()> {
        if dps.is_empty() {
            bail!("aucun DP à écrire");
        }
        let h = self.handle(id).ok_or_else(|| anyhow!("appareil inconnu"))?;
        let description = {
            let cfg = h.cfg.read().unwrap();
            let st = h.state.read().unwrap();
            let kind = kinds::detect(cfg.kind.as_deref(), &st.dps);
            let description = described.unwrap_or_else(|| {
                let parts: Vec<String> = dps.iter().map(|(k, v)| kinds::describe_write(k, v, &kind, &st.dps, &cfg.schema)).collect();
                parts.join(" · ")
            });
            // Étalonnage d'usine, valeurs inconnues de l'appareil, plages : refus.
            if let Err(e) = kinds::check_writable(&dps, &st.dps, &cfg.schema) {
                drop((cfg, st));
                self.log(Level::Warning, source, id, format!("Refusé : {e}"));
                bail!(e);
            }
            let switches: Vec<String> = match &kind {
                Kind::Switch { gangs } => gangs.clone(),
                k => k.switch_dp(&st.dps).into_iter().collect(),
            };
            let turns_on = switches.iter().any(|s| dps.get(s) == Some(&Value::Bool(true)));
            if turns_on && cfg.safety.forbids_now(Local::now().time()) {
                let r = cfg.safety.forbidden.as_ref().map(|r| r.label()).unwrap_or_default();
                drop((cfg, st));
                self.log(Level::Warning, source, id, format!("Refusé : allumage interdit par les garde-fous ({r})"));
                bail!("allumage interdit par les garde-fous ({r})");
            }
            description
        };
        let (tx, rx) = oneshot::channel();
        let sent = dps.clone();
        // Noté avant l'envoi : l'écho d'une prise rapide peut arriver avant la fin de cette fonction,
        // et ne doit pas passer pour un changement fait ailleurs.
        h.note_sent(&sent);
        let result = async {
            h.tx.send(Command::Set(dps, tx)).await.map_err(|_| anyhow!("appareil arrêté"))?;
            tokio::time::timeout(Duration::from_secs(5), rx).await.map_err(|_| anyhow!("délai dépassé"))?.map_err(|_| anyhow!("appareil arrêté"))?
        }
        .await;
        match &result {
            Ok(()) => {
                // Un curseur que l'on fait glisser ne fait qu'une entrée ; un marche/arrêt en fait toujours une.
                if sent.values().any(Value::is_boolean) {
                    self.log(Level::Info, source, id, description);
                } else {
                    let key = format!("{id}:{}", sent.keys().cloned().collect::<Vec<_>>().join(","));
                    self.log_keyed(Level::Info, source, id, &key, description);
                }
            }
            Err(e) => self.log(Level::Warning, source, id, format!("Échec : {description} ({e:#})")),
        }
        result
    }

    /// Allume/éteint via le DP principal (toutes les sorties d'une multiprise),
    /// ou `dp` s'il est précisé.
    pub async fn power(&self, id: &str, on: bool, dp: Option<String>, source: Source) -> Result<String> {
        // Aspirateur robot : « allumer » le lance, « éteindre » le renvoie à sa base.
        if dp.is_none() && self.is_vacuum(id) {
            let cmd = if on { crate::vacuum::Command::Start } else { crate::vacuum::Command::Dock };
            self.vacuum_command(id, cmd, source).await?;
            return Ok(String::new());
        }
        let h = self.handle(id).ok_or_else(|| anyhow!("appareil inconnu"))?;
        let dps_now = h.state.read().unwrap().dps.clone();
        let kind = kinds::detect(h.cfg.read().unwrap().kind.as_deref(), &dps_now);
        let targets: Vec<String> = match (dp, &kind) {
            (Some(dp), _) => vec![dp],
            (None, Kind::Switch { gangs }) => gangs.clone(),
            (None, k) => vec![k.switch_dp(&dps_now).ok_or_else(|| anyhow!("pas de DP marche/arrêt connu (état pas encore reçu ?)"))?],
        };
        let m: Map<String, Value> = targets.iter().map(|d| (d.clone(), Value::Bool(on))).collect();
        self.set_dps(id, m, source).await?;
        Ok(targets.join(","))
    }

    /// Programme la minuterie de l'appareil (0 = annuler).
    pub async fn set_countdown(&self, id: &str, secs: u64, source: Source) -> Result<()> {
        let h = self.handle(id).ok_or_else(|| anyhow!("appareil inconnu"))?;
        let dp = {
            let cfg = h.cfg.read().unwrap();
            let st = h.state.read().unwrap();
            let kind = kinds::detect(cfg.kind.as_deref(), &st.dps);
            kinds::countdown_dp(&kind, &st.dps, &cfg.schema).ok_or_else(|| anyhow!("cet appareil n'a pas de minuterie"))?
        };
        let mut m = Map::new();
        m.insert(dp, json!(secs.min(86_400)));
        self.set_dps(id, m, source).await
    }

    pub fn refresh(&self, id: &str) -> Result<()> {
        let h = self.handle(id).ok_or_else(|| anyhow!("appareil inconnu"))?;
        let _ = h.tx.try_send(Command::Refresh);
        Ok(())
    }

    pub fn add(self: &Arc<Self>, mut cfg: DeviceConfig) -> Result<DeviceView> {
        cfg.id = cfg.id.trim().to_string();
        if cfg.id.is_empty() {
            bail!("identifiant requis");
        }
        codec::parse_key(&cfg.key)?;
        if let Some(v) = &cfg.version {
            v.parse::<Version>()?;
        }
        if cfg.name.trim().is_empty() {
            cfg.name = cfg.id.clone();
        }
        let created = self.config.lock().unwrap().upsert(cfg.clone());
        let h = self.spawn_actor(cfg, DeviceState::default());
        self.persist();
        self.emit_device(&h);
        let id = h.cfg.read().unwrap().id.clone();
        self.log(Level::Info, Source::User, &id, if created { "Appareil ajouté" } else { "Appareil mis à jour (réimporté)" });
        Ok(view_of(&h))
    }

    pub fn update(self: &Arc<Self>, id: &str, p: DevicePatch) -> Result<DeviceView> {
        let h = self.handle(id).ok_or_else(|| anyhow!("appareil inconnu"))?;
        let before = h.cfg.read().unwrap().clone();
        let mut cfg = before.clone();
        if let Some(name) = non_empty(p.name) {
            cfg.name = name;
        }
        if p.room.is_some() {
            cfg.room = non_empty(p.room);
        }
        if p.kind.is_some() {
            cfg.kind = non_empty(p.kind);
        }
        if let Some(b) = p.powers_pc {
            cfg.powers_pc = b;
        }
        let mut restart = false;
        if p.ip.is_some() {
            cfg.ip = non_empty(p.ip);
            restart = true;
        }
        if p.version.is_some() {
            cfg.version = non_empty(p.version);
            if let Some(v) = &cfg.version {
                v.parse::<Version>()?;
            }
            restart = true;
        }
        if let Some(key) = non_empty(p.key) {
            codec::parse_key(&key)?;
            cfg.key = key;
            restart = true;
        }
        let mut changes = Vec::new();
        let opt = |o: &Option<String>| o.clone().unwrap_or_else(|| "automatique".into());
        if before.name != cfg.name {
            changes.push(format!("renommé « {} » → « {} »", before.name, cfg.name));
        }
        if before.room != cfg.room {
            changes.push(format!("pièce : {}", cfg.room.clone().unwrap_or_else(|| "aucune".into())));
        }
        if before.kind != cfg.kind {
            changes.push(format!("type : {}", opt(&cfg.kind)));
        }
        if before.ip != cfg.ip {
            changes.push(format!("adresse IP : {}", opt(&cfg.ip)));
        }
        if before.version != cfg.version {
            changes.push(format!("protocole : {}", opt(&cfg.version)));
        }
        if before.key != cfg.key {
            changes.push("clé locale modifiée".into());
        }
        if before.powers_pc != cfg.powers_pc {
            changes.push(if cfg.powers_pc { "alimente ce PC".into() } else { "n'alimente pas ce PC".into() });
        }
        let h = if restart {
            let state = DeviceState { online: false, ..h.state.read().unwrap().clone() };
            self.spawn_actor(cfg, state)
        } else {
            *h.cfg.write().unwrap() = cfg;
            h
        };
        self.persist();
        self.emit_device(&h);
        if !changes.is_empty() {
            self.log(Level::Info, Source::User, id, format!("Réglages : {}", changes.join(", ")));
        }
        Ok(view_of(&h))
    }

    pub fn set_safety(&self, id: &str, safety: Safety) -> Result<()> {
        if let Some(r) = &safety.forbidden {
            if automation::parse_hm(&r.start).is_none() || automation::parse_hm(&r.end).is_none() {
                bail!("plage horaire invalide");
            }
        }
        let h = self.handle(id).ok_or_else(|| anyhow!("appareil inconnu"))?;
        let summary = safety_summary(&safety);
        h.cfg.write().unwrap().safety = safety;
        self.log(Level::Info, Source::User, id, summary);
        self.persist();
        self.emit_device(&h);
        // Réévaluer tout de suite (minuterie de sécurité, plage interdite…).
        let _ = h.tx.try_send(Command::Refresh);
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        let h = self.devices.write().unwrap().remove(id).ok_or_else(|| anyhow!("appareil inconnu"))?;
        self.record(Entry::new(Level::Info, Source::User, id, &h.name(), "Appareil supprimé (ses programmes et son historique aussi)"));
        let _ = h.tx.try_send(Command::Shutdown);
        self.forget_problems(id);
        {
            let mut cfg = self.config.lock().unwrap();
            cfg.devices.retain(|d| d.id != id);
            cfg.schedules.retain(|s| s.device != id);
            cfg.scenarios.retain(|s| s.device != id);
        }
        self.energy.lock().unwrap().forget(id);
        self.vac_watch.lock().unwrap().remove(id);
        self.persist();
        self.emit(Event::Removed(id.to_string()));
        Ok(())
    }

    // ---- Programmes ------------------------------------------------------------

    pub fn schedules(&self) -> Vec<Schedule> {
        self.config.lock().unwrap().schedules.clone()
    }

    /// Crée (id 0) ou remplace un programme. Renvoie son identifiant.
    pub fn save_schedule(&self, mut s: Schedule) -> Result<u64> {
        s.validate().map_err(|e| anyhow!(e))?;
        if self.handle(&s.device).is_none() && !self.is_vacuum(&s.device) {
            bail!("appareil inconnu");
        }
        if s.action.for_vacuum() && !self.is_vacuum(&s.device) {
            bail!("nettoyer ou renvoyer à la base : réservé aux aspirateurs robots");
        }
        let (id, before) = {
            let mut cfg = self.config.lock().unwrap();
            if s.id == 0 {
                s.id = cfg.schedules.iter().map(|x| x.id).max().unwrap_or(0) + 1;
            }
            let before = cfg.schedules.iter().find(|x| x.id == s.id).cloned();
            match cfg.schedules.iter_mut().find(|x| x.id == s.id) {
                Some(x) => *x = s.clone(),
                None => cfg.schedules.push(s.clone()),
            }
            (s.id, before)
        };
        let msg = match &before {
            None => format!("Programme créé : {}", s.summary()),
            Some(b) if b.enabled != s.enabled && b.summary() == s.summary() => {
                format!("Programme {} : {}", if s.enabled { "activé" } else { "suspendu" }, s.summary())
            }
            Some(b) => format!("Programme modifié : {} (avant : {})", s.summary(), b.summary()),
        };
        self.log(Level::Info, Source::User, &s.device, msg);
        self.persist();
        self.emit(Event::Schedules);
        Ok(id)
    }

    pub fn delete_schedule(&self, id: u64) {
        let removed = self.config.lock().unwrap().schedules.iter().find(|s| s.id == id).cloned();
        if let Some(s) = &removed {
            self.log(Level::Info, Source::User, &s.device, format!("Programme supprimé : {}", s.summary()));
        }
        self.config.lock().unwrap().schedules.retain(|s| s.id != id);
        self.persist();
        self.emit(Event::Schedules);
    }

    fn start_scheduler(self: &Arc<Self>) {
        let hub = self.clone();
        tokio::spawn(async move {
            let mut last = Local::now().naive_local();
            loop {
                sleep(Duration::from_secs(5)).await;
                // Scénarios d'absence (pièce vide depuis N minutes).
                hub.scene_tick();
                // Problèmes qui durent : hors ligne, caméra sans image, analyse en erreur.
                hub.problems_tick();
                let now = Local::now().naive_local();
                if (now.date(), now.hour(), now.minute()) == (last.date(), last.hour(), last.minute()) {
                    continue;
                }
                let schedules = hub.config.lock().unwrap().schedules.clone();
                let events = automation::due_events(&schedules, last, now);
                last = now;
                for ev in events {
                    // La commande elle-même est journalisée (origine : programme).
                    if let Err(e) = hub.run_action(&ev.device, ev.action, Source::Schedule).await {
                        let action = ev.action.noun();
                        hub.log(Level::Warning, Source::Schedule, &ev.device, format!("{action} de {} non exécuté : {e:#}", automation::fmt_hm(ev.at.time())));
                    }
                }
            }
        });
    }

    // ---- Découverte ----------------------------------------------------------

    fn start_discovery(self: &Arc<Self>) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ports = discovery::spawn_listeners(tx);
        tracing::info!("découverte réseau active ({ports}/3 ports UDP)");
        let hub = self.clone();
        tokio::spawn(async move {
            while let Some(d) = rx.recv().await {
                hub.on_discovered(d);
            }
        });
        tokio::spawn(async {
            loop {
                discovery::send_discovery_request().await;
                sleep(Duration::from_secs(30)).await;
            }
        });
    }

    pub fn trigger_scan(&self) {
        self.log(Level::Info, Source::User, "", "Recherche d'appareils sur le Wi-Fi");
        tokio::spawn(discovery::send_discovery_request());
    }

    fn on_discovered(&self, d: Discovered) {
        let (changed, first) = {
            let mut map = self.discovered.write().unwrap();
            let prev = map.insert(d.id.clone(), d.clone());
            (prev.as_ref().map_or(true, |p| p.ip != d.ip || p.version != d.version), prev.is_none())
        };
        if first && self.handle(&d.id).is_none() {
            self.log(Level::Info, Source::System, "", format!("Nouvel appareil détecté sur le Wi-Fi : {} ({}, protocole {})", d.id, d.ip, d.version));
        }
        if let Some(h) = self.handle(&d.id) {
            let ip_changed = {
                let mut cfg = h.cfg.write().unwrap();
                if cfg.port.is_none() && cfg.ip.as_deref() != Some(d.ip.as_str()) {
                    cfg.ip = Some(d.ip.clone());
                    true
                } else {
                    false
                }
            };
            if ip_changed {
                self.log(Level::Info, Source::System, &d.id, format!("Nouvelle adresse IP : {}", d.ip));
                self.persist();
            }
            if !h.state.read().unwrap().online {
                let _ = h.tx.try_send(Command::Refresh);
            }
        }
        if changed {
            tracing::info!("annonce : {} @ {} (v{})", d.id, d.ip, d.version);
            self.emit(Event::Discovered);
        }
    }

    fn target_for(&self, h: &DeviceHandle) -> Result<Target> {
        let cfg = h.cfg.read().unwrap().clone();
        let key = codec::parse_key(&cfg.key)?;
        let seen = self.discovered.read().unwrap().get(&cfg.id).cloned();
        let ip = cfg
            .ip
            .clone()
            .or_else(|| seen.as_ref().map(|d| d.ip.clone()))
            .ok_or_else(|| anyhow!("IP inconnue : en attente d'une annonce de l'appareil sur le réseau"))?;
        let version = match cfg.version.as_deref().or(seen.as_ref().map(|d| d.version.as_str())) {
            Some(v) => v.parse()?,
            None => Version::V33,
        };
        Ok(Target { id: cfg.id, key, ip, port: cfg.port.unwrap_or(PORT_TCP), version })
    }

    // ---- Garde-fous et énergie, appliqués par l'acteur -------------------------

    /// Après une mise à jour d'état (`update` = DP reçus, vide lors d'un battement de cœur).
    async fn after_update(&self, dev: &DeviceHandle, writer: &mut Writer, update: &Map<String, Value>) -> Result<()> {
        let now = std::time::Instant::now();
        let (cfg, dps) = (dev.cfg.read().unwrap().clone(), dev.state.read().unwrap().dps.clone());
        let kind = kinds::detect(cfg.kind.as_deref(), &dps);
        let countdown_dp = kinds::countdown_dp(&kind, &dps, &cfg.schema);
        if let Some(dp) = &countdown_dp {
            if let Some(v) = update.get(dp).and_then(Value::as_u64) {
                // Nouvelle minuterie : réglée par HomeSmart si elle répond à ce qu'il a envoyé
                // (sinon par l'application Smart Life ou l'appareil lui-même).
                let prev_left = countdown_left(&dev.state.read().unwrap());
                if v > 0 && prev_left.is_none_or(|l| v.abs_diff(l) > 15) {
                    let ours = dev.sent.lock().unwrap().get(dp).is_some_and(|(sv, t)| sv.as_u64() == Some(v) && t.elapsed() < Duration::from_secs(30));
                    dev.watch.lock().unwrap().timer_ours = ours;
                }
                dev.state.write().unwrap().countdown = (v > 0).then_some((v, now));
            }
        }
        let fresh_power = update.contains_key("19");
        let power = kinds::power_w(&dps, &cfg.schema).filter(|_| matches!(kind, Kind::Plug { metering: true }) || kinds::code_of(&cfg.schema, "19") == Some("cur_power"));
        if let (true, Some(w)) = (fresh_power, power) {
            self.energy.lock().unwrap().record(&cfg.id, Local::now(), w);
        }

        if kind == Kind::Vacuum {
            // Aspirateur : son activité et ses erreurs (ni garde-fous ni consommation).
            self.vacuum_observe(&cfg.id, &crate::vacuum::tuya::status(&dps, &cfg.schema));
            return Ok(());
        }
        let switch_dp = match &kind {
            Kind::Switch { gangs } => gangs.first().cloned(),
            k => k.switch_dp(&dps),
        };
        let on = switch_dp.as_ref().and_then(|d| dps.get(d)).and_then(Value::as_bool);
        // Détection des problèmes : défauts signalés, tension, consommation inhabituelle.
        self.watch_measures(dev, update, on, power, fresh_power);
        let (fault_bits, fault_labels) = kinds::faults(&dps, &cfg.schema);
        let obs = Observation {
            now_local: Local::now().time(),
            on,
            power_w: power,
            faults: fault_bits,
            fault_labels,
            has_countdown: countdown_dp.is_some(),
            countdown_left: countdown_left(&dev.state.read().unwrap()),
        };
        let actions = {
            let mut g = dev.guard.lock().unwrap();
            let actions = automation::evaluate(&cfg.safety, &mut g, &obs, fresh_power, now);
            // Anti-répétition : une coupure déjà demandée il y a moins de 5 s.
            let recent_off = g.last_off.is_some_and(|t| now.duration_since(t) < Duration::from_secs(5));
            let actions: Vec<GuardAction> = actions.into_iter().filter(|a| !(recent_off && matches!(a, GuardAction::TurnOff(_)))).collect();
            if actions.iter().any(|a| matches!(a, GuardAction::TurnOff(_))) {
                g.last_off = Some(now);
            }
            actions
        };
        for a in actions {
            match a {
                GuardAction::TurnOff(reason) => {
                    let mut m = Map::new();
                    match &kind {
                        Kind::Switch { gangs } => gangs.iter().for_each(|g| {
                            m.insert(g.clone(), Value::Bool(false));
                        }),
                        _ => {
                            if let Some(d) = &switch_dp {
                                m.insert(d.clone(), Value::Bool(false));
                            }
                        }
                    }
                    if !m.is_empty() {
                        writer.control(&m).await?;
                        dev.note_sent(&m);
                        self.log(Level::Critical, Source::Guard, &cfg.id, format!("Éteint : {reason}"));
                    }
                }
                GuardAction::SetCountdown(secs) => {
                    if let Some(dp) = &countdown_dp {
                        let mut m = Map::new();
                        m.insert(dp.clone(), json!(secs));
                        writer.control(&m).await?;
                        dev.note_sent(&m);
                        dev.state.write().unwrap().countdown = Some((secs, now));
                        dev.watch.lock().unwrap().timer_ours = true;
                        self.log(Level::Info, Source::Guard, &cfg.id, format!("Minuterie de sécurité : extinction dans {}", kinds::fmt_secs(secs)));
                    }
                }
            }
        }
        Ok(())
    }
}

impl Hub {
    /// Changements arrivés de l'appareil : ceux que HomeSmart n'a pas demandés sont des actions
    /// faites ailleurs (bouton physique, autre application, minuterie interne, coupure de courant).
    /// Ils sont jugés quelques secondes plus tard (la ligne de commande inscrit les siens
    /// elle-même) ; une extinction ou un allumage non demandés sont des problèmes, avec leur cause
    /// probable. `outage` : premier état reçu après une perte de liaison de cette durée.
    fn log_device_changes(self: &Arc<Self>, dev: &Arc<DeviceHandle>, changes: &[(String, Value)], outage: Option<Duration>) {
        if changes.is_empty() {
            return;
        }
        let (cfg, st) = (dev.cfg.read().unwrap().clone(), dev.state.read().unwrap().clone());
        let kind = kinds::detect(cfg.kind.as_deref(), &st.dps);
        // La minuterie de la prise arrivait à échéance (ou a échu pendant une perte de liaison, PC en
        // veille…) : c'est elle qui a basculé.
        let by_timer = st.countdown.is_some_and(|(v, t)| {
            let e = t.elapsed().as_secs();
            e + 15 >= v && (e <= v + 15 || outage.is_some())
        });
        let timer_ours = dev.watch.lock().unwrap().timer_ours;
        let switches: Vec<String> = match &kind {
            Kind::Switch { gangs } => gangs.clone(),
            k => k.switch_dp(&st.dps).into_iter().collect(),
        };
        let mut parts = Vec::new();
        let mut outputs = Vec::new();
        let mut turned: Option<bool> = None;
        {
            let mut sent = dev.sent.lock().unwrap();
            for (dp, new) in changes {
                if kinds::is_measurement(dp, &kind, &st.dps, &cfg.schema) {
                    continue;
                }
                if let Some((v, t)) = sent.get(dp) {
                    if v == new && t.elapsed() < OWN_ECHO {
                        sent.remove(dp);
                        continue;
                    }
                }
                let text = kinds::describe_write(dp, new, &kind, &st.dps, &cfg.schema);
                if let (true, Some(on)) = (switches.contains(dp), new.as_bool()) {
                    turned = Some(turned.unwrap_or(false) || on);
                    outputs.push(text.clone());
                }
                parts.push(text);
            }
        }
        if parts.is_empty() {
            return;
        }
        let suffix = match (by_timer, timer_ours) {
            (true, true) => " (minuterie réglée par HomeSmart)",
            (true, false) => " (minuterie de la prise)",
            _ => "",
        };
        // État de l'appareil avec ce changement (défauts signalés, réglage au retour du courant).
        let mut after = st.dps.clone();
        after.extend(changes.iter().cloned());
        let switch = turned.map(|on| {
            let change = crate::problems::SwitchChange {
                on,
                by_timer,
                timer_ours,
                after_outage: outage,
                after_cut: kinds::after_power_cut(&after, &cfg.schema),
                faults: kinds::faults(&after, &cfg.schema).1,
                guarded: cfg.safety.enabled,
            };
            (outputs, change)
        });
        self.defer_external(dev, problems::ExternalChange { at: Local::now(), parts, suffix, switch });
    }
}

fn safety_summary(s: &Safety) -> String {
    if !s.enabled {
        return "Garde-fous désactivés".into();
    }
    let mut p = Vec::new();
    if s.confirm_on {
        p.push("confirmation avant d'allumer".to_string());
    }
    if let Some(m) = s.max_on_minutes {
        p.push(format!("arrêt auto après {}", automation::fmt_minutes(m)));
    }
    if let Some(w) = s.max_power_w {
        p.push(format!("limite {} W", kinds::fr(w, 0)));
    }
    if let Some(r) = &s.forbidden {
        p.push(format!("interdit {}", r.label()));
    }
    if s.off_on_fault {
        p.push("coupure sur défaut".into());
    }
    format!("Garde-fous activés : {}", if p.is_empty() { "aucune règle".into() } else { p.join(", ") })
}

fn interval_ms(secs: f64) -> u64 {
    if secs.is_finite() && secs > 0.0 {
        (secs.clamp(1.0, 60.0) * 1000.0) as u64
    } else {
        DEFAULT_METER_INTERVAL.as_millis() as u64
    }
}

fn countdown_left(st: &DeviceState) -> Option<u64> {
    st.countdown.map(|(v, t)| v.saturating_sub(t.elapsed().as_secs())).filter(|v| *v > 0)
}

fn view_of(h: &DeviceHandle) -> DeviceView {
    let cfg = h.cfg.read().unwrap().clone();
    let st = h.state.read().unwrap().clone();
    let on_for_s = h.guard.lock().unwrap().on_since.map(|t| t.elapsed().as_secs());
    let (_, faults) = kinds::faults(&st.dps, &cfg.schema);
    DeviceView {
        name: cfg.display_name().to_string(),
        countdown_left: countdown_left(&st),
        on_for_s,
        faults,
        powers_pc: cfg.powers_pc,
        id: cfg.id,
        room: cfg.room,
        kind: cfg.kind,
        ip: st.ip.or(cfg.ip),
        version: st.version.or(cfg.version),
        online: st.online,
        error: st.error,
        updated: st.updated,
        latency_ms: st.latency_ms,
        dps: st.dps,
        schema: cfg.schema,
        safety: cfg.safety,
    }
}

// ---- Acteur d'un appareil ----------------------------------------------------

async fn run_actor(hub: Arc<Hub>, dev: Arc<DeviceHandle>, mut rx: mpsc::Receiver<Command>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let mut connected = false;
        let result = match hub.target_for(&dev) {
            Ok(t) => session(&hub, &dev, &t, &mut rx, &mut connected).await,
            Err(e) => Err(e),
        };
        let id = dev.cfg.read().unwrap().id.clone();
        hub.energy.lock().unwrap().break_series(&id);
        let err = match result {
            Ok(()) => return, // arrêt demandé
            Err(e) => e,
        };
        let name = dev.cfg.read().unwrap().display_name().to_string();
        let msg = format!("{err:#}");
        let (changed, was_online) = {
            let mut s = dev.state.write().unwrap();
            let changed = s.online || s.error.as_deref() != Some(msg.as_str());
            let was_online = s.online;
            s.online = false;
            s.error = Some(msg.clone());
            (changed, was_online)
        };
        if was_online {
            hub.note_lost(&dev, &msg);
        } else {
            hub.note_error(&dev, &msg);
        }
        if changed {
            tracing::warn!("{name} : {msg}");
            hub.emit_device(&dev);
            let guards = if dev.cfg.read().unwrap().safety.enabled { " — les garde-fous de HomeSmart ne peuvent plus agir" } else { "" };
            let what = if was_online { "Connexion perdue" } else { "Injoignable" };
            // Simple information : si la liaison ne revient pas vite, la détection des problèmes
            // le signale (« Hors ligne », avec la cause probable).
            hub.log(Level::Info, Source::System, &id, format!("{what} : {msg}{guards}"));
        }
        if connected {
            backoff = Duration::from_secs(1);
        }
        let until = Instant::now() + backoff;
        loop {
            tokio::select! {
                _ = sleep_until(until) => break,
                c = rx.recv() => match c {
                    None | Some(Command::Shutdown) => return,
                    Some(Command::Set(_, reply)) => { let _ = reply.send(Err(anyhow!("appareil hors ligne : {msg}"))); }
                    Some(Command::Refresh) => break,
                }
            }
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn session(
    hub: &Arc<Hub>,
    dev: &Arc<DeviceHandle>,
    t: &Target,
    rx: &mut mpsc::Receiver<Command>,
    connected: &mut bool,
) -> Result<()> {
    let (mut reader, mut writer) = device::connect(t).await?;
    *connected = true;
    hub.note_connected(dev);
    {
        let mut s = dev.state.write().unwrap();
        s.online = true;
        s.error = None;
        s.ip = Some(t.ip.clone());
        s.version = Some(t.version.to_string());
    }
    hub.log(Level::Info, Source::System, &t.id, format!("Connecté ({}, protocole {})", t.ip, t.version));
    hub.emit_device(dev);

    let hint = || -> Vec<String> { dev.state.read().unwrap().dps.keys().cloned().collect() };
    writer.query_all(&hint()).await?;

    let mut hb = tokio::time::interval_at(Instant::now() + HEARTBEAT, HEARTBEAT);
    hb.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_rx = Instant::now();
    // (envoi, échéance) de la dernière commande en attente de confirmation.
    let mut pending: Option<(Instant, Instant)> = None;
    let mut last_meter = Instant::now();

    loop {
        let deadline = pending.map(|p| p.1);
        let confirm = async move {
            match deadline {
                Some(d) => sleep_until(d).await,
                None => std::future::pending().await,
            }
        };
        // Mesures : seulement pour les appareils qui en ont, au rythme choisi (relu à chaque tour).
        let metering: Vec<u32> = {
            let s = dev.state.read().unwrap();
            METERING_DPS.into_iter().filter(|k| s.dps.get(&k.to_string()).is_some_and(Value::is_number)).collect()
        };
        let meter_at = (!metering.is_empty()).then(|| last_meter + hub.meter_interval());
        let meter = async move {
            match meter_at {
                Some(d) => sleep_until(d).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            msg = reader.next() => {
                let msg = msg?;
                last_rx = Instant::now();
                if msg.text.contains("data unvalid") && !writer.codec.dev22 {
                    // Appareil « device22 » : il faut lister les DP demandés.
                    writer.codec.dev22 = true;
                    reader.codec.dev22 = true;
                    writer.probe_dev22().await?;
                    continue;
                }
                if let Some(dps) = msg.dps() {
                    let update = dps.clone();
                    let latency = pending.take().map(|(sent, _)| sent.elapsed().as_millis() as u64);
                    let changes: Vec<(String, Value)> = {
                        let s = dev.state.read().unwrap();
                        update.iter().filter(|(k, v)| s.dps.get(*k).is_some_and(|old| old != *v)).map(|(k, v)| (k.clone(), v.clone())).collect()
                    };
                    // Premier état complet après une perte de liaison : ce qui a changé pendant.
                    let (switch_dp, countdown_dp) = {
                        let cfg = dev.cfg.read().unwrap();
                        let mut merged = dev.state.read().unwrap().dps.clone();
                        merged.extend(update.clone());
                        let kind = kinds::detect(cfg.kind.as_deref(), &merged);
                        let sw = match &kind {
                            Kind::Switch { gangs } => gangs.first().cloned(),
                            k => k.switch_dp(&merged),
                        };
                        (sw, kinds::countdown_dp(&kind, &merged, &cfg.schema))
                    };
                    let outage = hub.take_reconnected(dev, &update, switch_dp.as_deref());
                    hub.log_device_changes(dev, &changes, outage.map(|o| o.0));
                    if let Some(o) = outage {
                        let switched = changes.iter().any(|(k, _)| Some(k.as_str()) == switch_dp.as_deref());
                        hub.check_restart(dev, o, &update, countdown_dp.as_deref(), switched);
                    }
                    {
                        let mut s = dev.state.write().unwrap();
                        for (k, v) in &update {
                            s.dps.insert(k.clone(), v.clone());
                        }
                        s.updated = now_ms();
                        if latency.is_some() {
                            s.latency_ms = latency;
                        }
                    }
                    hub.after_update(dev, &mut writer, &update).await?;
                    hub.emit_device(dev);
                } else if let Some(rc) = msg.retcode.filter(|&rc| rc != 0) {
                    tracing::warn!("{} : code retour {rc} (cmd {}) {}", t.id, msg.cmd, msg.text);
                }
            }
            cmd = rx.recv() => match cmd {
                None | Some(Command::Shutdown) => return Ok(()),
                Some(Command::Set(dps, reply)) => {
                    match writer.control(&dps).await {
                        Ok(()) => {
                            let _ = reply.send(Ok(()));
                            let now = Instant::now();
                            pending = Some((now, now + CONFIRM_DELAY));
                        }
                        Err(e) => {
                            let _ = reply.send(Err(anyhow!("{e:#}")));
                            return Err(e);
                        }
                    }
                }
                Some(Command::Refresh) => {
                    writer.query_all(&hint()).await?;
                    hub.after_update(dev, &mut writer, &Map::new()).await?;
                }
            },
            _ = hb.tick() => {
                if last_rx.elapsed() > SILENCE_LIMIT {
                    bail!("l'appareil ne répond plus");
                }
                writer.heartbeat().await?;
                // Surveillance continue : durée maximale, plage interdite.
                hub.after_update(dev, &mut writer, &Map::new()).await?;
            }
            _ = meter => {
                last_meter = Instant::now();
                writer.update_dps(&metering).await?;
            }
            _ = confirm => {
                // Pas de confirmation spontanée : on relit l'état.
                pending = None;
                writer.query_all(&hint()).await?;
            }
        }
    }
}
