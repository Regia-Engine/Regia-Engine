// SPDX-License-Identifier: MPL-2.0
//! The path tracer's cloud layer, for validating the real-time cloud shadow map and the overcast
//! sky light (ADR-320, ADR-321; ADR-1834).
//!
//! The real-time renderer draws clouds as a raymarched noise field and approximates their
//! effect on the ground in two places: the sun's transmittance through the layer (a Beer–Lambert
//! march, ADR-320) and the dimming and greying of the sky light under a covered sky (ADR-321).
//! Neither had a reference. The tracer already scatters light in horizontal slabs of medium
//! ([`PtMediumLayer`]: free-flight sampling, next-event estimation through the slab, any number
//! of bounces), so a cloud layer here is such a slab: a homogeneous, strongly forward-scattering,
//! non-absorbing medium between the layer's base and top, with the extinction the renderer's own
//! parameters imply ([`PtCloudLayer::from_settings`]).
//!
//! A slab is horizontally uniform, so it models the fully covered column. A sky with gaps is
//! validated as the coverage-weighted mix of the clear and the covered traces
//! ([`coverage_mix`]); the mix is exact for the sun term (a column is either lit or not) and a
//! first-order estimate for the sky term.
//!
//! - [`PtCloudLayer`]: the slab, its tracer layer and its Beer–Lambert references.
//! - [`renderer_ground_ratio`]: what the real-time model says the ground gets under the layer.
//! - [`measure_ground_ratio`]: what the tracer says (a lit plane under the layer).
//! - [`two_stream_diffuse_transmittance`]: the closed-form diffuse transmission of a thick
//!   conservative slab, a check that needs no trace.
//!
//! References: Beer–Lambert transmittance; Henyey & Greenstein, "Diffuse radiation in the
//! Galaxy", Astrophysical Journal 93, 1941 (the phase function); Meador & Weaver, "Two-stream
//! approximations to radiative transfer in planetary atmospheres", J. Atmos. Sci. 37, 1980 (the
//! diffuse transmission of a conservatively scattering layer).

use regia_render::{
    CLOUD_OVERCAST_DIM, CloudSettings, WeatherMap, cloud_overcast_factor, cloud_shadow_march, type_gradient,
};

use crate::scene::{PtMediumLayer, PtPhase, PtScene, PtSky};
use crate::scenes;
use crate::{PathTracer, PtError, PtMaterial, PtSettings};

/// Henyey–Greenstein asymmetry of cloud droplets in the visible: strongly forward scattering
/// (Mie scattering by water drops of a few micrometres gives 0.8 to 0.9).
pub const CLOUD_ASYMMETRY: f32 = 0.85;

/// Single-scattering albedo of a cloud: water does not absorb visible light, so cloud
/// droplets scatter all of what they extinguish.
pub const CLOUD_ALBEDO: f32 = 1.0;

/// Radius of the planet the shadow-map reference march uses, metres (Earth's mean radius; the
/// runtime reads the active planet's).
pub const REFERENCE_PLANET_RADIUS_M: f32 = 6_371_000.0;

/// A homogeneous cloud slab: the tracer's version of one cloud layer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PtCloudLayer {
    /// World Y of the layer's base, metres.
    pub base_m: f32,
    /// World Y of the layer's top, metres.
    pub top_m: f32,
    /// Extinction coefficient inside the slab, 1/m.
    pub extinction: f32,
    /// Single-scattering albedo, 0..1 ([`CLOUD_ALBEDO`]).
    pub albedo: f32,
    /// Henyey–Greenstein asymmetry, -1..1 ([`CLOUD_ASYMMETRY`]).
    pub asymmetry: f32,
}

impl PtCloudLayer {
    /// A fully covered slab with the given vertical optical depth.
    #[must_use]
    pub fn overcast(base_m: f32, top_m: f32, optical_depth: f32) -> Self {
        let thickness = (top_m - base_m).max(1.0);
        Self {
            base_m,
            top_m: base_m + thickness,
            extinction: optical_depth.max(0.0) / thickness,
            albedo: CLOUD_ALBEDO,
            asymmetry: CLOUD_ASYMMETRY,
        }
    }

