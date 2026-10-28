//! Relecture des enregistrements de la carte SD, en local, par RTSP horodaté.
//!
//! Les caméras Imou / Dahua servent la vidéo déjà enregistrée sur leur carte SD par un flux RTSP
//! spécial : `/cam/playback?channel=1&subtype=0&starttime=…&endtime=…`. **Lecture seule :** on lit
//! la carte, on n'y écrit rien (le formatage, la suppression et la rétention passent par une API
//! verrouillée par Imou, impossible en local sur ce modèle).
//!
//! Deux pièges vérifiés sur la vraie caméra (Imou Ranger 2C) :
//! - **les horodatages sont en UTC** : la caméra indexe ses enregistrements sur son horloge UTC,
//!   pas sur l'heure locale. On convertit donc toujours en UTC avant d'interroger.
//! - **la présence de vidéo se lit dans le `npt`** : une période vide renvoie `200 OK` avec
//!   `a=range:npt=0-0.000000` (et non une erreur). Seule une durée `npt` > 0 signifie « il y a de
//!   la vidéo ». Les périodes dans le futur renvoient des réponses fantaisistes : on ne sonde que le passé.
//!
//! - **la relecture saute les trous** : sur une période qui contient plusieurs séquences, la caméra
//!   enchaîne les séquences sans attendre, et l'horodatage des images fait un saut égal au trou.
//!   L'horodatage d'une image vaut donc toujours « heure de la première image + pts ».
//!
//! Le décodage (relecture) et l'export passent par FFmpeg (les enregistrements sont en H.265).

use super::stream::{AudioSink, FrameSink};
use super::{ffmpeg, CameraConfig, Frame};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Local, NaiveDate, TimeZone, Utc};
use md5::{Digest, Md5};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Sondages de la carte menés en même temps : la caméra n'accepte que peu de connexions.
const PROBES_AT_ONCE: usize = 3;

/// La caméra ne ferme pas la session à la fin de la période relue (FFmpeg attendrait ~80 s).
/// Passé ce délai de silence réseau (en µs), FFmpeg considère le flux comme fini et termine
/// proprement : dernières images décodées, fichier .mp4 finalisé (vérifié sur la vraie caméra).
/// Les trous, eux, sont sautés instantanément, donc jamais confondus avec la fin.
const RTSP_IDLE_US: &str = "4000000";

/// Secours si FFmpeg ne s'arrête pas de lui-même : silence au-delà duquel on l'arrête.
const END_IDLE: Duration = Duration::from_secs(12);

/// Position de lecture partagée : secondes écoulées depuis la première image (`f64` en bits),
/// `NaN` tant qu'aucune image n'est sortie.
pub type Clock = Arc<AtomicU64>;

pub fn new_clock() -> Clock {
    Arc::new(AtomicU64::new(f64::NAN.to_bits()))
}

pub fn read_clock(c: &Clock) -> Option<f64> {
    Some(f64::from_bits(c.load(Ordering::Relaxed))).filter(|v| v.is_finite())
}

/// Disponibilité des enregistrements pour une heure (vue par la frise de l'interface).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HourFootage {
    /// Heure locale, 0…23.
    pub hour: u32,
    /// Minutes de vidéo trouvées sur cette heure (0 = aucune).
    pub minutes: f32,
    /// Heure encore à venir : non sondée (la caméra ne peut rien avoir enregistré).
    pub future: bool,
    /// La caméra a répondu pour cette heure.
    pub answered: bool,
}

impl HourFootage {
    pub fn has_video(&self) -> bool {
        self.minutes > 0.0
    }
}

/// Une séquence enregistrée sur la carte (bornes en UTC, à la seconde près le plus souvent).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clip {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl Clip {
    pub fn secs(&self) -> i64 {
        (self.end - self.start).num_seconds().max(1)
    }

    pub fn contains(&self, t: DateTime<Utc>) -> bool {
        t >= self.start && t < self.end
    }
}

/// Enregistrements d'une journée (heure locale), tels que lus sur la carte.
#[derive(Clone, Debug, Default)]
pub struct DayFootage {
    pub date: NaiveDate,
    /// Heure par heure : minutes de vidéo (exactes une fois l'analyse terminée).
    pub hours: Vec<HourFootage>,
    /// Séquences, dans l'ordre chronologique.
    pub clips: Vec<Clip>,
    /// Avancement de l'analyse, 0…1.
    pub progress: f32,
    pub complete: bool,
    /// Sondages restés sans réponse (le résultat peut être incomplet).
    pub failed: usize,
    /// Moment de l'analyse (pour ne réanalyser que la fin de la journée en cours).
    pub scanned_at: Option<DateTime<Utc>>,
}

impl DayFootage {
    pub fn total_secs(&self) -> i64 {
        self.clips.iter().map(Clip::secs).sum()
    }

    /// Séquence qui contient `t`, ou à défaut la première qui commence après.
    pub fn clip_from(&self, t: DateTime<Utc>) -> Option<usize> {
        self.clips.iter().position(|c| c.end > t)
    }

    /// Séquence qui contient `t`.
    pub fn clip_at(&self, t: DateTime<Utc>) -> Option<usize> {
        self.clips.iter().position(|c| c.contains(t))
    }
}

/// Bornes UTC d'une heure locale (`None` : heure sautée au passage à l'heure d'été).
pub fn hour_window(date: NaiveDate, hour: u32) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let start = local_at(date, hour)?;
    let end = if hour >= 23 { local_at(date.succ_opt()?, 0)? } else { local_at(date, hour + 1).unwrap_or(start + ChronoDuration::hours(1)) };
    Some((start.with_timezone(&Utc), end.with_timezone(&Utc)))
}

/// Bornes UTC d'une journée locale.
pub fn day_window(date: NaiveDate) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    Some((local_at(date, 0)?.with_timezone(&Utc), local_at(date.succ_opt()?, 0)?.with_timezone(&Utc)))
}

/// Horodatage au format attendu par la caméra : `2026_10_20_14_05_00`, **en UTC**.
fn fmt_cam_time(t: DateTime<Utc>) -> String {
    t.format("%Y_%m_%d_%H_%M_%S").to_string()
}

/// URL RTSP de relecture d'une période (flux principal enregistré), **sans** identifiants.
pub fn playback_url(cfg: &CameraConfig, start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    format!(
        "rtsp://{}:{}/cam/playback?channel=1&subtype=0&starttime={}&endtime={}",
        cfg.host,
        cfg.rtsp_port,
        fmt_cam_time(start),
        fmt_cam_time(end)
    )
}

/// Encode un composant « user info » d'URL (RFC 3986) pour l'insérer sans danger.
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Même URL, avec les identifiants pour FFmpeg (usage local uniquement ; **ne jamais journaliser**).
fn playback_url_with_creds(cfg: &CameraConfig, start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    let base = playback_url(cfg, start, end);
    if cfg.username.is_empty() {
        return base;
    }
    let rest = base.strip_prefix("rtsp://").unwrap_or(&base);
    let creds = if cfg.password.is_empty() { enc(&cfg.username) } else { format!("{}:{}", enc(&cfg.username), enc(&cfg.password)) };
    format!("rtsp://{creds}@{rest}")
}

/// Dossier des enregistrements exportés : `HOMESMART_RECORDINGS`, sinon `Vidéos/HomeSmart`.
pub fn recordings_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("HOMESMART_RECORDINGS").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_default();
    home.join("Videos").join("HomeSmart")
}

/// Nom de fichier d'un extrait exporté (heure locale) : `<caméra> 2026-10-20 14h05-14h12.mp4`.
pub fn export_name(camera: &str, start: DateTime<Local>, end: DateTime<Local>) -> String {
    let clean: String = camera.chars().map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { '_' }).collect();
    format!("{} {} {}-{}.mp4", clean.trim(), start.format("%Y-%m-%d"), start.format("%Hh%M"), end.format("%Hh%M"))
}

// ---- Présence d'enregistrements (DESCRIBE brut, lecture du npt) ----------------------------

