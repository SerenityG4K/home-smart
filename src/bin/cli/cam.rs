//! `homesmart-cli cam …` : caméras IP en local (Imou / Dahua, et toute caméra RTSP + ONVIF).
//! Chaque action est inscrite au journal, comme depuis l'application.

use super::jlog;
use anyhow::{anyhow, bail, Context, Result};
use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use clap::Subcommand;
use homesmart::camera::{self, discover, onvif::Onvif, playback, stream, Brand, CameraConfig, Quality, TamperConfig, TamperSensitivity};
use homesmart::config::Config;
use homesmart::journal::Level;
use homesmart::tplink::camera::{AlarmConfig, TapoCam, TapoSecret};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Subcommand)]
pub enum CamCmd {
    /// Cherche les caméras du réseau (Imou / Dahua, ONVIF, port RTSP)
    Scan,
    /// Liste les caméras configurées
    List,
    /// Ajoute une caméra. Imou : le mot de passe est le code de sécurité (« SC ») sous la caméra ;
    /// Tapo : le compte de la caméra créé dans l'application Tapo (--user et mot de passe)
    Add {
        /// Adresse IP de la caméra
        host: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        room: Option<String>,
        /// Marque : imou, dahua, tapo, reolink, hikvision, amcrest, axis, foscam, autre
        /// (sinon devinée d'après la caméra)
        #[arg(long)]
        brand: Option<String>,
        /// Utilisateur (par défaut celui de la marque : admin ; obligatoire pour une Tapo)
        #[arg(long)]
        user: Option<String>,
        /// Sinon : variable HOMESMART_CAM_PASSWORD, ou saisie au clavier
        #[arg(long, env = "HOMESMART_CAM_PASSWORD", hide_env_values = true)]
        password: Option<String>,
        /// Caméra d'une autre marque : chemin RTSP du flux principal (ex. /live/ch0)
        #[arg(long)]
        main_path: Option<String>,
        #[arg(long)]
        sub_path: Option<String>,
    },
    /// Appaire une caméra HomeSmart (fabriquée soi-même) avec son code, puis l'ajoute
    Pair {
        /// Adresse IP de la caméra
        host: String,
        /// Code d'appairage donné à l'installation de l'agent (« ABCD-EFGH »)
        code: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        room: Option<String>,
        /// Port de l'agent de la caméra
        #[arg(long, default_value_t = homesmart_cam_proto::API_PORT)]
        port: u16,
    },
    /// Caméra HomeSmart : état et réglages (sans option : affiche), ex. : cam agent Salon --res 4k --fps 25 --mic off
    Agent {
        camera: String,
        /// Définition du flux HD : 4k, 1440p, 1080p, 720p, ou LxH
        #[arg(long)]
        res: Option<String>,
        #[arg(long)]
        fps: Option<u32>,
        /// Débit du flux HD en kbit/s (sinon celui conseillé pour la définition)
        #[arg(long)]
        kbps: Option<u32>,
        /// Codec du flux HD : h265 ou h264
        #[arg(long)]
        codec: Option<String>,
        /// Micro : on ou off
        #[arg(long)]
        mic: Option<String>,
        /// Gain du micro en dB (−20 à +30)
        #[arg(long, allow_hyphen_values = true)]
        gain: Option<f32>,
        /// Image retournée (caméra au plafond) : on ou off
        #[arg(long)]
        rotate: Option<String>,
    },
    /// Supprime une caméra
    Remove { camera: String },
    /// Teste la vidéo et le pilotage ONVIF
    Test { camera: String },
    /// Informations de la caméra (modèle, firmware, orientation, positions)
    Info { camera: String },
    /// Enregistre une image (HD par défaut)
    Snapshot {
        camera: String,
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Flux fluide (plus léger) au lieu du flux HD
        #[arg(long)]
        sub: bool,
    },
    /// Oriente la caméra : gauche, droite, haut, bas (ou left, right, up, down)
    Move {
        camera: String,
        direction: String,
        /// Durée du mouvement, en millisecondes
        #[arg(long, default_value_t = 400)]
        ms: u64,
        /// Vitesse, de 0.1 à 1
        #[arg(long, default_value_t = 0.6)]
        speed: f32,
    },
    /// Retour à la position d'origine
    Home { camera: String },
    /// Liste les positions mémorisées
    Presets { camera: String },
    /// Affiche ou change la qualité d'un flux, ex. : cam encoder Salon --res 2304x1296 --fps 25 --kbps 4096
    Encoder {
        camera: String,
        /// Flux fluide (sinon le flux HD)
        #[arg(long)]
        sub: bool,
        /// Résolution, ex. 1920x1080
        #[arg(long)]
        res: Option<String>,
        #[arg(long)]
        fps: Option<u32>,
        #[arg(long)]
        kbps: Option<u32>,
    },
    /// FFmpeg (décodage des flux HD en H.265) : état, ou installation avec --install
    Ffmpeg {
        #[arg(long)]
        install: bool,
    },
    /// Réglages d'image : luminosité, contraste, vision de nuit, WDR (sans option : affiche)
    Image {
        camera: String,
        #[arg(long)]
        brightness: Option<f32>,
        #[arg(long)]
        contrast: Option<f32>,
        /// Vision de nuit : auto, jour ou nuit
        #[arg(long)]
        night: Option<String>,
        /// WDR (plage dynamique étendue) : on ou off
        #[arg(long)]
        wdr: Option<String>,
    },
    /// Mesure le flux en direct : cadence, débit, temps de décodage, processeur
    Watch {
        camera: String,
        /// Flux HD (sinon le flux fluide)
        #[arg(long)]
        hd: bool,
        #[arg(long, default_value_t = 10)]
        secs: u64,
        /// Décode aussi le son (sans le jouer) et affiche son niveau
        #[arg(long)]
        audio: bool,
    },
    /// Va à une position mémorisée (nom ou numéro)
    Goto { camera: String, preset: String },
    /// Suivi automatique d'une personne (essai) : la caméra tourne pour la garder au centre
    Follow(super::follow::FollowArgs),
    /// Enregistrements de la carte SD (lecture seule) : les séquences d'un jour, à la seconde près
    Recordings {
        camera: String,
        /// Jour AAAA-MM-JJ (par défaut : aujourd'hui)
        #[arg(long)]
        date: Option<String>,
    },
    /// Relit une période de la carte SD (décode et compte les images, pour vérifier)
    Replay {
        camera: String,
        /// Début, ex. "2026-10-20 14:05"
        #[arg(long)]
        start: String,
        /// Fin, ex. "2026-10-20 14:12"
        #[arg(long)]
        end: String,
        /// S'arrêter après N secondes (sinon jusqu'au bout de la période)
        #[arg(long)]
        secs: Option<u64>,
    },
    /// Caméra Tapo : mot de passe du compte TP-Link (alarme, suivi, carte SD). Sans option : état
    Tapo {
        camera: String,
        /// Mot de passe du compte TP-Link (sinon : variable HOMESMART_TPLINK_PASSWORD, ou saisie)
        #[arg(long)]
        password: Option<String>,
    },
    /// Alarme (sirène et lumière). Sans option : affiche. `--start` / `--stop` : déclenche
    Alarm {
        camera: String,
        /// Active l'alarme automatique (sur détection de la caméra)
        #[arg(long)]
        on: bool,
        /// Désactive l'alarme automatique
        #[arg(long, conflicts_with = "on")]
        off: bool,
        /// Sirène : on ou off
        #[arg(long)]
        siren: Option<String>,
        /// Lumière : on ou off
        #[arg(long)]
        light: Option<String>,
        /// Volume de la sirène : low, normal, high (selon le modèle)
        #[arg(long)]
        volume: Option<String>,
        /// Durée d'une alarme, en secondes
        #[arg(long)]
        duration: Option<u32>,
        /// Déclenche l'alarme tout de suite
        #[arg(long)]
        start: bool,
        /// Arrête l'alarme en cours
        #[arg(long, conflicts_with = "start")]
        stop: bool,
    },
    /// Suivi des mouvements intégré à la caméra (Tapo) : on ou off (sans valeur : affiche)
    Track {
        camera: String,
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
    },
    /// Détection de sabotage (objectif masqué, caméra détournée, image floutée). Sans option : état
    Tamper {
        camera: String,
        /// Analyse de l'image par HomeSmart : on ou off
        #[arg(long)]
        image: Option<String>,
        /// Alertes de la caméra elle-même (ONVIF) : on ou off
        #[arg(long = "cam")]
        cam_side: Option<String>,
        /// Sensibilité : faible, moyenne, élevée
        #[arg(long)]
        sensitivity: Option<String>,
        /// Coupe toute la détection
        #[arg(long)]
        off: bool,
    },
    /// Exporte un extrait de la carte SD en .mp4 (copie directe, sans ré-encodage)
    Export {
        camera: String,
        /// Début, ex. "2026-10-20 14:05"
        #[arg(long)]
        start: String,
        /// Fin, ex. "2026-10-20 14:12"
        #[arg(long)]
        end: String,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

pub(super) fn find(path: &Path, query: &str) -> Result<CameraConfig> {
    Config::load(path)?.find_camera(query).cloned().ok_or_else(|| anyhow!("caméra inconnue : {query} (voir `homesmart-cli cam list`)"))
}

fn onvif(c: &CameraConfig) -> Result<Onvif> {
    Onvif::connect(&c.host, c.onvif_port, &c.username, &c.password)
}

async fn blocking<R: Send + 'static>(f: impl FnOnce() -> Result<R> + Send + 'static) -> Result<R> {
    tokio::task::spawn_blocking(f).await.map_err(|e| anyhow!("{e}"))?
}

pub async fn run(cmd: CamCmd, path: &Path) -> Result<()> {
    match cmd {
        CamCmd::Scan => scan(path).await,
        CamCmd::List => list(path),
        CamCmd::Add { host, name, room, brand, user, password, main_path, sub_path } => add(path, host, name, room, brand, user, password, main_path, sub_path).await,
        CamCmd::Pair { host, code, name, room, port } => pair(path, host, code, name, room, port).await,
        CamCmd::Agent { camera, res, fps, kbps, codec, mic, gain, rotate } => agent(path, &find(path, &camera)?, res, fps, kbps, codec, mic, gain, rotate).await,
        CamCmd::Remove { camera } => {
            let c = find(path, &camera)?;
            let mut cfg = Config::load(path)?;
            cfg.cameras.retain(|x| x.id != c.id);
            cfg.save(path)?;
            jlog(path, Level::Info, &c.id, c.display_name(), "Caméra supprimée");
            println!("Supprimée : {}", c.display_name());
            // Caméra HomeSmart : elle oublie ce HomeSmart.
            if c.agent.is_some() {
                let c2 = c.clone();
                match blocking(move || camera::agent::unpair(&c2)).await {
                    Ok(()) => jlog(path, Level::Info, &c.id, c.display_name(), "La caméra a oublié ce HomeSmart"),
                    Err(e) => {
                        jlog(path, Level::Warning, &c.id, c.display_name(), format!("La caméra n'a pas pu oublier ce HomeSmart : {e:#}"));
                        eprintln!("La caméra n'a pas pu être prévenue ({e:#}) : sur la caméra, `homesmart-cam reset` efface ses appairages.");
                    }
                }
            }
            Ok(())
        }
        CamCmd::Test { camera } => test(path, &find(path, &camera)?).await,
        CamCmd::Info { camera } => info(&find(path, &camera)?).await,
        CamCmd::Watch { camera, hd, secs, audio } => watch(path, &find(path, &camera)?, hd, secs, audio).await,
        CamCmd::Ffmpeg { install } => ffmpeg(path, install).await,
        CamCmd::Image { camera, brightness, contrast, night, wdr } => image(path, &find(path, &camera)?, brightness, contrast, night, wdr).await,
        CamCmd::Encoder { camera, sub, res, fps, kbps } => encoder(path, &find(path, &camera)?, sub, res, fps, kbps).await,
        CamCmd::Snapshot { camera, output, sub } => snapshot(path, &find(path, &camera)?, output, sub).await,
        CamCmd::Follow(a) => super::follow::run(a, path).await,
        CamCmd::Tapo { camera, password } => tapo(path, &find(path, &camera)?, password).await,
        CamCmd::Alarm { camera, on, off, siren, light, volume, duration, start, stop } => alarm(path, &find(path, &camera)?, on, off, siren, light, volume, duration, start, stop).await,
        CamCmd::Track { camera, state } => track(path, &find(path, &camera)?, state).await,
        CamCmd::Tamper { camera, image, cam_side, sensitivity, off } => tamper(path, &find(path, &camera)?, image, cam_side, sensitivity, off).await,
        CamCmd::Recordings { camera, date } => recordings(&find(path, &camera)?, date).await,
        CamCmd::Replay { camera, start, end, secs } => replay(path, &find(path, &camera)?, &start, &end, secs).await,
        CamCmd::Export { camera, start, end, output } => export(path, &find(path, &camera)?, &start, &end, output).await,
        CamCmd::Move { camera, direction, ms, speed } => {
            let c = find(path, &camera)?;
            let s = speed.clamp(0.1, 1.0);
            let (pan, tilt, label) = match direction.to_lowercase().as_str() {
                "gauche" | "left" | "g" => (-s, 0.0, "gauche"),
                "droite" | "right" | "d" => (s, 0.0, "droite"),
                "haut" | "up" | "h" => (0.0, s, "haut"),
                "bas" | "down" | "b" => (0.0, -s, "bas"),
                other => bail!("direction inconnue : {other} (gauche, droite, haut, bas)"),
            };
            // Axes inversés dans les réglages de la caméra (comme dans l'application).
            let (pan, tilt) = (if c.invert_pan { -pan } else { pan }, if c.invert_tilt { -tilt } else { tilt });
            let c2 = c.clone();
            let r = blocking(move || {
                let o = onvif(&c2)?;
                o.continuous_move(pan, tilt)?;
                std::thread::sleep(Duration::from_millis(ms.min(10_000)));
                o.stop()
            })
            .await;
            match &r {
                Ok(()) => {
                    jlog(path, Level::Info, &c.id, c.display_name(), format!("Orientation : {label} ({ms} ms)"));
                    println!("{} : orientée vers {label} ({ms} ms)", c.display_name());
                }
                Err(e) => jlog(path, Level::Warning, &c.id, c.display_name(), format!("Orientation ({label}) refusée : {e:#}")),
            }
            r
        }
        CamCmd::Home { camera } => {
            let c = find(path, &camera)?;
            let c2 = c.clone();
            let r = blocking(move || {
                let o = onvif(&c2)?;
                match o.goto_home() {
                    // Pas de position d'origine ONVIF (Imou) : position mémorisée « Origine ».
                    Err(e) if format!("{e:#}").contains("NotImplemented") || format!("{e:#}").contains("ActionNotSupported") => {
                        let p = o.presets().ok().and_then(|l| l.into_iter().find(|p| p.name.trim().eq_ignore_ascii_case("origine")));
                        match p {
                            Some(p) => o.goto_preset(&p.token),
                            None => Err(anyhow!("cette caméra ne gère pas de position d'origine en ONVIF")),
                        }
                    }
                    r => r,
                }
            })
            .await;
            match &r {
                Ok(()) => jlog(path, Level::Info, &c.id, c.display_name(), "Retour à la position d'origine"),
                Err(e) => jlog(path, Level::Warning, &c.id, c.display_name(), format!("Retour à la position d'origine refusé : {e:#}")),
            }
            r
        }
        CamCmd::Presets { camera } => {
            let c = find(path, &camera)?;
            let list = blocking(move || onvif(&c)?.presets()).await?;
            if list.is_empty() {
                println!("Aucune position mémorisée.");
            }
            for p in list {
                println!("  {:>3}  {}", p.token, p.name);
            }
            Ok(())
        }
        CamCmd::Goto { camera, preset } => {
            let c = find(path, &camera)?;
            let c2 = c.clone();
            let q = preset.clone();
            let r = blocking(move || {
                let o = onvif(&c2)?;
                let list = o.presets()?;
                let p = list
                    .iter()
                    .find(|p| p.token == q || p.name.eq_ignore_ascii_case(&q))
                    .ok_or_else(|| anyhow!("position inconnue : {q} (voir `cam presets`)"))?;
                o.goto_preset(&p.token)?;
                Ok(p.name.clone())
            })
            .await;
            match &r {
                Ok(name) => {
                    jlog(path, Level::Info, &c.id, c.display_name(), format!("Position « {name} » rappelée"));
                    println!("{} : position « {name} »", c.display_name());
                }
                Err(e) => jlog(path, Level::Warning, &c.id, c.display_name(), format!("Rappel de la position « {preset} » refusé : {e:#}")),
            }
            r.map(|_| ())
        }
    }
}

async fn scan(path: &Path) -> Result<()> {
    let cfg = Config::load(path)?;
    println!("Recherche des caméras (Imou / Dahua, ONVIF, RTSP)…");
    let found = discover::scan(Duration::from_millis(2500)).await;
    let named: Vec<String> = found.iter().filter(|f| f.model.is_some() || f.onvif).map(|f| format!("{} à {}", f.title(), f.ip)).collect();
    jlog(path, Level::Info, "", "", if named.is_empty() { format!("Recherche de caméras : {} résultat(s)", found.len()) } else { format!("Recherche de caméras : {}", named.join(", ")) });
    if found.is_empty() {
        println!("Aucune caméra trouvée. Est-elle allumée et sur le même réseau que le PC ?");
        return Ok(());
    }
    println!("{:<16} {:<30} {:<18} {:<5} {:<5} ÉTAT", "IP", "CAMÉRA", "MAC", "RTSP", "ONVIF");
    for f in &found {
        let known = cfg.cameras.iter().find(|c| c.host == f.ip || (f.serial.is_some() && c.serial == f.serial));
        let state = known.map(|c| format!("ajoutée (« {} »)", c.display_name())).unwrap_or_else(|| "nouvelle".into());
        let yes = |b: bool| if b { "oui" } else { "-" };
        println!("{:<16} {:<30} {:<18} {:<5} {:<5} {state}", f.ip, f.title(), f.mac.as_deref().unwrap_or("-"), yes(f.rtsp), yes(f.onvif));
        if let (Some(s), Some(fw)) = (&f.serial, &f.firmware) {
            println!("{:<16} n° de série {s} · firmware {fw}", "");
        }
        if !f.aliases.is_empty() {
            println!("{:<16} répond aussi sur {} (adresse d'usine)", "", f.aliases.join(", "));
        }
        if f.needs_tapo_account() {
            println!("{:<16} vidéo et ONVIF fermés : crée d'abord son compte caméra dans l'application Tapo", "");
            println!("{:<16} (la caméra, Paramètres, Paramètres avancés, Compte de la caméra)", "");
        }
    }
    if found.iter().any(|f| !cfg.cameras.iter().any(|c| c.host == f.ip)) {
        println!("\nPour en ajouter une : homesmart-cli cam add <IP> --name \"Salon\"  (le code de sécurité sera demandé)");
        if found.iter().any(|f| f.brand == Brand::Tapo && !cfg.cameras.iter().any(|c| c.host == f.ip)) {
            println!("Une Tapo : homesmart-cli cam add <IP> --brand tapo --user <compte de la caméra>  (créé dans l'application Tapo)");
        }
        if found.iter().any(|f| f.brand == Brand::HomeSmart && !cfg.cameras.iter().any(|c| c.host == f.ip)) {
            println!("Une caméra HomeSmart : homesmart-cli cam pair <IP> <CODE>  (code donné à l'installation de son agent)");
        }
    }
    Ok(())
}

fn list(path: &Path) -> Result<()> {
    let cfg = Config::load(path)?;
    if cfg.cameras.is_empty() {
        println!("Aucune caméra (voir `homesmart-cli cam scan`).");
        return Ok(());
    }
    println!("{:<22} {:<12} {:<16} {:<16} {:<8} ID", "NOM", "PIÈCE", "IP", "MODÈLE", "MOT DE P.");
    for c in &cfg.cameras {
        println!(
            "{:<22} {:<12} {:<16} {:<16} {:<8} {}",
            c.display_name(),
            c.room.as_deref().unwrap_or("-"),
            c.host,
            c.model.as_deref().unwrap_or("-"),
            if c.password.is_empty() { "absent" } else { "oui" },
            c.id
        );
    }
    Ok(())
}

/// Marque écrite par l'utilisateur (nom ou libellé, sans tenir compte de la casse).
fn parse_brand(s: &str) -> Result<Brand> {
    let n = s.trim().to_lowercase();
    if matches!(n.as_str(), "autre" | "generic" | "onvif" | "rtsp") {
        return Ok(Brand::Generic);
    }
    Brand::from_vendor(&n).or_else(|| Brand::ALL.into_iter().find(|b| b.label().to_lowercase() == n)).ok_or_else(|| anyhow!("marque « {s} » inconnue (imou, dahua, tapo, reolink, hikvision, amcrest, axis, foscam, autre)"))
}

#[allow(clippy::too_many_arguments)]
async fn add(path: &Path, host: String, name: Option<String>, room: Option<String>, brand: Option<String>, user: Option<String>, password: Option<String>, main_path: Option<String>, sub_path: Option<String>) -> Result<()> {
    let brand = brand.as_deref().map(parse_brand).transpose()?;
    if brand == Some(Brand::HomeSmart) {
        bail!("une caméra HomeSmart s'ajoute avec son code d'appairage : homesmart-cli cam pair {host} <CODE>");
    }
    if brand == Some(Brand::Tapo) && user.as_deref().is_none_or(|u| u.trim().is_empty()) {
        bail!("une Tapo demande le nom de son compte caméra (--user), créé dans l'application Tapo : la caméra, Paramètres, Paramètres avancés, Compte de la caméra");
    }
    let password = match password {
        Some(p) => p,
        None if std::io::stdin().is_terminal() => {
            eprint!("{}", if brand == Some(Brand::Tapo) { "Mot de passe du compte de la caméra : " } else { "Code de sécurité / mot de passe de la caméra (étiquette « SC » sous la caméra) : " });
            let mut s = String::new();
            std::io::stdin().read_line(&mut s)?;
            s.trim().to_string()
        }
        None => String::new(),
    };
    let mut c = CameraConfig::new(&host);
    c.brand = brand.unwrap_or(if main_path.is_some() { Brand::Generic } else { Brand::Imou });
    c.onvif_port = c.brand.default_onvif_port();
    c.username = user.unwrap_or_else(|| c.brand.default_user().to_string());
    (c.password, c.room, c.main_path, c.sub_path) = (password, room, main_path, sub_path);
    // Modèle, n° de série et marque, si la caméra répond déjà en ONVIF : sur le port de la marque,
    // sinon sur 2020 (marque non précisée : c'est peut-être une Tapo).
    let mut ports = vec![c.onvif_port];
    if brand.is_none() && c.onvif_port != 2020 {
        ports.push(2020);
    }
    for port in ports {
        let mut probe = c.clone();
        probe.onvif_port = port;
        if let Ok(Ok(info)) = tokio::task::spawn_blocking(move || onvif(&probe).and_then(|o| o.device_info())).await {
            c.onvif_port = port;
            if !info.model.is_empty() {
                c.model = Some(info.model);
            }
            if !info.serial.is_empty() {
                c.serial = Some(info.serial);
            }
            if brand.is_none() {
                if let Some(b) = Brand::from_vendor(&info.manufacturer).or((port == 2020).then_some(Brand::Tapo)) {
                    c.brand = b;
                }
            }
            break;
        }
    }
    c.name = name.unwrap_or_else(|| format!("Caméra {}", c.brand.label()));
    c.id = c.make_id();
    let mut cfg = Config::load(path)?;
    if let Some(e) = cfg.cameras.iter().find(|x| x.id == c.id || x.host == c.host) {
        bail!("cette caméra est déjà ajoutée (« {} »)", e.display_name());
    }
    cfg.cameras.push(c.clone());
    cfg.save(path)?;
    let model = c.model.as_deref().map(|m| format!("{m}, ")).unwrap_or_default();
    jlog(path, Level::Info, &c.id, c.display_name(), format!("Caméra ajoutée ({model}{})", c.host));
    println!("Ajoutée : « {} » ({model}{})", c.display_name(), c.host);
    if c.password.is_empty() {
        println!("Attention : pas de mot de passe. La vidéo demandera le code de sécurité de la caméra.");
    }
    println!("Vérifie la connexion : homesmart-cli cam test \"{}\"", c.display_name());
    Ok(())
}

async fn pair(path: &Path, host: String, code: String, name: Option<String>, room: Option<String>, port: u16) -> Result<()> {
    let host = host.trim().to_string();
    if let Some(e) = Config::load(path)?.cameras.iter().find(|x| x.host == host) {
        bail!("cette caméra est déjà ajoutée (« {} »)", e.display_name());
    }
    let h = host.clone();
    let (link, info) = match blocking(move || camera::agent::pair(&h, port, &code)).await {
        Ok(x) => x,
        Err(e) => {
            jlog(path, Level::Warning, "", "", format!("Appairage de la caméra HomeSmart {host} refusé : {e:#}"));
            return Err(e);
        }
    };
    let mut c = CameraConfig::new(&host);
    (c.name, c.room) = (name.unwrap_or_default(), room);
    camera::agent::apply_pairing(&mut c, link, &info);
    c.id = c.make_id();
    let mut cfg = Config::load(path)?;
    if let Some(e) = cfg.cameras.iter().find(|x| x.id == c.id || x.serial == c.serial) {
        let name = e.display_name().to_string();
        let c2 = c.clone();
        let _ = blocking(move || camera::agent::unpair(&c2)).await;
        bail!("cette caméra est déjà ajoutée (« {name} »)");
    }
    cfg.cameras.push(c.clone());
    cfg.save(path)?;
    jlog(path, Level::Info, &c.id, c.display_name(), format!("Caméra HomeSmart appairée et ajoutée ({}, {})", info.board, c.host));
    println!("Appairée : « {} » ({}, {})", c.display_name(), info.board, c.host);
    let caps = &info.caps;
    println!("  {}×{} jusqu'à {} i/s, {}{}", caps.max_width, caps.max_height, caps.max_fps, caps.codecs.iter().map(|c| c.label()).collect::<Vec<_>>().join(" / "), if caps.audio { ", micro" } else { ", sans micro" });
    println!("Réglages : homesmart-cli cam agent \"{}\"", c.display_name());
    Ok(())
}

fn oui_non(b: bool) -> &'static str {
    if b {
        "oui"
    } else {
        "non"
    }
}

