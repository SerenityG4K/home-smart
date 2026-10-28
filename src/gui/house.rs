//! Vue « Maison 3D » : plan éditable (pièces, appareils) et maquette 3D vivante.

use super::icons::{self, Icon};
use super::three::{self, v3, Camera, Scene};
use super::{fx, theme, widgets, App};
use eframe::egui::{self, vec2, Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Layout, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui};
use homesmart::house::{HousePlan, Room};
use homesmart::hub::{DevicePatch, DeviceView};
use homesmart::kinds::{self, Kind};

const ROOM_TINTS: [Color32; 6] = [theme::AMBER, theme::BLUE, theme::GREEN, theme::VIOLET, theme::RED, Color32::from_rgb(0x22, 0xd3, 0xee)];

enum Drag {
    Room { id: u64, grab: [f32; 2] },
    Resize { id: u64 },
    Device { id: String },
}

pub struct HouseState {
    /// 0 = maquette 3D, 1 = plan.
    pub mode: usize,
    cam: Option<Camera>,
    pub full_walls: bool,
    drag: Option<Drag>,
    /// Copie de travail pendant un glissement (enregistrée au relâchement).
    edit: Option<HousePlan>,
    selected_room: Option<u64>,
    rename: String,
}

impl Default for HouseState {
    fn default() -> Self {
        HouseState { mode: 0, cam: None, full_walls: false, drag: None, edit: None, selected_room: None, rename: String::new() }
    }
}

fn snap(v: f32, step: f32) -> f32 {
    (v / step).round() * step
}

/// Position de chaque appareil sur le plan.
fn positions(plan: &HousePlan, devices: &[DeviceView]) -> Vec<[f32; 2]> {
    let mut per_room: std::collections::HashMap<String, usize> = Default::default();
    devices
        .iter()
        .map(|d| {
            let room = d.room.clone().unwrap_or_default();
            let i = per_room.entry(room.clone()).or_default();
            let p = plan.position_of(&d.id, d.room.as_deref(), *i);
            if !plan.placements.contains_key(&d.id) {
                *i += 1;
            }
            p
        })
        .collect()
}

fn device_state(app: &App, d: &DeviceView, kind: &Kind) -> (bool, String) {
    if !d.online {
        return (false, "Hors ligne".into());
    }
    let on = app.is_on(d, kind);
    let text = match (on, kinds::power_w(&d.dps, &d.schema)) {
        (true, Some(w)) => format!("{} W", kinds::fr(w, 0)),
        (true, None) => "Allumé".into(),
        (false, _) => "Éteint".into(),
    };
    (on, text)
}

