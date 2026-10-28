//! `homesmart-cli vision …` : modèles d'IA (état, installation), essais sur une image.

use anyhow::Result;
use clap::Subcommand;
use homesmart::vision::{engine, models};
use std::path::Path;
use std::time::Instant;

#[derive(Subcommand)]
pub enum VisionCmd {
    /// Modèles d'IA : état ; avec --install, téléchargement vérifié depuis leur source officielle
    Models {
        #[arg(long)]
        install: bool,
    },
    /// Charge chaque modèle installé : accélération utilisée, entrées et sorties attendues
    Info {
        /// Processeur seulement (sinon la carte graphique si possible)
        #[arg(long)]
        cpu: bool,
    },
    /// Essai sur un passage de la carte SD : personnes, visages, cohérence des empreintes,
    /// temps de calcul. N'affiche que des chiffres (aucune image n'est enregistrée).
    Replay {
        camera: String,
        /// Début, ex. "2026-10-20 20:52:59"
        #[arg(long)]
        start: String,
        /// Fin
        #[arg(long)]
        end: String,
        /// Images analysées par seconde
        #[arg(long, default_value_t = 4.0)]
        fps: f32,
        #[arg(long)]
        cpu: bool,
        /// Aussi les objets (YOLOX, toutes classes COCO), comparés au modèle des personnes
        #[arg(long)]
        objects: bool,
    },
    /// Toute la chaîne de l'application (personnes, chutes, animaux, sons, voix) sur une journée
    /// de la carte SD : nombre d'événements qu'elle aurait journalisés. Chiffres seulement.
    Audit {
        camera: String,
        /// Jour, ex. "2026-10-21" (aujourd'hui par défaut)
        #[arg(long)]
        date: Option<String>,
        /// Seulement les séquences commencées avant cette heure, ex. "19:00" (pour comparer deux
        /// essais sur les mêmes séquences)
        #[arg(long)]
        until: Option<String>,
        #[arg(long, default_value_t = 8.0)]
        fps: f32,
        #[arg(long)]
        cpu: bool,
    },
    /// La chaîne de l'application sur le direct : pistes, scores, postures, événements (chiffres seulement)
    Probe {
        camera: String,
        #[arg(long, default_value_t = 30)]
        secs: u64,
        #[arg(long, default_value_t = 8.0)]
        fps: f32,
        /// Flux fluide (sinon HD)
        #[arg(long)]
        sub: bool,
        #[arg(long)]
        cpu: bool,
    },
    /// Présence vue par une caméra : minutes occupées, heure par heure
    Presence {
        camera: String,
        /// Jour, ex. "2026-10-21" (aujourd'hui par défaut)
        #[arg(long)]
        date: Option<String>,
    },
    /// Entrées et sorties d'un fichier .onnx (développement)
    Inspect {
        file: std::path::PathBuf,
        #[arg(long)]
        cpu: bool,
    },
    /// Qualité mesurée sur des images et vidéos réelles annotées (jeu d'évaluation local)
    Eval {
        #[command(subcommand)]
        what: super::eval::EvalCmd,
    },
    /// Coups et violence sur des vidéos réelles : violences trouvées et fausses alertes
    ViolenceEval(super::violence::ViolenceEval),
}

