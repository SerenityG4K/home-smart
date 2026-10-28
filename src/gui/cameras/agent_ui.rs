//! Réglages d'une caméra HomeSmart (fabriquée soi-même), lus et écrits par son agent : état de
//! l'encodage, définition, cadence et débit des deux flux, micro, image retournée.

use super::super::{theme, widgets, App, Toast};
use eframe::egui::{self, RichText, Ui};
use homesmart::camera::agent::AgentState;
use homesmart::hub::{CameraView, Source};
use homesmart_cam_proto::{Caps, Settings, StreamSettings, VideoCodec, RESOLUTIONS};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// État et réglages relus pendant que le panneau est affiché.
const REFRESH: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(in crate::gui) struct AgentUi {
    slot: Arc<Mutex<Slot>>,
}

#[derive(Default)]
struct Slot {
    id: String,
    loading: bool,
    saving: bool,
    error: Option<String>,
    state: Option<AgentState>,
    /// Réglages en cours de modification (égaux à ceux de la caméra s'il n'y a rien à appliquer).
    edit: Option<Settings>,
    loaded_at: Option<Instant>,
}

fn duration(s: u64) -> String {
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min", s / 60),
        3600..=86_399 => format!("{} h {:02}", s / 3600, s % 3600 / 60),
        _ => format!("{} j {} h", s / 86_400, s % 86_400 / 3600),
    }
}

fn resolution_name(w: u32, h: u32) -> String {
    match RESOLUTIONS.iter().find(|(rw, rh, _)| (*rw, *rh) == (w, h)) {
        Some((_, _, name)) => format!("{name} ({w}×{h})"),
        None => format!("{w}×{h}"),
    }
}

/// Choix de définition (parmi `RESOLUTIONS`, dans la limite `max`) ; renvoie vrai si changé.
fn resolution_row(ui: &mut Ui, id: &str, s: &mut StreamSettings, max: (u32, u32)) -> bool {
    let before = (s.width, s.height);
    ui.horizontal(|ui| {
        ui.add_sized(egui::vec2(80.0, 18.0), egui::Label::new(RichText::new("Définition").size(12.5).color(theme::muted())));
        egui::ComboBox::from_id_salt(id).selected_text(resolution_name(s.width, s.height)).show_ui(ui, |ui| {
            for (w, h, _) in RESOLUTIONS.iter().filter(|(w, h, _)| *w <= max.0 && *h <= max.1) {
                if ui.selectable_label((s.width, s.height) == (*w, *h), resolution_name(*w, *h)).clicked() {
                    (s.width, s.height) = (*w, *h);
                }
            }
        });
    });
    (s.width, s.height) != before
}

/// Cadence, débit et (si `codecs` en propose plusieurs) codec d'un flux ; renvoie vrai si la
/// cadence ou le codec a changé (le débit conseillé suit alors).
fn stream_rows(ui: &mut Ui, s: &mut StreamSettings, max_fps: u32, kbps_max: u32, codecs: &[VideoCodec]) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.add_sized(egui::vec2(80.0, 18.0), egui::Label::new(RichText::new("Images/s").size(12.5).color(theme::muted())));
        ui.spacing_mut().slider_width = (ui.available_width() - 56.0).max(60.0);
        changed |= ui.add(egui::Slider::new(&mut s.fps, 1..=max_fps.max(1))).changed();
    });
    ui.horizontal(|ui| {
        ui.add_sized(egui::vec2(80.0, 18.0), egui::Label::new(RichText::new("Débit").size(12.5).color(theme::muted())));
        ui.add(egui::DragValue::new(&mut s.kbps).range(100..=kbps_max).speed(20.0).suffix(" kbit/s"));
        let advised = Caps::suggested_kbps(s.width, s.height, s.fps, s.codec).min(kbps_max);
        if s.kbps != advised && ui.small_button("conseillé").on_hover_text(format!("{advised} kbit/s pour cette définition")).clicked() {
            s.kbps = advised;
        }
    });
    if codecs.len() > 1 {
        ui.horizontal(|ui| {
            ui.add_sized(egui::vec2(80.0, 18.0), egui::Label::new(RichText::new("Codec").size(12.5).color(theme::muted())));
            for c in codecs {
                if ui.add(egui::Button::selectable(s.codec == *c, c.label())).clicked() && s.codec != *c {
                    s.codec = *c;
                    changed = true;
                }
            }
        });
    }
    changed
}

