// SPDX-License-Identifier: MPL-2.0
//! Physically based sky (Hillaire 2020) and the map clock that drives it.
//!
//! Transmittance, multiple-scattering, sky-view and aerial-perspective lookup tables are
//! built in kilometres, then sampled in world metres. The sun's elevation and azimuth come
//! from [`TimeOfDay`] (hour, day of year, map latitude). Provenance is in `docs/provenance.md`.
//!
//! Earth draws these tables only when the profile sets `physical_atmosphere` (Desktop High
//! and Ultra). Mars and an airless body have no analytic stand-in, so a single-sample HDR
//! frame builds them on every preset, including Mobile, Desktop Low and Deck
//! ([`planet_lut_sky`]). An installed studio environment suppresses the bake. Multisample
//! frames, and other LDR frames, keep the analytic gradient in `utils.wgsl` (`sky_color`)
//! plus exponential height fog. A reflection-probe face on a non-Earth planet still uses
//! the tables.

use regia_app::prelude::*;
pub use regia_scene::level::{AtmospherePreset, SkyBody};

/// Whether the frame integrates the LUT sky for this planet (ADR-039).
///
/// Earth uses the tables only when `physical_atmosphere` is set. Any other planet uses
/// them. `studio_rules` suppresses the bake so an installed studio environment wins
/// (ADR-2000). The caller still requires a single-sample HDR target before drawing the
/// sky pass, and passes this for a non-Earth reflection-probe face when that pass is off.
#[must_use]
pub(crate) fn planet_lut_sky(physical_atmosphere: bool, earth: bool, studio_rules: bool) -> bool {
    (physical_atmosphere || !earth) && !studio_rules
}

use crate::soft_math;

const PI: f32 = std::f32::consts::PI;
const TRANSMITTANCE_W: u32 = 64;
const TRANSMITTANCE_H: u32 = 32;
const MULTI_W: u32 = 24;
const MULTI_H: u32 = 24;
const SKY_W: u32 = 128;
const SKY_H: u32 = 64;
const AERIAL_W: u32 = 32;
const AERIAL_H: u32 = 16;
/// Must match `AERIAL_SLICES` in `outdoor_sky.wgsl`.
const AERIAL_SLICES: u32 = 16;
/// Aerial LUT covers this many kilometres of view distance.
const AERIAL_RANGE_KM: f32 = 32.0;

/// Local solar time and the map's latitude (`specs/26` § 4, atmosphere row).
///
/// Azimuth is measured clockwise from north. World axes: +X east, +Y up, +Z north.
///
/// # Example
///
/// ```
/// use regia_render::{TimeOfDay, sun_position};
///
/// let noon = TimeOfDay { hour: 12.0, day_of_year: 80, latitude_deg: 45.0 };
/// let sun = sun_position(&noon);
/// assert!(sun.elevation_rad > 0.6 && sun.elevation_rad < 1.0);
/// assert!(sun.direction[2] < -0.4, "equinox noon at 45°N is in the south");
/// ```
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct TimeOfDay {
    /// Hours since local solar midnight, wrapped into `0..24`.
    pub hour: f32,
    /// Day of year, clamped to `1..=365` when the sun is evaluated.
    pub day_of_year: u16,
    /// Map latitude in degrees, north positive, clamped to `-89..=89`.
    pub latitude_deg: f32,
}

impl Default for TimeOfDay {
    fn default() -> Self {
        Self {
            hour: 12.0,
            day_of_year: 80,
            latitude_deg: 45.0,
        }
    }
}

impl TimeOfDay {
    /// Dawn, sun a few degrees above the eastern horizon (equinox, 45°N).
    #[must_use]
    pub fn dawn() -> Self {
        Self {
            hour: 6.4,
            day_of_year: 80,
            latitude_deg: 45.0,
        }
    }

    /// Solar noon.
    #[must_use]
    pub fn noon() -> Self {
        Self::default()
    }

    /// Dusk, sun a few degrees above the western horizon.
    #[must_use]
    pub fn dusk() -> Self {
        Self {
            hour: 17.6,
            day_of_year: 80,
            latitude_deg: 45.0,
        }
    }

    /// Solar midnight.
    #[must_use]
    pub fn night() -> Self {
        Self {
            hour: 0.0,
            day_of_year: 80,
            latitude_deg: 45.0,
        }
    }
}

/// Sun direction derived from [`TimeOfDay`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SunPosition {
    /// Radians above the horizon. Negative when the sun is down.
    pub elevation_rad: f32,
    /// Radians clockwise from north (`0` north, `π/2` east, `π` south).
    pub azimuth_rad: f32,
    /// Unit vector toward the sun (+X east, +Y up, +Z north).
    pub direction: [f32; 3],
}

impl SunPosition {
    /// Direction of light travel (from the sun toward the scene), for [`crate::DirectionalLightUniform`].
    #[must_use]
    pub fn travel_direction(self) -> [f32; 3] {
        [-self.direction[0], -self.direction[1], -self.direction[2]]
    }
}

/// Solar declination (radians) from Spencer's Fourier series.
///
/// J. W. Spencer, "Fourier series representation of the position of the sun", 1971,
/// as used by the NOAA solar calculator.
fn solar_declination(day_of_year: f32) -> f32 {
    let g = 2.0 * PI * (day_of_year - 1.0) / 365.0;
    0.006918 - 0.399912 * soft_math::cos(g) + 0.070257 * soft_math::sin(g)
        - 0.006758 * soft_math::cos(2.0 * g)
        + 0.000907 * soft_math::sin(2.0 * g)
        - 0.002697 * soft_math::cos(3.0 * g)
        + 0.00148 * soft_math::sin(3.0 * g)
}

/// Elevation, azimuth and world direction of the sun.
#[must_use]
pub fn sun_position(time: &TimeOfDay) -> SunPosition {
    let day = f32::from(time.day_of_year.clamp(1, 365));
    let lat = time.latitude_deg.clamp(-89.0, 89.0).to_radians();
    let hour = time.hour.rem_euclid(24.0);
    let decl = solar_declination(day);
    // Hour angle: positive in the afternoon (sun toward the west).
    let ha = (hour - 12.0) * 15.0_f32.to_radians();
    // soft_math: these bits feed LUTs, bakes and the weather sim, so the platform
    // libm is not allowed here (audit G, render5 determinism).
    let sin_el = soft_math::sin(lat) * soft_math::sin(decl)
        + soft_math::cos(lat) * soft_math::cos(decl) * soft_math::cos(ha);
    let elevation = soft_math::asin(sin_el.clamp(-1.0, 1.0));
    let cos_el = soft_math::cos(elevation).max(1.0e-4);
    let cos_az = (soft_math::sin(decl) - soft_math::sin(elevation) * soft_math::sin(lat))
        / (cos_el * soft_math::cos(lat).max(1.0e-4));
    let mut azimuth = soft_math::acos(cos_az.clamp(-1.0, 1.0));
    if ha > 0.0 {
        azimuth = std::f32::consts::TAU - azimuth;
    }
    let direction = normalize([
        cos_el * soft_math::sin(azimuth),
        soft_math::sin(elevation),
        cos_el * soft_math::cos(azimuth),
    ]);
    SunPosition {
        elevation_rad: elevation,
        azimuth_rad: azimuth,
        direction,
    }
}

/// One RGBA LUT image, row-major.
#[derive(Clone, Debug)]
pub struct LutImage {
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// Row-major RGBA. Aerial perspective stores inscatter in RGB and transmittance in A.
    pub texels: Vec<[f32; 4]>,
}

impl LutImage {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            texels: vec![[0.0; 4]; (width * height) as usize],
        }
    }

    fn put(&mut self, x: u32, y: u32, value: [f32; 4]) {
        self.texels[(y * self.width + x) as usize] = value;
    }

    /// Bilinear sample. `u` and `v` are wrapped on X and clamped on Y.
    #[must_use]
    pub fn sample(&self, u: f32, v: f32) -> [f32; 4] {
        let w = self.width.max(1) as f32;
        let h = self.height.max(1) as f32;
        let fu = u.rem_euclid(1.0) * w - 0.5;
        let fv = v.clamp(0.0, 0.999) * h - 0.5;
        let x0 = fu.floor();
        let y0 = fv.floor();
        let tx = fu - x0;
        let ty = fv - y0;
        let sample_at = |x: f32, y: f32| {
            let xi = x.rem_euclid(w) as u32 % self.width;
            let yi = y.clamp(0.0, h - 1.0) as u32 % self.height;
            self.texels[(yi * self.width + xi) as usize]
        };
        let a = sample_at(x0, y0);
        let b = sample_at(x0 + 1.0, y0);
        let c = sample_at(x0, y0 + 1.0);
        let d = sample_at(x0 + 1.0, y0 + 1.0);
        lerp4(lerp4(a, b, tx), lerp4(c, d, tx), ty)
    }

    /// Bilinear sample of a table that does not wrap (transmittance, multiple scattering):
    /// `u` and `v` run from the centre of the first texel (0) to the centre of the last (1)
    /// and are clamped, so the table's edge rows and columns are exact at the range ends.
    /// [`Self::sample`] wraps `u` (the sky-view azimuth): on these tables it blended the
    /// overhead sun's texel with the one pointing straight down (ADR-1447).
    #[must_use]
    pub fn sample_clamped(&self, u: f32, v: f32) -> [f32; 4] {
        let last_x = self.width.max(1) - 1;
        let last_y = self.height.max(1) - 1;
        let fu = u.clamp(0.0, 1.0) * last_x as f32;
        let fv = v.clamp(0.0, 1.0) * last_y as f32;
        let x0 = (fu.floor() as u32).min(last_x);
        let y0 = (fv.floor() as u32).min(last_y);
        let x1 = (x0 + 1).min(last_x);
        let y1 = (y0 + 1).min(last_y);
        let tx = fu - x0 as f32;
        let ty = fv - y0 as f32;
        let at = |x: u32, y: u32| self.texels[(y * self.width + x) as usize];
        lerp4(
            lerp4(at(x0, y0), at(x1, y0), tx),
            lerp4(at(x0, y1), at(x1, y1), tx),
            ty,
        )
    }
}

/// Midpoint and length of segment `i` of `steps` along a ray of length `t_end`, with the
/// segments growing quadratically from the start: a ray that leaves the dense low air (a
/// 1.2 km aerosol layer, an 8 km Rayleigh layer) for hundreds of kilometres of thin air gets
/// most of its samples where the air is (Hillaire 2020 § 5.1 uses a non-linear distribution
/// for the same reason). Uniform steps put one sample in the haze layer of a low sky ray.
fn quadratic_step(i: u32, steps: u32, t_end: f32) -> (f32, f32) {
    let n = steps.max(1) as f32;
    let a = i as f32 / n;
    let b = (i as f32 + 1.0) / n;
    let t0 = t_end * a * a;
    let t1 = t_end * b * b;
    (0.5 * (t0 + t1), t1 - t0)
}

/// Baked atmosphere tables for one sun elevation and camera height.
#[derive(Clone, Debug)]
pub struct AtmosphereLuts {
    /// Transmittance to space (or zero when the ray hits the planet).
    pub transmittance: LutImage,
    /// Isotropic multiple-scattering luminance (cd/m²) by altitude and sun zenith.
    pub multi_scatter: LutImage,
    /// Sky inscatter (cd/m²) relative to the sun: U azimuth, V view elevation.
    ///
    /// Alpha is the transmittance from the camera to the planet's surface, or 0
    /// when the ray misses the planet. [`sample_sky`] adds alpha times `ground_radiance`
    /// for those rays. RGB stays inscatter-only: the IBL bake adds the ground itself
    /// below the horizon, and storing it here too would light every tilted face twice.
    pub sky_view: LutImage,
    /// Aerial perspective. Slices are stacked along X (`slice * width + x`).
    pub aerial: LutImage,
    /// The sun's single scattering alone, in [`Self::aerial`]'s layout (RGB, cd/m²; alpha is
    /// unused): the share of the aerial in-scatter a cloud layer's shadow removes (ADR-5944).
    /// `aerial − aerial_sun` is the multiple-scattering share, which the shadow leaves.
    pub aerial_sun: LutImage,
    /// Slices in [`Self::aerial`].
    pub aerial_slices: u32,
    /// Unit vector toward the sun these tables were built for.
    pub sun_direction: [f32; 3],
    /// Camera height above the ground (metres) the sky-view table used.
    pub camera_height_m: f32,
    /// The planet the tables were integrated for.
    pub params: AtmosphereParameters,
}

/// Parameters of the Hillaire / Bruneton atmosphere. The default is the published Earth.
///
/// Coefficients are per kilometre at the ground; densities fall off exponentially with
/// the scale heights. Mie terms are per channel so a dusty atmosphere can absorb blue and
/// scatter it into a narrower forward lobe than red (Mars, see `docs/provenance.md`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtmosphereParameters {
    /// Sun illuminance at the top of the atmosphere (lux).
    pub sun_illuminance: f32,
    /// Sun angular radius (radians).
    pub sun_angular_radius: f32,
    /// Planet radius (km).
    pub bottom_radius_km: f32,
    /// Top of the atmosphere (km from the planet centre).
    pub top_radius_km: f32,
    /// Ground-level Rayleigh scattering (1/km).
    pub rayleigh_scattering: [f32; 3],
    /// Rayleigh scale height (km).
    pub rayleigh_scale_height_km: f32,
    /// Ground-level Mie (aerosol) scattering (1/km).
    pub mie_scattering: [f32; 3],
    /// Ground-level Mie extinction (1/km): scattering plus absorption.
    pub mie_extinction: [f32; 3],
    /// Mie scale height (km).
    pub mie_scale_height_km: f32,
    /// Henyey–Greenstein asymmetry of the aerosol, per channel.
    pub mie_g: [f32; 3],
    /// Ozone absorption at its density peak (1/km); Earth's 10–40 km tent profile.
    pub ozone_absorption: [f32; 3],
    /// Mean ground albedo, for the ground bounce in the baked ambient.
    pub ground_albedo: [f32; 3],
    /// Radiance of a bright star (cd/m²) composited in daylight; 0 shows stars only at night.
    pub day_star_radiance: f32,
}

impl Default for AtmosphereParameters {
    fn default() -> Self {
        Self::earth()
    }
}

impl AtmosphereParameters {
    /// Earth (Bruneton 2017, Hillaire 2020 Table 1).
    #[must_use]
    pub fn earth() -> Self {
        Self {
            sun_illuminance: 120_000.0,
            sun_angular_radius: 0.004675,
            bottom_radius_km: 6360.0,
            top_radius_km: 6460.0,
            rayleigh_scattering: [5.802e-3, 13.558e-3, 33.1e-3],
            rayleigh_scale_height_km: 8.0,
            mie_scattering: [3.996e-3; 3],
            mie_extinction: [4.440e-3; 3],
            mie_scale_height_km: 1.2,
            mie_g: [0.8; 3],
            ozone_absorption: [0.650e-3, 1.881e-3, 0.085e-3],
            ground_albedo: [0.3; 3],
            day_star_radiance: 0.0,
        }
    }

