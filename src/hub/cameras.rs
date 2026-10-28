//! Caméras dans le hub : une tâche par caméra.
//!
//! Le flux vidéo n'est ouvert que tant que l'interface le demande (vignette ou vue
//! affichée) : au repos, la tâche se contente de vérifier de temps en temps que la
//! caméra répond (simple connexion TCP, rien n'est décodé). Chaque action — ouverture
//! du direct, orientation, positions, captures, réglages, recherche — est journalisée.

use super::{Event, Hub, Level, Source};
use crate::camera::onvif::{DeviceInfo, EncoderConfig, EncoderOptions, ImagingOptions, ImagingSettings, Onvif, Preset};
use crate::camera::playback::{self, DayFootage};
use crate::camera::{self, discover, stream, Brand, CameraConfig, Frame, Quality, StreamStats};
use crate::kinds;
use crate::tplink;
use crate::vision::tamper::{TamperDetector, TamperEvent};
use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Local, NaiveDate, Utc};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};
use tokio::time::sleep;

/// Le flux reste ouvert tant que l'interface le redemande dans ce délai.
const WANT_TTL: Duration = Duration::from_millis(2500);
/// Vérification de présence au repos.
const PROBE_EVERY: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Après un verrouillage de la caméra (mots de passe erronés), délai avant un unique nouvel essai.
const LOCKOUT_WAIT: Duration = Duration::from_secs(600);
/// Mouvements d'orientation regroupés dans une même entrée du journal.
const PTZ_TRAIL: Duration = Duration::from_secs(3);

pub(super) enum CamCmd {
    Restart,
    Shutdown,
}

pub struct CameraHandle {
    cfg: RwLock<CameraConfig>,
    state: RwLock<CamState>,
    frame: Mutex<Option<Arc<Frame>>>,
    /// Dernière demande de chaque flux par l'interface : [fluide, HD].
    want: Mutex<[Option<Instant>; 2]>,
    /// Flux demandé en continu par l'analyse IA (sans délai d'expiration), en plus de l'interface.
    vision_want: Mutex<Option<Quality>>,
    wake: Notify,
    onvif: Mutex<Option<Arc<Onvif>>>,
    ptz_trail: Mutex<(Option<Instant>, Vec<&'static str>)>,
    /// Dernière orientation commandée par quelqu'un (pas par le suivi automatique).
    manual_move: Mutex<Option<Instant>>,
    /// La caméra refuse le mouvement continu ONVIF (certaines Tapo) : on la tourne par petits pas.
    relative_ptz: AtomicBool,
    /// Petits pas en cours, jusqu'à `camera_stop`.
    nudges: Mutex<Option<tokio::task::JoinHandle<()>>>,
    got_frame: AtomicBool,
    /// Son du direct : où envoyer les échantillons (l'interface quand on écoute).
    audio: RwLock<Option<stream::AudioSink>>,
    /// La session en cours a-t-elle la piste son ?
    session_audio: AtomicBool,
    /// Écoute par IA du micro (en plus, ou à la place, de l'écoute par l'utilisateur).
    listen_tap: RwLock<Option<stream::AudioSink>>,
    /// Relecture des enregistrements de la carte SD (lecture seule) : état et dernière image.
    playback: Mutex<PlaybackState>,
    playback_frame: Mutex<Option<Arc<Frame>>>,
    /// Son de la relecture : où l'envoyer (l'interface quand on écoute). Le son est toujours
    /// décodé avec l'image ; sans écoute, il est simplement jeté.
    playback_audio: RwLock<Option<stream::AudioSink>>,
    /// Surveillance du sabotage par l'image : garde le flux fluide ouvert tant qu'elle tourne.
    tamper_want: AtomicBool,
    /// Arrête la (les) tâche(s) de surveillance du sabotage en cours.
    tamper_stop: Mutex<Option<Arc<AtomicBool>>>,
    tx: mpsc::Sender<CamCmd>,
}

/// Relecture en cours (ou son dernier résultat) pour une caméra.
#[derive(Default)]
struct PlaybackState {
    /// Présent tant qu'une relecture tourne ; le passer à `true` l'arrête.
    cancel: Option<Arc<AtomicBool>>,
    start: Option<DateTime<Local>>,
    end: Option<DateTime<Local>>,
    /// Heure de la première image attendue (la position = origine + horloge de lecture).
    origin: Option<DateTime<Local>>,
    clock: Option<playback::Clock>,
    /// En pause : la dernière image reste affichée, la position est figée.
    paused: bool,
    frozen: Option<DateTime<Local>>,
    /// La séquence a-t-elle du son ? (`playback::SOUND_*`)
    sound: Arc<std::sync::atomic::AtomicU8>,
    /// Des images arrivent.
    playing: bool,
    /// Lecture arrivée au bout de la période.
    ended: bool,
    error: Option<String>,
    exporting: bool,
    export_cancel: Option<Arc<AtomicBool>>,
    /// Secondes déjà copiées (`f64` en bits) sur `export_total`.
    export_done_secs: Arc<std::sync::atomic::AtomicU64>,
    export_total: f64,
    export_range: Option<(DateTime<Local>, DateTime<Local>)>,
    export_done: Option<PathBuf>,
    export_error: Option<String>,
}

/// État de la relecture, pour l'affichage.
#[derive(Clone, Debug, Default)]
pub struct PlaybackView {
    /// Une relecture est en cours.
    pub active: bool,
    pub start: Option<DateTime<Local>>,
    pub end: Option<DateTime<Local>>,
    /// Heure de l'image affichée (horodatage d'enregistrement).
    pub position: Option<DateTime<Local>>,
    pub paused: bool,
    /// Son de la séquence : `Some(true)` présent, `Some(false)` absent, `None` pas encore connu.
    pub sound: Option<bool>,
    /// Le son de la relecture est écouté.
    pub listening: bool,
    pub playing: bool,
    pub ended: bool,
    pub error: Option<String>,
    pub exporting: bool,
    /// Avancement de l'export en cours, 0…1.
    pub export_progress: f32,
    pub export_range: Option<(DateTime<Local>, DateTime<Local>)>,
    pub export_done: Option<PathBuf>,
    pub export_error: Option<String>,
}

#[derive(Clone, Default)]
struct CamState {
    online: Option<bool>,
    streaming: Option<Quality>,
    stats: Option<StreamStats>,
    error: Option<String>,
    notice: Option<String>,
    /// Flux HD en H.265 et FFmpeg absent : on montre le flux fluide.
    main_unavailable: bool,
    info: Option<DeviceInfo>,
    ptz: Option<bool>,
    presets: Vec<Preset>,
    /// La caméra gère-t-elle les positions mémorisées / la position d'origine en ONVIF ?
    presets_supported: Option<bool>,
    home_supported: Option<bool>,
    onvif_error: Option<String>,
    /// Plus de nouvel essai automatique (identifiants refusés) : attendre « Réessayer ».
    waiting_retry: bool,
    /// Flux demandé mais en erreur depuis (détection des problèmes : flux vidéo perdu).
    failing_since: Option<Instant>,
    /// Injoignable depuis (sonde du port RTSP).
    offline_since: Option<Instant>,
}

/// État d'une caméra, pour l'affichage.
#[derive(Clone, Debug)]
pub struct CameraView {
    pub id: String,
    pub name: String,
    pub host: String,
    pub room: Option<String>,
    pub brand: Brand,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub mac: Option<String>,
    pub username: String,
    pub has_password: bool,
    pub rtsp_port: u16,
    pub onvif_port: u16,
    pub online: Option<bool>,
    pub streaming: Option<Quality>,
    pub stats: Option<StreamStats>,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub main_unavailable: bool,
    pub info: Option<DeviceInfo>,
    pub ptz: Option<bool>,
    pub presets: Vec<Preset>,
    pub presets_supported: Option<bool>,
    pub home_supported: Option<bool>,
    pub onvif_error: Option<String>,
    pub waiting_retry: bool,
}

/// Modification partielle d'une caméra (`None` = inchangé).
#[derive(Deserialize, Default, Clone)]
pub struct CameraPatch {
    pub name: Option<String>,
    pub room: Option<String>,
    pub host: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub brand: Option<Brand>,
    pub rtsp_port: Option<u16>,
    pub onvif_port: Option<u16>,
    pub main_path: Option<String>,
    pub sub_path: Option<String>,
    pub invert_tilt: Option<bool>,
    pub invert_pan: Option<bool>,
}

/// FFmpeg, nécessaire aux flux H.265 (flux HD des Imou) : présence et installation.
#[derive(Clone, Debug, Default)]
pub struct FfmpegStatus {
    /// La recherche a eu lieu (sinon les autres champs ne sont pas encore connus).
    pub checked: bool,
    /// Recherche en cours.
    pub searching: bool,
    pub path: Option<PathBuf>,
    pub version: Option<String>,
    /// Installation en un clic possible (Windows + winget).
    pub can_install: bool,
    /// Commande à lancer soi-même sinon.
    pub manual: String,
    pub installing: bool,
    /// Étape en cours (« Téléchargement… 42 % »).
    pub step: String,
    pub error: Option<String>,
}

/// Résultat d'un test de connexion.
#[derive(Clone, Debug, Default)]
pub struct TestReport {
    pub video: Option<StreamStats>,
    pub video_error: Option<String>,
    pub info: Option<DeviceInfo>,
    pub ptz: bool,
    pub onvif_error: Option<String>,
}

impl CameraHandle {
    fn name(&self) -> String {
        self.cfg.read().unwrap().display_name().to_string()
    }

    fn id(&self) -> String {
        self.cfg.read().unwrap().id.clone()
    }

    /// Flux à ouvrir : HD si demandé (récemment par l'interface, ou par l'analyse) et lisible,
    /// sinon fluide, sinon aucun.
    fn target(&self) -> Option<Quality> {
        let w = *self.want.lock().unwrap();
        let v = *self.vision_want.lock().unwrap();
        let tamper = self.tamper_want.load(Ordering::Relaxed);
        let fresh = |t: Option<Instant>| t.is_some_and(|t| t.elapsed() < WANT_TTL);
        let main_ok = !self.state.read().unwrap().main_unavailable;
        if (fresh(w[1]) || v == Some(Quality::Main)) && main_ok {
            Some(Quality::Main)
        } else if fresh(w[0]) || fresh(w[1]) || v.is_some() || tamper {
            Some(Quality::Sub)
        } else {
            None
        }
    }

    /// L'interface affiche-t-elle cette caméra (sinon le flux n'est ouvert que pour l'analyse) ?
    fn ui_wants(&self) -> bool {
        self.want.lock().unwrap().iter().any(|t| t.is_some_and(|t| t.elapsed() < WANT_TTL))
    }

    async fn until_target_changes(&self, q: Quality) {
        loop {
            sleep(Duration::from_millis(400)).await;
            if self.target() != Some(q) {
                return;
            }
        }
    }

    fn view(&self) -> CameraView {
        let c = self.cfg.read().unwrap();
        let s = self.state.read().unwrap();
        CameraView {
            id: c.id.clone(),
            name: c.display_name().to_string(),
            host: c.host.clone(),
            room: c.room.clone(),
            brand: c.brand,
            model: c.model.clone(),
            serial: c.serial.clone(),
            mac: c.mac.clone(),
            username: c.username.clone(),
            has_password: !c.password.is_empty(),
            rtsp_port: c.rtsp_port,
            onvif_port: c.onvif_port,
            online: s.online,
            streaming: s.streaming,
            stats: s.stats.clone(),
            error: s.error.clone(),
            notice: s.notice.clone(),
            main_unavailable: s.main_unavailable,
            info: s.info.clone(),
            ptz: s.ptz,
            presets: s.presets.clone(),
            presets_supported: s.presets_supported,
            home_supported: s.home_supported,
            onvif_error: s.onvif_error.clone(),
            waiting_retry: s.waiting_retry,
        }
    }
}

/// La caméra répond qu'elle ne gère pas cette commande ONVIF (facultative).
/// Petits pas d'une caméra sans mouvement continu : un toutes les `NUDGE_EVERY`, de `NUDGE`
/// (part de toute la course) à pleine vitesse, et jamais plus de `NUDGE_MAX` sans nouvel ordre.
const NUDGE: f32 = 0.05;
const NUDGE_EVERY: Duration = Duration::from_millis(300);
const NUDGE_MAX: Duration = Duration::from_secs(20);

/// Exécute une commande ONVIF (session gardée en cache, rouverte en cas d'erreur).
async fn onvif_call<R: Send + 'static>(h: &Arc<CameraHandle>, f: impl FnOnce(&Onvif) -> Result<R> + Send + 'static) -> Result<R> {
    let cached = h.onvif.lock().unwrap().clone();
    let cfg = h.cfg.read().unwrap().clone();
    let h2 = h.clone();
    tokio::task::spawn_blocking(move || {
        let o = match cached {
            Some(o) => o,
            None => {
                let o = Arc::new(Onvif::connect(&cfg.host, cfg.onvif_port, &cfg.username, &cfg.password)?);
                *h2.onvif.lock().unwrap() = Some(o.clone());
                o
            }
        };
        let r = f(&o);
        if r.is_err() {
            *h2.onvif.lock().unwrap() = None;
        }
        r
    })
    .await
    .map_err(|e| anyhow!("tâche ONVIF : {e}"))?
}

fn not_implemented(e: &anyhow::Error) -> bool {
    let t = format!("{e:#}");
    t.contains("NotImplemented") || t.contains("ActionNotSupported")
}

/// « à gauche », « vers le haut à droite »…
fn direction(pan: f32, tilt: f32) -> &'static str {
    match (pan.partial_cmp(&0.0), tilt.partial_cmp(&0.0)) {
        (Some(std::cmp::Ordering::Less), Some(std::cmp::Ordering::Greater)) => "haut-gauche",
        (Some(std::cmp::Ordering::Greater), Some(std::cmp::Ordering::Greater)) => "haut-droite",
        (Some(std::cmp::Ordering::Less), Some(std::cmp::Ordering::Less)) => "bas-gauche",
        (Some(std::cmp::Ordering::Greater), Some(std::cmp::Ordering::Less)) => "bas-droite",
        (Some(std::cmp::Ordering::Less), _) => "gauche",
        (Some(std::cmp::Ordering::Greater), _) => "droite",
        (_, Some(std::cmp::Ordering::Greater)) => "haut",
        (_, Some(std::cmp::Ordering::Less)) => "bas",
        _ => "immobile",
    }
}

impl Hub {
    pub(super) fn start_cameras(self: &Arc<Self>) {
        let list = self.config.lock().unwrap().cameras.clone();
        for cfg in list {
            self.spawn_camera(cfg);
        }
    }

