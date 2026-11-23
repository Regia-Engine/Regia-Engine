// SPDX-License-Identifier: MPL-2.0
// Water bodies in the runtime frame (ADR-1399). Drawn by `WaterRenderer`'s translucent layer after
// the opaque frame, with the scene depth bound read-only (depth compare Always: the depth test is
// done here, so the wet band beside a stream can darken banks that stand in front of the surface).
//
// Two compositions:
// - composite (High, Ultra): the frame's colour copy is sampled for refraction and screen-space
//   reflection; the output replaces the pixel (premultiplied, alpha = waterline fade);
// - blend (Low, or no copy): the surface adds reflection and in-scatter and covers the bed by the
//   Beer-Lambert opacity through fixed-function blending.
// Colours are radiance times `out_scale` (the frame's pre-exposure on an HDR frame). On a display
// pass (mode.w > 0) they are tonemapped with 1 - exp(-x) and, for a display-encoded target, gamma
// encoded.
//
// Sources (docs/provenance.md, ADR-1399): flow maps after Vlachos 2010; LEAN-style roughness from
// the stored mean squared slope (Olano & Baker 2010); Schlick Fresnel; GGX with Smith G (Walter et
// al. 2007); Beer-Lambert transmittance and single scattering in a homogeneous medium (PBR 3rd ed.
// § 11.1); potential flow around a cylinder; standing waves at the deep-water wavelength
// 2 pi v^2 / g; the Kelvin wake half-angle (asin 1/3).
//
// Swell (ADR-1620): trochoidal waves (Gerstner 1802; Fournier & Reeves 1986; Tessendorf 2001;
// Finch 2004) with deep-water dispersion, on a camera-centered ring grid (after Losasso & Hoppe
// 2004). The CPU mirror is `swell.rs` (`SwellInstant::grid_offset`, `SwellInstant::sample`) and
// `gpu.rs` (`ring_vertex`); crest foam from the Jacobian of the horizontal map (Tessendorf 2001).
//
// FFT ocean (ADR-1969): tiling maps from the compute pass `water_ocean.wgsl`, read with
// `textureLoad`, blended by the depth with the shoaled swell. The CPU mirror is `ocean.rs`
// (`OceanFrame::grid_offset`, `OceanFrame::sample`, `FftOcean::shore_weight`).

struct Frame {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    // xyz eye, w wrapped seconds
    eye: vec4<f32>,
    // xyz toward the sun, w sun illuminance x out_scale (0: no sun)
    sun_dir: vec4<f32>,
    // rgb sun colour, w ambient (sky) illuminance x out_scale
    sun_color: vec4<f32>,
    // rgb zenith radiance x out_scale
    sky_zenith: vec4<f32>,
    // rgb horizon radiance x out_scale
    sky_horizon: vec4<f32>,
    // width, height, 1/width, 1/height
    screen: vec4<f32>,
    // near, far, tan(fov_y / 2), height of the water surface above the eye's xz when the eye is
    // under water (ADR-2342; 0 otherwise)
    depth: vec4<f32>,
    // flow phase a, phase b, weight a, cycle seconds
    flow: vec4<f32>,
    // coarse phase, coarse weight, coarse cycle seconds, two ripple scales (1) or one (0)
    coarse: vec4<f32>,
    // ripple scroll in tiles: layer a xy, layer b zw
    scroll: vec4<f32>,
    // x composite (1) or blend (0), y reflection steps, z obstacles, w display pass (0 HDR,
    // 1 linear display target, 2 display-encoded target)
    mode: vec4<f32>,
    // x hit refinement steps, y sun shadow steps, z debug view (0 off, 1 transmitted, 2 reflected,
    // 3 normal, 4 Fresnel, 5 foam (r) and ice (g)), w the underwater view (ADR-2342): 0 off (the
    // surface is not drawn from below), 1 on with the eye above water, body slot + 2 with the eye
    // under that body's surface
    misc: vec4<f32>,
}

struct Body {
    // rgb absorption (1/m), w scattering (1/m)
    absorption: vec4<f32>,
    // rgb scatter albedo, w roughness
    scatter: vec4<f32>,
    // normal strength, ripple tile (m), refraction, foam
    surface: vec4<f32>,
    // wet height (m), wet width (m), wet darkening, glint
    wet: vec4<f32>,
    // ice width (m), ice max depth (m), ice on (1), clarity
    ice: vec4<f32>,
    // rgb ice albedo
    ice_color: vec4<f32>,
    // caustics (ADR-2432): x strength (0: off, also on a tier that compiles the pass out),
    // y pattern scale, z chromatic spread, w shaft march steps (0: no shafts)
    caustic: vec4<f32>,
}

struct Bodies {
    b: array<Body, 16>,
}

struct Obstacles {
    // x, z, radius, strength
    o: array<vec4<f32>, 64>,
}

@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var<uniform> bodies: Bodies;
@group(0) @binding(2) var<uniform> obstacles: Obstacles;
@group(0) @binding(3) var depth_tex: texture_depth_2d;
@group(0) @binding(4) var scene_tex: texture_2d<f32>;
@group(0) @binding(5) var ripple_tex: texture_2d<f32>;
@group(0) @binding(6) var ripple_samp: sampler;
@group(0) @binding(7) var clamp_samp: sampler;

struct SwellBody {
    // x first wave in the table, y waves, z crest foam strength, w foam onset (1 - J)
    header: vec4<f32>,
    // rest rectangle: center x, center z, half x, half z
    rect: vec4<f32>,
    // x still level, y 1 = drawn on the ring grid with this swell, z 1 = rides the frame's ocean
    level: vec4<f32>,
}

struct Swell {
    // ring grid center x, z (the camera snapped to two cells), finest cell (m), inner half size (m)
    grid: vec4<f32>,
    b: array<SwellBody, 16>,
    // per wave: (k x, k z, A, H), then (phase at the grid center, omega, 0, 0)
    w: array<vec4<f32>, 256>,
    // shifts of the ring boundaries, two per vec4 (per-ring snap, ADR-2342)
    shifts: array<vec4<f32>, 10>,
}

@group(0) @binding(8) var<uniform> swell: Swell;

// Shore fields (spec 87, ADR-1828): per texel the depth (x) and the signed shoreline distance (y),
// one layer per field; High and Ultra only (Low binds none).
struct Shore {
    // per field: (origin x, origin z, cell, 0), (texels x, texels z, 0, 0)
    f: array<vec4<f32>, 8>,
    // field + 1 of each body slot (0: none), four slots per vec4
    map: array<vec4<f32>, 4>,
}

@group(0) @binding(9) var shore_tex: texture_2d_array<f32>;
@group(0) @binding(10) var<uniform> shore: Shore;

// The frame's FFT ocean (ADR-1969): two layers per band in one array, layer 0 (Dx, h, Dz, Dxz) and
// layer 1 (hx, hz, Dxx, Dzz); a 1 x 1 stand-in when no ocean is drawn.
struct Ocean {
    // x bands, y crest foam strength, z foam onset (1 - J), w 1 = the array holds a lingering-foam
    // history layer per band after the maps (ADR-2843)
    header: vec4<f32>,
    // x depth that carries the whole ocean, y depth that carries none, z 1 = rescale the whitecaps
    // by the deviation the faded bands lost (ADR-2843)
    shore: vec4<f32>,
    // per band: (grid center u, v in tiles, 1 / tile, texels), (fade wave number, mean square slope, 0, 0)
    band: array<vec4<f32>, 6>,
}

@group(0) @binding(11) var ocean_tex: texture_2d_array<f32>;
@group(0) @binding(12) var<uniform> ocean: Ocean;

const PI: f32 = 3.14159265;
const TAU: f32 = 6.2831853;
const GRAVITY: f32 = 9.81;
const WATER_IOR: f32 = 1.333;
const WATER_F0: f32 = 0.020373;
const ICE_F0: f32 = 0.0175;
const SLOPE_RANGE: f32 = 1.0;
const SLOPE_SQ_RANGE: f32 = 0.5;
// tan(asin(1/3)): the Kelvin wake half-angle.
const KELVIN_TAN: f32 = 0.35355;
const PLANE_HALF_WIDTH: f32 = 1.0e8;
const MAX_SWELL_WAVES: u32 = 32u;
const FOAM_RAMP: f32 = 0.3;
const SHORE_DEEP: f32 = 1.0e30;
const BREAKING_INDEX: f32 = 0.78;
const SHOAL_MAX: f32 = 2.0;
const DEEP_KH: f32 = 6.2831853;
const MIN_KH: f32 = 0.0001;
const BREAKER_ONSET: f32 = 0.7;

struct VsIn {
    @location(0) position: vec3<f32>,
    @location(1) body: u32,
    @location(2) flow: vec2<f32>,
    @location(3) right: vec2<f32>,
    @location(4) lateral: f32,
    @location(5) half_width: f32,
    @location(6) ripple: vec2<f32>,
}

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) @interpolate(flat) body: u32,
    @location(2) flow: vec2<f32>,
    @location(3) right: vec2<f32>,
    @location(4) lateral: f32,
    @location(5) half_width: f32,
    @location(6) ripple: vec2<f32>,
    // Swell planes: the rest point, meters from the ring grid's center.
    @location(7) rest: vec2<f32>,
}

// Displacement a wave of wave number `k` keeps on cells of `cell` meters: 1 from four vertices per
// wavelength, 0 at two (CPU: `water::vertex_wave_fade`).
fn wave_fade(cell: f32, k: f32) -> f32 {
    return clamp(2.0 - 2.0 * cell * k / PI, 0.0, 1.0);
}

// The cell the grid fades waves by at offset `local` from its center (CPU: `ring_fade_cell`).
fn ring_cell(local: vec2<f32>) -> f32 {
    let d = max(abs(local.x), abs(local.y));
    return swell.grid.z * max(2.0 * d / max(swell.grid.w, 1.0e-6), 1.0);
}

// The shore field of body slot `b`, or -1 (CPU: the body's `WaterShore`, tier and switch).
fn shore_field(b: u32) -> i32 {
    return i32(shore.map[b / 4u][b % 4u]) - 1;
}

// Depth (x) and shoreline distance (y) of field `f` at world `p` (x, z), bilinear between texel
// centers; z is 1 inside the field. Outside or without a field: deep water (CPU:
// `ShoreField::sample`).
fn shore_sample(f: i32, p: vec2<f32>) -> vec3<f32> {
    if (f < 0) {
        return vec3<f32>(SHORE_DEEP, SHORE_DEEP, 0.0);
    }
    let a = shore.f[2u * u32(f)];
    let sz = shore.f[2u * u32(f) + 1u];
    let u = (p.x - a.x) / a.z;
    let v = (p.y - a.y) / a.z;
    if (!(u >= 0.0 && v >= 0.0 && u <= sz.x - 1.0 && v <= sz.y - 1.0)) {
        return vec3<f32>(SHORE_DEEP, SHORE_DEEP, 0.0);
    }
    let i0 = min(floor(u), sz.x - 2.0);
    let j0 = min(floor(v), sz.y - 2.0);
    let fu = u - i0;
    let fv = v - j0;
    let c = vec2<i32>(i32(i0), i32(j0));
    let t00 = textureLoad(shore_tex, c, f, 0).xy;
    let t10 = textureLoad(shore_tex, c + vec2<i32>(1, 0), f, 0).xy;
    let t01 = textureLoad(shore_tex, c + vec2<i32>(0, 1), f, 0).xy;
    let t11 = textureLoad(shore_tex, c + vec2<i32>(1, 1), f, 0).xy;
    let top = t00 + (t10 - t00) * fu;
    let bottom = t01 + (t11 - t01) * fu;
    return vec3<f32>(top + (bottom - top) * fv, 1.0);
}

