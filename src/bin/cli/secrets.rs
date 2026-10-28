//! `homesmart-cli secrets …` : où sont rangés les clés et mots de passe, et garde-fou pour qui
//! travaille sur HomeSmart : aucune valeur privée de ta configuration (clé locale, mot de passe,
//! jeton, identifiant d'appareil) ne doit partir dans un fichier publié.
//!
//! `audit --staged` sert de crochet git (`.githooks/pre-commit`) : il examine ce qui va être
//! commité et refuse le commit si l'une de ces valeurs y est. Seul l'endroit est affiché, jamais la
//! valeur.

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use homesmart::config::{self, Config};
use homesmart::protect::{self, Stored};
use homesmart::tplink::camera::hex_upper;
use md5::{Digest, Md5};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Subcommand)]
pub enum SecretsCmd {
    /// Où sont rangés les clés et mots de passe, et comment ils sont protégés
    Status,
    /// Cherche dans des fichiers les valeurs privées de ta configuration (clés, mots de passe,
    /// jetons, identifiants et n° de série) ; échoue si l'une y est
    Audit {
        /// Fichiers ou dossiers à examiner
        paths: Vec<PathBuf>,
        /// Examine ce qui est prêt à être commité (index git) : pour le crochet pre-commit
        #[arg(long)]
        staged: bool,
    },
}

pub async fn run(cmd: SecretsCmd, path: &Path) -> Result<()> {
    match cmd {
        SecretsCmd::Status => status(path),
        SecretsCmd::Audit { paths, staged } => audit(path, &paths, staged),
    }
}

fn status(path: &Path) -> Result<()> {
    let cfg = Config::load(path)?;
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let left = serde_json::from_str::<Config>(&raw).map(|mut c| c.take_secrets().count()).unwrap_or(0);
    let s = cfg.clone().take_secrets();
    let vault = config::secrets_path(path);
    println!("Configuration : {}", path.display());
    if left > 0 {
        println!("  {left} secret(s) encore écrits dedans : rangement à part impossible (voir le journal)");
    } else {
        println!("  aucun secret dedans");
    }
    println!("Secrets : {}", vault.display());
    let stored = protect::read_file(&vault, config::SECRETS_AS)?.map(|(_, s)| s).unwrap_or(Stored::Nothing);
    println!(
        "  {}",
        match stored {
            Stored::Sealed(p) => format!("chiffrés : {}", p.describe()),
            Stored::Clear => "en clair : aucun trousseau (Secret Service) pour les chiffrer ; seul ton compte peut les lire".into(),
            Stored::Nothing => "aucun secret enregistré".into(),
        }
    );
    let cams: usize = s.cameras.values().map(|c| usize::from(!c.password.is_empty()) + usize::from(!c.agent.is_empty()) + usize::from(c.tapo.is_some())).sum();
    println!("  {} clé(s) locale(s) d'appareils, {cams} secret(s) de caméras, {} secret(s) d'aspirateurs", s.devices.len(), s.vacuums.len());
    Ok(())
}

/// Une valeur à ne jamais publier, et ce qu'elle est (pour le message).
struct Needle {
    bytes: Vec<u8>,
    what: String,
}

/// Plus court, une valeur se retrouverait par hasard (« admin », « 1234 »).
const MIN_LEN: usize = 6;

/// Les valeurs privées de la configuration : secrets, mais aussi ce qui désigne tes appareils.
fn needles(cfg: &Config) -> (Vec<Needle>, Vec<(String, String)>) {
    let mut out = Vec::new();
    let mut add = |value: &str, what: String| {
        if value.len() >= MIN_LEN {
            out.push(Needle { bytes: value.as_bytes().to_vec(), what });
        }
    };
    for d in &cfg.devices {
        let name = d.display_name();
        add(&d.key, format!("clé locale de « {name} »"));
        add(&d.id, format!("identifiant de l'appareil « {name} »"));
    }
    // Empreinte MD5 du mot de passe TP-Link → caméra : le mot de passe lui-même n'est pas gardé,
    // on reconnaît un mot qui lui donnerait la même empreinte.
    let mut tapo = Vec::new();
    for c in &cfg.cameras {
        let name = c.display_name();
        add(&c.password, format!("mot de passe de la caméra « {name} »"));
        if let Some(a) = &c.agent {
            add(&a.secret, format!("secret d'appairage de la caméra « {name} »"));
        }
        if let Some(t) = &c.tapo {
            for h in [&t.md5, &t.sha256] {
                add(h, format!("empreinte du mot de passe TP-Link de « {name} »"));
                add(&h.to_lowercase(), format!("empreinte du mot de passe TP-Link de « {name} »"));
            }
            tapo.push((t.md5.clone(), format!("mot de passe TP-Link de la caméra « {name} »")));
        }
        if let Some(serial) = &c.serial {
            add(serial, format!("n° de série de la caméra « {name} »"));
        }
        if let Some(mac) = &c.mac {
            for variant in mac_variants(mac) {
                add(&variant, format!("adresse MAC de la caméra « {name} »"));
            }
        }
    }
    for v in &cfg.vacuums {
        let secret = match &v.link {
            homesmart::vacuum::Link::Valetudo { password, .. } => password.clone().unwrap_or_default(),
            homesmart::vacuum::Link::Miio { token, .. } => token.clone(),
        };
        add(&secret, format!("mot de passe ou jeton de l'aspirateur « {} »", v.name));
    }
    (out, tapo)
}