    /// The fully covered column the renderer's settings describe: the layer between
    /// `altitude_bottom` and `altitude_top` with the optical depth of a column under full
    /// weather, `density · thickness · mean height profile` (the same `τ` ADR-321 uses), scaled
    /// by `fill`, the share of the column the noise actually fills. The runtime assumes
    /// [`regia_render::COLUMN_FILL`]; sweeping `fill` against the tracer is how that constant is checked.
    #[must_use]
    pub fn from_settings(settings: &CloudSettings, fill: f32) -> Self {
        let thickness = (settings.altitude_top - settings.altitude_bottom).max(1.0);
        // The renderer's own 8-step average of the height profile (`cloud_overcast_factor`).
        let steps = 8;
        let profile: f32 = (0..steps)
            .map(|i| type_gradient((i as f32 + 0.5) / steps as f32, settings.cloud_type))
            .sum::<f32>()
            / steps as f32;
        let tau = settings.density.max(0.0) * thickness * profile * fill.max(0.0);
        Self::overcast(
            settings.altitude_bottom,
            settings.altitude_bottom + thickness,
            tau,
        )
    }

    /// Thickness of the slab, metres.
    #[must_use]
    pub fn thickness_m(&self) -> f32 {
        (self.top_m - self.base_m).max(0.0)
    }

    /// Optical depth of the slab straight up.
    #[must_use]
    pub fn optical_depth(&self) -> f32 {
        self.extinction * self.thickness_m()
    }

    /// The tracer layer: scattering `albedo · σ`, absorption `(1 − albedo) · σ`, constant
    /// density between base and top, Henyey–Greenstein phase. Not `aerial`: the sun and the sky
    /// both see the slab.
    #[must_use]
    pub fn medium_layer(&self) -> PtMediumLayer {
        let albedo = self.albedo.clamp(0.0, 1.0);
        let sigma = self.extinction.max(0.0);
        PtMediumLayer {
            scattering: [sigma * albedo; 3],
            absorption: [sigma * (1.0 - albedo); 3],
            reference_height: self.base_m,
            falloff: 0.0,
            min_height: self.base_m,
            max_height: self.top_m,
            phase: PtPhase::HenyeyGreenstein([self.asymmetry.clamp(-0.99, 0.99); 3]),
            aerial: false,
        }
    }

    /// Adds the slab to the scene's media.
    ///
    /// # Errors
    /// [`PtError::Scene`] when the scene already holds the tracer's limit of eight layers.
    pub fn add_to(&self, scene: &mut PtScene) -> Result<(), PtError> {
        if scene.media.layers.len() >= 8 {
            return Err(PtError::Scene("a scene holds at most 8 medium layers".into()));
        }
        scene.media.layers.push(self.medium_layer());
        Ok(())
    }

    /// Fraction of the sun's beam that crosses the flat slab unscattered, `exp(−τ / sin(elev))`
    /// for a sun at `sun_y` (the sine of its elevation). 1 for a sun at or below the horizon
    /// plane's grazing limit the renderer also ignores ([`regia_render::CLOUD_SHADOW_MIN_SUN_Y`]).
    #[must_use]
    pub fn beam_transmittance(&self, sun_y: f32) -> f32 {
        if sun_y.is_nan() || sun_y < regia_render::CLOUD_SHADOW_MIN_SUN_Y {
            return 1.0;
        }
        (-self.optical_depth() / sun_y).exp()
    }
}

/// The diffuse (all scattered plus unscattered, isotropic incidence) transmittance of a
/// conservatively scattering slab of optical depth `tau` and asymmetry `g`, from the two-stream
/// approximation: `1 / (1 + 3/4 · (1 − g) · tau)` (Meador & Weaver 1980). The slab is thin when
/// `tau (1 − g)` is well under 1, where the formula tends to 1.
#[must_use]
pub fn two_stream_diffuse_transmittance(tau: f32, g: f32) -> f32 {
    1.0 / (1.0 + 0.75 * (1.0 - g.clamp(-1.0, 1.0)) * tau.max(0.0))
}

/// Mix of a clear and a covered quantity by the sky's coverage `0..1`.
#[must_use]
pub fn coverage_mix(clear: f32, covered: f32, coverage: f32) -> f32 {
    let c = coverage.clamp(0.0, 1.0);
    clear * (1.0 - c) + covered * c
}

/// What the real-time renderer does to the sun's light under `layer` (ADR-320): the transmittance
/// its shadow-map march ([`cloud_shadow_march`], the CPU mirror of the map's texel) gives through
/// a layer of constant normalised density 1 and `settings.density = extinction`. The march is
/// Beer–Lambert on the curved shells, so it removes every photon the slab scatters, including
/// the forward-scattered ones the tracer delivers to the ground.
#[must_use]
pub fn renderer_sun_ratio(layer: &PtCloudLayer, sun_y: f32) -> f32 {
    let sun_y = sun_y.clamp(0.0, 1.0);
    let to_sun = [(1.0 - sun_y * sun_y).max(0.0).sqrt(), sun_y, 0.0];
    let settings = CloudSettings {
        density: layer.extinction,
        altitude_bottom: layer.base_m,
        altitude_top: layer.top_m,
        ..CloudSettings::default()
    };
    cloud_shadow_march([0.0, 0.0], to_sun, &settings, REFERENCE_PLANET_RADIUS_M, |_| 1.0)
}

