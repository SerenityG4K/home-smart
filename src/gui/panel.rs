//! Onglets du panneau d'un appareil : minuterie, énergie, programmes, sécurité, options.

use super::{charts, theme, widgets, App};
use chrono::{Datelike, Local, Timelike};
use eframe::egui::{self, vec2, Align, Color32, CornerRadius, Layout, Margin, RichText, Ui};
use homesmart::automation::{self, Action, Safety, Schedule, TimeRange};
use homesmart::hub::DeviceView;
use homesmart::kinds::{self, Kind};
use serde_json::{json, Value};
use widgets::fr;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanelTab {
    Control,
    Energy,
    Programs,
    Safety,
    Options,
}

// ---- Formulaires -------------------------------------------------------------------

pub struct SchedForm {
    pub id: u64,
    pub device: String,
    /// Aspirateur robot : nettoyer / retour à la base au lieu d'allumer / éteindre.
    pub vacuum: bool,
    /// 0 = plage (allumé de… à…), 1 = allumer à, 2 = éteindre à
    /// (aspirateur : nettoyer de… à…, nettoyer à, retour à la base à).
    pub mode: usize,
    pub days: [bool; 7],
    pub start: (u32, u32),
    pub end: (u32, u32),
    pub error: Option<String>,
}

fn hm(s: &str) -> (u32, u32) {
    automation::parse_hm(s).map(|t| (t.hour(), t.minute())).unwrap_or((7, 0))
}

fn hm_str(t: (u32, u32)) -> String {
    format!("{:02}:{:02}", t.0, t.1)
}

impl SchedForm {
    pub fn new(device: &str, vacuum: bool) -> Self {
        let (start, end) = if vacuum { ((10, 0), (11, 30)) } else { ((6, 30), (8, 0)) };
        SchedForm { id: 0, device: device.to_string(), vacuum, mode: if vacuum { 1 } else { 0 }, days: [true, true, true, true, true, false, false], start, end, error: None }
    }

    pub fn from(s: &Schedule, vacuum: bool) -> Self {
        SchedForm {
            id: s.id,
            device: s.device.clone(),
            vacuum: vacuum || s.action.for_vacuum(),
            mode: match (&s.end, s.action) {
                (Some(_), _) => 0,
                (None, Action::On | Action::Clean) => 1,
                (None, Action::Off | Action::Dock) => 2,
            },
            days: s.days,
            start: hm(&s.start),
            end: s.end.as_deref().map(hm).unwrap_or((8, 0)),
            error: None,
        }
    }

    fn to_schedule(&self, enabled: bool) -> Schedule {
        Schedule {
            id: self.id,
            device: self.device.clone(),
            enabled,
            days: self.days,
            start: hm_str(self.start),
            end: (self.mode == 0).then(|| hm_str(self.end)),
            action: match (self.vacuum, self.mode == 2) {
                (false, false) => Action::On,
                (false, true) => Action::Off,
                (true, false) => Action::Clean,
                (true, true) => Action::Dock,
            },
        }
    }

    fn labels(&self) -> [&'static str; 3] {
        if self.vacuum {
            ["Nettoyage de… à…", "Nettoyer à", "Retour à la base à"]
        } else {
            ["Plage horaire", "Allumer à", "Éteindre à"]
        }
    }
}

/// Un appareil qu'un programme peut commander (prises, lampes… et aspirateurs robots).
#[derive(Clone, Debug)]
pub struct Target {
    pub id: String,
    pub name: String,
    pub vacuum: bool,
}

pub struct SafetyForm {
    pub device: String,
    pub s: Safety,
    max_on: u32,
    power: f64,
    f_start: (u32, u32),
    f_end: (u32, u32),
    dirty: bool,
}

