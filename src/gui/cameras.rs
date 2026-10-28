//! Vue « Caméras » : mosaïque en direct, vue détaillée (orientation, positions, captures,
//! réglages), ajout guidé avec test de connexion et recherche sur le réseau.
//!
//! Le flux d'une caméra n'est demandé au hub que lorsqu'elle est affichée : quitter la vue
//! ferme les flux au bout de quelques secondes, et plus rien n'est décodé.

use super::icons::{self, Icon};
use super::{fx, theme, widgets, App, Toast};
use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Id, Layout, Margin, Rect, RichText, ScrollArea, Sense, Stroke,
    StrokeKind, Ui,
};
use homesmart::camera::discover::Found;
use homesmart::camera::{Brand, CameraConfig, Frame, Quality};
use homesmart::camera::onvif::{ImagingOptions, ImagingSettings};
use homesmart::hub::{CameraPatch, CameraView, EncoderConfig, EncoderOptions, Source, TestReport};
use std::collections::HashMap;
use std::f32::consts::PI;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

mod agent_ui;
mod protect_ui;
mod recordings;
mod vision_ui;
mod zones_ui;
pub(super) mod zoom;

/// En dessous de cette largeur, les commandes d'une caméra passent sous la vidéo.
const STACK_WIDTH: f32 = 820.0;

/// Ordre d'orientation, exécuté dans l'ordre pour chaque caméra (un arrêt ne doit jamais doubler un départ).
#[derive(Debug, Clone, PartialEq)]
enum PtzCmd {
    Move(f32, f32),
    Stop,
    Home,
    Goto(String),
}

/// Orientation tenue (pavé, flèches du clavier). Pendant une image, chacun propose une direction ;
/// une seule décision est prise en fin d'image, et un ordre ne part que si la direction change.
/// Un appui maintenu donne donc un seul « Move », puis un seul « Stop » au relâchement, quel que
/// soit le nombre d'images et l'ordre dans lequel pavé et clavier sont lus.
#[derive(Default)]
pub(super) struct PtzDriver {
    /// Mouvement en cours : (caméra, pan, tilt).
    moving: Option<(String, f32, f32)>,
    /// Proposition de l'image en cours : (caméra, direction) ; une direction l'emporte sur « rien ».
    proposed: Option<(String, Option<(f32, f32)>)>,
}

impl PtzDriver {
    fn propose(&mut self, id: &str, want: Option<(f32, f32)>) {
        if self.proposed.as_ref().is_some_and(|(_, w)| w.is_some()) {
            return;
        }
        self.proposed = Some((id.to_string(), want));
    }

    /// Fin d'image : les ordres à envoyer, dans l'ordre (aucun si rien ne change). Sans
    /// proposition pendant l'image (caméra quittée, fenêtre changée), le mouvement s'arrête.
    fn commit(&mut self) -> Vec<(String, PtzCmd)> {
        let (id, want) = self.proposed.take().unwrap_or_default();
        let mut out = Vec::new();
        match (self.moving.take(), want) {
            (Some((cid, p, t)), Some((np, nt))) if cid == id && p == np && t == nt => self.moving = Some((cid, p, t)),
            (cur, Some((np, nt))) => {
                if let Some((cid, ..)) = cur.filter(|c| c.0 != id) {
                    out.push((cid, PtzCmd::Stop));
                }
                out.push((id.clone(), PtzCmd::Move(np, nt)));
                self.moving = Some((id, np, nt));
            }
            (Some((cid, ..)), None) => out.push((cid, PtzCmd::Stop)),
            (None, None) => {}
        }
        out
    }
}

#[derive(Default)]
pub(super) struct CamUi {
    textures: HashMap<String, (egui::TextureHandle, Arc<Frame>)>,
    shown: HashMap<String, ShownStats>,
    /// Réglages d'image de la caméra affichée (chargés à l'ouverture de « Image »).
    img: Arc<Mutex<ImgState>>,
    /// Réglages d'encodage de la caméra affichée (chargés à l'ouverture de « Qualité vidéo »).
    enc: Arc<Mutex<EncState>>,
    /// Caméra HomeSmart affichée : état et réglages lus sur son agent.
    agent: agent_ui::AgentUi,
    pub selected: Option<String>,
    pub hd: bool,
    /// Vidéo seule, sans le panneau de commandes.
    pub wide: bool,
    pub add: Option<AddCam>,
    settings: Option<CamSettings>,
    pub scan: Arc<Mutex<ScanState>>,
    auto_scanned: bool,
    /// Son en cours d'écoute : la sortie audio et la caméra écoutée.
    player: Option<homesmart::camera::audio::Player>,
    audio_cam: Option<String>,
    pub volume: f32,
    /// Largeur de la colonne des commandes (réglable avec la poignée).
    side_width: f32,
    /// « Qualité vidéo » / « Image » ouvertes d'office (captures d'écran).
    pub open_quality: bool,
    pub open_image: bool,
    /// Vue « Enregistrements » (carte SD) au lieu du direct.
    pub rec_mode: bool,
    /// Défilement jusqu'aux scénarios (captures d'écran).
    pub open_scenarios: bool,
    /// Onglet de la colonne : 0 commandes, 1 vision IA, 2 réglages.
    pub side_tab: usize,
    /// Orientation tenue : une décision par image (pavé et clavier).
    ptz_drive: PtzDriver,
    pub speed: f32,
    preset_name: String,
    ptz: HashMap<String, mpsc::UnboundedSender<PtzCmd>>,
    last_capture: Arc<Mutex<Option<PathBuf>>>,
    confirm_delete: bool,
    /// Relecture des enregistrements de la carte SD (lecture seule).
    pub rec: recordings::RecUi,
    /// Vision par IA (réglages, enregistrement des visages).
    pub vision: vision_ui::VisionUi,
    /// Sabotage et réglages Tapo (alarme, suivi, carte SD).
    pub protect: protect_ui::ProtectUi,
    /// Zones de la vision IA (liste, dessin sur la vidéo).
    pub zones: zones_ui::ZonesUi,
    /// Mesure de l'orientation (HOMESMART_PTZ_LOG = fichier) : commandes envoyées, horodatées.
    pub ptz_log: Option<PtzLog>,
    /// Appui simulé sur l'orientation (mesures), et place du pavé à l'écran à l'image précédente.
    pub ptz_script: Option<PtzScript>,
    pad_rect: Option<Rect>,
    /// Zoom numérique de chaque vidéo affichée (caméra, ou « id~pb » pour la relecture).
    pub zoom: HashMap<String, zoom::Zoom>,
    /// HD prise par le zoom : caméra, et `true` tant qu'elle tient (`false` : l'utilisateur a
    /// repris la main avec le bouton HD ; plus de bascule automatique avant le retour à 1×).
    auto_hd: Option<(String, bool)>,
    /// Gestes de zoom simulés (captures d'écran), et place de l'image du direct à l'image précédente.
    pub zoom_script: Option<zoom::ZoomScript>,
    zoom_area: Option<Rect>,
}

/// Journal des ordres d'orientation envoyés (mesures), écrit à la fermeture.
pub(super) struct PtzLog {
    path: PathBuf,
    start: Instant,
    lines: Vec<String>,
}

impl PtzLog {
    pub fn from_env() -> Option<Self> {
        std::env::var_os("HOMESMART_PTZ_LOG").map(|p| PtzLog { path: PathBuf::from(p), start: Instant::now(), lines: Vec::new() })
    }

    fn note(&mut self, what: &str) {
        self.lines.push(format!("{} {what}", self.start.elapsed().as_millis()));
    }

    pub fn write(&self) {
        if let Err(e) = std::fs::write(&self.path, self.lines.join("\n") + "\n") {
            eprintln!("journal d'orientation impossible à écrire : {e}");
        }
    }
}

/// Appui simulé de 3 s vers la droite, à la souris sur le pavé ou avec la flèche du clavier
/// (HOMESMART_SELECT=cam:Nom#ptz-souris ou #ptz-clavier).
pub(super) struct PtzScript {
    mouse: bool,
    start: Instant,
    pressed: bool,
    released: bool,
    last_key: Instant,
}

impl PtzScript {
    pub fn new(mouse: bool) -> Self {
        PtzScript { mouse, start: Instant::now(), pressed: false, released: false, last_key: Instant::now() }
    }
}

impl CamUi {
    pub fn new() -> Self {
        CamUi { speed: 0.6, volume: 1.0, side_width: 360.0, ptz_log: PtzLog::from_env(), ..Default::default() }
    }
}

/// Images réellement affichées (cadence et coût de préparation côté interface).
#[derive(Default)]
pub(super) struct ShownStats {
    window: Option<Instant>,
    count: u32,
    busy: Duration,
    pub fps: f32,
    pub prepare_ms: f32,
}

impl ShownStats {
    fn record(&mut self, took: Duration) {
        let start = *self.window.get_or_insert_with(Instant::now);
        self.count += 1;
        self.busy += took;
        let elapsed = start.elapsed();
        if elapsed >= Duration::from_secs(1) {
            self.fps = self.count as f32 / elapsed.as_secs_f32();
            self.prepare_ms = self.busy.as_secs_f32() * 1000.0 / self.count.max(1) as f32;
            (self.window, self.count, self.busy) = (Some(Instant::now()), 0, Duration::ZERO);
        }
    }
}

/// Résolutions proposées en boutons : les standards (480p à 1440p) et celles que la caméra
/// propose en plus ; `ok` = accepté par la caméra.
fn resolution_choices(available: &[(u32, u32)]) -> Vec<(String, u32, u32, bool)> {
    let name = |w: u32, h: u32| match h {
        1440 => "1440p (2K)".to_string(),
        1296 => "1296p (3 MP)".to_string(),
        2160 => "2160p (4K)".to_string(),
        _ if w * 3 == h * 4 && h < 1000 => format!("{h}p (4:3)"),
        _ => format!("{h}p"),
    };
    let mut out: Vec<(String, u32, u32, bool)> = Vec::new();
    for (w, h) in [(640u32, 480u32), (1280, 720), (1920, 1080), (2560, 1440)] {
        // Même hauteur proposée par la caméra (ex. 640×480 ou 704×480) : on prend la sienne.
        match available.iter().find(|(_, ah)| *ah == h) {
            Some(&(aw, ah)) => out.push((name(aw, ah), aw, ah, true)),
            None => out.push((name(w, h), w, h, false)),
        }
    }
    for &(w, h) in available {
        if !out.iter().any(|(_, ow, oh, _)| (*ow, *oh) == (w, h)) {
            out.push((name(w, h), w, h, true));
        }
    }
    out.sort_by_key(|(_, w, h, _)| (h * w, *h));
    out
}

/// Image opaque → texture : simple recopie (pas de calcul d'alpha).
fn color_image(f: &Frame) -> egui::ColorImage {
    let pixels = f.rgba.chunks_exact(4).map(|p| Color32::from_rgba_premultiplied(p[0], p[1], p[2], 255)).collect();
    egui::ColorImage::new([f.width, f.height], pixels)
}

#[derive(Default)]
struct ImgState {
    id: String,
    loading: bool,
    current: ImagingSettings,
    edit: ImagingSettings,
    options: ImagingOptions,
    error: Option<String>,
}

#[derive(Default)]
struct EncState {
    id: String,
    loading: bool,
    list: Vec<(Quality, EncoderConfig, EncoderOptions)>,
    /// Valeurs en cours de modification, dans le même ordre que `list`.
    edits: Vec<EncoderConfig>,
    error: Option<String>,
}

#[derive(Default)]
pub(super) struct ScanState {
    pub running: bool,
    pub done: bool,
    pub results: Vec<Found>,
}

pub(super) struct AddCam {
    pub host: String,
    pub name: String,
    pub room: String,
    pub user: String,
    pub password: String,
    show_pw: bool,
    pub brand: Brand,
    rtsp_port: String,
    onvif_port: String,
    main_path: String,
    sub_path: String,
    model: Option<String>,
    serial: Option<String>,
    mac: Option<String>,
    /// Tapo sans compte caméra (vidéo et ONVIF encore fermés).
    needs_account: bool,
    /// Caméra HomeSmart : port de son agent (le code d'appairage se tape dans `password`).
    agent_port: String,
    error: Option<String>,
    test: Arc<Mutex<Option<Option<TestReport>>>>,
    /// Appairage d'une caméra HomeSmart : en cours (`Some(None)`), puis son résultat.
    pairing: Arc<Mutex<Option<Option<Result<CameraView, String>>>>>,
}

impl Default for AddCam {
    fn default() -> Self {
        AddCam {
            host: String::new(),
            name: String::new(),
            room: String::new(),
            user: "admin".into(),
            password: String::new(),
            show_pw: false,
            brand: Brand::Imou,
            rtsp_port: "554".into(),
            onvif_port: "80".into(),
            main_path: String::new(),
            sub_path: String::new(),
            model: None,
            serial: None,
            mac: None,
            needs_account: false,
            agent_port: homesmart_cam_proto::API_PORT.to_string(),
            error: None,
            test: Arc::default(),
            pairing: Arc::default(),
        }
    }
}