fn md5_hex(s: &str) -> String {
    Md5::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// `realm`, `nonce` et éventuel `qop` d'un en-tête `WWW-Authenticate: Digest …`.
fn parse_www_auth(head: &str) -> Option<(String, String, Option<String>)> {
    let line = head.lines().find(|l| l.to_ascii_lowercase().starts_with("www-authenticate"))?;
    let get = |k: &str| {
        let pat = format!("{k}=\"");
        let i = line.find(&pat)? + pat.len();
        let j = line[i..].find('"')? + i;
        Some(line[i..j].to_string())
    };
    Some((get("realm")?, get("nonce")?, get("qop")))
}

/// En-tête `Authorization: Digest …` (RFC 2069 sans `qop`, ou `qop=auth`).
fn digest_header(user: &str, pass: &str, method: &str, uri: &str, realm: &str, nonce: &str, qop: Option<&str>) -> String {
    let ha1 = md5_hex(&format!("{user}:{realm}:{pass}"));
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    match qop {
        Some(q) if q.split(',').any(|x| x.trim() == "auth") => {
            let cnonce = format!("{:08x}", rand::random::<u32>());
            let nc = "00000001";
            let resp = md5_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"));
            format!("Digest username=\"{user}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", qop=auth, nc={nc}, cnonce=\"{cnonce}\", response=\"{resp}\"")
        }
        _ => {
            let resp = md5_hex(&format!("{ha1}:{nonce}:{ha2}"));
            format!("Digest username=\"{user}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{resp}\"")
        }
    }
}

/// Durée annoncée dans le SDP (`a=range:npt=0-<secondes>`), ou `None` si ouvert (`now-`).
fn parse_npt(sdp: &str) -> Option<f64> {
    let i = sdp.find("a=range:npt=")? + "a=range:npt=".len();
    let token = sdp[i..].split(['\r', '\n', ' ']).next()?;
    let dur = token.split('-').nth(1)?.trim();
    if dur.is_empty() || dur.starts_with("now") {
        return None;
    }
    dur.parse().ok()
}

async fn read_rtsp(s: &mut tokio::net::TcpStream) -> Result<(u16, String, String)> {
    use tokio::io::AsyncReadExt;
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut tmp = [0u8; 2048];
    loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let body_start = pos + 4;
            let clen: usize = head
                .lines()
                .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.trim().parse().ok()))
                .unwrap_or(0);
            while buf.len() < body_start + clen {
                let n = s.read(&mut tmp).await?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let status = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse().ok()).unwrap_or(0);
            let body = String::from_utf8_lossy(&buf[body_start..(body_start + clen).min(buf.len())]).to_string();
            return Ok((status, head, body));
        }
        let n = s.read(&mut tmp).await?;
        if n == 0 {
            bail!("connexion RTSP fermée avant la réponse");
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

/// DESCRIBE d'une période de relecture. `Ok(None)` = aucune vidéo (404, ou 200 de durée nulle),
/// `Ok(Some(secondes))` = durée de vidéo disponible sur la période.
pub async fn footage_seconds(cfg: &CameraConfig, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Option<f64>> {
    use tokio::io::AsyncWriteExt;
    let uri = playback_url(cfg, start, end);
    let addr = format!("{}:{}", cfg.host, cfg.rtsp_port);
    let (user, pass) = (cfg.username.clone(), cfg.password.clone());
    let work = async {
        let mut s = tokio::net::TcpStream::connect(&addr).await.context("connexion RTSP")?;
        s.write_all(format!("DESCRIBE {uri} RTSP/1.0\r\nCSeq: 1\r\nAccept: application/sdp\r\n\r\n").as_bytes()).await?;
        let (status, head, body) = read_rtsp(&mut s).await?;
        let (status, body) = if status == 401 {
            let (realm, nonce, qop) = parse_www_auth(&head).ok_or_else(|| anyhow!("défi d'authentification RTSP illisible"))?;
            let auth = digest_header(&user, &pass, "DESCRIBE", &uri, &realm, &nonce, qop.as_deref());
            s.write_all(format!("DESCRIBE {uri} RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\nAuthorization: {auth}\r\n\r\n").as_bytes()).await?;
            let (st, _h, b) = read_rtsp(&mut s).await?;
            (st, b)
        } else {
            (status, body)
        };
        match status {
            200 => Ok(Some(parse_npt(&body).unwrap_or(0.0))),
            404 | 453 => Ok(None),
            401 | 403 => bail!("identifiants refusés par la caméra (RTSP {status})"),
            other => bail!("réponse RTSP inattendue ({other})"),
        }
    };
    tokio::time::timeout(Duration::from_secs(9), work).await.map_err(|_| anyhow!("la caméra ne répond pas (DESCRIBE)"))?
}

/// Y a-t-il de la vidéo enregistrée sur cette période ? (durée > 0).
pub async fn available(cfg: &CameraConfig, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<bool> {
    Ok(footage_seconds(cfg, start, end).await?.is_some_and(|s| s > 0.0))
}

/// Début d'une heure locale donnée (gère les sauts d'heure d'été).
pub fn local_at(date: NaiveDate, hour: u32) -> Option<DateTime<Local>> {
    Local.from_local_datetime(&date.and_hms_opt(hour, 0, 0)?).earliest()
}

/// Sonde une période : `Some(secondes)` (0 = rien), `None` = pas de réponse après un second essai
/// (le direct, la frise et les vignettes se partagent les rares connexions de la caméra).
async fn probe(cfg: &CameraConfig, a: DateTime<Utc>, b: DateTime<Utc>) -> Option<f64> {
    for _ in 0..2 {
        if let Ok(v) = footage_seconds(cfg, a, b).await {
            return Some(v.unwrap_or(0.0));
        }
    }
    None
}

/// Sonde plusieurs périodes, quelques-unes à la fois ; résultats dans l'ordre des périodes.
async fn probe_all(cfg: &CameraConfig, windows: &[(DateTime<Utc>, DateTime<Utc>)]) -> Vec<Option<f64>> {
    use futures::stream::{self, StreamExt};
    // Tâches toutes construites d'avance (sans fermeture qui emprunte) : le futur reste `Send`.
    let jobs: Vec<_> = windows
        .iter()
        .copied()
        .map(|(a, b)| {
            let cfg = cfg.clone();
            async move { probe(&cfg, a, b).await }
        })
        .collect();
    stream::iter(jobs).buffered(PROBES_AT_ONCE).collect().await
}

/// Minutes de vidéo heure par heure pour une journée (heure **locale**), convertie en UTC pour
/// interroger la caméra. Ici « minutes » = étendue de la première à la dernière image de l'heure
/// (sondage rapide, 24 requêtes) ; `scan_day_detail` donne le détail exact. Les heures futures ne
/// sont pas sondées (`future = true`).
pub async fn scan_day(cfg: &CameraConfig, date: NaiveDate) -> Vec<HourFootage> {
    let now = Utc::now();
    let windows: Vec<Option<(DateTime<Utc>, DateTime<Utc>)>> = (0..24u32).map(|h| hour_window(date, h).filter(|(a, _)| *a < now).map(|(a, b)| (a, b.min(now)))).collect();
    let asked: Vec<(DateTime<Utc>, DateTime<Utc>)> = windows.iter().flatten().copied().collect();
    let mut answers = probe_all(cfg, &asked).await.into_iter();
    (0..24u32)
        .map(|h| match windows[h as usize] {
            None => HourFootage { hour: h, future: hour_window(date, h).is_some(), ..Default::default() },
            Some(_) => match answers.next().flatten() {
                Some(v) => HourFootage { hour: h, minutes: (v / 60.0) as f32, future: false, answered: true },
                None => HourFootage { hour: h, ..Default::default() },
            },
        })
        .collect()
}

/// Tranche de la carte sondée : début, longueur (s), secondes de vidéo (étendue).
type Slot = (DateTime<Utc>, i64, f64);

fn slot_end(s: &Slot) -> DateTime<Utc> {
    s.0 + ChronoDuration::seconds(s.1)
}

/// Tranche entièrement couverte (à une seconde près).
fn slot_full(s: &Slot) -> bool {
    s.2 >= s.1 as f64 - 1.0
}

/// Découpe `[a, b)` en tranches de `step` secondes.
fn split(a: DateTime<Utc>, b: DateTime<Utc>, step: i64) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut out = Vec::new();
    let mut t = a;
    while t < b {
        let e = (t + ChronoDuration::seconds(step)).min(b);
        out.push((t, e));
        t = e;
    }
    out
}

/// Séquences à partir des tranches sondées. `joined[i]` : la tranche `i` se prolonge sans trou
/// dans la tranche `i + 1` (vérifié par un sondage à cheval). `exact` : bornes mesurées des
/// tranches isolées. Règle d'une suite de tranches jointes : la première se termine au bord
/// (début = bord − sa durée), la dernière commence au bord (fin = début de tranche + sa durée).
fn build_clips(slots: &[Slot], joined: &[bool], exact: &HashMap<DateTime<Utc>, Clip>) -> Vec<Clip> {
    let secs = |v: f64| ChronoDuration::seconds(v.round().max(0.0) as i64);
    let mut out: Vec<Clip> = Vec::new();
    let mut i = 0;
    while i < slots.len() {
        let mut j = i;
        while j + 1 < slots.len() && joined.get(j).copied().unwrap_or(false) {
            j += 1;
        }
        let (a, la, sa) = slots[i];
        let (b, lb, sb) = slots[j];
        let clip = if i == j {
            exact.get(&a).copied().unwrap_or_else(|| {
                // Position inconnue dans la tranche : on la centre (erreur ≤ la moitié du vide).
                let s = sa.min(la as f64);
                let start = a + secs((la as f64 - s) / 2.0);
                Clip { start, end: start + secs(s.max(1.0)) }
            })
        } else {
            Clip { start: a + secs(la as f64 - sa.min(la as f64)), end: b + secs(sb.min(lb as f64).max(1.0)) }
        };
        // Deux séquences qui se touchent (coupure d'une analyse précédente) n'en font qu'une.
        match out.last_mut() {
            Some(last) if clip.start <= last.end + ChronoDuration::seconds(1) => last.end = last.end.max(clip.end),
            _ => out.push(clip),
        }
        i = j + 1;
    }
    out
}

/// Minutes de vidéo de chaque heure, d'après les séquences.
fn hours_from_clips(date: NaiveDate, clips: &[Clip], base: &[HourFootage]) -> Vec<HourFootage> {
    (0..24u32)
        .map(|h| {
            let mut f = base.get(h as usize).copied().unwrap_or(HourFootage { hour: h, ..Default::default() });
            if let Some((a, b)) = hour_window(date, h) {
                let secs: i64 = clips.iter().map(|c| (c.end.min(b) - c.start.max(a)).num_seconds().max(0)).sum();
                if f.answered || secs > 0 {
                    f.minutes = secs as f32 / 60.0;
                }
            }
            f
        })
        .collect()
}

/// Analyse détaillée d'une journée : heures (24 sondages), puis tranches de 5 min des heures où il
/// y a de la vidéo, puis minutes des tranches partielles, puis raccords entre minutes voisines.
/// Résultat à la seconde près en général, pour quelques dizaines à quelques centaines de sondages
/// rapides (~0,1 s chacun). `progress` reçoit les résultats intermédiaires (heures, puis séquences
/// approchées à 5 min). `prev` : analyse précédente du même jour, dont les heures révolues sont
/// reprises telles quelles (seule la fin de la journée en cours est réanalysée).
pub async fn scan_day_detail(cfg: &CameraConfig, date: NaiveDate, prev: Option<&DayFootage>, progress: &(dyn Fn(&DayFootage) + Send + Sync)) -> DayFootage {
    let now = Utc::now();
    let mut day = DayFootage { date, ..Default::default() };
    // Reprise : tout ce qui précède l'heure pendant laquelle l'analyse précédente a eu lieu.
    let keep_before = prev.filter(|p| p.date == date && p.complete).and_then(|p| {
        let at = p.scanned_at?.with_timezone(&Local);
        if at.date_naive() != date {
            return day_window(date).map(|(_, e)| e); // analysée après coup : journée entière acquise
        }
        hour_window(date, chrono::Timelike::hour(&at)).map(|(a, _)| a)
    });
    let kept: Vec<Clip> = match (prev, keep_before) {
        (Some(p), Some(cut)) => p.clips.iter().filter(|c| c.start < cut).map(|c| Clip { start: c.start, end: c.end.min(cut) }).collect(),
        _ => Vec::new(),
    };

    // 1. Les heures.
    let windows: Vec<Option<(DateTime<Utc>, DateTime<Utc>)>> = (0..24u32)
        .map(|h| hour_window(date, h).filter(|(a, _)| *a < now && keep_before.is_none_or(|k| *a >= k)).map(|(a, b)| (a, b.min(now))))
        .collect();
    let asked: Vec<(DateTime<Utc>, DateTime<Utc>)> = windows.iter().flatten().copied().collect();
    let mut answers = probe_all(cfg, &asked).await.into_iter();
    let mut busy_hours = Vec::new();
    day.hours = (0..24u32)
        .map(|h| match windows[h as usize] {
            None => {
                let w = hour_window(date, h);
                let future = w.is_some_and(|(a, _)| a >= now);
                let reused = prev.and_then(|p| p.hours.get(h as usize).copied()).filter(|_| !future && keep_before.is_some());
                reused.unwrap_or(HourFootage { hour: h, future, ..Default::default() })
            }
            Some(w) => match answers.next().flatten() {
                Some(v) => {
                    if v > 0.0 {
                        busy_hours.push(w);
                    }
                    HourFootage { hour: h, minutes: (v / 60.0) as f32, future: false, answered: true }
                }
                None => {
                    day.failed += 1;
                    HourFootage { hour: h, ..Default::default() }
                }
            },
        })
        .collect();
    day.clips = kept.clone();
    day.progress = 0.25;
    progress(&day);

    // 2. Tranches de 5 min dans les heures où il y a de la vidéo.
    let coarse_w: Vec<(DateTime<Utc>, DateTime<Utc>)> = busy_hours.iter().flat_map(|&(a, b)| split(a, b, 300)).collect();
    let mut coarse: Vec<Slot> = Vec::new();
    for (&(a, b), r) in coarse_w.iter().zip(probe_all(cfg, &coarse_w).await) {
        match r {
            Some(v) if v > 0.0 => coarse.push((a, (b - a).num_seconds(), v)),
            Some(_) => {}
            None => day.failed += 1,
        }
    }
    // Séquences approchées (à 5 min près) en attendant le détail.
    let mut approx = kept.clone();
    approx.extend(build_clips(&coarse, &vec![false; coarse.len()], &HashMap::new()));
    approx.sort_by_key(|c| c.start);
    day.clips = approx;
    day.progress = 0.5;
    progress(&day);

    // 3. Minutes des tranches. Une tranche « pleine » peut cacher deux séquences, une à chaque
    //    bout (l'étendue va de la première image à la dernière) : on la détaille aussi, sauf si
    //    toute son heure est pleine (enregistrement continu : inutile de sonder 60 minutes).
    let continuous = |s: &Slot| {
        busy_hours.iter().any(|&(a, b)| {
            let inside: Vec<&Slot> = coarse.iter().filter(|c| c.0 >= a && c.0 < b).collect();
            s.0 >= a && s.0 < b && inside.len() == split(a, b, 300).len() && inside.iter().all(|c| slot_full(c))
        })
    };
    let mut fine: Vec<Slot> = coarse.iter().filter(|s| continuous(s)).copied().collect();
    let fine_w: Vec<(DateTime<Utc>, DateTime<Utc>)> = coarse.iter().filter(|s| !continuous(s)).flat_map(|s| split(s.0, slot_end(s), 60)).collect();
    for (&(a, b), r) in fine_w.iter().zip(probe_all(cfg, &fine_w).await) {
        match r {
            Some(v) if v > 0.0 => fine.push((a, (b - a).num_seconds(), v)),
            Some(_) => {}
            None => day.failed += 1,
        }
    }
    fine.sort_by_key(|s| s.0);
    day.progress = 0.8;

    // 4. Raccords : deux tranches voisines sont-elles une seule séquence ? Oui si l'étendue des
    //    deux réunies vaut la somme des deux (aucun trou entre elles).
    let pairs: Vec<usize> = (0..fine.len().saturating_sub(1)).filter(|&i| slot_end(&fine[i]) == fine[i + 1].0 && !(slot_full(&fine[i]) && slot_full(&fine[i + 1]))).collect();
    let pair_w: Vec<(DateTime<Utc>, DateTime<Utc>)> = pairs.iter().map(|&i| (fine[i].0, slot_end(&fine[i + 1]))).collect();
    let mut joined = vec![false; fine.len()];
    for i in 0..fine.len().saturating_sub(1) {
        joined[i] = slot_end(&fine[i]) == fine[i + 1].0 && slot_full(&fine[i]) && slot_full(&fine[i + 1]);
    }
    for (&i, r) in pairs.iter().zip(probe_all(cfg, &pair_w).await) {
        match r {
            Some(v) => joined[i] = (v - (fine[i].2 + fine[i + 1].2)).abs() <= 2.0,
            None => day.failed += 1,
        }
    }
    // 5. Tranches isolées remplies à plus de moitié : un sondage qui démarre forcément pendant la
    //    vidéo donne sa fin exacte (étendue = fin − début du sondage), donc aussi son début.
    let lone: Vec<usize> = (0..fine.len())
        .filter(|&i| {
            let alone = (i == 0 || !joined[i - 1]) && !joined[i];
            alone && !slot_full(&fine[i]) && fine[i].2 * 2.0 > fine[i].1 as f64
        })
        .collect();
    let lone_w: Vec<(DateTime<Utc>, DateTime<Utc>)> =
        lone.iter().map(|&i| (fine[i].0 + ChronoDuration::seconds((fine[i].1 as f64 - fine[i].2).ceil() as i64), slot_end(&fine[i]))).collect();
    let mut exact = HashMap::new();
    for ((&i, &(x, _)), r) in lone.iter().zip(&lone_w).zip(probe_all(cfg, &lone_w).await) {
        if let Some(f) = r.filter(|f| *f > 0.0) {
            let end = x + ChronoDuration::seconds(f.round() as i64);
            exact.insert(fine[i].0, Clip { start: end - ChronoDuration::seconds(fine[i].2.round() as i64), end });
        }
    }

    let mut clips = kept;
    clips.extend(build_clips(&fine, &joined, &exact));
    clips.sort_by_key(|c| c.start);
    // Fusion finale (séquence coupée à la reprise d'une analyse précédente).
    let mut merged: Vec<Clip> = Vec::with_capacity(clips.len());
    for c in clips {
        match merged.last_mut() {
            Some(last) if c.start <= last.end + ChronoDuration::seconds(1) => last.end = last.end.max(c.end),
            _ => merged.push(c),
        }
    }
    day.hours = hours_from_clips(date, &merged, &day.hours);
    day.clips = merged;
    day.progress = 1.0;
    day.complete = true;
    day.scanned_at = Some(now);
    progress(&day);
    day
}

/// Y a-t-il de la vidéo sur chacune de ces journées ? Un seul sondage par jour.
/// `None` : la caméra n'a pas répondu ; les jours à venir valent `Some(false)`.
pub async fn scan_days(cfg: &CameraConfig, dates: &[NaiveDate]) -> Vec<(NaiveDate, Option<bool>)> {
    let now = Utc::now();
    let windows: Vec<Option<(DateTime<Utc>, DateTime<Utc>)>> = dates.iter().map(|d| day_window(*d).filter(|(a, _)| *a < now).map(|(a, b)| (a, b.min(now)))).collect();
    let asked: Vec<(DateTime<Utc>, DateTime<Utc>)> = windows.iter().flatten().copied().collect();
    let mut answers = probe_all(cfg, &asked).await.into_iter();
    dates
        .iter()
        .zip(&windows)
        .map(|(d, w)| match w {
            None => (*d, Some(false)),
            Some(_) => (*d, answers.next().flatten().map(|v| v > 0.0)),
        })
        .collect()
}

// ---- Relecture (décodage) et export (FFmpeg) ----------------------------------------------

/// `pts_time:12.34` d'une ligne `showinfo` de FFmpeg.
fn parse_pts_time(line: &str) -> Option<f64> {
    let i = line.find("pts_time:")? + "pts_time:".len();
    line[i..].split_whitespace().next()?.parse().ok()
}

/// Commande FFmpeg sans fenêtre console.
fn ffmpeg_command(ffmpeg: &Path) -> Command {
    let mut cmd = Command::new(ffmpeg);
    cmd.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // pas de fenêtre console
    }
    cmd
}

/// Fréquence du son relu (celle du micro des Imou ; FFmpeg rééchantillonne au besoin).
pub const AUDIO_RATE: u32 = 16_000;

/// Avance du son sur l'image : le temps que les échantillons passent dans la file et la carte son.
const AUDIO_LEAD: f64 = 0.1;

/// Son de la séquence relue (`ReplayOptions::sound`) : pas encore connu, présent, absent.
pub const SOUND_UNKNOWN: u8 = 0;
pub const SOUND_YES: u8 = 1;
pub const SOUND_NONE: u8 = 2;

/// Options de la relecture décodée.
#[derive(Clone, Default)]
pub struct ReplayOptions {
    /// Reçoit la position : secondes depuis la première image, trous sautés compris (voir
    /// l'en-tête du module).
    pub clock: Option<Clock>,
    /// Reçoit le son (mono, `AUDIO_RATE` Hz), calé sur l'image : chaque bloc part quand l'image
    /// du même instant arrive. Même connexion RTSP que l'image (la caméra en accepte peu).
    pub audio: Option<AudioSink>,
    /// La séquence a-t-elle du son ? (`SOUND_*`)
    pub sound: Option<Arc<AtomicU8>>,
}

/// `nb_samples:1024` d'une ligne `ashowinfo` de FFmpeg.
fn parse_nb_samples(line: &str) -> Option<usize> {
    let i = line.find("nb_samples:")? + "nb_samples:".len();
    line[i..].split_whitespace().next()?.parse().ok()
}

/// Décode la relecture d'une période en images RGBA (via FFmpeg lisant le RTSP de la carte SD).
///
/// Bloquant : à lancer dans un fil (`spawn_blocking`). Rend la main quand la lecture est terminée
/// (fin de la période), annulée (`cancel` passé à `true`), ou en cas d'erreur. Chaque image est
/// mise à l'échelle dans `width`×`height` (bandes noires, sans déformation). `on_open` est appelé
/// à la première image. `start`/`end` sont en UTC (format interne de la caméra). Position et son :
/// voir `ReplayOptions` ; une séquence sans piste son est relue sans (FFmpeg relancé une fois).
#[allow(clippy::too_many_arguments)]
pub fn decode_blocking(
    cfg: &CameraConfig,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    width: u32,
    height: u32,
    cancel: Arc<AtomicBool>,
    sink: FrameSink,
    on_open: impl Fn(),
    opts: ReplayOptions,
) -> Result<()> {
    let ffmpeg = ffmpeg::find().ok_or_else(|| anyhow!("FFmpeg est nécessaire pour relire les enregistrements (H.265) et reste introuvable"))?;
    let job = Decode { ffmpeg: &ffmpeg, cfg, start, end, width, height, cancel: &cancel, sink: &sink, on_open: &on_open, opts: &opts };
    if opts.audio.is_some() && job.run(true)? == Attempt::Done {
        return Ok(());
    }
    if opts.audio.is_some() {
        if let Some(s) = &opts.sound {
            s.store(SOUND_NONE, Ordering::Relaxed);
        }
    }
    job.run(false).map(|_| ())
}

#[derive(PartialEq)]
enum Attempt {
    Done,
    /// La séquence n'a pas de piste son : relancer sans.
    NoAudio,
}

struct Decode<'a> {
    ffmpeg: &'a Path,
    cfg: &'a CameraConfig,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    width: u32,
    height: u32,
    cancel: &'a Arc<AtomicBool>,
    sink: &'a FrameSink,
    on_open: &'a dyn Fn(),
    opts: &'a ReplayOptions,
}

