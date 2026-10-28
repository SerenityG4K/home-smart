//! Commandes lancées par l'agent : le serveur RTSP (MediaMTX) et la chaîne qui lui publie les
//! deux flux (`/main`, `/sub`), image et son.
//!
//! Le son part en PCM 16 bits gros-boutiste à 16 kHz, mono (L16) : sans perte, sans codec à
//! licencier, et au format qu'attend l'écoute de HomeSmart (voix, sons, transcription).

use crate::config::{AgentConfig, Backend};
use anyhow::{anyhow, Result};
use homesmart_cam_proto::{self as proto, Settings, StreamSettings, VideoCodec};
use std::path::PathBuf;

/// Fréquence du micro dans les flux.
pub const AUDIO_RATE: u32 = 16_000;

/// Outils trouvés sur la machine.
#[derive(Clone, Debug, Default)]
pub struct ResolvedTools {
    pub mediamtx: Option<PathBuf>,
    pub ffmpeg: Option<PathBuf>,
    pub gst_launch: Option<PathBuf>,
}

pub fn stream_url(cfg: &AgentConfig, path: &str) -> String {
    format!("rtsp://127.0.0.1:{}{path}", cfg.rtsp_port)
}

fn video_device(cfg: &AgentConfig) -> String {
    cfg.video_device.clone().unwrap_or_else(|| match cfg.backend {
        // Radxa OS nomme ainsi la sortie de l'ISP du premier capteur (à confirmer sur la carte).
        Backend::Rockchip => "/dev/video-camera0".into(),
        _ => "/dev/video0".into(),
    })
}

fn audio_device(cfg: &AgentConfig) -> String {
    cfg.audio_device.clone().unwrap_or_else(|| "default".into())
}

fn with_audio(cfg: &AgentConfig, caps: &proto::Caps) -> bool {
    cfg.settings.audio.enabled && caps.audio
}

fn tool(t: &Option<PathBuf>, name: &str) -> Result<String> {
    t.as_ref().map(|p| p.to_string_lossy().into_owned()).ok_or_else(|| anyhow!("{name} est introuvable : installe-le (voir cam/README.md) ou indique son chemin dans la configuration"))
}

/// Commande qui publie les deux flux vers le serveur RTSP local.
pub fn publisher(cfg: &AgentConfig, caps: &proto::Caps, tools: &ResolvedTools) -> Result<Vec<String>> {
    match cfg.backend {
        Backend::Rockchip => Ok(gstreamer_rockchip(cfg, caps, &tool(&tools.gst_launch, "gst-launch-1.0")?)),
        Backend::V4l2 => {
            let s = &cfg.settings.main;
            let video = vec!["-f".into(), "v4l2".into(), "-framerate".into(), s.fps.to_string(), "-video_size".into(), format!("{}x{}", s.width, s.height), "-i".into(), video_device(cfg)];
            let audio = vec!["-f".into(), "alsa".into(), "-channels".into(), "1".into(), "-sample_rate".into(), AUDIO_RATE.to_string(), "-i".into(), audio_device(cfg)];
            Ok(ffmpeg(cfg, caps, &tool(&tools.ffmpeg, "FFmpeg")?, video, audio, "veryfast"))
        }
        Backend::Test => {
            let s = &cfg.settings.main;
            let video = vec!["-re".into(), "-f".into(), "lavfi".into(), "-i".into(), format!("testsrc2=size={}x{}:rate={}", s.width, s.height, s.fps)];
            let audio = vec!["-re".into(), "-f".into(), "lavfi".into(), "-i".into(), format!("sine=frequency=440:sample_rate={AUDIO_RATE}")];
            Ok(ffmpeg(cfg, caps, &tool(&tools.ffmpeg, "FFmpeg")?, video, audio, "ultrafast"))
        }
        Backend::Custom => custom(cfg),
    }
}

