// SPDX-License-Identifier: MPL-2.0
//! Grass fields (ADR-1400): dense ground cover drawn as GPU-generated clumps of geometric blades,
//! placed procedurally around the camera from a frozen RON format (`.grass.ron` v1,
//! `docs/formats/grass-ron.md`) and splat-driven masks.
//!
//! - [`GrassSet`] / [`GrassClass`]: the format: per class, the density and its rules (the
//!   terrain layers it grows on, slope and altitude bands, a density map, noise, exclusion
//!   paths: the same rule vocabulary as [`crate::ScatterClass`]), the blade shape, its colours
//!   (dead stems, green shoots), range, shadows and sway.
//! - [`mask`]: [`GrassMask`], the per-class density / greenness / height / tint texture the
//!   GPU reads, baked on the CPU from the map, the layer stack and painted splat weights.
//! - [`select`]: the patch quadtree around the camera and the density-by-distance law (CPU
//!   mirror of the GPU placement, unit-tested).
//! - [`quality`]: [`GrassQuality`] tiers and their [`GrassBudget`], the cheap fallback for
//!   Low, Deck and Mobile included.
//! - [`control`]: [`TerrainGrassHandle`], the agent-protocol handle (`terrain.grass`,
//!   `terrain.set_grass`).
//!
//! The GPU side is `crate::gpu::grass` ([`crate::GrassLayer`]): bound to a
//! [`crate::TerrainSharedDraw`] with [`crate::TerrainSharedDraw::set_grass`], it generates the
//! clumps in a compute pass at the start of the frame and draws them in the shared depth,
//! shadow, colour and velocity passes right after the terrain. Nothing runs when no layer is
//! bound.
//!
//! Determinism: every clump is a pure function of (class seed, fine patch coordinates,
//! candidate index), the mask and the heightfield; the GPU writes its instance lists with a
//! fixed-order prefix sum (no global atomics), so two renders of the same frame are
//! bit-identical on one device.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ScatterExclusion;

pub mod control;
pub mod mask;
pub mod quality;
pub mod select;
pub mod wake;

pub use control::{GrassGust, GrassReport, GrassRequest, TerrainGrassHandle};
pub use mask::{GrassMask, GrassMaskInputs, GrassMultipliers, MultiplierImage};
pub use quality::{GrassBudget, GrassQuality};
pub use select::{
    GrassPatch, GrassView, MAX_GRASS_PATCHES, MAX_PATCH_LEVEL, PATCH_CAMERA, PATCH_M, PATCH_NO_CAMERA,
    PATCH_SHADOW, candidate, candidates_per_fine_patch, density_fraction, select_patches,
};
pub use wake::{GRASS_WAKE_ROWS, GrassWake};

/// Current version of the `.grass.ron` format.
pub const GRASS_FORMAT_VERSION: u32 = 1;

/// Most grass classes one set may hold (the GPU draws them as one batch per LOD).
pub const MAX_GRASS_CLASSES: usize = 8;

/// Most blades in one clump.
pub const MAX_BLADES: u32 = 16;

/// Errors for grass sets and masks.
#[derive(Clone, Debug, Error)]
pub enum GrassError {
    /// Unknown format version.
    #[error("unsupported grass set version {found}, expected {expected}")]
    BadVersion {
        /// Version found in the file.
        found: u32,
        /// Version this build reads/writes.
        expected: u32,
    },
    /// A class field is out of range.
    #[error("grass class '{class}': field '{field}' has invalid value {value}")]
    BadParameter {
        /// Class name.
        class: String,
        /// Field name.
        field: &'static str,
        /// Offending value.
        value: f32,
    },
    /// A structural problem (empty or duplicate name, too many classes, bad layer index).
    #[error("grass set: {0}")]
    Invalid(String),
    /// A density map the set names was not supplied, or its size is wrong.
    #[error("grass density map '{path}': {reason}")]
    Asset {
        /// Asset path from the `.grass.ron`.
        path: String,
        /// What is wrong.
        reason: String,
    },
    /// RON parse or write failure.
    #[error("grass set RON: {0}")]
    Ron(String),
}

