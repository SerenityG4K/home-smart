//! Coups et violence : une personne qui en frappe une autre (coups de poing, coups de pied,
//! bousculade, bagarre), d'après les squelettes des personnes suivies, sans autre modèle.
//!
//! Une alerte « violence » est critique : mieux vaut en manquer une que crier au loup. D'où :
//! - **coup** : un poignet ou une cheville part vite vers une autre personne toute proche et arrive
//!   sur sa silhouette. La vitesse est mesurée par rapport au buste (marcher ou courir ne compte
//!   pas), en tailles de la personne par seconde (indépendante de la distance à la caméra), sur au
//!   moins 0,12 s (indépendante de la cadence d'analyse) ;
//! - un poing compte seulement sous le haut de la silhouette de l'autre : accolades, tapes dans la
//!   main, bises et salutations se passent à hauteur de tête et d'épaules ;
//! - **squelettes fiables seulement** : points sûrs, silhouette qui n'a pas sauté d'une image à
//!   l'autre (piste reprise par une autre personne dans une mêlée), pas de « deuxième personne »
//!   qui n'est qu'une détection fantôme autour de la première ;
//! - **répétition** : plusieurs coups entre les deux mêmes personnes en peu de temps. Un geste
//!   brusque isolé ne suffit pas, même un vrai coup (une gifle, une poussée) : seule une bagarre
//!   qui dure déclenche l'alerte ;
//! - **pas dans une foule** : au-delà de 5 personnes à l'image (sport collectif, fête, ronde de
//!   capoeira), les gestes se croisent trop pour dire qui frappe qui ;
//! - **coup de pied d'un corps entier** : une cheville ne compte que si celui qui frappe est tout
//!   entier dans l'image. Coupé par le bord, ses jambes hors champ sont devinées par le modèle.
//!
//! Le coup de poing amical dans l'épaule ne compte pas (trop haut), et une tape isolée encore
//! moins (pas de répétition). Mais vue de la caméra, une chamaillerie qui dure ressemble à une
//! bagarre : l'écoute aide alors (des rires entendus autour des coups retiennent l'alerte, voir
//! `listen::sounds::LAUGHTER` et le hub).
//!
//! Réglé avec `homesmart-cli vision violence-eval` sur 1 548 vidéos réelles, RTMO-s à 8 analyses
//! par seconde. Aucune fausse alerte sur les 480 vidéos sans violence d'origine (accolades, tapes
//! dans la main, saluts, sauts de joie, poignées de main, passants de vraies caméras de
//! surveillance, chutes et gestes du quotidien). Jeux, sports et gestes amicaux de Kinetics-700
//! (YouTube) : 11 fausses alertes sur 481 scènes de la vie à la maison (boxe 5/48, lutte 3/47,
//! oreillers 1/48, massage 1/46, poignée de main 1/50 ; jamais sur les chatouilles, tapes dans la
//! main, accolades, bras de fer ni chatouilles du nombril), 4 sur 99 en capoeira et plaquages
//! (67 sans les deux dernières règles) ; avec les rires entendus, 9 sur 481. Trouvées : 13 % des
//! 440 scènes violentes (AIRTLab 51/230, extraits de 2 s de vraies caméras de surveillance 6/150,
//! coups uniques d'UT-Interaction 0/60, voulu). Davantage à 15 analyses par seconde (78/440),
//! bien moins avec RTMO-m (23/440 : il voit plus de silhouettes, souvent coupées par le bord),
//! presque rien à 4 (gestes trop brefs pour être vus).
//!
//! Les cris et les chocs (CED) ne servent pas : sur les vidéos qui ont du son, ils ne distinguent
//! presque rien (cris ou chocs dans 2 à 4 % des bagarres seulement).

use super::geom::{BBox, Point};
use super::track::Tracker;
use super::Pose;
use std::collections::{HashMap, VecDeque};

/// Au-delà de cet écart (secondes), deux squelettes d'une piste ne se comparent plus.
const KEEP: f64 = 0.5;

/// Poignets et chevilles : ce qui frappe.
const WRISTS: [usize; 2] = [9, 10];
const ANKLES: [usize; 2] = [15, 16];

