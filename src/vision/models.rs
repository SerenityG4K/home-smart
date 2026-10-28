//! Modèles d'IA : catalogue, emplacement et téléchargement.
//!
//! Les modèles ne sont pas livrés avec HomeSmart (ni dans le dépôt) : chacun est téléchargé à
//! la demande depuis sa source officielle, une seule fois, puis vérifié par son empreinte
//! SHA-256 avant d'être utilisé. Seuls des modèles sous licence libre permissive sont listés.

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Un modèle du catalogue.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelSpec {
    /// Identifiant stable (nom de fichier local).
    pub id: &'static str,
    /// À quoi il sert, pour l'interface.
    pub role: &'static str,
    pub url: &'static str,
    /// Empreinte SHA-256 attendue du fichier téléchargé (hexadécimal minuscule).
    pub sha256: &'static str,
    /// Taille du fichier téléchargé, en octets.
    pub size: u64,
    /// Modèle livré dans une archive .zip : nom du fichier à en extraire, et sa taille.
    pub member: Option<(&'static str, u64)>,
    pub license: &'static str,
    /// Page d'origine (crédits).
    pub source: &'static str,
}

/// Personnes et squelettes (17 points), un seul passage : RTMO-s (OpenMMLab, MMPose).
pub const RTMO_S: ModelSpec = ModelSpec {
    id: "rtmo-s-body7-640",
    role: "personnes et postures",
    url: "https://download.openmmlab.com/mmpose/v1/projects/rtmo/onnx_sdk/rtmo-s_8xb32-600e_body7-640x640-dac2bf74_20231211.zip",
    sha256: "3da7ad88b209f9da8be87ba5c325610639af04de3b5c8a96649b98cd9e2848a2",
    size: 36_442_532,
    member: Some(("end2end.onnx", 39_617_685)),
    license: "Apache-2.0",
    source: "https://github.com/open-mmlab/mmpose/tree/main/projects/rtmo",
};

/// Personnes et squelettes, version plus grosse et plus précise : RTMO-m (même format).
pub const RTMO_M: ModelSpec = ModelSpec {
    id: "rtmo-m-body7-640",
    role: "personnes et postures (précision élevée)",
    url: "https://download.openmmlab.com/mmpose/v1/projects/rtmo/onnx_sdk/rtmo-m_16xb16-600e_body7-640x640-39e78cc4_20231211.zip",
    sha256: "6b9d3be1323cc030444f731bbe593ea61f59b9481e23da2f8b60cb4b72136e16",
    size: 82_659_451,
    member: Some(("end2end.onnx", 89_269_977)),
    license: "Apache-2.0",
    source: "https://github.com/open-mmlab/mmpose/tree/main/projects/rtmo",
};

/// Détection des visages et de leurs 5 points : YuNet (OpenCV Zoo).
pub const YUNET: ModelSpec = ModelSpec {
    id: "yunet-2023mar",
    role: "visages",
    url: "https://huggingface.co/opencv/face_detection_yunet/resolve/main/face_detection_yunet_2023mar.onnx",
    sha256: "8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4",
    size: 232_589,
    member: None,
    license: "MIT",
    source: "https://github.com/opencv/opencv_zoo/tree/main/models/face_detection_yunet",
};

/// Empreintes de visage (reconnaissance) : SFace (OpenCV Zoo).
pub const SFACE: ModelSpec = ModelSpec {
    id: "sface-2021dec",
    role: "reconnaissance des visages",
    url: "https://huggingface.co/opencv/face_recognition_sface/resolve/main/face_recognition_sface_2021dec.onnx",
    sha256: "0ba9fbfa01b5270c96627c4ef784da859931e02f04419c829e83484087c34e79",
    size: 38_696_353,
    member: None,
    license: "Apache-2.0",
    source: "https://github.com/opencv/opencv_zoo/tree/main/models/face_recognition_sface",
};

/// Animaux et véhicules (80 sortes d'objets COCO) : YOLOX-s (Megvii).
pub const YOLOX_S: ModelSpec = ModelSpec {
    id: "yolox-s",
    role: "animaux et véhicules",
    url: "https://github.com/Megvii-BaseDetection/YOLOX/releases/download/0.1.1rc0/yolox_s.onnx",
    sha256: "c5c2d13e59ae883e6af3b45daea64af4833a4951c92d116ec270d9ddbe998063",
    size: 35_858_002,
    member: None,
    license: "Apache-2.0",
    source: "https://github.com/Megvii-BaseDetection/YOLOX",
};

/// Modèle des personnes choisi.
pub fn persons_model(m: super::PersonModel) -> ModelSpec {
    match m {
        super::PersonModel::Standard => RTMO_S,
        super::PersonModel::High => RTMO_M,
    }
}

/// Modèles nécessaires à la vision avec ce modèle des personnes.
pub fn vision_models(m: super::PersonModel) -> [ModelSpec; 4] {
    [persons_model(m), YUNET, SFACE, YOLOX_S]
}

/// Tout le catalogue de la vision, dans l'ordre d'installation.
pub const ALL: [ModelSpec; 4] = [RTMO_S, YUNET, SFACE, YOLOX_S];

// ---- Écoute ------------------------------------------------------------------------------------

/// Sons de la maison (527 sortes de sons AudioSet) : CED-mini (export ONNX de sherpa-onnx, int8).
pub const CED_MINI: ModelSpec = ModelSpec {
    id: "ced-mini-int8",
    role: "sons de la maison",
    url: "https://huggingface.co/k2-fsa/sherpa-onnx-ced-mini-audio-tagging-2024-04-19/resolve/4da6df50dc47b25f91074b5ef7df4c1cc0dd7d4d/model.int8.onnx",
    sha256: "ff29f39f9fbe637f72535160e9d006d61d872fdab0fce838672265b9b38cf946",
    size: 10_451_715,
    member: None,
    license: "Apache-2.0",
    source: "https://github.com/RicherMans/CED",
};

/// Noms des 527 sons (ontologie AudioSet de Google).
pub const CED_LABELS: ModelSpec = ModelSpec {
    id: "ced-labels.csv",
    role: "noms des sons",
    url: "https://huggingface.co/k2-fsa/sherpa-onnx-ced-mini-audio-tagging-2024-04-19/resolve/4da6df50dc47b25f91074b5ef7df4c1cc0dd7d4d/class_labels_indices.csv",
    sha256: "cdd1049833c4b86127c2773ac0d14a2754b6a6d0d1798002ed5c66e699708429",
    size: 14_675,
    member: None,
    license: "CC-BY-4.0",
    source: "https://research.google.com/audioset/",
};

/// Détection de la voix : Silero VAD v6.
pub const SILERO_VAD: ModelSpec = ModelSpec {
    id: "silero-vad-v6",
    role: "détection de la voix",
    url: "https://raw.githubusercontent.com/snakers4/silero-vad/v6.2.3/src/silero_vad/data/silero_vad.onnx",
    sha256: "1a153a22f4509e292a94e67d6f9b85e8deb25b4988682b7e174c65279d8788e3",
    size: 2_327_524,
    member: None,
    license: "MIT",
    source: "https://github.com/snakers4/silero-vad",
};

/// Transcription de la parole : Whisper small (OpenAI), export ONNX int8 d'onnx-community.
pub const WHISPER_ENCODER: ModelSpec = ModelSpec {
    id: "whisper-small-encoder-int8",
    role: "transcription (écoute)",
    url: "https://huggingface.co/onnx-community/whisper-small/resolve/36050c46d777d46dc4b5f43f6d90574fc38f8732/onnx/encoder_model_int8.onnx",
    sha256: "2601c9eb2d345c5916d4576d36f663a7c96589740fb2273828c48c3fc2c7db75",
    size: 92_326_127,
    member: None,
    license: "MIT",
    source: "https://github.com/openai/whisper",
};

pub const WHISPER_DECODER: ModelSpec = ModelSpec {
    id: "whisper-small-decoder-int8",
    role: "transcription (texte)",
    url: "https://huggingface.co/onnx-community/whisper-small/resolve/36050c46d777d46dc4b5f43f6d90574fc38f8732/onnx/decoder_model_merged_int8.onnx",
    sha256: "ec07c3cbb64172c39791e26ee870a65ac22b458c36722bfe2776b3dbf741e0c9",
    size: 156_750_845,
    member: None,
    license: "MIT",
    source: "https://github.com/openai/whisper",
};

pub const WHISPER_TOKENIZER: ModelSpec = ModelSpec {
    id: "whisper-tokenizer.json",
    role: "vocabulaire de la transcription",
    url: "https://huggingface.co/onnx-community/whisper-small/resolve/36050c46d777d46dc4b5f43f6d90574fc38f8732/tokenizer.json",
    sha256: "27fc476bfe7f17299480be2273fc0608e4d5a99aba2ab5dec5374b4482d1a566",
    size: 2_480_466,
    member: None,
    license: "MIT",
    source: "https://github.com/openai/whisper",
};

/// Ce qu'il faut pour reconnaître les sons.
pub const SOUNDS: [ModelSpec; 2] = [CED_MINI, CED_LABELS];
/// Ce qu'il faut pour transcrire la parole.
pub const SPEECH: [ModelSpec; 4] = [SILERO_VAD, WHISPER_ENCODER, WHISPER_DECODER, WHISPER_TOKENIZER];

/// Dossier des modèles : `HOMESMART_MODELS`, sinon le dossier de données de l'utilisateur
/// (`%LOCALAPPDATA%\HomeSmart\models` sous Windows, `~/.local/share/homesmart/models` sous Linux).
pub fn models_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("HOMESMART_MODELS").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("HomeSmart"));
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
        .map(|d| d.join("homesmart"));
    base.unwrap_or_default().join("models")
}

