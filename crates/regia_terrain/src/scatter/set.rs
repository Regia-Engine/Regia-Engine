// SPDX-License-Identifier: MPL-2.0
//! `.scatter.ron` format v2: a seeded set of scatter *classes* (boulders, gravel, debris), each
//! with a weighted list of cooked glTF meshes, density rules (density map, procedural noise,
//! slope / altitude / layer mask filters, exclusion paths) and its look (tint, base blend,
//! contact AO, LOD bias, distance fade).
//!
//! v1 files (one [`ScatterRule`] = one mesh) load through [`ScatterSet::from_ron`], which converts
//! them to a one-class v2 set (`docs/formats/terrain.md` § 2, ADR-047). The v1 → v2 step changes
//! the shape of the file, so it runs at parse time; `regia upgrade-project` re-saves the result.

use serde::{Deserialize, Serialize};

use super::{ScatterError, ScatterRule};
use crate::layers::{MAX_LAYERS, TerrainLayers};

/// Current version of the `.scatter.ron` format.
pub const SCATTER_SET_VERSION: u32 = 2;

/// Version of the legacy single-rule format ([`ScatterRule`]).
pub const SCATTER_RULE_V1_VERSION: u32 = 1;

/// Version of the cooked (pak) scatter payload ([`ScatterSet::to_cooked`]).
pub const COOKED_SCATTER_VERSION: u32 = 1;

/// Type path of a cooked scatter set in the asset manifest.
pub const SCATTER_TYPE_PATH: &str = "regia_terrain::ScatterSet";

/// A whole map's scatter (`.scatter.ron`, format v2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScatterSet {
    /// Format version; loaders reject unknown versions.
    pub version: u32,
    /// Deterministic seed. The same seed, map and file give the same placement on every client.
    pub seed: u64,
    /// Scatter classes, each placed independently (its stream is keyed by the class name, so
    /// reordering classes does not move anything).
    pub classes: Vec<ScatterClass>,
}

/// One cooked glTF mesh a class may place.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScatterMeshRef {
    /// Asset path of the glTF (`.glb`/`.gltf`). Authored LODs follow the glTF conventions
    /// (`_LOD0`, `_LOD1`, …); pivot at the centre of the base, +Y up, metres.
    pub mesh: String,
    /// Relative pick weight inside the class (> 0).
    #[serde(default = "one")]
    pub weight: f32,
    /// Minimum uniform scale.
    #[serde(default = "one")]
    pub min_scale: f32,
    /// Maximum uniform scale.
    #[serde(default = "one")]
    pub max_scale: f32,
    /// Size distribution exponent: scale = min + (max − min) · uᵇ. 1 is uniform; larger values
    /// make small instances common and big ones rare (natural talus).
    #[serde(default = "one")]
    pub scale_bias: f32,
}

fn one() -> f32 {
    1.0
}

/// An exclusion zone: a circle (one point) or a capsule chain along a path (two or more points,
/// e.g. a road centre line). Nothing of the class is placed within `radius` metres of it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScatterExclusion {
    /// World XZ points, metres.
    pub points: Vec<[f32; 2]>,
    /// Clearance, metres.
    pub radius: f32,
}

impl ScatterExclusion {
    /// True when `(x, z)` is inside the zone.
    #[must_use]
    pub fn contains(&self, x: f32, z: f32) -> bool {
        let r2 = self.radius * self.radius;
        match self.points.as_slice() {
            [] => false,
            [p] => {
                let (dx, dz) = (x - p[0], z - p[1]);
                dx * dx + dz * dz <= r2
            }
            points => points
                .windows(2)
                .any(|w| segment_distance_sq([x, z], w[0], w[1]) <= r2),
        }
    }
}

