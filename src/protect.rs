//! Protection au repos des données sensibles : les empreintes des visages, et les clés et mots de
//! passe de la configuration (`config::Secrets`).
//!
//! - **Windows** : DPAPI (`CryptProtectData`, portée utilisateur). La clé dépend du compte Windows
//!   et de son mot de passe : les données ne se relisent qu'avec ce compte, sur ce PC.
//! - **Linux** : une clé AES-256 tirée au hasard est rangée dans le trousseau de la session
//!   (Secret Service : GNOME Keyring, KWallet, KeePassXC…) ; les données sont chiffrées en
//!   AES-256-GCM avec cette clé.
//! - **Sans trousseau, ou ailleurs** : rien n'est chiffré, et l'appelant le dit. Une clé posée à
//!   côté du fichier ne protégerait de rien : on ne fait pas semblant.

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

/// Comment des données sont chiffrées.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protection {
    /// DPAPI, portée utilisateur (Windows).
    Dpapi,
    /// AES-256-GCM, clé dans le trousseau de la session (Secret Service, Linux).
    SecretService,
}

impl Protection {
    /// Nom rangé dans le fichier.
    pub fn id(self) -> &'static str {
        match self {
            Protection::Dpapi => "dpapi",
            Protection::SecretService => "secret-service",
        }
    }

    pub fn from_id(id: &str) -> Option<Protection> {
        [Protection::Dpapi, Protection::SecretService].into_iter().find(|p| p.id() == id)
    }

    /// Pour le journal.
    pub fn describe(self) -> &'static str {
        match self {
            Protection::Dpapi => "DPAPI, liée à ton compte Windows",
            Protection::SecretService => "AES-256-GCM, clé dans le trousseau de ta session",
        }
    }
}

/// Chiffre `plain` avec la protection de ce système. `what` nomme les données (« faces ») et
/// sépare leur clé de celle d'autres données. `Ok(None)` : aucune protection disponible.
pub fn seal(what: &str, plain: &[u8]) -> Result<Option<(Protection, Vec<u8>)>> {
    #[cfg(windows)]
    return dpapi::protect(what, plain).map(|c| Some((Protection::Dpapi, c)));
    #[cfg(target_os = "linux")]
    return keyring::seal(what, plain).map(|c| c.map(|c| (Protection::SecretService, c)));
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (what, plain);
        Ok(None)
    }
}

/// Déchiffre ce que `seal` a produit.
pub fn open(protection: Protection, what: &str, sealed: &[u8]) -> Result<Vec<u8>> {
    match protection {
        #[cfg(windows)]
        Protection::Dpapi => dpapi::unprotect(what, sealed),
        #[cfg(target_os = "linux")]
        Protection::SecretService => keyring::open(what, sealed),
        p => {
            let _ = (what, sealed);
            bail!("données chiffrées par « {} », qui n'existe pas sur ce système", p.id())
        }
    }
}

/// Comment des données sont rangées sur le disque.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stored {
    /// Pas encore de fichier.
    Nothing,
    /// En clair : ancien format, ou aucune protection sur ce système.
    Clear,
    Sealed(Protection),
}

/// Fichier chiffré sur le disque : la protection employée et le contenu chiffré (base64).
#[derive(Serialize, Deserialize)]
pub(crate) struct SealedFile {
    pub protection: String,
    pub data: String,
}

/// Lit un fichier écrit par `write_file` : son contenu en clair et comment il était rangé, `None`
/// s'il n'existe pas. Un JSON qui n'est pas une enveloppe chiffrée est rendu tel quel (`Clear` :
/// ancien format en clair). Un fichier abîmé, ou chiffré par un autre compte ou sur un autre PC,
/// est une erreur : à l'appelant de ne pas l'écraser.
pub fn read_file(path: &Path, what: &str) -> Result<Option<(Vec<u8>, Stored)>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("lecture de {}", path.display())),
    };
    let v: serde_json::Value = serde_json::from_slice(&bytes).context("fichier abîmé (JSON invalide)")?;
    if v.get("protection").is_none() {
        return Ok(Some((bytes, Stored::Clear)));
    }
    let f: SealedFile = serde_json::from_value(v).context("enveloppe chiffrée abîmée")?;
    let p = Protection::from_id(&f.protection).ok_or_else(|| anyhow!("protection inconnue « {} »", f.protection))?;
    let sealed = B64.decode(f.data.as_bytes()).context("contenu chiffré abîmé (base64)")?;
    Ok(Some((open(p, what, &sealed)?, Stored::Sealed(p))))
}

