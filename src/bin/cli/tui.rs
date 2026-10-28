//! `homesmart-cli tui` : HomeSmart en plein écran dans le terminal.
//!
//! Même moteur que l'application (hub) : connexions permanentes, garde-fous, programmes,
//! énergie et journal. Les actions faites ici sont journalisées avec la source « Ligne de
//! commande ». L'aperçu vidéo des caméras s'affiche en demi-blocs de couleur. Les aspirateurs
//! robots ont leur onglet (lancer, pause, base, signal sonore, puissance).

use anyhow::Result;
use homesmart::camera::{Frame, Quality};
use homesmart::hub::{CameraView, DeviceView, Hub, Level, Source, VacuumView};
use homesmart::vacuum::{Activity, Command};
use homesmart::kinds::{self, Kind};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Bar, BarChart, BarGroup, Block, BorderType, Cell, Paragraph, Row, Table, TableState, Tabs, Widget, Wrap};
use ratatui::{DefaultTerminal, Frame as TFrame};
use serde_json::{Map, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// Couleurs de l'application.
const AMBER: Color = Color::Rgb(0xf6, 0xb8, 0x3c);
const GREEN: Color = Color::Rgb(0x3d, 0xd6, 0x8c);
const BLUE: Color = Color::Rgb(0x5c, 0xa9, 0xff);
const RED: Color = Color::Rgb(0xff, 0x6b, 0x6b);
const VIOLET: Color = Color::Rgb(0xa7, 0x8b, 0xfa);
const TEXT: Color = Color::Rgb(0xe9, 0xec, 0xf1);
const WEAK: Color = Color::Rgb(0x8b, 0x95, 0xa7);
const FAINT: Color = Color::Rgb(0x5d, 0x66, 0x76);
const SEL_BG: Color = Color::Rgb(0x20, 0x27, 0x36);

const TABS: [&str; 5] = ["Appareils", "Caméras", "Énergie", "Journal", "Aspirateurs"];

/// Lance la TUI (sur la configuration, ou sur des appareils simulés avec `demo`).
pub async fn run(path: &Path, demo: bool) -> Result<()> {
    let path = if demo { homesmart::demo::prepare().await? } else { path.to_path_buf() };
    let instance = if demo { None } else { homesmart::instance::acquire() };
    let hub = Hub::new(path)?;
    if demo {
        homesmart::demo::seed_energy(&hub);
        hub.set_fast_detection(true);
    }
    hub.start(true);
    if demo {
        homesmart::demo::seed_schedules(&hub);
    }
    let mut app = Tui::new(hub.clone(), tokio::runtime::Handle::current());
    app.other_instance = !demo && instance.is_none();
    app.demo = demo;

    // Clavier lu dans un fil à part (lecture bloquante), transmis à la boucle principale.
    let (tx, mut keys) = tokio::sync::mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if tx.send(ev).is_err() {
                break;
            }
        }
    });

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, &mut keys).await;
    ratatui::restore();
    hub.shutdown();
    drop(instance);
    result
}

