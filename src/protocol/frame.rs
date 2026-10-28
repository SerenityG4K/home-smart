//! Trames binaires Tuya.
//!
//! Format 55AA (3.1 → 3.4) :
//! `000055AA | seq u32 | cmd u32 | len u32 | [retcode u32] | payload | crc32 u32 ou hmac 32o | 0000AA55`
//! (`len` couvre tout ce qui suit l'en-tête, pied compris.)
//!
//! Format 6699 (3.5) :
//! `00006699 | 0000 | seq u32 | cmd u32 | len u32 | iv 12o | AES-GCM([retcode] + payload) | tag 16o | 00009966`
//! (`len` = iv + chiffré + tag ; l'AAD GCM correspond aux octets 4..18 de l'en-tête.)

use super::crypto::{self, Key};
use anyhow::{bail, Result};

pub const PREFIX_55AA: u32 = 0x0000_55AA;
pub const SUFFIX_55AA: u32 = 0x0000_AA55;
pub const PREFIX_6699: u32 = 0x0000_6699;
pub const SUFFIX_6699: u32 = 0x0000_9966;

const PREFIX_55AA_BYTES: [u8; 4] = PREFIX_55AA.to_be_bytes();
const PREFIX_6699_BYTES: [u8; 4] = PREFIX_6699.to_be_bytes();
const MAX_LEN: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct Frame {
    #[allow(dead_code)] // informatif (journalisation, tests)
    pub seq: u32,
    pub cmd: u32,
    pub retcode: Option<u32>,
    /// 55AA : charge utile telle que reçue (éventuellement chiffrée).
    /// 6699 : charge utile déjà déchiffrée.
    pub payload: Vec<u8>,
}

/// Contrôle d'intégrité des trames 55AA.
#[derive(Clone, Copy)]
pub enum Integrity<'a> {
    Crc,
    Hmac(&'a Key),
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

pub fn pack_55aa(seq: u32, cmd: u32, retcode: Option<u32>, payload: &[u8], integrity: Integrity) -> Vec<u8> {
    let footer = match integrity {
        Integrity::Crc => 8,
        Integrity::Hmac(_) => 36,
    };
    let rc_len = if retcode.is_some() { 4 } else { 0 };
    let mut buf = Vec::with_capacity(16 + rc_len + payload.len() + footer);
    buf.extend_from_slice(&PREFIX_55AA.to_be_bytes());
    buf.extend_from_slice(&seq.to_be_bytes());
    buf.extend_from_slice(&cmd.to_be_bytes());
    buf.extend_from_slice(&((rc_len + payload.len() + footer) as u32).to_be_bytes());
    if let Some(rc) = retcode {
        buf.extend_from_slice(&rc.to_be_bytes());
    }
    buf.extend_from_slice(payload);
    match integrity {
        Integrity::Crc => {
            let crc = crypto::crc32(&buf);
            buf.extend_from_slice(&crc.to_be_bytes());
        }
        Integrity::Hmac(key) => {
            let mac = crypto::hmac_sha256(key, &buf);
            buf.extend_from_slice(&mac);
        }
    }
    buf.extend_from_slice(&SUFFIX_55AA.to_be_bytes());
    buf
}

pub fn pack_6699(seq: u32, cmd: u32, retcode: Option<u32>, payload: &[u8], key: &Key) -> Vec<u8> {
    let mut data = Vec::with_capacity(4 + payload.len());
    if let Some(rc) = retcode {
        data.extend_from_slice(&rc.to_be_bytes());
    }
    data.extend_from_slice(payload);
    let len = 12 + data.len() + 16;
    let mut buf = Vec::with_capacity(18 + len + 4);
    buf.extend_from_slice(&PREFIX_6699.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf.extend_from_slice(&seq.to_be_bytes());
    buf.extend_from_slice(&cmd.to_be_bytes());
    buf.extend_from_slice(&(len as u32).to_be_bytes());
    let iv: [u8; 12] = rand::random();
    let tag = crypto::gcm_encrypt(key, &iv, &buf[4..18], &mut data);
    buf.extend_from_slice(&iv);
    buf.extend_from_slice(&data);
    buf.extend_from_slice(&tag);
    buf.extend_from_slice(&SUFFIX_6699.to_be_bytes());
    buf
}

fn find_prefix(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == PREFIX_55AA_BYTES || w == PREFIX_6699_BYTES)
}

/// Options de décodage.
#[derive(Clone, Copy)]
pub struct DecodeOpts<'a> {
    /// Clé HMAC (55AA ≥ 3.4) ou GCM (6699). `None` → CRC32 pour le 55AA.
    pub key: Option<&'a Key>,
    /// Les 55AA de cette session sont signés en HMAC (sinon CRC32).
    pub hmac: bool,
    /// Détecter un code retour en tête de charge (trames émises par un appareil).
    pub detect_retcode: bool,
}

/// Extrait la prochaine trame complète de `buf` (les octets consommés sont retirés).
/// `None` = trame incomplète, il faut lire davantage.
pub fn next_frame(buf: &mut Vec<u8>, opts: DecodeOpts) -> Option<Result<Frame>> {
    match find_prefix(buf) {
        None => {
            // On garde 3 octets au cas où un préfixe serait coupé en deux.
            let keep = buf.len().min(3);
            buf.drain(..buf.len() - keep);
            return None;
        }
        Some(0) => {}
        Some(i) => {
            buf.drain(..i);
        }
    }
    let is_6699 = buf[..4] == PREFIX_6699_BYTES;
    let header = if is_6699 { 18 } else { 16 };
    if buf.len() < header {
        return None;
    }
    let len = be32(&buf[header - 4..header]) as usize;
    if len > MAX_LEN {
        buf.drain(..4);
        return Some(Err(anyhow::anyhow!("trame de longueur aberrante ({len} octets), ignorée")));
    }
    let total = if is_6699 { header + len + 4 } else { header + len };
    if buf.len() < total {
        return None;
    }
    let bytes: Vec<u8> = buf.drain(..total).collect();
    Some(if is_6699 { decode_6699(&bytes, opts) } else { decode_55aa(&bytes, opts) })
}

