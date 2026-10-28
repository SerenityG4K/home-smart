//! API HTTP de la caméra.
//!
//! Publiques : `GET /api/info`, `POST /api/pair/start`, `POST /api/pair/finish`.
//! Signées (voir `homesmart_cam_proto`) : `GET /api/status`, `GET|PUT /api/settings`,
//! `POST /api/restart`, `DELETE /api/pair` (oublier ce HomeSmart).

use crate::config::{new_client_id, unix_now, AgentConfig, Client};
use axum::body::{to_bytes, Body};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use homesmart_cam_proto::{self as proto, ApiError, CamAnswer, Caps, Info, PairChallenge, PairFinish, PairStart, Paired, ProcStatus, Settings, Status};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BODY_LIMIT: usize = 64 * 1024;
/// Essais d'appairage ratés tolérés sur `PAIR_WINDOW` avant de refuser.
const PAIR_MAX_ATTEMPTS: usize = 5;
const PAIR_WINDOW: Duration = Duration::from_secs(600);
/// Délai pour terminer un appairage commencé.
const PAIR_TTL: Duration = Duration::from_secs(60);

pub const MODEL: &str = "Caméra HomeSmart";

/// Ce que l'API partage avec le reste de l'agent.
pub struct Shared {
    pub cfg: Mutex<AgentConfig>,
    pub path: PathBuf,
    pub caps: Caps,
    pub board: String,
    pub started: Instant,
}

impl Shared {
    pub fn info(&self) -> Info {
        let cfg = self.cfg.lock().unwrap();
        Info {
            id: cfg.id.clone(),
            name: cfg.settings.name.clone(),
            model: MODEL.into(),
            board: self.board.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
            protocol: proto::PROTOCOL,
            paired: !cfg.clients.is_empty(),
            rtsp_port: cfg.rtsp_port,
            caps: self.caps.clone(),
        }
    }

    fn save(&self, cfg: &AgentConfig) -> Result<(), ApiErr> {
        cfg.save(&self.path).map_err(|e| ApiErr(StatusCode::INTERNAL_SERVER_ERROR, format!("enregistrement de la configuration : {e:#}")))
    }
}

/// Les processus de l'image et du son, vus de l'API.
pub trait Media: Send + Sync {
    fn restart_pipeline(&self);
    /// Les HomeSmart appairés ont changé (accès au serveur RTSP).
    fn clients_changed(&self);
    /// (chaîne d'encodage, serveur RTSP)
    fn status(&self) -> (ProcStatus, ProcStatus);
}

struct Pending {
    answer: CamAnswer,
    client_name: String,
    created: Instant,
}

#[derive(Default)]
struct Pairing {
    pending: HashMap<String, Pending>,
    /// Appairages commencés et pas (encore) réussis : (identifiant, date).
    attempts: Vec<(String, Instant)>,
}

pub struct App {
    pub shared: Arc<Shared>,
    media: Arc<dyn Media>,
    pairing: Mutex<Pairing>,
    /// Nonces déjà vus (anti-rejeu) : « client:nonce » → heure annoncée.
    nonces: Mutex<HashMap<String, i64>>,
    /// Appelé quand le nom ou l'état d'appairage change (annonce mDNS).
    on_change: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl App {
    pub fn new(shared: Arc<Shared>, media: Arc<dyn Media>, on_change: Option<Arc<dyn Fn() + Send + Sync>>) -> Arc<App> {
        Arc::new(App { shared, media, pairing: Mutex::default(), nonces: Mutex::default(), on_change })
    }

    fn changed(&self) {
        if let Some(f) = &self.on_change {
            f();
        }
    }
}

/// Erreur renvoyée en JSON (`{"error": "…"}`).
pub struct ApiErr(StatusCode, String);

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.0, Json(ApiError { error: self.1 })).into_response()
    }
}

#[derive(Clone)]
struct ClientId(String);