// Linear theory's shoaling coefficient (CPU: `shore::shoal_coefficient`).
fn shoal_coefficient(k: f32, h: f32) -> f32 {
    let kh = max(k * h, MIN_KH);
    if (kh >= DEEP_KH) {
        return 1.0;
    }
    let two = 2.0 * kh;
    let n = 1.0 + two / sinh(two);
    return 1.0 / sqrt(tanh(kh) * n);
}

// A wave's amplitude scale at depth `h` (x) and its shoaled amplitude over the breaking cap (y)
// (CPU: `shore::shoal_scale`).
fn shoal_scale(k: f32, a: f32, h: f32) -> vec2<f32> {
    if (!(h > 0.0)) {
        return vec2<f32>(0.0, 1.0);
    }
    let ks = shoal_coefficient(k, h);
    let cap = 0.5 * BREAKING_INDEX * h;
    if (!(a > 0.0)) {
        return vec2<f32>(min(ks, SHOAL_MAX), 0.0);
    }
    return vec2<f32>(clamp(min(ks, cap / a), 0.0, SHOAL_MAX), ks * a / cap);
}

// Breaker foam (CPU: `shore::breaker_foam`).
fn breaker_foam(ratio: f32) -> f32 {
    return smoothstep(BREAKER_ONSET, 1.0, ratio);
}

// Shore foam over a band `band` meters out from the shoreline (CPU: `shore::shore_foam`).
fn shore_foam(distance: f32, band: f32) -> f32 {
    if (!(distance >= 0.0) || !(band > 0.0)) {
        return 0.0;
    }
    return 1.0 - smoothstep(0.0, band, distance);
}

// Displacement of the rest point `l` (meters from the grid center) by body `b`'s waves, each faded
// on cells of `cell` meters and shoaled at depth `h` (CPU: `SwellInstant::grid_offset_local_on`).
fn swell_offset(b: u32, l: vec2<f32>, cell: f32, h: f32) -> vec3<f32> {
    let first = u32(swell.b[b].header.x);
    let count = u32(swell.b[b].header.y);
    var o = vec3<f32>(0.0);
    for (var i = 0u; i < MAX_SWELL_WAVES; i = i + 1u) {
        if (i >= count) {
            break;
        }
        let a = swell.w[min(2u * (first + i), 254u)];
        let p = swell.w[min(2u * (first + i) + 1u, 255u)];
        let k = max(length(a.xy), 1.0e-6);
        let fade = wave_fade(cell, k) * shoal_scale(k, a.z, h).x;
        let th = a.x * l.x + a.y * l.y + p.x;
        let c = cos(th);
        let s = sin(th);
        o = o + vec3<f32>(fade * a.w * (a.x / k) * c, fade * a.z * s, fade * a.w * (a.y / k) * c);
    }
    return o;
}

// The swell's slope (dh/dx, dh/dz), the horizontal Jacobian (z) and the largest breaking ratio
// (w) at the rest point `l` over depth `h`, waves faded on `footprint` meters per pixel (CPU:
// `SwellInstant::sample_on`, unfaded).
fn swell_frame(b: u32, l: vec2<f32>, footprint: f32, h: f32) -> vec4<f32> {
    let first = u32(swell.b[b].header.x);
    let count = u32(swell.b[b].header.y);
    var dpdx = vec3<f32>(1.0, 0.0, 0.0);
    var dpdz = vec3<f32>(0.0, 0.0, 1.0);
    var ratio = 0.0;
    for (var i = 0u; i < MAX_SWELL_WAVES; i = i + 1u) {
        if (i >= count) {
            break;
        }
        let a = swell.w[min(2u * (first + i), 254u)];
        let p = swell.w[min(2u * (first + i) + 1u, 255u)];
        let k = max(length(a.xy), 1.0e-6);
        let shoal = shoal_scale(k, a.z, h);
        ratio = max(ratio, shoal.y);
        let fade = wave_fade(footprint, k) * shoal.x;
        let dir = a.xy / k;
        let th = a.x * l.x + a.y * l.y + p.x;
        let c = cos(th);
        let s = sin(th);
        dpdx = dpdx + fade * vec3<f32>(-a.w * dir.x * a.x * s, a.z * a.x * c, -a.w * dir.y * a.x * s);
        dpdz = dpdz + fade * vec3<f32>(-a.w * dir.x * a.y * s, a.z * a.y * c, -a.w * dir.y * a.y * s);
    }
    let n = normalize(cross(dpdz, dpdx));
    let ny = max(n.y, 0.05);
    return vec4<f32>(-n.x / ny, -n.z / ny, dpdx.x * dpdz.z - dpdx.z * dpdz.x, ratio);
}

// The ocean's share at depth `h` for body `b` (CPU: `FftOcean::shore_weight`); 0 off the ocean.
fn ocean_weight(b: u32, h: f32) -> f32 {
    if (swell.b[b].level.z < 0.5 || !(h > 0.0)) {
        return 0.0;
    }
    let span = max(ocean.shore.x - ocean.shore.y, 1.0e-3);
    let x = clamp((h - ocean.shore.y) / span, 0.0, 1.0);
    return x * x * (3.0 - 2.0 * x);
}

// Band `band`'s value in `layer` at the rest point `l` (meters from the grid center), bilinear
// between texels, wrapping (CPU: `BilinearCell`).
fn ocean_load(band: u32, layer: i32, l: vec2<f32>) -> vec4<f32> {
    let a = ocean.band[2u * band];
    let uv = fract(a.xy + l * a.z) * a.w;
    let c = floor(uv);
    let f = uv - c;
    let mask = i32(a.w) - 1;
    let i0 = i32(c.x) & mask;
    let j0 = i32(c.y) & mask;
    let i1 = (i0 + 1) & mask;
    let j1 = (j0 + 1) & mask;
    let page = 2 * i32(band) + layer;
    let t00 = textureLoad(ocean_tex, vec2<i32>(i0, j0), page, 0);
    let t10 = textureLoad(ocean_tex, vec2<i32>(i1, j0), page, 0);
    let t01 = textureLoad(ocean_tex, vec2<i32>(i0, j1), page, 0);
    let t11 = textureLoad(ocean_tex, vec2<i32>(i1, j1), page, 0);
    let top = t00 + (t10 - t00) * f.x;
    let bottom = t01 + (t11 - t01) * f.x;
    return top + (bottom - top) * f.y;
}

// Displacement of the rest point `l` by the ocean's bands, each faded on cells of `cell` meters
// (CPU: `OceanFrame::grid_offset`).
fn ocean_offset(l: vec2<f32>, cell: f32) -> vec3<f32> {
    var o = vec3<f32>(0.0);
    let bands = u32(ocean.header.x);
    for (var b = 0u; b < 3u; b = b + 1u) {
        if (b >= bands) {
            break;
        }
        let fade = wave_fade(cell, ocean.band[2u * b + 1u].x);
        if (fade > 0.0) {
            o = o + fade * ocean_load(b, 0, l).xyz;
        }
    }
    return o;
}

// The ocean's slope (dh/dx, dh/dz), its Jacobian (z) and the mean square slope the faded bands
// lost (w) at the rest point `l`, bands faded on `footprint` meters per pixel (CPU:
// `OceanFrame::sample`, unfaded).
fn ocean_frame(l: vec2<f32>, footprint: f32) -> vec4<f32> {
    var dpdx = vec3<f32>(1.0, 0.0, 0.0);
    var dpdz = vec3<f32>(0.0, 0.0, 1.0);
    var lost = 0.0;
    let bands = u32(ocean.header.x);
    for (var b = 0u; b < 3u; b = b + 1u) {
        if (b >= bands) {
            break;
        }
        let m = ocean.band[2u * b + 1u];
        let fade = wave_fade(footprint, m.x);
        lost = lost + (1.0 - fade * fade) * m.y;
        if (fade > 0.0) {
            let t0 = ocean_load(b, 0, l);
            let t1 = ocean_load(b, 1, l);
            dpdx = dpdx + fade * vec3<f32>(t1.z, t1.x, t0.w);
            dpdz = dpdz + fade * vec3<f32>(t0.w, t1.y, t1.w);
        }
    }
    let n = normalize(cross(dpdz, dpdx));
    let ny = max(n.y, 0.05);
    return vec4<f32>(-n.x / ny, -n.z / ny, dpdx.x * dpdz.z - dpdx.z * dpdz.x, lost);
}

// Crest foam coverage from the Jacobian (CPU: `swell::crest_foam`).
fn crest_foam(j: f32, strength: f32, onset: f32) -> f32 {
    return clamp(strength * smoothstep(onset, onset + FOAM_RAMP, 1.0 - j), 0.0, 1.0);
}

// The most the whitecap rescale multiplies the drawn compression by (CPU:
// `ocean_foam::WHITECAP_RESCALE_MAX`).
const WHITECAP_RESCALE_MAX: f32 = 4.0;

// Band `band`'s lingering-foam history at the rest point `l` (ADR-2843): layer `2 bands + band`,
// bilinear and wrapping as `ocean_load` reads the maps (CPU: `FoamHistory::sample`).
fn ocean_history(band: u32, bands: u32, l: vec2<f32>) -> f32 {
    let a = ocean.band[2u * band];
    let uv = fract(a.xy + l * a.z) * a.w;
    let c = floor(uv);
    let f = uv - c;
    let mask = i32(a.w) - 1;
    let i0 = i32(c.x) & mask;
    let j0 = i32(c.y) & mask;
    let i1 = (i0 + 1) & mask;
    let j1 = (j0 + 1) & mask;
    let page = 2 * i32(bands) + i32(band);
    let t00 = textureLoad(ocean_tex, vec2<i32>(i0, j0), page, 0).x;
    let t10 = textureLoad(ocean_tex, vec2<i32>(i1, j0), page, 0).x;
    let t01 = textureLoad(ocean_tex, vec2<i32>(i0, j1), page, 0).x;
    let t11 = textureLoad(ocean_tex, vec2<i32>(i1, j1), page, 0).x;
    let top = t00 + (t10 - t00) * f.x;
    let bottom = t01 + (t11 - t01) * f.x;
    return top + (bottom - top) * f.y;
}

