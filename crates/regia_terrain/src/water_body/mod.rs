// SPDX-License-Identifier: MPL-2.0
//! Water bodies in the runtime frame (ADR-1399): lakes and oceans as planes, streams and rivers as
//! sloped ribbons along a centerline, drawn by the frame renderer through the translucent-layer seam.
//!
//! This is the runtime half that ADR-1237 left open. The offscreen terrain renderer still shades
//! the shoreline plane of [`crate::WaterPlane`]. With [`crate::WaterPlugin`] and
//! [`WaterSettings::terrain_planes`] left on (the default), that plane is also a [`WaterBody`]
//! tagged [`TerrainWater`], drawn by any host that runs `regia_render`'s frame renderer.
//! `terrain_planes: false` removes those bodies. Any other [`WaterBody`] is drawn by that same
//! frame renderer over any bed: terrain, meshes, scattered pebbles. A body that is not derived
//! from a terrain plane does not need a [`crate::Terrain`].
//!
//! # Pieces
//!
//! - [`WaterBody`] (a component): the shape ([`WaterShape::Plane`] or [`WaterShape::Ribbon`]), the
//!   look ([`WaterLook`]), obstacles that raise standing waves ([`WaterObstacle`]) and an optional
//!   thin ice shelf ([`IceShelf`]).
//! - [`mesh`]: the CPU mesh the GPU draws (a ribbon of seven columns per centerline point, or a plane
//!   grid), with the flow velocity per vertex. A ribbon's speed is authored or comes from Manning's
//!   equation ([`manning_speed`]).
//! - [`clock`]: the deterministic water clock. Every animated term is a pure function of the
//!   fixed-step tick ([`WaterClock`]), so two renders of the same tick are identical.
//! - [`normal_map`]: the tileable ripple normal map, made on the CPU from a fixed seed.
//! - [`gpu`]: [`WaterRenderer`], the GPU side, and the [`regia_render::TranslucentLayer`] that
//!   draws it.
//! - [`plugin`]: [`WaterPlugin`], the ECS bridge (clock, body upload, layer install, tier) and the
//!   runtime body of each terrain's shoreline plane ([`TerrainWater`], ADR-1620).
//! - [`swell`]: trochoidal waves on a plane ([`Swell`], ADR-1620), the exact CPU surface the shader
//!   mirrors, and the camera-centered ring grid a swell plane is drawn on.
//! - [`shore`]: the shore bake ([`ShoreField`], spec 87, ADR-1828), a signed shoreline distance
//!   and depth over a plane's bed, and the shoaling, breaking and shore foam it drives.
//! - [`float`]: floating bodies ([`Floater`]): buoyancy and drag from the water surface; the
//!   physics bridge is `regia_framework::buoyancy::BuoyancyPlugin`.
//! - [`query`]: the world surface query over every body kind ([`WaterQuery`], [`surface_at`]).
//! - [`ocean`]: the FFT ocean of a plane ([`FftOcean`], ADR-1969): a wind sea from a directional
//!   spectrum, the compute pass's CPU mirror, and the blend into the swell near a shore.
//!
//! # Surface and ray queries
//!
//! [`WaterBody::surface`] answers the height, normal, velocity and crest foam of the visible
//! surface at a world `(x, z)` (on a swell plane through a fixed-count horizontal inverse);
//! [`WaterBody::raycast`] finds the first crossing of a ray by a bracketed march and bisection with
//! fixed step counts. Both are pure functions of the body and the [`WaterClock`].
//!
//! # Tiers
//!
//! [`WaterTier::Low`] (Steam Deck, mobile, Desktop Low) blends the surface over the frame with
//! fixed-function alpha: Beer–Lambert opacity from the depth buffer, sky and sun reflection, wet
//! banks, one ripple scale, no screen-space refraction or reflection, no standing waves. It needs no
//! scene copy. [`WaterTier::High`] and [`WaterTier::Ultra`] composite over a copy of the frame:
//! refraction, screen-space reflection with a sky fallback, standing waves, a screen-space sun
//! shadow test and two ripple scales.

#[cfg(test)]
use std::cell::Cell;
use std::cell::RefCell;

#[cfg(test)]
thread_local! {
    static SPEED_BUILDS: Cell<u32> = const { Cell::new(0) };
    static RIBBON_SCANS: Cell<u32> = const { Cell::new(0) };
}

/// Cached Manning speeds for one ribbon, keyed by the point slice and a fingerprint of its fields.
struct SpeedSlot {
    ptr: usize,
    fp: u64,
    speeds: Vec<f32>,
}

/// A few ribbons per thread. The hot path is one body sampled many times.
const SPEED_CACHE_SLOTS: usize = 4;

thread_local! {
    static SPEED_CACHE: RefCell<Vec<SpeedSlot>> = const { RefCell::new(Vec::new()) };
}

fn ribbon_fingerprint(points: &[RibbonPoint], manning: f32) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    let mut mix = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    };
    mix(&manning.to_le_bytes());
    mix(&(points.len() as u64).to_le_bytes());
    for point in points {
        mix(&point.position[0].to_le_bytes());
        mix(&point.position[1].to_le_bytes());
        mix(&point.position[2].to_le_bytes());
        mix(&point.width.to_le_bytes());
        mix(&point.depth.to_le_bytes());
        match point.speed {
            Some(speed) => {
                mix(&[1]);
                mix(&speed.to_le_bytes());
            }
            None => mix(&[0]),
        }
    }
    hash
}

fn build_ribbon_speeds(points: &[RibbonPoint], manning: f32) -> Vec<f32> {
    note_speed_build();
    (0..points.len())
        .map(|i| {
            let p = points[i];
            p.speed.unwrap_or_else(|| {
                let a = points[i.saturating_sub(1)].position;
                let b = points[(i + 1).min(points.len() - 1)].position;
                let run = ((b[0] - a[0]).powi(2) + (b[2] - a[2]).powi(2)).sqrt();
                let slope = if run > 0.0 { (a[1] - b[1]) / run } else { 0.0 };
                manning_speed(p.width, p.depth, slope, manning)
            })
        })
        .collect()
}

/// Runs `f` on the ribbon's speeds, rebuilding them only when the points or the roughness changed.
/// `f` must not query another ribbon: the cache borrow is held for the call.
fn with_ribbon_speeds<R>(points: &[RibbonPoint], manning: f32, f: impl FnOnce(&[f32]) -> R) -> R {
    let ptr = points.as_ptr() as usize;
    let fp = ribbon_fingerprint(points, manning);
    SPEED_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(i) = cache.iter().position(|slot| slot.ptr == ptr && slot.fp == fp) {
            let slot = cache.remove(i);
            cache.push(slot);
            let last = cache.len() - 1;
            return f(&cache[last].speeds);
        }
        let speeds = build_ribbon_speeds(points, manning);
        if cache.len() >= SPEED_CACHE_SLOTS {
            cache.remove(0);
        }
        cache.push(SpeedSlot { ptr, fp, speeds });
        let last = cache.len() - 1;
        f(&cache[last].speeds)
    })
}

/// `true` when `(x, z)` is outside the centerline's box expanded by the widest half-width.
fn ribbon_outside(points: &[RibbonPoint], x: f32, z: f32) -> bool {
    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut min_z = f32::INFINITY;
    let mut max_z = f32::NEG_INFINITY;
    let mut half = 0.0f32;
    for point in points {
        min_x = min_x.min(point.position[0]);
        max_x = max_x.max(point.position[0]);
        min_z = min_z.min(point.position[2]);
        max_z = max_z.max(point.position[2]);
        half = half.max(point.width * 0.5);
    }
    if !min_x.is_finite() {
        return true;
    }
    let pad = 1.0e-3;
    x < min_x - half - pad || x > max_x + half + pad || z < min_z - half - pad || z > max_z + half + pad
}

/// Unit normal of a centerline segment from its slope `dy` over the plan run.
fn segment_normal(dx: f32, dy: f32, dz: f32, len2: f32) -> [f32; 3] {
    if len2 <= 1.0e-8 {
        return [0.0, 1.0, 0.0];
    }
    let sx = dy * dx / len2;
    let sz = dy * dz / len2;
    let len = (sx * sx + 1.0 + sz * sz).sqrt();
    if len.is_finite() && len > 1.0e-12 {
        [-sx / len, 1.0 / len, -sz / len]
    } else {
        [0.0, 1.0, 0.0]
    }
}

fn note_speed_build() {
    #[cfg(test)]
    SPEED_BUILDS.with(|c| c.set(c.get().saturating_add(1)));
}