pub fn router(app: Arc<App>) -> Router {
    let signed = Router::new()
        .route("/api/status", get(status))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/restart", post(restart))
        .route("/api/pair", delete(unpair))
        .layer(middleware::from_fn_with_state(app.clone(), auth));
    Router::new()
        .route("/api/info", get(info))
        .route("/api/pair/start", post(pair_start))
        .route("/api/pair/finish", post(pair_finish))
        .merge(signed)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(app)
}

/// Sert l'API sur `listener` jusqu'à l'abandon du futur (tests de HomeSmart).
pub async fn serve(listener: tokio::net::TcpListener, app: Arc<App>) -> std::io::Result<()> {
    axum::serve(listener, router(app)).await
}

fn unauthorized(msg: &str) -> Response {
    ApiErr(StatusCode::UNAUTHORIZED, msg.into()).into_response()
}

/// Vérifie la signature, l'heure et le nonce d'une requête.
async fn auth(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let (parts, body) = req.into_parts();
    let header = |k: &str| parts.headers.get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
    let (Some(client), Some(time), Some(nonce), Some(sig)) = (header(proto::H_CLIENT), header(proto::H_TIME), header(proto::H_NONCE), header(proto::H_SIG)) else {
        return unauthorized("requête non signée");
    };
    let Ok(time) = time.parse::<i64>() else { return unauthorized("heure illisible") };
    let now = unix_now() as i64;
    if (now - time).abs() > proto::MAX_CLOCK_SKEW_S {
        // L'heure de la caméra accompagne le refus : HomeSmart corrige l'écart et recommence.
        let mut r = unauthorized("horloge décalée entre HomeSmart et la caméra");
        r.headers_mut().insert(proto::H_TIME, HeaderValue::from(now));
        return r;
    }
    let Some(secret) = app.shared.cfg.lock().unwrap().client(&client).and_then(Client::secret_bytes) else {
        return unauthorized("ce HomeSmart n'est pas (ou plus) appairé à la caméra");
    };
    let Ok(bytes) = to_bytes(body, BODY_LIMIT).await else {
        return ApiErr(StatusCode::PAYLOAD_TOO_LARGE, "requête trop grosse".into()).into_response();
    };
    let path = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if !proto::verify_signature(&secret, parts.method.as_str(), path, time, &nonce, &bytes, &sig) {
        return unauthorized("signature invalide");
    }
    {
        let mut seen = app.nonces.lock().unwrap();
        seen.retain(|_, t| (now - *t).abs() <= 2 * proto::MAX_CLOCK_SKEW_S);
        if seen.insert(format!("{client}:{nonce}"), time).is_some() {
            return unauthorized("requête rejouée");
        }
    }
    let mut req = Request::from_parts(parts, Body::from(bytes));
    req.extensions_mut().insert(ClientId(client));
    next.run(req).await
}

async fn info(State(app): State<Arc<App>>) -> Json<Info> {
    Json(app.shared.info())
}

async fn pair_start(State(app): State<Arc<App>>, Json(req): Json<PairStart>) -> Result<Json<PairChallenge>, ApiErr> {
    let client_name = req.client_name.trim().to_string();
    if client_name.is_empty() || client_name.chars().count() > 64 {
        return Err(ApiErr(StatusCode::BAD_REQUEST, "nom du HomeSmart manquant ou trop long".into()));
    }
    let code = app.shared.cfg.lock().unwrap().setup_code.clone();
    let mut p = app.pairing.lock().unwrap();
    p.attempts.retain(|(_, t)| t.elapsed() < PAIR_WINDOW);
    p.pending.retain(|_, x| x.created.elapsed() < PAIR_TTL);
    if p.attempts.len() >= PAIR_MAX_ATTEMPTS {
        let wait = p.attempts.first().map_or(PAIR_WINDOW, |(_, t)| PAIR_WINDOW.saturating_sub(t.elapsed()));
        return Err(ApiErr(StatusCode::TOO_MANY_REQUESTS, format!("trop d'essais d'appairage ratés : la caméra refuse pendant encore {} min", wait.as_secs().div_ceil(60))));
    }
    let answer = CamAnswer::new(&code, &req).map_err(|e| ApiErr(StatusCode::BAD_REQUEST, e.to_string()))?;
    let pair_id = hex::encode(rand::random::<[u8; 8]>());
    let challenge = PairChallenge { pair_id: pair_id.clone(), msg: answer.msg.clone(), confirm: answer.confirm.clone() };
    p.attempts.push((pair_id.clone(), Instant::now()));
    p.pending.insert(pair_id, Pending { answer, client_name, created: Instant::now() });
    Ok(Json(challenge))
}

