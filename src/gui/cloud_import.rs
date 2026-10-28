//! Fenêtre « Importer depuis Smart Life » : QR code → liste des appareils → import.

use super::{theme, widgets, App};
use eframe::egui::{self, vec2, Align, Color32, CornerRadius, Id, Layout, Margin, RichText, Sense, Stroke};
use homesmart::cloud::{self, CloudDevice};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const QR_LIFETIME: Duration = Duration::from_secs(5 * 60);

pub enum CloudState {
    EnterCode,
    Working(String),
    Qr { dark: Vec<bool>, width: usize },
    Choose(Vec<CloudDevice>),
    Error(String),
}

pub struct CloudImport {
    user_code: String,
    state: Arc<Mutex<CloudState>>,
    cancel: Arc<AtomicBool>,
    selected: Vec<bool>,
}

impl CloudImport {
    pub fn new() -> Self {
        CloudImport {
            user_code: String::new(),
            state: Arc::new(Mutex::new(CloudState::EnterCode)),
            cancel: Arc::new(AtomicBool::new(false)),
            selected: Vec::new(),
        }
    }
}

impl CloudImport {
    /// Étape QR pré-remplie (captures de contrôle de l'interface).
    pub fn preview_qr() -> Self {
        let imp = CloudImport::new();
        if let Ok(code) = qrcode::QrCode::new("tuyaSmart--qrLogin?token=AYpreview0123456789") {
            *imp.state.lock().unwrap() = CloudState::Qr {
                width: code.width(),
                dark: code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect(),
            };
        }
        imp
    }
}

impl Drop for CloudImport {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Tout le dialogue réseau, dans un fil bloquant (appels HTTPS synchrones).
fn run(user_code: String, state: Arc<Mutex<CloudState>>, cancel: Arc<AtomicBool>, ctx: egui::Context) {
    let set = |s: CloudState| {
        *state.lock().unwrap() = s;
        ctx.request_repaint();
    };
    let login = match cloud::start_login(&user_code) {
        Ok(l) => l,
        Err(e) => return set(CloudState::Error(format!("{e:#}"))),
    };
    match qrcode::QrCode::new(login.qr_content()) {
        Ok(code) => set(CloudState::Qr {
            width: code.width(),
            dark: code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect(),
        }),
        Err(e) => return set(CloudState::Error(format!("QR code impossible : {e}"))),
    }
    let start = Instant::now();
    let session = loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        if start.elapsed() > QR_LIFETIME {
            return set(CloudState::Error("Le QR code a expiré. Recommence pour en obtenir un nouveau.".into()));
        }
        match cloud::poll_login(&login) {
            Ok(Some(s)) => break s,
            Ok(None) => {}
            Err(e) => tracing::debug!("attente du scan : {e:#}"),
        }
        std::thread::sleep(Duration::from_secs(2));
    };
    set(CloudState::Working("Connecté ! Récupération de tes appareils et de leurs clés…".into()));
    let result = session.fetch_devices();
    session.logout();
    match result {
        Ok(list) if list.is_empty() => set(CloudState::Error("Aucun appareil dans ce compte Smart Life.".into())),
        Ok(list) => set(CloudState::Choose(list)),
        Err(e) => set(CloudState::Error(format!("{e:#}"))),
    }
}

fn paint_qr(ui: &mut egui::Ui, dark: &[bool], width: usize, size: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(12), Color32::WHITE);
    let quiet = 4.0;
    let module = size / (width as f32 + 2.0 * quiet);
    for (i, &on) in dark.iter().enumerate() {
        if on {
            let (x, y) = ((i % width) as f32, (i / width) as f32);
            let min = rect.min + vec2((x + quiet) * module, (y + quiet) * module);
            painter.rect_filled(egui::Rect::from_min_size(min, vec2(module + 0.3, module + 0.3)), 0.0, Color32::BLACK);
        }
    }
}

fn step(ui: &mut egui::Ui, n: &str, text: &str) {
    ui.horizontal(|ui| {
        widgets::pill(ui, n, theme::bg(), theme::accent());
        ui.label(RichText::new(text).size(13.5));
    });
}

