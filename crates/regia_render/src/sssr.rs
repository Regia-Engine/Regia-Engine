// SPDX-License-Identifier: MPL-2.0
//! Stochastic screen-space reflections (Desktop High and Ultra).
//!
//! A port of AMD FidelityFX SSSR and its reflection denoiser (SDK v1.1.4, MIT) to wgpu compute.
//! One frame is:
//!
//! 1. a single-pass min-depth pyramid ([`crate::depth_pyramid`], seven levels; level 0 kept
//!    as next frame's depth history);
//! 2. `sssr_classify.wgsl`: decode the surface code from the HDR alpha, reconstruct the normal
//!    from depth, and compact the 2×2 quads that trace (one ray per quad, every pixel of a
//!    mirror) into a ray list and the 8×8 tiles with glossy pixels into a tile list;
//! 3. `sssr_intersect.wgsl`: a GGX visible-normal ray per traced pixel, marched through the
//!    pyramid (hierarchical traversal), validated, with the captured probes as the miss fallback;
//! 4. `sssr_reproject.wgsl`, `sssr_prefilter.wgsl`, `sssr_resolve.wgsl`: temporal reprojection,
//!    an edge-stopping spatial filter, and temporal accumulation on the tile list;
//! 5. `sssr_composite.wgsl`: `out = hdr · (1 − coverage · f) + W · radiance`.
//!
//! ## Surface inputs
//!
//! The renderer is forward, so there is no G-buffer. With stochastic SSR on, the forward pass
//! writes a 14-bit code into the HDR alpha (`lab_forward.wgsl` `ssr_pack`, see
//! [`pack_surface_code`]): perceptual roughness, the split-sum specular weight W, and the share f
//! of the pixel that is environment specular. The normal comes from depth, so normal maps do not
//! bend reflections. A 0..1 alpha (the older fade-coded roughness the terrain, impostors and
//! blended draws write) is not traced; a shared draw opts in by writing the code
//! (`SharedPassDraw::screen_space_reflective`).
//!
//! ## Budget
//!
//! About 50 bytes per pixel (≈ 178 MiB at 2560×1440), plus the "ssr color" output it shares
//! with the older SSR (8 bytes per pixel): see [`Targets`] for the formats and aliases.
//! The frame frees everything after `REFLECTION_IDLE_FRAMES` frames without a reflection pass.
//!
//! ## Provenance
//!
//! FidelityFX SDK v1.1.4 (`gpu/sssr/*`, `gpu/denoiser/ffx_denoiser_reflections_*`, `gpu/spd/*`),
//! Heitz 2018 (GGX VNDF sampling), Jimenez 2014 (interleaved gradient noise), Roberts 2018 (R2),
//! Playdead 2015 (history AABB clip); see `docs/ported-sources.toml` and `docs/provenance.md`.

use crate::frame_work::counted::*;
use std::borrow::Cow;
use std::collections::HashMap;

use crate::RenderError;
use crate::bind_cache::BindCache;
use crate::depth_pyramid::{Reduce, SpdChain, SpdKernel};

/// Default perceptual roughness below which reflection rays are traced; the value in effect is
/// [`sssr_roughness_cutoff`] (v0.1.14: a knob, [`set_sssr_roughness_cutoff`]).
pub const SSSR_ROUGHNESS_CUTOFF: f32 = 0.55;
/// The range [`set_sssr_roughness_cutoff`] and [`SSSR_ROUGHNESS_CUTOFF_ENV`] clamp to: from
/// [`SSSR_ROUGHNESS_BAND`] (a lower cutoff would fade even a mirror) to 1 (every surface traces).
pub const SSSR_ROUGHNESS_CUTOFF_RANGE: (f32, f32) = (SSSR_ROUGHNESS_BAND, 1.0);
/// Environment override of the cutoff (`REGIA_SSSR_ROUGHNESS_CUTOFF=0.7`): when it holds a
/// number it beats [`set_sssr_roughness_cutoff`], so a tester can try a value without a rebuild.
/// Read once, at the first frame that asks.
pub const SSSR_ROUGHNESS_CUTOFF_ENV: &str = "REGIA_SSSR_ROUGHNESS_CUTOFF";
/// Width of the roughness band below [`SSSR_ROUGHNESS_CUTOFF`] over which traced reflections fade
/// into the forward pass's environment specular.
pub const SSSR_ROUGHNESS_BAND: f32 = 0.15;
/// Depth thickness accepted behind a surface: 0.03 m per metre of view depth.
/// View depth is floored at 1 m, so the slab is at least 0.03 m. A hit at or
/// past that slab has confidence 0 (`validate_hit` in `sssr_intersect.wgsl`).
pub const SSSR_THICKNESS: f32 = 0.03;
/// Maximum hierarchical traversal steps per ray.
pub const SSSR_MAX_STEPS: u32 = 96;
/// Perceptual roughness from which a ray marches the half-resolution pyramid level (FidelityFX's
/// most detailed mip 1): the denoiser blurs these reflections over more than a pixel anyway.
pub const SSSR_HALF_RES_ROUGHNESS: f32 = 0.35;
/// Maximum traversal steps of a half-resolution ray.
pub const SSSR_HALF_RES_MAX_STEPS: u32 = 48;
/// History clip weight of the temporal resolve (FidelityFX `temporalStabilityFactor`).
const TEMPORAL_STABILITY: f32 = 0.7;
/// Temporal variance above which a pixel that shares its quad's ray traces its own.
const VARIANCE_THRESHOLD: f32 = 0.01;
/// Counter readbacks in a row with no glossy tile after which the pass goes dormant: only a
/// quarter-resolution probe (`sssr_probe.wgsl`) runs, watching for glossy pixels. The targets
/// stay allocated (ADR-168: a wake must not create resources mid-play, `specs/27` § 2.4) and are
/// cleared to zero on the wake, so the woken frame matches one on fresh targets; only memory
/// pressure ([`SssrPass::trim`]) frees them while dormant.
const DORMANT_AFTER: u32 = 30;
/// Coarsest pyramid level the traversal climbs to (FidelityFX `FFX_SSSR_DEPTH_HIERARCHY_MAX_MIP`).
const MAX_MIP: u32 = 6;

/// The host's cutoff ([`set_sssr_roughness_cutoff`]) as `f32` bits.
static CUTOFF_BITS: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(SSSR_ROUGHNESS_CUTOFF.to_bits());
/// [`SSSR_ROUGHNESS_CUTOFF_ENV`], parsed once.
static CUTOFF_ENV: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();

/// `cutoff` clamped to [`SSSR_ROUGHNESS_CUTOFF_RANGE`]; a non-finite value is the default.
fn clamp_cutoff(cutoff: f32) -> f32 {
    if cutoff.is_finite() {
        cutoff.clamp(SSSR_ROUGHNESS_CUTOFF_RANGE.0, SSSR_ROUGHNESS_CUTOFF_RANGE.1)
    } else {
        SSSR_ROUGHNESS_CUTOFF
    }
}

/// The cutoff an environment value names (`None` for an empty or non-numeric value).
fn parse_cutoff(text: &str) -> Option<f32> {
    text.trim()
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite())
        .map(clamp_cutoff)
}

/// Sets the perceptual roughness below which stochastic SSR traces rays, for every renderer in
/// the process, from the next frame (default [`SSSR_ROUGHNESS_CUTOFF`], 0.55). Traced
/// reflections fade into the environment specular over the [`SSSR_ROUGHNESS_BAND`] below it, so a
/// higher cutoff keeps sharp reflections on rougher surfaces (and traces more pixels). Clamped to
/// [`SSSR_ROUGHNESS_CUTOFF_RANGE`]; returns the value stored. Not persisted: a game that offers it
/// as a setting applies it at start. [`SSSR_ROUGHNESS_CUTOFF_ENV`] beats it while set.
pub fn set_sssr_roughness_cutoff(cutoff: f32) -> f32 {
    let cutoff = clamp_cutoff(cutoff);
    CUTOFF_BITS.store(cutoff.to_bits(), std::sync::atomic::Ordering::Relaxed);
    cutoff
}

/// [`SSSR_ROUGHNESS_CUTOFF_ENV`]'s value when it holds a number (clamped), else `None`.
#[must_use]
pub fn sssr_roughness_cutoff_env() -> Option<f32> {
    *CUTOFF_ENV.get_or_init(|| {
        regia_core::brand::env_var(SSSR_ROUGHNESS_CUTOFF_ENV)
            .ok()
            .and_then(|v| parse_cutoff(&v))
    })
}

/// The cutoff in effect: [`SSSR_ROUGHNESS_CUTOFF_ENV`] when set, else the value of
/// [`set_sssr_roughness_cutoff`] (default [`SSSR_ROUGHNESS_CUTOFF`]).
#[must_use]
pub fn sssr_roughness_cutoff() -> f32 {
    sssr_roughness_cutoff_env()
        .unwrap_or_else(|| f32::from_bits(CUTOFF_BITS.load(std::sync::atomic::Ordering::Relaxed)))
}

/// Work of a recent stochastic SSR frame, read back from the GPU a few frames late.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SssrStats {
    /// 2×2 quads that traced at least one ray.
    pub ray_quads: u32,
    /// 8×8 tiles with a glossy pixel (the denoiser's work).
    pub tiles: u32,
    /// The cost probe's counters of that frame ([`set_sssr_cost_probe`]); `None` when the probe
    /// was off (or the pass was dormant).
    pub cost: Option<SssrCostCounters>,
}

/// The stochastic SSR cost probe (ADR-2557): what one frame's classification and trace did,
/// counted on the GPU into a debug buffer and read back with [`SssrStats`]. Counts, not times, so
/// a software rasterizer reads the same numbers as a card for the same frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SssrCostCounters {
    /// Pixels classified mirror (perceptual roughness below 0.01): every one traces its own ray.
    pub mirror_pixels: u32,
    /// Pixels classified glossy (below the cutoff, not a mirror): one ray per 2×2 quad plus the
    /// variance-guided extra rays, then the denoiser.
    pub glossy_pixels: u32,
    /// Pixels skipped (sky, rough, or without a surface code).
    pub skipped_pixels: u32,
    /// Rays traced (pixels that traced their own ray).
    pub rays: u32,
    /// Glossy pixels that asked for their own ray from a noisy temporal history.
    pub variance_rays: u32,
    /// Rays the cost bound's tile budget dropped ([`SssrCostBound::tile_ray_budget`]).
    pub budget_dropped_rays: u32,
    /// Pixels that took a quad neighbour's ray instead of tracing one.
    pub shared_pixels: u32,
    /// Hierarchical traversal steps, summed over every ray.
    pub steps: u32,
    /// Rays that ran out of steps without a hit.
    pub exhausted_rays: u32,
    /// Rays that marched the half-resolution pyramid level.
    pub half_res_rays: u32,
    /// The most steps one ray took.
    pub max_steps: u32,
    /// Rays whose hit passed validation (some screen-space coverage).
    pub hits: u32,
}

/// Counter words of [`SssrCostCounters`] (the `SssrCost` struct of `sssr_common.wgsl`).
const COST_WORDS: usize = 12;

