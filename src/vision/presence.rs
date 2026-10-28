//! Présence par pièce : combien de minutes, heure par heure, une caméra a vu quelqu'un.
//!
//! Tiré de l'analyse IA (rien n'est enregistré sans elle), gardé 60 jours dans `presence.json`
//! à côté de la configuration. Que des durées : ni image, ni nom.

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate, Timelike};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Jours gardés.
const KEEP_DAYS: i64 = 60;

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
struct Data {
    /// Jour → caméra → secondes de présence par heure (24 cases).
    days: BTreeMap<NaiveDate, HashMap<String, Vec<f32>>>,
}

#[derive(Debug)]
pub struct PresenceStore {
    path: PathBuf,
    data: Data,
    dirty: bool,
}

impl PresenceStore {
    pub fn load(path: PathBuf) -> Self {
        let data = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        PresenceStore { path, data, dirty: false }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Ajoute `secs` secondes de présence vue par `camera` à l'instant `at`.
    pub fn add(&mut self, camera: &str, at: DateTime<Local>, secs: f32) {
        if secs <= 0.0 {
            return;
        }
        let hours = self.data.days.entry(at.date_naive()).or_default().entry(camera.to_string()).or_insert_with(|| vec![0.0; 24]);
        let h = at.hour() as usize;
        hours.resize(24, 0.0);
        hours[h] = (hours[h] + secs).min(3600.0);
        self.dirty = true;
        // Les jours trop anciens s'en vont.
        let oldest = at.date_naive() - chrono::Duration::days(KEEP_DAYS);
        self.data.days.retain(|d, _| *d > oldest);
    }

    /// Minutes de présence par heure, pour un jour.
    pub fn day(&self, camera: &str, date: NaiveDate) -> [f32; 24] {
        let mut out = [0.0f32; 24];
        if let Some(v) = self.data.days.get(&date).and_then(|d| d.get(camera)) {
            for (o, s) in out.iter_mut().zip(v) {
                *o = s / 60.0;
            }
        }
        out
    }

    /// Oublie une caméra (supprimée).
    pub fn forget(&mut self, camera: &str) {
        for d in self.data.days.values_mut() {
            self.dirty |= d.remove(camera).is_some();
        }
    }

    pub fn save_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&self.data)?).with_context(|| format!("écriture de {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)?;
        self.dirty = false;
        Ok(())
    }
}

/// « 2 h 15 », « 12 min », « moins d'une minute ».
pub fn fmt_minutes(m: f32) -> String {
    let m = m.round() as u32;
    match (m / 60, m % 60) {
        (0, 0) => "moins d'une minute".into(),
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m:02}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn hours_days_and_persistence() {
        let path = std::env::temp_dir().join(format!("homesmart-presence-{}.json", std::process::id()));
        let mut p = PresenceStore::load(path.clone());
        let at = Local.with_ymd_and_hms(2026, 10, 21, 20, 30, 0).unwrap();
        p.add("cam", at, 90.0);
        p.add("cam", at, 30.0);
        p.add("cam", at + chrono::Duration::hours(1), 600.0);
        // Une heure ne peut pas compter plus de 60 minutes.
        p.add("autre", at, 5000.0);
        let d = p.day("cam", at.date_naive());
        assert_eq!((d[20], d[21], d[19]), (2.0, 10.0, 0.0));
        assert_eq!(p.day("autre", at.date_naive())[20], 60.0);
        p.save_if_dirty().unwrap();
        let back = PresenceStore::load(path.clone());
        assert_eq!(back.day("cam", at.date_naive()), d);
        // 60 jours plus tard, le premier jour est oublié.
        let mut p = back;
        p.add("cam", at + chrono::Duration::days(61), 10.0);
        assert_eq!(p.day("cam", at.date_naive()), [0.0; 24]);
        p.forget("cam");
        assert_eq!(p.day("cam", (at + chrono::Duration::days(61)).date_naive()), [0.0; 24]);
        let _ = std::fs::remove_file(path);
        assert_eq!(fmt_minutes(135.0), "2 h 15");
        assert_eq!(fmt_minutes(0.2), "moins d'une minute");
    }
}
