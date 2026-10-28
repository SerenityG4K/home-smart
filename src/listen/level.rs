//! Niveau sonore : bruit de fond appris en continu et bruits forts, sans aucun modèle.
//!
//! Sert de détecteur universel (marche sur n'importe quel PC, même sans modèles installés) et
//! d'indicateur de niveau pour l'interface.

use super::dsp::rms_dbfs;
use std::collections::VecDeque;

/// Durée d'un bloc de mesure.
pub const BLOCK_SECS: f64 = 0.1;

/// Réglages du détecteur de bruits forts.
#[derive(Clone, Debug, PartialEq)]
pub struct LoudRules {
    /// Écart minimal avec le bruit de fond (dB).
    pub above_floor: f32,
    /// Niveau absolu minimal (dBFS) : un souffle dans une pièce silencieuse n'est pas un bruit fort.
    pub min_level: f32,
    /// Pas deux signalements à moins de tant de secondes.
    pub cooldown: f64,
}

impl Default for LoudRules {
    fn default() -> Self {
        LoudRules { above_floor: 25.0, min_level: -32.0, cooldown: 10.0 }
    }
}

/// Un bruit fort.
#[derive(Clone, Debug, PartialEq)]
pub struct LoudNoise {
    /// Début (secondes depuis le début de l'écoute).
    pub at: f64,
    /// Niveau de crête (dBFS) et bruit de fond à ce moment.
    pub peak: f32,
    pub floor: f32,
}

/// Mesure du niveau, bloc par bloc (16 kHz attendus mais pas exigés).
pub struct LevelMeter {
    rules: LoudRules,
    rate: u32,
    pending: Vec<f32>,
    /// Niveaux des dernières secondes, pour le bruit de fond (percentile bas).
    history: VecDeque<f32>,
    /// Temps écoulé (s).
    now: f64,
    last_loud: Option<f64>,
    /// Bruit fort en cours : début et crête.
    current: Option<(f64, f32)>,
    pub level: f32,
    pub floor: f32,
}

/// Fenêtre du bruit de fond : une minute.
const HISTORY_BLOCKS: usize = 600;

impl LevelMeter {
    pub fn new(rate: u32, rules: LoudRules) -> Self {
        LevelMeter { rules, rate, pending: Vec::new(), history: VecDeque::new(), now: 0.0, last_loud: None, current: None, level: -100.0, floor: -100.0 }
    }

    /// Ajoute des échantillons ; renvoie les bruits forts terminés.
    pub fn push(&mut self, x: &[f32]) -> Vec<LoudNoise> {
        let block = ((self.rate as f64 * BLOCK_SECS) as usize).max(1);
        self.pending.extend_from_slice(x);
        let mut out = Vec::new();
        let mut start = 0;
        while self.pending.len() - start >= block {
            let level = rms_dbfs(&self.pending[start..start + block]);
            start += block;
            self.now += BLOCK_SECS;
            if let Some(e) = self.block(level) {
                out.push(e);
            }
        }
        self.pending.drain(..start);
        out
    }

    fn block(&mut self, level: f32) -> Option<LoudNoise> {
        self.level = level;
        // Bruit de fond : 20e centile de la dernière minute (les bruits brefs ne le font pas monter).
        let floor = if self.history.len() >= 20 {
            let mut v: Vec<f32> = self.history.iter().copied().collect();
            let k = v.len() / 5;
            *v.select_nth_unstable_by(k, |a, b| a.total_cmp(b)).1
        } else {
            level
        };
        self.floor = floor;
        self.history.push_back(level);
        if self.history.len() > HISTORY_BLOCKS {
            self.history.pop_front();
        }
        // Au démarrage, quelques secondes pour apprendre le bruit de fond.
        let learning = self.history.len() < 20;
        let loud = !learning && level >= self.rules.min_level && level - floor >= self.rules.above_floor;
        match (&mut self.current, loud) {
            (None, true) => {
                self.current = Some((self.now - BLOCK_SECS, level));
                None
            }
            (Some((_, peak)), true) => {
                *peak = peak.max(level);
                None
            }
            (Some(_), false) => {
                let (at, peak) = self.current.take().unwrap();
                if self.last_loud.is_some_and(|t| at - t < self.rules.cooldown) {
                    return None;
                }
                self.last_loud = Some(at);
                Some(LoudNoise { at, peak, floor })
            }
            (None, false) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listen::dsp::sine;

    #[test]
    fn learns_the_floor_and_flags_bangs() {
        let mut m = LevelMeter::new(16_000, LoudRules::default());
        // 10 s de bruit de fond faible (−60 dBFS environ).
        let quiet = sine(200.0, 16_000, 10.0, 0.0014);
        assert!(m.push(&quiet).is_empty());
        assert!((m.floor + 60.0).abs() < 3.0, "fond {}", m.floor);
        // Claquement de 0,3 s à −12 dBFS, puis retour au calme.
        let mut events = m.push(&sine(900.0, 16_000, 0.3, 0.35));
        events.extend(m.push(&sine(200.0, 16_000, 1.0, 0.0014)));
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(events[0].peak > -15.0 && (events[0].at - 10.0).abs() < 0.2, "{events:?}");
        // Deuxième claquement trop proche : pas de second signalement.
        let mut again = m.push(&sine(900.0, 16_000, 0.3, 0.35));
        again.extend(m.push(&sine(200.0, 16_000, 1.0, 0.0014)));
        assert!(again.is_empty());
    }

    #[test]
    fn loud_room_is_not_an_alarm() {
        // Musique forte et continue : le fond monte, rien n'est signalé.
        let mut m = LevelMeter::new(16_000, LoudRules::default());
        assert!(m.push(&sine(440.0, 16_000, 30.0, 0.3)).is_empty());
        assert!(m.floor > -15.0);
    }
}
