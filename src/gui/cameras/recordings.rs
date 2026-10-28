//! Vue « Enregistrements » d'une caméra : sa carte SD, jour par jour, en lecture seule.
//!
//! Frise de la journée (molette : zoom, glisser : se déplacer, clic : relire à partir de là),
//! liste des séquences avec vignettes, relecture continue d'une séquence à l'autre (la caméra
//! saute les trous), pause, séquence précédente / suivante, export d'une séquence en .mp4.
//! La relecture s'arrête dès qu'on quitte la vue.

use super::super::icons::{self, Icon};
use super::super::{theme, widgets, App};
use super::{color_image, fit, open_folder};
use chrono::{DateTime, Datelike, Local, NaiveDate, Timelike, Utc};
use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Id, Key, LayerId, Layout, Margin, Order, Rect, RichText, ScrollArea, Sense, Stroke,
    StrokeKind, Ui,
};
use homesmart::camera::playback::{self, Clip, DayFootage};
use homesmart::camera::Frame;
use homesmart::hub::{CameraView, PlaybackView, Source};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Taille des vignettes (16:9).
const THUMB: (u32, u32) = (192, 108);
/// La journée en cours est réanalysée au-delà de ce délai.
const TODAY_STALE: Duration = Duration::from_secs(120);
/// Zoom maximal de la frise : 5 minutes à l'écran.
const MIN_SPAN: f64 = 300.0;

#[derive(Default)]
pub(in crate::gui) struct RecUi {
    /// Caméra affichée (tout repart de zéro quand elle change).
    cam: String,
    pub date: Option<NaiveDate>,
    days: Arc<Mutex<HashMap<NaiveDate, DayScan>>>,
    presence: Arc<Mutex<Presence>>,
    /// Dernière séquence relue (export, navigation, surbrillance).
    current: Option<Clip>,
    /// Plage visible de la frise, en secondes depuis le début de la journée.
    view: Option<(f64, f64)>,
    thumbs: Arc<Mutex<Thumbs>>,
    thumb_tex: HashMap<i64, egui::TextureHandle>,
    /// Dernier affichage de la vue (la relecture s'arrête quand on la quitte).
    shown_at: Option<Instant>,
    /// Séquence déjà amenée à l'écran dans la liste (suivi de la lecture).
    scrolled_to: Option<i64>,
    /// Relire la dernière séquence dès que la journée est analysée (captures d'écran), avec le son.
    pub autoplay: bool,
    pub autosound: bool,
    /// Sortie audio quand on écoute le son de la relecture (garde l'écoute d'une séquence à l'autre).
    pub(in crate::gui) player: Option<homesmart::camera::audio::Player>,
    /// Clic simple sur l'image zoomée, en attente : lecture / pause, sauf s'il devient un double-clic.
    click_at: Option<Instant>,
}

#[derive(Default)]
struct DayScan {
    loading: bool,
    data: Option<DayFootage>,
    error: Option<String>,
    at: Option<Instant>,
}

#[derive(Default)]
struct Presence {
    loading: bool,
    /// Jour → (vidéo présente ?, moment du sondage).
    days: HashMap<NaiveDate, (Option<bool>, Instant)>,
}

#[derive(Default)]
struct Thumbs {
    busy: bool,
    /// Vignettes visibles sans image, de haut en bas (redemandées à chaque affichage).
    wanted: Vec<DateTime<Utc>>,
    /// Début de séquence → image (`None` : échec, on ne réessaie pas).
    frames: HashMap<i64, Option<Arc<Frame>>>,
}

fn local(t: DateTime<Utc>) -> DateTime<Local> {
    t.with_timezone(&Local)
}

fn secs(n: f64) -> chrono::Duration {
    chrono::Duration::milliseconds((n * 1000.0) as i64)
}

/// Secondes écoulées depuis le début de la journée.
fn offset(d0: DateTime<Utc>, t: DateTime<Utc>) -> f64 {
    (t - d0).num_milliseconds() as f64 / 1000.0
}

/// Durée d'une séquence sur la vignette : « 0:44 », « 12:05 ».
fn badge(s: i64) -> String {
    format!("{}:{:02}", s / 60, s % 60)
}

/// Durée en toutes lettres : « 44 s », « 1 min 05 », « 2 h 03 ».
fn human(s: i64) -> String {
    match (s / 3600, (s % 3600) / 60, s % 60) {
        (0, 0, s) => format!("{s} s"),
        (0, m, s) => format!("{m} min {s:02}"),
        (h, m, _) => format!("{h} h {m:02}"),
    }
}

const WEEKDAYS: [&str; 7] = ["lundi", "mardi", "mercredi", "jeudi", "vendredi", "samedi", "dimanche"];
const MONTHS: [&str; 12] = ["janvier", "février", "mars", "avril", "mai", "juin", "juillet", "août", "septembre", "octobre", "novembre", "décembre"];

/// « Aujourd'hui », « Hier », « sam. 17 ».
fn day_label(d: NaiveDate, today: NaiveDate) -> String {
    if d == today {
        "Aujourd'hui".into()
    } else if today.pred_opt() == Some(d) {
        "Hier".into()
    } else {
        format!("{}. {}", &WEEKDAYS[d.weekday().num_days_from_monday() as usize][..3], d.day())
    }
}

/// « mardi 20 octobre ».
fn long_date(d: NaiveDate) -> String {
    format!("{} {} {}", WEEKDAYS[d.weekday().num_days_from_monday() as usize], d.day(), MONTHS[d.month0() as usize])
}

/// Séquence de la liste qui correspond à `clip` (les bornes s'affinent d'une analyse à l'autre).
fn find_clip(clips: &[Clip], clip: Clip) -> Option<usize> {
    let mid = clip.start + (clip.end - clip.start) / 2;
    clips.iter().position(|c| *c == clip).or_else(|| clips.iter().position(|c| c.contains(mid) || clip.contains(c.start)))
}

/// Zoom de la frise autour de `anchor` (secondes), borné à la journée.
fn zoom_view(v: (f64, f64), factor: f64, anchor: f64, total: f64) -> (f64, f64) {
    let span = ((v.1 - v.0) * factor).clamp(MIN_SPAN, total);
    let k = if v.1 > v.0 { (anchor - v.0) / (v.1 - v.0) } else { 0.5 };
    clamp_view(anchor - span * k, span, total)
}

fn clamp_view(start: f64, span: f64, total: f64) -> (f64, f64) {
    let span = span.clamp(MIN_SPAN.min(total), total);
    let start = start.clamp(0.0, total - span);
    (start, start + span)
}

/// Bouton rond à icône ; `primary` : plein, à la couleur d'accent.
fn round_button(ui: &mut Ui, icon: Icon, size: f32, primary: bool, enabled: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), if enabled { Sense::click() } else { Sense::hover() });
    let h = ui.ctx().animate_bool_with_time(resp.id, enabled && resp.hovered(), 0.12);
    let p = ui.painter();
    let (fill, fg) = if primary {
        (theme::mix(theme::accent(), Color32::WHITE, h * 0.15), theme::bg())
    } else {
        (theme::mix(theme::raised(), theme::raised_hi(), h), theme::mix(theme::muted(), theme::text(), h))
    };
    let alpha = if enabled { 1.0 } else { 0.4 };
    p.circle_filled(rect.center(), size / 2.0, theme::alpha(fill, alpha));
    let k = if primary { 0.42 } else { 0.44 };
    icons::paint(p, Rect::from_center_size(rect.center(), vec2(size * k, size * k)), icon, theme::alpha(fg, alpha));
    if enabled && resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    resp
}

