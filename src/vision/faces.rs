//! Visages connus : galerie des habitants enrôlés et comparaison des empreintes.
//!
//! Une **empreinte** est le vecteur que le modèle de reconnaissance tire d'un visage redressé ;
//! deux visages de la même personne ont des empreintes proches (similarité cosinus élevée).
//! La galerie garde quelques empreintes par personne (vues différentes : face, profil, lunettes)
//! et retient la meilleure ressemblance.
//!
//! **Données biométriques** : la galerie n'existe que si l'utilisateur enrôle quelqu'un, elle
//! reste dans le dossier de HomeSmart (`faces.json`) et ne contient que des empreintes (aucune
//! photo) ; une personne supprimée l'est entièrement. Sur le disque, elle est chiffrée
//! (`crate::protect` : DPAPI sous Windows, trousseau de la session sous Linux) ; en clair
//! seulement si le système n'offre aucune protection, et l'appelant le signale.

use crate::protect::{self, Protection};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

/// Empreintes gardées au plus par personne (au-delà, les plus anciennes cèdent la place).
pub const MAX_EMBEDDINGS: usize = 24;

/// Norme 1 (en place). Un vecteur nul reste nul.
pub fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

/// Similarité cosinus de deux vecteurs (déjà normés ou non), −1…1.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KnownPerson {
    pub id: String,
    pub name: String,
    /// Empreintes normées, toutes issues du même modèle (`FaceGallery::model`).
    pub embeddings: Vec<Vec<f32>>,
    pub added: DateTime<Local>,
}

/// Galerie des visages connus.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FaceGallery {
    /// Modèle qui a produit les empreintes : d'un modèle à l'autre, elles ne se comparent pas.
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub people: Vec<KnownPerson>,
}

/// Résultat d'une comparaison.
#[derive(Clone, Debug, PartialEq)]
pub enum Identity {
    Known { id: String, name: String, similarity: f32 },
    Unknown { best: f32 },
}

/// Nom de la galerie pour `protect` (clé du trousseau, entropie DPAPI).
const SEALED_AS: &str = "faces";

/// Galerie chiffrée sur le disque : la protection employée et le contenu chiffré (base64).
#[derive(Serialize, Deserialize)]
struct SealedFile {
    protection: String,
    data: String,
}

/// Comment la galerie est rangée sur le disque.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stored {
    /// Pas encore de fichier.
    Nothing,
    /// En clair : ancien format, ou aucune protection sur ce système.
    Clear,
    Sealed(Protection),
}

/// Écrit un fichier lisible par ton seul compte (droits 600 sous Linux ; sous Windows, le
/// dossier du profil l'est déjà).
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut f = options.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

