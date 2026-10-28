//! Appareils Tuya simulés (côté appareil du protocole) pour la démo et les tests.

use crate::config::DeviceConfig;
use crate::protocol::codec::{self, Codec};
use crate::protocol::crypto::{self, Key};
use crate::protocol::frame;
use crate::protocol::{cmd, Version};
use anyhow::{bail, Result};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct SimSpec {
    pub id: String,
    pub key: Key,
    pub version: Version,
    pub dps: Value,
    /// Puissance consommée allumé, en dixièmes de W (prises à mesure).
    pub watts: u32,
}

/// Commandes d'un appareil simulé, pour provoquer ce qui arrive aux vraies prises (démo, essais) :
/// bouton physique, coupure de courant, défaut signalé, tension, appareil branché qui ne tire plus.
#[derive(Clone)]
pub struct SimCtl {
    dps: Arc<Mutex<Map<String, Value>>>,
    /// États poussés aux clients connectés (comme la vraie prise après un appui sur son bouton).
    push: broadcast::Sender<Map<String, Value>>,
    /// Coupe toutes les connexions (coupure de courant).
    kill: broadcast::Sender<()>,
    /// Sans courant : les connexions sont refusées.
    down: Arc<AtomicBool>,
    /// Puissance tirée par l'appareil branché quand la prise est allumée (dixièmes de W).
    load: Arc<AtomicU32>,
    /// Tension du secteur (dixièmes de V).
    voltage: Arc<AtomicU32>,
    /// Minuterie en cours (une nouvelle annule la précédente).
    timer: Arc<AtomicU64>,
    /// Robot aspirateur : son « cerveau » (nettoyage, batterie, retour à la base).
    vacuum: Option<crate::vacuum::sim::Shared>,
}

fn registry() -> &'static Mutex<HashMap<String, SimCtl>> {
    static R: OnceLock<Mutex<HashMap<String, SimCtl>>> = OnceLock::new();
    R.get_or_init(Mutex::default)
}

/// Commandes de l'appareil simulé `id` (démo).
pub fn control(id: &str) -> Option<SimCtl> {
    registry().lock().unwrap().get(id).cloned()
}

impl SimCtl {
    fn on(&self) -> bool {
        self.dps.lock().unwrap().get("1").and_then(Value::as_bool).unwrap_or(false)
    }

    /// Mesures cohérentes avec l'état (prise à mesure seulement).
    fn meter(&self, st: &mut Map<String, Value>, out: &mut Map<String, Value>) {
        if !st.contains_key("19") {
            return;
        }
        let on = st.get("1").and_then(Value::as_bool).unwrap_or(false);
        let load = self.load.load(Ordering::Relaxed);
        let power = if on { load + rand::random::<u32>() % (load / 25 + 1) } else { 0 };
        let v = self.voltage.load(Ordering::Relaxed).max(1);
        for (k, val) in [("19", json!(power)), ("18", json!(power as u64 * 1000 / v as u64)), ("20", json!(v))] {
            st.insert(k.into(), val.clone());
            out.insert(k.into(), val);
        }
    }

    /// Écrit des DP comme le ferait la prise elle-même, et les pousse aux clients.
    pub fn set(&self, m: Map<String, Value>) {
        let mut out = m.clone();
        {
            let mut st = self.dps.lock().unwrap();
            for (k, v) in &m {
                st.insert(k.clone(), v.clone());
            }
            if m.contains_key("1") {
                self.meter(&mut st, &mut out);
            }
        }
        if let Some(v) = m.get("9").and_then(Value::as_u64) {
            self.arm_timer(v);
        }
        let _ = self.push.send(out);
    }

    /// Appui sur le bouton de la prise : elle bascule.
    pub fn press_button(&self) {
        let on = !self.on();
        let mut m = Map::new();
        m.insert("1".into(), json!(on));
        self.set(m);
    }

