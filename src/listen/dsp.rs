//! Traitement du signal pour l'écoute : rééchantillonnage et spectrogrammes mel.
//!
//! Les modèles audio attendent du 16 kHz mono, et presque tous un spectrogramme mel calculé
//! d'une façon précise (fenêtre, échelle mel, normalisation, logarithme) : chaque recette est
//! décrite par un [`MelConfig`] pour reproduire exactement celle de l'entraînement.

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use std::f64::consts::PI;
use std::sync::Arc;

/// Rééchantillonneur au fil de l'eau, à noyau sinc fenêtré (filtre anti-repliement inclus).
pub struct Resampler {
    from: u32,
    to: u32,
    /// Avance dans l'entrée par échantillon de sortie.
    step: f64,
    /// Fréquence de coupure, en fraction de la fréquence d'entrée.
    fc: f64,
    /// Demi-largeur du noyau, en échantillons d'entrée.
    half: f64,
    buf: Vec<f32>,
    pos: f64,
}

/// Passages par zéro du sinc de chaque côté : compromis qualité / calcul.
const ZERO_CROSSINGS: f64 = 10.0;

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        let (f, t) = (from.max(1) as f64, to.max(1) as f64);
        let fc = 0.5 * (t / f).min(1.0) * 0.92;
        let half = ZERO_CROSSINGS / (2.0 * fc);
        // Historique nul au départ : le premier échantillon de sortie tombe sur le premier d'entrée.
        let pad = half.ceil() as usize;
        Resampler { from, to, step: f / t, fc, half, buf: vec![0.0; pad], pos: pad as f64 }
    }

    pub fn rates(&self) -> (u32, u32) {
        (self.from, self.to)
    }

    /// Ajoute `input` et pousse dans `out` tous les échantillons de sortie calculables.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.from == self.to {
            out.extend_from_slice(input);
            return;
        }
        self.buf.extend_from_slice(input);
        let n = self.buf.len() as f64;
        while self.pos + self.half + 1.0 < n {
            let t = self.pos;
            let lo = (t - self.half).ceil().max(0.0) as usize;
            let hi = ((t + self.half).floor() as usize).min(self.buf.len() - 1);
            let mut acc = 0.0f64;
            for (k, &x) in self.buf[lo..=hi].iter().enumerate() {
                let d = (lo + k) as f64 - t;
                acc += x as f64 * kernel(d, self.fc, self.half);
            }
            out.push(acc as f32);
            self.pos += self.step;
        }
        // On ne garde que l'historique encore utile.
        let keep_from = (self.pos - self.half).floor() as isize - 1;
        if keep_from > 0 {
            let k = (keep_from as usize).min(self.buf.len());
            self.buf.drain(..k);
            self.pos -= k as f64;
        }
    }
}

/// Sinc passe-bas de coupure `fc` (fraction de la fréquence d'entrée), fenêtre de Hann.
fn kernel(d: f64, fc: f64, half: f64) -> f64 {
    if d.abs() >= half {
        return 0.0;
    }
    let x = 2.0 * fc * d;
    let sinc = if x.abs() < 1e-9 { 1.0 } else { (PI * x).sin() / (PI * x) };
    let w = 0.5 + 0.5 * (PI * d / half).cos();
    2.0 * fc * sinc * w
}

/// Échelle mel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MelScale {
    /// 2595·log10(1 + f/700) (HTK, TensorFlow).
    Htk,
    /// Linéaire sous 1 kHz, logarithmique au-dessus (Slaney, librosa par défaut, Whisper).
    Slaney,
}

impl MelScale {
    pub fn hz_to_mel(self, f: f64) -> f64 {
        match self {
            MelScale::Htk => 2595.0 * (1.0 + f / 700.0).log10(),
            MelScale::Slaney => {
                let (f_sp, min_log_hz) = (200.0 / 3.0, 1000.0);
                let min_log_mel = min_log_hz / f_sp;
                let logstep = 6.4f64.ln() / 27.0;
                if f >= min_log_hz {
                    min_log_mel + (f / min_log_hz).ln() / logstep
                } else {
                    f / f_sp
                }
            }
        }
    }

