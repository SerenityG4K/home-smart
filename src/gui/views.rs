//! Vues globales (énergie, calendrier, alertes) et fenêtres annexes.

use super::panel::{SchedForm, Target};
use super::icons::Icon;
use super::{autostart, theme, widgets, App};
use chrono::{Datelike, Local, Timelike};
use eframe::egui::{self, pos2, vec2, Align, Align2, Color32, CornerRadius, FontId, Id, Layout, Margin, Rect, RichText, ScrollArea, Sense, Stroke, Ui};
use homesmart::automation::{self, Action};
use homesmart::hub::{DeviceView, Level, Source};
use serde_json::{Map, Value};
use widgets::fr;

const PALETTE: [Color32; 7] = [
    theme::AMBER,
    theme::GREEN,
    theme::BLUE,
    theme::VIOLET,
    Color32::from_rgb(0xf4, 0x72, 0xb6),
    Color32::from_rgb(0x22, 0xd3, 0xee),
    Color32::from_rgb(0xfb, 0x92, 0x3c),
];

/// Filtres du journal.
pub struct JournalFilter {
    /// 0 tout, 1 actions, 2 attention, 3 sécurité.
    pub level: usize,
    /// Origines affichées (vide = toutes).
    pub sources: Vec<Source>,
    pub device: Option<String>,
    pub search: String,
    pub limit: usize,
    /// Dernier résultat : (dernière entrée du journal, filtres) → lignes. Sans lui, tout le
    /// journal (jusqu'à 20 000 entrées) serait refiltré à chaque image.
    cache: Option<(JournalKey, std::sync::Arc<Vec<homesmart::hub::Alert>>)>,
}

type JournalKey = (u64, usize, Vec<Source>, Option<String>, String, usize);

impl Default for JournalFilter {
    fn default() -> Self {
        JournalFilter { level: 0, sources: Vec::new(), device: None, search: String::new(), limit: 300, cache: None }
    }
}

fn device_color(devices: &[Target], id: &str) -> Color32 {
    PALETTE[devices.iter().position(|d| d.id == id).unwrap_or(0) % PALETTE.len()]
}

/// « s'allume », « nettoie »…
fn verb_3s(a: Action) -> &'static str {
    match a {
        Action::On => "s'allume",
        Action::Off => "s'éteint",
        Action::Clean => "nettoie",
        Action::Dock => "rentre à sa base",
    }
}

impl App {
    // ---- Calendrier -----------------------------------------------------------------------

