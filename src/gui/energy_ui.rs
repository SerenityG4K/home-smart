//! Énergie : un grand chiffre pour la période choisie, un bâton par heure, jour ou mois, et ce
//! qui consomme le plus.

use super::{theme, widgets, App};
use chrono::{Datelike, Duration as ChronoDuration, Local, NaiveDate};
use eframe::egui::{self, pos2, vec2, Align2, CornerRadius, FontId, Rect, RichText, ScrollArea, Sense, Ui};
use homesmart::automation::DAY_NAMES;
use homesmart::hub::DeviceView;
use homesmart::kinds;
use widgets::fr;

const MONTHS: [&str; 12] = ["janv.", "févr.", "mars", "avr.", "mai", "juin", "juil.", "août", "sept.", "oct.", "nov.", "déc."];
const MONTHS_LONG: [&str; 12] = ["janvier", "février", "mars", "avril", "mai", "juin", "juillet", "août", "septembre", "octobre", "novembre", "décembre"];
const PERIODS: [&str; 4] = ["Jour", "Semaine", "Mois", "Année"];

/// Période affichée : 0 aujourd'hui (par heure), 1 sept derniers jours, 2 trente derniers jours
/// (par jour), 3 douze derniers mois.
pub struct EnergyChart {
    pub period: usize,
}

impl EnergyChart {
    pub fn new(period: usize) -> Self {
        EnergyChart { period }
    }
}

/// « aujourd'hui », « sur 7 jours »… pour accompagner un total.
fn period_words(period: usize) -> &'static str {
    match period {
        0 => "aujourd'hui",
        1 => "sur 7 jours",
        2 => "sur 30 jours",
        _ => "sur 12 mois",
    }
}

/// Données d'une période : étiquettes courtes (sous les bâtons), longues (survol), et les kWh de
/// chaque appareil par intervalle.
pub fn build(app: &App, devices: &[&DeviceView], period: usize) -> (Vec<String>, Vec<String>, Vec<(String, Vec<f64>)>) {
    let today = Local::now().date_naive();
    let day_short = |d: NaiveDate| if d == today { "auj.".to_string() } else { format!("{}.", &DAY_NAMES[d.weekday().num_days_from_monday() as usize][..3]) };
    let day_long = |d: NaiveDate| format!("{} {}", DAY_NAMES[d.weekday().num_days_from_monday() as usize], d.format("%d/%m"));
    let (short, long): (Vec<String>, Vec<String>) = match period {
        0 => (0..24).map(|h| (if h % 6 == 0 { format!("{h} h") } else { String::new() }, format!("Aujourd'hui, {h} h – {} h", h + 1))).unzip(),
        1 => (0..7).rev().map(|i| today - ChronoDuration::days(i)).map(|d| (day_short(d), day_long(d))).unzip(),
        2 => (0..30)
            .rev()
            .map(|i| today - ChronoDuration::days(i))
            .map(|d| (if d == today { "auj.".into() } else if d.day() % 5 == 0 { d.format("%d/%m").to_string() } else { String::new() }, day_long(d)))
            .unzip(),
        _ => months(today).into_iter().map(|d| (MONTHS[d.month0() as usize].to_string(), format!("{} {}", MONTHS_LONG[d.month0() as usize], d.year()))).unzip(),
    };
    let series = devices
        .iter()
        .map(|d| {
            let values: Vec<f64> = app.hub.energy(|e| match period {
                0 => e.hourly(&d.id, today).iter().map(|wh| wh / 1000.0).collect(),
                1 | 2 => {
                    let n = if period == 1 { 7 } else { 30 };
                    (0..n).rev().map(|i| e.day_wh(&d.id, today - ChronoDuration::days(i)) / 1000.0).collect()
                }
                _ => months(today).iter().map(|m| e.month_wh(&d.id, *m) / 1000.0).collect(),
            });
            (d.name.clone(), values)
        })
        .collect();
    (short, long, series)
}

