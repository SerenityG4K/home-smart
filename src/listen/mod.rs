//! Écoute par IA, en local : sons de la maison (alarme, bris de verre, aboiements, pleurs…)
//! et parole transcrite en texte, à partir du micro des caméras.
//!
//! Tout est optionnel et désactivé par défaut. Le son n'est jamais enregistré : il est analysé
//! en mémoire, par blocs de quelques secondes, puis oublié ; seuls les événements (et, si la
//! transcription est activée, le texte reconnu) vont au journal.

pub mod dsp;
pub mod level;
pub mod sounds;
pub mod vad;
pub mod wav;
pub mod whisper;