impl SssrCostCounters {
    /// Mean hierarchical steps per traced ray (0 without rays).
    #[must_use]
    pub fn steps_per_ray(&self) -> f32 {
        if self.rays == 0 {
            0.0
        } else {
            self.steps as f32 / self.rays as f32
        }
    }

    /// Pixels classified in all (mirror + glossy + skipped): the internal frame size.
    #[must_use]
    pub fn classified_pixels(&self) -> u64 {
        u64::from(self.mirror_pixels) + u64::from(self.glossy_pixels) + u64::from(self.skipped_pixels)
    }

    /// Decodes the counter words in `sssr_common.wgsl` order.
    fn from_words(w: &[u32; COST_WORDS]) -> Self {
        Self {
            mirror_pixels: w[0],
            glossy_pixels: w[1],
            skipped_pixels: w[2],
            rays: w[3],
            variance_rays: w[4],
            budget_dropped_rays: w[5],
            shared_pixels: w[6],
            steps: w[7],
            exhausted_rays: w[8],
            half_res_rays: w[9],
            max_steps: w[10],
            hits: w[11],
        }
    }
}

/// The opt-in cost bound of stochastic SSR (ADR-2557): limits a frame's trace by construction, so
/// a frame where most of the ground turns smooth (a wet road) cannot multiply the reflection cost.
/// Off by default ([`sssr_cost_bound`] is `None`): the base trace, bit for bit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SssrCostBound {
    /// Glossy pixels may still buy their own ray from a noisy temporal history (the base
    /// behaviour). `false` traces one ray per 2×2 quad and leaves the rest to the temporal resolve.
    pub variance_rays: bool,
    /// Own rays per 8×8 tile above which the tile traces one ray per quad (its first traced
    /// pixel, shared with the quad's other glossy pixels, mirrors included). 0: no cap.
    pub tile_ray_budget: u32,
    /// Traversal steps of a ray at the roughness cutoff; a mirror ray keeps the base limit and the
    /// limit falls linearly in between. 0: no roughness limit.
    pub min_steps: u32,
    /// View depth (m) from which a ray's step limit falls as `depth_m / view depth` (to half at
    /// twice the depth, never lower). 0: no distance limit.
    pub step_falloff_depth_m: f32,
    /// Perceptual roughness from which a ray marches the half-resolution pyramid level with
    /// [`SSSR_HALF_RES_MAX_STEPS`] (the base is [`SSSR_HALF_RES_ROUGHNESS`]).
    pub half_res_roughness: f32,
    /// Perceptual roughness from which glossy pixels (not mirrors) trace one ray per 4×4 block
    /// instead of one per 2×2 quad: the block's first such pixel traces and the others take its
    /// ray, so the denoiser sees a quarter of the samples there. 1: off (the quad rate).
    /// [`Self::sanitized`] moves it to the middle between two of the surface code's 32 roughness
    /// steps, so the classify and the trace always agree on a pixel.
    pub block_roughness: f32,
}

impl SssrCostBound {
    /// The bound chosen from the counters and the FLIP of the wet road scene
    /// (`tests/sssr_wet_cost.rs`, ADR-2557): no variance-guided rays, at most 16 own rays per
    /// tile, and one ray per 4×4 block from roughness 0.11 (3.5 / 31: wet concrete is about 0.46, a
    /// sealed wet surface about 0.16; a puddle, 0.02, keeps one ray per quad). The step limits
    /// and the lower half-resolution threshold are left off: the step limits removed under 1 % of
    /// the steps, and the half-resolution march from 0.1 lost most of the sealed surface's hits.
    pub const WET: Self = Self {
        variance_rays: false,
        tile_ray_budget: 16,
        min_steps: 0,
        step_falloff_depth_m: 0.0,
        half_res_roughness: SSSR_HALF_RES_ROUGHNESS,
        block_roughness: (3.0 + 0.5) / 31.0,
    };

    /// Every field in range: non-finite numbers take [`Self::WET`]'s value, the roughness is
    /// clamped to `0..=1`, the depth to `>= 0`, the steps to [`SSSR_MAX_STEPS`] and the tile budget
    /// to 64 (an 8×8 tile).
    #[must_use]
    pub fn sanitized(self) -> Self {
        let finite = |v: f32, fallback: f32| if v.is_finite() { v } else { fallback };
        Self {
            variance_rays: self.variance_rays,
            tile_ray_budget: self.tile_ray_budget.min(64),
            min_steps: self.min_steps.min(SSSR_MAX_STEPS),
            step_falloff_depth_m: finite(self.step_falloff_depth_m, Self::WET.step_falloff_depth_m).max(0.0),
            half_res_roughness: finite(self.half_res_roughness, Self::WET.half_res_roughness).clamp(0.0, 1.0),
            block_roughness: snap_between_codes(finite(self.block_roughness, 1.0)),
        }
    }

    /// The uniform's `bound` row (`sssr_common.wgsl`).
    fn row(&self) -> [f32; 4] {
        [
            1.0,
            self.tile_ray_budget as f32,
            self.min_steps as f32,
            self.step_falloff_depth_m,
        ]
    }
}

/// `roughness` clamped to `0..=1` and, below 1, moved to the middle between two of the surface
/// code's roughness steps (`k / 31`), where the 8-bit copy the trace reads cannot cross it.
fn snap_between_codes(roughness: f32) -> f32 {
    let r = roughness.clamp(0.0, 1.0);
    if r >= 1.0 {
        1.0
    } else {
        ((r * 31.0).floor() + 0.5) / 31.0
    }
}

/// Environment override of the cost bound (`REGIA_SSSR_COST_BOUND=wet` or `off`): when set to one
/// of those it beats [`set_sssr_cost_bound`] (`wet`, `on`, `1`: [`SssrCostBound::WET`]; `off`,
/// `0`: no bound), so a bench can compare both without a rebuild. Read once.
pub const SSSR_COST_BOUND_ENV: &str = "REGIA_SSSR_COST_BOUND";