/// Décode un datagramme ou un tampon contenant exactement une trame.
pub fn decode_one(bytes: &[u8], opts: DecodeOpts) -> Result<Frame> {
    let mut buf = bytes.to_vec();
    match next_frame(&mut buf, opts) {
        Some(r) => r,
        None => bail!("trame incomplète"),
    }
}

fn split_retcode(body: &[u8], detect: bool) -> (Option<u32>, Vec<u8>) {
    // Heuristique (comme tinytuya) : un code retour est un petit entier, donc ses
    // 3 premiers octets sont nuls ; ni du JSON ni un en-tête « 3.x » ne commencent ainsi.
    if detect && body.len() >= 4 && body[..3] == [0, 0, 0] {
        (Some(be32(&body[..4])), body[4..].to_vec())
    } else {
        (None, body.to_vec())
    }
}

fn decode_55aa(bytes: &[u8], opts: DecodeOpts) -> Result<Frame> {
    let total = bytes.len();
    let footer = if opts.hmac { 36 } else { 8 };
    if total < 16 + footer {
        bail!("trame 55AA trop courte");
    }
    if be32(&bytes[total - 4..]) != SUFFIX_55AA {
        bail!("suffixe 55AA invalide");
    }
    let signed = &bytes[..total - footer];
    if opts.hmac {
        let key = opts.key.ok_or_else(|| anyhow::anyhow!("clé HMAC manquante"))?;
        if crypto::hmac_sha256(key, signed)[..] != bytes[total - 36..total - 4] {
            bail!("signature HMAC invalide (clé locale incorrecte ?)");
        }
    } else if crypto::crc32(signed) != be32(&bytes[total - 8..total - 4]) {
        bail!("CRC invalide");
    }
    let (retcode, payload) = split_retcode(&bytes[16..total - footer], opts.detect_retcode);
    Ok(Frame { seq: be32(&bytes[4..8]), cmd: be32(&bytes[8..12]), retcode, payload })
}

fn decode_6699(bytes: &[u8], opts: DecodeOpts) -> Result<Frame> {
    let total = bytes.len();
    let len = be32(&bytes[14..18]) as usize;
    if len < 28 {
        bail!("trame 6699 trop courte");
    }
    if be32(&bytes[total - 4..]) != SUFFIX_6699 {
        bail!("suffixe 6699 invalide");
    }
    let key = opts.key.ok_or_else(|| anyhow::anyhow!("clé GCM manquante"))?;
    let iv = &bytes[18..30];
    let mut data = bytes[30..18 + len - 16].to_vec();
    let tag = &bytes[18 + len - 16..18 + len];
    crypto::gcm_decrypt(key, iv, &bytes[4..18], &mut data, tag)?;
    let (retcode, payload) = split_retcode(&data, opts.detect_retcode);
    Ok(Frame { seq: be32(&bytes[6..10]), cmd: be32(&bytes[10..14]), retcode, payload })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: Key = *b"0123456789abcdef";

    #[test]
    fn roundtrip_55aa_crc_and_hmac() {
        for integrity in [Integrity::Crc, Integrity::Hmac(&KEY)] {
            let hmac = matches!(integrity, Integrity::Hmac(_));
            let raw = pack_55aa(42, 7, Some(0), b"{\"a\":1}", integrity);
            let f = decode_one(&raw, DecodeOpts { key: Some(&KEY), hmac, detect_retcode: true }).unwrap();
            assert_eq!((f.seq, f.cmd, f.retcode), (42, 7, Some(0)));
            assert_eq!(f.payload, b"{\"a\":1}");
        }
    }

    #[test]
    fn roundtrip_6699() {
        let raw = pack_6699(5, 13, None, b"3.5\0\0\0\0\0\0\0\0\0\0\0\0{}", &KEY);
        let f = decode_one(&raw, DecodeOpts { key: Some(&KEY), hmac: true, detect_retcode: true }).unwrap();
        assert_eq!((f.seq, f.cmd, f.retcode), (5, 13, None));
        assert!(f.payload.starts_with(b"3.5"));
    }

    #[test]
    fn stream_resync_and_partial() {
        let a = pack_55aa(1, 9, None, b"", Integrity::Crc);
        let b = pack_55aa(2, 8, None, b"{}", Integrity::Crc);
        let mut buf = vec![1, 2, 3];
        buf.extend_from_slice(&a);
        buf.extend_from_slice(&b[..10]);
        let opts = DecodeOpts { key: None, hmac: false, detect_retcode: false };
        assert_eq!(next_frame(&mut buf, opts).unwrap().unwrap().seq, 1);
        assert!(next_frame(&mut buf, opts).is_none());
        buf.extend_from_slice(&b[10..]);
        assert_eq!(next_frame(&mut buf, opts).unwrap().unwrap().seq, 2);
        assert!(buf.is_empty());
    }

    #[test]
    fn wrong_key_is_rejected() {
        let raw = pack_6699(1, 8, None, b"{}", &KEY);
        let bad = *b"fedcba9876543210";
        assert!(decode_one(&raw, DecodeOpts { key: Some(&bad), hmac: true, detect_retcode: true }).is_err());
    }
}