/// Pastille de jour ; point bleu s'il y a de la vidéo, texte pâle s'il n'y en a pas.
fn day_chip(ui: &mut Ui, label: &str, selected: bool, has: Option<bool>) -> egui::Response {
    let font = if selected { theme::bold(12.5) } else { FontId::proportional(12.5) };
    let g = ui.painter().layout_no_wrap(label.to_string(), font.clone(), theme::text());
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 26.0, 30.0), Sense::click());
    let h = ui.ctx().animate_bool_with_time(resp.id, resp.hovered() && !selected, 0.12);
    let p = ui.painter();
    let fill = if selected { theme::accent() } else { theme::mix(theme::raised(), theme::raised_hi(), h) };
    p.rect_filled(rect, CornerRadius::same(10), fill);
    let color = if selected {
        theme::bg()
    } else if has == Some(false) {
        theme::faint()
    } else {
        theme::mix(theme::muted(), theme::text(), h.max(0.6))
    };
    p.text(rect.center() - vec2(0.0, 1.0), Align2::CENTER_CENTER, label, font, color);
    if has == Some(true) {
        p.circle_filled(pos2(rect.center().x, rect.bottom() - 5.0), 2.2, if selected { theme::bg() } else { theme::accent() });
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    resp
}

impl App {
    /// La vue entière : jours, image, commandes de lecture, frise, liste des séquences.
    pub(in crate::gui) fn recordings_view(&mut self, ui: &mut Ui, c: &CameraView) {
        if self.cams.rec.cam != c.id {
            // Autre caméra : le son de la précédente se coupe avec elle.
            if self.cams.rec.player.is_some() {
                let _ = self.hub.set_playback_audio(&self.cams.rec.cam, None, Source::User);
            }
            let (date, autoplay, autosound) = (self.cams.rec.date, self.cams.rec.autoplay, self.cams.rec.autosound);
            self.cams.rec = RecUi { cam: c.id.clone(), date, autoplay, autosound, ..RecUi::default() };
        }
        self.cams.rec.shown_at = Some(Instant::now());
        let today = Local::now().date_naive();
        let date = *self.cams.rec.date.get_or_insert(today);
        self.rec_ensure_day(&c.id, date, today, false);
        self.rec_ensure_presence(&c.id, date, today);
        let (day, loading, error) = {
            let days = self.cams.rec.days.lock().unwrap();
            let s = days.get(&date);
            (s.and_then(|s| s.data.clone()), s.is_some_and(|s| s.loading), s.and_then(|s| s.error.clone()))
        };
        if let Some(d) = day.as_ref().filter(|d| self.cams.rec.autoplay && d.complete && !d.clips.is_empty()) {
            self.cams.rec.autoplay = false;
            self.rec_play(c, date, d, d.clips.len() - 1);
            if std::mem::take(&mut self.cams.rec.autosound) {
                self.rec_sound(c, true);
            }
        }
        let pb = self.hub.camera_playback(&c.id).unwrap_or_default();
        let position = pb.position.map(|t| t.with_timezone(&Utc));
        // La séquence en cours suit la position de lecture.
        if let (Some(p), Some(d)) = (position, &day) {
            if let Some(i) = d.clip_at(p) {
                self.cams.rec.current = Some(d.clips[i]);
            }
        }

        // Raccourcis : Espace = lecture / pause, ← → = séquence précédente / suivante, S = son.
        if ui.ctx().memory(|m| m.focused().is_none()) {
            let (space, left, right, s) =
                ui.input(|i| (i.key_pressed(Key::Space), i.key_pressed(Key::ArrowLeft), i.key_pressed(Key::ArrowRight), i.key_pressed(Key::S)));
            if s && pb.sound != Some(false) {
                self.rec_sound(c, self.cams.rec.player.is_none());
            } else if space {
                self.rec_toggle(c, date, day.as_ref(), &pb);
            } else if left {
                self.rec_step(c, date, day.as_ref(), -1);
            } else if right {
                self.rec_step(c, date, day.as_ref(), 1);
            }
        }

        self.rec_days_row(ui, c, date, today, day.as_ref(), loading);
        ui.add_space(10.0);

        let avail = ui.available_size();
        const GRIP: f32 = 14.0;
        let max_side = (avail.x * 0.5).max(280.0);
        let side = if self.cams.wide { 0.0 } else { self.cams.side_width.clamp(260.0, max_side) };
        let lw = (avail.x - side - if side > 0.0 { GRIP + 4.0 } else { 0.0 }).max(260.0);
        // Sous l'image : commandes (≈ 52), frise (≈ 104), remarques (≈ 40), espacements.
        let vh = (lw * 9.0 / 16.0).min(avail.y - 222.0).max(150.0);
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(lw);
                self.rec_video(ui, c, date, &pb, day.as_ref(), vec2(lw, vh));
                ui.add_space(8.0);
                self.rec_transport(ui, c, date, &pb, day.as_ref());
                ui.add_space(8.0);
                let current = self.cams.rec.current;
                if let Some(t) = self.rec_timeline(ui, date, day.as_ref(), loading, position, current, lw) {
                    self.rec_seek(c, date, day.as_ref(), t);
                }
                self.rec_notes(ui, c, date, today, day.as_ref(), error);
            });
            if side > 0.0 {
                self.side_grip(ui, &c.id, avail.y - 10.0, vh, side, max_side);
                ui.vertical(|ui| {
                    ui.set_width(side);
                    self.rec_list(ui, c, date, day.as_ref(), loading, &pb, avail.y - 10.0);
                });
            }
        });
        // Les vignettes attendent la fin de l'analyse et le démarrage d'une lecture (la caméra
        // n'accepte que peu de connexions à la fois).
        self.rec_pump_thumbs(&c.id, loading || (pb.active && !pb.playing));
        if loading || pb.active || pb.exporting {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
    }

    /// Résumé du jour pour la colonne du direct (si la carte a déjà été lue).
    /// La relecture ne survit pas à la vue : on l'arrête dès qu'elle n'est plus affichée.
    pub(in crate::gui) fn recordings_housekeeping(&mut self) {
        let rec = &self.cams.rec;
        if rec.cam.is_empty() || rec.shown_at.is_some_and(|t| t.elapsed() < Duration::from_millis(800)) {
            return;
        }
        let id = rec.cam.clone();
        if self.hub.camera_playback(&id).is_some_and(|p| p.active || p.paused || p.ended || p.error.is_some()) {
            let _ = self.hub.camera_stop_playback(&id, Source::User);
        }
        // Le son se coupe aussi (comme le micro du direct quand on quitte la caméra).
        if self.cams.rec.player.take().is_some() {
            let _ = self.hub.set_playback_audio(&id, None, Source::User);
        }
        self.cams.rec.shown_at = None;
    }

    /// Écoute (ou coupe) le son de la relecture. L'écoute reste active d'une séquence à l'autre.
    fn rec_sound(&mut self, c: &CameraView, on: bool) {
        if !on {
            let _ = self.hub.set_playback_audio(&c.id, None, Source::User);
            self.cams.rec.player = None;
            return;
        }
        match homesmart::camera::audio::Player::new() {
            Ok(player) => {
                player.output.set_volume(self.cams.volume);
                player.output.set_enhance(self.hub.prefs().audio_enhance);
                let out = player.output.clone();
                let sink: homesmart::camera::stream::AudioSink = Arc::new(move |s: &[f32], rate: u32| out.push(s, rate));
                match self.hub.set_playback_audio(&c.id, Some(sink), Source::User) {
                    Ok(()) => self.cams.rec.player = Some(player),
                    Err(e) => self.toast(format!("Son : {e:#}"), true),
                }
            }
            Err(e) => self.toast(format!("Son impossible : {e:#}"), true),
        }
    }

    // ---- Données : analyses de la carte, jours, vignettes ----------------------------------

    /// Analyse la journée si besoin (première fois, journée en cours périmée, ou `force`).
    fn rec_ensure_day(&mut self, id: &str, date: NaiveDate, today: NaiveDate, force: bool) {
        let prev = {
            let mut days = self.cams.rec.days.lock().unwrap();
            let e = days.entry(date).or_default();
            if e.loading {
                return;
            }
            // Une journée passée ne change plus ; celle en cours se réanalyse de temps en temps ;
            // après un échec, on attend un peu avant de réessayer tout seul.
            let fresh = e.at.is_some_and(|t| date != today || t.elapsed() <= TODAY_STALE);
            let after_error = e.error.is_some() && e.at.is_some_and(|t| t.elapsed() < Duration::from_secs(30));
            if !force && (after_error || (e.data.is_some() && fresh)) {
                return;
            }
            (e.loading, e.error) = (true, None);
            // Journée en cours : on ne réanalyse que sa fin. Jour passé : tout (bouton « Réanalyser »).
            if date == today { e.data.clone() } else { None }
        };
        let refresh = prev.is_some();
        let (hub, slot, presence, ctx, id) = (self.hub.clone(), self.cams.rec.days.clone(), self.cams.rec.presence.clone(), self.ctx.clone(), id.to_string());
        self.rt.spawn(async move {
            let progress = {
                let (slot, ctx) = (slot.clone(), ctx.clone());
                move |d: &DayFootage| {
                    // Une réanalyse ne remplace l'affichage qu'une fois terminée (pas de clignotement).
                    if !refresh {
                        slot.lock().unwrap().entry(date).or_default().data = Some(d.clone());
                        ctx.request_repaint();
                    }
                }
            };
            let r = hub.camera_scan_day(&id, date, prev, progress).await;
            {
                let mut days = slot.lock().unwrap();
                let e = days.entry(date).or_default();
                (e.loading, e.at) = (false, Some(Instant::now()));
                match r {
                    Ok(d) => {
                        presence.lock().unwrap().days.insert(date, (Some(!d.clips.is_empty()), Instant::now()));
                        (e.data, e.error) = (Some(d), None);
                    }
                    Err(err) => e.error = Some(format!("{err:#}")),
                }
            }
            ctx.request_repaint();
        });
    }

    /// Jours de la bande : y a-t-il de la vidéo ? (un sondage par jour, après l'analyse du jour affiché).
    fn rec_ensure_presence(&mut self, id: &str, date: NaiveDate, today: NaiveDate) {
        let busy = self.cams.rec.days.lock().unwrap().get(&date).is_some_and(|s| s.loading);
        if busy {
            return;
        }
        let dates: Vec<NaiveDate> = {
            let mut p = self.cams.rec.presence.lock().unwrap();
            if p.loading {
                return;
            }
            let want: Vec<NaiveDate> = strip_days(date, today)
                .into_iter()
                .filter(|d| match p.days.get(d) {
                    None => true,
                    Some((_, at)) => *d == today && at.elapsed() > TODAY_STALE,
                })
                .collect();
            if want.is_empty() {
                return;
            }
            p.loading = true;
            want
        };
        let (hub, slot, ctx, id) = (self.hub.clone(), self.cams.rec.presence.clone(), self.ctx.clone(), id.to_string());
        self.rt.spawn(async move {
            let r = hub.camera_recording_days(&id, dates.clone()).await;
            let mut p = slot.lock().unwrap();
            p.loading = false;
            let now = Instant::now();
            match r {
                Ok(list) => {
                    for (d, has) in list {
                        p.days.insert(d, (has, now));
                    }
                }
                // Pas de réponse : on n'insiste pas tout de suite.
                Err(_) => {
                    for d in dates {
                        p.days.entry(d).or_insert((None, now));
                    }
                }
            }
            ctx.request_repaint();
        });
    }

    /// Vignette d'une séquence : `None` = pas encore là, `Some(None)` = indisponible.
    fn rec_thumb(&mut self, at: DateTime<Utc>) -> Option<Option<egui::TextureId>> {
        let key = at.timestamp();
        if let Some(t) = self.cams.rec.thumb_tex.get(&key) {
            return Some(Some(t.id()));
        }
        let frame = self.cams.rec.thumbs.lock().unwrap().frames.get(&key).cloned()?;
        let Some(frame) = frame else { return Some(None) };
        let tex = self.ctx.load_texture(format!("rec-thumb-{key}"), color_image(&frame), egui::TextureOptions::LINEAR);
        let id = tex.id();
        self.cams.rec.thumb_tex.insert(key, tex);
        Some(Some(id))
    }

    /// Une vignette à la fois, la plus haute visible d'abord.
    fn rec_pump_thumbs(&mut self, id: &str, hold: bool) {
        if hold || self.hub.ffmpeg_status().path.is_none() {
            return;
        }
        let at = {
            let mut t = self.cams.rec.thumbs.lock().unwrap();
            if t.busy {
                return;
            }
            let Some(at) = t.wanted.iter().find(|a| !t.frames.contains_key(&a.timestamp())).copied() else { return };
            t.busy = true;
            at
        };
        let (hub, slot, ctx, id) = (self.hub.clone(), self.cams.rec.thumbs.clone(), self.ctx.clone(), id.to_string());
        self.rt.spawn(async move {
            // Une seconde après le début : la première image est parfois encore un fondu.
            let r = hub.camera_thumbnail(&id, at + chrono::Duration::seconds(1), THUMB.0, THUMB.1).await;
            let mut t = slot.lock().unwrap();
            t.busy = false;
            t.frames.insert(at.timestamp(), r.ok().map(Arc::new));
            ctx.request_repaint();
        });
    }

    // ---- Actions de lecture ----------------------------------------------------------------

    /// Relit à partir de la séquence `i`, puis enchaîne les suivantes jusqu'à la fin de la journée.
    fn rec_play(&mut self, c: &CameraView, date: NaiveDate, day: &DayFootage, i: usize) {
        let Some(&clip) = day.clips.get(i) else { return };
        self.rec_play_from(c, date, clip.start, clip);
    }

    fn rec_play_from(&mut self, c: &CameraView, date: NaiveDate, at: DateTime<Utc>, clip: Clip) {
        let end = playback::day_window(date).map_or(clip.end, |(_, e)| e.min(Utc::now())).max(clip.end);
        self.cams.rec.current = Some(clip);
        self.cams.rec.scrolled_to = None;
        if let Err(e) = self.hub.camera_play_recording(&c.id, local(at), local(end), local(at), Source::User) {
            self.toast(format!("Relecture impossible : {e:#}"), true);
        }
    }

    /// Clic sur la frise : relit à partir de cet instant, ou de la séquence suivante s'il tombe dans un trou.
    fn rec_seek(&mut self, c: &CameraView, date: NaiveDate, day: Option<&DayFootage>, t: DateTime<Utc>) {
        let Some(d) = day else { return };
        match d.clip_from(t) {
            Some(i) if d.clips[i].contains(t) => self.rec_play_from(c, date, t, d.clips[i]),
            Some(i) => self.rec_play(c, date, d, i),
            None => self.toast(format!("Aucune vidéo après {}", local(t).format("%H:%M")), false),
        }
    }

    /// Séquence précédente (ou début de la séquence en cours) / suivante.
    fn rec_step(&mut self, c: &CameraView, date: NaiveDate, day: Option<&DayFootage>, dir: i32) {
        let Some(d) = day.filter(|d| !d.clips.is_empty()) else { return };
        let n = d.clips.len();
        let pos = self.hub.camera_playback(&c.id).and_then(|p| p.position).map(|t| t.with_timezone(&Utc));
        let cur = self.cams.rec.current.and_then(|cl| find_clip(&d.clips, cl));
        let target = match (dir < 0, cur) {
            (true, Some(i)) if pos.is_some_and(|p| p > d.clips[i].start + chrono::Duration::seconds(3)) => i,
            (true, Some(i)) => i.saturating_sub(1),
            (false, Some(i)) if i + 1 < n => i + 1,
            (false, Some(_)) => return,
            (true, None) => n - 1,
            (false, None) => 0,
        };
        self.rec_play(c, date, d, target);
    }

    /// Lecture / pause ; sans lecture en cours, relit la séquence choisie (ou la plus récente).
    fn rec_toggle(&mut self, c: &CameraView, date: NaiveDate, day: Option<&DayFootage>, pb: &PlaybackView) {
        if pb.active {
            let _ = self.hub.camera_pause_playback(&c.id, Source::User);
            return;
        }
        let Some(d) = day.filter(|d| !d.clips.is_empty()) else { return };
        if pb.paused && !pb.ended {
            if let Some(at) = pb.position.map(|t| t.with_timezone(&Utc)) {
                let clip = d.clip_at(at).map(|i| d.clips[i]).or(self.cams.rec.current).unwrap_or(d.clips[d.clips.len() - 1]);
                self.rec_play_from(c, date, at, clip);
                return;
            }
        }
        let i = self.cams.rec.current.and_then(|cl| find_clip(&d.clips, cl)).unwrap_or(d.clips.len() - 1);
        self.rec_play(c, date, d, i);
    }

    fn rec_export(&mut self, c: &CameraView, clip: Clip) {
        let one = chrono::Duration::seconds(1);
        if let Err(e) = self.hub.camera_export_recording(&c.id, local(clip.start - one), local(clip.end + one), Source::User) {
            self.toast(format!("Export impossible : {e:#}"), true);
        }
    }

    fn rec_set_date(&mut self, c: &CameraView, date: NaiveDate) {
        if self.cams.rec.date == Some(date) {
            return;
        }
        let _ = self.hub.camera_stop_playback(&c.id, Source::User);
        (self.cams.rec.date, self.cams.rec.view, self.cams.rec.current) = (Some(date), None, None);
    }

    // ---- Dessin ----------------------------------------------------------------------------

    /// Bande des jours (une semaine), résumé du jour et bouton de réanalyse.
    fn rec_days_row(&mut self, ui: &mut Ui, c: &CameraView, date: NaiveDate, today: NaiveDate, day: Option<&DayFootage>, loading: bool) {
        let presence: HashMap<NaiveDate, Option<bool>> = self.cams.rec.presence.lock().unwrap().days.iter().map(|(d, (h, _))| (*d, *h)).collect();
        let mut pick = None;
        let mut rescan = false;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if ui.add(egui::Button::new("◀").min_size(vec2(30.0, 30.0))).on_hover_text("Semaine précédente").clicked() {
                pick = date.checked_sub_signed(chrono::Duration::days(7));
            }
            for d in strip_days(date, today) {
                let has = presence.get(&d).copied().flatten();
                let tip = match has {
                    Some(true) => "Il y a de la vidéo ce jour-là",
                    Some(false) => "Aucune vidéo ce jour-là",
                    None => "Pas encore vérifié",
                };
                if day_chip(ui, &day_label(d, today), d == date, has).on_hover_text(format!("{} — {tip}", long_date(d))).clicked() {
                    pick = Some(d);
                }
            }
            let next = date.checked_add_signed(chrono::Duration::days(7)).map(|d| d.min(today));
            if ui.add_enabled(date < today, egui::Button::new("▶").min_size(vec2(30.0, 30.0))).on_hover_text("Semaine suivante").clicked() {
                pick = next;
            }
            ui.add_space(10.0);
            let summary = match day {
                Some(d) if loading && !d.complete => format!("Analyse de la carte SD… {:.0} %", d.progress * 100.0),
                None if loading => "Analyse de la carte SD…".to_string(),
                Some(d) if d.clips.is_empty() => format!("{} · aucune séquence", long_date(date)),
                Some(d) => format!(
                    "{} · {} séquence{} · {} de vidéo",
                    long_date(date),
                    d.clips.len(),
                    if d.clips.len() > 1 { "s" } else { "" },
                    human(d.total_secs())
                ),
                None => long_date(date),
            };
            ui.label(RichText::new(summary).size(12.5).color(theme::muted()));
            if loading {
                ui.spinner();
            } else if widgets::icon_button(ui, Icon::Refresh, 28.0).on_hover_text("Réanalyser la carte pour ce jour").clicked() {
                rescan = true;
            }
        });
        if let Some(d) = pick.filter(|d| *d <= today) {
            self.rec_set_date(c, d);
        }
        if rescan {
            self.rec_ensure_day(&c.id, date, today, true);
        }
    }

    /// L'image relue (zoomable comme le direct), ou un écran d'accueil qui propose la dernière séquence.
    fn rec_video(&mut self, ui: &mut Ui, c: &CameraView, date: NaiveDate, pb: &PlaybackView, day: Option<&DayFootage>, size: egui::Vec2) {
        let key = format!("{}~pb", c.id);
        let was_zoomed = self.cams.zoom.get(&key).is_some_and(|z| z.is_zoomed());
        // Le glisser ne sert qu'à se déplacer dans l'image zoomée.
        let (rect, resp) = ui.allocate_exact_size(size, if was_zoomed { Sense::click_and_drag() } else { Sense::click() });
        let started = pb.active || pb.paused || pb.ended || pb.error.is_some();
        let tex = if started { self.playback_texture(&c.id) } else { None };
        let p = ui.painter_at(rect);
        p.rect_filled(rect, CornerRadius::same(16), Color32::from_rgb(6, 8, 12));
        let mut shown = None;
        if let Some((id, [w, h])) = tex {
            let img = fit(rect, w as f32 / h.max(1) as f32);
            let (z, _) = self.zoom_gestures(ui, &resp, &key, img, true);
            let r = if (img.width() - rect.width()).abs() < 1.0 && (img.height() - rect.height()).abs() < 1.0 { 16 } else { 0 };
            p.add(egui::epaint::RectShape::filled(img, CornerRadius::same(r), Color32::WHITE).with_texture(id, z.visible()));
            shown = Some((img, z.is_zoomed()));
        }
        if !started {
            let ffmpeg = self.hub.ffmpeg_status().path.is_some();
            let clips = day.map(|d| d.clips.as_slice()).unwrap_or(&[]);
            icons::paint(&p, Rect::from_center_size(rect.center() - vec2(0.0, 46.0), vec2(42.0, 42.0)), Icon::Film, theme::faint());
            let msg = if !ffmpeg {
                "FFmpeg est nécessaire pour relire les enregistrements (H.265) : installe-le ci-dessous."
            } else if day.is_none_or(|d| !d.complete) && clips.is_empty() {
                "Lecture de la carte SD…"
            } else if clips.is_empty() {
                "Aucune séquence enregistrée ce jour-là."
            } else {
                "Choisis une séquence dans la liste, ou clique sur la frise."
            };
            let g = p.layout(msg.to_string(), FontId::proportional(13.5), theme::muted(), (rect.width() - 60.0).max(120.0));
            p.galley(pos2(rect.center().x - g.size().x / 2.0, rect.center().y - 10.0), g, theme::muted());
            if let (true, Some(last)) = (ffmpeg, clips.last()) {
                let label = format!("Relire la dernière séquence ({})", local(last.start).format("%H:%M"));
                let bw = ui.painter().layout_no_wrap(label.clone(), theme::bold(14.0), theme::bg()).size().x + 32.0;
                let br = Rect::from_center_size(pos2(rect.center().x, rect.center().y + 34.0), vec2(bw, 36.0));
                // Posé par-dessus l'image, sans déplacer la mise en page (contrairement à `put`).
                if ui.place(br, widgets::primary_button(&label, theme::accent())).clicked() {
                    if let Some(d) = day {
                        self.rec_play(c, date, d, d.clips.len() - 1);
                    }
                }
            }
            return;
        }
        // Message par-dessus l'image.
        let msg = if let Some(e) = &pb.error {
            Some(format!("Relecture impossible : {e}"))
        } else if pb.active && !pb.playing {
            Some("Chargement de la séquence…".to_string())
        } else if pb.ended {
            Some("Fin des enregistrements de la journée".to_string())
        } else {
            None
        };
        if let Some(msg) = msg {
            p.rect_filled(rect, CornerRadius::same(16), Color32::from_black_alpha(if tex.is_some() { 120 } else { 0 }));
            let g = p.layout(msg, FontId::proportional(14.0), Color32::WHITE, (rect.width() - 40.0).max(80.0));
            p.galley(pos2(rect.center().x - g.size().x / 2.0, rect.center().y - g.size().y / 2.0), g, Color32::WHITE);
        } else if pb.paused {
            p.circle_filled(rect.center(), 30.0, Color32::from_black_alpha(140));
            icons::paint(&p, Rect::from_center_size(rect.center() + vec2(2.0, 0.0), vec2(26.0, 26.0)), Icon::Play, Color32::WHITE);
        }
        // Pastille d'état.
        let (label, dot) = if pb.paused { ("EN PAUSE", theme::muted()) } else { ("RELECTURE", theme::accent()) };
        let g = p.layout_no_wrap(label.to_string(), theme::bold(10.0), Color32::WHITE);
        let pill_size = g.size() + vec2(26.0, 8.0);
        let pill = Rect::from_min_size(pos2(rect.right() - 10.0 - pill_size.x, rect.top() + 10.0), pill_size);
        p.rect_filled(pill, CornerRadius::same(10), Color32::from_black_alpha(150));
        p.circle_filled(pill.left_center() + vec2(11.0, 0.0), 4.0, dot);
        p.galley(pill.left_top() + vec2(20.0, 4.0), g, Color32::WHITE);
        // Heure de l'image relue.
        if let Some(t) = pb.position {
            let text = format!("{} · {}", day_label(t.date_naive(), Local::now().date_naive()), t.format("%H:%M:%S"));
            let g = p.layout_no_wrap(text, theme::bold(13.0), Color32::WHITE);
            let bg = Rect::from_min_size(pos2(rect.left() + 10.0, rect.bottom() - 10.0 - g.size().y - 10.0), g.size() + vec2(18.0, 10.0));
            p.rect_filled(bg, CornerRadius::same(9), Color32::from_black_alpha(160));
            p.galley(bg.min + vec2(9.0, 5.0), g, Color32::WHITE);
        }
        let zoomed = shown.is_some_and(|(_, z)| z);
        if resp.hovered() && (pb.active || pb.paused) && !zoomed {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        // Clic : lecture / pause. Zoomée, le double-clic revient à l'image entière : le clic
        // simple attend d'être sûr qu'aucun second ne suit.
        if pb.active || pb.paused {
            if resp.double_clicked() {
                self.cams.rec.click_at = None;
            } else if resp.clicked() {
                if was_zoomed {
                    self.cams.rec.click_at = Some(Instant::now());
                } else {
                    self.rec_toggle(c, date, day, pb);
                }
            }
            if let Some(t) = self.cams.rec.click_at {
                if t.elapsed().as_secs_f64() > ui.ctx().options(|o| o.input_options.max_double_click_delay) {
                    self.cams.rec.click_at = None;
                    self.rec_toggle(c, date, day, pb);
                } else {
                    ui.ctx().request_repaint_after(Duration::from_millis(40));
                }
            }
        }
        if let Some((img, _)) = shown {
            self.zoom_badge(ui, &key, img);
        }
    }

    /// Précédente / lecture-pause / suivante, heure relue, export de la séquence.
    fn rec_transport(&mut self, ui: &mut Ui, c: &CameraView, date: NaiveDate, pb: &PlaybackView, day: Option<&DayFootage>) {
        let clips = day.map(|d| d.clips.as_slice()).unwrap_or(&[]);
        let idx = self.cams.rec.current.and_then(|cl| find_clip(clips, cl));
        let any = !clips.is_empty();
        let (mut prev, mut toggle, mut next, mut export, mut cancel) = (false, false, false, None, false);
        let mut sound: Option<bool> = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            prev = round_button(ui, Icon::Prev, 32.0, false, any).on_hover_text("Séquence précédente (←)").clicked();
            let icon = if pb.active { Icon::Pause } else { Icon::Play };
            toggle = round_button(ui, icon, 40.0, true, any).on_hover_text("Lecture / pause (Espace)").clicked();
            next = round_button(ui, Icon::Next, 32.0, false, any && idx.is_none_or(|i| i + 1 < clips.len())).on_hover_text("Séquence suivante (→)").clicked();
            // Son de la séquence (micro de la caméra, enregistré avec l'image).
            let listening = self.cams.rec.player.is_some();
            if pb.sound == Some(false) {
                let r = round_button(ui, Icon::Muted, 32.0, false, false);
                r.on_hover_text("Cette séquence n'a pas de son");
            } else {
                let tip = if listening { "Couper le son (S)" } else { "Écouter le son de la séquence (S)" };
                if round_button(ui, if listening { Icon::Speaker } else { Icon::Muted }, 32.0, false, any).on_hover_text(tip).clicked() {
                    sound = Some(!listening);
                }
                // Volume, s'il reste de la place (fenêtre étroite : le bouton seul).
                if listening && ui.available_width() > 360.0 {
                    ui.spacing_mut().slider_width = 80.0;
                    if ui.add(egui::Slider::new(&mut self.cams.volume, 0.0..=2.0).show_value(false)).on_hover_text("Volume").changed() {
                        if let Some(p) = &self.cams.rec.player {
                            p.output.set_volume(self.cams.volume);
                        }
                    }
                    self.enhance_toggle(ui);
                }
            }
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let time = pb.position.map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_else(|| "--:--:--".into());
                ui.label(RichText::new(time).font(theme::bold(17.0)).color(if pb.position.is_some() { theme::text() } else { theme::faint() }));
                let sub = match idx {
                    Some(i) => format!("séquence {} sur {} · {}", i + 1, clips.len(), human(clips[i].secs())),
                    None if any => format!("{} séquence{}", clips.len(), if clips.len() > 1 { "s" } else { "" }),
                    None => "aucune séquence".to_string(),
                };
                ui.label(RichText::new(sub).size(11.5).color(theme::muted()));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // Fenêtre étroite : versions compactes (icône seule, barre plus courte).
                let room = ui.available_width();
                if pb.exporting {
                    if ui.button("Annuler").on_hover_text("Arrête l'export (le fichier partiel est supprimé)").clicked() {
                        cancel = true;
                    }
                    let when = pb.export_range.map(|(s, _)| s.format("%H:%M:%S").to_string()).unwrap_or_default();
                    let text = if room > 250.0 { format!("Export {when} · {:.0} %", pb.export_progress * 100.0) } else { format!("{:.0} %", pb.export_progress * 100.0) };
                    ui.add(egui::ProgressBar::new(pb.export_progress).desired_width((room - 80.0).clamp(60.0, 170.0)).text(text));
                } else if let Some(i) = idx {
                    let tip = "Copie la séquence sur le PC en .mp4 (dossier Vidéos\\HomeSmart), sans ré-encodage. La caméra la diffuse à vitesse réelle : compter sa durée.";
                    let clicked = if room >= 200.0 {
                        let b = ui.add(egui::Button::new("        Exporter la séquence").min_size(vec2(0.0, 32.0))).on_hover_text(tip);
                        icons::paint(ui.painter(), Rect::from_center_size(b.rect.left_center() + vec2(18.0, 0.0), vec2(15.0, 15.0)), Icon::Download, theme::muted());
                        b.clicked()
                    } else {
                        round_button(ui, Icon::Download, 32.0, false, true).on_hover_text(format!("Exporter la séquence — {tip}")).clicked()
                    };
                    if clicked {
                        export = Some(clips[i]);
                    }
                }
            });
        });
        // Résultat du dernier export.
        let mut close = false;
        if let Some(path) = &pb.export_done {
            ui.horizontal_wrapped(|ui| {
                let name = path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
                ui.label(RichText::new(format!("Séquence exportée : {name}")).size(12.0).color(theme::accent()));
                if ui.small_button("Ouvrir le dossier").clicked() {
                    open_folder(Some(path.as_path()));
                }
                close = ui.small_button("×").on_hover_text("Masquer").clicked();
            });
        }
        if let Some(e) = &pb.export_error {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("Export impossible : {e}")).size(12.0).color(theme::warn()));
                close = ui.small_button("×").on_hover_text("Masquer").clicked();
            });
        }
        if close {
            self.hub.camera_clear_export(&c.id);
        }
        if let Some(on) = sound {
            self.rec_sound(c, on);
        }
        if prev {
            self.rec_step(c, date, day, -1);
        }
        if toggle {
            self.rec_toggle(c, date, day, pb);
        }
        if next {
            self.rec_step(c, date, day, 1);
        }
        if let Some(clip) = export {
            self.rec_export(c, clip);
        }
        if cancel {
            let _ = self.hub.camera_cancel_export(&c.id, Source::User);
        }
    }

    /// Frise de la journée. Renvoie l'instant cliqué (relecture à partir de là).
    #[allow(clippy::too_many_arguments)]
    fn rec_timeline(&mut self, ui: &mut Ui, date: NaiveDate, day: Option<&DayFootage>, loading: bool, position: Option<DateTime<Utc>>, current: Option<Clip>, width: f32) -> Option<DateTime<Utc>> {
        let (d0, d1) = playback::day_window(date)?;
        let total = offset(d0, d1);
        let clips = day.map(|d| d.clips.as_slice()).unwrap_or(&[]);
        let mut view = self.cams.rec.view.unwrap_or((0.0, total));
        let (mut zoom, mut reset, mut fit_clips) = (None, false, false);
        ui.horizontal(|ui| {
            // L'aide s'efface quand la place manque (les boutons de zoom restent).
            if width > 540.0 {
                ui.label(RichText::new("Molette : zoom · glisser : se déplacer · clic : relire à partir de là").size(11.0).color(theme::faint()));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                reset = ui.small_button("Journée").on_hover_text("Toute la journée").clicked();
                fit_clips = ui.add_enabled(!clips.is_empty(), egui::Button::new("Ajuster").small()).on_hover_text("Cadrer sur les séquences").clicked();
                if ui.small_button("+").on_hover_text("Zoomer").clicked() {
                    zoom = Some(0.5);
                }
                if ui.small_button("−").on_hover_text("Dézoomer").clicked() {
                    zoom = Some(2.0);
                }
            });
        });
        ui.add_space(2.0);
        let (rect, resp) = ui.allocate_exact_size(vec2(width, 50.0), Sense::click_and_drag());
        ui.allocate_exact_size(vec2(width, 18.0), Sense::hover());
        let x_of = |t: f64, v: (f64, f64)| rect.left() + ((t - v.0) / (v.1 - v.0)) as f32 * rect.width();
        let t_of = |x: f32, v: (f64, f64)| v.0 + ((x - rect.left()) / rect.width()) as f64 * (v.1 - v.0);

        // Molette : zoom autour du pointeur ; glisser : déplacement.
        if resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > 0.0 {
                let anchor = resp.hover_pos().map(|p| t_of(p.x, view)).unwrap_or((view.0 + view.1) / 2.0);
                view = zoom_view(view, (-(scroll as f64) * 0.004).exp(), anchor, total);
                ui.ctx().input_mut(|i| i.smooth_scroll_delta = egui::Vec2::ZERO);
            }
        }
        if resp.dragged() {
            let dt = -(resp.drag_delta().x as f64) / rect.width() as f64 * (view.1 - view.0);
            view = clamp_view(view.0 + dt, view.1 - view.0, total);
        }
        if let Some(z) = zoom {
            let pos = position.map(|p| offset(d0, p)).filter(|s| *s >= view.0 && *s <= view.1);
            view = zoom_view(view, z, pos.unwrap_or((view.0 + view.1) / 2.0), total);
        }
        if reset {
            view = (0.0, total);
        }
        if let (true, Some(f), Some(l)) = (fit_clips, clips.first(), clips.last()) {
            let (a, b) = (offset(d0, f.start), offset(d0, l.end));
            let pad = ((b - a) * 0.06).max(90.0);
            view = clamp_view(a - pad, b - a + 2.0 * pad, total);
        }
        // La tête de lecture reste visible (sauf pendant qu'on fait glisser la frise).
        if let Some(s) = position.map(|p| offset(d0, p)).filter(|s| (0.0..=total).contains(s)) {
            if !resp.dragged() && (s < view.0 || s > view.1) {
                let span = view.1 - view.0;
                view = clamp_view(s - span * 0.2, span, total);
            }
        }
        self.cams.rec.view = Some(view);

        let p = ui.painter_at(rect.expand2(vec2(0.0, 20.0)));
        p.rect_filled(rect, CornerRadius::same(10), theme::raised());
        // Graduations.
        let span = view.1 - view.0;
        let step = [60.0, 300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0, 10800.0, 21600.0]
            .into_iter()
            .find(|s| (s / span) as f32 * rect.width() >= 62.0)
            .unwrap_or(21600.0);
        let mut t = (view.0 / step).ceil() * step;
        while t <= view.1 + 0.5 {
            let x = x_of(t, view);
            p.line_segment([pos2(x, rect.top() + 4.0), pos2(x, rect.bottom() - 4.0)], Stroke::new(1.0, theme::line()));
            // Pas de « 00 h » du lendemain au bout de la frise ; libellés gardés dans la largeur.
            if t < total - 1.0 {
                let label = local(d0 + secs(t)).format(if step >= 3600.0 { "%H h" } else { "%H:%M" }).to_string();
                let g = p.layout_no_wrap(label, FontId::proportional(10.5), theme::faint());
                let lx = (x - g.size().x / 2.0).clamp(rect.left(), rect.right() - g.size().x);
                p.galley(pos2(lx, rect.bottom() + 4.0), g, theme::faint());
            }
            t += step;
        }
        let band = |a: f64, b: f64, inset: f32| {
            let (x0, x1) = (x_of(a, view).max(rect.left()), x_of(b, view).min(rect.right()));
            (x1 > rect.left() && x0 < rect.right()).then(|| Rect::from_min_max(pos2(x0, rect.top() + inset), pos2(x1.max(x0 + 2.0), rect.bottom() - inset)))
        };
        if let Some(d) = day {
            for h in &d.hours {
                let Some((a, b)) = playback::hour_window(date, h.hour) else { continue };
                // Pendant l'analyse : les heures où il y a de la vidéo, en attendant les séquences.
                if !d.complete && h.has_video() {
                    if let Some(r) = band(offset(d0, a), offset(d0, b), 14.0) {
                        p.rect_filled(r, CornerRadius::same(3), theme::alpha(theme::accent(), 0.18));
                    }
                }
                // Heures restées sans réponse.
                if d.complete && !h.answered && !h.future && !h.has_video() {
                    if let Some(r) = band(offset(d0, a), offset(d0, b), 4.0) {
                        p.rect_filled(r, CornerRadius::same(3), theme::alpha(theme::accent(), 0.12));
                    }
                }
            }
        }
        // Les séquences.
        let cur = current.and_then(|cl| find_clip(clips, cl));
        for (i, cl) in clips.iter().enumerate() {
            if let Some(r) = band(offset(d0, cl.start), offset(d0, cl.end), 9.0) {
                let color = if cur == Some(i) { theme::accent() } else { theme::accent() };
                p.rect_filled(r, CornerRadius::same(3), color);
            }
        }
        // Le futur (journée en cours).
        let now = offset(d0, Utc::now());
        if now < total {
            let x = x_of(now, view).clamp(rect.left(), rect.right());
            if x < rect.right() {
                p.rect_filled(Rect::from_min_max(pos2(x, rect.top()), rect.max), CornerRadius::same(10), theme::alpha(theme::bg(), 0.55));
                if x > rect.left() {
                    p.line_segment([pos2(x, rect.top() + 3.0), pos2(x, rect.bottom() - 3.0)], Stroke::new(1.5, theme::alpha(theme::accent(), 0.8)));
                }
            }
        }
        if loading && clips.is_empty() {
            p.text(rect.center(), Align2::CENTER_CENTER, "Analyse de la carte SD…", FontId::proportional(12.0), theme::muted());
        }
        // Tête de lecture.
        if let Some(s) = position.map(|p| offset(d0, p)).filter(|s| *s >= view.0 && *s <= view.1) {
            let x = x_of(s, view);
            p.line_segment([pos2(x, rect.top() - 2.0), pos2(x, rect.bottom() + 2.0)], Stroke::new(2.0, Color32::WHITE));
            p.circle_filled(pos2(x, rect.top() - 1.0), 4.5, Color32::WHITE);
        }
        // Survol : heure sous le pointeur, et la séquence qu'elle touche.
        if let Some(hp) = resp.hover_pos().filter(|_| !resp.dragged()) {
            let s = t_of(hp.x, view);
            let at = d0 + secs(s);
            p.line_segment([pos2(hp.x, rect.top()), pos2(hp.x, rect.bottom())], Stroke::new(1.0, theme::alpha(Color32::WHITE, 0.5)));
            let text = match clips.iter().find(|c| c.contains(at)) {
                Some(c) => format!("{} · séquence de {}", local(at).format("%H:%M:%S"), human(c.secs())),
                None => match clips.iter().find(|c| c.start > at) {
                    Some(c) => format!("{} · prochaine séquence à {}", local(at).format("%H:%M:%S"), local(c.start).format("%H:%M:%S")),
                    None => local(at).format("%H:%M:%S").to_string(),
                },
            };
            let tp = ui.ctx().layer_painter(LayerId::new(Order::Tooltip, Id::unique(("rec-hover", rect.min.x as i32))));
            let g = tp.layout_no_wrap(text, FontId::proportional(11.5), theme::text());
            let w = g.size().x + 14.0;
            let x = (hp.x - w / 2.0).clamp(rect.left(), rect.right() - w);
            let box_ = Rect::from_min_size(pos2(x, rect.top() - g.size().y - 12.0), vec2(w, g.size().y + 8.0));
            tp.rect_filled(box_, CornerRadius::same(6), theme::surface_hi());
            tp.rect_stroke(box_, CornerRadius::same(6), Stroke::new(1.0, theme::line()), StrokeKind::Inside);
            tp.galley(box_.min + vec2(7.0, 4.0), g, theme::text());
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        if resp.dragged() {
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        }
        if resp.clicked() {
            if let Some(pp) = resp.interact_pointer_pos() {
                return Some(d0 + secs(t_of(pp.x, view)));
            }
        }
        None
    }

    /// FFmpeg absent, erreurs d'analyse, sondages sans réponse, et le rappel « lecture seule ».
    fn rec_notes(&mut self, ui: &mut Ui, c: &CameraView, date: NaiveDate, today: NaiveDate, day: Option<&DayFootage>, error: Option<String>) {
        ui.add_space(4.0);
        if self.hub.ffmpeg_status().path.is_none() {
            egui::Frame::new().fill(theme::surface()).corner_radius(CornerRadius::same(14)).inner_margin(Margin::same(14)).show(ui, |ui| self.ffmpeg_panel(ui));
            ui.add_space(6.0);
        }
        if let Some(e) = error {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("Carte SD illisible : {e}")).size(12.5).color(theme::warn()));
                if ui.small_button("Réessayer").clicked() {
                    self.rec_ensure_day(&c.id, date, today, true);
                }
            });
        }
        if let Some(n) = day.map(|d| d.failed).filter(|n| *n > 0) {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("{n} vérification{} restée{} sans réponse : il manque peut-être des séquences.", if n > 1 { "s" } else { "" }, if n > 1 { "s" } else { "" })).size(11.5).color(theme::warn()));
                if ui.small_button("Réanalyser").clicked() {
                    self.rec_ensure_day(&c.id, date, today, true);
                }
            });
        }
        ui.label(
            RichText::new("Lecture seule : formatage, rétention et mode d'enregistrement de la carte ne sont pas accessibles en local sur ce modèle.")
                .size(11.0)
                .color(theme::faint()),
        );
    }

    /// Liste des séquences, de la plus récente à la plus ancienne, regroupées par heure.
    #[allow(clippy::too_many_arguments)]
    fn rec_list(&mut self, ui: &mut Ui, c: &CameraView, date: NaiveDate, day: Option<&DayFootage>, loading: bool, pb: &PlaybackView, height: f32) {
        let clips: Vec<Clip> = day.map(|d| d.clips.clone()).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label(RichText::new("SÉQUENCES").font(theme::bold(11.5)).color(theme::muted()));
            if !clips.is_empty() {
                widgets::pill(ui, &clips.len().to_string(), theme::text(), theme::raised_hi());
            }
            if loading {
                ui.spinner();
            }
        });
        ui.add_space(6.0);
        if clips.is_empty() {
            let msg = if loading { "Recherche des séquences sur la carte…" } else { "Aucune séquence ce jour-là." };
            ui.label(RichText::new(msg).size(12.5).color(theme::muted()));
            return;
        }
        let cur = self.cams.rec.current.and_then(|cl| find_clip(&clips, cl));
        let follow = cur.map(|i| clips[i].start.timestamp()).filter(|k| self.cams.rec.scrolled_to != Some(*k));
        let (mut play, mut export) = (None, None);
        let mut wanted = Vec::new();
        let id_salt = ("rec-list", c.id.clone(), date);
        ScrollArea::vertical().id_salt(id_salt).max_height((height - 34.0).max(120.0)).auto_shrink([false, false]).show(ui, |ui| {
            let w = ui.available_width();
            let mut last_hour = None;
            for i in (0..clips.len()).rev() {
                let cl = clips[i];
                let ls = local(cl.start);
                if last_hour != Some(ls.hour()) {
                    last_hour = Some(ls.hour());
                    let n = clips.iter().filter(|x| local(x.start).hour() == ls.hour()).count();
                    if i + 1 != clips.len() {
                        ui.add_space(6.0);
                    }
                    ui.label(RichText::new(format!("{} h  ·  {n} séquence{}", ls.hour(), if n > 1 { "s" } else { "" })).size(11.5).color(theme::faint()));
                    ui.add_space(2.0);
                }
                let (row, resp) = ui.allocate_exact_size(vec2(w, 72.0), Sense::click());
                if ui.is_rect_visible(row) {
                    let is_cur = cur == Some(i);
                    let hover = resp.hovered();
                    let p = ui.painter_at(row);
                    let bg = if is_cur {
                        theme::alpha(theme::accent(), 0.13)
                    } else if hover {
                        theme::raised()
                    } else {
                        Color32::TRANSPARENT
                    };
                    p.rect_filled(row, CornerRadius::same(10), bg);
                    if is_cur {
                        p.rect_stroke(row, CornerRadius::same(10), Stroke::new(1.0, theme::alpha(theme::accent(), 0.6)), StrokeKind::Inside);
                    }
                    // Vignette et durée.
                    let th = Rect::from_min_size(row.min + vec2(6.0, 6.0), vec2(107.0, 60.0));
                    match self.rec_thumb(cl.start) {
                        Some(Some(tex)) => {
                            p.add(egui::epaint::RectShape::filled(th, CornerRadius::same(8), Color32::WHITE).with_texture(tex, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))));
                        }
                        other => {
                            p.rect_filled(th, CornerRadius::same(8), theme::raised_hi());
                            icons::paint(&p, Rect::from_center_size(th.center(), vec2(20.0, 20.0)), Icon::Film, theme::faint());
                            if other.is_none() {
                                wanted.push(cl.start);
                            }
                        }
                    }
                    let g = p.layout_no_wrap(badge(cl.secs()), theme::bold(10.0), Color32::WHITE);
                    let br = Rect::from_min_size(th.right_bottom() - g.size() - vec2(12.0, 7.0), g.size() + vec2(8.0, 3.0));
                    p.rect_filled(br, CornerRadius::same(5), Color32::from_black_alpha(170));
                    p.galley(br.min + vec2(4.0, 1.5), g, Color32::WHITE);
                    // Heure, durée, état.
                    let tx = th.right() + 12.0;
                    p.text(pos2(tx, row.top() + 12.0), Align2::LEFT_TOP, ls.format("%H:%M:%S").to_string(), theme::bold(14.0), theme::text());
                    p.text(pos2(tx, row.top() + 33.0), Align2::LEFT_TOP, format!("{} · jusqu'à {}", human(cl.secs()), local(cl.end).format("%H:%M:%S")), FontId::proportional(11.5), theme::muted());
                    if is_cur {
                        let (state, color) = if pb.active {
                            ("en lecture", theme::accent())
                        } else if pb.paused {
                            ("en pause", theme::muted())
                        } else {
                            ("dernière relue", theme::faint())
                        };
                        p.text(pos2(tx, row.top() + 50.0), Align2::LEFT_TOP, state, theme::bold(11.0), color);
                    }
                    // Export de cette séquence (au survol).
                    if hover || is_cur {
                        let er = Rect::from_center_size(pos2(row.right() - 22.0, row.center().y), vec2(30.0, 30.0));
                        let e = ui.interact(er, Id::unique(("rec-export", &c.id, cl.start.timestamp())), Sense::click()).on_hover_text("Exporter cette séquence (.mp4)");
                        if e.hovered() {
                            p.circle_filled(er.center(), 15.0, theme::raised_hi());
                        }
                        icons::paint(&p, Rect::from_center_size(er.center(), vec2(15.0, 15.0)), Icon::Download, if e.hovered() { theme::text() } else { theme::muted() });
                        if e.clicked() {
                            export = Some(cl);
                        }
                    }
                    if hover {
                        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    }
                }
                if resp.clicked() && export.is_none() {
                    play = Some(i);
                }
                if follow == Some(cl.start.timestamp()) {
                    resp.scroll_to_me(Some(Align::Center));
                    self.cams.rec.scrolled_to = follow;
                }
            }
        });
        self.cams.rec.thumbs.lock().unwrap().wanted = wanted;
        if let (Some(i), Some(d)) = (play, day) {
            self.rec_play(c, date, d, i);
        }
        if let Some(clip) = export {
            self.rec_export(c, clip);
        }
    }
}

