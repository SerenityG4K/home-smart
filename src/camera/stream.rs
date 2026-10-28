//! Lecture d'un flux RTSP et décodage des images.
//!
//! Réseau : `retina` (RTSP pur Rust, transport TCP entrelacé, authentification Digest).
//! Décodage : OpenH264 pour le H.264, dans un fil à part pour ne jamais bloquer le
//! réseau ni l'interface. Le H.265 (flux HD de certaines caméras) passe par FFmpeg s'il
//! est installé ; sinon `NeedsFfmpeg` est renvoyé et l'appelant bascule sur le flux fluide.

use super::{Frame, StreamStats};
use anyhow::{anyhow, bail, Context, Result};
use futures::StreamExt;
use retina::client::{Credentials, PlayOptions, SessionOptions, SetupOptions, TcpTransportOptions, Transport};
use retina::codec::{CodecItem, FrameFormat, ParametersRef};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::timeout;

/// Délai sans image au-delà duquel le flux est considéré comme coupé.
const STALL: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Reçoit chaque image décodée.
pub type FrameSink = Arc<dyn Fn(Frame) + Send + Sync>;

/// Reçoit le son décodé : échantillons mono −1…1 et leur fréquence.
pub type AudioSink = Arc<dyn Fn(&[f32], u32) + Send + Sync>;

/// Le flux est en H.265 et FFmpeg n'est pas disponible pour le décoder.
#[derive(Debug)]
pub struct NeedsFfmpeg;

impl std::fmt::Display for NeedsFfmpeg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("flux en H.265 : installe FFmpeg pour le décoder, ou utilise le flux fluide (H.264)")
    }
}

impl std::error::Error for NeedsFfmpeg {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

impl Codec {
    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::H265 => "H.265",
        }
    }
}

/// Compteurs partagés avec le fil de décodage.
#[derive(Default)]
struct Counters {
    frames: AtomicU64,
    /// Temps passé à décoder et convertir, en microsecondes.
    busy_us: AtomicU64,
}

impl Counters {
    fn record(&self, started: Instant) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.busy_us.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
}

struct Packet {
    data: Vec<u8>,
    key: bool,
    dims: (u32, u32),
}

/// Explique une erreur de connexion en termes simples.
pub fn describe_error(e: &anyhow::Error) -> String {
    if e.downcast_ref::<NeedsFfmpeg>().is_some() {
        return NeedsFfmpeg.to_string();
    }
    if let Some(r) = e.downcast_ref::<retina::Error>() {
        match r.status_code() {
            Some(401) => return "identifiants refusés (code de sécurité ou mot de passe incorrect)".into(),
            Some(404) => return "flux introuvable sur la caméra (chemin RTSP)".into(),
            Some(403) => return "accès temporairement bloqué par la caméra (RTSP 403) : elle se verrouille quelques minutes après plusieurs mots de passe erronés".into(),
            Some(453) => return "la caméra n'a plus de flux disponible (RTSP 453)".into(),
            // Réponse acceptée (2xx) mais illisible : c'est son contenu qu'il faut expliquer.
            Some(code) if !(200..300).contains(&code) => return format!("la caméra refuse le flux (RTSP {code})"),
            _ => {}
        }
    }
    let text = format!("{e:#}");
    if text.contains("refused") || text.contains("refusé") || text.contains("10061") {
        "la caméra refuse la connexion (RTSP désactivé ?)".into()
    } else if text.contains("timed out") || text.contains("deadline") || text.contains("délai") || text.contains("10060") {
        "la caméra ne répond pas (éteinte ou hors du réseau ?)".into()
    } else if text.contains("10054") || text.contains("reset by peer") || text.contains("fermée par l") || text.contains("UnexpectedEof") || text.contains("unexpected EOF") {
        "la caméra a coupé la connexion (redémarrage, Wi-Fi perdu ?)".into()
    } else {
        // Le client RTSP détaille la connexion et l'heure sur les lignes suivantes : sans elles, la
        // même panne garde le même texte (et n'est pas réinscrite à chaque nouvel essai).
        text.lines().next().unwrap_or_default().trim().to_string()
    }
}

/// Refus d'identifiants (401) ou verrouillage après des essais ratés (403) : réessayer
/// automatiquement entretiendrait le verrouillage de la caméra.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthRefusal {
    BadPassword,
    Locked,
}

pub fn auth_refusal(e: &anyhow::Error) -> Option<AuthRefusal> {
    match e.downcast_ref::<retina::Error>()?.status_code()? {
        401 => Some(AuthRefusal::BadPassword),
        403 => Some(AuthRefusal::Locked),
        _ => None,
    }
}

/// Codec vidéo annoncé par un nom d'encodage SDP.
fn codec_of(name: &str) -> Option<Codec> {
    match name.to_ascii_lowercase().as_str() {
        "h264" => Some(Codec::H264),
        "h265" | "hevc" => Some(Codec::H265),
        _ => None,
    }
}

