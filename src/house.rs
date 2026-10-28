//! Plan de la maison : pièces rectangulaires (en mètres) et position des appareils.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Room {
    pub id: u64,
    pub name: String,
    /// Coin nord-ouest (x vers l'est, y vers le sud), en mètres.
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub d: f32,
}

impl Room {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.d
    }

    pub fn center(&self) -> [f32; 2] {
        [self.x + self.w / 2.0, self.y + self.d / 2.0]
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HousePlan {
    #[serde(default)]
    pub rooms: Vec<Room>,
    /// Identifiant d'appareil → position (x, y) en mètres.
    #[serde(default)]
    pub placements: BTreeMap<String, [f32; 2]>,
    #[serde(default = "default_height")]
    pub wall_height: f32,
}

fn default_height() -> f32 {
    2.5
}

impl Default for HousePlan {
    fn default() -> Self {
        HousePlan { rooms: Vec::new(), placements: BTreeMap::new(), wall_height: default_height() }
    }
}

impl HousePlan {
    pub fn is_empty(&self) -> bool {
        self.rooms.is_empty() && self.placements.is_empty()
    }

    pub fn next_id(&self) -> u64 {
        self.rooms.iter().map(|r| r.id).max().unwrap_or(0) + 1
    }

    /// Pièces créées à partir des noms de pièce des appareils, côte à côte sur deux rangées.
    pub fn from_room_names(names: &[String]) -> HousePlan {
        let mut plan = HousePlan::default();
        let (w, d) = (4.5f32, 4.0f32);
        let per_row = names.len().div_ceil(2).max(1);
        for (i, name) in names.iter().enumerate() {
            let (col, row) = (i % per_row, i / per_row);
            plan.rooms.push(Room { id: i as u64 + 1, name: name.clone(), x: col as f32 * w, y: row as f32 * d, w, d });
        }
        plan
    }

    pub fn room_named(&self, name: &str) -> Option<&Room> {
        self.rooms.iter().find(|r| r.name.eq_ignore_ascii_case(name))
    }

    pub fn room_at(&self, x: f32, y: f32) -> Option<&Room> {
        self.rooms.iter().find(|r| r.contains(x, y))
    }

    /// Emprise du plan (x0, y0, x1, y1), au moins 4 × 4 m.
    pub fn bounds(&self) -> [f32; 4] {
        let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        for r in &self.rooms {
            b = [b[0].min(r.x), b[1].min(r.y), b[2].max(r.x + r.w), b[3].max(r.y + r.d)];
        }
        for p in self.placements.values() {
            b = [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])];
        }
        if b[0] > b[2] {
            return [0.0, 0.0, 4.0, 4.0];
        }
        b
    }

    /// Position d'un appareil : enregistrée, sinon répartie dans sa pièce, sinon devant la maison.
    pub fn position_of(&self, id: &str, room: Option<&str>, index_in_room: usize) -> [f32; 2] {
        if let Some(p) = self.placements.get(id) {
            return *p;
        }
        match room.and_then(|r| self.room_named(r)) {
            Some(r) => {
                let cols = 2usize;
                let (c, l) = ((index_in_room % cols) as f32, (index_in_room / cols) as f32);
                [r.x + r.w * (0.28 + 0.44 * c), r.y + r.d * (0.42 + 0.3 * l).min(0.85)]
            }
            None => {
                let b = self.bounds();
                [b[0] + 0.6 + index_in_room as f32 * 1.0, b[3] + 1.0]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rooms_from_names_and_positions() {
        let plan = HousePlan::from_room_names(&["Salon".into(), "Chambre".into(), "Bureau".into()]);
        assert_eq!(plan.rooms.len(), 3);
        let p = plan.position_of("x", Some("Chambre"), 0);
        assert_eq!(plan.room_at(p[0], p[1]).unwrap().name, "Chambre");
        // Appareil sans pièce : devant la maison, hors des pièces.
        let q = plan.position_of("y", None, 0);
        assert!(plan.room_at(q[0], q[1]).is_none());
    }
}