pub async fn run(cmd: VisionCmd, path: &Path) -> Result<()> {
    match cmd {
        VisionCmd::Models { install } => list_models(install).await,
        VisionCmd::Info { cpu } => info(cpu),
        VisionCmd::Replay { camera, start, end, fps, cpu, objects } => replay(path, &camera, &start, &end, fps, cpu, objects).await,
        VisionCmd::Audit { camera, date, until, fps, cpu } => audit(path, &camera, date, until, fps, cpu).await,
        VisionCmd::Probe { camera, secs, fps, sub, cpu } => probe(path, &camera, secs, fps, sub, cpu).await,
        VisionCmd::Presence { camera, date } => {
            use homesmart::vision::presence::{fmt_minutes, PresenceStore};
            let c = super::cam::find(path, &camera)?;
            let day = match date {
                Some(d) => chrono::NaiveDate::parse_from_str(&d, "%Y-%m-%d")?,
                None => chrono::Local::now().date_naive(),
            };
            let store = PresenceStore::load(path.with_file_name("presence.json"));
            let hours = store.day(&c.id, day);
            println!("{} — {day} : présence {}", c.display_name(), fmt_minutes(hours.iter().sum()));
            for (h, m) in hours.iter().enumerate().filter(|(_, m)| **m > 0.0) {
                println!("  {h:02} h  {:<30} {}", "█".repeat((m / 2.0).ceil() as usize), fmt_minutes(*m));
            }
            Ok(())
        }
        VisionCmd::Inspect { file, cpu } => {
            let t0 = Instant::now();
            let l = engine::load(&file, if cpu { engine::Accel::Cpu } else { engine::Accel::Auto }, &[])?;
            println!("{} — {} (chargé en {} ms)", file.display(), l.backend, t0.elapsed().as_millis());
            for i in l.session.inputs() {
                println!("  entrée  {} {:?}", i.name(), i.dtype());
            }
            for o in l.session.outputs() {
                println!("  sortie  {} {:?}", o.name(), o.dtype());
            }
            Ok(())
        }
        VisionCmd::Eval { what } => tokio::task::spawn_blocking(move || super::eval::run(what)).await?,
        VisionCmd::ViolenceEval(a) => tokio::task::spawn_blocking(move || super::violence::run(a)).await?,
    }
}

