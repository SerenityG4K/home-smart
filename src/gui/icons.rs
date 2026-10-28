//! Icônes vectorielles dessinées au pinceau (aucune police requise).
//! Coordonnées exprimées sur une grille de 24 × 24.

use eframe::egui::{pos2, Color32, Painter, Pos2, Rect, Shape, Stroke};
use homesmart::kinds::Kind;
use std::f32::consts::PI;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Icon {
    Bulb,
    Plug,
    Strip,
    Blinds,
    Dial,
    House,
    Close,
    Camera,
    Shutter,
    Speaker,
    Muted,
    Play,
    Pause,
    Prev,
    Next,
    Download,
    Refresh,
    Film,
    Bolt,
    Bell,
    Clock,
    Gear,
    Plus,
    Cube,
    Dots,
    Chevron,
    Back,
    Shield,
    Bookmark,
    Expand,
    Person,
    Fall,
    Face,
    Check,
    Alert,
    Vacuum,
    Stop,
}

impl From<&Kind> for Icon {
    fn from(k: &Kind) -> Self {
        match k {
            Kind::LightV2 | Kind::LightV1 => Icon::Bulb,
            Kind::Plug { .. } => Icon::Plug,
            Kind::Switch { .. } => Icon::Strip,
            Kind::Cover => Icon::Blinds,
            Kind::Vacuum => Icon::Vacuum,
            Kind::Generic => Icon::Dial,
        }
    }
}

struct Pen<'a> {
    p: &'a Painter,
    rect: Rect,
    stroke: Stroke,
    color: Color32,
}

impl Pen<'_> {
    fn at(&self, x: f32, y: f32) -> Pos2 {
        pos2(self.rect.left() + x / 24.0 * self.rect.width(), self.rect.top() + y / 24.0 * self.rect.height())
    }
    fn u(&self, v: f32) -> f32 {
        v / 24.0 * self.rect.width()
    }
    fn line(&self, pts: &[(f32, f32)]) {
        let pts: Vec<Pos2> = pts.iter().map(|&(x, y)| self.at(x, y)).collect();
        self.p.add(Shape::line(pts, self.stroke));
    }
    fn closed(&self, pts: &[(f32, f32)]) {
        let pts: Vec<Pos2> = pts.iter().map(|&(x, y)| self.at(x, y)).collect();
        self.p.add(Shape::closed_line(pts, self.stroke));
    }
    fn circle(&self, x: f32, y: f32, r: f32) {
        self.p.circle_stroke(self.at(x, y), self.u(r), self.stroke);
    }
    fn dot(&self, x: f32, y: f32, r: f32) {
        self.p.circle_filled(self.at(x, y), self.u(r), self.color);
    }
    /// Arc de cercle, angles en degrés (0 = droite, sens horaire à l'écran).
    fn arc(&self, x: f32, y: f32, r: f32, from: f32, to: f32) {
        let n = 18;
        let pts: Vec<Pos2> = (0..=n)
            .map(|i| {
                let a = (from + (to - from) * i as f32 / n as f32) * PI / 180.0;
                self.at(x + r * a.cos(), y + r * a.sin())
            })
            .collect();
        self.p.add(Shape::line(pts, self.stroke));
    }
    fn rounded(&self, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) {
        let rect = Rect::from_min_max(self.at(x0, y0), self.at(x1, y1));
        self.p.rect_stroke(rect, self.u(r), self.stroke, eframe::egui::StrokeKind::Middle);
    }
    /// Polygone convexe plein.
    fn fill(&self, pts: &[(f32, f32)]) {
        let pts: Vec<Pos2> = pts.iter().map(|&(x, y)| self.at(x, y)).collect();
        self.p.add(Shape::convex_polygon(pts, self.color, Stroke::NONE));
    }
}