/// What the real-time renderer does to the sky light under a fully covered `layer` (ADR-321):
/// the IBL scales the sky by `1 − CLOUD_OVERCAST_DIM · overcast` (the grey-toward-luminance part
/// does not change a grey sky), with `overcast` the opacity `1 − exp(−τ)` of the covered column.
#[must_use]
pub fn renderer_sky_ratio(layer: &PtCloudLayer) -> f32 {
    let opacity = 1.0 - (-layer.optical_depth()).exp();
    1.0 - CLOUD_OVERCAST_DIM * opacity
}

/// Ground light a sun and a uniform sky give a horizontal surface under the renderer's model
/// ([`renderer_sun_ratio`], [`renderer_sky_ratio`]), relative to the clear sky. `sun_lux` is the
/// illuminance at normal incidence and `sky_lux` the sky's horizontal illuminance.
#[must_use]
pub fn renderer_ground_ratio(layer: &PtCloudLayer, sun_y: f32, sun_lux: f32, sky_lux: f32) -> f32 {
    // The same NaN sun as [`reference_ground_ratio`] (the lowest sun), so the gap compares one
    // sun on both sides instead of a NaN ratio floored to 1e-6.
    let sun_y = if sun_y.is_nan() { 0.02 } else { sun_y };
    let direct = sun_lux * sun_y.clamp(0.0, 1.0);
    let clear = direct + sky_lux;
    if clear <= 0.0 {
        return 1.0;
    }
    (direct * renderer_sun_ratio(layer, sun_y) + sky_lux * renderer_sky_ratio(layer)) / clear
}

/// The overcast factor the renderer reads from `settings` for a sky of full weather (every
/// weather texel at 1): `cloud_overcast_factor` on a weather map filled with ones, so the
/// layer's thickness and profile are checked without a noise field.
#[must_use]
pub fn full_cover_overcast_factor(settings: &CloudSettings) -> f32 {
    let mut full = CloudSettings {
        coverage: 1.0,
        ..*settings
    };
    full.map_resolution = 4;
    let map = WeatherMap {
        resolution: 4,
        rgba: vec![255; 4 * 4 * 4],
        filled: 1.0,
    };
    cloud_overcast_factor(&full, &map)
}

/// Which light a traced measurement uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudLight {
    /// The sun and the sky.
    Both,
    /// The sun alone (a black sky): ADR-320, the sun term.
    SunOnly,
    /// The sky alone (no sun): ADR-321, the sky light.
    SkyOnly,
}

/// A traced measurement of one layer at one sun height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CloudGroundMeasure {
    /// Mean luminance of the lit plane without the layer.
    pub clear: f64,
    /// Mean luminance of the lit plane under the layer.
    pub covered: f64,
}

impl CloudGroundMeasure {
    /// `covered / clear`.
    #[must_use]
    pub fn ratio(&self) -> f64 {
        if self.clear > 0.0 {
            self.covered / self.clear
        } else {
            1.0
        }
    }
}

/// Path vertices a trace needs for `layer`: a photon random-walks through the slab with an
/// effective optical depth `τ (1 − g)`, which takes about its square in scattering events to
/// cross; sixteen times that, plus a floor for the plane's own bounces, leaves under a percent of
/// the energy cut off. At most [`crate::MAX_PATH_VERTICES`].
#[must_use]
pub fn suggested_max_bounces(layer: &PtCloudLayer) -> u32 {
    let eff = layer.optical_depth() * (1.0 - layer.asymmetry.clamp(-1.0, 1.0)).max(0.0);
    let walk = (16.0 * eff * eff).ceil().min(crate::MAX_PATH_VERTICES as f32) as u32;
    (32 + walk).min(crate::MAX_PATH_VERTICES)
}

/// Illuminance of the validation sun, lux (a clear noon; the ratio is independent of it).
pub const VALIDATION_SUN_LUX: f32 = 100_000.0;

/// Radiance of the validation sky, nit. A uniform sky of radiance `L` lights a horizontal
/// plane with `π · L` lux, so 8000 is about 25 klux, a clear sky's share of a clear noon.
pub const VALIDATION_SKY_NIT: f32 = 8_000.0;