/// Toute la chaîne de l'application sur une journée de la carte SD (lecture seule) : combien
/// d'événements elle aurait journalisés, par sorte et par gravité. Chiffres seulement : ni
/// image, ni son, ni texte, ni nom de son ou de personne.
async fn audit(path: &Path, camera: &str, date: Option<String>, until: Option<String>, fps: f32, cpu: bool) -> Result<()> {
    use homesmart::camera::{playback, stream::AudioSink, stream::FrameSink, Frame};
    use homesmart::listen::dsp::Resampler;
    use homesmart::listen::sounds::{Gravity, SoundDetector, SoundTagger};
    use homesmart::listen::vad::{SegmentRules, Segmenter, SileroVad, CHUNK};
    use homesmart::vision::actions::{Action, ActionEngine, Rules};
    use homesmart::vision::faces::FaceGallery;
    use homesmart::vision::objects::ObjectKind;
    use homesmart::vision::onnx::{RtmoPersons, YoloxObjects};
    use homesmart::vision::pipeline::{PersonCheck, Pipeline, VisionEvent};
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicBool;
    use std::sync::{mpsc, Arc, Mutex};

    let c = super::cam::find(path, camera)?;
    let day = match date {
        Some(d) => chrono::NaiveDate::parse_from_str(&d, "%Y-%m-%d")?,
        None => chrono::Local::now().date_naive(),
    };
    let accel = if cpu { engine::Accel::Cpu } else { engine::Accel::Auto };
    // Lire la carte SD est une action : journalisée (« Ligne de commande »), comme dans l'application.
    super::jlog(path, homesmart::journal::Level::Info, &c.id, c.display_name(), format!("Analyse de la carte SD du {day} (essai de la chaîne IA, chiffres seulement)"));
    let detail = playback::scan_day_detail(&c, day, None, &|_| {}).await;
    let until = until.map(|u| chrono::NaiveTime::parse_from_str(&u, "%H:%M")).transpose()?;
    let clips: Vec<_> = detail.clips.iter().filter(|k| until.is_none_or(|u| k.start.with_timezone(&chrono::Local).time() < u)).cloned().collect();
    println!("{} — {} : {} séquence(s), {:.0} s de vidéo, {fps} analyses/s", c.display_name(), day, clips.len(), clips.iter().map(|k| (k.end - k.start).num_seconds()).sum::<i64>());
    let kinds: Vec<ObjectKind> = [ObjectKind::Cat, ObjectKind::Dog, ObjectKind::Bird, ObjectKind::Horse, ObjectKind::Sheep, ObjectKind::Cow, ObjectKind::WildAnimal].to_vec();
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut sound_counts = [0u32; 3];
    let mut critical: Vec<String> = Vec::new();
    let (mut frames, mut speech_segments, mut audio_secs) = (0u64, 0u32, 0f64);
    let gallery = FaceGallery::default();
    let tagger = Arc::new(Mutex::new(SoundTagger::load(&models::CED_MINI.path(), &models::CED_LABELS.path())?));
    let mut vad = SileroVad::load(&models::SILERO_VAD.path())?;
    for clip in &clips {
        // Une chaîne neuve par séquence : chacune commence pièce vide, comme dans la réalité.
        let mut persons = RtmoPersons::new(accel)?;
        persons.threshold = 0.25;
        let mut p = Pipeline::new(Box::new(persons), None, ActionEngine::new(Vec::new(), Rules::default()))
            .with_objects(Box::new(YoloxObjects::new(accel)?), kinds.clone(), 0.5)
            .with_person_check(PersonCheck::default());
        let (tx, rx) = mpsc::sync_channel::<Frame>(4);
        let sink: FrameSink = Arc::new(move |f| {
            let _ = tx.send(f);
        });
        let audio: Arc<Mutex<Vec<f32>>> = Arc::default();
        let a2 = audio.clone();
        let rate = Arc::new(Mutex::new(16_000u32));
        let r2 = rate.clone();
        let asink: AudioSink = Arc::new(move |s: &[f32], r: u32| {
            a2.lock().unwrap().extend_from_slice(s);
            *r2.lock().unwrap() = r;
        });
        let cfg = c.clone();
        let (s, e) = (clip.start, clip.end);
        let opts = playback::ReplayOptions { audio: Some(asink), ..playback::ReplayOptions::default() };
        let decoder = std::thread::spawn(move || playback::decode_blocking(&cfg, s, e, 1280, 720, Arc::new(AtomicBool::new(false)), sink, || {}, opts));
        // Horloge de l'analyse : celle de la vidéo (une image sur n pour la cadence demandée).
        let src_fps = 25.0f64;
        let every = (src_fps / fps.max(0.5) as f64).round().max(1.0) as u64;
        let mut i = 0u64;
        for frame in rx {
            i += 1;
            if i % every != 0 {
                continue;
            }
            frames += 1;
            let now = i as f64 / src_fps;
            let (_, events) = p.process(now, &frame, &gallery)?;
            for e in events {
                let key = match e {
                    VisionEvent::Action(a) => match a.action {
                        Action::Arrived => "personne arrivée",
                        Action::Left { .. } => "personne partie",
                        Action::Fall => "CHUTE",
                        Action::Down { .. } => "AU SOL",
                        Action::GotUp => "relevée",
                        Action::HandRaised => "main levée",
                        Action::ArmsUp => "BRAS LEVÉS",
                        Action::Still { .. } => "immobile",
                        _ => "zone",
                    },
                    VisionEvent::Object { change, .. } => match change {
                        homesmart::vision::objects::ObjectChange::Appeared => "animal apparu",
                        _ => "animal parti",
                    },
                    VisionEvent::Identified { .. } => "visage",
                    VisionEvent::Violence(_) => "VIOLENCE",
                };
                *counts.entry(key.to_string()).or_default() += 1;
            }
        }
        let _ = decoder.join();
        // Le son de la séquence : sons de la maison (par gravité) et phrases détectées.
        let pcm = std::mem::take(&mut *audio.lock().unwrap());
        let r = *rate.lock().unwrap();
        let mut x = Vec::new();
        Resampler::new(r, 16_000).process(&pcm, &mut x);
        audio_secs += x.len() as f64 / 16_000.0;
        let labels = tagger.lock().unwrap().labels.clone();
        let mut det = SoundDetector::new(&labels);
        let t = tagger.clone();
        for ev in det.push(&x, &mut |w| t.lock().unwrap().scores(w))? {
            // Alertes critiques (alarme incendie, verre brisé, détonation) : laquelle, et à quel score,
            // pour régler les seuils — rien d'autre sur le son de la maison.
            if ev.sound.gravity() == Gravity::Critical {
                critical.push(format!("{} {:.2}", ev.sound.label(), ev.score));
            }
            sound_counts[match ev.sound.gravity() {
                Gravity::Critical => 0,
                Gravity::Warning => 1,
                Gravity::Info => 2,
            }] += 1;
        }
        vad.reset();
        let mut seg = Segmenter::new(SegmentRules::default());
        for ch in x.chunks_exact(CHUNK) {
            if seg.push(ch, vad.prob(ch)?).is_some() {
                speech_segments += 1;
            }
        }
    }
    println!("  {frames} images analysées · {:.0} s de son", audio_secs);
    for (k, n) in &counts {
        println!("  {k:<18} {n}");
    }
    println!("  sons reconnus : {} critique(s), {} avertissement(s), {} info(s) · phrases détectées : {speech_segments}", sound_counts[0], sound_counts[1], sound_counts[2]);
    if !critical.is_empty() {
        println!("  alertes critiques : {}", critical.join(", "));
    }
    Ok(())
}

