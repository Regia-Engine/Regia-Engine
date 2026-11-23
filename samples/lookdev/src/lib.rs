// SPDX-License-Identifier: MIT-0
//! Photoreal look-dev harness for the DO27 alpha maps.
//!
//! Two procedural stand-ins — Europa (*Bore Twelve*) and Mars (*The Switchbacks*) —
//! match the terrain data shape the game streams: a 16-bit eroded heightfield, six
//! material layers with height maps, a macro-variation map, a road cut into the
//! surface, a tunnel portal, and seeded rock scatter. `lookdev --shots` renders a
//! fixed camera list offscreen and writes `shots.json`. `lookdev --compare` prints
//! the deltas the lead reads after each merge.
//!
//! The sample links only the crates on this branch. In-flight lanes (volumetric
//! atmosphere and clouds, terrain material height-blend / macro sampling, heightfield
//! streaming) are detected at build time by [`features`]; the render path turns on
//! every knob the public terrain and profile API already exposes, so a merge that
//! extends those knobs shows up in the next shot list without a harness edit.
//!
//! Specification context: `docs/specs/26-terrain-vfx-highend.md` (terrain rows, Desktop
//! High/Ultra) and `docs/specs/13-renderer.md` § 3 (profiles, including Deck).

#![warn(missing_docs)]

pub mod black_sky;
pub mod chase;
pub mod cooked_runtime;
pub mod default_look;
pub mod dense_scatter;
pub mod do27;
pub mod do27_bench;
pub mod dyn_light;
pub mod erosion;
pub mod feature_cost;
pub mod features;
pub mod fx;
pub mod hero;
pub mod hero_ref;
pub mod interiors;
pub mod jitter_units;
pub mod look_ci;
pub mod look_r2;
pub mod match_scene;
pub mod measure;
pub mod mem_budget;
pub mod mobile_aa;
pub mod motion;
pub mod moving_units;
pub mod perf_heat;
mod portal;
pub mod pt_plates;
pub mod render;
pub mod report;
pub mod scatter_shots;
pub mod scene;
pub mod session_summary;
pub mod shots;
pub mod soak;
pub mod soak_bundle;
pub mod soak_plot;
pub mod spans;
pub mod tsr_motion;
pub mod ultrawide;
pub mod vis_cooked;
pub mod vis_stress;
#[cfg(not(target_os = "android"))]
pub mod window_host;
mod workspace_bin;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Failures while building a stand-in, rendering a shot, or reading a report.
#[derive(Debug, thiserror::Error)]
pub enum LookdevError {
    /// A configuration value cannot produce a scene or a shot.
    #[error("invalid lookdev config: {0}")]
    Config(String),
    /// Heightfield, layers, scatter, or the terrain GPU rejected the stand-in.
    #[error(transparent)]
    Terrain(#[from] regia_terrain::TerrainError),
    /// The tunnel portal mesh could not be built.
    #[error(transparent)]
    Tunnel(#[from] regia_terrain::mesh_tunnel::MeshTunnelError),
    /// The headless device or a frame submission failed.
    #[error(transparent)]
    Render(#[from] regia_render::RenderError),
    /// A shot file could not be read or written.
    #[error("io error at {path}: {source}")]
    Io {
        /// Path that failed.
        path: String,
        /// The filesystem error.
        source: std::io::Error,
    },
    /// PNG encoding failed.
    #[error("png encode: {0}")]
    Png(String),
    /// `shots.json` was not the report this harness writes.
    #[error("shots.json: {0}")]
    Json(String),
}

/// Which stand-in planet to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Planet {
    /// Icy chaos terrain: snow, wind-packed snow, blue ice, dirty ice, frosted basalt, regolith.
    Europa,
    /// Stratified canyon country: sandstone, scree, dust, gravel, bedrock, dune sand.
    Mars,
}

impl Planet {
    /// Stable directory and file name (`europa`, `mars`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Europa => "europa",
            Self::Mars => "mars",
        }
    }

    /// DO27 map this stand-in stands in for.
    #[must_use]
    pub fn map_name(self) -> &'static str {
        match self {
            Self::Europa => "Bore Twelve",
            Self::Mars => "The Switchbacks",
        }
    }

    /// Noise and scatter seed. Fixed so two machines build the same island.
    #[must_use]
    pub fn seed(self) -> u32 {
        match self {
            Self::Europa => 0xB012_E1CE,
            Self::Mars => 0x4D25_5A1D,
        }
    }

    /// Both alpha planets, Europa first.
    #[must_use]
    pub fn both() -> &'static [Planet] {
        &[Self::Europa, Self::Mars]
    }
}

