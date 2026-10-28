//! Animaux et véhicules : les sortes d'objets suivies (parmi les 80 du jeu COCO), et leur suivi
//! d'une analyse à l'autre — une piste par sorte (un chat qui bouge ne devient pas un chien).

use super::geom::BBox;
use super::track::{Detection, TrackChange, Tracker};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ObjectKind {
    Cat,
    Dog,
    Bird,
    Horse,
    Sheep,
    Cow,
    /// Éléphant, ours, zèbre, girafe : rares à la maison, mais à ne pas manquer.
    WildAnimal,
    Bicycle,
    Car,
    Motorcycle,
    Bus,
    Truck,
}

impl ObjectKind {
    /// Sorte d'après l'indice de classe COCO (0 = personne, ignorée ici : le modèle des personnes
    /// s'en charge, avec les squelettes).
    pub fn from_coco(class: usize) -> Option<ObjectKind> {
        Some(match class {
            1 => ObjectKind::Bicycle,
            2 => ObjectKind::Car,
            3 => ObjectKind::Motorcycle,
            5 => ObjectKind::Bus,
            7 => ObjectKind::Truck,
            14 => ObjectKind::Bird,
            15 => ObjectKind::Cat,
            16 => ObjectKind::Dog,
            17 => ObjectKind::Horse,
            18 => ObjectKind::Sheep,
            19 => ObjectKind::Cow,
            20..=23 => ObjectKind::WildAnimal,
            _ => return None,
        })
    }

    /// Score minimal, par sorte : mesuré sur COCO val2017 (meilleur compromis précision / rappel,
    /// `homesmart-cli vision eval coco`). Le suivi exige en plus deux détections de suite.
    pub fn threshold(self) -> f32 {
        match self {
            ObjectKind::Cat | ObjectKind::Horse | ObjectKind::Sheep | ObjectKind::Cow => 0.4,
            ObjectKind::Dog | ObjectKind::Car => 0.35,
            ObjectKind::Bird | ObjectKind::Bicycle | ObjectKind::Motorcycle | ObjectKind::Truck => 0.3,
            ObjectKind::WildAnimal => 0.45,
            ObjectKind::Bus => 0.5,
        }
    }

    pub fn is_animal(self) -> bool {
        !matches!(self, ObjectKind::Bicycle | ObjectKind::Car | ObjectKind::Motorcycle | ObjectKind::Bus | ObjectKind::Truck)
    }

    /// Nom en français, sans article (« chat »).
    pub fn label(self) -> &'static str {
        match self {
            ObjectKind::Cat => "chat",
            ObjectKind::Dog => "chien",
            ObjectKind::Bird => "oiseau",
            ObjectKind::Horse => "cheval",
            ObjectKind::Sheep => "mouton",
            ObjectKind::Cow => "vache",
            ObjectKind::WildAnimal => "animal sauvage",
            ObjectKind::Bicycle => "vélo",
            ObjectKind::Car => "voiture",
            ObjectKind::Motorcycle => "moto",
            ObjectKind::Bus => "bus",
            ObjectKind::Truck => "camion",
        }
    }

    /// Avec l'article indéfini (« un chat », « une voiture »).
    pub fn with_article(self) -> String {
        let feminine = matches!(self, ObjectKind::Cow | ObjectKind::Car | ObjectKind::Motorcycle);
        format!("{} {}", if feminine { "une" } else { "un" }, self.label())
    }
}

/// Un objet détecté dans une image.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectDetection {
    pub kind: ObjectKind,
    pub bbox: BBox,
    pub score: f32,
}

/// Animaux et véhicules retenus parmi les détections brutes du modèle (indice de classe COCO,
/// boîte, score) : chaque sorte à son seuil ; un même animal vu à la fois chat et chien (cases
/// voisines), ou un même véhicule vu à la fois voiture et camion, ne compte qu'une fois, sous la
/// sorte la plus sûre.
pub fn from_raw(raw: &[(usize, BBox, f32)]) -> Vec<ObjectDetection> {
    let mut found: Vec<ObjectDetection> = raw
        .iter()
        .filter_map(|&(class, bbox, score)| ObjectKind::from_coco(class).map(|kind| ObjectDetection { kind, bbox, score }))
        .filter(|d| d.score >= d.kind.threshold())
        .collect();
    found.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<ObjectDetection> = Vec::new();
    for d in found {
        if !kept.iter().any(|k| k.kind.is_animal() == d.kind.is_animal() && k.bbox.iou(&d.bbox) >= 0.7) {
            kept.push(d);
        }
    }
    kept
}

/// Un objet suivi, pour l'affichage.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectView {
    pub kind: ObjectKind,
    pub track: u32,
    pub bbox: BBox,
    pub score: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ObjectChange {
    Appeared,
    Left { stayed: f64 },
}

/// Suivi des objets, une piste par sorte.
#[derive(Default)]
pub struct ObjectTracker {
    trackers: HashMap<ObjectKind, Tracker>,
    since: HashMap<(ObjectKind, u32), f64>,
}

