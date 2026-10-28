//! `homesmart-cli vac …` : aspirateurs robots, en local (Tuya, Valetudo, miIO). Chaque commande est
//! inscrite au journal, comme depuis l'application (origine : ligne de commande).

use super::{jlog, target};
use anyhow::{anyhow, bail, Result};
use clap::Subcommand;
use homesmart::config::{Config, DeviceConfig};
use homesmart::device;
use homesmart::journal::Level;
use homesmart::kinds;
use homesmart::vacuum::{self, tuya, Command, Link, Status, VacuumConfig};
use std::path::Path;
use std::time::Duration;

#[derive(Subcommand)]
pub enum VacCmd {
    /// Cherche les robots du réseau (Valetudo, miIO), sans rien modifier
    Scan {
        #[arg(long, default_value_t = 5)]
        secs: u64,
    },
    /// Liste les aspirateurs configurés (Tuya et autres)
    List,
    /// État d'un robot : activité, batterie, réglages, erreurs, usure
    Status { robot: String },
    /// Lance un nettoyage
    Start { robot: String },
    /// Met le nettoyage en pause
    Pause { robot: String },
    /// Arrête le robot sur place
    Stop { robot: String },
    /// Renvoie le robot à sa base
    Dock { robot: String },
    /// Le robot émet un son pour qu'on le trouve
    Locate { robot: String },
    /// Autre commande : fan:<valeur>, mode:<valeur>, water:<valeur>, pattern:<valeur> (voir `vac status`)
    Run { robot: String, command: String },
    /// Ajoute un robot Valetudo (micrologiciel libre : API locale, sans compte)
    AddValetudo {
        /// Adresse IP du robot
        host: String,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        room: Option<String>,
        /// Identifiants, si l'authentification HTTP de Valetudo est activée
        #[arg(long)]
        user: Option<String>,
        #[arg(long, env = "HOMESMART_VAC_PASSWORD", hide_env_values = true)]
        password: Option<String>,
    },
    /// Ajoute un robot miIO (Xiaomi, Roborock, Dreame d'avant 2022) avec le jeton de l'appareil
    AddMiio {
        host: String,
        /// Jeton de 32 caractères hexadécimaux (sinon : variable HOMESMART_VAC_TOKEN)
        #[arg(long, env = "HOMESMART_VAC_TOKEN", hide_env_values = true)]
        token: String,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        room: Option<String>,
    },
    /// Supprime un robot Valetudo ou miIO (un robot Tuya se supprime avec `remove`)
    Remove { robot: String },
}

/// Un robot de la configuration.
enum Robot {
    Tuya(DeviceConfig),
    Other(VacuumConfig),
}

impl Robot {
    fn id(&self) -> &str {
        match self {
            Robot::Tuya(d) => &d.id,
            Robot::Other(v) => &v.id,
        }
    }

    fn name(&self) -> &str {
        match self {
            Robot::Tuya(d) => d.display_name(),
            Robot::Other(v) => v.display_name(),
        }
    }
}

/// Appareil Tuya qui est un aspirateur (type choisi ou description des DP).
fn is_tuya_vacuum(d: &DeviceConfig) -> bool {
    d.kind.as_deref() == Some("vacuum") || tuya::schema_is_vacuum(&d.schema)
}

fn robots(cfg: &Config) -> Vec<Robot> {
    let mut out: Vec<Robot> = cfg.devices.iter().filter(|d| is_tuya_vacuum(d)).cloned().map(Robot::Tuya).collect();
    out.extend(cfg.vacuums.iter().cloned().map(Robot::Other));
    out
}

fn find(path: &Path, query: &str) -> Result<Robot> {
    let cfg = Config::load(path)?;
    let q = query.trim();
    let ql = q.to_lowercase();
    let list = robots(&cfg);
    let i = list
        .iter()
        .position(|r| r.id() == q || r.name().eq_ignore_ascii_case(q))
        .or_else(|| {
            let hits: Vec<usize> = list.iter().enumerate().filter(|(_, r)| r.name().to_lowercase().contains(&ql)).map(|(i, _)| i).collect();
            (hits.len() == 1).then(|| hits[0])
        })
        .ok_or_else(|| anyhow!("aspirateur inconnu : {query} (voir `homesmart-cli vac list`)"))?;
    Ok(list.into_iter().nth(i).unwrap())
}