fn segment_distance_sq(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let ab = [b[0] - a[0], b[1] - a[1]];
    let ap = [p[0] - a[0], p[1] - a[1]];
    let len2 = ab[0] * ab[0] + ab[1] * ab[1];
    let t = if len2 > 0.0 {
        ((ap[0] * ab[0] + ap[1] * ab[1]) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let d = [ap[0] - ab[0] * t, ap[1] - ab[1] * t];
    d[0] * d[0] + d[1] * d[1]
}

/// One scatter class: what to place, where, how densely and how it looks.
///
/// Every field but `name` and `meshes` has a default, so a generator may emit only what it changes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScatterClass {
    /// Class name (unique in the set; seeds the class's random stream).
    pub name: String,
    /// Weighted meshes; at least one.
    pub meshes: Vec<ScatterMeshRef>,
    /// Placement cell size, metres. Determinism is per cell.
    pub cell_size: f32,
    /// Expected candidates per cell before filters (the fraction is a probability).
    pub density: f32,
    /// Optional density map: a grayscale PNG (8- or 16-bit) covering the whole terrain with the
    /// heightfield's mapping (column → +X, row → +Z); white keeps every candidate, black none.
    pub density_map: Option<String>,
    /// Feature size of the procedural density noise, metres (0 = off).
    pub noise_scale: f32,
    /// How strongly the noise thins the density (0 = off, 1 = bare patches).
    pub noise_amount: f32,
    /// Minimum slope, radians from up.
    pub slope_min: f32,
    /// Maximum slope, radians from up.
    pub slope_max: f32,
    /// Altitude band, metres.
    pub height_min: f32,
    /// Altitude band, metres.
    pub height_max: f32,
    /// Terrain layers the class grows on: bit *i* = layer *i*; 0 = any layer.
    pub layer_mask: u8,
    /// Minimum summed weight of the masked layers.
    pub layer_min_weight: f32,
    /// Skip terrain holes (tunnel entrances).
    pub avoid_holes: bool,
    /// Exclusion zones (roads, pads, set pieces).
    pub exclusions: Vec<ScatterExclusion>,
    /// Yaw is uniform in `0..yaw_jitter` radians.
    pub yaw_jitter: f32,
    /// How far the instance's up axis follows the ground normal (0 = upright, 1 = normal).
    pub align_to_normal: f32,
    /// Cap on the tilt from vertical, radians.
    pub tilt_max: f32,
    /// Random extra tilt, radians (natural toppled look).
    pub tilt_jitter: f32,
    /// Fraction of the mesh height buried below the ground (plus the slope residual).
    pub sink: f32,
    /// Linear multiplier on the mesh base colour.
    pub tint: [f32; 3],
    /// ± brightness variation between instances (0..=0.5).
    pub tint_jitter: f32,
    /// Linear colour the bottom of each instance blends toward (the terrain layer it sits in).
    pub base_tint: [f32; 3],
    /// Fraction of the mesh height the base blend covers (0 = off).
    pub base_blend: f32,
    /// Darkening of the terrain under each instance (0 = off, 1 = black core).
    pub contact_ao: f32,
    /// Contact shadow radius as a multiple of the instance footprint radius.
    pub contact_radius: f32,
    /// Multiplier on projected size before LOD selection (> 1 keeps detail longer).
    pub lod_bias: f32,
    /// Distance where instances start shrinking out, metres.
    pub fade_start: f32,
    /// Distance where instances are gone, metres.
    pub fade_end: f32,
    /// Instances cast sun and local-light shadows. Casters are drawn only into the shadow
    /// pages their footprint touches and into at most two sun cascades (the one that holds
    /// them and the next).
    pub cast_shadows: bool,
    /// Coarsest LOD that still casts (0..=3); coarser instances (a pixel or two on screen)
    /// cast nothing.
    pub shadow_max_lod: u8,
    /// Instances farther than this from the camera cast nothing, metres (0 = up to `fade_end`).
    pub shadow_distance: f32,
}

impl Default for ScatterClass {
    fn default() -> Self {
        Self {
            name: "rocks".into(),
            meshes: Vec::new(),
            cell_size: 8.0,
            density: 1.0,
            density_map: None,
            noise_scale: 0.0,
            noise_amount: 0.0,
            slope_min: 0.0,
            slope_max: 0.9,
            height_min: -100_000.0,
            height_max: 100_000.0,
            layer_mask: 0,
            layer_min_weight: 0.3,
            avoid_holes: true,
            exclusions: Vec::new(),
            yaw_jitter: std::f32::consts::TAU,
            align_to_normal: 0.6,
            tilt_max: 0.6,
            tilt_jitter: 0.12,
            sink: 0.15,
            tint: [1.0, 1.0, 1.0],
            tint_jitter: 0.15,
            base_tint: [0.5, 0.5, 0.5],
            base_blend: 0.0,
            contact_ao: 0.55,
            contact_radius: 1.6,
            lod_bias: 1.0,
            fade_start: 250.0,
            fade_end: 320.0,
            cast_shadows: true,
            shadow_max_lod: 3,
            shadow_distance: 0.0,
        }
    }
}

impl Default for ScatterSet {
    fn default() -> Self {
        Self {
            version: SCATTER_SET_VERSION,
            seed: 2747,
            classes: Vec::new(),
        }
    }
}

impl ScatterSet {
    /// Converts a v1 rule into a one-class v2 set that places the same mesh with the same filters.
    #[must_use]
    pub fn from_v1(rule: &ScatterRule) -> Self {
        let class = ScatterClass {
            name: "rocks".into(),
            meshes: vec![ScatterMeshRef {
                mesh: rule.mesh.clone(),
                weight: 1.0,
                min_scale: rule.min_scale,
                max_scale: rule.max_scale,
                scale_bias: 1.0,
            }],
            cell_size: rule.cell_size,
            density: rule.instances_per_cell,
            slope_min: 0.0,
            slope_max: rule.slope_max,
            height_min: rule.height_min,
            height_max: rule.height_max,
            layer_mask: rule
                .layer_index
                .map_or(0, |i| 1u8.checked_shl(u32::from(i)).unwrap_or(0)),
            layer_min_weight: rule.layer_min_weight,
            avoid_holes: rule.avoid_holes,
            yaw_jitter: rule.yaw_jitter_radians,
            align_to_normal: if rule.align_to_normal { 1.0 } else { 0.0 },
            tilt_max: rule.tilt_max_radians,
            tilt_jitter: 0.0,
            sink: 0.0,
            tint_jitter: 0.0,
            contact_ao: 0.0,
            fade_start: rule.fade_start_distance,
            fade_end: rule.fade_end_distance,
            ..ScatterClass::default()
        };
        Self {
            version: SCATTER_SET_VERSION,
            seed: rule.seed,
            classes: vec![class],
        }
    }

    /// Validates the set against an optional layer stack.
    ///
    /// # Errors
    ///
    /// See [`ScatterError`].
    pub fn validate(&self, layers: Option<&TerrainLayers>) -> Result<(), ScatterError> {
        if self.version != SCATTER_SET_VERSION {
            return Err(ScatterError::BadVersion {
                found: self.version,
                expected: SCATTER_SET_VERSION,
            });
        }
        let mut names = std::collections::HashSet::new();
        for class in &self.classes {
            if !names.insert(class.name.as_str()) {
                return Err(ScatterError::Class {
                    class: class.name.clone(),
                    reason: "duplicate class name".into(),
                });
            }
            class.validate(layers)?;
        }
        Ok(())
    }

    /// Serialises to a RON string.
    ///
    /// # Errors
    ///
    /// See [`ScatterSet::validate`] and [`crate::TerrainError::RonSer`].
    pub fn to_ron(&self) -> Result<String, crate::TerrainError> {
        self.validate(None)?;
        // `\n` on every OS (ron's default is `\r\n` on Windows), so files match across hosts.
        Ok(ron::ser::to_string_pretty(self, regia_core::ron_pretty())?)
    }

    /// Parses and validates a `.scatter.ron` of any supported version (v1 converts to v2).
    ///
    /// # Errors
    ///
    /// A newer version is rejected before the body is parsed (clear "upgrade the engine" error);
    /// otherwise see [`ScatterSet::validate`] and [`crate::TerrainError::Ron`].
    pub fn from_ron(text: &str, layers: Option<&TerrainLayers>) -> Result<Self, crate::TerrainError> {
        let version = regia_core::version::peek_ron_version(text).map_err(|e| {
            crate::TerrainError::InvalidScatter(ScatterError::Migration(format!("bad version: {e}")))
        })?;
        let set = match version {
            SCATTER_RULE_V1_VERSION => Self::from_v1(&ScatterRule::from_ron(text, layers)?),
            SCATTER_SET_VERSION => ron::from_str::<Self>(text)?,
            found => {
                return Err(crate::TerrainError::InvalidScatter(ScatterError::BadVersion {
                    found,
                    expected: SCATTER_SET_VERSION,
                }));
            }
        };
        set.validate(layers)?;
        Ok(set)
    }

    /// The cooked form `regia cook` writes into a pak: a little-endian `u32` cooked-format
    /// version ([`COOKED_SCATTER_VERSION`]) then the postcard-encoded v2 set.
    ///
    /// # Errors
    ///
    /// [`ScatterError::Migration`] when encoding fails (not expected for a valid set).
    pub fn to_cooked(&self) -> Result<Vec<u8>, ScatterError> {
        let mut out = COOKED_SCATTER_VERSION.to_le_bytes().to_vec();
        let body =
            postcard::to_allocvec(self).map_err(|e| ScatterError::Migration(format!("postcard: {e}")))?;
        out.extend(body);
        Ok(out)
    }

    /// Loads the cooked form, rejecting an unknown cooked version and re-validating the set.
    ///
    /// # Errors
    ///
    /// [`ScatterError::BadVersion`] for another cooked version, [`ScatterError::Migration`] for a
    /// truncated or corrupt payload, and the [`ScatterSet::validate`] errors.
    pub fn from_cooked(bytes: &[u8], layers: Option<&TerrainLayers>) -> Result<Self, ScatterError> {
        let (head, body) = bytes
            .split_first_chunk::<4>()
            .ok_or_else(|| ScatterError::Migration("cooked scatter is shorter than its header".into()))?;
        let version = u32::from_le_bytes(*head);
        if version != COOKED_SCATTER_VERSION {
            return Err(ScatterError::BadVersion {
                found: version,
                expected: COOKED_SCATTER_VERSION,
            });
        }
        let set: Self = postcard::from_bytes(body)
            .map_err(|e| ScatterError::Migration(format!("cooked scatter: {e}")))?;
        set.validate(layers)?;
        Ok(set)
    }

    /// Every mesh path the set references, deduplicated, in first-use order.
    #[must_use]
    pub fn mesh_paths(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for class in &self.classes {
            for mesh in &class.meshes {
                if !out.contains(&mesh.mesh.as_str()) {
                    out.push(mesh.mesh.as_str());
                }
            }
        }
        out
    }

    /// Every density-map path the set references, deduplicated.
    #[must_use]
    pub fn density_map_paths(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for path in self.classes.iter().filter_map(|c| c.density_map.as_deref()) {
            if !out.contains(&path) {
                out.push(path);
            }
        }
        out
    }
}

impl ScatterClass {
    /// Validates one class.
    ///
    /// # Errors
    ///
    /// [`ScatterError::Class`] naming the class and the bad field.
    pub fn validate(&self, layers: Option<&TerrainLayers>) -> Result<(), ScatterError> {
        let bad = |reason: String| ScatterError::Class {
            class: self.name.clone(),
            reason,
        };
        if self.name.is_empty() {
            return Err(bad("empty class name".into()));
        }
        if self.meshes.is_empty() {
            return Err(bad("no meshes".into()));
        }
        for mesh in &self.meshes {
            let ok = !mesh.mesh.is_empty()
                && mesh.weight.is_finite()
                && mesh.weight > 0.0
                && mesh.min_scale.is_finite()
                && mesh.min_scale > 0.0
                && mesh.max_scale.is_finite()
                && mesh.max_scale >= mesh.min_scale
                && mesh.scale_bias.is_finite()
                && mesh.scale_bias > 0.0;
            if !ok {
                return Err(bad(format!(
                    "mesh '{}' needs a path, weight > 0, 0 < min_scale <= max_scale, scale_bias > 0",
                    mesh.mesh
                )));
            }
        }
        for (field, value) in [
            ("density", self.density),
            ("noise_scale", self.noise_scale),
            ("noise_amount", self.noise_amount),
            ("slope_min", self.slope_min),
            ("slope_max", self.slope_max),
            ("layer_min_weight", self.layer_min_weight),
            ("yaw_jitter", self.yaw_jitter),
            ("align_to_normal", self.align_to_normal),
            ("tilt_max", self.tilt_max),
            ("tilt_jitter", self.tilt_jitter),
            ("sink", self.sink),
            ("tint_jitter", self.tint_jitter),
            ("base_blend", self.base_blend),
            ("contact_ao", self.contact_ao),
            ("contact_radius", self.contact_radius),
            ("lod_bias", self.lod_bias),
            ("fade_start", self.fade_start),
            ("fade_end", self.fade_end),
            ("shadow_distance", self.shadow_distance),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(bad(format!("{field} = {value} must be finite and >= 0")));
            }
        }
        if !(self.cell_size.is_finite() && self.cell_size >= 0.25) {
            return Err(bad(format!("cell_size = {} must be >= 0.25 m", self.cell_size)));
        }
        if self.density > 64.0 {
            return Err(bad(format!("density = {} exceeds 64 per cell", self.density)));
        }
        if self.slope_max < self.slope_min {
            return Err(bad("slope_max < slope_min".into()));
        }
        if !(self.height_min.is_finite() && self.height_max.is_finite()) || self.height_max < self.height_min
        {
            return Err(bad(
                "height band must be finite with height_min <= height_max".into()
            ));
        }
        for (field, value) in [
            ("noise_amount", self.noise_amount),
            ("align_to_normal", self.align_to_normal),
            ("sink", self.sink),
            ("base_blend", self.base_blend),
            ("contact_ao", self.contact_ao),
        ] {
            if value > 1.0 {
                return Err(bad(format!("{field} = {value} must be <= 1")));
            }
        }
        if self.tint_jitter > 0.5 {
            return Err(bad(format!("tint_jitter = {} must be <= 0.5", self.tint_jitter)));
        }
        if self.fade_end < self.fade_start {
            return Err(bad("fade_end < fade_start".into()));
        }
        if self.shadow_max_lod > 3 {
            return Err(bad(format!(
                "shadow_max_lod = {} must be 0..=3",
                self.shadow_max_lod
            )));
        }
        for colour in [self.tint, self.base_tint] {
            if colour.iter().any(|c| !c.is_finite() || *c < 0.0 || *c > 4.0) {
                return Err(bad(format!("color {colour:?} must be linear 0..=4")));
            }
        }
        for zone in &self.exclusions {
            if zone.points.is_empty()
                || !zone.radius.is_finite()
                || zone.radius < 0.0
                || zone.points.iter().flatten().any(|v| !v.is_finite())
            {
                return Err(bad(
                    "exclusion needs at least one finite point and radius >= 0".into()
                ));
            }
        }
        if let Some(path) = &self.density_map
            && path.is_empty()
        {
            return Err(bad("empty density_map path".into()));
        }
        if let Some(layers) = layers {
            let count = layers.count().min(MAX_LAYERS);
            let allowed = if count >= 8 { u8::MAX } else { (1u8 << count) - 1 };
            if self.layer_mask & !allowed != 0 {
                return Err(ScatterError::BadLayerIndex {
                    index: (7 - (self.layer_mask & !allowed).leading_zeros()) as u8,
                    count: layers.count(),
                });
            }
        }
        Ok(())
    }
}

