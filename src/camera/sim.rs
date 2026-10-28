//! Caméra Imou simulée (mode démo et tests) : un vrai serveur RTSP (authentification
//! Digest, H.264 encodé par OpenH264, RTP entrelacé sur TCP) et un serveur ONVIF (PTZ,
//! positions mémorisées) qui se comportent comme la caméra. L'image est une pièce dessinée
//! qui défile quand on oriente la caméra, avec l'horodatage incrusté comme sur les vraies.

use super::{onvif, xml, Quality};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use md5::{Digest, Md5};
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, UsageType};
use openh264::formats::{RgbSliceU8, YUVBuffer};
use openh264::OpenH264API;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

pub const MODEL: &str = "IPC-K2EC-3H1W";

/// Réglage d'encodage d'un flux simulé (modifiable en ONVIF, comme sur la vraie caméra).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SimEncoder {
    pub width: usize,
    pub height: usize,
    pub fps: f32,
    pub kbps: u32,
}

/// Résolutions proposées pour le flux principal et le flux secondaire.
const MAIN_RESOLUTIONS: [(usize, usize); 4] = [(2560, 1440), (2304, 1296), (1920, 1080), (1280, 720)];
const SUB_RESOLUTIONS: [(usize, usize); 2] = [(640, 360), (320, 180)];

pub struct SimCamera {
    pub rtsp_port: u16,
    pub onvif_port: u16,
    pub state: Arc<SimState>,
}

pub struct SimState {
    pub user: String,
    pub password: String,
    pub serial: String,
    /// Variante du décor (couleurs, objets).
    scene: u32,
    ptz: Mutex<Ptz>,
    presets: Mutex<Vec<(String, String, f32, f32)>>,
    next_preset: AtomicU32,
    params: Mutex<HashMap<(usize, usize), (Vec<u8>, Vec<u8>)>>,
    /// [principal, secondaire]
    encoders: Mutex<[SimEncoder; 2]>,
    /// Essais d'identifiants ratés (pour vérifier que HomeSmart ne s'acharne pas).
    pub auth_failures: AtomicU32,
    /// Réglages d'image (luminosité, contraste, saturation, netteté, IR, WDR, contre-jour).
    pub imaging: Mutex<crate::camera::onvif::ImagingSettings>,
    /// La carte SD a-t-elle le son ? (faux : séquences muettes, pour les tests)
    pub replay_sound: AtomicBool,
    /// Flux coupé (caméra débranchée, Wi-Fi perdu) : la vidéo s'arrête, les connexions RTSP sont refusées.
    pub down: AtomicBool,
    /// Refuse le mouvement continu ONVIF (comme certaines Tapo) : seuls les déplacements relatifs marchent.
    pub relative_only: AtomicBool,
    /// Micro en PCM 16 bits à 16 kHz (L16, comme une caméra HomeSmart) au lieu du G.711 8 kHz.
    pub pcm_audio: AtomicBool,
}

#[derive(Default)]
struct Ptz {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    target: Option<(f32, f32)>,
    at: Option<Instant>,
}

impl Ptz {
    /// Position actuelle (−1…1), après application du mouvement en cours.
    fn advance(&mut self) -> (f32, f32) {
        let now = Instant::now();
        let dt = self.at.map_or(0.0, |t| now.duration_since(t).as_secs_f32()).min(0.5);
        self.at = Some(now);
        if let Some((tx, ty)) = self.target {
            let step = 0.9 * dt;
            let (dx, dy) = (tx - self.x, ty - self.y);
            let d = (dx * dx + dy * dy).sqrt();
            if d <= step {
                (self.x, self.y) = (tx, ty);
                self.target = None;
            } else {
                self.x += dx / d * step;
                self.y += dy / d * step;
            }
        } else {
            self.x = (self.x + self.vx * 0.6 * dt).clamp(-1.0, 1.0);
            self.y = (self.y - self.vy * 0.6 * dt).clamp(-1.0, 1.0);
        }
        (self.x, self.y)
    }
}

fn slot(q: Quality) -> usize {
    match q {
        Quality::Main => 0,
        Quality::Sub => 1,
    }
}

fn registry() -> &'static Mutex<HashMap<u16, Arc<SimState>>> {
    static R: std::sync::OnceLock<Mutex<HashMap<u16, Arc<SimState>>>> = std::sync::OnceLock::new();
    R.get_or_init(Mutex::default)
}

/// Caméra simulée qui sert ce port RTSP (démo : couper son flux).
pub fn by_port(rtsp_port: u16) -> Option<Arc<SimState>> {
    registry().lock().unwrap().get(&rtsp_port).cloned()
}

/// Démarre une caméra simulée sur 127.0.0.1 (ports choisis par le système).
pub async fn start(password: &str, scene: u32) -> Result<SimCamera> {
    start_with(password, scene, SimEncoder { width: 1280, height: 720, fps: 12.0, kbps: 1800 }).await
}

/// Comme `start`, avec le réglage du flux principal (ex. 2560×1440 à 60 i/s pour mesurer).
pub async fn start_with(password: &str, scene: u32, main: SimEncoder) -> Result<SimCamera> {
    let rtsp = TcpListener::bind("127.0.0.1:0").await?;
    let http = TcpListener::bind("127.0.0.1:0").await?;
    let (rtsp_port, onvif_port) = (rtsp.local_addr()?.port(), http.local_addr()?.port());
    let state = Arc::new(SimState {
        user: "admin".into(),
        password: password.to_string(),
        serial: format!("SIM{:012X}", 0xA1B2_0000_0000u64 + scene as u64),
        scene,
        ptz: Mutex::default(),
        presets: Mutex::new(vec![("1".into(), "Entrée".into(), 0.8, 0.0), ("2".into(), "Canapé".into(), -0.1, 0.2)]),
        next_preset: AtomicU32::new(3),
        params: Mutex::default(),
        encoders: Mutex::new([main, SimEncoder { width: 640, height: 360, fps: 12.0, kbps: 600 }]),
        auth_failures: AtomicU32::new(0),
        imaging: Mutex::new(crate::camera::onvif::ImagingSettings {
            brightness: Some(50.0),
            contrast: Some(50.0),
            saturation: Some(50.0),
            sharpness: Some(50.0),
            ir_cut: Some("AUTO".into()),
            wdr: Some("OFF".into()),
            wdr_level: Some(50.0),
            backlight: Some("OFF".into()),
        }),
        replay_sound: AtomicBool::new(true),
        down: AtomicBool::new(false),
        relative_only: AtomicBool::new(false),
        pcm_audio: AtomicBool::new(false),
    });
    registry().lock().unwrap().insert(rtsp_port, state.clone());
    let st = state.clone();
    tokio::spawn(async move {
        while let Ok((s, _)) = rtsp.accept().await {
            if st.down.load(Ordering::Relaxed) {
                drop(s);
                continue;
            }
            let st = st.clone();
            tokio::spawn(async move {
                if let Err(e) = rtsp_conn(s, st).await {
                    tracing::debug!("caméra simulée (RTSP) : {e:#}");
                }
            });
        }
    });
    let st = state.clone();
    tokio::spawn(async move {
        while let Ok((s, _)) = http.accept().await {
            let st = st.clone();
            tokio::spawn(async move {
                if let Err(e) = onvif_conn(s, st, rtsp_port, onvif_port).await {
                    tracing::debug!("caméra simulée (ONVIF) : {e:#}");
                }
            });
        }
    });
    Ok(SimCamera { rtsp_port, onvif_port, state })
}

// ---- Image -------------------------------------------------------------------------

const GLYPHS: &[(char, [u8; 7])] = &[
    ('0', [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110]),
    ('1', [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110]),
    ('2', [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111]),
    ('3', [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110]),
    ('4', [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010]),
    ('5', [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110]),
    ('6', [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110]),
    ('7', [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000]),
    ('8', [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110]),
    ('9', [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100]),
    ('-', [0, 0, 0, 0b11111, 0, 0, 0]),
    (':', [0, 0b01100, 0b01100, 0, 0b01100, 0b01100, 0]),
    ('I', [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110]),
    ('M', [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001]),
    ('O', [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110]),
    ('U', [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110]),
    ('S', [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110]),
];

