// SPDX-License-Identifier: MPL-2.0
//! Collision layers (specs/15 § 3): up to 32 named layers from `config/physics.ron` with a
//! symmetric collision matrix and a separate query matrix, mapped to rapier interaction groups.
//!
//! The file names layers, then lists **pairs**:
//!
//! ```ron
//! (
//!     layers: ["default", "player", "trigger"],
//!     // symmetric: a pair collides in both directions; writing one direction is enough
//!     collides: [("default", "default"), ("player", "default"), ("player", "trigger")],
//!     // (query layer, seen layer): queries belonging to "player" can see these layers
//!     queries: [("player", "default"), ("player", "trigger")],
//! )
//! ```
//!
//! Colliders get interaction groups `{ memberships: 1 << layer, filter: collision_row }` —
//! rapier's pairwise test `(a.m & b.f) && (b.m & a.f)` then implements the symmetric matrix
//! exactly. Queries get `{ memberships: ALL, filter: requested ∩ query_row }`: a query
//! belonging to layer `q` only sees the layers row `q` of the query matrix allows. A layer
//! that collides with nothing has an empty filter and is invisible to queries as well
//! (rapier tests the query's memberships against the collider's own filter).
//!
//! Provenance: pair-test semantics of rapier `InteractionGroups::test` (rapier3d public API,
//! Apache-2.0).

use rapier3d::geometry::{Group, InteractionGroups, InteractionTestMode};
use regia_ecs::prelude::*;
use thiserror::Error;

/// The maximum number of collision layers (one bitmask row of 32 bits).
pub const MAX_LAYERS: usize = 32;

/// A problem with `config/physics.ron`.
#[derive(Debug, Error)]
pub enum LayerConfigError {
    /// The file could not be read.
    #[error("physics layer config: {0}")]
    Io(#[from] std::io::Error),
    /// The file is not valid RON for the layer schema.
    #[error("physics layer config: {0}")]
    Parse(#[from] ron::error::SpannedError),
    /// More than [`MAX_LAYERS`] layers.
    #[error("physics layer config: {0} layers (max {MAX_LAYERS})")]
    TooManyLayers(usize),
    /// A pair names a layer that is not in `layers`.
    #[error("physics layer config: unknown layer '{0}'")]
    UnknownLayer(String),
}

/// The project-relative path of the layer config (`config/physics.ron`).
pub const LAYER_CONFIG_PATH: &str = "config/physics.ron";

/// The parsed shape of `config/physics.ron`.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct LayerFile {
    /// The named layers, in bitmask order.
    pub layers: Vec<String>,
    /// Pairs of layers that generate contacts/events. Symmetric; one direction suffices.
    pub collides: Vec<(String, String)>,
    /// `(query layer, seen layer)` pairs: the query matrix.
    pub queries: Vec<(String, String)>,
}

impl LayerFile {
    /// Parses a config from RON text.
    pub fn parse(text: &str) -> Result<Self, LayerConfigError> {
        Ok(ron::from_str(text)?)
    }

    /// Reads and parses `config/physics.ron` (or any path).
    pub fn load(path: &std::path::Path) -> Result<Self, LayerConfigError> {
        Self::parse(&std::fs::read_to_string(path)?)
    }

    /// The config as pretty RON that [`Self::parse`] reads back to an equal value (the editor's
    /// layer matrix writes this; comments are not kept, ADR-7233).
    pub fn to_ron(&self) -> Result<String, ron::Error> {
        let mut text = ron::ser::to_string_pretty(self, regia_core::ron_pretty())?;
        text.push('\n');
        Ok(text)
    }
}

impl Default for LayerFile {
    fn default() -> Self {
        Self {
            layers: vec!["default".into()],
            collides: vec![("default".into(), "default".into())],
            queries: vec![("default".into(), "default".into())],
        }
    }
}

/// The runtime collision-layer table (specs/15 § 3): names plus the collision and query
/// matrices as bitmask rows.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct PhysicsLayers {
    /// Layer names in bitmask order.
    pub names: Vec<String>,
    /// `collision_rows[i]` — the layers layer `i` collides with (symmetric matrix).
    pub collision_rows: [u32; MAX_LAYERS],
    /// `query_rows[i]` — the layers queries belonging to layer `i` can see.
    pub query_rows: [u32; MAX_LAYERS],
}

impl Default for PhysicsLayers {
    fn default() -> Self {
        // One "default" layer that collides with and is visible to itself. Rows past the
        // layer list are unused (and read as "collides with nothing" if referenced).
        let mut collision_rows = [0u32; MAX_LAYERS];
        let mut query_rows = [0u32; MAX_LAYERS];
        collision_rows[0] = 1;
        query_rows[0] = 1;
        Self {
            names: vec!["default".into()],
            collision_rows,
            query_rows,
        }
    }
}

