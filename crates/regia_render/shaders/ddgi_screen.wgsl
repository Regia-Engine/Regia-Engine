// SPDX-License-Identifier: MPL-2.0
// GPU irradiance probes: the half-resolution screen lookup (compute, after the depth
// prepass; `ddgi.screen.w` pixels per texel, 2 unless measured, ADR-1033). Each texel
// reconstructs the surface under the top-left pixel of its block
// from the depth buffer (normal from the depth neighbours, taking the smaller difference
// on each axis so an edge does not bend it, and another pair of neighbours where that pair
// straddles a crease: `screen_normal`) and stores the probe irradiance there toward the
// six ambient-cube directions (one probe-weight pass). The forward and terrain passes rebuild
// the irradiance for their own normal from 2×2 texels instead of weighting 8
// probes per cascade per pixel.
// Composed as ddgi_common.wgsl + ddgi_sample.wgsl + this file.

@group(0) @binding(0) var<uniform> ddgi: DdgiUniform;
@group(0) @binding(1) var<storage, read_write> ddgi_data: array<u32>;
@group(0) @binding(2) var<storage, read_write> ddgi_rays: array<vec4<f32>>;

struct ScreenParams {
    inv_view_proj: mat4x4<f32>,
    // xyz = eye.
    eye: vec4<f32>,
    // xy = full-resolution size (the depth buffer).
    size: vec4<u32>,
}

@group(1) @binding(0) var<uniform> screen: ScreenParams;
@group(1) @binding(1) var depth: texture_depth_2d;

fn world_at(px: vec2<i32>) -> vec4<f32> {
    let full = vec2<i32>(screen.size.xy);
    let c = clamp(px, vec2<i32>(0), full - vec2<i32>(1));
    let d = textureLoad(depth, c, 0);
    let uv = (vec2<f32>(c) + vec2<f32>(0.5)) / vec2<f32>(full);
    let clip = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, d, 1.0);
    let w = screen.inv_view_proj * clip;
    return vec4<f32>(w.xyz / w.w, d);
}

// How far (the sine of the angle) the diagonal pixel of a quadrant may stand off the plane of the two neighbors
// beside it before the texel's normal is taken from another quadrant (`screen_normal`, ADR-5796). A flat surface
// reads 0 but for the depth buffer's rounding; a pair of neighbors that straddles a crease reads 0.3 and more.
const SCREEN_PLANAR: f32 = 0.05;

// The plane through `p` (the surface under pixel `px`) and its neighbors one pixel along x (`sx` = ±1) and one
// along y (`sy` = ±1): xyz = its normal (either sign), w = how far the diagonal pixel between the two stands off
// that plane, as the sine of the angle (`SCREEN_PLANAR`). w = 1e30 where the quadrant leaves the frame, holds
// the sky, or spans no plane.
fn screen_quadrant(px: vec2<i32>, p: vec3<f32>, sx: i32, sy: i32) -> vec4<f32> {
    let none = vec4<f32>(0.0, 0.0, 0.0, 1.0e30);
    let full = vec2<i32>(screen.size.xy);
    let q = px + vec2<i32>(sx, sy);
    if (q.x < 0 || q.y < 0 || q.x >= full.x || q.y >= full.y) {
        return none;
    }
    let a = world_at(px + vec2<i32>(sx, 0));
    let c = world_at(px + vec2<i32>(0, sy));
    let g = world_at(q);
    if (a.w >= 1.0 || c.w >= 1.0 || g.w >= 1.0) {
        return none;
    }
    let d = g.xyz - p;
    let cr = cross(c.xyz - p, a.xyz - p);
    let len2 = dot(cr, cr);
    let dd = dot(d, d);
    // NaN and ∞ by their exponent bits, sign shifted out (see `ddgi_depth_normal`).
    if ((bitcast<u32>(len2) << 1u) >= 0xff000000u || len2 <= 1.0e-30
        || (bitcast<u32>(dd) << 1u) >= 0xff000000u || dd <= 1.0e-30) {
        return none;
    }
    let n = cr * inverseSqrt(len2);
    return vec4<f32>(n, abs(dot(n, d)) * inverseSqrt(dd));
}