// The ocean's crest foam with the ADR-2843 options on (CPU: `OceanFrame::drawn_foam`): the drawn
// Jacobian `fr.z` rescaled by the deviation the faded bands lost (`fr.w`) when `shore.z` is on,
// and the larger of that and every band's lingering foam, faded on `footprint`, when `header.w`
// is on. The default frame never calls this.
fn ocean_foam_extra(fr: vec4<f32>, l: vec2<f32>, footprint: f32) -> f32 {
    var total = 0.0;
    var lingering = 0.0;
    let bands = u32(ocean.header.x);
    for (var b = 0u; b < 3u; b = b + 1u) {
        if (b >= bands) {
            break;
        }
        let m = ocean.band[2u * b + 1u];
        total = total + m.y;
        let fade = wave_fade(footprint, m.x);
        if (ocean.header.w > 0.5 && fade > 0.0) {
            lingering = max(lingering, fade * ocean_history(b, bands, l));
        }
    }
    var j = fr.z;
    if (ocean.shore.z > 0.5 && total > 0.0) {
        let drawn = max(total - fr.w, 0.0);
        var r = WHITECAP_RESCALE_MAX;
        if (drawn > 0.0) {
            r = min(sqrt(total / drawn), WHITECAP_RESCALE_MAX);
        }
        j = 1.0 - r * (1.0 - j);
    }
    let instant = crest_foam(j, ocean.header.y, ocean.header.z);
    return max(instant, clamp(ocean.header.y * lingering, 0.0, 1.0));
}

// Shift of the ring boundary `j` (the boundary at the inner half size times 2^j).
fn ring_node(j: u32) -> vec2<f32> {
    let v = swell.shifts[j / 2u];
    return select(v.xy, v.zw, (j % 2u) == 1u);
}

// How far the per-ring snap moves the ring-grid vertex at `local` (CPU: `RingGrid::ring_shift`):
// zero inside the inner square, then linear in the Chebyshev distance between the shifts of the
// ring's two boundaries. The ring count is not in the uniform: a boundary past the last ring holds
// zero shifts, so the loop's end is the grid's edge.
fn ring_shift(local: vec2<f32>) -> vec2<f32> {
    let rho = max(abs(local.x), abs(local.y));
    let inner = swell.grid.w;
    if (rho <= inner) {
        return vec2<f32>(0.0);
    }
    var upper = inner;
    var k = 1u;
    for (var i = 1u; i < 19u; i = i + 1u) {
        if (rho > upper * 2.0) {
            upper = upper * 2.0;
            k = k + 1u;
        }
    }
    let t = clamp((rho - upper) / upper, 0.0, 1.0);
    return ring_node(k - 1u) * (1.0 - t) + ring_node(k) * t;
}

// The rest point (meters from the grid center) of the ring-grid vertex at `local`: the grid
// center plus the offset, clamped to body `b`'s rest rectangle.
fn ring_rest(b: u32, local: vec2<f32>) -> vec2<f32> {
    let o = swell.grid.xy;
    let r = swell.b[b].rect;
    return clamp(o + local, r.xy - r.zw, r.xy + r.zw) - o;
}

// World position of the ring-grid vertex at `local` for swell body `b` (CPU: `gpu::ring_vertex`).
fn ring_world(b: u32, local: vec2<f32>) -> vec3<f32> {
    let o = swell.grid.xy;
    let l = ring_rest(b, local + ring_shift(local));
    let h = shore_sample(shore_field(b), o + l).x;
    let w = ocean_weight(b, h);
    let cell = ring_cell(local);
    var d = vec3<f32>(0.0);
    if (w < 1.0) {
        d = (1.0 - w) * swell_offset(b, l, cell, h);
    }
    if (w > 0.0) {
        d = d + w * ocean_offset(l, cell);
    }
    return vec3<f32>(o.x + l.x + d.x, swell.b[b].level.x + d.y, o.y + l.y + d.z);
}

@vertex
fn vs_main(v: VsIn) -> VsOut {
    let b = min(v.body, 15u);
    if (swell.b[b].level.y > 0.5) {
        // A swell plane on the ring grid: the position holds the offset from the grid's center.
        let l = ring_rest(b, v.position.xz + ring_shift(v.position.xz));
        let world = ring_world(b, v.position.xz);
        let rest = swell.grid.xy + l;
        return VsOut(
            frame.view_proj * vec4<f32>(world, 1.0),
            world,
            v.body,
            v.flow,
            v.right,
            0.0,
            v.half_width,
            vec2<f32>(-rest.y, rest.x),
            l,
        );
    }
    // Built by value: a struct-typed function local breaks SPIR-V 1.6 (ADR-026).
    return VsOut(
        frame.view_proj * vec4<f32>(v.position, 1.0),
        v.position,
        v.body,
        v.flow,
        v.right,
        v.lateral,
        v.half_width,
        v.ripple,
        vec2<f32>(0.0),
    );
}

fn linear_depth(d: f32) -> f32 {
    let n = frame.depth.x;
    let f = frame.depth.y;
    return n * f / max(f - d * (f - n), 1.0e-6);
}

fn world_at(uv: vec2<f32>, d: f32) -> vec3<f32> {
    let ndc = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, d, 1.0);
    let w = frame.inv_view_proj * ndc;
    return w.xyz / w.w;
}

// Screen uv (xy) and NDC depth (z) of a world point; w = 0 behind the eye.
fn project(p: vec3<f32>) -> vec4<f32> {
    let c = frame.view_proj * vec4<f32>(p, 1.0);
    if (c.w <= 1.0e-5) {
        return vec4<f32>(0.0);
    }
    let ndc = c.xyz / c.w;
    return vec4<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5, ndc.z, 1.0);
}

fn on_screen(uv: vec2<f32>) -> bool {
    return all(uv >= vec2<f32>(0.0)) && all(uv <= vec2<f32>(1.0));
}

fn texel_of(uv: vec2<f32>) -> vec2<i32> {
    let size = vec2<i32>(frame.screen.xy);
    return clamp(vec2<i32>(uv * frame.screen.xy), vec2<i32>(0), size - vec2<i32>(1));
}

fn fresnel(cos_theta: f32, f0: f32) -> f32 {
    let m = 1.0 - clamp(cos_theta, 0.0, 1.0);
    let m2 = m * m;
    return f0 + (1.0 - f0) * m2 * m2 * m;
}

// Analytic sky: horizon to zenith by elevation.
fn sky(dir: vec3<f32>) -> vec3<f32> {
    let t = pow(clamp(dir.y, 0.0, 1.0), 0.45);
    return mix(frame.sky_horizon.rgb, frame.sky_zenith.rgb, t);
}

// Height of the standing-wave field of one obstacle at offset `d` from its center.
fn standing(d: vec2<f32>, radius: f32, fdir: vec2<f32>, k: f32, lambda: f32) -> f32 {
    let side = vec2<f32>(-fdir.y, fdir.x);
    let u = dot(d, fdir);
    let w = dot(d, side);
    let r = length(d);
    let rr = max(r - radius, 0.0);
    let up = clamp(-u / max(r, 1.0e-4), 0.0, 1.0);
    // Pile-up rings, strongest upstream.
    let bow = cos(k * rr) * exp(-rr / (radius + 0.6 * lambda)) * (0.3 + 0.7 * up);
    // Kelvin wake arms downstream.
    let down = max(u, 0.0);
    let arm = abs(w) - (radius + down * KELVIN_TAN);
    let spread = 0.02 + 0.015 * down + 0.1 * radius * radius;
    let wake = cos(k * down) * exp(-arm * arm / spread) * exp(-down / (6.0 * lambda + 4.0 * radius))
        * smoothstep(0.0, radius + 0.1, u);
    return bow + 0.8 * wake;
}

// Whitewater around one obstacle, 0..1.
fn whitewater(d: vec2<f32>, radius: f32, fdir: vec2<f32>) -> f32 {
    let side = vec2<f32>(-fdir.y, fdir.x);
    let u = dot(d, fdir);
    let w = dot(d, side);
    let r = length(d);
    let rr = max(r - radius, 0.0);
    let up = clamp(-u / max(r, 1.0e-4), 0.0, 1.0);
    let cushion = (1.0 - smoothstep(0.0, 0.08 + 0.2 * radius, rr)) * (0.25 + 0.75 * up);
    let down = max(u, 0.0);
    let arm = abs(w) - (radius + down * KELVIN_TAN);
    let trail = exp(-arm * arm / 0.004) * exp(-down / (1.2 * radius + 0.3)) * step(0.0, u);
    return clamp(cushion + 0.5 * trail, 0.0, 1.0);
}

struct Ripple {
    slope: vec2<f32>,
    variance: f32,
    height: f32,
}

fn decode_ripple(t: vec4<f32>) -> Ripple {
    let slope = (t.rg * 2.0 - 1.0) * SLOPE_RANGE;
    return Ripple(slope, max(t.b * SLOPE_SQ_RANGE - dot(slope, slope), 0.0), t.a);
}

struct Hit {
    uv: vec2<f32>,
    weight: f32,
}

// Screen-space reflection: an exponential march along `dir` from `origin` against the scene depth,
// then a bisection between the last two steps. Explicit loads only (FXC-safe loops).
fn trace_reflection(origin: vec3<f32>, dir: vec3<f32>, dist: f32) -> Hit {
    let miss = Hit(vec2<f32>(0.0), 0.0);
    let steps = u32(frame.mode.y);
    if (steps == 0u) {
        return miss;
    }
    var t = 0.03 + 0.004 * dist;
    let t_max = clamp(dist * 3.0 + 20.0, 20.0, 600.0);
    let growth = pow(t_max / t, 1.0 / f32(steps));
    var prev = t;
    for (var i = 0u; i < 64u; i = i + 1u) {
        if (i >= steps) {
            break;
        }
        prev = t;
        t = t * growth;
        let q = project(origin + dir * t);
        if (q.w == 0.0 || !on_screen(q.xy) || q.z > 1.0) {
            break;
        }
        let sd = textureLoad(depth_tex, texel_of(q.xy), 0);
        if (sd >= 1.0) {
            continue;
        }
        let lr = linear_depth(q.z);
        let ls = linear_depth(sd);
        let thickness = max(0.25, (t - prev) * 1.5) + 0.02 * ls;
        if (lr > ls && lr - ls < thickness) {
            var a = prev;
            var b = t;
            let refine = u32(frame.misc.x);
            for (var j = 0u; j < 8u; j = j + 1u) {
                if (j >= refine) {
                    break;
                }
                let m = 0.5 * (a + b);
                let qm = project(origin + dir * m);
                let sm = textureLoad(depth_tex, texel_of(qm.xy), 0);
                if (qm.w != 0.0 && sm < 1.0 && linear_depth(qm.z) > linear_depth(sm)) {
                    b = m;
                } else {
                    a = m;
                }
            }
            let qh = project(origin + dir * b);
            let edge = min(min(qh.x, 1.0 - qh.x), min(qh.y, 1.0 - qh.y));
            return Hit(qh.xy, smoothstep(0.0, 0.06, edge) * (1.0 - smoothstep(0.7, 1.0, t / t_max)));
        }
    }
    return miss;
}

// The sky seen in the frame itself in direction `dir`, when that direction is on screen and the
// frame shows sky there. Returns the uv and 1, or 0.
fn sky_on_screen(dir: vec3<f32>) -> Hit {
    let miss = Hit(vec2<f32>(0.0), 0.0);
    let q = project(frame.eye.xyz + dir * (frame.depth.y * 0.5));
    if (q.w == 0.0 || !on_screen(q.xy)) {
        return miss;
    }
    let sd = textureLoad(depth_tex, texel_of(q.xy), 0);
    if (sd < 1.0) {
        return miss;
    }
    let edge = min(min(q.x, 1.0 - q.x), min(q.y, 1.0 - q.y));
    return Hit(q.xy, smoothstep(0.0, 0.05, edge));
}

