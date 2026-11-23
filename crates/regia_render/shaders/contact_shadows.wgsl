// SPDX-License-Identifier: MPL-2.0
// Screen-space contact shadows (`contact_shadows.rs`, docs/provenance.md).
//
// Regia Engine's own design from the public description of line-coherent screen-space shadows
// (Bend Studio, "Days Gone" screen-space shadows, SIGGRAPH 2023 Advances talk and blog) and
// the general screen-space shadow ray-march literature. No third-party code.
//
// A ray from a surface toward the sun projects onto a screen line through the sun's screen
// point P_L (the vanishing point of the sun direction). Every workgroup takes 64 consecutive
// pixels of one such line, loads the depths along it (plus STEPS pixels further toward the
// light and one behind the chunk) into workgroup memory once, and each thread marches its own
// ray through that shared strip. The CPU (`contact_shadows::plan`) splits the screen into four sectors around P_L and
// each sector into 64-pixel chunks of major-axis distance, and lists the lines of every chunk
// so that every pixel is written exactly once (the lowest-|e| line that lands on it).
//
// Depth is Regia Engine's DirectX-convention perspective (0 = near, 1 = far, cleared to 1).
// Output: R = sun visibility in 0..1 (1 = lit), multiplied into the sun term only.

const GROUP: u32 = 64u;
// Pixels marched toward the light. Must match `contact_shadows::STEPS`.
const STEPS: u32 = 48u;
// GROUP + STEPS + 1: the chunk, the march ahead of it and one pixel behind it (the receiver
// plane of the chunk's last pixel).
const SLOTS: u32 = 113u;
// Linear depth marking "no surface" (sky, off screen, behind the light point).
const NONE: f32 = 1e30;

struct Sector {
    // x = major axis (0 = x, 1 = y), y = major sign (±1), z = c0 (major pixel of j = 0),
    // w = integer part of the light point's minor coordinate.
    ints: vec4<i32>,
    // x = delta (major distance of j = 0's pixel centre from the light point), y = fraction of
    // the light point's minor coordinate.
    floats: vec4<f32>,
}

struct Params {
    // x, y = depth size (pixels), z = near, w = far.
    screen: vec4<f32>,
    // x = 1 when rays march toward the light point (sun in front of the camera), 0 away from
    // it; y = total workgroups; z = chunk count; w = workgroups per grid row.
    dispatch: vec4<u32>,
    // Light point integer parts (x, y); z = pixels marched this frame (<= STEPS).
    light_i: vec4<i32>,
    // Light point fractions (x, y); z = tolerated fraction of the row gap of interpolated
    // strip samples (`INTERP_ERROR`); w = largest assumed thickness (m, `THICKNESS_MAX_M`).
    light_f: vec4<f32>,
    // x = relative bias, y = relative thickness, z = minimum thickness (m), w = strength.
    tune: vec4<f32>,
    // x = distance fade start (m), y = fade end (m), z = edge threshold (relative), w = hard steps.
    tune2: vec4<f32>,
    sectors: array<Sector, 4>,
}

