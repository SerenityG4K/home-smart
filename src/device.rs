//! Connexion TCP à un appareil : négociation, lecture et écriture en parallèle.

use crate::protocol::codec::{self, Codec};
use crate::protocol::crypto::Key;
use crate::protocol::frame::{self, Frame};
use crate::protocol::{cmd, Version};
use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::time::{timeout, Instant};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(4);

/// Tout ce qu'il faut pour joindre un appareil.
#[derive(Clone, Debug)]
pub struct Target {
    pub id: String,
    pub key: Key,
    pub ip: String,
    pub port: u16,
    pub version: Version,
}

/// Message décodé reçu d'un appareil.
#[derive(Debug)]
pub struct Message {
    pub cmd: u32,
    pub retcode: Option<u32>,
    pub json: Option<Value>,
    pub text: String,
}

impl Message {
    pub fn dps(&self) -> Option<&Map<String, Value>> {
        self.json.as_ref().and_then(codec::extract_dps)
    }
}

pub struct Reader {
    half: OwnedReadHalf,
    buf: Vec<u8>,
    pub codec: Codec,
}

pub struct Writer {
    half: OwnedWriteHalf,
    seq: u32,
    pub codec: Codec,
}

impl Reader {
    /// Lit la prochaine trame. Annulable sans perte (utilisable dans `select!`).
    pub async fn next_frame(&mut self) -> Result<Frame> {
        loop {
            if let Some(res) = frame::next_frame(&mut self.buf, self.codec.decode_opts(true)) {
                return res;
            }
            if self.half.read_buf(&mut self.buf).await? == 0 {
                bail!("connexion fermée par l'appareil");
            }
        }
    }

    /// Lit et déchiffre le prochain message. Annulable sans perte.
    pub async fn next(&mut self) -> Result<Message> {
        let f = self.next_frame().await?;
        let plain = match self.codec.decode_payload(&f) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("charge illisible (cmd {}) : {e}", f.cmd);
                Vec::new()
            }
        };
        let json = if plain.first() == Some(&b'{') { serde_json::from_slice(&plain).ok() } else { None };
        Ok(Message { cmd: f.cmd, retcode: f.retcode, json, text: String::from_utf8_lossy(&plain).into_owned() })
    }
}

impl Writer {
    pub async fn send(&mut self, command: u32, payload: &[u8]) -> Result<()> {
        self.seq = self.seq.wrapping_add(1);
        let raw = self.codec.encode(self.seq, command, payload, None);
        self.half.write_all(&raw).await.context("écriture vers l'appareil")?;
        Ok(())
    }

    pub async fn query(&mut self, dps_hint: &[String]) -> Result<()> {
        let (c, p) = self.codec.query(dps_hint);
        self.send(c, &p).await
    }

    /// Requête d'état complète ; pour un device22 sans DP connus, sonde les plages usuelles.
    pub async fn query_all(&mut self, dps_hint: &[String]) -> Result<()> {
        if self.codec.dev22 && dps_hint.is_empty() {
            self.probe_dev22().await
        } else {
            self.query(dps_hint).await
        }
    }

    pub async fn probe_dev22(&mut self) -> Result<()> {
        for (c, p) in self.codec.dev22_probe() {
            self.send(c, &p).await?;
        }
        Ok(())
    }

    pub async fn control(&mut self, dps: &Map<String, Value>) -> Result<()> {
        let (c, p) = self.codec.control(dps);
        self.send(c, &p).await
    }

    pub async fn heartbeat(&mut self) -> Result<()> {
        let (c, p) = self.codec.heartbeat();
        self.send(c, &p).await
    }

    pub async fn update_dps(&mut self, ids: &[u32]) -> Result<()> {
        let (c, p) = self.codec.update_dps(ids);
        self.send(c, &p).await
    }
}

