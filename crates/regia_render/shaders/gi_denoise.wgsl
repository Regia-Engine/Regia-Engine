// SPDX-License-Identifier: MPL-2.0
// Spatio-temporal GI denoiser, the GPU twin of the CPU reference in `src/gi_denoise.rs`
// (ADR-864). Every pass here mirrors one function there; keep them in step (the
// `gi_denoise_gpu` test compares the two on the regression scenarios).
//
// Schied et al., "Spatiotemporal Variance-Guided Filtering", HPG 2017 (temporal accumulation
// of colour and luminance moments, depth/normal history validation, the spatial variance
// fallback, the variance-guided luminance weight); Dammertz et al., "Edge-Avoiding À-Trous
// Wavelet Transform for fast Global Illumination Filtering", HPG 2010 (the B3-spline à-trous
// passes); Schied, Peters, Dachsbacher, "Gradient Estimation for Real-Time Adaptive Temporal
// Filtering", HPG 2018 (shortening the history where the shading changed: the anti-lag pass,
// here from per-square means).
//
// Buffers (one `vec4<f32>` per slot, row-major pixels):
//   frame_in: 3 per pixel  - noisy rgb, guide (normal xyz, depth), motion (xy).
//   history:  3 per pixel  - colour rgb + history length, moments (m1, m2), previous guide.
//   acc:      2 per pixel  - temporal colour rgb + length, (m1, m2, dz/dx, dz/dy).
//   src, dst: 1 per pixel  - colour rgb + variance (the à-trous ping-pong).
//   blocks:   1 per anti-lag square - the square's blend floor (x).
// Entry points, dispatched in order: reproject, antilag, temporal, variance, atrous × N,
// store_history. `reproject` leaves the reprojected history in `acc` (colour + length,
// moments + valid flag); `temporal` replaces it with the blended values and the depth gradient.

