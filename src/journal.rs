//! Journal permanent de toutes les actions et alertes.
//!
//! Une ligne JSON par entrée dans `journal.jsonl`, à côté de la configuration.
//! L'application et la ligne de commande y écrivent toutes les deux ; l'application
//! relit régulièrement le fichier pour afficher aussi les actions de la CLI.
//!
//! Les actions répétées très rapidement sur le même réglage (glissement d'un curseur)
//! sont regroupées en une seule entrée, qui garde la valeur finale.
//!
//! Les problèmes détectés sont écrits tout de suite et forcés sur le disque : HomeSmart tourne
//! parfois sur un PC branché sur une prise qui coupe, et ce sont les dernières lignes avant la
//! coupure qui en expliquent la cause. Une coupure peut laisser des octets nuls en fin de fichier :
//! ils sont ignorés à la lecture.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Entrées gardées en mémoire (le fichier, lui, en conserve davantage).
const MEMORY_CAP: usize = 20_000;
/// Au-delà, le fichier est compacté au démarrage en gardant les entrées les plus récentes.
const FILE_CAP_BYTES: u64 = 8 * 1024 * 1024;
const COALESCE: Duration = Duration::from_secs(3);

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Level {
    Info,
    Warning,
    Critical,
}

/// Qui est à l'origine de l'entrée.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Source {
    /// Toi, depuis l'application.
    User,
    /// La ligne de commande.
    Cli,
    /// Un programme horaire.
    Schedule,
    /// Un garde-fou.
    Guard,
    /// L'appareil lui-même (bouton physique, autre application, minuterie interne).
    Device,
    /// HomeSmart (connexions, découverte, démarrage).
    System,
    /// L'analyse des caméras par IA (personnes, visages, chutes, zones).
    Vision,
    /// L'écoute du micro des caméras par IA (sons de la maison, parole).
    Listen,
    /// Un scénario (appareil commandé par ce que voient ou entendent les caméras).
    Scenario,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::User => "Toi",
            Source::Cli => "Ligne de commande",
            Source::Schedule => "Programme",
            Source::Guard => "Garde-fou",
            Source::Device => "Appareil",
            Source::System => "Système",
            Source::Vision => "Vision IA",
            Source::Listen => "Écoute IA",
            Source::Scenario => "Scénario",
        }
    }

    pub const ALL: [Source; 9] = [Source::User, Source::Cli, Source::Schedule, Source::Guard, Source::Device, Source::System, Source::Vision, Source::Listen, Source::Scenario];
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    /// Numéro local (attribué au chargement), croissant.
    #[serde(skip)]
    pub id: u64,
    pub at: DateTime<Local>,
    pub level: Level,
    pub source: Source,
    #[serde(default)]
    pub device: String,
    #[serde(default)]
    pub device_name: String,
    pub message: String,
    /// Clé de regroupement (même réglage modifié en rafale).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    /// Processus auteur, pour ne pas relire ses propres lignes.
    #[serde(default)]
    pub pid: u32,
}

impl Entry {
    pub fn new(level: Level, source: Source, device: &str, device_name: &str, message: impl Into<String>) -> Self {
        Entry {
            id: 0,
            at: Local::now(),
            level,
            source,
            device: device.to_string(),
            device_name: device_name.to_string(),
            message: message.into(),
            key: String::new(),
            pid: std::process::id(),
        }
    }

    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = key.into();
        self
    }
}

pub struct Journal {
    path: PathBuf,
    entries: VecDeque<Entry>,
    next_id: u64,
    read_offset: u64,
    /// Dernière entrée, pas encore écrite car elle peut encore être regroupée.
    open_key: Option<String>,
}

fn append_line(path: &Path, e: &Entry) {
    write_line(path, e, false);
}

/// Ajoute la ligne ; `sync` : attendre qu'elle soit réellement sur le disque.
fn write_line(path: &Path, e: &Entry, sync: bool) {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        let _ = std::fs::create_dir_all(dir);
    }
    let line = match serde_json::to_string(e) {
        Ok(l) => l + "\n",
        Err(_) => return,
    };
    let res = std::fs::OpenOptions::new().create(true).append(true).open(path).and_then(|mut f| {
        f.write_all(line.as_bytes())?;
        if sync {
            f.sync_data()?;
        }
        Ok(())
    });
    if let Err(err) = res {
        tracing::error!("écriture du journal impossible ({}) : {err}", path.display());
    }
}

