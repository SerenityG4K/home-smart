//! Aspirateurs robots : tuile de la page Maison, panneau (état, commandes, réglages du nettoyage,
//! usure, automatismes, programmes, connexion) et fenêtre d'ajout (Valetudo, miIO ; les robots
//! Tuya s'importent comme les autres appareils).

use super::icons::{self, Icon};
use super::{fx, one_line, panel, problems_ui, theme, widgets, App, Toast, View, TILE_H};
use eframe::egui::{self, pos2, vec2, Align, Align2, Color32, CornerRadius, CursorIcon, Id, Label, Layout, Margin, Rect, RichText, ScrollArea, Sense, Stroke, StrokeKind, Ui};
use homesmart::automation::{Action, Scenario, Trigger};
use homesmart::hub::{Level, Source, VacuumPatch, VacuumView};
use homesmart::vacuum::{self, scan::Found, Activity, Choice, Command, Family, Link, Status, VacuumConfig};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Résultat d'une tâche lancée en arrière-plan (recherche, essai de connexion).
type Slot<T> = Arc<Mutex<Option<T>>>;

/// Fenêtre « Ajouter un aspirateur robot ».
pub struct VacAdd {
    /// 0 = Valetudo, 1 = miIO, 2 = Tuya / Smart Life.
    family: usize,
    host: String,
    port: String,
    user: String,
    password: String,
    token: String,
    show_secret: bool,
    name: String,
    room: String,
    found: Slot<Vec<Found>>,
    scanning: bool,
    test: Slot<Result<(String, Status), String>>,
    testing: bool,
    error: Option<String>,
}

impl VacAdd {
    pub fn new() -> VacAdd {
        VacAdd {
            family: 0,
            host: String::new(),
            port: String::new(),
            user: String::new(),
            password: String::new(),
            token: String::new(),
            show_secret: false,
            name: String::new(),
            room: String::new(),
            found: Arc::default(),
            scanning: false,
            test: Arc::default(),
            testing: false,
            error: None,
        }
    }

    fn link(&self) -> Result<Link, String> {
        let host = self.host.trim().to_string();
        if host.is_empty() {
            return Err("indique l'adresse IP du robot".into());
        }
        let port = match self.port.trim() {
            "" => None,
            p => Some(p.parse::<u16>().map_err(|_| format!("port invalide : {p}"))?),
        };
        let opt = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
        Ok(match self.family {
            0 => Link::Valetudo { host, port, user: opt(&self.user), password: opt(&self.user).and(Some(self.password.clone())) },
            _ => {
                vacuum::miio::parse_token(&self.token).map_err(|e| format!("{e:#}"))?;
                Link::Miio { host, token: self.token.trim().to_lowercase(), port }
            }
        })
    }
}

/// Réglages de connexion d'un robot hors Tuya (page « Réglages » du panneau).
pub struct VacForm {
    id: String,
    name: String,
    room: String,
    host: String,
    port: String,
    user: String,
    password: String,
    token: String,
    show_secret: bool,
    confirm_delete: bool,
    error: Option<String>,
}

/// État de l'interface des aspirateurs.
#[derive(Default)]
pub struct VacUi {
    pub add: Option<VacAdd>,
    form: Option<VacForm>,
    /// Dernière commande envoyée depuis l'interface : (robot, quand), pour l'indicateur d'envoi.
    sent: Option<(String, Instant)>,
    /// Durée d'absence choisie pour « nettoyer quand la maison est vide », et le robot pour qui
    /// elle a été relue dans son automatisme.
    empty_minutes: u32,
    empty_for: Option<String>,
}

const SENT_TTL: Duration = Duration::from_millis(2500);

/// Jauge de batterie : contour, niveau coloré, éclair quand le robot se recharge.
fn battery(ui: &mut Ui, level: Option<u8>, charging: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(46.0, 24.0), Sense::hover());
    let p = ui.painter();
    let body = Rect::from_min_max(rect.min, rect.max - vec2(5.0, 0.0));
    p.rect_stroke(body, CornerRadius::same(6), Stroke::new(1.8, theme::muted()), StrokeKind::Inside);
    p.rect_filled(Rect::from_center_size(pos2(body.right() + 2.5, body.center().y), vec2(3.0, 9.0)), CornerRadius::same(1), theme::muted());
    if let Some(l) = level {
        let color = if charging {
            theme::accent()
        } else if l <= 10 {
            theme::crit()
        } else if l <= 20 {
            theme::warn()
        } else {
            theme::accent()
        };
        let inner = body.shrink(3.5);
        let w = (inner.width() * l as f32 / 100.0).max(2.0);
        p.rect_filled(Rect::from_min_size(inner.min, vec2(w, inner.height())), CornerRadius::same(3), color);
    }
    if charging {
        icons::paint(p, Rect::from_center_size(body.center(), vec2(15.0, 15.0)), Icon::Bolt, theme::text());
    }
}

/// Bouton d'action rond de 54 px, avec son libellé dessous.
fn action(ui: &mut Ui, icon: Icon, label: &str, primary: bool, enabled: bool, busy: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(76.0, 82.0), if enabled { Sense::click() } else { Sense::hover() });
    let h = ui.ctx().animate_bool_with_time(resp.id, enabled && resp.hovered(), 0.12);
    let c = pos2(rect.center().x, rect.top() + 28.0);
    let p = ui.painter();
    let (fill, fg) = if primary {
        (theme::mix(theme::accent(), theme::text(), 0.12 * h), theme::on_accent())
    } else {
        (theme::mix(theme::raised(), theme::raised_hi(), h), theme::text())
    };
    let alpha = if enabled { 1.0 } else { 0.4 };
    p.circle_filled(c, 27.0, theme::alpha(fill, alpha));
    if busy {
        // Commande envoyée, en attente de l'effet : un arc qui tourne autour du bouton.
        let t = ui.input(|i| i.time) as f32;
        let pts: Vec<egui::Pos2> = (0..=16).map(|i| {
            let a = t * 5.0 + i as f32 * 0.12;
            c + vec2(a.cos(), a.sin()) * 30.5
        }).collect();
        p.add(egui::Shape::line(pts, Stroke::new(2.5, theme::accent())));
        ui.ctx().request_repaint_after(Duration::from_millis(33));
    }
    icons::paint(p, Rect::from_center_size(c, vec2(22.0, 22.0)), icon, theme::alpha(fg, alpha));
    p.text(pos2(rect.center().x, rect.bottom() - 10.0), Align2::CENTER_CENTER, label, egui::FontId::proportional(12.5), theme::alpha(theme::muted(), alpha.max(0.7)));
    if enabled && resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    resp
}

