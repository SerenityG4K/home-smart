//! `homesmart-cli vision eval …` : qualité des détections sur des images et vidéos réelles
//! annotées (téléchargées à part, jamais livrées avec HomeSmart), pour régler les seuils.
//!
//! - `coco` : personnes (et squelettes, gestes), animaux, véhicules sur COCO val2017, y compris
//!   tels que l'application les retient (deuxième avis, seuil de chaque sorte) ;
//! - `faces` : vérification de visages sur les paires LFW (même personne ou non) ;
//! - `identify` : foyers simulés sur LFW, avec le vote de l'application sur plusieurs visages ;
//! - `falls` : chutes sur des vidéos (UR Fall, CAUCAFall), avec la vraie chaîne d'analyse ;
//! - `alarms` : toute la chaîne d'alertes (suivi, chutes, gestes, violence) sur toutes les vidéos
//!   du jeu d'essai, personnes trouvées gardées en cache (le même que `violence-eval`) ;
//! - `images` : ce que l'analyse trouve dans un dossier d'images libres.
//!
//! Options de dégradation : `--night` (vision nocturne : gris, peu contrasté, bruité) et
//! `--scale` (image réduite : personnes ou visages plus lointains).

use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use homesmart::camera::Frame;
use homesmart::vision::actions::hands_up;
use homesmart::vision::eval::{oks, Pairs, Scores, Truth};
use homesmart::vision::geom::BBox;
use homesmart::vision::objects::ObjectKind;
use homesmart::vision::onnx::{RtmoPersons, SfaceEmbedder, YoloxObjects, YunetFaces};
use homesmart::vision::pipeline::{FaceDetector, FaceEmbedder, PersonDetector, Pipeline};
use homesmart::vision::track::Detection;
use homesmart::vision::{engine::Accel, picture, Keypoint, Pose};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Subcommand)]
pub enum EvalCmd {
    /// Personnes, squelettes, animaux, véhicules sur COCO val2017 (dossier `coco` du jeu d'essai)
    Coco {
        /// Dossier du jeu d'évaluation (contient coco/)
        #[arg(long)]
        data: PathBuf,
        #[arg(long)]
        cpu: bool,
        /// Nombre maximal d'images
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        night: bool,
        /// Facteur de réduction des images (0,5 : personnes deux fois plus petites)
        #[arg(long, default_value_t = 1.0)]
        scale: f32,
        /// Sans animaux ni véhicules (plus rapide)
        #[arg(long)]
        persons_only: bool,
        /// Modèle des personnes : s (standard) ou m (précision élevée)
        #[arg(long, default_value = "s")]
        model: String,
        /// Écrit chaque détection (chiffres seulement) dans ce fichier CSV, pour l'analyse des seuils
        #[arg(long)]
        dump: Option<PathBuf>,
    },
    /// Visages : paires LFW (pairs.txt)
    Faces {
        #[arg(long)]
        data: PathBuf,
        #[arg(long)]
        cpu: bool,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        night: bool,
        /// Largeur visée des visages, en pixels (LFW : ≈ 110) — simule l'éloignement
        #[arg(long)]
        px: Option<f32>,
        /// Seuil du détecteur de visages (réglage de l'application : 0,7)
        #[arg(long)]
        det: Option<f32>,
    },
    /// Visages : foyers simulés sur LFW (habitants enregistrés avec quelques photos, inconnus de
    /// passage), vote de l'application sur plusieurs visages par passage : habitants nommés,
    /// pris pour un inconnu ou pour un autre, inconnus nommés à tort
    Identify {
        #[arg(long)]
        data: PathBuf,
        #[arg(long)]
        cpu: bool,
        /// Foyers simulés
        #[arg(long, default_value_t = 400)]
        households: usize,
        /// Habitants par foyer (autant d'inconnus de passage)
        #[arg(long, default_value_t = 4)]
        residents: usize,
        /// Photos enregistrées par habitant
        #[arg(long, default_value_t = 3)]
        enrolled: usize,
        #[arg(long)]
        night: bool,
        /// Largeur visée des visages, en pixels (LFW : ≈ 110) — simule l'éloignement
        #[arg(long)]
        px: Option<f32>,
        /// Parts minimales des visages reconnus à essayer pour nommer quelqu'un (réglage de
        /// l'application par défaut ; ex. « 0,0.34,0.5 »)
        #[arg(long, value_delimiter = ',')]
        share: Vec<f32>,
    },
    /// Chutes : vidéos dont le chemin contient « fall » (chutes) ou non (vie courante)
    Falls {
        #[arg(long)]
        data: PathBuf,
        #[arg(long)]
        cpu: bool,
        /// Analyses par seconde (comme le réglage de l'application)
        #[arg(long, default_value_t = 8.0)]
        fps: f32,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        night: bool,
        /// Détail vidéo par vidéo
        #[arg(long)]
        verbose: bool,
        /// Écrit les personnes suivies, image par image (boîtes et squelettes, chiffres seulement)
        #[arg(long)]
        dump: Option<PathBuf>,
        /// Seulement les vidéos dont le chemin contient ce texte
        #[arg(long)]
        only: Option<String>,
        /// Temps au sol avant de confirmer une chute (s ; réglage de l'application par défaut)
        #[arg(long)]
        hold: Option<f64>,
        /// Modèle des personnes : s (standard) ou m (précision élevée)
        #[arg(long, default_value = "s")]
        model: String,
    },
    /// Toute la chaîne d'alertes de l'application (suivi, chutes, gestes, violence) sur toutes les
    /// vidéos du jeu d'essai (falls/, violence/, friendly/, ambiguous/) : trouvées, manquées,
    /// fausses alertes par heure de vidéo
    Alarms {
        #[arg(long)]
        data: PathBuf,
        #[arg(long)]
        cpu: bool,
        #[arg(long, default_value_t = 8.0)]
        fps: f32,
        /// Modèle des personnes : s (standard) ou m (précision élevée)
        #[arg(long, default_value = "s")]
        model: String,
        #[arg(long)]
        night: bool,
        /// Dossier où garder les personnes trouvées (le même que `violence-eval --cache`)
        #[arg(long)]
        cache: Option<PathBuf>,
        /// Seulement les vidéos dont le chemin contient ce texte
        #[arg(long)]
        only: Option<String>,
        /// Événements de chaque vidéo
        #[arg(long)]
        verbose: bool,
        /// Écrit les personnes suivies, image par image (boîtes et squelettes, chiffres seulement)
        #[arg(long)]
        dump: Option<PathBuf>,
    },
    /// Dossier d'images : personnes, squelettes, visages, animaux trouvés (chiffres seulement)
    Images {
        dir: PathBuf,
        #[arg(long)]
        cpu: bool,
    },
}