fn text(rgb: &mut [u8], w: usize, h: usize, x0: usize, y0: usize, scale: usize, s: &str) {
    for (i, ch) in s.chars().enumerate() {
        let Some((_, g)) = GLYPHS.iter().find(|(c, _)| *c == ch) else { continue };
        for (row, bits) in g.iter().enumerate() {
            for col in 0..5 {
                if bits & (0b10000 >> col) == 0 {
                    continue;
                }
                let (px, py) = (x0 + (i * 6 + col) * scale, y0 + row * scale);
                // Contour sombre puis pixel blanc, comme l'incrustation des caméras.
                for (dx, dy, c) in [(1usize, 1usize, 0u8), (0, 0, 245)] {
                    for yy in 0..scale {
                        for xx in 0..scale {
                            let (x, y) = (px + xx + dx, py + yy + dy);
                            if x < w && y < h {
                                let o = (y * w + x) * 3;
                                rgb[o..o + 3].copy_from_slice(&[c, c, c]);
                            }
                        }
                    }
                }
            }
        }
    }
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

fn inside(u: f32, v: f32, u0: f32, v0: f32, u1: f32, v1: f32) -> bool {
    u >= u0 && u <= u1 && v >= v0 && v <= v1
}

/// Couleur du décor au point (u, v) du panorama, à l'instant t.
fn scene_color(u: f32, v: f32, t: f32, scene: u32) -> [f32; 3] {
    let warm = scene % 2 == 0;
    let wall = if warm { [206.0, 192.0, 170.0] } else { [176.0, 190.0, 198.0] };
    let mut c = if v < 0.25 {
        mix(wall, [wall[0] * 0.8, wall[1] * 0.8, wall[2] * 0.82], (v + 0.8) / 1.05)
    } else {
        // Parquet : lames décalées d'une rangée à l'autre.
        let row = ((v - 0.25) * 14.0).floor();
        let plank = ((u * 3.0 + row * 0.37).fract() + 1.0).fract();
        let base = [128.0, 88.0, 58.0];
        let shade = 0.85 + 0.15 * ((row * 1.7).sin() * 0.5 + 0.5);
        let mut b = [base[0] * shade, base[1] * shade, base[2] * shade];
        if plank < 0.02 || ((v - 0.25) * 14.0).fract() < 0.05 {
            b = [b[0] * 0.7, b[1] * 0.7, b[2] * 0.7];
        }
        b
    };
    if inside(u, v, -2.0, 0.24, 2.0, 0.27) {
        c = [235.0, 232.0, 226.0]; // plinthe
    }
    // Fenêtre : ciel et nuage qui passe.
    if inside(u, v, -0.95, -0.58, -0.4, 0.04) {
        let frame = 0.018;
        let on_frame = !inside(u, v, -0.95 + frame, -0.58 + frame, -0.4 - frame, 0.04 - frame)
            || (u - -0.675).abs() < frame * 0.5
            || (v - -0.27).abs() < frame * 0.5;
        if on_frame {
            c = [240.0, 240.0, 236.0];
        } else {
            let sky = mix([96.0, 160.0, 228.0], [190.0, 220.0, 245.0], (v + 0.58) / 0.62);
            let cx = -0.95 + ((t * 0.02).fract()) * 0.75;
            let d = ((u - cx).powi(2) * 4.0 + (v + 0.4).powi(2) * 12.0).sqrt();
            c = mix([250.0, 250.0, 252.0], sky, (d - 0.12) / 0.08);
        }
    }
    // Tableau.
    if inside(u, v, 0.05, -0.47, 0.36, -0.2) {
        c = if inside(u, v, 0.07, -0.45, 0.34, -0.22) {
            mix([230.0, 120.0, 80.0], [70.0, 110.0, 190.0], (u - 0.07) / 0.27 + (v + 0.45) * 0.8)
        } else {
            [60.0, 48.0, 40.0]
        };
    }
    // Porte.
    if inside(u, v, 0.95, -0.5, 1.28, 0.25) {
        c = if ((u - 1.22).powi(2) + (v + 0.1).powi(2)).sqrt() < 0.012 { [210.0, 180.0, 90.0] } else { [150.0, 112.0, 80.0] };
        if !inside(u, v, 0.965, -0.485, 1.265, 0.25) {
            c = [235.0, 232.0, 226.0];
        }
    }
    // Canapé.
    let sofa = if warm { [52.0, 110.0, 118.0] } else { [150.0, 62.0, 60.0] };
    if inside(u, v, -0.25, 0.02, 0.55, 0.3) {
        c = sofa;
        if inside(u, v, -0.22, 0.04, 0.52, 0.14) && ((u + 0.22) / 0.37).fract() < 0.03 {
            c = [sofa[0] * 0.7, sofa[1] * 0.7, sofa[2] * 0.7];
        }
        if v > 0.14 {
            c = [sofa[0] * 0.85, sofa[1] * 0.85, sofa[2] * 0.85];
        }
    }
    // Lampe : pied, abat-jour et halo.
    let glow = (-(((u - 0.75).powi(2) + (v + 0.33).powi(2)) * 18.0)).exp();
    c = mix(c, [255.0, 226.0, 160.0], glow * 0.55);
    if inside(u, v, 0.742, -0.3, 0.758, 0.26) {
        c = [70.0, 66.0, 60.0];
    }
    if v >= -0.42 && v <= -0.28 && (u - 0.75).abs() <= 0.05 + (v + 0.42) * 0.4 {
        c = [250.0, 236.0, 200.0];
    }
    // Le chat qui se promène (utile pour tester la détection de mouvement).
    let cat_u = (t * 0.35).sin() * 1.1;
    let dir = (t * 0.35).cos().signum();
    let body = ((u - cat_u) / 0.075).powi(2) + ((v - 0.47) / 0.035).powi(2);
    let head = ((u - cat_u - dir * 0.075) / 0.03).powi(2) + ((v - 0.43) / 0.03).powi(2);
    let tail = (u - cat_u + dir * 0.09).abs() < 0.008 && v > 0.40 && v < 0.47;
    if body < 1.0 || head < 1.0 || tail {
        c = [48.0, 46.0, 50.0];
    }
    // Ombre douce sous le canapé et vignettage.
    if inside(u, v, -0.27, 0.3, 0.57, 0.33) {
        c = [c[0] * 0.75, c[1] * 0.75, c[2] * 0.75];
    }
    c
}

/// Rend l'image vue par la caméra orientée en (pan, tilt).
/// Champ de vision de la caméra simulée (en unités du panorama).
fn fov(w: usize, h: usize) -> (f32, f32) {
    (1.2, 1.2 * h as f32 / w as f32)
}

/// Dessine une zone de l'image (x0..x1, y0..y1) vue depuis l'orientation (pan, tilt).
fn render_region(rgb: &mut [u8], w: usize, h: usize, pan: f32, tilt: f32, t: f32, scene: u32, (x0, y0, x1, y1): (usize, usize, usize, usize)) {
    let (fu, fv) = fov(w, h);
    for py in y0..y1.min(h) {
        let ny = py as f32 / h as f32 - 0.5;
        let v = tilt * 0.35 + ny * fv;
        for px in x0..x1.min(w) {
            let nx = px as f32 / w as f32 - 0.5;
            let u = pan * 0.75 + nx * fu;
            let mut c = scene_color(u, v, t, scene);
            let vig = 1.0 - 0.35 * (nx * nx + ny * ny);
            c = [c[0] * vig, c[1] * vig, c[2] * vig];
            let o = (py * w + px) * 3;
            rgb[o] = c[0].clamp(0.0, 255.0) as u8;
            rgb[o + 1] = c[1].clamp(0.0, 255.0) as u8;
            rgb[o + 2] = c[2].clamp(0.0, 255.0) as u8;
        }
    }
}

/// Rectangle de l'image (en pixels) couvrant la zone du panorama u0..u1, v0..v1.
fn to_pixels(w: usize, h: usize, pan: f32, tilt: f32, (u0, v0, u1, v1): (f32, f32, f32, f32)) -> (usize, usize, usize, usize) {
    let (fu, fv) = fov(w, h);
    let x = |u: f32| (((u - pan * 0.75) / fu + 0.5) * w as f32).clamp(0.0, w as f32) as usize;
    let y = |v: f32| (((v - tilt * 0.35) / fv + 0.5) * h as f32).clamp(0.0, h as f32) as usize;
    (x(u0), y(v0), x(u1) + 1, y(v1) + 1)
}

/// Zone du chat qui se promène (avec sa marge).
fn cat_zone(t: f32) -> (f32, f32, f32, f32) {
    let cat_u = (t * 0.35).sin() * 1.1;
    (cat_u - 0.16, 0.38, cat_u + 0.16, 0.52)
}

/// La fenêtre (nuages qui passent lentement).
const WINDOW_ZONE: (f32, f32, f32, f32) = (-0.95, -0.58, -0.4, 0.04);

fn overlays(rgb: &mut [u8], w: usize, h: usize, overlay: &str) {
    let scale = (h / 180).max(1);
    text(rgb, w, h, 8 * scale, 6 * scale, scale, overlay);
    text(rgb, w, h, w.saturating_sub(60 * scale), h.saturating_sub(14 * scale), scale, "IMOU SIM");
}

pub fn render(w: usize, h: usize, pan: f32, tilt: f32, t: f32, scene: u32, overlay: &str) -> Vec<u8> {
    let mut rgb = vec![0u8; w * h * 3];
    render_region(&mut rgb, w, h, pan, tilt, t, scene, (0, 0, w, h));
    overlays(&mut rgb, w, h, overlay);
    rgb
}

/// Photo de fond (développement, captures d'écran) : `HOMESMART_SIM_PHOTO` = un JPEG ou PNG.
/// Remplace la pièce dessinée, pour faire travailler l'analyse IA sur une vraie image (aucune
/// photo n'est livrée avec HomeSmart).
fn sim_photo(w: usize, h: usize) -> Option<Vec<u8>> {
    static PHOTO: std::sync::OnceLock<Option<super::Frame>> = std::sync::OnceLock::new();
    let f = PHOTO
        .get_or_init(|| std::env::var_os("HOMESMART_SIM_PHOTO").filter(|p| !p.is_empty()).and_then(|p| crate::vision::picture::load(std::path::Path::new(&p)).ok()))
        .as_ref()?;
    let r = crate::vision::picture::resize(f, w, h);
    Some(r.rgba.chunks(4).flat_map(|p| [p[0], p[1], p[2]]).collect())
}

/// Rendu incrémental : l'image complète n'est redessinée que si la caméra a tourné ;
/// sinon seules les zones animées (et l'horodatage) le sont. Permet 1440p à 60 i/s.
struct Painter {
    w: usize,
    h: usize,
    scene: u32,
    base: Vec<u8>,
    at: Option<(f32, f32)>,
    last_t: f32,
}

impl Painter {
    fn frame(&mut self, pan: f32, tilt: f32, t: f32, overlay: &str) -> Vec<u8> {
        let (w, h, scene) = (self.w, self.h, self.scene);
        if self.at != Some((pan, tilt)) {
            self.base = match sim_photo(w, h) {
                Some(photo) => photo,
                None => {
                    let mut base = vec![0u8; w * h * 3];
                    render_region(&mut base, w, h, pan, tilt, t, scene, (0, 0, w, h));
                    base
                }
            };
            self.at = Some((pan, tilt));
        } else if sim_photo(1, 1).is_none() {
            // Le chat : là où il était et là où il est ; les nuages, une fois par seconde.
            for z in [cat_zone(self.last_t), cat_zone(t)] {
                render_region(&mut self.base, w, h, pan, tilt, t, scene, to_pixels(w, h, pan, tilt, z));
            }
            if t as u32 != self.last_t as u32 {
                render_region(&mut self.base, w, h, pan, tilt, t, scene, to_pixels(w, h, pan, tilt, WINDOW_ZONE));
            }
        }
        self.last_t = t;
        let mut rgb = self.base.clone();
        overlays(&mut rgb, w, h, overlay);
        rgb
    }
}

fn encoder(e: SimEncoder) -> Result<Encoder> {
    let cfg = EncoderConfig::new()
        .bitrate(BitRate::from_bps(e.kbps * 1000))
        .max_frame_rate(FrameRate::from_hz(e.fps))
        .usage_type(UsageType::CameraVideoRealTime)
        .intra_frame_period(IntraFramePeriod::from_num_frames(24));
    Encoder::with_api_config(OpenH264API::from_source(), cfg).map_err(|e| anyhow!("encodeur H.264 : {e}"))
}

/// Unités NAL d'un flux Annex B, sans leurs préfixes.
fn nals(annexb: &[u8]) -> Vec<&[u8]> {
    openh264::nal_units(annexb)
        .map(|n| {
            let skip = n.iter().take_while(|&&b| b == 0).count();
            if n.get(skip) == Some(&1) {
                &n[skip + 1..]
            } else {
                n
            }
        })
        .filter(|n| !n.is_empty())
        .collect()
}

impl SimState {
    fn now_label() -> String {
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
    }

    /// SPS et PPS du flux (pour le SDP), obtenus en encodant une première image.
    fn parameter_sets(&self, w: usize, h: usize) -> Result<(Vec<u8>, Vec<u8>)> {
        if let Some(p) = self.params.lock().unwrap().get(&(w, h)) {
            return Ok(p.clone());
        }
        let mut enc = encoder(SimEncoder { width: w, height: h, fps: 12.0, kbps: 1000 })?;
        let rgb = vec![128u8; w * h * 3];
        let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&rgb, (w, h)));
        let bits = enc.encode(&yuv).map_err(|e| anyhow!("encodage : {e}"))?.to_vec();
        let (mut sps, mut pps) = (None, None);
        for n in nals(&bits) {
            match n[0] & 0x1f {
                7 => sps = Some(n.to_vec()),
                8 => pps = Some(n.to_vec()),
                _ => {}
            }
        }
        let p = (sps.ok_or_else(|| anyhow!("pas de SPS"))?, pps.ok_or_else(|| anyhow!("pas de PPS"))?);
        self.params.lock().unwrap().insert((w, h), p.clone());
        Ok(p)
    }

    pub fn encoder(&self, q: Quality) -> SimEncoder {
        self.encoders.lock().unwrap()[slot(q)]
    }

    pub fn position(&self) -> (f32, f32) {
        self.ptz.lock().unwrap().advance()
    }
}

