//! `homesmart-cli vision violence-eval` : coups et violence sur des vidéos réelles (jeu
//! d'évaluation local, jamais livré avec HomeSmart), avec la chaîne de l'application : personnes
//! et squelettes (RTMO), suivi, règles de violence. Chiffres seulement.
//!
//! Vidéos cherchées dans `--data` (à télécharger à part, pour un usage local seulement) :
//! - `violence/airtlab/…` : AIRTLab (Bianculli et al., Data in Brief 2020, recherche et
//!   enseignement ; github.com/airtlab/A-Dataset-for-Automatic-Violence-Detection-in-Videos,
//!   1,1 Go), 350 scènes jouées dans une pièce, filmées par deux caméras en hauteur. Les scènes
//!   sans violence sont choisies pour tromper : accolades, tapes dans la main, sauts de joie… ;
//! - `violence/survfight/…` : Surveillance Camera Fight Dataset (Aktı et al., IPTA 2019, licence
//!   MIT ; github.com/seymanurakti/fight-detection-surv-dataset, 88 Mo), 300 extraits de 2 s de
//!   vraies caméras de surveillance (bagarres et scènes ordinaires) ;
//! - `violence/utinteraction/{violent,non-violent}/…` : UT-Interaction, version découpée
//!   (Ryoo et Aggarwal, SDHA 2010 ; cvrc.ece.utexas.edu/SDHA2010, 30 Mo), 120 scènes d'une seule
//!   action, rangées selon le dernier chiffre du nom : 2 coup de pied, 4 coup de poing, 5 poussée
//!   (violent) ; 0 poignée de main, 1 accolade, 3 montrer du doigt (non-violent) ;
//! - `falls/…` : chutes et vie courante (UR Fall, CAUCAFall) : aucune violence ;
//! - `friendly/kinetics/<classe>/…` : jeux, sports et gestes amicaux de Kinetics-700-2020
//!   (DeepMind ; miroir s3.amazonaws.com/kinetics/700_2020/val, une archive par classe, environ
//!   50 extraits de 10 s de YouTube) : bataille d'oreillers, chatouilles, lutte, bras de fer, boxe,
//!   capoeira, plaquages, tapes dans la main, accolades… Aucune n'est une agression : la moindre
//!   alerte est fausse ;
//! - `ambiguous/kinetics/<classe>/…` : gifles de Kinetics (concours, farces, vraies gifles) :
//!   comptées à part.
//!
//! Une vidéo est violente si son chemin contient `violent` ou `fight` (mais pas `non-violent` ni
//! `nofight`, ni sous `friendly/` ou `ambiguous/`) ; tout le reste est de la vie courante, où la
//! moindre alerte est fausse.
//!
//! `--cache` garde les personnes trouvées par RTMO, vidéo par vidéo : les essais suivants des
//! règles ne refont pas l'analyse d'image (quelques secondes au lieu de plusieurs minutes).

use anyhow::{anyhow, bail, Result};
use clap::Args;
use homesmart::camera::Frame;
use homesmart::vision::engine::Accel;
use homesmart::vision::geom::{BBox, Point};
use homesmart::vision::onnx::RtmoPersons;
use homesmart::vision::pipeline::{PersonDetector, Pipeline};
use homesmart::vision::track::{Detection, Tracker};
use homesmart::vision::violence::{ViolenceDetector, ViolenceRules};
use homesmart::vision::{Keypoint, Pose};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Args)]
pub struct ViolenceEval {
    /// Dossier du jeu d'évaluation (contient violence/ et falls/)
    #[arg(long)]
    data: PathBuf,
    #[arg(long)]
    cpu: bool,
    /// Analyses par seconde (comme le réglage de l'application)
    #[arg(long, default_value_t = 8.0)]
    fps: f32,
    #[arg(long)]
    limit: Option<usize>,
    /// Seulement les vidéos dont le chemin contient ce texte
    #[arg(long)]
    only: Option<String>,
    /// Détail vidéo par vidéo
    #[arg(long)]
    verbose: bool,
    /// Modèle des personnes : s (standard) ou m (précision élevée)
    #[arg(long, default_value = "s")]
    model: String,
    /// Dossier où garder les personnes trouvées (essais suivants bien plus rapides)
    #[arg(long)]
    cache: Option<PathBuf>,
    /// Écrit les personnes suivies, image par image (boîtes et squelettes, chiffres seulement)
    #[arg(long)]
    dump: Option<PathBuf>,
    /// Écrit chaque coup vu (CSV, chiffres seulement) avec ce qui suit chez celui qui est frappé,
    /// et la liste des vidéos à côté (même nom, `.videos.csv`)
    #[arg(long)]
    strike_log: Option<PathBuf>,
    /// Réglages à essayer (sinon ceux de l'application, voir `ViolenceRules`)
    #[arg(long)]
    min_point: Option<f32>,
    #[arg(long)]
    wrist_speed: Option<f32>,
    #[arg(long)]
    ankle_speed: Option<f32>,
    #[arg(long)]
    reach: Option<f32>,
    #[arg(long, allow_hyphen_values = true)]
    toward: Option<f32>,
    #[arg(long, allow_hyphen_values = true)]
    wrist_low: Option<f32>,
    #[arg(long)]
    steady: Option<f32>,
    #[arg(long)]
    same_person: Option<f32>,
    #[arg(long)]
    base: Option<f64>,
    #[arg(long)]
    strikes: Option<u32>,
    #[arg(long)]
    window: Option<f64>,
    #[arg(long)]
    max_people: Option<usize>,
    #[arg(long, allow_hyphen_values = true)]
    edge: Option<f32>,
    /// Mesure aussi le son des vidéos qui en ont (CED : cris, chocs, rires), et l'effet des rires
    /// entendus sur les alertes (comme l'application quand l'écoute des sons est activée)
    #[arg(long)]
    sounds: bool,
}