impl App {
    pub(super) fn house_view(&mut self, ui: &mut Ui, devices: &[DeviceView]) {
        let plan = self.house.edit.clone().unwrap_or_else(|| self.hub.house());
        // En-tête : retour à la maison, titre, et le choix maquette / plan.
        ui.horizontal(|ui| {
            ui.set_min_height(42.0);
            if widgets::round_button(ui, Icon::Back, false).on_hover_text("Retour à la maison").clicked() {
                self.view = super::View::Devices;
            }
            ui.add_space(4.0);
            ui.label(RichText::new("Plan 3D").font(theme::bold(26.0)));
            if !plan.rooms.is_empty() || self.house.edit.is_some() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(i) = widgets::segmented(ui, &["Maquette 3D", "Plan"], Some(self.house.mode), theme::accent()) {
                        self.house.mode = i;
                    }
                });
            }
        });
        ui.add_space(12.0);
        if plan.rooms.is_empty() && self.house.edit.is_none() {
            self.house_empty(ui, devices);
            return;
        }
        ui.horizontal(|ui| {
            if self.house.mode == 0 {
                ui.checkbox(&mut self.house.full_walls, "Murs complets");
                if ui.button("Recentrer").clicked() {
                    self.house.cam = None;
                }
            } else {
                if ui.button("+  Pièce").clicked() {
                    let mut p = plan.clone();
                    let b = p.bounds();
                    let id = p.next_id();
                    let name = format!("Pièce {id}");
                    p.rooms.push(Room { id, name: name.clone(), x: snap(b[2] + 0.5, 0.25), y: snap(b[1], 0.25), w: 3.5, d: 3.0 });
                    self.hub.set_house(p, &format!("pièce « {name} » ajoutée"), &format!("add{id}"));
                    self.house.selected_room = Some(id);
                    self.house.rename = name;
                }
                ui.label(RichText::new("Glisse une pièce pour la déplacer, son coin pour la redimensionner, un appareil pour le placer.").size(12.0).color(theme::faint()));
            }
        });
        ui.add_space(10.0);
        let size = vec2(ui.available_width(), (ui.available_height() - 8.0).max(380.0));
        if self.house.mode == 0 {
            self.house_3d(ui, &plan, devices, size);
        } else {
            self.house_plan(ui, plan, devices, size);
        }
    }

    fn house_empty(&mut self, ui: &mut Ui, devices: &[DeviceView]) {
        egui::Frame::new().fill(theme::surface()).corner_radius(CornerRadius::same(18)).inner_margin(egui::Margin::same(28)).show(ui, |ui| {
            ui.set_max_width(640.0);
            ui.label(RichText::new("Ta maison en 3D").font(theme::bold(20.0)));
            ui.add_space(6.0);
            ui.label(
                RichText::new("Dessine tes pièces, places-y tes appareils, et retrouve-les dans une maquette 3D où chaque appareil allumé s'illumine en direct.")
                    .color(theme::muted()),
            );
            ui.add_space(12.0);
            let mut names: Vec<String> = devices.iter().filter_map(|d| d.room.clone()).collect();
            names.sort();
            names.dedup();
            ui.horizontal(|ui| {
                let label = if names.is_empty() { "Commencer avec une pièce".to_string() } else { format!("Créer mes {} pièce(s) à partir des appareils", names.len()) };
                if ui.add(widgets::primary_button(&label, theme::accent()).min_size(vec2(0.0, 34.0))).clicked() {
                    let names = if names.is_empty() { vec!["Salon".to_string()] } else { names.clone() };
                    let plan = HousePlan::from_room_names(&names);
                    self.hub.set_house(plan, &format!("{} pièce(s) créée(s) : {}", names.len(), names.join(", ")), "create");
                    self.house.mode = 1;
                }
            });
            ui.add_space(6.0);
            ui.label(RichText::new("Tu pourras ensuite ajuster la taille et la place de chaque pièce dans l'onglet « Plan ».").size(12.0).color(theme::faint()));
        });
    }

    // ---- Maquette 3D -------------------------------------------------------------------

    fn house_3d(&mut self, ui: &mut Ui, plan: &HousePlan, devices: &[DeviceView], size: egui::Vec2) {
        let b = plan.bounds();
        let (cx, cz) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
        let span = (b[2] - b[0]).max(b[3] - b[1]).max(4.0);
        let mut cam = self.house.cam.take().unwrap_or_else(|| Camera::new(v3(0.0, 0.0, 0.0), span * 1.05 + 2.0));
        let to3 = |x: f32, y: f32, h: f32| v3(x - cx, h, y - cz);

        // Présence en direct : personnes vues par les caméras analysées, pièce par pièce (nom de
        // pièce de la caméra = nom de pièce du plan).
        let occupancy: Vec<(String, usize)> = self
            .hub
            .cameras()
            .iter()
            .filter_map(|c| {
                let room = c.room.as_ref()?.trim().to_lowercase();
                let a = self.hub.camera_analysis(&c.id).filter(|a| a.running)?;
                let fresh = a.updated.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(3));
                Some((room, if fresh { a.persons.len() } else { 0 }))
            })
            .collect();
        let people_in = |name: &str| -> Option<usize> {
            let n = name.trim().to_lowercase();
            let mut found = occupancy.iter().filter(|(r, _)| *r == n).peekable();
            found.peek()?;
            Some(found.map(|(_, k)| *k).max().unwrap_or(0))
        };

        let mut s = Scene::default();
        // Sol et quadrillage.
        let m = 1.5;
        let (gx0, gz0, gx1, gz1) = (b[0] - m, b[1] - m, b[2] + m, b[3] + m);
        s.quad(to3(gx0, gz0, -0.02), to3(gx0, gz1, -0.02), to3(gx1, gz1, -0.02), to3(gx1, gz0, -0.02), Color32::from_rgb(0x12, 0x17, 0x22), 0);
        s.layer = 1;
        for (i, r) in plan.rooms.iter().enumerate() {
            let mut tint = theme::mix(Color32::from_rgb(0x1b, 0x21, 0x2e), ROOM_TINTS[i % ROOM_TINTS.len()], 0.18);
            if people_in(&r.name).is_some_and(|n| n > 0) {
                tint = theme::mix(tint, theme::accent(), 0.3);
            }
            let (x0, z0, x1, z1) = (r.x, r.y, r.x + r.w, r.y + r.d);
            s.quad(to3(x0, z0, 0.0), to3(x0, z1, 0.0), to3(x1, z1, 0.0), to3(x1, z0, 0.0), tint, 0);
        }
        s.layer = 2;
        let grid = Stroke::new(1.0, theme::alpha(theme::line(), 0.55));
        let mut x = gx0.ceil();
        while x <= gx1 {
            s.lines.push((to3(x, gz0, 0.0), to3(x, gz1, 0.0), grid));
            x += 1.0;
        }
        let mut z = gz0.ceil();
        while z <= gz1 {
            s.lines.push((to3(gx0, z, 0.0), to3(gx1, z, 0.0), grid));
            z += 1.0;
        }
        // Pièces : sol teinté, murs (coupés à mi-hauteur ou complets).
        let wall_h = if self.house.full_walls { plan.wall_height } else { 0.9 };
        let wall = Color32::from_rgb(0xb4, 0xbc, 0xcb);
        let t = 0.06;
        for r in plan.rooms.iter() {
            let (x0, z0, x1, z1) = (r.x, r.y, r.x + r.w, r.y + r.d);
            s.cuboid(to3(x0 - t, z0 - t, 0.0), to3(x1 + t, z0 + t, wall_h), wall, 0);
            s.cuboid(to3(x0 - t, z1 - t, 0.0), to3(x1 + t, z1 + t, wall_h), wall, 0);
            s.cuboid(to3(x0 - t, z0 + t, 0.0), to3(x0 + t, z1 - t, wall_h), wall, 0);
            s.cuboid(to3(x1 - t, z0 + t, 0.0), to3(x1 + t, z1 - t, wall_h), wall, 0);
            let c = r.center();
            s.labels.push((to3(c[0], r.y + r.d * 0.12, 0.02), r.name.clone(), theme::bold(12.0), theme::alpha(theme::muted(), 0.9)));
            // Personnes vues : une silhouette par personne (jusqu'à 4), et le compte.
            match people_in(&r.name) {
                Some(0) => s.labels.push((to3(c[0], r.y + r.d * 0.22, 0.02), "vide · surveillée".into(), FontId::proportional(11.0), theme::alpha(theme::muted(), 0.7))),
                Some(n) => {
                    for k in 0..n.min(4) {
                        let px = c[0] + (k as f32 - (n.min(4) as f32 - 1.0) / 2.0) * 0.5;
                        let pz = c[1] + r.d * 0.15;
                        s.cuboid(to3(px - 0.09, pz - 0.07, 0.0), to3(px + 0.09, pz + 0.07, 0.85), theme::mix(theme::accent(), Color32::BLACK, 0.25), 0);
                        let head = to3(px, pz, 1.02);
                        s.sphere(head, 0.13, theme::accent(), 0, true);
                        s.glows.push((head, 0.9, theme::alpha(theme::accent(), 0.3)));
                    }
                    let label = if n == 1 { "1 personne".to_string() } else { format!("{n} personnes") };
                    s.labels.push((to3(c[0], c[1] + r.d * 0.15, 1.4), label, theme::bold(12.0), theme::accent()));
                }
                None => {}
            }
        }
        // Appareils : socle + sphère ; allumés, ils brillent de leur couleur.
        let pos = positions(plan, devices);
        for (i, (d, p)) in devices.iter().zip(&pos).enumerate() {
            let kind = kinds::detect(d.kind.as_deref(), &d.dps);
            let accent = theme::kind_accent(&kind);
            let (on, text) = device_state(self, d, &kind);
            let tag = i as u32 + 1;
            let selected = self.selected.as_deref() == Some(d.id.as_str());
            s.cuboid(to3(p[0] - 0.18, p[1] - 0.18, 0.0), to3(p[0] + 0.18, p[1] + 0.18, 0.45), Color32::from_rgb(0x3a, 0x42, 0x52), tag);
            let c = to3(p[0], p[1], 0.7);
            if on {
                s.sphere(c, 0.26, theme::mix(accent, Color32::WHITE, 0.25), tag, false);
                s.glows.push((c, 1.8, theme::alpha(accent, 0.4)));
            } else {
                s.sphere(c, 0.26, if d.online { Color32::from_rgb(0x5d, 0x66, 0x76) } else { Color32::from_rgb(0x50, 0x2e, 0x33) }, tag, true);
            }
            if selected {
                s.glows.push((c, 0.9, theme::alpha(Color32::WHITE, 0.18)));
            }
            let color = if on { accent } else { theme::muted() };
            // Étiquettes à deux hauteurs en alternance, pour éviter qu'elles se chevauchent.
            let h = 1.25;
            s.lines.push((to3(p[0], p[1], 0.96), to3(p[0], p[1], h - 0.12), Stroke::new(1.0, theme::alpha(color, 0.35))));
            s.labels.push((to3(p[0], p[1], h), format!("{}  ·  {text}", d.name), FontId::proportional(12.0), color));
        }

        let (resp, hover) = three::viewport(ui, size, &mut cam, &s);
        self.house.cam = Some(cam);
        if let Some(tag) = hover {
            if let Some(d) = devices.get(tag as usize - 1) {
                let (_, text) = device_state(self, d, &kinds::detect(d.kind.as_deref(), &d.dps));
                let resp = resp.clone().on_hover_text_at_pointer(format!("{}\n{text}\nCliquer pour ouvrir", d.name));
                if resp.clicked() {
                    let id = d.id.clone();
                    self.select(&id);
                }
            }
        }
    }

    // ---- Plan 2D éditable ----------------------------------------------------------------

    fn house_plan(&mut self, ui: &mut Ui, plan: HousePlan, devices: &[DeviceView], size: egui::Vec2) {
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 16.0, theme::alpha(theme::bg(), 0.4));
        // Échelle : tout le plan (plus une marge) tient dans la zone, mais on garde une échelle stable
        // pendant un glissement pour que rien ne « saute ».
        let b = if self.house.drag.is_some() { self.hub.house().bounds() } else { plan.bounds() };
        let margin = 2.0;
        let (w, h) = (b[2] - b[0] + margin * 2.0, b[3] - b[1] + margin * 2.0);
        let scale = (rect.width() / w).min(rect.height() / h).clamp(18.0, 140.0);
        let origin = rect.center() - vec2((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0) * scale;
        let to_screen = |x: f32, y: f32| origin + vec2(x, y) * scale;
        let to_plan = |q: Pos2| {
            let v = (q - origin) / scale;
            [v.x, v.y]
        };

        // Quadrillage (1 m).
        let tl = to_plan(rect.min);
        let br = to_plan(rect.max);
        let grid = Stroke::new(1.0, theme::alpha(theme::line(), 0.5));
        let mut x = tl[0].floor();
        while x <= br[0] {
            p.line_segment([to_screen(x, tl[1]), to_screen(x, br[1])], grid);
            x += 1.0;
        }
        let mut y = tl[1].floor();
        while y <= br[1] {
            p.line_segment([to_screen(tl[0], y), to_screen(br[0], y)], grid);
            y += 1.0;
        }

        let pointer = resp.interact_pointer_pos().or(resp.hover_pos());
        let pos = positions(&plan, devices);
        let device_at = |q: Pos2| pos.iter().position(|pp| (to_screen(pp[0], pp[1]) - q).length() < 16.0);
        let handle_rect = |r: &Room| Rect::from_center_size(to_screen(r.x + r.w, r.y + r.d), vec2(14.0, 14.0));

        // Pièces.
        for (i, r) in plan.rooms.iter().enumerate() {
            let rr = Rect::from_min_max(to_screen(r.x, r.y), to_screen(r.x + r.w, r.y + r.d));
            let tint = ROOM_TINTS[i % ROOM_TINTS.len()];
            let sel = self.house.selected_room == Some(r.id);
            p.rect_filled(rr, 6.0, theme::alpha(tint, if sel { 0.16 } else { 0.09 }));
            p.rect_stroke(rr, 6.0, Stroke::new(if sel { 2.0 } else { 1.2 }, theme::alpha(tint, if sel { 0.9 } else { 0.55 })), StrokeKind::Inside);
            p.text(rr.center_top() + vec2(0.0, 12.0), Align2::CENTER_TOP, &r.name, theme::bold(13.0), theme::text());
            p.text(
                rr.center_top() + vec2(0.0, 30.0),
                Align2::CENTER_TOP,
                format!("{} × {} m", kinds::fr(r.w as f64, 1), kinds::fr(r.d as f64, 1)),
                FontId::proportional(11.0),
                theme::muted(),
            );
            let hr = handle_rect(r);
            p.rect_filled(hr, 3.0, theme::alpha(tint, 0.85));
        }

        // Appareils.
        for (i, d) in devices.iter().enumerate() {
            let kind = kinds::detect(d.kind.as_deref(), &d.dps);
            let accent = theme::kind_accent(&kind);
            let (on, _) = device_state(self, d, &kind);
            let c = to_screen(pos[i][0], pos[i][1]);
            if on {
                fx::glow(&p, c, 34.0, theme::alpha(accent, 0.35));
            }
            let sel = self.selected.as_deref() == Some(d.id.as_str());
            p.circle_filled(c, 13.0, if on { theme::mix(theme::raised(), accent, 0.35) } else { theme::raised_hi() });
            p.circle_stroke(c, 13.0, Stroke::new(if sel { 2.0 } else { 1.0 }, if sel { Color32::WHITE } else { theme::mix(theme::line(), accent, 0.6) }));
            icons::paint(&p, Rect::from_center_size(c, vec2(14.0, 14.0)), Icon::from(&kind), if on { accent } else { theme::muted() });
            p.text(c + vec2(0.0, 17.0), Align2::CENTER_TOP, &d.name, FontId::proportional(11.0), theme::muted());
        }

        // Glisser : appareil (prioritaire), poignée de coin, ou pièce.
        if resp.drag_started() {
            if let Some(start) = ui.input(|i| i.pointer.press_origin()) {
                self.house.edit = Some(plan.clone());
                self.house.drag = if let Some(i) = device_at(start) {
                    Some(Drag::Device { id: devices[i].id.clone() })
                } else if let Some(r) = plan.rooms.iter().rev().find(|r| handle_rect(r).expand(4.0).contains(start)) {
                    Some(Drag::Resize { id: r.id })
                } else {
                    let q = to_plan(start);
                    plan.rooms.iter().rev().find(|r| r.contains(q[0], q[1])).map(|r| Drag::Room { id: r.id, grab: [q[0] - r.x, q[1] - r.y] })
                };
                if let Some(Drag::Room { id, .. } | Drag::Resize { id }) = &self.house.drag {
                    self.house.selected_room = Some(*id);
                    self.house.rename = plan.rooms.iter().find(|r| r.id == *id).map(|r| r.name.clone()).unwrap_or_default();
                }
            }
        }
        if let (Some(q), Some(drag), Some(edit)) = (pointer.filter(|_| resp.dragged()), &self.house.drag, self.house.edit.as_mut()) {
            let q = to_plan(q);
            match drag {
                Drag::Room { id, grab } => {
                    if let Some(r) = edit.rooms.iter_mut().find(|r| r.id == *id) {
                        let (nx, ny) = (snap(q[0] - grab[0], 0.25), snap(q[1] - grab[1], 0.25));
                        // Les appareils posés dans la pièce la suivent.
                        let (dx, dy) = (nx - r.x, ny - r.y);
                        let moved = r.clone();
                        r.x = nx;
                        r.y = ny;
                        for (i, d) in devices.iter().enumerate() {
                            if moved.contains(pos[i][0], pos[i][1]) && (dx != 0.0 || dy != 0.0) {
                                edit.placements.insert(d.id.clone(), [pos[i][0] + dx, pos[i][1] + dy]);
                            }
                        }
                    }
                }
                Drag::Resize { id } => {
                    if let Some(r) = edit.rooms.iter_mut().find(|r| r.id == *id) {
                        r.w = snap(q[0] - r.x, 0.25).max(1.0);
                        r.d = snap(q[1] - r.y, 0.25).max(1.0);
                    }
                }
                Drag::Device { id } => {
                    edit.placements.insert(id.clone(), [snap(q[0], 0.05), snap(q[1], 0.05)]);
                }
            }
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        }
        if resp.drag_stopped() {
            if let (Some(edit), Some(drag)) = (self.house.edit.take(), self.house.drag.take()) {
                self.commit_drag(edit, drag, devices);
            }
        }
        if resp.clicked() {
            if let Some(q) = pointer {
                if let Some(i) = device_at(q) {
                    let id = devices[i].id.clone();
                    self.select(&id);
                } else {
                    let pq = to_plan(q);
                    self.house.selected_room = plan.rooms.iter().rev().find(|r| r.contains(pq[0], pq[1])).map(|r| r.id);
                    self.house.rename = plan.rooms.iter().find(|r| Some(r.id) == self.house.selected_room).map(|r| r.name.clone()).unwrap_or_default();
                }
            }
        }
        if pointer.is_some_and(|q| device_at(q).is_some() || plan.rooms.iter().any(|r| handle_rect(r).contains(q))) && !resp.dragged() {
            ui.ctx().set_cursor_icon(CursorIcon::Grab);
        }

        // Réglages de la pièce sélectionnée, en surimpression.
        if let Some(room) = self.house.selected_room.and_then(|id| plan.rooms.iter().find(|r| r.id == id).cloned()) {
            let box_rect = Rect::from_min_size(rect.left_top() + vec2(14.0, 14.0), vec2(250.0, 96.0));
            let mut ui2 = ui.new_child(egui::UiBuilder::new().max_rect(box_rect).layout(Layout::top_down(Align::Min)));
            egui::Frame::new().fill(theme::alpha(theme::surface(), 0.95)).corner_radius(CornerRadius::same(12)).inner_margin(egui::Margin::same(12)).stroke(Stroke::new(1.0, theme::line())).show(&mut ui2, |ui| {
                ui.label(RichText::new("Pièce").size(11.5).color(theme::muted()));
                let r = ui.add(egui::TextEdit::singleline(&mut self.house.rename).desired_width(220.0));
                if r.lost_focus() && !self.house.rename.trim().is_empty() && self.house.rename.trim() != room.name {
                    let mut p2 = self.hub.house();
                    let new = self.house.rename.trim().to_string();
                    if let Some(rr) = p2.rooms.iter_mut().find(|rr| rr.id == room.id) {
                        rr.name = new.clone();
                    }
                    self.hub.set_house(p2, &format!("pièce « {} » renommée « {new} »", room.name), &format!("rename{}", room.id));
                }
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Supprimer la pièce").color(theme::crit())).clicked() {
                        let mut p2 = self.hub.house();
                        p2.rooms.retain(|rr| rr.id != room.id);
                        self.hub.set_house(p2, &format!("pièce « {} » supprimée", room.name), &format!("del{}", room.id));
                        self.house.selected_room = None;
                    }
                });
            });
        }
    }

    fn commit_drag(&mut self, edit: HousePlan, drag: Drag, devices: &[DeviceView]) {
        match drag {
            Drag::Room { id, .. } => {
                let name = edit.rooms.iter().find(|r| r.id == id).map(|r| r.name.clone()).unwrap_or_default();
                self.hub.set_house(edit, &format!("pièce « {name} » déplacée"), &format!("move{id}"));
            }
            Drag::Resize { id } => {
                let r = edit.rooms.iter().find(|r| r.id == id).cloned();
                if let Some(r) = r {
                    let what = format!("pièce « {} » redimensionnée ({} × {} m)", r.name, kinds::fr(r.w as f64, 2), kinds::fr(r.d as f64, 2));
                    self.hub.set_house(edit, &what, &format!("size{id}"));
                }
            }
            Drag::Device { id } => {
                let pos = edit.placements.get(&id).copied();
                let room = pos.and_then(|p| edit.room_at(p[0], p[1])).map(|r| r.name.clone());
                let dev = devices.iter().find(|d| d.id == id);
                let name = dev.map(|d| d.name.clone()).unwrap_or_default();
                let what = match &room {
                    Some(r) => format!("« {name} » placé dans « {r} »"),
                    None => format!("« {name} » placé hors des pièces"),
                };
                self.hub.set_house(edit, &what, &format!("dev{id}"));
                // La pièce de l'appareil suit sa position sur le plan.
                if let (Some(room), Some(d)) = (room, dev) {
                    if d.room.as_deref() != Some(room.as_str()) {
                        if let Err(e) = self.hub.update(&id, DevicePatch { room: Some(room), ..Default::default() }) {
                            self.toast(format!("{e:#}"), true);
                        }
                    }
                }
            }
        }
    }
}
