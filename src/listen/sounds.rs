//! Sons de la maison : CED (classifieur AudioSet, 527 sortes de sons) et ce qu'on en retient.
//!
//! Modèle : CED-mini (export ONNX de sherpa-onnx, quantifié int8). Entrée `feats` [1, 64, T] :
//! spectrogramme mel en dB (16 kHz, fenêtre de Hann 512, pas 160, centré en miroir, 64 bandes
//! HTK 0–8 kHz sans normalisation, 10·log10, plancher à max − 120 dB) ; sortie `prob` [1, 527],
//! probabilités indépendantes (sigmoïde) des classes AudioSet.

use super::dsp::{Mel, MelConfig, MelScale};
use crate::vision::engine::{self, Accel};
use anyhow::{anyhow, bail, Context, Result};
use ort::session::Session;
use ort::value::TensorRef;
use std::collections::HashMap;
use std::path::Path;

pub const RATE: u32 = 16_000;

/// Recette du spectrogramme de CED.
pub fn ced_mel() -> MelConfig {
    MelConfig { rate: RATE, n_fft: 512, win: 512, hop: 160, n_mels: 64, fmin: 0.0, fmax: 8_000.0, scale: MelScale::Htk, area_norm: false, mel_triangles: false, power: true }
}

/// Classifieur de sons AudioSet.
pub struct SoundTagger {
    session: Session,
    mel: Mel,
    /// Noms des classes (anglais, ontologie AudioSet).
    pub labels: Vec<String>,
    pub backend: &'static str,
}

impl SoundTagger {
    /// Toujours sur le processeur : ≈ 5 ms par fenêtre, et ce modèle quantifié (int8) fait
    /// planter DirectML (violation d'accès dans le pilote, constatée sur une RTX 5070 Ti).
    pub fn load(model: &Path, labels: &Path) -> Result<SoundTagger> {
        let l = engine::load(model, Accel::Cpu, &[("batch_size", 1)])?;
        let labels = parse_labels(&std::fs::read_to_string(labels).with_context(|| format!("lecture de {}", labels.display()))?)?;
        Ok(SoundTagger { session: l.session, mel: Mel::new(ced_mel()), labels, backend: l.backend })
    }

    /// Probabilités des 527 classes pour un extrait 16 kHz (≥ 0,2 s).
    pub fn scores(&mut self, audio: &[f32]) -> Result<Vec<f32>> {
        let (frames, m) = self.mel.compute(audio, true);
        if frames < 16 {
            bail!("extrait trop court");
        }
        let n = self.mel.cfg.n_mels;
        let mut feats = vec![0.0f32; n * frames];
        let mut max = f32::MIN;
        for f in 0..frames {
            for b in 0..n {
                let v = 10.0 * m[f * n + b].max(1e-10).log10();
                feats[b * frames + f] = v;
                max = max.max(v);
            }
        }
        for v in &mut feats {
            *v = v.max(max - 120.0);
        }
        let out = self.session.run(ort::inputs!["feats" => TensorRef::from_array_view(([1usize, n, frames], &feats[..]))?])?;
        let (_, p) = out["prob"].try_extract_tensor::<f32>()?;
        if p.len() != self.labels.len() {
            bail!("CED : {} sorties pour {} étiquettes", p.len(), self.labels.len());
        }
        Ok(p.to_vec())
    }
}

/// `class_labels_indices.csv` : index,mid,display_name (nom entre guillemets s'il contient une virgule).
pub fn parse_labels(csv: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for line in csv.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let mut parts = line.splitn(3, ',');
        let idx: usize = parts.next().unwrap_or("").trim().parse().map_err(|_| anyhow!("étiquettes : ligne invalide « {line} »"))?;
        let _mid = parts.next();
        let name = parts.next().unwrap_or("").trim().trim_matches('"').to_string();
        if idx != out.len() {
            bail!("étiquettes : index {idx} inattendu");
        }
        out.push(name);
    }
    Ok(out)
}

/// Gravité d'un son pour le journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gravity {
    Info,
    Warning,
    Critical,
}

/// Sons de la maison qu'on signale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HomeSound {
    SmokeAlarm,
    Alarm,
    GlassBreak,
    Scream,
    BabyCry,
    Crying,
    Gunshot,
    DogBark,
    Cat,
    Doorbell,
    Knock,
    DoorSlam,
    Phone,
    Cough,
    Snore,
    Footsteps,
    Siren,
}

