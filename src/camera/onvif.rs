//! Client ONVIF minimal : informations, profils vidéo, orientation (PTZ) et positions mémorisées.
//!
//! Requêtes SOAP signées en WS-Security (UsernameToken, mot de passe condensé). Les appels
//! sont bloquants : à lancer hors de l'interface (thread ou `spawn_blocking`).

use super::xml::{self, escape};
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::time::Duration;

const NS_DEVICE: &str = "http://www.onvif.org/ver10/device/wsdl";
const NS_MEDIA: &str = "http://www.onvif.org/ver10/media/wsdl";
const NS_PTZ: &str = "http://www.onvif.org/ver20/ptz/wsdl";
const NS_SCHEMA: &str = "http://www.onvif.org/ver10/schema";
const NS_IMAGING: &str = "http://www.onvif.org/ver20/imaging/wsdl";
const NS_EVENTS: &str = "http://www.onvif.org/ver10/events/wsdl";
const NS_ANALYTICS: &str = "http://www.onvif.org/ver20/analytics/wsdl";
const NS_WSN: &str = "http://docs.oasis-open.org/wsn/b-2";
const NS_WSA: &str = "http://www.w3.org/2005/08/addressing";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub manufacturer: String,
    pub model: String,
    pub firmware: String,
    pub serial: String,
    pub hardware: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub token: String,
    /// Capteur vidéo associé (réglages d'image).
    pub source: String,
    pub name: String,
    pub encoding: String,
    pub width: u32,
    pub height: u32,
    pub ptz: bool,
}

/// Réglages d'image du capteur (ONVIF Imaging). `None` : réglage absent sur cette caméra.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImagingSettings {
    pub brightness: Option<f32>,
    pub contrast: Option<f32>,
    pub saturation: Option<f32>,
    pub sharpness: Option<f32>,
    /// Filtre infrarouge : « AUTO », « ON » (jour), « OFF » (nuit).
    pub ir_cut: Option<String>,
    /// Plage dynamique étendue (WDR, le « HDR » des caméras) : « ON » / « OFF » et niveau.
    pub wdr: Option<String>,
    pub wdr_level: Option<f32>,
    /// Compensation de contre-jour (BLC).
    pub backlight: Option<String>,
}

/// Valeurs possibles de chaque réglage d'image.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImagingOptions {
    pub brightness: Option<(f32, f32)>,
    pub contrast: Option<(f32, f32)>,
    pub saturation: Option<(f32, f32)>,
    pub sharpness: Option<(f32, f32)>,
    pub ir_cut: Vec<String>,
    pub wdr: Vec<String>,
    pub wdr_level: Option<(f32, f32)>,
    pub backlight: Vec<String>,
}

pub fn parse_imaging_settings(body: &str) -> ImagingSettings {
    let Some(st) = xml::element(body, "ImagingSettings") else { return ImagingSettings::default() };
    let num = |name: &str| st.find_text(name).and_then(|s| s.parse::<f32>().ok());
    let mode = |name: &str| st.find(name).and_then(|e| e.find_text("Mode"));
    ImagingSettings {
        brightness: num("Brightness"),
        contrast: num("Contrast"),
        saturation: num("ColorSaturation"),
        sharpness: num("Sharpness"),
        ir_cut: st.find_text("IrCutFilter"),
        wdr: mode("WideDynamicRange"),
        wdr_level: st.find("WideDynamicRange").and_then(|e| e.find_text("Level")).and_then(|s| s.parse().ok()),
        backlight: mode("BacklightCompensation"),
    }
}

pub fn parse_imaging_options(body: &str) -> ImagingOptions {
    let Some(o) = xml::element(body, "ImagingOptions") else { return ImagingOptions::default() };
    let range = |e: Option<xml::Element>| {
        let e = e?;
        Some((e.find_text("Min")?.parse().ok()?, e.find_text("Max")?.parse().ok()?))
    };
    let modes = |name: &str| o.find(name).map(|e| e.find_all("Mode").iter().map(|m| m.text()).collect()).unwrap_or_default();
    ImagingOptions {
        brightness: range(o.find("Brightness")),
        contrast: range(o.find("Contrast")),
        saturation: range(o.find("ColorSaturation")),
        sharpness: range(o.find("Sharpness")),
        ir_cut: o.find_all("IrCutFilterModes").iter().map(|m| m.text()).collect(),
        wdr: modes("WideDynamicRange"),
        wdr_level: range(o.find("WideDynamicRange").and_then(|e| e.find("Level"))),
        backlight: modes("BacklightCompensation"),
    }
}

/// Réglage d'encodage vidéo d'un flux (ONVIF Media).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EncoderConfig {
    pub token: String,
    pub name: String,
    pub encoding: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub gov: u32,
    pub quality: f32,
    pub profile: String,
}

/// Valeurs acceptées par la caméra pour un réglage d'encodage.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EncoderOptions {
    pub resolutions: Vec<(u32, u32)>,
    pub fps: (u32, u32),
    pub gov: (u32, u32),
    pub bitrate_kbps: Option<(u32, u32)>,
    pub quality: (f32, f32),
    pub profiles: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preset {
    pub token: String,
    pub name: String,
}

/// Session ONVIF vers une caméra : adresses des services et profil utilisé.
#[derive(Clone, Debug)]
pub struct Onvif {
    device_url: String,
    user: String,
    password: String,
    /// Décalage d'horloge caméra − PC, appliqué à l'horodatage WS-Security.
    offset: ChronoDuration,
    media_url: Option<String>,
    ptz_url: Option<String>,
    imaging_url: Option<String>,
    events_url: Option<String>,
    analytics_url: Option<String>,
    pub events: bool,
    pub profiles: Vec<Profile>,
}