impl SafetyForm {
    fn new(device: &str, s: &Safety) -> Self {
        SafetyForm {
            device: device.to_string(),
            max_on: s.max_on_minutes.unwrap_or(240),
            power: s.max_power_w.unwrap_or(3000.0),
            f_start: s.forbidden.as_ref().map(|r| hm(&r.start)).unwrap_or((23, 0)),
            f_end: s.forbidden.as_ref().map(|r| hm(&r.end)).unwrap_or((6, 0)),
            s: s.clone(),
            dirty: false,
        }
    }

    fn to_safety(&self) -> Safety {
        let mut s = self.s.clone();
        if s.max_on_minutes.is_some() {
            s.max_on_minutes = Some(self.max_on.max(1));
        }
        if s.max_power_w.is_some() {
            s.max_power_w = Some(self.power.max(1.0));
        }
        if s.forbidden.is_some() {
            s.forbidden = Some(TimeRange { start: hm_str(self.f_start), end: hm_str(self.f_end) });
        }
        s
    }
}

/// Saisie d'une heure « HH : MM ».
pub fn time_edit(ui: &mut Ui, t: &mut (u32, u32)) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        changed |= ui.add(egui::DragValue::new(&mut t.0).range(0..=23).speed(0.15).custom_formatter(|v, _| format!("{:02}", v as u32))).changed();
        ui.label(":");
        changed |= ui.add(egui::DragValue::new(&mut t.1).range(0..=59).speed(0.3).custom_formatter(|v, _| format!("{:02}", v as u32))).changed();
    });
    changed
}

pub fn fmt_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    match (h, m) {
        (0, 0) => format!("{s} s"),
        (0, m) => format!("{m} min {s:02} s"),
        (h, m) => format!("{h} h {m:02} min"),
    }
}

fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(theme::surface())
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn note(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(11.5).color(theme::faint()));
}

impl App {
    /// Les appareils et les aspirateurs robots (ceux hors Tuya après les autres).
    pub(super) fn targets(&self, devices: &[DeviceView]) -> Vec<Target> {
        let mut out: Vec<Target> = devices.iter().map(|d| Target { id: d.id.clone(), name: d.name.clone(), vacuum: Self::kind_of_view(d) == Kind::Vacuum }).collect();
        for v in self.hub.vacuums() {
            if !out.iter().any(|t| t.id == v.id) {
                out.push(Target { id: v.id, name: v.name, vacuum: true });
            }
        }
        out
    }

    fn switch_on(d: &DeviceView, kind: &Kind) -> Option<bool> {
        let dp = match kind {
            Kind::Switch { gangs } => gangs.first().cloned(),
            k => k.switch_dp(&d.dps),
        }?;
        d.dps.get(&dp).and_then(Value::as_bool)
    }

    fn spawn_countdown(&self, d: &DeviceView, secs: u64) {
        let (hub, toasts, ctx, id, name) = (self.hub.clone(), self.toasts.clone(), self.ctx.clone(), d.id.clone(), d.name.clone());
        self.rt.spawn(async move {
            if let Err(e) = hub.set_countdown(&id, secs, homesmart::hub::Source::User).await {
                toasts.lock().unwrap().push(super::Toast { text: format!("{name} : {e:#}"), error: true, at: std::time::Instant::now() });
                ctx.request_repaint();
            }
        });
    }

    // ---- Contrôle : minuterie et défauts --------------------------------------------

