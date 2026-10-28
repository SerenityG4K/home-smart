//! Historique de consommation : on intègre la puissance mesurée (W) dans le temps.
//!
//! - Wh par heure, conservés ~13 mois ;
//! - courbe de puissance moyenne par tranche de 5 minutes, conservée 2 jours.
//!
//! Un trou de plus de 2 minutes entre deux mesures (appareil hors ligne, application
//! fermée) n'est pas extrapolé : seule l'énergie réellement observée est comptée.

use anyhow::Result;
use chrono::{DateTime, Datelike, Duration as ChronoDuration, Local, NaiveDate, Timelike};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

const MAX_GAP_S: f64 = 120.0;
const KEEP_DAYS: i64 = 400;
pub const SLOTS: usize = 288; // tranches de 5 min

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Curve {
    pub avg: Vec<f32>,
    pub n: Vec<u32>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct DeviceEnergy {
    /// "AAAA-MM-JJ" → 24 valeurs en Wh.
    #[serde(default)]
    pub hourly: BTreeMap<String, Vec<f64>>,
    /// "AAAA-MM-JJ" → puissance moyenne (W) par tranche de 5 min.
    #[serde(default)]
    pub curve: BTreeMap<String, Curve>,
}

#[derive(Serialize, Deserialize, Default)]
struct Data {
    #[serde(default)]
    devices: HashMap<String, DeviceEnergy>,
}

pub struct EnergyStore {
    path: PathBuf,
    data: Data,
    last: HashMap<String, (DateTime<Local>, f64)>,
    dirty: bool,
}

pub fn day_key(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

impl EnergyStore {
    pub fn load(path: PathBuf) -> Self {
        let data = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        EnergyStore { path, data, last: HashMap::new(), dirty: false }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Enregistre une mesure de puissance.
    pub fn record(&mut self, id: &str, now: DateTime<Local>, watts: f64) {
        let watts = watts.max(0.0);
        let dev = self.data.devices.entry(id.to_string()).or_default();
        if let Some((t0, w0)) = self.last.get(id) {
            let dt = (now - *t0).num_milliseconds() as f64 / 1000.0;
            if dt > 0.0 && dt <= MAX_GAP_S {
                let wh = (w0 + watts) / 2.0 * dt / 3600.0;
                let hours = dev.hourly.entry(day_key(now.date_naive())).or_insert_with(|| vec![0.0; 24]);
                hours[now.hour() as usize] += wh;
            }
        }
        let slot = ((now.hour() * 60 + now.minute()) / 5) as usize;
        let curve = dev.curve.entry(day_key(now.date_naive())).or_insert_with(|| Curve { avg: vec![0.0; SLOTS], n: vec![0; SLOTS] });
        let n = curve.n[slot] as f32;
        curve.avg[slot] = (curve.avg[slot] * n + watts as f32) / (n + 1.0);
        curve.n[slot] += 1;
        self.last.insert(id.to_string(), (now, watts));
        self.dirty = true;
    }

    /// L'appareil a été perdu de vue : on ne fait pas le pont entre deux sessions.
    pub fn break_series(&mut self, id: &str) {
        self.last.remove(id);
    }

    fn prune(&mut self, today: NaiveDate) {
        let oldest = day_key(today - ChronoDuration::days(KEEP_DAYS));
        let yesterday = day_key(today - ChronoDuration::days(1));
        for dev in self.data.devices.values_mut() {
            dev.hourly.retain(|k, _| *k >= oldest);
            dev.curve.retain(|k, _| *k >= yesterday);
        }
    }

    pub fn save_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.prune(Local::now().date_naive());
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string(&self.data)?)?;
        std::fs::rename(&tmp, &self.path)?;
        self.dirty = false;
        Ok(())
    }

    pub fn forget(&mut self, id: &str) {
        self.data.devices.remove(id);
        self.last.remove(id);
        self.dirty = true;
    }

    // ---- Lecture ---------------------------------------------------------------

    pub fn day_wh(&self, id: &str, day: NaiveDate) -> f64 {
        self.data.devices.get(id).and_then(|d| d.hourly.get(&day_key(day))).map(|h| h.iter().sum()).unwrap_or(0.0)
    }

    pub fn hourly(&self, id: &str, day: NaiveDate) -> Vec<f64> {
        self.data.devices.get(id).and_then(|d| d.hourly.get(&day_key(day))).cloned().unwrap_or_else(|| vec![0.0; 24])
    }

    /// Wh des `n` derniers jours, du plus ancien au plus récent (aujourd'hui compris).
    pub fn last_days(&self, id: &str, today: NaiveDate, n: usize) -> Vec<(NaiveDate, f64)> {
        (0..n as i64).rev().map(|i| today - ChronoDuration::days(i)).map(|d| (d, self.day_wh(id, d))).collect()
    }

    pub fn month_wh(&self, id: &str, today: NaiveDate) -> f64 {
        let prefix = today.format("%Y-%m").to_string();
        self.data
            .devices
            .get(id)
            .map(|d| d.hourly.iter().filter(|(k, _)| k.starts_with(&prefix)).map(|(_, h)| h.iter().sum::<f64>()).sum())
            .unwrap_or(0.0)
    }

    /// Mois précédent (même calendrier), pour comparer.
    pub fn previous_month_wh(&self, id: &str, today: NaiveDate) -> f64 {
        let first = today.with_day(1).unwrap_or(today);
        self.month_wh(id, first - ChronoDuration::days(1))
    }

    pub fn curve(&self, id: &str, day: NaiveDate) -> Option<Vec<f32>> {
        self.data.devices.get(id).and_then(|d| d.curve.get(&day_key(day))).map(|c| c.avg.clone())
    }

    /// Puissance habituelle de l'appareil quand il fonctionne vraiment (W), d'après l'historique :
    /// médiane de la moitié haute des tranches de 5 min où il consommait (un radiateur qui chauffe,
    /// un PC allumé), sinon la plus forte heure des 30 derniers jours (minorant : une heure à
    /// 1 200 Wh, c'est au moins 1 200 W). `None` si l'appareil n'a jamais tiré au moins 30 W.
    pub fn typical_active_w(&self, id: &str, today: NaiveDate) -> Option<f64> {
        let dev = self.data.devices.get(id)?;
        let mut active: Vec<f64> =
            dev.curve.values().flat_map(|c| c.avg.iter().zip(&c.n)).filter(|(w, n)| **n > 0 && **w >= 10.0).map(|(w, _)| *w as f64).collect();
        let w = if active.len() >= 6 {
            active.sort_by(f64::total_cmp);
            let top = &active[active.len() / 2..];
            top[top.len() / 2]
        } else {
            let oldest = day_key(today - ChronoDuration::days(30));
            dev.hourly.iter().filter(|(k, _)| **k >= oldest).flat_map(|(_, h)| h.iter().copied()).fold(0.0, f64::max)
        };
        (w >= 30.0).then_some(w)
    }

    pub fn has_data(&self, id: &str) -> bool {
        self.data.devices.get(id).is_some_and(|d| !d.hourly.is_empty())
    }

    /// Données fictives pour la démonstration (30 jours d'historique).
    pub fn seed_demo(&mut self, id: &str, base_w: f64, today: DateTime<Local>) {
        let dev = self.data.devices.entry(id.to_string()).or_default();
        for d in 1..30 {
            let day = today.date_naive() - ChronoDuration::days(d);
            let k = (d as f64 * 1.7).sin().abs() * 0.6 + 0.6;
            let hours: Vec<f64> = (0..24)
                .map(|h| {
                    let active = matches!(h, 6..=8 | 18..=23);
                    base_w * if active { k } else { 0.08 }
                })
                .collect();
            dev.hourly.insert(day_key(day), hours);
        }
        let mut curve = Curve { avg: vec![0.0; SLOTS], n: vec![1; SLOTS] };
        let mut hours = vec![0.0; 24];
        let now_slot = ((today.hour() * 60 + today.minute()) / 5) as usize;
        // Veille : quelques watts, sous le seuil où l'appareil « consomme » (`typical_active_w`).
        // À 5 % de la puissance (60 W pour un radiateur de 1 200 W), elle passait pour du
        // fonctionnement, et la puissance habituelle tombait à 60 W aux heures où la veille
        // occupe plus des trois quarts de la journée écoulée (vers 6 h et 18 h).
        let standby = (base_w * 0.05).min(5.0);
        for (s, v) in curve.avg.iter_mut().enumerate().take(now_slot) {
            let h = s / 12;
            let active = matches!(h, 6..=8 | 18..=23) || s + 18 > now_slot;
            *v = (if active { base_w * (0.9 + 0.2 * ((s as f64) * 0.7).sin()) } else { standby }) as f32;
            hours[h] += *v as f64 / 12.0;
        }
        dev.curve.insert(day_key(today.date_naive()), curve);
        dev.hourly.insert(day_key(today.date_naive()), hours);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(h: u32, m: u32, s: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 20, h, m, s).unwrap()
    }

    fn store() -> EnergyStore {
        EnergyStore::load(std::env::temp_dir().join(format!("hs-energy-{}.json", rand::random::<u32>())))
    }

    #[test]
    fn integrates_power_over_time() {
        let mut e = store();
        // 1000 W pendant une heure, une mesure toutes les 10 s → 1 kWh.
        for i in 0..=360 {
            let s = i * 10;
            e.record("p", t(10, 0, 0) + ChronoDuration::seconds(s), 1000.0);
        }
        let wh = e.day_wh("p", t(10, 0, 0).date_naive());
        assert!((wh - 1000.0).abs() < 1.0, "{wh}");
        // La dernière mesure (11:00:00) tombe dans l'heure suivante.
        let h = e.hourly("p", t(10, 0, 0).date_naive());
        assert!((h[10] + h[11] - 1000.0).abs() < 1.0 && h[11] < 3.0, "{:?}", &h[10..12]);
    }

    #[test]
    fn gaps_are_not_extrapolated() {
        let mut e = store();
        e.record("p", t(10, 0, 0), 1000.0);
        e.record("p", t(10, 30, 0), 1000.0); // 30 min sans nouvelles : rien compté
        assert_eq!(e.day_wh("p", t(10, 0, 0).date_naive()), 0.0);
        e.record("p", t(10, 30, 36), 1000.0); // 36 s → 10 Wh
        assert!((e.day_wh("p", t(10, 0, 0).date_naive()) - 10.0).abs() < 1e-6);
    }

    #[test]
    fn typical_power_from_history() {
        let mut e = store();
        assert_eq!(e.typical_active_w("p", t(9, 0, 0).date_naive()), None);
        // Radiateur : il chauffe à ~1 200 W par moments, thermostat coupé (0 W) le reste du temps,
        // et une tranche à moitié (600 W en moyenne) quand il s'arrête en cours de tranche.
        let day = t(9, 0, 0).date_naive();
        let mut c = Curve { avg: vec![0.0; SLOTS], n: vec![0; SLOTS] };
        for (s, w) in [(100, 1210.0), (101, 1205.0), (102, 1198.0), (103, 600.0), (104, 0.0), (105, 0.0), (106, 1220.0), (107, 1190.0), (108, 25.0)] {
            (c.avg[s], c.n[s]) = (w, 12);
        }
        e.data.devices.entry("p".into()).or_default().curve.insert(day_key(day), c);
        let w = e.typical_active_w("p", day).unwrap();
        assert!((1190.0..=1220.0).contains(&w), "{w}");
        // Sans courbe : la plus forte heure des 30 derniers jours.
        let mut h = vec![0.0; 24];
        h[21] = 1198.0;
        h[22] = 410.0;
        let mut only_hours = store();
        only_hours.data.devices.entry("q".into()).or_default().hourly.insert(day_key(day), h);
        assert_eq!(only_hours.typical_active_w("q", day), Some(1198.0));
        // Un appareil qui ne tire que quelques watts n'a pas de puissance « habituelle ».
        let mut tiny = store();
        tiny.data.devices.entry("r".into()).or_default().hourly.insert(day_key(day), vec![4.0; 24]);
        assert_eq!(tiny.typical_active_w("r", day), None);
    }

    #[test]
    fn demo_history_keeps_its_running_power_at_any_hour() {
        // Vers 6 h et 18 h, la veille occupe plus des trois quarts de la journée écoulée : elle ne
        // doit pas devenir la puissance « habituelle » (60 W pour un radiateur de 1 200 W).
        for (h, m) in [(0, 10), (6, 30), (12, 0), (19, 0), (23, 55)] {
            let mut e = store();
            e.seed_demo("p", 1200.0, t(h, m, 0));
            let w = e.typical_active_w("p", t(h, m, 0).date_naive()).unwrap();
            assert!(w >= 900.0, "{h:02}:{m:02} : {w} W");
        }
    }

    #[test]
    fn persists_and_reloads() {
        let mut e = store();
        e.record("p", t(9, 0, 0), 500.0);
        e.record("p", t(9, 1, 0), 500.0);
        e.save_if_dirty().unwrap();
        let again = EnergyStore::load(e.path().to_path_buf());
        assert!((again.day_wh("p", t(9, 0, 0).date_naive()) - 500.0 / 60.0).abs() < 1e-6);
        let _ = std::fs::remove_file(e.path());
    }
}