    fn spawn_camera(self: &Arc<Self>, cfg: CameraConfig) -> Arc<CameraHandle> {
        let (tx, rx) = mpsc::channel(8);
        let id = cfg.id.clone();
        let h = Arc::new(CameraHandle {
            cfg: RwLock::new(cfg),
            state: RwLock::default(),
            frame: Mutex::default(),
            want: Mutex::default(),
            vision_want: Mutex::default(),
            wake: Notify::new(),
            onvif: Mutex::default(),
            ptz_trail: Mutex::default(),
            manual_move: Mutex::default(),
            relative_ptz: AtomicBool::new(false),
            nudges: Mutex::default(),
            got_frame: AtomicBool::new(false),
            audio: RwLock::default(),
            session_audio: AtomicBool::new(false),
            listen_tap: RwLock::default(),
            playback: Mutex::default(),
            playback_frame: Mutex::default(),
            playback_audio: RwLock::default(),
            tamper_want: AtomicBool::new(false),
            tamper_stop: Mutex::default(),
            tx,
        });
        if let Some(old) = self.cameras.write().unwrap().insert(id, h.clone()) {
            let _ = old.tx.try_send(CamCmd::Shutdown);
        }
        tokio::spawn(run_camera(self.clone(), h.clone(), rx));
        self.restart_tamper(&h);
        h
    }

    fn camera_handle(&self, id: &str) -> Result<Arc<CameraHandle>> {
        self.cameras.read().unwrap().get(id).cloned().ok_or_else(|| anyhow!("caméra inconnue"))
    }

    pub(super) fn camera_name(&self, id: &str) -> Option<String> {
        self.cameras.read().unwrap().get(id).map(|h| h.name())
    }

    fn persist_cameras(&self) {
        let list: Vec<CameraConfig> = {
            let cams = self.cameras.read().unwrap();
            let order: Vec<String> = self.config.lock().unwrap().cameras.iter().map(|c| c.id.clone()).collect();
            order.iter().filter_map(|id| cams.get(id)).map(|h| h.cfg.read().unwrap().clone()).collect()
        };
        self.config.lock().unwrap().cameras = list;
        self.persist();
    }

    // ---- Lecture ----------------------------------------------------------------------

    pub fn cameras(&self) -> Vec<CameraView> {
        let order: Vec<String> = self.config.lock().unwrap().cameras.iter().map(|c| c.id.clone()).collect();
        let cams = self.cameras.read().unwrap();
        order.iter().filter_map(|id| cams.get(id)).map(|h| h.view()).collect()
    }

    pub fn camera(&self, id: &str) -> Option<CameraView> {
        self.cameras.read().unwrap().get(id).map(|h| h.view())
    }

    pub fn camera_config(&self, id: &str) -> Option<CameraConfig> {
        self.cameras.read().unwrap().get(id).map(|h| h.cfg.read().unwrap().clone())
    }

    /// Fonction appelée à chaque nouvelle image (l'interface s'en sert pour se redessiner).
    pub fn set_frame_waker(&self, f: Arc<dyn Fn() + Send + Sync>) {
        *self.frame_waker.write().unwrap() = Some(f);
    }

    /// L'interface affiche cette caméra : garder (ou ouvrir) le flux demandé.
    pub fn camera_want(&self, id: &str, q: Quality) {
        let Some(h) = self.cameras.read().unwrap().get(id).cloned() else { return };
        h.want.lock().unwrap()[q as usize] = Some(Instant::now());
        if h.state.read().unwrap().streaming != h.target() {
            h.wake.notify_one();
        }
    }

    /// Flux gardé ouvert en continu pour l'analyse IA (`None` : plus besoin). L'interface peut
    /// toujours demander mieux (HD) ; la caméra n'a qu'un flux à la fois, le plus exigeant.
    pub fn camera_vision_want(&self, id: &str, q: Option<Quality>) {
        let Some(h) = self.cameras.read().unwrap().get(id).cloned() else { return };
        *h.vision_want.lock().unwrap() = q;
        if h.state.read().unwrap().streaming != h.target() {
            h.wake.notify_one();
        }
    }

    /// Dernière image décodée (la plus récente du direct, ou la dernière connue).
    /// Écoute (Some) ou coupe (None) le son du direct. La piste son n'est demandée à la caméra
    /// que pendant l'écoute ; l'activer rouvre le flux une fois si besoin.
    pub fn set_camera_audio(&self, id: &str, sink: Option<stream::AudioSink>, source: Source) -> Result<()> {
        let h = self.camera_handle(id)?;
        let on = sink.is_some();
        let was = h.audio.read().unwrap().is_some();
        *h.audio.write().unwrap() = sink;
        if on == was {
            return Ok(());
        }
        self.log(Level::Info, source, id, if on { "Son du direct activé (micro de la caméra)" } else { "Son du direct coupé" });
        let streaming = h.state.read().unwrap().streaming.is_some();
        if on && streaming && !h.session_audio.load(Ordering::Relaxed) {
            let _ = h.tx.try_send(CamCmd::Restart);
        }
        self.emit(Event::Cameras);
        Ok(())
    }

    /// Branche (Some) ou débranche (None) l'écoute par IA du micro. Comme pour l'écoute par
    /// l'utilisateur, la piste son n'est demandée qu'au besoin (flux rouvert une fois si nécessaire).
    pub fn camera_listen_tap(&self, id: &str, tap: Option<stream::AudioSink>) {
        let Ok(h) = self.camera_handle(id) else { return };
        let on = tap.is_some();
        *h.listen_tap.write().unwrap() = tap;
        let streaming = h.state.read().unwrap().streaming.is_some();
        if on && streaming && !h.session_audio.load(Ordering::Relaxed) {
            let _ = h.tx.try_send(CamCmd::Restart);
        }
    }

    /// Débranche l'écoute `tap` seulement si c'est toujours elle qui est branchée (une écoute
    /// relancée entre-temps garde la sienne).
    pub fn camera_listen_untap(&self, id: &str, tap: &stream::AudioSink) {
        let Ok(h) = self.camera_handle(id) else { return };
        let mut cur = h.listen_tap.write().unwrap();
        if cur.as_ref().is_some_and(|c| Arc::ptr_eq(c, tap)) {
            *cur = None;
        }
    }

    pub fn camera_frame(&self, id: &str) -> Option<Arc<Frame>> {
        self.cameras.read().unwrap().get(id).and_then(|h| h.frame.lock().unwrap().clone())
    }

    // ---- Relecture des enregistrements de la carte SD (lecture seule) ------------------------

    /// Dernière image de relecture (séparée du direct).
    pub fn camera_playback_frame(&self, id: &str) -> Option<Arc<Frame>> {
        self.cameras.read().unwrap().get(id).and_then(|h| h.playback_frame.lock().unwrap().clone())
    }

    /// État de la relecture, pour l'interface.
    pub fn camera_playback(&self, id: &str) -> Option<PlaybackView> {
        self.cameras.read().unwrap().get(id).map(|h| {
            let p = h.playback.lock().unwrap();
            let position = if p.paused {
                p.frozen
            } else {
                match (p.origin, p.clock.as_ref().and_then(playback::read_clock)) {
                    (Some(o), Some(secs)) => Some(o + chrono::Duration::milliseconds((secs * 1000.0) as i64)),
                    _ => None,
                }
            };
            let done = f64::from_bits(p.export_done_secs.load(Ordering::Relaxed));
            PlaybackView {
                active: p.cancel.is_some(),
                start: p.start,
                end: p.end,
                position,
                paused: p.paused,
                sound: match p.sound.load(Ordering::Relaxed) {
                    playback::SOUND_YES => Some(true),
                    playback::SOUND_NONE => Some(false),
                    _ => None,
                },
                listening: h.playback_audio.read().unwrap().is_some(),
                playing: p.playing,
                ended: p.ended,
                error: p.error.clone(),
                exporting: p.exporting,
                export_progress: if p.export_total > 0.0 && done.is_finite() { (done / p.export_total).clamp(0.0, 1.0) as f32 } else { 0.0 },
                export_range: p.export_range,
                export_done: p.export_done.clone(),
                export_error: p.export_error.clone(),
            }
        })
    }

    fn playback_config(&self, id: &str) -> Result<CameraConfig> {
        let cfg = self.camera_config(id).ok_or_else(|| anyhow!("caméra inconnue"))?;
        if cfg.password.is_empty() {
            bail!("code de sécurité manquant : renseigne-le dans les réglages de la caméra");
        }
        Ok(cfg)
    }

    /// Enregistrements d'une journée, à la seconde près ; `progress` reçoit les résultats
    /// intermédiaires. Lecture de la carte (non journalisée : c'est une consultation, pas une action).
    pub async fn camera_scan_day(&self, id: &str, date: NaiveDate, prev: Option<DayFootage>, progress: impl Fn(&DayFootage) + Send + Sync) -> Result<DayFootage> {
        let cfg = self.playback_config(id)?;
        Ok(playback::scan_day_detail(&cfg, date, prev.as_ref(), &progress).await)
    }

    /// Jours qui ont de la vidéo sur la carte (un sondage par jour). Lecture (non journalisée).
    pub async fn camera_recording_days(&self, id: &str, dates: Vec<NaiveDate>) -> Result<Vec<(NaiveDate, Option<bool>)>> {
        let cfg = self.playback_config(id)?;
        Ok(playback::scan_days(&cfg, &dates).await)
    }

    /// Vignette d'une séquence (première image à partir de `at`). Lecture (non journalisée).
    pub async fn camera_thumbnail(&self, id: &str, at: DateTime<Utc>, width: u32, height: u32) -> Result<Frame> {
        let cfg = self.playback_config(id)?;
        tokio::task::spawn_blocking(move || playback::thumbnail_blocking(&cfg, at, width, height)).await.map_err(|e| anyhow!("{e}"))?
    }