    /// Appareil branché : ce qu'il tire allumé (W), par exemple 25 W pour un radiateur dont le
    /// thermostat ne chauffe pas.
    pub fn set_load(&self, watts: f64) {
        self.load.store((watts * 10.0).round().max(0.0) as u32, Ordering::Relaxed);
        if self.on() {
            let mut m = Map::new();
            m.insert("1".into(), json!(true));
            self.set(m);
        }
    }

    /// Tension du secteur (V), poussée tout de suite et reprise aux mesures suivantes.
    pub fn set_voltage(&self, volts: f64) {
        let dv = (volts * 10.0).round() as u32;
        self.voltage.store(dv, Ordering::Relaxed);
        let mut m = Map::new();
        m.insert("20".into(), json!(dv));
        self.set(m);
    }

    /// Défauts signalés (DP 26, bits : surintensité, surtension, surpuissance, sous-intensité,
    /// sous-tension, sous-puissance).
    pub fn set_fault(&self, bits: u32) {
        let mut m = Map::new();
        m.insert("26".into(), json!(bits));
        self.set(m);
    }

    /// Coupure de courant de `secs` : plus de liaison, puis la prise redémarre comme une vraie,
    /// selon « Après une coupure de courant » (DP 38) ; sa minuterie est perdue.
    pub async fn power_cut(&self, secs: f64) {
        self.down.store(true, Ordering::Relaxed);
        self.timer.fetch_add(1, Ordering::Relaxed);
        let _ = self.kill.send(());
        tokio::time::sleep(Duration::from_secs_f64(secs)).await;
        {
            let mut st = self.dps.lock().unwrap();
            let after = st.get("38").and_then(Value::as_str).unwrap_or("memory").to_string();
            let on = match after.as_str() {
                "off" | "power_off" | "0" => false,
                "on" | "power_on" | "1" => true,
                _ => st.get("1").and_then(Value::as_bool).unwrap_or(false),
            };
            st.insert("1".into(), json!(on));
            for k in ["9", "26"] {
                if st.contains_key(k) {
                    st.insert(k.into(), json!(0));
                }
            }
            let mut ignored = Map::new();
            self.meter(&mut st, &mut ignored);
        }
        self.down.store(false, Ordering::Relaxed);
    }

    /// Minuterie de la prise : au bout de `secs`, elle bascule (0 = annulée).
    fn arm_timer(&self, secs: u64) {
        let gen = self.timer.fetch_add(1, Ordering::Relaxed) + 1;
        if secs == 0 {
            return;
        }
        let ctl = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            if ctl.timer.load(Ordering::Relaxed) != gen {
                return;
            }
            let mut m = Map::new();
            m.insert("1".into(), json!(!ctl.on()));
            m.insert("9".into(), json!(0));
            ctl.timer.fetch_add(1, Ordering::Relaxed);
            ctl.set(m);
        });
    }
}

/// Robot aspirateur Tuya : un cerveau qui fait évoluer ses DP (nettoyage, batterie, retour), poussés
/// aux clients connectés comme le ferait le vrai robot.
fn vacuum_brain(id: &str, ctl: &SimCtl) -> crate::vacuum::sim::Shared {
    use crate::vacuum::sim::{self as vsim, SimRobot};
    use crate::vacuum::Activity;
    let brain = {
        // État de départ : celui des DP donnés.
        let dps = ctl.dps.lock().unwrap();
        let mut b = SimRobot::new(dps.get("14").and_then(Value::as_str).unwrap_or("normal"));
        if let Some(n) = dps.get("6").and_then(Value::as_f64) {
            b.battery = n;
        }
        b.activity = match dps.get("5").and_then(Value::as_str) {
            Some("smart_clean") => Activity::Cleaning,
            Some("charging") => Activity::Charging,
            Some("standby" | "sleep") => Activity::Idle,
            _ => Activity::Docked,
        };
        Arc::new(Mutex::new(b))
    };
    vsim::register(id, brain.clone());
    let (dps, push, b) = (ctl.dps.clone(), ctl.push.clone(), brain.clone());
    vsim::spawn_clock(brain.clone(), move || {
        let now = b.lock().unwrap().tuya_dps();
        let diff: Map<String, Value> = {
            let mut st = dps.lock().unwrap();
            let diff: Map<String, Value> = now.into_iter().filter(|(k, v)| st.get(k) != Some(v)).collect();
            for (k, v) in &diff {
                st.insert(k.clone(), v.clone());
            }
            diff
        };
        if !diff.is_empty() {
            let _ = push.send(diff);
        }
    });
    brain
}

