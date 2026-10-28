//! Caméras HomeSmart (fabriquées soi-même) : dialogue avec leur agent `homesmart-cam`.
//!
//! La vidéo et le son passent en RTSP comme pour les autres caméras (flux `/main` et `/sub`,
//! micro en PCM 16 kHz). Le reste passe par l'API HTTP de l'agent : appairage par code
//! (SPAKE2 : le code ne circule jamais), puis requêtes signées avec le secret né de
//! l'appairage, d'où vient aussi le mot de passe RTSP. Appels bloquants (à lancer hors de
//! l'interface et de la boucle async).

use super::{AgentLink, Brand, CameraConfig};
use anyhow::{anyhow, bail, Context, Result};
use homesmart_cam_proto::{self as proto, ApiError, HubPairing, Info, PairChallenge, Paired, Settings, Status};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(6);

fn http() -> ureq::Agent {
    ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(TIMEOUT)).build().into()
}

/// Écart d'horloge mesuré avec chaque caméra (secondes à ajouter à notre heure), corrigé quand
/// une caméra refuse une requête pour cause d'horloge décalée (caméra sans accès à l'heure).
fn clock_offsets() -> &'static Mutex<HashMap<String, i64>> {
    static O: std::sync::OnceLock<Mutex<HashMap<String, i64>>> = std::sync::OnceLock::new();
    O.get_or_init(Mutex::default)
}

fn base(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("http://[{host}]:{port}")
    } else {
        format!("http://{host}:{port}")
    }
}

/// Lit le corps d'une réponse : l'objet attendu, ou le message d'erreur de la caméra.
fn read<T: serde::de::DeserializeOwned>(mut r: ureq::http::Response<ureq::Body>) -> Result<T> {
    let status = r.status();
    let text = r.body_mut().read_to_string().context("réponse de la caméra")?;
    if status.is_success() {
        return serde_json::from_str(&text).with_context(|| format!("réponse de la caméra illisible : {text}"));
    }
    match serde_json::from_str::<ApiError>(&text) {
        Ok(e) => bail!("{}", e.error),
        Err(_) => bail!("la caméra a répondu {status}"),
    }
}

fn connect_error(e: ureq::Error, host: &str) -> anyhow::Error {
    match e {
        ureq::Error::Timeout(_) => anyhow!("la caméra {host} ne répond pas (éteinte ou hors du réseau ?)"),
        ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::ConnectionRefused => anyhow!("la caméra {host} refuse la connexion : l'agent homesmart-cam tourne-t-il ?"),
        e => anyhow!("caméra {host} : {e}"),
    }
}

/// Présentation de la caméra (sans appairage).
pub fn info(host: &str, api_port: u16) -> Result<Info> {
    let r = http().get(format!("{}/api/info", base(host, api_port))).call().map_err(|e| connect_error(e, host))?;
    read(r)
}

/// Nom sous lequel ce HomeSmart se présente à la caméra.
pub fn client_name() -> String {
    let host = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).ok().filter(|h| !h.trim().is_empty());
    match host {
        Some(h) => format!("HomeSmart ({h})"),
        None => "HomeSmart".into(),
    }
}

/// Appairage avec le code de la caméra. Renvoie le lien à garder et la présentation de la caméra.
pub fn pair(host: &str, api_port: u16, code: &str) -> Result<(AgentLink, Info)> {
    if proto::normalize_code(code).len() != proto::CODE_LEN {
        bail!("le code d'appairage fait 8 caractères (« ABCD-EFGH »)");
    }
    let info = info(host, api_port)?;
    if info.protocol != proto::PROTOCOL {
        bail!("agent de la caméra en version {} du protocole (HomeSmart parle la version {}) : mets-les à jour", info.protocol, proto::PROTOCOL);
    }
    let a = http();
    let url = base(host, api_port);
    let (hub, start) = HubPairing::start(code, &client_name());
    let r = a.post(format!("{url}/api/pair/start")).send_json(&start).map_err(|e| connect_error(e, host))?;
    let ch: PairChallenge = read(r)?;
    // Un code faux se voit ici, sans rien envoyer de plus : la preuve de la caméra ne colle pas.
    let (secret, fin) = hub.finish(&ch).map_err(|e| anyhow!("{e}"))?;
    let r = a.post(format!("{url}/api/pair/finish")).send_json(&fin).map_err(|e| connect_error(e, host))?;
    let paired: Paired = read(r)?;
    Ok((AgentLink { api_port, client_id: paired.client_id, secret: proto::secret_to_hex(&secret) }, info))
}