fn dims(p: Option<ParametersRef<'_>>) -> (u32, u32) {
    match p {
        Some(ParametersRef::Video(v)) => v.pixel_dimensions(),
        _ => (0, 0),
    }
}

/// Lit le flux jusqu'à erreur, coupure, ou abandon du futur (fermeture propre côté caméra).
/// `on_stats` est appelé environ une fois par seconde.
pub async fn run(url: &str, user: &str, password: &str, sink: FrameSink, on_stats: impl Fn(StreamStats) + Send) -> Result<()> {
    run_with_audio(url, user, password, sink, on_stats, None).await
}

/// Comme `run`, avec le son de la caméra s'il y en a un (`audio` reçoit les échantillons décodés).
pub async fn run_with_audio(url: &str, user: &str, password: &str, sink: FrameSink, on_stats: impl Fn(StreamStats) + Send, audio: Option<AudioSink>) -> Result<()> {
    let parsed = url::Url::parse(url).with_context(|| format!("adresse RTSP invalide : {url}"))?;
    let creds = (!user.is_empty()).then(|| Credentials { username: user.to_string(), password: password.to_string() });
    let opts = SessionOptions::default().creds(creds).user_agent("HomeSmart".into());
    let mut session = timeout(CONNECT_TIMEOUT, retina::client::Session::describe(parsed, opts))
        .await
        .map_err(|_| anyhow!("délai dépassé : la caméra ne répond pas"))??;
    let (index, codec) = session
        .streams()
        .iter()
        .enumerate()
        .find_map(|(i, s)| (s.media() == "video").then(|| codec_of(s.encoding_name()).map(|c| (i, c))).flatten())
        .ok_or_else(|| anyhow!("pas de vidéo H.264 / H.265 dans ce flux"))?;
    if codec == Codec::H265 && ffmpeg_path().is_none() {
        return Err(NeedsFfmpeg.into());
    }
    let setup = SetupOptions::default().transport(Transport::Tcp(TcpTransportOptions::default())).frame_format(FrameFormat::SIMPLE);
    timeout(CONNECT_TIMEOUT, session.setup(index, setup)).await.map_err(|_| anyhow!("délai dépassé (SETUP)"))??;
    // Piste son, seulement si on veut l'écouter (AAC brut + sa configuration, G.711 ou PCM 16 bits).
    let audio_index = audio.as_ref().and_then(|_| session.streams().iter().position(|s| s.media() == "audio" && super::audio::supported(s.encoding_name())));
    if let Some(ai) = audio_index {
        let format = FrameFormat { aac_framing: retina::codec::aac::Framing::Raw, ..FrameFormat::SIMPLE };
        let setup = SetupOptions::default().transport(Transport::Tcp(TcpTransportOptions::default())).frame_format(format);
        timeout(CONNECT_TIMEOUT, session.setup(ai, setup)).await.map_err(|_| anyhow!("délai dépassé (SETUP audio)"))??;
    }
    let audio_encoding = audio_index.map(|ai| session.streams()[ai].encoding_name().to_ascii_lowercase());
    // Image et son : un serveur qui vient de démarrer (MediaMTX d'une caméra HomeSmart) peut
    // répondre au PLAY sans `rtptime` ; on part alors du premier paquet de chaque piste.
    let play = PlayOptions::default().initial_timestamp(retina::client::InitialTimestampPolicy::Permissive);
    let playing = timeout(CONNECT_TIMEOUT, session.play(play)).await.map_err(|_| anyhow!("délai dépassé (PLAY)"))??;
    let mut demuxed = playing.demuxed()?;
    let mut size = dims(demuxed.streams()[index].parameters());

    // Fil de décodage : file courte ; si le décodeur prend du retard, on saute jusqu'à la prochaine image clé.
    let (tx, rx) = sync_channel::<Packet>(24);
    let counters = Arc::new(Counters::default());
    let counter = counters.clone();
    let decoder = std::thread::Builder::new()
        .name("homesmart-video".into())
        .spawn(move || decode_loop(codec, rx, sink, counter))
        .context("fil de décodage")?;

    let mut stats = StreamStats { codec: codec.label().into(), width: size.0, height: size.1, ..StreamStats::default() };
    on_stats(stats.clone());
    let mut audio_decoder: Option<super::audio::Decoder> = None;
    let mut samples: Vec<f32> = Vec::new();
    let (mut window_start, mut bytes, mut skipping) = (Instant::now(), 0u64, false);
    let (mut last_decoded, mut last_busy) = (0u64, 0u64);
    let result: Result<()> = loop {
        let item = match timeout(STALL, demuxed.next()).await {
            Err(_) => break Err(anyhow!("plus d'images depuis {} s", STALL.as_secs())),
            Ok(None) => break Err(anyhow!("la caméra a fermé le flux")),
            Ok(Some(Err(e))) => break Err(e.into()),
            Ok(Some(Ok(item))) => item,
        };
        if let (CodecItem::AudioFrame(f), Some(ai), Some(out)) = (&item, audio_index, audio.as_ref()) {
            if f.stream_id() == ai {
                if audio_decoder.is_none() {
                    if let Some(ParametersRef::Audio(p)) = demuxed.streams()[ai].parameters() {
                        let enc = audio_encoding.as_deref().unwrap_or("");
                        match super::audio::Decoder::new(enc, p.clock_rate(), p.channels().get(), p.extra_data()) {
                            Ok(d) => {
                                stats.audio = format!("{} {} kHz", super::audio::codec_label(enc), p.clock_rate() / 1000);
                                audio_decoder = Some(d);
                            }
                            Err(e) => tracing::warn!("son de la caméra : {e:#}"),
                        }
                    }
                }
                if let Some(d) = audio_decoder.as_mut() {
                    samples.clear();
                    match d.decode(f.data(), &mut samples) {
                        Ok(()) => out(&samples, d.rate()),
                        Err(e) => tracing::debug!("paquet audio illisible : {e:#}"),
                    }
                }
            }
            continue;
        }
        if let CodecItem::VideoFrame(f) = item {
            if f.has_new_parameters() {
                size = dims(demuxed.streams()[index].parameters());
                tracing::debug!("{} : nouveaux paramètres vidéo, {}×{}", codec.label(), size.0, size.1);
            }
            // En H.265, retina ne prend que les IDR pour des images clés (pas les CRA).
            let key = f.is_random_access_point();
            let data = f.into_data();
            bytes += data.len() as u64;
            if skipping && !key {
                stats.dropped += 1;
                continue;
            }
            match tx.try_send(Packet { data, key, dims: size }) {
                Ok(()) => skipping = false,
                Err(TrySendError::Full(_)) => {
                    skipping = true;
                    stats.dropped += 1;
                }
                Err(TrySendError::Disconnected(_)) => break Err(anyhow!("décodeur arrêté")),
            }
        }
        let elapsed = window_start.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let n = counters.frames.load(Ordering::Relaxed);
            let busy = counters.busy_us.load(Ordering::Relaxed);
            stats.fps = (n - last_decoded) as f32 / elapsed.as_secs_f32();
            if n > last_decoded {
                stats.decode_ms = (busy - last_busy) as f32 / 1000.0 / (n - last_decoded) as f32;
            }
            last_busy = busy;
            stats.kbps = bytes as f32 * 8.0 / 1000.0 / elapsed.as_secs_f32();
            (stats.width, stats.height) = size;
            on_stats(stats.clone());
            last_decoded = n;
            bytes = 0;
            window_start = Instant::now();
        }
    };
    drop(tx);
    let _ = decoder.join();
    result
}

