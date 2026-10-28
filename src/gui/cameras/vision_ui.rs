//! Vision par IA dans la vue d'une caméra : réglages, état, personnes en vue (avec
//! « Enregistrer ce visage »), visages connus, et dessin des détections sur la vidéo.

use super::super::icons::{self, Icon};
use super::super::{theme, widgets, App};
use eframe::egui::{self, pos2, vec2, Color32, CornerRadius, Painter, Rect, RichText, Stroke, StrokeKind, Ui};
use homesmart::hub::{CameraView, Level, Source};
use homesmart::vision::engine::Accel;
use homesmart::vision::faces::Verdict;
use homesmart::vision::pipeline::PersonView;
use std::time::{Duration, Instant};

/// Os du squelette COCO (paires de points) dessinés sur la vidéo.
const BONES: [(usize, usize); 16] = [(5, 7), (7, 9), (6, 8), (8, 10), (5, 6), (5, 11), (6, 12), (11, 12), (11, 13), (13, 15), (12, 14), (14, 16), (0, 1), (0, 2), (1, 3), (2, 4)];

#[derive(Default)]
pub(in crate::gui) struct VisionUi {
    /// Enregistrement d'un visage en cours : (caméra, piste, nom saisi).
    enroll: Option<(String, u32, String)>,
    /// Renommage d'une personne connue : (identifiant, nom saisi).
    rename: Option<(String, String)>,
    /// Suppression à confirmer.
    confirm_delete: Option<String>,
    /// Masquer les cadres et les zones sur la vidéo.
    pub hide_overlay: bool,
    /// Scénario en cours d'ajout.
    scenario: Option<ScenarioForm>,
    /// État de la vision (lu en partie sur le disque), gardé un court instant.
    status: Option<(Instant, homesmart::hub::VisionStatus)>,
    /// Derniers événements affichés : (caméra, dernière entrée du journal, liste).
    events: Option<(String, u64, Vec<(chrono::DateTime<chrono::Local>, Level, String)>)>,
}

/// Formulaire « Ajouter un scénario ».
pub(in crate::gui) struct ScenarioForm {
    camera: String,
    /// Indice dans `Trigger::CHOICES`.
    trigger: usize,
    minutes: u32,
    on: bool,
    device: String,
    hours: bool,
    start: String,
    end: String,
    error: Option<String>,
}

/// Couleur et libellé d'une personne selon ce qu'on sait d'elle.
fn badge(p: &PersonView) -> (Color32, String) {
    match &p.verdict {
        Verdict::Person { name, .. } => (theme::accent(), name.clone()),
        Verdict::Stranger => (theme::warn(), "Inconnu".to_string()),
        Verdict::Undecided => (theme::accent(), "Personne".to_string()),
    }
}

/// Présence du jour : 24 barres (minutes occupées par heure), et le total.
fn presence_bars(ui: &mut Ui, day: &[f32; 24], total: f32) {
    ui.horizontal(|ui| {
        let text = if total < 0.05 { "aucune".to_string() } else { homesmart::vision::presence::fmt_minutes(total) };
        ui.label(RichText::new(format!("Présence aujourd'hui : {text}")).size(11.5).color(theme::muted()));
    });
    let width = ui.available_width().min(340.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 30.0), egui::Sense::hover());
    let p = ui.painter();
    let now = chrono::Timelike::hour(&chrono::Local::now()) as usize;
    let w = rect.width() / 24.0;
    for (h, m) in day.iter().enumerate() {
        let x = rect.left() + w * h as f32;
        let k = (m / 60.0).clamp(0.0, 1.0);
        let bg = Rect::from_min_max(pos2(x + 1.0, rect.top()), pos2(x + w - 1.0, rect.bottom() - 8.0));
        p.rect_filled(bg, CornerRadius::same(2), theme::alpha(theme::muted(), if h <= now { 0.12 } else { 0.05 }));
        if k > 0.0 {
            let bar = Rect::from_min_max(pos2(bg.left(), bg.bottom() - bg.height() * k.max(0.08)), bg.max);
            p.rect_filled(bar, CornerRadius::same(2), theme::accent());
        }
    }
    for h in [0usize, 6, 12, 18] {
        p.text(pos2(rect.left() + w * h as f32, rect.bottom()), egui::Align2::LEFT_BOTTOM, format!("{h} h"), egui::FontId::proportional(9.0), theme::faint());
    }
    if let Some(pos) = resp.hover_pos() {
        let h = (((pos.x - rect.left()) / w).floor() as usize).min(23);
        resp.on_hover_text_at_pointer(format!("{h} h – {} h : {}", h + 1, homesmart::vision::presence::fmt_minutes(day[h])));
    }
}

/// Un choix de la surveillance : icône, nom, explication et case ronde ; un clic l'inverse.
fn pick(ui: &mut Ui, icon: Icon, label: &str, hint: &str, on: &mut bool) {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 52.0), egui::Sense::click());
    let h = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(14), theme::mix(theme::raised(), theme::raised_hi(), 0.5 * h));
    icons::paint(p, Rect::from_center_size(pos2(rect.left() + 22.0, rect.center().y), vec2(18.0, 18.0)), icon, theme::muted());
    p.text(pos2(rect.left() + 44.0, rect.center().y - 8.0), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(14.5), theme::text());
    p.text(pos2(rect.left() + 44.0, rect.center().y + 9.0), egui::Align2::LEFT_CENTER, hint, egui::FontId::proportional(11.5), theme::faint());
    let ck = pos2(rect.right() - 22.0, rect.center().y);
    if *on {
        p.circle_filled(ck, 11.0, theme::accent());
        icons::paint(p, Rect::from_center_size(ck, vec2(14.0, 14.0)), Icon::Check, theme::on_accent());
    } else {
        p.circle_stroke(ck, 10.0, Stroke::new(2.0, theme::faint()));
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.clicked() {
        *on = !*on;
    }
    ui.add_space(-2.0);
}