pub fn run(cmd: EvalCmd) -> Result<()> {
    match cmd {
        EvalCmd::Coco { data, cpu, limit, night, scale, persons_only, dump, model } => coco(&data, accel(cpu), limit, night, scale, persons_only, dump.as_deref(), &model),
        EvalCmd::Faces { data, cpu, limit, night, px, det } => faces(&data, accel(cpu), limit, night, px, det),
        EvalCmd::Identify { data, cpu, households, residents, enrolled, night, px, share } => identify(&data, accel(cpu), households, residents, enrolled, night, px, &share),
        EvalCmd::Falls { data, cpu, fps, limit, night, verbose, dump, only, hold, model } => falls(&data, accel(cpu), fps, limit, night, verbose, dump.as_deref(), only.as_deref(), hold, &model),
        EvalCmd::Alarms { data, cpu, fps, model, night, cache, only, verbose, dump } => alarms(&data, accel(cpu), fps, &model, night, cache.as_deref(), only.as_deref(), verbose, dump.as_deref()),
        EvalCmd::Images { dir, cpu } => images(&dir, accel(cpu)),
    }
}

fn person_model(name: &str) -> Result<&'static homesmart::vision::models::ModelSpec> {
    match name {
        "s" => Ok(&homesmart::vision::models::RTMO_S),
        "m" => Ok(&homesmart::vision::models::RTMO_M),
        other => bail!("modèle « {other} » inconnu (s ou m)"),
    }
}

fn accel(cpu: bool) -> Accel {
    if cpu {
        Accel::Cpu
    } else {
        Accel::Auto
    }
}

/// Vision nocturne simulée (infrarouge) : niveaux de gris, contraste écrasé, bruit de capteur.
fn night_vision(f: &mut Frame, seed: u64) {
    let mut s = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407) | 1;
    for p in f.rgba.chunks_mut(4) {
        let g = 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32;
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        // Bruit à peu près gaussien (somme de deux uniformes), écart-type ≈ 7 niveaux.
        let n = (((s & 0xFFFF) as f32 + ((s >> 16) & 0xFFFF) as f32) / 65535.0 - 1.0) * 12.0;
        let v = (40.0 + g * 0.55 + n).clamp(0.0, 255.0) as u8;
        p[0] = v;
        p[1] = v;
        p[2] = v;
    }
}

/// Image en vision nocturne simulée (même bruit pour la même graine).
pub(super) fn night(mut f: Frame, seed: u64) -> Frame {
    night_vision(&mut f, seed);
    f
}

fn prepare(mut f: Frame, night: bool, scale: f32, seed: u64) -> Frame {
    if (scale - 1.0).abs() > 1e-3 {
        let (w, h) = (((f.width as f32 * scale).round() as usize).max(8), ((f.height as f32 * scale).round() as usize).max(8));
        f = picture::resize(&f, w, h);
    }
    if night {
        night_vision(&mut f, seed);
    }
    f
}

// ---- COCO -----------------------------------------------------------------------------------

#[derive(Deserialize)]
struct CocoFile {
    images: Vec<CocoImage>,
    annotations: Vec<CocoAnn>,
    categories: Vec<CocoCat>,
}

#[derive(Deserialize)]
struct CocoImage {
    id: u64,
    file_name: String,
    width: f32,
    height: f32,
}

#[derive(Deserialize)]
struct CocoAnn {
    id: u64,
    image_id: u64,
    category_id: u32,
    bbox: [f32; 4],
    iscrowd: u8,
    area: f32,
    #[serde(default)]
    keypoints: Vec<f32>,
}

#[derive(Deserialize)]
struct CocoCat {
    id: u32,
    name: String,
}

/// Seuils candidats pour la recherche du meilleur réglage.
fn grid() -> Vec<f32> {
    (1..=19).map(|k| k as f32 * 0.05).collect()
}