impl Decode<'_> {
    fn run(&self, with_audio: bool) -> Result<Attempt> {
        let (width, height) = (self.width, self.height);
        let url = playback_url_with_creds(self.cfg, self.start, self.end);
        // Horodatages nécessaires pour l'horloge et pour caler le son.
        let timed = self.opts.clock.is_some() || with_audio;
        let mut vf = format!("scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2");
        if timed {
            // Horodatage de chaque image sur la sortie d'erreur (sans somme de contrôle : gratuit).
            vf.push_str(",showinfo=checksum=0");
        }
        // Le son sort par un port local (une seule sortie standard, déjà prise par l'image).
        let listener = if with_audio { Some(std::net::TcpListener::bind("127.0.0.1:0").context("port local pour le son")?) } else { None };
        let mut cmd = ffmpeg_command(self.ffmpeg);
        cmd.args(["-hide_banner", "-nostats", "-loglevel", if timed { "info" } else { "error" }, "-rtsp_transport", "tcp", "-timeout", RTSP_IDLE_US, "-i"])
            .arg(&url)
            .args(["-map", "0:v:0", "-vf"])
            .arg(&vf)
            // Cadence d'origine, sans images dupliquées : aux trous, l'horodatage saute ; forcer une
            // cadence fixe ferait recopier des milliers d'images pour combler chaque trou.
            .args(["-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"]);
        if let Some(l) = &listener {
            let port = l.local_addr()?.port();
            // Mono 16 bits, horodaté bloc par bloc (`ashowinfo`), envoyé sans attendre (`flush_packets`).
            cmd.args(["-map", "0:a:0?", "-af"])
                .arg(format!("aresample={AUDIO_RATE},aformat=sample_fmts=s16:channel_layouts=mono,ashowinfo"))
                .args(["-c:a", "pcm_s16le", "-f", "s16le", "-flush_packets", "1"])
                .arg(format!("tcp://127.0.0.1:{port}?tcp_nodelay=1"));
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(if timed { Stdio::piped() } else { Stdio::null() });
        let mut child = cmd.spawn().context("lancement de FFmpeg")?;
        let mut stdout = child.stdout.take().ok_or_else(|| anyhow!("FFmpeg : sortie indisponible"))?;

        // Sortie d'erreur : horodatages des images (dans leur ordre : la n-ième ligne `showinfo` est
        // celle de la n-ième image), blocs de son (horodatage, taille), et dernières autres lignes.
        let stamps: Arc<Mutex<std::collections::VecDeque<f64>>> = Arc::default();
        let blocks: Arc<Mutex<std::collections::VecDeque<(f64, usize)>>> = Arc::default();
        let tail: Arc<Mutex<Vec<String>>> = Arc::default();
        let reader = child.stderr.take().map(|err| {
            let (stamps, blocks, tail) = (stamps.clone(), blocks.clone(), tail.clone());
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                    if line.contains("ashowinfo") {
                        if let (Some(t), Some(n)) = (parse_pts_time(&line), parse_nb_samples(&line)) {
                            blocks.lock().unwrap().push_back((t, n));
                        }
                    } else if let Some(t) = parse_pts_time(&line) {
                        stamps.lock().unwrap().push_back(t);
                    } else {
                        let mut tail = tail.lock().unwrap();
                        tail.push(line);
                        if tail.len() > 12 {
                            tail.remove(0);
                        }
                    }
                }
            })
        });
        let child = Arc::new(Mutex::new(child));
        let t0 = Instant::now();
        // Dernière image : moment (ms depuis le lancement ; 0 = aucune) et horodatage brut.
        let last_frame = Arc::new(AtomicU64::new(0));
        let video_pts = Arc::new(AtomicU64::new(f64::NAN.to_bits()));
        let done = Arc::new(AtomicBool::new(false));

        // Fil veilleur : tue FFmpeg dès qu'on demande l'arrêt ; quand plus rien n'arrive après la
        // fin de la période (secours, FFmpeg finit normalement de lui-même), lui demande de
        // s'arrêter proprement, puis force au bout de 3 s ; se termine quand FFmpeg a fini.
        let mut stdin = child.lock().unwrap().stdin.take();
        let watcher = {
            let (child, cancel, done, last_frame) = (child.clone(), self.cancel.clone(), done.clone(), last_frame.clone());
            std::thread::spawn(move || {
                let mut asked: Option<Instant> = None;
                loop {
                    if cancel.load(Ordering::Relaxed) || done.load(Ordering::Relaxed) || asked.is_some_and(|a| a.elapsed() > Duration::from_secs(3)) {
                        let _ = child.lock().unwrap().kill();
                        return;
                    }
                    if matches!(child.lock().unwrap().try_wait(), Ok(Some(_))) {
                        return;
                    }
                    let last = last_frame.load(Ordering::Relaxed);
                    if asked.is_none() && last > 0 && t0.elapsed().as_millis() as u64 > last + END_IDLE.as_millis() as u64 {
                        if let Some(mut i) = stdin.take() {
                            use std::io::Write;
                            let _ = i.write_all(b"q\n");
                        }
                        asked = Some(Instant::now());
                    }
                    std::thread::sleep(Duration::from_millis(120));
                }
            })
        };
        let pump = match (listener, &self.opts.audio) {
            (Some(l), Some(out)) => {
                let a = AudioPump {
                    out: out.clone(),
                    blocks: blocks.clone(),
                    video_pts: video_pts.clone(),
                    last_frame: last_frame.clone(),
                    cancel: self.cancel.clone(),
                    done: done.clone(),
                    sound: self.opts.sound.clone(),
                    t0,
                };
                Some(std::thread::spawn(move || a.run(l)))
            }
            _ => None,
        };

        let frame_len = width as usize * height as usize * 4;
        let mut buf = vec![0u8; frame_len];
        let mut seq = 0u64;
        let mut first: Option<f64> = None;
        use std::io::Read;
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                break;
            }
            if stdout.read_exact(&mut buf).is_err() {
                break; // fin du flux = lecture terminée
            }
            if seq == 0 {
                (self.on_open)();
            }
            if timed {
                // La ligne de l'image précède normalement l'image elle-même ; on l'attend un instant sinon.
                let waited = Instant::now();
                loop {
                    if let Some(t) = stamps.lock().unwrap().pop_front() {
                        let p0 = *first.get_or_insert(t);
                        if let Some(clock) = &self.opts.clock {
                            clock.store((t - p0).max(0.0).to_bits(), Ordering::Relaxed);
                        }
                        video_pts.store(t.to_bits(), Ordering::Relaxed);
                        break;
                    }
                    if waited.elapsed() > Duration::from_millis(40) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            last_frame.store((t0.elapsed().as_millis() as u64).max(1), Ordering::Relaxed);
            seq += 1;
            (self.sink)(Frame { seq, width: width as usize, height: height as usize, rgba: buf.clone() });
        }
        done.store(true, Ordering::Relaxed);
        let _ = watcher.join();
        let _ = child.lock().unwrap().wait();
        if let Some(r) = reader {
            let _ = r.join();
        }
        if let Some(p) = pump {
            let _ = p.join();
        }
        // Sans piste son, FFmpeg refuse la sortie son avant la première image.
        let no_audio = with_audio && seq == 0 && tail.lock().unwrap().iter().any(|l| l.contains("does not contain any stream"));
        Ok(if no_audio { Attempt::NoAudio } else { Attempt::Done })
    }
}