fn note_ribbon_scan() {
    #[cfg(test)]
    RIBBON_SCANS.with(|c| c.set(c.get().saturating_add(1)));
}

#[cfg(test)]
fn test_reset_ribbon_counts() {
    SPEED_BUILDS.with(|c| c.set(0));
    RIBBON_SCANS.with(|c| c.set(0));
}

#[cfg(test)]
fn test_speed_builds() -> u32 {
    SPEED_BUILDS.with(|c| c.get())
}

#[cfg(test)]
fn test_ribbon_scans() -> u32 {
    RIBBON_SCANS.with(|c| c.get())
}

pub mod caustics;
mod caustics_gpu;
pub(crate) use caustics_gpu::contract_rows as shaft_contract_rows;
pub mod clock;
pub mod cull;
pub mod float;
pub mod gpu;
pub mod mesh;
pub mod normal_map;
pub mod ocean;
pub mod ocean_foam;
mod ocean_gpu;
pub mod plugin;
pub mod query;
pub mod sea_state;
pub mod shore;
pub mod swell;

pub use caustics::WaterCaustics;
pub use caustics_gpu::{
    CausticStats, SHAFT_HISTORY_FRAMES, SHAFT_SHADOW_BIAS, WaterSunShadow, shaft_jitter_offset,
};
pub use clock::{WaterClock, flow_phases};
pub use float::{FloatForces, FloatPoint, FloatPose, Floater, FloaterState, float_forces};
pub use gpu::{WaterDebug, WaterFrameStats, WaterRenderer, WaterSky};
pub use mesh::{WaterMesh, WaterVertex, build_mesh};
pub use ocean::{FftOcean, OceanFrame, OceanSpectrum, OceanTables, WaveMode};
pub use ocean_gpu::{
    OCEAN_PASS_NAME, OceanCost, OceanGridError, OceanMaps, check_ocean_grid, ocean_binding_bytes,
};
pub use plugin::{TerrainWater, WaterPlugin, WaterSettings, WaterState, terrain_plane_body};
pub use query::{
    BodyRef, DrawnWaterCache, WaterDrawnOceans, WaterQuery, cache_drawn_water, drawn_cache_rebuilds,
    raycast_all, surface_at,
};
pub use sea_state::{
    MIN_FETCH, SEA_TABLE, SeaRow, SeaState, SeaWind, WeatherSea, apply_weather_sea, quantize_wind,
};
pub use shore::{ShoreBakeDesc, ShoreError, ShoreField, ShoreSample, ShoreStats, WaterShore};
pub use swell::{Swell, SwellWave};

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

/// Standard gravity (m/s²), for Manning's equation and the standing-wave length.
pub const GRAVITY: f32 = 9.81;

/// Most water bodies the GPU draws in one frame (uniform array size in `water_body.wgsl`).
pub const MAX_BODIES: usize = 16;

/// Most obstacles the GPU evaluates per frame (uniform array size in `water_body.wgsl`). The host
/// keeps the ones nearest the camera ([`gpu::WaterRenderer::set_bodies`]).
pub const MAX_OBSTACLES: usize = 64;

/// Slowest ribbon speed (m/s) Manning's equation is allowed to give: still pools.
pub const MIN_FLOW_SPEED: f32 = 0.02;

/// Fastest ribbon speed (m/s): a steep mountain chute.
pub const MAX_FLOW_SPEED: f32 = 6.0;

/// Smallest centerline slope used by Manning's equation (a flat reach still drains).
pub const MIN_SLOPE: f32 = 1.0e-4;

/// Steepest centerline slope used by Manning's equation (beyond it the water is a fall).
pub const MAX_SLOPE: f32 = 0.25;

/// Errors from [`WaterBody::validate`].
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum WaterBodyError {
    /// A ribbon needs at least two centerline points.
    #[error("a ribbon needs at least 2 points, got {0}")]
    TooFewPoints(usize),
    /// A point, size or color is NaN or infinite.
    #[error("{0} is not finite")]
    NotFinite(&'static str),
    /// A size that must be positive is not.
    #[error("{0} must be positive")]
    NotPositive(&'static str),
    /// A value is outside its documented range.
    #[error("{name} = {value} is outside {min}..={max}")]
    OutOfRange {
        /// Field name.
        name: &'static str,
        /// The value given.
        value: f32,
        /// Smallest allowed.
        min: f32,
        /// Largest allowed.
        max: f32,
    },
    /// Two consecutive ribbon points sit at the same place.
    #[error("ribbon points {0} and {1} coincide")]
    DuplicatePoint(usize, usize),
    /// A swell holds more waves than [`swell::MAX_SWELL_WAVES`].
    #[error("a swell holds at most {max} waves, got {got}")]
    TooManyWaves {
        /// Waves given.
        got: usize,
        /// Most allowed.
        max: usize,
    },
    /// The swell's fold sum `Σ steepness · k · A` is above [`swell::MAX_FOLD`]: crests would fold
    /// over themselves.
    #[error("swell fold sum {0} is above 1: lower the steepness or the heights")]
    SwellFolds(f32),
    /// A swell was given to a ribbon; only planes carry one.
    #[error("only a plane body carries a swell")]
    SwellNeedsPlane,
    /// An ocean was given to a ribbon; only planes carry one.
    #[error("only a plane body carries an ocean")]
    OceanNeedsPlane,
    /// The ocean's tiles do not fall by at least a factor of two.
    #[error("ocean tiles must run longest first, each at least twice the next")]
    OceanTiles,
    /// The ocean's grid is not 0 (the tier chooses) or one of [`ocean::OCEAN_GRIDS`].
    #[error("ocean grid must be 0 (the tier chooses), 128, 256, 512 or 1024, got {0}")]
    OceanGrid(u32),
    /// `caustics.tier_cost` is `low`: the caustic pass is compiled out of Low (ADR-2432).
    #[error("caustics.tier_cost must be high or ultra: the pass is compiled out of low")]
    CausticsTier,
    /// `caustics.shaft_steps` is not 0 or 8 to 16 (ADR-2432).
    #[error("caustics.shaft_steps must be 0 or 8 to 16, got {0}")]
    ShaftSteps(u32),
}

/// One centerline point of a ribbon water body.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RibbonPoint {
    /// World position of the water surface on the centerline (y is the surface height).
    pub position: [f32; 3],
    /// Water width at this point, bank to bank (m).
    pub width: f32,
    /// Water depth on the centerline (m), used for the flow speed.
    pub depth: f32,
    /// Surface speed on the centerline (m/s). `None`: Manning's equation from the slope, width and
    /// depth ([`manning_speed`]).
    #[serde(default)]
    pub speed: Option<f32>,
}

/// The shape of a water body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaterShape {
    /// A level surface: a lake, a pond or the sea. The plane is a rectangle the terrain clips.
    Plane {
        /// Center of the rectangle; y is the water level.
        center: [f32; 3],
        /// Half its size along x and z (m).
        half_extent: [f32; 2],
        /// Surface drift (m/s) along x and z; zero for still water.
        #[serde(default)]
        flow: [f32; 2],
    },
    /// A stream or river: a surface that follows its centerline, sloped where the points fall.
    Ribbon {
        /// Centerline points from the source downstream. The water flows from the first to the last.
        points: Vec<RibbonPoint>,
    },
}

/// How a body of water looks. Absorption and scattering are per meter of water crossed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WaterLook {
    /// Beer–Lambert absorption coefficient per channel (1/m). Pure water absorbs red most.
    pub absorption: [f32; 3],
    /// Scattering coefficient (1/m): how fast suspended matter turns the body opaque.
    pub scattering: f32,
    /// Color of the light scattered back by suspended matter (linear albedo, 0..1).
    pub scatter_color: [f32; 3],
    /// Surface roughness (GGX alpha is its square), 0.02..=1.
    pub roughness: f32,
    /// Ripple slope scale; 1 is the default chop, 0 is a mirror.
    pub normal_strength: f32,
    /// Ripple tile length (m): the wavelength of the finest ripple scale is about 1/8 of it.
    pub ripple_scale: f32,
    /// Screen-space refraction strength (High and Ultra), 0..=1.
    pub refraction: f32,
    /// Height above the water a bank stays wet (m).
    pub wet_height: f32,
    /// Width of the wet band beside a ribbon (m).
    pub wet_width: f32,
    /// How much darker a wet bank is (0 none, 1 black).
    pub wet_darkening: f32,
    /// Whitewater intensity around obstacles, 0..=4.
    pub foam: f32,
    /// Scale on the sun glint (1 is physical).
    pub glint: f32,
    /// Caustics on what lies below the surface and sun shafts under it (ADR-2432). Off by
    /// default and left out of a save while it is.
    #[serde(skip_serializing_if = "WaterCaustics::is_default")]
    pub caustics: WaterCaustics,
}