/// Écrit `plain` chiffré si le système le permet, sinon en clair, de façon atomique (fichier
/// temporaire puis renommage) et lisible par ton seul compte ; renvoie ce qui a été fait.
pub fn write_file(path: &Path, what: &str, plain: &[u8]) -> Result<Stored> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let (bytes, stored) = match seal(what, plain)? {
        Some((p, sealed)) => (serde_json::to_vec(&SealedFile { protection: p.id().to_string(), data: B64.encode(sealed) })?, Stored::Sealed(p)),
        None => (plain.to_vec(), Stored::Clear),
    };
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    write_private(&tmp, &bytes).with_context(|| format!("écriture de {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("écriture de {}", path.display()))?;
    Ok(stored)
}

/// Écrit un fichier lisible par ton seul compte (droits 600 sous Linux ; sous Windows, le
/// dossier du profil l'est déjà).
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut f = options.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

#[cfg(windows)]
mod dpapi {
    use anyhow::{Context, Result};
    use std::ptr;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB};

    fn blob(data: &[u8]) -> Result<CRYPT_INTEGER_BLOB> {
        let len = u32::try_from(data.len()).context("données trop grandes pour DPAPI")?;
        Ok(CRYPT_INTEGER_BLOB { cbData: len, pbData: data.as_ptr().cast_mut() })
    }

    /// Entropie propre à HomeSmart et aux données : pas un secret, mais un autre programme lancé
    /// sous ton compte doit la connaître pour déchiffrer par mégarde.
    fn entropy(what: &str) -> Vec<u8> {
        format!("HomeSmart/{what}/1").into_bytes()
    }

    /// Copie le résultat de DPAPI, puis l'efface et le rend à Windows.
    ///
    /// # Safety
    /// `out` vient d'un appel DPAPI réussi (mémoire allouée par `LocalAlloc`).
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        ptr::write_bytes(out.pbData, 0, out.cbData as usize);
        LocalFree(out.pbData.cast());
        v
    }

    pub fn protect(what: &str, plain: &[u8]) -> Result<Vec<u8>> {
        let (input, entropy) = (blob(plain)?, entropy(what));
        let label: Vec<u16> = format!("HomeSmart ({what})").encode_utf16().chain([0]).collect();
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: ptr::null_mut() };
        // SAFETY : entrées valides pendant l'appel ; `out` est rendu à Windows par `take`.
        unsafe {
            if CryptProtectData(&input, label.as_ptr(), &blob(&entropy)?, ptr::null(), ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out) == 0 {
                return Err(std::io::Error::last_os_error()).context("chiffrement DPAPI");
            }
            Ok(take(out))
        }
    }

    pub fn unprotect(what: &str, sealed: &[u8]) -> Result<Vec<u8>> {
        let (input, entropy) = (blob(sealed)?, entropy(what));
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: ptr::null_mut() };
        // SAFETY : comme `protect`.
        unsafe {
            if CryptUnprotectData(&input, ptr::null_mut(), &blob(&entropy)?, ptr::null(), ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out) == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("déchiffrement DPAPI impossible : fichier abîmé, ou chiffré par un autre compte Windows ou sur un autre PC");
            }
            Ok(take(out))
        }
    }
}

/// AES-256-GCM : nonce aléatoire de 12 octets en tête, `what` authentifié avec le contenu.
#[cfg(any(target_os = "linux", test))]
mod aead {
    use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
    use aes_gcm::{Aes256Gcm, Nonce};
    use anyhow::{anyhow, bail, Result};

    const NONCE: usize = 12;
    const TAG: usize = 16;

    fn aad(what: &str) -> Vec<u8> {
        format!("HomeSmart/{what}/1").into_bytes()
    }

    pub fn new_key() -> Vec<u8> {
        Aes256Gcm::generate_key(OsRng).to_vec()
    }

    pub fn encrypt(key: &[u8], what: &str, plain: &[u8]) -> Result<Vec<u8>> {
        let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| anyhow!("clé AES-256 invalide"))?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let sealed = cipher.encrypt(&nonce, Payload { msg: plain, aad: &aad(what) }).map_err(|_| anyhow!("chiffrement AES-GCM"))?;
        Ok([nonce.as_slice(), sealed.as_slice()].concat())
    }

    pub fn decrypt(key: &[u8], what: &str, sealed: &[u8]) -> Result<Vec<u8>> {
        let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| anyhow!("clé AES-256 invalide"))?;
        if sealed.len() < NONCE + TAG {
            bail!("données chiffrées tronquées");
        }
        let (nonce, body) = sealed.split_at(NONCE);
        cipher
            .decrypt(Nonce::from_slice(nonce), Payload { msg: body, aad: &aad(what) })
            .map_err(|_| anyhow!("déchiffrement impossible : fichier abîmé, ou clé du trousseau différente"))
    }
}

#[cfg(target_os = "linux")]
mod keyring {
    use super::aead;
    use anyhow::{anyhow, bail, Context, Result};
    use secret_service::blocking::SecretService;
    use secret_service::{EncryptionType, Error};
    use std::collections::HashMap;