/// The host's cost bound ([`set_sssr_cost_bound`]).
static COST_BOUND: std::sync::RwLock<Option<SssrCostBound>> = std::sync::RwLock::new(None);
/// [`SSSR_COST_BOUND_ENV`], parsed once: `Some(bound)` when it names one.
static COST_BOUND_ENV: std::sync::OnceLock<Option<Option<SssrCostBound>>> = std::sync::OnceLock::new();
/// The cost probe switch ([`set_sssr_cost_probe`]).
static COST_PROBE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The bound an environment value names: `Some(Some(WET))`, `Some(None)` (off) or `None` (not a
/// bound name: no override).
fn parse_cost_bound(text: &str) -> Option<Option<SssrCostBound>> {
    match text.trim().to_ascii_lowercase().as_str() {
        "wet" | "on" | "1" | "true" => Some(Some(SssrCostBound::WET)),
        "off" | "0" | "false" | "none" => Some(None),
        _ => None,
    }
}

/// Sets the stochastic SSR cost bound for every renderer in the process, from the next frame
/// (`None`, the default, is the base trace). Returns the previous host value. Not persisted.
/// [`SSSR_COST_BOUND_ENV`] beats it while set.
pub fn set_sssr_cost_bound(bound: Option<SssrCostBound>) -> Option<SssrCostBound> {
    let bound = bound.map(SssrCostBound::sanitized);
    match COST_BOUND.write() {
        Ok(mut guard) => std::mem::replace(&mut *guard, bound),
        Err(poisoned) => std::mem::replace(&mut *poisoned.into_inner(), bound),
    }
}

/// [`SSSR_COST_BOUND_ENV`]'s bound when it names one (`Some(None)` forces the bound off), else
/// `None`.
#[must_use]
pub fn sssr_cost_bound_env() -> Option<Option<SssrCostBound>> {
    *COST_BOUND_ENV.get_or_init(|| {
        regia_core::brand::env_var(SSSR_COST_BOUND_ENV)
            .ok()
            .and_then(|v| parse_cost_bound(&v))
    })
}

/// The cost bound in effect: [`SSSR_COST_BOUND_ENV`] when it names one, else the value of
/// [`set_sssr_cost_bound`] (default `None`).
#[must_use]
pub fn sssr_cost_bound() -> Option<SssrCostBound> {
    sssr_cost_bound_env().unwrap_or_else(|| match COST_BOUND.read() {
        Ok(guard) => *guard,
        Err(poisoned) => *poisoned.into_inner(),
    })
}

/// Turns the stochastic SSR cost probe on or off for every renderer in the process, from the
/// next frame; returns the previous value. On, the classify and intersect passes run variants
/// that count into a debug buffer ([`SssrCostCounters`], read back through [`SssrStats::cost`]).
/// The probe changes no pixel; it costs a few atomics per ray, so leave it off when timing.
/// The first probed frame builds the variants (two or three compute pipelines).
pub fn set_sssr_cost_probe(on: bool) -> bool {
    COST_PROBE.swap(on, std::sync::atomic::Ordering::Relaxed)
}

/// Whether the cost probe is on ([`set_sssr_cost_probe`]).
#[must_use]
pub fn sssr_cost_probe() -> bool {
    COST_PROBE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Bytes of one counters readback: the two list counters, then the cost words.
const READBACK_BYTES: u64 = 8 + COST_WORDS as u64 * 4;

/// One staging buffer of the counters readback.
struct StatsSlot {
    buffer: wgpu::Buffer,
    state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// [`SssrPass`] frame the copy was recorded in.
    frame: u64,
    /// The copy holds the cost probe's words after the counters.
    with_cost: bool,
}

/// One reflection probe as the shader reads it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct SssrProbe {
    /// xyz = centre.
    pub center: [f32; 4],
    /// xyz = half extent, w = blend weight.
    pub half: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SssrParams {
    inv_view_proj: [[f32; 4]; 4],
    view_proj: [[f32; 4]; 4],
    prev_view_proj: [[f32; 4]; 4],
    camera: [f32; 4],
    screen: [f32; 4],
    proj: [f32; 4],
    tuning: [f32; 4],
    temporal: [f32; 4],
    forward: [f32; 4],
    budget: [f32; 4],
    bound: [f32; 4],
    probes: [SssrProbe; 4],
    /// The local captures' boxes (ADR-2860): centre and weight, half extent and fade.
    captures: [SssrProbe; 4],
}

/// Inputs of one stochastic SSR frame.
pub(crate) struct SssrFrame<'a> {
    /// The blue-noise table for the ray directions (ADR-2075); `None` is the plain intersect.
    pub(crate) blue_noise: Option<&'a std::sync::Arc<crate::blue_noise::BlueNoiseGpu>>,
    /// Lit HDR colour with the surface code in alpha (Rgba16Float).
    pub hdr: &'a wgpu::TextureView,
    /// Output: a render target of the same size and format as `hdr`.
    pub out: &'a wgpu::TextureView,
    /// A query set and the first of three slots: stamped after the classification, after the
    /// trace and after the denoiser.
    pub stamps: Option<(&'a wgpu::QuerySet, u32)>,
    /// Single-sample depth of the same size (standard Z).
    pub depth: &'a wgpu::TextureView,
    /// Internal width.
    pub width: u32,
    /// Internal height.
    pub height: u32,
    /// HDR format (the composite target's).
    pub format: wgpu::TextureFormat,
    /// Inverse of [`Self::view_proj`].
    pub inv_view_proj: [[f32; 4]; 4],
    /// The (jittered) view-projection the depth was drawn with.
    pub view_proj: [[f32; 4]; 4],
    /// `[projection[2][2], projection[3][2]]`: linear depth = `b / (d + a)`.
    pub proj_terms: [f32; 2],
    /// Eye position.
    pub eye: [f32; 3],
    /// Unit view direction.
    pub forward: [f32; 3],
    /// Near plane distance.
    pub near: f32,
    /// Frame counter (noise sequence).
    pub frame_index: u64,
    /// Up to four probes and their count.
    pub probes: [SssrProbe; 4],
    /// Probes in use.
    pub probe_count: u32,
    /// The local reflection captures' boxes (ADR-2860; centre and weight, half extent and fade
    /// distance): a miss inside one keeps the forward pass's HDR capture. All zero without.
    pub captures: [SssrProbe; 4],
    /// LDR-to-scene scale of the probe images.
    pub probe_scale: f32,
    /// The probe equirect array (four layers).
    pub probe_view: &'a wgpu::TextureView,
    /// Its filtering sampler.
    pub probe_sampler: &'a wgpu::Sampler,
}

/// `true` when `format` can carry the surface code (a signed half-float alpha).
#[must_use]
pub(crate) fn format_supported(format: wgpu::TextureFormat) -> bool {
    format == wgpu::TextureFormat::Rgba16Float
}

/// `true` when `material` can write a glossy surface code (perceptual roughness below
/// [`sssr_roughness_cutoff`]) on some pixel. Conservative: anything that can lower the roughness
/// on the GPU (a material graph, a packed ORM map, detail roughness, a negative edge-wear bias)
/// counts. A frame without such a material skips the pass and pays nothing.
#[must_use]
pub(crate) fn material_may_be_glossy(material: &crate::LabMaterial) -> bool {
    let hs = &material.hard_surface;
    material.surface_graph.is_some()
        || material.roughness < sssr_roughness_cutoff()
        || material.orm_packed
        || hs.detail_roughness_strength > 0.0
        || (hs.edge_intensity > 0.0 && hs.edge_roughness_bias < 0.0)
}

/// The half-float bits `lab_forward.wgsl` `ssr_pack` writes into the HDR alpha.
///
/// 5 bits perceptual roughness, 5 bits √weight, 4 bits environment share, as a negative normal
/// half float (sign set, exponent 1..=16), so it is exact in Rgba16Float and never equals the
/// 0..1 alpha of the older SSR.
#[must_use]
pub fn pack_surface_code(perceptual: f32, weight: f32, env_share: f32) -> u16 {
    let q = |v: f32, max: f32| (v.clamp(0.0, 1.0) * max).round() as u32;
    let code =
        q(perceptual, 31.0) | (q(weight.clamp(0.0, 1.0).sqrt(), 31.0) << 5) | (q(env_share, 15.0) << 10);
    (0x8000 | ((1 + (code >> 10)) << 10) | (code & 1023)) as u16
}

/// Decodes [`pack_surface_code`]: `(perceptual roughness, weight, env share)`, or `None` for a
/// value that is not a code.
#[must_use]
pub fn unpack_surface_code(bits: u16) -> Option<(f32, f32, f32)> {
    let bits = u32::from(bits);
    let e = (bits >> 10) & 31;
    if bits & 0x8000 == 0 || !(1..=16).contains(&e) {
        return None;
    }
    let code = ((e - 1) << 10) | (bits & 1023);
    let r = (code & 31) as f32 / 31.0;
    let w = ((code >> 5) & 31) as f32 / 31.0;
    let f = ((code >> 10) & 15) as f32 / 15.0;
    Some((r, w * w, f))
}

struct Pipes {
    spd: SpdKernel,
    probe_bgl: wgpu::BindGroupLayout,
    probe: wgpu::ComputePipeline,
    /// The dormant probe's counters (same layout as the pass's).
    probe_counters: wgpu::Buffer,
    classify_bgl: wgpu::BindGroupLayout,
    classify: wgpu::ComputePipeline,
    args_bgl: wgpu::BindGroupLayout,
    args: wgpu::ComputePipeline,
    intersect_bgl: wgpu::BindGroupLayout,
    /// The intersect pass's blue-noise variant (ADR-2075), built when a frame or warm asks.
    intersect_blue: Option<wgpu::ComputePipeline>,
    intersect: wgpu::ComputePipeline,
    /// The cost probe's variants (`SSSR_STATS`, ADR-2557), built by the first probed frame.
    cost: Option<CostPipes>,
    reproject_bgl: wgpu::BindGroupLayout,
    reproject: wgpu::ComputePipeline,
    prefilter_bgl: wgpu::BindGroupLayout,
    prefilter: wgpu::ComputePipeline,
    resolve_bgl: wgpu::BindGroupLayout,
    resolve: wgpu::ComputePipeline,
    composite_bgl: wgpu::BindGroupLayout,
    composite: wgpu::RenderPipeline,
    format: wgpu::TextureFormat,
    linear: wgpu::Sampler,
    /// Nearest, clamp to edge (the reproject's normal-history gathers).
    point: wgpu::Sampler,
    params: wgpu::Buffer,
}

/// The cost probe's pipeline variants (`SSSR_STATS`): same layouts as the plain ones.
struct CostPipes {
    classify: wgpu::ComputePipeline,
    intersect: wgpu::ComputePipeline,
    /// With blue noise too, built when a probed frame uses blue noise.
    intersect_blue: Option<wgpu::ComputePipeline>,
}

/// Shader defs of a variant: the cost probe's `SSSR_STATS`, plus blue noise when `blue`.
fn cost_defs(blue: bool) -> HashMap<String, naga_oil::compose::ShaderDefValue> {
    let mut defs = if blue {
        crate::blue_noise::variant_defs()
    } else {
        HashMap::new()
    };
    defs.insert(
        "SSSR_STATS".to_string(),
        naga_oil::compose::ShaderDefValue::Bool(true),
    );
    defs
}

/// A 2-D texture with the views the passes bind.
struct Tex {
    /// Owned with its view; cleared on a wake from dormancy ([`Targets::clear`]).
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// Bytes of the texture (memory accounting).
    bytes: u64,
}

/// The min-depth pyramid: level 0 (this frame's depth, kept as next frame's history) in its own
/// texture per parity, levels 1.. in one half-size mip chain both parities write. One
/// single-pass downsampler dispatch writes them all.
struct Pyramid {
    level0: [Tex; 2],
    /// Owned with its views; cleared on a wake from dormancy ([`Targets::clear`]).
    mips: wgpu::Texture,
    /// Levels 1.. (the traversal above level 0).
    mips_all: wgpu::TextureView,
    chain: [SpdChain; 2],
    bytes: u64,
}

/// The persistent target set. FidelityFX-sized formats (packed into `R32Uint` where WGSL has no
/// 16-bit or 10:10:10:2 storage format), and every buffer whose lifetime allows it aliased:
///
/// - `normal` (×2, this frame and last): 12:12 octahedral normal + 8-bit roughness code.
/// - radiance: the traced reflection lives in `history[cur]` from the classify to the prefilter:
///   nothing reads that slot this frame before the resolve overwrites it. (Not the output
///   target: that is a render target the outdoor composite blends into, and a colour target
///   also bound as storage cost the denoiser ~0.05 ms at 1080p on a 5090.)
/// - `ray_var`: the ray length (f32 bits) from the intersect to the reproject, which reads it
///   and writes `pack2x16float(temporal variance, sample count)` over it (read-write).
/// - prefiltered radiance: the history slot the reproject has just read (`history[1 − cur]`).
/// - `variance`: one buffer, `pack2x16float(variance, sample count)`. The prefilter writes its
///   variance there and the resolve updates it in place (read-write); the next frame's
///   classify and reproject read it as the history.
struct Targets {
    size: (u32, u32),
    format: wgpu::TextureFormat,
    /// The output (composite target) these targets were built for.
    out: wgpu::TextureView,
    pyramid: Pyramid,
    normal: [Tex; 2],
    ray_var: Tex,
    // Bound only through the cached denoiser bind groups; owned here with the rest.
    #[allow(dead_code)]
    reprojected: Tex,
    #[allow(dead_code)]
    average: Tex,
    history: [Tex; 2],
    variance: Tex,
    ray_list: wgpu::Buffer,
    tile_list: wgpu::Buffer,
    counters: wgpu::Buffer,
    /// The cost probe's counters ([`SssrCostCounters`]); bound always, written by the probed
    /// variants only.
    cost: wgpu::Buffer,
    args: wgpu::Buffer,
    args_bg: wgpu::BindGroup,
    reproject_bg: [wgpu::BindGroup; 2],
    prefilter_bg: [wgpu::BindGroup; 2],
    resolve_bg: [wgpu::BindGroup; 2],
}

impl Targets {
    /// Zeroes every texture and buffer, as wgpu zero-initialises freshly created ones: a set
    /// kept through a dormant spell then gives the woken frame exactly the inputs a newly
    /// allocated set would (ADR-168). Needs [`wgpu::Features::CLEAR_TEXTURE`].
    fn clear(&self, encoder: &mut wgpu::CommandEncoder) {
        let whole = wgpu::ImageSubresourceRange::default();
        let textures = self
            .pyramid
            .level0
            .iter()
            .chain(&self.normal)
            .chain(&self.history)
            .chain([&self.ray_var, &self.reprojected, &self.average, &self.variance])
            .map(|t| &t.texture)
            .chain([&self.pyramid.mips]);
        for texture in textures {
            encoder.clear_texture(texture, &whole);
        }
        for buffer in [
            &self.ray_list,
            &self.tile_list,
            &self.counters,
            &self.cost,
            &self.args,
        ] {
            encoder.clear_buffer(buffer, 0, None);
        }
    }

    fn bytes(&self) -> u64 {
        let tex = self.pyramid.bytes
            + self.normal.iter().map(|t| t.bytes).sum::<u64>()
            + self.history.iter().map(|t| t.bytes).sum::<u64>()
            + self.ray_var.bytes
            + self.reprojected.bytes
            + self.average.bytes
            + self.variance.bytes;
        tex + self.ray_list.size()
            + self.tile_list.size()
            + self.counters.size()
            + self.cost.size()
            + self.args.size()
    }
}

/// Persistent stochastic SSR state: pipelines, targets and the reflection history.
#[derive(Default)]
pub(crate) struct SssrPass {
    pipes: Option<Pipes>,
    targets: Option<Targets>,
    param_bytes: Vec<u8>,
    /// Index of the history slot written this frame.
    parity: usize,
    /// The other slot holds last frame's reflection.
    history_valid: bool,
    last_view_proj: Option<[[f32; 4]; 4]>,
    /// Classify, intersect and composite bind groups keyed by the external views.
    classify_cache: BindCache<(usize, wgpu::TextureView, wgpu::TextureView)>,
    intersect_cache: BindCache<(usize, wgpu::TextureView, wgpu::TextureView, wgpu::Sampler)>,
    composite_cache: BindCache<(usize, wgpu::TextureView)>,
    /// Counters readback ring ([`Self::stats`]).
    stats_slots: Vec<StatsSlot>,
    /// Slot copied this frame, mapped by [`Self::after_submit`].
    stats_pending: Option<usize>,
    /// Frames recorded (stamps the readback slots).
    frames: u64,
    /// Newest counters read back, with the frame they came from.
    last_stats: Option<(u64, SssrStats)>,
    /// Readbacks in a row that found no glossy tile.
    idle_readbacks: u32,
    /// Asleep: only the probe runs ([`Self::sleeping`]); the targets are kept.
    dormant: bool,
    /// The kept targets hold a dormant spell's stale contents: the next [`Self::record`]
    /// clears them to zero first.
    stale_targets: bool,
    /// Probe bind groups keyed by the HDR and depth views.
    probe_cache: BindCache<(wgpu::TextureView, wgpu::TextureView)>,
    /// Complete frames (ADR-2582): the pass never goes dormant on a counter readback, whose
    /// landing frame depends on the GPU's speed.
    never_sleep: bool,
}

impl SssrPass {
    /// Builds the intersect pass's blue-noise variant once the plain set exists. Returns how many
    /// pipelines that asks for.
    pub(crate) fn prewarm_blue_noise(
        &mut self,
        device: &crate::device::TrackedDevice,
        noise: &crate::blue_noise::BlueNoiseGpu,
    ) -> Result<usize, RenderError> {
        let Some(pipes) = self.pipes.as_mut() else {
            return Ok(0);
        };
        if pipes.intersect_blue.is_none() {
            let naga = crate::shader_lib::compose_source(
                include_str!("../shaders/sssr_intersect.wgsl"),
                "sssr_intersect.wgsl",
                crate::blue_noise::variant_defs(),
            )?;
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("sssr intersect blue noise"),
                source: wgpu::ShaderSource::Naga(Cow::Owned(naga)),
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("sssr intersect blue noise"),
                bind_group_layouts: &[Some(&pipes.intersect_bgl), Some(&noise.layout)],
                immediate_size: 0,
            });
            pipes.intersect_blue = Some(device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("sssr intersect blue noise"),
                layout: Some(&layout),
                module: &module,
                entry_point: Some("intersect"),
                compilation_options: Default::default(),
                cache: crate::pipeline_cache::cache_for(&device).as_deref(),
            }));
        }
        Ok(1)
    }

    /// Builds the cost probe's variants (and the blue-noise one when `noise` is given) once the
    /// plain set exists.
    fn ensure_cost_pipes(
        &mut self,
        device: &crate::device::TrackedDevice,
        noise: Option<&crate::blue_noise::BlueNoiseGpu>,
    ) -> Result<(), RenderError> {
        let Some(pipes) = self.pipes.as_mut() else {
            return Ok(());
        };
        let variant = |source: &str,
                       file: &str,
                       blue: bool,
                       layouts: &[Option<&wgpu::BindGroupLayout>],
                       entry: &'static str|
         -> Result<wgpu::ComputePipeline, RenderError> {
            let naga = crate::shader_lib::compose_source(source, file, cost_defs(blue))?;
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("sssr cost probe"),
                source: wgpu::ShaderSource::Naga(Cow::Owned(naga)),
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("sssr cost probe"),
                bind_group_layouts: layouts,
                immediate_size: 0,
            });
            Ok(device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("sssr cost probe"),
                layout: Some(&layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: crate::pipeline_cache::cache_for(&device).as_deref(),
            }))
        };
        let intersect_src = include_str!("../shaders/sssr_intersect.wgsl");
        if pipes.cost.is_none() {
            pipes.cost = Some(CostPipes {
                classify: variant(
                    include_str!("../shaders/sssr_classify.wgsl"),
                    "sssr_classify.wgsl",
                    false,
                    &[Some(&pipes.classify_bgl)],
                    "classify",
                )?,
                intersect: variant(
                    intersect_src,
                    "sssr_intersect.wgsl",
                    false,
                    &[Some(&pipes.intersect_bgl)],
                    "intersect",
                )?,
                intersect_blue: None,
            });
        }
        if let (Some(noise), Some(cost)) = (noise, pipes.cost.as_mut())
            && cost.intersect_blue.is_none()
        {
            cost.intersect_blue = Some(variant(
                intersect_src,
                "sssr_intersect.wgsl",
                true,
                &[Some(&pipes.intersect_bgl), Some(&noise.layout)],
                "intersect",
            )?);
        }
        Ok(())
    }

    /// The newest ray and tile counts read back (a few frames old), `None` before the first.
    pub(crate) fn stats(&self) -> Option<SssrStats> {
        self.last_stats.map(|(_, stats)| stats)
    }

    /// Whether this frame should run only the dormant probe ([`Self::record_probe`]): the last
    /// [`DORMANT_AFTER`] readbacks found nothing glossy. Going dormant keeps the targets and
    /// their bind groups (ADR-168) and drops the history; the wake clears the targets.
    pub(crate) fn sleeping(&mut self) -> bool {
        self.collect_stats();
        if self.never_sleep {
            // Woken as by a glossy readback: the targets a dormant spell kept are cleared first.
            self.dormant = false;
            self.idle_readbacks = 0;
            return false;
        }
        if !self.dormant && self.idle_readbacks >= DORMANT_AFTER {
            self.dormant = true;
            self.reset_history();
            self.stale_targets = self.targets.is_some();
        }
        self.dormant
    }

    /// Complete frames (ADR-2582): never go dormant on a readback, and wake now.
    pub(crate) fn set_never_sleep(&mut self, on: bool) {
        self.never_sleep = on;
    }

    /// Whether the pass is dormant (only the probe runs).
    pub(crate) fn dormant(&self) -> bool {
        self.dormant
    }

    /// Memory pressure: frees the targets if the pass is dormant (they are not in use) and
    /// returns the bytes freed. The wake then allocates them again (the pre-ADR-168 path).
    pub(crate) fn trim(&mut self) -> u64 {
        if !self.dormant {
            return 0;
        }
        let bytes = self.target_bytes();
        self.release_targets();
        bytes
    }

    /// The dormant frame: counts the 16×16 blocks with a glossy pixel (one sample per 2×2) for
    /// the readback that wakes the pass, and writes the three stamps. Draws nothing.
    ///
    /// # Errors
    ///
    /// A shader fails to compose.
    #[tracing::instrument(skip_all)]
    pub(crate) fn record_probe(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        frame: &SssrFrame<'_>,
    ) -> Result<(), RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        self.frames += 1;
        self.collect_stats();
        let stats_slot = self.stats_slot(device);
        self.ensure_pipes(device, frame.format)?;
        let Some(pipes) = self.pipes.as_ref() else {
            return Err(RenderError::Shader("sssr: no pipelines".into()));
        };
        let (w, h) = (frame.width.max(1), frame.height.max(1));
        let params = sssr_params(frame, (w, h), None, false);
        belt.write_if_changed(
            device,
            encoder,
            &pipes.params,
            bytemuck::bytes_of(&params),
            &mut self.param_bytes,
        );
        encoder.clear_buffer(&pipes.probe_counters, 0, None);
        let bind_group = probe_bind_group(&mut self.probe_cache, device, pipes, frame);
        {
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("sssr probe"),
                timestamp_writes: None,
            });
            pass.counted_set_pipeline(&pipes.probe);
            pass.counted_set_bind_group(0, &bind_group, &[]);
            pass.counted_dispatch(w.div_ceil(16), h.div_ceil(16), 1);
        }
        if let Some(index) = stats_slot
            && let Some(slot) = self.stats_slots.get_mut(index)
        {
            encoder.copy_buffer_to_buffer(&pipes.probe_counters, 0, &slot.buffer, 0, 8);
            slot.with_cost = false;
            self.stats_pending = Some(index);
        }
        if let Some((set, base)) = frame.stamps {
            for k in 0..3 {
                encoder.write_timestamp(set, base + k);
            }
        }
        Ok(())
    }

    /// Maps the counters copied by this frame's [`Self::record`]; call after the submit.
    pub(crate) fn after_submit(&mut self) {
        let Some(index) = self.stats_pending.take() else {
            return;
        };
        let Some(slot) = self.stats_slots.get(index) else {
            return;
        };
        let state = std::sync::Arc::clone(&slot.state);
        state.store(crate::readback::ST_INFLIGHT, std::sync::atomic::Ordering::Release);
        slot.buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                crate::readback::store_map_result(&state, result.is_ok());
            });
    }

    /// Takes every mapped readback slot (the device poll ran their callbacks).
    ///
    /// The dormancy counter must end at the same value for the same counters whatever order the
    /// map callbacks left the slots ready, so the ready slots decode first and apply in ascending
    /// frame order, and a readback older than one already applied is ignored (an older glossy
    /// result must not reset a newer run of empty ones, and an older empty one must not add to a
    /// newer run).
    fn collect_stats(&mut self) {
        use std::sync::atomic::Ordering;
        let mut decoded: Vec<(u64, u32, u32, Option<SssrCostCounters>)> = Vec::new();
        for slot in &self.stats_slots {
            match slot.state.load(Ordering::Acquire) {
                crate::readback::ST_READY => {
                    if let Ok(view) = slot.buffer.slice(..).get_mapped_range() {
                        let words: Vec<u32> = view
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|c| u32::from_le_bytes(*c))
                            .collect();
                        drop(view);
                        let cost = slot
                            .with_cost
                            .then(|| words.get(2..2 + COST_WORDS))
                            .flatten()
                            .and_then(|w| <&[u32; COST_WORDS]>::try_from(w).ok())
                            .map(SssrCostCounters::from_words);
                        if let [ray_quads, tiles, ..] = words[..] {
                            decoded.push((slot.frame, ray_quads, tiles, cost));
                        }
                    }
                    slot.buffer.unmap();
                    slot.state.store(crate::readback::ST_IDLE, Ordering::Release);
                }
                crate::readback::ST_FAILED => slot.state.store(crate::readback::ST_IDLE, Ordering::Release),
                _ => {}
            }
        }
        decoded.sort_by_key(|(frame, ..)| *frame);
        for (frame, ray_quads, tiles, cost) in decoded {
            self.apply_stats_frame(frame, ray_quads, tiles, cost);
        }
    }

    /// Applies one counters readback to the dormancy counter and the newest stats. Readbacks
    /// apply in ascending frame order ([`Self::collect_stats`]); a frame older than the newest
    /// applied is a stale duplicate of history the counter already saw and changes nothing.
    fn apply_stats_frame(&mut self, frame: u64, ray_quads: u32, tiles: u32, cost: Option<SssrCostCounters>) {
        if self.last_stats.is_some_and(|(newest, _)| newest >= frame) {
            return;
        }
        if tiles == 0 {
            self.idle_readbacks = self.idle_readbacks.saturating_add(1);
        } else {
            // Something glossy in view: the next frame runs the full pass.
            self.idle_readbacks = 0;
            self.dormant = false;
        }
        self.last_stats = Some((
            frame,
            SssrStats {
                ray_quads,
                tiles,
                cost,
            },
        ));
    }

    /// An idle readback slot for this frame's counters (created on first use).
    fn stats_slot(&mut self, device: &wgpu::Device) -> Option<usize> {
        if self.stats_pending.is_some() {
            return None;
        }
        if self.stats_slots.is_empty() {
            self.stats_slots = (0..crate::readback::READBACK_SLOTS)
                .map(|_| StatsSlot {
                    buffer: device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("sssr counters readback"),
                        size: READBACK_BYTES,
                        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                        mapped_at_creation: false,
                    }),
                    state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(crate::readback::ST_IDLE)),
                    frame: 0,
                    with_cost: false,
                })
                .collect();
        }
        let index = self.stats_slots.iter().position(|slot| {
            slot.state.load(std::sync::atomic::Ordering::Acquire) == crate::readback::ST_IDLE
        })?;
        self.stats_slots[index].frame = self.frames;
        Some(index)
    }

    /// Drops the history (a camera cut or a resize): the next frame starts accumulating again.
    pub(crate) fn reset_history(&mut self) {
        self.history_valid = false;
        self.last_view_proj = None;
    }

    /// Frees every target and the bind groups that reference them; the next
    /// [`Self::record`] allocates them again and starts a new history.
    pub(crate) fn release_targets(&mut self) {
        if self.targets.take().is_some() {
            self.clear_caches();
            self.reset_history();
        }
        self.stale_targets = false;
    }

    /// Bytes of the allocated targets and buffers (0 when released).
    pub(crate) fn target_bytes(&self) -> u64 {
        self.targets.as_ref().map_or(0, Targets::bytes)
    }

    /// Records one SSSR frame into `frame.out` and returns that view.
    ///
    /// # Errors
    ///
    /// A shader fails to compose.
    #[tracing::instrument(skip_all)]
    pub(crate) fn record(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        frame: &SssrFrame<'_>,
    ) -> Result<wgpu::TextureView, RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        self.frames += 1;
        self.collect_stats();
        let stats_slot = self.stats_slot(device);
        self.ensure_pipes(device, frame.format)?;
        if let Some(noise) = frame.blue_noise {
            self.prewarm_blue_noise(device, noise)?;
        }
        let probed = sssr_cost_probe();
        if probed {
            self.ensure_cost_pipes(device, frame.blue_noise.map(|noise| &**noise))?;
        }
        self.ensure_targets(
            device,
            frame.width.max(1),
            frame.height.max(1),
            frame.format,
            frame.out,
        );
        let (Some(pipes), Some(targets)) = (self.pipes.as_ref(), self.targets.as_mut()) else {
            return Err(RenderError::Shader("sssr: no pipelines".into()));
        };
        if std::mem::take(&mut self.stale_targets) {
            targets.clear(encoder);
        }
        // The dormant probe's bind group for these views, built while awake: the first dormant
        // frame then creates nothing mid-play (`specs/27` § 2.4).
        probe_bind_group(&mut self.probe_cache, device, pipes, frame);
        let cur = self.parity;
        let (w, h) = targets.size;
        let params = sssr_params(frame, (w, h), self.last_view_proj, self.history_valid);
        belt.write_if_changed(
            device,
            encoder,
            &pipes.params,
            bytemuck::bytes_of(&params),
            &mut self.param_bytes,
        );
        encoder.clear_buffer(&targets.counters, 0, None);
        let cost_pipes = pipes.cost.as_ref().filter(|_| probed);
        if cost_pipes.is_some() {
            encoder.clear_buffer(&targets.cost, 0, None);
        }
        let classify_bg =
            self.classify_cache
                .get_or_insert_with((cur, frame.hdr.clone(), frame.depth.clone()), || {
                    device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("sssr classify"),
                        layout: &pipes.classify_bgl,
                        entries: &[
                            buffer(0, &pipes.params),
                            view(1, frame.hdr),
                            view(2, frame.depth),
                            view(3, &targets.normal[cur].view),
                            view(4, &targets.history[cur].view),
                            buffer(5, &targets.ray_list),
                            buffer(6, &targets.tile_list),
                            buffer(7, &targets.counters),
                            view(8, &targets.variance.view),
                            buffer(10, &targets.cost),
                        ],
                    })
                });
        let intersect_bg = self.intersect_cache.get_or_insert_with(
            (
                cur,
                frame.hdr.clone(),
                frame.probe_view.clone(),
                frame.probe_sampler.clone(),
            ),
            || {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("sssr intersect"),
                    layout: &pipes.intersect_bgl,
                    entries: &[
                        buffer(0, &pipes.params),
                        view(1, frame.hdr),
                        view(2, &targets.pyramid.mips_all),
                        view(10, &targets.pyramid.level0[cur].view),
                        view(3, &targets.normal[cur].view),
                        buffer(4, &targets.ray_list),
                        buffer(5, &targets.counters),
                        view(6, &targets.history[cur].view),
                        view(7, &targets.ray_var.view),
                        view(8, frame.probe_view),
                        wgpu::BindGroupEntry {
                            binding: 9,
                            resource: wgpu::BindingResource::Sampler(frame.probe_sampler),
                        },
                        buffer(11, &targets.cost),
                    ],
                })
            },
        );
        {
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("sssr trace"),
                timestamp_writes: None,
            });
            targets.pyramid.chain[cur].record(device, &pipes.spd, &mut pass, frame.depth);
            pass.counted_set_pipeline(cost_pipes.map_or(&pipes.classify, |c| &c.classify));
            pass.counted_set_bind_group(0, &classify_bg, &[]);
            pass.counted_dispatch(w.div_ceil(8), h.div_ceil(8), 1);
            pass.counted_set_pipeline(&pipes.args);
            pass.counted_set_bind_group(0, &targets.args_bg, &[]);
            pass.counted_dispatch(1, 1, 1);
        }
        if let Some((set, base)) = frame.stamps {
            encoder.write_timestamp(set, base);
        }
        {
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("sssr intersect"),
                timestamp_writes: None,
            });
            let (plain, blue) = match cost_pipes {
                Some(c) => (&c.intersect, c.intersect_blue.as_ref()),
                None => (&pipes.intersect, pipes.intersect_blue.as_ref()),
            };
            match (frame.blue_noise, blue) {
                (Some(noise), Some(variant)) => {
                    pass.counted_set_pipeline(variant);
                    pass.counted_set_bind_group(1, &noise.group, &[]);
                }
                _ => pass.counted_set_pipeline(plain),
            }
            pass.counted_set_bind_group(0, &intersect_bg, &[]);
            pass.counted_dispatch_indirect(&targets.args, 0);
        }
        if let Some(index) = stats_slot
            && let Some(slot) = self.stats_slots.get_mut(index)
        {
            encoder.copy_buffer_to_buffer(&targets.counters, 0, &slot.buffer, 0, 8);
            slot.with_cost = cost_pipes.is_some();
            if slot.with_cost {
                encoder.copy_buffer_to_buffer(&targets.cost, 0, &slot.buffer, 8, READBACK_BYTES - 8);
            }
            self.stats_pending = Some(index);
        }
        if let Some((set, base)) = frame.stamps {
            encoder.write_timestamp(set, base + 1);
        }
        {
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("sssr denoise"),
                timestamp_writes: None,
            });
            pass.counted_set_pipeline(&pipes.reproject);
            pass.counted_set_bind_group(0, &targets.reproject_bg[cur], &[]);
            pass.counted_dispatch_indirect(&targets.args, 12);
            pass.counted_set_pipeline(&pipes.prefilter);
            pass.counted_set_bind_group(0, &targets.prefilter_bg[cur], &[]);
            pass.counted_dispatch_indirect(&targets.args, 12);
            pass.counted_set_pipeline(&pipes.resolve);
            pass.counted_set_bind_group(0, &targets.resolve_bg[cur], &[]);
            pass.counted_dispatch_indirect(&targets.args, 12);
        }
        if let Some((set, base)) = frame.stamps {
            encoder.write_timestamp(set, base + 2);
        }
        let composite_bg = self
            .composite_cache
            .get_or_insert_with((cur, frame.hdr.clone()), || {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("sssr composite"),
                    layout: &pipes.composite_bgl,
                    entries: &[
                        buffer(0, &pipes.params),
                        view(1, frame.hdr),
                        view(2, &targets.history[cur].view),
                        view(3, &targets.pyramid.level0[cur].view),
                    ],
                })
            });
        {
            let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("sssr composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &targets.out,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.counted_set_pipeline(&pipes.composite);
            pass.counted_set_bind_group(0, &composite_bg, &[]);
            pass.counted_draw(0..3, 0..1);
        }
        let out = targets.out.clone();
        self.parity = 1 - cur;
        self.history_valid = true;
        self.last_view_proj = Some(frame.view_proj);
        Ok(out)
    }

    fn ensure_pipes(
        &mut self,
        device: &crate::device::TrackedDevice,
        format: wgpu::TextureFormat,
    ) -> Result<(), RenderError> {
        if self.pipes.as_ref().is_some_and(|p| p.format == format) {
            return Ok(());
        }
        let c = wgpu::ShaderStages::COMPUTE;
        let nf = |b: u32| tex_entry(b, c, false, wgpu::TextureViewDimension::D2);
        let filt = |b: u32| tex_entry(b, c, true, wgpu::TextureViewDimension::D2);
        let ut = |b: u32| uint_tex_entry(b, c);
        let wo = wgpu::StorageTextureAccess::WriteOnly;
        let rw = wgpu::StorageTextureAccess::ReadWrite;
        let st = |b: u32, f: wgpu::TextureFormat, access| storage_tex_entry(b, f, access);
        let ssbo = |b: u32, ro: bool| storage_buf_entry(b, ro);
        let samp = |b: u32| wgpu::BindGroupLayoutEntry {
            binding: b,
            visibility: c,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        use wgpu::TextureFormat::{R32Uint, Rgba16Float};
        let classify_bgl = bgl(
            device,
            "sssr classify",
            &[
                uniform_entry(0, c),
                nf(1),
                nf(2),
                st(3, R32Uint, wo),
                st(4, Rgba16Float, wo),
                ssbo(5, false),
                ssbo(6, false),
                ssbo(7, false),
                ut(8),
                ssbo(10, false),
            ],
        );
        let probe_bgl = bgl(
            device,
            "sssr probe",
            &[uniform_entry(0, c), nf(1), nf(2), ssbo(7, false)],
        );
        let args_bgl = bgl(device, "sssr args", &[ssbo(7, false), ssbo(9, false)]);
        let intersect_bgl = bgl(
            device,
            "sssr intersect",
            &[
                uniform_entry(0, c),
                nf(1),
                nf(2),
                ut(3),
                ssbo(4, true),
                ssbo(5, true),
                st(6, Rgba16Float, wo),
                st(7, R32Uint, wo),
                tex_entry(8, c, true, wgpu::TextureViewDimension::D2Array),
                samp(9),
                nf(10),
                ssbo(11, false),
            ],
        );
        let reproject_bgl = bgl(
            device,
            "sssr reproject",
            &[
                uniform_entry(0, c),
                ssbo(1, true),
                nf(2),
                ut(4),
                ut(5),
                nf(6),
                nf(7),
                filt(8),
                ut(9),
                samp(11),
                st(12, Rgba16Float, wo),
                st(13, R32Uint, rw),
                st(14, Rgba16Float, wo),
                ssbo(15, true),
                wgpu::BindGroupLayoutEntry {
                    binding: 16,
                    visibility: c,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                    count: None,
                },
            ],
        );
        let prefilter_bgl = bgl(
            device,
            "sssr prefilter",
            &[
                uniform_entry(0, c),
                ssbo(1, true),
                nf(2),
                ut(3),
                ut(4),
                nf(5),
                filt(6),
                samp(7),
                st(8, Rgba16Float, wo),
                st(9, R32Uint, wo),
                ssbo(15, true),
            ],
        );
        let resolve_bgl = bgl(
            device,
            "sssr resolve",
            &[
                uniform_entry(0, c),
                ssbo(1, true),
                nf(2),
                filt(5),
                nf(6),
                ut(7),
                samp(8),
                st(9, Rgba16Float, wo),
                st(10, R32Uint, rw),
                ssbo(15, true),
            ],
        );
        let f = wgpu::ShaderStages::FRAGMENT;
        let composite_bgl = bgl(
            device,
            "sssr composite",
            &[
                uniform_entry(0, f),
                tex_entry(1, f, false, wgpu::TextureViewDimension::D2),
                tex_entry(2, f, false, wgpu::TextureViewDimension::D2),
                tex_entry(3, f, false, wgpu::TextureViewDimension::D2),
            ],
        );
        let probe_mod = module(
            device,
            include_str!("../shaders/sssr_probe.wgsl"),
            "sssr_probe.wgsl",
        )?;
        let classify_mod = module(
            device,
            include_str!("../shaders/sssr_classify.wgsl"),
            "sssr_classify.wgsl",
        )?;
        let intersect_mod = module(
            device,
            include_str!("../shaders/sssr_intersect.wgsl"),
            "sssr_intersect.wgsl",
        )?;
        let reproject_mod = module(
            device,
            include_str!("../shaders/sssr_reproject.wgsl"),
            "sssr_reproject.wgsl",
        )?;
        let prefilter_mod = module(
            device,
            include_str!("../shaders/sssr_prefilter.wgsl"),
            "sssr_prefilter.wgsl",
        )?;
        let resolve_mod = module(
            device,
            include_str!("../shaders/sssr_resolve.wgsl"),
            "sssr_resolve.wgsl",
        )?;
        let composite_mod = module(
            device,
            include_str!("../shaders/sssr_composite.wgsl"),
            "sssr_composite.wgsl",
        )?;
        let composite_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sssr composite"),
            bind_group_layouts: &[Some(&composite_bgl)],
            immediate_size: 0,
        });
        let composite = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("sssr composite"),
            layout: Some(&composite_layout),
            vertex: wgpu::VertexState {
                module: &composite_mod,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &composite_mod,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&device).as_deref(),
        });
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sssr params"),
            size: size_of::<SssrParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let point = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sssr point"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let linear = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sssr linear"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        // The seven compute pipelines compile side by side (`pipeline_warm::join_all`).
        type Job<'a> = Box<dyn FnOnce() -> wgpu::ComputePipeline + Send + 'a>;
        let jobs: Vec<Job<'_>> = vec![
            Box::new(|| compute(device, &probe_bgl, &probe_mod, "probe")),
            Box::new(|| compute(device, &classify_bgl, &classify_mod, "classify")),
            Box::new(|| compute(device, &args_bgl, &classify_mod, "prepare_args")),
            Box::new(|| compute(device, &intersect_bgl, &intersect_mod, "intersect")),
            Box::new(|| compute(device, &reproject_bgl, &reproject_mod, "reproject")),
            Box::new(|| compute(device, &prefilter_bgl, &prefilter_mod, "prefilter")),
            Box::new(|| compute(device, &resolve_bgl, &resolve_mod, "resolve")),
        ];
        let [probe, classify, args, intersect, reproject, prefilter, resolve] =
            <[wgpu::ComputePipeline; 7]>::try_from(crate::pipeline_warm::join_all(jobs))
                .map_err(|_| RenderError::Shader("sssr pipelines".into()))?;
        self.pipes = Some(Pipes {
            spd: SpdKernel::new(device, Reduce::Min)?,
            probe,
            probe_bgl,
            probe_counters: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("sssr probe counters"),
                size: 8,
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_DST
                    | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }),
            classify,
            args,
            intersect,
            reproject,
            prefilter,
            resolve,
            classify_bgl,
            args_bgl,
            intersect_bgl,
            intersect_blue: None,
            cost: None,
            reproject_bgl,
            prefilter_bgl,
            resolve_bgl,
            composite_bgl,
            composite,
            format,
            linear,
            point,
            params,
        });
        // New layouts invalidate every bind group.
        self.targets = None;
        self.param_bytes.clear();
        self.clear_caches();
        Ok(())
    }

    fn clear_caches(&mut self) {
        self.probe_cache.clear();
        self.classify_cache.clear();
        self.intersect_cache.clear();
        self.composite_cache.clear();
    }

    fn ensure_targets(
        &mut self,
        device: &crate::device::TrackedDevice,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
        out: &wgpu::TextureView,
    ) {
        // Without `CLEAR_TEXTURE` a kept set cannot be zeroed: allocate a fresh one instead.
        if self.stale_targets && !device.features().contains(wgpu::Features::CLEAR_TEXTURE) {
            self.release_targets();
        }
        if self
            .targets
            .as_ref()
            .is_some_and(|t| t.size == (width, height) && t.format == format && t.out == *out)
        {
            return;
        }
        // A new set starts zeroed.
        self.stale_targets = false;
        let Some(pipes) = self.pipes.as_ref() else {
            return;
        };
        let sampled_storage = wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING;
        let make = |format: wgpu::TextureFormat, w: u32, h: u32, label: &str| -> Tex {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: sampled_storage,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bytes = u64::from(w) * u64::from(h) * u64::from(format.block_copy_size(None).unwrap_or(4));
            Tex { texture, view, bytes }
        };
        use wgpu::TextureFormat::{R32Float, R32Uint, Rgba16Float};
        let full = |format, label: &str| make(format, width, height, label);
        let pyramid = {
            let mips = pyramid_mips(width, height);
            let level0 = [full(R32Float, "sssr depth 0"), full(R32Float, "sssr depth 1")];
            let (mw, mh) = ((width >> 1).max(1), (height >> 1).max(1));
            let mip_count = (mips - 1).max(1);
            let label = "sssr depth mips";
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: mw,
                    height: mh,
                    depth_or_array_layers: 1,
                },
                mip_level_count: mip_count,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: R32Float,
                usage: sampled_storage,
                view_formats: &[],
            });
            let level = |l: u32, usage: wgpu::TextureUsages| {
                texture.create_view(&wgpu::TextureViewDescriptor {
                    label: Some(label),
                    usage: Some(usage),
                    base_mip_level: l,
                    mip_level_count: Some(1),
                    ..Default::default()
                })
            };
            let upper = mips - 1;
            let chain = [0usize, 1].map(|parity| {
                let storage: Vec<_> = std::iter::once(level0[parity].view.clone())
                    .chain((0..upper).map(|l| level(l, wgpu::TextureUsages::STORAGE_BINDING)))
                    .collect();
                let sampled: Vec<_> = std::iter::once(level0[parity].view.clone())
                    .chain((0..upper).map(|l| level(l, wgpu::TextureUsages::TEXTURE_BINDING)))
                    .collect();
                SpdChain::new(device, &pipes.spd, &storage, &sampled, width, height)
            });
            let mips_all = texture.create_view(&wgpu::TextureViewDescriptor {
                label: Some(label),
                usage: Some(wgpu::TextureUsages::TEXTURE_BINDING),
                ..Default::default()
            });
            let bytes = level0[0].bytes
                + level0[1].bytes
                + (0..mip_count)
                    .map(|l| u64::from((mw >> l).max(1)) * u64::from((mh >> l).max(1)) * 4)
                    .sum::<u64>();
            Pyramid {
                level0,
                mips: texture,
                mips_all,
                chain,
                bytes,
            }
        };
        let normal = [full(R32Uint, "sssr normal 0"), full(R32Uint, "sssr normal 1")];
        let ray_var = full(R32Uint, "sssr ray length + variance");
        let reprojected = full(Rgba16Float, "sssr reprojected");
        let average = make(Rgba16Float, width.div_ceil(8), height.div_ceil(8), "sssr average");
        // Slot 1 − cur also holds the prefiltered radiance between the reproject and the resolve.
        let history = [
            full(Rgba16Float, "sssr history 0"),
            full(Rgba16Float, "sssr history 1"),
        ];
        let variance = full(R32Uint, "sssr variance");
        let buf = |label: &str, size: u64, usage: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: size.max(16),
                usage,
                mapped_at_creation: false,
            })
        };
        // COPY_DST: zeroed on a wake from dormancy (`Targets::clear`).
        let storage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
        // One entry per 2×2 quad (`sssr_classify.wgsl` `pack_quad`).
        let ray_list = buf(
            "sssr rays",
            u64::from(width.div_ceil(2)) * u64::from(height.div_ceil(2)) * 4,
            storage,
        );
        let tile_list = buf(
            "sssr tiles",
            u64::from(width.div_ceil(8)) * u64::from(height.div_ceil(8)) * 4,
            storage,
        );
        let counters = buf("sssr counters", 8, storage | wgpu::BufferUsages::COPY_SRC);
        let cost = buf(
            "sssr cost probe",
            COST_WORDS as u64 * 4,
            storage | wgpu::BufferUsages::COPY_SRC,
        );
        let args = buf("sssr args", 24, storage | wgpu::BufferUsages::INDIRECT);
        let args_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sssr args"),
            layout: &pipes.args_bgl,
            entries: &[buffer(7, &counters), buffer(9, &args)],
        });
        let sampler = |b: u32| wgpu::BindGroupEntry {
            binding: b,
            resource: wgpu::BindingResource::Sampler(&pipes.linear),
        };
        let reproject_bg = [0usize, 1].map(|cur| {
            let prev = 1 - cur;
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("sssr reproject"),
                layout: &pipes.reproject_bgl,
                entries: &[
                    buffer(0, &pipes.params),
                    buffer(1, &tile_list),
                    view(2, &history[cur].view),
                    view(4, &normal[cur].view),
                    view(5, &normal[prev].view),
                    view(6, &pyramid.level0[cur].view),
                    view(7, &pyramid.level0[prev].view),
                    view(8, &history[prev].view),
                    view(9, &variance.view),
                    sampler(11),
                    view(12, &reprojected.view),
                    view(13, &ray_var.view),
                    view(14, &average.view),
                    buffer(15, &counters),
                    wgpu::BindGroupEntry {
                        binding: 16,
                        resource: wgpu::BindingResource::Sampler(&pipes.point),
                    },
                ],
            })
        });
        let prefilter_bg = [0usize, 1].map(|cur| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("sssr prefilter"),
                layout: &pipes.prefilter_bgl,
                entries: &[
                    buffer(0, &pipes.params),
                    buffer(1, &tile_list),
                    view(2, &history[cur].view),
                    view(3, &ray_var.view),
                    view(4, &normal[cur].view),
                    view(5, &pyramid.level0[cur].view),
                    view(6, &average.view),
                    sampler(7),
                    view(8, &history[1 - cur].view),
                    view(9, &variance.view),
                    buffer(15, &counters),
                ],
            })
        });
        let resolve_bg = [0usize, 1].map(|cur| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("sssr resolve"),
                layout: &pipes.resolve_bgl,
                entries: &[
                    buffer(0, &pipes.params),
                    buffer(1, &tile_list),
                    view(2, &history[1 - cur].view),
                    view(5, &average.view),
                    view(6, &reprojected.view),
                    view(7, &normal[cur].view),
                    sampler(8),
                    view(9, &history[cur].view),
                    view(10, &variance.view),
                    buffer(15, &counters),
                ],
            })
        });
        self.targets = Some(Targets {
            size: (width, height),
            format,
            out: out.clone(),
            pyramid,
            normal,
            ray_var,
            reprojected,
            average,
            history,
            variance,
            ray_list,
            tile_list,
            counters,
            cost,
            args,
            args_bg,
            reproject_bg,
            prefilter_bg,
            resolve_bg,
        });
        self.clear_caches();
        self.reset_history();
    }
}

