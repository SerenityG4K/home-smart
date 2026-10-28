//! `homesmart-cli listen …` : écoute par IA (sons de la maison, parole → texte) sur des
//! fichiers WAV, installation des modèles et mesures sur des enregistrements réels annotés.

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use homesmart::listen::dsp::Resampler;
use homesmart::listen::sounds::{HomeSound, SoundDetector, SoundTagger};
use homesmart::listen::vad::{SegmentRules, Segmenter, SileroVad, CHUNK};
use homesmart::listen::whisper::Whisper;
use homesmart::listen::wav;
use homesmart::vision::engine::Accel;
use homesmart::vision::models::{self, ModelSpec};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Subcommand)]
pub enum ListenCmd {
    /// Modèles de l'écoute : état ; avec --install, téléchargement vérifié
    Models {
        #[arg(long)]
        install: bool,
    },
    /// Sons reconnus dans un fichier WAV
    Tag {
        file: PathBuf,
        /// Classes AudioSet affichées par fenêtre
        #[arg(long, default_value_t = 3)]
        top: usize,
    },
    /// Parole d'un fichier WAV → texte (voix détectée, puis Whisper)
    Transcribe {
        file: PathBuf,
        #[arg(long, default_value = "fr")]
        lang: String,
        #[arg(long)]
        cpu: bool,
        /// Tout le fichier d'un bloc (≤ 30 s), sans détection de la voix
        #[arg(long)]
        whole: bool,
    },
    /// Mesures sur des enregistrements réels annotés
    Eval {
        #[command(subcommand)]
        what: ListenEval,
    },
}

#[derive(Subcommand)]
pub enum ListenEval {
    /// Sons de la maison sur ESC-50 (dossier audio/esc50 du jeu d'évaluation)
    Sounds {
        #[arg(long)]
        data: PathBuf,
        /// Simule un micro de caméra en téléphonie (8 kHz, G.711 µ-law)
        #[arg(long)]
        phone: bool,
        /// Durée d'une fenêtre d'analyse (s)
        #[arg(long)]
        window: Option<f32>,
        /// Sons à trouver mêlés à de la parole (FLEURS), à ce rapport en dB (ex. 0 : aussi fort)
        #[arg(long)]
        mix: Option<f32>,
    },
    /// Appels à l'aide (dossier audio/help : NN.wav + NN.txt) : alerte quand il faut, et seulement alors
    Alerts {
        #[arg(long)]
        data: PathBuf,
        #[arg(long)]
        cpu: bool,
        #[arg(long)]
        snr: Option<f32>,
    },
    /// Bruits sans parole (ESC-50) : phrases « fantômes » qui iraient au journal
    Phantom {
        #[arg(long)]
        data: PathBuf,
        #[arg(long)]
        cpu: bool,
    },
    /// Parole française sur FLEURS (dossier audio/fleurs) : taux d'erreur sur les mots
    Speech {
        #[arg(long)]
        data: PathBuf,
        #[arg(long)]
        cpu: bool,
        #[arg(long, default_value_t = 40)]
        limit: usize,
        #[arg(long)]
        phone: bool,
        /// Bruit de fond ajouté (rapport signal / bruit en dB, ex. 10)
        #[arg(long)]
        snr: Option<f32>,
        /// Nature du bruit : « maison » (pluie, aspirateur, lave-linge… d'ESC-50) ou « blanc »
        #[arg(long, default_value = "maison")]
        noise: String,
        /// Phrases mal reconnues affichées
        #[arg(long)]
        verbose: bool,
    },
}

