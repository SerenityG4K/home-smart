//! Vision par IA, **entièrement en local** : détection de personnes, posture, actions (arrivée,
//! zones, présence prolongée, chute) et reconnaissance des visages des habitants.
//!
//! Organisation (du plus simple au plus spécialisé) :
//! - `geom` : boîtes, points, zones, en coordonnées normalisées (indépendantes de la résolution) ;
//! - `prep` : préparation des images pour les modèles (mise à l'échelle, redressement des visages) ;
//! - `track` : suivi des personnes d'une image à l'autre ;
//! - `actions` : règles qui transforment les pistes en événements (chute, zones…) ;
//! - `faces` : galerie des visages connus et comparaison des empreintes.
//!
//! Aucune image ne quitte le PC ; les empreintes de visage (données biométriques) restent dans
//! le dossier de HomeSmart et ne sont créées que sur demande explicite (enrôlement).

pub mod actions;
pub mod autotrack;
pub mod engine;
pub mod eval;
pub mod faces;
pub mod geom;
pub mod models;
pub mod objects;
pub mod onnx;
pub mod picture;
pub mod pipeline;
pub mod presence;
pub mod prep;
pub mod tamper;
pub mod track;
pub mod violence;

use actions::Zone;
use geom::Point;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Réglages de la vision (enregistrés dans la configuration).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VisionSettings {
    /// Où faire tourner les modèles (carte graphique si possible, ou processeur).
    #[serde(default)]
    pub accel: engine::Accel,
    /// Réglages par caméra (identifiant de caméra → réglages).
    #[serde(default)]
    pub cameras: BTreeMap<String, CameraVision>,
    /// Mode absence : toute personne vue par une caméra analysée est une alerte (avec photo).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub armed: bool,
    /// Modèle des personnes : standard (RTMO-s) ou précision élevée (RTMO-m).
    #[serde(default, skip_serializing_if = "PersonModel::is_standard")]
    pub person_model: PersonModel,
}

/// Taille du modèle des personnes et squelettes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PersonModel {
    /// RTMO-s : rapide, partout (34 ms par image sur un processeur récent).
    #[default]
    Standard,
    /// RTMO-m : plus juste, surtout de nuit (fausses personnes −29 %, chutes de nuit 72 % au lieu
    /// de 68 % avec 4 % de fausses alertes au lieu de 11 %), deux fois plus de calcul.
    High,
}

impl PersonModel {
    pub fn is_standard(&self) -> bool {
        *self == PersonModel::Standard
    }
}

impl VisionSettings {
    pub fn is_empty(&self) -> bool {
        self.cameras.is_empty() && self.accel == engine::Accel::Auto && !self.armed && self.person_model.is_standard()
    }

    pub fn camera(&self, id: &str) -> CameraVision {
        self.cameras.get(id).cloned().unwrap_or_default()
    }
}

fn yes() -> bool {
    true
}

/// 8 analyses par seconde : sur 150 vidéos réelles, 74 % des chutes trouvées (60 % à 4, 75 % à
/// 15 pour plus de fausses alertes). Plus lent sur un petit processeur, l'analyse s'y adapte
/// d'elle-même (elle ne prend que les images qu'elle a le temps de traiter).
fn default_fps() -> f32 {
    8.0
}

/// Analyse d'une caméra.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CameraVision {
    #[serde(default)]
    pub enabled: bool,
    /// Reconnaissance des visages connus (sinon : personnes et actions seulement).
    #[serde(default = "yes")]
    pub faces: bool,
    /// Détection de chute.
    #[serde(default = "yes")]
    pub falls: bool,
    /// Images analysées par seconde.
    #[serde(default = "default_fps")]
    pub fps: f32,
    /// Analyser le flux HD (visages plus lointains, plus de calcul) plutôt que le flux fluide.
    #[serde(default = "yes")]
    pub hd: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub zones: Vec<Zone>,
    /// Animaux (chat, chien, oiseau…).
    #[serde(default = "yes")]
    pub animals: bool,
    /// Véhicules (voiture, vélo, moto, camion, bus) : utile dehors.
    #[serde(default)]
    pub vehicles: bool,
    /// Gestes : main levée, deux bras levés (appel à l'aide).
    #[serde(default = "yes")]
    pub gestures: bool,
    /// Alerte si une personne reste immobile plus de N minutes (0 = jamais).
    #[serde(default)]
    pub still_minutes: u32,
    /// Une photo (PNG, sur ce PC) à chaque événement marquant.
    #[serde(default)]
    pub snapshots: bool,
    /// Écoute du micro : sons de la maison (alarme, verre brisé, pleurs, aboiements…).
    #[serde(default)]
    pub sounds: bool,
    /// Écoute du micro : parole transcrite en texte (journal), alerte sur un appel à l'aide.
    #[serde(default)]
    pub speech: bool,
    /// Violence : coups répétés entre deux personnes (alerte critique). 8 analyses/s au moins.
    #[serde(default)]
    pub violence: bool,
    /// Caméra motorisée : tourner pour garder la personne au centre.
    #[serde(default)]
    pub follow: bool,
    /// Suivre aussi en hauteur (certaines caméras, comme la Ranger 2C, ignorent l'inclinaison).
    #[serde(default)]
    pub follow_tilt: bool,
}