/// « on » / « off » (et leurs équivalents) → booléen.
fn on_off(s: &str) -> Result<bool> {
    match s.trim().to_lowercase().as_str() {
        "on" | "oui" | "1" | "true" | "actif" => Ok(true),
        "off" | "non" | "0" | "false" | "coupé" | "coupe" => Ok(false),
        other => bail!("« {other} » : on ou off attendu"),
    }
}

#[allow(clippy::too_many_arguments)]
async fn agent(path: &Path, c: &CameraConfig, res: Option<String>, fps: Option<u32>, kbps: Option<u32>, codec: Option<String>, mic: Option<String>, gain: Option<f32>, rotate: Option<String>) -> Result<()> {
    let c2 = c.clone();
    let st = blocking(move || camera::agent::state(&c2)).await?;
    let change = res.is_some() || fps.is_some() || kbps.is_some() || codec.is_some() || mic.is_some() || gain.is_some() || rotate.is_some();
    if !change {
        let (i, s, p) = (&st.info, &st.status, &st.settings);
        println!("{} — {}{} · agent {}", c.display_name(), i.board, i.caps.sensor.as_deref().map(|s| format!(" · capteur {s}")).unwrap_or_default(), i.version);
        let proc = |name: &str, p: &homesmart_cam_proto::ProcStatus| match (p.running, &p.last_error) {
            (true, _) => format!("{name} : en marche{}", p.since_s.map(|s| format!(" depuis {s} s")).unwrap_or_default()),
            (false, Some(e)) => format!("{name} : arrêté — {e}"),
            (false, None) => format!("{name} : arrêté"),
        };
        println!("  {}", proc("Encodage", &s.pipeline));
        println!("  {}", proc("Serveur vidéo", &s.server));
        println!("  Allumée depuis {} s{}{}", s.uptime_s, s.cpu_temp_c.map(|t| format!(" · processeur {t:.0} °C")).unwrap_or_default(), s.load.map(|l| format!(" · charge {l:.1}")).unwrap_or_default());
        for (label, x) in [("HD", &p.main), ("fluide", &p.sub)] {
            println!("  Flux {label:<7} {}×{} · {} i/s · {} kbit/s · {}", x.width, x.height, x.fps, x.kbps, x.codec.label());
        }
        println!("  Micro : {}{}", if p.audio.enabled { "activé" } else { "coupé" }, if p.audio.enabled { format!(" (gain {:+.0} dB, PCM 16 kHz)", p.audio.gain_db) } else { String::new() });
        println!("  Image retournée : {}", if p.rotate_180 { "oui" } else { "non" });
        return Ok(());
    }
    let before = st.settings.clone();
    let mut s = before.clone();
    let mut retune = false;
    if let Some(r) = res {
        let r = r.trim().to_lowercase();
        let (w, h) = match homesmart_cam_proto::RESOLUTIONS.iter().find(|(_, _, n)| n.to_lowercase() == r) {
            Some((w, h, _)) => (*w, *h),
            None => r.split_once('x').and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?))).ok_or_else(|| anyhow!("définition « {r} » inconnue (4k, 1440p, 1080p, 720p ou LxH)"))?,
        };
        (s.main.width, s.main.height, retune) = (w, h, true);
    }
    if let Some(f) = fps {
        (s.main.fps, retune) = (f, true);
        s.sub.fps = s.sub.fps.min(f);
    }
    if let Some(k) = codec {
        s.main.codec = match k.trim().to_lowercase().replace(['.', '-'], "").as_str() {
            "h265" | "hevc" => homesmart_cam_proto::VideoCodec::H265,
            "h264" | "avc" => homesmart_cam_proto::VideoCodec::H264,
            other => bail!("codec « {other} » inconnu (h265 ou h264)"),
        };
        retune = true;
    }
    s.main.kbps = match kbps {
        Some(k) => k,
        None if retune => homesmart_cam_proto::Caps::suggested_kbps(s.main.width, s.main.height, s.main.fps, s.main.codec),
        None => s.main.kbps,
    };
    if let Some(m) = mic {
        s.audio.enabled = on_off(&m)?;
    }
    if let Some(g) = gain {
        s.audio.gain_db = g;
    }
    if let Some(r) = rotate {
        s.rotate_180 = on_off(&r)?;
    }
    s.validate(&st.info.caps).map_err(|e| anyhow!(e))?;
    let c2 = c.clone();
    let r = blocking(move || camera::agent::set_settings(&c2, &s)).await;
    match &r {
        Ok(after) => {
            let changes = camera::agent::describe_changes(&before, after);
            let msg = if changes.is_empty() { "sans changement".to_string() } else { changes.join(", ") };
            jlog(path, Level::Info, &c.id, c.display_name(), format!("Réglages de la caméra : {msg}"));
            println!("{} : {msg} (la caméra relance son encodage)", c.display_name());
        }
        Err(e) => jlog(path, Level::Warning, &c.id, c.display_name(), format!("Réglages de la caméra refusés : {e:#}")),
    }
    r.map(|_| ())
}

