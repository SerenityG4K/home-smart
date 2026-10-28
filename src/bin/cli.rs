//! homesmart-cli : pilotage des appareils en ligne de commande (scripts, raccourcis).
//!
//! Chaque commande ouvre sa propre connexion Wi-Fi vers l'appareil. Beaucoup
//! d'appareils n'acceptent qu'une connexion à la fois : si l'application de
//! bureau est ouverte, l'appareil peut refuser la CLI (ou inversement).

#[path = "cli/cam.rs"]
mod cam;
#[path = "cli/eval.rs"]
mod eval;
#[path = "cli/follow.rs"]
mod follow;
#[path = "cli/listen.rs"]
mod listen;
#[path = "cli/tplink.rs"]
mod tplink;
#[path = "cli/tui.rs"]
mod tui;
#[path = "cli/vac.rs"]
mod vac;
#[path = "cli/vision.rs"]
mod vision;
#[path = "cli/violence.rs"]
mod violence;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use homesmart::config::{self, Config, DeviceConfig};
use homesmart::device::{self, Target};
use homesmart::discovery;
use homesmart::journal::{self, Entry, Level, Source};
use homesmart::kinds;
use homesmart::protocol::{codec, Version, PORT_TCP};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "homesmart-cli", version, about = "Pilote tes appareils Tuya / Smart Life en Wi-Fi local, sans compte ni cloud")]
struct Cli {
    /// Fichier de configuration (par défaut : celui de l'application)
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Interface plein écran dans le terminal : appareils, caméras, énergie, journal
    Tui {
        /// Appareils et caméras simulés (aucun matériel requis)
        #[arg(long)]
        demo: bool,
    },
    /// Cherche les appareils Tuya qui s'annoncent sur le réseau Wi-Fi
    Scan {
        #[arg(long, default_value_t = 8)]
        secs: u64,
    },
    /// Liste les appareils configurés
    List,
    /// Ajoute ou met à jour un appareil
    Add {
        id: String,
        /// Clé locale (16 caractères)
        key: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        ip: Option<String>,
        /// 3.1, 3.2, 3.3, 3.4 ou 3.5
        #[arg(long)]
        version: Option<String>,
        #[arg(long)]
        room: Option<String>,
        /// light, plug, switch, cover ou generic
        #[arg(long)]
        kind: Option<String>,
    },
    /// Importe le devices.json produit par tinytuya
    Import { file: PathBuf },
    /// Récupère une seule fois les clés locales depuis ton compte Smart Life (QR code)
    ImportCloud {
        /// Smart Life : Moi → Paramètres → Compte et sécurité → Code utilisateur
        user_code: String,
    },
    /// Supprime un appareil
    Remove { device: String },
    /// Affiche l'état (les DP) d'un appareil
    Status { device: String },
    /// Allume un appareil
    On {
        device: String,
        #[arg(long)]
        dp: Option<String>,
    },
    /// Éteint un appareil
    Off {
        device: String,
        #[arg(long)]
        dp: Option<String>,
    },
    /// Écrit une valeur de DP, ex. : homesmart-cli set "Lampe du salon" 22 500
    /// (les réglages d'usine, comme l'étalonnage des prises, sont refusés)
    Set { device: String, dp: String, value: String },
    /// Caméras IP (Imou / Dahua, RTSP + ONVIF) : scan, add, test, snapshot, move, goto…
    Cam {
        #[command(subcommand)]
        cmd: cam::CamCmd,
    },
    /// Appareils TP-Link en local (Tapo, Kasa) : recherche, état, marche/arrêt
    Tplink {
        #[command(subcommand)]
        cmd: tplink::TplinkCmd,
    },
    /// Aspirateurs robots en local (Tuya, Valetudo, miIO) : scan, status, start, dock, add-valetudo…
    Vac {
        #[command(subcommand)]
        cmd: vac::VacCmd,
    },
    /// Vision par IA, en local : modèles (installation, état), essais
    Vision {
        #[command(subcommand)]
        cmd: vision::VisionCmd,
    },
    /// Écoute par IA, en local : sons de la maison, parole → texte (fichiers WAV, mesures)
    Listen {
        #[command(subcommand)]
        cmd: listen::ListenCmd,
    },
    /// Affiche les mesures en direct (puissance, courant, tension) et le rythme réel de la prise
    Watch {
        device: String,
        /// Intervalle de demande des mesures, en secondes
        #[arg(long, default_value_t = 2.0)]
        interval: f64,
        /// Arrêter au bout de N secondes (sinon Ctrl+C)
        #[arg(long)]
        secs: Option<u64>,
    },
}