struct Chunk {
    sector: u32,
    k: i32,
    e_lo: i32,
    start: u32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var depth_tex: texture_depth_2d;
@group(0) @binding(2) var<storage, read> chunks: array<Chunk>;
@group(0) @binding(3) var out_tex: texture_storage_2d<rgba8unorm, write>;

var<workgroup> wg_chunk: u32;
var<workgroup> strip: array<f32, SLOTS>;
// How far the strip depth of each slot may be from the true depth along the line (metres):
// the gap between the two rows it was taken from, in full across an edge and a fraction of it
// when interpolated (curved surfaces seen at grazing angles).
var<workgroup> strip_err: array<f32, SLOTS>;

fn linear_depth(d: f32) -> f32 {
    let n = params.screen.z;
    let f = params.screen.w;
    return n * f / max(f - d * (f - n), 1e-6);
}

fn size_of_axis(axis: i32) -> i32 {
    return select(i32(params.screen.x), i32(params.screen.y), axis == 1);
}

// Linear depth at (major, minor) along the sector's axes; NONE off screen or for sky.
fn texel_depth(axis: i32, major: i32, minor: i32) -> f32 {
    if (minor < 0 || minor >= size_of_axis(1 - axis)) {
        return NONE;
    }
    let p = select(vec2<i32>(major, minor), vec2<i32>(minor, major), axis == 1);
    let d = textureLoad(depth_tex, p, 0);
    if (d >= 1.0) {
        return NONE;
    }
    return linear_depth(d);
}

// Major-axis distance M of chunk k's far end (one pixel past it): lines of the chunk are one
// pixel apart there, so no pixel of the chunk falls between two lines.
fn chunk_span(sec: Sector, k: i32) -> f32 {
    return f32(64 * k + 65) + sec.floats.x;
}

// Minor coordinate of line `e` at slot `lp` (pixel j = 64k + lp), relative to the light
// point's integer part plus e: y = L + e·m/M = (L_i + e) + (L_f − e·(M − m)/M), with M − m =
// 65 − lp exact, so the float term stays small even when the light point is far off screen.
fn line_offset(sec: Sector, k: i32, e: i32, lp: i32) -> f32 {
    return sec.floats.y - f32(e) * f32(65 - lp) / chunk_span(sec, k);
}

// Pixel row (minor index) line `e` covers at slot `lp`.
fn line_row(sec: Sector, k: i32, e: i32, lp: i32) -> i32 {
    return sec.ints.w + e + i32(floor(line_offset(sec, k, e, lp)));
}

// Depth of the line at slot `lp` (x) and its uncertainty (y): interpolated across the two
// minor neighbours in 1/z (exact on planes, so flat ground does not shadow itself), or the
// nearer one across an edge.
fn strip_depth(sec: Sector, k: i32, e: i32, lp: i32) -> vec2<f32> {
    let j = 64 * k + lp;
    if (j < 0) {
        return vec2<f32>(NONE, 0.0);
    }
    let axis = sec.ints.x;
    let major = sec.ints.z + sec.ints.y * j;
    if (major < 0 || major >= size_of_axis(axis)) {
        return vec2<f32>(NONE, 0.0);
    }
    let t = line_offset(sec, k, e, lp) - 0.5;
    let ft = floor(t);
    let w = t - ft;
    let r0 = sec.ints.w + e + i32(ft);
    let za = texel_depth(axis, major, r0);
    let zb = texel_depth(axis, major, r0 + 1);
    let near_one = select(zb, za, w < 0.5);
    if (za >= NONE || zb >= NONE) {
        return vec2<f32>(near_one, 0.0);
    }
    let step = abs(za - zb);
    if (step > params.tune2.z * min(za, zb)) {
        return vec2<f32>(near_one, step);
    }
    return vec2<f32>(1.0 / mix(1.0 / za, 1.0 / zb, w), step * params.light_f.z);
}

// The sector a pixel belongs to: x± when |dy| <= |dx| (x− takes dx = 0), else y±. The same
// arithmetic for every sector, so each pixel has exactly one.
fn classify(p: vec2<i32>) -> u32 {
    let dx = f32(p.x - params.light_i.x) + (0.5 - params.light_f.x);
    let dy = f32(p.y - params.light_i.y) + (0.5 - params.light_f.y);
    if (abs(dy) <= abs(dx)) {
        return select(1u, 0u, dx > 0.0);
    }
    return select(3u, 2u, dy > 0.0);
}

@compute @workgroup_size(64)
fn cs_main(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_index) lid: u32) {
    let wid = wg.y * params.dispatch.w + wg.x;
    if (lid == 0u) {
        // The last chunk whose first workgroup is at or before this one.
        var lo = 0u;
        var hi = max(params.dispatch.z, 1u);
        while (hi - lo > 1u) {
            let mid = (lo + hi) / 2u;
            if (chunks[mid].start <= wid) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        wg_chunk = lo;
    }
    let ci = workgroupUniformLoad(&wg_chunk);
    let rec = chunks[ci];
    let sec = params.sectors[min(rec.sector, 3u)];
    let e = rec.e_lo + i32(wid) - i32(rec.start);
    let k = rec.k;
    let toward = params.dispatch.x == 1u;
    // The strip runs from STEPS pixels before the chunk (toward the light point) to one past
    // it, or from one before the chunk to STEPS pixels past it (away from the light point).
    let base = select(-1, -i32(STEPS), toward);
    let first = strip_depth(sec, k, e, base + i32(lid));
    strip[lid] = first.x;
    strip_err[lid] = first.y;
    if (lid + GROUP < SLOTS) {
        let second = strip_depth(sec, k, e, base + i32(lid + GROUP));
        strip[lid + GROUP] = second.x;
        strip_err[lid + GROUP] = second.y;
    }
    workgroupBarrier();
    if (wid >= params.dispatch.y) {
        return;
    }

    let local = i32(lid);
    let axis = sec.ints.x;
    let j = 64 * k + local;
    let major = sec.ints.z + sec.ints.y * j;
    let row = line_row(sec, k, e, local);
    let p = select(vec2<i32>(major, row), vec2<i32>(row, major), axis == 1);
    let size = vec2<i32>(i32(params.screen.x), i32(params.screen.y));
    if (p.x < 0 || p.y < 0 || p.x >= size.x || p.y >= size.y) {
        return;
    }
    if (classify(p) != rec.sector) {
        return;
    }
    // Lines are ordered along the minor axis; the one closest to e = 0 among those that land
    // on this pixel writes it.
    if (e > 0 && line_row(sec, k, e - 1, local) == row) {
        return;
    }
    if (e < 0 && line_row(sec, k, e + 1, local) == row) {
        return;
    }

    let s0 = local - base;
    // The ray starts from the pixel's own depth. The strip samples the line at its sub-pixel
    // position, which at a silhouette can be the neighbouring row: another, farther surface,
    // whose ray would pass behind this pixel's own nearer surface and dot silhouette rims
    // with false shadow. How far the line's sample is from the pixel's depth joins the
    // receiver's uncertainty (on a plane that is the sub-pixel offset's share of the slope).
    let own = textureLoad(depth_tex, p, 0);
    let z_line = strip[s0];
    var z0 = NONE;
    if (own < 1.0) {
        z0 = linear_depth(own);
    }
    var lit = 1.0;
    if (z0 < NONE) {
        // Along the screen line, 1/z of the ray is affine in the fraction u of the way to the
        // light point: 1/z = (1 − u)/z0, with u = ±i/m after i pixels (m = the pixel's
        // major-axis distance from the light point).
        let m = f32(j) + sec.floats.x;
        let steps = u32(clamp(params.light_i.z, 1, i32(STEPS)));
        let hard = params.tune2.w;
        let err0 = strip_err[s0] + select(abs(z_line - z0), 0.0, z_line >= NONE);
        // March direction in strip slots: toward the light point is toward lower slots.
        let dir = select(1, -1, toward);
        // The receiver's own surface along the line: 1/z of a plane is affine along a screen
        // line, so its 1/z one pixel further is 1/zl + g, anchored on the line's own sample
        // zl (the samples ahead are line samples too). g comes from the pixel behind the
        // receiver (away from the light, where its own surface continues even at a contact)
        // or, across an edge there, the pixel ahead; with neither on the same surface the
        // receiver is a one-pixel sliver and casts nothing onto itself either way.
        let zl = select(z_line, z0, z_line >= NONE);
        let inv0 = 1.0 / zl;
        let zb = strip[s0 - dir];
        let zf = strip[s0 + dir];
        let edge = params.tune2.z;
        var g = 0.0;
        var has_plane = false;
        if (zb < NONE && abs(zb - zl) <= edge * min(zb, zl)) {
            g = inv0 - 1.0 / zb;
            has_plane = true;
        } else if (zf < NONE && abs(zf - zl) <= edge * min(zf, zl)) {
            g = 1.0 / zf - inv0;
            has_plane = true;
        }
        var occlusion = 0.0;
        for (var i = 1u; i <= steps; i++) {
            let fi = f32(i);
            var zr: f32;
            if (toward) {
                if (fi >= m) {
                    break;
                }
                zr = z0 * m / (m - fi);
            } else {
                zr = z0 * m / (m + fi);
            }
            let si = s0 + dir * i32(i);
            let zs = strip[si];
            let gap = zr - zs;
            // Relative to depth, at most `THICKNESS_MAX_M` (ADR-1449: far away a relative
            // thickness made twig cards metres in front of the ground solid), at least the floor.
            let thickness = max(min(params.tune.y * zr, params.light_f.w), params.tune.z);
            // What the strip depths of the sample and the receiver may be off by (grazing
            // ground, silhouettes), on top of the bias.
            let slack = params.tune.x * zr + strip_err[si] + err0;
            // An occluder rises off the receiver's surface toward the camera: a sample on
            // that surface (or behind it) is the receiver itself, however the thickness
            // heuristic places it relative to the ray. A sunlit surface facing away from the
            // camera (inside-out meshes, smoothed normals past a silhouette) otherwise darkens
            // itself, because its nearer part lies in front of every ray leaving it.
            var off_surface = true;
            if (has_plane) {
                let inv_p = inv0 + g * fi;
                // A plane receding past the horizon (1/z <= 0) is behind everything.
                if (inv_p > 0.0) {
                    let zp = 1.0 / inv_p;
                    off_surface = zp - zs > params.tune.x * zp + strip_err[si] + err0;
                }
            }
            // The sample must be in front of the ray by more than the slack, closer than the
            // assumed occluder thickness, and off the receiver's own surface.
            if (gap > slack && gap < thickness && off_surface) {
                // Hard near the caster, fading toward the end of the march.
                let weight = clamp((f32(steps) - fi) / max(f32(steps) - hard, 1.0), 0.0, 1.0);
                occlusion = max(occlusion, select(weight, 1.0, fi <= hard));
            }
        }
        let fade = 1.0 - smoothstep(params.tune2.x, params.tune2.y, z0);
        lit = 1.0 - occlusion * params.tune.w * fade;
    }
    textureStore(out_tex, p, vec4<f32>(lit, 0.0, 0.0, 1.0));
}
