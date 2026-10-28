//! HomeSmart : application de bureau (Windows / Linux) pour piloter en Wi-Fi local
//! les appareils Tuya / Smart Life, sans compte ni cloud.

// Pas de console noire derrière la fenêtre sous Windows (en version optimisée).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod gui;

use clap::Parser;
use homesmart::config;
use homesmart::hub::Hub;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "homesmart", version, about = "Pilote tes appareils Tuya / Smart Life en Wi-Fi local, sans compte ni cloud")]
struct Args {
    /// Fichier de configuration (appareils + clés locales)
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Lance l'application avec des appareils simulés (aucun matériel requis)
    #[arg(long)]
    demo: bool,
    /// Démarre réduite (lancement automatique à l'ouverture de session)
    #[arg(long)]
    minimized: bool,
}

/// Charge le hub ; si la configuration est illisible, on la met de côté plutôt que de planter.
fn load_hub(path: PathBuf) -> (Arc<Hub>, Option<String>) {
    match Hub::new(path.clone()) {
        Ok(h) => (h, None),
        Err(e) => {
            let backup = path.with_extension("json.illisible");
            let _ = std::fs::rename(&path, &backup);
            let notice = format!("Configuration illisible ({e:#}), mise de côté dans {}", backup.display());
            (Hub::new(path).expect("configuration vide"), Some(notice))
        }
    }
}

fn main() -> eframe::Result {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    // Le réseau tourne sur un runtime tokio en arrière-plan ; l'interface sur le fil principal.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime tokio");
    let _guard = rt.enter();

    let path = if args.demo {
        rt.block_on(homesmart::demo::prepare()).unwrap_or_else(|e| {
            eprintln!("démo impossible : {e:#}");
            std::process::exit(1);
        })
    } else {
        args.config.unwrap_or_else(config::default_path)
    };
    // Une seule instance sur les vrais appareils (la démo, elle, a les siens).
    let instance = if args.demo { None } else { homesmart::instance::acquire() };
    let (hub, mut notice) = load_hub(path.clone());
    if !args.demo && instance.is_none() {
        notice = Some(homesmart::instance::ALREADY_RUNNING.to_string());
    }
    if args.demo {
        homesmart::demo::seed_energy(&hub);
        // Démo : la détection des problèmes réagit en quelques secondes.
        hub.set_fast_detection(true);
    }
    hub.start(true);
    if args.demo {
        homesmart::demo::seed_schedules(&hub);
        if std::env::var_os("HOMESMART_DEMO_PANNES").is_some() {
            rt.spawn(homesmart::demo::incidents(hub.clone()));
        }
    }

    // HOMESMART_SIZE=900x600 : taille de départ de la fenêtre (captures d'écran, essais de mise en page).
    let size = std::env::var("HOMESMART_SIZE")
        .ok()
        .and_then(|v| {
            let (w, h) = v.split_once('x')?;
            Some([w.parse().ok()?, h.parse().ok()?])
        })
        .unwrap_or([1200.0, 780.0]);
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("HomeSmart")
            .with_app_id("homesmart")
            .with_inner_size(size)
            .with_min_inner_size([780.0, 520.0])
            .with_icon(gui::theme::window_icon()),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    let handle = rt.handle().clone();
    let demo = args.demo;
    let minimized = args.minimized;
    eframe::run_native(
        "HomeSmart",
        options,
        Box::new(move |cc| Ok(Box::new(gui::App::new(cc, handle, hub, demo, path, notice, minimized)))),
    )
}
