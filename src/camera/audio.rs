//! Son des caméras (micro) : décodage AAC, G.711 ou PCM 16 bits (L16, caméras HomeSmart), puis
//! lecture sur la sortie audio du PC.
//!
//! Le décodage se fait au fil du flux RTSP ; la lecture (cpal) tourne sur le fil audio du
//! système. Entre les deux, une file courte : si elle grossit (réseau saccadé), les
//! échantillons les plus anciens sont jetés pour que le son reste calé sur l'image (avec un
//! fondu de 5 ms, sans claquement). Avant la file : son amélioré (voir `enhance`), puis
//! rééchantillonnage à noyau sinc vers la carte son (plus d'aigus métalliques ni de repliement).

use anyhow::{anyhow, bail, Context, Result};
use super::enhance::Enhancer;
use crate::listen::dsp::Resampler;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;

/// Décodeur d'une piste audio de caméra, vers des échantillons mono −1…1.
pub struct Decoder {
    kind: Kind,
    rate: u32,
    scratch: Vec<f32>,
}

enum Kind {
    Aac(Box<symphonia_codec_aac::AacDecoder>, usize),
    Pcma,
    Pcmu,
    /// PCM linéaire 16 bits, gros-boutiste (RFC 3551), entrelacé sur `usize` canaux.
    L16(usize),
}

/// Nom lisible d'un codec son annoncé en SDP (« AAC », « G.711 », « PCM »).
pub fn codec_label(encoding: &str) -> &'static str {
    match encoding.to_ascii_lowercase().as_str() {
        "mpeg4-generic" | "aac" | "mp4a-latm" => "AAC",
        "pcma" | "pcmu" => "G.711",
        "l16" => "PCM",
        _ => "?",
    }
}

/// Le décodeur sait-il lire ce codec son (nom d'encodage SDP) ?
pub fn supported(encoding: &str) -> bool {
    codec_label(encoding) != "?"
}

/// G.711 loi A → échantillon 16 bits.
pub fn alaw(a: u8) -> i16 {
    let a = a ^ 0x55;
    let seg = (a >> 4) & 0x07;
    let mut t = ((a & 0x0f) as i16) << 4;
    t += 8;
    if seg >= 1 {
        t += 0x100;
    }
    if seg > 1 {
        t <<= seg - 1;
    }
    if a & 0x80 != 0 {
        t
    } else {
        -t
    }
}

/// G.711 loi µ → échantillon 16 bits.
pub fn ulaw(u: u8) -> i16 {
    let u = !u;
    let t = ((((u & 0x0f) as i16) << 3) + 0x84) << ((u >> 4) & 0x07);
    if u & 0x80 != 0 {
        0x84 - t
    } else {
        t - 0x84
    }
}