pub fn paint(p: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let width = (rect.width() / 24.0 * 1.9).max(1.2);
    let pen = Pen { p, rect, stroke: Stroke::new(width, color), color };
    match icon {
        Icon::Bulb => {
            pen.arc(12.0, 10.0, 6.0, 135.0, 405.0);
            pen.line(&[(7.8, 14.2), (9.5, 16.5), (14.5, 16.5), (16.2, 14.2)]);
            pen.line(&[(9.8, 19.3), (14.2, 19.3)]);
            pen.line(&[(10.8, 21.6), (13.2, 21.6)]);
        }
        Icon::Plug => {
            pen.line(&[(9.5, 3.5), (9.5, 8.0)]);
            pen.line(&[(14.5, 3.5), (14.5, 8.0)]);
            pen.closed(&[(6.5, 8.0), (17.5, 8.0), (17.5, 12.0), (14.0, 16.0), (10.0, 16.0), (6.5, 12.0)]);
            pen.line(&[(12.0, 16.0), (12.0, 21.0)]);
        }
        Icon::Strip => {
            pen.rounded(2.5, 8.0, 21.5, 16.0, 2.5);
            for x in [7.0, 12.0, 17.0] {
                pen.dot(x, 12.0, 1.5);
            }
        }
        Icon::Blinds => {
            pen.line(&[(3.5, 4.5), (20.5, 4.5)]);
            for y in [8.0, 11.5, 15.0] {
                pen.line(&[(5.0, y), (19.0, y)]);
            }
            pen.line(&[(12.0, 15.0), (12.0, 19.0)]);
            pen.dot(12.0, 20.0, 1.3);
        }
        Icon::Dial => {
            pen.circle(12.0, 12.0, 8.0);
            pen.line(&[(12.0, 12.0), (16.0, 8.0)]);
            pen.dot(12.0, 12.0, 1.6);
        }
        Icon::House => {
            pen.line(&[(3.5, 11.0), (12.0, 4.0), (20.5, 11.0)]);
            pen.line(&[(6.0, 9.5), (6.0, 20.0), (18.0, 20.0), (18.0, 9.5)]);
            pen.line(&[(10.0, 20.0), (10.0, 14.5), (14.0, 14.5), (14.0, 20.0)]);
        }
        Icon::Camera => {
            // Caméra motorisée : tête ronde avec objectif, sur son socle.
            pen.circle(12.0, 10.0, 7.0);
            pen.circle(12.0, 10.0, 3.0);
            pen.dot(12.0, 10.0, 1.1);
            pen.line(&[(8.5, 16.2), (7.0, 20.5), (17.0, 20.5), (15.5, 16.2)]);
        }
        Icon::Shutter => {
            // Appareil photo (capture).
            pen.rounded(3.0, 7.0, 21.0, 19.0, 2.5);
            pen.line(&[(8.0, 7.0), (9.5, 4.5), (14.5, 4.5), (16.0, 7.0)]);
            pen.circle(12.0, 13.0, 3.6);
        }
        Icon::Speaker => {
            pen.closed(&[(3.5, 9.0), (7.5, 9.0), (12.5, 4.5), (12.5, 19.5), (7.5, 15.0), (3.5, 15.0)]);
            pen.arc(12.5, 12.0, 4.0, -45.0, 45.0);
            pen.arc(12.5, 12.0, 7.5, -45.0, 45.0);
        }
        Icon::Muted => {
            pen.closed(&[(3.5, 9.0), (7.5, 9.0), (12.5, 4.5), (12.5, 19.5), (7.5, 15.0), (3.5, 15.0)]);
            pen.line(&[(15.5, 9.0), (21.0, 15.0)]);
            pen.line(&[(21.0, 9.0), (15.5, 15.0)]);
        }
        Icon::Close => {
            pen.line(&[(6.0, 6.0), (18.0, 18.0)]);
            pen.line(&[(18.0, 6.0), (6.0, 18.0)]);
        }
        Icon::Play => pen.fill(&[(7.0, 4.5), (19.5, 12.0), (7.0, 19.5)]),
        Icon::Pause => {
            pen.fill(&[(6.5, 4.5), (10.0, 4.5), (10.0, 19.5), (6.5, 19.5)]);
            pen.fill(&[(14.0, 4.5), (17.5, 4.5), (17.5, 19.5), (14.0, 19.5)]);
        }
        Icon::Prev => {
            pen.fill(&[(5.0, 5.0), (8.0, 5.0), (8.0, 19.0), (5.0, 19.0)]);
            pen.fill(&[(19.0, 5.0), (19.0, 19.0), (8.5, 12.0)]);
        }
        Icon::Next => {
            pen.fill(&[(16.0, 5.0), (19.0, 5.0), (19.0, 19.0), (16.0, 19.0)]);
            pen.fill(&[(5.0, 5.0), (15.5, 12.0), (5.0, 19.0)]);
        }
        Icon::Download => {
            pen.line(&[(12.0, 3.5), (12.0, 15.0)]);
            pen.line(&[(7.0, 10.5), (12.0, 15.5), (17.0, 10.5)]);
            pen.line(&[(4.5, 16.0), (4.5, 20.0), (19.5, 20.0), (19.5, 16.0)]);
        }
        Icon::Refresh => {
            pen.arc(12.0, 12.0, 7.5, -60.0, 230.0);
            pen.line(&[(15.5, 3.2), (16.0, 6.0), (13.2, 6.8)]);
        }
        Icon::Bolt => pen.closed(&[(13.0, 2.5), (5.0, 13.5), (11.0, 13.5), (10.0, 21.5), (18.5, 10.5), (12.5, 10.5)]),
        Icon::Bell => {
            pen.line(&[(6.0, 16.5), (6.0, 11.0)]);
            pen.arc(12.0, 11.0, 6.0, 180.0, 360.0);
            pen.line(&[(18.0, 11.0), (18.0, 16.5), (19.5, 18.0), (4.5, 18.0), (6.0, 16.5)]);
            pen.arc(12.0, 20.0, 2.0, 0.0, 180.0);
        }
        Icon::Clock => {
            pen.circle(12.0, 12.0, 8.5);
            pen.line(&[(12.0, 7.5), (12.0, 12.0), (15.0, 14.0)]);
        }
        Icon::Gear => {
            pen.circle(12.0, 12.0, 3.0);
            for k in 0..8 {
                let a = k as f32 * PI / 4.0;
                let (c, s) = (a.cos(), a.sin());
                pen.line(&[(12.0 + 6.0 * c, 12.0 + 6.0 * s), (12.0 + 9.0 * c, 12.0 + 9.0 * s)]);
            }
            pen.circle(12.0, 12.0, 6.0);
        }
        Icon::Plus => {
            pen.line(&[(12.0, 5.0), (12.0, 19.0)]);
            pen.line(&[(5.0, 12.0), (19.0, 12.0)]);
        }
        Icon::Cube => {
            pen.closed(&[(12.0, 3.0), (20.0, 7.5), (20.0, 16.5), (12.0, 21.0), (4.0, 16.5), (4.0, 7.5)]);
            pen.line(&[(4.0, 7.5), (12.0, 12.0), (20.0, 7.5)]);
            pen.line(&[(12.0, 12.0), (12.0, 21.0)]);
        }
        Icon::Dots => {
            for x in [6.0, 12.0, 18.0] {
                pen.dot(x, 12.0, 1.7);
            }
        }
        Icon::Chevron => pen.line(&[(9.0, 5.5), (15.5, 12.0), (9.0, 18.5)]),
        Icon::Back => pen.line(&[(15.0, 5.5), (8.5, 12.0), (15.0, 18.5)]),
        Icon::Shield => pen.closed(&[(12.0, 3.0), (19.0, 6.0), (19.0, 11.5), (17.0, 16.5), (12.0, 21.0), (7.0, 16.5), (5.0, 11.5), (5.0, 6.0)]),
        Icon::Bookmark => pen.closed(&[(7.0, 4.0), (17.0, 4.0), (17.0, 20.0), (12.0, 16.0), (7.0, 20.0)]),
        Icon::Expand => {
            pen.line(&[(4.0, 9.0), (4.0, 4.0), (9.0, 4.0)]);
            pen.line(&[(15.0, 4.0), (20.0, 4.0), (20.0, 9.0)]);
            pen.line(&[(4.0, 15.0), (4.0, 20.0), (9.0, 20.0)]);
            pen.line(&[(15.0, 20.0), (20.0, 20.0), (20.0, 15.0)]);
        }
        Icon::Person => {
            pen.circle(12.0, 7.5, 3.5);
            pen.arc(12.0, 20.5, 7.0, 180.0, 360.0);
        }
        Icon::Fall => {
            pen.circle(6.0, 12.5, 2.5);
            pen.line(&[(9.5, 14.0), (20.0, 11.5)]);
            pen.line(&[(3.0, 19.0), (21.0, 19.0)]);
        }
        Icon::Face => {
            pen.circle(12.0, 12.0, 8.5);
            pen.dot(9.0, 10.0, 1.2);
            pen.dot(15.0, 10.0, 1.2);
            pen.arc(12.0, 12.5, 3.5, 30.0, 150.0);
        }
        Icon::Check => pen.line(&[(5.0, 12.5), (9.5, 17.0), (19.0, 7.5)]),
        Icon::Vacuum => {
            // Robot vu de dessus : corps rond, tourelle du laser, pare-chocs à l'avant.
            pen.circle(12.0, 12.0, 8.5);
            pen.circle(12.0, 9.0, 2.4);
            pen.arc(12.0, 12.0, 5.8, 25.0, 155.0);
        }
        Icon::Stop => pen.fill(&[(7.0, 7.0), (17.0, 7.0), (17.0, 17.0), (7.0, 17.0)]),
        Icon::Alert => {
            pen.closed(&[(12.0, 3.5), (21.0, 19.5), (3.0, 19.5)]);
            pen.line(&[(12.0, 9.5), (12.0, 14.0)]);
            pen.dot(12.0, 16.8, 1.2);
        }
        Icon::Film => {
            pen.rounded(3.0, 5.0, 21.0, 19.0, 2.0);
            pen.line(&[(7.0, 5.0), (7.0, 19.0)]);
            pen.line(&[(17.0, 5.0), (17.0, 19.0)]);
            for y in [9.0, 15.0] {
                pen.line(&[(3.0, y), (7.0, y)]);
                pen.line(&[(17.0, y), (21.0, y)]);
            }
        }
    }
}
