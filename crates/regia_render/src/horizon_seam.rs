// SPDX-License-Identifier: MPL-2.0
//! The horizon seam and the CPU model of `aerial_from_tables` (ADR-3294).
//!
//! On a frame that draws the atmosphere tables, the sky shades from the sky-view table and the
//! ground from the aerial-perspective table (Hillaire 2020). Two things still split them at the
//! horizon by default: the tables end at 32 km, so ground past that keeps the 32 km haze while
//! the sky's last row holds the whole path to the edge of the air; and the precipitation's
//! extinction (spec 99 § 3.2) reaches only the ground, with the analytic `haze_inscatter` as its
//! in-scatter, in the lab frame's stand-in units, while the sky stays clear.
//!
//! [`measure_horizon_seam`] is the CPU readback measure: the step between the sky's last row and
//! the farthest ground row below it, per column, on a linear image and its sky mask.
//! [`table_air`] and [`forward_weather_inscatter`] mirror the shader terms of both modes, and
//! [`horizon_seam_model`] predicts the seam of either mode from the tables alone (no image),
//! which `render.get_weather` reports.

use crate::atmosphere::{AtmosphereLuts, ground_radiance, sample_aerial, sample_sky_table};
use crate::outdoor::TableAir;

/// The end of the aerial table's last slice (m), `TABLE_AIR_RANGE_M` in `outdoor_sky.wgsl`.
pub const TABLE_AIR_RANGE_M: f32 = 32_000.0;

/// The step between the sky's last row and the farthest ground row below it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HorizonSeam {
    /// Columns that had a sky pixel directly above a ground pixel.
    pub columns: u32,
    /// Mean linear colour of the sky's last row over those columns.
    pub sky: [f32; 3],
    /// Mean linear colour of the farthest ground row over those columns.
    pub ground: [f32; 3],
    /// `Σ |sky − ground| / Σ |sky|` over the columns (Euclidean RGB): 0 is seamless.
    pub step: f32,
    /// `Σ |L(sky) − L(ground)| / Σ L(sky)` (Rec. 709 luminance).
    pub luminance_step: f32,
}

