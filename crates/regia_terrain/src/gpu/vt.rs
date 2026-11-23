// SPDX-License-Identifier: MPL-2.0
//! Terrain virtual texture (ADR-117 "Terrain virtual texture").
//!
//! The photoreal surface (`surface_photoreal` in `terrain.wgsl`: layer blend, strata, relief,
//! cavity, horizon sky and bounce, macro tint) cached in pages of one virtual texture over the
//! map's XZ, so most terrain pixels read eight filtered texels instead of shading it. View-
//! dependent terms (sun and sky lighting, shadows, glints, fog) stay per pixel.
//!
//! - **Pages.** LOD `l` has texels of `lod0_texel_m · 2^l` and `(side0 >> l)²` pages of
//!   `page_texels²` texels plus a one-texel border (the neighbours' edge texels, baked from
//!   the same XZ, so bilinear filtering is seamless across page edges). Page tables are one
//!   storage buffer (slot + 1, 0 = not resident); the atlas is four plain 2-D textures (no
//!   sparse resources), plus a fifth in the whole-surface mode (the horizon slopes).
//! - **Bake.** `cs_vt_bake` runs `surface_photoreal` per texel on what the colour pass would
//!   interpolate for a pixel there (the camera's CDLOD triangle under it, from a node map of
//!   the camera selection), with the footprint of a pixel that picks that LOD and the
//!   camera's distance. A page baked from the current camera is the pixel's own surface, up
//!   to footprint and filtering. In the whole-surface mode `cs_vt_bake_horizon` also stores
//!   the horizon's per-sector slopes (map E), from which the colour pass finishes the
//!   planet-shine fill's and the dusty sky's horizon terms per pixel.
//! - **Feedback.** The colour pass records the LOD each sampled pixel wanted on its LOD-0
//!   page; `vt.wgsl` reduces that to a request pyramid and an allocation list (after Wicked
//!   Engine): appended in arrival order while the count fits, else reduced by a hashed
//!   `atomicMax` so an over-capacity burst keeps the same set on every GPU and driver
//!   ([`VtPageManager`] reads it back two frames later).
//! - **Misses never show.** A pixel whose two pages are not both resident shades the surface
//!   in full. Pages are re-baked when the camera has moved enough to change their distance-
//!   dependent terms ([`DistTerms`]), so a held view converges to exact pages.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use regia_app::prelude::{Resource, bevy_ecs};
use serde::{Deserialize, Serialize};

use crate::TerrainError;

/// Most LODs the page tables hold (four `vec4<u32>` offsets in the uniform).
pub const VT_MAX_LODS: u32 = 16;

/// How far a height change reaches into a page's baked terms, metres: the sun-occlusion march
/// (`self_shadow_reach_m`, at most 400 m) and the horizon taps (up to 270 m), plus slack for the
/// cavity and slope taps. [`VtPageManager::invalidate_rect`] callers grow the edited rectangle by
/// this much.
pub const HEIGHT_REACH_M: f32 = 450.0;
/// Workgroups per row of the allocation dispatch (`vt.wgsl`).
const ALLOC_ROW_GROUPS: u32 = 32768;
/// Header words of the allocation list (`vt.wgsl`): count, missed, hit, unused.
const ALLOC_HEADER: u32 = 4;
/// Bit `cs_allocate` OR-s into every stored allocation-list word (`vt.wgsl`). A zero word
/// (no page hashed to that overflow slot this frame) is dropped by `parse_alloc_words`.
/// Keys keep to bits 0..29 (`lod < 16`), so the marker survives the round trip.
const ALLOC_VALID: u32 = 1 << 31;
/// The position bits of a packed page key (`y << 13 | x`); the LOD sits above them.
const ALLOC_POS_MASK: u32 = (1 << 26) - 1;
/// The odd multipliers of the 26-bit position permutation (`alloc_word`; `vt.wgsl`'s
/// `perm26`). Odd, so each multiply is a bijection modulo 2^26.
const PERM26_MUL: [u32; 2] = [0x2c1b_3c6d, 0x297a_2d39];
/// The multiplicative inverses of [`PERM26_MUL`] modulo 2^32 (and so modulo 2^26).
const PERM26_INV: [u32; 2] = [odd_inverse(PERM26_MUL[0]), odd_inverse(PERM26_MUL[1])];

/// The inverse of odd `a` modulo 2^32 by Newton's iteration (`x ← x·(2 − a·x)` doubles the
/// correct low bits; `a` itself is correct to 3 bits, since `a² ≡ 1 mod 8` for odd `a`).
const fn odd_inverse(a: u32) -> u32 {
    let mut x = a;
    let mut i = 0;
    while i < 5 {
        x = x.wrapping_mul(2u32.wrapping_sub(a.wrapping_mul(x)));
        i += 1;
    }
    x
}

/// A bijection of the 26 position bits (`y << 13 | x`): odd multiply, xorshift, odd multiply,
/// xorshift, all modulo 2^26. Twin of `perm26` in `vt.wgsl` (the CPU only decodes, so this
/// encoder is the tests' mirror). A 13-bit xorshift of a 26-bit value is its own inverse.
#[cfg(test)]
fn perm26(v: u32) -> u32 {
    let mut v = v.wrapping_mul(PERM26_MUL[0]) & ALLOC_POS_MASK;
    v ^= v >> 13;
    v = v.wrapping_mul(PERM26_MUL[1]) & ALLOC_POS_MASK;
    v ^ (v >> 13)
}

/// The inverse of `perm26`.
fn unperm26(v: u32) -> u32 {
    let mut v = v ^ (v >> 13);
    v = v.wrapping_mul(PERM26_INV[1]) & ALLOC_POS_MASK;
    v ^= v >> 13;
    v.wrapping_mul(PERM26_INV[0]) & ALLOC_POS_MASK
}

/// The allocation-list word `cs_allocate` stores for packed page `key`:
/// `ALLOC_VALID | lod << 26 | perm26(y << 13 | x)`. The overflow region keeps each slot's
/// maximum word, so the LOD still decides first (a coarser page wins), and within a LOD the
/// winner is decided by the permuted position: no part of the window is preferred. Comparing
/// the raw key instead kept the largest y, then x, so a burst over capacity granted pages
/// only to the window's last rows (rev-kimi-19, fixrev-22-k finding 3). The WGSL twin is in
/// `cs_allocate`; this mirror serves the tests.
#[cfg(test)]
fn alloc_word(key: u32) -> u32 {
    ALLOC_VALID | (key & !ALLOC_POS_MASK) | perm26(key & ALLOC_POS_MASK)
}

/// The packed page key of a stored allocation word (`alloc_word` inverted, marker stripped).
fn alloc_key(word: u32) -> u32 {
    (word & !ALLOC_VALID & !ALLOC_POS_MASK) | unperm26(word & ALLOC_POS_MASK)
}
/// Readback slots (two frames in flight plus the one being filled).
const READBACK_SLOTS: usize = 3;
/// Frames between a slot's submit and its read.
const READBACK_LATENCY: u64 = 2;
const ST_IDLE: u8 = 0;
const ST_INFLIGHT: u8 = 1;
const ST_READY: u8 = 2;
const ST_FAILED: u8 = 3;
/// Timestamps per frame: bake start/end, reduce start/end.
const STAMPS: u32 = 4;

/// Settings of the terrain virtual texture ([`crate::TerrainSharedDraw::set_virtual_texture`]).
///
/// Opt-in (ADR-041): frames with the texture match frames without it to the footprint and
/// filtering error of the texels, and it pays off only where the surface is the colour pass's
/// main cost (photoreal material, Desktop High).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VirtualTextureSettings {
    /// Texel size of LOD 0, metres. Pixels whose footprint is finer shade in full.
    pub lod0_texel_m: f32,
    /// Payload texels per page side (the atlas slot adds a one-texel border).
    pub page_texels: u32,
    /// Atlas slots per side (`slots²` resident pages). The atlas is `slots · (page_texels + 2)`
    /// texels per side, 32 bytes per texel (40 in the whole-surface mode, with map E).
    pub atlas_slots: u32,
    /// Texels per pixel footprint (1 = a texel per pixel; 2 = four texels per pixel, closer to
    /// the per-pixel surface and four times the bake and atlas).
    pub oversample: f32,
    /// Pixels nearer than this (metres) shade in full and request nothing.
    ///
    /// The whole-surface cache ([`VtCache::Surface`]) never serves nearer than 24 m
    /// (`max(near_m, 24)` in the colour shader): the near-field detail has faded there and
    /// the atlas does not store frost. Lighting and relief use this value unchanged, so a
    /// setting under 24 does serve those nearer pixels. [`Self::validate`] accepts any
    /// finite `near_m >= 0`.
    pub near_m: f32,
    /// Pixels on triangles steeper than this |n.y| shade in full (a top-down texel stretches
    /// down a cliff by 1 / |n.y|).
    pub min_normal_y: f32,
    /// New pages baked per frame at most.
    pub bakes_per_frame: u32,
    /// Stale pages re-baked per frame at most (after the new ones).
    pub rebakes_per_frame: u32,
    /// Staleness score at which a page is re-baked ([`DistTerms::score`]; 1 ≈ a 2 % change of
    /// a distance fade, half a texel of a distance cut-off line, or 1 % of the distance).
    pub stale_threshold: f32,
    /// What the texture caches ([`VtCache`]).
    pub cache: VtCache,
    /// Read one of the two LODs per pixel, picked with the trilinear weight (interleaved
    /// gradient noise, new every frame), instead of both: half the atlas reads. Needs the
    /// temporal filter (TAA) to average the two; off for frames without it.
    pub stochastic_lod: bool,
    /// Also run on a device without forced early depth tests ([`VtDepthTest::Late`]: DX12 and
    /// Metal under wgpu 30, which do not offer `Features::SHADER_EARLY_DEPTH_TEST`). The colour
    /// pass then tests depth after the shader, so fragments the depth prepass already hides
    /// still run it and record feedback: the frame is the same, the cost is higher and more
    /// pages are requested (ADR-2122; not measured on a DX12 or Metal driver yet). Off: such
    /// a device keeps the setting and shades the terrain in full, as before. Ignored where
    /// forced early depth tests exist.
    pub late_depth_fallback: bool,
}

/// How the terrain colour pass with the virtual texture tests depth on a device (ADR-2122).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VtDepthTest {
    /// Forced early depth tests (`@early_depth_test(force)`): only visible fragments run the
    /// shader and write feedback. Vulkan and GLES 3.1+.
    Early,
    /// No forced early depth tests (DX12, Metal): the texture runs only with
    /// [`VirtualTextureSettings::late_depth_fallback`], and the feedback writes move the depth
    /// test after the shader.
    Late,
    /// Fewer than five bind groups, or no room in the fragment stage for the texture's page
    /// table, feedback and parameters next to the terrain's own bindings.
    Unsupported,
}

impl VtDepthTest {
    /// What a device with `limits` and `features` offers. Late needs the fragment stage to hold
    /// the texture's two storage buffers and one uniform on top of what the shared colour pass
    /// binds without it (with or without the engine's local lights, as
    /// `crate::local_lights_fit` decided for this device), so its pipeline always fits.
    #[must_use]
    pub fn of_device(limits: &wgpu::Limits, features: wgpu::Features) -> Self {
        if limits.max_bind_groups < regia_render::TERRAIN_SHARED_BIND_GROUPS {
            return Self::Unsupported;
        }
        if features.contains(wgpu::Features::SHADER_EARLY_DEPTH_TEST) {
            return Self::Early;
        }
        let lights = crate::gpu::local_lights_fit(limits, features);
        let (light_storage, light_uniform) = if lights {
            (
                regia_render::SHARED_LIGHTS_STORAGE_BUFFERS,
                regia_render::SHARED_LIGHTS_UNIFORM_BUFFERS,
            )
        } else {
            (0, 0)
        };
        if limits.max_storage_buffers_per_shader_stage >= 4 + light_storage
            && limits.max_uniform_buffers_per_shader_stage >= 11 + light_uniform
        {
            Self::Late
        } else {
            Self::Unsupported
        }
    }

    /// Whether `settings` run on a device with this depth test.
    #[must_use]
    pub fn runs(self, settings: &VirtualTextureSettings) -> bool {
        match self {
            Self::Early => true,
            Self::Late => settings.late_depth_fallback,
            Self::Unsupported => false,
        }
    }

    /// Whether this device runs the texture for `settings`.
    ///
    /// `Early` runs any settings, including when none are applied yet. `Late` runs only
    /// when `settings` carry [`VirtualTextureSettings::late_depth_fallback`]. `Unsupported`
    /// never runs. [`crate::gpu::TerrainGpu::vt_supported`] and
    /// [`TerrainVtHandle::is_supported`] both use this rule (ADR-2122).
    #[must_use]
    pub fn supports(self, settings: Option<VirtualTextureSettings>) -> bool {
        match self {
            Self::Early => true,
            Self::Late => settings.is_some_and(|s| s.late_depth_fallback),
            Self::Unsupported => false,
        }
    }

    /// `"early"`, `"late"` or `"unsupported"` (the agent's `terrain.vt` `depth_test`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Early => "early",
            Self::Late => "late",
            Self::Unsupported => "unsupported",
        }
    }
}

/// What the terrain virtual texture caches; everything else the colour pass still shades
/// per pixel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VtCache {
    /// The lighting terms read from the heightfield: cliff-relief normal gradient,
    /// heightfield sun occlusion, cavity, the horizon's per-sector slopes (the sky term is
    /// finished per pixel with the pixel's normal) and the bounce. Closest to the uncached
    /// frame, the smallest saving.
    Lighting,
    /// The lighting terms plus the strata and talus albedo factor and the fractured-rock
    /// normal: everything but the layer textures and the mid-range relief, which carry the
    /// pixel-scale detail.
    Relief,
    /// The whole surface (the default): the largest saving; the layer textures' pixel-scale
    /// detail is reconstructed from texels (slightly softer).
    #[default]
    Surface,
}

impl VtCache {
    /// The shader's `atlas.w`.
    fn code(self) -> f32 {
        match self {
            Self::Surface => 0.0,
            Self::Lighting => 1.0,
            Self::Relief => 2.0,
        }
    }
}

impl Default for VirtualTextureSettings {
    fn default() -> Self {
        Self {
            lod0_texel_m: 0.02,
            page_texels: 126,
            atlas_slots: 32,
            oversample: 1.0,
            near_m: 24.0,
            min_normal_y: 0.25,
            bakes_per_frame: 48,
            rebakes_per_frame: 4,
            stale_threshold: 1.0,
            cache: VtCache::Surface,
            stochastic_lod: true,
            late_depth_fallback: false,
        }
    }
}

impl VirtualTextureSettings {
    /// What a render profile's `TerrainBudget::virtual_texture` turns on (ADR-157 addendum v0.1.2): the
    /// defaults with both LODs read per pixel, so frames without TAA (Desktop Low's FXAA) show
    /// no stochastic-LOD noise. [`Self::late_depth_fallback`] stays off: the profile's
    /// `TerrainBudget::vt_late_depth` turns it on (ADR-2122,
    /// `TerrainGpu::set_virtual_texture_from_budget`).
    #[must_use]
    pub fn for_profile() -> Self {
        Self {
            stochastic_lod: false,
            ..Self::default()
        }
    }

    /// Smaller atlas and fewer bakes per frame for the Deck / mobile tiers (3072² atlas,
    /// 377 MB with map E; the 2048² one overflowed on DO27's wide plates at the Deck's render size).
    #[must_use]
    pub fn compact() -> Self {
        Self {
            atlas_slots: 24,
            bakes_per_frame: 24,
            rebakes_per_frame: 2,
            ..Self::default()
        }
    }

    /// Atlas bytes: four RGBA16F maps (32 bytes per texel), plus the horizon slopes' map E
    /// in the whole-surface mode (40 bytes per texel).
    #[must_use]
    pub fn atlas_bytes(&self) -> u64 {
        let side = u64::from(self.atlas_slots) * u64::from(self.page_texels + 2);
        let per_texel = if self.cache == VtCache::Surface { 40 } else { 32 };
        side * side * per_texel
    }

