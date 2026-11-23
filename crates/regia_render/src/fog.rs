// SPDX-License-Identifier: MPL-2.0
//! Distance and exponential height fog (`specs/13` § 6).
//!
//! Optical depth grows with view distance and falls off with height so valleys
//! hold more fog than peaks. Provenance: Hoffman & Preetham, "Rendering Outdoor
//! Light Scattering", GDC 2002 (public slides); Filament atmospheric fog notes
//! (Apache-2.0). Re-derived; no proprietary engine code consulted.

use std::sync::{Arc, Mutex, PoisonError};

use regia_app::prelude::*;

/// Per-view fog (`specs/13` § 6: exponential height fog).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FogSettings {
    /// Master enable.
    pub enabled: bool,
    /// Base extinction density at [`Self::base_height`] (1/metres).
    pub density: f32,
    /// Exponential height falloff (1/metres). Larger values hug the ground.
    pub height_falloff: f32,
    /// World Y (metres) at which [`Self::density`] applies.
    pub base_height: f32,
    /// Fog colour in linear display-referred RGB.
    ///
    /// Shaders that light in photometric units must divide this by the EV100
    /// exposure before the mix. The exposure pass then returns it to this range.
    /// Paths that already exposure the lighting (terrain) mix this colour directly.
    ///
    /// With [`Self::sky_inscatter`] the mesh renderer uses only this colour's hue
    /// (normalised to unit luminance) as a tint on the sky in-scatter.
    pub color: [f32; 3],
    /// Aerial perspective: in-scatter the horizon sky radiance in the view direction
    /// (plus a forward-scattering glow toward the sun) instead of the flat [`Self::color`],
    /// with the optical depth integrated along the ray. Distance then recedes into the
    /// sky the frame shows. The mesh forward pass and the shared terrain colour pass
    /// both honour this; the offscreen terrain path keeps the flat colour.
    pub sky_inscatter: bool,
}

impl Default for FogSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            density: 0.012,
            height_falloff: 0.08,
            base_height: 0.0,
            color: [0.55, 0.62, 0.72],
            sky_inscatter: false,
        }
    }
}

impl FogSettings {
    /// Dense haze suitable for outdoor heightfield evidence (visible within ~150 m).
    #[must_use]
    pub fn outdoor_haze() -> Self {
        Self {
            enabled: true,
            density: 0.018,
            height_falloff: 0.06,
            base_height: 0.0,
            color: [0.58, 0.66, 0.78],
            sky_inscatter: false,
        }
    }

    /// The default look's aerial perspective ([`crate::RenderSettings::default`]):
    /// sky-coloured haze that takes ~10 % of the contrast at 500 m and ~35 % at 2 km
    /// for a camera near the base height, thinning with altitude (≈ 1 km scale height).
    /// A stand-in until the M15c atmosphere replaces it.
    #[must_use]
    pub fn aerial() -> Self {
        Self {
            enabled: true,
            density: 2.2e-4,
            height_falloff: 1.0e-3,
            base_height: 0.0,
            color: [0.70, 0.72, 0.75],
            sky_inscatter: true,
        }
    }

    /// Packs into a GPU `vec4`: density, height_falloff, base_height, enabled (1/0).
    #[must_use]
    pub fn params_vec4(&self) -> [f32; 4] {
        [
            self.density.max(0.0),
            self.height_falloff.max(0.0),
            self.base_height,
            if self.enabled { 1.0 } else { 0.0 },
        ]
    }

    /// Packs fog colour into a GPU `vec4`; `a` = 1 for [`Self::sky_inscatter`].
    #[must_use]
    pub fn color_vec4(&self) -> [f32; 4] {
        [
            self.color[0],
            self.color[1],
            self.color[2],
            if self.sky_inscatter { 1.0 } else { 0.0 },
        ]
    }
}