/// Remplit la configuration d'une caméra tout juste appairée.
pub fn apply_pairing(c: &mut CameraConfig, link: AgentLink, info: &Info) {
    let secret = proto::secret_from_hex(&link.secret).unwrap_or_default();
    c.brand = Brand::HomeSmart;
    c.username = link.client_id.clone();
    c.password = proto::rtsp_password(&secret);
    c.rtsp_port = info.rtsp_port;
    (c.main_path, c.sub_path) = (None, None);
    c.model = Some(info.board.clone()).filter(|b| !b.is_empty());
    c.serial = Some(info.id.clone());
    if c.name.trim().is_empty() {
        c.name = info.name.clone();
    }
    c.agent = Some(link);
}

fn link(c: &CameraConfig) -> Result<(&AgentLink, [u8; 32])> {
    let l = c.agent.as_ref().ok_or_else(|| anyhow!("« {} » n'est pas une caméra HomeSmart appairée", c.display_name()))?;
    let secret = proto::secret_from_hex(&l.secret).ok_or_else(|| anyhow!("secret d'appairage illisible : supprime la caméra et appaire-la à nouveau"))?;
    Ok((l, secret))
}

/// Requête signée ; recommence une fois en corrigeant l'heure si la caméra la trouve décalée.
fn signed(c: &CameraConfig, method: &str, path: &str, body: Option<Vec<u8>>) -> Result<ureq::http::Response<ureq::Body>> {
    let (l, secret) = link(c)?;
    let a = http();
    let url = format!("{}{path}", base(&c.host, l.api_port));
    let body = body.unwrap_or_default();
    for attempt in 0..2 {
        let offset = clock_offsets().lock().unwrap().get(&c.host).copied().unwrap_or(0);
        let time = chrono::Utc::now().timestamp() + offset;
        let nonce = proto::new_nonce();
        let sig = proto::signature(&secret, method, path, time, &nonce, &body);
        let headers = [(proto::H_CLIENT, l.client_id.clone()), (proto::H_TIME, time.to_string()), (proto::H_NONCE, nonce), (proto::H_SIG, sig)];
        let r = match method {
            "GET" => headers.iter().fold(a.get(&url), |r, (k, v)| r.header(*k, v)).call(),
            "DELETE" => headers.iter().fold(a.delete(&url), |r, (k, v)| r.header(*k, v)).call(),
            "PUT" => headers.iter().fold(a.put(&url), |r, (k, v)| r.header(*k, v)).header("content-type", "application/json").send(&body[..]),
            _ => headers.iter().fold(a.post(&url), |r, (k, v)| r.header(*k, v)).header("content-type", "application/json").send(&body[..]),
        }
        .map_err(|e| connect_error(e, &c.host))?;
        let cam_time = r.headers().get(proto::H_TIME).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok());
        match (r.status().as_u16(), cam_time) {
            (401, Some(t)) if attempt == 0 => {
                clock_offsets().lock().unwrap().insert(c.host.clone(), t - chrono::Utc::now().timestamp());
                continue;
            }
            _ => return Ok(r),
        }
    }
    unreachable!("la boucle renvoie au second essai")
}

/// Ce que montre l'interface d'une caméra HomeSmart : présentation, réglages, état.
#[derive(Clone, Debug)]
pub struct AgentState {
    pub info: Info,
    pub settings: Settings,
    pub status: Status,
}