/// A whole map's grass (`.grass.ron`, format v1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrassSet {
    /// Format version; loaders reject unknown versions.
    pub version: u32,
    /// Deterministic seed: the same seed, map and file grow the same blades on every client.
    #[serde(default)]
    pub seed: u64,
    /// Grass classes, each placed independently (its stream is keyed by the class name, so
    /// reordering classes moves nothing).
    pub classes: Vec<GrassClass>,
}

impl Default for GrassSet {
    fn default() -> Self {
        Self {
            version: GRASS_FORMAT_VERSION,
            seed: 0,
            classes: Vec::new(),
        }
    }
}

/// How one terrain layer drives a class: where the layer's splat weight is `w`, the class grows
/// at `w · density` of its full density, with `w · green` of its blades green shoots and
/// `height` times its blade height (weighted over the listed layers).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrassLayerRule {
    /// Terrain layer index (`.terrain.layers.ron` order, 0 = base).
    pub layer: u8,
    /// Density multiplier on this layer, `0..=1`.
    #[serde(default = "one")]
    pub density: f32,
    /// Green-shoot fraction on this layer, `0..=1` (added to the class's `green_fraction`).
    #[serde(default)]
    pub green: f32,
    /// Blade height multiplier on this layer, `0..=2`.
    #[serde(default = "one")]
    pub height: f32,
}

/// One kind of grass: placement rules, blade shape, colours, range.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GrassClass {
    /// Class name (unique in the set; keys its random stream).
    pub name: String,
    /// Clumps per square metre where every rule passes fully, `0..=64`.
    pub density: f32,
    /// Optional grayscale density map over the whole terrain (asset path; the host decodes it
    /// into a [`crate::DensityMap`] and passes it in [`GrassMaskInputs`]).
    pub density_map: Option<String>,
    /// The terrain layers this class grows on. Empty: every layer at full density, no shoots
    /// beyond `green_fraction`.
    pub layers: Vec<GrassLayerRule>,
    /// Patchiness noise wavelength, metres.
    pub noise_scale: f32,
    /// Patchiness strength, `0..=1` (0 = uniform).
    pub noise_amount: f32,
    /// Lowest ground slope it grows on, radians.
    pub slope_min: f32,
    /// Steepest ground slope it grows on, radians.
    pub slope_max: f32,
    /// Lowest altitude, metres.
    pub height_min: f32,
    /// Highest altitude, metres.
    pub height_max: f32,
    /// Grow nothing inside terrain holes.
    pub avoid_holes: bool,
    /// Nothing grows within these zones (roads, paths, building pads).
    pub exclusions: Vec<ScatterExclusion>,
    /// Blades per clump, `1..=16` (near LOD; far LODs draw fewer, wider blades).
    pub blades: u32,
    /// Blade height range `[min, max]`, metres.
    pub blade_height: [f32; 2],
    /// Blade width at the root, metres.
    pub blade_width: f32,
    /// Radius the blades of a clump root within, metres.
    pub clump_radius: f32,
    /// Outward lean of the blades from the clump centre, radians (`0..=1.4`).
    pub lean: f32,
    /// How far blade tips droop, `0..=1` (dead winter grass is matted: 0.5–0.8).
    pub bend: f32,
    /// Linear albedo of a stem at its root.
    pub base_color: [f32; 3],
    /// Linear albedo of a stem at its tip.
    pub tip_color: [f32; 3],
    /// Linear albedo of a green shoot at its root.
    pub green_color: [f32; 3],
    /// Linear albedo of a green shoot at its tip.
    pub green_tip_color: [f32; 3],
    /// Fraction of blades that are green shoots everywhere, `0..=1` (the layer rules add to it).
    pub green_fraction: f32,
    /// Height of a green shoot relative to a stem, `0.1..=2`.
    pub green_height: f32,
    /// Per-clump brightness and hue jitter, `0..=0.5`.
    pub color_jitter: f32,
    /// Perceptual roughness of the blades.
    pub roughness: f32,
    /// Light through a back-lit blade, `0..=1`.
    pub translucency: f32,
    /// Occlusion at the root, `0..=1` (0 = black roots, 1 = none).
    pub base_occlusion: f32,
    /// Farthest distance the class draws, metres (the quality tier caps it).
    pub range: f32,
    /// Casts sun shadows (inside the tier's shadow distance).
    pub cast_shadows: bool,
    /// Sway amplitude at the tip, metres (0 = still air).
    pub sway: f32,
    /// Sway frequency, hertz.
    pub sway_speed: f32,
}