    /// Démarre la relecture d'une période (heures locales). Remplace une relecture précédente.
    /// `origin` : heure de la première image attendue (début de la période, ou début de la
    /// première séquence si la période commence dans un trou) ; elle cale l'horloge affichée.
    pub fn camera_play_recording(self: &Arc<Self>, id: &str, start: DateTime<Local>, end: DateTime<Local>, origin: DateTime<Local>, source: Source) -> Result<()> {
        let h = self.camera_handle(id)?;
        let cfg = self.playback_config(id)?;
        if end <= start {
            bail!("période invalide");
        }
        if let Some(c) = h.playback.lock().unwrap().cancel.take() {
            c.store(true, Ordering::Relaxed);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let clock = playback::new_clock();
        let sound = Arc::new(std::sync::atomic::AtomicU8::new(playback::SOUND_UNKNOWN));
        {
            // L'image précédente reste affichée (assombrie) jusqu'à la première de la nouvelle lecture.
            let mut p = h.playback.lock().unwrap();
            let keep = (p.exporting, p.export_cancel.take(), p.export_done_secs.clone(), p.export_total, p.export_range, p.export_done.take(), p.export_error.take());
            *p = PlaybackState { cancel: Some(cancel.clone()), start: Some(start), end: Some(end), origin: Some(origin), clock: Some(clock.clone()), sound: sound.clone(), ..PlaybackState::default() };
            (p.exporting, p.export_cancel, p.export_done_secs, p.export_total, p.export_range, p.export_done, p.export_error) = keep;
        }
        self.log(Level::Info, source, id, format!("Relecture ouverte ({} → {})", origin.format("%Y-%m-%d %H:%M:%S"), end.format("%H:%M")));
        self.emit(Event::Cameras);

        let (hub, h2, id) = (self.clone(), h.clone(), id.to_string());
        tokio::spawn(async move {
            let sink: stream::FrameSink = {
                let (h, hub) = (h2.clone(), hub.clone());
                Arc::new(move |f: Frame| {
                    *h.playback_frame.lock().unwrap() = Some(Arc::new(f));
                    if let Some(w) = hub.frame_waker.read().unwrap().clone() {
                        w();
                    }
                })
            };
            let on_open = {
                let (h, hub) = (h2.clone(), hub.clone());
                move || {
                    h.playback.lock().unwrap().playing = true;
                    hub.emit(Event::Cameras);
                }
            };
            let (su, eu, cancel2) = (start.with_timezone(&Utc), end.with_timezone(&Utc), cancel.clone());
            // Le son est toujours décodé (même connexion) ; il ne part vers la carte son que si
            // quelqu'un écoute, ce qui peut changer en cours de lecture.
            let audio: stream::AudioSink = {
                let h = h2.clone();
                Arc::new(move |s: &[f32], rate: u32| {
                    let out = h.playback_audio.read().unwrap().clone();
                    if let Some(out) = out {
                        out(s, rate);
                    }
                })
            };
            let opts = playback::ReplayOptions { clock: Some(clock), audio: Some(audio), sound: Some(sound) };
            let r = tokio::task::spawn_blocking(move || playback::decode_blocking(&cfg, su, eu, 1280, 720, cancel2, sink, on_open, opts)).await;
            // Finalisation, sauf si une nouvelle relecture (ou une pause) a déjà pris la place.
            let mut p = h2.playback.lock().unwrap();
            if p.cancel.as_ref().is_some_and(|c| Arc::ptr_eq(c, &cancel)) {
                (p.cancel, p.playing) = (None, false);
                let msg = match r {
                    Ok(Ok(())) => {
                        // L'horloge garde la position de la dernière image.
                        p.ended = true;
                        "Relecture terminée".to_string()
                    }
                    Ok(Err(e)) => {
                        let m = stream::describe_error(&e);
                        p.error = Some(m.clone());
                        format!("Relecture interrompue : {m}")
                    }
                    Err(e) => {
                        let m = format!("{e}");
                        p.error = Some(m.clone());
                        format!("Relecture interrompue : {m}")
                    }
                };
                drop(p);
                hub.log(Level::Info, Source::System, &id, msg);
                hub.emit(Event::Cameras);
            }
        });
        Ok(())
    }

    /// Arrête la relecture en cours et efface son état (retour au direct). Un export éventuel
    /// continue en arrière-plan.
    pub fn camera_stop_playback(&self, id: &str, source: Source) -> Result<()> {
        let h = self.camera_handle(id)?;
        let (was_active, had_state) = {
            let mut p = h.playback.lock().unwrap();
            let active = p.cancel.take();
            if let Some(c) = &active {
                c.store(true, Ordering::Relaxed);
            }
            let had_state = active.is_some() || p.ended || p.paused || p.error.is_some() || p.start.is_some();
            // On repart propre, en gardant l'export en cours / son résultat.
            let keep = (p.exporting, p.export_cancel.take(), p.export_done_secs.clone(), p.export_total, p.export_range, p.export_done.take(), p.export_error.take());
            *p = PlaybackState::default();
            (p.exporting, p.export_cancel, p.export_done_secs, p.export_total, p.export_range, p.export_done, p.export_error) = keep;
            (active.is_some(), had_state)
        };
        *h.playback_frame.lock().unwrap() = None;
        if was_active {
            self.log(Level::Info, source, id, "Relecture arrêtée");
        }
        if had_state {
            self.emit(Event::Cameras);
        }
        Ok(())
    }

    /// Met la relecture en pause : la dernière image reste affichée et la position est figée.
    /// La reprise relance la lecture à cette position (`camera_play_recording`).
    pub fn camera_pause_playback(&self, id: &str, source: Source) -> Result<Option<DateTime<Local>>> {
        let h = self.camera_handle(id)?;
        let at = {
            let mut p = h.playback.lock().unwrap();
            let Some(c) = p.cancel.take() else { return Ok(None) };
            c.store(true, Ordering::Relaxed);
            let at = match (p.origin, p.clock.as_ref().and_then(playback::read_clock)) {
                (Some(o), Some(s)) => Some(o + chrono::Duration::milliseconds((s * 1000.0) as i64)),
                _ => p.origin,
            };
            (p.paused, p.frozen, p.playing) = (true, at, false);
            at
        };
        let when = at.map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_default();
        self.log(Level::Info, source, id, format!("Relecture en pause ({when})"));
        self.emit(Event::Cameras);
        Ok(at)
    }

    /// Écoute (Some) ou coupe (None) le son de la relecture. Il est décodé avec l'image, sur la
    /// même connexion : l'activer en cours de lecture ne relance rien.
    pub fn set_playback_audio(&self, id: &str, sink: Option<stream::AudioSink>, source: Source) -> Result<()> {
        let h = self.camera_handle(id)?;
        let on = sink.is_some();
        let was = h.playback_audio.read().unwrap().is_some();
        *h.playback_audio.write().unwrap() = sink;
        if on != was {
            self.log(Level::Info, source, id, if on { "Son de la relecture activé" } else { "Son de la relecture coupé" });
            self.emit(Event::Cameras);
        }
        Ok(())
    }

    /// Exporte une période en `.mp4` sur le PC (tâche de fond : la caméra diffuse à vitesse réelle).
    pub fn camera_export_recording(self: &Arc<Self>, id: &str, start: DateTime<Local>, end: DateTime<Local>, source: Source) -> Result<PathBuf> {
        let h = self.camera_handle(id)?;
        let cfg = self.playback_config(id)?;
        if end <= start {
            bail!("période invalide");
        }
        if h.playback.lock().unwrap().exporting {
            bail!("un export est déjà en cours pour cette caméra");
        }
        let out = playback::recordings_dir().join(playback::export_name(&h.name(), start, end));
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(std::sync::atomic::AtomicU64::new(0f64.to_bits()));
        {
            let mut p = h.playback.lock().unwrap();
            (p.exporting, p.export_done, p.export_error) = (true, None, None);
            (p.export_cancel, p.export_done_secs, p.export_total, p.export_range) = (Some(cancel.clone()), done.clone(), (end - start).num_milliseconds() as f64 / 1000.0, Some((start, end)));
        }
        let file = out.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
        self.log(Level::Info, source, id, format!("Export démarré ({} → {}) : {file}", start.format("%Y-%m-%d %H:%M:%S"), end.format("%H:%M:%S")));
        self.emit(Event::Cameras);

        let (hub, h2, id, out2) = (self.clone(), h.clone(), id.to_string(), out.clone());
        tokio::spawn(async move {
            let (su, eu, o) = (start.with_timezone(&Utc), end.with_timezone(&Utc), out2.clone());
            let waker = hub.clone();
            let r = tokio::task::spawn_blocking(move || {
                playback::export_blocking(&cfg, su, eu, &o, cancel, &|secs| {
                    done.store(secs.to_bits(), Ordering::Relaxed);
                    waker.emit(Event::Cameras);
                })
            })
            .await;
            {
                let mut p = h2.playback.lock().unwrap();
                (p.exporting, p.export_cancel) = (false, None);
                match &r {
                    Ok(Ok(())) => p.export_done = Some(out2.clone()),
                    Ok(Err(e)) if format!("{e}") == "export annulé" => {}
                    Ok(Err(e)) => p.export_error = Some(format!("{e:#}")),
                    Err(e) => p.export_error = Some(format!("{e}")),
                }
            }
            let file = out2.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
            match r {
                Ok(Ok(())) => hub.log(Level::Info, Source::System, &id, format!("Extrait exporté : {file}")),
                Ok(Err(e)) if format!("{e}") == "export annulé" => {}
                Ok(Err(e)) => hub.log(Level::Warning, Source::System, &id, format!("Export impossible ({file}) : {e:#}")),
                Err(e) => hub.log(Level::Warning, Source::System, &id, format!("Export impossible ({file}) : {e}")),
            }
            hub.emit(Event::Cameras);
        });
        Ok(out)
    }

    /// Annule l'export en cours (le fichier partiel est supprimé).
    pub fn camera_cancel_export(&self, id: &str, source: Source) -> Result<()> {
        let h = self.camera_handle(id)?;
        let Some(c) = h.playback.lock().unwrap().export_cancel.take() else { return Ok(()) };
        c.store(true, Ordering::Relaxed);
        self.log(Level::Info, source, id, "Export annulé");
        self.emit(Event::Cameras);
        Ok(())
    }

    /// Efface le résultat du dernier export (bandeau refermé).
    pub fn camera_clear_export(&self, id: &str) {
        if let Ok(h) = self.camera_handle(id) {
            let mut p = h.playback.lock().unwrap();
            (p.export_done, p.export_error) = (None, None);
        }
        self.emit(Event::Cameras);
    }

    // ---- Ajout, réglages, suppression --------------------------------------------------------

    pub fn add_camera(self: &Arc<Self>, mut cfg: CameraConfig, source: Source) -> Result<CameraView> {
        cfg.host = cfg.host.trim().to_string();
        if cfg.host.is_empty() {
            bail!("adresse IP manquante");
        }
        if cfg.id.is_empty() {
            cfg.id = cfg.make_id();
        }
        {
            let config = self.config.lock().unwrap();
            if let Some(c) = config.cameras.iter().find(|c| c.id == cfg.id || c.host == cfg.host || (cfg.serial.is_some() && c.serial == cfg.serial)) {
                bail!("cette caméra est déjà ajoutée (« {} »)", c.display_name());
            }
        }
        if cfg.name.trim().is_empty() {
            cfg.name = match cfg.brand {
                Brand::Generic => "Caméra".into(),
                b => format!("Caméra {}", b.label()),
            };
        }
        self.config.lock().unwrap().cameras.push(cfg.clone());
        let h = self.spawn_camera(cfg.clone());
        let model = cfg.model.as_deref().map(|m| format!("{m}, ")).unwrap_or_default();
        self.log(Level::Info, source, &cfg.id, format!("Caméra ajoutée ({model}{})", cfg.host));
        self.persist_cameras();
        self.emit(Event::Cameras);
        Ok(h.view())
    }

    /// Appaire une caméra HomeSmart (fabriquée soi-même) avec son code, puis l'ajoute : vidéo et
    /// son en RTSP comme les autres, réglages par son agent. `cfg` : adresse, nom, pièce.
    pub async fn pair_camera(self: &Arc<Self>, mut cfg: CameraConfig, api_port: u16, code: String, source: Source) -> Result<CameraView> {
        cfg.host = cfg.host.trim().to_string();
        if cfg.host.is_empty() {
            bail!("adresse IP manquante");
        }
        // Avant l'appairage : un essai inutile compterait parmi ceux que la caméra limite.
        if let Some(c) = self.config.lock().unwrap().cameras.iter().find(|c| c.host == cfg.host) {
            bail!("cette caméra est déjà ajoutée (« {} »)", c.display_name());
        }
        let host = cfg.host.clone();
        let r = tokio::task::spawn_blocking(move || camera::agent::pair(&host, api_port, &code)).await.map_err(|e| anyhow!("{e}"))?;
        let (link, info) = match r {
            Ok(x) => x,
            Err(e) => {
                self.log(Level::Warning, source, "", format!("Appairage de la caméra HomeSmart {} refusé : {e:#}", cfg.host));
                return Err(e);
            }
        };
        camera::agent::apply_pairing(&mut cfg, link, &info);
        self.log(Level::Info, source, "", format!("Caméra HomeSmart appairée : « {} » ({}, {})", info.name, info.board, cfg.host));
        match self.add_camera(cfg.clone(), source) {
            Ok(v) => Ok(v),
            Err(e) => {
                // Déjà présente sous une autre adresse : la caméra n'a pas à garder ce second accès.
                let _ = tokio::task::spawn_blocking(move || camera::agent::unpair(&cfg)).await;
                Err(e)
            }
        }
    }

    /// Présentation, réglages et état d'une caméra HomeSmart, lus sur son agent.
    pub async fn camera_agent(&self, id: &str) -> Result<camera::agent::AgentState> {
        let cfg = self.camera_config(id).ok_or_else(|| anyhow!("caméra inconnue"))?;
        tokio::task::spawn_blocking(move || camera::agent::state(&cfg)).await.map_err(|e| anyhow!("{e}"))?
    }

    /// Applique des réglages à une caméra HomeSmart (journalisés), puis rouvre son flux : la
    /// caméra relance son encodage avec les nouveaux réglages.
    pub async fn set_camera_agent_settings(self: &Arc<Self>, id: &str, before: homesmart_cam_proto::Settings, after: homesmart_cam_proto::Settings, source: Source) -> Result<homesmart_cam_proto::Settings> {
        let h = self.camera_handle(id)?;
        let cfg = h.cfg.read().unwrap().clone();
        let r = tokio::task::spawn_blocking(move || camera::agent::set_settings(&cfg, &after)).await.map_err(|e| anyhow!("{e}"))?;
        match r {
            Ok(s) => {
                let changes = camera::agent::describe_changes(&before, &s);
                let msg = if changes.is_empty() { "Réglages de la caméra renvoyés, sans changement".to_string() } else { format!("Réglages de la caméra : {}", changes.join(", ")) };
                self.log(Level::Info, source, id, msg);
                let tx = h.tx.clone();
                tokio::spawn(async move {
                    sleep(Duration::from_secs(2)).await;
                    let _ = tx.try_send(CamCmd::Restart);
                });
                self.emit(Event::Cameras);
                Ok(s)
            }
            Err(e) => {
                self.log(Level::Warning, source, id, format!("Réglages de la caméra refusés : {e:#}"));
                Err(e)
            }
        }
    }

    pub fn update_camera(self: &Arc<Self>, id: &str, p: CameraPatch, source: Source) -> Result<CameraView> {
        let h = self.camera_handle(id)?;
        let before = h.cfg.read().unwrap().clone();
        let mut c = before.clone();
        let opt = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        if let Some(n) = p.name {
            c.name = n.trim().to_string();
        }
        if let Some(r) = p.room {
            c.room = opt(Some(r));
        }
        if let Some(v) = p.host.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            c.host = v;
        }
        if let Some(v) = p.username {
            c.username = v.trim().to_string();
        }
        if let Some(v) = p.password {
            c.password = v;
        }
        if let Some(v) = p.brand {
            c.brand = v;
        }
        if let Some(v) = p.rtsp_port.filter(|&p| p > 0) {
            c.rtsp_port = v;
        }
        if let Some(v) = p.onvif_port.filter(|&p| p > 0) {
            c.onvif_port = v;
        }
        if let Some(v) = p.main_path {
            c.main_path = opt(Some(v));
        }
        if let Some(v) = p.sub_path {
            c.sub_path = opt(Some(v));
        }
        if let Some(v) = p.invert_tilt {
            c.invert_tilt = v;
        }
        if let Some(v) = p.invert_pan {
            c.invert_pan = v;
        }
        let mut changes = Vec::new();
        if c.name != before.name {
            changes.push(format!("nom « {} » → « {} »", before.display_name(), c.display_name()));
        }
        if c.room != before.room {
            changes.push(format!("pièce : {}", c.room.as_deref().unwrap_or("aucune")));
        }
        if c.host != before.host {
            changes.push(format!("adresse {} → {}", before.host, c.host));
        }
        if c.username != before.username {
            changes.push(format!("utilisateur « {} »", c.username));
        }
        if c.password != before.password {
            // Le mot de passe lui-même n'est jamais écrit dans le journal.
            changes.push(if c.password.is_empty() { "mot de passe effacé".into() } else { "mot de passe modifié".into() });
        }
        if c.brand != before.brand {
            changes.push(format!("marque : {}", c.brand.label()));
        }
        if (c.rtsp_port, c.onvif_port) != (before.rtsp_port, before.onvif_port) {
            changes.push(format!("ports RTSP {} / ONVIF {}", c.rtsp_port, c.onvif_port));
        }
        if (&c.main_path, &c.sub_path) != (&before.main_path, &before.sub_path) {
            changes.push("chemins RTSP modifiés".into());
        }
        if c.invert_tilt != before.invert_tilt {
            changes.push(if c.invert_tilt { "haut / bas inversés".into() } else { "haut / bas normaux".into() });
        }
        if c.invert_pan != before.invert_pan {
            changes.push(if c.invert_pan { "gauche / droite inversés".into() } else { "gauche / droite normaux".into() });
        }
        if changes.is_empty() {
            return Ok(h.view());
        }
        let reconnect = (&c.host, &c.username, &c.password, c.rtsp_port, c.onvif_port, &c.main_path, &c.sub_path, c.brand)
            != (&before.host, &before.username, &before.password, before.rtsp_port, before.onvif_port, &before.main_path, &before.sub_path, before.brand);
        *h.cfg.write().unwrap() = c;
        self.log_keyed(Level::Info, source, id, &format!("cam:{id}:settings"), format!("Caméra modifiée : {}", changes.join(", ")));
        self.persist_cameras();
        if reconnect {
            *h.onvif.lock().unwrap() = None;
            {
                let mut s = h.state.write().unwrap();
                (s.error, s.onvif_error, s.notice, s.main_unavailable) = (None, None, None, false);
            }
            let _ = h.tx.try_send(CamCmd::Restart);
            tokio::spawn(refresh_onvif(self.clone(), h.clone()));
        }
        self.emit(Event::Cameras);
        Ok(h.view())
    }

