//! Vision par IA dans le hub : un fil d'analyse par caméra activée.
//!
//! Le fil garde le flux de la caméra ouvert (`camera_vision_want`), prend la dernière image à
//! la cadence choisie, la passe dans la chaîne d'analyse (`vision::pipeline`) et transforme les
//! événements en entrées du journal (source « Vision IA »). L'interface lit l'état de chaque
//! analyse (personnes vues, noms, temps de calcul) pour dessiner par-dessus la vidéo.
//!
//! Visages : la galerie (`faces.json`, à côté de la configuration) ne se remplit que sur demande
//! explicite (« Enregistrer ce visage ») ; les empreintes des inconnus ne sont jamais gardées.
//! Elle est chiffrée sur le disque (une ancienne galerie en clair l'est à sa première lecture) ;
//! illisible, elle n'est jamais écrasée.

use super::{Event, Hub, Level, Source};
use crate::camera::{Frame, Quality};
use crate::kinds;
use crate::automation::Trigger;
use crate::vision::actions::{Action, ActionEngine, Rules};
use crate::vision::autotrack::{AutoTrackEvent, AutoTrackSettings, AutoTracker, PtzCommand};
use crate::vision::engine::Accel;
use crate::vision::faces::{FaceGallery, Stored, Verdict};
use crate::vision::models;
use crate::vision::objects::{ObjectChange, ObjectKind, ObjectView};
use crate::vision::onnx::{RtmoPersons, SfaceEmbedder, YoloxObjects, YunetFaces};
use crate::vision::pipeline::{FaceDetector, FaceEmbedder, PersonCheck, PersonView, Pipeline, VisionEvent};
use crate::vision::violence::ViolenceEvent;
use crate::vision::{CameraVision, VisionSettings};
use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// État de l'analyse d'une caméra, pour l'affichage.
#[derive(Clone, Debug, Default)]
pub struct Analysis {
    /// Modèles en cours de chargement.
    pub starting: bool,
    pub running: bool,
    pub error: Option<String>,
    /// Ce qui fait tourner les modèles (« carte graphique (DirectML) », « processeur »).
    pub backend: String,
    pub persons: Vec<PersonView>,
    /// Animaux et véhicules en vue.
    pub objects: Vec<ObjectView>,
    /// Les animaux / véhicules sont demandés mais leur modèle n'est pas installé.
    pub objects_missing: bool,
    /// Taille de l'image analysée.
    pub frame_size: (usize, usize),
    /// Analyses par seconde réellement faites, et durée moyenne d'une analyse (ms).
    pub rate: f32,
    pub ms: f32,
    pub updated: Option<Instant>,
}

/// Installation des modèles.
#[derive(Clone, Debug, Default)]
pub struct ModelInstall {
    pub running: bool,
    /// Étape en cours (« Téléchargement de sface… 40 % »).
    pub step: String,
    pub error: Option<String>,
}

/// Vue d'ensemble pour l'interface.
#[derive(Clone, Debug)]
pub struct VisionStatus {
    pub models_ready: bool,
    /// Mégaoctets encore à télécharger.
    pub missing_mb: f64,
    pub install: ModelInstall,
    pub accel: Accel,
    pub person_model: crate::vision::PersonModel,
}

/// Une personne connue (sans ses empreintes).
#[derive(Clone, Debug, PartialEq)]
pub struct KnownFaceView {
    pub id: String,
    pub name: String,
    /// Nombre de vues enregistrées.
    pub samples: usize,
}

struct Worker {
    stop: Arc<AtomicBool>,
}

#[derive(Default)]
pub(super) struct VisionState {
    workers: HashMap<String, Worker>,
    analysis: HashMap<String, Analysis>,
    /// Écoute du micro, par caméra.
    pub(super) listening: HashMap<String, super::Listening>,
    /// Présence par pièce (minutes par heure), chargée à la première mesure.
    presence: Option<crate::vision::presence::PresenceStore>,
    install: ModelInstall,
    gallery: Option<Arc<RwLock<FaceGallery>>>,
    /// Galerie illisible au chargement : laissée intacte sur le disque, rien n'y est écrit.
    gallery_error: Option<String>,
    /// Avertissement « galerie en clair » déjà écrit dans le journal.
    clear_warned: bool,
}

fn models_ready(faces: bool, persons: crate::vision::PersonModel) -> bool {
    models::persons_model(persons).is_installed() && (!faces || (models::YUNET.is_installed() && models::SFACE.is_installed()))
}

/// Début de phrase.
fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Ce qui change dans les zones d'une caméra, en clair pour le journal. L'interface change une
/// chose à la fois : à nombre égal, les zones se comparent rang par rang (renommage, délai, tracé).
fn zone_changes(before: &[crate::vision::actions::Zone], after: &[crate::vision::actions::Zone]) -> Vec<String> {
    let linger = |secs: u32| match secs {
        0 => "sans alerte de présence prolongée".to_string(),
        s => format!("présence prolongée signalée après {}", kinds::fmt_secs(s as u64)),
    };
    if before.len() != after.len() {
        let added = after.iter().filter(|a| !before.iter().any(|b| b.name == a.name)).map(|a| format!("zone « {} » ajoutée ({})", a.name, linger(a.linger_secs)));
        let removed = before.iter().filter(|b| !after.iter().any(|a| a.name == b.name)).map(|b| format!("zone « {} » supprimée", b.name));
        return added.chain(removed).collect();
    }
    let mut out = Vec::new();
    for (b, a) in before.iter().zip(after) {
        if b.name != a.name {
            out.push(format!("zone « {} » renommée « {} »", b.name, a.name));
        }
        if b.linger_secs != a.linger_secs {
            out.push(format!("zone « {} » : {}", a.name, linger(a.linger_secs)));
        }
        if b.area != a.area {
            out.push(format!("zone « {} » redessinée", a.name));
        }
    }
    out
}

impl Hub {
    fn faces_path(&self) -> std::path::PathBuf {
        self.config_path.with_file_name("faces.json")
    }

    /// Galerie des visages (chargée à la première demande). Illisible, elle est vide en mémoire
    /// et laissée intacte sur le disque (`writable_gallery` refuse alors d'y écrire).
    fn gallery(&self) -> Arc<RwLock<FaceGallery>> {
        if let Some(g) = &self.analysis.lock().unwrap().gallery {
            return g.clone();
        }
        // Un chargement à la fois, hors du verrou de l'état : le trousseau peut demander son mot de passe.
        static LOADING: Mutex<()> = Mutex::new(());
        let _one = LOADING.lock().unwrap();
        if let Some(g) = &self.analysis.lock().unwrap().gallery {
            return g.clone();
        }
        let (g, error) = match self.load_gallery() {
            Ok(g) => (g, None),
            Err(e) => {
                let e = format!("{e:#}");
                self.log(Level::Warning, Source::System, "", format!("Galerie des visages illisible ({e}) : personne ne sera reconnu, le fichier est laissé intact"));
                (FaceGallery::default(), Some(e))
            }
        };
        let mut v = self.analysis.lock().unwrap();
        v.gallery_error = error;
        v.gallery.insert(Arc::new(RwLock::new(g))).clone()
    }

