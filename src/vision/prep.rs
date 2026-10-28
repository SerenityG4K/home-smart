//! Préparation des images pour les modèles : mise à l'échelle avec bandes (« letterbox »),
//! redressement d'un visage sur le gabarit 112×112 des modèles de reconnaissance, et conversion
//! en tenseur CHW (canaux, lignes, colonnes) au format attendu par chaque modèle.
//!
//! Tout part d'une image RGBA (`camera::Frame`) ; rien n'est alloué si le tampon de sortie a déjà
//! la bonne taille (appel à chaque image).

use super::geom::Point;

/// Format des valeurs d'entrée d'un modèle : valeur = (pixel × `scale` − `mean`) / `std`, par canal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    /// Canaux dans l'ordre B, G, R (modèles issus d'OpenCV) plutôt que R, G, B.
    pub bgr: bool,
    pub scale: f32,
    pub mean: [f32; 3],
    pub std: [f32; 3],
}

impl Layout {
    /// Pixels bruts 0…255.
    pub const fn raw(bgr: bool) -> Layout {
        Layout { bgr, scale: 1.0, mean: [0.0; 3], std: [1.0; 3] }
    }

    /// Pixels ramenés à 0…1.
    pub const fn unit(bgr: bool) -> Layout {
        Layout { bgr, scale: 1.0 / 255.0, mean: [0.0; 3], std: [1.0; 3] }
    }

    #[inline]
    fn put(&self, out: &mut [f32], plane: usize, i: usize, rgb: [f32; 3]) {
        for (c, &v) in rgb.iter().enumerate() {
            let ch = if self.bgr { 2 - c } else { c };
            out[ch * plane + i] = (v * self.scale - self.mean[ch]) / self.std[ch];
        }
    }
}

/// Lecture bilinéaire d'un pixel RGBA aux coordonnées (x, y) en pixels (centre du pixel = +0,5).
/// Hors de l'image : `fill`.
#[inline]
fn sample(rgba: &[u8], w: usize, h: usize, x: f32, y: f32, fill: f32) -> [f32; 3] {
    let (fx, fy) = (x - 0.5, y - 0.5);
    if fx < -1.0 || fy < -1.0 || fx > w as f32 || fy > h as f32 {
        return [fill; 3];
    }
    let (x0, y0) = (fx.floor(), fy.floor());
    let (tx, ty) = (fx - x0, fy - y0);
    let px = |xi: i64, yi: i64| -> [f32; 3] {
        let (xc, yc) = (xi.clamp(0, w as i64 - 1) as usize, yi.clamp(0, h as i64 - 1) as usize);
        let o = (yc * w + xc) * 4;
        [rgba[o] as f32, rgba[o + 1] as f32, rgba[o + 2] as f32]
    };
    let (xi, yi) = (x0 as i64, y0 as i64);
    let (a, b, c, d) = (px(xi, yi), px(xi + 1, yi), px(xi, yi + 1), px(xi + 1, yi + 1));
    let mut out = [0.0; 3];
    for k in 0..3 {
        let top = a[k] + (b[k] - a[k]) * tx;
        let bot = c[k] + (d[k] - c[k]) * tx;
        out[k] = top + (bot - top) * ty;
    }
    out
}

/// Correspondance entre l'image d'origine et l'entrée du modèle après mise à l'échelle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Letterbox {
    pub scale: f32,
    pub pad_x: f32,
    pub pad_y: f32,
    /// Taille (en pixels) de la région mise à l'échelle.
    pub src_w: f32,
    pub src_h: f32,
}

impl Letterbox {
    /// Point en pixels de l'entrée du modèle → coordonnées normalisées de l'image d'origine.
    pub fn to_norm(&self, x: f32, y: f32) -> Point {
        Point::new(((x - self.pad_x) / self.scale) / self.src_w, ((y - self.pad_y) / self.scale) / self.src_h)
    }
}

/// Met l'image RGBA `w`×`h` à l'échelle dans `dw`×`dh` sans la déformer ; le reste est rempli
/// de `fill` (gris 114 pour YOLOX, noir sinon). `centered` : image centrée (sinon calée en haut
/// à gauche, comme l'attendent les détecteurs YOLOX). Écrit un tenseur CHW dans `out`.
#[allow(clippy::too_many_arguments)]
pub fn letterbox(rgba: &[u8], w: usize, h: usize, dw: usize, dh: usize, fill: u8, centered: bool, layout: Layout, out: &mut Vec<f32>) -> Letterbox {
    letterbox_region(rgba, w, h, (0.0, 0.0, w as f32, h as f32), dw, dh, fill, centered, None, layout, out)
}

