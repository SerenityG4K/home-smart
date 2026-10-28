//! `homesmart-cam` : l'agent d'une caméra HomeSmart (à fabriquer soi-même).
//!
//! Il tourne sur la caméra (Radxa ROCK 4D + Camera 4K, ou toute machine Linux avec une
//! webcam) : il lance le serveur RTSP (MediaMTX) et la chaîne qui y publie l'image 4K et le
//! son du micro, s'annonce sur le réseau, et répond à HomeSmart (appairage, réglages, état).
//! `homesmart-cam run --sim` fait la même chose avec une mire de test, même sous Windows.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use homesmart_cam::api::{self, App, Media, Shared};
use homesmart_cam::config::{self, AgentConfig, Backend};
use homesmart_cam::pipeline::{self, ResolvedTools};
use homesmart_cam::{board, mdns, supervisor};
use homesmart_cam_proto::{self as proto, ProcStatus};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Parser)]
#[command(name = "homesmart-cam", version, about = "Agent d'une caméra HomeSmart : flux RTSP avec micro, appairage, réglages")]
struct Cli {
    /// Fichier de configuration (créé au premier lancement)
    #[arg(long, global = true, env = "HOMESMART_CAM_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Lance la caméra (commande par défaut)
    Run {
        /// Mire de test et son simulé au lieu du capteur (essai sans matériel)
        #[arg(long)]
        sim: bool,
    },
    /// Affiche le code d'appairage à taper dans HomeSmart
    Code {
        #[arg(long)]
        sim: bool,
    },
    /// Oublie tous les HomeSmart appairés et tire un nouveau code
    Reset {
        #[arg(long)]
        sim: bool,
    },
    /// Affiche les commandes qui seraient lancées (diagnostic)
    Pipeline {
        #[arg(long)]
        sim: bool,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).with_target(false).init();
    let cli = Cli::parse();
    let cmd = cli.cmd.unwrap_or(Cmd::Run { sim: false });
    let sim = matches!(cmd, Cmd::Run { sim: true } | Cmd::Code { sim: true } | Cmd::Reset { sim: true } | Cmd::Pipeline { sim: true });
    // En simulation, une configuration à part : celle de la vraie caméra reste intacte.
    let path = cli.config.unwrap_or_else(|| {
        let p = config::default_path();
        if sim {
            p.with_file_name("config-sim.json")
        } else {
            p
        }
    });
    match cmd {
        Cmd::Run { .. } => tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run(&path, sim)),
        Cmd::Code { .. } => {
            let (cfg, _) = load_or_create(&path, sim)?;
            println!("Code d'appairage : {}", proto::format_code(&cfg.setup_code));
            println!("HomeSmart appairés : {}", cfg.clients.len());
            Ok(())
        }
        Cmd::Reset { .. } => {
            let (mut cfg, _) = load_or_create(&path, sim)?;
            cfg.clients.clear();
            cfg.setup_code = proto::new_code();
            cfg.save(&path)?;
            println!("Appairages oubliés. Nouveau code : {}", proto::format_code(&cfg.setup_code));
            println!("Relance l'agent (systemctl restart homesmart-cam) pour l'appliquer.");
            Ok(())
        }
        Cmd::Pipeline { .. } => {
            let (cfg, _) = load_or_create(&path, sim)?;
            let tools = resolve_tools(&cfg);
            let caps = board::caps(&cfg, has_x265(&cfg, &tools));
            println!("Backend : {}", cfg.backend.label());
            println!("Matériel : {}", serde_json::to_string(&caps)?);
            match pipeline::publisher(&cfg, &caps, &tools) {
                Ok(a) => println!("Chaîne d'encodage :\n  {}", a.join(" ")),
                Err(e) => println!("Chaîne d'encodage : {e:#}"),
            }
            let masked = cfg.clients.iter().map(|c| proto::secret_from_hex(&c.secret).map(|s| proto::rtsp_password(&s))).flatten().fold(pipeline::mediamtx_yml(&cfg), |y, pass| y.replace(&pass, "********"));
            println!("Serveur RTSP ({}) :\n{masked}", tools.mediamtx.as_ref().map_or("MediaMTX introuvable".into(), |p| p.display().to_string()));
            Ok(())
        }
    }
}

/// Lit la configuration, ou la crée (identifiant, code, backend détecté, réglages de départ).
/// Le booléen dit si elle vient d'être créée.
fn load_or_create(path: &Path, sim: bool) -> Result<(AgentConfig, bool)> {
    if let Some(cfg) = AgentConfig::load(path)? {
        return Ok((cfg, false));
    }
    let backend = if sim { Backend::Test } else { board::detect_backend() };
    let mut cfg = AgentConfig {
        id: board::machine_id(),
        setup_code: proto::new_code(),
        clients: Vec::new(),
        backend,
        settings: proto::Caps { max_width: 1920, max_height: 1080, max_fps: 30, codecs: vec![proto::VideoCodec::H264], audio: false, sensor: None }.default_settings(api::MODEL),
        video_device: None,
        audio_device: None,
        has_mic: None,
        caps: None,
        api_port: proto::API_PORT,
        rtsp_port: proto::RTSP_PORT,
        tools: config::Tools::default(),
        custom: Vec::new(),
    };
    let tools = resolve_tools(&cfg);
    cfg.settings = board::caps(&cfg, has_x265(&cfg, &tools)).default_settings(if sim { "Caméra d'essai" } else { api::MODEL });
    cfg.save(path)?;
    tracing::info!("configuration créée : {}", path.display());
    Ok((cfg, true))
}