/// La chaîne de l'application sur le direct d'une caméra, en chiffres : pistes (score, âge de la
/// dernière détection sûre, boîte, posture), avis du second modèle, événements. Ni image, ni nom.
async fn probe(path: &Path, camera: &str, secs: u64, fps: f32, sub: bool, cpu: bool) -> Result<()> {
    use homesmart::camera::{stream, Frame, Quality};
    use homesmart::vision::actions::{posture, ActionEngine, Rules};
    use homesmart::vision::faces::FaceGallery;
    use homesmart::vision::objects::ObjectKind;
    use homesmart::vision::onnx::{RtmoPersons, YoloxObjects};
    use homesmart::vision::pipeline::{PersonCheck, Pipeline, VisionEvent};
    use std::sync::{mpsc, Arc};

    let c = super::cam::find(path, camera)?;
    let q = if sub { Quality::Sub } else { Quality::Main };
    super::jlog(path, homesmart::journal::Level::Info, &c.id, c.display_name(), format!("Vidéo en direct ouverte (essai de la chaîne IA, {secs} s, chiffres seulement)"));
    let accel = if cpu { engine::Accel::Cpu } else { engine::Accel::Auto };
    let (tx, rx) = mpsc::sync_channel::<Frame>(2);
    let sink: stream::FrameSink = Arc::new(move |f| {
        let _ = tx.try_send(f);
    });
    let worker = std::thread::spawn(move || -> Result<()> {
        let mut persons = RtmoPersons::new(accel)?;
        persons.threshold = 0.25;
        let mut p = Pipeline::new(Box::new(persons), None, ActionEngine::new(Vec::new(), Rules::default()))
            .with_objects(Box::new(YoloxObjects::new(accel)?), Vec::<ObjectKind>::new(), 0.5)
            .with_person_check(PersonCheck::default());
        let gallery = FaceGallery::default();
        let t0 = std::time::Instant::now();
        let every = std::time::Duration::from_secs_f32(1.0 / fps.max(0.5));
        let mut last = std::time::Instant::now() - every;
        let mut last_print = -1i64;
        for frame in rx {
            if last.elapsed() < every {
                continue;
            }
            last = std::time::Instant::now();
            let now = t0.elapsed().as_secs_f64();
            let (views, events) = p.process(now, &frame, &gallery)?;
            for e in &events {
                if let VisionEvent::Action(a) = e {
                    println!("  {now:6.1} s  ÉVÉNEMENT piste {} : {:?}", a.track, a.action);
                }
            }
            if now as i64 != last_print {
                last_print = now as i64;
                let (fw, fh) = (frame.width as f32, frame.height as f32);
                let mut line = format!("  {now:6.1} s  {} piste(s)", views.len());
                for v in &views {
                    let t = p.tracker.track(v.track);
                    let post = t.map(|t| format!("{:?}", posture(t, fw, fh))).unwrap_or_default();
                    let pts = v.pose.as_ref().map_or(0, |po| po.points.iter().filter(|k| k.score >= 0.3).count());
                    line.push_str(&format!(
                        " | #{} score {:.2} boîte [{:.2} {:.2} {:.2} {:.2}] {post} points {pts}",
                        v.track, v.score, v.bbox.x0, v.bbox.y0, v.bbox.x1, v.bbox.y1
                    ));
                }
                println!("{line}");
            }
        }
        Ok(())
    });
    let url = c.rtsp_url(q);
    let run = stream::run(&url, &c.username, &c.password, sink, |_| {});
    let _ = tokio::time::timeout(std::time::Duration::from_secs(secs), run).await;
    let r = worker.join().map_err(|_| anyhow::anyhow!("analyse interrompue"))?;
    super::jlog(path, homesmart::journal::Level::Info, &c.id, c.display_name(), "Vidéo en direct fermée (essai de la chaîne IA)");
    r
}