    pub fn mel_to_hz(self, m: f64) -> f64 {
        match self {
            MelScale::Htk => 700.0 * (10f64.powf(m / 2595.0) - 1.0),
            MelScale::Slaney => {
                let (f_sp, min_log_hz) = (200.0 / 3.0, 1000.0);
                let min_log_mel = min_log_hz / f_sp;
                let logstep = 6.4f64.ln() / 27.0;
                if m >= min_log_mel {
                    min_log_hz * (logstep * (m - min_log_mel)).exp()
                } else {
                    f_sp * m
                }
            }
        }
    }
}

/// Recette d'un spectrogramme mel.
#[derive(Clone, Debug, PartialEq)]
pub struct MelConfig {
    pub rate: u32,
    pub n_fft: usize,
    /// Longueur de la fenêtre de Hann (≤ n_fft ; complétée de zéros à la fin).
    pub win: usize,
    pub hop: usize,
    pub n_mels: usize,
    pub fmin: f64,
    pub fmax: f64,
    pub scale: MelScale,
    /// Normalisation « Slaney » des triangles (aire constante), comme librosa par défaut.
    pub area_norm: bool,
    /// Triangles tracés dans l'espace mel (TensorFlow) plutôt qu'en hertz (librosa).
    pub mel_triangles: bool,
    /// Puissance (|X|²) plutôt que module (|X|).
    pub power: bool,
}

impl MelConfig {
    /// Whisper (OpenAI) : 16 kHz, fenêtre 25 ms, pas 10 ms, 80 bandes Slaney normalisées.
    pub fn whisper(n_mels: usize) -> Self {
        MelConfig { rate: 16_000, n_fft: 400, win: 400, hop: 160, n_mels, fmin: 0.0, fmax: 8_000.0, scale: MelScale::Slaney, area_norm: true, mel_triangles: false, power: true }
    }
}

/// Calcul des spectrogrammes mel d'une recette.
pub struct Mel {
    pub cfg: MelConfig,
    window: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    /// Pour chaque bande : premier bin de fréquence et poids.
    filters: Vec<(usize, Vec<f32>)>,
}

impl Mel {
    pub fn new(cfg: MelConfig) -> Self {
        assert!(cfg.win <= cfg.n_fft && cfg.hop > 0 && cfg.n_mels > 0);
        // Hann « périodique » (comme torch.hann_window et tf.signal.hann_window par défaut).
        let window = (0..cfg.win).map(|i| (0.5 - 0.5 * (2.0 * PI * i as f64 / cfg.win as f64).cos()) as f32).collect();
        let fft = FftPlanner::new().plan_fft_forward(cfg.n_fft);
        let filters = filterbank(&cfg);
        Mel { cfg, window, fft, filters }
    }

    pub fn n_bins(&self) -> usize {
        self.cfg.n_fft / 2 + 1
    }

    /// Spectre de puissance (ou de module) d'une trame d'au plus `win` échantillons.
    fn spectrum(&self, frame: &[f32], buf: &mut Vec<Complex<f32>>, out: &mut Vec<f32>) {
        buf.clear();
        buf.extend(frame.iter().zip(&self.window).map(|(&x, &w)| Complex::new(x * w, 0.0)));
        buf.resize(self.cfg.n_fft, Complex::new(0.0, 0.0));
        self.fft.process(buf);
        out.clear();
        out.extend(buf[..self.n_bins()].iter().map(|c| if self.cfg.power { c.norm_sqr() } else { c.norm() }));
    }

