// SPDX-License-Identifier: MPL-2.0
//! Baked diffuse GI at runtime (baked lighting phase 2, `docs/research/strategic-review-20261012.md`
//! § 3): a [`BakedGiVolume`] (`.fgiv`, baked by `regia bake-gi`) installed on the renderer
//! ([`crate::FrameRenderer::set_baked_gi`]) lights every surface the screen sees.
//!
//! - **Load**: the volume's file sections (meta, SH, moments) go to one storage buffer unchanged,
//!   straight from the asset's bytes into the mapped buffer, with the ground heights and a brick
//!   table after them. Nothing is decoded on the CPU.
//! - **Lookup**: `static_gi(p, n, v)` (`shaders/baked_gi.wgsl`; mirrored here by
//!   [`sample_static_gi`]): the terrain layer bilinearly over four columns and linearly between
//!   the two levels around the height above the ground, a detail brick overriding it inside its
//!   box; per corner the DDGI weights (normal and view bias, wrap, Chebyshev visibility from the
//!   baked distance moments, trilinear).
//! - **Shading**: a half-resolution compute pass after the depth prepass writes the lookup in the
//!   layout of the DDGI screen lookup, so the forward pass (diffuse, rough specular, the probe/sky
//!   specular-occlusion ratio) and the terrain's `engine_gi` read it unchanged. Its fill component
//!   (sky-body planet-shine, shadowed by the bake) replaces the forward and terrain passes' live,
//!   unshadowed fill where it covers.
//! - **Policy** ([`crate::FrameRenderer::render_into`]): while a volume lights the frame, DDGI and
//!   SSGI are off and full-resolution GTAO drops to half resolution; the sun, its shadows and the
//!   paged shadow cache are unchanged (direct light stays live).
//!
//! Each component is linear in its light: [`component_scales`] rescales the baked light to the
//! frame's (the sun and fill follow the frame's lights, sky and emissive take
//! [`BakedGiSettings`]), so a look change after the bake still applies.
//!
//! **Baked emitters** (ADR-1386): a `.fgiv` v3 volume names the emitters its emissive component
//! holds ([`regia_asset::baked_gi::BakedEmitter`]). The renderer flags the fitted emissive area
//! lights of those emitters ([`BakedEmitterSet`]), and the forward pass takes a flagged light's
//! diffuse back out where the volume covers the receiver (and its specular where the probes stand
//! in for the rough lobe), so the emitter is counted once. Emitters the bake did not trace keep
//! their area lights everywhere.
//!
//! **Near field** (ADR-281, ADR-282): an interior brick (`.fgiv` v2,
//! [`regia_asset::baked_gi::BrickFlags::INTERIOR`]) keeps the terrain layer's outdoor light
//! out of an enclosed room, and capsule occluders for dynamic units
//! ([`crate::FrameRenderer::set_gi_occluders`], [`GiCapsule`]) darken the baked light next to
//! them in the screen pass.
//!
//! **Leak control** (ADR-366): a probe's flags may carry wall codes, set by the bake where the
//! segment to a grid neighbor crosses geometry, with the wall's position on it. A lookup cell
//! whose edges carry them splits into the groups of corners its unblocked edges join, and only
//! the group on the receiver's side of the wall lights it, so a thin wall keeps the light of its
//! far side out.
//!
//! **Quality pass** (ADR-861, each a [`BakedGiSettings`] switch, on by default):
//! [`BakedGiSettings::deringing`] evaluates a strongly directional L1 as a non-negative lobe with
//! the same mean and first moment, [`BakedGiSettings::leak_guard`] keeps wall codes on edges to
//! rejected probes from opening or moving a wall, and [`BakedGiSettings::sun_relight`] lets the
//! sun bounce of probes lit from below follow the sun's elevation ([`SunRelight`]).
//! **Several volumes** (ADR-1097, [`BakedGiLayer`],
//! [`crate::FrameRenderer::set_baked_gi_layers`]): adjacent tiles, a streamed level's bake beside
//! the map's own and a nested set piece blend in the same screen pass (`blend.rs`, mirrored by
//! [`sample_static_gi_blend`]), continuously across their seams.
//! **ZH3 from L1** (ADR-1377, [`BakedGiSettings::zh3`], opt-in): the summed SH is
//! evaluated with a hallucinated quadratic zonal harmonic along the luminance L1 direction
//! ([`zh3_irradiance_over_pi`]), from the same stored data.

use crate::frame_work::counted::*;
use std::sync::Arc;

use crate::bind_cache::BindCache;
use crate::ddgi::{DdgiScreenTexel, DdgiUniformGpu};
use crate::ddgi_gpu::ScreenParamsGpu;
use regia_asset::baked_gi::{
    BakedEmitter, BakedGiVolume, DetailBrick, GI_COMPONENTS, GiComponent, MOMENT_TEXEL_BYTES, ProbeFlags,
    ShRgb,
};

mod ao_split;
mod blend;
mod debug;
pub mod memory;
mod occluders;
pub(crate) mod points;
#[cfg(test)]
mod visibility_tests;
mod zh3;

pub use ao_split::{PROBE_AO_RESOLVE, ProbeAoSplit, SsaoSample, probe_ao_keep, terrain_resolve_distance};
pub use blend::{
    BakedGiFootprint, BakedGiLayer, MAX_BAKED_GI_VOLUMES, SplitFade, order_layers, sample_static_gi_blend,
    split_fades,
};
pub use debug::{
    CornerTrace, GiLayer, GiLookupTrace, ProbeDebugPoint, ProbeFilter, ProbeQuery, VolumeSummary,
    probe_debug_points, trace_static_gi, volume_bounds, volume_summary,
};
pub use occluders::{
    GiCapsule, MAX_GI_OCCLUDERS, capsule_list_key, capsule_visibility, capsule_visibility_split,
};
pub use points::{GiPointsError, MAX_GI_POINTS, StaticGiQuery, gpu_sample_static_gi};
pub use zh3::zh3_irradiance_over_pi;

const COMMON: &str = include_str!("../shaders/ddgi_common.wgsl");
const LOOKUP: &str = include_str!("../shaders/baked_gi.wgsl");
const SET: &str = include_str!("../shaders/baked_gi_set.wgsl");
const SCREEN: &str = include_str!("../shaders/baked_gi_screen.wgsl");

/// Words per probe of the SH section.
#[cfg(test)]
const SH_WORDS: usize = 16;
/// Words per brick of the brick table.
const BRICK_WORDS: usize = 12;
/// Storage bindings the screen pass reads volumes from (ADR-2618; `baked_gi_screen.wgsl`
/// bindings 3, 5, 6, 7). Volumes pack into them first fit, each slot at most the device's
/// storage-binding size, so a minimum-spec device (128 MiB bindings) installs every part of a
/// split volume (at most `regia_asset::baked_gi_split::MAX_SPLIT_PARTS`, one per slot).
pub const BAKED_GI_VOLUME_SLOTS: usize = 4;
/// Bins of the brick-loop probe's histogram ([`BrickVisits`]; `baked_gi_screen.wgsl`
/// `BGI_VISIT_BINS`): bin `k` counts the texels whose lookups tested `k` bricks, the last bin
/// every count at or past it.
pub const BRICK_VISIT_BINS: usize = 64;
/// Words per texel of the screen lookup (`ddgi_common.wgsl` `DDGI_SCREEN_WORDS`).
const SCREEN_WORDS: u64 = 8;
/// Words per texel of the probe-relative AO split, after the lookup's texels
/// (`baked_gi_screen.wgsl` `bgi_split_word`, [`ProbeAoSplit`]; ADR-1104).
const SPLIT_WORDS: u64 = 3;
/// Words per texel of the capsule occluders' visibility, after the split words, only while capsules
/// are set (`baked_gi_screen.wgsl` `bgi_occluder_word`; ADR-2656).
const OCCLUDER_WORDS: u64 = 1;

/// Whether the capsule occluder words of a `texels` lookup fit: in the storage binding and in the
/// lookup buffer itself (`buffer_bytes`). The buffer only grows, and a large frame near the
/// binding limit makes one without the occluder words, so a later smaller frame must not write
/// them past its end (`bgi_occluder_word` indexes by this frame's texels).
fn occluder_words_fit(texels: u64, buffer_bytes: u64, binding_max: u64) -> bool {
    let bytes = texels.saturating_mul((SCREEN_WORDS + SPLIT_WORDS + OCCLUDER_WORDS) * 4);
    bytes <= binding_max && bytes <= buffer_bytes
}

/// Lookup width, height and step for a `full` frame at `wanted`, given a storage binding of
/// `binding_max` bytes.
///
/// The screen pass writes `SCREEN_WORDS` + `SPLIT_WORDS` words per texel with no bounds check.
/// `fit_lookup` tries `wanted` and then half. When neither write fits, the step doubles until
/// the 11-word write fits. A 1×1 grid is the floor. An empty frame stays 1×1 at step 2.
fn screen_lookup_for_binding(
    full: (u32, u32),
    wanted: crate::ddgi::GiLookupResolution,
    binding_max: u64,
) -> (u32, u32, u32) {
    let write_fits = |w: u32, h: u32| {
        u64::from(w).saturating_mul(u64::from(h)) * (SCREEN_WORDS + SPLIT_WORDS) * 4 <= binding_max
    };
    if let Some(found) = crate::ddgi::fit_lookup(full, wanted, write_fits) {
        return found;
    }
    if full.0 == 0 || full.1 == 0 {
        return (1, 1, 2);
    }
    let mut step = wanted.step().max(2);
    loop {
        let w = full.0.div_ceil(step).max(1);
        let h = full.1.div_ceil(step).max(1);
        if write_fits(w, h) || (w == 1 && h == 1) {
            return (w, h, step);
        }
        let next = step.saturating_mul(2);
        if next == step {
            return (1, 1, step);
        }
        step = next;
    }
}
/// `DdgiUniform::ctrl.w` bit: the screen lookup holds the occluder words, so a lightmap receiver
/// multiplies its baked diffuse term by them (`ddgi_sample.wgsl` `DDGI_CTRL_OCCLUDER_VIS`, ADR-2656).
pub(crate) const CTRL_BAKED_OCCLUDER_VIS: u32 = 1 << 15;
/// `DdgiUniform::ctrl.w` bit: the screen irradiance holds the sky-body fill (baked, shadowed),
/// so the forward and terrain passes drop their live fill where it covers.
pub(crate) const CTRL_BAKED_FILL: u32 = 8;
/// `DdgiUniform::ctrl.w` bit: the screen irradiance holds the emissive component, so the forward
/// pass takes the area lights of the emitters the volume holds back out where it covers
/// (ADR-1386; `lab_forward.wgsl`).
pub(crate) const CTRL_BAKED_EMITTERS: u32 = 16;
/// Above this angle between the frame's sun and the baked one, the bake's bounce light is for
/// another sun (warned once per volume).
const SUN_MISMATCH_DEG: f32 = 3.0;
/// Added to an interior brick's selection key (`baked_gi.wgsl` `BGI_INTERIOR_PRIORITY`).
const INTERIOR_PRIORITY: f32 = 1.0e4;
/// Added to each layer's visibility where a brick blends with the terrain layer
/// (`baked_gi.wgsl` `BGI_SEEN_FLOOR`): two blind layers keep the plain blend.
const SEEN_FLOOR: f32 = 0.02;
/// `BakedGiUniform::extra.z` bit: de-ring the L1 evaluation (`baked_gi.wgsl`
/// `BGI_LOOKUP_DERING`, [`BakedGiSettings::deringing`]).
const LOOKUP_DERING: u32 = 1;
/// `extra.z` bit: the leak guard on the wall codes (`BGI_LOOKUP_LEAK_GUARD`,
/// [`BakedGiSettings::leak_guard`]).
const LOOKUP_LEAK_GUARD: u32 = 2;
/// `extra.z` bit: the Chebyshev bound per moment tap (`BGI_LOOKUP_PER_TAP`,
/// [`GiVisibilityFilter::PerTap`], ADR-1105).
const LOOKUP_PER_TAP: u32 = 4;
/// `extra.z` bit: the hallucinated ZH3 evaluation of the summed SH (`BGI_LOOKUP_ZH3`,
/// [`BakedGiSettings::zh3`], ADR-1377).
const LOOKUP_ZH3: u32 = 8;
/// |L1| / L00 of radiance uniform over a hemisphere, Y1·π / (Y00·2π) (`BGI_HEMISPHERE_L1`).
const HEMISPHERE_L1: f32 = 0.866_025_4;
/// Normal component past which the leak guard takes the side the receiver faces
/// (`BGI_WALL_NORMAL`).
const WALL_NORMAL: f32 = 0.5;
/// Added to both sun elevations by [`SunRelight::ground_factor`].
const RELIGHT_EPS: f32 = 0.02;
/// Cells by which an interior brick holds a point outside its probe box (`baked_gi.wgsl`
/// `BGI_INTERIOR_EDGE_SLACK`, ADR-1364): a floor pixel by a wall, or a surface exactly on the box,
/// lands a few ulps either side of the face and must not flip to the outdoor light.
const INTERIOR_EDGE_SLACK: f32 = 0.125;

/// The screen lookup shader's full source.
#[must_use]
pub(crate) fn screen_source() -> String {
    format!("{COMMON}\n{LOOKUP}\n{SET}\n{SCREEN}")
}

/// Tunables of the baked GI lookup ([`crate::FrameRenderer::set_baked_gi_settings`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BakedGiSettings {
    /// Push of the receiver along its normal before the lookup, × the smallest cell edge
    /// (DDGI's normal bias).
    pub normal_bias: f32,
    /// Push toward the viewer, × the smallest cell edge (DDGI's view bias).
    pub view_bias: f32,
    /// Multiplier on the whole baked irradiance.
    pub intensity: f32,
    /// The sky component's light relative to the bake (1 = the sky the volume was baked under).
    pub sky_scale: f32,
    /// The emissive component's light relative to the bake.
    pub emissive_scale: f32,
    /// The sun-bounce component follows the frame's sun (illuminance × color over the baked
    /// one; 0 while the sun is off). `false`: as baked.
    pub follow_sun: bool,
    /// The fill component follows the frame's sky-body fill (its illuminance over the baked
    /// one; 0 without a fill). `false`: as baked.
    pub follow_fill: bool,
    /// The baked fill (direct and bounced, shadowed by the bake) replaces the forward and
    /// terrain passes' live fill where the volume covers. `false` keeps the live fill and drops
    /// the baked fill component (the old, unshadowed look).
    pub replace_live_fill: bool,
    /// De-ring the L1 irradiance (ADR-861). Where a channel's L1 is stronger than a
    /// non-negative linear evaluation allows (light from a narrow solid angle: a window, a lit
    /// patch, a sky body), evaluate the non-negative lobe with the same mean and first moment
    /// instead of clamping the linear form at 0: no light behind the source and no energy added
    /// by the clamp. Light as smooth as a hemisphere (a sky, the ground) evaluates exactly as
    /// before. `false`: the linear form, clamped. Only with [`Self::zh3`] off.
    pub deringing: bool,
    /// Leak guard on the baked wall codes (ADR-861). In a lookup cell with walls, an edge with a
    /// rejected probe at either end (under the ground, inside geometry) joins nothing and its
    /// code does not move the wall's position (that segment crossed the floor or the solid, not
    /// the wall), and where every wall along an axis sits on such edges the receiver takes the
    /// side its normal faces. `false`: the ADR-366 split alone.
    pub leak_guard: bool,
    /// Sun relight (ADR-861): the sun bounce of a probe lit from below (by the ground) follows
    /// the frame's sun elevation against the baked one ([`SunRelight`]), on top of the
    /// illuminance factor of [`BakedGiSettings::follow_sun`] (which it needs). Light bounced from
    /// the side or above keeps the baked factor. `false`: as baked.
    pub sun_relight: bool,
    /// How the forward pass reads its indirect specular from the baked light (ADR-1103):
    /// through the GGX lobe at every roughness ([`crate::ProbeSpecular::Lobe`], the default) or
    /// only in the rough tail (the earlier look).
    pub specular: crate::ProbeSpecular,
    /// Probe-relative screen-space AO (ADR-1104, [`ProbeAoSplit`]): where the volume's light
    /// covers a pixel, the SSAO counts only the occlusion its probes do not already hold, and
    /// the capsule occluders leave the SSAO's share of their occlusion to it. `false` multiplies
    /// the whole SSAO and the whole capsule occlusion into the baked light (counting a baked
    /// wall's or a unit's occlusion twice; for comparison).
    pub probe_relative_ao: bool,
    /// How a corner probe's Chebyshev visibility is filtered over its four bilinear moment taps
    /// (ADR-1105).
    pub visibility_filter: GiVisibilityFilter,
    /// Light-bleeding reduction on the Chebyshev visibility (ADR-1105): the bound's tail
    /// `[amount, 1]` is remapped to `[0, 1]`, so a probe the moments call half visible counts
    /// for less. 0 turns it off; clamped to `0..=`[`MAX_LEAK_REDUCTION`]. Only corners the
    /// moments already find partly hidden change: a receiver every probe sees reads as before.
    pub leak_reduction: f32,
    /// Evaluate the summed L1 SH with a hallucinated quadratic zonal harmonic (ZH3 from L1,
    /// ADR-1377, [`zh3_irradiance_over_pi`]): along the luminance L1 direction, a band-2 term
    /// estimated from the L1 band, so light through an opening or from a lit patch reads with
    /// its directional contrast instead of L1's flattened, negative-behind lobe. Uniform light
    /// and any probe L1 holds without ringing (a sky, a hemisphere) read as L1, bit for bit; the
    /// `.fgiv` data is the same. It replaces [`Self::deringing`] (which then has no effect).
    /// Off by default: on the gi-compare fixtures it moved the FLIP against the path tracer by
    /// −0.0002 to +0.0009 (ADR-1377), so it waits for a by-eye review. `false`: the L1
    /// evaluation (de-ringed or not).
    pub zh3: bool,
    /// The brick-loop probe (ADR-2618): the screen pass counts, per texel, the detail bricks its
    /// lookups tested (the grid cell's candidate list, summed over the blended volumes) into a
    /// [`BRICK_VISIT_BINS`]-bin histogram, read back without stalling a frame later
    /// ([`BakedGiStatus::brick_visits`]). A debug measurement for timing the brick loop on a
    /// device; the light is unchanged. A held view (the static-screen cache serves the frame)
    /// records no lookup and so captures nothing until the view moves. Off by default.
    pub brick_visit_probe: bool,
}

impl Default for BakedGiSettings {
    fn default() -> Self {
        Self {
            normal_bias: 0.1,
            view_bias: 0.3,
            intensity: 1.0,
            sky_scale: 1.0,
            emissive_scale: 1.0,
            follow_sun: true,
            follow_fill: true,
            replace_live_fill: true,
            deringing: true,
            leak_guard: true,
            sun_relight: true,
            specular: crate::ProbeSpecular::Lobe,
            probe_relative_ao: true,
            visibility_filter: GiVisibilityFilter::PerTap,
            leak_reduction: DEFAULT_LEAK_REDUCTION,
            zh3: false,
            brick_visit_probe: false,
        }
    }
}

impl BakedGiSettings {
    /// The lookup's leak control before ADR-1105: the moments filtered first, no light-bleeding
    /// reduction (DDGI as published). Every other field keeps its default.
    #[must_use]
    pub fn legacy_visibility() -> Self {
        Self {
            visibility_filter: GiVisibilityFilter::Moments,
            leak_reduction: 0.0,
            ..Self::default()
        }
    }

    /// [`Self::leak_reduction`] as the lookup uses it: clamped to `0..=`[`MAX_LEAK_REDUCTION`],
    /// 0 when not finite.
    #[must_use]
    pub fn effective_leak_reduction(&self) -> f32 {
        if self.leak_reduction.is_finite() {
            self.leak_reduction.clamp(0.0, MAX_LEAK_REDUCTION)
        } else {
            0.0
        }
    }
}

/// Default [`BakedGiSettings::leak_reduction`].
pub const DEFAULT_LEAK_REDUCTION: f32 = 0.25;

/// Largest [`BakedGiSettings::leak_reduction`] the lookup uses (at 1 every partly hidden probe
/// would drop out).
pub const MAX_LEAK_REDUCTION: f32 = 0.95;

/// How the baked GI lookup filters a corner probe's Chebyshev visibility over the four bilinear
/// taps of its octahedral distance moments ([`BakedGiSettings::visibility_filter`], ADR-1105).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum GiVisibilityFilter {
    /// The moments are filtered bilinearly, then one Chebyshev bound is taken (DDGI as
    /// published). Across a depth edge the filtered moments hold a mean between the two sides
    /// and a large variance, so a receiver behind a thin wall can read as visible.
    Moments,
    /// One Chebyshev bound per tap, the bounds filtered bilinearly (percentage-closer
    /// filtering): a tap that sees a wall in front of the receiver keeps its share hidden.
    #[default]
    PerTap,
}

impl GiVisibilityFilter {
    /// The protocol name: `"moments"` or `"per_tap"`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Moments => "moments",
            Self::PerTap => "per_tap",
        }
    }

    /// The filter named `name` ([`Self::name`]).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "moments" => Some(Self::Moments),
            "per_tap" => Some(Self::PerTap),
            _ => None,
        }
    }
}

/// The sun relight of one frame ([`sun_relight`], ADR-861): the up component of the unit
/// vector toward the baked sun and toward the frame's sun. Sun light bounced off level ground
/// scales with the sun's elevation (the ground's illuminance is `E · sin h`), so the sun bounce of
/// a probe lit from below takes [`SunRelight::ground_factor`], in proportion to how much of its
/// L1 points down and how directional it is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SunRelight {
    /// Up component of the unit vector toward the sun the volume was baked under.
    pub baked_up: f32,
    /// Up component of the unit vector toward the frame's sun.
    pub frame_up: f32,
}

impl SunRelight {
    /// No relight: the sun bounce as baked (still times the illuminance factor).
    pub const NONE: Self = Self {
        baked_up: 1.0,
        frame_up: 1.0,
    };

    /// `(max(frame_up, 0) + ε) / (max(baked_up, 0) + ε)` with ε = 0.02: the factor on sun light
    /// bounced off level ground. Exactly 1 for [`SunRelight::NONE`].
    #[must_use]
    pub fn ground_factor(&self) -> f32 {
        (self.frame_up.max(0.0) + RELIGHT_EPS) / (self.baked_up.max(0.0) + RELIGHT_EPS)
    }
}

impl Default for SunRelight {
    fn default() -> Self {
        Self::NONE
    }
}

/// The frame's [`SunRelight`] for `volume`: [`SunRelight::NONE`] when
/// [`BakedGiSettings::sun_relight`] or [`BakedGiSettings::follow_sun`] is off, the frame has no
/// sun direction, or the volume was baked without a sun.
#[must_use]
pub fn sun_relight(
    volume: &BakedGiVolume,
    settings: &BakedGiSettings,
    lights: &BakedGiFrameLights,
) -> SunRelight {
    if !(settings.sun_relight && settings.follow_sun) || volume.lights.sun_illuminance <= 0.0 {
        return SunRelight::NONE;
    }
    let Some(a) = lights.to_sun else {
        return SunRelight::NONE;
    };
    let b = volume.lights.sun_direction;
    let (la, lb) = (length(a), length(b));
    if !(la > 0.0 && lb > 0.0) {
        return SunRelight::NONE;
    }
    SunRelight {
        baked_up: b[1] / lb,
        frame_up: a[1] / la,
    }
}

/// The frame's lights the components follow ([`component_scales`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BakedGiFrameLights {
    /// Sun illuminance × color (lux), `None` when the sun is off or down.
    pub sun: Option<[f32; 3]>,
    /// Unit vector toward the sun.
    pub to_sun: Option<[f32; 3]>,
    /// Sky-body fill illuminance × tint (lux), `None` without a fill.
    pub fill: Option<[f32; 3]>,
}

/// What the renderer did with the installed volume ([`crate::FrameRenderer::baked_gi_status`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BakedGiStatus {
    /// A volume is installed (the level's own, [`crate::FrameRenderer::set_baked_gi`], or a
    /// usable layer, [`crate::FrameRenderer::set_baked_gi_layers`]).
    pub installed: bool,
    /// The last main-view frame was lit by it.
    pub active: bool,
    /// Probes in the installed volume and every layer's.
    pub probes: usize,
    /// Bytes of the GPU copy (0 before the first frame that used it).
    pub gpu_bytes: u64,
    /// Why the volume cannot be used on this device (too large for one storage binding).
    pub error: Option<String>,
    /// Factor on each component's baked light last frame ([`GiComponent`] order, per channel).
    pub scales: [[f32; 3]; GI_COMPONENTS],
    /// Angle between the frame's sun and the baked sun, degrees (0 without a sun).
    pub sun_mismatch_deg: f32,
    /// Capsule occluders in use ([`crate::FrameRenderer::set_gi_occluders`]).
    pub occluders: usize,
    /// Capsules the last [`crate::FrameRenderer::set_gi_occluders`] dropped: unusable (a
    /// non-finite end, radius or strength ≤ 0) or past [`MAX_GI_OCCLUDERS`].
    pub occluders_dropped: usize,
    /// Interior bricks of the installed volume.
    pub interior_bricks: usize,
    /// Probes of the installed volume with a wall code (ADR-366): the bake found geometry
    /// between them and a grid neighbor, and the lookup keeps the receiver's side of it. 0 for
    /// a volume baked without the wall test.
    pub wall_probes: usize,
    /// The sun relight of the last frame ([`sun_relight`], ADR-861); its
    /// [`SunRelight::ground_factor`] is 1 when the sun stands where it was baked or
    /// [`BakedGiSettings::sun_relight`] is off, and below 1 for a lower sun. [`SunRelight::NONE`]
    /// before the first frame that used the volume.
    pub sun_relight: SunRelight,
    /// Volumes the last frame blended (the installed one and the layers; 0 when none lit it).
    pub volumes: usize,
    /// Usable volumes the last frame left out: past [`MAX_BAKED_GI_VOLUMES`] in priority order,
    /// or past the device's storage-binding size.
    pub volumes_dropped: usize,
    /// The last main-view frame ran the probe-relative AO split
    /// ([`BakedGiSettings::probe_relative_ao`] on, the volume lit the frame and the SSAO ran).
    pub probe_relative_ao: bool,
    /// Emitters the installed volume's bake recorded (ADR-1386): their area lights give way to
    /// the bake where it covers. `None` without a volume, or for a volume from a format v1 or v2
    /// file, which does not name its emitters: every emissive area light then stays, and on the
    /// tiers that draw them the baked emitters are counted twice until the map is rebaked.
    pub emitters: Option<usize>,
    /// Cells of the installed volume's brick lookup grid (ADR-1835; 0 without bricks).
    pub brick_grid_cells: usize,
    /// Brick references the grid's cells list in all: the most bricks the screen pass tests
    /// for one texel is the longest list, never more than the brick count.
    pub brick_grid_entries: usize,
    /// The volumes the last frame blended, in blend order (ADR-2618; empty when none lit it).
    pub volume_list: Vec<BakedGiVolumeEntry>,
    /// Storage slots the volumes used (at most [`BAKED_GI_VOLUME_SLOTS`]; 0 when none lit).
    pub volume_slots: usize,
    /// The brick-loop probe's last histogram ([`BakedGiSettings::brick_visit_probe`]; `None`
    /// while it is off or before its first read-back).
    pub brick_visits: Option<BrickVisits>,
}