#[allow(clippy::too_many_arguments)]
fn coco(data: &Path, accel: Accel, limit: Option<usize>, night: bool, scale: f32, persons_only: bool, dump: Option<&Path>, model: &str) -> Result<()> {
    let dir = data.join("coco");
    let read = |name: &str| -> Result<CocoFile> {
        let p = dir.join(name);
        let f = std::fs::File::open(&p).with_context(|| format!("{} (lance d'abord le téléchargement du jeu d'essai)", p.display()))?;
        Ok(serde_json::from_reader(std::io::BufReader::new(f))?)
    };
    let inst = read("instances_val2017.json")?;
    let kp = read("person_keypoints_val2017.json")?;
    let keypoints: HashMap<u64, Vec<f32>> = kp.annotations.into_iter().filter(|a| !a.keypoints.is_empty()).map(|a| (a.id, a.keypoints)).collect();
    // Indice YOLOX (0…79) = rang de la catégorie COCO triée par identifiant.
    let mut cats: Vec<&CocoCat> = inst.categories.iter().collect();
    cats.sort_by_key(|c| c.id);
    let index: HashMap<u32, usize> = cats.iter().enumerate().map(|(i, c)| (c.id, i)).collect();
    let names: Vec<&str> = cats.iter().map(|c| c.name.as_str()).collect();
    let mut by_image: HashMap<u64, Vec<&CocoAnn>> = HashMap::new();
    for a in &inst.annotations {
        by_image.entry(a.image_id).or_default().push(a);
    }
    let mut images: Vec<&CocoImage> = inst.images.iter().filter(|im| dir.join("val2017").join(&im.file_name).exists()).collect();
    images.sort_by_key(|im| im.id);
    if let Some(n) = limit {
        images.truncate(n);
    }
    if images.is_empty() {
        bail!("aucune image COCO dans {}", dir.join("val2017").display());
    }

    let mut rtmo = RtmoPersons::with_model(person_model(model)?, accel)?;
    rtmo.threshold = 0.05;
    let mut yolox = if persons_only { None } else { Some(YoloxObjects::new(accel)?) };
    if let Some(y) = yolox.as_mut() {
        y.threshold = 0.05;
    }
    println!(
        "COCO val2017 : {} image(s){}{} — personnes : {}{}",
        images.len(),
        if night { ", vision nocturne simulée" } else { "" },
        if (scale - 1.0).abs() > 1e-3 { format!(", réduites ×{scale}") } else { String::new() },
        rtmo.backend,
        yolox.as_ref().map(|y| format!(", objets : {}", y.backend)).unwrap_or_default()
    );
    // Personnes : toutes tailles, et « moyennes et grandes » (≥ 32×32 px dans l'image d'origine) ;
    // et comme dans l'application, avec le deuxième avis de YOLOX (`PersonCheck`).
    let (mut all, mut big, mut checked) = (Scores::default(), Scores::default(), Scores::default());
    let check = homesmart::vision::pipeline::PersonCheck::default();
    // Gestes, image par image (personnes bien trouvées) : (vrais, vus, vus à tort) pour les deux
    // bras levés, puis pour une main levée.
    let mut gestures = [(0u32, 0u32, 0u32); 2];
    // Animaux et véhicules tels que l'application les retient (seuil de chaque sorte, doublons).
    let mut kept: HashMap<ObjectKind, Scores> = HashMap::new();
    let mut oks_v: Vec<f32> = Vec::new();
    // Personnes vues par YOLOX (comparaison) et classes d'objets suivies.
    let tracked: Vec<usize> = [0usize, 1, 2, 3, 5, 7, 14, 15, 16, 17, 18, 19].to_vec();
    let mut objs: HashMap<usize, Scores> = tracked.iter().map(|&c| (c, Scores::default())).collect();
    let (mut t_p, mut t_o) = (0f64, 0f64);
    let t_all = Instant::now();
    let mut csv = match dump {
        Some(p) => {
            let mut f = std::io::BufWriter::new(std::fs::File::create(p)?);
            use std::io::Write;
            writeln!(f, "image,model,class,score,w,h,truth_iou,truth_class,truth_small,kp_mean,kp_top8,kp_conf,yolox_person,torso_px")?;
            Some(f)
        }
        None => None,
    };
    for (n, im) in images.iter().enumerate() {
        let frame = picture::load(&dir.join("val2017").join(&im.file_name))?;
        let frame = prepare(frame, night, scale, im.id);
        let anns = by_image.get(&im.id).map(|v| v.as_slice()).unwrap_or(&[]);
        let norm = |b: &[f32; 4]| BBox::new(b[0] / im.width, b[1] / im.height, (b[0] + b[2]) / im.width, (b[1] + b[3]) / im.height);

        let t0 = Instant::now();
        let dets = rtmo.detect(&frame)?;
        t_p += t0.elapsed().as_secs_f64();
        let persons: Vec<&&CocoAnn> = anns.iter().filter(|a| a.category_id == 1).collect();
        let truths_all: Vec<Truth> = persons.iter().map(|a| Truth { bbox: norm(&a.bbox), ignore: a.iscrowd != 0 }).collect();
        let truths_big: Vec<Truth> = persons.iter().map(|a| Truth { bbox: norm(&a.bbox), ignore: a.iscrowd != 0 || a.area < 32.0 * 32.0 }).collect();
        let boxes: Vec<(BBox, f32)> = dets.iter().map(|d| (d.bbox, d.score)).collect();
        all.add_image(&boxes, &truths_all, 0.5);
        let matched = big.add_image(&boxes, &truths_big, 0.5);
        // Squelettes des personnes bien trouvées (score ≥ 0,5) : OKS face à l'annotation.
        for (d, m) in dets.iter().zip(&matched) {
            let (Some(j), Some(pose)) = (m, &d.pose) else { continue };
            if d.score < 0.5 {
                continue;
            }
            let Some(k) = keypoints.get(&persons[*j].id) else { continue };
            let truth: Vec<(f32, f32)> = (0..17).map(|i| (k[i * 3], k[i * 3 + 1])).collect();
            let visible: Vec<bool> = (0..17).map(|i| k[i * 3 + 2] > 0.0).collect();
            let pred: Vec<(f32, f32)> = pose.points.iter().map(|p| (p.at.x * im.width, p.at.y * im.height)).collect();
            if let Some(v) = oks(&pred, &truth, &visible, persons[*j].area) {
                oks_v.push(v);
            }
            // Mains levées d'après le squelette annoté, et d'après celui du modèle.
            let mut points = [Keypoint::default(); 17];
            for (i, p) in points.iter_mut().enumerate() {
                *p = Keypoint { at: homesmart::vision::geom::Point::new(k[i * 3] / im.width, k[i * 3 + 1] / im.height), score: if visible[i] { 1.0 } else { 0.0 } };
            }
            let (tl, tr) = hands_up(&Pose { points }, &norm(&persons[*j].bbox));
            let (pl, pr) = hands_up(pose, &d.bbox);
            for (g, truth, seen) in [(0, tl && tr, pl && pr), (1, tl != tr, pl != pr)] {
                let c = &mut gestures[g];
                (c.0, c.1, c.2) = (c.0 + truth as u32, c.1 + seen as u32, c.2 + (seen && !truth) as u32);
            }
        }
        let found = match yolox.as_mut() {
            Some(y) => {
                let t1 = Instant::now();
                let f = y.detect_raw(&frame, true)?;
                t_o += t1.elapsed().as_secs_f64();
                f
            }
            None => Vec::new(),
        };
        if let Some(f) = csv.as_mut() {
            use std::io::Write;
            // Meilleure vraie boîte (toutes classes) recouvrant une détection : IoU, classe, petite ?
            let best_truth = |b: &BBox| {
                anns.iter().filter(|a| a.iscrowd == 0).map(|a| (norm(&a.bbox).iou(b), index.get(&a.category_id).copied().unwrap_or(99), a.area < 32.0 * 32.0)).max_by(|x, y| x.0.total_cmp(&y.0)).unwrap_or((0.0, 99, false))
            };
            for d in &dets {
                let (iou, class, small) = best_truth(&d.bbox);
                let (mean, top8, conf) = d.pose.as_ref().map_or((0.0, 0.0, 0), |p| {
                    let mut s: Vec<f32> = p.points.iter().map(|k| k.score).collect();
                    s.sort_by(|a, b| b.total_cmp(a));
                    (s.iter().sum::<f32>() / 17.0, s[..8].iter().sum::<f32>() / 8.0, s.iter().filter(|&&v| v >= 0.3).count())
                });
                let yp = found.iter().filter(|o| o.0 == 0).map(|o| if o.1.iou(&d.bbox) >= 0.5 { o.2 } else { 0.0 }).fold(0.0, f32::max);
                let torso = d.pose.as_ref().and_then(|p| p.torso_angle(frame.width as f32, frame.height as f32)).unwrap_or(-1.0);
                writeln!(f, "{},rtmo,0,{:.4},{:.4},{:.4},{:.3},{class},{},{mean:.3},{top8:.3},{conf},{yp:.3},{torso:.1}", im.id, d.score, d.bbox.width(), d.bbox.height(), iou, small as u8)?;
            }
            for (c, b, s) in &found {
                let (iou, class, small) = best_truth(b);
                writeln!(f, "{},yolox,{c},{s:.4},{:.4},{:.4},{iou:.3},{class},{},0,0,0,0,-1", im.id, b.width(), b.height(), small as u8)?;
            }
        }
        if yolox.is_some() {
            for (&class, scores) in objs.iter_mut() {
                let truths: Vec<Truth> = anns.iter().filter(|a| index.get(&a.category_id) == Some(&class)).map(|a| Truth { bbox: norm(&a.bbox), ignore: a.iscrowd != 0 }).collect();
                let dets: Vec<(BBox, f32)> = found.iter().filter(|f| f.0 == class).map(|f| (f.1, f.2)).collect();
                scores.add_image(&dets, &truths, 0.5);
            }
            // Deuxième avis : une silhouette au score modeste n'est gardée que si YOLOX voit
            // aussi quelqu'un au même endroit.
            let seconds: Vec<&(usize, BBox, f32)> = found.iter().filter(|f| f.0 == 0 && f.2 >= check.second).collect();
            let gated: Vec<(BBox, f32)> = boxes.iter().filter(|(b, s)| *s >= check.strong || seconds.iter().any(|f| f.1.iou(b) >= check.iou)).copied().collect();
            checked.add_image(&gated, &truths_big, 0.5);
            let app = homesmart::vision::objects::from_raw(&found);
            for kind in KINDS {
                let truths: Vec<Truth> = anns
                    .iter()
                    .filter(|a| index.get(&a.category_id).and_then(|&c| ObjectKind::from_coco(c)) == Some(kind))
                    .map(|a| Truth { bbox: norm(&a.bbox), ignore: a.iscrowd != 0 })
                    .collect();
                let dets: Vec<(BBox, f32)> = app.iter().filter(|o| o.kind == kind).map(|o| (o.bbox, o.score)).collect();
                kept.entry(kind).or_default().add_image(&dets, &truths, 0.5);
            }
        }
        if (n + 1) % 250 == 0 {
            eprintln!("  … {} / {}", n + 1, images.len());
        }
    }
    let ms = |t: f64| t * 1000.0 / images.len() as f64;
    println!("Temps : personnes {:.1} ms/image, objets {:.1} ms/image (total {:.0} s)", ms(t_p), ms(t_o), t_all.elapsed().as_secs_f32());
    println!();
    println!("PERSONNES ({}, appariement IoU ≥ 0,5)", person_model(model)?.id);
    println!("  AP50 toutes tailles {:.3} · moyennes et grandes {:.3} ({} / {} personnes à trouver)", all.ap(), big.ap(), all.positives, big.positives);
    for t in [0.3, 0.4, 0.5, 0.6, 0.7] {
        let (p, r) = big.at(t);
        let (fa, ne) = big.false_alarm_images(t);
        println!("  seuil {t:.2} : précision {:.3} · rappel {:.3} · fausses alertes {fa}/{ne} image(s) sans personne", p, r);
    }
    let (t, p, r, f1) = big.best_f1(&grid());
    println!("  meilleur F1 : seuil {t:.2} (précision {p:.3}, rappel {r:.3}, F1 {f1:.3}) — réglage actuel 0,50");
    if yolox.is_some() {
        let (p, r) = checked.at(Pipeline::NEW_PERSON);
        let (fa, ne) = checked.false_alarm_images(Pipeline::NEW_PERSON);
        println!("  comme l'application (seuil 0,50 et deuxième avis de YOLOX) : précision {p:.3} · rappel {r:.3} · fausses alertes {fa}/{ne} image(s) sans personne");
    }
    if !oks_v.is_empty() {
        oks_v.sort_by(f32::total_cmp);
        let mean = oks_v.iter().sum::<f32>() / oks_v.len() as f32;
        let good = oks_v.iter().filter(|&&v| v >= 0.75).count() as f32 / oks_v.len() as f32;
        println!("  squelettes : OKS moyen {mean:.3}, médian {:.3}, {:.0} % ≥ 0,75 ({} personnes)", oks_v[oks_v.len() / 2], good * 100.0, oks_v.len());
        for (g, what) in [(0, "deux bras levés"), (1, "une main levée")] {
            let (truth, seen, wrong) = gestures[g];
            let pct = |a: u32, b: u32| a as f32 * 100.0 / b.max(1) as f32;
            println!("  {what} (squelette annoté) : {truth} cas · vus {}/{truth} ({:.0} %) · vus à tort {wrong} ({:.0} % des vus)", seen - wrong, pct(seen - wrong, truth), pct(wrong, seen));
        }
    }
    if yolox.is_some() {
        println!();
        println!("OBJETS (YOLOX-s) : AP50 · précision / rappel à 0,45 · meilleur seuil");
        for c in &tracked {
            let s = &objs[c];
            if s.positives == 0 {
                continue;
            }
            let (p, r) = s.at(0.45);
            let (t, bp, br, _) = s.best_f1(&grid());
            let (fa, ne) = s.false_alarm_images(0.45);
            println!(
                "  {:<11} AP50 {:.3} · {p:.2} / {r:.2} · meilleur {t:.2} ({bp:.2} / {br:.2}) · {} à trouver · fausses alertes {fa}/{ne}",
                names[*c],
                s.ap(),
                s.positives
            );
        }
        println!("Comme l'application (seuil de chaque sorte, un animal vu sous deux sortes compte une fois) :");
        for kind in KINDS {
            let s = &kept[&kind];
            if s.positives == 0 {
                continue;
            }
            // Déjà triés au seuil de la sorte : toute détection gardée compte.
            let (p, r) = s.at(f32::MIN_POSITIVE);
            let (fa, ne) = s.false_alarm_images(f32::MIN_POSITIVE);
            println!("  {:<15} seuil {:.2} · précision {p:.2} · rappel {r:.2} · {} à trouver · images sans elle prises pour elle {fa}/{ne}", kind.label(), kind.threshold(), s.positives);
        }
    }
    Ok(())
}