    pub(super) fn calendar_view(&mut self, ui: &mut Ui, devices: &[DeviceView]) {
        let schedules = self.hub.schedules();
        let devices = &self.targets(devices)[..];
        widgets::page_header(ui, "Programmes", |ui| {
            let b = ui.add_enabled_ui(!devices.is_empty(), |ui| widgets::round_button(ui, Icon::Plus, true)).inner;
            if b.on_hover_text("Nouveau programme").clicked() && self.sched_form.is_none() {
                self.sched_form = devices.first().map(|d| SchedForm::new(&d.id, d.vacuum));
            }
        });
        // Combien sont actifs, et ce qui va se passer ensuite.
        let active = schedules.iter().filter(|s| s.enabled).count();
        let now = Local::now().naive_local();
        let next = schedules.iter().filter(|s| s.enabled).filter_map(|s| s.next_event(now).map(|e| (e, s))).min_by_key(|(e, _)| e.at);
        let strong = match active {
            0 => "Aucun programme actif".to_string(),
            1 => "1 programme actif".to_string(),
            n => format!("{n} programmes actifs"),
        };
        let rest = next
            .map(|(e, s)| {
                let name = devices.iter().find(|d| d.id == s.device).map(|d| d.name.as_str()).unwrap_or("?");
                let day = match (e.at.date() - now.date()).num_days() {
                    0 => "aujourd'hui".to_string(),
                    1 => "demain".to_string(),
                    _ => automation::DAY_NAMES[e.at.weekday().num_days_from_monday() as usize].to_string(),
                };
                format!(" · prochain : {name} {} {day} à {}", verb_3s(e.action), automation::fmt_hm(e.at.time()))
            })
            .unwrap_or_default();
        widgets::lede(ui, &strong, &rest);
        ui.add_space(16.0);
        let wide = ui.available_width() >= 900.0;
        ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            let grid = |this: &mut Self, ui: &mut Ui| {
                widgets::panel_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new("Semaine type").font(theme::bold(15.0)));
                    ui.add_space(4.0);
                    this.week_grid(ui, devices, &schedules);
                });
            };
            let list = |this: &mut Self, ui: &mut Ui| {
                if this.sched_form.is_some() {
                    this.schedule_form_ui(ui, Some(devices), theme::accent());
                    ui.add_space(12.0);
                }
                if schedules.is_empty() {
                    ui.label(RichText::new("Aucun programme pour l'instant. Exemple : le chauffage de 6 h 30 à 8 h en semaine.").color(theme::muted()));
                }
                for d in devices {
                    let list: Vec<_> = schedules.iter().filter(|s| s.device == d.id).cloned().collect();
                    if list.is_empty() {
                        continue;
                    }
                    widgets::section_title(ui, &d.name, None);
                    for s in &list {
                        this.schedule_row(ui, s, None, theme::accent());
                        ui.add_space(6.0);
                    }
                    ui.add_space(8.0);
                }
                ui.label(RichText::new("Exécutés par HomeSmart : laisse-le ouvert (même réduit) et active « Lancer à l'ouverture de session » dans les préférences.").size(12.0).color(theme::faint()));
            };
            if wide {
                let side = (ui.available_width() * 0.42).clamp(340.0, 520.0);
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width() - side - 18.0);
                        grid(self, ui);
                    });
                    ui.add_space(10.0);
                    ui.vertical(|ui| {
                        ui.set_width(side);
                        list(self, ui);
                    });
                });
            } else {
                grid(self, ui);
                ui.add_space(14.0);
                list(self, ui);
            }
        });
    }

    fn week_grid(&mut self, ui: &mut Ui, devices: &[Target], schedules: &[automation::Schedule]) {
        let width = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(vec2(width, 24.0 * 15.0 + 26.0), Sense::click());
        let p = ui.painter();
        let left = 40.0;
        let grid = Rect::from_min_max(rect.min + vec2(left, 22.0), rect.max - vec2(0.0, 4.0));
        let col = grid.width() / 7.0;
        let hour_h = grid.height() / 24.0;
        let now = Local::now();
        let today = now.weekday().num_days_from_monday() as usize;
        for i in 0..7 {
            let x = grid.left() + col * i as f32;
            let c = if i == today { theme::accent() } else { theme::muted() };
            p.text(pos2(x + col / 2.0, rect.top() + 9.0), Align2::CENTER_CENTER, automation::DAY_NAMES[i], FontId::proportional(12.0), c);
            p.rect_filled(Rect::from_min_size(pos2(x + 1.0, grid.top()), vec2(col - 2.0, grid.height())), CornerRadius::same(6), theme::raised().gamma_multiply(0.45));
        }
        for h in (0..=24).step_by(3) {
            let y = grid.top() + hour_h * h as f32;
            p.line_segment([pos2(grid.left(), y), pos2(grid.right(), y)], Stroke::new(1.0, theme::line().gamma_multiply(0.6)));
            p.text(pos2(grid.left() - 6.0, y), Align2::RIGHT_CENTER, format!("{h} h"), FontId::proportional(10.5), theme::faint());
        }
        let y_of = |h: u32, m: u32| grid.top() + hour_h * (h as f32 + m as f32 / 60.0);
        let mut hits: Vec<(Rect, u64)> = Vec::new();
        for s in schedules {
            let color = device_color(devices, &s.device);
            let color = if s.enabled { color } else { color.gamma_multiply(0.3) };
            let Some(start) = automation::parse_hm(&s.start) else { continue };
            for day in (0..7).filter(|d| s.days[*d]) {
                let x0 = grid.left() + col * day as f32 + 4.0;
                let x1 = x0 + col - 8.0;
                match s.end.as_deref().and_then(automation::parse_hm) {
                    Some(end) => {
                        let mut seg = |d: usize, y0: f32, y1: f32| {
                            let x0 = grid.left() + col * d as f32 + 4.0;
                            let r = Rect::from_min_max(pos2(x0, y0), pos2(x0 + col - 8.0, y1.max(y0 + 3.0)));
                            let fill = if matches!(s.action, Action::On | Action::Clean) { color.gamma_multiply(0.75) } else { color.gamma_multiply(0.25) };
                            p.rect_filled(r, CornerRadius::same(4), fill);
                            hits.push((r, s.id));
                        };
                        if end > start {
                            seg(day, y_of(start.hour(), start.minute()), y_of(end.hour(), end.minute()));
                        } else {
                            seg(day, y_of(start.hour(), start.minute()), grid.bottom());
                            seg((day + 1) % 7, grid.top(), y_of(end.hour(), end.minute()));
                        }
                    }
                    None => {
                        let y = y_of(start.hour(), start.minute());
                        p.line_segment([pos2(x0, y), pos2(x1, y)], Stroke::new(2.0, color));
                        let c = pos2(x0 + 6.0, y);
                        if matches!(s.action, Action::On | Action::Clean) {
                            p.circle_filled(c, 4.5, color);
                        } else {
                            p.circle_filled(c, 4.5, theme::bg());
                            p.circle_stroke(c, 4.5, Stroke::new(1.8, color));
                        }
                        hits.push((Rect::from_center_size(pos2((x0 + x1) / 2.0, y), vec2(col - 8.0, 10.0)), s.id));
                    }
                }
            }
        }
        // Heure actuelle.
        let y = y_of(now.hour(), now.minute());
        let x = grid.left() + col * today as f32;
        p.line_segment([pos2(x + 2.0, y), pos2(x + col - 2.0, y)], Stroke::new(2.0, theme::crit()));
        p.circle_filled(pos2(x + 2.0, y), 3.5, theme::crit());

        if let Some(pos) = resp.hover_pos() {
            if let Some((_, id)) = hits.iter().rev().find(|(r, _)| r.contains(pos)) {
                if let Some(s) = schedules.iter().find(|s| s.id == *id) {
                    let name = devices.iter().find(|d| d.id == s.device).map(|d| d.name.as_str()).unwrap_or("?");
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    let text = format!("{name} : {}\n(cliquer pour modifier)", s.summary());
                    if resp.clicked() {
                        let vacuum = devices.iter().any(|d| d.id == s.device && d.vacuum);
                        self.sched_form = Some(SchedForm::from(s, vacuum));
                    }
                    resp.on_hover_text_at_pointer(text);
                }
            }
        }
    }

    // ---- Journal (onglet « Alertes ») ----------------------------------------------------

    pub(super) fn alerts_view(&mut self, ui: &mut Ui, devices: &[DeviceView]) {
        let latest = self.hub.alerts(1).first().map_or(0, |e| e.id);
        self.alerts_seen = latest;
        let journal_path = self.hub.journal_path();
        let total = self.hub.journal_len();
        let f = &mut self.journal;
        widgets::page_header(ui, "Activité", |ui| {
            ui.add(egui::TextEdit::singleline(&mut f.search).hint_text("Rechercher…").desired_width(220.0).margin(vec2(10.0, 7.0)));
        });
        ui.add_space(12.0);
        ui.horizontal_wrapped(|ui| {
            if let Some(i) = widgets::segmented(ui, &["Tout", "Actions", "Attention", "Sécurité"], Some(f.level), theme::accent()) {
                f.level = i;
            }
            ui.add_space(6.0);
            // Filtres moins courants (appareil, origine) : rangés dans un menu.
            let filtered = f.device.is_some() || !f.sources.is_empty();
            let label = if filtered { "Filtres (actifs)" } else { "Filtres" };
            let resp = ui.add(egui::Button::new(RichText::new(label).color(if filtered { theme::accent() } else { theme::text() })).min_size(vec2(0.0, 34.0)));
            egui::Popup::menu(&resp).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                ui.set_min_width(240.0);
                ui.label(RichText::new("Appareil").size(12.5).color(theme::muted()));
                let current = f.device.as_ref().and_then(|id| devices.iter().find(|d| &d.id == id)).map(|d| d.name.clone()).unwrap_or_else(|| "Tous les appareils".into());
                egui::ComboBox::from_id_salt("journal-dev").selected_text(current).width(220.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut f.device, None, "Tous les appareils");
                    for d in devices {
                        ui.selectable_value(&mut f.device, Some(d.id.clone()), &d.name);
                    }
                });
                ui.add_space(6.0);
                ui.label(RichText::new("Origine").size(12.5).color(theme::muted()));
                for src in Source::ALL {
                    let mut on = f.sources.is_empty() || f.sources.contains(&src);
                    if ui.checkbox(&mut on, src.label()).changed() {
                        if f.sources.is_empty() {
                            f.sources = Source::ALL.into_iter().filter(|s| *s != src).collect();
                        } else if on {
                            f.sources.push(src);
                        } else {
                            f.sources.retain(|s| *s != src);
                        }
                        if f.sources.len() == Source::ALL.len() {
                            f.sources.clear();
                        }
                    }
                }
                if filtered && ui.button("Tout afficher").clicked() {
                    (f.device, f.sources) = (None, Vec::new());
                }
            });
        });
        ui.add_space(10.0);

        let search = f.search.trim().to_lowercase();
        let (level, sources, device) = (f.level, f.sources.clone(), f.device.clone());
        let key: JournalKey = (latest, level, sources.clone(), device.clone(), search.clone(), f.limit);
        let rows = match &f.cache {
            Some((k, rows)) if *k == key => rows.clone(),
            _ => {
                let rows = std::sync::Arc::new(self.hub.journal_query(f.limit, |e| {
                    let lvl_ok = match level {
                        1 => e.level == Level::Info,
                        2 => e.level == Level::Warning,
                        3 => e.level == Level::Critical,
                        _ => true,
                    };
                    lvl_ok
                        && (sources.is_empty() || sources.contains(&e.source))
                        && device.as_ref().is_none_or(|d| &e.device == d)
                        && (search.is_empty() || e.message.to_lowercase().contains(&search) || e.device_name.to_lowercase().contains(&search))
                }));
                f.cache = Some((key, rows.clone()));
                rows
            }
        };
        ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            widgets::cap_width(ui, 1000.0);
            if rows.is_empty() {
                ui.label(RichText::new("Rien ne correspond.").color(theme::faint()));
            }
            let today = Local::now().date_naive();
            let mut current_day = None;
            for e in rows.iter() {
                let day = e.at.date_naive();
                if current_day != Some(day) {
                    current_day = Some(day);
                    let label = match (today - day).num_days() {
                        0 => "Aujourd'hui".to_string(),
                        1 => "Hier".to_string(),
                        _ => format!("{} {}", automation::DAY_NAMES[day.weekday().num_days_from_monday() as usize], day.format("%d/%m/%Y")),
                    };
                    ui.add_space(10.0);
                    widgets::section_title(ui, &label, None);
                }
                let tint = match e.level {
                    Level::Critical => Some(theme::crit()),
                    Level::Warning => Some(theme::warn()),
                    Level::Info => None,
                };
                let frame = egui::Frame::new().inner_margin(Margin::symmetric(10, 7)).corner_radius(CornerRadius::same(10)).fill(tint.map_or(Color32::TRANSPARENT, |c| theme::alpha(c, 0.10)));
                let r = frame.show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(e.at.format("%H:%M").to_string()).size(12.5).color(theme::faint())).on_hover_text(e.at.format("%H:%M:%S").to_string());
                        if let Some(c) = tint {
                            widgets::dot(ui, c);
                        }
                        if !e.device_name.is_empty() {
                            ui.label(RichText::new(&e.device_name).strong());
                        }
                        ui.label(&e.message);
                        ui.label(RichText::new(format!("· {}", e.source.label())).size(12.0).color(theme::faint()));
                    });
                });
                if tint.is_none() {
                    let b = r.response.rect;
                    ui.painter().line_segment([pos2(b.left() + 10.0, b.bottom()), pos2(b.right() - 10.0, b.bottom())], Stroke::new(1.0, theme::line()));
                }
            }
            if rows.len() >= f.limit {
                ui.add_space(6.0);
                if ui.button("Afficher plus").clicked() {
                    f.limit += 300;
                }
            }
            ui.add_space(10.0);
            ui.label(RichText::new(format!("{} entrée(s) sur {total} · journal : {}", rows.len(), journal_path.display())).size(12.0).color(theme::faint()));
        });
    }

    // ---- Fenêtres --------------------------------------------------------------------------

    pub(super) fn prefs_dialog(&mut self, ctx: &egui::Context) {
        if !self.prefs_open {
            return;
        }
        let mut prefs = self.hub.prefs();
        let frame = egui::Frame::new().fill(theme::surface()).corner_radius(CornerRadius::same(22)).inner_margin(Margin::same(24));
        let modal = egui::Modal::new(Id::unique("prefs")).backdrop_color(theme::scrim()).frame(frame).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.label(RichText::new("Préférences").font(theme::bold(21.0)));
            ui.add_space(14.0);
            ScrollArea::vertical().max_height(ctx.content_rect().height() - 170.0).show(ui, |ui| {

            widgets::section_title(ui, "Apparence", None);
            widgets::row(ui, "Thème", Some("sombre, ou clair pour une pièce lumineuse"), |ui| {
                if let Some(i) = widgets::segmented(ui, &["Sombre", "Clair"], Some(prefs.light_theme as usize), theme::accent()) {
                    prefs.light_theme = i == 1;
                    theme::set_light(ctx, prefs.light_theme);
                    self.hub.set_prefs(prefs.clone());
                }
            });
            ui.add_space(12.0);

            widgets::section_title(ui, "Au démarrage", None);
            let mut auto = self.autostart;
            widgets::row(ui, "Lancer à l'ouverture de session", Some("démarre réduit : programmes et garde-fous toujours actifs"), |ui| {
                if widgets::toggle(ui, auto, theme::accent(), true).clicked() {
                    auto = !auto;
                }
            });
            if auto != self.autostart {
                match autostart::set(auto) {
                    Ok(()) => {
                        self.autostart = auto;
                        self.hub.log(Level::Info, Source::User, "", if auto { "Lancement automatique activé" } else { "Lancement automatique désactivé" });
                        self.toast(if auto { "HomeSmart se lancera à l'ouverture de session" } else { "Lancement automatique désactivé" }, false);
                    }
                    Err(e) => self.toast(format!("Lancement automatique : {e:#}"), true),
                }
            }
            widgets::row(ui, "Notifications du système", Some("chute, appel à l'aide, alarme… même fenêtre réduite"), |ui| {
                if widgets::toggle(ui, prefs.os_notifications, theme::accent(), true).clicked() {
                    prefs.os_notifications = !prefs.os_notifications;
                    self.hub.set_prefs(prefs.clone());
                }
            });
            ui.add_space(12.0);

            widgets::section_title(ui, "Mesures et énergie", None);
            let options = [1.0, 2.0, 5.0, 10.0];
            let current = options.iter().position(|o| (*o - prefs.meter_interval_s).abs() < 0.01);
            widgets::row(ui, "Rafraîchir les mesures", Some("2 s : bon équilibre ; 1 s sollicite davantage les prises"), |ui| {
                if let Some(i) = widgets::segmented(ui, &["1 s", "2 s", "5 s", "10 s"], current, theme::accent()) {
                    prefs.meter_interval_s = options[i];
                    self.hub.set_prefs(prefs.clone());
                }
            });
            widgets::row(ui, "Prix du kWh", Some("TTC, pour estimer le coût"), |ui| {
                if ui.add(egui::DragValue::new(&mut prefs.price_kwh).range(0.0..=2.0).speed(0.001).fixed_decimals(4).suffix(" €")).changed() {
                    self.hub.set_prefs(prefs.clone());
                }
            });
            ui.add_space(12.0);

            widgets::section_title(ui, "Caméras (vidéo H.265)", None);
            self.ffmpeg_panel(ui);
            ui.add_space(10.0);
            egui::CollapsingHeader::new(RichText::new("Fichiers").size(13.5).color(theme::muted())).id_salt("prefs-files").show(ui, |ui| {
                ui.label(RichText::new(format!("Configuration : {}", self.config_path.display())).size(12.0).color(theme::faint()));
                ui.label(RichText::new(format!("Énergie : {}", self.config_path.with_file_name("energy.json").display())).size(12.0).color(theme::faint()));
                ui.label(RichText::new(format!("Journal : {}", self.hub.journal_path().display())).size(12.0).color(theme::faint()));
            });
            });
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(widgets::primary_button("Fermer", theme::accent()).min_size(vec2(90.0, 36.0))).clicked() {
                    self.prefs_open = false;
                }
            });
        });
        if modal.should_close() {
            self.prefs_open = false;
        }
    }

    /// Confirmation d'allumage d'un appareil protégé.
    pub(super) fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some((id, name, dps)) = self.confirm.take() else { return };
        let mut keep = true;
        let safety = self.hub.view(&id).map(|v| v.safety).unwrap_or_default();
        let frame = egui::Frame::new().fill(theme::surface()).stroke(Stroke::new(1.0, theme::warn().gamma_multiply(0.6))).corner_radius(CornerRadius::same(16)).inner_margin(Margin::same(22));
        let modal = egui::Modal::new(Id::unique("confirm-on")).frame(frame).show(ctx, |ui| {
            ui.set_width(400.0);
            ui.label(RichText::new(format!("Allumer « {name} » ?")).size(18.0).strong());
            ui.add_space(6.0);
            ui.label(RichText::new("Cet appareil est marqué comme à risque. Vérifie qu'il peut fonctionner sans danger.").color(theme::muted()));
            ui.add_space(6.0);
            if let Some(m) = safety.max_on_minutes {
                ui.label(format!("•  Il s'éteindra automatiquement au bout de {}.", automation::fmt_minutes(m)));
            }
            if let Some(w) = safety.max_power_w {
                ui.label(format!("•  Coupure au-delà de {} W.", fr(w, 0)));
            }
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(widgets::primary_button("Allumer", theme::accent()).min_size(vec2(90.0, 32.0))).clicked() {
                    for (k, v) in &dps {
                        self.set(&id, k, v.clone());
                    }
                    keep = false;
                }
                if ui.button("Annuler").clicked() {
                    keep = false;
                    self.hub.log(Level::Info, Source::User, &id, "Allumage annulé (confirmation refusée)");
                }
            });
        });
        if modal.should_close() && keep {
            keep = false;
            self.hub.log(Level::Info, Source::User, &id, "Allumage annulé (confirmation refusée)");
        }
        if keep {
            self.confirm = Some((id, name, dps));
        }
    }

    /// Fermeture : proposer de réduire si des programmes ou garde-fous sont actifs.
    pub(super) fn close_guard(&mut self, ctx: &egui::Context, devices: &[DeviceView]) {
        let active = self.hub.schedules().iter().any(|s| s.enabled) || devices.iter().any(|d| d.safety.enabled);
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close && active {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.close_dialog = true;
        }
        if !self.close_dialog {
            return;
        }
        let frame = egui::Frame::new().fill(theme::surface()).stroke(Stroke::new(1.0, theme::line())).corner_radius(CornerRadius::same(16)).inner_margin(Margin::same(22));
        let modal = egui::Modal::new(Id::unique("close")).frame(frame).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(RichText::new("Garder HomeSmart actif ?").size(18.0).strong());
            ui.add_space(6.0);
            ui.label(
                RichText::new("Des programmes ou des garde-fous sont actifs : ils ne fonctionnent que si HomeSmart reste ouvert. La minuterie des prises continue, elle, sans HomeSmart.")
                    .color(theme::muted()),
            );
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(widgets::primary_button("Réduire", theme::accent()).min_size(vec2(90.0, 32.0))).clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    self.close_dialog = false;
                }
                if ui.button("Quitter quand même").clicked() {
                    self.allow_close = true;
                    self.close_dialog = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
        if modal.should_close() {
            self.close_dialog = false;
        }
    }

    /// Allumage depuis l'interface : confirmation si l'appareil le demande.
    pub(super) fn request_power(&mut self, d: &DeviceView, dps: Vec<(String, bool)>) {
        let turns_on = dps.iter().any(|(_, v)| *v);
        if turns_on && d.safety.enabled && d.safety.confirm_on {
            let m: Map<String, Value> = dps.into_iter().map(|(k, v)| (k, Value::Bool(v))).collect();
            self.confirm = Some((d.id.clone(), d.name.clone(), m));
            return;
        }
        if turns_on && d.safety.forbids_now(Local::now().time()) {
            let r = d.safety.forbidden.as_ref().map(|r| r.label()).unwrap_or_default();
            self.toast(format!("{} : allumage interdit par les garde-fous ({r})", d.name), true);
            self.hub.log(Level::Warning, Source::User, &d.id, format!("Refusé : allumage interdit par les garde-fous ({r})"));
            return;
        }
        for (k, v) in dps {
            self.set(&d.id, &k, Value::Bool(v));
        }
    }
}