/// Piste annoncée par la caméra (DESCRIBE) : vidéo, audio…
#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    pub media: String,
    pub encoding: String,
    pub clock_rate: u32,
    pub channels: Option<u16>,
}

/// Pistes du flux, sans le lire (une seule requête DESCRIBE).
pub async fn tracks(url: &str, user: &str, password: &str) -> Result<Vec<Track>> {
    let parsed = url::Url::parse(url).with_context(|| format!("adresse RTSP invalide : {url}"))?;
    let creds = (!user.is_empty()).then(|| Credentials { username: user.to_string(), password: password.to_string() });
    let opts = SessionOptions::default().creds(creds).user_agent("HomeSmart".into());
    let session = timeout(CONNECT_TIMEOUT, retina::client::Session::describe(parsed, opts))
        .await
        .map_err(|_| anyhow!("délai dépassé : la caméra ne répond pas"))??;
    Ok(session
        .streams()
        .iter()
        .map(|s| Track { media: s.media().to_string(), encoding: s.encoding_name().to_string(), clock_rate: s.clock_rate_hz(), channels: s.channels().map(|c| c.get()) })
        .collect())
}

/// Ouvre le flux le temps d'obtenir une image (capture, test de connexion).
pub async fn grab(url: &str, user: &str, password: &str, wait: Duration) -> Result<(Frame, StreamStats)> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(1);
    let sink: FrameSink = Arc::new(move |f| {
        let _ = tx.try_send(f);
    });
    let stats = Arc::new(std::sync::Mutex::new(StreamStats::default()));
    let st = stats.clone();
    let play = run(url, user, password, sink, move |s| *st.lock().unwrap() = s);
    tokio::pin!(play);
    let frame = tokio::select! {
        r = &mut play => {
            r?;
            bail!("flux terminé avant la première image");
        }
        f = rx.recv() => f.ok_or_else(|| anyhow!("aucune image reçue"))?,
        _ = tokio::time::sleep(wait) => bail!("aucune image reçue en {} s", wait.as_secs()),
    };
    let mut s = stats.lock().unwrap().clone();
    (s.width, s.height) = (frame.width as u32, frame.height as u32);
    Ok((frame, s))
}