    pub(super) fn faults_banner(&mut self, ui: &mut Ui, d: &DeviceView) {
        if d.faults.is_empty() {
            return;
        }
        egui::Frame::new().fill(theme::crit().gamma_multiply(0.14)).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(format!("La prise signale : {}", d.faults.join(", "))).strong().color(theme::crit()));
            note(ui, "Vérifie l'appareil branché avant de le rallumer.");
        });
        ui.add_space(10.0);
    }

    /// Minuterie sur une ligne (exécutée par la prise elle-même, même PC éteint).
    pub(super) fn timer_line(&mut self, ui: &mut Ui, d: &DeviceView, kind: &Kind) {
        if kinds::countdown_dp(kind, &d.dps, &d.schema).is_none() {
            return;
        }
        let on = Self::switch_on(d, kind).unwrap_or(false);
        ui.add_space(16.0);
        if let Some(left) = d.countdown_left {
            ui.horizontal(|ui| {
                let what = if on { "S'éteindra dans" } else { "S'allumera dans" };
                ui.label(RichText::new(format!("{what} {}", fmt_duration(left))).size(14.5).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Annuler").clicked() {
                        self.spawn_countdown(d, 0);
                    }
                });
            });
            ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
            return;
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.spacing_mut().button_padding = vec2(10.0, 7.0);
            ui.label(RichText::new(if on { "Éteindre dans" } else { "Allumer dans" }).size(13.5).color(theme::muted()))
                .on_hover_text("Minuterie exécutée par la prise elle-même : elle fonctionne même si le PC est éteint.");
            for (label, mins) in [("30 min", 30u64), ("1 h", 60), ("2 h", 120), ("4 h", 240)] {
                if ui.add(egui::Button::new(label).min_size(vec2(0.0, 34.0))).clicked() {
                    self.spawn_countdown(d, mins * 60);
                }
            }
            if ui.add(egui::Button::selectable(self.timer_custom, "…").min_size(vec2(34.0, 34.0))).on_hover_text("Autre durée").clicked() {
                self.timer_custom = !self.timer_custom;
            }
        });
        if self.timer_custom {
            ui.horizontal(|ui| {
                let (h, m) = &mut self.countdown_custom;
                ui.add(egui::DragValue::new(h).range(0..=23).suffix(" h"));
                ui.add(egui::DragValue::new(m).range(0..=59).suffix(" min"));
                let secs = (*h as u64 * 60 + *m as u64) * 60;
                if ui.add_enabled(secs > 0, egui::Button::new("Lancer")).clicked() {
                    self.spawn_countdown(d, secs);
                    self.timer_custom = false;
                }
            });
        }
    }

    // ---- Énergie ------------------------------------------------------------------------

    pub(super) fn energy_tab(&mut self, ui: &mut Ui, d: &DeviceView, accent: Color32) {
        let today = Local::now().date_naive();
        let price = self.hub.prefs().price_kwh;
        let id = d.id.clone();
        let (today_wh, month_wh, prev_wh, curve) =
            self.hub.energy(|e| (e.day_wh(&id, today), e.month_wh(&id, today), e.previous_month_wh(&id, today), e.curve(&id, today)));
        let now_w = kinds::power_w(&d.dps, &d.schema).unwrap_or(0.0);
        let width = (ui.available_width() - 16.0) / 3.0;
        ui.horizontal(|ui| {
            widgets::stat_value(ui, ("now", &d.id), "Maintenant", now_w, 0, "W", accent, width);
            widgets::stat_value(ui, ("today", &d.id), "Aujourd'hui", today_wh / 1000.0, 2, "kWh", theme::text(), width);
            widgets::stat_value(ui, ("month", &d.id), "Ce mois-ci", month_wh / 1000.0, 1, "kWh", theme::text(), width);
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Coût : {} € aujourd'hui  ·  {} € ce mois-ci", fr(today_wh / 1000.0 * price, 2), fr(month_wh / 1000.0 * price, 2))).color(theme::muted()));
        });
        if prev_wh > 0.0 {
            note(ui, &format!("Mois dernier : {} kWh ({} €)", fr(prev_wh / 1000.0, 1), fr(prev_wh / 1000.0 * price, 2)));
        }
        ui.add_space(12.0);
        ui.label(RichText::new("Puissance aujourd'hui").strong());
        let now = Local::now();
        let slot = ((now.hour() * 60 + now.minute()) / 5) as usize + 1;
        charts::power_curve(ui, curve.as_deref().unwrap_or(&[]), slot, ui.available_width(), accent);
        ui.add_space(12.0);
        ui.label(RichText::new("Consommation").font(theme::bold(14.0)));
        ui.add_space(4.0);
        self.device_energy_chart(ui, d);
        note(ui, "Calculé par HomeSmart à partir des mesures de la prise, pendant que l'application est ouverte. Prix du kWh modifiable dans les préférences.");
    }

    // ---- Programmes ---------------------------------------------------------------------

    pub(super) fn programs_tab(&mut self, ui: &mut Ui, device: &str, vacuum: bool, accent: Color32) {
        let list: Vec<Schedule> = self.hub.schedules().into_iter().filter(|s| s.device == device).collect();
        if self.sched_form.as_ref().is_some_and(|f| f.device == device) {
            self.schedule_form_ui(ui, None, accent);
            ui.add_space(10.0);
        }
        if list.is_empty() && self.sched_form.is_none() {
            card(ui, |ui| {
                ui.label(RichText::new("Aucun programme").strong());
                note(ui, if vacuum { "Exemple : nettoyer à 10 h en semaine, quand la maison est vide." } else { "Exemple : allumer le chauffage de 6 h 30 à 8 h en semaine." });
            });
            ui.add_space(8.0);
        }
        for s in &list {
            self.schedule_row(ui, s, None, accent);
            ui.add_space(6.0);
        }
        if self.sched_form.is_none() && ui.add(widgets::primary_button("+  Nouveau programme", accent).min_size(vec2(0.0, 30.0))).clicked() {
            self.sched_form = Some(SchedForm::new(device, vacuum));
        }
        ui.add_space(8.0);
        note(ui, "Les programmes sont exécutés par HomeSmart : laisse l'application ouverte (même réduite) et active le lancement automatique dans les préférences.");
    }

    /// Une ligne de programme (activation, résumé, prochain déclenchement, actions).
    pub(super) fn schedule_row(&mut self, ui: &mut Ui, s: &Schedule, device_name: Option<&str>, accent: Color32) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                if widgets::toggle(ui, s.enabled, accent, true).on_hover_text("Activer / suspendre").clicked() {
                    let mut x = s.clone();
                    x.enabled = !x.enabled;
                    if let Err(e) = self.hub.save_schedule(x) {
                        self.toast(format!("{e:#}"), true);
                    }
                }
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    if let Some(n) = device_name {
                        ui.label(RichText::new(n).size(12.0).color(theme::muted()));
                    }
                    ui.add(egui::Label::new(RichText::new(s.summary()).strong().color(if s.enabled { theme::text() } else { theme::faint() })).wrap());
                    if let Some(ev) = s.enabled.then(|| s.next_event(Local::now().naive_local())).flatten() {
                        let wd = automation::DAY_NAMES[ev.at.weekday().num_days_from_monday() as usize];
                        note(ui, &format!("Prochain : {wd} {} à {} ({})", ev.at.format("%d/%m"), automation::fmt_hm(ev.at.time()), ev.action.noun()));
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Supprimer").clicked() {
                        self.hub.delete_schedule(s.id);
                    }
                    if ui.small_button("Modifier").clicked() {
                        self.sched_form = Some(SchedForm::from(s, self.hub.is_vacuum(&s.device)));
                    }
                });
            });
        });
    }

    /// Formulaire de création / modification. `devices` permet de choisir l'appareil (vue calendrier).
    pub(super) fn schedule_form_ui(&mut self, ui: &mut Ui, devices: Option<&[Target]>, accent: Color32) {
        let Some(mut f) = self.sched_form.take() else { return };
        let mut keep = true;
        card(ui, |ui| {
            ui.label(RichText::new(if f.id == 0 { "Nouveau programme" } else { "Modifier le programme" }).size(14.5).strong());
            ui.add_space(6.0);
            if let Some(list) = devices {
                ui.horizontal(|ui| {
                    ui.label("Appareil");
                    let current = list.iter().find(|d| d.id == f.device).map(|d| d.name.clone()).unwrap_or_else(|| "Choisir…".into());
                    egui::ComboBox::from_id_salt("sched-dev").selected_text(current).width(220.0).show_ui(ui, |ui| {
                        for d in list {
                            if ui.selectable_value(&mut f.device, d.id.clone(), &d.name).clicked() && f.vacuum != d.vacuum {
                                f.vacuum = d.vacuum;
                            }
                        }
                    });
                });
            }
            if let Some(i) = widgets::segmented(ui, &f.labels(), Some(f.mode), accent) {
                f.mode = i;
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for i in 0..7 {
                    let sel = f.days[i];
                    let b = egui::Button::new(RichText::new(automation::DAY_LETTERS[i]).strong().color(if sel { theme::bg() } else { theme::muted() }))
                        .fill(if sel { accent } else { theme::raised() })
                        .corner_radius(CornerRadius::same(14))
                        .min_size(vec2(30.0, 28.0));
                    if ui.add(b).on_hover_text(automation::DAY_NAMES[i]).clicked() {
                        f.days[i] = !sel;
                    }
                }
            });
            ui.horizontal(|ui| {
                for (label, days) in [
                    ("Semaine", [true, true, true, true, true, false, false]),
                    ("Week-end", [false, false, false, false, false, true, true]),
                    ("Tous", [true; 7]),
                ] {
                    if ui.small_button(label).clicked() {
                        f.days = days;
                    }
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(match (f.vacuum, f.mode) {
                    (false, 0) => "Allumé de",
                    (false, 1) => "Allumer à",
                    (false, _) => "Éteindre à",
                    (true, 0) => "Nettoyer de",
                    (true, 1) => "Nettoyer à",
                    (true, _) => "Retour à la base à",
                });
                time_edit(ui, &mut f.start);
                if f.mode == 0 {
                    ui.label("à");
                    time_edit(ui, &mut f.end);
                    if f.end <= f.start {
                        ui.label(RichText::new("(le lendemain)").size(11.5).color(theme::faint()));
                    }
                }
            });
            if let Some(e) = &f.error {
                ui.label(RichText::new(e).color(theme::crit()));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.add(widgets::primary_button("Enregistrer", accent)).clicked() {
                    let enabled = self.hub.schedules().iter().find(|s| s.id == f.id).is_none_or(|s| s.enabled);
                    match self.hub.save_schedule(f.to_schedule(enabled)) {
                        Ok(_) => {
                            self.toast("Programme enregistré", false);
                            keep = false;
                        }
                        Err(e) => f.error = Some(format!("{e:#}")),
                    }
                }
                if ui.button("Annuler").clicked() {
                    keep = false;
                }
            });
        });
        if keep {
            self.sched_form = Some(f);
        }
    }

    // ---- Sécurité -----------------------------------------------------------------------

    pub(super) fn safety_tab(&mut self, ui: &mut Ui, d: &DeviceView, kind: &Kind, _accent: Color32) {
        if self.safety_form.as_ref().is_none_or(|f| f.device != d.id) {
            self.safety_form = Some(SafetyForm::new(&d.id, &d.safety));
        }
        let Some(mut f) = self.safety_form.take() else { return };
        let has_countdown = kinds::countdown_dp(kind, &d.dps, &d.schema).is_some();
        let metering = kinds::power_w(&d.dps, &d.schema).is_some();
        let has_fault = d.schema.values().any(|s| s.get("code").and_then(Value::as_str) == Some("fault"));
        let mut save = false;

        if !f.s.enabled && automation::looks_risky(&d.name) {
            egui::Frame::new().fill(theme::warn().gamma_multiply(0.14)).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(12)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(format!("« {} » ressemble à un appareil de chauffage.", d.name)).strong().color(theme::warn()));
                note(ui, "Réglages conseillés : confirmation avant d'allumer, arrêt automatique après 4 h, coupure au-delà de 3 000 W et en cas de défaut.");
                ui.add_space(4.0);
                if ui.add(widgets::primary_button("Appliquer les réglages recommandés", theme::accent())).clicked() {
                    f = SafetyForm::new(&d.id, &Safety::heater_preset());
                    save = true;
                }
            });
            ui.add_space(10.0);
        }

        card(ui, |ui| {
            widgets::row(ui, "Activer les garde-fous", Some("pour un appareil à risque : chauffage, fer, four…"), |ui| {
                if widgets::toggle(ui, f.s.enabled, theme::accent(), true).clicked() {
                    f.s.enabled = !f.s.enabled;
                    f.dirty = true;
                }
            });
            ui.add_space(6.0);
            ui.add_enabled_ui(f.s.enabled, |ui| {
                ui.separator();
                f.dirty |= ui.checkbox(&mut f.s.confirm_on, "Demander confirmation avant d'allumer").changed();

                let mut use_max = f.s.max_on_minutes.is_some();
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut use_max, "Éteindre automatiquement après").changed() {
                        f.s.max_on_minutes = use_max.then_some(f.max_on);
                        f.dirty = true;
                    }
                    f.dirty |= ui.add_enabled(use_max, egui::DragValue::new(&mut f.max_on).range(1..=1440).speed(1.0).suffix(" min")).changed();
                    if use_max {
                        ui.label(RichText::new(format!("({})", automation::fmt_minutes(f.max_on))).color(theme::faint()));
                    }
                });
                note(
                    ui,
                    if has_countdown {
                        "    Programmé dans la minuterie de la prise : coupe même si HomeSmart est fermé."
                    } else {
                        "    Assuré par HomeSmart : il doit rester ouvert."
                    },
                );

                if metering {
                    let mut use_p = f.s.max_power_w.is_some();
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut use_p, "Couper si la puissance dépasse").changed() {
                            f.s.max_power_w = use_p.then_some(f.power);
                            f.dirty = true;
                        }
                        f.dirty |= ui.add_enabled(use_p, egui::DragValue::new(&mut f.power).range(10.0..=3680.0).speed(10.0).suffix(" W")).changed();
                    });
                    let every = fr(self.hub.prefs().meter_interval_s, 0);
                    note(ui, &format!("    Vérifié à chaque mesure (toutes les {every} s), coupure après deux dépassements de suite."));
                }

                let mut use_f = f.s.forbidden.is_some();
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut use_f, "Interdire l'allumage de").changed() {
                        f.s.forbidden = use_f.then(|| TimeRange { start: hm_str(f.f_start), end: hm_str(f.f_end) });
                        f.dirty = true;
                    }
                    ui.add_enabled_ui(use_f, |ui| {
                        f.dirty |= time_edit(ui, &mut f.f_start);
                        ui.label("à");
                        f.dirty |= time_edit(ui, &mut f.f_end);
                    });
                });
                note(ui, "    S'il est allumé pendant cette plage, il est éteint, et les programmes ne l'allument pas.");

                if has_fault {
                    f.dirty |= ui.checkbox(&mut f.s.off_on_fault, "Couper si la prise signale un défaut (surintensité, surtension…)").changed();
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(f.dirty, widgets::primary_button("Enregistrer", theme::accent())).clicked() {
                    save = true;
                }
                if f.dirty {
                    ui.label(RichText::new("Modifications non enregistrées").size(12.0).color(theme::warn()));
                }
            });
        });

        // Comportement au retour du courant : réglage de l'appareil lui-même.
        if let Some((dp, _)) = d.schema.iter().find(|(_, s)| s.get("code").and_then(Value::as_str) == Some("relay_status")) {
            let dp = dp.clone();
            ui.add_space(10.0);
            card(ui, |ui| {
                ui.label(RichText::new("Après une coupure de courant").strong());
                self.enum_combo(ui, d, &dp, "relay_status");
                let current = d.dps.get(&dp).and_then(Value::as_str);
                let range = kinds::enum_range(&d.schema, &dp);
                // Valeur « rester éteinte » dans le vocabulaire de cette prise (power_off ou off).
                let off = range
                    .iter()
                    .position(|v| v == "power_off")
                    .and_then(|i| kinds::local_enum_range(current, &range).and_then(|l| l.get(i).cloned()));
                if let (true, Some(off)) = (f.s.enabled, off) {
                    if current != Some(off.as_str()) {
                        ui.add_space(4.0);
                        ui.label(RichText::new("Pour un appareil à risque, il est plus sûr qu'il reste éteint au retour du courant.").size(12.0).color(theme::warn()));
                        if ui.button("Régler sur « Rester éteinte »").clicked() {
                            self.set(&d.id, &dp, json!(off));
                        }
                    }
                }
                note(ui, "Réglage enregistré dans la prise : il s'applique même sans HomeSmart.");
            });
        }

        if save {
            match self.hub.set_safety(&d.id, f.to_safety()) {
                Ok(()) => {
                    self.toast("Garde-fous enregistrés", false);
                    f = SafetyForm::new(&d.id, &f.to_safety());
                }
                Err(e) => self.toast(format!("{e:#}"), true),
            }
        }
        ui.add_space(8.0);
        note(ui, "Les garde-fous agissent tant que HomeSmart est ouvert ; la minuterie de sécurité et le comportement après coupure sont gérés par la prise elle-même.");
        self.safety_form = Some(f);
    }

    // ---- Options de l'appareil ----------------------------------------------------------

    /// Liste déroulante pour un DP énuméré, avec libellés français.
    pub(super) fn enum_combo(&mut self, ui: &mut Ui, d: &DeviceView, dp: &str, code: &str) {
        let current = self.text(d, dp).unwrap_or_default();
        let Some(range) = kinds::local_enum_range(d.dps.get(dp).and_then(Value::as_str), &kinds::enum_range(&d.schema, dp)) else {
            ui.label(RichText::new(kinds::enum_label(code, &current)).strong())
                .on_hover_text("Valeurs de l'appareil inconnues : modification bloquée par prudence");
            return;
        };
        let mut chosen = None;
        egui::ComboBox::from_id_salt(("enum-opt", &d.id, dp)).selected_text(kinds::enum_label(code, &current)).width(240.0).show_ui(ui, |ui| {
            for v in &range {
                if ui.selectable_label(*v == current, kinds::enum_label(code, v)).clicked() {
                    chosen = Some(v.clone());
                }
            }
        });
        if let Some(c) = chosen {
            self.set(&d.id, dp, json!(c));
        }
    }

    pub(super) fn options_tab(&mut self, ui: &mut Ui, d: &DeviceView, accent: Color32) {
        let find = |code: &str| d.schema.iter().find(|(_, s)| s.get("code").and_then(Value::as_str) == Some(code)).map(|(k, _)| k.clone());
        // « Après une coupure de courant » est déjà dans la partie Sécurité de la même page : une
        // deuxième liste aurait le même identifiant et aucune des deux ne répondrait.
        let light = find("light_mode");
        if light.is_some() {
            card(ui, |ui| {
                ui.label(RichText::new("Options de l'appareil").size(14.5).strong());
                ui.add_space(6.0);
                if let Some(dp) = &light {
                    ui.label("Voyant lumineux");
                    self.enum_combo(ui, d, dp, "light_mode");
                }
            });
            ui.add_space(10.0);
        }
        ui.add_enabled_ui(d.online, |ui| {
            egui::CollapsingHeader::new(RichText::new("Tous les points de données (avancé)").color(theme::muted()))
                .id_salt(("dps", &d.id))
                .show(ui, |ui| self.dp_editor(ui, d, accent));
        });
        egui::CollapsingHeader::new(RichText::new("Connexion et identité").color(theme::muted()))
            .id_salt(("settings", &d.id))
            .default_open(!d.online)
            .show(ui, |ui| self.settings_ui(ui, d));
    }
}
