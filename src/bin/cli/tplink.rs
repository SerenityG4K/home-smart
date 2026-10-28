//! `homesmart-cli tplink …` : appareils TP-Link en local (Tapo, Kasa) : recherche, état, marche/arrêt.
//!
//! Les prises et ampoules Tapo demandent l'e-mail et le mot de passe du compte TP-Link (protocole
//! KLAP, sans cloud) ; les anciennes prises Kasa n'en demandent aucun. Les caméras Tapo s'ajoutent
//! avec `cam add --brand tapo`.

use super::jlog;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use homesmart::journal::Level;
use homesmart::tplink::{discover, Credentials, Device};
use std::io::IsTerminal;
use std::path::Path;
use std::time::Duration;

#[derive(Subcommand)]
pub enum TplinkCmd {
    /// Cherche les appareils TP-Link (Tapo, Kasa) du réseau local
    Scan,
    /// État d'un appareil : nom, modèle, marche/arrêt, puissance
    Info {
        /// Adresse IP de l'appareil
        host: String,
        #[command(flatten)]
        account: Account,
    },
    /// Allume un appareil
    On {
        host: String,
        #[command(flatten)]
        account: Account,
    },
    /// Éteint un appareil
    Off {
        host: String,
        #[command(flatten)]
        account: Account,
    },
    /// Luminosité d'une ampoule Tapo (1 à 100 %)
    Brightness {
        host: String,
        percent: u8,
        #[command(flatten)]
        account: Account,
    },
}

/// Compte TP-Link (Tapo) : inutile pour les anciennes Kasa.
#[derive(clap::Args)]
pub struct Account {
    /// E-mail du compte TP-Link (sinon variable HOMESMART_TPLINK_EMAIL)
    #[arg(long, env = "HOMESMART_TPLINK_EMAIL")]
    email: Option<String>,
    /// Mot de passe du compte (sinon variable HOMESMART_TPLINK_PASSWORD, ou saisie au clavier)
    #[arg(long, env = "HOMESMART_TPLINK_PASSWORD", hide_env_values = true)]
    password: Option<String>,
}

impl Account {
    fn credentials(&self) -> Result<Credentials> {
        let ask = |what: &str| -> Result<String> {
            if !std::io::stdin().is_terminal() {
                return Ok(String::new());
            }
            eprint!("{what} (vide pour une ancienne Kasa) : ");
            let mut s = String::new();
            std::io::stdin().read_line(&mut s)?;
            Ok(s.trim().to_string())
        };
        let email = match &self.email {
            Some(e) => e.clone(),
            None => ask("E-mail du compte TP-Link")?,
        };
        let password = match &self.password {
            Some(p) => p.clone(),
            None if email.is_empty() => String::new(),
            None => ask("Mot de passe du compte TP-Link")?,
        };
        Ok(Credentials { email, password })
    }
}

pub async fn run(cmd: TplinkCmd, path: &Path) -> Result<()> {
    match cmd {
        TplinkCmd::Scan => scan(path).await,
        TplinkCmd::Info { host, account } => {
            let creds = account.credentials()?;
            let (info, protocol) = blocking(move || {
                let mut d = Device::connect(&host, &creds)?;
                let p = d.protocol();
                Ok((d.info()?, p))
            })
            .await?;
            println!("{} — {} ({protocol})", if info.name.is_empty() { "sans nom" } else { info.name.as_str() }, info.model);
            if let Some(on) = info.on {
                println!("  état        {}", if on { "allumé" } else { "éteint" });
            }
            if let Some(w) = info.power_w {
                println!("  puissance   {w:.1} W");
            }
            if let Some(wh) = info.today_wh {
                println!("  aujourd'hui {:.2} kWh", wh / 1000.0);
            }
            if let Some(b) = info.brightness {
                println!("  luminosité  {b} %");
            }
            println!("  MAC {} · firmware {}{}", info.mac, info.firmware, info.rssi.map(|r| format!(" · Wi-Fi {r} dBm")).unwrap_or_default());
            Ok(())
        }
        TplinkCmd::On { host, account } => switch(path, host, account, true).await,
        TplinkCmd::Off { host, account } => switch(path, host, account, false).await,
        TplinkCmd::Brightness { host, percent, account } => {
            let creds = account.credentials()?;
            let h = host.clone();
            let name = blocking(move || {
                let mut d = Device::connect(&h, &creds)?;
                let name = d.info().map(|i| i.name).unwrap_or_default();
                d.set_brightness(percent)?;
                Ok(name)
            })
            .await?;
            let label = if name.is_empty() { host.clone() } else { name };
            jlog(path, Level::Info, &format!("tplink:{host}"), &label, format!("Luminosité {}% (TP-Link, en local)", percent.clamp(1, 100)));
            println!("{label} : luminosité {} %", percent.clamp(1, 100));
            Ok(())
        }
    }
}

