// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::cloud_density

// The volumetric cloud density, one definition for the raster march (`clouds.wgsl`) and the
// reference path tracer's cloud field (`regia_pt/shaders/pt.wgsl`, ADR-2127). Everything here is
// pure arithmetic on values the caller has already fetched: the raster samples its textures with
// a hardware sampler, the tracer reads the same texels (`clouds::cloud_noise`, the weather map,
// `clouds::cloud_curl`) from a storage buffer and filters them by hand. The order of the steps
// is the order `density_at` in `clouds.wgsl` calls them.
//
// Shape: Schneider, "The Real-Time Volumetric Cloudscapes of Horizon: Zero Dawn", SIGGRAPH 2015,
// and GPU Pro 7 ch. 4; height profiles: Schneider & Vos, "Nubis", SIGGRAPH 2017.

fn remap(v: f32, lo: f32, hi: f32, new_lo: f32, new_hi: f32) -> f32 {
    return new_lo + (v - lo) / max(hi - lo, 1e-5) * (new_hi - new_lo);
}

fn sat(v: f32) -> f32 {
    return clamp(v, 0.0, 1.0);
}

// Nubis height profiles: stratus (thin, low), cumulus (rounded base, tapering top),
// towering cumulus (tall). `t` blends stratus → cumulus → towering.
fn type_gradient(h: f32, t: f32) -> f32 {
    let stratus = sat(remap(h, 0.0, 0.08, 0.0, 1.0)) * sat(remap(h, 0.18, 0.3, 1.0, 0.0));
    let cumulus = sat(remap(h, 0.0, 0.2, 0.0, 1.0)) * sat(remap(h, 0.35, 0.7, 1.0, 0.0));
    let tower = sat(remap(h, 0.0, 0.1, 0.0, 1.0)) * sat(remap(h, 0.7, 1.0, 1.0, 0.0));
    let a = mix(stratus, cumulus, sat(t * 2.0));
    return mix(a, tower, sat(t * 2.0 - 1.0));
}

// Height of `p` above the planet surface (radius `planet`), with the camera at the local origin.
fn cloud_altitude(p: vec3<f32>, planet: f32) -> f32 {
    let c = vec3<f32>(0.0, -planet, 0.0);
    return length(p - c) - planet;
}

// Position of an altitude inside the layer, 0 at the base and 1 at the top.
fn cloud_layer_fraction(alt: f32, bottom: f32, top: f32) -> f32 {
    return sat((alt - bottom) / max(top - bottom, 1.0));
}

// Distances along unit `dir` from `eye` to a sphere of radius `r` around the planet centre.
// x: nearer root, y: farther root. A miss is `(-1, -1)`. Roots behind the ray stay negative
// (the sphere is there; the ray does not meet it in front).
//
// `|o|² − r²` expanded about the planet radius, so f32 never squares a 6e6 m length
// (that square's ulp is kilometres). `o.y − planet` is the camera height. The root uses
// `q = −(b + sign(b)√disc)` (Haines, Günther, Akenine-Möller, Ray Tracing Gems ch. 7).
fn cloud_shell(eye: vec3<f32>, dir: vec3<f32>, r: f32, planet: f32) -> vec2<f32> {
    let c0 = vec3<f32>(0.0, -planet, 0.0);
    let o = eye - c0;
    let b = dot(o, dir);
    let shell_alt = r - planet;
    let excess = o.y - planet;
    let horiz2 = o.x * o.x + o.z * o.z;
    let cc = horiz2 + 2.0 * planet * (excess - shell_alt) + excess * excess - shell_alt * shell_alt;
    let disc = b * b - cc;
    if (disc < 0.0) {
        return vec2<f32>(-1.0);
    }
    let s = sqrt(disc);
    let sign_b = select(-1.0, 1.0, b >= 0.0);
    let q = -(b + sign_b * s);
    let tiny = abs(q) < 1e-4;
    let q_safe = select(q, -1.0, tiny);
    let t_div = cc / q_safe;
    let t_a = select(q, -b - s, tiny);
    let t_b = select(t_div, -b + s, tiny);
    return vec2<f32>(min(t_a, t_b), max(t_a, t_b));
}