fn print_status(name: &str, st: &Status, caps: &vacuum::Caps) {
    println!("{name} — {}", st.activity.label());
    if let Some(b) = st.battery {
        println!("  Batterie        {b} %{}", if st.charging { " (en charge)" } else { "" });
    }
    let fmt = |v: &Option<String>, label: &dyn Fn(&str) -> String| v.as_deref().map(label).unwrap_or_else(|| "—".into());
    if !caps.fans.is_empty() {
        println!("  Puissance       {}   (fan:{})", fmt(&st.fan, &|v| caps.fan_label(v)), caps.fans.iter().map(|c| c.value.as_str()).collect::<Vec<_>>().join(" | "));
    }
    if !caps.modes.is_empty() {
        println!("  Mode            {}   (mode:{})", fmt(&st.mode, &|v| caps.mode_label(v)), caps.modes.iter().map(|c| c.value.as_str()).collect::<Vec<_>>().join(" | "));
    }
    if !caps.waters.is_empty() {
        println!("  Eau             {}   (water:{})", fmt(&st.water, &|v| caps.water_label(v)), caps.waters.iter().map(|c| c.value.as_str()).collect::<Vec<_>>().join(" | "));
    }
    if !caps.patterns.is_empty() {
        println!("  Façons          pattern:{}", caps.patterns.iter().map(|c| c.value.as_str()).collect::<Vec<_>>().join(" | "));
    }
    if let (Some(a), Some(m)) = (st.area_m2, st.clean_min) {
        if a > 0.0 || m > 0 {
            println!("  Nettoyage       {} m² en {}", kinds::fr(a, 1), kinds::fmt_secs(m as u64 * 60));
        }
    }
    for e in &st.errors {
        println!("  ERREUR          {}", e.sentence());
    }
    for c in &st.consumables {
        let left = match (c.percent, c.hours) {
            (Some(p), Some(h)) => format!("{} % ({} h)", kinds::fr(p, 0), kinds::fr(h, 0)),
            (Some(p), None) => format!("{} %", kinds::fr(p, 0)),
            (None, Some(h)) => format!("{} h", kinds::fr(h, 0)),
            (None, None) => "—".into(),
        };
        println!("  {:<15} {left}", c.label);
    }
}

pub async fn run(cmd: VacCmd, path: &Path) -> Result<()> {
    match cmd {
        VacCmd::Scan { secs } => {
            println!("Écoute du réseau pendant {secs} s (Valetudo par mDNS, miIO par poignée de main)…");
            jlog(path, Level::Info, "", "", "Recherche d'aspirateurs robots sur le réseau (Valetudo, miIO)");
            let found = vacuum::scan::scan(Duration::from_secs(secs.clamp(1, 60))).await;
            if found.is_empty() {
                println!("Aucun robot Valetudo ni miIO ne répond.");
            }
            for f in &found {
                println!("  {:<16} {:<32} {}", f.host, f.family.label(), f.label);
            }
            println!("Robots Tuya / Smart Life : `homesmart-cli scan`, puis import des clés (`import-cloud`).");
            println!("Dreame récents (application Dreamehome) : aucune API locale ; seul Valetudo permet de s'en passer.");
            Ok(())
        }
        VacCmd::List => {
            let cfg = Config::load(path)?;
            let list = robots(&cfg);
            if list.is_empty() {
                println!("Aucun aspirateur configuré.");
            }
            for r in &list {
                let (family, host) = match r {
                    Robot::Tuya(d) => ("Tuya (local)".to_string(), d.ip.clone().unwrap_or_else(|| "auto".into())),
                    Robot::Other(v) => (v.link.family().label().to_string(), v.link.host().to_string()),
                };
                println!("{:<24} {:<32} {:<16} {}", r.name(), family, host, r.id());
            }
            Ok(())
        }
        VacCmd::Status { robot } => match find(path, &robot)? {
            Robot::Tuya(d) => {
                let t = target(&d).await?;
                let dps = device::fetch_status(&t).await?;
                print_status(d.display_name(), &tuya::status(&dps, &d.schema), &tuya::caps(&dps, &d.schema));
                Ok(())
            }
            Robot::Other(v) => {
                let (model, st, caps) = homesmart::hub::read_vacuum_link(&v.link).await?;
                print_status(&format!("{} ({model})", v.display_name()), &st, &caps);
                Ok(())
            }
        },
        VacCmd::Start { robot } => command(path, &robot, Command::Start).await,
        VacCmd::Pause { robot } => command(path, &robot, Command::Pause).await,
        VacCmd::Stop { robot } => command(path, &robot, Command::Stop).await,
        VacCmd::Dock { robot } => command(path, &robot, Command::Dock).await,
        VacCmd::Locate { robot } => command(path, &robot, Command::Locate).await,
        VacCmd::Run { robot, command: c } => {
            let cmd = Command::parse(&c).ok_or_else(|| anyhow!("commande inconnue : {c} (start, pause, stop, dock, locate, fan:…, mode:…, water:…, pattern:…)"))?;
            command(path, &robot, cmd).await
        }
        VacCmd::AddValetudo { host, port, name, room, user, password } => {
            let password = user.as_ref().map(|_| password.unwrap_or_default());
            add(path, Link::Valetudo { host, port, user, password }, name, room).await
        }
        VacCmd::AddMiio { host, token, port, name, room } => {
            vacuum::miio::parse_token(&token)?;
            add(path, Link::Miio { host, token: token.trim().to_lowercase(), port }, name, room).await
        }
        VacCmd::Remove { robot } => {
            let r = find(path, &robot)?;
            let Robot::Other(v) = r else { bail!("« {} » est un appareil Tuya : `homesmart-cli remove`", r.name()) };
            let mut cfg = Config::load(path)?;
            cfg.vacuums.retain(|x| x.id != v.id);
            cfg.schedules.retain(|s| s.device != v.id);
            cfg.scenarios.retain(|s| s.device != v.id);
            cfg.save(path)?;
            jlog(path, Level::Info, &v.id, v.display_name(), "Aspirateur supprimé (ses programmes et scénarios aussi)");
            println!("Supprimé : {}", v.display_name());
            Ok(())
        }
    }
}