/// Sortes suivies par l'application.
const KINDS: [ObjectKind; 12] = [
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
];

// ---- Visages (LFW) ----------------------------------------------------------------------------

fn faces(data: &Path, accel: Accel, limit: Option<usize>, night: bool, px: Option<f32>, det_threshold: Option<f32>) -> Result<()> {
    let dir = data.join("lfw");
    let pairs_txt = std::fs::read_to_string(dir.join("pairs.txt")).context("pairs.txt (jeu LFW)")?;
    let mut pairs: Vec<(String, String, bool)> = Vec::new();
    let img = |name: &str, n: &str| -> Result<String> { Ok(format!("{name}/{name}_{:04}.jpg", n.parse::<u32>()?)) };
    for line in pairs_txt.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.len() {
            3 => pairs.push((img(f[0], f[1])?, img(f[0], f[2])?, true)),
            4 => pairs.push((img(f[0], f[1])?, img(f[2], f[3])?, false)),
            _ => {}
        }
    }
    if let Some(n) = limit {
        // Autant de paires « même » que « autre » : on prend dans chaque pli.
        let (same, diff): (Vec<_>, Vec<_>) = pairs.into_iter().partition(|p| p.2);
        pairs = same.into_iter().take(n / 2).chain(diff.into_iter().take(n / 2)).collect();
    }
    let mut det = YunetFaces::new(accel)?;
    if let Some(t) = det_threshold {
        det.threshold = t;
    }
    let mut emb = SfaceEmbedder::new(accel)?;
    println!("LFW : {} paires{}{} — {} / {}", pairs.len(), if night { ", vision nocturne simulée" } else { "" }, px.map(|p| format!(", visages réduits à ≈ {p:.0} px")).unwrap_or_default(), det.backend, emb.backend);
    let mut cache: HashMap<String, Option<Vec<f32>>> = HashMap::new();
    let mut sizes: Vec<f32> = Vec::new();
    let mut embed = |rel: &str| -> Result<Option<Vec<f32>>> {
        if let Some(v) = cache.get(rel) {
            return Ok(v.clone());
        }
        let mut frame = picture::load(&dir.join("lfw").join(rel))?;
        if let Some(px) = px {
            // Les visages de LFW font ≈ 110 px de large dans une image de 250.
            let k = (px / 110.0).min(1.0);
            frame = prepare(frame, false, k, 0);
        }
        if night {
            night_vision(&mut frame, rel.len() as u64);
        }
        let found = det.detect(&frame, BBox::new(0.0, 0.0, 1.0, 1.0))?;
        // LFW : la personne de la paire est le visage du centre.
        let c = homesmart::vision::geom::Point::new(0.5, 0.5);
        let best = found.into_iter().min_by(|a, b| a.bbox.center().dist(c).total_cmp(&b.bbox.center().dist(c)));
        let v = match best {
            Some(f) => {
                sizes.push(f.bbox.width() * frame.width as f32);
                Some(emb.embed(&frame, &f)?)
            }
            None => None,
        };
        cache.insert(rel.to_string(), v.clone());
        Ok(v)
    };
    let mut res = Pairs::default();
    let t0 = Instant::now();
    for (a, b, same) in &pairs {
        match (embed(a)?, embed(b)?) {
            (Some(x), Some(y)) => {
                let s = homesmart::vision::faces::cosine(&x, &y);
                if *same {
                    res.same.push(s)
                } else {
                    res.different.push(s)
                }
            }
            _ => res.missed += 1,
        }
    }
    sizes.sort_by(f32::total_cmp);
    println!("  {:.1} s · visages trouvés : largeur médiane {:.0} px · paires sans visage : {}", t0.elapsed().as_secs_f32(), sizes.get(sizes.len() / 2).copied().unwrap_or(0.0), res.missed);
    let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len().max(1) as f32;
    println!("  ressemblance moyenne : même personne {:.3} · personnes différentes {:.3}", mean(&res.same), mean(&res.different));
    for t in [0.30, 0.33, 0.36, 0.40, 0.45] {
        let (far, frr) = res.rates(t);
        println!("  seuil {t:.2} : acceptés à tort {:.2} % · refusés à tort {:.1} % · exactitude {:.1} %", far * 100.0, frr * 100.0, res.accuracy(t) * 100.0);
    }
    let (bt, ba) = res.best_accuracy();
    println!("  meilleure exactitude {:.1} % au seuil {bt:.2}", ba * 100.0);
    for far in [0.01, 0.001] {
        let t = res.threshold_for_far(far);
        println!("  acceptés à tort ≤ {:.1} % : seuil {t:.2} (refusés à tort {:.1} %)", far * 100.0, res.rates(t).1 * 100.0);
    }
    Ok(())
}

