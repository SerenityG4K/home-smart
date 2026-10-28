//! Zones de la vision IA (porte, couloir, lit…), dessinées directement sur la vidéo.
//!
//! « Ajouter une zone » passe la vidéo en mode dessin : chaque clic pose un sommet ; un double-clic,
//! un clic sur le premier sommet, Entrée ou « Terminer » ferme le polygone ; Retour arrière retire
//! le dernier sommet ; Échap annule. On donne ensuite un nom et le délai de présence prolongée, et
//! la zone rejoint les réglages d'analyse de la caméra (journalisé). Coordonnées normalisées (0…1) :
//! une zone garde son sens en fluide comme en HD.

use super::super::{theme, widgets, App};
use eframe::egui::{self, pos2, vec2, Color32, CornerRadius, Painter, Pos2, Rect, Response, RichText, Shape, Stroke, Ui};
use homesmart::hub::{CameraView, Source};
use homesmart::kinds;
use homesmart::vision::actions::{same_name, Zone};
use homesmart::vision::geom::{Point, Polygon};
use homesmart::vision::CameraVision;
use std::time::Duration;

/// Couleur des zones.
const ZONE: Color32 = theme::VIOLET;
/// Écart minimal entre deux sommets (fraction de l'image) : un double-clic n'en pose pas deux.
const MIN_GAP: f32 = 0.004;
/// Rayon (pixels) autour du premier sommet où un clic ferme le polygone.
const CLOSE_PX: f32 = 10.0;

#[derive(Default)]
pub(in crate::gui) struct ZonesUi {
    draft: Option<Draft>,
    /// Zone en cours de modification : (caméra, nom actuel, nom saisi, délai saisi).
    edit: Option<(String, String, String, u32)>,
    /// Suppression à confirmer : (caméra, nom).
    confirm_delete: Option<(String, String)>,
    /// Caméra dont la liste des zones est dépliée (ses zones restent dessinées sans analyse).
    list_open: Option<String>,
}

/// Zone en cours de dessin.
struct Draft {
    camera: String,
    points: Vec<Point>,
    /// Polygone fermé : reste à lui donner un nom.
    closed: bool,
    name: String,
    linger: u32,
    /// Donner le focus au champ du nom (une fois).
    focus: bool,
}

impl Draft {
    fn new(camera: &str) -> Self {
        Draft { camera: camera.to_string(), points: Vec::new(), closed: false, name: String::new(), linger: 0, focus: false }
    }

    /// Ferme le polygone s'il peut servir de zone.
    fn finish(&mut self) -> bool {
        if Polygon(self.points.clone()).problem().is_some() {
            return false;
        }
        (self.closed, self.focus) = (true, true);
        true
    }
}

impl ZonesUi {
    /// Une zone est en train d'être dessinée (sommets) sur cette caméra.
    pub fn drawing(&self, camera: &str) -> bool {
        self.draft.as_ref().is_some_and(|d| d.camera == camera && !d.closed)
    }

    /// Une nouvelle zone est en cours sur cette caméra (dessin ou nom).
    pub fn editing(&self, camera: &str) -> bool {
        self.draft.as_ref().is_some_and(|d| d.camera == camera)
    }

    /// Abandonne la zone en cours sur cette caméra (dessin, nom, modification) ; vrai s'il y en avait une.
    pub fn cancel(&mut self, camera: &str) -> bool {
        let draft = self.draft.take_if(|d| d.camera == camera).is_some();
        let edit = self.edit.take_if(|e| e.0 == camera).is_some();
        draft || edit
    }
}

/// Modification de la liste des zones d'une caméra.
#[derive(Debug, PartialEq)]
enum Change {
    Add(Zone),
    /// (nom actuel, nouveau nom, délai de présence prolongée).
    Update(String, String, u32),
    Remove(String),
}

/// Zones après la modification.
fn apply(zones: &[Zone], change: &Change) -> Vec<Zone> {
    let mut out = zones.to_vec();
    match change {
        Change::Add(z) => out.push(z.clone()),
        Change::Update(old, name, linger) => {
            if let Some(z) = out.iter_mut().find(|z| &z.name == old) {
                (z.name, z.linger_secs) = (name.trim().to_string(), *linger);
            }
        }
        Change::Remove(name) => out.retain(|z| &z.name != name),
    }
    out
}