impl Default for CameraVision {
    fn default() -> Self {
        CameraVision {
            enabled: false,
            faces: true,
            falls: true,
            fps: default_fps(),
            hd: true,
            zones: Vec::new(),
            animals: true,
            vehicles: false,
            gestures: true,
            still_minutes: 0,
            snapshots: false,
            sounds: false,
            speech: false,
            violence: false,
            follow: false,
            follow_tilt: false,
        }
    }
}

/// Un point du squelette : position (normalisée) et confiance du modèle.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Keypoint {
    pub at: Point,
    pub score: f32,
}

/// Squelette à 17 points, dans l'ordre COCO : nez, yeux (g, d), oreilles (g, d), épaules,
/// coudes, poignets, hanches, genoux, chevilles (chaque fois gauche puis droite).
#[derive(Clone, Debug, PartialEq)]
pub struct Pose {
    pub points: [Keypoint; 17],
}

impl Pose {
    pub const NOSE: usize = 0;
    pub const L_SHOULDER: usize = 5;
    pub const R_SHOULDER: usize = 6;
    pub const L_HIP: usize = 11;
    pub const R_HIP: usize = 12;
    pub const L_ANKLE: usize = 15;
    pub const R_ANKLE: usize = 16;

    /// Milieu de deux points s'ils sont assez sûrs.
    fn mid(&self, a: usize, b: usize, min: f32) -> Option<Point> {
        let (p, q) = (self.points[a], self.points[b]);
        (p.score >= min && q.score >= min).then(|| Point::new((p.at.x + q.at.x) / 2.0, (p.at.y + q.at.y) / 2.0))
    }

    /// Milieu des hanches, si le modèle les voit.
    pub fn hips(&self) -> Option<Point> {
        self.mid(Self::L_HIP, Self::R_HIP, 0.3)
    }

    /// Inclinaison du buste (milieu des épaules → milieu des hanches) par rapport à la verticale,
    /// en degrés, mesurée en pixels (`frame_w` × `frame_h`). 0 = debout, 90 = allongé.
    pub fn torso_angle(&self, frame_w: f32, frame_h: f32) -> Option<f32> {
        let s = self.mid(Self::L_SHOULDER, Self::R_SHOULDER, 0.3)?;
        let h = self.mid(Self::L_HIP, Self::R_HIP, 0.3)?;
        let (dx, dy) = ((h.x - s.x) * frame_w, (h.y - s.y) * frame_h);
        if dx.abs() + dy.abs() < 1.0 {
            return None;
        }
        Some(dx.abs().atan2(dy.abs()).to_degrees())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn pose(shoulders: (f32, f32), hips: (f32, f32)) -> Pose {
        let mut points = [Keypoint::default(); 17];
        for (i, (x, y)) in [(Pose::L_SHOULDER, shoulders), (Pose::R_SHOULDER, shoulders), (Pose::L_HIP, hips), (Pose::R_HIP, hips)] {
            points[i] = Keypoint { at: Point::new(x, y), score: 0.9 };
        }
        Pose { points }
    }

    #[test]
    fn torso_angle_standing_and_lying() {
        assert!(pose((0.5, 0.3), (0.5, 0.6)).torso_angle(640.0, 480.0).unwrap() < 1.0);
        assert!(pose((0.3, 0.8), (0.6, 0.82)).torso_angle(640.0, 480.0).unwrap() > 80.0);
        // Points peu sûrs : pas d'avis.
        let mut p = pose((0.5, 0.3), (0.5, 0.6));
        p.points[Pose::L_HIP].score = 0.1;
        assert_eq!(p.torso_angle(640.0, 480.0), None);
    }
}
