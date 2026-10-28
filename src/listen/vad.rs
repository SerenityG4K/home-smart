//! Détection de la voix : Silero VAD (v6, licence MIT) et découpage en phrases.
//!
//! Modèle : entrée `input` [1, 576] (64 échantillons de contexte + 512 nouveaux = 32 ms à
//! 16 kHz), `state` [2, 1, 128] et `sr` (16000) ; sorties `output` [1, 1] (probabilité de
//! parole) et `stateN` (état suivant).

use crate::vision::engine::{self, Accel};
use anyhow::Result;
use ort::session::Session;
use ort::value::TensorRef;
use std::path::Path;

pub const RATE: u32 = 16_000;
/// Pas d'analyse : 512 échantillons (32 ms).
pub const CHUNK: usize = 512;
const CONTEXT: usize = 64;

/// Silero VAD, au fil de l'eau.
pub struct SileroVad {
    session: Session,
    state: Vec<f32>,
    context: Vec<f32>,
    input: Vec<f32>,
    pub backend: &'static str,
}

impl SileroVad {
    pub fn load(model: &Path) -> Result<SileroVad> {
        // Minuscule et séquentiel : le processeur est plus rapide qu'un aller-retour à la carte graphique.
        let l = engine::load(model, Accel::Cpu, &[])?;
        Ok(SileroVad { session: l.session, state: vec![0.0; 2 * 128], context: vec![0.0; CONTEXT], input: Vec::with_capacity(CONTEXT + CHUNK), backend: l.backend })
    }

    pub fn reset(&mut self) {
        self.state.iter_mut().for_each(|v| *v = 0.0);
        self.context.iter_mut().for_each(|v| *v = 0.0);
    }

    /// Probabilité de parole des 512 échantillons suivants.
    pub fn prob(&mut self, chunk: &[f32]) -> Result<f32> {
        debug_assert_eq!(chunk.len(), CHUNK);
        self.input.clear();
        self.input.extend_from_slice(&self.context);
        self.input.extend_from_slice(chunk);
        let sr = [RATE as i64];
        let out = self.session.run(ort::inputs![
            "input" => TensorRef::from_array_view(([1usize, CONTEXT + CHUNK], &self.input[..]))?,
            "state" => TensorRef::from_array_view(([2usize, 1, 128], &self.state[..]))?,
            "sr" => TensorRef::from_array_view((Vec::<usize>::new(), &sr[..]))?,
        ])?;
        let p = out["output"].try_extract_tensor::<f32>()?.1.first().copied().unwrap_or(0.0);
        let (_, st) = out["stateN"].try_extract_tensor::<f32>()?;
        if st.len() == self.state.len() {
            self.state.copy_from_slice(st);
        }
        self.context.copy_from_slice(&chunk[CHUNK - CONTEXT..]);
        Ok(p)
    }
}

/// Réglages du découpage (valeurs de Silero, fin de phrase plus tolérante pour la parole lente).
#[derive(Clone, Debug, PartialEq)]
pub struct SegmentRules {
    pub threshold: f32,
    /// Seuil de sortie (hystérésis).
    pub release: f32,
    /// Durée minimale d'une phrase (s).
    pub min_speech: f64,
    /// Silence qui termine une phrase (s).
    pub min_silence: f64,
    /// Marge gardée avant et après (s).
    pub pad: f64,
    /// Durée maximale d'une phrase (s) : au-delà, elle est coupée (Whisper lit 30 s au plus).
    pub max_speech: f64,
}

impl Default for SegmentRules {
    fn default() -> Self {
        SegmentRules { threshold: 0.5, release: 0.35, min_speech: 0.4, min_silence: 0.7, pad: 0.2, max_speech: 25.0 }
    }
}

/// Une phrase entendue.
#[derive(Clone, Debug, PartialEq)]
pub struct Speech {
    /// Début et fin (secondes depuis le début de l'écoute).
    pub start: f64,
    pub end: f64,
    /// Son 16 kHz de la phrase (marges comprises).
    pub audio: Vec<f32>,
}

/// Regroupe les probabilités de parole (une par bloc de 32 ms) en phrases.
pub struct Segmenter {
    pub rules: SegmentRules,
    /// Son récent (pour la marge avant) puis phrase en cours.
    buf: Vec<f32>,
    /// Début (en échantillons absolus) de `buf`.
    buf_start: u64,
    /// Échantillons reçus.
    pos: u64,
    /// Phrase en cours : début (échantillons absolus) et dernier bloc de parole.
    current: Option<(u64, u64)>,
}