/// Mot de passe condensé WS-Security : Base64(SHA-1(nonce + date + mot de passe)).
pub fn password_digest(nonce: &[u8], created: &str, password: &str) -> String {
    let mut h = Sha1::new();
    h.update(nonce);
    h.update(created.as_bytes());
    h.update(password.as_bytes());
    B64.encode(h.finalize())
}

fn security_header(user: &str, password: &str, now: DateTime<Utc>) -> String {
    let mut nonce = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut nonce);
    let created = now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    let digest = password_digest(&nonce, &created, password);
    format!(
        r#"<s:Header><Security s:mustUnderstand="1" xmlns="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd"><UsernameToken><Username>{}</Username><Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{digest}</Password><Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{}</Nonce><Created xmlns="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd">{created}</Created></UsernameToken></Security></s:Header>"#,
        escape(user),
        B64.encode(nonce),
    )
}

pub fn envelope(header: &str, body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">{header}<s:Body xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">{body}</s:Body></s:Envelope>"#
    )
}

fn agent_with(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into()
}

/// Message d'une faute SOAP (`<Reason><Text>` ou code `Subcode`).
pub fn fault(body: &str) -> Option<String> {
    let f = xml::element(body, "Fault")?;
    let reason = f.find("Reason").and_then(|r| r.find_text("Text")).filter(|s| !s.is_empty());
    let code = f.find_all("Value").last().map(|v| v.text());
    Some(match (reason, code) {
        (Some(r), Some(c)) => format!("{r} ({c})"),
        (Some(r), None) => r,
        (None, Some(c)) => c,
        (None, None) => "faute SOAP".into(),
    })
}

/// Heure UTC annoncée par `GetSystemDateAndTime`.
pub fn parse_utc(body: &str) -> Option<DateTime<Utc>> {
    let utc = xml::element(body, "UTCDateTime")?;
    let n = |parent: &str, name: &str| utc.find(parent).and_then(|p| p.find_text(name)).and_then(|s| s.parse::<u32>().ok());
    let date = NaiveDate::from_ymd_opt(n("Date", "Year")? as i32, n("Date", "Month")?, n("Date", "Day")?)?;
    let dt = date.and_hms_opt(n("Time", "Hour")?, n("Time", "Minute")?, n("Time", "Second")?)?;
    Some(dt.and_utc())
}

pub fn parse_profiles(body: &str) -> Vec<Profile> {
    xml::elements(body, "Profiles")
        .into_iter()
        .filter_map(|p| {
            let token = p.attr("token")?;
            let enc = p.find("VideoEncoderConfiguration");
            let res = enc.and_then(|e| e.find("Resolution"));
            let num = |name: &str| res.and_then(|r| r.find_text(name)).and_then(|s| s.parse().ok()).unwrap_or(0);
            Some(Profile {
                token,
                source: p.find("VideoSourceConfiguration").and_then(|v| v.find_text("SourceToken")).unwrap_or_default(),
                name: p.find_text("Name").unwrap_or_default(),
                encoding: enc.and_then(|e| e.find_text("Encoding")).unwrap_or_default(),
                width: num("Width"),
                height: num("Height"),
                ptz: p.find("PTZConfiguration").is_some(),
            })
        })
        .collect()
}

pub fn parse_presets(body: &str) -> Vec<Preset> {
    xml::elements(body, "Preset")
        .into_iter()
        .filter_map(|p| {
            let token = p.attr("token")?;
            let name = p.find_text("Name").filter(|n| !n.is_empty()).unwrap_or_else(|| format!("Position {token}"));
            Some(Preset { token, name })
        })
        .collect()
}

pub fn parse_encoder_configs(body: &str) -> Vec<EncoderConfig> {
    xml::elements(body, "Configurations")
        .into_iter()
        .filter_map(|c| {
            let token = c.attr("token")?;
            let num = |parent: Option<xml::Element>, name: &str| parent.and_then(|p| p.find_text(name)).and_then(|s| s.parse::<f32>().ok()).unwrap_or(0.0);
            let res = c.find("Resolution");
            let rate = c.find("RateControl");
            let h264 = c.find("H264");
            Some(EncoderConfig {
                token,
                name: c.find_text("Name").unwrap_or_default(),
                encoding: c.find_text("Encoding").unwrap_or_default(),
                width: num(res, "Width") as u32,
                height: num(res, "Height") as u32,
                fps: num(rate, "FrameRateLimit") as u32,
                bitrate_kbps: num(rate, "BitrateLimit") as u32,
                gov: num(h264, "GovLength") as u32,
                quality: c.find_text("Quality").and_then(|s| s.parse().ok()).unwrap_or(0.0),
                profile: h264.and_then(|h| h.find_text("H264Profile")).unwrap_or_default(),
            })
        })
        .collect()
}

pub fn parse_encoder_options(body: &str) -> EncoderOptions {
    let mut o = EncoderOptions::default();
    let range = |parent: Option<xml::Element>, name: &str| {
        let r = parent.and_then(|p| p.find(name));
        let v = |k: &str| r.and_then(|r| r.find_text(k)).and_then(|s| s.parse::<f32>().ok());
        (v("Min").unwrap_or(0.0), v("Max").unwrap_or(0.0))
    };
    let opts = xml::element(body, "Options");
    o.quality = range(opts, "QualityRange");
    if let Some(h) = opts.and_then(|x| x.find("H264")) {
        o.resolutions = h
            .find_all("ResolutionsAvailable")
            .iter()
            .filter_map(|r| Some((r.find_text("Width")?.parse().ok()?, r.find_text("Height")?.parse().ok()?)))
            .collect();
        let f = range(Some(h), "FrameRateRange");
        o.fps = (f.0 as u32, f.1 as u32);
        let g = range(Some(h), "GovLengthRange");
        o.gov = (g.0 as u32, g.1 as u32);
        o.profiles = h.find_all("H264ProfilesSupported").iter().map(|p| p.text()).collect();
    }
    if let Some(ext) = opts.and_then(|x| x.find("Extension")).and_then(|e| e.find("H264")) {
        let b = range(Some(ext), "BitrateRange");
        if b.1 > 0.0 {
            o.bitrate_kbps = Some((b.0 as u32, b.1 as u32));
        }
    }
    o
}