impl App {
    fn load_agent(&mut self, id: &str, keep_edit: bool) {
        let slot = self.cams.agent.slot.clone();
        {
            let mut s = slot.lock().unwrap();
            if s.id != id {
                *s = Slot { id: id.to_string(), ..Slot::default() };
            }
            s.loading = true;
            s.loaded_at = Some(Instant::now());
        }
        let (hub, ctx, id) = (self.hub.clone(), self.ctx.clone(), id.to_string());
        self.rt.spawn(async move {
            let r = hub.camera_agent(&id).await;
            let mut s = slot.lock().unwrap();
            if s.id != id {
                return;
            }
            s.loading = false;
            match r {
                Ok(st) => {
                    // Une modification en cours est gardée ; sinon on montre les réglages de la caméra.
                    let pending = keep_edit && s.edit.is_some() && s.state.as_ref().map(|o| &o.settings) != s.edit.as_ref();
                    if !pending {
                        s.edit = Some(st.settings.clone());
                    }
                    (s.state, s.error) = (Some(st), None);
                }
                Err(e) => s.error = Some(format!("{e:#}")),
            }
            ctx.request_repaint();
        });
    }

    /// Section « Caméra HomeSmart » des réglages.
    pub(super) fn agent_section(&mut self, ui: &mut Ui, c: &CameraView) {
        let (stale, due) = {
            let s = self.cams.agent.slot.lock().unwrap();
            (s.id != c.id, !s.loading && !s.saving && s.loaded_at.is_none_or(|t| t.elapsed() > REFRESH))
        };
        if stale || due {
            self.load_agent(&c.id, !stale);
        }
        self.ctx.request_repaint_after(REFRESH);
        widgets::section_title(ui, "Caméra HomeSmart", None);
        let slot = self.cams.agent.slot.clone();
        let mut apply: Option<(Settings, Settings)> = None;
        let mut reload = false;
        {
            let mut guard = slot.lock().unwrap();
            let s = &mut *guard;
            let Some(state) = s.state.clone() else {
                if let Some(e) = &s.error {
                    ui.label(RichText::new(format!("Agent injoignable : {e}")).size(12.5).color(theme::warn()));
                    if ui.button("Réessayer").clicked() {
                        reload = true;
                    }
                } else {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Lecture de la caméra…").size(12.5).color(theme::muted()));
                    });
                }
                drop(guard);
                if reload {
                    self.load_agent(&c.id, false);
                }
                return;
            };
            let st = &state.status;
            if let Some(e) = &s.error {
                ui.label(RichText::new(format!("Dernière lecture ratée : {e}")).size(12.0).color(theme::warn()));
            }
            if !st.server.running {
                let why = st.server.last_error.as_deref().unwrap_or("arrêté");
                ui.label(RichText::new(format!("✖  Serveur vidéo de la caméra : {why}")).size(12.5).color(theme::crit()));
            } else if st.pipeline.running {
                let since = st.pipeline.since_s.map(|s| format!(" depuis {}", duration(s))).unwrap_or_default();
                ui.label(RichText::new(format!("●  Image et son en direct{since}")).size(12.5).color(theme::accent()));
            } else {
                let why = st.pipeline.last_error.as_deref().unwrap_or("arrêté");
                ui.label(RichText::new(format!("✖  Encodage arrêté : {why}")).size(12.5).color(theme::crit()));
            }
            let mut sys = Vec::new();
            if let Some(t) = st.cpu_temp_c {
                sys.push(format!("processeur {t:.0} °C"));
            }
            if let Some(l) = st.load {
                sys.push(format!("charge {l:.1}"));
            }
            sys.push(format!("allumée depuis {}", duration(st.uptime_s)));
            if st.pipeline.restarts > 0 {
                sys.push(format!("{} relance(s) de l'encodage", st.pipeline.restarts));
            }
            ui.label(RichText::new(sys.join(" · ")).size(12.0).color(theme::muted()));
            let i = &state.info;
            let sensor = i.caps.sensor.as_deref().map(|s| format!(" · capteur {s}")).unwrap_or_default();
            ui.label(RichText::new(format!("{}{sensor} · agent {}", i.board, i.version)).size(11.5).color(theme::faint()));
            ui.add_space(8.0);