impl AddCam {
    pub fn from_found(f: &Found) -> Self {
        let mut a = AddCam { host: f.ip.clone(), model: f.model.clone(), serial: f.serial.clone(), mac: f.mac.clone(), needs_account: f.needs_tapo_account(), ..AddCam::default() };
        a.set_brand(f.brand);
        if let Some(p) = f.onvif_port {
            a.onvif_port = p.to_string();
        }
        if let Some(p) = f.rtsp_port {
            a.rtsp_port = p.to_string();
        }
        if let Some(p) = f.agent_port {
            a.agent_port = p.to_string();
        }
        // Caméra HomeSmart : son nom est annoncé avec elle.
        if f.brand == Brand::HomeSmart {
            a.name = f.vendor.clone().unwrap_or_default();
        }
        a
    }

    /// Change de marque : l'utilisateur et les ports suivent, s'ils n'ont pas été modifiés.
    pub fn set_brand(&mut self, b: Brand) {
        let old = self.brand;
        if self.user.trim() == old.default_user() {
            self.user = b.default_user().to_string();
        }
        if self.onvif_port.trim() == old.default_onvif_port().to_string() {
            self.onvif_port = b.default_onvif_port().to_string();
        }
        if self.rtsp_port.trim() == old.default_rtsp_port().to_string() {
            self.rtsp_port = b.default_rtsp_port().to_string();
        }
        self.brand = b;
    }

    /// Code d'appairage complet (8 caractères) pour une caméra HomeSmart.
    fn code_ok(&self) -> bool {
        homesmart_cam_proto::normalize_code(&self.password).len() == homesmart_cam_proto::CODE_LEN
    }

    fn config(&self) -> CameraConfig {
        let mut c = CameraConfig::new(&self.host);
        c.name = self.name.trim().to_string();
        c.room = Some(self.room.trim().to_string()).filter(|s| !s.is_empty());
        c.username = self.user.trim().to_string();
        c.password = self.password.trim().to_string();
        c.brand = self.brand;
        c.rtsp_port = self.rtsp_port.trim().parse().unwrap_or(554);
        c.onvif_port = self.onvif_port.trim().parse().unwrap_or(80);
        c.main_path = Some(self.main_path.trim().to_string()).filter(|s| !s.is_empty());
        c.sub_path = Some(self.sub_path.trim().to_string()).filter(|s| !s.is_empty());
        (c.model, c.serial, c.mac) = (self.model.clone(), self.serial.clone(), self.mac.clone());
        c
    }
}

struct CamSettings {
    id: String,
    name: String,
    room: String,
    host: String,
    user: String,
    password: String,
    show_pw: bool,
    brand: Brand,
    rtsp_port: String,
    onvif_port: String,
    main_path: String,
    sub_path: String,
    invert_tilt: bool,
    invert_pan: bool,
}

impl CamSettings {
    fn of(c: &CameraConfig) -> Self {
        CamSettings {
            id: c.id.clone(),
            name: c.name.clone(),
            room: c.room.clone().unwrap_or_default(),
            host: c.host.clone(),
            user: c.username.clone(),
            password: c.password.clone(),
            show_pw: false,
            brand: c.brand,
            rtsp_port: c.rtsp_port.to_string(),
            onvif_port: c.onvif_port.to_string(),
            main_path: c.main_path.clone().unwrap_or_default(),
            sub_path: c.sub_path.clone().unwrap_or_default(),
            invert_tilt: c.invert_tilt,
            invert_pan: c.invert_pan,
        }
    }

    fn patch(&self) -> CameraPatch {
        CameraPatch {
            name: Some(self.name.clone()),
            room: Some(self.room.clone()),
            host: Some(self.host.clone()),
            username: Some(self.user.clone()),
            password: Some(self.password.trim().to_string()),
            brand: Some(self.brand),
            rtsp_port: self.rtsp_port.trim().parse().ok(),
            onvif_port: self.onvif_port.trim().parse().ok(),
            main_path: Some(self.main_path.clone()),
            sub_path: Some(self.sub_path.clone()),
            invert_tilt: Some(self.invert_tilt),
            invert_pan: Some(self.invert_pan),
        }
    }
}

/// Direction donnée par les flèches du clavier (x vers la droite, y vers le bas), si aucun champ
/// de saisie n'a le focus.
fn keyboard_dir(ui: &Ui) -> Option<(f32, f32)> {
    if ui.ctx().memory(|m| m.focused().is_some()) {
        return None;
    }
    let (l, r, u, d) = ui.input(|i| (i.key_down(egui::Key::ArrowLeft), i.key_down(egui::Key::ArrowRight), i.key_down(egui::Key::ArrowUp), i.key_down(egui::Key::ArrowDown)));
    let x = r as i32 as f32 - l as i32 as f32;
    let y = d as i32 as f32 - u as i32 as f32;
    (x != 0.0 || y != 0.0).then_some((x, y))
}

/// Ajuste un rectangle au format de l'image (bandes noires si besoin).
fn fit(rect: Rect, aspect: f32) -> Rect {
    let w = rect.width().min(rect.height() * aspect);
    Rect::from_center_size(rect.center(), vec2(w, w / aspect))
}

fn status_of(c: &CameraView, live: bool) -> (Color32, String) {
    if live {
        (theme::accent(), "En direct".into())
    } else if let Some(e) = &c.error {
        (theme::crit(), e.clone())
    } else if c.streaming.is_some() {
        (theme::warn(), "Connexion…".into())
    } else {
        match c.online {
            Some(true) => (theme::accent(), "Joignable".into()),
            Some(false) => (theme::crit(), "Injoignable".into()),
            None => (theme::faint(), "Vérification…".into()),
        }
    }
}

impl App {
    /// Envoie un ordre d'orientation ; les ordres d'une même caméra partent dans l'ordre.
    fn ptz(&mut self, id: &str, cmd: PtzCmd) {
        if let Some(log) = &mut self.cams.ptz_log {
            let what = match &cmd {
                PtzCmd::Move(p, t) => format!("Move {p:+.2} {t:+.2}"),
                PtzCmd::Stop => "Stop".to_string(),
                PtzCmd::Home => "Home".to_string(),
                PtzCmd::Goto(t) => format!("Goto {t}"),
            };
            log.note(&what);
        }
        let tx = self.cams.ptz.entry(id.to_string()).or_insert_with(|| {
            let (tx, mut rx) = mpsc::unbounded_channel::<PtzCmd>();
            let (hub, toasts, ctx, id) = (self.hub.clone(), self.toasts.clone(), self.ctx.clone(), id.to_string());
            self.rt.spawn(async move {
                while let Some(cmd) = rx.recv().await {
                    let r = match cmd {
                        PtzCmd::Move(p, t) => hub.camera_move(&id, p, t, Source::User).await,
                        PtzCmd::Stop => hub.camera_stop(&id, Source::User).await,
                        PtzCmd::Home => hub.camera_home(&id, Source::User).await,
                        PtzCmd::Goto(t) => hub.camera_goto_preset(&id, &t, Source::User).await,
                    };
                    if let Err(e) = r {
                        toasts.lock().unwrap().push(Toast { text: format!("Orientation : {e:#}"), error: true, at: Instant::now() });
                        ctx.request_repaint();
                    }
                }
            });
            tx
        });
        let _ = tx.send(cmd);
    }

    /// Fin d'image : envoie la seule décision d'orientation de l'image (voir `PtzDriver`).
    pub(super) fn ptz_commit(&mut self) {
        for (id, cmd) in self.cams.ptz_drive.commit() {
            self.ptz(&id, cmd);
        }
    }

    /// Texture de la dernière image de la caméra (mise à jour seulement si l'image a changé).
    fn cam_texture(&mut self, id: &str) -> Option<(egui::TextureId, [usize; 2])> {
        let frame = self.hub.camera_frame(id)?;
        Some(self.texture_cached(id.to_string(), frame))
    }

    /// Texture de la dernière image de relecture (carte SD), séparée du direct.
    fn playback_texture(&mut self, id: &str) -> Option<(egui::TextureId, [usize; 2])> {
        let frame = self.hub.camera_playback_frame(id)?;
        Some(self.texture_cached(format!("{id}~pb"), frame))
    }

    /// Met une image en cache sous `key` et renvoie sa texture (réutilisée si l'image n'a pas changé).
    fn texture_cached(&mut self, key: String, frame: Arc<Frame>) -> (egui::TextureId, [usize; 2]) {
        if let Some((tex, last)) = self.cams.textures.get(&key) {
            if Arc::ptr_eq(last, &frame) {
                return (tex.id(), tex.size());
            }
        }
        let started = Instant::now();
        let image = color_image(&frame);
        let out = match self.cams.textures.get_mut(&key) {
            Some((tex, last)) => {
                tex.set(image, egui::TextureOptions::LINEAR);
                *last = frame;
                (tex.id(), tex.size())
            }
            None => {
                let tex = self.ctx.load_texture(format!("cam-{key}"), image, egui::TextureOptions::LINEAR);
                let out = (tex.id(), tex.size());
                self.cams.textures.insert(key.clone(), (tex, frame));
                out
            }
        };
        self.cams.shown.entry(key).or_default().record(started.elapsed());
        out
    }

    /// Tuile d'une caméra (page Maison) : vignette en direct et nom ; clic → la caméra ouverte.
    pub(super) fn camera_card(&mut self, ui: &mut Ui, c: &CameraView, size: egui::Vec2) {
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
        self.hub.camera_want(&c.id, Quality::Sub);
        let tex = self.cam_texture(&c.id);
        let live = c.streaming.is_some() && tex.is_some() && c.error.is_none();
        self.paint_cam_tile(ui, rect, c, tex, live, 22, resp.hovered());
        if resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        if resp.clicked() {
            self.open_camera(&c.id);
        }
    }

    /// Ouvre la vue d'une caméra (page Caméras, colonne Surveillance).
    pub(super) fn open_camera(&mut self, id: &str) {
        self.view = super::View::Cameras;
        self.cams.selected = Some(id.to_string());
        (self.cams.settings, self.cams.confirm_delete, self.cams.side_tab) = (None, false, 0);
    }

    /// Vignette d'une caméra : l'image remplit le cadre (rognée au centre), un dégradé en bas et
    /// le nom avec un point lagon s'il est en direct ; sinon l'état (connexion, injoignable…).
    fn paint_cam_tile(&self, ui: &Ui, rect: Rect, c: &CameraView, tex: Option<(egui::TextureId, [usize; 2])>, live: bool, radius: u8, hovered: bool) {
        let p = ui.painter_at(rect);
        p.rect_filled(rect, CornerRadius::same(radius), Color32::from_rgb(8, 10, 14));
        if let Some((id, [w, hgt])) = tex {
            let img = w as f32 / hgt.max(1) as f32;
            let frame = rect.width() / rect.height();
            let uv = if img > frame {
                let k = frame / img;
                Rect::from_min_max(pos2((1.0 - k) / 2.0, 0.0), pos2((1.0 + k) / 2.0, 1.0))
            } else {
                let k = img / frame;
                Rect::from_min_max(pos2(0.0, (1.0 - k) / 2.0), pos2(1.0, (1.0 + k) / 2.0))
            };
            p.add(egui::epaint::RectShape::filled(rect, CornerRadius::same(radius), Color32::WHITE).with_texture(id, uv));
            if !live {
                p.rect_filled(rect, CornerRadius::same(radius), Color32::from_black_alpha(130));
            }
        } else {
            icons::paint(&p, Rect::from_center_size(rect.center() - vec2(0.0, 12.0), vec2(30.0, 30.0)), Icon::Camera, Color32::from_gray(120));
        }
        // Dégradé sombre en bas pour lire le nom : bord haut droit et transparent, coins du bas
        // arrondis comme la vignette.
        let r = radius as f32;
        let top = rect.bottom() - rect.height().min(76.0);
        let mid = rect.bottom() - r;
        let mut mesh = egui::Mesh::default();
        mesh.colored_vertex(pos2(rect.left(), top), Color32::TRANSPARENT);
        mesh.colored_vertex(pos2(rect.right(), top), Color32::TRANSPARENT);
        mesh.colored_vertex(pos2(rect.right(), mid), Color32::from_black_alpha(175));
        mesh.colored_vertex(pos2(rect.left(), mid), Color32::from_black_alpha(175));
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        p.add(egui::Shape::mesh(mesh));
        let foot = Rect::from_min_max(pos2(rect.left(), mid), rect.right_bottom());
        p.rect_filled(foot, CornerRadius { nw: 0, ne: 0, sw: radius, se: radius }, Color32::from_black_alpha(175));
        let x = rect.left() + 16.0;
        let y = rect.bottom() - 16.0;
        if live {
            p.circle_filled(pos2(x + 4.0, y - 8.0), 7.0, theme::alpha(theme::accent(), 0.3));
            p.circle_filled(pos2(x + 4.0, y - 8.0), 4.0, theme::accent());
        }
        let name_x = if live { x + 16.0 } else { x };
        let name_w = p.text(pos2(name_x, y - 8.0), Align2::LEFT_CENTER, &c.name, theme::bold(15.0), Color32::WHITE).width();
        if !live {
            let (_, text) = status_of(c, false);
            let msg = if c.streaming.is_some() && c.error.is_none() { "Connexion…".to_string() } else { text };
            // À droite du nom, coupé avec « … » s'il manque de place (une erreur de flux peut être longue).
            let room = rect.right() - 14.0 - (name_x + name_w + 12.0);
            if room > 30.0 {
                let ink = Color32::from_white_alpha(210);
                let g = super::one_line(&p, &msg, FontId::proportional(12.0), ink, room);
                p.galley(pos2(rect.right() - 14.0 - g.size().x, y - 8.0 - g.size().y / 2.0), g, ink);
            }
        }
        if hovered {
            p.rect_stroke(rect, CornerRadius::same(radius), Stroke::new(2.0, theme::alpha(theme::accent(), 0.8)), StrokeKind::Inside);
        }
    }