/// Half edge of the validation ground, metres. Light that a thick slab scatters sideways comes
/// back to the ground tens of kilometres away; a small plane would lose it, and the sky below
/// the plane's edge would add light the real ground never lets in.
pub const VALIDATION_GROUND_HALF_M: f32 = 150_000.0;

/// Albedo of the validation ground.
pub const VALIDATION_GROUND_ALBEDO: f32 = 0.5;

/// The validation scene without a layer: a very large lambert ground at y = 0 seen from 2 m,
/// lit by the validation sun at `sun_y` (sine of its elevation) and a uniform sky, as `light`
/// selects.
#[must_use]
pub fn validation_scene(sun_y: f32, light: CloudLight) -> PtScene {
    // A NaN `sun_y` is the lowest sun (`clamp` passes NaN through, which made a NaN sun).
    let elevation = if sun_y.is_nan() {
        0.02f32
    } else {
        sun_y.clamp(0.02, 1.0)
    }
    .asin();
    let mut scene = scenes::sun_plane(
        PtMaterial::lambert([VALIDATION_GROUND_ALBEDO; 3]),
        elevation,
        VALIDATION_SUN_LUX,
        0.0047,
        8,
    );
    // Replace the 100 m plane by the large one (same material, same instance).
    let h = VALIDATION_GROUND_HALF_M;
    if let Some(mesh) = scene.meshes.first_mut() {
        *mesh = scenes::quad([[-h, 0.0, -h], [-h, 0.0, h], [h, 0.0, h], [h, 0.0, -h]]);
    }
    match light {
        CloudLight::Both => scene.sky = PtSky::Uniform([VALIDATION_SKY_NIT; 3]),
        CloudLight::SunOnly => {}
        CloudLight::SkyOnly => {
            scene.sky = PtSky::Uniform([VALIDATION_SKY_NIT; 3]);
            scene.sun = None;
        }
    }
    scene
}

/// Traces the validation ground lit by the validation sun at `sun_y` (the sine of its elevation) and a
/// uniform sky (as `light` selects), once clear and once under `layer`, and returns the mean luminance of both.
///
/// The camera looks at the plane from 2 m, so the layer is never in view and all it does is
/// change the light that arrives: the sun through the slab, and the sky and the slab's own
/// scattered light.
///
/// # Errors
/// [`PtError`] from the scene or the GPU.
pub fn measure_ground_ratio(
    tracer: &PathTracer,
    layer: &PtCloudLayer,
    sun_y: f32,
    light: CloudLight,
    spp: u32,
    seed: u32,
) -> Result<CloudGroundMeasure, PtError> {
    let _span = tracing::info_span!("pt_cloud_measure", spp).entered();
    let build = |with_layer: bool| -> Result<PtScene, PtError> {
        let mut scene = validation_scene(sun_y, light);
        if with_layer {
            layer.add_to(&mut scene)?;
        }
        Ok(scene)
    };
    let settings = PtSettings {
        spp,
        max_bounces: suggested_max_bounces(layer),
        seed,
        rr_start: 8,
        jitter: true,
        samples_per_submit: 8,
    };
    let mean = |scene: &PtScene| -> Result<f64, PtError> {
        let img = tracer.render(scene, &settings)?;
        let n = img.beauty.len().max(1) as f64;
        Ok(img
            .beauty
            .iter()
            .map(|c| 0.2126 * f64::from(c[0]) + 0.7152 * f64::from(c[1]) + 0.0722 * f64::from(c[2]))
            .sum::<f64>()
            / n)
    };
    Ok(CloudGroundMeasure {
        clear: mean(&build(false)?)?,
        covered: mean(&build(true)?)?,
    })
}