    /// Checks every field's range.
    ///
    /// # Errors
    ///
    /// [`TerrainError::VirtualTexture`] naming the first field out of range.
    pub fn validate(&self) -> Result<(), TerrainError> {
        let bad = |what: &str| Err(TerrainError::VirtualTexture(what.to_owned()));
        if !(self.lod0_texel_m.is_finite() && self.lod0_texel_m > 0.0) {
            return bad("lod0_texel_m must be finite and > 0");
        }
        if !(8..=1022).contains(&self.page_texels) {
            return bad("page_texels must be 8..=1022");
        }
        if !(1..=64).contains(&self.atlas_slots) {
            return bad("atlas_slots must be 1..=64");
        }
        if !(self.oversample.is_finite() && (0.25..=4.0).contains(&self.oversample)) {
            return bad("oversample must be 0.25..=4");
        }
        if !(self.near_m.is_finite() && self.near_m >= 0.0) {
            return bad("near_m must be finite and >= 0");
        }
        if !(0.0..=1.0).contains(&self.min_normal_y) {
            return bad("min_normal_y must be 0..=1");
        }
        if self.bakes_per_frame == 0 || self.bakes_per_frame > 1024 || self.rebakes_per_frame > 1024 {
            return bad("bakes_per_frame must be 1..=1024 and rebakes_per_frame <= 1024");
        }
        if !(self.stale_threshold.is_finite() && self.stale_threshold > 0.0) {
            return bad("stale_threshold must be finite and > 0");
        }
        Ok(())
    }
}

/// Counters of the terrain virtual texture, last frame ([`crate::TerrainSharedDraw::vt_stats`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct VtStats {
    /// Resident pages.
    pub resident: u32,
    /// Atlas slots.
    pub capacity: u32,
    /// New pages baked this frame.
    pub baked: u32,
    /// Stale pages re-baked this frame.
    pub rebaked: u32,
    /// Pages the last readback requested.
    pub requested: u32,
    /// Requested pages not resident after this frame's bakes (they shade in full meanwhile).
    pub missing: u32,
    /// Frames the oldest still-missing request has waited (0 = everything resident).
    pub oldest_miss_frames: u32,
    /// Sampled pixels (one in 1024) the texture served, of those it was asked for, in the last
    /// readback: hits / (hits + misses); 1 when nothing was asked.
    pub hit_rate: f32,
    /// GPU time of the page bakes, ms (0 without timestamps).
    pub bake_ms: f32,
    /// GPU time of the feedback reduction, ms (0 without timestamps).
    pub reduce_ms: f32,
    /// The whole cache was dropped this frame (sun, material, heights or settings changed).
    pub flushed: bool,
}

/// Page grid of the virtual texture over a map `world_width` metres wide.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VtLayout {
    /// LOD-0 pages per side (a power of two).
    pub side0: u32,
    /// LOD count (`log2(side0) + 1`, at most [`VT_MAX_LODS`]).
    pub lods: u32,
    /// Page-table offset of each LOD.
    pub offsets: [u32; VT_MAX_LODS as usize],
    /// Pages over all LODs.
    pub total: u32,
    /// LOD-0 texel, metres.
    pub texel0_m: f32,
    /// Payload texels per page side.
    pub page_texels: u32,
    /// The map-extent lane: the texture's origin is `(-half_width, -half_width)`. Half the map
    /// width, less the map's grid origin ([`Self::with_grid_origin`], ADR-8179).
    pub half_width: f32,
}

impl VtLayout {
    /// The layout for `world_width` with `settings`: the smallest power-of-two page count
    /// whose LOD-0 pages cover the map.
    ///
    /// # Errors
    ///
    /// [`TerrainError::VirtualTexture`] when the map would need more than 2048 LOD-0 pages a
    /// side (raise `lod0_texel_m`).
    pub fn new(world_width: f32, settings: &VirtualTextureSettings) -> Result<Self, TerrainError> {
        let page_m = settings.lod0_texel_m * settings.page_texels as f32;
        // Check the page count while it is still a float: the cast to `u32` saturates and
        // `next_power_of_two` panics past 2^31 (audit finding 4).
        let pages = (world_width / page_m).ceil();
        if !pages.is_finite() || pages > 2048.0 {
            return Err(TerrainError::VirtualTexture(format!(
                "{pages} LOD-0 pages per side over {world_width} m: raise lod0_texel_m"
            )));
        }
        let side0 = (pages.max(1.0) as u32).next_power_of_two();
        let lods = (side0.trailing_zeros() + 1).min(VT_MAX_LODS);
        let mut offsets = [0u32; VT_MAX_LODS as usize];
        let mut total = 0u32;
        for (l, offset) in offsets.iter_mut().enumerate().take(lods as usize) {
            *offset = total;
            let side = (side0 >> l).max(1);
            total += side * side;
        }
        Ok(Self {
            side0,
            lods,
            offsets,
            total,
            texel0_m: settings.lod0_texel_m,
            page_texels: settings.page_texels,
            half_width: world_width * 0.5,
        })
    }

    /// The layout over a map centred on world XZ `origin` ([`crate::TerrainMap::grid_origin`],
    /// equal on X and Z): the texture's origin moves with the map (ADR-8179).
    #[must_use]
    pub fn with_grid_origin(mut self, origin: [f32; 2]) -> Self {
        self.half_width -= origin[0];
        self
    }

    /// Pages per side at `lod`.
    #[must_use]
    pub fn side(&self, lod: u32) -> u32 {
        (self.side0 >> lod).max(1)
    }

    /// Texel size at `lod`, metres.
    #[must_use]
    pub fn texel_m(&self, lod: u32) -> f32 {
        self.texel0_m * (1u32 << lod) as f32
    }

    /// Page-table index of page `(x, y)` at `lod`.
    #[must_use]
    pub fn index(&self, lod: u32, x: u32, y: u32) -> u32 {
        self.offsets[lod as usize] + y * self.side(lod) + x
    }

    /// World rectangle of a page: `(min_x, min_z, size)` metres.
    #[must_use]
    pub fn page_rect(&self, lod: u32, x: u32, y: u32) -> (f32, f32, f32) {
        let size = self.texel_m(lod) * self.page_texels as f32;
        (
            -self.half_width + x as f32 * size,
            -self.half_width + y as f32 * size,
            size,
        )
    }

    /// World XZ of texel `(i, j)` of a page's slot (border included: `i = 0` is the border).
    #[must_use]
    pub fn texel_xz(&self, lod: u32, x: u32, y: u32, i: u32, j: u32) -> [f32; 2] {
        let (x0, z0, _) = self.page_rect(lod, x, y);
        let t = self.texel_m(lod);
        [x0 + (i as f32 - 1.0 + 0.5) * t, z0 + (j as f32 - 1.0 + 0.5) * t]
    }
}

/// IEEE 754 binary16 to `f32` (the atlas readback).
#[must_use]
pub fn f16_to_f32(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from((bits >> 10) & 0x1f);
    let mant = f32::from(bits & 0x3ff);
    match exp {
        0 => sign * mant * 2f32.powi(-24),
        31 if mant == 0.0 => sign * f32::INFINITY,
        31 => f32::NAN,
        _ => sign * (1.0 + mant / 1024.0) * 2f32.powi(exp - 15),
    }
}

/// Packs a page key as the allocation list does: `lod << 26 | y << 13 | x`.
#[must_use]
pub fn pack_page(lod: u32, x: u32, y: u32) -> u32 {
    (lod << 26) | (y << 13) | x
}

/// Unpacks [`pack_page`]: `(lod, x, y)`.
#[must_use]
pub fn unpack_page(key: u32) -> (u32, u32, u32) {
    (key >> 26, key & 0x1fff, (key >> 13) & 0x1fff)
}

/// CPU stand-in of `cs_allocate` (`vt.wgsl`): the pages the allocation list hands back for
/// one frame's `requested` packed keys. While the count fits `capacity` the list is the
/// complete set (arrival order, which the CPU sorts); over capacity it is the hashed
/// overflow region — every page atomicMax-es its stored word (`alloc_word`) at
/// `hash_overflow_slot(key)`, so the surviving set is a pure function of the request set (the
/// maximum wins whatever order the writes land in, a coarser page's word compares larger
/// than a finer one's, and within a LOD the permuted position decides), never a function of
/// GPU workgroup scheduling.
#[cfg(test)]
fn allocate_pages(layout: &VtLayout, requested: &[u32], capacity: u32) -> Vec<u32> {
    let valid = |key: u32| {
        let (lod, x, y) = unpack_page(key);
        lod < layout.lods && x < layout.side(lod) && y < layout.side(lod)
    };
    if requested.iter().copied().filter(|&k| valid(k)).count() as u32 <= capacity {
        let mut keys: Vec<u32> = requested.iter().copied().filter(|&k| valid(k)).collect();
        keys.sort_unstable();
        return keys;
    }
    let mut overflow = vec![0u32; capacity as usize];
    for &key in requested {
        if !valid(key) {
            continue;
        }
        let slot = hash_overflow_slot(key | ALLOC_VALID, capacity) as usize;
        overflow[slot] = overflow[slot].max(alloc_word(key));
    }
    let mut keys: Vec<u32> = overflow
        .into_iter()
        .filter(|&word| word & ALLOC_VALID != 0)
        .map(alloc_key)
        .collect();
    keys.sort_unstable();
    keys
}

/// The overflow slot a page hashes to: an avalanche finalizer (C. Wellons' lowbias32,
/// xor-multiply scramble, public-domain sources, see `docs/provenance.md`) of the *packed
/// key* — LOD included — so pages that share a `local` on different LODs, or sit a multiple
/// of a power-of-two capacity apart within one, do not systematically collide. `atomicMax`
/// on the stored word (`alloc_word`) still resolves the remaining collisions, coarse first. Must match the WGSL
/// twin `hash_overflow_slot` in `vt.wgsl`; `wgsl_mirror_uses_the_same_overflow_hash` binds
/// the two.
#[cfg(test)]
fn hash_overflow_slot(key: u32, capacity: u32) -> u32 {
    let mut h = key;
    h ^= h >> 16;
    h = h.wrapping_mul(0x7feb_352d);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846c_a68b);
    h ^= h >> 16;
    h % capacity
}

/// Parses the allocation list the GPU read back (`vt.wgsl`'s `allocation` buffer): the
/// surviving page keys (marker stripped, holes dropped) and the miss/hit counters. `words`
/// is the whole read-back buffer; `capacity` the per-region word count. While the request
/// count fits, the arrival region holds every page (lossless); over capacity only the
/// hashed overflow region is complete — its surviving set is a pure function of the
/// request set, not of workgroup scheduling. Words without the valid marker were never
/// stored this frame; every stored word is decoded back to its page key (`alloc_key`).
fn parse_alloc_words(words: &[u32], capacity: u32) -> (Vec<u32>, u32, u32) {
    let start = ALLOC_HEADER as usize;
    let capacity = capacity as usize;
    let count = words.first().copied().unwrap_or(0);
    let region: &[u32] = if count as usize <= capacity {
        words.get(start..start + count as usize).unwrap_or(&[])
    } else {
        words.get(start + capacity..start + 2 * capacity).unwrap_or(&[])
    };
    let keys = region
        .iter()
        .filter(|&&word| word & ALLOC_VALID != 0)
        .map(|&word| alloc_key(word))
        .collect();
    let misses = words.get(1).copied().unwrap_or(0);
    let hits = words.get(2).copied().unwrap_or(0);
    (keys, misses, hits)
}

/// The surface's dependence on camera distance (`surface_photoreal`'s `dist` terms), for
/// deciding when a resident page has gone stale as the camera moves.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DistTerms {
    /// Smooth fades over `[start, end]` metres with a weight.
    pub bands: Vec<(f32, f32, f32)>,
    /// Hard cut-offs at a distance (a term switches off beyond it) with a weight.
    pub cuts: Vec<(f32, f32)>,
    /// Weight of the slow terms (CDLOD morph, the relief march's reach) per relative change
    /// of the distance.
    pub base: f32,
}

impl DistTerms {
    /// The terms of the shader for the given material uniform rows and budget flags:
    /// `params1` (detail fade), `relief` (march reach), `rock` (mid relief), the terrain
    /// budget `mip_info` (flags, far-layer and vertex-normal distances) and whether the
    /// shared path's horizon term is on.
    #[must_use]
    pub fn from_shader(
        params1: [f32; 4],
        relief: [f32; 4],
        rock: [f32; 4],
        mip_info: [f32; 4],
        horizon: bool,
    ) -> Self {
        let mut bands = Vec::new();
        let mut cuts = Vec::new();
        // Near-field detail and the mid relief's fade-in (both done by 24 m).
        if relief[3] > 0.0 {
            bands.push((6.0, 24.0, 1.0));
        }
        if rock[1] > 0.0 {
            bands.push((10.0, 24.0, 1.0));
            bands.push((180.0, 250.0, 1.0));
        }
        if params1[1] > params1[0] {
            bands.push((params1[0], params1[1], 0.5));
        }
        let flags = mip_info[1] as u32;
        if flags & 2 != 0 {
            bands.push((mip_info[2], mip_info[2] + 20.0, 1.0));
        }
        if flags & 4 != 0 {
            bands.push((mip_info[3], mip_info[3] + 20.0, 1.0));
        }
        if relief[0] > 0.0 {
            cuts.push((300.0, 1.0));
        }
        if horizon {
            cuts.push((2000.0, 1.0));
        }
        Self {
            bands,
            cuts,
            base: 0.3,
        }
    }

    /// Staleness of a page whose texels lie `d_min..=d_max` metres from the camera now, baked
    /// from a camera `moved` metres away (each texel's distance changed by at most `moved`),
    /// with texels of `texel_m`. 0 when the camera has not moved.
    #[must_use]
    pub fn score(&self, d_min: f32, d_max: f32, moved: f32, texel_m: f32) -> f32 {
        if moved <= 0.0 {
            return 0.0;
        }
        let lo = (d_min - moved).max(0.0);
        let hi = d_max + moved;
        let mut score = self.base * moved / d_min.max(1.0) / 0.01;
        for &(a, b, w) in &self.bands {
            if hi >= a && lo <= b {
                // smoothstep's steepest slope is 1.5 / (b − a); 2 % of the fade counts 1.
                score += w * moved * 1.5 / (b - a).max(1e-3) / 0.02;
            }
        }
        for &(c, w) in &self.cuts {
            if hi >= c && lo <= c {
                score += w * moved / (0.5 * texel_m.max(1e-4));
            }
        }
        score
    }
}

/// One resident page.
#[derive(Clone, Copy, Debug)]
struct Resident {
    slot: u32,
    /// Frame of the last request that named it.
    seen: u64,
    /// Camera the page was baked from.
    eye: [f32; 3],
}

/// This frame's work for the GPU: bake jobs (`[lod, x, y, slot]`) and page-table writes
/// (`[index, value]`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VtPlan {
    /// Pages to bake, `[lod, x, y, slot]`.
    pub jobs: Vec<[u32; 4]>,
    /// Page-table writes, `[index, slot + 1 or 0]`.
    pub table: Vec<[u32; 2]>,
    /// New pages among `jobs` (the rest are re-bakes).
    pub new_pages: u32,
}

/// Distance range from `eye` to a page's texels. Split out so a staleness walk can score
/// residents without borrowing the whole manager.
fn page_rect_distance(layout: &VtLayout, key: u32, eye: [f32; 3], altitude: [f32; 2]) -> (f32, f32) {
    let (lod, x, y) = unpack_page(key);
    let (x0, z0, size) = layout.page_rect(lod, x, y);
    let dx = (x0 - eye[0]).max(eye[0] - (x0 + size)).max(0.0);
    let dz = (z0 - eye[2]).max(eye[2] - (z0 + size)).max(0.0);
    let fx = (eye[0] - x0).abs().max((eye[0] - x0 - size).abs());
    let fz = (eye[2] - z0).abs().max((eye[2] - z0 - size).abs());
    let dy_min = (altitude[0] - eye[1]).max(eye[1] - altitude[1]).max(0.0);
    let dy_max = (eye[1] - altitude[0]).abs().max((eye[1] - altitude[1]).abs());
    (
        (dx * dx + dz * dz + dy_min * dy_min).sqrt(),
        (fx * fx + fz * fz + dy_max * dy_max).sqrt(),
    )
}

