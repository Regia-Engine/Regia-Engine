// SPDX-License-Identifier: MPL-2.0
#import regia::utils
#import regia::cloud_shadow
#import regia::cloud_density::{remap, sat, type_gradient, cloud_altitude, cloud_layer_fraction, cloud_shell}
#import regia::cloud_density::{density_weather_uv, density_breakup_coord, density_breakup, density_cover}
#import regia::cloud_density::{density_shape_coord, density_base, density_detail_anchor, density_curl_uv}
#import regia::cloud_density::{density_curl_swirl, density_detail_coord, density_erode}

// Raymarched volumetric cloud layer (`src/clouds.rs`).
//
// - Shape: Schneider, "The Real-Time Volumetric Cloudscapes of Horizon: Zero Dawn",
//   SIGGRAPH 2015 (and GPU Pro 7): a weather map (R coverage, G cloud type), a tiling
//   3D Perlin–Worley + Worley FBM base shape eroded by a smaller Worley FBM detail
//   texture, per-type height gradients (Schneider & Vos, "Nubis", SIGGRAPH 2017).
// - Light: Beer's law with a short march toward the sun, the "Beer–Powder" darkening of
//   sun-facing edges (Schneider 2015), a dual-lobe Henyey–Greenstein phase and
//   Wrenninge-style multiple-scattering octaves (Hillaire, "Physically Based Sky,
//   Atmosphere and Cloud Rendering in Frostbite", SIGGRAPH 2016 course), energy-
//   conserving integration (Hillaire, "Physically-based & Unified Volumetric Rendering
//   in Frostbite", SIGGRAPH 2015), ambient from the sky.
// - Cone light (`cloud_cone_light`): the light march takes five taps spread inside a cone
//   toward the sun plus one long tap for distant cloud (Schneider 2015); the multiple-
//   scattering octaves are weighted by the in-scatter probability that darkens thin edges
//   and bases (Schneider & Vos 2017); the detail erosion is displaced by a 2D curl-noise
//   field (Bridson, Hourihan, Nordenstam, "Curl-Noise for Procedural Fluid Flow",
//   SIGGRAPH 2007), strongest at the bases.
// - Sun at altitude: the sun colour and lux transmitted to the layer base and top, from
//   the atmosphere's transmittance table, so the layer stays lit (and reddens) after the
//   camera's sunset. Aerial perspective in front of the cloud comes from the aerial LUT
//   at the cloud's scatter distance (Hillaire 2016; Hillaire, "A Scalable and Production
//   Ready Sky and Atmosphere Rendering Technique", EGSR 2020).
// - Cost: marched at a reduced resolution with interleaved-gradient-noise start jitter
//   (Jimenez 2014) that changes each frame. The upsample is nearest-depth (Shopf, GDC 2009),
//   so a cloud in front of a surface draws over it and a closer surface stays in front.
// - Shell: `|o|² − r²` expanded about the planet radius, then the stable quadratic
//   (Haines, Günther, Akenine-Möller, Ray Tracing Gems ch. 7, 2019). The product
//   `(length(o) − r)(length(o) + r)` still loses metres on a long chord in f32.
// - Steps: a big step four times the small one, refined on a hit, back to big after eight
//   empty small steps (Schneider, SIGGRAPH 2015). Inside the layer the small step starts
//   at 20 m and grows so the fade span finishes in the step budget.
// - Offline (ADR-1403, the Cinematic profile): `hdr.w` marches per texel, each with its start
//   jitter in its own stratum (`(ign + k) / n`, stratified sampling) and its ray through point k
//   of the R2 sequence inside the texel (Roberts 2018), averaged in linear light. One march
//   (every real-time preset) is the old centred march, unchanged.
// - Temporal: each frame marches one texel of every 4×4 block (Bayer order) into a small
//   "fresh" target. A reconstruct pass reprojects the previous result along each texel's
//   scatter distance with the previous unjittered view-projection (wind drift removed),
//   fetches it depth-aware with a Catmull-Rom filter, clamps it toward the 3×3 fresh
//   neighbourhood as far as the block's fresh sample shows the history is stale, and
//   falls back to the fresh texels on the same surface where the history is off screen or
//   disoccluded (Schneider 2015; Hillaire 2016; Bauer, RDR2, SIGGRAPH 2019). A camera cut
//   marches every texel.
// Froxel fog still integrates through this band. Subtracting it would change the sky
// outside this lane, so the double count stays.

struct CloudGpu {
    inv_view_proj: mat4x4<f32>,
    // Toward the sun (xyz), sun illuminance in lux (w).
    sun: vec4<f32>,
    // Sun colour (rgb), multiple-scattering strength (w).
    sun_color: vec4<f32>,
    // Bottom, top (m), extinction at full density (1/m), coverage.
    layer: vec4<f32>,
    // Wind x, z (m/s), time (s), weather tile size (m).
    wind: vec4<f32>,
    // Shape tile (m), detail tile (m), cloud type 0..1, planet radius (m).
    shape: vec4<f32>,
    // Eye (xyz), frame index (w).
    camera: vec4<f32>,
    // Sky ambient radiance at the cloud tops (rgb), fade distance (m).
    ambient_top: vec4<f32>,
    // Ambient at the cloud bases (rgb), single-scattering albedo (w).
    ambient_bottom: vec4<f32>,
    // Mode, view steps, map resolution, shadow floor.
    draw: vec4<f32>,
    // Low-res width, height; full width, height.
    targets: vec4<f32>,
    // x = the HDR target's pre-exposure (the composite writes radiance × x).
    // y = the march's resolution divisor (full-res texels per low-res texel).
    // z = the cloud transmittance map's extent (its debug view).
    // w = marches averaged per texel (1 = the single centred march; ADR-1403).
    hdr: vec4<f32>,
    // Inverse of the unjittered view-projection. View rays come from this matrix.
    // `inv_view_proj` still unprojects the jittered depth buffer.
    ray_from_clip: mat4x4<f32>,
    // Unjittered view-projection of the frame that wrote the history being read.
    history_view_proj: mat4x4<f32>,
    // x, y = the 4×4 sub-position this frame marches; z, w = fresh target width, height.
    temporal: vec4<f32>,
    // x, y = wind drift (m, world x and z) since the history frame; z = 1 when the history
    // is valid; w = how far the weather (coverage, type, density) moved over the refresh
    // window, 0..1 of a weather cut (ADR-429). The reconstruct reads `stale` (ADR-1410).
    reproject: vec4<f32>,
    // Sun colour × lux transmitted to the layer base (rgb); w = 1 when the aerial LUT is
    // bound (otherwise the march keeps the exponential distance fade).
    sun_bottom: vec4<f32>,
    // The same at the layer top (rgb); w = 1 for the cone light march.
    sun_top: vec4<f32>,
    // x = curl displacement of the detail erosion (m), y = dark-edge (in-scatter
    // probability) strength 0..1, z = curl tile (m), w = cone length / layer thickness.
    light: vec4<f32>,
    // Staleness of the history of each 4×4 sub-position, `[y][x]`, 0..1 (ADR-1410): how far
    // the weather moved since that texel was last marched (for this frame's sub-position, since
    // its march before this one), in 1/16 of a weather cut.
    stale: array<vec4<f32>, 4>,
    // Cloud look (ADR-1408, `cloud_types.rs`): x = diffusion strength (0: the exponential
    // octaves), y = detail erosion (0.5 before), z = edge breakup amplitude, w = breakup tile (m).
    look: vec4<f32>,
    // Cirrus sheet: x = coverage (0: none), y = altitude (m), z = vertical optical depth,
    // w = streak tile across the wind (m).
    cirrus: vec4<f32>,
    // The sun transmitted to the sheet (colour × lux, rgb); w = streak stretch along the wind.
    cirrus_sun: vec4<f32>,
    // Streak direction on world x, z (unit); the sheet's drift (m, x and z).
    cirrus_dir: vec4<f32>,
    // Fibrous cirrus (ADR-1509): fiber strength, fiber scale (m), shear amount, hook amount.
    cirrus_fiber: vec4<f32>,
    // Cosine and sine of the shear direction from the wind, breakup, 1 when fibrous (0 draws
    // the ADR-1470 trails).
    cirrus_shear: vec4<f32>,
    // Crisp cirrus (ADR-1516): x = crisp amount (0: the ADR-1509 sheet), y = sheet samples per
    // march texel (1 when not crisp), zw unused.
    cirrus_crisp: vec4<f32>,
}

@group(0) @binding(0) var<uniform> u: CloudGpu;
@group(0) @binding(1) var weather_tex: texture_2d<f32>;
@group(0) @binding(2) var linear_samp: sampler;
@group(0) @binding(3) var scene_depth: texture_depth_2d;
@group(0) @binding(4) var shape_tex: texture_3d<f32>;
@group(0) @binding(5) var detail_tex: texture_3d<f32>;
@group(0) @binding(6) var cloud_tex: texture_2d<f32>;
// R = transmittance-weighted scatter distance (m), G = footprint-centre scene distance (m),
// B = confidence of the reconstructed value (1 marched, 0 filled from a neighbour),
// A = softness: how much filtered resampling the value has been through since it was marched.
@group(0) @binding(7) var cloud_dist: texture_2d<f32>;
// Previous reconstructed clouds and their distances (reconstruct pass only).
@group(0) @binding(8) var history_tex: texture_2d<f32>;
@group(0) @binding(9) var history_dist: texture_2d<f32>;
// The aerial-perspective LUT of `outdoor_sky.wgsl` (16 slices side by side), or a 1×1
// stand-in when `u.sun_bottom.w` is 0.
@group(0) @binding(10) var aerial_lut: texture_2d<f32>;
// Tileable 2D curl field (RG, 0..1 encoding −1..1), `clouds::cloud_curl`.
@group(0) @binding(11) var curl_tex: texture_2d<f32>;

// Half floats saturate at 65504. Sky, and any farther hit, stores this sentinel.
const SKY_M: f32 = 60000.0;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
}

// Full-screen triangle. No function-local struct or array `var` (ADR-026).
@vertex
fn vs_fullscreen(@builtin(vertex_index) id: u32) -> VsOut {
    let x = select(-1.0, 3.0, id == 1u);
    let y = select(-1.0, 3.0, id == 2u);
    return VsOut(vec4<f32>(x, y, 0.0, 1.0));
}

const PI: f32 = 3.14159265359;

fn weather_at(xz: vec2<f32>) -> vec2<f32> {
    let uv = density_weather_uv(xz, u.wind.xy * u.wind.z, u.wind.w);
    return textureSampleLevel(weather_tex, linear_samp, uv, 0.0).rg;
}

// Height of `p` above the planet surface, with the camera standing at the local origin.
fn altitude(p: vec3<f32>) -> f32 {
    return cloud_altitude(p, u.shape.w);
}

fn layer_fraction(alt: f32) -> f32 {
    return cloud_layer_fraction(alt, u.layer.x, u.layer.y);
}

// Normalised density 0..1 at `p`. `cheap` skips the detail erosion (light march). The arithmetic
// is `regia::cloud_density`, shared with the path tracer's cloud field (ADR-2127); this function
// fetches the textures.
fn density_at(p: vec3<f32>, cheap: bool) -> f32 {
    let alt = altitude(p);
    let h = layer_fraction(alt);
    if (alt < u.layer.x || alt > u.layer.y) {
        return 0.0;
    }
    var w = weather_at(p.xz);
    if (u.look.z > 0.0) {
        let bc = density_breakup_coord(p.xz, u.wind.xy * u.wind.z, u.look.w);
        let bn = textureSampleLevel(shape_tex, linear_samp, bc, 0.0);
        w.x = density_breakup(w.x, u.look.z, bn);
    }
    let cov = density_cover(w.x, u.layer.w);
    let grad = type_gradient(h, sat(w.y));
    if (cov * grad <= 0.0) {
        return 0.0;
    }
    let drift = vec3<f32>(u.wind.x, 0.0, u.wind.y) * u.wind.z;
    let sp = density_shape_coord(p, drift, u.shape.x);
    let s = textureSampleLevel(shape_tex, linear_samp, sp, 0.0);
    let base = density_base(s, cov, grad);
    if (base <= 0.0 || cheap) {
        return base;
    }
    var dq = density_detail_anchor(p, drift);
    if (u.light.x > 0.0) {
        let cuv = density_curl_uv(dq, u.light.z);
        let curl = textureSampleLevel(curl_tex, linear_samp, cuv, 0.0).rg;
        dq = density_curl_swirl(dq, curl, u.light.x, h);
    }
    let dp = density_detail_coord(dq, u.shape.y);
    let d = textureSampleLevel(detail_tex, linear_samp, dp, 0.0);
    return density_erode(base, d, h, u.look.y);
}

fn hg(cos_t: f32, g: f32) -> f32 {
    let g2 = g * g;
    let denom = max(1.0 + g2 - 2.0 * g * cos_t, 1e-4);
    return (1.0 - g2) / (4.0 * PI * denom * sqrt(denom));
}

// Dual-lobe phase: a strong forward silver lining and a weaker back lobe.
fn dual_hg(cos_t: f32, scale: f32) -> f32 {
    return mix(hg(cos_t, 0.8 * scale), hg(cos_t, -0.3 * scale), 0.3);
}

// Interleaved gradient noise (Jimenez 2014), offset each frame.
fn ign(px: vec2<f32>, frame: f32) -> f32 {
    let p = px + vec2<f32>(5.588238, 5.588238) * frame;
    return fract(52.9829189 * fract(dot(p, vec2<f32>(0.06711056, 0.00583715))));
}

// Distances along unit `dir` from `eye` to a sphere of radius `r` around the planet centre
// (`regia::cloud_density::cloud_shell`).
fn shell(eye: vec3<f32>, dir: vec3<f32>, r: f32) -> vec2<f32> {
    return cloud_shell(eye, dir, r, u.shape.w);
}

// `1 + r + … + r^(n−1)`, at most `CloudMarch::STEP_LIMIT` (256) terms. Adding the
// powers avoids cancelling `(r^n − 1) / (r − 1)` when `r` is near 1. Matches
// `geometric_series_sum` in `clouds.rs`.
fn geometric_series_sum(r: f32, n: i32) -> f32 {
    var sum = 0.0;
    var term = 1.0;
    let count = min(max(n, 0), 256);
    for (var i = 0; i < 256; i++) {
        if (i >= count) {
            break;
        }
        sum += term;
        term *= r;
    }
    return sum;
}