/// Essai de bout en bout sur la carte SD (lecture seule) : chiffres uniquement.
async fn replay(path: &Path, camera: &str, start: &str, end: &str, fps: f32, cpu: bool, objects: bool) -> Result<()> {
    use homesmart::camera::{playback, stream::FrameSink, Frame};
    use homesmart::vision::faces::cosine;
    use homesmart::vision::onnx::{RtmoPersons, SfaceEmbedder, YunetFaces};
    use homesmart::vision::pipeline::{FaceDetector, FaceEmbedder, PersonDetector, Pipeline};
    use homesmart::vision::track::Tracker;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;
    use std::sync::{mpsc, Arc};

    let c = super::cam::find(path, camera)?;
    let (s, e) = (super::cam::parse_dt(start)?, super::cam::parse_dt(end)?);
    let accel = if cpu { engine::Accel::Cpu } else { engine::Accel::Auto };
    let (mut persons, mut faces, mut embedder) = (RtmoPersons::new(accel)?, YunetFaces::new(accel)?, SfaceEmbedder::new(accel)?);
    // Seuil d'ouverture d'une piste dans l'application (le modèle renvoie aussi les plus faibles).
    persons.threshold = Pipeline::NEW_PERSON;
    let mut yolox = if objects { Some(homesmart::vision::onnx::YoloxObjects::new(accel)?) } else { None };
    let mut class_counts: HashMap<usize, u32> = HashMap::new();
    let (mut iou_sum, mut iou_n, mut t_obj) = (0f32, 0u32, 0f64);
    println!("{} — analyse de {} → {} ({fps} i/s) · personnes : {} · visages : {} · empreintes : {}", c.display_name(), s.format("%H:%M:%S"), e.format("%H:%M:%S"), persons.backend, faces.backend, embedder.backend);

    // Relecture pleine définition (2304×1296 sur la Ranger 2C) : les visages lointains y sont nets.
    let (tx, rx) = mpsc::sync_channel::<Frame>(2);
    let sink: FrameSink = Arc::new(move |f| {
        let _ = tx.try_send(f);
    });
    let cfg = c.clone();
    let (su, eu) = (s.with_timezone(&chrono::Utc), e.with_timezone(&chrono::Utc));
    let decoder = std::thread::spawn(move || playback::decode_blocking(&cfg, su, eu, 2304, 1296, Arc::new(AtomicBool::new(false)), sink, || {}, playback::ReplayOptions::default()));

    let mut tracker = Tracker::default();
    let mut embeddings: HashMap<u32, Vec<Vec<f32>>> = HashMap::new();
    let (mut frames, mut with_people, mut faces_seen) = (0u32, 0u32, 0u32);
    let (mut t_person, mut t_face, mut t_embed, mut n_face, mut n_embed) = (0f64, 0f64, 0f64, 0u32, 0u32);
    let every = std::time::Duration::from_secs_f32(1.0 / fps.max(0.2));
    let mut last = Instant::now() - every;
    let t_start = Instant::now();
    for frame in rx {
        if last.elapsed() < every {
            continue;
        }
        last = Instant::now();
        frames += 1;
        let t0 = Instant::now();
        let dets = persons.detect(&frame)?;
        t_person += t0.elapsed().as_secs_f64();
        tracker.update(t_start.elapsed().as_secs_f64(), &dets);
        if !dets.is_empty() {
            with_people += 1;
        }
        let mut line = format!("  image {frames:>3} : {} personne(s)", dets.len());
        for t in tracker.tracks().iter().filter(|t| t.last_seen >= t_start.elapsed().as_secs_f64() - 0.01) {
            let t1 = Instant::now();
            let found = faces.detect(&frame, Pipeline::head_region(&t.bbox))?;
            t_face += t1.elapsed().as_secs_f64();
            n_face += 1;
            let best = found.into_iter().max_by(|a, b| a.score.total_cmp(&b.score));
            match best {
                Some(f) => {
                    faces_seen += 1;
                    let px = f.bbox.width() * frame.width as f32;
                    let t2 = Instant::now();
                    let emb = embedder.embed(&frame, &f)?;
                    t_embed += t2.elapsed().as_secs_f64();
                    n_embed += 1;
                    line.push_str(&format!(" · #{} visage {px:.0} px ({:.2})", t.id, f.score));
                    embeddings.entry(t.id).or_default().push(emb);
                }
                None => line.push_str(&format!(" · #{} pas de visage", t.id)),
            }
        }
        if let Some(y) = yolox.as_mut() {
            let t3 = Instant::now();
            let found = y.detect_raw(&frame, true)?;
            t_obj += t3.elapsed().as_secs_f64();
            for (class, bbox, _) in &found {
                *class_counts.entry(*class).or_default() += 1;
                // Vérification du décodage : la personne de YOLOX recouvre-t-elle celle de RTMO ?
                if *class == 0 {
                    if let Some(best) = dets.iter().map(|d| d.bbox.iou(bbox)).fold(None, |m: Option<f32>, v| Some(m.map_or(v, |m| m.max(v)))) {
                        iou_sum += best;
                        iou_n += 1;
                    }
                }
            }
        }
        if !dets.is_empty() {
            let scores: Vec<String> = dets.iter().map(|d| format!("{:.2}", d.score)).collect();
            println!("{line}  [scores {}]", scores.join(", "));
        }
    }
    let _ = decoder.join();
    println!("  {frames} image(s) analysée(s), {with_people} avec quelqu'un, {faces_seen} visage(s)");
    let ms = |t: f64, n: u32| if n > 0 { t * 1000.0 / n as f64 } else { 0.0 };
    println!("  temps moyen : personnes {:.1} ms · visages {:.1} ms · empreinte {:.1} ms", ms(t_person, frames), ms(t_face, n_face), ms(t_embed, n_embed));
    if yolox.is_some() {
        let mut counts: Vec<(usize, u32)> = class_counts.into_iter().collect();
        counts.sort_by_key(|(c, _)| *c);
        let list: Vec<String> = counts.iter().map(|(c, n)| format!("classe {c} ×{n}")).collect();
        println!("  objets (YOLOX, {:.1} ms) : {}", ms(t_obj, frames), if list.is_empty() { "aucun".to_string() } else { list.join(", ") });
        if iou_n > 0 {
            println!("  personne YOLOX vs RTMO : recouvrement moyen {:.2} sur {iou_n} détection(s)", iou_sum / iou_n as f32);
        }
    }
    // Cohérence : les empreintes d'une même piste doivent se ressembler (même personne).
    for (id, v) in &embeddings {
        if v.len() < 2 {
            continue;
        }
        let mut sims = Vec::new();
        for i in 0..v.len() {
            for j in i + 1..v.len() {
                sims.push(cosine(&v[i], &v[j]));
            }
        }
        let mean = sims.iter().sum::<f32>() / sims.len() as f32;
        let min = sims.iter().cloned().fold(f32::MAX, f32::min);
        println!("  piste #{id} : {} empreintes, ressemblance moyenne {mean:.2} (min {min:.2}) — seuil de reconnaissance 0,36", v.len());
    }
    Ok(())
}