/// Premiers jours des douze derniers mois, du plus ancien au mois en cours.
fn months(today: NaiveDate) -> Vec<NaiveDate> {
    let (mut y, mut m) = (today.year(), today.month());
    let mut out = Vec::with_capacity(12);
    for _ in 0..12 {
        out.push(NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(today));
        if m == 1 {
            (y, m) = (y - 1, 12);
        } else {
            m -= 1;
        }
    }
    out.reverse();
    out
}

/// Bâtons arrondis, le dernier (l'intervalle en cours) en lagon ; survol : le détail en kWh et €.
fn bars(ui: &mut Ui, short: &[String], long: &[String], totals: &[f64], price: f64, height: f32) {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let n = totals.len().max(1);
    let max = totals.iter().cloned().fold(0.0, f64::max).max(1e-6);
    let plot = Rect::from_min_max(rect.min + vec2(0.0, 24.0), rect.max - vec2(0.0, 24.0));
    let slot = plot.width() / n as f32;
    let bar_w = (slot * 0.62).clamp(3.0, 54.0);
    let p = ui.painter();
    let hover = resp.hover_pos().filter(|pos| plot.x_range().contains(pos.x)).map(|pos| (((pos.x - plot.left()) / slot) as usize).min(n - 1));
    let rest = theme::mix(theme::raised(), theme::accent(), if theme::is_light() { 0.35 } else { 0.32 });
    for (i, v) in totals.iter().enumerate() {
        let cx = plot.left() + slot * (i as f32 + 0.5);
        let h = (*v / max) as f32 * plot.height();
        let r = Rect::from_min_max(pos2(cx - bar_w / 2.0, plot.bottom() - h.max(2.0)), pos2(cx + bar_w / 2.0, plot.bottom()));
        let last = i == n - 1;
        let color = if last || hover == Some(i) { theme::accent() } else { rest };
        p.rect_filled(r, CornerRadius::same((bar_w / 3.0).min(12.0) as u8), color);
        if let Some(s) = short.get(i).filter(|s| !s.is_empty()) {
            p.text(pos2(cx, rect.bottom() - 4.0), Align2::CENTER_BOTTOM, s, FontId::proportional(12.0), if last { theme::accent() } else { theme::faint() });
        }
        if last && *v > 0.0 {
            p.text(pos2(cx, r.top() - 6.0), Align2::CENTER_BOTTOM, format!("{} €", fr(v * price, 2)), theme::bold(12.5), theme::text());
        }
    }
    if let Some(i) = hover {
        let v = totals[i];
        resp.on_hover_text_at_pointer(format!("{} : {} kWh · {} €", long.get(i).map(String::as_str).unwrap_or(""), fr(v, 2), fr(v * price, 2)));
    }
}