/// Démarre un appareil simulé sur `addr` (port 0 = port libre). Renvoie le port.
pub async fn spawn(spec: SimSpec, addr: &str) -> Result<u16> {
    Ok(spawn_ctl(spec, addr).await?.0)
}

/// Comme `spawn`, avec les commandes de l'appareil (pannes simulées).
pub async fn spawn_ctl(spec: SimSpec, addr: &str) -> Result<(u16, SimCtl)> {
    let listener = TcpListener::bind(addr).await?;
    let port = listener.local_addr()?.port();
    let dps = Arc::new(Mutex::new(spec.dps.as_object().cloned().unwrap_or_default()));
    let volts = dps.lock().unwrap().get("20").and_then(Value::as_u64).filter(|v| *v > 0).unwrap_or(2300) as u32;
    let ctl = SimCtl {
        dps,
        push: broadcast::channel(32).0,
        kill: broadcast::channel(4).0,
        down: Arc::default(),
        load: Arc::new(AtomicU32::new(spec.watts)),
        voltage: Arc::new(AtomicU32::new(volts)),
        timer: Arc::default(),
        vacuum: None,
    };
    let mut ctl = ctl;
    if crate::vacuum::tuya::looks_like(&ctl.dps.lock().unwrap()) {
        ctl.vacuum = Some(vacuum_brain(&spec.id, &ctl));
    }
    registry().lock().unwrap().insert(spec.id.clone(), ctl.clone());
    let c = ctl.clone();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            if c.down.load(Ordering::Relaxed) {
                // Sans courant : rien ne répond.
                drop(sock);
                continue;
            }
            let (spec, c) = (spec.clone(), c.clone());
            tokio::spawn(async move {
                if let Err(e) = serve_conn(sock, spec, c).await {
                    tracing::debug!("simulateur : {e:#}");
                }
            });
        }
    });
    Ok((port, ctl))
}

fn status_body(version: Version, id: &str, dps: &Map<String, Value>) -> Vec<u8> {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let v = if version.needs_session() {
        json!({ "protocol": 4, "t": t, "data": { "dps": dps } })
    } else {
        json!({ "devId": id, "dps": dps, "t": t })
    };
    v.to_string().into_bytes()
}

/// Effets de bord réalistes (mesures de la prise, position du volet…).
fn apply(state: &mut Map<String, Value>, req: &Map<String, Value>, ctl: &SimCtl) -> Map<String, Value> {
    let mut changed = req.clone();
    for (k, v) in req {
        state.insert(k.clone(), v.clone());
    }
    // Robot aspirateur : la commande agit sur son cerveau, qui donne le nouvel état.
    if let Some(brain) = &ctl.vacuum {
        let mut b = brain.lock().unwrap();
        if b.apply_tuya(req) {
            for (k, v) in b.tuya_dps() {
                if state.get(&k) != Some(&v) || req.contains_key(&k) {
                    state.insert(k.clone(), v.clone());
                    changed.insert(k, v);
                }
            }
            return changed;
        }
    }
    if req.contains_key("1") {
        ctl.meter(state, &mut changed);
    }
    if let Some(Value::String(c)) = req.get("1") {
        let pos = match c.as_str() {
            "open" => Some(100),
            "close" => Some(0),
            _ => None,
        };
        if let Some(p) = pos {
            if state.contains_key("2") {
                changed.insert("2".into(), json!(p));
                changed.insert("3".into(), json!(p));
            }
        }
    }
    if let Some(p) = req.get("2").filter(|_| state.contains_key("3")) {
        changed.insert("3".into(), p.clone());
    }
    for (k, v) in &changed {
        state.insert(k.clone(), v.clone());
    }
    changed
}