/// Sons AudioSet liés à une bagarre : cris…
const SHOUTS: [&str; 5] = ["Shout", "Bellow", "Yell", "Battle cry", "Screaming"];
/// … et chocs.
const HITS: [&str; 4] = ["Slap, smack", "Whack, thwack", "Thump, thud", "Smash, crash"];
/// Rires : une chamaillerie, pas une agression ?
const LAUGHS: [&str; 6] = ["Laughter", "Giggle", "Belly laugh", "Chuckle, chortle", "Snicker", "Baby laughter"];

/// Son de la vidéo en 16 kHz mono (vide si elle n'en a pas).
fn audio16k(path: &Path) -> Result<Vec<f32>> {
    let ffmpeg = homesmart::camera::ffmpeg::find().ok_or_else(|| anyhow!("FFmpeg introuvable"))?;
    let out = std::process::Command::new(&ffmpeg)
        .args(["-hide_banner", "-v", "error", "-i"])
        .arg(path)
        .args(["-vn", "-ac", "1", "-ar", "16000", "-f", "f32le", "-"])
        .stderr(std::process::Stdio::null())
        .output()?;
    Ok(out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect())
}

/// Plus forte probabilité de cris, de chocs et de rires sur la vidéo (fenêtres de 3 s, pas de
/// 0,5 s, comme l'écoute de l'application).
fn sound_peaks(tagger: &mut homesmart::listen::sounds::SoundTagger, x: &[f32]) -> Result<(f32, f32, f32)> {
    let idx = |names: &[&str]| -> Vec<usize> { tagger.labels.iter().enumerate().filter(|(_, l)| names.contains(&l.as_str())).map(|(i, _)| i).collect() };
    let (shouts, hits, laughs) = (idx(&SHOUTS), idx(&HITS), idx(&LAUGHS));
    let (win, hop) = (48_000usize, 8_000usize);
    let mut peaks = (0f32, 0f32, 0f32);
    let mut start = 0;
    loop {
        let w = &x[start..(start + win).min(x.len())];
        let p = tagger.scores(w)?;
        peaks.0 = shouts.iter().map(|&i| p[i]).fold(peaks.0, f32::max);
        peaks.1 = hits.iter().map(|&i| p[i]).fold(peaks.1, f32::max);
        peaks.2 = laughs.iter().map(|&i| p[i]).fold(peaks.2, f32::max);
        if start + win >= x.len() {
            break;
        }
        start += hop;
    }
    Ok(peaks)
}

/// Images d'une vidéo, décodées par FFmpeg à `fps` images par seconde (au plus 1280 de large :
/// le modèle travaille de toute façon en 640).
struct Video {
    child: std::process::Child,
    width: usize,
    height: usize,
}

