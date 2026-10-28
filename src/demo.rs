//! Mode démo, commun à l'application et à la TUI : appareils et caméras simulés,
//! configuration temporaire, historique d'énergie et programmes d'exemple.

use crate::config::Config;
use crate::discovery;
use crate::hub::Hub;
use crate::protocol::{codec, Version, PORT_TCP};
use crate::sim;
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

/// Code de sécurité des caméras simulées.
pub const CAM_CODE: &str = "L2A3B4C5";
/// Robot Valetudo simulé de la démo.
pub const VALETUDO_ID: &str = "vac-demo-valetudo";

/// Démarre les appareils simulés et écrit une configuration temporaire qui les décrit.
pub async fn prepare() -> anyhow::Result<PathBuf> {
    // Dossier à part : la démo a sa propre configuration, son journal et son historique.
    prepare_in(std::env::temp_dir().join("homesmart-demo")).await
}

/// Comme `prepare`, dans le dossier indiqué (vidé au préalable).
pub async fn prepare_in(dir: PathBuf) -> anyhow::Result<PathBuf> {
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("homesmart.json");
    let mut cfg = Config::default();
    for (spec, mut dev) in sim::demo_devices() {
        dev.port = Some(sim::spawn(spec, "127.0.0.1:0").await?);
        cfg.devices.push(dev);
    }
    let mut rooms: Vec<String> = cfg.devices.iter().filter_map(|d| d.room.clone()).collect();
    rooms.sort();
    rooms.dedup();
    cfg.house = crate::house::HousePlan::from_room_names(&rooms);
    // Deux caméras Imou simulées (vidéo H.264 encodée en direct, orientation ONVIF).
    for (i, (name, room)) in [("Salon", "Salon"), ("Entrée", "Entrée")].into_iter().enumerate() {
        // HOMESMART_DEMO_HD=2560x1440@60 : flux HD simulé plus exigeant (mesures).
        let hd = std::env::var("HOMESMART_DEMO_HD").ok().and_then(|v| {
            let (res, fps) = v.split_once('@')?;
            let (w, h) = res.split_once('x')?;
            Some(crate::camera::sim::SimEncoder { width: w.parse().ok()?, height: h.parse().ok()?, fps: fps.parse().ok()?, kbps: 8000 })
        });
        let cam = match hd {
            Some(main) => crate::camera::sim::start_with(CAM_CODE, i as u32, main).await?,
            None => crate::camera::sim::start(CAM_CODE, i as u32).await?,
        };
        let mut c = crate::camera::CameraConfig::new("127.0.0.1");
        (c.rtsp_port, c.onvif_port, c.password) = (cam.rtsp_port, cam.onvif_port, CAM_CODE.into());
        (c.name, c.room) = (name.into(), Some(room.into()));
        (c.model, c.serial) = (Some(crate::camera::sim::MODEL.into()), Some(cam.state.serial.clone()));
        c.id = c.make_id();
        cfg.cameras.push(c);
    }
    // Un robot sous Valetudo (API locale), simulé : le chemin des robots hors Tuya.
    let (port, robot) = crate::vacuum::sim::start_valetudo("L10s Ultra (simulé)", None).await?;
    let link = crate::vacuum::Link::Valetudo { host: "127.0.0.1".into(), port: Some(port), user: None, password: None };
    let vac = crate::vacuum::VacuumConfig { id: VALETUDO_ID.into(), name: "Aspirateur étage".into(), room: Some("Chambre".into()), link, model: None };
    crate::vacuum::sim::register(VALETUDO_ID, robot);
    cfg.vacuums.push(vac);
    std::env::set_var("HOMESMART_CAPTURES", dir.join("captures"));
    cfg.save(&path)?;

    // Un appareil « pas encore configuré » qui s'annonce, pour essayer l'ajout.
    let spec = sim::SimSpec {
        id: sim::DEMO_NEW_ID.into(),
        key: codec::parse_key(sim::DEMO_NEW_KEY)?,
        version: Version::V33,
        dps: json!({ "1": false, "9": 0 }),
        watts: 0,
    };
    if sim::spawn(spec, &format!("127.0.0.1:{PORT_TCP}")).await.is_ok() {
        tokio::spawn(async {
            let Ok(sock) = tokio::net::UdpSocket::bind("127.0.0.1:0").await else { return };
            let msg = discovery::build_announce(sim::DEMO_NEW_ID, "127.0.0.1", "3.3");
            loop {
                let _ = sock.send_to(&msg, "127.0.0.1:6667").await;
                tokio::time::sleep(Duration::from_secs(4)).await;
            }
        });
        tracing::info!("démo : l'appareil à ajouter a pour clé locale {}", sim::DEMO_NEW_KEY);
    }
    Ok(path)
}

