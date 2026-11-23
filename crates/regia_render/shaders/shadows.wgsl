// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::shadows

// Smallest sun PCSS filter radius, shadow texels (`regia_render::shadow::SUN_PCSS_MIN_RADIUS_TEXELS`).
// A contact shadow's true penumbra is far below a texel; this floor is the filter that keeps
// the texel grid from showing. It was 1.5 texels: on the 1 cm texels of a hero shot that
// blurred a thin caster's outline over ~4 cm, so the X-12's fin shadow lost its corners.
const SUN_PCSS_MIN_RADIUS_TEXELS: f32 = 1.0;

// BEGIN angular sun penumbra (ADR-3202, shadow filter mode 4; mirrored by
// `regia_render/tests/ao_contact_band.rs`). The PCSS filter's edge is a disc of the clamped radius
// convolved with each tap's bilinear tent, so a near-contact penumbra never gets narrower than
// about 1.7 texels (14 mm on cascade 0 of the spec 27 sun wall, where the sun's disc gives 6.8 mm).
// Where the sun's disc is narrower, the filtered visibility is steepened about its midpoint: the
// 0.5 contour stays where the filter put it, only the ramp narrows. Widths are 10-90 %, in texels:
// a uniform disc of radius r spreads an edge over 1.374 r; the filter's edge is that of its
// clamped radius with an effective 0.95-texel tent in quadrature, and a steepened S-shaped ramp
// comes out 0.9 of the plain width ratio. The ramp never gets narrower than 0.4 texels. Fitted per
// pixel on the CPU mirror of the filter (16 Vogel taps, bilinear), at every sub-texel position of
// the edge: within 10 % of the disc's width from a 0.6-texel radius up. (The texel grid still places
// an edge only to within half a texel: along an edge that crosses the grid that is a stair, not a
// wider ramp.)
const SUN_PENUMBRA_DISC_WIDTH: f32 = 1.374;
const SUN_PENUMBRA_TENT_WIDTH: f32 = 0.95;
const SUN_PENUMBRA_RAMP_FIT: f32 = 0.9;
const SUN_PENUMBRA_MIN_WIDTH_TEXELS: f32 = 0.4;

// The steepening for a true penumbra radius `true_radius` (texels, the blocker gap times the
// sun's angular radius, unclamped) filtered at `filter_radius` texels: 1 where the filter is
// already as narrow as the sun's disc.
fn sun_penumbra_gain(true_radius: f32, filter_radius: f32) -> f32 {
    let filter_disc = SUN_PENUMBRA_DISC_WIDTH * filter_radius;
    let filter_width = sqrt(filter_disc * filter_disc + SUN_PENUMBRA_TENT_WIDTH * SUN_PENUMBRA_TENT_WIDTH);
    let want = max(SUN_PENUMBRA_DISC_WIDTH * max(true_radius, 0.0), SUN_PENUMBRA_MIN_WIDTH_TEXELS);
    return max(SUN_PENUMBRA_RAMP_FIT * filter_width / want, 1.0);
}

// `lit` steepened by `gain` about 0.5.
fn sun_penumbra_steepen(lit: f32, gain: f32) -> f32 {
    return clamp(0.5 + (lit - 0.5) * gain, 0.0, 1.0);
}
// END angular sun penumbra

// 3×3 percentage-closer filtering. The caller owns the depth textures.

fn shadow_pcf_3x3(
    map: texture_depth_2d,
    samp: sampler_comparison,
    uv: vec2<f32>,
    compare_z: f32,
    texel: f32,
) -> f32 {
    var shadow: f32 = 0.0;
    for (var x: i32 = -1; x <= 1; x += 1) {
        for (var y: i32 = -1; y <= 1; y += 1) {
            let offset = vec2<f32>(f32(x), f32(y)) * texel;
            shadow += textureSampleCompare(map, samp, uv + offset, compare_z);
        }
    }
    return shadow / 9.0;
}

// 5×5 PCF. The "PCF" profile row (desktop low/high), wider than the 3×3 kernel.
fn shadow_pcf_5x5(
    map: texture_depth_2d,
    samp: sampler_comparison,
    uv: vec2<f32>,
    compare_z: f32,
    texel: f32,
) -> f32 {
    var shadow: f32 = 0.0;
    for (var x: i32 = -2; x <= 2; x += 1) {
        for (var y: i32 = -2; y <= 2; y += 1) {
            let offset = vec2<f32>(f32(x), f32(y)) * texel;
            shadow += textureSampleCompare(map, samp, uv + offset, compare_z);
        }
    }
    return shadow / 25.0;
}