/// Comme `letterbox`, pour la région `(x0, y0, x1, y1)` (en pixels) de l'image seulement ;
/// `max_scale` borne l'agrandissement d'une petite région. Le `Letterbox` rendu ramène aux
/// coordonnées normalisées **de la région**.
#[allow(clippy::too_many_arguments)]
pub fn letterbox_region(
    rgba: &[u8],
    w: usize,
    h: usize,
    region: (f32, f32, f32, f32),
    dw: usize,
    dh: usize,
    fill: u8,
    centered: bool,
    max_scale: Option<f32>,
    layout: Layout,
    out: &mut Vec<f32>,
) -> Letterbox {
    let (rx0, ry0) = (region.0.max(0.0), region.1.max(0.0));
    let (rw, rh) = ((region.2.min(w as f32) - rx0).max(1.0), (region.3.min(h as f32) - ry0).max(1.0));
    let plane = dw * dh;
    out.resize(plane * 3, 0.0);
    let mut scale = (dw as f32 / rw).min(dh as f32 / rh);
    if let Some(m) = max_scale {
        scale = scale.min(m);
    }
    let (sw, sh) = (rw * scale, rh * scale);
    let (pad_x, pad_y) = if centered { ((dw as f32 - sw) / 2.0, (dh as f32 - sh) / 2.0) } else { (0.0, 0.0) };
    let fill_px = [fill as f32; 3];
    for y in 0..dh {
        let sy = (y as f32 + 0.5 - pad_y) / scale;
        for x in 0..dw {
            let sx = (x as f32 + 0.5 - pad_x) / scale;
            let px = if sx < 0.0 || sy < 0.0 || sx >= rw || sy >= rh { fill_px } else { sample(rgba, w, h, rx0 + sx, ry0 + sy, fill as f32) };
            layout.put(out, plane, y * dw + x, px);
        }
    }
    Letterbox { scale, pad_x, pad_y, src_w: rw, src_h: rh }
}

/// Similitude 2D (rotation, échelle, translation) : `x' = a·x − b·y + tx`, `y' = b·x + a·y + ty`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Similarity {
    pub a: f32,
    pub b: f32,
    pub tx: f32,
    pub ty: f32,
}

impl Similarity {
    /// Meilleure similitude (moindres carrés) qui envoie `src` sur `dst`.
    pub fn estimate(src: &[Point], dst: &[Point]) -> Similarity {
        let n = src.len().min(dst.len()).max(1) as f32;
        let mean = |p: &[Point]| p.iter().fold(Point::new(0.0, 0.0), |m, q| Point::new(m.x + q.x / n, m.y + q.y / n));
        let (ms, md) = (mean(src), mean(dst));
        let (mut num_a, mut num_b, mut den) = (0.0f32, 0.0f32, 0.0f32);
        for (p, q) in src.iter().zip(dst) {
            let (px, py, qx, qy) = (p.x - ms.x, p.y - ms.y, q.x - md.x, q.y - md.y);
            num_a += px * qx + py * qy;
            num_b += px * qy - py * qx;
            den += px * px + py * py;
        }
        let (a, b) = if den > 0.0 { (num_a / den, num_b / den) } else { (1.0, 0.0) };
        Similarity { a, b, tx: md.x - (a * ms.x - b * ms.y), ty: md.y - (b * ms.x + a * ms.y) }
    }

    pub fn apply(&self, p: Point) -> Point {
        Point::new(self.a * p.x - self.b * p.y + self.tx, self.b * p.x + self.a * p.y + self.ty)
    }

    pub fn inverse(&self) -> Similarity {
        let d = self.a * self.a + self.b * self.b;
        let (a, b) = if d > 0.0 { (self.a / d, -self.b / d) } else { (1.0, 0.0) };
        // x = a'·(x' − tx) − b'·(y' − ty), y = b'·(x' − tx) + a'·(y' − ty)
        Similarity { a, b, tx: -(a * self.tx - b * self.ty), ty: -(b * self.tx + a * self.ty) }
    }
}

/// Gabarit des 5 points d'un visage (yeux, nez, coins de la bouche) dans l'image 112×112 attendue
/// par les modèles de reconnaissance de type ArcFace (dont SFace).
pub const FACE_TEMPLATE_112: [Point; 5] = [
    Point { x: 38.2946, y: 51.6963 },
    Point { x: 73.5318, y: 51.5014 },
    Point { x: 56.0252, y: 71.7366 },
    Point { x: 41.5493, y: 92.3655 },
    Point { x: 70.7299, y: 92.2041 },
];