pub fn state(c: &CameraConfig) -> Result<AgentState> {
    let (l, _) = link(c)?;
    Ok(AgentState { info: info(&c.host, l.api_port)?, settings: settings(c)?, status: status(c)? })
}

pub fn status(c: &CameraConfig) -> Result<Status> {
    read(signed(c, "GET", "/api/status", None)?)
}

pub fn settings(c: &CameraConfig) -> Result<Settings> {
    read(signed(c, "GET", "/api/settings", None)?)
}

/// Applique des réglages (la caméra les vérifie et relance son encodage) ; renvoie ceux retenus.
pub fn set_settings(c: &CameraConfig, s: &Settings) -> Result<Settings> {
    read(signed(c, "PUT", "/api/settings", Some(serde_json::to_vec(s)?))?)
}

/// Relance l'encodage de la caméra.
pub fn restart(c: &CameraConfig) -> Result<()> {
    let r = signed(c, "POST", "/api/restart", None)?;
    if r.status().is_success() {
        Ok(())
    } else {
        read::<serde_json::Value>(r).map(|_| ())
    }
}

/// Demande à la caméra d'oublier ce HomeSmart (caméra supprimée).
pub fn unpair(c: &CameraConfig) -> Result<()> {
    let r = signed(c, "DELETE", "/api/pair", None)?;
    if r.status().is_success() {
        Ok(())
    } else {
        read::<serde_json::Value>(r).map(|_| ())
    }
}