/// Rec. 709 luminance of a linear colour.
#[must_use]
pub fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn length(c: [f32; 3]) -> f32 {
    (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt()
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn mix3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn scale(a: [f32; 3], k: f32) -> [f32; 3] {
    [a[0] * k, a[1] * k, a[2] * k]
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

/// Measures the horizon seam of a readback: `rgb` is linear colour, row-major from the top row,
/// `sky` marks the pixels with the cleared depth. In each column the first sky pixel that has a
/// ground pixel directly below it is the sky's last row, and that ground pixel the farthest
/// ground row. `None` when the sizes disagree, a value is not finite, or no column has a
/// sky-over-ground edge.
#[must_use]
pub fn measure_horizon_seam(rgb: &[[f32; 3]], sky: &[bool], width: u32, height: u32) -> Option<HorizonSeam> {
    let (w, h) = (width as usize, height as usize);
    let count = w.checked_mul(h)?;
    if rgb.len() != count || sky.len() != count || h < 2 {
        return None;
    }
    let mut columns = 0u32;
    let (mut sky_sum, mut ground_sum) = ([0.0f64; 3], [0.0f64; 3]);
    let (mut diff, mut norm, mut lum_diff, mut lum_sum) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for x in 0..w {
        let Some(y) = (0..h - 1).find(|&y| sky[y * w + x] && !sky[(y + 1) * w + x]) else {
            continue;
        };
        let s = rgb[y * w + x];
        let g = rgb[(y + 1) * w + x];
        if s.iter().chain(g.iter()).any(|v| !v.is_finite()) {
            return None;
        }
        columns += 1;
        sky_sum = std::array::from_fn(|c| sky_sum[c] + f64::from(s[c]));
        ground_sum = std::array::from_fn(|c| ground_sum[c] + f64::from(g[c]));
        diff += f64::from(length(sub(s, g)));
        norm += f64::from(length(s));
        lum_diff += f64::from((luminance(s) - luminance(g)).abs());
        lum_sum += f64::from(luminance(s));
    }
    if columns == 0 {
        return None;
    }
    let n = f64::from(columns);
    Some(HorizonSeam {
        columns,
        sky: sky_sum.map(|v| (v / n) as f32),
        ground: ground_sum.map(|v| (v / n) as f32),
        step: (diff / norm.max(1e-12)) as f32,
        luminance_step: (lum_diff / lum_sum.max(1e-12)) as f32,
    })
}

/// Mean luminance of the sky pixels and of the ground pixels of a readback, `(sky, ground)`;
/// 0 for a region with no pixels.
#[must_use]
pub fn region_luminance(rgb: &[[f32; 3]], sky: &[bool]) -> (f32, f32) {
    let (mut s, mut sn, mut g, mut gn) = (0.0f64, 0u64, 0.0f64, 0u64);
    for (c, &is_sky) in rgb.iter().zip(sky) {
        let l = f64::from(luminance(*c));
        if is_sky {
            s += l;
            sn += 1;
        } else {
            g += l;
            gn += 1;
        }
    }
    let mean = |sum: f64, n: u64| if n == 0 { 0.0 } else { (sum / n as f64) as f32 };
    (mean(s, sn), mean(g, gn))
}

/// `sky_color` of `utils.wgsl` (the analytic stand-in sky the default weather term reads).
fn analytic_sky_color(dir: [f32; 3]) -> [f32; 3] {
    let t = dir[1];
    let horizon = [0.78, 0.85, 0.95];
    let zenith = [0.20, 0.40, 0.92];
    let ground = [0.34, 0.33, 0.31];
    if t > 0.0 {
        mix3(horizon, zenith, 1.0 - (-t * 3.0).exp())
    } else {
        mix3(horizon, ground, 1.0 - (t * 2.0).exp())
    }
}

fn henyey_greenstein(cos_theta: f32, g: f32) -> f32 {
    let g2 = g * g;
    let denom = (1.0 + g2 - 2.0 * g * cos_theta).max(1e-4);
    (1.0 - g2) / (4.0 * std::f32::consts::PI * denom * denom.sqrt())
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let l = length(v);
    if l > 0.0 {
        scale(v, 1.0 / l)
    } else {
        [0.0, 1.0, 0.0]
    }
}

/// `haze_inscatter` of `aerial.wgsl`: the analytic sky one notch above the horizon in the view's
/// azimuth times `sky_scale`, plus a forward lobe toward the sun.
#[must_use]
pub fn haze_inscatter(
    view: [f32; 3],
    to_sun: [f32; 3],
    sun_irradiance: [f32; 3],
    sky_scale: f32,
) -> [f32; 3] {
    let mut flat = [view[0], 0.0, view[2]];
    if flat[0] * flat[0] + flat[2] * flat[2] < 1e-6 {
        flat = [1.0, 0.0, 0.0];
    }
    let flat = normalize(flat);
    let horizon = normalize([flat[0], 0.06, flat[2]]);
    let sky = scale(analytic_sky_color(horizon), sky_scale);
    let cos = view[0] * to_sun[0] + view[1] * to_sun[1] + view[2] * to_sun[2];
    add(sky, scale(sun_irradiance, 0.02 * henyey_greenstein(cos, 0.6)))
}

/// `weather_inscatter` of `lab_forward.wgsl` (the default mode's rain in-scatter): with
/// `sky_inscatter` fog, [`haze_inscatter`] at [`crate::SKY_IBL_SCALE`] times the fog colour at
/// unit luminance; otherwise the fog colour over the exposure multiplier.
#[must_use]
pub fn forward_weather_inscatter(
    view: [f32; 3],
    to_sun: [f32; 3],
    sun_irradiance: [f32; 3],
    fog: &crate::FogSettings,
    exposure_multiplier: f32,
) -> [f32; 3] {
    if fog.sky_inscatter {
        let tint = scale(fog.color, 1.0 / luminance(fog.color).max(1e-3));
        let haze = haze_inscatter(view, to_sun, sun_irradiance, crate::SKY_IBL_SCALE);
        [haze[0] * tint[0], haze[1] * tint[1], haze[2] * tint[2]]
    } else {
        scale(fog.color, 1.0 / exposure_multiplier.max(1e-8))
    }
}

/// `horizon_sky` of `outdoor_sky.wgsl`: the sky table's first row above the horizon in `view`'s
/// azimuth (latitude `(π/2) / h²` maps to `v = 0.5 + 0.5 / h`, the row's centre).
#[must_use]
pub fn horizon_sky(luts: &AtmosphereLuts, view: [f32; 3]) -> [f32; 3] {
    let mut flat = [view[0], 0.0, view[2]];
    if flat[0] * flat[0] + flat[2] * flat[2] < 1e-12 {
        flat = [0.0, 0.0, 1.0];
    }
    let flat = normalize(flat);
    let h = luts.sky_view.height.max(1) as f32;
    let lat = std::f32::consts::FRAC_PI_2 / (h * h);
    let (s, c) = lat.sin_cos();
    sample_sky_table(luts, [flat[0] * c, s, flat[2] * c])
}

/// How much of the sky a drop under a raining deck sees covered (0..1, ADR-3294 (12)): the opacity
/// of a covered column, `1 − exp(−COLUMN_FILL · τ)` with `τ` the layer's optical depth at full
/// cover, the term [`crate::cloud_overcast_factor`] multiplies by the map's mean coverage. Rain
/// falls from the cloud above it, so the column over a wet pixel is a covered one, whatever the
/// layer's mean coverage. 0 without clouds.
#[must_use]
pub fn rain_deck_opacity(settings: &crate::CloudSettings) -> f32 {
    if settings.coverage <= 0.0 {
        return 0.0;
    }
    let thickness = (settings.altitude_top - settings.altitude_bottom).max(0.0);
    let steps = 8;
    let profile: f32 = (0..steps)
        .map(|i| crate::type_gradient((i as f32 + 0.5) / steps as f32, settings.cloud_type))
        .sum::<f32>()
        / steps as f32;
    let tau = settings.density.max(0.0) * thickness * profile;
    (1.0 - (-crate::COLUMN_FILL * tau).exp()).clamp(0.0, 1.0)
}

/// A deck's overcast factor clamped to 0..1 (NaN is no deck).
fn deck_factor(overcast: f32) -> f32 {
    if overcast.is_nan() {
        0.0
    } else {
        overcast.clamp(0.0, 1.0)
    }
}

/// The part of [`rain_inscatter`] that is the same for every pixel (the sky uniform's
/// `table_ground` row, cd/m²): half the lit planet ground under a drawn deck's `overcast` factor
/// (0..1, [`crate::overcast_sky`]'s rule), plus the sun transmitted to the eye spread over 4π (an
/// isotropic drop's share of a direct beam).
///
/// ADR-3294 (13): this row feeds only the rain veil, and rain falls from an optically thick deck
/// (nimbostratus), so under any drawn deck (`overcast` > 0) the deck blocks the direct sun
/// entirely — the sun share is 0, and the ground half keeps its diffuse-sky lighting alone
/// (`ground_radiance` without its sun term). With no deck the full sun share comes through.
#[must_use]
pub fn rain_inscatter_constant(luts: &AtmosphereLuts, overcast: f32) -> [f32; 3] {
    let o = deck_factor(overcast);
    let (color, lux) = crate::atmosphere::transmitted_sun(luts, &luts.params);
    let mut ground = ground_radiance(luts).map(|c| c.max(0.0));
    if o > 0.0 {
        // The sun term of the ground's irradiance, removed: the raining deck shadows it.
        let cos_sun = luts.sun_direction[1].max(0.0);
        for (c, g) in ground.iter_mut().enumerate() {
            let sun_e = color[c] * lux * cos_sun / std::f32::consts::PI;
            *g = (*g - luts.params.ground_albedo[c] * sun_e).max(0.0);
        }
    }
    let ground = crate::overcast_sky(scale(ground, 0.5), o);
    let sun = if o > 0.0 {
        [0.0; 3]
    } else {
        color.map(|c| (c * lux).max(0.0) / (4.0 * std::f32::consts::PI))
    };
    add(ground, sun)
}

/// `rain_inscatter` of `outdoor_sky.wgsl` (ADR-3294): the radiance the precipitation scatters
/// toward the eye, the sphere average an isotropic drop sees. The upper half is [`horizon_sky`]
/// under the deck's `overcast` rule; the rest is [`rain_inscatter_constant`] (the lit ground and
/// the sun; under a deck the sun is blocked and the ground is diffuse-lit, ADR-3294 (13)).
#[must_use]
pub fn rain_inscatter(luts: &AtmosphereLuts, view: [f32; 3], overcast: f32) -> [f32; 3] {
    let o = deck_factor(overcast);
    let sky = crate::overcast_sky(scale(horizon_sky(luts, view), 0.5), o);
    add(sky, rain_inscatter_constant(luts, o))
}

/// One slice of the aerial table at `(u, v)`, bilinear inside the slice (`aerial_texel`).
fn aerial_slice(luts: &AtmosphereLuts, u: f32, v: f32, slice: u32) -> [f32; 4] {
    let slices = luts.aerial_slices.max(1);
    let w = (luts.aerial.width / slices).max(1) as i64;
    let h = luts.aerial.height.max(1) as i64;
    let atlas = i64::from(luts.aerial.width.max(1));
    let fu = (u - u.floor()) * w as f32 - 0.5;
    let fv = v.clamp(0.0, 1.0) * h as f32 - 0.5;
    let (x0, y0) = (fu.floor() as i64, fv.floor() as i64);
    let (tx, ty) = (fu - fu.floor(), fv - fv.floor());
    let fetch = |x: i64, y: i64| -> [f32; 4] {
        let xi = x.rem_euclid(w) + i64::from(slice.min(slices - 1)) * w;
        let yi = y.clamp(0, h - 1);
        let index = (yi * atlas + xi) as usize;
        luts.aerial
            .texels
            .get(index)
            .copied()
            .unwrap_or([0.0, 0.0, 0.0, 1.0])
    };
    let lerp =
        |a: [f32; 4], b: [f32; 4], t: f32| -> [f32; 4] { std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t) };
    lerp(
        lerp(fetch(x0, y0), fetch(x0 + 1, y0), tx),
        lerp(fetch(x0, y0 + 1), fetch(x0 + 1, y0 + 1), tx),
        ty,
    )
}

/// The `(u, v)` of a view in the sky and aerial tables (`sky_uv` of `outdoor_sky.wgsl`).
fn table_uv(view: [f32; 3], sun: [f32; 3]) -> (f32, f32) {
    let view = normalize(view);
    let up_sun = [sun[0], 0.0, sun[2]];
    let up_view = [view[0], 0.0, view[2]];
    let sun_n = if length(up_sun) > 1e-4 {
        normalize(up_sun)
    } else {
        [0.0, 0.0, 1.0]
    };
    let view_n = if length(up_view) > 1e-4 {
        normalize(up_view)
    } else {
        [0.0, 1.0, 0.0]
    };
    // cross(sun_n, view_n) · up
    let sin_az = sun_n[2] * view_n[0] - sun_n[0] * view_n[2];
    let cos_az = (sun_n[0] * view_n[0] + sun_n[1] * view_n[1] + sun_n[2] * view_n[2]).clamp(-1.0, 1.0);
    let azimuth = sin_az.atan2(cos_az);
    let u = (azimuth / std::f32::consts::TAU + 0.5).rem_euclid(1.0);
    let lat = view[1].clamp(-1.0, 1.0).asin();
    let v = 0.5 + 0.5 * lat.signum() * (lat.abs() / std::f32::consts::FRAC_PI_2).sqrt();
    (u, v.clamp(0.0, 1.0))
}

/// `lite_aerial` of `outdoor_sky.wgsl`: one slice, the air in it taken as even.
fn lite_aerial(luts: &AtmosphereLuts, view: [f32; 3], dist_m: f32) -> ([f32; 3], f32) {
    let (u, v) = table_uv(view, luts.sun_direction);
    let slices = luts.aerial_slices.max(1);
    let step_km = TABLE_AIR_RANGE_M / 1000.0 / slices as f32;
    let km = dist_m.max(0.0) / 1000.0;
    let slice = ((km / step_km).ceil() as i64 - 1).clamp(0, i64::from(slices) - 1) as u32;
    let s = aerial_slice(luts, u, v, slice);
    let end_km = (slice + 1) as f32 * step_km;
    let ts = s[3].clamp(1e-6, 1.0);
    let x = (km / end_km).min(1.0);
    let t = ts.powf(x);
    let k = if 1.0 - ts > 1e-4 {
        (1.0 - t) / (1.0 - ts)
    } else {
        x
    };
    ([s[0] * k, s[1] * k, s[2] * k], t)
}

/// The air between the eye and a surface `dist_m` away along `view`, `(in-scatter cd/m²,
/// transmittance)`, as the outdoor composite applies it in `mode` (before the weather veil):
/// [`TableAir::Off`] is the aerial table clamped at its last slice; [`TableAir::Full`] and
/// [`TableAir::Lite`] carry it past the last slice to the horizon sky.
#[must_use]
pub fn table_air(luts: &AtmosphereLuts, view: [f32; 3], dist_m: f32, mode: TableAir) -> ([f32; 3], f32) {
    let (rgb, t) = match mode {
        TableAir::Lite => lite_aerial(luts, view, dist_m),
        TableAir::Off | TableAir::Full => sample_aerial(luts, view, dist_m),
    };
    if mode == TableAir::Off || dist_m <= TABLE_AIR_RANGE_M {
        return (rgb, t);
    }
    let edge_t = t.clamp(1e-6, 1.0);
    let far_t = edge_t.powf(dist_m / TABLE_AIR_RANGE_M);
    (mix3(rgb, horizon_sky(luts, view), 1.0 - far_t / edge_t), far_t)
}

/// What [`horizon_seam_model`] predicts for one mode.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeamPrediction {
    /// Mean sky's last row (cd/m²).
    pub sky: [f32; 3],
    /// Mean farthest ground row (cd/m²).
    pub ground: [f32; 3],
    /// `Σ |sky − ground| / Σ |sky|` over the azimuths, as [`HorizonSeam::step`].
    pub step: f32,
}

/// The seam both modes give, predicted from the tables alone.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HorizonSeamModel {
    /// The default frame (`aerial_from_tables` off).
    pub off: SeamPrediction,
    /// With `aerial_from_tables` on ([`TableAir::Full`]).
    pub on: SeamPrediction,
    /// The farthest ground the model looks at (m).
    pub far_m: f32,
    /// The precipitation's extinction it applied (1/m).
    pub extinction_per_m: f32,
}