impl Segmenter {
    pub fn new(rules: SegmentRules) -> Self {
        Segmenter { rules, buf: Vec::new(), buf_start: 0, pos: 0, current: None }
    }

    fn secs(&self, s: f64) -> u64 {
        (s * RATE as f64) as u64
    }

    /// Ajoute un bloc de [`CHUNK`] échantillons et sa probabilité ; renvoie la phrase terminée.
    pub fn push(&mut self, chunk: &[f32], p: f32) -> Option<Speech> {
        self.buf.extend_from_slice(chunk);
        let chunk_start = self.pos;
        self.pos += chunk.len() as u64;
        let mut done = None;
        match self.current {
            None if p >= self.rules.threshold => self.current = Some((chunk_start, self.pos)),
            None => {}
            Some((start, ref mut last)) => {
                if p >= self.rules.release {
                    *last = self.pos;
                }
                let last = *last;
                let silent = self.pos - last >= self.secs(self.rules.min_silence);
                let too_long = self.pos - start >= self.secs(self.rules.max_speech);
                if silent || too_long {
                    self.current = None;
                    if last - start >= self.secs(self.rules.min_speech) {
                        let pad = self.secs(self.rules.pad);
                        let from = start.saturating_sub(pad).max(self.buf_start);
                        let to = (last + pad).min(self.pos);
                        let a = (from - self.buf_start) as usize;
                        let b = (to - self.buf_start) as usize;
                        done = Some(Speech { start: from as f64 / RATE as f64, end: to as f64 / RATE as f64, audio: self.buf[a..b].to_vec() });
                    }
                    // Une phrase coupée parce que trop longue continue dans la suivante.
                    if too_long && !silent {
                        self.current = Some((self.pos, self.pos));
                    }
                }
            }
        }
        // Hors phrase, on ne garde que la marge avant.
        let keep_from = match self.current {
            Some((start, _)) => start.saturating_sub(self.secs(self.rules.pad)),
            None => self.pos.saturating_sub(self.secs(self.rules.pad)),
        };
        if keep_from > self.buf_start {
            let k = ((keep_from - self.buf_start) as usize).min(self.buf.len());
            self.buf.drain(..k);
            self.buf_start += k as u64;
        }
        done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_from_probabilities() {
        let mut s = Segmenter::new(SegmentRules::default());
        let chunk = vec![0.1f32; CHUNK];
        let mut out = Vec::new();
        // 1 s de silence, 1,5 s de parole avec un trou de 0,3 s, puis 1 s de silence.
        let blocks = |secs: f64| (secs * RATE as f64 / CHUNK as f64).round() as usize;
        let mut probs = vec![0.05; blocks(1.0)];
        probs.extend(vec![0.9; blocks(0.6)]);
        probs.extend(vec![0.1; blocks(0.3)]);
        probs.extend(vec![0.8; blocks(0.6)]);
        probs.extend(vec![0.05; blocks(1.0)]);
        for p in probs {
            out.extend(s.push(&chunk, p));
        }
        assert_eq!(out.len(), 1, "une seule phrase (le trou est trop court)");
        let sp = &out[0];
        assert!((sp.start - 0.8).abs() < 0.05 && (sp.end - 2.7).abs() < 0.08, "{} → {}", sp.start, sp.end);
        assert_eq!(sp.audio.len(), ((sp.end - sp.start) * RATE as f64).round() as usize);
        // Un bruit bref (100 ms) n'est pas une phrase.
        let mut s = Segmenter::new(SegmentRules::default());
        let mut n = 0;
        for p in [0.9, 0.9, 0.9].into_iter().chain(std::iter::repeat_n(0.0, 40)) {
            n += s.push(&chunk, p).is_some() as usize;
        }
        assert_eq!(n, 0);
    }

    #[test]
    fn long_speech_is_split() {
        let mut s = Segmenter::new(SegmentRules { max_speech: 5.0, ..SegmentRules::default() });
        let chunk = vec![0.0f32; CHUNK];
        let n: usize = (0..(12.0 * RATE as f64 / CHUNK as f64) as usize).filter_map(|_| s.push(&chunk, 0.9)).count();
        assert_eq!(n, 2, "12 s de parole continue → deux morceaux de 5 s (le reste en cours)");
    }
}