    pub fn remove_camera(self: &Arc<Self>, id: &str, source: Source) -> Result<()> {
        // Son analyse par IA s'arrête avec elle.
        self.stop_worker(id);
        if let Ok(h) = self.camera_handle(id) {
            if let Some(stop) = h.tamper_stop.lock().unwrap().take() {
                stop.store(true, Ordering::Relaxed);
            }
        }
        let name = self.camera_name(id).ok_or_else(|| anyhow!("caméra inconnue"))?;
        // Caméra HomeSmart : elle oublie ce HomeSmart (son accès au flux et à l'agent).
        if let Some(c) = self.camera_config(id).filter(|c| c.agent.is_some()) {
            let (hub, id, label) = (self.clone(), id.to_string(), format!("« {name} » ({})", c.host));
            tokio::spawn(async move {
                match tokio::task::spawn_blocking(move || camera::agent::unpair(&c)).await {
                    Ok(Ok(())) => hub.log(Level::Info, Source::System, &id, format!("La caméra {label} a oublié ce HomeSmart")),
                    Ok(Err(e)) => hub.log(Level::Warning, Source::System, &id, format!("La caméra {label} n'a pas pu oublier ce HomeSmart ({e:#}) : sur la caméra, `homesmart-cam reset` efface ses appairages")),
                    Err(_) => {}
                }
            });
        }
        // Journalisé avant le retrait, pour garder le nom dans l'entrée.
        self.record(crate::journal::Entry::new(Level::Info, source, id, &name, "Caméra supprimée"));
        let h = self.cameras.write().unwrap().remove(id);
        if let Some(h) = h {
            let _ = h.tx.try_send(CamCmd::Shutdown);
        }
        {
            let mut cfg = self.config.lock().unwrap();
            cfg.cameras.retain(|c| c.id != id);
            cfg.scenarios.retain(|s| s.camera != id);
        }
        self.forget_presence(id);
        self.persist();
        self.emit(Event::Cameras);
        Ok(())
    }

    // ---- ONVIF : orientation et positions ---------------------------------------------------