impl Default for WaterLook {
    fn default() -> Self {
        Self::clear_stream()
    }
}

impl WaterLook {
    /// A clear mountain stream over pebbles: little scattering, a faint brown-green tint.
    #[must_use]
    pub fn clear_stream() -> Self {
        Self {
            // Pope & Fry (1997) pure-water absorption is about 0.35, 0.06 and 0.015 1/m at 650,
            // 550 and 450 nm; stream water carries some humic matter, which absorbs blue.
            absorption: [0.3, 0.1, 0.09],
            scattering: 0.05,
            scatter_color: [0.28, 0.3, 0.24],
            roughness: 0.06,
            normal_strength: 1.0,
            ripple_scale: 1.5,
            refraction: 0.6,
            wet_height: 0.12,
            wet_width: 0.6,
            wet_darkening: 0.45,
            foam: 1.0,
            glint: 1.0,
            caustics: WaterCaustics::default(),
        }
    }

    /// A lake: deeper green-blue body, broader ripples.
    #[must_use]
    pub fn lake() -> Self {
        Self {
            absorption: [0.5, 0.18, 0.16],
            scattering: 0.25,
            scatter_color: [0.12, 0.3, 0.3],
            roughness: 0.05,
            normal_strength: 0.7,
            ripple_scale: 6.0,
            refraction: 0.5,
            wet_height: 0.08,
            wet_width: 1.5,
            wet_darkening: 0.4,
            foam: 0.5,
            glint: 1.0,
            caustics: WaterCaustics::default(),
        }
    }

    /// Open sea: blue body, longer ripples.
    #[must_use]
    pub fn ocean() -> Self {
        Self {
            absorption: [0.42, 0.07, 0.03],
            scattering: 0.12,
            scatter_color: [0.05, 0.22, 0.32],
            roughness: 0.07,
            normal_strength: 1.0,
            ripple_scale: 18.0,
            refraction: 0.4,
            wet_height: 0.3,
            wet_width: 3.0,
            wet_darkening: 0.4,
            foam: 0.5,
            glint: 1.0,
            caustics: WaterCaustics::default(),
        }
    }

    /// Checks every field is finite and inside its range.
    ///
    /// # Errors
    ///
    /// The first field that is not.
    pub fn validate(&self) -> Result<(), WaterBodyError> {
        for (name, v) in [
            ("look.absorption", self.absorption),
            ("look.scatter_color", self.scatter_color),
        ] {
            for c in v {
                range(name, c, 0.0, if name == "look.absorption" { 100.0 } else { 1.0 })?;
            }
        }
        range("look.scattering", self.scattering, 0.0, 100.0)?;
        range("look.roughness", self.roughness, 0.02, 1.0)?;
        range("look.normal_strength", self.normal_strength, 0.0, 8.0)?;
        range("look.ripple_scale", self.ripple_scale, 0.1, 1000.0)?;
        range("look.refraction", self.refraction, 0.0, 1.0)?;
        range("look.wet_height", self.wet_height, 0.0, 10.0)?;
        range("look.wet_width", self.wet_width, 0.0, 50.0)?;
        range("look.wet_darkening", self.wet_darkening, 0.0, 1.0)?;
        range("look.foam", self.foam, 0.0, 4.0)?;
        range("look.glint", self.glint, 0.0, 8.0)?;
        self.caustics.validate()?;
        Ok(())
    }
}

/// A rock or post that breaks the flow: the surface stands in waves upstream of it and trails a
/// V-shaped wake downstream. Applies to every body it sits in.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WaterObstacle {
    /// World position; x and z place it (y is ignored).
    pub position: [f32; 3],
    /// Radius at the waterline (m).
    pub radius: f32,
    /// Wave height scale, 0..=4 (1 is the velocity head `v² / 2g`).
    #[serde(default = "one")]
    pub strength: f32,
}

fn one() -> f32 {
    1.0
}

/// A thin shelf of ice along the edges, where the water is shallow and slow.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IceShelf {
    /// How far the shelf reaches in from a ribbon's bank (m).
    pub width: f32,
    /// Water shallower than this freezes on a plane or a ribbon (m).
    pub max_depth: f32,
    /// Ice albedo (linear).
    pub color: [f32; 3],
    /// How much of the bed shows through the ice, 0..=1.
    pub clarity: f32,
}

impl Default for IceShelf {
    fn default() -> Self {
        Self {
            width: 0.35,
            max_depth: 0.06,
            color: [0.7, 0.75, 0.8],
            clarity: 0.55,
        }
    }
}

impl IceShelf {
    /// Checks every field is finite and inside its range.
    ///
    /// # Errors
    ///
    /// The first field that is not.
    pub fn validate(&self) -> Result<(), WaterBodyError> {
        range("ice.width", self.width, 0.0, 100.0)?;
        range("ice.max_depth", self.max_depth, 0.0, 10.0)?;
        for c in self.color {
            range("ice.color", c, 0.0, 1.0)?;
        }
        range("ice.clarity", self.clarity, 0.0, 1.0)
    }
}

/// A body of water drawn by the runtime frame (ADR-1399).
#[derive(Component, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaterBody {
    /// Plane or ribbon.
    pub shape: WaterShape,
    /// Optics and surface.
    #[serde(default)]
    pub look: WaterLook,
    /// Obstacles that raise standing waves.
    #[serde(default)]
    pub obstacles: Vec<WaterObstacle>,
    /// Optional ice along the edges.
    #[serde(default)]
    pub ice: Option<IceShelf>,
    /// Manning roughness coefficient of the channel for [`manning_speed`] (0.03 for a clean
    /// natural stream, 0.05 for a weedy or bouldery one).
    #[serde(default = "default_manning")]
    pub manning_n: f32,
    /// Trochoidal swell of a plane (ADR-1620); `None` is a level surface.
    #[serde(default)]
    pub swell: Option<Swell>,
    /// FFT ocean of a plane (ADR-1969): drawn on High and Ultra, always answered by queries, and
    /// blended into the swell near a shore.
    #[serde(default)]
    pub ocean: Option<FftOcean>,
    /// The ocean set aside by [`Self::set_wave_mode`] while the plane runs its swell: not drawn,
    /// not asked by queries, brought back by [`WaveMode::Fft`] (ADR-2126).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_ocean: Option<FftOcean>,
}

fn default_manning() -> f32 {
    0.035
}

