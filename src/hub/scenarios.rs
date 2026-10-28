//! Scénarios : ce que voient et entendent les caméras déclenche des appareils (« personne dans
//! la chambre depuis 30 min → éteindre le radiateur », « chute → allumer la lumière »,
//! « maison vide depuis 30 min → nettoyer avec l'aspirateur », « mode absence activé → nettoyer »).
//!
//! Un scénario sans caméra vaut pour toutes les caméras analysées : l'absence y est « personne sur
//! aucune caméra » (maison vide). Les déclencheurs du mode absence valent pour la maison entière.
//!
//! L'analyse (image ou son) signale ses événements ; l'absence se mesure ici, à partir du
//! dernier instant où quelqu'un était en vue, et seulement tant que l'analyse tourne (sans elle,
//! on ne sait pas si la pièce est vide). Chaque déclenchement est journalisé (source
//! « Scénario »), avec sa raison, puis la commande de l'appareil elle-même.

use super::{Event, Hub, Level, Source};
use crate::automation::{absence_due, Action, Scenario, Trigger, SCENARIO_COOLDOWN};
use anyhow::{anyhow, bail, Result};
use chrono::Local;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

/// Ce que les scénarios savent des caméras.
#[derive(Default)]
pub(super) struct SceneState {
    /// Caméras analysées : (analyse lancée à, dernière personne vue).
    watched: HashMap<String, (Instant, Option<Instant>)>,
    /// Scénarios d'absence déjà déclenchés pour l'absence en cours.
    absence_fired: HashSet<u64>,
    /// Dernier déclenchement de chaque scénario sur événement.
    last_fired: HashMap<u64, Instant>,
    /// Exécution des commandes (le hub tourne dans un environnement tokio).
    rt: Option<tokio::runtime::Handle>,
}

impl SceneState {
    pub(super) fn set_runtime(&mut self) {
        self.rt = tokio::runtime::Handle::try_current().ok();
    }
}

impl Hub {
    /// Moteur tokio du hub, pour lancer une tâche depuis un fil d'analyse.
    pub(super) fn scene_runtime(&self) -> Option<tokio::runtime::Handle> {
        self.scenes.lock().unwrap().rt.clone()
    }
}

impl Hub {
    pub fn scenarios(&self) -> Vec<Scenario> {
        self.config.lock().unwrap().scenarios.clone()
    }

    fn device_name(&self, id: &str) -> String {
        Some(self.name_of(id)).filter(|n| !n.is_empty()).unwrap_or_else(|| id.to_string())
    }

    /// « Salon », « Toutes les caméras » ou « Maison ».
    fn scenario_place(&self, s: &Scenario) -> String {
        s.place(self.camera_name(&s.camera))
    }

    /// Ajoute un scénario (identifiant attribué ici).
    pub fn add_scenario(&self, mut s: Scenario, source: Source) -> Result<Scenario> {
        s.validate().map_err(|e| anyhow!(e))?;
        if s.trigger.house_wide() {
            s.camera.clear();
        } else if !s.camera.is_empty() && self.camera_name(&s.camera).is_none() {
            bail!("caméra inconnue");
        }
        if self.handle(&s.device).is_none() && !self.is_vacuum(&s.device) {
            bail!("appareil inconnu");
        }
        if s.action.for_vacuum() && !self.is_vacuum(&s.device) {
            bail!("nettoyer ou renvoyer à la base : réservé aux aspirateurs robots");
        }
        {
            let mut cfg = self.config.lock().unwrap();
            s.id = cfg.scenarios.iter().map(|x| x.id).max().unwrap_or(0) + 1;
            cfg.scenarios.push(s.clone());
        }
        self.persist();
        let cam = self.scenario_place(&s);
        self.log(Level::Info, source, &s.device, format!("Scénario ajouté — {cam} : {}", s.summary(&self.device_name(&s.device))));
        self.emit(Event::Schedules);
        Ok(s)
    }