impl FaceGallery {
    /// Lit la galerie, chiffrée ou en clair (ancien format). Un fichier abîmé, ou chiffré par un
    /// autre compte ou sur un autre PC, est une erreur : jamais une galerie vide qui l'écraserait.
    pub fn load(path: &Path) -> Result<(FaceGallery, Stored)> {
        match std::fs::read(path) {
            Ok(bytes) => Self::decode(&bytes).with_context(|| format!("lecture de {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((FaceGallery::default(), Stored::Nothing)),
            Err(e) => Err(e).with_context(|| format!("lecture de {}", path.display())),
        }
    }

    fn decode(bytes: &[u8]) -> Result<(FaceGallery, Stored)> {
        let v: serde_json::Value = serde_json::from_slice(bytes).context("fichier abîmé (JSON invalide)")?;
        if v.get("protection").is_none() {
            return Ok((serde_json::from_value(v).context("galerie en clair abîmée")?, Stored::Clear));
        }
        let f: SealedFile = serde_json::from_value(v).context("enveloppe chiffrée abîmée")?;
        let p = Protection::from_id(&f.protection).ok_or_else(|| anyhow!("protection inconnue « {} »", f.protection))?;
        let sealed = B64.decode(f.data.as_bytes()).context("contenu chiffré abîmé (base64)")?;
        let plain = protect::open(p, SEALED_AS, &sealed)?;
        Ok((serde_json::from_slice(&plain).context("galerie déchiffrée illisible")?, Stored::Sealed(p)))
    }

    /// Enregistrement atomique (fichier temporaire puis renommage), chiffré si le système le
    /// permet, sinon en clair ; renvoie ce qui a été fait.
    pub fn save(&self, path: &Path) -> Result<Stored> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let plain = serde_json::to_vec(self)?;
        let (bytes, stored) = match protect::seal(SEALED_AS, &plain)? {
            Some((p, sealed)) => (serde_json::to_vec(&SealedFile { protection: p.id().to_string(), data: B64.encode(sealed) })?, Stored::Sealed(p)),
            None => (plain, Stored::Clear),
        };
        let tmp = path.with_extension("json.tmp");
        write_private(&tmp, &bytes).with_context(|| format!("écriture de {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("écriture de {}", path.display()))?;
        Ok(stored)
    }

    /// Ajoute une empreinte à la personne `name` (créée si besoin) ; renvoie son identifiant.
    pub fn enroll(&mut self, name: &str, embedding: &[f32]) -> String {
        let mut e = embedding.to_vec();
        normalize(&mut e);
        let name = name.trim();
        if let Some(p) = self.people.iter_mut().find(|p| p.name.eq_ignore_ascii_case(name)) {
            p.embeddings.push(e);
            if p.embeddings.len() > MAX_EMBEDDINGS {
                p.embeddings.remove(0);
            }
            return p.id.clone();
        }
        let id = format!("p{:08x}", rand::random::<u32>());
        self.people.push(KnownPerson { id: id.clone(), name: name.to_string(), embeddings: vec![e], added: Local::now() });
        id
    }

    pub fn remove(&mut self, id: &str) -> Option<KnownPerson> {
        let i = self.people.iter().position(|p| p.id == id)?;
        Some(self.people.remove(i))
    }

    pub fn rename(&mut self, id: &str, name: &str) -> bool {
        match self.people.iter_mut().find(|p| p.id == id) {
            Some(p) => {
                p.name = name.trim().to_string();
                true
            }
            None => false,
        }
    }

    /// Qui est-ce ? Meilleure ressemblance sur toutes les empreintes ; connu au-delà de `threshold`.
    pub fn identify(&self, embedding: &[f32], threshold: f32) -> Identity {
        let mut best: Option<(&KnownPerson, f32)> = None;
        for p in &self.people {
            let s = p.embeddings.iter().map(|e| cosine(e, embedding)).fold(f32::MIN, f32::max);
            if best.is_none_or(|(_, b)| s > b) {
                best = Some((p, s));
            }
        }
        match best {
            Some((p, s)) if s >= threshold => Identity::Known { id: p.id.clone(), name: p.name.clone(), similarity: s },
            Some((_, s)) => Identity::Unknown { best: s },
            None => Identity::Unknown { best: 0.0 },
        }
    }
}

/// Vote sur plusieurs images avant d'affirmer qui est une personne suivie : un visage de
/// travers ou flou ne suffit pas à mettre un nom (ni à crier « inconnu »).
#[derive(Clone, Debug, Default)]
pub struct IdentityVotes {
    known: HashMap<String, (String, u32, f32)>,
    unknown: u32,
}

/// Verdict stable pour une piste.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    Undecided,
    Person { id: String, name: String, similarity: f32 },
    Stranger,
}

impl IdentityVotes {
    pub fn add(&mut self, identity: &Identity) {
        match identity {
            Identity::Known { id, name, similarity } => {
                let e = self.known.entry(id.clone()).or_insert((name.clone(), 0, 0.0));
                e.1 += 1;
                e.2 = e.2.max(*similarity);
            }
            Identity::Unknown { .. } => self.unknown += 1,
        }
    }

    /// Une personne connue dès 2 reconnaissances concordantes, majoritaires parmi les
    /// reconnaissances et faisant au moins `known_share` des visages nets vus (les visages non
    /// reconnus comptent contre : sans cela, un inconnu qui reste longtemps finit par cumuler deux
    /// ressemblances de hasard avec un habitant) ; inconnue après `stranger_after` visages nets
    /// sans aucune reconnaissance.
    pub fn verdict(&self, stranger_after: u32, known_share: f32) -> Verdict {
        if let Some((id, (name, n, sim))) = self.known.iter().max_by_key(|(_, (_, n, _))| *n) {
            let known: u32 = self.known.values().map(|v| v.1).sum();
            let others = known - n;
            if *n >= 2 && *n > others && *n as f32 >= known_share * (known + self.unknown) as f32 {
                return Verdict::Person { id: id.clone(), name: name.clone(), similarity: *sim };
            }
        }
        if self.known.is_empty() && self.unknown >= stranger_after {
            Verdict::Stranger
        } else {
            Verdict::Undecided
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(seed: u32, dim: usize) -> Vec<f32> {
        // Vecteur pseudo-aléatoire reproductible.
        let mut s = seed.wrapping_mul(2_654_435_761).max(1);
        let mut v: Vec<f32> = (0..dim)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s % 2000) as f32 / 1000.0 - 1.0
            })
            .collect();
        normalize(&mut v);
        v
    }

    fn near(v: &[f32], k: f32, seed: u32) -> Vec<f32> {
        let noise = unit(seed, v.len());
        let mut out: Vec<f32> = v.iter().zip(&noise).map(|(a, b)| a + k * b).collect();
        normalize(&mut out);
        out
    }

    #[test]
    fn enroll_identify_and_forget() {
        let (alice, bob) = (unit(1, 128), unit(2, 128));
        let mut g = FaceGallery { model: "sface".into(), people: vec![] };
        let ida = g.enroll("Alice", &alice);
        g.enroll("Bob", &bob);
        assert_eq!(g.enroll("alice", &near(&alice, 0.2, 9)), ida, "même personne, quelle que soit la casse");
        assert_eq!(g.people[0].embeddings.len(), 2);
        match g.identify(&near(&alice, 0.3, 7), 0.5) {
            Identity::Known { name, similarity, .. } => assert!(name == "Alice" && similarity > 0.8),
            other => panic!("{other:?}"),
        }
        assert!(matches!(g.identify(&unit(3, 128), 0.5), Identity::Unknown { .. }));
        assert!(g.remove(&ida).is_some());
        assert!(matches!(g.identify(&alice, 0.5), Identity::Unknown { .. }));
        // Aller-retour sur disque.
        let path = temp("forget");
        g.save(&path).unwrap();
        assert_eq!(FaceGallery::load(&path).unwrap().0, g);
        let _ = std::fs::remove_file(&path);
        assert_eq!(FaceGallery::load(&path).unwrap(), (FaceGallery::default(), Stored::Nothing));
    }

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("homesmart-faces-{name}-{}.json", std::process::id()))
    }

    fn sample() -> FaceGallery {
        let mut g = FaceGallery { model: "sface".into(), people: vec![] };
        g.enroll("Zoé Témoin", &unit(4, 128));
        g
    }

    /// Le fichier ne laisse rien lire de la galerie (si le système sait chiffrer).
    fn assert_opaque(path: &Path, stored: Stored) {
        let text = std::fs::read_to_string(path).unwrap();
        match stored {
            Stored::Sealed(p) => {
                assert!(!text.contains("Zoé") && !text.contains("embeddings"), "{text}");
                assert!(text.contains(&format!("\"protection\":\"{}\"", p.id())));
            }
            _ => {
                assert!(!cfg!(windows), "DPAPI existe toujours sous Windows");
                eprintln!("aucune protection disponible ici : galerie en clair, chiffrement non vérifié");
            }
        }
    }

    #[test]
    fn sealed_round_trip() {
        let (g, path) = (sample(), temp("sealed"));
        let stored = g.save(&path).unwrap();
        if cfg!(windows) {
            assert_eq!(stored, Stored::Sealed(Protection::Dpapi));
        }
        assert_opaque(&path, stored);
        assert_eq!(FaceGallery::load(&path).unwrap(), (g, stored));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn clear_gallery_is_read_then_sealed() {
        let (g, path) = (sample(), temp("clear"));
        // Ancien format : la galerie en JSON clair.
        std::fs::write(&path, serde_json::to_vec(&g).unwrap()).unwrap();
        assert_eq!(FaceGallery::load(&path).unwrap(), (g.clone(), Stored::Clear));
        let stored = g.save(&path).unwrap();
        assert_opaque(&path, stored);
        assert_eq!(FaceGallery::load(&path).unwrap(), (g, stored));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn damaged_file_is_refused_and_left_alone() {
        let path = temp("damaged");
        let stored = sample().save(&path).unwrap();
        let good = std::fs::read_to_string(&path).unwrap();
        let mut bad = vec![
            // Tronqué, illisible, ni galerie ni enveloppe, base64 invalide, protection inconnue.
            good.chars().take(good.chars().count() / 2).collect(),
            "\u{0}\u{1}pas du JSON".to_string(),
            "[1, 2, 3]".to_string(),
            r#"{"protection":"dpapi","data":"%%%"}"#.to_string(),
            r#"{"protection":"rot13","data":""}"#.to_string(),
        ];
        if let Stored::Sealed(p) = stored {
            // Contenu chiffré modifié : l'intégrité est vérifiée au déchiffrement.
            let f: SealedFile = serde_json::from_str(&good).unwrap();
            let mut data = B64.decode(&f.data).unwrap();
            let mid = data.len() / 2;
            data[mid] ^= 0x40;
            bad.push(serde_json::to_string(&SealedFile { protection: p.id().into(), data: B64.encode(data) }).unwrap());
        }
        for content in bad {
            std::fs::write(&path, &content).unwrap();
            assert!(FaceGallery::load(&path).is_err(), "accepté : {content}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content, "le fichier doit rester intact");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn votes_need_agreement() {
        let alice = Identity::Known { id: "a".into(), name: "Alice".into(), similarity: 0.7 };
        let bob = Identity::Known { id: "b".into(), name: "Bob".into(), similarity: 0.6 };
        let mut v = IdentityVotes::default();
        v.add(&alice);
        assert_eq!(v.verdict(4, 0.34), Verdict::Undecided);
        v.add(&bob);
        v.add(&alice);
        assert!(matches!(v.verdict(4, 0.34), Verdict::Person { ref name, .. } if name == "Alice"));
        let mut s = IdentityVotes::default();
        for _ in 0..3 {
            s.add(&Identity::Unknown { best: 0.1 });
        }
        assert_eq!(s.verdict(4, 0.34), Verdict::Undecided);
        s.add(&Identity::Unknown { best: 0.1 });
        assert_eq!(s.verdict(4, 0.34), Verdict::Stranger);
    }

    #[test]
    fn a_stranger_who_stays_is_not_named_by_chance() {
        // Un inconnu reste longtemps : deux ressemblances de hasard avec Alice parmi vingt visages
        // non reconnus ne font pas d'elle Alice…
        let alice = Identity::Known { id: "a".into(), name: "Alice".into(), similarity: 0.38 };
        let mut v = IdentityVotes::default();
        for i in 0..22 {
            v.add(if i == 9 || i == 17 { &alice } else { &Identity::Unknown { best: 0.2 } });
            assert!(!matches!(v.verdict(4, 0.34), Verdict::Person { .. }), "visage {i}");
        }
        // … alors qu'une habitante vue d'abord de biais, puis de face, est bien nommée.
        let mut r = IdentityVotes::default();
        for _ in 0..6 {
            r.add(&Identity::Unknown { best: 0.3 });
        }
        for _ in 0..4 {
            r.add(&alice);
        }
        assert!(matches!(r.verdict(4, 0.34), Verdict::Person { ref name, .. } if name == "Alice"));
    }
}
