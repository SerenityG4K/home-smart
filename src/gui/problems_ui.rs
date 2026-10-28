//! Page « Problèmes » : ce qui arrive aux appareils, aux caméras et à ce PC sans qu'on l'ait
//! demandé. En cours d'abord (avec la cause probable), puis l'historique de la semaine ; en démo,
//! de quoi provoquer chaque panne sur les appareils simulés.

use super::icons::{self, Icon};
use super::{theme, widgets, App, View};
use chrono::Local;
use eframe::egui::{self, pos2, vec2, Align, Color32, CornerRadius, CursorIcon, Layout, Margin, Rect, RichText, ScrollArea, Sense, Stroke, Ui};
use homesmart::hub::{DeviceView, Level, Problem, ProblemKind, Source};
use homesmart::problems;
use std::time::Duration;

/// Couleur d'une gravité.
pub(super) fn level_color(l: Level) -> Color32 {
    match l {
        Level::Critical => theme::crit(),
        Level::Warning => theme::warn(),
        Level::Info => theme::accent(),
    }
}

/// Le plus grave d'abord.
pub(super) fn rank(l: Level) -> u8 {
    match l {
        Level::Info => 0,
        Level::Warning => 1,
        Level::Critical => 2,
    }
}

/// « depuis 12 min », ou « à 20:42 » pour un événement (« · 3 fois » s'il se répète).
fn when(p: &Problem) -> String {
    let now = Local::now();
    let mut s = if p.kind.is_event() {
        if p.last.date_naive() == now.date_naive() {
            p.last.format("à %H:%M").to_string()
        } else {
            p.last.format("le %d/%m à %H:%M").to_string()
        }
    } else {
        let secs = (now - p.since).num_seconds().max(0) as u64;
        if secs < 60 {
            "depuis moins d'une minute".to_string()
        } else {
            format!("depuis {}", homesmart::kinds::fmt_secs(secs - secs % 60))
        }
    };
    if p.count > 1 {
        s += &format!(" · {} fois", p.count);
    }
    s
}

fn icon_of(k: ProblemKind) -> Icon {
    match k {
        ProblemKind::CameraOffline | ProblemKind::StreamLost => Icon::Camera,
        ProblemKind::VisionStopped => Icon::Person,
        ProblemKind::ListenStopped => Icon::Speaker,
        ProblemKind::Voltage | ProblemKind::LowPower => Icon::Bolt,
        ProblemKind::HostShutdown => Icon::Plug,
        ProblemKind::VacuumError => Icon::Vacuum,
        _ => Icon::Alert,
    }
}

fn plural(n: usize, word: &str) -> String {
    if n > 1 {
        format!("{n} {word}s")
    } else {
        format!("{n} {word}")
    }
}