/// Ouvre la connexion et, en 3.4/3.5, négocie la clé de session.
pub async fn connect(t: &Target) -> Result<(Reader, Writer)> {
    let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect((t.ip.as_str(), t.port)))
        .await
        .map_err(|_| anyhow::anyhow!("délai de connexion dépassé ({}:{})", t.ip, t.port))?
        .with_context(|| format!("connexion à {}:{}", t.ip, t.port))?;
    stream.set_nodelay(true).ok();
    let (rh, wh) = stream.into_split();
    let codec = Codec::new(t.version, &t.id, t.key);
    let mut reader = Reader { half: rh, buf: Vec::with_capacity(1024), codec: codec.clone() };
    let mut writer = Writer { half: wh, seq: 0, codec };

    if t.version.needs_session() {
        negotiate(&mut reader, &mut writer).await.map_err(|e| {
            let msg = format!("{e:#}");
            if msg.contains("clé locale") || msg.contains("version") {
                e
            } else {
                anyhow::anyhow!("négociation refusée ({msg}) : clé locale incorrecte ou mauvaise version de protocole ?")
            }
        })?;
    }
    Ok((reader, writer))
}

async fn negotiate(reader: &mut Reader, writer: &mut Writer) -> Result<()> {
    let local_nonce = random_nonce();
    writer.send(cmd::SESS_KEY_NEG_START, &local_nonce).await?;
    let resp = timeout(REPLY_TIMEOUT, async {
        loop {
            let f = reader.next_frame().await?;
            if f.cmd == cmd::SESS_KEY_NEG_RESP {
                return Ok::<_, anyhow::Error>(f);
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("l'appareil n'a pas répondu à la négociation (mauvaise version de protocole ?)"))??;
    let remote = writer.codec.check_negotiation(&resp, &local_nonce)?;
    let finish = writer.codec.negotiation_finish(&remote);
    writer.send(cmd::SESS_KEY_NEG_FINISH, &finish).await?;
    writer.codec.set_session(&local_nonce, &remote);
    reader.codec = writer.codec.clone();
    Ok(())
}

/// Nonce de 16 caractères alphanumériques (certains firmwares préfèrent l'ASCII).
fn random_nonce() -> [u8; 16] {
    use rand::Rng;
    const ALPHA: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut rng = rand::thread_rng();
    let mut n = [0u8; 16];
    for b in n.iter_mut() {
        *b = ALPHA[rng.gen_range(0..ALPHA.len())];
    }
    n
}

fn is_dev22_error(m: &Message) -> bool {
    m.text.contains("data unvalid")
}

/// Lecture ponctuelle de l'état (utilisée par la ligne de commande).
pub async fn fetch_status(t: &Target) -> Result<Map<String, Value>> {
    let (mut reader, mut writer) = connect(t).await?;
    writer.query_all(&[]).await?;
    let mut deadline = Instant::now() + REPLY_TIMEOUT;
    let mut dps = Map::new();
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        let msg = match timeout(wait, reader.next()).await {
            Ok(m) => m?,
            Err(_) if !dps.is_empty() => return Ok(dps),
            Err(_) => bail!("l'appareil n'a pas renvoyé son état"),
        };
        if is_dev22_error(&msg) && !writer.codec.dev22 {
            writer.codec.dev22 = true;
            reader.codec.dev22 = true;
            writer.probe_dev22().await?;
            continue;
        }
        if let Some(d) = msg.dps() {
            dps.extend(d.clone());
            if !writer.codec.dev22 {
                return Ok(dps);
            }
            // device22 : les réponses arrivent par morceaux, on laisse un court délai.
            deadline = deadline.min(Instant::now() + Duration::from_millis(400));
        }
    }
}

/// Écriture ponctuelle de DP. Renvoie les DP confirmés par l'appareil, s'il en envoie.
pub async fn set_dps(t: &Target, dps: &Map<String, Value>) -> Result<Map<String, Value>> {
    let (mut reader, mut writer) = connect(t).await?;
    writer.control(dps).await?;
    let mut confirmed = Map::new();
    let deadline = Instant::now() + Duration::from_millis(1500);
    while let Ok(Ok(msg)) = timeout(deadline.saturating_duration_since(Instant::now()), reader.next()).await {
        if let Some(d) = msg.dps() {
            confirmed.extend(d.clone());
            if dps.keys().all(|k| confirmed.contains_key(k)) {
                break;
            }
        }
        if msg.retcode.is_some_and(|rc| rc != 0) {
            bail!("l'appareil a refusé la commande (code {}) {}", msg.retcode.unwrap(), msg.text);
        }
    }
    Ok(confirmed)
}