async fn test(path: &Path, c: &CameraConfig) -> Result<()> {
    println!("{} ({}) :", c.display_name(), c.host);
    let start = Instant::now();
    let video = stream::grab(&c.rtsp_url(Quality::Sub), &c.username, &c.password, Duration::from_secs(12)).await;
    let video_line = match &video {
        Ok((_, s)) => {
            println!("  vidéo   OK en {} ms — {}", start.elapsed().as_millis(), s.summary());
            format!("vidéo OK ({})", s.summary())
        }
        Err(e) => {
            let msg = stream::describe_error(e);
            println!("  vidéo   ÉCHEC — {msg}");
            format!("vidéo impossible : {msg}")
        }
    };
    if let Ok(list) = stream::tracks(&c.rtsp_url(Quality::Sub), &c.username, &c.password).await {
        for t in list {
            let ch = t.channels.map(|n| format!(", {n} canal(aux)")).unwrap_or_default();
            println!("  piste   {} : {} ({} Hz{ch})", t.media, t.encoding, t.clock_rate);
        }
    }
    // Caméra HomeSmart : pas d'ONVIF, c'est son agent qui répond.
    if !c.brand.has_onvif() {
        let c2 = c.clone();
        let agent_line = match blocking(move || camera::agent::status(&c2)).await {
            Ok(s) => {
                let enc = if s.pipeline.running { "encodage en marche".to_string() } else { format!("encodage arrêté ({})", s.pipeline.last_error.as_deref().unwrap_or("?")) };
                println!("  agent   OK — {enc}");
                format!("agent OK, {enc}")
            }
            Err(e) => {
                println!("  agent   ÉCHEC — {e:#}");
                format!("agent : {e:#}")
            }
        };
        jlog(path, if video.is_ok() { Level::Info } else { Level::Warning }, &c.id, c.display_name(), format!("Test de connexion : {video_line} · {agent_line}"));
        return video.map(|_| ());
    }
    let c2 = c.clone();
    let o = blocking(move || {
        let o = onvif(&c2)?;
        Ok((o.device_info().ok(), o.ptz_supported(), o.events))
    })
    .await;
    let onvif_line = match &o {
        Ok((info, ptz, events)) => {
            if let Some(i) = info {
                println!("  ONVIF   OK — {} {} · firmware {}", i.manufacturer, i.model, i.firmware);
            } else {
                println!("  ONVIF   OK");
            }
            println!("  orientation : {} · événements : {}", if *ptz { "oui" } else { "non" }, if *events { "oui" } else { "non" });
            if *ptz { "ONVIF OK, orientable".to_string() } else { "ONVIF OK".to_string() }
        }
        Err(e) => {
            println!("  ONVIF   ÉCHEC — {e:#}");
            format!("ONVIF : {e:#}")
        }
    };
    jlog(path, if video.is_ok() { Level::Info } else { Level::Warning }, &c.id, c.display_name(), format!("Test de connexion : {video_line} · {onvif_line}"));
    video.map(|_| ())
}