/// Rockchip : capteur → ISP → `tee` → deux encodeurs matériels MPP → deux `rtspclientsink`.
/// Noms des propriétés d'après gstreamer-rockchip ; à valider sur la carte (sinon : backend
/// `custom`).
fn gstreamer_rockchip(cfg: &AgentConfig, caps: &proto::Caps, gst: &str) -> Vec<String> {
    let Settings { main, sub, rotate_180, .. } = &cfg.settings;
    let mut a: Vec<String> = vec![gst.into(), "-e".into()];
    let mut push = |s: &str| a.extend(s.split(' ').filter(|t| !t.is_empty()).map(str::to_string));
    push(&format!("v4l2src device={} io-mode=dmabuf ! video/x-raw,format=NV12,width={},height={},framerate={}/1 ! tee name=t", video_device(cfg), main.width, main.height, main.fps));
    push(&format!("t. ! queue max-size-buffers=4 leaky=downstream ! {} ! queue ! smain.", gst_encoder(main, false, *rotate_180)));
    let rate = if sub.fps < main.fps { format!("videorate ! video/x-raw,framerate={}/1 ! ", sub.fps) } else { String::new() };
    push(&format!("t. ! queue max-size-buffers=4 leaky=downstream ! {rate}{} ! queue ! ssub.", gst_encoder(sub, true, *rotate_180)));
    if with_audio(cfg, caps) {
        push(&format!(
            "alsasrc device={} ! audioconvert ! audioresample ! audio/x-raw,rate={AUDIO_RATE},channels=1 ! volume volume={:.4} ! audioconvert ! audio/x-raw,format=S16BE,rate={AUDIO_RATE},channels=1 ! tee name=a",
            audio_device(cfg),
            gain(cfg.settings.audio.gain_db)
        ));
        push("a. ! queue ! smain. a. ! queue ! ssub.");
    }
    push(&format!("rtspclientsink name=smain location={} protocols=tcp", stream_url(cfg, proto::MAIN_PATH)));
    push(&format!("rtspclientsink name=ssub location={} protocols=tcp", stream_url(cfg, proto::SUB_PATH)));
    a
}

fn gst_encoder(s: &StreamSettings, scale: bool, rotate: bool) -> String {
    let (enc, parse) = match s.codec {
        VideoCodec::H264 => ("mpph264enc", "h264parse"),
        VideoCodec::H265 => ("mpph265enc", "h265parse"),
    };
    let bps = s.kbps as u64 * 1000;
    let mut e = format!("{enc} rc-mode=cbr bps={bps} bps-max={} gop={}", bps * 3 / 2, s.fps * 2);
    if scale {
        e.push_str(&format!(" width={} height={}", s.width, s.height));
    }
    if rotate {
        e.push_str(" rotation=180");
    }
    format!("{e} ! {parse} config-interval=-1")
}

/// Gain en dB → facteur d'amplitude.
fn gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// FFmpeg : une entrée vidéo (et une entrée son), deux sorties RTSP.
fn ffmpeg(cfg: &AgentConfig, caps: &proto::Caps, ffmpeg: &str, video_in: Vec<String>, audio_in: Vec<String>, preset: &str) -> Vec<String> {
    let Settings { main, sub, rotate_180, audio, .. } = &cfg.settings;
    let sound = with_audio(cfg, caps);
    let mut a: Vec<String> = vec![ffmpeg.into(), "-hide_banner".into(), "-loglevel".into(), "error".into(), "-nostdin".into()];
    a.extend(video_in);
    if sound {
        a.extend(audio_in);
    }
    let turn = if *rotate_180 { "hflip,vflip," } else { "" };
    let sub_rate = if sub.fps < main.fps { format!(",fps={}", sub.fps) } else { String::new() };
    let mut graph = format!("[0:v]{turn}split=2[vm][vs0];[vs0]scale={}:{}{sub_rate}[vs]", sub.width, sub.height);
    if sound {
        graph.push_str(&format!(";[1:a]aresample={AUDIO_RATE},volume={:.1}dB,asplit=2[am][as]", audio.gain_db));
    }
    a.extend(["-filter_complex".into(), graph]);
    for (s, v, au, path) in [(main, "[vm]", "[am]", proto::MAIN_PATH), (sub, "[vs]", "[as]", proto::SUB_PATH)] {
        a.extend(["-map".into(), v.into()]);
        if sound {
            a.extend(["-map".into(), au.into()]);
        }
        let enc = match s.codec {
            VideoCodec::H264 => "libx264",
            VideoCodec::H265 => "libx265",
        };
        a.extend(
            [
                "-c:v",
                enc,
                "-preset",
                preset,
                "-tune",
                "zerolatency",
                "-pix_fmt",
                "yuv420p",
            ]
            .map(String::from),
        );
        a.extend(["-g".into(), (s.fps * 2).to_string(), "-b:v".into(), format!("{}k", s.kbps), "-maxrate".into(), format!("{}k", s.kbps), "-bufsize".into(), format!("{}k", s.kbps * 2)]);
        if s.codec == VideoCodec::H265 {
            // GOP fermé : des images clés IDR, et non CRA (que certains clients RTSP, dont celui
            // de HomeSmart, ne prennent pas pour des points de départ).
            a.extend(["-x265-params".into(), "log-level=error:open-gop=0".into()]);
        }
        if sound {
            a.extend(["-c:a".into(), "pcm_s16be".into(), "-ar".into(), AUDIO_RATE.to_string(), "-ac".into(), "1".into()]);
        }
        a.extend(["-f".into(), "rtsp".into(), "-rtsp_transport".into(), "tcp".into(), stream_url(cfg, path)]);
    }
    a
}