/// Position à l'écran → coordonnées normalisées dans l'image `img` ; un clic sur les bandes
/// noires se colle au bord le plus proche.
fn to_image(pos: Pos2, img: Rect) -> Point {
    let f = |v: f32, start: f32, len: f32| if len > 0.0 { ((v - start) / len).clamp(0.0, 1.0) } else { 0.0 };
    Point::new(f(pos.x, img.left(), img.width()), f(pos.y, img.top(), img.height()))
}

/// Coordonnées normalisées → position à l'écran.
fn to_screen(p: Point, img: Rect) -> Pos2 {
    pos2(img.left() + p.x * img.width(), img.top() + p.y * img.height())
}

/// Ajoute un sommet, sauf s'il retombe sur le précédent.
fn push_vertex(points: &mut Vec<Point>, p: Point) -> bool {
    if points.last().is_some_and(|q| q.dist(p) < MIN_GAP) {
        return false;
    }
    points.push(p);
    true
}

/// Le pointeur (à l'écran) est-il sur le premier sommet, de quoi fermer le polygone ?
fn on_first(points: &[Point], pos: Pos2, img: Rect) -> bool {
    points.len() >= 3 && to_screen(points[0], img).distance(pos) <= CLOSE_PX
}

/// Nom refusé pour une zone de cette caméra (`except` : la zone qu'on renomme).
fn name_problem(name: &str, zones: &[Zone], except: Option<&str>) -> Option<&'static str> {
    if name.trim().is_empty() {
        return Some("donne un nom à la zone");
    }
    if zones.iter().any(|z| Some(z.name.as_str()) != except && same_name(&z.name, name)) {
        return Some("une autre zone porte déjà ce nom");
    }
    None
}

/// Délai de présence prolongée, en clair.
fn linger_label(secs: u32) -> String {
    if secs == 0 {
        "sans alerte de durée".to_string()
    } else {
        format!("alerte après {}", kinds::fmt_secs(secs as u64))
    }
}

/// Sens de rotation de (a, b, c) : positif d'un côté, négatif de l'autre, nul si alignés.
fn turn(a: Point, b: Point, c: Point) -> f32 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

/// Triangles d'un polygone simple, par découpage en oreilles : egui ne remplit que les formes
/// convexes, or une zone peut être en L.
fn triangulate(pts: &[Point]) -> Vec<[usize; 3]> {
    let n = pts.len();
    // Sens du polygone : une oreille tourne dans le même sens que lui.
    let sign = (0..n).map(|i| pts[i].x * pts[(i + 1) % n].y - pts[(i + 1) % n].x * pts[i].y).sum::<f32>().signum();
    let inside = |p: Point, a: Point, b: Point, c: Point| {
        let (d1, d2, d3) = (turn(a, b, p), turn(b, c, p), turn(c, a, p));
        !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
    };
    let mut left: Vec<usize> = (0..n).collect();
    let mut out = Vec::with_capacity(n.saturating_sub(2));
    while left.len() > 3 {
        let m = left.len();
        let ear = (0..m).find_map(|i| {
            let (a, b, c) = (left[(i + m - 1) % m], left[i], left[(i + 1) % m]);
            let t = turn(pts[a], pts[b], pts[c]) * sign;
            if t <= 0.0 {
                // Sommet rentrant ; aligné, il s'ôte sans triangle.
                return (t == 0.0).then_some((i, None));
            }
            let blocked = left.iter().any(|&k| k != a && k != b && k != c && inside(pts[k], pts[a], pts[b], pts[c]));
            (!blocked).then_some((i, Some([a, b, c])))
        });
        let Some((i, tri)) = ear else { break };
        out.extend(tri);
        left.remove(i);
    }
    if left.len() == 3 {
        out.push([left[0], left[1], left[2]]);
    }
    out
}