/// Historique d'énergie d'exemple (avant `Hub::start`).
pub fn seed_energy(hub: &Hub) {
    for (spec, dev) in sim::demo_devices() {
        if spec.watts > 0 {
            hub.seed_demo_energy(&dev.id, spec.watts as f64 / 10.0);
        }
    }
}

/// Pannes simulées en série (variable HOMESMART_DEMO_PANNES, captures) : bouton de la Prise TV,
/// chauffage allumé par son bouton, sous-tension et tension basse, caméra Salon sans image, arrêt
/// brutal de ce PC. La détection, elle, est la vraie.
pub async fn incidents(hub: std::sync::Arc<Hub>) {
    use crate::hub::{Level, Source};
    let ctl = |id: &str| sim::control(id);
    // Pas de notification sur le bureau pendant une capture : on croirait à une vraie panne.
    hub.set_prefs(crate::config::Prefs { os_notifications: false, ..hub.prefs() });
    tokio::time::sleep(Duration::from_secs(3)).await;
    hub.log(Level::Info, Source::User, "", "Démo : pannes simulées (HOMESMART_DEMO_PANNES)");
    if let Some(tv) = ctl("bf5a1c0de000000plug01") {
        tv.press_button();
        tokio::time::sleep(Duration::from_secs(1)).await;
        tv.set_fault(1 << 4);
        tv.set_voltage(190.0);
    }
    if let Some(heater) = ctl("bf5a1c0de0000heat0plug") {
        heater.press_button();
    }
    // Brosse principale de l'aspirateur bloquée.
    if let Some(robot) = crate::vacuum::sim::robot(crate::sim::DEMO_VACUUM_ID) {
        robot.lock().unwrap().set_error(Some(crate::vacuum::ErrorKind::MainBrush));
    }
    if let Some(cam) = hub.cameras().into_iter().find(|c| c.name == "Salon").and_then(|c| crate::camera::sim::by_port(c.rtsp_port)) {
        cam.down.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    hub.demo_host_shutdown();
}

/// Programmes d'exemple (après `Hub::start`).
pub fn seed_schedules(hub: &Hub) {
    use crate::automation::{Action, Schedule};
    let heater = "bf5a1c0de0000heat0plug";
    let week = [true, true, true, true, true, false, false];
    for (days, start, end, action, dev) in [
        (week, "06:30", Some("08:00"), Action::On, heater),
        ([true; 7], "18:30", Some("22:00"), Action::On, heater),
        ([true; 7], "23:30", None, Action::Off, "bf5a1c0de000000plug01"),
        ([false, false, false, false, false, true, true], "09:00", Some("11:30"), Action::On, "bf5a1c0de0000000light1"),
        (week, "10:00", None, Action::Clean, crate::sim::DEMO_VACUUM_ID),
    ] {
        let s = Schedule { id: 0, device: dev.into(), enabled: true, days, start: start.into(), end: end.map(str::to_string), action };
        let _ = hub.save_schedule(s);
    }
    // Automatisme de l'aspirateur : nettoyer quand le mode absence est activé.
    let s = crate::automation::Scenario {
        id: 0,
        enabled: true,
        camera: String::new(),
        trigger: crate::automation::Trigger::AwayOn,
        device: crate::sim::DEMO_VACUUM_ID.into(),
        action: Action::Clean,
        during: None,
    };
    let _ = hub.add_scenario(s, crate::hub::Source::User);
}
