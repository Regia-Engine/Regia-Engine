// SPDX-License-Identifier: MPL-2.0
//! Ground variation and late-winter surfaces on the photoreal terrain path (ADR-1408).
//!
//! [`TerrainGroundLook`] adds what a tiling layer set cannot carry on its own, set with
//! [`crate::gpu::TerrainGpu::set_ground_look`] (or [`crate::TerrainSharedDraw::set_ground_look`]).
//! Every term is off in [`TerrainGroundLook::OFF`], the default, so a host that never sets it
//! renders the same pixels.
//!
//! - **Variation** (anti-tiling). Without a baked macro map nothing varies between the
//!   texture repeat (1–3 m) and the layer bands (hundreds of metres), so a field reads as one
//!   carpet even with the stochastic lattice hiding the repeat itself. Three octaves of world
//!   value noise (about 0.12, 0.4 and 1 × `variation_scale_m`) move brightness and a dry/green
//!   tint, and a second tap of each layer at a different scale and rotation ("remix", the
//!   multi-scale blend of Ulrich 2015 / Golus' detail-scale mixing, in the spirit of Heitz &
//!   Neyret 2018) is mixed in by a noise mask near the camera, so neighbouring repeats differ in
//!   their mid-scale structure too.
//! - **Thin patchy snow** that survives on slopes facing away from the sun and above a snow
//!   line, broken by noise into patches, with a darker wet melting fringe (spring snow albedo
//!   about 0.7: Warren & Wiscombe 1980, Warren 1982).
//! - **Wet mud**: dark (soil-coloured) ground and the melt fringe get darker and glossier
//!   (Lekner & Dorf 1988: a water film darkens a rough surface by total internal reflection and
//!   adds a specular sheen).
//! - **Hoarfrost**: a pale, faintly blue film where the geometric normal's Y goes from 0.6 to
//!   0.95 and the sun does not reach. Steep faces stay bare. Shadowed ground still roughens
//!   toward 0.95, including faces the film does not cover.
//!
//! Ground contact under scatter is the scatter contact map (`scatter/contact.rs`); hosts that
//! place scatter as their own entities feed it through
//! [`crate::ContactRaster::from_footprints`].

use regia_app::prelude::*;

/// Ground variation and late-winter terms (see the module docs). [`Self::OFF`] is the default.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerrainGroundLook {
    /// Strength of the world-space brightness and tint variation, `0..=2` (0 = off).
    pub variation: f32,
    /// Size of the largest variation patch, metres (at least 4).
    pub variation_scale_m: f32,
    /// Share of the second-scale layer tap mixed in near the camera, `0..=1` (0 = off).
    pub remix: f32,
    /// Scale of the second tap relative to the layer's tiling, `0.1..=0.9` (0.43: the
    /// texture's features 2.3× larger, rotated).
    pub remix_scale: f32,
    /// Amount of thin patchy snow, `0..=1` (0 = none).
    pub snow: f32,
    /// Altitude (metres) above which the snow reaches its full amount; below it the snow keeps
    /// only to shaded slopes.
    pub snow_line_m: f32,
    /// How much slopes facing away from the sun keep their snow, `0..=1`.
    pub snow_aspect: f32,
    /// Width of the darker, wet melting fringe around each patch, `0..=1`.
    pub melt_edge: f32,
    /// Size of the snow patches, metres.
    pub snow_patch_m: f32,
    /// Darkening and sheen of wet mud on dark, flat ground, `0..=1` (0 = off).
    pub mud_wetness: f32,
    /// Hoarfrost on ground the sun does not reach, `0..=1` (0 = off).
    pub hoarfrost: f32,
}

impl TerrainGroundLook {
    /// Every term off: the terrain as before ADR-1408.
    pub const OFF: Self = Self {
        variation: 0.0,
        variation_scale_m: 40.0,
        remix: 0.0,
        remix_scale: 0.43,
        snow: 0.0,
        snow_line_m: 250.0,
        snow_aspect: 0.7,
        melt_edge: 0.5,
        snow_patch_m: 9.0,
        mud_wetness: 0.0,
        hoarfrost: 0.0,
    };