/// Les 7 jours de la bande : la semaine qui finit aujourd'hui, ou qui entoure le jour choisi.
fn strip_days(date: NaiveDate, today: NaiveDate) -> Vec<NaiveDate> {
    let end = if (today - date).num_days() < 7 { today } else { date.checked_add_signed(chrono::Duration::days(3)).unwrap_or(date).min(today) };
    (0..7).rev().filter_map(|k| end.checked_sub_signed(chrono::Duration::days(k))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_stays_inside_the_day() {
        let total = 86_400.0;
        let v = zoom_view((0.0, total), 0.5, 43_200.0, total);
        assert_eq!(v, (21_600.0, 64_800.0));
        // Zoom maximal : 5 minutes, jamais en dehors de la journée.
        let v = zoom_view((86_000.0, 86_400.0), 0.01, 86_390.0, total);
        assert_eq!(v.1 - v.0, MIN_SPAN);
        assert!(v.1 <= total && v.0 >= 0.0);
        assert_eq!(clamp_view(-50.0, 600.0, total), (0.0, 600.0));
    }

    #[test]
    fn labels_in_french() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        assert_eq!(day_label(today, today), "Aujourd'hui");
        assert_eq!(day_label(NaiveDate::from_ymd_opt(2026, 10, 19).unwrap(), today), "Hier");
        assert_eq!(day_label(NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(), today), "sam. 17");
        assert_eq!(long_date(today), "mardi 20 octobre");
        assert_eq!(human(44), "44 s");
        assert_eq!(human(65), "1 min 05");
        assert_eq!(human(7380), "2 h 03");
        assert_eq!(badge(44), "0:44");
    }

    #[test]
    fn strip_follows_the_chosen_day() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        let s = strip_days(today, today);
        assert_eq!((s[0], s[6]), (NaiveDate::from_ymd_opt(2026, 10, 14).unwrap(), today));
        let old = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let s = strip_days(old, today);
        assert!(s.contains(&old));
        assert_eq!(s[6], NaiveDate::from_ymd_opt(2026, 10, 4).unwrap());
    }
}
