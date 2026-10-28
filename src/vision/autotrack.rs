//! Suivi automatique d'une personne par une caméra motorisée : la caméra tourne d'elle-même
//! (orientation et inclinaison) pour garder au centre la personne suivie.
//!
//! Le régulateur ne voit que les pistes (`track`) et l'heure ; il rend une vitesse ONVIF
//! (`ContinuousMove`, −1…1) ou un arrêt (`Stop`). Points clés :
//! - **retard compensé** : entre la prise d'une image et le moteur il s'écoule 0,3 à 0,5 s
//!   (flux, analyse, commande). L'écart mesuré sur une image est corrigé du chemin que la
//!   caméra a fait depuis (prédicteur de Smith) : sans cela, elle continuerait de tourner alors
//!   que la personne est déjà au centre, puis repartirait dans l'autre sens (oscillation) ;
//! - zone morte au centre (avec hystérésis), gain proportionnel, part de la vitesse de la
//!   personne, vitesses limitées et arrondies, quelques commandes par seconde au plus ;
//! - une seule personne suivie : la plus grande au départ, puis toujours la même piste. Perdue,
//!   la caméra s'arrête net et attend ; personne d'autre n'est suivi avant un délai ;
//! - retour à la position de départ en refaisant les mouvements à l'envers, à la même vitesse
//!   (la caméra n'a ni position absolue ni position mémorisée), puis petite correction d'après
//!   l'image du départ ([`home_nudge`]) : un vrai moteur ne refait pas exactement le même chemin ;
//! - aucune commande pendant qu'un utilisateur pilote la caméra à la main.
//!
//! Sens des vitesses : celui de l'image (`pan` > 0 : vers la droite, `tilt` > 0 : vers le haut).
//! L'inversion des axes propre à une caméra (`CameraConfig::invert_pan` / `invert_tilt`) est
//! faite par l'appelant au moment d'envoyer la commande ([`PtzCommand::inverted`]).

use super::geom::BBox;
use super::track::Track;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

/// Pas des vitesses envoyées. Peu de vitesses différentes : le retour au départ, qui refait
/// chaque mouvement à l'envers à la même vitesse, reste juste même si la vitesse réelle de la
/// caméra n'est pas proportionnelle à la vitesse demandée.
pub const SPEED_STEP: f32 = 0.05;

/// Sens du glissement de la personne dans l'image quand la caméra tourne : orienter vers la
/// droite la pousse vers la gauche (écart x qui baisse), incliner vers le haut la pousse vers
/// le bas (écart y qui monte).
const SIGN: [f32; 2] = [-1.0, 1.0];

/// Une fois partie, la caméra s'arrête quand l'écart prévu passe sous cette part de la zone morte.
const STOP_FRACTION: f32 = 0.4;

/// Écart minimal entre deux vitesses envoyées (sauf départ, arrêt ou changement de sens).
const SPEED_HYSTERESIS: f32 = 0.1;

/// Une piste vue depuis moins longtemps que ça est « présente » (secondes).
const FRESH: f64 = 0.3;

/// Fenêtre de mesure de la vitesse de la personne (secondes) et vitesse négligée en deçà
/// (fractions d'image par seconde : le tremblement des boîtes).
const VELOCITY_WINDOW: f64 = 0.8;
const VELOCITY_FLOOR: f32 = 0.05;

/// Freinage d'un axe à chaque changement de sens (réglage faux : caméra plus rapide ou plus en
/// retard que prévu), au plus bas, et relâchement après chaque période calme (secondes).
const DAMPING_STEP: f32 = 0.6;
const DAMPING_MIN: f32 = 0.25;
const DAMPING_CALM: f64 = 4.0;

/// Mouvement de retour plus court ignoré (secondes) : la caméra ne le ferait pas fidèlement.
const MIN_RETURN: f64 = 0.08;

/// Historique des commandes gardé au-delà du retard (secondes).
const HISTORY_KEEP: f64 = 3.0;

/// Commande à envoyer à la caméra.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PtzCommand {
    /// Mouvement continu (ONVIF `ContinuousMove`), vitesses −1…1.
    Move { pan: f32, tilt: f32 },
    /// Arrêt (ONVIF `Stop`).
    Stop,
}

impl PtzCommand {
    /// La même commande pour une caméra aux axes inversés.
    pub fn inverted(self, invert_pan: bool, invert_tilt: bool) -> PtzCommand {
        match self {
            PtzCommand::Move { pan, tilt } => PtzCommand::Move { pan: if invert_pan { -pan } else { pan }, tilt: if invert_tilt { -tilt } else { tilt } },
            PtzCommand::Stop => PtzCommand::Stop,
        }
    }
}

/// Sens d'un mouvement, en mots.
pub fn direction(pan: f32, tilt: f32) -> &'static str {
    match (pan.partial_cmp(&0.0), tilt.partial_cmp(&0.0)) {
        (Some(std::cmp::Ordering::Less), Some(std::cmp::Ordering::Greater)) => "haut-gauche",
        (Some(std::cmp::Ordering::Greater), Some(std::cmp::Ordering::Greater)) => "haut-droite",
        (Some(std::cmp::Ordering::Less), Some(std::cmp::Ordering::Less)) => "bas-gauche",
        (Some(std::cmp::Ordering::Greater), Some(std::cmp::Ordering::Less)) => "bas-droite",
        (Some(std::cmp::Ordering::Less), _) => "gauche",
        (Some(std::cmp::Ordering::Greater), _) => "droite",
        (_, Some(std::cmp::Ordering::Greater)) => "haut",
        (_, Some(std::cmp::Ordering::Less)) => "bas",
        _ => "immobile",
    }
}

/// Durée en secondes, à la française (« 1,4 s »).
fn secs_fr(s: f64) -> String {
    format!("{s:.1} s").replace('.', ",")
}

/// Réglages du suivi automatique.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoTrackSettings {
    /// Vitesse maximale envoyée (0…1). 0,3 fait déjà glisser l'image d'un quart de sa largeur
    /// par seconde sur la Ranger 2C ; plus vite, un retard mal réglé fait dépasser le centre.
    pub max_speed: f32,
    /// Plus petite vitesse qui fait vraiment tourner la caméra (la Ranger 2C ne bouge pas à 0,10,
    /// même une seconde entière ; elle bouge à 0,15).
    pub min_speed: f32,
    /// Glissement de l'image à la vitesse 1 : largeurs d'image par seconde (orientation) et
    /// hauteurs d'image par seconde (inclinaison), mesurés par `homesmart-cli cam follow
    /// --calibrate`. Trop haut, la caméra s'arrête avant le centre et s'y reprend en plusieurs
    /// fois ; trop bas, elle dépasse le centre. Par défaut : l'orientation de l'Imou Ranger 2C
    /// (IPC-K2EC-3H1W, flux fluide), mesurée de 0,85 à 1,13 selon la vitesse ; son inclinaison
    /// ne répond pas aux commandes ONVIF, d'où la même valeur, prudente.
    pub pan_rate: f32,
    pub tilt_rate: f32,
    /// Retard de toute la boucle (secondes) : prise de l'image, flux, analyse, commande, moteur.
    pub latency: f32,
    /// Part de l'écart rattrapée par seconde (1/s).
    pub gain: f32,
    /// Part de la vitesse de la personne ajoutée à la commande (0 : aucune, 1 : toute).
    pub velocity_gain: f32,
    /// Zone morte autour du centre, en fraction de la largeur et de la hauteur : la caméra ne
    /// bouge pas tant que la personne y reste.
    pub dead_zone: (f32, f32),
    /// Point visé dans la boîte de la personne, depuis le haut (0,4 : vers la poitrine).
    pub aim_y: f32,
    /// Suivre aussi en hauteur (inclinaison).
    pub tilt: bool,
    /// Écart minimal entre deux commandes (secondes). L'arrêt, lui, part tout de suite.
    pub min_interval: f64,
    /// Renvoyer la commande en cours au moins toutes les N secondes (certaines caméras arrêtent
    /// d'elles-mêmes un mouvement continu).
    pub refresh: f64,
    /// Personne plus vue depuis N secondes : la caméra s'arrête (elle est peut-être cachée).
    pub hold_after: f64,
    /// Plus vue depuis N secondes : perdue.
    pub lost_after: f64,
    /// Après une perte, délai avant de suivre quelqu'un d'autre (la même piste est reprise
    /// tout de suite si elle revient).
    pub reacquire_after: f64,
    /// Retour à la position de départ N secondes après la perte (`None` : la caméra reste).
    pub return_after: Option<f64>,
    /// Pause après un mouvement commandé à la main (secondes).
    pub manual_pause: f64,
    /// Plus petite boîte suivie (fraction de l'image).
    pub min_area: f32,
    /// Lissage de la position de la personne (secondes).
    pub smoothing: f64,
}

