//! Application de bureau (egui).

mod autostart;
mod cameras;
mod charts;
mod cloud_import;
mod energy_ui;
mod fx;
mod house;
mod icons;
mod panel;
mod png;
mod problems_ui;
pub mod theme;
mod three;
mod vacuum_ui;
mod views;
mod widgets;

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, CornerRadius, CursorIcon, Id, Label, Layout, Margin, Rect, RichText, ScrollArea, Sense,
    Stroke, StrokeKind, Ui,
};
use homesmart::config::{self, DeviceConfig};
use homesmart::hub::{DevicePatch, DeviceView, DiscoveredView, Hub};
use homesmart::kinds::{self, Hsv, Kind};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use icons::Icon;
use widgets::fr;

/// Durée pendant laquelle une valeur envoyée s'affiche avant confirmation de l'appareil.
const OVERLAY_TTL: Duration = Duration::from_millis(2500);
/// Intervalle minimal entre deux envois d'un même DP (glissement d'un curseur).
const SEND_INTERVAL: Duration = Duration::from_millis(110);
const TOAST_TTL: Duration = Duration::from_secs(5);

type DpKey = (String, String);

/// Vue principale affichée.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum View {
    Devices,
    Cameras,
    House,
    Energy,
    Calendar,
    Alerts,
    Problems,
}

struct Toast {
    text: String,
    error: bool,
    at: Instant,
}

#[derive(Default)]
struct AddForm {
    id: String,
    key: String,
    name: String,
    room: String,
    ip: String,
    version: String,
    show_key: bool,
    error: Option<String>,
}

struct SettingsForm {
    original: DeviceConfig,
    name: String,
    room: String,
    ip: String,
    version: String,
    key: String,
    kind: String,
    show_key: bool,
    confirm_delete: bool,
}

/// Capture automatique de l'interface (variables HOMESMART_SCREENSHOT / HOMESMART_SELECT).
struct AutoShot {
    path: PathBuf,
    at: Instant,
    select: Option<String>,
    requested: bool,
}

/// Mesure du rythme de l'interface (variable HOMESMART_PERF = fichier écrit à la fermeture) :
/// nombre d'images dessinées et temps passé dans `ui()` pour chacune.
struct Perf {
    path: PathBuf,
    since: Instant,
    /// Temps de `ui()` par image (ms) et temps processeur complet de l'image précédente (ms).
    ui_ms: Vec<f32>,
    cpu_ms: Vec<f32>,
    /// Origine des demandes de redessin (fichier:ligne), et leur nombre.
    causes: HashMap<String, u32>,
    /// Numéro d'image egui au début et à la fin (un passage refait dans la même image ne compte pas).
    frames: (u64, u64),
}

impl Perf {
    fn from_env() -> Option<Self> {
        std::env::var_os("HOMESMART_PERF").map(|p| Perf { path: PathBuf::from(p), since: Instant::now(), ui_ms: Vec::new(), cpu_ms: Vec::new(), causes: HashMap::new(), frames: (u64::MAX, 0) })
    }

