//! Mesure de la qualité des détections sur des images annotées (COCO…) : précision, rappel,
//! précision moyenne (AP), meilleur seuil ; et vérification de visages (paires même / autre
//! personne). Sert au réglage des seuils, avec `homesmart-cli vision eval`.

use super::geom::BBox;

/// Une vraie boîte (annotation). `ignore` : foule, ou trop petite pour compter ; une détection
/// qui tombe dessus n'est ni juste ni fausse.
#[derive(Clone, Copy, Debug)]
pub struct Truth {
    pub bbox: BBox,
    pub ignore: bool,
}

/// Détections notées sur un ensemble d'images, pour une sorte d'objet.
#[derive(Clone, Debug, Default)]
pub struct Scores {
    /// (score, juste ?) de chaque détection non ignorée.
    scored: Vec<(f32, bool)>,
    /// Vraies boîtes à trouver (hors ignorées).
    pub positives: usize,
    pub images: usize,
    /// Images sans aucune vraie boîte, et (score max des détections) dans chacune.
    empty_images: Vec<f32>,
}

impl Scores {
    /// Ajoute une image : appariement glouton par score décroissant (règle COCO), IoU ≥ `iou_min`.
    /// Renvoie, pour chaque détection, l'indice de la vraie boîte appariée.
    pub fn add_image(&mut self, dets: &[(BBox, f32)], truths: &[Truth], iou_min: f32) -> Vec<Option<usize>> {
        self.images += 1;
        self.positives += truths.iter().filter(|t| !t.ignore).count();
        if truths.iter().all(|t| t.ignore) {
            self.empty_images.push(dets.iter().map(|d| d.1).fold(0.0, f32::max));
        }
        let mut order: Vec<usize> = (0..dets.len()).collect();
        order.sort_by(|&a, &b| dets[b].1.total_cmp(&dets[a].1));
        let mut taken = vec![false; truths.len()];
        let mut matched = vec![None; dets.len()];
        for i in order {
            let (bbox, score) = dets[i];
            // Meilleure vraie boîte libre ; à défaut, une boîte ignorée qui la couvre.
            let best = truths.iter().enumerate().filter(|(j, t)| !taken[*j] && !t.ignore).map(|(j, t)| (j, t.bbox.iou(&bbox))).filter(|(_, iou)| *iou >= iou_min).max_by(|a, b| a.1.total_cmp(&b.1));
            match best {
                Some((j, _)) => {
                    taken[j] = true;
                    matched[i] = Some(j);
                    self.scored.push((score, true));
                }
                None => {
                    let on_ignored = truths.iter().any(|t| t.ignore && (t.bbox.iou(&bbox) >= iou_min || covered(&bbox, &t.bbox) >= 0.6));
                    if !on_ignored {
                        self.scored.push((score, false));
                    }
                }
            }
        }
        matched
    }

    /// Précision et rappel en ne gardant que les détections de score ≥ `threshold`.
    pub fn at(&self, threshold: f32) -> (f32, f32) {
        let (tp, fp) = self.scored.iter().filter(|s| s.0 >= threshold).fold((0, 0), |(tp, fp), s| if s.1 { (tp + 1, fp) } else { (tp, fp + 1) });
        let p = if tp + fp > 0 { tp as f32 / (tp + fp) as f32 } else { 1.0 };
        let r = if self.positives > 0 { tp as f32 / self.positives as f32 } else { 1.0 };
        (p, r)
    }

    /// Part des images sans vraie boîte où au moins une détection dépasse `threshold`
    /// (fausse alerte « quelqu'un est là » alors que la pièce est vide).
    pub fn false_alarm_images(&self, threshold: f32) -> (usize, usize) {
        (self.empty_images.iter().filter(|&&s| s >= threshold).count(), self.empty_images.len())
    }