impl ObjectTracker {
    /// Intègre les objets d'une analyse ; renvoie les apparitions et départs confirmés.
    pub fn update(&mut self, now: f64, dets: &[ObjectDetection]) -> Vec<(ObjectKind, u32, ObjectChange)> {
        let mut out = Vec::new();
        let mut kinds: Vec<ObjectKind> = self.trackers.keys().copied().chain(dets.iter().map(|d| d.kind)).collect();
        kinds.sort();
        kinds.dedup();
        for kind in kinds {
            let mine: Vec<Detection> = dets.iter().filter(|d| d.kind == kind).map(|d| Detection { bbox: d.bbox, score: d.score, pose: None }).collect();
            // Les animaux et véhicules sont analysés moins souvent : on les garde un peu plus longtemps.
            let tr = self.trackers.entry(kind).or_insert_with(|| {
                let mut t = Tracker::default();
                (t.max_gap, t.still_gap) = (4.0, 4.0);
                t
            });
            for change in tr.update(now, &mine) {
                match change {
                    TrackChange::Confirmed(id) => {
                        let first = tr.track(id).map_or(now, |t| t.first_seen);
                        self.since.insert((kind, id), first);
                        out.push((kind, id, ObjectChange::Appeared));
                    }
                    TrackChange::Lost { id, was_confirmed, seen } => {
                        let first = self.since.remove(&(kind, id));
                        if let (true, Some(f)) = (was_confirmed, first) {
                            out.push((kind, id, ObjectChange::Left { stayed: (seen - f).max(0.0) }));
                        }
                    }
                }
            }
        }
        out
    }

    /// Objets confirmés vus à la dernière analyse.
    pub fn views(&self, now: f64) -> Vec<ObjectView> {
        let mut v: Vec<ObjectView> = self
            .trackers
            .iter()
            .flat_map(|(kind, tr)| {
                tr.tracks().iter().filter(|t| t.confirmed && now - t.last_seen < 1.5).map(|t| ObjectView { kind: *kind, track: t.id, bbox: t.bbox, score: t.score })
            })
            .collect();
        v.sort_by_key(|o| (o.kind, o.track));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coco_classes_and_names() {
        assert_eq!(ObjectKind::from_coco(15), Some(ObjectKind::Cat));
        assert_eq!(ObjectKind::from_coco(0), None, "les personnes passent par l'autre modèle");
        assert_eq!(ObjectKind::from_coco(62), None, "pas les meubles");
        assert!(ObjectKind::Dog.is_animal() && !ObjectKind::Car.is_animal());
        assert_eq!(ObjectKind::Car.with_article(), "une voiture");
        assert_eq!(ObjectKind::Cat.with_article(), "un chat");
    }

    #[test]
    fn one_thing_seen_as_two_kinds_counts_once() {
        let b = BBox::new(0.2, 0.3, 0.6, 0.7);
        // Classes COCO : 2 voiture, 7 camion, 15 chat, 16 chien.
        let found = from_raw(&[(7, b, 0.5), (2, BBox::new(0.21, 0.3, 0.6, 0.71), 0.8), (15, b, 0.6), (16, BBox::new(0.2, 0.31, 0.61, 0.7), 0.45)]);
        let kinds: Vec<ObjectKind> = found.iter().map(|d| d.kind).collect();
        assert_eq!(kinds, vec![ObjectKind::Car, ObjectKind::Cat], "la sorte la plus sûre de chaque famille");
        // Deux voitures côte à côte restent deux.
        assert_eq!(from_raw(&[(2, BBox::new(0.0, 0.3, 0.3, 0.6), 0.9), (2, BBox::new(0.35, 0.3, 0.65, 0.6), 0.8)]).len(), 2);
    }

    #[test]
    fn a_cat_and_a_dog_are_tracked_apart() {
        let cat = |x: f32| ObjectDetection { kind: ObjectKind::Cat, bbox: BBox::new(x, 0.7, x + 0.1, 0.8), score: 0.8 };
        let dog = ObjectDetection { kind: ObjectKind::Dog, bbox: BBox::new(0.12, 0.7, 0.24, 0.82), score: 0.7 };
        let mut tr = ObjectTracker::default();
        assert!(tr.update(0.0, &[cat(0.1), dog.clone()]).is_empty());
        let mut seen = tr.update(0.5, &[cat(0.12), dog.clone()]);
        seen.sort_by_key(|(k, ..)| *k);
        assert_eq!(seen, vec![(ObjectKind::Cat, 1, ObjectChange::Appeared), (ObjectKind::Dog, 1, ObjectChange::Appeared)]);
        assert_eq!(tr.views(0.5).len(), 2);
        // Le chat part ; le chien reste.
        tr.update(2.0, &[dog.clone()]);
        let gone = tr.update(5.0, &[dog]);
        assert!(matches!(&gone[..], [(ObjectKind::Cat, 1, ObjectChange::Left { stayed })] if (*stayed - 0.5).abs() < 1e-9), "{gone:?}");
    }
}
