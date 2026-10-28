//! Chaîne d'analyse d'une caméra, indépendante des modèles qui y sont branchés :
//!
//! 1. **personnes** détectées sur l'image entière, puis suivies d'une image à l'autre ;
//! 2. **actions** tirées des pistes (arrivée, zones, chute…) ;
//! 3. **visages** cherchés dans le haut de chaque silhouette, sur l'image pleine résolution
//!    (un visage lointain n'y fait que quelques dizaines de pixels), puis comparés à la galerie ;
//!    chaque piste vote sur plusieurs images avant d'être nommée (ou déclarée inconnue).
//!
//! La reconnaissance est espacée : souvent tant qu'une piste n'est pas identifiée, rarement
//! ensuite (simple vérification). Les modèles sont derrière des traits : le moteur réel (ONNX)
//! et des imitations pour les tests s'y branchent de la même façon.

use super::actions::{ActionEngine, ActionEvent};
use super::faces::{FaceGallery, IdentityVotes, Verdict};
use super::geom::{BBox, Point};
use super::objects::{ObjectChange, ObjectDetection, ObjectKind, ObjectTracker, ObjectView};
use super::track::{Detection, Tracker};
use crate::camera::Frame;
use anyhow::Result;
use std::collections::HashMap;

/// Visage détecté : boîte et 5 points (œil droit, œil gauche, nez, coin droit puis gauche de la
/// bouche — droite et gauche de la personne, donc de gauche à droite sur l'image), normalisés.
#[derive(Clone, Debug, PartialEq)]
pub struct FaceBox {
    pub bbox: BBox,
    pub landmarks: [Point; 5],
    pub score: f32,
}

pub trait PersonDetector: Send {
    fn detect(&mut self, frame: &Frame) -> Result<Vec<Detection>>;
}

pub trait FaceDetector: Send {
    /// Visages dans la région `roi` de l'image (coordonnées normalisées de l'image entière).
    fn detect(&mut self, frame: &Frame, roi: BBox) -> Result<Vec<FaceBox>>;
}

/// Animaux et véhicules.
pub trait ObjectDetector: Send {
    fn detect(&mut self, frame: &Frame) -> Result<Vec<ObjectDetection>>;
    /// Personnes vues lors du dernier `detect` (boîte, score), si le modèle en voit : deuxième
    /// avis sur les silhouettes douteuses du modèle des personnes.
    fn last_persons(&self) -> Vec<(BBox, f32)> {
        Vec::new()
    }
}

/// Deuxième avis sur les personnes : une silhouette au score modeste n'est retenue que si le
/// détecteur d'objets voit lui aussi quelqu'un au même endroit (ou si elle prolonge une piste
/// déjà admise). Sur COCO val2017, deux fois moins d'images vides prises pour une présence, pour
/// 1 à 2 % de vraies personnes perdues (`homesmart-cli vision eval coco`).
#[derive(Clone, Debug, PartialEq)]
pub struct PersonCheck {
    /// Au-dessus, la silhouette se passe d'avis.
    pub strong: f32,
    /// Score minimal de la personne vue par le second modèle.
    pub second: f32,
    /// Recouvrement minimal entre les deux.
    pub iou: f32,
    /// Âge maximal de l'avis réutilisé (s) ; au-delà, le second modèle repasse.
    pub max_age: f64,
}

impl Default for PersonCheck {
    fn default() -> Self {
        PersonCheck { strong: 0.8, second: 0.2, iou: 0.4, max_age: 0.3 }
    }
}

pub trait FaceEmbedder: Send {
    /// Empreinte d'un visage (redressé d'après ses 5 points).
    fn embed(&mut self, frame: &Frame, face: &FaceBox) -> Result<Vec<f32>>;
    /// Nom du modèle (les empreintes de modèles différents ne se comparent pas).
    fn model(&self) -> &str;
}

/// Réglages de la reconnaissance.
#[derive(Clone, Debug, PartialEq)]
pub struct FaceRules {
    /// Similarité minimale pour reconnaître quelqu'un.
    pub threshold: f32,
    /// Visages nets sans aucune reconnaissance avant de parler d'inconnu.
    pub stranger_after: u32,
    /// Intervalle entre deux essais tant que la piste n'est pas identifiée (s).
    pub every_unknown: f64,
    /// Intervalle de vérification une fois identifiée (s).
    pub every_known: f64,
    /// Part minimale des visages nets reconnus comme la personne pour la nommer (voir
    /// `IdentityVotes::verdict`).
    pub known_share: f32,
    /// Largeur minimale d'un visage, en pixels de l'image analysée : en dessous, l'empreinte
    /// n'est pas fiable (le modèle travaille sur 112 × 112). Mesuré sur LFW (6 000 paires) :
    /// à 24 px, toujours 0,07 % d'acceptés à tort au seuil 0,36 (5,6 % de refusés, rattrapés
    /// par le vote sur plusieurs images) ; à 19 px, 0,13 % et 8,5 %.
    pub min_face_px: f32,
}