    pub fn remove_scenario(&self, id: u64, source: Source) {
        let removed = {
            let mut cfg = self.config.lock().unwrap();
            let r = cfg.scenarios.iter().find(|s| s.id == id).cloned();
            cfg.scenarios.retain(|s| s.id != id);
            r
        };
        if let Some(s) = removed {
            self.persist();
            let cam = self.scenario_place(&s);
            self.log(Level::Info, source, &s.device, format!("Scénario supprimé — {cam} : {}", s.summary(&self.device_name(&s.device))));
            self.emit(Event::Schedules);
        }
    }

    pub fn set_scenario_enabled(&self, id: u64, enabled: bool, source: Source) {
        let changed = {
            let mut cfg = self.config.lock().unwrap();
            cfg.scenarios.iter_mut().find(|s| s.id == id && s.enabled != enabled).map(|s| {
                s.enabled = enabled;
                s.clone()
            })
        };
        if let Some(s) = changed {
            self.persist();
            let cam = self.scenario_place(&s);
            let what = if enabled { "activé" } else { "désactivé" };
            self.log(Level::Info, source, &s.device, format!("Scénario {what} — {cam} : {}", s.summary(&self.device_name(&s.device))));
            self.emit(Event::Schedules);
        }
    }

    /// Une caméra commence (ou cesse) d'être analysée : l'absence ne compte que pendant l'analyse.
    pub(super) fn scene_watch(&self, camera: &str, on: bool) {
        let mut st = self.scenes.lock().unwrap();
        if st.rt.is_none() {
            st.rt = tokio::runtime::Handle::try_current().ok();
        }
        if on {
            st.watched.entry(camera.to_string()).or_insert((Instant::now(), None));
        } else {
            st.watched.remove(camera);
        }
    }

    /// Quelqu'un est en vue sur cette caméra : l'absence en cours est terminée.
    pub(super) fn scene_seen(&self, camera: &str) {
        let ids: Vec<u64> = self.config.lock().unwrap().scenarios.iter().filter(|s| s.camera == camera || s.camera.is_empty()).map(|s| s.id).collect();
        let mut st = self.scenes.lock().unwrap();
        if let Some(w) = st.watched.get_mut(camera) {
            w.1 = Some(Instant::now());
        }
        for id in ids {
            st.absence_fired.remove(&id);
        }
    }

    /// Un événement de l'analyse (arrivée, chute, appel à l'aide, son) : scénarios concernés.
    pub(super) fn scene_trigger(self: &Arc<Self>, camera: &str, trigger: Trigger) {
        let now_local = Local::now().time();
        let due: Vec<Scenario> = {
            let list = self.config.lock().unwrap().scenarios.clone();
            let mut st = self.scenes.lock().unwrap();
            list.into_iter()
                .filter(|s| (s.camera == camera || s.camera.is_empty()) && !s.trigger.house_wide() && s.trigger.same_kind(trigger) && s.active_at(now_local))
                .filter(|s| {
                    let ok = st.last_fired.get(&s.id).is_none_or(|t| t.elapsed() >= SCENARIO_COOLDOWN);
                    if ok {
                        st.last_fired.insert(s.id, Instant::now());
                    }
                    ok
                })
                .collect()
        };
        for s in due {
            self.run_scenario(s, trigger.label());
        }
    }

    /// Toutes les quelques secondes : scénarios d'absence arrivés à échéance.
    pub(super) fn scene_tick(self: &Arc<Self>) {
        let now_local = Local::now().time();
        let list = self.config.lock().unwrap().scenarios.clone();
        let due: Vec<(Scenario, u32)> = {
            let mut st = self.scenes.lock().unwrap();
            let mut out = Vec::new();
            for s in list.into_iter().filter(|s| s.active_at(now_local)) {
                let Trigger::Absence { minutes } = s.trigger else { continue };
                // Sans caméra : la maison est vide quand personne n'est vu sur aucune caméra analysée.
                let empty_for = if s.camera.is_empty() {
                    let Some(d) = st.watched.values().map(|(since, seen)| seen.unwrap_or(*since).elapsed()).min() else { continue };
                    d
                } else {
                    let Some(&(since, seen)) = st.watched.get(&s.camera) else { continue };
                    seen.unwrap_or(since).elapsed()
                };
                if absence_due(minutes, empty_for, st.absence_fired.contains(&s.id)) {
                    st.absence_fired.insert(s.id);
                    out.push((s, minutes));
                }
            }
            out
        };
        for (s, _) in due {
            let why = s.trigger.label();
            self.run_scenario(s, why);
        }
    }