async fn blocking<R: Send + 'static>(f: impl FnOnce() -> Result<R> + Send + 'static) -> Result<R> {
    tokio::task::spawn_blocking(f).await.map_err(|e| anyhow!("tâche : {e}"))?
}

async fn switch(path: &Path, host: String, account: Account, on: bool) -> Result<()> {
    let creds = account.credentials()?;
    let h = host.clone();
    let r = blocking(move || {
        let mut d = Device::connect(&h, &creds)?;
        let name = d.info().map(|i| i.name).unwrap_or_default();
        d.set_on(on)?;
        Ok(name)
    })
    .await;
    let id = format!("tplink:{host}");
    match r {
        Ok(name) => {
            let label = if name.is_empty() { host.clone() } else { name };
            jlog(path, Level::Info, &id, &label, if on { "Allumé (TP-Link, en local)" } else { "Éteint (TP-Link, en local)" });
            println!("{label} : {}", if on { "allumé" } else { "éteint" });
            Ok(())
        }
        Err(e) => {
            jlog(path, Level::Warning, &id, &host, format!("{} refusé : {e:#}", if on { "Allumage" } else { "Extinction" }));
            Err(e)
        }
    }
}

async fn scan(path: &Path) -> Result<()> {
    println!("Recherche des appareils TP-Link (Tapo, Kasa)…");
    let found = discover::scan(Duration::from_millis(2500)).await;
    let titles: Vec<String> = found.iter().map(|f| format!("{} à {}", f.title(), f.ip)).collect();
    jlog(path, Level::Info, "", "", if titles.is_empty() { "Recherche TP-Link : aucun appareil".to_string() } else { format!("Recherche TP-Link : {}", titles.join(", ")) });
    if found.is_empty() {
        println!("Aucun appareil TP-Link trouvé. Est-il allumé et sur le même réseau que le PC ?");
        return Ok(());
    }
    println!("{:<16} {:<28} {:<18} {:<6} ÉTAT", "IP", "APPAREIL", "MAC", "ACCÈS");
    for f in &found {
        let state = match (&f.name, f.on) {
            (Some(n), Some(on)) => format!("« {n} », {}", if on { "allumé" } else { "éteint" }),
            (Some(n), None) => format!("« {n} »"),
            _ if f.factory_default => "pas encore installé dans l'application".into(),
            _ => "-".into(),
        };
        println!("{:<16} {:<28} {:<18} {:<6} {state}", f.ip, f.title(), f.mac.as_deref().unwrap_or("-"), f.encryption);
    }
    println!();
    if found.iter().any(|f| f.kind == discover::TpKind::Camera) {
        println!("Caméra Tapo : homesmart-cli cam add <IP> --brand tapo --user <compte de la caméra>  (compte créé dans l'application Tapo)");
    }
    if found.iter().any(|f| f.kind != discover::TpKind::Camera && f.encryption != "XOR") {
        println!("Prise ou ampoule Tapo : homesmart-cli tplink info <IP> --email <e-mail du compte TP-Link>");
    }
    Ok(())
}
