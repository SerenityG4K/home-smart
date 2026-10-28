//! Les modèles réels, branchés sur la chaîne d'analyse (traits de `pipeline`) :
//!
//! - **RTMO-s** (MMPose) : personnes + squelettes en un passage. Entrée `input` [1,3,640,640],
//!   BGR 0…255 sans normalisation, image calée en haut à gauche sur fond gris 114 ; sorties
//!   `dets` [1,N,5] (x0, y0, x1, y1, score) et `keypoints` [1,N,17,3] (x, y, confiance).
//! - **YuNet** (OpenCV Zoo) : visages + 5 points. Entrée [1,3,640,640] BGR 0…255 calée en haut
//!   à gauche ; sorties `cls_s`, `obj_s`, `bbox_s`, `kps_s` par pas s = 8, 16, 32, décodées
//!   comme dans `face_detect.cpp` d'OpenCV.
//! - **SFace** (OpenCV Zoo) : empreinte de visage. Entrée `data` [1,3,112,112], RVB 0…255,
//!   visage redressé sur le gabarit 112×112 ; sortie `fc1` [1,128].

use super::engine::{self, Accel};
use super::geom::{nms, BBox, Point};
use super::models::{ModelSpec, RTMO_S, SFACE, YUNET};
use super::pipeline::{FaceBox, FaceDetector, FaceEmbedder, PersonDetector};
use super::prep::{align_face, letterbox, letterbox_region, Layout};
use super::track::Detection;
use super::{Keypoint, Pose};
use crate::camera::Frame;
use anyhow::{anyhow, bail, Result};
use ort::session::Session;
use ort::value::TensorRef;

fn session(spec: &ModelSpec, accel: Accel) -> Result<(Session, &'static str)> {
    if !spec.is_installed() {
        bail!("modèle « {} » absent : installe les modèles d'IA d'abord", spec.id);
    }
    let l = engine::load(&spec.path(), accel, &[("batch", 1)])?;
    Ok((l.session, l.backend))
}

/// Personnes et squelettes (RTMO-s).
pub struct RtmoPersons {
    session: Session,
    pub backend: &'static str,
    /// Score minimal d'une personne renvoyée (le suivi n'ouvre de piste qu'au-dessus de son
    /// propre seuil ; en dessous, la détection ne fait que prolonger une piste).
    pub threshold: f32,
    input: Vec<f32>,
}

impl RtmoPersons {
    pub const SIZE: usize = 640;

    pub fn new(accel: Accel) -> Result<Self> {
        Self::with_model(&RTMO_S, accel)
    }

    /// Une autre taille du même modèle (RTMO-m : plus précis, plus lent).
    pub fn with_model(spec: &ModelSpec, accel: Accel) -> Result<Self> {
        let (session, backend) = session(spec, accel)?;
        Ok(RtmoPersons { session, backend, threshold: 0.25, input: Vec::new() })
    }
}

impl PersonDetector for RtmoPersons {
    fn detect(&mut self, frame: &Frame) -> Result<Vec<Detection>> {
        let s = Self::SIZE;
        let lb = letterbox(&frame.rgba, frame.width, frame.height, s, s, 114, false, Layout::raw(true), &mut self.input);
        let out = self.session.run(ort::inputs!["input" => TensorRef::from_array_view(([1usize, 3, s, s], &self.input[..]))?])?;
        let (dshape, dets) = out["dets"].try_extract_tensor::<f32>()?;
        let (_, kps) = out["keypoints"].try_extract_tensor::<f32>()?;
        let n = dshape.get(1).copied().unwrap_or(0).max(0) as usize;
        if dets.len() < n * 5 || kps.len() < n * 17 * 3 {
            return Err(anyhow!("RTMO : sorties inattendues ({dshape:?})"));
        }
        let mut found: Vec<(BBox, f32, Pose)> = Vec::new();
        for i in 0..n {
            let d = &dets[i * 5..i * 5 + 5];
            if d[4] < self.threshold {
                continue;
            }
            let (a, b) = (lb.to_norm(d[0], d[1]), lb.to_norm(d[2], d[3]));
            let bbox = BBox::new(a.x, a.y, b.x, b.y).clamp();
            if bbox.area() <= 0.0 {
                continue;
            }
            let mut points = [Keypoint::default(); 17];
            for (k, p) in points.iter_mut().enumerate() {
                let o = (i * 17 + k) * 3;
                *p = Keypoint { at: lb.to_norm(kps[o], kps[o + 1]), score: kps[o + 2] };
            }
            found.push((bbox, d[4], Pose { points }));
        }
        let boxes: Vec<(BBox, f32)> = found.iter().map(|(b, s, _)| (*b, *s)).collect();
        Ok(nms(&boxes, 0.45).into_iter().map(|i| Detection { bbox: found[i].0, score: found[i].1, pose: Some(found[i].2.clone()) }).collect())
    }
}