    /// Énergie mel (linéaire, sans logarithme) de chaque trame : `frames × n_mels`, ligne par trame.
    /// `center` : signal prolongé en miroir de n_fft/2 de chaque côté (torch.stft, center=True).
    pub fn compute(&self, x: &[f32], center: bool) -> (usize, Vec<f32>) {
        let padded;
        let x = if center {
            padded = reflect_pad(x, self.cfg.n_fft / 2);
            &padded[..]
        } else {
            x
        };
        let need = if center { self.cfg.n_fft } else { self.cfg.win };
        if x.len() < need {
            return (0, Vec::new());
        }
        let frames = 1 + (x.len() - need) / self.cfg.hop;
        let mut out = vec![0.0f32; frames * self.cfg.n_mels];
        let (mut buf, mut spec) = (Vec::with_capacity(self.cfg.n_fft), Vec::new());
        // Fenêtre centrée dans la trame de n_fft quand on suit torch (center), sinon en tête (TF).
        let offset = if center { (self.cfg.n_fft - self.cfg.win) / 2 } else { 0 };
        for f in 0..frames {
            let start = f * self.cfg.hop + offset;
            self.spectrum(&x[start..start + self.cfg.win], &mut buf, &mut spec);
            let row = &mut out[f * self.cfg.n_mels..(f + 1) * self.cfg.n_mels];
            for (m, (first, w)) in self.filters.iter().enumerate() {
                row[m] = w.iter().zip(&spec[*first..]).map(|(a, b)| a * b).sum();
            }
        }
        (frames, out)
    }
}

/// Prolongement en miroir sans répéter le bord (numpy « reflect »).
fn reflect_pad(x: &[f32], pad: usize) -> Vec<f32> {
    let n = x.len();
    if n < 2 {
        let mut v = vec![x.first().copied().unwrap_or(0.0); pad];
        v.extend_from_slice(x);
        v.extend(std::iter::repeat_n(x.last().copied().unwrap_or(0.0), pad));
        return v;
    }
    let idx = |i: isize| -> usize {
        let period = 2 * (n as isize - 1);
        let mut j = i.rem_euclid(period);
        if j >= n as isize {
            j = period - j;
        }
        j as usize
    };
    (-(pad as isize)..(n + pad) as isize).map(|i| x[idx(i)]).collect()
}

/// Banc de filtres triangulaires mel, stocké creux (premier bin non nul + poids).
fn filterbank(cfg: &MelConfig) -> Vec<(usize, Vec<f32>)> {
    let n_bins = cfg.n_fft / 2 + 1;
    let s = cfg.scale;
    let (mlo, mhi) = (s.hz_to_mel(cfg.fmin), s.hz_to_mel(cfg.fmax));
    let edges_mel: Vec<f64> = (0..cfg.n_mels + 2).map(|i| mlo + (mhi - mlo) * i as f64 / (cfg.n_mels + 1) as f64).collect();
    let edges_hz: Vec<f64> = edges_mel.iter().map(|&m| s.mel_to_hz(m)).collect();
    let freqs: Vec<f64> = (0..n_bins).map(|k| k as f64 * cfg.rate as f64 / cfg.n_fft as f64).collect();
    (0..cfg.n_mels)
        .map(|m| {
            let weights: Vec<f32> = freqs
                .iter()
                .enumerate()
                .map(|(k, &f)| {
                    let w = if cfg.mel_triangles {
                        // TensorFlow : triangles en mel, bin continu ignoré.
                        if k == 0 {
                            0.0
                        } else {
                            let fm = s.hz_to_mel(f);
                            let lower = (fm - edges_mel[m]) / (edges_mel[m + 1] - edges_mel[m]);
                            let upper = (edges_mel[m + 2] - fm) / (edges_mel[m + 2] - edges_mel[m + 1]);
                            lower.min(upper).max(0.0)
                        }
                    } else {
                        let lower = (f - edges_hz[m]) / (edges_hz[m + 1] - edges_hz[m]);
                        let upper = (edges_hz[m + 2] - f) / (edges_hz[m + 2] - edges_hz[m + 1]);
                        lower.min(upper).max(0.0)
                    };
                    let norm = if cfg.area_norm { 2.0 / (edges_hz[m + 2] - edges_hz[m]) } else { 1.0 };
                    (w * norm) as f32
                })
                .collect();
            let first = weights.iter().position(|&w| w != 0.0).unwrap_or(0);
            let last = weights.iter().rposition(|&w| w != 0.0).map_or(first, |l| l + 1);
            (first, weights[first..last.max(first)].to_vec())
        })
        .collect()
}

/// Niveau RMS d'un bloc, en dBFS (−100 pour le silence numérique).
pub fn rms_dbfs(x: &[f32]) -> f32 {
    if x.is_empty() {
        return -100.0;
    }
    let ms = x.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / x.len() as f64;
    if ms <= 1e-10 {
        -100.0
    } else {
        (10.0 * ms.log10()) as f32
    }
}