impl Decoder {
    /// `encoding` : nom SDP (« mpeg4-generic », « pcma », « pcmu », « l16 »). `config` : AudioSpecificConfig pour l'AAC.
    pub fn new(encoding: &str, rate: u32, channels: u16, config: &[u8]) -> Result<Self> {
        let kind = match encoding.to_ascii_lowercase().as_str() {
            "mpeg4-generic" | "aac" | "mp4a-latm" => {
                let mut params = AudioCodecParameters::new();
                params.for_codec(CODEC_ID_AAC).with_sample_rate(rate);
                if !config.is_empty() {
                    params.with_extra_data(config.to_vec().into_boxed_slice());
                }
                let dec = symphonia_codec_aac::AacDecoder::try_new(&params, &AudioDecoderOptions::default()).map_err(|e| anyhow!("décodeur AAC : {e}"))?;
                Kind::Aac(Box::new(dec), channels.max(1) as usize)
            }
            "pcma" => Kind::Pcma,
            "pcmu" => Kind::Pcmu,
            "l16" => Kind::L16(channels.max(1) as usize),
            other => bail!("son en {other} non pris en charge"),
        };
        Ok(Decoder { kind, rate, scratch: Vec::new() })
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Ajoute à `out` les échantillons mono de ce paquet.
    pub fn decode(&mut self, data: &[u8], out: &mut Vec<f32>) -> Result<()> {
        match &mut self.kind {
            Kind::Pcma => out.extend(data.iter().map(|&b| alaw(b) as f32 / 32768.0)),
            Kind::Pcmu => out.extend(data.iter().map(|&b| ulaw(b) as f32 / 32768.0)),
            Kind::L16(channels) => {
                // Mono : moyenne des canaux de chaque trame (un octet impair en trop est ignoré).
                let frame = 2 * *channels;
                out.extend(data.chunks_exact(frame).map(|f| f.chunks_exact(2).map(|s| i16::from_be_bytes([s[0], s[1]]) as f32).sum::<f32>() / (32768.0 * *channels as f32)));
            }
            Kind::Aac(dec, channels) => {
                let packet = PacketRef::new(0, Default::default(), Default::default(), data);
                let buf = dec.decode_ref(&packet).map_err(|e| anyhow!("AAC : {e}"))?;
                self.scratch.clear();
                buf.copy_to_vec_interleaved(&mut self.scratch);
                let ch = (*channels).max(1);
                // Mono : moyenne des canaux.
                out.extend(self.scratch.chunks(ch).map(|f| f.iter().sum::<f32>() / f.len() as f32));
            }
        }
        Ok(())
    }
}

/// File entre le décodage et la carte son (partageable entre fils).
#[derive(Clone)]
pub struct Output {
    queue: Arc<Mutex<Queue>>,
    volume: Arc<AtomicU32>,
    enhance: Arc<AtomicBool>,
}

struct Queue {
    buf: VecDeque<f32>,
    rate: u32,
    enhancer: Option<Enhancer>,
    resampler: Option<Resampler>,
    /// Échantillons restant à faire monter en fondu (après un saut ou un silence forcé).
    fade_in: usize,
}

/// Latence maximale tolérée avant de rattraper le direct.
const MAX_LATENCY_S: f32 = 0.5;

/// Durée des fondus (5 ms) en échantillons.
fn fade_len(rate: u32) -> usize {
    rate as usize / 200
}

impl Output {
    fn new(rate: u32) -> Self {
        Output {
            queue: Arc::new(Mutex::new(Queue { buf: VecDeque::new(), rate, enhancer: None, resampler: None, fade_in: 0 })),
            volume: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            enhance: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Ajoute des échantillons mono à `rate` Hz (améliorés si demandé, puis rééchantillonnés).
    pub fn push(&self, mono: &[f32], rate: u32) {
        if mono.is_empty() || rate == 0 {
            return;
        }
        let mut q = self.queue.lock().unwrap();
        let mut x = mono.to_vec();
        if self.enhance.load(Ordering::Relaxed) {
            if q.enhancer.as_ref().is_none_or(|e| e.rate() != rate) {
                q.enhancer = Some(Enhancer::new(rate));
            }
            if let Some(e) = q.enhancer.as_mut() {
                e.process(&mut x);
            }
        }
        let out_rate = q.rate;
        if q.resampler.as_ref().is_none_or(|r| r.rates() != (rate, out_rate)) {
            q.resampler = Some(Resampler::new(rate, out_rate));
        }
        let mut y = Vec::with_capacity(x.len() * out_rate as usize / rate as usize + 64);
        if let Some(r) = q.resampler.as_mut() {
            r.process(&x, &mut y);
        }
        q.buf.extend(y);
        let max = (out_rate as f32 * MAX_LATENCY_S) as usize;
        if q.buf.len() > max {
            let excess = q.buf.len() - max / 2;
            q.buf.drain(..excess);
            q.fade_in = fade_len(out_rate);
        }
    }

    pub fn set_volume(&self, v: f32) {
        self.volume.store(v.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }

    /// Son amélioré (filtre des basses, volume automatique, limiteur) : activé par défaut.
    pub fn set_enhance(&self, on: bool) {
        if self.enhance.swap(on, Ordering::Relaxed) != on {
            self.queue.lock().unwrap().enhancer = None;
        }
    }

    fn pull(&self, n: usize, out: &mut Vec<f32>) {
        let vol = f32::from_bits(self.volume.load(Ordering::Relaxed));
        let mut q = self.queue.lock().unwrap();
        let fade = fade_len(q.rate).max(1);
        out.clear();
        for _ in 0..n {
            let s = match q.buf.pop_front() {
                Some(s) => s,
                None => {
                    // File vide (réseau) : silence, puis reprise en fondu.
                    q.fade_in = fade;
                    0.0
                }
            };
            let ramp = if q.fade_in > 0 {
                q.fade_in -= 1;
                1.0 - q.fade_in as f32 / fade as f32
            } else {
                1.0
            };
            out.push(s * vol * ramp);
        }
    }
}

/// Lecture sur la sortie audio par défaut. À garder sur le fil qui l'a créée (cpal).
pub struct Player {
    _stream: cpal::Stream,
    pub output: Output,
}

impl Player {
    pub fn new() -> Result<Player> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| anyhow!("aucune sortie audio"))?;
        let supported = device.default_output_config().context("sortie audio")?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let channels = config.channels as usize;
        let output = Output::new(config.sample_rate as u32);
        let err = |e| tracing::warn!("sortie audio : {e}");
        let stream = match format {
            cpal::SampleFormat::F32 => {
                let o = output.clone();
                let mut tmp = Vec::new();
                device.build_output_stream(
                    config,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        o.pull(data.len() / channels, &mut tmp);
                        for (frame, s) in data.chunks_mut(channels).zip(&tmp) {
                            frame.fill(*s);
                        }
                    },
                    err,
                    None,
                )
            }
            cpal::SampleFormat::I16 => {
                let o = output.clone();
                let mut tmp = Vec::new();
                device.build_output_stream(
                    config,
                    move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                        o.pull(data.len() / channels, &mut tmp);
                        for (frame, s) in data.chunks_mut(channels).zip(&tmp) {
                            frame.fill((s.clamp(-1.0, 1.0) * 32767.0) as i16);
                        }
                    },
                    err,
                    None,
                )
            }
            other => bail!("format de sortie audio {other:?} non pris en charge"),
        }
        .context("ouverture de la sortie audio")?;
        stream.play().context("lecture audio")?;
        Ok(Player { _stream: stream, output })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g711_tables() {
        // Valeurs de référence de la norme (silence et extrêmes).
        assert_eq!(alaw(0xd5), 8);
        assert_eq!(alaw(0x55), -8);
        assert_eq!(alaw(0xaa), 32256);
        assert_eq!(ulaw(0xff), 0);
        assert_eq!(ulaw(0x80), 32124);
        assert_eq!(ulaw(0x00), -32124);
    }