impl Default for AutoTrackSettings {
    fn default() -> Self {
        AutoTrackSettings {
            max_speed: 0.3,
            min_speed: 0.15,
            pan_rate: 0.9,
            tilt_rate: 0.9,
            latency: 0.45,
            gain: 1.2,
            velocity_gain: 0.6,
            dead_zone: (0.08, 0.12),
            aim_y: 0.4,
            tilt: true,
            min_interval: 0.25,
            refresh: 1.0,
            hold_after: 0.5,
            lost_after: 2.0,
            reacquire_after: 2.0,
            return_after: None,
            manual_pause: 10.0,
            min_area: 0.004,
            smoothing: 0.15,
        }
    }
}

/// Où en est le suivi.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Phase {
    /// Personne à suivre.
    Idle,
    /// Personne suivie (numéro de piste).
    Following(u32),
    /// Personne suivie perdue à l'instant `since` : caméra arrêtée, on attend.
    Lost { track: u32, since: f64 },
    /// Retour à la position de départ.
    Returning,
    /// Pilotage à la main : aucune commande jusqu'à `until`.
    Manual { until: f64 },
}

/// Ce qui mérite d'être dit (journal, affichage).
#[derive(Clone, Debug, PartialEq)]
pub enum AutoTrackEvent {
    /// Une personne est suivie (numéro de piste).
    Acquired(u32),
    /// La personne suivie est perdue : la caméra est arrêtée.
    Lost(u32),
    /// Un mouvement commence (sens).
    Moving(&'static str),
    /// Un mouvement s'arrête : sens successifs, durée.
    Moved { trail: Vec<&'static str>, secs: f64 },
    /// Retour au départ commencé : durée prévue des mouvements à l'envers.
    Returning { secs: f64 },
    /// Revenue à la position de départ.
    Home,
    /// Caméra pilotée à la main : pause.
    Manual,
}

impl AutoTrackEvent {
    pub fn describe(&self) -> String {
        match self {
            AutoTrackEvent::Acquired(id) => format!("personne n° {id} suivie"),
            AutoTrackEvent::Lost(id) => format!("personne n° {id} perdue, caméra arrêtée"),
            AutoTrackEvent::Moving(dir) => format!("orientation : {dir}"),
            AutoTrackEvent::Moved { trail, secs } => format!("arrêt après {} ({})", secs_fr(*secs), trail.join(" → ")),
            AutoTrackEvent::Returning { secs } => format!("retour à la position de départ ({} de mouvements à l'envers)", secs_fr(*secs)),
            AutoTrackEvent::Home => "revenue à la position de départ".into(),
            AutoTrackEvent::Manual => "caméra pilotée à la main : suivi en pause".into(),
        }
    }
}

/// Derniers chiffres du suivi (affichage, mesures).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Aim {
    pub track: u32,
    pub bbox: BBox,
    /// Écart au centre sur la dernière image (fractions de la largeur et de la hauteur ;
    /// x > 0 : à droite, y > 0 : en bas).
    pub error: (f32, f32),
    /// Écart prévu une fois faits les mouvements déjà commandés.
    pub predicted: (f32, f32),
    /// Vitesse de la personne (fractions d'image par seconde).
    pub velocity: (f32, f32),
}

/// La personne suivie, dans le repère de la position de départ.
#[derive(Clone, Debug)]
struct Estimate {
    track: u32,
    /// Instant de la dernière détection prise en compte.
    seen: f64,
    /// Écart au centre qu'elle aurait si la caméra n'avait pas bougé, lissé.
    world: [f32; 2],
    /// Mesures récentes (non lissées), pour la vitesse.
    recent: VecDeque<(f64, [f32; 2])>,
    velocity: [f32; 2],
    error: [f32; 2],
    bbox: BBox,
}

/// Le régulateur. `now` : secondes, sur la même horloge que le [`super::track::Tracker`].
#[derive(Clone, Debug)]
pub struct AutoTracker {
    pub settings: AutoTrackSettings,
    phase: Phase,
    /// Vitesses en cours (arrondies ; [0, 0] : arrêtée) et instant du dernier envoi.
    current: [f32; 2],
    sent_at: f64,
    /// Depuis quand les mouvements faits sont comptés pour la vitesse en cours.
    counted_to: f64,
    /// Dernier arrêt : on ne repart qu'après avoir vu une image prise depuis.
    stopped_at: f64,
    /// Chemin de l'image commandé, par morceaux : (début, vitesses, chemin au début), en
    /// fractions d'image.
    history: VecDeque<(f64, [f32; 2], [f64; 2])>,
    /// Mouvements faits depuis la position de départ : (axe, palier de vitesse) → secondes,
    /// signées.
    travel: BTreeMap<(usize, u32), f64>,
    est: Option<Estimate>,
    /// Axes en mouvement (hystérésis de la zone morte).
    active: [bool; 2],
    /// Freinage de chaque axe (1 : aucun), sens de sa dernière commande de suivi, dernier
    /// changement de sens (ou dernier relâchement).
    damping: [f32; 2],
    last_dir: [f32; 2],
    calm_since: [f64; 2],
    /// Mouvement en cours, pour le journal : début et sens successifs.
    moving: Option<(f64, Vec<&'static str>)>,
    /// Fin du mouvement de retour en cours, et sens du départ au début du retour (paliers
    /// dans ce sens d'abord).
    return_end: Option<f64>,
    home_dir: [f64; 2],
    /// Retour demandé par l'appelant ([`Self::go_home`]) : personne n'est suivi avant d'être revenu.
    homing: bool,
    events: Vec<AutoTrackEvent>,
    aim: Option<Aim>,
}

impl AutoTracker {
    pub fn new(settings: AutoTrackSettings) -> Self {
        AutoTracker {
            settings,
            phase: Phase::Idle,
            current: [0.0; 2],
            sent_at: f64::NEG_INFINITY,
            counted_to: 0.0,
            stopped_at: f64::NEG_INFINITY,
            history: VecDeque::new(),
            travel: BTreeMap::new(),
            est: None,
            active: [false; 2],
            damping: [1.0; 2],
            last_dir: [0.0; 2],
            calm_since: [0.0; 2],
            moving: None,
            return_end: None,
            home_dir: [0.0; 2],
            homing: false,
            events: Vec::new(),
            aim: None,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Piste suivie.
    pub fn target(&self) -> Option<u32> {
        match self.phase {
            Phase::Following(id) => Some(id),
            _ => None,
        }
    }

    pub fn is_moving(&self) -> bool {
        self.current != [0.0; 2]
    }

    /// Vitesses en cours (orientation, inclinaison).
    pub fn command(&self) -> (f32, f32) {
        (self.current[0], self.current[1])
    }

    pub fn aim(&self) -> Option<Aim> {
        self.aim
    }

    /// Freinage en cours (orientation, inclinaison) : 1 = aucun ; plus bas après des
    /// changements de sens rapprochés (signe d'un réglage à revoir : `--calibrate`).
    pub fn damping(&self) -> (f32, f32) {
        (self.damping[0], self.damping[1])
    }

    /// Événements depuis le dernier appel (à inscrire au journal).
    pub fn take_events(&mut self) -> Vec<AutoTrackEvent> {
        std::mem::take(&mut self.events)
    }

    /// Mouvements faits depuis la position de départ (vitesse × secondes, par axe).
    pub fn travel(&self) -> (f32, f32) {
        let mut t = [0f64; 2];
        for (&(axis, level), &s) in &self.travel {
            t[axis] += level as f64 * SPEED_STEP as f64 * s;
        }
        (t[0] as f32, t[1] as f32)
    }

    /// Prochain instant où le régulateur doit être appelé même sans nouvelle image (fin d'un
    /// mouvement de retour, renvoi de la commande, fin d'une attente).
    pub fn wake_at(&self) -> Option<f64> {
        let refresh = self.is_moving().then_some(self.sent_at + self.settings.refresh);
        let phase = match self.phase {
            Phase::Returning => self.return_end,
            Phase::Lost { since, .. } => self.settings.return_after.filter(|_| self.has_travel()).map(|a| since + a),
            Phase::Manual { until } => Some(until),
            _ => None,
        };
        [refresh, phase].into_iter().flatten().reduce(f64::min)
    }

    /// Un utilisateur vient de piloter la caméra : pause, et la position qu'il choisit devient
    /// la nouvelle position de départ. À appeler à chaque commande à la main.
    pub fn manual(&mut self, now: f64) {
        // Sa commande remplace la nôtre : le mouvement en cours n'est plus le nôtre.
        self.count_travel(now);
        if let Some((since, trail)) = self.moving.take() {
            self.events.push(AutoTrackEvent::Moved { trail, secs: now - since });
        }
        let path = self.path_at(now);
        self.history.push_back((now, [0.0; 2], path));
        self.current = [0.0; 2];
        self.travel.clear();
        (self.est, self.aim, self.active, self.return_end, self.homing) = (None, None, [false; 2], None, false);
        if !matches!(self.phase, Phase::Manual { .. }) {
            self.events.push(AutoTrackEvent::Manual);
        }
        self.phase = Phase::Manual { until: now + self.settings.manual_pause };
    }

    /// Retour à la position de départ tout de suite (fin du suivi, essai terminé).
    pub fn go_home(&mut self, now: f64) -> Option<PtzCommand> {
        if matches!(self.phase, Phase::Manual { .. }) {
            return None;
        }
        (self.est, self.aim, self.active) = (None, None, [false; 2]);
        self.homing = true;
        if self.phase == Phase::Returning {
            return self.return_step(now);
        }
        self.count_travel(now);
        if !self.has_travel() {
            (self.phase, self.homing) = (Phase::Idle, false);
            return self.send(now, [0.0; 2], true);
        }
        self.start_return(now)
    }

    /// Suivi désactivé : arrêt de la caméra là où elle est.
    pub fn stop(&mut self, now: f64) -> Option<PtzCommand> {
        if matches!(self.phase, Phase::Manual { .. }) {
            return None;
        }
        (self.phase, self.est, self.aim, self.active, self.return_end, self.homing) = (Phase::Idle, None, None, [false; 2], None, false);
        self.send(now, [0.0; 2], true)
    }

    /// À appeler après chaque analyse (pistes du `Tracker`), et à [`Self::wake_at`].
    pub fn update(&mut self, now: f64, tracks: &[Track]) -> Option<PtzCommand> {
        if let Phase::Manual { until } = self.phase {
            if now < until {
                return None;
            }
            self.phase = Phase::Idle;
        }
        // Personne suivie : toujours la même piste, jusqu'à sa perte.
        if let Phase::Following(id) = self.phase {
            let lost_after = self.settings.lost_after;
            match tracks.iter().find(|t| t.id == id && now - t.last_seen <= lost_after) {
                Some(t) => return self.follow(now, t),
                None => {
                    self.phase = Phase::Lost { track: id, since: now };
                    (self.est, self.aim, self.active) = (None, None, [false; 2]);
                    self.events.push(AutoTrackEvent::Lost(id));
                    return self.send(now, [0.0; 2], true);
                }
            }
        }
        if let Some(t) = self.pick(now, tracks).filter(|_| !self.homing) {
            self.return_end = None;
            self.phase = Phase::Following(t.id);
            self.events.push(AutoTrackEvent::Acquired(t.id));
            return self.follow(now, t);
        }
        match self.phase {
            Phase::Lost { since, .. } => {
                if self.settings.return_after.is_some_and(|after| now - since >= after) {
                    self.count_travel(now);
                    if self.has_travel() {
                        return self.start_return(now);
                    }
                }
                self.send(now, [0.0; 2], true)
            }
            Phase::Returning => self.return_step(now),
            _ => self.send(now, [0.0; 2], true),
        }
    }

    /// Personne à suivre : la même piste si elle revient juste après sa perte, sinon (après un
    /// délai) la plus grande.
    fn pick<'a>(&self, now: f64, tracks: &'a [Track]) -> Option<&'a Track> {
        let min_area = self.settings.min_area;
        let present = |t: &&Track| t.confirmed && now - t.last_seen <= FRESH && t.bbox.area() >= min_area;
        if let Phase::Lost { track, since } = self.phase {
            if let Some(t) = tracks.iter().filter(present).find(|t| t.id == track) {
                return Some(t);
            }
            if now - since < self.settings.reacquire_after {
                return None;
            }
        }
        tracks.iter().filter(present).max_by(|a, b| a.bbox.area().total_cmp(&b.bbox.area()))
    }