/// Contour, remplissage léger et nom d'une zone ; `strong` : quelqu'un s'y tient (ou zone en cours).
fn paint_zone(p: &Painter, img: Rect, pts: &[Point], name: &str, strong: bool) {
    let screen: Vec<Pos2> = pts.iter().map(|q| to_screen(*q, img)).collect();
    let mut mesh = egui::Mesh::default();
    let fill = theme::alpha(ZONE, if strong { 0.32 } else { 0.17 });
    for s in &screen {
        mesh.colored_vertex(*s, fill);
    }
    for [a, b, c] in triangulate(pts) {
        mesh.add_triangle(a as u32, b as u32, c as u32);
    }
    p.add(Shape::mesh(mesh));
    p.add(Shape::closed_line(screen.clone(), Stroke::new(if strong { 2.5 } else { 2.0 }, theme::alpha(ZONE, 0.95))));
    let n = screen.len().max(1) as f32;
    let center = pos2(screen.iter().map(|s| s.x).sum::<f32>() / n, screen.iter().map(|s| s.y).sum::<f32>() / n);
    label(p, img, center, name, theme::alpha(ZONE, 0.88), Color32::WHITE);
}

/// Pastille de texte centrée sur `at`, gardée dans l'image.
fn label(p: &Painter, img: Rect, at: Pos2, text: &str, bg: Color32, fg: Color32) {
    let g = p.layout(text.to_string(), theme::bold(11.5), fg, (img.width() - 24.0).max(60.0));
    let r = Rect::from_center_size(at, g.size() + vec2(14.0, 8.0));
    let shift = vec2((img.left() - r.left()).max(0.0) + (img.right() - r.right()).min(0.0), (img.top() - r.top()).max(0.0) + (img.bottom() - r.bottom()).min(0.0));
    let r = r.translate(shift);
    p.rect_filled(r, CornerRadius::same(7), bg);
    p.galley(r.min + vec2(7.0, 4.0), g, fg);
}

impl App {
    /// Zones de la caméra sur la vidéo : pendant leur modification, et sinon quand l'analyse est
    /// active ou que la liste des zones est dépliée (sauf « Masquer les cadres et les zones »).
    pub(in crate::gui) fn zones_overlay(&self, p: &Painter, img: Rect, c: &CameraView) {
        let z = &self.cams.zones;
        let editing = z.editing(&c.id) || z.edit.as_ref().is_some_and(|e| e.0 == c.id);
        let cv = self.hub.camera_vision(&c.id);
        let listed = self.cams.selected.as_deref() == Some(c.id.as_str()) && z.list_open.as_deref() == Some(c.id.as_str());
        if cv.zones.is_empty() || (!editing && (self.cams.vision.hide_overlay || !(cv.enabled || listed))) {
            return;
        }
        // Zone occupée : le point d'appui au sol d'une personne y est (comme pour le moteur d'actions).
        let feet: Vec<Point> = self
            .hub
            .camera_analysis(&c.id)
            .filter(|a| a.running && a.updated.is_some_and(|t| t.elapsed() < Duration::from_secs(2)))
            .map(|a| a.persons.iter().map(|p| p.bbox.foot()).collect())
            .unwrap_or_default();
        for zone in &cv.zones {
            let edited = z.edit.as_ref().is_some_and(|e| e.0 == c.id && e.1 == zone.name);
            paint_zone(p, img, &zone.area.0, &zone.name, edited || feet.iter().any(|f| zone.area.contains(*f)));
        }
    }