fn one() -> f32 {
    1.0
}

impl Default for GrassClass {
    fn default() -> Self {
        Self::winter_dead()
    }
}

impl GrassClass {
    /// Dead winter grass with a few early green shoots: matted straw-coloured stems 25–55 cm,
    /// about one blade in eight a short green shoot, still air.
    #[must_use]
    pub fn winter_dead() -> Self {
        Self {
            name: "dead_grass".into(),
            density: 10.0,
            density_map: None,
            layers: Vec::new(),
            noise_scale: 9.0,
            noise_amount: 0.45,
            slope_min: 0.0,
            slope_max: 0.75,
            height_min: -10_000.0,
            height_max: 10_000.0,
            avoid_holes: true,
            exclusions: Vec::new(),
            blades: 12,
            blade_height: [0.25, 0.55],
            blade_width: 0.011,
            clump_radius: 0.17,
            lean: 0.45,
            bend: 0.6,
            base_color: [0.24, 0.18, 0.10],
            tip_color: [0.58, 0.47, 0.30],
            green_color: [0.05, 0.10, 0.025],
            green_tip_color: [0.15, 0.23, 0.06],
            green_fraction: 0.12,
            green_height: 0.55,
            color_jitter: 0.18,
            roughness: 0.62,
            translucency: 0.4,
            base_occlusion: 0.5,
            range: 300.0,
            cast_shadows: true,
            sway: 0.0,
            sway_speed: 0.4,
        }
    }

    /// Summer meadow grass: upright green blades 35–75 cm with dry tips, a light sway.
    #[must_use]
    pub fn summer() -> Self {
        Self {
            name: "summer_grass".into(),
            density: 12.0,
            blades: 14,
            blade_height: [0.35, 0.75],
            blade_width: 0.008,
            clump_radius: 0.12,
            lean: 0.3,
            bend: 0.3,
            base_color: [0.035, 0.075, 0.018],
            tip_color: [0.17, 0.23, 0.07],
            green_color: [0.05, 0.12, 0.025],
            green_tip_color: [0.12, 0.26, 0.05],
            green_fraction: 0.5,
            green_height: 0.8,
            color_jitter: 0.15,
            roughness: 0.5,
            translucency: 0.5,
            base_occlusion: 0.3,
            sway: 0.03,
            sway_speed: 0.5,
            ..Self::winter_dead()
        }
    }