    /// A cold, damp late-winter upland morning: ground variation and remix on, thin snow
    /// patches on shaded slopes and high ground with wet fringes, wet mud, morning frost in
    /// the shadows.
    pub const LATE_WINTER: Self = Self {
        variation: 1.0,
        variation_scale_m: 40.0,
        remix: 0.5,
        remix_scale: 0.43,
        snow: 0.6,
        snow_line_m: 260.0,
        snow_aspect: 0.75,
        melt_edge: 0.6,
        snow_patch_m: 9.0,
        mud_wetness: 0.6,
        hoarfrost: 0.5,
    };

    /// Only the anti-tiling terms (variation and remix), for any landscape.
    pub const VARIED: Self = Self {
        variation: 1.0,
        remix: 0.5,
        ..Self::OFF
    };

    /// The named look (`off`, `varied`, `late_winter`), or `None`.
    #[must_use]
    pub fn preset(name: &str) -> Option<Self> {
        match name {
            "off" => Some(Self::OFF),
            "varied" => Some(Self::VARIED),
            "late_winter" => Some(Self::LATE_WINTER),
            _ => None,
        }
    }

    /// The names [`Self::preset`] accepts.
    pub const PRESETS: [&'static str; 3] = ["off", "varied", "late_winter"];

    /// Every value finite and in range (a non-finite value takes [`Self::OFF`]'s).
    #[must_use]
    pub fn sanitized(self) -> Self {
        let off = Self::OFF;
        let pick = |v: f32, fallback: f32| if v.is_finite() { v } else { fallback };
        Self {
            variation: pick(self.variation, 0.0).clamp(0.0, 2.0),
            variation_scale_m: pick(self.variation_scale_m, off.variation_scale_m).max(4.0),
            remix: pick(self.remix, 0.0).clamp(0.0, 1.0),
            remix_scale: pick(self.remix_scale, off.remix_scale).clamp(0.1, 0.9),
            snow: pick(self.snow, 0.0).clamp(0.0, 1.0),
            snow_line_m: pick(self.snow_line_m, off.snow_line_m),
            snow_aspect: pick(self.snow_aspect, off.snow_aspect).clamp(0.0, 1.0),
            melt_edge: pick(self.melt_edge, off.melt_edge).clamp(0.0, 1.0),
            snow_patch_m: pick(self.snow_patch_m, off.snow_patch_m).max(0.5),
            mud_wetness: pick(self.mud_wetness, 0.0).clamp(0.0, 1.0),
            hoarfrost: pick(self.hoarfrost, 0.0).clamp(0.0, 1.0),
        }
    }

    /// Whether any term is on.
    #[must_use]
    pub fn is_on(&self) -> bool {
        let s = self.sanitized();
        s.variation > 0.0 || s.remix > 0.0 || s.snow > 0.0 || s.mud_wetness > 0.0 || s.hoarfrost > 0.0
    }

    /// The three material-uniform rows `terrain.wgsl` reads: `ground` (variation, variation
    /// scale, remix, remix scale), `winter` (snow, snow line, aspect, melt edge) and `winter2`
    /// (patch size, mud wetness, hoarfrost, 0). All zero when every term is off.
    #[must_use]
    pub fn uniforms(&self) -> [[f32; 4]; 3] {
        if !self.is_on() {
            return [[0.0; 4]; 3];
        }
        let s = self.sanitized();
        [
            [s.variation, s.variation_scale_m, s.remix, s.remix_scale],
            [s.snow, s.snow_line_m, s.snow_aspect, s.melt_edge],
            [s.snow_patch_m, s.mud_wetness, s.hoarfrost, 0.0],
        ]
    }
}

impl Default for TerrainGroundLook {
    fn default() -> Self {
        Self::OFF
    }
}

/// A shared, generation-counted [`TerrainGroundLook`]: the shared terrain draw applies a new
/// value at its next prepare, so a host (or the agent, through [`TerrainGroundLookControl`])
/// changes the look after the draw has gone into the renderer.
#[derive(Debug, Clone, Default)]
pub struct GroundLookHandle(
    std::sync::Arc<std::sync::Mutex<(u64, TerrainGroundLook)>>,
    std::sync::Arc<std::sync::Mutex<Vec<crate::LayerAlbedo>>>,
    // The draw's `FillCanyonScope` (ADR-8412), as `FillCanyonScope::to_u8`.
    std::sync::Arc<std::sync::atomic::AtomicU8>,
);

impl GroundLookHandle {
    /// Stores the draw's layer albedo report (`TerrainSharedDraw::set_material` writes it,
    /// ADR-1425), so the agent's `terrain.get_ground_look` can show it.
    pub fn set_layer_albedo(&self, report: Vec<crate::LayerAlbedo>) {
        *self.1.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = report;
    }