/// Fog amount `1 − exp(−τ)` along a view ray of length `distance` ending at world height `world_y`.
///
/// `τ` is `distance · density · exp(−height_falloff · (world_y − base_height))`.
/// The return is that amount, clamped to `[0, 1]`, not `τ` itself.
#[must_use]
pub fn fog_factor(distance: f32, world_y: f32, fog: &FogSettings) -> f32 {
    if !fog.enabled || fog.density <= 0.0 || distance <= 0.0 {
        return 0.0;
    }
    let height_density = fog.density * (-fog.height_falloff * (world_y - fog.base_height)).exp();
    let optical = (distance.max(0.0) * height_density).max(0.0);
    1.0 - (-optical).exp()
}

/// Lerps `scene_rgb` toward fog colour by [`fog_factor`].
#[must_use]
pub fn apply_fog(scene_rgb: [f32; 3], distance: f32, world_y: f32, fog: &FogSettings) -> [f32; 3] {
    let t = fog_factor(distance, world_y, fog);
    [
        scene_rgb[0] + (fog.color[0] - scene_rgb[0]) * t,
        scene_rgb[1] + (fog.color[1] - scene_rgb[1]) * t,
        scene_rgb[2] + (fog.color[2] - scene_rgb[2]) * t,
    ]
}

// --- Froxel fog sun-shadow lookup (ADR-3122) ----------------------------------------------------
//
// CPU mirror of `shadow_at` and the scatter pass's sample placement in `volumetric_fog.wgsl`:
// which texel of the sun cascade a froxel sample reads, where it sits in the cascade's fade band,
// and the depth bias of the compare. `render.fog_shadow` reports it and
// `tests/fog_shadow_lookup.rs` checks it against the shader on lavapipe.
//
// The depth bias in shadow-map texels plus a slope term: "Common Techniques to Improve Shadow
// Depth Maps" (Microsoft Learn, Win32 docs); the receiver-plane reasoning behind the slope term:
// Isidoro, "Shadow Mapping: GPU-based Tips and Techniques" (GDC 2006). Froxel samples placed in
// the visible air in front of a surface: Wronski, "Volumetric Fog" (SIGGRAPH 2014 Advances).

/// How the froxel fog reads the sun shadow (`VolumetricFogSettings::sun_shadow`, ADR-3122).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FogSunShadowRule {
    /// The v0.2 rule, the default look: the compare's depth bias is a fixed
    /// [`FOG_LEGACY_SHADOW_BIAS_NDC`] share of the cascade's depth extent (about a metre along the
    /// sun on a 200 m view, several on a long one); a shadowed sample in the outer
    /// [`FOG_CASCADE_FADE`] of the cascade's square fades to lit; and a sample that lies beyond
    /// the surface it looks at is pulled back onto that surface. Air within the bias under a roof,
    /// samples on a roof's underside and shadowed air near the square's edge read the sun.
    #[default]
    Legacy,
    /// Calibrated against the path tracer (spec 27 § 2.2 fog rows): the bias is
    /// [`fog_sun_shadow_bias_m`], shadow-map texels of the cascade read plus a slope term; the
    /// compare decides everywhere inside the cascade's square (no fade to lit; outside it the
    /// fog has no shadow information and reads lit, as the forward pass does past its shadow
    /// distance); and a froxel whose slice holds a surface spreads its samples over the air in
    /// front of that surface ([`fog_sample_depth`]) instead of piling them onto it.
    Calibrated,
}

impl FogSunShadowRule {
    /// Every rule, in protocol order.
    pub const ALL: [Self; 2] = [Self::Legacy, Self::Calibrated];

    /// The protocol name: `legacy` or `calibrated`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Calibrated => "calibrated",
        }
    }

    /// The rule named `name` ([`Self::as_str`]).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == name)
    }

    /// The fog uniform's flag for this rule (`FogGpu::grid.w`): 0 legacy, 1 calibrated.
    #[must_use]
    pub const fn gpu_flag(self) -> f32 {
        match self {
            Self::Legacy => 0.0,
            Self::Calibrated => 1.0,
        }
    }
}