impl ModelSpec {
    /// Fichier installé : `<id>.onnx`, ou `<id>` tel quel s'il a déjà une extension (.csv, .json).
    pub fn path(&self) -> PathBuf {
        if self.id.contains('.') {
            models_dir().join(self.id)
        } else {
            models_dir().join(format!("{}.onnx", self.id))
        }
    }

    /// Taille du fichier .onnx installé.
    pub fn installed_size(&self) -> u64 {
        self.member.map_or(self.size, |(_, n)| n)
    }

    /// Présent sur le disque avec la bonne taille (l'empreinte a été vérifiée au téléchargement).
    pub fn is_installed(&self) -> bool {
        std::fs::metadata(self.path()).is_ok_and(|m| m.len() == self.installed_size())
    }
}

/// Empreinte SHA-256 d'un fichier.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).with_context(|| format!("lecture de {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Copie `reader` dans `dest` en vérifiant taille et empreinte ; `progress(reçu, total)`.
/// Écrit d'abord dans `<dest>.part`, renommé seulement si tout est conforme.
pub fn install_from(mut reader: impl Read, spec: &ModelSpec, dest: &Path, progress: &dyn Fn(u64, u64)) -> Result<()> {
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("création de {}", dir.display()))?;
    }
    let part = dest.with_extension("onnx.part");
    let mut out = std::fs::File::create(&part).with_context(|| format!("écriture de {}", part.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut got = 0u64;
    loop {
        let n = reader.read(&mut buf).context("téléchargement interrompu")?;
        if n == 0 {
            break;
        }
        got += n as u64;
        if got > spec.size + (1 << 20) {
            drop(out);
            let _ = std::fs::remove_file(&part);
            bail!("fichier plus gros qu'annoncé : téléchargement abandonné");
        }
        h.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        progress(got, spec.size);
    }
    out.flush()?;
    drop(out);
    let digest: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if digest != spec.sha256 || got != spec.size {
        let _ = std::fs::remove_file(&part);
        bail!("{} : fichier reçu non conforme (taille {got} au lieu de {}, empreinte différente) — rien n'a été installé", spec.id, spec.size);
    }
    match spec.member {
        None => std::fs::rename(&part, dest).with_context(|| format!("installation de {}", dest.display()))?,
        Some((name, size)) => {
            // Archive vérifiée : on n'en garde que le modèle.
            let r = extract_member(&part, name, size, dest);
            let _ = std::fs::remove_file(&part);
            r?;
        }
    }
    Ok(())
}

/// Extrait `name` de l'archive `zip_path` vers `dest` (via un fichier temporaire) ; `size` attendue.
fn extract_member(zip_path: &Path, name: &str, size: u64, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).context("archive du modèle illisible")?;
    let mut entry = archive.by_name(name).with_context(|| format!("« {name} » absent de l'archive"))?;
    if entry.size() != size {
        bail!("« {name} » : taille inattendue dans l'archive ({} au lieu de {size})", entry.size());
    }
    let tmp = dest.with_extension("onnx.extract");
    let mut out = std::fs::File::create(&tmp)?;
    let copied = std::io::copy(&mut entry, &mut out)?;
    drop(out);
    if copied != size {
        let _ = std::fs::remove_file(&tmp);
        bail!("« {name} » : extraction incomplète");
    }
    std::fs::rename(&tmp, dest).with_context(|| format!("installation de {}", dest.display()))?;
    Ok(())
}