/// Resolution and shot-list knobs. Tests use [`LookdevConfig::tiny`]; the evidence
/// run uses [`LookdevConfig::baseline`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LookdevConfig {
    /// Heightfield edge, samples. The baseline is 4096².
    pub height_resolution: u32,
    /// Macro-variation map edge. Configurable; the 8K target is [`Self::MACRO_8K`].
    pub macro_resolution: u32,
    /// Per-layer height-map edge.
    pub layer_height_resolution: u32,
    /// World extent along X and Z, metres.
    pub world_width_m: f32,
    /// Frames rendered per shot. Temporal stability uses every consecutive pair.
    pub frames: u32,
    /// Output width for Desktop High and Ultra, pixels.
    pub width: u32,
    /// Output height for Desktop High and Ultra, pixels.
    pub height: u32,
    /// PNG width after the box downsample. Wider frames are reduced to this.
    pub png_width: u32,
    /// Thermal-erosion iterations (Musgrave et al. 1989).
    pub thermal_iterations: u32,
    /// Hydraulic-erosion droplets (Mei, Decaudin, Hu 2007, simplified).
    pub hydraulic_drops: u32,
    /// Maximum rock instances uploaded per profile (the closest to the shot set).
    pub scatter_cap: usize,
    /// Planets to build, in order.
    pub planets: Vec<Planet>,
}

impl LookdevConfig {
    /// Nominal macro-variation edge ("8K-ish"). Pass it as `--macro`.
    pub const MACRO_8K: u32 = 8192;
    /// Baseline heightfield edge.
    pub const HEIGHT_4K: u32 = 4096;

    /// Evidence defaults: 4096² heightfield, 2048² macro, 1920×1080, 30 frames.
    ///
    /// `--macro 8192` ([`Self::MACRO_8K`]) stays valid. The evidence run stays at
    /// 2048 because the terrain shader does not sample the map yet. An 8192² `f32`
    /// image is 256 MiB before row padding, and generating plus uploading it for
    /// two planets would crowd the shared 10 minute GPU window without changing
    /// the pixels. A CPU test checks that 8192 allocates.
    #[must_use]
    pub fn baseline() -> Self {
        Self {
            height_resolution: Self::HEIGHT_4K,
            macro_resolution: 2048,
            layer_height_resolution: 512,
            world_width_m: 4096.0,
            frames: 30,
            width: 1920,
            height: 1080,
            png_width: 960,
            thermal_iterations: 5,
            hydraulic_drops: 4096,
            scatter_cap: 700,
            planets: Planet::both().to_vec(),
        }
    }

    /// Lavapipe shot-list size: coarse grids, full camera list, 30 frames.
    #[must_use]
    pub fn tiny() -> Self {
        Self {
            height_resolution: 48,
            macro_resolution: 16,
            layer_height_resolution: 8,
            world_width_m: 4096.0,
            frames: 30,
            width: 64,
            height: 36,
            png_width: 64,
            thermal_iterations: 2,
            hydraulic_drops: 24,
            scatter_cap: 24,
            planets: Planet::both().to_vec(),
        }
    }

    /// Rejects zero sizes and an empty planet list.
    ///
    /// # Errors
    ///
    /// [`LookdevError::Config`] when a dimension, frame count, or world width is unusable.
    pub fn validate(&self) -> Result<(), LookdevError> {
        let bad = |what: &str| LookdevError::Config(what.to_string());
        if self.height_resolution < 2 || self.macro_resolution < 2 || self.layer_height_resolution < 2 {
            return Err(bad("resolutions must be at least 2"));
        }
        if self.width < 2 || self.height < 2 || self.png_width < 1 {
            return Err(bad("output size must be at least 2×2"));
        }
        if self.frames < 2 {
            return Err(bad("temporal stability needs at least 2 frames"));
        }
        if !self.world_width_m.is_finite() || self.world_width_m < 16.0 {
            return Err(bad("world width must be finite and at least 16 m"));
        }
        if self.planets.is_empty() {
            return Err(bad("no planets selected"));
        }
        Ok(())
    }
}

/// Filesystem error with the path attached.
pub(crate) fn io_err(path: impl AsRef<std::path::Path>, source: std::io::Error) -> LookdevError {
    LookdevError::Io {
        path: path.as_ref().display().to_string(),
        source,
    }
}

/// Creates `dir` and returns it.
pub(crate) fn ensure_dir(dir: &std::path::Path) -> Result<PathBuf, LookdevError> {
    std::fs::create_dir_all(dir).map_err(|source| io_err(dir, source))?;
    Ok(dir.to_path_buf())
}