    /// Exécute une commande ONVIF (session gardée en cache, rouverte en cas d'erreur).
    async fn with_onvif<R: Send + 'static>(&self, h: &Arc<CameraHandle>, f: impl FnOnce(&Onvif) -> Result<R> + Send + 'static) -> Result<R> {
        onvif_call(h, f).await
    }

    /// Tourne une caméra sans mouvement continu : un premier pas tout de suite (son erreur est
    /// renvoyée), puis un pas toutes les `NUDGE_EVERY` jusqu'à `camera_stop`.
    async fn start_nudges(&self, h: &Arc<CameraHandle>, pan: f32, tilt: f32) -> Result<()> {
        if let Some(old) = h.nudges.lock().unwrap().take() {
            old.abort();
        }
        let (dx, dy) = (pan * NUDGE, tilt * NUDGE);
        onvif_call(h, move |o| o.relative_move(dx, dy)).await?;
        let h2 = h.clone();
        let task = tokio::spawn(async move {
            let start = Instant::now();
            while start.elapsed() < NUDGE_MAX {
                tokio::time::sleep(NUDGE_EVERY).await;
                if onvif_call(&h2, move |o| o.relative_move(dx, dy)).await.is_err() {
                    break;
                }
            }
        });
        *h.nudges.lock().unwrap() = Some(task);
        Ok(())
    }

    /// Démarre un mouvement continu (vitesses −1…1) ; `camera_stop` l'arrête.
    pub async fn camera_move(&self, id: &str, pan: f32, tilt: f32, source: Source) -> Result<()> {
        let h = self.camera_handle(id)?;
        if source != Source::Vision {
            *h.manual_move.lock().unwrap() = Some(Instant::now());
        }
        // Le journal garde le sens demandé ; la caméra reçoit le sens corrigé si ses axes sont inversés.
        let dir = direction(pan, tilt);
        let (ip, it) = {
            let c = h.cfg.read().unwrap();
            (c.invert_pan, c.invert_tilt)
        };
        let (pan, tilt) = (if ip { -pan } else { pan }, if it { -tilt } else { tilt });
        let r = if h.relative_ptz.load(Ordering::Relaxed) {
            self.start_nudges(&h, pan, tilt).await
        } else {
            match self.with_onvif(&h, move |o| o.continuous_move(pan, tilt)).await {
                // Mouvement continu inconnu (certaines Tapo) : petits pas, pour cette caméra désormais.
                Err(e) if not_implemented(&e) => {
                    h.relative_ptz.store(true, Ordering::Relaxed);
                    self.log(Level::Info, Source::System, id, "Cette caméra ne connaît pas le mouvement continu ONVIF : orientation par petits pas");
                    self.start_nudges(&h, pan, tilt).await
                }
                r => r,
            }
        };
        match &r {
            Ok(()) => {
                let trail = {
                    let mut t = h.ptz_trail.lock().unwrap();
                    if t.0.is_none_or(|at| at.elapsed() > PTZ_TRAIL) {
                        t.1.clear();
                    }
                    t.0 = Some(Instant::now());
                    if t.1.last() != Some(&dir) {
                        t.1.push(dir);
                    }
                    t.1.join(" → ")
                };
                self.log_keyed(Level::Info, source, id, &format!("cam:{id}:ptz"), format!("Orientation : {trail}"));
            }
            Err(e) => self.log(Level::Warning, source, id, format!("Orientation ({dir}) refusée : {e:#}")),
        }
        r
    }

    /// Dernière orientation commandée à la main (le suivi automatique fait alors une pause).
    pub(super) fn camera_last_manual_move(&self, id: &str) -> Option<Instant> {
        self.camera_handle(id).ok().and_then(|h| *h.manual_move.lock().unwrap())
    }

    pub async fn camera_stop(&self, id: &str, source: Source) -> Result<()> {
        let h = self.camera_handle(id)?;
        // Petits pas : il suffit de ne plus en envoyer (chacun s'arrête de lui-même).
        let nudging = h.nudges.lock().unwrap().take();
        if let Some(t) = nudging {
            t.abort();
            return Ok(());
        }
        if h.relative_ptz.load(Ordering::Relaxed) {
            return Ok(());
        }
        let r = self.with_onvif(&h, |o| o.stop()).await;
        if let Err(e) = &r {
            // Important : la caméra pourrait continuer de tourner jusqu'en butée.
            self.log(Level::Warning, source, id, format!("Arrêt de l'orientation refusé : {e:#}"));
        }
        r
    }

    pub async fn camera_home(&self, id: &str, source: Source) -> Result<()> {
        let h = self.camera_handle(id)?;
        // Les Imou n'ont pas de position d'origine ONVIF : on prend la position mémorisée « Origine ».
        let origin = h.state.read().unwrap().presets.iter().find(|p| p.name.trim().eq_ignore_ascii_case("origine")).map(|p| p.token.clone());
        let r = self
            .with_onvif(&h, move |o| match o.goto_home() {
                Err(e) if not_implemented(&e) => match &origin {
                    Some(t) => o.goto_preset(t),
                    None => Err(anyhow!("cette caméra n'a pas de position d'origine : oriente-la puis mémorise une position nommée « Origine »")),
                },
                r => r,
            })
            .await;
        match &r {
            Ok(()) => self.log(Level::Info, source, id, "Retour à la position d'origine"),
            Err(e) => {
                if e.to_string().contains("pas de position d'origine") {
                    h.state.write().unwrap().home_supported = Some(false);
                    self.emit(Event::Cameras);
                }
                self.log(Level::Warning, source, id, format!("Retour à la position d'origine impossible : {e:#}"));
            }
        }
        r
    }

    pub async fn camera_goto_preset(&self, id: &str, token: &str, source: Source) -> Result<()> {
        let h = self.camera_handle(id)?;
        let name = h.state.read().unwrap().presets.iter().find(|p| p.token == token).map(|p| p.name.clone()).unwrap_or_else(|| format!("n° {token}"));
        let t = token.to_string();
        let r = self.with_onvif(&h, move |o| o.goto_preset(&t)).await;
        match &r {
            Ok(()) => self.log(Level::Info, source, id, format!("Position « {name} » rappelée")),
            Err(e) => self.log(Level::Warning, source, id, format!("Rappel de la position « {name} » refusé : {e:#}")),
        }
        r
    }

    pub async fn camera_save_preset(&self, id: &str, name: &str, source: Source) -> Result<String> {
        let h = self.camera_handle(id)?;
        let n = name.trim().to_string();
        if n.is_empty() {
            bail!("nom de position manquant");
        }
        let n2 = n.clone();
        let r = self.with_onvif(&h, move |o| {
            let token = o.set_preset(&n2)?;
            Ok((token, o.presets().unwrap_or_default()))
        })
        .await;
        match r {
            Ok((token, presets)) => {
                h.state.write().unwrap().presets = presets;
                self.log(Level::Info, source, id, format!("Position « {n} » mémorisée"));
                self.emit(Event::Cameras);
                Ok(token)
            }
            Err(e) => {
                self.log(Level::Warning, source, id, format!("Mémorisation de la position « {n} » refusée : {e:#}"));
                Err(e)
            }
        }
    }

    pub async fn camera_remove_preset(&self, id: &str, token: &str, source: Source) -> Result<()> {
        let h = self.camera_handle(id)?;
        let name = h.state.read().unwrap().presets.iter().find(|p| p.token == token).map(|p| p.name.clone()).unwrap_or_else(|| format!("n° {token}"));
        let t = token.to_string();
        let r = self.with_onvif(&h, move |o| {
            o.remove_preset(&t)?;
            Ok(o.presets().unwrap_or_default())
        })
        .await;
        match r {
            Ok(presets) => {
                h.state.write().unwrap().presets = presets;
                self.log(Level::Info, source, id, format!("Position « {name} » supprimée"));
                self.emit(Event::Cameras);
                Ok(())
            }
            Err(e) => {
                self.log(Level::Warning, source, id, format!("Suppression de la position « {name} » refusée : {e:#}"));
                Err(e)
            }
        }
    }

    // ---- FFmpeg (flux H.265) ---------------------------------------------------------------------

    /// État de FFmpeg ; la première demande lance la recherche en arrière-plan.
    pub fn ffmpeg_status(self: &Arc<Self>) -> FfmpegStatus {
        let st = self.ffmpeg.lock().unwrap().clone();
        if !st.checked && !st.searching && !st.installing {
            self.refresh_ffmpeg(Source::System);
        }
        st
    }

    /// Recherche FFmpeg (au démarrage, ou « Vérifier à nouveau » après l'avoir installé soi-même).
    pub fn refresh_ffmpeg(self: &Arc<Self>, source: Source) {
        self.ffmpeg.lock().unwrap().searching = true;
        let hub = self.clone();
        tokio::task::spawn_blocking(move || {
            let had = hub.ffmpeg.lock().unwrap().path.is_some();
            let path = crate::camera::ffmpeg::find();
            let version = path.as_deref().and_then(crate::camera::ffmpeg::version);
            let can_install = path.is_none() && crate::camera::ffmpeg::can_install();
            {
                let mut st = hub.ffmpeg.lock().unwrap();
                (st.checked, st.searching, st.path, st.version, st.can_install) = (true, false, path.clone(), version.clone(), can_install);
                st.manual = crate::camera::ffmpeg::manual_command();
            }
            if source == Source::User {
                let msg = match (&path, &version) {
                    (Some(p), v) => format!("FFmpeg trouvé{} ({})", v.as_deref().map(|v| format!(", version {v}")).unwrap_or_default(), p.display()),
                    (None, _) => "FFmpeg toujours introuvable".to_string(),
                };
                hub.log(Level::Info, source, "", msg);
            }
            if path.is_some() && (!had || source == Source::User) {
                hub.ffmpeg_ready();
            }
            hub.emit(Event::Cameras);
        });
    }

    /// Installe FFmpeg (winget, paquet officiel Gyan.FFmpeg) ; la progression est dans `ffmpeg_status`.
    pub fn install_ffmpeg(self: &Arc<Self>, source: Source) {
        {
            let mut st = self.ffmpeg.lock().unwrap();
            if st.installing {
                return;
            }
            (st.installing, st.step, st.error) = (true, "Préparation…".into(), None);
        }
        self.log(Level::Info, source, "", format!("Installation de FFmpeg lancée (winget, paquet {})", crate::camera::ffmpeg::WINGET_ID));
        self.emit(Event::Cameras);
        let hub = self.clone();
        tokio::task::spawn_blocking(move || {
            let progress = |step: String| {
                let changed = {
                    let mut st = hub.ffmpeg.lock().unwrap();
                    let changed = st.step != step;
                    st.step = step;
                    changed
                };
                if changed {
                    hub.emit(Event::Cameras);
                }
            };
            let r = crate::camera::ffmpeg::install(progress);
            match r {
                Ok(path) => {
                    let version = crate::camera::ffmpeg::version(&path);
                    {
                        let mut st = hub.ffmpeg.lock().unwrap();
                        (st.installing, st.checked, st.path, st.version, st.step) = (false, true, Some(path.clone()), version.clone(), "Installé".into());
                    }
                    let v = version.map(|v| format!(" {v}")).unwrap_or_default();
                    hub.log(Level::Info, source, "", format!("FFmpeg{v} installé : les flux HD en H.265 sont maintenant lisibles"));
                    hub.ffmpeg_ready();
                }
                Err(e) => {
                    {
                        let mut st = hub.ffmpeg.lock().unwrap();
                        (st.installing, st.error) = (false, Some(format!("{e:#}")));
                    }
                    hub.log(Level::Warning, source, "", format!("Installation de FFmpeg échouée : {e:#}"));
                }
            }
            hub.emit(Event::Cameras);
        });
    }

    /// FFmpeg vient d'être trouvé : les caméras repassent en HD là où on l'avait remplacée par le flux fluide.
    fn ffmpeg_ready(&self) {
        let cams: Vec<Arc<CameraHandle>> = self.cameras.read().unwrap().values().cloned().collect();
        for h in cams {
            let was = {
                let mut s = h.state.write().unwrap();
                let was = s.main_unavailable;
                (s.main_unavailable, s.notice) = (false, None);
                was
            };
            if was {
                let _ = h.tx.try_send(CamCmd::Restart);
            }
        }
        self.emit(Event::Cameras);
    }

    /// Nouvel essai de connexion demandé (après un refus d'identifiants, par exemple).
    pub fn camera_retry(&self, id: &str, source: Source) -> Result<()> {
        let h = self.camera_handle(id)?;
        {
            let mut s = h.state.write().unwrap();
            (s.error, s.waiting_retry) = (None, false);
        }
        *h.onvif.lock().unwrap() = None;
        self.log(Level::Info, source, id, "Nouvelle tentative de connexion à la caméra");
        let _ = h.tx.try_send(CamCmd::Restart);
        self.emit(Event::Cameras);
        Ok(())
    }

    // ---- Image (luminosité, contraste, vision de nuit, WDR) ----------------------------------------

    pub async fn camera_imaging(&self, id: &str) -> Result<(ImagingSettings, ImagingOptions)> {
        let h = self.camera_handle(id)?;
        self.with_onvif(&h, |o| Ok((o.imaging_settings()?, o.imaging_options().unwrap_or_default()))).await
    }

    /// Applique les réglages d'image modifiés ; le journal dit ce qui a changé.
    pub async fn camera_set_imaging(&self, id: &str, before: ImagingSettings, after: ImagingSettings, source: Source) -> Result<()> {
        let h = self.camera_handle(id)?;
        let mut changes = Vec::new();
        let mut delta = ImagingSettings::default();
        let num = |name: &str, a: Option<f32>, b: Option<f32>, changes: &mut Vec<String>| {
            if a != b {
                if let Some(b) = b {
                    changes.push(format!("{name} {} → {}", a.map(|a| format!("{a:.0}")).unwrap_or_else(|| "?".into()), format!("{b:.0}")));
                    return Some(b);
                }
            }
            None
        };
        delta.brightness = num("luminosité", before.brightness, after.brightness, &mut changes);
        delta.contrast = num("contraste", before.contrast, after.contrast, &mut changes);
        delta.saturation = num("saturation", before.saturation, after.saturation, &mut changes);
        delta.sharpness = num("netteté", before.sharpness, after.sharpness, &mut changes);
        let label = |m: &str| match m {
            "AUTO" => "automatique",
            "ON" => "activé",
            "OFF" => "désactivé",
            other => other,
        }
        .to_string();
        if after.ir_cut != before.ir_cut {
            let m = after.ir_cut.clone().unwrap_or_default();
            let v = match m.as_str() {
                "AUTO" => "automatique",
                "ON" => "jour (filtre IR)",
                "OFF" => "nuit (infrarouge)",
                o => o,
            };
            changes.push(format!("vision de nuit : {v}"));
            delta.ir_cut = after.ir_cut.clone();
        }
        if after.wdr != before.wdr || after.wdr_level != before.wdr_level {
            changes.push(format!("WDR : {}{}", label(after.wdr.as_deref().unwrap_or("")), after.wdr_level.map(|l| format!(" ({l:.0})")).unwrap_or_default()));
            (delta.wdr, delta.wdr_level) = (after.wdr.clone(), after.wdr_level);
        }
        if after.backlight != before.backlight {
            changes.push(format!("contre-jour : {}", label(after.backlight.as_deref().unwrap_or(""))));
            delta.backlight = after.backlight.clone();
        }
        if changes.is_empty() {
            return Ok(());
        }
        let r = self.with_onvif(&h, move |o| o.set_imaging(&delta)).await;
        match &r {
            Ok(()) => self.log_keyed(Level::Info, source, id, &format!("cam:{id}:image"), format!("Image : {}", changes.join(", "))),
            Err(e) => self.log(Level::Warning, source, id, format!("Réglages d'image refusés : {e:#}")),
        }
        r
    }

    // ---- Qualité vidéo -------------------------------------------------------------------------

    /// Réglages d'encodage de chaque flux (HD d'abord) et les valeurs que la caméra accepte.
    pub async fn camera_encoders(&self, id: &str) -> Result<Vec<(Quality, EncoderConfig, EncoderOptions)>> {
        let h = self.camera_handle(id)?;
        self.with_onvif(&h, |o| {
            let mut list = o.encoder_configs()?;
            // Le flux principal est celui de plus grande résolution.
            list.sort_by_key(|e| std::cmp::Reverse(e.width * e.height));
            Ok(list
                .into_iter()
                .take(2)
                .enumerate()
                .map(|(i, e)| {
                    let opts = o.encoder_options(&e.token).unwrap_or_default();
                    (if i == 0 { Quality::Main } else { Quality::Sub }, e, opts)
                })
                .collect())
        })
        .await
    }

    /// Change la résolution, la cadence ou le débit d'un flux ; le direct est rouvert avec le nouveau réglage.
    /// Applique un réglage de qualité vidéo. Renvoie, le cas échéant, ce que la caméra a ajusté
    /// d'elle-même (beaucoup acceptent la demande mais plafonnent sans le dire, ex. le débit) :
    /// le réglage est relu après coup pour le savoir.
    pub async fn camera_set_encoder(&self, id: &str, q: Quality, new: EncoderConfig, source: Source) -> Result<Option<String>> {
        let h = self.camera_handle(id)?;
        let current = self.camera_encoders(id).await?.into_iter().find(|(qq, ..)| *qq == q).map(|(_, e, _)| e);
        let mut changes = Vec::new();
        if let Some(c) = &current {
            if (c.width, c.height) != (new.width, new.height) {
                changes.push(format!("{}×{} → {}×{}", c.width, c.height, new.width, new.height));
            }
            if c.fps != new.fps {
                changes.push(format!("{} → {} i/s", c.fps, new.fps));
            }
            if c.bitrate_kbps != new.bitrate_kbps {
                changes.push(format!("{} → {} kbit/s", c.bitrate_kbps, new.bitrate_kbps));
            }
        }
        if changes.is_empty() && current.is_some() {
            return Ok(None);
        }
        let n = new.clone();
        let r = self.with_onvif(&h, move |o| o.set_encoder(&n)).await;
        let what = format!("flux {}", q.label());
        if let Err(e) = &r {
            self.log(Level::Warning, source, id, format!("Qualité vidéo ({what}) refusée : {e:#}"));
            return r.map(|_| None);
        }
        self.log(Level::Info, source, id, format!("Qualité vidéo ({what}) : {}", changes.join(" · ")));
        // Profils relus et direct rouvert avec le nouveau réglage.
        *h.onvif.lock().unwrap() = None;
        let _ = h.tx.try_send(CamCmd::Restart);
        self.emit(Event::Cameras);
        // Ce que la caméra a vraiment gardé.
        let kept = self.camera_encoders(id).await.ok().and_then(|l| l.into_iter().find(|(qq, ..)| *qq == q)).map(|(_, e, _)| e);
        let Some(k) = kept else { return Ok(None) };
        let mut adjusted = Vec::new();
        if (k.width, k.height) != (new.width, new.height) {
            adjusted.push(format!("{}×{} au lieu de {}×{}", k.width, k.height, new.width, new.height));
        }
        if k.fps != new.fps {
            adjusted.push(format!("{} i/s au lieu de {}", k.fps, new.fps));
        }
        if k.bitrate_kbps != new.bitrate_kbps {
            adjusted.push(format!("{} kbit/s au lieu de {}", k.bitrate_kbps, new.bitrate_kbps));
        }
        if adjusted.is_empty() {
            return Ok(None);
        }
        let msg = format!("La caméra a gardé {} (probablement son maximum pour ce flux)", adjusted.join(", "));
        self.log(Level::Warning, Source::Device, id, format!("Qualité vidéo ({what}) : {msg}"));
        Ok(Some(msg))
    }

    // ---- Captures, recherche, test -------------------------------------------------------------

    /// Enregistre l'image actuelle (celle du direct s'il est ouvert, sinon une image HD prise exprès).
    pub async fn camera_snapshot(&self, id: &str, source: Source) -> Result<PathBuf> {
        let h = self.camera_handle(id)?;
        let live = h.state.read().unwrap().streaming.is_some();
        let current = h.frame.lock().unwrap().clone().filter(|_| live);
        let frame = match current {
            Some(f) => Ok(f),
            None => {
                let cfg = h.cfg.read().unwrap().clone();
                let q = if h.state.read().unwrap().main_unavailable { Quality::Sub } else { Quality::Main };
                let r = match stream::grab(&cfg.rtsp_url(q), &cfg.username, &cfg.password, Duration::from_secs(15)).await {
                    Err(e) if q == Quality::Main && e.downcast_ref::<stream::NeedsFfmpeg>().is_some() => {
                        stream::grab(&cfg.rtsp_url(Quality::Sub), &cfg.username, &cfg.password, Duration::from_secs(15)).await
                    }
                    r => r,
                };
                r.map(|(f, _)| Arc::new(f))
            }
        };
        let result = frame.and_then(|f| {
            let path = camera::captures_dir().join(camera::capture_name(&h.name()));
            camera::save_png(&f, &path)?;
            Ok((path, f.width, f.height))
        });
        match result {
            Ok((path, w, hgt)) => {
                let file = path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
                self.log(Level::Info, source, id, format!("Capture enregistrée ({w}×{hgt}) : {file}"));
                Ok(path)
            }
            Err(e) => {
                let msg = stream::describe_error(&e);
                self.log(Level::Warning, source, id, format!("Capture impossible : {msg}"));
                Err(anyhow!(msg))
            }
        }
    }

    /// Cherche les caméras du réseau (Dahua / Imou, ONVIF, port RTSP).
    pub async fn scan_cameras(&self, source: Source) -> Vec<discover::Found> {
        let list = discover::scan(Duration::from_millis(2500)).await;
        let named: Vec<String> = list.iter().filter(|f| f.model.is_some() || f.onvif).map(|f| format!("{} à {}", f.title(), f.ip)).collect();
        let msg = match (list.len(), named.is_empty()) {
            (0, _) => "Recherche de caméras : aucune trouvée".to_string(),
            (n, true) => format!("Recherche de caméras : {n} serveur(s) vidéo non identifié(s)"),
            (_, false) => format!("Recherche de caméras : {}", named.join(", ")),
        };
        self.log(Level::Info, source, "", msg);
        list
    }

    /// Essaie la vidéo (flux fluide) et ONVIF avec ces réglages, sans rien enregistrer.
    pub async fn test_camera(&self, cfg: &CameraConfig, source: Source) -> TestReport {
        let mut report = TestReport::default();
        match stream::grab(&cfg.rtsp_url(Quality::Sub), &cfg.username, &cfg.password, Duration::from_secs(12)).await {
            Ok((_, s)) => report.video = Some(s),
            Err(e) => report.video_error = Some(stream::describe_error(&e)),
        }
        // Une caméra HomeSmart se règle par son agent : pas d'ONVIF à essayer.
        if cfg.brand.has_onvif() {
            let (host, port, user, pass) = (cfg.host.clone(), cfg.onvif_port, cfg.username.clone(), cfg.password.clone());
            let onvif = tokio::task::spawn_blocking(move || {
                let o = Onvif::connect(&host, port, &user, &pass)?;
                Ok::<_, anyhow::Error>((o.device_info().ok(), o.ptz_supported()))
            })
            .await;
            match onvif {
                Ok(Ok((info, ptz))) => (report.info, report.ptz) = (info, ptz),
                Ok(Err(e)) => report.onvif_error = Some(format!("{e:#}")),
                Err(e) => report.onvif_error = Some(e.to_string()),
            }
        }
        let video = match (&report.video, &report.video_error) {
            (Some(s), _) => format!("vidéo OK ({})", s.summary()),
            (_, Some(e)) => format!("vidéo impossible : {e}"),
            _ => String::new(),
        };
        let onvif = match &report.onvif_error {
            None if !cfg.brand.has_onvif() => "réglages par l'agent de la caméra".to_string(),
            None if report.ptz => "ONVIF OK, orientable".to_string(),
            None => "ONVIF OK".to_string(),
            Some(e) => format!("ONVIF : {e}"),
        };
        let level = if report.video.is_some() { Level::Info } else { Level::Warning };
        self.log(level, source, "", format!("Test de connexion à la caméra {} : {video} · {onvif}", cfg.host));
        report
    }

    // ---- API locale Tapo : alarme, suivi des mouvements, carte SD ------------------------------

    /// Connexion partagée à l'API locale d'une caméra Tapo (empreinte du mot de passe du compte
    /// TP-Link). Échoue clairement si ce n'est pas une Tapo ou si le mot de passe manque.
    fn tapo_cam(&self, id: &str) -> Result<Arc<Mutex<tplink::camera::TapoCam>>> {
        let c = self.camera_config(id).ok_or_else(|| anyhow!("caméra inconnue"))?;
        if c.brand != Brand::Tapo {
            bail!("réglage réservé aux caméras Tapo");
        }
        let secret = c.tapo.filter(|s| s.is_set()).ok_or_else(|| {
            anyhow!("mot de passe du compte TP-Link non renseigné : ajoute-le dans les réglages de la caméra (il sert à l'alarme, au suivi et à la carte SD)")
        })?;
        Ok(tplink::camera::shared(&c.host, &secret))
    }

    /// Exécute une opération bloquante sur l'API locale Tapo, hors du fil de l'interface.
    async fn with_tapo<R: Send + 'static>(&self, id: &str, f: impl FnOnce(&mut tplink::camera::TapoCam) -> Result<R> + Send + 'static) -> Result<R> {
        let cam = self.tapo_cam(id)?;
        tokio::task::spawn_blocking(move || {
            let mut c = cam.lock().unwrap();
            c.login()?;
            f(&mut c)
        })
        .await
        .map_err(|e| anyhow!("tâche Tapo : {e}"))?
    }

    /// Enregistre le mot de passe du compte TP-Link (seulement ses empreintes) après l'avoir
    /// vérifié sur la caméra. Sert à l'alarme, au suivi des mouvements et à la carte SD.
    pub async fn set_camera_tapo_password(self: &Arc<Self>, id: &str, password: &str, source: Source) -> Result<()> {
        let c = self.camera_config(id).ok_or_else(|| anyhow!("caméra inconnue"))?;
        if c.brand != Brand::Tapo {
            bail!("réglage réservé aux caméras Tapo");
        }
        let secret = tplink::camera::TapoSecret::from_password(password);
        let (host, s2) = (c.host.clone(), secret.clone());
        // Vérifié avant d'être gardé (une seule tentative : chaque essai compte pour le verrouillage).
        tokio::task::spawn_blocking(move || {
            let mut cam = tplink::camera::TapoCam::new(&host, &s2);
            cam.login()
        })
        .await
        .map_err(|e| anyhow!("tâche Tapo : {e}"))??;
        {
            let h = self.camera_handle(id)?;
            h.cfg.write().unwrap().tapo = Some(secret.clone());
        }
        // La connexion partagée doit repartir avec la nouvelle empreinte.
        *tplink::camera::shared(&c.host, &secret).lock().unwrap() = tplink::camera::TapoCam::new(&c.host, &secret);
        self.log(Level::Info, source, id, "Mot de passe du compte TP-Link enregistré (alarme, suivi et carte SD Tapo disponibles)");
        self.persist_cameras();
        self.emit(Event::Cameras);
        Ok(())
    }

    pub async fn camera_alarm(&self, id: &str) -> Result<tplink::camera::AlarmConfig> {
        self.with_tapo(id, |c| c.alarm()).await
    }

    pub async fn camera_set_alarm(&self, id: &str, a: tplink::camera::AlarmConfig, source: Source) -> Result<()> {
        let before = self.with_tapo(id, |c| c.alarm()).await.ok();
        let a2 = a.clone();
        self.with_tapo(id, move |c| c.set_alarm(&a2)).await?;
        let mut parts = vec![if a.enabled { "activée".to_string() } else { "désactivée".to_string() }];
        let mut modes = Vec::new();
        if a.sound {
            modes.push("sirène");
        }
        if a.light {
            modes.push("lumière");
        }
        parts.push(modes.join(" et "));
        if before.as_ref().map(|b| b.volume.clone()).unwrap_or_default() != a.volume {
            if let Some(v) = &a.volume {
                parts.push(format!("volume {v}"));
            }
        }
        if let Some(d) = a.duration {
            parts.push(format!("durée {d} s"));
        }
        self.log(Level::Info, source, id, format!("Alarme de la caméra : {}", parts.join(", ")));
        self.emit(Event::Cameras);
        Ok(())
    }

    /// Déclenche (ou arrête) l'alarme tout de suite (sirène et lumière), quels que soient les réglages.
    pub async fn camera_trigger_alarm(&self, id: &str, on: bool, source: Source) -> Result<()> {
        self.with_tapo(id, move |c| c.manual_alarm(on)).await?;
        self.log(if on { Level::Warning } else { Level::Info }, source, id, if on { "Alarme déclenchée à la main (sirène et lumière)" } else { "Alarme arrêtée" });
        self.emit(Event::Cameras);
        Ok(())
    }

    pub async fn camera_auto_track(&self, id: &str) -> Result<bool> {
        self.with_tapo(id, |c| c.auto_track()).await
    }

    /// Suivi des mouvements intégré à la caméra (elle tourne toute seule pour suivre ce qui bouge).
    pub async fn camera_set_auto_track(&self, id: &str, on: bool, source: Source) -> Result<()> {
        self.with_tapo(id, move |c| c.set_auto_track(on)).await?;
        self.log(Level::Info, source, id, if on { "Suivi des mouvements de la caméra activé" } else { "Suivi des mouvements de la caméra désactivé" });
        self.emit(Event::Cameras);
        Ok(())
    }

    pub async fn camera_speaker_volume(&self, id: &str) -> Result<u8> {
        self.with_tapo(id, |c| c.speaker_volume()).await
    }

    pub async fn camera_set_speaker_volume(&self, id: &str, v: u8, source: Source) -> Result<()> {
        self.with_tapo(id, move |c| c.set_speaker_volume(v)).await?;
        self.log(Level::Info, source, id, format!("Volume du haut-parleur de la caméra : {v} %"));
        Ok(())
    }

    pub async fn camera_tapo_quality(&self, id: &str) -> Result<tplink::camera::VideoQuality> {
        self.with_tapo(id, |c| c.video_quality()).await
    }

    pub async fn camera_tapo_sd(&self, id: &str) -> Result<Vec<tplink::camera::SdCard>> {
        self.with_tapo(id, |c| c.sd_card()).await
    }

    /// Index des enregistrements d'un jour sur la carte SD d'une Tapo (bornes locales). La lecture
    /// et l'export de ces séquences passent par le flux propriétaire de la caméra, que HomeSmart
    /// ne sait pas ouvrir : seule la liste est disponible.
    pub async fn camera_tapo_recordings(&self, id: &str, date: NaiveDate) -> Result<Vec<(DateTime<Local>, DateTime<Local>)>> {
        let clips = self.with_tapo(id, move |c| c.recordings(date)).await?;
        Ok(clips
            .into_iter()
            .filter_map(|(a, b)| Some((DateTime::from_timestamp(a, 0)?.with_timezone(&Local), DateTime::from_timestamp(b, 0)?.with_timezone(&Local))))
            .collect())
    }

    // ---- Détection de sabotage -----------------------------------------------------------------

    pub fn camera_tamper(&self, id: &str) -> Option<camera::TamperConfig> {
        self.camera_config(id).map(|c| c.tamper)
    }

    /// Active ou coupe la détection de sabotage (par l'image dans HomeSmart, et/ou par les alertes
    /// de la caméra en ONVIF). Règle aussi le détecteur intégré de la caméra quand elle en a un.
    pub async fn set_camera_tamper(self: &Arc<Self>, id: &str, t: camera::TamperConfig, source: Source) -> Result<()> {
        let h = self.camera_handle(id)?;
        let before = h.cfg.read().unwrap().tamper;
        if before == t {
            return Ok(());
        }
        h.cfg.write().unwrap().tamper = t;
        let mut parts = Vec::new();
        if t.image != before.image {
            parts.push(format!("analyse de l'image {}", if t.image { "activée" } else { "coupée" }));
        }
        if t.camera != before.camera {
            parts.push(format!("alertes de la caméra {}", if t.camera { "activées" } else { "coupées" }));
        }
        if t.sensitivity != before.sensitivity {
            parts.push(format!("sensibilité {}", t.sensitivity.label()));
        }
        self.log(Level::Info, source, id, format!("Détection de sabotage : {}", if parts.is_empty() { "mise à jour".into() } else { parts.join(", ") }));
        self.persist_cameras();
        // Règle le détecteur intégré de la caméra (ONVIF), si elle en a un.
        if t.camera != before.camera || t.sensitivity != before.sensitivity {
            let sens = match t.sensitivity {
                camera::TamperSensitivity::Low => "low",
                camera::TamperSensitivity::Medium => "medium",
                camera::TamperSensitivity::High => "high",
            };
            let want = t.camera;
            let r = self
                .with_onvif(&h, move |o| match o.tamper_module()? {
                    Some(m) => o.set_tamper_module(&m, want, sens).map(|_| true),
                    None => Ok(false),
                })
                .await;
            match r {
                Ok(true) => self.log(Level::Info, source, id, format!("Détecteur de sabotage de la caméra {}", if want { "activé" } else { "désactivé" })),
                Ok(false) => {}
                // La caméra n'a pas de détecteur réglable : on s'appuie sur ses événements et/ou l'image.
                Err(e) => self.log(Level::Info, Source::System, id, format!("Détecteur intégré non réglable sur cette caméra ({e:#}) : surveillance par les événements et l'image")),
            }
        }
        self.restart_tamper(&h);
        self.emit(Event::Cameras);
        Ok(())
    }

    /// (Re)démarre la surveillance du sabotage d'une caméra selon ses réglages.
    pub(super) fn restart_tamper(self: &Arc<Self>, h: &Arc<CameraHandle>) {
        if let Some(stop) = h.tamper_stop.lock().unwrap().take() {
            stop.store(true, Ordering::Relaxed);
        }
        h.tamper_want.store(false, Ordering::Relaxed);
        let t = h.cfg.read().unwrap().tamper;
        let has_onvif = h.cfg.read().unwrap().brand.has_onvif();
        if !t.image && !(t.camera && has_onvif) {
            let _ = h.tx.try_send(CamCmd::Restart);
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        *h.tamper_stop.lock().unwrap() = Some(stop.clone());
        if t.image {
            h.tamper_want.store(true, Ordering::Relaxed);
            let _ = h.tx.try_send(CamCmd::Restart);
            tokio::spawn(tamper_image_monitor(self.clone(), h.clone(), stop.clone(), t.sensitivity));
        }
        if t.camera && has_onvif {
            tokio::spawn(tamper_events_monitor(self.clone(), h.clone(), stop));
        }
    }

    /// Signale (ou lève) une alerte de sabotage : journal, problèmes et scénarios.
    pub(super) fn report_tamper(self: &Arc<Self>, id: &str, source: Source, active: bool, detail: String) {
        use crate::problems::ProblemKind;
        if active {
            self.log(Level::Critical, source, id, format!("Sabotage de la caméra : {detail}"));
            self.raise(ProblemKind::CameraTampered, id, Level::Critical, detail);
            self.scene_trigger(id, crate::automation::Trigger::Tamper);
        } else {
            self.log(Level::Info, source, id, format!("Caméra : {detail}"));
            self.settle(ProblemKind::CameraTampered, id, &detail);
        }
    }
}