// PCSS-lite: a 4-tap blocker search, then a PCF kernel whose radius grows with
// the receiver–blocker gap. Fernando, "Percentage-Closer Soft Shadows", SIGGRAPH 2005.
// Standard-Z depth (0 near, 1 far): a blocker is closer, so its depth is smaller.
fn shadow_pcss_lite(
    map: texture_depth_2d,
    samp: sampler_comparison,
    uv: vec2<f32>,
    compare_z: f32,
    texel: f32,
) -> f32 {
    let dims = vec2<i32>(textureDimensions(map, 0));
    let coord = vec2<i32>(uv * vec2<f32>(dims));
    var blocker: f32 = 0.0;
    var n: f32 = 0.0;
    for (var x: i32 = -1; x <= 1; x += 2) {
        for (var y: i32 = -1; y <= 1; y += 2) {
            let c = clamp(coord + vec2<i32>(x, y) * 2, vec2<i32>(0), dims - vec2<i32>(1));
            let d = textureLoad(map, c, 0);
            if (d < compare_z) {
                blocker += d;
                n += 1.0;
            }
        }
    }
    var radius: f32 = 1.0;
    if (n > 0.0) {
        let avg = blocker / n;
        radius = clamp((compare_z - avg) / max(avg, 1.0e-4) * 8.0, 1.0, 4.0);
    }
    var shadow: f32 = 0.0;
    for (var x: i32 = -1; x <= 1; x += 1) {
        for (var y: i32 = -1; y <= 1; y += 1) {
            let offset = vec2<f32>(f32(x), f32(y)) * texel * radius;
            shadow += textureSampleCompare(map, samp, uv + offset, compare_z);
        }
    }
    return shadow / 9.0;
}

fn shadow_uv_from_clip(clip: vec4<f32>) -> vec3<f32> {
    var uv = clip.xyz / clip.w;
    // wgpu renders NDC +y to the texture's TOP row, and sampling with v = 0
    // also reads the top row — so the sample v must be the NDC y flipped:
    // v = 0.5 - ndc.y * 0.5 (the wgpu shadow-example Y-flip convention).
    // z is already ∈ [0, 1] (the engine uses the DirectX depth convention).
    uv = vec3<f32>(uv.x * 0.5 + 0.5, 1.0 - (uv.y * 0.5 + 0.5), uv.z);
    return uv;
}

// Vogel (golden-angle) disc sample `i` of `n`, radius ≤ 1, rotated by `rot` radians.
fn vogel_disc(i: u32, n: u32, rot: f32) -> vec2<f32> {
    let r = sqrt((f32(i) + 0.5) / f32(n));
    let a = f32(i) * 2.399963 + rot;
    return vec2<f32>(cos(a), sin(a)) * r;
}

// Depth slope of the receiver plane (normal `n`) in an orthographic shadow map: the change
// of the compare depth per unit of shadow UV along u and v (receiver-plane depth bias,
// Isidoro 2006, "Shadow Mapping: GPU-based Tips and Techniques"). Two tangents of the plane
// map linearly through the ortho `vp`. Zero when the plane is edge-on to the light.
fn receiver_plane_slope(vp: mat4x4<f32>, n: vec3<f32>) -> vec2<f32> {
    let helper = select(vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(0.0, 0.0, 1.0), abs(n.x) > 0.9);
    let t1 = normalize(cross(n, helper));
    let t2 = cross(n, t1);
    let a = (vp * vec4<f32>(t1, 0.0)).xyz;
    let b = (vp * vec4<f32>(t2, 0.0)).xyz;
    // NDC → UV: u = x/2 + 1/2, v = 1/2 − y/2.
    let au = vec3<f32>(a.x * 0.5, -a.y * 0.5, a.z);
    let bu = vec3<f32>(b.x * 0.5, -b.y * 0.5, b.z);
    let det = au.x * bu.y - au.y * bu.x;
    if (abs(det) < 1e-12) {
        return vec2<f32>(0.0);
    }
    return vec2<f32>(au.z * bu.y - au.y * bu.z, au.x * bu.z - au.z * bu.x) / det;
}

