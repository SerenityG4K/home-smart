//! Suivi des personnes d'une image à l'autre : chaque détection est rattachée à une piste
//! existante (meilleur recouvrement, ou à défaut boîte proche de même taille — l'analyse ne
//! tourne qu'à quelques images par seconde, une personne peut bouger beaucoup entre deux), sinon
//! elle ouvre une nouvelle piste. Une piste n'est « confirmée » qu'après quelques détections sûres
//! (évite les fausses alertes d'une détection isolée) et disparaît après un délai sans détection,
//! plus long si elle est restée immobile (personne assise mal vue, silhouette du décor).

use super::geom::BBox;
use super::Pose;
use std::collections::VecDeque;

/// Durée d'historique gardée par piste (pour les actions : chute, présence prolongée…).
const HISTORY_SECS: f64 = 12.0;

/// Une piste est « immobile » si elle n'a pas bougé (à un quart de sa hauteur près) depuis ce
/// temps (secondes) : elle a alors droit à `Tracker::still_gap`.
const STILL_SECS: f64 = 10.0;
const STILL_TOL: f32 = 0.25;

/// Une détection de personne dans une image.
#[derive(Clone, Debug, PartialEq)]
pub struct Detection {
    pub bbox: BBox,
    pub score: f32,
    /// Squelette, si le modèle de posture l'a fourni.
    pub pose: Option<Pose>,
}

#[derive(Clone, Debug)]
pub struct Track {
    pub id: u32,
    pub bbox: BBox,
    pub score: f32,
    pub pose: Option<Pose>,
    /// Instants (secondes, horloge de l'analyse) de première et dernière détection.
    pub first_seen: f64,
    pub last_seen: f64,
    /// Détections rattachées à la piste.
    pub hits: u32,
    /// Confirmée (assez de détections) : c'est seulement alors qu'on en parle.
    pub confirmed: bool,
    /// Boîtes récentes (instant, boîte), la plus ancienne en tête.
    pub history: VecDeque<(f64, BBox)>,
}

impl Track {
    /// Déplacement moyen du point d'appui, en largeurs d'image par seconde, sur `secs`.
    pub fn speed(&self, secs: f64) -> f32 {
        let Some(&(t1, b1)) = self.history.back() else { return 0.0 };
        let Some(&(t0, b0)) = self.history.iter().find(|(t, _)| *t >= t1 - secs) else { return 0.0 };
        if t1 <= t0 {
            return 0.0;
        }
        b1.foot().dist(b0.foot()) / (t1 - t0) as f32
    }

    /// Immobile depuis au moins `secs` secondes : ni le haut de sa boîte, ni son milieu en largeur
    /// n'ont bougé de plus de `tol` fois sa hauteur (le bas varie trop : jambes vues ou non).
    pub fn still_for(&self, secs: f64, tol: f32) -> bool {
        let Some(&(t1, b1)) = self.history.back() else { return false };
        if t1 - self.first_seen < secs {
            return false;
        }
        let (x, top, h) = (b1.center().x, b1.y0, b1.height().max(1e-3));
        self.history.iter().filter(|(t, _)| *t >= t1 - secs).all(|(_, b)| (b.center().x - x).abs() <= tol * h && (b.y0 - top).abs() <= tol * h)
    }

    /// Boîte telle qu'elle était il y a `secs` secondes (la plus proche).
    pub fn bbox_ago(&self, now: f64, secs: f64) -> Option<BBox> {
        let target = now - secs;
        self.history.iter().min_by(|a, b| (a.0 - target).abs().total_cmp(&(b.0 - target).abs())).map(|(_, b)| *b)
    }
}

/// Ce qui arrive aux pistes à chaque image.
#[derive(Clone, Debug, PartialEq)]
pub enum TrackChange {
    /// Piste confirmée : une personne est là.
    Confirmed(u32),
    /// Piste perdue (plus vue depuis le délai) ; `was_confirmed` : on l'avait annoncée ; `seen` :
    /// instant de sa dernière détection (une piste immobile attend longtemps avant d'être perdue).
    Lost { id: u32, was_confirmed: bool, seen: f64 },
}