#[cfg(test)]
thread_local! {
    /// How many times [`WaterBody::surface_on`] ran on this thread (unit tests). Per thread on
    /// purpose: the harness runs the binary's tests in parallel, and a process-wide counter also
    /// counted the calls of every test beside the one that reads it (it read 11 for 1 once in a
    /// loaded run).
    pub(crate) static SURFACE_ON_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl WaterBody {
    /// A still lake or the sea: a level rectangle.
    #[must_use]
    pub fn plane(center: [f32; 3], half_extent: [f32; 2], look: WaterLook) -> Self {
        Self {
            shape: WaterShape::Plane {
                center,
                half_extent,
                flow: [0.0, 0.0],
            },
            look,
            obstacles: Vec::new(),
            ice: None,
            manning_n: default_manning(),
            swell: None,
            ocean: None,
            parked_ocean: None,
        }
    }

    /// A stream along `points`, flowing from the first point to the last.
    #[must_use]
    pub fn ribbon(points: Vec<RibbonPoint>, look: WaterLook) -> Self {
        Self {
            shape: WaterShape::Ribbon { points },
            look,
            obstacles: Vec::new(),
            ice: None,
            manning_n: default_manning(),
            swell: None,
            ocean: None,
            parked_ocean: None,
        }
    }

    /// Checks the shape, the look, the obstacles and the ice.
    ///
    /// # Errors
    ///
    /// The first problem found.
    pub fn validate(&self) -> Result<(), WaterBodyError> {
        match &self.shape {
            WaterShape::Plane {
                center,
                half_extent,
                flow,
            } => {
                finite3("plane.center", *center)?;
                for e in half_extent {
                    if !e.is_finite() {
                        return Err(WaterBodyError::NotFinite("plane.half_extent"));
                    }
                    if *e <= 0.0 {
                        return Err(WaterBodyError::NotPositive("plane.half_extent"));
                    }
                }
                for f in flow {
                    range("plane.flow", *f, -MAX_FLOW_SPEED, MAX_FLOW_SPEED)?;
                }
            }
            WaterShape::Ribbon { points } => {
                if points.len() < 2 {
                    return Err(WaterBodyError::TooFewPoints(points.len()));
                }
                for (i, p) in points.iter().enumerate() {
                    finite3("ribbon.position", p.position)?;
                    range("ribbon.width", p.width, 0.01, 10_000.0)?;
                    range("ribbon.depth", p.depth, 0.0, 1_000.0)?;
                    if let Some(speed) = p.speed {
                        range("ribbon.speed", speed, 0.0, MAX_FLOW_SPEED)?;
                    }
                    if i > 0 {
                        let a = points[i - 1].position;
                        let dx = p.position[0] - a[0];
                        let dz = p.position[2] - a[2];
                        if dx * dx + dz * dz < 1.0e-8 {
                            return Err(WaterBodyError::DuplicatePoint(i - 1, i));
                        }
                    }
                }
            }
        }
        self.look.validate()?;
        for o in &self.obstacles {
            finite3("obstacle.position", o.position)?;
            range("obstacle.radius", o.radius, 0.01, 100.0)?;
            range("obstacle.strength", o.strength, 0.0, 4.0)?;
        }
        if let Some(ice) = &self.ice {
            ice.validate()?;
        }
        if let Some(swell) = &self.swell {
            if !matches!(self.shape, WaterShape::Plane { .. }) {
                return Err(WaterBodyError::SwellNeedsPlane);
            }
            swell.validate()?;
        }
        if let Some(ocean) = &self.ocean {
            if !matches!(self.shape, WaterShape::Plane { .. }) {
                return Err(WaterBodyError::OceanNeedsPlane);
            }
            ocean.validate()?;
        }
        range("manning_n", self.manning_n, 0.005, 0.5)
    }

    /// The swell this body draws and answers queries with: a plane's non-empty [`Swell`].
    #[must_use]
    pub fn active_swell(&self) -> Option<&Swell> {
        match (&self.shape, &self.swell) {
            (WaterShape::Plane { .. }, Some(swell)) if !swell.waves.is_empty() => Some(swell),
            _ => None,
        }
    }

    /// The visible surface at world `(x, z)` at the clock's tick: `None` outside the body.
    ///
    /// On a swell plane the rest point under `(x, z)` is found by [`swell::INVERSE_ITERATIONS`]
    /// Newton steps, so the height is the displaced surface's there (every wave, whatever the
    /// tier draws); the body holds the point when its rest point is inside the rectangle. A level
    /// plane answers its level; a ribbon its sloped surface ([`Self::probe`]) with the normal of
    /// that slope.
    #[must_use]
    pub fn surface(&self, x: f32, z: f32, clock: &WaterClock) -> Option<WaterSurface> {
        self.surface_on(x, z, clock, None)
    }

    /// [`Self::surface`] over a baked bed (spec 87 § 3): with `shore`, a plane's swell is shoaled
    /// and capped by the depth at each rest point, a point whose bed is above the level misses
    /// (dry land), and the answer carries the depth, the shoreline distance, the breaker foam
    /// and the shore foam. A ribbon ignores `shore` (its banks are its width).
    #[must_use]
    pub fn surface_on(
        &self,
        x: f32,
        z: f32,
        clock: &WaterClock,
        shore: Option<&ShoreField>,
    ) -> Option<WaterSurface> {
        #[cfg(test)]
        SURFACE_ON_CALLS.with(|calls| calls.set(calls.get() + 1));
        let plane_shore = if matches!(self.shape, WaterShape::Plane { .. }) {
            shore
        } else {
            None
        };
        if let Some(ocean) = self.active_ocean() {
            return self.ocean_surface(ocean, x, z, clock, plane_shore);
        }
        let bed = |r: [f32; 2]| plane_shore.and_then(|f| f.sample(r[0], r[1]));
        if let (
            Some(swell),
            WaterShape::Plane {
                center,
                half_extent,
                flow,
            },
        ) = (self.active_swell(), &self.shape)
        {
            let grow = if plane_shore.is_some() {
                shore::SHOAL_MAX
            } else {
                1.0
            };
            let shift = swell.max_shift() * grow;
            let pad = 1.0e-3;
            // No rest point inside the rectangle can reach a query this far out.
            if (x - center[0]).abs() > half_extent[0] + shift + pad
                || (z - center[2]).abs() > half_extent[1] + shift + pad
            {
                return None;
            }
            let instant = swell.instant(clock.seconds(), [f64::from(x), f64::from(z)], usize::MAX);
            let (rest, miss) = instant.rest_point_on(x, z, |r| bed(r).map(|b| b.depth));
            let inside = (rest[0] - center[0]).abs() <= half_extent[0]
                && (rest[1] - center[2]).abs() <= half_extent[1];
            if !inside {
                return None;
            }
            let under = bed(rest);
            if under.is_some_and(|b| b.depth <= 0.0) {
                return None;
            }
            let (sample, ratio) = instant.sample_on(rest, under.map(|b| b.depth));
            let v = sample.velocity;
            return Some(WaterSurface {
                height: center[1] + sample.offset[1],
                normal: sample.normal(),
                velocity: [v[0] + flow[0], v[1], v[2] + flow[1]],
                crest_foam: instant.foam_at(&sample),
                rest,
                miss,
                depth: under.map(|b| b.depth),
                shore_distance: under.map(|b| b.distance.max(0.0)),
                breaking: if under.is_some() {
                    shore::breaker_foam(ratio)
                } else {
                    0.0
                },
                shore_foam: under.map_or(0.0, |b| {
                    shore::shore_foam(b.distance.max(0.0), self.look.wet_width)
                }),
            });
        }
        if matches!(self.shape, WaterShape::Ribbon { .. }) {
            // One walk: the normal is the winning segment's slope. A joint used to take a
            // finite difference that straddled both segments.
            let (p, normal) = self.ribbon_at(x, z)?;
            return Some(WaterSurface {
                height: p.surface_height,
                normal,
                velocity: [p.flow[0], 0.0, p.flow[1]],
                crest_foam: 0.0,
                rest: [x, z],
                miss: 0.0,
                depth: p.depth,
                shore_distance: Some(p.half_width - p.lateral.abs()),
                breaking: 0.0,
                shore_foam: 0.0,
            });
        }
        let p = self.probe(x, z)?;
        let under = bed([x, z]);
        if under.is_some_and(|b| b.depth <= 0.0) {
            return None;
        }
        let (depth, shore_distance) = match under {
            // Wet here, so at least 0 (depth and distance cross zero up to half a texel apart).
            Some(b) => (Some(b.depth), Some(b.distance.max(0.0))),
            None => (None, None),
        };
        Some(WaterSurface {
            height: p.surface_height,
            normal: [0.0, 1.0, 0.0],
            velocity: [p.flow[0], 0.0, p.flow[1]],
            crest_foam: 0.0,
            rest: [x, z],
            miss: 0.0,
            depth,
            shore_distance,
            breaking: 0.0,
            shore_foam: under.map_or(0.0, |b| {
                shore::shore_foam(b.distance.max(0.0), self.look.wet_width)
            }),
        })
    }

    /// The first point where the ray from `origin` along `direction` (any length) crosses this
    /// body's surface within `max_distance` meters (at most [`swell::MAX_RAY_DISTANCE`]), at the
    /// clock's tick; `None` when it does not.
    ///
    /// The ray is clipped to the body's box (its rectangle or the ribbon's bounds, between the
    /// lowest trough and the highest crest), marched in [`swell::RAY_MARCH_STEPS`] equal steps on
    /// the height above the surface, and the first sign change is bisected
    /// [`swell::RAY_BISECTIONS`] times. A crest thinner than one step can be stepped over on a
    /// long grazing ray; [`WaterHit::step`] says how long the step was.
    #[must_use]
    pub fn raycast(
        &self,
        origin: [f32; 3],
        direction: [f32; 3],
        max_distance: f32,
        clock: &WaterClock,
    ) -> Option<WaterHit> {
        self.raycast_on(origin, direction, max_distance, clock, None)
    }

    /// [`Self::raycast`] against the surface [`Self::surface_on`] answers over `shore` (shoaled
    /// waves; no hit over dry land).
    #[must_use]
    pub fn raycast_on(
        &self,
        origin: [f32; 3],
        direction: [f32; 3],
        max_distance: f32,
        clock: &WaterClock,
        shore: Option<&ShoreField>,
    ) -> Option<WaterHit> {
        let len = (direction[0].powi(2) + direction[1].powi(2) + direction[2].powi(2)).sqrt();
        if !(len.is_finite() && len > 1.0e-9) || !origin.iter().all(|c| c.is_finite()) {
            return None;
        }
        let dir = [direction[0] / len, direction[1] / len, direction[2] / len];
        let max_distance = if max_distance.is_finite() {
            max_distance.clamp(0.0, swell::MAX_RAY_DISTANCE)
        } else {
            swell::MAX_RAY_DISTANCE
        };
        let (lo, hi) = self.bounds_at(shore.is_some(), clock);
        // Slab clip against the box.
        let mut t0 = 0.0f32;
        let mut t1 = max_distance;
        for (((o, d), l), h) in origin.iter().zip(&dir).zip(&lo).zip(&hi) {
            if d.abs() < 1.0e-9 {
                if o < l || o > h {
                    return None;
                }
            } else {
                let a = (l - o) / d;
                let b = (h - o) / d;
                t0 = t0.max(a.min(b));
                t1 = t1.min(a.max(b));
            }
        }
        if t0 > t1 {
            return None;
        }
        let at = |t: f32| {
            [
                origin[0] + dir[0] * t,
                origin[1] + dir[1] * t,
                origin[2] + dir[2] * t,
            ]
        };
        let above = |t: f32| {
            let p = at(t);
            self.surface_on(p[0], p[2], clock, shore).map(|s| p[1] - s.height)
        };
        let step = (t1 - t0) / swell::RAY_MARCH_STEPS as f32;
        let mut prev: Option<(f32, f32)> = above(t0).map(|f| (t0, f));
        for i in 1..=swell::RAY_MARCH_STEPS {
            let t = t0 + step * i as f32;
            let cur = above(t).map(|f| (t, f));
            if let (Some((ta, fa)), Some((tb, fb))) = (prev, cur)
                && (fa > 0.0) != (fb > 0.0)
            {
                let (mut a, mut b) = (ta, tb);
                for _ in 0..swell::RAY_BISECTIONS {
                    let m = 0.5 * (a + b);
                    match above(m) {
                        Some(fm) if (fm > 0.0) == (fa > 0.0) => a = m,
                        _ => b = m,
                    }
                }
                let t = 0.5 * (a + b);
                return self.hit_at(at(t), t, fa <= 0.0, step, clock, shore);
            }
            prev = cur;
        }
        None
    }

    fn hit_at(
        &self,
        point: [f32; 3],
        distance: f32,
        from_below: bool,
        step: f32,
        clock: &WaterClock,
        shore: Option<&ShoreField>,
    ) -> Option<WaterHit> {
        let surface = self.surface_on(point[0], point[2], clock, shore)?;
        Some(WaterHit {
            distance,
            point: [point[0], surface.height, point[2]],
            normal: surface.normal,
            from_below,
            step,
            surface,
        })
    }

    /// Whether world `(x, z)` can lie on this body after the surface shifts sideways.
    ///
    /// The box is at least the XZ of `bounds_on`: a plane grows by its swell shift and, for an
    /// ocean, by the larger of the spectrum's shift bound and the stand-in swell's shift
    /// (shoaled by `shore::SHOAL_MAX` when a shore field is in use) plus a millimeter. A ribbon
    /// uses the box of its banks. A point outside is not wet, so a world query skips
    /// `surface_on`.
    fn may_contain_xz(&self, x: f32, z: f32, shoaled: bool) -> bool {
        let grow = if shoaled { shore::SHOAL_MAX } else { 1.0 };
        let pad = 1.0e-3;
        match &self.shape {
            WaterShape::Plane {
                center, half_extent, ..
            } => {
                let mut shift = self.active_swell().map_or(0.0, |swell| swell.max_shift() * grow);
                if let Some(ocean) = self.active_ocean() {
                    let tables = ocean::tables_for(ocean, ocean::OCEAN_QUERY_SIZE);
                    let ocean_shift = tables.shift_bound.max(tables.stand_in.max_shift() * grow);
                    shift = shift.max(ocean_shift);
                }
                (x - center[0]).abs() <= half_extent[0] + shift + pad
                    && (z - center[2]).abs() <= half_extent[1] + shift + pad
            }
            WaterShape::Ribbon { points } => {
                let mut lo_x = f32::INFINITY;
                let mut hi_x = f32::NEG_INFINITY;
                let mut lo_z = f32::INFINITY;
                let mut hi_z = f32::NEG_INFINITY;
                for point in points {
                    let half = 0.5 * point.width + pad;
                    lo_x = lo_x.min(point.position[0] - half);
                    hi_x = hi_x.max(point.position[0] + half);
                    lo_z = lo_z.min(point.position[2] - half);
                    hi_z = hi_z.max(point.position[2] + half);
                }
                x >= lo_x && x <= hi_x && z >= lo_z && z <= hi_z
            }
        }
    }

    /// The box the surface stays in: min and max x, y, z (m).
    #[must_use]
    pub fn bounds(&self) -> ([f32; 3], [f32; 3]) {
        self.bounds_on(false)
    }

    /// [`Self::bounds_on`] at the clock's tick: an ocean's reach at that instant, not its bound
    /// over all time, so a ray marches a tight box.
    #[must_use]
    pub fn bounds_at(&self, shoaled: bool, clock: &WaterClock) -> ([f32; 3], [f32; 3]) {
        let reach = self.active_ocean().map(|o| {
            let frame = ocean::frame_at(o, clock.seconds());
            (frame.max_rise(), frame.max_shift())
        });
        self.bounds_with(shoaled, reach)
    }

    /// [`Self::bounds`], widened by [`shore::SHOAL_MAX`] when the swell may be shoaled.
    #[must_use]
    pub fn bounds_on(&self, shoaled: bool) -> ([f32; 3], [f32; 3]) {
        let reach = self.active_ocean().map(|o| {
            let tables = ocean::tables_for(o, ocean::OCEAN_QUERY_SIZE);
            (tables.rise_bound, tables.shift_bound)
        });
        self.bounds_with(shoaled, reach)
    }

    /// The box with an ocean's reach `(rise, shift)` taken in: a blend of the ocean and the swell
    /// stays under the larger of the two.
    fn bounds_with(&self, shoaled: bool, ocean: Option<(f32, f32)>) -> ([f32; 3], [f32; 3]) {
        let grow = if shoaled { shore::SHOAL_MAX } else { 1.0 };
        match &self.shape {
            WaterShape::Plane {
                center, half_extent, ..
            } => {
                let (rise, shift) = self
                    .fallback_swell()
                    .map_or((0.0, 0.0), |s| (grow * s.max_rise(), grow * s.max_shift()));
                let (rise, shift) = ocean.map_or((rise, shift), |o| (rise.max(o.0), shift.max(o.1)));
                let pad = 1.0e-3;
                (
                    [
                        center[0] - half_extent[0] - shift - pad,
                        center[1] - rise - pad,
                        center[2] - half_extent[1] - shift - pad,
                    ],
                    [
                        center[0] + half_extent[0] + shift + pad,
                        center[1] + rise + pad,
                        center[2] + half_extent[1] + shift + pad,
                    ],
                )
            }
            WaterShape::Ribbon { points } => {
                let mut lo = [f32::INFINITY; 3];
                let mut hi = [f32::NEG_INFINITY; 3];
                for p in points {
                    let half = 0.5 * p.width;
                    let pad = [half, 1.0e-3, half];
                    for (((l, h), c), pad) in lo.iter_mut().zip(&mut hi).zip(p.position).zip(pad) {
                        *l = l.min(c - pad);
                        *h = h.max(c + pad);
                    }
                }
                (lo, hi)
            }
        }
    }

    /// Surface speed at each ribbon point (m/s): the authored speed, or [`manning_speed`] from the
    /// local slope (central difference over the neighbors). Empty for a plane.
    #[must_use]
    pub fn point_speeds(&self) -> Vec<f32> {
        let WaterShape::Ribbon { points } = &self.shape else {
            return Vec::new();
        };
        with_ribbon_speeds(points, self.manning_n, <[f32]>::to_vec)
    }

    /// Length of a ribbon's centerline in plan (m); 0 for a plane.
    #[must_use]
    pub fn length(&self) -> f32 {
        let WaterShape::Ribbon { points } = &self.shape else {
            return 0.0;
        };
        points
            .windows(2)
            .map(|w| {
                ((w[1].position[0] - w[0].position[0]).powi(2)
                    + (w[1].position[2] - w[0].position[2]).powi(2))
                .sqrt()
            })
            .sum()
    }

    /// What the water is at `(x, z)`: `None` when the point is outside this body.
    #[must_use]
    pub fn probe(&self, x: f32, z: f32) -> Option<WaterPoint> {
        match &self.shape {
            WaterShape::Plane {
                center,
                half_extent,
                flow,
            } => {
                let inside =
                    (x - center[0]).abs() <= half_extent[0] && (z - center[2]).abs() <= half_extent[1];
                inside.then(|| WaterPoint {
                    surface_height: center[1],
                    flow: *flow,
                    lateral: 0.0,
                    half_width: f32::INFINITY,
                    depth: None,
                    distance_along: 0.0,
                })
            }
            WaterShape::Ribbon { .. } => self.ribbon_at(x, z).map(|(point, _)| point),
        }
    }

    /// The ribbon sample at `(x, z)` and the normal of the winning segment.
    ///
    /// A point outside the centerline box is `None` without walking segments or building speeds.
    fn ribbon_at(&self, x: f32, z: f32) -> Option<(WaterPoint, [f32; 3])> {
        let WaterShape::Ribbon { points } = &self.shape else {
            return None;
        };
        // Fewer than two points has no segment (audit v033b: the subtraction underflowed).
        if points.len() < 2 || ribbon_outside(points, x, z) {
            return None;
        }
        note_ribbon_scan();
        with_ribbon_speeds(points, self.manning_n, |speeds| {
            let mut best: Option<(f32, WaterPoint, [f32; 3])> = None;
            let mut along = 0.0f32;
            for i in 0..points.len() - 1 {
                let a = points[i];
                let b = points[i + 1];
                let dx = b.position[0] - a.position[0];
                let dy = b.position[1] - a.position[1];
                let dz = b.position[2] - a.position[2];
                let len2 = dx * dx + dz * dz;
                let len = len2.sqrt();
                let px = x - a.position[0];
                let pz = z - a.position[2];
                let t = ((px * dx + pz * dz) / len2).clamp(0.0, 1.0);
                let cx = a.position[0] + dx * t;
                let cz = a.position[2] + dz * t;
                let dist2 = (x - cx).powi(2) + (z - cz).powi(2);
                let half = 0.5 * (a.width + (b.width - a.width) * t);
                if dist2 <= half * half && best.as_ref().is_none_or(|(d, _, _)| dist2 < *d) {
                    let (tx, tz) = (dx / len, dz / len);
                    // Right of the flow (looking downstream with +y up): `(-tz, tx)`.
                    let lateral = (x - cx) * -tz + (z - cz) * tx;
                    let speed = speeds[i] + (speeds[i + 1] - speeds[i]) * t;
                    let profile = mesh::lateral_profile(lateral / half.max(1.0e-4));
                    best = Some((
                        dist2,
                        WaterPoint {
                            surface_height: a.position[1] + dy * t,
                            flow: [tx * speed * profile, tz * speed * profile],
                            lateral,
                            half_width: half,
                            depth: Some(a.depth + (b.depth - a.depth) * t),
                            distance_along: along + len * t,
                        },
                        segment_normal(dx, dy, dz, len2),
                    ));
                }
                along += len;
            }
            best.map(|(_, point, normal)| (point, normal))
        })
    }
}

/// The visible surface of a [`WaterBody`] at one point ([`WaterBody::surface`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct WaterSurface {
    /// Height of the surface (m).
    pub height: f32,
    /// Unit surface normal.
    pub normal: [f32; 3],
    /// Velocity of the surface water (m/s): the swell's orbital motion plus the drift or flow.
    pub velocity: [f32; 3],
    /// Crest foam coverage, 0..=1, before the look's foam scale and noise (0 without a swell).
    pub crest_foam: f32,
    /// The rest point (x, z) on the still plane whose displaced position is this point.
    pub rest: [f32; 2],
    /// Horizontal distance (m) between the displaced rest point and the query point after the
    /// inverse (0 without a swell).
    pub miss: f32,
    /// Water depth (m): a plane's baked bed under the rest point ([`ShoreField`]), a ribbon's
    /// authored depth; `None` when unknown.
    pub depth: Option<f32>,
    /// Horizontal distance to the shoreline (m): a plane's baked field, a ribbon's bank;
    /// `None` when unknown.
    pub shore_distance: Option<f32>,
    /// Breaker foam, 0..=1, where shoaled waves near their breaking height (0 without a field).
    pub breaking: f32,
    /// Shore foam, 0..=1, over the look's `wet_width` band out from the shoreline (0 without a
    /// field).
    pub shore_foam: f32,
}