    fn follow(&mut self, now: f64, t: &Track) -> Option<PtzCommand> {
        let s = self.settings.clone();
        let lat = s.latency as f64;
        let rates = [s.pan_rate.max(1e-3), s.tilt_rate.max(1e-3)];
        // Nouvelle détection : où serait la personne si la caméra n'avait pas bougé ? L'image
        // montre la caméra telle qu'elle était `latency` secondes avant l'analyse.
        let new = self.est.as_ref().is_none_or(|e| e.track != t.id || t.last_seen > e.seen + 1e-9);
        if new {
            let error = measure(&t.bbox, s.aim_y);
            let path = self.path_at(t.last_seen - lat);
            let world = [error[0] - SIGN[0] * path[0] as f32, error[1] - SIGN[1] * path[1] as f32];
            let cap = [s.max_speed * rates[0], s.max_speed * rates[1]];
            match self.est.as_mut().filter(|e| e.track == t.id) {
                Some(e) => {
                    let a = (1.0 - (-(t.last_seen - e.seen) / s.smoothing.max(1e-3)).exp()) as f32;
                    for i in 0..2 {
                        e.world[i] += (world[i] - e.world[i]) * a;
                    }
                    e.recent.push_back((t.last_seen, world));
                    while e.recent.front().is_some_and(|(at, _)| *at < t.last_seen - VELOCITY_WINDOW) {
                        e.recent.pop_front();
                    }
                    e.velocity = velocity(&e.recent, cap);
                    (e.seen, e.error, e.bbox) = (t.last_seen, error, t.bbox);
                }
                None => {
                    self.est = Some(Estimate { track: t.id, seen: t.last_seen, world, recent: VecDeque::from([(t.last_seen, world)]), velocity: [0.0; 2], error, bbox: t.bbox });
                    self.active = [false; 2];
                }
            }
        }
        let e = self.est.clone()?;
        let path_now = self.path_at(now);
        let horizon = ((now - e.seen).min(s.hold_after) + lat) as f32;
        let mut cmd = [0f32; 2];
        let mut predicted = [0f32; 2];
        let held = now - e.seen > s.hold_after;
        for i in 0..2 {
            if self.damping[i] < 1.0 && now - self.calm_since[i] >= DAMPING_CALM {
                (self.damping[i], self.calm_since[i]) = ((self.damping[i] * 1.25).min(1.0), now);
            }
        }
        for i in 0..2 {
            let v = e.velocity[i] * s.velocity_gain;
            let p = e.world[i] + v * horizon + SIGN[i] * path_now[i] as f32;
            predicted[i] = p;
            if (i == 1 && !s.tilt) || held {
                self.active[i] = false;
                continue;
            }
            // Hystérésis : on part au-delà de la zone morte, on s'arrête bien en deçà — sauf si la
            // personne s'éloigne du centre en marchant (vue de ce côté sur la dernière image) :
            // on l'accompagne au lieu de la laisser repartir vers le bord. Une vitesse qui ne
            // colle pas à l'image vient souvent d'un retard ou d'une vitesse de caméra mal réglés.
            let dz = if i == 0 { s.dead_zone.0 } else { s.dead_zone.1 };
            let near = p.abs() < dz * STOP_FRACTION;
            let walking = v != 0.0 && v.signum() == e.error[i].signum();
            if self.active[i] {
                self.active[i] = !near || walking;
            } else {
                self.active[i] = p.abs() > dz;
            }
            if !self.active[i] {
                continue;
            }
            let q = (s.gain * p + v) * self.damping[i];
            // Jamais à l'opposé de l'écart vu sur la dernière image : un dépassement seulement
            // prévu vient souvent d'un retard mal réglé ; un vrai se verra sur l'image suivante.
            if q.signum() != e.error[i].signum() {
                continue;
            }
            let want = -SIGN[i] * q / rates[i];
            // Presque au centre, on suit son pas sans la devancer.
            if near && want.abs() < s.min_speed {
                continue;
            }
            cmd[i] = quantize(want, s.min_speed, (s.max_speed * self.damping[i]).max(s.min_speed));
        }
        self.aim = Some(Aim {
            track: e.track,
            bbox: e.bbox,
            error: (e.error[0], e.error[1]),
            predicted: (predicted[0], predicted[1]),
            velocity: (e.velocity[0], e.velocity[1]),
        });
        // Petite retouche de vitesse : on garde la commande en cours (moins de commandes, moins
        // de paliers différents à refaire au retour).
        let keep = (0..2).all(|i| {
            let (a, b) = (self.current[i], cmd[i]);
            (a == 0.0) == (b == 0.0) && a.signum() == b.signum() && (a - b).abs() < SPEED_HYSTERESIS - 1e-4
        });
        let cmd = if keep { self.current } else { cmd };
        // Arrêtée : on regarde d'abord où l'arrêt l'a laissée (une image prise après l'arrêt)
        // avant de repartir ; sinon une caméra plus rapide ou plus en retard que réglé fait des
        // allers-retours à l'aveugle autour du centre.
        if self.current == [0.0; 2] && now - self.stopped_at < lat {
            return None;
        }
        let sent = self.send(now, cmd, false);
        if let Some(PtzCommand::Move { pan, tilt }) = sent {
            for (i, u) in [pan, tilt].into_iter().enumerate().filter(|(_, u)| *u != 0.0) {
                if self.last_dir[i] != 0.0 && u.signum() != self.last_dir[i] {
                    (self.damping[i], self.calm_since[i]) = ((self.damping[i] * DAMPING_STEP).max(DAMPING_MIN), now);
                }
                self.last_dir[i] = u.signum();
            }
        }
        sent
    }