// ---- Décodage --------------------------------------------------------------------

fn decode_loop(codec: Codec, rx: Receiver<Packet>, sink: FrameSink, decoded: Arc<Counters>) {
    let result = match codec {
        Codec::H264 => decode_h264(&rx, &sink, &decoded),
        Codec::H265 => decode_ffmpeg(&rx, &sink, decoded, "hevc"),
    };
    if let Err(e) = result {
        tracing::warn!("décodage vidéo arrêté : {e:#}");
    }
}

fn decode_h264(rx: &Receiver<Packet>, sink: &FrameSink, decoded: &Counters) -> Result<()> {
    let mut dec = openh264::decoder::Decoder::new().map_err(|e| anyhow!("OpenH264 : {e}"))?;
    let mut waiting_key = true;
    let mut seq = 0u64;
    for p in rx {
        if waiting_key && !p.key {
            continue;
        }
        waiting_key = false;
        let started = Instant::now();
        match dec.decode(&p.data) {
            Ok(Some(yuv)) => {
                use openh264::formats::YUVSource;
                let (w, h) = yuv.dimensions();
                let mut rgba = vec![0u8; w * h * 4];
                yuv.write_rgba8(&mut rgba);
                seq += 1;
                decoded.record(started);
                sink(Frame { seq, width: w, height: h, rgba });
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!("image illisible, attente de la prochaine image clé : {e}");
                waiting_key = true;
            }
        }
    }
    Ok(())
}

/// FFmpeg : à côté de l'exécutable, dans le PATH ou là où winget l'a installé.
pub fn ffmpeg_path() -> Option<PathBuf> {
    super::ffmpeg::find()
}

fn decode_ffmpeg(rx: &Receiver<Packet>, sink: &FrameSink, decoded: Arc<Counters>, format: &str) -> Result<()> {
    let ffmpeg = ffmpeg_path().ok_or(NeedsFfmpeg)?;
    let mut waiting_key = true;
    let mut child: Option<(std::process::Child, std::process::ChildStdin, (u32, u32), std::thread::JoinHandle<()>)> = None;
    for p in rx {
        if waiting_key && !p.key {
            continue;
        }
        waiting_key = false;
        if child.as_ref().is_some_and(|c| c.2 != p.dims) {
            if let Some((mut c, stdin, _, reader)) = child.take() {
                drop(stdin);
                let _ = c.kill();
                let _ = reader.join();
            }
        }
        if child.is_none() {
            let (w, h) = p.dims;
            if w == 0 || h == 0 {
                bail!("dimensions du flux inconnues");
            }
            tracing::debug!("décodage {format} par FFmpeg en {w}×{h}");
            let mut cmd = std::process::Command::new(&ffmpeg);
            cmd.args(["-hide_banner", "-loglevel", "error", "-fflags", "nobuffer", "-flags", "low_delay", "-probesize", "32", "-analyzeduration", "0", "-f", format, "-i", "pipe:0", "-f", "rawvideo", "-pix_fmt", "rgba", "-s"])
                .arg(format!("{w}x{h}"))
                .arg("pipe:1")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x0800_0000); // pas de fenêtre console
            }
            let mut c = cmd.spawn().context("lancement de FFmpeg")?;
            let stdin = c.stdin.take().ok_or_else(|| anyhow!("FFmpeg : entrée indisponible"))?;
            let mut stdout = c.stdout.take().ok_or_else(|| anyhow!("FFmpeg : sortie indisponible"))?;
            let sink = sink.clone();
            let frame_len = w as usize * h as usize * 4;
            let counter = decoded.clone();
            let reader = std::thread::spawn(move || {
                let mut seq = 0u64;
                loop {
                    let mut rgba = vec![0u8; frame_len];
                    if stdout.read_exact(&mut rgba).is_err() {
                        break;
                    }
                    seq += 1;
                    counter.frames.fetch_add(1, Ordering::Relaxed);
                    sink(Frame { seq, width: w as usize, height: h as usize, rgba });
                }
            });
            child = Some((c, stdin, p.dims, reader));
        }
        if let Some((_, stdin, _, _)) = child.as_mut() {
            if stdin.write_all(&p.data).is_err() {
                bail!("FFmpeg s'est arrêté");
            }
        }
    }
    if let Some((mut c, stdin, _, reader)) = child.take() {
        drop(stdin);
        let _ = c.kill();
        let _ = reader.join();
    }
    Ok(())
}