pub fn parse_device_info(body: &str) -> Option<DeviceInfo> {
    let r = xml::element(body, "GetDeviceInformationResponse")?;
    let t = |n: &str| r.find_text(n).unwrap_or_default();
    Some(DeviceInfo { manufacturer: t("Manufacturer"), model: t("Model"), firmware: t("FirmwareVersion"), serial: t("SerialNumber"), hardware: t("HardwareId") })
}

/// Un événement de la caméra (`NotificationMessage`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EventMessage {
    /// Sujet, ex. `tns1:RuleEngine/TamperDetector/Tamper`.
    pub topic: String,
    /// « Initialized », « Changed », « Deleted » (état) ; vide pour un événement ponctuel.
    pub operation: String,
    pub source: Vec<(String, String)>,
    pub data: Vec<(String, String)>,
}

impl EventMessage {
    /// Alerte de sabotage : `Some(true)` (début), `Some(false)` (fin), `None` : autre sujet.
    /// Tapo : `RuleEngine/TamperDetector/Tamper` (IsTamper) ; Dahua / Imou : le même, et
    /// `VideoSource/GlobalSceneChange` (State : « changement de scène », caméra déplacée ou masquée).
    pub fn tamper(&self) -> Option<bool> {
        let t = self.topic.to_ascii_lowercase();
        let watched = t.contains("tamperdetector") || t.contains("globalscenechange") || t.contains("imagetooblurry") || t.contains("signalloss");
        if !watched {
            return None;
        }
        let v = self.data.iter().find(|(n, _)| matches!(n.as_str(), "IsTamper" | "State" | "IsTooBlurry" | "IsSignalLoss")).map(|(_, v)| v.as_str())?;
        Some(v.eq_ignore_ascii_case("true") || v == "1")
    }

    /// État initial annoncé à l'abonnement (pas un changement).
    pub fn initial(&self) -> bool {
        self.operation.eq_ignore_ascii_case("Initialized")
    }
}

pub fn parse_events(body: &str) -> Vec<EventMessage> {
    let items = |e: Option<xml::Element>| -> Vec<(String, String)> {
        e.map(|e| e.find_all("SimpleItem").iter().filter_map(|i| Some((i.attr("Name")?, i.attr("Value")?))).collect()).unwrap_or_default()
    };
    xml::elements(body, "NotificationMessage")
        .into_iter()
        .map(|n| {
            let outer = n.find("Message");
            let msg = outer.and_then(|m| m.find("Message")).or(outer);
            EventMessage {
                topic: n.find_text("Topic").unwrap_or_default(),
                operation: msg.and_then(|m| m.attr("PropertyOperation")).unwrap_or_default(),
                source: items(msg.and_then(|m| m.find("Source"))),
                data: items(msg.and_then(|m| m.find("Data"))),
            }
        })
        .collect()
}

/// Détecteur de sabotage intégré à la caméra (module d'analyse ONVIF `tt:TamperEngine`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TamperModule {
    /// Jeton de la configuration d'analyse qui le contient.
    pub config: String,
    pub name: String,
    /// Type annoncé (`tt:TamperEngine`).
    pub kind: String,
    pub enabled: Option<bool>,
    pub sensitivity: Option<String>,
}

pub fn parse_tamper_module(profiles: &str) -> Option<TamperModule> {
    let configs = [xml::elements(profiles, "VideoAnalyticsConfiguration"), xml::elements(profiles, "Configurations")].concat();
    configs.into_iter().find_map(|c| {
        let module = c.find_all("AnalyticsModule").into_iter().find(|m| m.attr("Type").is_some_and(|t| t.ends_with("TamperEngine")))?;
        let value = |name: &str| module.find_all("SimpleItem").into_iter().find(|i| i.attr("Name").as_deref() == Some(name)).and_then(|i| i.attr("Value"));
        Some(TamperModule {
            config: c.attr("token").unwrap_or_default(),
            name: module.attr("Name").unwrap_or_default(),
            kind: module.attr("Type").unwrap_or_default(),
            enabled: value("Enabled").map(|v| v.eq_ignore_ascii_case("on") || v.eq_ignore_ascii_case("true")),
            sensitivity: value("Sensitivity"),
        })
    })
}