async fn event_loop(terminal: &mut DefaultTerminal, app: &mut Tui, keys: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Result<()> {
    let mut events = app.hub.events.subscribe();
    loop {
        terminal.draw(|f| app.draw(f))?;
        // Aperçu vidéo : ~12 images/s ; sinon on ne redessine qu'au besoin (et l'horloge).
        let wait = if app.preview_active() { Duration::from_millis(80) } else { Duration::from_millis(500) };
        tokio::select! {
            k = keys.recv() => match k {
                Some(ev) => app.on_event(ev),
                None => break,
            },
            r = events.recv() => {
                if let Err(tokio::sync::broadcast::error::RecvError::Closed) = r {
                    break;
                }
            }
            _ = tokio::time::sleep(wait) => {}
        }
        if app.quit {
            break;
        }
    }
    Ok(())
}

pub struct Tui {
    hub: Arc<Hub>,
    rt: tokio::runtime::Handle,
    tab: usize,
    dev: TableState,
    cam: TableState,
    energy: TableState,
    vac: TableState,
    journal_offset: usize,
    /// Allumage en attente de confirmation (garde-fou) : (id, nom).
    confirm: Option<(String, String)>,
    /// Résultats des actions lancées en arrière-plan : (texte, erreur, quand).
    messages: Arc<Mutex<Vec<(String, bool, Instant)>>>,
    preview: bool,
    ptz_busy: Arc<AtomicBool>,
    pub other_instance: bool,
    pub demo: bool,
    pub quit: bool,
}

impl Tui {
    pub fn new(hub: Arc<Hub>, rt: tokio::runtime::Handle) -> Self {
        let mut s = Tui {
            hub,
            rt,
            tab: 0,
            dev: TableState::default(),
            cam: TableState::default(),
            energy: TableState::default(),
            vac: TableState::default(),
            journal_offset: 0,
            confirm: None,
            messages: Arc::default(),
            preview: false,
            ptz_busy: Arc::default(),
            other_instance: false,
            demo: false,
            quit: false,
        };
        s.dev.select(Some(0));
        s.cam.select(Some(0));
        s.energy.select(Some(0));
        s.vac.select(Some(0));
        s
    }

    fn preview_active(&self) -> bool {
        self.tab == 1 && self.preview
    }

    fn say(&self, text: impl Into<String>, error: bool) {
        let mut m = self.messages.lock().unwrap();
        m.push((text.into(), error, Instant::now()));
        if m.len() > 20 {
            m.remove(0);
        }
    }

    /// Lance une action en arrière-plan ; son résultat s'affiche dans la barre d'état.
    fn spawn<F>(&self, ok: String, fut: F)
    where
        F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let msgs = self.messages.clone();
        self.rt.spawn(async move {
            let (text, err) = match fut.await {
                Ok(()) => (ok, false),
                Err(e) => (format!("{e:#}"), true),
            };
            msgs.lock().unwrap().push((text, err, Instant::now()));
        });
    }

    /// Appareils triés par pièce puis par nom (ordre de l'affichage).
    fn devices(&self) -> Vec<DeviceView> {
        let mut list = self.hub.list();
        list.sort_by(|a, b| (a.room.as_deref().unwrap_or("~"), a.name.to_lowercase()).cmp(&(b.room.as_deref().unwrap_or("~"), b.name.to_lowercase())));
        list
    }

    // ---- Clavier ------------------------------------------------------------------------

    pub fn on_event(&mut self, ev: Event) {
        let Event::Key(k) = ev else { return };
        // Windows envoie aussi les relâchements de touche : seuls les appuis comptent.
        if k.kind != KeyEventKind::Press {
            return;
        }
        if let Some((id, name)) = self.confirm.take() {
            match k.code {
                KeyCode::Char('o') | KeyCode::Char('O') | KeyCode::Char('y') | KeyCode::Enter => {
                    let hub = self.hub.clone();
                    self.spawn(format!("« {name} » allumé"), async move { hub.power(&id, true, None, Source::Cli).await.map(|_| ()) });
                }
                _ => self.say(format!("Allumage de « {name} » annulé"), false),
            }
            return;
        }
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Tab => self.tab = (self.tab + 1) % TABS.len(),
            KeyCode::BackTab => self.tab = (self.tab + TABS.len() - 1) % TABS.len(),
            KeyCode::F(n @ 1..=5) => self.tab = n as usize - 1,
            _ => match self.tab {
                0 => self.key_devices(k),
                1 => self.key_cameras(k),
                2 => self.key_energy(k),
                3 => self.key_journal(k),
                _ => self.key_vacuums(k),
            },
        }
    }

    fn key_devices(&mut self, k: KeyEvent) {
        let list = self.devices();
        let n = list.len();
        move_selection(&mut self.dev, n, k.code);
        let Some(d) = self.dev.selected().and_then(|i| list.get(i)).cloned() else { return };
        let kind = kinds::detect(d.kind.as_deref(), &d.dps);
        let hub = self.hub.clone();
        match k.code {
            KeyCode::Char(' ') | KeyCode::Enter => {
                if !d.online {
                    self.say(format!("« {} » est hors ligne", d.name), true);
                    return;
                }
                // Aspirateur robot : nettoyer, ou pause pendant un nettoyage.
                if let Some(v) = (kind == Kind::Vacuum).then(|| self.hub.vacuum(&d.id)).flatten() {
                    self.vac_toggle(&v);
                    return;
                }
                let on = is_on(&d, &kind);
                if !on && d.safety.enabled && d.safety.confirm_on {
                    self.confirm = Some((d.id.clone(), d.name.clone()));
                    return;
                }
                let label = format!("« {} » {}", d.name, if on { "éteint" } else { "allumé" });
                let id = d.id.clone();
                self.spawn(label, async move { hub.power(&id, !on, None, Source::Cli).await.map(|_| ()) });
            }
            KeyCode::Left | KeyCode::Right => {
                let up = k.code == KeyCode::Right;
                let (dp, lo, hi) = match kind {
                    Kind::LightV2 => ("22", 10.0, 1000.0),
                    Kind::LightV1 => ("3", 25.0, 255.0),
                    _ => return,
                };
                let cur = d.dps.get(dp).and_then(Value::as_f64).unwrap_or(lo);
                let step = (hi - lo) / 10.0;
                let v = (cur + if up { step } else { -step }).clamp(lo, hi).round();
                let pct = ((v - lo) / (hi - lo) * 100.0).round();
                let mut m = Map::new();
                m.insert(dp.into(), Value::from(v as i64));
                let id = d.id.clone();
                self.spawn(format!("« {} » : luminosité {pct} %", d.name), async move { hub.set_dps(&id, m, Source::Cli).await });
            }
            KeyCode::Char(c @ '1'..='9') => {
                // Multiprise : sortie n.
                if let Kind::Switch { gangs } = &kind {
                    let Some(dp) = gangs.get(c as usize - '1' as usize).cloned() else { return };
                    let on = d.dps.get(&dp).and_then(Value::as_bool).unwrap_or(false);
                    let mut m = Map::new();
                    m.insert(dp, Value::Bool(!on));
                    let id = d.id.clone();
                    self.spawn(format!("« {} » : sortie {c} {}", d.name, if on { "éteinte" } else { "allumée" }), async move { hub.set_dps(&id, m, Source::Cli).await });
                }
            }
            KeyCode::Char('m') => {
                // Minuterie : 15 → 30 → 60 → 120 min → aucune.
                let steps = [0u64, 15, 30, 60, 120];
                let cur = d.countdown_left.map(|s| s.div_ceil(60)).unwrap_or(0);
                let next = steps.iter().copied().find(|&m| m > cur).unwrap_or(0);
                let id = d.id.clone();
                let label = if next == 0 { format!("« {} » : minuterie annulée", d.name) } else { format!("« {} » : bascule dans {next} min", d.name) };
                self.spawn(label, async move { hub.set_countdown(&id, next * 60, Source::Cli).await });
            }
            KeyCode::Char('r') => {
                let _ = self.hub.refresh(&d.id);
                self.say(format!("« {} » : état redemandé", d.name), false);
            }
            KeyCode::Char('s') => {
                self.hub.trigger_scan();
                self.say("Recherche d'appareils sur le Wi-Fi…", false);
            }
            _ => {}
        }
    }

    fn key_cameras(&mut self, k: KeyEvent) {
        let cams = self.hub.cameras();
        if !k.modifiers.contains(KeyModifiers::SHIFT) {
            move_selection(&mut self.cam, cams.len(), k.code);
        }
        let Some(c) = self.cam.selected().and_then(|i| cams.get(i)).cloned() else { return };
        let hub = self.hub.clone();
        match k.code {
            KeyCode::Char('v') => {
                self.preview = !self.preview;
                self.say(if self.preview { "Aperçu en direct activé" } else { "Aperçu en direct arrêté" }, false);
            }
            KeyCode::Char('c') => {
                let id = c.id.clone();
                let msgs = self.messages.clone();
                self.rt.spawn(async move {
                    let (t, e) = match hub.camera_snapshot(&id, Source::Cli).await {
                        Ok(p) => (format!("Capture enregistrée : {}", p.display()), false),
                        Err(e) => (format!("Capture impossible : {e:#}"), true),
                    };
                    msgs.lock().unwrap().push((t, e, Instant::now()));
                });
            }
            KeyCode::Char('r') => {
                let _ = self.hub.camera_retry(&c.id, Source::Cli);
                self.say(format!("« {} » : nouvelle tentative de connexion", c.name), false);
            }
            // Orientation : Maj + flèches (un petit mouvement par appui).
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down if k.modifiers.contains(KeyModifiers::SHIFT) => {
                if c.ptz != Some(true) {
                    self.say(format!("« {} » n'est pas orientable", c.name), true);
                    return;
                }
                if self.ptz_busy.swap(true, Ordering::SeqCst) {
                    return;
                }
                let (pan, tilt) = match k.code {
                    KeyCode::Left => (-0.6, 0.0),
                    KeyCode::Right => (0.6, 0.0),
                    KeyCode::Up => (0.0, 0.6),
                    _ => (0.0, -0.6),
                };
                let (id, busy, msgs) = (c.id.clone(), self.ptz_busy.clone(), self.messages.clone());
                self.rt.spawn(async move {
                    let r = async {
                        hub.camera_move(&id, pan, tilt, Source::Cli).await?;
                        tokio::time::sleep(Duration::from_millis(350)).await;
                        hub.camera_stop(&id, Source::Cli).await
                    }
                    .await;
                    if let Err(e) = r {
                        msgs.lock().unwrap().push((format!("Orientation : {e:#}"), true, Instant::now()));
                    }
                    busy.store(false, Ordering::SeqCst);
                });
            }
            _ => {}
        }
    }

    /// Lance une commande d'aspirateur (origine : ligne de commande).
    fn vac_send(&self, v: &VacuumView, cmd: Command) {
        let hub = self.hub.clone();
        let label = format!("« {} » : {}", v.name, cmd.describe(&v.caps).to_lowercase());
        let id = v.id.clone();
        self.spawn(label, async move { hub.vacuum_command(&id, cmd, Source::Cli).await });
    }

    fn vac_toggle(&self, v: &VacuumView) {
        let cmd = if v.status.activity == Activity::Cleaning && v.caps.pause { Command::Pause } else { Command::Start };
        self.vac_send(v, cmd);
    }

    fn key_vacuums(&mut self, k: KeyEvent) {
        let list = self.hub.vacuums();
        move_selection(&mut self.vac, list.len(), k.code);
        let Some(v) = self.vac.selected().and_then(|i| list.get(i)).cloned() else { return };
        if matches!(k.code, KeyCode::Char(' ' | 'b' | 'x' | 't' | 'p') | KeyCode::Enter) && !v.online {
            self.say(format!("« {} » est hors ligne", v.name), true);
            return;
        }
        match k.code {
            KeyCode::Char(' ') | KeyCode::Enter => self.vac_toggle(&v),
            KeyCode::Char('b') => self.vac_send(&v, Command::Dock),
            KeyCode::Char('x') => self.vac_send(&v, Command::Stop),
            KeyCode::Char('t') => self.vac_send(&v, Command::Locate),
            KeyCode::Char('p') => {
                // Puissance suivante.
                let fans = &v.caps.fans;
                if fans.is_empty() {
                    self.say(format!("« {} » : puissance non réglable", v.name), true);
                    return;
                }
                let i = v.status.fan.as_deref().and_then(|f| fans.iter().position(|c| c.value == f)).map(|i| (i + 1) % fans.len()).unwrap_or(0);
                self.vac_send(&v, Command::Fan(fans[i].value.clone()));
            }
            KeyCode::Char('r') => {
                self.hub.vacuum_refresh(&v.id);
                self.say(format!("« {} » : état redemandé", v.name), false);
            }
            _ => {}
        }
    }

    fn key_energy(&mut self, k: KeyEvent) {
        let n = self.devices().iter().filter(|d| kinds::power_w(&d.dps, &d.schema).is_some()).count();
        move_selection(&mut self.energy, n, k.code);
    }

    fn key_journal(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Down => self.journal_offset += 1,
            KeyCode::Up => self.journal_offset = self.journal_offset.saturating_sub(1),
            KeyCode::PageDown => self.journal_offset += 15,
            KeyCode::PageUp => self.journal_offset = self.journal_offset.saturating_sub(15),
            KeyCode::Home => self.journal_offset = 0,
            _ => {}
        }
    }

    // ---- Affichage ----------------------------------------------------------------------

    pub fn draw(&mut self, f: &mut TFrame) {
        let area = f.area();
        let warn = self.other_instance as u16;
        let [header, warn_area, tabs, body, status] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(warn), Constraint::Length(1), Constraint::Min(5), Constraint::Length(2)]).areas(area);

        let devices = self.devices();
        let online = devices.iter().filter(|d| d.online).count();
        let cams = self.hub.cameras();
        let vacs = self.hub.vacuums();
        let mut head = vec![
            Span::styled(" HomeSmart ", Style::new().fg(Color::Black).bg(AMBER).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {online}/{} appareil(s) en ligne", devices.len()), Style::new().fg(TEXT)),
            Span::styled(format!(" · {} caméra(s)", cams.len()), Style::new().fg(WEAK)),
            Span::styled(format!(" · {} aspirateur(s)", vacs.len()), Style::new().fg(WEAK)),
            Span::styled("  ·  Wi-Fi local, sans compte ni cloud", Style::new().fg(FAINT)),
        ];
        if self.demo {
            head.push(Span::styled("  DÉMO ", Style::new().fg(Color::Black).bg(VIOLET)));
        }
        let clock = chrono::Local::now().format("%H:%M").to_string();
        f.render_widget(Paragraph::new(Line::from(head)), header);
        f.render_widget(Paragraph::new(Line::from(Span::styled(clock, Style::new().fg(WEAK)))).right_aligned(), header);
        if warn > 0 {
            f.render_widget(Paragraph::new(Span::styled(format!(" ⚠ {}", homesmart::instance::ALREADY_RUNNING), Style::new().fg(RED))), warn_area);
        }
        let titles: Vec<Line> = TABS.iter().enumerate().map(|(i, t)| Line::from(format!(" {} {t} ", i + 1))).collect();
        f.render_widget(
            Tabs::new(titles).select(self.tab).style(Style::new().fg(WEAK)).highlight_style(Style::new().fg(Color::Black).bg(AMBER).add_modifier(Modifier::BOLD)).divider(" "),
            tabs,
        );

        match self.tab {
            0 => self.draw_devices(f, body, &devices),
            1 => self.draw_cameras(f, body, &cams),
            2 => self.draw_energy(f, body, &devices),
            3 => self.draw_journal(f, body),
            _ => self.draw_vacuums(f, body, &vacs),
        }
        self.draw_status(f, status);
    }

    fn draw_status(&self, f: &mut TFrame, area: Rect) {
        let help = match (&self.confirm, self.tab) {
            (Some((_, name)), _) => format!("Allumer « {name} » (garde-fou) ?  o = oui · n = non"),
            (_, 0) => "↑↓ choisir · Espace allumer/éteindre · ←→ luminosité · 1-9 sorties · m minuterie · r état · s recherche".into(),
            (_, 1) => "↑↓ choisir · v aperçu · c capture · Maj+flèches orienter · r réessayer".into(),
            (_, 2) => "↑↓ choisir l'appareil (courbe du jour)".into(),
            (_, 3) => "↑↓ PgUp PgDn défiler · Début revenir en haut".into(),
            _ => "↑↓ choisir · Espace nettoyer/pause · b base · x arrêter · t signal sonore · p puissance · r état".into(),
        };
        let line2 = format!("{help}  ·  Tab/F1-F5 onglets · q quitter");
        let msg = self.messages.lock().unwrap().iter().rev().find(|m| m.2.elapsed() < Duration::from_secs(6)).cloned();
        let first = match msg {
            Some((t, err, _)) => Line::from(Span::styled(format!(" {t}"), Style::new().fg(if err { RED } else { GREEN }))),
            None => Line::from(""),
        };
        let style = if self.confirm.is_some() { Style::new().fg(AMBER).add_modifier(Modifier::BOLD) } else { Style::new().fg(FAINT) };
        f.render_widget(Paragraph::new(vec![first, Line::from(Span::styled(format!(" {line2}"), style))]), area);
    }

    fn draw_devices(&mut self, f: &mut TFrame, area: Rect, list: &[DeviceView]) {
        let [left, right] = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(area);
        let rows: Vec<Row> = list
            .iter()
            .map(|d| {
                let kind = kinds::detect(d.kind.as_deref(), &d.dps);
                let (state, color) = if !d.online {
                    ("✕ hors ligne", FAINT)
                } else if kind == Kind::Vacuum {
                    // Un robot ne s'allume pas : il nettoie ou attend.
                    if is_on(d, &kind) { ("● en marche", GREEN) } else { ("○ à l'arrêt", WEAK) }
                } else if is_on(d, &kind) {
                    ("● allumé", GREEN)
                } else {
                    ("○ éteint", WEAK)
                };
                Row::new(vec![
                    Cell::from(d.name.clone()).style(Style::new().fg(TEXT)),
                    Cell::from(d.room.clone().unwrap_or_default()).style(Style::new().fg(WEAK)),
                    Cell::from(kind.label()).style(Style::new().fg(WEAK)),
                    Cell::from(state).style(Style::new().fg(color)),
                    Cell::from(summary(d, &kind)).style(Style::new().fg(TEXT)),
                ])
            })
            .collect();
        if self.dev.selected().is_some_and(|i| i >= list.len()) {
            self.dev.select(Some(list.len().saturating_sub(1)));
        }
        let table = Table::new(rows, [Constraint::Fill(3), Constraint::Fill(2), Constraint::Length(11), Constraint::Length(13), Constraint::Fill(2)])
            .header(Row::new(["Appareil", "Pièce", "Type", "État", "Mesure"]).style(Style::new().fg(FAINT).add_modifier(Modifier::BOLD)))
            .row_highlight_style(Style::new().bg(SEL_BG).add_modifier(Modifier::BOLD))
            .highlight_symbol("▌")
            .block(panel("Appareils"));
        f.render_stateful_widget(table, left, &mut self.dev);

        let detail = match self.dev.selected().and_then(|i| list.get(i)) {
            Some(d) => device_detail(d),
            None => vec![Line::from(Span::styled("Aucun appareil. Ajoute-les depuis l'application ou `homesmart-cli add`.", Style::new().fg(WEAK)))],
        };
        f.render_widget(Paragraph::new(detail).wrap(Wrap { trim: false }).block(panel("Détail")), right);
    }

    fn draw_vacuums(&mut self, f: &mut TFrame, area: Rect, list: &[VacuumView]) {
        let [left, right] = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(area);
        let rows: Vec<Row> = list
            .iter()
            .map(|v| {
                let (state, color) = vac_state(v);
                Row::new(vec![
                    Cell::from(v.name.clone()).style(Style::new().fg(TEXT)),
                    Cell::from(v.room.clone().unwrap_or_default()).style(Style::new().fg(WEAK)),
                    Cell::from(v.family.label().split(" (").next().unwrap_or("").to_string()).style(Style::new().fg(WEAK)),
                    Cell::from(state).style(Style::new().fg(color)),
                    Cell::from(v.status.battery.map(|b| format!("{b} %")).unwrap_or_default()).style(Style::new().fg(TEXT)),
                ])
            })
            .collect();
        if self.vac.selected().is_some_and(|i| i >= list.len()) {
            self.vac.select(Some(list.len().saturating_sub(1)));
        }
        let table = Table::new(rows, [Constraint::Fill(3), Constraint::Fill(2), Constraint::Length(17), Constraint::Fill(3), Constraint::Length(9)])
            .header(Row::new(["Robot", "Pièce", "Famille", "État", "Batterie"]).style(Style::new().fg(FAINT).add_modifier(Modifier::BOLD)))
            .row_highlight_style(Style::new().bg(SEL_BG).add_modifier(Modifier::BOLD))
            .highlight_symbol("▌")
            .block(panel("Aspirateurs"));
        f.render_stateful_widget(table, left, &mut self.vac);
        let detail = match self.vac.selected().and_then(|i| list.get(i)) {
            Some(v) => vacuum_detail(v),
            None => vec![Line::from(Span::styled(
                "Aucun aspirateur. Robots Tuya : import Smart Life ; Valetudo / miIO : `homesmart-cli vac add-valetudo` ou `add-miio`, ou l'application.",
                Style::new().fg(WEAK),
            ))],
        };
        f.render_widget(Paragraph::new(detail).wrap(Wrap { trim: false }).block(panel("Détail")), right);
    }

    fn draw_cameras(&mut self, f: &mut TFrame, area: Rect, cams: &[CameraView]) {
        let [left, right] = Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(area);
        let rows: Vec<Row> = cams
            .iter()
            .map(|c| {
                let (state, color) = cam_state(c);
                Row::new(vec![
                    Cell::from(c.name.clone()).style(Style::new().fg(TEXT)),
                    Cell::from(c.room.clone().unwrap_or_default()).style(Style::new().fg(WEAK)),
                    Cell::from(state).style(Style::new().fg(color)),
                    Cell::from(c.host.clone()).style(Style::new().fg(WEAK)),
                ])
            })
            .collect();
        if self.cam.selected().is_some_and(|i| i >= cams.len()) {
            self.cam.select(Some(cams.len().saturating_sub(1)));
        }
        let [list_area, info_area] = Layout::vertical([Constraint::Length((cams.len() as u16 + 3).clamp(4, 10)), Constraint::Min(4)]).areas(left);
        let table = Table::new(rows, [Constraint::Fill(3), Constraint::Fill(2), Constraint::Length(12), Constraint::Length(15)])
            .header(Row::new(["Caméra", "Pièce", "État", "Adresse"]).style(Style::new().fg(FAINT).add_modifier(Modifier::BOLD)))
            .row_highlight_style(Style::new().bg(SEL_BG).add_modifier(Modifier::BOLD))
            .highlight_symbol("▌")
            .block(panel("Caméras"));
        f.render_stateful_widget(table, list_area, &mut self.cam);

        let Some(c) = self.cam.selected().and_then(|i| cams.get(i)).cloned() else {
            f.render_widget(Paragraph::new("Aucune caméra. Ajoute-en avec `homesmart-cli cam add` ou depuis l'application.").style(Style::new().fg(WEAK)).block(panel("Aperçu")), right);
            return;
        };
        f.render_widget(Paragraph::new(camera_info(&c)).wrap(Wrap { trim: false }).block(panel("Informations")), info_area);

        let block = panel(if self.preview { "Aperçu en direct (v pour arrêter)" } else { "Aperçu (v pour l'activer)" });
        let inner = block.inner(right);
        f.render_widget(block, right);
        if self.preview {
            self.hub.camera_want(&c.id, Quality::Sub);
            match self.hub.camera_frame(&c.id) {
                Some(frame) => f.render_widget(HalfBlocks(frame), inner),
                None => f.render_widget(Paragraph::new("Connexion à la caméra…").style(Style::new().fg(WEAK)), inner),
            }
        } else {
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(""),
                    Line::from(Span::styled("  Appuie sur v pour voir la caméra ici, en couleurs.", Style::new().fg(WEAK))),
                    Line::from(Span::styled("  (terminal en couleurs 24 bits conseillé : Windows Terminal, etc.)", Style::new().fg(FAINT))),
                ]),
                inner,
            );
        }
    }

    fn draw_energy(&mut self, f: &mut TFrame, area: Rect, devices: &[DeviceView]) {
        let metering: Vec<&DeviceView> = devices.iter().filter(|d| kinds::power_w(&d.dps, &d.schema).is_some()).collect();
        let today = chrono::Local::now().date_naive();
        let price = self.hub.prefs().price_kwh;
        let data: Vec<(f64, f64, Vec<f64>)> = metering.iter().map(|d| self.hub.energy(|e| (e.day_wh(&d.id, today), e.month_wh(&d.id, today), e.hourly(&d.id, today)))).collect();
        let [top, chart_area] = Layout::vertical([Constraint::Length((metering.len() as u16 + 4).clamp(5, 14)), Constraint::Min(6)]).areas(area);
        let money = |wh: f64| format!("{:.2} €", wh / 1000.0 * price).replace('.', ",");
        let kwh = |wh: f64, dec: usize| format!("{:.*}", dec, wh / 1000.0).replace('.', ",");
        let mut rows: Vec<Row> = metering
            .iter()
            .zip(&data)
            .map(|(d, (day, month, _))| {
                let w = kinds::power_w(&d.dps, &d.schema).unwrap_or(0.0);
                Row::new(vec![
                    Cell::from(d.name.clone()).style(Style::new().fg(TEXT)),
                    Cell::from(format!("{w:.0} W")).style(Style::new().fg(if w > 1.0 { AMBER } else { WEAK })),
                    Cell::from(format!("{} kWh", kwh(*day, 2))),
                    Cell::from(money(*day)).style(Style::new().fg(WEAK)),
                    Cell::from(format!("{} kWh", kwh(*month, 1))),
                    Cell::from(money(*month)).style(Style::new().fg(WEAK)),
                ])
            })
            .collect();
        let (td, tm): (f64, f64) = data.iter().fold((0.0, 0.0), |a, (d, m, _)| (a.0 + d, a.1 + m));
        rows.push(
            Row::new(vec![
                Cell::from("Total"),
                Cell::from(""),
                Cell::from(format!("{} kWh", kwh(td, 2))),
                Cell::from(money(td)),
                Cell::from(format!("{} kWh", kwh(tm, 1))),
                Cell::from(money(tm)),
            ])
            .style(Style::new().fg(TEXT).add_modifier(Modifier::BOLD)),
        );
        if self.energy.selected().is_some_and(|i| i >= metering.len()) {
            self.energy.select(Some(metering.len().saturating_sub(1)));
        }
        let table = Table::new(rows, [Constraint::Fill(3), Constraint::Length(11), Constraint::Length(12), Constraint::Length(9), Constraint::Length(12), Constraint::Length(10)])
            .header(Row::new(["Prise", "Maintenant", "Aujourd'hui", "Coût", "Ce mois-ci", "Coût"]).style(Style::new().fg(FAINT).add_modifier(Modifier::BOLD)))
            .row_highlight_style(Style::new().bg(SEL_BG))
            .highlight_symbol("▌")
            .block(panel(&format!("Consommation · {} €/kWh", format!("{price:.4}").replace('.', ","))));
        f.render_stateful_widget(table, top, &mut self.energy);

        let Some((d, (_, _, hourly))) = self.energy.selected().and_then(|i| metering.get(i).zip(data.get(i))) else {
            f.render_widget(Paragraph::new("Aucune prise avec mesure de consommation.").style(Style::new().fg(WEAK)).block(panel("Aujourd'hui")), chart_area);
            return;
        };
        let bars: Vec<Bar> = hourly
            .iter()
            .enumerate()
            .map(|(h, wh)| Bar::new(wh.round().max(0.0) as u64).label(Line::from(if h % 3 == 0 { format!("{h}h") } else { String::new() })).text_value(String::new()))
            .collect();
        let inner_w = chart_area.width.saturating_sub(2);
        let bar_w = (inner_w / 24).saturating_sub(1).clamp(1, 4);
        let chart = BarChart::default()
            .data(BarGroup::default().bars(&bars))
            .bar_width(bar_w)
            .bar_gap(1)
            .bar_style(Style::new().fg(AMBER))
            .label_style(Style::new().fg(FAINT))
            .block(panel(&format!("{} · aujourd'hui, Wh par heure", d.name)));
        f.render_widget(chart, chart_area);
    }

    fn draw_journal(&mut self, f: &mut TFrame, area: Rect) {
        let entries = self.hub.alerts(1000);
        let visible = area.height.saturating_sub(2) as usize;
        self.journal_offset = self.journal_offset.min(entries.len().saturating_sub(visible.max(1)));
        let lines: Vec<Line> = entries
            .iter()
            .skip(self.journal_offset)
            .take(visible)
            .map(|e| {
                let color = match e.level {
                    Level::Info => TEXT,
                    Level::Warning => AMBER,
                    _ => RED,
                };
                let who = if e.device_name.is_empty() { String::new() } else { format!("{} : ", e.device_name) };
                Line::from(vec![
                    Span::styled(e.at.format("%d/%m %H:%M:%S  ").to_string(), Style::new().fg(FAINT)),
                    Span::styled(format!("{:<17} ", e.source.label()), Style::new().fg(BLUE)),
                    Span::styled(who, Style::new().fg(WEAK)),
                    Span::styled(e.message.clone(), Style::new().fg(color)),
                ])
            })
            .collect();
        let title = format!("Journal · {} entrée(s), la plus récente en haut", entries.len());
        f.render_widget(Paragraph::new(lines).block(panel(&title)), area);
    }
}