impl App {
    /// Cadres, noms et squelettes par-dessus la vidéo (`img` : rectangle de l'image à l'écran).
    pub(in crate::gui) fn vision_overlay(&self, p: &Painter, img: Rect, c: &CameraView) {
        if self.cams.vision.hide_overlay {
            return;
        }
        let Some(a) = self.hub.camera_analysis(&c.id).filter(|a| a.running && a.updated.is_some_and(|t| t.elapsed() < Duration::from_secs(2))) else { return };
        let at = |x: f32, y: f32| pos2(img.left() + x * img.width(), img.top() + y * img.height());
        // Haut de la partie visible (une fois zoomée, `img` la dépasse) : les noms restent lisibles.
        let top = img.top().max(p.clip_rect().top());
        for person in &a.persons {
            let (color, label) = badge(person);
            let r = Rect::from_min_max(at(person.bbox.x0, person.bbox.y0), at(person.bbox.x1, person.bbox.y1));
            p.rect_stroke(r, CornerRadius::same(4), Stroke::new(2.0, color), StrokeKind::Outside);
            if let Some(pose) = &person.pose {
                let pts = &pose.points;
                for (i, j) in BONES {
                    if pts[i].score >= 0.35 && pts[j].score >= 0.35 {
                        p.line_segment([at(pts[i].at.x, pts[i].at.y), at(pts[j].at.x, pts[j].at.y)], Stroke::new(2.0, theme::alpha(color, 0.8)));
                    }
                }
                for k in pts.iter().filter(|k| k.score >= 0.35) {
                    p.circle_filled(at(k.at.x, k.at.y), 2.5, Color32::WHITE);
                }
            }
            if let Some(f) = &person.face {
                let fr = Rect::from_min_max(at(f.bbox.x0, f.bbox.y0), at(f.bbox.x1, f.bbox.y1));
                p.rect_stroke(fr, CornerRadius::same(3), Stroke::new(1.0, theme::alpha(Color32::WHITE, 0.8)), StrokeKind::Outside);
            }
            let g = p.layout_no_wrap(label, theme::bold(11.5), Color32::WHITE);
            let pill = Rect::from_min_size(pos2(r.left(), (r.top() - g.size().y - 8.0).max(top)), g.size() + vec2(12.0, 6.0));
            p.rect_filled(pill, CornerRadius::same(6), theme::alpha(color, 0.85));
            p.galley(pill.min + vec2(6.0, 3.0), g, Color32::WHITE);
        }
        // Animaux (violet) et véhicules (gris bleuté).
        for o in &a.objects {
            let color = if o.kind.is_animal() { theme::violet() } else { theme::mix(theme::muted(), theme::accent(), 0.4) };
            let r = Rect::from_min_max(at(o.bbox.x0, o.bbox.y0), at(o.bbox.x1, o.bbox.y1));
            p.rect_stroke(r, CornerRadius::same(4), Stroke::new(2.0, color), StrokeKind::Outside);
            let mut label = o.kind.label().to_string();
            label[..1].make_ascii_uppercase();
            let g = p.layout_no_wrap(label, theme::bold(11.0), Color32::WHITE);
            let pill = Rect::from_min_size(pos2(r.left(), (r.top() - g.size().y - 8.0).max(top)), g.size() + vec2(12.0, 6.0));
            p.rect_filled(pill, CornerRadius::same(6), theme::alpha(color, 0.85));
            p.galley(pill.min + vec2(6.0, 3.0), g, Color32::WHITE);
        }
    }

    /// Capture d'écran (« #scenarios ») : formulaire d'ajout ouvert, sur une absence.
    pub(in crate::gui) fn demo_scenario_form(&mut self, camera: &str) {
        let device = self.hub.list().first().map(|d| d.id.clone()).unwrap_or_default();
        self.cams.vision.scenario = Some(ScenarioForm { camera: camera.to_string(), trigger: 1, minutes: 30, on: false, device, hours: true, start: "22:00".into(), end: "07:00".into(), error: None });
    }