// Screen-space sun visibility: march toward the sun against the scene depth (bank and rock
// shadows on the water). 1 = lit.
fn sun_visibility(origin: vec3<f32>, to_sun: vec3<f32>) -> f32 {
    let steps = u32(frame.misc.y);
    if (steps == 0u || frame.sun_dir.w <= 0.0) {
        return 1.0;
    }
    var t = 0.08;
    let growth = pow(30.0 / t, 1.0 / f32(steps));
    for (var i = 0u; i < 32u; i = i + 1u) {
        if (i >= steps) {
            break;
        }
        t = t * growth;
        let q = project(origin + to_sun * t);
        if (q.w == 0.0 || !on_screen(q.xy)) {
            break;
        }
        let sd = textureLoad(depth_tex, texel_of(q.xy), 0);
        if (sd >= 1.0) {
            continue;
        }
        let lr = linear_depth(q.z);
        let ls = linear_depth(sd);
        if (lr > ls + 0.05 && lr - ls < 3.0 + 0.05 * t) {
            return 0.0;
        }
    }
    return 1.0;
}

// Sunlight focused by the ripples on the bed: the half-height contours of the advected ripple map
// (two flow-map layers) drawn as thin bright lines, strongest at a few decimeters of depth and none
// on calm water (`sun` carries the ripple strength), faded
// out where the map's mips would blur the lines into a uniform glow. Energy is roughly kept (the
// lines brighten what the mean darkens).
fn caustic_gain(bed_xz: vec2<f32>, flow: vec2<f32>, tile: f32, dist: f32, depth: f32, to_sun: vec3<f32>, sun_vis: f32, sun_e: f32, amb_e: f32) -> f32 {
    let ctile = tile * 0.8;
    let footprint = dist * 2.0 * frame.depth.z * frame.screen.w;
    let lod = log2(max(footprint / (ctile / 256.0), 1.0));
    let fade = (1.0 - smoothstep(1.0, 3.0, lod)) * smoothstep(0.02, 0.15, depth) * (1.0 - smoothstep(1.5, 4.0, depth));
    let sun = sun_vis * clamp(to_sun.y * 2.0, 0.0, 1.0) * sun_e / max(sun_e + amb_e, 1.0e-4);
    if (fade * sun <= 0.0) {
        return 1.0;
    }
    let cuv = bed_xz / ctile;
    let ha = textureSampleLevel(ripple_tex, ripple_samp, cuv - flow * (frame.flow.x * frame.flow.w) / ctile + frame.scroll.xy, lod).a;
    let hb = textureSampleLevel(ripple_tex, ripple_samp, cuv - flow * (frame.flow.y * frame.flow.w) / ctile + frame.scroll.zw + vec2<f32>(0.37, 0.61), lod).a;
    let h = mix(hb, ha, frame.flow.z);
    let line = pow(max(1.0 - abs(2.0 * h - 1.0), 0.0), 16.0);
    return max(1.0 + 0.75 * (line - 0.08) * fade * sun, 0.0);
}

// ---- Caustics and underwater sun shafts (ADR-2432) ----
//
// The tile pass `cs_caustic_tile` turns the ripple map into, per texel, the Jacobian terms of the
// sun's refraction map: (tr J, det J, r - r0). A receiver at depth d below the surface is lit
// 1 / |1 + d tr + d^2 det| times the flat surface's light: the inverse area of the refracted
// beam's footprint (flux through the surface is conserved). The CPU mirror and the reference
// (a count of refracted rays) are `caustics.rs`.

// Sampled by the fragment stage and the shaft pass; filled by the tile pass.
@group(1) @binding(0) var caustic_tile: texture_2d<f32>;
// Half-resolution shaft radiance, read by the underwater fog.
@group(1) @binding(1) var shaft_tex: texture_2d<f32>;

struct TileParams {
    // xyz toward the sun, w 1 when the sun is above the horizon
    sun: vec4<f32>,
    // scroll of layer a (xy) and b (zw), in tiles
    scroll: vec4<f32>,
}

@group(2) @binding(0) var<uniform> tile_params: TileParams;
@group(2) @binding(1) var tile_out: texture_storage_2d<rgba16float, write>;
@group(2) @binding(2) var ripple_c: texture_2d<f32>;
@group(2) @binding(3) var shaft_out: texture_storage_2d<rgba16float, write>;

// The shaft passes' parameters (ADR-3251), shared by the march, the resolve and the draw.
struct ShaftParams {
    // World to the sun shadow map's clip space (an orthographic cascade).
    light_vp: mat4x4<f32>,
    // The previous frame's view-projection, for the resolve's reprojection.
    prev_view_proj: mat4x4<f32>,
    // x 1 = a sun shadow map is bound, y depth bias, z 1 = fade the shadow out at the map's edge
    shadow: vec4<f32>,
    // x per-frame jitter offset (0: the fixed pattern), y 1 = the eye is above the surface,
    // z the surface level seen from above, w that body's slot
    march: vec4<f32>,
    // x 1 = the history is valid, y the most frames it averages
    history: vec4<f32>,
}

@group(2) @binding(4) var<uniform> shaft: ShaftParams;
// The frame's sun shadow (a cascade's depth, read as float), or a 1 x 1 stand-in at the far plane.
@group(2) @binding(5) var sun_shadow: texture_2d<f32>;
// The resolve: this frame's raw march, the accumulated history and the new accumulation.
@group(2) @binding(6) var shaft_raw: texture_2d<f32>;
@group(2) @binding(7) var shaft_hist: texture_2d<f32>;
@group(2) @binding(8) var shaft_hist_out: texture_storage_2d<rgba16float, write>;
// The draw's copy of the parameters (it reads `march` for the view from above).
@group(1) @binding(2) var<uniform> shaft_draw: ShaftParams;

const CAUSTIC_TILE: f32 = 256.0;
const CAUSTIC_MAX_DENSITY: f32 = 8.0;
const CAUSTIC_LAYER_WEIGHT: f32 = 0.70710678;
const CAUSTIC_CHROMA: f32 = 0.15;

// One ripple texel's slope, wrapped (the CPU `sample_slope`'s `at`).
fn ripple_slope_texel(i: vec2<i32>, size: vec2<i32>) -> vec2<f32> {
    let t = textureLoad(ripple_c, ((i % size) + size) % size, 0).rg;
    return (t * 2.0 - vec2<f32>(1.0)) * SLOPE_RANGE;
}

// The ripple map's slope at uv, bilinear with f32 weights (the CPU `sample_slope`). Filtered by
// hand, not by the sampler: the tile differentiates the slope over two texels (x64 per tile), and
// a sampler that filters 8-bit texels at 8-bit precision (software rasterizers, many mobile GPUs)
// turns its rounding into noise of a few tenths in the Jacobian.
fn ripple_slope(uv: vec2<f32>) -> vec2<f32> {
    let size = vec2<i32>(textureDimensions(ripple_c, 0));
    let f = uv * vec2<f32>(size) - vec2<f32>(0.5);
    let cell = floor(f);
    let t = f - cell;
    let i = vec2<i32>(cell);
    let a = ripple_slope_texel(i, size);
    let b = ripple_slope_texel(i + vec2<i32>(1, 0), size);
    let c = ripple_slope_texel(i + vec2<i32>(0, 1), size);
    let d = ripple_slope_texel(i + vec2<i32>(1, 1), size);
    let top = a + (b - a) * t.x;
    let bottom = c + (d - c) * t.x;
    return top + (bottom - top) * t.y;
}

// The tile's two-layer slope at tile coordinates uv (the CPU `tile_slope`).
fn tile_slope(uv: vec2<f32>) -> vec2<f32> {
    let a = ripple_slope(uv + tile_params.scroll.xy);
    let b = ripple_slope(uv + tile_params.scroll.zw + vec2<f32>(0.37, 0.61));
    return CAUSTIC_LAYER_WEIGHT * (a + b);
}

// Horizontal displacement per metre of depth of the sun's ray refracted at a surface of this slope.
fn sun_displacement(slope: vec2<f32>, to_sun: vec3<f32>) -> vec2<f32> {
    let n = normalize(vec3<f32>(-slope.x, 1.0, -slope.y));
    let t = refract(-to_sun, n, 1.0 / WATER_IOR);
    return t.xz / max(-t.y, 0.05);
}

@compute @workgroup_size(8, 8, 1)
fn cs_caustic_tile(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= 256u || id.y >= 256u) {
        return;
    }
    let sun = tile_params.sun.xyz;
    let uv = (vec2<f32>(id.xy) + vec2<f32>(0.5)) / CAUSTIC_TILE;
    let h = 2.0 / CAUSTIC_TILE;
    let r0 = sun_displacement(vec2<f32>(0.0), sun);
    let here = sun_displacement(tile_slope(uv), sun);
    let xp = sun_displacement(tile_slope(uv + vec2<f32>(h, 0.0)), sun);
    let xm = sun_displacement(tile_slope(uv - vec2<f32>(h, 0.0)), sun);
    let zp = sun_displacement(tile_slope(uv + vec2<f32>(0.0, h)), sun);
    let zm = sun_displacement(tile_slope(uv - vec2<f32>(0.0, h)), sun);
    let k = 0.5 / h;
    let jxx = (xp.x - xm.x) * k;
    let jzx = (xp.y - xm.y) * k;
    let jxz = (zp.x - zm.x) * k;
    let jzz = (zp.y - zm.y) * k;
    textureStore(tile_out, vec2<i32>(id.xy), vec4<f32>(jxx + jzz, jxx * jzz - jxz * jzx, here - r0));
}

// Relative irradiance at depth d (m) for a body's pattern scale and ripple strength: one
// fixed-point step finds the surface point whose ray lands on xz.
fn caustic_density_at(xz: vec2<f32>, d: f32, ctile: f32, ns: f32, r0: vec2<f32>, shift: vec2<f32>) -> f32 {
    let uv0 = (xz - d * r0) / ctile + shift;
    let c0 = textureSampleLevel(caustic_tile, ripple_samp, uv0, 0.0);
    let uv = uv0 - d * ns * c0.zw / ctile;
    let c = textureSampleLevel(caustic_tile, ripple_samp, uv, 0.0);
    let a = d * ns / ctile;
    let m = 1.0 + a * c.x + a * a * c.y;
    let floor_ = 1.0 / CAUSTIC_MAX_DENSITY;
    return sqrt(1.0 + floor_ * floor_) / sqrt(m * m + floor_ * floor_);
}

fn caustic_depth_fade(depth: f32, sigma: f32, sun_y: f32) -> f32 {
    return smoothstep(0.02, 0.15, depth)
        * exp(-max(sigma, 0.0) * depth / max(sun_y, 0.25))
        * (1.0 - smoothstep(2.0, 6.0, depth));
}