/// Fil du son relu : reçoit les échantillons de FFmpeg (port local), les découpe en blocs
/// horodatés, et rend chaque bloc quand l'image du même instant s'affiche.
struct AudioPump {
    out: AudioSink,
    blocks: Arc<Mutex<std::collections::VecDeque<(f64, usize)>>>,
    video_pts: Arc<AtomicU64>,
    last_frame: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    sound: Option<Arc<AtomicU8>>,
    t0: Instant,
}

impl AudioPump {
    fn run(self, listener: std::net::TcpListener) {
        use std::io::Read;
        // FFmpeg se connecte au démarrage ; s'il échoue avant (pas de son), on abandonne.
        let _ = listener.set_nonblocking(true);
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut conn = loop {
            match listener.accept() {
                Ok((c, _)) => break c,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if self.cancel.load(Ordering::Relaxed) || self.done.load(Ordering::Relaxed) || Instant::now() > deadline {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => return,
            }
        };
        let _ = conn.set_nonblocking(false);
        let _ = conn.set_read_timeout(Some(Duration::from_millis(20)));
        let mut bytes: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 8192];
        let mut pending: std::collections::VecDeque<(f64, Vec<f32>)> = std::collections::VecDeque::new();
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return; // pause ou arrêt : silence tout de suite
            }
            let eof = match conn.read(&mut tmp) {
                Ok(0) => true,
                Ok(n) => {
                    bytes.extend_from_slice(&tmp[..n]);
                    false
                }
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => false,
                Err(_) => true,
            };
            // Découpe en blocs d'après leurs horodatages (les octets peuvent précéder la ligne).
            loop {
                let head = self.blocks.lock().unwrap().front().copied();
                match head {
                    Some((pts, n)) if bytes.len() >= n * 2 => {
                        self.blocks.lock().unwrap().pop_front();
                        let s: Vec<f32> = bytes.drain(..n * 2).collect::<Vec<u8>>().chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect();
                        if let Some(flag) = &self.sound {
                            flag.store(SOUND_YES, Ordering::Relaxed);
                        }
                        pending.push_back((pts, s));
                    }
                    _ => break,
                }
            }
            // Instant affiché : horodatage de la dernière image, prolongé du temps écoulé depuis
            // (borné : si l'image s'arrête, le son l'attend).
            let v = f64::from_bits(self.video_pts.load(Ordering::Relaxed));
            if v.is_finite() {
                let since = (self.t0.elapsed().as_millis() as u64).saturating_sub(self.last_frame.load(Ordering::Relaxed)) as f64 / 1000.0;
                let now = v + since.min(0.25);
                while pending.front().is_some_and(|(pts, _)| *pts <= now + AUDIO_LEAD) {
                    if let Some((_, s)) = pending.pop_front() {
                        (self.out)(&s, AUDIO_RATE);
                    }
                }
            }
            // Jamais plus de 6 s en attente (FFmpeg retient ~2 s au démarrage, le temps d'analyser
            // le flux, puis lâche tout d'un coup) : au-delà, les blocs les plus anciens sont jetés.
            let mut waiting: usize = pending.iter().map(|(_, s)| s.len()).sum();
            while waiting > 6 * AUDIO_RATE as usize {
                if let Some((_, s)) = pending.pop_front() {
                    waiting -= s.len();
                }
            }
            if eof {
                // Fin de la période : ce qui reste accompagne les dernières images.
                for (_, s) in pending.drain(..) {
                    (self.out)(&s, AUDIO_RATE);
                }
                return;
            }
        }
    }
}