async fn serve_conn(sock: TcpStream, spec: SimSpec, ctl: SimCtl) -> Result<()> {
    let dps = ctl.dps.clone();
    let mut codec = Codec::new(spec.version, &spec.id, spec.key);
    let (mut rd, mut wr) = sock.into_split();
    let mut buf = Vec::new();
    let mut seq = 0u32;
    let mut client_nonce = [0u8; 16];
    let remote_nonce: [u8; 16] = rand::random();
    let (mut pushes, mut kill) = (ctl.push.subscribe(), ctl.kill.subscribe());
    // En 3.4 / 3.5, rien n'est poussé avant la négociation de la clé de session.
    let mut ready = !spec.version.needs_session();
    loop {
        let f = loop {
            if let Some(r) = frame::next_frame(&mut buf, codec.decode_opts(false)) {
                break r?;
            }
            tokio::select! {
                n = rd.read_buf(&mut buf) => if n? == 0 {
                    return Ok(());
                },
                m = pushes.recv() => if let (Ok(m), true) = (m, ready) {
                    seq = seq.wrapping_add(1);
                    wr.write_all(&codec.encode(seq, cmd::STATUS, &status_body(spec.version, &spec.id, &m), None)).await?;
                },
                _ = kill.recv() => return Ok(()),
            }
        };
        let mut out: Vec<(u32, Vec<u8>, Option<u32>)> = Vec::new();
        match f.cmd {
            cmd::SESS_KEY_NEG_START => {
                let n = codec.decrypt_raw(&f)?;
                if n.len() < 16 {
                    bail!("nonce trop court");
                }
                client_nonce.copy_from_slice(&n[..16]);
                out.push((cmd::SESS_KEY_NEG_RESP, codec.negotiation_response(&client_nonce, &remote_nonce), Some(0)));
            }
            cmd::SESS_KEY_NEG_FINISH => {
                if codec.decrypt_raw(&f)? != crypto::hmac_sha256(&spec.key, &remote_nonce) {
                    bail!("négociation refusée");
                }
                codec.set_session(&client_nonce, &remote_nonce);
                ready = true;
            }
            cmd::DP_QUERY | cmd::DP_QUERY_NEW => {
                let all = dps.lock().unwrap().clone();
                out.push((f.cmd, status_body(spec.version, &spec.id, &all), Some(0)));
            }
            cmd::CONTROL | cmd::CONTROL_NEW => {
                let plain = codec.decode_payload(&f)?;
                let v: Value = serde_json::from_slice(&plain)?;
                let req = codec::extract_dps(&v).cloned().unwrap_or_default();
                if !req.is_empty() && req.values().all(Value::is_null) {
                    // Requête d'état « device22 » : on renvoie les DP demandés qui existent.
                    let found: Map<String, Value> = {
                        let st = dps.lock().unwrap();
                        req.keys().filter_map(|k| st.get(k).map(|v| (k.clone(), v.clone()))).collect()
                    };
                    out.push((cmd::STATUS, status_body(spec.version, &spec.id, &found), Some(0)));
                } else {
                    let changed = apply(&mut dps.lock().unwrap(), &req, &ctl);
                    if let Some(v) = req.get("9").and_then(Value::as_u64) {
                        ctl.arm_timer(v);
                    }
                    out.push((f.cmd, Vec::new(), Some(0)));
                    out.push((cmd::STATUS, status_body(spec.version, &spec.id, &changed), None));
                }
            }
            cmd::HEART_BEAT => out.push((cmd::HEART_BEAT, Vec::new(), Some(0))),
            cmd::UPDATEDPS => {
                let mut st = dps.lock().unwrap();
                let mut m = Map::new();
                if let Some(Value::Number(p)) = st.get("19").cloned() {
                    let p = p.as_u64().unwrap_or(0);
                    let v = ctl.voltage.load(Ordering::Relaxed) as u64;
                    if p > 0 {
                        let w = ctl.load.load(Ordering::Relaxed) as u64;
                        let jitter = w - w / 40 + rand::random::<u64>() % (w / 20 + 1);
                        m.insert("19".into(), json!(jitter));
                        m.insert("18".into(), json!(jitter * 1000 / v.max(1)));
                    }
                    m.insert("20".into(), json!(v.saturating_sub(10) + rand::random::<u64>() % 20));
                }
                for (k, v) in &m {
                    st.insert(k.clone(), v.clone());
                }
                if !m.is_empty() {
                    out.push((cmd::STATUS, status_body(spec.version, &spec.id, &m), None));
                }
            }
            _ => {}
        }
        for (c, body, rc) in out {
            seq = seq.wrapping_add(1);
            wr.write_all(&codec.encode(seq, c, &body, rc)).await?;
        }
    }
}

