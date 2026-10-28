//! Zoom numérique sur la vidéo d'une caméra (direct et relecture) : molette vers le point visé,
//! glisser pour se déplacer, double-clic ou « 1× » pour revenir à l'image entière.
//!
//! Un simple affichage : la caméra n'en sait rien et rien n'est journalisé (seul le passage en HD
//! qu'il déclenche l'est). Les calques (zones, détections) se placent comme sans zoom, sur le
//! rectangle qu'occuperait l'image entière (agrandi et décalé), découpé à la partie visible.

use super::super::{fx, theme, App};
use eframe::egui::{self, pos2, vec2, Align2, Color32, CornerRadius, CursorIcon, Id, Pos2, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2};
use homesmart::hub::{CameraView, Level, Source};
use std::time::{Duration, Instant};

/// Zoom maximal.
pub(super) const MAX: f32 = 8.0;
/// À partir de ce zoom, le direct passe en HD pour garder une image nette.
pub(super) const HD_FROM: f32 = 1.5;
/// Sensibilité de la molette : un cran (≈ 50 points) zoome d'environ 28 %.
const WHEEL: f32 = 0.005;

/// Gestes simulés sur le direct (« #zoom-gestes », captures d'écran) : molette vers un point,
/// glisser pour se déplacer, puis double-clic pour revenir à 1× (« #zoom-gestes+double »).
pub(in crate::gui) struct ZoomScript {
    start: Instant,
    step: usize,
    double: bool,
}

impl ZoomScript {
    pub fn new(double: bool) -> Self {
        ZoomScript { start: Instant::now(), step: 0, double }
    }
}

/// Cadrage : facteur et centre de la partie visible, en coordonnées normalisées de l'image (0 à 1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(in crate::gui) struct Zoom {
    pub factor: f32,
    pub center: Pos2,
}

impl Default for Zoom {
    fn default() -> Self {
        Zoom { factor: 1.0, center: pos2(0.5, 0.5) }
    }
}

impl Zoom {
    pub fn new(factor: f32, center: Pos2) -> Self {
        let mut z = Zoom { factor, center };
        z.clamp();
        z
    }

    pub fn is_zoomed(&self) -> bool {
        self.factor > 1.0 + 1e-3
    }

    /// Partie visible de l'image, en coordonnées normalisées.
    pub fn visible(&self) -> Rect {
        Rect::from_center_size(self.center, Vec2::splat(1.0 / self.factor))
    }

    /// Facteur dans ses bornes, partie visible gardée dans l'image.
    fn clamp(&mut self) {
        self.factor = self.factor.clamp(1.0, MAX);
        let half = 0.5 / self.factor;
        self.center = pos2(self.center.x.clamp(half, 1.0 - half), self.center.y.clamp(half, 1.0 - half));
    }

    /// Zoom multiplié par `k`, le point de l'image sous `at` (à l'écran) restant sous `at` ;
    /// `img` : place de l'image à l'écran.
    pub fn zoom_at(&mut self, k: f32, at: Pos2, img: Rect) {
        if img.width() <= 0.0 || img.height() <= 0.0 {
            return;
        }
        let v = self.visible();
        let t = ((at - img.min) / img.size()).clamp(Vec2::ZERO, Vec2::splat(1.0));
        let under = v.min + t * v.size();
        self.factor = (self.factor * k).clamp(1.0, MAX);
        // En dézoomant, presque 1× devient l'image entière, exactement.
        if k < 1.0 && self.factor < 1.01 {
            self.factor = 1.0;
        }
        let size = 1.0 / self.factor;
        self.center = under - t * size + Vec2::splat(size / 2.0);
        self.clamp();
    }

    /// L'image zoomée suit le pointeur (`delta` : déplacement à l'écran).
    pub fn pan(&mut self, delta: Vec2, img: Rect) {
        if img.width() <= 0.0 || img.height() <= 0.0 {
            return;
        }
        self.center -= delta / (img.size() * self.factor);
        self.clamp();
    }

    /// Rectangle qu'occupe l'image entière à l'écran, zoomée dans `img` (le dépasse une fois zoomée).
    pub fn full_rect(&self, img: Rect) -> Rect {
        let size = img.size() * self.factor;
        Rect::from_min_size(img.min - self.visible().min.to_vec2() * size, size)
    }