    /// Lit la galerie ; une galerie en clair (ancien format) est aussitôt réécrite chiffrée.
    fn load_gallery(&self) -> Result<FaceGallery> {
        let path = self.faces_path();
        let (g, stored) = FaceGallery::load(&path)?;
        if stored == Stored::Clear {
            match g.save(&path) {
                Ok(Stored::Sealed(p)) => self.log(Level::Info, Source::System, "", format!("Galerie des visages chiffrée ({}) : l'ancien fichier en clair est remplacé", p.describe())),
                Ok(_) => self.warn_clear_gallery(),
                Err(e) => self.log(Level::Warning, Source::System, "", format!("Galerie des visages restée en clair : chiffrement impossible ({e:#})")),
            }
        }
        Ok(g)
    }

    /// Une fois par lancement : rien ne permet de chiffrer la galerie sur ce système.
    fn warn_clear_gallery(&self) {
        if !std::mem::replace(&mut self.analysis.lock().unwrap().clear_warned, true) {
            self.log(
                Level::Warning,
                Source::System,
                "",
                "Galerie des visages enregistrée en clair : aucun trousseau (Secret Service) pour la chiffrer ; seul ton compte peut la lire",
            );
        }
    }

    /// Galerie à modifier. Tant que le fichier est illisible, on réessaie de le lire (trousseau
    /// déverrouillé entre-temps…) et, à défaut, on refuse : l'écrire l'écraserait.
    fn writable_gallery(&self) -> Result<Arc<RwLock<FaceGallery>>> {
        let g = self.gallery();
        if self.analysis.lock().unwrap().gallery_error.is_some() {
            let fresh = self.load_gallery().map_err(|e| {
                anyhow!("galerie des visages illisible ({e:#}) ; elle est laissée intacte. Pour repartir de zéro, supprime {}", self.faces_path().display())
            })?;
            *g.write().unwrap() = fresh;
            self.analysis.lock().unwrap().gallery_error = None;
            self.log(Level::Info, Source::System, "", "Galerie des visages relue : la reconnaissance reprend");
        }
        Ok(g)
    }

    /// File des commandes d'orientation du suivi automatique, exécutées dans l'ordre (un arrêt
    /// ne doit jamais passer avant le mouvement qu'il arrête).
    fn ptz_sender(self: &Arc<Self>, id: &str) -> Option<tokio::sync::mpsc::UnboundedSender<PtzCommand>> {
        let rt = self.scene_runtime()?;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PtzCommand>();
        let (hub, id) = (self.clone(), id.to_string());
        rt.spawn(async move {
            while let Some(cmd) = rx.recv().await {
                let r = match cmd {
                    PtzCommand::Move { pan, tilt } => hub.camera_move(&id, pan, tilt, Source::Vision).await,
                    PtzCommand::Stop => hub.camera_stop(&id, Source::Vision).await,
                };
                if let Err(e) = r {
                    tracing::warn!("suivi automatique : {e:#}");
                }
            }
        });
        Some(tx)
    }

    /// Mode absence activé ?
    pub fn vision_armed(&self) -> bool {
        self.config.lock().unwrap().vision.armed
    }

    /// Active ou désactive le mode absence (toutes les caméras analysées).
    pub fn set_vision_armed(self: &Arc<Self>, armed: bool, source: Source) {
        {
            let mut cfg = self.config.lock().unwrap();
            if cfg.vision.armed == armed {
                return;
            }
            cfg.vision.armed = armed;
        }
        self.persist();
        let analysed = self.vision_settings().cameras.values().filter(|c| c.enabled).count();
        let msg = if armed {
            format!("Mode absence activé : toute personne vue est une alerte, avec photo ({analysed} caméra(s) analysée(s))")
        } else {
            "Mode absence désactivé".to_string()
        };
        self.log(if armed && analysed == 0 { Level::Warning } else { Level::Info }, source, "", msg);
        self.emit(Event::Cameras);
        // Scénarios de la maison entière (« mode absence activé → nettoyer »).
        self.scene_away(armed);
    }

    pub fn vision_settings(&self) -> VisionSettings {
        self.config.lock().unwrap().vision.clone()
    }

    pub fn camera_vision(&self, id: &str) -> CameraVision {
        self.config.lock().unwrap().vision.camera(id)
    }

    pub fn vision_status(&self) -> VisionStatus {
        let pm = self.config.lock().unwrap().vision.person_model;
        let missing: u64 = models::vision_models(pm).iter().filter(|m| !m.is_installed()).map(|m| m.size).sum();
        VisionStatus {
            models_ready: models_ready(true, pm),
            person_model: pm,
            missing_mb: missing as f64 / 1e6,
            install: self.analysis.lock().unwrap().install.clone(),
            accel: self.config.lock().unwrap().vision.accel,
        }
    }

    fn with_presence<T>(&self, f: impl FnOnce(&mut crate::vision::presence::PresenceStore) -> T) -> T {
        let mut v = self.analysis.lock().unwrap();
        let p = v.presence.get_or_insert_with(|| crate::vision::presence::PresenceStore::load(self.config_path.with_file_name("presence.json")));
        f(p)
    }

    /// Minutes de présence vues par une caméra, heure par heure, un jour donné.
    pub fn presence_day(&self, camera: &str, date: chrono::NaiveDate) -> [f32; 24] {
        self.with_presence(|p| p.day(camera, date))
    }

    pub(super) fn save_presence(&self) {
        if let Err(e) = self.with_presence(|p| p.save_if_dirty()) {
            tracing::error!("sauvegarde de la présence impossible : {e:#}");
        }
    }

    pub(super) fn forget_presence(&self, camera: &str) {
        self.with_presence(|p| p.forget(camera));
    }

    pub fn camera_analysis(&self, id: &str) -> Option<Analysis> {
        self.analysis.lock().unwrap().analysis.get(id).cloned()
    }

    /// Lance l'analyse des caméras activées (au démarrage du hub).
    pub(super) fn start_vision(self: &Arc<Self>) {
        let settings = self.vision_settings();
        for (id, cv) in settings.cameras.iter().filter(|(_, c)| c.enabled) {
            if self.camera_name(id).is_some() {
                self.start_worker(id, cv.clone(), settings.accel);
            }
        }
    }