impl Default for FaceRules {
    fn default() -> Self {
        FaceRules { threshold: 0.36, stranger_after: 4, every_unknown: 0.8, every_known: 6.0, known_share: 0.34, min_face_px: 28.0 }
    }
}

/// Ce que la chaîne sait d'une personne suivie.
#[derive(Clone, Debug, PartialEq)]
pub struct PersonView {
    pub track: u32,
    pub bbox: BBox,
    pub score: f32,
    /// Squelette (si le modèle le donne).
    pub pose: Option<super::Pose>,
    pub verdict: Verdict,
    /// Dernier visage vu sur cette piste.
    pub face: Option<FaceBox>,
    /// Sa dernière empreinte (pour enregistrer la personne en un clic) ; en mémoire seulement.
    pub embedding: Option<Vec<f32>>,
}

/// Événements de l'image analysée.
#[derive(Clone, Debug, PartialEq)]
pub enum VisionEvent {
    Action(ActionEvent),
    /// Une piste vient d'être identifiée (ou déclarée inconnue).
    Identified { track: u32, verdict: Verdict, at: f64 },
    /// Un animal ou un véhicule apparaît ou s'en va.
    Object { kind: ObjectKind, track: u32, change: ObjectChange, at: f64 },
    /// Coups répétés entre deux personnes (voir `violence`).
    Violence(super::violence::ViolenceEvent),
}

#[derive(Default)]
struct FaceState {
    votes: IdentityVotes,
    verdict: Option<Verdict>,
    last_try: Option<f64>,
    face: Option<FaceBox>,
    embedding: Option<Vec<f32>>,
}

pub struct Pipeline {
    pub tracker: Tracker,
    pub actions: ActionEngine,
    pub face_rules: FaceRules,
    persons: Box<dyn PersonDetector>,
    faces: Option<(Box<dyn FaceDetector>, Box<dyn FaceEmbedder>)>,
    face_states: HashMap<u32, FaceState>,
    objects: Option<Objects>,
    check: Option<PersonCheck>,
    violence: Option<super::violence::ViolenceDetector>,
}

/// Animaux et véhicules : analysés à leur propre rythme (bien plus lent que les personnes,
/// suffisant pour eux), et seulement les sortes demandées.
struct Objects {
    detector: Box<dyn ObjectDetector>,
    tracker: ObjectTracker,
    kinds: Vec<ObjectKind>,
    every: f64,
    last: Option<f64>,
    views: Vec<ObjectView>,
    /// Personnes vues à la dernière analyse.
    persons: Vec<(BBox, f32)>,
}

impl Objects {
    fn run(&mut self, now: f64, frame: &Frame, events: &mut Vec<VisionEvent>) -> Result<()> {
        self.last = Some(now);
        let found: Vec<ObjectDetection> = self.detector.detect(frame)?.into_iter().filter(|d| self.kinds.contains(&d.kind)).collect();
        self.persons = self.detector.last_persons();
        for (kind, track, change) in self.tracker.update(now, &found) {
            events.push(VisionEvent::Object { kind, track, change, at: now });
        }
        self.views = self.tracker.views(now);
        Ok(())
    }
}

impl Pipeline {
    /// Score à partir duquel une silhouette ouvre une piste (sur COCO : précision 0,93 et
    /// rappel 0,83 à 0,5 ; voir `homesmart-cli vision eval coco`). Plus bas, elle ne fait que
    /// prolonger une piste.
    pub const NEW_PERSON: f32 = 0.5;

    pub fn new(persons: Box<dyn PersonDetector>, faces: Option<(Box<dyn FaceDetector>, Box<dyn FaceEmbedder>)>, actions: ActionEngine) -> Self {
        let mut tracker = Tracker::default();
        tracker.high = Self::NEW_PERSON;
        Pipeline { tracker, actions, face_rules: FaceRules::default(), persons, faces, face_states: HashMap::new(), objects: None, check: None, violence: None }
    }

    /// Ajoute la détection de la violence (coups répétés entre deux personnes).
    pub fn with_violence(mut self) -> Self {
        self.violence = Some(super::violence::ViolenceDetector::new(super::violence::ViolenceRules::default()));
        self
    }