impl Video {
    fn open(path: &Path, fps: f32) -> Result<Video> {
        let ffmpeg = homesmart::camera::ffmpeg::find().ok_or_else(|| anyhow!("FFmpeg introuvable"))?;
        let probe = std::process::Command::new(&ffmpeg).args(["-hide_banner", "-i"]).arg(path).output()?;
        let text = String::from_utf8_lossy(&probe.stderr);
        let (width, height) = text
            .lines()
            .filter(|l| l.contains("Video:"))
            .find_map(|l| {
                l.split([',', ' ']).find_map(|w| {
                    let (a, b) = w.split_once('x')?;
                    Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
                })
            })
            .ok_or_else(|| anyhow!("dimensions de {} introuvables", path.display()))?;
        // UR Fall : profondeur à gauche, couleur à droite (640×240) ; on ne garde que la couleur.
        let (filter, width, height) = if (width, height) == (640, 240) {
            (format!("crop=320:240:320:0,fps={fps}"), 320, 240)
        } else if width > 1280 {
            let h = ((height as f32 * 1280.0 / width as f32 / 2.0).round() as usize) * 2;
            (format!("fps={fps},scale=1280:{h}"), 1280, h)
        } else {
            (format!("fps={fps}"), width, height)
        };
        let child = std::process::Command::new(&ffmpeg)
            .args(["-hide_banner", "-v", "error", "-i"])
            .arg(path)
            .args(["-vf", &filter, "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        Ok(Video { child, width, height })
    }

    fn next(&mut self) -> Option<Frame> {
        use std::io::Read;
        let mut rgba = vec![0u8; self.width * self.height * 4];
        self.child.stdout.as_mut()?.read_exact(&mut rgba).ok()?;
        Some(Frame { seq: 0, width: self.width, height: self.height, rgba })
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(super) fn videos(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            videos(&p, out);
        } else if p.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "mp4" | "avi" | "webm" | "mkv" | "mpeg" | "mpg")) {
            out.push(p);
        }
    }
}

/// Une piste à une image : instant, boîte, squelette.
type Seen = (f64, BBox, Option<Pose>);

/// Milieu du buste (ou de la boîte), en pixels.
fn torso_px(s: &Seen, fw: f32, fh: f32) -> Point {
    let c = s
        .2
        .as_ref()
        .and_then(|p| {
            let pts: Vec<Point> = [5, 6, 11, 12].iter().map(|&i| p.points[i]).filter(|k| k.score >= 0.3).map(|k| k.at).collect();
            (pts.len() >= 3).then(|| Point::new(pts.iter().map(|p| p.x).sum::<f32>() / pts.len() as f32, pts.iter().map(|p| p.y).sum::<f32>() / pts.len() as f32))
        })
        .unwrap_or(s.1.center());
    Point::new(c.x * fw, c.y * fh)
}

/// Tête (nez, sinon yeux et oreilles), en pixels.
fn head_px(s: &Seen, fw: f32, fh: f32) -> Option<Point> {
    let p = s.2.as_ref()?;
    let pts: Vec<Point> = (0..5).map(|i| p.points[i]).filter(|k| k.score >= 0.3).map(|k| k.at).collect();
    (!pts.is_empty()).then(|| Point::new(pts.iter().map(|p| p.x).sum::<f32>() / pts.len() as f32 * fw, pts.iter().map(|p| p.y).sum::<f32>() / pts.len() as f32 * fh))
}

fn dist(a: Point, b: Point) -> f32 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