/// Télécharge et vérifie un modèle s'il n'est pas déjà là. Bloquant.
pub fn ensure(spec: &ModelSpec, progress: &dyn Fn(u64, u64)) -> Result<PathBuf> {
    let dest = spec.path();
    if spec.is_installed() {
        return Ok(dest);
    }
    let resp = ureq::get(spec.url).call().map_err(|e| anyhow!("téléchargement de {} impossible : {e}", spec.id))?;
    install_from(resp.into_body().into_reader(), spec, &dest, progress)?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_install_and_refusal() {
        let data = b"modele de test".repeat(1000);
        let sha: String = Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
        let sha: &'static str = Box::leak(sha.into_boxed_str());
        let spec = ModelSpec { id: "essai", role: "test", url: "", sha256: sha, size: data.len() as u64, member: None, license: "MIT", source: "" };
        let dir = std::env::temp_dir().join(format!("homesmart-models-{}", std::process::id()));
        let dest = dir.join("essai.onnx");
        let seen = std::sync::Mutex::new(0u64);
        install_from(&data[..], &spec, &dest, &|got, _| *seen.lock().unwrap() = got).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert_eq!(*seen.lock().unwrap(), data.len() as u64);
        assert_eq!(sha256_file(&dest).unwrap(), sha);
        // Contenu altéré : refusé, rien d'installé à la place.
        let bad = ModelSpec { sha256: "00", ..spec };
        let dest2 = dir.join("autre.onnx");
        assert!(install_from(&data[..], &bad, &dest2, &|_, _| {}).is_err());
        assert!(!dest2.exists() && !dest2.with_extension("onnx.part").exists());

        // Modèle livré en archive : seul le membre voulu est gardé.
        let zip_path = dir.join("paquet.zip");
        {
            let mut w = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
            let o = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            w.start_file("deploy.json", o).unwrap();
            w.write_all(b"{}").unwrap();
            w.start_file("end2end.onnx", o).unwrap();
            w.write_all(&data).unwrap();
            w.finish().unwrap();
        }
        let zbytes = std::fs::read(&zip_path).unwrap();
        let zsha: String = Sha256::digest(&zbytes).iter().map(|b| format!("{b:02x}")).collect();
        let zspec = ModelSpec { sha256: Box::leak(zsha.into_boxed_str()), size: zbytes.len() as u64, member: Some(("end2end.onnx", data.len() as u64)), ..spec };
        let dest3 = dir.join("depuis-zip.onnx");
        install_from(&zbytes[..], &zspec, &dest3, &|_, _| {}).unwrap();
        assert_eq!(std::fs::read(&dest3).unwrap(), data);
        assert!(!dest3.with_extension("onnx.part").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