    fn write(&self) {
        fn stats(v: &[f32]) -> String {
            if v.is_empty() {
                return "aucune".into();
            }
            let mut s = v.to_vec();
            s.sort_by(f32::total_cmp);
            let at = |q: f32| s[((s.len() - 1) as f32 * q) as usize];
            let mean = s.iter().sum::<f32>() / s.len() as f32;
            format!("moyenne {mean:.2} ms, médiane {:.2}, p95 {:.2}, max {:.2}", at(0.5), at(0.95), at(1.0))
        }
        let secs = self.since.elapsed().as_secs_f32();
        let mut causes: Vec<(&String, &u32)> = self.causes.iter().collect();
        causes.sort_by(|a, b| b.1.cmp(a.1));
        let causes: String = causes.iter().take(8).map(|(c, n)| format!("  {n} × {c}
")).collect();
        let text = format!(
            "passages {}
images {}
durée {secs:.1} s
images/s {:.1}
ui {}
cpu {}
redessins demandés par :
{causes}",
            self.ui_ms.len(),
            self.frames.1.saturating_sub(self.frames.0),
            self.ui_ms.len() as f32 / secs.max(0.001),
            stats(&self.ui_ms),
            stats(&self.cpu_ms)
        );
        if let Err(e) = std::fs::write(&self.path, text) {
            eprintln!("mesure impossible à écrire : {e}");
        }
    }
}

pub struct App {
    rt: tokio::runtime::Handle,
    hub: Arc<Hub>,
    ctx: egui::Context,
    demo: bool,
    config_path: PathBuf,
    selected: Option<String>,
    overlay: HashMap<DpKey, (Value, Instant)>,
    pending: HashMap<DpKey, Value>,
    last_sent: HashMap<DpKey, Instant>,
    text_buf: HashMap<DpKey, String>,
    toasts: Arc<Mutex<Vec<Toast>>>,
    add: Option<AddForm>,
    cloud: Option<cloud_import::CloudImport>,
    settings: Option<SettingsForm>,
    scan_until: Option<Instant>,
    auto: Option<AutoShot>,
    view: View,
    tab: panel::PanelTab,
    sched_form: Option<panel::SchedForm>,
    safety_form: Option<panel::SafetyForm>,
    prefs_open: bool,
    autostart: bool,
    /// Allumage en attente de confirmation : (id, nom, DP).
    confirm: Option<(String, String, Map<String, Value>)>,
    close_dialog: bool,
    allow_close: bool,
    alerts_seen: u64,
    /// Dernière alerte déjà montrée en notification.
    toasted: u64,
    countdown_custom: (u32, u32),
    start_minimized: bool,
    journal: views::JournalFilter,
    /// Vue affichée et début de sa transition d'apparition.
    shown_view: View,
    view_since: Instant,
    house: house::HouseState,
    /// La fenêtre a-t-elle le focus (rythme des redessins).
    focused: Arc<std::sync::atomic::AtomicBool>,
    energy_chart: energy_ui::EnergyChart,
    energy_dev_chart: energy_ui::EnergyChart,
    cams: cameras::CamUi,
    /// Photo de visage déposée sur la fenêtre : (fichier, nom saisi, erreur).
    photo_face: Option<(PathBuf, String, Option<String>)>,
    perf: Option<Perf>,
    /// Minuterie : saisie d'une durée libre ouverte.
    timer_custom: bool,
    /// Problèmes en cours (relus à chaque image : tuiles, bandeau, page Problèmes).
    problems_now: Vec<homesmart::hub::Problem>,
    /// Aspirateurs robots : fenêtre d'ajout, réglages, commande en cours d'envoi.
    vac: vacuum_ui::VacUi,
}

/// Une tuile de la page Maison.
enum Tile<'a> {
    Dev(&'a DeviceView),
    Vac(&'a homesmart::hub::VacuumView),
    Cam(&'a homesmart::hub::CameraView),
}

/// Tuiles de la page Maison : largeur minimale et hauteur.
const TILE_MIN_W: f32 = 150.0;
const TILE_H: f32 = 136.0;

/// Ligne de la navigation latérale (icône et nom, ou icône seule quand elle est repliée) ; la
/// pastille (nombre, couleur) signale des alertes ou des problèmes.
fn nav_item(ui: &mut Ui, icon: Icon, label: &str, active: bool, badge: Option<(usize, Color32)>, rail: bool) -> egui::Response {
    let width = if rail { 46.0 } else { ui.available_width() };
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 42.0), Sense::click());
    let h = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let p = ui.painter();
    if active {
        p.rect_filled(rect, CornerRadius::same(12), theme::raised());
    } else if h > 0.0 {
        p.rect_filled(rect, CornerRadius::same(12), theme::alpha(theme::raised(), 0.6 * h));
    }
    let ic = if rail { Rect::from_center_size(rect.center(), vec2(20.0, 20.0)) } else { Rect::from_center_size(pos2(rect.left() + 22.0, rect.center().y), vec2(20.0, 20.0)) };
    icons::paint(p, ic, icon, if active { theme::accent() } else { theme::mix(theme::muted(), theme::text(), h) });
    if !rail {
        let font = if active { theme::bold(14.5) } else { egui::FontId::proportional(14.5) };
        p.text(pos2(rect.left() + 44.0, rect.center().y), Align2::LEFT_CENTER, label, font, if active { theme::text() } else { theme::muted() });
    }
    if let Some((n, fill)) = badge {
        let text = if n > 99 { "99+".to_string() } else { n.to_string() };
        let ink = theme::ink_on(fill);
        let g = p.layout_no_wrap(text, theme::bold(10.5), ink);
        let size = vec2((g.size().x + 10.0).max(18.0), 18.0);
        let c = if rail { pos2(rect.right() - 7.0, rect.top() + 9.0) } else { pos2(rect.right() - 12.0 - size.x / 2.0, rect.center().y) };
        let r = Rect::from_center_size(c, size);
        p.rect_filled(r, CornerRadius::same(9), fill);
        p.galley(r.center() - g.size() / 2.0, g, ink);
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    if rail {
        resp.on_hover_text(label)
    } else {
        resp
    }
}

/// Texte sur une ligne, coupé avec « … » s'il dépasse `width`.
fn one_line(p: &egui::Painter, text: &str, font: egui::FontId, color: Color32, width: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = egui::text::TextWrapping { max_width: width.max(10.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    p.layout_job(job)
}

const VERSIONS: [&str; 6] = ["", "3.1", "3.2", "3.3", "3.4", "3.5"];
const KINDS: [(&str, &str); 7] =
    [("", "Automatique"), ("light", "Lumière"), ("plug", "Prise"), ("switch", "Multiprise"), ("cover", "Volet"), ("vacuum", "Aspirateur robot"), ("generic", "Générique")];

fn plural(n: usize, word: &str) -> String {
    if n > 1 {
        format!("{n} {word}s")
    } else {
        format!("{n} {word}")
    }
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        rt: tokio::runtime::Handle,
        hub: Arc<Hub>,
        demo: bool,
        config_path: PathBuf,
        notice: Option<String>,
        start_minimized: bool,
    ) -> Self {
        let light = match std::env::var("HOMESMART_THEME").ok().as_deref() {
            Some("clair") => true,
            Some("sombre") => false,
            _ => hub.prefs().light_theme,
        };
        theme::set_light(&cc.egui_ctx, light);
        theme::apply(&cc.egui_ctx);
        let ctx = cc.egui_ctx.clone();

        // Chaque changement d'état redessine la fenêtre : dans les 100 ms si elle a le focus,
        // sinon regroupé toutes les 3 s (les alertes restent immédiates).
        let mut rx = hub.events.subscribe();
        let repaint = ctx.clone();
        let focused = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let focus = focused.clone();
        let notify_hub = hub.clone();
        rt.spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            loop {
                match rx.recv().await {
                    Ok(homesmart::hub::Event::Alert(id)) => {
                        repaint.request_repaint();
                        // Alerte critique : notification du système, même fenêtre réduite ou cachée.
                        if notify_hub.prefs().os_notifications {
                            let entry = notify_hub.alerts_after(id.saturating_sub(1)).into_iter().find(|e| e.id == id);
                            if let Some(e) = entry.filter(|e| e.level == homesmart::hub::Level::Critical) {
                                os_notify(&e);
                            }
                        }
                    }
                    Ok(_) | Err(RecvError::Lagged(_)) => {
                        // Regroupés : au plus un redessin par 100 ms, même quand le hub en annonce
                        // des dizaines par seconde (analyse IA, téléchargement d'un modèle).
                        if focus.load(std::sync::atomic::Ordering::Relaxed) {
                            repaint.request_repaint_after(Duration::from_millis(100));
                        } else {
                            repaint.request_repaint_after(Duration::from_secs(3));
                        }
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });

        // Chaque nouvelle image d'une caméra affichée redessine la fenêtre, une seule fois : egui
        // dessine deux images pour un `request_repaint()` immédiat, une seule avec un délai.
        let frames = ctx.clone();
        hub.set_frame_waker(Arc::new(move || frames.request_repaint_after(Duration::from_millis(1))));

        let auto = std::env::var_os("HOMESMART_SCREENSHOT").map(|p| AutoShot {
            path: PathBuf::from(p),
            at: Instant::now()
                + Duration::from_millis(std::env::var("HOMESMART_SHOT_DELAY").ok().and_then(|s| s.parse().ok()).unwrap_or(1800)),
            select: std::env::var("HOMESMART_SELECT").ok().filter(|s| !s.is_empty()),
            requested: false,
        });

        let app = App {
            rt,
            hub,
            ctx,
            demo,
            config_path,
            selected: None,
            overlay: HashMap::new(),
            pending: HashMap::new(),
            last_sent: HashMap::new(),
            text_buf: HashMap::new(),
            toasts: Arc::default(),
            add: None,
            cloud: None,
            settings: None,
            scan_until: None,
            auto,
            view: View::Devices,
            tab: panel::PanelTab::Control,
            sched_form: None,
            safety_form: None,
            prefs_open: false,
            autostart: autostart::is_enabled(),
            confirm: None,
            close_dialog: false,
            allow_close: false,
            alerts_seen: 0,
            toasted: 0,
            countdown_custom: (1, 0),
            start_minimized,
            journal: views::JournalFilter::default(),
            shown_view: View::Devices,
            view_since: Instant::now(),
            focused,
            house: house::HouseState::default(),
            energy_chart: energy_ui::EnergyChart::new(1),
            energy_dev_chart: energy_ui::EnergyChart::new(1),
            cams: cameras::CamUi::new(),
            photo_face: None,
            perf: Perf::from_env(),
            timer_custom: false,
            problems_now: Vec::new(),
            vac: vacuum_ui::VacUi::default(),
        };
        let mut app = app;
        app.alerts_seen = app.hub.alert_stats(0).0;
        app.toasted = app.alerts_seen;
        if let Some(n) = notice {
            app.toast(n, true);
        }
        app
    }

    fn toast(&self, text: impl Into<String>, error: bool) {
        self.toasts.lock().unwrap().push(Toast { text: text.into(), error, at: Instant::now() });
        self.ctx.request_repaint();
    }

    fn select(&mut self, id: &str) {
        if self.selected.as_deref() != Some(id) {
            self.selected = Some(id.to_string());
            self.settings = None;
            self.safety_form = None;
            self.sched_form = None;
            self.tab = panel::PanelTab::Control;
        }
    }

    // ---- Valeurs et envois -------------------------------------------------------

    fn value(&self, d: &DeviceView, dp: &str) -> Option<Value> {
        match self.overlay.get(&(d.id.clone(), dp.to_string())) {
            Some((v, _)) => Some(v.clone()),
            None => d.dps.get(dp).cloned(),
        }
    }

    fn num(&self, d: &DeviceView, dp: &str) -> Option<f64> {
        self.value(d, dp).and_then(|v| v.as_f64())
    }

    fn flag(&self, d: &DeviceView, dp: &str) -> Option<bool> {
        self.value(d, dp).and_then(|v| v.as_bool())
    }

    fn text(&self, d: &DeviceView, dp: &str) -> Option<String> {
        self.value(d, dp).and_then(|v| v.as_str().map(str::to_string))
    }

    /// Mémorise la valeur (affichée immédiatement) et la met en file d'envoi.
    fn set(&mut self, id: &str, dp: &str, v: Value) {
        let key = (id.to_string(), dp.to_string());
        self.overlay.insert(key.clone(), (v.clone(), Instant::now()));
        self.pending.insert(key, v);
    }

    /// Envoie ce qui est prêt ; les DP d'un même appareil partent dans une seule commande.
    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let now = Instant::now();
        let ready: HashSet<String> = self
            .pending
            .keys()
            .filter(|k| self.last_sent.get(*k).is_none_or(|t| now.duration_since(*t) >= SEND_INTERVAL))
            .map(|k| k.0.clone())
            .collect();
        let mut by_dev: HashMap<String, Map<String, Value>> = HashMap::new();
        self.pending.retain(|(id, dp), v| {
            if ready.contains(id) {
                by_dev.entry(id.clone()).or_default().insert(dp.clone(), v.clone());
                false
            } else {
                true
            }
        });
        for (id, dps) in by_dev {
            for k in dps.keys() {
                self.last_sent.insert((id.clone(), k.clone()), now);
            }
            let (hub, toasts, ctx) = (self.hub.clone(), self.toasts.clone(), self.ctx.clone());
            let name = hub.view(&id).map(|v| v.name).unwrap_or_else(|| id.clone());
            self.rt.spawn(async move {
                if let Err(e) = hub.set_dps(&id, dps, homesmart::hub::Source::User).await {
                    toasts.lock().unwrap().push(Toast { text: format!("{name} : {e:#}"), error: true, at: Instant::now() });
                    ctx.request_repaint();
                }
            });
        }
    }

    fn housekeeping(&mut self, devices: &[DeviceView]) {
        let now = Instant::now();
        let pending = &self.pending;
        self.overlay.retain(|key, (v, at)| {
            let age = now.duration_since(*at);
            if age > OVERLAY_TTL {
                return false;
            }
            if pending.contains_key(key) {
                return true;
            }
            let actual = devices.iter().find(|d| d.id == key.0).and_then(|d| d.dps.get(&key.1));
            actual != Some(v)
        });
        self.toasts.lock().unwrap().retain(|t| t.at.elapsed() < TOAST_TTL);
        if self.scan_until.is_some_and(|t| now >= t) {
            self.scan_until = None;
        }
    }

    fn is_on(&self, d: &DeviceView, kind: &Kind) -> bool {
        match kind {
            Kind::Switch { gangs } => gangs.iter().any(|g| self.flag(d, g).unwrap_or(false)),
            Kind::Cover => self.num(d, "3").or_else(|| self.num(d, "2")).unwrap_or(0.0) > 0.0,
            _ => kind.switch_dp(&d.dps).and_then(|dp| self.flag(d, &dp)).unwrap_or(false),
        }
    }

    fn schema_code<'a>(d: &'a DeviceView, dp: &str) -> Option<&'a str> {
        d.schema.get(dp).and_then(|s| s.get("code")).and_then(Value::as_str)
    }

    fn summary(&self, d: &DeviceView, kind: &Kind) -> (String, Option<Color32>) {
        if !d.online {
            return (d.error.clone().unwrap_or_else(|| "Connexion…".into()), None);
        }
        let on = self.is_on(d, kind);
        match kind {
            Kind::LightV2 | Kind::LightV1 => {
                if !on {
                    return ("Éteinte".into(), None);
                }
                let v2 = *kind == Kind::LightV2;
                let (mode_dp, b_dp, c_dp, lo, hi) = if v2 { ("21", "22", "24", 10.0, 1000.0) } else { ("2", "3", "5", 25.0, 255.0) };
                if self.text(d, mode_dp).as_deref() == Some("colour") {
                    let hsv = self.text(d, c_dp).and_then(|s| if v2 { kinds::parse_colour_v2(&s) } else { kinds::parse_colour_v1(&s) });
                    let swatch = hsv.map(|c| {
                        let (r, g, b) = kinds::hsv_to_rgb(Hsv { v: 1.0, ..c });
                        Color32::from_rgb(r, g, b)
                    });
                    return ("Couleur".into(), swatch);
                }
                let raw = self.num(d, b_dp).unwrap_or(hi);
                let pct = ((raw - lo) / (hi - lo) * 100.0).round().clamp(1.0, 100.0);
                (format!("Luminosité {pct:.0} %"), None)
            }
            Kind::Plug { metering } => match (on, metering, kinds::metering(&d.dps)) {
                (_, true, Some((w, _, _))) if on || w > 0.0 => (format!("{} W", fr(w, 1)), None),
                (true, _, _) => ("Allumée".into(), None),
                _ => ("Éteinte".into(), None),
            },
            Kind::Switch { gangs } => {
                let n = gangs.iter().filter(|g| self.flag(d, g).unwrap_or(false)).count();
                (format!("{n} / {} sorties allumées", gangs.len()), None)
            }
            Kind::Cover => {
                let p = self.num(d, "3").or_else(|| self.num(d, "2")).unwrap_or(0.0);
                (if p <= 0.0 { "Fermé".into() } else { format!("Ouvert à {p:.0} %") }, None)
            }
            Kind::Vacuum => (homesmart::vacuum::tuya::status(&d.dps, &d.schema).summary(), None),
            Kind::Generic => {
                let mut keys: Vec<&String> = d.dps.keys().collect();
                keys.sort_by_key(|k| k.parse::<u32>().unwrap_or(u32::MAX));
                let parts: Vec<String> = keys
                    .into_iter()
                    .filter_map(|k| {
                        let v = self.value(d, k)?;
                        if v.is_boolean() || v.is_null() {
                            return None;
                        }
                        let label = Self::schema_code(d, k).map(kinds::label_for_code).unwrap_or_else(|| format!("DP {k}"));
                        Some(format!("{label} {}", self.format_value(d, k, &v)))
                    })
                    .take(2)
                    .collect();
                (if parts.is_empty() { if on { "Allumé".into() } else { "Éteint".into() } } else { parts.join(" · ") }, None)
            }
        }
    }

    /// Valeur lisible selon le schéma (échelle, unité).
    fn format_value(&self, d: &DeviceView, dp: &str, v: &Value) -> String {
        let values = d.schema.get(dp).and_then(|s| s.get("values"));
        match v {
            Value::Number(n) => {
                let scale = values.and_then(|x| x.get("scale")).and_then(Value::as_i64).unwrap_or(0).clamp(0, 6) as i32;
                let unit = values.and_then(|x| x.get("unit")).and_then(Value::as_str).unwrap_or("");
                let x = n.as_f64().unwrap_or(0.0) / 10f64.powi(scale);
                let s = fr(x, scale as usize);
                if unit.is_empty() {
                    s
                } else if unit.starts_with('°') || unit == "%" {
                    format!("{s} {unit}")
                } else {
                    format!("{s} {unit}")
                }
            }
            Value::String(s) => s.clone(),
            Value::Bool(b) => (if *b { "oui" } else { "non" }).into(),
            other => other.to_string(),
        }
    }

    // ---- Disposition générale ------------------------------------------------------

    /// Navigation latérale : rubriques (icône et nom, ou icône seule si la fenêtre est étroite),
    /// Préférences en bas.
    fn sidebar(&mut self, ui: &mut Ui, rail: bool) {
        let (_, unread) = self.hub.alert_stats(self.alerts_seen);
        ui.horizontal(|ui| {
            if !rail {
                ui.add_space(4.0);
            }
            let (r, _) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::hover());
            ui.painter().rect_filled(r, CornerRadius::same(10), theme::accent());
            icons::paint(ui.painter(), r.shrink(7.0), Icon::House, theme::on_accent());
            if !rail {
                ui.label(RichText::new("HomeSmart").font(theme::bold(16.0)));
                if self.demo {
                    widgets::pill(ui, "DÉMO", theme::bg(), theme::violet());
                }
            }
        });
        ui.add_space(18.0);
        ui.spacing_mut().item_spacing.y = 4.0;
        let items = [
            (View::Devices, Icon::House, "Maison"),
            (View::Cameras, Icon::Camera, "Caméras"),
            (View::Energy, Icon::Bolt, "Énergie"),
            (View::Problems, Icon::Alert, "Problèmes"),
            (View::Alerts, Icon::Bell, "Activité"),
            (View::Calendar, Icon::Clock, "Programmes"),
        ];
        // Problèmes en cours (sans les simples informations), couleur du plus grave.
        let serious: Vec<&homesmart::hub::Problem> = self.problems_now.iter().filter(|p| p.level != homesmart::hub::Level::Info).collect();
        let worst = serious.iter().map(|p| problems_ui::rank(p.level)).max();
        for (view, icon, label) in items {
            let active = self.view == view || (view == View::Devices && self.view == View::House);
            let badge = match view {
                View::Alerts if unread > 0 => Some((unread, theme::crit())),
                View::Problems if !serious.is_empty() => Some((serious.len(), if worst == Some(2) { theme::crit() } else { theme::warn() })),
                _ => None,
            };
            if nav_item(ui, icon, label, active, badge, rail).clicked() {
                if self.view != view {
                    self.selected = None;
                }
                // Un clic sur la rubrique déjà affichée revient à sa page d'accueil.
                if self.view == View::Cameras && view == View::Cameras {
                    if let Some(id) = self.cams.selected.take() {
                        if self.cams.rec_mode {
                            let _ = self.hub.camera_stop_playback(&id, homesmart::hub::Source::User);
                        }
                        self.cams.zones.cancel(&id);
                        (self.cams.rec_mode, self.cams.wide) = (false, false);
                    }
                }
                self.view = view;
            }
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            if nav_item(ui, Icon::Gear, "Préférences", self.prefs_open, None, rail).clicked() {
                self.prefs_open = true;
            }
        });
    }