// The surface normal under pixel `px` (world position `p`, `v` toward the eye) from the depth of the pixels
// around it: `ddgi_depth_normal`'s (the shorter difference on each axis), with two corrections (ADR-5796).
//
// 1. A neighbor outside the frame is no difference at all. The clamped read used to return the pixel itself,
//    and `p` minus a second reconstruction of `p` is 0 only where both round alike: on an NVIDIA card it left a
//    rounding remainder, which the rule took for the shorter difference. Single texels of the frame's first row
//    and first column then held a normal that pointed anywhere, and with it another probe set's light (on the
//    Low tier, where the 2 m cell around a ceiling holds the probes over the roof, the daylight outside).
// 2. The shorter difference on x and the shorter one on y may lie on two surfaces: at a crease between a wall
//    and a roof one neighbor is the wall's and the other the roof's, and their cross product points along the
//    crease, into the room. No probe stands behind such a normal's plane, so the probes behind the roof or the
//    wall passed the front weight (`ddgi_front_weight`): single bright or dark texels along the creases near
//    the eye on the Low tier. On one flat surface the two differences the rule left lie in the plane of the
//    two it took, which costs no depth read to see. Where one of them stands off that plane (a crease or an
//    edge beside the pixel), the pair it took is tested against the diagonal pixel between its two neighbors,
//    which lies in their plane when all three are one surface. Where that one stands off too, the quadrant
//    whose diagonal lies nearest its own plane gives the normal, if it is clearly flatter. A texel on a flat
//    surface keeps the normal it had, bit for bit, and reads no more depth than before.
fn screen_normal(px: vec2<i32>, p: vec3<f32>, v: vec3<f32>) -> vec3<f32> {
    let full = vec2<i32>(screen.size.xy);
    let zero = vec3<f32>(0.0);
    let r = select(zero, world_at(px + vec2<i32>(1, 0)).xyz - p, px.x + 1 < full.x);
    let l = select(zero, p - world_at(px - vec2<i32>(1, 0)).xyz, px.x > 0);
    let u = select(zero, world_at(px + vec2<i32>(0, 1)).xyz - p, px.y + 1 < full.y);
    let b = select(zero, p - world_at(px - vec2<i32>(0, 1)).xyz, px.y > 0);
    let n = ddgi_depth_normal(r, l, u, b, v);
    // The neighbors `ddgi_depth_normal` took, and the two it left (a missing one is a zero difference, which
    // lies in every plane).
    let rr = dot(r, r);
    let ll = dot(l, l);
    let uu = dot(u, u);
    let bb = dot(b, b);
    let take_r = ll == 0.0 || (rr > 0.0 && rr < ll);
    let take_u = bb == 0.0 || (uu > 0.0 && uu < bb);
    let left_x = select(r, l, take_r);
    let left_y = select(u, b, take_u);
    let off = max(
        abs(dot(n, left_x)) * inverseSqrt(max(dot(left_x, left_x), 1.0e-30)),
        abs(dot(n, left_y)) * inverseSqrt(max(dot(left_y, left_y), 1.0e-30)),
    );
    // NaN compares false: a texel with a non-finite neighbor goes on to the quadrants, which refuse it.
    if (off <= SCREEN_PLANAR) {
        return n;
    }
    let taken = screen_quadrant(px, p, select(-1, 1, take_r), select(-1, 1, take_u));
    // In one plane with its diagonal (the crease or edge lies beside the pair), or no diagonal to test it by.
    if (taken.w <= SCREEN_PLANAR || taken.w >= 1.0e20) {
        return n;
    }
    var best = taken;
    for (var k = 0; k < 4; k = k + 1) {
        let q = screen_quadrant(px, p, select(-1, 1, (k & 1) == 1), select(-1, 1, (k & 2) == 2));
        if (q.w < best.w) {
            best = q;
        }
    }
    if (best.w > 0.5 * taken.w) {
        return n;
    }
    return select(best.xyz, -best.xyz, dot(best.xyz, v) < 0.0);
}