    /// Mode dessin sur la vidéo (`resp` : sa réponse aux clics) : sommets, trait vers le pointeur,
    /// consigne ; puis la zone fermée, le temps de la nommer.
    pub(in crate::gui) fn zone_draft(&mut self, ui: &Ui, p: &Painter, img: Rect, resp: &Response, c: &CameraView) {
        let detail = self.cams.selected.as_deref() == Some(c.id.as_str());
        let Some(d) = self.cams.zones.draft.as_mut().filter(|d| d.camera == c.id && detail) else { return };
        let hover = resp.hover_pos().map(|h| to_screen(to_image(h, img), img));
        if !d.closed {
            if resp.double_clicked() {
                // Le premier clic du double-clic a déjà posé le dernier sommet.
                d.finish();
            } else if let Some(pos) = resp.interact_pointer_pos().filter(|_| resp.clicked()) {
                if on_first(&d.points, pos, img) {
                    d.finish();
                } else {
                    push_vertex(&mut d.points, to_image(pos, img));
                }
            }
            if ui.ctx().memory(|m| m.focused().is_none()) {
                let (back, enter) = ui.input(|i| (i.key_pressed(egui::Key::Backspace), i.key_pressed(egui::Key::Enter)));
                if back {
                    d.points.pop();
                }
                if enter {
                    d.finish();
                }
            }
        }
        let problem = Polygon(d.points.clone()).problem();
        if d.closed {
            let name = if d.name.trim().is_empty() { "Nouvelle zone" } else { d.name.trim() };
            paint_zone(p, img, &d.points, name, true);
        } else {
            let pts: Vec<Pos2> = d.points.iter().map(|q| to_screen(*q, img)).collect();
            if problem.is_none() {
                paint_zone(p, img, &d.points, "Nouvelle zone", false);
            } else if pts.len() >= 2 {
                p.add(Shape::line(pts.clone(), Stroke::new(2.0, ZONE)));
            }
            // Trait vers le pointeur, et de retour au premier sommet.
            if let (Some(last), Some(h)) = (pts.last(), hover) {
                p.extend(Shape::dashed_line(&[*last, h], Stroke::new(1.5, theme::alpha(Color32::WHITE, 0.85)), 6.0, 4.0));
                if pts.len() >= 2 {
                    p.extend(Shape::dashed_line(&[h, pts[0]], Stroke::new(1.0, theme::alpha(ZONE, 0.7)), 4.0, 4.0));
                }
            }
            let closing = hover.is_some_and(|h| on_first(&d.points, h, img));
            for (i, s) in pts.iter().enumerate() {
                let r = if i == 0 && closing { 7.5 } else { 4.5 };
                p.circle_filled(*s, r, if i == 0 && closing { ZONE } else { Color32::WHITE });
                p.circle_stroke(*s, r, Stroke::new(2.0, ZONE));
            }
        }
        // Consigne en bas de la partie visible de l'image (une fois zoomée, `img` la dépasse), ou en
        // haut (sous l'horodatage et l'indicateur de zoom) quand on dessine en bas.
        let view = img.intersect(p.clip_rect());
        let near = hover.or_else(|| d.points.last().map(|q| to_screen(*q, img)));
        let y = if near.is_some_and(|h| h.y > view.top() + view.height() * 0.66) { view.top() + 62.0 } else { view.bottom() - 22.0 };
        let n = d.points.len();
        let (tip, color) = match (d.closed, problem) {
            (true, _) => ("Donne un nom à la zone dans la colonne de droite, puis « Enregistrer ».".to_string(), Color32::WHITE),
            (false, _) if n < 3 => (format!("Clique pour poser les sommets ({n} sur 3 au moins) · Échap : annuler"), Color32::WHITE),
            (false, Some(pb)) => (format!("Zone impossible : {pb} · Retour arrière : retirer le dernier point"), theme::warn()),
            (false, None) => ("Double-clic, Entrée ou clic sur le premier point : terminer · Retour arrière : dernier point · Échap : annuler".to_string(), Color32::WHITE),
        };
        label(p, view, pos2(view.center().x, y), &tip, Color32::from_black_alpha(175), color);
    }