    /// Validates every range.
    ///
    /// # Errors
    ///
    /// [`GrassError::BadParameter`] or [`GrassError::Invalid`] naming the field.
    pub fn validate(&self, layer_count: Option<usize>) -> Result<(), GrassError> {
        let bad = |field: &'static str, value: f32| GrassError::BadParameter {
            class: self.name.clone(),
            field,
            value,
        };
        let check = |field: &'static str, value: f32, lo: f32, hi: f32| {
            if value.is_finite() && (lo..=hi).contains(&value) {
                Ok(())
            } else {
                Err(bad(field, value))
            }
        };
        if self.name.trim().is_empty() {
            return Err(GrassError::Invalid("a class has an empty name".into()));
        }
        check("density", self.density, 0.0, 64.0)?;
        check("noise_scale", self.noise_scale, 0.1, 10_000.0)?;
        check("noise_amount", self.noise_amount, 0.0, 1.0)?;
        check("slope_min", self.slope_min, 0.0, 1.6)?;
        check("slope_max", self.slope_max, 0.0, 1.6)?;
        if self.slope_max < self.slope_min {
            return Err(bad("slope_max", self.slope_max));
        }
        if !self.height_min.is_finite() || !self.height_max.is_finite() || self.height_max < self.height_min {
            return Err(bad("height_max", self.height_max));
        }
        if !(1..=MAX_BLADES).contains(&self.blades) {
            return Err(bad("blades", self.blades as f32));
        }
        check("blade_height[0]", self.blade_height[0], 0.01, 4.0)?;
        check("blade_height[1]", self.blade_height[1], 0.01, 4.0)?;
        if self.blade_height[1] < self.blade_height[0] {
            return Err(bad("blade_height[1]", self.blade_height[1]));
        }
        check("blade_width", self.blade_width, 0.0005, 0.2)?;
        check("clump_radius", self.clump_radius, 0.0, 2.0)?;
        check("lean", self.lean, 0.0, 1.4)?;
        check("bend", self.bend, 0.0, 1.0)?;
        for (field, c) in [
            ("base_color", self.base_color),
            ("tip_color", self.tip_color),
            ("green_color", self.green_color),
            ("green_tip_color", self.green_tip_color),
        ] {
            for v in c {
                check(field, v, 0.0, 1.0)?;
            }
        }
        check("green_fraction", self.green_fraction, 0.0, 1.0)?;
        check("green_height", self.green_height, 0.1, 2.0)?;
        check("color_jitter", self.color_jitter, 0.0, 0.5)?;
        check("roughness", self.roughness, 0.04, 1.0)?;
        check("translucency", self.translucency, 0.0, 1.0)?;
        check("base_occlusion", self.base_occlusion, 0.0, 1.0)?;
        check("range", self.range, 1.0, 5_000.0)?;
        check("sway", self.sway, 0.0, 0.5)?;
        check("sway_speed", self.sway_speed, 0.0, 10.0)?;
        for rule in &self.layers {
            if let Some(count) = layer_count
                && usize::from(rule.layer) >= count
            {
                return Err(GrassError::Invalid(format!(
                    "class '{}': layer {} exceeds the {count}-layer stack",
                    self.name, rule.layer
                )));
            }
            if usize::from(rule.layer) >= crate::MAX_LAYERS {
                return Err(GrassError::Invalid(format!(
                    "class '{}': layer {} exceeds {} layers",
                    self.name,
                    rule.layer,
                    crate::MAX_LAYERS
                )));
            }
            check("layers.density", rule.density, 0.0, 1.0)?;
            check("layers.green", rule.green, 0.0, 1.0)?;
            check("layers.height", rule.height, 0.0, 2.0)?;
        }
        for zone in &self.exclusions {
            if !zone.radius.is_finite() || zone.radius < 0.0 {
                return Err(bad("exclusions.radius", zone.radius));
            }
        }
        Ok(())
    }

    /// The class's own random stream seed: the set seed mixed with the FNV-1a hash of the name.
    #[must_use]
    pub fn stream_seed(&self, set_seed: u64) -> u32 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.name.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let mut z = set_seed ^ h;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        (z ^ (z >> 32)) as u32
    }
}

impl GrassSet {
    /// One class of dead winter grass with early shoots ([`GrassClass::winter_dead`]).
    #[must_use]
    pub fn winter() -> Self {
        Self {
            classes: vec![GrassClass::winter_dead()],
            ..Self::default()
        }
    }

    /// One class of summer grass ([`GrassClass::summer`]).
    #[must_use]
    pub fn summer() -> Self {
        Self {
            classes: vec![GrassClass::summer()],
            ..Self::default()
        }
    }

    /// Validates the set (version, class count, unique names, every class).
    ///
    /// # Errors
    ///
    /// See [`GrassError`].
    pub fn validate(&self, layer_count: Option<usize>) -> Result<(), GrassError> {
        if self.version != GRASS_FORMAT_VERSION {
            return Err(GrassError::BadVersion {
                found: self.version,
                expected: GRASS_FORMAT_VERSION,
            });
        }
        if self.classes.len() > MAX_GRASS_CLASSES {
            return Err(GrassError::Invalid(format!(
                "{} classes, at most {MAX_GRASS_CLASSES}",
                self.classes.len()
            )));
        }
        for (i, class) in self.classes.iter().enumerate() {
            class.validate(layer_count)?;
            if self.classes[..i].iter().any(|c| c.name == class.name) {
                return Err(GrassError::Invalid(format!(
                    "duplicate class name '{}'",
                    class.name
                )));
            }
        }
        Ok(())
    }

