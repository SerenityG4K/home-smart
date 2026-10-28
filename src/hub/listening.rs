//! Écoute par IA dans le hub : le micro d'une caméra, analysé en continu (sons de la maison,
//! et parole transcrite si l'option est choisie). Journal : source « Écoute IA ».
//!
//! Le son n'est jamais enregistré : il passe en mémoire par blocs de quelques secondes puis est
//! oublié. Ne vont au journal que les sons reconnus et, seulement si la transcription est
//! activée, le texte entendu (avec une alerte si on y entend un appel à l'aide).

use super::{Event, Hub, Level, Source};
use crate::camera::stream::AudioSink;
use crate::camera::Quality;
use crate::listen::dsp::{rms_dbfs, Resampler};
use crate::automation::Trigger;
use crate::listen::sounds::{Gravity, HomeSound, SoundDetector, SoundTagger};
use crate::listen::vad::{SegmentRules, Segmenter, SileroVad, Speech, CHUNK};
use crate::listen::whisper::Whisper;
use crate::vision::engine::Accel;
use crate::vision::models::{self, ModelSpec};
use crate::vision::CameraVision;
use anyhow::{anyhow, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Ce que l'écoute a entendu récemment, pour l'interface.
#[derive(Clone, Debug, Default)]
pub struct Listening {
    pub running: bool,
    pub error: Option<String>,
    /// Niveau sonore du moment (dBFS) ; le micro n'a encore rien envoyé : `None`.
    pub level: Option<f32>,
    /// Derniers sons reconnus et phrases entendues (le plus récent d'abord).
    pub heard: Vec<(Instant, String)>,
    /// Derniers rires entendus (jamais signalés : ils servent à reconnaître une chamaillerie).
    pub laughed: Option<Instant>,
    /// Dernier cri ou pleurs entendus.
    pub distress: Option<Instant>,
}

/// Mots qui, entendus, déclenchent une alerte.
pub const HELP_WORDS: [&str; 6] = ["au secours", "à l'aide", "aidez-moi", "aide-moi", "appelez les secours", "appelle les secours"];

/// Une phrase contient-elle un appel à l'aide ? (sans tenir compte des accents ni de la casse)
pub fn calls_for_help(text: &str) -> bool {
    let fold = |s: &str| -> String {
        s.to_lowercase()
            .chars()
            .map(|c| match c {
                'à' | 'â' | 'ä' => 'a',
                'é' | 'è' | 'ê' | 'ë' => 'e',
                'î' | 'ï' => 'i',
                'ô' | 'ö' => 'o',
                'ù' | 'û' | 'ü' => 'u',
                'ç' => 'c',
                '’' | '-' => '\'',
                c => c,
            })
            .collect::<String>()
            .replace('\'', " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let t = fold(text);
    HELP_WORDS.iter().any(|w| t.contains(&fold(w)))
}

fn missing(list: &[ModelSpec]) -> Vec<&'static str> {
    list.iter().filter(|m| !m.is_installed()).map(|m| m.id).collect()
}

/// Modèles de l'écoute encore à télécharger pour ces réglages (Mo).
pub fn missing_mb(cv: &CameraVision) -> f64 {
    let mut list: Vec<ModelSpec> = Vec::new();
    if cv.sounds {
        list.extend(models::SOUNDS);
    }
    if cv.speech {
        list.extend(models::SPEECH);
    }
    list.iter().filter(|m| !m.is_installed()).map(|m| m.size as f64 / 1e6).sum()
}

impl Hub {
    pub fn camera_listening(&self, id: &str) -> Option<Listening> {
        self.analysis.lock().unwrap().listening.get(id).cloned()
    }

    fn set_listening(&self, id: &str, f: impl FnOnce(&mut Listening)) {
        self.note_listening(id, f);
        self.emit(Event::Cameras);
    }

    /// Comme `set_listening`, sans prévenir l'interface (rien d'affiché ne change).
    fn note_listening(&self, id: &str, f: impl FnOnce(&mut Listening)) {
        if let Some(l) = self.analysis.lock().unwrap().listening.get_mut(id) {
            f(l);
        }
    }

    /// Lance l'écoute d'une caméra (fil séparé de l'image), si ses réglages le demandent.
    pub(super) fn start_listening(self: &Arc<Self>, id: &str, cv: &CameraVision, accel: Accel, stop: Arc<AtomicBool>) {
        if !(cv.sounds || cv.speech) {
            return;
        }
        self.analysis.lock().unwrap().listening.insert(id.to_string(), Listening::default());
        let (hub, id, cv) = (self.clone(), id.to_string(), cv.clone());
        let spawned = std::thread::Builder::new().name(format!("homesmart-ecoute-{id}")).spawn(move || {
            if let Err(e) = listen_worker(&hub, &id, &cv, accel, &stop) {
                let msg = format!("{e:#}");
                hub.log(Level::Warning, Source::Listen, &id, format!("Écoute IA impossible : {msg}"));
                hub.set_listening(&id, |l| (l.running, l.error) = (false, Some(msg)));
            }
        });
        if let Err(e) = spawned {
            tracing::error!("fil d'écoute impossible : {e}");
        }
    }

    /// Installe les modèles de l'écoute qui manquent (sons et/ou parole), en tâche de fond.
    pub fn install_listen_models(self: &Arc<Self>, sounds: bool, speech: bool, source: Source) {
        let mut list: Vec<ModelSpec> = Vec::new();
        if sounds {
            list.extend(models::SOUNDS);
        }
        if speech {
            list.extend(models::SPEECH);
        }
        self.install_models(list, "de l'écoute", source);
    }
}

/// Phrase à transcrire.
struct Utterance {
    speech: Speech,
}

fn listen_worker(hub: &Arc<Hub>, id: &str, cv: &CameraVision, accel: Accel, stop: &AtomicBool) -> Result<()> {
    let mut need: Vec<ModelSpec> = Vec::new();
    if cv.sounds {
        need.extend(models::SOUNDS);
    }
    if cv.speech {
        need.extend(models::SPEECH);
    }
    let absent = missing(&need);
    if !absent.is_empty() {
        return Err(anyhow!("modèles de l'écoute non installés ({})", absent.join(", ")));
    }
    let mut tagger = if cv.sounds { Some(SoundTagger::load(&models::CED_MINI.path(), &models::CED_LABELS.path())?) } else { None };
    let mut detector = tagger.as_ref().map(|t| SoundDetector::new(&t.labels));
    let mut vad = if cv.speech { Some(SileroVad::load(&models::SILERO_VAD.path())?) } else { None };
    let mut segmenter = Segmenter::new(SegmentRules::default());
    // Transcription dans son propre fil : une phrase prend 1 à 2 s de calcul, le micro n'attend pas.
    let stt: Option<SyncSender<Utterance>> = if cv.speech {
        let (tx, rx) = sync_channel::<Utterance>(3);
        // Processeur : 8× plus rapide que le temps réel, et les modèles quantifiés (int8) sont
        // fragiles sur DirectML selon les cartes et les pilotes ; le gain y est faible (×1,3).
        let _ = accel;
        let whisper = Whisper::load(&models::WHISPER_ENCODER.path(), &models::WHISPER_DECODER.path(), &models::WHISPER_TOKENIZER.path(), Accel::Cpu)?;
        let (hub, id) = (hub.clone(), id.to_string());
        std::thread::Builder::new().name(format!("homesmart-parole-{id}")).spawn(move || transcriber(&hub, &id, whisper, rx))?;
        Some(tx)
    } else {
        None
    };
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }

    // Le micro : la caméra envoie des blocs décodés (souvent 16 kHz) ; file courte, jamais bloquante.
    let (tx, rx): (SyncSender<(Vec<f32>, u32)>, Receiver<(Vec<f32>, u32)>) = sync_channel(64);
    let tap: AudioSink = Arc::new(move |s: &[f32], rate: u32| {
        let _ = tx.try_send((s.to_vec(), rate));
    });
    hub.camera_listen_tap(id, Some(tap.clone()));
    // Débranché à la sortie, même sur erreur.
    struct Untap<'a>(&'a Hub, &'a str, AudioSink);
    impl Drop for Untap<'_> {
        fn drop(&mut self) {
            self.0.camera_listen_untap(self.1, &self.2);
        }
    }
    let _untap = Untap(hub, id, tap);
    hub.camera_vision_want(id, Some(if cv.hd { Quality::Main } else { Quality::Sub }));
    let what = match (cv.sounds, cv.speech) {
        (true, true) => "sons et parole",
        (true, false) => "sons",
        _ => "parole",
    };
    hub.log(Level::Info, Source::Listen, id, format!("Écoute IA démarrée ({what}, en local, rien n'est enregistré)"));
    hub.set_listening(id, |l| (l.running, l.error) = (true, None));

    let mut resampler: Option<Resampler> = None;
    let (mut pcm, mut pending) = (Vec::<f32>::new(), Vec::<f32>::new());
    let mut last_level = Instant::now();
    let mut started = None::<Instant>;
    let mut waiting_logged = false;
    let mut late_logged = false;
    while !stop.load(Ordering::Relaxed) {
        let (block, rate) = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(b) => b,
            Err(RecvTimeoutError::Timeout) => {
                // Pas de son au bout de 15 s : la caméra n'a pas de micro (ou il est coupé).
                if started.is_none() && !waiting_logged && last_level.elapsed() > Duration::from_secs(15) {
                    waiting_logged = true;
                    hub.set_listening(id, |l| l.error = Some("la caméra n'envoie pas de son (pas de micro, ou micro coupé dans son application)".into()));
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if started.is_none() {
            started = Some(Instant::now());
            hub.set_listening(id, |l| l.error = None);
        }
        if resampler.as_ref().is_none_or(|r| r.rates().0 != rate) {
            resampler = Some(Resampler::new(rate, 16_000));
        }
        pcm.clear();
        resampler.as_mut().unwrap().process(&block, &mut pcm);
        if last_level.elapsed() >= Duration::from_millis(250) {
            last_level = Instant::now();
            let lv = rms_dbfs(&pcm);
            hub.set_listening(id, |l| l.level = Some(lv));
        }
        // Sons de la maison.
        if let (Some(t), Some(d)) = (tagger.as_mut(), detector.as_mut()) {
            let laughed = d.laughed;
            for e in d.push(&pcm, &mut |w| t.scores(w))? {
                if matches!(e.sound, HomeSound::Scream | HomeSound::Crying) {
                    hub.note_listening(id, |l| l.distress = Some(Instant::now()));
                }
                let level = match e.sound.gravity() {
                    Gravity::Critical => Level::Critical,
                    Gravity::Warning => Level::Warning,
                    Gravity::Info => Level::Info,
                };
                let mut chars = e.sound.label().chars();
                let what: String = chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default();
                let photo = (cv.snapshots && level == Level::Critical).then(|| hub.camera_frame(id)).flatten().map(|f| hub.event_photo(id, &f, "son"));
                let msg = match &photo {
                    Some(p) => format!("Son entendu : {} ({:.0} %) — photo : {p}", e.sound.label(), e.score * 100.0),
                    None => format!("Son entendu : {} ({:.0} %)", e.sound.label(), e.score * 100.0),
                };
                hub.log_keyed(level, Source::Listen, id, &format!("ecoute:{id}:{:?}", e.sound), msg);
                match e.sound {
                    HomeSound::SmokeAlarm => hub.scene_trigger(id, Trigger::FireAlarm),
                    HomeSound::GlassBreak => hub.scene_trigger(id, Trigger::GlassBreak),
                    _ => {}
                }
                hub.set_listening(id, |l| {
                    l.heard.insert(0, (Instant::now(), what));
                    l.heard.truncate(6);
                });
            }
            if d.laughed != laughed {
                hub.note_listening(id, |l| l.laughed = Some(Instant::now()));
            }
        }
        // Parole : découpée en phrases, transcrites à part.
        if let (Some(v), Some(tx)) = (vad.as_mut(), stt.as_ref()) {
            pending.extend_from_slice(&pcm);
            let mut start = 0;
            while pending.len() - start >= CHUNK {
                let chunk = &pending[start..start + CHUNK];
                start += CHUNK;
                let p = v.prob(chunk)?;
                if let Some(speech) = segmenter.push(chunk, p) {
                    match tx.try_send(Utterance { speech }) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) if !late_logged => {
                            late_logged = true;
                            hub.log(Level::Warning, Source::Listen, id, "Transcription en retard : des phrases sont sautées (calcul trop lent sur ce PC)");
                        }
                        Err(_) => {}
                    }
                }
            }
            pending.drain(..start);
        }
    }
    hub.log(Level::Info, Source::Listen, id, "Écoute IA arrêtée");
    Ok(())
}