pub fn run(cmd: ListenCmd) -> Result<()> {
    match cmd {
        ListenCmd::Models { install } => list_models(install),
        ListenCmd::Tag { file, top } => tag(&file, top),
        ListenCmd::Transcribe { file, lang, cpu, whole } => transcribe(&file, &lang, accel(cpu), whole),
        ListenCmd::Eval { what: ListenEval::Sounds { data, phone, window, mix } } => eval_sounds(&data, phone, window, mix),
        ListenCmd::Eval { what: ListenEval::Phantom { data, cpu } } => eval_phantom(&data, accel(cpu)),
        ListenCmd::Eval { what: ListenEval::Alerts { data, cpu, snr } } => eval_help(&data, accel(cpu), snr),
        ListenCmd::Eval { what: ListenEval::Speech { data, cpu, limit, phone, snr, noise, verbose } } => eval_speech(&data, accel(cpu), limit, phone, snr, &noise, verbose),
    }
}

fn accel(cpu: bool) -> Accel {
    if cpu {
        Accel::Cpu
    } else {
        Accel::Auto
    }
}

fn list_models(install: bool) -> Result<()> {
    println!("Modèles de l'écoute ({}) :", models::models_dir().display());
    let all: Vec<ModelSpec> = models::SOUNDS.iter().chain(models::SPEECH.iter()).copied().collect();
    for m in &all {
        let state = if m.is_installed() { "installé" } else { "absent" };
        println!("  {:<28} {:<32} {:>6.1} Mo  {:<10} {state}", m.id, m.role, m.size as f64 / 1e6, m.license);
    }
    if install {
        for m in all.iter().filter(|m| !m.is_installed()) {
            println!("Téléchargement de {} depuis {}", m.id, m.url);
            models::ensure(m, &|_, _| {})?;
            println!("  vérifié (SHA-256) et installé");
        }
    }
    Ok(())
}

/// Lit un WAV et le ramène en 16 kHz mono.
fn load16k(path: &Path) -> Result<Vec<f32>> {
    let (x, rate) = wav::read(path)?;
    Ok(resample(&x, rate, 16_000))
}

fn resample(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    let mut r = Resampler::new(from, to);
    let mut out = Vec::with_capacity(x.len() * to as usize / from.max(1) as usize + 64);
    r.process(x, &mut out);
    // Fin du signal : on pousse un peu de silence pour vider le filtre.
    r.process(&vec![0.0; from as usize / 50], &mut out);
    out.truncate(x.len() * to as usize / from.max(1) as usize);
    out
}

/// Micro « téléphonique » : 8 kHz, G.711 µ-law, puis retour en 16 kHz.
fn phone(x16: &[f32]) -> Vec<f32> {
    let low = resample(x16, 16_000, 8_000);
    let coded: Vec<f32> = low.iter().map(|&s| homesmart::camera::audio::ulaw(homesmart::camera::sim::ulaw_encode((s.clamp(-1.0, 1.0) * 32767.0) as i16)) as f32 / 32768.0).collect();
    resample(&coded, 8_000, 16_000)
}

fn tagger() -> Result<SoundTagger> {
    for m in models::SOUNDS {
        if !m.is_installed() {
            bail!("modèle « {} » absent : homesmart-cli listen models --install", m.id);
        }
    }
    SoundTagger::load(&models::CED_MINI.path(), &models::CED_LABELS.path())
}

fn tag(file: &Path, top: usize) -> Result<()> {
    let x = load16k(file)?;
    let mut t = tagger()?;
    println!("{} : {:.1} s — {}", file.display(), x.len() as f32 / 16_000.0, t.backend);
    let mut det = SoundDetector::new(&t.labels.clone());
    let mut first = true;
    let labels = t.labels.clone();
    let t0 = Instant::now();
    let mut windows = 0;
    let events = det.push(&x, &mut |w: &[f32]| {
        let p = t.scores(w)?;
        windows += 1;
        let mut idx: Vec<usize> = (0..p.len()).collect();
        idx.sort_by(|&a, &b| p[b].total_cmp(&p[a]));
        let best: Vec<String> = idx.iter().take(top).map(|&i| format!("{} {:.2}", labels[i], p[i])).collect();
        if first || windows % 2 == 1 {
            println!("  fenêtre {windows:>3} : {}", best.join(" · "));
        }
        first = false;
        Ok(p)
    })?;
    println!("  {windows} fenêtre(s) en {:.0} ms ({:.1} ms par fenêtre de 2 s)", t0.elapsed().as_secs_f64() * 1000.0, t0.elapsed().as_secs_f64() * 1000.0 / windows.max(1) as f64);
    for e in events {
        println!("  → {} à {:.1} s ({:.2})", e.sound.label(), e.at, e.score);
    }
    Ok(())
}