// ---- Aides ------------------------------------------------------------------------------

fn panel(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Rgb(0x25, 0x2d, 0x3c)))
        .title(Span::styled(format!(" {title} "), Style::new().fg(TEXT).add_modifier(Modifier::BOLD)))
}

fn move_selection(state: &mut TableState, n: usize, code: KeyCode) {
    if n == 0 {
        return;
    }
    let i = state.selected().unwrap_or(0).min(n - 1);
    let next = match code {
        KeyCode::Down => (i + 1).min(n - 1),
        KeyCode::Up => i.saturating_sub(1),
        KeyCode::Home => 0,
        KeyCode::End => n - 1,
        _ => i,
    };
    state.select(Some(next));
}

fn is_on(d: &DeviceView, kind: &Kind) -> bool {
    let flag = |dp: &str| d.dps.get(dp).and_then(Value::as_bool).unwrap_or(false);
    match kind {
        Kind::Vacuum => homesmart::vacuum::tuya::status(&d.dps, &d.schema).activity.is_active(),
        Kind::Switch { gangs } => gangs.iter().any(|g| flag(g)),
        Kind::Cover => d.dps.get("3").or_else(|| d.dps.get("2")).and_then(Value::as_f64).unwrap_or(0.0) > 0.0,
        _ => kind.switch_dp(&d.dps).is_some_and(|dp| flag(&dp)),
    }
}

