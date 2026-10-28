//! Géométrie commune : boîtes, points, polygones, en coordonnées **normalisées** (0…1 sur la
//! largeur et la hauteur de l'image). Ainsi une zone ou une détection garde son sens quelle que
//! soit la résolution du flux (fluide 640×480 ou HD 2304×1296).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub fn new(x: f32, y: f32) -> Self {
        Point { x, y }
    }

    pub fn dist(self, o: Point) -> f32 {
        ((self.x - o.x).powi(2) + (self.y - o.y).powi(2)).sqrt()
    }
}

/// Boîte alignée sur les axes : coin haut-gauche (x0, y0), coin bas-droit (x1, y1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BBox {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl BBox {
    pub fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Self {
        BBox { x0: x0.min(x1), y0: y0.min(y1), x1: x0.max(x1), y1: y0.max(y1) }
    }

    pub fn width(&self) -> f32 {
        (self.x1 - self.x0).max(0.0)
    }

    pub fn height(&self) -> f32 {
        (self.y1 - self.y0).max(0.0)
    }

    pub fn area(&self) -> f32 {
        self.width() * self.height()
    }

    pub fn center(&self) -> Point {
        Point::new((self.x0 + self.x1) / 2.0, (self.y0 + self.y1) / 2.0)
    }

    /// Point d'appui au sol (milieu du bas) : c'est lui qui dit dans quelle zone se tient une personne.
    pub fn foot(&self) -> Point {
        Point::new((self.x0 + self.x1) / 2.0, self.y1)
    }

    /// Rapport largeur / hauteur **en pixels** (les coordonnées normalisées déforment sinon).
    pub fn aspect(&self, frame_w: f32, frame_h: f32) -> f32 {
        let h = self.height() * frame_h;
        if h <= 0.0 {
            return 0.0;
        }
        self.width() * frame_w / h
    }

    pub fn intersection(&self, o: &BBox) -> f32 {
        let w = (self.x1.min(o.x1) - self.x0.max(o.x0)).max(0.0);
        let h = (self.y1.min(o.y1) - self.y0.max(o.y0)).max(0.0);
        w * h
    }

    /// Recouvrement (intersection sur union), 0…1.
    pub fn iou(&self, o: &BBox) -> f32 {
        let i = self.intersection(o);
        let u = self.area() + o.area() - i;
        if u <= 0.0 {
            0.0
        } else {
            i / u
        }
    }

    /// Boîte ramenée dans l'image.
    pub fn clamp(&self) -> BBox {
        BBox::new(self.x0.clamp(0.0, 1.0), self.y0.clamp(0.0, 1.0), self.x1.clamp(0.0, 1.0), self.y1.clamp(0.0, 1.0))
    }

    /// Boîte agrandie de `k` (0.2 = +20 %) autour de son centre.
    pub fn grow(&self, k: f32) -> BBox {
        let (dw, dh) = (self.width() * k / 2.0, self.height() * k / 2.0);
        BBox::new(self.x0 - dw, self.y0 - dh, self.x1 + dw, self.y1 + dh)
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x0 && p.x <= self.x1 && p.y >= self.y0 && p.y <= self.y1
    }
}

/// Suppression des doublons : garde la meilleure boîte de chaque groupe qui se recouvre à plus
/// de `iou` ; `items` : (boîte, score). Renvoie les indices gardés, du meilleur score au moins bon.
pub fn nms(items: &[(BBox, f32)], iou: f32) -> Vec<usize> {
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&a, &b| items[b].1.total_cmp(&items[a].1));
    let mut keep: Vec<usize> = Vec::new();
    for i in order {
        if keep.iter().all(|&k| items[k].0.iou(&items[i].0) <= iou) {
            keep.push(i);
        }
    }
    keep
}

/// Polygone fermé (zone dessinée sur l'image).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Polygon(pub Vec<Point>);

impl Polygon {
    /// Le point est-il dedans ? (règle pair-impair ; un polygone de moins de 3 points ne contient rien)
    pub fn contains(&self, p: Point) -> bool {
        let pts = &self.0;
        if pts.len() < 3 {
            return false;
        }
        let mut inside = false;
        let mut j = pts.len() - 1;
        for i in 0..pts.len() {
            let (a, b) = (pts[i], pts[j]);
            if (a.y > p.y) != (b.y > p.y) && p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x {
                inside = !inside;
            }
            j = i;
        }
        inside
    }

    /// Toute l'image.
    pub fn full() -> Polygon {
        Polygon(vec![Point::new(0.0, 0.0), Point::new(1.0, 0.0), Point::new(1.0, 1.0), Point::new(0.0, 1.0)])
    }

    /// Aire, en fraction de l'image (formule du lacet).
    pub fn area(&self) -> f32 {
        let pts = &self.0;
        let n = pts.len();
        (0..n).map(|i| pts[i].x * pts[(i + 1) % n].y - pts[(i + 1) % n].x * pts[i].y).sum::<f32>().abs() / 2.0
    }

    /// Deux côtés non voisins se touchent ou se croisent (zone « en nœud papillon »).
    pub fn self_intersects(&self) -> bool {
        let (pts, n) = (&self.0, self.0.len());
        for i in 0..n {
            for j in i + 2..n {
                // Le dernier côté et le premier partagent un sommet.
                if i == 0 && j == n - 1 {
                    continue;
                }
                if segments_meet(pts[i], pts[(i + 1) % n], pts[j], pts[(j + 1) % n]) {
                    return true;
                }
            }
        }
        false
    }