impl App {
    pub(super) fn cloud_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut imp) = self.cloud.take() else { return };
        let mut close = false;
        let mut chosen: Option<Vec<CloudDevice>> = None;
        let discovered = self.hub.discovered();
        let frame = egui::Frame::new()
            .fill(theme::surface())
            .stroke(Stroke::new(1.0, theme::line()))
            .corner_radius(CornerRadius::same(16))
            .inner_margin(Margin::same(22));
        let modal = egui::Modal::new(Id::unique("cloud-import")).frame(frame).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.label(RichText::new("Importer depuis Smart Life").size(19.0).strong());
            ui.label(
                RichText::new("Une seule fois : HomeSmart lit les clés locales de tes appareils, ferme la session, puis pilote tout en Wi-Fi local.")
                    .size(12.5)
                    .color(theme::muted()),
            );
            ui.add_space(14.0);

            let mut state = imp.state.lock().unwrap();
            match &*state {
                CloudState::EnterCode => {
                    step(ui, "1", "Dans Smart Life : Moi  ›  ⚙ Paramètres  ›  Compte et sécurité");
                    step(ui, "2", "Copie le « Code utilisateur » et colle-le ici :");
                    ui.add_space(6.0);
                    let r = ui.add(egui::TextEdit::singleline(&mut imp.user_code).hint_text("Code utilisateur (sensible à la casse)").desired_width(f32::INFINITY));
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Dans l'app, la demande d'accès apparaîtra au nom de « Home Assistant » : HomeSmart utilise le même mécanisme que l'intégration Tuya officielle de Home Assistant.")
                            .size(11.5)
                            .color(theme::faint()),
                    );
                    ui.add_space(12.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let ok = !imp.user_code.trim().is_empty();
                        if ui.add_enabled(ok, widgets::primary_button("Afficher le QR code", theme::accent()).min_size(vec2(0.0, 32.0))).clicked() || (ok && enter) {
                            *state = CloudState::Working("Demande d'un QR code à Smart Life…".into());
                            let (code, st, cancel, c) = (imp.user_code.trim().to_string(), imp.state.clone(), imp.cancel.clone(), ctx.clone());
                            self.rt.spawn_blocking(move || run(code, st, cancel, c));
                        }
                        if ui.button("Annuler").clicked() {
                            close = true;
                        }
                    });
                }
                CloudState::Working(msg) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(msg);
                    });
                    ui.add_space(10.0);
                    if ui.button("Annuler").clicked() {
                        close = true;
                    }
                }
                CloudState::Qr { dark, width } => {
                    ui.horizontal(|ui| {
                        paint_qr(ui, dark, *width, 220.0);
                        ui.add_space(12.0);
                        ui.vertical(|ui| {
                            ui.set_max_width(220.0);
                            step(ui, "1", "Ouvre Smart Life");
                            step(ui, "2", "Accueil  ›  « + » en haut à droite  ›  Scanner");
                            step(ui, "3", "Scanne ce code puis confirme la connexion");
                            ui.add_space(12.0);
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(RichText::new("En attente du scan…").color(theme::muted()));
                            });
                        });
                    });
                    ui.add_space(12.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Annuler").clicked() {
                            close = true;
                        }
                    });
                }
                CloudState::Choose(list) => {
                    if imp.selected.len() != list.len() {
                        imp.selected = list.iter().map(|d| !d.sub && d.local_key.len() == 16).collect();
                    }
                    ui.label(RichText::new(format!("{} appareil(s) trouvé(s) dans ton compte :", list.len())).strong());
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for (i, d) in list.iter().enumerate() {
                            let lan = discovered.iter().find(|x| x.device.id == d.id);
                            let usable = !d.sub && d.local_key.len() == 16;
                            egui::Frame::new().fill(theme::surface()).corner_radius(CornerRadius::same(10)).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.add_enabled(usable, egui::Checkbox::without_text(&mut imp.selected[i]));
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 1.0;
                                        ui.label(RichText::new(&d.name).strong());
                                        let detail = [d.product_name.as_str(), d.home.as_str()].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join("  ·  ");
                                        ui.label(RichText::new(detail).size(11.5).color(theme::muted()));
                                    });
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if d.sub {
                                            widgets::pill(ui, "via passerelle : non pris en charge", theme::bg(), theme::faint());
                                        } else if d.local_key.len() != 16 {
                                            widgets::pill(ui, "pas de clé locale", theme::bg(), theme::crit());
                                        } else if let Some(x) = lan {
                                            widgets::pill(ui, &format!("sur le Wi-Fi · {}", x.device.ip), theme::bg(), theme::accent());
                                        } else {
                                            widgets::pill(ui, "pas vu sur le Wi-Fi", theme::bg(), theme::accent());
                                        }
                                    });
                                });
                            });
                            ui.add_space(4.0);
                        }
                    });
                    ui.add_space(10.0);
                    let n = imp.selected.iter().filter(|s| **s).count();
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add_enabled(n > 0, widgets::primary_button(&format!("Importer ({n})"), theme::accent()).min_size(vec2(0.0, 32.0))).clicked() {
                            chosen = Some(list.iter().zip(&imp.selected).filter(|(_, s)| **s).map(|(d, _)| d.clone()).collect());
                        }
                        if ui.button("Annuler").clicked() {
                            close = true;
                        }
                    });
                }
                CloudState::Error(msg) => {
                    ui.label(RichText::new(msg).color(theme::crit()));
                    ui.add_space(12.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add(widgets::primary_button("Recommencer", theme::accent())).clicked() {
                            *state = CloudState::EnterCode;
                        }
                        if ui.button("Fermer").clicked() {
                            close = true;
                        }
                    });
                }
            }
        });
        if modal.should_close() {
            close = true;
        }

        if let Some(list) = chosen {
            let mut ok = 0;
            for d in &list {
                let lan = discovered.iter().find(|x| x.device.id == d.id);
                let cfg = d.to_config(lan.map(|x| x.device.ip.clone()), lan.map(|x| x.device.version.clone()));
                match self.hub.add(cfg) {
                    Ok(_) => ok += 1,
                    Err(e) => self.toast(format!("{} : {e:#}", d.name), true),
                }
            }
            self.toast(format!("{ok} appareil(s) importé(s). Le compte Smart Life n'est plus utilisé."), false);
            return;
        }
        if !close {
            self.cloud = Some(imp);
        }
    }
}