// ---- RTSP --------------------------------------------------------------------------

struct Request {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

async fn read_request<R: tokio::io::AsyncBufRead + Unpin>(rd: &mut R) -> Result<Option<Request>> {
    let mut line = String::new();
    loop {
        line.clear();
        if rd.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        if !line.trim().is_empty() {
            break;
        }
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let url = parts.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    loop {
        line.clear();
        if rd.read_line(&mut line).await? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let len: usize = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    if len > 0 {
        let mut body = vec![0u8; len.min(65536)];
        rd.read_exact(&mut body).await?;
    }
    Ok(Some(Request { method, url, headers }))
}

fn md5_hex(s: &str) -> String {
    Md5::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Paramètres d'un en-tête `Authorization: Digest k="v", …`.
fn digest_params(h: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(rest) = h.trim().strip_prefix("Digest") else { return out };
    let mut s = rest.trim();
    while !s.is_empty() {
        let Some(eq) = s.find('=') else { break };
        let key = s[..eq].trim().trim_start_matches(',').trim().to_ascii_lowercase();
        let after = s[eq + 1..].trim_start();
        let (val, next) = if let Some(q) = after.strip_prefix('"') {
            let end = q.find('"').unwrap_or(q.len());
            (q[..end].to_string(), q.get(end + 1..).unwrap_or(""))
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (after[..end].trim().to_string(), &after[end..])
        };
        out.insert(key, val);
        s = next.trim_start().trim_start_matches(',').trim_start();
    }
    out
}

fn digest_ok(req: &Request, st: &SimState, realm: &str, nonce: &str) -> bool {
    let Some(p) = req.header("authorization").map(digest_params) else { return false };
    let get = |k: &str| p.get(k).map(String::as_str).unwrap_or("");
    if get("username") != st.user || get("nonce") != nonce {
        return false;
    }
    let ha1 = md5_hex(&format!("{}:{realm}:{}", st.user, st.password));
    let ha2 = md5_hex(&format!("{}:{}", req.method, get("uri")));
    let expected = match get("qop") {
        "" => md5_hex(&format!("{ha1}:{nonce}:{ha2}")),
        qop => md5_hex(&format!("{ha1}:{nonce}:{}:{}:{qop}:{ha2}", get("nc"), get("cnonce"))),
    };
    expected == get("response")
}

async fn send(wr: &tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>, status: &str, cseq: &str, headers: &[(&str, String)], body: &str) -> Result<()> {
    let mut out = format!("RTSP/1.0 {status}\r\nCSeq: {cseq}\r\nServer: Rtsp Server/3.0\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    out.push_str("\r\n");
    out.push_str(body);
    wr.lock().await.write_all(out.as_bytes()).await?;
    Ok(())
}

async fn rtsp_conn(stream: TcpStream, st: Arc<SimState>) -> Result<()> {
    let _ = stream.set_nodelay(true);
    let (rd, wr) = stream.into_split();
    let wr = Arc::new(tokio::sync::Mutex::new(wr));
    let mut rd = BufReader::new(rd);
    let realm = format!("Login to {}", st.serial);
    let nonce = format!("{:032X}", rand::random::<u128>());
    let mut quality = Quality::Sub;
    let mut setup_url = String::new();
    let session = format!("{:08X}", rand::random::<u32>());
    let mut playing: Option<Arc<AtomicBool>> = None;
    // Piste audio configurée : (URL de contrôle, canal entrelacé).
    let mut audio: Option<(String, u8)> = None;
    // Relecture de la carte SD demandée : les parties de séquences à envoyer.
    let mut replay: Option<Vec<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>> = None;
    loop {
        // Données entrelacées éventuelles du client (RTCP) : ignorées.
        let buf = rd.fill_buf().await?;
        if buf.is_empty() {
            break;
        }
        if buf[0] == b'$' {
            let mut head = [0u8; 4];
            rd.read_exact(&mut head).await?;
            let mut skip = vec![0u8; u16::from_be_bytes([head[2], head[3]]) as usize];
            rd.read_exact(&mut skip).await?;
            continue;
        }
        let Some(req) = read_request(&mut rd).await? else { break };
        let cseq = req.header("cseq").unwrap_or("0").to_string();
        let needs_auth = matches!(req.method.as_str(), "DESCRIBE" | "SETUP" | "PLAY");
        if needs_auth && !digest_ok(&req, &st, &realm, &nonce) {
            if req.header("authorization").is_some() {
                st.auth_failures.fetch_add(1, Ordering::Relaxed);
            }
            send(&wr, "401 Unauthorized", &cseq, &[("WWW-Authenticate", format!("Digest realm=\"{realm}\",nonce=\"{nonce}\""))], "").await?;
            continue;
        }
        match req.method.as_str() {
            "OPTIONS" => send(&wr, "200 OK", &cseq, &[("Public", "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER".into())], "").await?,
            "DESCRIBE" if playback_window(&req.url).is_some() => {
                // Carte SD : 404 si la période est vide, sinon l'étendue (1re → dernière image) en npt.
                let (a, b) = playback_window(&req.url).unwrap_or_default();
                let parts = sim_footage(st.scene, a, b);
                let (Some(first), Some(last)) = (parts.first(), parts.last()) else {
                    send(&wr, "404 Not Found", &cseq, &[], "").await?;
                    continue;
                };
                let span = (last.1 - first.0).num_milliseconds() as f64 / 1000.0;
                quality = Quality::Main;
                let e = st.encoder(quality);
                let (sps, pps) = st.parameter_sets(e.width, e.height)?;
                let profile: String = sps.get(1..4).unwrap_or(&[0x42, 0xc0, 0x1f]).iter().map(|b| format!("{b:02X}")).collect();
                let sdp = format!(
                    "v=0\r\no=- 2 2 IN IP4 127.0.0.1\r\ns=HomeSmart Sim Playback\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\na=control:*\r\na=range:npt=0-{span:.6}\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=fmtp:96 packetization-mode=1;profile-level-id={profile};sprop-parameter-sets={},{}\r\na=control:trackID=0\r\n{}",
                    B64.encode(&sps),
                    B64.encode(&pps),
                    if st.replay_sound.load(Ordering::Relaxed) { "m=audio 0 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=control:trackID=1\r\n" } else { "" }
                );
                replay = Some(parts);
                let base = format!("{}/", req.url.trim_end_matches('/'));
                send(&wr, "200 OK", &cseq, &[("Content-Type", "application/sdp".into()), ("Content-Base", base)], &sdp).await?;
            }
            "DESCRIBE" => {
                quality = if req.url.contains("subtype=0") { Quality::Main } else { Quality::Sub };
                let e = st.encoder(quality);
                let (w, h, fps) = (e.width, e.height, e.fps);
                let (sps, pps) = st.parameter_sets(w, h)?;
                let profile: String = sps.get(1..4).unwrap_or(&[0x42, 0xc0, 0x1f]).iter().map(|b| format!("{b:02X}")).collect();
                let audio_line = if st.pcm_audio.load(Ordering::Relaxed) { "m=audio 0 RTP/AVP 97\r\na=rtpmap:97 L16/16000/1" } else { "m=audio 0 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000" };
                let sdp = format!(
                    "v=0\r\no=- 2 2 IN IP4 127.0.0.1\r\ns=HomeSmart Sim\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\na=control:*\r\na=range:npt=now-\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=fmtp:96 packetization-mode=1;profile-level-id={profile};sprop-parameter-sets={},{}\r\na=framerate:{fps}\r\na=control:trackID=0\r\n{audio_line}\r\na=control:trackID=1\r\n",
                    B64.encode(&sps),
                    B64.encode(&pps)
                );
                let base = format!("{}/", req.url.trim_end_matches('/'));
                send(&wr, "200 OK", &cseq, &[("Content-Type", "application/sdp".into()), ("Content-Base", base)], &sdp).await?;
            }
            "SETUP" => {
                let transport = req.header("transport").unwrap_or("RTP/AVP/TCP;unicast;interleaved=0-1").to_string();
                if !transport.contains("TCP") {
                    send(&wr, "461 Unsupported Transport", &cseq, &[], "").await?;
                    continue;
                }
                if req.url.trim_end_matches('/').ends_with("trackID=1") {
                    // Piste audio : canal entrelacé demandé par le client (2-3 en général).
                    let ch = transport
                        .split(';')
                        .find_map(|p| p.strip_prefix("interleaved="))
                        .and_then(|r| r.split('-').next())
                        .and_then(|c| c.parse::<u8>().ok())
                        .unwrap_or(2);
                    audio = Some((req.url.clone(), ch));
                    send(&wr, "200 OK", &cseq, &[("Transport", format!("RTP/AVP/TCP;unicast;interleaved={}-{};ssrc=5E6F7A8B", ch, ch + 1)), ("Session", format!("{session};timeout=60"))], "").await?;
                    continue;
                }
                setup_url = req.url.clone();
                send(&wr, "200 OK", &cseq, &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1;ssrc=1A2B3C4D".into()), ("Session", format!("{session};timeout=60"))], "").await?;
            }
            "PLAY" => {
                let (seq0, ts0) = (rand::random::<u16>() & 0x7fff, rand::random::<u32>() & 0x7fff_ffff);
                let (aseq0, ats0) = (rand::random::<u16>() & 0x7fff, rand::random::<u32>() & 0x7fff_ffff);
                let mut info = format!("url={setup_url};seq={seq0};rtptime={ts0}");
                if let Some((url, _)) = &audio {
                    info.push_str(&format!(",url={url};seq={aseq0};rtptime={ats0}"));
                }
                send(&wr, "200 OK", &cseq, &[("Session", session.clone()), ("Range", "npt=0.000-".into()), ("RTP-Info", info)], "").await?;
                if playing.is_none() {
                    let stop = Arc::new(AtomicBool::new(false));
                    playing = Some(stop.clone());
                    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
                    let st2 = st.clone();
                    let (tx_audio, stop_audio) = (tx.clone(), stop.clone());
                    let parts = replay.clone();
                    // En relecture, le son part du même fil que l'image (mêmes horodatages, mêmes sauts).
                    let replay_audio = audio.as_ref().map(|(_, ch)| (*ch, aseq0, ats0));
                    std::thread::Builder::new()
                        .name("homesmart-sim-cam".into())
                        .spawn(move || {
                            let r = match &parts {
                                Some(parts) => replay_loop(&st2, parts, &tx, &stop, (seq0, ts0), replay_audio),
                                None => encode_loop(&st2, quality, &tx, &stop, seq0, ts0),
                            };
                            if let Err(e) = r {
                                tracing::debug!("caméra simulée : {e:#}");
                            }
                        })
                        .context("fil d'encodage")?;
                    if let (Some((_, ch)), None) = (audio.clone(), &replay) {
                        let pcm = st.pcm_audio.load(Ordering::Relaxed);
                        std::thread::Builder::new()
                            .name("homesmart-sim-micro".into())
                            .spawn(move || audio_loop(&tx_audio, &stop_audio, ch, aseq0, ats0, pcm))
                            .context("fil audio")?;
                    } else {
                        drop(tx_audio);
                    }
                    let wr2 = wr.clone();
                    let is_replay = replay.is_some();
                    let st3 = st.clone();
                    tokio::spawn(async move {
                        while let Some(buf) = rx.recv().await {
                            // Flux coupé : plus d'image, la connexion se ferme.
                            if st3.down.load(Ordering::Relaxed) {
                                let _ = wr2.lock().await.shutdown().await;
                                break;
                            }
                            if wr2.lock().await.write_all(&buf).await.is_err() {
                                break;
                            }
                        }
                        // Fin de la relecture : la caméra ferme la connexion.
                        if is_replay {
                            let _ = wr2.lock().await.shutdown().await;
                        }
                    });
                }
            }
            "TEARDOWN" => {
                send(&wr, "200 OK", &cseq, &[("Session", session.clone())], "").await?;
                break;
            }
            "GET_PARAMETER" | "SET_PARAMETER" => send(&wr, "200 OK", &cseq, &[("Session", session.clone())], "").await?,
            _ => send(&wr, "501 Not Implemented", &cseq, &[], "").await?,
        }
    }
    if let Some(stop) = playing {
        stop.store(true, Ordering::Relaxed);
    }
    Ok(())
}

/// G.711 µ-law : échantillon 16 bits → octet (algorithme de référence g711.c).
pub fn ulaw_encode(pcm: i16) -> u8 {
    const SEG_END: [i32; 8] = [0x3F, 0x7F, 0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF, 0x1FFF];
    let mut v = (pcm as i32) >> 2;
    let mask = if v < 0 {
        v = -v;
        0x7F
    } else {
        0xFF
    };
    v = v.min(8159) + (0x84 >> 2);
    match SEG_END.iter().position(|&e| v <= e) {
        None => (0x7F ^ mask) as u8,
        Some(seg) => ((((seg as i32) << 4) | ((v >> (seg + 1)) & 0x0F)) ^ mask) as u8,
    }
}

/// Micro simulé : un la (440 Hz) par paquets de 20 ms, en G.711 8 kHz, ou en PCM 16 bits
/// 16 kHz (`pcm`, type RTP 97).
fn audio_loop(tx: &tokio::sync::mpsc::Sender<Vec<u8>>, stop: &AtomicBool, channel: u8, mut seq: u16, ts0: u32, pcm: bool) {
    let (rate, pt, bytes_per_sample) = if pcm { (16_000u32, 97u8, 2usize) } else { (8_000, 0, 1) };
    let per_packet = rate / 50;
    let payload = per_packet as usize * bytes_per_sample;
    let start = Instant::now();
    let mut n: u32 = 0;
    while !stop.load(Ordering::Relaxed) {
        let due = start + Duration::from_millis(20) * n;
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let mut pkt = Vec::with_capacity(4 + 12 + payload);
        pkt.extend_from_slice(&[b'$', channel]);
        pkt.extend_from_slice(&((12 + payload) as u16).to_be_bytes());
        pkt.extend_from_slice(&[0x80, pt]);
        pkt.extend_from_slice(&seq.to_be_bytes());
        pkt.extend_from_slice(&ts0.wrapping_add(n * per_packet).to_be_bytes());
        pkt.extend_from_slice(&0x5E6F_7A8Bu32.to_be_bytes());
        for i in 0..per_packet {
            let t = (n * per_packet + i) as f32 / rate as f32;
            let s = ((t * 440.0 * std::f32::consts::TAU).sin() * 0.3 * 32767.0) as i16;
            if pcm {
                pkt.extend_from_slice(&s.to_be_bytes());
            } else {
                pkt.push(ulaw_encode(s));
            }
        }
        seq = seq.wrapping_add(1);
        if tx.blocking_send(pkt).is_err() {
            break;
        }
        n += 1;
    }
}

// ---- Carte SD simulée ------------------------------------------------------------------

/// Séquences « enregistrées » d'une journée (détection de mouvement : 25 à 85 s, plus nombreuses
/// le matin et le soir), sur les 3 derniers jours seulement, et jamais dans le futur.
pub fn sim_clips(date: chrono::NaiveDate, scene: u32) -> Vec<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    use chrono::Datelike;
    let age = (chrono::Local::now().date_naive() - date).num_days();
    if !(0..=2).contains(&age) {
        return Vec::new();
    }
    let mut seed = (date.num_days_from_ce() as u64).wrapping_mul(2_654_435_761) ^ (scene as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut all = Vec::new();
    for h in 0..24u32 {
        let n = match h {
            0..=5 => rnd() % 2,
            6..=8 => 2 + rnd() % 3,
            9..=16 => rnd() % 3,
            17..=22 => 3 + rnd() % 5,
            _ => rnd() % 2,
        };
        let Some((a, _)) = super::playback::hour_window(date, h) else { continue };
        for _ in 0..n {
            let s = a + chrono::Duration::seconds((rnd() % 3540) as i64);
            all.push((s, s + chrono::Duration::seconds(25 + (rnd() % 60) as i64)));
        }
    }
    all.sort();
    let now = chrono::Utc::now();
    let mut out: Vec<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> = Vec::new();
    for (s, e) in all {
        // Au moins 90 s entre deux détections (délai de réarmement, comme sur les vraies caméras).
        if s >= now || out.last().is_some_and(|l| s <= l.1 + chrono::Duration::seconds(90)) {
            continue;
        }
        out.push((s, e.min(now)));
    }
    out
}

/// Parties de séquences comprises dans une période de relecture (dans l'ordre).
fn sim_footage(scene: u32, a: chrono::DateTime<chrono::Utc>, b: chrono::DateTime<chrono::Utc>) -> Vec<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    use chrono::Local;
    let (d0, d1) = (a.with_timezone(&Local).date_naive(), b.with_timezone(&Local).date_naive());
    let mut out = Vec::new();
    let mut d = d0.pred_opt().unwrap_or(d0);
    while d <= d1 {
        for (s, e) in sim_clips(d, scene) {
            let (s, e) = (s.max(a), e.min(b));
            if s < e {
                out.push((s, e));
            }
        }
        let Some(next) = d.succ_opt() else { break };
        d = next;
    }
    out.sort();
    out.dedup();
    out
}

/// Période d'une URL `/cam/playback?…&starttime=AAAA_MM_JJ_HH_MM_SS&endtime=…` (UTC).
fn playback_window(url: &str) -> Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    if !url.contains("/cam/playback") {
        return None;
    }
    let get = |k: &str| {
        let i = url.find(&format!("{k}="))? + k.len() + 1;
        let v = url[i..].split(['&', '/', ' ']).next()?;
        chrono::NaiveDateTime::parse_from_str(v, "%Y_%m_%d_%H_%M_%S").ok().map(|n| n.and_utc())
    };
    Some((get("starttime")?, get("endtime")?))
}

/// Découpe une image encodée en paquets RTP entrelacés (RFC 6184, FU-A).
fn packetize(units: &[&[u8]], seq: &mut u16, ts: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(units.iter().map(|u| u.len()).sum::<usize>() + 64);
    for (i, nal) in units.iter().enumerate() {
        let last = i + 1 == units.len();
        let mut push = |payload: &[&[u8]], marker: bool| {
            let len: usize = payload.iter().map(|p| p.len()).sum::<usize>() + 12;
            out.push(b'$');
            out.push(0);
            out.extend_from_slice(&(len as u16).to_be_bytes());
            out.push(0x80);
            out.push(if marker { 0x80 | 96 } else { 96 });
            out.extend_from_slice(&seq.to_be_bytes());
            out.extend_from_slice(&ts.to_be_bytes());
            out.extend_from_slice(&0x1A2B_3C4Du32.to_be_bytes());
            for p in payload {
                out.extend_from_slice(p);
            }
            *seq = seq.wrapping_add(1);
        };
        const MTU: usize = 1400;
        if nal.len() <= MTU {
            push(&[nal], last);
        } else {
            let (hdr, body) = (nal[0], &nal[1..]);
            let indicator = (hdr & 0xe0) | 28;
            let chunks: Vec<&[u8]> = body.chunks(MTU - 2).collect();
            for (j, c) in chunks.iter().enumerate() {
                let mut fu = hdr & 0x1f;
                if j == 0 {
                    fu |= 0x80;
                }
                if j + 1 == chunks.len() {
                    fu |= 0x40;
                }
                push(&[&[indicator, fu], c], last && j + 1 == chunks.len());
            }
        }
    }
    out
}

/// Paquet RTP entrelacé de son G.711 µ-law (20 ms à 8 kHz).
fn ulaw_packet(channel: u8, seq: u16, ts: u32, samples: impl Iterator<Item = i16>) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(4 + 12 + 160);
    pkt.extend_from_slice(&[b'$', channel]);
    pkt.extend_from_slice(&((12 + 160) as u16).to_be_bytes());
    pkt.extend_from_slice(&[0x80, 0]);
    pkt.extend_from_slice(&seq.to_be_bytes());
    pkt.extend_from_slice(&ts.to_be_bytes());
    pkt.extend_from_slice(&0x5E6F_7A8Bu32.to_be_bytes());
    pkt.extend(samples.map(ulaw_encode));
    pkt
}

/// Relecture de la carte simulée, comme la vraie caméra : chaque séquence à vitesse réelle, les
/// trous sautés aussitôt (l'horodatage RTP fait le saut), l'heure d'enregistrement incrustée.
/// `audio` : (canal entrelacé, n° de séquence, horodatage RTP) de la piste son si le client l'a
/// demandée : un bip par seconde, d'une hauteur propre à chaque séquence.
fn replay_loop(
    st: &SimState,
    parts: &[(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)],
    tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    stop: &AtomicBool,
    (mut seq, ts0): (u16, u32),
    mut audio: Option<(u8, u16, u32)>,
) -> Result<()> {
    let Some(&(origin, _)) = parts.first() else { return Ok(()) };
    let e = st.encoder(Quality::Main);
    let (w, h, fps) = (e.width, e.height, e.fps);
    let mut enc = encoder(e)?;
    let mut painter = Painter { w, h, scene: st.scene, base: Vec::new(), at: None, last_t: 0.0 };
    let start = Instant::now();
    let mut played = 0.0f64;
    for &(a, b) in parts {
        // Chaque séquence a son cadrage (la caméra a tourné entre deux détections).
        let k = a.timestamp().unsigned_abs();
        let (pan, tilt) = (((k % 17) as f32 / 8.0 - 1.0) * 0.8, ((k % 7) as f32 / 3.0 - 1.0) * 0.4);
        let len = (b - a).num_milliseconds() as f64 / 1000.0;
        let pitch = 330.0 + (k % 6) as f32 * 90.0;
        // Son déjà envoyé dans cette séquence, en paquets de 20 ms.
        let mut packets = 0u32;
        let mut n = 0u32;
        loop {
            let t = n as f64 / fps as f64;
            if t >= len || stop.load(Ordering::Relaxed) {
                break;
            }
            if let Some(wait) = (start + Duration::from_secs_f64(played + t)).checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            // Le son jusqu'à la fin de cette image (même horloge que l'image, trous sautés compris).
            if let Some((ch, aseq, ats0)) = audio.as_mut() {
                while (packets as f64 + 1.0) * 0.02 <= (t + 1.0 / fps as f64).min(len) {
                    let s0 = packets * 160;
                    let at = a + chrono::Duration::milliseconds(packets as i64 * 20);
                    let ts = ats0.wrapping_add(((at - origin).num_milliseconds() as f64 * 8.0) as u32);
                    let tone = (0..160u32).map(|i| {
                        let x = (s0 + i) as f32 / 8000.0;
                        let on = (x % 1.0) < 0.25; // bip de 250 ms chaque seconde
                        if on { ((x * pitch * std::f32::consts::TAU).sin() * 0.3 * 32767.0) as i16 } else { 0 }
                    });
                    if tx.blocking_send(ulaw_packet(*ch, *aseq, ts, tone)).is_err() {
                        return Ok(());
                    }
                    *aseq = aseq.wrapping_add(1);
                    packets += 1;
                }
            }
            let at = a + chrono::Duration::milliseconds((t * 1000.0) as i64);
            let label = at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string();
            let rgb = painter.frame(pan, tilt, (at.timestamp() % 100_000) as f32 + (t.fract() as f32), &label);
            let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&rgb, (w, h)));
            let bits = enc.encode(&yuv).map_err(|e| anyhow!("encodage : {e}"))?.to_vec();
            let ts = ts0.wrapping_add(((at - origin).num_milliseconds() as f64 * 90.0) as u32);
            if tx.blocking_send(packetize(&nals(&bits), &mut seq, ts)).is_err() {
                return Ok(());
            }
            n += 1;
        }
        played += len;
    }
    Ok(())
}

/// Encode les images au rythme de la caméra et les découpe en paquets RTP (RFC 6184, FU-A).
fn encode_loop(st: &SimState, q: Quality, tx: &tokio::sync::mpsc::Sender<Vec<u8>>, stop: &AtomicBool, mut seq: u16, ts0: u32) -> Result<()> {
    let e = st.encoder(q);
    let (w, h, fps) = (e.width, e.height, e.fps);
    let mut enc = encoder(e)?;
    let mut painter = Painter { w, h, scene: st.scene, base: Vec::new(), at: None, last_t: 0.0 };
    let start = Instant::now();
    let period = Duration::from_secs_f32(1.0 / fps);
    let mut n: u32 = 0;
    while !stop.load(Ordering::Relaxed) {
        let due = start + period * n;
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let (pan, tilt) = st.position();
        let mut rgb = painter.frame(pan, tilt, start.elapsed().as_secs_f32(), &SimState::now_label());
        let (b, c) = {
            let i = st.imaging.lock().unwrap();
            (i.brightness.unwrap_or(50.0), i.contrast.unwrap_or(50.0))
        };
        if b != 50.0 || c != 50.0 {
            let (gain, off) = (c / 50.0, (b - 50.0) * 2.0);
            for v in rgb.iter_mut() {
                *v = ((*v as f32 - 128.0) * gain + 128.0 + off).clamp(0.0, 255.0) as u8;
            }
        }
        let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&rgb, (w, h)));
        let bits = enc.encode(&yuv).map_err(|e| anyhow!("encodage : {e}"))?.to_vec();
        let ts = ts0.wrapping_add((n as f32 * 90_000.0 / fps) as u32);
        if tx.blocking_send(packetize(&nals(&bits), &mut seq, ts)).is_err() {
            break;
        }
        n += 1;
    }
    Ok(())
}