/// The dormant probe's bind group for `frame`'s HDR and depth views (cached by them).
fn probe_bind_group(
    cache: &mut BindCache<(wgpu::TextureView, wgpu::TextureView)>,
    device: &crate::device::TrackedDevice,
    pipes: &Pipes,
    frame: &SssrFrame<'_>,
) -> wgpu::BindGroup {
    cache.get_or_insert_with((frame.hdr.clone(), frame.depth.clone()), || {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sssr probe"),
            layout: &pipes.probe_bgl,
            entries: &[
                buffer(0, &pipes.params),
                view(1, frame.hdr),
                view(2, frame.depth),
                buffer(7, &pipes.probe_counters),
            ],
        })
    })
}

/// The uniform of one SSSR frame of size `(w, h)`.
fn sssr_params(
    frame: &SssrFrame<'_>,
    (w, h): (u32, u32),
    last_view_proj: Option<[[f32; 4]; 4]>,
    history_valid: bool,
) -> SssrParams {
    let mips = pyramid_mips(w, h);
    let bound = sssr_cost_bound();
    SssrParams {
        inv_view_proj: frame.inv_view_proj,
        view_proj: frame.view_proj,
        prev_view_proj: last_view_proj.unwrap_or(frame.view_proj),
        camera: [
            frame.eye[0],
            frame.eye[1],
            frame.eye[2],
            (frame.frame_index % 1024) as f32,
        ],
        screen: [w as f32, h as f32, 1.0 / w as f32, 1.0 / h as f32],
        proj: [
            frame.proj_terms[0],
            frame.proj_terms[1],
            frame.probe_count.min(4) as f32,
            frame.probe_scale,
        ],
        tuning: tuning(sssr_roughness_cutoff()),
        temporal: [
            TEMPORAL_STABILITY,
            VARIANCE_THRESHOLD,
            (mips - 1).min(MAX_MIP) as f32,
            if history_valid { 1.0 } else { 0.0 },
        ],
        forward: [frame.forward[0], frame.forward[1], frame.forward[2], frame.near],
        budget: budget_row(bound),
        bound: bound.map_or([0.0; 4], |b| b.row()),
        probes: frame.probes,
        captures: frame.captures,
    }
}