async fn info(c: &CameraConfig) -> Result<()> {
    let c2 = c.clone();
    let (info, ptz, profiles, presets, encoders) = blocking(move || {
        let o = onvif(&c2)?;
        let presets = if o.ptz_supported() { o.presets().unwrap_or_default() } else { Vec::new() };
        let encoders: Vec<_> = o
            .encoder_configs()
            .unwrap_or_default()
            .into_iter()
            .map(|e| {
                let opts = o.encoder_options(&e.token);
                (e, opts)
            })
            .collect();
        Ok((o.device_info()?, o.ptz_supported(), o.profiles.clone(), presets, encoders))
    })
    .await?;
    println!("{} ({})", c.display_name(), c.host);
    println!("  fabricant   {}", info.manufacturer);
    println!("  modèle      {}", info.model);
    println!("  firmware    {}", info.firmware);
    println!("  n° de série {}", info.serial);
    println!("  orientable  {}", if ptz { "oui" } else { "non" });
    for p in profiles {
        println!("  profil      {} — {} {}×{}", p.token, p.encoding, p.width, p.height);
    }
    for p in presets {
        println!("  position    {:>3}  {}", p.token, p.name);
    }
    for (e, opts) in encoders {
        println!("  encodage    {} « {} » — {} {}×{} · {} i/s · {} kbit/s · GOP {} · qualité {} · {}", e.token, e.name, e.encoding, e.width, e.height, e.fps, e.bitrate_kbps, e.gov, e.quality, e.profile);
        match opts {
            Ok(o) => {
                let res: Vec<String> = o.resolutions.iter().map(|(w, h)| format!("{w}×{h}")).collect();
                println!("              possible : {} · {}–{} i/s · GOP {}–{}{}", res.join(", "), o.fps.0, o.fps.1, o.gov.0, o.gov.1, o.bitrate_kbps.map(|b| format!(" · {}–{} kbit/s", b.0, b.1)).unwrap_or_default());
            }
            Err(err) => println!("              options illisibles : {err:#}"),
        }
    }
    Ok(())
}

