//! Surveillance de la qualité de l'image d'une caméra : détecte quand elle ne montre plus sa
//! scène habituelle (objectif masqué, image floutée, caméra détournée).
//!
//! Aucun modèle d'IA : quelques mesures sur une image réduite, environ une fois par seconde,
//! comparées à ce que la caméra voit d'ordinaire (appris au démarrage, puis adapté lentement) :
//! - **objectif masqué** : image presque uniforme et sans détails (champ sombre ou bouché) ;
//! - **image floutée** : même scène, mais beaucoup moins de détails fins qu'à l'habitude ;
//! - **caméra détournée** : presque toute l'image a changé, mesurée bloc par bloc sur la
//!   luminosité et sur les contours — une personne qui passe ou une lampe allumée ne suffit pas.
//!
//! Chaque état doit durer un moment (selon la sensibilité) avant d'être signalé, et l'alerte se
//! clôt quand l'image redevient normale quelques secondes. Une orientation commandée (`moved`) et
//! le passage en noir et blanc (vision de nuit) font réapprendre la vue de référence.

use crate::camera::{Frame, TamperSensitivity};

/// Grille de la vue de référence (luminosité moyenne et densité de contours par case).
const GW: usize = 32;
const GH: usize = 24;
/// Blocs comparés pour juger une caméra détournée.
const BX: usize = 4;
const BY: usize = 3;
/// Apprentissage de la vue de référence au démarrage (s).
const LEARN: f64 = 4.0;
/// L'image doit rester normale ce temps-là pour clore une alerte (s).
const CLEAR_AFTER: f64 = 5.0;
/// Après une orientation commandée : images ignorées ce temps-là, puis réapprentissage (s).
const MOVE_SETTLE: f64 = 4.0;
/// Une vue changée qui dure aussi longtemps devient la nouvelle référence (s).
const ADOPT_AFTER: f64 = 600.0;

/// Ce qui ne va pas avec l'image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TamperKind {
    /// Objectif masqué : champ uniforme, aucun détail.
    Covered,
    /// Image floutée : la scène est là mais les détails fins ont disparu.
    Blurred,
    /// Caméra détournée : la vue n'a plus rien à voir avec l'habituelle.
    Redirected,
}

impl TamperKind {
    pub fn label(self) -> &'static str {
        match self {
            TamperKind::Covered => "objectif masqué (champ uniforme, aucun détail)",
            TamperKind::Blurred => "image floutée (objectif sali ou déréglé)",
            TamperKind::Redirected => "caméra détournée (la vue a changé)",
        }
    }
}

/// Signaux mesurés sur une image (indépendants de la résolution).
#[derive(Clone, Debug, PartialEq)]
struct Signature {
    /// Luminosité moyenne de chaque case (0…1).
    luma: Vec<f32>,
    /// Densité de contours de chaque case (0…1) : moyenne des écarts entre voisins.
    edges: Vec<f32>,
    /// Détail fin global (0…1) : part de contraste local sur toute l'image (netteté).
    detail: f32,
    /// Contraste global (écart-type de la luminosité, 0…1).
    spread: f32,
}

impl Signature {
    /// Mesure une image RGBA.
    fn of(frame: &Frame) -> Signature {
        let (w, h) = (frame.width, frame.height);
        let mut luma = vec![0f32; GW * GH];
        let mut count = vec![0f32; GW * GH];
        let gray: Vec<f32> = frame.rgba.chunks_exact(4).map(|p| (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32) / 255.0).collect();
        if gray.len() < w * h || w < 2 || h < 2 {
            return Signature { luma: vec![0.0; GW * GH], edges: vec![0.0; GW * GH], detail: 0.0, spread: 0.0 };
        }
        // Luminosité moyenne par case.
        for y in 0..h {
            let gy = y * GH / h;
            for x in 0..w {
                let gx = x * GW / w;
                let c = gy * GW + gx;
                luma[c] += gray[y * w + x];
                count[c] += 1.0;
            }
        }
        for (l, n) in luma.iter_mut().zip(&count) {
            *l /= n.max(1.0);
        }
        // Contours : écart au voisin de droite et du bas, accumulé par case ; et détail global.
        let mut edges = vec![0f32; GW * GH];
        let mut detail_sum = 0f64;
        let mut detail_n = 0f64;
        for y in 0..h - 1 {
            let gy = y * GH / h;
            for x in 0..w - 1 {
                let i = y * w + x;
                let d = (gray[i] - gray[i + 1]).abs() + (gray[i] - gray[i + w]).abs();
                edges[gy * GW + x * GW / w] += d;
                detail_sum += d as f64;
                detail_n += 1.0;
            }
        }
        for (e, n) in edges.iter_mut().zip(&count) {
            *e = (*e / n.max(1.0)).min(1.0);
        }
        let mean = luma.iter().sum::<f32>() / luma.len() as f32;
        let var = luma.iter().map(|l| (l - mean).powi(2)).sum::<f32>() / luma.len() as f32;
        Signature { luma, edges, detail: (detail_sum / detail_n.max(1.0)) as f32, spread: var.sqrt() }
    }