/// One volume a frame blended ([`BakedGiStatus::volume_list`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BakedGiVolumeEntry {
    /// Probes of the volume.
    pub probes: usize,
    /// Bytes of its sections in the volume buffer.
    pub bytes: u64,
    /// The storage slot holding it.
    pub slot: usize,
    /// Its layer's priority (0 for the level's own volume).
    pub priority: i32,
    /// Its layer's weight, clamped to 0–1.
    pub weight: f32,
    /// `(index, count, group)` when it is a part of a split volume (`.fgiv` v4).
    pub split: Option<(u32, u32, u64)>,
    /// The edges (`-x, -z, +x, +z`) across which it blends with an installed neighbor part.
    pub joined: [bool; 4],
}

/// The brick-loop probe's histogram ([`BakedGiSettings::brick_visit_probe`], ADR-2618).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BrickVisits {
    /// Texels per count of bricks tested ([`BRICK_VISIT_BINS`] bins; the last holds every
    /// count at or past it).
    pub histogram: Vec<u32>,
    /// Texels counted (every lit texel of one frame's lookup).
    pub texels: u64,
    /// Mean bricks tested per texel (the last bin counted at its index).
    pub mean: f32,
    /// The highest bin with a texel.
    pub max: u32,
    /// Captures read back so far (one per frame while the read-back keeps up).
    pub captures: u64,
}

impl BrickVisits {
    /// The summary of a read-back `histogram`.
    #[must_use]
    pub fn of(histogram: Vec<u32>, captures: u64) -> Self {
        let texels: u64 = histogram.iter().map(|c| u64::from(*c)).sum();
        let weighted: u64 = histogram
            .iter()
            .enumerate()
            .map(|(k, c)| k as u64 * u64::from(*c))
            .sum();
        let max = histogram.iter().rposition(|c| *c > 0).unwrap_or(0) as u32;
        Self {
            mean: if texels > 0 {
                weighted as f32 / texels as f32
            } else {
                0.0
            },
            histogram,
            texels,
            max,
            captures,
        }
    }
}

/// The volume's emitter light reaches the frame under `settings`: neither the emissive component
/// nor the whole bake is scaled to 0 ([`BakedGiSettings::emissive_scale`],
/// [`BakedGiSettings::intensity`]). Only then do the area lights of its emitters give way to it.
fn lights_emitters(settings: &BakedGiSettings) -> bool {
    settings.emissive_scale > 0.0 && settings.intensity > 0.0
}

/// The emitters an installed volume holds ([`BakedGiVolume::emitters`], ADR-1386), sorted along
/// X for the lookup: which fitted emissive area lights the forward pass leaves to the bake.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BakedEmitterSet {
    /// By `min[0]`.
    emitters: Vec<BakedEmitter>,
    /// The largest [`BakedEmitter::slack`] (bounds the X search window).
    slack: f32,
}

impl BakedEmitterSet {
    /// The set of `emitters` (a volume's list).
    #[must_use]
    pub fn new(emitters: &[BakedEmitter]) -> Self {
        let mut emitters = emitters.to_vec();
        emitters.sort_by(|a, b| a.min[0].total_cmp(&b.min[0]));
        let slack = emitters.iter().map(BakedEmitter::slack).fold(0.0, f32::max);
        Self { emitters, slack }
    }

    /// Emitters in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.emitters.len()
    }

    /// The set is empty (the bake traced no emitter).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.emitters.is_empty()
    }

    /// `true` when a drawn emitter with world bounds `drawn` is one the bake traced
    /// ([`BakedEmitter::matches`] against any record).
    #[must_use]
    pub fn holds(&self, drawn: &BakedEmitter) -> bool {
        let x = drawn.min[0];
        let start = self.emitters.partition_point(|e| e.min[0] < x - self.slack);
        self.emitters[start..]
            .iter()
            .take_while(|e| e.min[0] <= x + self.slack)
            .any(|e| e.matches(drawn.min, drawn.max))
    }
}

/// One baked GI lookup ([`sample_static_gi`]): irradiance / π for the normal, per component and
/// summed, and the coverage the passes blend it in by.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StaticGiSample {
    /// Irradiance / π of all components (scaled), clamped at 0 after the sum, as the shader.
    pub irradiance_over_pi: [f32; 3],
    /// Irradiance / π per component ([`GiComponent`] order), scaled, each clamped at 0.
    pub components: [[f32; 3]; GI_COMPONENTS],
    /// 0 outside the volume (or with every corner probe rejected), 1 well inside it.
    pub coverage: f32,
    /// The share of the interpolation weight whose probes see the receiver (Chebyshev
    /// visibility from the baked distance moments, 1 = every corner sees it). Low values mark
    /// receivers the probes cannot see (behind a wall from all of them): where a light leak or
    /// a dark leak is likely. 0 without coverage.
    pub visibility: f32,
    /// The share of [`Self::coverage`] the detail brick's light holds (0 without one): the
    /// coverage times the brick's fraction of the light (its coverage share weighted by how much
    /// of its cell sees the receiver, as the lookup blends the layers). What the probe-relative
    /// AO split reads ([`ProbeAoSplit`], ADR-1104).
    pub brick_share: f32,
    /// The share of [`Self::coverage`] the terrain layer's light holds (`brick_share +
    /// terrain_share` is the coverage, except inside an interior brick with no valid corner,
    /// which reads black at full coverage with both shares 0).
    pub terrain_share: f32,
    /// The largest cell edge of the brick that lights the point, metres (0 without one;
    /// several volumes: averaged by their brick light).
    pub brick_cell: f32,
    /// The largest column spacing of the terrain layer that lights the point, metres (0 without
    /// one; several volumes: averaged by their terrain light).
    pub terrain_cell: f32,
}

impl StaticGiSample {
    /// A sample holding only the probe-relative AO shares of coverage `cov` split as `split`.
    fn from_split(cov: f32, split: &LayerSplit) -> Self {
        Self {
            brick_share: cov * split.brick,
            terrain_share: cov * split.terrain,
            brick_cell: split.brick_cell,
            terrain_cell: split.terrain_cell,
            ..Self::default()
        }
    }
}

fn lum(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// The factor on each component's baked light for the frame's lights: sun bounce = the frame's
/// sun over the baked sun per channel (when [`BakedGiSettings::follow_sun`]), sky and emissive
/// from the settings, fill = the frame's fill over the baked fill (when
/// [`BakedGiSettings::follow_fill`]; 0 when [`BakedGiSettings::replace_live_fill`] is off,
/// because the live fill then stays).
#[must_use]
pub fn component_scales(
    volume: &BakedGiVolume,
    settings: &BakedGiSettings,
    lights: &BakedGiFrameLights,
) -> [[f32; 3]; GI_COMPONENTS] {
    let baked = volume.lights;
    let sun = if settings.follow_sun {
        match lights.sun {
            Some(s) => std::array::from_fn(|c| {
                let b = baked.sun_illuminance * baked.sun_color[c];
                if b > 0.0 { s[c] / b } else { 0.0 }
            }),
            None => [0.0; 3],
        }
    } else {
        [1.0; 3]
    };
    let fill = if !settings.replace_live_fill {
        [0.0; 3]
    } else if settings.follow_fill {
        let f = lights.fill.map_or(0.0, lum);
        [if baked.fill_lux > 0.0 {
            f / baked.fill_lux
        } else {
            0.0
        }; 3]
    } else {
        [1.0; 3]
    };
    [sun, [settings.sky_scale; 3], fill, [settings.emissive_scale; 3]]
}

/// The frame quality while a baked volume lights the frame: no probe update (DDGI) and no
/// SSGI (the bake carries the bounce, and SSGI would count it twice), full-resolution GTAO at
/// half resolution (the bake carries the long-range sky visibility; GTAO keeps the contacts).
/// Shadows, the paged cache, SSR and everything else are unchanged.
#[must_use]
pub fn policy(mut quality: crate::FrameQuality) -> crate::FrameQuality {
    quality.gi_probes = false;
    quality.screen_space_gi = false;
    if quality.ssao == crate::SsaoMode::GtaoFull {
        quality.ssao = crate::SsaoMode::GtaoHalfRes;
    }
    quality
}

/// Angle between the frame's sun and the baked one, degrees (0 without a sun on either side).
#[must_use]
pub fn sun_mismatch_deg(volume: &BakedGiVolume, lights: &BakedGiFrameLights) -> f32 {
    let (Some(a), true) = (lights.to_sun, volume.lights.sun_illuminance > 0.0) else {
        return 0.0;
    };
    let b = volume.lights.sun_direction;
    let la = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
    let lb = (b[0] * b[0] + b[1] * b[1] + b[2] * b[2]).sqrt();
    if la <= 0.0 || lb <= 0.0 {
        return 0.0;
    }
    let cos = ((a[0] * b[0] + a[1] * b[1] + a[2] * b[2]) / (la * lb)).clamp(-1.0, 1.0);
    cos.acos().to_degrees()
}

// ---- CPU mirror of `baked_gi.wgsl` ----

type V3 = [f32; 3];

fn add(a: V3, b: V3) -> V3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn mul(a: V3, k: f32) -> V3 {
    [a[0] * k, a[1] * k, a[2] * k]
}
fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn length(a: V3) -> f32 {
    dot(a, a).sqrt()
}
fn normalize(a: V3) -> V3 {
    let l = length(a);
    if l > 0.0 { mul(a, 1.0 / l) } else { [0.0; 3] }
}

/// Texel `(x, y)` of a `res` × `res` octahedral map, where `x` and `y` may lie one texel past an
/// edge (`baked_gi.wgsl` `bgi_oct_texel`). Each edge folds at its midpoint, so the texel across
/// the right edge at row `y` is `(res − 1, res − 1 − y)` and across a corner the opposite corner
/// (the border DDGI copies around each probe, Majercik et al. 2019 § 4.2).
fn oct_texel(x: i32, y: i32, res: i32) -> (usize, usize) {
    let (mut tx, mut ty) = (x, y);
    if tx < 0 || tx >= res {
        (tx, ty) = (tx.clamp(0, res - 1), res - 1 - ty);
    }
    if ty < 0 || ty >= res {
        (tx, ty) = (res - 1 - tx, ty.clamp(0, res - 1));
    }
    (tx.max(0) as usize, ty.max(0) as usize)
}

/// Probe `i`'s four bilinear moment taps toward `dir` (unorm: mean / far, mean² / far², in the
/// order x0y0, x1y0, x0y1, x1y1) and the bilinear fractions (`baked_gi.wgsl` `bgi_moment_taps`).
/// A tap past an edge of the map reads the texel across the fold ([`oct_texel`]), so the filter
/// is continuous over the whole sphere.
fn moment_taps(volume: &BakedGiVolume, i: usize, dir: V3) -> ([[f32; 2]; 4], [f32; 2]) {
    let res = volume.moment_resolution.max(1) as usize;
    let texels = res * res;
    let bytes = volume.moment_bytes();
    let ri = res as i32;
    let at = |x: i32, y: i32| -> [f32; 2] {
        let (tx, ty) = oct_texel(x, y, ri);
        let k = (i * texels + ty * res + tx) * MOMENT_TEXEL_BYTES;
        match bytes.get(k..k + 4) {
            Some(b) => [
                f32::from(u16::from_le_bytes([b[0], b[1]])) / 65535.0,
                f32::from(u16::from_le_bytes([b[2], b[3]])) / 65535.0,
            ],
            None => [0.0; 2],
        }
    };
    let uv = regia_asset::baked_gi::oct_encode(dir);
    let rf = res as f32;
    let fx = uv[0].clamp(0.0, 1.0) * rf - 0.5;
    let fy = uv[1].clamp(0.0, 1.0) * rf - 0.5;
    let (flx, fly) = (fx.floor(), fy.floor());
    let (x0, y0) = (flx as i32, fly as i32);
    (
        [at(x0, y0), at(x0 + 1, y0), at(x0, y0 + 1), at(x0 + 1, y0 + 1)],
        [fx - flx, fy - fly],
    )
}

/// Chebyshev upper bound (Donnelly & Lauritzen 2006) on the chance that a receiver `r` metres
/// out is visible from the moments `mean`, `sq` (metres): 1 at or before the mean; `floor_var`
/// floors the variance (`baked_gi.wgsl` `bgi_chebyshev`).
fn chebyshev_bound(mean: f32, sq: f32, r: f32, floor_var: f32) -> f32 {
    if r <= mean {
        return 1.0;
    }
    let variance = (sq - mean * mean).abs().max(floor_var);
    let d = r - mean;
    variance / (variance + d * d)
}

/// The visibility of a receiver `r` metres from probe `i` toward `dir`, cell scale `s`
/// (`baked_gi.wgsl` `bgi_visibility`; keep the two in step): the Chebyshev bound per tap
/// filtered bilinearly ([`GiVisibilityFilter::PerTap`], percentage-closer filtering) or on the
/// bilinear moments ([`GiVisibilityFilter::Moments`], DDGI), then the light-bleeding reduction
/// ([`BakedGiSettings::leak_reduction`]).
fn visibility(volume: &BakedGiVolume, settings: &BakedGiSettings, i: usize, dir: V3, r: f32, s: f32) -> f32 {
    let (taps, f) = moment_taps(volume, i, dir);
    let far = volume.max_distance;
    let floor_var = 1e-4 * s * s;
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let mut vis = match settings.visibility_filter {
        GiVisibilityFilter::PerTap => {
            let c = taps.map(|t| chebyshev_bound(t[0] * far, t[1] * far * far, r, floor_var));
            lerp(lerp(c[0], c[1], f[0]), lerp(c[2], c[3], f[0]), f[1])
        }
        GiVisibilityFilter::Moments => {
            let m: [f32; 2] = std::array::from_fn(|k| {
                lerp(
                    lerp(taps[0][k], taps[1][k], f[0]),
                    lerp(taps[2][k], taps[3][k], f[0]),
                    f[1],
                )
            });
            chebyshev_bound(m[0] * far, m[1] * far * far, r, floor_var)
        }
    };
    let k = settings.effective_leak_reduction();
    if k > 0.0 && vis < 1.0 {
        vis = ((vis - k) / (1.0 - k)).clamp(0.0, 1.0);
    }
    vis
}

/// The DDGI corner weight (`baked_gi.wgsl` `bgi_corner`) and the corner's visible share: its
/// Chebyshev visibility alone times its interpolation weight.
#[allow(clippy::too_many_arguments)]
fn corner(
    volume: &BakedGiVolume,
    settings: &BakedGiSettings,
    i: usize,
    probe_pos: V3,
    p: V3,
    biased: V3,
    n: V3,
    s: f32,
    tri: f32,
) -> (f32, f32) {
    let to_probe = normalize(add(sub(probe_pos, p), [1e-6, 0.0, 0.0]));
    let mut w = (dot(to_probe, n) + 1.0) * 0.5;
    w = w * w + 0.2;
    let from_probe = sub(biased, probe_pos);
    let r = length(from_probe).max(1e-4);
    let v0 = visibility(volume, settings, i, mul(from_probe, 1.0 / r), r, s);
    let mut vis = 1.0;
    if v0 < 1.0 {
        vis = (v0 * v0 * v0).max(0.0);
        w *= vis;
    }
    w = w.max(1e-6);
    if w < 0.2 {
        w = w * w * w / (0.2 * 0.2);
    }
    let t = tri.max(0.001);
    (w * t, vis * t)
}

/// Weighted SH per component, weight sum, coverage, whether it is an interior brick, and over
/// the corners used the visible share and the interpolation weight. For the debug trace
/// (`debug.rs`, ADR-1109): whether a cell was found, its eight corners, the corners the wall
/// mask kept (bit `k` for corner `k`) and the brick the part came from.
struct Part {
    sh: [ShRgb; GI_COMPONENTS],
    wsum: f32,
    cov: f32,
    interior: bool,
    vis: f32,
    tri: f32,
    cell: bool,
    corners: Corners,
    kept: u8,
    brick: Option<usize>,
    /// A brick's largest cell edge (m); 0 for the terrain layer (ADR-1104).
    edge: f32,
}

impl Part {
    fn empty() -> Self {
        Self {
            sh: [[[0.0; 4]; 3]; GI_COMPONENTS],
            wsum: 0.0,
            cov: 0.0,
            interior: false,
            vis: 0.0,
            tri: 0.0,
            cell: false,
            corners: Corners::default(),
            kept: 0,
            brick: None,
            edge: 0.0,
        }
    }

    /// How much of the cell sees the receiver (`baked_gi.wgsl` `bgi_part_visibility`).
    fn visibility(&self) -> f32 {
        if self.tri <= 0.0 {
            0.0
        } else {
            (self.vis / self.tri).clamp(0.0, 1.0)
        }
    }

    /// Adds a corner: its weight and visible share ([`corner`]) and its interpolation weight.
    fn add_corner(
        &mut self,
        volume: &BakedGiVolume,
        i: usize,
        (w, vis): (f32, f32),
        tri: f32,
        light: &Light,
    ) {
        self.add_probe(volume, i, w, light);
        self.vis += vis;
        self.tri += tri.max(0.001);
    }

    /// Adds probe `i` at weight `w`: each component scaled by its factor, the sun bounce also by
    /// the probe's [`relight_factor`].
    fn add_probe(&mut self, volume: &BakedGiVolume, i: usize, w: f32, light: &Light) {
        for (c, comp) in [
            GiComponent::SunBounce,
            GiComponent::Sky,
            GiComponent::Fill,
            GiComponent::Emissive,
        ]
        .into_iter()
        .enumerate()
        {
            let k = light.scales[c];
            if k[0] + k[1] + k[2] <= 0.0 {
                continue;
            }
            let sh = volume.probe_sh(i, comp);
            let scaled: ShRgb = std::array::from_fn(|ch| sh[ch].map(|v| v * k[ch]));
            let f = if comp == GiComponent::SunBounce {
                relight_factor(&scaled, light.ground)
            } else {
                1.0
            };
            for (dst, src) in self.sh[c].iter_mut().zip(&scaled) {
                for (d, v) in dst.iter_mut().zip(src) {
                    *d += v * w * f;
                }
            }
        }
        self.wsum += w;
    }
}

/// The light a lookup adds probes under: the component factors ([`component_scales`]) and the
/// sun-bounce ground factor ([`SunRelight::ground_factor`]; 1 = as baked).
struct Light<'a> {
    scales: &'a [[f32; 3]; GI_COMPONENTS],
    ground: f32,
}

/// The sun-bounce factor of one probe (`baked_gi.wgsl` `bgi_relight`) from its scaled SH `sh`
/// and the ground factor `g`: `1 + (g − 1) · a · down`, where `a` is the luminance L1's strength
/// against radiance uniform over a hemisphere (clamped to 1) and `down` how much it points below
/// the horizon. Exactly 1 for `g` = 1.
fn relight_factor(sh: &ShRgb, g: f32) -> f32 {
    let w = [0.2126f32, 0.7152, 0.0722];
    let lum0 = w[0] * sh[0][0] + w[1] * sh[1][0] + w[2] * sh[2][0];
    let lum1: V3 = std::array::from_fn(|a| w[0] * sh[0][a + 1] + w[1] * sh[1][a + 1] + w[2] * sh[2][a + 1]);
    let len = length(lum1);
    if !(len > 0.0 && lum0 > 0.0) {
        return 1.0;
    }
    let a = (len / (HEMISPHERE_L1 * lum0)).min(1.0);
    let down = (-lum1[1] / len).max(0.0);
    1.0 + (g - 1.0) * a * down
}

/// The eight corners of one lookup cell: probe index, flags, weight (`None` for an invalid
/// probe), visible share and interpolation weight ([`corner`]), and the probe's position
/// (relocated when valid, nominal when not; for the debug trace). Corner `k` takes the high end
/// of axis `b` when bit `b` of `k` is set.
#[derive(Clone, Copy, Default)]
struct Corners {
    index: [usize; 8],
    flags: [u16; 8],
    weight: [Option<f32>; 8],
    vis: [f32; 8],
    tri: [f32; 8],
    position: [V3; 8],
}

impl Corners {
    /// Records corner `k`'s probe: its index, flags, position and interpolation weight.
    fn visit(&mut self, k: usize, index: usize, flags: u16, position: V3, tri: f32) {
        self.index[k] = index;
        self.flags[k] = flags;
        self.position[k] = position;
        self.tri[k] = tri;
    }

    /// Records a valid corner's [`corner`] result and interpolation weight.
    fn set(&mut self, k: usize, (w, vis): (f32, f32), tri: f32) {
        self.weight[k] = Some(w);
        self.vis[k] = vis;
        self.tri[k] = tri;
    }

    /// Adds the corners `wall_mask` keeps to `part`, in corner order (the same order and
    /// weights as before ADR-366 when no probe has a wall code), and records them in `part`.
    fn add_to(&self, part: &mut Part, volume: &BakedGiVolume, light: &Light, cell: &CellAxes) {
        let weights = self.weight.map(|w| w.unwrap_or(0.0));
        let valid = self
            .weight
            .iter()
            .enumerate()
            .fold(0u8, |m, (k, w)| if w.is_some() { m | (1 << k) } else { m });
        let mask = wall_mask(&self.flags, &weights, valid, cell);
        for (k, (&index, w)) in self.index.iter().zip(self.weight).enumerate() {
            if let Some(w) = w
                && mask & (1 << k) != 0
            {
                part.add_corner(volume, index, (w, self.vis[k]), self.tri[k], light);
            }
        }
        part.cell = true;
        part.corners = *self;
        part.kept = mask & valid;
    }
}

/// Edge `e` (0..12) of a cell: its low corner, its high corner, and the corner bit it runs
/// along (`baked_gi.wgsl` `bgi_wall_mask` enumerates them the same way, in the same order).
fn cell_edge(e: usize) -> (usize, usize, usize) {
    let b = e / 4;
    let j = e % 4;
    let lo = ((j >> b) << (b + 1)) | (j & ((1 << b) - 1));
    (lo, lo | (1 << b), b)
}

/// What a lookup cell's corner bit `b` runs along: the flags' grid axis, whether both ends are
/// one probe (clamped at the grid's edge), the receiver's fraction from the low end and its
/// normal's component; and whether the leak guard is on ([`BakedGiSettings::leak_guard`]).
#[derive(Clone, Copy, Debug)]
struct CellAxes {
    axis: [usize; 3],
    same: [bool; 3],
    f: [f32; 3],
    normal: [f32; 3],
    guard: bool,
}

/// Which corners of a cell light the receiver, bit `k` for corner `k` (ADR-366). Without a
/// wall code on any edge of the cell: all eight (the lookup is unchanged). Otherwise:
///
/// - An edge is blocked when its low corner's flags hold a wall toward the high corner (an edge
///   clamped to one probe never is). The corners split into the groups the unblocked edges join.
/// - The receiver's corner: along each corner bit, the high end when the receiver's fraction is
///   past the walls on the edges along that bit (their positions interpolated by the receiver's
///   fractions on the other two bits), else the low end; the nearer end along a bit without a
///   wall.
/// - Its group lights the receiver. If that group holds no valid probe (the receiver's side is
///   solid, or every probe there was rejected), the group with the largest summed weight does,
///   ties to the group of the lowest corner.
///
/// With the leak guard (`cell.guard`, ADR-861; `valid` bit `k` set for a valid corner `k`): an
/// edge with an invalid end joins nothing and its code does not move the wall position, and
/// along a bit whose walls all sit on such edges, a receiver whose normal points along the bit by
/// at least `WALL_NORMAL` takes the side it faces instead of the cell's middle.
fn wall_mask(flags: &[u16; 8], weights: &[f32; 8], valid: u8, cell: &CellAxes) -> u8 {
    let live = |k: usize| valid & (1 << k) != 0;
    let mut blocked = [false; 12];
    let mut has_wall = [false; 3];
    let mut wall_sum = [0.0f32; 3];
    let mut wall_weight = [0.0f32; 3];
    for (e, block) in blocked.iter_mut().enumerate() {
        let (lo, hi, b) = cell_edge(e);
        if cell.same[b] {
            continue;
        }
        let Some(t) = ProbeFlags::wall_at(flags[lo], cell.axis[b]) else {
            continue;
        };
        *block = true;
        has_wall[b] = true;
        if cell.guard && !(live(lo) && live(hi)) {
            continue;
        }
        let mut w = 1.0f32;
        for (c, &fc) in cell.f.iter().enumerate() {
            if c != b {
                w *= if lo & (1 << c) != 0 { fc } else { 1.0 - fc };
            }
        }
        w += 1e-4;
        wall_sum[b] += w * t;
        wall_weight[b] += w;
    }
    if !blocked.contains(&true) {
        return 0xff;
    }
    if cell.guard {
        for (e, block) in blocked.iter_mut().enumerate() {
            let (lo, hi, _) = cell_edge(e);
            if !(live(lo) && live(hi)) {
                *block = true;
            }
        }
    }
    let mut label: [usize; 8] = std::array::from_fn(|k| k);
    // A path inside a cube has at most seven edges: seven sweeps settle every group's minimum.
    for _ in 0..7 {
        for (e, &block) in blocked.iter().enumerate() {
            if !block {
                let (lo, hi, _) = cell_edge(e);
                let m = label[lo].min(label[hi]);
                label[lo] = m;
                label[hi] = m;
            }
        }
    }
    let anchor = (0..3).fold(0usize, |k, b| {
        let at = if wall_weight[b] > 0.0 {
            wall_sum[b] / wall_weight[b]
        } else {
            0.5
        };
        let known = wall_weight[b] > 0.0;
        let unknown = cell.guard && has_wall[b] && !known;
        let high = if unknown && cell.normal[b].abs() >= WALL_NORMAL {
            cell.normal[b] > 0.0
        } else {
            cell.f[b] > at
        };
        if high { k | (1 << b) } else { k }
    });
    let mut sum = [0.0f32; 8];
    for (&l, &w) in label.iter().zip(weights) {
        sum[l] += w;
    }
    let best = if sum[label[anchor]] > 0.0 {
        label[anchor]
    } else {
        (1..8).fold(0, |b, l| if sum[l] > sum[b] { l } else { b })
    };
    label
        .iter()
        .enumerate()
        .filter(|&(_, &l)| l == best)
        .fold(0u8, |m, (k, _)| m | (1 << k))
}