/// [`FogSunShadowRule::Legacy`]'s depth bias, in the cascade's NDC depth.
pub const FOG_LEGACY_SHADOW_BIAS_NDC: f32 = 0.002;

/// [`FogSunShadowRule::Legacy`]: share of the cascade's half-width over which the fog's sun shadow
/// fades to lit at the edge of the square. The cascade may keep a placement for many frames
/// (cascade anchoring), and the fade softens the step when it moves.
pub const FOG_CASCADE_FADE: f32 = 0.15;

/// [`fog_sun_shadow_bias_m`]: constant bias in texels of the cascade read. Half a texel keeps
/// a precision margin without lighting a whole texel of air under a flat roof (ADR-4199).
pub const FOG_SHADOW_BIAS_TEXELS: f32 = 0.5;

/// [`fog_sun_shadow_bias_m`]: texels per unit slope of level ground seen from the sun. A sample
/// on lit ground sits up to half a texel from the texel centre, which is that slope times half a
/// texel deeper or shallower along the sun.
pub const FOG_SHADOW_GROUND_SLOPE_TEXELS: f32 = 0.5;

/// [`fog_sun_shadow_bias_m`]: the ground slope is capped here (a sun about 5.7° high).
pub const FOG_SHADOW_MAX_SLOPE: f32 = 10.0;

/// The sun cascade the fog reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FogSunCascade {
    /// World to shadow clip (orthographic, depth `0..1`).
    pub light_view_proj: [[f32; 4]; 4],
    /// Shadow-map size (texels on a side).
    pub resolution: u32,
    /// World size of one shadow texel (metres).
    pub texel_m: f32,
    /// The orthographic depth extent (metres): NDC depth 1 is this far along the sun from 0.
    pub depth_m: f32,
}

impl FogSunCascade {
    /// The cascade drawn with `light_view_proj` at `resolution`² texels. An orthographic matrix's
    /// x row has length `1 / half-width` and its z row `1 / depth extent`.
    #[must_use]
    pub fn new(light_view_proj: [[f32; 4]; 4], resolution: u32) -> Self {
        let m = light_view_proj;
        let row = |r: usize| (m[0][r] * m[0][r] + m[1][r] * m[1][r] + m[2][r] * m[2][r]).sqrt();
        let (x, z) = (row(0), row(2));
        let resolution = resolution.max(1);
        Self {
            light_view_proj,
            resolution,
            texel_m: if x > 0.0 {
                2.0 / (x * resolution as f32)
            } else {
                0.0
            },
            depth_m: if z > 0.0 { 1.0 / z } else { 0.0 },
        }
    }

    /// `(ndc.x, ndc.y, ndc.z)` of `world` in this cascade.
    #[must_use]
    pub fn ndc(&self, world: [f32; 3]) -> [f32; 3] {
        let m = self.light_view_proj;
        let clip = |r: usize| m[0][r] * world[0] + m[1][r] * world[1] + m[2][r] * world[2] + m[3][r];
        let w = clip(3).max(1.0e-5);
        [clip(0) / w, clip(1) / w, clip(2) / w]
    }

    /// The compare's depth bias in NDC depth under `rule` for a sun traveling along `sun_travel`.
    #[must_use]
    pub fn bias_ndc(&self, rule: FogSunShadowRule, sun_travel: [f32; 3]) -> f32 {
        match rule {
            FogSunShadowRule::Legacy => FOG_LEGACY_SHADOW_BIAS_NDC,
            FogSunShadowRule::Calibrated if self.depth_m > 0.0 => {
                fog_sun_shadow_bias_m(self.texel_m, sun_travel) / self.depth_m
            }
            FogSunShadowRule::Calibrated => 0.0,
        }
    }