    fn start_return(&mut self, now: f64) -> Option<PtzCommand> {
        self.phase = Phase::Returning;
        self.return_end = None;
        let secs = self.travel.values().filter(|s| s.abs() >= MIN_RETURN).map(|s| s.abs()).sum();
        self.events.push(AutoTrackEvent::Returning { secs });
        self.home_dir = [0.0; 2];
        for (&(axis, level), &s) in &self.travel {
            self.home_dir[axis] -= level as f64 * s;
        }
        self.return_step(now)
    }

    /// Retour : chaque palier de vitesse refait à l'envers, axe par axe, le temps qu'il a duré ;
    /// d'abord ceux qui vont vers le départ, puis les autres (un seul changement de sens par axe).
    fn return_step(&mut self, now: f64) -> Option<PtzCommand> {
        if self.return_end.is_some_and(|end| now < end) {
            return self.send(now, self.current, false);
        }
        self.count_travel(now);
        let home = self.home_dir;
        let next = self
            .travel
            .iter()
            .filter(|(_, s)| s.abs() >= MIN_RETURN)
            .min_by_key(|(&(axis, level), &s)| (axis, -s.signum() != home[axis].signum(), std::cmp::Reverse(level)))
            .map(|(&(axis, level), &s)| (axis, level, s));
        match next {
            Some((axis, level, s)) => {
                let mut cmd = [0.0; 2];
                cmd[axis] = -(s.signum() as f32) * level as f32 * SPEED_STEP;
                self.return_end = Some(now + s.abs());
                self.send(now, cmd, true)
            }
            None => {
                (self.return_end, self.homing) = (None, false);
                self.travel.clear();
                self.phase = Phase::Idle;
                self.events.push(AutoTrackEvent::Home);
                self.send(now, [0.0; 2], true)
            }
        }
    }

    fn has_travel(&self) -> bool {
        self.travel.values().any(|s| s.abs() >= MIN_RETURN)
    }

    /// Envoie `cmd` si elle change (l'arrêt et `force` tout de suite, le reste au plus toutes
    /// les `min_interval` secondes) ; renvoie la commande en cours toutes les `refresh` secondes.
    fn send(&mut self, now: f64, cmd: [f32; 2], force: bool) -> Option<PtzCommand> {
        let stopped = cmd == [0.0; 2];
        if cmd == self.current {
            if !stopped && now - self.sent_at >= self.settings.refresh {
                self.sent_at = now;
                return Some(PtzCommand::Move { pan: cmd[0], tilt: cmd[1] });
            }
            return None;
        }
        if !stopped && !force && now - self.sent_at < self.settings.min_interval {
            return None;
        }
        self.apply(now, cmd);
        Some(if stopped { PtzCommand::Stop } else { PtzCommand::Move { pan: cmd[0], tilt: cmd[1] } })
    }

    fn apply(&mut self, now: f64, cmd: [f32; 2]) {
        self.count_travel(now);
        let path = self.path_at(now);
        self.history.push_back((now, cmd, path));
        let keep_from = now - self.settings.latency as f64 - HISTORY_KEEP;
        while self.history.len() > 1 && self.history[1].0 < keep_from {
            self.history.pop_front();
        }
        let dir = direction(cmd[0], cmd[1]);
        if cmd == [0.0; 2] {
            self.stopped_at = now;
            if let Some((since, trail)) = self.moving.take() {
                self.events.push(AutoTrackEvent::Moved { trail, secs: now - since });
            }
        } else if let Some((_, trail)) = self.moving.as_mut() {
            if trail.last() != Some(&dir) {
                trail.push(dir);
            }
        } else {
            self.moving = Some((now, vec![dir]));
            self.events.push(AutoTrackEvent::Moving(dir));
        }
        (self.current, self.sent_at) = (cmd, now);
    }

    /// Ajoute aux mouvements faits la commande en cours, jusqu'à `now`.
    fn count_travel(&mut self, now: f64) {
        let dt = now - self.counted_to;
        if dt > 0.0 {
            for (axis, &u) in self.current.iter().enumerate() {
                if u != 0.0 {
                    let level = (u.abs() / SPEED_STEP).round() as u32;
                    *self.travel.entry((axis, level)).or_default() += dt * u.signum() as f64;
                }
            }
        }
        self.counted_to = now;
    }