    /// Change les réglages d'analyse d'une caméra (et relance son analyse).
    pub fn set_camera_vision(self: &Arc<Self>, id: &str, v: CameraVision, source: Source) -> Result<()> {
        if self.camera_name(id).is_none() {
            bail!("caméra inconnue");
        }
        let before = self.camera_vision(id);
        if before == v {
            return Ok(());
        }
        if before.zones != v.zones {
            if let Some(p) = crate::vision::actions::zones_problem(&v.zones) {
                bail!("{p}");
            }
        }
        let accel = {
            let mut cfg = self.config.lock().unwrap();
            cfg.vision.cameras.insert(id.to_string(), v.clone());
            cfg.vision.accel
        };
        self.persist();
        let mut changes = Vec::new();
        let onoff = |b: bool| if b { "activée" } else { "désactivée" };
        if before.enabled != v.enabled {
            changes.push(format!("analyse {}", onoff(v.enabled)));
        }
        if before.faces != v.faces {
            changes.push(format!("reconnaissance des visages {}", onoff(v.faces)));
        }
        if before.falls != v.falls {
            changes.push(format!("détection de chute {}", onoff(v.falls)));
        }
        if before.hd != v.hd {
            changes.push(format!("flux analysé : {}", if v.hd { "HD" } else { "fluide" }));
        }
        if before.fps != v.fps {
            changes.push(format!("{} analyses par seconde", kinds::fr(v.fps as f64, 0)));
        }
        if before.animals != v.animals {
            changes.push(format!("détection des animaux {}", onoff(v.animals)));
        }
        if before.vehicles != v.vehicles {
            changes.push(format!("détection des véhicules {}", onoff(v.vehicles)));
        }
        if before.gestures != v.gestures {
            changes.push(format!("gestes {}", if v.gestures { "activés" } else { "désactivés" }));
        }
        if before.still_minutes != v.still_minutes {
            changes.push(match v.still_minutes {
                0 => "alerte d'immobilité désactivée".to_string(),
                m => format!("alerte d'immobilité après {m} min"),
            });
        }
        if before.snapshots != v.snapshots {
            changes.push(format!("photos des événements {}", if v.snapshots { "activées" } else { "désactivées" }));
        }
        if before.sounds != v.sounds {
            changes.push(format!("écoute des sons {}", onoff(v.sounds)));
        }
        if before.speech != v.speech {
            changes.push(format!("transcription de la parole {}", onoff(v.speech)));
        }
        if before.violence != v.violence {
            changes.push(format!("détection de la violence {}", onoff(v.violence)));
        }
        if before.follow != v.follow {
            changes.push(format!("suivi automatique de la personne {}", if v.follow { "activé" } else { "désactivé" }));
        }
        if before.follow_tilt != v.follow_tilt {
            changes.push(format!("suivi en hauteur {}", if v.follow_tilt { "activé" } else { "désactivé" }));
        }
        changes.extend(zone_changes(&before.zones, &v.zones));
        self.log(Level::Info, source, id, format!("Vision IA : {}", changes.join(", ")));
        self.stop_worker(id);
        if v.enabled {
            self.start_worker(id, v, accel);
        }
        self.emit(Event::Cameras);
        Ok(())
    }

    /// Modèle des personnes : standard ou précision élevée (installé au besoin ; relance les analyses).
    pub fn set_vision_person_model(self: &Arc<Self>, pm: crate::vision::PersonModel, source: Source) {
        let settings = {
            let mut cfg = self.config.lock().unwrap();
            if cfg.vision.person_model == pm {
                return;
            }
            cfg.vision.person_model = pm;
            cfg.vision.clone()
        };
        self.persist();
        let what = match pm {
            crate::vision::PersonModel::Standard => "standard (RTMO-s)",
            crate::vision::PersonModel::High => "élevée (RTMO-m, plus juste de nuit, deux fois plus de calcul)",
        };
        self.log(Level::Info, source, "", format!("Vision IA : précision des personnes {what}"));
        if !models::persons_model(pm).is_installed() {
            self.install_vision_models(Source::System);
        }
        for (id, cv) in settings.cameras.iter().filter(|(_, c)| c.enabled) {
            self.stop_worker(id);
            self.start_worker(id, cv.clone(), settings.accel);
        }
    }

    /// Carte graphique ou processeur (relance les analyses en cours).
    pub fn set_vision_accel(self: &Arc<Self>, accel: Accel, source: Source) {
        let settings = {
            let mut cfg = self.config.lock().unwrap();
            if cfg.vision.accel == accel {
                return;
            }
            cfg.vision.accel = accel;
            cfg.vision.clone()
        };
        self.persist();
        let what = match accel {
            Accel::Auto => "carte graphique si possible",
            Accel::Cpu => "processeur seulement",
        };
        self.log(Level::Info, source, "", format!("Vision IA : calcul sur {what}"));
        for (id, cv) in settings.cameras.iter().filter(|(_, c)| c.enabled) {
            self.stop_worker(id);
            self.start_worker(id, cv.clone(), accel);
        }
    }

    /// Arrête l'analyse d'une caméra (réglage, suppression de la caméra).
    pub(super) fn stop_worker(&self, id: &str) {
        let w = {
            let mut v = self.analysis.lock().unwrap();
            v.analysis.remove(id);
            v.listening.remove(id);
            v.workers.remove(id)
        };
        if let Some(w) = w {
            w.stop.store(true, Ordering::Relaxed);
            self.camera_vision_want(id, None);
        }
    }

    fn start_worker(self: &Arc<Self>, id: &str, cv: CameraVision, accel: Accel) {
        let stop = Arc::new(AtomicBool::new(false));
        {
            let mut v = self.analysis.lock().unwrap();
            v.workers.insert(id.to_string(), Worker { stop: stop.clone() });
            v.analysis.insert(id.to_string(), Analysis { starting: true, ..Analysis::default() });
        }
        self.start_listening(id, &cv, accel, stop.clone());
        let (hub, id, gallery) = (self.clone(), id.to_string(), self.gallery());
        let spawned = std::thread::Builder::new().name(format!("homesmart-vision-{id}")).spawn(move || worker(hub, id, cv, accel, stop, gallery));
        if let Err(e) = spawned {
            tracing::error!("fil d'analyse impossible : {e}");
        }
    }

    fn set_analysis(&self, id: &str, f: impl FnOnce(&mut Analysis)) {
        if let Some(a) = self.analysis.lock().unwrap().analysis.get_mut(id) {
            f(a);
        }
        self.emit(Event::Cameras);
    }

    /// Télécharge les modèles manquants (vérifiés), en tâche de fond ; relance les analyses
    /// qui les attendaient.
    pub fn install_vision_models(self: &Arc<Self>, source: Source) {
        let pm = self.config.lock().unwrap().vision.person_model;
        self.install_models(models::vision_models(pm).to_vec(), "d'IA", source);
    }