/// Temps processeur consommé par ce processus (secondes), pour mesurer le coût du décodage.
fn cpu_seconds() -> f64 {
    #[cfg(windows)]
    {
        #[repr(C)]
        #[derive(Default)]
        struct FileTime(u32, u32);
        extern "system" {
            fn GetCurrentProcess() -> isize;
            fn GetProcessTimes(h: isize, c: *mut FileTime, e: *mut FileTime, k: *mut FileTime, u: *mut FileTime) -> i32;
        }
        let (mut c, mut e, mut k, mut u) = (FileTime::default(), FileTime::default(), FileTime::default(), FileTime::default());
        // SAFETY : appel Win32 documenté, pointeurs vers des structures locales valides.
        let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
        if ok == 0 {
            return 0.0;
        }
        let t = |f: &FileTime| (((f.1 as u64) << 32) | f.0 as u64) as f64 / 1e7;
        t(&k) + t(&u)
    }
    #[cfg(not(windows))]
    {
        std::fs::read_to_string("/proc/self/stat")
            .ok()
            .and_then(|s| {
                let rest = s.rsplit(')').next()?.to_string();
                let f: Vec<&str> = rest.split_whitespace().collect();
                Some((f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?) / 100.0)
            })
            .unwrap_or(0.0)
    }
}

async fn encoder(path: &Path, c: &CameraConfig, sub: bool, res: Option<String>, fps: Option<u32>, kbps: Option<u32>) -> Result<()> {
    let c2 = c.clone();
    let (o, list) = blocking(move || {
        let o = onvif(&c2)?;
        let mut list = o.encoder_configs()?;
        list.sort_by_key(|e| std::cmp::Reverse(e.width * e.height));
        Ok((o, list))
    })
    .await?;
    let i = if sub { 1 } else { 0 };
    let cur = list.get(i).cloned().ok_or_else(|| anyhow!("flux introuvable"))?;
    let label = if sub { "fluide" } else { "HD" };
    if res.is_none() && fps.is_none() && kbps.is_none() {
        println!("Flux {label} : {}×{} · {} i/s · {} kbit/s ({})", cur.width, cur.height, cur.fps, cur.bitrate_kbps, cur.encoding);
        return Ok(());
    }
    let mut new = cur.clone();
    if let Some(r) = res {
        let (w, h) = r.split_once(['x', 'X', '×']).ok_or_else(|| anyhow!("résolution attendue sous la forme 1920x1080"))?;
        (new.width, new.height) = (w.trim().parse()?, h.trim().parse()?);
    }
    if let Some(f) = fps {
        new.fps = f;
    }
    if let Some(k) = kbps {
        new.bitrate_kbps = k;
    }
    let n = new.clone();
    let r = blocking(move || o.set_encoder(&n)).await;
    let msg = format!("{}×{} · {} i/s · {} kbit/s → {}×{} · {} i/s · {} kbit/s", cur.width, cur.height, cur.fps, cur.bitrate_kbps, new.width, new.height, new.fps, new.bitrate_kbps);
    match &r {
        Ok(()) => {
            jlog(path, Level::Info, &c.id, c.display_name(), format!("Qualité vidéo (flux {label}) : {msg}"));
            println!("Flux {label} : {msg}");
        }
        Err(e) => jlog(path, Level::Warning, &c.id, c.display_name(), format!("Qualité vidéo (flux {label}) refusée : {e:#}")),
    }
    r
}

async fn image(path: &Path, c: &CameraConfig, brightness: Option<f32>, contrast: Option<f32>, night: Option<String>, wdr: Option<String>) -> Result<()> {
    use homesmart::camera::onvif::ImagingSettings;
    let ir = night.map(|n| match n.to_lowercase().as_str() {
        "jour" | "day" | "on" => "ON".to_string(),
        "nuit" | "night" | "off" => "OFF".to_string(),
        _ => "AUTO".to_string(),
    });
    let wdr = wdr.map(|w| if w.eq_ignore_ascii_case("on") { "ON".to_string() } else { "OFF".to_string() });
    let change = ImagingSettings { brightness, contrast, ir_cut: ir, wdr, ..ImagingSettings::default() };
    let c2 = c.clone();
    let apply = change != ImagingSettings::default();
    let ch = change.clone();
    let (now, opts) = blocking(move || {
        let o = onvif(&c2)?;
        if apply {
            o.set_imaging(&ch)?;
        }
        Ok((o.imaging_settings()?, o.imaging_options().unwrap_or_default()))
    })
    .await?;
    if apply {
        jlog(path, Level::Info, &c.id, c.display_name(), format!("Réglages d'image modifiés : {change:?}"));
    }
    let f = |v: Option<f32>| v.map(|v| format!("{v}")).unwrap_or_else(|| "-".into());
    let r = |v: Option<(f32, f32)>| v.map(|(a, b)| format!(" ({a}–{b})")).unwrap_or_default();
    println!("{} — réglages d'image", c.display_name());
    println!("  luminosité   {}{}", f(now.brightness), r(opts.brightness));
    println!("  contraste    {}{}", f(now.contrast), r(opts.contrast));
    println!("  saturation   {}{}", f(now.saturation), r(opts.saturation));
    println!("  netteté      {}{}", f(now.sharpness), r(opts.sharpness));
    println!("  filtre IR    {} (possible : {})", now.ir_cut.as_deref().unwrap_or("-"), opts.ir_cut.join(", "));
    println!("  WDR          {} niveau {} (possible : {})", now.wdr.as_deref().unwrap_or("-"), f(now.wdr_level), opts.wdr.join(", "));
    println!("  contre-jour  {} (possible : {})", now.backlight.as_deref().unwrap_or("-"), opts.backlight.join(", "));
    Ok(())
}