impl App {
    pub(super) fn energy_view(&mut self, ui: &mut Ui, devices: &[DeviceView]) {
        let price = self.hub.prefs().price_kwh;
        let meters: Vec<&DeviceView> = devices.iter().filter(|d| kinds::power_w(&d.dps, &d.schema).is_some() || self.hub.energy(|e| e.has_data(&d.id))).collect();
        let mut period = self.energy_chart.period;
        widgets::page_header(ui, "Énergie", |ui| {
            if let Some(i) = widgets::segmented(ui, &PERIODS, Some(period), theme::accent()) {
                period = i;
            }
        });
        self.energy_chart.period = period;
        ui.add_space(14.0);
        ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            if meters.is_empty() {
                widgets::panel_frame().show(ui, |ui| {
                    ui.label(RichText::new("Aucun appareil ne mesure sa consommation").font(theme::bold(17.0)));
                    ui.label(RichText::new("Les prises qui mesurent l'énergie apparaissent ici d'elles-mêmes.").color(theme::muted()));
                });
                return;
            }
            let (short, long, series) = build(self, &meters, period);
            let totals: Vec<f64> = (0..short.len()).map(|i| series.iter().map(|(_, v)| v.get(i).copied().unwrap_or(0.0)).sum()).collect();
            let kwh: f64 = totals.iter().sum();
            let now_w: f64 = meters.iter().filter(|d| d.online).filter_map(|d| kinds::power_w(&d.dps, &d.schema)).sum();
            // Le grand chiffre : ce que la période a coûté.
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                ui.label(RichText::new(format!("{} €", fr(kwh * price, 2))).font(theme::light(56.0)));
                ui.label(RichText::new(format!("{} kWh {}", fr(kwh, 1), period_words(period))).size(18.0).color(theme::muted()));
                ui.add_space(18.0);
                ui.label(RichText::new(format!("En ce moment : {}", widgets::watts(now_w))).size(14.0).color(theme::muted()));
            });
            ui.add_space(16.0);
            // Graphique et classement côte à côte (l'un sous l'autre si la place manque).
            // Marge pour la barre de défilement, qui flotte par-dessus le bord droit.
            ui.set_max_width(ui.available_width() - 12.0);
            let wide = ui.available_width() >= 860.0;
            let chart_h = (ui.available_height() - 60.0).clamp(220.0, 380.0);
            let mut ranking: Vec<(String, f64)> = series.iter().map(|(n, v)| (n.clone(), v.iter().sum())).collect();
            ranking.sort_by(|a, b| b.1.total_cmp(&a.1));
            let chart_title = match period {
                0 => "Par heure",
                3 => "Par mois",
                _ => "Par jour",
            };
            let chart = |ui: &mut Ui| {
                widgets::panel_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(chart_title).font(theme::bold(15.0)));
                    bars(ui, &short, &long, &totals, price, chart_h);
                });
            };
            let list = |ui: &mut Ui| {
                widgets::panel_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new("Ce qui consomme").font(theme::bold(15.0)));
                    ui.add_space(6.0);
                    let top = ranking.first().map(|r| r.1).unwrap_or(0.0).max(1e-6);
                    for (name, v) in &ranking {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(name).size(14.5));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(RichText::new(format!("{} €", fr(v * price, 2))).size(13.5).color(theme::muted()));
                            });
                        });
                        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 8.0), Sense::hover());
                        ui.painter().rect_filled(r, CornerRadius::same(4), theme::raised());
                        let k = (*v / top) as f32;
                        if k > 0.0 {
                            ui.painter().rect_filled(Rect::from_min_size(r.min, vec2((r.width() * k).max(6.0), r.height())), CornerRadius::same(4), theme::accent());
                        }
                        ui.add_space(8.0);
                    }
                    ui.add_space(4.0);
                    ui.label(RichText::new(format!("Prix du kWh : {} € (Préférences)", fr(price, 4))).size(12.0).color(theme::faint()));
                });
            };
            if wide {
                let side = (ui.available_width() * 0.32).clamp(280.0, 380.0);
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width() - side - 16.0);
                        chart(ui);
                    });
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.set_width(side);
                        list(ui);
                    });
                });
            } else {
                chart(ui);
                ui.add_space(14.0);
                list(ui);
            }
            ui.add_space(10.0);
            ui.label(RichText::new("Calculé par HomeSmart à partir des mesures des prises, pendant que l'application est ouverte.").size(12.0).color(theme::faint()));
        });
    }

    /// Consommation d'un seul appareil (page « Consommation » de son panneau).
    pub(super) fn device_energy_chart(&mut self, ui: &mut Ui, d: &DeviceView) {
        let price = self.hub.prefs().price_kwh;
        if let Some(i) = widgets::segmented(ui, &PERIODS, Some(self.energy_dev_chart.period), theme::accent()) {
            self.energy_dev_chart.period = i;
        }
        let period = self.energy_dev_chart.period;
        let (short, long, series) = build(self, &[d], period);
        let totals: Vec<f64> = series.first().map(|(_, v)| v.clone()).unwrap_or_default();
        let kwh: f64 = totals.iter().sum();
        ui.add_space(6.0);
        ui.label(RichText::new(format!("{} kWh {} · {} €", fr(kwh, 2), period_words(period), fr(kwh * price, 2))).size(14.0).color(theme::muted()));
        bars(ui, &short, &long, &totals, price, 200.0);
    }
}