    /// Télécharge ceux de `list` qui manquent (un téléchargement à la fois pour tout HomeSmart).
    pub(super) fn install_models(self: &Arc<Self>, list: Vec<models::ModelSpec>, what: &str, source: Source) {
        {
            let mut v = self.analysis.lock().unwrap();
            if v.install.running {
                return;
            }
            v.install = ModelInstall { running: true, step: "Préparation…".into(), error: None };
        }
        let missing: Vec<models::ModelSpec> = list.iter().filter(|m| !m.is_installed()).copied().collect();
        let mb: f64 = missing.iter().map(|m| m.size as f64 / 1e6).sum();
        self.log(Level::Info, source, "", format!("Installation des modèles {what} ({:.0} Mo, sources officielles, empreintes vérifiées)", mb));
        self.emit(Event::Cameras);
        let hub = self.clone();
        std::thread::spawn(move || {
            let mut failed = None;
            for m in &missing {
                let step = |got: u64, total: u64| {
                    let pct = got * 100 / total.max(1);
                    let mut v = hub.analysis.lock().unwrap();
                    v.install.step = format!("Téléchargement de « {} » ({}) : {pct} %", m.role, m.license);
                    drop(v);
                    hub.emit(Event::Cameras);
                };
                if let Err(e) = models::ensure(m, &step) {
                    failed = Some(format!("{e:#}"));
                    break;
                }
            }
            {
                let mut v = hub.analysis.lock().unwrap();
                v.install = ModelInstall { running: false, step: String::new(), error: failed.clone() };
            }
            match failed {
                Some(e) => hub.log(Level::Warning, Source::System, "", format!("Installation des modèles d'IA impossible : {e}")),
                None => {
                    hub.log(Level::Info, Source::System, "", "Modèles installés et vérifiés");
                    // Les analyses (image ou écoute) arrêtées faute de modèles repartent.
                    let settings = hub.vision_settings();
                    for (id, cv) in settings.cameras.iter().filter(|(_, c)| c.enabled) {
                        let failed_before = hub.camera_analysis(id).is_some_and(|a| a.error.is_some()) || hub.camera_listening(id).is_some_and(|l| l.error.is_some() && !l.running);
                        if failed_before {
                            hub.stop_worker(id);
                            hub.start_worker(id, cv.clone(), settings.accel);
                        }
                    }
                }
            }
            hub.emit(Event::Cameras);
        });
    }

    // ---- Visages connus ---------------------------------------------------------------------

    pub fn known_faces(&self) -> Vec<KnownFaceView> {
        let g = self.gallery();
        let g = g.read().unwrap();
        g.people.iter().map(|p| KnownFaceView { id: p.id.clone(), name: p.name.clone(), samples: p.embeddings.len() }).collect()
    }

    /// Modifie la galerie : la copie modifiée ne remplace l'actuelle qu'une fois enregistrée.
    fn update_gallery<T>(&self, f: impl FnOnce(&mut FaceGallery) -> Result<T>) -> Result<T> {
        let g = self.writable_gallery()?;
        let mut g = g.write().unwrap();
        let mut next = g.clone();
        let out = f(&mut next)?;
        if next.save(&self.faces_path())? == Stored::Clear {
            self.warn_clear_gallery();
        }
        *g = next;
        Ok(out)
    }