    /// Mode absence armé : bandeau framboise en haut de tous les écrans.
    fn armed_banner(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
            icons::paint(ui.painter(), r, Icon::Shield, Color32::WHITE);
            ui.label(RichText::new("Mode absence activé : toute personne vue déclenche une alerte").strong().color(Color32::WHITE));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let b = egui::Button::new(RichText::new("Désactiver").strong().color(theme::crit())).fill(Color32::WHITE).corner_radius(CornerRadius::same(10));
                if ui.add(b).clicked() {
                    self.hub.set_vision_armed(false, homesmart::hub::Source::User);
                }
            });
        });
    }

    /// Page « Maison » : une phrase d'état, puis chaque pièce et ses tuiles.
    fn dashboard(&mut self, ui: &mut Ui, devices: &[DeviceView]) {
        let cams = self.hub.cameras();
        let vacs = self.hub.vacuums();
        widgets::page_header(ui, "Maison", |ui| {
            if widgets::round_button(ui, Icon::Plus, true).on_hover_text("Ajouter un appareil").clicked() {
                self.open_add();
            }
            ui.add_space(2.0);
            if widgets::round_button(ui, Icon::Cube, false).on_hover_text("Plan 3D de la maison").clicked() {
                self.view = View::House;
            }
        });
        if !devices.is_empty() || !vacs.is_empty() {
            let on = devices.iter().filter(|d| d.online && self.is_on(d, &Self::kind_of_view(d))).count();
            let offline = devices.iter().filter(|d| !d.online).count() + vacs.iter().filter(|v| !v.online && v.family != homesmart::vacuum::Family::Tuya).count();
            let power: f64 = devices.iter().filter(|d| d.online).filter_map(|d| kinds::power_w(&d.dps, &d.schema)).sum();
            let metering = devices.iter().any(|d| kinds::power_w(&d.dps, &d.schema).is_some());
            let strong = match on {
                0 => "Tout est éteint".to_string(),
                1 => "1 appareil allumé".to_string(),
                n => format!("{n} appareils allumés"),
            };
            let mut rest = String::new();
            if metering {
                rest += &format!(" · {} en ce moment", widgets::watts(power));
            }
            let cleaning: Vec<&str> = vacs.iter().filter(|v| v.online && v.status.activity == homesmart::vacuum::Activity::Cleaning).map(|v| v.name.as_str()).collect();
            match cleaning.as_slice() {
                [] => {}
                [one] => rest += &format!(" · « {one} » nettoie"),
                many => rest += &format!(" · {} aspirateurs nettoient", many.len()),
            }
            if offline > 0 {
                rest += &format!(" · {} hors ligne", plural(offline, "appareil"));
            }
            widgets::lede(ui, &strong, &rest);
        }
        self.problems_banner(ui);
        ui.add_space(18.0);
        ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            if devices.is_empty() && cams.is_empty() && vacs.is_empty() {
                self.empty_state(ui);
                return;
            }
            // Les caméras et les aspirateurs sont rangés par pièce avec les autres appareils (les
            // robots Tuya sont des appareils, montrés avec les autres aspirateurs).
            let mut rooms: Vec<(String, Vec<Tile>)> = Vec::new();
            let mut put = |room: &Option<String>, t| {
                let room = room.clone().unwrap_or_else(|| "Sans pièce".into());
                match rooms.iter_mut().find(|(r, _)| *r == room) {
                    Some((_, list)) => list.push(t),
                    None => rooms.push((room, vec![t])),
                }
            };
            for d in devices.iter().filter(|d| Self::kind_of_view(d) != Kind::Vacuum) {
                put(&d.room, Tile::Dev(d));
            }
            for v in &vacs {
                put(&v.room, Tile::Vac(v));
            }
            for c in &cams {
                put(&c.room, Tile::Cam(c));
            }
            // Les caméras après les appareils de leur pièce.
            for (_, list) in rooms.iter_mut() {
                list.sort_by_key(|t| matches!(t, Tile::Cam(_)));
            }
            // Autant de colonnes que la largeur le permet (deux au moins) ; une caméra en prend deux.
            let gap = 16.0;
            let avail = ui.available_width() - 2.0;
            let cols = (((avail + gap) / (TILE_MIN_W + gap)).floor() as usize).max(2);
            let w = (avail - gap * (cols as f32 - 1.0)) / cols as f32;
            for (room, items) in rooms {
                ui.label(RichText::new(&room).font(theme::bold(15.0)).color(theme::muted()));
                ui.add_space(4.0);
                let mut rows: Vec<Vec<(usize, Tile)>> = vec![Vec::new()];
                let mut used = 0;
                for it in items {
                    let span = if matches!(it, Tile::Cam(_)) { 2.min(cols) } else { 1 };
                    if used + span > cols {
                        rows.push(Vec::new());
                        used = 0;
                    }
                    used += span;
                    rows.last_mut().unwrap().push((span, it));
                }
                for row in rows.into_iter().filter(|r| !r.is_empty()) {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        for (span, it) in row {
                            match it {
                                Tile::Dev(d) => self.tile(ui, d, w),
                                Tile::Vac(v) => self.vacuum_tile(ui, v, w),
                                Tile::Cam(c) => self.camera_card(ui, c, vec2(w * span as f32 + gap * (span as f32 - 1.0), TILE_H)),
                            }
                        }
                    });
                    ui.add_space(gap);
                }
                ui.add_space(10.0);
            }
        });
    }

    fn empty_state(&mut self, ui: &mut Ui) {
        widgets::panel_frame().inner_margin(Margin::same(28)).show(ui, |ui| {
            widgets::cap_width(ui, 620.0);
            ui.label(RichText::new("Aucun appareil pour l'instant").font(theme::bold(20.0)));
            ui.add_space(6.0);
            ui.label(
                RichText::new("HomeSmart pilote tes appareils Tuya / Smart Life directement en Wi-Fi, sans compte ni cloud. Le plus simple : importer d'un coup tous tes appareils et leurs clés depuis Smart Life.")
                    .color(theme::muted()),
            );
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if ui.add(widgets::primary_button("Importer depuis Smart Life", theme::accent()).min_size(vec2(0.0, 38.0))).clicked() {
                    self.cloud = Some(cloud_import::CloudImport::new());
                }
                if ui.add(egui::Button::new("Ajouter à la main").min_size(vec2(0.0, 38.0))).clicked() {
                    self.open_add();
                }
            });
            ui.add_space(8.0);
            ui.label(RichText::new("Tu peux aussi glisser ici le devices.json de tinytuya.").size(12.5).color(theme::faint()));
        });
    }

    /// Ouvre l'ajout d'un appareil et demande aux appareils du Wi-Fi de s'annoncer.
    fn open_add(&mut self) {
        self.add = Some(AddForm::default());
        if self.scan_until.is_none() {
            self.hub.trigger_scan();
            self.scan_until = Some(Instant::now() + Duration::from_secs(4));
        }
    }

    /// État court d'une tuile : « 72 % », « 1,4 kW », « Éteinte », « Hors ligne »…
    fn tile_state(&self, d: &DeviceView, kind: &Kind, on: bool) -> String {
        if !d.online {
            return "Hors ligne".into();
        }
        if !d.faults.is_empty() {
            return d.faults.join(", ");
        }
        match kind {
            Kind::LightV2 | Kind::LightV1 if on => {
                let s = self.summary(d, kind).0;
                s.strip_prefix("Luminosité ").map(str::to_string).unwrap_or(s)
            }
            Kind::LightV2 | Kind::LightV1 => "Éteinte".into(),
            Kind::Plug { .. } if on => kinds::power_w(&d.dps, &d.schema).map(widgets::watts).unwrap_or_else(|| "Allumée".into()),
            Kind::Plug { .. } => "Éteinte".into(),
            Kind::Switch { gangs } if on => {
                let n = gangs.iter().filter(|g| self.flag(d, g).unwrap_or(false)).count();
                format!("{n} sur {}", gangs.len())
            }
            Kind::Switch { .. } => "Éteinte".into(),
            Kind::Cover | Kind::Vacuum | Kind::Generic => self.summary(d, kind).0,
        }
    }

    /// Allume ou éteint un appareil d'un geste ; `false` s'il n'a pas d'interrupteur (volet…).
    fn toggle_device(&mut self, d: &DeviceView, kind: &Kind) -> bool {
        match kind {
            Kind::Cover => false,
            Kind::Switch { gangs } => {
                let on = gangs.iter().any(|g| self.flag(d, g).unwrap_or(false));
                self.request_power(d, gangs.iter().map(|g| (g.clone(), !on)).collect());
                true
            }
            _ => match kind.switch_dp(&d.dps) {
                Some(dp) => {
                    let on = self.flag(d, &dp).unwrap_or(false);
                    self.request_power(d, vec![(dp, !on)]);
                    true
                }
                None => false,
            },
        }
    }

    /// Tuile d'un appareil : un clic allume ou éteint, un clic droit (ou « ⋯ ») ouvre le détail.
    /// Allumée, elle est claire ; éteinte, sombre ; hors ligne, seulement esquissée.
    fn tile(&mut self, ui: &mut Ui, d: &DeviceView, w: f32) {
        let kind = Self::kind_of_view(d);
        let on = d.online && self.is_on(d, &kind);
        let (rect, resp) = ui.allocate_exact_size(vec2(w, TILE_H), Sense::click());
        let id = Id::unique(("tile", &d.id));
        let t = ui.ctx().animate_bool_with_time(id.with("on"), on, 0.2);
        let h = ui.ctx().animate_bool_with_time(id.with("h"), resp.hovered(), 0.12);
        let selected = self.selected.as_deref() == Some(d.id.as_str());
        let light = theme::is_light();
        let p = ui.painter();
        if d.online {
            if t > 0.01 {
                fx::shadow(p, rect, 22, -0.2 + 0.4 * h);
            }
            let off = theme::mix(theme::tile_off(), theme::raised(), 0.7 * h);
            p.rect_filled(rect, CornerRadius::same(22), theme::mix(off, theme::tile_on(), t));
        } else {
            p.rect_stroke(rect, CornerRadius::same(22), Stroke::new(1.5, theme::line()), StrokeKind::Inside);
        }
        // Problème en cours (le plus grave, sans les simples informations) : bord de sa couleur.
        let problem = self.problems_now.iter().filter(|p| p.device == d.id && p.level != homesmart::hub::Level::Info).max_by_key(|p| problems_ui::rank(p.level));
        if let Some(pr) = problem {
            p.rect_stroke(rect, CornerRadius::same(22), Stroke::new(2.0, problems_ui::level_color(pr.level)), StrokeKind::Inside);
        } else if !d.faults.is_empty() {
            p.rect_stroke(rect, CornerRadius::same(22), Stroke::new(2.0, theme::warn()), StrokeKind::Inside);
        }
        if selected {
            p.rect_stroke(rect.expand(3.0), CornerRadius::same(25), Stroke::new(2.5, theme::accent()), StrokeKind::Outside);
        }
        // Pastille de l'icône.
        let disc = Rect::from_min_size(rect.min + vec2(16.0, 16.0), vec2(42.0, 42.0));
        let disc_off = if light { Color32::from_rgb(0xdf, 0xe3, 0xe8) } else { theme::raised() };
        let faded = if d.online { 1.0 } else { 0.5 };
        p.circle_filled(disc.center(), 21.0, theme::alpha(theme::mix(disc_off, theme::sun(), t), faded));
        icons::paint(p, disc.shrink(10.5), Icon::from(&kind), theme::alpha(theme::mix(theme::muted(), theme::ink_on(theme::sun()), t), faded));
        // Nom et état.
        let ink = theme::mix(theme::text(), theme::tile_ink(), t);
        let ink2 = theme::mix(theme::muted(), theme::tile_ink_2(), t);
        let mut state = self.tile_state(d, &kind, on);
        let mut state_color = if !d.online {
            theme::off()
        } else if !d.faults.is_empty() {
            theme::warn()
        } else {
            ink2
        };
        if let Some(pr) = problem.filter(|_| d.online && d.faults.is_empty()) {
            (state, state_color) = (pr.kind.title().to_string(), problems_ui::level_color(pr.level));
        }
        let name = one_line(p, &d.name, theme::bold(15.0), theme::alpha(ink, faded), w - 32.0);
        let st = one_line(p, &state, egui::FontId::proportional(13.0), state_color, w - 32.0);
        p.galley(pos2(rect.left() + 16.0, rect.bottom() - 16.0 - st.size().y - 2.0 - name.size().y), name, ink);
        p.galley(pos2(rect.left() + 16.0, rect.bottom() - 16.0 - st.size().y), st, state_color);
        // « ⋯ » au survol : ouvre le détail.
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
            self.select(&d.id);
        } else if resp.clicked() && !(d.online && self.toggle_device(d, &kind)) {
            self.select(&d.id);
        }
        resp.on_hover_text(if d.online { "Clic : allumer ou éteindre · clic droit ou « ⋯ » : détails" } else { "Clic : détails" });
    }

    // ---- Panneau d'un appareil -----------------------------------------------------------

    /// Panneau d'un appareil, posé par-dessus l'écran : un clic à côté, la croix ou Échap le ferme
    /// (Échap revient d'abord à la page principale du panneau).
    fn device_sheet(&mut self, ctx: &egui::Context, devices: &[DeviceView]) {
        let t = ctx.animate_bool_with_time(Id::unique("sheet-open"), self.selected.is_some() && matches!(self.view, View::Devices | View::House), 0.18);
        let Some(d) = self.selected.clone().and_then(|id| devices.iter().find(|d| d.id == id).cloned()) else { return };
        // Un aspirateur robot a son propre panneau (`vacuum_sheet`).
        if !matches!(self.view, View::Devices | View::House) || Self::kind_of_view(&d) == Kind::Vacuum {
            return;
        }
        let screen = ctx.content_rect();
        let width = if screen.width() < 720.0 { screen.width() - 24.0 } else { 400.0 };
        let height = screen.height() - 24.0;
        let area = egui::Modal::default_area(Id::unique("device-sheet")).anchor(Align2::RIGHT_TOP, vec2(-12.0 + (1.0 - t) * 48.0, 12.0));
        let mut frame = egui::Frame::new().fill(theme::surface()).corner_radius(CornerRadius::same(26)).inner_margin(Margin::symmetric(24, 22));
        frame = frame.shadow(egui::epaint::Shadow { offset: [-12, 0], blur: 48, spread: 0, color: Color32::from_black_alpha(if theme::is_light() { 40 } else { 120 }) });
        if !theme::is_light() {
            frame = frame.stroke(Stroke::new(1.0, theme::line()));
        }
        let modal = egui::Modal::new(Id::unique("device-sheet")).area(area).backdrop_color(theme::scrim()).frame(frame).show(ctx, |ui| {
            ui.set_opacity(0.4 + 0.6 * t);
            ui.set_width(width - 48.0);
            ui.set_max_width(width - 48.0);
            ui.set_height(height - 44.0);
            ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                if self.tab == panel::PanelTab::Control {
                    self.sheet_main(ui, &d);
                } else {
                    self.sheet_page(ui, &d);
                }
            });
        });
        if modal.should_close() {
            if self.tab != panel::PanelTab::Control && !modal.backdrop_response.clicked() {
                self.tab = panel::PanelTab::Control;
            } else {
                self.selected = None;
            }
        }
    }

    /// Page principale du panneau : allumer / éteindre, l'essentiel, la minuterie, puis trois lignes.
    fn sheet_main(&mut self, ui: &mut Ui, d: &DeviceView) {
        let kind = Self::kind_of_view(d);
        let on = d.online && self.is_on(d, &kind);
        let metering = kinds::power_w(&d.dps, &d.schema).is_some() || self.hub.energy(|e| e.has_data(&d.id));
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(52.0, 52.0), Sense::hover());
            let t = ui.ctx().animate_bool_with_time(Id::unique(("sheet-on", &d.id)), on, 0.18);
            widgets::paint_bubble(ui, r, Icon::from(&kind), t, theme::sun());
            ui.allocate_ui(vec2(ui.available_width() - 48.0, 52.0), |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.add(Label::new(RichText::new(&d.name).font(theme::bold(22.0))).truncate());
                    ui.label(RichText::new(d.room.as_deref().unwrap_or("Sans pièce")).size(13.5).color(theme::muted()));
                });
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::round_button(ui, Icon::Close, false).on_hover_text("Fermer (Échap)").clicked() {
                    self.selected = None;
                }
            });
        });
        ui.add_space(18.0);
        if !d.online {
            // La détection des problèmes dit depuis quand, et la cause probable.
            let why = self.problems_now.iter().find(|p| p.device == d.id && p.kind == homesmart::hub::ProblemKind::Offline).map(|p| p.detail.clone());
            egui::Frame::new().fill(theme::raised()).corner_radius(CornerRadius::same(14)).inner_margin(Margin::same(12)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("Hors ligne").strong().color(theme::text()));
                let text = why.or_else(|| d.error.clone()).unwrap_or_else(|| "L'appareil ne répond pas : vérifie qu'il est branché et sur le Wi-Fi.".into());
                ui.label(RichText::new(text).size(12.5).color(theme::muted()));
            });
            ui.add_space(12.0);
        }
        self.faults_banner(ui, d);
        self.device_problems(ui, d);
        ui.visuals_mut().selection.bg_fill = theme::sun();
        let accent = theme::sun();
        ui.add_enabled_ui(d.online, |ui| {
            let switch = match &kind {
                Kind::Cover | Kind::Switch { .. } => None,
                k => k.switch_dp(&d.dps),
            };
            match &kind {
                Kind::Switch { gangs } => {
                    if let Some(v) = widgets::power_switch(ui, on, d.online) {
                        self.request_power(d, gangs.iter().map(|g| (g.clone(), v)).collect());
                    }
                }
                _ => {
                    if let Some(dp) = &switch {
                        if let Some(v) = widgets::power_switch(ui, on, d.online) {
                            self.request_power(d, vec![(dp.clone(), v)]);
                        }
                    }
                }
            }
            ui.add_space(14.0);
            match &kind {
                Kind::LightV2 | Kind::LightV1 => self.light_controls(ui, d, &kind, accent),
                Kind::Plug { .. } => {
                    if let Some((w, _, _)) = kinds::metering(&d.dps) {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            ui.label(RichText::new(fr(w, 0)).font(theme::light(50.0)));
                            ui.label(RichText::new("W").size(18.0).color(theme::muted()));
                        });
                    }
                }
                Kind::Switch { gangs } => self.switch_controls(ui, d, gangs, accent),
                Kind::Cover => self.cover_controls(ui, d, accent),
                Kind::Vacuum => {}
                Kind::Generic => self.dp_editor(ui, d, accent),
            }
        });
        // Effet des garde-fous, en une phrase (un clic ouvre les réglages de sécurité).
        if d.safety.enabled {
            let text = match (on, d.on_for_s, d.safety.max_on_minutes) {
                (true, Some(on_for), Some(max)) => format!("S'éteindra seul dans {}", panel::fmt_duration((max as u64 * 60).saturating_sub(on_for))),
                _ => "Garde-fous actifs".to_string(),
            };
            ui.add_space(8.0);
            let r = ui
                .horizontal(|ui| {
                    let (ic, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                    icons::paint(ui.painter(), ic, Icon::Shield, theme::accent());
                    ui.label(RichText::new(text).size(13.5).color(theme::muted()));
                })
                .response
                .interact(Sense::click());
            if r.on_hover_text("Réglages de sécurité").clicked() {
                self.tab = panel::PanelTab::Options;
            }
        }
        self.timer_line(ui, d, &kind);
        ui.add_space(20.0);
        if metering {
            let today = self.hub.energy(|e| e.day_wh(&d.id, chrono::Local::now().date_naive()));
            if widgets::nav_row(ui, "Consommation", &format!("{} kWh aujourd'hui", fr(today / 1000.0, 1))).clicked() {
                self.tab = panel::PanelTab::Energy;
            }
        }
        let programs = self.hub.schedules().iter().filter(|s| s.device == d.id && s.enabled).count();
        let value = match programs {
            0 => "aucun".to_string(),
            1 => "1 actif".to_string(),
            n => format!("{n} actifs"),
        };
        if widgets::nav_row(ui, "Programmes", &value).clicked() {
            self.tab = panel::PanelTab::Programs;
        }
        if widgets::nav_row(ui, "Réglages", "sécurité, nom, pièce").clicked() {
            self.tab = panel::PanelTab::Options;
        }
    }

    /// Une page du panneau (consommation, programmes, réglages), avec retour à la page principale.
    fn sheet_page(&mut self, ui: &mut Ui, d: &DeviceView) {
        let kind = Self::kind_of_view(d);
        let accent = theme::sun();
        let title = match self.tab {
            panel::PanelTab::Energy => "Consommation",
            panel::PanelTab::Programs => "Programmes",
            _ => "Réglages",
        };
        ui.horizontal(|ui| {
            if widgets::round_button(ui, Icon::Back, false).on_hover_text("Retour").clicked() {
                self.tab = panel::PanelTab::Control;
            }
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new(title).font(theme::bold(21.0)));
                ui.label(RichText::new(&d.name).size(12.5).color(theme::muted()));
            });
        });
        ui.add_space(16.0);
        ui.visuals_mut().selection.bg_fill = accent;
        match self.tab {
            panel::PanelTab::Energy => self.energy_tab(ui, d, accent),
            panel::PanelTab::Programs => self.programs_tab(ui, &d.id, false, theme::accent()),
            _ => {
                widgets::section_title(ui, "Sécurité", None);
                self.safety_tab(ui, d, &kind, accent);
                ui.add_space(16.0);
                widgets::section_title(ui, "Appareil", None);
                self.options_tab(ui, d, theme::accent());
            }
        }
    }

    fn power_row(&mut self, ui: &mut Ui, d: &DeviceView, dp: &str, label: &str, accent: Color32) {
        let on = self.flag(d, dp).unwrap_or(false);
        widgets::row(ui, label, None, |ui| {
            if widgets::toggle(ui, on, accent, d.online).clicked() {
                self.request_power(d, vec![(dp.to_string(), !on)]);
            }
        });
    }

    /// Curseur 0‥100 % lié à un DP numérique `lo..=hi`.
    fn percent_slider(&mut self, ui: &mut Ui, d: &DeviceView, dp: &str, label: &str, hint: Option<&str>, lo: f64, hi: f64, min_pct: f64) {
        let raw = self.num(d, dp).unwrap_or(hi);
        let mut pct = ((raw - lo) / (hi - lo) * 100.0).clamp(min_pct, 100.0);
        ui.label(RichText::new(label).size(14.0));
        if let Some(h) = hint {
            ui.label(RichText::new(h).size(11.0).color(theme::faint()));
        }
        ui.spacing_mut().slider_width = ui.available_width() - 70.0;
        let r = ui.add(egui::Slider::new(&mut pct, min_pct..=100.0).suffix(" %").integer().trailing_fill(true));
        if r.changed() {
            let v = (lo + pct / 100.0 * (hi - lo)).round() as i64;
            self.set(&d.id, dp, json!(v));
        }
        ui.add_space(6.0);
    }

    fn light_controls(&mut self, ui: &mut Ui, d: &DeviceView, kind: &Kind, accent: Color32) {
        let v2 = *kind == Kind::LightV2;
        let (sw, mode_dp, b_dp, t_dp, c_dp) = if v2 { ("20", "21", "22", "23", "24") } else { ("1", "2", "3", "4", "5") };
        let (lo, hi) = if v2 { (10.0, 1000.0) } else { (25.0, 255.0) };
        let has_colour = d.dps.contains_key(c_dp);
        let mode = self.text(d, mode_dp).unwrap_or_else(|| "white".into());
        if has_colour && d.dps.contains_key(mode_dp) {
            let sel = match mode.as_str() {
                "white" => Some(0),
                "colour" => Some(1),
                _ => None,
            };
            if let Some(i) = widgets::segmented(ui, &["Blanc", "Couleur"], sel, accent) {
                self.set(&d.id, mode_dp, json!(["white", "colour"][i]));
                if !self.flag(d, sw).unwrap_or(false) {
                    self.request_power(d, vec![(sw.to_string(), true)]);
                }
            }
            ui.add_space(10.0);
        }
        if mode == "colour" && has_colour {
            let current = self.text(d, c_dp).and_then(|s| if v2 { kinds::parse_colour_v2(&s) } else { kinds::parse_colour_v1(&s) });
            let c = current.unwrap_or(Hsv { h: 0.08, s: 0.8, v: 1.0 });
            let mut hsva = egui::ecolor::Hsva::new(c.h, c.s, c.v, 1.0);
            ui.spacing_mut().slider_width = ui.available_width() - 30.0;
            if egui::widgets::color_picker::color_picker_hsva_2d(ui, &mut hsva, egui::widgets::color_picker::Alpha::Opaque) {
                let c = Hsv { h: hsva.h, s: hsva.s, v: hsva.v.max(0.01) };
                let s = if v2 { kinds::encode_colour_v2(c) } else { kinds::encode_colour_v1(c) };
                self.set(&d.id, c_dp, json!(s));
            }
            ui.label(RichText::new("La luminosité de la couleur se règle avec la valeur (curseur du bas).").size(11.0).color(theme::faint()));
        } else {
            self.percent_slider(ui, d, b_dp, "Luminosité", None, lo, hi, 1.0);
            if d.dps.contains_key(t_dp) {
                self.percent_slider(ui, d, t_dp, "Teinte du blanc", Some("0 % = chaud (orangé)  ·  100 % = froid (bleuté)"), 0.0, if v2 { 1000.0 } else { 255.0 }, 0.0);
            }
        }
    }

    fn switch_controls(&mut self, ui: &mut Ui, d: &DeviceView, gangs: &[String], accent: Color32) {
        for (i, g) in gangs.iter().enumerate() {
            let label = Self::schema_code(d, g).map(kinds::label_for_code).unwrap_or_else(|| format!("Sortie {}", i + 1));
            let label = if label == "Marche" { "Sortie 1".to_string() } else { label };
            self.power_row(ui, d, g, &label, accent);
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            for (text, value) in [("Tout allumer", true), ("Tout éteindre", false)] {
                if ui.button(text).clicked() {
                    self.request_power(d, gangs.iter().map(|g| (g.clone(), value)).collect());
                }
            }
        });
    }

    fn cover_controls(&mut self, ui: &mut Ui, d: &DeviceView, accent: Color32) {
        let width = (ui.available_width() - 16.0) / 3.0;
        ui.horizontal(|ui| {
            for (text, cmd) in [("Ouvrir", "open"), ("Stop", "stop"), ("Fermer", "close")] {
                let active = self.text(d, "1").as_deref() == Some(cmd) && cmd != "stop";
                let btn = egui::Button::new(RichText::new(text).color(if active { theme::bg() } else { theme::text() }))
                    .fill(if active { accent } else { theme::raised() })
                    .min_size(vec2(width, 38.0));
                if ui.add(btn).clicked() {
                    self.set(&d.id, "1", json!(cmd));
                }
            }
        });
        if d.dps.contains_key("2") {
            ui.add_space(12.0);
            let hint = self.num(d, "3").map(|p| format!("Position actuelle : {p:.0} %"));
            self.percent_slider(ui, d, "2", "Position", hint.as_deref(), 0.0, 100.0, 0.0);
        }
    }

    /// Éditeur générique de tous les DP, guidé par le schéma s'il existe.
    fn dp_editor(&mut self, ui: &mut Ui, d: &DeviceView, accent: Color32) {
        let mut keys: Vec<String> = d.dps.keys().cloned().collect();
        keys.sort_by_key(|k| k.parse::<u32>().unwrap_or(u32::MAX));
        if keys.is_empty() {
            ui.label(RichText::new("En attente de l'état de l'appareil…").color(theme::muted()));
        }
        let protected_dps = kinds::protected_dps(&d.dps, &d.schema);
        for k in keys {
            let Some(v) = self.value(d, &k) else { continue };
            let code = Self::schema_code(d, &k).map(str::to_string);
            let label = code.as_deref().map(kinds::label_for_code).unwrap_or_else(|| format!("DP {k}"));
            let hint = match &code {
                Some(c) => format!("dp {k} · {c}"),
                None => format!("dp {k}"),
            };
            let protected = protected_dps.contains(&k);
            let readonly = protected || code.as_deref().is_some_and(kinds::is_readonly_code);
            let hint = if protected { format!("{hint} · réglage d'usine, verrouillé") } else { hint };
            let values = d.schema.get(&k).and_then(|s| s.get("values")).cloned().unwrap_or(Value::Null);
            let is_bitmap = d.schema.get(&k).and_then(|s| s.get("type")).and_then(Value::as_str) == Some("Bitmap");
            let code_s = code.clone().unwrap_or_default();
            widgets::row(ui, &label, Some(&hint), |ui| match &v {
                Value::Number(_) if is_bitmap => {
                    let (_, labels) = kinds::faults(&d.dps, &d.schema);
                    if labels.is_empty() {
                        ui.label(RichText::new("aucun").color(theme::accent()));
                    } else {
                        ui.label(RichText::new(labels.join(", ")).strong().color(theme::crit()));
                    }
                }
                Value::Bool(b) => {
                    if widgets::toggle(ui, *b, accent, d.online && !readonly).clicked() {
                        self.set(&d.id, &k, Value::Bool(!b));
                    }
                }
                Value::Number(n) if readonly => {
                    ui.label(RichText::new(self.format_value(d, &k, &v)).strong());
                    let _ = n;
                }
                Value::Number(n) => {
                    let mut x = n.as_f64().unwrap_or(0.0);
                    let min = values.get("min").and_then(Value::as_f64);
                    let max = values.get("max").and_then(Value::as_f64);
                    let scale = values.get("scale").and_then(Value::as_i64).unwrap_or(0).clamp(0, 6) as i32;
                    let unit = values.get("unit").and_then(Value::as_str).unwrap_or("").to_string();
                    let changed = if let (Some(lo), Some(hi)) = (min, max) {
                        let div = 10f64.powi(scale);
                        let step = values.get("step").and_then(Value::as_f64).unwrap_or(1.0).max(1.0);
                        ui.spacing_mut().slider_width = 150.0;
                        ui.add(
                            egui::Slider::new(&mut x, lo..=hi)
                                .step_by(step)
                                .custom_formatter(move |v, _| format!("{} {unit}", fr(v / div, scale as usize)).trim().to_string()),
                        )
                        .changed()
                    } else {
                        ui.add(egui::DragValue::new(&mut x).speed(1.0)).changed()
                    };
                    if changed {
                        self.set(&d.id, &k, json!(x.round() as i64));
                    }
                }
                Value::String(s) => {
                    let schema_range = kinds::enum_range(&d.schema, &k);
                    let local = kinds::local_enum_range(d.dps.get(&k).and_then(Value::as_str), &schema_range);
                    let range = local.clone().unwrap_or_default();
                    let readonly = readonly || (!schema_range.is_empty() && local.is_none());
                    if readonly {
                        ui.label(RichText::new(s).strong());
                    } else if !range.is_empty() {
                        let mut chosen = None;
                        egui::ComboBox::from_id_salt(("enum", &d.id, &k)).selected_text(kinds::enum_label(&code_s, s)).width(170.0).show_ui(ui, |ui| {
                            for opt in &range {
                                if ui.selectable_label(opt == s, kinds::enum_label(&code_s, opt)).clicked() {
                                    chosen = Some(opt.clone());
                                }
                            }
                        });
                        if let Some(c) = chosen {
                            self.set(&d.id, &k, json!(c));
                        }
                    } else {
                        let key = (d.id.clone(), k.clone());
                        let buf = self.text_buf.entry(key.clone()).or_insert_with(|| s.clone());
                        let r = ui.add(egui::TextEdit::singleline(buf).desired_width(150.0));
                        let submit = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if submit {
                            let new = buf.clone();
                            self.text_buf.remove(&key);
                            self.set(&d.id, &k, json!(new));
                        } else if !r.has_focus() {
                            self.text_buf.remove(&key);
                        }
                    }
                }
                Value::Null => {
                    ui.label(RichText::new("—").color(theme::faint()));
                }
                other => {
                    ui.label(RichText::new(other.to_string()).monospace().size(12.0));
                }
            });
        }
    }

    // ---- Réglages ------------------------------------------------------------------

    fn settings_ui(&mut self, ui: &mut Ui, d: &DeviceView) {
        if self.settings.as_ref().is_none_or(|s| s.original.id != d.id) {
            let Some(cfg) = self.hub.config_of(&d.id) else { return };
            self.settings = Some(SettingsForm {
                name: cfg.name.clone(),
                room: cfg.room.clone().unwrap_or_default(),
                ip: cfg.ip.clone().unwrap_or_default(),
                version: cfg.version.clone().unwrap_or_default(),
                key: cfg.key.clone(),
                kind: cfg.kind.clone().unwrap_or_default(),
                original: cfg,
                show_key: false,
                confirm_delete: false,
            });
        }
        let Some(mut s) = self.settings.take() else { return };
        let mut save = false;
        let mut delete = false;
        // État de la connexion (informations techniques, ici seulement).
        let mut info = vec![if d.online { "En ligne".to_string() } else { "Hors ligne".to_string() }];
        info.extend(d.ip.clone());
        info.extend(d.version.as_ref().map(|v| format!("protocole {v}")));
        if let (true, Some(ms)) = (d.online, d.latency_ms) {
            info.push(format!("réponse en {ms} ms"));
        }
        ui.label(RichText::new(info.join("  ·  ")).size(12.5).color(theme::muted()));
        ui.add_space(6.0);
        if matches!(Self::kind_of_view(d), Kind::Plug { .. } | Kind::Switch { .. }) {
            let hint = "Quand elle coupe, ce PC et HomeSmart s'arrêtent avec elle : au redémarrage, HomeSmart dit ce qui s'est passé et la nomme.";
            let flip = widgets::row(ui, "Alimente ce PC", Some(hint), |ui| widgets::toggle(ui, d.powers_pc, theme::accent(), true).clicked());
            if flip {
                if let Err(e) = self.hub.update(&d.id, DevicePatch { powers_pc: Some(!d.powers_pc), ..Default::default() }) {
                    self.toast(format!("{e:#}"), true);
                }
            }
            ui.add_space(6.0);
        }
        egui::Grid::new(("settings-grid", &d.id)).num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            let w = 210.0;
            ui.label("Nom");
            ui.add(egui::TextEdit::singleline(&mut s.name).desired_width(w));
            ui.end_row();
            ui.label("Pièce");
            ui.add(egui::TextEdit::singleline(&mut s.room).hint_text("Salon, Chambre…").desired_width(w));
            ui.end_row();
            ui.label("Type");
            let current = KINDS.iter().find(|(k, _)| *k == s.kind).map(|(_, l)| *l).unwrap_or("Automatique");
            egui::ComboBox::from_id_salt(("kind", &d.id)).selected_text(current).width(w).show_ui(ui, |ui| {
                for (k, l) in KINDS {
                    ui.selectable_value(&mut s.kind, k.to_string(), l);
                }
            });
            ui.end_row();
            ui.label("Adresse IP");
            ui.add(egui::TextEdit::singleline(&mut s.ip).hint_text("automatique").desired_width(w));
            ui.end_row();
            ui.label("Protocole");
            let shown = if s.version.is_empty() { "Automatique".to_string() } else { s.version.clone() };
            egui::ComboBox::from_id_salt(("ver", &d.id)).selected_text(shown).width(w).show_ui(ui, |ui| {
                for v in VERSIONS {
                    ui.selectable_value(&mut s.version, v.to_string(), if v.is_empty() { "Automatique" } else { v });
                }
            });
            ui.end_row();
            ui.label("Clé locale");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut s.key).password(!s.show_key).desired_width(w - 64.0));
                ui.checkbox(&mut s.show_key, "voir");
            });
            ui.end_row();
            ui.label(RichText::new("Identifiant").color(theme::muted()));
            ui.label(RichText::new(&d.id).monospace().size(12.0).color(theme::muted()));
            ui.end_row();
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.add(widgets::primary_button("Enregistrer", theme::accent())).clicked() {
                save = true;
            }
            if !s.confirm_delete {
                if ui.button(RichText::new("Supprimer…").color(theme::crit())).clicked() {
                    s.confirm_delete = true;
                }
            } else {
                if ui.add(egui::Button::new(RichText::new("Confirmer la suppression").color(theme::on_accent())).fill(theme::crit())).clicked() {
                    delete = true;
                }
                if ui.button("Annuler").clicked() {
                    s.confirm_delete = false;
                }
            }
        });

        if delete {
            match self.hub.remove(&d.id) {
                Ok(()) => {
                    self.toast(format!("« {} » supprimé", d.name), false);
                    self.selected = None;
                }
                Err(e) => self.toast(format!("{e:#}"), true),
            }
            return;
        }
        if save {
            let o = &s.original;
            let changed = |new: &str, old: Option<&str>| (new.trim() != old.unwrap_or("")).then(|| new.trim().to_string());
            let patch = DevicePatch {
                name: changed(&s.name, Some(&o.name)),
                room: changed(&s.room, o.room.as_deref()),
                kind: changed(&s.kind, o.kind.as_deref()),
                ip: changed(&s.ip, o.ip.as_deref()),
                version: changed(&s.version, o.version.as_deref()),
                key: changed(&s.key, Some(&o.key)),
                powers_pc: None,
            };
            match self.hub.update(&d.id, patch) {
                Ok(_) => {
                    self.toast("Réglages enregistrés", false);
                    self.settings = None;
                    return;
                }
                Err(e) => self.toast(format!("{e:#}"), true),
            }
        }
        self.settings = Some(s);
    }

    // ---- Ajout d'un appareil ---------------------------------------------------------

    /// Ajout d'un appareil : depuis Smart Life (le plus simple), parmi ceux qui s'annoncent sur le
    /// Wi-Fi, ou à la main (identifiant et clé locale).
    fn add_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut f) = self.add.take() else { return };
        let mut close = false;
        let mut submit = false;
        let mut open_cloud = false;
        let mut open_vacuum = false;
        let unknown: Vec<DiscoveredView> = self.hub.discovered().into_iter().filter(|d| !d.known).collect();
        let scanning = self.scan_until.is_some();
        let mut rescan = false;
        let frame = egui::Frame::new().fill(theme::surface()).corner_radius(CornerRadius::same(22)).inner_margin(Margin::same(24));
        let modal = egui::Modal::new(Id::unique("add-device")).backdrop_color(theme::scrim()).frame(frame).show(ctx, |ui| {
            ui.set_width(460.0);
            ScrollArea::vertical().max_height(ctx.content_rect().height() - 140.0).show(ui, |ui| {
            ui.label(RichText::new("Ajouter un appareil").font(theme::bold(21.0)));
            ui.add_space(4.0);
            ui.label(RichText::new("Le plus simple : récupérer d'un coup tous tes appareils et leurs clés depuis Smart Life.").size(13.0).color(theme::muted()));
            ui.add_space(12.0);
            if ui.add(widgets::primary_button("Importer depuis Smart Life", theme::accent()).min_size(vec2(ui.available_width(), 40.0))).clicked() {
                open_cloud = true;
            }
            ui.add_space(8.0);
            if ui.add(egui::Button::new("Aspirateur robot : Valetudo, Xiaomi, Roborock…").min_size(vec2(ui.available_width(), 36.0))).clicked() {
                open_vacuum = true;
            }
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Sur le Wi-Fi").font(theme::bold(14.0)).color(theme::muted()));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if scanning {
                        ui.spinner();
                    } else if ui.small_button("Chercher à nouveau").clicked() {
                        rescan = true;
                    }
                });
            });
            if unknown.is_empty() {
                ui.label(RichText::new(if scanning { "Recherche des appareils qui s'annoncent…" } else { "Aucun nouvel appareil ne s'annonce pour l'instant." }).size(12.5).color(theme::faint()));
            }
            for d in unknown.iter().take(5) {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&d.device.id).monospace().size(12.5));
                    ui.label(RichText::new(&d.device.ip).size(12.5).color(theme::muted()));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Choisir").clicked() {
                            (f.id, f.ip, f.version) = (d.device.id.clone(), d.device.ip.clone(), d.device.version.clone());
                        }
                    });
                });
            }
            ui.add_space(14.0);
            ui.label(RichText::new("À la main").font(theme::bold(14.0)).color(theme::muted()));
            ui.add_space(4.0);
            egui::Grid::new("add-grid").num_columns(2).spacing([12.0, 9.0]).show(ui, |ui| {
                let w = 300.0;
                ui.label("Identifiant");
                ui.add(egui::TextEdit::singleline(&mut f.id).hint_text("bf1a2b3c4d5e6f7a8b9c").desired_width(w));
                ui.end_row();
                ui.label("Clé locale");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut f.key).password(!f.show_key).hint_text("16 caractères").desired_width(w - 100.0));
                    let n = f.key.trim().len();
                    ui.label(RichText::new(format!("{n}/16")).size(12.0).color(if n == 16 { theme::accent() } else { theme::faint() }));
                    ui.checkbox(&mut f.show_key, "voir");
                });
                ui.end_row();
                ui.label("Nom");
                ui.add(egui::TextEdit::singleline(&mut f.name).hint_text("Lampe du salon").desired_width(w));
                ui.end_row();
                ui.label("Pièce");
                ui.add(egui::TextEdit::singleline(&mut f.room).hint_text("Salon").desired_width(w));
                ui.end_row();
            });
            egui::CollapsingHeader::new(RichText::new("Avancé : adresse IP et protocole").size(13.0).color(theme::muted())).id_salt("add-advanced").show(ui, |ui| {
                egui::Grid::new("add-grid-2").num_columns(2).spacing([12.0, 9.0]).show(ui, |ui| {
                    let w = 260.0;
                    ui.label("Adresse IP");
                    ui.add(egui::TextEdit::singleline(&mut f.ip).hint_text("automatique (annonce Wi-Fi)").desired_width(w));
                    ui.end_row();
                    ui.label("Protocole");
                    let shown = if f.version.is_empty() { "Automatique".to_string() } else { f.version.clone() };
                    egui::ComboBox::from_id_salt("add-ver").selected_text(shown).width(w).show_ui(ui, |ui| {
                        for v in VERSIONS {
                            ui.selectable_value(&mut f.version, v.to_string(), if v.is_empty() { "Automatique" } else { v });
                        }
                    });
                    ui.end_row();
                });
            });
            if let Some(e) = &f.error {
                ui.add_space(6.0);
                ui.label(RichText::new(e).color(theme::crit()));
            }
            });
            ui.add_space(14.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let valid = !f.id.trim().is_empty() && f.key.trim().len() == 16;
                if ui.add_enabled(valid, widgets::primary_button("Ajouter", theme::accent()).min_size(vec2(100.0, 36.0))).clicked() {
                    submit = true;
                }
                if ui.add(egui::Button::new("Annuler").min_size(vec2(0.0, 36.0))).clicked() {
                    close = true;
                }
            });
        });
        if modal.should_close() {
            close = true;
        }
        if rescan {
            self.hub.trigger_scan();
            self.scan_until = Some(Instant::now() + Duration::from_secs(4));
        }
        if open_cloud {
            self.cloud = Some(cloud_import::CloudImport::new());
            return;
        }
        if open_vacuum {
            self.vac.add = Some(vacuum_ui::VacAdd::new());
            return;
        }
        if submit {
            let cfg = DeviceConfig {
                id: f.id.trim().to_string(),
                key: f.key.trim().to_string(),
                name: f.name.trim().to_string(),
                ip: Some(f.ip.trim().to_string()).filter(|s| !s.is_empty()),
                port: None,
                version: Some(f.version.clone()).filter(|s| !s.is_empty()),
                room: Some(f.room.trim().to_string()).filter(|s| !s.is_empty()),
                kind: None,
                schema: Map::new(),
                safety: Default::default(),
                powers_pc: false,
            };
            match self.hub.add(cfg) {
                Ok(v) => {
                    self.toast(format!("« {} » ajouté", v.name), false);
                    self.select(&v.id);
                    return;
                }
                Err(e) => f.error = Some(format!("{e:#}")),
            }
        }
        if !close {
            self.add = Some(f);
        }
    }

    // ---- Divers ------------------------------------------------------------------------

    fn import_dropped(&mut self, ctx: &egui::Context) {
        let files: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        for path in files {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            // Une photo : visage à enregistrer pour la reconnaissance.
            if path.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png")) {
                self.photo_face = Some((path, String::new(), None));
                continue;
            }
            match config::import_tinytuya(&path) {
                Ok(list) if list.is_empty() => self.toast(format!("{name} : aucun appareil avec clé locale"), true),
                Ok(list) => {
                    let total = list.len();
                    let ok = list.into_iter().filter(|d| self.hub.add(d.clone()).is_ok()).count();
                    self.toast(format!("{ok} / {total} appareil(s) importé(s) depuis {name}"), ok < total);
                }
                Err(e) => self.toast(format!("{name} : {e:#}"), true),
            }
        }
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let screen = ctx.content_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::unique("drop")));
            painter.rect_filled(screen, 0.0, Color32::from_black_alpha(190));
            painter.text(
                screen.center(),
                Align2::CENTER_CENTER,
                "Dépose le devices.json de tinytuya (appareils) ou la photo d'un visage (JPEG, PNG)",
                egui::FontId::proportional(20.0),
                theme::text(),
            );
        }
    }

    /// Notifications : elles glissent depuis la droite puis s'effacent en fondu.
    fn toasts_ui(&self, ctx: &egui::Context) {
        let toasts = self.toasts.lock().unwrap();
        let mut y = -20.0;
        for (i, t) in toasts.iter().enumerate().rev() {
            let age = t.at.elapsed().as_secs_f32();
            let appear = fx::ease_out(age / 0.32);
            let fade = ((TOAST_TTL.as_secs_f32() - age) / 0.45).clamp(0.0, 1.0);
            if appear < 1.0 || fade < 1.0 {
                ctx.request_repaint();
            }
            let accent = if t.error { theme::crit() } else { theme::accent() };
            let r = egui::Area::new(Id::unique(("toast", i, t.at)))
                .anchor(Align2::RIGHT_BOTTOM, vec2(-20.0 + (1.0 - appear) * 80.0, y))
                .order(egui::Order::Foreground)
                .interactable(false)
                .show(ctx, |ui| {
                    ui.set_opacity(appear.min(fade));
                    egui::Frame::new()
                        .fill(theme::alpha(theme::surface(), 0.96))
                        .corner_radius(CornerRadius::same(12))
                        .inner_margin(Margin { left: 16, right: 16, top: 11, bottom: 11 })
                        .stroke(Stroke::new(1.0, theme::mix(theme::line(), accent, 0.35)))
                        .shadow(egui::epaint::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(120) })
                        .show(ui, |ui| {
                            ui.set_max_width(380.0);
                            ui.horizontal(|ui| {
                                let (bar, _) = ui.allocate_exact_size(vec2(3.0, 18.0), Sense::hover());
                                ui.painter().rect_filled(bar, CornerRadius::same(2), accent);
                                ui.add(egui::Label::new(RichText::new(&t.text).color(if t.error { theme::crit() } else { theme::text() })).wrap());
                            });
                        });
                });
            y -= r.response.rect.height() + 8.0;
        }
    }

    fn kind_of_view(d: &DeviceView) -> Kind {
        kinds::detect(d.kind.as_deref(), &d.dps)
    }

    /// Affiche les nouvelles alertes en notification ; une alerte de sécurité attire l'attention.
    fn notify_alerts(&mut self, ctx: &egui::Context) {
        let (latest, _) = self.hub.alert_stats(self.toasted);
        if self.view == View::Alerts {
            self.alerts_seen = latest;
        }
        if latest <= self.toasted {
            return;
        }
        let fresh = self.hub.alerts_after(self.toasted);
        if fresh.is_empty() {
            self.toasted = latest;
            return;
        }
        use homesmart::hub::{Level, Source};
        let notable = |a: &&homesmart::hub::Alert| a.level != Level::Info || matches!(a.source, Source::Schedule | Source::Guard | Source::Cli | Source::Scenario);
        for a in fresh.iter().rev().filter(notable).take(4) {
            let who = if a.device_name.is_empty() { String::new() } else { format!("{} : ", a.device_name) };
            // Un problème : son titre seulement (le détail est sur la page Problèmes).
            let text = match homesmart::problems::parse_key(&a.key) {
                Some((kind, _, false)) => kind.title().to_string(),
                _ => a.message.clone(),
            };
            self.toast(format!("{who}{text}"), a.level != Level::Info);
            if a.level == homesmart::hub::Level::Critical {
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Critical));
            }
        }
        self.toasted = fresh[0].id;
    }

    fn auto_shot(&mut self, ctx: &egui::Context, devices: &[DeviceView]) {
        let Some(mut a) = self.auto.take() else { return };
        if let Some(sel) = a.select.take() {
            if sel == "+cloudqr" {
                self.cloud = Some(cloud_import::CloudImport::preview_qr());
            } else if sel == "+cloud" {
                self.cloud = Some(cloud_import::CloudImport::new());
            } else if sel == "+vac" {
                self.vac.add = Some(vacuum_ui::VacAdd::new());
            } else if let Some(v) = sel.strip_prefix("!clean:").and_then(|n| self.hub.vacuums().into_iter().find(|v| v.name == n && v.online)) {
                // « !clean:Nom » : lance un nettoyage (captures du robot au travail).
                self.vac_start_for_shot(&v.id);
                self.select(&v.id);
            } else if sel == "+add" {
                self.add = Some(AddForm { id: "bf1a2b3c4d5e6f7a8b9c".into(), name: "Lampe de chevet".into(), room: "Chambre".into(), ..AddForm::default() });
            } else if let Some(v) = sel.strip_prefix('@') {
                self.view = match v {
                    "cameras" => View::Cameras,
                    "energy" => View::Energy,
                    "calendar" => View::Calendar,
                    "alerts" => View::Alerts,
                    "problems" => View::Problems,
                    "house" => View::House,
                    // « @house+ia » : la maison 3D, l'analyse IA activée sur toutes les caméras (présence).
                    "house+ia" => {
                        for c in self.hub.cameras() {
                            let on = homesmart::vision::CameraVision { enabled: true, ..self.hub.camera_vision(&c.id) };
                            let _ = self.hub.set_camera_vision(&c.id, on, homesmart::hub::Source::User);
                        }
                        View::House
                    }
                    "energy3d" => View::Energy,
                    "plan" => {
                        self.house.mode = 1;
                        View::House
                    }
                    _ => View::Devices,
                };
            } else if let Some(n) = sel.strip_prefix("cam:") {
                // Caméra en vue détaillée ; « cam:Nom#hd » pour le flux HD.
                let (n, tag) = n.split_once('#').unwrap_or((n, ""));
                let hd = tag == "hd";
                self.cams.open_quality = tag == "qualite";
                self.cams.open_image = tag == "image";
                // « #enreg » : vue Enregistrements ; « #lecture » : la même, en relisant la dernière
                // séquence ; « #lecture+son » : avec le son.
                self.cams.rec_mode = tag == "enreg" || tag.starts_with("lecture");
                self.cams.rec.autoplay = tag.starts_with("lecture");
                self.cams.rec.autosound = tag == "lecture+son";
                // « #vision » : colonne sur la vision IA ; « #vision+on » : analyse activée ;
                // « #zones » : une zone d'exemple, dessinée et listée ; « #zones+dessin » : avec une
                // autre en cours de dessin.
                // « #scenarios » : formulaire d'ajout de scénario ouvert ; « #reglages » : onglet Réglages.
                // « #avance » : page Avancé (vision IA complète) ; zones et scénarios y sont aussi.
                self.cams.side_tab = if tag.starts_with("zones") || tag == "scenarios" || tag == "avance" {
                    1
                } else if matches!(tag, "qualite" | "image" | "reglages") {
                    2
                } else {
                    0
                };
                self.cams.open_scenarios = tag == "scenarios";
                // « #ptz-souris » / « #ptz-clavier » : appui simulé de 3 s vers la droite (mesures).
                if let Some(how) = tag.strip_prefix("ptz-") {
                    self.cams.ptz_script = Some(cameras::PtzScript::new(how == "souris"));
                }
                match self.hub.cameras().into_iter().find(|c| c.name == n) {
                    Some(c) => {
                        self.view = View::Cameras;
                        // « #vision+ecoute » : analyse et écoute (sons et parole) activées.
                        if tag == "vision+on" || tag == "vision+ecoute" {
                            let listen = tag == "vision+ecoute";
                            let on = homesmart::vision::CameraVision { enabled: true, sounds: listen, speech: listen, ..self.hub.camera_vision(&c.id) };
                            let _ = self.hub.set_camera_vision(&c.id, on, homesmart::hub::Source::User);
                        }
                        if tag.starts_with("zones") {
                            self.demo_zones(&c.id, tag.starts_with("zones+dessin"));
                        }
                        if tag == "scenarios" {
                            self.demo_scenario_form(&c.id);
                        }
                        // « #zoom-gestes » : molette puis glisser simulés ; « #zoom-gestes+double » : puis
                        // double-clic.
                        if let Some(more) = tag.strip_prefix("zoom-gestes") {
                            self.cams.zoom_script = Some(cameras::zoom::ZoomScript::new(more == "+double"));
                        }
                        // « #zoom » (seul ou ajouté : « #lecture+zoom », « #zones+zoom ») : vue zoomée
                        // (2,5×, un peu à droite du centre) ; la relecture si elle est demandée.
                        if tag.split('+').any(|t| t == "zoom") {
                            let key = if self.cams.rec_mode { format!("{}~pb", c.id) } else { c.id.clone() };
                            self.cams.zoom.insert(key, cameras::zoom::Zoom::new(2.5, pos2(0.62, 0.45)));
                        }
                        self.cams.selected = Some(c.id);
                        self.cams.hd = hd;
                    }
                    None => a.select = Some(sel.clone()),
                }
            } else if sel == "+prefs" {
                self.prefs_open = true;
            } else if sel == "+cam" {
                self.view = View::Cameras;
                let found = homesmart::camera::discover::Found {
                    ip: "192.168.1.164".into(),
                    model: Some("IPC-K2EC-3H1W".into()),
                    serial: Some("A1B2C3D4E5F6789".into()),
                    ..Default::default()
                };
                let mut f = cameras::AddCam::from_found(&found);
                (f.name, f.room) = ("Salon".into(), "Salon".into());
                self.cams.add = Some(f);
            } else if sel == "+cam-hs" {
                // Ajout d'une caméra HomeSmart trouvée en mDNS, code d'appairage tapé.
                self.view = View::Cameras;
                let found = homesmart::camera::discover::Found {
                    ip: "192.168.1.60".into(),
                    brand: homesmart::camera::Brand::HomeSmart,
                    model: Some("Radxa ROCK 4D".into()),
                    serial: Some("70c9324b2bdc".into()),
                    vendor: Some("Entrée".into()),
                    rtsp: true,
                    agent_port: Some(homesmart_cam_proto::API_PORT),
                    rtsp_port: Some(homesmart_cam_proto::RTSP_PORT),
                    ..Default::default()
                };
                let mut f = cameras::AddCam::from_found(&found);
                (f.room, f.password) = ("Entrée".into(), "K7PQ-3XWD".into());
                self.cams.add = Some(f);
            } else if let Some(d) = sel.strip_prefix("!confirm:").and_then(|n| devices.iter().find(|d| d.name == n)) {
                let dp = Self::kind_of_view(d).switch_dp(&d.dps).unwrap_or_else(|| "1".into());
                self.request_power(d, vec![(dp, true)]);
            } else if let Some(d) = sel.strip_prefix("!new:").and_then(|n| devices.iter().find(|d| d.name == n)) {
                self.select(&d.id);
                self.tab = panel::PanelTab::Programs;
                self.sched_form = Some(panel::SchedForm::new(&d.id, Self::kind_of_view(d) == Kind::Vacuum));
            } else if let Some((id, tab)) = sel.split_once('#').and_then(|(n, t)| devices.iter().find(|d| d.name == n).map(|d| (d.id.clone(), t.to_string()))) {
                self.select(&id);
                self.tab = match tab.as_str() {
                    "energy" => panel::PanelTab::Energy,
                    "programs" => panel::PanelTab::Programs,
                    "safety" => panel::PanelTab::Safety,
                    "options" => panel::PanelTab::Options,
                    _ => panel::PanelTab::Control,
                };
            } else if let Some(d) = devices.iter().find(|d| d.name == sel) {
                self.select(&d.id);
            } else if let Some((v, tab)) = self.hub.vacuums().into_iter().find_map(|v| {
                // Aspirateur hors Tuya : « Nom » ou « Nom#programs|options ».
                let (n, t) = sel.split_once('#').unwrap_or((sel.as_str(), ""));
                (v.name == n).then(|| (v, t.to_string()))
            }) {
                self.select(&v.id);
                self.tab = match tab.as_str() {
                    "programs" => panel::PanelTab::Programs,
                    "options" => panel::PanelTab::Options,
                    _ => panel::PanelTab::Control,
                };
            } else {
                a.select = Some(sel);
            }
        }
        if !a.requested && Instant::now() >= a.at {
            a.requested = true;
            // Capture automatique : la fenêtre se ferme ensuite sans demander de confirmation.
            self.allow_close = true;
            let path = a.path.clone();
            let c = ctx.clone();
            ctx.request_screenshot(move |img| {
                let rgba: Vec<u8> = img.pixels.iter().flat_map(|p| p.to_array()).collect();
                if let Err(e) = png::write_rgba(&path, img.size[0], img.size[1], &rgba) {
                    eprintln!("capture impossible : {e}");
                }
                c.send_viewport_cmd(egui::ViewportCommand::Close);
                c.request_repaint();
            });
        }
        // Sélection pas encore trouvée, ou capture demandée : on revient vite ; sinon on attend
        // l'heure de la capture, sans redessin en plus (les mesures de HOMESMART_PERF restent justes).
        let wait = if a.select.is_some() || a.requested { Duration::from_millis(100) } else { a.at.saturating_duration_since(Instant::now()) };
        ctx.request_repaint_after(wait);
        self.auto = Some(a);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        let started = Instant::now();
        let ctx = ui.ctx().clone();
        self.import_dropped(&ctx);
        let devices = self.hub.list();
        self.problems_now = self.hub.problems();
        self.housekeeping(&devices);
        if self.selected.as_ref().is_some_and(|id| !devices.iter().any(|d| &d.id == id) && !self.hub.is_vacuum(id)) {
            self.selected = None;
            self.settings = None;
        }

        if std::mem::take(&mut self.start_minimized) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        self.notify_alerts(&ctx);
        let has_focus = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        self.focused.store(has_focus, std::sync::atomic::Ordering::Relaxed);
        fx::background(&ctx);
        if self.view != self.shown_view {
            self.shown_view = self.view;
            self.view_since = Instant::now();
        }
        let vt = fx::progress(&ctx, self.view_since, 0.3);

        // Navigation à gauche ; repliée en icônes quand la fenêtre est étroite.
        let rail = ctx.content_rect().width() < 1000.0;
        let pad: i8 = if rail { 11 } else { 14 };
        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(if rail { 68.0 } else { 212.0 })
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(theme::nav()).inner_margin(Margin { left: pad, right: pad, top: 18, bottom: 14 }).stroke(Stroke::new(1.0, theme::line())))
            .show(ui, |ui| self.sidebar(ui, rail));
        if self.hub.vision_armed() {
            egui::Panel::top("armed")
                .show_separator_line(false)
                .frame(egui::Frame::new().fill(theme::crit()).inner_margin(Margin::symmetric(if rail { 22 } else { 36 }, 8)))
                .show(ui, |ui| self.armed_banner(ui));
        }
        let (side, top): (i8, i8) = if rail { (22, 22) } else { (36, 28) };
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(Color32::TRANSPARENT).inner_margin(Margin { left: side, right: side, top, bottom: 14 }))
            .show(ui, |ui| {
                // Transition entre les vues : fondu et léger glissement vers le haut.
                ui.set_opacity(0.2 + 0.8 * vt);
                ui.add_space((1.0 - vt) * 18.0);
                match self.view {
                View::Devices => self.dashboard(ui, &devices),
                View::Cameras => self.cameras_view(ui),
                View::Energy => self.energy_view(ui, &devices),
                View::Calendar => self.calendar_view(ui, &devices),
                View::Alerts => self.alerts_view(ui, &devices),
                View::Problems => self.problems_view(ui, &devices),
                View::House => self.house_view(ui, &devices),
                }
            });

        // Une seule décision d'orientation par image (pavé et clavier ont proposé leur direction).
        self.ptz_commit();
        self.audio_housekeeping();
        self.recordings_housekeeping();
        self.device_sheet(&ctx, &devices);
        self.vacuum_sheet(&ctx);
        self.add_dialog(&ctx);
        self.vacuum_add_dialog(&ctx);
        self.add_camera_dialog(&ctx);
        self.cloud_dialog(&ctx);
        self.prefs_dialog(&ctx);
        self.confirm_dialog(&ctx);
        self.photo_face_dialog(&ctx);
        self.close_guard(&ctx, &devices);
        self.toasts_ui(&ctx);
        self.auto_shot(&ctx, &devices);
        self.flush();

        // Redessin seulement quand c'est utile : l'interface ne tourne pas en boucle.
        // Les changements d'état des appareils déclenchent eux-mêmes un redessin.
        if !self.overlay.is_empty() || !self.pending.is_empty() || self.scan_until.is_some() {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
        if let Some(t) = self.toasts.lock().unwrap().iter().map(|t| TOAST_TTL.saturating_sub(t.at.elapsed())).min() {
            ctx.request_repaint_after(t + Duration::from_millis(50));
        }
        // Horloge du calendrier, durées affichées.
        ctx.request_repaint_after(Duration::from_secs(30));
        if let Some(p) = &mut self.perf {
            p.ui_ms.push(started.elapsed().as_secs_f32() * 1000.0);
            if let Some(cpu) = frame.info().cpu_usage {
                p.cpu_ms.push(cpu * 1000.0);
            }
            let nr = ctx.cumulative_frame_nr();
            p.frames = (p.frames.0.min(nr), nr);
            for c in ctx.repaint_causes() {
                *p.causes.entry(format!("{}:{} {}", c.file.rsplit(['/', '\\']).next().unwrap_or(c.file), c.line, c.reason)).or_default() += 1;
            }
        }
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.ptz_script_input(raw_input);
        self.zoom_script_input(raw_input);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(p) = &self.perf {
            p.write();
        }
        if let Some(l) = &self.cams.ptz_log {
            l.write();
        }
        self.hub.shutdown();
    }
}

/// Notification du système (Windows, Linux) pour une alerte critique ; envoyée depuis un fil à
/// part (l'appel peut prendre un instant), sans jamais bloquer l'interface.
fn os_notify(e: &homesmart::hub::Alert) {
    let title = if e.device_name.is_empty() { "HomeSmart — alerte".to_string() } else { format!("HomeSmart — {}", e.device_name) };
    let body = e.message.clone();
    std::thread::spawn(move || {
        if let Err(err) = notify_rust::Notification::new().appname("HomeSmart").summary(&title).body(&body).show() {
            tracing::warn!("notification du système impossible : {err}");
        }
    });
}

#[cfg(test)]
mod notify_tests {
    /// Essai manuel : `cargo test --release --bin homesmart -- --ignored os_notification`.
    #[test]
    #[ignore]
    fn os_notification() {
        notify_rust::Notification::new().appname("HomeSmart").summary("HomeSmart — essai").body("Les alertes critiques s'afficheront ainsi.").show().unwrap();
    }
}