fn whisper(accel: Accel) -> Result<Whisper> {
    for m in models::SPEECH {
        if !m.is_installed() {
            bail!("modèle « {} » absent : homesmart-cli listen models --install", m.id);
        }
    }
    Whisper::load(&models::WHISPER_ENCODER.path(), &models::WHISPER_DECODER.path(), &models::WHISPER_TOKENIZER.path(), accel)
}

/// Phrases d'un son 16 kHz, découpées par la détection de la voix.
fn segments(vad: &mut SileroVad, x: &[f32]) -> Result<Vec<homesmart::listen::vad::Speech>> {
    vad.reset();
    let mut seg = Segmenter::new(SegmentRules::default());
    let mut out = Vec::new();
    let mut padded = x.to_vec();
    // Silence final : la dernière phrase se termine.
    padded.extend(vec![0.0; 16_000]);
    let rem = padded.len() % CHUNK;
    if rem != 0 {
        padded.extend(vec![0.0; CHUNK - rem]);
    }
    for c in padded.chunks(CHUNK) {
        let p = vad.prob(c)?;
        out.extend(seg.push(c, p));
    }
    Ok(out)
}

fn transcribe(file: &Path, lang: &str, accel: Accel, whole: bool) -> Result<()> {
    let x = load16k(file)?;
    let t0 = Instant::now();
    let mut w = whisper(accel)?;
    println!("{} : {:.1} s — Whisper chargé en {} ms ({})", file.display(), x.len() as f32 / 16_000.0, t0.elapsed().as_millis(), w.backend);
    let parts: Vec<(f64, f64, Vec<f32>)> = if whole {
        vec![(0.0, x.len() as f64 / 16_000.0, x.clone())]
    } else {
        let mut vad = SileroVad::load(&models::SILERO_VAD.path())?;
        let t1 = Instant::now();
        let s = segments(&mut vad, &x)?;
        println!("  voix : {} phrase(s) trouvée(s) en {} ms", s.len(), t1.elapsed().as_millis());
        s.into_iter().map(|s| (s.start, s.end, s.audio)).collect()
    };
    for (a, b, audio) in parts {
        let t = Instant::now();
        let r = w.transcribe(&audio, lang, 160)?;
        let flag = if r.is_doubtful() { " [écarté]" } else { "" };
        println!("  [{a:5.1} → {b:5.1} s] {}{flag}  ({} ms, confiance {:.2}, pas de parole {:.2})", r.text, t.elapsed().as_millis(), r.avg_logprob, r.no_speech);
    }
    Ok(())
}

// ---- Évaluations ------------------------------------------------------------------------------

/// Son attendu pour chaque catégorie d'ESC-50 (None : son « neutre », rien ne doit sortir).
fn esc_expected(category: &str) -> Option<HomeSound> {
    Some(match category {
        "dog" => HomeSound::DogBark,
        "cat" => HomeSound::Cat,
        "crying_baby" => HomeSound::BabyCry,
        "glass_breaking" => HomeSound::GlassBreak,
        "door_wood_knock" => HomeSound::Knock,
        "clock_alarm" => HomeSound::Alarm,
        "siren" => HomeSound::Siren,
        "coughing" => HomeSound::Cough,
        "footsteps" => HomeSound::Footsteps,
        "snoring" => HomeSound::Snore,
        _ => return None,
    })
}