/// The uniform's `tuning` row for roughness cutoff `cutoff`: cutoff, fade band, thickness, steps.
fn tuning(cutoff: f32) -> [f32; 4] {
    [cutoff, SSSR_ROUGHNESS_BAND, SSSR_THICKNESS, SSSR_MAX_STEPS as f32]
}

/// The uniform's `budget` row: the half-resolution roughness and steps, and whether glossy pixels
/// are kept from buying variance-guided rays (the cost bound; the base row with `None`).
fn budget_row(bound: Option<SssrCostBound>) -> [f32; 4] {
    match bound {
        None => [SSSR_HALF_RES_ROUGHNESS, SSSR_HALF_RES_MAX_STEPS as f32, 0.0, 0.0],
        Some(b) => [
            b.half_res_roughness,
            SSSR_HALF_RES_MAX_STEPS as f32,
            if b.variance_rays { 0.0 } else { 1.0 },
            b.block_roughness,
        ],
    }
}

/// Levels of the SSSR pyramid: up to [`MAX_MIP`] + 1, fewer on a tiny frame.
fn pyramid_mips(width: u32, height: u32) -> u32 {
    (width.max(height).max(1).ilog2() + 1).min(MAX_MIP + 1)
}

fn module(
    device: &crate::device::TrackedDevice,
    source: &str,
    file: &str,
) -> Result<wgpu::ShaderModule, RenderError> {
    let naga = crate::shader_lib::compose_source(source, file, HashMap::new())?;
    Ok(device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(file),
        source: wgpu::ShaderSource::Naga(Cow::Owned(naga)),
    }))
}

