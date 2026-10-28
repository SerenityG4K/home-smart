//! Lecture d'images fixes (JPEG, PNG) en [`Frame`] : photos à analyser, jeux d'évaluation.

use crate::camera::Frame;
use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;

/// Lit un fichier JPEG ou PNG.
pub fn load(path: &Path) -> Result<Frame> {
    let bytes = std::fs::read(path).with_context(|| format!("lecture de {}", path.display()))?;
    decode(&bytes).with_context(|| format!("{}", path.display()))
}

/// Décode une image JPEG ou PNG (reconnue à sa signature) en RGBA.
pub fn decode(bytes: &[u8]) -> Result<Frame> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        decode_jpeg(bytes)
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        decode_png(bytes)
    } else {
        bail!("format d'image non reconnu (JPEG ou PNG attendu)")
    }
}

fn decode_jpeg(bytes: &[u8]) -> Result<Frame> {
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut d = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    let rgba = d.decode().map_err(|e| anyhow!("JPEG illisible : {e:?}"))?;
    let info = d.info().ok_or_else(|| anyhow!("JPEG sans dimensions"))?;
    let (width, height) = (info.width as usize, info.height as usize);
    if rgba.len() != width * height * 4 {
        bail!("JPEG : {} octets pour {width}×{height}", rgba.len());
    }
    Ok(Frame { seq: 0, width, height, rgba })
}

fn decode_png(bytes: &[u8]) -> Result<Frame> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().context("PNG illisible")?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or_else(|| anyhow!("PNG trop grand"))?];
    let info = reader.next_frame(&mut buf).context("PNG illisible")?;
    let (width, height) = (info.width as usize, info.height as usize);
    let px = width * height;
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..px * 4].to_vec(),
        png::ColorType::Rgb => buf[..px * 3].chunks(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf[..px * 2].chunks(2).flat_map(|c| [c[0], c[0], c[0], c[1]]).collect(),
        png::ColorType::Grayscale => buf[..px].iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => bail!("PNG à palette non développée"),
    };
    Ok(Frame { seq: 0, width, height, rgba })
}

/// Redimensionne (moyenne des pixels couverts en réduction, bilinéaire en agrandissement).
pub fn resize(src: &Frame, w: usize, h: usize) -> Frame {
    let (sw, sh) = (src.width, src.height);
    let mut rgba = vec![0u8; w * h * 4];
    if w == 0 || h == 0 || sw == 0 || sh == 0 {
        return Frame { seq: src.seq, width: w, height: h, rgba };
    }
    let (fx, fy) = (sw as f32 / w as f32, sh as f32 / h as f32);
    for y in 0..h {
        for x in 0..w {
            let o = (y * w + x) * 4;
            if fx >= 1.0 && fy >= 1.0 {
                let (x0, x1) = ((x as f32 * fx) as usize, (((x + 1) as f32 * fx).ceil() as usize).min(sw).max((x as f32 * fx) as usize + 1));
                let (y0, y1) = ((y as f32 * fy) as usize, (((y + 1) as f32 * fy).ceil() as usize).min(sh).max((y as f32 * fy) as usize + 1));
                let mut acc = [0u32; 4];
                for yy in y0..y1 {
                    for xx in x0..x1 {
                        let i = (yy * sw + xx) * 4;
                        for c in 0..4 {
                            acc[c] += src.rgba[i + c] as u32;
                        }
                    }
                }
                let n = ((x1 - x0) * (y1 - y0)) as u32;
                for c in 0..4 {
                    rgba[o + c] = (acc[c] / n) as u8;
                }
            } else {
                let sx = ((x as f32 + 0.5) * fx - 0.5).clamp(0.0, (sw - 1) as f32);
                let sy = ((y as f32 + 0.5) * fy - 0.5).clamp(0.0, (sh - 1) as f32);
                let (x0, y0) = (sx as usize, sy as usize);
                let (x1, y1) = ((x0 + 1).min(sw - 1), (y0 + 1).min(sh - 1));
                let (ax, ay) = (sx - x0 as f32, sy - y0 as f32);
                for c in 0..4 {
                    let p = |xx: usize, yy: usize| src.rgba[(yy * sw + xx) * 4 + c] as f32;
                    let top = p(x0, y0) * (1.0 - ax) + p(x1, y0) * ax;
                    let bottom = p(x0, y1) * (1.0 - ax) + p(x1, y1) * ax;
                    rgba[o + c] = (top * (1.0 - ay) + bottom * ay).round() as u8;
                }
            }
        }
    }
    Frame { seq: src.seq, width: w, height: h, rgba }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_down_and_up() {
        // Damier 4×4 de blocs 2×2 : réduit de moitié, chaque pixel est la moyenne de son bloc.
        let mut rgba = Vec::new();
        for y in 0..4 {
            for x in 0..4 {
                let v = if (x / 2 + y / 2) % 2 == 0 { 200 } else { 40 };
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let f = Frame { seq: 0, width: 4, height: 4, rgba };
        let half = resize(&f, 2, 2);
        assert_eq!(half.rgba.chunks(4).map(|p| p[0]).collect::<Vec<_>>(), vec![200, 40, 40, 200]);
        let up = resize(&half, 4, 4);
        assert_eq!((up.width, up.height, up.rgba.len()), (4, 4, 64));
        assert_eq!(up.rgba[0], 200);
    }

    #[test]
    fn png_round_trip_and_unknown_format() {
        let mut rgba = Vec::new();
        for y in 0..8u8 {
            for x in 0..12u8 {
                rgba.extend_from_slice(&[x * 20, y * 30, 128, 255]);
            }
        }
        let frame = Frame { seq: 0, width: 12, height: 8, rgba };
        let path = std::env::temp_dir().join(format!("homesmart-picture-{}.png", std::process::id()));
        crate::camera::save_png(&frame, &path).unwrap();
        let back = load(&path).unwrap();
        assert_eq!((back.width, back.height), (12, 8));
        assert_eq!(back.rgba, frame.rgba);
        let _ = std::fs::remove_file(path);
        assert!(decode(b"GIF89a").is_err());
    }
}