/// Ce qui suit un coup chez celui qui est frappé, en tailles de sa silhouette : recul du buste
/// dans la demi-seconde (et avant, pour comparer), tête qui part par rapport au buste, silhouette
/// qui se tasse, écart entre les deux personnes une seconde après.
fn strike_row(s: &homesmart::vision::violence::Strike, hist: &HashMap<u32, Vec<Seen>>, fw: f32, fh: f32) -> Option<String> {
    let (to, from) = (hist.get(&s.to)?, hist.get(&s.from)?);
    let at = |h: &Vec<Seen>, t: f64| h.iter().min_by(|a, b| (a.0 - t).abs().total_cmp(&(b.0 - t).abs())).cloned();
    let now = at(to, s.at).filter(|x| (x.0 - s.at).abs() < 1e-6)?;
    let size = now.1.height() * fh;
    let c0 = torso_px(&now, fw, fh);
    let window = move |lo: f64, hi: f64| to.iter().filter(move |x| x.0 > lo + 1e-6 && x.0 <= hi + 1e-6);
    let recoil = window(s.at, s.at + 0.5).map(|x| dist(torso_px(x, fw, fh), c0)).fold(0f32, f32::max) / size;
    let before = window(s.at - 0.5, s.at - 0.01).map(|x| dist(torso_px(x, fw, fh), c0)).fold(0f32, f32::max) / size;
    let rel = |x: &Seen| head_px(x, fw, fh).map(|h| Point::new(h.x - torso_px(x, fw, fh).x, h.y - torso_px(x, fw, fh).y));
    let head = rel(&now).map(|h0| window(s.at, s.at + 0.5).filter_map(rel).map(|h| dist(h, h0)).fold(0f32, f32::max) / size).unwrap_or(-1.0);
    let shrink = window(s.at, s.at + 1.0).map(|x| x.1.height() / now.1.height()).fold(1f32, f32::min);
    let gap = |t: f64| Some(dist(torso_px(&at(to, t)?, fw, fh), torso_px(&at(from, t)?, fw, fh)) / size);
    let (g0, g1) = (gap(s.at)?, gap(s.at + 1.0)?);
    // Celui qui frappe : membre levé (0 = bas de sa boîte, 1 = haut), membre tendu (distance à
    // l'épaule ou à la hanche du même côté, en tailles), confiance du point.
    let me = at(from, s.at).filter(|x| (x.0 - s.at).abs() < 1e-6)?;
    let pose = me.2.as_ref()?;
    let k = pose.points[s.limb];
    let lift = (me.1.y1 - k.at.y) / me.1.height().max(1e-6);
    let root_pt = pose.points[match s.limb {
        9 => 5,
        10 => 6,
        15 => 11,
        _ => 12,
    }];
    let reach = dist(Point::new(k.at.x * fw, k.at.y * fh), Point::new(root_pt.at.x * fw, root_pt.at.y * fh)) / (me.1.height() * fh);
    // Corps coupé par le bord de l'image (celui qui frappe, celui qui est frappé).
    let cut = |b: &BBox| (b.x0 < 0.01 || b.y0 < 0.01 || b.x1 > 0.99 || b.y1 > 0.99) as u8;
    // Personnes à cet instant, et mouvement d'ensemble (médiane du déplacement des bustes des
    // autres pistes depuis l'image d'avant, en tailles) : la caméra bouge-t-elle ?
    let others: Vec<f32> = hist
        .iter()
        .filter(|(id, _)| **id != s.from && **id != s.to)
        .filter_map(|(_, h)| {
            let i = h.iter().position(|x| (x.0 - s.at).abs() < 1e-6)?;
            let (p0, p1) = (h.get(i.checked_sub(1)?)?, &h[i]);
            (s.at - p0.0 < 0.3).then(|| dist(torso_px(p1, fw, fh), torso_px(p0, fw, fh)) / (p1.1.height() * fh))
        })
        .collect();
    let people = 2 + hist.iter().filter(|(id, h)| **id != s.from && **id != s.to && h.iter().any(|x| (x.0 - s.at).abs() < 1e-6)).count();
    let mut sorted = others.clone();
    sorted.sort_by(f32::total_cmp);
    let global = sorted.get(sorted.len() / 2).copied().unwrap_or(-1.0);
    Some(format!(
        "{:.3},{},{},{},{:.3},{:.3},{:.3},{recoil:.4},{before:.4},{head:.4},{shrink:.3},{g0:.3},{g1:.3},{lift:.3},{reach:.3},{:.3},{},{},{people},{global:.4}",
        s.at,
        s.from,
        s.to,
        s.limb,
        s.speed,
        s.height,
        s.toward,
        k.score,
        cut(&me.1),
        cut(&now.1)
    ))
}

/// Personnes trouvées dans chaque image d'une vidéo, et taille des images.
pub(super) struct Found {
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) frames: Vec<Vec<Detection>>,
}

/// Personnes trouvées par RTMO dans chaque image de `path`, lues dans le cache `cache` si elles y
/// sont (sinon calculées, puis rangées) ; `night` : vision nocturne simulée avant l'analyse.
#[allow(clippy::too_many_arguments)]
pub(super) fn found(path: &Path, rel: &str, fps: f32, night: bool, model: &str, accel: Accel, cache: Option<&Path>, rtmo: &mut Option<RtmoPersons>) -> Result<Found> {
    let cached = cache.map(|c| c.join(format!("{}.{model}.{fps}{}.txt", rel.replace(['/', ':'], "__"), if night { ".night" } else { "" })));
    if let Some(p) = cached.as_ref().filter(|p| p.is_file()) {
        return Found::load(p);
    }
    let spec = match model {
        "s" => &homesmart::vision::models::RTMO_S,
        "m" => &homesmart::vision::models::RTMO_M,
        other => bail!("modèle « {other} » inconnu (s ou m)"),
    };
    let persons = match rtmo.as_mut() {
        Some(r) => r,
        None => rtmo.insert(RtmoPersons::with_model(spec, accel)?),
    };
    let mut video = Video::open(path, fps)?;
    let mut frames = Vec::new();
    while let Some(frame) = video.next() {
        let frame = if night { super::eval::night(frame, frames.len() as u64) } else { frame };
        frames.push(persons.detect(&frame)?);
    }
    let f = Found { width: video.width, height: video.height, frames };
    if let Some(p) = &cached {
        f.save(p)?;
    }
    Ok(f)
}