    /// Chemin de l'image commandé jusqu'à `t` (fractions de largeur et de hauteur).
    fn path_at(&self, t: f64) -> [f64; 2] {
        let rates = [self.settings.pan_rate as f64, self.settings.tilt_rate as f64];
        match self.history.iter().rev().find(|h| h.0 <= t) {
            Some(&(start, u, base)) => [base[0] + rates[0] * u[0] as f64 * (t - start), base[1] + rates[1] * u[1] as f64 * (t - start)],
            None => self.history.front().map_or([0.0; 2], |h| h.2),
        }
    }
}

/// Écart entre le point visé et le centre de l'image.
fn measure(b: &BBox, aim_y: f32) -> [f32; 2] {
    let x = b.center().x - 0.5;
    // Personne qui remplit presque toute la hauteur : incliner n'y changerait rien.
    let y = if b.height() >= 0.85 { 0.0 } else { b.y0 + aim_y * b.height() - 0.5 };
    [x, y]
}

/// Vitesse (pente des moindres carrés) sur les mesures récentes, nulle si elles couvrent trop
/// peu de temps ou si elle se confond avec le tremblement des boîtes ; plafonnée à ce que la
/// caméra peut suivre.
fn velocity(recent: &VecDeque<(f64, [f32; 2])>, cap: [f32; 2]) -> [f32; 2] {
    let (Some(first), Some(last)) = (recent.front(), recent.back()) else { return [0.0; 2] };
    if recent.len() < 3 || last.0 - first.0 < 0.35 {
        return [0.0; 2];
    }
    let n = recent.len() as f64;
    let mt = recent.iter().map(|r| r.0).sum::<f64>() / n;
    let mut v = [0f32; 2];
    for (i, vi) in v.iter_mut().enumerate() {
        let mw = recent.iter().map(|r| r.1[i] as f64).sum::<f64>() / n;
        let num: f64 = recent.iter().map(|r| (r.0 - mt) * (r.1[i] as f64 - mw)).sum();
        let den: f64 = recent.iter().map(|r| (r.0 - mt).powi(2)).sum();
        let slope = if den > 0.0 { (num / den) as f32 } else { 0.0 };
        *vi = if slope.abs() < VELOCITY_FLOOR { 0.0 } else { slope.clamp(-cap[i], cap[i]) };
    }
    v
}

/// Petit mouvement qui rattrape l'écart restant avec l'image du départ, une fois les mouvements
/// refaits à l'envers : `shift` est le glissement du décor depuis le départ, en pixels, mesuré
/// par [`Profiles::shift_to`] sur une image de `size` pixels. Rend la commande et sa durée
/// (l'axe le plus décalé d'abord, à la vitesse `speed`), ou `None` si l'écart ne dépasse pas
/// `tolerance` pixels. À refaire, image à l'appui, tant qu'il en rend un (deux ou trois fois).
pub fn home_nudge(shift: (f32, f32), size: (usize, usize), s: &AutoTrackSettings, speed: f32, tolerance: f32) -> Option<(PtzCommand, f64)> {
    let px = [shift.0, shift.1];
    let frac = [shift.0 / size.0.max(1) as f32, shift.1 / size.1.max(1) as f32];
    let rates = [s.pan_rate.max(1e-3), s.tilt_rate.max(1e-3)];
    let axis = (0..2).filter(|&i| px[i].abs() > tolerance && (i == 0 || s.tilt)).max_by(|&a, &b| px[a].abs().total_cmp(&px[b].abs()))?;
    let speed = quantize(speed, s.min_speed, 1.0);
    // Un objet du décor décalé se ramène comme une personne : vers lui.
    let mut cmd = [0.0; 2];
    cmd[axis] = -SIGN[axis] * frac[axis].signum() * speed;
    Some((PtzCommand::Move { pan: cmd[0], tilt: cmd[1] }, (frac[axis].abs() / (rates[axis] * speed)) as f64))
}

/// Vitesse arrondie au pas, entre la vitesse minimale et la vitesse maximale (signe gardé).
fn quantize(u: f32, min: f32, max: f32) -> f32 {
    if u == 0.0 {
        return 0.0;
    }
    let lo = min.clamp(SPEED_STEP, 1.0);
    let a = u.abs().clamp(lo, max.clamp(SPEED_STEP, 1.0).max(lo));
    let level = (a / SPEED_STEP).round().max(1.0);
    level * SPEED_STEP * u.signum()
}

/// Profils d'une image (luminosité moyenne de chaque colonne et de chaque ligne) : assez pour
/// mesurer de combien l'image a glissé entre deux instants (étalonnage, vérification du retour
/// au départ), sans rien garder de l'image elle-même.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Profiles {
    pub cols: Vec<f32>,
    pub rows: Vec<f32>,
}

impl Profiles {
    pub fn from_rgba(width: usize, height: usize, rgba: &[u8]) -> Profiles {
        let (mut cols, mut rows) = (vec![0f32; width], vec![0f32; height]);
        if width == 0 || height == 0 || rgba.len() < width * height * 4 {
            return Profiles { cols, rows };
        }
        for (y, line) in rgba.chunks_exact(width * 4).take(height).enumerate() {
            let mut sum = 0f32;
            for (x, p) in line.chunks_exact(4).enumerate() {
                let l = 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32;
                cols[x] += l;
                sum += l;
            }
            rows[y] = sum / width as f32;
        }
        cols.iter_mut().for_each(|c| *c /= height as f32);
        Profiles { cols, rows }
    }

    /// Glissement du décor de `self` à `later`, en pixels (x > 0 : vers la droite, y > 0 : vers
    /// le bas), cherché jusqu'à `max` pixels, et ressemblance des profils (0…1 ; sous 0,5 la
    /// mesure est douteuse).
    pub fn shift_to(&self, later: &Profiles, max: usize) -> ((f32, f32), f32) {
        let (dx, qx) = shift_1d(&self.cols, &later.cols, max);
        let (dy, qy) = shift_1d(&self.rows, &later.rows, max);
        ((dx, dy), qx.min(qy))
    }
}

/// Décalage `d` tel que `b[i] ≈ a[i − d]` : corrélation normalisée des variations des profils,
/// affinée entre deux pixels.
fn shift_1d(a: &[f32], b: &[f32], max: usize) -> (f32, f32) {
    let (da, db) = (edges(a), edges(b));
    let n = da.len().min(db.len());
    if n < 8 {
        return (0.0, 0.0);
    }
    let max = max.min(n / 3) as i64;
    let score = |d: i64| -> f32 {
        let (lo, hi) = (d.max(0) as usize, (n as i64 + d.min(0)) as usize);
        let (mut s, mut sa, mut sb) = (0f64, 0f64, 0f64);
        for i in lo..hi {
            let (x, y) = (da[(i as i64 - d) as usize] as f64, db[i] as f64);
            s += x * y;
            sa += x * x;
            sb += y * y;
        }
        if sa <= 0.0 || sb <= 0.0 {
            0.0
        } else {
            (s / (sa * sb).sqrt()) as f32
        }
    };
    let scores: Vec<f32> = (-max..=max).map(score).collect();
    let Some((best, &q)) = scores.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)) else { return (0.0, 0.0) };
    let mut d = best as f32 - max as f32;
    if best > 0 && best + 1 < scores.len() {
        let (l, c, r) = (scores[best - 1], scores[best], scores[best + 1]);
        let den = l - 2.0 * c + r;
        if den < 0.0 {
            d += (0.5 * (l - r) / den).clamp(-0.5, 0.5);
        }
    }
    (d, q.max(0.0))
}