/// Jeu d'appareils de démonstration : (spécification, configuration à enregistrer).
pub fn demo_devices() -> Vec<(SimSpec, DeviceConfig)> {
    let mut list = demo_base();
    let mut heater = list[1].clone();
    heater.0.id = "bf5a1c0de0000heat0plug".into();
    heater.0.key = *b"Ch4uff4ge-s4lon!";
    heater.0.watts = 15200;
    heater.0.dps["1"] = json!(false);
    heater.0.dps["19"] = json!(0);
    heater.0.dps["18"] = json!(0);
    heater.0.dps["38"] = json!("power_off");
    heater.1.id = heater.0.id.clone();
    heater.1.key = String::from_utf8_lossy(&heater.0.key).into_owned();
    heater.1.name = "Chauffage".into();
    heater.1.room = Some("Chambre".into());
    heater.1.safety = crate::automation::Safety::heater_preset();
    list.insert(2, heater);
    // Robot aspirateur Tuya (catégorie « sd », description standard), sur sa base.
    let key = *b"R0b0t-4spir4t3ur";
    let id = DEMO_VACUUM_ID;
    list.push((
        SimSpec { id: id.into(), key, version: Version::V34, dps: crate::vacuum::tuya::standard_dps(), watts: 0 },
        DeviceConfig {
            id: id.into(),
            key: String::from_utf8_lossy(&key).into_owned(),
            name: "Robot aspirateur".into(),
            ip: Some("127.0.0.1".into()),
            port: None,
            version: Some("3.4".into()),
            room: Some("Salon".into()),
            kind: Some("vacuum".into()),
            schema: crate::vacuum::tuya::standard_schema().as_object().cloned().unwrap_or_default(),
            safety: Default::default(),
            powers_pc: false,
        },
    ));
    list
}

/// Robot aspirateur Tuya de la démo.
pub const DEMO_VACUUM_ID: &str = "bf5a1c0de00000vacuum1";