/// Where a ray meets a [`WaterBody`] ([`WaterBody::raycast`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct WaterHit {
    /// Distance along the ray (m).
    pub distance: f32,
    /// The hit point on the surface.
    pub point: [f32; 3],
    /// Unit surface normal there.
    pub normal: [f32; 3],
    /// The ray started under the surface and crossed it going up.
    pub from_below: bool,
    /// Length of one march step (m): crests thinner than this may be stepped over.
    pub step: f32,
    /// The full surface answer at the hit.
    pub surface: WaterSurface,
}

/// What a [`WaterBody`] is at one point ([`WaterBody::probe`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct WaterPoint {
    /// Water surface height (m).
    pub surface_height: f32,
    /// Surface velocity along x and z (m/s), slower toward a ribbon's banks.
    pub flow: [f32; 2],
    /// Signed distance from a ribbon's centerline (m, positive to the right looking downstream); 0
    /// on a plane.
    pub lateral: f32,
    /// Half the water width there (m); infinite on a plane.
    pub half_width: f32,
    /// Authored centerline depth of a ribbon (m); `None` on a plane (the bed decides).
    pub depth: Option<f32>,
    /// Distance from the ribbon's source along the centerline (m); 0 on a plane.
    pub distance_along: f32,
}

impl WaterPoint {
    /// Flow speed (m/s).
    #[must_use]
    pub fn speed(&self) -> f32 {
        (self.flow[0] * self.flow[0] + self.flow[1] * self.flow[1]).sqrt()
    }