impl Found {
    fn save(&self, path: &Path) -> Result<()> {
        let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
        writeln!(f, "{} {} {}", self.width, self.height, self.frames.len())?;
        for (i, dets) in self.frames.iter().enumerate() {
            for d in dets {
                write!(f, "{i} {:.4} {:.5} {:.5} {:.5} {:.5}", d.score, d.bbox.x0, d.bbox.y0, d.bbox.x1, d.bbox.y1)?;
                if let Some(p) = &d.pose {
                    for k in &p.points {
                        write!(f, " {:.5} {:.5} {:.4}", k.at.x, k.at.y, k.score)?;
                    }
                }
                writeln!(f)?;
            }
        }
        Ok(())
    }

    fn load(path: &Path) -> Result<Found> {
        let f = std::io::BufReader::new(std::fs::File::open(path)?);
        let mut lines = f.lines();
        let head: Vec<usize> = lines.next().ok_or_else(|| anyhow!("cache vide"))??.split_whitespace().map(|v| v.parse()).collect::<Result<_, _>>()?;
        let [width, height, n] = head[..] else { bail!("cache : en-tête invalide") };
        let mut frames = vec![Vec::new(); n];
        for line in lines {
            let v: Vec<f32> = line?.split_whitespace().map(|v| v.parse()).collect::<Result<_, _>>()?;
            if v.len() < 6 {
                bail!("cache : ligne invalide");
            }
            let pose = (v.len() >= 6 + 51).then(|| {
                let mut points = [Keypoint::default(); 17];
                for (k, p) in points.iter_mut().enumerate() {
                    *p = Keypoint { at: Point::new(v[6 + k * 3], v[7 + k * 3]), score: v[8 + k * 3] };
                }
                Pose { points }
            });
            let i = v[0] as usize;
            frames.get_mut(i).ok_or_else(|| anyhow!("cache : image {i} hors de la vidéo"))?.push(Detection { bbox: BBox::new(v[2], v[3], v[4], v[5]), score: v[1], pose });
        }
        Ok(Found { width, height, frames })
    }
}

/// Violente ou non, d'après le chemin (en minuscules, séparateurs « / »).
pub(super) fn is_violent(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    let calm = parts.iter().any(|p| p.contains("non-violen") || p.contains("nonviolen") || p.contains("nofight") || p.contains("no_fight") || p.starts_with("nofi"));
    let elsewhere = ["falls/", "friendly/", "ambiguous/"].iter().any(|d| rel.starts_with(d));
    !calm && !elsewhere && parts.iter().any(|p| p.contains("violen") || p.contains("fight") || p.starts_with("fi"))
}

/// Groupe pour le bilan : jeu de données (et caméra pour AIRTLab).
pub(super) fn group(rel: &str) -> String {
    let parts: Vec<&str> = rel.split('/').collect();
    match parts.first() {
        Some(&"falls") => "chutes et vie courante (UR Fall, CAUCAFall)".into(),
        Some(&"friendly") => format!("{} {}", parts.get(1).copied().unwrap_or("?"), parts.get(2).copied().unwrap_or("?")),
        Some(&"ambiguous") => format!("ambigu : {} {}", parts.get(1).copied().unwrap_or("?"), parts.get(2).copied().unwrap_or("?")),
        Some(&"violence") => {
            let set = parts.get(1).copied().unwrap_or("?");
            match parts.iter().find(|p| p.starts_with("cam")) {
                Some(cam) => format!("{set} {cam}"),
                None => set.to_string(),
            }
        }
        _ => parts.first().copied().unwrap_or("?").to_string(),
    }
}

/// Chemin raccourci pour l'affichage : sans les dossiers des archives (« …-master », etc.).
pub(super) fn short(rel: &str) -> String {
    rel.split('/').filter(|p| !p.ends_with("-master") && *p != "violence-detection-dataset").collect::<Vec<_>>().join("/")
}

/// Actions jouées dans chaque scène AIRTLab (fichiers CSV du jeu) : (violent ?, fichier) → actions.
fn airtlab_actions(root: &Path) -> HashMap<(bool, String), Vec<String>> {
    let mut out = HashMap::new();
    let mut csvs = Vec::new();
    find_files(root, "action-classes.csv", &mut csvs);
    for p in csvs {
        let violent = !p.file_name().unwrap_or_default().to_string_lossy().starts_with("non");
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        for line in text.lines().skip(1) {
            if let Some((file, actions)) = line.split_once(';') {
                out.insert((violent, file.trim().to_string()), actions.split(',').map(|a| a.trim().to_string()).collect());
            }
        }
    }
    out
}

fn find_files(dir: &Path, suffix: &str, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for p in rd.filter_map(|e| e.ok()).map(|e| e.path()) {
        if p.is_dir() {
            find_files(&p, suffix, out);
        } else if p.to_string_lossy().ends_with(suffix) {
            out.push(p);
        }
    }
}