/// Visages et 5 points (YuNet), cherchés dans une région de l'image.
pub struct YunetFaces {
    session: Session,
    pub backend: &'static str,
    pub threshold: f32,
    input: Vec<f32>,
}

impl YunetFaces {
    pub const SIZE: usize = 640;

    pub fn new(accel: Accel) -> Result<Self> {
        let (session, backend) = session(&YUNET, accel)?;
        Ok(YunetFaces { session, backend, threshold: 0.7, input: Vec::new() })
    }
}

impl FaceDetector for YunetFaces {
    fn detect(&mut self, frame: &Frame, roi: BBox) -> Result<Vec<FaceBox>> {
        let s = Self::SIZE;
        let (fw, fh) = (frame.width as f32, frame.height as f32);
        let region = (roi.x0 * fw, roi.y0 * fh, roi.x1 * fw, roi.y1 * fh);
        // Une petite région est agrandie (au plus ×4 : au-delà, l'image n'apporte plus rien).
        let lb = letterbox_region(&frame.rgba, frame.width, frame.height, region, s, s, 0, false, Some(4.0), Layout::raw(true), &mut self.input);
        let out = self.session.run(ort::inputs!["input" => TensorRef::from_array_view(([1usize, 3, s, s], &self.input[..]))?])?;
        // Coordonnées dans l'entrée du modèle → normalisées dans l'image entière.
        let to_frame = |x: f32, y: f32| {
            let p = lb.to_norm(x, y);
            Point::new(roi.x0 + p.x * roi.width(), roi.y0 + p.y * roi.height())
        };
        let mut found: Vec<FaceBox> = Vec::new();
        for stride in [8usize, 16, 32] {
            let (_, cls) = out[format!("cls_{stride}").as_str()].try_extract_tensor::<f32>()?;
            let (_, obj) = out[format!("obj_{stride}").as_str()].try_extract_tensor::<f32>()?;
            let (_, bbox) = out[format!("bbox_{stride}").as_str()].try_extract_tensor::<f32>()?;
            let (_, kps) = out[format!("kps_{stride}").as_str()].try_extract_tensor::<f32>()?;
            let cols = s / stride;
            let st = stride as f32;
            for idx in 0..cls.len().min(obj.len()) {
                let score = (cls[idx].clamp(0.0, 1.0) * obj[idx].clamp(0.0, 1.0)).sqrt();
                if score < self.threshold {
                    continue;
                }
                let (c, r) = ((idx % cols) as f32, (idx / cols) as f32);
                let b = &bbox[idx * 4..idx * 4 + 4];
                let (cx, cy) = ((c + b[0]) * st, (r + b[1]) * st);
                let (w, h) = (b[2].exp() * st, b[3].exp() * st);
                let (p0, p1) = (to_frame(cx - w / 2.0, cy - h / 2.0), to_frame(cx + w / 2.0, cy + h / 2.0));
                let k = &kps[idx * 10..idx * 10 + 10];
                let landmarks = [0, 1, 2, 3, 4].map(|j| to_frame((k[2 * j] + c) * st, (k[2 * j + 1] + r) * st));
                found.push(FaceBox { bbox: BBox::new(p0.x, p0.y, p1.x, p1.y), landmarks, score });
            }
        }
        let boxes: Vec<(BBox, f32)> = found.iter().map(|f| (f.bbox, f.score)).collect();
        Ok(nms(&boxes, 0.3).into_iter().map(|i| found[i].clone()).collect())
    }
}

/// Animaux et véhicules (YOLOX-s, 80 sortes d'objets COCO ; les personnes sont laissées à RTMO).
///
/// Entrée `images` [1,3,640,640] BGR 0…255 sans normalisation, image calée en haut à gauche sur
/// fond gris 114. Sortie `output` [1,8400,85] *non décodée* : pour chaque case de chaque grille
/// (pas 8, 16, 32, lignes puis colonnes) : x, y, w, h bruts, objet, puis 80 probabilités de
/// classe (déjà passées par la sigmoïde). Décodage : `xy = (p + case) × pas`, `wh = exp(p) × pas`.
pub struct YoloxObjects {
    session: Session,
    pub backend: &'static str,
    pub threshold: f32,
    input: Vec<f32>,
    /// Personnes de la dernière analyse.
    persons: Vec<(BBox, f32)>,
}

impl YoloxObjects {
    pub const SIZE: usize = 640;

    pub fn new(accel: Accel) -> Result<Self> {
        let (session, backend) = session(&super::models::YOLOX_S, accel)?;
        // Seuil bas : chaque sorte a ensuite le sien (`ObjectKind::threshold`), et les personnes
        // servent de deuxième avis dès 0,2.
        Ok(YoloxObjects { session, backend, threshold: 0.2, input: Vec::new(), persons: Vec::new() })
    }
}