/// Choix d'un réglage : contrôle segmenté s'il tient sur la largeur, liste déroulante sinon.
fn choice(ui: &mut Ui, id: &str, list: &[Choice], current: Option<&str>, enabled: bool) -> Option<String> {
    let labels: Vec<&str> = list.iter().map(|c| c.label.as_str()).collect();
    let selected = current.and_then(|v| list.iter().position(|c| c.value == v));
    // Même mesure que `widgets::segmented` (texte en gras, marges, largeur minimale).
    let width: f32 = labels.iter().map(|l| (ui.painter().layout_no_wrap(l.to_string(), theme::bold(13.5), theme::text()).size().x + 28.0).max(56.0)).sum::<f32>() + 6.0;
    if width <= ui.available_width() {
        let mut out = None;
        ui.add_enabled_ui(enabled, |ui| {
            out = widgets::segmented(ui, &labels, selected, theme::accent()).filter(|i| Some(*i) != selected).map(|i| list[i].value.clone());
        });
        return out;
    }
    let mut out = None;
    let shown = selected.map(|i| list[i].label.clone()).unwrap_or_else(|| "—".into());
    ui.add_enabled_ui(enabled, |ui| {
        egui::ComboBox::from_id_salt(("vac-choice", id)).selected_text(shown).width(ui.available_width().min(260.0)).show_ui(ui, |ui| {
            for c in list {
                if ui.selectable_label(current == Some(c.value.as_str()), &c.label).clicked() && current != Some(c.value.as_str()) {
                    out = Some(c.value.clone());
                }
            }
        });
    });
    out
}

/// État court d'une tuile : « Nettoyage · 64 % », « Brosse principale bloquée », « Hors ligne ».
pub(super) fn tile_state(v: &VacuumView) -> String {
    if !v.online {
        return "Hors ligne".into();
    }
    let what = match v.status.activity {
        Activity::Cleaning => "Nettoyage",
        Activity::Returning => "Retour à la base",
        Activity::Charging => "En charge",
        Activity::Docked => "Sur sa base",
        a => a.label(),
    };
    match (v.status.errors.first(), v.status.battery) {
        (Some(e), _) => e.label.clone(),
        (None, Some(b)) => format!("{what} · {b} %"),
        (None, None) => what.to_string(),
    }
}

impl App {
    /// Envoie une commande au robot (origine : toi) ; un échec s'affiche en notification.
    fn vac_cmd(&mut self, id: &str, cmd: Command) {
        self.vac.sent = Some((id.to_string(), Instant::now()));
        let (hub, toasts, ctx, id) = (self.hub.clone(), self.toasts.clone(), self.ctx.clone(), id.to_string());
        self.rt.spawn(async move {
            if let Err(e) = hub.vacuum_command(&id, cmd, Source::User).await {
                let name = hub.vacuum(&id).map(|v| v.name).unwrap_or_default();
                toasts.lock().unwrap().push(Toast { text: format!("{name} : {e:#}"), error: true, at: Instant::now() });
            }
            ctx.request_repaint();
        });
    }

    /// Captures automatiques : un nettoyage lancé comme depuis l'interface.
    pub(super) fn vac_start_for_shot(&mut self, id: &str) {
        self.vac_cmd(id, Command::Start);
    }

    fn vac_busy(&self, id: &str) -> bool {
        self.vac.sent.as_ref().is_some_and(|(i, t)| i == id && t.elapsed() < SENT_TTL)
    }

    /// Commande d'un clic sur la tuile : pause pendant un nettoyage, sinon nettoyer.
    fn vac_tap(&mut self, v: &VacuumView) -> bool {
        let cmd = match v.status.activity {
            Activity::Cleaning if v.caps.pause => Command::Pause,
            Activity::Error => return false,
            _ => Command::Start,
        };
        self.vac_cmd(&v.id, cmd);
        true
    }