fn demo_base() -> Vec<(SimSpec, DeviceConfig)> {
    let mk = |id: &str, key: &[u8; 16], version: Version, name: &str, room: &str, kind: Option<&str>, dps: Value, schema: Value| {
        let watts = dps.get("19").and_then(Value::as_u64).unwrap_or(0) as u32;
        (
            SimSpec { id: id.into(), key: *key, version, dps, watts },
            DeviceConfig {
                id: id.into(),
                key: String::from_utf8_lossy(key).into_owned(),
                name: name.into(),
                ip: Some("127.0.0.1".into()),
                port: None,
                version: Some(version.to_string()),
                room: Some(room.into()),
                kind: kind.map(str::to_string),
                schema: schema.as_object().cloned().unwrap_or_default(),
                safety: Default::default(),
                powers_pc: false,
            },
        )
    };
    vec![
        mk(
            "bf5a1c0de0000000light1",
            b"Lp8$x2!kQz7#mW4r",
            Version::V33,
            "Lampe du salon",
            "Salon",
            None,
            json!({ "20": true, "21": "white", "22": 720, "23": 380, "24": "00f003e803e8" }),
            json!({}),
        ),
        mk(
            "bf5a1c0de000000plug01",
            b"Pr1s3-TV.k3y_000",
            Version::V34,
            "Prise TV",
            "Salon",
            Some("plug"),
            json!({ "1": true, "9": 0, "17": 0, "18": 210, "19": 486, "20": 2304, "22": 626, "23": 30683, "24": 17264, "25": 1048, "26": 0, "38": "last", "40": "relay" }),
            plug_schema(),
        ),
        mk(
            "bf5a1c0de0000strip01",
            b"Str1pK3y-bureau!",
            Version::V33,
            "Multiprise bureau",
            "Bureau",
            Some("switch"),
            json!({ "1": true, "2": false, "3": true, "4": false }),
            json!({
                "1": { "code": "switch_1", "type": "Boolean", "values": {} },
                "2": { "code": "switch_2", "type": "Boolean", "values": {} },
                "3": { "code": "switch_3", "type": "Boolean", "values": {} },
                "4": { "code": "switch_usb1", "type": "Boolean", "values": {} }
            }),
        ),
        mk(
            "bf5a1c0de000cover0001",
            b"V0let-chambre-01",
            Version::V35,
            "Volet chambre",
            "Chambre",
            None,
            json!({ "1": "stop", "2": 60, "3": 60 }),
            json!({}),
        ),
        mk(
            "bf5a1c0de000heater001",
            b"Ch4uff4ge-v31-ok",
            Version::V31,
            "Radiateur",
            "Chambre",
            None,
            json!({ "1": true, "2": 20, "3": 19, "4": "auto" }),
            json!({
                "1": { "code": "switch", "type": "Boolean", "values": {} },
                "2": { "code": "temp_set", "type": "Integer", "values": { "unit": "°C", "min": 5, "max": 30, "scale": 0, "step": 1 } },
                "3": { "code": "temp_current", "type": "Integer", "values": { "unit": "°C", "min": -20, "max": 50, "scale": 0, "step": 1 } },
                "4": { "code": "mode", "type": "Enum", "values": { "range": ["auto", "manual", "eco"] } }
            }),
        ),
    ]
}

/// Description des DP d'une prise à mesure (identique aux prises Tuya courantes).
pub fn plug_schema() -> Value {
    json!({
        "1": { "code": "switch_1", "type": "Boolean", "values": {} },
        "9": { "code": "countdown_1", "type": "Integer", "values": { "unit": "s", "min": 0, "max": 86400, "scale": 0, "step": 1 } },
        "17": { "code": "add_ele", "type": "Integer", "values": { "min": 0, "max": 50000, "scale": 3, "step": 100 } },
        "18": { "code": "cur_current", "type": "Integer", "values": { "unit": "mA", "min": 0, "max": 30000, "scale": 0, "step": 1 } },
        "19": { "code": "cur_power", "type": "Integer", "values": { "unit": "W", "min": 0, "max": 80000, "scale": 1, "step": 1 } },
        "20": { "code": "cur_voltage", "type": "Integer", "values": { "unit": "V", "min": 0, "max": 5000, "scale": 1, "step": 1 } },
        "22": { "code": "voltage_coe", "type": "Integer", "values": { "min": 0, "max": 1000000, "scale": 0, "step": 1 } },
        "23": { "code": "electric_coe", "type": "Integer", "values": { "min": 0, "max": 1000000, "scale": 0, "step": 1 } },
        "24": { "code": "power_coe", "type": "Integer", "values": { "min": 0, "max": 1000000, "scale": 0, "step": 1 } },
        "25": { "code": "electricity_coe", "type": "Integer", "values": { "min": 0, "max": 1000000, "scale": 0, "step": 1 } },
        "26": { "code": "fault", "type": "Bitmap", "values": { "label": ["ov_cr", "ov_vol", "ov_pwr", "ls_cr", "ls_vol", "ls_pow"] } },
        "38": { "code": "relay_status", "type": "Enum", "values": { "range": ["power_off", "power_on", "last"] } },
        "40": { "code": "light_mode", "type": "Enum", "values": { "range": ["relay", "pos", "none"] } }
    })
}