/// Mesure ou réglage principal, en une ligne.
fn summary(d: &DeviceView, kind: &Kind) -> String {
    if let Some(w) = kinds::power_w(&d.dps, &d.schema) {
        return format!("{w:.0} W");
    }
    match kind {
        Kind::LightV2 => d.dps.get("22").and_then(Value::as_f64).map(|v| format!("{:.0} %", (v - 10.0) / 990.0 * 100.0)).unwrap_or_default(),
        Kind::LightV1 => d.dps.get("3").and_then(Value::as_f64).map(|v| format!("{:.0} %", (v - 25.0) / 230.0 * 100.0)).unwrap_or_default(),
        Kind::Switch { gangs } => format!("{}/{} sorties", gangs.iter().filter(|g| d.dps.get(g.as_str()).and_then(Value::as_bool).unwrap_or(false)).count(), gangs.len()),
        Kind::Cover => d.dps.get("3").or_else(|| d.dps.get("2")).and_then(Value::as_f64).map(|v| format!("ouvert à {v:.0} %")).unwrap_or_default(),
        Kind::Vacuum => homesmart::vacuum::tuya::status(&d.dps, &d.schema).summary(),
        _ => String::new(),
    }
}

fn vac_state(v: &VacuumView) -> (String, Color) {
    if !v.online {
        return ("✕ hors ligne".into(), FAINT);
    }
    if let Some(e) = v.status.errors.first() {
        return (format!("⚠ {}", e.label), RED);
    }
    let color = match v.status.activity {
        Activity::Cleaning | Activity::Returning => GREEN,
        Activity::Paused => AMBER,
        _ => WEAK,
    };
    (v.status.activity.label().to_string(), color)
}

