//! Appareils TP-Link simulés, pour les tests et la démo : une prise Tapo qui parle KLAP (HTTP) et
//! une ancienne prise Kasa (TCP 9999, XOR), sur 127.0.0.1.

use super::{kasa, klap};
use anyhow::Result;
use rand::RngCore;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, AsyncBufReadExt};
use tokio::net::{TcpListener, TcpStream};

/// Prise Tapo simulée : son état (réponse de `get_device_info`, plus `current_power` en mW).
pub struct SimTapo {
    pub port: u16,
    pub state: Arc<Mutex<Map<String, Value>>>,
}

/// Session en cours : graines et chiffrement, par cookie.
#[derive(Default)]
struct Sessions {
    pending: HashMap<String, ([u8; 16], [u8; 16])>,
    open: HashMap<String, klap::Cipher>,
}

/// Démarre une prise Tapo P110 simulée, installée avec ce compte.
pub async fn start_tapo(email: &str, password: &str, nickname: &str) -> Result<SimTapo> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine};
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let state = Arc::new(Mutex::new(
        json!({
            "device_id": "SIM-P110", "model": "P110", "type": "SMART.TAPOPLUG", "fw_ver": "1.3.1 Build 240621",
            "mac": "AA-BB-CC-00-01-10", "nickname": B64.encode(nickname), "device_on": true, "rssi": -48,
            "current_power": 42_500, "today_energy": 310,
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    ));
    let auth = klap::auth_hash_v2(email, password);
    let sessions = Arc::new(Mutex::new(Sessions::default()));
    let st = state.clone();
    tokio::spawn(async move {
        while let Ok((s, _)) = listener.accept().await {
            let (st, auth, sessions) = (st.clone(), auth.clone(), sessions.clone());
            tokio::spawn(async move {
                let _ = tapo_conn(s, st, auth, sessions).await;
            });
        }
    });
    Ok(SimTapo { port, state })
}

/// Lit une requête HTTP : (chemin, en-têtes en minuscules, corps).
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

async fn reply(w: &mut BufReader<TcpStream>, status: u16, extra: &str, body: &[u8]) -> Result<()> {
    let head = format!("HTTP/1.1 {status} {}\r\nContent-Length: {}\r\n{extra}\r\n", if status == 200 { "OK" } else { "Forbidden" }, body.len());
    w.get_mut().write_all(head.as_bytes()).await?;
    w.get_mut().write_all(body).await?;
    Ok(())
}

async fn tapo_conn(s: TcpStream, state: Arc<Mutex<Map<String, Value>>>, auth: Vec<u8>, sessions: Arc<Mutex<Sessions>>) -> Result<()> {
    let mut r = BufReader::new(s);
    while let Some((path, headers, body)) = read_http(&mut r).await? {
        let cookie = headers.get("cookie").map(|c| c.trim().to_string()).unwrap_or_default();
        if path == "/app/handshake1" && body.len() == 16 {
            let mut remote = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut remote);
            let local: [u8; 16] = body.as_slice().try_into()?;
            let mut out = remote.to_vec();
            out.extend_from_slice(&klap::server_proof(&local, &remote, &auth));
            let id = format!("TP_SESSIONID={:016X}", rand::random::<u64>());
            sessions.lock().unwrap().pending.insert(id.clone(), (local, remote));
            reply(&mut r, 200, &format!("Set-Cookie: {id};TIMEOUT=86400\r\n"), &out).await?;
        } else if path == "/app/handshake2" {
            let pending = sessions.lock().unwrap().pending.remove(&cookie);
            match pending {
                Some((local, remote)) if body == klap::client_proof(&local, &remote, &auth) => {
                    sessions.lock().unwrap().open.insert(cookie, klap::Cipher::new(&local, &remote, &auth));
                    reply(&mut r, 200, "", b"").await?;
                }
                _ => reply(&mut r, 403, "", b"").await?,
            }
        } else if let Some(seq) = path.strip_prefix("/app/request?seq=").and_then(|s| s.parse::<i32>().ok()) {
            let cipher = sessions.lock().unwrap().open.get(&cookie).cloned();
            let Some(cipher) = cipher else {
                reply(&mut r, 403, "", b"").await?;
                continue;
            };
            let req: Value = serde_json::from_slice(&cipher.decrypt(seq, &body)?)?;
            let answer = {
                let mut st = state.lock().unwrap();
                match req.get("method").and_then(Value::as_str).unwrap_or_default() {
                    "get_device_info" => {
                        let mut info = st.clone();
                        info.remove("current_power");
                        info.remove("today_energy");
                        json!({ "error_code": 0, "result": info })
                    }
                    "set_device_info" => {
                        if let Some(p) = req.get("params").and_then(Value::as_object) {
                            for (k, v) in p {
                                st.insert(k.clone(), v.clone());
                            }
                        }
                        json!({ "error_code": 0 })
                    }
                    "get_energy_usage" => {
                        let on = st.get("device_on").and_then(Value::as_bool).unwrap_or(false);
                        let p = if on { st.get("current_power").cloned().unwrap_or(json!(0)) } else { json!(0) };
                        json!({ "error_code": 0, "result": { "current_power": p, "today_energy": st.get("today_energy").cloned().unwrap_or(json!(0)) } })
                    }
                    _ => json!({ "error_code": -1 }),
                }
            };
            reply(&mut r, 200, "", &cipher.seal(seq, answer.to_string().as_bytes())).await?;
        } else {
            reply(&mut r, 403, "", b"").await?;
        }
    }
    Ok(())
}