/// Commande journalisée : réussie, refusée ou échouée.
async fn command(path: &Path, robot: &str, cmd: Command) -> Result<()> {
    let r = find(path, robot)?;
    let (id, name) = (r.id().to_string(), r.name().to_string());
    match r {
        Robot::Tuya(d) => {
            let t = target(&d).await?;
            let dps = device::fetch_status(&t).await?;
            let caps = tuya::caps(&dps, &d.schema);
            let what = cmd.describe(&caps);
            let req = cmd.check(&caps).and_then(|_| tuya::command(&cmd, &dps, &d.schema)).and_then(|m| kinds::check_writable(&m, &dps, &d.schema).map(|_| m));
            let req = match req {
                Ok(m) => m,
                Err(e) => {
                    jlog(path, Level::Warning, &id, &name, format!("Refusé : {what} — {e}"));
                    bail!(e);
                }
            };
            match device::set_dps(&t, &req).await {
                Ok(_) => {
                    jlog(path, Level::Info, &id, &name, what.clone());
                    println!("{name} : {what}");
                    Ok(())
                }
                Err(e) => {
                    jlog(path, Level::Warning, &id, &name, format!("Échec : {what} ({e:#})"));
                    Err(e)
                }
            }
        }
        Robot::Other(v) => {
            let (what, result) = homesmart::hub::run_vacuum_link(&v.link, &cmd).await?;
            match result {
                Ok(()) => {
                    jlog(path, Level::Info, &id, &name, what.clone());
                    println!("{name} : {what}");
                    Ok(())
                }
                Err(e) => {
                    jlog(path, Level::Warning, &id, &name, format!("Échec : {what} ({e:#})"));
                    Err(e)
                }
            }
        }
    }
}

async fn add(path: &Path, link: Link, name: Option<String>, room: Option<String>) -> Result<()> {
    let mut cfg = Config::load(path)?;
    let host = link.host().trim().to_string();
    if let Some(v) = cfg.vacuums.iter().find(|v| v.link.host() == host && v.link.port() == link.port()) {
        bail!("cet aspirateur est déjà ajouté (« {} »)", v.display_name());
    }
    println!("Connexion à {host}…");
    let (model, st) = homesmart::hub::test_vacuum_link(&link).await?;
    let base = VacuumConfig::make_id(&host);
    let taken = |id: &str| cfg.vacuums.iter().any(|v| v.id == id) || cfg.devices.iter().any(|d| d.id == id);
    let id = (1..).map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") }).find(|id| !taken(id)).unwrap_or(base);
    let family = link.family().label();
    let v = VacuumConfig { id: id.clone(), name: name.unwrap_or_else(|| "Aspirateur".into()), room, link, model: Some(model.clone()).filter(|m| !m.is_empty()) };
    let label = v.display_name().to_string();
    cfg.vacuums.push(v);
    cfg.save(path)?;
    jlog(path, Level::Info, &id, &label, format!("Aspirateur ajouté ({family}, {host})"));
    println!("Ajouté : « {label} » — {model} — {}", st.summary());
    println!("(Si HomeSmart est ouvert, relance-le pour qu'il le voie.)");
    Ok(())
}