/// The CPU side of the cache: which page lives in which atlas slot, what to bake next, what
/// to evict. Pure (no GPU), so the policy is unit-tested.
#[derive(Debug)]
pub struct VtPageManager {
    layout: VtLayout,
    settings: VirtualTextureSettings,
    resident: HashMap<u32, Resident>,
    free: Vec<u32>,
    /// The latest readback's requests and the frame they were recorded on.
    requests: Vec<u32>,
    request_frame: u64,
    /// When each still-missing page was first requested (for the pop-in counter).
    missing_since: HashMap<u32, u64>,
    /// Resident pages whose heights changed under them ([`Self::invalidate_rect`]): re-baked in
    /// place (same slot, the page table untouched) the next frames, coarse first.
    dirty: HashSet<u32>,
    stats: VtStats,
    /// Bumps when a page is inserted, removed or the cache is flushed.
    resident_epoch: u64,
    /// Epoch last handed to the snapshot publisher.
    published_epoch: u64,
    /// Reused by [`Self::plan`] so a settled frame does not allocate the work lists.
    scratch_missing: Vec<u32>,
    scratch_stale: Vec<(f32, u32)>,
    scratch_still: Vec<u32>,
    scratch_evict: Vec<(u64, u32)>,
    scratch_dirty: Vec<u32>,
    scratch_queued: HashSet<u32>,
    /// Eye of the last staleness walk. Compared with [`PartialEq`] (NaN never matches).
    scored_eye: Option<[f32; 3]>,
    /// The last walk queued no rebake, so the same eye can skip the resident walk.
    score_idle: bool,
    /// Resident pages the staleness walk has visited.
    #[cfg(test)]
    resident_walks: u64,
    /// Plans that built fresh scratch rather than reusing it.
    #[cfg(test)]
    scratch_allocs: u64,
    /// Plans that reused the scratch buffers.
    #[cfg(test)]
    scratch_reuses: u64,
}

/// Inputs of [`VtPageManager::plan`] about the camera and the map.
#[derive(Clone, Copy, Debug)]
pub struct VtView<'a> {
    /// This frame's index.
    pub frame: u64,
    /// Camera position, world metres.
    pub eye: [f32; 3],
    /// Lowest and highest terrain altitude (bounds the vertical distance to a page).
    pub altitude: [f32; 2],
    /// Distance terms of the surface.
    pub terms: &'a DistTerms,
}

impl VtPageManager {
    /// An empty cache for `layout` with `settings.atlas_slots²` slots.
    #[must_use]
    pub fn new(layout: VtLayout, settings: VirtualTextureSettings) -> Self {
        let slots = settings.atlas_slots * settings.atlas_slots;
        Self {
            layout,
            settings,
            resident: HashMap::new(),
            free: (0..slots).rev().collect(),
            requests: Vec::new(),
            request_frame: 0,
            missing_since: HashMap::new(),
            dirty: HashSet::new(),
            stats: VtStats {
                capacity: slots,
                hit_rate: 1.0,
                ..VtStats::default()
            },
            resident_epoch: 0,
            published_epoch: 0,
            scratch_missing: Vec::new(),
            scratch_stale: Vec::new(),
            scratch_still: Vec::new(),
            scratch_evict: Vec::new(),
            scratch_dirty: Vec::new(),
            scratch_queued: HashSet::new(),
            scored_eye: None,
            score_idle: false,
            #[cfg(test)]
            resident_walks: 0,
            #[cfg(test)]
            scratch_allocs: 0,
            #[cfg(test)]
            scratch_reuses: 0,
        }
    }

    /// Records that the residency snapshot now matches [`Self::resident_epoch`].
    fn mark_published(&mut self) {
        self.published_epoch = self.resident_epoch;
    }

    /// Whether the GPU-facing slot snapshot must be replaced.
    ///
    /// A rebake changes texels in slots the snapshot already names, so it does not publish.
    /// Residency inserts, removals and flushes bump `resident_epoch`.
    /// Eye equality for the staleness skip is bitwise (`PartialEq`): a NaN eye never matches,
    /// and there is no motion epsilon, because a distance cut goes stale inside one texel.
    pub(crate) fn slots_need_republish(&self, flushed: bool) -> bool {
        flushed || self.resident_epoch != self.published_epoch
    }

    /// The page grid.
    #[must_use]
    pub fn layout(&self) -> &VtLayout {
        &self.layout
    }

    /// Counters of the last [`Self::plan`].
    #[must_use]
    pub fn stats(&self) -> VtStats {
        self.stats
    }

    /// Slot of a resident page.
    #[must_use]
    pub fn slot_of(&self, lod: u32, x: u32, y: u32) -> Option<u32> {
        self.resident.get(&pack_page(lod, x, y)).map(|r| r.slot)
    }

    /// Drops every page (the caller clears the page tables on the GPU).
    pub fn flush(&mut self) {
        let slots = self.settings.atlas_slots * self.settings.atlas_slots;
        self.resident.clear();
        self.free = (0..slots).rev().collect();
        self.missing_since.clear();
        self.dirty.clear();
        self.resident_epoch = self.resident_epoch.wrapping_add(1);
    }

    /// Marks every resident page whose world rectangle meets `[min, max]` (`[x, z]` metres) for
    /// an in-place re-bake, instead of dropping the whole cache: the pages keep their slots and
    /// keep showing their old texels until the new ones land. The caller grows the rectangle by
    /// how far a height change reaches into a page's baked terms ([`HEIGHT_REACH_M`]). Returns
    /// the number of pages marked.
    pub fn invalidate_rect(&mut self, min: [f32; 2], max: [f32; 2]) -> u32 {
        let mut marked = 0;
        for &key in self.resident.keys() {
            let (lod, x, y) = unpack_page(key);
            let (x0, z0, size) = self.layout.page_rect(lod, x, y);
            if x0 <= max[0]
                && x0 + size >= min[0]
                && z0 <= max[1]
                && z0 + size >= min[1]
                && self.dirty.insert(key)
            {
                marked += 1;
            }
        }
        marked
    }

    /// Pages waiting for their in-place re-bake.
    #[must_use]
    pub fn dirty_pages(&self) -> usize {
        self.dirty.len()
    }

    /// Records a readback: the requested pages (packed) as of `frame`, and the sampled hit
    /// and miss counts. Keys outside the layout are ignored.
    pub fn set_requests(&mut self, frame: u64, keys: &[u32], hits: u32, misses: u32) {
        self.requests.clear();
        for &key in keys {
            let (lod, x, y) = unpack_page(key);
            if lod < self.layout.lods && x < self.layout.side(lod) && y < self.layout.side(lod) {
                self.requests.push(key);
            }
        }
        self.requests.sort_unstable();
        self.requests.dedup();
        self.request_frame = frame;
        // The resident walk skips pages not requested in the last 30 frames. One that comes
        // back was not scored by the walk that let the eye settle, so it ends the settle.
        let recent = frame.saturating_sub(30);
        for key in &self.requests {
            if let Some(r) = self.resident.get_mut(key) {
                if r.seen < recent {
                    self.score_idle = false;
                }
                r.seen = r.seen.max(frame);
            }
        }
        self.stats.hit_rate = if hits + misses == 0 {
            1.0
        } else {
            hits as f32 / (hits + misses) as f32
        };
    }

    /// This frame's bakes: requested pages that are not resident (coarsest first, evicting
    /// the least recently requested pages the latest readback did not name), then the
    /// stalest resident pages still in use.
    pub fn plan(&mut self, view: &VtView<'_>) -> VtPlan {
        #[cfg(test)]
        let scratch_before = self.scratch_missing.capacity()
            + self.scratch_stale.capacity()
            + self.scratch_still.capacity()
            + self.scratch_evict.capacity()
            + self.scratch_dirty.capacity()
            + self.scratch_queued.capacity();
        let mut plan = VtPlan::default();
        // Pages whose heights changed: re-baked in place, coarse first. One nobody has asked for
        // lately is dropped instead (its next request bakes it fresh).
        let mut dirty_baked = 0u32;
        if !self.dirty.is_empty() {
            let recent = self.request_frame.saturating_sub(30);
            self.scratch_dirty.clear();
            for key in &self.dirty {
                self.scratch_dirty.push(*key);
            }
            self.scratch_dirty
                .sort_unstable_by(|a, b| (b >> 26).cmp(&(a >> 26)).then(a.cmp(b)));
            let dirty_n = self.scratch_dirty.len();
            let mut dirty_i = 0;
            while dirty_i < dirty_n {
                let key = self.scratch_dirty[dirty_i];
                dirty_i += 1;
                let Some(&Resident { slot, seen, .. }) = self.resident.get(&key) else {
                    self.dirty.remove(&key);
                    continue;
                };
                let (lod, x, y) = unpack_page(key);
                if seen < recent {
                    self.resident.remove(&key);
                    self.resident_epoch = self.resident_epoch.wrapping_add(1);
                    self.free.push(slot);
                    plan.table.push([self.layout.index(lod, x, y), 0]);
                    self.dirty.remove(&key);
                } else if dirty_baked < self.settings.bakes_per_frame {
                    if let Some(r) = self.resident.get_mut(&key) {
                        r.eye = view.eye;
                        // Count the re-bake as this frame's use, so the same `plan` cannot
                        // also evict the page and bake a missing page into the same slot
                        // (two jobs racing on one atlas texel in a single dispatch).
                        r.seen = self.request_frame;
                    }
                    plan.jobs.push([lod, x, y, slot]);
                    self.dirty.remove(&key);
                    dirty_baked += 1;
                }
            }
        }
        self.scratch_missing.clear();
        for &key in &self.requests {
            if !self.resident.contains_key(&key) {
                self.scratch_missing.push(key);
            }
        }
        // Coarse first: a pixel needs its page and the next coarser one, and coarse pages
        // serve many pixels.
        self.scratch_missing
            .sort_unstable_by(|a, b| (b >> 26).cmp(&(a >> 26)).then(a.cmp(b)));
        let mut evict_ready = false;
        let mut baked = 0u32;
        let missing_n = self.scratch_missing.len();
        let mut missing_i = 0;
        while missing_i < missing_n {
            let key = self.scratch_missing[missing_i];
            missing_i += 1;
            if baked + dirty_baked >= self.settings.bakes_per_frame {
                break;
            }
            let slot = if let Some(slot) = self.free.pop() {
                slot
            } else {
                if !evict_ready {
                    self.scratch_evict.clear();
                    for (page, resident) in &self.resident {
                        if resident.seen < self.request_frame {
                            self.scratch_evict.push((resident.seen, *page));
                        }
                    }
                    self.scratch_evict.sort_unstable_by(|a, b| b.cmp(a));
                    evict_ready = true;
                }
                let Some((_, victim)) = self.scratch_evict.pop() else {
                    break;
                };
                let Some(old) = self.resident.remove(&victim) else {
                    continue;
                };
                self.resident_epoch = self.resident_epoch.wrapping_add(1);
                let (lod, x, y) = unpack_page(victim);
                plan.table.push([self.layout.index(lod, x, y), 0]);
                old.slot
            };
            let (lod, x, y) = unpack_page(key);
            self.resident.insert(
                key,
                Resident {
                    slot,
                    seen: self.request_frame,
                    eye: view.eye,
                },
            );
            self.resident_epoch = self.resident_epoch.wrapping_add(1);
            plan.jobs.push([lod, x, y, slot]);
            plan.table.push([self.layout.index(lod, x, y), slot + 1]);
            baked += 1;
        }
        plan.new_pages = baked;
        // Re-bakes: pages requested recently whose distance terms the camera has moved.
        // A settled eye whose last walk queued nothing is not scored again. The compare is
        // bitwise: a NaN eye never matches, and a sub-texel move can still cross a cut.
        self.scratch_queued.clear();
        for job in &plan.jobs {
            self.scratch_queued.insert(pack_page(job[0], job[1], job[2]));
        }
        let eye_held = self.score_idle && self.scored_eye == Some(view.eye);
        self.scratch_stale.clear();
        let mut queued_rebakes = 0u32;
        if self.settings.rebakes_per_frame > 0 && !eye_held {
            let recent = self.request_frame.saturating_sub(30);
            let eye = view.eye;
            let altitude = view.altitude;
            let threshold = self.settings.stale_threshold;
            let layout = self.layout;
            let terms = view.terms;
            let resident = &self.resident;
            let queued = &self.scratch_queued;
            let stale = &mut self.scratch_stale;
            #[cfg(test)]
            let walks = &mut self.resident_walks;
            for (&key, r) in resident {
                #[cfg(test)]
                {
                    let seen = *walks;
                    *walks = seen.wrapping_add(1);
                }
                if r.seen < recent || queued.contains(&key) {
                    continue;
                }
                let moved =
                    ((eye[0] - r.eye[0]).powi(2) + (eye[1] - r.eye[1]).powi(2) + (eye[2] - r.eye[2]).powi(2))
                        .sqrt();
                if moved <= 0.0 {
                    continue;
                }
                let (d_min, d_max) = page_rect_distance(&layout, key, eye, altitude);
                let texel = layout.texel_m(key >> 26);
                let score = terms.score(d_min, d_max, moved, texel);
                if score >= threshold {
                    stale.push((score, key));
                }
            }
            self.scratch_stale
                .sort_unstable_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            let take = (self.settings.rebakes_per_frame as usize).min(self.scratch_stale.len());
            let mut stale_i = 0;
            while stale_i < take {
                let key = self.scratch_stale[stale_i].1;
                stale_i += 1;
                if let Some(r) = self.resident.get_mut(&key) {
                    r.eye = view.eye;
                    let (lod, x, y) = unpack_page(key);
                    plan.jobs.push([lod, x, y, r.slot]);
                    queued_rebakes += 1;
                }
            }
            self.score_idle = queued_rebakes == 0;
            self.scored_eye = Some(view.eye);
        } else if self.settings.rebakes_per_frame == 0 {
            self.score_idle = true;
            self.scored_eye = Some(view.eye);
        }
        let rebaked = dirty_baked + queued_rebakes;
        // Pop-in accounting: how long the oldest unserved request has waited.
        self.scratch_still.clear();
        for &key in &self.requests {
            if !self.resident.contains_key(&key) {
                self.scratch_still.push(key);
            }
        }
        self.scratch_still.sort_unstable();
        let still_n = self.scratch_still.len();
        let still = std::mem::take(&mut self.scratch_still);
        self.missing_since
            .retain(|key, _| still.binary_search(key).is_ok());
        for &key in &still {
            self.missing_since.entry(key).or_insert(view.frame);
        }
        self.scratch_still = still;
        let oldest = self.missing_since.values().copied().min().unwrap_or(view.frame);
        self.stats.resident = self.resident.len() as u32;
        self.stats.baked = baked;
        self.stats.rebaked = rebaked;
        self.stats.requested = self.requests.len() as u32;
        self.stats.missing = still_n as u32;
        self.stats.oldest_miss_frames = view.frame.saturating_sub(oldest) as u32;
        #[cfg(test)]
        {
            let scratch_after = self.scratch_missing.capacity()
                + self.scratch_stale.capacity()
                + self.scratch_still.capacity()
                + self.scratch_evict.capacity()
                + self.scratch_dirty.capacity()
                + self.scratch_queued.capacity();
            if scratch_after > scratch_before {
                self.scratch_allocs = self.scratch_allocs.wrapping_add(1);
            } else {
                self.scratch_reuses = self.scratch_reuses.wrapping_add(1);
            }
        }
        plan
    }
}

/// `VtParams` in `terrain.wgsl`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct VtParamsUniform {
    dims: [f32; 4],
    info: [f32; 4],
    view: [f32; 4],
    atlas: [f32; 4],
    extra: [f32; 4],
    lod_offset: [u32; 16],
}

/// `VtReduce` in `vt.wgsl`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct VtReduceUniform {
    dims: [u32; 4],
    lod_offset: [u32; 16],
    misc: [u32; 4],
}

/// One staging buffer of the readback ring.
struct ReadSlot {
    buffer: wgpu::Buffer,
    state: Arc<AtomicU8>,
    /// Frame whose copy the buffer holds.
    frame: u64,
}

/// CPU mirror of the virtual-texture node map (one `u32` per cell of the root grid).
struct NodeMapCpu {
    cells: u32,
    cpu: Vec<u32>,
    scratch: Vec<u32>,
    key: u64,
    /// Node-buffer slot of the selection's first node when the map was painted.
    base: u32,
    valid: bool,
    /// Times the map was painted.
    #[cfg(test)]
    rebuilds: u64,
    /// Times the scratch buffer was allocated.
    #[cfg(test)]
    allocs: u64,
}