    /// Mélange lent vers une nouvelle image (adaptation de la référence aux changements de lumière).
    fn blend(&mut self, other: &Signature, a: f32) {
        for (s, o) in self.luma.iter_mut().zip(&other.luma) {
            *s += (o - *s) * a;
        }
        for (s, o) in self.edges.iter_mut().zip(&other.edges) {
            *s += (o - *s) * a;
        }
        self.detail += (other.detail - self.detail) * a;
        self.spread += (other.spread - self.spread) * a;
    }

    /// Part des blocs (BX×BY) dont la luminosité ou les contours ont nettement changé (0…1).
    fn changed_fraction(&self, other: &Signature) -> f32 {
        let mut changed = 0;
        for by in 0..BY {
            for bx in 0..BX {
                let (mut dl, mut de, mut n) = (0f32, 0f32, 0f32);
                for gy in by * GH / BY..(by + 1) * GH / BY {
                    for gx in bx * GW / BX..(bx + 1) * GW / BX {
                        let c = gy * GW + gx;
                        dl += (self.luma[c] - other.luma[c]).abs();
                        de += (self.edges[c] - other.edges[c]).abs();
                        n += 1.0;
                    }
                }
                n = n.max(1.0);
                if dl / n > 0.18 || de / n > 0.12 {
                    changed += 1;
                }
            }
        }
        changed as f32 / (BX * BY) as f32
    }
}

/// Seuils d'une sensibilité.
struct Thresholds {
    /// Secondes qu'un état doit durer avant d'être signalé.
    hold: f64,
    /// En dessous : image jugée uniforme (objectif masqué).
    detail_floor: f32,
    spread_floor: f32,
    /// Chute du détail sous cette part de l'habitude, scène par ailleurs inchangée : floutée.
    blur_ratio: f32,
    /// Part de blocs changés au-delà de laquelle la caméra est jugée détournée.
    redirect_fraction: f32,
}

fn thresholds(s: TamperSensitivity) -> Thresholds {
    match s {
        TamperSensitivity::Low => Thresholds { hold: 12.0, detail_floor: 0.006, spread_floor: 0.015, blur_ratio: 0.30, redirect_fraction: 0.92 },
        TamperSensitivity::Medium => Thresholds { hold: 7.0, detail_floor: 0.010, spread_floor: 0.025, blur_ratio: 0.40, redirect_fraction: 0.80 },
        TamperSensitivity::High => Thresholds { hold: 4.0, detail_floor: 0.016, spread_floor: 0.035, blur_ratio: 0.50, redirect_fraction: 0.66 },
    }
}

/// Ce que le détecteur signale à l'appelant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TamperEvent {
    /// Un sabotage dure depuis assez longtemps : à signaler.
    Started(TamperKind),
    /// L'image est redevenue normale : l'alerte peut être close.
    Cleared,
}

/// Détecteur de sabotage par l'image, pour une caméra.
pub struct TamperDetector {
    sens: TamperSensitivity,
    reference: Option<Signature>,
    /// Instant de départ de l'apprentissage, ou `None` une fois appris.
    learning_since: Option<f64>,
    /// Images ignorées jusqu'à cet instant (après une orientation commandée).
    settle_until: f64,
    /// Depuis quand l'anomalie courante dure (et laquelle), avant le seuil `hold`.
    pending: Option<(TamperKind, f64)>,
    /// Sabotage déjà signalé en cours.
    active: Option<TamperKind>,
    /// Depuis quand l'image est redevenue normale (pour clore), ou pour adopter une nouvelle vue.
    normal_since: Option<f64>,
}

impl TamperDetector {
    pub fn new(sens: TamperSensitivity) -> TamperDetector {
        TamperDetector { sens, reference: None, learning_since: None, settle_until: 0.0, pending: None, active: None, normal_since: None }
    }

    pub fn active(&self) -> Option<TamperKind> {
        self.active
    }

    /// Réapprend la vue de référence (après une orientation ou un changement de mode).
    pub fn moved(&mut self, now: f64) {
        self.reference = None;
        self.learning_since = None;
        self.settle_until = now + MOVE_SETTLE;
        self.pending = None;
        self.normal_since = None;
    }