/// Variations d'un profil : différence entre voisins, après un léger lissage.
fn edges(p: &[f32]) -> Vec<f32> {
    let smooth: Vec<f32> = (0..p.len())
        .map(|i| {
            let (lo, hi) = (i.saturating_sub(1), (i + 2).min(p.len()));
            p[lo..hi].iter().sum::<f32>() / (hi - lo) as f32
        })
        .collect();
    smooth.windows(2).map(|w| w[1] - w[0]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vision::track::{Detection, Tracker};

    /// Banc d'essai : une caméra dont l'image glisse de `rate` × vitesse (fractions d'image par
    /// seconde), `act` secondes après la commande ; une analyse à 8 images par seconde qui rend
    /// chaque image `meas` secondes après sa prise ; une personne dont `person(t)` donne l'écart
    /// au centre de l'image de départ (`None` : elle n'est pas là).
    struct Bench {
        rate: [f32; 2],
        act: f64,
        meas: f64,
        t: f64,
        next_frame: f64,
        /// Glissement de l'image depuis le départ (fractions d'image).
        cam: [f64; 2],
        sent: Vec<(f64, PtzCommand)>,
        pending: VecDeque<(f64, Option<BBox>)>,
        tracker: Tracker,
        at: AutoTracker,
        /// (instant, écart réel au centre)
        log: Vec<(f64, [f32; 2])>,
        noise: u32,
        events: Vec<(f64, AutoTrackEvent)>,
        /// Vitesse réelle de l'image selon la vitesse demandée, si elle n'est pas proportionnelle.
        curve: Option<fn(f32) -> f32>,
    }

    const DT: f64 = 0.005;

    /// Orientation de l'Imou Ranger 2C mesurée (flux fluide) : immobile à 0,10, 0,17 largeur
    /// d'image par seconde à 0,15 et 0,26 à 0,30, soit environ 0,49 × vitesse^0,56.
    fn ranger_2c(u: f32) -> f32 {
        if u.abs() < 0.125 {
            return 0.0;
        }
        0.49 * u.abs().powf(0.56) * u.signum()
    }

    impl Bench {
        fn new(settings: AutoTrackSettings, rate_factor: f32, latency: f64) -> Bench {
            let rate = [settings.pan_rate * rate_factor, settings.tilt_rate * rate_factor];
            let mut tracker = Tracker::default();
            tracker.high = 0.5;
            Bench {
                rate,
                act: 0.1,
                meas: latency - 0.1,
                t: 0.0,
                next_frame: 0.0,
                cam: [0.0; 2],
                sent: Vec::new(),
                pending: VecDeque::new(),
                tracker,
                at: AutoTracker::new(settings),
                log: Vec::new(),
                noise: 7,
                events: Vec::new(),
                curve: None,
            }
        }

        /// Vitesse que la caméra applique à l'instant `t`.
        fn speed_at(&self, t: f64) -> [f32; 2] {
            match self.sent.iter().rev().find(|(at, _)| *at <= t - self.act) {
                Some((_, PtzCommand::Move { pan, tilt })) => [*pan, *tilt],
                _ => [0.0; 2],
            }
        }

        fn jitter(&mut self) -> f32 {
            self.noise = self.noise.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (((self.noise >> 16) % 1000) as f32 / 1000.0 - 0.5) * 0.008
        }

        fn error(&self, p: [f32; 2]) -> [f32; 2] {
            [p[0] + SIGN[0] * self.cam[0] as f32, p[1] + SIGN[1] * self.cam[1] as f32]
        }

        fn sent_now(&mut self, cmd: Option<PtzCommand>) {
            if let Some(c) = cmd {
                self.sent.push((self.t, c));
            }
            for e in self.at.take_events() {
                self.events.push((self.t, e));
            }
        }

        fn run(&mut self, until: f64, person: impl Fn(f64) -> Option<[f32; 2]>) {
            let aim_y = self.at.settings.aim_y;
            while self.t < until {
                self.t += DT;
                let s = self.speed_at(self.t);
                for i in 0..2 {
                    let v = match self.curve {
                        Some(f) if s[i] != 0.0 => f(s[i]),
                        _ => self.rate[i] * s[i],
                    };
                    self.cam[i] += v as f64 * DT;
                }
                let seen = person(self.t).map(|p| self.error(p));
                if let Some(e) = seen {
                    self.log.push((self.t, e));
                }
                if self.t >= self.next_frame {
                    self.next_frame += 0.125;
                    let bbox = match seen.filter(|e| e[0].abs() < 0.5 && e[1].abs() < 0.5) {
                        Some(e) => {
                            let (h, w) = (0.5, 0.15);
                            let (cx, ay) = (0.5 + e[0] + self.jitter(), 0.5 + e[1] + self.jitter());
                            Some(BBox::new(cx - w / 2.0, ay - aim_y * h, cx + w / 2.0, ay + (1.0 - aim_y) * h).clamp())
                        }
                        None => None,
                    };
                    self.pending.push_back((self.t + self.meas, bbox));
                }
                let mut analysed = false;
                while self.pending.front().is_some_and(|(at, _)| *at <= self.t) {
                    let (_, bbox) = self.pending.pop_front().unwrap();
                    let dets: Vec<Detection> = bbox.into_iter().map(|b| Detection { bbox: b, score: 0.9, pose: None }).collect();
                    self.tracker.update(self.t, &dets);
                    let cmd = self.at.update(self.t, self.tracker.tracks());
                    self.sent_now(cmd);
                    analysed = true;
                }
                if !analysed && self.at.wake_at().is_some_and(|w| w <= self.t) {
                    let cmd = self.at.update(self.t, self.tracker.tracks());
                    self.sent_now(cmd);
                }
            }
        }

        /// Mesures sur l'axe `axis` entre `from` et `to` : instant où l'écart entre dans la zone
        /// morte pour de bon, dépassement (au-delà du centre, du côté opposé au départ), écart
        /// final, inversions de sens des commandes.
        fn metrics(&self, axis: usize, from: f64, to: f64) -> (Option<f64>, f32, f32, usize) {
            let dz = if axis == 0 { self.at.settings.dead_zone.0 } else { self.at.settings.dead_zone.1 };
            let part: Vec<&(f64, [f32; 2])> = self.log.iter().filter(|(t, _)| *t >= from && *t <= to).collect();
            let start = part.first().map_or(0.0, |p| p.1[axis]).signum();
            let settle = part.iter().rposition(|p| p.1[axis].abs() >= dz).map_or(Some(from), |i| part.get(i + 1).map(|p| p.0 - from));
            let overshoot = part.iter().map(|p| -start * p.1[axis]).fold(0.0, f32::max);
            let last = part.last().map_or(0.0, |p| p.1[axis]);
            let mut reversals = 0;
            let mut prev = 0.0f32;
            for (_, c) in self.sent.iter().filter(|(t, _)| *t >= from && *t <= to) {
                if let PtzCommand::Move { pan, tilt } = c {
                    let u = if axis == 0 { *pan } else { *tilt };
                    if u != 0.0 {
                        if prev != 0.0 && u.signum() != prev.signum() {
                            reversals += 1;
                        }
                        prev = u;
                    }
                }
            }
            (settle, overshoot, last, reversals)
        }

        /// Plus grand nombre de commandes sur une seconde.
        fn max_rate(&self) -> usize {
            self.sent.iter().map(|(t, _)| self.sent.iter().filter(|(u, _)| *u >= *t && *u < *t + 1.0).count()).max().unwrap_or(0)
        }
    }

    /// Personne immobile, apparue loin du centre : centrée sans osciller, pour un modèle de la
    /// caméra juste ou faux (vitesse de l'image ×0,4 à ×1,6, retard 0,25 à 0,7 s pour 0,45 réglé).
    /// Seul le coin où la caméra est à la fois bien plus rapide et bien plus en retard que réglé
    /// dépasse le centre : le freinage l'amortit (pas d'oscillation entretenue).
    #[test]
    fn centers_without_oscillation_despite_a_wrong_model() {
        println!("vitesse réelle/modèle · retard réel → centrée en (s) · dépassement x / y · écart final x / y · inversions · commandes/s max");
        for &factor in &[0.4f32, 0.6, 1.0, 1.6] {
            for &lat in &[0.25f64, 0.45, 0.7] {
                let mut b = Bench::new(AutoTrackSettings::default(), factor, lat);
                b.run(10.0, |t| (t > 0.3).then_some([0.32, 0.18]));
                let (sx, ox, fx, rx) = b.metrics(0, 0.3, 10.0);
                let (sy, oy, fy, ry) = b.metrics(1, 0.3, 10.0);
                println!(
                    "  ×{factor:.1} · {lat:.2} s → {:.1} s · {ox:.3} / {oy:.3} · {fx:+.3} / {fy:+.3} · {rx} / {ry} · {}",
                    sx.unwrap_or(f64::NAN).max(sy.unwrap_or(f64::NAN)),
                    b.max_rate()
                );
                let extreme = factor > 1.5 && lat > 0.6;
                let (settle, over, rev) = if extreme { (8.0, 0.25, 3) } else { (4.0, 0.08, 0) };
                assert!(sx.is_some_and(|s| s < settle) && sy.is_some_and(|s| s < settle), "×{factor} {lat} s : pas centrée ({sx:?}, {sy:?})");
                assert!(ox < over && oy < over.max(0.12), "×{factor} {lat} s : dépassement {ox} / {oy}");
                assert!(rx <= rev && ry <= rev, "×{factor} {lat} s : oscillation ({rx} / {ry} inversions)");
                assert!(b.max_rate() <= 5, "×{factor} {lat} s : {} commandes en une seconde", b.max_rate());
                assert!(!b.at.is_moving(), "×{factor} {lat} s : la caméra tourne encore");
            }
        }
    }

    /// Caméra au moteur pas proportionnel (celui de la Ranger 2C), en largeur seulement : comme
    /// sur la vraie, son inclinaison ne répond pas.
    #[test]
    fn centers_with_the_measured_ranger_2c_motor() {
        for &(lat, x) in &[(0.42f64, 0.32f32), (0.42, -0.2), (0.6, 0.4)] {
            let settings = AutoTrackSettings { tilt: false, max_speed: 0.3, ..AutoTrackSettings::default() };
            let mut b = Bench::new(settings, 1.0, lat);
            b.curve = Some(ranger_2c);
            b.run(8.0, |t| (t > 0.3).then_some([x, 0.05]));
            let (settle, over, last, rev) = b.metrics(0, 0.3, 8.0);
            println!("Ranger 2C · retard {lat:.2} s · départ x {x:+.2} → centrée en {settle:?} s, dépassement {over:.3}, écart final {last:+.3}, inversions {rev}, {} commandes", b.sent.len());
            assert!(settle.is_some_and(|s| s < 4.0) && over < 0.08 && rev <= 1, "{settle:?} {over} {rev}");
            assert!(b.sent.iter().all(|(_, c)| !matches!(c, PtzCommand::Move { tilt, .. } if *tilt != 0.0)), "inclinaison commandée");
        }
    }

    /// Personne qui traverse la pièce : la caméra l'accompagne.
    #[test]
    fn follows_a_walking_person() {
        let mut b = Bench::new(AutoTrackSettings::default(), 1.0, 0.45);
        // Au centre, puis marche vers la droite à 0,12 largeur d'image par seconde pendant 5 s.
        let walk = |t: f64| Some([(0.12 * (t - 1.0).clamp(0.0, 5.0)) as f32, 0.0]);
        b.run(10.0, walk);
        let during: Vec<f32> = b.log.iter().filter(|(t, _)| (2.5..6.0).contains(t)).map(|(_, e)| e[0].abs()).collect();
        let worst = during.iter().cloned().fold(0.0, f32::max);
        let (settle, overshoot, last, _) = b.metrics(0, 6.0, 10.0);
        println!("marche : écart max {worst:.3} pendant la marche, recentrée {settle:?} s après l'arrêt, dépassement {overshoot:.3}, écart final {last:+.3}");
        assert!(worst < 0.2, "écart {worst} pendant la marche");
        assert!(last.abs() < 0.08 && overshoot < 0.08, "après la marche : écart {last}, dépassement {overshoot}");
        // Sans la part de vitesse, la personne s'écarte davantage.
        let mut slow = Bench::new(AutoTrackSettings { velocity_gain: 0.0, ..AutoTrackSettings::default() }, 1.0, 0.45);
        slow.run(10.0, walk);
        let worst_slow = slow.log.iter().filter(|(t, _)| (2.5..6.0).contains(t)).map(|(_, e)| e[0].abs()).fold(0.0, f32::max);
        println!("marche sans la part de vitesse : écart max {worst_slow:.3}");
        assert!(worst_slow >= worst);
    }

    #[test]
    fn no_command_while_the_person_is_near_the_center() {
        let mut b = Bench::new(AutoTrackSettings::default(), 1.0, 0.45);
        b.run(6.0, |_| Some([0.05, -0.08]));
        assert!(b.sent.is_empty(), "{:?}", b.sent);
        assert_eq!(b.at.target(), Some(1));
    }

    fn track(id: u32, bbox: BBox, last_seen: f64) -> Track {
        Track { id, bbox, score: 0.9, pose: None, first_seen: 0.0, last_seen, hits: 5, confirmed: true, history: VecDeque::new() }
    }

    #[test]
    fn follows_the_largest_and_never_jumps_to_someone_else() {
        let mut at = AutoTracker::new(AutoTrackSettings::default());
        let small = BBox::new(0.05, 0.3, 0.15, 0.6);
        let big = BBox::new(0.7, 0.1, 0.95, 0.9);
        at.update(0.0, &[track(1, small, 0.0), track(2, big, 0.0)]);
        assert_eq!(at.target(), Some(2), "la plus grande");
        // L'autre grandit (elle s'approche) : on garde la même piste.
        let closer = BBox::new(0.0, 0.0, 0.45, 1.0);
        at.update(0.5, &[track(1, closer, 0.5), track(2, big, 0.5)]);
        assert_eq!(at.target(), Some(2));
        // Une piste non confirmée n'est jamais choisie.
        let mut b = AutoTracker::new(AutoTrackSettings::default());
        let mut t = track(3, big, 0.0);
        t.confirmed = false;
        assert_eq!(b.update(0.0, &[t]), None);
        assert_eq!(b.target(), None);
    }

    #[test]
    fn stops_at_once_when_lost_then_waits_before_anyone_else() {
        let mut at = AutoTracker::new(AutoTrackSettings::default());
        let right = BBox::new(0.75, 0.2, 0.95, 0.8);
        let left = BBox::new(0.05, 0.2, 0.25, 0.8);
        assert!(matches!(at.update(0.0, &[track(1, right, 0.0)]), Some(PtzCommand::Move { pan, .. }) if pan > 0.0));
        // La personne n'est plus détectée : arrêt après `hold_after`, la piste reste suivie.
        assert_ne!(at.update(0.4, &[track(1, right, 0.0)]), Some(PtzCommand::Stop));
        assert_eq!(at.update(0.7, &[track(1, right, 0.0)]), Some(PtzCommand::Stop));
        assert_eq!(at.target(), Some(1));
        // Perdue (le suivi des pistes l'a oubliée) ; une autre personne est là, à gauche.
        at.update(1.0, &[track(2, left, 1.0)]);
        assert!(matches!(at.phase(), Phase::Lost { track: 1, .. }));
        assert_eq!(at.take_events().iter().filter(|e| matches!(e, AutoTrackEvent::Lost(1))).count(), 1);
        for k in 0..15 {
            let now = 1.0 + 0.125 * k as f64;
            assert_eq!(at.update(now, &[track(2, left, now)]), None, "{now} s : pas de saut vers l'autre personne");
        }
        // Après le délai, elle est suivie.
        assert!(matches!(at.update(3.1, &[track(2, left, 3.1)]), Some(PtzCommand::Move { pan, .. }) if pan < 0.0));
        assert_eq!(at.target(), Some(2));
    }

    #[test]
    fn the_same_track_is_taken_back_at_once() {
        let mut at = AutoTracker::new(AutoTrackSettings::default());
        let b = BBox::new(0.45, 0.2, 0.55, 0.8);
        at.update(0.0, &[track(4, b, 0.0)]);
        at.update(2.5, &[track(4, b, 0.0)]);
        assert!(matches!(at.phase(), Phase::Lost { track: 4, .. }));
        at.update(2.6, &[track(4, b, 2.6)]);
        assert_eq!(at.target(), Some(4));
    }

    /// Suivi, perte, puis retour à la position de départ en refaisant les mouvements à l'envers.
    #[test]
    fn returns_to_the_start_by_reversing_the_moves() {
        for &factor in &[0.6f32, 1.0, 1.6] {
            let settings = AutoTrackSettings { return_after: Some(3.0), ..AutoTrackSettings::default() };
            let mut b = Bench::new(settings, factor, 0.45);
            // Elle apparaît en haut à droite, marche vers la droite et disparaît en marchant.
            b.run(35.0, |t| (t < 7.0).then(|| [0.3 + (0.08 * (t - 3.0).max(0.0)) as f32, -0.2]));
            let moved = b.events.iter().any(|(_, e)| matches!(e, AutoTrackEvent::Moving(_)));
            let lost = b.events.iter().find(|(_, e)| matches!(e, AutoTrackEvent::Lost(1))).map(|(t, _)| *t).expect("jamais perdue");
            let back = b.events.iter().find(|(_, e)| matches!(e, AutoTrackEvent::Home)).map(|(t, _)| *t).expect("jamais revenue");
            // Dernière image où elle était : prise à 7 s, analysée 0,35 s plus tard. Si la caméra
            // tournait encore, elle s'arrête vite ; ensuite, plus rien jusqu'au retour.
            let moving = matches!(b.sent.iter().rev().find(|(t, _)| *t < 7.35), Some((_, PtzCommand::Move { .. })));
            let stop = b.sent.iter().find(|(t, c)| moving && *t >= 7.35 && *c == PtzCommand::Stop).map(|(t, _)| *t - 7.35);
            let returning_at = b.events.iter().find(|(_, e)| matches!(e, AutoTrackEvent::Returning { .. })).map_or(f64::MAX, |(t, _)| *t);
            assert!(!b.sent.iter().any(|(t, c)| *t > 8.0 && *t < returning_at && *c != PtzCommand::Stop), "mouvement après la perte");
            let returning = b.events.iter().find_map(|(_, e)| if let AutoTrackEvent::Returning { secs } = e { Some(*secs) } else { None }).unwrap_or(0.0);
            println!("×{factor:.1} : arrêt {stop:?} s après sa dernière détection, perdue à {lost:.1} s, retour de {returning:.1} s, revenue à {back:.1} s, écart au départ {:+.4} / {:+.4}", b.cam[0], b.cam[1]);
            assert!(stop.is_none_or(|s| s <= 0.65), "arrêt tardif : {stop:?}");
            assert!(moved);
            assert!(back - lost >= 3.0, "retour avant le délai");
            assert!(b.cam[0].abs() < 0.01 && b.cam[1].abs() < 0.01, "×{factor} : écart au départ {:?}", b.cam);
            let returning_at = b.events.iter().find(|(_, e)| matches!(e, AutoTrackEvent::Returning { .. })).unwrap().0;
            let mut turns = [0usize; 2];
            let mut prev = [0f32; 2];
            for (_, c) in b.sent.iter().filter(|(t, _)| *t >= returning_at) {
                if let PtzCommand::Move { pan, tilt } = c {
                    for (i, u) in [*pan, *tilt].into_iter().enumerate().filter(|(_, u)| *u != 0.0) {
                        turns[i] += (prev[i] != 0.0 && u.signum() != prev[i].signum()) as usize;
                        prev[i] = u;
                    }
                }
            }
            assert!(turns[0] <= 1 && turns[1] <= 1, "×{factor} : retour en zigzag {turns:?}");
            assert_eq!(b.at.phase(), Phase::Idle);
            assert!(!b.at.is_moving());
            assert_eq!(b.at.travel(), (0.0, 0.0));
        }
    }

    #[test]
    fn go_home_reverses_even_mid_course() {
        let mut b = Bench::new(AutoTrackSettings::default(), 1.0, 0.45);
        b.run(2.0, |_| Some([0.4, 0.0]));
        assert!(b.at.is_moving());
        let cmd = b.at.go_home(b.t);
        b.sent_now(cmd);
        assert_eq!(b.at.phase(), Phase::Returning);
        // Plus d'analyse (essai terminé) : seuls les réveils pilotent la caméra.
        b.run(8.0, |_| None);
        assert!(b.cam[0].abs() < 0.01, "écart au départ {:?}", b.cam);
        assert!(!b.at.is_moving());
    }

    #[test]
    fn pauses_while_someone_drives_the_camera() {
        let mut at = AutoTracker::new(AutoTrackSettings::default());
        let right = BBox::new(0.75, 0.2, 0.95, 0.8);
        assert!(at.update(0.0, &[track(1, right, 0.0)]).is_some());
        at.manual(0.5);
        assert!(matches!(at.take_events().last(), Some(AutoTrackEvent::Manual)));
        for k in 1..80 {
            let now = 0.5 + 0.125 * k as f64;
            assert_eq!(at.update(now, &[track(1, right, now)]), None, "{now} s : commande pendant le pilotage à la main");
        }
        assert_eq!(at.travel(), (0.0, 0.0), "la position choisie à la main devient le départ");
        assert_eq!(at.go_home(5.0), None);
        // Fin de la pause : le suivi reprend.
        assert!(at.update(10.6, &[track(1, right, 10.6)]).is_some());
    }

    #[test]
    fn speeds_are_limited_rounded_and_inverted_on_demand() {
        assert_eq!(quantize(2.0, 0.15, 0.5), 0.5);
        assert_eq!(quantize(0.04, 0.15, 0.3), 0.15);
        assert_eq!(quantize(-0.5, 0.15, 0.1), -0.15, "vitesse maximale sous la minimale");
        assert_eq!(quantize(-0.02, 0.1, 0.5), -0.1);
        assert!((quantize(0.33, 0.1, 0.5) - 0.35).abs() < 1e-6);
        assert_eq!(PtzCommand::Move { pan: 0.3, tilt: -0.2 }.inverted(false, true), PtzCommand::Move { pan: 0.3, tilt: 0.2 });
        assert_eq!(PtzCommand::Stop.inverted(true, true), PtzCommand::Stop);
        assert_eq!(direction(0.2, -0.1), "bas-droite");
        assert_eq!(AutoTrackEvent::Moved { trail: vec!["droite", "bas-droite"], secs: 1.44 }.describe(), "arrêt après 1,4 s (droite → bas-droite)");
    }

    #[test]
    fn nudge_back_towards_the_start_image() {
        let s = AutoTrackSettings::default();
        // Décor glissé de 20 px vers la droite : la caméra a tourné à gauche, on tourne à droite.
        let (cmd, secs) = home_nudge((20.0, 3.0), (640, 480), &s, 0.1, 3.0).unwrap();
        assert_eq!(cmd, PtzCommand::Move { pan: 0.15, tilt: 0.0 }, "jamais sous la vitesse minimale");
        assert!((secs - 20.0 / 640.0 / (0.9 * 0.15)).abs() < 1e-3, "{secs}");
        // Décor glissé vers le bas : la caméra a monté, on descend.
        assert!(matches!(home_nudge((1.0, 9.0), (640, 480), &s, 0.15, 3.0), Some((PtzCommand::Move { pan, tilt }, _)) if pan == 0.0 && tilt < 0.0));
        // Sans inclinaison, l'écart vertical reste ; écart négligeable : rien.
        assert_eq!(home_nudge((1.0, 9.0), (640, 480), &AutoTrackSettings { tilt: false, ..s.clone() }, 0.15, 3.0), None);
        assert_eq!(home_nudge((2.0, -2.5), (640, 480), &s, 0.15, 3.0), None);
    }

    #[test]
    fn image_shift_from_profiles() {
        let (w, h) = (320usize, 180usize);
        // Décor : taches pseudo-aléatoires (ni motif répété, ni image réelle).
        let mut seed = 11u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            ((seed >> 16) % 10_000) as f32 / 10_000.0
        };
        let blobs: Vec<[f32; 4]> = (0..80).map(|_| [rnd() * 400.0 - 40.0, rnd() * 260.0 - 40.0, 4.0 + rnd() * 20.0, rnd() * 2.0 - 1.0]).collect();
        let image = |dx: f32, dy: f32| -> Vec<u8> {
            let mut px = vec![0u8; w * h * 4];
            for y in 0..h {
                for x in 0..w {
                    let (u, v) = (x as f32 - dx, y as f32 - dy);
                    let l: f32 = blobs.iter().map(|b| b[3] * (-((u - b[0]).powi(2) + (v - b[1]).powi(2)) / (b[2] * b[2])).exp()).sum();
                    let g = (128.0 + 70.0 * l).clamp(0.0, 255.0) as u8;
                    let o = (y * w + x) * 4;
                    px[o..o + 4].copy_from_slice(&[g, g, g, 255]);
                }
            }
            px
        };
        let a = Profiles::from_rgba(w, h, &image(0.0, 0.0));
        let ((dx, dy), q) = a.shift_to(&Profiles::from_rgba(w, h, &image(7.0, -4.0)), 30);
        assert!((dx - 7.0).abs() < 0.6 && (dy + 4.0).abs() < 0.6 && q > 0.5, "({dx}, {dy}) {q}");
        let ((dx, dy), q) = a.shift_to(&a, 30);
        assert!(dx.abs() < 0.01 && dy.abs() < 0.01 && q > 0.99);
    }
}