async fn ffmpeg(path: &Path, install: bool) -> Result<()> {
    use homesmart::camera::ffmpeg as ff;
    if let Some(p) = ff::find() {
        let v = ff::version(&p).unwrap_or_default();
        println!("FFmpeg {v} : {}", p.display());
        return Ok(());
    }
    if !install {
        println!("FFmpeg est absent : les flux HD en H.265 ne peuvent pas être affichés.");
        if ff::can_install() {
            println!("Installe-le avec : homesmart-cli cam ffmpeg --install   (paquet {} via winget)", ff::WINGET_ID);
        } else {
            println!("Installe-le avec : {}", ff::manual_command());
        }
        return Ok(());
    }
    jlog(path, Level::Info, "", "", format!("Installation de FFmpeg lancée (winget, paquet {})", ff::WINGET_ID));
    let r = blocking(|| {
        let last = std::sync::Mutex::new(String::new());
        ff::install(|step| {
            let mut l = last.lock().unwrap();
            if *l != step {
                eprintln!("  {step}");
                *l = step;
            }
        })
    })
    .await;
    match r {
        Ok(p) => {
            let v = ff::version(&p).map(|v| format!(" {v}")).unwrap_or_default();
            jlog(path, Level::Info, "", "", format!("FFmpeg{v} installé : les flux HD en H.265 sont maintenant lisibles"));
            println!("FFmpeg{v} installé : {}", p.display());
            Ok(())
        }
        Err(e) => {
            jlog(path, Level::Warning, "", "", format!("Installation de FFmpeg échouée : {e:#}"));
            Err(e)
        }
    }
}

async fn watch(path: &Path, c: &CameraConfig, hd: bool, secs: u64, with_audio: bool) -> Result<()> {
    let q = if hd { Quality::Main } else { Quality::Sub };
    println!("{} — flux {} ({}), {secs} s :", c.display_name(), q.label(), c.rtsp_url(q));
    // Ouvrir la vidéo est une action : elle est journalisée comme depuis l'application.
    jlog(path, Level::Info, &c.id, c.display_name(), format!("Vidéo en direct ouverte (flux {}, mesure de {secs} s)", q.label()));
    let sink: stream::FrameSink = std::sync::Arc::new(|_| {});
    let (cpu0, t0) = (cpu_seconds(), Instant::now());
    // Son : échantillons reçus et crête sur la dernière seconde (rien n'est joué).
    let level = std::sync::Arc::new(std::sync::Mutex::new((0usize, 0f32, 0u32)));
    let audio: Option<stream::AudioSink> = with_audio.then(|| {
        let l = level.clone();
        std::sync::Arc::new(move |s: &[f32], rate: u32| {
            let mut g = l.lock().unwrap();
            g.0 += s.len();
            g.1 = s.iter().fold(g.1, |m, x| m.max(x.abs()));
            g.2 = rate;
        }) as stream::AudioSink
    });
    let lv = level.clone();
    let printer = move |s: homesmart::camera::StreamStats| {
        if s.fps > 0.0 {
            let cpu = (cpu_seconds() - cpu0) / t0.elapsed().as_secs_f64() * 100.0;
            let decode = if s.decode_ms > 0.0 { format!("décodage {:.1} ms/image", s.decode_ms) } else { "décodé par FFmpeg".to_string() };
            println!("  {} · {decode} · {} sautée(s) · processeur {:.0} % d'un cœur (hors FFmpeg)", s.summary(), s.dropped, cpu);
            if with_audio {
                let mut g = lv.lock().unwrap();
                println!("      son {} : {} échantillons à {} Hz · crête {:.0} %", if s.audio.is_empty() { "?" } else { &s.audio }, g.0, g.2, g.1 * 100.0);
                *g = (0, 0.0, g.2);
            }
        }
    };
    let url = c.rtsp_url(q);
    let run = stream::run_with_audio(&url, &c.username, &c.password, sink, printer, audio);
    let r = match tokio::time::timeout(Duration::from_secs(secs), run).await {
        Ok(r) => r.map_err(|e| anyhow!(stream::describe_error(&e))),
        Err(_) => Ok(()),
    };
    let level = if r.is_ok() { Level::Info } else { Level::Warning };
    jlog(path, level, &c.id, c.display_name(), format!("Vidéo en direct fermée (flux {}, {})", q.label(), homesmart::kinds::fmt_secs(t0.elapsed().as_secs())));
    r
}

async fn snapshot(path: &Path, c: &CameraConfig, output: Option<PathBuf>, sub: bool) -> Result<()> {
    let q = if sub { Quality::Sub } else { Quality::Main };
    let r = match stream::grab(&c.rtsp_url(q), &c.username, &c.password, Duration::from_secs(15)).await {
        Err(e) if q == Quality::Main && e.downcast_ref::<stream::NeedsFfmpeg>().is_some() => {
            eprintln!("Flux HD en H.265 (FFmpeg absent) : capture depuis le flux fluide.");
            stream::grab(&c.rtsp_url(Quality::Sub), &c.username, &c.password, Duration::from_secs(15)).await
        }
        r => r,
    };
    let out = output.unwrap_or_else(|| camera::captures_dir().join(camera::capture_name(c.display_name())));
    let saved = r.and_then(|(f, _)| camera::save_png(&f, &out).map(|_| (f.width, f.height)));
    match saved {
        Ok((w, h)) => {
            let file = out.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
            jlog(path, Level::Info, &c.id, c.display_name(), format!("Capture enregistrée ({w}×{h}) : {file}"));
            println!("{}", out.display());
            Ok(())
        }
        Err(e) => {
            let msg = stream::describe_error(&e);
            jlog(path, Level::Warning, &c.id, c.display_name(), format!("Capture impossible : {msg}"));
            Err(anyhow!(msg)).context(format!("capture de {}", c.display_name()))
        }
    }
}

/// Analyse une date-heure locale « AAAA-MM-JJ HH:MM[:SS] ».
pub(super) fn parse_dt(s: &str) -> Result<chrono::DateTime<Local>> {
    let s = s.trim();
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, fmt) {
            return Local.from_local_datetime(&n).earliest().ok_or_else(|| anyhow!("heure locale invalide : {s}"));
        }
    }
    bail!("date/heure attendue sous la forme \"AAAA-MM-JJ HH:MM\" (reçu : {s})")
}

/// Empreinte du mot de passe du compte TP-Link d'une caméra Tapo, enregistrée dans la configuration.
fn tapo_cam(c: &CameraConfig) -> Result<TapoCam> {
    if c.brand != Brand::Tapo {
        bail!("réglage réservé aux caméras Tapo");
    }
    let secret = c
        .tapo
        .clone()
        .filter(TapoSecret::is_set)
        .ok_or_else(|| anyhow!("mot de passe du compte TP-Link non enregistré : homesmart-cli cam tapo \"{}\" --password …", c.display_name()))?;
    Ok(TapoCam::new(&c.host, &secret))
}

/// Enregistre la configuration modifiée d'une caméra (empreinte Tapo, réglages de sabotage).
fn save_camera(path: &Path, c: &CameraConfig) -> Result<()> {
    let mut cfg = Config::load(path)?;
    let slot = cfg.cameras.iter_mut().find(|x| x.id == c.id).ok_or_else(|| anyhow!("caméra inconnue"))?;
    *slot = c.clone();
    cfg.save(path)
}