    /// Wavelength (m) of the waves that stand still against this flow: the deep-water wave whose
    /// phase speed equals the flow speed, `2π v² / g`, kept within 0.15..=3 m.
    #[must_use]
    pub fn standing_wavelength(&self) -> f32 {
        standing_wavelength(self.speed())
    }

    /// Froude number `v / sqrt(g·h)` from the authored depth (`None` on a plane).
    #[must_use]
    pub fn froude(&self) -> Option<f32> {
        self.depth
            .filter(|d| *d > 0.0)
            .map(|d| self.speed() / (GRAVITY * d).sqrt())
    }
}

/// Mean flow speed of an open channel by Manning's equation, `v = R^(2/3) · S^(1/2) / n`, for a
/// rectangular section `width × depth` (hydraulic radius `R = w·d / (w + 2d)`), slope `S` (rise
/// over run, clamped to [`MIN_SLOPE`]..=[`MAX_SLOPE`]) and roughness `n`. The result is clamped
/// to [`MIN_FLOW_SPEED`]..=[`MAX_FLOW_SPEED`].
#[must_use]
pub fn manning_speed(width: f32, depth: f32, slope: f32, n: f32) -> f32 {
    let w = width.max(1.0e-3);
    let d = depth.max(1.0e-3);
    let radius = w * d / (w + 2.0 * d);
    let s = if slope.is_finite() {
        slope.clamp(MIN_SLOPE, MAX_SLOPE)
    } else {
        MIN_SLOPE
    };
    let v = radius.powf(2.0 / 3.0) * s.sqrt() / n.max(0.005);
    if v.is_finite() {
        v.clamp(MIN_FLOW_SPEED, MAX_FLOW_SPEED)
    } else {
        MIN_FLOW_SPEED
    }
}

/// Wavelength (m) of a deep-water wave whose phase speed is `speed`: `2π v² / g`, clamped to
/// 0.15..=3 m (shorter is capillary, longer is not a "small" standing wave).
#[must_use]
pub fn standing_wavelength(speed: f32) -> f32 {
    (std::f32::consts::TAU * speed * speed / GRAVITY).clamp(0.15, 3.0)
}