fn vacuum_detail(v: &VacuumView) -> Vec<Line<'static>> {
    let k = |s: &str| Span::styled(format!("{s:<18} "), Style::new().fg(WEAK));
    let t = |s: String| Span::styled(s, Style::new().fg(TEXT));
    let st = &v.status;
    let mut out = vec![
        Line::from(Span::styled(v.name.clone(), Style::new().fg(TEXT).add_modifier(Modifier::BOLD))),
        Line::from(Span::styled(format!("{} · {}", v.family.label(), v.room.clone().unwrap_or_else(|| "sans pièce".into())), Style::new().fg(WEAK))),
        Line::from(""),
    ];
    let conn = if v.online {
        format!("en ligne{}{}", v.host.as_deref().map(|h| format!(" · {h}")).unwrap_or_default(), v.model.as_deref().map(|m| format!(" · {m}")).unwrap_or_default())
    } else {
        format!("hors ligne{}", v.error.as_deref().map(|e| format!(" ({e})")).unwrap_or_default())
    };
    out.push(Line::from(vec![k("Connexion"), Span::styled(conn, Style::new().fg(if v.online { GREEN } else { RED }))]));
    out.push(Line::from(vec![k("Activité"), t(st.activity.label().to_string())]));
    if let Some(b) = st.battery {
        out.push(Line::from(vec![k("Batterie"), t(format!("{b} %{}", if st.charging { " (en charge)" } else { "" }))]));
    }
    if let Some(f) = &st.fan {
        out.push(Line::from(vec![k("Puissance"), t(v.caps.fan_label(f))]));
    }
    if let Some(m) = &st.mode {
        out.push(Line::from(vec![k("Mode"), t(v.caps.mode_label(m))]));
    }
    if let Some(w) = st.water.as_ref().filter(|_| !v.caps.waters.is_empty()) {
        out.push(Line::from(vec![k("Eau"), t(v.caps.water_label(w))]));
    }
    if let Some((a, m)) = st.area_m2.zip(st.clean_min).filter(|(a, m)| *a > 0.0 || *m > 0) {
        out.push(Line::from(vec![k("Nettoyage"), t(format!("{} m² en {}", kinds::fr(a, 1), kinds::fmt_secs(m as u64 * 60)))]));
    }
    for e in &st.errors {
        out.push(Line::from(vec![k("Erreur"), Span::styled(e.sentence(), Style::new().fg(RED))]));
    }
    if !st.consumables.is_empty() {
        out.push(Line::from(""));
        out.push(Line::from(Span::styled("Usure", Style::new().fg(FAINT).add_modifier(Modifier::BOLD))));
        for c in &st.consumables {
            let left = match (c.percent, c.hours) {
                (Some(p), _) => format!("{} %", kinds::fr(p, 0)),
                (None, Some(h)) => format!("encore {} h", kinds::fr(h, 0)),
                _ => "—".into(),
            };
            out.push(Line::from(vec![k(&c.label), t(left)]));
        }
    }
    out
}