impl NodeMapCpu {
    fn new(cells: u32) -> Self {
        Self {
            cells,
            cpu: Vec::new(),
            scratch: Vec::new(),
            key: 0,
            base: 0,
            valid: false,
            #[cfg(test)]
            rebuilds: 0,
            #[cfg(test)]
            allocs: 0,
        }
    }

    /// Paints `nodes` for `key`. Returns whether the bytes the GPU holds changed.
    ///
    /// `base` is the node-buffer slot the selection starts at: the map stores slots, and the
    /// shadow cascades ahead of the camera selection move it while the selection is held. The
    /// same `(key, base)` paints once. The iterator is not pulled on a repeat, so a held camera
    /// does not allocate a node list or a fresh map.
    fn rebuild(
        &mut self,
        key: u64,
        base: u32,
        nodes: impl Iterator<Item = (crate::cdlod::QuadNode, u32)>,
    ) -> bool {
        if self.valid && self.key == key && self.base == base {
            return false;
        }
        #[cfg(test)]
        {
            self.rebuilds += 1;
        }
        self.key = key;
        self.base = base;
        self.valid = true;
        let n = (self.cells as usize).saturating_mul(self.cells as usize);
        if self.scratch.len() == n {
            self.scratch.fill(0);
        } else {
            self.scratch = vec![0u32; n];
            #[cfg(test)]
            {
                self.allocs += 1;
            }
        }
        let cells = self.cells as usize;
        for (node, slot) in nodes {
            let span = 1usize << node.level;
            let x0 = node.x as usize * span;
            let z0 = node.z as usize * span;
            for z in z0..(z0 + span).min(cells) {
                let row = &mut self.scratch[z * cells..(z + 1) * cells];
                for cell in row.iter_mut().take((x0 + span).min(cells)).skip(x0) {
                    *cell = slot + 1;
                }
            }
        }
        if self.scratch == self.cpu {
            return false;
        }
        std::mem::swap(&mut self.cpu, &mut self.scratch);
        true
    }
}

/// GPU state of the virtual texture: atlas, tables, feedback, pipelines and readback ring.
pub(crate) struct VtGpu {
    settings: VirtualTextureSettings,
    manager: VtPageManager,
    params: VtParamsUniform,
    params_buffer: wgpu::Buffer,
    params_bytes: Vec<u8>,
    reduce_buffer: wgpu::Buffer,
    reduce_bytes: Vec<u8>,
    pages_buffer: wgpu::Buffer,
    feedback_buffer: wgpu::Buffer,
    requests_buffer: wgpu::Buffer,
    /// The allocation list: an arrival-order region and a hashed overflow region
    /// (`vt.wgsl`), each `alloc_capacity` words after the header.
    alloc_buffer: wgpu::Buffer,
    alloc_capacity: u32,
    updates_buffer: wgpu::Buffer,
    updates_capacity: u32,
    jobs_buffer: wgpu::Buffer,
    node_map_buffer: wgpu::Buffer,
    node_map: NodeMapCpu,
    atlas: [wgpu::Texture; 4],
    atlas_storage: [wgpu::TextureView; 4],
    /// Map E: the horizon slopes (whole-surface mode; 1 texel otherwise).
    atlas_e: wgpu::Texture,
    atlas_e_storage: wgpu::TextureView,
    bake_layout: wgpu::BindGroupLayout,
    bake_bind: Option<wgpu::BindGroup>,
    bake_nodes: Option<wgpu::Buffer>,
    bake_pipeline: wgpu::ComputePipeline,
    /// The bake writes map E itself (`cs_vt_bake_e`; binding 11 in `bake_layout`).
    bake_e: bool,
    /// `cs_vt_bake_horizon` and its group 2 (whole-surface mode).
    horizon_layout: wgpu::BindGroupLayout,
    horizon_bind: Option<wgpu::BindGroup>,
    horizon_pipeline: Option<wgpu::ComputePipeline>,
    reduce_bind: wgpu::BindGroup,
    table_pipeline: wgpu::ComputePipeline,
    requests_pipeline: wgpu::ComputePipeline,
    allocate_pipeline: wgpu::ComputePipeline,
    sample_layout: wgpu::BindGroupLayout,
    sample_bind: wgpu::BindGroup,
    ring: Vec<ReadSlot>,
    /// Slot this frame's copy goes into, and whether `encode_after` recorded it.
    copy_slot: Option<usize>,
    copied: AtomicBool,
    queries: Option<(wgpu::QuerySet, wgpu::Buffer)>,
    timestamp_period: f32,
    frame: u64,
    content_key: Option<u64>,
    clear_table: bool,
    jobs_now: u32,
    updates_now: u32,
    /// Table writes the update buffer could not take this frame (applied next frame).
    deferred: Vec<[u32; 2]>,
    stats: VtStats,
    handle: TerrainVtHandle,
}

fn storage_entry(
    binding: u32,
    visibility: wgpu::ShaderStages,
    read_only: bool,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
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

fn storage_texture_entry(binding: u32, format: wgpu::TextureFormat) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format,
            view_dimension: wgpu::TextureViewDimension::D2,
        },
        count: None,
    }
}

fn buffer(device: &wgpu::Device, label: &str, size: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size.max(16),
        usage,
        mapped_at_creation: false,
    })
}

const ATLAS_FORMATS: [wgpu::TextureFormat; 4] = [wgpu::TextureFormat::Rgba16Float; 4];
/// Map E (the horizon slopes, signed and unbounded: half floats, like the split modes' map B).
const ATLAS_E_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// What [`VtGpu::new`] needs from the terrain renderer.
pub(crate) struct VtGpuInit<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    pub settings: VirtualTextureSettings,
    pub world_width: f32,
    /// The map's grid origin ([`crate::TerrainMap::grid_origin`], ADR-8179).
    pub grid_origin: [f32; 2],
    pub root_level: u32,
    pub frame_layout: &'a wgpu::BindGroupLayout,
    pub terrain_layout: &'a wgpu::BindGroupLayout,
    /// `terrain.wgsl` as the shared pipelines compile it (`cs_vt_bake`, `cs_vt_bake_horizon`).
    pub shader: &'a wgpu::ShaderModule,
    /// Source and device registry key the immutable bake pipelines across VT rebuilds.
    pub shader_source: &'a str,
    pub shared_pipelines: &'a regia_render::device_pipelines::DevicePipelines,
    /// The handle diagnostics read through.
    pub handle: TerrainVtHandle,
}

/// Immutable VT bake state, shared across terrains and dry/weather atlas rebuilds.
#[derive(Clone)]
struct BakePipelines {
    bake_layout: wgpu::BindGroupLayout,
    bake_pipeline: wgpu::ComputePipeline,
    bake_e: bool,
    horizon_layout: wgpu::BindGroupLayout,
    horizon_pipeline: Option<wgpu::ComputePipeline>,
}

fn bake_pipelines(init: &VtGpuInit<'_>) -> BakePipelines {
    let whole = init.settings.cache == VtCache::Surface;
    let bake_e = whole && init.device.limits().max_storage_textures_per_shader_stage >= 5;
    let key = regia_render::device_pipelines::pipeline_key(&(
        "regia_terrain vt bake",
        init.shader_source,
        whole,
        bake_e,
    ));
    init.shared_pipelines.get_or_create(key, || {
        let d = regia_render::TrackedDevice::observe(init.device);
        // Bake: group 2 = nodes, params, jobs, atlas (storage), node map; map E too where the
        // device binds five storage textures per stage (`cs_vt_bake_e`: one pass, the slopes
        // the surface computes anyway). With four (regia_render's mobile tier) map E gets its
        // own pass, `cs_vt_bake_horizon`, which walks the geometry a second time.
        let mut bake_entries = vec![
            storage_entry(0, wgpu::ShaderStages::COMPUTE, true),
            uniform_entry(4, wgpu::ShaderStages::COMPUTE),
            storage_entry(5, wgpu::ShaderStages::COMPUTE, true),
            storage_texture_entry(6, ATLAS_FORMATS[0]),
            storage_texture_entry(7, ATLAS_FORMATS[1]),
            storage_texture_entry(8, ATLAS_FORMATS[2]),
            storage_texture_entry(9, ATLAS_FORMATS[3]),
            storage_entry(10, wgpu::ShaderStages::COMPUTE, true),
        ];
        if bake_e {
            bake_entries.push(storage_texture_entry(11, ATLAS_E_FORMAT));
        }
        let bake_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("terrain vt bake layout"),
            entries: &bake_entries,
        });
        let bake_pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("terrain vt bake pipeline layout"),
            bind_group_layouts: &[
                Some(init.frame_layout),
                Some(init.terrain_layout),
                Some(&bake_layout),
            ],
            immediate_size: 0,
        });
        let bake_pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("terrain vt bake"),
            layout: Some(&bake_pipeline_layout),
            module: init.shader,
            entry_point: Some(if bake_e { "cs_vt_bake_e" } else { "cs_vt_bake" }),
            compilation_options: Default::default(),
            cache: regia_render::pipeline_cache::cache_for(&d).as_deref(),
        });
        // Map E's own bake on four-storage-texture devices.
        let horizon_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("terrain vt horizon bake layout"),
            entries: &[
                storage_entry(0, wgpu::ShaderStages::COMPUTE, true),
                uniform_entry(4, wgpu::ShaderStages::COMPUTE),
                storage_entry(5, wgpu::ShaderStages::COMPUTE, true),
                storage_entry(10, wgpu::ShaderStages::COMPUTE, true),
                storage_texture_entry(11, ATLAS_E_FORMAT),
            ],
        });
        let horizon_pipeline = (whole && !bake_e).then(|| {
            let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("terrain vt horizon bake pipeline layout"),
                bind_group_layouts: &[
                    Some(init.frame_layout),
                    Some(init.terrain_layout),
                    Some(&horizon_layout),
                ],
                immediate_size: 0,
            });
            d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("terrain vt horizon bake"),
                layout: Some(&layout),
                module: init.shader,
                entry_point: Some("cs_vt_bake_horizon"),
                compilation_options: Default::default(),
                cache: regia_render::pipeline_cache::cache_for(&d).as_deref(),
            })
        });

        BakePipelines {
            bake_layout,
            bake_pipeline,
            bake_e,
            horizon_layout,
            horizon_pipeline,
        }
    })
}

/// Prepares a shader variant without allocating a second atlas or publishing a VT handle.
pub(crate) fn warm_bake_pipelines(init: &VtGpuInit<'_>) {
    let _ = bake_pipelines(init);
}