    /// Where `world` falls in this cascade and what the compare there reads.
    #[must_use]
    pub fn sample(
        &self,
        rule: FogSunShadowRule,
        sun_travel: [f32; 3],
        world: [f32; 3],
    ) -> FogSunShadowSample {
        let [x, y, z] = self.ndc(world);
        let (u, v) = (x * 0.5 + 0.5, 0.5 - y * 0.5);
        let inside = (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v) && (0.0..=1.0).contains(&z);
        let edge = x.abs().max(y.abs());
        let fade = match rule {
            FogSunShadowRule::Legacy => fog_cascade_fade(edge),
            FogSunShadowRule::Calibrated => 0.0,
        };
        let size = self.resolution.max(1);
        let texel = inside.then(|| {
            let t = |c: f32| ((c * size as f32) as u32).min(size - 1);
            [t(u), t(v)]
        });
        let state = if !inside {
            FogCascadeState::Outside
        } else if fade > 0.0 {
            FogCascadeState::Faded
        } else {
            FogCascadeState::Covered
        };
        FogSunShadowSample {
            state,
            edge,
            fade,
            texel,
            ndc_z: z,
            bias_ndc: self.bias_ndc(rule, sun_travel),
        }
    }
}

/// [`FogSunShadowRule::Calibrated`]'s depth bias (metres along the sun) on a cascade of `texel_m`
/// texels: [`FOG_SHADOW_BIAS_TEXELS`] plus [`FOG_SHADOW_GROUND_SLOPE_TEXELS`] times the slope of
/// level ground seen from the sun (capped at [`FOG_SHADOW_MAX_SLOPE`]). Air has no normal; level
/// ground is the surface the froxel samples nearest a surface sit over most.
#[must_use]
pub fn fog_sun_shadow_bias_m(texel_m: f32, sun_travel: [f32; 3]) -> f32 {
    let [x, y, z] = sun_travel;
    let len = (x * x + y * y + z * z).sqrt();
    let up = if len > 0.0 { (y / len).abs() } else { 1.0 };
    let slope = ((1.0 - up * up).max(0.0).sqrt() / up.max(1.0e-3)).min(FOG_SHADOW_MAX_SLOPE);
    texel_m.max(0.0) * FOG_SHADOW_GROUND_SLOPE_TEXELS.mul_add(slope, FOG_SHADOW_BIAS_TEXELS)
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * 2.0f32.mul_add(-t, 3.0)
}

/// The fade at `edge = max(|ndc.x|, |ndc.y|)`: `smoothstep(1 − FOG_CASCADE_FADE, 1, edge)`, 0 inside
/// the band and 1 at the square's edge. A shadowed sample reads this much sun.
#[must_use]
pub fn fog_cascade_fade(edge: f32) -> f32 {
    smoothstep(1.0 - FOG_CASCADE_FADE, 1.0, edge)
}

/// Where a fog sample sits in the cascade the fog reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FogCascadeState {
    /// Inside the square, short of the fade band: the compare alone decides.
    Covered,
    /// In the outer [`FOG_CASCADE_FADE`] of the square under [`FogSunShadowRule::Legacy`]: a
    /// shadowed sample reads [`FogSunShadowSample::fade`] of the sun.
    Faded,
    /// Outside the square or its depth range: the sample reads the sun unshadowed.
    Outside,
}

impl FogCascadeState {
    /// The protocol name: `covered`, `faded` or `outside`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Covered => "covered",
            Self::Faded => "faded",
            Self::Outside => "outside",
        }
    }
}

/// One fog sample's sun-shadow lookup ([`FogSunCascade::sample`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FogSunShadowSample {
    /// Where the sample sits in the cascade.
    pub state: FogCascadeState,
    /// `max(|ndc.x|, |ndc.y|)`.
    pub edge: f32,
    /// The sun a shadowed sample still reads: [`fog_cascade_fade`] of [`Self::edge`] under
    /// [`FogSunShadowRule::Legacy`], 0 under [`FogSunShadowRule::Calibrated`].
    pub fade: f32,
    /// Shadow-map texel `(x, y)` the compare reads (`None` outside).
    pub texel: Option<[u32; 2]>,
    /// The sample's NDC depth in the cascade.
    pub ndc_z: f32,
    /// The compare's bias in NDC depth.
    pub bias_ndc: f32,
}