    /// « 2,5× », à la française.
    pub fn label(&self) -> String {
        let s = format!("{:.1}", self.factor);
        format!("{}×", s.strip_suffix(".0").unwrap_or(&s).replace('.', ","))
    }
}

impl App {
    /// Caméra ouverte : le zoom des autres est oublié ; le sien prend le flux HD au-delà de
    /// `HD_FROM` (s'il n'est pas déjà affiché) et le rend au retour à 1×, les deux passages au
    /// journal. Le bouton HD reprend la main.
    pub(in crate::gui) fn zoom_follow(&mut self, c: &CameraView) {
        let pb = format!("{}~pb", c.id);
        self.cams.zoom.retain(|k, _| *k == c.id || *k == pb);
        if let Some((_, on)) = self.cams.auto_hd.take_if(|(id, _)| *id != c.id) {
            if on {
                self.cams.hd = false;
            }
        }
        let z = self.cams.zoom.get(&c.id).copied().unwrap_or_default();
        let auto = self.cams.auto_hd.as_ref().map(|(_, on)| *on);
        if !z.is_zoomed() {
            if auto == Some(true) {
                self.cams.hd = false;
                self.hub.log(Level::Info, Source::User, &c.id, "Retour au flux fluide : zoom revenu à l'image entière");
            }
            self.cams.auto_hd = None;
        } else if auto == Some(true) && c.main_unavailable {
            // Flux HD illisible ici (H.265 sans FFmpeg) : le zoom n'insiste pas.
            self.cams.hd = false;
            self.cams.auto_hd = Some((c.id.clone(), false));
        } else if auto.is_none() && !self.cams.hd && !c.main_unavailable && z.factor >= HD_FROM {
            self.cams.hd = true;
            self.cams.auto_hd = Some((c.id.clone(), true));
            self.hub.log(Level::Info, Source::User, &c.id, format!("Flux HD demandé automatiquement (zoom {}) pour garder une image nette", z.label()));
        }
    }

    /// Gestes de zoom sur une vidéo (`resp` ; `img` : place de l'image à l'écran), cadrage rangé
    /// sous `key` : molette ou pincement vers le pointeur, glisser pour se déplacer une fois
    /// zoomé, double-clic pour revenir à l'image entière (si `double`). Renvoie le cadrage, et si
    /// le double-clic a servi.
    pub(in crate::gui) fn zoom_gestures(&mut self, ui: &Ui, resp: &Response, key: &str, img: Rect, double: bool) -> (Zoom, bool) {
        let before = self.cams.zoom.get(key).copied().unwrap_or_default();
        let mut z = before;
        if resp.hovered() {
            let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
            let k = pinch * (scroll * WHEEL).exp();
            if (k - 1.0).abs() > 1e-4 {
                if let Some(at) = resp.hover_pos() {
                    z.zoom_at(k, at, img);
                }
                // La molette zoome au lieu de faire défiler la page, sauf pour dézoomer à 1×.
                if before.is_zoomed() || k > 1.0 {
                    ui.ctx().input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
                }
            }
        }
        if z.is_zoomed() && resp.dragged_by(egui::PointerButton::Primary) {
            z.pan(resp.drag_delta(), img);
        }
        let reset = double && z.is_zoomed() && resp.double_clicked();
        if reset {
            z = Zoom::default();
        }
        if z.is_zoomed() && (resp.hovered() || resp.dragged()) {
            ui.ctx().set_cursor_icon(if resp.dragged() { CursorIcon::Grabbing } else { CursorIcon::Grab });
        }
        if z != before {
            if z.is_zoomed() {
                self.cams.zoom.insert(key.to_string(), z);
            } else {
                self.cams.zoom.remove(key);
            }
            ui.ctx().request_repaint();
        }
        (z, reset)
    }