/// Différences lisibles entre deux réglages (pour le journal).
pub fn describe_changes(before: &Settings, after: &Settings) -> Vec<String> {
    let mut out = Vec::new();
    if before.name != after.name {
        out.push(format!("nom « {} »", after.name));
    }
    for (label, a, b) in [("HD", &before.main, &after.main), ("fluide", &before.sub, &after.sub)] {
        if (a.width, a.height) != (b.width, b.height) {
            out.push(format!("flux {label} en {}×{}", b.width, b.height));
        }
        if a.fps != b.fps {
            out.push(format!("flux {label} à {} i/s", b.fps));
        }
        if a.kbps != b.kbps {
            out.push(format!("flux {label} à {} kbit/s", b.kbps));
        }
        if a.codec != b.codec {
            out.push(format!("flux {label} en {}", b.codec.label()));
        }
    }
    if before.audio.enabled != after.audio.enabled {
        out.push(if after.audio.enabled { "micro activé".into() } else { "micro coupé".into() });
    }
    if before.audio.gain_db != after.audio.gain_db {
        out.push(format!("gain du micro {:+.0} dB", after.audio.gain_db));
    }
    if before.rotate_180 != after.rotate_180 {
        out.push(if after.rotate_180 { "image retournée".into() } else { "image à l'endroit".into() });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use homesmart_cam::api::{App, Media, Shared};
    use homesmart_cam::config::{AgentConfig, Backend, Tools};
    use homesmart_cam_proto::{Caps, ProcStatus, VideoCodec};
    use std::sync::Arc;
    use std::time::Instant;

    struct NoMedia;

    impl Media for NoMedia {
        fn restart_pipeline(&self) {}
        fn clients_changed(&self) {}
        fn status(&self) -> (ProcStatus, ProcStatus) {
            (ProcStatus { running: true, ..ProcStatus::default() }, ProcStatus { running: true, ..ProcStatus::default() })
        }
    }

    /// Une vraie API de caméra HomeSmart sur 127.0.0.1 (sans flux : voir `sim` pour le RTSP).
    async fn camera() -> (u16, Arc<Shared>) {
        let caps = Caps { max_width: 3840, max_height: 2160, max_fps: 30, codecs: vec![VideoCodec::H264, VideoCodec::H265], audio: true, sensor: Some("Sony IMX415".into()) };
        let dir = std::env::temp_dir().join(format!("hs-agent-{}", rand::random::<u32>()));
        let cfg = AgentConfig {
            id: "70c9324b2bdc".into(),
            setup_code: "K7PQ3XWD".into(),
            clients: Vec::new(),
            backend: Backend::Test,
            settings: caps.default_settings("Entrée"),
            video_device: None,
            audio_device: None,
            has_mic: None,
            caps: None,
            api_port: 0,
            rtsp_port: 8554,
            tools: Tools::default(),
            custom: Vec::new(),
        };
        let shared = Arc::new(Shared { cfg: std::sync::Mutex::new(cfg), path: dir.join("config.json"), caps, board: "Radxa ROCK 4D".into(), started: Instant::now() });
        let app = App::new(shared.clone(), Arc::new(NoMedia), None);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { homesmart_cam::api::serve(listener, app).await.unwrap() });
        (port, shared)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pair_configure_and_forget_a_homesmart_camera() {
        let (port, shared) = camera().await;
        tokio::task::spawn_blocking(move || {
            // Mauvais code : refusé ici, sans que la caméra n'enregistre rien.
            let e = pair("127.0.0.1", port, "AAAA-AAAA").unwrap_err();
            assert!(format!("{e:#}").contains("code d'appairage incorrect"), "{e:#}");
            assert!(pair("127.0.0.1", port, "trop-court").is_err());

            let (link, info) = pair("127.0.0.1", port, "k7pq-3xwd").unwrap();
            assert_eq!((info.board.as_str(), info.caps.max_width), ("Radxa ROCK 4D", 3840));
            let mut c = CameraConfig::new("127.0.0.1");
            apply_pairing(&mut c, link.clone(), &info);
            assert_eq!((c.brand, c.rtsp_port, c.name.as_str(), c.serial.as_deref()), (Brand::HomeSmart, 8554, "Entrée", Some("70c9324b2bdc")));
            assert_eq!(c.rtsp_url(super::super::Quality::Main), "rtsp://127.0.0.1:8554/main");
            assert_eq!(c.rtsp_url(super::super::Quality::Sub), "rtsp://127.0.0.1:8554/sub");
            // Le mot de passe RTSP de HomeSmart est celui que la caméra donne à son serveur.
            let yml = homesmart_cam::pipeline::mediamtx_yml(&shared.cfg.lock().unwrap());
            assert!(yml.contains(&format!("user: \"{}\"", c.username)) && yml.contains(&format!("pass: \"{}\"", c.password)));

            let st = status(&c).unwrap();
            assert!(st.pipeline.running && st.clients == 1);
            let before = settings(&c).unwrap();
            let mut s = before.clone();
            s.main.width = 7680;
            assert!(format!("{:#}", set_settings(&c, &s).unwrap_err()).contains("dépasse"));
            s.main = homesmart_cam_proto::StreamSettings { width: 1920, height: 1080, fps: 25, kbps: 3000, codec: VideoCodec::H264 };
            s.audio.enabled = false;
            let after = set_settings(&c, &s).unwrap();
            assert_eq!(describe_changes(&before, &after), vec!["flux HD en 1920×1080", "flux HD à 25 i/s", "flux HD à 3000 kbit/s", "flux HD en H.264", "micro coupé"]);
            restart(&c).unwrap();

            // Caméra dont l'horloge retarde d'une heure : HomeSmart s'aligne et la requête passe.
            clock_offsets().lock().unwrap().insert(c.host.clone(), -3600);
            assert!(status(&c).is_ok());
            assert!(clock_offsets().lock().unwrap()[&c.host].abs() < 5);

            unpair(&c).unwrap();
            assert!(format!("{:#}", status(&c).unwrap_err()).contains("plus) appairé"));
            // Plus d'agent : message clair.
            let mut gone = c.clone();
            gone.agent.as_mut().unwrap().api_port = 1;
            assert!(format!("{:#}", status(&gone).unwrap_err()).contains("refuse la connexion"));
        })
        .await
        .unwrap();
    }
}