async fn pair_finish(State(app): State<Arc<App>>, Json(fin): Json<PairFinish>) -> Result<Json<Paired>, ApiErr> {
    let pending = {
        let mut p = app.pairing.lock().unwrap();
        let pending = p.pending.remove(&fin.pair_id).filter(|x| x.created.elapsed() < PAIR_TTL).ok_or_else(|| ApiErr(StatusCode::GONE, "appairage expiré : recommence".into()))?;
        if !pending.answer.check(&fin) {
            return Err(ApiErr(StatusCode::FORBIDDEN, "code d'appairage incorrect".into()));
        }
        p.attempts.retain(|(id, _)| id != &fin.pair_id);
        pending
    };
    let client = Client { id: new_client_id(), name: pending.client_name, secret: proto::secret_to_hex(&pending.answer.secret), paired_at: unix_now() };
    let id = client.id.clone();
    {
        let mut cfg = app.shared.cfg.lock().unwrap();
        cfg.clients.push(client);
        app.shared.save(&cfg)?;
    }
    tracing::info!("HomeSmart appairé : {id}");
    app.media.clients_changed();
    app.changed();
    Ok(Json(Paired { client_id: id }))
}

async fn status(State(app): State<Arc<App>>) -> Json<Status> {
    let (pipeline, server) = app.media.status();
    Json(Status {
        uptime_s: crate::board::uptime_s(app.shared.started),
        cpu_temp_c: crate::board::cpu_temp_c(),
        load: crate::board::load_1min(),
        pipeline,
        server,
        clients: app.shared.cfg.lock().unwrap().clients.len(),
    })
}

async fn get_settings(State(app): State<Arc<App>>) -> Json<Settings> {
    Json(app.shared.cfg.lock().unwrap().settings.clone())
}

async fn put_settings(State(app): State<Arc<App>>, Json(mut s): Json<Settings>) -> Result<Json<Settings>, ApiErr> {
    s.name = s.name.trim().to_string();
    s.validate(&app.shared.caps).map_err(|e| ApiErr(StatusCode::BAD_REQUEST, e))?;
    let renamed = {
        let mut cfg = app.shared.cfg.lock().unwrap();
        let renamed = cfg.settings.name != s.name;
        cfg.settings = s.clone();
        app.shared.save(&cfg)?;
        renamed
    };
    app.media.restart_pipeline();
    if renamed {
        app.changed();
    }
    Ok(Json(s))
}

async fn restart(State(app): State<Arc<App>>) -> StatusCode {
    app.media.restart_pipeline();
    StatusCode::NO_CONTENT
}