impl PhysicsLayers {
    /// Builds the layer table from a parsed config, symmetrizing the collision matrix.
    pub fn from_file(file: &LayerFile) -> Result<Self, LayerConfigError> {
        if file.layers.len() > MAX_LAYERS {
            return Err(LayerConfigError::TooManyLayers(file.layers.len()));
        }
        let index_of = |name: &str| -> Result<u16, LayerConfigError> {
            file.layers
                .iter()
                .position(|n| n == name)
                .map(|i| i as u16)
                .ok_or_else(|| LayerConfigError::UnknownLayer(name.to_owned()))
        };
        let mut collision_rows = [0u32; MAX_LAYERS];
        let mut query_rows = [0u32; MAX_LAYERS];
        for (a, b) in &file.collides {
            let (ia, ib) = (index_of(a)?, index_of(b)?);
            // Symmetric: one direction in the file enables both.
            collision_rows[ia as usize] |= 1 << ib;
            collision_rows[ib as usize] |= 1 << ia;
        }
        for (from, seen) in &file.queries {
            let (i, j) = (index_of(from)?, index_of(seen)?);
            query_rows[i as usize] |= 1 << j;
        }
        Ok(Self {
            names: file.layers.clone(),
            collision_rows,
            query_rows,
        })
    }

    /// The index of a named layer, if present.
    pub fn index_of(&self, name: &str) -> Option<u16> {
        self.names.iter().position(|n| n == name).map(|i| i as u16)
    }

    /// The query row of layer `index`; an index past the table sees nothing.
    fn query_row(&self, index: u16) -> u32 {
        self.query_rows.get(index as usize).copied().unwrap_or(0)
    }

    /// Interaction groups for a collider on `layer`: implements the collision matrix.
    pub fn interaction_groups(&self, layer: &crate::components::CollisionLayer) -> InteractionGroups {
        // An index past the table has no row: it is a member of nothing and collides with nothing.
        let (member, filter) = match self.collision_rows.get(layer.index as usize) {
            Some(&row) => (1u32 << layer.index, row),
            None => (0, 0),
        };
        InteractionGroups::new(
            Group::from_bits_retain(member),
            Group::from_bits_retain(filter),
            InteractionTestMode::And,
        )
    }