/// Monte Carlo transport through the slab in one dimension, the closed-form-free reference the
/// traced ratios are checked against: unit packets enter the top (the sun at `sun_y` when
/// `sun_y` is `Some`, otherwise a uniform sky with cosine-weighted directions), random-walk
/// with free flights `−ln ξ / σ` and Henyey–Greenstein scattering, and every arrival at the
/// base counts as ground irradiance (the ground, a lambert surface of `ground_albedo`, sends the
/// packet back up with probability `ground_albedo` and a cosine-weighted direction). Packets
/// that leave through the top are lost (the black sky of the sun-only measurement; a
/// uniform sky sends its own light in, which is already counted by the entering packets).
///
/// Returns the arrivals per entering packet divided by the clear value (1 arrival per packet
/// for a sun or sky above bare ground, before the ground's own reflection is added). The
/// ground's reflection that never meets the slab again equals the clear case's, so the ratio is
/// the traced `covered / clear`.
#[must_use]
pub fn slab_transport_reference(
    layer: &PtCloudLayer,
    sun_y: Option<f32>,
    ground_albedo: f32,
    packets: u32,
    seed: u64,
) -> f64 {
    let sigma = f64::from(layer.extinction);
    let height = f64::from(layer.thickness_m());
    let albedo = f64::from(layer.albedo);
    let g = f64::from(layer.asymmetry);
    let ground = f64::from(ground_albedo);
    // A NaN extinction passes all light too (it would otherwise walk every packet to the vertex cap).
    // A NaN asymmetry: the renderer's diffuse reach (`(1 - g).max(0)`) is then 0, so the layer
    // passes all light there too; traced, it made every scattering cosine NaN.
    if sigma.is_nan() || g.is_nan() || sigma <= 0.0 || height <= 0.0 {
        return 1.0;
    }
    // A NaN sun is the lowest sun, the same one `validation_scene` traces. `clamp` keeps a
    // NaN, and `NaN <= 0` never meets the ground, so every packet walked to the vertex cap.
    let sun_y = sun_y.map(|y| if y.is_nan() { 0.02 } else { y.clamp(0.02, 1.0) });
    let mut rng = seed ^ 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        // SplitMix64.
        rng = rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    };
    let mut arrivals = 0.0_f64;
    for _ in 0..packets {
        // Direction cosine along +y; entering packets go down.
        let mut mu = -match sun_y {
            Some(y) => f64::from(y),
            None => next().sqrt(),
        };
        let mut y = height;
        let mut alive = true;
        let mut vertices = 0;
        while alive && vertices < 20_000 {
            vertices += 1;
            let free = -(1.0 - next()).ln() / sigma;
            let ny = y + mu * free;
            if ny >= height {
                alive = false; // left through the top
            } else if ny <= 0.0 {
                arrivals += 1.0;
                if next() < ground {
                    y = 0.0;
                    mu = next().sqrt(); // lambert, upward
                } else {
                    alive = false;
                }
            } else {
                y = ny;
                if next() >= albedo {
                    alive = false;
                } else {
                    // Henyey–Greenstein: cosine of the scattering angle, then rotate mu.
                    let u = next();
                    let cos_t = if g.abs() < 1.0e-3 {
                        1.0 - 2.0 * u
                    } else {
                        let f = (1.0 - g * g) / (1.0 - g + 2.0 * g * u);
                        (1.0 + g * g - f * f) / (2.0 * g)
                    };
                    let phi = std::f64::consts::TAU * next();
                    let sin_t = (1.0 - cos_t * cos_t).max(0.0).sqrt();
                    let sin_mu = (1.0 - mu * mu).max(0.0).sqrt();
                    mu = (mu * cos_t + sin_mu * sin_t * phi.cos()).clamp(-1.0, 1.0);
                }
            }
        }
    }
    arrivals / f64::from(packets.max(1))
}

/// Ground light the transport reference ([`slab_transport_reference`]) gives a horizontal
/// surface under a sun at `sun_y` and a uniform sky, relative to the clear sky, with the same
/// light mix [`renderer_ground_ratio`] uses. The reference is exact for any optical depth; the
/// traced ratio agrees with it up to about τ = 10 (beyond, a 64-pixel trace is too noisy).
#[must_use]
pub fn reference_ground_ratio(layer: &PtCloudLayer, sun_y: f32, sun_lux: f32, sky_lux: f32) -> f32 {
    // Same NaN sun as [`slab_transport_reference`]: `clamp` would leave it NaN, `clear <= 0`
    // would be false, and both reference walks would run.
    let sun_y = if sun_y.is_nan() { 0.02 } else { sun_y };
    let direct = sun_lux * sun_y.clamp(0.0, 1.0);
    let clear = direct + sky_lux;
    if clear <= 0.0 {
        return 1.0;
    }
    let sun = slab_transport_reference(layer, Some(sun_y), VALIDATION_GROUND_ALBEDO, 20_000, 1) as f32;
    let sky = slab_transport_reference(layer, None, VALIDATION_GROUND_ALBEDO, 20_000, 2) as f32;
    (direct * sun + sky_lux * sky) / clear
}