fn eval_sounds(data: &Path, phone_mic: bool, window: Option<f32>, mix: Option<f32>) -> Result<()> {
    let root = data.join("audio").join("esc50").join("audio");
    let mut clips: Vec<(String, PathBuf)> = Vec::new();
    for cat in std::fs::read_dir(&root).with_context(|| format!("{}", root.display()))? {
        let cat = cat?.path();
        let name = cat.file_name().unwrap_or_default().to_string_lossy().to_string();
        for f in std::fs::read_dir(&cat)? {
            clips.push((name.clone(), f?.path()));
        }
    }
    // Parole ordinaire (FLEURS) : la caméra entend surtout des gens parler, rien ne doit sortir.
    let fleurs = data.join("audio").join("fleurs").join("wav");
    if let Ok(rd) = std::fs::read_dir(&fleurs) {
        let mut speech: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        speech.sort();
        clips.extend(speech.into_iter().map(|p| ("parole".to_string(), p)));
    }
    clips.sort();
    // Parole de fond pour les sons à trouver (quelqu'un parle pendant que le chien aboie).
    let speech_bg: Vec<Vec<f32>> = match mix {
        Some(_) => clips.iter().filter(|c| c.0 == "parole").take(40).map(|c| load16k(&c.1)).collect::<Result<_>>()?,
        None => Vec::new(),
    };
    let mut t = tagger()?;
    println!(
        "ESC-50 + parole : {} extraits{}{} — {}",
        clips.len(),
        if phone_mic { ", micro 8 kHz G.711 simulé" } else { "" },
        mix.map(|m| format!(", sons à trouver mêlés à de la parole ({m:+.0} dB)")).unwrap_or_default(),
        t.backend
    );
    let labels = t.labels.clone();
    let map = homesmart::listen::sounds::SoundMap::new(&labels);
    // Scores de chaque son de la maison, fenêtre par fenêtre (2 s, pas de 0,5 s), et décisions
    // du détecteur réel (seuils, fenêtres consécutives).
    struct Clip {
        cat: String,
        scores: HashMap<HomeSound, Vec<f32>>,
        events: Vec<HomeSound>,
    }
    let mut rows: Vec<Clip> = Vec::new();
    let t0 = Instant::now();
    let mut windows = 0;
    for (n, (cat, path)) in clips.iter().enumerate() {
        let mut x = load16k(path)?;
        if let (Some(db), Some(_)) = (mix, esc_expected(cat)) {
            let bg = &speech_bg[n % speech_bg.len().max(1)];
            let power = |v: &[f32]| v.iter().map(|s| s * s).sum::<f32>() / v.len().max(1) as f32;
            let k = (power(&x) / 10f32.powf(db / 10.0) / power(bg).max(1e-12)).sqrt();
            for (i, v) in x.iter_mut().enumerate() {
                *v += k * bg[i % bg.len()];
            }
        }
        if phone_mic {
            x = phone(&x);
        }
        let mut det = SoundDetector::new(&labels);
        if let Some(w) = window {
            det.win = (w * 16_000.0) as usize;
        }
        let mut scores: HashMap<HomeSound, Vec<f32>> = HashMap::new();
        let events = det.push(&x, &mut |w: &[f32]| {
            let p = t.scores(w)?;
            windows += 1;
            for (h, s) in map.scores(&p) {
                scores.entry(h).or_default().push(s);
            }
            Ok(p)
        })?;
        rows.push(Clip { cat: cat.clone(), scores, events: events.into_iter().map(|e| e.sound).collect() });
    }
    println!("  {windows} fenêtres en {:.1} s ({:.1} ms par fenêtre)", t0.elapsed().as_secs_f32(), t0.elapsed().as_secs_f64() * 1000.0 / windows.max(1) as f64);
    // Score d'un extrait : meilleure fenêtre (1) ou meilleure paire de fenêtres consécutives (2).
    let score = |c: &Clip, h: HomeSound, n: u32| -> f32 {
        let v = c.scores.get(&h).map(|v| v.as_slice()).unwrap_or(&[]);
        if n <= 1 {
            v.iter().copied().fold(0.0, f32::max)
        } else {
            v.windows(2).map(|w| w[0].min(w[1])).fold(0.0, f32::max)
        }
    };
    println!("  son                    réglage   trouvés  fausses alertes      seuil le plus bas à ≤ 1 % de faux (1 fenêtre · 2 fenêtres)");
    for h in HomeSound::ALL {
        let pos: Vec<&Clip> = rows.iter().filter(|r| esc_expected(&r.cat) == Some(h)).collect();
        let neg: Vec<&Clip> = rows.iter().filter(|r| esc_expected(&r.cat) != Some(h)).collect();
        let fa: Vec<String> = {
            let mut m: HashMap<&str, u32> = HashMap::new();
            for r in neg.iter().filter(|r| r.events.contains(&h)) {
                *m.entry(r.cat.as_str()).or_default() += 1;
            }
            let mut v: Vec<String> = m.into_iter().map(|(c, n)| format!("{c}×{n}")).collect();
            v.sort();
            v
        };
        let rate = |set: &[&Clip], th: f32, n: u32| set.iter().filter(|r| score(r, h, n) >= th).count() as f32 / set.len().max(1) as f32;
        let pick = |n: u32| -> String {
            if pos.is_empty() {
                return "—".into();
            }
            match (1..=60).map(|k| k as f32 * 0.01).find(|&th| rate(&neg, th, n) <= 0.01) {
                Some(th) => format!("{th:.2} → {:.0} %", rate(&pos, th, n) * 100.0),
                None => "aucun".into(),
            }
        };
        let found = if pos.is_empty() { "—".to_string() } else { format!("{}/{}", pos.iter().filter(|r| r.events.contains(&h)).count(), pos.len()) };
        println!(
            "  {:<22} {:.2}×{}    {found:<8} {:<20} {} · {}",
            h.label(),
            h.threshold(),
            h.windows(),
            if fa.is_empty() { "0".to_string() } else { fa.join(", ") },
            pick(1),
            pick(2)
        );
    }
    Ok(())
}

