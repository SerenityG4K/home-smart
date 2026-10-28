//! Son plus agréable à écouter (micro des caméras, en direct ou en relecture) :
//!
//! - **filtre des basses** (passe-haut à 90 Hz) : ronflement, souffle grave, composante continue ;
//! - **volume automatique** : les micros de caméra sont faibles (crête à 1 % sur une Imou), la voix
//!   est ramenée vers −20 dBFS, sans jamais faire monter le bruit de fond quand rien ne se passe ;
//! - **limiteur doux** : un bruit fort ne sature pas.
//!
//! Tout se fait par blocs, au fil de l'eau, sans retard ajouté. Ce traitement ne sert qu'à
//! l'écoute : l'analyse par IA reçoit le son brut.

/// Filtre biquadratique (forme directe II transposée).
#[derive(Clone, Debug)]
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    z: [f32; 2],
}

impl Biquad {
    /// Passe-haut de Butterworth (Q = 0,707) à `fc` Hz.
    fn highpass(rate: u32, fc: f32) -> Biquad {
        let w = 2.0 * std::f32::consts::PI * fc / rate.max(1) as f32;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        let b0 = (1.0 + cos) / 2.0 / a0;
        Biquad { b: [b0, -(1.0 + cos) / a0, b0], a: [-2.0 * cos / a0, (1.0 - alpha) / a0], z: [0.0; 2] }
    }

    fn run(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// Niveau visé pour la voix (RMS), gain maximal, niveau visé pour le bruit de fond.
const TARGET: f32 = 0.1;
const MAX_GAIN: f32 = 20.0;
const QUIET_TARGET: f32 = 0.003;

pub struct Enhancer {
    rate: u32,
    hp: Biquad,
    /// Bruit de fond estimé (RMS) et gain appliqué.
    floor: f32,
    gain: f32,
}

impl Enhancer {
    pub fn new(rate: u32) -> Enhancer {
        Enhancer { rate, hp: Biquad::highpass(rate, 90.0), floor: 0.0, gain: 1.0 }
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Traite un bloc sur place.
    pub fn process(&mut self, x: &mut [f32]) {
        if x.is_empty() {
            return;
        }
        for v in x.iter_mut() {
            *v = self.hp.run(*v);
        }
        let rms = (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt().max(1e-7);
        // Bruit de fond : suit vite les baisses, remonte lentement (≈ 6 dB par 10 s).
        let secs = x.len() as f32 / self.rate.max(1) as f32;
        self.floor = if self.floor <= 0.0 || rms < self.floor { rms } else { self.floor * (1.0 + 0.07 * secs) };
        // Il se passe quelque chose (voix, bruit) : on vise le niveau d'écoute ; sinon, le bruit de
        // fond reste discret.
        let active = rms > self.floor * 2.5;
        let wanted = if active { TARGET / rms } else { QUIET_TARGET / self.floor.max(1e-7) };
        let wanted = wanted.clamp(1.0, MAX_GAIN);
        // Baisse rapide (20 ms), hausse lente (1 s), en douceur d'un échantillon à l'autre.
        let tau = if wanted < self.gain { 0.02 } else { 1.0 };
        let k = 1.0 - (-secs / tau).exp();
        let next = self.gain + (wanted - self.gain) * k;
        let n = x.len() as f32;
        for (i, v) in x.iter_mut().enumerate() {
            let g = self.gain + (next - self.gain) * (i as f32 + 1.0) / n;
            *v = limit(*v * g);
        }
        self.gain = next;
    }
}

/// Limiteur doux : transparent sous 0,8, puis arrondi vers 1.
fn limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= 0.8 {
        x
    } else {
        x.signum() * (0.8 + 0.2 * ((a - 0.8) / 0.2).tanh())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listen::dsp::{rms_dbfs, sine};

    #[test]
    fn highpass_removes_rumble_keeps_voice() {
        let mut f = Biquad::highpass(16_000, 90.0);
        let low: Vec<f32> = sine(30.0, 16_000, 1.0, 0.5).into_iter().map(|v| f.run(v)).collect();
        let mut f = Biquad::highpass(16_000, 90.0);
        let mid: Vec<f32> = sine(1000.0, 16_000, 1.0, 0.5).into_iter().map(|v| f.run(v)).collect();
        assert!(rms_dbfs(&low[8000..]) < rms_dbfs(&sine(30.0, 16_000, 0.5, 0.5)) - 15.0);
        assert!((rms_dbfs(&mid[8000..]) - rms_dbfs(&sine(1000.0, 16_000, 0.5, 0.5))).abs() < 0.5);
    }

    #[test]
    fn quiet_voice_is_raised_but_silence_stays_quiet() {
        let mut e = Enhancer::new(16_000);
        let mut out = Vec::new();
        // 3 s de souffle à −60 dBFS, puis une « voix » à −42 dBFS pendant 4 s, puis le souffle.
        let mut seed = 7u64;
        let mut hiss = |n: usize| -> Vec<f32> {
            (0..n)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    ((seed >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0) * 0.0017
                })
                .collect()
        };
        let mut signal = hiss(48_000);
        let voice: Vec<f32> = sine(400.0, 16_000, 4.0, 0.011).iter().zip(hiss(64_000)).map(|(a, b)| a + b).collect();
        signal.extend(voice);
        signal.extend(hiss(48_000));
        for block in signal.chunks(1024) {
            let mut b = block.to_vec();
            e.process(&mut b);
            out.extend(b);
        }
        let quiet_before = rms_dbfs(&out[16_000..48_000]);
        let voice_level = rms_dbfs(&out[48_000 + 32_000..48_000 + 64_000]);
        assert!(voice_level > -28.0 && voice_level < -14.0, "voix à {voice_level:.1} dBFS");
        assert!(quiet_before < -45.0, "souffle remonté à {quiet_before:.1} dBFS");
        // Un claquement fort ne sature pas.
        let mut loud = sine(800.0, 16_000, 0.2, 0.99);
        e.process(&mut loud);
        assert!(loud.iter().all(|v| v.abs() <= 1.0));
    }
}