fn custom(cfg: &AgentConfig) -> Result<Vec<String>> {
    if cfg.custom.is_empty() {
        return Err(anyhow!("backend « custom » sans commande : remplis « custom » dans la configuration"));
    }
    let Settings { main, sub, audio, .. } = &cfg.settings;
    let codec = |c: VideoCodec| match c {
        VideoCodec::H264 => "h264",
        VideoCodec::H265 => "h265",
    };
    let vars = [
        ("{main_url}", stream_url(cfg, proto::MAIN_PATH)),
        ("{sub_url}", stream_url(cfg, proto::SUB_PATH)),
        ("{width}", main.width.to_string()),
        ("{height}", main.height.to_string()),
        ("{fps}", main.fps.to_string()),
        ("{kbps}", main.kbps.to_string()),
        ("{codec}", codec(main.codec).into()),
        ("{sub_width}", sub.width.to_string()),
        ("{sub_height}", sub.height.to_string()),
        ("{sub_fps}", sub.fps.to_string()),
        ("{sub_kbps}", sub.kbps.to_string()),
        ("{sub_codec}", codec(sub.codec).into()),
        ("{video_device}", video_device(cfg)),
        ("{audio_device}", audio_device(cfg)),
        ("{gain_db}", format!("{:.1}", audio.gain_db)),
    ];
    Ok(cfg.custom.iter().map(|arg| vars.iter().fold(arg.clone(), |s, (k, v)| s.replace(k, v))).collect())
}

/// Configuration de MediaMTX : RTSP seul (TCP, Digest), publication réservée à la machine
/// elle-même, lecture réservée aux HomeSmart appairés (mot de passe dérivé de leur secret).
pub fn mediamtx_yml(cfg: &AgentConfig) -> String {
    let mut y = String::new();
    y.push_str("# Écrit par homesmart-cam à chaque démarrage : ne pas modifier à la main.\n");
    y.push_str("logLevel: warn\nlogDestinations: [stdout]\nreadTimeout: 10s\nwriteTimeout: 10s\nwriteQueueSize: 2048\n");
    y.push_str("api: false\nmetrics: false\npprof: false\nplayback: false\nrtmp: false\nhls: false\nwebrtc: false\nsrt: false\n");
    y.push_str(&format!("rtsp: true\nrtspTransports: [tcp]\nrtspAddress: :{}\nrtspAuthMethods: [digest]\n", cfg.rtsp_port));
    y.push_str("authMethod: internal\nauthInternalUsers:\n");
    y.push_str("  - user: any\n    pass:\n    ips: [\"127.0.0.1\", \"::1\"]\n    permissions:\n      - action: publish\n        path:\n");
    for c in &cfg.clients {
        let Some(secret) = c.secret_bytes() else { continue };
        y.push_str(&format!("  - user: \"{}\"\n    pass: \"{}\"\n    ips: []\n    permissions:\n      - action: read\n        path:\n", c.id, proto::rtsp_password(&secret)));
    }
    y.push_str("paths:\n  main:\n    source: publisher\n  sub:\n    source: publisher\n");
    y
}