/// Tirage pseudo-aléatoire reproductible (xorshift).
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n.max(1) as u64) as usize
    }
}

/// Ce qu'un passage a donné : nommé juste, pris pour un autre, pour un inconnu (à un moment du
/// passage, chaque changement d'avis étant une annonce), et le verdict à la fin.
#[derive(Default)]
struct Visits {
    n: u32,
    right: u32,
    wrong: u32,
    stranger: u32,
    final_right: u32,
    final_stranger: u32,
}

#[allow(clippy::too_many_arguments)]
fn identify(data: &Path, accel: Accel, households: usize, residents: usize, enrolled: usize, night: bool, px: Option<f32>, shares: &[f32]) -> Result<()> {
    use homesmart::vision::faces::{FaceGallery, IdentityVotes, Verdict};
    use homesmart::vision::pipeline::FaceRules;
    let dir = data.join("lfw").join("lfw");
    let mut people: Vec<(String, Vec<PathBuf>)> = Vec::new();
    for e in std::fs::read_dir(&dir).with_context(|| format!("{} (jeu LFW)", dir.display()))?.filter_map(|e| e.ok()) {
        let mut imgs: Vec<PathBuf> = std::fs::read_dir(e.path())?.filter_map(|f| f.ok()).map(|f| f.path()).collect();
        if imgs.len() >= 4 {
            imgs.sort();
            people.push((e.file_name().to_string_lossy().to_string(), imgs));
        }
    }
    people.sort();
    let mut det = YunetFaces::new(accel)?;
    let mut emb = SfaceEmbedder::new(accel)?;
    let t0 = Instant::now();
    // Empreintes de chaque personne (images sans visage écartées).
    let mut embs: Vec<(String, Vec<Vec<f32>>)> = Vec::new();
    for (name, imgs) in &people {
        let mut v = Vec::new();
        for (i, p) in imgs.iter().enumerate() {
            let mut frame = picture::load(p)?;
            if let Some(px) = px {
                frame = prepare(frame, false, (px / 110.0).min(1.0), 0);
            }
            if night {
                night_vision(&mut frame, i as u64 + name.len() as u64);
            }
            let c = homesmart::vision::geom::Point::new(0.5, 0.5);
            if let Some(f) = det.detect(&frame, BBox::new(0.0, 0.0, 1.0, 1.0))?.into_iter().min_by(|a, b| a.bbox.center().dist(c).total_cmp(&b.bbox.center().dist(c))) {
                v.push(emb.embed(&frame, &f)?);
            }
        }
        if v.len() >= 4 {
            embs.push((name.clone(), v));
        }
    }
    let rules = FaceRules::default();
    let enrollable: Vec<usize> = (0..embs.len()).filter(|&i| embs[i].1.len() >= enrolled + 2).collect();
    println!(
        "LFW, foyers simulés : {households} foyers de {residents} habitants ({enrolled} photos enregistrées chacun) et {residents} inconnus de passage ; {} personnes avec au moins 4 visages ({:.0} s){}{}",
        embs.len(),
        t0.elapsed().as_secs_f32(),
        if night { ", vision nocturne simulée" } else { "" },
        px.map(|p| format!(", visages réduits à ≈ {p:.0} px")).unwrap_or_default()
    );
    let shares = if shares.is_empty() { vec![rules.known_share] } else { shares.to_vec() };
    for (share, tries) in shares.iter().flat_map(|&s| [4usize, 10, 40, 200].map(move |t| (s, t))) {
        if tries == 4 {
            println!("  seuil {:.2}, inconnu après {} visages sans reconnaissance, nommé avec au moins {:.0} % des visages", rules.threshold, rules.stranger_after, share * 100.0);
        }
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let (mut res, mut str_) = (Visits::default(), Visits::default());
        for _ in 0..households {
            // Habitants tirés au hasard, leurs premières photos (mélangées) enregistrées.
            let mut home: Vec<usize> = Vec::new();
            while home.len() < residents.min(enrollable.len()) {
                let i = enrollable[rng.below(enrollable.len())];
                if !home.contains(&i) {
                    home.push(i);
                }
            }
            let mut gallery = FaceGallery { model: "sface".into(), people: Vec::new() };
            let mut ids = Vec::new();
            let mut probes: Vec<Vec<&Vec<f32>>> = Vec::new();
            for &i in &home {
                let mut order: Vec<&Vec<f32>> = embs[i].1.iter().collect();
                for k in (1..order.len()).rev() {
                    order.swap(k, rng.below(k + 1));
                }
                let mut id = String::new();
                for e in &order[..enrolled] {
                    id = gallery.enroll(&embs[i].0, e);
                }
                ids.push(id);
                probes.push(order[enrolled..].to_vec());
            }
            let mut strangers: Vec<usize> = Vec::new();
            while strangers.len() < residents.min(embs.len() - home.len()) {
                let i = rng.below(embs.len());
                if !home.contains(&i) && !strangers.contains(&i) {
                    strangers.push(i);
                }
            }
            // Un passage : `tries` visages tirés parmi les siens (le même revient souvent, comme
            // d'une image à la suivante).
            let visit = |pool: &[&Vec<f32>], me: Option<&str>, out: &mut Visits, rng: &mut Rng| {
                let mut votes = IdentityVotes::default();
                let (mut right, mut wrong, mut stranger) = (false, false, false);
                let mut last = Verdict::Undecided;
                for _ in 0..tries {
                    votes.add(&gallery.identify(pool[rng.below(pool.len())], rules.threshold));
                    last = votes.verdict(rules.stranger_after, share);
                    match &last {
                        Verdict::Person { id, .. } if Some(id.as_str()) == me => right = true,
                        Verdict::Person { .. } => wrong = true,
                        Verdict::Stranger => stranger = true,
                        Verdict::Undecided => {}
                    }
                }
                out.n += 1;
                (out.right, out.wrong, out.stranger) = (out.right + right as u32, out.wrong + wrong as u32, out.stranger + stranger as u32);
                out.final_right += matches!(&last, Verdict::Person { id, .. } if Some(id.as_str()) == me) as u32;
                out.final_stranger += matches!(last, Verdict::Stranger) as u32;
            };
            for (k, pool) in probes.iter().enumerate() {
                visit(pool, Some(&ids[k]), &mut res, &mut rng);
            }
            for &s in &strangers {
                let pool: Vec<&Vec<f32>> = embs[s].1.iter().collect();
                visit(&pool, None, &mut str_, &mut rng);
            }
        }
        let pct = |a: u32, b: u32| a as f32 * 100.0 / b.max(1) as f32;
        println!(
            "  passages de {tries:>3} visages : habitants nommés {:.1} % (à la fin {:.1} %), pris pour un inconnu {:.1} %, pour un autre {:.2} % · inconnus signalés {:.1} % (à la fin {:.1} %), nommés à tort {:.2} %",
            pct(res.right, res.n),
            pct(res.final_right, res.n),
            pct(res.stranger, res.n),
            pct(res.wrong, res.n),
            pct(str_.stranger, str_.n),
            pct(str_.final_stranger, str_.n),
            pct(str_.wrong, str_.n)
        );
    }
    Ok(())
}