    #[test]
    fn resampling_and_latency_cap() {
        let o = Output::new(48_000);
        // Valeur constante : sans amélioration (le filtre des basses l'effacerait).
        o.set_enhance(false);
        o.push(&vec![0.5; 1600], 16_000); // 100 ms
        let n = o.queue.lock().unwrap().buf.len();
        assert!((4700..=4800).contains(&n), "{n}");
        // Une seconde d'un coup : la file est ramenée sous la latence maximale, en fondu.
        o.push(&vec![0.5; 16_000], 16_000);
        assert!(o.queue.lock().unwrap().buf.len() <= (48_000.0 * MAX_LATENCY_S) as usize);
        o.set_volume(0.5);
        let mut out = Vec::new();
        o.pull(400, &mut out);
        assert!(out[0] < 0.01, "pas de saut brusque : {}", out[0]);
        assert!(out[300..].iter().all(|&s| (s - 0.25).abs() < 0.01), "{:?}", &out[300..310]);
    }

    #[test]
    fn an_underrun_fades_back_in() {
        let o = Output::new(16_000);
        o.set_enhance(false);
        let mut out = Vec::new();
        o.pull(50, &mut out);
        o.push(&vec![0.5; 4000], 16_000);
        o.pull(200, &mut out);
        assert!(out[0] < 0.02 && (out[150] - 0.5).abs() < 0.02, "{} {}", out[0], out[150]);
    }

    #[test]
    fn aac_decoder_builds() {
        // AudioSpecificConfig AAC-LC, 16 kHz, mono : 0x14 0x08.
        let d = Decoder::new("mpeg4-generic", 16_000, 1, &[0x14, 0x08]).unwrap();
        assert_eq!(d.rate(), 16_000);
        assert!(Decoder::new("opus", 48_000, 2, &[]).is_err());
    }

    #[test]
    fn l16_is_big_endian_and_downmixed() {
        let mut d = Decoder::new("L16", 16_000, 1, &[]).unwrap();
        let mut out = Vec::new();
        // 0x4000 = +0,5 ; 0xC000 = −0,5 ; 0x7FFF ≈ +1.
        d.decode(&[0x40, 0x00, 0xC0, 0x00, 0x7F, 0xFF], &mut out).unwrap();
        assert_eq!(out.len(), 3);
        assert!((out[0] - 0.5).abs() < 1e-4 && (out[1] + 0.5).abs() < 1e-4 && out[2] > 0.999, "{out:?}");
        // Stéréo : moyenne gauche / droite par trame.
        let mut d = Decoder::new("l16", 48_000, 2, &[]).unwrap();
        out.clear();
        d.decode(&[0x40, 0x00, 0x00, 0x00, 0x20, 0x00, 0x20, 0x00], &mut out).unwrap();
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.25).abs() < 1e-4 && (out[1] - 0.25).abs() < 1e-4, "{out:?}");
        assert_eq!((codec_label("L16"), codec_label("PCMU"), codec_label("MPEG4-GENERIC")), ("PCM", "G.711", "AAC"));
        assert!(supported("l16") && !supported("opus"));
    }
}