    /// Liste « Zones » de la section Vision IA : zones de la caméra, ajout par dessin sur la
    /// vidéo, renommage (avec le délai), suppression.
    pub(in crate::gui) fn zones_list(&mut self, ui: &mut Ui, c: &CameraView) {
        let cv = self.hub.camera_vision(&c.id);
        let zones = cv.zones.clone();
        let has_image = self.hub.camera_frame(&c.id).is_some();
        let mut change = None;
        let z = &mut self.cams.zones;
        let header = egui::CollapsingHeader::new(RichText::new(format!("Zones ({})", zones.len())).size(13.0))
            .id_salt(("vision-zones", &c.id))
            .default_open(true)
            .show(ui, |ui| {
                if zones.is_empty() && !z.editing(&c.id) {
                    ui.label(
                        RichText::new("Dessine sur la vidéo les endroits à surveiller (porte, couloir, lit…) : entrées, sorties et présences prolongées iront au journal.")
                            .size(12.0)
                            .color(theme::muted()),
                    );
                }
                for zone in &zones {
                    ui.horizontal_wrapped(|ui| match z.edit.as_mut().filter(|e| e.0 == c.id && e.1 == zone.name) {
                        Some((_, _, name, linger)) => {
                            let r = ui.add(egui::TextEdit::singleline(name).desired_width(110.0));
                            ui.add(egui::DragValue::new(linger).range(0..=86_400).speed(2.0).suffix(" s")).on_hover_text("Présence prolongée signalée au-delà de ce délai ; 0 = jamais");
                            let problem = name_problem(name, &zones, Some(&zone.name));
                            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            if (ui.add_enabled(problem.is_none(), egui::Button::new("OK").small()).clicked() || enter) && problem.is_none() {
                                change = Some(Change::Update(zone.name.clone(), name.clone(), *linger));
                            }
                            if ui.small_button("Annuler").clicked() {
                                z.edit = None;
                            } else if let Some(pb) = problem {
                                ui.label(RichText::new(pb).size(11.5).color(theme::warn()));
                            }
                        }
                        None => {
                            widgets::dot(ui, ZONE);
                            ui.label(RichText::new(&zone.name).size(13.0));
                            ui.label(RichText::new(linger_label(zone.linger_secs)).size(11.5).color(theme::faint()));
                            if ui.small_button("Renommer").on_hover_text("Nom et délai de présence prolongée").clicked() {
                                z.edit = Some((c.id.clone(), zone.name.clone(), zone.name.clone(), zone.linger_secs));
                            }
                            if z.confirm_delete.as_ref().is_some_and(|(cam, n)| cam == &c.id && n == &zone.name) {
                                if ui.add(egui::Button::new(RichText::new("Supprimer vraiment").color(theme::crit())).small()).clicked() {
                                    change = Some(Change::Remove(zone.name.clone()));
                                }
                            } else if ui.small_button("Supprimer").clicked() {
                                z.confirm_delete = Some((c.id.clone(), zone.name.clone()));
                            }
                        }
                    });
                }
                ui.add_space(4.0);
                let mut cancel = false;
                match z.draft.as_mut().filter(|d| d.camera == c.id) {
                    // Dessin en cours : les sommets se posent sur la vidéo.
                    Some(d) if !d.closed => {
                        let n = d.points.len();
                        ui.label(RichText::new(format!("Nouvelle zone : {n} point{}", if n > 1 { "s" } else { "" })).size(12.5));
                        ui.label(
                            RichText::new("Clique sur la vidéo pour poser les sommets. Double-clic, Entrée ou clic sur le premier point pour fermer ; Retour arrière retire le dernier point ; Échap annule.")
                                .size(11.5)
                                .color(theme::faint()),
                        );
                        let problem = Polygon(d.points.clone()).problem();
                        if let Some(pb) = problem.filter(|_| n >= 3) {
                            ui.label(RichText::new(format!("Zone impossible : {pb}.")).size(12.0).color(theme::warn()));
                        }
                        ui.horizontal_wrapped(|ui| {
                            if ui.add_enabled(problem.is_none(), widgets::primary_button("Terminer", ZONE)).clicked() {
                                d.finish();
                            }
                            if ui.add_enabled(n > 0, egui::Button::new("Retirer le dernier point")).clicked() {
                                d.points.pop();
                            }
                            cancel = ui.button("Annuler").clicked();
                        });
                    }
                    // Polygone fermé : nom et délai.
                    Some(d) => {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new("Nom").size(12.5).color(theme::muted()));
                            let r = ui.add(egui::TextEdit::singleline(&mut d.name).hint_text("ex. Porte").desired_width(150.0));
                            if std::mem::take(&mut d.focus) {
                                r.request_focus();
                            }
                            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && name_problem(&d.name, &zones, None).is_none() {
                                change = Some(Change::Add(Zone { name: d.name.trim().to_string(), area: Polygon(d.points.clone()), linger_secs: d.linger }));
                            }
                        });
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new("Présence prolongée après").size(12.5).color(theme::muted()));
                            ui.add(egui::DragValue::new(&mut d.linger).range(0..=86_400).speed(2.0).suffix(" s"));
                            ui.label(RichText::new("0 = jamais").size(11.5).color(theme::faint()));
                        });
                        let problem = name_problem(&d.name, &zones, None);
                        if let Some(pb) = problem.filter(|_| !d.name.trim().is_empty()) {
                            ui.label(RichText::new(pb).size(12.0).color(theme::warn()));
                        }
                        ui.horizontal_wrapped(|ui| {
                            if ui.add_enabled(problem.is_none(), widgets::primary_button("Enregistrer", theme::accent())).clicked() {
                                change = Some(Change::Add(Zone { name: d.name.trim().to_string(), area: Polygon(d.points.clone()), linger_secs: d.linger }));
                            }
                            if ui.button("Redessiner").clicked() {
                                d.closed = false;
                            }
                            cancel = ui.button("Annuler").clicked();
                        });
                    }
                    None => {
                        let b = ui.add_enabled(has_image, egui::Button::new("+  Ajouter une zone").min_size(vec2(0.0, 30.0)));
                        if b.on_disabled_hover_text("Il faut une image de la caméra pour dessiner la zone").clicked() {
                            z.draft = Some(Draft::new(&c.id));
                            (z.edit, z.confirm_delete) = (None, None);
                        }
                    }
                }
                if cancel {
                    z.draft = None;
                }
                if !cv.enabled && !zones.is_empty() {
                    ui.label(RichText::new("Les zones servent quand l'analyse de cette caméra est active.").size(11.5).color(theme::faint()));
                }
            });
        z.list_open = (header.openness > 0.0).then(|| c.id.clone());

        let Some(change) = change else { return };
        let next = CameraVision { zones: apply(&zones, &change), ..cv };
        match self.hub.set_camera_vision(&c.id, next, Source::User) {
            Ok(()) => match &change {
                Change::Add(zone) => {
                    self.cams.zones.draft = None;
                    self.toast(format!("Zone « {} » enregistrée", zone.name), false);
                }
                Change::Update(..) => self.cams.zones.edit = None,
                Change::Remove(_) => self.cams.zones.confirm_delete = None,
            },
            Err(e) => self.toast(format!("{e:#}"), true),
        }
    }

    /// Captures d'écran : une zone d'exemple (le canapé de la pièce simulée), et une autre en cours
    /// de dessin si `drawing`.
    pub(in crate::gui) fn demo_zones(&mut self, camera: &str, drawing: bool) {
        let poly = |pts: &[(f32, f32)]| Polygon(pts.iter().map(|&(x, y)| Point::new(x, y)).collect());
        let sofa = Zone { name: "Canapé".into(), area: poly(&[(0.27, 0.55), (0.98, 0.55), (0.99, 0.99), (0.40, 0.99), (0.24, 0.86)]), linger_secs: 3600 };
        let cv = CameraVision { zones: vec![sofa], ..self.hub.camera_vision(camera) };
        let _ = self.hub.set_camera_vision(camera, cv, Source::User);
        if drawing {
            let mut d = Draft::new(camera);
            d.points = poly(&[(0.02, 0.72), (0.19, 0.70), (0.22, 0.97), (0.03, 0.99)]).0;
            self.cams.zones.draft = Some(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pts(list: &[(f32, f32)]) -> Vec<Point> {
        list.iter().map(|&(x, y)| Point::new(x, y)).collect()
    }

    fn zone(name: &str) -> Zone {
        Zone { name: name.into(), area: Polygon(pts(&[(0.1, 0.1), (0.4, 0.1), (0.4, 0.4)])), linger_secs: 0 }
    }

    #[test]
    fn clicks_to_image_coordinates() {
        // Image de 400 × 200 px posée en (100, 50), bandes noires autour.
        let img = Rect::from_min_size(pos2(100.0, 50.0), vec2(400.0, 200.0));
        assert_eq!(to_image(pos2(300.0, 150.0), img), Point::new(0.5, 0.5));
        assert_eq!(to_image(pos2(100.0, 250.0), img), Point::new(0.0, 1.0));
        // Clic sur les bandes noires : collé au bord.
        assert_eq!(to_image(pos2(40.0, 400.0), img), Point::new(0.0, 1.0));
        let p = pos2(220.0, 90.0);
        assert!(to_screen(to_image(p, img), img).distance(p) < 1e-3);
        // Image pas encore dimensionnée : pas de division par zéro.
        assert_eq!(to_image(pos2(5.0, 5.0), Rect::from_min_size(pos2(5.0, 5.0), vec2(0.0, 0.0))), Point::new(0.0, 0.0));
    }

    #[test]
    fn vertices_and_closing() {
        let img = Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 500.0));
        let mut v = Vec::new();
        assert!(push_vertex(&mut v, Point::new(0.1, 0.1)));
        // Second clic d'un double-clic au même endroit : ignoré.
        assert!(!push_vertex(&mut v, Point::new(0.101, 0.1)));
        assert!(push_vertex(&mut v, Point::new(0.5, 0.1)));
        // Deux sommets : le premier ne ferme rien.
        assert!(!on_first(&v, pos2(100.0, 50.0), img));
        push_vertex(&mut v, Point::new(0.5, 0.6));
        assert!(on_first(&v, pos2(106.0, 55.0), img));
        assert!(!on_first(&v, pos2(120.0, 50.0), img));
        let mut d = Draft::new("cam");
        d.points = v.clone();
        assert!(d.finish() && d.closed && d.focus);
        // Côtés croisés : le polygone reste ouvert.
        let mut bad = Draft::new("cam");
        bad.points = pts(&[(0.2, 0.2), (0.6, 0.6), (0.6, 0.2), (0.2, 0.6)]);
        assert!(!bad.finish() && !bad.closed);
    }

    #[test]
    fn names_and_changes() {
        let zones = vec![zone("Porte"), zone("Lit")];
        assert_eq!(name_problem("  ", &zones, None), Some("donne un nom à la zone"));
        assert_eq!(name_problem("porte", &zones, None), Some("une autre zone porte déjà ce nom"));
        assert_eq!(name_problem("Couloir", &zones, None), None);
        // Renommer « Porte » en « PORTE » : c'est la même zone.
        assert_eq!(name_problem("PORTE", &zones, Some("Porte")), None);
        assert_eq!(name_problem("Lit", &zones, Some("Porte")), Some("une autre zone porte déjà ce nom"));

        let added = apply(&zones, &Change::Add(zone("Couloir")));
        assert_eq!(added.iter().map(|z| z.name.as_str()).collect::<Vec<_>>(), ["Porte", "Lit", "Couloir"]);
        let renamed = apply(&zones, &Change::Update("Porte".into(), " Entrée ".into(), 45));
        assert_eq!((renamed[0].name.as_str(), renamed[0].linger_secs), ("Entrée", 45));
        assert_eq!(renamed[0].area, zones[0].area);
        assert_eq!(apply(&zones, &Change::Remove("Porte".into())), vec![zone("Lit")]);
        assert_eq!(linger_label(0), "sans alerte de durée");
        assert_eq!(linger_label(90), "alerte après 1 min 30 s");
    }

    #[test]
    fn concave_zones_are_filled_exactly() {
        let area = |p: &[Point], tris: &[[usize; 3]]| tris.iter().map(|&[a, b, c]| turn(p[a], p[b], p[c]).abs() / 2.0).sum::<f32>();
        let square = pts(&[(0.2, 0.2), (0.6, 0.2), (0.6, 0.6), (0.2, 0.6)]);
        assert_eq!(triangulate(&square).len(), 2);
        // En L, dans les deux sens de rotation : 4 triangles, toute la surface et rien qu'elle.
        let mut l = pts(&[(0.1, 0.1), (0.3, 0.1), (0.3, 0.5), (0.6, 0.5), (0.6, 0.7), (0.1, 0.7)]);
        for _ in 0..2 {
            let tris = triangulate(&l);
            assert_eq!(tris.len(), 4);
            assert!((area(&l, &tris) - Polygon(l.clone()).area()).abs() < 1e-5);
            for &[a, b, c] in &tris {
                let mid = Point::new((l[a].x + l[b].x + l[c].x) / 3.0, (l[a].y + l[b].y + l[c].y) / 3.0);
                assert!(Polygon(l.clone()).contains(mid), "triangle hors de la zone");
            }
            l.reverse();
        }
        assert!(triangulate(&square[..2]).is_empty());
    }
}