// ---- Chutes (vidéos) ---------------------------------------------------------------------------

/// Images d'une vidéo, décodées par FFmpeg à `fps` images par seconde.
struct Video {
    child: std::process::Child,
    width: usize,
    height: usize,
}

impl Video {
    fn open(path: &Path, fps: f32) -> Result<Video> {
        let ffmpeg = homesmart::camera::ffmpeg::find().ok_or_else(|| anyhow!("FFmpeg introuvable"))?;
        // Dimensions : lues dans la description du flux.
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
        let (filter, width) = if (width, height) == (640, 240) { (format!("crop=320:240:320:0,fps={fps}"), 320) } else { (format!("fps={fps}"), width) };
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

fn videos(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            videos(&p, out);
        } else if p.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "mp4" | "avi" | "webm" | "mkv")) {
            out.push(p);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn falls(data: &Path, accel: Accel, fps: f32, limit: Option<usize>, night: bool, verbose: bool, dump: Option<&Path>, only: Option<&str>, hold: Option<f64>, model: &str) -> Result<()> {
    use std::io::Write;
    use homesmart::vision::actions::{Action, ActionEngine, Rules};
    use homesmart::vision::faces::FaceGallery;
    use homesmart::vision::pipeline::{Pipeline, VisionEvent};
    let root = data.join("falls");
    let mut list = Vec::new();
    videos(&root, &mut list);
    if let Some(o) = only {
        list.retain(|p| p.to_string_lossy().replace('\\', "/").contains(o));
    }
    if let Some(n) = limit {
        list.truncate(n);
    }
    if list.is_empty() {
        bail!("aucune vidéo dans {}", root.display());
    }
    println!("Chutes : {} vidéo(s), {fps} analyses/s{}", list.len(), if night { ", vision nocturne simulée" } else { "" });
    let gallery = FaceGallery::default();
    // (vidéos, détectées) pour les chutes, et (vidéos, fausses alertes) pour la vie courante.
    let (mut falls_n, mut falls_hit, mut adl_n, mut adl_false) = (0, 0, 0, 0);
    let mut delays: Vec<f64> = Vec::new();
    let mut no_person = 0;
    let mut csv = match dump {
        Some(p) => {
            let mut f = std::io::BufWriter::new(std::fs::File::create(p)?);
            write!(f, "video,fall,t,w,h,track,score,x0,y0,x1,y1")?;
            for k in 0..17 {
                write!(f, ",k{k}x,k{k}y,k{k}s")?;
            }
            writeln!(f)?;
            Some(f)
        }
        None => None,
    };
    for path in &list {
        let rel = path.strip_prefix(&root).unwrap_or(path).to_string_lossy().replace('\\', "/");
        let is_fall = is_fall_video(path);
        let mut video = Video::open(path, fps)?;
        let persons = RtmoPersons::with_model(person_model(model)?, accel)?;
        let rules = Rules { fall_hold: hold.unwrap_or(Rules::default().fall_hold), ..Rules::default() };
        let mut p = Pipeline::new(Box::new(persons), None, ActionEngine::new(Vec::new(), rules));
        let (mut i, mut first_fall, mut seen, mut downs) = (0u64, None, false, 0);
        // Les vidéos s'arrêtent souvent juste après la chute ; dans la vraie vie, la personne
        // reste là : on prolonge de 2 s avec la dernière image (personne immobile).
        let tail = (2.0 * fps).round() as u64;
        let mut last: Option<Frame> = None;
        let mut extra = 0;
        while let Some(frame) = video.next().inspect(|f| last = Some(f.clone())).or_else(|| {
            extra += 1;
            (extra <= tail).then(|| last.clone()).flatten()
        }) {
            let frame = if night { prepare(frame, true, 1.0, i) } else { frame };
            let now = i as f64 / fps as f64;
            let (views, events) = p.process(now, &frame, &gallery)?;
            seen |= !views.is_empty();
            if let Some(f) = csv.as_mut() {
                for v in &views {
                    write!(f, "{rel},{},{now:.3},{},{},{},{:.3},{:.4},{:.4},{:.4},{:.4}", is_fall as u8, frame.width, frame.height, v.track, v.score, v.bbox.x0, v.bbox.y0, v.bbox.x1, v.bbox.y1)?;
                    match &v.pose {
                        Some(pose) => {
                            for k in &pose.points {
                                write!(f, ",{:.4},{:.4},{:.3}", k.at.x, k.at.y, k.score)?;
                            }
                        }
                        None => write!(f, "{}", ",0,0,0".repeat(17))?,
                    }
                    writeln!(f)?;
                }
            }
            for e in &events {
                if let (true, VisionEvent::Action(a)) = (verbose && only.is_some(), e) {
                    println!("    {now:5.2} s : piste {} {:?}", a.track, a.action);
                }
                if let VisionEvent::Action(a) = e {
                    match a.action {
                        Action::Fall if first_fall.is_none() => first_fall = Some(now),
                        Action::Down { .. } => downs += 1,
                        _ => {}
                    }
                }
            }
            i += 1;
        }
        if !seen {
            no_person += 1;
        }
        let hit = first_fall.is_some();
        if is_fall {
            falls_n += 1;
            falls_hit += hit as u32;
            if let Some(t) = first_fall {
                delays.push(t);
            }
        } else {
            adl_n += 1;
            adl_false += hit as u32;
        }
        if verbose || (is_fall != hit) {
            let what = match (is_fall, hit) {
                (true, true) => "chute détectée",
                (true, false) => "CHUTE MANQUÉE",
                (false, true) => "FAUSSE ALERTE",
                (false, false) => "rien (juste)",
            };
            println!("  {rel} : {what}{}{}{}", first_fall.map(|t| format!(" à {t:.1} s")).unwrap_or_default(), if downs > 0 { format!(", au sol ×{downs}") } else { String::new() }, if seen { "" } else { " (personne jamais vue)" });
        }
    }
    println!(
        "Chutes trouvées : {falls_hit}/{falls_n} ({:.0} %) · fausses alertes : {adl_false}/{adl_n} vidéos de vie courante · vidéos sans personne vue : {no_person}",
        falls_hit as f32 * 100.0 / falls_n.max(1) as f32
    );
    if !delays.is_empty() {
        delays.sort_by(f64::total_cmp);
        println!("  instant de détection dans la vidéo : médian {:.1} s", delays[delays.len() / 2]);
    }
    Ok(())
}

/// Vidéo de chute (sinon vie courante), d'après le dossier parent et le nom du fichier
/// (« Fall forward/… », « fall-01… », « adl-03… », « Walk/… »), sans le nom des jeux de données
/// (« CAUCAFall », « urfall »).
fn is_fall_video(path: &Path) -> bool {
    let parent = path.parent().and_then(|p| p.file_name()).unwrap_or_default().to_string_lossy().to_ascii_lowercase();
    let key = format!("{parent}/{}", path.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase()).replace("caucafall", "").replace("urfall", "");
    key.contains("fall") && !key.contains("adl")
}

// ---- Alertes sur les vidéos ---------------------------------------------------------------------

/// Détecteur rejoué : les personnes trouvées d'avance pour l'image en cours.
struct Replay(std::sync::Arc<std::sync::Mutex<Vec<Detection>>>);

impl PersonDetector for Replay {
    fn detect(&mut self, _frame: &Frame) -> Result<Vec<Detection>> {
        Ok(std::mem::take(&mut *self.0.lock().unwrap()))
    }
}

/// Jeux et sports de Kinetics où l'on tombe et se frappe pour de bon : une chute n'y est pas une
/// fausse alerte (un coup, si : ce n'est pas une agression).
const ROUGH_PLAY: [&str; 5] = ["capoeira", "pillow fight", "punching person (boxing)", "tackling", "wrestling"];

/// Nature d'une vidéo du jeu d'essai, pour le bilan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    /// Chute (UR Fall, CAUCAFall).
    Fall,
    /// Bagarre.
    Fight,
    /// Jeu ou sport brutal (on y tombe et se frappe pour de bon).
    Rough,
    /// Gifles de Kinetics : vraies ou pour rire, comptées à part.
    Ambiguous,
    /// Vie courante : rien n'y devrait déclencher d'alerte.
    Calm,
}