struct Params {
    // x = width, y = height, z = à-trous tap spacing, w = flags (1: history valid,
    // 2: store_history takes the colour from `src`, the first à-trous pass).
    size: vec4<u32>,
    // alpha_color, alpha_moments, max_history, depth_tolerance.
    temporal: vec4<f32>,
    // normal_tolerance, sigma_depth, depth_epsilon, sigma_normal.
    edge: vec4<f32>,
    // sigma_luminance, spatial_variance_frames, variance_radius, unused.
    extra: vec4<f32>,
    // antilag_sigma, square side, squares per row, squares per column.
    antilag: vec4<f32>,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> frame_in: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read_write> history: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read_write> acc: array<vec4<f32>>;
@group(0) @binding(4) var<storage, read> src: array<vec4<f32>>;
@group(0) @binding(5) var<storage, read_write> dst: array<vec4<f32>>;
@group(0) @binding(6) var<storage, read_write> blocks: array<vec4<f32>>;

const GRADIENT_EDGE: f32 = 0.25;
const MIN_HISTORY_WEIGHT: f32 = 0.01;
const LUMINANCE_EPSILON: f32 = 1e-10;
const MAX_INPUT: f32 = 65504.0;
const FINITE_MAX: f32 = 3.4e38;
const MIN_ANTILAG_SAMPLES: u32 = 8u;

fn width() -> i32 {
    return i32(params.size.x);
}

fn height() -> i32 {
    return i32(params.size.y);
}

fn on_screen(p: vec2<i32>) -> bool {
    return p.x >= 0 && p.y >= 0 && p.x < width() && p.y < height();
}

fn pixel(p: vec2<i32>) -> u32 {
    return u32(p.y * width() + p.x);
}

// Finite and above 0 (NaN fails the first test, infinity the second).
fn has_surface(z: f32) -> bool {
    return z > 0.0 && z <= FINITE_MAX;
}

fn sanitize(v: f32) -> f32 {
    return select(0.0, min(v, MAX_INPUT), v > 0.0 && v <= FINITE_MAX);
}

fn luminance(c: vec3<f32>) -> f32 {
    return 0.2126 * c.x + 0.7152 * c.y + 0.0722 * c.z;
}

fn guide_at(i: u32) -> vec4<f32> {
    return frame_in[3u * i + 1u];
}

// One side's depth difference (x) and whether it counts (y > 0).
fn gradient_side(p: vec2<i32>, d: vec2<i32>, z: f32, sgn: f32) -> vec2<f32> {
    let q = p + d;
    if !on_screen(q) {
        return vec2<f32>(0.0, 0.0);
    }
    let zq = guide_at(pixel(q)).w;
    if !has_surface(zq) || abs(zq - z) > GRADIENT_EDGE * z {
        return vec2<f32>(0.0, 0.0);
    }
    return vec2<f32>(sgn * (zq - z), 1.0);
}

fn gradient_pick(a: vec2<f32>, b: vec2<f32>) -> f32 {
    if a.y > 0.0 && b.y > 0.0 {
        return select(b.x, a.x, abs(a.x) <= abs(b.x));
    }
    if a.y > 0.0 {
        return a.x;
    }
    if b.y > 0.0 {
        return b.x;
    }
    return 0.0;
}

fn depth_gradient(p: vec2<i32>, z: f32) -> vec2<f32> {
    let gx = gradient_pick(gradient_side(p, vec2<i32>(1, 0), z, 1.0), gradient_side(p, vec2<i32>(-1, 0), z, -1.0));
    let gy = gradient_pick(gradient_side(p, vec2<i32>(0, 1), z, 1.0), gradient_side(p, vec2<i32>(0, -1), z, -1.0));
    return vec2<f32>(gx, gy);
}

// Depth (against the local gradient) and normal edge-stopping weight of tap `gq` at pixel
// offset `d` from the pixel with guide `gp` and depth gradient `grad`.
fn geometry_weight(gp: vec4<f32>, grad: vec2<f32>, gq: vec4<f32>, d: vec2<f32>) -> f32 {
    let expected = abs(grad.x * d.x + grad.y * d.y);
    let wz = exp(-abs(gq.w - gp.w) / (params.edge.y * expected + params.edge.z * gp.w));
    let nd = gp.x * gq.x + gp.y * gq.y + gp.z * gq.z;
    let wn = select(0.0, pow(max(nd, 1e-30), params.edge.w), nd > 0.0);
    return wz * wn;
}

// B3-spline à-trous weight by tap distance (3/8, 1/4, 1/16).
fn b3(d: i32) -> f32 {
    return select(select(0.0625, 0.25, d == 1), 0.375, d == 0);
}

// 3×3 Gaussian weight by Manhattan distance (1/4, 1/8, 1/16).
fn gauss3(d: i32) -> f32 {
    return select(select(0.0625, 0.125, d == 1), 0.25, d == 0);
}

fn input_color(i: u32) -> vec3<f32> {
    let raw = frame_in[3u * i].xyz;
    return vec3<f32>(sanitize(raw.x), sanitize(raw.y), sanitize(raw.z));
}

@compute @workgroup_size(8, 8)
fn reproject(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if !on_screen(p) {
        return;
    }
    let i = pixel(p);
    let g = guide_at(i);
    acc[2u * i] = vec4<f32>(0.0);
    acc[2u * i + 1u] = vec4<f32>(0.0);
    if !has_surface(g.w) || (params.size.w & 1u) == 0u {
        return;
    }
    let q = vec2<f32>(p) + frame_in[3u * i + 2u].xy;
    // Outside this range (or not finite) every tap is off screen.
    let near = q.x > -2.0 && q.y > -2.0 && q.x < f32(width()) + 1.0 && q.y < f32(height()) + 1.0;
    if !near {
        return;
    }
    var hc = vec3<f32>(0.0);
    var hm = vec2<f32>(0.0);
    var hl = 0.0;
    var ws = 0.0;
    let base = floor(q);
    let fr = q - base;
    let b = vec2<i32>(base);
    for (var k = 0; k < 4; k++) {
        let o = vec2<i32>(k & 1, k >> 1u);
        let wt = select(1.0 - fr.x, fr.x, o.x == 1) * select(1.0 - fr.y, fr.y, o.y == 1);
        let t = b + o;
        if wt <= 0.0 || !on_screen(t) {
            continue;
        }
        let j = pixel(t);
        let h_color = history[3u * j];
        let h_moments = history[3u * j + 1u];
        let pg = history[3u * j + 2u];
        let normal_dot = pg.x * g.x + pg.y * g.y + pg.z * g.z;
        if h_color.w <= 0.0 || !has_surface(pg.w) || abs(pg.w - g.w) > params.temporal.w * g.w
            || normal_dot < params.edge.x {
            continue;
        }
        hc += wt * h_color.xyz;
        hm += wt * h_moments.xy;
        hl += wt * h_color.w;
        ws += wt;
    }
    if ws >= MIN_HISTORY_WEIGHT {
        let inv = 1.0 / ws;
        acc[2u * i] = vec4<f32>(hc * inv, hl * inv);
        acc[2u * i + 1u] = vec4<f32>(hm * inv, 1.0, 0.0);
    }
}

// One invocation per anti-lag square: the blend floor from this frame's mean input luminance
// against the reprojected history's, where they differ by more than `antilag_sigma` standard
// errors of the input mean.
@compute @workgroup_size(8, 8)
fn antilag(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cell = vec2<i32>(gid.xy);
    let side = i32(params.antilag.y);
    let cols = i32(params.antilag.z);
    let rows = i32(params.antilag.w);
    if cell.x >= cols || cell.y >= rows {
        return;
    }
    var count = 0u;
    var sum_c = 0.0;
    var sum_cc = 0.0;
    var sum_h = 0.0;
    for (var dy = 0; dy < side; dy++) {
        for (var dx = 0; dx < side; dx++) {
            let q = cell * side + vec2<i32>(dx, dy);
            if !on_screen(q) {
                continue;
            }
            let j = pixel(q);
            if acc[2u * j + 1u].z <= 0.0 || !has_surface(guide_at(j).w) {
                continue;
            }
            let lc = luminance(input_color(j));
            sum_c += lc;
            sum_cc += lc * lc;
            sum_h += luminance(acc[2u * j].xyz);
            count += 1u;
        }
    }
    var floor_weight = 0.0;
    if params.antilag.x > 0.0 && count >= MIN_ANTILAG_SAMPLES {
        let n = f32(count);
        let mc = sum_c / n;
        let mh = sum_h / n;
        let se = sqrt(max(sum_cc / n - mc * mc, 0.0) / n);
        let d = abs(mc - mh);
        let top = max(mc, mh);
        if d > params.antilag.x * se && top > 0.0 {
            floor_weight = min(d / top, 1.0);
        }
    }
    blocks[u32(cell.y * cols + cell.x)] = vec4<f32>(floor_weight, 0.0, 0.0, 0.0);
}

@compute @workgroup_size(8, 8)
fn temporal(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if !on_screen(p) {
        return;
    }
    let i = pixel(p);
    let g = guide_at(i);
    if !has_surface(g.w) {
        acc[2u * i] = vec4<f32>(0.0);
        acc[2u * i + 1u] = vec4<f32>(0.0);
        return;
    }
    let c = input_color(i);
    let l = luminance(c);
    let r0 = acc[2u * i];
    let r1 = acc[2u * i + 1u];
    var color = c;
    var moments = vec2<f32>(l, l * l);
    var len = 1.0;
    if r1.z > 0.0 {
        let side = i32(params.antilag.y);
        let cols = i32(params.antilag.z);
        let antilag_floor = blocks[u32((p.y / side) * cols + p.x / side)].x;
        len = min(r0.w + 1.0, params.temporal.z);
        if antilag_floor > 0.0 {
            len = min(len, max(1.0 / antilag_floor, 1.0));
        }
        let a = max(max(1.0 / len, params.temporal.x), antilag_floor);
        let am = max(max(1.0 / len, params.temporal.y), antilag_floor);
        color = r0.xyz + (c - r0.xyz) * a;
        moments = vec2<f32>(r1.x + (l - r1.x) * am, r1.y + (l * l - r1.y) * am);
    }
    acc[2u * i] = vec4<f32>(color, len);
    acc[2u * i + 1u] = vec4<f32>(moments, depth_gradient(p, g.w));
}

@compute @workgroup_size(8, 8)
fn variance(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if !on_screen(p) {
        return;
    }
    let i = pixel(p);
    let g = guide_at(i);
    if !has_surface(g.w) {
        dst[i] = vec4<f32>(0.0);
        return;
    }
    let a0 = acc[2u * i];
    let a1 = acc[2u * i + 1u];
    let len = a0.w;
    let temporal_variance = max(a1.y - a1.x * a1.x, 0.0);
    if len >= params.extra.y {
        dst[i] = vec4<f32>(a0.xyz, temporal_variance);
        return;
    }
    let r = i32(params.extra.z);
    var sc = vec3<f32>(0.0);
    var s1 = 0.0;
    var s2 = 0.0;
    var sw = 0.0;
    for (var dy = -r; dy <= r; dy++) {
        for (var dx = -r; dx <= r; dx++) {
            let q = p + vec2<i32>(dx, dy);
            if !on_screen(q) {
                continue;
            }
            let j = pixel(q);
            let gq = guide_at(j);
            if !has_surface(gq.w) {
                continue;
            }
            let w = geometry_weight(g, a1.zw, gq, vec2<f32>(f32(dx), f32(dy)));
            let c0 = acc[2u * j];
            let c1 = acc[2u * j + 1u];
            sc += w * c0.xyz;
            s1 += w * c1.x;
            s2 += w * c1.y;
            sw += w;
        }
    }
    if sw <= 0.0 {
        dst[i] = vec4<f32>(a0.xyz, temporal_variance);
        return;
    }
    let inv = 1.0 / sw;
    let mean = s1 * inv;
    let v = max(s2 * inv - mean * mean, 0.0) * (params.extra.y / max(len, 1.0));
    dst[i] = vec4<f32>(sc * inv, v);
}

@compute @workgroup_size(8, 8)
fn atrous(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if !on_screen(p) {
        return;
    }
    let i = pixel(p);
    let g = guide_at(i);
    if !has_surface(g.w) {
        dst[i] = vec4<f32>(0.0);
        return;
    }
    let grad = acc[2u * i + 1u].zw;
    let center = src[i];
    // 3×3 Gaussian of the variance over the neighbours on the same surface.
    var gv = 0.0;
    var gw = 0.0;
    for (var dy = -1; dy <= 1; dy++) {
        for (var dx = -1; dx <= 1; dx++) {
            let q = p + vec2<i32>(dx, dy);
            if !on_screen(q) {
                continue;
            }
            let j = pixel(q);
            let gq = guide_at(j);
            if !has_surface(gq.w) {
                continue;
            }
            let k = gauss3(abs(dx) + abs(dy)) * geometry_weight(g, grad, gq, vec2<f32>(f32(dx), f32(dy)));
            gv += k * src[j].w;
            gw += k;
        }
    }
    let blurred = select(center.w, gv / gw, gw > 0.0);
    let lp = luminance(center.xyz);
    let dl = params.extra.x * sqrt(max(blurred, 0.0)) + LUMINANCE_EPSILON;
    let stride = i32(params.size.z);
    var sc = vec3<f32>(0.0);
    var sv = 0.0;
    var sw = 0.0;
    for (var dy = -2; dy <= 2; dy++) {
        for (var dx = -2; dx <= 2; dx++) {
            let q = p + vec2<i32>(dx, dy) * stride;
            if !on_screen(q) {
                continue;
            }
            let j = pixel(q);
            let gq = guide_at(j);
            if !has_surface(gq.w) {
                continue;
            }
            let s = src[j];
            let h = b3(abs(dx)) * b3(abs(dy));
            let wg = geometry_weight(g, grad, gq, vec2<f32>(f32(dx * stride), f32(dy * stride)));
            let wl = exp(-abs(luminance(s.xyz) - lp) / dl);
            let w = h * wg * wl;
            sc += w * s.xyz;
            sv += w * w * s.w;
            sw += w;
        }
    }
    if sw <= 0.0 {
        dst[i] = center;
        return;
    }
    let inv = 1.0 / sw;
    dst[i] = vec4<f32>(sc * inv, sv * inv * inv);
}

@compute @workgroup_size(8, 8)
fn store_history(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if !on_screen(p) {
        return;
    }
    let i = pixel(p);
    let a0 = acc[2u * i];
    var color = a0.xyz;
    if (params.size.w & 2u) != 0u {
        color = src[i].xyz;
    }
    history[3u * i] = vec4<f32>(color, a0.w);
    history[3u * i + 1u] = vec4<f32>(acc[2u * i + 1u].xy, 0.0, 0.0);
    history[3u * i + 2u] = guide_at(i);
}