fn valid(volume: &BakedGiVolume, i: usize) -> Option<V3> {
    let meta = volume.probe_meta(i);
    (meta.flags & ProbeFlags::VALID != 0).then_some(meta.offset)
}

fn terrain_part(
    volume: &BakedGiVolume,
    settings: &BakedGiSettings,
    light: &Light,
    p: V3,
    n: V3,
    v: V3,
) -> Part {
    let mut out = Part::empty();
    let Some(t) = volume.terrain.as_ref() else {
        return out;
    };
    let levels = t.levels.len();
    if levels == 0 {
        return out;
    }
    let (nx, nz) = (t.counts[0] as usize, t.counts[1] as usize);
    // A layer without columns has no probes (and `n_max` below would be negative).
    if nx == 0 || nz == 0 {
        return out;
    }
    let ground = |i: usize, j: usize| t.ground.get(j * nx + i).copied().unwrap_or(0.0);
    let level = |l: usize| t.levels[l];
    let n_max = [nx as f32 - 1.0, nz as f32 - 1.0];
    let g0 = [
        (p[0] - t.origin[0]) / t.spacing[0],
        (p[2] - t.origin[1]) / t.spacing[1],
    ];
    let inset = g0[0].min(g0[1]).min((n_max[0] - g0[0]).min(n_max[1] - g0[1]));
    let cov_xz = (inset + 1.0).clamp(0.0, 1.0);
    if cov_xz <= 0.0 {
        return out;
    }
    let top = level(levels - 1);
    let column = |g: [f32; 2]| {
        let gc = [g[0].clamp(0.0, n_max[0]), g[1].clamp(0.0, n_max[1])];
        let i0 = [
            gc[0].floor().min((n_max[0] - 1.0).max(0.0)) as usize,
            gc[1].floor().min((n_max[1] - 1.0).max(0.0)) as usize,
        ];
        let i1 = [(i0[0] + 1).min(nx - 1), (i0[1] + 1).min(nz - 1)];
        let f = [gc[0] - i0[0] as f32, gc[1] - i0[1] as f32];
        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let gr = lerp(
            lerp(ground(i0[0], i0[1]), ground(i1[0], i0[1]), f[0]),
            lerp(ground(i0[0], i1[1]), ground(i1[0], i1[1]), f[0]),
            f[1],
        );
        (i0, i1, f, gr)
    };
    let mut s = t.spacing[0].min(t.spacing[1]);
    {
        let (_, _, _, gr) = column(g0);
        let h = p[1] - gr;
        let mut gap = level(0);
        for l in 0..levels.saturating_sub(1) {
            if h >= level(l) {
                gap = level(l + 1) - level(l);
            }
        }
        s = s.min(gap.max(0.25));
    }
    let biased = add(
        p,
        mul(add(mul(n, settings.normal_bias), mul(v, settings.view_bias)), s),
    );
    let g = [
        (biased[0] - t.origin[0]) / t.spacing[0],
        (biased[2] - t.origin[1]) / t.spacing[1],
    ];
    let (i0, i1, f, gr) = column(g);
    let h = biased[1] - gr;
    let (mut l0, mut tl) = (0usize, 0.0f32);
    if h >= top {
        l0 = levels - 1;
    } else if h > level(0) {
        for l in 0..levels.saturating_sub(1) {
            let (lo, hi) = (level(l), level(l + 1));
            if h >= lo && h < hi {
                l0 = l;
                tl = (h - lo) / (hi - lo).max(1e-4);
            }
        }
    }
    let l1 = (l0 + 1).min(levels - 1);
    let cov_y = (2.0 - h / top.max(1e-3)).clamp(0.0, 1.0);
    if cov_y <= 0.0 {
        return out;
    }
    let mut corners = Corners::default();
    for k in 0..8u32 {
        let (ox, oz, ol) = (k & 1 == 1, (k >> 1) & 1 == 1, (k >> 2) & 1 == 1);
        let ci = if ox { i1[0] } else { i0[0] };
        let cj = if oz { i1[1] } else { i0[1] };
        let cl = if ol { l1 } else { l0 };
        let wx = if ox { f[0] } else { 1.0 - f[0] };
        let wz = if oz { f[1] } else { 1.0 - f[1] };
        let wl = if ol { tl } else { 1.0 - tl };
        let index = (cj * nx + ci) * levels + cl;
        let nominal = [
            t.origin[0] + ci as f32 * t.spacing[0],
            ground(ci, cj) + level(cl),
            t.origin[1] + cj as f32 * t.spacing[1],
        ];
        let tri = wx * wz * wl;
        let meta = volume.probe_meta(index);
        corners.visit(k as usize, index, meta.flags, nominal, tri);
        let Some(offset) = valid(volume, index) else {
            continue;
        };
        corners.position[k as usize] = add(nominal, offset);
        let w = corner(
            volume,
            settings,
            index,
            add(nominal, offset),
            p,
            biased,
            n,
            s,
            tri,
        );
        corners.set(k as usize, w, tri);
    }
    // Corner bits: 0 = column x, 1 = column z, 2 = level (the flags' Y axis).
    let cell = CellAxes {
        axis: [0, 2, 1],
        same: [i0[0] == i1[0], i0[1] == i1[1], l0 == l1],
        f: [f[0], f[1], tl],
        normal: [n[0], n[2], n[1]],
        guard: settings.leak_guard,
    };
    corners.add_to(&mut out, volume, light, &cell);
    out.cov = cov_xz * cov_y;
    out
}

fn brick_part(
    volume: &BakedGiVolume,
    settings: &BakedGiSettings,
    light: &Light,
    p: V3,
    n: V3,
    v: V3,
) -> Part {
    let mut out = Part::empty();
    let best = select_brick(volume, p, n, 0..volume.bricks.len());
    let Some((b, _, inset)) = best else {
        return out;
    };
    let brick = &volume.bricks[b];
    out.interior = brick.is_interior();
    out.brick = Some(b);
    brick_light(volume, settings, light, out, (brick, inset), (p, n, v))
}

/// The brick that holds `p` deepest among `candidates` (ascending brick indices; an interior
/// brick first): (brick, selection key, inset). The shader's `bgi_brick` loop, which scans the
/// candidates its lookup grid lists for the texel (ADR-1835); over every brick it is the plain
/// linear scan.
fn select_brick(
    volume: &BakedGiVolume,
    p: V3,
    n: V3,
    candidates: impl Iterator<Item = usize>,
) -> Option<(usize, f32, f32)> {
    // (brick, selection key, inset): an interior brick holding the point beats any other.
    let mut best: Option<(usize, f32, f32)> = None;
    for b in candidates {
        let brick = &volume.bricks[b];
        let interior = brick.is_interior();
        let q = if interior {
            let sp = brick.spacing;
            add(p, mul(n, 0.5 * sp[0].min(sp[1]).min(sp[2])))
        } else {
            p
        };
        let mut inset = f32::INFINITY;
        for (a, qa) in q.iter().enumerate() {
            let g = (qa - brick.origin[a]) / brick.spacing[a];
            inset = inset.min(g).min(brick.counts[a] as f32 - 1.0 - g);
        }
        let key = if interior && inset + INTERIOR_EDGE_SLACK > 0.0 {
            inset + INTERIOR_EDGE_SLACK + INTERIOR_PRIORITY
        } else {
            inset
        };
        if key > best.map_or(0.0, |(_, k, _)| k) {
            best = Some((b, key, inset));
        }
    }
    best
}

/// The light of `brick` (selected at `inset`) at `(p, n, v)`, added to `out`.
fn brick_light(
    volume: &BakedGiVolume,
    settings: &BakedGiSettings,
    light: &Light,
    mut out: Part,
    (brick, inset): (&DetailBrick, f32),
    (p, n, v): (V3, V3, V3),
) -> Part {
    let sp = brick.spacing;
    out.edge = sp[0].max(sp[1]).max(sp[2]);
    let s = sp[0].min(sp[1]).min(sp[2]);
    let biased = add(
        p,
        mul(add(mul(n, settings.normal_bias), mul(v, settings.view_bias)), s),
    );
    let mut i0 = [0usize; 3];
    let mut i1 = [0usize; 3];
    let mut f = [0.0f32; 3];
    for a in 0..3 {
        // Floored at 0: a NaN point can select a brick with a zero count (`clamp` panics when
        // its bounds cross).
        let c_max = (brick.counts[a] as f32 - 1.0).max(0.0);
        let g = ((biased[a] - brick.origin[a]) / sp[a]).clamp(0.0, c_max);
        i0[a] = g.floor().min((c_max - 1.0).max(0.0)) as usize;
        i1[a] = (i0[a] + 1).min((brick.counts[a] as usize).saturating_sub(1));
        f[a] = g - i0[a] as f32;
    }
    let mut corners = Corners::default();
    for k in 0..8u32 {
        let o = [k & 1 == 1, (k >> 1) & 1 == 1, (k >> 2) & 1 == 1];
        let cell: [usize; 3] = std::array::from_fn(|a| if o[a] { i1[a] } else { i0[a] });
        let wa: [f32; 3] = std::array::from_fn(|a| if o[a] { f[a] } else { 1.0 - f[a] });
        let index = brick.index(cell[0] as u32, cell[1] as u32, cell[2] as u32);
        let nominal = brick.nominal_position(cell[0] as u32, cell[1] as u32, cell[2] as u32);
        let tri = wa[0] * wa[1] * wa[2];
        let meta = volume.probe_meta(index);
        corners.visit(k as usize, index, meta.flags, nominal, tri);
        let Some(offset) = valid(volume, index) else {
            continue;
        };
        corners.position[k as usize] = add(nominal, offset);
        let w = corner(
            volume,
            settings,
            index,
            add(nominal, offset),
            p,
            biased,
            n,
            s,
            tri,
        );
        corners.set(k as usize, w, tri);
    }
    let cell = CellAxes {
        axis: [0, 1, 2],
        same: std::array::from_fn(|a| i0[a] == i1[a]),
        f,
        normal: n,
        guard: settings.leak_guard,
    };
    corners.add_to(&mut out, volume, light, &cell);
    out.cov = if out.interior { 1.0 } else { inset.clamp(0.0, 1.0) };
    out
}

/// One channel's irradiance / π toward `n` from its L00 `l0` and L1 `l1` (`baked_gi.wgsl`
/// `bgi_channel_irradiance`): `r0 + r1·n` with `r0 = Y00·L00` and `r1 = (2/3)·Y1·L1`, clamped
/// at 0. With `dering` and `|r1| > r0` (the linear form goes negative behind the light) it is the
/// lobe `r0·(p + 1)·q^p`, `q = (1 + r̂1·n) / 2`: over the sphere its mean is `r0` and its first
/// moment `r1` for `p = 4x / (3 − 2x)`, `x = |r1| / (2·r0)` clamped to 1, it is never negative,
/// and at `|r1| = r0` it equals the linear form (ADR-861).
fn channel_irradiance(l0: f32, l1: V3, n: V3, dering: bool) -> f32 {
    let a0 = regia_asset::baked_gi::SH_Y00;
    let a1 = regia_asset::baked_gi::SH_Y1 * (2.0 / 3.0);
    let r0 = a0 * l0;
    let len = a1 * length(l1);
    if dering && r0 > 0.0 && len > r0 {
        let x = (len / (2.0 * r0)).min(1.0);
        let p = 4.0 * x / (3.0 - 2.0 * x);
        let q = (0.5 * (1.0 + a1 * dot(l1, n) / len)).max(1e-6);
        return r0 * (p + 1.0) * q.powf(p);
    }
    (r0 + a1 * dot(l1, n)).max(0.0)
}

/// Irradiance / π toward `n` from summed SH, per channel ([`channel_irradiance`]).
fn irradiance_over_pi(sh: &ShRgb, n: V3, dering: bool) -> V3 {
    std::array::from_fn(|ch| channel_irradiance(sh[ch][0], [sh[ch][1], sh[ch][2], sh[ch][3]], n, dering))
}

/// How the lookup turns summed SH into irradiance (`bgi_irradiance_over_pi`): the ZH3 flag and
/// the de-ringing flag of [`BakedGiSettings`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GiEval {
    /// [`BakedGiSettings::zh3`].
    pub(crate) zh3: bool,
    /// [`BakedGiSettings::deringing`] (only without ZH3).
    pub(crate) dering: bool,
}

impl GiEval {
    /// The evaluation `settings` select.
    pub(crate) fn of(settings: &BakedGiSettings) -> Self {
        Self {
            zh3: settings.zh3,
            dering: settings.deringing,
        }
    }

    /// Irradiance / π toward `n` from the summed SH `sh`: ZH3 ([`zh3_irradiance_over_pi`]) or
    /// L1 per channel (`channel_irradiance`).
    pub(crate) fn irradiance(self, sh: &ShRgb, n: V3) -> V3 {
        if self.zh3 {
            zh3_irradiance_over_pi(sh, n)
        } else {
            irradiance_over_pi(sh, n, self.dering)
        }
    }
}

/// The baked irradiance at `p` (unit normal `n`, unit `v` toward the viewer) with the
/// component factors `scales` ([`component_scales`]): the CPU mirror of the shader's
/// `static_gi` (the same corners, weights and blends), for gameplay queries and tests.
///
/// Inside an interior brick ([`regia_asset::baked_gi::BrickFlags::INTERIOR`]) only that
/// brick lights the point, at full coverage (black if none of its corners is valid). "Inside"
/// is tested at the point pushed half a cell along `n`, against the probe box grown by an
/// eighth of a cell (ADR-1364), so surfaces on the box's faces stay inside. Corners behind a
/// baked wall ([`regia_asset::baked_gi::ProbeFlags::WALLS`], ADR-366) are skipped. Capsule
/// occluders are not applied here: multiply by [`capsule_visibility`].
///
/// The sun bounce is as baked ([`SunRelight::NONE`]); [`sample_static_gi_relit`] applies the
/// frame's [`sun_relight`] as the renderer does.
#[must_use]
pub fn sample_static_gi(
    volume: &BakedGiVolume,
    settings: &BakedGiSettings,
    scales: &[[f32; 3]; GI_COMPONENTS],
    p: [f32; 3],
    n: [f32; 3],
    v: [f32; 3],
) -> StaticGiSample {
    sample_static_gi_relit(volume, settings, scales, &SunRelight::NONE, p, n, v)
}

/// [`sample_static_gi`] under the sun relight `relight` ([`sun_relight`] of the frame's lights,
/// ADR-861): the lookup the renderer runs while the sun has moved since the bake.
#[must_use]
pub fn sample_static_gi_relit(
    volume: &BakedGiVolume,
    settings: &BakedGiSettings,
    scales: &[[f32; 3]; GI_COMPONENTS],
    relight: &SunRelight,
    p: [f32; 3],
    n: [f32; 3],
    v: [f32; 3],
) -> StaticGiSample {
    sample_of(
        volume_sh(volume, settings, scales, relight.ground_factor(), p, n, v),
        settings,
        n,
    )
}

/// The [`StaticGiSample`] toward `n` of one volume's lookup ([`volume_sh`]).
fn sample_of(g: Option<VolumeSh>, settings: &BakedGiSettings, n: V3) -> StaticGiSample {
    let Some(g) = g else {
        return StaticGiSample::default();
    };
    if !g.lit {
        // An interior brick with no valid corner: black at full coverage.
        return StaticGiSample {
            coverage: g.cov,
            ..StaticGiSample::default()
        };
    }
    let mut total = [[0.0f32; 4]; 3];
    let mut components = [[0.0f32; 3]; GI_COMPONENTS];
    for (c, comp) in g.sh.iter().enumerate() {
        for ch in 0..3 {
            for m in 0..4 {
                total[ch][m] += comp[ch][m];
            }
        }
        components[c] = GiEval::of(settings).irradiance(comp, n);
    }
    StaticGiSample {
        irradiance_over_pi: GiEval::of(settings).irradiance(&total, n),
        components,
        coverage: g.cov,
        visibility: g.vis,
        ..StaticGiSample::from_split(g.cov, &g.split)
    }
}

/// One volume's lookup before the irradiance, as `static_gi` in `baked_gi.wgsl` returns it: the
/// per-component SH at full strength (× intensity over the visibility-weighted share), the
/// coverage and the visibility. `None` where the volume covers nothing; `lit` is false for an
/// interior brick with no valid corner (black at full coverage). Shared by
/// [`sample_static_gi_relit`] and the multi-volume blend (`blend.rs`).
struct VolumeSh {
    sh: [ShRgb; GI_COMPONENTS],
    cov: f32,
    vis: f32,
    lit: bool,
    split: LayerSplit,
}

/// How a lookup's light divides between the layers, for the probe-relative AO split (ADR-1104,
/// `baked_gi.wgsl` `StaticGi::split`): the fractions of the light the detail brick and the
/// terrain layer hold (they sum to 1 where the volume lights the point, 0 otherwise) and their
/// cell sizes (m: the brick's largest cell edge, the terrain layer's largest column spacing; 0
/// for a layer that holds nothing). Several volumes blend it by their share of the light, each
/// cell size weighted by its layer's light.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct LayerSplit {
    brick: f32,
    terrain: f32,
    brick_cell: f32,
    terrain_cell: f32,
}

impl LayerSplit {
    /// Adds `other` at weight `w` to a running sum (the cell sizes summed weighted by their
    /// layer's light; [`Self::finish`] averages them).
    fn accumulate(&mut self, other: &Self, w: f32) {
        self.brick += w * other.brick;
        self.terrain += w * other.terrain;
        self.brick_cell += w * other.brick * other.brick_cell;
        self.terrain_cell += w * other.terrain * other.terrain_cell;
    }

    /// A running sum ([`Self::accumulate`]) with its fractions scaled by `k` (1 / the summed
    /// weight) and its cell sizes averaged.
    fn finish(&self, k: f32) -> Self {
        let avg = |sum: f32, w: f32| if w > 0.0 { sum / w } else { 0.0 };
        Self {
            brick: self.brick * k,
            terrain: self.terrain * k,
            brick_cell: avg(self.brick_cell, self.brick),
            terrain_cell: avg(self.terrain_cell, self.terrain),
        }
    }
}

/// [`VolumeSh`] of `volume` at `p` with the component factors `scales` and the sun-bounce
/// ground factor `ground` ([`SunRelight::ground_factor`]).
fn volume_sh(
    volume: &BakedGiVolume,
    settings: &BakedGiSettings,
    scales: &[[f32; 3]; GI_COMPONENTS],
    ground: f32,
    p: [f32; 3],
    n: [f32; 3],
    v: [f32; 3],
) -> Option<VolumeSh> {
    lookup(volume, settings, scales, ground, p, n, v).sh
}

/// What [`lookup`] found: the volume's [`VolumeSh`], the brick part, the terrain part when it
/// was consulted (not inside an interior brick, and the brick left coverage), and the coverage
/// each layer took (`[brick, terrain]`). The parts feed the debug trace (`debug.rs`).
struct Lookup {
    sh: Option<VolumeSh>,
    brick: Part,
    terrain: Option<Part>,
    taken: [f32; 2],
}

/// The lookup behind [`volume_sh`], keeping its parts.
fn lookup(
    volume: &BakedGiVolume,
    settings: &BakedGiSettings,
    scales: &[[f32; 3]; GI_COMPONENTS],
    ground: f32,
    p: [f32; 3],
    n: [f32; 3],
    v: [f32; 3],
) -> Lookup {
    let light = Light { scales, ground };
    let br = brick_part(volume, settings, &light, p, n, v);
    let (mut wb, mut vb) = (0.0, 0.0);
    if br.wsum > 0.0 && br.cov > 0.0 {
        wb = br.cov;
        vb = br.visibility();
    }
    let mut cov = wb;
    let (mut wt, mut vt) = (0.0, 0.0);
    let mut terrain = None;
    if br.interior {
        cov = 1.0;
    } else if wb < 1.0 {
        let tr = terrain_part(volume, settings, &light, p, n, v);
        if tr.wsum > 0.0 && tr.cov > 0.0 {
            // The terrain share is what the brick leaves, at the terrain's own coverage.
            wt = (1.0 - wb) * tr.cov;
            vt = tr.visibility();
            cov += wt;
        }
        terrain = Some(tr);
    }
    let sh = 'sh: {
        if cov <= 1e-5 {
            break 'sh None;
        }
        let mut sh = [[[0.0f32; 4]; 3]; GI_COMPONENTS];
        let mut accumulate = |part: &Part, k: f32| {
            for (dst, src) in sh.iter_mut().zip(&part.sh) {
                for ch in 0..3 {
                    for m in 0..4 {
                        dst[ch][m] += src[ch][m] * k;
                    }
                }
            }
        };
        // Each layer's share weighted by how much of its cell sees the receiver.
        let kb = wb * (vb + SEEN_FLOOR);
        let kt = wt * (vt + SEEN_FLOOR);
        if kb > 0.0 {
            accumulate(&br, kb / br.wsum);
        }
        if kt > 0.0
            && let Some(tr) = &terrain
        {
            accumulate(tr, kt / tr.wsum);
        }
        let total_k = kb + kt;
        if total_k <= 0.0 {
            // An interior brick with no valid corner: black at full coverage.
            break 'sh Some(VolumeSh {
                sh: [[[0.0; 4]; 3]; GI_COMPONENTS],
                cov,
                vis: 0.0,
                lit: false,
                split: LayerSplit::default(),
            });
        }
        let visibility = (wb * vb + wt * vt) / (wb + wt);
        let k = settings.intensity / total_k;
        for comp in &mut sh {
            for ch in comp.iter_mut() {
                for m in ch.iter_mut() {
                    *m *= k;
                }
            }
        }
        let terrain_cell = volume
            .terrain
            .as_ref()
            .map_or(0.0, |t| t.spacing[0].max(t.spacing[1]));
        Some(VolumeSh {
            sh,
            cov,
            vis: visibility,
            lit: true,
            split: LayerSplit {
                brick: kb / total_k,
                terrain: kt / total_k,
                brick_cell: if kb > 0.0 { br.edge } else { 0.0 },
                terrain_cell: if kt > 0.0 { terrain_cell } else { 0.0 },
            },
        })
    };
    Lookup {
        sh,
        brick: br,
        terrain,
        taken: [wb, wt],
    }
}

// ---- GPU ----

/// `BakedGiUniform` in `baked_gi.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct BakedGiUniformGpu {
    terrain: [f32; 4],
    counts: [u32; 4],
    levels: [f32; 8],
    words: [u32; 4],
    extra: [u32; 4],
    params: [f32; 4],
    scales: [[f32; 4]; GI_COMPONENTS],
    /// x = the sun-bounce ground factor, y = the light-bleeding reduction (ADR-1105).
    relight: [f32; 4],
}

impl BakedGiUniformGpu {
    /// The uniform for `volume` laid out at `words`, with the lookup `settings`, the component
    /// factors `scales` ([`component_scales`]; divided here by the volume's encode exposure) and
    /// the sun relight `relight` ([`sun_relight`]).
    fn of(
        volume: &BakedGiVolume,
        words: &VolumeWords,
        settings: &BakedGiSettings,
        scales: &[[f32; 3]; GI_COMPONENTS],
        relight: &SunRelight,
    ) -> Self {
        let exposure = volume.exposure();
        let t = volume.terrain.as_ref();
        let mut levels = [0.0f32; 8];
        if let Some(t) = t {
            for (dst, l) in levels.iter_mut().zip(&t.levels) {
                *dst = *l;
            }
        }
        let w = words;
        Self {
            terrain: t.map_or([0.0, 0.0, 1.0, 1.0], |t| {
                [t.origin[0], t.origin[1], t.spacing[0], t.spacing[1]]
            }),
            counts: [
                t.map_or(0, |t| t.counts[0]),
                t.map_or(0, |t| t.counts[1]),
                t.map_or(0, |t| t.levels.len() as u32),
                volume.bricks.len() as u32,
            ],
            levels,
            words: [w.meta as u32, w.sh as u32, w.moments as u32, w.ground as u32],
            extra: [
                w.bricks as u32,
                volume.moment_resolution.max(1),
                if settings.deringing { LOOKUP_DERING } else { 0 }
                    | if settings.leak_guard { LOOKUP_LEAK_GUARD } else { 0 }
                    | if settings.visibility_filter == GiVisibilityFilter::PerTap {
                        LOOKUP_PER_TAP
                    } else {
                        0
                    }
                    | if settings.zh3 { LOOKUP_ZH3 } else { 0 },
                volume.probe_count() as u32,
            ],
            params: [
                volume.max_distance,
                settings.normal_bias,
                settings.view_bias,
                settings.intensity,
            ],
            scales: std::array::from_fn(|c| {
                let e = exposure[c].max(f32::MIN_POSITIVE);
                [scales[c][0] / e, scales[c][1] / e, scales[c][2] / e, 0.0]
            }),
            relight: [
                relight.ground_factor(),
                settings.effective_leak_reduction(),
                0.0,
                0.0,
            ],
        }
    }
}

