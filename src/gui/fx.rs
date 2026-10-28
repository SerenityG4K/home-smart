//! Effets visuels : dégradés, lueurs, ombres douces et animations.
//!
//! Les animations ne demandent un redessin que pendant qu'elles jouent :
//! au repos, l'interface ne consomme rien.

use super::theme;
use eframe::egui::epaint::{Mesh, Shadow};
use eframe::egui::{self, pos2, vec2, Color32, CornerRadius, Id, Painter, Pos2, Rect, Shape, Ui};
use std::f32::consts::TAU;
use std::time::Instant;

/// Lueur radiale douce (décroissance quasi gaussienne vers le transparent).
pub fn glow(p: &Painter, center: Pos2, radius: f32, color: Color32) {
    if radius <= 1.0 || color.a() == 0 {
        return;
    }
    let rings = [(0.0, 1.0), (0.35, 0.62), (0.65, 0.25), (1.0, 0.0)];
    let n = 40u32;
    let mut m = Mesh::default();
    for (r, k) in rings {
        for i in 0..n {
            let a = i as f32 / n as f32 * TAU;
            m.colored_vertex(center + vec2(a.cos(), a.sin()) * radius * r, color.gamma_multiply(k));
        }
    }
    for ring in 0..rings.len() as u32 - 1 {
        for i in 0..n {
            let (a, b) = (ring * n + i, ring * n + (i + 1) % n);
            let (c, d) = (a + n, b + n);
            m.add_triangle(a, b, d);
            m.add_triangle(a, d, c);
        }
    }
    p.add(Shape::mesh(m));
}

/// Fond de l'application : uni, avec un halo lagon très discret en haut à droite (thème sombre).
pub fn background(ctx: &egui::Context) {
    let rect = ctx.content_rect();
    let p = ctx.layer_painter(egui::LayerId::background());
    p.rect_filled(rect, 0.0, theme::bg());
    if !theme::is_light() {
        let r = rect.width().max(rect.height());
        glow(&p, pos2(rect.right() - rect.width() * 0.2, rect.top() - r * 0.08), r * 0.5, theme::alpha(theme::accent(), 0.05));
    }
}

/// Ombre portée douce sous un rectangle arrondi.
pub fn shadow(p: &Painter, rect: Rect, radius: u8, lift: f32) {
    let s = Shadow {
        offset: [0, (4.0 + lift * 6.0) as i8],
        blur: (14.0 + lift * 14.0) as u8,
        spread: 0,
        color: Color32::from_black_alpha((90.0 + lift * 50.0) as u8),
    };
    p.add(s.as_shape(rect, CornerRadius::same(radius)));
}

// ---- Animations -----------------------------------------------------------------------

/// Courbe « ease-out » cubique.
pub fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// Valeur animée vers `target` (interpolation douce, sans redessin au repos).
pub fn tween(ui: &Ui, id: impl std::hash::Hash + std::fmt::Debug, target: f32, secs: f32) -> f32 {
    ui.ctx().animate_value_with_time(Id::unique(id), target, secs)
}

/// Progression 0 → 1 depuis `since`, avec ease-out ; redemande un dessin tant que ce n'est pas fini.
pub fn progress(ctx: &egui::Context, since: Instant, secs: f32) -> f32 {
    let t = since.elapsed().as_secs_f32() / secs;
    if t < 1.0 {
        ctx.request_repaint();
    }
    ease_out(t)
}