// ---- ONVIF -------------------------------------------------------------------------

fn soap_ok(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8" standalone="yes" ?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl"><s:Body>{body}</s:Body></s:Envelope>"#
    )
}

/// Vérifie le jeton WS-Security d'une requête.
fn wsse_ok(req: &str, st: &SimState) -> bool {
    let Some(tok) = xml::element(req, "UsernameToken") else { return false };
    let (Some(user), Some(digest), Some(nonce), Some(created)) = (tok.find_text("Username"), tok.find_text("Password"), tok.find_text("Nonce"), tok.find_text("Created")) else {
        return false;
    };
    let Ok(nonce) = B64.decode(nonce.as_bytes()) else { return false };
    user == st.user && onvif::password_digest(&nonce, &created, &st.password) == digest
}

fn onvif_answer(req: &str, st: &SimState, rtsp_port: u16, onvif_port: u16) -> String {
    let body = xml::element(req, "Body").map(|b| b.inner).unwrap_or("");
    let action = body.trim_start().strip_prefix('<').and_then(|s| s.split([' ', '>', '/']).next()).unwrap_or("");
    let action = action.rsplit(':').next().unwrap_or(action).to_string();
    let base = format!("http://127.0.0.1:{onvif_port}/onvif");
    let token = |el: &str| xml::text(body, el).unwrap_or_default();
    match action.as_str() {
        "GetSystemDateAndTime" => {
            let n = chrono::Utc::now();
            use chrono::{Datelike, Timelike};
            return soap_ok(&format!(
                "<tds:GetSystemDateAndTimeResponse><tds:SystemDateAndTime><tt:DateTimeType>Manual</tt:DateTimeType><tt:UTCDateTime><tt:Time><tt:Hour>{}</tt:Hour><tt:Minute>{}</tt:Minute><tt:Second>{}</tt:Second></tt:Time><tt:Date><tt:Year>{}</tt:Year><tt:Month>{}</tt:Month><tt:Day>{}</tt:Day></tt:Date></tt:UTCDateTime></tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse>",
                n.hour(), n.minute(), n.second(), n.year(), n.month(), n.day()
            ));
        }
        "GetCapabilities" => {
            return soap_ok(&format!(
                "<tds:GetCapabilitiesResponse><tds:Capabilities><tt:Device><tt:XAddr>{base}/device_service</tt:XAddr></tt:Device><tt:Events><tt:XAddr>{base}/event_service</tt:XAddr></tt:Events><tt:Media><tt:XAddr>{base}/media_service</tt:XAddr></tt:Media><tt:PTZ><tt:XAddr>{base}/ptz_service</tt:XAddr></tt:PTZ><tt:Imaging><tt:XAddr>{base}/imaging_service</tt:XAddr></tt:Imaging></tds:Capabilities></tds:GetCapabilitiesResponse>"
            ));
        }
        _ => {}
    }
    // Comme les Imou : sans authentification valide, un corps de réponse vide.
    if !wsse_ok(req, st) {
        return soap_ok("");
    }
    match action.as_str() {
        "GetDeviceInformation" => soap_ok(&format!(
            "<tds:GetDeviceInformationResponse><tds:Manufacturer>Imou (simulée)</tds:Manufacturer><tds:Model>{MODEL}</tds:Model><tds:FirmwareVersion>2.800.0000000.22.R</tds:FirmwareVersion><tds:SerialNumber>{}</tds:SerialNumber><tds:HardwareId>1.00</tds:HardwareId></tds:GetDeviceInformationResponse>",
            st.serial
        )),
        "GetProfiles" => {
            let profile = |i: u32, q: Quality| {
                let e = st.encoder(q);
                let (w, h) = (e.width, e.height);
                format!("<trt:Profiles token=\"MediaProfile0000{i}\" fixed=\"true\"><tt:Name>Profile{i}</tt:Name><tt:VideoSourceConfiguration token=\"VSC0\"><tt:SourceToken>VideoSource0</tt:SourceToken></tt:VideoSourceConfiguration><tt:VideoEncoderConfiguration token=\"V{i}\"><tt:Encoding>H264</tt:Encoding><tt:Resolution><tt:Width>{w}</tt:Width><tt:Height>{h}</tt:Height></tt:Resolution></tt:VideoEncoderConfiguration><tt:PTZConfiguration token=\"PTZ0\"><tt:Name>PTZ</tt:Name></tt:PTZConfiguration></trt:Profiles>")
            };
            soap_ok(&format!("<trt:GetProfilesResponse>{}{}</trt:GetProfilesResponse>", profile(0, Quality::Main), profile(1, Quality::Sub)))
        }
        "GetImagingSettings" => {
            let i = st.imaging.lock().unwrap().clone();
            let n = |v: Option<f32>| v.unwrap_or(50.0);
            soap_ok(&format!(
                "<timg:GetImagingSettingsResponse xmlns:timg=\"http://www.onvif.org/ver20/imaging/wsdl\"><timg:ImagingSettings><tt:BacklightCompensation><tt:Mode>{}</tt:Mode></tt:BacklightCompensation><tt:Brightness>{}</tt:Brightness><tt:ColorSaturation>{}</tt:ColorSaturation><tt:Contrast>{}</tt:Contrast><tt:IrCutFilter>{}</tt:IrCutFilter><tt:Sharpness>{}</tt:Sharpness><tt:WideDynamicRange><tt:Mode>{}</tt:Mode><tt:Level>{}</tt:Level></tt:WideDynamicRange></timg:ImagingSettings></timg:GetImagingSettingsResponse>",
                i.backlight.unwrap_or_default(), n(i.brightness), n(i.saturation), n(i.contrast), i.ir_cut.unwrap_or_default(), n(i.sharpness), i.wdr.unwrap_or_default(), n(i.wdr_level)
            ))
        }
        "GetOptions" => soap_ok(
            "<timg:GetOptionsResponse xmlns:timg=\"http://www.onvif.org/ver20/imaging/wsdl\"><timg:ImagingOptions><tt:BacklightCompensation><tt:Mode>OFF</tt:Mode><tt:Mode>ON</tt:Mode></tt:BacklightCompensation><tt:Brightness><tt:Min>0</tt:Min><tt:Max>100</tt:Max></tt:Brightness><tt:ColorSaturation><tt:Min>0</tt:Min><tt:Max>100</tt:Max></tt:ColorSaturation><tt:Contrast><tt:Min>0</tt:Min><tt:Max>100</tt:Max></tt:Contrast><tt:IrCutFilterModes>ON</tt:IrCutFilterModes><tt:IrCutFilterModes>OFF</tt:IrCutFilterModes><tt:IrCutFilterModes>AUTO</tt:IrCutFilterModes><tt:Sharpness><tt:Min>0</tt:Min><tt:Max>100</tt:Max></tt:Sharpness><tt:WideDynamicRange><tt:Mode>OFF</tt:Mode><tt:Mode>ON</tt:Mode><tt:Level><tt:Min>0</tt:Min><tt:Max>100</tt:Max></tt:Level></tt:WideDynamicRange></timg:ImagingOptions></timg:GetOptionsResponse>",
        ),
        "SetImagingSettings" => {
            let new = crate::camera::onvif::parse_imaging_settings(body);
            let mut i = st.imaging.lock().unwrap();
            macro_rules! take {
                ($f:ident) => {
                    if new.$f.is_some() {
                        i.$f = new.$f.clone();
                    }
                };
            }
            take!(brightness);
            take!(contrast);
            take!(saturation);
            take!(sharpness);
            take!(ir_cut);
            take!(wdr);
            take!(wdr_level);
            take!(backlight);
            soap_ok("<timg:SetImagingSettingsResponse xmlns:timg=\"http://www.onvif.org/ver20/imaging/wsdl\"/>")
        }
        "GetVideoEncoderConfigurations" => {
            let conf = |i: usize, q: Quality| {
                let e = st.encoder(q);
                format!(
                    "<trt:Configurations token=\"VideoEncoder00{i}\"><tt:Name>VideoEncoder00{i}</tt:Name><tt:UseCount>1</tt:UseCount><tt:Encoding>H264</tt:Encoding><tt:Resolution><tt:Width>{}</tt:Width><tt:Height>{}</tt:Height></tt:Resolution><tt:Quality>4</tt:Quality><tt:RateControl><tt:FrameRateLimit>{}</tt:FrameRateLimit><tt:EncodingInterval>1</tt:EncodingInterval><tt:BitrateLimit>{}</tt:BitrateLimit></tt:RateControl><tt:H264><tt:GovLength>{}</tt:GovLength><tt:H264Profile>Main</tt:H264Profile></tt:H264></trt:Configurations>",
                    e.width, e.height, e.fps as u32, e.kbps, (e.fps as u32 * 2).max(1)
                )
            };
            soap_ok(&format!("<trt:GetVideoEncoderConfigurationsResponse>{}{}</trt:GetVideoEncoderConfigurationsResponse>", conf(0, Quality::Main), conf(1, Quality::Sub)))
        }
        "GetVideoEncoderConfigurationOptions" => {
            let list: &[(usize, usize)] = if token("ConfigurationToken").ends_with('1') { &SUB_RESOLUTIONS } else { &MAIN_RESOLUTIONS };
            let res: String = list.iter().map(|(w, h)| format!("<tt:ResolutionsAvailable><tt:Width>{w}</tt:Width><tt:Height>{h}</tt:Height></tt:ResolutionsAvailable>")).collect();
            soap_ok(&format!(
                "<trt:GetVideoEncoderConfigurationOptionsResponse><trt:Options><tt:QualityRange><tt:Min>1</tt:Min><tt:Max>6</tt:Max></tt:QualityRange><tt:H264>{res}<tt:GovLengthRange><tt:Min>1</tt:Min><tt:Max>150</tt:Max></tt:GovLengthRange><tt:FrameRateRange><tt:Min>1</tt:Min><tt:Max>60</tt:Max></tt:FrameRateRange><tt:EncodingIntervalRange><tt:Min>1</tt:Min><tt:Max>1</tt:Max></tt:EncodingIntervalRange><tt:H264ProfilesSupported>Main</tt:H264ProfilesSupported></tt:H264><tt:Extension><tt:H264><tt:BitrateRange><tt:Min>128</tt:Min><tt:Max>12288</tt:Max></tt:BitrateRange></tt:H264></tt:Extension></trt:Options></trt:GetVideoEncoderConfigurationOptionsResponse>"
            ))
        }
        "SetVideoEncoderConfiguration" => {
            let conf = xml::element(body, "Configuration");
            let num = |name: &str| conf.and_then(|c| c.find_text(name)).and_then(|s| s.parse::<f32>().ok());
            let i = if conf.and_then(|c| c.attr("token")).is_some_and(|t| t.ends_with('1')) { 1 } else { 0 };
            let allowed: &[(usize, usize)] = if i == 1 { &SUB_RESOLUTIONS } else { &MAIN_RESOLUTIONS };
            let (w, h) = (num("Width").unwrap_or(0.0) as usize, num("Height").unwrap_or(0.0) as usize);
            if !allowed.contains(&(w, h)) {
                return soap_ok("<s:Fault><s:Code><s:Value>s:Sender</s:Value><s:Subcode><s:Value>ter:ConfigModify</s:Value></s:Subcode></s:Code><s:Reason><s:Text>Resolution not supported</s:Text></s:Reason></s:Fault>");
            }
            let mut enc = st.encoders.lock().unwrap();
            enc[i] = SimEncoder { width: w, height: h, fps: num("FrameRateLimit").unwrap_or(12.0).clamp(1.0, 60.0), kbps: num("BitrateLimit").unwrap_or(1000.0).clamp(128.0, 12288.0) as u32 };
            soap_ok("<trt:SetVideoEncoderConfigurationResponse/>")
        }
        "GetStreamUri" => {
            let sub = if token("ProfileToken").ends_with('1') { 1 } else { 0 };
            soap_ok(&format!(
                "<trt:GetStreamUriResponse><trt:MediaUri><tt:Uri>rtsp://127.0.0.1:{rtsp_port}/cam/realmonitor?channel=1&amp;subtype={sub}</tt:Uri></trt:MediaUri></trt:GetStreamUriResponse>"
            ))
        }
        "ContinuousMove" if st.relative_only.load(Ordering::Relaxed) => {
            soap_ok("<s:Fault><s:Code><s:Value>s:Receiver</s:Value><s:Subcode><s:Value>ter:ActionNotSupported</s:Value></s:Subcode></s:Code><s:Reason><s:Text>Optional Action Not Implemented</s:Text></s:Reason></s:Fault>")
        }
        "RelativeMove" => {
            let pt = xml::element(body, "PanTilt");
            let f = |k: &str| pt.and_then(|p| p.attr(k)).and_then(|s| s.parse::<f32>().ok()).unwrap_or(0.0);
            let mut p = st.ptz.lock().unwrap();
            let (x, y) = p.advance();
            let (bx, by) = p.target.unwrap_or((x, y));
            p.target = Some(((bx + f("x")).clamp(-1.0, 1.0), (by + f("y")).clamp(-1.0, 1.0)));
            soap_ok("<tptz:RelativeMoveResponse/>")
        }
        "ContinuousMove" => {
            let pt = xml::element(body, "PanTilt");
            let f = |k: &str| pt.and_then(|p| p.attr(k)).and_then(|s| s.parse::<f32>().ok()).unwrap_or(0.0);
            let mut p = st.ptz.lock().unwrap();
            p.advance();
            (p.vx, p.vy, p.target) = (f("x").clamp(-1.0, 1.0), f("y").clamp(-1.0, 1.0), None);
            soap_ok("<tptz:ContinuousMoveResponse/>")
        }
        "Stop" => {
            let mut p = st.ptz.lock().unwrap();
            p.advance();
            (p.vx, p.vy) = (0.0, 0.0);
            soap_ok("<tptz:StopResponse/>")
        }
        "AbsoluteMove" => {
            let pt = xml::element(body, "PanTilt");
            let f = |k: &str| pt.and_then(|p| p.attr(k)).and_then(|s| s.parse::<f32>().ok()).unwrap_or(0.0);
            let mut p = st.ptz.lock().unwrap();
            p.advance();
            p.target = Some((f("x").clamp(-1.0, 1.0), f("y").clamp(-1.0, 1.0)));
            soap_ok("<tptz:AbsoluteMoveResponse/>")
        }
        "GotoHomePosition" => {
            let mut p = st.ptz.lock().unwrap();
            p.advance();
            p.target = Some((0.0, 0.0));
            soap_ok("<tptz:GotoHomePositionResponse/>")
        }
        "GetPresets" => {
            let list: String = st
                .presets
                .lock()
                .unwrap()
                .iter()
                .map(|(t, n, _, _)| format!("<tptz:Preset token=\"{t}\"><tt:Name>{}</tt:Name></tptz:Preset>", xml::escape(n)))
                .collect();
            soap_ok(&format!("<tptz:GetPresetsResponse>{list}</tptz:GetPresetsResponse>"))
        }
        "GotoPreset" => {
            let t = token("PresetToken");
            let target = st.presets.lock().unwrap().iter().find(|p| p.0 == t).map(|p| (p.2, p.3));
            match target {
                Some(xy) => {
                    let mut p = st.ptz.lock().unwrap();
                    p.advance();
                    p.target = Some(xy);
                    soap_ok("<tptz:GotoPresetResponse/>")
                }
                None => soap_ok("<s:Fault><s:Code><s:Value>s:Sender</s:Value><s:Subcode><s:Value>ter:NoToken</s:Value></s:Subcode></s:Code><s:Reason><s:Text>No such preset</s:Text></s:Reason></s:Fault>"),
            }
        }
        "SetPreset" => {
            let name = token("PresetName");
            let (x, y) = st.position();
            let t = st.next_preset.fetch_add(1, Ordering::Relaxed).to_string();
            st.presets.lock().unwrap().push((t.clone(), name, x, y));
            soap_ok(&format!("<tptz:SetPresetResponse><tptz:PresetToken>{t}</tptz:PresetToken></tptz:SetPresetResponse>"))
        }
        "RemovePreset" => {
            let t = token("PresetToken");
            st.presets.lock().unwrap().retain(|p| p.0 != t);
            soap_ok("<tptz:RemovePresetResponse/>")
        }
        _ => soap_ok("<s:Fault><s:Code><s:Value>s:Receiver</s:Value><s:Subcode><s:Value>ter:ActionNotSupported</s:Value></s:Subcode></s:Code><s:Reason><s:Text>Action not supported</s:Text></s:Reason></s:Fault>"),
    }
}

