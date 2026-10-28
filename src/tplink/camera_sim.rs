//! Caméra Tapo simulée (API locale, en HTTP sur 127.0.0.1) pour les tests : connexion en deux
//! temps avec verrouillage après 10 essais, requêtes chiffrées vérifiées (numéro et étiquette),
//! alarme, suivi des mouvements, haut-parleur, carte SD et liste des enregistrements.

use super::camera::{device_confirm, digest_passwd, session_key, tag, TapoSecret};
use super::klap::{cbc_decrypt, cbc_encrypt};
use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Ce que la caméra simulée a en mémoire (lisible et modifiable par les tests).
#[derive(Clone, Debug)]
pub struct CamState {
    pub alarm: Value,
    /// Alarme déclenchée à la main en ce moment.
    pub siren: bool,
    pub track: bool,
    pub volume: u8,
    /// Séquences enregistrées (secondes Unix, horloge de la caméra).
    pub clips: Vec<(i64, i64)>,
    /// Requêtes `login` reçues, et essais restants avant verrouillage.
    pub logins: u32,
    pub attempts_left: i64,
    /// Requêtes chiffrées acceptées.
    pub requests: u32,
}

pub struct SimTapoCam {
    pub port: u16,
    pub state: Arc<Mutex<CamState>>,
}

impl SimTapoCam {
    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

struct Pending {
    cnonce: String,
    nonce: String,
}

struct Open {
    lsk: [u8; 16],
    ivb: [u8; 16],
    seq: i64,
    cnonce: String,
}

#[derive(Default)]
struct Sessions {
    pending: Option<Pending>,
    open: HashMap<String, Open>,
}

/// Démarre une Tapo C210 simulée, installée avec ce mot de passe de compte TP-Link.
pub async fn start(password: &str) -> Result<SimTapoCam> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let state = Arc::new(Mutex::new(CamState {
        alarm: json!({ "enabled": "off", "alarm_type": "0", "light_type": "0", "alarm_mode": ["sound", "light"], "alarm_volume": "normal", "alarm_duration": 30 }),
        siren: false,
        track: false,
        volume: 50,
        clips: Vec::new(),
        logins: 0,
        attempts_left: 10,
        requests: 0,
    }));
    let secret = TapoSecret::from_password(password);
    let sessions = Arc::new(Mutex::new(Sessions::default()));
    let st = state.clone();
    tokio::spawn(async move {
        while let Ok((s, _)) = listener.accept().await {
            let (st, secret, sessions) = (st.clone(), secret.clone(), sessions.clone());
            tokio::spawn(async move {
                let _ = conn(s, st, secret, sessions).await;
            });
        }
    });
    Ok(SimTapoCam { port, state })
}

async fn read_http(r: &mut BufReader<TcpStream>) -> Result<Option<(String, HashMap<String, String>, Vec<u8>)>> {
    let mut line = String::new();
    if r.read_line(&mut line).await? == 0 {
        return Ok(None);
    }
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).await? == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some((path, headers, body)))
}

async fn reply(r: &mut BufReader<TcpStream>, body: &Value) -> Result<()> {
    let b = body.to_string();
    let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json;charset=UTF-8\r\nContent-Length: {}\r\n\r\n", b.len());
    r.get_mut().write_all(head.as_bytes()).await?;
    r.get_mut().write_all(b.as_bytes()).await?;
    Ok(())
}

async fn conn(s: TcpStream, state: Arc<Mutex<CamState>>, secret: TapoSecret, sessions: Arc<Mutex<Sessions>>) -> Result<()> {
    let mut r = BufReader::new(s);
    while let Some((path, headers, body)) = read_http(&mut r).await? {
        let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        if path == "/" {
            let answer = login(&req, &state, &secret, &sessions);
            reply(&mut r, &answer).await?;
        } else if let Some(stok) = path.strip_prefix("/stok=").and_then(|p| p.strip_suffix("/ds")) {
            let answer = secure(stok, &headers, &body, &req, &state, &secret, &sessions);
            reply(&mut r, &answer).await?;
        } else {
            reply(&mut r, &json!({ "error_code": -40106 })).await?;
        }
    }
    Ok(())
}