/// `log2(renderer / reference)` of the ground light under `layer` at `sun_y`, with the validation
/// sun and sky: negative where the real-time model lights the ground less than the transport
/// reference does. One stop is a factor of two.
#[must_use]
pub fn renderer_log2_gap(layer: &PtCloudLayer, sun_y: f32) -> f32 {
    let sky = std::f32::consts::PI * VALIDATION_SKY_NIT;
    let r = renderer_ground_ratio(layer, sun_y, VALIDATION_SUN_LUX, sky).max(1.0e-6);
    let t = reference_ground_ratio(layer, sun_y, VALIDATION_SUN_LUX, sky).max(1.0e-6);
    (r / t).log2()
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::COLUMN_FILL;

    /// A NaN sun is the lowest sun on the renderer side too, so the gap is not a NaN ratio
    /// floored to 1e-6 (about -20 stops) (corr-low-render, corr-pt finding 9).
    #[test]
    fn a_nan_sun_is_the_lowest_sun_on_the_renderer_side() {
        let l = PtCloudLayer::overcast(1000.0, 3000.0, 5.0);
        let nan = renderer_ground_ratio(&l, f32::NAN, VALIDATION_SUN_LUX, 1000.0);
        let low = renderer_ground_ratio(&l, 0.02, VALIDATION_SUN_LUX, 1000.0);
        assert_eq!(nan.to_bits(), low.to_bits(), "NaN sun {nan}, lowest sun {low}");
    }

    /// A NaN asymmetry passes all light, as the renderer's model does, instead of walking every
    /// packet with NaN cosines (corr-low-render, corr-pt finding 10).
    #[test]
    fn a_nan_asymmetry_passes_all_light() {
        let mut l = PtCloudLayer::overcast(1000.0, 3000.0, 5.0);
        l.asymmetry = f32::NAN;
        let t = slab_transport_reference(&l, Some(0.5), 0.0, 24, 1);
        assert_eq!(t, 1.0, "NaN asymmetry traced as {t}");
    }

    #[test]
    fn an_overcast_layer_has_the_requested_optical_depth() {
        let l = PtCloudLayer::overcast(1500.0, 4000.0, 10.0);
        assert!((l.optical_depth() - 10.0).abs() < 1.0e-4);
        assert_eq!(l.base_m, 1500.0);
        assert_eq!(l.top_m, 4000.0);
    }

    /// A NaN sun used to walk every packet to the vertex cap (`clamp` keeps NaN, and `NaN <= 0`
    /// never meets the ground) and report no arrivals. It is the lowest sun, the same one
    /// [`validation_scene`] traces.
    #[test]
    fn a_nan_sun_is_the_lowest_sun_in_the_transport_reference() {
        let layer = PtCloudLayer::overcast(1000.0, 3000.0, 0.2);
        let nan = slab_transport_reference(&layer, Some(f32::NAN), 0.5, 24, 7);
        let low = slab_transport_reference(&layer, Some(0.02), 0.5, 24, 7);
        assert!(
            low > 0.0,
            "the lowest sun must deliver some ground light, got {low}"
        );
        assert_eq!(nan, low);
        assert!(nan.is_finite());
    }

    #[test]
    fn the_suggested_bounce_budget_follows_its_formula_and_cap() {
        // ceil(16 (tau (1 - g))^2) + 32 with tau 10, g 0.85: 16 * 1.5^2 = 36, so 68.
        let mut l = PtCloudLayer::overcast(1000.0, 3000.0, 10.0);
        l.asymmetry = 0.85;
        assert_eq!(suggested_max_bounces(&l), 68);
        // A very thick, forward-poor slab hits the vertex cap.
        let thick = PtCloudLayer::overcast(1000.0, 3000.0, 500.0);
        assert_eq!(suggested_max_bounces(&thick), crate::MAX_PATH_VERTICES);
    }

    #[test]
    fn the_tracer_layer_conserves_energy_and_covers_the_slab() {
        let l = PtCloudLayer::overcast(1000.0, 3000.0, 20.0);
        let m = l.medium_layer();
        assert_eq!((m.min_height, m.max_height), (1000.0, 3000.0));
        assert_eq!(m.falloff, 0.0);
        assert!(!m.aerial);
        // Albedo 1: everything extinguished is scattered.
        assert!((m.scattering[0] - l.extinction).abs() < 1.0e-9);
        assert_eq!(m.absorption, [0.0; 3]);
        let mut absorbing = l;
        absorbing.albedo = 0.9;
        let m = absorbing.medium_layer();
        assert!((m.scattering[0] + m.absorption[0] - l.extinction).abs() < 1.0e-7);
    }

    #[test]
    fn the_layer_becomes_active_media_and_the_ninth_layer_is_refused() {
        let l = PtCloudLayer::overcast(1000.0, 3000.0, 5.0);
        let mut scene = scenes::sun_plane(PtMaterial::lambert([0.5; 3]), 1.0, 1000.0, 0.0047, 4);
        assert!(!scene.media.is_active());
        l.add_to(&mut scene).expect("first layer");
        assert!(scene.media.is_active());
        for _ in 1..8 {
            l.add_to(&mut scene).expect("up to eight");
        }
        assert!(matches!(l.add_to(&mut scene), Err(PtError::Scene(_))));
    }

    #[test]
    fn beam_transmittance_is_beer_lambert_along_the_slant() {
        let l = PtCloudLayer::overcast(1500.0, 4000.0, 2.0);
        assert!((l.beam_transmittance(1.0) - (-2.0f32).exp()).abs() < 1.0e-6);
        assert!((l.beam_transmittance(0.5) - (-4.0f32).exp()).abs() < 1.0e-6);
        assert_eq!(l.beam_transmittance(0.0), 1.0);
        assert_eq!(l.beam_transmittance(f32::NAN), 1.0);
    }

    #[test]
    fn two_stream_limits() {
        assert!((two_stream_diffuse_transmittance(0.0, 0.85) - 1.0).abs() < 1.0e-6);
        let thick = two_stream_diffuse_transmittance(100.0, 0.85);
        assert!((thick - 1.0 / (1.0 + 0.75 * 0.15 * 100.0)).abs() < 1.0e-6);
        assert!(thick < two_stream_diffuse_transmittance(10.0, 0.85));
        // Isotropic scattering blocks more than forward scattering at the same depth.
        assert!(two_stream_diffuse_transmittance(10.0, 0.0) < two_stream_diffuse_transmittance(10.0, 0.85));
    }

    #[test]
    fn coverage_mix_interpolates() {
        assert_eq!(coverage_mix(1.0, 0.2, 0.0), 1.0);
        assert_eq!(coverage_mix(1.0, 0.2, 1.0), 0.2);
        assert!((coverage_mix(1.0, 0.2, 0.5) - 0.6).abs() < 1.0e-6);
        assert_eq!(coverage_mix(1.0, 0.2, 7.0), 0.2);
    }

    #[test]
    fn the_renderers_shadow_march_agrees_with_beer_lambert_for_a_uniform_slab() {
        // The renderer marches the curved shells; for a sun well above the horizon the slab is
        // flat to a fraction of a percent of the optical depth.
        let l = PtCloudLayer::overcast(1500.0, 4000.0, 1.5);
        for sun_y in [0.3_f32, 0.6, 0.9] {
            let settings = CloudSettings {
                density: l.extinction,
                altitude_bottom: l.base_m,
                altitude_top: l.top_m,
                ..CloudSettings::default()
            };
            let to_sun = [(1.0 - sun_y * sun_y).sqrt(), sun_y, 0.0];
            let marched =
                cloud_shadow_march([0.0, 0.0], to_sun, &settings, REFERENCE_PLANET_RADIUS_M, |_| 1.0);
            let beer = l.beam_transmittance(sun_y);
            assert!(
                (marched - beer).abs() < 0.01 * beer.max(0.05),
                "sun_y {sun_y}: marched {marched}, Beer {beer}"
            );
        }
    }

    #[test]
    fn from_settings_follows_density_thickness_and_fill() {
        let s = CloudSettings {
            density: 0.04,
            altitude_bottom: 1500.0,
            altitude_top: 4000.0,
            cloud_type: 0.5,
            ..CloudSettings::default()
        };
        let a = PtCloudLayer::from_settings(&s, COLUMN_FILL);
        let b = PtCloudLayer::from_settings(&s, 2.0 * COLUMN_FILL);
        assert!((b.optical_depth() / a.optical_depth() - 2.0).abs() < 1.0e-4);
        assert_eq!((a.base_m, a.top_m), (1500.0, 4000.0));
        assert!(a.optical_depth() > 0.0);
        let zero = PtCloudLayer::from_settings(&s, 0.0);
        assert_eq!(zero.optical_depth(), 0.0);
    }

    #[test]
    fn the_full_cover_factor_matches_the_layers_opacity() {
        let s = CloudSettings {
            density: 0.04,
            coverage: 1.0,
            ..CloudSettings::default()
        };
        let o = full_cover_overcast_factor(&s);
        let l = PtCloudLayer::from_settings(&s, COLUMN_FILL);
        let opacity = 1.0 - (-l.optical_depth()).exp();
        // The weather boost clamps the cover to 1, so the factor is the opacity itself.
        assert!((o - opacity).abs() < 1.0e-4, "factor {o}, opacity {opacity}");
    }

    #[test]
    fn a_thin_layer_leaves_the_renderers_ground_light_unchanged() {
        let l = PtCloudLayer::overcast(1500.0, 4000.0, 0.0);
        let r = renderer_ground_ratio(&l, 0.8, 100_000.0, 25_000.0);
        assert!((r - 1.0).abs() < 1.0e-5, "{r}");
    }

    #[test]
    fn a_thick_layer_darkens_the_renderers_ground_light_to_the_sky_floor() {
        let l = PtCloudLayer::overcast(1500.0, 4000.0, 60.0);
        let sun_y = 0.8;
        let r = renderer_ground_ratio(&l, sun_y, 100_000.0, 25_000.0);
        let floor = 25_000.0 * (1.0 - CLOUD_OVERCAST_DIM) / (100_000.0 * sun_y + 25_000.0);
        assert!((r - floor).abs() < 1.0e-3, "ratio {r}, floor {floor}");
    }

    /// The largest darkening (stops) of the real-time model against the reference that ADR-1834
    /// recorded over the grid below. A change that makes it worse fails; a fix that brings it
    /// down should lower this number (the point of the record).
    const RECORDED_MAX_DARKENING_STOPS: f32 = 3.5;

    #[test]
    fn the_renderers_ground_light_is_never_brighter_than_the_reference_and_its_gap_is_recorded() {
        let mut worst = 0.0_f32;
        for tau in [0.5_f32, 1.0, 2.0, 5.0, 10.0, 30.0] {
            for sun_y in [0.2_f32, 0.5, 0.9] {
                let l = PtCloudLayer::overcast(1500.0, 4000.0, tau);
                let gap = renderer_log2_gap(&l, sun_y);
                // Up to tau 30 the renderer only removes light the reference keeps.
                assert!(
                    gap < 0.05,
                    "tau {tau} sun_y {sun_y}: renderer brighter by {gap} stops"
                );
                worst = worst.max(-gap);
            }
        }
        assert!(
            worst <= RECORDED_MAX_DARKENING_STOPS,
            "the renderer is {worst} stops darker than the reference, recorded {RECORDED_MAX_DARKENING_STOPS}"
        );
        // And the gap is real, so the record is not vacuous.
        assert!(
            worst > 2.0,
            "the recorded gap vanished ({worst}): lower RECORDED_MAX_DARKENING_STOPS"
        );
    }

    #[test]
    fn the_reference_is_exact_at_the_limits_and_falls_with_depth() {
        let thin = PtCloudLayer::overcast(1500.0, 4000.0, 0.0);
        assert_eq!(slab_transport_reference(&thin, Some(0.7), 0.5, 100, 1), 1.0);
        let mut last = f64::MAX;
        for tau in [0.1_f32, 1.0, 5.0, 20.0] {
            let l = PtCloudLayer::overcast(1500.0, 4000.0, tau);
            let r = slab_transport_reference(&l, Some(0.7), 0.5, 20_000, 3);
            assert!(r < last + 0.02, "tau {tau}: {r} after {last}");
            // Forward scattering delivers more than the unscattered beam alone.
            assert!(r >= f64::from(l.beam_transmittance(0.7)) - 0.02, "tau {tau}: {r}");
            last = r;
        }
        // The reference is deterministic per seed.
        let l = PtCloudLayer::overcast(1500.0, 4000.0, 3.0);
        assert_eq!(
            slab_transport_reference(&l, None, 0.5, 2000, 9),
            slab_transport_reference(&l, None, 0.5, 2000, 9)
        );
    }

    #[test]
    fn the_reference_matches_two_stream_for_an_absorbing_free_diffuse_slab_over_a_black_ground() {
        // Black ground: arrivals are the slab's diffuse transmittance for a cosine-weighted sky.
        // Two-stream is an approximation (it assumes isotropic incidence and a thick slab), so
        // the bound is loose; it catches a wrong phase function or a lost factor of 2.
        for tau in [10.0_f32, 30.0] {
            let l = PtCloudLayer::overcast(1500.0, 4000.0, tau);
            let mc = slab_transport_reference(&l, None, 0.0, 40_000, 4) as f32;
            let two = two_stream_diffuse_transmittance(tau, l.asymmetry);
            assert!(
                (mc / two - 1.0).abs() < 0.35,
                "tau {tau}: reference {mc}, two-stream {two}"
            );
        }
    }

    #[test]
    fn the_ratio_falls_with_optical_depth() {
        let mut last = f32::MAX;
        for tau in [0.0, 0.5, 2.0, 8.0, 30.0] {
            let l = PtCloudLayer::overcast(1500.0, 4000.0, tau);
            let r = renderer_ground_ratio(&l, 0.7, 100_000.0, 25_000.0);
            assert!(r <= last + 1.0e-6, "tau {tau}: {r} after {last}");
            last = r;
        }
    }
}
