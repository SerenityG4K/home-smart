//! Protection d'une caméra dans sa vue de réglages : détection de sabotage (toutes les caméras),
//! et réglages propres aux Tapo (alarme sirène / lumière, suivi des mouvements, carte SD).

use super::super::{theme, widgets, App, Toast};
use eframe::egui::{self, RichText, Ui};
use homesmart::camera::{Brand, TamperConfig, TamperSensitivity};
use homesmart::hub::Source;
use homesmart::tplink::camera::{AlarmConfig, SdCard};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// État des réglages Tapo affichés (chargés par l'API locale à l'ouverture de la section).
#[derive(Default)]
struct TapoState {
    id: String,
    loading: bool,
    error: Option<String>,
    /// Le mot de passe du compte TP-Link est enregistré (sinon : rien n'est lisible).
    has_password: bool,
    alarm: AlarmConfig,
    alarm_edit: AlarmConfig,
    track: bool,
    sd: Vec<SdCard>,
}

#[derive(Default)]
pub(in crate::gui) struct ProtectUi {
    /// Réglages de sabotage en cours d'édition : (caméra, valeurs).
    tamper: Option<(String, TamperConfig)>,
    tapo: Arc<Mutex<TapoState>>,
    pw_input: String,
    show_pw: bool,
}

impl App {
    /// Section « Détection de sabotage » (objectif masqué, caméra détournée, image floutée).
    pub(in crate::gui) fn protect_section(&mut self, ui: &mut Ui, c: &super::CameraView) {
        // Valeurs chargées depuis la configuration à la première ouverture (ou changement de caméra).
        if self.cams.protect.tamper.as_ref().is_none_or(|(id, _)| id != &c.id) {
            let t = self.hub.camera_tamper(&c.id).unwrap_or_default();
            self.cams.protect.tamper = Some((c.id.clone(), t));
        }
        let mut apply: Option<TamperConfig> = None;
        egui::CollapsingHeader::new(RichText::new("Détection de sabotage").font(theme::bold(14.0))).id_salt(("cam-tamper", &c.id)).show(ui, |ui| {
            ui.label(RichText::new("Alerte si l'objectif est masqué, la caméra détournée ou l'image floutée. L'alerte va au journal, aux problèmes et aux scénarios.").size(11.5).color(theme::faint()));
            let Some((_, t)) = self.cams.protect.tamper.as_mut() else { return };
            let mut changed = false;
            changed |= ui.checkbox(&mut t.image, "Analyser l'image dans HomeSmart").on_hover_text("Garde le flux fluide ouvert ; un peu de processeur, aucun modèle d'IA").changed();
            if c.brand.has_onvif() {
                changed |= ui.checkbox(&mut t.camera, "Utiliser les alertes de la caméra").on_hover_text("La caméra signale elle-même le sabotage (ONVIF), sans rien décoder sur le PC").changed();
            }
            ui.horizontal(|ui| {
                ui.label(RichText::new("Sensibilité").size(12.5).color(theme::muted()));
                for s in TamperSensitivity::ALL {
                    if ui.add(egui::Button::selectable(t.sensitivity == s, s.label())).clicked() && t.sensitivity != s {
                        t.sensitivity = s;
                        changed = true;
                    }
                }
            });
            if !t.image && !t.camera {
                ui.label(RichText::new("Désactivée.").size(11.5).color(theme::faint()));
            }
            if changed {
                apply = Some(*t);
            }
        });
        ui.add_space(6.0);
        if let Some(t) = apply {
            let (hub, ctx, id) = (self.hub.clone(), self.ctx.clone(), c.id.clone());
            self.rt.spawn(async move {
                let _ = hub.set_camera_tamper(&id, t, Source::User).await;
                ctx.request_repaint();
            });
        }
    }