impl Kind {
    fn of(rel: &str, path: &Path) -> Kind {
        let class = rel.split('/').nth(2).unwrap_or("");
        if rel.starts_with("falls/") && is_fall_video(path) {
            Kind::Fall
        } else if super::violence::is_violent(&rel.to_ascii_lowercase()) {
            Kind::Fight
        } else if rel.starts_with("friendly/") && ROUGH_PLAY.contains(&class) {
            Kind::Rough
        } else if rel.starts_with("ambiguous/") {
            Kind::Ambiguous
        } else {
            Kind::Calm
        }
    }
}

/// Ce que la chaîne a signalé dans un groupe de vidéos (une vidéo de chute « trouvée » a au moins
/// une chute ; une alerte « dans la vidéo » est née avant la fin figée ajoutée au bout).
#[derive(Clone, Default)]
struct Alarms {
    videos: u32,
    secs: f64,
    found_falls: u32,
    falls: u32,
    falls_inside: u32,
    found_fights: u32,
    violence: u32,
    arms_up: u32,
    arms_inside: u32,
    down: u32,
    hands: u32,
    /// Arrivées au-delà du plus grand nombre de personnes vues ensemble (pistes coupées en deux).
    extra_arrivals: u32,
}

impl Alarms {
    fn add(&mut self, o: &Alarms) {
        self.videos += o.videos;
        self.secs += o.secs;
        self.found_falls += o.found_falls;
        self.falls += o.falls;
        self.falls_inside += o.falls_inside;
        self.found_fights += o.found_fights;
        self.violence += o.violence;
        self.arms_up += o.arms_up;
        self.arms_inside += o.arms_inside;
        self.down += o.down;
        self.hands += o.hands;
        self.extra_arrivals += o.extra_arrivals;
    }
}