impl HomeSound {
    pub const ALL: [HomeSound; 17] = [
        HomeSound::SmokeAlarm,
        HomeSound::Alarm,
        HomeSound::GlassBreak,
        HomeSound::Scream,
        HomeSound::BabyCry,
        HomeSound::Crying,
        HomeSound::Gunshot,
        HomeSound::DogBark,
        HomeSound::Cat,
        HomeSound::Doorbell,
        HomeSound::Knock,
        HomeSound::DoorSlam,
        HomeSound::Phone,
        HomeSound::Cough,
        HomeSound::Snore,
        HomeSound::Footsteps,
        HomeSound::Siren,
    ];

    /// Classes AudioSet regroupées sous ce son.
    pub fn audioset(self) -> &'static [&'static str] {
        match self {
            HomeSound::SmokeAlarm => &["Smoke detector, smoke alarm", "Fire alarm"],
            HomeSound::Alarm => &["Alarm", "Alarm clock", "Buzzer", "Beep, bleep"],
            // « Glass » seul répond aussi aux verres qui tintent et aux gouttes : on ne le garde pas.
            HomeSound::GlassBreak => &["Breaking", "Shatter", "Smash, crash"],
            HomeSound::Scream => &["Screaming", "Shout", "Yell", "Children shouting"],
            HomeSound::BabyCry => &["Baby cry, infant cry"],
            HomeSound::Crying => &["Crying, sobbing", "Whimper"],
            HomeSound::Gunshot => &["Gunshot, gunfire", "Explosion", "Machine gun", "Fusillade"],
            HomeSound::DogBark => &["Bark", "Dog", "Howl", "Bow-wow", "Yip", "Growling"],
            HomeSound::Cat => &["Meow", "Cat", "Caterwaul", "Hiss"],
            HomeSound::Doorbell => &["Doorbell", "Ding-dong"],
            HomeSound::Knock => &["Knock"],
            HomeSound::DoorSlam => &["Slam", "Door"],
            HomeSound::Phone => &["Telephone bell ringing", "Ringtone", "Telephone"],
            HomeSound::Cough => &["Cough"],
            HomeSound::Snore => &["Snoring"],
            HomeSound::Footsteps => &["Walk, footsteps"],
            HomeSound::Siren => &["Siren", "Civil defense siren", "Police car (siren)", "Ambulance (siren)", "Fire engine, fire truck (siren)"],
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            HomeSound::SmokeAlarm => "alarme incendie (détecteur de fumée)",
            HomeSound::Alarm => "alarme ou sonnerie",
            HomeSound::GlassBreak => "bris de verre",
            HomeSound::Scream => "cri",
            HomeSound::BabyCry => "pleurs de bébé",
            HomeSound::Crying => "pleurs",
            HomeSound::Gunshot => "détonation",
            HomeSound::DogBark => "aboiements",
            HomeSound::Cat => "miaulements",
            HomeSound::Doorbell => "sonnette",
            HomeSound::Knock => "on frappe",
            HomeSound::DoorSlam => "porte qui claque",
            HomeSound::Phone => "téléphone qui sonne",
            HomeSound::Cough => "toux",
            HomeSound::Snore => "ronflements",
            HomeSound::Footsteps => "bruits de pas",
            HomeSound::Siren => "sirène",
        }
    }

    pub fn gravity(self) -> Gravity {
        match self {
            HomeSound::SmokeAlarm | HomeSound::GlassBreak | HomeSound::Gunshot => Gravity::Critical,
            HomeSound::Scream | HomeSound::BabyCry | HomeSound::Crying | HomeSound::Alarm => Gravity::Warning,
            _ => Gravity::Info,
        }
    }

    /// Seuil de probabilité. CED moyenne sur toute la fenêtre : un son bref y pèse peu, d'où
    /// des seuils bas. Réglés sur des enregistrements réels (ESC-50 + 289 phrases de FLEURS
    /// comme sons « neutres », `homesmart-cli listen eval sounds`) : au plus ≈ 1 % d'extraits
    /// neutres signalés, avec une marge.
    pub fn threshold(self) -> f32 {
        match self {
            HomeSound::SmokeAlarm => 0.3,
            HomeSound::Alarm => 0.15,
            // 0,1 trouvait un extrait de plus sur ESC-50, mais déclenchait deux fausses alertes (0,11 et 0,13)
            // en un quart d'heure d'enregistrements d'une vraie chambre : 0,2.
            HomeSound::GlassBreak => 0.2,
            HomeSound::Scream => 0.3,
            HomeSound::BabyCry => 0.15,
            HomeSound::Crying => 0.25,
            HomeSound::Gunshot => 0.4,
            HomeSound::DogBark => 0.2,
            HomeSound::Cat => 0.2,
            HomeSound::Doorbell => 0.3,
            HomeSound::Knock => 0.2,
            HomeSound::DoorSlam => 0.4,
            HomeSound::Phone => 0.3,
            HomeSound::Cough => 0.15,
            HomeSound::Snore => 0.1,
            HomeSound::Footsteps => 0.15,
            HomeSound::Siren => 0.15,
        }
    }

    /// Fenêtres consécutives au-dessus du seuil avant de signaler (1 : son bref).
    pub fn windows(self) -> u32 {
        match self {
            HomeSound::GlassBreak | HomeSound::Gunshot | HomeSound::Knock | HomeSound::DoorSlam | HomeSound::Doorbell | HomeSound::Scream | HomeSound::Cough => 1,
            _ => 2,
        }
    }
}