impl FogSunShadowSample {
    /// Whether the compare finds a caster: the sample is deeper than `stored + bias`.
    #[must_use]
    pub fn occluded(&self, stored: f32) -> bool {
        self.texel.is_some() && self.ndc_z > stored + self.bias_ndc
    }

    /// The sun visibility `shadow_at` returns given the shadow map's stored depth at
    /// [`Self::texel`]: 1 outside or unoccluded, [`Self::fade`] when occluded.
    #[must_use]
    pub fn visibility(&self, stored: f32) -> f32 {
        if self.occluded(stored) { self.fade } else { 1.0 }
    }
}

/// Distance along a froxel sub-ray at which the scatter pass samples the air (`fs_scatter`):
/// sub-sample `i` of 4 sits at `mid + len (fract(0.375 + 0.618034 i) − 0.5) / 2` in the slice
/// `[start, start + len]` (centre `mid`). Under [`FogSunShadowRule::Legacy`] `surface` is the
/// farthest surface the froxel's four sub-rays meet and the sample is clamped to it: in a slice
/// that holds a surface, the samples beyond it pile onto it (or lie behind another sub-ray's
/// nearer surface). Under [`FogSunShadowRule::Calibrated`] `surface` is this sub-ray's own
/// surface, and when it lies inside the slice the sample's place in the slice is scaled onto the
/// air in front of it, `[start, surface]`, so every sample lies in visible air; a sub-ray whose
/// surface is in front of the slice sees no air in it and gives `None` (the froxel averages the
/// sub-rays that see air, and holds no in-scatter when none does).
#[must_use]
pub fn fog_sample_depth(
    rule: FogSunShadowRule,
    start: f32,
    mid: f32,
    len: f32,
    i: u32,
    surface: f32,
) -> Option<f32> {
    let jitter = ((0.618_034f32 * i as f32 + 0.375).fract() - 0.5) * 0.5;
    let depth = len.mul_add(jitter, mid);
    match rule {
        FogSunShadowRule::Legacy => Some(depth.min(surface)),
        FogSunShadowRule::Calibrated if surface <= start => None,
        FogSunShadowRule::Calibrated if surface >= start + len => Some(depth.min(surface)),
        FogSunShadowRule::Calibrated => {
            let share = (surface - start) / len.max(1.0e-6);
            Some(share.mul_add(depth - start, start))
        }
    }
}

/// The weight of a froxel sub-ray's sample in the froxel (`fs_scatter`): 1 under
/// [`FogSunShadowRule::Legacy`] (a plain average of the four); under
/// [`FogSunShadowRule::Calibrated`] the share of the slice `[start, start + len]` in front of the
/// sub-ray's own `surface`, so a sub-ray that sees a thin layer of air in front of a surface does
/// not stand for the whole slice (pixels behind it read the slice whole), and one whose surface is
/// in front of the slice weighs 0. A froxel whose weights are all 0 holds no in-scatter.
#[must_use]
pub fn fog_sample_weight(rule: FogSunShadowRule, start: f32, len: f32, surface: f32) -> f32 {
    match rule {
        FogSunShadowRule::Legacy => 1.0,
        FogSunShadowRule::Calibrated => ((surface - start) / len.max(1.0e-6)).clamp(0.0, 1.0),
    }
}

/// What the last froxel fog pass read for the sun shadow ([`FogSunShadowProbe`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FogSunShadowFrame {
    /// The rule it ran.
    pub rule: FogSunShadowRule,
    /// Index of the cascade it read among the frame's sun cascades (0 = finest), when known.
    pub cascade_index: Option<usize>,
    /// Sun cascades the frame drew.
    pub cascade_count: usize,
    /// The cascade it read; `None` when the fog ran without sun shadows (every sample lit).
    pub cascade: Option<FogSunCascade>,
    /// The sun's travel direction.
    pub sun_travel: [f32; 3],
}