impl VtGpu {
    /// Builds the atlas, tables, buffers and the bake / reduction pipelines.
    pub(crate) fn new(init: &VtGpuInit<'_>) -> Result<Self, TerrainError> {
        init.settings.validate()?;
        if init.root_level > 10 {
            return Err(TerrainError::VirtualTexture(format!(
                "quadtree root level {} > 10: the node map would exceed 1024² cells",
                init.root_level
            )));
        }
        let d = init.device;
        let _span = tracing::info_span!("terrain_vt_create").entered();
        let layout = VtLayout::new(init.world_width, &init.settings)?.with_grid_origin(init.grid_origin);
        let s = init.settings;
        let slot = s.page_texels + 2;
        let atlas_side = s.atlas_slots * slot;
        let max_side = d.limits().max_texture_dimension_2d;
        if atlas_side > max_side {
            return Err(TerrainError::VirtualTexture(format!(
                "atlas {atlas_side}² exceeds the device's {max_side}"
            )));
        }
        let storage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
        let params_buffer = buffer(
            d,
            "terrain vt params",
            std::mem::size_of::<VtParamsUniform>() as u64,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        let reduce_buffer = buffer(
            d,
            "terrain vt reduce params",
            std::mem::size_of::<VtReduceUniform>() as u64,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        let pages_buffer = buffer(d, "terrain vt page tables", u64::from(layout.total) * 4, storage);
        let feedback_buffer = buffer(
            d,
            "terrain vt feedback",
            (u64::from(layout.side0) * u64::from(layout.side0) + 2) * 4,
            storage,
        );
        let requests_buffer = buffer(d, "terrain vt requests", u64::from(layout.total) * 4, storage);
        let alloc_capacity = (s.atlas_slots * s.atlas_slots * 4).clamp(1024, 65536);
        let alloc_bytes = u64::from(ALLOC_HEADER + 2 * alloc_capacity) * 4;
        let alloc_buffer = buffer(
            d,
            "terrain vt allocation",
            alloc_bytes,
            storage | wgpu::BufferUsages::COPY_SRC,
        );
        let updates_capacity = 8192u32;
        let updates_buffer = buffer(
            d,
            "terrain vt table writes",
            u64::from(updates_capacity) * 8,
            storage,
        );
        let jobs_buffer = buffer(
            d,
            "terrain vt jobs",
            u64::from(s.bakes_per_frame + s.rebakes_per_frame) * 16,
            storage,
        );
        let node_cells = 1u32 << init.root_level;
        let node_map_buffer = buffer(
            d,
            "terrain vt node map",
            u64::from(node_cells) * u64::from(node_cells) * 4,
            storage,
        );
        let atlas: [wgpu::Texture; 4] = std::array::from_fn(|i| {
            let texture = d.create_texture(&wgpu::TextureDescriptor {
                label: Some("terrain vt atlas"),
                size: wgpu::Extent3d {
                    width: atlas_side,
                    height: atlas_side,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: ATLAS_FORMATS[i],
                usage: wgpu::TextureUsages::STORAGE_BINDING
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            regia_render::resource_inspector::track(&format!("terrain vt atlas {i}"), &texture);
            texture
        });
        let atlas_storage: [wgpu::TextureView; 4] =
            std::array::from_fn(|i| atlas[i].create_view(&wgpu::TextureViewDescriptor::default()));
        // Map E only where the colour pass reads it (the split modes keep the slopes in B).
        let whole = s.cache == VtCache::Surface;
        let e_side = if whole { atlas_side } else { 1 };
        let atlas_e = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("terrain vt atlas horizon"),
            size: wgpu::Extent3d {
                width: e_side,
                height: e_side,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: ATLAS_E_FORMAT,
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let atlas_e_storage = atlas_e.create_view(&wgpu::TextureViewDescriptor::default());
        let atlas_e_sample = atlas_e.create_view(&wgpu::TextureViewDescriptor {
            usage: Some(wgpu::TextureUsages::TEXTURE_BINDING),
            ..Default::default()
        });
        let atlas_sample: [wgpu::TextureView; 4] = std::array::from_fn(|i| {
            atlas[i].create_view(&wgpu::TextureViewDescriptor {
                usage: Some(wgpu::TextureUsages::TEXTURE_BINDING),
                ..Default::default()
            })
        });
        let sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("terrain vt sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let BakePipelines {
            bake_layout,
            bake_pipeline,
            bake_e,
            horizon_layout,
            horizon_pipeline,
        } = bake_pipelines(init);

        // Reduction and table writes (`vt.wgsl`).
        let reduce_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("terrain vt reduce layout"),
            entries: &[
                uniform_entry(0, wgpu::ShaderStages::COMPUTE),
                storage_entry(1, wgpu::ShaderStages::COMPUTE, false),
                storage_entry(2, wgpu::ShaderStages::COMPUTE, false),
                storage_entry(3, wgpu::ShaderStages::COMPUTE, false),
                storage_entry(4, wgpu::ShaderStages::COMPUTE, false),
                storage_entry(5, wgpu::ShaderStages::COMPUTE, true),
            ],
        });
        let reduce_bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("terrain vt reduce bind"),
            layout: &reduce_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: reduce_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: feedback_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: requests_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: alloc_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: pages_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: updates_buffer.as_entire_binding(),
                },
            ],
        });
        let vt_module = d.create_shader_module(wgpu::include_wgsl!("vt.wgsl"));
        let reduce_pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("terrain vt reduce pipeline layout"),
            bind_group_layouts: &[Some(&reduce_layout)],
            immediate_size: 0,
        });
        let compute = |entry: &str| {
            let key = regia_render::device_pipelines::pipeline_key(&(
                "regia_terrain vt reduce",
                include_str!("vt.wgsl"),
                entry,
            ));
            init.shared_pipelines.get_or_create(key, || {
                regia_render::TrackedDevice::observe(d).create_compute_pipeline(
                    &wgpu::ComputePipelineDescriptor {
                        label: Some(entry),
                        layout: Some(&reduce_pipeline_layout),
                        module: &vt_module,
                        entry_point: Some(entry),
                        compilation_options: Default::default(),
                        cache: regia_render::pipeline_cache::cache_for(d).as_deref(),
                    },
                )
            })
        };
        let table_pipeline = compute("cs_table");
        let requests_pipeline = compute("cs_requests");
        let allocate_pipeline = compute("cs_allocate");

        // Colour pass: group 4.
        let frag = wgpu::ShaderStages::FRAGMENT;
        let sampled = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: frag,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let sample_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("terrain vt sample layout"),
            entries: &[
                uniform_entry(1, frag),
                storage_entry(2, frag, true),
                storage_entry(3, frag, false),
                sampled(4),
                sampled(5),
                sampled(6),
                sampled(7),
                sampled(9),
                wgpu::BindGroupLayoutEntry {
                    binding: 8,
                    visibility: frag,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let sample_bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("terrain vt sample bind"),
            layout: &sample_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: params_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: pages_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: feedback_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&atlas_sample[0]),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(&atlas_sample[1]),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::TextureView(&atlas_sample[2]),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: wgpu::BindingResource::TextureView(&atlas_sample[3]),
                },
                wgpu::BindGroupEntry {
                    binding: 8,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 9,
                    resource: wgpu::BindingResource::TextureView(&atlas_e_sample),
                },
            ],
        });

        let stamps = d.features().contains(wgpu::Features::TIMESTAMP_QUERY);
        let queries = stamps.then(|| {
            (
                d.create_query_set(&wgpu::QuerySetDescriptor {
                    label: Some("terrain vt timestamps"),
                    ty: wgpu::QueryType::Timestamp,
                    count: STAMPS,
                }),
                buffer(
                    d,
                    "terrain vt timestamp resolve",
                    u64::from(STAMPS) * 8,
                    wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                ),
            )
        });
        let slot_bytes = alloc_bytes + u64::from(STAMPS) * 8;
        let ring = (0..READBACK_SLOTS)
            .map(|_| ReadSlot {
                buffer: buffer(
                    d,
                    "terrain vt readback",
                    slot_bytes,
                    wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                ),
                state: Arc::new(AtomicU8::new(ST_IDLE)),
                frame: 0,
            })
            .collect();

        let mut lod_offset = [0u32; 16];
        lod_offset.copy_from_slice(&layout.offsets);
        let params = VtParamsUniform {
            dims: [s.lod0_texel_m, s.page_texels as f32, 1.0, slot as f32],
            info: [layout.lods as f32, layout.side0 as f32, atlas_side as f32, 1.0],
            view: [s.oversample, s.near_m, s.min_normal_y, 0.0],
            atlas: [
                s.atlas_slots as f32,
                node_cells as f32,
                init.world_width / node_cells as f32,
                s.cache.code(),
            ],
            extra: [if s.stochastic_lod { 1.0 } else { 0.0 }, 0.0, 0.0, 0.0],
            lod_offset,
        };
        let manager = VtPageManager::new(layout, s);
        let mut stats = manager.stats();
        stats.capacity = s.atlas_slots * s.atlas_slots;
        Ok(Self {
            settings: s,
            manager,
            params,
            params_buffer,
            params_bytes: Vec::new(),
            reduce_buffer,
            reduce_bytes: Vec::new(),
            pages_buffer,
            feedback_buffer,
            requests_buffer,
            alloc_buffer,
            alloc_capacity,
            updates_buffer,
            updates_capacity,
            jobs_buffer,
            node_map_buffer,
            node_map: NodeMapCpu::new(node_cells),
            atlas,
            atlas_storage,
            atlas_e,
            atlas_e_storage,
            bake_layout,
            bake_bind: None,
            bake_nodes: None,
            bake_pipeline,
            bake_e,
            horizon_layout,
            horizon_bind: None,
            horizon_pipeline,
            reduce_bind,
            table_pipeline,
            requests_pipeline,
            allocate_pipeline,
            sample_layout,
            sample_bind,
            ring,
            copy_slot: None,
            copied: AtomicBool::new(false),
            queries,
            timestamp_period: init.queue.get_timestamp_period(),
            frame: 0,
            content_key: None,
            // The tables start zeroed (wgpu zero-initialises buffers): nothing resident.
            clear_table: false,
            jobs_now: 0,
            updates_now: 0,
            deferred: Vec::new(),
            stats,
            handle: init.handle.clone(),
        })
    }

    /// Group 4 layout of the virtual-texture colour pipeline.
    pub(crate) fn sample_layout(&self) -> &wgpu::BindGroupLayout {
        &self.sample_layout
    }

    /// Group 4 of the virtual-texture colour pipeline.
    pub(crate) fn sample_bind(&self) -> &wgpu::BindGroup {
        &self.sample_bind
    }

    /// Counters of the last frame.
    pub(crate) fn stats(&self) -> VtStats {
        self.stats
    }

    /// The page grid.
    pub(crate) fn layout(&self) -> VtLayout {
        *self.manager.layout()
    }

    /// Publishes the residency to the handle when it changed this frame.
    fn publish(&self, device: &wgpu::Device, queue: &wgpu::Queue, changed: bool) {
        let mut guard = self.handle.lock();
        if !changed && let Some(snap) = guard.snapshot.as_mut() {
            snap.stats = self.stats;
            return;
        }
        guard.snapshot = Some(VtSnapshot {
            device: device.clone(),
            queue: queue.clone(),
            atlas: self.atlas.clone(),
            atlas_e: self.atlas_e.clone(),
            settings: self.settings,
            layout: *self.manager.layout(),
            slots: self.manager.resident.iter().map(|(k, r)| (*k, r.slot)).collect(),
            stats: self.stats,
        });
    }

    /// The node map of a camera selection: per leaf-node cell, the node buffer slot + 1 of
    /// the selected node covering it. `nodes` yields `(node, slot)`. `key` is the selection
    /// key and `base` the slot of its first node. A repeat of the same key and base does not
    /// paint. Identical coverage is not uploaded.
    pub(crate) fn build_node_map(
        &mut self,
        key: u64,
        base: u32,
        nodes: impl Iterator<Item = (crate::cdlod::QuadNode, u32)>,
    ) -> bool {
        self.node_map.rebuild(key, base, nodes)
    }

    /// Marks the resident pages a height edit over `[min, max]` (`[x, z]` metres) can change for
    /// an in-place re-bake ([`VtPageManager::invalidate_rect`]); returns how many.
    pub(crate) fn invalidate_rect(&mut self, min: [f32; 2], max: [f32; 2]) -> u32 {
        self.manager.invalidate_rect(min, max)
    }

    /// Reads finished readbacks, plans this frame's bakes and stages their uploads.
    ///
    /// `nodes` is the terrain's node buffer (rebinds the bake group when it was regrown),
    /// `content_key` hashes everything the surface depends on besides the camera (a change
    /// drops every page), `terms` its distance terms.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        nodes: &wgpu::Buffer,
        node_map_changed: bool,
        content_key: u64,
        eye: [f32; 3],
        altitude: [f32; 2],
        terms: &DistTerms,
        upload: &mut regia_render::SharedUpload<'_>,
    ) {
        let _span = tracing::info_span!("terrain_vt_prepare").entered();
        self.frame += 1;
        let frame = self.frame;
        self.read_back();
        // Map the slot the last frame copied into (submitted since).
        if let Some(i) = self.copy_slot.take()
            && self.copied.swap(false, Ordering::AcqRel)
        {
            let slot = &mut self.ring[i];
            slot.frame = frame - 1;
            slot.state.store(ST_INFLIGHT, Ordering::Release);
            let state = Arc::clone(&slot.state);
            slot.buffer
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |result| {
                    state.store(
                        if result.is_ok() { ST_READY } else { ST_FAILED },
                        Ordering::Release,
                    );
                });
        }
        self.copy_slot = self
            .ring
            .iter()
            .position(|s| s.state.load(Ordering::Acquire) == ST_IDLE);

        let flushed = self.content_key != Some(content_key);
        if flushed {
            self.manager.flush();
            self.deferred.clear();
            self.clear_table = self.content_key.is_some();
            self.content_key = Some(content_key);
        } else {
            self.clear_table = false;
        }
        let plan = self.manager.plan(&VtView {
            frame,
            eye,
            altitude,
            terms,
        });
        let mut table = std::mem::take(&mut self.deferred);
        table.extend_from_slice(&plan.table);
        if table.len() > self.updates_capacity as usize {
            self.deferred = table.split_off(self.updates_capacity as usize);
        }
        self.jobs_now = plan.jobs.len() as u32;
        self.updates_now = table.len() as u32;
        if !plan.jobs.is_empty() {
            upload.write(&self.jobs_buffer, 0, bytemuck::cast_slice(&plan.jobs));
        }
        if !table.is_empty() {
            upload.write(&self.updates_buffer, 0, bytemuck::cast_slice(&table));
        }
        if node_map_changed {
            upload.write(&self.node_map_buffer, 0, bytemuck::cast_slice(&self.node_map.cpu));
        }
        self.params.view[3] = (frame % 4096) as f32;
        let params = self.params;
        upload.write_if_changed(
            &mut self.params_bytes,
            &self.params_buffer,
            bytemuck::bytes_of(&params),
        );
        let layout = *self.manager.layout();
        let mut lod_offset = [0u32; 16];
        lod_offset.copy_from_slice(&layout.offsets);
        let reduce = VtReduceUniform {
            dims: [layout.side0, layout.lods, self.alloc_capacity, layout.total],
            lod_offset,
            misc: [self.updates_now, 0, 0, 0],
        };
        upload.write_if_changed(
            &mut self.reduce_bytes,
            &self.reduce_buffer,
            bytemuck::bytes_of(&reduce),
        );
        let rebind = self.bake_nodes.as_ref() != Some(nodes) || self.bake_bind.is_none();
        if rebind {
            self.bake_bind = Some(
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("terrain vt bake bind"),
                    layout: &self.bake_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: nodes.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: self.params_buffer.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 5,
                            resource: self.jobs_buffer.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 6,
                            resource: wgpu::BindingResource::TextureView(&self.atlas_storage[0]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 7,
                            resource: wgpu::BindingResource::TextureView(&self.atlas_storage[1]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 8,
                            resource: wgpu::BindingResource::TextureView(&self.atlas_storage[2]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 9,
                            resource: wgpu::BindingResource::TextureView(&self.atlas_storage[3]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 10,
                            resource: self.node_map_buffer.as_entire_binding(),
                        },
                    ]
                    .into_iter()
                    .chain(self.bake_e.then_some(wgpu::BindGroupEntry {
                        binding: 11,
                        resource: wgpu::BindingResource::TextureView(&self.atlas_e_storage),
                    }))
                    .collect::<Vec<_>>(),
                }),
            );
            self.horizon_bind = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("terrain vt horizon bake bind"),
                layout: &self.horizon_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: nodes.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: self.params_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 5,
                        resource: self.jobs_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 10,
                        resource: self.node_map_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 11,
                        resource: wgpu::BindingResource::TextureView(&self.atlas_e_storage),
                    },
                ],
            }));
            self.bake_nodes = Some(nodes.clone());
        }
        let bake_ms = self.stats.bake_ms;
        let reduce_ms = self.stats.reduce_ms;
        self.stats = self.manager.stats();
        self.stats.bake_ms = bake_ms;
        self.stats.reduce_ms = reduce_ms;
        self.stats.flushed = flushed;
        let publish_slots = self.manager.slots_need_republish(flushed);
        if publish_slots {
            self.manager.mark_published();
        }
        self.publish(device, queue, publish_slots);
    }

    /// Reads every mapped slot old enough: the newest becomes the request list.
    fn read_back(&mut self) {
        let mut newest: Option<usize> = None;
        for (i, slot) in self.ring.iter().enumerate() {
            match slot.state.load(Ordering::Acquire) {
                ST_READY if self.frame.saturating_sub(slot.frame) >= READBACK_LATENCY => {
                    if newest.is_none_or(|n| self.ring[n].frame < slot.frame) {
                        newest = Some(i);
                    }
                }
                ST_FAILED => slot.state.store(ST_IDLE, Ordering::Release),
                _ => {}
            }
        }
        for (i, slot) in self.ring.iter().enumerate() {
            if slot.state.load(Ordering::Acquire) != ST_READY
                || self.frame.saturating_sub(slot.frame) < READBACK_LATENCY
            {
                continue;
            }
            if Some(i) == newest {
                let words: Option<Vec<u32>> = slot
                    .buffer
                    .slice(..)
                    .get_mapped_range()
                    .ok()
                    .map(|view| bytemuck::cast_slice(&view).to_vec());
                if let Some(words) = words {
                    let (keys, misses, hits) = parse_alloc_words(&words, self.alloc_capacity);
                    self.manager.set_requests(slot.frame, &keys, hits, misses);
                    let stamp_at = (ALLOC_HEADER + 2 * self.alloc_capacity) as usize;
                    if self.queries.is_some()
                        && let Some(stamps) = words.get(stamp_at..stamp_at + 2 * STAMPS as usize)
                    {
                        let t = |k: usize| u64::from(stamps[2 * k]) | (u64::from(stamps[2 * k + 1]) << 32);
                        let ms = |a: u64, b: u64| b.saturating_sub(a) as f32 * self.timestamp_period * 1e-6;
                        self.stats.bake_ms = ms(t(0), t(1));
                        self.stats.reduce_ms = ms(t(2), t(3));
                    }
                }
            }
            slot.buffer.unmap();
            slot.state.store(ST_IDLE, Ordering::Release);
        }
    }

    /// Page-table writes and bakes, before the frame's passes.
    pub(crate) fn encode_before(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        frame_bind: &wgpu::BindGroup,
        terrain_bind: &wgpu::BindGroup,
    ) {
        let _span = tracing::trace_span!("terrain_vt_bake").entered();
        if self.clear_table {
            encoder.clear_buffer(&self.pages_buffer, 0, None);
        }
        // Timed by the pass's own timestamps (an empty pass when there is nothing to do, so
        // the pair is always written).
        if self.updates_now > 0 || self.jobs_now > 0 || self.queries.is_some() {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("terrain vt bake"),
                timestamp_writes: self
                    .queries
                    .as_ref()
                    .map(|(qs, _)| wgpu::ComputePassTimestampWrites {
                        query_set: qs,
                        beginning_of_pass_write_index: Some(0),
                        end_of_pass_write_index: Some(1),
                    }),
            });
            if self.updates_now > 0 {
                pass.set_pipeline(&self.table_pipeline);
                pass.set_bind_group(0, &self.reduce_bind, &[]);
                pass.dispatch_workgroups(self.updates_now.div_ceil(64), 1, 1);
            }
            if self.jobs_now > 0
                && let Some(bind) = &self.bake_bind
            {
                let groups = (self.settings.page_texels + 2).div_ceil(8);
                pass.set_pipeline(&self.bake_pipeline);
                pass.set_bind_group(0, frame_bind, &[]);
                pass.set_bind_group(1, terrain_bind, &[]);
                pass.set_bind_group(2, bind, &[]);
                pass.dispatch_workgroups(groups, groups, self.jobs_now);
                if let (Some(pipeline), Some(bind)) = (&self.horizon_pipeline, &self.horizon_bind) {
                    pass.set_pipeline(pipeline);
                    pass.set_bind_group(2, bind, &[]);
                    pass.dispatch_workgroups(groups, groups, self.jobs_now);
                }
            }
        }
        encoder.clear_buffer(&self.feedback_buffer, 0, None);
    }

    /// Feedback reduction and the copy out for the CPU, after the colour pass.
    pub(crate) fn encode_after(&self, encoder: &mut wgpu::CommandEncoder) {
        let _span = tracing::trace_span!("terrain_vt_reduce").entered();
        encoder.clear_buffer(&self.requests_buffer, 0, None);
        // The whole allocation list: the CPU drops words without [`ALLOC_VALID`], so stale
        // data must not survive the frame.
        encoder.clear_buffer(&self.alloc_buffer, 0, None);
        {
            let layout = self.manager.layout();
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("terrain vt feedback"),
                timestamp_writes: self
                    .queries
                    .as_ref()
                    .map(|(qs, _)| wgpu::ComputePassTimestampWrites {
                        query_set: qs,
                        beginning_of_pass_write_index: Some(2),
                        end_of_pass_write_index: Some(3),
                    }),
            });
            pass.set_bind_group(0, &self.reduce_bind, &[]);
            pass.set_pipeline(&self.requests_pipeline);
            let groups = layout.side0.div_ceil(8);
            pass.dispatch_workgroups(groups, groups, 1);
            pass.set_pipeline(&self.allocate_pipeline);
            // 2-D past the 65 535-group limit (`ALLOC_ROW_GROUPS` in vt.wgsl).
            let groups = layout.total.div_ceil(64);
            pass.dispatch_workgroups(groups.min(ALLOC_ROW_GROUPS), groups.div_ceil(ALLOC_ROW_GROUPS), 1);
        }
        let alloc_bytes = u64::from(ALLOC_HEADER + 2 * self.alloc_capacity) * 4;
        if let Some((qs, resolve)) = &self.queries {
            encoder.resolve_query_set(qs, 0..STAMPS, resolve, 0);
        }
        if let Some(i) = self.copy_slot
            && let Some(slot) = self.ring.get(i)
        {
            encoder.copy_buffer_to_buffer(&self.alloc_buffer, 0, &slot.buffer, 0, alloc_bytes);
            if let Some((_, resolve)) = &self.queries {
                encoder.copy_buffer_to_buffer(resolve, 0, &slot.buffer, alloc_bytes, u64::from(STAMPS) * 8);
            }
            self.copied.store(true, Ordering::Release);
        }
    }
}

