//! Parole → texte : Whisper (OpenAI, exporté en ONNX), en local.
//!
//! - Spectrogramme : 30 s complétées de silence, STFT 400 / pas 160 centrée (miroir), dernière
//!   trame retirée, 80 bandes mel Slaney normalisées, log10, plancher à max − 8, (x + 4) / 4.
//! - Encodeur : `input_features` [1, 80, 3000] → `last_hidden_state` [1, 1500, d].
//! - Décodeur « fusionné » avec cache : `input_ids`, `encoder_hidden_states`,
//!   `past_key_values.{i}.{decoder,encoder}.{key,value}`, `use_cache_branch` → `logits`,
//!   `present.*`. Premier passage sans cache (invite complète), puis un jeton à la fois.
//! - Décodage glouton, sans horodatage ; probabilité « pas de parole » lue au premier passage.

use super::dsp::{Mel, MelConfig};
use crate::vision::engine::{self, Accel};
use anyhow::{anyhow, bail, Context, Result};
use ort::session::{Session, SessionInputValue};
use ort::value::TensorRef;
use std::collections::HashMap;
use std::path::Path;

pub const RATE: u32 = 16_000;
/// Fenêtre de Whisper : 30 s.
pub const WINDOW: usize = 30 * RATE as usize;
const FRAMES: usize = 3000;

/// Jetons spéciaux (modèles multilingues tiny…medium).
#[derive(Clone, Copy, Debug)]
pub struct Special {
    pub eot: i64,
    pub sot: i64,
    pub transcribe: i64,
    pub no_speech: i64,
    pub no_timestamps: i64,
    /// Premier jeton spécial (tout ce qui suit est réservé : langues, tâches, horodatages).
    pub first_special: i64,
}

/// Tokeniseur de sortie : jeton → octets (BPE « byte-level » de GPT-2).
pub struct Tokenizer {
    pieces: Vec<Vec<u8>>,
    by_name: HashMap<String, i64>,
}