    /// The draw's layer albedo report (empty until a material with textures is set).
    #[must_use]
    pub fn layer_albedo(&self) -> Vec<crate::LayerAlbedo> {
        self.1
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Stores `look` (sanitized) and bumps the generation.
    pub fn set(&self, look: TerrainGroundLook) {
        let mut guard = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.0 = guard.0.wrapping_add(1);
        guard.1 = look.sanitized();
    }

    /// The look and its generation (0 until the first [`Self::set`]).
    #[must_use]
    pub fn get(&self) -> (u64, TerrainGroundLook) {
        *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Stores the draw's planet-shine fill scope (ADR-8412); the draw takes it at its next
    /// prepare (`TerrainSharedDraw::set_fill_canyon_scope`).
    pub fn set_fill_canyon_scope(&self, scope: crate::FillCanyonScope) {
        self.2.store(scope.to_u8(), std::sync::atomic::Ordering::Relaxed);
    }

    /// The draw's planet-shine fill scope ([`crate::FillCanyonScope::Off`] until set).
    #[must_use]
    pub fn fill_canyon_scope(&self) -> crate::FillCanyonScope {
        crate::FillCanyonScope::from_u8(self.2.load(std::sync::atomic::Ordering::Relaxed))
    }
}

/// The ECS resource a host inserts with its terrain draw's [`GroundLookHandle`]
/// (`TerrainSharedDraw::ground_look_handle`), so agent `terrain.get_ground_look` /
/// `terrain.set_ground_look` reach the draw (book `agents/terrain-late-winter.md`).
#[derive(Resource, Debug, Clone, Default)]
pub struct TerrainGroundLookControl(pub GroundLookHandle);

/// How strongly a surface with normal `n` (unit) faces away from a sun along `to_sun` (unit),
/// `−1..1` times the slope's steepness (0 on flat ground): the aspect term of the snow cover.
/// Mirrors `shade_facing` in `terrain.wgsl`.
#[must_use]
pub fn shade_facing(n: [f32; 3], to_sun: [f32; 3]) -> f32 {
    let horiz = (to_sun[0] * to_sun[0] + to_sun[2] * to_sun[2]).sqrt();
    if horiz < 1.0e-4 {
        return 0.0;
    }
    -(n[0] * to_sun[0] + n[2] * to_sun[2]) / horiz
}

/// The snow amount before the patch noise (`0..`): the snow line ramp over 80 m plus the
/// aspect term, shed off steep faces. Mirrors `snow_amount` in `terrain.wgsl`.
#[must_use]
pub fn snow_amount(look: &TerrainGroundLook, altitude_m: f32, n: [f32; 3], to_sun: [f32; 3]) -> f32 {
    let s = look.sanitized();
    if s.snow <= 0.0 {
        return 0.0;
    }
    let line = ((altitude_m - s.snow_line_m) / 80.0 + 0.5).clamp(0.0, 1.0);
    let aspect = s.snow_aspect * shade_facing(n, to_sun) * 2.0;
    let shed = ((n[1] - 0.55) / 0.25).clamp(0.0, 1.0);
    (s.snow * (0.25 + 0.75 * line + aspect) * shed).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_is_the_default_and_packs_zeros() {
        assert_eq!(TerrainGroundLook::default(), TerrainGroundLook::OFF);
        assert!(!TerrainGroundLook::OFF.is_on());
        assert_eq!(TerrainGroundLook::OFF.uniforms(), [[0.0; 4]; 3]);
        assert!(TerrainGroundLook::LATE_WINTER.is_on());
        assert!(TerrainGroundLook::VARIED.is_on());
        let rows = TerrainGroundLook::LATE_WINTER.uniforms();
        assert_eq!(rows[0][0], 1.0);
        assert_eq!(rows[1][0], 0.6);
        assert_eq!(rows[2][2], 0.5);
    }

    #[test]
    fn presets_resolve_and_are_in_range() {
        for name in TerrainGroundLook::PRESETS {
            let look = TerrainGroundLook::preset(name).expect("preset");
            assert_eq!(look, look.sanitized(), "{name}");
        }
        assert!(TerrainGroundLook::preset("summer").is_none());
    }

    #[test]
    fn sanitize_clamps_and_replaces_non_finite() {
        let wild = TerrainGroundLook {
            variation: 9.0,
            variation_scale_m: 0.0,
            remix: f32::NAN,
            remix_scale: 5.0,
            snow: -1.0,
            snow_line_m: f32::INFINITY,
            snow_aspect: 2.0,
            melt_edge: -3.0,
            snow_patch_m: 0.0,
            mud_wetness: 4.0,
            hoarfrost: f32::NAN,
        }
        .sanitized();
        assert_eq!(wild.variation, 2.0);
        assert_eq!(wild.variation_scale_m, 4.0);
        assert_eq!(wild.remix, 0.0);
        assert_eq!(wild.remix_scale, 0.9);
        assert_eq!(wild.snow, 0.0);
        assert_eq!(wild.snow_line_m, TerrainGroundLook::OFF.snow_line_m);
        assert_eq!(wild.snow_aspect, 1.0);
        assert_eq!(wild.melt_edge, 0.0);
        assert_eq!(wild.snow_patch_m, 0.5);
        assert_eq!(wild.mud_wetness, 1.0);
        assert_eq!(wild.hoarfrost, 0.0);
    }

    #[test]
    fn handle_counts_generations_and_sanitizes() {
        let handle = GroundLookHandle::default();
        assert_eq!(handle.get(), (0, TerrainGroundLook::OFF));
        handle.set(TerrainGroundLook::LATE_WINTER);
        let clone = handle.clone();
        clone.set(TerrainGroundLook {
            variation: 7.0,
            ..TerrainGroundLook::VARIED
        });
        let (generation, look) = handle.get();
        assert_eq!(generation, 2);
        assert_eq!(look.variation, 2.0);
    }

    /// Snow keeps to slopes facing away from the sun: with the sun low in the south-east, a
    /// north-west-facing slope holds more than flat ground, which holds more than a sunny
    /// slope; high ground holds more than low; a cliff sheds it.
    #[test]
    fn snow_keeps_to_shaded_slopes_and_high_ground() {
        let look = TerrainGroundLook::LATE_WINTER;
        let to_sun = {
            let v = [0.79_f32, 0.36, -0.49];
            let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            [v[0] / l, v[1] / l, v[2] / l]
        };
        let tilt = |x: f32, z: f32| {
            let l = (x * x + 1.0 + z * z).sqrt();
            [x / l, 1.0 / l, z / l]
        };
        let shaded = tilt(-0.3, 0.2);
        let sunny = tilt(0.3, -0.2);
        let flat = [0.0, 1.0, 0.0];
        assert!(shade_facing(shaded, to_sun) > 0.0);
        assert!(shade_facing(sunny, to_sun) < 0.0);
        assert_eq!(shade_facing(flat, to_sun), 0.0);
        let at = |alt: f32, n: [f32; 3]| snow_amount(&look, alt, n, to_sun);
        assert!(at(120.0, shaded) > at(120.0, flat));
        assert!(at(120.0, flat) > at(120.0, sunny));
        assert!(at(400.0, flat) > at(120.0, flat));
        assert_eq!(at(400.0, tilt(2.0, 0.0)), 0.0, "a 63° face sheds its snow");
        assert_eq!(snow_amount(&TerrainGroundLook::OFF, 400.0, flat, to_sun), 0.0);
        // Overhead sun: no aspect preference.
        assert_eq!(shade_facing(shaded, [0.0, 1.0, 0.0]), 0.0);
    }
}

/// What the terrain did with the surface weather (spec 99 § 4.5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerrainSurfaceWeatherReport {
    /// The shared colour pass runs the variant with the surface-weather code.
    pub variant_on: bool,
    /// Frames shaded with it.
    pub frames_wet: u64,
}