/// `BakedGiLayerGpu` in `baked_gi_set.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct BakedGiLayerGpu {
    rect: [f32; 4],
    fade: [f32; 4],
    /// x = tier, y = volume slot, z = 1 with a per-edge split fade (`edges`, `bands`).
    tier: [u32; 4],
    edges: [f32; 4],
    bands: [f32; 4],
    /// x = min Y, y = max Y, z = Y reach, w = 1 when the volume is a closed brick box
    /// (`BakedGiFootprint::y_closed`). 0 for a terrain layer, which stays open in Y.
    vertical: [f32; 4],
}

/// `BakedGiSet` in `baked_gi_set.wgsl`: the uniform of the screen pass.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct BakedGiSetGpu {
    count: [u32; 4],
    volumes: [BakedGiUniformGpu; MAX_BAKED_GI_VOLUMES],
    layers: [BakedGiLayerGpu; MAX_BAKED_GI_VOLUMES],
}

// Counts `BrickGrid` builds on this thread. Steady frames must not add to it.
#[cfg(test)]
thread_local! {
    static BRICK_GRID_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Builds the brick lookup grid. Tests count every call on this thread.
/// Notes one render-side brick-grid request (a test counter). The grid itself is the volume's own
/// cached one (`BakedGiVolume::brick_grid_*`, built once per volume by regia_asset).
fn note_brick_grid_build() {
    #[cfg(test)]
    BRICK_GRID_BUILDS.with(|n| n.set(n.get() + 1));
}

/// Brick-grid sizes for one volume, keyed by its `Arc` address. The grid is a pure function of
/// the brick table, and a volume's table does not change while that `Arc` is alive.
#[derive(Clone)]
struct GridMemo {
    id: usize,
    words: usize,
    cells: usize,
    entries: usize,
    /// `BrickGrid::to_words` from the build that filled this row, until the first upload takes it.
    upload: Vec<u32>,
}

fn volume_grid_id(volume: &Arc<BakedGiVolume>) -> usize {
    Arc::as_ptr(volume) as usize
}

/// Word offsets of the volume buffer's sections.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct VolumeWords {
    meta: usize,
    sh: usize,
    moments: usize,
    ground: usize,
    bricks: usize,
    /// The brick lookup grid (`BrickGrid`, ADR-1835), after the brick table.
    grid: usize,
    end: usize,
    /// The storage slot whose buffer these offsets index (ADR-2618).
    slot: usize,
}

impl VolumeWords {
    /// Section offsets for `volume` when its brick grid is `grid_words` words. Does not build the grid.
    fn layout(volume: &BakedGiVolume, grid_words: usize) -> Self {
        let meta = 0;
        let sh = meta + volume.meta_bytes().len() / 4;
        let moments = sh + volume.sh_bytes().len() / 4;
        let ground = moments + volume.moment_bytes().len() / 4;
        let bricks = ground + volume.terrain.as_ref().map_or(0, |t| t.ground.len());
        let grid = bricks + volume.bricks.len() * BRICK_WORDS;
        let end = grid + grid_words;
        Self {
            meta,
            sh,
            moments,
            ground,
            bricks,
            grid,
            end,
            slot: 0,
        }
    }

    fn of(volume: &BakedGiVolume) -> Self {
        note_brick_grid_build();
        Self::layout(volume, volume.brick_grid_word_count())
    }

    /// The same sections starting `base` words into a shared buffer.
    fn shifted(self, base: usize) -> Self {
        Self {
            meta: self.meta + base,
            sh: self.sh + base,
            moments: self.moments + base,
            ground: self.ground + base,
            bricks: self.bricks + base,
            grid: self.grid + base,
            end: self.end + base,
            slot: self.slot,
        }
    }

    /// The same sections in storage slot `slot`.
    fn in_slot(self, slot: usize) -> Self {
        Self { slot, ..self }
    }
}

/// Packs volumes of `sizes` words (blend order) into at most [`BAKED_GI_VOLUME_SLOTS`] storage
/// slots of `slot_words` words each, first fit: each volume's slot and first word, or `None`
/// for a volume that fits no slot (larger than one, or every slot full). A device whose one
/// binding holds them all keeps them all in slot 0, one after another, as before ADR-2618.
fn pack_slots(sizes: &[usize], slot_words: u64) -> Vec<Option<(usize, usize)>> {
    let mut ends = [0usize; BAKED_GI_VOLUME_SLOTS];
    sizes
        .iter()
        .map(|&n| {
            let slot = (0..BAKED_GI_VOLUME_SLOTS).find(|&s| (ends[s] + n) as u64 <= slot_words)?;
            let at = ends[slot];
            ends[slot] += n;
            Some((slot, at))
        })
        .collect()
}

/// The blended volumes on the GPU, one after another in one buffer per storage slot.
struct VolumeGpu {
    /// The volumes it holds, in blend order (identity: `Arc::ptr_eq`).
    sources: Vec<Arc<BakedGiVolume>>,
    /// One buffer per slot in use (at least one, at most [`BAKED_GI_VOLUME_SLOTS`]).
    buffers: Vec<wgpu::Buffer>,
    /// Each volume's sections in its slot's buffer.
    words: Vec<VolumeWords>,
}

/// The same volumes, in the same order, by identity.
fn same_volumes(a: &[Arc<BakedGiVolume>], b: &[&BakedGiLayer]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| Arc::ptr_eq(x, &y.volume))
}

/// The screen pass's uniform for the blended layers `used` (sections at `words`).
fn set_uniform(
    used: &[&BakedGiLayer],
    words: &[VolumeWords],
    settings: &BakedGiSettings,
    lights: &BakedGiFrameLights,
) -> BakedGiSetGpu {
    let mut set: BakedGiSetGpu = bytemuck::Zeroable::zeroed();
    let n = used.len().min(MAX_BAKED_GI_VOLUMES);
    set.count = [
        n as u32,
        u32::from(blend::single_volume(used)),
        u32::from(settings.brick_visit_probe),
        0,
    ];
    let fades = blend::split_fades(used);
    let mut tier = 0u32;
    for (k, (layer, w)) in used.iter().zip(words).take(n).enumerate() {
        if k > 0 && layer.priority != used[k - 1].priority {
            tier += 1;
        }
        let scales = component_scales(&layer.volume, settings, lights);
        let relight = sun_relight(&layer.volume, settings, lights);
        set.volumes[k] = BakedGiUniformGpu::of(&layer.volume, w, settings, &scales, &relight);
        let fp = BakedGiFootprint::of(&layer.volume).unwrap_or_default();
        let vertical = if fp.y_closed {
            [fp.min_y, fp.max_y, fp.reach_y, 1.0]
        } else {
            [0.0; 4]
        };
        set.layers[k] = BakedGiLayerGpu {
            rect: [fp.min[0], fp.min[1], fp.max[0], fp.max[1]],
            fade: [
                layer.blend_distance,
                layer.clamped_weight(),
                fp.reach[0],
                fp.reach[1],
            ],
            tier: [tier, w.slot as u32, u32::from(fades[k].is_some()), 0],
            edges: fades[k].map_or([0.0; 4], |f| f.edges),
            bands: fades[k].map_or([0.0; 4], |f| f.bands),
            vertical,
        };
    }
    set
}

/// Writes the volume buffer's contents through `put(byte offset, bytes)` (`words.end × 4`
/// bytes in all): the file sections unchanged, then the ground heights, the brick table and its lookup grid.
fn write_volume(volume: &BakedGiVolume, words: &VolumeWords, put_bytes: impl FnMut(usize, &[u8])) {
    note_brick_grid_build();
    let grid = volume.brick_grid_words();
    write_volume_grid(volume, words, &grid, put_bytes);
}

/// [`write_volume`] with a grid already built. `grid.len()` is `words.end - words.grid`.
fn write_volume_grid(
    volume: &BakedGiVolume,
    words: &VolumeWords,
    grid: &[u32],
    mut put_bytes: impl FnMut(usize, &[u8]),
) {
    debug_assert_eq!(grid.len(), words.end.saturating_sub(words.grid));
    let mut put = |word: usize, bytes: &[u8]| {
        if !bytes.is_empty() {
            put_bytes(word * 4, bytes);
        }
    };
    put(words.meta, volume.meta_bytes());
    put(words.sh, volume.sh_bytes());
    put(words.moments, volume.moment_bytes());
    if let Some(t) = &volume.terrain {
        let ground: &[u8] = bytemuck::cast_slice(&t.ground);
        put(words.ground, ground);
    }
    for (b, brick) in volume.bricks.iter().enumerate() {
        let rec: [u32; BRICK_WORDS] = [
            brick.origin[0].to_bits(),
            brick.origin[1].to_bits(),
            brick.origin[2].to_bits(),
            brick.first_probe,
            brick.spacing[0].to_bits(),
            brick.spacing[1].to_bits(),
            brick.spacing[2].to_bits(),
            brick.flags,
            brick.counts[0],
            brick.counts[1],
            brick.counts[2],
            0,
        ];
        put(words.bricks + b * BRICK_WORDS, bytemuck::cast_slice(&rec));
    }
    put(words.grid, bytemuck::cast_slice(grid));
}

fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn storage_entry(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
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

fn uniform_buffer(d: &wgpu::Device, label: &str, size: u64) -> wgpu::Buffer {
    d.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// Pipelines and per-frame buffers of the screen pass.
struct ScreenGpu {
    pipeline: wgpu::ComputePipeline,
    core_bgl: wgpu::BindGroupLayout,
    screen_bgl: wgpu::BindGroupLayout,
    /// `DdgiUniform` the shading reads (screen lookup only).
    shade_uniform: wgpu::Buffer,
    /// `BakedGiOccluders` (binding 4) and what it holds (see `stage_occluders`).
    occ_uniform: wgpu::Buffer,
    occ_bytes: Vec<u8>,
    shade_bytes: Vec<u8>,
    bgi_uniform: wgpu::Buffer,
    bgi_bytes: Vec<u8>,
    params: wgpu::Buffer,
    params_bytes: Vec<u8>,
    /// The screen lookup words and their capacity in texels.
    data: Option<(wgpu::Buffer, u64)>,
    core_bg: Option<wgpu::BindGroup>,
    screen_bgs: BindCache<wgpu::TextureView, wgpu::BindGroup>,
    /// The brick-loop probe's histogram (binding 8, [`BRICK_VISIT_BINS`] words).
    visit_hist: wgpu::Buffer,
    /// Its read-back (created on the probe's first frame).
    visits: Option<VisitReadback>,
}

/// [`VisitReadback::state`] values.
const VISIT_IDLE: u8 = 0;
const VISIT_COPIED: u8 = 1;
const VISIT_MAPPING: u8 = 2;
const VISIT_MAPPED: u8 = 3;

/// The brick-loop probe's read-back without a stall: a frame copies the histogram into the
/// staging buffer, the next maps it, and a later one reads it once the map is done.
struct VisitReadback {
    staging: wgpu::Buffer,
    state: Arc<std::sync::atomic::AtomicU8>,
    last: Option<BrickVisits>,
    captures: u64,
}

impl VisitReadback {
    fn new(d: &wgpu::Device) -> Self {
        Self {
            staging: d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("baked gi brick visits readback"),
                size: (BRICK_VISIT_BINS * 4) as u64,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            state: Arc::new(std::sync::atomic::AtomicU8::new(VISIT_IDLE)),
            last: None,
            captures: 0,
        }
    }

    /// Advances the read-back at the start of a frame (the previous frame's copy is submitted):
    /// maps a copied histogram, reads a mapped one.
    fn advance(&mut self) {
        use std::sync::atomic::Ordering;
        match self.state.load(Ordering::Acquire) {
            VISIT_COPIED => {
                self.state.store(VISIT_MAPPING, Ordering::Release);
                let state = self.state.clone();
                self.staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
                    state.store(
                        if r.is_ok() { VISIT_MAPPED } else { VISIT_IDLE },
                        Ordering::Release,
                    );
                });
            }
            VISIT_MAPPED => {
                let words: Option<Vec<u32>> = self
                    .staging
                    .slice(..)
                    .get_mapped_range()
                    .ok()
                    .map(|view| bytemuck::pod_collect_to_vec(&view));
                self.staging.unmap();
                if let Some(words) = words {
                    self.captures += 1;
                    self.last = Some(BrickVisits::of(words, self.captures));
                }
                self.state.store(VISIT_IDLE, Ordering::Release);
            }
            _ => {}
        }
    }
}

impl ScreenGpu {
    fn new(d: &wgpu::Device) -> Self {
        let core_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("baked gi core bgl"),
            entries: &[
                uniform_entry(0),
                storage_entry(1, false),
                uniform_entry(2),
                storage_entry(3, true),
                uniform_entry(4),
                storage_entry(5, true),
                storage_entry(6, true),
                storage_entry(7, true),
                storage_entry(8, false),
            ],
        });
        let screen_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("baked gi screen bgl"),
            entries: &[
                uniform_entry(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("baked gi screen"),
            source: wgpu::ShaderSource::Wgsl(screen_source().into()),
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("baked gi screen"),
            bind_group_layouts: &[Some(&core_bgl), Some(&screen_bgl)],
            immediate_size: 0,
        });
        let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("baked gi screen lookup"),
            layout: Some(&layout),
            module: &module,
            entry_point: Some("baked_screen_lookup"),
            compilation_options: Default::default(),
            cache: crate::pipeline_cache::cache_for(&d).as_deref(),
        });
        Self {
            pipeline,
            core_bgl,
            screen_bgl,
            shade_uniform: uniform_buffer(d, "baked gi shading uniform", size_of::<DdgiUniformGpu>() as u64),
            shade_bytes: Vec::new(),
            occ_uniform: uniform_buffer(d, "baked gi occluders", occluders::OCCLUDER_UNIFORM_BYTES),
            // A new buffer is zero-initialized: a zero count.
            occ_bytes: vec![0; 16],
            bgi_uniform: uniform_buffer(d, "baked gi uniform", size_of::<BakedGiSetGpu>() as u64),
            bgi_bytes: Vec::new(),
            params: uniform_buffer(d, "baked gi screen params", size_of::<ScreenParamsGpu>() as u64),
            params_bytes: Vec::new(),
            data: None,
            core_bg: None,
            screen_bgs: BindCache::default(),
            visit_hist: d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("baked gi brick visits"),
                size: (BRICK_VISIT_BINS * 4) as u64,
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_SRC
                    | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            visits: None,
        }
    }
}

/// The renderer's baked GI state: the installed volume, its settings and GPU resources.
#[derive(Default)]
pub(crate) struct BakedGiRuntime {
    /// The installed volume ([`crate::FrameRenderer::set_baked_gi`]).
    pub(crate) volume: Option<Arc<BakedGiVolume>>,
    /// More volumes blended with it ([`crate::FrameRenderer::set_baked_gi_layers`]).
    pub(crate) layers: Vec<BakedGiLayer>,
    pub(crate) settings: BakedGiSettings,
    /// `Some(false)` never uses the volume, `Some(true)` uses it whatever the frame quality's
    /// [`crate::FrameQuality::baked_gi`] says; `None` follows the quality.
    pub(crate) enabled: Option<bool>,
    screen: Option<ScreenGpu>,
    gpu: Option<VolumeGpu>,
    /// Brick-grid sizes of the volumes held here. Filled when a volume is installed and reused
    /// by later frames and by [`Self::status`].
    grid_memo: Vec<GridMemo>,
    /// The volume lights this frame.
    pub(crate) active: bool,
    /// Bumped whenever the shading buffers change (the view bind groups that bind them rebuild).
    pub(crate) generation: u64,
    status: BakedGiStatus,
    /// Half-resolution size of this frame's lookup.
    half: (u32, u32),
    /// The volume whose sun mismatch was already warned about.
    warned: Option<usize>,
    /// Capsule occluders for dynamic units (usable, at most [`MAX_GI_OCCLUDERS`]).
    pub(crate) occluders: Vec<GiCapsule>,
    /// Capsules the last `set_occluders` dropped.
    occluders_dropped: usize,
    /// The lights of the last frame `begin` prepared (`sample`).
    last_lights: BakedGiFrameLights,
    /// The SSAO kernel's radius (m) when it runs this frame, else 0 (set before [`Self::begin`]).
    pub(crate) ssao_radius: f32,
    /// The installed volume's [`BakedEmitterSet`], built on first use.
    emitter_set: Option<Arc<BakedEmitterSet>>,
    /// A version 1/2 volume (no emitter list) was warned about.
    warned_emitters: bool,
}

impl BakedGiRuntime {
    /// Installs (or with `None` removes) the volume.
    pub(crate) fn install(&mut self, volume: Option<Arc<BakedGiVolume>>) {
        let same = match (&self.volume, &volume) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.volume = volume;
        if self.volume.is_none() && self.layers.is_empty() {
            self.gpu = None;
        }
        self.emitter_set = None;
        self.warned_emitters = false;
        self.status.error = None;
        self.active = false;
        self.sync_grid_memo();
    }

    /// Drops grid rows for volumes this runtime no longer holds, and builds a row for each
    /// volume it does hold that it has not seen.
    fn sync_grid_memo(&mut self) {
        let mut keep = Vec::with_capacity(1 + self.layers.len());
        if let Some(volume) = &self.volume {
            keep.push(volume_grid_id(volume));
        }
        for layer in &self.layers {
            keep.push(volume_grid_id(&layer.volume));
        }
        self.grid_memo.retain(|memo| keep.contains(&memo.id));
        let volumes = self.live_volume_arcs();
        for volume in &volumes {
            self.ensure_grid(volume);
        }
    }

    fn live_volume_arcs(&self) -> Vec<Arc<BakedGiVolume>> {
        let mut volumes = Vec::with_capacity(1 + self.layers.len());
        if let Some(volume) = &self.volume {
            volumes.push(Arc::clone(volume));
        }
        for layer in &self.layers {
            volumes.push(Arc::clone(&layer.volume));
        }
        volumes
    }

    /// Word count of `volume`'s brick grid. The first time this runtime sees the `Arc` it builds
    /// the grid and keeps the upload words for [`Self::take_grid_upload`].
    fn ensure_grid(&mut self, volume: &Arc<BakedGiVolume>) -> usize {
        let id = volume_grid_id(volume);
        if let Some(memo) = self.grid_memo.iter().find(|memo| memo.id == id) {
            return memo.words;
        }
        note_brick_grid_build();
        let words = volume.brick_grid_word_count();
        self.grid_memo.push(GridMemo {
            id,
            words,
            cells: volume.brick_grid_cell_count(),
            entries: volume.brick_grid_entry_count(),
            upload: volume.brick_grid_words(),
        });
        words
    }

    /// The grid words for an upload. Uses the words stored by [`Self::ensure_grid`] the first
    /// time, and builds again only when those words were already taken (a later re-upload).
    fn take_grid_upload(&mut self, volume: &Arc<BakedGiVolume>) -> Vec<u32> {
        let id = volume_grid_id(volume);
        let Some(index) = self.grid_memo.iter().position(|memo| memo.id == id) else {
            note_brick_grid_build();
            return volume.brick_grid_words();
        };
        let memo = &mut self.grid_memo[index];
        if memo.words == 0 || !memo.upload.is_empty() {
            return std::mem::take(&mut memo.upload);
        }
        note_brick_grid_build();
        volume.brick_grid_words()
    }

    /// The installed volume's baked emitters (ADR-1386), `None` without a volume, when its
    /// emitter light is scaled away (`lights_emitters`), or when the volume does not name them
    /// (format v1/v2, warned once).
    pub(crate) fn emitter_set(&mut self) -> Option<Arc<BakedEmitterSet>> {
        if !lights_emitters(&self.settings) {
            return None;
        }
        let volume = self.volume.as_ref()?;
        let Some(list) = volume.emitters.as_ref() else {
            if !self.warned_emitters {
                self.warned_emitters = true;
                tracing::warn!(
                    target: "regia_render",
                    "the baked GI volume does not name its emitters (format v1/v2): emissive area lights add to its emissive light; rebake the map with this build (ADR-1386)"
                );
            }
            return None;
        };
        Some(
            self.emitter_set
                .get_or_insert_with(|| Arc::new(BakedEmitterSet::new(list)))
                .clone(),
        )
    }

    /// Replaces the layers blended with the installed volume; returns how many volumes the
    /// frame will blend (the installed one included, at most [`MAX_BAKED_GI_VOLUMES`]).
    pub(crate) fn set_layers(&mut self, layers: &[BakedGiLayer]) -> usize {
        let changed = self.layers.len() != layers.len()
            || self.layers.iter().zip(layers).any(|(a, b)| {
                !Arc::ptr_eq(&a.volume, &b.volume)
                    || a.priority != b.priority
                    || a.blend_distance.to_bits() != b.blend_distance.to_bits()
                    || a.weight.to_bits() != b.weight.to_bits()
            });
        if changed {
            let new_volumes = self.layers.len() != layers.len()
                || self
                    .layers
                    .iter()
                    .zip(layers)
                    .any(|(a, b)| !Arc::ptr_eq(&a.volume, &b.volume));
            self.layers = layers.to_vec();
            if new_volumes {
                // A volume that did not fit may fit in the new set.
                self.status.error = None;
            }
            if self.volume.is_none() && self.layers.is_empty() {
                self.gpu = None;
            }
            self.sync_grid_memo();
        }
        order_layers(&self.effective_layers()).0.len()
    }

    /// The blended baked irradiance at `p` (unit normal `n`, unit `v` toward the viewer) with
    /// the last prepared frame's lights ([`sample_static_gi_blend`]).
    pub(crate) fn sample(&self, p: [f32; 3], n: [f32; 3], v: [f32; 3]) -> StaticGiSample {
        sample_static_gi_blend(
            &self.effective_layers(),
            &self.settings,
            &self.last_lights,
            p,
            n,
            v,
        )
    }

    /// The volumes a frame blends before ordering: the installed one (priority 0, full weight,
    /// no blend distance) first, then the layers.
    fn effective_layers(&self) -> Vec<BakedGiLayer> {
        let mut all = Vec::with_capacity(1 + self.layers.len());
        if let Some(v) = &self.volume {
            all.push(BakedGiLayer::new(v.clone()));
        }
        all.extend(self.layers.iter().cloned());
        all
    }

    /// Replaces the capsule occluders (kept until the next call); returns how many are used.
    pub(crate) fn set_occluders(&mut self, capsules: &[GiCapsule]) -> usize {
        self.occluders_dropped = occluders::select_occluders(capsules, &mut self.occluders);
        self.occluders.len()
    }

    /// Whether the volume should light a frame whose quality asks `quality_wants`.
    pub(crate) fn wants(&self, quality_wants: bool) -> bool {
        (self.volume.is_some() || self.layers.iter().any(BakedGiLayer::usable))
            && self.status.error.is_none()
            && self.enabled.unwrap_or(quality_wants)
            && crate::GiLook::depth_available()
    }