/// Bruits sans parole (ESC-50) passés dans la chaîne voix → texte : combien de phrases
/// « fantômes » iraient au journal ? (sons publics du jeu d'essai, pas ceux de la maison)
fn eval_phantom(data: &Path, accel: Accel) -> Result<()> {
    let root = data.join("audio").join("esc50").join("audio");
    let mut clips: Vec<(String, PathBuf)> = Vec::new();
    for cat in std::fs::read_dir(&root).with_context(|| format!("{}", root.display()))? {
        let cat = cat?.path();
        let name = cat.file_name().unwrap_or_default().to_string_lossy().to_string();
        for f in std::fs::read_dir(&cat)? {
            clips.push((name.clone(), f?.path()));
        }
    }
    clips.sort();
    let mut w = whisper(accel)?;
    let mut vad = SileroVad::load(&models::SILERO_VAD.path())?;
    println!("Bruits sans parole (ESC-50) : {} extraits — {}", clips.len(), w.backend);
    let (mut segs_n, mut kept, mut dropped) = (0, 0, 0);
    let mut by_cat: HashMap<String, u32> = HashMap::new();
    for (cat, path) in &clips {
        let x = load16k(path)?;
        for s in segments(&mut vad, &x)? {
            segs_n += 1;
            let r = w.transcribe(&s.audio, "fr", 160)?;
            if r.is_doubtful() {
                dropped += 1;
            } else {
                kept += 1;
                *by_cat.entry(cat.clone()).or_default() += 1;
                println!("  {cat} : « {} » (confiance {:.2}, pas de parole {:.2})", r.text, r.avg_logprob, r.no_speech);
            }
        }
    }
    let mut v: Vec<(String, u32)> = by_cat.into_iter().collect();
    v.sort();
    println!("  morceaux pris pour de la voix : {segs_n} · écartés : {dropped} · phrases fantômes au journal : {kept} {v:?}");
    Ok(())
}