    /// Fait vérifier les silhouettes douteuses par le détecteur d'objets (voir [`PersonCheck`]).
    /// Sans effet tant qu'aucun détecteur d'objets n'est branché.
    pub fn with_person_check(mut self, check: PersonCheck) -> Self {
        self.check = Some(check);
        self
    }

    /// Ajoute la détection des animaux / véhicules `kinds`, une analyse toutes les `every` secondes.
    pub fn with_objects(mut self, detector: Box<dyn ObjectDetector>, kinds: Vec<ObjectKind>, every: f64) -> Self {
        self.objects = Some(Objects { detector, tracker: ObjectTracker::default(), kinds, every, last: None, views: Vec::new(), persons: Vec::new() });
        self
    }

    /// Animaux et véhicules suivis (dernière analyse).
    pub fn objects(&self) -> &[ObjectView] {
        self.objects.as_ref().map_or(&[], |o| &o.views[..])
    }

    /// Région où chercher le visage d'une silhouette : son haut, un peu élargi.
    pub fn head_region(person: &BBox) -> BBox {
        let h = person.height();
        BBox::new(person.x0, person.y0, person.x1, person.y0 + h * 0.45).grow(0.25).clamp()
    }

    /// Analyse une image prise à `now` (secondes). `gallery` : visages connus (peut être vide).
    pub fn process(&mut self, now: f64, frame: &Frame, gallery: &FaceGallery) -> Result<(Vec<PersonView>, Vec<VisionEvent>)> {
        let mut dets = self.persons.detect(frame)?;
        let mut object_events = Vec::new();
        if let (Some(check), Some(o)) = (&self.check, self.objects.as_mut()) {
            // Une silhouette qui prolonge une piste existante est déjà admise.
            let known: Vec<BBox> = self.tracker.tracks().iter().map(|t| t.bbox).collect();
            let admitted = |d: &Detection, persons: &[(BBox, f32)]| {
                d.score >= check.strong || known.iter().any(|b| b.iou(&d.bbox) >= 0.3) || persons.iter().any(|(b, s)| *s >= check.second && b.iou(&d.bbox) >= check.iou)
            };
            // Une détection faible n'ouvre pas de piste : pas besoin d'avis pour elle.
            let high = self.tracker.high;
            let doubtful = dets.iter().any(|d| d.score >= high && !admitted(d, &o.persons));
            if doubtful && o.last.is_none_or(|at| now - at > check.max_age) {
                o.run(now, frame, &mut object_events)?;
            }
            dets.retain(|d| d.score < high || admitted(d, &o.persons));
        }
        let changes = self.tracker.update(now, &dets);
        let (fw, fh) = (frame.width as f32, frame.height as f32);
        let mut events: Vec<VisionEvent> = self.actions.update(now, &self.tracker, &changes, fw, fh).into_iter().map(VisionEvent::Action).collect();
        events.extend(object_events);
        if let Some(v) = self.violence.as_mut() {
            events.extend(v.update(now, &self.tracker, fw, fh).into_iter().map(VisionEvent::Violence));
        }
        // Pistes disparues : on oublie leurs visages.
        let alive: Vec<u32> = self.tracker.tracks().iter().map(|t| t.id).collect();
        self.face_states.retain(|id, _| alive.contains(id));

        if let Some((detector, embedder)) = self.faces.as_mut() {
            let rules = &self.face_rules;
            let comparable = gallery.people.is_empty() || gallery.model == embedder.model();
            for t in self.tracker.tracks().iter().filter(|t| t.confirmed && t.last_seen >= now) {
                let st = self.face_states.entry(t.id).or_default();
                let every = if matches!(st.verdict, Some(Verdict::Person { .. })) { rules.every_known } else { rules.every_unknown };
                if st.last_try.is_some_and(|at| now - at < every) {
                    continue;
                }
                st.last_try = Some(now);
                let faces = detector.detect(frame, Self::head_region(&t.bbox))?;
                // Le visage le plus net de la silhouette, assez grand pour être fiable.
                let Some(face) = faces
                    .into_iter()
                    .filter(|f| f.bbox.width() * frame.width as f32 >= rules.min_face_px && t.bbox.grow(0.1).contains(f.bbox.center()))
                    .max_by(|a, b| a.score.total_cmp(&b.score))
                else {
                    continue;
                };
                if comparable {
                    let emb = embedder.embed(frame, &face)?;
                    st.votes.add(&gallery.identify(&emb, rules.threshold));
                    let v = st.votes.verdict(rules.stranger_after, rules.known_share);
                    let changed = match (&st.verdict, &v) {
                        (_, Verdict::Undecided) => false,
                        (Some(Verdict::Person { id: a, .. }), Verdict::Person { id: b, .. }) => a != b,
                        (Some(old), new) => std::mem::discriminant(old) != std::mem::discriminant(new),
                        (None, _) => true,
                    };
                    if changed {
                        st.verdict = Some(v.clone());
                        events.push(VisionEvent::Identified { track: t.id, verdict: v, at: now });
                    }
                    st.embedding = Some(emb);
                }
                st.face = Some(face);
            }
        }

        if let Some(o) = self.objects.as_mut().filter(|o| !o.kinds.is_empty() && o.last.is_none_or(|at| now - at >= o.every)) {
            o.run(now, frame, &mut events)?;
        }

        // Une piste immobile qu'on laisse patienter sans la voir n'est pas montrée (ni comptée
        // comme présence) au-delà du délai habituel.
        let fresh = self.tracker.max_gap;
        let views = self
            .tracker
            .tracks()
            .iter()
            .filter(|t| t.confirmed && now - t.last_seen <= fresh)
            .map(|t| {
                let st = self.face_states.get(&t.id);
                PersonView {
                    track: t.id,
                    bbox: t.bbox,
                    score: t.score,
                    pose: t.pose.clone(),
                    verdict: st.and_then(|s| s.verdict.clone()).unwrap_or(Verdict::Undecided),
                    face: st.and_then(|s| s.face.clone()),
                    embedding: st.and_then(|s| s.embedding.clone()),
                }
            })
            .collect();
        Ok((views, events))
    }
}