    /// Parses and validates a `.grass.ron` document. `layers` (when known) checks the layer
    /// indices against the terrain's stack.
    ///
    /// # Errors
    ///
    /// [`GrassError::Ron`] for malformed RON, else what [`Self::validate`] reports.
    pub fn from_ron(text: &str, layers: Option<&crate::TerrainLayers>) -> Result<Self, GrassError> {
        let set: Self = ron::from_str(text).map_err(|e| GrassError::Ron(e.to_string()))?;
        set.validate(layers.map(crate::TerrainLayers::count))?;
        Ok(set)
    }

    /// Writes the set as pretty RON.
    ///
    /// # Errors
    ///
    /// [`GrassError::Ron`] when serialisation fails.
    pub fn to_ron(&self) -> Result<String, GrassError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty().struct_names(true).new_line("\n"))
            .map_err(|e| GrassError::Ron(e.to_string()))
    }
}

/// The `from` versions of the format's migration steps (none: v1 is the first version).
#[must_use]
pub fn grass_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde field-list fingerprint of the `.grass.ron` format (hashed by
/// `cargo xtask check-formats`).
#[must_use]
pub fn grass_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<GrassSet>(),
        serde_field_names::<GrassClass>(),
        serde_field_names::<GrassLayerRule>(),
        serde_field_names::<ScatterExclusion>(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_validate_and_round_trip() {
        for set in [GrassSet::winter(), GrassSet::summer()] {
            set.validate(None).expect("preset validates");
            let text = set.to_ron().expect("ron");
            let back = GrassSet::from_ron(&text, None).expect("parse");
            assert_eq!(back, set);
        }
    }

    #[test]
    fn minimal_file_takes_the_winter_defaults() {
        let set = GrassSet::from_ron(
            r#"GrassSet(version: 1, classes: [GrassClass(name: "a", density: 3.0)])"#,
            None,
        )
        .expect("parse");
        assert_eq!(set.seed, 0);
        assert_eq!(set.classes[0].density, 3.0);
        assert_eq!(set.classes[0].blades, GrassClass::winter_dead().blades);
    }

    #[test]
    fn bad_values_are_named() {
        let mut set = GrassSet::winter();
        set.classes[0].density = -1.0;
        let err = set.validate(None).expect_err("negative density");
        assert!(err.to_string().contains("density"), "{err}");
        let mut set = GrassSet::winter();
        set.classes[0].blades = 0;
        assert!(set.validate(None).is_err());
        let mut set = GrassSet::winter();
        set.classes[0].blade_height = [0.5, 0.2];
        assert!(set.validate(None).is_err());
        let mut set = GrassSet::winter();
        set.version = 2;
        assert!(matches!(
            set.validate(None),
            Err(GrassError::BadVersion { found: 2, .. })
        ));
    }

    #[test]
    fn duplicate_names_and_bad_layers_are_rejected() {
        let mut set = GrassSet::winter();
        set.classes.push(GrassClass::winter_dead());
        assert!(set.validate(None).is_err());
        let mut set = GrassSet::winter();
        set.classes[0].layers.push(GrassLayerRule {
            layer: 3,
            density: 1.0,
            green: 0.0,
            height: 1.0,
        });
        assert!(set.validate(Some(2)).is_err());
        assert!(set.validate(Some(4)).is_ok());
    }

    #[test]
    fn stream_seed_depends_on_name_and_seed_only() {
        let a = GrassClass::winter_dead();
        let mut b = GrassClass::summer();
        b.name = a.name.clone();
        assert_eq!(a.stream_seed(7), b.stream_seed(7));
        assert_ne!(a.stream_seed(7), a.stream_seed(8));
        assert_ne!(a.stream_seed(7), GrassClass::summer().stream_seed(7));
    }

    #[test]
    fn fingerprint_lists_the_class_fields() {
        let f = grass_schema_fingerprint();
        for field in ["density", "blade_height", "green_fraction", "sway"] {
            assert!(f.contains(field), "{field} missing from {f}");
        }
    }
}