/// Informations, orientation et positions mémorisées (en ONVIF), au démarrage et après un changement de réglages.
async fn refresh_onvif(hub: Arc<Hub>, h: Arc<CameraHandle>) {
    // Caméra HomeSmart : ni orientation ni positions, ses réglages passent par son agent.
    if !h.cfg.read().unwrap().brand.has_onvif() {
        {
            let mut s = h.state.write().unwrap();
            (s.ptz, s.presets_supported, s.onvif_error) = (Some(false), Some(false), None);
        }
        hub.emit(Event::Cameras);
        return;
    }
    if h.cfg.read().unwrap().password.is_empty() {
        h.state.write().unwrap().onvif_error = Some("code de sécurité manquant".into());
        hub.emit(Event::Cameras);
        return;
    }
    let needs_paths = {
        let c = h.cfg.read().unwrap();
        c.brand == Brand::Generic && c.main_path.as_deref().unwrap_or("").trim().is_empty()
    };
    let r = hub
        .with_onvif(&h, move |o| {
            let info = o.device_info().ok();
            let presets = if o.ptz_supported() {
                match o.presets() {
                    Ok(p) => (p, Some(true)),
                    Err(e) if not_implemented(&e) => (Vec::new(), Some(false)),
                    Err(_) => (Vec::new(), None),
                }
            } else {
                (Vec::new(), Some(false))
            };
            // Caméra d'une autre marque sans chemin RTSP : on demande ses adresses à la caméra.
            let uris = if needs_paths { o.stream_uris().ok() } else { None };
            Ok((info, o.ptz_supported(), presets, uris))
        })
        .await;
    let id = h.id();
    match r {
        Ok((info, ptz, (presets, presets_supported), uris)) => {
            let mut changed = false;
            if let Some((main, sub)) = uris {
                let mut c = h.cfg.write().unwrap();
                if let Some((port, path)) = camera::split_rtsp_uri(&main) {
                    c.main_path = Some(path.clone());
                    if let Some(p) = port {
                        c.rtsp_port = p;
                    }
                    c.sub_path = sub.as_deref().and_then(camera::split_rtsp_uri).map(|(_, p)| p);
                    let msg = format!("Chemins RTSP détectés en ONVIF : {path}{}", c.sub_path.as_deref().map(|s| format!(" (fluide : {s})")).unwrap_or_default());
                    drop(c);
                    hub.log(Level::Info, Source::System, &id, msg);
                    changed = true;
                    let _ = h.tx.try_send(CamCmd::Restart);
                }
            }
            if let Some(i) = &info {
                let mut c = h.cfg.write().unwrap();
                if c.model.is_none() && !i.model.is_empty() {
                    c.model = Some(i.model.clone());
                    changed = true;
                }
                if c.serial.is_none() && !i.serial.is_empty() {
                    c.serial = Some(i.serial.clone());
                    changed = true;
                }
            }
            {
                let mut s = h.state.write().unwrap();
                (s.info, s.ptz, s.presets, s.presets_supported, s.onvif_error) = (info, Some(ptz), presets, presets_supported, None);
            }
            if changed {
                hub.persist_cameras();
            }
        }
        Err(e) => {
            let msg = format!("{e:#}");
            let first = h.state.read().unwrap().onvif_error.as_deref() != Some(msg.as_str());
            h.state.write().unwrap().onvif_error = Some(msg.clone());
            if first {
                hub.log(Level::Warning, Source::System, &id, format!("Pilotage ONVIF indisponible : {msg}"));
            }
        }
    }
    hub.emit(Event::Cameras);
}

impl Hub {
    /// Toutes les 5 s : caméras injoignables depuis `offline_after`, ou sans image depuis `after` (problèmes).
    pub(super) fn camera_problems_tick(&self, offline_after: Duration, after: Duration) {
        use crate::problems::{self, ProblemKind};
        let cams: Vec<Arc<CameraHandle>> = self.cameras.read().unwrap().values().cloned().collect();
        for h in cams {
            let (id, host, port) = {
                let c = h.cfg.read().unwrap();
                (c.id.clone(), c.host.clone(), c.rtsp_port)
            };
            let s = h.state.read().unwrap().clone();
            let offline = s.offline_since.map(|t| t.elapsed()).filter(|d| *d >= offline_after);
            match offline {
                Some(d) => {
                    let (level, text) = problems::camera_offline(&host, port, d);
                    self.raise(ProblemKind::CameraOffline, &id, level, text);
                }
                None if s.offline_since.is_none() => self.settle(ProblemKind::CameraOffline, &id, "la caméra répond de nouveau"),
                None => {}
            }
            let failing = s.failing_since.map(|t| t.elapsed()).filter(|d| *d >= after);
            match (failing, &s.error) {
                (Some(d), Some(e)) if offline.is_none() => {
                    let (level, text) = problems::stream_lost(e, d, s.waiting_retry);
                    self.raise(ProblemKind::StreamLost, &id, level, text);
                }
                _ if s.failing_since.is_none() && self.problem_open(ProblemKind::StreamLost, &id) => {
                    let why = if s.streaming.is_some() || h.got_frame.load(Ordering::Relaxed) { "les images arrivent de nouveau" } else { "flux plus demandé" };
                    self.settle(ProblemKind::StreamLost, &id, why);
                }
                _ => {}
            }
        }
    }
}

/// La caméra répond-elle ? Les changements sont journalisés.
async fn probe(hub: &Hub, h: &CameraHandle) {
    let (host, port) = {
        let c = h.cfg.read().unwrap();
        (c.host.clone(), c.rtsp_port)
    };
    let up = discover::port_open(&host, port, Duration::from_secs(3)).await;
    let before = h.state.read().unwrap().online;
    if before == Some(up) {
        return;
    }
    {
        let mut s = h.state.write().unwrap();
        s.online = Some(up);
        s.offline_since = if up { None } else { s.offline_since.or(Some(Instant::now())) };
    }
    let id = h.id();
    match (before, up) {
        (Some(false), true) => hub.log(Level::Info, Source::System, &id, "Caméra de nouveau joignable"),
        (_, false) => hub.log(Level::Warning, Source::System, &id, format!("Caméra injoignable ({host}:{port})")),
        _ => {}
    }
    hub.emit(Event::Cameras);
}