#[derive(Clone, Debug)]
pub struct Tracker {
    next_id: u32,
    tracks: Vec<Track>,
    /// Recouvrement minimal pour rattacher une détection à une piste.
    pub iou_min: f32,
    /// Détections nécessaires pour confirmer une piste…
    pub min_hits: u32,
    /// … et durée minimale depuis la première (secondes) : à 30 analyses par seconde, deux
    /// détections ne font que 66 ms, un reflet passager ne doit pas devenir une personne.
    pub min_age: f64,
    /// Délai sans détection avant d'abandonner une piste (secondes).
    pub max_gap: f64,
    /// Score minimal pour ouvrir une piste, et pour la confirmer. En dessous, une détection ne
    /// fait que prolonger une piste confirmée (idée de ByteTrack) : une personne au sol, vue de
    /// biais ou à demi cachée, garde sa piste — et sa posture — au lieu de disparaître juste après
    /// sa chute. Mais une piste pas encore confirmée ne se nourrit que de détections sûres : sinon
    /// un objet du décor vu une seule fois au-dessus du seuil, puis faiblement (constaté sur une
    /// vraie caméra : score 0,25 à 0,5 pendant des minutes), devenait une « personne ».
    pub high: f32,
    /// Délai sans détection pour une piste restée immobile au moins `STILL_SECS` (secondes). Une
    /// personne assise ou couchée sans bouger est souvent mal vue (de dos, sous une couverture,
    /// dans la pénombre), et ne s'en va pas sans bouger d'abord ; une silhouette immobile du décor
    /// (photo, affiche, écran) ne fait plus « arrive puis s'en va » toutes les minutes (constaté
    /// sur une vraie caméra : 80 arrivées et 40 « visage inconnu » dans la journée).
    pub still_gap: f64,
}

impl Default for Tracker {
    fn default() -> Self {
        Tracker { next_id: 1, tracks: Vec::new(), iou_min: 0.25, min_hits: 2, min_age: 0.3, max_gap: 2.5, high: 0.0, still_gap: 60.0 }
    }
}

impl Tracker {
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    pub fn track(&self, id: u32) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    /// Ressemblance piste ↔ détection (0 = aucune) : recouvrement, ou à défaut proximité
    /// (déplacement rapide entre deux analyses, ou changement de forme : une personne qui tombe
    /// passe d'une boîte haute et étroite à une boîte basse et large). Toujours moins bien
    /// qu'un vrai recouvrement ; à proximité égale, la boîte de taille la plus proche l'emporte.
    fn affinity(t: &BBox, d: &BBox, iou_min: f32) -> f32 {
        let iou = t.iou(d);
        if iou >= iou_min {
            return iou;
        }
        let reach = 0.75 * t.width().max(t.height()).max(d.width().max(d.height())).max(1e-3);
        let dist = t.center().dist(d.center());
        if dist >= reach {
            return 0.0;
        }
        let size = t.area().min(d.area()) / t.area().max(d.area()).max(1e-6);
        0.01 + iou_min * 0.5 * (1.0 - dist / reach) * (0.5 + 0.5 * size)
    }

