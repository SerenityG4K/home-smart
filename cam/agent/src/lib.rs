//! Agent d'une caméra HomeSmart (à fabriquer soi-même) : voir `main.rs` et `cam/README.md`.
//! En bibliothèque pour que les tests de HomeSmart puissent démarrer une vraie API de caméra.

pub mod api;
pub mod board;
pub mod config;
pub mod mdns;
pub mod pipeline;
pub mod supervisor;