#[cfg(test)]
mod tests {
    use super::super::actions::{Action, Rules};
    use super::super::faces::normalize;
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Détecteur imité : renvoie ce qu'on lui a préparé pour l'image en cours.
    struct Script(Arc<Mutex<Vec<Detection>>>);
    impl PersonDetector for Script {
        fn detect(&mut self, _f: &Frame) -> Result<Vec<Detection>> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    /// Un visage au milieu du haut de chaque région demandée.
    struct Faces;
    impl FaceDetector for Faces {
        fn detect(&mut self, _f: &Frame, roi: BBox) -> Result<Vec<FaceBox>> {
            let c = roi.center();
            let b = BBox::new(c.x - 0.03, roi.y0 + 0.02, c.x + 0.03, roi.y0 + 0.1);
            Ok(vec![FaceBox { bbox: b, landmarks: [c; 5], score: 0.9 }])
        }
    }

    /// Empreinte imitée : dépend de la position horizontale du visage (gauche = Alice).
    struct Embed;
    impl FaceEmbedder for Embed {
        fn embed(&mut self, _f: &Frame, face: &FaceBox) -> Result<Vec<f32>> {
            let mut v = if face.bbox.center().x < 0.5 { vec![1.0, 0.1, 0.0] } else { vec![0.0, 0.2, 1.0] };
            normalize(&mut v);
            Ok(v)
        }
        fn model(&self) -> &str {
            "imitation"
        }
    }

    fn person(x: f32) -> Detection {
        Detection { bbox: BBox::new(x, 0.2, x + 0.12, 0.9), score: 0.9, pose: None }
    }

    #[test]
    fn people_are_followed_named_and_strangers_flagged() {
        let script = Arc::new(Mutex::new(vec![person(0.2), person(0.7)]));
        let mut p = Pipeline::new(Box::new(Script(script.clone())), Some((Box::new(Faces), Box::new(Embed))), ActionEngine::new(vec![], Rules::default()));
        let mut gallery = FaceGallery { model: "imitation".into(), people: vec![] };
        gallery.enroll("Alice", &[1.0, 0.1, 0.0]);
        let frame = Frame { seq: 1, width: 1280, height: 720, rgba: vec![0; 1280 * 720 * 4] };
        let mut events = Vec::new();
        let mut views = Vec::new();
        for i in 0..12 {
            let (v, e) = p.process(i as f64 * 0.5, &frame, &gallery).unwrap();
            events.extend(e);
            views = v;
        }
        let arrived = events.iter().filter(|e| matches!(e, VisionEvent::Action(ActionEvent { action: Action::Arrived, .. }))).count();
        assert_eq!(arrived, 2);
        let named: Vec<&Verdict> = events.iter().filter_map(|e| if let VisionEvent::Identified { verdict, .. } = e { Some(verdict) } else { None }).collect();
        assert_eq!(named.len(), 2, "une annonce par personne, pas à chaque image : {named:?}");
        assert!(named.iter().any(|v| matches!(v, Verdict::Person { name, .. } if name == "Alice")));
        assert!(named.contains(&&Verdict::Stranger));
        assert_eq!(views.len(), 2);
        assert!(views.iter().all(|v| v.face.is_some()));
    }