    /// Interaction groups for a query: `memberships = ALL` (a query is not a collider), the
    /// filter is the requested layer mask intersected with the query matrix row of `from`.
    pub fn query_groups(
        &self,
        from: Option<&crate::components::CollisionLayer>,
        requested_mask: Option<u32>,
    ) -> InteractionGroups {
        let mask = match (from, requested_mask) {
            (Some(layer), Some(mask)) => mask & self.query_row(layer.index),
            (Some(layer), None) => self.query_row(layer.index),
            (None, mask) => mask.unwrap_or(u32::MAX),
        };
        InteractionGroups::new(
            Group::all(),
            Group::from_bits_retain(mask),
            InteractionTestMode::And,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::CollisionLayer;

    #[test]
    fn a_layer_index_past_the_table_collides_and_queries_with_nothing() {
        let layers = PhysicsLayers::default();
        for index in [32u16, 64, 1000] {
            let layer = CollisionLayer::new(index);
            let groups = layers.interaction_groups(&layer);
            assert_eq!(groups.memberships.bits(), 0, "index {index}");
            assert_eq!(groups.filter.bits(), 0, "index {index}");
            assert_eq!(layers.query_groups(Some(&layer), None).filter.bits(), 0);
            assert_eq!(layers.query_groups(Some(&layer), Some(u32::MAX)).filter.bits(), 0);
        }
        let default = layers.interaction_groups(&CollisionLayer::new(0));
        assert_eq!((default.memberships.bits(), default.filter.bits()), (1, 1));
    }

    #[test]
    fn default_is_one_self_colliding_layer() {
        let layers = PhysicsLayers::default();
        assert_eq!(layers.names, vec!["default"]);
        assert_eq!(layers.collision_rows[0], 1);
        assert_eq!(layers.query_rows[0], 1);
    }

    #[test]
    fn parse_and_build_from_ron() {
        let file = LayerFile::parse(
            r#"( layers: ["world", "player", "trigger"],
                 collides: [("world", "world"), ("player", "world"), ("trigger", "player")],
                 queries: [("player", "world"), ("player", "trigger")] )"#,
        )
        .expect("parses");
        let layers = PhysicsLayers::from_file(&file).expect("builds");
        assert_eq!(layers.names.len(), 3);
        assert_eq!(layers.index_of("player"), Some(1));
        // world ↔ world, world ↔ player (symmetrized from ("player", "world"))
        assert_eq!(layers.collision_rows[0], 0b011);
        // player ↔ world, player ↔ trigger (symmetrized from ("trigger", "player"))
        assert_eq!(layers.collision_rows[1], 0b101);
        // trigger ↔ player
        assert_eq!(layers.collision_rows[2], 0b010);
    }

    #[test]
    fn to_ron_round_trips() {
        let file = LayerFile {
            layers: vec!["world".into(), "pla\"yer".into()],
            collides: vec![("world".into(), "pla\"yer".into())],
            queries: vec![("pla\"yer".into(), "world".into())],
        };
        let text = file.to_ron().expect("serializes");
        assert_eq!(LayerFile::parse(&text).expect("parses back"), file);
    }

    #[test]
    fn collision_matrix_symmetrizes() {
        let file = LayerFile {
            layers: vec!["a".into(), "b".into()],
            collides: vec![("a".into(), "b".into())], // only one direction written
            queries: vec![],
        };
        let layers = PhysicsLayers::from_file(&file).expect("builds");
        assert_eq!(layers.collision_rows[0], 0b10);
        assert_eq!(layers.collision_rows[1], 0b01);
    }

    #[test]
    fn unknown_layer_in_pairs_is_an_error() {
        let file = LayerFile {
            layers: vec!["a".into()],
            collides: vec![("a".into(), "ghost".into())],
            queries: vec![],
        };
        let err = match PhysicsLayers::from_file(&file) {
            Err(error) => error,
            Ok(layers) => panic!("expected an error, built {layers:?}"),
        };
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn more_than_32_layers_is_an_error() {
        let file = LayerFile {
            layers: (0..33).map(|i| format!("l{i}")).collect(),
            collides: vec![],
            queries: vec![],
        };
        let err = match PhysicsLayers::from_file(&file) {
            Err(error) => error,
            Ok(layers) => panic!("expected an error, built {layers:?}"),
        };
        assert!(err.to_string().contains("33"), "{err}");
    }

    #[test]
    fn interaction_groups_follow_the_matrix() {
        let file = LayerFile {
            layers: vec!["a".into(), "b".into()],
            collides: vec![("a".into(), "a".into())], // a collides with itself only
            queries: vec![],
        };
        let layers = PhysicsLayers::from_file(&file).expect("builds");
        let ga = layers.interaction_groups(&CollisionLayer::new(0));
        let gb = layers.interaction_groups(&CollisionLayer::new(1));
        // rapier pair test: (a.m & b.f) && (b.m & a.f)
        assert!(!ga.test(gb), "a and b do not collide");
        assert!(ga.test(ga), "a collides with itself");
        assert!(!gb.test(gb), "b collides with nothing");
    }

    #[test]
    fn query_groups_follow_the_query_matrix() {
        let file = LayerFile {
            layers: vec!["world".into(), "player".into(), "trigger".into()],
            collides: vec![
                ("world".into(), "world".into()),
                ("player".into(), "world".into()),
                ("player".into(), "trigger".into()),
            ],
            queries: vec![("player".into(), "world".into())],
        };
        let layers = PhysicsLayers::from_file(&file).expect("builds");
        // A player query sees only "world" (not "trigger", not "player").
        let g = layers.query_groups(Some(&CollisionLayer::new(1)), None);
        let world = layers.interaction_groups(&CollisionLayer::new(0));
        let trigger = layers.interaction_groups(&CollisionLayer::new(2));
        assert!(g.test(world), "player query sees world");
        assert!(!g.test(trigger), "player query cannot see trigger");
        // Without a from-layer the query sees everything that collides with anything.
        let all = layers.query_groups(None, None);
        assert!(all.test(world));
    }

    #[test]
    fn layer_file_default_loads() {
        let layers = PhysicsLayers::from_file(&LayerFile::default()).expect("builds");
        assert_eq!(layers, PhysicsLayers::default());
    }

    /// The checked-in example (`config/physics.ron`) parses and builds — guards it against
    /// drifting from the schema.
    #[test]
    fn repo_example_config_parses_and_builds() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/physics.ron");
        let file = LayerFile::load(&path).expect("config/physics.ron parses");
        let layers = PhysicsLayers::from_file(&file).expect("config/physics.ron builds");
        assert!(layers.names.len() >= 2);
        assert_eq!(layers.names[0], "world");
        // The matrix is symmetric: world ↔ player from one written direction.
        let world = layers.index_of("world").expect("world");
        let player = layers.index_of("player").expect("player");
        assert!(layers.collision_rows[world as usize] & (1 << player) != 0);
        assert!(layers.collision_rows[player as usize] & (1 << world) != 0);
        // Debris collides with the world only — not with itself or the player.
        let debris = layers.index_of("debris").expect("debris");
        assert_eq!(layers.collision_rows[debris as usize], 1 << world);
        // Player queries see world and trigger.
        let trigger = layers.index_of("trigger").expect("trigger");
        let row = layers.query_rows[player as usize];
        assert_eq!(row, (1 << world) | (1 << trigger));
    }
}