    /// Précision moyenne (AP), interpolée sur 101 points de rappel comme COCO.
    pub fn ap(&self) -> f32 {
        if self.positives == 0 {
            return 0.0;
        }
        let mut s = self.scored.clone();
        s.sort_by(|a, b| b.0.total_cmp(&a.0));
        let (mut tp, mut fp) = (0usize, 0usize);
        let mut curve: Vec<(f32, f32)> = Vec::with_capacity(s.len());
        for (_, ok) in &s {
            if *ok {
                tp += 1
            } else {
                fp += 1
            }
            curve.push((tp as f32 / self.positives as f32, tp as f32 / (tp + fp) as f32));
        }
        // Précision rendue décroissante (enveloppe), puis lue aux 101 rappels.
        for i in (0..curve.len().saturating_sub(1)).rev() {
            curve[i].1 = curve[i].1.max(curve[i + 1].1);
        }
        let mut sum = 0.0;
        for k in 0..=100 {
            let r = k as f32 / 100.0;
            if let Some(&(_, p)) = curve.iter().find(|(rc, _)| *rc >= r - 1e-6) {
                sum += p;
            }
        }
        sum / 101.0
    }

    /// Seuil qui maximise F1 parmi `candidates` : (seuil, précision, rappel, F1).
    pub fn best_f1(&self, candidates: &[f32]) -> (f32, f32, f32, f32) {
        candidates
            .iter()
            .map(|&t| {
                let (p, r) = self.at(t);
                let f1 = if p + r > 0.0 { 2.0 * p * r / (p + r) } else { 0.0 };
                (t, p, r, f1)
            })
            .max_by(|a, b| a.3.total_cmp(&b.3))
            .unwrap_or((0.5, 0.0, 0.0, 0.0))
    }
}

/// Part de `a` couverte par `b`.
fn covered(a: &BBox, b: &BBox) -> f32 {
    let w = (a.x1.min(b.x1) - a.x0.max(b.x0)).max(0.0);
    let h = (a.y1.min(b.y1) - a.y0.max(b.y0)).max(0.0);
    if a.area() > 0.0 {
        w * h / a.area()
    } else {
        0.0
    }
}

/// Similarités de paires de visages (même personne ou non) : taux d'erreur selon le seuil.
#[derive(Clone, Debug, Default)]
pub struct Pairs {
    pub same: Vec<f32>,
    pub different: Vec<f32>,
    /// Paires sans visage trouvé dans l'une des deux images.
    pub missed: usize,
}

impl Pairs {
    /// (acceptés à tort parmi les différents, refusés à tort parmi les mêmes) au seuil `t`.
    pub fn rates(&self, t: f32) -> (f32, f32) {
        let far = self.different.iter().filter(|&&s| s >= t).count() as f32 / self.different.len().max(1) as f32;
        let frr = self.same.iter().filter(|&&s| s < t).count() as f32 / self.same.len().max(1) as f32;
        (far, frr)
    }

    /// Exactitude (paires bien classées) au seuil `t`.
    pub fn accuracy(&self, t: f32) -> f32 {
        let ok = self.same.iter().filter(|&&s| s >= t).count() + self.different.iter().filter(|&&s| s < t).count();
        ok as f32 / (self.same.len() + self.different.len()).max(1) as f32
    }

    /// Plus petit seuil (pas de 0,01) qui garde les acceptations à tort sous `far`.
    pub fn threshold_for_far(&self, far: f32) -> f32 {
        (0..=100).map(|k| k as f32 / 100.0).find(|&t| self.rates(t).0 <= far).unwrap_or(1.0)
    }

    /// Seuil de meilleure exactitude.
    pub fn best_accuracy(&self) -> (f32, f32) {
        (0..=100).map(|k| k as f32 / 100.0).map(|t| (t, self.accuracy(t))).max_by(|a, b| a.1.total_cmp(&b.1)).unwrap_or((0.5, 0.0))
    }
}

