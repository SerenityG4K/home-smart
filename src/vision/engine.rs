//! Moteur d'inférence : ONNX Runtime (crate `ort`), choisi à l'exécution.
//!
//! - **Windows** : DirectML d'abord, qui accélère sur toute carte graphique DirectX 12 (NVIDIA,
//!   AMD, Intel), sans rien installer ; s'il échoue (pas de carte compatible, pilote), repli sur
//!   le processeur.
//! - **Ailleurs** : le processeur (ONNX Runtime y est très bien optimisé).
//!
//! Le réglage « processeur seulement » force le repli (machines où la carte graphique est prise).

use anyhow::{anyhow, Result};
use ort::environment::Environment;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use std::path::Path;
use std::sync::OnceLock;

/// Où faire tourner les modèles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Accel {
    /// Carte graphique si possible, sinon processeur.
    #[default]
    Auto,
    /// Processeur seulement.
    Cpu,
}

/// Une session prête, et ce qui la fait tourner (pour l'affichage).
pub struct Loaded {
    pub session: Session,
    pub backend: &'static str,
}

/// Environnement ONNX Runtime du processus (créé une fois).
fn environment() -> Result<&'static Environment> {
    static ENV: OnceLock<std::result::Result<Environment, String>> = OnceLock::new();
    ENV.get_or_init(|| ort::init().build().map_err(|e| format!("{e}")))
        .as_ref()
        .map_err(|e| anyhow!("ONNX Runtime indisponible : {e}"))
}

/// Fils de calcul par session sur processeur : la moitié des cœurs (plusieurs caméras et
/// l'interface tournent en même temps).
fn cpu_threads() -> usize {
    std::thread::available_parallelism().map(|n| (n.get() / 2).max(1)).unwrap_or(2)
}

/// Charge un modèle. `fixed` : dimensions dynamiques à fixer (ex. `("batch", 1)`) — DirectML
/// est bien plus rapide quand toutes les tailles sont connues d'avance.
pub fn load(path: &Path, accel: Accel, fixed: &[(&str, i64)]) -> Result<Loaded> {
    let env = environment()?;
    let err = |e: ort::Error<ort::session::builder::SessionBuilder>| anyhow!("{e}");
    #[cfg(windows)]
    if accel == Accel::Auto {
        let gpu = (|| -> Result<Session> {
            let mut b = Session::builder(env)?
                .with_optimization_level(GraphOptimizationLevel::Level3)
                .map_err(err)?
                // Avertissements de forme des modèles (ex. SFace, exporté avec un ancien outil) : sans intérêt ici.
                .with_log_level(ort::logging::LogLevel::Error)
                .map_err(err)?
                // DirectML n'accepte ni l'exécution parallèle ni les motifs mémoire.
                .with_parallel_execution(false)
                .map_err(err)?
                .with_memory_pattern(false)
                .map_err(err)?;
            for (name, size) in fixed {
                b = b.with_dimension_override(name, *size).map_err(err)?;
            }
            let mut b = b.with_execution_providers([ort::ep::DirectML::default().build().error_on_failure()]).map_err(err)?;
            Ok(b.commit_from_file(path)?)
        })();
        match gpu {
            Ok(session) => return Ok(Loaded { session, backend: "carte graphique (DirectML)" }),
            Err(e) => tracing::info!("DirectML indisponible pour {} ({e:#}) : processeur", path.display()),
        }
    }
    let _ = accel;
    let mut b = Session::builder(env)?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(err)?
        // Avertissements de forme des modèles (ex. SFace, exporté avec un ancien outil) : sans intérêt ici.
        .with_log_level(ort::logging::LogLevel::Error)
        .map_err(err)?
        .with_intra_threads(cpu_threads())
        .map_err(err)?;
    for (name, size) in fixed {
        b = b.with_dimension_override(name, *size).map_err(err)?;
    }
    Ok(Loaded { session: b.commit_from_file(path)?, backend: "processeur" })
}

/// Entrées et sorties d'un modèle (nom et type), pour vérifier ce qu'on lui donne.
pub fn describe(session: &Session) -> String {
    let fmt = |o: &[ort::value::Outlet]| o.iter().map(|x| format!("{} {:?}", x.name(), x.dtype())).collect::<Vec<_>>().join(" ; ");
    format!("entrées : {} — sorties : {}", fmt(session.inputs()), fmt(session.outputs()))
}