    /// Intègre les détections de l'image prise à l'instant `now` (secondes).
    pub fn update(&mut self, now: f64, dets: &[Detection]) -> Vec<TrackChange> {
        let mut changes = Vec::new();
        // Paires (piste, détection) par ressemblance décroissante, attribution gloutonne : les
        // détections sûres d'abord, puis les faibles pour les pistes restées sans rien.
        let high = self.high;
        let mut pairs: Vec<(bool, f32, usize, usize)> = Vec::new();
        for (ti, t) in self.tracks.iter().enumerate() {
            for (di, d) in dets.iter().enumerate() {
                let strong = d.score >= high;
                if !strong && !t.confirmed {
                    continue;
                }
                // Une détection faible ne prolonge une piste que là où elle était (recouvrement) :
                // sinon un objet douteux du décor « capture » la piste d'une vraie personne passée
                // près de lui (constaté sur une vraie caméra : saut de piste pris pour une chute).
                // Exception : la personne qui tombe, dont la boîte couchée recouvre ses propres pieds.
                let at_feet = t.bbox.intersection(&d.bbox) > 0.0 && d.bbox.grow(0.05).contains(t.bbox.foot());
                // Une piste immobile qu'on laisse patienter ne se reprend que sur place.
                let waiting = now - t.last_seen > self.max_gap;
                let a = if (strong || at_feet) && !waiting { Self::affinity(&t.bbox, &d.bbox, self.iou_min) } else { Some(t.bbox.iou(&d.bbox)).filter(|&i| i >= self.iou_min).unwrap_or(0.0) };
                if a > 0.0 {
                    pairs.push((strong, a, ti, di));
                }
            }
        }
        pairs.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.total_cmp(&a.1)));
        let (mut used_t, mut used_d) = (vec![false; self.tracks.len()], vec![false; dets.len()]);
        for (_, _, ti, di) in pairs {
            if used_t[ti] || used_d[di] {
                continue;
            }
            (used_t[ti], used_d[di]) = (true, true);
            let (t, d) = (&mut self.tracks[ti], &dets[di]);
            (t.bbox, t.score, t.last_seen) = (d.bbox, d.score, now);
            if d.pose.is_some() {
                t.pose = d.pose.clone();
            }
            t.hits += 1;
            t.history.push_back((now, d.bbox));
            while t.history.front().is_some_and(|(at, _)| *at < now - HISTORY_SECS) {
                t.history.pop_front();
            }
            if !t.confirmed && t.hits >= self.min_hits && now - t.first_seen >= self.min_age {
                t.confirmed = true;
                changes.push(TrackChange::Confirmed(t.id));
            }
        }
        // Détections sûres orphelines : nouvelles pistes.
        for (_, d) in dets.iter().enumerate().filter(|(i, d)| !used_d[*i] && d.score >= high) {
            let id = self.next_id;
            self.next_id += 1;
            let confirmed = self.min_hits <= 1;
            self.tracks.push(Track {
                id,
                bbox: d.bbox,
                score: d.score,
                pose: d.pose.clone(),
                first_seen: now,
                last_seen: now,
                hits: 1,
                confirmed,
                history: VecDeque::from([(now, d.bbox)]),
            });
            if confirmed {
                changes.push(TrackChange::Confirmed(id));
            }
        }
        // Pistes plus vues depuis trop longtemps.
        let (max_gap, still_gap) = (self.max_gap, self.still_gap);
        self.tracks.retain(|t| {
            let gap = if t.confirmed && t.still_for(STILL_SECS, STILL_TOL) { max_gap.max(still_gap) } else { max_gap };
            let alive = now - t.last_seen <= gap;
            if !alive {
                changes.push(TrackChange::Lost { id: t.id, was_confirmed: t.confirmed, seen: t.last_seen });
            }
            alive
        });
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det(x: f32, y: f32) -> Detection {
        Detection { bbox: BBox::new(x, y, x + 0.1, y + 0.4), score: 0.9, pose: None }
    }

    #[test]
    fn follows_two_people_and_forgets_them() {
        let mut tr = Tracker::default();
        assert!(tr.update(0.0, &[det(0.1, 0.3), det(0.6, 0.3)]).is_empty());
        // Deuxième image : les deux sont confirmées, avec leurs numéros d'origine.
        let ch = tr.update(0.5, &[det(0.62, 0.3), det(0.12, 0.31)]);
        assert!(ch.len() == 2 && ch.contains(&TrackChange::Confirmed(1)) && ch.contains(&TrackChange::Confirmed(2)), "{ch:?}");
        assert!((tr.track(1).unwrap().bbox.x0 - 0.12).abs() < 1e-6);
        // Déplacement rapide sans recouvrement (analyse lente) : même piste.
        tr.update(1.0, &[det(0.2, 0.3), det(0.62, 0.3)]);
        assert_eq!(tr.tracks().len(), 2);
        assert!(tr.track(1).unwrap().speed(1.0) > 0.05);
        // Plus personne pendant 3 s : pistes perdues, une seule fois chacune.
        let lost = tr.update(4.0, &[]);
        assert_eq!(lost.len(), 2);
        assert!(lost.contains(&TrackChange::Lost { id: 1, was_confirmed: true, seen: 1.0 }));
        assert!(tr.update(4.5, &[]).is_empty());
    }

    #[test]
    fn confirmation_takes_time_at_high_rate() {
        // 30 analyses par seconde : confirmée après 0,3 s, pas dès la deuxième image.
        let mut tr = Tracker::default();
        let mut confirmed_at = None;
        for i in 0..20 {
            let t = i as f64 / 30.0;
            if tr.update(t, &[det(0.4, 0.3)]).contains(&TrackChange::Confirmed(1)) {
                confirmed_at = Some(t);
            }
        }
        let t = confirmed_at.expect("jamais confirmée");
        assert!((0.3..0.4).contains(&t), "confirmée à {t}");
    }

    #[test]
    fn a_weak_detection_elsewhere_never_takes_over_a_track() {
        let mut tr = Tracker { high: 0.5, ..Tracker::default() };
        let person = |x: f32| Detection { bbox: BBox::new(x, 0.1, x + 0.26, 1.0), score: 0.85, pose: None };
        tr.update(0.0, &[person(0.66)]);
        tr.update(0.4, &[person(0.66)]);
        // La personne n'est pas vue une image ; un objet du décor, en bas à gauche, l'est faiblement.
        let ghost = Detection { bbox: BBox::new(0.14, 0.57, 0.45, 1.0), score: 0.25, pose: None };
        tr.update(0.5, &[ghost.clone()]);
        tr.update(0.6, &[person(0.65), ghost]);
        assert_eq!(tr.tracks().len(), 1, "ni capture de la piste, ni piste pour l'objet");
        assert!((tr.track(1).unwrap().bbox.x0 - 0.65).abs() < 1e-6);
    }

    #[test]
    fn weak_detections_only_extend_tracks() {
        let mut tr = Tracker { high: 0.5, ..Tracker::default() };
        let weak = |x: f32, y: f32| Detection { score: 0.3, ..det(x, y) };
        tr.update(0.0, &[det(0.4, 0.3)]);
        tr.update(0.4, &[det(0.4, 0.32)]);
        // La personne tombe : détection faible et boîte couchée, la piste continue.
        let lying = Detection { bbox: BBox::new(0.35, 0.6, 0.65, 0.75), score: 0.3, pose: None };
        tr.update(0.8, &[lying.clone(), weak(0.9, 0.1)]);
        assert_eq!(tr.tracks().len(), 1, "une détection faible isolée n'ouvre pas de piste");
        assert_eq!(tr.track(1).unwrap().bbox, lying.bbox);
        assert_eq!(tr.track(1).unwrap().last_seen, 0.8);
    }

    #[test]
    fn an_object_seen_once_above_the_threshold_is_not_a_person() {
        // Constaté sur une vraie caméra : un objet immobile du décor, vu faiblement en continu
        // (0,25 à 0,5), passe de temps en temps une image au-dessus du seuil.
        let mut tr = Tracker { high: 0.5, ..Tracker::default() };
        let thing = |score: f32| Detection { score, ..det(0.68, 0.48) };
        let mut changes = Vec::new();
        for i in 0..96 {
            let peak = i % 24 == 0;
            changes.extend(tr.update(i as f64 * 0.125, &[thing(if peak { 0.55 } else { 0.3 })]));
        }
        assert!(!changes.iter().any(|c| matches!(c, TrackChange::Confirmed(_))), "{changes:?}");
        // Une vraie personne, même vue tantôt nettement, tantôt faiblement, est confirmée.
        let mut tr = Tracker { high: 0.5, ..Tracker::default() };
        let mut confirmed = None;
        for i in 0..10 {
            let score = if i % 2 == 0 { 0.6 } else { 0.4 };
            if tr.update(i as f64 * 0.125, &[Detection { score, ..det(0.3, 0.3) }]).contains(&TrackChange::Confirmed(1)) {
                confirmed = Some(i);
            }
        }
        assert_eq!(confirmed, Some(4), "deux détections sûres, 0,3 s d'écart au moins");
        // À une analyse toutes les deux secondes, la piste en attente tient jusqu'à la suivante.
        let mut tr = Tracker { high: 0.5, ..Tracker::default() };
        tr.update(0.0, &[det(0.3, 0.3)]);
        assert_eq!(tr.update(2.0, &[det(0.31, 0.3)]), vec![TrackChange::Confirmed(1)]);
    }

    #[test]
    fn someone_still_is_not_lost_at_the_first_gap() {
        let mut tr = Tracker { high: 0.5, ..Tracker::default() };
        let mut changes = Vec::new();
        // Immobile 12 s (le bas de la boîte varie : jambes vues ou non), puis plus vu 40 s : la
        // piste attend ; revue au même endroit, c'est la même.
        for i in 0..24 {
            let d = Detection { bbox: BBox::new(0.68, 0.48, 0.75, if i % 3 == 0 { 0.8 } else { 0.67 }), score: 0.6, pose: None };
            changes.extend(tr.update(i as f64 * 0.5, &[d]));
        }
        for i in 0..80 {
            changes.extend(tr.update(12.0 + i as f64 * 0.5, &[]));
        }
        changes.extend(tr.update(52.5, &[Detection { bbox: BBox::new(0.68, 0.48, 0.75, 0.67), score: 0.55, pose: None }]));
        assert_eq!(changes, vec![TrackChange::Confirmed(1)]);
        // Plus vue du tout : perdue après le délai des pistes immobiles.
        let lost: Vec<TrackChange> = (0..130).flat_map(|i| tr.update(53.0 + i as f64 * 0.5, &[])).collect();
        assert_eq!(lost, vec![TrackChange::Lost { id: 1, was_confirmed: true, seen: 52.5 }]);
        // Quelqu'un qui marche, puis n'est plus vu : perdu au délai habituel.
        let mut tr = Tracker::default();
        for i in 0..30 {
            tr.update(i as f64 * 0.5, &[det(0.05 + i as f32 * 0.02, 0.3)]);
        }
        assert!(tr.update(17.6, &[]).contains(&TrackChange::Lost { id: 1, was_confirmed: true, seen: 14.5 }));
    }

    #[test]
    fn single_false_detection_is_never_announced() {
        let mut tr = Tracker::default();
        tr.update(0.0, &[det(0.5, 0.5)]);
        let ch = tr.update(3.0, &[]);
        assert_eq!(ch, vec![TrackChange::Lost { id: 1, was_confirmed: false, seen: 0.0 }]);
    }
}