impl super::pipeline::ObjectDetector for YoloxObjects {
    fn detect(&mut self, frame: &Frame) -> Result<Vec<super::objects::ObjectDetection>> {
        let raw = self.detect_raw(frame, true)?;
        self.persons = raw.iter().filter(|r| r.0 == 0).map(|r| (r.1, r.2)).collect();
        Ok(super::objects::from_raw(&raw))
    }

    fn last_persons(&self) -> Vec<(BBox, f32)> {
        self.persons.clone()
    }
}

impl YoloxObjects {
    /// Toutes les classes COCO (indice, boîte, score), doublons supprimés classe par classe ;
    /// `persons` : garder aussi la classe 0 (pour vérifier le décodage face au modèle des personnes).
    pub fn detect_raw(&mut self, frame: &Frame, persons: bool) -> Result<Vec<(usize, BBox, f32)>> {
        let s = Self::SIZE;
        let lb = letterbox(&frame.rgba, frame.width, frame.height, s, s, 114, false, Layout::raw(true), &mut self.input);
        let out = self.session.run(ort::inputs!["images" => TensorRef::from_array_view(([1usize, 3, s, s], &self.input[..]))?])?;
        let (shape, data) = out["output"].try_extract_tensor::<f32>()?;
        let rows = shape.get(1).copied().unwrap_or(0).max(0) as usize;
        let width = shape.get(2).copied().unwrap_or(0).max(0) as usize;
        if width < 85 || data.len() < rows * width {
            bail!("YOLOX : sorties inattendues ({shape:?})");
        }
        let mut found: Vec<(usize, BBox, f32)> = Vec::new();
        let mut row = 0usize;
        for stride in [8usize, 16, 32] {
            let cells = s / stride;
            for gy in 0..cells {
                for gx in 0..cells {
                    let r = &data[row * width..row * width + width];
                    row += 1;
                    // Meilleure classe (hors personne sauf demande), pondérée par la présence d'un objet.
                    let first = if persons { 0 } else { 1 };
                    let (class, p) = r[5..85].iter().enumerate().skip(first).fold((0, 0.0f32), |best, (i, &v)| if v > best.1 { (i, v) } else { best });
                    let score = r[4] * p;
                    if score < self.threshold {
                        continue;
                    }
                    let st = stride as f32;
                    let (cx, cy) = ((r[0] + gx as f32) * st, (r[1] + gy as f32) * st);
                    let (w, h) = (r[2].exp() * st, r[3].exp() * st);
                    let (a, b) = (lb.to_norm(cx - w / 2.0, cy - h / 2.0), lb.to_norm(cx + w / 2.0, cy + h / 2.0));
                    let bbox = BBox::new(a.x, a.y, b.x, b.y).clamp();
                    if bbox.area() > 0.0 {
                        found.push((class, bbox, score));
                    }
                }
            }
        }
        // Doublons supprimés classe par classe.
        let mut out_objs = Vec::new();
        let mut classes: Vec<usize> = found.iter().map(|f| f.0).collect();
        classes.sort_unstable();
        classes.dedup();
        for class in classes {
            let mine: Vec<(BBox, f32)> = found.iter().filter(|f| f.0 == class).map(|f| (f.1, f.2)).collect();
            out_objs.extend(nms(&mine, 0.45).into_iter().map(|i| (class, mine[i].0, mine[i].1)));
        }
        Ok(out_objs)
    }
}

/// Empreintes de visage (SFace).
pub struct SfaceEmbedder {
    session: Session,
    pub backend: &'static str,
    input: Vec<f32>,
}

impl SfaceEmbedder {
    pub fn new(accel: Accel) -> Result<Self> {
        let (session, backend) = session(&SFACE, accel)?;
        Ok(SfaceEmbedder { session, backend, input: Vec::new() })
    }
}

impl FaceEmbedder for SfaceEmbedder {
    fn embed(&mut self, frame: &Frame, face: &FaceBox) -> Result<Vec<f32>> {
        let (fw, fh) = (frame.width as f32, frame.height as f32);
        let lm = face.landmarks.map(|p| Point::new(p.x * fw, p.y * fh));
        align_face(&frame.rgba, frame.width, frame.height, &lm, 112, Layout::raw(false), &mut self.input);
        let out = self.session.run(ort::inputs!["data" => TensorRef::from_array_view(([1usize, 3, 112, 112], &self.input[..]))?])?;
        let (_, v) = out["fc1"].try_extract_tensor::<f32>()?;
        let mut e = v.to_vec();
        super::faces::normalize(&mut e);
        Ok(e)
    }

    fn model(&self) -> &str {
        SFACE.id
    }
}