/// Fil de transcription : phrases → texte → journal.
fn transcriber(hub: &Arc<Hub>, id: &str, mut whisper: Whisper, rx: Receiver<Utterance>) {
    for u in rx {
        let t0 = Instant::now();
        let r = match whisper.transcribe(&u.speech.audio, "fr", 160) {
            Ok(r) => r,
            Err(e) => {
                hub.log(Level::Warning, Source::Listen, id, format!("Transcription impossible : {e:#}"));
                continue;
            }
        };
        tracing::debug!("transcription en {} ms : {:?}", t0.elapsed().as_millis(), r);
        if r.is_doubtful() {
            continue;
        }
        let help = calls_for_help(&r.text);
        let (level, msg) = if help { (Level::Critical, format!("Appel à l'aide entendu : « {} »", r.text)) } else { (Level::Info, format!("Parole : « {} »", r.text)) };
        hub.log(level, Source::Listen, id, msg);
        if help {
            hub.scene_trigger(id, Trigger::HelpCall);
        }
        let shown = if help { format!("Appel à l'aide : « {} »", r.text) } else { format!("« {} »", r.text) };
        hub.set_listening(id, |l| {
            l.heard.insert(0, (Instant::now(), shown));
            l.heard.truncate(6);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::{self, CameraConfig};

    /// Écoute activée sur la caméra simulée (son : un la continu en G.711) : la piste son est
    /// demandée, le niveau arrive (≈ −13 dBFS), et tout est journalisé « Écoute IA ».
    /// Sautée si les modèles des sons ne sont pas installés (`homesmart-cli listen models --install`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn listening_hears_the_simulated_microphone() {
        if models::SOUNDS.iter().any(|m| !m.is_installed()) {
            eprintln!("modèles des sons absents : test sauté");
            return;
        }
        let dir = std::env::temp_dir().join(format!("homesmart-listentest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sim = camera::sim::start("CODE1234", 0).await.unwrap();
        let hub = Hub::new(dir.join("homesmart.json")).unwrap();
        hub.start(false);
        let mut cfg = CameraConfig::new("127.0.0.1");
        (cfg.rtsp_port, cfg.onvif_port, cfg.password) = (sim.rtsp_port, sim.onvif_port, "CODE1234".into());
        let v = hub.add_camera(cfg, Source::User).unwrap();
        let on = CameraVision { enabled: true, fps: 2.0, hd: false, faces: false, animals: false, sounds: true, ..CameraVision::default() };
        hub.set_camera_vision(&v.id, on.clone(), Source::User).unwrap();
        let t0 = Instant::now();
        loop {
            let l = hub.camera_listening(&v.id).unwrap_or_default();
            assert!(l.error.is_none() || l.running, "{:?}", l.error);
            if let Some(db) = l.level {
                assert!((db + 13.5).abs() < 3.0, "niveau {db} dBFS");
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(30), "aucun son reçu : {l:?}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        hub.set_camera_vision(&v.id, CameraVision { enabled: false, ..on }, Source::User).unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let log = hub.alerts(80);
        let has = |src: Source, txt: &str| log.iter().any(|a| a.source == src && a.message.contains(txt));
        assert!(has(Source::User, "écoute des sons activée"), "{log:#?}");
        assert!(has(Source::Listen, "Écoute IA démarrée"), "{log:#?}");
        assert!(has(Source::Listen, "Écoute IA arrêtée"), "{log:#?}");
        assert!(!log.iter().any(|a| a.message.contains("Son du direct activé")), "l'écoute par IA n'est pas l'utilisateur");
        hub.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn help_words() {
        assert!(calls_for_help("Au secours !"));
        assert!(calls_for_help("A l’aide, je suis tombé"));
        assert!(calls_for_help("aidez moi s'il vous plaît"));
        assert!(!calls_for_help("Je vais chercher de l'aide-mémoire"));
        assert!(!calls_for_help("On secoue le tapis"));
    }
}
