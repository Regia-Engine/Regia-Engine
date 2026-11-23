// SPDX-License-Identifier: MPL-2.0
//! Seeded procedural scatter (spec 26 § 1): deterministic placement of instanced props driven
//! by a frozen RON format (`docs/formats/terrain.md` § 2).
//!
//! - [`set`]: the `.scatter.ron` v2 format — a [`ScatterSet`] of classes, each with weighted glTF
//!   meshes, density rules and look parameters. v1 files ([`ScatterRule`], below) convert on load.
//! - [`placement`]: deterministic placement of a set over a [`TerrainMap`].
//! - [`field`]: the runtime — per-cell culling, LOD and distance fade, emitting
//!   [`regia_render::FrameMesh`]es that the frame renderer's GPU culler (frustum + Hi-Z) and
//!   multi-draw-indirect path draw as instances.
//! - [`contact`]: the soft contact-AO / base-blend map the terrain shader samples under rocks.
//! - [`rock`]: procedural stand-in rock meshes (tests, evidence, maps without authored rocks).
//! - [`impostor`]: octahedral impostor cards for the far field (opt-in, ADR-116).
//! - [`detail`]: opt-in screen-size fade and small-caster limits for dense scatter.
//! - [`clump`]: clumps of pebbles merged into one scatter mesh, and a coarse vertex-clustered LOD.
//! - [`runtime`]: a field drawn by the engine's ECS frame (no entity per instance).
//! - [`painted`]: hand-placed instances, stored per tile and drawn through the same field (ADR-1936).
//!
//! The rest of this file is the legacy v1 single-rule API ([`ScatterRule`], [`place`]), kept for
//! the offscreen terrain renderer and v1 loading.
//!
//! Determinism is per cell: cell `(cx, cz)` draws its randomness from a PCG32 generator seeded
//! by a SplitMix64-style mix of the rule seed and the cell coordinates, so the placement is
//! identical for every client, every run and every iteration order (DO27 requirement). All
//! arithmetic is `f32` with a fixed operation order; nothing reads the GPU, the clock or the
//! platform.

use regia_core::math::Rng;
use regia_core::version::{self, VersionStep};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::holes::HolesMask;
use crate::layers::TerrainLayers;
use crate::sampler::TerrainMap;

pub mod caster;
pub mod clump;
// Gameplay colliders baked from the scatter placement, and their push-out query (Q-08, ADR-8029).
pub mod colliders;
pub mod contact;
pub mod control;
pub mod detail;
pub mod field;
pub mod impostor;
pub mod painted;
pub mod placement;
pub mod rock;
pub mod runtime;
pub mod season;
pub mod set;
pub mod size_intent;
// Dense-forest flythrough: clearance queries, corridor carve, canopy gaps, motion LOD, wake sway (ADR-1503).
pub mod forest;

pub use set::{
    COOKED_SCATTER_VERSION, SCATTER_RULE_V1_VERSION, SCATTER_SET_VERSION, SCATTER_TYPE_PATH, ScatterClass,
    ScatterExclusion, ScatterMeshRef, ScatterSet, scatter_migration_sources, scatter_schema_fingerprint,
};

/// Version of the `.scatter.ron` format this build reads and writes ([`ScatterSet`]).
pub const SCATTER_FORMAT_VERSION: u32 = SCATTER_SET_VERSION;

/// Errors returned for invalid scatter rules.
#[derive(Clone, Debug, Error)]
pub enum ScatterError {
    /// Unknown format version.
    #[error("unsupported scatter rule version {found}, expected {expected}")]
    BadVersion {
        /// Version found in the file.
        found: u32,
        /// Version this build reads/writes.
        expected: u32,
    },
    /// A numeric parameter is out of range (finite, positive where required, min ≤ max).
    #[error("scatter rule field '{field}' has invalid value {value}")]
    BadParameter {
        /// Field name.
        field: &'static str,
        /// Offending value.
        value: f32,
    },
    /// `layer_index` exceeds the layer stack.
    #[error("layer index {index} exceeds the {count}-layer stack")]
    BadLayerIndex {
        /// The referenced layer index.
        index: u8,
        /// Layers in the stack.
        count: usize,
    },
    /// A migration step failed or is missing.
    #[error("scatter rule migration failed: {0}")]
    Migration(String),
    /// A v2 class is invalid.
    #[error("scatter class '{class}': {reason}")]
    Class {
        /// Class name.
        class: String,
        /// What is wrong.
        reason: String,
    },
    /// A referenced asset (mesh or density map) was not supplied.
    #[error("scatter asset '{path}' is missing or unusable: {reason}")]
    Asset {
        /// Asset path from the `.scatter.ron`.
        path: String,
        /// What is wrong.
        reason: String,
    },
}