fn login(req: &Value, state: &Arc<Mutex<CamState>>, secret: &TapoSecret, sessions: &Arc<Mutex<Sessions>>) -> Value {
    let p = req.get("params").cloned().unwrap_or(Value::Null);
    let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let mut st = state.lock().unwrap();
    st.logins += 1;
    if st.attempts_left <= 0 {
        return json!({ "error_code": -40404, "result": { "data": { "code": -40404, "sec_left": 1800, "time": 0, "max_time": 10 } } });
    }
    // Chaque requête de connexion compte (comme sur la vraie caméra).
    st.attempts_left -= 1;
    let digest = s("digest_passwd");
    if digest.is_empty() {
        let nonce = super::camera::nonce(8);
        let confirm = device_confirm(&s("cnonce"), &secret.sha256, &nonce);
        sessions.lock().unwrap().pending = Some(Pending { cnonce: s("cnonce"), nonce: nonce.clone() });
        return json!({ "error_code": -40413, "result": { "data": { "code": -40401, "time": st.attempts_left, "max_time": 10, "encrypt_type": ["3"], "key": "", "nonce": nonce, "device_confirm": confirm } } });
    }
    let pending = sessions.lock().unwrap().pending.take();
    match pending {
        Some(pd) if pd.cnonce == s("cnonce") && digest == digest_passwd(&secret.sha256, &pd.cnonce, &pd.nonce) => {
            st.attempts_left = 10;
            let stok = super::camera::nonce(16).to_lowercase();
            let seq = 1000 + (rand::random::<u16>() as i64);
            let open = Open { lsk: session_key("lsk", &pd.cnonce, &pd.nonce, &secret.sha256), ivb: session_key("ivb", &pd.cnonce, &pd.nonce, &secret.sha256), seq, cnonce: pd.cnonce };
            sessions.lock().unwrap().open.insert(stok.clone(), open);
            json!({ "error_code": 0, "result": { "stok": stok, "user_group": "root", "start_seq": seq } })
        }
        _ => json!({ "error_code": -40413, "result": { "data": { "code": -40411, "time": st.attempts_left, "max_time": 10 } } }),
    }
}

#[allow(clippy::too_many_arguments)]
fn secure(stok: &str, headers: &HashMap<String, String>, body: &[u8], req: &Value, state: &Arc<Mutex<CamState>>, secret: &TapoSecret, sessions: &Arc<Mutex<Sessions>>) -> Value {
    let mut ss = sessions.lock().unwrap();
    let Some(open) = ss.open.get_mut(stok) else { return json!({ "error_code": -40401 }) };
    let seq: i64 = headers.get("seq").and_then(|v| v.parse().ok()).unwrap_or(-1);
    let body_text = String::from_utf8_lossy(body);
    if seq != open.seq || headers.get("tapo_tag").map(String::as_str) != Some(&tag(&secret.sha256, &open.cnonce, &body_text, seq)) {
        return json!({ "error_code": -40401 });
    }
    open.seq += 1;
    let Some(enc) = req.pointer("/params/request").and_then(Value::as_str) else { return json!({ "error_code": -40106 }) };
    let Ok(plain) = B64.decode(enc).map_err(anyhow::Error::from).and_then(|ct| cbc_decrypt(&open.lsk, &open.ivb, &ct)) else { return json!({ "error_code": -40401 }) };
    let inner: Value = serde_json::from_slice(&plain).unwrap_or(Value::Null);
    let (lsk, ivb) = (open.lsk, open.ivb);
    drop(ss);
    let answer = handle(&inner, state);
    state.lock().unwrap().requests += 1;
    json!({ "error_code": 0, "result": { "response": B64.encode(cbc_encrypt(&lsk, &ivb, answer.to_string().as_bytes())) } })
}