// Weather map coordinate of world `xz`, drifted by the wind. `domain` is the tile (m).
fn density_weather_uv(xz: vec2<f32>, drift: vec2<f32>, domain: f32) -> vec2<f32> {
    let scrolled = xz - drift;
    return scrolled / max(domain, 1.0) + vec2<f32>(0.5);
}

// Shape-noise coordinate of the edge breakup at world `xz`. `tile` is the breakup tile (m).
fn density_breakup_coord(xz: vec2<f32>, drift: vec2<f32>, tile: f32) -> vec3<f32> {
    let bq = (xz - drift) / max(tile, 1.0);
    return vec3<f32>(bq.x, 0.43, bq.y);
}

// Edge breakup (ADR-1408): a finer Worley coverage noise moves the weather edge so the field
// breaks into lobes and cells. Zero mean, so the covered fraction stays. `bn` is the shape
// noise at `density_breakup_coord`.
fn density_breakup(w: f32, amount: f32, bn: vec4<f32>) -> f32 {
    let fine = bn.g * 0.6 + bn.b * 0.4;
    return sat(w + amount * (fine - 0.5) * 2.0);
}

// The column's cover from the weather value and the layer's coverage.
fn density_cover(w: f32, coverage: f32) -> f32 {
    return sat(w * mix(0.75, 1.15, coverage));
}

// Shape-noise coordinate of `p`. `drift` is the wind drift (x, 0, z); `tile` the shape tile (m).
fn density_shape_coord(p: vec3<f32>, drift: vec3<f32>, tile: f32) -> vec3<f32> {
    return (p - drift) / max(tile, 1.0);
}

// The base shape: `s` (shape noise) inside the cover and the height profile. The result is the
// density before the detail erosion, at most `cov`.
fn density_base(s: vec4<f32>, cov: f32, grad: f32) -> f32 {
    let fbm = s.g * 0.625 + s.b * 0.25 + s.a * 0.125;
    var base = sat(remap(s.r, -(1.0 - fbm), 1.0, 0.0, 1.0));
    base *= grad;
    return sat(remap(base, 1.0 - cov, 1.0, 0.0, 1.0)) * cov;
}

// The point the detail erosion reads, before its curl swirl (the detail drifts faster).
fn density_detail_anchor(p: vec3<f32>, drift: vec3<f32>) -> vec3<f32> {
    return p - drift * 1.6;
}

// Curl-field coordinate of a detail point. `tile` is the curl tile (m).
fn density_curl_uv(dq: vec3<f32>, tile: f32) -> vec2<f32> {
    return dq.xz / max(tile, 1.0);
}

// Curl-noise swirl of the detail erosion, strongest at the wispy bases. `rg` is the curl texel
// (0..1 encoding −1..1), `strength` the displacement (m), `h` the layer fraction.
fn density_curl_swirl(dq: vec3<f32>, rg: vec2<f32>, strength: f32, h: f32) -> vec3<f32> {
    let curl = rg * 2.0 - vec2<f32>(1.0);
    return dq + vec3<f32>(curl.x, 0.0, curl.y) * (strength * (1.0 - h));
}

// Detail-noise coordinate of a detail point. `tile` is the detail tile (m).
fn density_detail_coord(dq: vec3<f32>, tile: f32) -> vec3<f32> {
    return dq / max(tile, 1.0);
}

// The base shape eroded by the detail noise `d`. Wispy bases, billowy tops. `erosion` is the
// look's depth, 0.5 before ADR-1408 (a zero row is the old layer).
fn density_erode(base: f32, d: vec4<f32>, h: f32, erosion: f32) -> f32 {
    let dfbm = d.r * 0.625 + d.g * 0.25 + d.b * 0.125;
    let dmod = mix(dfbm, 1.0 - dfbm, sat(h * 4.0));
    let depth = select(0.5, erosion, erosion > 0.0);
    return sat(remap(base, dmod * depth, 1.0, 0.0, 1.0));
}