    /// Le mode absence change : scénarios de la maison entière.
    pub(super) fn scene_away(self: &Arc<Self>, armed: bool) {
        let now_local = Local::now().time();
        let want = if armed { Trigger::AwayOn } else { Trigger::AwayOff };
        let due: Vec<Scenario> = self.config.lock().unwrap().scenarios.iter().filter(|s| s.trigger == want && s.active_at(now_local)).cloned().collect();
        for s in due {
            self.run_scenario(s, want.label());
        }
    }

    /// Exécute un scénario : journal (avec la raison), puis commande de l'appareil.
    fn run_scenario(self: &Arc<Self>, s: Scenario, why: String) {
        let cam = self.scenario_place(&s);
        let dev = self.device_name(&s.device);
        let on = s.action == Action::On;
        // Un appareil à risque qui demande confirmation n'est jamais allumé sans toi.
        if on && self.view(&s.device).is_some_and(|v| v.safety.enabled && v.safety.confirm_on) {
            self.log(Level::Warning, Source::Scenario, &s.device, format!("Scénario non exécuté — {cam} : {why}, mais « {dev} » demande une confirmation pour être allumé (garde-fou)"));
            return;
        }
        let what = s.action.verb();
        self.log(Level::Info, Source::Scenario, &s.device, format!("Scénario — {cam} : {why} → {what} « {dev} »"));
        let Some(rt) = self.scenes.lock().unwrap().rt.clone() else {
            self.log(Level::Warning, Source::Scenario, &s.device, "Scénario non exécuté : moteur arrêté");
            return;
        };
        let hub = self.clone();
        rt.spawn(async move {
            // La commande elle-même est journalisée (origine : scénario).
            if let Err(e) = hub.run_action(&s.device, s.action, Source::Scenario).await {
                hub.log(Level::Warning, Source::Scenario, &s.device, format!("Scénario : {what} « {dev} » impossible ({e:#})"));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::{Safety, TimeRange};
    use crate::camera::{self, CameraConfig};
    use crate::config::DeviceConfig;
    use crate::protocol::Version;
    use serde_json::json;
    use std::time::Duration;

    async fn wait_on(hub: &Hub, id: &str, on: bool) {
        let t0 = Instant::now();
        while hub.view(id).and_then(|v| v.dps.get("1").cloned()) != Some(json!(on)) {
            assert!(t0.elapsed() < Duration::from_secs(8), "l'appareil n'est jamais passé à {on}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Prise et caméra simulées : une chute allume la lampe, une pièce vide éteint le radiateur
    /// (une seule fois par absence), et un appareil qui demande confirmation n'est pas allumé.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn scenarios_drive_devices() {
        let dir = std::env::temp_dir().join(format!("homesmart-scenarios-{}", rand::random::<u32>()));
        let hub = Hub::new(dir.join("cfg.json")).unwrap();
        hub.start(false);
        let key = *b"0123456789abcdef";
        let mut ids = Vec::new();
        for (n, id) in ["bf0000000000000lampe", "bf000000000000radiat"].into_iter().enumerate() {
            let spec = crate::sim::SimSpec { id: id.into(), key, version: Version::V34, dps: json!({ "1": n == 1 }), watts: 0 };
            let port = crate::sim::spawn(spec, "127.0.0.1:0").await.unwrap();
            hub.add(DeviceConfig {
                id: id.into(),
                key: "0123456789abcdef".into(),
                name: if n == 0 { "Lampe".into() } else { "Radiateur".into() },
                ip: Some("127.0.0.1".into()),
                port: Some(port),
                version: Some("3.4".into()),
                kind: Some("plug".into()),
                ..Default::default()
            })
            .unwrap();
            ids.push(id.to_string());
        }
        let (lamp, heater) = (ids[0].clone(), ids[1].clone());
        wait_on(&hub, &heater, true).await;
        let sim = camera::sim::start("CODE1234", 0).await.unwrap();
        let mut cfg = CameraConfig::new("127.0.0.1");
        (cfg.rtsp_port, cfg.onvif_port, cfg.password) = (sim.rtsp_port, sim.onvif_port, "CODE1234".into());
        let cam = hub.add_camera(cfg, Source::User).unwrap().id;

        let all_day = TimeRange { start: "00:00".into(), end: "23:59".into() };
        let fall = Scenario { id: 0, enabled: true, camera: cam.clone(), trigger: Trigger::Fall, device: lamp.clone(), action: Action::On, during: Some(all_day) };
        hub.add_scenario(fall, Source::User).unwrap();
        let empty = Scenario { id: 0, enabled: true, camera: cam.clone(), trigger: Trigger::Absence { minutes: 1 }, device: heater.clone(), action: Action::Off, during: None };
        let empty = hub.add_scenario(empty, Source::User).unwrap();
        assert!(hub.add_scenario(Scenario { device: "inconnu".into(), ..empty.clone() }, Source::User).is_err());

        // Chute vue par l'analyse : la lampe s'allume.
        hub.scene_trigger(&cam, Trigger::Fall);
        wait_on(&hub, &lamp, true).await;

        // Pièce vide depuis plus d'une minute (horloge avancée) : radiateur éteint, une fois.
        hub.scene_watch(&cam, true);
        let back = |secs: u64| Instant::now().checked_sub(Duration::from_secs(secs)).unwrap();
        hub.scenes.lock().unwrap().watched.insert(cam.clone(), (back(90), None));
        hub.scene_tick();
        wait_on(&hub, &heater, false).await;
        hub.power(&heater, true, None, Source::User).await.unwrap();
        hub.scene_tick();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(hub.view(&heater).unwrap().dps["1"], json!(true), "une seule extinction par absence");
        // Quelqu'un passe, puis la pièce se vide de nouveau : nouvelle extinction.
        hub.scene_seen(&cam);
        hub.scenes.lock().unwrap().watched.insert(cam.clone(), (back(300), Some(back(70))));
        hub.scene_tick();
        wait_on(&hub, &heater, false).await;

        // Garde-fou « confirmation à l'allumage » : un scénario ne l'allume jamais.
        hub.set_safety(&heater, Safety { enabled: true, confirm_on: true, ..Default::default() }).unwrap();
        hub.add_scenario(Scenario { id: 0, enabled: true, camera: cam.clone(), trigger: Trigger::Arrival, device: heater.clone(), action: Action::On, during: None }, Source::User).unwrap();
        hub.scene_trigger(&cam, Trigger::Arrival);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(hub.view(&heater).unwrap().dps["1"], json!(false));

        let log = hub.alerts(100);
        let has = |src: Source, txt: &str| log.iter().any(|a| a.source == src && a.message.contains(txt));
        assert!(has(Source::User, "Scénario ajouté"), "{log:#?}");
        assert!(has(Source::Scenario, "une chute → allumer « Lampe »"), "{log:#?}");
        assert!(has(Source::Scenario, "personne depuis 1 min → éteindre « Radiateur »"), "{log:#?}");
        assert!(has(Source::Scenario, "demande une confirmation"), "{log:#?}");
        // Supprimer la caméra supprime ses scénarios.
        hub.remove_camera(&cam, Source::User).unwrap();
        assert!(hub.scenarios().is_empty());
        hub.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }
}