fn handle(req: &Value, state: &Arc<Mutex<CamState>>) -> Value {
    let method = req.get("method").and_then(Value::as_str).unwrap_or_default();
    match method {
        "multipleRequest" => {
            let responses: Vec<Value> = req.pointer("/params/requests").and_then(Value::as_array).into_iter().flatten().map(|r| one(r, state)).collect();
            json!({ "error_code": 0, "result": { "responses": responses } })
        }
        "set" => {
            if let Some(a) = req.pointer("/msg_alarm/chn1_msg_alarm_info") {
                let mut st = state.lock().unwrap();
                for (k, v) in a.as_object().into_iter().flatten() {
                    st.alarm[k] = v.clone();
                }
                return json!({ "error_code": 0 });
            }
            json!({ "error_code": -40101 })
        }
        "do" => match req.pointer("/msg_alarm/manual_msg_alarm/action").and_then(Value::as_str) {
            Some(a) => {
                state.lock().unwrap().siren = a == "start";
                json!({ "error_code": 0 })
            }
            None => json!({ "error_code": -40106 }),
        },
        _ => json!({ "error_code": -40105 }),
    }
}

fn one(r: &Value, state: &Arc<Mutex<CamState>>) -> Value {
    let method = r.get("method").and_then(Value::as_str).unwrap_or_default().to_string();
    let p = r.get("params").cloned().unwrap_or(Value::Null);
    let mut st = state.lock().unwrap();
    let ok = |result: Value| json!({ "method": method, "result": result, "error_code": 0 });
    match method.as_str() {
        "getDeviceInfo" => ok(json!({ "device_info": { "basic_info": { "device_model": "C210", "sw_version": "1.5.5 Build 260915 Rel.61819n", "hw_version": "2.0", "device_alias": "Salon", "mac": "DC-62-79-00-00-01" } } })),
        "getClockStatus" => ok(json!({ "system": { "clock_status": { "seconds_from_1970": chrono::Utc::now().timestamp() - 7, "local_time": "" } } })),
        "getLastAlarmInfo" => ok(json!({ "msg_alarm": { "chn1_msg_alarm_info": st.alarm.clone() } })),
        "getTargetTrackConfig" => ok(json!({ "target_track": { "target_track_info": { "enabled": if st.track { "on" } else { "off" } } } })),
        "setTargetTrackConfig" => {
            st.track = p.pointer("/target_track/target_track_info/enabled").and_then(Value::as_str) == Some("on");
            ok(json!({}))
        }
        "getAudioConfig" => ok(json!({ "audio_config": { "speaker": { "volume": st.volume.to_string() }, "microphone": { "volume": "80", "sampling_rate": "8" } } })),
        "setSpeakerVolume" => {
            st.volume = p.pointer("/audio_config/speaker/volume").and_then(Value::as_u64).unwrap_or(st.volume as u64).min(100) as u8;
            ok(json!({}))
        }
        "getVideoQualities" => ok(json!({ "video": { "main": { "resolution": "2304*1296", "bitrate": "1382", "bitrate_type": "vbr", "encode_type": "H264", "quality": "3", "frame_rate": "65566" } } })),
        "getSdCardStatus" => ok(json!({ "harddisk_manage": { "hd_info": [{ "hd_info_1": { "status": "normal", "total_space": "29.7GB", "free_space": "3.1GB", "video_total_space": "28.9GB", "video_free_space": "2.4GB" } }] } })),
        "getUserID" => ok(json!({ "user_id": 77 })),
        "searchDateWithVideo" => {
            let mut days: Vec<String> = st.clips.iter().filter_map(|(a, _)| chrono::DateTime::from_timestamp(*a, 0)).map(|d| d.with_timezone(&chrono::Local).format("%Y%m%d").to_string()).collect();
            days.dedup();
            let list: Vec<Value> = days.iter().enumerate().map(|(i, d)| json!({ format!("search_results_{i}"): { "date": d } })).collect();
            ok(json!({ "playback": { "search_results": list } }))
        }
        "searchVideoOfDay" => {
            if p.pointer("/playback/search_video_utility/id").and_then(Value::as_i64) != Some(77) {
                return json!({ "method": method, "error_code": -71103 });
            }
            let date = p.pointer("/playback/search_video_utility/date").and_then(Value::as_str).unwrap_or_default().to_string();
            let start = p.pointer("/playback/search_video_utility/start_index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let end = p.pointer("/playback/search_video_utility/end_index").and_then(Value::as_u64).unwrap_or(99) as usize;
            let of_day: Vec<&(i64, i64)> =
                st.clips.iter().filter(|(a, _)| chrono::DateTime::from_timestamp(*a, 0).is_some_and(|d| d.with_timezone(&chrono::Local).format("%Y%m%d").to_string() == date)).collect();
            let list: Vec<Value> = of_day.iter().enumerate().filter(|(i, _)| *i >= start && *i <= end).map(|(i, (a, b))| json!({ format!("video{i}"): { "startTime": a, "endTime": b, "vedio_type": 2 } })).collect();
            ok(json!({ "playback": { "search_video_results": list } }))
        }
        _ => json!({ "method": method, "error_code": -40210 }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::camera::{AlarmConfig, TapoCam, TapoSecret};
    use super::*;
    use anyhow::Result;

    /// Poignée de main chiffrée de bout en bout, puis alarme, suivi des mouvements et index SD.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_session_against_sim() {
        let sim = start("Sim-pass1!").await.unwrap();
        // Deux séquences un même jour (horloge de la caméra = PC − 7 s).
        let base_ts = chrono::Local::now().with_time(chrono::NaiveTime::from_hms_opt(14, 0, 0).unwrap()).unwrap().timestamp();
        {
            let mut st = sim.state.lock().unwrap();
            st.clips = vec![(base_ts, base_ts + 60), (base_ts + 600, base_ts + 645)];
        }
        let date = chrono::Local::now().date_naive();
        let (base, state) = (sim.base(), sim.state.clone());
        let out = tokio::task::spawn_blocking(move || -> Result<_> {
            let secret = TapoSecret::from_password("Sim-pass1!");
            let mut cam = TapoCam::with_base(&base, &secret);
            cam.login()?;
            let info = cam.device_info()?;
            // Alarme : sirène seule, volume fort.
            cam.set_alarm(&AlarmConfig { enabled: true, sound: true, light: false, volume: Some("high".into()), duration: Some(20) })?;
            let alarm = cam.alarm()?;
            cam.manual_alarm(true)?;
            cam.set_auto_track(true)?;
            let track = cam.auto_track()?;
            let days = cam.recording_days(date, date)?;
            let clips = cam.recordings(date)?;
            Ok((info.model, alarm, track, days.len(), clips.len()))
        })
        .await
        .unwrap()
        .unwrap();
        let (model, alarm, track, days, clips) = out;
        assert_eq!(model, "C210");
        assert_eq!((alarm.enabled, alarm.sound, alarm.light), (true, true, false));
        assert_eq!(alarm.volume.as_deref(), Some("high"));
        assert!(track);
        assert_eq!((days, clips), (1, 2));
        let st = state.lock().unwrap();
        assert!(st.siren, "l'alarme manuelle doit être en marche");
        assert!(st.track);
        assert_eq!(st.alarm["alarm_volume"], json!("high"));
        // Les requêtes chiffrées ont bien été acceptées (numéro et étiquette vérifiés par le sim).
        assert!(st.requests >= 6, "requêtes chiffrées acceptées : {}", st.requests);
    }

    /// Mauvais mot de passe : refus clair, sans second `login`, et le verrouillage se rapproche.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wrong_password_is_refused_once() {
        let sim = start("Sim-pass1!").await.unwrap();
        let (base, state) = (sim.base(), sim.state.clone());
        let err = tokio::task::spawn_blocking(move || {
            let mut cam = TapoCam::with_base(&base, &TapoSecret::from_password("faux"));
            cam.login().err().map(|e| format!("{e:#}"))
        })
        .await
        .unwrap()
        .unwrap();
        assert!(err.contains("refusé"), "{err}");
        // Une seule tentative consommée (device_confirm vérifié côté client avant tout second login).
        assert_eq!(sim.state.lock().unwrap().logins, 1);
        assert_eq!(state.lock().unwrap().attempts_left, 9);
    }
}