impl FogSunShadowFrame {
    /// The compare's bias in NDC depth (0 without a cascade).
    #[must_use]
    pub fn bias_ndc(&self) -> f32 {
        self.cascade
            .map_or(0.0, |c| c.bias_ndc(self.rule, self.sun_travel))
    }

    /// The compare's bias in metres along the sun (0 without a cascade).
    #[must_use]
    pub fn bias_m(&self) -> f32 {
        self.cascade.map_or(0.0, |c| self.bias_ndc() * c.depth_m)
    }

    /// The lookup at `world`; `None` without a cascade (lit everywhere).
    #[must_use]
    pub fn sample(&self, world: [f32; 3]) -> Option<FogSunShadowSample> {
        self.cascade.map(|c| c.sample(self.rule, self.sun_travel, world))
    }
}

/// A slot the froxel fog writes what it read for the sun shadow into, once per drawn fog pass
/// (`OutdoorParams::fog_shadow_probe`). Clones share the slot. As a resource (the render plugin
/// inserts one) the runtime shares it with the renderer and `render.fog_shadow` reads it. Its `Debug` prints no contents,
/// so a filled probe does not change the outdoor parameters' key.
#[derive(Resource, Clone, Default)]
pub struct FogSunShadowProbe(Arc<Mutex<Option<FogSunShadowFrame>>>);

impl FogSunShadowProbe {
    /// The last fog pass's sun-shadow read, `None` before the first.
    #[must_use]
    pub fn last(&self) -> Option<FogSunShadowFrame> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stores a fog pass's read.
    pub fn publish(&self, frame: FogSunShadowFrame) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(frame);
    }
}

impl std::fmt::Debug for FogSunShadowProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FogSunShadowProbe")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_fog_is_identity() {
        let fog = FogSettings::default();
        assert_eq!(fog_factor(100.0, 10.0, &fog), 0.0);
        let c = apply_fog([0.2, 0.3, 0.4], 100.0, 10.0, &fog);
        assert_eq!(c, [0.2, 0.3, 0.4]);
    }

    #[test]
    fn distance_increases_fog() {
        let fog = FogSettings::outdoor_haze();
        let near = fog_factor(20.0, 5.0, &fog);
        let far = fog_factor(200.0, 5.0, &fog);
        assert!(far > near + 0.1, "near={near} far={far}");
        assert!(far <= 1.0);
    }

    #[test]
    fn height_reduces_fog() {
        let fog = FogSettings::outdoor_haze();
        let valley = fog_factor(120.0, 2.0, &fog);
        let peak = fog_factor(120.0, 80.0, &fog);
        assert!(valley > peak + 0.05, "valley={valley} peak={peak}");
    }

    #[test]
    fn apply_moves_toward_fog_color() {
        let fog = FogSettings::outdoor_haze();
        let out = apply_fog([0.0, 0.0, 0.0], 250.0, 5.0, &fog);
        assert!(out[0] > 0.1 && out[2] > out[0], "got {out:?}");
    }

    /// At the base height with no falloff, optical depth is `distance * density` and the
    /// return is `1 - exp(-τ)`, not `τ`.
    #[test]
    fn fog_factor_is_the_amount_not_the_optical_depth() {
        let fog = FogSettings {
            enabled: true,
            density: 0.01,
            height_falloff: 0.0,
            base_height: 0.0,
            ..FogSettings::default()
        };
        let amount = fog_factor(100.0, 0.0, &fog);
        let expect = 1.0 - (-1.0f32).exp();
        assert!((amount - expect).abs() < 1.0e-5, "{amount} vs {expect}");
        assert!((amount - 1.0).abs() > 0.1, "the return is not the optical depth");
    }
}