async fn onvif_conn(stream: TcpStream, st: Arc<SimState>, rtsp_port: u16, onvif_port: u16) -> Result<()> {
    let mut rd = BufReader::new(stream);
    let mut len = 0usize;
    let mut line = String::new();
    loop {
        line.clear();
        if rd.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; len.min(1 << 20)];
    rd.read_exact(&mut body).await?;
    let answer = onvif_answer(&String::from_utf8_lossy(&body), &st, rtsp_port, onvif_port);
    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/soap+xml; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}", answer.len());
    rd.get_mut().write_all(resp.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::{stream, CameraConfig, Quality};

    #[test]
    fn digest_header_parsing() {
        let p = digest_params(r#"Digest username="admin", realm="Login to X", nonce="N", uri="rtsp://h/a?b=1", response="abc", algorithm=MD5"#);
        assert_eq!(p["username"], "admin");
        assert_eq!(p["uri"], "rtsp://h/a?b=1");
        assert_eq!(p["algorithm"], "MD5");
    }

    #[test]
    fn ulaw_roundtrip() {
        for x in [-32000i16, -1000, -8, 0, 8, 1000, 12345, 32000] {
            let back = crate::camera::audio::ulaw(ulaw_encode(x)) as i32;
            assert!((back - x as i32).abs() <= (x as i32).abs() / 16 + 16, "{x} → {back}");
        }
    }

    /// Son : piste audio RTSP mise en place, décodée et livrée (le la de la caméra simulée).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn audio_track() {
        let sim = start("SON12345", 0).await.unwrap();
        let mut cfg = CameraConfig::new("127.0.0.1");
        (cfg.rtsp_port, cfg.password) = (sim.rtsp_port, "SON12345".into());
        let got = Arc::new(Mutex::new((Vec::<f32>::new(), 0u32)));
        let g2 = got.clone();
        let audio: stream::AudioSink = Arc::new(move |s: &[f32], rate| {
            let mut g = g2.lock().unwrap();
            g.0.extend_from_slice(s);
            g.1 = rate;
        });
        let stats = Arc::new(Mutex::new(crate::camera::StreamStats::default()));
        let st2 = stats.clone();
        let url = cfg.rtsp_url(Quality::Sub);
        let run = stream::run_with_audio(&url, "admin", "SON12345", Arc::new(|_| {}), move |s| *st2.lock().unwrap() = s, Some(audio));
        let _ = tokio::time::timeout(Duration::from_millis(1500), run).await;
        let (samples, rate) = got.lock().unwrap().clone();
        assert_eq!(rate, 8000);
        assert!(samples.len() > 4000, "{} échantillons", samples.len());
        let peak = samples.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!((0.2..0.4).contains(&peak), "crête {peak}");
        assert_eq!(stats.lock().unwrap().audio, "G.711 8 kHz");
    }

    /// Son PCM 16 bits à 16 kHz (L16, micro d'une caméra HomeSmart) : décodé sans perte.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pcm_audio_track() {
        let sim = start("PCM12345", 0).await.unwrap();
        sim.state.pcm_audio.store(true, Ordering::Relaxed);
        let mut cfg = CameraConfig::new("127.0.0.1");
        (cfg.rtsp_port, cfg.password) = (sim.rtsp_port, "PCM12345".into());
        let tracks = stream::tracks(&cfg.rtsp_url(Quality::Sub), "admin", "PCM12345").await.unwrap();
        let a = tracks.iter().find(|t| t.media == "audio").unwrap();
        assert_eq!((a.encoding.to_ascii_lowercase().as_str(), a.clock_rate, a.channels), ("l16", 16_000, Some(1)));
        let got = Arc::new(Mutex::new((Vec::<f32>::new(), 0u32)));
        let g2 = got.clone();
        let audio: stream::AudioSink = Arc::new(move |s: &[f32], rate| {
            let mut g = g2.lock().unwrap();
            g.0.extend_from_slice(s);
            g.1 = rate;
        });
        let stats = Arc::new(Mutex::new(crate::camera::StreamStats::default()));
        let st2 = stats.clone();
        let url = cfg.rtsp_url(Quality::Sub);
        let run = stream::run_with_audio(&url, "admin", "PCM12345", Arc::new(|_| {}), move |s| *st2.lock().unwrap() = s, Some(audio));
        let _ = tokio::time::timeout(Duration::from_millis(1500), run).await;
        let (samples, rate) = got.lock().unwrap().clone();
        assert_eq!(rate, 16_000);
        assert!(samples.len() > 8000, "{} échantillons", samples.len());
        // Sans compression : la crête du la reste à 0,3 à 1/32768 près (le G.711 s'en écarte).
        let peak = samples.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!((peak - 0.3).abs() < 0.001, "crête {peak}");
        assert_eq!(stats.lock().unwrap().audio, "PCM 16 kHz");
    }

    /// Mesure : flux 1440p à 60 i/s de bout en bout (lancer avec `--ignored --nocapture`, en release).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore]
    async fn bench_1440p60() {
        let main = SimEncoder { width: 2560, height: 1440, fps: 60.0, kbps: 8000 };
        let sim = start_with("BENCH123", 0, main).await.unwrap();
        let mut cfg = CameraConfig::new("127.0.0.1");
        (cfg.rtsp_port, cfg.password) = (sim.rtsp_port, "BENCH123".into());
        let last = Arc::new(Mutex::new(Vec::new()));
        let l2 = last.clone();
        let sink: stream::FrameSink = Arc::new(|_| {});
        let url = cfg.rtsp_url(Quality::Main);
        let run = stream::run(&url, "admin", "BENCH123", sink, move |s| l2.lock().unwrap().push(s));
        let _ = tokio::time::timeout(Duration::from_secs(8), run).await;
        for s in last.lock().unwrap().iter().filter(|s| s.fps > 0.0) {
            println!("{} · décodage {:.2} ms/image · {} sautée(s)", s.summary(), s.decode_ms, s.dropped);
        }
    }

    /// Toute la chaîne : RTSP + Digest + RTP/FU-A + décodage OpenH264, puis ONVIF + PTZ.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_to_end() {
        let sim = start("L2A3B4C5", 1).await.unwrap();
        let mut cfg = CameraConfig::new("127.0.0.1");
        (cfg.rtsp_port, cfg.onvif_port, cfg.password) = (sim.rtsp_port, sim.onvif_port, "L2A3B4C5".into());

        let (frame, stats) = stream::grab(&cfg.rtsp_url(Quality::Sub), "admin", &cfg.password, Duration::from_secs(20)).await.unwrap();
        assert_eq!((frame.width, frame.height), (640, 360));
        assert_eq!(frame.rgba.len(), 640 * 360 * 4);
        assert_eq!(stats.codec, "H.264");
        // Le mur (haut de l'image) est clair : l'image est bien décodée, pas noire.
        let o = (40 * 640 + 320) * 4;
        assert!(frame.rgba[o] > 100, "pixel {:?}", &frame.rgba[o..o + 4]);

        let bad = stream::grab(&cfg.rtsp_url(Quality::Sub), "admin", "faux", Duration::from_secs(10)).await.unwrap_err();
        assert!(stream::describe_error(&bad).contains("identifiants refusés"), "{bad:#}");

        let (host, port, pass) = (cfg.host.clone(), cfg.onvif_port, cfg.password.clone());
        let st = sim.state.clone();
        tokio::task::spawn_blocking(move || {
            let o = onvif::Onvif::connect(&host, port, "admin", &pass).unwrap();
            assert!(o.ptz_supported());
            assert_eq!(o.device_info().unwrap().model, MODEL);
            o.continuous_move(1.0, 0.0).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            o.stop().unwrap();
            assert!(st.position().0 > 0.05);
            let token = o.set_preset("Bureau").unwrap();
            assert!(o.presets().unwrap().iter().any(|p| p.token == token && p.name == "Bureau"));
            o.goto_preset("1").unwrap();
            o.remove_preset(&token).unwrap();
            assert!(!o.presets().unwrap().iter().any(|p| p.token == token));
            let err = onvif::Onvif::connect(&host, port, "admin", "faux").and_then(|o| o.device_info()).unwrap_err();
            assert!(format!("{err:#}").contains("identifiants refusés"), "{err:#}");
        })
        .await
        .unwrap();
    }
}