    /// Scénarios de cette caméra : liste, suggestion, ajout.
    fn scenarios_section(&mut self, ui: &mut Ui, c: &CameraView, cv: &homesmart::vision::CameraVision) {
        use homesmart::automation::{looks_risky, Action, Scenario, TimeRange, Trigger};
        let all = self.hub.scenarios();
        let mine: Vec<Scenario> = all.iter().filter(|s| s.camera == c.id).cloned().collect();
        let devices = self.hub.list();
        // Appareils et aspirateurs robots (un robot se lance ou rentre à sa base).
        let targets = self.targets(&devices);
        let name_of = |id: &str| targets.iter().find(|d| d.id == id).map(|d| d.name.clone()).unwrap_or_else(|| id.to_string());
        ui.add_space(8.0);
        egui::CollapsingHeader::new(RichText::new(format!("Scénarios ({})", mine.len())).size(13.0)).id_salt(("scenarios", &c.id)).default_open(true).show(ui, |ui| {
            ui.label(RichText::new("Ce que voit ou entend la caméra commande tes appareils (seulement pendant que l'analyse tourne).").size(11.5).color(theme::faint()));
            let (mut toggle, mut delete) = (None, None);
            for s in &mine {
                ui.horizontal_wrapped(|ui| {
                    if widgets::toggle(ui, s.enabled, theme::accent(), true).clicked() {
                        toggle = Some((s.id, !s.enabled));
                    }
                    let mut text = s.summary(&name_of(&s.device));
                    text[..1].make_ascii_uppercase();
                    ui.label(RichText::new(text).size(12.5).color(if s.enabled { theme::text() } else { theme::muted() }));
                    if ui.small_button("Supprimer").clicked() {
                        delete = Some(s.id);
                    }
                });
                if s.trigger.needs_listening() && !cv.sounds {
                    ui.label(RichText::new("  ↳ active « Sons de la maison » pour ce déclencheur").size(11.0).color(theme::warn()));
                }
            }
            if let Some((id, on)) = toggle {
                self.hub.set_scenario_enabled(id, on, Source::User);
            }
            if let Some(id) = delete {
                self.hub.remove_scenario(id, Source::User);
            }
            // Suggestion : un appareil à risque dans la même pièce, sans scénario d'absence.
            let room = c.room.clone().unwrap_or_default();
            let risky = devices.iter().find(|d| {
                (looks_risky(&d.name) || d.safety.enabled) && !room.is_empty() && d.room.as_deref() == Some(room.as_str()) && !mine.iter().any(|s| s.device == d.id && matches!(s.trigger, Trigger::Absence { .. }))
            });
            if let Some(d) = risky {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("Suggestion : éteindre « {} » quand la pièce est vide depuis 30 min", d.name)).size(12.0).color(theme::accent()));
                    if ui.small_button("Ajouter").clicked() {
                        let s = Scenario { id: 0, enabled: true, camera: c.id.clone(), trigger: Trigger::Absence { minutes: 30 }, device: d.id.clone(), action: Action::Off, during: None };
                        if let Err(e) = self.hub.add_scenario(s, Source::User) {
                            self.toast(format!("{e:#}"), true);
                        }
                    }
                });
            }
            let form = &mut self.cams.vision.scenario;
            match form.as_mut().filter(|f| f.camera == c.id) {
                None => {
                    if ui.small_button("+ Ajouter un scénario").clicked() {
                        *form = Some(ScenarioForm { camera: c.id.clone(), trigger: 0, minutes: 30, on: true, device: targets.first().map(|d| d.id.clone()).unwrap_or_default(), hours: false, start: "22:00".into(), end: "07:00".into(), error: None });
                    }
                }
                Some(f) => {
                    let mut done = false;
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Quand").size(12.5).color(theme::muted()));
                        egui::ComboBox::from_id_salt(("scen-trigger", &c.id)).selected_text(match Trigger::CHOICES[f.trigger] { Trigger::Absence { .. } => "personne depuis…".to_string(), t => t.label() }).show_ui(ui, |ui| {
                            for (i, t) in Trigger::CHOICES.iter().enumerate() {
                                let label = if matches!(t, Trigger::Absence { .. }) { "personne depuis…".to_string() } else { t.label() };
                                ui.selectable_value(&mut f.trigger, i, label);
                            }
                        });
                        if matches!(Trigger::CHOICES[f.trigger], Trigger::Absence { .. }) {
                            ui.add(egui::DragValue::new(&mut f.minutes).range(1..=1440).suffix(" min"));
                        }
                    });
                    let vacuum = targets.iter().any(|t| t.id == f.device && t.vacuum);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("alors").size(12.5).color(theme::muted()));
                        let labels = if vacuum { ["Nettoyer", "Rentrer à la base"] } else { ["Allumer", "Éteindre"] };
                        if let Some(i) = widgets::segmented(ui, &labels, Some(if f.on { 0 } else { 1 }), theme::accent()) {
                            f.on = i == 0;
                        }
                        egui::ComboBox::from_id_salt(("scen-device", &c.id)).selected_text(name_of(&f.device)).show_ui(ui, |ui| {
                            for d in &targets {
                                ui.selectable_value(&mut f.device, d.id.clone(), &d.name);
                            }
                        });
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.checkbox(&mut f.hours, "seulement de");
                        ui.add_enabled(f.hours, egui::TextEdit::singleline(&mut f.start).desired_width(58.0));
                        ui.label("à");
                        ui.add_enabled(f.hours, egui::TextEdit::singleline(&mut f.end).desired_width(58.0));
                    });
                    if let Some(e) = &f.error {
                        ui.label(RichText::new(e).size(12.0).color(theme::warn()));
                    }
                    let mut add = None;
                    ui.horizontal(|ui| {
                        if ui.add(widgets::primary_button("Ajouter", theme::accent())).clicked() {
                            let trigger = match Trigger::CHOICES[f.trigger] {
                                Trigger::Absence { .. } => Trigger::Absence { minutes: f.minutes },
                                t => t,
                            };
                            let during = f.hours.then(|| TimeRange { start: f.start.trim().to_string(), end: f.end.trim().to_string() });
                            let action = match (vacuum, f.on) {
                                (false, true) => Action::On,
                                (false, false) => Action::Off,
                                (true, true) => Action::Clean,
                                (true, false) => Action::Dock,
                            };
                            add = Some(Scenario { id: 0, enabled: true, camera: c.id.clone(), trigger, device: f.device.clone(), action, during });
                        }
                        if ui.button("Annuler").clicked() {
                            done = true;
                        }
                    });
                    if let Some(s) = add {
                        match self.hub.add_scenario(s, Source::User) {
                            Ok(_) => done = true,
                            Err(e) => f.error = Some(format!("{e:#}")),
                        }
                    }
                    if done {
                        *form = None;
                    }
                }
            }
        });
    }

    /// Photo de visage déposée sur la fenêtre : nom de la personne, puis enregistrement.
    pub(in crate::gui) fn photo_face_dialog(&mut self, ctx: &egui::Context) {
        let Some((path, mut name, mut error)) = self.photo_face.take() else { return };
        let mut keep = true;
        let frame = egui::Frame::new().fill(theme::surface()).stroke(Stroke::new(1.0, theme::line())).corner_radius(CornerRadius::same(16)).inner_margin(egui::Margin::same(22));
        let modal = egui::Modal::new(egui::Id::unique("photo-face")).frame(frame).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(RichText::new("Enregistrer un visage depuis une photo").size(18.0).strong());
            ui.add_space(4.0);
            ui.label(RichText::new(path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()).size(12.0).color(theme::muted()));
            ui.label(
                RichText::new("Une photo nette, de face, d'une seule personne. Seule l'empreinte du visage est gardée (chiffrée), pas la photo.")
                    .size(12.0)
                    .color(theme::faint()),
            );
            ui.add_space(8.0);
            let mut save = false;
            ui.horizontal(|ui| {
                ui.label(RichText::new("Nom").size(12.5).color(theme::muted()));
                let r = ui.add(egui::TextEdit::singleline(&mut name).hint_text("ex. Alice").desired_width(200.0));
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    save = true;
                }
            });
            if let Some(e) = &error {
                ui.label(RichText::new(e).size(12.0).color(theme::warn()));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.add(widgets::primary_button("Enregistrer", theme::accent())).clicked() {
                    save = true;
                }
                if ui.button("Annuler").clicked() {
                    keep = false;
                }
            });
            if save {
                match self.hub.enroll_face_photo(&path, &name, Source::User) {
                    Ok(()) => {
                        self.toast(format!("Visage de {} enregistré", name.trim()), false);
                        keep = false;
                    }
                    Err(e) => error = Some(format!("{e:#}")),
                }
            }
        });
        if modal.should_close() {
            keep = false;
        }
        if keep {
            self.photo_face = Some((path, name, error));
        }
    }

    /// Écoute du micro : réglages, modèles, niveau et derniers sons ou phrases entendus.
    fn listen_settings(&mut self, ui: &mut Ui, c: &CameraView, cv: &mut homesmart::vision::CameraVision, status: &homesmart::hub::VisionStatus) {
        widgets::row(ui, "Sons de la maison", Some("alarme, verre brisé, pleurs, cris, aboiements, on frappe…"), |ui| {
            ui.add_space(8.0);
            if widgets::toggle(ui, cv.sounds, theme::accent(), true).clicked() {
                cv.sounds = !cv.sounds;
            }
        });
        widgets::row(ui, "Transcrire la parole", Some("texte au journal ; alerte sur « au secours »"), |ui| {
            ui.add_space(8.0);
            if widgets::toggle(ui, cv.speech, theme::accent(), true).clicked() {
                cv.speech = !cv.speech;
            }
        });
        let mb = homesmart::hub::listen_missing_mb(cv);
        if mb > 0.0 {
            egui::Frame::new().fill(theme::surface()).corner_radius(CornerRadius::same(12)).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                ui.label(RichText::new(format!("Modèles de l'écoute à installer ({mb:.0} Mo, licences libres, sources officielles, empreintes vérifiées).")).size(12.0));
                if status.install.running {
                    ui.horizontal_wrapped(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(&status.install.step).size(12.0).color(theme::muted()));
                    });
                    ui.ctx().request_repaint_after(Duration::from_millis(300));
                } else if ui.add(widgets::primary_button("Installer", theme::accent())).clicked() {
                    self.hub.install_listen_models(cv.sounds, cv.speech, Source::User);
                    self.cams.vision.status = None;
                }
                if let Some(e) = &status.install.error {
                    ui.label(RichText::new(e).size(12.0).color(theme::warn()));
                }
            });
        }
        if let Some(l) = self.hub.camera_listening(&c.id) {
            if let Some(e) = &l.error {
                ui.label(RichText::new(format!("Écoute : {e}")).size(11.5).color(theme::warn()));
            } else if l.running {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Écoute active").size(11.5).color(theme::accent()));
                    if let Some(db) = l.level {
                        // Niveau du micro : −70 dB (silence) à −10 dB (fort).
                        let (r, _) = ui.allocate_exact_size(vec2(80.0, 6.0), egui::Sense::hover());
                        let k = ((db + 70.0) / 60.0).clamp(0.0, 1.0);
                        ui.painter().rect_filled(r, CornerRadius::same(3), theme::alpha(theme::muted(), 0.25));
                        ui.painter().rect_filled(Rect::from_min_size(r.min, vec2(r.width() * k, r.height())), CornerRadius::same(3), theme::accent());
                        ui.label(RichText::new(format!("{db:.0} dB")).size(11.0).color(theme::faint()));
                    }
                });
                ui.ctx().request_repaint_after(Duration::from_millis(300));
            } else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Écoute : chargement…").size(11.5).color(theme::muted()));
                });
                ui.ctx().request_repaint_after(Duration::from_millis(300));
            }
            for (at, what) in l.heard.iter().filter(|(at, _)| at.elapsed() < Duration::from_secs(180)).take(3) {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("il y a {} s", at.elapsed().as_secs())).size(11.0).color(theme::faint()));
                    ui.label(RichText::new(what).size(12.0));
                });
            }
        }
        if cv.sounds || cv.speech {
            ui.label(RichText::new("Le son n'est jamais enregistré : il est analysé en mémoire puis oublié. Seuls les sons reconnus (et le texte, si la transcription est activée) vont au journal.").size(11.0).color(theme::faint()));
        }
    }

    /// État de la vision (modèles installés : lu sur le disque), gardé deux secondes ; relu à
    /// chaque image pendant une installation.
    fn vision_status_cached(&mut self) -> homesmart::hub::VisionStatus {
        if let Some((at, s)) = &self.cams.vision.status {
            if at.elapsed() < Duration::from_secs(2) && !s.install.running {
                return s.clone();
            }
        }
        let s = self.hub.vision_status();
        self.cams.vision.status = Some((Instant::now(), s.clone()));
        s
    }

    /// Derniers événements d'une caméra : le journal n'est relu que s'il a une entrée de plus.
    fn vision_events_cached(&mut self, camera: &str) -> Vec<(chrono::DateTime<chrono::Local>, Level, String)> {
        let last = self.hub.alerts(1).first().map_or(0, |e| e.id);
        if let Some((cam, id, list)) = &self.cams.vision.events {
            if cam == camera && *id == last {
                return list.clone();
            }
        }
        let list = self.hub.vision_events(camera, 8);
        self.cams.vision.events = Some((camera.to_string(), last, list.clone()));
        list
    }

    /// « Surveillance » d'une caméra, à côté de la vidéo : un interrupteur, son état, trois choix
    /// simples de ce qui doit prévenir, puis deux lignes vers « Avancé » (1) et « Réglages de la
    /// caméra » (2) ; renvoie la page demandée.
    pub(in crate::gui) fn surveillance_section(&mut self, ui: &mut Ui, c: &CameraView) -> Option<usize> {
        let status = self.vision_status_cached();
        let mut cv = self.hub.camera_vision(&c.id);
        let before = cv.clone();
        let analysis = self.hub.camera_analysis(&c.id);
        let mut page = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Surveillance").font(theme::bold(18.0)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::toggle(ui, cv.enabled, theme::accent(), status.models_ready).clicked() && status.models_ready {
                    cv.enabled = !cv.enabled;
                }
            });
        });
        // État en une ligne.
        let (text, color) = match &analysis {
            Some(a) if a.error.is_some() => (format!("Impossible : {}", a.error.clone().unwrap_or_default()), theme::warn()),
            Some(a) if a.starting => ("Chargement…".to_string(), theme::muted()),
            Some(a) if a.running && !a.persons.is_empty() => (format!("Active · {} en vue", if a.persons.len() > 1 { format!("{} personnes", a.persons.len()) } else { "1 personne".into() }), theme::accent()),
            Some(a) if a.running => ("Active · aucune personne en vue".to_string(), theme::accent()),
            _ if cv.enabled => ("Démarrage…".to_string(), theme::muted()),
            _ => ("Désactivée : l'image n'est pas analysée".to_string(), theme::muted()),
        };
        ui.horizontal_wrapped(|ui| {
            if analysis.as_ref().is_some_and(|a| a.starting) {
                ui.spinner();
                ui.ctx().request_repaint_after(Duration::from_millis(300));
            }
            ui.label(RichText::new(text).size(13.0).color(color));
        });
        // Modèles absents : installation en un clic.
        if !status.models_ready {
            ui.add_space(8.0);
            ui.label(RichText::new(format!("Modèles d'IA à installer ({:.0} Mo, licences libres, sources officielles).", status.missing_mb)).size(12.5).color(theme::muted()));
            if status.install.running {
                ui.horizontal_wrapped(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(&status.install.step).size(12.0).color(theme::muted()));
                });
                ui.ctx().request_repaint_after(Duration::from_millis(300));
            } else if ui.add(widgets::primary_button("Installer les modèles", theme::accent())).clicked() {
                self.hub.install_vision_models(Source::User);
                self.cams.vision.status = None;
            }
            if let Some(e) = &status.install.error {
                ui.label(RichText::new(e).size(12.0).color(theme::warn()));
            }
        }
        if cv.enabled {
            ui.add_space(14.0);
            ui.label(RichText::new("Me prévenir en cas de").size(13.0).color(theme::muted()));
            ui.add_space(2.0);
            pick(ui, Icon::Fall, "Chute", "quelqu'un tombe et reste au sol", &mut cv.falls);
            pick(ui, Icon::Person, "Appel à l'aide", "deux bras levés quelques secondes", &mut cv.gestures);
            pick(ui, Icon::Face, "Visage connu", "dire qui est là (visages enregistrés)", &mut cv.faces);
        }
        if cv != before {
            if let Err(e) = self.hub.set_camera_vision(&c.id, cv.clone(), Source::User) {
                self.toast(format!("{e:#}"), true);
            }
        }
        ui.add_space(14.0);
        if widgets::nav_row(ui, "Avancé", "gestes, zones, scénarios…").clicked() {
            page = Some(1);
        }
        if widgets::nav_row(ui, "Réglages de la caméra", "").clicked() {
            page = Some(2);
        }
        ui.add_space(8.0);
        ui.label(RichText::new("Tout est calculé sur ce PC : aucune image ne sort.").size(11.5).color(theme::faint()));
        page
    }

    /// Onglet « Vision IA » d'une caméra : l'interrupteur et l'état de l'analyse en tête, ce qui
    /// est en vue, puis les réglages rangés par thème (repérer, réagir, écouter, avancé), les
    /// zones, les scénarios et les visages connus.
    pub(in crate::gui) fn vision_section(&mut self, ui: &mut Ui, c: &CameraView) {
        let status = self.vision_status_cached();
        let mut cv = self.hub.camera_vision(&c.id);
        let before = cv.clone();
        let analysis = self.hub.camera_analysis(&c.id);

        // Modèles absents : installation en un clic (sources officielles, empreintes vérifiées).
        if !status.models_ready {
            egui::Frame::new().fill(theme::surface()).corner_radius(CornerRadius::same(12)).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
                ui.label(
                    RichText::new(format!(
                        "Modèles d'IA à installer : sous licence libre ({:.0} Mo), téléchargés depuis leurs sources officielles et vérifiés.",
                        status.missing_mb
                    ))
                    .size(12.5),
                );
                ui.add_space(6.0);
                if status.install.running {
                    ui.horizontal_wrapped(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(&status.install.step).size(12.0).color(theme::muted()));
                    });
                    ui.ctx().request_repaint_after(Duration::from_millis(300));
                } else if ui.add(widgets::primary_button("Installer les modèles", theme::accent())).clicked() {
                    self.hub.install_vision_models(Source::User);
                    self.cams.vision.status = None;
                }
                if let Some(e) = &status.install.error {
                    ui.label(RichText::new(e).size(12.0).color(theme::warn()));
                }
            });
            ui.add_space(8.0);
        }

        widgets::row(ui, "Analyser cette caméra", Some("personnes, gestes, chutes, visages connus"), |ui| {
            ui.add_space(8.0);
            if widgets::toggle(ui, cv.enabled, theme::accent(), status.models_ready).clicked() && status.models_ready {
                cv.enabled = !cv.enabled;
            }
        });
        // État de l'analyse : toujours visible, sous l'interrupteur.
        match &analysis {
            Some(a) => {
                let (text, color) = if let Some(e) = &a.error {
                    (format!("Analyse impossible : {e}"), theme::warn())
                } else if a.starting {
                    ("Chargement des modèles…".to_string(), theme::muted())
                } else if a.rate > 0.0 {
                    (format!("Active · {} · {:.0} analyses/s · {:.0} ms par image", a.backend, a.rate, a.ms), theme::accent())
                } else {
                    (format!("Active · {} · en attente d'images", a.backend), theme::muted())
                };
                ui.horizontal_wrapped(|ui| {
                    if a.starting {
                        ui.spinner();
                    }
                    ui.label(RichText::new(text).size(11.5).color(color));
                });
                if a.starting {
                    ui.ctx().request_repaint_after(Duration::from_millis(300));
                }
                // Présence aujourd'hui : minutes occupées, heure par heure.
                let day = self.hub.presence_day(&c.id, chrono::Local::now().date_naive());
                let total: f32 = day.iter().sum();
                if total > 0.0 || a.running {
                    presence_bars(ui, &day, total);
                }
                if a.objects_missing {
                    ui.label(RichText::new("Animaux et véhicules : leur modèle n'est pas encore installé (bouton ci-dessus).").size(11.5).color(theme::warn()));
                }
            }
            None if !cv.enabled => {
                ui.label(RichText::new("Désactivée : l'image n'est pas analysée. Tout se calcule sur ce PC, rien ne part sur Internet.").size(11.5).color(theme::faint()));
            }
            None => {}
        }

        // Personnes et animaux en vue, avec « Enregistrer ce visage ».
        if let Some(a) = analysis.filter(|a| a.running && (!a.persons.is_empty() || !a.objects.is_empty())) {
            ui.add_space(8.0);
            ui.label(RichText::new("En vue maintenant").size(12.0).color(theme::faint()));
            for o in &a.objects {
                ui.horizontal_wrapped(|ui| {
                    widgets::dot(ui, if o.kind.is_animal() { theme::violet() } else { theme::muted() });
                    let mut label = o.kind.label().to_string();
                    label[..1].make_ascii_uppercase();
                    ui.label(RichText::new(format!("{label} · {:.0} %", o.score * 100.0)).size(12.5));
                });
            }
            for p in &a.persons {
                let (color, label) = badge(p);
                ui.horizontal_wrapped(|ui| {
                    widgets::dot(ui, color);
                    let detail = match &p.verdict {
                        Verdict::Person { similarity, .. } => format!("{label} · {:.0} %", similarity * 100.0),
                        Verdict::Stranger => "Visage inconnu".to_string(),
                        Verdict::Undecided if p.face.is_some() => "Identification…".to_string(),
                        Verdict::Undecided => "Visage pas encore visible".to_string(),
                    };
                    ui.label(RichText::new(detail).size(12.5));
                    let enrolling = self.cams.vision.enroll.as_ref().is_some_and(|(cam, t, _)| cam == &c.id && *t == p.track);
                    if p.embedding.is_some() && !enrolling && ui.small_button("Enregistrer ce visage…").clicked() {
                        let name = match &p.verdict {
                            Verdict::Person { name, .. } => name.clone(),
                            _ => String::new(),
                        };
                        self.cams.vision.enroll = Some((c.id.clone(), p.track, name));
                    }
                });
            }
            ui.ctx().request_repaint_after(Duration::from_millis(300));
        }
        let mut save_enroll = None;
        if let Some((cam, track, name)) = self.cams.vision.enroll.as_mut().filter(|(cam, _, _)| cam == &c.id) {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Nom").size(12.5).color(theme::muted()));
                let r = ui.add(egui::TextEdit::singleline(name).hint_text("ex. Alice").desired_width(140.0));
                let ok = ui.add(widgets::primary_button("Enregistrer", theme::accent())).clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                if ok {
                    save_enroll = Some((cam.clone(), *track, name.clone()));
                }
                if ui.button("Annuler").clicked() {
                    save_enroll = Some((String::new(), 0, String::new()));
                }
            });
            ui.label(RichText::new("Enregistre plusieurs vues (face, profil, lunettes) : la reconnaissance n'en sera que meilleure.").size(11.0).color(theme::faint()));
        }
        if let Some((cam, track, name)) = save_enroll {
            if !cam.is_empty() {
                match self.hub.enroll_face(&cam, track, &name, Source::User) {
                    Ok(()) => self.toast(format!("Visage de {} enregistré", name.trim()), false),
                    Err(e) => self.toast(format!("{e:#}"), true),
                }
            }
            self.cams.vision.enroll = None;
        }

        // Derniers événements de cette caméra (journal, source « Vision IA »).
        let recent = self.vision_events_cached(&c.id);
        if !recent.is_empty() {
            ui.add_space(8.0);
            egui::CollapsingHeader::new(RichText::new("Derniers événements").size(13.0)).id_salt(("vision-recent", &c.id)).default_open(cv.enabled).show(ui, |ui| {
                for (at, level, msg) in &recent {
                    let color = match level {
                        Level::Critical => theme::crit(),
                        Level::Warning => theme::warn(),
                        Level::Info => theme::text(),
                    };
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(at.format("%H:%M:%S").to_string()).size(11.5).color(theme::faint()));
                        ui.label(RichText::new(msg).size(12.0).color(color));
                    });
                }
            });
        }

        // Réglages de l'analyse, rangés par thème.
        if cv.enabled {
            ui.add_space(8.0);
            let detect = [cv.faces, cv.falls, cv.gestures, cv.violence, cv.still_minutes > 0, cv.animals, cv.vehicles];
            let on = detect.iter().filter(|x| **x).count();
            egui::CollapsingHeader::new(RichText::new(format!("Ce que l'IA repère  ·  {on} sur {}", detect.len())).size(13.0))
                .id_salt(("vision-detect", &c.id))
                .default_open(true)
                .show(ui, |ui| {
                    widgets::row(ui, "Visages connus", Some("reconnaît seulement ceux que tu enregistres"), |ui| {
                        ui.add_space(8.0);
                        if widgets::toggle(ui, cv.faces, theme::accent(), true).clicked() {
                            cv.faces = !cv.faces;
                        }
                    });
                    widgets::row(ui, "Chutes", Some("alerte si quelqu'un reste au sol"), |ui| {
                        ui.add_space(8.0);
                        if widgets::toggle(ui, cv.falls, theme::accent(), true).clicked() {
                            cv.falls = !cv.falls;
                        }
                    });
                    widgets::row(ui, "Gestes", Some("main levée ; deux bras levés = appel à l'aide"), |ui| {
                        ui.add_space(8.0);
                        if widgets::toggle(ui, cv.gestures, theme::accent(), true).clicked() {
                            cv.gestures = !cv.gestures;
                        }
                    });
                    let hint = if cv.fps < 8.0 {
                        "coups répétés entre deux personnes ; demande 8 analyses/s au moins"
                    } else if cv.sounds {
                        "coups répétés entre deux personnes (alerte critique, sauf si l'on entend rire)"
                    } else {
                        "coups répétés entre deux personnes ; avec l'écoute des sons, une chamaillerie où l'on rit ne sonne pas"
                    };
                    widgets::row(ui, "Violence", Some(hint), |ui| {
                        ui.add_space(8.0);
                        if widgets::toggle(ui, cv.violence, theme::accent(), true).clicked() {
                            cv.violence = !cv.violence;
                        }
                    });
                    widgets::row(ui, "Animaux", Some("chat, chien, oiseau…"), |ui| {
                        ui.add_space(8.0);
                        if widgets::toggle(ui, cv.animals, theme::accent(), true).clicked() {
                            cv.animals = !cv.animals;
                        }
                    });
                    widgets::row(ui, "Véhicules", Some("voiture, vélo, moto, camion (dehors)"), |ui| {
                        ui.add_space(8.0);
                        if widgets::toggle(ui, cv.vehicles, theme::accent(), true).clicked() {
                            cv.vehicles = !cv.vehicles;
                        }
                    });
                    ui.add_space(2.0);
                    ui.label(RichText::new("Immobilité prolongée").size(14.0));
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        for (m, label) in [(0u32, "Non"), (10, "10 min"), (30, "30 min"), (60, "1 h")] {
                            if ui.add(egui::Button::selectable(cv.still_minutes == m, label)).on_hover_text("Signale une personne qui ne bouge plus du tout pendant ce temps").clicked() {
                                cv.still_minutes = m;
                            }
                        }
                    });
                    ui.label(RichText::new("alerte si une personne ne bouge plus du tout pendant ce temps").size(11.0).color(theme::faint()));
                });
            egui::CollapsingHeader::new(RichText::new("Réactions").size(13.0)).id_salt(("vision-react", &c.id)).default_open(true).show(ui, |ui| {
                if c.ptz == Some(true) {
                    widgets::row(ui, "Suivre la personne", Some("la caméra tourne pour la garder au centre ; pause dès que tu la pilotes"), |ui| {
                        ui.add_space(8.0);
                        if widgets::toggle(ui, cv.follow, theme::accent(), true).clicked() {
                            cv.follow = !cv.follow;
                        }
                    });
                    if cv.follow {
                        ui.checkbox(&mut cv.follow_tilt, "Suivre aussi en hauteur (si la caméra l'accepte)");
                    }
                }
                widgets::row(ui, "Photo des événements", Some("PNG dans Images\\HomeSmart\\Vision"), |ui| {
                    ui.add_space(8.0);
                    if widgets::toggle(ui, cv.snapshots, theme::accent(), true).clicked() {
                        cv.snapshots = !cv.snapshots;
                    }
                });
                ui.label(RichText::new("Chaque événement va au journal (onglet Alertes) ; les plus graves s'affichent aussi en notification.").size(11.0).color(theme::faint()));
            });
            let listening = [cv.sounds, cv.speech].iter().filter(|x| **x).count();
            let state = if listening == 0 { "coupée".to_string() } else { format!("{listening} sur 2") };
            egui::CollapsingHeader::new(RichText::new(format!("Écoute du micro  ·  {state}")).size(13.0))
                .id_salt(("vision-listen", &c.id))
                .default_open(listening > 0)
                .show(ui, |ui| self.listen_settings(ui, c, &mut cv, &status));
        }

        // Réglages avancés : cadence et qualité de l'analyse, calcul (communs à toutes les caméras).
        egui::CollapsingHeader::new(RichText::new("Réglages avancés").size(13.0)).id_salt(("vision-advanced", &c.id)).default_open(false).show(ui, |ui| {
            if cv.enabled {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Image analysée").size(12.5).color(theme::muted()));
                    if let Some(i) = widgets::segmented(ui, &["Fluide", "HD"], Some(cv.hd as usize), theme::accent()) {
                        cv.hd = i == 1;
                    }
                });
                ui.label(RichText::new("HD : visages reconnus de plus loin, mais plus de calcul (et FFmpeg pour le H.265).").size(11.0).color(theme::faint()));
                ui.add_space(4.0);
                ui.label(RichText::new("Analyses par seconde").size(12.5).color(theme::muted()));
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    // Au-delà de la cadence de la caméra (25 i/s en HD sur une Imou), chaque image
                    // n'est analysée qu'une fois : 30 revient alors à « toutes les images ».
                    for r in [2.0f32, 4.0, 8.0, 12.0, 16.0, 24.0, 30.0] {
                        let on = (r - cv.fps).abs() < 0.5;
                        if ui.add(egui::Button::selectable(on, format!("{r:.0}"))).on_hover_text("Plus d'analyses : cadres plus fluides et chutes mieux vues (8 conseillé), plus de calcul").clicked() {
                            cv.fps = r;
                        }
                    }
                });
                ui.label(RichText::new("8 conseillé : davantage suit mieux les gestes, mais demande plus de calcul.").size(11.0).color(theme::faint()));
                ui.add_space(6.0);
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Calcul").size(12.5).color(theme::muted()));
                let sel = match status.accel {
                    Accel::Auto => 0,
                    Accel::Cpu => 1,
                };
                if let Some(i) = widgets::segmented(ui, &["Carte graphique", "Processeur"], Some(sel), theme::accent()) {
                    self.hub.set_vision_accel(if i == 0 { Accel::Auto } else { Accel::Cpu }, Source::User);
                    self.cams.vision.status = None;
                }
            })
            .response
            .on_hover_text("Carte graphique : DirectML sous Windows (NVIDIA, AMD, Intel), avec repli automatique sur le processeur.");
            ui.horizontal_wrapped(|ui| {
                use homesmart::vision::PersonModel;
                ui.label(RichText::new("Précision").size(12.5).color(theme::muted()));
                let sel = if status.person_model == PersonModel::High { 1 } else { 0 };
                if let Some(i) = widgets::segmented(ui, &["Standard", "Élevée"], Some(sel), theme::accent()) {
                    self.hub.set_vision_person_model(if i == 1 { PersonModel::High } else { PersonModel::Standard }, Source::User);
                    self.cams.vision.status = None;
                }
            })
            .response
            .on_hover_text("Élevée : modèle plus gros (RTMO-m, 83 Mo à télécharger), plus juste surtout de nuit — mesuré : 29 % de fausses personnes en moins, chutes de nuit mieux vues. Deux fois plus de calcul sur le processeur (≈ 70 ms par image), presque rien sur la carte graphique.");
            ui.label(RichText::new("Calcul et précision valent pour toutes les caméras. « Élevée » est plus juste de nuit (83 Mo à télécharger).").size(11.0).color(theme::faint()));
            if cv.enabled || !cv.zones.is_empty() {
                ui.add_space(4.0);
                ui.checkbox(&mut self.cams.vision.hide_overlay, "Masquer les cadres et les zones sur la vidéo");
            }
        });
        if cv != before {
            if let Err(e) = self.hub.set_camera_vision(&c.id, cv.clone(), Source::User) {
                self.toast(format!("{e:#}"), true);
            }
        }

        // Zones dessinées sur la vidéo (entrée, sortie, présence prolongée).
        ui.add_space(8.0);
        self.zones_list(ui, c);

        // Scénarios : la caméra commande des appareils.
        if self.cams.open_scenarios {
            ui.scroll_to_cursor(Some(egui::Align::TOP));
        }
        self.scenarios_section(ui, c, &cv);

        // Visages connus (communs à toutes les caméras).
        let known = self.hub.known_faces();
        ui.add_space(8.0);
        egui::CollapsingHeader::new(RichText::new(format!("Visages connus ({})", known.len())).size(13.0)).id_salt("vision-known").show(ui, |ui| {
            if known.is_empty() {
                ui.label(RichText::new("Personne pour l'instant : active l'analyse, puis « Enregistrer ce visage » quand quelqu'un passe.").size(12.0).color(theme::muted()));
            }
            ui.label(RichText::new("Ou glisse la photo d'un visage (JPEG, PNG) sur la fenêtre.").size(11.5).color(theme::faint()));
            let (mut rename, mut delete) = (None, None);
            for k in &known {
                ui.horizontal_wrapped(|ui| {
                    match self.cams.vision.rename.as_mut().filter(|(id, _)| id == &k.id) {
                        Some((_, name)) => {
                            ui.add(egui::TextEdit::singleline(name).desired_width(120.0));
                            if ui.small_button("OK").clicked() {
                                rename = Some((k.id.clone(), name.clone()));
                            }
                        }
                        None => {
                            ui.label(RichText::new(&k.name).size(13.0));
                            ui.label(RichText::new(format!("{} vue{}", k.samples, if k.samples > 1 { "s" } else { "" })).size(11.5).color(theme::faint()));
                            if ui.small_button("Renommer").clicked() {
                                self.cams.vision.rename = Some((k.id.clone(), k.name.clone()));
                            }
                        }
                    }
                    if self.cams.vision.confirm_delete.as_deref() == Some(k.id.as_str()) {
                        if ui.add(egui::Button::new(RichText::new("Supprimer vraiment").color(theme::crit())).small()).clicked() {
                            delete = Some(k.id.clone());
                        }
                    } else if ui.small_button("Supprimer").clicked() {
                        self.cams.vision.confirm_delete = Some(k.id.clone());
                    }
                });
            }
            if let Some((id, name)) = rename {
                if let Err(e) = self.hub.rename_face(&id, &name, Source::User) {
                    self.toast(format!("{e:#}"), true);
                }
                self.cams.vision.rename = None;
            }
            if let Some(id) = delete {
                if let Err(e) = self.hub.remove_face(&id, Source::User) {
                    self.toast(format!("{e:#}"), true);
                }
                self.cams.vision.confirm_delete = None;
            }
        });
        ui.add_space(10.0);
        ui.label(
            RichText::new("Tout reste sur ce PC. Aucun visage n'est enregistré sans ton accord, et les inconnus ne sont pas mémorisés.")
                .size(11.0)
                .color(theme::faint()),
        );
    }
}