#[allow(clippy::too_many_arguments)]
fn alarms(data: &Path, accel: Accel, fps: f32, model: &str, night: bool, cache: Option<&Path>, only: Option<&str>, verbose: bool, dump: Option<&Path>) -> Result<()> {
    use homesmart::vision::actions::{Action, ActionEngine, Rules};
    use homesmart::vision::faces::FaceGallery;
    use homesmart::vision::pipeline::VisionEvent;
    use std::collections::BTreeMap;
    use std::io::Write;
    let mut list = Vec::new();
    for sub in ["falls", "violence", "friendly", "ambiguous"] {
        super::violence::videos(&data.join(sub), &mut list);
    }
    let rel_of = |p: &Path| p.strip_prefix(data).unwrap_or(p).to_string_lossy().replace('\\', "/");
    if let Some(o) = only {
        list.retain(|p| rel_of(p).contains(o));
    }
    if list.is_empty() {
        bail!("aucune vidéo dans {}", data.display());
    }
    if let Some(c) = cache {
        std::fs::create_dir_all(c)?;
    }
    println!("Alertes : {} vidéo(s), {fps} analyses/s, RTMO-{model}{}", list.len(), if night { ", vision nocturne simulée" } else { "" });
    let gallery = FaceGallery::default();
    let mut rtmo = None;
    let mut groups: BTreeMap<String, Alarms> = BTreeMap::new();
    let mut kinds: BTreeMap<Kind, Alarms> = BTreeMap::new();
    let mut csv = match dump {
        Some(p) => {
            let mut f = std::io::BufWriter::new(std::fs::File::create(p)?);
            write!(f, "video,t,w,h,track,score,x0,y0,x1,y1")?;
            for k in 0..17 {
                write!(f, ",k{k}x,k{k}y,k{k}s")?;
            }
            writeln!(f)?;
            Some(f)
        }
        None => None,
    };
    let t_all = Instant::now();
    for (n, path) in list.iter().enumerate() {
        let rel = rel_of(path);
        let kind = Kind::of(&rel, path);
        let found = super::violence::found(path, &rel, fps, night, model, accel, cache, &mut rtmo)?;
        // La chaîne de l'application, sur les personnes trouvées d'avance.
        let shared = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut p = Pipeline::new(Box::new(Replay(shared.clone())), None, ActionEngine::new(Vec::new(), Rules::default())).with_violence();
        let frame = Frame { seq: 0, width: found.width, height: found.height, rgba: Vec::new() };
        // Les vidéos s'arrêtent souvent juste après l'action : 2 s de plus avec la dernière image
        // (une personne tombée reste au sol ; ailleurs, cette fin figée peut créer des alertes que
        // la vraie vie n'aurait pas : elles sont comptées à part).
        let last = found.frames.last().cloned().unwrap_or_default();
        let tail = (2.0 * fps).round() as usize;
        let end = found.frames.len() as f64 / fps as f64;
        let (mut arrived, mut most) = (0u32, 0usize);
        let mut v = Alarms { videos: 1, secs: end, ..Alarms::default() };
        for (i, dets) in found.frames.iter().chain(std::iter::repeat_n(&last, tail)).enumerate() {
            let now = i as f64 / fps as f64;
            *shared.lock().unwrap() = dets.clone();
            let (views, events) = p.process(now, &frame, &gallery)?;
            most = most.max(views.len());
            if let Some(f) = csv.as_mut() {
                for w in views.iter().filter(|w| p.tracker.track(w.track).is_some_and(|t| t.last_seen >= now)) {
                    write!(f, "{},{now:.3},{},{},{},{:.3},{:.4},{:.4},{:.4},{:.4}", rel.replace(',', ";"), found.width, found.height, w.track, w.score, w.bbox.x0, w.bbox.y0, w.bbox.x1, w.bbox.y1)?;
                    match &w.pose {
                        Some(pose) => {
                            for k in &pose.points {
                                write!(f, ",{:.4},{:.4},{:.3}", k.at.x, k.at.y, k.score)?;
                            }
                        }
                        None => write!(f, "{}", ",0,0,0".repeat(17))?,
                    }
                    writeln!(f)?;
                }
            }
            let inside = (now < end) as u32;
            for e in &events {
                match e {
                    VisionEvent::Action(a) => match a.action {
                        Action::Arrived => arrived += 1,
                        Action::Fall => (v.falls, v.falls_inside) = (v.falls + 1, v.falls_inside + inside),
                        Action::Down { .. } => v.down += 1,
                        Action::ArmsUp => (v.arms_up, v.arms_inside) = (v.arms_up + 1, v.arms_inside + inside),
                        Action::HandRaised => v.hands += 1,
                        _ => continue,
                    },
                    VisionEvent::Violence(_) => v.violence += 1,
                    _ => continue,
                }
                if verbose {
                    println!("  {} : {now:5.2} s {e:?}", super::violence::short(&rel));
                }
            }
        }
        v.extra_arrivals = arrived.saturating_sub(most as u32);
        v.found_falls = (kind == Kind::Fall && v.falls > 0) as u32;
        v.found_fights = (kind == Kind::Fight && v.violence > 0) as u32;
        groups.entry(super::violence::group(&rel)).or_default().add(&v);
        kinds.entry(kind).or_default().add(&v);
        if (n + 1) % 250 == 0 {
            eprintln!("  … {} / {} ({:.0} s)", n + 1, list.len(), t_all.elapsed().as_secs_f32());
        }
    }
    println!();
    println!("  {:<44} {:>6} {:>7} {:>6} {:>8} {:>5} {:>5} {:>5} {:>9}", "groupe", "vidéos", "minutes", "chutes", "violence", "bras", "sol", "main", "arrivées+");
    for (name, g) in &groups {
        println!("  {name:<44} {:>6} {:>7.1} {:>6} {:>8} {:>5} {:>5} {:>5} {:>9}", g.videos, g.secs / 60.0, g.falls, g.violence, g.arms_up, g.down, g.hands, g.extra_arrivals);
    }
    let get = |k: Kind| kinds.get(&k).cloned().unwrap_or_default();
    let (fall, fight, rough, calm) = (get(Kind::Fall), get(Kind::Fight), get(Kind::Rough), get(Kind::Calm));
    let pct = |a: u32, b: u32| a as f32 * 100.0 / b.max(1) as f32;
    let hours = calm.secs / 3600.0;
    let per_hour = |n: u32| n as f64 / hours.max(1e-9);
    println!();
    println!("  Vie courante : {} vidéos, {hours:.2} h (« dans la vidéo » : alerte née avant la fin figée)", calm.videos);
    println!(
        "  Chutes : trouvées {}/{} ({:.0} %) · fausses alertes {} ({:.1} par heure), dont {} dans la vidéo ({:.1} par heure)",
        fall.found_falls,
        fall.videos,
        pct(fall.found_falls, fall.videos),
        calm.falls,
        per_hour(calm.falls),
        calm.falls_inside,
        per_hour(calm.falls_inside)
    );
    let mut no_fight = calm.clone();
    no_fight.add(&rough);
    no_fight.add(&fall);
    println!(
        "  Violence : trouvée {}/{} ({:.0} %) · fausses alertes {} sur {} vidéos sans violence ({:.1} par heure)",
        fight.found_fights,
        fight.videos,
        pct(fight.found_fights, fight.videos),
        no_fight.violence,
        no_fight.videos,
        no_fight.violence as f64 * 3600.0 / no_fight.secs.max(1e-9)
    );
    println!(
        "  Bras levés (alerte) : vie courante {} ({:.1} par heure), dont {} dans la vidéo · vidéos de chute {} · bagarres {} · jeux brutaux {}",
        calm.arms_up,
        per_hour(calm.arms_up),
        calm.arms_inside,
        fall.arms_up,
        fight.arms_up,
        rough.arms_up
    );
    let all: f64 = kinds.values().map(|a| a.secs).sum::<f64>() / 3600.0;
    let extra: u32 = kinds.values().map(|a| a.extra_arrivals).sum();
    println!("  Arrivées en trop (piste perdue puis rouverte, doublon) : {extra} en {all:.2} h ({:.0} par heure), dont vie courante {} · {:.0} s", extra as f64 / all.max(1e-9), calm.extra_arrivals, t_all.elapsed().as_secs_f32());
    Ok(())
}

// ---- Dossier d'images ---------------------------------------------------------------------------

fn images(dir: &Path, accel: Accel) -> Result<()> {
    let mut list: Vec<PathBuf> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png"))).collect();
    list.sort();
    let mut rtmo = RtmoPersons::new(accel)?;
    rtmo.threshold = 0.3;
    let mut faces = YunetFaces::new(accel)?;
    let mut yolox = YoloxObjects::new(accel)?;
    yolox.threshold = 0.3;
    for p in &list {
        let frame = picture::load(p)?;
        let dets = rtmo.detect(&frame)?;
        let mut line = format!("{} ({}×{}) : ", p.file_name().unwrap_or_default().to_string_lossy(), frame.width, frame.height);
        let persons: Vec<String> = dets
            .iter()
            .map(|d| {
                let posture = d.pose.as_ref().map(|po| format!("torse {:.0}°", po.torso_angle(frame.width as f32, frame.height as f32).unwrap_or(f32::NAN))).unwrap_or_default();
                let face = faces.detect(&frame, homesmart::vision::pipeline::Pipeline::head_region(&d.bbox)).ok().and_then(|f| f.into_iter().map(|f| f.score).reduce(f32::max));
                format!("personne {:.2} [{posture}{}]", d.score, face.map(|s| format!(", visage {s:.2}")).unwrap_or_default())
            })
            .collect();
        line.push_str(&if persons.is_empty() { "personne : aucune".to_string() } else { persons.join(" · ") });
        let objs = yolox.detect_raw(&frame, false)?;
        if !objs.is_empty() {
            let o: Vec<String> = objs.iter().map(|(c, _, s)| format!("classe {c} {s:.2}")).collect();
            line.push_str(&format!(" · objets : {}", o.join(", ")));
        }
        println!("{line}");
    }
    Ok(())
}