/// Exporte la période dans un fichier `.mp4` (image copiée telle quelle, sans ré-encodage). Bloquant.
///
/// La caméra diffuse la relecture à vitesse réelle (les trous, eux, sont sautés) : l'export dure
/// à peu près la durée de vidéo de la période — on vise des séquences, pas des heures. `start`/`end`
/// en UTC. `progress` reçoit les secondes déjà copiées ; `cancel` interrompt (fichier supprimé).
/// Le son est copié lui aussi (AAC chez Imou) ; un son que le .mp4 refuse (G.711 de certaines
/// caméras) est converti en AAC.
pub fn export_blocking(cfg: &CameraConfig, start: DateTime<Utc>, end: DateTime<Utc>, out: &Path, cancel: Arc<AtomicBool>, progress: &dyn Fn(f64)) -> Result<()> {
    let ffmpeg = ffmpeg::find().ok_or_else(|| anyhow!("FFmpeg est nécessaire pour exporter et reste introuvable"))?;
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("création de {}", dir.display()))?;
    }
    let url = playback_url_with_creds(cfg, start, end);
    let max_secs = (end - start).num_seconds().max(1);
    let mut refused = false;
    match export_once(&ffmpeg, &url, max_secs, out, &cancel, progress, false, &mut refused) {
        // Refusé dès l'en-tête du fichier, avant de lire la vidéo : on recommence aussitôt.
        Err(_) if refused => export_once(&ffmpeg, &url, max_secs, out, &cancel, progress, true, &mut refused),
        r => r,
    }
}

