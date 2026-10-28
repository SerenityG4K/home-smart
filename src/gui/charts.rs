//! Graphiques dessinés au pinceau : courbe de puissance et barres de consommation.

use super::theme;
use super::widgets::fr;
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Rect, Sense, Shape, Stroke, Ui};

const AXIS_FONT: f32 = 10.5;

/// Maximum « rond » de l'axe et nombre de graduations (pas de 1, 2 ou 5 × 10ⁿ).
pub fn nice_axis(v: f64) -> (f64, usize) {
    if v <= 0.0 {
        return (1.0, 4);
    }
    let p = 10f64.powf(v.log10().floor());
    for (m, n) in [(1.0, 5), (2.0, 4), (2.5, 5), (4.0, 4), (5.0, 5), (8.0, 4), (10.0, 5)] {
        if v <= m * p + 1e-9 {
            return (m * p, n);
        }
    }
    (10.0 * p, 5)
}

fn frame(ui: &mut Ui, size: egui::Vec2) -> (Rect, Rect, egui::Response) {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let plot = Rect::from_min_max(rect.min + vec2(44.0, 8.0), rect.max - vec2(8.0, 22.0));
    (rect, plot, resp)
}

fn grid(ui: &Ui, plot: Rect, axis: (f64, usize), unit: &str) {
    let (max, n) = axis;
    let step = max / n as f64;
    let decimals = if step >= 1.0 { 0 } else if step >= 0.1 { 1 } else { 2 };
    let p = ui.painter();
    for i in 0..=n {
        let y = plot.bottom() - plot.height() * i as f32 / n as f32;
        p.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0, theme::line().gamma_multiply(if i == 0 { 1.4 } else { 0.7 })));
        let v = step * i as f64;
        let label = format!("{} {unit}", fr(v, decimals));
        p.text(pos2(plot.left() - 6.0, y), Align2::RIGHT_CENTER, label, FontId::proportional(AXIS_FONT), theme::faint());
    }
}

/// Courbe de puissance sur 24 h (une valeur par tranche de 5 min).
pub fn power_curve(ui: &mut Ui, values: &[f32], upto_slot: usize, width: f32, color: Color32) {
    let (_, plot, resp) = frame(ui, vec2(width, 170.0));
    let n = values.len().max(1);
    let shown = &values[..upto_slot.min(values.len())];
    let axis = nice_axis(shown.iter().cloned().fold(0.0f32, f32::max) as f64 * 1.05);
    let max = axis.0;
    grid(ui, plot, axis, "W");
    let p = ui.painter();
    for h in [0, 6, 12, 18, 24] {
        let x = plot.left() + plot.width() * h as f32 / 24.0;
        p.text(pos2(x, plot.bottom() + 4.0), Align2::CENTER_TOP, format!("{h} h"), FontId::proportional(AXIS_FONT), theme::faint());
    }
    let pt = |i: usize, v: f32| pos2(plot.left() + plot.width() * (i as f32 + 0.5) / n as f32, plot.bottom() - plot.height() * (v as f64 / max) as f32);
    if shown.len() >= 2 {
        let line: Vec<_> = shown.iter().enumerate().map(|(i, v)| pt(i, *v)).collect();
        // Remplissage léger sous la courbe, découpé en bandes verticales (forme non convexe).
        for w in line.windows(2) {
            let quad = vec![w[0], w[1], pos2(w[1].x, plot.bottom()), pos2(w[0].x, plot.bottom())];
            p.add(Shape::convex_polygon(quad, color.gamma_multiply(0.14), Stroke::NONE));
        }
        p.add(Shape::line(line, Stroke::new(1.8, color)));
    } else {
        p.text(plot.center(), Align2::CENTER_CENTER, "Pas encore de mesure aujourd'hui", FontId::proportional(12.0), theme::faint());
    }
    if let Some(pos) = resp.hover_pos().filter(|p| plot.contains(*p)) {
        let i = (((pos.x - plot.left()) / plot.width()) * n as f32).floor() as usize;
        if let Some(v) = shown.get(i) {
            let x = pt(i, *v).x;
            p.line_segment([pos2(x, plot.top()), pos2(x, plot.bottom())], Stroke::new(1.0, theme::muted().gamma_multiply(0.5)));
            p.circle_filled(pt(i, *v), 3.5, color);
            let mins = i * 5;
            resp.on_hover_text_at_pointer(format!("{:02}:{:02}  ·  {} W", mins / 60, mins % 60, fr(*v as f64, 0)));
        }
    }
}