/// Une adresse MAC s'écrit de plusieurs façons : « DC:62:… », « dc-62-… », « DC62… ».
fn mac_variants(mac: &str) -> Vec<String> {
    let hex: String = mac.chars().filter(char::is_ascii_hexdigit).collect();
    if hex.len() != 12 {
        return vec![mac.to_string()];
    }
    let pairs: Vec<&str> = (0..6).map(|i| &hex[i * 2..i * 2 + 2]).collect();
    let mut out = Vec::new();
    for sep in [":", "-", ""] {
        let joined = pairs.join(sep);
        out.push(joined.to_uppercase());
        out.push(joined.to_lowercase());
    }
    out
}

/// Ligne (à partir de 1) de la première occurrence de `needle`.
fn find_line(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let at = hay.windows(needle.len()).position(|w| w == needle)?;
    Some(hay[..at].iter().filter(|b| **b == b'\n').count() + 1)
}

/// Mots d'une ligne qui pourraient être un mot de passe (entre guillemets, ou isolés).
fn candidate_words(line: &str) -> impl Iterator<Item = &str> {
    line.split(|c: char| c.is_whitespace() || "\"'`()[]{},;<>".contains(c)).filter(|w| (MIN_LEN..=64).contains(&w.len()))
}

/// Ce que contient un fichier : (ligne, ce que c'est) pour chaque valeur privée trouvée.
fn scan(content: &[u8], needles: &[Needle], tapo: &[(String, String)]) -> Vec<(usize, String)> {
    let mut hits: Vec<(usize, String)> = needles.iter().filter_map(|n| find_line(content, &n.bytes).map(|l| (l, n.what.clone()))).collect();
    if !tapo.is_empty() {
        if let Ok(text) = std::str::from_utf8(content) {
            'lines: for (i, line) in text.lines().enumerate() {
                for w in candidate_words(line) {
                    let md5 = hex_upper(&Md5::digest(w.as_bytes()));
                    if let Some((_, what)) = tapo.iter().find(|(h, _)| *h == md5) {
                        hits.push((i + 1, what.clone()));
                        continue 'lines;
                    }
                }
            }
        }
    }
    hits.sort();
    hits.dedup();
    hits
}

/// Fichiers prêts à être commités, tels qu'ils seront commités (index git).
fn staged_files() -> Result<Vec<(String, Vec<u8>)>> {
    let git = |args: &[&str]| -> Result<Vec<u8>> {
        let out = Command::new("git").args(args).output().context("git introuvable")?;
        if !out.status.success() {
            bail!("git {} : {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(out.stdout)
    };
    let names = git(&["diff", "--cached", "--name-only", "-z", "--diff-filter=ACMR"])?;
    let mut files = Vec::new();
    for name in names.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let name = String::from_utf8_lossy(name).into_owned();
        let content = git(&["show", &format!(":{name}")])?;
        files.push((name, content));
    }
    Ok(files)
}

/// Fichiers sous `paths` (dossiers parcourus, hors compilation et dépôt git).
fn files_under(paths: &[PathBuf]) -> Result<Vec<(String, Vec<u8>)>> {
    const SKIP: [&str; 4] = ["target", ".git", "graphify-out", "node_modules"];
    let mut stack: Vec<PathBuf> = paths.to_vec();
    let mut files = Vec::new();
    while let Some(p) = stack.pop() {
        let meta = std::fs::metadata(&p).with_context(|| format!("lecture de {}", p.display()))?;
        if meta.is_dir() {
            for e in std::fs::read_dir(&p)?.flatten() {
                if !SKIP.iter().any(|s| e.file_name() == *s) {
                    stack.push(e.path());
                }
            }
        } else if meta.len() <= 64 << 20 {
            files.push((p.display().to_string(), std::fs::read(&p).with_context(|| format!("lecture de {}", p.display()))?));
        }
    }
    files.sort();
    Ok(files)
}

fn audit(path: &Path, paths: &[PathBuf], staged: bool) -> Result<()> {
    let files = match (staged, paths.is_empty()) {
        (true, _) => staged_files()?,
        (false, false) => files_under(paths)?,
        (false, true) => bail!("indique des fichiers ou des dossiers, ou --staged pour ce qui va être commité"),
    };
    let (needles, tapo) = needles(&Config::load(path)?);
    let mut found = 0;
    for (name, content) in &files {
        for (line, what) in scan(content, &needles, &tapo) {
            println!("  {name}:{line} : {what}");
            found += 1;
        }
    }
    if found > 0 {
        bail!("{found} valeur(s) privée(s) de ta configuration trouvée(s) : remplace-les par des valeurs factices avant de publier");
    }
    println!("Aucune valeur privée de ta configuration dans {} fichier(s).", files.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_values_but_never_prints_them() {
        let needles = vec![Needle { bytes: b"k3y-l0cale-T3st!".to_vec(), what: "clé locale de « Prise »".into() }];
        let tapo = vec![(hex_upper(&Md5::digest(b"Sim-pass1!")), "mot de passe TP-Link de « Salon »".into())];
        let file = b"fn main() {\n    let k = \"k3y-l0cale-T3st!\";\n    start(\"Sim-pass1!\").await;\n    let ok = \"autre\";\n}\n";
        let hits = scan(file, &needles, &tapo);
        assert_eq!(hits, vec![(2, "clé locale de « Prise »".to_string()), (3, "mot de passe TP-Link de « Salon »".to_string())]);
        assert!(hits.iter().all(|(_, what)| !what.contains("T3st") && !what.contains("Sim-pass")));
        assert!(scan(b"rien de prive ici\n", &needles, &tapo).is_empty());
    }

    #[test]
    fn mac_written_any_way() {
        let v = mac_variants("DC:62:79:00:00:01");
        for s in ["DC:62:79:00:00:01", "dc-62-79-00-00-01", "DC6279000001"] {
            assert!(v.iter().any(|x| x == s), "{s} absent de {v:?}");
        }
    }
}