    /// Image en direct (ou dernière image connue, assombrie, avec ce qui se passe au centre),
    /// zoomable (voir `zoom`). Renvoie aussi `true` si un double-clic a servi à revenir à l'image
    /// entière.
    fn video(&mut self, ui: &mut Ui, c: &CameraView, q: Quality, size: egui::Vec2, radius: u8) -> (egui::Response, bool) {
        self.hub.camera_want(&c.id, q);
        // Le glisser ne sert qu'à se déplacer dans l'image zoomée.
        let sense = if self.cams.zoom.contains_key(&c.id) { Sense::click_and_drag() } else { Sense::click() };
        let (rect, resp) = ui.allocate_exact_size(size, sense);
        let tex = self.cam_texture(&c.id);
        let live = c.streaming.is_some() && tex.is_some() && c.error.is_none() && c.stats.as_ref().is_some_and(|s| s.fps > 0.0 || s.width > 0);
        let drawing = self.cams.zones.drawing(&c.id) && self.cams.selected.as_deref() == Some(c.id.as_str());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, CornerRadius::same(radius), Color32::from_rgb(6, 8, 12));
        let (mut zoomed, mut unzoomed) = (false, false);
        match tex {
            Some((id, [w, h])) => {
                let img = fit(rect, w as f32 / h.max(1) as f32);
                // En dessinant une zone, le double-clic la ferme : il ne ramène pas à l'image entière.
                let (z, reset) = self.zoom_gestures(ui, &resp, &c.id, img, !drawing);
                (zoomed, unzoomed) = (z.is_zoomed(), reset);
                let r = if (img.width() - rect.width()).abs() < 1.0 && (img.height() - rect.height()).abs() < 1.0 { radius } else { 0 };
                p.add(egui::epaint::RectShape::filled(img, CornerRadius::same(r), Color32::WHITE).with_texture(id, z.visible()));
                // Calques placés sur l'image entière (agrandie et décalée par le zoom), découpés à la vue.
                let full = z.full_rect(img);
                let o = if zoomed { p.with_clip_rect(img) } else { p.clone() };
                // Zones de la vision IA, sous les détections.
                self.zones_overlay(&o, full, c);
                if !live {
                    p.rect_filled(rect, CornerRadius::same(radius), Color32::from_black_alpha(120));
                } else {
                    // Détections de l'analyse IA (cadres, noms, squelettes).
                    self.vision_overlay(&o, full, c);
                }
                // Zone en cours de dessin (clics sur la vidéo), par-dessus tout.
                self.zone_draft(ui, &o, full, &resp, c);
            }
            None => {
                icons::paint(&p, Rect::from_center_size(rect.center() - vec2(0.0, 14.0), vec2(40.0, 40.0)), Icon::Camera, Color32::from_gray(110));
            }
        }
        if !live {
            let (_, text) = status_of(c, live);
            let msg = if c.streaming.is_some() && c.error.is_none() { "Connexion à la caméra…".to_string() } else { text };
            let g = p.layout(msg, FontId::proportional(14.0), Color32::WHITE, (rect.width() - 40.0).max(80.0));
            let y = if tex.is_none() { rect.center().y + 16.0 } else { rect.center().y - g.size().y / 2.0 };
            p.galley(pos2(rect.center().x - g.size().x / 2.0, y), g, Color32::WHITE);
            if c.streaming.is_some() && c.error.is_none() {
                ui.ctx().request_repaint_after(Duration::from_millis(250));
            }
        }
        // Zoomée, la main pour se déplacer (posée par `zoom_gestures`).
        if resp.hovered() && drawing && !resp.dragged() {
            ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
        } else if resp.hovered() && !zoomed {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        (resp, unzoomed)
    }

    pub(super) fn cameras_view(&mut self, ui: &mut Ui) {
        let cams = self.hub.cameras();
        if self.cams.selected.as_ref().is_some_and(|id| !cams.iter().any(|c| &c.id == id)) {
            self.cams.selected = None;
        }
        // Aucune caméra : on cherche tout de suite sur le réseau.
        if cams.is_empty() && !self.cams.auto_scanned {
            self.cams.auto_scanned = true;
            self.start_cam_scan(Source::System);
        }
        match self.cams.selected.clone().and_then(|id| cams.iter().find(|c| c.id == id).cloned()) {
            Some(c) => self.camera_detail(ui, &c),
            None => self.camera_grid(ui, &cams),
        }
    }