    /// Un chat imité, toujours au même endroit.
    struct Cat;
    impl ObjectDetector for Cat {
        fn detect(&mut self, _f: &Frame) -> Result<Vec<ObjectDetection>> {
            Ok(vec![
                ObjectDetection { kind: ObjectKind::Cat, bbox: BBox::new(0.5, 0.7, 0.6, 0.8), score: 0.8 },
                ObjectDetection { kind: ObjectKind::Car, bbox: BBox::new(0.1, 0.4, 0.4, 0.6), score: 0.9 },
            ])
        }
    }

    #[test]
    fn animals_at_their_own_pace_and_only_those_asked() {
        let script = Arc::new(Mutex::new(vec![]));
        let mut p = Pipeline::new(Box::new(Script(script)), None, ActionEngine::default()).with_objects(Box::new(Cat), vec![ObjectKind::Cat], 0.5);
        let frame = Frame { seq: 1, width: 640, height: 480, rgba: vec![0; 640 * 480 * 4] };
        let gallery = FaceGallery::default();
        let mut events = Vec::new();
        // 10 analyses par seconde pendant 1,5 s : les objets, eux, toutes les 0,5 s.
        for i in 0..15 {
            events.extend(p.process(i as f64 * 0.1, &frame, &gallery).unwrap().1);
        }
        let objects: Vec<&VisionEvent> = events.iter().filter(|e| matches!(e, VisionEvent::Object { .. })).collect();
        assert!(matches!(objects[..], [VisionEvent::Object { kind: ObjectKind::Cat, change: ObjectChange::Appeared, .. }]), "{objects:?}");
        assert_eq!(p.objects().len(), 1, "la voiture n'était pas demandée");
    }

    /// Second modèle imité : voit une personne à gauche seulement ; compte ses passages.
    struct Second(Arc<Mutex<u32>>);
    impl ObjectDetector for Second {
        fn detect(&mut self, _f: &Frame) -> Result<Vec<ObjectDetection>> {
            *self.0.lock().unwrap() += 1;
            Ok(vec![])
        }
        fn last_persons(&self) -> Vec<(BBox, f32)> {
            vec![(BBox::new(0.2, 0.2, 0.32, 0.9), 0.6)]
        }
    }

    #[test]
    fn doubtful_people_need_a_second_opinion() {
        // À gauche, une vraie personne (score modeste, vue aussi par le second modèle) ; à droite,
        // une silhouette douteuse que lui ne voit pas (manteau, reflet…) ; au milieu, une nette.
        let weak = |x: f32| Detection { score: 0.55, ..person(x) };
        let script = Arc::new(Mutex::new(vec![weak(0.2), weak(0.7), Detection { score: 0.9, ..person(0.45) }]));
        let runs = Arc::new(Mutex::new(0));
        let mut p = Pipeline::new(Box::new(Script(script)), None, ActionEngine::default())
            .with_objects(Box::new(Second(runs.clone())), vec![], 0.5)
            .with_person_check(PersonCheck::default());
        let frame = Frame { seq: 1, width: 640, height: 480, rgba: vec![0; 640 * 480 * 4] };
        let gallery = FaceGallery::default();
        let mut views = Vec::new();
        for i in 0..20 {
            views = p.process(i as f64 * 0.1, &frame, &gallery).unwrap().0;
        }
        let mut xs: Vec<f32> = views.iter().map(|v| v.bbox.x0).collect();
        xs.sort_by(f32::total_cmp);
        assert_eq!(xs, vec![0.2, 0.45], "la silhouette douteuse de droite est écartée");
        // L'avis est réutilisé 0,3 s : 2 s à 10 analyses/s → bien moins de 20 passages.
        let n = *runs.lock().unwrap();
        assert!((5..=8).contains(&n), "{n} passages du second modèle");
    }

    #[test]
    fn embeddings_from_another_model_are_not_compared() {
        let script = Arc::new(Mutex::new(vec![person(0.2)]));
        let mut p = Pipeline::new(Box::new(Script(script)), Some((Box::new(Faces), Box::new(Embed))), ActionEngine::default());
        let mut gallery = FaceGallery { model: "autre-modèle".into(), people: vec![] };
        gallery.enroll("Alice", &[1.0, 0.1, 0.0]);
        let frame = Frame { seq: 1, width: 640, height: 480, rgba: vec![0; 640 * 480 * 4] };
        for i in 0..10 {
            let (_, e) = p.process(i as f64 * 0.5, &frame, &gallery).unwrap();
            assert!(!e.iter().any(|e| matches!(e, VisionEvent::Identified { .. })));
        }
    }
}