fn resolve_tools(cfg: &AgentConfig) -> ResolvedTools {
    ResolvedTools {
        mediamtx: board::find_tool(cfg.tools.mediamtx.as_ref(), "mediamtx"),
        ffmpeg: board::find_tool(cfg.tools.ffmpeg.as_ref(), "ffmpeg"),
        gst_launch: board::find_tool(cfg.tools.gst_launch.as_ref(), "gst-launch-1.0"),
    }
}

fn has_x265(cfg: &AgentConfig, tools: &ResolvedTools) -> bool {
    matches!(cfg.backend, Backend::V4l2 | Backend::Test) && tools.ffmpeg.as_deref().is_some_and(board::ffmpeg_has_x265)
}

/// Les deux processus surveillés.
struct Supervised {
    server: Arc<supervisor::Proc>,
    pipeline: Arc<supervisor::Proc>,
    shared: Arc<Shared>,
    yml: PathBuf,
}

impl Media for Supervised {
    fn restart_pipeline(&self) {
        self.pipeline.restart();
    }

    fn clients_changed(&self) {
        // MediaMTX relit sa configuration quand le fichier change ; on le relance quand même
        // si l'écriture échoue (la version sur disque serait périmée).
        if let Err(e) = write_yml(&self.shared, &self.yml) {
            tracing::warn!("{e:#}");
            self.server.restart();
        }
    }

    fn status(&self) -> (ProcStatus, ProcStatus) {
        (self.pipeline.status(), self.server.status())
    }
}

fn write_yml(shared: &Shared, path: &Path) -> Result<()> {
    let yml = pipeline::mediamtx_yml(&shared.cfg.lock().unwrap());
    config::write_private(path, yml.as_bytes()).context("configuration du serveur RTSP")
}

async fn run(path: &Path, sim: bool) -> Result<()> {
    let (mut cfg, created) = load_or_create(path, sim)?;
    let tools = resolve_tools(&cfg);
    let caps = board::caps(&cfg, has_x265(&cfg, &tools));
    // Réglages enregistrés qui ne tiennent plus (autre matériel) : on repart des réglages de départ.
    if let Err(e) = cfg.settings.validate(&caps) {
        tracing::warn!("réglages incompatibles avec ce matériel ({e}) : retour aux réglages de départ");
        cfg.settings = caps.default_settings(&cfg.settings.name);
        cfg.save(path)?;
    }
    let (api_port, rtsp_port, backend) = (cfg.api_port, cfg.rtsp_port, cfg.backend);
    let code = proto::format_code(&cfg.setup_code);
    let paired = cfg.clients.len();
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let yml = dir.join("mediamtx.yml");
    let shared = Arc::new(Shared { cfg: Mutex::new(cfg), path: path.to_path_buf(), caps: caps.clone(), board: board::board_name(), started: Instant::now() });

    let (s, y, t) = (shared.clone(), yml.clone(), tools.clone());
    let server = supervisor::spawn(
        "serveur RTSP",
        Arc::new(move || {
            write_yml(&s, &y)?;
            Ok(Some(pipeline::server(&t, &y)?))
        }),
        None,
    );
    let (s, t, c) = (shared.clone(), tools.clone(), caps.clone());
    let pipeline = supervisor::spawn("chaîne d'encodage", Arc::new(move || Ok(Some(pipeline::publisher(&s.cfg.lock().unwrap(), &c, &t)?))), Some(rtsp_port));
    let media = Arc::new(Supervised { server, pipeline, shared: shared.clone(), yml });

    let announcer = match mdns::Announcer::new() {
        Ok(a) => Some(Arc::new(a)),
        Err(e) => {
            tracing::warn!("annonce mDNS impossible ({e:#}) : ajoute la caméra dans HomeSmart par son adresse IP");
            None
        }
    };
    let on_change: Option<Arc<dyn Fn() + Send + Sync>> = announcer.clone().map(|a| {
        let s = shared.clone();
        Arc::new(move || {
            if let Err(e) = a.publish(&s) {
                tracing::warn!("annonce mDNS : {e:#}");
            }
        }) as Arc<dyn Fn() + Send + Sync>
    });
    if let Some(f) = &on_change {
        f();
    }
    let app = App::new(shared.clone(), media, on_change);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", api_port)).await.with_context(|| format!("port {api_port} déjà pris"))?;
    tracing::info!("caméra prête ({}, {}×{}) : API sur le port {api_port}, RTSP sur le port {rtsp_port}", backend.label(), caps.max_width, caps.max_height);
    if created || paired == 0 {
        tracing::info!("code d'appairage à taper dans HomeSmart : {code}");
    }
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("arrêt demandé");
    };
    axum::serve(listener, api::router(app)).with_graceful_shutdown(shutdown).await?;
    drop(announcer);
    Ok(())
}