    /// Juge une image. Renvoie un événement seulement quand l'état franchit un seuil.
    pub fn update(&mut self, now: f64, frame: &Frame) -> Option<TamperEvent> {
        if now < self.settle_until {
            return None;
        }
        let sig = Signature::of(frame);
        let t = thresholds(self.sens);

        // Apprentissage de la référence au démarrage (moyenne des premières images).
        let reference = match &mut self.reference {
            None => {
                self.reference = Some(sig);
                self.learning_since = Some(now);
                return None;
            }
            Some(r) => {
                if let Some(since) = self.learning_since {
                    r.blend(&sig, 0.5);
                    if now - since < LEARN {
                        return None;
                    }
                    self.learning_since = None;
                }
                r
            }
        };

        // Diagnostic de l'image courante.
        let uniform = sig.detail < t.detail_floor && sig.spread < t.spread_floor;
        let changed = reference.changed_fraction(&sig);
        let blurred = !uniform && reference.detail > 0.0 && sig.detail < reference.detail * t.blur_ratio && changed < t.redirect_fraction;
        let now_kind = if uniform {
            Some(TamperKind::Covered)
        } else if changed >= t.redirect_fraction {
            Some(TamperKind::Redirected)
        } else if blurred {
            Some(TamperKind::Blurred)
        } else {
            None
        };

        match now_kind {
            Some(kind) => {
                self.normal_since = None;
                // Image normale : la référence suit lentement les changements de lumière.
                match self.pending {
                    Some((k, _)) if k == kind => {}
                    _ => self.pending = Some((kind, now)),
                }
                let held = self.pending.map(|(_, since)| now - since).unwrap_or(0.0);
                if self.active != Some(kind) && held >= t.hold {
                    self.active = Some(kind);
                    return Some(TamperEvent::Started(kind));
                }
                None
            }
            None => {
                self.pending = None;
                reference.blend(&sig, 0.02);
                let since = *self.normal_since.get_or_insert(now);
                if self.active.is_some() && now - since >= CLEAR_AFTER {
                    self.active = None;
                    return Some(TamperEvent::Cleared);
                }
                // Scène changée durablement mais sans alerte en cours : nouvelle référence.
                if self.active.is_none() && now - since >= ADOPT_AFTER {
                    *reference = sig;
                    self.normal_since = Some(now);
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: usize, h: usize, f: impl Fn(usize, usize) -> u8) -> Frame {
        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let v = f(x, y);
                let i = (y * w + x) * 4;
                rgba[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        Frame { seq: 0, width: w, height: h, rgba }
    }

    /// Scène « normale » : damier fin et contrasté (beaucoup de détail).
    fn scene(w: usize, h: usize, phase: usize) -> Frame {
        frame(w, h, |x, y| if ((x / 4 + y / 4 + phase) % 2) == 0 { 40 } else { 210 })
    }

    fn feed(d: &mut TamperDetector, mut t: f64, step: f64, n: usize, f: impl Fn() -> Frame) -> Vec<TamperEvent> {
        let mut out = Vec::new();
        for _ in 0..n {
            if let Some(e) = d.update(t, &f()) {
                out.push(e);
            }
            t += step;
        }
        out
    }

    #[test]
    fn learns_then_detects_cover_and_recovers() {
        let mut d = TamperDetector::new(TamperSensitivity::Medium);
        // Apprentissage sur la scène normale : aucun événement.
        assert!(feed(&mut d, 0.0, 1.0, 6, || scene(128, 96, 0)).is_empty());
        assert!(d.active().is_none());
        // Objectif masqué (champ gris uniforme) : signalé après le délai.
        let ev = feed(&mut d, 6.0, 1.0, 10, || frame(128, 96, |_, _| 15));
        assert_eq!(ev, vec![TamperEvent::Started(TamperKind::Covered)]);
        // Retour à la normale : alerte close.
        let ev = feed(&mut d, 20.0, 1.0, 8, || scene(128, 96, 0));
        assert_eq!(ev, vec![TamperEvent::Cleared]);
        assert!(d.active().is_none());
    }

    #[test]
    fn detects_redirect_but_not_a_passerby() {
        let mut d = TamperDetector::new(TamperSensitivity::Medium);
        feed(&mut d, 0.0, 1.0, 6, || scene(128, 96, 0));
        // Une bande sombre sur le tiers gauche (quelqu'un qui passe) : pas d'alerte.
        let passer = || frame(128, 96, |x, y| if x < 40 { 30 } else if ((x / 4 + y / 4) % 2) == 0 { 40 } else { 210 });
        assert!(feed(&mut d, 6.0, 1.0, 10, passer).is_empty());
        // Scène entièrement différente (caméra tournée) : signalée.
        let ev = feed(&mut d, 30.0, 1.0, 10, || frame(128, 96, |x, y| if ((x / 7 + y / 3) % 2) == 0 { 90 } else { 160 }));
        assert_eq!(ev.first(), Some(&TamperEvent::Started(TamperKind::Redirected)));
    }

    #[test]
    fn move_resets_reference() {
        let mut d = TamperDetector::new(TamperSensitivity::High);
        feed(&mut d, 0.0, 1.0, 6, || scene(128, 96, 0));
        // Orientation commandée : la nouvelle scène devient la référence, pas une alerte.
        d.moved(6.0);
        let ev = feed(&mut d, 11.0, 1.0, 12, || scene(128, 96, 1));
        assert!(ev.is_empty(), "{ev:?}");
    }
}