#[cfg(test)]
pub(crate) fn sine(freq: f64, rate: u32, secs: f64, amp: f32) -> Vec<f32> {
    let n = (rate as f64 * secs) as usize;
    (0..n).map(|i| amp * (2.0 * PI * freq * i as f64 / rate as f64).sin() as f32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fréquence dominante estimée par passages par zéro.
    fn zero_crossing_freq(x: &[f32], rate: u32) -> f64 {
        let z = x.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        z as f64 / 2.0 / (x.len() as f64 / rate as f64)
    }

    #[test]
    fn resampler_keeps_tones_and_removes_aliases() {
        for (from, to, f) in [(8_000, 16_000, 440.0), (48_000, 16_000, 3_000.0), (44_100, 16_000, 1_000.0), (16_000, 16_000, 500.0)] {
            let x = sine(f, from, 1.0, 0.5);
            let mut r = Resampler::new(from, to);
            let mut y = Vec::new();
            // Par petits blocs irréguliers, comme les paquets RTP.
            for chunk in x.chunks(137) {
                r.process(chunk, &mut y);
            }
            let expected = to as f64 * (x.len() as f64 / from as f64);
            assert!((y.len() as f64 - expected).abs() < expected * 0.01 + 64.0, "{from}->{to}: {} vs {expected}", y.len());
            let mid = &y[y.len() / 4..y.len() * 3 / 4];
            assert!((zero_crossing_freq(mid, to) - f).abs() < f * 0.02, "{from}->{to}");
            let peak = mid.iter().fold(0.0f32, |a, v| a.max(v.abs()));
            assert!((peak - 0.5).abs() < 0.03, "{from}->{to}: amplitude {peak}");
        }
        // 12 kHz n'existe pas à 16 kHz : doit disparaître, pas se replier en 4 kHz.
        let x = sine(12_000.0, 48_000, 0.5, 0.5);
        let mut r = Resampler::new(48_000, 16_000);
        let mut y = Vec::new();
        r.process(&x, &mut y);
        assert!(rms_dbfs(&y[200..]) < -40.0, "repliement : {}", rms_dbfs(&y[200..]));
    }

    #[test]
    fn mel_scales_round_trip() {
        for s in [MelScale::Htk, MelScale::Slaney] {
            for f in [0.0, 300.0, 1000.0, 4000.0, 7999.0] {
                assert!((s.mel_to_hz(s.hz_to_mel(f)) - f).abs() < 1e-6);
            }
        }
        // Valeurs de référence de librosa (Slaney) et HTK.
        assert!((MelScale::Slaney.hz_to_mel(1000.0) - 15.0).abs() < 1e-9);
        assert!((MelScale::Htk.hz_to_mel(1000.0) - 999.985_6).abs() < 1e-3);
    }

    #[test]
    fn tone_lands_in_its_band() {
        let mel = Mel::new(MelConfig::whisper(80));
        // Filtres : chaque bande a des poids, et ils couvrent tout le spectre utile.
        assert!(mel.filters.iter().all(|(_, w)| !w.is_empty()));
        for f in [250.0, 1000.0, 3000.0] {
            let x = sine(f, 16_000, 0.5, 0.3);
            let (frames, m) = mel.compute(&x, true);
            assert_eq!(frames, 1 + x.len() / 160);
            let row = &m[(frames / 2) * 80..(frames / 2 + 1) * 80];
            let best = (0..80).max_by(|&a, &b| row[a].total_cmp(&row[b])).unwrap();
            let s = MelScale::Slaney;
            let (lo, hi) = (s.hz_to_mel(0.0), s.hz_to_mel(8000.0));
            let center = s.mel_to_hz(lo + (hi - lo) * (best + 1) as f64 / 81.0);
            assert!((center - f).abs() < f * 0.12 + 40.0, "{f} Hz → bande {best} ({center:.0} Hz)");
        }
    }

    #[test]
    fn reflect_padding_matches_numpy() {
        assert_eq!(reflect_pad(&[1.0, 2.0, 3.0, 4.0], 2), vec![3.0, 2.0, 1.0, 2.0, 3.0, 4.0, 3.0, 2.0]);
    }
}