#[derive(Default)]
struct Tally {
    violent: u32,
    found: u32,
    calm: u32,
    false_alarms: u32,
}

pub fn run(a: ViolenceEval) -> Result<()> {
    let accel = if a.cpu { Accel::Cpu } else { Accel::Auto };
    let mut list = Vec::new();
    for sub in ["violence", "falls", "friendly", "ambiguous"] {
        videos(&a.data.join(sub), &mut list);
    }
    let rel_of = |p: &Path| p.strip_prefix(&a.data).unwrap_or(p).to_string_lossy().replace('\\', "/");
    if let Some(o) = &a.only {
        list.retain(|p| rel_of(p).contains(o.as_str()));
    }
    if let Some(n) = a.limit {
        list.truncate(n);
    }
    if list.is_empty() {
        bail!("aucune vidéo dans {}", a.data.join("violence").display());
    }
    let d = ViolenceRules::default();
    let rules = ViolenceRules {
        min_point: a.min_point.unwrap_or(d.min_point),
        wrist_speed: a.wrist_speed.unwrap_or(d.wrist_speed),
        ankle_speed: a.ankle_speed.unwrap_or(d.ankle_speed),
        reach: a.reach.unwrap_or(d.reach),
        toward: a.toward.unwrap_or(d.toward),
        wrist_low: a.wrist_low.unwrap_or(d.wrist_low),
        steady: a.steady.unwrap_or(d.steady),
        same_person: a.same_person.unwrap_or(d.same_person),
        base: a.base.unwrap_or(d.base),
        strikes: a.strikes.unwrap_or(d.strikes),
        window: a.window.unwrap_or(d.window),
        max_people: a.max_people.unwrap_or(d.max_people),
        edge: a.edge.unwrap_or(d.edge),
        ..d
    };
    let actions = airtlab_actions(&a.data.join("violence"));
    println!("Violence : {} vidéo(s), {} analyses/s · {rules:?}", list.len(), a.fps);
    if let Some(c) = &a.cache {
        std::fs::create_dir_all(c)?;
    }
    let mut rtmo: Option<RtmoPersons> = None;
    let mut csv = match &a.dump {
        Some(p) => {
            let mut f = std::io::BufWriter::new(std::fs::File::create(p)?);
            write!(f, "video,violent,t,w,h,track,score,x0,y0,x1,y1")?;
            for k in 0..17 {
                write!(f, ",k{k}x,k{k}y,k{k}s")?;
            }
            writeln!(f)?;
            Some(f)
        }
        None => None,
    };
    let mut strike_csv = match &a.strike_log {
        Some(p) => {
            let mut f = std::io::BufWriter::new(std::fs::File::create(p)?);
            writeln!(f, "video,group,violent,at,from,to,limb,speed,height,toward,recoil,before,head,shrink,gap,gap_after,lift,reach,score,cut_from,cut_to,people,global")?;
            let mut v = std::io::BufWriter::new(std::fs::File::create(p.with_extension("videos.csv"))?);
            writeln!(v, "video,group,violent,alert,shout,hit,laugh")?;
            Some((f, v))
        }
        None => None,
    };
    let mut tallies: BTreeMap<String, Tally> = BTreeMap::new();
    // Actions AIRTLab : (scènes, alertes).
    let mut by_action: BTreeMap<(bool, String), (u32, u32)> = BTreeMap::new();
    let mut delays: Vec<f64> = Vec::new();
    let mut tagger = if a.sounds {
        use homesmart::vision::models;
        Some(homesmart::listen::sounds::SoundTagger::load(&models::CED_MINI.path(), &models::CED_LABELS.path())?)
    } else {
        None
    };
    // Vidéos avec son : (violente, alerte de l'image, cris, chocs, rires, groupe).
    let mut heard: Vec<(bool, bool, f32, f32, f32, String)> = Vec::new();
    let t_all = Instant::now();
    let tail = a.fps.round() as usize;
    for (n, path) in list.iter().enumerate() {
        let rel = rel_of(path);
        let violent = is_violent(&rel.to_ascii_lowercase());
        let found = found(path, &rel, a.fps, false, &a.model, accel, a.cache.as_deref(), &mut rtmo)?;
        // Chaîne de l'application : même suivi, même seuil d'ouverture des pistes.
        let mut tracker = Tracker::default();
        tracker.high = Pipeline::NEW_PERSON;
        let mut detector = ViolenceDetector::new(rules.clone());
        if strike_csv.is_some() {
            detector.record = Some(Vec::new());
        }
        let mut hist: HashMap<u32, Vec<Seen>> = HashMap::new();
        let (fw, fh) = (found.width as f32, found.height as f32);
        let mut first: Option<f64> = None;
        let mut alerts = 0;
        // Les extraits s'arrêtent net : on prolonge d'une seconde avec la dernière image.
        let last = found.frames.last().cloned().unwrap_or_default();
        for (i, dets) in found.frames.iter().chain(std::iter::repeat_n(&last, tail)).enumerate() {
            let now = i as f64 / a.fps as f64;
            tracker.update(now, dets);
            for e in detector.update(now, &tracker, fw, fh) {
                if a.verbose && a.only.is_some() {
                    println!("    {now:5.2} s : pistes {} et {}, {} coups", e.tracks.0, e.tracks.1, e.strikes);
                }
                first.get_or_insert(now);
                alerts += 1;
            }
            if strike_csv.is_some() {
                for t in tracker.tracks().iter().filter(|t| t.confirmed && t.last_seen >= now) {
                    hist.entry(t.id).or_default().push((now, t.bbox, t.pose.clone()));
                }
            }
            if let Some(f) = csv.as_mut() {
                for t in tracker.tracks().iter().filter(|t| t.confirmed && t.last_seen >= now) {
                    write!(f, "{rel},{},{now:.3},{},{},{},{:.3},{:.4},{:.4},{:.4},{:.4}", violent as u8, found.width, found.height, t.id, t.score, t.bbox.x0, t.bbox.y0, t.bbox.x1, t.bbox.y1)?;
                    match &t.pose {
                        Some(p) => {
                            for k in &p.points {
                                write!(f, ",{:.4},{:.4},{:.3}", k.at.x, k.at.y, k.score)?;
                            }
                        }
                        None => write!(f, "{}", ",0,0,0".repeat(17))?,
                    }
                    writeln!(f)?;
                }
            }
        }
        let hit = first.is_some();
        // Son de la vidéo : plus forts cris, chocs et rires (−1 sans son ou sans --sounds).
        let mut peaks = (-1f32, -1f32, -1f32);
        if let Some(t) = tagger.as_mut() {
            let x = audio16k(path)?;
            if x.len() >= 8_000 {
                peaks = sound_peaks(t, &x)?;
                heard.push((violent, hit, peaks.0, peaks.1, peaks.2, group(&rel)));
            }
        }
        if let (Some((f, v)), Some(log)) = (strike_csv.as_mut(), detector.record.take()) {
            let (name, g) = (short(&rel).replace(',', ";"), group(&rel).replace(',', ";"));
            for s in &log {
                if let Some(row) = strike_row(s, &hist, fw, fh) {
                    writeln!(f, "{name},{g},{},{row}", violent as u8)?;
                }
            }
            writeln!(v, "{name},{g},{},{},{:.3},{:.3},{:.3}", violent as u8, hit as u8, peaks.0, peaks.1, peaks.2)?;
        }
        let t = tallies.entry(group(&rel)).or_default();
        if violent {
            t.violent += 1;
            t.found += hit as u32;
            if let Some(at) = first {
                delays.push(at);
            }
        } else {
            t.calm += 1;
            t.false_alarms += hit as u32;
        }
        // Actions jouées : fichiers CSV d'AIRTLab, ou fin du nom (« 0_1_4-punch.avi », UT-Interaction).
        let file = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let played: Vec<String> = match actions.get(&(violent, file)) {
            Some(acts) if rel.contains("airtlab") => acts.clone(),
            _ if rel.contains("utinteraction") => stem.rsplit_once('-').map(|(_, act)| vec![act.to_string()]).unwrap_or_default(),
            _ => Vec::new(),
        };
        for act in played {
            let e = by_action.entry((violent, act)).or_default();
            e.0 += 1;
            e.1 += hit as u32;
        }
        // Les violences manquées sont nombreuses (règle prudente) : détaillées seulement avec --verbose.
        if a.verbose || (hit && !violent) {
            let what = match (violent, hit) {
                (true, true) => "violence trouvée",
                (true, false) => "violence manquée",
                (false, true) => "FAUSSE ALERTE",
                (false, false) => "rien (juste)",
            };
            println!("  {} : {what}{}{}", short(&rel), first.map(|t| format!(" à {t:.1} s")).unwrap_or_default(), if alerts > 1 { format!(" (×{alerts})") } else { String::new() });
        }
        if (n + 1) % 50 == 0 {
            eprintln!("  … {} / {} ({:.0} s)", n + 1, list.len(), t_all.elapsed().as_secs_f32());
        }
    }
    println!();
    let pct = |a: u32, b: u32| a as f32 * 100.0 / b.max(1) as f32;
    let mut all = Tally::default();
    for (g, t) in &tallies {
        let mut line = format!("  {g:<45}");
        if t.violent > 0 {
            line.push_str(&format!(" trouvées {}/{} ({:.0} %)", t.found, t.violent, pct(t.found, t.violent)));
        }
        if t.calm > 0 {
            line.push_str(&format!(" fausses alertes {}/{} ({:.1} %)", t.false_alarms, t.calm, pct(t.false_alarms, t.calm)));
        }
        println!("{line}");
        // Les scènes ambiguës (vraies gifles ou jeux ?) ne comptent dans aucun total.
        if g.starts_with("ambigu") {
            continue;
        }
        (all.violent, all.found, all.calm, all.false_alarms) = (all.violent + t.violent, all.found + t.found, all.calm + t.calm, all.false_alarms + t.false_alarms);
    }
    println!(
        "Violences trouvées : {}/{} ({:.0} %) · fausses alertes : {}/{} vidéos sans violence ({:.1} %) · {:.0} s",
        all.found,
        all.violent,
        pct(all.found, all.violent),
        all.false_alarms,
        all.calm,
        pct(all.false_alarms, all.calm),
        t_all.elapsed().as_secs_f32()
    );
    if !delays.is_empty() {
        delays.sort_by(f64::total_cmp);
        println!("  instant de l'alerte dans la vidéo : médian {:.1} s", delays[delays.len() / 2]);
    }
    if !heard.is_empty() {
        let (nv, nc) = (heard.iter().filter(|h| h.0).count(), heard.iter().filter(|h| !h.0).count());
        println!("  son ({nv} vidéo(s) violente(s) et {nc} calme(s) avec son) : part au-dessus du seuil, violentes / calmes");
        type Heard = (bool, bool, f32, f32, f32, String);
        for th in [0.1f32, 0.2, 0.3, 0.5] {
            let share = |v: bool, f: &dyn Fn(&Heard) -> bool| heard.iter().filter(|h| h.0 == v && f(h)).count() as f32 * 100.0 / heard.iter().filter(|h| h.0 == v).count().max(1) as f32;
            let shout = |h: &Heard| h.2 >= th;
            let hit = |h: &Heard| h.3 >= th;
            let laugh = |h: &Heard| h.4 >= th;
            println!(
                "    ≥ {th:.1} : cris {:.0} % / {:.0} % · chocs {:.0} % / {:.0} % · rires {:.0} % / {:.0} %",
                share(true, &shout),
                share(false, &shout),
                share(true, &hit),
                share(false, &hit),
                share(true, &laugh),
                share(false, &laugh)
            );
        }
        // Application avec l'écoute des sons : une alerte avec des rires entendus (et pas de cri)
        // est retenue.
        let laughed = |h: &Heard| h.4 >= homesmart::listen::sounds::LAUGHTER_MIN && h.2 < 0.3;
        let (mut found, mut violent, mut false_alarms, mut calm) = (0, 0, 0, 0);
        for h in heard.iter().filter(|h| !h.5.starts_with("ambigu")) {
            if h.0 {
                violent += 1;
                found += (h.1 && !laughed(h)) as u32;
            } else {
                calm += 1;
                false_alarms += (h.1 && !laughed(h)) as u32;
            }
        }
        let before = (heard.iter().filter(|h| h.0 && h.1 && !h.5.starts_with("ambigu")).count(), heard.iter().filter(|h| !h.0 && h.1 && !h.5.starts_with("ambigu")).count());
        println!("  avec l'écoute (rires ≥ {:.2} sans cri : alerte retenue), vidéos avec son : trouvées {found}/{violent} (au lieu de {}) · fausses alertes {false_alarms}/{calm} (au lieu de {})", homesmart::listen::sounds::LAUGHTER_MIN, before.0, before.1);
        // Rires par groupe : vidéos avec son, part avec des rires (≥ 0,1 / ≥ 0,3).
        let mut groups: BTreeMap<&str, (u32, u32, u32)> = BTreeMap::new();
        for h in &heard {
            let e = groups.entry(h.5.as_str()).or_default();
            e.0 += 1;
            e.1 += (h.4 >= 0.1) as u32;
            e.2 += (h.4 >= 0.3) as u32;
        }
        for (g, (n, l1, l3)) in groups {
            println!("    rires, {g} : {l1}/{n} ≥ 0,1 · {l3}/{n} ≥ 0,3");
        }
    }
    if !by_action.is_empty() {
        let list: Vec<String> = by_action.iter().map(|((v, act), (n, hit))| format!("{act}{} {hit}/{n}", if *v { "" } else { " (calme)" })).collect();
        println!("  par action (vidéos avec alerte / vidéos) : {}", list.join(" · "));
    }
    Ok(())
}