fn device_detail(d: &DeviceView) -> Vec<Line<'static>> {
    let kind = kinds::detect(d.kind.as_deref(), &d.dps);
    let k = |s: &str| Span::styled(format!("{s:<14}"), Style::new().fg(WEAK));
    let v = |s: String| Span::styled(s, Style::new().fg(TEXT));
    let mut out = vec![
        Line::from(Span::styled(d.name.clone(), Style::new().fg(TEXT).add_modifier(Modifier::BOLD))),
        Line::from(Span::styled(format!("{} · {}", kind.label(), d.room.clone().unwrap_or_else(|| "sans pièce".into())), Style::new().fg(WEAK))),
        Line::from(""),
    ];
    let conn = if d.online {
        format!("en ligne{}{}", d.ip.as_deref().map(|ip| format!(" · {ip}")).unwrap_or_default(), d.latency_ms.map(|l| format!(" · {l} ms")).unwrap_or_default())
    } else {
        format!("hors ligne{}", d.error.as_deref().map(|e| format!(" ({e})")).unwrap_or_default())
    };
    out.push(Line::from(vec![k("Connexion"), Span::styled(conn, Style::new().fg(if d.online { GREEN } else { RED }))]));
    out.push(Line::from(vec![k("État"), v(if is_on(d, &kind) { "allumé".into() } else { "éteint".into() })]));
    if let Some(s) = d.countdown_left.filter(|s| *s > 0) {
        out.push(Line::from(vec![k("Minuterie"), v(format!("bascule dans {}", kinds::fmt_secs(s)))]));
    }
    if let Some(s) = d.on_for_s {
        out.push(Line::from(vec![k("Allumé depuis"), v(kinds::fmt_secs(s))]));
    }
    if d.safety.enabled {
        let mut g = Vec::new();
        if d.safety.confirm_on {
            g.push("confirmation".to_string());
        }
        if let Some(m) = d.safety.max_on_minutes {
            g.push(format!("arrêt après {m} min"));
        }
        if let Some(w) = d.safety.max_power_w {
            g.push(format!("max {w:.0} W"));
        }
        out.push(Line::from(vec![k("Garde-fous"), Span::styled(g.join(", "), Style::new().fg(AMBER))]));
    }
    for fault in &d.faults {
        out.push(Line::from(vec![k("Défaut"), Span::styled(fault.clone(), Style::new().fg(RED))]));
    }
    out.push(Line::from(""));
    out.push(Line::from(Span::styled("Points de données", Style::new().fg(FAINT).add_modifier(Modifier::BOLD))));
    let mut keys: Vec<&String> = d.dps.keys().collect();
    keys.sort_by_key(|k| k.parse::<u32>().unwrap_or(u32::MAX));
    for dp in keys {
        let code = d.schema.get(dp).and_then(|s| s.get("code")).and_then(Value::as_str).map(kinds::label_for_code).unwrap_or_else(|| format!("DP {dp}"));
        out.push(Line::from(vec![k(&code), v(kinds::format_value(dp, &d.dps[dp], &d.schema))]));
    }
    out
}