    /// CPU work and uploads for one main-view frame: the volume buffer when the set of volumes
    /// is new, the uniforms, the screen buffer when the frame outgrew it. `lookup` is the screen
    /// lookup's resolution (half unless a measurement asks otherwise; one whose buffer would pass
    /// the device's storage-binding limit falls back to half). Returns whether a volume lights
    /// this frame (`false`: none fits the device, set [`BakedGiStatus::error`]).
    pub(crate) fn begin(
        &mut self,
        d: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        screen: &ScreenParamsGpu,
        lights: &BakedGiFrameLights,
        lookup: crate::ddgi::GiLookupResolution,
    ) -> bool {
        self.active = false;
        self.status.probe_relative_ao = false;
        self.last_lights = *lights;
        let all = self.effective_layers();
        let (ordered, capped) = order_layers(&all);
        if ordered.is_empty() {
            return false;
        }
        let _span = tracing::info_span!("baked_gi prepare").entered();
        // Volumes in blend order, packed first fit into the storage slots (ADR-2618), each at
        // most one storage binding; a volume that fits no slot is left out (and counted), the
        // rest still blend.
        let limits = d.limits();
        let allowed = limits.max_storage_buffer_binding_size.min(limits.max_buffer_size);
        let mut own = Vec::with_capacity(ordered.len());
        for layer in &ordered {
            let grid_words = self.ensure_grid(&layer.volume);
            own.push(VolumeWords::layout(&layer.volume, grid_words));
        }
        let sizes: Vec<usize> = own.iter().map(|w| w.end).collect();
        let packed = pack_slots(&sizes, allowed / 4);
        let mut used: Vec<&BakedGiLayer> = Vec::with_capacity(ordered.len());
        let mut words: Vec<VolumeWords> = Vec::with_capacity(ordered.len());
        let mut slot_ends = [0usize; BAKED_GI_VOLUME_SLOTS];
        let mut too_large = 0usize;
        for ((&layer, w), at) in ordered.iter().zip(&own).zip(&packed) {
            let Some((slot, first)) = *at else {
                too_large += 1;
                continue;
            };
            let w = w.shifted(first).in_slot(slot);
            slot_ends[slot] = slot_ends[slot].max(w.end);
            used.push(layer);
            words.push(w);
        }
        let slots_used = slot_ends.iter().rposition(|e| *e > 0).map_or(1, |s| s + 1);
        self.status.volumes_dropped = capped + too_large;
        if used.is_empty() {
            let grid_words = self.ensure_grid(&ordered[0].volume);
            let bytes = VolumeWords::layout(&ordered[0].volume, grid_words).end as u64 * 4;
            let message = format!(
                "baked GI volume needs {bytes} bytes in one storage binding; this device allows {allowed}"
            );
            if self.status.error.as_deref() != Some(message.as_str()) {
                tracing::warn!(target: "regia_render", "{message}; the frame keeps its other GI");
            }
            self.status.error = Some(message);
            self.status.volumes = 0;
            self.gpu = None;
            return false;
        }
        if too_large > 0 && self.gpu.as_ref().is_none_or(|g| !same_volumes(&g.sources, &used)) {
            tracing::warn!(
                target: "regia_render",
                dropped = too_large,
                "baked GI volumes past this device's storage-binding size ({allowed} bytes) are left out"
            );
        }
        if self.gpu.as_ref().is_none_or(|g| !same_volumes(&g.sources, &used)) {
            let mut buffers = Vec::with_capacity(slots_used);
            for (slot, &slot_end) in slot_ends.iter().enumerate().take(slots_used) {
                let bytes = (slot_end as u64 * 4).max(16);
                let buffer = d.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(if slot == 0 {
                        "baked gi volume"
                    } else {
                        "baked gi volume slot"
                    }),
                    size: bytes,
                    usage: wgpu::BufferUsages::STORAGE,
                    mapped_at_creation: true,
                });
                match buffer.slice(..).get_mapped_range_mut() {
                    Ok(mut view) => {
                        for (layer, w) in used.iter().zip(&words).filter(|(_, w)| w.slot == slot) {
                            let grid = self.take_grid_upload(&layer.volume);
                            write_volume_grid(&layer.volume, w, &grid, |at, bytes| {
                                view.slice(at..at + bytes.len()).copy_from_slice(bytes);
                            });
                        }
                    }
                    Err(err) => {
                        let message = format!("baked GI volume upload failed: {err}");
                        tracing::warn!(target: "regia_render", "{message}; the frame keeps its other GI");
                        self.status.error = Some(message);
                        self.gpu = None;
                        return false;
                    }
                }
                buffer.unmap();
                buffers.push(buffer);
            }
            tracing::info!(
                target: "regia_render",
                volumes = used.len(),
                slots = slots_used,
                probes = used.iter().map(|l| l.volume.probe_count()).sum::<usize>(),
                bytes = buffers.iter().map(wgpu::Buffer::size).sum::<u64>(),
                "baked GI volume uploaded"
            );
            self.gpu = Some(VolumeGpu {
                sources: used.iter().map(|l| l.volume.clone()).collect(),
                buffers,
                words,
            });
            if let Some(s) = self.screen.as_mut() {
                s.core_bg = None;
            }
            self.generation = self.generation.wrapping_add(1);
        }
        let screen_gpu = self.screen.get_or_insert_with(|| ScreenGpu::new(d));
        let Some(vol_gpu) = self.gpu.as_ref() else {
            return false;
        };
        let binding_max = allowed;
        let (lookup_w, lookup_h, step) =
            screen_lookup_for_binding((screen.size[0], screen.size[1]), lookup, binding_max);
        let half = (lookup_w, lookup_h);
        let texels = u64::from(half.0) * u64::from(half.1);
        if self.settings.brick_visit_probe {
            screen_gpu
                .visits
                .get_or_insert_with(|| VisitReadback::new(d))
                .advance();
        } else {
            screen_gpu.visits = None;
        }
        if screen_gpu.data.as_ref().is_none_or(|(_, cap)| *cap < texels) {
            // Grows only: a smaller frame reuses the larger buffer. The occluder words are reserved
            // when they fit the binding. The shader writes the 11 lookup and split words on every
            // dispatched texel, so the buffer is never shorter than that write.
            let cap = texels.max(screen_gpu.data.as_ref().map_or(0, |(_, c)| *c));
            let per = (SCREEN_WORDS + SPLIT_WORDS) * 4;
            let write = cap.saturating_mul(per).max(16);
            let with_occluder = cap.saturating_mul(per + OCCLUDER_WORDS * 4).max(16);
            let size = if with_occluder <= binding_max {
                with_occluder
            } else {
                write
            };
            screen_gpu.data = Some((
                d.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("baked gi screen lookup"),
                    size,
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                }),
                cap,
            ));
            screen_gpu.core_bg = None;
            self.generation = self.generation.wrapping_add(1);
        }
        self.half = half;
        // The status and the sun warning follow the first volume (the installed one when set).
        let volume = &used[0].volume;
        let scales = component_scales(volume, &self.settings, lights);
        let mismatch = sun_mismatch_deg(volume, lights);
        let key = Arc::as_ptr(volume) as usize;
        if mismatch > SUN_MISMATCH_DEG && self.warned != Some(key) {
            self.warned = Some(key);
            tracing::warn!(
                target: "regia_render",
                mismatch_deg = mismatch,
                "the frame's sun is {mismatch:.1}° from the baked GI volume's: its bounce light is for the baked sun"
            );
        }
        let relight = sun_relight(volume, &self.settings, lights);
        let uniform = set_uniform(&used, &vol_gpu.words, &self.settings, lights);
        let fill_scaled = used.iter().any(|l| {
            component_scales(&l.volume, &self.settings, lights)[GiComponent::Fill as usize]
                .iter()
                .any(|v| *v > 0.0)
        });
        self.status.volumes = used.len();
        let fades = blend::split_fades(&used);
        self.status.volume_list = used
            .iter()
            .zip(&vol_gpu.words)
            .zip(&fades)
            .map(|((l, w), f)| BakedGiVolumeEntry {
                probes: l.volume.probe_count(),
                bytes: (w.end - w.meta) as u64 * 4,
                slot: w.slot,
                priority: l.priority,
                weight: l.clamped_weight(),
                split: l.volume.split.as_ref().map(|s| (s.index, s.count, s.group)),
                joined: f.map_or([false; 4], |f| f.joined),
            })
            .collect();
        self.status.volume_slots = vol_gpu.buffers.len();
        let fill_in = self.settings.replace_live_fill && fill_scaled;
        let emitters_in = lights_emitters(&self.settings);
        // A lookup at the very limit of a storage binding keeps its texels and goes without the words
        // (a receiver then takes no capsule); the screen shader writes them only under this bit.
        let buffer_bytes = screen_gpu.data.as_ref().map_or(0, |(b, _)| b.size());
        let occluder_words =
            !self.occluders.is_empty() && occluder_words_fit(texels, buffer_bytes, binding_max);
        let shade = DdgiUniformGpu::screen_only(
            [0, half.0, half.1, step],
            1 | if fill_in { CTRL_BAKED_FILL } else { 0 }
                | if emitters_in { CTRL_BAKED_EMITTERS } else { 0 }
                | if occluder_words {
                    CTRL_BAKED_OCCLUDER_VIS
                } else {
                    0
                }
                | self.settings.specular.ctrl_bits(),
        );
        crate::staging::defer_if_changed(
            belt,
            d,
            &mut screen_gpu.bgi_bytes,
            &screen_gpu.bgi_uniform,
            bytemuck::bytes_of(&uniform),
        );
        crate::staging::defer_if_changed(
            belt,
            d,
            &mut screen_gpu.shade_bytes,
            &screen_gpu.shade_uniform,
            bytemuck::bytes_of(&shade),
        );
        // eye.w: the SSAO radius the capsules leave their kernel share to (0: none).
        let split_on = self.settings.probe_relative_ao && self.ssao_radius > 0.0;
        let mut params = *screen;
        params.eye[3] = if split_on { self.ssao_radius } else { 0.0 };
        crate::staging::defer_if_changed(
            belt,
            d,
            &mut screen_gpu.params_bytes,
            &screen_gpu.params,
            bytemuck::bytes_of(&params),
        );
        if let Some(bytes) = occluders::stage_occluders(&mut screen_gpu.occ_bytes, &self.occluders) {
            belt.defer(d, &screen_gpu.occ_uniform, 0, &bytes);
        }
        if screen_gpu.core_bg.is_none()
            && let Some((data, _)) = screen_gpu.data.as_ref()
        {
            screen_gpu.core_bg = Some(
                d.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("baked gi core"),
                    layout: &screen_gpu.core_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: screen_gpu.shade_uniform.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: data.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: screen_gpu.bgi_uniform.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: vol_gpu.buffers[0].as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: screen_gpu.occ_uniform.as_entire_binding(),
                        },
                        // Unused slots bind slot 0 again (read-only aliasing is allowed).
                        wgpu::BindGroupEntry {
                            binding: 5,
                            resource: vol_gpu
                                .buffers
                                .get(1)
                                .unwrap_or(&vol_gpu.buffers[0])
                                .as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 6,
                            resource: vol_gpu
                                .buffers
                                .get(2)
                                .unwrap_or(&vol_gpu.buffers[0])
                                .as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 7,
                            resource: vol_gpu
                                .buffers
                                .get(3)
                                .unwrap_or(&vol_gpu.buffers[0])
                                .as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 8,
                            resource: screen_gpu.visit_hist.as_entire_binding(),
                        },
                    ],
                }),
            );
        }
        self.status.scales = scales;
        self.status.sun_mismatch_deg = mismatch;
        self.status.sun_relight = relight;
        self.status.probe_relative_ao = split_on;
        self.active = true;
        true
    }

    /// The probe-relative AO split the SSAO kernel reads this frame (ADR-1104): `None` unless
    /// the volume lights the frame, [`BakedGiSettings::probe_relative_ao`] is on and the SSAO
    /// runs (`ssao_radius` > 0). The split words are written by [`Self::record_screen`], which
    /// the frame records before the SSAO dispatch.
    pub(crate) fn ao_split(&self) -> Option<crate::ssao::SsaoProbeSplit> {
        if !self.active || !self.status.probe_relative_ao {
            return None;
        }
        let (data, _) = self.screen.as_ref()?.data.as_ref()?;
        let first = u64::from(self.half.0) * u64::from(self.half.1) * SCREEN_WORDS;
        Some(crate::ssao::SsaoProbeSplit {
            buffer: data.clone(),
            first_word: u32::try_from(first).ok()?,
            size: self.half,
        })
    }

    /// The uniform and data buffers the shading reads while the volume lights the frame.
    pub(crate) fn shading_buffers(&self) -> Option<(wgpu::Buffer, wgpu::Buffer)> {
        if !self.active {
            return None;
        }
        let s = self.screen.as_ref()?;
        let (data, _) = s.data.as_ref()?;
        Some((s.shade_uniform.clone(), data.clone()))
    }

    /// Records the screen lookup over `depth` (after the depth prepass).
    pub(crate) fn record_screen(
        &mut self,
        d: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        depth: &wgpu::TextureView,
    ) {
        if !self.active {
            return;
        }
        let half = self.half;
        let Some(s) = self.screen.as_mut() else {
            return;
        };
        let Some(core) = s.core_bg.clone() else {
            return;
        };
        let _span = tracing::info_span!("baked_gi screen").entered();
        let params = s.params.clone();
        let layout = s.screen_bgl.clone();
        let bg = s.screen_bgs.get_or_insert_with(depth.clone(), || {
            d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("baked gi screen"),
                layout: &layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: params.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(depth),
                    },
                ],
            })
        });
        // The brick-loop probe captures this frame when its read-back is free.
        let capture = s
            .visits
            .as_ref()
            .is_some_and(|v| v.state.load(std::sync::atomic::Ordering::Acquire) == VISIT_IDLE);
        if capture {
            encoder.clear_buffer(&s.visit_hist, 0, None);
        }
        {
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("baked gi screen lookup"),
                timestamp_writes: None,
            });
            pass.counted_set_pipeline(&s.pipeline);
            pass.counted_set_bind_group(0, &core, &[]);
            pass.counted_set_bind_group(1, &bg, &[]);
            pass.counted_dispatch(half.0.div_ceil(8), half.1.div_ceil(8), 1);
        }
        if capture && let Some(v) = s.visits.as_ref() {
            encoder.copy_buffer_to_buffer(&s.visit_hist, 0, &v.staging, 0, (BRICK_VISIT_BINS * 4) as u64);
            v.state.store(VISIT_COPIED, std::sync::atomic::Ordering::Release);
        }
    }

    /// Ends a frame the volume did not light.
    pub(crate) fn deactivate(&mut self) {
        self.active = false;
        self.status.probe_relative_ao = false;
    }

    /// The status, refreshed.
    pub(crate) fn status(&self) -> BakedGiStatus {
        let mut s = self.status.clone();
        s.installed = self.volume.is_some() || self.layers.iter().any(BakedGiLayer::usable);
        s.active = self.active;
        s.probes = self.volume.as_ref().map_or(0, |v| v.probe_count())
            + self.layers.iter().map(|l| l.volume.probe_count()).sum::<usize>();
        if !self.active {
            s.volumes = 0;
            s.volume_list.clear();
            s.volume_slots = 0;
        }
        s.gpu_bytes = self
            .gpu
            .as_ref()
            .map_or(0, |g| g.buffers.iter().map(wgpu::Buffer::size).sum());
        s.brick_visits = self
            .screen
            .as_ref()
            .and_then(|g| g.visits.as_ref())
            .and_then(|v| v.last.clone());
        s.occluders = self.occluders.len();
        s.occluders_dropped = self.occluders_dropped;
        s.interior_bricks = self.volume.as_ref().map_or(0, |v| v.interior_brick_count());
        s.wall_probes = self.volume.as_ref().map_or(0, |v| v.wall_probe_count());
        s.emitters = self
            .volume
            .as_ref()
            .and_then(|v| v.emitters.as_ref())
            .map(Vec::len);
        s.brick_grid_cells = 0;
        s.brick_grid_entries = 0;
        if let Some(volume) = &self.volume
            && let Some(memo) = self
                .grid_memo
                .iter()
                .find(|memo| memo.id == volume_grid_id(volume))
        {
            s.brick_grid_cells = memo.cells;
            s.brick_grid_entries = memo.entries;
        }
        s
    }

    /// A copy for a stand-in renderer: the volume, the layers, its settings and the occluders
    /// (GPU state is per renderer).
    pub(crate) fn share(&self) -> Self {
        Self {
            volume: self.volume.clone(),
            layers: self.layers.clone(),
            settings: self.settings,
            enabled: self.enabled,
            occluders: self.occluders.clone(),
            occluders_dropped: self.occluders_dropped,
            grid_memo: self.grid_memo.clone(),
            ..Self::default()
        }
    }

    /// Reads the last frame's screen lookup back (tests and debugging; blocks): lookup width,
    /// height and the texels, row-major. `None` before the volume lit a frame.
    pub(crate) fn read_back(
        &self,
        device: &crate::RenderDevice,
    ) -> Result<Option<(u32, u32, Vec<DdgiScreenTexel>)>, crate::RenderError> {
        let Some((data, _)) = self.screen.as_ref().and_then(|s| s.data.as_ref()) else {
            return Ok(None);
        };
        let (w, h) = self.half;
        let size = u64::from(w) * u64::from(h) * SCREEN_WORDS * 4;
        let d = &device.device;
        let staging = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("baked gi readback"),
            size: size.max(16),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("baked gi readback"),
        });
        encoder.copy_buffer_to_buffer(data, 0, &staging, 0, size);
        device.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device
            .tracked()
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(crate::RenderError::Poll)?;
        rx.recv()
            .map_err(|_| crate::RenderError::Readback("map callback dropped".into()))?
            .map_err(|e| crate::RenderError::Readback(e.to_string()))?;
        let words: Vec<u32> = bytemuck::pod_collect_to_vec(
            &staging
                .slice(..)
                .get_mapped_range()
                .map_err(|e| crate::RenderError::Readback(e.to_string()))?,
        );
        staging.unmap();
        let texels = words
            .as_chunks::<{ SCREEN_WORDS as usize }>()
            .0
            .iter()
            .take((w * h) as usize)
            .map(|t| DdgiScreenTexel {
                e: std::array::from_fn(|j| crate::ddgi::unpack_rgb9e5(t[j])),
                coverage: f32::from_bits(t[7]),
            })
            .collect();
        Ok(Some((w, h, texels)))
    }
}

/// Copies `size` bytes of `buffer` from byte `offset` back to the CPU as words (tests and
/// debugging; blocks and submits).
fn read_words(
    device: &crate::RenderDevice,
    buffer: &wgpu::Buffer,
    offset: u64,
    size: u64,
) -> Result<Vec<u32>, crate::RenderError> {
    let d = &device.device;
    let staging = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("baked gi split readback"),
        size: size.max(16),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("baked gi split readback"),
    });
    encoder.copy_buffer_to_buffer(buffer, offset, &staging, 0, size);
    device.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .tracked()
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(crate::RenderError::Poll)?;
    rx.recv()
        .map_err(|_| crate::RenderError::Readback("map callback dropped".into()))?
        .map_err(|e| crate::RenderError::Readback(e.to_string()))?;
    let words: Vec<u32> = bytemuck::pod_collect_to_vec(
        &staging
            .slice(..)
            .get_mapped_range()
            .map_err(|e| crate::RenderError::Readback(e.to_string()))?,
    );
    staging.unmap();
    Ok(words)
}