    /// Pourquoi ce polygone ne peut pas servir de zone (None : il convient).
    pub fn problem(&self) -> Option<&'static str> {
        if self.0.len() < 3 {
            return Some("il faut au moins 3 points");
        }
        if self.0.iter().any(|p| !(0.0..=1.0).contains(&p.x) || !(0.0..=1.0).contains(&p.y)) {
            return Some("un point sort de l'image");
        }
        if self.self_intersects() {
            return Some("ses côtés se croisent");
        }
        if self.area() < 0.001 {
            return Some("zone trop petite");
        }
        None
    }
}

/// Les segments [a, b] et [c, d] ont-ils un point commun ?
fn segments_meet(a: Point, b: Point, c: Point, d: Point) -> bool {
    let turn = |p: Point, q: Point, r: Point| (q.x - p.x) * (r.y - p.y) - (q.y - p.y) * (r.x - p.x);
    let (d1, d2, d3, d4) = (turn(c, d, a), turn(c, d, b), turn(a, b, c), turn(a, b, d));
    if d1 * d2 < 0.0 && d3 * d4 < 0.0 {
        return true;
    }
    // Contact : un bout posé sur l'autre segment.
    let on = |p: Point, q: Point, r: Point| r.x >= p.x.min(q.x) && r.x <= p.x.max(q.x) && r.y >= p.y.min(q.y) && r.y <= p.y.max(q.y);
    (d1 == 0.0 && on(c, d, a)) || (d2 == 0.0 && on(c, d, b)) || (d3 == 0.0 && on(a, b, c)) || (d4 == 0.0 && on(a, b, d))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iou_and_nms() {
        let a = BBox::new(0.0, 0.0, 0.5, 0.5);
        let b = BBox::new(0.25, 0.0, 0.75, 0.5);
        assert!((a.iou(&b) - 1.0 / 3.0).abs() < 1e-6);
        assert_eq!(a.iou(&BBox::new(0.6, 0.6, 0.9, 0.9)), 0.0);
        // Deux boîtes presque identiques et une à part : la meilleure du groupe, puis l'isolée.
        let items = [(a, 0.6), (BBox::new(0.01, 0.0, 0.5, 0.5), 0.9), (BBox::new(0.6, 0.6, 0.9, 0.9), 0.5)];
        assert_eq!(nms(&items, 0.5), vec![1, 2]);
    }

    #[test]
    fn polygons_and_feet() {
        let tri = Polygon(vec![Point::new(0.0, 0.0), Point::new(1.0, 0.0), Point::new(0.0, 1.0)]);
        assert!(tri.contains(Point::new(0.2, 0.2)));
        assert!(!tri.contains(Point::new(0.8, 0.8)));
        assert!(!Polygon(vec![Point::new(0.0, 0.0), Point::new(1.0, 1.0)]).contains(Point::new(0.5, 0.5)));
        let person = BBox::new(0.4, 0.2, 0.5, 0.9);
        assert_eq!(person.foot(), Point::new(0.45, 0.9));
        // 0,1 × 640 px de large pour 0,7 × 480 px de haut : debout.
        assert!(person.aspect(640.0, 480.0) < 0.3);
    }

    fn poly(pts: &[(f32, f32)]) -> Polygon {
        Polygon(pts.iter().map(|&(x, y)| Point::new(x, y)).collect())
    }

    #[test]
    fn zone_shapes() {
        let square = poly(&[(0.2, 0.2), (0.6, 0.2), (0.6, 0.6), (0.2, 0.6)]);
        assert!((square.area() - 0.16).abs() < 1e-6);
        assert_eq!(square.problem(), None);
        // En L (concave) : valable.
        let l = poly(&[(0.1, 0.1), (0.3, 0.1), (0.3, 0.5), (0.6, 0.5), (0.6, 0.7), (0.1, 0.7)]);
        assert!(!l.self_intersects());
        assert!((l.area() - (0.2 * 0.6 + 0.3 * 0.2)).abs() < 1e-6);
        assert_eq!(l.problem(), None);
        // Nœud papillon : les côtés se croisent.
        let bowtie = poly(&[(0.2, 0.2), (0.6, 0.6), (0.6, 0.2), (0.2, 0.6)]);
        assert!(bowtie.self_intersects());
        assert_eq!(bowtie.problem(), Some("ses côtés se croisent"));
        // Un sommet repassé : contact entre côtés non voisins.
        assert!(poly(&[(0.2, 0.2), (0.6, 0.2), (0.4, 0.4), (0.6, 0.6), (0.2, 0.6), (0.4, 0.4)]).self_intersects());
        assert_eq!(poly(&[(0.2, 0.2), (0.6, 0.2)]).problem(), Some("il faut au moins 3 points"));
        assert_eq!(poly(&[(0.2, 0.2), (1.2, 0.2), (0.2, 0.6)]).problem(), Some("un point sort de l'image"));
        assert_eq!(poly(&[(0.2, 0.2), (0.4, 0.2), (0.6, 0.2)]).problem(), Some("zone trop petite"));
    }
}