// Compare depth of the receiver plane at texel offset `d` (texels) from the receiver:
// `slope` is the plane's depth per texel and `bend` lowers the plane by that much per texel
// of distance. A real receiver is not a plane: across a 9-texel PCSS disc on a far cascade
// a rolling hill or a crease bends away by metres, and an exact plane self-shadows the
// concave side. `bend` is a slope allowance (`CSM_BEND` in `lab_forward.wgsl`).
fn plane_depth(z: f32, slope: vec2<f32>, bend: f32, d: vec2<f32>) -> f32 {
    return z + dot(slope, d) - bend * length(d);
}

// The 2×2 texels whose top-left is `c`, each compared against the receiver plane at its own
// centre (`d` = that top-left centre minus the receiver, in texels), as 1 = lit in
// (c, c+x, c+y, c+xy) order. One gather through the non-filtering `samp`: WGSL returns
// (c+y, c+xy, c+x, c) in x, y, z, w.
fn plane_quad_lit(
    map: texture_depth_2d,
    samp: sampler,
    c: vec2<i32>,
    inv_dims: vec2<f32>,
    z: f32,
    slope: vec2<f32>,
    bend: f32,
    d: vec2<f32>,
) -> vec4<f32> {
    return plane_quad_compare(textureGather(map, samp, vec2<f32>(c + vec2<i32>(1)) * inv_dims), z, slope, bend, d);
}

// The compare of `plane_quad_lit` for a gather `g` the caller made (WGSL order: c+y, c+xy,
// c+x, c), so a filter that picks its map per tap (`cascade_gather` in `lab_forward.wgsl`)
// shares it.
fn plane_quad_compare(g: vec4<f32>, z: f32, slope: vec2<f32>, bend: f32, d: vec2<f32>) -> vec4<f32> {
    let zq = vec4<f32>(
        plane_depth(z, slope, bend, d),
        plane_depth(z, slope, bend, d + vec2<f32>(1.0, 0.0)),
        plane_depth(z, slope, bend, d + vec2<f32>(0.0, 1.0)),
        plane_depth(z, slope, bend, d + vec2<f32>(1.0, 1.0)),
    );
    return select(vec4<f32>(0.0), vec4<f32>(1.0), zq <= vec4<f32>(g.w, g.z, g.x, g.y));
}

// One bilinear compare at `p` (texel units) whose four texels each compare against the
// receiver plane at their own centre (`plane_depth`, receiver at `uv_px`). The hardware
// 2×2 compare uses one depth for all four, so a sloped receiver needed a bias as large as
// the plane's change across them.
fn plane_bilinear(
    map: texture_depth_2d,
    samp: sampler,
    inv_dims: vec2<f32>,
    p: vec2<f32>,
    uv_px: vec2<f32>,
    z: f32,
    slope: vec2<f32>,
    bend: f32,
) -> f32 {
    let q = p - vec2<f32>(0.5);
    let b = floor(q);
    let f = q - b;
    let s = plane_quad_lit(map, samp, vec2<i32>(b), inv_dims, z, slope, bend, b + vec2<f32>(0.5) - uv_px);
    return mix(mix(s.x, s.y, f.x), mix(s.z, s.w, f.x), f.y);
}

// (2r+1)² bilinear PCF taps one texel apart around `uv`, over the receiver plane: the
// (2r+2)² texels they touch, gathered 2×2 at a time and each compared at its centre, with
// the taps' summed weights (1 − f, 1, …, 1, f per axis). r = 0 is the hardware 2×2 filter.
fn shadow_pcf_plane(
    map: texture_depth_2d,
    samp: sampler,
    uv: vec2<f32>,
    compare_z: f32,
    slope: vec2<f32>,
    bend: f32,
    radius: i32,
) -> f32 {
    let dims = vec2<f32>(textureDimensions(map, 0));
    let inv_dims = 1.0 / dims;
    let uv_px = uv * dims;
    let q = uv_px - vec2<f32>(0.5);
    let b = floor(q);
    let f = q - b;
    let c = vec2<i32>(b);
    let d0 = b + vec2<f32>(0.5) - uv_px;
    var lit = 0.0;
    for (var y: i32 = -radius; y <= radius; y += 2) {
        let wy = vec2<f32>(select(1.0, 1.0 - f.y, y == -radius), select(1.0, f.y, y + 1 == radius + 1));
        for (var x: i32 = -radius; x <= radius; x += 2) {
            let wx = vec2<f32>(select(1.0, 1.0 - f.x, x == -radius), select(1.0, f.x, x + 1 == radius + 1));
            let o = vec2<i32>(x, y);
            let s = plane_quad_lit(map, samp, c + o, inv_dims, compare_z, slope, bend, d0 + vec2<f32>(o));
            lit += dot(s, vec4<f32>(wx.x * wy.x, wx.y * wy.x, wx.x * wy.y, wx.y * wy.y));
        }
    }
    let n = f32(2 * radius + 1);
    return lit / (n * n);
}