    /// Section « Caméra Tapo » : mot de passe du compte TP-Link, alarme, suivi, carte SD.
    pub(in crate::gui) fn tapo_section(&mut self, ui: &mut Ui, c: &super::CameraView) {
        if c.brand != Brand::Tapo {
            return;
        }
        let has_password = self.hub.camera_config(&c.id).is_some_and(|cfg| cfg.tapo.as_ref().is_some_and(|s| s.is_set()));
        egui::CollapsingHeader::new(RichText::new("Caméra Tapo").font(theme::bold(14.0))).id_salt(("cam-tapo", &c.id)).default_open(!has_password).show(ui, |ui| {
            if !has_password {
                self.tapo_password_ui(ui, c);
                return;
            }
            // Chargé une fois par caméra (alarme, suivi, carte SD).
            if self.cams.protect.tapo.lock().unwrap().id != c.id {
                self.load_tapo(&c.id);
            }
            self.tapo_body_ui(ui, c);
        });
        ui.add_space(6.0);
    }

    fn tapo_password_ui(&mut self, ui: &mut Ui, c: &super::CameraView) {
        ui.label(
            RichText::new("Pour l'alarme, le suivi des mouvements et la carte SD, HomeSmart a besoin du mot de passe du compte TP-Link (celui de l'application Tapo, pas le « compte de la caméra »). Seules ses empreintes sont gardées, jamais le mot de passe.")
                .size(11.5)
                .color(theme::faint()),
        );
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.cams.protect.pw_input).password(!self.cams.protect.show_pw).hint_text("mot de passe TP-Link").desired_width(180.0));
            ui.checkbox(&mut self.cams.protect.show_pw, "voir");
        });
        let pw = self.cams.protect.pw_input.clone();
        if ui.add_enabled(!pw.trim().is_empty(), widgets::primary_button("Enregistrer", theme::accent())).clicked() {
            self.cams.protect.pw_input.clear();
            let (hub, toasts, ctx, id) = (self.hub.clone(), self.toasts.clone(), self.ctx.clone(), c.id.clone());
            self.rt.spawn(async move {
                let (text, error) = match hub.set_camera_tapo_password(&id, &pw, Source::User).await {
                    Ok(()) => ("Mot de passe TP-Link accepté par la caméra".to_string(), false),
                    Err(e) => (format!("Mot de passe refusé : {e:#}"), true),
                };
                toasts.lock().unwrap().push(Toast { text, error, at: Instant::now() });
                ctx.request_repaint();
            });
        }
    }

    fn tapo_body_ui(&mut self, ui: &mut Ui, c: &super::CameraView) {
        let slot = self.cams.protect.tapo.clone();
        let mut set_alarm: Option<AlarmConfig> = None;
        let mut trigger: Option<bool> = None;
        let mut set_track: Option<bool> = None;
        let mut reload = false;
        {
            let mut st = slot.lock().unwrap();
            if st.loading {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Lecture des réglages de la caméra…").size(12.5).color(theme::muted()));
                });
                return;
            }
            if let Some(e) = st.error.clone() {
                ui.label(RichText::new(format!("Réglages Tapo indisponibles : {e}")).size(12.5).color(theme::warn()));
                if ui.button("Réessayer").clicked() {
                    reload = true;
                }
            }
            // --- Alarme ---
            ui.label(RichText::new("Alarme").font(theme::bold(13.0)));
            let saved = st.alarm.clone();
            let a = &mut st.alarm_edit;
            ui.checkbox(&mut a.enabled, "Alarme automatique (sur détection de la caméra)");
            ui.horizontal(|ui| {
                ui.checkbox(&mut a.sound, "Sirène");
                ui.checkbox(&mut a.light, "Lumière");
            });
            if let Some(v) = a.volume.clone() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Volume").size(12.5).color(theme::muted()));
                    for opt in ["low", "normal", "high"] {
                        let name = match opt {
                            "low" => "faible",
                            "high" => "fort",
                            _ => "normal",
                        };
                        if ui.add(egui::Button::selectable(v == opt, name)).clicked() {
                            a.volume = Some(opt.to_string());
                        }
                    }
                });
            }
            let changed = *a != saved;
            ui.horizontal(|ui| {
                if ui.add_enabled(changed, widgets::primary_button("Enregistrer", theme::accent())).clicked() {
                    set_alarm = Some(a.clone());
                }
                if changed && ui.button("Annuler").clicked() {
                    *a = saved;
                }
            });
            ui.horizontal(|ui| {
                if ui.add(widgets::primary_button("Déclencher maintenant", theme::crit())).clicked() {
                    trigger = Some(true);
                }
                if ui.button("Arrêter").clicked() {
                    trigger = Some(false);
                }
            });
            ui.add_space(8.0);
            // --- Suivi des mouvements ---
            ui.label(RichText::new("Suivi des mouvements").font(theme::bold(13.0)));
            let mut track = st.track;
            if ui.checkbox(&mut track, "La caméra tourne toute seule pour suivre ce qui bouge").changed() {
                set_track = Some(track);
            }
            ui.label(RichText::new("Suivi intégré à la caméra. (Le suivi par l'IA de HomeSmart est dans l'onglet Vision.)").size(11.0).color(theme::faint()));
            // --- Carte SD ---
            if !st.sd.is_empty() {
                ui.add_space(8.0);
                ui.label(RichText::new("Carte SD").font(theme::bold(13.0)));
                for card in &st.sd {
                    ui.label(RichText::new(format!("{} — {} libres sur {}", card.status, card.video_free, card.video_total)).size(12.0).color(theme::muted()));
                }
                ui.label(RichText::new("La liste des enregistrements est dans l'onglet « Enregistrements ». La lecture et l'export passent par le flux propriétaire de la caméra, que HomeSmart ne sait pas ouvrir.").size(11.0).color(theme::faint()));
            }
        }
        if reload {
            self.load_tapo(&c.id);
        }
        if let Some(a) = set_alarm {
            self.spawn_tapo(c, move |hub, id| async move { hub.camera_set_alarm(&id, a, Source::User).await }, "Alarme enregistrée", true);
        }
        if let Some(on) = trigger {
            self.spawn_tapo(c, move |hub, id| async move { hub.camera_trigger_alarm(&id, on, Source::User).await }, if on { "Alarme déclenchée" } else { "Alarme arrêtée" }, false);
        }
        if let Some(on) = set_track {
            self.spawn_tapo(c, move |hub, id| async move { hub.camera_set_auto_track(&id, on, Source::User).await }, if on { "Suivi activé" } else { "Suivi désactivé" }, true);
        }
    }

    fn load_tapo(&mut self, id: &str) {
        *self.cams.protect.tapo.lock().unwrap() = TapoState { id: id.to_string(), loading: true, has_password: true, ..TapoState::default() };
        let (hub, slot, ctx, id) = (self.hub.clone(), self.cams.protect.tapo.clone(), self.ctx.clone(), id.to_string());
        self.rt.spawn(async move {
            let alarm = hub.camera_alarm(&id).await;
            let track = hub.camera_auto_track(&id).await.unwrap_or(false);
            let sd = hub.camera_tapo_sd(&id).await.unwrap_or_default();
            let mut st = slot.lock().unwrap();
            if st.id == id {
                st.loading = false;
                match alarm {
                    Ok(a) => {
                        st.alarm = a.clone();
                        st.alarm_edit = a;
                        st.track = track;
                        st.sd = sd;
                    }
                    Err(e) => st.error = Some(format!("{e:#}")),
                }
            }
            ctx.request_repaint();
        });
    }

    /// Lance une action Tapo, affiche un message, et recharge l'état en cas de changement.
    fn spawn_tapo<F, Fut>(&mut self, c: &super::CameraView, f: F, ok_msg: &'static str, reload: bool)
    where
        F: FnOnce(Arc<homesmart::hub::Hub>, String) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = anyhow::Result<()>> + Send,
    {
        let (hub, toasts, ctx, id, slot) = (self.hub.clone(), self.toasts.clone(), self.ctx.clone(), c.id.clone(), self.cams.protect.tapo.clone());
        self.rt.spawn(async move {
            let (text, error) = match f(hub.clone(), id.clone()).await {
                Ok(()) => (ok_msg.to_string(), false),
                Err(e) => (format!("{e:#}"), true),
            };
            if !error && reload {
                // Les appels réseau d'abord, puis un verrou bref (jamais tenu pendant un await).
                let alarm = hub.camera_alarm(&id).await.ok();
                let track = hub.camera_auto_track(&id).await.ok();
                let mut st = slot.lock().unwrap();
                if st.id == id {
                    if let Some(a) = alarm {
                        st.alarm = a.clone();
                        st.alarm_edit = a;
                    }
                    if let Some(t) = track {
                        st.track = t;
                    }
                }
            }
            toasts.lock().unwrap().push(Toast { text, error, at: Instant::now() });
            ctx.request_repaint();
        });
    }
}