// The gain on the sun's light at a receiver p below the surface at height surface_y, per colour
// channel (the channels focus at slightly different depths when the body asks for dispersion).
// `weight` is the sun's share of the light at p (shadow and sun height included), `shift` the
// flow's advection of the pattern in tiles.
fn caustic_receiver(p: vec3<f32>, surface_y: f32, shift: vec2<f32>, body: Body, slot: u32, dist: f32, weight: f32) -> vec3<f32> {
    let depth = surface_y - p.y;
    let ctile = max(body.surface.y, 0.1) * max(body.caustic.y, 0.1);
    let to_sun = frame.sun_dir.xyz;
    let footprint = dist * 2.0 * frame.depth.z * frame.screen.w;
    let lod = log2(max(footprint / (ctile / CAUSTIC_TILE), 1.0));
    let sigma = body.absorption.g + body.absorption.w;
    var fade = caustic_depth_fade(depth, sigma, to_sun.y) * (1.0 - smoothstep(1.5, 3.5, lod)) * weight;
    // Masked by the shore depth field where the body has one: no caustics on the dry edge.
    let field = shore_field(slot);
    if (field >= 0) {
        let sd = shore_sample(field, p.xz);
        if (sd.z > 0.5) {
            fade = fade * smoothstep(0.02, 0.15, sd.x);
        }
    }
    if (fade <= 0.0 || body.caustic.x <= 0.0) {
        return vec3<f32>(1.0);
    }
    let ns = body.surface.x;
    let r0 = sun_displacement(vec2<f32>(0.0), to_sun);
    let amount = clamp(body.caustic.x * fade, 0.0, 4.0);
    let spread = body.caustic.z * CAUSTIC_CHROMA;
    let dr = caustic_density_at(p.xz, depth * (1.0 - spread), ctile, ns, r0, shift);
    let dg = caustic_density_at(p.xz, depth, ctile, ns, r0, shift);
    let db = caustic_density_at(p.xz, depth * (1.0 + spread), ctile, ns, r0, shift);
    return vec3<f32>(1.0) + (vec3<f32>(dr, dg, db) - vec3<f32>(1.0)) * amount;
}

// Henyey-Greenstein phase function.
fn shaft_phase(cos_theta: f32) -> f32 {
    let g = 0.6;
    return (1.0 - g * g) / (4.0 * PI * pow(max(1.0 + g * g - 2.0 * g * cos_theta, 1.0e-3), 1.5));
}

// The sun shadow map's depth test at p (1 lit, 0 shadowed): outside the map lit, faded to lit
// over the outer 15 % of the square like the volumetric fog's cascade read.
fn shadow_lit(p: vec3<f32>) -> f32 {
    let clip = shaft.light_vp * vec4<f32>(p, 1.0);
    let ndc = clip.xyz / max(clip.w, 1.0e-5);
    let suv = vec2<f32>(ndc.x * 0.5 + 0.5, 1.0 - (ndc.y * 0.5 + 0.5));
    if (suv.x < 0.0 || suv.x > 1.0 || suv.y < 0.0 || suv.y > 1.0 || ndc.z < 0.0 || ndc.z > 1.0) {
        return 1.0;
    }
    let dims = vec2<i32>(textureDimensions(sun_shadow, 0));
    let texel = min(vec2<i32>(suv * vec2<f32>(dims)), dims - vec2<i32>(1));
    let stored = textureLoad(sun_shadow, texel, 0).r;
    if (ndc.z > stored + shaft.shadow.y) {
        if (shaft.shadow.z > 0.5) {
            return smoothstep(0.85, 1.0, max(abs(ndc.x), abs(ndc.y)));
        }
        return 0.0;
    }
    return 1.0;
}

// Whether the sun reaches q, `dq` metres under the surface, through the occluders of the sun
// shadow map (ADR-3251). Above the surface the light travels along `to_sun`, so a pier, a hull or
// a cliff is tested where the refracted beam to q crossed the surface, exactly. Below it the map
// can only be read along `to_sun`: a caster that lies between q and the sun and below the surface
// (a cave roof, a hull's keel) shadows q; one above the surface along that straight line is the
// first test's business and is ignored here, so it does not shadow q twice.
fn shaft_sun_visibility(q: vec3<f32>, dq: f32, beam: vec3<f32>, to_sun: vec3<f32>) -> f32 {
    let s = q + beam * (dq / min(beam.y, -1.0e-3));
    let above = shadow_lit(s);
    if (above <= 0.0) {
        return 0.0;
    }
    let cq = shaft.light_vp * vec4<f32>(q, 1.0);
    let nq = cq.xyz / max(cq.w, 1.0e-5);
    let suv = vec2<f32>(nq.x * 0.5 + 0.5, 1.0 - (nq.y * 0.5 + 0.5));
    if (suv.x < 0.0 || suv.x > 1.0 || suv.y < 0.0 || suv.y > 1.0 || nq.z < 0.0 || nq.z > 1.0) {
        return above;
    }
    let dims = vec2<i32>(textureDimensions(sun_shadow, 0));
    let texel = min(vec2<i32>(suv * vec2<f32>(dims)), dims - vec2<i32>(1));
    let stored = textureLoad(sun_shadow, texel, 0).r;
    let c = q + to_sun * (dq / max(to_sun.y, 1.0e-3));
    let cc = shaft.light_vp * vec4<f32>(c, 1.0);
    let crossing = cc.z / max(cc.w, 1.0e-5);
    if (nq.z > stored + shaft.shadow.y && stored > crossing + shaft.shadow.y) {
        return 0.0;
    }
    return above;
}

// Underwater sun shafts at half resolution (ADR-2432): a short march along the view ray in the
// water medium; each step adds the sun's scattering, lit by the same tile as the receivers (only
// the brightening above the ambient, so the medium keeps the base water column's light), dimmed
// by the water between the surface and the step and between the step and the eye. With a sun
// shadow bound the occluders cut the beam (ADR-3251); seen from above (ADR-3251, opt in) the march
// runs from where the view ray enters the water to what it sees below.
@compute @workgroup_size(8, 8, 1)
fn cs_caustic_shafts(@builtin(global_invocation_id) id: vec3<u32>) {
    let size = vec2<u32>(textureDimensions(shaft_out));
    if (id.x >= size.x || id.y >= size.y) {
        return;
    }
    if (shaft.march.y > 0.5) {
        textureStore(shaft_out, vec2<i32>(id.xy), vec4<f32>(shafts_from_above(id.xy, size), 0.0));
        return;
    }
    let slot = u32(frame.misc.w) - 2u;
    let body = bodies.b[min(slot, 15u)];
    let steps = u32(body.caustic.w);
    var rgb = vec3<f32>(0.0);
    if (frame.misc.w >= 1.5 && steps > 0u && body.caustic.x > 0.0) {
        let eye = frame.eye.xyz;
        let surface_y = frame.depth.w;
        let uv = (vec2<f32>(id.xy) + vec2<f32>(0.5)) / vec2<f32>(size);
        let d = textureLoad(depth_tex, texel_of(uv), 0);
        let p = world_at(uv, d);
        let to_p = p - eye;
        let scene_dist = min(length(to_p), 1.0e4);
        let dir = to_p / max(length(to_p), 1.0e-4);
        var surf_dist = 1.0e9;
        if (dir.y > 1.0e-4) {
            surf_dist = (surface_y - eye.y) / dir.y;
        }
        // Shafts fade out over a long path: the water is too dim to see them past that.
        let path = clamp(min(scene_dist, surf_dist), 0.0, 60.0);
        let sigma_t = body.absorption.rgb + vec3<f32>(body.absorption.w);
        let to_sun = frame.sun_dir.xyz;
        let beam = refract(-to_sun, vec3<f32>(0.0, 1.0, 0.0), 1.0 / WATER_IOR);
        let phase = shaft_phase(dot(beam, -dir));
        let ctile = max(body.surface.y, 0.1) * max(body.caustic.y, 0.1);
        let r0 = sun_displacement(vec2<f32>(0.0), to_sun);
        let ns = body.surface.x;
        let n = f32(steps);
        let jitter = shaft_jitter(id.xy);
        let dt = path / n;
        var acc = vec3<f32>(0.0);
        for (var i = 0u; i < 16u; i = i + 1u) {
            if (i >= steps) {
                break;
            }
            let t = (f32(i) + jitter) * dt;
            let q = eye + dir * t;
            let dq = surface_y - q.y;
            if (dq <= 0.0) {
                continue;
            }
            let dens = caustic_density_at(q.xz, dq, ctile, ns, r0, vec2<f32>(0.0));
            var lift = max(dens - 1.0, 0.0) * smoothstep(0.02, 0.3, dq) * (1.0 - smoothstep(4.0, 12.0, dq));
            if (shaft.shadow.x > 0.5 && lift > 0.0) {
                lift = lift * shaft_sun_visibility(q, dq, beam, to_sun);
            }
            acc = acc + lift * exp(-sigma_t * (dq / max(to_sun.y, 0.25) + t)) * dt;
        }
        rgb = body.scatter.rgb * frame.sun_color.rgb * (frame.sun_dir.w * body.absorption.w * phase * body.caustic.x) * acc;
    }
    textureStore(shaft_out, vec2<i32>(id.xy), vec4<f32>(rgb, 0.0));
}

// The march's jitter for texel `id`: the fixed interleaved-gradient pattern, shifted by the
// frame's offset when the history averages the frames (ADR-3251; offset 0 is the old pattern).
fn shaft_jitter(id: vec2<u32>) -> f32 {
    let base = fract(52.9829189 * fract(dot(vec2<f32>(id), vec2<f32>(0.06711056, 0.00583715))));
    if (shaft.march.x == 0.0) {
        return base;
    }
    return fract(base + shaft.march.x);
}

// Shafts seen from above (ADR-3251, opt in): the view ray enters the water at the flat level of
// the body in `shaft.march` and the march runs from there to what the pixel sees below, at most
// 60 m, lit and shadowed like the march under the surface. Pixels whose ray does not enter the
// water before it meets the scene get none.
fn shafts_from_above(id: vec2<u32>, size: vec2<u32>) -> vec3<f32> {
    let body = bodies.b[min(u32(shaft.march.w), 15u)];
    let steps = u32(body.caustic.w);
    if (steps == 0u || body.caustic.x <= 0.0) {
        return vec3<f32>(0.0);
    }
    let eye = frame.eye.xyz;
    let surface_y = shaft.march.z;
    let uv = (vec2<f32>(id) + vec2<f32>(0.5)) / vec2<f32>(size);
    let d = textureLoad(depth_tex, texel_of(uv), 0);
    let p = world_at(uv, d);
    let to_p = p - eye;
    let scene_dist = min(length(to_p), 1.0e4);
    let dir = to_p / max(length(to_p), 1.0e-4);
    if (dir.y > -1.0e-4 || eye.y <= surface_y) {
        return vec3<f32>(0.0);
    }
    let t0 = (eye.y - surface_y) / -dir.y;
    if (scene_dist <= t0) {
        return vec3<f32>(0.0);
    }
    let path = clamp(scene_dist - t0, 0.0, 60.0);
    let sigma_t = body.absorption.rgb + vec3<f32>(body.absorption.w);
    let to_sun = frame.sun_dir.xyz;
    let beam = refract(-to_sun, vec3<f32>(0.0, 1.0, 0.0), 1.0 / WATER_IOR);
    // The view ray bends at the surface too; the march follows the refracted ray.
    let wdir = refract(dir, vec3<f32>(0.0, 1.0, 0.0), 1.0 / WATER_IOR);
    let phase = shaft_phase(dot(beam, -wdir));
    let ctile = max(body.surface.y, 0.1) * max(body.caustic.y, 0.1);
    let r0 = sun_displacement(vec2<f32>(0.0), to_sun);
    let ns = body.surface.x;
    let dt = path / f32(steps);
    let jitter = shaft_jitter(id);
    let entry = eye + dir * t0;
    var acc = vec3<f32>(0.0);
    for (var i = 0u; i < 16u; i = i + 1u) {
        if (i >= steps) {
            break;
        }
        let t = (f32(i) + jitter) * dt;
        let q = entry + wdir * t;
        let dq = surface_y - q.y;
        if (dq <= 0.0) {
            continue;
        }
        let dens = caustic_density_at(q.xz, dq, ctile, ns, r0, vec2<f32>(0.0));
        var lift = max(dens - 1.0, 0.0) * smoothstep(0.02, 0.3, dq) * (1.0 - smoothstep(4.0, 12.0, dq));
        if (shaft.shadow.x > 0.5 && lift > 0.0) {
            lift = lift * shaft_sun_visibility(q, dq, beam, to_sun);
        }
        acc = acc + lift * exp(-sigma_t * (dq / max(to_sun.y, 0.25) + t)) * dt;
    }
    return body.scatter.rgb * frame.sun_color.rgb * (frame.sun_dir.w * body.absorption.w * phase * body.caustic.x) * acc;
}