/// Rires (AudioSet). Pas un son qu'on signale : entendus autour de coups vus à l'image, ils font
/// d'une « bagarre » une chamaillerie (voir `vision::violence`).
pub const LAUGHTER: [&str; 6] = ["Laughter", "Giggle", "Belly laugh", "Chuckle, chortle", "Snicker", "Baby laughter"];

/// Probabilité à partir de laquelle on tient des rires pour entendus (fenêtres de 3 s). Mesuré avec
/// `homesmart-cli vision violence-eval --sounds` : rires entendus dans 1 % des bagarres qui ont du
/// son, mais dans un quart des luttes, la moitié des batailles d'oreillers et les trois quarts des
/// chatouilles de Kinetics-700.
pub const LAUGHTER_MIN: f32 = 0.1;

/// Indices AudioSet de chaque son de la maison, résolus par nom.
pub struct SoundMap {
    pub classes: Vec<(HomeSound, Vec<usize>)>,
    /// Indices des rires.
    pub laughter: Vec<usize>,
}

impl SoundMap {
    pub fn new(labels: &[String]) -> SoundMap {
        let by_name: HashMap<&str, usize> = labels.iter().enumerate().map(|(i, l)| (l.as_str(), i)).collect();
        let classes = HomeSound::ALL.iter().map(|&h| (h, h.audioset().iter().filter_map(|n| by_name.get(n).copied()).collect())).collect();
        let laughter = LAUGHTER.iter().filter_map(|n| by_name.get(n).copied()).collect();
        SoundMap { classes, laughter }
    }

    /// Probabilité des rires (max de leurs classes).
    pub fn laughter(&self, probs: &[f32]) -> f32 {
        self.laughter.iter().map(|&i| probs.get(i).copied().unwrap_or(0.0)).fold(0.0, f32::max)
    }

    /// Score de chaque son de la maison (max de ses classes).
    pub fn scores(&self, probs: &[f32]) -> Vec<(HomeSound, f32)> {
        self.classes.iter().map(|(h, idx)| (*h, idx.iter().map(|&i| probs.get(i).copied().unwrap_or(0.0)).fold(0.0, f32::max))).collect()
    }
}

/// Un son reconnu.
#[derive(Clone, Debug, PartialEq)]
pub struct SoundEvent {
    pub sound: HomeSound,
    pub score: f32,
    /// Secondes depuis le début de l'écoute.
    pub at: f64,
}

/// Fenêtres glissantes, lissage et pause entre deux signalements d'un même son.
pub struct SoundDetector {
    pub map: SoundMap,
    buf: Vec<f32>,
    /// Durée de la fenêtre et pas (échantillons).
    pub win: usize,
    pub hop: usize,
    now: f64,
    streak: HashMap<HomeSound, u32>,
    last: HashMap<HomeSound, f64>,
    pub cooldown: f64,
    /// Sons à signaler (les autres sont calculés mais tus).
    pub enabled: Vec<HomeSound>,
    /// Fin de la dernière fenêtre où des rires ont été entendus (secondes depuis le début).
    pub laughed: Option<f64>,
}

impl SoundDetector {
    pub fn new(labels: &[String]) -> SoundDetector {
        SoundDetector {
            map: SoundMap::new(labels),
            buf: Vec::new(),
            // 3 s : CED a besoin de contexte (1 s : bien moins juste), et l'alerte reste rapide.
            win: 3 * RATE as usize,
            hop: RATE as usize / 2,
            now: 0.0,
            streak: HashMap::new(),
            last: HashMap::new(),
            cooldown: 30.0,
            enabled: HomeSound::ALL.to_vec(),
            laughed: None,
        }
    }