    pub(super) fn start_cam_scan(&mut self, source: Source) {
        let scan = self.cams.scan.clone();
        {
            let mut s = scan.lock().unwrap();
            if s.running {
                return;
            }
            s.running = true;
        }
        let (hub, ctx) = (self.hub.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let found = hub.scan_cameras(source).await;
            let mut s = scan.lock().unwrap();
            (s.running, s.done, s.results) = (false, true, found);
            ctx.request_repaint();
        });
    }

    /// Page « Caméras » : le mode absence, une phrase d'état, puis les vidéos.
    fn camera_grid(&mut self, ui: &mut Ui, cams: &[CameraView]) {
        let armed = self.hub.vision_armed();
        let analysed = self.hub.vision_settings().cameras.values().filter(|c| c.enabled).count();
        widgets::page_header(ui, "Caméras", |ui| {
            if widgets::round_button(ui, Icon::Plus, true).on_hover_text("Ajouter une caméra").clicked() {
                self.cams.add = Some(AddCam::default());
                self.start_cam_scan(Source::User);
            }
            ui.add_space(6.0);
            if !cams.is_empty() {
                let tip = if analysed == 0 { "Active d'abord la surveillance d'une caméra" } else { "Toute personne vue devient une alerte, avec photo et notification" };
                if widgets::toggle(ui, armed, theme::crit(), true).on_hover_text(tip).clicked() {
                    self.hub.set_vision_armed(!armed, Source::User);
                }
                ui.label(RichText::new("Mode absence").size(14.0).color(theme::muted()));
            }
        });
        if !cams.is_empty() {
            let (strong, rest) = self.cameras_lede(cams, analysed);
            widgets::lede(ui, &strong, &rest);
        }
        ui.add_space(18.0);
        ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            if cams.is_empty() {
                widgets::panel_frame().inner_margin(Margin::same(26)).show(ui, |ui| {
                    widgets::cap_width(ui, 640.0);
                    ui.label(RichText::new("Aucune caméra pour l'instant").font(theme::bold(20.0)));
                    ui.add_space(6.0);
                    ui.label(RichText::new("La vidéo va directement de la caméra au PC, sans compte ni cloud. Il faut son adresse IP et, pour une Imou, le code de sécurité (8 caractères sur l'étiquette « SC »).").color(theme::muted()));
                    ui.add_space(12.0);
                    if ui.add(widgets::primary_button("Ajouter une caméra", theme::accent()).min_size(vec2(0.0, 38.0))).clicked() {
                        self.cams.add = Some(AddCam::default());
                    }
                });
                ui.add_space(18.0);
                self.scan_results(ui, cams);
                return;
            }
            let avail = ui.available_width() - 2.0;
            let cols = if avail < 640.0 { 1 } else if avail < 1400.0 { 2 } else { 3 };
            let gap = 18.0;
            let w = (avail - gap * (cols as f32 - 1.0)) / cols as f32;
            let h = w * 9.0 / 16.0;
            let mut open = None;
            for row in cams.chunks(cols) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for c in row {
                        self.hub.camera_want(&c.id, Quality::Sub);
                        let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
                        let tex = self.cam_texture(&c.id);
                        let live = c.streaming.is_some() && tex.is_some() && c.error.is_none();
                        self.paint_cam_tile(ui, rect, c, tex, live, 24, resp.hovered());
                        if let Some(img) = tex.map(|(_, [tw, th])| fit(rect, tw as f32 / th.max(1) as f32)).filter(|_| live) {
                            self.vision_overlay(&ui.painter_at(rect), img, c);
                        }
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                        }
                        if resp.clicked() {
                            open = Some(c.id.clone());
                        }
                    }
                });
                ui.add_space(gap);
            }
            if let Some(id) = open {
                self.cams.selected = Some(id);
                (self.cams.settings, self.cams.confirm_delete, self.cams.side_tab) = (None, false, 0);
            }
        });
    }

    /// Phrase d'état des caméras : ce qui se passe, ou « Tout est calme ».
    fn cameras_lede(&self, cams: &[CameraView], analysed: usize) -> (String, String) {
        if analysed == 0 {
            return ("Surveillance désactivée".into(), " · active-la sur une caméra pour être prévenu".into());
        }
        let mut seen: Vec<(String, usize)> = Vec::new();
        for c in cams {
            if let Some(a) = self.hub.camera_analysis(&c.id).filter(|a| a.running && !a.persons.is_empty()) {
                seen.push((c.room.clone().unwrap_or_else(|| c.name.clone()), a.persons.len()));
            }
        }
        match seen.as_slice() {
            [] => ("Tout est calme".into(), " · aucune personne en vue".into()),
            [(room, n)] => (format!("{} en vue", if *n > 1 { format!("{n} personnes") } else { "1 personne".into() }), format!(" · {room}")),
            many => (format!("{} personnes en vue", many.iter().map(|(_, n)| n).sum::<usize>()), format!(" · {}", many.iter().map(|(r, _)| r.as_str()).collect::<Vec<_>>().join(", "))),
        }
    }

    fn scan_results(&mut self, ui: &mut Ui, cams: &[CameraView]) {
        let (running, done, results) = {
            let s = self.cams.scan.lock().unwrap();
            (s.running, s.done, s.results.clone())
        };
        if !done && !running {
            return;
        }
        let fresh: Vec<&Found> = results.iter().filter(|f| !cams.iter().any(|c| c.host == f.ip || (f.serial.is_some() && c.serial == f.serial))).collect();
        widgets::section_title(ui, "Sur le réseau", Some(if running { "·  recherche en cours…" } else { "·  caméras pas encore ajoutées" }));
        if done && fresh.is_empty() {
            ui.label(RichText::new("Aucune caméra trouvée. Vérifie qu'elle est allumée et connectée au même Wi-Fi que le PC.").color(theme::muted()));
            return;
        }
        for f in fresh {
            widgets::panel_frame().inner_margin(Margin::symmetric(16, 12)).show(ui, |ui| {
                widgets::cap_width(ui, 760.0);
                ui.horizontal(|ui| {
                    widgets::icon_bubble(ui, Icon::Camera, 38.0, 0.0, theme::accent());
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(RichText::new(f.title()).font(theme::bold(14.5)));
                        ui.label(RichText::new(&f.ip).size(12.5).color(theme::muted()));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add(widgets::primary_button("Ajouter", theme::accent()).min_size(vec2(90.0, 32.0))).clicked() {
                            self.cams.add = Some(AddCam::from_found(f));
                        }
                    });
                });
            });
            ui.add_space(8.0);
        }
    }

    /// Une caméra ouverte : la vidéo et ses boutons posés dessus, la surveillance à côté
    /// (ou dessous si la fenêtre est étroite).
    fn camera_detail(&mut self, ui: &mut Ui, c: &CameraView) {
        self.zoom_follow(c);
        let q = if self.cams.hd && !c.main_unavailable { Quality::Main } else { Quality::Sub };
        let (mut leave, mut mode) = (false, None);
        // Échap abandonne d'abord une zone en cours (dessin, nom, modification), puis quitte la caméra.
        let esc = ui.input(|i| i.key_pressed(egui::Key::Escape)) && !self.cams.zones.cancel(&c.id);
        let stacked = ui.available_width() < STACK_WIDTH;
        ui.horizontal(|ui| {
            ui.set_min_height(42.0);
            if widgets::round_button(ui, Icon::Back, false).on_hover_text("Toutes les caméras (Échap)").clicked() || esc {
                leave = true;
            }
            ui.add_space(4.0);
            ui.label(RichText::new(&c.name).font(theme::bold(26.0)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(i) = widgets::segmented(ui, &["Direct", "Enregistrements"], Some(self.cams.rec_mode as usize), theme::accent()) {
                    mode = Some(i == 1);
                }
            });
        });
        if leave {
            if self.cams.rec_mode {
                let _ = self.hub.camera_stop_playback(&c.id, Source::User);
            }
            self.cams.zones.cancel(&c.id);
            (self.cams.selected, self.cams.rec_mode, self.cams.wide) = (None, false, false);
            // Le zoom ne survit pas à la caméra (la HD qu'il a prise non plus : le flux se ferme).
            self.cams.zoom.clear();
            if self.cams.auto_hd.take().is_some_and(|(_, on)| on) {
                self.cams.hd = false;
            }
            return;
        }
        if let Some(rec) = mode.filter(|m| *m != self.cams.rec_mode) {
            self.cams.rec_mode = rec;
            if rec {
                // Le direct (et son micro) s'arrête : la caméra garde ses connexions pour la carte SD.
                self.stop_audio(Source::User);
            } else {
                let _ = self.hub.camera_stop_playback(&c.id, Source::User);
            }
        }
        ui.add_space(14.0);
        if self.cams.rec_mode {
            self.recordings_view(ui, c);
            return;
        }
        // Une zone en cours garde le panneau visible, sur « Avancé » : on y donne son nom.
        let editing = self.cams.zones.editing(&c.id);
        if editing {
            self.cams.side_tab = 1;
        }
        let show_side = !self.cams.wide || editing;
        let avail = ui.available_size();
        let aspect = self.cams.textures.get(&c.id).map(|(t, _)| t.size()).map_or(16.0 / 9.0, |[w, h]| w as f32 / h.max(1) as f32);
        if stacked || !show_side {
            ScrollArea::vertical().id_salt(("cam-stacked", self.cams.side_tab)).auto_shrink([false; 2]).show(ui, |ui| {
                let full = ui.available_width();
                let vh = (full / aspect).min(if show_side { avail.y * 0.66 } else { avail.y - 8.0 }).max(160.0);
                let vw = (vh * aspect).min(full);
                self.live_column(ui, c, q, vw, vh);
                if show_side {
                    ui.add_space(16.0);
                    widgets::panel_frame().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        self.camera_side(ui, c);
                    });
                } else {
                    self.ptz_keys(ui, c);
                }
            });
            return;
        }
        let gap = 18.0;
        let side = (avail.x * 0.3).clamp(290.0, 360.0);
        let vw = avail.x - side - gap;
        let vh = (vw / aspect).min(avail.y - 8.0).max(160.0);
        let vw = (vh * aspect).min(vw);
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(vw);
                self.live_column(ui, c, q, vw, vh);
            });
            ui.add_space(avail.x - vw - side - 8.0 - ui.spacing().item_spacing.x);
            ui.vertical(|ui| {
                ui.set_width(side);
                widgets::panel_frame().show(ui, |ui| {
                    ui.set_width(side - 36.0);
                    ScrollArea::vertical().id_salt(("cam-side", self.cams.side_tab)).max_height(avail.y - 44.0).show(ui, |ui| self.camera_side(ui, c));
                });
            });
        });
    }

    /// La vidéo, avec ses boutons posés dessus (capture, son, positions, HD, plein écran) et le pavé
    /// d'orientation dans le coin ; dessous, seulement les messages de la caméra s'il y en a.
    fn live_column(&mut self, ui: &mut Ui, c: &CameraView, q: Quality, vw: f32, vh: f32) {
        // En mode dessin, le double-clic ferme la zone au lieu d'agrandir la vidéo ; zoomée, il
        // revient d'abord à l'image entière.
        let drawing = self.cams.zones.drawing(&c.id);
        let (resp, unzoomed) = self.video(ui, c, q, vec2(vw, vh), 22);
        if resp.double_clicked() && !drawing && !unzoomed {
            self.cams.wide = !self.cams.wide;
        }
        let rect = resp.rect;
        let img = self.cams.textures.get(&c.id).map_or(rect, |(t, _)| fit(rect, t.size()[0] as f32 / t.size()[1].max(1) as f32));
        self.cams.zoom_area = Some(img);
        self.zoom_badge(ui, &c.id, img);
        if !drawing {
            self.video_buttons(ui, c, rect);
            self.ptz_pad(ui, c, rect);
        }
        if c.main_unavailable && self.cams.hd {
            // Flux HD en H.265 sans FFmpeg : on propose de l'installer, sur place.
            ui.add_space(10.0);
            widgets::panel_frame().show(ui, |ui| {
                ui.set_max_width(vw);
                if let Some(n) = &c.notice {
                    ui.label(RichText::new(n).size(12.5).color(theme::warn()));
                    ui.add_space(6.0);
                }
                self.ffmpeg_panel(ui);
            });
        } else if let Some(n) = &c.notice {
            ui.add_space(6.0);
            ui.label(RichText::new(n).size(12.5).color(theme::warn()));
        }
        if let (Some(e), true) = (&c.error, c.waiting_retry) {
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("{e}. HomeSmart ne réessaie pas tout seul, pour ne pas prolonger le verrouillage.")).size(12.5).color(theme::warn()));
                if ui.button("Réessayer").clicked() {
                    let _ = self.hub.camera_retry(&c.id, Source::User);
                }
            });
        }
    }

    /// Boutons ronds posés en bas de la vidéo.
    fn video_buttons(&mut self, ui: &mut Ui, c: &CameraView, video: Rect) {
        let listening = self.cams.audio_cam.as_deref() == Some(c.id.as_str());
        let n = 5.0;
        let (size, gap, pad) = (44.0, 8.0, 6.0);
        let bar_w = n * size + (n - 1.0) * gap + pad * 2.0;
        let bar = Rect::from_center_size(pos2(video.center().x, video.bottom() - 16.0 - (size + pad * 2.0) / 2.0), vec2(bar_w, size + pad * 2.0));
        if bar.width() > video.width() - 20.0 {
            return;
        }
        ui.painter().rect_filled(bar, CornerRadius::same(255), Color32::from_rgba_unmultiplied(10, 13, 19, 185));
        let slot = |i: usize| Rect::from_min_size(pos2(bar.left() + pad + i as f32 * (size + gap), bar.top() + pad), vec2(size, size));
        let button = |ui: &mut Ui, i: usize, salt: &str, primary: bool, active: bool| -> egui::Response {
            let r = slot(i);
            let resp = ui.interact(r, Id::unique(("cam-btn", salt)), Sense::click());
            let h = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
            let fill = if primary {
                Color32::WHITE
            } else if active {
                theme::accent()
            } else {
                Color32::from_white_alpha((26.0 + 30.0 * h) as u8)
            };
            ui.painter().circle_filled(r.center(), size / 2.0, fill);
            if resp.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            resp
        };
        let ink = |primary: bool, active: bool| if primary { Color32::from_rgb(16, 20, 26) } else if active { theme::on_accent() } else { Color32::WHITE };
        // Capture.
        let shot = button(ui, 0, "shot", true, false).on_hover_text("Capture (PNG dans Images\\HomeSmart)");
        icons::paint(ui.painter(), slot(0).shrink(12.0), Icon::Shutter, ink(true, false));
        if shot.clicked() {
            let (hub, toasts, ctx, id, last) = (self.hub.clone(), self.toasts.clone(), self.ctx.clone(), c.id.clone(), self.cams.last_capture.clone());
            self.rt.spawn(async move {
                let (text, error) = match hub.camera_snapshot(&id, Source::User).await {
                    Ok(p) => {
                        let t = format!("Capture enregistrée : {}", p.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default());
                        *last.lock().unwrap() = Some(p);
                        (t, false)
                    }
                    Err(e) => (format!("Capture impossible : {e:#}"), true),
                };
                toasts.lock().unwrap().push(Toast { text, error, at: Instant::now() });
                ctx.request_repaint();
            });
        }
        // Son du micro (un clic droit règle le volume).
        let sound = button(ui, 1, "sound", false, listening).on_hover_text(if listening { "Couper le son · clic droit : volume" } else { "Écouter le micro de la caméra" });
        icons::paint(ui.painter(), slot(1).shrink(12.0), if listening { Icon::Speaker } else { Icon::Muted }, ink(false, listening));
        if sound.clicked() {
            if listening {
                self.stop_audio(Source::User);
            } else {
                self.start_audio(&c.id);
            }
        }
        egui::Popup::context_menu(&sound).show(|ui| {
            ui.set_min_width(200.0);
            ui.label(RichText::new("Volume").size(12.5).color(theme::muted()));
            if ui.add(egui::Slider::new(&mut self.cams.volume, 0.0..=2.0).show_value(false)).changed() {
                if let Some(p) = &self.cams.player {
                    p.output.set_volume(self.cams.volume);
                }
            }
            self.enhance_toggle(ui);
        });
        // Positions mémorisées.
        let presets = button(ui, 2, "presets", false, false).on_hover_text("Positions mémorisées");
        icons::paint(ui.painter(), slot(2).shrink(12.5), Icon::Bookmark, ink(false, false));
        egui::Popup::menu(&presets).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
            ui.set_min_width(240.0);
            self.presets_ui(ui, c);
        });
        // HD.
        let hd = button(ui, 3, "hd", false, self.cams.hd).on_hover_text(if self.cams.hd { "Image HD (cliquer pour l'image fluide)" } else { "Image fluide (cliquer pour la HD)" });
        ui.painter().text(slot(3).center(), Align2::CENTER_CENTER, "HD", theme::bold(13.0), ink(false, self.cams.hd));
        if hd.clicked() {
            self.cams.hd = !self.cams.hd;
            // Le bouton reprend la main sur la HD prise par le zoom.
            if let Some(auto) = self.cams.auto_hd.as_mut() {
                auto.1 = false;
            }
        }
        // Plein écran (vidéo seule).
        let wide = button(ui, 4, "wide", false, self.cams.wide).on_hover_text(if self.cams.wide { "Afficher la surveillance" } else { "Vidéo seule (double-clic sur l'image)" });
        icons::paint(ui.painter(), slot(4).shrink(12.5), Icon::Expand, ink(false, self.cams.wide));
        if wide.clicked() {
            self.cams.wide = !self.cams.wide;
        }
    }

    /// Poignée entre l'image et la colonne de droite : glisser pour la régler, double-clic pour
    /// revenir à la largeur d'origine. `handle_y` : hauteur de l'image (la prise est centrée dessus).
    pub(in crate::gui) fn side_grip(&mut self, ui: &mut Ui, id: &str, height: f32, handle_y: f32, side: f32, max_side: f32) {
        let (grip, resp) = ui.allocate_exact_size(vec2(14.0, height.max(handle_y)), Sense::drag());
        let hot = resp.hovered() || resp.dragged();
        if hot {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        }
        let t = fx::tween(ui, ("cam-grip", id), hot as i32 as f32, 0.12);
        let x = grip.center().x;
        ui.painter().line_segment([pos2(x, grip.top() + 4.0), pos2(x, grip.bottom() - 4.0)], Stroke::new(1.0 + t, theme::mix(theme::line(), theme::accent(), t)));
        ui.painter().rect_filled(Rect::from_center_size(pos2(x, grip.top() + handle_y / 2.0), vec2(4.0, 36.0)), CornerRadius::same(2), theme::mix(theme::raised_hi(), theme::accent(), t));
        if resp.dragged() {
            self.cams.side_width = (side - resp.drag_delta().x).clamp(260.0, max_side);
        }
        if resp.double_clicked() {
            self.cams.side_width = 360.0;
        }
    }

    /// Appui simulé : à 2 s, appui vers la droite (souris sur le pavé, ou flèche du clavier répétée
    /// comme le fait le système), relâché à 5 s.
    pub(super) fn ptz_script_input(&mut self, raw: &mut egui::RawInput) {
        let Some(pad) = self.cams.pad_rect else { return };
        let Some(s) = self.cams.ptz_script.as_mut() else { return };
        let t = s.start.elapsed().as_secs_f32();
        let pos = pad.center() + vec2(pad.width() * 0.32, 0.0);
        let modifiers = egui::Modifiers::default();
        let key = |pressed: bool, repeat: bool| egui::Event::Key { key: egui::Key::ArrowRight, physical_key: None, pressed, repeat, modifiers };
        let mut note = None;
        if !s.pressed && t >= 2.0 {
            s.pressed = true;
            if s.mouse {
                raw.events.push(egui::Event::PointerMoved(pos));
                raw.events.push(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers });
            } else {
                raw.events.push(key(true, false));
                s.last_key = Instant::now();
            }
            note = Some("APPUI");
        } else if s.pressed && !s.released && t >= 5.0 {
            s.released = true;
            if s.mouse {
                raw.events.push(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers });
            } else {
                raw.events.push(key(false, false));
            }
            note = Some("RELACHE");
        } else if s.pressed && !s.released && !s.mouse && s.last_key.elapsed() >= Duration::from_millis(33) {
            raw.events.push(key(true, true));
            s.last_key = Instant::now();
        }
        if let (Some(n), Some(log)) = (note, &mut self.cams.ptz_log) {
            log.note(n);
        }
    }

    /// Panneau à côté de la vidéo : la surveillance, et deux pages (« Avancé », « Réglages de la caméra »).
    fn camera_side(&mut self, ui: &mut Ui, c: &CameraView) {
        if self.cams.side_tab == 0 {
            if let Some(page) = self.surveillance_section(ui, c) {
                self.cams.side_tab = page;
            }
        } else {
            let title = if self.cams.side_tab == 1 { "Avancé" } else { "Réglages de la caméra" };
            ui.horizontal(|ui| {
                if widgets::round_button(ui, Icon::Back, false).on_hover_text("Retour").clicked() {
                    self.cams.side_tab = 0;
                }
                ui.label(RichText::new(title).font(theme::bold(18.0)));
            });
            ui.add_space(12.0);
            if self.cams.side_tab == 1 {
                self.vision_section(ui, c);
            } else {
                self.settings_tab(ui, c);
            }
        }
        // Les flèches du clavier orientent la caméra.
        self.ptz_keys(ui, c);
    }

    /// Réglages de la caméra : orientation, son, image, qualité, nom et connexion, informations.
    fn settings_tab(&mut self, ui: &mut Ui, c: &CameraView) {
        if c.ptz == Some(true) {
            widgets::section_title(ui, "Orientation", None);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Vitesse").size(13.5).color(theme::muted()));
                ui.spacing_mut().slider_width = (ui.available_width() - 8.0).clamp(80.0, 220.0);
                ui.add(egui::Slider::new(&mut self.cams.speed, 0.2..=1.0).show_value(false));
            });
            // Axes inversés (caméra posée à l'envers, ou firmware qui les inverse) : corrigé ici.
            if let Some(cfg) = self.hub.camera_config(&c.id) {
                let (mut tilt, mut pan) = (cfg.invert_tilt, cfg.invert_pan);
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Inverser").size(13.5).color(theme::muted()));
                    ui.checkbox(&mut tilt, "haut / bas");
                    ui.checkbox(&mut pan, "gauche / droite");
                });
                if (tilt, pan) != (cfg.invert_tilt, cfg.invert_pan) {
                    let patch = CameraPatch { invert_tilt: Some(tilt), invert_pan: Some(pan), ..CameraPatch::default() };
                    if let Err(e) = self.hub.update_camera(&c.id, patch, Source::User) {
                        self.toast(format!("{e:#}"), true);
                    }
                }
            }
            ui.label(RichText::new("Les flèches du clavier orientent aussi la caméra.").size(12.0).color(theme::faint()));
            ui.add_space(12.0);
        }
        widgets::section_title(ui, "Son", None);
        self.enhance_toggle(ui);
        ui.add_space(12.0);
        // Détection de sabotage (toutes les caméras) et réglages Tapo (alarme, suivi, carte SD).
        self.protect_section(ui, c);
        if c.brand == Brand::Tapo {
            self.tapo_section(ui, c);
        }
        if c.brand == Brand::HomeSmart {
            // Caméra fabriquée soi-même : qualité, micro et état par son agent (pas d'ONVIF).
            self.agent_section(ui, c);
            ui.add_space(12.0);
        } else {
            self.onvif_sections(ui, c);
        }

        // Nom, adresse et identifiants.
        let cfg = self.hub.camera_config(&c.id);
        let (mut save, mut delete) = (None, false);
        egui::CollapsingHeader::new(RichText::new("Nom et connexion").font(theme::bold(14.0))).id_salt(("cam-settings", &c.id)).show(ui, |ui| {
            if self.cams.settings.as_ref().is_none_or(|s| s.id != c.id) {
                self.cams.settings = cfg.as_ref().map(CamSettings::of);
            }
            let Some(s) = self.cams.settings.as_mut() else { return };
            egui::Grid::new(("cam-set-grid", &c.id)).num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                let w = (ui.available_width() - 130.0).clamp(130.0, 200.0);
                ui.label("Nom");
                ui.add(egui::TextEdit::singleline(&mut s.name).desired_width(w));
                ui.end_row();
                ui.label("Pièce");
                ui.add(egui::TextEdit::singleline(&mut s.room).desired_width(w));
                ui.end_row();
                ui.label("Adresse IP");
                ui.add(egui::TextEdit::singleline(&mut s.host).desired_width(w));
                ui.end_row();
                // Caméra HomeSmart : identifiants nés de l'appairage, ports annoncés par son agent.
                if s.brand != Brand::HomeSmart {
                    ui.label("Utilisateur");
                    ui.add(egui::TextEdit::singleline(&mut s.user).desired_width(w));
                    ui.end_row();
                    ui.label("Code / mot de passe");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut s.password).password(!s.show_pw).desired_width(w - 50.0));
                        ui.checkbox(&mut s.show_pw, "voir");
                    });
                    ui.end_row();
                    ui.label("Marque");
                    egui::ComboBox::from_id_salt(("cam-brand", &c.id)).selected_text(s.brand.label()).width(w).show_ui(ui, |ui| {
                        // Devenir une caméra HomeSmart demande un appairage (formulaire d'ajout).
                        for b in Brand::ALL.into_iter().filter(|b| *b != Brand::HomeSmart) {
                            ui.selectable_value(&mut s.brand, b, b.label());
                        }
                    });
                    ui.end_row();
                    ui.label("Ports RTSP / ONVIF");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut s.rtsp_port).desired_width(60.0));
                        ui.add(egui::TextEdit::singleline(&mut s.onvif_port).desired_width(60.0));
                    });
                    ui.end_row();
                }
                if s.brand == Brand::Generic {
                    ui.label("Chemin RTSP HD");
                    ui.add(egui::TextEdit::singleline(&mut s.main_path).hint_text("/live/ch0").desired_width(w));
                    ui.end_row();
                    ui.label("Chemin RTSP fluide");
                    ui.add(egui::TextEdit::singleline(&mut s.sub_path).hint_text("/live/ch1").desired_width(w));
                    ui.end_row();
                }
            });
            ui.add_space(8.0);
            if ui.add(widgets::primary_button("Enregistrer", theme::accent())).clicked() {
                save = Some(s.patch());
            }
            ui.add_space(10.0);
            if !self.cams.confirm_delete {
                if ui.add(egui::Button::new(RichText::new("Supprimer la caméra").color(theme::crit()))).clicked() {
                    self.cams.confirm_delete = true;
                }
            } else {
                let note = if c.brand == Brand::HomeSmart { "(la caméra oublie aussi ce HomeSmart)" } else { "(la caméra elle-même n'est pas modifiée)" };
                ui.label(RichText::new(format!("Supprimer cette caméra de HomeSmart ? {note}")).size(12.5));
                ui.horizontal(|ui| {
                    if ui.add(widgets::primary_button("Supprimer", theme::crit())).clicked() {
                        delete = true;
                    }
                    if ui.button("Annuler").clicked() {
                        self.cams.confirm_delete = false;
                    }
                });
            }
        });
        ui.add_space(6.0);

        // Informations (techniques : ici seulement).
        egui::CollapsingHeader::new(RichText::new("Informations").font(theme::bold(14.0))).id_salt(("cam-info-h", &c.id)).show(ui, |ui| {
            egui::Grid::new(("cam-info", &c.id)).num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                let mut row = |k: &str, v: String| {
                    ui.label(RichText::new(k).size(12.5).color(theme::muted()));
                    ui.label(RichText::new(v).size(12.5));
                    ui.end_row();
                };
                let model = c.info.as_ref().map(|i| i.model.clone()).filter(|m| !m.is_empty()).or(c.model.clone());
                row("Marque", c.brand.label().to_string());
                if let Some(m) = model {
                    row("Modèle", m);
                }
                if let Some(i) = &c.info {
                    if !i.firmware.is_empty() {
                        row("Firmware", i.firmware.clone());
                    }
                }
                if let Some(s) = &c.serial {
                    row("N° de série", s.clone());
                }
                row("Adresse", if c.brand.has_onvif() { format!("{} (RTSP {}, ONVIF {})", c.host, c.rtsp_port, c.onvif_port) } else { format!("{} (RTSP {}, agent)", c.host, c.rtsp_port) });
                if let Some(m) = &c.mac {
                    row("MAC", m.clone());
                }
                if let Some(s) = &c.stats {
                    row("Flux", format!("{} · {} · {}×{}", c.streaming.map(|q| q.label()).unwrap_or("—"), s.codec, s.width, s.height));
                    let shown = self.cams.shown.get(&c.id).filter(|sh| sh.fps > 0.0).map(|sh| format!(" · {:.0} affichées ({:.1} ms)", sh.fps, sh.prepare_ms)).unwrap_or_default();
                    row("Cadence", format!("{:.0} i/s{shown}", s.fps));
                    row("Débit", format!("{:.0} kbit/s", s.kbps));
                }
                if let Some(e) = &c.onvif_error {
                    row("ONVIF", e.clone());
                }
            });
        });

        // Actions exécutées une fois le formulaire dessiné.
        if let Some(patch) = save {
            match self.hub.update_camera(&c.id, patch, Source::User) {
                Ok(v) => {
                    self.cams.settings = self.hub.camera_config(&v.id).as_ref().map(CamSettings::of);
                    self.toast(format!("« {} » enregistrée", v.name), false);
                }
                Err(e) => self.toast(format!("{e:#}"), true),
            }
        }
        if delete {
            self.cams.confirm_delete = false;
            match self.hub.remove_camera(&c.id, Source::User) {
                Ok(()) => {
                    self.toast(format!("« {} » supprimée", c.name), false);
                    self.cams.selected = None;
                    self.cams.textures.remove(&c.id);
                }
                Err(e) => self.toast(format!("{e:#}"), true),
            }
        }
    }

    /// Image et qualité vidéo, réglées en ONVIF.
    fn onvif_sections(&mut self, ui: &mut Ui, c: &CameraView) {
        egui::CollapsingHeader::new(RichText::new("Image").font(theme::bold(14.0)))
            .id_salt(("cam-image", &c.id))
            .default_open(self.cams.open_image)
            .show(ui, |ui| {
                ui.label(RichText::new("Luminosité, contraste, vision de nuit, contre-jour.").size(12.0).color(theme::faint()));
                self.image_ui(ui, c);
            });
        ui.add_space(6.0);

        if self.cams.open_quality {
            // Capture d'écran automatique : faire défiler jusqu'à cette section.
            ui.scroll_to_cursor(Some(Align::TOP));
        }
        egui::CollapsingHeader::new(RichText::new("Qualité vidéo").font(theme::bold(14.0)))
            .id_salt(("cam-quality", &c.id))
            .default_open(self.cams.open_quality)
            .show(ui, |ui| {
                ui.label(RichText::new("Résolution, images par seconde et débit de chaque flux.").size(12.0).color(theme::faint()));
                self.quality_ui(ui, c);
            });
        ui.add_space(6.0);
    }

    fn load_imaging(&mut self, id: &str) {
        *self.cams.img.lock().unwrap() = ImgState { id: id.to_string(), loading: true, ..ImgState::default() };
        let (hub, slot, ctx, id) = (self.hub.clone(), self.cams.img.clone(), self.ctx.clone(), id.to_string());
        self.rt.spawn(async move {
            let r = hub.camera_imaging(&id).await;
            let mut st = slot.lock().unwrap();
            if st.id == id {
                st.loading = false;
                match r {
                    Ok((cur, opts)) => (st.current, st.edit, st.options) = (cur.clone(), cur, opts),
                    Err(e) => st.error = Some(format!("{e:#}")),
                }
            }
            ctx.request_repaint();
        });
    }

    /// Luminosité, contraste, saturation, netteté ; vision de nuit, WDR et contre-jour si la caméra les propose.
    fn image_ui(&mut self, ui: &mut Ui, c: &CameraView) {
        if self.cams.img.lock().unwrap().id != c.id {
            self.load_imaging(&c.id);
        }
        let slot = self.cams.img.clone();
        let mut apply: Option<(ImagingSettings, ImagingSettings)> = None;
        let mut reload = false;
        {
            let mut st = slot.lock().unwrap();
            if st.loading {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Lecture des réglages d'image…").size(12.5).color(theme::muted()));
                });
                return;
            }
            if let Some(e) = st.error.clone() {
                ui.label(RichText::new(format!("Réglages d'image indisponibles : {e}")).size(12.5).color(theme::warn()));
                if ui.button("Réessayer").clicked() {
                    reload = true;
                }
            } else {
                let ImgState { current, edit, options, .. } = &mut *st;
                let slider = |ui: &mut Ui, label: &str, v: &mut Option<f32>, range: Option<(f32, f32)>| {
                    if let (Some(x), Some((lo, hi))) = (v.as_mut(), range) {
                        ui.horizontal(|ui| {
                            ui.add_sized(vec2(80.0, 18.0), egui::Label::new(RichText::new(label).size(12.5).color(theme::muted())));
                            // Le curseur prend la place restante, case de valeur comprise (≈ 48 px).
                            ui.spacing_mut().slider_width = (ui.available_width() - 56.0).max(60.0);
                            ui.add(egui::Slider::new(x, lo..=hi).fixed_decimals(0));
                        });
                    }
                };
                slider(ui, "Luminosité", &mut edit.brightness, options.brightness);
                slider(ui, "Contraste", &mut edit.contrast, options.contrast);
                slider(ui, "Saturation", &mut edit.saturation, options.saturation);
                slider(ui, "Netteté", &mut edit.sharpness, options.sharpness);
                let modes = |ui: &mut Ui, label: &str, v: &mut Option<String>, list: &[String], names: &[(&str, &str)]| {
                    if list.is_empty() || v.is_none() {
                        return;
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.add_sized(vec2(80.0, 18.0), egui::Label::new(RichText::new(label).size(12.5).color(theme::muted())));
                        for m in list {
                            let name = names.iter().find(|(k, _)| k == m).map(|(_, n)| *n).unwrap_or(m.as_str());
                            if ui.add(egui::Button::selectable(v.as_deref() == Some(m.as_str()), name)).clicked() {
                                *v = Some(m.clone());
                            }
                        }
                    });
                };
                modes(ui, "Vision de nuit", &mut edit.ir_cut, &options.ir_cut, &[("AUTO", "Auto"), ("ON", "Jour"), ("OFF", "Nuit")]);
                modes(ui, "WDR (HDR)", &mut edit.wdr, &options.wdr, &[("ON", "Activé"), ("OFF", "Désactivé")]);
                if edit.wdr.as_deref() == Some("ON") {
                    slider(ui, "Niveau WDR", &mut edit.wdr_level, options.wdr_level);
                }
                modes(ui, "Contre-jour", &mut edit.backlight, &options.backlight, &[("ON", "Activé"), ("OFF", "Désactivé")]);
                let missing: Vec<&str> = [("vision de nuit", options.ir_cut.is_empty()), ("WDR / HDR", options.wdr.is_empty()), ("contre-jour", options.backlight.is_empty())]
                    .iter()
                    .filter(|(_, m)| *m)
                    .map(|(n, _)| *n)
                    .collect();
                if !missing.is_empty() {
                    ui.label(RichText::new(format!("Non réglable sur cette caméra : {}.", missing.join(", "))).size(11.5).color(theme::faint()));
                }
                let changed = *edit != *current;
                ui.horizontal(|ui| {
                    if ui.add_enabled(changed, widgets::primary_button("Appliquer", theme::accent())).clicked() {
                        apply = Some((current.clone(), edit.clone()));
                    }
                    if changed && ui.button("Annuler").clicked() {
                        *edit = current.clone();
                    }
                });
            }
        }
        if reload {
            self.load_imaging(&c.id);
        }
        if let Some((before, after)) = apply {
            let (hub, toasts, ctx, id, slot) = (self.hub.clone(), self.toasts.clone(), self.ctx.clone(), c.id.clone(), self.cams.img.clone());
            self.rt.spawn(async move {
                let (text, error) = match hub.camera_set_imaging(&id, before, after.clone(), Source::User).await {
                    Ok(()) => {
                        slot.lock().unwrap().current = after;
                        ("Réglages d'image enregistrés".to_string(), false)
                    }
                    Err(e) => (format!("Réglages d'image refusés : {e:#}"), true),
                };
                toasts.lock().unwrap().push(Toast { text, error, at: Instant::now() });
                ctx.request_repaint();
            });
        }
    }

    fn load_encoders(&mut self, id: &str) {
        {
            let mut e = self.cams.enc.lock().unwrap();
            *e = EncState { id: id.to_string(), loading: true, ..EncState::default() };
        }
        let (hub, slot, ctx, id) = (self.hub.clone(), self.cams.enc.clone(), self.ctx.clone(), id.to_string());
        self.rt.spawn(async move {
            let r = hub.camera_encoders(&id).await;
            let mut e = slot.lock().unwrap();
            if e.id == id {
                e.loading = false;
                match r {
                    Ok(list) => {
                        e.edits = list.iter().map(|(_, c, _)| c.clone()).collect();
                        e.list = list;
                    }
                    Err(err) => e.error = Some(format!("{err:#}")),
                }
            }
            ctx.request_repaint();
        });
    }

    /// Résolution, cadence et débit de chaque flux, dans les limites annoncées par la caméra.
    fn quality_ui(&mut self, ui: &mut Ui, c: &CameraView) {
        let stale = self.cams.enc.lock().unwrap().id != c.id;
        if stale {
            self.load_encoders(&c.id);
        }
        let slot = self.cams.enc.clone();
        let mut apply: Option<(Quality, EncoderConfig)> = None;
        let mut reload = false;
        {
            let mut st = slot.lock().unwrap();
            if st.loading {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Lecture des réglages de la caméra…").size(12.5).color(theme::muted()));
                });
                return;
            }
            if let Some(e) = &st.error {
                ui.label(RichText::new(format!("Réglages illisibles : {e}")).size(12.5).color(theme::warn()));
                if ui.button("Réessayer").clicked() {
                    reload = true;
                }
            }
            let EncState { list, edits, .. } = &mut *st;
            for (i, (q, cur, opts)) in list.iter().enumerate() {
                let Some(ed) = edits.get_mut(i) else { continue };
                ui.add_space(4.0);
                ui.label(RichText::new(format!("Flux {}", q.label())).font(theme::bold(13.0)));
                ui.label(RichText::new(format!("Actuel : {}×{} · {} i/s · {} kbit/s", cur.width, cur.height, cur.fps, cur.bitrate_kbps)).size(11.5).color(theme::muted()));
                let mut res: Vec<(u32, u32)> = opts.resolutions.clone();
                if !res.contains(&(cur.width, cur.height)) {
                    res.insert(0, (cur.width, cur.height));
                }
                ui.add_space(4.0);
                ui.label(RichText::new("Résolution").size(12.5).color(theme::muted()));
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
                    // Flux HD : les formats standard (grisés si la caméra ne les propose pas) ; flux fluide : les siens.
                    let choices = if *q == Quality::Main { resolution_choices(&res) } else { resolution_choices(&res).into_iter().filter(|c| c.3).collect() };
                    for (label, w, h, ok) in choices {
                        let selected = ok && (ed.width, ed.height) == (w, h);
                        let b = ui.add_enabled(ok, egui::Button::selectable(selected, label));
                        if b.clicked() {
                            (ed.width, ed.height) = (w, h);
                        }
                        if ok {
                            b.on_hover_text(format!("{w}×{h}"));
                        } else {
                            b.on_disabled_hover_text("Cette caméra ne propose pas cette résolution");
                        }
                    }
                });
                ui.add_space(4.0);
                ui.label(RichText::new("Images par seconde").size(12.5).color(theme::muted()));
                let (lo, hi) = if opts.fps.1 > 0 { (opts.fps.0.max(1), opts.fps.1) } else { (1, cur.fps.max(1)) };
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
                    let mut presets: Vec<u32> = vec![15, 24, 25, 30, 60];
                    if !presets.contains(&hi) {
                        presets.push(hi);
                        presets.sort_unstable();
                    }
                    for f in presets {
                        let ok = f >= lo && f <= hi;
                        let b = ui.add_enabled(ok, egui::Button::selectable(ed.fps == f, format!("{f}")));
                        if b.clicked() {
                            ed.fps = f;
                        }
                        if !ok {
                            b.on_disabled_hover_text(format!("Cette caméra va jusqu'à {hi} images/s"));
                        }
                    }
                });
                // Place laissée pour la case « 25 i/s » à droite du curseur.
                ui.spacing_mut().slider_width = (ui.available_width() - 84.0).max(80.0);
                ui.add(egui::Slider::new(&mut ed.fps, lo..=hi).suffix(" i/s"));
                ui.add_space(4.0);
                ui.label(RichText::new("Débit").size(12.5).color(theme::muted()));
                // Beaucoup de caméras n'annoncent pas leurs limites (la Ranger 2C, par exemple) :
                // des valeurs courantes en un clic, et la valeur exacte à saisir ou faire glisser.
                let (blo, bhi) = opts.bitrate_kbps.unwrap_or((128, 16384));
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
                    for kbps in [512u32, 1024, 2048, 3072, 4096, 6144, 8192] {
                        if kbps < blo || kbps > bhi {
                            continue;
                        }
                        let label = if kbps >= 1024 { format!("{} Mbit/s", homesmart::kinds::fr(kbps as f64 / 1024.0, if kbps % 1024 == 0 { 0 } else { 1 })) } else { format!("{kbps} kbit/s") };
                        if ui.add(egui::Button::selectable(ed.bitrate_kbps == kbps, label)).clicked() {
                            ed.bitrate_kbps = kbps;
                        }
                    }
                    ui.add(egui::DragValue::new(&mut ed.bitrate_kbps).range(blo..=bhi).speed(32.0).suffix(" kbit/s")).on_hover_text("Glisser, ou cliquer pour saisir une valeur");
                });
                let max_res = opts.resolutions.iter().max_by_key(|(w, h)| w * h).copied().unwrap_or((cur.width, cur.height));
                ui.label(
                    RichText::new(format!("Maximum de la caméra : {}×{} à {} i/s", max_res.0, max_res.1, if opts.fps.1 > 0 { opts.fps.1 } else { cur.fps }))
                        .size(11.5)
                        .color(theme::faint()),
                );
                if *q == Quality::Main && (c.main_unavailable || c.stats.as_ref().is_some_and(|s| s.codec == "H.265")) {
                    ui.label(
                        RichText::new("Ce flux est en H.265 : l'enregistrer par ONVIF le passe en H.264 (images plus faciles à décoder, mais fichiers plus gros sur la carte mémoire).")
                            .size(11.5)
                            .color(theme::warn()),
                    );
                }
                let changed = (ed.width, ed.height, ed.fps, ed.bitrate_kbps) != (cur.width, cur.height, cur.fps, cur.bitrate_kbps);
                ui.horizontal(|ui| {
                    if ui.add_enabled(changed, widgets::primary_button("Appliquer", theme::accent())).clicked() {
                        apply = Some((*q, ed.clone()));
                    }
                    if changed && ui.button("Annuler").clicked() {
                        *ed = cur.clone();
                    }
                });
                ui.add_space(6.0);
            }
        }
        if reload {
            self.load_encoders(&c.id);
        }
        if let Some((q, cfg)) = apply {
            let (hub, toasts, ctx, id, slot) = (self.hub.clone(), self.toasts.clone(), self.ctx.clone(), c.id.clone(), self.cams.enc.clone());
            self.rt.spawn(async move {
                let (text, error) = match hub.camera_set_encoder(&id, q, cfg, Source::User).await {
                    Ok(None) => (format!("Qualité vidéo du flux {} enregistrée", q.label()), false),
                    // La caméra a plafonné une valeur : le dire, plutôt qu'un « enregistré » trompeur.
                    Ok(Some(adjusted)) => (format!("Flux {} : {adjusted}", q.label()), true),
                    Err(e) => (format!("Qualité vidéo refusée : {e:#}"), true),
                };
                // Relire les réglages au prochain affichage.
                slot.lock().unwrap().id.clear();
                toasts.lock().unwrap().push(Toast { text, error, at: Instant::now() });
                ctx.request_repaint();
            });
        }
    }

    /// « Son amélioré » : volume automatique, filtre des basses, limiteur (préférence gardée).
    pub(super) fn enhance_toggle(&mut self, ui: &mut Ui) {
        let mut prefs = self.hub.prefs();
        let r = ui.add(egui::Button::selectable(prefs.audio_enhance, "Son amélioré")).on_hover_text("Volume automatique (les micros de caméra sont faibles), filtre des basses, pas de saturation");
        if r.clicked() {
            prefs.audio_enhance = !prefs.audio_enhance;
            for p in [&self.cams.player, &self.cams.rec.player].into_iter().flatten() {
                p.output.set_enhance(prefs.audio_enhance);
            }
            self.hub.set_prefs(prefs);
        }
    }

    /// Écoute le micro d'une caméra (une seule à la fois).
    fn start_audio(&mut self, id: &str) {
        self.stop_audio(Source::User);
        match homesmart::camera::audio::Player::new() {
            Ok(player) => {
                player.output.set_volume(self.cams.volume);
                player.output.set_enhance(self.hub.prefs().audio_enhance);
                let out = player.output.clone();
                let sink: homesmart::camera::stream::AudioSink = Arc::new(move |s: &[f32], rate: u32| out.push(s, rate));
                match self.hub.set_camera_audio(id, Some(sink), Source::User) {
                    Ok(()) => {
                        self.cams.player = Some(player);
                        self.cams.audio_cam = Some(id.to_string());
                    }
                    Err(e) => self.toast(format!("Son : {e:#}"), true),
                }
            }
            Err(e) => self.toast(format!("Son impossible : {e:#}"), true),
        }
    }

    /// Coupe le son en cours (bouton, ou caméra quittée).
    pub(super) fn stop_audio(&mut self, source: Source) {
        if let Some(id) = self.cams.audio_cam.take() {
            let _ = self.hub.set_camera_audio(&id, None, source);
        }
        self.cams.player = None;
    }

    /// Le son ne reste actif que dans la vue détaillée de la caméra écoutée.
    pub(super) fn audio_housekeeping(&mut self) {
        let keep = matches!(&self.cams.audio_cam, Some(id) if self.view == super::View::Cameras && self.cams.selected.as_deref() == Some(id.as_str()));
        if self.cams.audio_cam.is_some() && !keep {
            self.stop_audio(Source::User);
        }
    }

    /// Encart FFmpeg : état, installation en un clic (Windows) ou commande à copier (Linux).
    pub(super) fn ffmpeg_panel(&mut self, ui: &mut Ui) {
        let st = self.hub.ffmpeg_status();
        if !st.checked || st.searching {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Recherche de FFmpeg…").size(12.5).color(theme::muted()));
            });
            ui.ctx().request_repaint_after(Duration::from_millis(300));
            return;
        }
        if let Some(path) = &st.path {
            let v = st.version.as_deref().map(|v| format!(" {v}")).unwrap_or_default();
            ui.label(RichText::new(format!("✔  FFmpeg{v} : les flux HD en H.265 s'affichent.")).size(12.5).color(theme::accent()));
            ui.label(RichText::new(path.display().to_string()).size(11.0).color(theme::faint()));
            return;
        }
        if st.installing {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new(format!("Installation de FFmpeg : {}", st.step)).size(12.5));
            });
            ui.label(RichText::new("Environ une minute. Tu peux continuer à utiliser HomeSmart.").size(11.5).color(theme::faint()));
            ui.ctx().request_repaint_after(Duration::from_millis(300));
            return;
        }
        ui.label(
            RichText::new("FFmpeg décode le H.265. Gratuit (licence GPL), environ 100 Mo, installé une seule fois ; il ne tourne que pendant que tu regardes en HD.")
                .size(12.0)
                .color(theme::muted()),
        );
        if let Some(e) = &st.error {
            ui.label(RichText::new(format!("L'installation a échoué : {e}")).size(12.0).color(theme::crit()));
        }
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            if st.can_install {
                let label = if st.error.is_some() { "Réessayer l'installation" } else { "Installer FFmpeg" };
                if ui.add(widgets::primary_button(label, theme::accent()).min_size(vec2(0.0, 32.0))).on_hover_text("Paquet officiel Gyan.FFmpeg via winget, sans droits administrateur").clicked() {
                    self.hub.install_ffmpeg(Source::User);
                }
            } else {
                ui.label(RichText::new("Dans un terminal :").size(12.0));
                ui.label(RichText::new(&st.manual).monospace().size(12.0));
                if ui.button("Copier").clicked() {
                    ui.ctx().copy_text(st.manual.clone());
                    self.toast("Commande copiée", false);
                }
            }
            if ui.button("Vérifier à nouveau").on_hover_text("Si tu as installé FFmpeg toi-même (ou posé ffmpeg.exe à côté de HomeSmart.exe)").clicked() {
                self.hub.refresh_ffmpeg(Source::User);
            }
        });
    }

    /// Flèches du clavier → orientation, quand le pavé n'est pas affiché (autre onglet, vidéo agrandie).
    fn ptz_keys(&mut self, ui: &mut Ui, c: &CameraView) {
        if c.ptz != Some(true) {
            return;
        }
        let speed = if self.cams.speed > 0.0 { self.cams.speed } else { 0.6 };
        let want = keyboard_dir(ui);
        self.cams.ptz_drive.propose(&c.id, want.map(|(x, y)| (x * speed, -y * speed)));
        if want.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Pavé d'orientation posé dans le coin de la vidéo : on appuie dans une direction et on
    /// maintient ; le centre ramène à la position d'origine. Les flèches du clavier marchent aussi.
    fn ptz_pad(&mut self, ui: &mut Ui, c: &CameraView, video: Rect) {
        if c.ptz != Some(true) {
            return;
        }
        if self.cams.speed <= 0.0 {
            self.cams.speed = 0.6;
        }
        let size = 108.0;
        if video.width() < 420.0 || video.height() < 220.0 {
            self.ptz_keys(ui, c);
            return;
        }
        let rect = Rect::from_min_size(video.right_bottom() - vec2(size + 16.0, size + 16.0), vec2(size, size));
        self.cams.pad_rect = Some(rect);
        let resp = ui.interact(rect, Id::unique(("ptz-pad", &c.id)), Sense::click_and_drag());
        let center = rect.center();
        let (outer, inner) = (size / 2.0, 18.0);
        let pointer = ui.input(|i| i.pointer.interact_pos().or(i.pointer.hover_pos()));
        let sector = |pos: egui::Pos2| {
            let d = pos - center;
            (d.length() > inner && d.length() <= outer + 6.0).then(|| (((d.y.atan2(d.x) / (PI / 4.0)).round() as i32).rem_euclid(8)) as usize)
        };
        let hovered_sector = pointer.filter(|_| resp.hovered() || resp.is_pointer_button_down_on()).and_then(sector);
        let pressing = resp.is_pointer_button_down_on();
        // Directions : 0 = droite, puis dans le sens horaire à l'écran.
        const DIRS: [(f32, f32); 8] = [(1.0, 0.0), (1.0, -1.0), (0.0, -1.0), (-1.0, -1.0), (-1.0, 0.0), (-1.0, 1.0), (0.0, 1.0), (1.0, 1.0)];
        let want = if pressing { hovered_sector.map(|k| DIRS[k]) } else { None }.or_else(|| keyboard_dir(ui));
        let active = want.map(|(x, y)| {
            let a = y.atan2(x);
            (((a / (PI / 4.0)).round() as i32).rem_euclid(8)) as usize
        });
        let speed = self.cams.speed;
        self.cams.ptz_drive.propose(&c.id, want.map(|(x, y)| (x * speed, -y * speed)));
        if want.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }

        let p = ui.painter();
        p.circle_filled(center, outer, Color32::from_rgba_unmultiplied(10, 13, 19, 150));
        p.circle_stroke(center, outer, Stroke::new(1.0, Color32::from_white_alpha(30)));
        for k in 0..8 {
            let a = k as f32 * PI / 4.0;
            let lit = active == Some(k);
            let hover = hovered_sector == Some(k);
            let t = fx::tween(ui, ("ptz-sector", &c.id, k), if lit { 1.0 } else if hover { 0.45 } else { 0.0 }, 0.12);
            if t > 0.01 {
                let n = 10;
                let mut pts = Vec::with_capacity(n * 2 + 2);
                for i in 0..=n {
                    let b = a - PI / 8.0 + PI / 4.0 * i as f32 / n as f32;
                    pts.push(center + vec2(b.cos(), b.sin()) * (outer - 2.0));
                }
                for i in (0..=n).rev() {
                    let b = a - PI / 8.0 + PI / 4.0 * i as f32 / n as f32;
                    pts.push(center + vec2(b.cos(), b.sin()) * (inner + 4.0));
                }
                p.add(egui::Shape::convex_polygon(pts, theme::alpha(theme::accent(), 0.45 * t), Stroke::NONE));
            }
            let dir = vec2(a.cos(), a.sin());
            let tip = center + dir * (outer - 12.0);
            let color = theme::mix(Color32::from_white_alpha(190), Color32::WHITE, t);
            if k % 2 == 0 {
                let perp = vec2(-dir.y, dir.x);
                let base = tip - dir * 7.0;
                p.add(egui::Shape::line(vec![base + perp * 6.0, tip, base - perp * 6.0], Stroke::new(2.2, color)));
            }
        }
        // Centre : retour à la position d'origine (masqué si la caméra n'en a pas).
        let home = c.home_supported != Some(false) && c.presets_supported != Some(false);
        let center_hover = home && pointer.is_some_and(|pos| (pos - center).length() <= inner) && resp.hovered();
        let ch = fx::tween(ui, ("ptz-home", &c.id), center_hover as i32 as f32, 0.12);
        p.circle_filled(center, inner, Color32::from_white_alpha((40.0 + 50.0 * ch) as u8));
        if home && resp.clicked() && pointer.is_some_and(|pos| (pos - center).length() <= inner) {
            self.ptz(&c.id, PtzCmd::Home);
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        resp.on_hover_text(if home { "Maintenir pour orienter · centre : position d'origine · flèches du clavier" } else { "Maintenir pour orienter · flèches du clavier" });
    }

    fn presets_ui(&mut self, ui: &mut Ui, c: &CameraView) {
        widgets::section_title(ui, "Positions mémorisées", Some(&format!("·  {}", c.presets.len())));
        if c.presets_supported == Some(false) {
            ui.label(RichText::new("Cette caméra ne gère pas les positions mémorisées en ONVIF.").size(12.5).color(theme::muted()));
            return;
        }
        let mut goto = None;
        let mut remove = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
            for p in &c.presets {
                let b = ui.add(egui::Button::new(RichText::new(&p.name).size(12.5)).corner_radius(CornerRadius::same(12)).min_size(vec2(0.0, 26.0)));
                if b.clicked() {
                    goto = Some(p.token.clone());
                }
                b.on_hover_text("Clic : y aller · clic droit : supprimer").context_menu(|ui| {
                    if ui.button(format!("Supprimer « {} »", p.name)).clicked() {
                        remove = Some(p.token.clone());
                        ui.close();
                    }
                });
            }
        });
        if let Some(t) = goto {
            self.ptz(&c.id, PtzCmd::Goto(t));
        }
        if let Some(t) = remove {
            let (hub, toasts, ctx, id) = (self.hub.clone(), self.toasts.clone(), self.ctx.clone(), c.id.clone());
            self.rt.spawn(async move {
                if let Err(e) = hub.camera_remove_preset(&id, &t, Source::User).await {
                    toasts.lock().unwrap().push(Toast { text: format!("{e:#}"), error: true, at: Instant::now() });
                    ctx.request_repaint();
                }
            });
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.cams.preset_name).hint_text("Nom de la position").desired_width(170.0));
            let ok = !self.cams.preset_name.trim().is_empty();
            if ui.add_enabled(ok, egui::Button::new("Mémoriser ici")).clicked() {
                let name = std::mem::take(&mut self.cams.preset_name);
                let (hub, toasts, ctx, id) = (self.hub.clone(), self.toasts.clone(), self.ctx.clone(), c.id.clone());
                self.rt.spawn(async move {
                    let (text, error) = match hub.camera_save_preset(&id, &name, Source::User).await {
                        Ok(_) => (format!("Position « {} » mémorisée", name.trim()), false),
                        Err(e) => (format!("{e:#}"), true),
                    };
                    toasts.lock().unwrap().push(Toast { text, error, at: Instant::now() });
                    ctx.request_repaint();
                });
            }
        });
    }

    pub(super) fn add_camera_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut f) = self.cams.add.take() else { return };
        let (mut close, mut submit, mut test, mut rescan) = (false, false, false, false);
        let mut done: Option<CameraView> = None;
        // Caméras trouvées sur le réseau, pas encore ajoutées.
        let (running, results) = {
            let s = self.cams.scan.lock().unwrap();
            (s.running, s.results.clone())
        };
        let known = self.hub.cameras();
        let fresh: Vec<Found> = results.into_iter().filter(|r| !known.iter().any(|c| c.host == r.ip || (r.serial.is_some() && c.serial == r.serial))).collect();
        let frame = egui::Frame::new().fill(theme::surface()).corner_radius(CornerRadius::same(22)).inner_margin(Margin::same(24));
        let modal = egui::Modal::new(Id::unique("add-camera")).backdrop_color(theme::scrim()).frame(frame).show(ctx, |ui| {
            ui.set_width(470.0);
            ScrollArea::vertical().max_height(ctx.content_rect().height() - 140.0).show(ui, |ui| {
            ui.label(RichText::new("Ajouter une caméra").font(theme::bold(21.0)));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Sur le réseau").font(theme::bold(14.0)).color(theme::muted()));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if running {
                        ui.spinner();
                    } else if ui.small_button("Chercher à nouveau").clicked() {
                        rescan = true;
                    }
                });
            });
            if fresh.is_empty() {
                ui.label(RichText::new(if running { "Recherche des caméras Imou / Dahua, ONVIF et RTSP…" } else { "Aucune nouvelle caméra trouvée." }).size(12.5).color(theme::faint()));
            }
            for r in fresh.iter().take(4) {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(r.title()).size(13.5));
                    ui.label(RichText::new(&r.ip).size(12.5).color(theme::muted()));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Choisir").clicked() {
                            let (name, room, password) = (f.name.clone(), f.room.clone(), f.password.clone());
                            f = AddCam::from_found(r);
                            (f.name, f.room, f.password) = (name, room, password);
                        }
                    });
                });
            }
            ui.add_space(12.0);
            let diy = f.brand == Brand::HomeSmart;
            let sub = match (&f.model, f.brand) {
                (m, Brand::HomeSmart) => format!("Caméra HomeSmart{} — image et son en RTSP, réglages par son agent, tout en local.", m.as_deref().map(|m| format!(" ({m})")).unwrap_or_default()),
                (Some(m), b) => format!("{} {m} — vidéo en RTSP, pilotage en ONVIF, tout en local.", b.label()),
                _ => "Vidéo en RTSP, pilotage en ONVIF, tout en local.".to_string(),
            };
            ui.label(RichText::new(sub).size(12.5).color(theme::muted()));
            ui.add_space(12.0);
            egui::Grid::new("add-cam-grid").num_columns(2).spacing([12.0, 9.0]).show(ui, |ui| {
                let w = 300.0;
                ui.label("Adresse IP");
                ui.add(egui::TextEdit::singleline(&mut f.host).hint_text("192.168.1.164").desired_width(w));
                ui.end_row();
                ui.label("Nom");
                ui.add(egui::TextEdit::singleline(&mut f.name).hint_text("Salon").desired_width(w));
                ui.end_row();
                ui.label("Pièce");
                ui.add(egui::TextEdit::singleline(&mut f.room).hint_text("Salon").desired_width(w));
                ui.end_row();
                if diy {
                    // Le code n'est pas secret une fois utilisé : affiché en clair, en capitales.
                    ui.label("Code d'appairage");
                    ui.add(egui::TextEdit::singleline(&mut f.password).hint_text("ABCD-EFGH").char_limit(12).font(egui::TextStyle::Monospace).desired_width(130.0));
                    ui.end_row();
                } else {
                    ui.label("Utilisateur");
                    ui.add(egui::TextEdit::singleline(&mut f.user).hint_text(if f.brand == Brand::Tapo { "nom du compte de la caméra" } else { "" }).desired_width(w));
                    ui.end_row();
                    ui.label(match f.brand {
                        Brand::Imou => "Code de sécurité",
                        Brand::Tapo => "Mot de passe du compte caméra",
                        _ => "Mot de passe",
                    });
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut f.password).password(!f.show_pw).hint_text(if f.brand == Brand::Imou { "étiquette « SC »" } else { "" }).desired_width(w - 56.0));
                        ui.checkbox(&mut f.show_pw, "voir");
                    });
                    ui.end_row();
                }
                ui.label("Marque");
                let mut brand = f.brand;
                egui::ComboBox::from_id_salt("add-cam-brand").selected_text(f.brand.label()).width(w).show_ui(ui, |ui| {
                    for b in Brand::ALL {
                        ui.selectable_value(&mut brand, b, b.label());
                    }
                });
                if brand != f.brand {
                    f.set_brand(brand);
                }
                ui.end_row();
            });
            ui.add_space(6.0);
            if diy {
                egui::CollapsingHeader::new(RichText::new("Avancé : port de l'agent").color(theme::accent())).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Port de l'agent");
                        ui.add(egui::TextEdit::singleline(&mut f.agent_port).desired_width(70.0));
                    });
                });
            } else {
                egui::CollapsingHeader::new(RichText::new("Avancé : ports et chemins RTSP").color(theme::accent())).show(ui, |ui| {
                    egui::Grid::new("add-cam-adv").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                        ui.label("Port RTSP");
                        ui.add(egui::TextEdit::singleline(&mut f.rtsp_port).desired_width(70.0));
                        ui.end_row();
                        ui.label("Port ONVIF");
                        ui.add(egui::TextEdit::singleline(&mut f.onvif_port).desired_width(70.0));
                        ui.end_row();
                        ui.label("Chemin HD");
                        ui.add(egui::TextEdit::singleline(&mut f.main_path).hint_text("automatique (marque ou ONVIF)").desired_width(250.0));
                        ui.end_row();
                        ui.label("Chemin fluide");
                        ui.add(egui::TextEdit::singleline(&mut f.sub_path).hint_text("automatique (marque ou ONVIF)").desired_width(250.0));
                        ui.end_row();
                    });
                });
            }
            if f.needs_account && f.brand == Brand::Tapo {
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Cette Tapo garde sa vidéo et son orientation fermées : elle n'a pas encore de compte caméra. Crée-le dans l'application Tapo (voir ci-dessous), puis reviens ici.")
                        .size(12.5)
                        .color(theme::warn()),
                );
            }
            if let Some(help) = f.brand.account_help() {
                let title = match f.brand {
                    Brand::Imou => "Où trouver le code de sécurité ?",
                    Brand::HomeSmart => "Où trouver le code d'appairage ?",
                    _ => "Où trouver ces identifiants ?",
                };
                egui::CollapsingHeader::new(RichText::new(title).color(theme::accent())).default_open(f.brand == Brand::Tapo).show(ui, |ui| {
                    ui.label(RichText::new(help).size(12.5).color(theme::muted()));
                });
            }
            // Résultat du test de connexion.
            let state = f.test.lock().unwrap().clone();
            match state {
                Some(None) => {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Test en cours (vidéo puis ONVIF)…").color(theme::muted()));
                    });
                    ctx.request_repaint_after(Duration::from_millis(200));
                }
                Some(Some(r)) => {
                    ui.add_space(6.0);
                    match (&r.video, &r.video_error) {
                        (Some(s), _) => ui.label(RichText::new(format!("✔  Vidéo : {}", s.summary())).color(theme::accent())),
                        (_, Some(e)) => ui.label(RichText::new(format!("✖  Vidéo : {e}")).color(theme::crit())),
                        _ => ui.label(""),
                    };
                    match &r.onvif_error {
                        None => {
                            let model = r.info.as_ref().map(|i| format!(" — {} {}", i.manufacturer, i.model)).unwrap_or_default();
                            ui.label(RichText::new(format!("✔  ONVIF{model}{}", if r.ptz { " · orientable" } else { "" })).color(theme::accent()))
                        }
                        Some(e) => ui.label(RichText::new(format!("✖  ONVIF : {e}")).color(theme::warn())),
                    };
                    if let Some(i) = &r.info {
                        if f.model.is_none() && !i.model.is_empty() {
                            f.model = Some(i.model.clone());
                        }
                        if f.serial.is_none() && !i.serial.is_empty() {
                            f.serial = Some(i.serial.clone());
                        }
                    }
                }
                None => {}
            }
            // Appairage d'une caméra HomeSmart : en cours, ou son résultat.
            let paired = f.pairing.lock().unwrap().clone();
            match paired {
                Some(None) => {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Appairage avec la caméra…").color(theme::muted()));
                    });
                    ctx.request_repaint_after(Duration::from_millis(200));
                }
                Some(Some(Ok(v))) => done = Some(v),
                Some(Some(Err(e))) => {
                    f.error = Some(e);
                    *f.pairing.lock().unwrap() = None;
                }
                None => {}
            }
            if let Some(e) = &f.error {
                ui.add_space(6.0);
                ui.label(RichText::new(e).color(theme::crit()));
            }
            });
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // Une Tapo n'a pas d'utilisateur par défaut : sans le compte de la caméra, rien ne marcherait.
                let diy = f.brand == Brand::HomeSmart;
                let pairing = matches!(*f.pairing.lock().unwrap(), Some(None));
                let valid = !f.host.trim().is_empty() && !(f.brand == Brand::Tapo && f.user.trim().is_empty()) && !(diy && !f.code_ok()) && !pairing;
                if ui.add_enabled(valid, widgets::primary_button(if diy { "Appairer" } else { "Ajouter" }, theme::accent()).min_size(vec2(96.0, 32.0))).clicked() {
                    submit = true;
                }
                let testing = matches!(*f.test.lock().unwrap(), Some(None));
                // Caméra HomeSmart : l'appairage vérifie tout (agent, code, puis la vidéo à l'ouverture).
                if !diy && ui.add_enabled(valid && !testing, egui::Button::new("Tester la connexion").min_size(vec2(0.0, 32.0))).clicked() {
                    test = true;
                }
                if ui.button("Annuler").clicked() {
                    close = true;
                }
            });
        });
        if modal.should_close() {
            close = true;
        }
        if rescan {
            self.start_cam_scan(Source::User);
        }
        if test {
            *f.test.lock().unwrap() = Some(None);
            let (hub, slot, ctx, cfg) = (self.hub.clone(), f.test.clone(), self.ctx.clone(), f.config());
            self.rt.spawn(async move {
                let r = hub.test_camera(&cfg, Source::User).await;
                *slot.lock().unwrap() = Some(Some(r));
                ctx.request_repaint();
            });
        }
        if submit && f.brand == Brand::HomeSmart {
            f.error = None;
            *f.pairing.lock().unwrap() = Some(None);
            let (hub, slot, ctx, cfg) = (self.hub.clone(), f.pairing.clone(), self.ctx.clone(), f.config());
            let port = f.agent_port.trim().parse().unwrap_or(homesmart_cam_proto::API_PORT);
            let code = f.password.clone();
            self.rt.spawn(async move {
                let r = hub.pair_camera(cfg, port, code, Source::User).await.map_err(|e| format!("{e:#}"));
                *slot.lock().unwrap() = Some(Some(r));
                ctx.request_repaint();
            });
        } else if submit {
            match self.hub.add_camera(f.config(), Source::User) {
                Ok(v) => {
                    self.toast(format!("« {} » ajoutée", v.name), false);
                    self.cams.selected = Some(v.id);
                    self.view = super::View::Cameras;
                    return;
                }
                Err(e) => f.error = Some(format!("{e:#}")),
            }
        }
        if let Some(v) = done {
            self.toast(format!("« {} » appairée et ajoutée", v.name), false);
            self.cams.selected = Some(v.id);
            self.view = super::View::Cameras;
            return;
        }
        if !close {
            self.cams.add = Some(f);
        }
    }
}