// `r ≥ 1` such that `n` terms starting at 1 sum to `sum_units`. Binary search, matching
// `geometric_ratio` in `clouds.rs`. Returns 1 when `n` steps of length 1 already cover it.
fn geometric_ratio(sum_units: f32, n: i32) -> f32 {
    if (n <= 1 || sum_units <= f32(max(n, 1))) {
        return 1.0;
    }
    var lo = 1.0;
    var hi = 2.0;
    for (var k = 0; k < 8; k++) {
        if (geometric_series_sum(hi, n) >= sum_units) {
            break;
        }
        hi *= 2.0;
    }
    for (var k = 0; k < 20; k++) {
        let mid = 0.5 * (lo + hi);
        if (geometric_series_sum(mid, n) < sum_units) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    return 0.5 * (lo + hi);
}

// Growth ratio for an inside-layer ray. 1 when 20 m steps already cover the segment.
// Matches the `eye_inside` branch of `cloud_small_step`.
fn inside_ratio(span: f32, steps: i32) -> f32 {
    let near = 20.0;
    let n = max(steps, 1);
    if (span <= near * f32(n)) {
        return 1.0;
    }
    return geometric_ratio(span / near, n);
}

// View-march small step (metres). Outside the layer it is a fraction of the span over
// the step budget, growing with distance and floored at 4 m. Inside the layer it starts
// at 20 m (`inside_r` from `inside_ratio`) and grows so `steps` small steps sum to the
// segment: a 67.5 km fade span finishes in 64 steps. Matches `cloud_small_step`.
fn small_step(t: f32, t0: f32, t1: f32, steps: i32, eye_inside: bool, inside_r: f32) -> f32 {
    let span = max(t1 - t0, 1.0);
    let along = clamp(t - t0, 0.0, span);
    if (eye_inside) {
        return 20.0 + (inside_r - 1.0) * along;
    }
    let cover = span / f32(max(steps, 1));
    let ramp = mix(0.35, 1.8, sat(along / span) * sat(along / span));
    let grown = cover * ramp;
    let cap = max(cover * 2.0, 4.0);
    return clamp(grown, 4.0, cap);
}

// Optical depth toward the sun from `p` over six growing steps (cone-free, Schneider 2015).
fn light_depth(p: vec3<f32>) -> f32 {
    let thickness = max(u.layer.y - u.layer.x, 1.0);
    let base_step = thickness * 0.06;
    var od = 0.0;
    var t = 0.0;
    for (var i = 0; i < 6; i++) {
        let step = base_step * (1.0 + f32(i) * 0.9);
        t += step;
        let q = p + u.sun.xyz * (t - 0.5 * step);
        od += density_at(q, i >= 3) * step;
    }
    return od;
}

// Cone half-angle tangent (about 10°). The cone's length, as a fraction of the layer
// thickness, is `u.light.w`.
const CONE_TAN: f32 = 0.18;

// Optical depth toward the sun through a cone (Schneider 2015): five taps over half the
// layer thickness with steps growing 1 : 1.5 : 2 : 2.5 : 3, each pushed off the sun axis
// by a golden-angle spiral (rotated per sample by `seed`) out to the cone's radius at that
// distance. The first three taps erode with the detail noise, as the straight march does
// (with two, the uneroded fringe under a sunlit top shadows it and the tops go grey); the
// rest and one long tap, standing for the segment out to three times the cone, read the
// base shape only.
fn light_depth_cone(p: vec3<f32>, seed: f32) -> f32 {
    let thickness = max(u.layer.y - u.layer.x, 1.0);
    let span = max(u.light.w, 0.05) * thickness;
    let s = u.sun.xyz;
    let helper = select(vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(1.0, 0.0, 0.0), abs(s.y) > 0.9);
    let e1 = normalize(cross(s, helper));
    let e2 = cross(s, e1);
    var od = 0.0;
    var t = 0.0;
    for (var i = 0; i < 5; i++) {
        // Weights 1, 1.5, 2, 2.5, 3 sum to 10.
        let step = span * (1.0 + 0.5 * f32(i)) / 10.0;
        let mid = t + 0.5 * step;
        t += step;
        let angle = (f32(i) + seed) * 2.39996;
        let radius = sqrt((f32(i) + 0.5) / 5.0) * mid * CONE_TAN;
        let q = p + s * mid + (e1 * cos(angle) + e2 * sin(angle)) * radius;
        od += density_at(q, i >= 3) * step;
    }
    // Long tap in the middle of [span, 3·span], weighted by that segment.
    od += density_at(p + s * (2.0 * span), true) * (2.0 * span);
    return od;
}

// In-scatter probability (Schneider & Vos 2017): multiple scattering builds up inside a
// cloud, so thin wisps and the undersides read darker than dense cores. A depth term that
// rises with density (sharper toward the top) times a vertical term that fades in over the
// lowest part of the layer.
fn inscatter_probability(dens: f32, h: f32) -> f32 {
    let depth_p = 0.05 + pow(max(dens, 0.0), clamp(remap(h, 0.3, 0.85, 0.5, 2.0), 0.5, 2.0));
    let vert_p = pow(sat(remap(h, 0.07, 0.14, 0.1, 1.0)), 0.8);
    return sat(depth_p * vert_p);
}

// Diffuse transmission of a conservative-scattering slab of optical depth `tau`, droplet
// asymmetry 0.85: `1 / (1 + ¾ (1 − g) τ)` (two-stream; Bohren, Am. J. Phys. 55, 1987). Matches
// `cloud_types::diffuse_transmission`.
fn diffuse_transmission(tau: f32) -> f32 {
    return 1.0 / (1.0 + 0.75 * (1.0 - 0.85) * max(tau, 0.0));
}

// In-scatter over one step at unit transmittance (xyz) and the step's transmittance (w).
// Energy-conserving ∫ S·T (Hillaire 2015). The caller multiplies xyz by the transmittance
// arriving at the step. `seed` rotates the cone taps (cone light only).
fn shade_step(p: vec3<f32>, dens: f32, dt: f32, cos_t: f32, seed: f32) -> vec4<f32> {
    let sigma = u.layer.z;
    let albedo = u.ambient_bottom.w;
    let h = layer_fraction(altitude(p));
    // The sun as transmitted to this height in the layer.
    let sun_rad = mix(u.sun_bottom.rgb, u.sun_top.rgb, h);
    let sigma_t = dens * sigma;
    let sigma_s = sigma_t * albedo;
    let cone = u.sun_top.w > 0.5;
    var od = 0.0;
    if (cone) {
        od = light_depth_cone(p, seed);
    } else {
        od = light_depth(p);
    }
    let tau_l = od * sigma;
    // Multiple-scattering octaves: scattering, extinction and eccentricity
    // halve per octave (Wrenninge 2013, via Hillaire 2016). Octaves 1 and 2 are the
    // multiple scattering and carry the in-scatter probability.
    let single = dual_hg(cos_t, 1.0) * exp(-tau_l);
    var multi = 0.0;
    if (u.look.x > 0.0) {
        // Diffuse light (ADR-1408): the multiply scattered sun as a two-stream field whose
        // transmission falls as 1 / τ (Bohren 1987), nearly isotropic.
        multi = u.look.x * diffuse_transmission(tau_l) / (4.0 * PI);
    } else {
        var a = u.sun_color.w;
        var b = 0.5;
        var c = 0.5;
        for (var o = 1; o < 3; o++) {
            multi += a * dual_hg(cos_t, c) * exp(-b * tau_l);
            a *= u.sun_color.w;
            b *= 0.5;
            c *= 0.5;
        }
    }
    // With the diffuse light (ADR-1408) its transmission already holds how far the light is
    // inside the cloud, so the in-scatter probability and the Beer–Powder rim darkening are
    // kept at a third: at full strength they took three quarters off a sunlit face seen with
    // the sun behind the camera (grey cumulus).
    let soften = select(1.0, 0.35, u.look.x > 0.0);
    let prob = mix(1.0, inscatter_probability(dens, h), u.light.y * soften);
    let sun_term = single + prob * multi;
    // Beer–Powder: thin, sun-facing edges in-scatter less; strongest when
    // looking away from the sun.
    let powder = 1.0 - exp(-2.0 * tau_l - 0.15);
    let powder_mix = mix(1.0, powder, (0.5 - 0.5 * cos_t) * soften);
    let ambient = mix(u.ambient_bottom.rgb, u.ambient_top.rgb, h);
    let s = sigma_s * (sun_rad * sun_term * powder_mix + ambient);
    let step_t = exp(-sigma_t * dt);
    let integ = (s - s * step_t) / max(sigma_t, 1e-6);
    return vec4<f32>(integ, step_t);
}

// Marches one view ray out to `far_t` (the farthest surface in the footprint, or the
// sky sentinel). Returns in-scattered radiance (rgb) and transmittance (a). Writes the
// transmittance-weighted mean scatter distance (metres, 0 when the ray misses cloud).
//
// Froxel fog still walks this same band. That double count is left in place.
fn march(eye: vec3<f32>, dir: vec3<f32>, jitter: f32, far_t: f32, scatter_d: ptr<function, f32>) -> vec4<f32> {
    *scatter_d = 0.0;
    // A cirrus sheet alone runs the pass with no volumetric coverage (ADR-1408).
    if (u.layer.w <= 0.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    let r = u.shape.w;
    let r_eye = r + altitude(eye);
    let bottom = shell(eye, dir, r + u.layer.x);
    let top = shell(eye, dir, r + u.layer.y);
    var t0 = 0.0;
    var t1 = 0.0;
    let eye_inside = r_eye >= r + u.layer.x && r_eye <= r + u.layer.y;
    if (r_eye < r + u.layer.x) {
        // Below the layer: enter at the base, leave at the top.
        if (bottom.y <= 0.0) {
            return vec4<f32>(0.0, 0.0, 0.0, 1.0);
        }
        t0 = bottom.y;
        t1 = top.y;
    } else if (eye_inside) {
        // Inside: march to the nearer exit.
        t0 = 0.0;
        t1 = select(top.y, min(top.y, bottom.x), bottom.x > 0.0);
    } else {
        if (top.x <= 0.0) {
            return vec4<f32>(0.0, 0.0, 0.0, 1.0);
        }
        t0 = top.x;
        t1 = select(top.y, bottom.x, bottom.x > 0.0);
    }
    let fade = max(u.ambient_top.w, 1.0);
    // 1.5 matches `CLOUD_FADE_MULTIPLE`. With the 45 km fade this is a 67.5 km span.
    t1 = min(t1, t0 + fade * 1.5);
    // `far_t == SKY_M` means the footprint saw sky. That sentinel is only how far a
    // half float can store a hit; it must not cut a shell that starts past 60 km
    // (a grazing horizon ray enters the layer around there). A real surface still
    // clamps the march.
    if (far_t < SKY_M) {
        t1 = min(t1, max(far_t, 0.0));
    }
    if (t1 <= t0) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    let steps = max(i32(u.draw.y), 1);
    // Only an inside-layer ray needs the geometric ratio. `select` would still
    // evaluate it, and the search is a few thousand adds.
    var inside_r = 1.0;
    if (eye_inside) {
        inside_r = inside_ratio(max(t1 - t0, 1.0), steps);
    }
    let cos_t = dot(dir, u.sun.xyz);
    var transmit = 1.0;
    var scatter = vec3<f32>(0.0);
    var dist_sum = 0.0;
    var weight_sum = 0.0;
    var mode_big = true;
    var empty_run = 0;
    // Empty small steps count only after the ray has passed the hit that started the refine.
    var refine_until = -1.0;
    // Density of the last cloud sample and a forced next step (ADR-1408 optical step cap).
    var dens_prev = 0.0;
    var force_step = -1.0;
    var t = t0;
    // A camera inside a cloud would otherwise spend its first big step in the dark.
    if (eye_inside) {
        let dens0 = density_at(eye, false);
        let dt0 = min(20.0, t1 - t0);
        if (dens0 > 0.0 && dt0 > 0.0) {
            let lit = shade_step(eye, dens0, dt0, cos_t, jitter);
            scatter += transmit * lit.xyz;
            let w = transmit * (1.0 - lit.w);
            dist_sum += w * (t0 + 0.5 * dt0);
            weight_sum += w;
            transmit *= lit.w;
            t = t0 + dt0;
            mode_big = false;
            refine_until = t;
        }
    }
    if (mode_big) {
        let ss0 = small_step(t0, t0, t1, steps, eye_inside, inside_r);
        t = t0 + ss0 * jitter;
    }
    // Loop bound = `CloudMarch::STEP_LIMIT`; the real-time presets stop at 96 steps.
    for (var i = 0; i < 256; i++) {
        if (i >= steps || t >= t1 || transmit < 0.01) {
            break;
        }
        let ss = small_step(t, t0, t1, steps, eye_inside, inside_r);
        let big = ss * 4.0;
        var step = select(ss, big, mode_big);
        if (!mode_big && u.look.x > 0.0) {
            // Inside a cloud the step stays under two optical depths (ADR-1408): the lit rind
            // of a dense cloud is about 1/σ deep, and a 100 m step shaded every surface like
            // its interior (flat, foggy clouds). A dense ray ends in a few such steps.
            if (force_step > 0.0) {
                step = force_step;
                force_step = -1.0;
            } else if (dens_prev > 0.0) {
                step = min(step, max(2.0 / (max(u.layer.z, 1e-4) * max(dens_prev, 0.05)), 4.0));
            }
        }
        let dt = min(step, t1 - t);
        if (dt <= 1e-3) {
            break;
        }
        let sample_t = t + 0.5 * dt;
        let p = eye + dir * sample_t;
        if (mode_big) {
            if (density_at(p, true) > 0.0) {
                // The cheap sample hit cloud inside this big step. `t` is still the
                // start of that step, so the small march begins here (Schneider 2015
                // steps back to where the large step entered the cloud). Subtracting
                // another big step would re-walk the previous empty interval.
                mode_big = false;
                refine_until = t + dt;
                empty_run = 0;
                continue;
            }
            t += dt;
        } else {
            let cheap = density_at(p, true);
            var dens = 0.0;
            if (cheap > 0.0) {
                dens = density_at(p, false);
            }
            if (dens > 0.0 && u.look.x > 0.0 && dens_prev <= 0.0) {
                // A long step that enters a dense cloud steps back and enters it in short
                // steps, so the first sample lies in the lit rind, not deep inside.
                let optical = max(2.0 / (max(u.layer.z, 1e-4) * max(dens, 0.05)), 4.0);
                if (dt > optical * 1.5) {
                    dens_prev = dens;
                    force_step = optical;
                    continue;
                }
            }
            if (dens > 0.0) {
                dens_prev = dens;
                empty_run = 0;
                // A new cone rotation per sample (golden-ratio sequence from the ray's jitter).
                let lit = shade_step(p, dens, dt, cos_t, fract(jitter + 0.618034 * f32(i)));
                scatter += transmit * lit.xyz;
                let w = transmit * (1.0 - lit.w);
                dist_sum += w * sample_t;
                weight_sum += w;
                transmit *= lit.w;
            } else if (t >= refine_until) {
                empty_run += 1;
                if (empty_run >= 8) {
                    mode_big = true;
                    empty_run = 0;
                    dens_prev = 0.0;
                }
            }
            t += dt;
        }
    }
    var mean_d = 0.0;
    if (weight_sum > 0.0) {
        mean_d = dist_sum / weight_sum;
        *scatter_d = min(mean_d, SKY_M);
    }
    if (u.sun_bottom.w > 0.5) {
        if (weight_sum <= 0.0) {
            return vec4<f32>(0.0, 0.0, 0.0, 1.0);
        }
        // The air between the eye and the cloud (Hillaire 2016, 2020): the cloud's light is
        // dimmed by the air's transmittance and its opacity hides the air's in-scatter that
        // the sky behind it already holds. The composite is `bg · a + rgb`, so
        // `rgb = T_air · S_cloud + (1 − T_cloud) · S_air`. Past the table's 32 km the cloud
        // also fades out over the old 45 km scale, so horizon undersampling stays hidden.
        let air = aerial_at(dir, mean_d);
        let beyond = max(mean_d - AERIAL_RANGE_M, 0.0);
        let keep = exp(-beyond / fade);
        let a = mix(1.0, transmit, keep);
        let rgb = (air.a * scatter) * keep + (1.0 - a) * air.rgb;
        return vec4<f32>(rgb, a);
    }
    // No table: the segment heuristic fade. The stored scatter distance is the weighted
    // mean above, for the depth test, and does not move this fade.
    let dist = t0 + 0.5 * (t1 - t0) * (1.0 - transmit);
    let keep = exp(-dist / fade);
    return vec4<f32>(scatter * keep, mix(1.0, transmit, keep));
}

// Aerial-perspective table: 16 slices side by side, slice i the integral out to
// (i + 1) · 2 km. Matches `AERIAL_SLICES` / `aerial_at` in `outdoor_sky.wgsl`.
const AERIAL_SLICES: i32 = 16;
const AERIAL_STEP_KM: f32 = 2.0;
const AERIAL_RANGE_M: f32 = 32000.0;

// Sky-view / aerial table coordinates: azimuth from the sun (u), non-linear latitude (v,
// Hillaire 2020 § 5.3). Matches `sky_uv` in `outdoor_sky.wgsl`.
fn aerial_uv(view: vec3<f32>, sun: vec3<f32>) -> vec2<f32> {
    let up = vec3<f32>(0.0, 1.0, 0.0);
    let sun_h = sun - up * dot(sun, up);
    let view_h = view - up * dot(view, up);
    let sun_n = select(vec3<f32>(0.0, 0.0, 1.0), normalize(sun_h), length(sun_h) > 1e-4);
    let view_n = select(up, normalize(view_h), length(view_h) > 1e-4);
    let sin_az = dot(cross(sun_n, view_n), up);
    let cos_az = clamp(dot(sun_n, view_n), -1.0, 1.0);
    let azimuth = atan2(sin_az, cos_az);
    let uu = fract(azimuth / 6.28318530718 + 0.5);
    let lat = asin(clamp(view.y, -1.0, 1.0));
    let vv = 0.5 + 0.5 * sign(lat) * sqrt(abs(lat) / 1.57079632679);
    return vec2<f32>(uu, clamp(vv, 0.0, 1.0));
}

fn aerial_fetch(x: i32, y: i32, slice: i32, w: i32, h: i32) -> vec4<f32> {
    var xi = x % w;
    if (xi < 0) {
        xi += w;
    }
    let yi = clamp(y, 0, h - 1);
    return textureLoad(aerial_lut, vec2<i32>(xi + slice * w, yi), 0);
}

// Bilinear inside one slice (the table is not filterable): azimuth wraps, latitude clamps.
fn aerial_texel(uv: vec2<f32>, slice: i32) -> vec4<f32> {
    let dims = vec2<i32>(textureDimensions(aerial_lut));
    let w = max(dims.x / AERIAL_SLICES, 1);
    let h = max(dims.y, 1);
    let fu = fract(uv.x) * f32(w) - 0.5;
    let fv = clamp(uv.y, 0.0, 1.0) * f32(h) - 0.5;
    let x0 = i32(floor(fu));
    let y0 = i32(floor(fv));
    let tx = fu - floor(fu);
    let ty = fv - floor(fv);
    let a = aerial_fetch(x0, y0, slice, w, h);
    let b = aerial_fetch(x0 + 1, y0, slice, w, h);
    let c = aerial_fetch(x0, y0 + 1, slice, w, h);
    let d = aerial_fetch(x0 + 1, y0 + 1, slice, w, h);
    return mix(mix(a, b, tx), mix(c, d, tx), ty);
}

// In-scatter (rgb) and transmittance (a) of the air out to `dist_m` along `view`. Below the
// first slice it blends from clear air.
fn aerial_at(view: vec3<f32>, dist_m: f32) -> vec4<f32> {
    let uv = aerial_uv(view, u.sun.xyz);
    let pos = clamp((dist_m / 1000.0) / AERIAL_STEP_KM, 0.0, f32(AERIAL_SLICES));
    if (pos <= 1.0) {
        return mix(vec4<f32>(0.0, 0.0, 0.0, 1.0), aerial_texel(uv, 0), pos);
    }
    let f = pos - 1.0;
    let s0 = min(i32(floor(f)), AERIAL_SLICES - 1);
    let s1 = min(s0 + 1, AERIAL_SLICES - 1);
    let t = f - f32(s0);
    return mix(aerial_texel(uv, s0), aerial_texel(uv, s1), t);
}

// Rayleigh scale height of the Earth's air (m; U.S. Standard Atmosphere 1976). In a stratified
// atmosphere the fraction of the air along any upward ray that lies above height h is
// exp(−h / H), whatever the ray's elevation. Matches `CIRRUS_AIR_SCALE_HEIGHT_M`.
const CIRRUS_AIR_SCALE_HEIGHT_M: f32 = 8000.0;

// Cirrus ice phase function (ADR-1425): two Henyey–Greenstein terms (Kattawar 1975), a sharp
// forward lobe for the diffraction peak (g 0.9, weight 0.7) and a broad one for the refracted
// and side-scattered light (g 0.4), mean asymmetry 0.75 as measured for cirrus ice (Macke et al.
// 1996, Baum et al. 2005). Matches `cirrus_phase` in `cloud_types.rs`.
fn cirrus_phase(cos_t: f32) -> f32 {
    return 0.7 * hg(cos_t, 0.9) + 0.3 * hg(cos_t, 0.4);
}

// Cirrus fallstreaks (ADR-1470). Every constant matches the `CIRRUS_*` one of the same name in
// `cloud_types.rs`, and `cirrus_density` matches `cirrus_density` there line for line.
// Generating-cell grid: one cell is `stretch` streak tiles along the wind and one tile across.
const CIRRUS_CELL_ALONG: f32 = 1.0;
// Fraction of generating cells that hold a head.
const CIRRUS_CELL_ACTIVE: f32 = 0.92;
// Faint ice veil between the trails, inside a band.
const CIRRUS_VEIL: f32 = 0.07;
// Area mean of the trails (fibres included) with every cell in a band; the far-field value.
const CIRRUS_TRAIL_MEAN: f32 = 0.078;
// Peak density of the strongest trail heads (overlaps clip at 1).
const CIRRUS_TRAIL_GAIN: f32 = 1.6;
// Depth of the fibre modulation inside a trail, 0..1.
const CIRRUS_FIBRE_CONTRAST: f32 = 0.85;
// Keeps the fibre modulation's area mean at about 1.
const CIRRUS_FIBRE_GAIN: f32 = 2.4;
// Fibre-noise cells per streak tile across a trail (one bright fibre per one to two cells).
const CIRRUS_FIBRE_CELLS: f32 = 14.0;
// A trail's cross profile is widened to at least this many footprints (and its amplitude
// lowered to match), so it never narrows below the texel.
const CIRRUS_TRAIL_FILTER: f32 = 4.0;

// pcg2d (Jarzynski & Olano, "Hash Functions for GPU Rendering", JCGT 9(3), 2020): eight bytes of
// hash for an integer cell.
fn cirrus_pcg2d(c: vec2<i32>) -> vec2<u32> {
    var v = bitcast<vec2<u32>>(c) * 1664525u + vec2<u32>(1013904223u);
    v.x += v.y * 1664525u;
    v.y += v.x * 1664525u;
    v = v ^ (v >> vec2<u32>(16u));
    v.x += v.y * 1664525u;
    v.y += v.x * 1664525u;
    v = v ^ (v >> vec2<u32>(16u));
    return v;
}

// Byte `k` (0..3) of `h` as 0..1.
fn cirrus_byte(h: u32, k: u32) -> f32 {
    return f32((h >> (8u * k)) & 255u) / 255.0;
}

// The PCG output hash of one integer (Jarzynski & Olano 2020), 0..1 in 24 bits.
fn cirrus_hash1(n: i32) -> f32 {
    var h = bitcast<u32>(n) * 747796405u + 2891336453u;
    h = ((h >> ((h >> 28u) + 4u)) ^ h) * 277803737u;
    h = (h >> 22u) ^ h;
    return f32(h >> 8u) / 16777216.0;
}

// 1D gradient noise (Perlin 2002 quintic fade), about −0.5..0.5.
fn cirrus_noise1(x: f32) -> f32 {
    let i = floor(x);
    let f = x - i;
    let n = i32(i);
    let g0 = cirrus_hash1(n) * 2.0 - 1.0;
    let g1 = cirrus_hash1(n + 1) * 2.0 - 1.0;
    let w = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);
    return mix(g0 * f, g1 * (f - 1.0), w);
}

// Normalised ice density (0..1) of the cirrus sheet at `x` streak tiles along the wind and `y`
// across it, inside a band of strength `band`, for a pixel footprint of `footprint` tiles across
// the trails (ADR-1470).
//
// Cirrus uncinus and fibratus are fallstreaks: ice crystals grow in small generating cells near
// the cloud top and fall out of them; the wind shear between the levels they fall through lays
// each trail out downwind as a hook-headed streak that widens and thins as crystals of
// different sizes fall at different speeds and spread (Marshall 1953; Heymsfield 1975). So:
// generating cells sit at random points of a staggered grid (most hold a head, some do not);
// each draws a soft head and a tapering trail of random length, width, hook and lateral drift;
// overlapping trails add. Inside the strongest trail, soft fibres run along it (1D gradient
// noise across the trail, broken along it). Where a pixel covers several fibres the fibres
// fade to their mean, and where it covers a trail the field fades to `CIRRUS_TRAIL_MEAN`, so
// nothing aliases into hair near the horizon.
fn cirrus_density(x: f32, y: f32, band: f32, stretch: f32, footprint: f32) -> f32 {
    let b = clamp(band, 0.0, 1.0);
    if (b <= 0.0) {
        return 0.0;
    }
    let far = b * (CIRRUS_VEIL + CIRRUS_TRAIL_MEAN);
    let blur = smoothstep(0.2, 0.5, footprint);
    if (blur >= 1.0) {
        return min(far, 1.0);
    }
    let cx = CIRRUS_CELL_ALONG * clamp(stretch, 1.0, 32.0);
    var total = 0.0;
    var best = 0.0;
    var best_n = 0.0;
    var best_u = 0.0;
    var best_seed = 0.0;
    let iy = i32(floor(y));
    for (var dj = -1; dj <= 1; dj++) {
        let j = iy + dj;
        // Rows staggered by the golden ratio so heads never line up across the wind.
        let xr = x + fract(f32(j) * 0.618034) * cx;
        let ix = i32(floor(xr / cx));
        for (var k = 0; k < 2; k++) {
            let c = ix - k;
            let h = cirrus_pcg2d(vec2<i32>(c, j));
            if (cirrus_byte(h.x, 0u) >= CIRRUS_CELL_ACTIVE) {
                continue;
            }
            let w0 = 0.12 + 0.28 * cirrus_byte(h.y, 0u);
            let hx = (f32(c) + 0.1 + 0.8 * cirrus_byte(h.x, 1u)) * cx;
            let hy = f32(j) + 0.25 + 0.5 * cirrus_byte(h.x, 2u);
            let len = cx * (0.4 + 0.55 * cirrus_byte(h.x, 3u));
            let drift = (cirrus_byte(h.y, 1u) - 0.5) * 0.5;
            let hook = (cirrus_byte(h.y, 2u) - 0.5) * 1.2;
            let seed = cirrus_byte(h.y, 3u);
            let s = xr - hx;
            let u_raw = s / len;
            let front = min(0.8 * w0, 0.1 * cx);
            if (s <= -front || u_raw >= 1.0) {
                continue;
            }
            let us = clamp(u_raw, 0.0, 1.0);
            let head = smoothstep(-front, front, s);
            let tail = (1.0 - smoothstep(0.0, 1.0, u_raw)) * (1.0 - 0.5 * us);
            let n = y - hy - drift * us - hook * w0 * us * us;
            let w = w0 * (0.6 + 1.2 * us);
            // The profile convolved with the footprint keeps its area: wider and fainter.
            let fp = CIRRUS_TRAIL_FILTER * footprint;
            // Capped so a trail never reaches past the neighbouring rows the loop visits.
            let we = min(sqrt(w * w + fp * fp), max(w, 0.72));
            let a = n / we;
            var across = clamp(1.0 - a * a, 0.0, 1.0);
            across = across * across;
            let d = (0.45 + 0.55 * seed) * CIRRUS_TRAIL_GAIN * head * tail * across * (w / we);
            total += d;
            if (d > best) {
                best = d;
                best_n = n;
                best_u = us;
                best_seed = seed;
            }
        }
    }
    var fibre = 1.0;
    // Where two trails overlap evenly the fibres fade, so the switch of frame is unseen.
    let dom = clamp(2.0 * best / max(total, 1e-6) - 1.0, 0.0, 1.0);
    // Each fibre octave fades before a texel covers a third of one of its bright fibres.
    let cells = footprint * CIRRUS_FIBRE_CELLS;
    let pre = 1.0 - smoothstep(0.12, 0.3, cells);
    if (best > 0.0 && dom * pre > 0.0) {
        let fine = 1.0 - smoothstep(0.05, 0.13, cells);
        let fc = best_n * CIRRUS_FIBRE_CELLS + best_seed * 37.0 + best_u * 0.8;
        let f = 0.65 * cirrus_noise1(fc) + 0.35 * fine * cirrus_noise1(fc * 2.3 + 11.0);
        let fib = smoothstep(-0.4, 0.4, f);
        let brk = smoothstep(-0.3, 0.25, cirrus_noise1(best_u * 3.0 + best_seed * 53.0));
        let contrast = CIRRUS_FIBRE_CONTRAST * dom * pre;
        fibre = mix(1.0, fib * (0.5 + 0.5 * brk) * CIRRUS_FIBRE_GAIN, contrast);
    }
    let near = b * (CIRRUS_VEIL + min(total * fibre, 1.0));
    return clamp(mix(near, far, blur), 0.0, 1.0);
}

// Fibrous cirrus (ADR-1509). Every constant matches the `CIRRUS_*` one of the same name in
// `cloud_types.rs`, and `cirrus_density_fibrous` matches `cirrus_density_fibrous` there.
// Fall depth of the trails' tails below the sheet at full shear (m; Heymsfield 1975).
const CIRRUS_FALL_DEPTH_M: f32 = 2500.0;
// Largest apparent slant of a trail on the sheet (tiles per unit of fall fraction).
const CIRRUS_SLANT_MAX: f32 = 3.0;
// Share of the sheet's broad curl warp a fibrous sheet keeps.
const CIRRUS_FIBROUS_WARP: f32 = 0.4;
// A fibrous trail's cross profile is widened to at least this many footprints.
const CIRRUS_FIBROUS_FILTER: f32 = 2.0;
// Extra density of an uncinus head tuft at full hook.
const CIRRUS_HOOK_TUFT: f32 = 1.2;
// Strand sharpness: `1 − |noise| · this`, squared.
const CIRRUS_STRAND_RIDGE: f32 = 3.2;
// Density left between the strands.
const CIRRUS_STRAND_BASE: f32 = 0.15;
// Area mean of the strand modulation before normalising.
const CIRRUS_STRAND_MEAN: f32 = 0.415;
// Breaks per trail length along each strand.
const CIRRUS_STRAND_BREAK: f32 = 4.0;
// Peak share of the faint fiber patches between the trails.
const CIRRUS_SHEET_FIBERS: f32 = 0.12;
// Area mean of the envelope of those patches.
const CIRRUS_FIBER_PATCH_MEAN: f32 = 0.18;
// Length over width of a sheet fiber.
const CIRRUS_FIBER_ANISO: f32 = 18.0;
// Area mean of one sheet fiber octave.
const CIRRUS_FIBER_OCTAVE_MEAN: f32 = 0.157;

// Crisp cirrus (ADR-1516). Every constant matches the `CIRRUS_*` one of the same name in
// `cloud_types.rs`. A crisp trail's cross profile is widened to at least this many footprints
// (the sheet is supersampled, so one sub-sample footprint is enough).
const CIRRUS_CRISP_FILTER: f32 = 1.0;
// Sharpest strand ridge at full crisp (`1 − |noise| · this`, squared).
const CIRRUS_CRISP_RIDGE: f32 = 7.0;
// The ridge is never sharper than this over the footprint in strand spacings.
const CIRRUS_CRISP_RIDGE_WIDTH: f32 = 0.45;
// Density left between the strands at full crisp.
const CIRRUS_CRISP_BASE: f32 = 0.04;
// Area mean of the strand modulation for ridge sharpness k: base + A / (k + B) (fitted).
const CIRRUS_CRISP_MEAN_A: f32 = 1.48;
const CIRRUS_CRISP_MEAN_B: f32 = 2.3;
// Share of the veil between the trails kept at full crisp.
const CIRRUS_CRISP_VEIL: f32 = 0.4;
// Largest normalised density at full crisp (1 otherwise): strand peaks are not clipped.
const CIRRUS_CRISP_CAP: f32 = 3.0;

// One axis of a cubic B-spline texture lookup as two linear taps (Sigg & Hadwiger, "Fast
// Third-Order Texture Filtering", GPU Gems 2 ch. 20, 2005): the two tap coordinates (uv) and
// the weight of the first. The B-spline is C2, so a field read through it bends smoothly where
// a bilinear read has a kink at every texel edge (ADR-1516). Matches `cirrus_bspline_axis`.
fn cirrus_bspline_axis(uv: f32, size: f32) -> vec3<f32> {
    let x = uv * size - 0.5;
    let i = floor(x);
    let f = x - i;
    let f2 = f * f;
    let f3 = f2 * f;
    let w0 = (1.0 - 3.0 * f + 3.0 * f2 - f3) / 6.0;
    let w1 = (4.0 - 6.0 * f2 + 3.0 * f3) / 6.0;
    let w2 = (1.0 + 3.0 * f + 3.0 * f2 - 3.0 * f3) / 6.0;
    let w3 = f3 / 6.0;
    let g0 = w0 + w1;
    let h0 = (i - 0.5 + w1 / g0) / size;
    let h1 = (i + 1.5 + w3 / (w2 + w3)) / size;
    return vec3<f32>(h0, h1, g0);
}

// The curl texture at `uv` through the cubic B-spline (four bilinear taps).
fn cirrus_curl_smooth(uv: vec2<f32>) -> vec2<f32> {
    let size = vec2<f32>(textureDimensions(curl_tex));
    let ax = cirrus_bspline_axis(uv.x, size.x);
    let ay = cirrus_bspline_axis(uv.y, size.y);
    let c00 = textureSampleLevel(curl_tex, linear_samp, vec2<f32>(ax.x, ay.x), 0.0).rg;
    let c10 = textureSampleLevel(curl_tex, linear_samp, vec2<f32>(ax.y, ay.x), 0.0).rg;
    let c01 = textureSampleLevel(curl_tex, linear_samp, vec2<f32>(ax.x, ay.y), 0.0).rg;
    let c11 = textureSampleLevel(curl_tex, linear_samp, vec2<f32>(ax.y, ay.y), 0.0).rg;
    return mix(mix(c10, c00, ax.z), mix(c11, c01, ax.z), 1.0 - ay.z);
}

// Channel r of the shape texture at (`xz.x`, `slice`, `xz.y`) through the cubic B-spline in x
// and z (four trilinear taps; the slice stays linear).
fn cirrus_shape_smooth(xz: vec2<f32>, slice: f32) -> f32 {
    let size = vec3<f32>(textureDimensions(shape_tex));
    let ax = cirrus_bspline_axis(xz.x, size.x);
    let az = cirrus_bspline_axis(xz.y, size.z);
    let s00 = textureSampleLevel(shape_tex, linear_samp, vec3<f32>(ax.x, slice, az.x), 0.0).r;
    let s10 = textureSampleLevel(shape_tex, linear_samp, vec3<f32>(ax.y, slice, az.x), 0.0).r;
    let s01 = textureSampleLevel(shape_tex, linear_samp, vec3<f32>(ax.x, slice, az.y), 0.0).r;
    let s11 = textureSampleLevel(shape_tex, linear_samp, vec3<f32>(ax.y, slice, az.y), 0.0).r;
    return mix(mix(s10, s00, ax.z), mix(s11, s01, ax.z), 1.0 - az.z);
}

// 2D gradient noise (Perlin 2002 quintic fade) on the pcg2d hash, about −0.7..0.7.
fn cirrus_noise2(x: f32, y: f32) -> f32 {
    let fx0 = floor(x);
    let fy0 = floor(y);
    let fx = x - fx0;
    let fy = y - fy0;
    let ix = i32(fx0);
    let iy = i32(fy0);
    let h00 = cirrus_pcg2d(vec2<i32>(ix, iy)).x;
    let h10 = cirrus_pcg2d(vec2<i32>(ix + 1, iy)).x;
    let h01 = cirrus_pcg2d(vec2<i32>(ix, iy + 1)).x;
    let h11 = cirrus_pcg2d(vec2<i32>(ix + 1, iy + 1)).x;
    let c00 = (cirrus_byte(h00, 0u) * 2.0 - 1.0) * fx + (cirrus_byte(h00, 1u) * 2.0 - 1.0) * fy;
    let c10 = (cirrus_byte(h10, 0u) * 2.0 - 1.0) * (fx - 1.0) + (cirrus_byte(h10, 1u) * 2.0 - 1.0) * fy;
    let c01 = (cirrus_byte(h01, 0u) * 2.0 - 1.0) * fx + (cirrus_byte(h01, 1u) * 2.0 - 1.0) * (fy - 1.0);
    let c11 = (cirrus_byte(h11, 0u) * 2.0 - 1.0) * (fx - 1.0) + (cirrus_byte(h11, 1u) * 2.0 - 1.0) * (fy - 1.0);
    let wx = fx * fx * fx * (fx * (fx * 6.0 - 15.0) + 10.0);
    let wy = fy * fy * fy * (fy * (fy * 6.0 - 15.0) + 10.0);
    let a = c00 + (c10 - c00) * wx;
    let b = c01 + (c11 - c01) * wx;
    return a + (b - a) * wy;
}

// One octave of the sheet fibers: thin ridges where an anisotropic noise crosses zero, broken
// along their length.
fn cirrus_fiber_octave(a: f32, b: f32, octave: f32, freq: f32) -> f32 {
    let g = cirrus_noise2(a * freq / CIRRUS_FIBER_ANISO + 17.3 * octave, b * freq + 5.1 * octave);
    let ridge = 1.0 - min(abs(g) * 2.5, 1.0);
    let gate = smoothstep(-0.12, 0.22, cirrus_noise2(
        a * freq / (CIRRUS_FIBER_ANISO * 2.5) + 3.7 * octave + 40.0,
        b * freq * 0.3 + 9.2 * octave,
    ));
    return ridge * ridge * ridge * gate;
}

// One sheet fiber octave faded to its mean before a footprint covers a third of it.
fn cirrus_fiber_term(a: f32, b: f32, footprint: f32, octave: f32, freq: f32, amp: f32) -> f32 {
    let m = CIRRUS_FIBER_OCTAVE_MEAN;
    let keep = 1.0 - smoothstep(0.12, 0.35, footprint * freq);
    var v = m;
    if (keep > 0.0) {
        v = cirrus_fiber_octave(a, b, octave, freq);
    }
    return amp * (m + (v - m) * keep);
}

// The sheet fibers, three octaves, area mean 1 (`footprint` in fiber units).
fn cirrus_sheet_fibers(a: f32, b: f32, footprint: f32) -> f32 {
    let total = cirrus_fiber_term(a, b, footprint, 0.0, 1.0, 1.0)
        + cirrus_fiber_term(a, b, footprint, 1.0, 2.03, 0.8)
        + cirrus_fiber_term(a, b, footprint, 2.0, 4.1, 0.6);
    return total / ((1.0 + 0.8 + 0.6) * CIRRUS_FIBER_OCTAVE_MEAN);
}

// One fibrous trail's density at `p` (shear frame, staggered) from the generating cell `cell`,
// with the apparent slant `slant` (tiles per unit of fall fraction). `fib` is the
// `cirrus_fiber` row (fiber, fiber scale, shear, hook); `fs` the fiber scale and `cx` the cell
// length, in tiles; `crisp` the crisp amount (ADR-1516, 0: the ADR-1509 trail).
fn cirrus_fibrous_trail(cell: vec2<i32>, p: vec2<f32>, slant: vec2<f32>, h0: f32, footprint: f32, fs: f32, cx: f32, fib: vec4<f32>, crisp: f32) -> f32 {
    let hb = cirrus_pcg2d(cell);
    if (cirrus_byte(hb.x, 0u) >= CIRRUS_CELL_ACTIVE) {
        return 0.0;
    }
    let fiber = fib.x;
    let shear = fib.z;
    let hook = fib.w;
    let pvs = slant.x;
    let pvn = slant.y;
    let w0 = 0.12 + 0.28 * cirrus_byte(hb.y, 0u);
    let hs = (f32(cell.x) + 0.1 + 0.8 * cirrus_byte(hb.x, 1u)) * cx;
    let hn = f32(cell.y) + 0.25 + 0.5 * cirrus_byte(hb.x, 2u);
    let len = cx * (0.4 + 0.55 * cirrus_byte(hb.x, 3u)) * (1.0 + shear);
    let drift = (cirrus_byte(hb.y, 1u) - 0.5) * 0.5;
    let bend = (cirrus_byte(hb.y, 2u) - 0.5) * 1.2 * (1.0 - hook) * w0;
    let veer = hook * (0.35 + 0.35 * cirrus_byte(hb.y, 2u));
    let seed = cirrus_byte(hb.y, 3u);
    // The chord from the head to the tail's end.
    let a_s = len + pvs;
    let a_n = pvn + drift + veer + bend;
    let a2 = max(a_s * a_s + a_n * a_n, 1e-6);
    let alen = sqrt(a2);
    let rs = p.x - hs;
    let rn = p.y - hn;
    let along = (rs * a_s + rn * a_n) / alen;
    let t0 = along / alen;
    let front = min(0.8 * w0, 0.1 * cx);
    if (along <= -front || t0 >= 1.0) {
        return 0.0;
    }
    // The fall fraction whose point projects onto the chord where `p` does (two Newton steps).
    var uf = clamp(t0, 0.0, 1.0);
    for (var it = 0; it < 2; it++) {
        let os = len * (h0 * uf + (1.0 - h0) * uf * uf) + pvs * uf;
        let on = (pvn + drift) * uf + veer * uf * uf * uf + bend * uf * uf;
        let ds = len * (h0 + 2.0 * (1.0 - h0) * uf) + pvs;
        let dn = pvn + drift + 3.0 * veer * uf * uf + 2.0 * bend * uf;
        let cu = (os * a_s + on * a_n) / a2;
        let dc = max((ds * a_s + dn * a_n) / a2, 0.05);
        uf = clamp(uf - (cu - t0) / dc, 0.0, 1.0);
    }
    let os = len * (h0 * uf + (1.0 - h0) * uf * uf) + pvs * uf;
    let on = (pvn + drift) * uf + veer * uf * uf * uf + bend * uf * uf;
    let dist = ((rs - os) * (-a_n) + (rn - on) * a_s) / alen;
    let head = smoothstep(-front, front, along);
    let tail = (1.0 - smoothstep(0.0, 1.0, t0)) * (1.0 - 0.5 * uf);
    let near_head = exp(-(uf / 0.1) * (uf / 0.1));
    let tuft = 1.0 + hook * CIRRUS_HOOK_TUFT * near_head;
    let w = w0 * (0.6 + (1.2 + 0.8 * shear) * uf) * (1.0 - 0.35 * hook * near_head);
    // The profile convolved with the footprint keeps its area: wider and fainter.
    var fp = CIRRUS_FIBROUS_FILTER * footprint;
    if (crisp > 0.0) {
        fp = mix(CIRRUS_FIBROUS_FILTER, CIRRUS_CRISP_FILTER, crisp) * footprint;
    }
    let we = min(sqrt(w * w + fp * fp), max(w, 0.72));
    let a = dist / we;
    let across = clamp(1.0 - a * a, 0.0, 1.0);
    // Strands combed along the trail, spreading as it widens.
    let q = dist / w * (w0 / fs);
    let spacing = fs * w / w0;
    let pre = 1.0 - smoothstep(0.15, 0.4, footprint / spacing);
    var strands = 1.0;
    if (fiber * pre > 0.0) {
        let fine = 1.0 - smoothstep(0.15, 0.4, 2.3 * footprint / spacing);
        let f = 0.65 * cirrus_noise1(q + seed * 37.0 + uf * 0.7)
            + 0.35 * fine * cirrus_noise1(q * 2.3 + 11.0 + seed * 13.0);
        var k = CIRRUS_STRAND_RIDGE;
        var base = CIRRUS_STRAND_BASE;
        var mean = CIRRUS_STRAND_MEAN;
        if (crisp > 0.0) {
            // Crisp (ADR-1516): the ridge as sharp as the footprint allows, up to
            // `CIRRUS_CRISP_RIDGE`, over less ice between the strands; the mean follows.
            let kmax = mix(CIRRUS_STRAND_RIDGE, CIRRUS_CRISP_RIDGE, crisp);
            k = clamp(CIRRUS_CRISP_RIDGE_WIDTH * spacing / max(footprint, 1e-9), CIRRUS_STRAND_RIDGE, kmax);
            base = mix(CIRRUS_STRAND_BASE, CIRRUS_CRISP_BASE, crisp);
            mean = base + CIRRUS_CRISP_MEAN_A / (k + CIRRUS_CRISP_MEAN_B);
        }
        let r = clamp(1.0 - abs(f) * k, 0.0, 1.0);
        // Breaks along the strands, different from strand to strand.
        let brk = smoothstep(-0.25, 0.3, cirrus_noise2(uf * CIRRUS_STRAND_BREAK + seed * 53.0, q));
        let v = (base + r * r * brk) / mean;
        strands = 1.0 + (v - 1.0) * fiber * pre;
    }
    return (0.45 + 0.55 * seed) * CIRRUS_TRAIL_GAIN * head * tail * tuft * across * across * (w / we) * strands;
}

// Normalised ice density (0..1) of a fibrous cirrus sheet (ADR-1509) at `x` streak tiles along
// the wind and `y` across it, in a band of strength `band`, for a footprint of `footprint`
// tiles across the shear; `slant` is the view's horizontal direction over its vertical part in
// the wind frame (along, across). `fib` = (fiber, fiber scale m, shear, hook), `shr` = (cos,
// sin of the shear direction, breakup, on), `tile` the streak tile (m), `crisp` the crisp
// amount (ADR-1516; above 0 the density may reach `1 + 2 · crisp`).
//
// Fallstreaks (Marshall 1953; Heymsfield 1975): with a wind that grows linearly with depth a
// falling crystal's offset from its head grows as the square of the depth, a veering wind adds
// a cubic sideways bend, and the tail lies below the sheet, so seen at a slant it is displaced
// on the sheet by `slant` times its depth: the uncinus comma. Strands combed along each trail
// spread as it widens; faint fiber patches lie along the shear between the trails; a patch noise
// breaks the band into separate patches. See `cirrus_density_fibrous` in `cloud_types.rs`.
fn cirrus_density_fibrous(x: f32, y: f32, band: f32, footprint: f32, slant: vec2<f32>, stretch: f32, tile: f32, fib: vec4<f32>, shr: vec4<f32>, crisp: f32) -> f32 {
    let b0 = clamp(band, 0.0, 1.0);
    if (b0 <= 0.0) {
        return 0.0;
    }
    let fiber = fib.x;
    let shear = fib.z;
    let hook = fib.w;
    let breakup = shr.z;
    let cs = shr.x;
    let sn = shr.y;
    let s = x * cs + y * sn;
    let n = -x * sn + y * cs;
    let ps = slant.x * cs + slant.y * sn;
    let pn = -slant.x * sn + slant.y * cs;
    let cx = CIRRUS_CELL_ALONG * clamp(stretch, 1.0, 32.0);
    // Patches: the band broken by a noise stretched along the shear.
    let pz = cirrus_noise2(s / (cx * 0.45) + 71.0, n / 0.7 + 13.0)
        + 0.5 * cirrus_noise2(s / (cx * 0.2) + 5.0, n / 0.33 + 29.0);
    let thr = breakup * 0.3 - 0.3;
    let b = b0 * (1.0 + (smoothstep(thr, thr + 0.25, pz) - 1.0) * breakup);
    if (b <= 0.0) {
        return 0.0;
    }
    // Crisp (ADR-1516): less veil between the trails, and strand peaks up to `cap`.
    var thin = 1.0;
    var cap = 1.0;
    if (crisp > 0.0) {
        thin = mix(1.0, CIRRUS_CRISP_VEIL, crisp);
        cap = mix(1.0, CIRRUS_CRISP_CAP, crisp);
    }
    let veil_far = (CIRRUS_VEIL * (1.0 - fiber) + fiber * CIRRUS_SHEET_FIBERS * CIRRUS_FIBER_PATCH_MEAN) * thin;
    let trail_far = CIRRUS_TRAIL_MEAN * (1.0 + 1.3 * shear) * (1.0 + 0.1 * hook) * (1.0 - 0.2 * fiber);
    let far = b * (veil_far + trail_far);
    let blur = smoothstep(0.2, 0.5, footprint);
    if (blur >= 1.0) {
        return min(far, cap);
    }
    let fs = fib.y / tile;
    // The tail's apparent slant on the sheet per unit of fall fraction, clamped.
    let fall = shear * CIRRUS_FALL_DEPTH_M / tile;
    var pv = vec2<f32>(ps, pn) * fall;
    let mag = length(pv);
    if (mag > CIRRUS_SLANT_MAX) {
        pv = pv * (CIRRUS_SLANT_MAX / mag);
    }
    let h0 = 1.0 - 0.75 * hook;
    let lmax = cx * 0.95 * (1.0 + shear);
    // Rows and cells whose trail can reach this point (see `cloud_types.rs`).
    let j0 = i32(floor(n - max(pv.y, 0.0) - 3.0));
    let j1 = min(i32(floor(n - min(pv.y, 0.0) + 1.3)), j0 + 9);
    var total = 0.0;
    for (var j = j0; j <= j1; j++) {
        // Rows staggered by the golden ratio so heads never line up across the shear.
        let sr = s + fract(f32(j) * 0.618034) * cx;
        let c0 = i32(floor((sr - lmax - max(pv.x, 0.0) - 1.05) / cx - 0.9));
        let c1 = min(i32(floor((sr - min(pv.x, 0.0) + 1.05) / cx)), c0 + 8);
        for (var c = c0; c <= c1; c++) {
            total += cirrus_fibrous_trail(vec2<i32>(c, j), vec2<f32>(sr, n), pv, h0, footprint, fs, cx, fib, crisp);
        }
    }
    var veil = CIRRUS_VEIL * (1.0 - fiber);
    if (fiber > 0.0) {
        let env = smoothstep(0.0, 0.35, cirrus_noise2(s / (cx * 0.3) + 21.0, n / 0.35 + 3.0));
        veil += fiber * CIRRUS_SHEET_FIBERS * env * cirrus_sheet_fibers(s / fs, n / fs, footprint / fs);
    }
    let near = b * (veil * thin + min(total, cap));
    return clamp(mix(near, far, blur), 0.0, cap);
}

// The high cirrus sheet along `dir` (ADR-1408, ADR-1425, ADR-1470): radiance (rgb) and the
// multiplier of the sky behind it (a), to composite behind the volumetric layer. `texel_angle`
// is the angle one march texel spans (radians), for the footprint prefilter.
//
// Shape: fallstreak trails (`cirrus_density`) on a sheet whose coordinates are bent by a broad
// curl warp (the shear turns with position, so neighbouring trail groups are not parallel), in
// bands from a broad and a middle octave of the shape noise covering about `coverage` of the sky.
//
// Light: single scattering (optical depth ≤ 0.3, Sassen & Cho 1992) of the sun at the sheet's
// height through the ice phase function, plus the sky light above the sheet and the light
// coming up from the air and ground below it. The sheet hides only the air *behind* it,
// `exp(−h / H)` of the sky's in-scatter; the air in front still reaches the eye. So a streak
// adds `T_air · (1 − T) · L` and removes `(1 − T) · exp(−h / H) · sky`, and cirrus is brighter
// than the sky around it, strongly near the sun and slightly away from it.
fn cirrus_sheet(dir: vec3<f32>, texel_angle: f32) -> vec4<f32> {
    let none = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    let cov = u.cirrus.x;
    if (cov <= 0.0) {
        return none;
    }
    let r = u.shape.w;
    let eye = u.camera.xyz;
    if (altitude(eye) >= u.cirrus.y) {
        return none;
    }
    let t = shell(eye, dir, r + u.cirrus.y).y;
    if (t <= 0.0) {
        return none;
    }
    let p = eye + dir * t;
    let rel = p.xz - u.cirrus_dir.zw;
    let along = u.cirrus_dir.xy;
    let across = vec2<f32>(-along.y, along.x);
    let tile = max(u.cirrus.w, 1.0);
    let stretch = max(u.cirrus_sun.w, 1.0);
    let x = dot(rel, along) / tile;
    let y = dot(rel, across) / tile;
    let q = vec2<f32>(x / stretch, y);
    // Bands: the broad octave of the Perlin–Worley channel covers about `coverage` of the sky; a
    // middle octave thins them in places, so the trails come in groups of varying density.
    // A crisp sheet (ADR-1516) reads the band and warp textures through a cubic B-spline: a
    // bilinear read is piecewise linear, so the warped trails ran as polygons with a kink at
    // every curl texel edge (the saw-tooth) and the band edges as polygons too. The smooth
    // reads cross-fade from the bilinear reads by the crisp amount, so a small crisp changes
    // the sheet continuously (0 keeps the bilinear path bit-identical and its cost); only the
    // supersampling below steps on above 0.
    let crisp_amt = u.cirrus_crisp.x;
    var broad = 0.0;
    if (crisp_amt > 0.0) {
        broad = mix(
            textureSampleLevel(shape_tex, linear_samp, vec3<f32>(q.x * 0.23, 0.83, q.y * 0.23), 0.0).r,
            cirrus_shape_smooth(vec2<f32>(q.x * 0.23, q.y * 0.23), 0.83),
            crisp_amt);
    } else {
        broad = textureSampleLevel(shape_tex, linear_samp, vec3<f32>(q.x * 0.23, 0.83, q.y * 0.23), 0.0).r;
    }
    var band = smoothstep(1.0 - cov, min(1.0 - cov + 0.3, 1.0), broad);
    if (band <= 0.0) {
        return none;
    }
    var middle = 0.0;
    if (crisp_amt > 0.0) {
        middle = mix(
            textureSampleLevel(shape_tex, linear_samp, vec3<f32>(q.x * 0.6, 0.37, q.y * 0.45), 0.0).r,
            cirrus_shape_smooth(vec2<f32>(q.x * 0.6, q.y * 0.45), 0.37),
            crisp_amt);
    } else {
        middle = textureSampleLevel(shape_tex, linear_samp, vec3<f32>(q.x * 0.6, 0.37, q.y * 0.45), 0.0).r;
    }
    band *= 0.35 + 0.65 * smoothstep(0.3, 0.7, middle);
    // The shear turns slowly with position: a broad curl warp bends the trail groups (about ±20°).
    var curl_raw = vec2<f32>(0.0);
    if (crisp_amt > 0.0) {
        curl_raw = mix(
            textureSampleLevel(curl_tex, linear_samp, vec2<f32>(q.x * 0.12, q.y * 0.025), 0.0).rg,
            cirrus_curl_smooth(vec2<f32>(q.x * 0.12, q.y * 0.025)),
            crisp_amt);
    } else {
        curl_raw = textureSampleLevel(curl_tex, linear_samp, vec2<f32>(q.x * 0.12, q.y * 0.025), 0.0).rg;
    }
    let curl = curl_raw * 2.0 - vec2<f32>(1.0);
    // A fibrous sheet keeps `CIRRUS_FIBROUS_WARP` of the bend, so its streaks run along the shear
    // instead of curling into S-strokes (ADR-1509); 2 × 1 and 1.4 × 1 are exact, so the ADR-1470
    // path is unchanged.
    let warp = select(1.0, CIRRUS_FIBROUS_WARP, u.cirrus_shear.w > 0.5);
    let xw = x + (2.0 * warp) * curl.x;
    let yw = y + (1.4 * warp) * curl.y;
    let up = normalize(p - vec3<f32>(0.0, -r, 0.0));
    let mu = max(abs(dot(dir, up)), 0.03);
    // The texel's footprint on the sheet across the trails: stretched by 1 / mu along the view's
    // horizontal direction, not across it.
    let flat_dir = dir - up * dot(dir, up);
    let across3 = vec3<f32>(across.x, 0.0, across.y);
    let c1 = dot(across3, flat_dir) / max(length(flat_dir), 1e-4);
    let footprint = t * texel_angle * sqrt(c1 * c1 / (mu * mu) + max(1.0 - c1 * c1, 0.0)) / tile;
    var d = 0.0;
    if (u.cirrus_shear.w > 0.5) {
        // Fibrous (ADR-1509): the footprint across the shear, and the view's slant on the sheet
        // (its horizontal direction over its vertical part, in the wind frame).
        let shr = u.cirrus_shear;
        let across_s = across3 * shr.x - vec3<f32>(along.x, 0.0, along.y) * shr.y;
        let c2 = dot(across_s, flat_dir) / max(length(flat_dir), 1e-4);
        let footprint_s = t * texel_angle * sqrt(c2 * c2 / (mu * mu) + max(1.0 - c2 * c2, 0.0)) / tile;
        let slant = vec2<f32>(dot(flat_dir, vec3<f32>(along.x, 0.0, along.y)), dot(flat_dir, across3)) / mu;
        d = cirrus_density_fibrous(xw, yw, band, footprint_s, slant, stretch, tile, u.cirrus_fiber, shr, u.cirrus_crisp.x);
    } else {
        d = cirrus_density(xw, yw, band, stretch, footprint);
    }
    let trans = exp(-u.cirrus.z * d / mu);
    let cos_t = dot(dir, u.sun.xyz);
    // Sky light: from above, the air beyond the sheet; from below, the air and ground under it
    // (`ambient_bottom`: half sky, half ground bounce). Half the sphere each.
    let behind = exp(-u.cirrus.y / CIRRUS_AIR_SCALE_HEIGHT_M);
    let ambient = 0.5 * (behind * u.ambient_top.rgb + u.ambient_bottom.rgb);
    let lit = (1.0 - trans) * (u.cirrus_sun.rgb * cirrus_phase(cos_t) + ambient);
    // The sky behind the sheet: `bg · a` with `a = 1 − (1 − T) · exp(−h / H)`.
    let hidden = (1.0 - trans) * behind;
    let fade = max(u.ambient_top.w, 1.0);
    if (u.sun_bottom.w > 0.5) {
        let air = aerial_at(dir, t);
        let keep = exp(-max(t - AERIAL_RANGE_M, 0.0) / fade);
        return vec4<f32>(air.a * lit * keep, 1.0 - hidden * keep);
    }
    let keep = exp(-t / fade);
    return vec4<f32>(lit * keep, 1.0 - hidden * keep);
}

// View ray from the unjittered inverse, so TAA jitter does not swim the march.
fn view_ray(uv: vec2<f32>) -> vec3<f32> {
    let ndc = vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    let far_h = u.ray_from_clip * vec4<f32>(ndc, 1.0, 1.0);
    let near_h = u.ray_from_clip * vec4<f32>(ndc, 0.0, 1.0);
    return normalize(far_h.xyz / far_h.w - near_h.xyz / near_h.w);
}

// World distance from the eye to the surface at a full-resolution pixel. Sky is `SKY_M`.
// Depth was written with the jittered projection, so this uses `inv_view_proj`.
fn surface_distance(pix: vec2<i32>, eye: vec3<f32>) -> f32 {
    let dims_u = textureDimensions(scene_depth);
    let dims = vec2<i32>(dims_u);
    let p = clamp(pix, vec2<i32>(0), dims - vec2<i32>(1));
    let depth = textureLoad(scene_depth, p, 0);
    if (depth >= 1.0) {
        return SKY_M;
    }
    let dims_f = max(vec2<f32>(dims_u), vec2<f32>(1.0));
    let uv = (vec2<f32>(p) + vec2<f32>(0.5)) / dims_f;
    let ndc = vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    let h = u.inv_view_proj * vec4<f32>(ndc, depth, 1.0);
    return min(length(h.xyz / h.w - eye), SKY_M);
}

// Farthest surface in this low-resolution texel's footprint. Any sky texel opens the
// march to the sky sentinel, so a cloud in front of the far surface is still integrated.
// The depth pyramid is max-only and is not built for these frames, so the footprint is
// read here (at most 4×4, strided when the divisor is larger).
fn footprint_far(low_pix: vec2<i32>, div: i32, eye: vec3<f32>) -> f32 {
    let n = min(max(div, 1), 4);
    let stride = max(div / max(n, 1), 1);
    let origin = low_pix * max(div, 1);
    var far_t = 0.0;
    var sky = false;
    for (var y = 0; y < 4; y++) {
        if (y >= n) {
            break;
        }
        for (var x = 0; x < 4; x++) {
            if (x >= n) {
                break;
            }
            let d = surface_distance(origin + vec2<i32>(x, y) * stride, eye);
            sky = sky || d >= SKY_M;
            far_t = max(far_t, d);
        }
    }
    if (sky) {
        return SKY_M;
    }
    return far_t;
}

struct MarchOut {
    @location(0) color: vec4<f32>,
    @location(1) dist: vec4<f32>,
}

// Scene distance at the centre of a low-resolution texel's footprint.
fn footprint_scene(low_pix: vec2<i32>) -> f32 {
    let div = max(i32(u.hdr.y), 1);
    let center = low_pix * div + vec2<i32>(div / 2);
    return surface_distance(center, u.camera.xyz);
}

// Most marches per texel. Matches `CloudMarch::MAX_SAMPLES`.
const MAX_MARCH_SAMPLES: i32 = 16;

// Point `k` of the R2 sequence (Roberts 2018) started at the texel centre, in texel units.
// Matches `CloudMarch::sub_texel`.
fn r2_offset(k: i32) -> vec2<f32> {
    return fract(vec2<f32>(0.5) + f32(k) * vec2<f32>(0.7548777, 0.5698403));
}

// Marches one low-resolution texel. Colour is radiance (rgb) and transmittance (a). The
// second target stores the scatter distance (R) and the footprint-centre scene distance (G).
// With `hdr.w` > 1 (ADR-1403) it averages that many marches: march k starts in jitter stratum
// `(ign + k) / n` and casts its ray through R2 point k of the texel. The colour averages in
// linear light (the composite `bg · a + rgb` is linear in both); the scatter distance is the
// opacity-weighted mean, so a march that misses the cloud does not pull it toward zero.
fn march_texel(low_pix: vec2<i32>) -> MarchOut {
    let dims = max(u.targets.xy, vec2<f32>(1.0));
    let jitter = ign(vec2<f32>(low_pix) + vec2<f32>(0.5), u.camera.w);
    let div = max(i32(u.hdr.y), 1);
    let far_t = footprint_far(low_pix, div, u.camera.xyz);
    let scene_d = footprint_scene(low_pix);
    let n = clamp(i32(u.hdr.w), 1, MAX_MARCH_SAMPLES);
    // One call site, so the pipeline does not inline the march twice. March 0 is the centred
    // march with the texel's own jitter (R2 point 0 is the centre, stratum 0 of 1 is `jitter`),
    // and with one march its result is returned as it is.
    var color_sum = vec4<f32>(0.0);
    var first_d = 0.0;
    var dist_sum = 0.0;
    var dist_weight = 0.0;
    // The cirrus sheet once per texel, along the centre ray (march 0's), prefiltered over the
    // angle one texel spans (ADR-1470); every march composites the same sheet.
    // A crisp sheet (ADR-1516, `cirrus_crisp.y` > 1) is the mean of that many sheet samples
    // through the texel's R2 points, each prefiltered over its share of the texel (the angle over
    // the square root of the count): a box filter over the texel whose taps resolve strands about
    // one sample wide. One call site, as for the march.
    let sheet_on = u.cirrus.x > 0.0 && far_t >= SKY_M;
    var cs = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    if (sheet_on) {
        let centre = vec2<f32>(low_pix) + vec2<f32>(0.5);
        let centre_dir = view_ray(centre / dims);
        let texel_angle = length(view_ray((centre + vec2<f32>(1.0, 0.0)) / dims) - centre_dir);
        let ns = clamp(i32(u.cirrus_crisp.y), 1, MAX_MARCH_SAMPLES);
        var acc = vec4<f32>(0.0);
        for (var k = 0; k < MAX_MARCH_SAMPLES; k++) {
            if (k >= ns) {
                break;
            }
            var dir = centre_dir;
            var angle = texel_angle;
            if (ns > 1) {
                dir = view_ray((vec2<f32>(low_pix) + r2_offset(k)) / dims);
                angle = texel_angle / sqrt(f32(ns));
            }
            let one = cirrus_sheet(dir, angle);
            if (k == 0) {
                acc = one;
            } else {
                acc += one;
            }
        }
        cs = acc;
        if (ns > 1) {
            cs = acc / f32(ns);
        }
    }
    for (var k = 0; k < MAX_MARCH_SAMPLES; k++) {
        if (k >= n) {
            break;
        }
        let dir = view_ray((vec2<f32>(low_pix) + r2_offset(k)) / dims);
        let stratum = (jitter + f32(k)) / f32(n);
        var d = 0.0;
        var c = march(u.camera.xyz, dir, stratum, far_t, &d);
        // The scatter distance weighs the volumetric layer's opacity only (ADR-1408 keeps the
        // cirrus sheet out of it).
        let w = 1.0 - c.a;
        if (sheet_on) {
            // The cirrus sheet sits behind the layer: `bg · a_c + L_c` under `rgb + bg · a`
            // (ADR-1408), composited per march so the average stays linear.
            c = vec4<f32>(c.rgb + c.a * cs.rgb, c.a * cs.a);
        }
        if (k == 0) {
            color_sum = c;
            first_d = d;
        } else {
            color_sum += c;
        }
        dist_sum += w * d;
        dist_weight += w;
    }
    if (n <= 1) {
        return MarchOut(color_sum, vec4<f32>(first_d, scene_d, 1.0, 0.0));
    }
    var scatter_d = 0.0;
    if (dist_weight > 0.0) {
        scatter_d = dist_sum / dist_weight;
    }
    return MarchOut(color_sum / f32(n), vec4<f32>(scatter_d, scene_d, 1.0, 0.0));
}

// Reduced-resolution march of every texel (first frame, camera cut, reprojection off).
@fragment
fn fs_cloud_march(in: VsOut) -> MarchOut {
    return march_texel(vec2<i32>(in.pos.xy));
}

// Edge of the reprojection block (texels). Matches `CLOUD_BLOCK`.
const BLOCK: i32 = 4;
// Weight of a fresh sample over its history on a still texel. Matches `CLOUD_FRESH_WEIGHT`.
// A texel that moved a texel or more since the history frame, whose history has been
// resampled (softness ≥ 1), or whose block reads as stale takes the fresh sample whole.
const FRESH_WEIGHT: f32 = 0.5;
// Confidence a filled texel gains per frame while it blends in each frame's fill.
const FILL_GAIN: f32 = 0.25;

// The low-resolution texel fresh texel `block` marches this frame. A partial block at the
// right or bottom edge clamps to its last texel. Matches `cloud_fresh_texel`.
fn fresh_low(block: vec2<i32>) -> vec2<i32> {
    let low_dims = max(vec2<i32>(u.targets.xy), vec2<i32>(1));
    let sub = vec2<i32>(u.temporal.xy);
    return min(block * BLOCK + sub, low_dims - vec2<i32>(1));
}

// One texel of every 4×4 block, at this frame's Bayer sub-position.
@fragment
fn fs_cloud_fresh(in: VsOut) -> MarchOut {
    return march_texel(fresh_low(vec2<i32>(in.pos.xy)));
}

// Catmull-Rom fetch of the history colour at `uv`, as nine bilinear taps (the two middle
// weights of each axis share one tap). Each tap is clamped to the inner half-texel so the
// repeat sampler never wraps across the frame edge.
fn history_catmull_rom(uv: vec2<f32>) -> vec4<f32> {
    let size = max(vec2<f32>(textureDimensions(history_tex)), vec2<f32>(1.0));
    let inv = 1.0 / size;
    let sample_pos = uv * size;
    let tex1 = floor(sample_pos - vec2<f32>(0.5)) + vec2<f32>(0.5);
    let f = sample_pos - tex1;
    let w0 = f * (-0.5 + f * (1.0 - 0.5 * f));
    let w1 = 1.0 + f * f * (-2.5 + 1.5 * f);
    let w2 = f * (0.5 + f * (2.0 - 1.5 * f));
    let w3 = f * f * (-0.5 + 0.5 * f);
    let w12 = w1 + w2;
    let off12 = w2 / max(w12, vec2<f32>(1e-6));
    let lo = 0.5 * inv;
    let hi = vec2<f32>(1.0) - 0.5 * inv;
    let p0 = clamp((tex1 - vec2<f32>(1.0)) * inv, lo, hi);
    let p12 = clamp((tex1 + off12) * inv, lo, hi);
    let p3 = clamp((tex1 + vec2<f32>(2.0)) * inv, lo, hi);
    var acc = vec4<f32>(0.0);
    acc += textureSampleLevel(history_tex, linear_samp, vec2<f32>(p0.x, p0.y), 0.0) * (w0.x * w0.y);
    acc += textureSampleLevel(history_tex, linear_samp, vec2<f32>(p12.x, p0.y), 0.0) * (w12.x * w0.y);
    acc += textureSampleLevel(history_tex, linear_samp, vec2<f32>(p3.x, p0.y), 0.0) * (w3.x * w0.y);
    acc += textureSampleLevel(history_tex, linear_samp, vec2<f32>(p0.x, p12.y), 0.0) * (w0.x * w12.y);
    acc += textureSampleLevel(history_tex, linear_samp, vec2<f32>(p12.x, p12.y), 0.0) * (w12.x * w12.y);
    acc += textureSampleLevel(history_tex, linear_samp, vec2<f32>(p3.x, p12.y), 0.0) * (w3.x * w12.y);
    acc += textureSampleLevel(history_tex, linear_samp, vec2<f32>(p0.x, p3.y), 0.0) * (w0.x * w3.y);
    acc += textureSampleLevel(history_tex, linear_samp, vec2<f32>(p12.x, p3.y), 0.0) * (w12.x * w3.y);
    acc += textureSampleLevel(history_tex, linear_samp, vec2<f32>(p3.x, p3.y), 0.0) * (w3.x * w3.y);
    return acc;
}

// Distance along `dir` to the middle of the cloud layer, or 0 when the ray never meets it.
// A texel whose fresh neighbour saw no cloud reprojects from here.
fn layer_mid_distance(dir: vec3<f32>) -> f32 {
    let r = u.shape.w + 0.5 * (u.layer.x + u.layer.y);
    let hit = shell(u.camera.xyz, dir, r);
    if (hit.y > 0.0) {
        return select(hit.y, hit.x, hit.x > 0.0);
    }
    return 0.0;
}

// Two scene distances belong to different surfaces (disocclusion or a new occluder).
fn depth_breaks(a: f32, b: f32) -> bool {
    return abs(a - b) > max(32.0, 0.1 * min(a, b));
}

// Where the cloud seen along `dir` at distance `d` was in the history frame: uv (xy) and
// 1 in z when it lands on the history's screen. The wind drift since then is removed.
fn reproject_uv(dir: vec3<f32>, d: f32) -> vec3<f32> {
    if (u.reproject.z < 0.5 || d <= 0.0) {
        return vec3<f32>(0.0);
    }
    let world = u.camera.xyz + dir * d - vec3<f32>(u.reproject.x, 0.0, u.reproject.y);
    let clip = u.history_view_proj * vec4<f32>(world, 1.0);
    if (clip.w <= 1e-4) {
        return vec3<f32>(0.0);
    }
    let ndc = clip.xy / clip.w;
    let uv = vec2<f32>(0.5 + 0.5 * ndc.x, 0.5 - 0.5 * ndc.y);
    let inside = all(uv >= vec2<f32>(0.0)) && all(uv <= vec2<f32>(1.0));
    return vec3<f32>(uv, select(0.0, 1.0, inside));
}

// Distance to reproject a texel from: its cloud's scatter distance, else the layer middle,
// never past the texel's own surface.
fn reprojection_distance(dir: vec3<f32>, scatter: f32, scene_d: f32) -> f32 {
    var d = scatter;
    if (d <= 0.0) {
        d = layer_mid_distance(dir);
    }
    if (scene_d < SKY_M) {
        d = min(d, scene_d);
    }
    return d;
}

// How far a fresh sample and the reprojected history at the same texel disagree, in units
// of what start jitter alone produces: ≤ 1 is noise, larger means the history is stale
// (lighting, coverage, shape change). Transmittance compares absolutely; radiance relatively,
// weighted by how opaque the texel is, so clear sky never reads as change.
fn staleness(fresh: vec4<f32>, hist: vec4<f32>) -> f32 {
    let d_t = abs(fresh.a - hist.a) / 0.2;
    let lf = dot(fresh.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    let lh = dot(hist.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    let rel = abs(lf - lh) / max(max(lf, lh), 1e-4);
    let opacity = 1.0 - min(fresh.a, hist.a);
    return max(d_t, rel * opacity / 0.35);
}

// History colour (rgba) and distance at `uv`, with w of the last vec4 = 1 when usable.
// Catmull-Rom when the 2×2 history texels around `uv` all hold this surface and `uv` is off
// a texel centre; otherwise the nearest of them that does (a point fetch, so a depth edge never blends sky into rock);
// unusable when none does.
struct HistoryFetch {
    color: vec4<f32>,
    dist: vec4<f32>,
    ok: bool,
}

fn fetch_history(uv: vec2<f32>, scene_d: f32) -> HistoryFetch {
    let dims = max(vec2<i32>(textureDimensions(history_dist)), vec2<i32>(1));
    let pos = uv * vec2<f32>(dims) - vec2<f32>(0.5);
    let base = vec2<i32>(floor(pos));
    let frac = pos - floor(pos);
    var all_match = true;
    var best_w = -1.0;
    var best_p = vec2<i32>(0);
    for (var y = 0; y < 2; y++) {
        for (var x = 0; x < 2; x++) {
            let p = clamp(base + vec2<i32>(x, y), vec2<i32>(0), dims - vec2<i32>(1));
            let g = textureLoad(history_dist, p, 0).g;
            let wx = select(1.0 - frac.x, frac.x, x == 1);
            let wy = select(1.0 - frac.y, frac.y, y == 1);
            if (depth_breaks(g, scene_d)) {
                all_match = false;
            } else if (wx * wy > best_w) {
                best_w = wx * wy;
                best_p = p;
            }
        }
    }
    if (best_w < 0.0) {
        return HistoryFetch(vec4<f32>(0.0), vec4<f32>(0.0), false);
    }
    var dist = textureLoad(history_dist, best_p, 0);
    // On a texel centre (a held camera, no wind) copy the texel exactly. A filtered value
    // written back to half floats every frame drifts by the conversion's rounding.
    let off_centre = abs(frac - round(frac));
    let off = max(off_centre.x, off_centre.y);
    if (all_match && off > 1e-3) {
        // A half-texel resample softens most; count it as one unit of softness.
        dist.a = min(dist.a + 2.0 * off, 4.0);
        return HistoryFetch(history_catmull_rom(uv), dist, true);
    }
    return HistoryFetch(textureLoad(history_tex, best_p, 0), dist, true);
}

// `v` rounded to the nearest half float (ties to even), so writing it to an `Rgba16Float`
// target is exact. The history is rewritten every frame; a target whose conversion rounds
// toward zero (lavapipe) otherwise darkens it a little on each rewrite.
fn half_round(v: f32) -> f32 {
    let a = min(abs(v), 65504.0);
    // Below the smallest normal half the step is fixed at 2^-24.
    var ulp = 5.9604645e-8;
    if (a >= 6.1035156e-5) {
        let e = i32((bitcast<u32>(a) >> 23u) & 0xffu) - 127;
        ulp = exp2(f32(e - 10));
    }
    return sign(v) * round(a / ulp) * ulp;
}

// `v` with each NaN or ±∞ component replaced by `fallback`'s (exponent bits all ones; `max`
// and `clamp` do not remove NaN: SPIR-V leaves FMax/FClamp of a NaN undefined). The test is
// `(bits << 1) >= 0xff000000`: the masked `(bits & 0x7f800000) == 0x7f800000` form is folded
// away on computed values by the NVIDIA 620 Linux Vulkan compiler (ADR-3150, ADR-8366).
fn finite_or4(v: vec4<f32>, fallback: vec4<f32>) -> vec4<f32> {
    let bad = (bitcast<vec4<u32>>(v) << vec4<u32>(1u)) >= vec4<u32>(0xff000000u);
    return select(v, fallback, bad);
}

fn half_round4(v: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(half_round(v.x), half_round(v.y), half_round(v.z), half_round(v.w));
}

// Rebuilds every low-resolution texel from this frame's fresh texels and the history.
// - The texel this frame marched blends its fresh value with the history.
// - The others reproject: the world point at the texel's scatter distance (the fresh
//   neighbour's, else the layer middle; never past the texel's own surface), minus the
//   wind drift since the history frame, projected with the history's unjittered
//   view-projection, fetched depth-aware (`fetch_history`).
// - The history is clamped to the min/max of the 3×3 fresh neighbours only as far as the
//   block's own fresh sample disagrees with the history at that texel (`staleness`), or the
//   weather moved since the history frame (`reproject.w`, ADR-429): a still or rigidly
//   moving sky keeps its sub-block detail, a changed one is pulled in.
// - Off screen, no history, or no history texel on this surface (disocclusion): the fresh
//   texels around it, bilinear in fresh space, skipping those on another surface; the
//   closest-depth fresh neighbour when all are.
@fragment
fn fs_cloud_reconstruct(in: VsOut) -> MarchOut {
    let low = vec2<i32>(in.pos.xy);
    let low_dims = max(vec2<i32>(u.targets.xy), vec2<i32>(1));
    let fresh_dims = max(vec2<i32>(u.temporal.zw), vec2<i32>(1));
    let block = min(low / BLOCK, fresh_dims - vec2<i32>(1));
    let scene_d = footprint_scene(low);
    let own_c = textureLoad(cloud_tex, block, 0);
    let own_d = textureLoad(cloud_dist, block, 0);
    var lo = own_c;
    var hi = own_c;
    var near_c = own_c;
    var near_d = own_d;
    var near_err = abs(own_d.g - scene_d);
    for (var dy = -1; dy <= 1; dy++) {
        for (var dx = -1; dx <= 1; dx++) {
            let p = clamp(block + vec2<i32>(dx, dy), vec2<i32>(0), fresh_dims - vec2<i32>(1));
            let c = textureLoad(cloud_tex, p, 0);
            let d = textureLoad(cloud_dist, p, 0);
            lo = min(lo, c);
            hi = max(hi, c);
            let err = abs(d.g - scene_d);
            if (err < near_err) {
                near_err = err;
                near_c = c;
                near_d = d;
            }
        }
    }
    // Dilation: fresh texel `b` sits at low texel `b * 4 + sub`, so this texel is at
    // `(low − sub) / 4` in fresh space. Bilinear over the four around it, on this surface.
    let fc = (vec2<f32>(low) - u.temporal.xy) / f32(BLOCK);
    let fb = vec2<i32>(floor(fc));
    let ff = fc - floor(fc);
    var dil_acc = vec4<f32>(0.0);
    var dil_w = 0.0;
    var dil_d = near_d;
    var dil_best = -1.0;
    for (var y = 0; y < 2; y++) {
        for (var x = 0; x < 2; x++) {
            let p = clamp(fb + vec2<i32>(x, y), vec2<i32>(0), fresh_dims - vec2<i32>(1));
            let d = textureLoad(cloud_dist, p, 0);
            if (!depth_breaks(d.g, scene_d)) {
                let wx = select(1.0 - ff.x, ff.x, x == 1);
                let wy = select(1.0 - ff.y, ff.y, y == 1);
                let w = max(wx * wy, 1e-4);
                dil_acc += textureLoad(cloud_tex, p, 0) * w;
                dil_w += w;
                if (w > dil_best) {
                    dil_best = w;
                    dil_d = d;
                }
            }
        }
    }
    var dil_c = near_c;
    if (dil_w > 0.0) {
        dil_c = dil_acc / dil_w;
    }
    let is_fresh = all(low == fresh_low(block));
    if (is_fresh) {
        dil_c = own_c;
        dil_d = own_d;
    }
    let dims_f = vec2<f32>(low_dims);
    // Staleness of this block: its fresh sample against the history at the same texel.
    let f_low = fresh_low(block);
    let f_dir = view_ray((vec2<f32>(f_low) + vec2<f32>(0.5)) / dims_f);
    let f_uv = reproject_uv(f_dir, reprojection_distance(f_dir, own_d.r, own_d.g));
    var clamp_k = 1.0;
    if (f_uv.z > 0.5) {
        let f_hist = fetch_history(f_uv.xy, own_d.g);
        if (f_hist.ok) {
            clamp_k = sat(staleness(own_c, f_hist.color) - 1.0);
        }
    }
    // While the weather changes, a texel's history is stale by as much as the weather moved
    // since the texel was last marched, even where one fresh sample cannot show it yet
    // (ADR-429). Per texel, not per frame (ADR-1410): one clamp for the whole frame kept every
    // texel pulled into its fresh neighbours' box for as long as any texel was stale, so the
    // texels already re-marched at the new weather lost their sub-block detail too. Zero with
    // steady weather.
    let sub = clamp(low - block * BLOCK, vec2<i32>(0), vec2<i32>(BLOCK - 1));
    clamp_k = max(clamp_k, sat(u.stale[sub.y][sub.x]));
    let dir = view_ray((vec2<f32>(low) + vec2<f32>(0.5)) / dims_f);
    let prev = reproject_uv(dir, reprojection_distance(dir, dil_d.r, scene_d));
    var color = dil_c;
    var scatter = dil_d.r;
    // A marched texel is fully trusted; a filled one starts at 0.
    var conf = select(0.0, 1.0, is_fresh);
    // A filled texel is as soft as one resample; a marched one is sharp.
    var soft = select(1.0, 0.0, is_fresh);
    if (prev.z > 0.5) {
        let h = fetch_history(prev.xy, scene_d);
        if (h.ok) {
            let hist = mix(h.color, clamp(h.color, lo, hi), clamp_k);
            if (is_fresh) {
                // Texels the history moved by (each move resamples it and softens it). A moved
                // or stale history (lighting, wind detail, softened by a pan) is replaced.
                let moved = length(prev.xy * dims_f - (vec2<f32>(low) + vec2<f32>(0.5)));
                let w = mix(FRESH_WEIGHT, 1.0, max(max(sat(moved), clamp_k), sat(h.dist.a)));
                color = mix(hist, own_c, w);
                soft = (1.0 - w) * h.dist.a;
                scatter = select(h.dist.r, own_d.r, own_d.r > 0.0);
            } else {
                // A filled texel keeps averaging in this frame's fill (a new set of fresh
                // texels each frame) until it is marched or confident.
                let c = sat(h.dist.b);
                color = mix(dil_c, hist, 0.5 + 0.5 * c);
                scatter = h.dist.r;
                conf = c + (1.0 - c) * FILL_GAIN;
                soft = mix(1.0, h.dist.a, 0.5 + 0.5 * c);
            }
        }
    }
    // This is next frame's history: a NaN or ∞ written here would be reprojected into every
    // later frame (ADR-869). A non-finite component restarts as clear sky (no in-scatter,
    // transmittance 1) and an unknown distance as the sky with no confidence.
    color = finite_or4(color, vec4<f32>(0.0, 0.0, 0.0, 1.0));
    color = vec4<f32>(max(color.rgb, vec3<f32>(0.0)), clamp(color.a, 0.0, 1.0));
    let dist = finite_or4(vec4<f32>(scatter, scene_d, conf, soft), vec4<f32>(0.0, SKY_M, 0.0, 1.0));
    return MarchOut(half_round4(color), dist);
}

// Distance along unit `dir` from the eye to the first point of the volumetric layer: the
// march's own `t0` (`march`). 0 with the eye inside the layer; `SKY_M` when the ray never
// meets it, or the layer has no coverage (a cirrus sheet alone draws on sky texels only).
fn layer_entry_distance(dir: vec3<f32>) -> f32 {
    if (u.layer.w <= 0.0) {
        return SKY_M;
    }
    let r = u.shape.w;
    let r_eye = r + altitude(u.camera.xyz);
    if (r_eye < r + u.layer.x) {
        let bottom = shell(u.camera.xyz, dir, r + u.layer.x);
        return select(SKY_M, bottom.y, bottom.y > 0.0);
    }
    if (r_eye <= r + u.layer.y) {
        return 0.0;
    }
    let top = shell(u.camera.xyz, dir, r + u.layer.y);
    return select(SKY_M, top.x, top.x > 0.0);
}

// Depth-aware upsample over the whole frame (not only sky). The 2×2 neighbourhood is
// searched for the scene distance closest to this pixel. Samples within 1 m of that
// best distance blend with bilinear weights, so a flat sky (every depth ties) stays
// smooth; a nearer surface wins outright and is not mixed with the sky sample behind
// it. A cloud that sits behind the pixel is dropped, which stops a mixed footprint
// that marched out to the sky from covering a nearer rock. Blend is scene * T + S,
// so (0, 0, 0, 1) leaves the pixel as the geometry pass drew it.
//
// A surface nearer than the point where its view ray enters the layer has no cloud in front
// of it, whatever the low-resolution texels around it hold, and is left as drawn (ADR-5792).
// The scatter-distance test below cannot say so on its own: a texel whose centre is on the
// surface but whose footprint reached the sky holds cloud radiance, the reconstruction's
// Catmull-Rom history fetch spreads that radiance over the neighbouring surface texels while
// their scatter distance stays 0 (a point fetch), and a scatter distance of 0 reads as "no
// cloud to drop". Trees against a cloudy sky then carried texel-sized specks of cloud.
@fragment
fn fs_cloud_composite(in: VsOut) -> @location(0) vec4<f32> {
    let pix = vec2<i32>(in.pos.xy);
    let sd = surface_distance(pix, u.camera.xyz);
    let full = max(u.targets.zw, vec2<f32>(1.0));
    let low = max(u.targets.xy, vec2<f32>(1.0));
    let uv = in.pos.xy / full;
    if (sd < SKY_M && sd <= layer_entry_distance(view_ray(uv))) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    let low_f = uv * low - vec2<f32>(0.5);
    let base = vec2<i32>(floor(low_f));
    let frac = low_f - floor(low_f);
    let low_dims = vec2<i32>(textureDimensions(cloud_tex));
    var best_err = 1e30;
    for (var y = 0; y < 2; y++) {
        for (var x = 0; x < 2; x++) {
            let p = clamp(base + vec2<i32>(x, y), vec2<i32>(0), low_dims - vec2<i32>(1));
            let scene_d = textureLoad(cloud_dist, p, 0).g;
            best_err = min(best_err, abs(scene_d - sd));
        }
    }
    var acc = vec4<f32>(0.0);
    var acc_scatter = 0.0;
    var wsum = 0.0;
    for (var y = 0; y < 2; y++) {
        for (var x = 0; x < 2; x++) {
            let p = clamp(base + vec2<i32>(x, y), vec2<i32>(0), low_dims - vec2<i32>(1));
            let dist = textureLoad(cloud_dist, p, 0);
            let err = abs(dist.g - sd);
            if (abs(err - best_err) <= 1.0) {
                let wx = select(1.0 - frac.x, frac.x, x == 1);
                let wy = select(1.0 - frac.y, frac.y, y == 1);
                let bw = wx * wy;
                acc += textureLoad(cloud_tex, p, 0) * bw;
                acc_scatter += dist.r * bw;
                wsum += bw;
            }
        }
    }
    var out_c = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    var best_scatter = 0.0;
    if (wsum > 0.0) {
        out_c = acc / wsum;
        best_scatter = acc_scatter / wsum;
    }
    // Half-float ulp at 8 km is about 8 m and at 60 km about 32 m.
    let slack = max(32.0, sd * 0.02);
    if (best_scatter > 0.0 && best_scatter > sd + slack) {
        out_c = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    let scale = select(1.0, u.hdr.x, u.hdr.x > 0.0);
    return vec4<f32>(out_c.rgb * scale, clamp(out_c.a, 0.0, 1.0));
}

// The 3D noise fills only part of a cloudy column, and multiple scattering lightens the
// shadow below it; the shadow uses this fraction of the column's optical depth.
const COLUMN_FILL: f32 = 0.05;

// Optical depth of the weather column (no 3D noise): cloud shadows and the top view.
fn column_depth(xz: vec2<f32>) -> f32 {
    let steps = 8;
    let thickness = max(u.layer.y - u.layer.x, 0.0);
    let dt = thickness / f32(steps);
    var od = 0.0;
    let w = weather_at(xz);
    let cov = sat(w.x * mix(0.75, 1.15, u.layer.w));
    for (var i = 0; i < steps; i++) {
        let h = (f32(i) + 0.5) / f32(steps);
        od += cov * type_gradient(h, w.y) * dt;
    }
    // Thin weather (low coverage) erodes to sparse wisps: the mean density of a column grows
    // with the square of its coverage (ADR-1443), so the raw weather value weights the depth
    // again. 1 where the weather is full, so thick cloud keeps its depth.
    return od * u.layer.z * sat(w.x);
}

fn projected_xz(ground_xz: vec2<f32>, ground_y: f32) -> vec2<f32> {
    let mid = 0.5 * (u.layer.x + u.layer.y);
    let rise = mid - ground_y;
    let up = u.sun.y;
    if (abs(up) < 1e-3) {
        return ground_xz;
    }
    return ground_xz + u.sun.xz * (rise / up);
}

fn encode_color(color: vec3<f32>) -> vec4<f32> {
    return vec4<f32>(regia::utils::encode_srgb(clamp(color, vec3<f32>(0.0), vec3<f32>(1.0))), 1.0);
}

// Top-down view of one weather tile (tests): an orthographic march straight down through
// the full 3D density. White where the column is optically thick (T < ½ reads as cloud).
@fragment
fn fs_cloud_coverage(in: VsOut) -> @location(0) vec4<f32> {
    let res = max(u.draw.z, 1.0);
    let uv = in.pos.xy / res;
    let xz = (uv - vec2<f32>(0.5)) * u.wind.w;
    let steps = 48;
    let dt = (u.layer.y - u.layer.x) / f32(steps);
    var od = 0.0;
    for (var i = 0; i < steps; i++) {
        let alt = u.layer.x + (f32(i) + 0.5) * dt;
        od += density_at(vec3<f32>(xz.x, alt, xz.y), false) * u.layer.z * dt;
    }
    let transmit = exp(-od);
    let sky = vec3<f32>(0.12, 0.28, 0.62);
    // Thicker columns read brighter from above (more of the column scatters back up).
    let cloud = vec3<f32>(0.96, 0.95, 0.92) * (0.55 + 0.45 * clamp(od / 60.0, 0.0, 1.0));
    return encode_color(mix(cloud, sky, clamp(transmit, 0.0, 1.0)));
}

// Ground under the projected cloud column (tests).
@fragment
fn fs_cloud_ground(in: VsOut) -> @location(0) vec4<f32> {
    let res = max(u.draw.z, 1.0);
    let uv = in.pos.xy / res;
    let ground_xz = (uv - vec2<f32>(0.5)) * u.wind.w;
    let xz = projected_xz(ground_xz, 0.0);
    let transmit = exp(-column_depth(xz) * COLUMN_FILL);
    let shade = mix(transmit, 1.0, u.draw.w);
    return encode_color(vec3<f32>(0.62, 0.58, 0.50) * shade);
}

// ---- Cloud transmittance map (ADR-320) ----
// A top-down R16F map of the sun's transmittance through the layer (Hillaire, "Physically Based
// Sky, Atmosphere and Cloud Rendering in Frostbite", SIGGRAPH 2016: a cloud shadow map read by
// the surface and volumetric lighting; Bauer, "Creating the Atmospheric World of Red Dead
// Redemption 2", SIGGRAPH 2019: clouds shadowing the fog). Texel (i, j) holds the sun line
// through the point where it crosses the plane y = layer base. Addressing is toroidal: world
// cell c lives in texel c mod size, so a moving window keeps its texels. On the map pass's own
// uniform `temporal` = (window start x, z in cells, map size, texel metres).

// Density samples along the sun per texel. Matches `CLOUD_SHADOW_MAP_STEPS`.
const SHADOW_MAP_STEPS: i32 = 16;

// The world cell texel `texel` holds in the current window. Matches `cloud_shadow_texel_cell`.
fn shadow_map_cell(texel: vec2<i32>) -> vec2<f32> {
    let size = max(i32(u.temporal.z), 1);
    let n = vec2<i32>(size);
    let start = vec2<i32>(u.temporal.xy);
    let rel = ((texel - start) % n + n) % n;
    return vec2<f32>(start + rel);
}

// Sun transmittance of the line along `u.sun.xyz` through (x, layer base, z): from where it
// leaves the base shell to where it leaves the top shell, 16 midpoint samples of the march's own
// density. Matches `cloud_shadow_march` (a missed shell is (−1, −1) in both).
fn shadow_map_transmittance(xz: vec2<f32>) -> f32 {
    let to_sun = u.sun.xyz;
    let r = u.shape.w;
    let o = vec3<f32>(xz.x, u.layer.x, xz.y);
    let bottom = shell(o, to_sun, r + u.layer.x);
    let top = shell(o, to_sun, r + u.layer.y);
    let t0 = min(bottom.y, 0.0);
    let t1 = top.y;
    if (t1 <= t0) {
        return 1.0;
    }
    let dt = (t1 - t0) / f32(SHADOW_MAP_STEPS);
    var od = 0.0;
    for (var i = 0; i < SHADOW_MAP_STEPS; i++) {
        let t = t0 + (f32(i) + 0.5) * dt;
        od += density_at(o + to_sun * t, false) * dt;
    }
    return exp(-od * u.layer.z);
}

// One map texel: the sun's transmittance at the base-plane centre of the cell it holds.
@fragment
fn fs_cloud_shadow_map(in: VsOut) -> @location(0) vec4<f32> {
    let cell = shadow_map_cell(vec2<i32>(in.pos.xy));
    let xz = (cell + vec2<f32>(0.5)) * max(u.temporal.w, 1e-3);
    return vec4<f32>(shadow_map_transmittance(xz), 0.0, 0.0, 1.0);
}

// Debug view `cloud_shadow_map` (ADR-320): the sun's transmittance through the layer from the
// cloud transmittance map (bound as `cloud_tex`), grey at every surface; sky pixels show it
// where the view ray meets the layer base (black where it never does). `hdr.z` is the map
// extent. Written at 5000 cd/m² × T so the frame's exposure shows it near mid grey.
@fragment
fn fs_cloud_shadow_debug(in: VsOut) -> @location(0) vec4<f32> {
    let pix = vec2<i32>(in.pos.xy);
    let depth = textureLoad(scene_depth, pix, 0);
    let eye = u.camera.xyz;
    var p = eye;
    if (depth >= 1.0) {
        let full = max(u.targets.zw, vec2<f32>(1.0));
        let dir = view_ray(in.pos.xy / full);
        let k = (u.layer.x - eye.y) / dir.y;
        if (abs(dir.y) < 1e-3 || k <= 0.0) {
            return vec4<f32>(0.0, 0.0, 0.0, 0.0);
        }
        p = eye + dir * k;
    } else {
        let dims = max(vec2<f32>(textureDimensions(scene_depth)), vec2<f32>(1.0));
        let uv = in.pos.xy / dims;
        let ndc = vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
        let h = u.inv_view_proj * vec4<f32>(ndc, depth, 1.0);
        p = h.xyz / h.w;
    }
    let params = vec4<f32>(u.hdr.z, u.layer.x, u.layer.y, 1.0);
    let t = regia::cloud_shadow::cloud_sun_transmittance(cloud_tex, params, p, u.sun.xyz, eye);
    let scale = select(1.0, u.hdr.x, u.hdr.x > 0.0);
    return vec4<f32>(vec3<f32>(t * 5000.0 * scale), 0.0);
}

// Cloud shadow on opaque pixels: multiplies the shaded colour. Not drawn when the cloud
// transmittance map shadows the sun term instead (`CloudSettings::shadow_map`).
@fragment
fn fs_cloud_shadow(in: VsOut) -> @location(0) vec4<f32> {
    let depth = textureLoad(scene_depth, vec2<i32>(in.pos.xy), 0);
    if (depth >= 1.0) {
        discard;
    }
    let dims = max(vec2<f32>(textureDimensions(scene_depth)), vec2<f32>(1.0));
    let uv = in.pos.xy / dims;
    let ndc = vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    let h = u.inv_view_proj * vec4<f32>(ndc, depth, 1.0);
    let world = h.xyz / h.w;
    let xz = projected_xz(world.xz, world.y);
    let transmit = exp(-column_depth(xz) * COLUMN_FILL);
    let shade = mix(transmit, 1.0, u.draw.w);
    return vec4<f32>(0.0, 0.0, 0.0, clamp(shade, 0.0, 1.0));
}