    /// Gestes simulés (voir `ZoomScript`), une étape par image à partir de 2 s, sur l'image du
    /// direct telle qu'affichée à l'image précédente.
    pub(in crate::gui) fn zoom_script_input(&mut self, raw: &mut egui::RawInput) {
        let Some(img) = self.cams.zoom_area else { return };
        let Some(s) = self.cams.zoom_script.as_mut() else { return };
        if s.start.elapsed() < Duration::from_secs(2) {
            return;
        }
        let modifiers = egui::Modifiers::default();
        let at = img.lerp_inside(vec2(0.3, 0.4));
        let to = at + vec2(-160.0, -60.0);
        let button = |pos: Pos2, pressed: bool| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers };
        let wheel = egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Line, delta: vec2(0.0, 1.0), phase: egui::TouchPhase::Move, source: egui::MouseWheelSource::Wheel, modifiers };
        match s.step {
            // Trois crans de molette, le pointeur posé sur l'image.
            0 => raw.events.extend([egui::Event::PointerMoved(at), wheel.clone(), wheel.clone(), wheel]),
            // Le zoom se fait en douceur, puis on attrape l'image et on la tire.
            30 => raw.events.push(button(at, true)),
            31..=38 => raw.events.push(egui::Event::PointerMoved(at + (to - at) * (s.step - 30) as f32 / 8.0)),
            39 => raw.events.push(button(to, false)),
            60 | 62 if s.double => raw.events.push(button(to, true)),
            61 | 63 if s.double => raw.events.push(button(to, false)),
            _ if s.step > 63 => return,
            _ => {}
        }
        s.step += 1;
    }

    /// Indicateur discret en haut à gauche de l'image (`img`), une fois zoomé : la partie montrée
    /// sur une mini-carte, le facteur, et « 1× » pour revenir à l'image entière.
    pub(in crate::gui) fn zoom_badge(&mut self, ui: &Ui, key: &str, img: Rect) {
        let z = self.cams.zoom.get(key).copied().unwrap_or_default();
        let t = fx::tween(ui, ("zoom-badge", key), z.is_zoomed() as i32 as f32, 0.15);
        if !z.is_zoomed() || img.width() < 220.0 || img.height() < 120.0 {
            return;
        }
        let fade = |c: Color32| c.gamma_multiply(t);
        let p = ui.painter();
        let galley = p.layout_no_wrap(z.label(), theme::bold(13.0), Color32::WHITE);
        let map = vec2(24.0 * (img.width() / img.height()).clamp(0.75, 2.4), 24.0);
        let btn = vec2(30.0, 24.0);
        let pill = Rect::from_min_size(img.min + vec2(12.0, 12.0), vec2(10.0 + map.x + 9.0 + galley.size().x + 9.0 + btn.x + 6.0, 36.0));
        p.rect_filled(pill, CornerRadius::same(255), fade(Color32::from_rgba_unmultiplied(10, 13, 19, 185)));
        // Mini-carte : l'image entière, et la partie montrée.
        let m = Rect::from_min_size(pos2(pill.left() + 10.0, pill.center().y - map.y / 2.0), map);
        p.rect_filled(m, CornerRadius::same(3), fade(Color32::from_white_alpha(18)));
        p.rect_stroke(m, CornerRadius::same(3), Stroke::new(1.0, fade(Color32::from_white_alpha(90))), StrokeKind::Inside);
        let v = z.visible();
        let part = Rect::from_min_max(m.min + v.min.to_vec2() * m.size(), m.min + v.max.to_vec2() * m.size());
        p.rect_filled(part, CornerRadius::same(2), theme::alpha(theme::accent(), 0.5 * t));
        p.rect_stroke(part, CornerRadius::same(2), Stroke::new(1.0, fade(theme::accent())), StrokeKind::Inside);
        let text_y = pill.center().y - galley.size().y / 2.0;
        p.galley_with_override_text_color(pos2(m.right() + 9.0, text_y), galley, fade(Color32::WHITE));
        // Retour à l'image entière.
        let b = Rect::from_min_size(pos2(pill.right() - 6.0 - btn.x, pill.center().y - btn.y / 2.0), btn);
        let resp = ui.interact(b, Id::unique(("zoom-reset", key)), Sense::click());
        let h = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
        p.rect_filled(b, CornerRadius::same(12), fade(Color32::from_white_alpha((30.0 + 40.0 * h) as u8)));
        p.text(b.center(), Align2::CENTER_CENTER, "1×", theme::bold(12.0), fade(Color32::WHITE));
        if resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        if resp.on_hover_text("Revenir à l'image entière (double-clic sur l'image)").clicked() {
            self.cams.zoom.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMG: Rect = Rect { min: pos2(100.0, 50.0), max: pos2(900.0, 500.0) };

    fn close(a: Pos2, b: Pos2) -> bool {
        a.distance(b) < 1e-3
    }

    /// Le point de l'image sous le pointeur, à l'écran, pour ce cadrage.
    fn under(z: &Zoom, at: Pos2) -> Pos2 {
        let full = z.full_rect(IMG);
        pos2((at.x - full.left()) / full.width(), (at.y - full.top()) / full.height())
    }

    #[test]
    fn zoom_keeps_the_point_under_the_pointer() {
        let mut z = Zoom::default();
        let at = pos2(300.0, 200.0);
        let target = under(&z, at);
        z.zoom_at(2.0, at, IMG);
        assert!((z.factor - 2.0).abs() < 1e-5);
        assert!(close(under(&z, at), target), "{:?} ≠ {target:?}", under(&z, at));
        // Encore, ailleurs : le nouveau point visé reste en place lui aussi.
        let at2 = pos2(650.0, 420.0);
        let target2 = under(&z, at2);
        z.zoom_at(1.5, at2, IMG);
        assert!(close(under(&z, at2), target2));
        // La partie visible occupe exactement l'image à l'écran.
        let v = z.visible();
        let full = z.full_rect(IMG);
        assert!(close(full.min + v.min.to_vec2() * full.size(), IMG.min));
        assert!(close(full.min + v.max.to_vec2() * full.size(), IMG.max));
    }

    #[test]
    fn stays_inside_the_image_and_its_limits() {
        // Zoom dans un coin : la partie visible ne sort pas de l'image.
        let mut z = Zoom::default();
        z.zoom_at(3.0, IMG.min, IMG);
        let v = z.visible();
        assert!(v.min.x >= -1e-6 && v.min.y >= -1e-6 && close(v.min, pos2(0.0, 0.0)));
        // Glisser bien au-delà du bord droit : collé au bord, sans le dépasser.
        z.pan(vec2(-10_000.0, -10_000.0), IMG);
        let v = z.visible();
        assert!(close(v.max, pos2(1.0, 1.0)), "{v:?}");
        // Facteur borné.
        z.zoom_at(100.0, IMG.center(), IMG);
        assert_eq!(z.factor, MAX);
        assert!((Zoom::new(0.2, pos2(0.9, 0.1)).factor - 1.0).abs() < 1e-6);
        // Rien à déplacer à 1×.
        let mut one = Zoom::default();
        one.pan(vec2(300.0, -200.0), IMG);
        assert_eq!(one, Zoom::default());
    }

    #[test]
    fn back_to_the_whole_image() {
        let mut z = Zoom::default();
        z.zoom_at(2.5, pos2(800.0, 120.0), IMG);
        assert!(z.is_zoomed());
        // En dézoomant autant qu'on a zoomé, on retrouve l'image entière, centrée.
        z.zoom_at(1.0 / 2.5, pos2(200.0, 400.0), IMG);
        assert!(!z.is_zoomed());
        assert_eq!(z.factor, 1.0);
        assert!(close(z.center, pos2(0.5, 0.5)));
        assert_eq!(z.full_rect(IMG), IMG);
        // Presque 1× en dézoomant : exactement 1×.
        let mut z = Zoom::new(1.012, pos2(0.5, 0.5));
        z.zoom_at(0.995, IMG.center(), IMG);
        assert_eq!(z.factor, 1.0);
        // Un petit pas de molette depuis 1× zoome bien.
        let mut z = Zoom::default();
        z.zoom_at(1.004, IMG.center(), IMG);
        assert!(z.is_zoomed());
        assert_eq!(Zoom::new(2.0, pos2(0.5, 0.5)).label(), "2×");
        assert_eq!(Zoom::new(2.46, pos2(0.5, 0.5)).label(), "2,5×");
    }
}