/// Surveillance du sabotage par l'image : prend une image par seconde du flux fluide (gardé
/// ouvert par `tamper_want`) et la juge. Le détecteur repart à zéro quand la caméra est orientée.
async fn tamper_image_monitor(hub: Arc<Hub>, h: Arc<CameraHandle>, stop: Arc<AtomicBool>, sens: camera::TamperSensitivity) {
    let id = h.id();
    let mut detector = TamperDetector::new(sens);
    let t0 = Instant::now();
    let mut last_frame: Option<Arc<Frame>> = None;
    let mut last_manual = *h.manual_move.lock().unwrap();
    while !stop.load(Ordering::Relaxed) {
        sleep(Duration::from_millis(900)).await;
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // Caméra orientée depuis la dernière fois : la vue de référence n'est plus valable.
        let m = *h.manual_move.lock().unwrap();
        if m != last_manual {
            last_manual = m;
            detector.moved(t0.elapsed().as_secs_f64());
        }
        let Some(frame) = h.frame.lock().unwrap().clone() else { continue };
        if last_frame.as_ref().is_some_and(|f| Arc::ptr_eq(f, &frame)) {
            continue; // pas de nouvelle image (flux fermé ?)
        }
        last_frame = Some(frame.clone());
        if let Some(ev) = detector.update(t0.elapsed().as_secs_f64(), &frame) {
            match ev {
                TamperEvent::Started(kind) => hub.report_tamper(&id, Source::Vision, true, kind.label().to_string()),
                TamperEvent::Cleared => hub.report_tamper(&id, Source::Vision, false, "l'image est redevenue normale".into()),
            }
            hub.emit(Event::Cameras);
        }
    }
}

/// Surveillance du sabotage par les alertes de la caméra (ONVIF) : abonnement aux événements et
/// attente en boucle. L'abonnement est renouvelé avant son échéance ; une coupure est retentée.
async fn tamper_events_monitor(hub: Arc<Hub>, h: Arc<CameraHandle>, stop: Arc<AtomicBool>) {
    let id = h.id();
    const TERM: u32 = 60;
    while !stop.load(Ordering::Relaxed) {
        let sub = match onvif_call(&h, |o| o.subscribe_events(TERM)).await {
            Ok(s) => s,
            Err(_) => {
                // La caméra ne répond pas (ou pas d'événements ONVIF) : nouvel essai plus tard.
                for _ in 0..30 {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    sleep(Duration::from_secs(1)).await;
                }
                continue;
            }
        };
        let mut last_renew = Instant::now();
        loop {
            if stop.load(Ordering::Relaxed) {
                let (h2, sub2) = (h.clone(), sub.clone());
                let _ = onvif_call(&h2, move |o| {
                    o.unsubscribe_events(&sub2);
                    Ok(())
                })
                .await;
                return;
            }
            let sub2 = sub.clone();
            let events = onvif_call(&h, move |o| o.pull_events(&sub2, 20, 20)).await;
            let Ok(events) = events else { break };
            for e in events {
                // L'état initial annoncé à l'abonnement n'est pas un sabotage qui vient d'arriver.
                if e.initial() {
                    continue;
                }
                match e.tamper() {
                    Some(true) => {
                        hub.report_tamper(&id, Source::Device, true, format!("la caméra signale un sabotage ({})", e.topic.rsplit('/').next().unwrap_or("Tamper")));
                        hub.emit(Event::Cameras);
                    }
                    Some(false) => {
                        hub.report_tamper(&id, Source::Device, false, "la caméra ne signale plus de sabotage".into());
                        hub.emit(Event::Cameras);
                    }
                    None => {}
                }
            }
            if last_renew.elapsed() >= Duration::from_secs((TERM / 2) as u64) {
                let sub2 = sub.clone();
                if onvif_call(&h, move |o| o.renew_events(&sub2, TERM)).await.is_err() {
                    break; // l'abonnement a expiré : on en rouvre un
                }
                last_renew = Instant::now();
            }
        }
    }
}