fn compute(
    device: &crate::device::TrackedDevice,
    bgl: &wgpu::BindGroupLayout,
    module: &wgpu::ShaderModule,
    entry: &'static str,
) -> wgpu::ComputePipeline {
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(entry),
        bind_group_layouts: &[Some(bgl)],
        immediate_size: 0,
    });
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(entry),
        layout: Some(&layout),
        module,
        entry_point: Some(entry),
        compilation_options: Default::default(),
        cache: crate::pipeline_cache::cache_for(&device).as_deref(),
    })
}

fn bgl(
    device: &crate::device::TrackedDevice,
    label: &str,
    entries: &[wgpu::BindGroupLayoutEntry],
) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries,
    })
}

fn uniform_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn tex_entry(
    binding: u32,
    visibility: wgpu::ShaderStages,
    filterable: bool,
    view_dimension: wgpu::TextureViewDimension,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable },
            view_dimension,
            multisampled: false,
        },
        count: None,
    }
}

fn uint_tex_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Uint,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn storage_tex_entry(
    binding: u32,
    format: wgpu::TextureFormat,
    access: wgpu::StorageTextureAccess,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::StorageTexture {
            access,
            format,
            view_dimension: wgpu::TextureViewDimension::D2,
        },
        count: None,
    }
}

