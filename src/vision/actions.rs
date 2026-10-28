//! Actions : règles qui transforment les pistes de personnes en événements compréhensibles.
//!
//! - **Arrivée / départ** d'une personne (piste confirmée, puis perdue) ;
//! - **Zones** dessinées sur l'image : entrée, sortie, et **présence prolongée** au-delà d'un
//!   délai propre à chaque zone (le point d'appui au sol de la personne fait foi) ;
//! - **Chute** : passage *brusque* de debout à allongé (posture du squelette si le modèle la
//!   donne, sinon forme de la boîte), avec la tête qui descend nettement ; puis **personne au
//!   sol** si elle reste allongée et immobile, et **relevée** quand elle se redresse. S'allonger
//!   lentement (canapé, lit) n'est pas une chute, ni une piste qui passe à une autre silhouette ;
//! - **Gestes** (squelette) : une main levée tenue un instant, ou les deux bras levés tenus
//!   deux secondes (signal d'appel à l'aide), chez une personne qui n'est pas couchée ;
//! - **Immobilité** : une personne qui ne bouge plus depuis le délai choisi.
//!
//! Tout est mesuré en secondes sur l'horloge de l'analyse, qui ne tourne qu'à quelques images par
//! seconde : les seuils sont choisis pour ce rythme.

use super::geom::{BBox, Polygon};
use super::track::{Track, TrackChange, Tracker};
use super::Pose;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

/// Une zone de l'image (porte, couloir, lit…).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Zone {
    pub name: String,
    pub area: Polygon,
    /// Présence prolongée signalée au-delà de ce délai (secondes) ; 0 = jamais.
    #[serde(default)]
    pub linger_secs: u32,
}