/// Picks the water tier for a render preset: Low on Steam Deck, mobile and Desktop Low (the cheap
/// fallback), High on Desktop High, Ultra on Desktop Ultra and Cinematic (offline, ADR-1403).
#[must_use]
pub fn tier_for_preset(preset: regia_render::RenderProfilePreset) -> WaterTier {
    use regia_render::RenderProfilePreset as P;
    match preset {
        P::MobileLow | P::MobileHigh | P::DesktopLow | P::Deck => WaterTier::Low,
        P::DesktopHigh => WaterTier::High,
        P::DesktopUltra | P::Cinematic => WaterTier::Ultra,
    }
}

/// How much work the water does per pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaterTier {
    /// Alpha-blended over the frame: absorption from depth, sky and sun reflection, wet banks, one
    /// ripple scale. No scene copy, no screen-space rays. Deck, mobile, Desktop Low.
    Low,
    /// Composited over a scene copy: refraction, screen-space reflection (24 steps), standing
    /// waves (32 obstacles), screen-space sun shadow, two ripple scales.
    #[default]
    High,
    /// High with 48 reflection steps, a finer hit search and 64 obstacles.
    Ultra,
}

impl WaterTier {
    /// Screen-space reflection march steps (0 = sky only).
    #[must_use]
    pub fn ssr_steps(self) -> u32 {
        match self {
            Self::Low => 0,
            Self::High => 24,
            Self::Ultra => 48,
        }
    }

    /// Obstacles evaluated per pixel.
    #[must_use]
    pub fn obstacle_budget(self) -> usize {
        match self {
            Self::Low => 0,
            Self::High => 32,
            Self::Ultra => MAX_OBSTACLES,
        }
    }

    /// `true` when the tier composites over a copy of the frame (refraction, reflection rays).
    #[must_use]
    pub fn composites(self) -> bool {
        !matches!(self, Self::Low)
    }

    /// Most swell waves the tier draws per body (queries always use all of them).
    #[must_use]
    pub fn swell_waves(self) -> usize {
        match self {
            Self::Low => 8,
            Self::High => 16,
            Self::Ultra => swell::MAX_SWELL_WAVES,
        }
    }

    /// Most cells across the half of a swell plane's inner ring-grid square
    /// ([`swell::RingGrid::for_view`]); narrow views use up to this many.
    #[must_use]
    pub fn ring_cells(self) -> u32 {
        match self {
            Self::Low => 16,
            Self::High => 32,
            Self::Ultra => 48,
        }
    }

    /// The tier's protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::High => "high",
            Self::Ultra => "ultra",
        }
    }

    /// Parses a protocol name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "low" => Some(Self::Low),
            "high" => Some(Self::High),
            "ultra" => Some(Self::Ultra),
            _ => None,
        }
    }
}

fn range(name: &'static str, value: f32, min: f32, max: f32) -> Result<(), WaterBodyError> {
    if !value.is_finite() {
        return Err(WaterBodyError::NotFinite(name));
    }
    if value < min || value > max {
        return Err(WaterBodyError::OutOfRange {
            name,
            value,
            min,
            max,
        });
    }
    Ok(())
}

