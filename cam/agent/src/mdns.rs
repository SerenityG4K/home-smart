//! Annonce de la caméra sur le réseau local (mDNS / DNS-SD), pour que HomeSmart la trouve seul.

use crate::api::{Shared, MODEL};
use anyhow::Result;
use homesmart_cam_proto as proto;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::sync::Mutex;

pub struct Announcer {
    daemon: ServiceDaemon,
    /// Nom complet de l'annonce en cours (à retirer avant d'en publier une autre).
    current: Mutex<Option<String>>,
}

/// Nom de l'instance : le nom de la caméra et la fin de son identifiant (unique sur le réseau).
pub fn instance_name(name: &str, id: &str) -> String {
    let clean: String = name.chars().filter(|c| *c != '.' && !c.is_control()).collect();
    let short = &id[id.len().saturating_sub(4)..];
    let base: String = clean.trim().chars().take(48).collect();
    format!("{} {short}", if base.is_empty() { MODEL } else { &base })
}

impl Announcer {
    pub fn new() -> Result<Announcer> {
        Ok(Announcer { daemon: ServiceDaemon::new()?, current: Mutex::new(None) })
    }

    /// (Re)publie l'annonce : nom, identifiant, ports, état d'appairage.
    pub fn publish(&self, shared: &Shared) -> Result<()> {
        let info = shared.info();
        let api_port = shared.cfg.lock().unwrap().api_port;
        let props = [
            ("id", info.id.clone()),
            ("name", info.name.clone()),
            ("model", info.model.clone()),
            ("board", info.board.clone()),
            ("proto", proto::PROTOCOL.to_string()),
            ("rtsp", info.rtsp_port.to_string()),
            ("paired", if info.paired { "1".into() } else { "0".into() }),
            ("ver", info.version.clone()),
        ];
        let host = format!("{}.local.", crate::board::hostname());
        let service = ServiceInfo::new(proto::SERVICE_TYPE, &instance_name(&info.name, &info.id), &host, "", api_port, &props[..])?.enable_addr_auto();
        let mut current = self.current.lock().unwrap();
        if let Some(old) = current.take() {
            if old != service.get_fullname() {
                let _ = self.daemon.unregister(&old);
            }
        }
        *current = Some(service.get_fullname().to_string());
        self.daemon.register(service)?;
        Ok(())
    }
}

impl Drop for Announcer {
    fn drop(&mut self) {
        if let Some(name) = self.current.lock().unwrap().take() {
            // Annonce de départ, pour que HomeSmart ne garde pas une caméra éteinte.
            if let Ok(rx) = self.daemon.unregister(&name) {
                let _ = rx.recv_timeout(std::time::Duration::from_millis(500));
            }
        }
        let _ = self.daemon.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names() {
        assert_eq!(instance_name("Salon", "70c9324b2bdc"), "Salon 2bdc");
        assert_eq!(instance_name("  ", "70c9324b2bdc"), "Caméra HomeSmart 2bdc");
        assert_eq!(instance_name("a.b", "x1"), "ab x1");
    }
}