/// Deux noms de zone qui se confondraient (casse et espaces ignorés).
pub fn same_name(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// Pourquoi ces zones d'une caméra sont inutilisables (None : elles conviennent). Les noms doivent
/// être différents : le moteur et le journal distinguent les zones par leur nom.
pub fn zones_problem(zones: &[Zone]) -> Option<String> {
    for (i, z) in zones.iter().enumerate() {
        if z.name.trim().is_empty() {
            return Some("zone sans nom".into());
        }
        if let Some(p) = z.area.problem() {
            return Some(format!("zone « {} » : {p}", z.name));
        }
        if zones[..i].iter().any(|o| same_name(&o.name, &z.name)) {
            return Some(format!("deux zones s'appellent « {} »", z.name.trim()));
        }
    }
    None
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Une personne apparaît.
    Arrived,
    /// Elle n'est plus vue ; `stayed` : durée de présence (secondes).
    Left { stayed: f64 },
    EnteredZone(String),
    LeftZone(String),
    /// Toujours dans la zone depuis `secs`.
    Lingering { zone: String, secs: f64 },
    /// Chute probable : passage brusque de debout à allongé.
    Fall,
    /// Toujours au sol, quasi immobile, depuis `secs` (après une chute).
    Down { secs: f64 },
    /// Relevée après une chute.
    GotUp,
    /// Une main levée (tenue un instant).
    HandRaised,
    /// Les deux bras levés, tenus : signal d'appel à l'aide.
    ArmsUp,
    /// Aucun mouvement depuis `secs`.
    Still { secs: f64 },
}

impl Action {
    /// Les actions qui demandent qu'on aille voir.
    pub fn is_alarm(&self) -> bool {
        matches!(self, Action::Fall | Action::Down { .. } | Action::ArmsUp)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActionEvent {
    pub track: u32,
    pub action: Action,
    pub at: f64,
    pub bbox: BBox,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Posture {
    #[default]
    Unknown,
    Upright,
    Lying,
}

#[derive(Default)]
struct TrackState {
    first_seen: f64,
    /// Zones occupées → instant d'entrée.
    zones: HashMap<String, f64>,
    lingering_sent: HashSet<String>,
    posture: Posture,
    /// Avis consécutifs « allongé » / « debout », et depuis quand : une posture ne change
    /// qu'après `HOLD` secondes et deux avis concordants (une image mal lue ne fait ni une
    /// chute, ni un relevé, quelle que soit la cadence d'analyse).
    lying_votes: u32,
    lying_since: Option<f64>,
    upright_votes: u32,
    upright_since: Option<f64>,
    /// Moments récents où la personne se tenait debout : (instant, boîte, hauteur des hanches).
    /// Référence pour mesurer une chute.
    standing: VecDeque<(f64, BBox, Option<f32>)>,
    /// Chute possible : (instant, haut et hauteur de la boîte debout de référence). Confirmée si
    /// la personne reste au sol `fall_hold` secondes ; annulée si elle se relève avant.
    fall_candidate: Option<(f64, f32, f32)>,
    /// Boîte à l'image précédente (saut de piste) et dernière chute signalée.
    last_box: Option<BBox>,
    last_fall: Option<f64>,
    /// Au sol depuis (après une chute), et la silhouette debout d'avant la chute.
    down_since: Option<f64>,
    fall_ref: Option<(f32, f32)>,
    down_sent: bool,
    /// Gestes : depuis quand, dernière fois vu, déjà signalé.
    hand: Gesture,
    arms: Gesture,
    /// Immobilité : position de référence et depuis quand.
    still_ref: Option<(f64, BBox)>,
    still_sent: bool,
}

/// Un geste tenu : signalé une fois tenu `hold` secondes ; réarmé après `HOLD` secondes sans lui
/// (une image où le poignet n'est pas vu ne l'interrompt pas).
#[derive(Default)]
struct Gesture {
    since: Option<f64>,
    last: Option<f64>,
    sent: bool,
}

impl Gesture {
    /// Renvoie vrai quand le geste vient d'être tenu assez longtemps.
    fn update(&mut self, now: f64, on: bool, hold: f64) -> bool {
        if on {
            self.last = Some(now);
            let since = *self.since.get_or_insert(now);
            if !self.sent && now - since >= hold {
                self.sent = true;
                return true;
            }
        } else if self.last.is_none_or(|l| now - l > HOLD) {
            *self = Gesture::default();
        }
        false
    }
}

/// Mains au-dessus de la tête (gauche, droite), d'après le squelette.
pub fn hands_up(pose: &Pose, person: &BBox) -> (bool, bool) {
    let p = &pose.points;
    let sure = |i: usize| (p[i].score >= 0.3).then_some(p[i].at.y);
    // Hauteur de la tête : le nez, sinon les yeux, sinon un peu au-dessus des épaules.
    let head = sure(Pose::NOSE)
        .or_else(|| sure(1).zip(sure(2)).map(|(a, b)| (a + b) / 2.0))
        .or_else(|| sure(Pose::L_SHOULDER).zip(sure(Pose::R_SHOULDER)).map(|(a, b)| (a + b) / 2.0 - 0.12 * person.height()));
    let Some(head) = head else { return (false, false) };
    let above = |wrist: usize| sure(wrist).is_some_and(|y| y < head - 0.06 * person.height());
    (above(9), above(10))
}

/// Réglages des règles.
#[derive(Clone, Debug, PartialEq)]
pub struct Rules {
    pub fall: bool,
    /// Délai au sol avant l'alerte « personne au sol » (secondes).
    pub down_after: f64,
    /// Durée maximale du passage debout → au sol pour parler de chute (secondes).
    pub fall_window: f64,
    /// Temps passé au sol avant de confirmer la chute (secondes) : ramasser un objet, s'accroupir
    /// ou nouer ses lacets ne durent pas, une chute si.
    pub fall_hold: f64,
    /// Gestes (main levée, bras levés).
    pub gestures: bool,
    /// Immobilité signalée au-delà de ce délai (secondes) ; 0 = jamais.
    pub still_after: f64,
}

impl Default for Rules {
    fn default() -> Self {
        Rules { fall: true, down_after: 20.0, fall_window: 1.5, fall_hold: 1.5, gestures: true, still_after: 0.0 }
    }
}

/// Durée minimale d'une posture avant d'en tenir compte (secondes).
const HOLD: f64 = 0.3;

/// Boîte au moins aussi large que ça (largeur / hauteur, en pixels) : personne couchée, même si
/// le buste est mal lu (personne au sol vue de loin, petite dans l'image).
const LYING_ASPECT: f32 = 1.3;

/// Règles de chute, réglées sur 150 vidéos réelles (CAUCAFall : caméra d'angle au plafond dans
/// une vraie pièce ; UR Fall : caméra de face) avec `homesmart-cli vision eval falls` :
/// 74 % des chutes trouvées, 3 % de fausses alertes sur les gestes du quotidien (s'asseoir,
/// s'agenouiller, ramasser un objet, sautiller, marcher), contre 52 % et 23 % auparavant.
/// - passage à allongé, la tête (haut de la silhouette) au moins 20 % plus bas qu'au plus haut
///   de la position debout des 1,5 s d'avant ;
/// - ou descente brusque de la tête (60 %) et des hanches (40 %) : chute vers la caméra ou dos à
///   elle, où le corps couché paraît encore « debout » à l'image ;
/// - puis la personne reste au sol (tête au moins 45 % plus bas, ou couchée) `fall_hold` s.
const FALL_LYING_DROP: f32 = 0.2;
const FALL_HEAD_DROP: f32 = 0.6;
const FALL_HIP_DROP: f32 = 0.4;
const FALL_STAY_DROP: f32 = 0.45;
/// La position debout de référence ne compte que vue nettement (score du modèle) : une
/// silhouette douteuse (objet du décor, reflet) ne sert jamais de point de départ à une chute.
const STANDING_MIN_SCORE: f32 = 0.5;
/// Une chute ne rend pas la silhouette plus haute, ni bien plus étroite : au-delà, la piste est
/// passée à une autre silhouette (le buste seul d'une personne puis son corps entier, une autre
/// personne toute proche…). Mesuré sur 1 548 vidéos (`homesmart-cli vision eval alarms`) : les
/// 59 vraies chutes trouvées restent sous 1,11 en hauteur et au-dessus de 0,8 en largeur ; ces
/// deux bornes ôtent un quart des fausses chutes de la vie courante.
const FALL_MAX_GROW: f32 = 1.25;
const FALL_MIN_WIDTH: f32 = 0.6;
/// Bas de l'image : une boîte qui y arrive est coupée (jambes hors champ). « Couché » d'après la
/// seule forme de la boîte (squelette peu sûr) ne vaut rien si la personne debout l'était déjà.
const BOTTOM_EDGE: f32 = 0.97;
/// Après une chute signalée, pas d'autre avant ce délai pour la même personne.
const FALL_COOLDOWN: f64 = 30.0;

#[derive(Default)]
pub struct ActionEngine {
    pub zones: Vec<Zone>,
    pub rules: Rules,
    states: HashMap<u32, TrackState>,
}

/// Posture d'après le squelette (prioritaire s'il est fiable), sinon d'après la forme de la boîte.
pub fn posture(t: &Track, frame_w: f32, frame_h: f32) -> Posture {
    let r = t.bbox.aspect(frame_w, frame_h);
    if let Some(angle) = t.pose.as_ref().and_then(|p| p.torso_angle(frame_w, frame_h)) {
        return if angle >= 60.0 || r >= LYING_ASPECT {
            Posture::Lying
        } else if angle <= 35.0 && r <= 0.9 {
            Posture::Upright
        } else {
            Posture::Unknown
        };
    }
    if r >= 1.2 {
        Posture::Lying
    } else if r > 0.0 && r <= 0.8 {
        Posture::Upright
    } else {
        Posture::Unknown
    }
}

impl ActionEngine {
    pub fn new(zones: Vec<Zone>, rules: Rules) -> Self {
        ActionEngine { zones, rules, states: HashMap::new() }
    }

    /// Événements de cette image. `changes` : ce que le suivi vient d'annoncer pour la même image.
    pub fn update(&mut self, now: f64, tracker: &Tracker, changes: &[TrackChange], frame_w: f32, frame_h: f32) -> Vec<ActionEvent> {
        let mut out = Vec::new();
        for c in changes {
            match c {
                TrackChange::Confirmed(id) => {
                    if let Some(t) = tracker.track(*id) {
                        self.states.insert(*id, TrackState { first_seen: t.first_seen, ..TrackState::default() });
                        out.push(ActionEvent { track: *id, action: Action::Arrived, at: now, bbox: t.bbox });
                    }
                }
                TrackChange::Lost { id, was_confirmed, seen } => {
                    if let Some(st) = self.states.remove(id) {
                        if *was_confirmed {
                            out.push(ActionEvent { track: *id, action: Action::Left { stayed: (seen - st.first_seen).max(0.0) }, at: now, bbox: BBox::default() });
                        }
                    }
                }
            }
        }
        for t in tracker.tracks().iter().filter(|t| t.confirmed && t.last_seen >= now) {
            let Some(st) = self.states.get_mut(&t.id) else { continue };
            let ev = |action| ActionEvent { track: t.id, action, at: now, bbox: t.bbox };
            // Zones.
            let foot = t.bbox.foot();
            for z in &self.zones {
                let inside = z.area.contains(foot);
                match (inside, st.zones.get(&z.name).copied()) {
                    (true, None) => {
                        st.zones.insert(z.name.clone(), now);
                        out.push(ev(Action::EnteredZone(z.name.clone())));
                    }
                    (true, Some(since)) => {
                        let secs = now - since;
                        if z.linger_secs > 0 && secs >= z.linger_secs as f64 && st.lingering_sent.insert(z.name.clone()) {
                            out.push(ev(Action::Lingering { zone: z.name.clone(), secs }));
                        }
                    }
                    (false, Some(_)) => {
                        st.zones.remove(&z.name);
                        st.lingering_sent.remove(&z.name);
                        out.push(ev(Action::LeftZone(z.name.clone())));
                    }
                    (false, None) => {}
                }
            }
            let p = posture(t, frame_w, frame_h);
            // Gestes : mains au-dessus de la tête d'une personne debout ou assise. Couchée, « au-dessus »
            // ne veut rien dire à l'image (sur les vidéos de chute, 8 « bras levés » sur 8 l'étaient).
            if let (true, Some(pose)) = (self.rules.gestures, &t.pose) {
                let (l, r) = hands_up(pose, &t.bbox);
                let upright = p != Posture::Lying;
                if st.hand.update(now, upright && l != r, 0.6) {
                    out.push(ev(Action::HandRaised));
                }
                if st.arms.update(now, upright && l && r, 2.0) {
                    out.push(ev(Action::ArmsUp));
                }
            }
            // Immobilité : la boîte ne bouge plus (ni place, ni taille) depuis le délai.
            if self.rules.still_after > 0.0 {
                let moved = st.still_ref.is_none_or(|(_, b)| b.center().dist(t.bbox.center()) > 0.03 || (t.bbox.area() / b.area().max(1e-6) - 1.0).abs() > 0.15);
                if moved {
                    (st.still_ref, st.still_sent) = (Some((now, t.bbox)), false);
                } else if let Some((since, _)) = st.still_ref.filter(|_| !st.still_sent) {
                    if now - since >= self.rules.still_after {
                        st.still_sent = true;
                        out.push(ev(Action::Still { secs: now - since }));
                    }
                }
            }
            // Posture et chute.
            if !self.rules.fall {
                continue;
            }
            match p {
                Posture::Lying => {
                    (st.lying_votes, st.upright_votes, st.upright_since) = (st.lying_votes + 1, 0, None);
                    st.lying_since.get_or_insert(now);
                }
                Posture::Upright => {
                    (st.upright_votes, st.lying_votes, st.lying_since) = (st.upright_votes + 1, 0, None);
                    st.upright_since.get_or_insert(now);
                }
                Posture::Unknown => {}
            }
            let steady = |votes: u32, since: Option<f64>| votes >= 2 && since.is_some_and(|s| now - s >= HOLD);
            let was = st.posture;
            let (top, height) = (t.bbox.y0, t.bbox.height());
            let hips = t.pose.as_ref().and_then(|p| p.hips()).map(|h| h.y);
            let upright = p == Posture::Upright && steady(st.upright_votes, st.upright_since);
            // La boîte ne recouvre plus du tout la précédente : ce n'est plus la même silhouette (piste
            // reprise par une autre détection) ; rien de ce qui précède ne vaut pour une chute.
            let jumped = st.last_box.is_some_and(|b| b.intersection(&t.bbox) <= 0.0);
            st.last_box = Some(t.bbox);
            if jumped {
                st.standing.clear();
                st.fall_candidate = None;
            }
            if upright && t.score >= STANDING_MIN_SCORE {
                st.standing.push_back((now, t.bbox, hips));
            }
            while st.standing.front().is_some_and(|s| now - s.0 > self.rules.fall_window) {
                st.standing.pop_front();
            }
            let lies_down = p == Posture::Lying && steady(st.lying_votes, st.lying_since) && was != Posture::Lying;
            // Chute possible : comparée au plus haut de la position debout récente.
            if st.fall_candidate.is_none() && st.down_since.is_none() && st.last_fall.is_none_or(|f| now - f >= FALL_COOLDOWN) {
                if let Some(&(_, b0, hips0)) = st.standing.iter().filter(|s| now - s.0 >= 0.15).min_by(|a, b| a.1.y0.total_cmp(&b.1.y0)) {
                    let (top0, h0) = (b0.y0, b0.height());
                    let drop = (top - top0) / h0.max(1e-3);
                    let shape_only = t.pose.as_ref().and_then(|p| p.torso_angle(frame_w, frame_h)).is_none();
                    let to_lying = lies_down && was == Posture::Upright && drop >= FALL_LYING_DROP && !(shape_only && b0.y1 >= BOTTOM_EDGE);
                    let sudden = drop >= FALL_HEAD_DROP && hips.zip(hips0).is_some_and(|(h, h0_)| (h - h0_) / h0.max(1e-3) >= FALL_HIP_DROP);
                    let same_silhouette = height <= h0 * FALL_MAX_GROW && t.bbox.width() >= b0.width() * FALL_MIN_WIDTH;
                    if (to_lying || sudden) && same_silhouette {
                        st.fall_candidate = Some((now, top0, h0));
                    }
                }
            }
            if let Some((since, top0, h0)) = st.fall_candidate {
                let low = (top - top0) / h0.max(1e-3) >= FALL_STAY_DROP || p == Posture::Lying;
                if !low {
                    // Relevée aussitôt : ramassait quelque chose, s'accroupissait…
                    st.fall_candidate = None;
                } else if now - since >= self.rules.fall_hold {
                    (st.fall_candidate, st.down_since, st.fall_ref, st.last_fall) = (None, Some(since), Some((top0, h0)), Some(now));
                    out.push(ev(Action::Fall));
                }
            }
            if upright && was != Posture::Upright {
                st.posture = Posture::Upright;
            } else if lies_down {
                st.posture = Posture::Lying;
            }
            // Relevée : debout, et de nouveau à sa hauteur d'avant la chute (une chute vers la
            // caméra laisse la silhouette « debout » à l'image alors que la personne est au sol).
            if upright && st.down_since.is_some() && st.fall_ref.is_none_or(|(top0, h0)| (top - top0) / h0.max(1e-3) < 0.25) {
                (st.down_since, st.fall_ref, st.down_sent) = (None, None, false);
                out.push(ev(Action::GotUp));
            }
            if let (Some(since), false) = (st.down_since, st.down_sent) {
                if now - since >= self.rules.down_after && t.speed(5.0) < 0.03 {
                    st.down_sent = true;
                    out.push(ev(Action::Down { secs: now - since }));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::geom::Point;
    use super::super::track::Detection;
    use super::*;

    const W: f32 = 640.0;
    const H: f32 = 480.0;

    fn standing(x: f32) -> Detection {
        Detection { bbox: BBox::new(x, 0.25, x + 0.12, 0.95), score: 0.9, pose: None }
    }

    fn lying(x: f32) -> Detection {
        Detection { bbox: BBox::new(x - 0.1, 0.78, x + 0.35, 0.97), score: 0.9, pose: None }
    }

    fn run(engine: &mut ActionEngine, tracker: &mut Tracker, t: f64, dets: &[Detection]) -> Vec<Action> {
        let ch = tracker.update(t, dets);
        engine.update(t, tracker, &ch, W, H).into_iter().map(|e| e.action).collect()
    }

    #[test]
    fn zones_and_lingering() {
        let door = Zone { name: "Porte".into(), area: Polygon(vec![Point::new(0.0, 0.5), Point::new(0.3, 0.5), Point::new(0.3, 1.0), Point::new(0.0, 1.0)]), linger_secs: 5 };
        let mut e = ActionEngine::new(vec![door], Rules::default());
        let mut tr = Tracker::default();
        assert!(run(&mut e, &mut tr, 0.0, &[standing(0.6)]).is_empty());
        assert_eq!(run(&mut e, &mut tr, 0.5, &[standing(0.6)]), vec![Action::Arrived]);
        assert_eq!(run(&mut e, &mut tr, 1.0, &[standing(0.1)]), vec![Action::EnteredZone("Porte".into())]);
        assert!(run(&mut e, &mut tr, 3.0, &[standing(0.1)]).is_empty());
        let a = run(&mut e, &mut tr, 6.5, &[standing(0.1)]);
        assert!(matches!(&a[..], [Action::Lingering { zone, secs }] if zone == "Porte" && *secs >= 5.0));
        // Une seule alerte par passage.
        assert!(run(&mut e, &mut tr, 9.0, &[standing(0.1)]).is_empty());
        assert_eq!(run(&mut e, &mut tr, 9.5, &[standing(0.6)]), vec![Action::LeftZone("Porte".into())]);
        let gone = run(&mut e, &mut tr, 13.0, &[]);
        assert!(matches!(&gone[..], [Action::Left { stayed }] if (*stayed - 9.5).abs() < 1e-9), "présence : de la première à la dernière détection");
    }

    #[test]
    fn zone_checks() {
        let zone = |name: &str, pts: &[(f32, f32)]| Zone { name: name.into(), area: Polygon(pts.iter().map(|&(x, y)| Point::new(x, y)).collect()), linger_secs: 0 };
        let square = [(0.1, 0.1), (0.4, 0.1), (0.4, 0.4), (0.1, 0.4)];
        assert_eq!(zones_problem(&[zone("Porte", &square), zone("Lit", &square)]), None);
        assert_eq!(zones_problem(&[zone("Porte", &square), zone(" porte ", &square)]).as_deref(), Some("deux zones s'appellent « porte »"));
        assert_eq!(zones_problem(&[zone("  ", &square)]).as_deref(), Some("zone sans nom"));
        assert_eq!(zones_problem(&[zone("Lit", &square[..2])]).as_deref(), Some("zone « Lit » : il faut au moins 3 points"));
        assert!(same_name("Entrée", "ENTRÉE ") && !same_name("Entrée", "Entree"));
    }

    #[test]
    fn sudden_fall_then_down_then_up() {
        let mut e = ActionEngine::new(vec![], Rules { down_after: 10.0, ..Rules::default() });
        let mut tr = Tracker::default();
        for i in 0..4 {
            run(&mut e, &mut tr, i as f64 * 0.5, &[standing(0.4)]);
        }
        // En une seconde, la personne passe au sol, et y reste.
        assert!(run(&mut e, &mut tr, 2.5, &[lying(0.4)]).is_empty(), "une seule image ne suffit pas");
        let mut down = Vec::new();
        for i in 0..=24 {
            let t = 3.0 + i as f64 * 0.5;
            down.extend(run(&mut e, &mut tr, t, &[lying(0.4)]).into_iter().map(|a| (t, a)));
        }
        // Chute confirmée 1,5 s après le passage au sol, « au sol » 10 s après la chute.
        assert!(matches!(&down[..], [(t1, Action::Fall), (_, Action::Down { secs })] if (*t1 - 4.5).abs() < 1e-9 && *secs >= 10.0), "{down:?}");
        assert!(run(&mut e, &mut tr, 16.0, &[standing(0.4)]).is_empty(), "une seule image debout ne suffit pas");
        assert_eq!(run(&mut e, &mut tr, 16.5, &[standing(0.4)]), vec![Action::GotUp]);
    }

    /// Personne debout, squelette avec mains en bas ou levées (gauche, droite).
    fn with_hands(left_up: bool, right_up: bool) -> Detection {
        use super::super::{Keypoint, Pose};
        let mut points = [Keypoint::default(); 17];
        let mut set = |i: usize, x: f32, y: f32| points[i] = Keypoint { at: Point::new(x, y), score: 0.9 };
        set(Pose::NOSE, 0.46, 0.32);
        set(Pose::L_SHOULDER, 0.43, 0.4);
        set(Pose::R_SHOULDER, 0.49, 0.4);
        set(Pose::L_HIP, 0.44, 0.62);
        set(Pose::R_HIP, 0.48, 0.62);
        set(9, 0.42, if left_up { 0.2 } else { 0.6 });
        set(10, 0.5, if right_up { 0.2 } else { 0.6 });
        Detection { pose: Some(Pose { points }), ..standing(0.4) }
    }

    #[test]
    fn hand_raised_and_arms_up() {
        let mut e = ActionEngine::new(vec![], Rules::default());
        let mut tr = Tracker::default();
        let mut all = Vec::new();
        let mut t = 0.0;
        let mut step = |e: &mut ActionEngine, tr: &mut Tracker, d: Detection, all: &mut Vec<Action>| {
            all.extend(run(e, tr, t, &[d]));
            t += 0.25;
        };
        for _ in 0..4 {
            step(&mut e, &mut tr, with_hands(false, false), &mut all);
        }
        // Main gauche levée 1 s : signalée une fois.
        for _ in 0..4 {
            step(&mut e, &mut tr, with_hands(true, false), &mut all);
        }
        for _ in 0..4 {
            step(&mut e, &mut tr, with_hands(false, false), &mut all);
        }
        // Deux bras levés 3 s : appel à l'aide, une fois.
        for _ in 0..12 {
            step(&mut e, &mut tr, with_hands(true, true), &mut all);
        }
        assert_eq!(all, vec![Action::Arrived, Action::HandRaised, Action::ArmsUp], "{all:?}");
    }

    #[test]
    fn still_for_too_long() {
        let mut e = ActionEngine::new(vec![], Rules { still_after: 5.0, ..Rules::default() });
        let mut tr = Tracker::default();
        let mut all = Vec::new();
        for i in 0..30 {
            all.extend(run(&mut e, &mut tr, i as f64 * 0.5, &[standing(0.4)]));
        }
        assert!(matches!(&all[..], [Action::Arrived, Action::Still { secs }] if *secs >= 5.0), "{all:?}");
        // Elle bouge : réarmé, de nouveau signalé après 5 s sans bouger.
        let mut again = Vec::new();
        for i in 0..14 {
            again.extend(run(&mut e, &mut tr, 15.0 + i as f64 * 0.5, &[standing(0.55)]));
        }
        assert!(matches!(&again[..], [Action::Still { .. }]), "{again:?}");
    }

    #[test]
    fn high_rate_ignores_a_glitch_but_sees_the_fall() {
        // 30 analyses par seconde : une image isolée « allongé » n'est pas une chute…
        let mut e = ActionEngine::new(vec![], Rules::default());
        let mut tr = Tracker::default();
        let mut all = Vec::new();
        let mut t = 0.0;
        for i in 0..90 {
            t = i as f64 / 30.0;
            let d = if i == 50 { lying(0.4) } else { standing(0.4) };
            all.extend(run(&mut e, &mut tr, t, &[d]));
        }
        assert_eq!(all, vec![Action::Arrived], "{all:?}");
        // … une vraie chute (au sol et qui y reste) est vue une fois, moins de 2 s après.
        let mut falls = Vec::new();
        for i in 1..=75 {
            let now = t + i as f64 / 30.0;
            falls.extend(run(&mut e, &mut tr, now, &[lying(0.4)]).into_iter().map(|a| (now - t, a)));
        }
        assert!(matches!(&falls[..], [(dt, Action::Fall)] if *dt < 2.0), "{falls:?}");
    }

    /// Silhouette avec squelette : haut de la boîte, hanches, buste vertical ou couché.
    fn posed(top: f32, hips: f32, lying_torso: bool) -> Detection {
        use super::super::{Keypoint, Pose};
        let mut points = [Keypoint::default(); 17];
        let mut set = |i: usize, x: f32, y: f32| points[i] = Keypoint { at: Point::new(x, y), score: 0.9 };
        if lying_torso {
            set(Pose::L_SHOULDER, 0.3, hips - 0.02);
            set(Pose::R_SHOULDER, 0.3, hips);
            set(Pose::L_HIP, 0.5, hips - 0.02);
            set(Pose::R_HIP, 0.5, hips);
        } else {
            set(Pose::L_SHOULDER, 0.43, top + 0.1);
            set(Pose::R_SHOULDER, 0.49, top + 0.1);
            set(Pose::L_HIP, 0.44, hips);
            set(Pose::R_HIP, 0.48, hips);
        }
        Detection { bbox: BBox::new(0.4, top, 0.52, 0.97), score: 0.9, pose: Some(Pose { points }) }
    }

    #[test]
    fn bending_down_then_standing_up_is_not_a_fall() {
        let mut e = ActionEngine::new(vec![], Rules::default());
        let mut tr = Tracker::default();
        let mut all = Vec::new();
        let mut t = 0.0;
        for (n, d) in [(8, posed(0.25, 0.6, false)), (6, lying(0.4)), (8, posed(0.25, 0.6, false))] {
            for _ in 0..n {
                all.extend(run(&mut e, &mut tr, t, &[d.clone()]));
                t += 0.25;
            }
        }
        assert_eq!(all, vec![Action::Arrived], "1,5 s penché puis relevé : {all:?}");
    }

    #[test]
    fn fall_towards_the_camera_is_seen() {
        // Caméra au plafond : la personne tombe vers elle ; le buste reste « vertical » à l'image,
        // mais la tête et les hanches descendent d'un coup, puis ne remontent pas.
        let mut e = ActionEngine::new(vec![], Rules::default());
        let mut tr = Tracker::default();
        let mut all = Vec::new();
        let mut t = 0.0;
        for _ in 0..8 {
            all.extend(run(&mut e, &mut tr, t, &[posed(0.25, 0.6, false)]));
            t += 0.25;
        }
        for _ in 0..12 {
            all.extend(run(&mut e, &mut tr, t, &[posed(0.7, 0.9, false)]));
            t += 0.25;
        }
        assert_eq!(all, vec![Action::Arrived, Action::Fall], "{all:?}");
    }

    #[test]
    fn a_track_that_jumps_elsewhere_is_not_a_fall() {
        // Une piste debout à droite, puis reprise d'un coup par une silhouette couchée en bas à
        // gauche (objet du décor) : pas de chute.
        let mut e = ActionEngine::new(vec![], Rules::default());
        let mut tr = Tracker::default();
        let mut all = Vec::new();
        let mut t = 0.0;
        for _ in 0..8 {
            all.extend(run(&mut e, &mut tr, t, &[Detection { bbox: BBox::new(0.66, 0.11, 0.92, 1.0), score: 0.85, pose: None }]));
            t += 0.25;
        }
        for _ in 0..12 {
            all.extend(run(&mut e, &mut tr, t, &[Detection { bbox: BBox::new(0.14, 0.57, 0.6, 1.0), score: 0.85, pose: None }]));
            t += 0.25;
        }
        assert!(!all.contains(&Action::Fall), "{all:?}");
    }

    #[test]
    fn a_doubtful_silhouette_never_starts_a_fall() {
        let mut e = ActionEngine::new(vec![], Rules::default());
        let mut tr = Tracker::default();
        let mut all = Vec::new();
        let weak = |d: Detection| Detection { score: 0.3, ..d };
        for i in 0..4 {
            all.extend(run(&mut e, &mut tr, i as f64 * 0.5, &[weak(standing(0.4))]));
        }
        for i in 0..8 {
            all.extend(run(&mut e, &mut tr, 2.0 + i as f64 * 0.5, &[weak(lying(0.4))]));
        }
        assert!(!all.contains(&Action::Fall), "{all:?}");
    }

    /// Boîte et squelette (épaules et hanches au milieu, nez au-dessus des épaules, poignets à part).
    fn framed(b: BBox, shoulders: f32, hips: f32, wrists: f32) -> Detection {
        use super::super::{Keypoint, Pose};
        let mut points = [Keypoint::default(); 17];
        let x = (b.x0 + b.x1) / 2.0;
        let mut set = |i: usize, x: f32, y: f32| points[i] = Keypoint { at: Point::new(x, y), score: 0.9 };
        set(Pose::NOSE, x, shoulders - 0.04);
        set(Pose::L_SHOULDER, x - 0.02, shoulders);
        set(Pose::R_SHOULDER, x + 0.02, shoulders);
        set(Pose::L_HIP, x - 0.02, hips);
        set(Pose::R_HIP, x + 0.02, hips);
        set(9, x - 0.04, wrists);
        set(10, x + 0.04, wrists);
        Detection { bbox: b, score: 0.9, pose: Some(Pose { points }) }
    }

    #[test]
    fn a_bust_then_the_whole_body_is_not_a_fall() {
        // Constaté sur des vidéos réelles : la piste commence sur le buste seul d'une personne (boîte
        // deux fois plus petite), puis passe à son corps entier. Le haut de la boîte et les hanches
        // « descendent » d'un coup, mais une chute ne rend pas la silhouette deux fois plus haute.
        let mut e = ActionEngine::new(vec![], Rules::default());
        let mut tr = Tracker::default();
        let mut all = Vec::new();
        let mut t = 0.0;
        for (n, d) in [(8, framed(BBox::new(0.40, 0.10, 0.52, 0.40), 0.16, 0.36, 0.3)), (16, framed(BBox::new(0.38, 0.36, 0.54, 0.98), 0.46, 0.70, 0.65))] {
            for _ in 0..n {
                all.extend(run(&mut e, &mut tr, t, &[d.clone()]));
                t += 0.25;
            }
        }
        assert_eq!(all, vec![Action::Arrived], "{all:?}");
    }

    #[test]
    fn lying_by_shape_alone_needs_the_legs_in_view() {
        // Debout, coupée par le bas de l'image (jambes hors champ), sans squelette sûr ; puis une boîte
        // large tout en bas : sa forme ne dit rien, ce n'est pas une chute.
        let mut e = ActionEngine::new(vec![], Rules::default());
        let mut tr = Tracker::default();
        let mut all = Vec::new();
        for i in 0..6 {
            all.extend(run(&mut e, &mut tr, i as f64 * 0.25, &[Detection { bbox: BBox::new(0.4, 0.3, 0.55, 1.0), score: 0.9, pose: None }]));
        }
        for i in 0..12 {
            all.extend(run(&mut e, &mut tr, 1.5 + i as f64 * 0.25, &[Detection { bbox: BBox::new(0.3, 0.7, 0.75, 1.0), score: 0.9, pose: None }]));
        }
        assert_eq!(all, vec![Action::Arrived], "{all:?}");
    }

    #[test]
    fn arms_up_while_lying_is_not_a_call_for_help() {
        // Au sol, buste horizontal : des poignets plus hauts que le nez à l'image ne sont pas des bras levés.
        let mut e = ActionEngine::new(vec![], Rules { fall: false, ..Rules::default() });
        let mut tr = Tracker::default();
        let mut lying = framed(BBox::new(0.3, 0.7, 0.75, 0.95), 0.8, 0.82, 0.7);
        if let Some(p) = lying.pose.as_mut() {
            p.points[Pose::L_SHOULDER].at = Point::new(0.40, 0.80);
            p.points[Pose::R_SHOULDER].at = Point::new(0.41, 0.81);
            p.points[Pose::L_HIP].at = Point::new(0.60, 0.81);
            p.points[Pose::R_HIP].at = Point::new(0.61, 0.82);
        }
        let mut all = Vec::new();
        for i in 0..20 {
            all.extend(run(&mut e, &mut tr, i as f64 * 0.25, &[lying.clone()]));
        }
        assert_eq!(all, vec![Action::Arrived], "{all:?}");
        // Debout, les mêmes bras levés tenus 2 s : l'appel est vu.
        let mut e = ActionEngine::new(vec![], Rules { fall: false, ..Rules::default() });
        let mut tr = Tracker::default();
        let up = framed(BBox::new(0.4, 0.15, 0.52, 0.95), 0.32, 0.6, 0.12);
        let mut all = Vec::new();
        for i in 0..14 {
            all.extend(run(&mut e, &mut tr, i as f64 * 0.25, &[up.clone()]));
        }
        assert_eq!(all, vec![Action::Arrived, Action::ArmsUp], "{all:?}");
    }

    #[test]
    fn lying_down_slowly_is_not_a_fall() {
        let mut e = ActionEngine::new(vec![], Rules::default());
        let mut tr = Tracker::default();
        for i in 0..4 {
            run(&mut e, &mut tr, i as f64 * 0.5, &[standing(0.4)]);
        }
        // Six secondes « assis » (forme indécise) avant de s'allonger : ce n'est pas une chute.
        let seated = Detection { bbox: BBox::new(0.36, 0.55, 0.66, 0.95), score: 0.9, pose: None };
        for i in 0..12 {
            assert!(run(&mut e, &mut tr, 2.0 + i as f64 * 0.5, &[seated.clone()]).is_empty());
        }
        let mut all = Vec::new();
        for i in 0..6 {
            all.extend(run(&mut e, &mut tr, 8.0 + i as f64 * 0.5, &[lying(0.4)]));
        }
        assert!(all.is_empty(), "{all:?}");
    }
}