/// Appareil « non configuré » annoncé sur la boucle locale, pour tester l'ajout.
pub const DEMO_NEW_ID: &str = "bf5a1c0de0000newplug1";
pub const DEMO_NEW_KEY: &str = "N0uv3lle-pr1se!!";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{self, Target};

    async fn start(version: Version, dps: Value) -> Target {
        let key = *b"0123456789abcdef";
        let id = "bf00000000000000test";
        let port = spawn(SimSpec { id: id.into(), key, version, dps, watts: 5000 }, "127.0.0.1:0").await.unwrap();
        Target { id: id.into(), key, ip: "127.0.0.1".into(), port, version }
    }

    #[tokio::test]
    async fn status_and_control_every_version() {
        for v in [Version::V31, Version::V32, Version::V33, Version::V34, Version::V35] {
            let t = start(v, json!({ "1": false, "2": 7 })).await;
            let st = device::fetch_status(&t).await.unwrap_or_else(|e| panic!("{v} status: {e:#}"));
            assert_eq!(st["2"], json!(7), "version {v}");
            let mut req = Map::new();
            req.insert("1".into(), json!(true));
            let ok = device::set_dps(&t, &req).await.unwrap_or_else(|e| panic!("{v} set: {e:#}"));
            assert_eq!(ok["1"], json!(true), "version {v}");
            let st = device::fetch_status(&t).await.unwrap();
            assert_eq!(st["1"], json!(true), "version {v}");
        }
    }

    #[tokio::test]
    async fn wrong_key_is_reported() {
        for v in [Version::V34, Version::V35] {
            let mut t = start(v, json!({ "1": false })).await;
            t.key = *b"AAAAAAAAAAAAAAAA";
            let err = device::fetch_status(&t).await.unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("clé locale") || msg.contains("HMAC") || msg.contains("GCM"), "{v}: {msg}");
        }
    }

    /// Attend que l'état de l'appareil satisfasse `pred` (vue sérialisée en JSON pour les tests).
    async fn wait_device(
        hub: &crate::hub::Hub,
        rx: &mut tokio::sync::broadcast::Receiver<crate::hub::Event>,
        id: &str,
        pred: impl Fn(&Value) -> bool,
    ) -> Value {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(6);
        loop {
            if let Some(v) = hub.view(id).map(|v| serde_json::to_value(v).unwrap()) {
                if pred(&v) {
                    return v;
                }
            }
            let _ = tokio::time::timeout_at(deadline, rx.recv()).await.expect("délai dépassé");
        }
    }

    #[tokio::test]
    async fn guards_countdown_and_forbidden_window() {
        use crate::automation::{Safety, TimeRange};
        use crate::hub::Hub;
        let dir = std::env::temp_dir().join(format!("homesmart-guard-{}", rand::random::<u32>()));
        let hub = Hub::new(dir.join("cfg.json")).unwrap();
        hub.start(false);
        let t = start(Version::V34, json!({ "1": false, "9": 0, "19": 0, "26": 0 })).await;
        let mut events = hub.events.subscribe();
        hub.add(DeviceConfig {
            id: t.id.clone(),
            key: "0123456789abcdef".into(),
            name: "Chauffage test".into(),
            ip: Some(t.ip.clone()),
            port: Some(t.port),
            version: Some("3.4".into()),
            kind: Some("plug".into()),
            schema: plug_schema().as_object().cloned().unwrap(),
            safety: Safety { enabled: true, max_on_minutes: Some(30), ..Default::default() },
            ..Default::default()
        })
        .unwrap();
        wait_device(&hub, &mut events, &t.id, |d| d["online"] == json!(true) && d["dps"]["9"].is_number()).await;

        // Allumage : la minuterie de sécurité de l'appareil est programmée à 30 min.
        hub.power(&t.id, true, None, crate::hub::Source::User).await.unwrap();
        let d = wait_device(&hub, &mut events, &t.id, |d| d["countdown_left"].as_u64().is_some_and(|c| c > 1700)).await;
        assert!(d["countdown_left"].as_u64().unwrap() <= 1800);

        // Plage interdite qui couvre l'heure actuelle : extinction immédiate.
        let now = chrono::Local::now().time();
        let fmt = |t: chrono::NaiveTime| crate::automation::fmt_hm(t);
        let range = TimeRange { start: fmt(now - chrono::Duration::minutes(1)), end: fmt(now + chrono::Duration::minutes(60)) };
        hub.set_safety(&t.id, Safety { enabled: true, forbidden: Some(range), ..Default::default() }).unwrap();
        wait_device(&hub, &mut events, &t.id, |d| d["dps"]["1"] == json!(false)).await;
        // …et rallumer est refusé.
        let err = hub.power(&t.id, true, None, crate::hub::Source::User).await.unwrap_err();
        assert!(format!("{err:#}").contains("interdit"), "{err:#}");
        assert!(hub.alerts(100).iter().any(|a| a.source == crate::hub::Source::Guard && a.message.contains("interdit")));
        assert!(hub.alerts(100).iter().any(|a| a.message.starts_with("Refusé")));
        // L'étalonnage d'usine reste intouchable, même via l'API du hub.
        let mut req = Map::new();
        req.insert("22".into(), json!(1));
        let err = hub.set_dps(&t.id, req, crate::hub::Source::User).await.unwrap_err();
        assert!(format!("{err:#}").contains("usine"), "{err:#}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn hub_end_to_end() {
        use crate::hub::Hub;
        let dir = std::env::temp_dir().join(format!("homesmart-test-{}", rand::random::<u32>()));
        let path = dir.join("cfg.json");
        let hub = Hub::new(path.clone()).unwrap();
        hub.start(false);
        let t = start(Version::V34, json!({ "1": false, "19": 0, "20": 2300 })).await;
        let mut events = hub.events.subscribe();
        hub.add(DeviceConfig {
            id: t.id.clone(),
            key: "0123456789abcdef".into(),
            name: "Test".into(),
            ip: Some(t.ip.clone()),
            port: Some(t.port),
            version: Some("3.4".into()),
            room: None,
            kind: None,
            schema: Map::new(),
            safety: Default::default(),
            powers_pc: false,
        })
        .unwrap();
        // Attendre la connexion et l'état initial.
        wait_device(&hub, &mut events, &t.id, |d| d["dps"]["20"] == json!(2300)).await;
        let dp = hub.power(&t.id, true, None, crate::hub::Source::User).await.unwrap();
        assert_eq!(dp, "1");
        let v = wait_device(&hub, &mut events, &t.id, |d| d["dps"]["1"] == json!(true) && d["latency_ms"].is_u64()).await;
        assert!(v["dps"]["19"].as_u64().unwrap() > 0, "la prise doit consommer");
        // Journal : l'allumage est inscrit une seule fois, avec toi comme origine (pas en double comme « appareil »).
        let log = hub.alerts(100);
        assert!(log.iter().any(|a| a.source == crate::hub::Source::User && a.message == "Allumé"), "{log:?}");
        assert!(!log.iter().any(|a| a.source == crate::hub::Source::Device), "{log:?}");
        assert!(log.iter().any(|a| a.message == "Appareil ajouté"));
        assert!(log.iter().any(|a| a.message.starts_with("Connecté")));
        // Les mesures arrivent au rythme demandé (1 s ici) sans autre sollicitation.
        hub.set_prefs(crate::config::Prefs { meter_interval_s: 1.0, ..Default::default() });
        let first = hub.view(&t.id).unwrap().updated;
        wait_device(&hub, &mut events, &t.id, |d| d["updated"].as_u64().unwrap() > first + 500).await;
        // La configuration a été écrite sur disque.
        let saved = crate::config::Config::load(&path).unwrap();
        assert_eq!(saved.devices.len(), 1);
        hub.remove(&t.id).unwrap();
        assert!(crate::config::Config::load(&path).unwrap().devices.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