    fn attributes(what: &str) -> HashMap<&str, &str> {
        HashMap::from([("application", "homesmart"), ("purpose", what)])
    }

    /// Pas de trousseau : pas de bus de session, ou personne pour répondre au nom
    /// `org.freedesktop.secrets` (aucun service installé, ou qui ne démarre pas).
    fn absent(e: &Error) -> bool {
        use zbus::DBusError;
        let name = match e {
            Error::Unavailable => return true,
            Error::Zbus(zbus::Error::MethodError(name, ..)) => name.to_string(),
            Error::Zbus(zbus::Error::FDO(e)) => e.name().to_string(),
            Error::ZbusFdo(e) => e.name().to_string(),
            _ => return false,
        };
        let name = name.trim_start_matches("org.freedesktop.DBus.Error.");
        name == "ServiceUnknown" || name == "NameHasNoOwner" || name.starts_with("Spawn.")
    }

    /// Clé de `what` rangée dans le trousseau par défaut ; si `create`, tirée au hasard et rangée
    /// quand elle manque. `Ok(None)` : pas de trousseau sur ce système.
    fn key(what: &str, create: bool) -> Result<Option<Vec<u8>>> {
        let ss = match SecretService::connect(EncryptionType::Dh) {
            Ok(ss) => ss,
            Err(e) if absent(&e) => return Ok(None),
            Err(e) => return Err(e).context("trousseau (Secret Service)"),
        };
        // Le trousseau par défaut (« login ») survit à la session, contrairement à « session ».
        let collection = match ss.get_default_collection() {
            Ok(c) => c,
            Err(Error::NoResult) => return Ok(None),
            Err(e) => return Err(e).context("trousseau par défaut"),
        };
        if collection.is_locked()? {
            collection.unlock().context("trousseau verrouillé")?;
        }
        if let Some(item) = collection.search_items(attributes(what))?.first() {
            if item.is_locked()? {
                item.unlock().context("clé verrouillée dans le trousseau")?;
            }
            let key = item.get_secret()?;
            if key.len() != 32 {
                bail!("clé de « {what} » abîmée dans le trousseau");
            }
            return Ok(Some(key));
        }
        if !create {
            bail!("clé de « {what} » introuvable dans le trousseau (trousseau réinitialisé, ou autre compte ?)");
        }
        let key = aead::new_key();
        let label = format!("HomeSmart : clé de chiffrement ({what})");
        collection.create_item(&label, attributes(what), &key, false, "application/octet-stream").context("rangement de la clé dans le trousseau")?;
        Ok(Some(key))
    }

    pub fn seal(what: &str, plain: &[u8]) -> Result<Option<Vec<u8>>> {
        match key(what, true)? {
            Some(key) => aead::encrypt(&key, what, plain).map(Some),
            None => Ok(None),
        }
    }

    pub fn open(what: &str, sealed: &[u8]) -> Result<Vec<u8>> {
        let key = key(what, false)?.ok_or_else(|| anyhow!("aucun trousseau (Secret Service) sur ce système : la clé de ces données y est rangée"))?;
        aead::decrypt(&key, what, sealed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aead_round_trip_and_tamper() {
        let key = aead::new_key();
        let sealed = aead::encrypt(&key, "faces", b"empreintes").unwrap();
        assert!(!sealed.windows(10).any(|w| w == b"empreintes"));
        assert_eq!(aead::decrypt(&key, "faces", &sealed).unwrap(), b"empreintes");
        // Autre clé, autres données, contenu modifié ou tronqué : refusés.
        assert!(aead::decrypt(&aead::new_key(), "faces", &sealed).is_err());
        assert!(aead::decrypt(&key, "autre", &sealed).is_err());
        let mut bad = sealed.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(aead::decrypt(&key, "faces", &bad).is_err());
        assert!(aead::decrypt(&key, "faces", &sealed[..20]).is_err());
    }

    /// Chiffrement réel du système (DPAPI ; trousseau sous Linux s'il y en a un).
    #[test]
    fn system_round_trip() {
        let Some((p, sealed)) = seal("homesmart-test", b"empreintes").unwrap() else {
            assert!(!cfg!(windows), "DPAPI existe toujours sous Windows");
            eprintln!("aucune protection disponible ici : aller-retour non vérifié");
            return;
        };
        if cfg!(windows) {
            assert_eq!(p, Protection::Dpapi);
        }
        assert!(!sealed.windows(10).any(|w| w == b"empreintes"));
        assert_eq!(open(p, "homesmart-test", &sealed).unwrap(), b"empreintes");
        let mut bad = sealed.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 1;
        assert!(open(p, "homesmart-test", &bad).is_err());
        assert_eq!(Protection::from_id(p.id()), Some(p));
    }
}