    /// Ajoute du son 16 kHz ; `classify` donne les probabilités AudioSet d'une fenêtre.
    pub fn push(&mut self, audio: &[f32], classify: &mut dyn FnMut(&[f32]) -> Result<Vec<f32>>) -> Result<Vec<SoundEvent>> {
        self.buf.extend_from_slice(audio);
        let mut out = Vec::new();
        while self.buf.len() >= self.win {
            let probs = classify(&self.buf[..self.win])?;
            let t = self.now + self.win as f64 / RATE as f64;
            if self.map.laughter(&probs) >= LAUGHTER_MIN {
                self.laughed = Some(t);
            }
            for (h, s) in self.map.scores(&probs) {
                let above = s >= h.threshold();
                let n = self.streak.entry(h).or_insert(0);
                *n = if above { *n + 1 } else { 0 };
                if above && *n >= h.windows() && self.enabled.contains(&h) && self.last.get(&h).is_none_or(|&l| t - l >= self.cooldown) {
                    self.last.insert(h, t);
                    out.push(SoundEvent { sound: h, score: s, at: t });
                }
            }
            self.buf.drain(..self.hop);
            self.now += self.hop as f64 / RATE as f64;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels() -> Vec<String> {
        parse_labels("index,mid,display_name\n0,/m/09x0r,\"Speech\"\n1,/m/05tny_,\"Bark\"\n2,/m/07rkbfh,\"Dog\"\n3,/m/01b_21,\"Smoke detector, smoke alarm\"\n").unwrap()
    }

    #[test]
    fn labels_and_mapping() {
        let l = labels();
        assert_eq!(l, ["Speech", "Bark", "Dog", "Smoke detector, smoke alarm"]);
        let m = SoundMap::new(&l);
        let s: HashMap<HomeSound, f32> = m.scores(&[0.9, 0.2, 0.7, 0.1]).into_iter().collect();
        assert_eq!(s[&HomeSound::DogBark], 0.7);
        assert_eq!(s[&HomeSound::SmokeAlarm], 0.1);
        assert_eq!(s[&HomeSound::Cat], 0.0);
    }

    #[test]
    fn laughter_is_noted_not_reported() {
        let l = parse_labels("index,mid,display_name\n0,/m/09x0r,\"Speech\"\n1,/m/01j3sz,\"Laughter\"\n2,/m/07sq110,\"Belly laugh\"\n").unwrap();
        let mut d = SoundDetector::new(&l);
        assert_eq!(d.map.laughter(&[0.9, 0.05, 0.3]), 0.3);
        // Parole seule, puis des rires : notés (fin de la fenêtre), jamais signalés.
        let mut n = 0;
        let mut laughs = |_: &[f32]| -> Result<Vec<f32>> {
            n += 1;
            Ok(if n <= 2 { vec![0.9, 0.0, 0.0] } else { vec![0.5, 0.4, 0.0] })
        };
        assert!(d.push(&vec![0.0; 3 * RATE as usize + RATE as usize / 2], &mut laughs).unwrap().is_empty());
        assert_eq!(d.laughed, None);
        assert!(d.push(&vec![0.0; RATE as usize / 2], &mut laughs).unwrap().is_empty());
        assert_eq!(d.laughed, Some(4.0));
    }

    #[test]
    fn smoothing_and_cooldown() {
        let mut d = SoundDetector::new(&labels());
        // Aboiements (2 fenêtres de 3 s requises) pendant 4 s, puis encore 3 s (pause de 30 s).
        let mut barking = |_: &[f32]| -> Result<Vec<f32>> { Ok(vec![0.0, 0.8, 0.8, 0.0]) };
        let ev = d.push(&vec![0.0; 4 * RATE as usize], &mut barking).unwrap();
        assert_eq!(ev.len(), 1, "{ev:?}");
        assert_eq!(ev[0].sound, HomeSound::DogBark);
        assert!((ev[0].at - 3.5).abs() < 1e-9, "deuxième fenêtre : {}", ev[0].at);
        assert!(d.push(&vec![0.0; 3 * RATE as usize], &mut barking).unwrap().is_empty());
        // Alarme incendie, une seule fenêtre : pas encore (2 requises).
        let mut d = SoundDetector::new(&labels());
        let mut once = {
            let mut n = 0;
            move |_: &[f32]| -> Result<Vec<f32>> {
                n += 1;
                Ok(if n == 1 { vec![0.0, 0.0, 0.0, 0.9] } else { vec![0.0; 4] })
            }
        };
        assert!(d.push(&vec![0.0; 3 * RATE as usize], &mut once).unwrap().is_empty());
    }
}