/// The legacy v1 scatter rule: one mesh, one filter set (`.scatter.ron` format v1). New files use
/// [`ScatterSet`]; [`ScatterSet::from_ron`] converts a v1 file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScatterRule {
    /// Format version; loaders reject unknown versions.
    pub version: u32,
    /// Deterministic seed. The same seed gives the same placement on every client.
    pub seed: u64,
    /// Asset path of the instanced mesh (glTF; wiring lands with the asset graph).
    pub mesh: String,
    /// Placement cell size, metres. Determinism is per cell.
    pub cell_size: f32,
    /// Expected instances per cell (fractional; the fraction is a probability).
    pub instances_per_cell: f32,
    /// Minimum uniform scale.
    pub min_scale: f32,
    /// Maximum uniform scale.
    pub max_scale: f32,
    /// Yaw is uniform in `0..yaw_jitter_radians`.
    pub yaw_jitter_radians: f32,
    /// Tilt instances to follow the surface normal (blended by `tilt_max_radians`).
    pub align_to_normal: bool,
    /// Maximum tilt from vertical when aligned, radians.
    pub tilt_max_radians: f32,
    /// Height filter band, metres.
    pub height_min: f32,
    /// Height filter band, metres.
    pub height_max: f32,
    /// Maximum slope (radians from up); steeper ground is rejected.
    pub slope_max: f32,
    /// Only place where this material layer's weight passes `layer_min_weight` (optional).
    pub layer_index: Option<u8>,
    /// Minimum layer weight when `layer_index` is set.
    pub layer_min_weight: f32,
    /// Skip cells inside terrain holes (tunnel entrances).
    pub avoid_holes: bool,
    /// Distance where instance fading starts, metres.
    pub fade_start_distance: f32,
    /// Distance where instances are fully faded, metres.
    pub fade_end_distance: f32,
}

impl Default for ScatterRule {
    fn default() -> Self {
        Self {
            version: SCATTER_RULE_V1_VERSION,
            seed: 2747,
            mesh: "models/rock_small.gltf".into(),
            cell_size: 8.0,
            instances_per_cell: 0.7,
            min_scale: 0.6,
            max_scale: 1.8,
            yaw_jitter_radians: std::f32::consts::TAU,
            align_to_normal: false,
            tilt_max_radians: 0.12,
            height_min: -1000.0,
            height_max: 1000.0,
            slope_max: 0.7,
            layer_index: None,
            layer_min_weight: 0.5,
            avoid_holes: true,
            fade_start_distance: 220.0,
            fade_end_distance: 320.0,
        }
    }
}

/// Smallest accepted `cell_size` in metres (bounds the cell grid walked by `place`).
const MIN_CELL_SIZE: f32 = 0.25;
/// Largest accepted `instances_per_cell` (bounds the per-cell placement loop).
const MAX_INSTANCES_PER_CELL: f32 = 1024.0;