async fn run_camera(hub: Arc<Hub>, h: Arc<CameraHandle>, mut rx: mpsc::Receiver<CamCmd>) {
    tokio::spawn(refresh_onvif(hub.clone(), h.clone()));
    let id = h.id();
    let key = format!("cam:{id}:live");
    let mut backoff = Duration::from_secs(2);
    let mut last_error: Option<String> = None;
    let mut next_probe = Instant::now();
    loop {
        let Some(q) = h.target() else {
            // Plus personne ne regarde ni n'analyse : une erreur de flux n'est plus un problème.
            h.state.write().unwrap().failing_since = None;
            if Instant::now() >= next_probe {
                probe(&hub, &h).await;
                next_probe = Instant::now() + PROBE_EVERY;
            }
            tokio::select! {
                _ = h.wake.notified() => {}
                _ = tokio::time::sleep_until(next_probe.into()) => {}
                cmd = rx.recv() => match cmd {
                    Some(CamCmd::Restart) => next_probe = Instant::now(),
                    Some(CamCmd::Shutdown) | None => return,
                },
            }
            continue;
        };
        let cfg = h.cfg.read().unwrap().clone();
        if cfg.password.is_empty() {
            h.state.write().unwrap().error = Some("code de sécurité manquant : renseigne-le dans les réglages de la caméra".into());
            hub.emit(Event::Cameras);
            tokio::select! {
                _ = sleep(Duration::from_secs(2)) => {}
                cmd = rx.recv() => if !matches!(cmd, Some(CamCmd::Restart)) { return },
            }
            continue;
        }
        {
            let mut s = h.state.write().unwrap();
            (s.streaming, s.stats) = (Some(q), None);
        }
        h.got_frame.store(false, Ordering::Relaxed);
        hub.emit(Event::Cameras);
        let opened = Instant::now();
        // Ouvert pour l'affichage (action de l'utilisateur) ou seulement pour l'analyse IA (système).
        let (who, opened_msg, closed_msg) = if h.ui_wants() {
            (Source::User, "Vidéo en direct ouverte", "Vidéo en direct fermée")
        } else {
            (Source::System, "Flux ouvert pour l'analyse IA", "Flux de l'analyse IA fermé")
        };
        hub.log_keyed(Level::Info, who, &id, &key, format!("{opened_msg} (flux {})", q.label()));

        let sink: stream::FrameSink = {
            let (h, hub) = (h.clone(), hub.clone());
            Arc::new(move |f: Frame| {
                *h.frame.lock().unwrap() = Some(Arc::new(f));
                if !h.got_frame.swap(true, Ordering::Relaxed) {
                    let mut s = h.state.write().unwrap();
                    (s.online, s.error, s.failing_since, s.offline_since) = (Some(true), None, None, None);
                    drop(s);
                    hub.emit(Event::Cameras);
                }
                // L'interface n'est réveillée que si elle affiche cette caméra : un flux ouvert
                // seulement pour l'analyse IA ne la redessine pas à chaque image.
                if h.ui_wants() {
                    let waker = hub.frame_waker.read().unwrap().clone();
                    if let Some(w) = waker {
                        w();
                    }
                }
            })
        };
        let on_stats = {
            let (h, hub) = (h.clone(), hub.clone());
            move |s: StreamStats| {
                h.state.write().unwrap().stats = Some(s);
                hub.emit(Event::Cameras);
            }
        };
        let url = cfg.rtsp_url(q);
        // Piste son seulement si quelqu'un écoute (toi, ou l'écoute par IA) à l'ouverture du flux.
        let with_audio = h.audio.read().unwrap().is_some() || h.listen_tap.read().unwrap().is_some();
        h.session_audio.store(with_audio, Ordering::Relaxed);
        let audio: Option<stream::AudioSink> = with_audio.then(|| {
            let h = h.clone();
            Arc::new(move |s: &[f32], rate: u32| {
                let sink = h.audio.read().unwrap().clone();
                if let Some(a) = sink {
                    a(s, rate);
                }
                let tap = h.listen_tap.read().unwrap().clone();
                if let Some(t) = tap {
                    t(s, rate);
                }
            }) as stream::AudioSink
        });
        let outcome = tokio::select! {
            r = stream::run_with_audio(&url, &cfg.username, &cfg.password, sink, on_stats, audio) => Some(r),
            _ = h.until_target_changes(q) => None,
            cmd = rx.recv() => match cmd {
                Some(CamCmd::Restart) => None,
                Some(CamCmd::Shutdown) | None => {
                    hub.log(Level::Info, who, &id, format!("{closed_msg} (flux {}, {})", q.label(), kinds::fmt_secs(opened.elapsed().as_secs())));
                    return;
                }
            },
        };
        h.state.write().unwrap().streaming = None;
        let had_frames = h.got_frame.load(Ordering::Relaxed);
        hub.log(Level::Info, who, &id, format!("{closed_msg} (flux {}, {})", q.label(), kinds::fmt_secs(opened.elapsed().as_secs())));
        if had_frames {
            backoff = Duration::from_secs(2);
            last_error = None;
        }
        match outcome {
            None | Some(Ok(())) => {}
            Some(Err(e)) if e.downcast_ref::<stream::NeedsFfmpeg>().is_some() => {
                {
                    let mut s = h.state.write().unwrap();
                    s.main_unavailable = true;
                    s.notice = Some("Le flux HD de cette caméra est en H.265 : il faut FFmpeg pour l'afficher. En attendant, HomeSmart montre le flux fluide.".into());
                }
                hub.log(Level::Warning, Source::System, &id, "Flux HD en H.265, non décodable sans FFmpeg : affichage du flux fluide");
            }
            Some(Err(e)) => {
                let msg = stream::describe_error(&e);
                {
                    let mut s = h.state.write().unwrap();
                    s.error = Some(msg.clone());
                    s.failing_since.get_or_insert_with(Instant::now);
                }
                if last_error.as_ref() != Some(&msg) {
                    // Simple information : si les images ne reviennent pas, la détection des problèmes
                    // le signale (« Flux vidéo perdu »).
                    hub.log(Level::Info, Source::System, &id, format!("Vidéo indisponible : {msg}"));
                    last_error = Some(msg);
                }
                hub.emit(Event::Cameras);
                // Identifiants refusés ou caméra verrouillée : pas de nouvel essai automatique
                // (chaque essai raté prolonge le verrouillage). On attend « Réessayer » ou un
                // changement de réglages ; après un verrouillage, un seul essai au bout de 10 min.
                let pause = match stream::auth_refusal(&e) {
                    Some(stream::AuthRefusal::BadPassword) => None,
                    Some(stream::AuthRefusal::Locked) => Some(LOCKOUT_WAIT),
                    None => Some(backoff),
                };
                h.state.write().unwrap().waiting_retry = pause.is_none() || pause == Some(LOCKOUT_WAIT);
                hub.emit(Event::Cameras);
                tokio::select! {
                    _ = async {
                        match pause {
                            Some(p) => sleep(p).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {}
                    cmd = rx.recv() => match cmd {
                        Some(CamCmd::Restart) => {}
                        Some(CamCmd::Shutdown) | None => return,
                    },
                }
                h.state.write().unwrap().waiting_retry = false;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
        hub.emit(Event::Cameras);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directions() {
        assert_eq!(direction(-0.5, 0.0), "gauche");
        assert_eq!(direction(0.5, 0.5), "haut-droite");
        assert_eq!(direction(0.0, -1.0), "bas");
        assert_eq!(direction(0.0, 0.0), "immobile");
    }

    /// Caméra HomeSmart (fabriquée soi-même) : appairage par code, sans ONVIF, réglages
    /// journalisés, et suppression qui lui fait oublier ce HomeSmart.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn homesmart_camera_lifecycle() {
        use homesmart_cam::api::{App, Media, Shared};
        use homesmart_cam::config::{AgentConfig, Backend, Tools};
        use homesmart_cam_proto::{Caps, ProcStatus, VideoCodec};
        struct NoMedia;
        impl Media for NoMedia {
            fn restart_pipeline(&self) {}
            fn clients_changed(&self) {}
            fn status(&self) -> (ProcStatus, ProcStatus) {
                (ProcStatus { running: true, ..ProcStatus::default() }, ProcStatus { running: true, ..ProcStatus::default() })
            }
        }
        let dir = std::env::temp_dir().join(format!("homesmart-diycam-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let caps = Caps { max_width: 3840, max_height: 2160, max_fps: 30, codecs: vec![VideoCodec::H264, VideoCodec::H265], audio: true, sensor: Some("Sony IMX415".into()) };
        let agent = AgentConfig {
            id: "70c9324b2bdc".into(),
            setup_code: "K7PQ3XWD".into(),
            clients: Vec::new(),
            backend: Backend::Test,
            settings: caps.default_settings("Couloir"),
            video_device: None,
            audio_device: None,
            has_mic: None,
            caps: None,
            api_port: 0,
            rtsp_port: 18554,
            tools: Tools::default(),
            custom: Vec::new(),
        };
        let shared = Arc::new(Shared { cfg: Mutex::new(agent), path: dir.join("cam.json"), caps, board: "Radxa ROCK 4D".into(), started: Instant::now() });
        let app = App::new(shared.clone(), Arc::new(NoMedia), None);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { homesmart_cam::api::serve(listener, app).await });

        let hub = Hub::new(dir.join("homesmart.json")).unwrap();
        hub.start(false);
        let mut cfg = CameraConfig::new("127.0.0.1");
        cfg.room = Some("Entrée".into());
        // Mauvais code : refusé, journalisé, rien d'ajouté.
        let e = hub.pair_camera(cfg.clone(), port, "AAAA-AAAA".into(), Source::User).await.unwrap_err();
        assert!(format!("{e:#}").contains("incorrect"), "{e:#}");
        assert!(hub.cameras().is_empty());
        let v = hub.pair_camera(cfg.clone(), port, "k7pq-3xwd".into(), Source::User).await.unwrap();
        assert_eq!((v.brand, v.name.as_str(), v.rtsp_port), (Brand::HomeSmart, "Couloir", 18554));
        assert!(hub.camera_config(&v.id).unwrap().agent.is_some());
        // Une seconde fois : refusé avant même de demander le code à la caméra.
        assert!(hub.pair_camera(cfg, port, "K7PQ-3XWD".into(), Source::User).await.is_err());
        assert_eq!(shared.cfg.lock().unwrap().clients.len(), 1);
        // Pas d'ONVIF sur une caméra HomeSmart : ni erreur, ni orientation.
        let t0 = Instant::now();
        while hub.camera(&v.id).unwrap().ptz.is_none() {
            assert!(t0.elapsed() < Duration::from_secs(5), "état ONVIF jamais fixé");
            sleep(Duration::from_millis(50)).await;
        }
        let view = hub.camera(&v.id).unwrap();
        assert_eq!((view.ptz, view.onvif_error.as_deref()), (Some(false), None));

        let st = hub.camera_agent(&v.id).await.unwrap();
        assert_eq!(st.info.caps.sensor.as_deref(), Some("Sony IMX415"));
        let mut after = st.settings.clone();
        (after.main.fps, after.audio.enabled) = (25, false);
        hub.set_camera_agent_settings(&v.id, st.settings.clone(), after, Source::User).await.unwrap();
        assert!(!shared.cfg.lock().unwrap().settings.audio.enabled);
        let mut bad = st.settings.clone();
        bad.main.width = 7680;
        assert!(hub.set_camera_agent_settings(&v.id, st.settings, bad, Source::User).await.is_err());
        let log: Vec<String> = hub.alerts(50).iter().map(|a| a.message.clone()).collect();
        for want in ["Appairage de la caméra HomeSmart 127.0.0.1 refusé", "Caméra HomeSmart appairée", "Réglages de la caméra : flux HD à 25 i/s, micro coupé", "Réglages de la caméra refusés"] {
            assert!(log.iter().any(|m| m.contains(want)), "{want} : {log:#?}");
        }

        // Supprimée de HomeSmart : la caméra oublie ce HomeSmart.
        hub.remove_camera(&v.id, Source::User).unwrap();
        let t0 = Instant::now();
        while !shared.cfg.lock().unwrap().clients.is_empty() {
            assert!(t0.elapsed() < Duration::from_secs(5), "la caméra n'a pas oublié ce HomeSmart");
            sleep(Duration::from_millis(50)).await;
        }
        hub.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Mauvais code : un seul essai, puis plus rien tant qu'on ne demande pas « Réessayer »
    /// (sinon la caméra resterait verrouillée).
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn no_retry_on_bad_password() {
        let dir = std::env::temp_dir().join(format!("homesmart-locktest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sim = camera::sim::start("BONCODE1", 0).await.unwrap();
        let hub = Hub::new(dir.join("homesmart.json")).unwrap();
        hub.start(false);
        let mut cfg = CameraConfig::new("127.0.0.1");
        (cfg.rtsp_port, cfg.onvif_port, cfg.password) = (sim.rtsp_port, sim.onvif_port, "MAUVAIS1".into());
        let v = hub.add_camera(cfg, Source::User).unwrap();
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(7) {
            hub.camera_want(&v.id, Quality::Sub);
            sleep(Duration::from_millis(200)).await;
        }
        let view = hub.camera(&v.id).unwrap();
        assert!(view.waiting_retry, "doit attendre « Réessayer »");
        assert!(view.error.as_deref().unwrap_or("").contains("identifiants refusés"), "{:?}", view.error);
        assert_eq!(sim.state.auth_failures.load(Ordering::Relaxed), 1, "un seul essai raté");

        hub.camera_retry(&v.id, Source::User).unwrap();
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(3) {
            hub.camera_want(&v.id, Quality::Sub);
            sleep(Duration::from_millis(200)).await;
        }
        assert_eq!(sim.state.auth_failures.load(Ordering::Relaxed), 2, "un essai de plus après « Réessayer »");
        let log: Vec<String> = hub.alerts(50).iter().map(|a| a.message.clone()).collect();
        assert!(log.iter().any(|m| m.contains("Nouvelle tentative de connexion")), "{log:#?}");
        hub.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Caméra d'une autre marque ajoutée sans chemin RTSP : le chemin est demandé en ONVIF.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn rtsp_path_from_onvif() {
        let dir = std::env::temp_dir().join(format!("homesmart-uritest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sim = camera::sim::start("CODE1234", 0).await.unwrap();
        let hub = Hub::new(dir.join("homesmart.json")).unwrap();
        hub.start(false);
        let mut cfg = CameraConfig::new("127.0.0.1");
        (cfg.onvif_port, cfg.password, cfg.brand) = (sim.onvif_port, "CODE1234".into(), Brand::Generic);
        cfg.rtsp_port = 9; // faux port : c'est l'adresse annoncée en ONVIF qui doit le corriger
        let v = hub.add_camera(cfg, Source::User).unwrap();
        let t0 = Instant::now();
        while hub.camera_config(&v.id).unwrap().main_path.is_none() {
            assert!(t0.elapsed() < Duration::from_secs(10), "chemin non détecté");
            sleep(Duration::from_millis(100)).await;
        }
        let c = hub.camera_config(&v.id).unwrap();
        assert_eq!(c.main_path.as_deref(), Some("/cam/realmonitor?channel=1&subtype=0"));
        assert_eq!(c.sub_path.as_deref(), Some("/cam/realmonitor?channel=1&subtype=1"));
        assert_eq!(c.rtsp_port, sim.rtsp_port);
        // Et la vidéo arrive bien par ce chemin.
        let t0 = Instant::now();
        while hub.camera_frame(&v.id).is_none() {
            hub.camera_want(&v.id, Quality::Sub);
            assert!(t0.elapsed() < Duration::from_secs(15), "pas d'image");
            sleep(Duration::from_millis(100)).await;
        }
        hub.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Image : réglages lus, seuls les changements envoyés, et journalisés.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn imaging_settings() {
        let dir = std::env::temp_dir().join(format!("homesmart-imgtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sim = camera::sim::start("CODE1234", 0).await.unwrap();
        let hub = Hub::new(dir.join("homesmart.json")).unwrap();
        hub.start(false);
        let mut cfg = CameraConfig::new("127.0.0.1");
        (cfg.rtsp_port, cfg.onvif_port, cfg.password) = (sim.rtsp_port, sim.onvif_port, "CODE1234".into());
        let v = hub.add_camera(cfg, Source::User).unwrap();
        let (cur, opts) = hub.camera_imaging(&v.id).await.unwrap();
        assert_eq!(cur.brightness, Some(50.0));
        assert_eq!(opts.ir_cut, vec!["ON", "OFF", "AUTO"]);
        let mut new = cur.clone();
        (new.brightness, new.ir_cut, new.wdr) = (Some(70.0), Some("OFF".into()), Some("ON".into()));
        hub.camera_set_imaging(&v.id, cur, new, Source::User).await.unwrap();
        let i = sim.state.imaging.lock().unwrap().clone();
        assert_eq!((i.brightness, i.ir_cut.as_deref(), i.wdr.as_deref(), i.contrast), (Some(70.0), Some("OFF"), Some("ON"), Some(50.0)));
        let log: Vec<String> = hub.alerts(30).iter().map(|a| a.message.clone()).collect();
        assert!(log.iter().any(|m| m.contains("Image : luminosité 50 → 70, vision de nuit : nuit (infrarouge), WDR : activé")), "{log:#?}");
        hub.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Qualité vidéo : limites lues en ONVIF, changement appliqué au flux et journalisé.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn encoder_settings() {
        let dir = std::env::temp_dir().join(format!("homesmart-enctest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sim = camera::sim::start("CODE1234", 0).await.unwrap();
        let hub = Hub::new(dir.join("homesmart.json")).unwrap();
        hub.start(false);
        let mut cfg = CameraConfig::new("127.0.0.1");
        (cfg.rtsp_port, cfg.onvif_port, cfg.password, cfg.name) = (sim.rtsp_port, sim.onvif_port, "CODE1234".into(), "Bureau".into());
        let v = hub.add_camera(cfg, Source::User).unwrap();

        let list = hub.camera_encoders(&v.id).await.unwrap();
        let (q, main, opts) = list[0].clone();
        assert_eq!(q, Quality::Main);
        assert_eq!((main.width, main.height), (1280, 720));
        assert!(opts.resolutions.contains(&(2560, 1440)));
        assert_eq!(opts.fps, (1, 60));

        let new = EncoderConfig { width: 1920, height: 1080, fps: 25, bitrate_kbps: 4096, ..main.clone() };
        hub.camera_set_encoder(&v.id, Quality::Main, new, Source::User).await.unwrap();
        let e = sim.state.encoder(Quality::Main);
        assert_eq!((e.width, e.height, e.fps as u32, e.kbps), (1920, 1080, 25, 4096));
        // Une résolution que la caméra ne propose pas est refusée.
        let bad = EncoderConfig { width: 999, height: 111, ..main };
        assert!(hub.camera_set_encoder(&v.id, Quality::Main, bad, Source::User).await.is_err());

        let log: Vec<String> = hub.alerts(50).iter().map(|a| a.message.clone()).collect();
        assert!(log.iter().any(|m| m.contains("Qualité vidéo (flux HD) : 1280×720 → 1920×1080 · 12 → 25 i/s")), "{log:#?}");
        assert!(log.iter().any(|m| m.contains("Qualité vidéo (flux HD) refusée")), "{log:#?}");
        hub.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Caméra qui refuse le mouvement continu (comme certaines Tapo) : HomeSmart la tourne par
    /// petits pas tant que le bouton est appuyé, et s'arrête au relâchement.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn camera_without_continuous_move_turns_by_small_steps() {
        let dir = std::env::temp_dir().join(format!("homesmart-nudgetest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sim = camera::sim::start("CODE1234", 0).await.unwrap();
        sim.state.relative_only.store(true, Ordering::Relaxed);
        let hub = Hub::new(dir.join("homesmart.json")).unwrap();
        let mut cfg = CameraConfig::new("127.0.0.1");
        (cfg.rtsp_port, cfg.onvif_port, cfg.password, cfg.name) = (sim.rtsp_port, sim.onvif_port, "CODE1234".into(), "Tapo".into());
        let v = hub.add_camera(cfg, Source::User).unwrap();
        let x0 = sim.state.position().0;
        hub.camera_move(&v.id, 1.0, 0.0, Source::User).await.unwrap();
        sleep(Duration::from_millis(1000)).await;
        let x1 = sim.state.position().0;
        assert!(x1 - x0 > 0.1, "la caméra a tourné vers la droite par petits pas : {x0} → {x1}");
        hub.camera_stop(&v.id, Source::User).await.unwrap();
        // Le dernier pas se termine, puis plus rien ne bouge.
        sleep(Duration::from_millis(400)).await;
        let x2 = sim.state.position().0;
        sleep(Duration::from_millis(700)).await;
        assert!((sim.state.position().0 - x2).abs() < 1e-3, "plus aucun pas après l'arrêt");
        // Les ordres suivants vont directement aux petits pas, sans réessayer le mouvement continu.
        hub.camera_move(&v.id, -1.0, 0.0, Source::User).await.unwrap();
        sleep(Duration::from_millis(700)).await;
        hub.camera_stop(&v.id, Source::User).await.unwrap();
        assert!(sim.state.position().0 < x2, "retour vers la gauche");
        let log: Vec<String> = hub.alerts(50).iter().map(|a| a.message.clone()).collect();
        assert!(log.iter().any(|m| m.contains("orientation par petits pas")), "{log:#?}");
        assert!(!log.iter().any(|m| m.contains("refusé")), "aucun refus au journal : {log:#?}");
        hub.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Hub complet avec une caméra simulée : direct, orientation, positions, capture, journal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn camera_in_hub() {
        let dir = std::env::temp_dir().join(format!("homesmart-camtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("HOMESMART_CAPTURES", dir.join("captures"));
        let sim = camera::sim::start("CODE1234", 0).await.unwrap();
        let hub = Hub::new(dir.join("homesmart.json")).unwrap();
        hub.start(false);
        let mut cfg = CameraConfig::new("127.0.0.1");
        (cfg.rtsp_port, cfg.onvif_port, cfg.password, cfg.name) = (sim.rtsp_port, sim.onvif_port, "CODE1234".into(), "Salon".into());
        let v = hub.add_camera(cfg.clone(), Source::User).unwrap();
        assert!(hub.add_camera(cfg, Source::User).is_err(), "doublon refusé");

        // Le flux s'ouvre quand on le demande, et produit des images.
        let t0 = Instant::now();
        while hub.camera_frame(&v.id).is_none() {
            hub.camera_want(&v.id, Quality::Sub);
            assert!(t0.elapsed() < Duration::from_secs(20), "pas d'image");
            sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(hub.camera(&v.id).unwrap().streaming, Some(Quality::Sub));
        // ONVIF : modèle lu, orientable, positions de la caméra.
        let t0 = Instant::now();
        while hub.camera(&v.id).unwrap().ptz.is_none() {
            assert!(t0.elapsed() < Duration::from_secs(10));
            sleep(Duration::from_millis(100)).await;
        }
        let view = hub.camera(&v.id).unwrap();
        assert_eq!(view.ptz, Some(true));
        assert_eq!(view.model.as_deref(), Some(camera::sim::MODEL));
        assert!(view.presets.iter().any(|p| p.name == "Entrée"));

        hub.camera_move(&v.id, -1.0, 0.0, Source::User).await.unwrap();
        hub.camera_move(&v.id, 1.0, 0.0, Source::User).await.unwrap();
        hub.camera_stop(&v.id, Source::User).await.unwrap();
        hub.camera_save_preset(&v.id, "Fenêtre", Source::User).await.unwrap();
        let shot = hub.camera_snapshot(&v.id, Source::User).await.unwrap();
        assert!(std::fs::metadata(&shot).unwrap().len() > 10_000);

        // Plus demandé : le flux se ferme tout seul.
        let t0 = Instant::now();
        while hub.camera(&v.id).unwrap().streaming.is_some() {
            assert!(t0.elapsed() < Duration::from_secs(8), "le flux reste ouvert");
            sleep(Duration::from_millis(200)).await;
        }
        hub.update_camera(&v.id, CameraPatch { password: Some("autre".into()), ..Default::default() }, Source::User).unwrap();
        hub.remove_camera(&v.id, Source::User).unwrap();

        let log: Vec<String> = hub.alerts(100).iter().map(|a| a.message.clone()).collect();
        let has = |s: &str| log.iter().any(|m| m.contains(s));
        for s in ["Caméra ajoutée", "Vidéo en direct ouverte", "Vidéo en direct fermée", "Orientation : gauche → droite", "Position « Fenêtre » mémorisée", "Capture enregistrée", "mot de passe modifié", "Caméra supprimée"] {
            assert!(has(s), "absent du journal : {s}\n{log:#?}");
        }
        assert!(!log.iter().any(|m| m.contains("autre")), "le mot de passe ne doit jamais apparaître");
        hub.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