            let caps = &i.caps;
            let edit = s.edit.get_or_insert_with(|| state.settings.clone());
            ui.label(RichText::new("Flux HD").font(theme::bold(13.0)));
            let mut retune = resolution_row(ui, "agent-main-res", &mut edit.main, (caps.max_width, caps.max_height));
            retune |= stream_rows(ui, &mut edit.main, caps.max_fps, 40_000, &caps.codecs);
            if retune {
                edit.main.kbps = Caps::suggested_kbps(edit.main.width, edit.main.height, edit.main.fps, edit.main.codec);
            }
            ui.add_space(6.0);
            ui.label(RichText::new("Flux fluide (vignettes, analyse IA)").font(theme::bold(13.0)));
            // Flux fluide : au plus 1080p (et jamais plus que le flux HD).
            let sub_max = (edit.main.width.min(1920), edit.main.height.min(1080));
            let mut retune = resolution_row(ui, "agent-sub-res", &mut edit.sub, sub_max);
            // Le flux fluide reste en H.264 : décodé sans FFmpeg, et léger pour l'analyse.
            retune |= stream_rows(ui, &mut edit.sub, edit.main.fps, 8_000, &[VideoCodec::H264]);
            if retune {
                edit.sub.kbps = Caps::suggested_kbps(edit.sub.width, edit.sub.height, edit.sub.fps, edit.sub.codec).min(8_000);
            }
            ui.add_space(6.0);
            ui.label(RichText::new("Micro").font(theme::bold(13.0)));
            if caps.audio {
                ui.checkbox(&mut edit.audio.enabled, "Micro activé");
                if edit.audio.enabled {
                    ui.horizontal(|ui| {
                        ui.add_sized(egui::vec2(80.0, 18.0), egui::Label::new(RichText::new("Gain").size(12.5).color(theme::muted())));
                        ui.spacing_mut().slider_width = (ui.available_width() - 64.0).max(60.0);
                        ui.add(egui::Slider::new(&mut edit.audio.gain_db, -20.0..=30.0).fixed_decimals(0).suffix(" dB"));
                    });
                }
                ui.label(RichText::new("Son en PCM 16 bits à 16 kHz : sans perte, au format de l'écoute IA.").size(11.5).color(theme::faint()));
            } else {
                ui.label(RichText::new("Aucun micro détecté sur la caméra (USB ou I²S).").size(12.5).color(theme::faint()));
            }
            ui.add_space(4.0);
            ui.checkbox(&mut edit.rotate_180, "Image retournée (caméra fixée au plafond)");
            ui.add_space(8.0);

            let invalid = edit.validate(caps).err();
            if let Some(e) = &invalid {
                ui.label(RichText::new(e).size(12.0).color(theme::warn()));
            }
            let changed = *edit != state.settings;
            ui.horizontal(|ui| {
                if s.saving {
                    ui.spinner();
                    ui.label(RichText::new("Envoi… la caméra relance son encodage").size(12.5).color(theme::muted()));
                } else {
                    if ui.add_enabled(changed && invalid.is_none(), widgets::primary_button("Appliquer", theme::accent())).clicked() {
                        apply = Some((state.settings.clone(), edit.clone()));
                    }
                    if changed && ui.button("Annuler").clicked() {
                        *edit = state.settings.clone();
                    }
                    if ui.button("Actualiser").clicked() {
                        reload = true;
                    }
                }
            });
            if apply.is_some() {
                s.saving = true;
            }
        }
        if reload {
            self.load_agent(&c.id, true);
        }
        if let Some((before, mut after)) = apply {
            // La caméra s'annonce sous le nom qu'elle porte dans HomeSmart.
            after.name = c.name.chars().take(64).collect::<String>().trim().to_string();
            if after.name.is_empty() {
                after.name = before.name.clone();
            }
            let (hub, toasts, ctx, id, name) = (self.hub.clone(), self.toasts.clone(), self.ctx.clone(), c.id.clone(), c.name.clone());
            self.rt.spawn(async move {
                let r = hub.set_camera_agent_settings(&id, before, after, Source::User).await;
                let (text, error) = {
                    let mut s = slot.lock().unwrap();
                    s.saving = false;
                    match r {
                        Ok(applied) => {
                            if let Some(st) = s.state.as_mut() {
                                st.settings = applied.clone();
                            }
                            s.edit = Some(applied);
                            (format!("Réglages envoyés à « {name} »"), false)
                        }
                        Err(e) => (format!("Réglages refusés : {e:#}"), true),
                    }
                };
                toasts.lock().unwrap().push(Toast { text, error, at: Instant::now() });
                ctx.request_repaint();
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(42), "42 s");
        assert_eq!(duration(600), "10 min");
        assert_eq!(duration(3 * 3600 + 5 * 60), "3 h 05");
        assert_eq!(duration(2 * 86_400 + 7200), "2 j 2 h");
    }
}