async fn unpair(State(app): State<Arc<App>>, Extension(ClientId(id)): Extension<ClientId>) -> Result<StatusCode, ApiErr> {
    {
        let mut cfg = app.shared.cfg.lock().unwrap();
        cfg.clients.retain(|c| c.id != id);
        app.shared.save(&cfg)?;
    }
    tracing::info!("HomeSmart désappairé : {id}");
    app.media.clients_changed();
    app.changed();
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Backend, Tools};
    use homesmart_cam_proto::{HubPairing, VideoCodec};
    use std::sync::atomic::{AtomicU32, Ordering};

    #[derive(Default)]
    struct FakeMedia {
        restarts: AtomicU32,
        clients: AtomicU32,
    }

    impl Media for FakeMedia {
        fn restart_pipeline(&self) {
            self.restarts.fetch_add(1, Ordering::Relaxed);
        }
        fn clients_changed(&self) {
            self.clients.fetch_add(1, Ordering::Relaxed);
        }
        fn status(&self) -> (ProcStatus, ProcStatus) {
            (ProcStatus { running: true, ..ProcStatus::default() }, ProcStatus { running: true, ..ProcStatus::default() })
        }
    }

    struct Cam {
        base: String,
        media: Arc<FakeMedia>,
        shared: Arc<Shared>,
        dir: PathBuf,
    }

    impl Drop for Cam {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    async fn start() -> Cam {
        let dir = std::env::temp_dir().join(format!("hscam-api-{}", rand::random::<u32>()));
        let caps = Caps { max_width: 3840, max_height: 2160, max_fps: 30, codecs: vec![VideoCodec::H264, VideoCodec::H265], audio: true, sensor: None };
        let cfg = AgentConfig {
            id: "aabbccddeeff".into(),
            setup_code: "ABCDEFGH".into(),
            clients: Vec::new(),
            backend: Backend::Test,
            settings: caps.default_settings("Salon"),
            video_device: None,
            audio_device: None,
            has_mic: None,
            caps: None,
            api_port: 0,
            rtsp_port: 8554,
            tools: Tools::default(),
            custom: Vec::new(),
        };
        let shared = Arc::new(Shared { cfg: Mutex::new(cfg), path: dir.join("config.json"), caps, board: "Essai".into(), started: Instant::now() });
        let media = Arc::new(FakeMedia::default());
        let app = App::new(shared.clone(), media.clone(), None);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router(app)).await.unwrap() });
        Cam { base, media, shared, dir }
    }

    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder().http_status_as_error(false).build().into()
    }

    /// Appairage complet avec `code` ; renvoie (client, secret) ou le statut d'erreur.
    fn pair(base: &str, code: &str) -> Result<(String, [u8; 32]), u16> {
        let a = agent();
        let (hub, start) = HubPairing::start(code, "PC de test");
        let mut r = a.post(format!("{base}/api/pair/start")).send_json(&start).unwrap();
        if r.status() != 200 {
            return Err(r.status().as_u16());
        }
        let ch: PairChallenge = r.body_mut().read_json().unwrap();
        let (secret, fin) = hub.finish(&ch).map_err(|_| 403u16)?;
        let mut r = a.post(format!("{base}/api/pair/finish")).send_json(&fin).unwrap();
        if r.status() != 200 {
            return Err(r.status().as_u16());
        }
        let p: Paired = r.body_mut().read_json().unwrap();
        Ok((p.client_id, secret))
    }

    fn signed(base: &str, method: &str, path: &str, client: &str, secret: &[u8; 32], time: i64, nonce: &str, body: &[u8]) -> (u16, String) {
        let sig = proto::signature(secret, method, path, time, nonce, body);
        let a = agent();
        let url = format!("{base}{path}");
        let headers = [(proto::H_CLIENT, client.to_string()), (proto::H_TIME, time.to_string()), (proto::H_NONCE, nonce.to_string()), (proto::H_SIG, sig)];
        let mut r = match method {
            "GET" => headers.iter().fold(a.get(&url), |r, (k, v)| r.header(*k, v)).call().unwrap(),
            "DELETE" => headers.iter().fold(a.delete(&url), |r, (k, v)| r.header(*k, v)).call().unwrap(),
            "PUT" => headers.iter().fold(a.put(&url), |r, (k, v)| r.header(*k, v)).header("content-type", "application/json").send(body).unwrap(),
            _ => headers.iter().fold(a.post(&url), |r, (k, v)| r.header(*k, v)).send(body).unwrap(),
        };
        (r.status().as_u16(), r.body_mut().read_to_string().unwrap_or_default())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pairing_then_signed_requests() {
        let cam = start().await;
        let base = cam.base.clone();
        let media = cam.media.clone();
        let shared = cam.shared.clone();
        tokio::task::spawn_blocking(move || {
            let info: Info = agent().get(format!("{base}/api/info")).call().unwrap().body_mut().read_json().unwrap();
            assert!(!info.paired && info.model == MODEL && info.caps.max_width == 3840);

            let (client, secret) = pair(&base, "abcd-efgh").unwrap();
            assert!(client.starts_with("hs-"));
            assert_eq!(media.clients.load(Ordering::Relaxed), 1);
            assert_eq!(shared.cfg.lock().unwrap().clients.len(), 1);
            let info: Info = agent().get(format!("{base}/api/info")).call().unwrap().body_mut().read_json().unwrap();
            assert!(info.paired);

            let now = unix_now() as i64;
            let (code, body) = signed(&base, "GET", "/api/status", &client, &secret, now, "n1", b"");
            assert_eq!(code, 200, "{body}");
            let st: Status = serde_json::from_str(&body).unwrap();
            assert!(st.pipeline.running && st.clients == 1);
            // Rejeu, requête non signée, mauvais secret, horloge décalée.
            assert_eq!(signed(&base, "GET", "/api/status", &client, &secret, now, "n1", b"").0, 401);
            assert_eq!(agent().get(format!("{base}/api/status")).call().unwrap().status(), 401);
            assert_eq!(signed(&base, "GET", "/api/status", &client, &[9u8; 32], now, "n2", b"").0, 401);
            let a = agent();
            let late = now - 3600;
            let sig = proto::signature(&secret, "GET", "/api/status", late, "n3", b"");
            let r = a.get(format!("{base}/api/status")).header(proto::H_CLIENT, &client).header(proto::H_TIME, late.to_string()).header(proto::H_NONCE, "n3").header(proto::H_SIG, sig).call().unwrap();
            assert_eq!(r.status(), 401);
            let cam_time: i64 = r.headers().get(proto::H_TIME).unwrap().to_str().unwrap().parse().unwrap();
            assert!((cam_time - now).abs() < 5);

            // Réglages : refusés s'ils dépassent le matériel, appliqués sinon (chaîne relancée).
            let (code, body) = signed(&base, "GET", "/api/settings", &client, &secret, now, "n4", b"");
            assert_eq!(code, 200);
            let mut s: Settings = serde_json::from_str(&body).unwrap();
            s.main.fps = 60;
            let bad = serde_json::to_vec(&s).unwrap();
            let (code, body) = signed(&base, "PUT", "/api/settings", &client, &secret, now, "n5", &bad);
            assert_eq!(code, 400, "{body}");
            assert!(body.contains("images/s"), "{body}");
            s.main.fps = 25;
            s.name = "Entrée".into();
            let good = serde_json::to_vec(&s).unwrap();
            let (code, body) = signed(&base, "PUT", "/api/settings", &client, &secret, now, "n6", &good);
            assert_eq!(code, 200, "{body}");
            assert_eq!(media.restarts.load(Ordering::Relaxed), 1);
            assert_eq!(shared.cfg.lock().unwrap().settings.name, "Entrée");
            // Un corps modifié après signature est refusé.
            let sig = proto::signature(&secret, "PUT", "/api/settings", now, "n7", &good);
            let r = agent().put(format!("{base}/api/settings")).header(proto::H_CLIENT, &client).header(proto::H_TIME, now.to_string()).header(proto::H_NONCE, "n7").header(proto::H_SIG, sig).header("content-type", "application/json").send(&bad[..]).unwrap();
            assert_eq!(r.status(), 401);

            // Oublier ce HomeSmart : ses requêtes sont ensuite refusées.
            assert_eq!(signed(&base, "DELETE", "/api/pair", &client, &secret, now, "n8", b"").0, 204);
            assert_eq!(signed(&base, "GET", "/api/status", &client, &secret, now, "n9", b"").0, 401);
            assert!(shared.cfg.lock().unwrap().clients.is_empty());
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wrong_codes_lock_pairing() {
        let cam = start().await;
        let base = cam.base.clone();
        tokio::task::spawn_blocking(move || {
            // HomeSmart voit tout de suite que le code est faux (preuve de la caméra fausse).
            for _ in 0..PAIR_MAX_ATTEMPTS {
                assert_eq!(pair(&base, "ZZZZ-ZZZZ"), Err(403));
            }
            // Ensuite, même le bon code est refusé un moment.
            assert_eq!(pair(&base, "ABCD-EFGH"), Err(429));
        })
        .await
        .unwrap();
        assert!(cam.shared.cfg.lock().unwrap().clients.is_empty());
    }
}