async fn tapo(path: &Path, c: &CameraConfig, password: Option<String>) -> Result<()> {
    if c.brand != Brand::Tapo {
        bail!("réglage réservé aux caméras Tapo");
    }
    if let Some(pw) = password.or_else(|| std::env::var("HOMESMART_TPLINK_PASSWORD").ok().filter(|s| !s.is_empty())).or_else(|| {
        std::io::stdin().is_terminal().then(|| {
            eprint!("Mot de passe du compte TP-Link (celui de l'application Tapo, pas le compte de la caméra) : ");
            let mut s = String::new();
            std::io::stdin().read_line(&mut s).ok();
            s.trim().to_string()
        })
    }) {
        if pw.is_empty() {
            bail!("mot de passe vide");
        }
        let secret = TapoSecret::from_password(&pw);
        let (host, s2) = (c.host.clone(), secret.clone());
        blocking(move || TapoCam::new(&host, &s2).login()).await.context("vérification du mot de passe")?;
        let mut c2 = c.clone();
        c2.tapo = Some(secret);
        save_camera(path, &c2)?;
        jlog(path, Level::Info, &c.id, c.display_name(), "Mot de passe du compte TP-Link enregistré (alarme, suivi et carte SD Tapo)");
        println!("Mot de passe accepté par la caméra et enregistré.");
        return Ok(());
    }
    // Sans mot de passe : état de l'API locale.
    let c2 = c.clone();
    let (info, quality, sd) = blocking(move || {
        let mut t = tapo_cam(&c2)?;
        t.login()?;
        Ok((t.device_info()?, t.video_quality().ok(), t.sd_card().unwrap_or_default()))
    })
    .await?;
    println!("{} ({}) — API locale Tapo :", c.display_name(), c.host);
    println!("  modèle {} · firmware {}", info.model, info.firmware);
    if let Some(q) = quality {
        let res = q.resolution.as_deref().unwrap_or("?");
        let br = q.bitrate_kbps.map(|b| format!(" · {b} kbit/s")).unwrap_or_default();
        println!("  flux principal : {res}{br} · {}", q.encode_type.as_deref().unwrap_or(""));
    }
    for card in &sd {
        println!("  carte SD : {} · {} libres sur {}", card.status, card.video_free, card.video_total);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn alarm(path: &Path, c: &CameraConfig, on: bool, off: bool, siren: Option<String>, light: Option<String>, volume: Option<String>, duration: Option<u32>, start: bool, stop: bool) -> Result<()> {
    if start || stop {
        let c2 = c.clone();
        blocking(move || {
            let mut t = tapo_cam(&c2)?;
            t.login()?;
            t.manual_alarm(start)
        })
        .await?;
        jlog(path, if start { Level::Warning } else { Level::Info }, &c.id, c.display_name(), if start { "Alarme déclenchée à la main" } else { "Alarme arrêtée" });
        println!("{}", if start { "Alarme déclenchée." } else { "Alarme arrêtée." });
        return Ok(());
    }
    let change = on || off || siren.is_some() || light.is_some() || volume.is_some() || duration.is_some();
    let c2 = c.clone();
    let mut a = blocking(move || {
        let mut t = tapo_cam(&c2)?;
        t.login()?;
        t.alarm()
    })
    .await?;
    if !change {
        println!("{} — alarme :", c.display_name());
        println!("  automatique : {}", if a.enabled { "activée" } else { "désactivée" });
        println!("  sirène : {} · lumière : {}", oui_non(a.sound), oui_non(a.light));
        if let Some(v) = &a.volume {
            println!("  volume : {v}");
        }
        if let Some(d) = a.duration {
            println!("  durée : {d} s");
        }
        println!("  déclencher : homesmart-cli cam alarm \"{}\" --start", c.display_name());
        return Ok(());
    }
    if on {
        a.enabled = true;
    }
    if off {
        a.enabled = false;
    }
    if let Some(s) = siren {
        a.sound = on_off(&s)?;
    }
    if let Some(l) = light {
        a.light = on_off(&l)?;
    }
    if volume.is_some() {
        a.volume = volume;
    }
    if let Some(d) = duration {
        a.duration = Some(d);
    }
    let (c2, a2) = (c.clone(), a.clone());
    blocking(move || {
        let mut t = tapo_cam(&c2)?;
        t.login()?;
        t.set_alarm(&a2)
    })
    .await?;
    jlog(path, Level::Info, &c.id, c.display_name(), format!("Alarme : {} (sirène {}, lumière {})", if a.enabled { "activée" } else { "désactivée" }, oui_non(a.sound), oui_non(a.light)));
    println!("Alarme enregistrée.");
    Ok(())
}

async fn track(path: &Path, c: &CameraConfig, state: Option<String>) -> Result<()> {
    match state {
        None => {
            let c2 = c.clone();
            let on = blocking(move || {
                let mut t = tapo_cam(&c2)?;
                t.login()?;
                t.auto_track()
            })
            .await?;
            println!("{} — suivi des mouvements de la caméra : {}", c.display_name(), if on { "activé" } else { "désactivé" });
        }
        Some(s) => {
            let on = on_off(&s)?;
            let c2 = c.clone();
            blocking(move || {
                let mut t = tapo_cam(&c2)?;
                t.login()?;
                t.set_auto_track(on)
            })
            .await?;
            jlog(path, Level::Info, &c.id, c.display_name(), if on { "Suivi des mouvements de la caméra activé" } else { "Suivi des mouvements de la caméra désactivé" });
            println!("Suivi des mouvements {}.", if on { "activé" } else { "désactivé" });
        }
    }
    Ok(())
}

async fn tamper(path: &Path, c: &CameraConfig, image: Option<String>, cam_side: Option<String>, sensitivity: Option<String>, off: bool) -> Result<()> {
    let mut t: TamperConfig = c.tamper;
    let change = off || image.is_some() || cam_side.is_some() || sensitivity.is_some();
    if !change {
        println!("{} — détection de sabotage :", c.display_name());
        println!("  analyse de l'image (HomeSmart) : {}", oui_non(t.image));
        println!("  alertes de la caméra (ONVIF)    : {}", oui_non(t.camera));
        println!("  sensibilité : {}", t.sensitivity.label());
        return Ok(());
    }
    if off {
        (t.image, t.camera) = (false, false);
    }
    if let Some(v) = image {
        t.image = on_off(&v)?;
    }
    if let Some(v) = cam_side {
        t.camera = on_off(&v)?;
    }
    if let Some(s) = sensitivity {
        t.sensitivity = TamperSensitivity::parse(&s).ok_or_else(|| anyhow!("sensibilité « {s} » inconnue (faible, moyenne, élevée)"))?;
    }
    let mut c2 = c.clone();
    c2.tamper = t;
    save_camera(path, &c2)?;
    // Détecteur intégré de la caméra (ONVIF), si elle en a un et sait le régler.
    if c.brand.has_onvif() {
        let (c3, want) = (c.clone(), t.camera);
        let sens = match t.sensitivity {
            TamperSensitivity::Low => "low",
            TamperSensitivity::Medium => "medium",
            TamperSensitivity::High => "high",
        };
        let r = blocking(move || {
            let o = onvif(&c3)?;
            match o.tamper_module()? {
                Some(m) => o.set_tamper_module(&m, want, sens).map(|_| true),
                None => Ok(false),
            }
        })
        .await;
        match r {
            Ok(true) => println!("  (détecteur intégré de la caméra {})", if want { "activé" } else { "désactivé" }),
            Ok(false) => {}
            Err(e) => println!("  (détecteur intégré non réglable sur cette caméra : {e:#})"),
        }
    }
    jlog(path, Level::Info, &c.id, c.display_name(), format!("Détection de sabotage : image {}, caméra {}, sensibilité {}", oui_non(t.image), oui_non(t.camera), t.sensitivity.label()));
    println!("Détection de sabotage enregistrée. (Elle agit quand l'application HomeSmart tourne.)");
    Ok(())
}

async fn recordings(c: &CameraConfig, date: Option<String>) -> Result<()> {
    let date = match date {
        Some(d) => NaiveDate::parse_from_str(d.trim(), "%Y-%m-%d").with_context(|| format!("date attendue AAAA-MM-JJ (reçu : {d})"))?,
        None => Local::now().date_naive(),
    };
    // Tapo : la caméra donne l'index de sa carte SD (jours et séquences). La lecture et l'export
    // de ces séquences passent par son flux propriétaire, que HomeSmart ne sait pas ouvrir.
    if c.brand == Brand::Tapo {
        let (c2, d) = (c.clone(), date);
        let clips = blocking(move || {
            let mut t = tapo_cam(&c2)?;
            t.login()?;
            t.recordings(d)
        })
        .await?;
        println!("{} — enregistrements du {} (carte SD, index Tapo) :", c.display_name(), date);
        if clips.is_empty() {
            println!("  → aucune vidéo ce jour-là");
        } else {
            let total: i64 = clips.iter().map(|(a, b)| (*b - *a).max(0)).sum();
            println!("  → {} séquence(s), {} de vidéo :", clips.len(), homesmart::kinds::fmt_secs(total as u64));
            for (a, b) in &clips {
                let (s, e) = (chrono::DateTime::from_timestamp(*a, 0).unwrap_or_default().with_timezone(&Local), chrono::DateTime::from_timestamp(*b, 0).unwrap_or_default().with_timezone(&Local));
                println!("     {} → {}  ({} s)", s.format("%H:%M:%S"), e.format("%H:%M:%S"), (b - a).max(0));
            }
            println!("  (lecture et export indisponibles sur Tapo : le flux de relecture est propriétaire et chiffré)");
        }
        return Ok(());
    }
    println!("{} — enregistrements du {} (carte SD, lecture seule) :", c.display_name(), date);
    let t0 = Instant::now();
    let day = playback::scan_day_detail(c, date, None, &|_| {}).await;
    let bar: String = day.hours.iter().map(|f| if f.future { ' ' } else if !f.answered { '?' } else if f.has_video() { '#' } else { '.' }).collect();
    println!("  heures 0→23 : [{bar}]   (# vidéo · . vide · espace = à venir · ? sans réponse)");
    if day.clips.is_empty() {
        println!("  → aucune vidéo ce jour-là");
    } else {
        let total = day.total_secs();
        println!("  → {} séquence(s), {} de vidéo (analyse en {:.1} s) :", day.clips.len(), homesmart::kinds::fmt_secs(total as u64), t0.elapsed().as_secs_f32());
        for clip in &day.clips {
            let (s, e) = (clip.start.with_timezone(&Local), clip.end.with_timezone(&Local));
            println!("     {} → {}  ({} s)", s.format("%H:%M:%S"), e.format("%H:%M:%S"), clip.secs());
        }
        let last = day.clips[day.clips.len() - 1];
        let (s, e) = (last.start.with_timezone(&Local), last.end.with_timezone(&Local));
        println!("  Relire la dernière : homesmart-cli cam replay \"{}\" --start \"{}\" --end \"{}\"", c.display_name(), s.format("%Y-%m-%d %H:%M:%S"), e.format("%Y-%m-%d %H:%M:%S"));
    }
    if day.failed > 0 {
        println!("  ({} sondage(s) sans réponse : le résultat peut être incomplet)", day.failed);
    }
    Ok(())
}

async fn replay(path: &Path, c: &CameraConfig, start: &str, end: &str, secs: Option<u64>) -> Result<()> {
    let (s, e) = (parse_dt(start)?, parse_dt(end)?);
    if e <= s {
        bail!("la fin doit être après le début");
    }
    println!("{} — relecture {} → {}{} :", c.display_name(), s.format("%Y-%m-%d %H:%M:%S"), e.format("%H:%M:%S"), secs.map(|n| format!(" (max {n}s)")).unwrap_or_default());
    jlog(path, Level::Info, &c.id, c.display_name(), format!("Relecture ouverte ({} → {})", s.format("%Y-%m-%d %H:%M"), e.format("%H:%M")));
    let cancel = Arc::new(AtomicBool::new(false));
    if let Some(n) = secs {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(n)).await;
            cancel.store(true, Ordering::Relaxed);
        });
    }
    let count = Arc::new(AtomicU64::new(0));
    let sink: stream::FrameSink = {
        let count = count.clone();
        Arc::new(move |_f| {
            count.fetch_add(1, Ordering::Relaxed);
        })
    };
    let (cfg, cancel2) = (c.clone(), cancel.clone());
    let clock = playback::new_clock();
    // Son : échantillons reçus et crête (rien n'est joué), pour vérifier qu'il suit l'image.
    let heard = Arc::new(std::sync::Mutex::new((0usize, 0f32)));
    let audio: stream::AudioSink = {
        let heard = heard.clone();
        Arc::new(move |s: &[f32], _rate: u32| {
            let mut h = heard.lock().unwrap();
            h.0 += s.len();
            h.1 = s.iter().fold(h.1, |m, x| m.max(x.abs()));
        })
    };
    let sound = Arc::new(std::sync::atomic::AtomicU8::new(playback::SOUND_UNKNOWN));
    let opts = playback::ReplayOptions { clock: Some(clock.clone()), audio: Some(audio), sound: Some(sound.clone()) };
    let t0 = Instant::now();
    let r = blocking(move || playback::decode_blocking(&cfg, s.with_timezone(&Utc), e.with_timezone(&Utc), 1280, 720, cancel2, sink, || {}, opts)).await;
    let n = count.load(Ordering::Relaxed);
    let dur = t0.elapsed().as_secs_f64();
    match &r {
        Ok(()) => {
            println!("  {n} image(s) décodée(s) en {dur:.1}s ({:.1} i/s)", if dur > 0.0 { n as f64 / dur } else { 0.0 });
            // La caméra saute les trous : l'horloge avance plus vite que le temps réel quand il y en a.
            if let Some(p) = playback::read_clock(&clock) {
                println!("  dernière image : {} (+{p:.1}s depuis la première image)", (s + chrono::Duration::milliseconds((p * 1000.0) as i64)).format("%H:%M:%S"));
            }
            let (samples, peak) = *heard.lock().unwrap();
            match sound.load(Ordering::Relaxed) {
                playback::SOUND_NONE => println!("  son : aucun (la séquence n'a pas de piste son)"),
                _ if samples == 0 => println!("  son : rien reçu"),
                _ => println!("  son : {:.1} s à {} Hz · crête {:.0} %", samples as f64 / playback::AUDIO_RATE as f64, playback::AUDIO_RATE, peak * 100.0),
            }
            jlog(path, Level::Info, &c.id, c.display_name(), format!("Relecture terminée ({n} images, {})", homesmart::kinds::fmt_secs(t0.elapsed().as_secs())));
        }
        Err(e) => jlog(path, Level::Warning, &c.id, c.display_name(), format!("Relecture impossible : {e:#}")),
    }
    if r.is_ok() && n == 0 {
        bail!("aucune image décodée : pas d'enregistrement sur cette période, ou FFmpeg a échoué");
    }
    r
}