/// Réglages, mesurés avec `homesmart-cli vision violence-eval`.
#[derive(Clone, Debug, PartialEq)]
pub struct ViolenceRules {
    /// Confiance minimale des points du squelette utilisés (poignet ou cheville, et au moins 3
    /// points du buste) : dans une mêlée, les points douteux sont souvent ceux de l'autre.
    pub min_point: f32,
    /// Vitesse minimale d'un poignet, par rapport au buste, en tailles de la personne par seconde…
    pub wrist_speed: f32,
    /// … et d'une cheville (un coup de pied, une bousculade : les jambes restent calmes quand on
    /// se salue ou qu'on s'embrasse).
    pub ankle_speed: f32,
    /// Au-delà, ce n'est pas un geste mais un point du squelette qui saute d'une personne à l'autre.
    pub max_speed: f32,
    /// Agrandissement de la silhouette de l'autre (0,4 = +40 %) pour dire que le coup l'atteint.
    pub reach: f32,
    /// Cosinus minimal entre le geste et la direction de l'autre : le geste va vers lui.
    pub toward: f32,
    /// Un poing compte s'il arrive sous ce niveau de la silhouette de l'autre (0 = haut, 1 = pieds).
    pub wrist_low: f32,
    /// Recouvrement minimal (IoU) de la silhouette de celui qui frappe entre les deux images. En
    /// dessous, la piste a sauté d'une personne à l'autre (mêlée, accolade, doublon) : le
    /// « geste » mesuré n'en est pas un.
    pub steady: f32,
    /// Deux silhouettes dont la plus petite est à ce point (part de sa surface) dans l'autre sont
    /// sans doute une seule personne vue deux fois (détection fantôme autour d'elle) : on ne
    /// mesure rien entre elles.
    pub same_person: f32,
    /// Durée minimale sur laquelle mesurer un geste (secondes) : l'image d'il y a au moins ce
    /// temps sert de point de départ. Plus vite que 8 analyses par seconde, la vitesse mesurée
    /// d'une image à la suivante grossit les pics et le tremblement des points.
    pub base: f64,
    /// Images avec un coup nécessaires, entre les deux mêmes personnes…
    pub strikes: u32,
    /// … comptées au plus une par `min_gap` secondes (le compte ne dépend pas de la cadence)…
    pub min_gap: f64,
    /// … en au plus ce temps (secondes).
    pub window: f64,
    /// Après une alerte, pas d'autre avant ce délai (secondes), même entre d'autres pistes : une
    /// bagarre renumérote souvent les pistes, c'est toujours la même.
    pub cooldown: f64,
    /// Au-delà de ce nombre de personnes à l'image, on ne cherche pas de coups.
    pub max_people: usize,
    /// Une cheville compte seulement si la boîte de celui qui frappe est à au moins cette marge des
    /// bords de l'image (part de l'image).
    pub edge: f32,
}

impl Default for ViolenceRules {
    fn default() -> Self {
        ViolenceRules {
            min_point: 0.5,
            wrist_speed: 2.5,
            ankle_speed: 1.25,
            max_speed: 8.0,
            reach: 0.4,
            toward: 0.0,
            wrist_low: 0.4,
            steady: 0.4,
            same_person: 0.9,
            base: 0.12,
            strikes: 4,
            min_gap: 0.1,
            window: 4.0,
            cooldown: 60.0,
            max_people: 5,
            edge: 0.01,
        }
    }
}

/// Violence entre deux personnes suivies.
#[derive(Clone, Debug, PartialEq)]
pub struct ViolenceEvent {
    /// Les deux pistes (la plus petite d'abord).
    pub tracks: (u32, u32),
    pub at: f64,
    /// Les deux personnes ensemble.
    pub bbox: BBox,
    /// Images avec un coup dans la fenêtre.
    pub strikes: u32,
}

/// Un coup vu à une image (gardé seulement pour la mesure, voir `ViolenceDetector::record`).
#[derive(Clone, Debug, PartialEq)]
pub struct Strike {
    pub at: f64,
    /// Celui qui frappe, celui qui est frappé.
    pub from: u32,
    pub to: u32,
    /// Point du squelette qui frappe (poignet 9/10, cheville 15/16).
    pub limb: usize,
    /// Vitesse par rapport au buste, en tailles de la personne par seconde.
    pub speed: f32,
    /// Où il arrive dans la silhouette de l'autre (0 = haut, 1 = pieds).
    pub height: f32,
    /// Cosinus entre le geste et la direction de l'autre.
    pub toward: f32,
}