/// Un essai d'export. `aac` : convertit le son en AAC au lieu de le copier. `refused` passe à
/// vrai si le conteneur .mp4 a refusé le codec du son.
#[allow(clippy::too_many_arguments)]
fn export_once(ffmpeg: &Path, url: &str, max_secs: i64, out: &Path, cancel: &Arc<AtomicBool>, progress: &dyn Fn(f64), aac: bool, refused: &mut bool) -> Result<()> {
    let codecs: &[&str] = if aac { &["-c:v", "copy", "-c:a", "aac", "-b:a", "64k"] } else { &["-c", "copy"] };
    let mut cmd = ffmpeg_command(ffmpeg);
    // Le fichier commence à la première image : sa durée ne peut pas dépasser la période (`-t`).
    cmd.args(["-hide_banner", "-nostats", "-loglevel", "error", "-rtsp_transport", "tcp", "-timeout", RTSP_IDLE_US, "-i"])
        .arg(url)
        .args(["-t", &max_secs.to_string()])
        .args(codecs)
        .args(["-progress", "pipe:1", "-y"])
        .arg(out)
        // Entrée ouverte : « q » arrête FFmpeg proprement (fichier .mp4 finalisé, donc lisible).
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("lancement de FFmpeg")?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("FFmpeg : sortie indisponible"))?;
    let mut stdin = child.stdin.take();
    let stderr = child.stderr.take();
    let errors = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut e) = stderr {
            use std::io::Read;
            let _ = e.read_to_string(&mut s);
        }
        s
    });
    let child = Arc::new(Mutex::new(child));
    // Dernière avancée de la copie (ms depuis le lancement ; 0 = rien encore).
    let t0 = Instant::now();
    let moved = Arc::new(AtomicU64::new(0));
    // Fil veilleur : tue FFmpeg dès qu'on annule ; quand plus rien n'arrive (fin de la période, la
    // caméra ne ferme pas la session), demande l'arrêt propre, puis force au bout de 8 s.
    let watcher = {
        let (child, cancel, moved) = (child.clone(), cancel.clone(), moved.clone());
        std::thread::spawn(move || {
            let mut asked: Option<Instant> = None;
            loop {
                if cancel.load(Ordering::Relaxed) || asked.is_some_and(|a| a.elapsed() > Duration::from_secs(8)) {
                    let _ = child.lock().unwrap().kill();
                    return;
                }
                if matches!(child.lock().unwrap().try_wait(), Ok(Some(_))) {
                    return;
                }
                let last = moved.load(Ordering::Relaxed);
                if asked.is_none() && last > 0 && t0.elapsed().as_millis() as u64 > last + END_IDLE.as_millis() as u64 {
                    if let Some(mut i) = stdin.take() {
                        use std::io::Write;
                        let _ = i.write_all(b"q\n");
                    }
                    asked = Some(Instant::now());
                }
                std::thread::sleep(Duration::from_millis(150));
            }
        })
    };
    {
        use std::io::BufRead;
        let mut seen = -1i64;
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(us) = line.strip_prefix("out_time_us=").and_then(|v| v.trim().parse::<i64>().ok()) {
                // Seule une vraie avancée compte (pas le 0 du démarrage, avant la première image).
                if us > seen && us > 0 {
                    seen = us;
                    moved.store((t0.elapsed().as_millis() as u64).max(1), Ordering::Relaxed);
                }
                progress(us.max(0) as f64 / 1e6);
            }
        }
    }
    let _ = watcher.join();
    let _ = child.lock().unwrap().wait();
    let err = errors.join().unwrap_or_default();
    if cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(out);
        bail!("export annulé");
    }
    *refused = err.contains("Could not find tag for codec") || err.contains("not currently supported in container");
    let size = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    if size < 1024 {
        let _ = std::fs::remove_file(out);
        let detail = err.lines().filter(|l| !l.trim().is_empty()).next_back().unwrap_or("FFmpeg a échoué");
        bail!("export impossible ({detail})");
    }
    Ok(())
}