async fn export(path: &Path, c: &CameraConfig, start: &str, end: &str, output: Option<PathBuf>) -> Result<()> {
    let (s, e) = (parse_dt(start)?, parse_dt(end)?);
    if e <= s {
        bail!("la fin doit être après le début");
    }
    let out = output.unwrap_or_else(|| playback::recordings_dir().join(playback::export_name(c.display_name(), s, e)));
    println!("{} — export {} → {} vers {}", c.display_name(), s.format("%Y-%m-%d %H:%M:%S"), e.format("%H:%M:%S"), out.display());
    println!("  (la caméra diffuse à vitesse réelle : l'export prend environ la durée de vidéo de la période)");
    let (cfg, out2) = (c.clone(), out.clone());
    let total = (e - s).num_seconds().max(1) as f64;
    let r = blocking(move || {
        let shown = std::sync::Mutex::new(-1i64);
        playback::export_blocking(&cfg, s.with_timezone(&Utc), e.with_timezone(&Utc), &out2, Arc::new(AtomicBool::new(false)), &|secs| {
            let pct = ((secs / total) * 100.0).clamp(0.0, 100.0) as i64;
            let mut last = shown.lock().unwrap();
            if pct / 10 != *last / 10 {
                *last = pct;
                println!("  … {pct} %");
            }
        })
    })
    .await;
    match r {
        Ok(()) => {
            let sz = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            let file = out.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
            jlog(path, Level::Info, &c.id, c.display_name(), format!("Extrait exporté ({} → {}) : {file}", s.format("%Y-%m-%d %H:%M"), e.format("%H:%M")));
            println!("  {} ({:.1} Mo)", out.display(), sz as f64 / 1e6);
            Ok(())
        }
        Err(e) => {
            jlog(path, Level::Warning, &c.id, c.display_name(), format!("Export impossible : {e:#}"));
            Err(e)
        }
    }
}