fn storage_buf_entry(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn buffer(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

fn view(binding: u32, view: &wgpu::TextureView) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: wgpu::BindingResource::TextureView(view),
    }
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "sssr_classify.wgsl,sssr_composite.wgsl,sssr_intersect.wgsl,sssr_prefilter.wgsl,sssr_probe.wgsl,sssr_reproject.wgsl,sssr_resolve.wgsl",
        "SssrParams",
        SssrParams {
            inv_view_proj,
            view_proj,
            prev_view_proj,
            camera,
            screen,
            proj,
            tuning,
            temporal,
            forward,
            budget,
            bound,
            probes,
            captures
        }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roughness_cutoff_default_is_unchanged_and_feeds_the_uniform() {
        // The default the v0.1.13 look was blessed with.
        assert_eq!(SSSR_ROUGHNESS_CUTOFF, 0.55);
        assert_eq!(tuning(SSSR_ROUGHNESS_CUTOFF), [0.55, 0.15, 0.03, 96.0]);
        // The knob's value is what the classify and trace shaders read (`tuning.x`).
        assert_eq!(tuning(0.7)[0], 0.7);
        assert_eq!(tuning(0.7)[1..], tuning(SSSR_ROUGHNESS_CUTOFF)[1..]);
        // Clamping and the environment parser.
        assert_eq!(clamp_cutoff(0.0), SSSR_ROUGHNESS_BAND);
        assert_eq!(clamp_cutoff(2.0), 1.0);
        assert_eq!(clamp_cutoff(f32::NAN), SSSR_ROUGHNESS_CUTOFF);
        assert_eq!(parse_cutoff(" 0.7 "), Some(0.7));
        assert_eq!(parse_cutoff("5"), Some(1.0));
        assert_eq!(parse_cutoff(""), None);
        assert_eq!(parse_cutoff("high"), None);
        assert_eq!(parse_cutoff("inf"), None);
    }

    /// Hermite smoothstep, the same curve `validate_hit` uses on the thickness slab.
    fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
        let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    }

    /// Hit confidence from `sssr_intersect.wgsl`: thickness is [`SSSR_THICKNESS`] times
    /// view depth floored at 1 m, and confidence is `(1 - smoothstep(0, thickness, distance))²`.
    fn hit_confidence(view_depth_m: f32, distance_m: f32) -> f32 {
        let thickness = SSSR_THICKNESS * view_depth_m.max(1.0);
        let confidence = 1.0 - smoothstep(0.0, thickness, distance_m);
        confidence * confidence
    }

    #[test]
    fn thickness_is_three_centimetres_per_metre_and_rejects_a_half_metre_gap() {
        assert_eq!(SSSR_THICKNESS, 0.03);
        assert_eq!(tuning(SSSR_ROUGHNESS_CUTOFF)[2], 0.03);
        // On the surface the confidence is 1. At the slab edge it is 0.
        assert!((hit_confidence(1.0, 0.0) - 1.0).abs() < 1.0e-6);
        assert_eq!(hit_confidence(1.0, 0.03), 0.0);
        // At 2 m the slab is 0.06 m, so a candidate 0.5 m behind the surface is dropped.
        assert_eq!(hit_confidence(2.0, 0.5), 0.0);
        assert!((hit_confidence(2.0, 0.0) - 1.0).abs() < 1.0e-6);
    }

    /// IEEE half bits → f32 (normal numbers only; enough for the codes).
    fn half_to_f32(bits: u16) -> f32 {
        let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
        let e = i32::from((bits >> 10) & 31);
        let m = f32::from(bits & 1023);
        sign * (1.0 + m / 1024.0) * 2f32.powi(e - 15)
    }

    #[test]
    fn every_surface_code_round_trips_and_is_negative() {
        for r in 0..32u32 {
            for w in 0..32u32 {
                for f in 0..16u32 {
                    let (pr, pw, pf) = (r as f32 / 31.0, (w as f32 / 31.0).powi(2), f as f32 / 15.0);
                    let bits = pack_surface_code(pr, pw, pf);
                    let value = half_to_f32(bits);
                    assert!(value < 0.0 && value.is_finite(), "code {r},{w},{f} → {value}");
                    let (dr, dw, df) = unpack_surface_code(bits).expect("a code");
                    assert!((dr - pr).abs() < 1e-6 && (dw - pw).abs() < 1e-5 && (df - pf).abs() < 1e-6);
                }
            }
        }
    }

    #[test]
    fn legacy_roughness_alpha_is_not_a_code() {
        // 0.35 (the old mirror floor), 0.7 and 1.0 (the sky) as halves.
        for bits in [0x3599u16, 0x399a, 0x3c00, 0x0000] {
            assert_eq!(unpack_surface_code(bits), None);
        }
    }

    #[test]
    fn every_sssr_shader_composes_and_validates() {
        let spd = include_str!("../shaders/sssr_spd.wgsl");
        let mut sources = vec![];
        for defs in [
            vec![],
            vec!["SPD_MIN"],
            vec!["SPD_FROM_DEPTH"],
            vec!["SPD_MIN", "SPD_FROM_DEPTH"],
        ] {
            sources.push(("sssr_spd.wgsl", spd, defs));
        }
        for (name, source) in [
            (
                "sssr_classify.wgsl",
                include_str!("../shaders/sssr_classify.wgsl"),
            ),
            (
                "sssr_intersect.wgsl",
                include_str!("../shaders/sssr_intersect.wgsl"),
            ),
            (
                "sssr_reproject.wgsl",
                include_str!("../shaders/sssr_reproject.wgsl"),
            ),
            (
                "sssr_prefilter.wgsl",
                include_str!("../shaders/sssr_prefilter.wgsl"),
            ),
            ("sssr_resolve.wgsl", include_str!("../shaders/sssr_resolve.wgsl")),
            ("sssr_probe.wgsl", include_str!("../shaders/sssr_probe.wgsl")),
            (
                "sssr_composite.wgsl",
                include_str!("../shaders/sssr_composite.wgsl"),
            ),
        ] {
            sources.push((name, source, vec![]));
        }
        // The cost probe's variants (ADR-2557), with and without blue noise.
        sources.push((
            "sssr_classify.wgsl",
            include_str!("../shaders/sssr_classify.wgsl"),
            vec!["SSSR_STATS"],
        ));
        for defs in [
            vec!["SSSR_STATS"],
            vec!["SSSR_STATS", "BLUE_NOISE"],
            vec!["BLUE_NOISE"],
        ] {
            sources.push((
                "sssr_intersect.wgsl",
                include_str!("../shaders/sssr_intersect.wgsl"),
                defs,
            ));
        }
        for (name, source, defs) in sources {
            let defs = defs
                .into_iter()
                .map(|d| (d.to_string(), naga_oil::compose::ShaderDefValue::Bool(true)))
                .collect();
            let module = match crate::shader_lib::compose_source(source, name, defs) {
                Ok(module) => module,
                Err(err) => panic!("{name}: {err}"),
            };
            if let Err(err) = naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::all(),
            )
            .validate(&module)
            {
                panic!("{name}: {err:?}");
            }
        }
    }

    #[test]
    fn params_match_the_shader_layout() {
        // 3 matrices, 8 vec4s (the cost bound's row since ADR-2557), 4 probes of 2 vec4s, and
        // 4 local capture boxes of 2 vec4s (ADR-2860).
        assert_eq!(size_of::<SssrParams>(), 3 * 64 + 8 * 16 + 4 * 32 + 4 * 32);
    }

    #[test]
    fn the_cost_bound_is_off_by_default_and_leaves_the_base_rows() {
        // Off: the uniform rows the base trace was blessed with (ADR-2557: default frames do not move).
        assert_eq!(
            budget_row(None),
            [SSSR_HALF_RES_ROUGHNESS, SSSR_HALF_RES_MAX_STEPS as f32, 0.0, 0.0]
        );
        let wet = SssrCostBound::WET;
        assert_eq!(budget_row(Some(wet)), [0.35, 48.0, 1.0, 3.5 / 31.0]);
        assert_eq!(wet.row(), [1.0, 16.0, 0.0, 0.0]);
        let keep_variance = SssrCostBound {
            variance_rays: true,
            ..wet
        };
        assert_eq!(budget_row(Some(keep_variance))[2], 0.0);
        // Environment names.
        assert_eq!(parse_cost_bound(" Wet "), Some(Some(wet)));
        assert_eq!(parse_cost_bound("1"), Some(Some(wet)));
        assert_eq!(parse_cost_bound("off"), Some(None));
        assert_eq!(parse_cost_bound("0"), Some(None));
        assert_eq!(parse_cost_bound(""), None);
        assert_eq!(parse_cost_bound("fast"), None);
    }

    #[test]
    fn a_sanitized_bound_is_in_range() {
        let wild = SssrCostBound {
            variance_rays: true,
            tile_ray_budget: 1000,
            min_steps: 1000,
            step_falloff_depth_m: f32::NAN,
            half_res_roughness: 7.0,
            block_roughness: f32::INFINITY,
        }
        .sanitized();
        assert_eq!(wild.block_roughness, 1.0, "non-finite: off");
        assert_eq!(wild.tile_ray_budget, 64);
        assert_eq!(wild.min_steps, SSSR_MAX_STEPS);
        assert_eq!(wild.step_falloff_depth_m, SssrCostBound::WET.step_falloff_depth_m);
        assert_eq!(wild.half_res_roughness, 1.0);
        assert!(wild.variance_rays);
        let negative = SssrCostBound {
            step_falloff_depth_m: -3.0,
            half_res_roughness: -1.0,
            ..SssrCostBound::WET
        }
        .sanitized();
        assert_eq!(negative.step_falloff_depth_m, 0.0);
        assert_eq!(negative.half_res_roughness, 0.0);
        assert_eq!(SssrCostBound::WET.sanitized(), SssrCostBound::WET);
        // The block-rate threshold sits between two 5-bit roughness codes, and 1 stays off.
        for raw in [0.0f32, 0.1, 0.29, 0.3, 0.3226, 0.5, 0.99] {
            let snapped = snap_between_codes(raw);
            let steps = snapped * 31.0;
            assert!((steps - steps.floor() - 0.5).abs() < 1e-4, "{raw} → {snapped}");
            assert!((snapped - raw).abs() <= 0.5 / 31.0 + 1e-6);
        }
        assert_eq!(snap_between_codes(1.0), 1.0);
        assert_eq!(snap_between_codes(3.0), 1.0);
        assert_eq!(snap_between_codes(-1.0), 0.5 / 31.0);
    }

    #[test]
    fn cost_words_decode_in_shader_order() {
        let words: [u32; COST_WORDS] = std::array::from_fn(|i| i as u32 + 1);
        let c = SssrCostCounters::from_words(&words);
        assert_eq!(
            (c.mirror_pixels, c.glossy_pixels, c.skipped_pixels, c.rays),
            (1, 2, 3, 4)
        );
        assert_eq!(
            (c.variance_rays, c.budget_dropped_rays, c.shared_pixels, c.steps),
            (5, 6, 7, 8)
        );
        assert_eq!(
            (c.exhausted_rays, c.half_res_rays, c.max_steps, c.hits),
            (9, 10, 11, 12)
        );
        assert_eq!(c.classified_pixels(), 6);
        assert_eq!(c.steps_per_ray(), 2.0);
        assert_eq!(SssrCostCounters::default().steps_per_ray(), 0.0);
        // The WGSL struct has one atomic per word.
        let common = include_str!("../shaders/sssr_common.wgsl");
        let body = common
            .split("struct SssrCost {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .unwrap_or_default();
        assert_eq!(body.matches("atomic<u32>").count(), COST_WORDS);
        assert_eq!(READBACK_BYTES, 8 + 4 * COST_WORDS as u64);
    }

    #[test]
    fn pyramid_stops_at_the_traversal_limit() {
        assert_eq!(pyramid_mips(2560, 1440), 7);
        assert_eq!(pyramid_mips(64, 36), 7);
        assert_eq!(pyramid_mips(16, 9), 5);
        assert_eq!(pyramid_mips(1, 1), 1);
    }

    /// Builds one stats readback slot whose counters say `tiles` glossy tiles on `frame`, mapped
    /// and `ST_READY` the way a finished map callback leaves it.
    fn ready_stats_slot(device: &crate::device::RenderDevice, frame: u64, tiles: u32) -> StatsSlot {
        let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sssr stats readback test"),
            size: READBACK_BYTES,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut data = [0u8; 8];
        data[..4].copy_from_slice(&10u32.to_le_bytes());
        data[4..].copy_from_slice(&tiles.to_le_bytes());
        device.queue.write_buffer(&buffer, 0, &data);
        let state = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(crate::readback::ST_INFLIGHT));
        buffer.slice(..).map_async(wgpu::MapMode::Read, {
            let state = std::sync::Arc::clone(&state);
            move |result| crate::readback::store_map_result(&state, result.is_ok())
        });
        device
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("stats buffer map");
        assert_eq!(
            state.load(std::sync::atomic::Ordering::Acquire),
            crate::readback::ST_READY
        );
        StatsSlot {
            buffer,
            state,
            frame,
            with_cost: false,
        }
    }

    /// The render6 determinism audit: the dormancy counter must end at the same value for the
    /// same counters whatever order the map callbacks left the slots ready. Here the slot order
    /// is frames 4, 5, 3 with the glossy tiles in the oldest frame; the index-order walk let the
    /// old glossy readback reset the two newer empty ones (counter 0 instead of 2), so one map
    /// schedule reached `DORMANT_AFTER` two frames before another.
    #[test]
    fn collect_stats_applies_readbacks_in_frame_order() {
        let device = crate::device::test_device();
        let mut pass = SssrPass::default();
        for (frame, tiles) in [(4u64, 0u32), (5, 0), (3, 7)] {
            pass.stats_slots.push(ready_stats_slot(&device, frame, tiles));
        }
        pass.collect_stats();
        assert_eq!(
            pass.idle_readbacks, 2,
            "frames 4 and 5 are empty after the glossy frame 3"
        );
        assert_eq!(
            pass.last_stats.map(|(frame, _)| frame),
            Some(5),
            "the newest counters are what `stats` reports"
        );
        assert!(
            pass.stats_slots
                .iter()
                .all(|slot| slot.state.load(std::sync::atomic::Ordering::Acquire)
                    == crate::readback::ST_IDLE),
            "every consumed slot is idle again for reuse"
        );
        // The same counters as a one-slot-per-pump schedule land on the same counter.
        let mut pump = SssrPass::default();
        for (frame, tiles) in [(3u64, 7u32), (4, 0), (5, 0)] {
            pump.stats_slots.push(ready_stats_slot(&device, frame, tiles));
            pump.collect_stats();
            pump.stats_slots.clear();
        }
        assert_eq!(pump.idle_readbacks, 2, "one pump per readback agrees");
        // An older empty readback that maps late, after a newer glossy one, adds nothing.
        // Two separate collects: in one collect the ascending sort alone would apply the
        // frames in order and the stale-readback guard would never run (Muse's read-only
        // review of fix-render6, finding 4).
        let mut late = SssrPass::default();
        late.stats_slots.push(ready_stats_slot(&device, 6, 7));
        late.collect_stats();
        assert_eq!(late.idle_readbacks, 0, "the glossy readback reset the counter");
        late.stats_slots.clear();
        late.stats_slots.push(ready_stats_slot(&device, 4, 0));
        late.collect_stats();
        assert_eq!(late.idle_readbacks, 0, "the stale empty readback is ignored");
    }
}