fn cam_state(c: &CameraView) -> (&'static str, Color) {
    if c.error.is_some() {
        ("⚠ erreur", RED)
    } else if c.streaming.is_some() {
        ("● en direct", GREEN)
    } else {
        match c.online {
            Some(true) => ("○ joignable", WEAK),
            Some(false) => ("✕ injoignable", RED),
            None => ("… vérification", FAINT),
        }
    }
}

fn camera_info(c: &CameraView) -> Vec<Line<'static>> {
    let k = |s: &str| Span::styled(format!("{s:<12}"), Style::new().fg(WEAK));
    let v = |s: String| Span::styled(s, Style::new().fg(TEXT));
    let mut out = vec![Line::from(Span::styled(c.name.clone(), Style::new().fg(TEXT).add_modifier(Modifier::BOLD)))];
    let model = c.info.as_ref().map(|i| i.model.clone()).filter(|m| !m.is_empty()).or(c.model.clone()).unwrap_or_default();
    out.push(Line::from(vec![k("Modèle"), v(format!("{} {model}", c.brand.label()))]));
    if let Some(i) = &c.info {
        if !i.firmware.is_empty() {
            out.push(Line::from(vec![k("Firmware"), v(i.firmware.clone())]));
        }
    }
    out.push(Line::from(vec![k("Adresse"), v(format!("{} (RTSP {}, ONVIF {})", c.host, c.rtsp_port, c.onvif_port))]));
    if let Some(s) = &c.stats {
        out.push(Line::from(vec![k("Flux"), v(s.summary())]));
    }
    let ptz = match c.ptz {
        Some(true) => "oui (Maj + flèches)",
        Some(false) => "non",
        None => "…",
    };
    out.push(Line::from(vec![k("Orientable"), v(ptz.into())]));
    if let Some(e) = &c.error {
        out.push(Line::from(vec![k("Erreur"), Span::styled(e.clone(), Style::new().fg(RED))]));
    }
    if let Some(n) = &c.notice {
        out.push(Line::from(Span::styled(n.clone(), Style::new().fg(AMBER))));
    }
    out
}

/// Image en demi-blocs « ▀ » : chaque caractère porte deux pixels (haut en encre, bas en fond).
pub struct HalfBlocks(pub Arc<Frame>);