impl Onvif {
    /// Ouvre une session : heure de la caméra, services disponibles et profils vidéo.
    pub fn connect(host: &str, port: u16, user: &str, password: &str) -> Result<Onvif> {
        let hostport = if port == 80 { host.to_string() } else { format!("{host}:{port}") };
        let mut s = Onvif {
            device_url: format!("http://{hostport}/onvif/device_service"),
            user: user.to_string(),
            password: password.to_string(),
            offset: ChronoDuration::zero(),
            media_url: None,
            ptz_url: None,
            imaging_url: None,
            events_url: None,
            analytics_url: None,
            events: false,
            profiles: Vec::new(),
        };
        // Sans authentification : horloge (pour signer avec l'heure de la caméra) et services.
        let body = s.call(&s.device_url, &format!(r#"<GetSystemDateAndTime xmlns="{NS_DEVICE}"/>"#), false, "GetSystemDateAndTimeResponse")
            .context("la caméra ne répond pas en ONVIF")?;
        if let Some(cam) = parse_utc(&body) {
            s.offset = cam - Utc::now();
        }
        let caps = s.call(&s.device_url, &format!(r#"<GetCapabilities xmlns="{NS_DEVICE}"><Category>All</Category></GetCapabilities>"#), true, "GetCapabilitiesResponse")?;
        let xaddr = |name: &str| xml::element(&caps, name).and_then(|e| e.find_text("XAddr")).map(|u| rebase(&u, &hostport));
        s.media_url = xaddr("Media");
        s.ptz_url = xaddr("PTZ");
        s.imaging_url = xaddr("Imaging");
        s.events_url = xaddr("Events");
        s.analytics_url = xaddr("Analytics");
        s.events = s.events_url.is_some();
        let media = s.media_url.clone().ok_or_else(|| anyhow!("pas de service vidéo ONVIF"))?;
        let body = s.call(&media, &format!(r#"<GetProfiles xmlns="{NS_MEDIA}"/>"#), true, "GetProfilesResponse")?;
        s.profiles = parse_profiles(&body);
        Ok(s)
    }

    pub fn ptz_supported(&self) -> bool {
        self.ptz_url.is_some() && self.profiles.iter().any(|p| p.ptz)
    }

    fn ptz_profile(&self) -> Result<(&str, &str)> {
        let url = self.ptz_url.as_deref().ok_or_else(|| anyhow!("cette caméra n'est pas orientable (pas de PTZ ONVIF)"))?;
        let p = self.profiles.iter().find(|p| p.ptz).or(self.profiles.first()).ok_or_else(|| anyhow!("aucun profil vidéo"))?;
        Ok((url, p.token.as_str()))
    }

    pub fn device_info(&self) -> Result<DeviceInfo> {
        let body = self.call(&self.device_url, &format!(r#"<GetDeviceInformation xmlns="{NS_DEVICE}"/>"#), true, "GetDeviceInformationResponse")?;
        parse_device_info(&body).ok_or_else(|| anyhow!("réponse illisible"))
    }

    /// Mouvement continu (vitesses −1…1) jusqu'à `stop`.
    pub fn continuous_move(&self, pan: f32, tilt: f32) -> Result<()> {
        let (url, token) = self.ptz_profile()?;
        let body = format!(
            r#"<ContinuousMove xmlns="{NS_PTZ}"><ProfileToken>{}</ProfileToken><Velocity><PanTilt x="{pan:.2}" y="{tilt:.2}" xmlns="{NS_SCHEMA}"/></Velocity></ContinuousMove>"#,
            escape(token)
        );
        self.call(url, &body, true, "ContinuousMoveResponse").map(|_| ())
    }

    /// Déplacement relatif (part de toute la course, −1…1) : pour les caméras qui refusent le
    /// mouvement continu, répété tant que l'on veut tourner.
    pub fn relative_move(&self, pan: f32, tilt: f32) -> Result<()> {
        let (url, token) = self.ptz_profile()?;
        let body = format!(
            r#"<RelativeMove xmlns="{NS_PTZ}"><ProfileToken>{}</ProfileToken><Translation><PanTilt x="{pan:.3}" y="{tilt:.3}" xmlns="{NS_SCHEMA}"/></Translation></RelativeMove>"#,
            escape(token)
        );
        self.call(url, &body, true, "RelativeMoveResponse").map(|_| ())
    }

    pub fn stop(&self) -> Result<()> {
        let (url, token) = self.ptz_profile()?;
        let body = format!(r#"<Stop xmlns="{NS_PTZ}"><ProfileToken>{}</ProfileToken><PanTilt>true</PanTilt><Zoom>true</Zoom></Stop>"#, escape(token));
        self.call(url, &body, true, "StopResponse").map(|_| ())
    }

    fn imaging(&self) -> Result<(&str, &str)> {
        let url = self.imaging_url.as_deref().ok_or_else(|| anyhow!("pas de réglages d'image ONVIF sur cette caméra"))?;
        let source = self.profiles.iter().map(|p| p.source.as_str()).find(|s| !s.is_empty()).ok_or_else(|| anyhow!("capteur vidéo inconnu"))?;
        Ok((url, source))
    }

    pub fn imaging_settings(&self) -> Result<ImagingSettings> {
        let (url, source) = self.imaging()?;
        let body = self.call(url, &format!(r#"<GetImagingSettings xmlns="{NS_IMAGING}"><VideoSourceToken>{}</VideoSourceToken></GetImagingSettings>"#, escape(source)), true, "GetImagingSettingsResponse")?;
        Ok(parse_imaging_settings(&body))
    }

    pub fn imaging_options(&self) -> Result<ImagingOptions> {
        let (url, source) = self.imaging()?;
        let body = self.call(url, &format!(r#"<GetOptions xmlns="{NS_IMAGING}"><VideoSourceToken>{}</VideoSourceToken></GetOptions>"#, escape(source)), true, "GetOptionsResponse")?;
        Ok(parse_imaging_options(&body))
    }

    /// Applique les réglages renseignés (les autres ne sont pas touchés).
    pub fn set_imaging(&self, s: &ImagingSettings) -> Result<()> {
        let (url, source) = self.imaging()?;
        let mut x = String::new();
        // Ordre imposé par le schéma ONVIF.
        if let Some(m) = &s.backlight {
            x.push_str(&format!("<BacklightCompensation xmlns=\"{NS_SCHEMA}\"><Mode>{}</Mode></BacklightCompensation>", escape(m)));
        }
        for (tag, v) in [("Brightness", s.brightness), ("ColorSaturation", s.saturation), ("Contrast", s.contrast)] {
            if let Some(v) = v {
                x.push_str(&format!("<{tag} xmlns=\"{NS_SCHEMA}\">{v}</{tag}>"));
            }
        }
        if let Some(m) = &s.ir_cut {
            x.push_str(&format!("<IrCutFilter xmlns=\"{NS_SCHEMA}\">{}</IrCutFilter>", escape(m)));
        }
        if let Some(v) = s.sharpness {
            x.push_str(&format!("<Sharpness xmlns=\"{NS_SCHEMA}\">{v}</Sharpness>"));
        }
        if let Some(m) = &s.wdr {
            let level = s.wdr_level.map(|l| format!("<Level>{l}</Level>")).unwrap_or_default();
            x.push_str(&format!("<WideDynamicRange xmlns=\"{NS_SCHEMA}\"><Mode>{}</Mode>{level}</WideDynamicRange>", escape(m)));
        }
        let body = format!(
            r#"<SetImagingSettings xmlns="{NS_IMAGING}"><VideoSourceToken>{}</VideoSourceToken><ImagingSettings>{x}</ImagingSettings><ForcePersistence>true</ForcePersistence></SetImagingSettings>"#,
            escape(source)
        );
        self.call(url, &body, true, "SetImagingSettingsResponse").map(|_| ())
    }

    /// Adresse RTSP d'un profil (`GetStreamUri`).
    pub fn stream_uri(&self, profile: &str) -> Result<String> {
        let media = self.media_url.as_deref().ok_or_else(|| anyhow!("pas de service vidéo ONVIF"))?;
        let body = format!(
            r#"<GetStreamUri xmlns="{NS_MEDIA}"><StreamSetup><Stream xmlns="{NS_SCHEMA}">RTP-Unicast</Stream><Transport xmlns="{NS_SCHEMA}"><Protocol>RTSP</Protocol></Transport></StreamSetup><ProfileToken>{}</ProfileToken></GetStreamUri>"#,
            escape(profile)
        );
        let r = self.call(media, &body, true, "GetStreamUriResponse")?;
        xml::element(&r, "MediaUri").and_then(|m| m.find_text("Uri")).filter(|u| !u.is_empty()).ok_or_else(|| anyhow!("adresse du flux absente"))
    }

    /// Adresses RTSP du flux principal (plus grande résolution) et du flux secondaire.
    pub fn stream_uris(&self) -> Result<(String, Option<String>)> {
        let mut profiles = self.profiles.clone();
        profiles.sort_by_key(|p| std::cmp::Reverse(p.width * p.height));
        let main = profiles.first().ok_or_else(|| anyhow!("aucun profil vidéo"))?;
        let main_uri = self.stream_uri(&main.token)?;
        let sub_uri = profiles.last().filter(|p| p.token != main.token).and_then(|p| self.stream_uri(&p.token).ok());
        Ok((main_uri, sub_uri))
    }

    /// Réglages d'encodage actuels de chaque flux.
    pub fn encoder_configs(&self) -> Result<Vec<EncoderConfig>> {
        let media = self.media_url.as_deref().ok_or_else(|| anyhow!("pas de service vidéo ONVIF"))?;
        let body = self.call(media, &format!(r#"<GetVideoEncoderConfigurations xmlns="{NS_MEDIA}"/>"#), true, "GetVideoEncoderConfigurationsResponse")?;
        Ok(parse_encoder_configs(&body))
    }

    /// Valeurs possibles (résolutions, cadence, débit) pour un réglage d'encodage.
    pub fn encoder_options(&self, config_token: &str) -> Result<EncoderOptions> {
        let media = self.media_url.as_deref().ok_or_else(|| anyhow!("pas de service vidéo ONVIF"))?;
        let body = self.call(
            media,
            &format!(r#"<GetVideoEncoderConfigurationOptions xmlns="{NS_MEDIA}"><ConfigurationToken>{}</ConfigurationToken></GetVideoEncoderConfigurationOptions>"#, escape(config_token)),
            true,
            "GetVideoEncoderConfigurationOptionsResponse",
        )?;
        Ok(parse_encoder_options(&body))
    }

    /// Change la résolution, la cadence et le débit d'un flux (le reste du réglage est conservé).
    pub fn set_encoder(&self, c: &EncoderConfig) -> Result<()> {
        let media = self.media_url.as_deref().ok_or_else(|| anyhow!("pas de service vidéo ONVIF"))?;
        let profile = if c.profile.is_empty() { "Main" } else { c.profile.as_str() };
        let body = format!(
            r#"<SetVideoEncoderConfiguration xmlns="{NS_MEDIA}"><Configuration token="{}"><Name xmlns="{NS_SCHEMA}">{}</Name><UseCount xmlns="{NS_SCHEMA}">1</UseCount><Encoding xmlns="{NS_SCHEMA}">H264</Encoding><Resolution xmlns="{NS_SCHEMA}"><Width>{}</Width><Height>{}</Height></Resolution><Quality xmlns="{NS_SCHEMA}">{}</Quality><RateControl xmlns="{NS_SCHEMA}"><FrameRateLimit>{}</FrameRateLimit><EncodingInterval>1</EncodingInterval><BitrateLimit>{}</BitrateLimit></RateControl><H264 xmlns="{NS_SCHEMA}"><GovLength>{}</GovLength><H264Profile>{profile}</H264Profile></H264><Multicast xmlns="{NS_SCHEMA}"><Address><Type>IPv4</Type><IPv4Address>224.1.2.4</IPv4Address></Address><Port>40000</Port><TTL>64</TTL><AutoStart>false</AutoStart></Multicast><SessionTimeout xmlns="{NS_SCHEMA}">PT60S</SessionTimeout></Configuration><ForcePersistence>true</ForcePersistence></SetVideoEncoderConfiguration>"#,
            escape(&c.token),
            escape(&c.name),
            c.width,
            c.height,
            c.quality,
            c.fps,
            c.bitrate_kbps,
            c.gov.max(1),
        );
        self.call(media, &body, true, "SetVideoEncoderConfigurationResponse").map(|_| ())
    }

    pub fn presets(&self) -> Result<Vec<Preset>> {
        let (url, token) = self.ptz_profile()?;
        let body = self.call(url, &format!(r#"<GetPresets xmlns="{NS_PTZ}"><ProfileToken>{}</ProfileToken></GetPresets>"#, escape(token)), true, "GetPresetsResponse")?;
        Ok(parse_presets(&body))
    }

    pub fn goto_preset(&self, preset: &str) -> Result<()> {
        let (url, token) = self.ptz_profile()?;
        let body = format!(r#"<GotoPreset xmlns="{NS_PTZ}"><ProfileToken>{}</ProfileToken><PresetToken>{}</PresetToken></GotoPreset>"#, escape(token), escape(preset));
        self.call(url, &body, true, "GotoPresetResponse").map(|_| ())
    }

    /// Mémorise la position actuelle ; renvoie le jeton de la position.
    pub fn set_preset(&self, name: &str) -> Result<String> {
        let (url, token) = self.ptz_profile()?;
        let body = format!(r#"<SetPreset xmlns="{NS_PTZ}"><ProfileToken>{}</ProfileToken><PresetName>{}</PresetName></SetPreset>"#, escape(token), escape(name));
        let r = self.call(url, &body, true, "SetPresetResponse")?;
        Ok(xml::text(&r, "PresetToken").unwrap_or_default())
    }

    pub fn remove_preset(&self, preset: &str) -> Result<()> {
        let (url, token) = self.ptz_profile()?;
        let body = format!(r#"<RemovePreset xmlns="{NS_PTZ}"><ProfileToken>{}</ProfileToken><PresetToken>{}</PresetToken></RemovePreset>"#, escape(token), escape(preset));
        self.call(url, &body, true, "RemovePresetResponse").map(|_| ())
    }

    /// Position d'origine ; si la caméra ne la gère pas (cas des Imou), retour au centre (0, 0).
    pub fn goto_home(&self) -> Result<()> {
        let (url, token) = self.ptz_profile()?;
        let body = format!(r#"<GotoHomePosition xmlns="{NS_PTZ}"><ProfileToken>{}</ProfileToken></GotoHomePosition>"#, escape(token));
        match self.call(url, &body, true, "GotoHomePositionResponse") {
            Ok(_) => Ok(()),
            Err(e) if format!("{e:#}").contains("NotImplemented") || format!("{e:#}").contains("ActionNotSupported") => self.absolute_move(0.0, 0.0),
            Err(e) => Err(e),
        }
    }

    /// Va à une position absolue (−1…1 en panoramique et en inclinaison).
    pub fn absolute_move(&self, pan: f32, tilt: f32) -> Result<()> {
        let (url, token) = self.ptz_profile()?;
        let body = format!(
            r#"<AbsoluteMove xmlns="{NS_PTZ}"><ProfileToken>{}</ProfileToken><Position><PanTilt x="{pan:.3}" y="{tilt:.3}" xmlns="{NS_SCHEMA}"/></Position></AbsoluteMove>"#,
            escape(token)
        );
        self.call(url, &body, true, "AbsoluteMoveResponse").map(|_| ())
    }

    // ---- Événements (abonnement « PullPoint ») -------------------------------------------------

    /// Ouvre un abonnement aux événements de la caméra ; renvoie son adresse.
    pub fn subscribe_events(&self, secs: u32) -> Result<String> {
        let url = self.events_url.as_deref().ok_or_else(|| anyhow!("pas d'événements ONVIF sur cette caméra"))?;
        let body = format!(r#"<CreatePullPointSubscription xmlns="{NS_EVENTS}"><InitialTerminationTime>PT{secs}S</InitialTerminationTime></CreatePullPointSubscription>"#);
        let r = self.call(url, &body, true, "CreatePullPointSubscriptionResponse")?;
        xml::element(&r, "SubscriptionReference").and_then(|e| e.find_text("Address")).filter(|a| !a.is_empty()).ok_or_else(|| anyhow!("adresse d'abonnement absente"))
    }

    /// En-tête WS-Addressing d'une requête vers un abonnement : sans le `To`, les Dahua / Imou
    /// répondent « Resource Unknown » (vérifié sur la Ranger 2C).
    fn addressing(action: &str, to: &str) -> String {
        format!(r#"<wsa:Action xmlns:wsa="{NS_WSA}">{action}</wsa:Action><wsa:To xmlns:wsa="{NS_WSA}">{}</wsa:To>"#, escape(to))
    }

    /// Attend jusqu'à `wait` secondes les événements de l'abonnement.
    pub fn pull_events(&self, sub: &str, wait: u32, limit: u32) -> Result<Vec<EventMessage>> {
        let body = format!(r#"<PullMessages xmlns="{NS_EVENTS}"><Timeout>PT{wait}S</Timeout><MessageLimit>{limit}</MessageLimit></PullMessages>"#);
        let extra = Self::addressing("http://www.onvif.org/ver10/events/wsdl/PullPointSubscription/PullMessagesRequest", sub);
        let r = self.call_full(sub, &body, true, &extra, Duration::from_secs(wait as u64 + 6), "PullMessagesResponse")?;
        Ok(parse_events(&r))
    }

    /// Prolonge l'abonnement.
    pub fn renew_events(&self, sub: &str, secs: u32) -> Result<()> {
        let body = format!(r#"<Renew xmlns="{NS_WSN}"><TerminationTime>PT{secs}S</TerminationTime></Renew>"#);
        let extra = Self::addressing("http://docs.oasis-open.org/wsn/bw-2/SubscriptionManager/RenewRequest", sub);
        self.call_full(sub, &body, true, &extra, Duration::from_secs(5), "RenewResponse").map(|_| ())
    }

    /// Ferme l'abonnement (sans erreur : la caméra l'oublie de toute façon à son échéance).
    pub fn unsubscribe_events(&self, sub: &str) {
        let body = format!(r#"<Unsubscribe xmlns="{NS_WSN}"/>"#);
        let extra = Self::addressing("http://docs.oasis-open.org/wsn/bw-2/SubscriptionManager/UnsubscribeRequest", sub);
        let _ = self.call_full(sub, &body, true, &extra, Duration::from_secs(3), "UnsubscribeResponse");
    }

    // ---- Détection de sabotage intégrée (module d'analyse « TamperEngine ») -------------------

    /// Détecteur de sabotage de la caméra, s'il en a un (lu dans ses profils).
    pub fn tamper_module(&self) -> Result<Option<TamperModule>> {
        let media = self.media_url.as_deref().ok_or_else(|| anyhow!("pas de service vidéo ONVIF"))?;
        let body = self.call(media, &format!(r#"<GetProfiles xmlns="{NS_MEDIA}"/>"#), true, "GetProfilesResponse")?;
        Ok(parse_tamper_module(&body))
    }

    /// Active ou coupe le détecteur de sabotage de la caméra (sensibilité « low », « medium »,
    /// « high »). Forme exacte acceptée par la Tapo C210 (vérifiée) : paramètres dans l'espace
    /// de noms du schéma ONVIF, module sans espace de noms.
    pub fn set_tamper_module(&self, m: &TamperModule, enabled: bool, sensitivity: &str) -> Result<()> {
        let url = self.analytics_url.as_deref().ok_or_else(|| anyhow!("pas de service d'analyse ONVIF sur cette caméra"))?;
        let body = format!(
            r#"<ModifyAnalyticsModules xmlns="{NS_ANALYTICS}"><ConfigurationToken>{}</ConfigurationToken><AnalyticsModule xmlns="" Name="{}" Type="{}"><Parameters xmlns="{NS_SCHEMA}"><SimpleItem Name="Sensitivity" Value="{}"/><SimpleItem Name="Enabled" Value="{}"/></Parameters></AnalyticsModule></ModifyAnalyticsModules>"#,
            escape(&m.config),
            escape(&m.name),
            escape(&m.kind),
            escape(sensitivity),
            if enabled { "on" } else { "off" },
        );
        self.call(url, &body, true, "ModifyAnalyticsModulesResponse").map(|_| ())
    }

    /// Envoie une requête SOAP et vérifie que la réponse attendue est bien là.
    fn call(&self, url: &str, body: &str, auth: bool, expect: &str) -> Result<String> {
        self.call_full(url, body, auth, "", Duration::from_secs(5), expect)
    }

    /// Requête SOAP, avec des en-têtes en plus (WS-Addressing) et un délai d'attente donné.
    fn call_full(&self, url: &str, body: &str, auth: bool, extra: &str, timeout: Duration, expect: &str) -> Result<String> {
        let mut header = if auth { security_header(&self.user, &self.password, Utc::now() + self.offset) } else { String::new() };
        if !extra.is_empty() {
            header = if header.is_empty() { format!("<s:Header>{extra}</s:Header>") } else { header.replace("</s:Header>", &format!("{extra}</s:Header>")) };
        }
        let mut resp = agent_with(timeout)
            .post(url)
            .header("Content-Type", "application/soap+xml; charset=utf-8")
            .send(envelope(&header, body))
            .with_context(|| format!("connexion à {url}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        if let Some(f) = fault(&text) {
            if f.contains("NotAuthorized") || f.to_lowercase().contains("authority") || status == 401 {
                bail!("identifiants refusés par la caméra");
            }
            bail!("la caméra refuse : {f}");
        }
        if status == 401 {
            bail!("identifiants refusés par la caméra");
        }
        if xml::element(&text, expect).is_none() {
            // Les caméras Dahua / Imou répondent par un corps vide quand l'authentification échoue.
            if auth && xml::element(&text, "Body").is_some_and(|b| b.inner.trim().is_empty()) {
                bail!("identifiants refusés par la caméra");
            }
            bail!("réponse inattendue de la caméra (HTTP {status})");
        }
        Ok(text)
    }
}

/// Les caméras annoncent parfois une adresse interne : on garde le chemin, avec l'hôte réellement joint.
fn rebase(url: &str, hostport: &str) -> String {
    match url.split_once("://").and_then(|(scheme, rest)| rest.find('/').map(|i| (scheme, &rest[i..]))) {
        Some((scheme, path)) => format!("{scheme}://{hostport}{path}"),
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_matches_reference() {
        // Exemple du guide de programmation ONVIF (nonce, date, mot de passe).
        let nonce = B64.decode("LKqI6G/AikKCQrN0zqZFlg==").unwrap();
        assert_eq!(password_digest(&nonce, "2010-09-16T07:50:45Z", "userpassword"), "tuOSpGlFlIXsozq4HFNeeGeFLEI=");
    }

    #[test]
    fn parses_camera_answers() {
        let t = r#"<s:Body><tds:GetSystemDateAndTimeResponse><tds:SystemDateAndTime><tt:UTCDateTime><tt:Time><tt:Hour>14</tt:Hour><tt:Minute>10</tt:Minute><tt:Second>24</tt:Second></tt:Time><tt:Date><tt:Year>2026</tt:Year><tt:Month>10</tt:Month><tt:Day>20</tt:Day></tt:Date></tt:UTCDateTime></tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse></s:Body>"#;
        assert_eq!(parse_utc(t).unwrap().to_rfc3339(), "2026-10-20T14:10:24+00:00");

        let p = r#"<trt:GetProfilesResponse><trt:Profiles token="MediaProfile00000" fixed="true"><tt:Name>MediaProfile_Channel1_MainStream</tt:Name><tt:VideoEncoderConfiguration token="VideoEncoderToken000"><tt:Encoding>H264</tt:Encoding><tt:Resolution><tt:Width>2304</tt:Width><tt:Height>1296</tt:Height></tt:Resolution></tt:VideoEncoderConfiguration><tt:PTZConfiguration token="PtzConfigToken0"><tt:Name>PTZ</tt:Name></tt:PTZConfiguration></trt:Profiles><trt:Profiles token="MediaProfile00001"><tt:Name>Sub</tt:Name></trt:Profiles></trt:GetProfilesResponse>"#;
        let profiles = parse_profiles(p);
        assert_eq!(profiles.len(), 2);
        assert_eq!((profiles[0].width, profiles[0].height, profiles[0].ptz), (2304, 1296, true));
        assert_eq!(profiles[0].encoding, "H264");
        assert!(!profiles[1].ptz);

        let pr = r#"<tptz:GetPresetsResponse><tptz:Preset token="1"><tt:Name>Salon</tt:Name></tptz:Preset><tptz:Preset token="2"></tptz:Preset></tptz:GetPresetsResponse>"#;
        assert_eq!(parse_presets(pr), vec![Preset { token: "1".into(), name: "Salon".into() }, Preset { token: "2".into(), name: "Position 2".into() }]);

        let f = r#"<s:Body><s:Fault><s:Code><s:Value>s:Sender</s:Value><s:Subcode><s:Value>ter:NotAuthorized</s:Value></s:Subcode></s:Code><s:Reason><s:Text xml:lang="en">Sender not Authorized</s:Text></s:Reason></s:Fault></s:Body>"#;
        assert_eq!(fault(f).as_deref(), Some("Sender not Authorized (ter:NotAuthorized)"));
        assert_eq!(rebase("http://10.0.0.5/onvif/ptz_service", "192.168.1.164"), "http://192.168.1.164/onvif/ptz_service");
    }

    #[test]
    fn events_and_tamper_module_of_real_cameras() {
        // Réponses réelles (Tapo C210, Imou Ranger 2C), espaces de noms abrégés.
        let tapo = r#"<SOAP-ENV:Body><tev:PullMessagesResponse><tev:CurrentTime>2026-10-26T03:31:31Z</tev:CurrentTime><wsnt:NotificationMessage><wsnt:Topic Dialect="http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet">tns1:RuleEngine/TamperDetector/Tamper</wsnt:Topic><wsnt:Message><tt:Message UtcTime="2026-10-26T03:31:30Z" PropertyOperation="Changed"><tt:Source><tt:SimpleItem Name="VideoSourceConfigurationToken" Value="vsconf"/><tt:SimpleItem Name="Rule" Value="MyTamperDetectorRule"/></tt:Source><tt:Data><tt:SimpleItem Value="true" Name="IsTamper"></tt:SimpleItem></tt:Data></tt:Message></wsnt:Message></wsnt:NotificationMessage><wsnt:NotificationMessage><wsnt:Topic>tns1:RuleEngine/CellMotionDetector/Motion</wsnt:Topic><wsnt:Message><tt:Message PropertyOperation="Changed"><tt:Data><tt:SimpleItem Value="true" Name="IsMotion"/></tt:Data></tt:Message></wsnt:Message></wsnt:NotificationMessage></tev:PullMessagesResponse></SOAP-ENV:Body>"#;
        let ev = parse_events(tapo);
        assert_eq!(ev.len(), 2);
        assert_eq!((ev[0].tamper(), ev[0].initial()), (Some(true), false));
        assert_eq!(ev[0].source[1], ("Rule".to_string(), "MyTamperDetectorRule".to_string()));
        assert_eq!(ev[1].tamper(), None);
        let imou = r#"<wsnt:NotificationMessage><wsnt:Topic>tns1:VideoSource/GlobalSceneChange/ImagingService</wsnt:Topic><wsnt:Message><tt:Message UtcTime="2026-10-26T03:31:46Z" PropertyOperation="Initialized"><tt:Source><tt:SimpleItem Name="Source" Value="00000"/></tt:Source><tt:Data><tt:SimpleItem Name="State" Value="false"/></tt:Data></tt:Message></wsnt:Message></wsnt:NotificationMessage>"#;
        let ev = parse_events(imou);
        assert_eq!((ev[0].tamper(), ev[0].initial()), (Some(false), true));

        let profiles = r#"<trt:Profiles token="profile_1"><tt:VideoAnalyticsConfiguration token="VideoAnalyticsToken"><tt:Name>VideoAnalyticsName</tt:Name><tt:AnalyticsEngineConfiguration><tt:AnalyticsModule Name="MyCellMotionModule" Type="tt:CellMotionEngine"><tt:Parameters><tt:SimpleItem Value="medium" Name="Sensitivity"></tt:SimpleItem></tt:Parameters></tt:AnalyticsModule><tt:AnalyticsModule Name="MyTamperDetecModule" Type="tt:TamperEngine"><tt:Parameters><tt:SimpleItem Value="medium" Name="Sensitivity"></tt:SimpleItem><tt:SimpleItem Value="off" Name="Enabled"></tt:SimpleItem></tt:Parameters></tt:AnalyticsModule></tt:AnalyticsEngineConfiguration></tt:VideoAnalyticsConfiguration></trt:Profiles>"#;
        let m = parse_tamper_module(profiles).unwrap();
        assert_eq!(
            m,
            TamperModule { config: "VideoAnalyticsToken".into(), name: "MyTamperDetecModule".into(), kind: "tt:TamperEngine".into(), enabled: Some(false), sensitivity: Some("medium".into()) }
        );
        // Imou : seulement la détection de mouvement, pas de détecteur de sabotage réglable.
        let imou = r#"<trt:Configurations token="Analytics000"><tt:AnalyticsEngineConfiguration><tt:AnalyticsModule Name="MyCellMotionEngine" Type="tt:CellMotionEngine"/></tt:AnalyticsEngineConfiguration></trt:Configurations>"#;
        assert!(parse_tamper_module(imou).is_none());
    }
}