/// Ouvre le dossier des captures (en sélectionnant la dernière, sous Windows).
fn open_folder(last: Option<&std::path::Path>) {
    let dir = homesmart::camera::captures_dir();
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(windows)]
    {
        let mut cmd = std::process::Command::new("explorer");
        match last.filter(|p| p.exists()) {
            Some(p) => cmd.arg(format!("/select,{}", p.display())),
            None => cmd.arg(&dir),
        };
        let _ = cmd.spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = last;
        let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Appui maintenu 3 s à 60 images/s, relâché, puis une seconde sans rien ; à chaque image le
    /// pavé et le clavier proposent tous deux (dans l'ordre `pad_first` ou l'inverse), le clavier
    /// sans direction. Renvoie les ordres envoyés.
    fn hold(pad_first: bool) -> Vec<(String, PtzCmd)> {
        let mut d = PtzDriver::default();
        let mut sent = Vec::new();
        for frame in 0..240 {
            let pad = (frame < 180).then_some((0.6, 0.0));
            if pad_first {
                d.propose("cam", pad);
                d.propose("cam", None);
            } else {
                d.propose("cam", None);
                d.propose("cam", pad);
            }
            sent.extend(d.commit());
        }
        sent
    }

    #[test]
    fn held_press_sends_one_move_then_one_stop() {
        let expected = vec![("cam".to_string(), PtzCmd::Move(0.6, 0.0)), ("cam".to_string(), PtzCmd::Stop)];
        assert_eq!(hold(true), expected);
        assert_eq!(hold(false), expected);
    }

    #[test]
    fn direction_change_and_leaving() {
        let mut d = PtzDriver::default();
        d.propose("a", Some((0.6, 0.0)));
        assert_eq!(d.commit(), vec![("a".to_string(), PtzCmd::Move(0.6, 0.0))]);
        // Autre direction, même caméra : un nouveau Move, sans Stop entre les deux.
        d.propose("a", Some((-0.6, 0.0)));
        assert_eq!(d.commit(), vec![("a".to_string(), PtzCmd::Move(-0.6, 0.0))]);
        // Autre caméra : la première s'arrête avant que la seconde parte.
        d.propose("b", Some((0.6, 0.0)));
        assert_eq!(d.commit(), vec![("a".to_string(), PtzCmd::Stop), ("b".to_string(), PtzCmd::Move(0.6, 0.0))]);
        // Plus aucune proposition (vue quittée) : arrêt, une seule fois.
        assert_eq!(d.commit(), vec![("b".to_string(), PtzCmd::Stop)]);
        assert!(d.commit().is_empty());
    }

    #[test]
    fn resolution_buttons() {
        // Ranger 2C : 2304×1296, 1920×1080, 1280×960, 1280×720.
        let r = resolution_choices(&[(2304, 1296), (1920, 1080), (1280, 960), (1280, 720)]);
        let labels: Vec<(&str, bool)> = r.iter().map(|(l, _, _, ok)| (l.as_str(), *ok)).collect();
        assert_eq!(
            labels,
            vec![("480p (4:3)", false), ("720p", true), ("960p (4:3)", true), ("1080p", true), ("1296p (3 MP)", true), ("1440p (2K)", false)]
        );
    }
}