/// The light and weather [`horizon_seam_model`] predicts the seam under.
#[derive(Clone, Copy, Debug)]
pub struct SeamInputs<'a> {
    /// Unit vector toward the sun.
    pub to_sun: [f32; 3],
    /// The sun's colour × illuminance (lux) the forward pass shades with.
    pub sun_irradiance: [f32; 3],
    /// The precipitation's extinction (1/m).
    pub extinction_per_m: f32,
    /// The farthest ground (m): the far plane, or the edge of the map.
    pub far_m: f32,
    /// The host's fog, which colours the default mode's rain.
    pub fog: &'a crate::FogSettings,
    /// The exposure multiplier the default mode's flat fog colour is divided by.
    pub exposure_multiplier: f32,
}

/// Predicts the horizon seam of both modes over 16 azimuths around the eye: the sky's last row
/// is [`horizon_sky`]; the farthest ground row is the planet's Lambert ground (`ground_radiance`)
/// `far_m` away. Off: the forward pass's rain term ([`forward_weather_inscatter`]) on the ground,
/// then the clamped aerial table, and a clear sky. On: [`table_air`] on the ground, then the
/// weather veil on both (`exp(−σ d)` toward [`rain_inscatter`] with no deck, `d` = `far_m`).
#[must_use]
pub fn horizon_seam_model(luts: &AtmosphereLuts, inputs: &SeamInputs<'_>) -> HorizonSeamModel {
    let surface = ground_radiance(luts);
    let sigma = inputs.extinction_per_m.max(0.0);
    let d = inputs.far_m.max(0.0);
    let tw = (-sigma * d).exp();
    let mut acc = [(0.0f64, 0.0f64, [0.0f64; 3], [0.0f64; 3]); 2];
    for i in 0..16 {
        let a = i as f32 / 16.0 * std::f32::consts::TAU;
        // A ray a hair under the horizon, toward the far ground.
        let view = normalize([a.sin(), -1e-4, a.cos()]);
        let horizon = horizon_sky(luts, view);
        // Off: the forward pass's rain, then the clamped aerial table; the sky has no rain.
        let rain = forward_weather_inscatter(
            view,
            inputs.to_sun,
            inputs.sun_irradiance,
            inputs.fog,
            inputs.exposure_multiplier,
        );
        let forward = add(scale(surface, tw), scale(rain, 1.0 - tw));
        let (air, t) = table_air(luts, view, d, TableAir::Off);
        let off_ground = add(scale(forward, t), air);
        // On: the extended table, then the veil over both, toward the rain's in-scatter.
        let (air, t) = table_air(luts, view, d, TableAir::Full);
        let rain = rain_inscatter(luts, view, 0.0);
        let on_ground = add(scale(add(scale(surface, t), air), tw), scale(rain, 1.0 - tw));
        let on_sky = add(scale(horizon, tw), scale(rain, 1.0 - tw));
        for (slot, (sky, ground)) in [(horizon, off_ground), (on_sky, on_ground)]
            .into_iter()
            .enumerate()
        {
            let entry = &mut acc[slot];
            entry.0 += f64::from(length(sub(sky, ground)));
            entry.1 += f64::from(length(sky));
            entry.2 = std::array::from_fn(|c| entry.2[c] + f64::from(sky[c]) / 16.0);
            entry.3 = std::array::from_fn(|c| entry.3[c] + f64::from(ground[c]) / 16.0);
        }
    }
    let prediction = |(diff, norm, sky, ground): (f64, f64, [f64; 3], [f64; 3])| SeamPrediction {
        sky: sky.map(|v| v as f32),
        ground: ground.map(|v| v as f32),
        step: (diff / norm.max(1e-12)) as f32,
    };
    HorizonSeamModel {
        off: prediction(acc[0]),
        on: prediction(acc[1]),
        far_m: d,
        extinction_per_m: sigma,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luts(sun_elevation_deg: f32) -> AtmosphereLuts {
        let e = sun_elevation_deg.to_radians();
        crate::build_atmosphere_luts_for(
            &crate::AtmosphereParameters::earth(),
            [0.0, e.sin(), e.cos()],
            150.0,
        )
    }

    #[test]
    fn the_seam_of_a_uniform_image_is_zero_and_a_step_is_measured() {
        let (w, h) = (4u32, 4u32);
        let sky: Vec<bool> = (0..16).map(|i| i < 8).collect();
        let flat = vec![[1.0f32, 1.0, 1.0]; 16];
        let seam = measure_horizon_seam(&flat, &sky, w, h).expect("seam");
        assert_eq!(seam.columns, 4);
        assert!(seam.step.abs() < 1e-6 && seam.luminance_step.abs() < 1e-6);
        let mut stepped = flat;
        for p in stepped.iter_mut().skip(8) {
            *p = [0.5, 0.5, 0.5];
        }
        let seam = measure_horizon_seam(&stepped, &sky, w, h).expect("seam");
        assert!((seam.step - 0.5).abs() < 1e-5, "{seam:?}");
        assert!((seam.luminance_step - 0.5).abs() < 1e-5, "{seam:?}");
        assert_eq!(seam.ground, [0.5; 3]);
    }

    #[test]
    fn the_seam_refuses_bad_input() {
        assert!(measure_horizon_seam(&[[0.0; 3]; 3], &[true; 4], 2, 2).is_none());
        assert!(
            measure_horizon_seam(&[[0.0; 3]; 4], &[true; 4], 2, 2).is_none(),
            "no ground"
        );
        assert!(
            measure_horizon_seam(&[[0.0; 3]; 4], &[false; 4], 2, 2).is_none(),
            "no sky"
        );
        let nan = [[f32::NAN; 3], [f32::NAN; 3], [0.0; 3], [0.0; 3]];
        assert!(measure_horizon_seam(&nan, &[true, true, false, false], 2, 2).is_none());
        assert!(measure_horizon_seam(&[], &[], u32::MAX, u32::MAX).is_none());
    }

    #[test]
    fn region_luminance_splits_sky_and_ground() {
        let rgb = [[1.0; 3], [3.0; 3], [2.0; 3], [4.0; 3]];
        let (s, g) = region_luminance(&rgb, &[true, true, false, false]);
        assert!((s - 2.0).abs() < 1e-5 && (g - 3.0).abs() < 1e-5);
        assert_eq!(region_luminance(&rgb, &[false; 4]).0, 0.0);
    }

    /// The extension past the last slice is continuous at 32 km and meets the horizon sky.
    #[test]
    fn the_tables_carry_the_air_past_the_last_slice_to_the_horizon_sky() {
        let luts = luts(30.0);
        for view in [[0.0, -1e-4, 1.0], [1.0, -1e-4, 0.0], [0.0, -1e-4, -1.0]] {
            let view = normalize(view);
            let (edge, edge_t) = table_air(&luts, view, TABLE_AIR_RANGE_M, TableAir::Full);
            let (just, just_t) = table_air(&luts, view, TABLE_AIR_RANGE_M + 1.0, TableAir::Full);
            assert!(
                length(sub(edge, just)) <= 1e-3 * length(edge).max(1.0),
                "{edge:?} {just:?}"
            );
            assert!((edge_t - just_t).abs() < 1e-3);
            let (far, far_t) = table_air(&luts, view, 2.0e6, TableAir::Full);
            let horizon = horizon_sky(&luts, view);
            assert!(far_t < 1e-3, "{far_t}");
            assert!(
                length(sub(far, horizon)) < 0.02 * length(horizon),
                "{far:?} vs {horizon:?}"
            );
            // Off clamps at the last slice.
            let (off, _) = table_air(&luts, view, 2.0e6, TableAir::Off);
            assert_eq!(off, edge);
        }
    }

    /// Inside the range the full mode is the default table, and lite stays close to it.
    #[test]
    fn inside_the_range_full_is_the_table_and_lite_is_near_it() {
        let luts = luts(20.0);
        let view = normalize([0.3, -0.01, 1.0]);
        for d in [500.0, 1000.0, 5000.0, 20_000.0, 31_000.0] {
            let off = table_air(&luts, view, d, TableAir::Off);
            assert_eq!(table_air(&luts, view, d, TableAir::Full), off);
            let lite = table_air(&luts, view, d, TableAir::Lite);
            let rel = length(sub(lite.0, off.0)) / length(off.0).max(1e-6);
            assert!(rel < 0.12, "lite at {d} m differs by {rel}");
            assert!(
                (lite.1 - off.1).abs() < 0.05,
                "lite T at {d} m: {} vs {}",
                lite.1,
                off.1
            );
        }
    }

    /// The model: the switch closes the seam for far ground in clear air and in heavy rain.
    #[test]
    fn the_model_predicts_a_smaller_seam_with_the_switch() {
        let luts = luts(35.0);
        let (color, lux) = crate::transmitted_sun(&luts, &luts.params);
        let fog = crate::FogSettings {
            sky_inscatter: true,
            ..crate::FogSettings::default()
        };
        for sigma in [0.0, 3.912e-3] {
            let model = horizon_seam_model(
                &luts,
                &SeamInputs {
                    to_sun: luts.sun_direction,
                    sun_irradiance: color.map(|c| c * lux),
                    extinction_per_m: sigma,
                    far_m: 120_000.0,
                    fog: &fog,
                    exposure_multiplier: 1.0,
                },
            );
            assert!(model.on.step < model.off.step, "σ {sigma}: {model:?}");
            assert!(model.on.step.is_finite() && model.off.step.is_finite());
        }
    }

    /// ADR-3294: with no deck the rain's in-scatter is the mean of the horizon row and the lit
    /// ground plus the sun over 4π; a full deck greys the halves, dims them by
    /// `1 − CLOUD_OVERCAST_DIM`, and — it raining (ADR-3294 (13)) — blocks the sun and leaves the
    /// ground half diffuse-lit; a half deck lies between.
    #[test]
    fn rain_inscatter_is_the_sphere_average_and_the_deck_dims_it() {
        let luts = luts(45.0);
        let view = normalize([0.6, -1e-4, 0.8]);
        let sky = horizon_sky(&luts, view);
        let ground = ground_radiance(&luts);
        let (color, lux) = crate::transmitted_sun(&luts, &luts.params);
        let clear = rain_inscatter(&luts, view, 0.0);
        for c in 0..3 {
            let expect = 0.5 * (sky[c] + ground[c]) + color[c] * lux / (4.0 * std::f32::consts::PI);
            assert!(
                (clear[c] - expect).abs() <= 1e-4 * expect.max(1.0),
                "{clear:?} vs {expect}"
            );
        }
        // The deck rule (13): the sun share is 0 and the ground half lost its sun term.
        let cos_sun = luts.sun_direction[1].max(0.0);
        let diffuse: [f32; 3] = std::array::from_fn(|c| {
            (ground[c] - luts.params.ground_albedo[c] * color[c] * lux * cos_sun / std::f32::consts::PI)
                .max(0.0)
        });
        let keep = 1.0 - crate::CLOUD_OVERCAST_DIM;
        let grey = luminance(scale(add(sky, diffuse), 0.5)) * keep;
        let deck = rain_inscatter(&luts, view, 1.0);
        for c in deck {
            assert!(
                (c - grey).abs() <= 1e-3 * grey.max(1.0),
                "{deck:?} vs grey {grey}"
            );
        }
        let half = luminance(rain_inscatter(&luts, view, 0.5));
        assert!(
            luminance(deck) < half && half < luminance(clear),
            "{deck:?} {half} {clear:?}"
        );
        // The shader's split: the per-pixel half plus the uniform row.
        let split = add(
            crate::overcast_sky(scale(sky, 0.5), 0.5),
            rain_inscatter_constant(&luts, 0.5),
        );
        assert!((luminance(split) - half).abs() <= 1e-3 * half);
        // Bad deck input is clamped, never NaN.
        assert!(
            rain_inscatter(&luts, view, f32::NAN)
                .iter()
                .all(|c| c.is_finite())
        );
        assert_eq!(rain_inscatter(&luts, view, 7.0), deck);
    }

    /// ADR-3294 (12): the rain's deck is the covered column's opacity, which is the layer's
    /// overcast factor on a fully covered map and at least that on any map.
    #[test]
    fn rain_deck_opacity_is_the_covered_columns_overcast() {
        let settings = crate::CloudSettings {
            coverage: 1.0,
            ..crate::CloudSettings::default()
        };
        let full = crate::WeatherMap {
            resolution: 4,
            rgba: vec![255; 4 * 4 * 4],
            filled: 1.0,
        };
        let o = rain_deck_opacity(&settings);
        assert!(o > 0.0 && o <= 1.0, "{o}");
        let factor = crate::cloud_overcast_factor(&settings, &full);
        assert!((o - factor).abs() < 1e-5, "{o} vs {factor}");
        let half = crate::WeatherMap {
            resolution: 4,
            rgba: vec![100; 4 * 4 * 4],
            filled: 0.0,
        };
        assert!(rain_deck_opacity(&settings) >= crate::cloud_overcast_factor(&settings, &half));
        let none = crate::CloudSettings {
            coverage: 0.0,
            ..settings
        };
        assert_eq!(rain_deck_opacity(&none), 0.0);
    }

    #[test]
    fn haze_inscatter_matches_the_shader_constants() {
        // The horizon of the analytic sky, scaled, plus the sun lobe.
        let c = haze_inscatter([0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [0.0; 3], 1.0);
        let expect = analytic_sky_color(normalize([0.0, 0.06, 1.0]));
        assert!(length(sub(c, expect)) < 1e-6);
        let flat = forward_weather_inscatter(
            [0.0, 0.0, 1.0],
            [0.0, 1.0, 0.0],
            [0.0; 3],
            &crate::FogSettings::default(),
            2.0,
        );
        assert_eq!(flat, scale(crate::FogSettings::default().color, 0.5));
    }
}