fn main() {
    // Le CLI a beaucoup de sous-commandes : la machine à états asynchrone est volumineuse et
    // déborde la pile du fil principal (1 Mo sous Windows) en build debug. On l'exécute donc sur
    // un fil à grande pile.
    let worker = std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(run_cli).expect("démarrage du fil principal");
    if worker.join().is_err() {
        std::process::exit(1);
    }
}

#[tokio::main]
async fn run_cli() {
    let cli = Cli::parse();
    // La TUI occupe tout le terminal : aucun message de suivi ne doit s'y imprimer.
    if let Cmd::Tui { demo } = cli.cmd {
        let path = cli.config.clone().unwrap_or_else(config::default_path);
        if let Err(e) = tui::run(&path, demo).await {
            eprintln!("erreur : {e:#}");
            std::process::exit(1);
        }
        return;
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();
    let path = cli.config.clone().unwrap_or_else(config::default_path);
    if let Err(e) = run(cli.cmd, &path).await {
        eprintln!("erreur : {e:#}");
        std::process::exit(1);
    }
}

async fn run(cmd: Cmd, path: &Path) -> Result<()> {
    match cmd {
        Cmd::Tui { demo } => tui::run(path, demo).await,
        Cmd::Scan { secs } => scan(path, secs).await,
        Cmd::List => list(path),
        Cmd::Add { id, key, name, ip, version, room, kind } => {
            codec::parse_key(&key)?;
            if let Some(v) = &version {
                v.parse::<Version>()?;
            }
            let mut cfg = Config::load(path)?;
            let dev = DeviceConfig { name: name.unwrap_or_else(|| id.clone()), id, key, ip, port: None, version, room, kind, schema: Map::new(), safety: Default::default(), powers_pc: false };
            let label = dev.display_name().to_string();
            let dev_id = dev.id.clone();
            let created = cfg.upsert(dev);
            cfg.save(path)?;
            jlog(path, Level::Info, &dev_id, &label, if created { "Appareil ajouté" } else { "Appareil mis à jour" });
            println!("{} « {label} » ({})", if created { "Ajouté" } else { "Mis à jour" }, path.display());
            Ok(())
        }
        Cmd::Import { file } => {
            let devices = config::import_tinytuya(&file)?;
            let mut cfg = Config::load(path)?;
            let n = devices.len();
            for d in devices {
                println!("  + {} ({})", d.display_name(), d.id);
                jlog(path, Level::Info, &d.id, d.display_name(), format!("Appareil importé depuis {}", file.display()));
                cfg.upsert(d);
            }
            cfg.save(path)?;
            println!("{n} appareil(s) importé(s) dans {}", path.display());
            Ok(())
        }
        Cmd::ImportCloud { user_code } => import_cloud(path, &user_code).await,
        Cmd::Remove { device } => {
            let mut cfg = Config::load(path)?;
            let (id, name) = cfg.find(&device).map(|d| (d.id.clone(), d.display_name().to_string())).ok_or_else(|| anyhow!("appareil inconnu : {device}"))?;
            cfg.devices.retain(|d| d.id != id);
            cfg.schedules.retain(|s| s.device != id);
            cfg.save(path)?;
            jlog(path, Level::Info, &id, &name, "Appareil supprimé");
            println!("Supprimé : {id}");
            Ok(())
        }
        Cmd::Status { device } => status(path, &device).await,
        Cmd::On { device, dp } => power(path, &device, true, dp).await,
        Cmd::Off { device, dp } => power(path, &device, false, dp).await,
        Cmd::Watch { device, interval, secs } => watch(path, &device, interval, secs).await,
        // Future volumineuse (beaucoup de sous-commandes) : mise sur le tas pour ne pas charger
        // la pile du fil principal (débordement en build debug sinon).
        Cmd::Cam { cmd } => Box::pin(cam::run(cmd, path)).await,
        Cmd::Tplink { cmd } => tplink::run(cmd, path).await,
        Cmd::Vac { cmd } => vac::run(cmd, path).await,
        Cmd::Vision { cmd } => vision::run(cmd, path).await,
        Cmd::Listen { cmd } => tokio::task::spawn_blocking(move || listen::run(cmd)).await?,
        Cmd::Set { device, dp, value } => {
            let v = serde_json::from_str::<Value>(&value).unwrap_or(Value::String(value));
            let mut m = Map::new();
            m.insert(dp, v);
            set(path, &device, m).await
        }
    }
}

async fn import_cloud(path: &Path, user_code: &str) -> Result<()> {
    use homesmart::cloud;
    let code = user_code.to_string();
    let login = tokio::task::spawn_blocking(move || cloud::start_login(&code)).await??;
    let qr = qrcode::QrCode::new(login.qr_content())?;
    let art = qr.render::<qrcode::render::unicode::Dense1x2>().quiet_zone(true).build();
    println!("{art}");
    println!("Smart Life : Accueil → « + » → Scanner, puis confirme (la demande s'affiche au nom de « Home Assistant »).");
    // On écoute le réseau en parallèle pour connaître l'IP et la version de chaque appareil.
    let lan = tokio::spawn(discovery::scan(Duration::from_secs(10), None));
    let session = tokio::task::spawn_blocking(move || -> Result<cloud::Session> {
        let start = Instant::now();
        loop {
            if let Some(s) = cloud::poll_login(&login)? {
                return Ok(s);
            }
            if start.elapsed() > Duration::from_secs(300) {
                anyhow::bail!("QR code expiré, relance la commande");
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    })
    .await??;
    println!("Connecté, récupération des appareils…");
    let devices = tokio::task::spawn_blocking(move || {
        let r = session.fetch_devices();
        session.logout();
        r
    })
    .await??;
    let seen = lan.await.ok().and_then(Result::ok).unwrap_or_default();
    let mut cfg = Config::load(path)?;
    let mut n = 0;
    for d in &devices {
        if d.sub || d.local_key.len() != 16 {
            println!("  - {} ignoré ({})", d.name, if d.sub { "sous-appareil de passerelle" } else { "pas de clé locale" });
            continue;
        }
        let hit = seen.iter().find(|s| s.id == d.id);
        cfg.upsert(d.to_config(hit.map(|h| h.ip.clone()), hit.map(|h| h.version.clone())));
        jlog(path, Level::Info, &d.id, &d.name, "Appareil importé depuis Smart Life (clé locale récupérée)");
        println!("  + {} ({}){}", d.name, d.id, hit.map(|h| format!(" — {}", h.ip)).unwrap_or_default());
        n += 1;
    }
    cfg.save(path)?;
    jlog(path, Level::Info, "", "", format!("Import Smart Life : {n} appareil(s), session fermée"));
    println!("{n} appareil(s) importé(s) dans {}. La session Smart Life est fermée.", path.display());
    Ok(())
}

async fn scan(path: &Path, secs: u64) -> Result<()> {
    let cfg = Config::load(path)?;
    println!("Écoute du réseau pendant {secs} s…");
    let found = discovery::scan(Duration::from_secs(secs), None).await?;
    if found.is_empty() {
        println!("Aucun appareil Tuya détecté.");
        println!("  - le PC et les appareils sont-ils sur le même réseau Wi-Fi ?");
        println!("  - le pare-feu bloque-t-il les ports UDP 6666, 6667 et 7000 ?");
        println!("  - un appareil jamais appairé n'est pas encore sur le Wi-Fi (voir le README).");
        return Ok(());
    }
    println!("{:<16} {:<24} {:<5} NOM", "IP", "ID", "VER");
    for d in &found {
        let name = cfg.find(&d.id).map(|c| c.display_name().to_string()).unwrap_or_else(|| "(nouveau, clé locale requise)".into());
        println!("{:<16} {:<24} {:<5} {}", d.ip, d.id, d.version, name);
    }
    Ok(())
}

fn list(path: &Path) -> Result<()> {
    let cfg = Config::load(path)?;
    if cfg.devices.is_empty() {
        println!("Aucun appareil dans {}.", path.display());
        return Ok(());
    }
    println!("{:<24} {:<12} {:<16} {:<5} ID", "NOM", "PIÈCE", "IP", "VER");
    for d in &cfg.devices {
        println!(
            "{:<24} {:<12} {:<16} {:<5} {}",
            d.display_name(),
            d.room.as_deref().unwrap_or("-"),
            d.ip.as_deref().unwrap_or("auto"),
            d.version.as_deref().unwrap_or("auto"),
            d.id
        );
    }
    Ok(())
}

fn find_device(path: &Path, query: &str) -> Result<DeviceConfig> {
    Config::load(path)?.find(query).cloned().ok_or_else(|| anyhow!("appareil inconnu : {query} (voir `homesmart-cli list`)"))
}

/// Cible directe ; si l'IP n'est pas connue, on écoute le réseau pour la trouver.
async fn target(d: &DeviceConfig) -> Result<Target> {
    let key = codec::parse_key(&d.key)?;
    let (ip, version) = match &d.ip {
        Some(ip) => (ip.clone(), d.version.clone()),
        None => {
            eprintln!("Recherche de {} sur le réseau…", d.display_name());
            let found = discovery::scan(Duration::from_secs(8), Some(&d.id)).await?;
            let hit = found.into_iter().find(|f| f.id == d.id).context("appareil introuvable sur le réseau")?;
            (hit.ip, d.version.clone().or(Some(hit.version)))
        }
    };
    let version = version.as_deref().map(str::parse).transpose()?.unwrap_or(Version::V33);
    Ok(Target { id: d.id.clone(), key, ip, port: d.port.unwrap_or(PORT_TCP), version })
}

fn print_dps(dps: &Map<String, Value>, schema: &Map<String, Value>) {
    let mut keys: Vec<&String> = dps.keys().collect();
    keys.sort_by_key(|k| k.parse::<u32>().unwrap_or(u32::MAX));
    for k in keys {
        let code = schema.get(k).and_then(|s| s.get("code")).and_then(Value::as_str).unwrap_or("");
        println!("  {k:>4}  {code:<20} {}", dps[k]);
    }
}

async fn status(path: &Path, query: &str) -> Result<()> {
    let d = find_device(path, query)?;
    let t = target(&d).await?;
    let start = Instant::now();
    let dps = device::fetch_status(&t).await?;
    println!("{} — en ligne ({} ms, v{})", d.display_name(), start.elapsed().as_millis(), t.version);
    print_dps(&dps, &d.schema);
    Ok(())
}

async fn power(path: &Path, query: &str, on: bool, dp: Option<String>) -> Result<()> {
    let d = find_device(path, query)?;
    let t = target(&d).await?;
    let start = Instant::now();
    let dp = match dp {
        Some(dp) => dp,
        None => {
            let st = device::fetch_status(&t).await?;
            homesmart::kinds::detect(d.kind.as_deref(), &st)
                .switch_dp(&st)
                .ok_or_else(|| anyhow!("pas de DP marche/arrêt détecté, précise --dp"))?
        }
    };
    let mut m = Map::new();
    m.insert(dp.clone(), Value::Bool(on));
    write_logged(path, &d, &t, &m).await?;
    println!("{} : {} (dp {dp}, {} ms)", d.display_name(), if on { "allumé" } else { "éteint" }, start.elapsed().as_millis());
    Ok(())
}

/// Inscrit une action de la ligne de commande dans le journal partagé avec l'application.
fn jlog(path: &Path, level: Level, id: &str, name: &str, msg: impl Into<String>) {
    journal::append_to_file(&path.with_file_name("journal.jsonl"), &Entry::new(level, Source::Cli, id, name, msg));
}

/// Écriture journalisée, avec les mêmes garde-fous que l'application (étalonnage d'usine,
/// valeurs acceptées par l'appareil). L'état réel est lu d'abord : il dit si c'est une prise
/// à mesure et quel vocabulaire elle emploie (off / power_off…).
async fn write_logged(path: &Path, d: &DeviceConfig, t: &Target, req: &Map<String, Value>) -> Result<Map<String, Value>> {
    let state = device::fetch_status(t).await.unwrap_or_default();
    let kind = kinds::detect(d.kind.as_deref(), &state);
    let desc: Vec<String> = req.iter().map(|(k, v)| kinds::describe_write(k, v, &kind, &state, &d.schema)).collect();
    let desc = desc.join(" · ");
    if let Err(e) = kinds::check_writable(req, &state, &d.schema) {
        jlog(path, Level::Warning, &d.id, d.display_name(), format!("Refusé : {e}"));
        anyhow::bail!(e);
    }
    match device::set_dps(t, req).await {
        Ok(confirmed) => {
            jlog(path, Level::Info, &d.id, d.display_name(), desc);
            Ok(confirmed)
        }
        Err(e) => {
            jlog(path, Level::Warning, &d.id, d.display_name(), format!("Échec : {desc} ({e:#})"));
            Err(e)
        }
    }
}

/// Mesures en direct : demande les DP 18/19/20 au rythme voulu et affiche ce que la prise renvoie.
async fn watch(path: &Path, query: &str, interval: f64, secs: Option<u64>) -> Result<()> {
    use tokio::time::{interval_at, sleep_until, Instant as TInstant};
    let d = find_device(path, query)?;
    let t = target(&d).await?;
    let (mut reader, mut writer) = device::connect(&t).await?;
    writer.query_all(&[]).await?;
    let every = Duration::from_secs_f64(interval.clamp(0.5, 60.0));
    let mut tick = interval_at(TInstant::now() + every, every);
    let mut hb = interval_at(TInstant::now() + Duration::from_secs(10), Duration::from_secs(10));
    let end = secs.map(|s| TInstant::now() + Duration::from_secs(s));
    let started = Instant::now();
    let (mut replies, mut changes) = (0u32, 0u32);
    let mut last_power: Option<f64> = None;
    let mut change_times: Vec<f64> = Vec::new();
    println!("{} : mesures toutes les {:.1} s (Ctrl+C pour arrêter)", d.display_name(), every.as_secs_f64());
    loop {
        let stop = async {
            match end {
                Some(e) => sleep_until(e).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = stop => break,
            _ = tokio::signal::ctrl_c() => break,
            _ = tick.tick() => writer.update_dps(&[18, 19, 20]).await?,
            _ = hb.tick() => writer.heartbeat().await?,
            msg = reader.next() => {
                let msg: device::Message = msg?;
                let Some(dps) = msg.dps() else { continue };
                replies += 1;
                let n = |k: &str| dps.get(k).and_then(Value::as_f64);
                let power = n("19").map(|p| p / 10.0);
                let el = started.elapsed().as_secs_f64();
                let mut parts = Vec::new();
                if let Some(p) = power {
                    parts.push(format!("{p:>7.1} W"));
                }
                if let Some(a) = n("18") {
                    parts.push(format!("{:>6.3} A", a / 1000.0));
                }
                if let Some(v) = n("20") {
                    parts.push(format!("{:>6.1} V", v / 10.0));
                }
                for (k, v) in dps.iter().filter(|(k, _)| !["18", "19", "20"].contains(&k.as_str())) {
                    parts.push(format!("dp{k}={v}"));
                }
                if let Some(p) = power {
                    if last_power != Some(p) {
                        changes += 1;
                        change_times.push(el);
                        last_power = Some(p);
                    }
                }
                println!("{el:>7.2} s  {}", parts.join("  "));
            }
        }
    }
    let total = started.elapsed().as_secs_f64();
    println!("— {replies} réponse(s) en {total:.0} s, {changes} valeur(s) de puissance différente(s)");
    if change_times.len() >= 3 {
        let gaps: Vec<f64> = change_times.windows(2).map(|w| w[1] - w[0]).collect();
        println!("  nouvelle valeur de puissance toutes les {:.1} s en moyenne", gaps.iter().sum::<f64>() / gaps.len() as f64);
    }
    Ok(())
}

async fn set(path: &Path, query: &str, dps: Map<String, Value>) -> Result<()> {
    let d = find_device(path, query)?;
    let t = target(&d).await?;
    let start = Instant::now();
    let confirmed = write_logged(path, &d, &t, &dps).await?;
    if confirmed.is_empty() {
        println!("{} : commande envoyée ({} ms, sans confirmation)", d.display_name(), start.elapsed().as_millis());
    } else {
        println!("{} : {} ({} ms)", d.display_name(), Value::Object(confirmed), start.elapsed().as_millis());
    }
    Ok(())
}