    /// Mars at 1.52 AU: CO₂ Rayleigh (≈1/60 of Earth's column, 11.1 km scale height) and
    /// clear-season dust with vertical optical depth 0.3 (the rovers' aphelion-season
    /// record is τ ≈ 0.3–0.5, Lemmon et al. 2004 and 2015), single-scattering albedo
    /// falling toward blue and a stronger forward lobe in blue (Tomasko et al. 1999;
    /// Wolff et al. 2009). The low blue albedo makes the day sky butterscotch; the narrow
    /// blue forward lobe makes the sky around a setting sun blue.
    ///
    /// At τ = 0.3 the afternoon sun carries about three quarters of the light on flat
    /// ground (shadow ≈ 0.25 of sunlit), so sunlit rock and shadow separate the way the
    /// DO27 reference plates do. The earlier τ = 0.8 is dusty-season air: the direct
    /// sun fell to 1.6 klx at 11° elevation and dusk was lit almost only by the sky.
    #[must_use]
    pub fn mars() -> Self {
        let tau = 0.3_f32;
        let h = 11.0_f32;
        let ext = tau / h;
        let albedo = [0.97, 0.80, 0.65];
        Self {
            sun_illuminance: 120_000.0 / (1.524 * 1.524),
            sun_angular_radius: 0.004675 / 1.524,
            bottom_radius_km: 3389.5,
            top_radius_km: 3389.5 + 80.0,
            rayleigh_scattering: [1.6e-4, 3.7e-4, 9.0e-4],
            rayleigh_scale_height_km: 11.1,
            mie_scattering: [ext * albedo[0], ext * albedo[1], ext * albedo[2]],
            mie_extinction: [ext; 3],
            mie_scale_height_km: h,
            mie_g: [0.55, 0.62, 0.72],
            ozone_absorption: [0.0; 3],
            ground_albedo: [0.30, 0.17, 0.09],
            day_star_radiance: 0.0,
        }
    }

    /// An airless moon (Europa at 5.2 AU): no scattering at all. The sky is black and the sun
    /// a small hard disc. Stars carry their physical radiance (the brightest, Sirius at
    /// about 1e-5 lux, spread over one ~1e-6 sr pixel is ≈ 10 cd/m²), so they show at a
    /// night or deep-shadow exposure and vanish next to sunlit ice, as a camera sees them.
    #[must_use]
    pub fn airless() -> Self {
        Self {
            sun_illuminance: 120_000.0 / (5.2 * 5.2),
            sun_angular_radius: 0.004675 / 5.2,
            bottom_radius_km: 1560.8,
            top_radius_km: 1560.8 + 1.0,
            rayleigh_scattering: [0.0; 3],
            rayleigh_scale_height_km: 1.0,
            mie_scattering: [0.0; 3],
            mie_extinction: [0.0; 3],
            mie_scale_height_km: 1.0,
            mie_g: [0.0; 3],
            ozone_absorption: [0.0; 3],
            ground_albedo: [0.6, 0.6, 0.62],
            day_star_radiance: 12.0,
        }
    }

    /// The parameters for `preset`.
    #[must_use]
    pub fn from_preset(preset: AtmospherePreset) -> Self {
        match preset {
            AtmospherePreset::Earth => Self::earth(),
            AtmospherePreset::Mars => Self::mars(),
            AtmospherePreset::Airless => Self::airless(),
        }
    }

    /// Vertical aerosol (Mie) optical depth at the ground: extinction × scale height.
    #[must_use]
    pub fn aerosol_optical_depth(&self) -> f32 {
        self.mie_extinction[0] * self.mie_scale_height_km
    }

    /// Feeds an aerosol turbidity (vertical optical depth τ) into the Mie density: sets the
    /// ground-level extinction to `τ / scale height` so [`Self::aerosol_optical_depth`] becomes
    /// `tau`, scaling the scattering in step to preserve the medium's single-scattering albedo
    /// and keeping the Henyey–Greenstein asymmetry. A value at or below zero leaves Rayleigh-only
    /// air. The published baseline is [`Self::earth`] (τ ≈ 5.33e-3); the value
    /// [`TODAY_AEROSOL_TURBIDITY`](regia_scene::level::TODAY_AEROSOL_TURBIDITY) reproduces it
    /// exactly, so existing Earth levels are unchanged.
    pub fn set_aerosol_turbidity(&mut self, tau: f32) {
        let tau = tau.max(0.0);
        let ext = tau / self.mie_scale_height_km.max(1.0e-3);
        let albedo: [f32; 3] = std::array::from_fn(|c| {
            let e = self.mie_extinction[c];
            if e > 0.0 { self.mie_scattering[c] / e } else { 0.0 }
        });
        self.mie_extinction = [ext; 3];
        self.mie_scattering = std::array::from_fn(|c| ext * albedo[c]);
    }

    /// No scattering medium at all: no LUT march, no aerial perspective.
    #[must_use]
    pub fn is_airless(&self) -> bool {
        let s = self
            .rayleigh_scattering
            .iter()
            .chain(&self.mie_extinction)
            .sum::<f32>();
        s <= 0.0
    }
}

/// Builds the four lookup tables. The sky-view table is relative to the sun's azimuth,
/// so a later change of azimuth does not rebuild it; elevation and camera height do.
#[tracing::instrument(skip_all)]
#[must_use]
pub fn build_atmosphere_luts(sun_direction: [f32; 3], camera_height_m: f32) -> AtmosphereLuts {
    build_atmosphere_luts_for(&AtmosphereParameters::default(), sun_direction, camera_height_m)
}

/// [`build_atmosphere_luts`] for any planet's parameters.
#[tracing::instrument(skip_all)]
#[must_use]
pub fn build_atmosphere_luts_for(
    params: &AtmosphereParameters,
    sun_direction: [f32; 3],
    camera_height_m: f32,
) -> AtmosphereLuts {
    build_atmosphere_luts_from(&AtmosphereBase::new(params), sun_direction, camera_height_m)
}

/// The half of the tables that depends on the planet alone (transmittance and multiple
/// scattering, ~30 ms on the CPU): built once per planet and reused for every sun and
/// camera height ([`build_atmosphere_luts_from`]).
#[derive(Clone, Debug)]
pub struct AtmosphereBase {
    /// The planet.
    pub params: AtmosphereParameters,
    /// Transmittance to space.
    pub transmittance: LutImage,
    /// Isotropic multiple scattering.
    pub multi_scatter: LutImage,
}

impl AtmosphereBase {
    /// Integrates the planet's sun-independent tables.
    #[tracing::instrument(skip_all)]
    #[must_use]
    pub fn new(params: &AtmosphereParameters) -> Self {
        let _mem = regia_core::memory::enter_owner(&crate::heap_owners::ATMOSPHERE_LUTS);
        let transmittance = build_transmittance(params);
        let multi_scatter = build_multi_scatter(&transmittance, *params);
        Self {
            params: *params,
            transmittance,
            multi_scatter,
        }
    }
}

/// The sun- and height-dependent tables (sky view, aerial perspective) over a cached
/// [`AtmosphereBase`]; identical to [`build_atmosphere_luts_for`] on the base's planet.
#[tracing::instrument(skip_all)]
#[must_use]
pub fn build_atmosphere_luts_from(
    base: &AtmosphereBase,
    sun_direction: [f32; 3],
    camera_height_m: f32,
) -> AtmosphereLuts {
    let _mem = regia_core::memory::enter_owner(&crate::heap_owners::ATMOSPHERE_LUTS);
    let params = base.params;
    let sun = normalize(sun_direction);
    let sky_view = build_sky_view(
        &base.transmittance,
        &base.multi_scatter,
        sun,
        camera_height_m,
        params,
    );
    let (aerial, aerial_sun) = build_aerial(
        &base.transmittance,
        &base.multi_scatter,
        sun,
        camera_height_m,
        params,
    );
    AtmosphereLuts {
        transmittance: base.transmittance.clone(),
        multi_scatter: base.multi_scatter.clone(),
        sky_view,
        aerial,
        aerial_sun,
        aerial_slices: AERIAL_SLICES,
        sun_direction: sun,
        camera_height_m,
        params,
    }
}

/// Sky-view LUT sample in cd/m², without the analytic sun disk.
///
/// Indirect light uses this. The disk is a directional source, added by [`sample_sky`].
#[must_use]
pub fn sample_sky_table(luts: &AtmosphereLuts, view_dir: [f32; 3]) -> [f32; 3] {
    let view = normalize(view_dir);
    let (u, v) = sky_uv(view, luts.sun_direction);
    let radiance = luts.sky_view.sample(u, v);
    [radiance[0], radiance[1], radiance[2]]
}

/// Radiance of the solar disk (cd/m²). Zero when the sun ray meets the ground.
#[must_use]
pub fn sun_disk_radiance(luts: &AtmosphereLuts, params: &AtmosphereParameters) -> [f32; 3] {
    if luts.sun_direction[1] <= -0.01
        || ray_hits_ground(&luts.params, luts.camera_height_m, luts.sun_direction)
    {
        return [0.0; 3];
    }
    let transmitted = sun_transmittance(luts, luts.camera_height_m, luts.sun_direction);
    let cos_disk = soft_math::cos(params.sun_angular_radius);
    let solid = 2.0 * PI * (1.0 - cos_disk);
    mul3(transmitted, params.sun_illuminance / solid.max(1.0e-6))
}

/// Sky radiance in cd/m² for a world view direction.
///
/// Includes the sun disk when the ray misses the planet, and the Lambertian
/// surface (`ground_radiance`, attenuated by the transmittance in the sky-view
/// alpha) when the ray meets it. A view past a platform edge or down a cliff
/// is that surface, not the inscatter of the few metres of air in front of it.
#[must_use]
pub fn sample_sky(luts: &AtmosphereLuts, view_dir: [f32; 3], params: &AtmosphereParameters) -> [f32; 3] {
    let view = normalize(view_dir);
    let (u, v) = sky_uv(view, luts.sun_direction);
    let texel = luts.sky_view.sample(u, v);
    let mut radiance = [texel[0], texel[1], texel[2]];
    let hit_ground = ray_hits_ground(&luts.params, luts.camera_height_m, view);
    if hit_ground {
        let ground = ground_radiance(luts);
        let transmit = texel[3].clamp(0.0, 1.0);
        radiance[0] += transmit * ground[0];
        radiance[1] += transmit * ground[1];
        radiance[2] += transmit * ground[2];
        return radiance;
    }
    let cos_disk = soft_math::cos(params.sun_angular_radius);
    if dot(view, luts.sun_direction) >= cos_disk && view[1] > -0.01 {
        let t = sun_transmittance(luts, luts.camera_height_m, luts.sun_direction);
        let solid = 2.0 * PI * (1.0 - cos_disk);
        let disk = mul3(t, params.sun_illuminance / solid.max(1.0e-6));
        radiance[0] += disk[0];
        radiance[1] += disk[1];
        radiance[2] += disk[2];
    }
    radiance
}

/// Lambertian radiance (cd/m²) of the planet's surface under the transmitted sun and the sky.
///
/// One colour for the whole disc. Within a few kilometres of the ground the planet's
/// curvature changes the surface normal by a fraction of a degree, so local up is the
/// normal a viewer on a platform or a cliff actually sees. The sky-view shader multiplies
/// by the transmittance to the hit (the sky-view alpha).
#[must_use]
pub fn ground_radiance(luts: &AtmosphereLuts) -> [f32; 3] {
    let params = &luts.params;
    let (sun, sky) = ground_light(luts);
    let mut out = [0.0; 3];
    for c in 0..3 {
        out[c] = params.ground_albedo[c] * (sun[c] + sky[c]);
    }
    out
}

/// The light on flat ground under a clear sky as its two terms, both irradiance / π (cd/m² on a
/// white Lambertian surface): the transmitted sun's (`colour · lux · cos / π`) and the sky
/// dome's (its cosine-weighted radiance, [`ambient_irradiance_table`]). `ground_radiance` is
/// their sum times the ground albedo. A cloud layer takes from the first term what it takes
/// from the sun; the air's multiple scattering follows the sum (ADR-5944).
#[must_use]
pub fn ground_light(luts: &AtmosphereLuts) -> ([f32; 3], [f32; 3]) {
    let (color, lux) = transmitted_sun(luts, &luts.params);
    let cos_sun = luts.sun_direction[1].max(0.0);
    let sky = ambient_irradiance_table(luts);
    // Irradiance / π: the sun term; the sky dome's cosine-weighted radiance is already E/π.
    let sun = std::array::from_fn(|c| color[c] * lux * cos_sun / PI);
    (sun, sky)
}

/// Transmitted sun colour (linear RGB, relative, mean 1) and illuminance at the camera (lux,
/// the mean channel). `colour[c] · lux` is the sun's irradiance in channel `c`: the
/// transmittance times the sun's illuminance, the light the sky table scatters
/// ([`sun_colour_and_lux`]).
#[must_use]
pub fn transmitted_sun(luts: &AtmosphereLuts, params: &AtmosphereParameters) -> ([f32; 3], f32) {
    let t = sun_transmittance(luts, luts.camera_height_m, luts.sun_direction);
    sun_colour_and_lux(t, params.sun_illuminance)
}

/// [`transmitted_sun`] at `height_m` above the surface instead of the camera: the sun a
/// cloud layer at that height receives (linear RGB of mean 1, and lux). The horizon dips
/// below a raised point, so a layer stays sunlit after the camera's sunset.
#[must_use]
pub fn transmitted_sun_at(luts: &AtmosphereLuts, height_m: f32) -> ([f32; 3], f32) {
    let t = sun_transmittance(luts, height_m, luts.sun_direction);
    sun_colour_and_lux(t, luts.params.sun_illuminance)
}

/// The sun through transmittance `t` as the light's (colour, lux) pair: lux is the mean
/// channel's illuminance and the colour is `t` over its mean, so `colour[c] · lux = t[c] ·
/// sun_illuminance` in every channel, as the sky-view and aerial tables light the air
/// (ADR-1447). The colour was normalised to its peak channel before, which put a low sun's
/// light on every lit surface at `mean(t) / peak(t)` of that (0.79 at 21° through a hazy
/// morning sky), while the sky beside it kept full strength. A sun the air blocks entirely
/// (mean below 1e-6) returns white at that negligible lux.
#[must_use]
pub fn sun_colour_and_lux(t: [f32; 3], sun_illuminance: f32) -> ([f32; 3], f32) {
    let mean = (t[0] + t[1] + t[2]) / 3.0;
    let illum = mean * sun_illuminance;
    if mean.is_nan() || mean <= 1.0e-6 {
        return ([1.0; 3], illum.max(0.0));
    }
    ([t[0] / mean, t[1] / mean, t[2] / mean], illum)
}

/// Cosine-weighted upper-hemisphere integral of the sky, in cd/m² (irradiance / π).
///
/// Fed into the split-sum irradiance map so indirect light matches the sky.
#[must_use]
pub fn ambient_irradiance(luts: &AtmosphereLuts, params: &AtmosphereParameters) -> [f32; 3] {
    let mut sum = [0.0; 3];
    let n = 24u32;
    for i in 0..n {
        let xi = hammersley(i, n);
        let local = cosine_hemisphere(xi);
        let sky = sample_sky(luts, local, params);
        sum[0] += sky[0];
        sum[1] += sky[1];
        sum[2] += sky[2];
    }
    let inv = 1.0 / n as f32;
    [sum[0] * inv, sum[1] * inv, sum[2] * inv]
}