/// The versions a migration step leaves (`from` side) for `cargo xtask check-formats`: v1 → v2
/// is the parse-time conversion in [`ScatterSet::from_ron`].
pub fn scatter_migration_sources() -> Vec<u32> {
    vec![SCATTER_RULE_V1_VERSION]
}

/// The serde field-list fingerprint of the `.scatter.ron` format: the root set, then every nested
/// struct (hashed by `cargo xtask check-formats`).
pub fn scatter_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<ScatterSet>(),
        serde_field_names::<ScatterClass>(),
        serde_field_names::<ScatterMeshRef>(),
        serde_field_names::<ScatterExclusion>(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class() -> ScatterClass {
        ScatterClass {
            meshes: vec![ScatterMeshRef {
                mesh: "props/rock_a.glb".into(),
                weight: 1.0,
                min_scale: 0.5,
                max_scale: 2.0,
                scale_bias: 2.0,
            }],
            ..ScatterClass::default()
        }
    }

    #[test]
    fn v2_round_trips() {
        let set = ScatterSet {
            classes: vec![class()],
            ..ScatterSet::default()
        };
        let text = set.to_ron().expect("save");
        assert_eq!(ScatterSet::from_ron(&text, None).expect("load"), set);
    }

    #[test]
    fn minimal_class_uses_defaults() {
        let text = r#"ScatterSet(version: 2, seed: 9, classes: [
            ScatterClass(name: "gravel", meshes: [ScatterMeshRef(mesh: "rock.glb")]),
        ])"#;
        let set = ScatterSet::from_ron(text, None).expect("minimal file");
        let gravel = &set.classes[0];
        assert_eq!(gravel.meshes[0].weight, 1.0);
        assert_eq!(gravel.cell_size, ScatterClass::default().cell_size);
    }

    #[test]
    fn v1_converts_to_one_class() {
        let rule = ScatterRule {
            layer_index: Some(2),
            align_to_normal: true,
            ..ScatterRule::default()
        };
        let text = rule.to_ron().expect("v1 text");
        let set = ScatterSet::from_ron(&text, None).expect("v1 loads as v2");
        assert_eq!(set.version, SCATTER_SET_VERSION);
        assert_eq!(set.seed, rule.seed);
        let class = &set.classes[0];
        assert_eq!(class.meshes[0].mesh, rule.mesh);
        assert_eq!(class.layer_mask, 0b100);
        assert_eq!(class.align_to_normal, 1.0);
        assert_eq!(class.density, rule.instances_per_cell);
    }

    #[test]
    fn cooked_round_trip_and_rejections() {
        let set = ScatterSet {
            classes: vec![ScatterClass {
                density_map: Some("maps/rocks.png".into()),
                exclusions: vec![ScatterExclusion {
                    points: vec![[1.0, 2.0], [3.0, 4.0]],
                    radius: 5.0,
                }],
                ..class()
            }],
            ..ScatterSet::default()
        };
        let bytes = set.to_cooked().expect("cook");
        assert_eq!(ScatterSet::from_cooked(&bytes, None).expect("load"), set);
        let mut newer = bytes.clone();
        newer[0] = 9;
        assert!(matches!(
            ScatterSet::from_cooked(&newer, None),
            Err(ScatterError::BadVersion { found: 9, .. })
        ));
        assert!(ScatterSet::from_cooked(&bytes[..2], None).is_err());
        assert!(ScatterSet::from_cooked(&bytes[..bytes.len() / 2], None).is_err());
    }

    #[test]
    fn newer_version_is_rejected_before_parsing() {
        let err = ScatterSet::from_ron("(version: 3)", None).expect_err("v3");
        assert!(err.to_string().contains("version 3"), "{err}");
    }

    #[test]
    fn bad_classes_are_rejected_with_their_name() {
        for broken in [
            ScatterClass {
                meshes: Vec::new(),
                ..class()
            },
            ScatterClass {
                density: f32::NAN,
                ..class()
            },
            ScatterClass {
                fade_end: 1.0,
                fade_start: 2.0,
                ..class()
            },
            ScatterClass { sink: 1.5, ..class() },
        ] {
            let err = broken.validate(None).expect_err("invalid");
            assert!(err.to_string().contains("rocks"), "{err}");
        }
        let dup = ScatterSet {
            classes: vec![class(), class()],
            ..ScatterSet::default()
        };
        assert!(dup.validate(None).is_err());
    }

    #[test]
    fn layer_mask_is_checked_against_the_stack() {
        let layers = TerrainLayers::default();
        let masked = ScatterClass {
            layer_mask: 0b10,
            ..class()
        };
        assert!(matches!(
            masked.validate(Some(&layers)),
            Err(ScatterError::BadLayerIndex { index: 1, count: 1 })
        ));
    }

    #[test]
    fn exclusion_circle_and_path() {
        let circle = ScatterExclusion {
            points: vec![[0.0, 0.0]],
            radius: 2.0,
        };
        assert!(circle.contains(1.0, 1.0));
        assert!(!circle.contains(2.0, 2.0));
        let road = ScatterExclusion {
            points: vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0]],
            radius: 1.0,
        };
        assert!(road.contains(5.0, 0.5));
        assert!(road.contains(10.5, 5.0));
        assert!(!road.contains(5.0, 5.0));
    }
}