/// Le squelette d'une piste à une image.
#[derive(Clone, Debug)]
struct Sample {
    at: f64,
    bbox: BBox,
    pose: Pose,
}

/// Une personne vue à cette image.
struct Body<'a> {
    id: u32,
    bbox: BBox,
    pose: &'a Pose,
}

#[derive(Default)]
pub struct ViolenceDetector {
    pub rules: ViolenceRules,
    /// Squelettes récents de chaque piste (au plus `KEEP` secondes), le plus ancien en tête.
    last: HashMap<u32, VecDeque<Sample>>,
    /// Par paire (plus petite piste d'abord) : instants des images où il y a eu un coup.
    strikes: HashMap<(u32, u32), VecDeque<f64>>,
    alerted: Option<f64>,
    /// Si présent, chaque coup vu y est ajouté (mesure seulement).
    pub record: Option<Vec<Strike>>,
}

/// Milieu du buste (au moins 3 des épaules et hanches assez sûres), en coordonnées normalisées.
fn torso_center(p: &Pose, min: f32) -> Option<Point> {
    let pts: Vec<Point> = [Pose::L_SHOULDER, Pose::R_SHOULDER, Pose::L_HIP, Pose::R_HIP].iter().map(|&i| p.points[i]).filter(|k| k.score >= min).map(|k| k.at).collect();
    (pts.len() >= 3).then(|| Point::new(pts.iter().map(|p| p.x).sum::<f32>() / pts.len() as f32, pts.iter().map(|p| p.y).sum::<f32>() / pts.len() as f32))
}

impl ViolenceDetector {
    pub fn new(rules: ViolenceRules) -> Self {
        ViolenceDetector { rules, ..ViolenceDetector::default() }
    }

    /// Coups portés par `a` à cette image (au plus un par membre et par personne frappée).
    fn blows(&self, a: &Body, prev: &Sample, now: f64, bodies: &[Body], fw: f32, fh: f32) -> Vec<Strike> {
        let r = &self.rules;
        let mut out = Vec::new();
        let dt = (now - prev.at) as f32;
        let (Some(c1), Some(c0)) = (torso_center(a.pose, r.min_point), torso_center(&prev.pose, r.min_point)) else { return out };
        // Taille de la personne : hauteur de sa boîte aux deux instants, en pixels.
        let size = (a.bbox.height() + prev.bbox.height()) / 2.0 * fh;
        if size < 1.0 || dt <= 0.0 || a.bbox.iou(&prev.bbox) < r.steady {
            return out;
        }
        let px = |p: Point| Point::new(p.x * fw, p.y * fh);
        let whole = a.bbox.x0 >= r.edge && a.bbox.y0 >= r.edge && a.bbox.x1 <= 1.0 - r.edge && a.bbox.y1 <= 1.0 - r.edge;
        for (limb, min_speed) in WRISTS.iter().map(|&l| (l, r.wrist_speed)).chain(ANKLES.iter().map(|&l| (l, r.ankle_speed))) {
            let (k1, k0) = (a.pose.points[limb], prev.pose.points[limb]);
            if k1.score < r.min_point || k0.score < r.min_point || (ANKLES.contains(&limb) && !whole) {
                continue;
            }
            // Déplacement par rapport au buste, en pixels.
            let (p1, p0, b1, b0) = (px(k1.at), px(k0.at), px(c1), px(c0));
            let (mx, my) = ((p1.x - b1.x) - (p0.x - b0.x), (p1.y - b1.y) - (p0.y - b0.y));
            let moved = (mx * mx + my * my).sqrt();
            let speed = moved / size / dt;
            if speed < min_speed || speed > r.max_speed {
                continue;
            }
            for b in bodies.iter().filter(|b| b.id != a.id) {
                if !b.bbox.grow(r.reach).contains(k1.at) || a.bbox.intersection(&b.bbox) >= r.same_person * a.bbox.area().min(b.bbox.area()) {
                    continue;
                }
                let height = (k1.at.y - b.bbox.y0) / b.bbox.height().max(1e-6);
                if WRISTS.contains(&limb) && height < r.wrist_low {
                    continue;
                }
                let target = px(b.bbox.center());
                let (dx, dy) = (target.x - b1.x, target.y - b1.y);
                let cos = (mx * dx + my * dy) / (moved * (dx * dx + dy * dy).sqrt()).max(1e-6);
                if cos < r.toward {
                    continue;
                }
                out.push(Strike { at: now, from: a.id, to: b.id, limb, speed, height, toward: cos });
            }
        }
        out
    }

