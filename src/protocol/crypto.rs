//! Primitives cryptographiques utilisées par le protocole Tuya local.

use aes::cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes128Gcm, Nonce, Tag};
use anyhow::{bail, Result};
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use sha2::Sha256;

pub type Key = [u8; 16];

/// Clé fixe des annonces UDP : md5("yGAdlopoPVldABfn").
pub fn udp_key() -> Key {
    md5(b"yGAdlopoPVldABfn")
}

pub fn md5(data: &[u8]) -> [u8; 16] {
    Md5::digest(data).into()
}

pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepte toute taille de clé");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

pub fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

/// AES-128-ECB, avec bourrage PKCS#7 optionnel.
pub fn ecb_encrypt(key: &Key, data: &[u8], pad: bool) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut buf = data.to_vec();
    if pad {
        let n = 16 - (buf.len() % 16);
        buf.extend(std::iter::repeat(n as u8).take(n));
    }
    debug_assert!(buf.len() % 16 == 0, "ECB sans bourrage : taille non multiple de 16");
    for block in buf.chunks_exact_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    buf
}

pub fn ecb_decrypt(key: &Key, data: &[u8], unpad: bool) -> Result<Vec<u8>> {
    if data.len() % 16 != 0 {
        bail!("données chiffrées de taille invalide ({} octets)", data.len());
    }
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut buf = data.to_vec();
    for block in buf.chunks_exact_mut(16) {
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
    }
    if unpad && !buf.is_empty() {
        let n = *buf.last().unwrap() as usize;
        if n == 0 || n > 16 || n > buf.len() || !buf[buf.len() - n..].iter().all(|&b| b as usize == n) {
            bail!("bourrage invalide (clé locale incorrecte ?)");
        }
        buf.truncate(buf.len() - n);
    }
    Ok(buf)
}

/// AES-128-GCM : chiffre `data` sur place et renvoie le tag.
pub fn gcm_encrypt(key: &Key, iv: &[u8; 12], aad: &[u8], data: &mut Vec<u8>) -> [u8; 16] {
    let cipher = Aes128Gcm::new(GenericArray::from_slice(key));
    let tag = cipher
        .encrypt_in_place_detached(Nonce::from_slice(iv), aad, data)
        .expect("chiffrement GCM");
    tag.into()
}

pub fn gcm_decrypt(key: &Key, iv: &[u8], aad: &[u8], data: &mut Vec<u8>, tag: &[u8]) -> Result<()> {
    if iv.len() != 12 || tag.len() != 16 {
        bail!("trame GCM mal formée");
    }
    let cipher = Aes128Gcm::new(GenericArray::from_slice(key));
    cipher
        .decrypt_in_place_detached(Nonce::from_slice(iv), aad, data, Tag::from_slice(tag))
        .map_err(|_| anyhow::anyhow!("authentification GCM échouée (clé locale incorrecte ?)"))
}

pub fn xor16(a: &[u8; 16], b: &[u8; 16]) -> [u8; 16] {
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = a[i] ^ b[i];
    }
    out
}

pub fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecb_roundtrip() {
        let key = *b"0123456789abcdef";
        for len in [0, 1, 15, 16, 17, 100] {
            let data: Vec<u8> = (0..len as u8).collect();
            let enc = ecb_encrypt(&key, &data, true);
            assert_eq!(enc.len() % 16, 0);
            assert_eq!(ecb_decrypt(&key, &enc, true).unwrap(), data);
        }
    }

    #[test]
    fn gcm_roundtrip() {
        let key = *b"0123456789abcdef";
        let iv = [7u8; 12];
        let mut data = b"{\"dps\":{\"1\":true}}".to_vec();
        let tag = gcm_encrypt(&key, &iv, b"aad", &mut data);
        gcm_decrypt(&key, &iv, b"aad", &mut data, &tag).unwrap();
        assert_eq!(data, b"{\"dps\":{\"1\":true}}");
    }

    #[test]
    fn udp_key_is_stable() {
        assert_eq!(hex(&udp_key()), "6c1ec8e2bb9bb59ab50b0daf649b410a");
    }
}