/// Ancienne prise Kasa simulée (TCP, protocole XOR) : son `get_sysinfo`.
pub struct SimKasa {
    pub port: u16,
    pub sysinfo: Arc<Mutex<Map<String, Value>>>,
}

pub async fn start_kasa(alias: &str) -> Result<SimKasa> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let sysinfo = Arc::new(Mutex::new(
        json!({ "alias": alias, "model": "HS110(EU)", "mac": "50:C7:BF:00:00:01", "type": "IOT.SMARTPLUGSWITCH", "relay_state": 1, "sw_ver": "1.5.4", "feature": "TIM:ENE", "rssi": -55 })
            .as_object()
            .cloned()
            .unwrap_or_default(),
    ));
    let si = sysinfo.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let si = si.clone();
            tokio::spawn(async move {
                let mut len = [0u8; 4];
                if s.read_exact(&mut len).await.is_err() {
                    return;
                }
                let mut data = vec![0u8; u32::from_be_bytes(len) as usize];
                if s.read_exact(&mut data).await.is_err() {
                    return;
                }
                let Ok(req) = serde_json::from_slice::<Value>(&kasa::decrypt(&data)) else { return };
                let answer = {
                    let mut info = si.lock().unwrap();
                    if req.pointer("/system/get_sysinfo").is_some() {
                        json!({ "system": { "get_sysinfo": info.clone() } })
                    } else if let Some(state) = req.pointer("/system/set_relay_state/state").and_then(Value::as_u64) {
                        info.insert("relay_state".into(), json!(state));
                        json!({ "system": { "set_relay_state": { "err_code": 0 } } })
                    } else if req.pointer("/emeter/get_realtime").is_some() {
                        let on = info.get("relay_state").and_then(Value::as_u64) == Some(1);
                        json!({ "emeter": { "get_realtime": { "power_mw": if on { 18_200 } else { 0 }, "voltage_mv": 231_000, "current_ma": 85, "err_code": 0 } } })
                    } else {
                        json!({ "err_code": -1 })
                    }
                };
                let msg = kasa::encrypt(answer.to_string().as_bytes());
                let _ = s.write_all(&(msg.len() as u32).to_be_bytes()).await;
                let _ = s.write_all(&msg).await;
            });
        }
    });
    Ok(SimKasa { port, sysinfo })
}
