//! Lecture de fichiers WAV (PCM 8/16/24/32 bits ou flottants) en échantillons mono −1…1.

use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;

/// Échantillons mono et fréquence.
pub fn read(path: &Path) -> Result<(Vec<f32>, u32)> {
    let bytes = std::fs::read(path).with_context(|| format!("lecture de {}", path.display()))?;
    parse(&bytes).with_context(|| format!("{}", path.display()))
}

pub fn parse(b: &[u8]) -> Result<(Vec<f32>, u32)> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        bail!("pas un fichier WAV");
    }
    let u16le = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32le = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let (mut fmt, mut data) = (None, None);
    let mut o = 12;
    while o + 8 <= b.len() {
        let (id, len) = (&b[o..o + 4], u32le(o + 4) as usize);
        let body = o + 8;
        let end = (body + len).min(b.len());
        match id {
            b"fmt " if len >= 16 => {
                let mut format = u16le(body);
                // WAVE_FORMAT_EXTENSIBLE : le vrai format est au début du sous-format.
                if format == 0xFFFE && len >= 26 {
                    format = u16le(body + 24);
                }
                fmt = Some((format, u16le(body + 2), u32le(body + 4), u16le(body + 14)));
            }
            b"data" => data = Some(&b[body..end]),
            _ => {}
        }
        o = body + len + (len & 1);
    }
    let (format, channels, rate, bits) = fmt.ok_or_else(|| anyhow!("WAV sans format"))?;
    let data = data.ok_or_else(|| anyhow!("WAV sans données"))?;
    let ch = channels.max(1) as usize;
    let samples: Vec<f32> = match (format, bits) {
        (1, 8) => data.iter().map(|&v| (v as f32 - 128.0) / 128.0).collect(),
        (1, 16) => data.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect(),
        (1, 24) => data.chunks_exact(3).map(|c| (i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8) as f32 / 8_388_608.0).collect(),
        (1, 32) => data.chunks_exact(4).map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / 2_147_483_648.0).collect(),
        (3, 32) => data.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect(),
        _ => bail!("WAV : format {format} sur {bits} bits non pris en charge"),
    };
    let mono = samples.chunks(ch).map(|f| f.iter().sum::<f32>() / f.len() as f32).collect();
    Ok((mono, rate))
}

/// WAV PCM 16 bits mono (essais, exports).
pub fn encode(samples: &[f32], rate: u32) -> Vec<u8> {
    let data_len = samples.len() * 2;
    let mut out = Vec::with_capacity(44 + data_len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    for s in samples {
        out.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let x: Vec<f32> = (0..1000).map(|i| ((i as f32) * 0.01).sin() * 0.5).collect();
        let (y, rate) = parse(&encode(&x, 16_000)).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(y.len(), x.len());
        assert!(x.iter().zip(&y).all(|(a, b)| (a - b).abs() < 1e-4));
        assert!(parse(b"RIFF....AVI ").is_err());
    }
}