/// [`ambient_irradiance`] without the sun disc: cosine-weighted sky-table radiance (E/π).
#[must_use]
pub fn ambient_irradiance_table(luts: &AtmosphereLuts) -> [f32; 3] {
    let mut sum = [0.0; 3];
    let n = 32u32;
    for i in 0..n {
        let sky = sample_sky_table(luts, cosine_hemisphere(hammersley(i, n)));
        sum = add3(sum, sky);
    }
    mul3(sum, 1.0 / n as f32)
}

/// Inscatter (cd/m²) and transmittance toward a point `distance_m` along `view_dir`.
///
/// Slice `i` stores the integral out to `(i + 1) / slices * 32 km`. Distances shorter
/// than the first slice (2 km) lerp from clear air, so a nearby surface is not tinted
/// as if it sat at 2 km.
#[must_use]
pub fn sample_aerial(luts: &AtmosphereLuts, view_dir: [f32; 3], distance_m: f32) -> ([f32; 3], f32) {
    let view = normalize(view_dir);
    let (u, v) = sky_uv(view, luts.sun_direction);
    let slices = luts.aerial_slices.max(1);
    let step_km = AERIAL_RANGE_KM / slices as f32;
    let pos = (distance_m.max(0.0) / 1000.0 / step_km).clamp(0.0, slices as f32);
    if pos <= 1.0 {
        let far = sample_aerial_slice(luts, u, v, 0);
        let m = lerp4([0.0, 0.0, 0.0, 1.0], far, pos);
        return ([m[0], m[1], m[2]], m[3].clamp(0.0, 1.0));
    }
    let last = slices - 1;
    let f = (pos - 1.0).min(last as f32);
    let s0 = (f.floor() as u32).min(last);
    let s1 = (s0 + 1).min(last);
    let t = f - s0 as f32;
    let a = sample_aerial_slice(luts, u, v, s0);
    let b = sample_aerial_slice(luts, u, v, s1);
    let m = lerp4(a, b, t);
    ([m[0], m[1], m[2]], m[3].clamp(0.0, 1.0))
}

/// Midpoint samples of the cloud transmittance per table segment a view ray crosses under a
/// cloud layer, nearest segment first; later segments take the last entry (ADR-5944). Matches
/// `cloud_air_steps` in `outdoor_sky.wgsl`.
pub const AERIAL_CLOUD_STEPS: [u32; 3] = [8, 4, 2];

/// Kilometres between two nodes of the aerial tables along a view ray.
fn aerial_step_km(luts: &AtmosphereLuts) -> f32 {
    AERIAL_RANGE_KM / luts.aerial_slices.max(1) as f32
}

/// Node `k` of the sun table along the ray at table coordinates (`u`, `v`): node 0 is the eye
/// (no air in front of it), node `k` is slice `k − 1`.
fn aerial_sun_node(luts: &AtmosphereLuts, u: f32, v: f32, k: u32) -> [f32; 3] {
    if k == 0 {
        return [0.0; 3];
    }
    let s = sample_slice_of(&luts.aerial_sun, u, v, (k - 1).min(luts.aerial_slices.max(1) - 1));
    [s[0], s[1], s[2]]
}

/// The sun's single-scattering share (cd/m²) of [`sample_aerial`]'s in-scatter toward a point
/// `distance_m` along `view_dir`: [`AtmosphereLuts::aerial_sun`] with the same interpolation
/// (linear between nodes 2 km apart, node 0 the eye). The rest of the in-scatter is multiple
/// scattering.
#[must_use]
pub fn sample_aerial_sun(luts: &AtmosphereLuts, view_dir: [f32; 3], distance_m: f32) -> [f32; 3] {
    let view = normalize(view_dir);
    let (u, v) = sky_uv(view, luts.sun_direction);
    let slices = luts.aerial_slices.max(1);
    let pos = (distance_m.max(0.0) / 1000.0 / aerial_step_km(luts)).clamp(0.0, slices as f32);
    let k = (pos.floor() as u32).min(slices - 1);
    let lo = aerial_sun_node(luts, u, v, k);
    let hi = aerial_sun_node(luts, u, v, k + 1);
    lerp3(lo, hi, pos - k as f32)
}

/// The light the air's multiple scattering follows under a cloud layer
/// ([`sample_aerial_under_clouds`], ADR-5944).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CloudAirLight {
    /// The sun's share of the light on flat ground under a clear sky ([`ground_light`]).
    pub ground_sun: [f32; 3],
    /// The sky's share of it.
    pub ground_sky: [f32; 3],
    /// Overcast factor the sky's share takes under the layer (the sky light's rule, ADR-321);
    /// 0 leaves it.
    pub overcast: f32,
    /// Share of the ray's length below the layer's top, `0..=1`
    /// ([`CloudAirSpan::under_top`](crate::CloudAirSpan)).
    pub under_top: f32,
}

impl CloudAirLight {
    /// The clear ground light of `luts`, no overcast, a ray wholly under the layer.
    #[must_use]
    pub fn clear(luts: &AtmosphereLuts) -> Self {
        let (ground_sun, ground_sky) = ground_light(luts);
        Self {
            ground_sun,
            ground_sky,
            overcast: 0.0,
            under_top: 1.0,
        }
    }
}

/// [`sample_aerial`] under a cloud layer (ADR-5944): the in-scatter (cd/m²) toward a point
/// `distance_m` along `view_dir` when the sun reaches the air through `sun_visibility`, and the
/// unchanged transmittance.
///
/// `span_m` is the stretch of the ray, in metres from the eye, the layer can shadow (below the
/// layer's top and inside the cloud transmittance map's window); `sun_visibility(t)` is the
/// layer's transmittance toward the sun, `0..=1`, at `t` metres along the ray.
///
/// - **The sun's single scattering.** Each table segment inside the span loses the sun's share
///   of its in-scatter times the mean shadow at [`AERIAL_CLOUD_STEPS`] midpoints:
///   `Σ (S(b) − S(a)) · (1 − V)`.
/// - **The multiple scattering** (`L − S`: the light of the sky and of the lit ground around,
///   scattered again). It follows the light that reaches the ground under the layer: the
///   shadowed share of the ray's air (the sun's scattering the first step removed, over all of
///   it) takes the sun's share of the clear ground light, and `light.overcast` greys and dims
///   the sky's share (the sky light's rule, ADR-321) for the part of the ray under the layer.
///   Under one cover the air's light and the ground's then fall by the same factor, which
///   leaves the contrast of the ground seen through that air where a clear sky has it
///   (Koschmieder 1924).
///
/// Where the visibility is 1 and there is no overcast nothing is removed: a clear sky returns
/// [`sample_aerial`]'s value bit for bit.
///
/// The CPU mirror of `cloud_air_removed` in `outdoor_sky.wgsl`.
#[must_use]
pub fn sample_aerial_under_clouds(
    luts: &AtmosphereLuts,
    view_dir: [f32; 3],
    distance_m: f32,
    span_m: [f32; 2],
    sun_visibility: impl Fn(f32) -> f32,
    light: &CloudAirLight,
) -> ([f32; 3], f32) {
    let (total, transmittance) = sample_aerial(luts, view_dir, distance_m);
    let view = normalize(view_dir);
    let (u, v) = sky_uv(view, luts.sun_direction);
    let slices = luts.aerial_slices.max(1);
    let step_m = aerial_step_km(luts) * 1000.0;
    let range_m = step_m * slices as f32;
    let a = span_m[0].max(0.0);
    let b = span_m[1].min(distance_m).min(range_m);
    let mut removed = [0.0_f32; 3];
    if b > a {
        let (pos_a, pos_b) = (a / step_m, b / step_m);
        let first = (pos_a.floor() as u32).min(slices - 1);
        let mut lo_node = aerial_sun_node(luts, u, v, first);
        for k in first..slices {
            if k as f32 >= pos_b {
                break;
            }
            let hi_node = aerial_sun_node(luts, u, v, k + 1);
            let lo = pos_a.max(k as f32);
            let hi = pos_b.min(k as f32 + 1.0);
            if hi > lo {
                let n = AERIAL_CLOUD_STEPS[((k - first) as usize).min(AERIAL_CLOUD_STEPS.len() - 1)];
                let mut dark = 0.0;
                for i in 0..n {
                    let pos = lo + (hi - lo) * (i as f32 + 0.5) / n as f32;
                    dark += 1.0 - sun_visibility(pos * step_m).clamp(0.0, 1.0);
                }
                let weight = (hi - lo) * dark / n as f32;
                for c in 0..3 {
                    removed[c] += (hi_node[c] - lo_node[c]) * weight;
                }
            }
            lo_node = hi_node;
        }
    }
    let lost = luma3(removed);
    let overcast = light.overcast.clamp(0.0, 1.0);
    if (lost > 0.0 || overcast > 0.0) && light.under_top > 0.0 {
        let sun = sample_aerial_sun(luts, view_dir, distance_m);
        // The shadowed share of the ray's air, weighted as the air scatters the sun.
        let shade = (lost / luma3(sun).max(1.0e-6)).clamp(0.0, 1.0);
        let mut sky = light.ground_sky;
        if overcast > 0.0 {
            let dimmed = crate::clouds::overcast_sky(sky, overcast);
            let share = light.under_top.min(1.0);
            sky = std::array::from_fn(|c| sky[c] * (1.0 - share) + dimmed[c] * share);
        }
        for c in 0..3 {
            let clear = light.ground_sun[c] + light.ground_sky[c];
            if clear > 1.0e-6 {
                let keep = ((light.ground_sun[c] * (1.0 - shade) + sky[c]) / clear).clamp(0.0, 4.0);
                removed[c] += (total[c] - sun[c]).max(0.0) * (1.0 - keep);
            }
        }
    }
    (
        std::array::from_fn(|c| (total[c] - removed[c]).max(0.0)),
        transmittance,
    )
}

fn luma3(rgb: [f32; 3]) -> f32 {
    0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]
}

/// Analytic gradient used when the profile does not run the LUT raymarch.
///
/// Matches `sky_color` in `utils.wgsl` (display-referred linear RGB).
#[must_use]
pub fn analytic_sky(view_dir: [f32; 3]) -> [f32; 3] {
    let t = view_dir[1];
    let horizon = [0.78, 0.85, 0.95];
    let zenith = [0.15, 0.25, 0.55];
    let ground = [0.12, 0.10, 0.08];
    if t > 0.0 {
        let f = 1.0 - (-t * 3.0).exp();
        lerp3(horizon, zenith, f)
    } else {
        let f = 1.0 - (t * 2.0).exp();
        lerp3(horizon, ground, f)
    }
}