@compute @workgroup_size(8, 8)
fn screen_lookup(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= ddgi.screen.y || gid.y >= ddgi.screen.z) {
        return;
    }
    let word = ddgi.screen.x + (gid.y * ddgi.screen.y + gid.x) * DDGI_SCREEN_WORDS;
    // The pixel under the block's top-left corner (`ddgi.screen.w` pixels per texel).
    let px = vec2<i32>(gid.xy * ddgi.screen.w);
    let c = world_at(px);
    if (c.w >= 1.0) {
        // Sky: no surface here.
        for (var j = 0u; j < 6u; j = j + 1u) {
            ddgi_data[word + j] = 0u;
        }
        ddgi_data[word + 6u] = bitcast<u32>(1.0e30);
        ddgi_data[word + 7u] = bitcast<u32>(0.0);
        return;
    }
    let p = c.xyz;
    let v = normalize(screen.eye.xyz - p);
    let n = screen_normal(px, p, v);
    // The depth normal only steers the probe weights (side and bias); the irradiance itself is
    // stored toward four fixed directions so each fragment applies its own shading normal.
    //
    // The lookup's point stands a hair in front of the surface: 2 % of the finest spacing along the
    // depth normal, as a probe ray's hit is lifted (`ddgi_trace.wgsl`). A surface that lies on a
    // probe plane (a floor at y = 0) then takes the probe cell in front of it whatever the rounding
    // of its reconstructed position. With the probes weighted at the surface point (ADR-5374) that
    // rounding chose between the cell over the floor and the one under it, whose other layer is
    // buried: while the layer on the floor is not active (a probe on a surface is relocated on its
    // first update, so a renderer's first frame, and every newly scrolled-in row) the texels that
    // took the cell under the floor found no probe, fell back to the sky term, and the 2 × 2 gather
    // mixed their zero light into the texels beside them (ADR-5457).
    let q = p + n * (0.02 * ddgi.cas_a.origin.w);
    // A `let` read at constant indices: a `var` of `DdgiBasis` (or a dynamic `gi.e[j]`, which
    // naga spills to a local array) carries an explicit layout Vulkan forbids in Function storage
    // (ADR-026; the shader matrix's `ddgi_screen.wgsl` layout scan, ADR-2644).
    let gi = ddgi_irradiance_basis(q, n, v);
    var e0 = gi.e[0];
    var e1 = gi.e[1];
    var e2 = gi.e[2];
    var e3 = gi.e[3];
    var e4 = gi.e[4];
    var e5 = gi.e[5];
    if ((ddgi.ctrl.w & 4u) != 0u) {
        // Diagnostic (`DdgiSettings::screen_exact_debug`): the exact irradiance for `n`.
        let e = ddgi_irradiance(q, n, v).rgb;
        e0 = e;
        e1 = e;
        e2 = e;
        e3 = e;
        e4 = e;
        e5 = e;
    }
    ddgi_data[word] = ddgi_pack_rgb9e5(e0);
    ddgi_data[word + 1u] = ddgi_pack_rgb9e5(e1);
    ddgi_data[word + 2u] = ddgi_pack_rgb9e5(e2);
    ddgi_data[word + 3u] = ddgi_pack_rgb9e5(e3);
    ddgi_data[word + 4u] = ddgi_pack_rgb9e5(e4);
    ddgi_data[word + 5u] = ddgi_pack_rgb9e5(e5);
    ddgi_data[word + 6u] = bitcast<u32>(length(screen.eye.xyz - p));
    ddgi_data[word + 7u] = bitcast<u32>(max(gi.cov, 0.0));
}