    /// Tuile d'un aspirateur : allumée pendant qu'il nettoie ou rentre ; un clic lance (ou met en
    /// pause) le nettoyage, un clic droit ou « ⋯ » ouvre le panneau.
    pub(super) fn vacuum_tile(&mut self, ui: &mut Ui, v: &VacuumView, w: f32) {
        let active = v.online && v.status.activity.is_active();
        let (rect, resp) = ui.allocate_exact_size(vec2(w, TILE_H), Sense::click());
        let id = Id::unique(("vac-tile", &v.id));
        let t = ui.ctx().animate_bool_with_time(id.with("on"), active, 0.2);
        let h = ui.ctx().animate_bool_with_time(id.with("h"), resp.hovered(), 0.12);
        let selected = self.selected.as_deref() == Some(v.id.as_str());
        let p = ui.painter();
        if v.online {
            if t > 0.01 {
                fx::shadow(p, rect, 22, -0.2 + 0.4 * h);
            }
            let off = theme::mix(theme::tile_off(), theme::raised(), 0.7 * h);
            p.rect_filled(rect, CornerRadius::same(22), theme::mix(off, theme::tile_on(), t));
        } else {
            p.rect_stroke(rect, CornerRadius::same(22), Stroke::new(1.5, theme::line()), StrokeKind::Inside);
        }
        let problem = self.problems_now.iter().filter(|p| p.device == v.id && p.level != Level::Info).max_by_key(|p| problems_ui::rank(p.level));
        if let Some(pr) = problem {
            p.rect_stroke(rect, CornerRadius::same(22), Stroke::new(2.0, problems_ui::level_color(pr.level)), StrokeKind::Inside);
        }
        if selected {
            p.rect_stroke(rect.expand(3.0), CornerRadius::same(25), Stroke::new(2.5, theme::accent()), StrokeKind::Outside);
        }
        let disc = Rect::from_min_size(rect.min + vec2(16.0, 16.0), vec2(42.0, 42.0));
        let disc_off = if theme::is_light() { Color32::from_rgb(0xdf, 0xe3, 0xe8) } else { theme::raised() };
        let faded = if v.online { 1.0 } else { 0.5 };
        p.circle_filled(disc.center(), 21.0, theme::alpha(theme::mix(disc_off, theme::sun(), t), faded));
        icons::paint(p, disc.shrink(10.5), Icon::Vacuum, theme::alpha(theme::mix(theme::muted(), theme::ink_on(theme::sun()), t), faded));
        // Batterie : petite pile en haut à droite (sous le « ⋯ » du survol).
        if let (true, Some(b)) = (v.online, v.status.battery) {
            let body = Rect::from_min_size(pos2(rect.right() - 44.0, rect.top() + 52.0), vec2(22.0, 11.0));
            let edge = theme::alpha(theme::mix(theme::muted(), theme::tile_ink_2(), t), 0.9);
            p.rect_stroke(body, CornerRadius::same(3), Stroke::new(1.3, edge), StrokeKind::Inside);
            p.rect_filled(Rect::from_center_size(pos2(body.right() + 1.5, body.center().y), vec2(2.0, 5.0)), CornerRadius::same(1), edge);
            let inner = body.shrink(2.5);
            let color = if b <= 15 && !v.status.charging { theme::warn() } else { theme::mix(theme::accent(), theme::tile_ink(), t) };
            p.rect_filled(Rect::from_min_size(inner.min, vec2((inner.width() * b as f32 / 100.0).max(1.5), inner.height())), CornerRadius::same(1), color);
        }
        let ink = theme::mix(theme::text(), theme::tile_ink(), t);
        let ink2 = theme::mix(theme::muted(), theme::tile_ink_2(), t);
        let state = tile_state(v);
        let state_color = if !v.online {
            theme::off()
        } else if let Some(pr) = problem {
            problems_ui::level_color(pr.level)
        } else if !v.status.errors.is_empty() {
            theme::warn()
        } else {
            ink2
        };
        let name = one_line(p, &v.name, theme::bold(15.0), theme::alpha(ink, faded), w - 32.0);
        let st = one_line(p, &state, egui::FontId::proportional(13.0), state_color, w - 32.0);
        p.galley(pos2(rect.left() + 16.0, rect.bottom() - 16.0 - st.size().y - 2.0 - name.size().y), name, ink);
        p.galley(pos2(rect.left() + 16.0, rect.bottom() - 16.0 - st.size().y), st, state_color);
        let dots = Rect::from_center_size(pos2(rect.right() - 26.0, rect.top() + 28.0), vec2(30.0, 30.0));
        if h > 0.01 || selected {
            let k = if selected { 1.0 } else { h };
            p.circle_filled(dots.center(), 15.0, theme::alpha(theme::mix(theme::raised_hi(), Color32::from_rgb(0xe2, 0xe5, 0xea), t), 0.9 * k));
            icons::paint(p, dots.shrink(7.0), Icon::Dots, theme::alpha(ink, k));
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        let on_dots = resp.interact_pointer_pos().is_some_and(|pos| dots.contains(pos));
        if resp.secondary_clicked() || (resp.clicked() && on_dots) {
            self.select(&v.id);
        } else if resp.clicked() && !(v.online && self.vac_tap(v)) {
            self.select(&v.id);
        }
        let tip = match v.status.activity {
            Activity::Cleaning => "Clic : pause · clic droit ou « ⋯ » : détails",
            _ if v.online => "Clic : nettoyer · clic droit ou « ⋯ » : détails",
            _ => "Clic : détails",
        };
        resp.on_hover_text(tip);
    }

    // ---- Panneau ----------------------------------------------------------------------------

    /// Panneau d'un aspirateur, posé à droite comme celui des autres appareils.
    pub(super) fn vacuum_sheet(&mut self, ctx: &egui::Context) {
        let open = matches!(self.view, View::Devices | View::House) && self.selected.as_deref().is_some_and(|id| self.hub.is_vacuum(id));
        let t = ctx.animate_bool_with_time(Id::unique("vac-sheet-open"), open, 0.18);
        if !open {
            return;
        }
        let Some(v) = self.selected.clone().and_then(|id| self.hub.vacuum(&id)) else { return };
        let screen = ctx.content_rect();
        let width = if screen.width() < 720.0 { screen.width() - 24.0 } else { 400.0 };
        let height = screen.height() - 24.0;
        let area = egui::Modal::default_area(Id::unique("vac-sheet")).anchor(Align2::RIGHT_TOP, vec2(-12.0 + (1.0 - t) * 48.0, 12.0));
        let mut frame = egui::Frame::new().fill(theme::surface()).corner_radius(CornerRadius::same(26)).inner_margin(Margin::symmetric(24, 22));
        frame = frame.shadow(egui::epaint::Shadow { offset: [-12, 0], blur: 48, spread: 0, color: Color32::from_black_alpha(if theme::is_light() { 40 } else { 120 }) });
        if !theme::is_light() {
            frame = frame.stroke(Stroke::new(1.0, theme::line()));
        }
        let modal = egui::Modal::new(Id::unique("vac-sheet")).area(area).backdrop_color(theme::scrim()).frame(frame).show(ctx, |ui| {
            ui.set_opacity(0.4 + 0.6 * t);
            ui.set_width(width - 48.0);
            ui.set_max_width(width - 48.0);
            ui.set_height(height - 44.0);
            ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| match self.tab {
                panel::PanelTab::Control => self.vacuum_main(ui, &v),
                _ => self.vacuum_page(ui, &v),
            });
        });
        if self.vac_busy(&v.id) {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if modal.should_close() {
            if self.tab != panel::PanelTab::Control && !modal.backdrop_response.clicked() {
                self.tab = panel::PanelTab::Control;
            } else {
                self.selected = None;
                self.vac.form = None;
            }
        }
    }

    fn vacuum_main(&mut self, ui: &mut Ui, v: &VacuumView) {
        let st = &v.status;
        let active = v.online && st.activity.is_active();
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(52.0, 52.0), Sense::hover());
            let t = ui.ctx().animate_bool_with_time(Id::unique(("vac-sheet-on", &v.id)), active, 0.18);
            widgets::paint_bubble(ui, r, Icon::Vacuum, t, theme::sun());
            ui.allocate_ui(vec2(ui.available_width() - 48.0, 52.0), |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.add(Label::new(RichText::new(&v.name).font(theme::bold(22.0))).truncate());
                    ui.label(RichText::new(v.room.as_deref().unwrap_or("Sans pièce")).size(13.5).color(theme::muted()));
                });
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::round_button(ui, Icon::Close, false).on_hover_text("Fermer (Échap)").clicked() {
                    self.selected = None;
                }
            });
        });
        ui.add_space(16.0);
        if !v.online {
            let why = self.problems_now.iter().find(|p| p.device == v.id && p.kind == homesmart::hub::ProblemKind::Offline).map(|p| p.detail.clone());
            egui::Frame::new().fill(theme::raised()).corner_radius(CornerRadius::same(14)).inner_margin(Margin::same(12)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("Hors ligne").strong().color(theme::text()));
                let text = why.or_else(|| v.error.clone()).unwrap_or_else(|| "Le robot ne répond pas : vérifie qu'il est allumé et sur le Wi-Fi.".into());
                ui.label(RichText::new(text).size(12.5).color(theme::muted()));
            });
            ui.add_space(12.0);
        }
        // Erreurs du robot (« Aspirateur en difficulté ») et autres problèmes en cours.
        for p in self.problems_now.iter().filter(|p| p.device == v.id && p.kind != homesmart::hub::ProblemKind::Offline).cloned().collect::<Vec<_>>() {
            let color = problems_ui::level_color(p.level);
            egui::Frame::new().fill(theme::alpha(color, 0.12)).corner_radius(CornerRadius::same(12)).inner_margin(Margin::same(12)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(p.kind.title()).strong().color(color));
                ui.label(RichText::new(&p.detail).size(12.5).color(theme::muted()));
            });
            ui.add_space(10.0);
        }
        // État : activité en grand, batterie, surface et durée.
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                let label = if v.online { st.activity.label() } else { "État inconnu" };
                ui.label(RichText::new(label).font(theme::bold(24.0)));
                let mut parts = Vec::new();
                if let Some(a) = st.area_m2.filter(|a| *a > 0.0) {
                    parts.push(format!("{} m²", widgets::fr(a, if a < 10.0 { 1 } else { 0 })));
                }
                if let Some(m) = st.clean_min.filter(|m| *m > 0) {
                    parts.push(homesmart::kinds::fmt_secs(m as u64 * 60));
                }
                if !parts.is_empty() {
                    let lead = if st.activity == Activity::Cleaning { "En cours : " } else { "Dernier nettoyage : " };
                    ui.label(RichText::new(format!("{lead}{}", parts.join(" en "))).size(13.0).color(theme::muted()));
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(b) = st.battery {
                    ui.label(RichText::new(format!("{b} %")).font(theme::light(26.0)));
                }
                battery(ui, st.battery, st.charging);
            });
        });
        ui.add_space(14.0);
        // Commandes.
        let busy = self.vac_busy(&v.id);
        let online = v.online;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = ((ui.available_width() - 4.0 * 76.0) / 3.0).max(0.0);
            let (icon, label, cmd) = match st.activity {
                Activity::Cleaning if v.caps.pause => (Icon::Pause, "Pause", Command::Pause),
                Activity::Paused => (Icon::Play, "Reprendre", Command::Start),
                _ => (Icon::Play, "Nettoyer", Command::Start),
            };
            if action(ui, icon, label, true, online, busy).clicked() {
                self.vac_cmd(&v.id, cmd);
            }
            let at_dock = st.activity.at_dock();
            if action(ui, Icon::House, "Base", false, online && v.caps.dock && !at_dock, false).on_hover_text("Renvoyer le robot à sa base").clicked() {
                self.vac_cmd(&v.id, Command::Dock);
            }
            let moving = matches!(st.activity, Activity::Cleaning | Activity::Returning | Activity::Paused);
            if action(ui, Icon::Stop, "Arrêter", false, online && v.caps.stop && moving, false).on_hover_text("Arrêter sur place").clicked() {
                self.vac_cmd(&v.id, Command::Stop);
            }
            if action(ui, Icon::Speaker, "Trouver", false, online && v.caps.locate, false).on_hover_text("Le robot émet un son").clicked() {
                self.vac_cmd(&v.id, Command::Locate);
            }
        });
        // Réglages du nettoyage, selon ce que le robot sait faire.
        let caps = v.caps.clone();
        if !caps.fans.is_empty() || !caps.modes.is_empty() || !caps.waters.is_empty() || !caps.patterns.is_empty() {
            ui.add_space(14.0);
            widgets::section_title(ui, "Nettoyage", None);
            if !caps.fans.is_empty() {
                ui.label(RichText::new("Puissance d'aspiration").size(13.0).color(theme::muted()));
                if let Some(f) = choice(ui, "fan", &caps.fans, st.fan.as_deref(), online) {
                    self.vac_cmd(&v.id, Command::Fan(f));
                }
                ui.add_space(6.0);
            }
            if !caps.modes.is_empty() {
                ui.label(RichText::new("Mode").size(13.0).color(theme::muted()));
                if let Some(m) = choice(ui, "mode", &caps.modes, st.mode.as_deref(), online) {
                    self.vac_cmd(&v.id, Command::Mode(m));
                }
                ui.add_space(6.0);
            }
            if !caps.waters.is_empty() {
                ui.label(RichText::new("Débit d'eau (serpillière)").size(13.0).color(theme::muted()));
                if let Some(w) = choice(ui, "water", &caps.waters, st.water.as_deref(), online) {
                    self.vac_cmd(&v.id, Command::Water(w));
                }
                ui.add_space(6.0);
            }
            if !caps.patterns.is_empty() {
                ui.label(RichText::new("Nettoyer autrement").size(13.0).color(theme::muted()));
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().button_padding = vec2(10.0, 7.0);
                    for c in &caps.patterns {
                        if ui.add_enabled(online, egui::Button::new(&c.label)).on_hover_text("Lance un nettoyage de cette façon").clicked() {
                            self.vac_cmd(&v.id, Command::Pattern(c.value.clone()));
                        }
                    }
                });
            }
        }
        // Usure.
        if !st.consumables.is_empty() {
            ui.add_space(14.0);
            widgets::section_title(ui, "Usure", None);
            // Une ligne par pièce : nom, jauge (alignée d'une ligne à l'autre), ce qui reste.
            for c in &st.consumables {
                let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::hover());
                let p = ui.painter();
                let text = match (c.percent, c.hours) {
                    (Some(pc), Some(h)) => format!("{} % · {} h", widgets::fr(pc, 0), widgets::fr(h, 0)),
                    (Some(pc), None) => format!("{} %", widgets::fr(pc, 0)),
                    (None, Some(h)) => format!("encore {} h", widgets::fr(h, 0)),
                    (None, None) => "—".into(),
                };
                p.text(row.left_center(), Align2::LEFT_CENTER, &c.label, egui::FontId::proportional(13.5), theme::text());
                p.text(row.right_center(), Align2::RIGHT_CENTER, text, egui::FontId::proportional(12.5), theme::muted());
                if let Some(pc) = c.percent {
                    let bar = Rect::from_min_size(pos2(row.right() - 196.0, row.center().y - 3.0), vec2(84.0, 6.0));
                    p.rect_filled(bar, CornerRadius::same(3), theme::raised());
                    let color = if pc < 10.0 { theme::crit() } else if pc < 25.0 { theme::warn() } else { theme::accent() };
                    p.rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * (pc as f32 / 100.0), bar.height())), CornerRadius::same(3), color);
                }
            }
        }
        ui.add_space(14.0);
        self.vacuum_automations(ui, v);
        ui.add_space(14.0);
        let programs = self.hub.schedules().iter().filter(|s| s.device == v.id && s.enabled).count();
        let value = match programs {
            0 => "aucun".to_string(),
            1 => "1 actif".to_string(),
            n => format!("{n} actifs"),
        };
        if widgets::nav_row(ui, "Programmes", &value).clicked() {
            self.tab = panel::PanelTab::Programs;
        }
        let conn = match v.family {
            Family::Tuya => "Tuya, nom, pièce".to_string(),
            f => format!("{}, nom, pièce", f.label().split(" (").next().unwrap_or("")),
        };
        if widgets::nav_row(ui, "Réglages", &conn).clicked() {
            self.tab = panel::PanelTab::Options;
        }
    }

    /// Automatismes : scénarios « maison vide », « mode absence », « quelqu'un arrive ».
    fn vacuum_automations(&mut self, ui: &mut Ui, v: &VacuumView) {
        widgets::section_title(ui, "Automatismes", None);
        let scenarios = self.hub.scenarios();
        let find = |trigger: fn(Trigger) -> bool, action: Action| scenarios.iter().find(|s| s.device == v.id && s.camera.is_empty() && trigger(s.trigger) && s.action == action).cloned();
        let analysed = self.hub.vision_settings().cameras.values().filter(|c| c.enabled).count();
        // Durée affichée : celle de l'automatisme existant, relue à l'ouverture du panneau.
        if self.vac.empty_for.as_deref() != Some(v.id.as_str()) {
            let current = find(|t| matches!(t, Trigger::Absence { .. }), Action::Clean).and_then(|s| match s.trigger {
                Trigger::Absence { minutes } => Some(minutes),
                _ => None,
            });
            (self.vac.empty_minutes, self.vac.empty_for) = (current.unwrap_or(30), Some(v.id.clone()));
        }
        let rows: [(&str, String, fn(Trigger) -> bool, Trigger, Action); 4] = [
            (
                "Nettoyer quand la maison est vide",
                if analysed == 0 { "personne vu sur les caméras (il faut activer l'analyse IA d'une caméra)".into() } else { format!("personne vu sur les {analysed} caméra(s) analysée(s)") },
                |t| matches!(t, Trigger::Absence { .. }),
                Trigger::Absence { minutes: self.vac.empty_minutes },
                Action::Clean,
            ),
            ("Nettoyer quand le mode absence est activé", "au départ de la maison".into(), |t| t == Trigger::AwayOn, Trigger::AwayOn, Action::Clean),
            ("Rentrer à la base quand quelqu'un arrive", "vu par n'importe quelle caméra analysée".into(), |t| t == Trigger::Arrival, Trigger::Arrival, Action::Dock),
            ("Rentrer à la base à la fin du mode absence", "au retour à la maison".into(), |t| t == Trigger::AwayOff, Trigger::AwayOff, Action::Dock),
        ];
        for (label, hint, is, trigger, act) in rows {
            let existing = find(is, act);
            let on = existing.as_ref().is_some_and(|s| s.enabled);
            let mut toggled = false;
            widgets::row(ui, label, Some(&hint), |ui| {
                toggled = widgets::toggle(ui, on, theme::accent(), true).clicked();
                if matches!(trigger, Trigger::Absence { .. }) {
                    let r = ui.add(egui::DragValue::new(&mut self.vac.empty_minutes).range(5..=600).speed(1.0).suffix(" min"));
                    // Durée modifiée sur un automatisme existant : remplacé par le nouveau (une seule
                    // fois, au relâchement).
                    let m = self.vac.empty_minutes;
                    if (r.drag_stopped() || r.lost_focus()) && !r.dragged() {
                        if let Some(s) = existing.as_ref().filter(|s| s.trigger != Trigger::Absence { minutes: m }) {
                            self.hub.remove_scenario(s.id, Source::User);
                            let _ = self.hub.add_scenario(Scenario { id: 0, trigger: Trigger::Absence { minutes: m }, ..s.clone() }, Source::User);
                        }
                    }
                }
            });
            if toggled {
                match &existing {
                    Some(s) => self.hub.remove_scenario(s.id, Source::User),
                    None => {
                        let s = Scenario { id: 0, enabled: true, camera: String::new(), trigger, device: v.id.clone(), action: act, during: None };
                        if let Err(e) = self.hub.add_scenario(s, Source::User) {
                            self.toast(format!("{e:#}"), true);
                        }
                    }
                }
            }
        }
    }

    /// Pages « Programmes » et « Réglages » du panneau.
    fn vacuum_page(&mut self, ui: &mut Ui, v: &VacuumView) {
        let title = if self.tab == panel::PanelTab::Programs { "Programmes" } else { "Réglages" };
        ui.horizontal(|ui| {
            if widgets::round_button(ui, Icon::Back, false).on_hover_text("Retour").clicked() {
                self.tab = panel::PanelTab::Control;
            }
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new(title).font(theme::bold(21.0)));
                ui.label(RichText::new(&v.name).size(12.5).color(theme::muted()));
            });
        });
        ui.add_space(16.0);
        ui.visuals_mut().selection.bg_fill = theme::accent();
        if self.tab == panel::PanelTab::Programs {
            self.programs_tab(ui, &v.id, true, theme::accent());
            return;
        }
        // Connexion.
        let family = match v.family {
            Family::Tuya => "Tuya / Smart Life, en local (clé importée une fois)",
            Family::Valetudo => "Valetudo, en local, sans compte",
            Family::Miio => "miIO, en local (jeton de l'appareil)",
        };
        widgets::section_title(ui, "Robot", None);
        let info = [("Connexion", family.to_string()), ("Adresse", v.host.clone().unwrap_or_else(|| "automatique".into())), ("Modèle", v.model.clone().unwrap_or_else(|| "pas encore lu".into()))];
        for (k, val) in info {
            ui.horizontal(|ui| {
                ui.label(RichText::new(k).size(13.0).color(theme::muted()));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add(Label::new(RichText::new(val).size(13.0)).truncate());
                });
            });
        }
        ui.add_space(12.0);
        if v.family == Family::Tuya {
            // Un robot Tuya est un appareil comme les autres : mêmes réglages (nom, pièce, clé, DP).
            if let Some(d) = self.hub.view(&v.id) {
                self.options_tab(ui, &d, theme::accent());
            }
            return;
        }
        self.vacuum_settings(ui, v);
    }

    /// Nom, pièce et connexion d'un robot hors Tuya ; suppression.
    fn vacuum_settings(&mut self, ui: &mut Ui, v: &VacuumView) {
        let Some(cfg) = self.hub.vacuum_config(&v.id) else { return };
        if self.vac.form.as_ref().is_none_or(|f| f.id != v.id) {
            let (host, port, user, password, token) = match &cfg.link {
                Link::Valetudo { host, port, user, password } => (host.clone(), *port, user.clone().unwrap_or_default(), password.clone().unwrap_or_default(), String::new()),
                Link::Miio { host, token, port } => (host.clone(), *port, String::new(), String::new(), token.clone()),
            };
            self.vac.form = Some(VacForm {
                id: v.id.clone(),
                name: cfg.name.clone(),
                room: cfg.room.clone().unwrap_or_default(),
                host,
                port: port.map(|p| p.to_string()).unwrap_or_default(),
                user,
                password,
                token,
                show_secret: false,
                confirm_delete: false,
                error: None,
            });
        }
        let Some(mut f) = self.vac.form.take() else { return };
        let mut save = false;
        let mut delete = false;
        widgets::section_title(ui, "Identité et connexion", None);
        egui::Grid::new(("vac-settings", &v.id)).num_columns(2).spacing([12.0, 9.0]).show(ui, |ui| {
            let w = ui.available_width().min(220.0);
            ui.label("Nom");
            ui.add(egui::TextEdit::singleline(&mut f.name).desired_width(w));
            ui.end_row();
            ui.label("Pièce");
            ui.add(egui::TextEdit::singleline(&mut f.room).hint_text("Salon").desired_width(w));
            ui.end_row();
            ui.label("Adresse IP");
            ui.add(egui::TextEdit::singleline(&mut f.host).desired_width(w));
            ui.end_row();
            ui.label("Port");
            ui.add(egui::TextEdit::singleline(&mut f.port).hint_text(if v.family == Family::Miio { "54321" } else { "80" }).desired_width(w));
            ui.end_row();
            match v.family {
                Family::Miio => {
                    ui.label("Jeton");
                    ui.add(egui::TextEdit::singleline(&mut f.token).password(!f.show_secret).desired_width(w));
                    ui.end_row();
                }
                _ => {
                    ui.label("Utilisateur");
                    ui.add(egui::TextEdit::singleline(&mut f.user).hint_text("si l'accès est protégé").desired_width(w));
                    ui.end_row();
                    ui.label("Mot de passe");
                    ui.add(egui::TextEdit::singleline(&mut f.password).password(!f.show_secret).desired_width(w));
                    ui.end_row();
                }
            }
        });
        ui.checkbox(&mut f.show_secret, if v.family == Family::Miio { "Afficher le jeton" } else { "Afficher le mot de passe" });
        if let Some(e) = &f.error {
            ui.label(RichText::new(e).color(theme::crit()));
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.add(widgets::primary_button("Enregistrer", theme::accent())).clicked() {
                save = true;
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if f.confirm_delete {
                    if ui.add(widgets::primary_button("Supprimer vraiment", theme::crit())).clicked() {
                        delete = true;
                    }
                    if ui.button("Non").clicked() {
                        f.confirm_delete = false;
                    }
                } else if ui.button("Supprimer l'aspirateur").clicked() {
                    f.confirm_delete = true;
                }
            });
        });
        if delete {
            match self.hub.remove_vacuum(&v.id, Source::User) {
                Ok(()) => {
                    self.selected = None;
                    self.toast(format!("« {} » supprimé", v.name), false);
                }
                Err(e) => self.toast(format!("{e:#}"), true),
            }
            return;
        }
        if save {
            let port = match f.port.trim() {
                "" => Ok(None),
                p => p.parse::<u16>().map(Some).map_err(|_| format!("port invalide : {p}")),
            };
            let opt = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
            let link = port.map(|port| match v.family {
                Family::Miio => Link::Miio { host: f.host.trim().to_string(), token: f.token.trim().to_lowercase(), port },
                _ => Link::Valetudo { host: f.host.trim().to_string(), port, user: opt(&f.user), password: opt(&f.user).and(Some(f.password.clone())) },
            });
            match link {
                Err(e) => f.error = Some(e),
                Ok(link) => match self.hub.update_vacuum(&v.id, VacuumPatch { name: Some(f.name.clone()), room: Some(f.room.clone()), link: Some(link) }, Source::User) {
                    Ok(_) => {
                        f.error = None;
                        self.toast("Réglages enregistrés", false);
                    }
                    Err(e) => f.error = Some(format!("{e:#}")),
                },
            }
        }
        self.vac.form = Some(f);
    }

    // ---- Ajout ----------------------------------------------------------------------------------

    /// Fenêtre « Ajouter un aspirateur robot ».
    pub(super) fn vacuum_add_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut f) = self.vac.add.take() else { return };
        let mut close = false;
        let mut add = false;
        let mut test = false;
        let mut scan = false;
        let mut open_cloud = false;
        if f.scanning && f.found.lock().unwrap().is_some() {
            f.scanning = false;
        }
        if f.testing && f.test.lock().unwrap().is_some() {
            f.testing = false;
        }
        let frame = egui::Frame::new().fill(theme::surface()).corner_radius(CornerRadius::same(22)).inner_margin(Margin::same(24));
        let modal = egui::Modal::new(Id::unique("add-vacuum")).backdrop_color(theme::scrim()).frame(frame).show(ctx, |ui| {
            ui.set_width(480.0);
            ScrollArea::vertical().max_height(ctx.content_rect().height() - 140.0).show(ui, |ui| {
                ui.label(RichText::new("Ajouter un aspirateur robot").font(theme::bold(21.0)));
                ui.add_space(4.0);
                ui.label(RichText::new("HomeSmart le pilote en local, sans compte, dès que le robot le permet.").size(13.0).color(theme::muted()));
                ui.add_space(12.0);
                if let Some(i) = widgets::segmented(ui, &["Valetudo", "Xiaomi · Roborock (miIO)", "Tuya · Smart Life"], Some(f.family), theme::accent()) {
                    f.family = i;
                    f.test.lock().unwrap().take();
                }
                ui.add_space(10.0);
                let hint = match f.family {
                    0 => "Valetudo est un micrologiciel libre qui remplace celui du constructeur (Dreame, Roborock, Xiaomi…) : le robot se pilote alors en HTTP local, sans aucun cloud. Il s'installe soi-même, souvent en ouvrant le robot (valetudo.cloud).",
                    1 => "Robots Xiaomi, Roborock et Dreame fabriqués avant 2022, appairés dans Mi Home. Il faut le jeton de l'appareil (32 caractères), à récupérer une fois avec un outil d'extraction depuis ton compte Xiaomi ; HomeSmart ne contacte aucun serveur.",
                    _ => "Les robots Tuya / Smart Life (catégorie « aspirateur ») s'importent avec les autres appareils, clé locale comprise ; ils apparaissent ensuite dans Maison et se pilotent en local.",
                };
                ui.label(RichText::new(hint).size(12.5).color(theme::muted()));
                ui.add_space(10.0);
                if f.family == 2 {
                    if ui.add(widgets::primary_button("Importer depuis Smart Life", theme::accent()).min_size(vec2(ui.available_width(), 40.0))).clicked() {
                        open_cloud = true;
                    }
                } else {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Sur le réseau").font(theme::bold(14.0)).color(theme::muted()));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if f.scanning {
                                ui.spinner();
                            } else if ui.small_button("Chercher").clicked() {
                                scan = true;
                            }
                        });
                    });
                    let found = f.found.lock().unwrap().clone();
                    match &found {
                        None if !f.scanning => {
                            ui.label(RichText::new("« Chercher » écoute le réseau 4 s (Valetudo, miIO) sans rien modifier.").size(12.5).color(theme::faint()));
                        }
                        Some(list) if list.is_empty() => {
                            ui.label(RichText::new("Aucun robot Valetudo ni miIO ne répond sur le réseau.").size(12.5).color(theme::faint()));
                        }
                        _ => {}
                    }
                    for r in found.iter().flatten() {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&r.host).monospace().size(12.5));
                            ui.add(Label::new(RichText::new(&r.label).size(12.5).color(theme::muted())).truncate());
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.small_button("Choisir").clicked() {
                                    f.family = if r.family == Family::Miio { 1 } else { 0 };
                                    f.host = r.host.clone();
                                    if f.name.trim().is_empty() && r.family == Family::Valetudo {
                                        f.name = r.label.clone();
                                    }
                                }
                            });
                        });
                    }
                    ui.add_space(12.0);
                    egui::Grid::new("add-vac-grid").num_columns(2).spacing([12.0, 9.0]).show(ui, |ui| {
                        let w = 300.0;
                        ui.label("Adresse IP");
                        ui.add(egui::TextEdit::singleline(&mut f.host).hint_text("192.168.1.59").desired_width(w));
                        ui.end_row();
                        if f.family == 1 {
                            ui.label("Jeton");
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(&mut f.token).password(!f.show_secret).hint_text("32 caractères hexadécimaux").desired_width(w - 100.0));
                                let n = f.token.trim().len();
                                ui.label(RichText::new(format!("{n}/32")).size(12.0).color(if n == 32 { theme::accent() } else { theme::faint() }));
                                ui.checkbox(&mut f.show_secret, "voir");
                            });
                            ui.end_row();
                        }
                        ui.label("Nom");
                        ui.add(egui::TextEdit::singleline(&mut f.name).hint_text("Aspirateur").desired_width(w));
                        ui.end_row();
                        ui.label("Pièce");
                        ui.add(egui::TextEdit::singleline(&mut f.room).hint_text("Salon").desired_width(w));
                        ui.end_row();
                    });
                    egui::CollapsingHeader::new(RichText::new("Avancé : port, accès protégé").size(13.0).color(theme::muted())).id_salt("add-vac-adv").show(ui, |ui| {
                        egui::Grid::new("add-vac-grid-2").num_columns(2).spacing([12.0, 9.0]).show(ui, |ui| {
                            ui.label("Port");
                            ui.add(egui::TextEdit::singleline(&mut f.port).hint_text(if f.family == 1 { "54321" } else { "80" }).desired_width(120.0));
                            ui.end_row();
                            if f.family == 0 {
                                ui.label("Utilisateur");
                                ui.add(egui::TextEdit::singleline(&mut f.user).hint_text("si l'accès HTTP est protégé").desired_width(220.0));
                                ui.end_row();
                                ui.label("Mot de passe");
                                ui.add(egui::TextEdit::singleline(&mut f.password).password(!f.show_secret).desired_width(220.0));
                                ui.end_row();
                            }
                        });
                    });
                    // Résultat de l'essai.
                    match f.test.lock().unwrap().as_ref() {
                        Some(Ok((model, st))) => {
                            ui.add_space(6.0);
                            let model = if model.is_empty() { "Robot".to_string() } else { model.clone() };
                            ui.label(RichText::new(format!("Connexion réussie : {model} — {}", st.summary())).color(theme::accent()));
                        }
                        Some(Err(e)) => {
                            ui.add_space(6.0);
                            ui.label(RichText::new(format!("Essai échoué : {e}")).color(theme::crit()));
                        }
                        None => {}
                    }
                }
                ui.add_space(12.0);
                egui::Frame::new().fill(theme::raised()).corner_radius(CornerRadius::same(12)).inner_margin(Margin::same(12)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new("Dreame récents (application Dreamehome)").strong());
                    ui.label(
                        RichText::new(
                            "Ces robots n'ont aucune commande locale : ils ne répondent qu'au cloud de Dreame. Pour les piloter ici sans compte, il faut installer Valetudo sur le robot. \
                             Le passage par le cloud Dreamehome n'est pas proposé : son accès imite l'application Dreame et peut cesser du jour au lendemain.",
                        )
                        .size(12.0)
                        .color(theme::muted()),
                    );
                });
                if let Some(e) = &f.error {
                    ui.add_space(6.0);
                    ui.label(RichText::new(e).color(theme::crit()));
                }
            });
            ui.add_space(14.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if f.family != 2 {
                    if ui.add(widgets::primary_button("Ajouter", theme::accent()).min_size(vec2(100.0, 36.0))).clicked() {
                        add = true;
                    }
                    let label = if f.testing { "Essai…" } else { "Tester" };
                    if ui.add_enabled(!f.testing, egui::Button::new(label).min_size(vec2(0.0, 36.0))).clicked() {
                        test = true;
                    }
                }
                if ui.add(egui::Button::new("Annuler").min_size(vec2(0.0, 36.0))).clicked() {
                    close = true;
                }
            });
        });
        if f.scanning || f.testing {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        if modal.should_close() {
            close = true;
        }
        if open_cloud {
            self.cloud = Some(super::cloud_import::CloudImport::new());
            return;
        }
        if scan {
            f.scanning = true;
            f.found.lock().unwrap().take();
            let slot = f.found.clone();
            self.hub.log(Level::Info, Source::User, "", "Recherche d'aspirateurs robots sur le réseau (Valetudo, miIO)");
            self.rt.spawn(async move {
                let list = vacuum::scan::scan(Duration::from_secs(4)).await;
                *slot.lock().unwrap() = Some(list);
            });
        }
        if test {
            match f.link() {
                Err(e) => f.error = Some(e),
                Ok(link) => {
                    f.error = None;
                    f.testing = true;
                    f.test.lock().unwrap().take();
                    let slot = f.test.clone();
                    self.rt.spawn(async move {
                        let r = homesmart::hub::test_vacuum_link(&link).await.map_err(|e| format!("{e:#}"));
                        *slot.lock().unwrap() = Some(r);
                    });
                }
            }
        }
        if add {
            match f.link() {
                Err(e) => f.error = Some(e),
                Ok(link) => {
                    let model = f.test.lock().unwrap().as_ref().and_then(|r| r.as_ref().ok()).map(|(m, _)| m.clone()).filter(|m| !m.is_empty());
                    let cfg = VacuumConfig { id: String::new(), name: f.name.trim().to_string(), room: Some(f.room.trim().to_string()).filter(|r| !r.is_empty()), link, model };
                    match self.hub.add_vacuum(cfg, Source::User) {
                        Ok(v) => {
                            self.toast(format!("« {} » ajouté", v.name), false);
                            self.view = View::Devices;
                            self.select(&v.id);
                            return;
                        }
                        Err(e) => f.error = Some(format!("{e:#}")),
                    }
                }
            }
        }
        if !close {
            self.vac.add = Some(f);
        }
    }
}