impl Tokenizer {
    /// Lit `tokenizer.json` (format Hugging Face).
    pub fn load(path: &Path) -> Result<Tokenizer> {
        let text = std::fs::read_to_string(path).with_context(|| format!("lecture de {}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(json: &str) -> Result<Tokenizer> {
        let v: serde_json::Value = serde_json::from_str(json).context("tokenizer.json illisible")?;
        let vocab = v["model"]["vocab"].as_object().ok_or_else(|| anyhow!("tokenizer.json : vocabulaire absent"))?;
        let unicode_to_byte: HashMap<char, u8> = bytes_to_unicode().into_iter().map(|(b, c)| (c, b)).collect();
        let mut pieces: Vec<Vec<u8>> = Vec::new();
        let mut by_name = HashMap::new();
        let mut put = |id: usize, bytes: Vec<u8>| {
            if pieces.len() <= id {
                pieces.resize(id + 1, Vec::new());
            }
            pieces[id] = bytes;
        };
        for (tok, id) in vocab {
            let id = id.as_u64().ok_or_else(|| anyhow!("identifiant de jeton invalide"))? as usize;
            let bytes: Vec<u8> = tok.chars().filter_map(|c| unicode_to_byte.get(&c).copied()).collect();
            put(id, bytes);
        }
        if let Some(added) = v["added_tokens"].as_array() {
            for t in added {
                let (Some(id), Some(content)) = (t["id"].as_u64(), t["content"].as_str()) else { continue };
                by_name.insert(content.to_string(), id as i64);
                // Les jetons spéciaux n'apparaissent pas dans le texte.
                put(id as usize, Vec::new());
            }
        }
        Ok(Tokenizer { pieces, by_name })
    }

    pub fn id(&self, name: &str) -> Result<i64> {
        self.by_name.get(name).copied().ok_or_else(|| anyhow!("jeton {name} absent du tokeniseur"))
    }

    pub fn special(&self) -> Result<Special> {
        Ok(Special {
            eot: self.id("<|endoftext|>")?,
            sot: self.id("<|startoftranscript|>")?,
            transcribe: self.id("<|transcribe|>")?,
            no_speech: self.id("<|nocaptions|>").or_else(|_| self.id("<|nospeech|>"))?,
            no_timestamps: self.id("<|notimestamps|>")?,
            first_special: self.id("<|endoftext|>")?,
        })
    }

    pub fn language(&self, code: &str) -> Result<i64> {
        self.id(&format!("<|{code}|>"))
    }

    pub fn decode(&self, ids: &[i64]) -> String {
        let bytes: Vec<u8> = ids.iter().filter_map(|&i| self.pieces.get(i as usize)).flatten().copied().collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    pub fn len(&self) -> usize {
        self.pieces.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }
}

/// Table de GPT-2 : octet → caractère imprimable.
fn bytes_to_unicode() -> Vec<(u8, char)> {
    let mut bs: Vec<u32> = (b'!' as u32..=b'~' as u32).chain(0xA1..=0xAC).chain(0xAE..=0xFF).collect();
    let mut cs = bs.clone();
    let mut n = 0;
    for b in 0..256u32 {
        if !bs.contains(&b) {
            bs.push(b);
            cs.push(256 + n);
            n += 1;
        }
    }
    bs.into_iter().zip(cs).map(|(b, c)| (b as u8, char::from_u32(c).unwrap())).collect()
}

/// Spectrogramme d'entrée de Whisper : `[n_mels × 3000]`, ligne par bande.
pub fn features(mel: &Mel, audio: &[f32]) -> Vec<f32> {
    let n_mels = mel.cfg.n_mels;
    let mut x = audio[..audio.len().min(WINDOW)].to_vec();
    x.resize(WINDOW, 0.0);
    let (frames, m) = mel.compute(&x, true);
    debug_assert!(frames >= FRAMES);
    let mut out = vec![0.0f32; n_mels * FRAMES];
    let mut max = f32::MIN;
    for f in 0..FRAMES {
        for b in 0..n_mels {
            let v = m[f * n_mels + b].max(1e-10).log10();
            out[b * FRAMES + f] = v;
            max = max.max(v);
        }
    }
    for v in &mut out {
        *v = (v.max(max - 8.0) + 4.0) / 4.0;
    }
    out
}

/// Un texte reconnu.
#[derive(Clone, Debug, PartialEq)]
pub struct Transcript {
    pub text: String,
    /// Log-probabilité moyenne des jetons (confiance : > −0,5 bon, < −1 douteux).
    pub avg_logprob: f32,
    /// Probabilité « pas de parole » (au début du décodage).
    pub no_speech: f32,
    pub tokens: usize,
    /// Le modèle tournait en boucle (texte coupé) : transcription douteuse.
    pub looped: bool,
}

impl Transcript {
    /// À écarter : silence ou bruit pris pour de la parole, phrase « fantôme », boucle.
    pub fn is_doubtful(&self) -> bool {
        is_hallucination(&self.text) || self.looped || (self.no_speech > 0.6 && self.avg_logprob < -1.0) || self.avg_logprob < -1.5
    }
}

/// Modèle Whisper chargé (encodeur + décodeur à cache).
pub struct Whisper {
    encoder: Session,
    decoder: Session,
    pub tokenizer: Tokenizer,
    special: Special,
    mel: Mel,
    layers: usize,
    heads: usize,
    pub backend: &'static str,
}

impl Whisper {
    pub fn load(encoder: &Path, decoder: &Path, tokenizer: &Path, accel: Accel) -> Result<Whisper> {
        let e = engine::load(encoder, accel, &[("batch_size", 1)])?;
        let d = engine::load(decoder, accel, &[("batch_size", 1)])?;
        let tokenizer = Tokenizer::load(tokenizer)?;
        let special = tokenizer.special()?;
        let layers = d.session.inputs().iter().filter(|i| i.name().starts_with("past_key_values.") && i.name().ends_with(".decoder.key")).count();
        if layers == 0 {
            bail!("décodeur Whisper sans cache (past_key_values) : utiliser decoder_model_merged");
        }
        let heads = d
            .session
            .inputs()
            .iter()
            .find(|i| i.name() == "past_key_values.0.decoder.key")
            .and_then(|i| match i.dtype() {
                ort::value::ValueType::Tensor { shape, .. } => shape.get(1).copied(),
                _ => None,
            })
            .filter(|&h| h > 0)
            .ok_or_else(|| anyhow!("décodeur Whisper : nombre de têtes inconnu"))? as usize;
        let n_mels = match e.session.inputs().first().map(|i| i.dtype()) {
            Some(ort::value::ValueType::Tensor { shape, .. }) if shape.get(1).is_some_and(|&m| m > 0) => shape[1] as usize,
            _ => 80,
        };
        Ok(Whisper { encoder: e.session, decoder: d.session, tokenizer, special, mel: Mel::new(MelConfig::whisper(n_mels)), layers, heads, backend: e.backend })
    }

    /// Transcrit au plus 30 s de son 16 kHz mono. `language` : code ISO (« fr »).
    pub fn transcribe(&mut self, audio: &[f32], language: &str, max_tokens: usize) -> Result<Transcript> {
        let n_mels = self.mel.cfg.n_mels;
        let feats = features(&self.mel, audio);
        let enc_out = self.encoder.run(ort::inputs!["input_features" => TensorRef::from_array_view(([1usize, n_mels, FRAMES], &feats[..]))?])?;
        let (eshape, enc) = enc_out["last_hidden_state"].try_extract_tensor::<f32>()?;
        let (t_enc, d_model) = (eshape[1] as usize, eshape[2] as usize);
        let enc = enc.to_vec();
        drop(enc_out);

        let s = self.special;
        let prompt = vec![s.sot, self.tokenizer.language(language)?, s.transcribe, s.no_timestamps];
        let head_dim = d_model / self.heads;
        // Cache : (clé, valeur) du décodeur et de l'encodeur, par couche.
        let mut past: Vec<[Vec<f32>; 4]> = (0..self.layers).map(|_| [Vec::new(), Vec::new(), Vec::new(), Vec::new()]).collect();
        let mut past_len = 0usize;
        let mut tokens: Vec<i64> = Vec::new();
        let (mut sum_lp, mut no_speech) = (0.0f32, 0.0f32);
        let mut looped = false;
        // On parle à 3 mots par seconde environ : au-delà de 8 jetons par seconde, le modèle invente.
        let max_tokens = max_tokens.min(12 + (audio.len() as f32 / RATE as f32 * 8.0) as usize);
        let mut input: Vec<i64> = prompt.clone();
        let heads = self.heads;
        for step in 0..max_tokens {
            let first = step == 0;
            // Au premier passage, le cache est vide (longueur 0) et ignoré par le modèle.
            let (dl, el) = if first { (0, 0) } else { (past_len, t_enc) };
            let flag = [!first];
            let mut inputs: Vec<(String, SessionInputValue<'_>)> = Vec::with_capacity(3 + 4 * self.layers);
            inputs.push(("input_ids".into(), TensorRef::from_array_view(([1usize, input.len()], &input[..]))?.into()));
            inputs.push(("encoder_hidden_states".into(), TensorRef::from_array_view(([1usize, t_enc, d_model], &enc[..]))?.into()));
            for (i, p) in past.iter().enumerate() {
                let names = ["decoder.key", "decoder.value", "encoder.key", "encoder.value"];
                for (k, name) in names.iter().enumerate() {
                    let len = if k < 2 { dl } else { el };
                    inputs.push((format!("past_key_values.{i}.{name}"), TensorRef::from_array_view(([1usize, heads, len, head_dim], &p[k][..heads * len * head_dim]))?.into()));
                }
            }
            inputs.push(("use_cache_branch".into(), TensorRef::from_array_view(([1usize], &flag[..]))?.into()));
            let out = self.decoder.run(inputs)?;
            let (lshape, logits) = out["logits"].try_extract_tensor::<f32>()?;
            let (seq, vocab) = (lshape[1] as usize, lshape[2] as usize);
            if first {
                // « Pas de parole » : lu après le jeton de début de transcription.
                let row = &logits[..vocab];
                no_speech = softmax_at(row, s.no_speech as usize);
            }
            let row = &logits[(seq - 1) * vocab..seq * vocab];
            // Jetons autorisés : texte, et fin (sauf tout au début : un texte vide n'a pas de sens).
            let mut best = (0usize, f32::MIN);
            for (id, &v) in row.iter().enumerate() {
                let allowed = (id as i64) < s.first_special || (id as i64 == s.eot && !first);
                if allowed && v > best.1 {
                    best = (id, v);
                }
            }
            if first && (best.0 == 220) {
                // Espace seule en début de texte : interdite, comme dans Whisper.
                let mut second = (0usize, f32::MIN);
                for (id, &v) in row.iter().enumerate() {
                    if (id as i64) < s.first_special && id != 220 && v > second.1 {
                        second = (id, v);
                    }
                }
                best = second;
            }
            sum_lp += log_softmax_at(row, best.0);
            for (i, p) in past.iter_mut().enumerate() {
                p[0] = out[format!("present.{i}.decoder.key").as_str()].try_extract_tensor::<f32>()?.1.to_vec();
                p[1] = out[format!("present.{i}.decoder.value").as_str()].try_extract_tensor::<f32>()?.1.to_vec();
                if first {
                    p[2] = out[format!("present.{i}.encoder.key").as_str()].try_extract_tensor::<f32>()?.1.to_vec();
                    p[3] = out[format!("present.{i}.encoder.value").as_str()].try_extract_tensor::<f32>()?.1.to_vec();
                }
            }
            past_len += input.len();
            if best.0 as i64 == s.eot {
                break;
            }
            tokens.push(best.0 as i64);
            // Boucle (« la la la… », une phrase répétée) : on garde un exemplaire et on s'arrête.
            if let Some(cut) = repeated_tail(&tokens) {
                tokens.truncate(cut);
                looped = true;
                break;
            }
            input = vec![best.0 as i64];
        }
        let n = tokens.len().max(1);
        let text = strip_annotations(&self.tokenizer.decode(&tokens));
        Ok(Transcript { text, avg_logprob: sum_lp / (n + 1) as f32, no_speech, tokens: tokens.len(), looped })
    }
}

/// Fin de séquence faite d'un même motif (1 à 8 jetons) répété au moins 3 fois de suite :
/// longueur à garder (un seul exemplaire du motif).
fn repeated_tail(tokens: &[i64]) -> Option<usize> {
    for n in 1..=8usize {
        let reps = if n == 1 { 6 } else { 3 };
        if tokens.len() < n * reps {
            continue;
        }
        let tail = &tokens[tokens.len() - n..];
        if (1..reps).all(|k| &tokens[tokens.len() - (k + 1) * n..tokens.len() - k * n] == tail) {
            return Some(tokens.len() - (reps - 1) * n);
        }
    }
    None
}

fn softmax_at(row: &[f32], i: usize) -> f32 {
    log_softmax_at(row, i).exp()
}

fn log_softmax_at(row: &[f32], i: usize) -> f32 {
    let max = row.iter().copied().fold(f32::MIN, f32::max);
    let sum: f32 = row.iter().map(|v| (v - max).exp()).sum();
    row[i] - max - sum.ln()
}

/// Retire les descriptions de sons que Whisper ajoute (« *rires* », « [Musique] »,
/// « (applaudissements) ») : ce ne sont pas des paroles. Mesuré sur ESC-50 : toutes les
/// « phrases » tirées de toux, rires, éternuements ou sirènes étaient de ce genre.
pub fn strip_annotations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut close: Option<char> = None;
    for c in text.chars() {
        match close {
            Some(end) if c == end => close = None,
            Some(_) => {}
            None => match c {
                '*' => close = Some('*'),
                '[' => close = Some(']'),
                '(' => close = Some(')'),
                _ => out.push(c),
            },
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Phrases que Whisper « entend » dans le silence ou le bruit (sous-titres de vidéos de son
/// corpus d'entraînement) : jamais rapportées.
pub fn is_hallucination(text: &str) -> bool {
    let t = text.to_lowercase();
    let t = t.trim_matches(|c: char| !c.is_alphanumeric());
    const KNOWN: [&str; 14] = [
        "sous-titres réalisés par",
        "sous-titrage",
        "amara.org",
        "merci d'avoir regardé",
        "merci de votre attention",
        "abonnez-vous",
        "n'oubliez pas de vous abonner",
        "sous-titres fait par",
        "thank you for watching",
        "thanks for watching",
        "subtitles by",
        "radio-canada",
        "♪",
        "...",
    ];
    t.is_empty() || KNOWN.iter().any(|k| t.contains(k)) || t.chars().all(|c| !c.is_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_level_decoding() {
        // Mini tokeniseur : « Ġ » est l'espace de GPT-2, « Ã© » le é en UTF-8 vu octet par octet.
        let json = r#"{"model":{"vocab":{"Bon":0,"jour":1,"Ġle":2,"ĠcafÃ©":3,"!":4}},
            "added_tokens":[{"id":5,"content":"<|endoftext|>"},{"id":6,"content":"<|startoftranscript|>"},{"id":7,"content":"<|fr|>"},
            {"id":8,"content":"<|transcribe|>"},{"id":9,"content":"<|nocaptions|>"},{"id":10,"content":"<|notimestamps|>"}]}"#;
        let t = Tokenizer::parse(json).unwrap();
        assert_eq!(t.decode(&[0, 1, 2, 3, 4]), "Bonjour le café!");
        assert_eq!(t.decode(&[6, 0, 5]), "Bon");
        let s = t.special().unwrap();
        assert_eq!((s.eot, s.sot, s.transcribe, s.no_speech, s.no_timestamps), (5, 6, 8, 9, 10));
        assert_eq!(t.language("fr").unwrap(), 7);
    }

    #[test]
    fn gpt2_byte_table() {
        let t = bytes_to_unicode();
        assert_eq!(t.len(), 256);
        let map: HashMap<u8, char> = t.into_iter().collect();
        assert_eq!(map[&b'A'], 'A');
        assert_eq!(map[&b' '], 'Ġ');
        assert_eq!(map[&b'\n'], 'Ċ');
    }

    #[test]
    fn features_shape_and_scale() {
        let mel = Mel::new(MelConfig::whisper(80));
        let audio = crate::listen::dsp::sine(440.0, RATE, 2.0, 0.3);
        let f = features(&mel, &audio);
        assert_eq!(f.len(), 80 * FRAMES);
        let max = f.iter().copied().fold(f32::MIN, f32::max);
        let min = f.iter().copied().fold(f32::MAX, f32::min);
        // Plage de Whisper : max − 8 en log10, ramené par (x + 4) / 4.
        assert!((max - min - 2.0).abs() < 1e-4, "{min} … {max}");
    }

    #[test]
    fn loops_are_cut() {
        assert_eq!(repeated_tail(&[5, 6, 7, 8, 9, 7, 8, 9, 7, 8, 9]), Some(5));
        assert_eq!(repeated_tail(&[1, 2, 3, 4]), None);
        assert_eq!(repeated_tail(&[3, 3, 3, 3, 3, 3]), Some(1));
        assert_eq!(repeated_tail(&[3, 3, 3]), None, "« très très très » n'est pas une boucle");
    }

    #[test]
    fn sound_descriptions_are_not_speech() {
        assert_eq!(strip_annotations("*rires*"), "");
        assert_eq!(strip_annotations(" [Musique] "), "");
        assert_eq!(strip_annotations("Bonjour (rires) à tous *Cough*"), "Bonjour à tous");
        assert!(is_hallucination(&strip_annotations("*Cri de Brioche*")));
    }

    #[test]
    fn known_hallucinations() {
        assert!(is_hallucination("Sous-titres réalisés par la communauté d'Amara.org"));
        assert!(is_hallucination(" ... "));
        assert!(is_hallucination("Merci d'avoir regardé cette vidéo !"));
        assert!(!is_hallucination("Au secours, je suis tombé"));
    }
}