impl BakedGiRuntime {
    /// Reads the last frame's probe-relative AO split back (ADR-1104; tests and debugging,
    /// blocks): lookup width, height and the split per texel, row-major. `None` before the
    /// volume lit a frame.
    pub(crate) fn read_back_split(
        &self,
        device: &crate::RenderDevice,
    ) -> Result<Option<(u32, u32, Vec<ProbeAoSplit>)>, crate::RenderError> {
        let Some((data, _)) = self.screen.as_ref().and_then(|s| s.data.as_ref()) else {
            return Ok(None);
        };
        let (w, h) = self.half;
        let texels = u64::from(w) * u64::from(h);
        let words = read_words(device, data, texels * SCREEN_WORDS * 4, texels * SPLIT_WORDS * 4)?;
        let split = words
            .as_chunks::<{ SPLIT_WORDS as usize }>()
            .0
            .iter()
            .take(texels as usize)
            .map(|t| ProbeAoSplit::decode(*t))
            .collect();
        Ok(Some((w, h, split)))
    }
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![
        crate::contract_row!(
            "baked_gi_screen.wgsl",
            "BakedGiSet",
            BakedGiSetGpu {
                count,
                volumes,
                layers
            }
        ),
        crate::ContractRow::new(
            "baked_gi_points.wgsl",
            "BakedGiUniform",
            size_of::<BakedGiUniformGpu>(),
            vec![
                ("terrain", std::mem::offset_of!(BakedGiUniformGpu, terrain)),
                ("counts", std::mem::offset_of!(BakedGiUniformGpu, counts)),
                ("levels_a", std::mem::offset_of!(BakedGiUniformGpu, levels)),
                ("levels_b", std::mem::offset_of!(BakedGiUniformGpu, levels) + 16),
                ("words", std::mem::offset_of!(BakedGiUniformGpu, words)),
                ("extra", std::mem::offset_of!(BakedGiUniformGpu, extra)),
                ("params", std::mem::offset_of!(BakedGiUniformGpu, params)),
                ("scale_sun", std::mem::offset_of!(BakedGiUniformGpu, scales)),
                ("scale_sky", std::mem::offset_of!(BakedGiUniformGpu, scales) + 16),
                ("scale_fill", std::mem::offset_of!(BakedGiUniformGpu, scales) + 32),
                (
                    "scale_emissive",
                    std::mem::offset_of!(BakedGiUniformGpu, scales) + 48,
                ),
                ("relight", std::mem::offset_of!(BakedGiUniformGpu, relight)),
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lookup buffer made at the binding limit without the occluder words is reused by a
    /// smaller frame whose words would fit the binding but not that buffer: no occluder words.
    #[test]
    fn occluder_words_need_room_in_the_reused_lookup_buffer() {
        let binding_max = 128u64 << 20;
        let per = (SCREEN_WORDS + SPLIT_WORDS) * 4;
        let with = per + OCCLUDER_WORDS * 4;
        // A frame of `cap` texels whose words do not fit: the buffer holds only the lookup.
        let cap = binding_max / per;
        assert!(cap * with > binding_max);
        let buffer_bytes = cap * per;
        // A smaller frame: its words fit the binding, but they run past the buffer.
        let texels = binding_max / with;
        assert!(texels * with <= binding_max && texels * with > buffer_bytes);
        assert!(!occluder_words_fit(texels, buffer_bytes, binding_max));
        // Small enough for the old buffer, and a buffer made with the words: fine.
        assert!(occluder_words_fit(buffer_bytes / with, buffer_bytes, binding_max));
        assert!(occluder_words_fit(texels, texels * with, binding_max));
        assert!(!occluder_words_fit(u64::MAX, u64::MAX, binding_max));
    }
    use regia_asset::baked_gi::{
        BakeInfo, BakeLights, BrickFlags, DetailBrick, ProbeData, ProbeMeta, SH_Y00, TerrainLayer,
        VolumeLayout,
    };
    use regia_asset::baked_gi_grid::BrickGrid;

    /// Radiance SH of a uniform radiance `l` (no L1).
    fn uniform(l: [f32; 3]) -> ShRgb {
        let k = 4.0 * std::f32::consts::PI * SH_Y00;
        [
            [l[0] * k, 0.0, 0.0, 0.0],
            [l[1] * k, 0.0, 0.0, 0.0],
            [l[2] * k, 0.0, 0.0, 0.0],
        ]
    }

    fn probe(sky: [f32; 3], res: u32, far: f32, flags: u16) -> ProbeData {
        ProbeData {
            sh: [
                uniform([0.0; 3]),
                uniform(sky),
                uniform([0.0; 3]),
                uniform([0.0; 3]),
            ],
            moments: vec![[far, far * far]; (res * res) as usize],
            meta: ProbeMeta {
                offset: [0.0; 3],
                flags,
            },
        }
    }

    fn flat_terrain(sky: impl Fn(usize) -> [f32; 3]) -> BakedGiVolume {
        let t = TerrainLayer {
            origin: [0.0, 0.0],
            spacing: [4.0, 4.0],
            counts: [4, 4],
            levels: vec![1.0, 2.0, 4.0],
            ground: vec![10.0; 16],
        };
        let layout = VolumeLayout {
            terrain: Some(t),
            bricks: Vec::new(),
            moment_resolution: 4,
            max_distance: 16.0,
        };
        let probes: Vec<_> = (0..layout.probe_count())
            .map(|i| probe(sky(i), 4, 16.0, ProbeFlags::VALID))
            .collect();
        BakedGiVolume::from_probes(
            &layout,
            BakeLights {
                sky_scale: 1.0,
                ..BakeLights::default()
            },
            BakeInfo::default(),
            &probes,
        )
        .expect("volume")
    }

    fn scales(sky: f32) -> [[f32; 3]; 4] {
        [[0.0; 3], [sky; 3], [0.0; 3], [0.0; 3]]
    }

    #[test]
    fn a_uniform_sky_reads_its_radiance_inside_and_fades_outside() {
        let vol = flat_terrain(|_| [2.0, 1.0, 0.5]);
        let s = BakedGiSettings::default();
        let up = [0.0, 1.0, 0.0];
        let inside = sample_static_gi(&vol, &s, &scales(1.0), [6.0, 11.5, 6.0], up, up);
        assert!((inside.coverage - 1.0).abs() < 1e-5, "{inside:?}");
        // E/π of a uniform radiance L is L.
        for (got, want) in inside.irradiance_over_pi.iter().zip([2.0, 1.0, 0.5]) {
            assert!((got - want).abs() < 0.02 * want, "{inside:?}");
        }
        assert!((inside.components[1][0] - inside.irradiance_over_pi[0]).abs() < 1e-5);
        // Half a column outside: half coverage, same light.
        let edge = sample_static_gi(&vol, &s, &scales(1.0), [-2.0, 11.5, 6.0], up, up);
        assert!((edge.coverage - 0.5).abs() < 0.05, "{edge:?}");
        assert!((edge.irradiance_over_pi[0] - 2.0).abs() < 0.05);
        // A column spacing outside, or twice the top level above the ground: none.
        assert_eq!(
            sample_static_gi(&vol, &s, &scales(1.0), [-5.0, 11.5, 6.0], up, up).coverage,
            0.0
        );
        assert_eq!(
            sample_static_gi(&vol, &s, &scales(1.0), [6.0, 19.0, 6.0], up, up).coverage,
            0.0
        );
        // The component factor scales the light.
        let half = sample_static_gi(&vol, &s, &scales(0.5), [6.0, 11.5, 6.0], up, up);
        assert!((half.irradiance_over_pi[0] - 1.0).abs() < 0.02);
    }

    #[test]
    fn the_lookup_interpolates_between_levels_and_skips_invalid_probes() {
        // Level 0 (1 m) reads 1, level 1 (2 m) reads 3, level 2 (4 m) reads 3.
        let vol = flat_terrain(|i| if i % 3 == 0 { [1.0; 3] } else { [3.0; 3] });
        let s = BakedGiSettings {
            normal_bias: 0.0,
            view_bias: 0.0,
            ..BakedGiSettings::default()
        };
        let side = [1.0, 0.0, 0.0];
        let mid = sample_static_gi(&vol, &s, &scales(1.0), [6.0, 11.5, 6.0], side, side);
        assert!(
            mid.irradiance_over_pi[0] > 1.3 && mid.irradiance_over_pi[0] < 2.7,
            "{mid:?}"
        );
        let low = sample_static_gi(&vol, &s, &scales(1.0), [6.0, 10.5, 6.0], side, side);
        assert!((low.irradiance_over_pi[0] - 1.0).abs() < 0.05, "{low:?}");
        // Every probe invalid: no light, no coverage (the passes keep their own terms).
        let layout = flat_terrain(|_| [1.0; 3]).layout();
        let probes: Vec<_> = (0..layout.probe_count())
            .map(|_| probe([1.0; 3], 4, 16.0, ProbeFlags::UNDER_TERRAIN))
            .collect();
        let dead = BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
            .expect("volume");
        let r = sample_static_gi(&dead, &s, &scales(1.0), [6.0, 11.5, 6.0], side, side);
        assert_eq!(r.coverage, 0.0);
    }

    #[test]
    fn a_brick_overrides_the_terrain_inside_and_blends_over_its_last_cell() {
        let mut layout = flat_terrain(|_| [1.0; 3]).layout();
        layout.bricks.push(DetailBrick {
            origin: [4.0, 10.0, 4.0],
            spacing: [1.0; 3],
            counts: [5, 5, 5],
            first_probe: 0,
            flags: 0,
        });
        layout.assign_brick_offsets();
        let first = layout.bricks[0].first_probe as usize;
        let probes: Vec<_> = (0..layout.probe_count())
            .map(|i| {
                probe(
                    if i >= first { [5.0; 3] } else { [1.0; 3] },
                    4,
                    16.0,
                    ProbeFlags::VALID,
                )
            })
            .collect();
        let vol = BakedGiVolume::from_probes(
            &layout,
            BakeLights {
                sky_scale: 1.0,
                ..BakeLights::default()
            },
            BakeInfo::default(),
            &probes,
        )
        .expect("volume");
        let s = BakedGiSettings {
            normal_bias: 0.0,
            view_bias: 0.0,
            ..BakedGiSettings::default()
        };
        let up = [0.0, 1.0, 0.0];
        let deep = sample_static_gi(&vol, &s, &scales(1.0), [6.0, 12.0, 6.0], up, up);
        assert!((deep.irradiance_over_pi[0] - 5.0).abs() < 0.1, "{deep:?}");
        // Half a cell inside the brick: half brick, half terrain.
        let edge = sample_static_gi(&vol, &s, &scales(1.0), [4.5, 12.0, 6.0], up, up);
        assert!((edge.irradiance_over_pi[0] - 3.0).abs() < 0.2, "{edge:?}");
        let outside = sample_static_gi(&vol, &s, &scales(1.0), [2.0, 11.5, 2.0], up, up);
        assert!((outside.irradiance_over_pi[0] - 1.0).abs() < 0.05, "{outside:?}");
    }

    #[test]
    fn a_probe_behind_a_wall_is_rejected_by_its_moments() {
        // Column 1's probes see a wall 0.5 m away in every direction: a receiver past it reads
        // the other columns' light only.
        let vol = {
            let base = flat_terrain(|_| [1.0; 3]);
            let layout = base.layout();
            let t = layout.terrain.as_ref().expect("terrain");
            let probes: Vec<_> = (0..layout.probe_count())
                .map(|i| {
                    let (ci, _, _) = t.coords(i);
                    if ci == 1 {
                        ProbeData {
                            moments: vec![[0.5, 0.25]; 16],
                            ..probe([9.0; 3], 4, 16.0, ProbeFlags::VALID)
                        }
                    } else {
                        probe([1.0; 3], 4, 16.0, ProbeFlags::VALID)
                    }
                })
                .collect();
            BakedGiVolume::from_probes(
                &layout,
                BakeLights {
                    sky_scale: 1.0,
                    ..BakeLights::default()
                },
                BakeInfo::default(),
                &probes,
            )
            .expect("volume")
        };
        let s = BakedGiSettings::default();
        let up = [0.0, 1.0, 0.0];
        let r = sample_static_gi(&vol, &s, &scales(1.0), [6.0, 11.5, 6.0], up, up);
        assert!(r.irradiance_over_pi[0] < 1.5, "the hidden probes leaked: {r:?}");
    }

    /// `flat_terrain`'s layer with a 5 × 5 × 5 brick at 1 m from (4, 10, 4); `brick` and
    /// `terrain` make each layer's probes.
    fn brick_over_terrain(brick: impl Fn() -> ProbeData, terrain: impl Fn() -> ProbeData) -> BakedGiVolume {
        let mut layout = flat_terrain(|_| [1.0; 3]).layout();
        layout.bricks.push(DetailBrick {
            origin: [4.0, 10.0, 4.0],
            spacing: [1.0; 3],
            counts: [5, 5, 5],
            first_probe: 0,
            flags: 0,
        });
        layout.assign_brick_offsets();
        let first = layout.bricks[0].first_probe as usize;
        let probes: Vec<_> = (0..layout.probe_count())
            .map(|i| if i >= first { brick() } else { terrain() })
            .collect();
        BakedGiVolume::from_probes(
            &layout,
            BakeLights {
                sky_scale: 1.0,
                ..BakeLights::default()
            },
            BakeInfo::default(),
            &probes,
        )
        .expect("volume")
    }

    /// A probe that sees a surface 10 cm away in every direction: blind to any receiver past it.
    fn walled_in(sky: [f32; 3]) -> ProbeData {
        ProbeData {
            moments: vec![[0.1, 0.01]; 16],
            ..probe(sky, 4, 16.0, ProbeFlags::VALID)
        }
    }

    #[test]
    fn a_blind_brick_yields_to_the_terrain_that_sees_the_point() {
        let s = BakedGiSettings {
            normal_bias: 0.0,
            view_bias: 0.0,
            ..BakedGiSettings::default()
        };
        let up = [0.0, 1.0, 0.0];
        // Half a cell inside the brick's −x face (every brick corner ≥ 0.5 m away): the brick and
        // the terrain layer blend half and half. The brick's probes are walled in, the terrain's
        // see the point: the terrain's light, not the average (3.0) the plain blend gave.
        let vol = brick_over_terrain(
            || walled_in([5.0; 3]),
            || probe([1.0; 3], 4, 16.0, ProbeFlags::VALID),
        );
        let edge = sample_static_gi(&vol, &s, &scales(1.0), [4.5, 12.0, 6.0], up, up);
        assert!(
            edge.irradiance_over_pi[0] > 0.95 && edge.irradiance_over_pi[0] < 1.15,
            "{edge:?}"
        );
        // The coverage is the blend's, unchanged: the passes' analytic terms do not come back.
        assert!((edge.coverage - 1.0).abs() < 1e-5, "{edge:?}");
        // Half the interpolation weight (the terrain's half) sees the point.
        assert!((edge.visibility - 0.5).abs() < 0.05, "{edge:?}");
        // Deep inside the brick nothing else covers the point: its own light, as before.
        let deep = sample_static_gi(&vol, &s, &scales(1.0), [6.5, 12.0, 6.5], up, up);
        assert!((deep.irradiance_over_pi[0] - 5.0).abs() < 0.1, "{deep:?}");
        assert!(deep.visibility < 0.01, "{deep:?}");

        // The other way round: the walled-in terrain yields to the brick.
        let vol = brick_over_terrain(
            || probe([5.0; 3], 4, 16.0, ProbeFlags::VALID),
            || walled_in([1.0; 3]),
        );
        let edge = sample_static_gi(&vol, &s, &scales(1.0), [4.5, 12.0, 6.0], up, up);
        assert!(
            edge.irradiance_over_pi[0] > 4.8 && edge.irradiance_over_pi[0] < 5.05,
            "{edge:?}"
        );

        // Both blind: the plain blend.
        let vol = brick_over_terrain(|| walled_in([5.0; 3]), || walled_in([1.0; 3]));
        let edge = sample_static_gi(&vol, &s, &scales(1.0), [4.5, 12.0, 6.0], up, up);
        assert!((edge.irradiance_over_pi[0] - 3.0).abs() < 0.2, "{edge:?}");
        assert!(edge.visibility < 0.01, "{edge:?}");

        // Both see it: the plain blend, and full visibility.
        let vol = brick_over_terrain(
            || probe([5.0; 3], 4, 16.0, ProbeFlags::VALID),
            || probe([1.0; 3], 4, 16.0, ProbeFlags::VALID),
        );
        let edge = sample_static_gi(&vol, &s, &scales(1.0), [4.5, 12.0, 6.0], up, up);
        assert!((edge.irradiance_over_pi[0] - 3.0).abs() < 0.2, "{edge:?}");
        assert!((edge.visibility - 1.0).abs() < 1e-5, "{edge:?}");
    }

    #[test]
    fn oct_texels_past_an_edge_fold_back_across_its_midpoint() {
        // Sides: row y ↔ res − 1 − y; top and bottom: column x ↔ res − 1 − x; corners: the
        // opposite corner.
        for ((x, y), want) in [
            ((-1, 1), (0, 2)),
            ((4, 0), (3, 3)),
            ((1, -1), (2, 0)),
            ((2, 4), (1, 3)),
            ((-1, -1), (3, 3)),
            ((4, 4), (0, 0)),
            ((4, -1), (0, 3)),
            ((-1, 4), (3, 0)),
            ((2, 3), (2, 3)),
        ] {
            assert_eq!(oct_texel(x, y, 4), want, "({x}, {y})");
        }
        assert_eq!(oct_texel(-1, 0, 1), (0, 0));
        assert_eq!(oct_texel(1, 1, 1), (0, 0));
    }

    #[test]
    fn the_moment_filter_is_continuous_across_the_octahedral_folds() {
        for res in [4u32, 8] {
            let texels = (res * res) as usize;
            // A map whose texels all differ (means 1..13 m, under the 16 m clamp).
            let map: Vec<[f32; 2]> = (0..texels)
                .map(|t| {
                    let m = 1.0 + ((t * 7) % 13) as f32;
                    [m, m * m]
                })
                .collect();
            let layout = VolumeLayout {
                moment_resolution: res,
                ..flat_terrain(|_| [1.0; 3]).layout()
            };
            let probes: Vec<_> = (0..layout.probe_count())
                .map(|_| ProbeData {
                    moments: map.clone(),
                    ..probe([1.0; 3], res, 16.0, ProbeFlags::VALID)
                })
                .collect();
            let vol =
                BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
                    .expect("volume");
            // The old filter clamped its taps at the map's edges.
            let clamped = |dir: V3| {
                let uv = regia_asset::baked_gi::oct_encode(dir);
                let rf = res as f32;
                let r = res as usize;
                let fx = (uv[0] * rf - 0.5).clamp(0.0, rf - 1.0);
                let fy = (uv[1] * rf - 0.5).clamp(0.0, rf - 1.0);
                let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
                let (x1, y1) = ((x0 + 1).min(r - 1), (y0 + 1).min(r - 1));
                let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
                let m = |x: usize, y: usize| map[y * r + x][0];
                let top = m(x0, y0) + (m(x1, y0) - m(x0, y0)) * tx;
                let bottom = m(x0, y1) + (m(x1, y1) - m(x0, y1)) * tx;
                top + (bottom - top) * ty
            };
            let e = 1.0e-5;
            let (mut worst, mut worst_clamped) = (0.0f32, 0.0f32);
            let mut pairs = vec![([e, e, -1.0], [-e, -e, -1.0]), ([e, -e, -1.0], [-e, e, -1.0])];
            for a in (-9..=9).map(|k| k as f32 * 0.1) {
                for b in [0.3, 0.6, 1.0, 2.0] {
                    // Across y = 0 (a side edge) and across x = 0 (the top or bottom edge), both
                    // in the −z half, where the octahedral map folds.
                    pairs.push(([a, e, -b], [a, -e, -b]));
                    pairs.push(([e, a, -b], [-e, a, -b]));
                }
            }
            // The bilinear mean (metres) of the taps the lookup reads (`moment_taps`).
            let mean = |d: V3| {
                let (t, f) = moment_taps(&vol, 0, d);
                let l = |a: f32, b: f32, s: f32| a + (b - a) * s;
                l(l(t[0][0], t[1][0], f[0]), l(t[2][0], t[3][0], f[0]), f[1]) * vol.max_distance
            };
            for (p, q) in pairs {
                let (p, q) = (normalize(p), normalize(q));
                worst = worst.max((mean(p) - mean(q)).abs());
                worst_clamped = worst_clamped.max((clamped(p) - clamped(q)).abs());
            }
            assert!(
                worst < 0.05,
                "res {res}: the filter jumps {worst} m across a fold"
            );
            // The test can tell: the clamped taps jumped by metres.
            assert!(worst_clamped > 1.0, "res {res}: {worst_clamped}");
        }
    }

    #[test]
    fn scales_follow_the_frame_lights() {
        let mut vol = flat_terrain(|_| [1.0; 3]);
        vol.lights = BakeLights {
            sun_illuminance: 100_000.0,
            sun_color: [1.0, 0.9, 0.8],
            sun_direction: [0.0, 1.0, 0.0],
            sky_scale: 1.0,
            fill_lux: 2.0,
            emissive_scale: 1.0,
        };
        let s = BakedGiSettings::default();
        let lights = BakedGiFrameLights {
            sun: Some([50_000.0, 45_000.0, 40_000.0]),
            to_sun: Some([0.0, 0.0, 1.0]),
            fill: Some([1.0; 3]),
        };
        let k = component_scales(&vol, &s, &lights);
        for c in 0..3 {
            assert!((k[0][c] - 0.5).abs() < 1e-5, "{k:?}");
        }
        assert!((k[2][0] - 0.5).abs() < 1e-5);
        assert!((sun_mismatch_deg(&vol, &lights) - 90.0).abs() < 1e-3);
        let off = component_scales(
            &vol,
            &BakedGiSettings {
                replace_live_fill: false,
                ..s
            },
            &lights,
        );
        assert_eq!(off[2], [0.0; 3]);
        let dark = component_scales(&vol, &s, &BakedGiFrameLights::default());
        assert_eq!(dark[0], [0.0; 3]);
        assert_eq!(dark[2], [0.0; 3]);
    }

    #[test]
    fn the_volume_buffer_holds_the_sections_unchanged() {
        let vol = flat_terrain(|_| [1.0; 3]);
        let words = VolumeWords::of(&vol);
        let mut bytes = vec![0u8; words.end * 4];
        write_volume(&vol, &words, |at, b| bytes[at..at + b.len()].copy_from_slice(b));
        assert_eq!(&bytes[..vol.meta_bytes().len()], vol.meta_bytes());
        assert_eq!(
            &bytes[words.sh * 4..words.sh * 4 + vol.sh_bytes().len()],
            vol.sh_bytes()
        );
        assert_eq!(words.sh - words.meta, vol.probe_count() * 2);
        assert_eq!(words.moments - words.sh, vol.probe_count() * SH_WORDS);
        assert_eq!(
            f32::from_le_bytes(
                bytes[words.ground * 4..words.ground * 4 + 4]
                    .try_into()
                    .expect("4")
            ),
            10.0
        );
    }

    #[test]
    fn the_policy_drops_the_probes_and_ssgi_and_halves_gtao() {
        let ultra = crate::FrameQuality::from_profile(&crate::RenderProfile::from_preset(
            crate::RenderProfilePreset::DesktopUltra,
        ));
        let q = policy(ultra);
        assert!(!q.gi_probes && !q.screen_space_gi);
        if ultra.ssao == crate::SsaoMode::GtaoFull {
            assert_eq!(q.ssao, crate::SsaoMode::GtaoHalfRes);
        }
        assert_eq!(
            policy(crate::FrameQuality {
                ssao: crate::SsaoMode::HalfRes,
                ..ultra
            })
            .ssao,
            crate::SsaoMode::HalfRes
        );
        // Shadows and the rest are untouched.
        assert_eq!(q.paged_sun_shadows, ultra.paged_sun_shadows);
        assert_eq!(q.csm_cascade_count, ultra.csm_cascade_count);
        assert_eq!(q.contact_shadows, ultra.contact_shadows);
        assert_eq!(q.ssr, ultra.ssr);
        assert!(ultra.baked_gi, "presets use an installed bake");
        const { assert!(!crate::FrameQuality::LAB.baked_gi, "the lab baseline does not") };
    }

    #[test]
    fn a_stand_in_shares_the_volume_and_hands_back_one_set_meanwhile() {
        // `GameFrameWarm`: the stand-in gets what was installed; what the host installs on the
        // stand-in while the renderer is away comes back with it (`absorb_stand_in`).
        let vol = Arc::new(flat_terrain(|_| [1.0; 3]));
        let mut runtime = crate::FrameRenderer::new();
        runtime.set_baked_gi(Some(vol.clone()));
        runtime.set_baked_gi_settings(BakedGiSettings {
            intensity: 2.0,
            ..BakedGiSettings::default()
        });
        let mut stand_in = crate::FrameRenderer::new();
        runtime.share_installs_with(&mut stand_in);
        assert!(stand_in.baked_gi().is_some_and(|v| Arc::ptr_eq(v, &vol)));
        assert_eq!(stand_in.baked_gi_settings().intensity, 2.0);
        let other = Arc::new(flat_terrain(|_| [2.0; 3]));
        stand_in.set_baked_gi(Some(other.clone()));
        let mut returning = crate::FrameRenderer::new();
        returning.set_baked_gi(Some(vol.clone()));
        returning.absorb_stand_in(&mut stand_in);
        assert!(returning.baked_gi().is_some_and(|v| Arc::ptr_eq(v, &other)));
        // A stand-in that installed nothing leaves the returning renderer's volume.
        let mut untouched = crate::FrameRenderer::new();
        returning.absorb_stand_in(&mut untouched);
        assert!(returning.baked_gi().is_some_and(|v| Arc::ptr_eq(v, &other)));
        assert_eq!(returning.baked_gi_status().probes, other.probe_count());
    }

    /// The CPU mirror without flags (as before `.fgiv` v2, v0.1.26, with the ADR-506
    /// visibility-weighted blend and the ADR-861 evaluation that apply to every file): brick
    /// selection and blend with no interior bricks and no wall codes. Kept here so the v1
    /// compatibility test compares against it bit for bit.
    mod v1_reference {
        use super::super::*;

        fn brick_part(
            volume: &BakedGiVolume,
            settings: &BakedGiSettings,
            light: &Light,
            p: V3,
            n: V3,
            v: V3,
        ) -> Part {
            let mut out = Part::empty();
            let mut best: Option<(usize, f32)> = None;
            for (b, brick) in volume.bricks.iter().enumerate() {
                let mut inset = f32::INFINITY;
                for (a, pa) in p.iter().enumerate() {
                    let g = (pa - brick.origin[a]) / brick.spacing[a];
                    inset = inset.min(g).min(brick.counts[a] as f32 - 1.0 - g);
                }
                if inset > best.map_or(0.0, |(_, i)| i) {
                    best = Some((b, inset));
                }
            }
            let Some((b, inset)) = best else {
                return out;
            };
            let brick = &volume.bricks[b];
            let sp = brick.spacing;
            let s = sp[0].min(sp[1]).min(sp[2]);
            let biased = add(
                p,
                mul(add(mul(n, settings.normal_bias), mul(v, settings.view_bias)), s),
            );
            let mut i0 = [0usize; 3];
            let mut i1 = [0usize; 3];
            let mut f = [0.0f32; 3];
            for a in 0..3 {
                let c_max = brick.counts[a] as f32 - 1.0;
                let g = ((biased[a] - brick.origin[a]) / sp[a]).clamp(0.0, c_max);
                i0[a] = g.floor().min((c_max - 1.0).max(0.0)) as usize;
                i1[a] = (i0[a] + 1).min(brick.counts[a] as usize - 1);
                f[a] = g - i0[a] as f32;
            }
            for k in 0..8u32 {
                let o = [k & 1 == 1, (k >> 1) & 1 == 1, (k >> 2) & 1 == 1];
                let cell: [usize; 3] = std::array::from_fn(|a| if o[a] { i1[a] } else { i0[a] });
                let wa: [f32; 3] = std::array::from_fn(|a| if o[a] { f[a] } else { 1.0 - f[a] });
                let index = brick.index(cell[0] as u32, cell[1] as u32, cell[2] as u32);
                let Some(offset) = valid(volume, index) else {
                    continue;
                };
                let nominal = brick.nominal_position(cell[0] as u32, cell[1] as u32, cell[2] as u32);
                let tri = wa[0] * wa[1] * wa[2];
                let w = corner(
                    volume,
                    settings,
                    index,
                    add(nominal, offset),
                    p,
                    biased,
                    n,
                    s,
                    tri,
                );
                out.add_corner(volume, index, w, tri, light);
            }
            out.cov = inset.clamp(0.0, 1.0);
            out
        }

        pub(super) fn sample(
            volume: &BakedGiVolume,
            settings: &BakedGiSettings,
            scales: &[[f32; 3]; GI_COMPONENTS],
            p: [f32; 3],
            n: [f32; 3],
            v: [f32; 3],
        ) -> StaticGiSample {
            let light = Light {
                scales,
                ground: SunRelight::NONE.ground_factor(),
            };
            let br = brick_part(volume, settings, &light, p, n, v);
            let mut sh = [[[0.0f32; 4]; 3]; GI_COMPONENTS];
            let mut accumulate = |part: &Part, k: f32| {
                for (dst, src) in sh.iter_mut().zip(&part.sh) {
                    for ch in 0..3 {
                        for m in 0..4 {
                            dst[ch][m] += src[ch][m] * k;
                        }
                    }
                }
            };
            let (mut wb, mut vb) = (0.0, 0.0);
            if br.wsum > 0.0 && br.cov > 0.0 {
                wb = br.cov;
                vb = br.visibility();
            }
            let mut cov = wb;
            let (mut wt, mut vt) = (0.0, 0.0);
            let mut tr = Part::empty();
            if wb < 1.0 {
                tr = terrain_part(volume, settings, &light, p, n, v);
                if tr.wsum > 0.0 && tr.cov > 0.0 {
                    wt = (1.0 - wb) * tr.cov;
                    vt = tr.visibility();
                    cov += wt;
                }
            }
            if cov <= 1e-5 {
                return StaticGiSample::default();
            }
            let kb = wb * (vb + SEEN_FLOOR);
            let kt = wt * (vt + SEEN_FLOOR);
            if kb > 0.0 {
                accumulate(&br, kb / br.wsum);
            }
            if kt > 0.0 {
                accumulate(&tr, kt / tr.wsum);
            }
            let total_k = kb + kt;
            if total_k <= 0.0 {
                return StaticGiSample {
                    coverage: cov,
                    ..StaticGiSample::default()
                };
            }
            let visibility = (wb * vb + wt * vt) / (wb + wt);
            let k = settings.intensity / total_k;
            let mut total = [[0.0f32; 4]; 3];
            let mut components = [[0.0f32; 3]; GI_COMPONENTS];
            for (c, comp) in sh.iter().enumerate() {
                let mut scaled = *comp;
                for ch in 0..3 {
                    for m in 0..4 {
                        scaled[ch][m] *= k;
                        total[ch][m] += scaled[ch][m];
                    }
                }
                components[c] = GiEval::of(settings).irradiance(&scaled, n);
            }
            StaticGiSample {
                irradiance_over_pi: GiEval::of(settings).irradiance(&total, n),
                components,
                coverage: cov,
                visibility,
                ..StaticGiSample::default()
            }
        }
    }

    fn sample_bits(s: &StaticGiSample) -> Vec<u32> {
        s.irradiance_over_pi
            .iter()
            .chain(s.components.iter().flatten())
            .chain([&s.coverage, &s.visibility])
            .map(|v| v.to_bits())
            .collect()
    }

    /// Terrain lit `outdoor`, plus a 5 × 5 × 5 brick (1 m) at (4, 10, 4)..(8, 14, 8) lit
    /// `inside`, its probes valid unless `brick_valid` is false; `interior` flags it through
    /// the bake helper with a hand-written box around it.
    fn room(outdoor: f32, inside: f32, brick_valid: bool, interior: bool) -> BakedGiVolume {
        let mut layout = flat_terrain(|_| [1.0; 3]).layout();
        layout.bricks.push(DetailBrick {
            origin: [4.0, 10.0, 4.0],
            spacing: [1.0; 3],
            counts: [5, 5, 5],
            first_probe: 0,
            flags: 0,
        });
        layout.assign_brick_offsets();
        if interior {
            assert_eq!(
                layout.flag_interior_bricks(&[([3.9, 9.9, 3.9], [8.1, 14.1, 8.1])]),
                1
            );
        }
        let first = layout.bricks[0].first_probe as usize;
        let probes: Vec<_> = (0..layout.probe_count())
            .map(|i| {
                if i >= first {
                    let flags = if brick_valid {
                        ProbeFlags::VALID
                    } else {
                        ProbeFlags::INSIDE_GEOMETRY
                    };
                    probe([inside; 3], 4, 16.0, flags)
                } else {
                    probe([outdoor; 3], 4, 16.0, ProbeFlags::VALID)
                }
            })
            .collect();
        BakedGiVolume::from_probes(
            &layout,
            BakeLights {
                sky_scale: 1.0,
                ..BakeLights::default()
            },
            BakeInfo::default(),
            &probes,
        )
        .expect("volume")
    }

    #[test]
    fn a_version_1_file_looks_up_bit_for_bit_as_before() {
        // Terrain, two overlapping bricks, invalid probes and walls in the moments: every path
        // of the old lookup.
        let mut layout = flat_terrain(|_| [1.0; 3]).layout();
        for (origin, counts) in [([4.0, 10.0, 4.0], [5, 4, 5]), ([6.0, 10.5, 2.0], [4, 3, 6])] {
            layout.bricks.push(DetailBrick {
                origin,
                spacing: [1.0, 1.5, 1.0],
                counts,
                first_probe: 0,
                flags: 0,
            });
        }
        layout.assign_brick_offsets();
        let probes: Vec<_> = (0..layout.probe_count())
            .map(|i| {
                let l = 0.5 + (i % 11) as f32 * 0.3;
                let flags = if i % 7 == 3 {
                    ProbeFlags::INSIDE_GEOMETRY
                } else {
                    ProbeFlags::VALID
                };
                let mut p = probe([l, 0.8 * l, 0.6 * l], 4, 16.0, flags);
                if i % 5 == 0 {
                    p.moments = vec![[1.5, 2.5]; 16];
                }
                p
            })
            .collect();
        let vol = BakedGiVolume::from_probes(
            &layout,
            BakeLights {
                sky_scale: 1.0,
                ..BakeLights::default()
            },
            BakeInfo::default(),
            &probes,
        )
        .expect("volume");
        let v1 = BakedGiVolume::from_bytes(&vol.to_bytes_v1().expect("v1 bytes")).expect("v1 reads");
        let v2 = BakedGiVolume::from_bytes(&vol.to_bytes().expect("v2 bytes")).expect("v2 reads");
        assert!(v1.bricks.iter().all(|b| b.flags == 0));
        let s = BakedGiSettings::default();
        let k = scales(1.0);
        let normals = [
            [0.0, 1.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 0.0, -1.0],
            [0.6, 0.0, 0.8],
        ];
        let mut lit = 0;
        for xi in 0..14 {
            for yi in 0..6 {
                for zi in 0..14 {
                    let p = [
                        -1.0 + xi as f32 * 0.83,
                        10.2 + yi as f32 * 0.9,
                        -1.0 + zi as f32 * 0.79,
                    ];
                    for n in normals {
                        let v = normalize([n[0] + 0.3, n[1] + 0.5, n[2] - 0.2]);
                        let before = v1_reference::sample(&v1, &s, &k, p, n, v);
                        let now = sample_static_gi(&v1, &s, &k, p, n, v);
                        assert_eq!(sample_bits(&now), sample_bits(&before), "{p:?} {n:?}");
                        assert_eq!(
                            sample_bits(&sample_static_gi(&v2, &s, &k, p, n, v)),
                            sample_bits(&before)
                        );
                        lit += usize::from(before.coverage > 0.0);
                    }
                }
            }
        }
        assert!(lit > 1000, "the grid samples the volume ({lit} lit)");
    }

    #[test]
    fn an_interior_brick_keeps_the_terrain_out_and_does_not_blend_outward() {
        let s = BakedGiSettings {
            normal_bias: 0.0,
            view_bias: 0.0,
            ..BakedGiSettings::default()
        };
        let k = scales(1.0);
        let up = [0.0, 1.0, 0.0];
        let interior_room = |outdoor, inside| room(outdoor, inside, true, true);
        let dim = interior_room(1.0, 0.05);
        // Deep inside: the interior's light.
        let deep = sample_static_gi(&dim, &s, &k, [6.0, 12.0, 6.0], up, up);
        assert!((deep.irradiance_over_pi[0] - 0.05).abs() < 0.002, "{deep:?}");
        assert_eq!(deep.coverage, 1.0);
        // Half a cell inside a face (a plain brick blends half terrain there, see
        // `a_brick_overrides_the_terrain_inside_and_blends_over_its_last_cell`): still only the
        // interior, at full coverage.
        let edge = sample_static_gi(&dim, &s, &k, [4.5, 12.0, 6.0], up, up);
        assert!((edge.irradiance_over_pi[0] - 0.05).abs() < 0.002, "{edge:?}");
        assert_eq!(edge.coverage, 1.0);
        let plain = sample_static_gi(&room_plain(1.0, 0.05), &s, &k, [4.5, 12.0, 6.0], up, up);
        assert!(
            plain.irradiance_over_pi[0] > 0.4,
            "a plain brick blends: {plain:?}"
        );
        // Zero terrain contribution inside: a hundred times the outdoor light changes nothing
        // (any leak would show at that ratio).
        let bright = interior_room(100.0, 0.05);
        let same = |a: &StaticGiSample, b: &StaticGiSample| {
            a.coverage == b.coverage
                && a.irradiance_over_pi
                    .iter()
                    .zip(&b.irradiance_over_pi)
                    .all(|(x, y)| (x - y).abs() <= 1e-5 * (1.0 + y.abs()))
        };
        for p in [[6.0, 12.0, 6.0], [4.5, 12.0, 6.0], [7.6, 13.4, 4.4]] {
            let a = sample_static_gi(&bright, &s, &k, p, up, up);
            let b = sample_static_gi(&dim, &s, &k, p, up, up);
            assert!(same(&a, &b), "{p:?}: {a:?} {b:?}");
            assert!((b.irradiance_over_pi[0] - 0.05).abs() < 0.002, "{p:?}: {b:?}");
        }
        // The interior's light stays in: outside the box, or on its outer wall (normal out of
        // the box), the outdoor light only, whatever the interior holds.
        let lit_room = interior_room(1.0, 100.0);
        let out = [-1.0, 0.0, 0.0];
        for (p, n) in [
            ([2.0, 11.5, 6.0], up),
            ([4.0, 12.0, 6.0], out),
            ([4.2, 12.0, 6.0], out),
        ] {
            let a = sample_static_gi(&lit_room, &s, &k, p, n, n);
            let b = sample_static_gi(&dim, &s, &k, p, n, n);
            assert!(same(&a, &b), "{p:?}: {a:?} {b:?}");
            assert!((a.irradiance_over_pi[0] - 1.0).abs() < 0.05, "{p:?}: {a:?}");
        }
        // The same wall seen from inside (normal into the room) reads the interior.
        let inner = sample_static_gi(&dim, &s, &k, [4.2, 12.0, 6.0], [1.0, 0.0, 0.0], [1.0, 0.0, 0.0]);
        assert!((inner.irradiance_over_pi[0] - 0.05).abs() < 0.002, "{inner:?}");
        // An interior whose probes all failed: black at full coverage, no outdoor leak.
        let dead = room_dead(1.0);
        let r = sample_static_gi(&dead, &s, &k, [6.0, 12.0, 6.0], up, up);
        assert_eq!(r.coverage, 1.0);
        assert_eq!(r.irradiance_over_pi, [0.0; 3]);
    }

    fn room_plain(outdoor: f32, inside: f32) -> BakedGiVolume {
        room(outdoor, inside, true, false)
    }

    fn room_dead(outdoor: f32) -> BakedGiVolume {
        room(outdoor, 0.05, false, true)
    }

    #[test]
    fn an_interior_brick_wins_over_an_enclosing_brick() {
        // A large outdoor brick around the room: inside the room the interior brick lights the
        // point even though the outdoor one holds it deeper.
        let mut layout = room(1.0, 0.05, true, true).layout();
        layout.bricks.insert(
            0,
            DetailBrick {
                origin: [0.0, 9.0, 0.0],
                spacing: [1.0; 3],
                counts: [13, 8, 13],
                first_probe: 0,
                flags: 0,
            },
        );
        layout.assign_brick_offsets();
        let room_first = layout.bricks[1].first_probe as usize;
        let probes: Vec<_> = (0..layout.probe_count())
            .map(|i| {
                probe(
                    [if i >= room_first { 0.05 } else { 1.0 }; 3],
                    4,
                    16.0,
                    ProbeFlags::VALID,
                )
            })
            .collect();
        let vol = BakedGiVolume::from_probes(
            &layout,
            BakeLights {
                sky_scale: 1.0,
                ..BakeLights::default()
            },
            BakeInfo::default(),
            &probes,
        )
        .expect("volume");
        assert_eq!(vol.interior_brick_count(), 1);
        let s = BakedGiSettings {
            normal_bias: 0.0,
            view_bias: 0.0,
            ..BakedGiSettings::default()
        };
        let up = [0.0, 1.0, 0.0];
        let inside = sample_static_gi(&vol, &s, &scales(1.0), [6.0, 12.0, 6.0], up, up);
        assert!((inside.irradiance_over_pi[0] - 0.05).abs() < 0.002, "{inside:?}");
        let outside = sample_static_gi(&vol, &s, &scales(1.0), [2.0, 12.0, 6.0], up, up);
        assert!((outside.irradiance_over_pi[0] - 1.0).abs() < 0.05, "{outside:?}");
    }

    /// Look review (ADR-1364): a surface on an interior brick's face, or a few ulps outside it
    /// by a wall (a concave edge), reads the interior at full coverage; before the slack these
    /// pixels flipped to the outdoor light and drew bright seams along every room edge. A point
    /// further out than the slack still reads the outdoor light.
    #[test]
    fn surfaces_on_an_interior_brick_face_read_the_interior() {
        let s = BakedGiSettings {
            normal_bias: 0.0,
            view_bias: 0.0,
            ..BakedGiSettings::default()
        };
        let k = scales(1.0);
        let up = [0.0, 1.0, 0.0];
        let east = [1.0, 0.0, 0.0];
        // Probe box (4, 10, 4)..(8, 14, 8), 1 m cells, inside 0.05, outdoor 1.
        let dim = room(1.0, 0.05, true, true);
        for (p, n) in [
            // A floor half a cell under the box: pushed up, it lands exactly on the bottom face.
            ([6.0, 9.5, 6.0], up),
            // A floor pixel just outside the west face, by the wall.
            ([4.0 - 1e-4, 10.0, 6.0], up),
            ([4.0 - 0.1, 10.0, 6.0], up),
            // The west wall's inner face just under the bottom face, by the floor.
            ([4.0, 10.0 - 1e-4, 6.0], east),
            // The corner where the floor meets two walls.
            ([4.0 - 1e-4, 10.0, 4.0 - 1e-4], up),
        ] {
            let r = sample_static_gi(&dim, &s, &k, p, n, n);
            assert!(
                (r.irradiance_over_pi[0] - 0.05).abs() < 0.002,
                "{p:?} {n:?}: {r:?}"
            );
            assert_eq!(r.coverage, 1.0, "{p:?}");
        }
        // A fifth of a cell outside (beyond the slack of an eighth): the outdoor light.
        for p in [[4.0 - 0.2, 11.0, 6.0], [8.0 + 0.2, 11.0, 6.0]] {
            let r = sample_static_gi(&dim, &s, &k, p, up, up);
            assert!((r.irradiance_over_pi[0] - 1.0).abs() < 0.05, "{p:?}: {r:?}");
        }
    }

    #[test]
    fn the_brick_table_carries_the_flags_in_word_7() {
        let vol = room(1.0, 0.05, true, true);
        let words = VolumeWords::of(&vol);
        let mut bytes = vec![0u8; words.end * 4];
        write_volume(&vol, &words, |at, b| bytes[at..at + b.len()].copy_from_slice(b));
        let word = |w: usize| u32::from_le_bytes(bytes[w * 4..w * 4 + 4].try_into().expect("4"));
        assert_eq!(
            BRICK_WORDS, 12,
            "the table did not grow: flags use the spare word"
        );
        assert_eq!(word(words.bricks + 3), vol.bricks[0].first_probe);
        assert_eq!(word(words.bricks + 7), BrickFlags::INTERIOR);
        assert_eq!(word(words.bricks + 8), 5);
        assert_eq!(words.grid, words.bricks + BRICK_WORDS);
        assert_eq!(words.end, words.grid + BrickGrid::build(&vol.bricks).word_count());
        let src = screen_source();
        assert!(src.contains("const BGI_BRICK_INTERIOR: u32 = 1u;"));
        assert!(src.contains(&format!("const BGI_MAX_OCCLUDERS: u32 = {MAX_GI_OCCLUDERS}u;")));
        assert!(src.contains(&format!("array<vec4<f32>, {}>", 2 * MAX_GI_OCCLUDERS)));
        assert!(src.contains(&format!("BGI_INTERIOR_PRIORITY: f32 = {INTERIOR_PRIORITY:.1e}")));
        assert!(src.contains(&format!(
            "BGI_INTERIOR_EDGE_SLACK: f32 = {INTERIOR_EDGE_SLACK:?};"
        )));
    }

    #[test]
    fn a_capsule_darkens_the_baked_light_by_its_analytic_factor() {
        // The screen pass multiplies the lookup toward each direction by the capsules' factor:
        // a sphere of 1 m, 2 m above a lit point, leaves 3/4 of the light from above.
        let vol = flat_terrain(|_| [2.0, 1.0, 0.5]);
        let s = BakedGiSettings::default();
        let up = [0.0, 1.0, 0.0];
        let p = [6.0, 11.5, 6.0];
        let open = sample_static_gi(&vol, &s, &scales(1.0), p, up, up);
        let caps = [GiCapsule::new([5.0, 13.5, 6.0], [7.0, 13.5, 6.0], 1.0)];
        let k = capsule_visibility(&caps, p, up);
        assert!((k - 0.75).abs() < 1e-6, "{k}");
        assert!((open.irradiance_over_pi[0] * k - 1.5).abs() < 0.04, "{open:?}");
        // Behind the receiver (under the ground it faces up from): no change at all.
        let under = [GiCapsule::new([5.0, 9.0, 6.0], [7.0, 9.0, 6.0], 1.0)];
        assert_eq!(capsule_visibility(&under, p, up), 1.0);
    }

    #[test]
    fn the_occluder_list_is_capped_and_reported() {
        let mut r = crate::FrameRenderer::new();
        let ok = GiCapsule::sphere([0.0, 1.0, 0.0], 0.5);
        let mut list = vec![ok; MAX_GI_OCCLUDERS + 3];
        list.push(GiCapsule { radius: -1.0, ..ok });
        assert_eq!(r.set_gi_occluders(&list), MAX_GI_OCCLUDERS);
        let st = r.baked_gi_status();
        assert_eq!(st.occluders, MAX_GI_OCCLUDERS);
        assert_eq!(st.occluders_dropped, 4);
        assert_eq!(st.interior_bricks, 0);
        assert_eq!(r.gi_occluders().len(), MAX_GI_OCCLUDERS);
        r.set_baked_gi(Some(Arc::new(room(1.0, 0.05, true, true))));
        assert_eq!(r.baked_gi_status().interior_bricks, 1);
        // A stand-in shares them; an empty list clears them.
        let mut stand_in = crate::FrameRenderer::new();
        r.share_installs_with(&mut stand_in);
        assert_eq!(stand_in.gi_occluders().len(), MAX_GI_OCCLUDERS);
        assert_eq!(r.set_gi_occluders(&[]), 0);
        assert_eq!(r.baked_gi_status().occluders, 0);
        assert_eq!(r.baked_gi_status().occluders_dropped, 0);
    }

    #[test]
    fn the_lookup_reports_its_brick_and_terrain_shares() {
        // A 1 m brick over 4 m terrain columns (`room`): deep in the brick it holds all the
        // light, half a cell inside its face half, outside it the terrain layer holds it all.
        let vol = room(1.0, 0.5, true, false);
        let s = BakedGiSettings::default();
        let up = [0.0, 1.0, 0.0];
        let deep = sample_static_gi(&vol, &s, &scales(1.0), [6.0, 12.0, 6.0], up, up);
        assert!((deep.brick_share - 1.0).abs() < 1e-6, "{deep:?}");
        assert_eq!(deep.terrain_share, 0.0);
        assert_eq!(deep.brick_cell, 1.0);
        let edge = sample_static_gi(&vol, &s, &scales(1.0), [6.0, 12.0, 4.5], up, up);
        assert!((edge.brick_share - 0.5).abs() < 1e-5, "{edge:?}");
        assert!(
            (edge.brick_share + edge.terrain_share - edge.coverage).abs() < 1e-5,
            "{edge:?}"
        );
        let out = sample_static_gi(&vol, &s, &scales(1.0), [1.0, 11.5, 1.0], up, up);
        assert_eq!(out.brick_share, 0.0);
        assert_eq!(out.brick_cell, 0.0);
        assert!(
            (out.terrain_share - out.coverage).abs() < 1e-6 && out.coverage > 0.9,
            "{out:?}"
        );
        // The split the screen pass hands the SSAO: ρ = half the brick's cell, half the
        // terrain's column spacing.
        assert_eq!(
            (deep.terrain_cell, edge.terrain_cell, out.terrain_cell),
            (0.0, 4.0, 4.0)
        );
        let split = ProbeAoSplit::of(&edge);
        assert_eq!((split.brick_rho, split.terrain_rho), (0.5, 2.0));
    }

    #[test]
    fn probe_ao_wgsl_copies_match() {
        // The keep ramp is written twice (the SSAO kernel and the baked screen pass): the two
        // blocks are the same text, and the screen pass's resolve fraction is the CPU's.
        fn block(src: &str) -> &str {
            let start = src.find("// BEGIN probe-relative AO").expect("begin marker");
            let end = src[start..].find("// END probe-relative AO").expect("end marker");
            &src[start..start + end]
        }
        let ssao = include_str!("../shaders/ssao.wgsl");
        assert_eq!(block(ssao), block(SCREEN));
        assert!(block(SCREEN).contains("clamp(1.5 - height / max(rho, 1e-4), 0.0, 1.0)"));
        assert!(SCREEN.contains(&format!("const PROBE_AO_RESOLVE: f32 = {PROBE_AO_RESOLVE:?};")));
        // Three split words per texel, after the texels (`SPLIT_WORDS`), in both shaders.
        assert!(SCREEN.contains("3u * (gid.y * ddgi.screen.y + gid.x)"));
        assert!(ssao.contains("3u * (u32(t.y) * params.probe.y + u32(t.x))"));
        assert_eq!(SPLIT_WORDS, 3);
        assert_eq!(probe_ao_keep(0.2, 0.5), 1.0);
        // The occluder word: after the split words, in the writer and the reader, with one bit.
        assert_eq!(OCCLUDER_WORDS, 1);
        assert!(SCREEN.contains(
            "ddgi.screen.y * ddgi.screen.z * (DDGI_SCREEN_WORDS + 3u) + gid.y * ddgi.screen.y + gid.x"
        ));
        let sample = crate::ddgi_shading_wgsl();
        assert!(sample.contains("ddgi.screen.y * ddgi.screen.z * (DDGI_SCREEN_WORDS + 3u)"));
        assert!(COMMON.contains(&format!(
            "const DDGI_CTRL_OCCLUDER_VIS: u32 = {CTRL_BAKED_OCCLUDER_VIS}u;"
        )));
    }

    #[test]
    fn the_screen_shader_parses_and_validates() {
        let src = screen_source();
        let module =
            naga::front::wgsl::parse_str(&src).unwrap_or_else(|e| panic!("{}", e.emit_to_string(&src)));
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .unwrap_or_else(|e| panic!("{e:?}"));
    }

    /// A half lookup that does not fit the storage binding must not be the size the screen
    /// pass dispatches: that pass writes every texel with no `arrayLength` check.
    #[test]
    fn audfix_half_lookup_past_the_binding_stays_inside_it() {
        let binding_max = 1u64 << 20;
        let full = (3840, 2160);
        let (w, h, step) =
            screen_lookup_for_binding(full, crate::ddgi::GiLookupResolution::Half, binding_max);
        let bytes = u64::from(w) * u64::from(h) * (SCREEN_WORDS + SPLIT_WORDS) * 4;
        assert!(
            bytes <= binding_max,
            "dispatch {w}x{h} step {step} writes {bytes} bytes into a {binding_max}-byte binding"
        );
        assert!(w >= 1 && h >= 1);
        let covered_w = w.saturating_mul(step);
        let covered_h = h.saturating_mul(step);
        assert!(
            covered_w >= full.0 && covered_h >= full.1,
            "step {step} grid {w}x{h} does not cover {full:?}"
        );
    }

    /// A half lookup that fits stays half, and an empty frame stays a 1×1 step-2 grid.
    #[test]
    fn audfix_half_lookup_that_fits_stays_half() {
        let binding_max = 64u64 << 20;
        let (w, h, step) =
            screen_lookup_for_binding((1920, 1080), crate::ddgi::GiLookupResolution::Half, binding_max);
        assert_eq!((w, h, step), (960, 540, 2));
        let (w, h, step) =
            screen_lookup_for_binding((0, 0), crate::ddgi::GiLookupResolution::Half, binding_max);
        assert_eq!((w, h, step), (1, 1, 2));
    }

    /// Review baked-light #4: the bake's size check (`VolumeLayout::runtime_buffer_bytes`) is
    /// the buffer this module creates, so a volume the bake accepts fits the binding it checks.
    #[test]
    fn the_bake_size_check_matches_the_uploaded_buffer() {
        assert_eq!(BRICK_WORDS as u64 * 4, regia_asset::baked_gi::RUNTIME_BRICK_BYTES);
        for vol in [flat_terrain(|_| [1.0; 3]), room(1.0, 0.05, true, true)] {
            let words = VolumeWords::of(&vol);
            assert_eq!(
                (words.end as u64 * 4).max(16),
                vol.layout().runtime_buffer_bytes()
            );
        }
    }

    /// A volume whose bricks are the interleaved, overlapping boxes `boxes` (origin, counts,
    /// spacing, interior), with all-valid probes: only the table matters to the selection tests.
    fn bricks_volume(boxes: &[([f32; 3], [u32; 3], f32, bool)]) -> BakedGiVolume {
        let mut layout = flat_terrain(|_| [1.0; 3]).layout();
        for &(origin, counts, spacing, interior) in boxes {
            layout.bricks.push(DetailBrick {
                origin,
                spacing: [spacing; 3],
                counts,
                first_probe: 0,
                flags: if interior { BrickFlags::INTERIOR } else { 0 },
            });
        }
        layout.assign_brick_offsets();
        let probes: Vec<_> = (0..layout.probe_count())
            .map(|_| probe([1.0; 3], 4, 16.0, ProbeFlags::VALID))
            .collect();
        BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
            .expect("volume")
    }

    /// ADR-1835: scanning only the grid's candidates picks the brick the full scan picks, for
    /// every point of a dense sweep in and around overlapping plain and interior bricks, with
    /// normals that push interior tests across faces.
    #[test]
    fn the_brick_grid_picks_what_the_linear_scan_picks() {
        let boxes = [
            ([0.0, 0.0, 0.0], [13, 8, 13], 1.0, false),
            ([4.0, 0.0, 4.0], [5, 5, 5], 1.0, true),
            ([6.0, 1.0, 2.0], [4, 4, 9], 0.5, false),
            ([20.0, 0.0, 0.0], [9, 9, 9], 2.0, true),
            ([22.0, 2.0, 2.0], [4, 4, 4], 1.0, false),
            ([-8.0, 0.0, -8.0], [6, 3, 6], 0.7, false),
        ];
        let vol = bricks_volume(&boxes);
        let grid = BrickGrid::build(&vol.bricks);
        let normals = [
            [0.0, 1.0, 0.0],
            [1.0, 0.0, 0.0],
            [-0.6, 0.0, 0.8],
            [0.0, -1.0, 0.0],
        ];
        let mut checked = 0usize;
        for ix in -20..=44 {
            for iy in -6..=22 {
                for iz in -20..=36 {
                    let p = [ix as f32 * 0.75, iy as f32 * 0.5, iz as f32 * 0.5];
                    for n in normals {
                        let all = select_brick(&vol, p, n, 0..vol.bricks.len());
                        let via = select_brick(&vol, p, n, grid.candidates(p).iter().map(|&b| b as usize));
                        assert_eq!(all, via, "{p:?} {n:?}");
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 100_000);
        // A point nobody holds, and one in the interior-only margin, are both covered above;
        // pin that the sweep selects something and also nothing.
        assert!(select_brick(&vol, [5.0, 2.0, 5.0], [0.0, 1.0, 0.0], 0..vol.bricks.len()).is_some());
        assert!(select_brick(&vol, [100.0; 3], [0.0, 1.0, 0.0], 0..vol.bricks.len()).is_none());
        assert!(grid.candidates([f32::NAN; 3]).is_empty());
    }

    /// An installed volume builds its brick lookup grid once. A second word-count, the upload
    /// and two status reads reuse that grid.
    #[test]
    fn an_unchanged_volume_builds_its_brick_grid_once() {
        let vol = bricks_volume(&[
            ([0.0; 3], [8, 6, 8], 1.0, true),
            ([20.0, 0.0, 0.0], [6, 4, 6], 1.0, false),
        ]);
        let before = regia_asset::baked_gi_grid::brick_grid_builds();
        let words = VolumeWords::of(&vol);
        let again = VolumeWords::of(&vol);
        assert_eq!(words, again);
        let mut uploaded = Vec::new();
        write_volume(&vol, &words, |_, bytes| uploaded.extend_from_slice(bytes));
        assert!(!uploaded.is_empty());
        let mut runtime = BakedGiRuntime::default();
        runtime.install(Some(Arc::new(vol)));
        let first = runtime.status();
        let second = runtime.status();
        assert_eq!(first.brick_grid_cells, second.brick_grid_cells);
        assert!(first.brick_grid_cells > 0);
        assert_eq!(
            regia_asset::baked_gi_grid::brick_grid_builds(),
            before + 1,
            "word count, upload and status share one grid build"
        );
    }

    /// The uploaded grid words are `BrickGrid::to_words` right after the brick table, and the
    /// shader's constants and field offsets match the Rust layout.
    #[test]
    fn the_grid_follows_the_brick_table_in_the_buffer() {
        let vol = bricks_volume(&[
            ([0.0; 3], [5, 5, 5], 1.0, false),
            ([30.0, 0.0, 0.0], [5, 5, 5], 1.0, true),
        ]);
        let words = VolumeWords::of(&vol);
        let mut bytes = vec![0u8; words.end * 4];
        write_volume(&vol, &words, |at, b| bytes[at..at + b.len()].copy_from_slice(b));
        let grid = BrickGrid::build(&vol.bricks).to_words();
        assert!(!grid.is_empty());
        let (chunks, _) = bytes[words.grid * 4..].as_chunks::<4>();
        let got: Vec<u32> = chunks.iter().map(|c| u32::from_le_bytes(*c)).collect();
        assert_eq!(got, grid);
        assert_eq!(regia_asset::baked_gi_grid::BRICK_GRID_HEADER_WORDS, 8);
        assert_eq!(
            regia_asset::baked_gi_grid::BRICK_GRID_INTERIOR_SLACK,
            INTERIOR_EDGE_SLACK
        );
        assert!(screen_source().contains("const BGI_GRID_HEADER_WORDS: u32 = 8u;"));
    }

    /// ADR-366: which corners a cell's wall codes keep. Corner `k` has bit 0 = x, 1 = y, 2 = z.
    #[test]
    fn wall_codes_keep_the_receivers_side_of_the_cell() {
        let cell = |f: [f32; 3]| CellAxes {
            axis: [0, 1, 2],
            same: [false; 3],
            f,
            normal: [0.0; 3],
            guard: false,
        };
        let ones = [1.0f32; 8];
        // No codes: every corner, the lookup as before.
        assert_eq!(
            wall_mask(&[ProbeFlags::VALID; 8], &ones, 0xff, &cell([0.7, 0.5, 0.5])),
            0xff
        );
        // A wall across every x edge at the middle: the receiver's side of it.
        let mut flags = [ProbeFlags::VALID; 8];
        for k in [0, 2, 4, 6] {
            flags[k] |= ProbeFlags::wall(0, 0.5);
        }
        assert_eq!(wall_mask(&flags, &ones, 0xff, &cell([0.7, 0.5, 0.5])), 0xaa);
        assert_eq!(wall_mask(&flags, &ones, 0xff, &cell([0.3, 0.5, 0.5])), 0x55);
        // The receiver's side holds no valid probe: the side with the weight.
        let far_side_dead = [1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0];
        assert_eq!(
            wall_mask(&flags, &far_side_dead, 0xff, &cell([0.7, 0.5, 0.5])),
            0x55
        );
        // An edge clamped to one probe is never a wall.
        let clamped = CellAxes {
            same: [true, false, false],
            ..cell([0.7, 0.5, 0.5])
        };
        assert_eq!(wall_mask(&flags, &ones, 0xff, &clamped), 0xff);
        // A door: only the two x edges at y = 0 are walled; the y = 1 edges join the sides.
        let mut door = [ProbeFlags::VALID; 8];
        for k in [0, 4] {
            door[k] |= ProbeFlags::wall(0, 0.5);
        }
        assert_eq!(wall_mask(&door, &ones, 0xff, &cell([0.7, 0.2, 0.5])), 0xff);
        // A slanted wall (x = 0.2 at z = 0, x = 0.8 at z = 1): the receiver's side follows the
        // wall's position at its own z.
        let mut slant = [ProbeFlags::VALID; 8];
        for k in [0, 2] {
            slant[k] |= ProbeFlags::wall(0, 0.2);
        }
        for k in [4, 6] {
            slant[k] |= ProbeFlags::wall(0, 0.8);
        }
        assert_eq!(wall_mask(&slant, &ones, 0xff, &cell([0.5, 0.5, 0.0])), 0xaa);
        assert_eq!(wall_mask(&slant, &ones, 0xff, &cell([0.5, 0.5, 1.0])), 0x55);
        // In a terrain cell corner bit 1 is the column z (flags axis 2) and bit 2 the level
        // (flags axis 1): a Y (level) code on the low level walls the bit-2 edges.
        let terrain = CellAxes {
            axis: [0, 2, 1],
            same: [false; 3],
            f: [0.5, 0.5, 0.2],
            normal: [0.0; 3],
            guard: false,
        };
        let mut roof = [ProbeFlags::VALID; 8];
        for k in [0, 1, 2, 3] {
            roof[k] |= ProbeFlags::wall(1, 0.5);
        }
        assert_eq!(wall_mask(&roof, &ones, 0xff, &terrain), 0x0f);
        assert_eq!(
            wall_mask(&roof, &ones, 0xff, &cell([0.5, 0.5, 0.2])),
            0xff,
            "the wrong axis"
        );
    }

    /// ADR-861 leak guard, the case the room harness found: the bottom x edges run to probes
    /// under the floor (rejected) and their codes mark the floor crossing at 0.3, not the wall at
    /// 0.83. Unguarded they drag the wall position to 0.47 and a receiver on the inner wall face
    /// at 0.59 reads the outdoor corners; guarded the position is the valid edges' 0.83.
    #[test]
    fn the_leak_guard_keeps_codes_on_edges_to_rejected_probes_out_of_the_wall_position() {
        let mut flags = [ProbeFlags::VALID; 8];
        flags[1] = ProbeFlags::UNDER_TERRAIN | ProbeFlags::wall(1, 0.17);
        flags[5] = ProbeFlags::UNDER_TERRAIN | ProbeFlags::wall(1, 0.23);
        flags[0] |= ProbeFlags::wall(0, 0.3);
        flags[4] |= ProbeFlags::wall(0, 0.3);
        flags[2] |= ProbeFlags::wall(0, 0.83);
        flags[6] |= ProbeFlags::wall(0, 0.83);
        let weights = [1.0, 0.0, 1.0, 1.0, 1.0, 0.0, 1.0, 1.0];
        let valid = 0b1101_1101;
        let cell = |guard: bool| CellAxes {
            axis: [0, 1, 2],
            same: [false; 3],
            f: [0.592, 0.322, 0.621],
            normal: [-1.0, 0.0, 0.0],
            guard,
        };
        assert_eq!(
            wall_mask(&flags, &weights, valid, &cell(false)),
            0x88,
            "the outdoor corners"
        );
        assert_eq!(
            wall_mask(&flags, &weights, valid, &cell(true)),
            0x55,
            "the room's corners"
        );
        // No wall code at all: every corner, guard or not.
        let plain = [ProbeFlags::VALID; 8];
        assert_eq!(wall_mask(&plain, &weights, valid, &cell(true)), 0xff);
    }

    /// ADR-861 leak guard: when every wall along an axis sits on an edge to a rejected probe,
    /// its position is unknown and the receiver's normal picks the side (a receiver on an inner
    /// wall facing −x); without that the anchor falls on the rejected corner and the heaviest
    /// group, outdoors, lights the receiver.
    #[test]
    fn the_leak_guard_lets_the_normal_pick_the_side_of_an_unknown_wall() {
        let mut flags = [ProbeFlags::VALID; 8];
        flags[0] |= ProbeFlags::wall(1, 0.83);
        flags[1] |= ProbeFlags::wall(1, 0.7) | ProbeFlags::wall(2, 0.9);
        flags[4] |= ProbeFlags::wall(0, 0.5) | ProbeFlags::wall(1, 0.7);
        flags[5] = ProbeFlags::INSIDE_GEOMETRY | ProbeFlags::wall(1, 0.7);
        // The outdoor group (2, 3, 6, 7) carries more weight than the room's (0, 1, 4).
        let weights = [0.1, 0.1, 1.0, 1.0, 0.1, 0.0, 1.0, 1.0];
        let valid = 0b1101_1111;
        let cell = |normal: [f32; 3], guard: bool| CellAxes {
            axis: [0, 1, 2],
            same: [false; 3],
            f: [0.595, 0.62, 0.561],
            normal,
            guard,
        };
        let room = 0b0001_0011;
        assert_eq!(
            wall_mask(&flags, &weights, valid, &cell([-1.0, 0.0, 0.0], true)),
            room
        );
        assert_eq!(
            wall_mask(&flags, &weights, valid, &cell([0.0, 1.0, 0.0], true)),
            0xcc,
            "a normal across the unknown axis does not decide it"
        );
        assert_eq!(
            wall_mask(&flags, &weights, valid, &cell([-1.0, 0.0, 0.0], false)),
            room
        );
    }

    /// Unit vectors spread evenly over the sphere (a Fibonacci lattice).
    fn sphere(count: usize) -> Vec<V3> {
        let golden = std::f32::consts::PI * (3.0 - 5.0f32.sqrt());
        (0..count)
            .map(|i| {
                let y = 1.0 - 2.0 * (i as f32 + 0.5) / count as f32;
                let r = (1.0 - y * y).max(0.0).sqrt();
                let phi = golden * i as f32;
                [r * phi.cos(), y, r * phi.sin()]
            })
            .collect()
    }

    /// ADR-861 de-ringing: up to |r1| = r0 (a hemisphere of light) the evaluation is the linear
    /// form bit for bit; past it the lobe is never negative and keeps the mean and the first
    /// moment over the sphere that the clamped linear form gets wrong.
    #[test]
    fn deringing_keeps_the_mean_and_first_moment_and_never_goes_negative() {
        let y00 = regia_asset::baked_gi::SH_Y00;
        let a1 = regia_asset::baked_gi::SH_Y1 * (2.0 / 3.0);
        let dirs = sphere(20_000);
        // Just under a hemisphere from +y (|r1| = 0.999 r0): the linear form, bit for bit.
        let hemi_l0 = 1.0f32;
        let hemi_l1 = [0.0, 0.999 * hemi_l0 * y00 / a1, 0.0];
        for d in dirs.iter().step_by(97) {
            assert_eq!(
                channel_irradiance(hemi_l0, hemi_l1, *d, true).to_bits(),
                channel_irradiance(hemi_l0, hemi_l1, *d, false).to_bits()
            );
        }
        // A narrow source along (0.6, 0.0, 0.8): |r1| = 1.8 r0.
        let axis = [0.6f32, 0.0, 0.8];
        let l1 = mul(axis, 1.8 * y00 / a1);
        let r0 = y00;
        let r1 = mul(axis, 1.8 * y00);
        let moments = |dering: bool| {
            let (mut mean, mut first) = (0.0f64, [0.0f64; 3]);
            for d in &dirs {
                let e = channel_irradiance(1.0, l1, *d, dering);
                assert!(e >= 0.0, "{e} toward {d:?}");
                mean += f64::from(e);
                for (f, c) in first.iter_mut().zip(d) {
                    *f += f64::from(e * c);
                }
            }
            let n = dirs.len() as f64;
            (mean / n, first.map(|f| f / n))
        };
        let (mean, first) = moments(true);
        assert!(
            (mean - f64::from(r0)).abs() < 2e-3 * f64::from(r0),
            "mean {mean} vs {r0}"
        );
        for (f, r) in first.iter().zip(r1) {
            // The linear form's first moment is r1 / 3.
            assert!(
                (f - f64::from(r) / 3.0).abs() < 5e-3 * f64::from(r0),
                "first {first:?} vs {r1:?} / 3"
            );
        }
        let (clamped_mean, _) = moments(false);
        assert!(
            clamped_mean > mean * 1.05,
            "the clamp adds energy: {clamped_mean} vs {mean}"
        );
        // Right beside the source (90°) the clamped linear form reads r0, the lobe half of it.
        let side = [0.0, 1.0, 0.0];
        assert!(channel_irradiance(1.0, l1, side, true) < 0.6 * channel_irradiance(1.0, l1, side, false));
    }

    /// ADR-861 sun relight: a probe lit from below by the ground takes the ground factor, light
    /// from above none of it, and `SunRelight::NONE` (or the switch off) changes nothing.
    #[test]
    fn sun_relight_scales_ground_bounce_by_the_sun_elevation() {
        let none = SunRelight::NONE;
        assert_eq!(none.ground_factor(), 1.0);
        let lower = SunRelight {
            baked_up: 60f32.to_radians().sin(),
            frame_up: 25f32.to_radians().sin(),
        };
        let g = lower.ground_factor();
        assert!((g - (0.4226 + 0.02) / (0.8660 + 0.02)).abs() < 1e-3, "{g}");
        // Radiance uniform over the lower hemisphere: L1 = −0.866 L00 along y.
        let from_below = |l0: f32| -> ShRgb { [[l0, 0.0, -HEMISPHERE_L1 * l0, 0.0]; 3] };
        assert!((relight_factor(&from_below(2.0), g) - g).abs() < 1e-6);
        assert_eq!(relight_factor(&from_below(2.0), 1.0), 1.0);
        let from_above: ShRgb = [[2.0, 0.0, HEMISPHERE_L1 * 2.0, 0.0]; 3];
        assert_eq!(relight_factor(&from_above, g), 1.0);
        // Half as directional, from below: half the change.
        let half: ShRgb = [[2.0, 0.0, -0.5 * HEMISPHERE_L1 * 2.0, 0.0]; 3];
        assert!((relight_factor(&half, g) - (1.0 + 0.5 * (g - 1.0))).abs() < 1e-6);
        assert_eq!(relight_factor(&[[0.0; 4]; 3], g), 1.0, "a dark probe");

        let vol = flat_terrain(|_| [1.0; 3]);
        let lights = BakedGiFrameLights {
            sun: Some([1000.0; 3]),
            to_sun: Some([0.3, 0.4, 0.0]),
            fill: None,
        };
        let on = BakedGiSettings::default();
        assert_eq!(
            sun_relight(&vol, &on, &lights),
            SunRelight::NONE,
            "baked without a sun"
        );
        let mut sunny = vol.clone();
        sunny.lights.sun_illuminance = 1000.0;
        sunny.lights.sun_direction = [0.0, 2.0, 0.0];
        let r = sun_relight(&sunny, &on, &lights);
        assert!(
            (r.baked_up - 1.0).abs() < 1e-6 && (r.frame_up - 0.8).abs() < 1e-6,
            "{r:?}"
        );
        let off = BakedGiSettings {
            sun_relight: false,
            ..on
        };
        assert_eq!(sun_relight(&sunny, &off, &lights), SunRelight::NONE);
        let as_baked = BakedGiSettings {
            follow_sun: false,
            ..on
        };
        assert_eq!(sun_relight(&sunny, &as_baked, &lights), SunRelight::NONE);
        let no_dir = BakedGiFrameLights {
            to_sun: None,
            ..lights
        };
        assert_eq!(sun_relight(&sunny, &on, &no_dir), SunRelight::NONE);
    }

    /// ADR-1386: the set finds each recorded emitter from a drawn emitter's bounds (also a large
    /// emitter whose slack reaches past smaller ones along X), and nothing else.
    #[test]
    fn the_baked_emitter_set_finds_exactly_the_recorded_emitters() {
        let at = |lo: [f32; 3], hi: [f32; 3]| BakedEmitter { min: lo, max: hi };
        let strip = at([0.0, 3.0, 0.0], [12.0, 3.05, 0.1]);
        let mut list = vec![strip];
        for k in 0..40 {
            let x = 0.05 + k as f32 * 0.5;
            list.push(at([x, 1.0, 2.0], [x + 0.3, 1.3, 2.3]));
        }
        let set = BakedEmitterSet::new(&list);
        assert_eq!(set.len(), 41);
        assert!(!set.is_empty());
        for e in &list {
            assert!(set.holds(e), "{e:?}");
        }
        // The strip's slack is about 26 cm: shifted 20 cm along X it is still the strip, though
        // its min X now sorts after the lamps at 0.05 and 0.55.
        assert!(set.holds(&at([0.2, 3.0, 0.0], [12.2, 3.05, 0.1])));
        // Lamps moved 10 cm, a lamp between two recorded ones, one somewhere else: no match.
        assert!(!set.holds(&at([0.15, 1.0, 2.0], [0.45, 1.3, 2.3])));
        assert!(!set.holds(&at([0.3, 1.0, 2.0], [0.6, 1.3, 2.3])));
        assert!(!set.holds(&at([0.05, 4.0, 2.0], [0.35, 4.3, 2.3])));
        assert!(!BakedEmitterSet::new(&[]).holds(&strip));
        assert!(BakedEmitterSet::default().is_empty());
    }

    /// ADR-1386: the forward pass reads the same control bit and light tag the CPU writes.
    #[test]
    fn the_forward_pass_reads_the_baked_emitter_bit_and_tag() {
        let forward = include_str!("../shaders/lab_forward.wgsl");
        assert!(forward.contains(&format!("ctrl.w & {CTRL_BAKED_EMITTERS}u")));
        assert!(forward.contains(&format!("ctrl.w & {CTRL_BAKED_FILL}u")));
        assert_ne!(CTRL_BAKED_EMITTERS, CTRL_BAKED_FILL);
        // `cone_shadow.w = BAKED_EMITTER_TAG - code` decodes as `-1 - w`.
        assert_eq!(crate::emissive_lights::BAKED_EMITTER_TAG, -1.0);
        assert!(forward.contains("select(w, -1.0 - w, baked)"));
    }

    /// ADR-1386: the runtime names a v3 volume's emitters (status and set), and a v2 volume's
    /// as unknown, which tags no light.
    #[test]
    fn the_runtime_names_the_baked_emitters_of_a_v3_volume_only() {
        let mut volume = room_plain(10.0, 1.0);
        let mut runtime = BakedGiRuntime::default();
        runtime.install(Some(Arc::new(volume.clone())));
        assert_eq!(runtime.status().emitters, None);
        assert!(runtime.emitter_set().is_none(), "format v2: not recorded");
        volume.emitters = Some(vec![BakedEmitter {
            min: [-0.2, 2.9, -0.2],
            max: [0.2, 3.0, 0.2],
        }]);
        runtime.install(Some(Arc::new(volume)));
        assert_eq!(runtime.status().emitters, Some(1));
        let set = runtime.emitter_set().expect("recorded");
        assert_eq!(set.len(), 1);
        assert!(
            Arc::ptr_eq(&set, &runtime.emitter_set().expect("recorded")),
            "built once"
        );
        // The bake's emitter light scaled away: the area lights keep all of it.
        runtime.settings.emissive_scale = 0.0;
        assert!(!lights_emitters(&runtime.settings));
        assert!(runtime.emitter_set().is_none());
        runtime.settings = BakedGiSettings {
            intensity: 0.0,
            ..BakedGiSettings::default()
        };
        assert!(runtime.emitter_set().is_none());
        runtime.settings = BakedGiSettings::default();
        assert!(runtime.emitter_set().is_some());
        runtime.install(None);
        assert_eq!(runtime.status().emitters, None);
        assert!(runtime.emitter_set().is_none());
    }

    fn brick_grid_builds() -> usize {
        BRICK_GRID_BUILDS.with(|n| n.get())
    }

    fn reset_brick_grid_builds() {
        BRICK_GRID_BUILDS.with(|n| n.set(0));
    }

    /// Reading status twice must not rebuild the brick grid. A new volume builds once.
    #[test]
    fn steady_status_skips_brick_grid_build() {
        reset_brick_grid_builds();
        let vol = Arc::new(bricks_volume(&[([0.0, 0.0, 0.0], [5, 5, 5], 1.0, true)]));
        let mut runtime = BakedGiRuntime::default();
        runtime.install(Some(Arc::clone(&vol)));
        let mut built = brick_grid_builds();
        let first = runtime.status();
        // A cold status may build once and keep the counts. A second call must not.
        if brick_grid_builds() != built {
            assert_eq!(
                brick_grid_builds(),
                built + 1,
                "the first status built more than once"
            );
            built = brick_grid_builds();
        }
        let second = runtime.status();
        assert_eq!(
            brick_grid_builds(),
            built,
            "status rebuilt the brick grid (builds {})",
            brick_grid_builds()
        );
        let grid = BrickGrid::build(&vol.bricks);
        assert!(grid.cell_count() > 0 && grid.entry_count() > 0);
        assert_eq!(first.brick_grid_cells, grid.cell_count());
        assert_eq!(first.brick_grid_entries, grid.entry_count());
        assert_eq!(second.brick_grid_cells, first.brick_grid_cells);
        assert_eq!(second.brick_grid_entries, first.brick_grid_entries);
        let stand_in = runtime.share();
        assert_eq!(stand_in.status().brick_grid_cells, first.brick_grid_cells);
        assert_eq!(brick_grid_builds(), built, "a stand-in rebuilt the brick grid");
        // The same Arc again is not a new table.
        runtime.install(Some(Arc::clone(&vol)));
        assert_eq!(
            brick_grid_builds(),
            built,
            "reinstalling the same volume rebuilt the grid"
        );
        let other = Arc::new(bricks_volume(&[([8.0, 0.0, 0.0], [3, 3, 3], 1.0, false)]));
        runtime.install(Some(Arc::clone(&other)));
        assert_eq!(
            brick_grid_builds(),
            built + 1,
            "replacing the volume rebuilt the grid more than once"
        );
        let swapped = runtime.status();
        assert_eq!(brick_grid_builds(), built + 1, "status rebuilt after the swap");
        let other_grid = BrickGrid::build(&other.bricks);
        assert_eq!(swapped.brick_grid_cells, other_grid.cell_count());
        assert_eq!(swapped.brick_grid_entries, other_grid.entry_count());
        runtime.install(None);
        let cleared = runtime.status();
        assert_eq!(cleared.brick_grid_cells, 0);
        assert_eq!(cleared.brick_grid_entries, 0);
        assert_eq!(
            brick_grid_builds(),
            built + 1,
            "clearing the volume rebuilt the grid"
        );
    }

    /// A blended layer builds its grid when it is set, and a repeat of the same volume does not.
    #[test]
    fn a_layer_builds_its_brick_grid_once() {
        reset_brick_grid_builds();
        let mut runtime = BakedGiRuntime::default();
        let base = Arc::new(bricks_volume(&[([0.0, 0.0, 0.0], [4, 4, 4], 1.0, false)]));
        let layer = Arc::new(bricks_volume(&[([20.0, 0.0, 0.0], [3, 3, 3], 2.0, true)]));
        runtime.install(Some(Arc::clone(&base)));
        assert_eq!(brick_grid_builds(), 1);
        let blended = runtime.set_layers(&[BakedGiLayer::new(Arc::clone(&layer))]);
        assert_eq!(blended, 2);
        assert_eq!(
            brick_grid_builds(),
            2,
            "the layer rebuilt the installed volume's grid"
        );
        runtime.set_layers(&[BakedGiLayer::new(Arc::clone(&layer))]);
        assert_eq!(brick_grid_builds(), 2, "setting the same layer rebuilt a grid");
        assert_eq!(runtime.status().brick_grid_cells, {
            BrickGrid::build(&base.bricks).cell_count()
        });
        assert_eq!(brick_grid_builds(), 2, "status rebuilt after the layer was set");
    }

    /// The words kept for the upload are the grid's own words, and taking them does not build again.
    #[test]
    fn cached_grid_upload_matches_a_fresh_build() {
        reset_brick_grid_builds();
        let vol = Arc::new(bricks_volume(&[
            ([0.0, 0.0, 0.0], [5, 5, 5], 1.0, false),
            ([2.0, 0.0, 2.0], [4, 4, 4], 1.0, true),
        ]));
        let mut runtime = BakedGiRuntime::default();
        runtime.install(Some(Arc::clone(&vol)));
        assert_eq!(brick_grid_builds(), 1);
        let words = VolumeWords::layout(&vol, runtime.ensure_grid(&vol));
        assert_eq!(brick_grid_builds(), 1, "a second lookup rebuilt the grid");
        let cached = runtime.take_grid_upload(&vol);
        assert_eq!(brick_grid_builds(), 1, "taking the stored words rebuilt the grid");
        let fresh = BrickGrid::build(&vol.bricks).to_words();
        assert_eq!(cached, fresh);
        assert_eq!(words.end - words.grid, fresh.len());
        let mut bytes = vec![0u8; words.end * 4];
        write_volume_grid(&vol, &words, &cached, |at, part| {
            bytes[at..at + part.len()].copy_from_slice(part);
        });
        let (chunks, _) = bytes[words.grid * 4..].as_chunks::<4>();
        let got: Vec<u32> = chunks.iter().map(|word| u32::from_le_bytes(*word)).collect();
        assert_eq!(got, fresh);
    }

    /// Two frames of one unchanged volume size the brick grid once, and the uploaded section
    /// offsets match a fresh layout (ported from lane fix-render4-perf-g onto this file's
    /// brick-grid build counter).
    #[test]
    fn begin_on_an_unchanged_volume_builds_the_brick_grid_once() {
        let device = crate::test_device();
        let volume = Arc::new(bricks_volume(&[
            ([0.0; 3], [5, 5, 5], 1.0, false),
            ([30.0, 0.0, 0.0], [5, 5, 5], 1.0, true),
        ]));
        let expect = VolumeWords::of(&volume);
        let mut runtime = BakedGiRuntime::default();
        runtime.install(Some(Arc::clone(&volume)));
        let mut belt = crate::staging::StagingBelt::default();
        let screen = ScreenParamsGpu {
            size: [64, 64, 0, 0],
            ..Default::default()
        };
        let lights = BakedGiFrameLights::default();
        let lookup = crate::ddgi::GiLookupResolution::Half;
        reset_brick_grid_builds();
        assert!(runtime.begin(&device.device, &mut belt, &screen, &lights, lookup));
        let built = brick_grid_builds();
        assert!(runtime.begin(&device.device, &mut belt, &screen, &lights, lookup));
        assert_eq!(
            brick_grid_builds(),
            built,
            "the second begin rebuilds the brick grid to count words ({built} -> {})",
            brick_grid_builds()
        );
        let gpu = runtime.gpu.as_ref().expect("uploaded");
        assert_eq!(gpu.words[0].grid, expect.grid);
        assert_eq!(gpu.words[0].end, expect.end);
    }

    /// Install a volume with bricks, call `begin` twice, and require the second call and
    /// `status` to add no `BrickGrid` builds while the uploaded set stays the same.
    #[test]
    fn second_begin_skips_brick_grid_build() {
        let device = match crate::RenderDevice::headless() {
            Ok(device) => device,
            Err(err) => {
                eprintln!("skipping: no GPU adapter (lavapipe) available: {err}");
                return;
            }
        };
        reset_brick_grid_builds();
        let vol = Arc::new(bricks_volume(&[
            ([0.0, 0.0, 0.0], [5, 5, 5], 1.0, false),
            ([2.0, 0.0, 2.0], [4, 4, 4], 1.0, true),
        ]));
        let mut runtime = BakedGiRuntime::default();
        runtime.install(Some(Arc::clone(&vol)));
        let screen = crate::ddgi_gpu::ScreenParamsGpu {
            size: [64, 64, 32, 32],
            ..crate::ddgi_gpu::ScreenParamsGpu::default()
        };
        let lights = BakedGiFrameLights::default();
        let mut belt = crate::staging::StagingBelt::default();
        let lookup = crate::ddgi::GiLookupResolution::Half;
        assert!(
            runtime.begin(&device.device, &mut belt, &screen, &lights, lookup),
            "begin failed: {:?}",
            runtime.status().error
        );
        let built = brick_grid_builds();
        assert!(built >= 1, "the first begin never built the brick grid");
        let generation = runtime.generation;
        assert!(
            runtime.begin(&device.device, &mut belt, &screen, &lights, lookup),
            "second begin failed: {:?}",
            runtime.status().error
        );
        assert_eq!(
            brick_grid_builds(),
            built,
            "a steady begin rebuilt the brick grid (first {built}, now {})",
            brick_grid_builds()
        );
        assert_eq!(runtime.generation, generation, "same volumes were uploaded again");
        let status = runtime.status();
        assert_eq!(
            brick_grid_builds(),
            built,
            "status rebuilt the brick grid after begin"
        );
        let grid = BrickGrid::build(&vol.bricks);
        assert_eq!(status.brick_grid_cells, grid.cell_count());
        assert_eq!(status.brick_grid_entries, grid.entry_count());
    }
}