/// Similarité des points-clés d'une personne avec l'annotation (OKS de COCO, 0…1).
/// `pred`/`truth` : (x, y) en pixels ; `visible` : point annoté ; `area` : aire de la personne.
pub fn oks(pred: &[(f32, f32)], truth: &[(f32, f32)], visible: &[bool], area: f32) -> Option<f32> {
    // Tolérances par point (COCO) : nez, yeux, oreilles, épaules, coudes, poignets, hanches, genoux, chevilles.
    const SIGMAS: [f32; 17] = [0.026, 0.025, 0.025, 0.035, 0.035, 0.079, 0.079, 0.072, 0.072, 0.062, 0.062, 0.107, 0.107, 0.087, 0.087, 0.089, 0.089];
    let (mut sum, mut n) = (0.0, 0);
    for k in 0..17.min(pred.len()).min(truth.len()).min(visible.len()) {
        if !visible[k] {
            continue;
        }
        let (dx, dy) = (pred[k].0 - truth[k].0, pred[k].1 - truth[k].1);
        let var = (2.0 * SIGMAS[k]).powi(2);
        sum += (-(dx * dx + dy * dy) / (2.0 * area.max(1.0) * var)).exp();
        n += 1;
    }
    (n > 0).then(|| sum / n as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(x0: f32, y0: f32, x1: f32, y1: f32) -> BBox {
        BBox::new(x0, y0, x1, y1)
    }

    #[test]
    fn matching_precision_recall_and_ap() {
        let mut s = Scores::default();
        let truths = [Truth { bbox: b(0.1, 0.1, 0.3, 0.5), ignore: false }, Truth { bbox: b(0.6, 0.1, 0.8, 0.5), ignore: false }];
        // Une bonne détection, un doublon (faux), une fausse ailleurs ; la 2e personne est manquée.
        let dets = [(b(0.1, 0.1, 0.3, 0.5), 0.9), (b(0.11, 0.1, 0.31, 0.5), 0.6), (b(0.4, 0.7, 0.5, 0.9), 0.3)];
        let m = s.add_image(&dets, &truths, 0.5);
        assert_eq!(m, vec![Some(0), None, None]);
        assert_eq!(s.at(0.8), (1.0, 0.5));
        assert_eq!(s.at(0.0), (1.0 / 3.0, 0.5));
        // AP : rappel 0,5 atteint avec précision 1, puis plus rien.
        assert!((s.ap() - 51.0 / 101.0).abs() < 1e-4, "{}", s.ap());
        // Image vide avec une détection à 0,7 : fausse alerte au seuil 0,5, pas à 0,8.
        s.add_image(&[(b(0.2, 0.2, 0.4, 0.6), 0.7)], &[], 0.5);
        assert_eq!(s.false_alarm_images(0.5), (1, 1));
        assert_eq!(s.false_alarm_images(0.8), (0, 1));
        let (t, ..) = s.best_f1(&[0.2, 0.5, 0.8]);
        assert_eq!(t, 0.8);
    }

    #[test]
    fn detections_on_crowds_are_neutral() {
        let mut s = Scores::default();
        let truths = [Truth { bbox: b(0.0, 0.0, 1.0, 0.5), ignore: true }];
        s.add_image(&[(b(0.1, 0.1, 0.2, 0.3), 0.9)], &truths, 0.5);
        assert_eq!(s.at(0.0), (1.0, 1.0));
        assert_eq!(s.positives, 0);
    }

    #[test]
    fn face_pairs_thresholds() {
        let p = Pairs { same: vec![0.8, 0.6, 0.3], different: vec![0.1, 0.2, 0.45], missed: 0 };
        assert_eq!(p.rates(0.4), (1.0 / 3.0, 1.0 / 3.0));
        assert_eq!(p.threshold_for_far(0.0), 0.46);
        assert_eq!(p.best_accuracy().1, 5.0 / 6.0);
    }

    #[test]
    fn oks_is_one_for_exact_points() {
        let pts: Vec<(f32, f32)> = (0..17).map(|k| (k as f32 * 10.0, 50.0)).collect();
        let vis = [true; 17];
        assert!((oks(&pts, &pts, &vis, 10_000.0).unwrap() - 1.0).abs() < 1e-6);
        let off: Vec<(f32, f32)> = pts.iter().map(|p| (p.0 + 30.0, p.1)).collect();
        assert!(oks(&off, &pts, &vis, 10_000.0).unwrap() < 0.6);
        assert!(oks(&pts, &pts, &[false; 17], 1.0).is_none());
    }
}