/// What [`TerrainVtHandle`] reads: the atlas and the residency as of the last frame.
struct VtSnapshot {
    device: wgpu::Device,
    queue: wgpu::Queue,
    atlas: [wgpu::Texture; 4],
    atlas_e: wgpu::Texture,
    settings: VirtualTextureSettings,
    layout: VtLayout,
    slots: HashMap<u32, u32>,
    stats: VtStats,
}

/// [`TerrainVtHandle`]'s shared state.
#[derive(Default)]
struct VtHandleState {
    /// A terrain draw owns the handle.
    linked: bool,
    /// The settings in effect (`None` = off).
    settings: Option<VirtualTextureSettings>,
    /// How the device tests depth for the texture ([`VtDepthTest`]).
    depth_test: Option<VtDepthTest>,
    /// A change waiting for the draw's next frame (`Some(None)` turns the texture off).
    requested: Option<Option<VirtualTextureSettings>>,
    /// Why the last change was refused.
    error: Option<String>,
    snapshot: Option<VtSnapshot>,
}

/// The terrain virtual texture's handle, reachable after the draw is boxed into the renderer
/// ([`crate::TerrainSharedDraw::vt_handle`]): settings, counters, the page grid, the resident
/// pages and a blocking page readback (tests, the agent protocol's `terrain.vt`), and a
/// settings request the draw applies on its next frame (`terrain.set_vt`). Insert it as a
/// resource to expose it to agents.
#[derive(Resource, Clone, Default)]
pub struct TerrainVtHandle {
    inner: Arc<std::sync::Mutex<VtHandleState>>,
}

impl std::fmt::Debug for TerrainVtHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerrainVtHandle")
            .field("settings", &self.settings())
            .field("stats", &self.stats())
            .finish()
    }
}

impl TerrainVtHandle {
    fn lock(&self) -> std::sync::MutexGuard<'_, VtHandleState> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn with<R>(&self, f: impl FnOnce(&VtSnapshot) -> R) -> Option<R> {
        self.lock().snapshot.as_ref().map(f)
    }

    pub(crate) fn linked(depth_test: VtDepthTest) -> Self {
        let handle = Self::default();
        {
            let mut state = handle.lock();
            state.linked = true;
            state.depth_test = Some(depth_test);
        }
        handle
    }

    pub(crate) fn set_settings(&self, settings: Option<VirtualTextureSettings>) {
        let mut state = self.lock();
        state.settings = settings;
        if settings.is_none() {
            state.snapshot = None;
        }
    }

    pub(crate) fn set_error(&self, error: Option<String>) {
        self.lock().error = error;
    }

    pub(crate) fn take_request(&self) -> Option<Option<VirtualTextureSettings>> {
        self.lock().requested.take()
    }

    /// Asks the draw to switch the texture on with `settings` (or off with `None`) on its
    /// next frame. Invalid settings are refused then ([`Self::error`]).
    pub fn request(&self, settings: Option<VirtualTextureSettings>) {
        self.lock().requested = Some(settings);
    }

    /// A terrain draw owns this handle.
    #[must_use]
    pub fn is_linked(&self) -> bool {
        self.lock().linked
    }

    /// The device can run the texture with the current settings: forced early depth tests, or
    /// the late-depth path when the settings carry
    /// [`VirtualTextureSettings::late_depth_fallback`] ([`Self::depth_test`], ADR-2122). A
    /// late-depth device without that setting reads `false`, as it did before the path existed.
    #[must_use]
    pub fn is_supported(&self) -> bool {
        let state = self.lock();
        state
            .depth_test
            .is_some_and(|depth| depth.supports(state.settings))
    }

    /// How the linked draw's device tests depth for the texture (`Unsupported` when no draw
    /// owns the handle).
    #[must_use]
    pub fn depth_test(&self) -> VtDepthTest {
        self.lock().depth_test.unwrap_or(VtDepthTest::Unsupported)
    }

    /// A request is waiting for the draw's next frame.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.lock().requested.is_some()
    }

    /// Why the last request was refused.
    #[must_use]
    pub fn error(&self) -> Option<String> {
        self.lock().error.clone()
    }

    /// The settings in effect (`None` = off).
    #[must_use]
    pub fn settings(&self) -> Option<VirtualTextureSettings> {
        self.lock().settings
    }

    /// The page grid.
    #[must_use]
    pub fn layout(&self) -> Option<VtLayout> {
        self.with(|s| s.layout)
    }

    /// Counters of the last published frame.
    #[must_use]
    pub fn stats(&self) -> Option<VtStats> {
        self.with(|s| s.stats)
    }

    /// Resident pages, `(lod, x, y)`, sorted.
    #[must_use]
    pub fn resident_pages(&self) -> Vec<(u32, u32, u32)> {
        let mut pages = self
            .with(|s| s.slots.keys().map(|k| unpack_page(*k)).collect::<Vec<_>>())
            .unwrap_or_default();
        pages.sort_unstable();
        pages
    }

    /// Reads one resident page's slot back from the four atlas maps (border included,
    /// row-major, `(page_texels + 2)²` texels each) as `f32` RGBA. Blocks on the GPU.
    ///
    /// # Errors
    ///
    /// [`TerrainError::VirtualTexture`] when the texture has not run, the page is not
    /// resident or the readback fails.
    pub fn read_page(&self, lod: u32, x: u32, y: u32) -> Result<[Vec<[f32; 4]>; 4], TerrainError> {
        let maps = self.read_maps(lod, x, y, false)?;
        let mut out: [Vec<[f32; 4]>; 4] = Default::default();
        for (o, m) in out.iter_mut().zip(maps) {
            *o = m;
        }
        Ok(out)
    }

    /// Reads one resident page's slot back from map E, the horizon's per-sector slopes of the
    /// whole-surface mode (layout as [`Self::read_page`]; −1000 = not computed). Blocks on
    /// the GPU.
    ///
    /// # Errors
    ///
    /// As [`Self::read_page`], and when the texture runs a split mode (no map E).
    pub fn read_page_horizon(&self, lod: u32, x: u32, y: u32) -> Result<Vec<[f32; 4]>, TerrainError> {
        self.read_maps(lod, x, y, true)?
            .pop()
            .ok_or_else(|| TerrainError::VirtualTexture("readback returned no map".into()))
    }

    fn read_maps(&self, lod: u32, x: u32, y: u32, horizon: bool) -> Result<Vec<Vec<[f32; 4]>>, TerrainError> {
        let snap = self
            .with(|s| {
                (
                    s.device.clone(),
                    s.queue.clone(),
                    if horizon {
                        vec![s.atlas_e.clone()]
                    } else {
                        s.atlas.to_vec()
                    },
                    s.settings,
                    s.slots.get(&pack_page(lod, x, y)).copied(),
                )
            })
            .ok_or_else(|| TerrainError::VirtualTexture("the virtual texture has not run".into()))?;
        let (device, queue, textures, settings, slot) = snap;
        if horizon && settings.cache != VtCache::Surface {
            return Err(TerrainError::VirtualTexture(
                "map E exists only in the whole-surface mode".into(),
            ));
        }
        let slot = slot
            .ok_or_else(|| TerrainError::VirtualTexture(format!("page ({lod}, {x}, {y}) is not resident")))?;
        read_slot(&device, &queue, &textures, &settings, slot)
    }
}

