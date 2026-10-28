//! Une seule instance de HomeSmart à la fois (fenêtre ou terminal).
//!
//! Les appareils Tuya n'acceptent souvent qu'une connexion locale : deux HomeSmart ouverts
//! en même temps se les disputeraient. Le premier lancé réserve un port local ; les suivants
//! le voient et préviennent l'utilisateur (sans bloquer).

use std::net::{Ipv4Addr, TcpListener};

/// Port réservé (local uniquement, aucune connexion n'y est acceptée).
pub const PORT: u16 = 47_321;

/// Garde l'instance « principale » tant qu'elle est en vie.
pub struct Guard(#[allow(dead_code)] TcpListener);

/// `Some` si aucune autre instance n'est ouverte ; `None` sinon.
pub fn acquire() -> Option<Guard> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, PORT)).ok().map(Guard)
}

/// Message à montrer quand une autre instance est déjà ouverte.
pub const ALREADY_RUNNING: &str =
    "HomeSmart est déjà ouvert ailleurs (fenêtre ou terminal) : les appareils risquent de refuser une deuxième connexion. Ferme l'autre pour éviter les conflits.";