impl App {
    /// Bandeau de la page Maison : problèmes en cours (sauf simples informations) ; un clic ouvre la page.
    pub(super) fn problems_banner(&mut self, ui: &mut Ui) {
        let mut list: Vec<&Problem> = self.problems_now.iter().filter(|p| p.level != Level::Info).collect();
        if list.is_empty() {
            return;
        }
        list.sort_by_key(|p| std::cmp::Reverse(rank(p.level)));
        let color = level_color(list[0].level);
        let n = list.len();
        let summary: Vec<String> = list
            .iter()
            .take(2)
            .map(|p| {
                let who = if p.device_name.is_empty() { "Ce PC".to_string() } else { p.device_name.clone() };
                format!("{who} : {}", p.kind.title().to_lowercase())
            })
            .collect();
        let more = if n > 2 { format!(" · et {} autre(s)", n - 2) } else { String::new() };
        ui.add_space(10.0);
        let r = egui::Frame::new().fill(theme::alpha(color, 0.13)).corner_radius(CornerRadius::same(14)).inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                icons::paint(ui.painter(), r, Icon::Alert, color);
                ui.add_space(4.0);
                ui.label(RichText::new(format!("{} en cours", plural(n, "problème"))).strong().color(theme::text()));
                ui.add(egui::Label::new(RichText::new(format!("· {}{more}", summary.join(" · "))).color(theme::muted())).truncate());
            });
        });
        let resp = ui.interact(r.response.rect, egui::Id::unique("problems-banner"), Sense::click());
        if resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        if resp.on_hover_text("Voir les problèmes").clicked() {
            self.view = View::Problems;
        }
    }

    /// Problèmes en cours d'un appareil, dans son panneau (les défauts et le hors-ligne ont déjà leur cadre).
    pub(super) fn device_problems(&mut self, ui: &mut Ui, d: &DeviceView) {
        let list: Vec<Problem> = self.problems_now.iter().filter(|p| p.device == d.id && !matches!(p.kind, ProblemKind::Fault | ProblemKind::Offline)).cloned().collect();
        for p in list {
            let color = level_color(p.level);
            egui::Frame::new().fill(theme::alpha(color, 0.12)).corner_radius(CornerRadius::same(12)).inner_margin(Margin::same(12)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(p.kind.title()).strong().color(color));
                    ui.label(RichText::new(when(&p)).size(12.0).color(theme::faint()));
                    if p.kind.is_event() {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("Compris").on_hover_text("Retirer des problèmes en cours (c'est inscrit au journal)").clicked() {
                                self.hub.dismiss_problem(p.id, Source::User);
                            }
                        });
                    }
                });
                ui.label(RichText::new(&p.detail).size(12.5).color(theme::muted()));
            });
            ui.add_space(10.0);
        }
    }

    /// La page « Problèmes ».
    pub(super) fn problems_view(&mut self, ui: &mut Ui, devices: &[DeviceView]) {
        let list = self.problems_now.clone();
        widgets::page_header(ui, "Problèmes", |_| {});
        let grave = list.iter().filter(|p| p.level == Level::Critical).count();
        if list.is_empty() {
            widgets::lede(ui, "Tout va bien", " · rien d'anormal en ce moment");
        } else {
            let rest = if grave > 0 { format!(" · dont {} grave{}", grave, if grave > 1 { "s" } else { "" }) } else { String::new() };
            widgets::lede(ui, &format!("{} en cours", plural(list.len(), "problème")), &rest);
        }
        ui.add_space(14.0);
        ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            widgets::cap_width(ui, 1000.0);
            if !list.is_empty() {
                widgets::section_title(ui, "En cours", None);
                for p in &list {
                    self.problem_card(ui, p, devices);
                    ui.add_space(8.0);
                }
                ui.add_space(10.0);
            }
            if self.demo {
                self.demo_faults(ui, devices);
                ui.add_space(14.0);
            }
            self.recent_problems(ui);
            ui.add_space(14.0);
            ui.label(
                RichText::new(
                    "HomeSmart surveille : les extinctions et allumages que tu n'as pas demandés (bouton de la prise, autre application, minuterie de la prise, coupure de courant), \
                     les appareils hors ligne ou redémarrés, les défauts signalés par les prises, la tension du secteur, une consommation inhabituelle, \
                     les caméras sans image, l'analyse et l'écoute arrêtées, les erreurs des aspirateurs robots (brosse bloquée, bac plein, robot coincé…) \
                     et l'arrêt brutal de ce PC. Les problèmes graves déclenchent une notification du système (Préférences).",
                )
                .size(12.0)
                .color(theme::faint()),
            );
        });
    }

    fn problem_card(&mut self, ui: &mut Ui, p: &Problem, devices: &[DeviceView]) {
        let color = level_color(p.level);
        let device = devices.iter().find(|d| d.id == p.device).map(|d| d.id.clone()).or_else(|| self.hub.vacuum(&p.device).map(|v| v.id));
        let camera = (device.is_none() && !p.device.is_empty()).then(|| p.device.clone()).filter(|id| self.hub.camera(id).is_some());
        widgets::panel_frame().inner_margin(Margin::same(14)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(38.0, 38.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 19.0, theme::alpha(color, 0.16));
                icons::paint(ui.painter(), Rect::from_center_size(r.center(), vec2(19.0, 19.0)), icon_of(p.kind), color);
                ui.add_space(4.0);
                let buttons = 190.0;
                ui.allocate_ui_with_layout(vec2((ui.available_width() - buttons).max(160.0), 0.0), Layout::top_down(Align::Min), |ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.horizontal_wrapped(|ui| {
                        let who = if p.device_name.is_empty() { "Ce PC" } else { p.device_name.as_str() };
                        ui.label(RichText::new(who).font(theme::bold(15.0)));
                        ui.label(RichText::new(p.kind.title()).font(theme::bold(15.0)).color(color));
                    });
                    ui.label(RichText::new(&p.detail).size(13.0).color(theme::text()));
                    ui.label(RichText::new(format!("{} · {}", when(p), p.kind.source().label())).size(12.0).color(theme::faint()));
                });
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    if p.kind.is_event() && ui.button("Compris").on_hover_text("Retirer des problèmes en cours (c'est inscrit au journal)").clicked() {
                        self.hub.dismiss_problem(p.id, Source::User);
                    }
                    if let Some(id) = &device {
                        if ui.button("Ouvrir").clicked() {
                            self.view = View::Devices;
                            self.select(id);
                        }
                    }
                    if let Some(id) = &camera {
                        if ui.button("Ouvrir").clicked() {
                            self.open_camera(id);
                        }
                    }
                });
            });
        });
    }

    /// Historique : début et fin des problèmes des 7 derniers jours (lu dans le journal).
    fn recent_problems(&mut self, ui: &mut Ui) {
        let rows = self.hub.recent_problems(150, 7);
        widgets::section_title(ui, "Ces 7 derniers jours", Some(if rows.is_empty() { "rien" } else { "" }));
        let today = Local::now().date_naive();
        for e in &rows {
            let end = problems::parse_key(&e.key).is_some_and(|(_, _, end)| end);
            let color = if end { theme::faint() } else { level_color(e.level) };
            let r = ui.horizontal_wrapped(|ui| {
                let day = e.at.date_naive();
                let at = if day == today { e.at.format("%H:%M").to_string() } else { e.at.format("%d/%m %H:%M").to_string() };
                ui.label(RichText::new(at).size(12.5).color(theme::faint())).on_hover_text(e.at.format("%d/%m/%Y %H:%M:%S").to_string());
                if end {
                    let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                    icons::paint(ui.painter(), r, Icon::Check, theme::faint());
                } else {
                    widgets::dot(ui, color);
                }
                if !e.device_name.is_empty() {
                    ui.label(RichText::new(&e.device_name).strong().color(if end { theme::muted() } else { theme::text() }));
                }
                ui.label(RichText::new(&e.message).color(if end { theme::muted() } else { theme::text() }));
                ui.label(RichText::new(format!("· {}", e.source.label())).size(12.0).color(theme::faint()));
            });
            let b = r.response.rect;
            ui.painter().line_segment([pos2(b.left(), b.bottom() + 3.0), pos2(b.right(), b.bottom() + 3.0)], Stroke::new(1.0, theme::line()));
            ui.add_space(6.0);
        }
    }

    /// Démo : provoque chaque panne sur les appareils simulés (la détection, elle, est la vraie).
    fn demo_faults(&mut self, ui: &mut Ui, devices: &[DeviceView]) {
        let by_name = |n: &str| devices.iter().find(|d| d.name == n).map(|d| d.id.clone());
        let (tv, heater) = (by_name("Prise TV"), by_name("Chauffage"));
        let camera = self.hub.cameras().into_iter().find(|c| c.name == "Salon");
        widgets::panel_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Simuler une panne (démo)").font(theme::bold(15.0)));
            ui.label(RichText::new("Chaque bouton provoque ce qui arrive aux vraies prises ; la détection, elle, est celle de tous les jours.").size(12.5).color(theme::muted()));
            ui.add_space(8.0);
            let mut what: Option<(&str, Box<dyn FnOnce() + Send>)> = None;
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
                if let Some(ctl) = tv.as_deref().and_then(homesmart::sim::control) {
                    let c = ctl.clone();
                    if ui.button("Bouton de la Prise TV").clicked() {
                        what = Some(("bouton de la Prise TV", Box::new(move || c.press_button())));
                    }
                    let c = ctl.clone();
                    if ui.button("Coupure de courant (Prise TV, 8 s)").on_hover_text("Prise réglée pour rester éteinte au retour du courant, comme la Multiprise du bureau").clicked() {
                        what = Some((
                            "coupure de courant de 8 s sur la Prise TV",
                            Box::new(move || {
                                let mut m = serde_json::Map::new();
                                m.insert("38".into(), serde_json::json!("power_off"));
                                c.set(m);
                                tokio::spawn(async move { c.power_cut(8.0).await });
                            }),
                        ));
                    }
                    let c = ctl.clone();
                    if ui.button("Sous-tension signalée (15 s)").clicked() {
                        what = Some((
                            "défaut « sous-tension » sur la Prise TV",
                            Box::new(move || {
                                c.set_fault(1 << 4);
                                tokio::spawn(async move {
                                    tokio::time::sleep(Duration::from_secs(15)).await;
                                    c.set_fault(0);
                                });
                            }),
                        ));
                    }
                    let c = ctl.clone();
                    if ui.button("Creux de tension à 185 V (15 s)").clicked() {
                        what = Some((
                            "tension de 185 V sur la Prise TV",
                            Box::new(move || {
                                c.set_voltage(185.0);
                                tokio::spawn(async move {
                                    tokio::time::sleep(Duration::from_secs(15)).await;
                                    c.set_voltage(230.0);
                                });
                            }),
                        ));
                    }
                }
                if let Some((id, ctl)) = heater.as_deref().and_then(|id| homesmart::sim::control(id).map(|c| (id.to_string(), c))) {
                    let c = ctl.clone();
                    if ui.button("Chauffage allumé par son bouton").clicked() {
                        what = Some(("bouton du Chauffage", Box::new(move || c.press_button())));
                    }
                    let (c, hub) = (ctl.clone(), self.hub.clone());
                    if ui.button("Chauffage qui ne chauffe pas (25 W)").clicked() {
                        what = Some((
                            "chauffage allumé qui ne tire que 25 W",
                            Box::new(move || {
                                c.set_load(25.0);
                                tokio::spawn(async move {
                                    let _ = hub.power(&id, true, None, Source::User).await;
                                    tokio::time::sleep(Duration::from_secs(90)).await;
                                    c.set_load(1520.0);
                                });
                            }),
                        ));
                    }
                }
                if let Some(state) = camera.as_ref().and_then(|c| homesmart::camera::sim::by_port(c.rtsp_port)) {
                    if ui.button("Caméra Salon sans image (40 s)").clicked() {
                        what = Some((
                            "flux de la caméra Salon coupé 40 s",
                            Box::new(move || {
                                state.down.store(true, std::sync::atomic::Ordering::Relaxed);
                                tokio::spawn(async move {
                                    tokio::time::sleep(Duration::from_secs(40)).await;
                                    state.down.store(false, std::sync::atomic::Ordering::Relaxed);
                                });
                            }),
                        ));
                    }
                }
                // Aspirateurs simulés : brosse bloquée, robot coincé (réparés au bout de 40 s).
                for v in self.hub.vacuums() {
                    let Some(robot) = homesmart::vacuum::sim::robot(&v.id) else { continue };
                    for (label, kind) in [("brosse bloquée", homesmart::vacuum::ErrorKind::MainBrush), ("robot coincé", homesmart::vacuum::ErrorKind::Stuck)] {
                        let r = robot.clone();
                        if ui.button(format!("{} : {label} (40 s)", v.name)).clicked() {
                            what = Some((
                                if kind == homesmart::vacuum::ErrorKind::MainBrush { "brosse de l'aspirateur bloquée" } else { "aspirateur coincé" },
                                Box::new(move || {
                                    r.lock().unwrap().set_error(Some(kind));
                                    tokio::spawn(async move {
                                        tokio::time::sleep(Duration::from_secs(40)).await;
                                        r.lock().unwrap().set_error(None);
                                    });
                                }),
                            ));
                        }
                    }
                }
                let hub = self.hub.clone();
                if ui.button("Coupure de courant de ce PC").on_hover_text("Ce que HomeSmart dit au démarrage qui suit une coupure").clicked() {
                    what = Some(("coupure de courant de ce PC (constat du redémarrage)", Box::new(move || hub.demo_host_shutdown())));
                }
            });
            if let Some((label, act)) = what {
                self.hub.log(Level::Info, Source::User, "", format!("Démo : panne simulée — {label}"));
                self.rt.spawn(async move { act() });
            }
        });
    }
}