/// Appels à l'aide (phrases de synthèse vocale, dossier audio/help : NN.wav + NN.txt) : le texte
/// entendu déclenche-t-il l'alerte quand il faut, et seulement alors ? Avec `snr`, bruits de la
/// maison (ESC-50) mêlés à ce rapport.
fn eval_help(data: &Path, accel: Accel, snr: Option<f32>) -> Result<()> {
    use homesmart::hub::calls_for_help;
    let root = data.join("audio").join("help");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&root).with_context(|| format!("{}", root.display()))?.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "wav")).collect();
    files.sort();
    let noises: Vec<Vec<f32>> = match snr {
        Some(_) => ["vacuum_cleaner", "washing_machine", "rain", "keyboard_typing"]
            .iter()
            .filter_map(|c| std::fs::read_dir(data.join("audio").join("esc50").join("audio").join(c)).ok()?.next()?.ok())
            .map(|e| load16k(&e.path()))
            .collect::<Result<_>>()?,
        None => Vec::new(),
    };
    let mut w = whisper(accel)?;
    let mut vad = SileroVad::load(&models::SILERO_VAD.path())?;
    println!("Appels à l'aide : {} phrases{}", files.len(), snr.map(|s| format!(", bruits de la maison à {s} dB")).unwrap_or_default());
    let (mut ok, mut total) = (0, 0);
    for (n, f) in files.iter().enumerate() {
        let reference = std::fs::read_to_string(f.with_extension("txt")).unwrap_or_default();
        let reference = reference.trim_start_matches('\u{feff}').trim();
        let mut x = load16k(f)?;
        // Un peu de silence autour, comme dans une pièce.
        let mut padded = vec![0.0f32; 8000];
        padded.append(&mut x);
        padded.extend(vec![0.0f32; 8000]);
        let mut x = padded;
        if let (Some(db), false) = (snr, noises.is_empty()) {
            let bg = &noises[n % noises.len()];
            let power = |v: &[f32]| v.iter().map(|s| s * s).sum::<f32>() / v.len().max(1) as f32;
            let k = (power(&x) / 10f32.powf(db / 10.0) / power(bg).max(1e-12)).sqrt();
            for (i, v) in x.iter_mut().enumerate() {
                *v += k * bg[i % bg.len()];
            }
        }
        let mut text = String::new();
        for s in segments(&mut vad, &x)? {
            let r = w.transcribe(&s.audio, "fr", 160)?;
            if !r.is_doubtful() {
                text.push_str(&r.text);
                text.push(' ');
            }
        }
        let (want, got) = (calls_for_help(reference), calls_for_help(&text));
        total += 1;
        ok += (want == got) as u32;
        println!("  {} « {} » → « {} »{}", if want == got { "ok " } else { "RATÉ" }, reference, text.trim(), if got { "  [ALERTE]" } else { "" });
    }
    println!("  {ok}/{total} bien classées");
    Ok(())
}

/// Texte normalisé pour comparer des transcriptions : minuscules, sans ponctuation.
fn words(s: &str) -> Vec<String> {
    let s = s.to_lowercase().replace(['’', '\''], " ").replace('-', " ");
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(String::from).collect()
}