async fn list_models(install: bool) -> Result<()> {
    println!("Modèles d'IA ({}) :", models::models_dir().display());
    for m in models::ALL {
        let state = if m.is_installed() { "installé" } else { "absent" };
        println!("  {:<20} {:<28} {:>6.1} Mo  {:<11} {state}", m.id, m.role, m.size as f64 / 1e6, m.license);
    }
    if !install {
        if models::ALL.iter().any(|m| !m.is_installed()) {
            println!("Installe-les avec : homesmart-cli vision models --install");
        }
        return Ok(());
    }
    for m in models::ALL.iter().filter(|m| !m.is_installed()) {
        println!("Téléchargement de {} depuis {}", m.id, m.url);
        let spec = *m;
        let t0 = Instant::now();
        tokio::task::spawn_blocking(move || {
            let shown = std::sync::Mutex::new(0u64);
            models::ensure(&spec, &|got, total| {
                let pct = got * 100 / total.max(1);
                let mut s = shown.lock().unwrap();
                if pct / 20 != *s / 20 {
                    *s = pct;
                    println!("  … {pct} %");
                }
            })
        })
        .await??;
        println!("  vérifié (SHA-256) et installé en {:.1} s", t0.elapsed().as_secs_f32());
    }
    Ok(())
}

fn info(cpu: bool) -> Result<()> {
    let accel = if cpu { engine::Accel::Cpu } else { engine::Accel::Auto };
    for m in models::ALL.iter().filter(|m| m.is_installed()) {
        let t0 = Instant::now();
        let l = engine::load(&m.path(), accel, &[("batch", 1)])?;
        println!("{} — {} (chargé en {} ms)", m.id, l.backend, t0.elapsed().as_millis());
        println!("  {}", engine::describe(&l.session));
    }
    Ok(())
}