// Sun PCSS for the directional cascades (the PCSS-lite profile row).
// Fernando, "Percentage-Closer Soft Shadows", SIGGRAPH 2005: a blocker search, then a
// filter whose radius is the receiver–blocker distance × the sun's angular radius.
// Taps lie on a Vogel disc rotated per pixel (`rot`, interleaved gradient noise:
// Jimenez, SIGGRAPH 2014) and filter bilinearly, so the penumbra is a smooth ramp instead
// of the old 3×3 grid's visible texel steps. Every texel a tap reads compares against the
// receiver plane at its own centre (`plane_bilinear`; `slope` is depth per texel, `bend`
// the curvature allowance), so a wide kernel does not self-shadow sloped ground and needs no
// footprint bias.
//
// `penumbra_per_depth`: penumbra radius in texels per unit of normalised depth
// between receiver and blocker (tan(sun angular radius) / (texel_world · depth_scale)).
// `search_n` / `filter_n` are the live tap counts. A count at or below 0.5 keeps the
// historical 12 / 16. The loops stay capped at those historical counts.
fn shadow_pcss_sun(
    map: texture_depth_2d,
    samp: sampler,
    uv: vec2<f32>,
    compare_z: f32,
    penumbra_per_depth: f32,
    slope: vec2<f32>,
    bend: f32,
    rot: f32,
    search_n: f32,
    filter_n: f32,
    angular: bool,
) -> f32 {
    let dims_i = vec2<i32>(textureDimensions(map, 0));
    let hi = dims_i - vec2<i32>(1);
    let dims = vec2<f32>(dims_i);
    let uv_px = uv * dims;
    let search = 7.0;
    let search_taps = min(select(12u, u32(search_n + 0.5), search_n > 0.5), 12u);
    let filter_taps = min(select(16u, u32(filter_n + 0.5), filter_n > 0.5), 16u);
    var blocker = 0.0;
    var count = 0.0;
    for (var i: u32 = 0u; i < 12u; i = i + 1u) {
        if (i >= search_taps) {
            break;
        }
        let o = vogel_disc(i, search_taps, rot) * search;
        let c = clamp(vec2<i32>(floor(uv_px + o)), vec2<i32>(0), hi);
        let d = textureLoad(map, c, 0);
        // The search takes the exact plane: a blocker it skips makes the sample lit, so the
        // curvature allowance only softens the filter below.
        if (d < plane_depth(compare_z, slope, 0.0, vec2<f32>(c) + vec2<f32>(0.5) - uv_px)) {
            blocker += d;
            count += 1.0;
        }
    }
    if (count < 0.5) {
        return 1.0;
    }
    let avg = blocker / count;
    let radius = clamp((compare_z - avg) * penumbra_per_depth, SUN_PCSS_MIN_RADIUS_TEXELS, 9.0);
    var lit = 0.0;
    for (var i: u32 = 0u; i < 16u; i = i + 1u) {
        if (i >= filter_taps) {
            break;
        }
        let o = vogel_disc(i, filter_taps, rot + 1.3) * radius;
        lit += plane_bilinear(map, samp, 1.0 / dims, uv_px + o, uv_px, compare_z, slope, bend);
    }
    if (angular) {
        // Shadow filter mode 4 (ADR-3202): the sun's disc below the filter's footprint.
        let gain = sun_penumbra_gain((compare_z - avg) * penumbra_per_depth, radius);
        return sun_penumbra_steepen(lit / f32(filter_taps), gain);
    }
    return lit / f32(filter_taps);
}

// A tap that leaves the atlas tile is lit. The neighbour tile stores another
// light's depths; reading it paints a soft blotch that no caster made.
fn atlas_tap_lit(uv: vec2<f32>, tile_min: vec2<f32>, tile_max: vec2<f32>) -> bool {
    return uv.x < tile_min.x || uv.y < tile_min.y || uv.x > tile_max.x || uv.y > tile_max.y;
}