/// Commande du serveur RTSP.
pub fn server(tools: &ResolvedTools, yml: &std::path::Path) -> Result<Vec<String>> {
    Ok(vec![tool(&tools.mediamtx, "MediaMTX")?, yml.to_string_lossy().into_owned()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Client, Tools};

    fn cfg(backend: Backend) -> (AgentConfig, proto::Caps) {
        let caps = proto::Caps { max_width: 3840, max_height: 2160, max_fps: 30, codecs: vec![VideoCodec::H264, VideoCodec::H265], audio: true, sensor: None };
        let cfg = AgentConfig {
            id: "aabbccddeeff".into(),
            setup_code: "ABCDEFGH".into(),
            clients: vec![Client { id: "hs-01020304".into(), name: "PC".into(), secret: hex::encode([3u8; 32]), paired_at: 0 }],
            backend,
            settings: caps.default_settings("Salon"),
            video_device: None,
            audio_device: Some("hw:1,0".into()),
            has_mic: None,
            caps: None,
            api_port: proto::API_PORT,
            rtsp_port: 8554,
            tools: Tools::default(),
            custom: Vec::new(),
        };
        (cfg, caps)
    }

    fn tools() -> ResolvedTools {
        ResolvedTools { mediamtx: Some("/usr/local/bin/mediamtx".into()), ffmpeg: Some("ffmpeg".into()), gst_launch: Some("gst-launch-1.0".into()) }
    }

    #[test]
    fn rockchip_encodes_both_streams_in_hardware_with_pcm_sound() {
        let (mut c, caps) = cfg(Backend::Rockchip);
        let line = publisher(&c, &caps, &tools()).unwrap().join(" ");
        assert!(line.starts_with("gst-launch-1.0 -e v4l2src device=/dev/video-camera0"), "{line}");
        assert!(line.contains("width=3840,height=2160,framerate=30/1"), "{line}");
        assert!(line.contains("mpph265enc rc-mode=cbr bps="), "{line}");
        assert!(line.contains("mpph264enc rc-mode=cbr") && line.contains("width=1280 height=720"), "{line}");
        assert!(line.contains("alsasrc device=hw:1,0") && line.contains("format=S16BE,rate=16000,channels=1"), "{line}");
        assert!(line.contains("rtspclientsink name=smain location=rtsp://127.0.0.1:8554/main protocols=tcp"), "{line}");
        assert!(!line.contains("rotation"));
        // Micro coupé, image retournée, flux secondaire allégé.
        c.settings.audio.enabled = false;
        c.settings.rotate_180 = true;
        c.settings.sub.fps = 10;
        let line = publisher(&c, &caps, &tools()).unwrap().join(" ");
        assert!(!line.contains("alsasrc") && line.contains("rotation=180") && line.contains("videorate ! video/x-raw,framerate=10/1"), "{line}");
    }

    #[test]
    fn ffmpeg_backends_publish_two_outputs() {
        let (c, caps) = cfg(Backend::Test);
        let a = publisher(&c, &caps, &tools()).unwrap();
        let line = a.join(" ");
        assert!(line.contains("testsrc2=size=3840x2160:rate=30") && line.contains("sine=frequency=440:sample_rate=16000"), "{line}");
        assert_eq!(a.iter().filter(|x| x.as_str() == "-f").count(), 4, "{line}");
        assert!(line.contains("-c:v libx265") && line.contains("-c:v libx264"), "{line}");
        assert!(line.contains("open-gop=0"), "{line}");
        assert_eq!(a.iter().filter(|x| x.as_str() == "pcm_s16be").count(), 2, "{line}");
        assert!(a.last().unwrap().ends_with("/sub"));
        // Webcam sans micro : pas d'entrée ALSA ni de piste son.
        let (mut c, mut caps) = cfg(Backend::V4l2);
        caps.audio = false;
        c.settings.main.codec = VideoCodec::H264;
        let line = publisher(&c, &caps, &tools()).unwrap().join(" ");
        assert!(line.contains("-f v4l2 -framerate 30 -video_size 3840x2160 -i /dev/video0"), "{line}");
        assert!(!line.contains("alsa") && !line.contains("pcm_s16be") && !line.contains("[1:a]"), "{line}");
    }

    #[test]
    fn custom_command_placeholders() {
        let (mut c, caps) = cfg(Backend::Custom);
        assert!(publisher(&c, &caps, &tools()).is_err());
        c.custom = vec!["my-encoder".into(), "--out={main_url}".into(), "--size={width}x{height}@{fps}".into(), "--mic={audio_device}".into()];
        let a = publisher(&c, &caps, &tools()).unwrap();
        assert_eq!(a, vec!["my-encoder", "--out=rtsp://127.0.0.1:8554/main", "--size=3840x2160@30", "--mic=hw:1,0"]);
    }

    #[test]
    fn mediamtx_reads_only_for_paired_clients() {
        let (c, _) = cfg(Backend::Test);
        let y = mediamtx_yml(&c);
        assert!(y.contains("rtspAddress: :8554") && y.contains("rtspAuthMethods: [digest]") && y.contains("rtspTransports: [tcp]"));
        assert!(y.contains("user: \"hs-01020304\""));
        assert!(y.contains(&format!("pass: \"{}\"", proto::rtsp_password(&[3u8; 32]))));
        // La publication n'est permise qu'en local, et rien d'autre n'est ouvert.
        assert!(y.contains("ips: [\"127.0.0.1\", \"::1\"]\n    permissions:\n      - action: publish"));
        for off in ["api: false", "hls: false", "webrtc: false", "rtmp: false", "srt: false"] {
            assert!(y.contains(off), "{off}");
        }
        assert!(tool(&None, "MediaMTX").unwrap_err().to_string().contains("introuvable"));
    }
}