// Temporal accumulation of the shaft march (ADR-3251, opt in): the history is reprojected by the
// end of the texel's march path (exact for a turning camera, the path's far end's parallax for a
// moving one), clamped to this frame's 3 x 3 neighbourhood so moved light does not ghost, and
// averaged with this frame by 1 / n over at most `shaft.history.y` frames. Invalid history (a
// camera cut, a new target, the eye crossing the surface) restarts at this frame.
@compute @workgroup_size(8, 8, 1)
fn cs_shaft_resolve(@builtin(global_invocation_id) id: vec3<u32>) {
    let size = vec2<i32>(textureDimensions(shaft_hist_out));
    let ij = vec2<i32>(id.xy);
    if (ij.x >= size.x || ij.y >= size.y) {
        return;
    }
    let cur = textureLoad(shaft_raw, ij, 0).rgb;
    var lo = cur;
    var hi = cur;
    for (var dy = -1; dy <= 1; dy = dy + 1) {
        for (var dx = -1; dx <= 1; dx = dx + 1) {
            let c = textureLoad(shaft_raw, clamp(ij + vec2<i32>(dx, dy), vec2<i32>(0), size - vec2<i32>(1)), 0).rgb;
            lo = min(lo, c);
            hi = max(hi, c);
        }
    }
    var out = vec4<f32>(cur, 1.0);
    if (shaft.history.x > 0.5) {
        let uv = (vec2<f32>(id.xy) + vec2<f32>(0.5)) / vec2<f32>(size);
        let d = textureLoad(depth_tex, texel_of(uv), 0);
        let p = world_at(uv, d);
        let eye = frame.eye.xyz;
        let to_p = p - eye;
        let len = length(to_p);
        let end = eye + to_p / max(len, 1.0e-4) * clamp(len, 0.1, 60.0);
        let prev = shaft.prev_view_proj * vec4<f32>(end, 1.0);
        if (prev.w > 1.0e-5) {
            let puv = vec2<f32>(prev.x / prev.w * 0.5 + 0.5, 0.5 - prev.y / prev.w * 0.5);
            if (puv.x >= 0.0 && puv.x < 1.0 && puv.y >= 0.0 && puv.y < 1.0) {
                let h = textureLoad(shaft_hist, vec2<i32>(puv * vec2<f32>(size)), 0);
                let n = min(h.a + 1.0, max(shaft.history.y, 1.0));
                out = vec4<f32>(mix(clamp(h.rgb, lo, hi), cur, 1.0 / n), n);
            }
        }
    }
    textureStore(shaft_hist_out, ij, out);
}

fn finish(rgb: vec3<f32>, alpha: f32) -> vec4<f32> {
    var c = max(rgb, vec3<f32>(0.0));
    if (frame.mode.w > 0.5) {
        c = vec3<f32>(1.0) - exp(-c);
        if (frame.mode.w > 1.5) {
            c = pow(c, vec3<f32>(1.0 / 2.2));
        }
    }
    let a = clamp(alpha, 0.0, 1.0);
    return vec4<f32>(c * a, a);
}

// Light at the eye's depth, as the water's scattering sees it, and the radiance of an endless
// water column: what the eye sees looking sideways under water.
fn volume_radiance(body: Body, eye_depth: f32) -> vec3<f32> {
    let sigma_t = body.absorption.rgb + vec3<f32>(body.absorption.w);
    let to_sun = frame.sun_dir.xyz;
    let down = exp(-sigma_t * eye_depth / max(to_sun.y, 0.25));
    let light_in = (frame.sun_dir.w * max(to_sun.y, 0.0) * frame.sun_color.rgb * down
        + vec3<f32>(frame.sun_color.w) * exp(-sigma_t * eye_depth)) / PI;
    let single = body.absorption.w / max(sigma_t, vec3<f32>(1.0e-4));
    return body.scatter.rgb * light_in * single;
}

// The surface seen from below (ADR-2342): inside the critical angle a window shows the sky or
// the scene above, refracted and dimmed by the water between; outside it the surface mirrors the
// water column (total internal reflection).
fn underside(body: Body, n: vec3<f32>, v: vec3<f32>, s: vec3<f32>, d_scene: f32, uv: vec2<f32>) -> vec4<f32> {
    let sigma_t = body.absorption.rgb + vec3<f32>(body.absorption.w);
    let eye_depth = max(s.y - frame.eye.y, 0.0);
    let volume = volume_radiance(body, eye_depth);
    let dist = max(length(frame.eye.xyz - s), 1.0e-3);
    let trans = exp(-sigma_t * dist);
    let cos_i = clamp(dot(-n, v), 0.0, 1.0);
    let t = refract(-v, -n, WATER_IOR);
    let inside = dot(t, t) > 1.0e-6;
    // Schlick inside the window, a smooth rise to 1 over the last degrees before total reflection.
    let sin_t = WATER_IOR * sqrt(max(1.0 - cos_i * cos_i, 0.0));
    let window = 1.0 - smoothstep(0.92, 1.0, sin_t);
    let f = mix(1.0, fresnel(cos_i, WATER_F0), window);
    var above = sky(select(vec3<f32>(0.0, 1.0, 0.0), t, inside));
    let composite = frame.mode.x > 0.5;
    let sky_px = d_scene >= 1.0;
    if (composite && !sky_px && inside) {
        above = textureSampleLevel(scene_tex, clamp_samp, uv, 0.0).rgb;
    }
    let inscatter = volume * (vec3<f32>(1.0) - trans);
    // The shaft pass's radiance for the column between the eye and this surface (ADR-2432); the
    // fog pass adds it under a blended surface itself.
    var shafts = vec3<f32>(0.0);
    if (body.caustic.x > 0.0 && body.caustic.w > 0.5) {
        shafts = textureSampleLevel(shaft_tex, clamp_samp, uv, 0.0).rgb;
    }
    if (composite) {
        return finish(f * volume + (1.0 - f) * (above * trans + inscatter + shafts), 1.0);
    }
    // Blend: the fog pass has already laid the water column over the target; the surface only adds
    // what it mirrors.
    return finish(volume, f);
}

@vertex
fn vs_fog(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * 2.0 - vec2<f32>(1.0), 0.0, 1.0);
}

