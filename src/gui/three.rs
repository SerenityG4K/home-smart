//! Petit moteur 3D logiciel : perspective, éclairage plat, tri par profondeur, sélection.
//!
//! Tout est dessiné avec les maillages d'egui : aucune dépendance, aucun calcul au repos
//! (l'image n'est refaite que pendant une rotation, un zoom ou un changement d'état).

use super::{fx, theme};
use eframe::egui::epaint::Mesh;
use eframe::egui::{self, pos2, vec2, Align2, Color32, CursorIcon, FontId, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, Ui};
use std::ops::{Add, Mul, Sub};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct V3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

pub const fn v3(x: f32, y: f32, z: f32) -> V3 {
    V3 { x, y, z }
}

impl Add for V3 {
    type Output = V3;
    fn add(self, o: V3) -> V3 {
        v3(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl Sub for V3 {
    type Output = V3;
    fn sub(self, o: V3) -> V3 {
        v3(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

impl Mul<f32> for V3 {
    type Output = V3;
    fn mul(self, k: f32) -> V3 {
        v3(self.x * k, self.y * k, self.z * k)
    }
}

impl V3 {
    pub fn dot(self, o: V3) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }
    pub fn cross(self, o: V3) -> V3 {
        v3(self.y * o.z - self.z * o.y, self.z * o.x - self.x * o.z, self.x * o.y - self.y * o.x)
    }
    pub fn len(self) -> f32 {
        self.dot(self).sqrt()
    }
    pub fn norm(self) -> V3 {
        let l = self.len();
        if l > 1e-6 {
            self * (1.0 / l)
        } else {
            self
        }
    }
}

/// Caméra orbitale (y vers le haut).
#[derive(Clone, Debug)]
pub struct Camera {
    pub yaw: f32,
    pub pitch: f32,
    pub dist: f32,
    pub target: V3,
    /// Vitesse de rotation résiduelle (inertie).
    pub spin: f32,
    pub min_dist: f32,
    pub max_dist: f32,
}

impl Camera {
    pub fn new(target: V3, dist: f32) -> Self {
        Camera { yaw: 0.65, pitch: 0.75, dist, target, spin: 0.0, min_dist: dist * 0.35, max_dist: dist * 3.0 }
    }

    fn eye(&self) -> V3 {
        let (cp, sp) = (self.pitch.cos(), self.pitch.sin());
        self.target + v3(cp * self.yaw.sin(), sp, cp * self.yaw.cos()) * self.dist
    }

    fn basis(&self) -> (V3, V3, V3, V3) {
        let eye = self.eye();
        let fwd = (self.target - eye).norm();
        let right = fwd.cross(v3(0.0, 1.0, 0.0)).norm();
        let up = right.cross(fwd).norm();
        (eye, right, up, fwd)
    }
}

/// Projection préparée pour une image.
pub struct View {
    eye: V3,
    right: V3,
    up: V3,
    fwd: V3,
    center: Pos2,
    focal: f32,
}

impl View {
    pub fn new(cam: &Camera, rect: Rect) -> Self {
        let (eye, right, up, fwd) = cam.basis();
        View { eye, right, up, fwd, center: rect.center(), focal: rect.height().min(rect.width()) * 1.25 }
    }

    /// Point écran et profondeur ; `None` derrière la caméra.
    pub fn project(&self, p: V3) -> Option<(Pos2, f32)> {
        let d = p - self.eye;
        let z = d.dot(self.fwd);
        if z < 0.05 {
            return None;
        }
        let x = d.dot(self.right) / z * self.focal;
        let y = d.dot(self.up) / z * self.focal;
        Some((pos2(self.center.x + x, self.center.y - y), z))
    }
}

#[derive(Clone)]
pub struct Tri {
    pub v: [V3; 3],
    pub color: Color32,
    /// Identifiant pour la sélection à la souris (0 = aucun).
    pub tag: u32,
    /// Visible des deux côtés (pas d'élimination des faces arrière).
    pub double: bool,
    /// Ombrage selon la lumière (sinon couleur pleine, ex. pour ce qui « brille »).
    pub lit: bool,
    /// Couche de rendu : 0 sol, 1 planchers, 2 objets (le tri par profondeur se fait dans chaque couche).
    pub layer: u8,
}

#[derive(Default)]
pub struct Scene {
    /// Couche des prochains triangles (voir `Tri::layer`).
    pub layer: u8,
    pub tris: Vec<Tri>,
    pub lines: Vec<(V3, V3, Stroke)>,
    /// Halos lumineux (position, rayon en mètres, couleur), dessinés par-dessus.
    pub glows: Vec<(V3, f32, Color32)>,
    /// Textes ancrés dans la scène.
    pub labels: Vec<(V3, String, FontId, Color32)>,
}

impl Scene {
    pub fn tri(&mut self, a: V3, b: V3, c: V3, color: Color32, tag: u32) {
        self.tris.push(Tri { v: [a, b, c], color, tag, double: false, lit: true, layer: self.layer });
    }

    /// Quadrilatère a-b-c-d (sens antihoraire vu de face).
    pub fn quad(&mut self, a: V3, b: V3, c: V3, d: V3, color: Color32, tag: u32) {
        self.tri(a, b, c, color, tag);
        self.tri(a, c, d, color, tag);
    }

    /// Pavé aligné sur les axes.
    pub fn cuboid(&mut self, lo: V3, hi: V3, color: Color32, tag: u32) {
        let (x0, y0, z0, x1, y1, z1) = (lo.x, lo.y, lo.z, hi.x, hi.y, hi.z);
        let p = |x, y, z| v3(x, y, z);
        self.quad(p(x0, y1, z0), p(x0, y1, z1), p(x1, y1, z1), p(x1, y1, z0), color, tag); // dessus
        self.quad(p(x0, y0, z1), p(x1, y0, z1), p(x1, y1, z1), p(x0, y1, z1), color, tag); // avant (+z)
        self.quad(p(x1, y0, z0), p(x0, y0, z0), p(x0, y1, z0), p(x1, y1, z0), color, tag); // arrière
        self.quad(p(x1, y0, z1), p(x1, y0, z0), p(x1, y1, z0), p(x1, y1, z1), color, tag); // droite
        self.quad(p(x0, y0, z0), p(x0, y0, z1), p(x0, y1, z1), p(x0, y1, z0), color, tag); // gauche
        self.quad(p(x0, y0, z0), p(x1, y0, z0), p(x1, y0, z1), p(x0, y0, z1), color, tag); // dessous
    }

    /// Sphère basse résolution (pour les appareils).
    pub fn sphere(&mut self, c: V3, r: f32, color: Color32, tag: u32, lit: bool) {
        let (rings, segs) = (6, 10);
        let pt = |i: usize, j: usize| {
            let th = std::f32::consts::PI * i as f32 / rings as f32;
            let ph = std::f32::consts::TAU * j as f32 / segs as f32;
            c + v3(th.sin() * ph.cos(), th.cos(), th.sin() * ph.sin()) * r
        };
        for i in 0..rings {
            for j in 0..segs {
                let (a, b, cc, d) = (pt(i, j), pt(i + 1, j), pt(i + 1, j + 1), pt(i, j + 1));
                for v in [[a, d, cc], [a, cc, b]] {
                    self.tris.push(Tri { v, color, tag, double: false, lit, layer: self.layer });
                }
            }
        }
    }

    /// Dessine la scène ; renvoie l'étiquette de l'objet sous la souris.
    pub fn render(&self, p: &Painter, view: &View, hover: Option<Pos2>) -> Option<u32> {
        let light = v3(-0.45, 0.85, 0.35).norm();
        let mut faces: Vec<(u8, f32, [Pos2; 3], Color32, u32)> = Vec::with_capacity(self.tris.len());
        for t in &self.tris {
            let n = (t.v[1] - t.v[0]).cross(t.v[2] - t.v[0]).norm();
            let facing = n.dot(view.eye - t.v[0]);
            if facing <= 0.0 && !t.double {
                continue;
            }
            let (Some(a), Some(b), Some(c)) = (view.project(t.v[0]), view.project(t.v[1]), view.project(t.v[2])) else { continue };
            let color = if t.lit {
                let n = if facing < 0.0 { n * -1.0 } else { n };
                let k = 0.58 + 0.42 * n.dot(light).max(0.0);
                shade(t.color, k)
            } else {
                t.color
            };
            faces.push((t.layer, (a.1 + b.1 + c.1) / 3.0, [a.0, b.0, c.0], color, t.tag));
        }
        // Algorithme du peintre, couche par couche : du plus lointain au plus proche.
        faces.sort_by(|x, y| x.0.cmp(&y.0).then(y.1.total_cmp(&x.1)));
        let mesh_of = |range: &[(u8, f32, [Pos2; 3], Color32, u32)]| {
            let mut mesh = Mesh::default();
            for (_, _, pts, color, _) in range {
                let i = mesh.vertices.len() as u32;
                for q in pts {
                    mesh.colored_vertex(*q, *color);
                }
                mesh.add_triangle(i, i + 1, i + 2);
            }
            mesh
        };
        // Sol et planchers, puis le quadrillage par-dessus, puis les objets.
        let split = faces.iter().position(|f| f.0 >= 2).unwrap_or(faces.len());
        p.add(Shape::mesh(mesh_of(&faces[..split])));
        for (a, b, stroke) in &self.lines {
            if let (Some(a), Some(b)) = (view.project(*a), view.project(*b)) {
                p.line_segment([a.0, b.0], *stroke);
            }
        }
        p.add(Shape::mesh(mesh_of(&faces[split..])));
        for (c, r, color) in &self.glows {
            if let Some((s, z)) = view.project(*c) {
                fx::glow(p, s, (r / z * view.focal).clamp(4.0, 400.0), *color);
            }
        }
        // Étiquettes : on place d'abord les plus proches, et on remonte celles qui en chevauchent une autre.
        let mut placed: Vec<(f32, Rect, std::sync::Arc<egui::Galley>, Color32)> = Vec::new();
        let mut items: Vec<_> = self.labels.iter().filter_map(|(at, text, font, color)| view.project(*at).map(|(s, z)| (z, s, text, font, color))).collect();
        items.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (z, s, text, font, color) in items {
            let g = p.layout_no_wrap(text.clone(), font.clone(), *color);
            let mut r = Rect::from_center_size(s, g.size() + vec2(10.0, 4.0));
            for _ in 0..4 {
                match placed.iter().find(|(_, o, _, _)| o.expand(2.0).intersects(r)) {
                    Some((_, o, _, _)) => r = r.translate(vec2(0.0, o.top() - r.bottom() - 3.0)),
                    None => break,
                }
            }
            placed.push((z, r, g, *color));
        }
        for (_, r, g, color) in placed.into_iter().rev() {
            p.rect_filled(r, 6.0, theme::alpha(theme::bg(), 0.78));
            p.galley(r.center() - g.size() / 2.0, g, color);
        }
        let pos = hover?;
        faces.iter().rev().find(|(_, _, pts, _, tag)| *tag != 0 && in_triangle(pos, pts)).map(|f| f.4)
    }
}

fn shade(c: Color32, k: f32) -> Color32 {
    let f = |x: u8| (x as f32 * k).clamp(0.0, 255.0) as u8;
    Color32::from_rgba_premultiplied(f(c.r()), f(c.g()), f(c.b()), c.a())
}

fn in_triangle(p: Pos2, t: &[Pos2; 3]) -> bool {
    let s = |a: Pos2, b: Pos2| (p.x - b.x) * (a.y - b.y) - (a.x - b.x) * (p.y - b.y);
    let (d1, d2, d3) = (s(t[0], t[1]), s(t[1], t[2]), s(t[2], t[0]));
    let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(neg && pos)
}

/// Zone 3D interactive : glisser pour tourner (avec inertie), molette pour zoomer.
/// Renvoie la réponse et l'objet survolé.
pub fn viewport(ui: &mut Ui, size: egui::Vec2, cam: &mut Camera, scene: &Scene) -> (Response, Option<u32>) {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let ctx = ui.ctx().clone();
    if resp.dragged() {
        let d = resp.drag_delta();
        cam.yaw -= d.x * 0.008;
        cam.pitch = (cam.pitch + d.y * 0.006).clamp(0.15, 1.45);
        cam.spin = -d.x * 0.008;
        ctx.set_cursor_icon(CursorIcon::Grabbing);
    } else if cam.spin.abs() > 0.0004 {
        cam.yaw += cam.spin;
        cam.spin *= 0.92;
        ctx.request_repaint();
    } else {
        cam.spin = 0.0;
    }
    if resp.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            // Le zoom ne doit pas faire défiler la page en même temps.
            ctx.input_mut(|i| i.smooth_scroll_delta.y = 0.0);
            cam.dist = (cam.dist * (-scroll * 0.0015).exp()).clamp(cam.min_dist, cam.max_dist);
        }
    }
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 16.0, theme::alpha(theme::bg(), 0.35));
    fx::glow(&p, rect.center() + vec2(0.0, rect.height() * 0.25), rect.width() * 0.6, theme::alpha(theme::accent(), 0.05));
    let view = View::new(cam, rect);
    let hover = scene.render(&p, &view, resp.hover_pos());
    p.text(
        rect.left_bottom() + vec2(14.0, -12.0),
        Align2::LEFT_BOTTOM,
        "Glisser pour tourner · molette pour zoomer",
        FontId::proportional(11.0),
        theme::faint(),
    );
    if hover.is_some() && !resp.dragged() {
        ctx.set_cursor_icon(CursorIcon::PointingHand);
    }
    (resp, hover)
}