    /// Enregistre le visage de la personne suivie `track` sur la caméra `camera` sous le nom
    /// `name` (nouvelle personne, ou vue de plus d'une personne déjà connue).
    pub fn enroll_face(&self, camera: &str, track: u32, name: &str, source: Source) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            bail!("donne un nom à cette personne");
        }
        let emb = self
            .camera_analysis(camera)
            .and_then(|a| a.persons.into_iter().find(|p| p.track == track))
            .and_then(|p| p.embedding)
            .ok_or_else(|| anyhow!("pas encore de visage net pour cette personne : qu'elle regarde la caméra un instant"))?;
        let known = self.update_gallery(|g| {
            if g.model.is_empty() || g.people.is_empty() {
                g.model = models::SFACE.id.to_string();
            }
            let known = g.people.iter().any(|p| p.name.eq_ignore_ascii_case(name));
            g.enroll(name, &emb);
            Ok(known)
        })?;
        let msg = if known { format!("Nouvelle vue du visage de {name} enregistrée") } else { format!("Visage de {name} enregistré (reconnaissance en local)") };
        self.log(Level::Info, source, camera, msg);
        self.emit(Event::Cameras);
        Ok(())
    }

    /// Enregistre le visage d'une photo (JPEG ou PNG) sous `name` : une vue nette et de face
    /// améliore la reconnaissance. Les modèles ne sont chargés que le temps de l'opération ; la
    /// photo elle-même n'est pas gardée (seule son empreinte l'est).
    pub fn enroll_face_photo(&self, path: &std::path::Path, name: &str, source: Source) -> Result<()> {
        use crate::vision::geom::BBox;
        let name = name.trim();
        if name.is_empty() {
            bail!("donne un nom à cette personne");
        }
        if !(models::YUNET.is_installed() && models::SFACE.is_installed()) {
            bail!("modèles des visages non installés : installe les modèles d'IA d'abord");
        }
        let frame = crate::vision::picture::load(path)?;
        let accel = self.vision_settings().accel;
        let mut faces = YunetFaces::new(accel)?.detect(&frame, BBox::new(0.0, 0.0, 1.0, 1.0))?;
        faces.sort_by(|a, b| b.bbox.area().total_cmp(&a.bbox.area()));
        let face = faces.first().cloned().ok_or_else(|| anyhow!("aucun visage trouvé sur cette photo"))?;
        // Deux visages de taille voisine : impossible de savoir lequel est le bon.
        if faces.get(1).is_some_and(|f| f.bbox.area() > 0.5 * face.bbox.area()) {
            bail!("plusieurs visages sur cette photo : recadre-la sur une seule personne");
        }
        let px = face.bbox.width() * frame.width as f32;
        if px < 60.0 {
            bail!("visage trop petit sur cette photo ({px:.0} pixels de large) : prends une photo plus rapprochée");
        }
        let emb = SfaceEmbedder::new(accel)?.embed(&frame, &face)?;
        let known = self.update_gallery(|g| {
            if g.model.is_empty() || g.people.is_empty() {
                g.model = models::SFACE.id.to_string();
            }
            let known = g.people.iter().any(|p| p.name.eq_ignore_ascii_case(name));
            g.enroll(name, &emb);
            Ok(known)
        })?;
        let file = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let msg = if known { format!("Nouvelle vue du visage de {name} enregistrée depuis une photo ({file})") } else { format!("Visage de {name} enregistré depuis une photo ({file})") };
        self.log(Level::Info, source, "", msg);
        self.emit(Event::Cameras);
        Ok(())
    }

    pub fn rename_face(&self, id: &str, name: &str, source: Source) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            bail!("nom vide");
        }
        let old = self.update_gallery(|g| {
            let old = g.people.iter().find(|p| p.id == id).map(|p| p.name.clone()).ok_or_else(|| anyhow!("personne inconnue"))?;
            g.rename(id, name);
            Ok(old)
        })?;
        self.log(Level::Info, source, "", format!("Visage renommé : {old} → {name}"));
        self.emit(Event::Cameras);
        Ok(())
    }

    /// Supprime une personne et toutes ses empreintes.
    pub fn remove_face(&self, id: &str, source: Source) -> Result<()> {
        let p = self.update_gallery(|g| g.remove(id).ok_or_else(|| anyhow!("personne inconnue")))?;
        self.log(Level::Info, source, "", format!("Visage de {} supprimé (toutes ses empreintes)", p.name));
        self.emit(Event::Cameras);
        Ok(())
    }

    /// Derniers événements de l'analyse d'une caméra (le plus récent d'abord), pour l'interface.
    pub fn vision_events(&self, camera: &str, limit: usize) -> Vec<(chrono::DateTime<chrono::Local>, Level, String)> {
        self.journal_query(limit, |e| matches!(e.source, Source::Vision | Source::Listen) && e.device == camera && !e.message.starts_with("Analyse IA") && !e.message.starts_with("Écoute IA"))
            .into_iter()
            .map(|e| (e.at, e.level, e.message))
            .collect()
    }

    /// Événement d'analyse → journal. `photo` : nom de la photo prise à cet instant, s'il y en a une.
    fn vision_event(&self, camera: &str, e: &VisionEvent, names: &mut HashMap<u32, String>, photo: Option<&str>) {
        let who = |track: u32| names.get(&track).cloned().unwrap_or_else(|| "une personne".to_string());
        let (level, key, msg) = match e {
            VisionEvent::Violence(v) => {
                let both = format!("{} et {}", who(v.tracks.0), who(v.tracks.1));
                (Level::Critical, "violence", format!("Violence possible : coups répétés entre {both} ({} coups en quelques secondes)", v.strikes))
            }
            VisionEvent::Object { kind, change, .. } => {
                let what = if kind.is_animal() { "Animal" } else { "Véhicule" };
                match change {
                    ObjectChange::Appeared => (Level::Info, "objet", format!("{what} détecté : {}", kind.with_article())),
                    ObjectChange::Left { stayed } => (Level::Info, "objet", format!("{} n'est plus en vue (présence {})", cap(&kind.with_article()), kinds::fmt_secs(*stayed as u64))),
                }
            }
            VisionEvent::Identified { track, verdict, .. } => match verdict {
                Verdict::Person { name, similarity, .. } => {
                    names.insert(*track, name.clone());
                    (Level::Info, "id", format!("Reconnu : {name} ({:.0} %)", similarity * 100.0))
                }
                Verdict::Stranger => (Level::Warning, "id", "Visage inconnu".to_string()),
                Verdict::Undecided => return,
            },
            VisionEvent::Action(a) => {
                let w = who(a.track);
                match &a.action {
                    Action::Arrived if self.vision_armed() => (Level::Critical, "arrive", "Intrusion ? Personne détectée alors que le mode absence est activé".to_string()),
                    Action::Arrived => (Level::Info, "arrive", "Personne détectée".to_string()),
                    Action::Left { stayed } => {
                        names.remove(&a.track);
                        (Level::Info, "part", format!("{} n'est plus en vue (présence {})", cap(&w), kinds::fmt_secs(*stayed as u64)))
                    }
                    Action::EnteredZone(z) => (Level::Info, "zone", format!("{} entre dans « {z} »", cap(&w))),
                    Action::LeftZone(z) => (Level::Info, "zone", format!("{} quitte « {z} »", cap(&w))),
                    Action::Lingering { zone, secs } => (Level::Warning, "reste", format!("{} est dans « {zone} » depuis {}", cap(&w), kinds::fmt_secs(*secs as u64))),
                    Action::Fall => (Level::Critical, "chute", format!("Chute probable : {w}")),
                    Action::Down { secs } => (Level::Critical, "sol", format!("{} au sol depuis {}", cap(&w), kinds::fmt_secs(*secs as u64))),
                    Action::GotUp => (Level::Info, "debout", format!("{} est de nouveau debout", cap(&w))),
                    Action::HandRaised => (Level::Info, "main", format!("{} lève la main", cap(&w))),
                    Action::ArmsUp => (Level::Critical, "bras", format!("{} lève les deux bras : appel à l'aide ?", cap(&w))),
                    Action::Still { secs } => (Level::Warning, "immobile", format!("{} immobile depuis {}", cap(&w), kinds::fmt_secs(*secs as u64))),
                }
            }
        };
        let track = match e {
            VisionEvent::Identified { track, .. } => format!("{track}"),
            VisionEvent::Action(a) => format!("{}", a.track),
            VisionEvent::Object { kind, track, .. } => format!("{kind:?}{track}"),
            VisionEvent::Violence(v) => format!("{}-{}", v.tracks.0, v.tracks.1),
        };
        let msg = match photo {
            Some(p) => format!("{msg} — photo : {p}"),
            None => msg,
        };
        self.log_keyed(level, Source::Vision, camera, &format!("vision:{camera}:{track}:{key}"), msg);
    }

    /// Coups répétés, mais des rires entendus autour : une chamaillerie, au journal sans alerte.
    fn violence_play(&self, camera: &str, v: &ViolenceEvent, names: &HashMap<u32, String>) {
        let who = |track: u32| names.get(&track).cloned().unwrap_or_else(|| "une personne".to_string());
        let msg = format!("Chamaillerie probable entre {} et {} : coups répétés, mais des rires entendus (pas d'alerte)", who(v.tracks.0), who(v.tracks.1));
        self.log_keyed(Level::Info, Source::Vision, camera, &format!("vision:{camera}:{}-{}:jeu", v.tracks.0, v.tracks.1), msg);
    }

    /// Photo d'un événement (PNG, dossier des captures / Vision), écrite en arrière-plan.
    /// Renvoie le nom du fichier.
    pub(super) fn event_photo(&self, camera: &str, frame: &Arc<Frame>, what: &str) -> String {
        let name = format!("{} {} {what}.png", self.camera_name(camera).unwrap_or_default(), chrono::Local::now().format("%Y-%m-%d %Hh%M %Ss"));
        let clean: String = name.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c }).collect();
        let path = crate::camera::captures_dir().join("Vision").join(&clean);
        let frame = frame.clone();
        std::thread::spawn(move || {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = crate::camera::save_png(&frame, &path) {
                tracing::warn!("photo d'événement impossible : {e:#}");
            }
        });
        clean
    }
}

/// Violence vue alors que l'écoute des sons tourne : décidée un peu plus tard, selon ce qu'on entend.
struct PendingViolence {
    at: Instant,
    event: ViolenceEvent,
    frame: Arc<Frame>,
}

/// Rires entendus jusqu'à ce délai avant les coups…
const LAUGH_BEFORE: Duration = Duration::from_secs(10);
/// … ou pendant ce délai après : la violence attend ce temps avant d'être signalée.
const LAUGH_AFTER: Duration = Duration::from_secs(4);

/// Violences en attente : des rires entendus autour (sans cri ni pleurs) en font une chamaillerie,
/// au journal sans alerte ; sinon, passé `LAUGH_AFTER` (ou tout de suite si `now`), l'alerte
/// habituelle (photo, scénario).
fn settle_violence(hub: &Arc<Hub>, id: &str, cv: &CameraVision, pending: &mut Vec<PendingViolence>, names: &mut HashMap<u32, String>, now: bool) {
    if pending.is_empty() {
        return;
    }
    let listening = hub.camera_listening(id);
    pending.retain(|p| {
        let since = p.at.checked_sub(LAUGH_BEFORE).unwrap_or(p.at);
        let heard = |t: Option<Instant>| t.is_some_and(|t| t >= since);
        let (laughed, distress) = listening.as_ref().map_or((false, false), |l| (heard(l.laughed), heard(l.distress)));
        if laughed && !distress {
            hub.violence_play(id, &p.event, names);
            return false;
        }
        if !now && p.at.elapsed() < LAUGH_AFTER {
            return true;
        }
        let photo = cv.snapshots.then(|| hub.event_photo(id, &p.frame, "alerte"));
        hub.vision_event(id, &VisionEvent::Violence(p.event.clone()), names, photo.as_deref());
        hub.scene_trigger(id, Trigger::Violence);
        false
    });
}