// PCF kept inside one atlas rect. `textureSampleCompareLevel` so a divergent
// bounds check does not need implicit derivatives.
fn shadow_pcf_rect(
    map: texture_depth_2d,
    samp: sampler_comparison,
    uv: vec2<f32>,
    compare_z: f32,
    texel: f32,
    tile_min: vec2<f32>,
    tile_max: vec2<f32>,
    radius: i32,
) -> f32 {
    var shadow = 0.0;
    var n = 0.0;
    for (var x: i32 = -radius; x <= radius; x = x + 1) {
        for (var y: i32 = -radius; y <= radius; y = y + 1) {
            let p = uv + vec2<f32>(f32(x), f32(y)) * texel;
            if (atlas_tap_lit(p, tile_min, tile_max)) {
                shadow += 1.0;
            } else {
                shadow += textureSampleCompareLevel(map, samp, p, compare_z);
            }
            n += 1.0;
        }
    }
    return shadow / max(n, 1.0);
}

// Local-light PCSS. Same blocker search as `shadow_pcss_sun` (Fernando 2005,
// Vogel disc, Isidoro receiver-plane slope) but every tap stays inside the
// tile. No blocker closer than the receiver means the surface is lit: an
// unoccluded wall must not inherit a neighbour tile's near depths.
// `penumbra_per_depth` is penumbra texels per unit of normalised depth
// (source radius · depth extent / world texel).
//
// The search disc is 16 texels so it covers the filter clamp below. A 1-texel
// floor made the crate contact a hard line; 18 texels washed the umbra.
// `search_n` / `filter_n` match the sun. A count at or below 0.5 keeps 12 / 16.
// Both loops stay capped there.
fn shadow_pcss_atlas(
    map: texture_depth_2d,
    samp: sampler_comparison,
    uv: vec2<f32>,
    compare_z: f32,
    texel: f32,
    tile_min: vec2<f32>,
    tile_max: vec2<f32>,
    penumbra_per_depth: f32,
    slope_per_texel: f32,
    rot: f32,
    search_n: f32,
    filter_n: f32,
) -> f32 {
    let dims = vec2<f32>(textureDimensions(map, 0));
    let tile_min_px = tile_min * dims;
    let tile_max_px = max(tile_max * dims - vec2<f32>(1.0), tile_min_px);
    let search = 16.0;
    let search_taps = min(select(12u, u32(search_n + 0.5), search_n > 0.5), 12u);
    let filter_taps = min(select(16u, u32(filter_n + 0.5), filter_n > 0.5), 16u);
    var blocker = 0.0;
    var count = 0.0;
    for (var i: u32 = 0u; i < 12u; i = i + 1u) {
        if (i >= search_taps) {
            break;
        }
        let o = vogel_disc(i, search_taps, rot) * search;
        let p = uv * dims + o;
        if (p.x < tile_min_px.x || p.y < tile_min_px.y || p.x > tile_max_px.x || p.y > tile_max_px.y) {
            continue;
        }
        let d = textureLoad(map, vec2<i32>(p), 0);
        if (d < compare_z - length(o) * slope_per_texel) {
            blocker += d;
            count += 1.0;
        }
    }
    if (count < 0.5) {
        return 1.0;
    }
    let avg = blocker / count;
    // Min 2.5 texels is the contact penumbra: a 1-texel floor was a hard line
    // on the crate. Max 10 leaves a black core; 18 washed that core because
    // the penumbra scale already saturates a few millimetres off the caster.
    let radius = clamp((compare_z - avg) * penumbra_per_depth, 2.5, 10.0);
    var lit = 0.0;
    var n = 0.0;
    for (var i: u32 = 0u; i < 16u; i = i + 1u) {
        if (i >= filter_taps) {
            break;
        }
        let o = vogel_disc(i, filter_taps, rot + 1.3) * radius;
        let p = uv + o * texel;
        // A tap outside the tile is another light's depth. Counting it as lit
        // washed the umbra. Skip it and renormalise; no in-tile tap means lit.
        if (!atlas_tap_lit(p, tile_min, tile_max)) {
            lit += textureSampleCompareLevel(map, samp, p, compare_z - length(o) * slope_per_texel);
            n += 1.0;
        }
    }
    if (n < 0.5) {
        return 1.0;
    }
    return lit / n;
}
