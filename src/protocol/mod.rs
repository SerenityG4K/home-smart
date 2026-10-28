//! Protocole local Tuya (LAN, port TCP 6668), versions 3.1 à 3.5.
//!
//! Trois couches :
//! - `crypto` : AES-ECB/GCM, HMAC, MD5, CRC32 ;
//! - `frame`  : découpage des trames binaires 55AA (≤ 3.4) et 6699 (3.5) ;
//! - `codec`  : chiffrement des charges utiles selon la version + commandes JSON.

pub mod codec;
pub mod crypto;
pub mod frame;

use std::fmt;
use std::str::FromStr;

/// Numéros de commande Tuya utilisés ici.
pub mod cmd {
    pub const SESS_KEY_NEG_START: u32 = 3;
    pub const SESS_KEY_NEG_RESP: u32 = 4;
    pub const SESS_KEY_NEG_FINISH: u32 = 5;
    pub const CONTROL: u32 = 7;
    pub const STATUS: u32 = 8;
    pub const HEART_BEAT: u32 = 9;
    pub const DP_QUERY: u32 = 10;
    pub const CONTROL_NEW: u32 = 13;
    pub const DP_QUERY_NEW: u32 = 16;
    pub const UPDATEDPS: u32 = 18;
    pub const UDP_NEW: u32 = 19;
    pub const REQ_DEVINFO: u32 = 37;

    /// Commandes dont la charge utile n'a jamais l'en-tête « 3.x » + 12 octets.
    pub fn no_version_header(cmd: u32) -> bool {
        matches!(
            cmd,
            DP_QUERY | DP_QUERY_NEW | UPDATEDPS | HEART_BEAT | SESS_KEY_NEG_START | SESS_KEY_NEG_RESP | SESS_KEY_NEG_FINISH
        )
    }
}

pub const PORT_TCP: u16 = 6668;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Version {
    V31,
    V32,
    V33,
    V34,
    V35,
}

impl Version {
    pub fn as_str(self) -> &'static str {
        match self {
            Version::V31 => "3.1",
            Version::V32 => "3.2",
            Version::V33 => "3.3",
            Version::V34 => "3.4",
            Version::V35 => "3.5",
        }
    }

    /// Les versions 3.4+ négocient une clé de session et signent en HMAC/GCM.
    pub fn needs_session(self) -> bool {
        self >= Version::V34
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Version {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(match s.trim().trim_start_matches(['v', 'V']) {
            "3.1" => Version::V31,
            "3.2" => Version::V32,
            "3.3" | "" => Version::V33,
            "3.4" => Version::V34,
            "3.5" => Version::V35,
            other => anyhow::bail!("version de protocole inconnue : {other} (attendu 3.1 à 3.5)"),
        })
    }
}