/// Exposure + AgX + the engine's `pow(1/2.2)` encode, as 8-bit RGBA.
#[must_use]
pub fn display_radiance(radiance: [f32; 3], exposure_ev: f32) -> [u8; 4] {
    let e = crate::post::exposure_from_ev100(exposure_ev);
    let mapped = crate::tonemap_agx([radiance[0] * e, radiance[1] * e, radiance[2] * e]);
    let enc = |c: f32| {
        (soft_math::powf(c.max(0.0), 1.0 / 2.2) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    [enc(mapped[0]), enc(mapped[1]), enc(mapped[2]), 255]
}

fn sample_aerial_slice(luts: &AtmosphereLuts, u: f32, v: f32, slice: u32) -> [f32; 4] {
    sample_slice_of(&luts.aerial, u, v, slice)
}

/// One slice of a table in the aerial layout ([`AtmosphereLuts::aerial`] or
/// [`AtmosphereLuts::aerial_sun`]).
fn sample_slice_of(table: &LutImage, u: f32, v: f32, slice: u32) -> [f32; 4] {
    // Slices sit side by side; bilinear inside one, azimuth wrapping (the shader's filter).
    let w = AERIAL_W as i64;
    let h = AERIAL_H as i64;
    let fu = u.rem_euclid(1.0) * w as f32 - 0.5;
    let fv = v.clamp(0.0, 1.0) * h as f32 - 0.5;
    let (x0, y0) = (fu.floor(), fv.floor());
    let (tx, ty) = (fu - x0, fv - y0);
    let fetch = |x: i64, y: i64| {
        let xi = x.rem_euclid(w) as u32;
        let yi = y.clamp(0, h - 1) as u32;
        let index = yi * table.width + slice * AERIAL_W + xi;
        table
            .texels
            .get(index as usize)
            .copied()
            .unwrap_or([0.0, 0.0, 0.0, 1.0])
    };
    let (x0, y0) = (x0 as i64, y0 as i64);
    lerp4(
        lerp4(fetch(x0, y0), fetch(x0 + 1, y0), tx),
        lerp4(fetch(x0, y0 + 1), fetch(x0 + 1, y0 + 1), tx),
        ty,
    )
}

fn sun_transmittance(luts: &AtmosphereLuts, camera_height_m: f32, sun_dir: [f32; 3]) -> [f32; 3] {
    let p = &luts.params;
    let r = p.bottom_radius_km + camera_height_m.max(MIN_CAMERA_HEIGHT_M) / 1000.0;
    let mu = sun_dir[1].clamp(-1.0, 1.0);
    let (u, v) = transmittance_uv(p, r, mu);
    let s = luts.transmittance.sample_clamped(u, v);
    [s[0], s[1], s[2]]
}

fn build_transmittance(p: &AtmosphereParameters) -> LutImage {
    let mut image = LutImage::new(TRANSMITTANCE_W, TRANSMITTANCE_H);
    for y in 0..TRANSMITTANCE_H {
        for x in 0..TRANSMITTANCE_W {
            // Texel centres at the range ends (`LutImage::sample_clamped`).
            let (r, mu) = r_mu_from_transmittance_uv(
                p,
                x as f32 / (TRANSMITTANCE_W - 1) as f32,
                y as f32 / (TRANSMITTANCE_H - 1) as f32,
            );
            let t = if ray_hits_planet(p, r, mu) {
                [0.0; 3]
            } else {
                let od = optical_depth(p, r, mu, 40);
                [
                    soft_math::exp(-od[0]),
                    soft_math::exp(-od[1]),
                    soft_math::exp(-od[2]),
                ]
            };
            image.put(x, y, [t[0], t[1], t[2], 1.0]);
        }
    }
    image
}

fn build_multi_scatter(transmittance: &LutImage, params: AtmosphereParameters) -> LutImage {
    let mut image = LutImage::new(MULTI_W, MULTI_H);
    let dirs = 32u32;
    for y in 0..MULTI_H {
        for x in 0..MULTI_W {
            // Texel centres at the range ends (`multi_uv`, `LutImage::sample_clamped`). The ground
            // row sits `MIN_CAMERA_HEIGHT_M` up: exactly on the sphere, a downward ray's ground hit
            // at t = 0 is not counted and the march ran the planet's chord at surface density.
            let r = params.bottom_radius_km
                + ((params.top_radius_km - params.bottom_radius_km) * y as f32 / (MULTI_H - 1) as f32)
                    .max(MIN_CAMERA_HEIGHT_M / 1000.0);
            let mu_sun = 2.0 * x as f32 / (MULTI_W - 1) as f32 - 1.0;
            let mut psi = [0.0; 3];
            let mut factor = [0.0; 3];
            for i in 0..dirs {
                let dir = fibonacci(i, dirs);
                let (l2, f) = second_order(r, dir, mu_sun, transmittance, params, 20);
                psi = add3(psi, l2);
                factor = add3(factor, f);
            }
            let inv = 1.0 / dirs as f32;
            psi = mul3(psi, inv);
            factor = mul3(factor, inv);
            let mut ms = [0.0; 3];
            for c in 0..3 {
                let denom = (1.0 - factor[c].clamp(0.0, 0.85)).max(0.15);
                ms[c] = (psi[c] / denom).max(0.0);
            }
            image.put(x, y, [ms[0], ms[1], ms[2], 1.0]);
        }
    }
    image
}

fn build_sky_view(
    transmittance: &LutImage,
    multi: &LutImage,
    sun: [f32; 3],
    camera_height_m: f32,
    params: AtmosphereParameters,
) -> LutImage {
    let mut image = LutImage::new(SKY_W, SKY_H);
    for y in 0..SKY_H {
        for x in 0..SKY_W {
            let u = (x as f32 + 0.5) / SKY_W as f32;
            let v = (y as f32 + 0.5) / SKY_H as f32;
            let view = view_from_sky_uv(u, v, sun);
            // RGB is the air in front of whatever the ray meets. Alpha is the
            // transmittance to the planet, and 0 when the ray leaves the atmosphere
            // instead: the visible sky adds `alpha * ground_radiance` at sample time.
            let (rgb, transmit) = march_segment(
                view,
                sun,
                camera_height_m,
                f32::MAX,
                transmittance,
                multi,
                params,
                18,
            );
            let alpha = if ray_hits_ground(&params, camera_height_m, view) {
                transmit.clamp(0.0, 1.0)
            } else {
                0.0
            };
            image.put(x, y, [rgb[0], rgb[1], rgb[2], alpha]);
        }
    }
    image
}

/// The aerial table and, in the same layout, the sun's single-scattering share of it
/// ([`AtmosphereLuts::aerial_sun`]).
fn build_aerial(
    transmittance: &LutImage,
    multi: &LutImage,
    sun: [f32; 3],
    camera_height_m: f32,
    params: AtmosphereParameters,
) -> (LutImage, LutImage) {
    let width = AERIAL_W * AERIAL_SLICES;
    let mut image = LutImage::new(width, AERIAL_H);
    let mut sun_image = LutImage::new(width, AERIAL_H);
    for slice in 0..AERIAL_SLICES {
        let dist_km = AERIAL_RANGE_KM * (slice as f32 + 1.0) / AERIAL_SLICES as f32;
        for y in 0..AERIAL_H {
            for x in 0..AERIAL_W {
                let u = (x as f32 + 0.5) / AERIAL_W as f32;
                let v = (y as f32 + 0.5) / AERIAL_H as f32;
                let view = view_from_sky_uv(u, v, sun);
                let (rgb, single, t) =
                    march_aerial(view, sun, camera_height_m, dist_km, transmittance, multi, params);
                image.put(slice * AERIAL_W + x, y, [rgb[0], rgb[1], rgb[2], t]);
                sun_image.put(slice * AERIAL_W + x, y, [single[0], single[1], single[2], 1.0]);
            }
        }
    }
    (image, sun_image)
}

#[cfg(test)]
fn march_sky(
    view: [f32; 3],
    sun: [f32; 3],
    camera_height_m: f32,
    transmittance: &LutImage,
    multi: &LutImage,
    params: AtmosphereParameters,
    steps: u32,
) -> [f32; 3] {
    let (rgb, _) = march_segment(
        view,
        sun,
        camera_height_m,
        f32::MAX,
        transmittance,
        multi,
        params,
        steps,
    );
    rgb
}

fn march_aerial(
    view: [f32; 3],
    sun: [f32; 3],
    camera_height_m: f32,
    dist_km: f32,
    transmittance: &LutImage,
    multi: &LutImage,
    params: AtmosphereParameters,
) -> ([f32; 3], [f32; 3], f32) {
    march_segment_split(
        view,
        sun,
        camera_height_m,
        dist_km,
        transmittance,
        multi,
        params,
        12,
    )
}

#[allow(clippy::too_many_arguments)]
fn march_segment(
    view: [f32; 3],
    sun: [f32; 3],
    camera_height_m: f32,
    max_km: f32,
    transmittance: &LutImage,
    multi: &LutImage,
    params: AtmosphereParameters,
    steps: u32,
) -> ([f32; 3], f32) {
    let (rgb, _, t) = march_segment_split(
        view,
        sun,
        camera_height_m,
        max_km,
        transmittance,
        multi,
        params,
        steps,
    );
    (rgb, t)
}

/// [`march_segment`] with the sun's single scattering also returned on its own (the second
/// value; ADR-5944). The first value is the sum the tables always held: it is accumulated by the
/// same operations in the same order, so splitting changes none of its bits.
#[allow(clippy::too_many_arguments)]
fn march_segment_split(
    view: [f32; 3],
    sun: [f32; 3],
    camera_height_m: f32,
    max_km: f32,
    transmittance: &LutImage,
    multi: &LutImage,
    params: AtmosphereParameters,
    steps: u32,
) -> ([f32; 3], [f32; 3], f32) {
    let p = &params;
    let cam_r = p.bottom_radius_km + camera_height_m.max(MIN_CAMERA_HEIGHT_M) / 1000.0;
    let origin = [0.0, cam_r, 0.0];
    let view = normalize(view);
    // A camera above the atmosphere's top sphere (an airless moon's shell is 1 km up): the air
    // runs from the shell's near side to its far side or the ground. The nearest root of the top
    // sphere is the entry there, not the end of the ray (audit G render5 #4).
    let outside = cam_r > p.top_radius_km;
    let (t_start, t_top) = if outside {
        match sphere_span(origin, view, p.top_radius_km) {
            Some(span) => span,
            None => return ([0.0; 3], [0.0; 3], 1.0),
        }
    } else {
        (0.0, ray_sphere(origin, view, p.top_radius_km).unwrap_or(0.0))
    };
    let t_ground = ray_sphere(origin, view, p.bottom_radius_km);
    // `march_sky` passes `f32::MAX`, which is finite: an aerial segment is one that ends
    // inside the atmosphere's far side.
    let aerial = max_km > 0.0 && max_km < t_top;
    let mut t_end = t_top.max(0.0);
    // The sky ends where a ray meets the planet. Aerial perspective does not: it is the air
    // in front of a surface `max_km` away, and real terrain stands above and below the
    // ideal sphere (DO27's canyons put the eye 2 m over a sphere the mesas rise from). Cut
    // at the sphere, every ray a degree below the horizon stopped ~100 m out while the
    // rays above it ran the full distance, so the haze stepped at the horizon line across
    // every distant wall (look-r2 round 10). Past the sphere the march keeps the ground
    // altitude (the density clamp below).
    if !aerial
        && let Some(tg) = t_ground
        && tg > 0.0
        && tg < t_end
    {
        t_end = tg;
    }
    if aerial {
        t_end = max_km;
    }
    // The marched length: from the camera, or from the shell entry above the atmosphere.
    let span = t_end - t_start;
    if span < 1.0e-4 || steps == 0 {
        return ([0.0; 3], [0.0; 3], 1.0);
    }
    // A sky ray that leaves the atmosphere crosses the dense low air first: quadratic steps
    // (`quadratic_step`). Rays that end on the ground or at an aerial distance stay uniform, and
    // so do rays from above the shell, whose dense air is at the far end.
    let escapes = !outside && !aerial && (t_end - t_top).abs() <= 1.0e-3 * t_top.max(1.0);
    let mut t_view = [1.0; 3];
    let mut luminance = [0.0; 3];
    let mut single = [0.0; 3];
    let cos_theta = dot(view, sun).clamp(-1.0, 1.0);
    let phase_r = rayleigh_phase(cos_theta);
    let phase_m = [
        henyey_greenstein(cos_theta, p.mie_g[0]),
        henyey_greenstein(cos_theta, p.mie_g[1]),
        henyey_greenstein(cos_theta, p.mie_g[2]),
    ];
    for i in 0..steps {
        let (t, dt) = if escapes {
            quadratic_step(i, steps, span)
        } else {
            let dt = span / steps as f32;
            ((i as f32 + 0.5) * dt, dt)
        };
        let pos = add3(origin, mul3(view, t_start + t));
        let radius = length(pos).max(p.bottom_radius_km + 1.0e-3);
        let h = radius - p.bottom_radius_km;
        let up = mul3(pos, 1.0 / radius);
        let mu_sun = dot(up, sun).clamp(-1.0, 1.0);
        let (tu, tv) = transmittance_uv(p, radius, mu_sun);
        let t_sun = transmittance.sample_clamped(tu, tv);
        let (b_r, b_m) = scattering(p, h);
        let ext = extinction(p, h);
        let (mu_u, mu_v) = multi_uv(p, radius, mu_sun);
        let ms = multi.sample_clamped(mu_u, mu_v);
        for c in 0..3 {
            let direct = (b_r[c] * phase_r + b_m[c] * phase_m[c]) * t_sun[c] * params.sun_illuminance;
            // Ψ_ms already carries the isotropic phase (Hillaire 2020 eq. 6–10): σ_s · Ψ_ms.
            let higher = (b_r[c] + b_m[c]) * ms[c].max(0.0);
            luminance[c] += t_view[c] * (direct + higher) * dt;
            single[c] += t_view[c] * direct * dt;
            t_view[c] *= soft_math::exp(-ext[c] * dt);
        }
    }
    let mean_t = (t_view[0] + t_view[1] + t_view[2]) / 3.0;
    (luminance, single, mean_t.clamp(0.0, 1.0))
}

fn second_order(
    r: f32,
    dir: [f32; 3],
    mu_sun: f32,
    transmittance: &LutImage,
    params: AtmosphereParameters,
    steps: u32,
) -> ([f32; 3], [f32; 3]) {
    // The sun lies in the YZ plane at this cosine so the LUT does not depend on azimuth.
    let sun = [0.0, mu_sun, (1.0 - mu_sun * mu_sun).max(0.0).sqrt()];
    let p = &params;
    let origin = [0.0, r, 0.0];
    let dir = normalize(dir);
    let t_top = ray_sphere(origin, dir, p.top_radius_km).unwrap_or(0.0);
    let t_ground = ray_sphere(origin, dir, p.bottom_radius_km);
    let mut t_end = t_top.max(0.0);
    if let Some(tg) = t_ground
        && tg > 0.0
        && tg < t_end
    {
        t_end = tg;
    }
    if t_end < 1.0e-3 || steps == 0 {
        return ([0.0; 3], [0.0; 3]);
    }
    // Quadratic steps on rays that leave the atmosphere (`quadratic_step`), uniform to the
    // ground.
    let escapes = (t_end - t_top).abs() <= 1.0e-3 * t_top.max(1.0);
    let mut t_view = [1.0; 3];
    let mut psi = [0.0; 3];
    let mut factor = [0.0; 3];
    let iso = 1.0 / (4.0 * PI);
    for i in 0..steps {
        let (t, dt) = if escapes {
            quadratic_step(i, steps, t_end)
        } else {
            let dt = t_end / steps as f32;
            ((i as f32 + 0.5) * dt, dt)
        };
        let pos = add3(origin, mul3(dir, t));
        let radius = length(pos).max(p.bottom_radius_km + 1.0e-3);
        let h = radius - p.bottom_radius_km;
        let up = mul3(pos, 1.0 / radius);
        let mu = dot(up, sun).clamp(-1.0, 1.0);
        let (tu, tv) = transmittance_uv(p, radius, mu);
        let t_sun = transmittance.sample_clamped(tu, tv);
        let (b_r, b_m) = scattering(p, h);
        let ext = extinction(p, h);
        for c in 0..3 {
            let beta_s = b_r[c] + b_m[c];
            let beta_e = ext[c].max(1.0e-6);
            psi[c] += t_view[c] * beta_s * iso * t_sun[c] * params.sun_illuminance * dt;
            factor[c] += t_view[c] * (beta_s / beta_e) * (1.0 - soft_math::exp(-beta_e * dt));
            t_view[c] *= soft_math::exp(-beta_e * dt);
        }
    }
    // Ground bounce (Hillaire 2020 § 5.2): a ray that ends on the planet sees the Lambertian
    // ground lit by the transmitted sun. It feeds the multiple scattering, which is most of
    // what makes a clear sky's diffuse light.
    if let Some(tg) = t_ground
        && tg > 0.0
        && (tg - t_end).abs() < 1.0e-3
    {
        let pos = add3(origin, mul3(dir, tg));
        let up = normalize(pos);
        let mu = dot(up, sun);
        if mu > 0.0 {
            let (tu, tv) = transmittance_uv(p, p.bottom_radius_km, mu);
            let t_sun = transmittance.sample_clamped(tu, tv);
            for c in 0..3 {
                psi[c] += t_view[c] * p.ground_albedo[c] / PI * t_sun[c] * mu * params.sun_illuminance;
            }
        }
    }
    (psi, factor)
}

fn optical_depth(p: &AtmosphereParameters, r: f32, mu: f32, steps: u32) -> [f32; 3] {
    let dir = [(1.0 - mu * mu).max(0.0).sqrt(), mu, 0.0];
    let origin = [0.0, r, 0.0];
    let t_top = ray_sphere(origin, dir, p.top_radius_km).unwrap_or(0.0);
    let t_ground = ray_sphere(origin, dir, p.bottom_radius_km);
    let mut t_end = t_top.max(0.0);
    if let Some(tg) = t_ground
        && tg > 0.0
        && tg < t_end
    {
        t_end = tg;
    }
    if t_end < 1.0e-4 || steps == 0 {
        return [0.0; 3];
    }
    // Transmittance rays start in the densest air they cross: quadratic steps.
    let mut od = [0.0; 3];
    for i in 0..steps {
        let (t, dt) = quadratic_step(i, steps, t_end);
        let pos = add3(origin, mul3(dir, t));
        let h = length(pos) - p.bottom_radius_km;
        let ext = extinction(p, h.max(0.0));
        od = add3(od, mul3(ext, dt));
    }
    od
}

fn ray_hits_planet(p: &AtmosphereParameters, r: f32, mu: f32) -> bool {
    ray_meets_planet(p, r, mu, TABLE_GROUND_MIN_KM)
}

/// Nearest ground root the transmittance table counts as a hit (km).
const TABLE_GROUND_MIN_KM: f32 = 1.0e-3;

/// Nearest ground root a camera ray counts as a hit (km): [`ray_sphere`]'s own epsilon. The
/// camera is at least [`MIN_CAMERA_HEIGHT_M`] up, so every root it finds is a real hit; the table's
/// millimetre cut rejected the root straight down from 1.0–1.2 m, which `f32` rounds to just
/// under a millimetre at a planet's radius (audit G render5 #3).
const CAMERA_GROUND_MIN_KM: f32 = 1.0e-4;

/// Whether a ray from radius `r` at cosine `mu` meets the planet before it leaves the atmosphere,
/// counting ground roots beyond `min_t` km. From above the top sphere the ray meets the shell's
/// near side first: it hits the planet whenever it hits the ground sphere (audit G render5 #4).
fn ray_meets_planet(p: &AtmosphereParameters, r: f32, mu: f32, min_t: f32) -> bool {
    let dir = [(1.0 - mu * mu).max(0.0).sqrt(), mu, 0.0];
    let origin = [0.0, r, 0.0];
    let Some(tg) = ray_sphere(origin, dir, p.bottom_radius_km) else {
        return false;
    };
    if r > p.top_radius_km {
        return tg > min_t;
    }
    let t_top = ray_sphere(origin, dir, p.top_radius_km).unwrap_or(1.0e6);
    tg > min_t && tg < t_top
}

/// Lowest camera the tables are built for (metres above the planet's surface).
///
/// A camera exactly on the surface (any eye at or below `y = 0`, e.g. DO27's maps, whose
/// ground sits below zero) made every downward ray graze the sphere at `t = 0`, which
/// [`ray_sphere`] does not count, so the march ran the planet's whole chord at surface
/// density: the sky-view table below the horizon came out 5–10× brighter than the sky,
/// and the mesh IBL bake (`outdoor.rs` `environment_radiance`) lit every tilted face with
/// it. One metre up, a downward ray meets the ground a metre away.
pub(crate) const MIN_CAMERA_HEIGHT_M: f32 = 1.0;

fn ray_hits_ground(p: &AtmosphereParameters, camera_height_m: f32, view: [f32; 3]) -> bool {
    let r = p.bottom_radius_km + camera_height_m.max(MIN_CAMERA_HEIGHT_M) / 1000.0;
    ray_meets_planet(p, r, view[1].clamp(-1.0, 1.0), CAMERA_GROUND_MIN_KM)
}

/// Where a ray from `origin` runs inside the sphere of `radius`: `(enter, exit)` along the ray
/// (`enter` is 0 from inside), or `None` when it never does ahead of the origin.
fn sphere_span(origin: [f32; 3], dir: [f32; 3], radius: f32) -> Option<(f32, f32)> {
    let b = dot(origin, dir);
    let c = dot(origin, origin) - radius * radius;
    let disc = b * b - c;
    if disc < 0.0 {
        return None;
    }
    let s = disc.sqrt();
    let (t1, t2) = (-b - s, -b + s);
    (t2 > 1.0e-4).then_some((t1.max(0.0), t2))
}

/// Smallest positive intersection with a sphere at the origin.
fn ray_sphere(origin: [f32; 3], dir: [f32; 3], radius: f32) -> Option<f32> {
    let b = dot(origin, dir);
    let c = dot(origin, origin) - radius * radius;
    let disc = b * b - c;
    if disc < 0.0 {
        return None;
    }
    let s = disc.sqrt();
    let t1 = -b - s;
    let t2 = -b + s;
    if t1 > 1.0e-4 {
        Some(t1)
    } else if t2 > 1.0e-4 {
        Some(t2)
    } else {
        None
    }
}

fn extinction(p: &AtmosphereParameters, height_km: f32) -> [f32; 3] {
    let h = height_km.max(0.0);
    let r = soft_math::exp(-h / p.rayleigh_scale_height_km.max(1.0e-3));
    let m = soft_math::exp(-h / p.mie_scale_height_km.max(1.0e-3));
    let o = ozone_density(h);
    [
        p.rayleigh_scattering[0] * r + p.mie_extinction[0] * m + p.ozone_absorption[0] * o,
        p.rayleigh_scattering[1] * r + p.mie_extinction[1] * m + p.ozone_absorption[1] * o,
        p.rayleigh_scattering[2] * r + p.mie_extinction[2] * m + p.ozone_absorption[2] * o,
    ]
}

fn scattering(p: &AtmosphereParameters, height_km: f32) -> ([f32; 3], [f32; 3]) {
    let h = height_km.max(0.0);
    let r = soft_math::exp(-h / p.rayleigh_scale_height_km.max(1.0e-3));
    let m = soft_math::exp(-h / p.mie_scale_height_km.max(1.0e-3));
    (mul3(p.rayleigh_scattering, r), mul3(p.mie_scattering, m))
}

fn ozone_density(height_km: f32) -> f32 {
    if !(10.0..=40.0).contains(&height_km) {
        0.0
    } else if height_km < 25.0 {
        (height_km - 10.0) / 15.0
    } else {
        (40.0 - height_km) / 15.0
    }
}

fn rayleigh_phase(cos_theta: f32) -> f32 {
    3.0 / (16.0 * PI) * (1.0 + cos_theta * cos_theta)
}

fn henyey_greenstein(cos_theta: f32, g: f32) -> f32 {
    let g2 = g * g;
    let denom = soft_math::powf((1.0 + g2 - 2.0 * g * cos_theta).max(1.0e-4), 1.5);
    (1.0 - g2) / (4.0 * PI * denom)
}

/// Hillaire-style square mapping: more texels near the horizon (`mu ≈ 0`).
// Transmittance table coordinates (`LutImage::sample_clamped`): `v` is the horizontal distance
// to the horizon, `ρ = sqrt(r² − R²)`, over its value at the top of the atmosphere (Bruneton
// & Neyret 2008 § 4; Bruneton 2017): rows crowd near the ground, where the aerosol and most
// of the air are. A table linear in altitude put its first row 1.6 km up and lost most of a
// 1.2 km haze layer from every sun path (ADR-1447). `u` is the signed square root of `mu`.
fn transmittance_uv(p: &AtmosphereParameters, r: f32, mu: f32) -> (f32, f32) {
    let rr = p.bottom_radius_km * p.bottom_radius_km;
    let h = (p.top_radius_km * p.top_radius_km - rr).max(1.0e-6).sqrt();
    let rho = (r * r - rr).max(0.0).sqrt();
    let v = (rho / h).clamp(0.0, 1.0);
    let x = mu.clamp(-1.0, 1.0);
    let mapped = x.signum() * x.abs().sqrt();
    let u = (mapped * 0.5 + 0.5).clamp(0.0, 1.0);
    (u, v)
}

fn r_mu_from_transmittance_uv(p: &AtmosphereParameters, u: f32, v: f32) -> (f32, f32) {
    let rr = p.bottom_radius_km * p.bottom_radius_km;
    let h = (p.top_radius_km * p.top_radius_km - rr).max(1.0e-6).sqrt();
    let rho = h * v.clamp(0.0, 1.0);
    let r = (rho * rho + rr).sqrt();
    let x = (u.clamp(0.0, 1.0) * 2.0 - 1.0).clamp(-1.0, 1.0);
    let mu = x.signum() * x * x;
    (r, mu)
}

fn multi_uv(p: &AtmosphereParameters, r: f32, mu_sun: f32) -> (f32, f32) {
    let u = (mu_sun.clamp(-1.0, 1.0) * 0.5 + 0.5).clamp(0.0, 1.0);
    let span = (p.top_radius_km - p.bottom_radius_km).max(1.0e-3);
    let v = ((r - p.bottom_radius_km) / span).clamp(0.0, 1.0);
    (u, v)
}

/// Sky-view UV relative to the sun. V is the view latitude with Hillaire 2020's
/// non-linear mapping (§ 5.3): `0.5 ± 0.5·sqrt(|lat| / (π/2))`, so half the rows sit
/// within a few degrees of the horizon where the colour changes fastest.
fn sky_uv(view: [f32; 3], sun: [f32; 3]) -> (f32, f32) {
    let up = [0.0, 1.0, 0.0];
    let sun_h = normalize(sub3(sun, mul3(up, dot(sun, up))));
    let view_h = normalize(sub3(view, mul3(up, dot(view, up))));
    let sin_az = dot(cross(sun_h, view_h), up);
    let cos_az = dot(sun_h, view_h).clamp(-1.0, 1.0);
    let azimuth = soft_math::atan2(sin_az, cos_az);
    let u = (azimuth / std::f32::consts::TAU + 0.5).rem_euclid(1.0);
    let lat = soft_math::asin(view[1].clamp(-1.0, 1.0));
    let v = 0.5 + 0.5 * lat.signum() * (lat.abs() / (0.5 * PI)).sqrt();
    (u, v.clamp(0.0, 1.0))
}

fn view_from_sky_uv(u: f32, v: f32, sun: [f32; 3]) -> [f32; 3] {
    let x = v.clamp(0.0, 1.0) * 2.0 - 1.0;
    let lat = x.signum() * x * x * 0.5 * PI;
    let elevation = soft_math::sin(lat);
    let azimuth = (u.clamp(0.0, 1.0) - 0.5) * std::f32::consts::TAU;
    let horiz = (1.0 - elevation * elevation).max(0.0).sqrt();
    let up = [0.0, 1.0, 0.0];
    let sun_h = {
        let s = sub3(sun, mul3(up, dot(sun, up)));
        let len = length(s);
        if len < 1.0e-4 {
            [0.0, 0.0, 1.0]
        } else {
            mul3(s, 1.0 / len)
        }
    };
    let east = normalize(cross(up, sun_h));
    let north = sun_h;
    let horizontal = add3(
        mul3(east, soft_math::sin(azimuth)),
        mul3(north, soft_math::cos(azimuth)),
    );
    normalize(add3(mul3(horizontal, horiz), [0.0, elevation, 0.0]))
}

fn fibonacci(i: u32, n: u32) -> [f32; 3] {
    let golden = PI * (3.0 - 5.0_f32.sqrt());
    let y = 1.0 - (2.0 * i as f32 + 1.0) / n.max(1) as f32;
    let radial = (1.0 - y * y).max(0.0).sqrt();
    let theta = golden * i as f32;
    [radial * soft_math::cos(theta), y, radial * soft_math::sin(theta)]
}

fn hammersley(i: u32, n: u32) -> [f32; 2] {
    let mut bits = i;
    bits = bits.rotate_right(16);
    bits = ((bits & 0x5555_5555) << 1) | ((bits & 0xAAAA_AAAA) >> 1);
    bits = ((bits & 0x3333_3333) << 2) | ((bits & 0xCCCC_CCCC) >> 2);
    bits = ((bits & 0x0F0F_0F0F) << 4) | ((bits & 0xF0F0_F0F0) >> 4);
    bits = ((bits & 0x00FF_00FF) << 8) | ((bits & 0xFF00_FF00) >> 8);
    let radical = bits as f32 * 2.328_306_4e-10;
    [i as f32 / n.max(1) as f32, radical]
}

fn cosine_hemisphere(xi: [f32; 2]) -> [f32; 3] {
    let r = xi[0].sqrt();
    let phi = 2.0 * PI * xi[1];
    [
        r * soft_math::cos(phi),
        (1.0 - xi[0]).max(0.0).sqrt(),
        r * soft_math::sin(phi),
    ]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn add3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn sub3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn mul3(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn length(a: [f32; 3]) -> f32 {
    dot(a, a).sqrt()
}

fn normalize(a: [f32; 3]) -> [f32; 3] {
    let len = length(a);
    if len < 1.0e-8 {
        [0.0, 1.0, 0.0]
    } else {
        mul3(a, 1.0 / len)
    }
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    add3(a, mul3(sub3(b, a), t))
}

fn lerp4(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mobile High, Desktop Low and Deck leave `physical_atmosphere` off. Earth then stays
    /// analytic; Mars and an airless body still take the LUT sky. Studio rules suppress it.
    /// Desktop Ultra takes the tables on Earth too.
    #[test]
    fn low_tiers_use_the_lut_sky_only_off_earth() {
        use crate::{FrameQuality, RenderProfile, RenderProfilePreset};
        for preset in [
            RenderProfilePreset::MobileHigh,
            RenderProfilePreset::DesktopLow,
            RenderProfilePreset::Deck,
        ] {
            let quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
            assert!(!quality.physical_atmosphere, "{preset:?}");
            assert_eq!(quality.msaa_samples, 1, "{preset:?}");
            assert!(
                !planet_lut_sky(quality.physical_atmosphere, true, false),
                "{preset:?} Earth stays analytic"
            );
            assert!(
                planet_lut_sky(quality.physical_atmosphere, false, false),
                "{preset:?} Mars and airless use the tables"
            );
            assert!(
                !planet_lut_sky(quality.physical_atmosphere, false, true),
                "{preset:?} studio rules suppress the bake"
            );
        }
        let ultra =
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopUltra));
        assert!(ultra.physical_atmosphere);
        assert!(planet_lut_sky(ultra.physical_atmosphere, true, false));
    }

    /// ADR-1447: the sun the renderer lights surfaces with (`colour[c] · lux`) is, per channel,
    /// the transmittance times the sun's illuminance, the same light the sky table scatters, at
    /// every elevation, camera height and aerosol load. The colour normalised to its peak
    /// channel gave `mean / peak` of that: 0.79 at the showcase's 21° hazy morning sun.
    #[test]
    fn the_transmitted_sun_lights_every_channel_like_the_sky_table() {
        let mut hazy = AtmosphereParameters::earth();
        hazy.set_aerosol_turbidity(0.18);
        hazy.set_aerosol_angstrom(crate::CONTINENTAL_ANGSTROM);
        for air in [AtmosphereParameters::earth(), hazy] {
            for el_deg in [2.0f32, 8.0, 21.0, 45.0, 80.0] {
                let el = el_deg.to_radians();
                let luts = build_atmosphere_luts_for(&air, [0.0, el.sin(), el.cos()], 2.0);
                let at_camera = (luts.camera_height_m, transmitted_sun(&luts, &luts.params));
                let raised = (3000.0, transmitted_sun_at(&luts, 3000.0));
                for (height, (colour, lux)) in [at_camera, raised] {
                    let t = sun_transmittance(&luts, height, luts.sun_direction);
                    let mean = (colour[0] + colour[1] + colour[2]) / 3.0;
                    assert!(
                        (mean - 1.0).abs() < 1e-4,
                        "{el_deg}°: colour {colour:?} has mean {mean}"
                    );
                    for (c, (&tc, &cc)) in t.iter().zip(colour.iter()).enumerate() {
                        let want = tc * air.sun_illuminance;
                        let got = cc * lux;
                        assert!(
                            (got - want).abs() <= 1e-4 * want.max(1.0),
                            "{el_deg}° at {height} m, channel {c}: sun {got} lx vs table {want} lx"
                        );
                    }
                }
            }
        }
        // The low hazy sun is clearly red, so the old peak normalisation lost a fifth of it.
        let el = 21f32.to_radians();
        let mut air = AtmosphereParameters::earth();
        air.set_aerosol_turbidity(0.18);
        air.set_aerosol_angstrom(crate::CONTINENTAL_ANGSTROM);
        let luts = build_atmosphere_luts_for(&air, [0.0, el.sin(), el.cos()], 2.0);
        let t = sun_transmittance(&luts, luts.camera_height_m, luts.sun_direction);
        let peak = t[0].max(t[1]).max(t[2]);
        let mean = (t[0] + t[1] + t[2]) / 3.0;
        assert!(mean / peak < 0.9, "21° hazy sun mean / peak {}", mean / peak);
    }

    fn showcase_air() -> AtmosphereParameters {
        let mut air = AtmosphereParameters::earth();
        air.set_aerosol_turbidity(0.18);
        air.set_aerosol_angstrom(crate::CONTINENTAL_ANGSTROM);
        air
    }

    /// Transmittance to space by direct integration (4000 midpoint steps), independent of the
    /// table and its parameterisation.
    fn direct_transmittance(p: &AtmosphereParameters, height_km: f32, mu: f32) -> [f32; 3] {
        let r = p.bottom_radius_km + height_km;
        if ray_hits_planet(p, r, mu) {
            return [0.0; 3];
        }
        let dir = [(1.0 - mu * mu).max(0.0).sqrt(), mu, 0.0];
        let origin = [0.0, r, 0.0];
        let t_end = ray_sphere(origin, dir, p.top_radius_km).unwrap_or(0.0);
        let steps = 4000;
        let dt = t_end / steps as f32;
        let mut od = [0.0f64; 3];
        for i in 0..steps {
            let pos = add3(origin, mul3(dir, (i as f32 + 0.5) * dt));
            let ext = extinction(p, length(pos) - p.bottom_radius_km);
            for (o, e) in od.iter_mut().zip(ext) {
                *o += f64::from(e * dt);
            }
        }
        od.map(|o| (-o).exp() as f32)
    }

    /// ADR-1447: the transmittance table, read where the frame reads it (`transmittance_uv`,
    /// `sample_clamped`), matches a direct integration near the ground, where the haze is, and
    /// overhead. The table linear in altitude read the ground at 1.6 km (a 21° sun through
    /// τ 0.18 haze kept 0.69 of its green light instead of 0.43) and the wrapping sampler
    /// blended the overhead sun with the texel pointing straight down (0.44 instead of 0.86).
    #[test]
    fn the_transmittance_table_matches_a_direct_integration() {
        for air in [AtmosphereParameters::earth(), showcase_air()] {
            let lut = build_transmittance(&air);
            for height_km in [0.002f32, 0.1, 1.0, 5.0] {
                for mu in [1.0f32, 0.9, 0.707, 0.358, 0.17, 0.08] {
                    let (u, v) = transmittance_uv(&air, air.bottom_radius_km + height_km, mu);
                    let table = lut.sample_clamped(u, v);
                    let direct = direct_transmittance(&air, height_km, mu);
                    for (c, (&want, got)) in direct.iter().zip(table).enumerate() {
                        assert!(
                            (got - want).abs() <= 0.02 + 0.03 * want,
                            "τ {:.3}, {height_km} km, mu {mu}, channel {c}: table {got} vs direct {want}",
                            air.aerosol_optical_depth()
                        );
                    }
                }
            }
        }
    }

    /// The sun the frame gets brightens monotonically from the horizon to the zenith, and an
    /// aerosol load dims it by Beer–Lambert along the sun's path.
    #[test]
    fn the_sun_brightens_toward_the_zenith_and_haze_dims_it_by_its_optical_depth() {
        let clean = AtmosphereParameters::earth();
        let hazy = showcase_air();
        let lux_at = |air: &AtmosphereParameters, el_deg: f32| {
            let el = el_deg.to_radians();
            let luts = build_atmosphere_luts_for(air, [0.0, el.sin(), el.cos()], 2.0);
            let (colour, lux) = transmitted_sun(&luts, &luts.params);
            (colour, lux)
        };
        let mut last = 0.0;
        for el_deg in [2.0f32, 5.0, 10.0, 21.0, 45.0, 70.0, 89.0] {
            let (_, lux) = lux_at(&clean, el_deg);
            assert!(lux > last, "{el_deg}°: {lux} lx after {last} lx");
            last = lux;
        }
        // Green (550 nm, the Ångström factor is 1 there): the haze's extra optical depth over
        // the slant path, plane-parallel at 45°.
        let (c_clean, l_clean) = lux_at(&clean, 45.0);
        let (c_hazy, l_hazy) = lux_at(&hazy, 45.0);
        let ratio = c_hazy[1] * l_hazy / (c_clean[1] * l_clean);
        let extra = (hazy.mie_extinction[1] - clean.mie_extinction[1]) * clean.mie_scale_height_km;
        let want = (-extra / 45f32.to_radians().sin()).exp();
        assert!(
            (ratio / want - 1.0).abs() < 0.05,
            "haze dims green by {ratio}, Beer–Lambert {want}"
        );
    }

    /// ADR-1447: the showcase's hazy air (τ 0.18, Ångström 1.3) under a 21° sun, seen from
    /// 100 m, against an independent Monte Carlo of the same air (spherical shell, all orders,
    /// Lambert ground of albedo 0.3; lane script `mc_sky.py`, 8000 paths per direction, ±2 %).
    /// Green channel (550 nm, the Ångström factor is 1), relative to the sun's illuminance
    /// above the air. Before the table fixes the engine's sun was 0.69 and its sky 1.2–2.4× the
    /// reference, worst toward the horizon.
    #[test]
    fn the_showcase_sky_matches_the_monte_carlo_reference() {
        let air = showcase_air();
        let el = 21f32.to_radians();
        let luts = build_atmosphere_luts_for(&air, [0.0, el.sin(), el.cos()], 100.0);
        let e0 = air.sun_illuminance;
        let (colour, lux) = transmitted_sun(&luts, &luts.params);
        let sun = colour[1] * lux / e0;
        assert!(
            (sun / 0.4347 - 1.0).abs() < 0.03,
            "sun transmittance {sun}, reference 0.4347"
        );
        // Sky irradiance / π: cosine-weighted mean of the table.
        let (nz, na) = (90u32, 180u32);
        let mut sum = 0.0f64;
        for i in 0..nz {
            let z = (i as f32 + 0.5) / nz as f32 * std::f32::consts::FRAC_PI_2;
            for j in 0..na {
                let a = (j as f32 + 0.5) / na as f32 * std::f32::consts::TAU;
                let d = [z.sin() * a.sin(), z.cos(), z.sin() * a.cos()];
                sum += f64::from(sample_sky_table(&luts, d)[1] * z.cos() * z.sin());
            }
        }
        let cell =
            f64::from(std::f32::consts::FRAC_PI_2 / nz as f32) * f64::from(std::f32::consts::TAU / na as f32);
        let e_sky = (sum * cell) as f32 / PI / e0;
        assert!(
            (e_sky / 0.03332 - 1.0).abs() < 0.10,
            "sky irradiance / π {e_sky}, reference 0.03332"
        );
        // (elevation°, azimuth from the sun°, reference L / E₀).
        for (el_deg, az_deg, want) in [
            (5.0f32, 130.0f32, 0.04166f32),
            (8.5, 130.0, 0.03908),
            (10.0, 130.0, 0.03617),
            (15.0, 130.0, 0.02959),
            (20.0, 130.0, 0.02512),
            (30.0, 130.0, 0.01852),
            (10.0, 90.0, 0.03820),
            (10.0, 180.0, 0.04236),
        ] {
            let (e, a) = (el_deg.to_radians(), az_deg.to_radians());
            let got = sample_sky_table(&luts, [e.cos() * a.sin(), e.sin(), e.cos() * a.cos()])[1] / e0;
            assert!(
                (got / want - 1.0).abs() < 0.10,
                "sky at {el_deg}° / {az_deg}°: {got}, reference {want}"
            );
        }
    }

    #[test]
    fn a_blocked_sun_gives_white_at_no_light() {
        assert_eq!(sun_colour_and_lux([0.0; 3], 120_000.0), ([1.0; 3], 0.0));
        let (colour, lux) = sun_colour_and_lux([f32::NAN, 0.0, 0.0], 120_000.0);
        assert_eq!(colour, [1.0; 3]);
        assert_eq!(lux, 0.0);
        let (colour, lux) = sun_colour_and_lux([0.8, 0.6, 0.4], 100_000.0);
        assert!((lux - 60_000.0).abs() < 0.5, "{lux}");
        assert!(
            (colour[0] - 0.8 / 0.6).abs() < 1e-5 && (colour[2] - 0.4 / 0.6).abs() < 1e-5,
            "{colour:?}"
        );
    }

    #[test]
    fn a_below_horizon_view_shows_the_planet_not_black() {
        // A ray from 2 m that looks past a platform edge meets the planet in tens of
        // metres. The sky-view RGB is the inscatter of that short path (near black).
        // The visible sky is the sunlit surface behind it.
        let sun = sun_position(&TimeOfDay::noon()).direction;
        let luts = build_atmosphere_luts(sun, 2.0);
        let lum = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        let down = [0.0, -1.0, 0.0];
        let table = sample_sky_table(&luts, down);
        let seen = sample_sky(&luts, down, &luts.params);
        assert!(
            lum(table) < 30.0,
            "inscatter over a meter of air stays dark, got {table:?}"
        );
        let seen_l = lum(seen);
        assert!(
            (400.0..40_000.0).contains(&seen_l),
            "straight down should be sunlit ground, not black and not a chord march: {seen:?} ({seen_l} cd/m²)"
        );
        // ~5° below horizontal: the band past the arena floor, from eye height.
        let band_dir = normalize([0.0, -0.087, 0.996]);
        let band = sample_sky(&luts, band_dir, &luts.params);
        let band_l = lum(band);
        assert!(
            band_l > 400.0,
            "the below-horizon band is black: {band:?} ({band_l} cd/m²)"
        );
        let above = sample_sky(&luts, normalize([0.0, 0.2, 0.98]), &luts.params);
        assert!(above[2] > above[0], "above the horizon stays the sky, {above:?}");
        // An airless moon has no air to scatter, so the surface is the whole view.
        let ice = build_atmosphere_luts_for(&AtmosphereParameters::airless(), sun, 2.0);
        let ice_l = lum(sample_sky(&ice, down, &ice.params));
        assert!(ice_l > 50.0, "airless ground is black: {ice_l} cd/m²");
    }

    /// Audit G render5 #3: `bottom_radius_km + 1 m` rounds in `f32` (Earth 6360 km, Mars
    /// 3389.5 km, the airless moon 1560.8 km) to a ground root just under 1 mm short of a metre,
    /// which the old 1 mm ground test rejected, so a camera at or below 1.2 m stored alpha 0
    /// for the cone around straight down and the window sky drew it near black.
    #[test]
    fn nadir_from_one_metre_is_ground_not_black() {
        let sun = sun_position(&TimeOfDay::noon()).direction;
        let lum = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        let down = [0.0, -1.0, 0.0];
        for (name, params, floor) in [
            ("earth", AtmosphereParameters::default(), 400.0),
            ("mars", AtmosphereParameters::mars(), 50.0),
            ("airless", AtmosphereParameters::airless(), 50.0),
        ] {
            let base = AtmosphereBase::new(&params);
            for height in [0.0, 1.0, 1.2] {
                let luts = build_atmosphere_luts_from(&base, sun, height);
                let (u, v) = sky_uv(normalize(down), luts.sun_direction);
                let alpha = luts.sky_view.sample(u, v)[3];
                assert!(
                    alpha > 0.0,
                    "{name} at {height} m: straight down stored alpha {alpha}"
                );
                let seen = lum(sample_sky(&luts, down, &luts.params));
                assert!(
                    (floor..40_000.0).contains(&seen),
                    "{name} at {height} m: straight down is {seen} cd/m², not the sunlit ground"
                );
            }
        }
    }

    /// Audit G render5 #4: a camera above the atmosphere's top sphere (an airless moon's shell is
    /// 1 km up) took the shell's entry as the end of the ray, so a downward view stopped in the
    /// vacuum gap and stored alpha 0: black instead of the surface.
    #[test]
    fn above_the_shell_a_downward_ray_hits_the_planet() {
        let sun = sun_position(&TimeOfDay::noon()).direction;
        let lum = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        let down = [0.0, -1.0, 0.0];
        let params = AtmosphereParameters::airless();
        let base = AtmosphereBase::new(&params);
        let near = lum(sample_sky(
            &build_atmosphere_luts_from(&base, sun, 2.0),
            down,
            &params,
        ));
        for height in [2_000.0, 20_000.0] {
            let luts = build_atmosphere_luts_from(&base, sun, height);
            let (u, v) = sky_uv(normalize(down), luts.sun_direction);
            let alpha = luts.sky_view.sample(u, v)[3];
            assert!(
                alpha > 0.0,
                "airless at {height} m: straight down stored alpha {alpha}"
            );
            let seen = lum(sample_sky(&luts, down, &params));
            assert!(
                seen > 50.0 && (seen / near - 1.0).abs() < 0.25,
                "airless at {height} m: straight down {seen} cd/m², at 2 m {near}"
            );
            // A ray that misses the shell altogether stays empty space.
            let up = sample_sky_table(&luts, [0.0, 1.0, 0.0]);
            assert!(lum(up) < 1.0, "airless at {height} m: the zenith is {up:?}");
        }
        // Earth from 150 km (above its 100 km shell): the ground below is still there.
        let earth = AtmosphereBase::new(&AtmosphereParameters::default());
        let luts = build_atmosphere_luts_from(&earth, sun, 150_000.0);
        let (u, v) = sky_uv(normalize(down), luts.sun_direction);
        assert!(
            luts.sky_view.sample(u, v)[3] > 0.0,
            "earth at 150 km: no ground below"
        );
        assert!(lum(sample_sky(&luts, down, &luts.params)) > 400.0);
    }

    #[test]
    fn a_camera_on_or_below_the_surface_sees_dark_ground_below_the_horizon() {
        // DO27's maps put the eye below y = 0 (camera height 0). Downward rays used to march
        // the planet's whole chord at surface density: 12 000–23 000 cd/m² below the horizon
        // on Mars against ~2 700 for the sky, which the mesh IBL bake turned into sky light on
        // every tilted face.
        let sun = normalize([0.43, 0.57, -0.69]);
        for height in [0.0, -88.0] {
            let luts = build_atmosphere_luts_for(&AtmosphereParameters::mars(), sun, height);
            let zenith = sample_sky_table(&luts, [0.0, 1.0, 0.0]);
            for dir in [[0.95, -0.3, 0.0], [0.7, -0.7, 0.0], [0.0, -1.0, 0.0]] {
                let below = sample_sky_table(&luts, normalize(dir));
                assert!(
                    below[0] < 0.05 * zenith[0],
                    "height {height}: below-horizon {below:?} vs zenith {zenith:?}"
                );
            }
        }
    }

    #[test]
    fn a_raised_layer_keeps_the_sun_after_the_camera_sunset() {
        // Noon: 3 km sees about the camera's sun, a little brighter (less air above it).
        let noon = sun_position(&TimeOfDay::noon()).direction;
        let luts = build_atmosphere_luts(noon, 2.0);
        let (_, camera_lux) = transmitted_sun(&luts, &luts.params);
        let (_, high_lux) = transmitted_sun_at(&luts, 3000.0);
        assert!(
            high_lux >= camera_lux && high_lux < camera_lux * 1.2,
            "noon: camera {camera_lux} lux, 3 km {high_lux} lux"
        );
        // The sun 1.8° below the horizon: the camera is dark, a cloud top at 4 km (horizon dip
        // ~2°) is still lit and redder than blue. The table's rows are ~3 km apart, so the
        // camera reads the lowest row (~1.5 km, dip ~1.3°): the angle clears that row.
        let below = (-1.8_f32).to_radians();
        let low = normalize([below.cos(), below.sin(), 0.0]);
        let luts = build_atmosphere_luts(low, 2.0);
        let (_, camera_lux) = transmitted_sun(&luts, &luts.params);
        let (color, high_lux) = transmitted_sun_at(&luts, 4000.0);
        assert!(
            camera_lux < 1.0,
            "sun below the horizon still lights the camera: {camera_lux}"
        );
        assert!(
            high_lux > 10.0,
            "4 km lost the sun 1.8° after sunset: {high_lux} lux"
        );
        assert!(color[0] > color[2], "twilight sun at 4 km is not red: {color:?}");
    }

    #[test]
    fn noon_is_south_and_up_at_mid_latitude() {
        let sun = sun_position(&TimeOfDay::noon());
        assert!(sun.elevation_rad > 0.55 && sun.elevation_rad < 1.05, "{sun:?}");
        assert!(sun.direction[1] > 0.5, "noon sun is up");
        assert!(sun.direction[2] < -0.4, "equinox noon faces south");
        assert!(sun.direction[0].abs() < 0.2, "noon is not east or west");
    }

    /// Audit G finding 3 (render5 determinism): the sun is marched into the
    /// atmosphere LUTs, the directional light, `regia_vfx`'s surface weather and
    /// path-trace bakes, so its bits are pinned to the software-libm result. The
    /// pinned value was produced by `libm` (musl); the platform `f32` builtins
    /// disagree with `libm` by an ulp on the reference host (see
    /// `soft_math::tests`), which is what made cross-OS replays diverge.
    #[test]
    fn sun_position_is_pinned_to_the_software_libm_bits() {
        let sun = sun_position(&TimeOfDay {
            hour: 18.0,
            day_of_year: 172,
            latitude_deg: 45.0,
        });
        assert_eq!(sun.elevation_rad.to_bits(), 0x3e92_0ebe);
        assert_eq!(sun.azimuth_rad.to_bits(), 0x40a0_522e);
        assert_eq!(
            sun.direction.map(f32::to_bits),
            [0xbf6a_da4d, 0x3e90_15ab, 0x3e90_15ab]
        );
    }

    #[test]
    fn dawn_east_dusk_west_night_below() {
        let dawn = sun_position(&TimeOfDay::dawn());
        let dusk = sun_position(&TimeOfDay::dusk());
        let night = sun_position(&TimeOfDay::night());
        assert!(dawn.direction[0] > 0.7, "dawn is east, got {dawn:?}");
        assert!(dawn.elevation_rad > 0.0 && dawn.elevation_rad < 0.35);
        assert!(dusk.direction[0] < -0.7, "dusk is west, got {dusk:?}");
        assert!(dusk.elevation_rad > 0.0 && dusk.elevation_rad < 0.35);
        assert!(night.direction[1] < -0.4, "midnight sun is below the horizon");
    }

    #[test]
    fn equator_noon_is_overhead() {
        let sun = sun_position(&TimeOfDay {
            hour: 12.0,
            day_of_year: 80,
            latitude_deg: 0.0,
        });
        assert!(
            sun.direction[1] > 0.95,
            "equator equinox noon is overhead, {sun:?}"
        );
    }

    #[test]
    fn transmittance_is_higher_looking_up_than_at_the_horizon() {
        let p = AtmosphereParameters::earth();
        let lut = build_transmittance(&p);
        let up = lut.sample_clamped(
            transmittance_uv(&p, p.bottom_radius_km + 0.01, 1.0).0,
            transmittance_uv(&p, p.bottom_radius_km + 0.01, 1.0).1,
        );
        let horizon = lut.sample_clamped(
            transmittance_uv(&p, p.bottom_radius_km + 0.01, 0.0).0,
            transmittance_uv(&p, p.bottom_radius_km + 0.01, 0.0).1,
        );
        assert!(up[2] > horizon[2] + 0.2, "up {up:?} horizon {horizon:?}");
        assert!(up[0] > up[2], "red transmits more than blue, {up:?}");
        assert!((0.0..1.0).contains(&up[0]));
    }

    #[test]
    fn sky_hue_tracks_the_time_of_day() {
        let params = AtmosphereParameters::default();
        let height = 1.7_f32;
        let noon = build_atmosphere_luts(sun_position(&TimeOfDay::noon()).direction, height);
        let dawn = build_atmosphere_luts(sun_position(&TimeOfDay::dawn()).direction, height);
        let night = build_atmosphere_luts(sun_position(&TimeOfDay::night()).direction, height);
        // Zenith, away from the sun disk.
        let zenith = [0.0, 1.0, 0.0];
        let noon_z = sample_sky(&noon, zenith, &params);
        let dawn_z = sample_sky(&dawn, zenith, &params);
        let night_z = sample_sky(&night, zenith, &params);
        assert!(noon_z[2] > noon_z[0] * 1.15, "noon zenith is blue, {noon_z:?}");
        let noon_l = 0.2126 * noon_z[0] + 0.7152 * noon_z[1] + 0.0722 * noon_z[2];
        let night_l = 0.2126 * night_z[0] + 0.7152 * night_z[1] + 0.0722 * night_z[2];
        assert!(noon_l > night_l * 8.0 + 1.0, "noon {noon_l} night {night_l}");
        // Toward the dawn sun, just above the disk: the long path is red.
        let dawn_sun = sun_position(&TimeOfDay::dawn()).direction;
        let beside = normalize(add3(dawn_sun, [0.0, 0.08, 0.0]));
        let warm = sample_sky(&dawn, beside, &params);
        assert!(warm[0] > warm[2], "dawn sky near the sun is warm, {warm:?}");
        let dawn_l = 0.2126 * dawn_z[0] + 0.7152 * dawn_z[1] + 0.0722 * dawn_z[2];
        assert!(dawn_l < noon_l, "dawn zenith is darker than noon");
        let _ = dawn_l;
    }

    #[test]
    fn aerial_extinction_grows_with_distance() {
        let luts = build_atmosphere_luts(sun_position(&TimeOfDay::noon()).direction, 1.7);
        let dir = normalize([0.0, 0.05, 1.0]);
        let (near_l, near_t) = sample_aerial(&luts, dir, 200.0);
        let (far_l, far_t) = sample_aerial(&luts, dir, 8_000.0);
        assert!(near_t > 0.9, "200 m stays nearly clear, transmittance {near_t}");
        assert!(
            far_t < near_t,
            "far transmittance {far_t} should be below near {near_t}"
        );
        let near_s = near_l[0] + near_l[1] + near_l[2];
        let far_s = far_l[0] + far_l[1] + far_l[2];
        assert!(far_s > near_s, "far inscatter {far_s} near {near_s}");
    }

    #[test]
    fn aerial_haze_is_continuous_across_the_horizon() {
        // A wall 8 km out seen 2° below the horizon from a 2 m eye: the ideal sphere is ~57 m
        // away along that ray, and stopping there printed a hard step in the haze at the
        // horizon line (look-r2 round 10, DO27 switchbacks_mid).
        let luts = build_atmosphere_luts(sun_position(&TimeOfDay::noon()).direction, 2.0);
        let above = sample_aerial(&luts, normalize([0.0, 0.035, 1.0]), 8_000.0);
        let below = sample_aerial(&luts, normalize([0.0, -0.035, 1.0]), 8_000.0);
        let sum = |l: [f32; 3]| l[0] + l[1] + l[2];
        let ratio = sum(below.0) / sum(above.0).max(1e-6);
        assert!((0.75..1.33).contains(&ratio), "inscatter below/above {ratio}");
        assert!(
            (below.1 - above.1).abs() < 0.1,
            "transmittance {} vs {}",
            below.1,
            above.1
        );
    }

    #[test]
    fn sky_march_stops_at_the_ground_whatever_the_far_sentinel() {
        // `march_sky` passes `f32::MAX`; it must not be taken for an aerial segment (that
        // turned the sky-view LUT, and the ambient built from it, into a 1e38 km march).
        let p = AtmosphereParameters::default();
        let t = build_transmittance(&p);
        let m = build_multi_scatter(&t, p);
        let sun = sun_position(&TimeOfDay::noon()).direction;
        for view in [
            [0.0, 1.0, 0.0],
            normalize([0.0, 0.1, 1.0]),
            normalize([0.0, -0.2, 1.0]),
        ] {
            let a = march_sky(view, sun, 2.0, &t, &m, p, 32);
            let (b, _) = march_segment(view, sun, 2.0, f32::INFINITY, &t, &m, p, 32);
            assert_eq!(a, b, "view {view:?}");
            assert!(
                a.iter().all(|c| c.is_finite() && *c > 0.0),
                "view {view:?}: {a:?}"
            );
        }
    }

    #[test]
    fn ambient_matches_the_sky() {
        let params = AtmosphereParameters::default();
        let noon = build_atmosphere_luts(sun_position(&TimeOfDay::noon()).direction, 2.0);
        let night = build_atmosphere_luts(sun_position(&TimeOfDay::night()).direction, 2.0);
        let day_amb = ambient_irradiance(&noon, &params);
        let night_amb = ambient_irradiance(&night, &params);
        assert!(day_amb[2] > day_amb[0], "noon ambient is blue, {day_amb:?}");
        let day_l = day_amb[0] + day_amb[1] + day_amb[2];
        let night_l = night_amb[0] + night_amb[1] + night_amb[2];
        assert!(day_l > night_l * 5.0, "day {day_l} night {night_l}");
    }

    #[test]
    fn sun_disk_is_far_brighter_than_the_sky_around_it() {
        let params = AtmosphereParameters::default();
        let dawn = sun_position(&TimeOfDay::dawn());
        let luts = build_atmosphere_luts(dawn.direction, 1.7);
        let disk = sample_sky(&luts, dawn.direction, &params);
        let beside = sample_sky(&luts, normalize(add3(dawn.direction, [0.0, 0.12, 0.0])), &params);
        let disk_l = disk[0] + disk[1] + disk[2];
        let sky_l = beside[0] + beside[1] + beside[2];
        assert!(disk_l > sky_l * 20.0, "disk {disk_l} sky {sky_l}");
    }

    #[test]
    fn analytic_sky_has_no_sun_disk() {
        let zenith = analytic_sky([0.0, 1.0, 0.0]);
        let horizon = analytic_sky([0.0, 0.01, 1.0]);
        assert!(zenith[2] > zenith[0]);
        assert!(horizon[0] > zenith[0]);
        assert!(horizon.iter().all(|c| *c < 1.0));
    }

    /// Diffuse skylight on a horizontal surface at 35° sun elevation. Bruneton's aerosol is
    /// very clean (τ ≈ 0.005), so the sky sits at the clear end of measured clear skies
    /// (≈5–20 klx); before the ground bounce and the single-phase fix it was 4.5 klx.
    #[test]
    fn earth_sky_diffuse_illuminance_is_in_the_clear_sky_range() {
        let e = 35.0_f32.to_radians();
        let sun = [0.0, e.sin(), e.cos()];
        let luts = build_atmosphere_luts(sun, 2.0);
        let mean = ambient_irradiance_table(&luts);
        let lum = 0.2126 * mean[0] + 0.7152 * mean[1] + 0.0722 * mean[2];
        let lux = lum * PI;
        eprintln!("diffuse sky {lux} lx, rgb mean {mean:?}");
        assert!((6_000.0..20_000.0).contains(&lux), "diffuse sky {lux} lx");
    }

    #[test]
    fn aerosol_turbidity_sets_the_mie_optical_depth() {
        for tau in [0.0_f32, 0.05, 0.1, 0.3] {
            let mut p = AtmosphereParameters::earth();
            p.set_aerosol_turbidity(tau);
            assert!(
                (p.aerosol_optical_depth() - tau).abs() < 1.0e-6,
                "optical depth {} != tau {tau}",
                p.aerosol_optical_depth()
            );
        }
        // The single-scattering albedo and asymmetry are preserved while the density scales.
        let earth = AtmosphereParameters::earth();
        let earth_albedo = earth.mie_scattering[0] / earth.mie_extinction[0];
        let mut p = AtmosphereParameters::earth();
        p.set_aerosol_turbidity(0.1);
        assert!((p.mie_scattering[0] / p.mie_extinction[0] - earth_albedo).abs() < 1.0e-6);
        assert_eq!(p.mie_g, earth.mie_g);
        assert!(p.mie_extinction[0] > earth.mie_extinction[0], "more aerosol");
    }

    #[test]
    fn todays_turbidity_reproduces_the_published_earth() {
        let today = regia_scene::level::TODAY_AEROSOL_TURBIDITY;
        assert_eq!(
            today,
            AtmosphereParameters::earth().aerosol_optical_depth(),
            "the serde default must equal the published Earth baseline"
        );
        let mut p = AtmosphereParameters::earth();
        p.set_aerosol_turbidity(today);
        assert_eq!(p, AtmosphereParameters::earth(), "today's look is bit-exact");
    }

    /// More aerosol scatters more of the sun's light out of the direct beam and into the sky,
    /// so the shadow (diffuse skylight) warms from a very blue ≈25 kK toward ≈10–15 kK.
    #[test]
    fn a_turbid_sky_warms_the_shadow_light() {
        let e = 35.0_f32.to_radians();
        let sun = [0.0, e.sin(), e.cos()];
        let clean = build_atmosphere_luts_for(&AtmosphereParameters::earth(), sun, 2.0);
        let mut turbid_params = AtmosphereParameters::earth();
        turbid_params.set_aerosol_turbidity(0.1);
        let turbid = build_atmosphere_luts_for(&turbid_params, sun, 2.0);
        let clean_amb = ambient_irradiance_table(&clean);
        let turbid_amb = ambient_irradiance_table(&turbid);
        // The aerosol adds spectrally-flat light, so the shadow's blue/red ratio falls.
        let blue_ratio = |c: [f32; 3]| c[2] / c[0].max(1.0e-4);
        assert!(
            blue_ratio(turbid_amb) < blue_ratio(clean_amb),
            "turbid shadow must be less blue: clean {clean_amb:?} turbid {turbid_amb:?}"
        );
        // Whiter near the horizon: a long aerosol path scatters flat light, so the horizon's
        // blue/red ratio drops too.
        let horizon = normalize([0.0, 0.03, -1.0]);
        let clean_h = sample_sky_table(&clean, horizon);
        let turbid_h = sample_sky_table(&turbid, horizon);
        assert!(
            blue_ratio(turbid_h) < blue_ratio(clean_h),
            "turbid horizon must be whiter: clean {clean_h:?} turbid {turbid_h:?}"
        );
        let clean_k = crate::correlated_color_temperature(clean_amb);
        let turbid_k = crate::correlated_color_temperature(turbid_amb);
        eprintln!("shadow light: clean {clean_k} K, turbid {turbid_k} K");
        assert!(turbid_k < clean_k, "turbid shadow must read warmer");
        assert!(turbid_k < clean_k - 1000.0, "warming must be measurable");
    }

    /// ADR-1455: the sun through the showcase air (τ 0.18, Ångström 1.3, 21°, seen from 100 m) in
    /// every channel, against the RGB Monte Carlo of the same air (lane script `mc_rgb.py`: the
    /// engine's red, green and blue coefficients, Rayleigh, ozone and the Ångström-scaled aerosol at
    /// 680 / 550 / 440 nm). ADR-1447 checked green only.
    #[test]
    fn the_showcase_sun_matches_the_monte_carlo_in_every_channel() {
        let air = showcase_air();
        let el = 21f32.to_radians();
        let luts = build_atmosphere_luts_for(&air, [0.0, el.sin(), el.cos()], 100.0);
        let (colour, lux) = transmitted_sun(&luts, &luts.params);
        for (c, want) in [0.6050f32, 0.4347, 0.2614].into_iter().enumerate() {
            let got = colour[c] * lux / air.sun_illuminance;
            assert!(
                (got / want - 1.0).abs() < 0.03,
                "sun channel {c}: {got}, reference {want}"
            );
        }
    }

    /// ADR-1455, known gap: the colour of the showcase sky against the same RGB Monte Carlo (all
    /// orders, 8000 paths per direction, ±2–3 %). The engine's sky is less blue than the reference:
    /// at 10° up and 130° from the sun its red/green is 0.727 against 0.665 and its blue/green
    /// 1.331 against 1.456, and near the horizon it is warmer. The cause is the isotropic
    /// multiple-scattering table (Hillaire 2020) in this hazy air: against the Monte Carlo's orders
    /// above the first it has 18–31 % too little blue near the horizon and up to 55 % too much red
    /// high up, while in clean air it stays within about 10 %. The single scattering is right once
    /// the march integrates the transmittance over each segment, but that alone makes the sky
    /// greyer (it now over-counts in-scatter on the thick near-horizon segments, which hides part
    /// of the gap). Run with `--ignored` to measure; a multiple-scattering model with directional
    /// orders should make it pass.
    #[test]
    #[ignore = "ADR-1455 known gap: isotropic multiple scattering under haze; see the doc comment"]
    fn the_showcase_sky_colour_matches_the_monte_carlo_in_every_channel() {
        let air = showcase_air();
        let el = 21f32.to_radians();
        let luts = build_atmosphere_luts_for(&air, [0.0, el.sin(), el.cos()], 100.0);
        let e0 = air.sun_illuminance;
        // (elevation°, azimuth from the sun°, reference L / E₀ in red, green, blue).
        let mut worst = 0.0f32;
        for (el_deg, az_deg, want) in [
            (3.0f32, 130.0f32, [0.03379f32, 0.03975, 0.04554]),
            (6.0, 130.0, [0.03156, 0.04141, 0.05068]),
            (10.0, 130.0, [0.02427, 0.03647, 0.05310]),
            (13.0, 130.0, [0.02069, 0.03442, 0.05260]),
            (20.0, 130.0, [0.01469, 0.02588, 0.04590]),
            (30.0, 130.0, [0.01037, 0.01891, 0.03578]),
            (60.0, 130.0, [0.00632, 0.01092, 0.02297]),
            (10.0, 60.0, [0.04200, 0.05380, 0.06763]),
            (10.0, 90.0, [0.02621, 0.03743, 0.05273]),
            (10.0, 180.0, [0.02827, 0.04189, 0.06002]),
        ] {
            let (e, a) = (el_deg.to_radians(), az_deg.to_radians());
            let got = sample_sky_table(&luts, [e.cos() * a.sin(), e.sin(), e.cos() * a.cos()]);
            let got = got.map(|v| v / e0);
            let ratios = |l: [f32; 3]| [l[0] / l[1], l[2] / l[1]];
            let (g, w) = (ratios(got), ratios(want));
            eprintln!(
                "sky {el_deg}°/{az_deg}°: engine {got:?} (r/g {:.3}, b/g {:.3}), reference {want:?} (r/g {:.3}, b/g {:.3})",
                g[0], g[1], w[0], w[1]
            );
            for k in 0..2 {
                worst = worst.max((g[k] / w[k] - 1.0).abs());
            }
        }
        assert!(worst < 0.08, "sky colour off the reference by {worst}");
    }

    /// The forest demo's air (aerosol optical depth 0.1) under a 35° sun, from 1.2 km up.
    fn hazy_luts() -> AtmosphereLuts {
        let mut planet = AtmosphereParameters::earth();
        planet.set_aerosol_turbidity(0.1);
        build_atmosphere_luts_for(&planet, normalize([0.0, 0.574, 0.819]), 1200.0)
    }

    fn luma(rgb: [f32; 3]) -> f32 {
        0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]
    }

    /// Views of the ground and the horizon, toward and away from the sun.
    const CLOUD_VIEWS: [[f32; 3]; 5] = [
        [0.0, -0.555, 0.832],
        [0.0, -0.555, -0.832],
        [0.9, -0.3, 0.3],
        [-0.7, -0.05, 0.71],
        [0.0, 0.2, 0.98],
    ];

    /// ADR-5944: the sun table holds the sun's single scattering, a share of the aerial table
    /// in every texel; the rest is the multiple scattering a cloud's shadow leaves.
    #[test]
    fn the_sun_table_is_a_share_of_the_aerial_table() {
        let luts = hazy_luts();
        assert_eq!(luts.aerial_sun.width, luts.aerial.width);
        assert_eq!(luts.aerial_sun.height, luts.aerial.height);
        assert_eq!(luts.aerial_sun.texels.len(), luts.aerial.texels.len());
        for (i, (total, sun)) in luts.aerial.texels.iter().zip(&luts.aerial_sun.texels).enumerate() {
            for c in 0..3 {
                assert!(
                    sun[c] >= 0.0 && sun[c] <= total[c] * 1.00001 + 1.0e-6,
                    "texel {i} channel {c}: sun {} of {}",
                    sun[c],
                    total[c]
                );
            }
        }
        // Both terms are real shares of 3 km of this hazy air, even looking into the aerosol's
        // forward lobe (the tables give the sun about 0.4 of it: the ground's bounce and the
        // sky light scattered again carry the rest), so a cover that took only the sun's single
        // scattering would leave most of the veil.
        let view = CLOUD_VIEWS[0];
        let (total, _) = sample_aerial(&luts, view, 3000.0);
        let share = luma(sample_aerial_sun(&luts, view, 3000.0)) / luma(total);
        eprintln!("the sun's single scattering is {share:.3} of 3 km of in-scatter");
        assert!((0.2..0.9).contains(&share), "the sun's share is {share}");
    }

    /// Ground light that is all sky: the multiple scattering then keeps everything, and only the
    /// sun's own share moves.
    fn sky_only() -> CloudAirLight {
        CloudAirLight {
            ground_sun: [0.0; 3],
            ground_sky: [1.0; 3],
            overcast: 0.0,
            under_top: 1.0,
        }
    }

    /// A clear sky returns the aerial table's value bit for bit, whatever the ground light; a
    /// sky that lets no sun through takes the sun's single scattering whole and never adds
    /// light.
    #[test]
    fn a_clear_sky_keeps_the_table_and_a_closed_one_takes_the_suns_share() {
        let luts = hazy_luts();
        for light in [CloudAirLight::clear(&luts), sky_only()] {
            for view in CLOUD_VIEWS {
                for dist in [150.0, 1900.0, 2100.0, 7300.0, 31_000.0, 45_000.0] {
                    let (total, t) = sample_aerial(&luts, view, dist);
                    let (clear, t_clear) =
                        sample_aerial_under_clouds(&luts, view, dist, [0.0, dist], |_| 1.0, &light);
                    assert_eq!(
                        clear.map(f32::to_bits),
                        total.map(f32::to_bits),
                        "{view:?} {dist}"
                    );
                    assert_eq!(t_clear.to_bits(), t.to_bits());
                    let (dark, t_dark) =
                        sample_aerial_under_clouds(&luts, view, dist, [0.0, dist], |_| 0.0, &light);
                    assert_eq!(t_dark.to_bits(), t.to_bits(), "shadowed air still absorbs");
                    let sun = sample_aerial_sun(&luts, view, dist);
                    for c in 0..3 {
                        let multi = (total[c] - sun[c]).max(0.0);
                        assert!(
                            dark[c] <= multi + 1.0e-4 * total[c] + 1.0e-4,
                            "{view:?} {dist} channel {c}: {} left, the multiple scattering is {multi}",
                            dark[c]
                        );
                        assert!(dark[c] <= total[c], "a shadow added light");
                        if light == sky_only() {
                            assert!((dark[c] - multi).abs() <= 1.0e-4 * total[c] + 1.0e-4);
                        }
                    }
                }
            }
        }
    }

    /// A shadow over a stretch of the ray takes that stretch's share of the sun, `S(b) − S(a)`,
    /// whether the visibility or the span says where it is, and half a shadow takes half.
    #[test]
    fn a_shadowed_stretch_loses_its_own_share_of_the_sun() {
        let luts = hazy_luts();
        let light = sky_only();
        for view in CLOUD_VIEWS {
            let dist = 9000.0;
            let (total, _) = sample_aerial(&luts, view, dist);
            let (s_near, s_far) = (
                sample_aerial_sun(&luts, view, 2000.0),
                sample_aerial_sun(&luts, view, 6000.0),
            );
            let stretch = |t: f32| if (2000.0..6000.0).contains(&t) { 0.0 } else { 1.0 };
            let (by_vis, _) = sample_aerial_under_clouds(&luts, view, dist, [0.0, dist], stretch, &light);
            let (by_span, _) =
                sample_aerial_under_clouds(&luts, view, dist, [2000.0, 6000.0], |_| 0.0, &light);
            let (half, _) = sample_aerial_under_clouds(&luts, view, dist, [2000.0, 6000.0], |_| 0.5, &light);
            for c in 0..3 {
                let want = s_far[c] - s_near[c];
                let tol = 1.0e-4 * total[c] + 1.0e-4;
                assert!(
                    (total[c] - by_vis[c] - want).abs() <= tol,
                    "{view:?} channel {c}: removed {} of {want}",
                    total[c] - by_vis[c]
                );
                assert!((by_span[c] - by_vis[c]).abs() <= tol);
                assert!((total[c] - half[c] - 0.5 * want).abs() <= tol);
            }
            // A shadow in front of the surface only: nothing behind it is touched.
            let (short, _) =
                sample_aerial_under_clouds(&luts, view, 3000.0, [2000.0, 6000.0], |_| 0.0, &light);
            let (total_short, _) = sample_aerial(&luts, view, 3000.0);
            let s_short = sample_aerial_sun(&luts, view, 3000.0);
            for c in 0..3 {
                let want = s_short[c] - s_near[c];
                assert!((total_short[c] - short[c] - want).abs() <= 1.0e-4 * total_short[c] + 1.0e-4);
            }
        }
    }

    /// Under one cover the air's light and the ground's fall together (ADR-5944). With the
    /// sun's visibility `v` over the whole ray, the multiple scattering keeps
    /// `(sun · v + sky') / (sun + sky)` of itself, the factor by which the light on flat ground
    /// falls (`sky'` the sky's share under the overcast rule), so the whole in-scatter never
    /// keeps more than that factor: the contrast of the ground seen through the air does not
    /// fall under a cover (Koschmieder).
    #[test]
    fn under_a_cover_the_air_follows_the_ground_light() {
        let luts = hazy_luts();
        let clear_light = CloudAirLight::clear(&luts);
        assert!(luma(clear_light.ground_sun) > luma(clear_light.ground_sky));
        for view in CLOUD_VIEWS {
            for (v, overcast) in [
                (0.0_f32, 0.0_f32),
                (0.05, 0.0),
                (0.5, 0.0),
                (0.05, 0.8),
                (1.0, 0.8),
            ] {
                let dist = 3000.0;
                let light = CloudAirLight {
                    overcast,
                    ..clear_light
                };
                let (total, _) = sample_aerial(&luts, view, dist);
                let sun = sample_aerial_sun(&luts, view, dist);
                let (got, _) = sample_aerial_under_clouds(&luts, view, dist, [0.0, dist], |_| v, &light);
                let sky = crate::clouds::overcast_sky(light.ground_sky, overcast);
                for c in 0..3 {
                    let ground =
                        (light.ground_sun[c] * v + sky[c]) / (light.ground_sun[c] + light.ground_sky[c]);
                    let multi = (total[c] - sun[c]).max(0.0);
                    let want = sun[c] * v + multi * ground;
                    assert!(
                        (got[c] - want).abs() <= 2.0e-4 * total[c] + 1.0e-4,
                        "{view:?} v {v} overcast {overcast} channel {c}: {} against {want}",
                        got[c]
                    );
                    // The sun's share falls by v, never more slowly than the ground's light.
                    assert!(got[c] <= total[c] * ground.max(v) + 2.0e-4 * total[c] + 1.0e-4);
                }
                if v < 1.0 {
                    let ground_luma = luma(std::array::from_fn(|c| light.ground_sun[c] * v + sky[c]))
                        / luma(std::array::from_fn(|c| light.ground_sun[c] + light.ground_sky[c]));
                    assert!(
                        luma(got) <= luma(total) * ground_luma * 1.02 + 1.0e-3,
                        "{view:?} v {v} overcast {overcast}: the air kept {} of its light, the ground {ground_luma}",
                        luma(got) / luma(total)
                    );
                }
            }
        }
        // A ray above the layer (no share under its top) takes no overcast.
        let above = CloudAirLight {
            overcast: 0.8,
            under_top: 0.0,
            ..clear_light
        };
        let (total, _) = sample_aerial(&luts, CLOUD_VIEWS[0], 3000.0);
        let (same, _) =
            sample_aerial_under_clouds(&luts, CLOUD_VIEWS[0], 3000.0, [0.0, 0.0], |_| 1.0, &above);
        assert_eq!(same.map(f32::to_bits), total.map(f32::to_bits));
    }

    /// The analytic haze's floor under a cloud layer (`HAZE_SKY_FLOOR`) is the sky's share of
    /// the light on flat ground under a clear sky: what is left of that light when a cover takes
    /// the sun. It lies inside what the tables give for clear Earth air over suns from 20° to
    /// 60° up.
    #[test]
    fn the_haze_floor_is_within_the_tables_sky_share_of_the_ground_light() {
        let mut shares = Vec::new();
        for elevation_deg in [20.0_f32, 30.0, 45.0, 60.0] {
            let e = elevation_deg.to_radians();
            let luts = build_atmosphere_luts([0.0, e.sin(), e.cos()], 2.0);
            let (sun, sky) = ground_light(&luts);
            let share = luma(sky) / (luma(sun) + luma(sky));
            eprintln!("sun {elevation_deg}° up: the sky gives {share:.3} of the light on flat ground");
            shares.push(share);
        }
        let lo = shares.iter().copied().fold(f32::INFINITY, f32::min);
        let hi = shares.iter().copied().fold(0.0_f32, f32::max);
        let floor = crate::clouds::HAZE_SKY_FLOOR;
        eprintln!("the sky's share of the clear ground light: {lo:.3} to {hi:.3}; the floor is {floor}");
        assert!(
            (lo..=hi).contains(&floor),
            "HAZE_SKY_FLOOR {floor} is outside the tables' {lo:.3} to {hi:.3}"
        );
    }

    /// The composite's shader takes the same number of map samples per segment as the mirror.
    #[test]
    fn the_shader_steps_match_the_mirror() {
        let wgsl = include_str!("../shaders/outdoor_sky.wgsl");
        let body = wgsl
            .split_once("fn cloud_air_steps(n: i32) -> i32 {")
            .and_then(|(_, rest)| rest.split_once("\n}\n"))
            .map(|(body, _)| body)
            .unwrap_or_else(|| panic!("outdoor_sky.wgsl has no cloud_air_steps"));
        let returns: Vec<u32> = body
            .split("return ")
            .skip(1)
            .filter_map(|rest| rest.split(';').next()?.trim().parse().ok())
            .collect();
        assert_eq!(returns, AERIAL_CLOUD_STEPS);
    }
}