impl ScatterRule {
    /// Validates the rule against an optional layer stack.
    ///
    /// # Errors
    ///
    /// See [`ScatterError`].
    pub fn validate(&self, layers: Option<&TerrainLayers>) -> Result<(), ScatterError> {
        if self.version != SCATTER_RULE_V1_VERSION {
            return Err(ScatterError::BadVersion {
                found: self.version,
                expected: SCATTER_RULE_V1_VERSION,
            });
        }
        let bad = |field: &'static str, value: f32| ScatterError::BadParameter { field, value };
        for (field, value) in [
            ("cell_size", self.cell_size),
            ("instances_per_cell", self.instances_per_cell),
            ("min_scale", self.min_scale),
            ("max_scale", self.max_scale),
            ("yaw_jitter_radians", self.yaw_jitter_radians),
            ("tilt_max_radians", self.tilt_max_radians),
            ("slope_max", self.slope_max),
            ("layer_min_weight", self.layer_min_weight),
            ("fade_start_distance", self.fade_start_distance),
            ("fade_end_distance", self.fade_end_distance),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(bad(field, value));
            }
        }
        if self.cell_size < MIN_CELL_SIZE {
            return Err(bad("cell_size", self.cell_size));
        }
        if self.instances_per_cell > MAX_INSTANCES_PER_CELL {
            return Err(bad("instances_per_cell", self.instances_per_cell));
        }
        if self.max_scale < self.min_scale || self.min_scale <= 0.0 {
            return Err(bad("max_scale", self.max_scale));
        }
        if self.height_max < self.height_min || !self.height_min.is_finite() || !self.height_max.is_finite() {
            return Err(bad("height_max", self.height_max));
        }
        if self.slope_max <= 0.0 || self.slope_max > std::f32::consts::FRAC_PI_2 + 0.1 {
            return Err(bad("slope_max", self.slope_max));
        }
        if self.fade_end_distance < self.fade_start_distance {
            return Err(bad("fade_end_distance", self.fade_end_distance));
        }
        if let Some(index) = self.layer_index
            && let Some(layers) = layers
            && usize::from(index) >= layers.count()
        {
            return Err(ScatterError::BadLayerIndex {
                index,
                count: layers.count(),
            });
        }
        Ok(())
    }

    /// Serialises to a RON string.
    ///
    /// # Errors
    ///
    /// See [`ScatterRule::validate`] and [`crate::TerrainError::Ron`].
    pub fn to_ron(&self) -> Result<String, crate::TerrainError> {
        self.validate(None)?;
        // `\n` on every OS (ron's default is `\r\n` on Windows), so files match across hosts.
        Ok(ron::ser::to_string_pretty(self, regia_core::ron_pretty())?)
    }

    /// Parses and validates a RON string against an optional layer stack.
    ///
    /// # Errors
    ///
    /// See [`ScatterRule::validate`] and [`crate::TerrainError::Ron`]. The `version` field is
    /// peeked before the full deserialise, so a *newer* version is rejected even when its body no
    /// longer matches the current struct.
    pub fn from_ron(text: &str, layers: Option<&TerrainLayers>) -> Result<Self, crate::TerrainError> {
        let version = regia_core::version::peek_ron_version(text).map_err(|e| {
            crate::TerrainError::InvalidScatter(ScatterError::Migration(format!("bad version: {e}")))
        })?;
        if version > SCATTER_RULE_V1_VERSION {
            return Err(crate::TerrainError::InvalidScatter(ScatterError::BadVersion {
                found: version,
                expected: SCATTER_RULE_V1_VERSION,
            }));
        }
        let mut parsed: Self = ron::from_str(text)?;
        parsed.migrate_to_latest()?;
        parsed.validate(layers)?;
        Ok(parsed)
    }

    /// Reject a newer version, or run the registered migrator chain to bring an older rule up to
    /// [`SCATTER_RULE_V1_VERSION`] (`docs/specs/12-assets.md` § 4a). v1 is the only rule
    /// version; the v1 → v2 step is [`ScatterSet::from_v1`].
    pub fn migrate_to_latest(&mut self) -> Result<u32, ScatterError> {
        if self.version > SCATTER_RULE_V1_VERSION {
            return Err(ScatterError::BadVersion {
                found: self.version,
                expected: SCATTER_RULE_V1_VERSION,
            });
        }
        if self.version < SCATTER_RULE_V1_VERSION {
            let to = version::migrate(self, self.version, builtin_migrations(), SCATTER_RULE_V1_VERSION)
                .map_err(|e| ScatterError::Migration(e.to_string()))?;
            self.version = to;
        }
        Ok(self.version)
    }
}