    /// Événements de cette image : à appeler à chaque image analysée, après le suivi (`tracker`
    /// déjà mis à jour pour `now`), avec la taille de l'image analysée en pixels. Un détecteur par
    /// caméra (il retient les squelettes des dernières images et la dernière alerte).
    pub fn update(&mut self, now: f64, tracker: &Tracker, frame_w: f32, frame_h: f32) -> Vec<ViolenceEvent> {
        let bodies: Vec<Body> = tracker.tracks().iter().filter(|t| t.confirmed && t.last_seen >= now).filter_map(|t| Some(Body { id: t.id, bbox: t.bbox, pose: t.pose.as_ref()? })).collect();
        let mut hit: Vec<(u32, u32)> = Vec::new();
        let crowd = bodies.len() > self.rules.max_people;
        for a in bodies.iter().filter(|_| !crowd) {
            // Point de départ du geste : le squelette le plus récent d'au moins `base` secondes.
            let Some(prev) = self.last.get(&a.id).and_then(|h| h.iter().rev().find(|s| now - s.at >= self.rules.base - 1e-6)).filter(|s| now - s.at <= KEEP) else { continue };
            for s in self.blows(a, prev, now, &bodies, frame_w, frame_h) {
                let pair = (s.from.min(s.to), s.from.max(s.to));
                if !hit.contains(&pair) {
                    hit.push(pair);
                }
                if let Some(log) = self.record.as_mut() {
                    log.push(s);
                }
            }
        }
        for b in &bodies {
            let h = self.last.entry(b.id).or_default();
            h.push_back(Sample { at: now, bbox: b.bbox, pose: b.pose.clone() });
            while h.front().is_some_and(|s| now - s.at > KEEP) {
                h.pop_front();
            }
        }
        // Pistes disparues : on les oublie.
        let alive: Vec<u32> = tracker.tracks().iter().map(|t| t.id).collect();
        self.last.retain(|id, _| alive.contains(id));
        self.strikes.retain(|(a, b), _| alive.contains(a) && alive.contains(b));
        let r = &self.rules;
        let mut out = Vec::new();
        for pair in hit {
            let times = self.strikes.entry(pair).or_default();
            if times.back().is_some_and(|t| now - t < r.min_gap) {
                continue;
            }
            times.push_back(now);
            while times.front().is_some_and(|t| now - t > r.window) {
                times.pop_front();
            }
            if (times.len() as u32) < r.strikes || self.alerted.is_some_and(|at| now - at < r.cooldown) {
                continue;
            }
            let (Some(a), Some(b)) = (tracker.track(pair.0), tracker.track(pair.1)) else { continue };
            self.alerted = Some(now);
            let bbox = BBox::new(a.bbox.x0.min(b.bbox.x0), a.bbox.y0.min(b.bbox.y0), a.bbox.x1.max(b.bbox.x1), a.bbox.y1.max(b.bbox.y1));
            out.push(ViolenceEvent { tracks: pair, at: now, bbox, strikes: times.len() as u32 });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::track::Detection;
    use super::super::Keypoint;
    use super::*;

    const W: f32 = 640.0;
    const H: f32 = 480.0;
    /// 8 analyses par seconde, comme dans l'application.
    const STEP: f64 = 0.125;

    /// Personne debout dans la boîte [x, x + 0.12] × [0.2, 0.9], squelette complet de confiance
    /// `score` ; `wrist` / `ankle` : poignet / cheville droits ailleurs que le long du corps.
    fn body(x: f32, wrist: Option<(f32, f32)>, ankle: Option<(f32, f32)>, score: f32) -> Detection {
        let at: [(f32, f32); 17] = [
            (0.06, 0.25),
            (0.05, 0.24),
            (0.07, 0.24),
            (0.04, 0.25),
            (0.08, 0.25),
            (0.03, 0.33),
            (0.09, 0.33),
            (0.02, 0.45),
            (0.10, 0.45),
            (0.02, 0.55),
            (0.10, 0.55),
            (0.04, 0.55),
            (0.08, 0.55),
            (0.04, 0.72),
            (0.08, 0.72),
            (0.04, 0.88),
            (0.08, 0.88),
        ];
        let mut points = at.map(|(dx, y)| Keypoint { at: Point::new(x + dx, y), score });
        if let Some((wx, wy)) = wrist {
            points[10].at = Point::new(wx, wy);
        }
        if let Some((ax, ay)) = ankle {
            points[16].at = Point::new(ax, ay);
        }
        Detection { bbox: BBox::new(x, 0.2, x + 0.12, 0.9), score: 0.9, pose: Some(Pose { points }) }
    }

    fn calm(x: f32) -> Detection {
        body(x, None, None, 0.9)
    }

    /// Fait tourner suivi et détecteur sur une suite d'images (personnes de chaque image).
    fn run(frames: &[Vec<Detection>]) -> Vec<ViolenceEvent> {
        run_with(ViolenceRules::default(), frames)
    }

    fn run_with(rules: ViolenceRules, frames: &[Vec<Detection>]) -> Vec<ViolenceEvent> {
        let mut tracker = Tracker::default();
        let mut d = ViolenceDetector::new(rules);
        let mut out = Vec::new();
        for (i, dets) in frames.iter().enumerate() {
            let now = i as f64 * STEP;
            tracker.update(now, dets);
            out.extend(d.update(now, &tracker, W, H));
        }
        out
    }

    /// Une seconde de calme, puis `n` images où A (à gauche) fait `gesture` à B, en alternance
    /// avec le geste ramené.
    fn scene(b: f32, n: usize, gesture: impl Fn(bool) -> Detection) -> Vec<Vec<Detection>> {
        let mut frames: Vec<Vec<Detection>> = (0..8).map(|_| vec![calm(0.35), calm(b)]).collect();
        frames.extend((0..n).map(|i| vec![gesture(i % 2 == 0), calm(b)]));
        frames
    }

    /// Coup de poing dans le ventre de B (à droite) : poignet ramené à la poitrine, puis tendu.
    fn punch(out: bool) -> Detection {
        body(0.35, Some(if out { (0.57, 0.52) } else { (0.40, 0.42) }), None, 0.9)
    }

    fn kick(out: bool) -> Detection {
        body(0.35, None, Some(if out { (0.55, 0.70) } else { (0.43, 0.88) }), 0.9)
    }

    #[test]
    fn repeated_blows_raise_one_alert() {
        let ev = run(&scene(0.48, 40, punch));
        assert_eq!(ev.len(), 1, "une seule alerte pour toute la bagarre : {ev:?}");
        // Le premier geste part du poignet le long du corps (trop lent) ; ensuite un coup toutes
        // les deux images : le quatrième à la 17e image.
        assert_eq!((ev[0].tracks, ev[0].strikes), ((1, 2), 4));
        assert!((ev[0].at - 16.0 * STEP).abs() < 1e-9, "{ev:?}");
        // Cadre : les deux personnes ensemble.
        assert_eq!(ev[0].bbox, BBox::new(0.35, 0.2, calm(0.48).bbox.x1, 0.9));
        // Coups de pied : de même.
        assert_eq!(run(&scene(0.48, 40, kick)).len(), 1);
    }

    #[test]
    fn a_few_blows_are_not_enough() {
        // Trois coups (après le premier geste, trop lent) : pas d'alerte.
        assert!(run(&scene(0.48, 7, punch)).is_empty());
        // Coups trop espacés (un toutes les 1,5 s) : pas d'alerte non plus.
        let mut frames: Vec<Vec<Detection>> = (0..8).map(|_| vec![calm(0.35), calm(0.48)]).collect();
        for _ in 0..6 {
            frames.extend((0..10).map(|_| vec![calm(0.35), calm(0.48)]));
            frames.push(vec![punch(false), calm(0.48)]);
            frames.push(vec![punch(true), calm(0.48)]);
        }
        assert!(run(&frames).is_empty());
        // Les mêmes, rapprochés : alerte (le geste lui-même compte bien).
        let close: Vec<Vec<Detection>> = frames.iter().filter(|f| f[0] != calm(0.35)).cloned().collect();
        assert_eq!(run(&[&frames[..8], &close[..]].concat()).len(), 1);
    }

    #[test]
    fn fast_gestures_far_from_anyone() {
        // B est loin : A peut gesticuler, applaudir, faire du sport.
        assert!(run(&scene(0.8, 40, punch)).is_empty());
        assert!(run(&scene(0.8, 40, kick)).is_empty());
    }

    #[test]
    fn high_fives_and_hugs_at_head_height() {
        // Main tendue vite vers le haut de B (tape dans la main, accolade) : rien.
        let high_five = |out: bool| body(0.35, Some(if out { (0.52, 0.24) } else { (0.40, 0.42) }), None, 0.9);
        assert!(run(&scene(0.48, 40, high_five)).is_empty());
    }

    #[test]
    fn friendly_punches_on_the_shoulder() {
        // Un copain donne des petits coups de poing dans l'épaule de B, même vite et dix fois de
        // suite : le poing arrive en haut de sa silhouette, rien.
        let shoulder = |out: bool| body(0.35, Some(if out { (0.52, 0.33) } else { (0.38, 0.50) }), None, 0.9);
        assert!(run(&scene(0.48, 40, shoulder)).is_empty());
        // C'est bien la hauteur qui l'écarte : le même geste compterait plus bas.
        assert_eq!(run_with(ViolenceRules { wrist_low: 0.0, ..ViolenceRules::default() }, &scene(0.48, 40, shoulder)).len(), 1);
    }

    #[test]
    fn doubtful_points_and_jumping_points_are_not_blows() {
        // Squelette peu sûr (mêlée, personne à demi cachée).
        let unsure = |out: bool| Detection { pose: punch(out).pose.map(|p| Pose { points: p.points.map(|k| Keypoint { score: 0.45, ..k }) }), ..punch(out) };
        assert!(run(&scene(0.48, 40, unsure)).is_empty());
        // Poignet qui saute d'un bout à l'autre de l'image : point mal placé, pas un geste.
        let jump = |out: bool| body(0.35, Some(if out { (0.57, 0.52) } else { (0.0, 0.45) }), None, 0.9);
        assert!(run(&scene(0.48, 40, jump)).is_empty());
        // Silhouette qui change d'un coup (la piste passe à une autre personne, à demi cachée) :
        // ses points ne sont pas ceux d'un même corps, pas de coup.
        let switched = |out: bool| Detection { bbox: if out { BBox::new(0.40, 0.5, 0.47, 0.9) } else { punch(false).bbox }, ..punch(out) };
        assert!(run(&scene(0.48, 40, switched)).is_empty());
    }

    #[test]
    fn a_ghost_around_someone_is_not_a_second_person() {
        // Une détection fantôme englobe une personne qui gesticule : ses gestes « dans le
        // fantôme » ne comptent pas…
        let ghost = Detection { bbox: BBox::new(0.33, 0.15, 0.62, 0.95), score: 0.9, pose: calm(0.36).pose };
        let mut frames: Vec<Vec<Detection>> = (0..8).map(|_| vec![calm(0.35), ghost.clone()]).collect();
        frames.extend((0..40).map(|i| vec![punch(i % 2 == 0), ghost.clone()]));
        assert!(run(&frames).is_empty());
        // … alors qu'ils compteraient entre deux vraies personnes.
        assert_eq!(run_with(ViolenceRules { same_person: 2.0, ..ViolenceRules::default() }, &frames).len(), 1);
    }

    #[test]
    fn no_blows_in_a_crowd() {
        // Les mêmes coups, avec quatre autres personnes à l'image (match, fête) : rien.
        let mut frames = scene(0.48, 40, punch);
        for f in &mut frames {
            f.extend([0.02, 0.17, 0.65, 0.8].map(calm));
        }
        assert!(run(&frames).is_empty());
        // Avec trois autres seulement (cinq en tout), l'alerte part.
        for f in &mut frames {
            f.pop();
        }
        assert_eq!(run(&frames).len(), 1);
    }

    #[test]
    fn kicks_from_a_body_cut_by_the_frame() {
        // A, coupé par le bas de l'image, « donne des coups de pied » : ses chevilles hors champ
        // sont devinées, rien.
        let cut = |out: bool| Detection { bbox: BBox::new(0.35, 0.3, 0.47, 1.0), ..kick(out) };
        assert!(run(&scene(0.48, 40, cut)).is_empty());
        // Ses poings, eux, comptent toujours.
        let cut = |out: bool| Detection { bbox: BBox::new(0.35, 0.3, 0.47, 1.0), ..punch(out) };
        assert_eq!(run(&scene(0.48, 40, cut)).len(), 1);
    }

    #[test]
    fn walking_side_by_side_is_calm() {
        // Deux personnes marchent ensemble vite (tout le corps bouge), les jambes se balancent.
        let mut frames = Vec::new();
        for i in 0..60 {
            let x = 0.05 + i as f32 * 0.01;
            let swing = if i % 2 == 0 { 0.02 } else { -0.02 };
            frames.push(vec![body(x, None, Some((x + 0.08 + swing, 0.88)), 0.9), body(x + 0.13, None, Some((x + 0.21 - swing, 0.88)), 0.9)]);
        }
        assert!(run(&frames).is_empty());
    }

    /// Coups de pied à 30 analyses par seconde : la cheville part en 4 images, reste 2 images sur
    /// B, revient en 4 images, repose 2 images. `kicks` : nombre de coups.
    fn kicks_at_30_fps(kicks: usize) -> (Vec<ViolenceEvent>, usize) {
        let mut tracker = Tracker::default();
        let mut d = ViolenceDetector::default();
        let (rest, hit) = ((0.43f32, 0.88f32), (0.55f32, 0.70f32));
        let at = |k: f32| (rest.0 + (hit.0 - rest.0) * k, rest.1 + (hit.1 - rest.1) * k);
        let cycle = [0.25, 0.5, 0.75, 1.0, 1.0, 1.0, 0.75, 0.5, 0.25, 0.0, 0.0, 0.0];
        let mut ev = Vec::new();
        for i in 0..30 + 12 * kicks + 30 {
            let now = i as f64 / 30.0;
            let a = match i.checked_sub(30).filter(|k| *k < 12 * kicks) {
                Some(k) => body(0.35, None, Some(at(cycle[k % 12])), 0.9),
                None => calm(0.35),
            };
            tracker.update(now, &[a, calm(0.48)]);
            ev.extend(d.update(now, &tracker, W, H));
        }
        let counted = d.strikes.get(&(1, 2)).map_or(0, |t| t.len());
        (ev, counted)
    }

    #[test]
    fn counting_does_not_depend_on_the_frame_rate() {
        // 30 analyses par seconde : un coup de pied isolé donne plusieurs images « coup » de suite,
        // il compte pour un ou deux, comme à 8 analyses par seconde ; pas d'alerte.
        let (ev, counted) = kicks_at_30_fps(1);
        assert!(ev.is_empty() && (1..=2).contains(&counted), "{ev:?}, {counted} coup(s)");
        // Une vraie série de coups de pied : une alerte.
        assert_eq!(kicks_at_30_fps(8).0.len(), 1);
    }

    #[test]
    fn a_track_that_disappears_is_forgotten() {
        let mut tracker = Tracker::default();
        let mut d = ViolenceDetector::default();
        // Deux coups (le premier geste, trop lent, ne compte pas).
        let frames = scene(0.48, 5, punch);
        for (i, dets) in frames.iter().enumerate() {
            let now = i as f64 * STEP;
            tracker.update(now, dets);
            d.update(now, &tracker, W, H);
        }
        assert_eq!(d.strikes.get(&(1, 2)).map(|t| t.len()), Some(2));
        assert_eq!(d.last.get(&1).map(|h| h.len()), Some(5), "0,5 s d'historique à 8 analyses par seconde");
        // Plus personne pendant 3 s : pistes perdues, plus rien en mémoire.
        let now = frames.len() as f64 * STEP + 3.0;
        tracker.update(now, &[]);
        d.update(now, &tracker, W, H);
        assert!(d.last.is_empty() && d.strikes.is_empty());
    }
}