/// Blocking readback of atlas slot `slot` from each of `atlas` (RGBA16F or RGBA8 maps).
fn read_slot(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    atlas: &[wgpu::Texture],
    settings: &VirtualTextureSettings,
    slot: u32,
) -> Result<Vec<Vec<[f32; 4]>>, TerrainError> {
    let side = settings.page_texels + 2;
    let slots = settings.atlas_slots;
    let origin = wgpu::Origin3d {
        x: (slot % slots) * side,
        y: (slot / slots) * side,
        z: 0,
    };
    let mut out = Vec::with_capacity(atlas.len());
    for texture in atlas {
        let bpp: u32 = if texture.format() == wgpu::TextureFormat::Rgba16Float {
            8
        } else {
            4
        };
        let row = (side * bpp).div_ceil(256) * 256;
        let staging = buffer(
            device,
            "terrain vt page readback",
            u64::from(row) * u64::from(side),
            wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        );
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("terrain vt page readback"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(side),
                },
            },
            wgpu::Extent3d {
                width: side,
                height: side,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(Some(encoder.finish()));
        let slice = staging.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| TerrainError::VirtualTexture(format!("readback poll: {e}")))?;
        match rx.recv() {
            Ok(Ok(())) => {}
            _ => return Err(TerrainError::VirtualTexture("readback map failed".into())),
        }
        let view = slice
            .get_mapped_range()
            .map_err(|e| TerrainError::VirtualTexture(format!("readback range: {e}")))?;
        let mut texels = Vec::with_capacity((side * side) as usize);
        for r in 0..side {
            let line = &view[(r * row) as usize..(r * row + side * bpp) as usize];
            if bpp == 8 {
                for t in line.as_chunks::<8>().0 {
                    let h = |k: usize| f16_to_f32(u16::from_le_bytes([t[2 * k], t[2 * k + 1]]));
                    texels.push([h(0), h(1), h(2), h(3)]);
                }
            } else {
                for t in line.as_chunks::<4>().0 {
                    texels.push(t.map(|v| f32::from(v) / 255.0));
                }
            }
        }
        drop(view);
        staging.unmap();
        out.push(texels);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(slots: u32) -> VirtualTextureSettings {
        VirtualTextureSettings {
            atlas_slots: slots,
            bakes_per_frame: 64,
            rebakes_per_frame: 4,
            ..VirtualTextureSettings::default()
        }
    }

    #[test]
    fn layout_covers_the_map_with_a_power_of_two_page_grid() {
        let l = VtLayout::new(1280.0, &VirtualTextureSettings::default()).expect("layout");
        // 126 × 2 cm = 2.52 m pages: 508 needed, 512 a side.
        assert_eq!(l.side0, 512);
        assert_eq!(l.lods, 10);
        assert_eq!(l.offsets[1], 512 * 512);
        assert_eq!(l.total, (0..10).map(|k| (512u32 >> k).pow(2)).sum::<u32>());
        let (x0, z0, size) = l.page_rect(3, 2, 5);
        assert!((size - 126.0 * 0.16).abs() < 1e-3);
        assert!((x0 - (-640.0 + 2.0 * size)).abs() < 1e-3 && (z0 - (-640.0 + 5.0 * size)).abs() < 1e-3);
        assert!(VtLayout::new(1_000_000.0, &VirtualTextureSettings::default()).is_err());
    }

    #[test]
    fn neighbouring_pages_share_their_border_texels() {
        // Crack-free borders: the last payload texel of a page and the right border texel of
        // its left neighbour are the same XZ, so both bake the same value.
        let l = VtLayout::new(1280.0, &VirtualTextureSettings::default()).expect("layout");
        for lod in [0, 4, 9] {
            let p = l.page_texels;
            let side = l.side(lod);
            if side < 2 {
                continue;
            }
            for j in 0..p + 2 {
                // Left page's border texel (i = p + 1) = right page's first payload texel (i = 1).
                let a = l.texel_xz(lod, 0, 0, p + 1, j);
                let b = l.texel_xz(lod, 1, 0, 1, j);
                assert!((a[0] - b[0]).abs() < 1e-3 && (a[1] - b[1]).abs() < 1e-3);
                // And the right page's left border (i = 0) is the left page's last payload texel.
                let c = l.texel_xz(lod, 1, 0, 0, j);
                let e = l.texel_xz(lod, 0, 0, p, j);
                assert!((c[0] - e[0]).abs() < 1e-3 && (c[1] - e[1]).abs() < 1e-3);
            }
        }
    }

    #[test]
    fn half_floats_decode() {
        assert_eq!(f16_to_f32(0x3c00), 1.0);
        assert_eq!(f16_to_f32(0xc000), -2.0);
        assert_eq!(f16_to_f32(0x3555), 0.333_251_95);
        assert_eq!(f16_to_f32(0x0001), 2f32.powi(-24));
        assert!(f16_to_f32(0x7e00).is_nan());
    }

    #[test]
    fn under_capacity_requests_survive_regardless_of_position() {
        // A scattered view: few pages with huge locals on a big map. The list must hand
        // back every one of them while the count fits — a rank-by-index scheme dropped
        // pages whose index passed the capacity even with the list nearly empty.
        let l = VtLayout::new(1280.0, &settings(4)).expect("layout");
        let capacity = 1024; // (4² * 4).clamp(1024, 65536), `VtGpu::new`'s formula
        let requested: Vec<u32> = (0..29)
            .map(|i| pack_page(0, (i * 173) % 512, (i * 311) % 512))
            .collect();
        let mut expected = requested.clone();
        expected.sort_unstable();
        assert_eq!(allocate_pages(&l, &requested, capacity), expected);
        let mut reversed = requested;
        reversed.reverse();
        assert_eq!(allocate_pages(&l, &reversed, capacity), expected);
    }

    #[test]
    fn over_capacity_requests_are_insertion_order_independent() {
        // Determinism fix (audit: the old arrival-order atomic kept whichever pages a GPU
        // scheduling race happened to store first): over capacity the surviving set must
        // be a pure function of the request set. The CPU sorts the list, so only the set
        // is asserted, not an order.
        let l = VtLayout::new(1280.0, &settings(4)).expect("layout");
        let capacity = 1024;
        let mut requested: Vec<u32> = Vec::new();
        for lod in 2..=9u32 {
            let side = l.side(lod);
            for y in 0..side {
                for x in 0..side {
                    requested.push(pack_page(lod, x, y));
                }
            }
        }
        for i in 0..2048u32 {
            requested.push(pack_page(0, (i * 5) % 512, ((i * 11) / 512) % 512));
        }
        requested.sort_unstable();
        requested.dedup();
        assert!(requested.len() as u32 > capacity);
        let expected = allocate_pages(&l, &requested, capacity);
        assert!(expected.len() <= capacity as usize);
        assert!(expected.len() < requested.len(), "the overflow is dropped");
        for key in &expected {
            assert!(requested.contains(key));
        }
        let mut reversed = requested.clone();
        reversed.reverse();
        assert_eq!(allocate_pages(&l, &reversed, capacity), expected);
        let mut interleaved: Vec<u32> = Vec::with_capacity(requested.len());
        let (front, back) = requested.split_at(requested.len() / 2);
        for (a, b) in front.iter().zip(back) {
            interleaved.push(*a);
            interleaved.push(*b);
        }
        interleaved.extend_from_slice(&back[front.len()..]);
        assert_eq!(allocate_pages(&l, &interleaved, capacity), expected);
    }

    #[test]
    fn overflow_slot_collisions_resolve_coarse_first() {
        // At a colliding overflow slot the maximum packed key wins, and a coarser page's
        // key is larger than a finer one's: the coarse page is kept, deterministically. The
        // colliding fine page is found by scanning the hash: the old `(local * constant) %
        // capacity` collided locals a multiple of the capacity apart; the avalanche
        // finalizer spreads those, so the test searches for whatever collides today.
        let l = VtLayout::new(1280.0, &settings(4)).expect("layout");
        let capacity = 1024;
        let coarse = pack_page(9, 0, 0);
        let coarse_slot = hash_overflow_slot(coarse | ALLOC_VALID, capacity);
        let colliding = (0..512u32)
            .flat_map(|y| (0..512u32).map(move |x| (x, y)))
            .find(|&(x, y)| {
                (x != 0 || y != 0)
                    && hash_overflow_slot(pack_page(0, x, y) | ALLOC_VALID, capacity) == coarse_slot
            })
            .map(|(x, y)| pack_page(0, x, y))
            .expect("262144 fine pages over 1024 slots: one must collide with the coarse page");
        let mut requested: Vec<u32> = vec![coarse, colliding];
        for i in 0..2048u32 {
            requested.push(pack_page(0, i % 512, (i / 512) * 13 % 512));
        }
        requested.sort_unstable();
        requested.dedup();
        assert!(requested.len() as u32 > capacity);
        let kept = allocate_pages(&l, &requested, capacity);
        assert!(kept.contains(&coarse), "the coarse page lost its slot");
        assert!(
            !kept.contains(&colliding),
            "the colliding fine page must lose to the coarse one"
        );
    }

    #[test]
    fn overflow_hash_spreads_a_coherent_window() {
        // Muse review of fix-terrain1-determinism (finding 1): a coherent 256x256 LOD-0
        // window (65,536 requests over capacity 4096) must use most of the overflow slots,
        // with survivors spread over the whole window. The old
        // `(local * 2654435761) % capacity` used only 2,048 of 4,096 slots here — the modulo
        // kept just the low bits — and every survivor had y in 248..=255, so the resident set
        // converged to a strip and starved the rest of the map for as long as the burst.
        let l = VtLayout::new(1280.0, &settings(4)).expect("layout");
        assert_eq!(l.side(0), 512, "the audit's scenario: a 512-side LOD-0 grid");
        let capacity = 4096;
        let requested: Vec<u32> = (0..256u32)
            .flat_map(|y| (0..256u32).map(move |x| pack_page(0, x, y)))
            .collect();
        assert_eq!(requested.len(), 65_536);
        assert!(requested.len() as u32 > capacity);
        let kept = allocate_pages(&l, &requested, capacity);
        assert!(
            kept.len() >= 3500,
            "only {} of {capacity} overflow slots used: the hash still clusters",
            kept.len()
        );
        let rows: std::collections::BTreeSet<u32> = kept.iter().map(|&key| unpack_page(key).2).collect();
        assert!(
            rows.len() >= 64,
            "survivors confined to {} rows of 256: the hash still starves the map",
            rows.len()
        );
        // rev-kimi-19 finding 3: the slot's winner must not depend on where the page sits.
        // atomicMax on the raw packed key preferred the largest y, then x (3,550 of 4,096
        // survivors in rows 224..256, 2 below row 128); the stored priority now permutes the
        // position bits, so every 32-row band holds at least 1/16 of the survivors (an even
        // spread is 1/8).
        let mut bands = [0usize; 8];
        for &key in &kept {
            bands[(unpack_page(key).2 / 32) as usize] += 1;
        }
        for (band, &n) in bands.iter().enumerate() {
            assert!(
                n * 16 >= kept.len(),
                "rows {}..{} hold {n} of {} survivors (bands {bands:?}): the overflow priority \
                 still prefers part of the window",
                band * 32,
                band * 32 + 32,
                kept.len()
            );
        }
        for key in &kept {
            assert!(requested.contains(key));
        }
    }

    #[test]
    fn parse_alloc_words_picks_the_region_and_strips_markers() {
        // The read-back parser (Muse review finding 2): the arrival region is authoritative
        // while the count fits — count == capacity is still complete; one past it, the hashed
        // overflow region is. Holes (words without the marker) drop out; the marker is
        // stripped; a truncated buffer parses to nothing instead of panicking.
        let marked = alloc_word;
        let mut words = vec![0u32; ALLOC_HEADER as usize + 2 * 8];
        words[0] = 8;
        words[1] = 3; // missed
        words[2] = 9; // hit
        for (i, w) in words
            .iter_mut()
            .enumerate()
            .take(ALLOC_HEADER as usize + 8)
            .skip(ALLOC_HEADER as usize)
        {
            *w = marked(100 + (i - ALLOC_HEADER as usize) as u32);
        }
        // Overflow-region decoys must not leak into an under-capacity parse.
        words[ALLOC_HEADER as usize + 8] = marked(999);
        let (keys, misses, hits) = parse_alloc_words(&words, 8);
        assert_eq!(keys, (100..108).collect::<Vec<_>>());
        assert_eq!((misses, hits), (3, 9));

        // count == capacity: still the arrival region.
        let mut words = vec![0u32; ALLOC_HEADER as usize + 2 * 8];
        words[0] = 8;
        words[ALLOC_HEADER as usize] = marked(1);
        let (keys, _, _) = parse_alloc_words(&words, 8);
        assert_eq!(keys, vec![1]);

        // count == capacity + 1: the hashed region decides; holes drop; markers strip.
        let mut words = vec![0u32; ALLOC_HEADER as usize + 2 * 8];
        words[0] = 9;
        words[ALLOC_HEADER as usize] = marked(1); // arrival decoy
        words[ALLOC_HEADER as usize + 8] = marked(7);
        words[ALLOC_HEADER as usize + 9] = 0; // hole
        words[ALLOC_HEADER as usize + 10] = marked(8);
        let (keys, _, _) = parse_alloc_words(&words, 8);
        assert_eq!(keys, vec![7, 8]);

        // Truncated buffer: no panic, nothing parsed.
        let (keys, misses, hits) = parse_alloc_words(&[5], 8);
        assert!(keys.is_empty());
        assert_eq!((misses, hits), (0, 0));
    }

    #[test]
    fn wgsl_mirror_uses_the_same_overflow_hash_and_markers() {
        // The GPU runs the WGSL twin of `hash_overflow_slot` and writes the words
        // `parse_alloc_words` reads; this binds the two implementations so a drift of either
        // (finalizer constants, region offsets, marker bit, key packing) fails here, on the
        // CPU, without a GPU run (Muse review of fix-terrain1-determinism finding 2).
        let wgsl = include_str!("vt.wgsl");
        for needle in [
            "0x7feb352d",
            "0x846ca68b",
            "hash_overflow_slot(key, params.dims.z)",
            "ALLOC_HEADER + params.dims.z + h], word)",
            "atomicStore(&allocation[ALLOC_HEADER + n], word)",
            "ALLOC_VALID | (lod << 26u) | (y << 13u) | x",
            "ALLOC_VALID | (lod << 26u) | perm26((y << 13u) | x)",
            "(v * 0x2c1b3c6du) & 0x3ffffffu",
            "p = p ^ (p >> 13u)",
            "(p * 0x297a2d39u) & 0x3ffffffu",
            "return p ^ (p >> 13u)",
        ] {
            assert!(
                wgsl.contains(needle),
                "vt.wgsl lost `{needle}`: the WGSL/Rust twins drifted"
            );
        }
        assert_eq!(ALLOC_HEADER, 4);
        assert_eq!(ALLOC_VALID, 1 << 31);
        assert_eq!(PERM26_MUL, [0x2c1b_3c6d, 0x297a_2d39]);
        assert_eq!(ALLOC_POS_MASK, 0x3ff_ffff);
    }

    #[test]
    fn alloc_words_round_trip_and_keep_the_lod_first() {
        // rev-kimi-19 finding 3: the stored word permutes the position bits for the overflow
        // priority; the CPU must decode every word back to its key, the permutation must be a
        // bijection (no two pages share a word), and a coarser page's word must still compare
        // larger than any finer page's.
        for (i, &m) in PERM26_MUL.iter().enumerate() {
            assert_eq!(m.wrapping_mul(PERM26_INV[i]), 1, "multiplier {i} has no inverse");
        }
        let mut seen = std::collections::HashSet::new();
        for y in 0..512u32 {
            for x in 0..512u32 {
                let p = perm26((y << 13) | x);
                assert!(p <= ALLOC_POS_MASK);
                assert_eq!(unperm26(p), (y << 13) | x, "perm26 is not inverted at ({x}, {y})");
                assert!(seen.insert(p), "perm26 collides at ({x}, {y})");
            }
        }
        // The extremes of the 26-bit range too (x, y up to 8191).
        for v in [
            0,
            1,
            ALLOC_POS_MASK,
            ALLOC_POS_MASK - 1,
            1 << 13,
            (1 << 13) - 1,
            0x2aa_aaaa,
        ] {
            assert_eq!(unperm26(perm26(v)), v);
        }
        for (lod, x, y) in [
            (0, 0, 0),
            (9, 1, 1),
            (3, 511, 77),
            (0, 2047, 2047),
            (15, 8191, 8191),
        ] {
            let key = pack_page(lod, x, y);
            let word = alloc_word(key);
            assert_ne!(word & ALLOC_VALID, 0);
            assert_eq!((word >> 26) & 0xf, lod, "the LOD stays in the top bits");
            assert_eq!(alloc_key(word), key);
        }
        let coarse_min = (0..64u32).map(|v| alloc_word(pack_page(1, v, v))).min();
        let fine_max = (0..64u32).map(|v| alloc_word(pack_page(0, v * 3, v * 5))).max();
        assert!(coarse_min > fine_max, "a coarse page must outrank every fine one");
    }

    #[test]
    fn page_keys_round_trip() {
        for (lod, x, y) in [(0, 0, 0), (9, 1, 1), (3, 511, 77), (0, 2047, 2047)] {
            assert_eq!(unpack_page(pack_page(lod, x, y)), (lod, x, y));
        }
    }

    fn view(frame: u64, eye: [f32; 3], terms: &DistTerms) -> VtView<'_> {
        VtView {
            frame,
            eye,
            altitude: [0.0, 10.0],
            terms,
        }
    }

    #[test]
    fn requested_pages_bake_coarse_first_and_stay_resident() {
        let l = VtLayout::new(1280.0, &settings(4)).expect("layout");
        let mut m = VtPageManager::new(l, settings(4));
        let terms = DistTerms::default();
        let keys = [pack_page(0, 10, 10), pack_page(1, 5, 5), pack_page(9, 0, 0)];
        m.set_requests(1, &keys, 0, 10);
        let plan = m.plan(&view(3, [0.0, 5.0, 0.0], &terms));
        assert_eq!(plan.new_pages, 3);
        assert_eq!(plan.jobs[0][0], 9, "coarsest first");
        assert_eq!(plan.jobs[2][0], 0);
        assert_eq!(plan.table.len(), 3);
        assert!(plan.table.iter().all(|w| w[1] >= 1));
        assert_eq!(m.stats().missing, 0);
        // Same requests again: nothing to do.
        m.set_requests(2, &keys, 10, 0);
        let plan = m.plan(&view(4, [0.0, 5.0, 0.0], &terms));
        assert!(plan.jobs.is_empty() && plan.table.is_empty());
        assert_eq!(m.stats().resident, 3);
        assert!((m.stats().hit_rate - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_height_edit_rebakes_only_the_pages_it_reaches_and_in_place() {
        let l = VtLayout::new(1280.0, &settings(4)).expect("layout");
        let mut m = VtPageManager::new(l, settings(4));
        let terms = DistTerms::default();
        let (near, far) = (pack_page(0, 10, 10), pack_page(0, 30, 30));
        m.set_requests(1, &[near, far], 0, 10);
        let first = m.plan(&view(3, [0.0, 5.0, 0.0], &terms));
        assert_eq!(first.new_pages, 2);
        let slot_near = m.slot_of(0, 10, 10).expect("resident");
        // An edit inside the first page reaches it and not the second.
        let (x0, z0, size) = m.layout().page_rect(0, 10, 10);
        let marked = m.invalidate_rect(
            [x0 + size * 0.4, z0 + size * 0.4],
            [x0 + size * 0.6, z0 + size * 0.6],
        );
        assert_eq!(marked, 1);
        assert_eq!(m.dirty_pages(), 1);
        m.set_requests(2, &[near, far], 10, 0);
        let plan = m.plan(&view(4, [0.0, 5.0, 0.0], &terms));
        assert_eq!(
            plan.jobs,
            vec![[0, 10, 10, slot_near]],
            "re-baked into its own slot"
        );
        assert!(plan.table.is_empty(), "the page table is untouched");
        assert_eq!(plan.new_pages, 0);
        assert_eq!(m.stats().rebaked, 1);
        assert_eq!(m.dirty_pages(), 0);
        assert_eq!(m.stats().resident, 2);
        // Nothing left to do.
        let again = m.plan(&view(5, [0.0, 5.0, 0.0], &terms));
        assert!(again.jobs.is_empty());
    }

    #[test]
    fn a_dirty_page_nobody_asked_for_lately_is_dropped_not_rebaked() {
        let l = VtLayout::new(1280.0, &settings(4)).expect("layout");
        let mut m = VtPageManager::new(l, settings(4));
        let terms = DistTerms::default();
        let (a, b) = (pack_page(0, 10, 10), pack_page(0, 30, 30));
        m.set_requests(1, &[a, b], 0, 10);
        m.plan(&view(3, [0.0, 5.0, 0.0], &terms));
        let (x0, z0, size) = m.layout().page_rect(0, 10, 10);
        m.invalidate_rect(
            [x0 + size * 0.5, z0 + size * 0.5],
            [x0 + size * 0.5, z0 + size * 0.5],
        );
        // Only `b` is requested from now on, 100 frames later.
        m.set_requests(100, &[b], 10, 0);
        let plan = m.plan(&view(101, [0.0, 5.0, 0.0], &terms));
        assert!(plan.jobs.is_empty());
        assert_eq!(plan.table, vec![[m.layout().index(0, 10, 10), 0]]);
        assert!(m.slot_of(0, 10, 10).is_none());
        assert_eq!(m.dirty_pages(), 0);
    }

    /// Terrain1 correctness audit M2: a dirty page queued for its in-place re-bake kept its
    /// old `seen`, so the same `plan` could also evict it and hand its slot to a missing
    /// page — two jobs baking into one atlas slot in a single dispatch.
    #[test]
    fn dirty_page_is_not_rebaked_into_a_slot_reused_the_same_frame() {
        let l = VtLayout::new(1280.0, &settings(1)).expect("layout");
        let mut m = VtPageManager::new(l, settings(1));
        let terms = DistTerms::default();
        let p = pack_page(0, 10, 10);
        let missing = pack_page(0, 30, 30);
        m.set_requests(1, &[p], 0, 1);
        m.plan(&view(3, [0.0, 5.0, 0.0], &terms));
        // 30 frames later only the missing page is named: `p` is at the window's edge
        // (still re-baked, not dropped) and not named this frame (evictable — the bug).
        m.set_requests(31, &[missing], 0, 1);
        let (x0, z0, size) = m.layout().page_rect(0, 10, 10);
        assert_eq!(m.invalidate_rect([x0, z0], [x0 + size, z0 + size]), 1);
        let plan = m.plan(&view(32, [0.0, 5.0, 0.0], &terms));
        let mut slots = HashSet::new();
        for job in &plan.jobs {
            assert!(slots.insert(job[3]), "slot {} is baked twice in one plan", job[3]);
        }
        // The page table never names a page other than `p` as this slot's owner.
        let p_index = m.layout().index(0, 10, 10);
        assert!(
            plan.table.iter().all(|w| w[1] != 1 || w[0] == p_index),
            "the slot keeps its owner: {:?}",
            plan.table
        );
        assert_eq!(m.slot_of(0, 10, 10), Some(0));
        assert!(
            m.slot_of(0, 30, 30).is_none(),
            "the missing page waits for a free slot instead of sharing one"
        );
    }

    #[test]
    fn page_count_above_2_pow_31_is_an_error() {
        // The float-to-u32 cast saturates and `next_power_of_two` panics past 2^31, so the
        // page count must be checked before the cast (audit finding 4).
        assert!(VtLayout::new(1e10, &VirtualTextureSettings::default()).is_err());
        let tiny = VirtualTextureSettings {
            lod0_texel_m: 2e-7,
            page_texels: 8,
            ..VirtualTextureSettings::default()
        };
        assert!(VtLayout::new(4096.0, &tiny).is_err());
        // And the documented boundary still works.
        assert!(VtLayout::new(1_000_000.0, &VirtualTextureSettings::default()).is_err());
        assert!(VtLayout::new(1280.0, &VirtualTextureSettings::default()).is_ok());
    }

    #[test]
    fn a_flush_forgets_the_dirty_pages() {
        let l = VtLayout::new(1280.0, &settings(4)).expect("layout");
        let mut m = VtPageManager::new(l, settings(4));
        let terms = DistTerms::default();
        m.set_requests(1, &[pack_page(9, 0, 0)], 0, 1);
        m.plan(&view(3, [0.0, 5.0, 0.0], &terms));
        assert_eq!(m.invalidate_rect([-1e6; 2], [1e6; 2]), 1);
        m.flush();
        assert_eq!(m.dirty_pages(), 0);
    }

    #[test]
    fn eviction_takes_pages_the_latest_request_did_not_name() {
        let l = VtLayout::new(1280.0, &settings(2)).expect("layout");
        let mut m = VtPageManager::new(l, settings(2));
        let terms = DistTerms::default();
        let old: Vec<u32> = (0..4).map(|i| pack_page(0, i, 0)).collect();
        m.set_requests(1, &old, 0, 0);
        assert_eq!(m.plan(&view(3, [0.0; 3], &terms)).new_pages, 4);
        // Two old pages still wanted, two new ones: the two unnamed pages are evicted.
        let new = [old[0], old[1], pack_page(0, 9, 9), pack_page(0, 8, 8)];
        m.set_requests(5, &new, 0, 0);
        let plan = m.plan(&view(7, [0.0; 3], &terms));
        assert_eq!(plan.new_pages, 2);
        assert!(m.slot_of(0, 0, 0).is_some() && m.slot_of(0, 1, 0).is_some());
        assert!(m.slot_of(0, 2, 0).is_none() && m.slot_of(0, 3, 0).is_none());
        let evicted: Vec<u32> = plan.table.iter().filter(|w| w[1] == 0).map(|w| w[0]).collect();
        assert_eq!(evicted.len(), 2);
        // Over capacity with every page named: the overflow waits (shades in full).
        let five: Vec<u32> = (0..5).map(|i| pack_page(1, i, 1)).collect();
        m.set_requests(9, &five, 0, 0);
        let _ = m.plan(&view(11, [0.0; 3], &terms));
        m.set_requests(10, &five, 0, 0);
        let plan = m.plan(&view(12, [0.0; 3], &terms));
        assert!(plan.new_pages <= 1);
        assert_eq!(m.stats().missing, 1);
        assert!(m.stats().oldest_miss_frames >= 1);
    }

    #[test]
    fn a_held_camera_never_rebakes_and_a_moving_one_rebakes_banded_pages_first() {
        let l = VtLayout::new(1280.0, &settings(8)).expect("layout");
        let mut m = VtPageManager::new(l, settings(8));
        let terms = DistTerms::from_shader(
            [60.0, 220.0, 0.0, 0.0],
            [12.0, 0.5, 1.0, 1.0],
            [1.0, 1.0, 1.0, 0.0],
            [0.0; 4],
            true,
        );
        // Page near (inside the detail fade) and one far (beyond every band, under 2 km).
        let near = pack_page(4, 16, 15); // ~40 m pages around x≈0
        let far = pack_page(6, 6, 6);
        m.set_requests(1, &[near, far], 0, 0);
        let eye = [0.0, 30.0, 0.0];
        let _ = m.plan(&view(3, eye, &terms));
        m.set_requests(2, &[near, far], 0, 0);
        assert!(
            m.plan(&view(4, eye, &terms)).jobs.is_empty(),
            "held camera: exact pages, no re-bake"
        );
        // Move 2 m: the near page is in the 60–220 m fade band and goes stale.
        m.set_requests(3, &[near, far], 0, 0);
        let plan = m.plan(&view(5, [2.0, 30.0, 0.0], &terms));
        assert!(plan.jobs.iter().any(|j| pack_page(j[0], j[1], j[2]) == near));
        assert_eq!(m.stats().rebaked as usize, plan.jobs.len());
    }

    #[test]
    fn staleness_grows_with_motion_and_inside_bands() {
        let t = DistTerms::from_shader(
            [60.0, 220.0, 0.0, 0.0],
            [12.0, 0.5, 1.0, 0.0],
            [0.0; 4],
            [0.0; 4],
            false,
        );
        assert_eq!(t.score(100.0, 140.0, 0.0, 0.1), 0.0);
        let in_band = t.score(100.0, 140.0, 0.5, 0.1);
        let out_band = t.score(400.0, 440.0, 0.5, 0.1);
        assert!(in_band > out_band);
        assert!(t.score(100.0, 140.0, 1.0, 0.1) > in_band);
        // The 300 m relief cut-off: a page straddling it is stale after half a texel of motion.
        assert!(t.score(290.0, 310.0, 0.05, 0.1) >= 1.0);
    }

    #[test]
    fn settings_are_validated() {
        assert!(VirtualTextureSettings::default().validate().is_ok());
        let bad = VirtualTextureSettings {
            oversample: 0.0,
            ..VirtualTextureSettings::default()
        };
        assert!(bad.validate().is_err());
        // Whole-surface mode: four maps plus map E (the horizon slopes).
        assert_eq!(VirtualTextureSettings::default().atlas_bytes(), 4096 * 4096 * 40);
        assert_eq!(
            VirtualTextureSettings {
                cache: VtCache::Relief,
                ..VirtualTextureSettings::default()
            }
            .atlas_bytes(),
            4096 * 4096 * 32
        );
    }

    /// ADR-2122: forced early depth tests run any settings; without them (DX12, Metal) only
    /// `late_depth_fallback` settings run, and only where the fragment stage has room for the
    /// texture's bindings next to the terrain's (with or without local lights); fewer than
    /// five bind groups never run.
    #[test]
    fn the_depth_test_follows_the_device() {
        let early = wgpu::Features::SHADER_EARLY_DEPTH_TEST;
        let desktop = wgpu::Limits {
            max_bind_groups: 8,
            ..wgpu::Limits::default()
        };
        assert_eq!(VtDepthTest::of_device(&desktop, early), VtDepthTest::Early);
        assert_eq!(
            VtDepthTest::of_device(&desktop, wgpu::Features::empty()),
            VtDepthTest::Late
        );
        let four_groups = wgpu::Limits {
            max_bind_groups: 4,
            ..desktop.clone()
        };
        assert_eq!(
            VtDepthTest::of_device(&four_groups, early),
            VtDepthTest::Unsupported
        );
        // The mobile floor (8 storage, 12 uniform buffers per stage) fits lights plus texture
        // exactly; one storage buffer fewer keeps the lights and drops the late texture.
        let mut mobile = regia_render::MinspecProfile::Mobile.clamp_limits(&desktop);
        assert_eq!(
            VtDepthTest::of_device(&mobile, wgpu::Features::empty()),
            VtDepthTest::Late
        );
        mobile.max_storage_buffers_per_shader_stage = 7;
        assert!(crate::gpu::local_lights_fit(&mobile, wgpu::Features::empty()));
        assert_eq!(
            VtDepthTest::of_device(&mobile, wgpu::Features::empty()),
            VtDepthTest::Unsupported
        );

        let plain = VirtualTextureSettings::default();
        let late = VirtualTextureSettings {
            late_depth_fallback: true,
            ..plain
        };
        assert!(VtDepthTest::Early.runs(&plain) && VtDepthTest::Early.runs(&late));
        assert!(!VtDepthTest::Late.runs(&plain) && VtDepthTest::Late.runs(&late));
        assert!(!VtDepthTest::Unsupported.runs(&late));
        // `TerrainGpu::vt_supported` and `TerrainVtHandle::is_supported` use `supports`:
        // an early device is supported before settings exist; a late device only while
        // the current settings carry `late_depth_fallback`.
        assert!(VtDepthTest::Early.supports(None) && VtDepthTest::Early.supports(Some(plain)));
        assert!(!VtDepthTest::Late.supports(None) && !VtDepthTest::Late.supports(Some(plain)));
        assert!(VtDepthTest::Late.supports(Some(late)));
        assert!(!VtDepthTest::Unsupported.supports(Some(late)));
        assert_eq!(
            [VtDepthTest::Early, VtDepthTest::Late, VtDepthTest::Unsupported].map(VtDepthTest::name),
            ["early", "late", "unsupported"]
        );
    }

    /// ADR-2122: the fallback is off unless asked for (older settings without the field read as
    /// off); the render profile's settings stay the v0.1.2 ones (the profile's `vt_late_depth`
    /// bit asks for the fallback, not these settings).
    #[test]
    fn the_late_depth_fallback_is_opt_in() {
        assert!(!VirtualTextureSettings::default().late_depth_fallback);
        assert!(!VirtualTextureSettings::compact().late_depth_fallback);
        let old: VirtualTextureSettings =
            ron::from_str("(cache: surface, stochastic_lod: false)").expect("settings ron");
        assert!(!old.late_depth_fallback);
        let profile = VirtualTextureSettings::for_profile();
        assert!(!profile.late_depth_fallback && !profile.stochastic_lod);
        assert_eq!(
            VirtualTextureSettings {
                stochastic_lod: true,
                ..profile
            },
            VirtualTextureSettings::default()
        );
        assert_eq!(profile.atlas_bytes(), 671_088_640);
    }

    /// The colour shader's 24 m floor is the whole-surface cache only (`atlas.w == 0`).
    /// `select(max(near, 24), near, atlas.w > 0.5)` keeps `near` for lighting and relief.
    #[test]
    fn the_24m_near_floor_is_whole_surface_only() {
        let shader = include_str!("terrain.wgsl");
        assert!(
            shader.contains("let near_m = select(max(vt.view.y, 24.0), vt.view.y, vt.atlas.w > 0.5);"),
            "colour shader near floor"
        );
        assert_eq!(VtCache::Surface.code(), 0.0);
        assert!(VtCache::Lighting.code() > 0.5 && VtCache::Relief.code() > 0.5);
        let surface = 8.0_f32.max(24.0);
        let split = 8.0_f32;
        assert!(10.0 < surface, "a 10 m pixel is inside the surface floor");
        assert!(10.0 > split, "lighting and relief serve a 10 m pixel at near_m 8");
    }

    /// A second paint of the same selection key must not rebuild or allocate the node map.
    #[test]
    fn held_camera_does_not_rebuild_the_vt_node_map() {
        let mut map = NodeMapCpu::new(4);
        let nodes = [(crate::cdlod::QuadNode { level: 1, x: 0, z: 0 }, 3u32)];
        assert!(map.rebuild(7, 3, nodes.into_iter()));
        assert_eq!(map.rebuilds, 1);
        let allocs = map.allocs;
        assert!(
            !map.rebuild(7, 3, nodes.into_iter()),
            "held selection is unchanged"
        );
        assert_eq!(map.rebuilds, 1, "held camera repainted the node map");
        assert_eq!(map.allocs, allocs, "held camera allocated another node map");
        // Same coverage under a new key is painted (the iterator runs) but not uploaded.
        assert!(
            !map.rebuild(8, 3, nodes.into_iter()),
            "identical coverage still uploaded"
        );
        assert_eq!(map.rebuilds, 2, "a new key did not paint");
        let changed = [(crate::cdlod::QuadNode { level: 0, x: 1, z: 1 }, 1u32)];
        assert!(
            map.rebuild(9, 1, changed.into_iter()),
            "a different coverage did not upload"
        );
        assert_eq!(map.rebuilds, 3);
    }

    /// The node map stores node-buffer slots. A held camera selection whose base slot moves
    /// (the shadow cascades ahead of it reselected) must repaint: the old slots now name
    /// cascade nodes.
    #[test]
    fn a_moved_node_base_repaints_the_vt_node_map() {
        let mut map = NodeMapCpu::new(4);
        let node = crate::cdlod::QuadNode { level: 2, x: 0, z: 0 };
        assert!(map.rebuild(7, 0, [(node, 0u32)].into_iter()));
        assert_eq!(map.cpu[0], 1);
        assert!(
            map.rebuild(7, 10, [(node, 10u32)].into_iter()),
            "a moved base under the same selection key kept the stale slots"
        );
        assert_eq!(map.cpu[0], 11, "the map still names the old slot");
        assert!(map.cpu.iter().all(|&c| c == 11));
    }

    /// A page that drops out of the 30-frame request window is not scored while the eye
    /// moves. When it is requested again under a held eye it must be scored: the settled
    /// walk never saw it, and its distance terms are from an older eye.
    #[test]
    fn a_page_requested_again_under_a_held_eye_is_rescored() {
        let l = VtLayout::new(1280.0, &settings(8)).expect("layout");
        let mut m = VtPageManager::new(l, settings(8));
        let terms = DistTerms::from_shader(
            [60.0, 220.0, 0.0, 0.0],
            [12.0, 0.5, 1.0, 1.0],
            [1.0, 1.0, 1.0, 0.0],
            [0.0; 4],
            true,
        );
        let near = pack_page(4, 16, 15);
        let far = pack_page(6, 6, 6);
        let e0 = [0.0, 30.0, 0.0];
        let e1 = [2.0, 30.0, 0.0];
        m.set_requests(1, &[near], 0, 0);
        let _ = m.plan(&view(3, e0, &terms));
        m.set_requests(40, &[far], 0, 0);
        let _ = m.plan(&view(41, e0, &terms));
        // The eye moves while `near` is outside the window, then settles.
        m.set_requests(42, &[far], 0, 0);
        let _ = m.plan(&view(43, e1, &terms));
        m.set_requests(44, &[far], 0, 0);
        let _ = m.plan(&view(45, e1, &terms));
        m.set_requests(46, &[far], 0, 0);
        assert!(m.plan(&view(47, e1, &terms)).jobs.is_empty(), "the eye settled");
        // The camera turns back to `near` without moving.
        m.set_requests(48, &[near, far], 0, 0);
        let plan = m.plan(&view(49, e1, &terms));
        assert!(
            plan.jobs.iter().any(|j| pack_page(j[0], j[1], j[2]) == near),
            "the returning page kept its old-eye bake: {:?}",
            plan.jobs
        );
    }

    /// Once rebakes have caught up, another plan at the same eye must not rescore residents
    /// or rebuild scratch, and a rebake that does not move slots must not republish them.
    #[test]
    fn a_settled_cache_does_not_rescore_or_republish() {
        let l = VtLayout::new(1280.0, &settings(8)).expect("layout");
        let mut m = VtPageManager::new(l, settings(8));
        let terms = DistTerms::from_shader(
            [60.0, 220.0, 0.0, 0.0],
            [12.0, 0.5, 1.0, 1.0],
            [1.0, 1.0, 1.0, 0.0],
            [0.0; 4],
            true,
        );
        let near = pack_page(4, 16, 15);
        let far = pack_page(6, 6, 6);
        let eye = [0.0, 30.0, 0.0];
        m.set_requests(1, &[near, far], 0, 0);
        let _ = m.plan(&view(3, eye, &terms));
        m.mark_published();
        let epoch = m.resident_epoch;
        m.set_requests(2, &[near, far], 0, 0);
        assert!(m.plan(&view(4, eye, &terms)).jobs.is_empty());
        let walks = m.resident_walks;
        let allocs = m.scratch_allocs;
        let reuses = m.scratch_reuses;
        let again = m.plan(&view(5, eye, &terms));
        assert!(again.jobs.is_empty());
        assert_eq!(m.resident_walks, walks, "settled eye rescored residents");
        assert_eq!(m.scratch_allocs, allocs, "settled eye rebuilt scratch");
        assert!(m.scratch_reuses > reuses, "settled eye did not reuse scratch");
        assert_eq!(m.resident_epoch, epoch);
        assert!(!m.slots_need_republish(false));
        m.set_requests(6, &[near, far], 0, 0);
        let moved = m.plan(&view(7, [2.0, 30.0, 0.0], &terms));
        assert!(!moved.jobs.is_empty(), "a 2 m move still rebakes");
        assert_eq!(
            m.resident_epoch, epoch,
            "a rebake must not change residency slots"
        );
        assert!(
            !m.slots_need_republish(false),
            "rebake jobs republished an unchanged slot table"
        );
    }
}