/// Ajout direct au fichier, pour un autre processus (la ligne de commande).
pub fn append_to_file(path: &Path, e: &Entry) {
    append_line(path, e);
}

impl Journal {
    pub fn open(path: PathBuf) -> Self {
        let mut j = Journal { path, entries: VecDeque::new(), next_id: 1, read_offset: 0, open_key: None };
        j.compact_if_needed();
        j.read_from(0, None);
        j
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn compact_if_needed(&self) {
        let Ok(meta) = std::fs::metadata(&self.path) else { return };
        if meta.len() <= FILE_CAP_BYTES {
            return;
        }
        let Ok(text) = std::fs::read_to_string(&self.path) else { return };
        let lines: Vec<&str> = text.lines().collect();
        let keep = &lines[lines.len().saturating_sub(MEMORY_CAP)..];
        let tmp = self.path.with_extension("jsonl.tmp");
        if std::fs::write(&tmp, keep.join("\n") + "\n").is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    /// Lit les lignes complètes à partir de `offset` ; ignore celles du processus `skip_pid`.
    fn read_from(&mut self, offset: u64, skip_pid: Option<u32>) -> Vec<Entry> {
        let mut out = Vec::new();
        let Ok(mut f) = std::fs::File::open(&self.path) else { return out };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < offset {
            // Fichier compacté ou remplacé : on repart du début sans dupliquer.
            self.read_offset = len;
            return out;
        }
        if f.seek(SeekFrom::Start(offset)).is_err() {
            return out;
        }
        let mut buf = Vec::new();
        if f.take(len - offset).read_to_end(&mut buf).is_err() {
            return out;
        }
        // On ne consomme que jusqu'au dernier saut de ligne (ligne en cours d'écriture sinon).
        let complete = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        for line in BufReader::new(&buf[..complete]).lines().map_while(Result::ok) {
            // Octets nuls laissés par une coupure de courant avant la ligne suivante.
            let Ok(mut e) = serde_json::from_str::<Entry>(line.trim_start_matches('\0')) else { continue };
            if skip_pid == Some(e.pid) {
                continue;
            }
            e.id = self.next_id;
            self.next_id += 1;
            self.push(e.clone());
            out.push(e);
        }
        self.read_offset = offset + complete as u64;
        out
    }

    fn push(&mut self, e: Entry) {
        self.entries.push_back(e);
        while self.entries.len() > MEMORY_CAP {
            self.entries.pop_front();
        }
    }

    /// Écrit l'entrée en attente de regroupement, s'il y en a une.
    pub fn flush(&mut self) {
        if self.open_key.take().is_some() {
            if let Some(last) = self.entries.back() {
                append_line(&self.path, last);
            }
        }
    }

    /// Écrit l'entrée en attente si plus personne ne la modifie depuis le délai de regroupement.
    pub fn flush_stale(&mut self) {
        let stale = self.entries.back().is_some_and(|e| (Local::now() - e.at).to_std().map(|d| d >= COALESCE).unwrap_or(true));
        if stale {
            self.flush();
        }
    }

    /// Ajoute une entrée ; renvoie son numéro (celui de l'entrée regroupée le cas échéant).
    pub fn append(&mut self, mut e: Entry) -> u64 {
        if !e.key.is_empty() && self.open_key.as_deref() == Some(e.key.as_str()) {
            if let Some(last) = self.entries.back_mut() {
                let recent = (e.at - last.at).to_std().map(|d| d < COALESCE).unwrap_or(true);
                if recent && last.device == e.device && last.source == e.source {
                    last.message = e.message;
                    last.at = e.at;
                    last.level = last.level.max_with(e.level);
                    return last.id;
                }
            }
        }
        self.flush();
        e.id = self.next_id;
        self.next_id += 1;
        if e.key.is_empty() {
            append_line(&self.path, &e);
        } else {
            self.open_key = Some(e.key.clone());
        }
        let id = e.id;
        self.push(e);
        id
    }

    /// Ajoute une entrée sans regroupement et la force sur le disque (problèmes détectés).
    pub fn append_synced(&mut self, mut e: Entry) -> u64 {
        self.flush();
        e.id = self.next_id;
        self.next_id += 1;
        write_line(&self.path, &e, true);
        let id = e.id;
        self.push(e);
        id
    }

    /// Lignes ajoutées par d'autres processus depuis la dernière lecture.
    pub fn poll_external(&mut self) -> Vec<Entry> {
        let me = std::process::id();
        let offset = self.read_offset;
        self.read_from(offset, Some(me))
    }

    /// Les entrées, la plus récente d'abord.
    pub fn latest(&self, limit: usize) -> Vec<Entry> {
        self.entries.iter().rev().take(limit).cloned().collect()
    }

    /// Entrées qui satisfont `pred`, la plus récente d'abord, au plus `limit`.
    pub fn query(&self, limit: usize, pred: impl Fn(&Entry) -> bool) -> Vec<Entry> {
        self.entries.iter().rev().filter(|e| pred(e)).take(limit).cloned().collect()
    }

    pub fn after(&self, since: u64) -> Vec<Entry> {
        self.entries.iter().rev().take_while(|e| e.id > since).cloned().collect()
    }

    pub fn last_id(&self) -> u64 {
        self.entries.back().map_or(0, |e| e.id)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn unread_important(&self, since: u64) -> usize {
        self.entries.iter().rev().take_while(|e| e.id > since).filter(|e| e.level != Level::Info).count()
    }
}

impl Level {
    fn rank(self) -> u8 {
        match self {
            Level::Info => 0,
            Level::Warning => 1,
            Level::Critical => 2,
        }
    }

    fn max_with(self, other: Level) -> Level {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!("hs-journal-{}.jsonl", rand::random::<u32>()))
    }

    #[test]
    fn persists_and_reloads() {
        let p = tmp();
        let mut j = Journal::open(p.clone());
        j.append(Entry::new(Level::Info, Source::User, "a", "Lampe", "Allumée"));
        j.append(Entry::new(Level::Warning, Source::System, "a", "Lampe", "Connexion perdue"));
        let again = Journal::open(p.clone());
        let l = again.latest(10);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].message, "Connexion perdue");
        assert_eq!(l[1].source, Source::User);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn slider_drag_is_one_entry_with_final_value() {
        let p = tmp();
        let mut j = Journal::open(p.clone());
        for v in [10, 40, 72] {
            j.append(Entry::new(Level::Info, Source::User, "a", "Lampe", format!("Luminosité : {v} %")).with_key("a:22"));
        }
        j.append(Entry::new(Level::Info, Source::User, "a", "Lampe", "Éteinte"));
        let l = j.latest(10);
        assert_eq!(l.len(), 2);
        assert_eq!(l[1].message, "Luminosité : 72 %");
        // Sur disque aussi : une seule ligne pour le curseur.
        let again = Journal::open(p.clone());
        assert_eq!(again.latest(10).len(), 2);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn nul_bytes_left_by_a_power_cut_are_skipped() {
        let p = tmp();
        let mut j = Journal::open(p.clone());
        j.append(Entry::new(Level::Warning, Source::Device, "a", "Chauffage", "Sous-tension"));
        // Coupure : le système avait réservé la place, des zéros restent avant la ligne suivante.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(&[0u8; 180]).unwrap();
        drop(f);
        let mut next = Entry::new(Level::Info, Source::System, "", "", "HomeSmart démarré");
        next.pid = std::process::id().wrapping_add(1);
        append_to_file(&p, &next);
        let again = Journal::open(p.clone());
        let l = again.latest(10);
        assert_eq!(l.len(), 2, "{l:?}");
        assert_eq!(l[0].message, "HomeSmart démarré");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn synced_entries_are_on_disk_at_once() {
        let p = tmp();
        let mut j = Journal::open(p.clone());
        j.append(Entry::new(Level::Info, Source::User, "a", "Lampe", "Luminosité : 10 %").with_key("a:22"));
        j.append_synced(Entry::new(Level::Warning, Source::Device, "a", "Lampe", "Extinction non demandée").with_key("probleme:extinction:a"));
        // Sans `flush` : les deux lignes sont déjà dans le fichier, dans l'ordre.
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(text.lines().last().unwrap().contains("probleme:extinction:a"));
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn sees_lines_written_by_another_process() {
        let p = tmp();
        let mut j = Journal::open(p.clone());
        j.append(Entry::new(Level::Info, Source::User, "a", "Lampe", "Allumée"));
        let mut other = Entry::new(Level::Info, Source::Cli, "a", "Lampe", "Éteinte");
        other.pid = std::process::id().wrapping_add(1);
        append_to_file(&p, &other);
        let new = j.poll_external();
        assert_eq!(new.len(), 1, "seule la ligne de l'autre processus est relue");
        assert_eq!(new[0].source, Source::Cli);
        assert!(j.poll_external().is_empty());
        let _ = std::fs::remove_file(p);
    }
}