impl Widget for HalfBlocks {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let f = &self.0;
        if area.width == 0 || area.height == 0 || f.width == 0 || f.height == 0 {
            return;
        }
        // Grille de pixels : largeur × (hauteur × 2), au format de l'image.
        let (gw, gh) = (area.width as f32, area.height as f32 * 2.0);
        let aspect = f.width as f32 / f.height as f32;
        let (w, h) = if gw / gh > aspect { ((gh * aspect).floor(), gh) } else { (gw, (gw / aspect).floor()) };
        let (w, h) = (w.max(1.0) as u16, (h.max(2.0) as u16) & !1);
        let x0 = area.x + (area.width - w.min(area.width)) / 2;
        let y0 = area.y + (area.height - (h / 2).min(area.height)) / 2;
        let px = |x: u16, y: u16| {
            let sx = ((x as f32 + 0.5) / w as f32 * f.width as f32) as usize;
            let sy = ((y as f32 + 0.5) / h as f32 * f.height as f32) as usize;
            let o = (sy.min(f.height - 1) * f.width + sx.min(f.width - 1)) * 4;
            Color::Rgb(f.rgba[o], f.rgba[o + 1], f.rgba[o + 2])
        };
        for cy in 0..(h / 2) {
            for cx in 0..w {
                if let Some(cell) = buf.cell_mut((x0 + cx, y0 + cy)) {
                    cell.set_symbol("▀").set_fg(px(cx, cy * 2)).set_bg(px(cx, cy * 2 + 1));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn text(buf: &Buffer) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn half_blocks_two_pixels_per_cell() {
        // Image 2×2 : haut rouge, bas bleu.
        let mut rgba = Vec::new();
        for c in [[255u8, 0, 0, 255], [255, 0, 0, 255], [0, 0, 255, 255], [0, 0, 255, 255]] {
            rgba.extend_from_slice(&c);
        }
        let frame = Arc::new(Frame { seq: 1, width: 2, height: 2, rgba });
        let mut buf = Buffer::empty(Rect::new(0, 0, 2, 1));
        HalfBlocks(frame).render(Rect::new(0, 0, 2, 1), &mut buf);
        let c = &buf[(0, 0)];
        assert_eq!(c.symbol(), "▀");
        assert_eq!((c.fg, c.bg), (Color::Rgb(255, 0, 0), Color::Rgb(0, 0, 255)));
    }

    /// Aperçu vidéo dans le terminal, converti en image PNG pour le vérifier à l'œil
    /// (`HOMESMART_TUI_PNG=chemin.png cargo test -- --ignored preview_png`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    #[ignore]
    async fn preview_png() {
        let out = std::env::var("HOMESMART_TUI_PNG").unwrap_or_else(|_| "tui-preview.png".into());
        let dir = std::env::temp_dir().join(format!("homesmart-tui-png-{}", std::process::id()));
        let hub = Hub::new(homesmart::demo::prepare_in(dir.clone()).await.unwrap()).unwrap();
        hub.start(false);
        let mut app = Tui::new(hub.clone(), tokio::runtime::Handle::current());
        (app.tab, app.preview) = (1, true);
        let mut term = Terminal::new(TestBackend::new(140, 40)).unwrap();
        let t0 = Instant::now();
        while hub.cameras().first().and_then(|c| hub.camera_frame(&c.id)).is_none() {
            term.draw(|f| app.draw(f)).unwrap();
            assert!(t0.elapsed() < Duration::from_secs(15));
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        term.draw(|f| app.draw(f)).unwrap();
        // Chaque cellule devient 6×12 pixels : moitié haute = encre, moitié basse = fond.
        let buf = term.backend().buffer().clone();
        let (cw, ch) = (6usize, 12usize);
        let (w, h) = (buf.area.width as usize * cw, buf.area.height as usize * ch);
        let mut rgba = vec![0u8; w * h * 4];
        let rgb = |c: Color, dflt: [u8; 3]| match c {
            Color::Rgb(r, g, b) => [r, g, b],
            _ => dflt,
        };
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                let half = cell.symbol() == "▀";
                let top = if half { rgb(cell.fg, [0, 0, 0]) } else if cell.symbol().trim().is_empty() { rgb(cell.bg, [11, 14, 21]) } else { rgb(cell.fg, [200, 200, 200]) };
                let bottom = if half { rgb(cell.bg, [0, 0, 0]) } else { top };
                for py in 0..ch {
                    let c = if py < ch / 2 { top } else { bottom };
                    for px in 0..cw {
                        let o = ((y as usize * ch + py) * w + x as usize * cw + px) * 4;
                        rgba[o..o + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
                    }
                }
            }
        }
        homesmart::camera::save_png(&Frame { seq: 0, width: w, height: h, rgba }, std::path::Path::new(&out)).unwrap();
        hub.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Les cinq onglets s'affichent avec les appareils, caméras et aspirateurs simulés.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn renders_all_tabs() {
        let dir = std::env::temp_dir().join(format!("homesmart-tui-test-{}", std::process::id()));
        let path = homesmart::demo::prepare_in(dir.clone()).await.unwrap();
        let hub = Hub::new(path).unwrap();
        homesmart::demo::seed_energy(&hub);
        hub.start(false);
        // Laisser les appareils simulés répondre.
        let t0 = Instant::now();
        while hub.list().iter().filter(|d| d.online).count() < 3 && t0.elapsed() < Duration::from_secs(10) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let mut app = Tui::new(hub.clone(), tokio::runtime::Handle::current());
        app.demo = true;
        let mut term = Terminal::new(TestBackend::new(140, 36)).unwrap();
        let mut screens = Vec::new();
        while hub.vacuums().iter().filter(|v| v.online).count() < 2 && t0.elapsed() < Duration::from_secs(15) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        for tab in 0..5 {
            app.tab = tab;
            term.draw(|f| app.draw(f)).unwrap();
            screens.push(text(term.backend().buffer()));
        }
        if std::env::var_os("HOMESMART_TUI_PRINT").is_some() {
            for s in &screens {
                println!("{s}");
            }
        }
        assert!(screens[0].contains("Lampe du salon") && screens[0].contains("Chauffage"), "{}", screens[0]);
        assert!(screens[1].contains("Salon") && screens[1].contains("Entrée"), "{}", screens[1]);
        assert!(screens[2].contains("Aujourd'hui") && screens[2].contains("Total"), "{}", screens[2]);
        assert!(screens[3].contains("Journal"), "{}", screens[3]);
        assert!(screens[4].contains("Robot aspirateur") && screens[4].contains("Aspirateur étage") && screens[4].contains("Sur sa base"), "{}", screens[4]);
        // Nettoyer depuis l'onglet Aspirateurs : origine « Ligne de commande ».
        app.tab = 4;
        let vacs = hub.vacuums();
        app.vac.select(Some(vacs.iter().position(|v| v.name == "Robot aspirateur").unwrap()));
        app.on_event(Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));
        let t1 = Instant::now();
        while !hub.alerts(30).iter().any(|a| a.source == Source::Cli && a.device_name == "Robot aspirateur" && a.message == "Nettoyage lancé") {
            assert!(t1.elapsed() < Duration::from_secs(8), "nettoyage non lancé : {:#?}", hub.alerts(10));
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Allumer depuis la TUI : l'action part avec la source « Ligne de commande ».
        app.tab = 0;
        let list = app.devices();
        let i = list.iter().position(|d| d.name == "Prise TV").unwrap();
        app.dev.select(Some(i));
        let was = is_on(&list[i], &kinds::detect(list[i].kind.as_deref(), &list[i].dps));
        app.on_event(Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));
        let t0 = Instant::now();
        loop {
            let d = hub.view(&list[i].id).unwrap();
            if is_on(&d, &kinds::detect(d.kind.as_deref(), &d.dps)) != was {
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "la prise n'a pas basculé");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(hub.alerts(20).iter().any(|a| a.source == Source::Cli && a.device_name == "Prise TV"));
        hub.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