/// The built-in in-place migration chain of the v1 rule (empty: v1 is its first version).
pub fn builtin_migrations() -> &'static [VersionStep<ScatterRule>] {
    &[]
}

/// One placed instance (world space).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScatterInstance {
    /// World-space position (on the terrain surface), metres.
    pub position: [f32; 3],
    /// Yaw about +Y, radians.
    pub yaw: f32,
    /// Uniform scale.
    pub scale: f32,
    /// Surface normal at the position (for aligned drawing).
    pub normal: [f32; 3],
    /// Per-instance albedo: a natural rock/dirt hue (the terrain layer palette) with
    /// per-instance brightness variation.
    pub tint: [f32; 3],
}

/// Seed for one cell: SplitMix64-style avalanche of the rule seed and the cell coordinates
/// (Steele, Lea & Flood, "Fast Splittable Pseudorandom Number Generators", 2014 — public
/// domain constants), fed to PCG32.
fn cell_seed(seed: u64, cx: i32, cz: i32) -> u64 {
    let cx64 = cx as u32 as u64;
    let cz64 = cz as u32 as u64;
    let mut z = seed
        ^ cx64
            .wrapping_mul(0x94d0_49bb_1331_11eb)
            .wrapping_add(0x9e37_79b9_7f4a_7c15)
            .rotate_left(17)
        ^ cz64.wrapping_mul(0xbf58_476d_1ce4_e5b9).rotate_left(31);
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Places the instances of one cell. Public so callers can stream cells in any order with
/// identical results.
///
/// Returns an empty vector when the cell fails the filters (all instances of a cell inside a
/// hole, etc.).
pub fn place_cell(
    map: &TerrainMap,
    mask: Option<&HolesMask>,
    layers: Option<&TerrainLayers>,
    rule: &ScatterRule,
    cx: i32,
    cz: i32,
) -> Vec<ScatterInstance> {
    let half = map.world_width() * 0.5;
    let cell = rule.cell_size;
    let base_x = -half + cx as f32 * cell;
    let base_z = -half + cz as f32 * cell;
    // Whole cell outside the map: nothing to place.
    if base_x + cell <= -half || base_x >= half || base_z + cell <= -half || base_z >= half {
        return Vec::new();
    }

    let mut rng = Rng::with_stream(cell_seed(rule.seed, cx, cz), 0x5c17_cea5_5eed_1a57);
    let full = rule.instances_per_cell.floor();
    let fraction = rule.instances_per_cell - full;
    let count = if fraction > 0.0 && rng.chance(fraction) {
        full as u32 + 1
    } else {
        full as u32
    };

    let mut out = Vec::new();
    for _ in 0..count {
        let x = base_x + rng.f32() * cell;
        let z = base_z + rng.f32() * cell;
        let y = map.sample_bilinear(x, z);
        if y < rule.height_min || y > rule.height_max {
            continue;
        }
        let normal = map.normal_at(x, z);
        // det_acos: the platform `acos` may differ in the last bit between targets (ADR-5629).
        let slope = regia_core::math::det_acos(normal[1].clamp(-1.0, 1.0));
        if slope > rule.slope_max {
            continue;
        }
        if let Some(mask) = mask
            && rule.avoid_holes
            && mask.contains(x, z)
        {
            continue;
        }
        if let (Some(index), Some(layers)) = (rule.layer_index, layers) {
            let weights = layers.weights_in(
                slope,
                y,
                Some({
                    let (lo, hi) = map.min_max_height();
                    [lo, hi]
                }),
            );
            let weight = weights.get(usize::from(index)).copied().unwrap_or(0.0);
            if weight < rule.layer_min_weight {
                continue;
            }
        }
        let yaw = if rule.yaw_jitter_radians > 0.0 {
            rng.f32() * rule.yaw_jitter_radians
        } else {
            0.0
        };
        let scale = rule.min_scale + (rule.max_scale - rule.min_scale) * rng.f32();
        // Albedo in the terrain layer palette: grey rock toward warm dirt, so instances
        // blend with the ground they sit on, with ±20% per-instance brightness.
        let t = rng.f32();
        let shade = 0.8 + rng.f32() * 0.4;
        out.push(ScatterInstance {
            position: [x, y, z],
            yaw,
            scale,
            normal,
            tint: [
                (0.40 - 0.08 * t) * shade,
                (0.38 - 0.16 * t) * shade,
                (0.36 - 0.23 * t) * shade,
            ],
        });
    }
    out
}

/// Places every instance of a rule over the whole map, cell by cell (row-major).
///
/// Deterministic: the same rule, map, mask and layers always produce the same vector, on every
/// client (spec 26 § 1, DO27 requirement).
pub fn place(
    map: &TerrainMap,
    mask: Option<&HolesMask>,
    layers: Option<&TerrainLayers>,
    rule: &ScatterRule,
) -> Vec<ScatterInstance> {
    let half = map.world_width() * 0.5;
    let cells = (half * 2.0 / rule.cell_size).ceil() as i32;
    let mut out = Vec::new();
    for cz in 0..cells {
        for cx in 0..cells {
            out.extend(place_cell(map, mask, layers, rule, cx, cz));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_runaway_density_and_cell_counts() {
        let tiny = ScatterRule {
            cell_size: 1e-4,
            ..ScatterRule::default()
        };
        assert!(tiny.validate(None).is_err());
        let dense = ScatterRule {
            instances_per_cell: 1e20,
            ..ScatterRule::default()
        };
        assert!(dense.validate(None).is_err());
        assert!(ScatterRule::default().validate(None).is_ok());
    }

    fn bowl_map() -> TerrainMap {
        // Gentle bowl with a steep cliff wall at +X: filters have something to bite on.
        let n = 129u32;
        let width = 128.0f32;
        let mut heights = Vec::with_capacity((n * n) as usize);
        for row in 0..n {
            for col in 0..n {
                let half = width * 0.5;
                let x = col as f32 / (n - 1) as f32 * width - half;
                let z = row as f32 / (n - 1) as f32 * width - half;
                let d = (x * x + z * z).sqrt();
                // Basin up to 40 m, then a steep wall.
                let wall = if x > 30.0 { (x - 30.0) * 1.8 } else { 0.0 };
                heights.push(40.0 - d * 0.3 + wall);
            }
        }
        TerrainMap::from_heights(n, n, heights, width, 1.0, 0.0).expect("valid map")
    }

    #[test]
    fn same_seed_same_placement() {
        let map = bowl_map();
        let rule = ScatterRule {
            instances_per_cell: 2.0,
            ..Default::default()
        };
        let a = place(&map, None, None, &rule);
        let b = place(&map, None, None, &rule);
        assert!(!a.is_empty());
        assert_eq!(a, b);
    }

    #[test]
    fn placement_is_independent_of_cell_iteration_order() {
        let map = bowl_map();
        let rule = ScatterRule {
            instances_per_cell: 1.7,
            ..Default::default()
        };
        let mut cells: Vec<(i32, i32)> = Vec::new();
        let cells_across = ((map.world_width() / rule.cell_size).ceil()) as i32;
        for cz in 0..cells_across {
            for cx in 0..cells_across {
                cells.push((cx, cz));
            }
        }
        let mut order = cells.clone();
        Rng::new(5).shuffle(&mut order);

        let mut shuffled: Vec<ScatterInstance> = order
            .iter()
            .flat_map(|&(cx, cz)| place_cell(&map, None, None, &rule, cx, cz))
            .collect();
        let reference = place(&map, None, None, &rule);
        // Order within the vector differs; the multiset must not.
        let key = |i: &ScatterInstance| {
            (
                i.position[0].to_bits(),
                i.position[1].to_bits(),
                i.position[2].to_bits(),
                i.yaw.to_bits(),
            )
        };
        shuffled.sort_by_key(key);
        let mut sorted_reference = reference;
        sorted_reference.sort_by_key(key);
        assert_eq!(shuffled, sorted_reference);
    }

    #[test]
    fn different_seed_changes_placement() {
        let map = bowl_map();
        let a = place(&map, None, None, &ScatterRule::default());
        let b = place(
            &map,
            None,
            None,
            &ScatterRule {
                seed: 999,
                ..Default::default()
            },
        );
        assert_ne!(a, b);
    }

    #[test]
    fn slope_filter_rejects_the_cliff() {
        let map = bowl_map();
        let rule = ScatterRule {
            slope_max: 0.35,
            ..Default::default()
        };
        for instance in place(&map, None, None, &rule) {
            let n = map.normal_at(instance.position[0], instance.position[2]);
            let slope = n[1].acos();
            assert!(
                slope <= 0.35 + 1e-3,
                "instance on slope {slope} at {:?}",
                instance.position
            );
        }
    }

    #[test]
    fn height_and_layer_filters_apply() {
        let map = bowl_map();
        let low_only = ScatterRule {
            height_min: -100.0,
            height_max: 25.0,
            ..Default::default()
        };
        assert!(
            place(&map, None, None, &low_only)
                .iter()
                .all(|i| i.position[1] <= 25.0)
        );

        let mut layers = TerrainLayers::default();
        layers.layers[0].altitude_max = 25.0;
        layers.layers.push(crate::layers::LayerDesc {
            name: "cliff".into(),
            slope_min: 0.05,
            slope_max: 1.6,
            ..crate::layers::LayerDesc::ground()
        });
        let cliff_only = ScatterRule {
            layer_index: Some(1),
            layer_min_weight: 0.5,
            ..Default::default()
        };
        let placed = place(&map, None, Some(&layers), &cliff_only);
        assert!(
            placed
                .iter()
                .all(|i| map.normal_at(i.position[0], i.position[2])[1].acos() > 0.04),
            "layer filter should only place on slopes"
        );
    }

    #[test]
    fn holes_exclude_instances() {
        let map = bowl_map();
        let mask = HolesMask::new(vec![crate::holes::HoleEllipse {
            center: [0.0, 0.0],
            radius: [60.0, 60.0],
            rotation_radians: 0.0,
        }])
        .expect("valid");
        let rule = ScatterRule {
            instances_per_cell: 3.0,
            ..Default::default()
        };
        let placed = place(&map, Some(&mask), None, &rule);
        assert!(!placed.is_empty());
        assert!(
            placed
                .iter()
                .all(|i| !mask.contains(i.position[0], i.position[2]))
        );
    }

    #[test]
    fn instances_sit_on_the_surface_with_jitter() {
        let map = bowl_map();
        let rule = ScatterRule {
            instances_per_cell: 4.0,
            min_scale: 0.5,
            max_scale: 2.5,
            ..Default::default()
        };
        let placed = place(&map, None, None, &rule);
        assert!(!placed.is_empty());
        for i in &placed {
            let h = map.sample_bilinear(i.position[0], i.position[2]);
            assert!((i.position[1] - h).abs() < 1e-3);
            assert!((0.5..=2.5).contains(&i.scale));
            assert!((0.0..std::f32::consts::TAU).contains(&i.yaw));
        }
    }

    #[test]
    fn ron_round_trip_and_validation() {
        let rule = ScatterRule::default();
        let text = rule.to_ron().expect("serialize");
        assert_eq!(ScatterRule::from_ron(&text, None).expect("parse"), rule);

        let mut bad = rule.clone();
        bad.version = 7;
        let text = ron::ser::to_string(&bad).expect("raw serialize");
        assert!(matches!(
            ScatterRule::from_ron(&text, None),
            Err(crate::TerrainError::InvalidScatter(ScatterError::BadVersion {
                found: 7,
                ..
            }))
        ));

        let mut bad = rule.clone();
        bad.cell_size = 0.0;
        assert!(matches!(
            bad.validate(None),
            Err(ScatterError::BadParameter {
                field: "cell_size",
                ..
            })
        ));

        let mut indexed = rule.clone();
        indexed.layer_index = Some(3);
        let stack = TerrainLayers::default();
        assert!(matches!(
            indexed.validate(Some(&stack)),
            Err(ScatterError::BadLayerIndex { index: 3, count: 1 })
        ));
    }
}