/// Visage redressé : les 5 points `landmarks` (en pixels de l'image d'origine) sont amenés sur
/// le gabarit, et l'image `size`×`size` obtenue est écrite en tenseur CHW dans `out`.
pub fn align_face(rgba: &[u8], w: usize, h: usize, landmarks: &[Point; 5], size: usize, layout: Layout, out: &mut Vec<f32>) {
    let k = size as f32 / 112.0;
    let dst: Vec<Point> = FACE_TEMPLATE_112.iter().map(|p| Point::new(p.x * k, p.y * k)).collect();
    let back = Similarity::estimate(landmarks, &dst).inverse();
    let plane = size * size;
    out.resize(plane * 3, 0.0);
    for y in 0..size {
        for x in 0..size {
            let s = back.apply(Point::new(x as f32 + 0.5, y as f32 + 0.5));
            layout.put(out, plane, y * size + x, sample(rgba, w, h, s.x, s.y, 0.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: usize, h: usize) -> Vec<u8> {
        let mut v = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let o = (y * w + x) * 4;
                v[o..o + 4].copy_from_slice(&[(x * 255 / (w - 1)) as u8, (y * 255 / (h - 1)) as u8, 77, 255]);
            }
        }
        v
    }

    #[test]
    fn letterbox_keeps_proportions_and_maps_back() {
        let (w, h) = (640, 360);
        let img = gradient(w, h);
        let mut out = Vec::new();
        let lb = letterbox(&img, w, h, 320, 320, 114, false, Layout::raw(true), &mut out);
        assert_eq!(out.len(), 3 * 320 * 320);
        assert_eq!(lb.scale, 0.5);
        // Calée en haut : le bas de l'entrée (lignes 180+) est du gris 114.
        assert_eq!(out[300 * 320 + 10], 114.0);
        // BGR : le canal 0 est le bleu (77 partout dans l'image), le canal 2 le rouge (dégradé).
        assert!((out[50 * 320 + 100] - 77.0).abs() < 0.5);
        let red = out[2 * 320 * 320 + 50 * 320 + 160];
        assert!((red - 127.5).abs() < 3.0, "{red}");
        // Retour aux coordonnées normalisées de l'image d'origine.
        let p = lb.to_norm(160.0, 90.0);
        assert!((p.x - 0.5).abs() < 1e-6 && (p.y - 0.5).abs() < 1e-6);
        let c = letterbox(&img, w, h, 320, 320, 0, true, Layout::unit(false), &mut out);
        assert_eq!((c.pad_x, c.pad_y), (0.0, 70.0));
        assert!(out.iter().all(|v| (0.0..=1.0).contains(v)));
    }

    #[test]
    fn similarity_roundtrip() {
        // Rotation de 30°, échelle 2, translation : estimée exactement, puis inversée.
        let (a, b) = (2.0 * 30f32.to_radians().cos(), 2.0 * 30f32.to_radians().sin());
        let t = Similarity { a, b, tx: 5.0, ty: -3.0 };
        let src: Vec<Point> = FACE_TEMPLATE_112.to_vec();
        let dst: Vec<Point> = src.iter().map(|p| t.apply(*p)).collect();
        let est = Similarity::estimate(&src, &dst);
        assert!((est.a - a).abs() < 1e-3 && (est.b - b).abs() < 1e-3 && (est.tx - 5.0).abs() < 1e-2);
        let back = est.inverse().apply(dst[2]);
        assert!(back.dist(src[2]) < 1e-3);
    }

    #[test]
    fn aligned_face_samples_the_right_place() {
        // Un visage « dessiné » : ses points sont le gabarit déplacé de (100, 50) dans une image
        // dont chaque pixel encode sa position ; une fois redressé, le pixel (56, 72) doit
        // provenir du nez, en (156, 122).
        let (w, h) = (256, 256);
        let mut img = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let o = (y * w + x) * 4;
                img[o..o + 4].copy_from_slice(&[x as u8, y as u8, 0, 255]);
            }
        }
        let lm = FACE_TEMPLATE_112.map(|p| Point::new(p.x + 100.0, p.y + 50.0));
        let mut out = Vec::new();
        align_face(&img, w, h, &lm, 112, Layout::raw(false), &mut out);
        let plane = 112 * 112;
        let (r, g) = (out[72 * 112 + 56], out[plane + 72 * 112 + 56]);
        assert!((r - 156.0).abs() < 1.0 && (g - 122.0).abs() < 1.0, "{r} {g}");
    }
}