/// Fil d'analyse d'une caméra.
fn worker(hub: Arc<Hub>, id: String, cv: CameraVision, accel: Accel, stop: Arc<AtomicBool>, gallery: Arc<RwLock<FaceGallery>>) {
    let pm = hub.vision_settings().person_model;
    if !models_ready(cv.faces, pm) {
        hub.set_analysis(&id, |a| (a.starting, a.error) = (false, Some("modèles d'IA non installés".into())));
        return;
    }
    let loaded = (|| -> Result<(Pipeline, String)> {
        let persons = RtmoPersons::with_model(&models::persons_model(pm), accel)?;
        let backend = persons.backend.to_string();
        let faces = if cv.faces {
            let f: Box<dyn FaceDetector> = Box::new(YunetFaces::new(accel)?);
            let e: Box<dyn FaceEmbedder> = Box::new(SfaceEmbedder::new(accel)?);
            Some((f, e))
        } else {
            None
        };
        let rules = Rules { fall: cv.falls, gestures: cv.gestures, still_after: cv.still_minutes as f64 * 60.0, ..Rules::default() };
        let mut p = Pipeline::new(Box::new(persons), faces, ActionEngine::new(cv.zones.clone(), rules));
        if cv.violence {
            p = p.with_violence();
        }
        // Animaux et véhicules : leur propre modèle, deux analyses par seconde au plus.
        let kinds: Vec<ObjectKind> = [
            ObjectKind::Cat,
            ObjectKind::Dog,
            ObjectKind::Bird,
            ObjectKind::Horse,
            ObjectKind::Sheep,
            ObjectKind::Cow,
            ObjectKind::WildAnimal,
            ObjectKind::Bicycle,
            ObjectKind::Car,
            ObjectKind::Motorcycle,
            ObjectKind::Bus,
            ObjectKind::Truck,
        ]
        .into_iter()
        .filter(|k| if k.is_animal() { cv.animals } else { cv.vehicles })
        .collect();
        // Le modèle des objets sert aussi de deuxième avis sur les silhouettes douteuses (même
        // sans animaux ni véhicules demandés : il ne tourne alors qu'au besoin).
        if models::YOLOX_S.is_installed() {
            // Seuil d'ouverture inchangé (0,5) : à 0,45, 3 % de personnes en plus, mais de nuit
            // (COCO en vision nocturne simulée) deux fois plus d'images vides prises pour une présence.
            p = p.with_objects(Box::new(YoloxObjects::new(accel)?), kinds, 0.5_f64.max(1.0 / cv.fps.max(0.5) as f64)).with_person_check(PersonCheck::default());
        }
        Ok((p, backend))
    })();
    let objects_missing = (cv.animals || cv.vehicles) && !models::YOLOX_S.is_installed();
    let (mut pipeline, backend) = match loaded {
        Ok(p) => p,
        Err(e) => {
            let msg = format!("{e:#}");
            hub.log(Level::Warning, Source::Vision, &id, format!("Analyse IA impossible : {msg}"));
            hub.set_analysis(&id, |a| (a.starting, a.error) = (false, Some(msg)));
            return;
        }
    };
    if stop.load(Ordering::Relaxed) {
        return;
    }
    hub.camera_vision_want(&id, Some(if cv.hd { Quality::Main } else { Quality::Sub }));
    hub.log(Level::Info, Source::Vision, &id, format!("Analyse IA démarrée ({backend}, {} images/s)", kinds::fr(cv.fps as f64, 0)));
    hub.scene_watch(&id, true);
    {
        let b = backend.clone();
        hub.set_analysis(&id, |a| (a.starting, a.running, a.backend, a.objects_missing) = (false, true, b, objects_missing));
    }
    // Photos d'événements : pas plus d'une toutes les 5 s (sauf alertes graves).
    let mut last_photo: Option<Instant> = None;

    let period = Duration::from_secs_f32(1.0 / cv.fps.clamp(0.5, 30.0));
    let t0 = Instant::now();
    let mut last: Option<Arc<Frame>> = None;
    let mut names: HashMap<u32, String> = HashMap::new();
    let (mut window, mut count, mut busy) = (Instant::now(), 0u32, Duration::ZERO);
    let mut last_error: Option<String> = None;
    // Présence : temps écoulé depuis l'image analysée précédente, compté si quelqu'un est en vue.
    let mut last_analysed: Option<Instant> = None;
    // Suivi automatique (caméra motorisée) : commandes envoyées dans l'ordre par une tâche à part.
    // Orientation pas encore connue (ONVIF interrogé au démarrage) : on essaie quand même.
    let ptz_capable = hub.camera(&id).is_some_and(|c| c.ptz != Some(false));
    let mut pending: Vec<PendingViolence> = Vec::new();
    let mut follow = (cv.follow && ptz_capable).then(|| {
        hub.log(Level::Info, Source::Vision, &id, if cv.follow_tilt { "Suivi automatique activé (orientation et hauteur)" } else { "Suivi automatique activé (orientation)" });
        (
            AutoTracker::new(AutoTrackSettings { tilt: cv.follow_tilt, return_after: Some(10.0), ..AutoTrackSettings::default() }),
            hub.ptz_sender(&id),
            hub.camera_last_manual_move(&id),
        )
    });
    while !stop.load(Ordering::Relaxed) {
        let tick = Instant::now();
        let fresh = hub.camera_frame(&id).filter(|f| !last.as_ref().is_some_and(|l| Arc::ptr_eq(l, f)));
        if let Some(frame) = fresh {
            last = Some(frame.clone());
            let started = Instant::now();
            let result = {
                let g = gallery.read().unwrap();
                pipeline.process(t0.elapsed().as_secs_f64(), &frame, &g)
            };
            busy += started.elapsed();
            count += 1;
            match result {
                Ok((persons, mut events)) => {
                    // Violence avec l'écoute des sons en marche : on attend de savoir si des rires
                    // l'accompagnent (`settle_violence`).
                    if cv.sounds && hub.camera_listening(&id).is_some_and(|l| l.running && l.level.is_some()) {
                        events.retain(|e| match e {
                            VisionEvent::Violence(v) => {
                                pending.push(PendingViolence { at: Instant::now(), event: v.clone(), frame: frame.clone() });
                                false
                            }
                            _ => true,
                        });
                    }
                    // Une photo pour les événements marquants de cette image (si demandé).
                    let marquant = |e: &VisionEvent| match e {
                        VisionEvent::Action(a) => matches!(a.action, Action::Arrived | Action::Lingering { .. } | Action::Still { .. }) || a.action.is_alarm(),
                        VisionEvent::Identified { verdict, .. } => matches!(verdict, Verdict::Stranger),
                        VisionEvent::Object { change, .. } => matches!(change, ObjectChange::Appeared),
                        VisionEvent::Violence(_) => true,
                    };
                    // Mode absence : une arrivée est une alerte, toujours photographiée.
                    let intrusion = events.iter().any(|e| matches!(e, VisionEvent::Action(a) if a.action == Action::Arrived)) && hub.vision_armed();
                    let grave = intrusion || events.iter().any(|e| matches!(e, VisionEvent::Action(a) if a.action.is_alarm()) || matches!(e, VisionEvent::Violence(_)));
                    let photo = ((cv.snapshots || intrusion) && events.iter().any(marquant) && (grave || last_photo.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)))).then(|| {
                        last_photo = Some(Instant::now());
                        let what = if grave { "alerte" } else { "événement" };
                        hub.event_photo(&id, &frame, what)
                    });
                    for e in &events {
                        let p = photo.as_deref().filter(|_| marquant(e));
                        hub.vision_event(&id, e, &mut names, p);
                        let trigger = match e {
                            VisionEvent::Action(a) => match a.action {
                                Action::Arrived => Some(Trigger::Arrival),
                                Action::Fall => Some(Trigger::Fall),
                                Action::ArmsUp => Some(Trigger::HelpCall),
                                _ => None,
                            },
                            VisionEvent::Violence(_) => Some(Trigger::Violence),
                            _ => None,
                        };
                        if let Some(t) = trigger {
                            hub.scene_trigger(&id, t);
                        }
                        if intrusion && matches!(e, VisionEvent::Action(a) if a.action == Action::Arrived) {
                            hub.scene_trigger(&id, Trigger::Intrusion);
                        }
                    }
                    let gap = last_analysed.map_or(0.0, |t| t.elapsed().as_secs_f32().min(2.0));
                    last_analysed = Some(Instant::now());
                    if !persons.is_empty() {
                        hub.scene_seen(&id);
                        hub.with_presence(|p| p.add(&id, chrono::Local::now(), gap));
                    }
                    let objects = pipeline.objects().to_vec();
                    let rate = if window.elapsed() >= Duration::from_secs(2) {
                        let r = (count as f32 / window.elapsed().as_secs_f32(), busy.as_secs_f32() * 1000.0 / count.max(1) as f32);
                        (window, count, busy) = (Instant::now(), 0, Duration::ZERO);
                        Some(r)
                    } else {
                        None
                    };
                    let size = (frame.width, frame.height);
                    hub.set_analysis(&id, |a| {
                        (a.persons, a.objects, a.frame_size, a.updated, a.error) = (persons, objects, size, Some(Instant::now()), None);
                        if let Some((r, m)) = rate {
                            (a.rate, a.ms) = (r, m);
                        }
                    });
                    last_error = None;
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    if last_error.as_ref() != Some(&msg) {
                        hub.log(Level::Warning, Source::Vision, &id, format!("Analyse IA : {msg}"));
                        last_error = Some(msg.clone());
                    }
                    hub.set_analysis(&id, |a| a.error = Some(msg));
                }
            }
        }
        settle_violence(&hub, &id, &cv, &mut pending, &mut names, false);
        // Suivi automatique : la caméra tourne vers la personne suivie.
        if let Some((at, tx, manual)) = follow.as_mut() {
            let now = t0.elapsed().as_secs_f64();
            let m = hub.camera_last_manual_move(&id);
            if m.is_some() && m != *manual {
                *manual = m;
                at.manual(now);
            }
            if let (Some(cmd), Some(tx)) = (at.update(now, pipeline.tracker.tracks()), tx.as_ref()) {
                let _ = tx.send(cmd);
            }
            for e in at.take_events() {
                // Les mouvements eux-mêmes sont journalisés par `camera_move` (source « Vision IA »).
                if !matches!(e, AutoTrackEvent::Moving(_) | AutoTrackEvent::Moved { .. }) {
                    hub.log(Level::Info, Source::Vision, &id, format!("Suivi automatique : {}", e.describe()));
                }
            }
        }
        if let Some(wait) = period.checked_sub(tick.elapsed()) {
            std::thread::sleep(wait);
        }
    }
    if let Some((mut at, tx, _)) = follow.take() {
        if let (Some(cmd), Some(tx)) = (at.stop(t0.elapsed().as_secs_f64()), tx.as_ref()) {
            let _ = tx.send(cmd);
        }
    }
    // Analyse arrêtée pendant l'attente : rien ne se perd.
    settle_violence(&hub, &id, &cv, &mut pending, &mut names, true);
    hub.scene_watch(&id, false);
    hub.log(Level::Info, Source::Vision, &id, "Analyse IA arrêtée");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::{self, CameraConfig};
    use tokio::time::sleep;

    /// Coups répétés pendant que l'écoute tourne : des rires entendus en font une chamaillerie (au
    /// journal, sans alerte) ; avec un cri aussi, ou des rires trop anciens, l'alerte habituelle,
    /// une fois l'attente passée.
    #[test]
    fn laughter_turns_violence_into_play() {
        let dir = std::env::temp_dir().join(format!("homesmart-laughtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hub = Hub::new(dir.join("homesmart.json")).unwrap();
        let cv = CameraVision::default();
        let frame = Arc::new(Frame { seq: 0, width: 2, height: 2, rgba: vec![0; 16] });
        let fight = |a: u32, ago: u64| PendingViolence {
            at: Instant::now().checked_sub(Duration::from_secs(ago)).unwrap(),
            event: ViolenceEvent { tracks: (a, a + 1), at: 0.0, bbox: crate::vision::geom::BBox::new(0.1, 0.1, 0.5, 0.9), strikes: 4 },
            frame: frame.clone(),
        };
        let heard = |laughed: Option<u64>, distress: Option<u64>| {
            let ago = |s: Option<u64>| s.map(|s| Instant::now().checked_sub(Duration::from_secs(s)).unwrap());
            let l = super::super::listening::Listening { running: true, level: Some(-40.0), laughed: ago(laughed), distress: ago(distress), ..Default::default() };
            hub.analysis.lock().unwrap().listening.insert("cam".into(), l);
        };
        let count = |what: &str| hub.journal_query(50, |e| e.message.contains(what)).len();
        let mut names = HashMap::new();
        // Rires il y a 3 s : chamaillerie, sans attendre.
        heard(Some(3), None);
        let mut pending = vec![fight(1, 0)];
        settle_violence(&hub, "cam", &cv, &mut pending, &mut names, false);
        assert!(pending.is_empty());
        assert_eq!((count("Chamaillerie probable"), count("Violence possible")), (1, 0));
        // Rires et cri : on attend, puis l'alerte (ici forcée, comme à l'arrêt de l'analyse).
        heard(Some(0), Some(0));
        let mut pending = vec![fight(3, 0)];
        settle_violence(&hub, "cam", &cv, &mut pending, &mut names, false);
        assert_eq!(pending.len(), 1, "l'attente n'est pas finie");
        settle_violence(&hub, "cam", &cv, &mut pending, &mut names, true);
        assert!(pending.is_empty());
        assert_eq!(count("Violence possible"), 1);
        // Rires d'il y a 20 s : trop anciens, l'alerte part une fois l'attente passée.
        heard(Some(20), None);
        let mut pending = vec![fight(5, 5)];
        settle_violence(&hub, "cam", &cv, &mut pending, &mut names, false);
        assert!(pending.is_empty());
        assert_eq!((count("Chamaillerie probable"), count("Violence possible")), (1, 2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Analyse activée sur une caméra simulée : modèles chargés, flux ouvert pour l'analyse
    /// (journalisé « Système », pas « Toi »), analyses à la cadence voulue, puis arrêt propre.
    /// Sautée si les modèles ne sont pas installés (`homesmart-cli vision models --install`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn analysis_runs_and_stops() {
        if !models_ready(true, crate::vision::PersonModel::Standard) {
            eprintln!("modèles d'IA absents : test sauté");
            return;
        }
        let dir = std::env::temp_dir().join(format!("homesmart-visiontest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sim = camera::sim::start("CODE1234", 0).await.unwrap();
        let hub = Hub::new(dir.join("homesmart.json")).unwrap();
        hub.start(false);
        let mut cfg = CameraConfig::new("127.0.0.1");
        (cfg.rtsp_port, cfg.onvif_port, cfg.password) = (sim.rtsp_port, sim.onvif_port, "CODE1234".into());
        let v = hub.add_camera(cfg, Source::User).unwrap();

        let on = CameraVision { enabled: true, fps: 5.0, hd: false, ..CameraVision::default() };
        hub.set_camera_vision(&v.id, on.clone(), Source::User).unwrap();
        assert!(hub.config.lock().unwrap().vision.camera(&v.id).enabled, "réglage enregistré");
        // Quelques analyses, sans que l'interface ne demande le flux.
        let t0 = Instant::now();
        loop {
            let a = hub.camera_analysis(&v.id).unwrap_or_default();
            assert!(a.error.is_none(), "{:?}", a.error);
            if a.running && a.rate > 0.0 {
                assert!(a.rate > 2.0 && a.rate <= 6.0, "cadence {}", a.rate);
                assert_eq!(a.frame_size, (640, 360), "flux fluide analysé");
                assert!(a.persons.is_empty(), "personne dans la pièce simulée");
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(30), "analyse jamais lancée : {a:?}");
            sleep(Duration::from_millis(200)).await;
        }
        hub.set_camera_vision(&v.id, CameraVision { enabled: false, ..on }, Source::User).unwrap();
        assert!(hub.camera_analysis(&v.id).is_none());
        sleep(Duration::from_millis(800)).await;

        let log = hub.alerts(60);
        let has = |src: Source, txt: &str| log.iter().any(|a| a.source == src && a.message.contains(txt));
        assert!(has(Source::User, "Vision IA : analyse activée"), "{log:#?}");
        assert!(has(Source::System, "Flux ouvert pour l'analyse IA"), "{log:#?}");
        assert!(has(Source::Vision, "Analyse IA démarrée"), "{log:#?}");
        assert!(has(Source::Vision, "Analyse IA arrêtée"), "{log:#?}");
        assert!(!log.iter().any(|a| a.message.contains("Vidéo en direct ouverte")), "l'analyse n'est pas l'utilisateur");
        hub.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Mode absence : une arrivée devient une alerte critique ; désarmé, simple information.
    #[tokio::test]
    async fn armed_mode_turns_arrivals_into_alerts() {
        use crate::vision::actions::ActionEvent;
        use crate::vision::geom::BBox;
        let dir = std::env::temp_dir().join(format!("homesmart-armed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hub = Hub::new(dir.join("homesmart.json")).unwrap();
        let cam = hub.add_camera(CameraConfig::new("192.0.2.20"), Source::User).unwrap();
        let arrived = VisionEvent::Action(ActionEvent { track: 1, action: Action::Arrived, at: 0.0, bbox: BBox::default() });
        let mut names = HashMap::new();
        hub.vision_event(&cam.id, &arrived, &mut names, None);
        hub.set_vision_armed(true, Source::User);
        assert!(hub.vision_armed() && hub.config.lock().unwrap().vision.armed);
        let again = VisionEvent::Action(ActionEvent { track: 2, ..match &arrived { VisionEvent::Action(a) => a.clone(), _ => unreachable!() } });
        hub.vision_event(&cam.id, &again, &mut names, Some("photo.png"));
        hub.set_vision_armed(false, Source::User);
        let log = hub.alerts(20);
        assert!(log.iter().any(|a| a.level == Level::Info && a.message == "Personne détectée"), "{log:#?}");
        assert!(log.iter().any(|a| a.level == Level::Critical && a.message.starts_with("Intrusion ?") && a.message.ends_with("photo : photo.png")), "{log:#?}");
        assert!(log.iter().any(|a| a.source == Source::User && a.message.starts_with("Mode absence activé")));
        assert!(log.iter().any(|a| a.source == Source::User && a.message == "Mode absence désactivé"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sentence_case() {
        assert_eq!(cap("une personne"), "Une personne");
        assert_eq!(cap("élodie"), "Élodie");
        assert_eq!(cap(""), "");
    }

    #[test]
    fn zone_changes_in_words() {
        use crate::vision::actions::Zone;
        use crate::vision::geom::Polygon;
        let zone = |name: &str, linger_secs: u32| Zone { name: name.into(), area: Polygon::full(), linger_secs };
        let porte = zone("Porte", 30);
        assert_eq!(zone_changes(&[], &[porte.clone()]), ["zone « Porte » ajoutée (présence prolongée signalée après 30 s)"]);
        assert_eq!(zone_changes(&[porte.clone(), zone("Lit", 0)], &[porte.clone()]), ["zone « Lit » supprimée"]);
        assert_eq!(zone_changes(&[porte.clone()], &[zone("Entrée", 0)]), ["zone « Porte » renommée « Entrée »", "zone « Entrée » : sans alerte de présence prolongée"]);
        let mut moved = porte.clone();
        moved.area.0.pop();
        assert_eq!(zone_changes(&[porte.clone()], &[moved]), ["zone « Porte » redessinée"]);
        assert!(zone_changes(&[porte.clone()], &[porte]).is_empty());
    }

    /// Des zones invalides (nom en double) sont refusées, sans rien enregistrer ni journaliser.
    #[tokio::test]
    async fn invalid_zones_are_refused() {
        use crate::vision::actions::Zone;
        use crate::vision::geom::Polygon;
        let dir = std::env::temp_dir().join(format!("homesmart-zonestest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hub = Hub::new(dir.join("homesmart.json")).unwrap();
        let cam = hub.add_camera(CameraConfig::new("192.0.2.10"), Source::User).unwrap();
        let zone = |name: &str| Zone { name: name.into(), area: Polygon::full(), linger_secs: 0 };
        let twice = CameraVision { zones: vec![zone("Porte"), zone("porte")], ..CameraVision::default() };
        let err = hub.set_camera_vision(&cam.id, twice, Source::User).unwrap_err();
        assert!(err.to_string().contains("deux zones s'appellent"), "{err}");
        assert!(hub.camera_vision(&cam.id).zones.is_empty());
        let one = CameraVision { zones: vec![zone("Porte")], ..CameraVision::default() };
        hub.set_camera_vision(&cam.id, one, Source::User).unwrap();
        assert_eq!(hub.camera_vision(&cam.id).zones.len(), 1);
        assert!(hub.alerts(10).iter().any(|a| a.source == Source::User && a.message.contains("Vision IA : zone « Porte » ajoutée")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