fn finite3(name: &'static str, v: [f32; 3]) -> Result<(), WaterBodyError> {
    if v.iter().all(|c| c.is_finite()) {
        Ok(())
    } else {
        Err(WaterBodyError::NotFinite(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit v033b (Low): a ribbon built with fewer than two points underflowed `len() - 1` in
    /// `probe` (a panic in debug, an out-of-bounds index in release). It reports no water.
    #[test]
    fn a_ribbon_without_a_segment_has_no_water() {
        for points in [Vec::new(), stream_points().into_iter().take(1).collect()] {
            let body = WaterBody::ribbon(points, WaterLook::default());
            assert!(body.probe(0.0, 0.0).is_none());
        }
    }

    fn stream_points() -> Vec<RibbonPoint> {
        match stream().shape {
            WaterShape::Ribbon { points } => points,
            _ => Vec::new(),
        }
    }

    fn stream() -> WaterBody {
        let points = (0..6)
            .map(|i| RibbonPoint {
                position: [i as f32 * 2.0, 10.0 - i as f32 * 0.05, 0.0],
                width: 1.4,
                depth: 0.12,
                speed: None,
            })
            .collect();
        WaterBody::ribbon(points, WaterLook::clear_stream())
    }

    /// The showcase stream (1.4 m wide, 12 cm deep, 2.5 % fall) runs at under 1 m/s: a riffle,
    /// not a torrent. Deeper water at the same slope runs faster; a rougher bed runs slower.
    #[test]
    fn manning_speed_is_plausible_and_monotonic() {
        let v = manning_speed(1.4, 0.12, 0.025, 0.035);
        assert!((0.4..1.2).contains(&v), "{v}");
        assert!(manning_speed(1.4, 0.5, 0.025, 0.035) > v);
        assert!(manning_speed(1.4, 0.12, 0.025, 0.07) < v);
        assert!(manning_speed(1.4, 0.12, 0.1, 0.035) > v);
        // Flat and uphill reaches still drain at the floor; NaN does not propagate.
        assert!(manning_speed(1.4, 0.12, -0.3, 0.035) >= MIN_FLOW_SPEED);
        assert!(manning_speed(1.4, 0.12, f32::NAN, 0.035).is_finite());
        assert!(manning_speed(1.0e6, 1.0e6, 1.0, 0.005) <= MAX_FLOW_SPEED);
    }

    #[test]
    fn standing_wavelength_follows_deep_water_dispersion() {
        // 1 m/s: 2π / 9.81 ≈ 0.64 m.
        assert!((standing_wavelength(1.0) - 0.6405).abs() < 1.0e-3);
        assert_eq!(standing_wavelength(0.0), 0.15);
        // 6 m/s is 2π·36/9.81 ≈ 23 m before the 0.15..=3 clamp the probe and the rings use.
        assert_eq!(standing_wavelength(6.0), 3.0);
        assert_eq!(standing_wavelength(10.0), 3.0);
    }

    /// A point far from a ribbon builds no speeds and does not walk segments. A hit walks once
    /// and takes its normal from that segment's slope, and a second hit reuses the speeds.
    #[test]
    fn a_far_ribbon_misses_without_a_scan_and_a_hit_scans_once() {
        let mut points = Vec::new();
        for i in 0..32 {
            points.push(RibbonPoint {
                position: [i as f32 * 4.0, 8.0 - i as f32 * 0.1, 0.0],
                width: 2.0,
                depth: 0.3,
                speed: None,
            });
        }
        let body = WaterBody::ribbon(points, WaterLook::clear_stream());
        let clock = WaterClock::default();
        test_reset_ribbon_counts();
        assert!(body.surface(10_000.0, 10_000.0, &clock).is_none());
        assert_eq!(
            test_ribbon_scans(),
            0,
            "a bounds miss must not walk the centerline"
        );
        assert_eq!(test_speed_builds(), 0, "a bounds miss must not build speeds");
        test_reset_ribbon_counts();
        let hit = body.surface(10.0, 0.2, &clock).expect("on the ribbon");
        assert_eq!(test_ribbon_scans(), 1, "a hit walks the centerline once");
        assert_eq!(test_speed_builds(), 1, "speeds are built once");
        let _ = body.surface(14.0, -0.2, &clock).expect("still on the ribbon");
        assert_eq!(test_speed_builds(), 1, "the next sample reuses the speeds");
        assert_eq!(test_ribbon_scans(), 2);
        // Straight run along +x, falling 0.1 m per 4 m. Interior of a segment, so the
        // slope is dh/dx = -0.025, dh/dz = 0.
        let sx = -0.025f32;
        let len = (sx * sx + 1.0).sqrt();
        let want = [-sx / len, 1.0 / len, 0.0];
        for (got, exp) in hit.normal.iter().zip(want) {
            assert!((got - exp).abs() < 1.0e-4, "{:?} vs {want:?}", hit.normal);
        }
    }

    #[test]
    fn ribbon_probe_finds_center_banks_and_outside() {
        let body = stream();
        let center = body.probe(3.0, 0.0).expect("on the centerline");
        assert!((center.surface_height - (10.0 - 0.075)).abs() < 1.0e-4);
        assert!(center.lateral.abs() < 1.0e-5);
        assert!(
            center.flow[0] > 0.3 && center.flow[1].abs() < 1.0e-5,
            "{center:?}"
        );
        assert!((center.distance_along - 3.0).abs() < 1.0e-4);
        // Flowing +x with +y up, the right bank is +z.
        let right = body.probe(3.0, 0.6).expect("inside the right bank");
        assert!(right.lateral > 0.55, "{right:?}");
        assert!(right.speed() < center.speed(), "slower near the bank");
        assert!(body.probe(3.0, -0.9).is_none(), "outside the 1.4 m width");
        assert!(body.probe(-1.0, 0.0).is_none(), "upstream of the source");
        assert!(center.froude().is_some_and(|f| f > 0.0 && f < 1.5));
    }

    #[test]
    fn plane_probe_is_level_and_bounded() {
        let body = WaterBody::plane([0.0, 2.0, 0.0], [10.0, 5.0], WaterLook::lake());
        let p = body.probe(9.0, -4.0).expect("inside");
        assert_eq!(p.surface_height, 2.0);
        assert_eq!(p.speed(), 0.0);
        assert!(body.probe(11.0, 0.0).is_none());
    }

    #[test]
    fn validation_refuses_bad_bodies() {
        assert!(stream().validate().is_ok());
        let mut body = stream();
        if let WaterShape::Ribbon { points } = &mut body.shape {
            points.truncate(1);
        }
        assert_eq!(body.validate(), Err(WaterBodyError::TooFewPoints(1)));
        let mut body = stream();
        if let WaterShape::Ribbon { points } = &mut body.shape {
            points[2].position = points[1].position;
        }
        assert_eq!(body.validate(), Err(WaterBodyError::DuplicatePoint(1, 2)));
        let mut body = stream();
        body.look.roughness = f32::NAN;
        assert_eq!(body.validate(), Err(WaterBodyError::NotFinite("look.roughness")));
        let mut body = stream();
        body.obstacles.push(WaterObstacle {
            position: [1.0, 0.0, 0.0],
            radius: -1.0,
            strength: 1.0,
        });
        assert!(matches!(
            body.validate(),
            Err(WaterBodyError::OutOfRange {
                name: "obstacle.radius",
                ..
            })
        ));
        let mut body = WaterBody::plane([0.0; 3], [0.0, 1.0], WaterLook::ocean());
        assert_eq!(
            body.validate(),
            Err(WaterBodyError::NotPositive("plane.half_extent"))
        );
        body.shape = WaterShape::Plane {
            center: [0.0; 3],
            half_extent: [1.0, 1.0],
            flow: [0.0, 0.0],
        };
        body.ice = Some(IceShelf {
            clarity: 2.0,
            ..IceShelf::default()
        });
        assert!(body.validate().is_err());
    }

    #[test]
    fn body_round_trips_through_ron() {
        let mut body = stream();
        body.ice = Some(IceShelf::default());
        body.obstacles.push(WaterObstacle {
            position: [4.0, 0.0, 0.1],
            radius: 0.2,
            strength: 1.0,
        });
        let text = ron::to_string(&body).expect("serialize");
        let back: WaterBody = ron::from_str(&text).expect("parse");
        assert_eq!(back, body);
        // Missing look and obstacles take the defaults.
        let minimal: WaterBody =
            ron::from_str("(shape: plane(center: (0.0, 1.0, 0.0), half_extent: (5.0, 5.0)))").expect("parse");
        assert_eq!(minimal.look, WaterLook::default());
        assert!(minimal.validate().is_ok());
    }

    fn swell_sea() -> WaterBody {
        let mut sea = WaterBody::plane([10.0, 3.0, -5.0], [150.0, 150.0], WaterLook::ocean());
        sea.swell = Some(Swell::open_sea([1.0, 0.4], 1.5, 25.0, 10));
        sea
    }

    /// A ray hits the swell sea where the surface query says the surface is; a ray straight
    /// down hits at the queried height; from under the water the hit is flagged; rays outside the
    /// body or pointing away miss; a ribbon refuses a swell.
    #[test]
    fn raycast_agrees_with_the_surface_query() {
        let sea = swell_sea();
        assert!(sea.validate().is_ok(), "{:?}", sea.validate());
        let clock = WaterClock {
            tick: 9_000,
            ..WaterClock::default()
        };
        for (origin, dir) in [
            ([0.0f32, 20.0, 0.0], [0.3f32, -1.0, 0.2]),
            ([-40.0, 8.0, 12.0], [1.0, -0.12, -0.3]),
            ([25.0, 30.0, -60.0], [0.0, -1.0, 0.0]),
        ] {
            let hit = sea.raycast(origin, dir, 500.0, &clock).expect("hit");
            assert!(!hit.from_below);
            let q = sea.surface(hit.point[0], hit.point[2], &clock).expect("surface");
            assert!((q.height - hit.point[1]).abs() < 1.0e-4);
            // The hit lies on the ray.
            let len = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2]).sqrt();
            let y = origin[1] + dir[1] / len * hit.distance;
            assert!((y - q.height).abs() < 2.0e-3, "{y} vs {}", q.height);
        }
        let down = sea
            .raycast([25.0, 30.0, -60.0], [0.0, -1.0, 0.0], 500.0, &clock)
            .expect("hit");
        let q = sea.surface(25.0, -60.0, &clock).expect("surface");
        assert!((30.0 - down.distance - q.height).abs() < 2.0e-3);
        let up = sea
            .raycast([25.0, -10.0, -60.0], [0.0, 1.0, 0.0], 500.0, &clock)
            .expect("hit");
        assert!(up.from_below);
        assert!(
            sea.raycast([25.0, 30.0, -60.0], [0.0, 1.0, 0.0], 500.0, &clock)
                .is_none()
        );
        assert!(
            sea.raycast([500.0, 30.0, 0.0], [0.0, -1.0, 0.0], 500.0, &clock)
                .is_none()
        );
        assert!(
            sea.raycast([0.0, 30.0, 0.0], [0.0, 0.0, 0.0], 500.0, &clock)
                .is_none()
        );
        let mut ribbon = stream();
        ribbon.swell = sea.swell.clone();
        assert_eq!(ribbon.validate(), Err(WaterBodyError::SwellNeedsPlane));
        assert!(ribbon.active_swell().is_none());
    }

    /// The swell body round-trips through RON and an old body (no `swell`) still parses.
    #[test]
    fn swell_round_trips_through_ron() {
        let sea = swell_sea();
        let text = ron::to_string(&sea).expect("serialize");
        let back: WaterBody = ron::from_str(&text).expect("parse");
        assert_eq!(back, sea);
        let old: WaterBody =
            ron::from_str("(shape: plane(center: (0.0, 1.0, 0.0), half_extent: (5.0, 5.0)))").expect("parse");
        assert_eq!(old.swell, None);
        let wave: SwellWave =
            ron::from_str("(wavelength: 10.0, height: 1.0, direction: (1.0, 0.0))").expect("parse");
        assert_eq!(wave.steepness, 0.5);
        assert_eq!(wave.phase, 0.0);
    }

    #[test]
    fn tiers_follow_presets() {
        use regia_render::RenderProfilePreset as P;
        assert_eq!(tier_for_preset(P::Deck), WaterTier::Low);
        assert_eq!(tier_for_preset(P::MobileHigh), WaterTier::Low);
        assert_eq!(tier_for_preset(P::DesktopHigh), WaterTier::High);
        assert_eq!(tier_for_preset(P::DesktopUltra), WaterTier::Ultra);
        assert!(!WaterTier::Low.composites());
        assert_eq!(WaterTier::Low.ssr_steps(), 0);
        for tier in [WaterTier::Low, WaterTier::High, WaterTier::Ultra] {
            assert_eq!(WaterTier::from_name(tier.name()), Some(tier));
            assert!(tier.obstacle_budget() <= MAX_OBSTACLES);
        }
    }

    /// The caps `docs/specs/water.md` names: 16 bodies a frame (§ 1), at most 8 / 16 / 32 swell
    /// waves per body by tier and 128 in the frame's table, inner ring-grid half widths capped at
    /// 16 / 32 / 48 cells (§ 3), and the ocean's map size by tier (`water-ocean.md` § 3).
    #[test]
    fn tier_caps_follow_the_spec() {
        assert_eq!(MAX_BODIES, 16);
        assert_eq!(swell::MAX_SWELL_WAVES, 32);
        assert_eq!(swell::MAX_SWELL_TABLE, 128);
        let caps = |t: WaterTier| (t.swell_waves(), t.ring_cells(), t.ocean_size());
        assert_eq!(caps(WaterTier::Low), (8, 16, 0));
        assert_eq!(caps(WaterTier::High), (16, 32, 128));
        assert_eq!(caps(WaterTier::Ultra), (32, 48, 256));
        assert_eq!(WaterTier::default(), WaterTier::High);
    }
}