/// Vignette d'une séquence : la première image relue à partir de `at`, en `width`×`height` RGBA.
/// Bloquant (une connexion RTSP, une image H.265 décodée) ; abandonne au bout de 20 s.
pub fn thumbnail_blocking(cfg: &CameraConfig, at: DateTime<Utc>, width: u32, height: u32) -> Result<Frame> {
    let ffmpeg = ffmpeg::find().ok_or_else(|| anyhow!("FFmpeg introuvable"))?;
    let url = playback_url_with_creds(cfg, at, at + ChronoDuration::seconds(120));
    let vf = format!("scale={width}:{height}:force_original_aspect_ratio=increase,crop={width}:{height}");
    let mut cmd = ffmpeg_command(&ffmpeg);
    cmd.args(["-hide_banner", "-nostats", "-loglevel", "error", "-rtsp_transport", "tcp", "-timeout", RTSP_IDLE_US, "-i"])
        .arg(&url)
        .args(["-an", "-frames:v", "1", "-vf"])
        .arg(&vf)
        .args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().context("lancement de FFmpeg")?;
    let mut stdout = child.stdout.take().ok_or_else(|| anyhow!("FFmpeg : sortie indisponible"))?;
    let child = Arc::new(Mutex::new(child));
    let deadline = Instant::now() + Duration::from_secs(20);
    let done = Arc::new(AtomicBool::new(false));
    let watcher = {
        let (child, done) = (child.clone(), done.clone());
        std::thread::spawn(move || loop {
            if done.load(Ordering::Relaxed) || Instant::now() > deadline {
                let _ = child.lock().unwrap().kill();
                return;
            }
            if matches!(child.lock().unwrap().try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        })
    };
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    let r = {
        use std::io::Read;
        stdout.read_exact(&mut rgba)
    };
    done.store(true, Ordering::Relaxed);
    let _ = watcher.join();
    let _ = child.lock().unwrap().wait();
    r.map_err(|_| anyhow!("aucune image"))?;
    Ok(Frame { seq: 1, width: width as usize, height: height as usize, rgba })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 20, h, m, 0).unwrap()
    }

    #[test]
    fn url_uses_utc_and_creds() {
        let mut c = CameraConfig::new("192.168.1.164");
        assert_eq!(
            playback_url(&c, utc(14, 5), utc(14, 12)),
            "rtsp://192.168.1.164:554/cam/playback?channel=1&subtype=0&starttime=2026_10_20_14_05_00&endtime=2026_10_20_14_12_00"
        );
        c.username = "admin".into();
        c.password = "a b@c".into();
        let u = playback_url_with_creds(&c, utc(14, 5), utc(14, 12));
        assert!(u.starts_with("rtsp://admin:a%20b%40c@192.168.1.164:554/cam/playback?"), "{u}");
    }

    #[test]
    fn names_use_local_wall_clock() {
        let s = Local.with_ymd_and_hms(2026, 10, 20, 14, 5, 0).unwrap();
        let e = Local.with_ymd_and_hms(2026, 10, 20, 14, 12, 0).unwrap();
        assert_eq!(export_name("Chambre", s, e), "Chambre 2026-10-20 14h05-14h12.mp4");
        assert_eq!(export_name("Cam/Salon", s, e), "Cam_Salon 2026-10-20 14h05-14h12.mp4");
    }

    #[test]
    fn npt_parsing() {
        assert_eq!(parse_npt("a=control:*\r\na=range:npt=0-1755.000000\r\nm=video"), Some(1755.0));
        assert_eq!(parse_npt("a=range:npt=0-0.000000\r\n"), Some(0.0));
        assert_eq!(parse_npt("a=range:npt=now-\r\n"), None);
        assert_eq!(parse_npt("pas de range"), None);
    }

    #[test]
    fn clips_from_slots() {
        // Relevé réel (20 oct.) : 20:18 → 8 s, 20:19 → 39 s, jointes = une séquence 20:18:52 → 20:19:39.
        let slots = vec![(utc(20, 18), 60, 8.0), (utc(20, 19), 60, 39.0), (utc(20, 25), 60, 42.0), (utc(20, 41), 60, 20.0)];
        let mut exact = HashMap::new();
        let e = utc(20, 25) + ChronoDuration::seconds(42);
        exact.insert(utc(20, 25), Clip { start: utc(20, 25), end: e });
        let clips = build_clips(&slots, &[true, false, false, false], &exact);
        assert_eq!(clips.len(), 3);
        assert_eq!((clips[0].start, clips[0].end), (utc(20, 18) + ChronoDuration::seconds(52), utc(20, 19) + ChronoDuration::seconds(39)));
        assert_eq!(clips[1], Clip { start: utc(20, 25), end: e });
        // Tranche isolée sans mesure exacte : centrée.
        assert_eq!(clips[2].start, utc(20, 41) + ChronoDuration::seconds(20));
        assert_eq!(clips[2].secs(), 20);
        // Deux séquences qui se touchent n'en font qu'une.
        let touching = build_clips(&[(utc(9, 0), 300, 300.0), (utc(9, 5), 60, 60.0)], &[false, false], &HashMap::new());
        assert_eq!(touching, vec![Clip { start: utc(9, 0), end: utc(9, 6) }]);
    }

    #[test]
    fn pts_from_showinfo() {
        let l = "[Parsed_showinfo_1 @ 000001c52423f200] n:  12 pts:  108000 pts_time:361.2   duration:3600";
        assert_eq!(parse_pts_time(l), Some(361.2));
        assert_eq!(parse_pts_time("Stream #0:0: Video: h264"), None);
    }

    /// Caméra simulée prête à relire sa carte (mêmes réponses que la vraie).
    async fn sim_camera() -> (CameraConfig, u32) {
        let cam = crate::camera::sim::start("L2A3B4C5", 3).await.unwrap();
        let mut c = CameraConfig::new("127.0.0.1");
        (c.rtsp_port, c.onvif_port, c.password) = (cam.rtsp_port, cam.onvif_port, "L2A3B4C5".into());
        (c, 3)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn scan_finds_the_recorded_clips() {
        let (cfg, scene) = sim_camera().await;
        let date = Local::now().date_naive().pred_opt().unwrap();
        let truth = crate::camera::sim::sim_clips(date, scene);
        assert!(truth.len() > 10, "jeu d'essai trop maigre");
        let steps = Mutex::new(Vec::new());
        let day = scan_day_detail(&cfg, date, None, &|d| steps.lock().unwrap().push(d.progress)).await;
        assert!(day.complete && day.failed == 0);
        assert_eq!(steps.into_inner().unwrap(), vec![0.25, 0.5, 1.0]);
        let found: Vec<String> = day.clips.iter().map(|c| format!("{}→{}", c.start.format("%H:%M:%S"), c.end.format("%H:%M:%S"))).collect();
        let want: Vec<String> = truth.iter().map(|(s, e)| format!("{}→{}", s.format("%H:%M:%S"), e.format("%H:%M:%S"))).collect();
        assert_eq!(found.len(), want.len(), "trouvées {found:?}\nattendues {want:?}");
        let mut exact = 0;
        for (c, (s, e)) in day.clips.iter().zip(&truth) {
            let (ds, de) = ((c.start - *s).num_seconds().abs(), (c.end - *e).num_seconds().abs());
            assert!(ds <= 30 && de <= 30, "{c:?} au lieu de {s} → {e}");
            exact += (ds <= 1 && de <= 1) as usize;
        }
        // Presque toutes à la seconde près (seules les très courtes tranches isolées sont estimées).
        assert!(exact * 10 >= truth.len() * 8, "{exact} exactes sur {}", truth.len());
        let total: i64 = truth.iter().map(|(s, e)| (*e - *s).num_seconds()).sum();
        assert!((day.total_secs() - total).abs() <= truth.len() as i64 * 2);

        // Réanalyse de la journée : tout est repris de l'analyse précédente, sans rien perdre.
        let again = scan_day_detail(&cfg, date, Some(&day), &|_| {}).await;
        assert_eq!(again.clips, day.clips);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn replay_skips_gaps_and_keeps_the_clock() {
        if ffmpeg::find().is_none() {
            eprintln!("FFmpeg absent : test sauté");
            return;
        }
        let (cfg, scene) = sim_camera().await;
        let date = Local::now().date_naive().pred_opt().unwrap();
        let truth = crate::camera::sim::sim_clips(date, scene);
        // Deux secondes de fin d'une séquence, le trou (d'au moins une minute), deux secondes de la suivante.
        let i = truth.windows(2).position(|w| (w[1].0 - w[0].1).num_seconds() >= 60).expect("un trou d'une minute");
        let (a, b) = (truth[i], truth[i + 1]);
        let (start, end) = (a.1 - ChronoDuration::seconds(2), b.0 + ChronoDuration::seconds(2));
        let gap = (b.0 - a.1).num_seconds() as f64;
        let clock = new_clock();
        let frames = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let sink: FrameSink = {
            let frames = frames.clone();
            Arc::new(move |_f| {
                frames.fetch_add(1, Ordering::Relaxed);
            })
        };
        let t0 = Instant::now();
        let c2 = cfg.clone();
        let cl = clock.clone();
        let opts = ReplayOptions { clock: Some(cl), ..ReplayOptions::default() };
        tokio::task::spawn_blocking(move || decode_blocking(&c2, start, end, 320, 180, Arc::new(AtomicBool::new(false)), sink, || {}, opts))
            .await
            .unwrap()
            .unwrap();
        let took = t0.elapsed().as_secs_f64();
        let pos = read_clock(&clock).expect("horloge");
        assert!(frames.load(Ordering::Relaxed) > 20);
        // La position a sauté le trou (≈ 2 s + trou + 2 s), sans l'attendre en temps réel.
        assert!(pos > gap + 2.5 && pos < gap + 4.5, "position {pos} pour un trou de {gap} s");
        assert!(took < gap, "{took:.1} s pour relire 4 s de vidéo");
    }

    /// Relit 4 s au milieu d'une séquence d'hier sur une caméra simulée (avec ou sans son).
    /// Renvoie : images, secondes de son reçues, état du son, et la plus grande avance du son
    /// sur l'image observée à chaque bloc rendu (secondes).
    async fn replay_with_sound(with_sound: bool) -> (u64, f64, u8, f64) {
        let cam = crate::camera::sim::start("L2A3B4C5", 5).await.unwrap();
        cam.state.replay_sound.store(with_sound, Ordering::Relaxed);
        let mut cfg = CameraConfig::new("127.0.0.1");
        (cfg.rtsp_port, cfg.onvif_port, cfg.password) = (cam.rtsp_port, cam.onvif_port, "L2A3B4C5".into());
        let date = Local::now().date_naive().pred_opt().unwrap();
        let clip = crate::camera::sim::sim_clips(date, 5)[1];
        let (start, end) = (clip.0 + ChronoDuration::seconds(2), clip.0 + ChronoDuration::seconds(6));
        let frames = Arc::new(AtomicU64::new(0));
        let sink: FrameSink = {
            let frames = frames.clone();
            Arc::new(move |_f| {
                frames.fetch_add(1, Ordering::Relaxed);
            })
        };
        let clock = new_clock();
        // (secondes de son rendues, plus grande avance du son sur l'image)
        let heard = Arc::new(Mutex::new((0.0f64, 0.0f64)));
        let audio: AudioSink = {
            let (heard, clock) = (heard.clone(), clock.clone());
            Arc::new(move |s: &[f32], rate: u32| {
                let mut h = heard.lock().unwrap();
                h.0 += s.len() as f64 / rate as f64;
                let lead = h.0 - read_clock(&clock).unwrap_or(0.0);
                h.1 = h.1.max(lead);
            })
        };
        let sound = Arc::new(AtomicU8::new(SOUND_UNKNOWN));
        let opts = ReplayOptions { clock: Some(clock.clone()), audio: Some(audio), sound: Some(sound.clone()) };
        tokio::task::spawn_blocking(move || decode_blocking(&cfg, start, end, 320, 180, Arc::new(AtomicBool::new(false)), sink, || {}, opts))
            .await
            .unwrap()
            .unwrap();
        let (secs, lead) = *heard.lock().unwrap();
        (frames.load(Ordering::Relaxed), secs, sound.load(Ordering::Relaxed), lead)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn replay_plays_the_sound_with_the_image() {
        if ffmpeg::find().is_none() {
            eprintln!("FFmpeg absent : test sauté");
            return;
        }
        let (frames, secs, sound, lead) = replay_with_sound(true).await;
        assert!(frames > 20, "{frames} images");
        assert_eq!(sound, SOUND_YES);
        assert!((3.0..=5.0).contains(&secs), "{secs:.2} s de son pour 4 s de vidéo");
        // Le son suit l'image : jamais plus d'un bloc (64 ms) + l'avance voulue + une image d'écart.
        assert!(lead < AUDIO_LEAD + 0.064 + 0.12, "son en avance de {lead:.2} s");

        // Séquence muette : relue quand même (FFmpeg relancé sans le son), signalée sans son.
        let (frames, secs, sound, _) = replay_with_sound(false).await;
        assert!(frames > 20, "{frames} images");
        assert_eq!((sound, secs), (SOUND_NONE, 0.0));
    }

    #[test]
    fn audio_block_lines() {
        let l = "[Parsed_ashowinfo_2 @ 0000012af8808780] n:1 pts:1024 pts_time:0.064 fmt:s16 channels:1 chlayout:mono rate:16000 nb_samples:1024 checksum:8ABDF1DC";
        assert_eq!((parse_pts_time(l), parse_nb_samples(l)), (Some(0.064), Some(1024)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn export_writes_the_clip() {
        if ffmpeg::find().is_none() {
            eprintln!("FFmpeg absent : test sauté");
            return;
        }
        let (cfg, scene) = sim_camera().await;
        let date = Local::now().date_naive().pred_opt().unwrap();
        let clip = crate::camera::sim::sim_clips(date, scene)[2];
        // Trois secondes au milieu d'une séquence : FFmpeg s'arrête de lui-même, fichier finalisé.
        let (start, end) = (clip.0 + ChronoDuration::seconds(5), clip.0 + ChronoDuration::seconds(8));
        let dir = std::env::temp_dir().join(format!("homesmart-export-test-{}", std::process::id()));
        let out = dir.join("extrait.mp4");
        let done = Arc::new(Mutex::new(0.0f64));
        let (c2, o2, d2) = (cfg.clone(), out.clone(), done.clone());
        tokio::task::spawn_blocking(move || export_blocking(&c2, start, end, &o2, Arc::new(AtomicBool::new(false)), &|s| *d2.lock().unwrap() = s))
            .await
            .unwrap()
            .unwrap();
        assert!(std::fs::metadata(&out).unwrap().len() > 1024);
        assert!(*done.lock().unwrap() >= 2.5, "progression {}", done.lock().unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn digest_rfc2069() {
        // Vérifie la forme du condensé sans qop (celui qu'emploie la caméra).
        let h = digest_header("admin", "pass", "DESCRIBE", "rtsp://x/cam/playback", "Login to X", "ABCD", None);
        let ha1 = md5_hex("admin:Login to X:pass");
        let ha2 = md5_hex("DESCRIBE:rtsp://x/cam/playback");
        let expected = md5_hex(&format!("{ha1}:ABCD:{ha2}"));
        assert!(h.contains(&format!("response=\"{expected}\"")), "{h}");
        assert!(!h.contains("qop"));
    }
}