// The water column between the eye and what it sees, while the eye is under a surface (ADR-2342):
// Beer-Lambert along the ray to the scene or to the surface above, plus the column's in-scatter.
// With a body's caustics on (ADR-2432) the scene below the surface takes the focused sun and the
// column adds the shaft pass's radiance.
@fragment
fn fs_fog(@builtin(position) clip: vec4<f32>) -> @location(0) vec4<f32> {
    if (frame.misc.w < 1.5) {
        discard;
    }
    let body = bodies.b[min(u32(frame.misc.w) - 2u, 15u)];
    let eye = frame.eye.xyz;
    let surface_y = frame.depth.w;
    let uv = clip.xy * frame.screen.zw;
    let d = textureLoad(depth_tex, texel_of(uv), 0);
    let p = world_at(uv, d);
    let to_p = p - eye;
    let scene_dist = min(length(to_p), 1.0e4);
    let dir = to_p / max(length(to_p), 1.0e-4);
    var surf_dist = 1.0e9;
    if (dir.y > 1.0e-4) {
        surf_dist = (surface_y - eye.y) / dir.y;
    }
    let path = max(min(scene_dist, surf_dist), 0.0);
    let sigma_t = body.absorption.rgb + vec3<f32>(body.absorption.w);
    let trans = exp(-sigma_t * path);
    let volume = volume_radiance(body, max(surface_y - eye.y, 0.0));
    let inscatter = volume * (vec3<f32>(1.0) - trans);
    var shafts = vec3<f32>(0.0);
    var gain = vec3<f32>(1.0);
    if (body.caustic.x > 0.0) {
        if (body.caustic.w > 0.5) {
            shafts = textureSampleLevel(shaft_tex, clamp_samp, uv, 0.0).rgb;
        }
        if (d < 1.0 && p.y < surface_y) {
            // What lies under the surface is lit by the focused sun (ADR-2432).
            let sun_share = clamp(frame.sun_dir.y * 2.0, 0.0, 1.0) * frame.sun_dir.w / max(frame.sun_dir.w + frame.sun_color.w, 1.0e-4);
            gain = caustic_receiver(p, surface_y, vec2<f32>(0.0), body, min(u32(frame.misc.w) - 2u, 15u), scene_dist, sun_share);
        }
    }
    if (frame.mode.x > 0.5) {
        let scene = textureSampleLevel(scene_tex, clamp_samp, uv, 0.0).rgb;
        return finish(scene * gain * trans + inscatter + shafts, 1.0);
    }
    let opacity = 1.0 - dot(trans, vec3<f32>(0.3333));
    return finish((inscatter + shafts) / max(opacity, 1.0e-3), opacity);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let body = bodies.b[min(in.body, 15u)];
    let eye = frame.eye.xyz;
    let s = in.world;
    let to_eye = eye - s;
    let dist_s = max(length(to_eye), 1.0e-4);
    let v = to_eye / dist_s;
    let px = vec2<i32>(in.clip.xy);
    let uv = in.clip.xy * frame.screen.zw;
    let tile = max(body.surface.y, 0.1);

    // ---- Obstacles: flow deflection, standing waves, whitewater (no texture reads here) ----
    var flow = in.flow;
    var wave_grad = vec2<f32>(0.0);
    var foam = 0.0;
    let speed = length(in.flow);
    let n_obs = u32(frame.mode.z);
    if (speed > 0.02 && n_obs > 0u) {
        let fdir = in.flow / speed;
        let side = vec2<f32>(-fdir.y, fdir.x);
        let lambda = clamp(TAU * speed * speed / GRAVITY, 0.15, 3.0);
        let k = TAU / lambda;
        let head = speed * speed / (2.0 * GRAVITY);
        let e = 0.02;
        for (var i = 0u; i < 64u; i = i + 1u) {
            if (i >= n_obs) {
                break;
            }
            let o = obstacles.o[i];
            let d = s.xz - o.xy;
            let reach = o.z * 6.0 + 4.0 * lambda + 1.0;
            if (dot(d, d) > reach * reach) {
                continue;
            }
            // Potential flow around a cylinder of radius a: the free stream bends around it.
            let r2 = max(dot(d, d), o.z * o.z);
            let du = dot(d, fdir);
            let dw = dot(d, side);
            let a2r4 = o.z * o.z / (r2 * r2);
            flow = flow + speed * a2r4 * (-(du * du - dw * dw) * fdir - 2.0 * du * dw * side);
            let shimmer = 0.85 + 0.15 * sin(TAU * 0.5 * frame.eye.w + f32(i) * 1.7);
            let amp = head * o.w * shimmer;
            let hx = standing(d + vec2<f32>(e, 0.0), o.z, fdir, k, lambda)
                - standing(d - vec2<f32>(e, 0.0), o.z, fdir, k, lambda);
            let hz = standing(d + vec2<f32>(0.0, e), o.z, fdir, k, lambda)
                - standing(d - vec2<f32>(0.0, e), o.z, fdir, k, lambda);
            wave_grad = wave_grad + amp * vec2<f32>(hx, hz) / (2.0 * e);
            foam = foam + whitewater(d, o.z, fdir) * min(speed / 0.6, 1.0) * o.w;
        }
    }

    // ---- Swell (ADR-1620): the trochoidal surface's slope and crest foam ----
    let sb = min(in.body, 15u);
    let sfield = shore_field(sb);
    var ocean_lost = 0.0;
    if (swell.b[sb].level.y > 0.5) {
        let footprint = dist_s * 2.0 * frame.depth.z * frame.screen.w;
        let h = shore_sample(sfield, swell.grid.xy + in.rest).x;
        let w = ocean_weight(sb, h);
        if (w < 1.0) {
            let sf = swell_frame(sb, in.rest, footprint, h);
            wave_grad = wave_grad + (1.0 - w) * sf.xy;
            foam = foam + (1.0 - w) * crest_foam(sf.z, swell.b[sb].header.z, swell.b[sb].header.w);
            if (sfield >= 0) {
                foam = foam + (1.0 - w) * breaker_foam(sf.w);
            }
        }
        // ---- FFT ocean (ADR-1969): its slope, Jacobian foam and the detail its faded bands lost ----
        if (w > 0.0) {
            let fr = ocean_frame(in.rest, footprint);
            wave_grad = wave_grad + w * fr.xy;
            if (ocean.header.w > 0.5 || ocean.shore.z > 0.5) {
                // Lingering foam and the whitecap rescale (ADR-2843), both opt-in.
                foam = foam + w * ocean_foam_extra(fr, in.rest, footprint);
            } else {
                foam = foam + w * crest_foam(fr.z, ocean.header.y, ocean.header.z);
            }
            ocean_lost = w * fr.w;
        }
    }
    // ---- Shore (spec 87): the foam band along the baked shoreline ----
    if (sfield >= 0) {
        let bed = shore_sample(sfield, s.xz);
        if (bed.z > 0.5 && bed.x > 0.0) {
            // Wet: the distance is at least 0 (depth and distance cross zero up to half a
            // texel apart).
            foam = foam + shore_foam(max(bed.y, 0.0), body.wet.y);
        }
    }

    // ---- Ripples: two flow-map layers, plus a coarse pair on High and Ultra ----
    // They run in the flow's frame (along, across), stretched along moving water into streaks.
    let tangent = vec2<f32>(in.right.y, -in.right.x);
    let local_flow = vec2<f32>(dot(flow, tangent), dot(flow, in.right));
    let stretch = mix(1.0, 0.4, smoothstep(0.05, 0.8, length(local_flow)));
    let squash = vec2<f32>(stretch, 1.0);
    let base = in.ripple * squash / tile;
    let fa = local_flow * squash * (frame.flow.x * frame.flow.w) / tile;
    let fb = local_flow * squash * (frame.flow.y * frame.flow.w) / tile;
    let ra = decode_ripple(textureSample(ripple_tex, ripple_samp, base - fa + frame.scroll.xy));
    let rb = decode_ripple(textureSample(ripple_tex, ripple_samp, base - fb + frame.scroll.zw + vec2<f32>(0.37, 0.61)));
    let wa = frame.flow.z;
    var local_slope = ra.slope * wa + rb.slope * (1.0 - wa);
    var variance = ra.variance * wa + rb.variance * (1.0 - wa);
    let static_noise = textureSample(ripple_tex, ripple_samp, s.xz / (tile * 1.7) + vec2<f32>(0.19, 0.83)).a;
    let fine_noise = textureSample(ripple_tex, ripple_samp, s.xz / (tile * 0.31) + vec2<f32>(0.53, 0.29)).a;
    if (frame.coarse.w > 0.5) {
        let ctile = tile * 4.0;
        let cbase = in.ripple * squash / ctile;
        let ca = local_flow * squash * (frame.coarse.x * frame.coarse.z) / ctile;
        let cb = local_flow * squash * (fract(frame.coarse.x + 0.5) * frame.coarse.z) / ctile;
        let rc0 = decode_ripple(textureSample(ripple_tex, ripple_samp, cbase - ca + 0.5 * frame.scroll.zw + vec2<f32>(0.11, 0.23)));
        let rc1 = decode_ripple(textureSample(ripple_tex, ripple_samp, cbase - cb + 0.5 * frame.scroll.xy + vec2<f32>(0.71, 0.05)));
        let cw = frame.coarse.y;
        local_slope = local_slope + 0.6 * (rc0.slope * cw + rc1.slope * (1.0 - cw));
        variance = variance + 0.36 * (rc0.variance * cw + rc1.variance * (1.0 - cw));
        // Two more scales (ADR-1450): a broad swell (about 9 tiles, nearly static) that breaks the
        // repeat of the main layers, and a fine capillary layer (0.37 tile) advected with the flow.
        let swell = decode_ripple(textureSample(ripple_tex, ripple_samp, in.ripple * squash / (tile * 9.0) - local_flow * squash * (frame.coarse.x * frame.coarse.z) / (tile * 9.0) + vec2<f32>(0.43, 0.77)));
        let capill = decode_ripple(textureSample(ripple_tex, ripple_samp, base * 2.7 - fa * 2.7 + frame.scroll.zw * 1.9 + vec2<f32>(0.29, 0.61)));
        local_slope = local_slope + 0.45 * swell.slope + 0.25 * capill.slope;
        variance = variance + 0.2 * swell.variance + 0.06 * capill.variance;
    }
    // Back to world slopes (chain rule through the stretch); faster water is choppier.
    let chop = body.surface.x * (1.0 + 0.4 * min(speed, 2.0));
    local_slope = local_slope * squash;
    let slope = (local_slope.x * tangent + local_slope.y * in.right) * chop + wave_grad;
    variance = variance * chop * chop + ocean_lost;
    let n = normalize(vec3<f32>(-slope.x, 1.0, -slope.y));

    // ---- What the scene holds at this pixel ----
    let d_scene = textureLoad(depth_tex, px, 0);
    let sky_px = d_scene >= 1.0;
    var p = world_at(uv, d_scene);
    if (sky_px) {
        p = s - v * 1.0e5;
    }
    let lin_s = linear_depth(in.clip.z);
    let lin_p = select(linear_depth(d_scene), 1.0e9, sky_px);
    let in_front = lin_p < lin_s - 0.005;
    let half_w = in.half_width;
    let ribbon = half_w < PLANE_HALF_WIDTH;
    let lateral_p = in.lateral + dot(p.xz - s.xz, in.right);
    let on_water = abs(in.lateral) <= half_w;

    let to_sun = frame.sun_dir.xyz;
    let sun_e = frame.sun_dir.w;
    let amb_e = frame.sun_color.w;
    let sun_rgb = frame.sun_color.rgb;

    // ---- Wet banks: the scene stands in front of the surface, or lies beside a ribbon ----
    if (in_front || !on_water) {
        let h = p.y - s.y;
        let wet_h = body.wet.x * (0.6 + 0.8 * static_noise);
        var near_water = 1.0;
        if (ribbon) {
            // A gradient from the waterline outward, with a ragged border (no plateau, no kerb).
            let out_d = max(abs(lateral_p) - half_w, 0.0) / max(body.wet.y, 0.05);
            let fall = 1.0 - smoothstep(0.0, 1.0, out_d + 0.35 * (static_noise - 0.5));
            near_water = fall * fall;
        } else {
            let reach = body.wet.y + body.wet.x / max(v.y, 0.05);
            near_water = 1.0 - smoothstep(0.7 * reach, reach, length(p.xz - s.xz));
        }
        // The film thins from a third of the wet height up (not from the waterline itself, which left
        // the first centimetres above the water nearly dry).
        let wet = (1.0 - smoothstep(0.35 * wet_h, wet_h, h)) * near_water * select(1.0, 0.0, sky_px);
        if (wet <= 0.002 || eye.y < s.y) {
            discard;
        }
        // A wet film reflects a little sky at grazing angles.
        // The sheen fades with the wetness, so the band's outer edge carries no sky-colored line.
        let sheen = fresnel(abs(dot(v, vec3<f32>(0.0, 1.0, 0.0))), WATER_F0) * sky(reflect(-v, vec3<f32>(0.0, 1.0, 0.0)));
        return finish(sheen * 0.25 * wet / max(body.wet.z, 0.05), wet * body.wet.z * 0.9);
    }
    if (eye.y < s.y) {
        // Seen from below: not drawn with the underwater view off (ADR-1399 known issue), else
        // the underside of the surface (ADR-2342).
        if (frame.misc.w < 0.5) {
            discard;
        }
        return underside(body, n, v, s, d_scene, uv);
    }

    // ---- The water itself ----
    let cos_v = clamp(dot(n, v), 1.0e-3, 1.0);
    let cos_t = sqrt(max(1.0 - (1.0 - cos_v * cos_v) / (WATER_IOR * WATER_IOR), 0.0));
    let depth_v = max(s.y - p.y, 0.0);
    // Soft shoreline: fade by depth over 12 cm and, on a ribbon, by the distance to the bank.
    var shore = smoothstep(0.0, 0.12, depth_v);
    var bank = 1.0;
    if (ribbon) {
        bank = 1.0 - smoothstep(half_w - 0.18, half_w, abs(in.lateral) + 0.06 * (static_noise - 0.5));
        shore = shore * (0.15 + 0.85 * bank);
    }
    // Reflections use a calmer normal than the shading normal: a small stream is not a mirror up close.
    // From 40 m to 150 m the calming fades out (and where the ripples are below the pixel the normal is
    // already an average), so a distant stream still shimmers.
    let calm_off = max(clamp(4.0 * variance, 0.0, 1.0), smoothstep(40.0, 150.0, dist_s));
    let n_refl = normalize(mix(vec3<f32>(0.0, 1.0, 0.0), n, mix(0.55, 1.0, calm_off)));
    let cos_r = clamp(dot(n_refl, v), 1.0e-3, 1.0);
    let f = fresnel(cos_r, WATER_F0) * mix(1.0, 0.7, clamp(body.scatter.w * 2.0 + 4.0 * variance, 0.0, 1.0));
    var refl_dir = reflect(-v, n_refl);
    refl_dir.y = max(abs(refl_dir.y), 0.02);
    refl_dir = normalize(refl_dir);

    let composite = frame.mode.x > 0.5;
    let sun_vis = select(1.0, sun_visibility(s + vec3<f32>(0.0, 0.02, 0.0), to_sun), composite);

    // Reflection: screen-space hit, else the sky the frame shows in that direction, else the
    // analytic sky.
    var refl = sky(refl_dir);
    var hit_uv = vec2<f32>(0.0);
    var hit_w = 0.0;
    var sky_uv = vec2<f32>(0.0);
    var sky_w = 0.0;
    if (composite) {
        let hit = trace_reflection(s, refl_dir, dist_s);
        hit_uv = hit.uv;
        hit_w = hit.weight;
        let sky_hit = sky_on_screen(refl_dir);
        sky_uv = sky_hit.uv;
        sky_w = sky_hit.weight;
    }
    // Roughness-aware: a 4-tap cross blur whose radius grows with the roughness and the lost ripple
    // detail, then a pull toward the mean sky so mirror-sharp contrast does not read as chrome.
    let blur = clamp(sqrt(body.scatter.w * body.scatter.w + 2.0 * variance) * 0.9, 0.0, 0.6);
    let br = (0.004 + 0.03 * blur) * vec2<f32>(1.0, frame.screen.x * frame.screen.w);
    let sky_seen = 0.4 * textureSampleLevel(scene_tex, clamp_samp, sky_uv, 0.0).rgb
        + 0.15 * (textureSampleLevel(scene_tex, clamp_samp, sky_uv + vec2<f32>(br.x, 0.0), 0.0).rgb
        + textureSampleLevel(scene_tex, clamp_samp, sky_uv - vec2<f32>(br.x, 0.0), 0.0).rgb
        + textureSampleLevel(scene_tex, clamp_samp, sky_uv + vec2<f32>(0.0, br.y), 0.0).rgb
        + textureSampleLevel(scene_tex, clamp_samp, sky_uv - vec2<f32>(0.0, br.y), 0.0).rgb);
    let hit_seen = 0.4 * textureSampleLevel(scene_tex, clamp_samp, hit_uv, 0.0).rgb
        + 0.15 * (textureSampleLevel(scene_tex, clamp_samp, hit_uv + vec2<f32>(br.x, 0.0), 0.0).rgb
        + textureSampleLevel(scene_tex, clamp_samp, hit_uv - vec2<f32>(br.x, 0.0), 0.0).rgb
        + textureSampleLevel(scene_tex, clamp_samp, hit_uv + vec2<f32>(0.0, br.y), 0.0).rgb
        + textureSampleLevel(scene_tex, clamp_samp, hit_uv - vec2<f32>(0.0, br.y), 0.0).rgb);
    refl = mix(refl, sky_seen, sky_w);
    refl = mix(refl, hit_seen, hit_w);
    refl = mix(refl, mix(frame.sky_horizon.rgb, frame.sky_zenith.rgb, 0.35), 0.5 * blur);

    // Refraction: the bed where the Snell-refracted view ray meets it (bed taken as locally flat),
    // unless the scene there is in front of the surface.
    var bed_uv = uv;
    var bed_depth = depth_v;
    if (composite) {
        let t_dir = refract(-v, n, 1.0 / WATER_IOR);
        let bed_point = s + t_dir * (depth_v / max(-t_dir.y, 0.2));
        let q = project(bed_point);
        if (q.w != 0.0 && on_screen(q.xy)) {
            let cand = mix(uv, q.xy, body.surface.z);
            let cd = textureLoad(depth_tex, texel_of(cand), 0);
            if (linear_depth(cd) > lin_s) {
                bed_uv = cand;
                let bp = select(world_at(cand, cd), s - v * 1.0e5, cd >= 1.0);
                bed_depth = max(s.y - bp.y, 0.0);
            }
        }
    }
    var bed = textureSampleLevel(scene_tex, clamp_samp, bed_uv, 0.0).rgb;
    if (composite) {
        let bed_xz = s.xz - v.xz * (bed_depth / max(v.y, 0.2));
        if (body.caustic.x > 0.0) {
            // The tile pass's pattern (ADR-2432), carried by the flow like the ripples.
            let ctile = max(body.surface.y, 0.1) * max(body.caustic.y, 0.1);
            let sun_share = sun_vis * clamp(to_sun.y * 2.0, 0.0, 1.0) * sun_e / max(sun_e + amb_e, 1.0e-4);
            let shift = -flow * (frame.flow.x * frame.flow.w) / ctile;
            bed = bed * caustic_receiver(vec3<f32>(bed_xz.x, s.y - bed_depth, bed_xz.y), s.y, shift, body, min(in.body, 15u), dist_s, sun_share);
        } else {
            bed = bed * caustic_gain(bed_xz, flow, tile, dist_s, bed_depth, to_sun, sun_vis * min(chop, 1.0), sun_e, amb_e);
        }
    }

    // Beer-Lambert along the view path through the water plus the light's way down.
    let path = min(bed_depth / max(cos_t, 0.2) + bed_depth, 1.0e4);
    let sigma_t = body.absorption.rgb + vec3<f32>(body.absorption.w);
    let trans = exp(-sigma_t * path);
    let light_in = (sun_e * max(to_sun.y, 0.0) * sun_vis * sun_rgb + vec3<f32>(amb_e)) / PI;
    let single = body.absorption.w / max(sigma_t, vec3<f32>(1.0e-4));
    // Sediment tint (ADR-1450): silt-brown scattering that grows with depth and varies slowly across
    // the stream, so shallows read tan-green and the middle darker.
    let silt = clamp(0.25 + 0.3 * static_noise + 0.2 * clamp(bed_depth * 2.0, 0.0, 1.0), 0.0, 0.7);
    let scatter_rgb = mix(body.scatter.rgb, vec3<f32>(0.30, 0.24, 0.14), silt * 0.6);
    var inscatter = scatter_rgb * light_in * single * (vec3<f32>(1.0) - trans);
    // Shafts seen from above (ADR-3251, opt in): the shaft pass marched this body's water.
    if (shaft_draw.march.y > 0.5 && u32(shaft_draw.march.w) == min(in.body, 15u) && body.caustic.x > 0.0) {
        inscatter = inscatter + textureSampleLevel(shaft_tex, clamp_samp, uv, 0.0).rgb;
    }

    // Sun glint: GGX with the lost ripple detail added to the roughness.
    let rough = body.scatter.w;
    let a2 = clamp(rough * rough * rough * rough + 2.0 * variance, 1.0e-5, 1.0);
    let hv = normalize(to_sun + v);
    let n_l = max(dot(n, to_sun), 0.0);
    let n_h = max(dot(n, hv), 0.0);
    let dd = n_h * n_h * (a2 - 1.0) + 1.0;
    let ggx = a2 / (PI * dd * dd);
    let g1v = 2.0 * cos_v / (cos_v + sqrt(a2 + (1.0 - a2) * cos_v * cos_v));
    let g1l = 2.0 * n_l / (n_l + sqrt(a2 + (1.0 - a2) * n_l * n_l));
    let spec = ggx * g1v * g1l * fresnel(max(dot(hv, v), 0.0), WATER_F0) / (4.0 * cos_v)
        * sun_e * sun_vis * body.wet.w;
    let glint = spec * sun_rgb * select(0.0, 1.0, n_l > 0.0);

    // Whitewater, lit like a rough white surface.
    var foam_amt = clamp(foam * body.surface.w * (0.3 + 1.2 * fine_noise), 0.0, 1.0);
    if (ribbon) {
        // A thin broken foam line where the water meets the bank.
        let edge_line = smoothstep(0.55, 1.0, 1.0 - bank) * smoothstep(0.35, 0.7, fine_noise);
        foam_amt = max(foam_amt, edge_line * 0.35 * min(body.surface.w, 1.0));
    }
    let foam_rgb = vec3<f32>(0.85) * light_in;

    // Ice along the edges and over shallow water.
    var ice = 0.0;
    if (body.ice.z > 0.5) {
        // Ice grows in from the bank and over the shallowest water; the noise roughens its border.
        var edge = 1.0 - smoothstep(body.ice.y * 0.6, body.ice.y, bed_depth);
        if (ribbon) {
            edge = max(edge, smoothstep(half_w - body.ice.x, half_w, abs(in.lateral)));
        }
        let ragged = 0.6 * static_noise + 0.4 * fine_noise - 0.5;
        ice = smoothstep(0.45, 0.55, edge + 0.8 * ragged) * (1.0 - smoothstep(0.6, 1.2, speed));
    }

    var rgb: vec3<f32>;
    var alpha: f32;
    if (composite) {
        let water = f * refl + (1.0 - f) * (bed * trans + inscatter) + glint;
        rgb = mix(water, foam_rgb, foam_amt * 0.85);
        alpha = shore;
    } else {
        // Blend: the target already holds the bed; cover it by the body's opacity.
        let t_mean = dot(trans, vec3<f32>(0.3333));
        let opacity = 1.0 - (1.0 - f) * t_mean;
        rgb = f * refl + (1.0 - f) * inscatter + glint;
        rgb = mix(rgb, foam_rgb, foam_amt * 0.85);
        alpha = mix(opacity, 1.0, foam_amt * 0.85) * shore;
        rgb = rgb / max(alpha, 1.0e-3) * shore;
    }
    if (ice > 0.0) {
        let ice_n = normalize(vec3<f32>(-0.15 * slope.x, 1.0, -0.15 * slope.y));
        // Bubbly white patches in clearer grey ice.
        let albedo = body.ice_color.rgb * (0.7 + 0.6 * fine_noise);
        let lit = albedo * (sun_e * max(dot(ice_n, to_sun), 0.0) * sun_vis * sun_rgb + vec3<f32>(amb_e)) / PI;
        let fi = fresnel(max(dot(ice_n, v), 0.0), ICE_F0);
        let under = select(inscatter, bed * trans + inscatter, composite);
        let ice_rgb = fi * refl + (1.0 - fi) * mix(lit, under, body.ice.w);
        rgb = mix(rgb, ice_rgb, ice);
        alpha = max(alpha, ice * select(0.9, 1.0, composite));
    }
    // Debug views (agent `water.set { debug }`): scalar views are scaled to the horizon sky so
    // they survive the exposure.
    let view = u32(frame.misc.z);
    if (view != 0u) {
        let unit = frame.sky_horizon.g;
        if (view == 1u) {
            rgb = select((1.0 - f) * inscatter, bed * trans + inscatter, composite);
        } else if (view == 2u) {
            rgb = refl;
        } else if (view == 3u) {
            rgb = (n * 0.5 + vec3<f32>(0.5)) * unit;
        } else if (view == 4u) {
            rgb = vec3<f32>(f) * unit;
        } else {
            rgb = vec3<f32>(clamp(foam_amt, 0.0, 1.0), ice, 0.0) * unit;
        }
        alpha = 1.0;
    }
    return finish(rgb, alpha);
}