/// Distance d'édition sur les mots.
fn word_errors(reference: &[String], hyp: &[String]) -> usize {
    let mut prev: Vec<usize> = (0..=hyp.len()).collect();
    for (i, r) in reference.iter().enumerate() {
        let mut cur = vec![i + 1; hyp.len() + 1];
        for (j, h) in hyp.iter().enumerate() {
            cur[j + 1] = (prev[j] + (r != h) as usize).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[hyp.len()]
}

fn eval_speech(data: &Path, accel: Accel, limit: usize, phone_mic: bool, snr: Option<f32>, noise: &str, verbose: bool) -> Result<()> {
    let root = data.join("audio").join("fleurs");
    let tsv = std::fs::read_to_string(root.join("dev.tsv")).context("dev.tsv (FLEURS)")?;
    let mut items: Vec<(String, String)> = tsv
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f.len() >= 4).then(|| (f[1].to_string(), f[3].to_string()))
        })
        .filter(|(file, _)| root.join("wav").join(file).exists())
        .collect();
    items.sort();
    items.dedup_by(|a, b| a.0 == b.0);
    items.truncate(limit);
    let mut w = whisper(accel)?;
    let mut vad = SileroVad::load(&models::SILERO_VAD.path())?;
    // Bruits de la maison (ESC-50), mis bout à bout et répétés sous chaque phrase.
    let mut house: Vec<Vec<f32>> = Vec::new();
    if noise != "blanc" {
        for cat in ["rain", "vacuum_cleaner", "washing_machine", "keyboard_typing", "water_drops", "toilet_flush", "clock_tick"] {
            let dir = data.join("audio").join("esc50").join("audio").join(cat);
            if let Some(f) = std::fs::read_dir(&dir).ok().and_then(|mut d| d.next()).and_then(|e| e.ok()) {
                house.push(load16k(&f.path())?);
            }
        }
    }
    println!(
        "FLEURS (français lu) : {} phrases{}{} — {}",
        items.len(),
        if phone_mic { ", micro 8 kHz G.711 simulé" } else { "" },
        snr.map(|s| format!(", bruit {} à {s} dB", if house.is_empty() { "blanc" } else { "de la maison" })).unwrap_or_default(),
        w.backend
    );
    let (mut errors, mut total, mut audio_s, mut busy) = (0usize, 0usize, 0f64, 0f64);
    let (mut missed_vad, mut dropped) = (0, 0);
    let mut seed = 0x2545F4914F6CDD1Du64;
    for (n, (file, reference)) in items.iter().enumerate() {
        let mut x = load16k(&root.join("wav").join(file))?;
        if let (Some(snr), false) = (snr, house.is_empty()) {
            let bg = &house[n % house.len()];
            let p_sig = x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32;
            let p_bg = bg.iter().map(|v| v * v).sum::<f32>() / bg.len().max(1) as f32;
            let k = (p_sig / 10f32.powf(snr / 10.0) / p_bg.max(1e-12)).sqrt();
            for (i, v) in x.iter_mut().enumerate() {
                *v += k * bg[i % bg.len()];
            }
        } else if let Some(snr) = snr {
            let p_sig = x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32;
            let amp = (p_sig / 10f32.powf(snr / 10.0)).sqrt() * 3f32.sqrt();
            for v in &mut x {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *v += amp * ((seed >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0);
            }
        }
        if phone_mic {
            x = phone(&x);
        }
        audio_s += x.len() as f64 / 16_000.0;
        let t = Instant::now();
        let segs = segments(&mut vad, &x)?;
        if segs.is_empty() {
            missed_vad += 1;
        }
        let mut text = String::new();
        for s in segs {
            let r = w.transcribe(&s.audio, "fr", 160)?;
            if r.is_doubtful() {
                dropped += 1;
                continue;
            }
            text.push_str(&r.text);
            text.push(' ');
        }
        busy += t.elapsed().as_secs_f64();
        let (rw, hw) = (words(reference), words(&text));
        let e = word_errors(&rw, &hw);
        errors += e;
        total += rw.len();
        if verbose && e * 4 > rw.len() {
            println!("  {file} : {e}/{} erreurs\n    attendu : {reference}\n    entendu : {}", rw.len(), text.trim());
        }
    }
    println!(
        "  taux d'erreur sur les mots : {:.1} % ({errors}/{total}) · {:.0} s de son en {:.0} s ({:.2}× le temps réel) · sans voix détectée : {missed_vad} · morceaux écartés : {dropped}",
        errors as f32 * 100.0 / total.max(1) as f32,
        audio_s,
        busy,
        busy / audio_s.max(1e-9)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_error_rate() {
        let r = words("Le chat, dort sur l'étagère.");
        assert_eq!(r, ["le", "chat", "dort", "sur", "l", "étagère"]);
        assert_eq!(word_errors(&r, &words("le chien dort sur l étagère")), 1);
        assert_eq!(word_errors(&r, &words("")), 6);
    }
}
