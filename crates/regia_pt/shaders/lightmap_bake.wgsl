// SPDX-License-Identifier: MPL-2.0
// The GPU lightmap baker (feature `lightmap-gpu`, ADR-2024, `src/lightmap/gpu.rs`): one texel's
// irradiance estimate per invocation, a port of `LightmapScene::texel_irradiance` in
// `src/lightmap.rs`. It draws the same random numbers in the same order from the same streams, so
// a texel takes the same paths as on the CPU and the two agree up to float rounding. Keep the two
// in step: `crates/regia_pt/tests/lightmap_gpu.rs` compares them.

const PI: f32 = 3.14159265358979;
const TAU: f32 = 6.28318530717959;
// The CPU's f32::INFINITY as a ray length: WGSL has no infinity literal.
const FAR: f32 = 1e30;
// Deep enough for any BVH the host lets through (`gpu.rs` checks the depth).
const STACK: u32 = 96u;
// Invocations per workgroup (`@workgroup_size` of `bake`, `WORKGROUP` in `gpu.rs`).
const GROUP: u32 = 32u;

struct Params {
    first_task: u32,
    task_count: u32,
    first_sample: u32,
    sample_count: u32,
    seed: u32,
    bounces: u32,
    direct_sun: u32,
    has_sky: u32,
    sky: vec4<f32>,
    // xyz toward the sun, w: 1 when the scene has one.
    sun_toward: vec4<f32>,
    // rgb illuminance × color, w: cos of the angular radius (2 for a delta sun).
    sun_light: vec4<f32>,
    light_count: u32,
    emitter_count: u32,
    emitter_total: f32,
    sun_lit: u32,
    // The frame's extra directional lights (ADR-7254): xyz toward each, w = 1 when on; then rgb
    // illuminance × color, w = cos of the angular radius (2 for a delta light). Lit like the sun.
    extra_toward: array<vec4<f32>, 3>,
    extra_light: array<vec4<f32>, 3>,
}

struct Node {
    min: vec3<f32>,
    a: u32,
    max: vec3<f32>,
    count: u32,
}

struct Tri {
    p0: vec4<f32>,
    p1: vec4<f32>,
    p2: vec4<f32>,
    n0: vec4<f32>,
    n1: vec4<f32>,
    n2: vec4<f32>,
    face: vec4<f32>,
    albedo: vec4<f32>,
}

struct Emitter {
    radiance: vec3<f32>,
    tri: u32,
    area: f32,
    two_sided: u32,
    cdf: f32,
    pad: u32,
}

struct Light {
    // 0 point, 1 spot, 2 sphere.
    kind: u32,
    inner_cos: f32,
    outer_cos: f32,
    radius: f32,
    position: vec4<f32>,
    direction: vec4<f32>,
    // Intensity (point, spot) or radiance (sphere).
    color: vec4<f32>,
}

struct Task {
    p: vec3<f32>,
    key: u32,
    n: vec4<f32>,
    face: vec4<f32>,
}

// What the host reads back: the running sums of one texel.
struct Accum {
    sum: vec3<f32>,
    lum_sum: f32,
    dir: vec3<f32>,
    dir_weight: f32,
    lum_sq: f32,
    back: u32,
    taken: u32,
    rays: u32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> nodes: array<Node>;
@group(0) @binding(2) var<storage, read> order: array<u32>;
@group(0) @binding(3) var<storage, read> tris: array<Tri>;
@group(0) @binding(4) var<storage, read> emitters: array<Emitter>;
@group(0) @binding(5) var<storage, read> lights: array<Light>;
@group(0) @binding(6) var<storage, read> tasks: array<Task>;
@group(0) @binding(7) var<storage, read_write> accum: array<Accum>;

var<private> rng_state: u32;
var<private> rays_traced: u32;

// The BVH traversal stacks of the workgroup's invocations, interleaved: entry `i` of invocation
// `lane` is `bvh_stack[i * GROUP + lane]`. A Function array would need an explicit-layout
// variable, which `VUID-StandaloneSpirv-None-10684` forbids (ADR-7587, ADR-7879); no barrier is
// needed, each invocation touches only its own entries. Length `STACK * GROUP` (12 KiB, under
// WebGPU's 16 KiB floor; `gpu.rs` pins the product).
var<workgroup> bvh_stack: array<u32, 3072>;
// `local_invocation_index` of this invocation (set by `bake`).
var<private> lane: u32;

fn lum(c: vec3<f32>) -> f32 {
    return 0.2126 * c.x + 0.7152 * c.y + 0.0722 * c.z;
}

// PCG hash (Jarzynski & Olano 2020).
fn pcg(v: u32) -> u32 {
    let state = v * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

fn rng_new(seed: u32, texel: u32, sample_index: u32) {
    rng_state = pcg(seed ^ pcg(texel ^ pcg(sample_index + 0x9e3779b9u)));
}

fn uniform_01() -> f32 {
    rng_state = pcg(rng_state);
    return f32(rng_state >> 8u) * (1.0 / 16777216.0);
}

// Point `i` of the first two Sobol dimensions, XOR-shifted (`sobol_02` of `lightmap.rs`, ADR-4108;
// Kollig & Keller 2002): the texel's own hemisphere directions, stratified.
fn sobol_02(i: u32, shift: vec2<u32>) -> vec2<f32> {
    let x = reverseBits(i);
    var y = 0u;
    var v = 1u << 31u;
    var k = i;
    loop {
        if (k == 0u) {
            break;
        }
        if ((k & 1u) != 0u) {
            y = y ^ v;
        }
        k = k >> 1u;
        v = v ^ (v >> 1u);
    }
    return vec2<f32>(f32((x ^ shift.x) >> 8u), f32((y ^ shift.y) >> 8u)) * (1.0 / 16777216.0);
}

// The digital shift of a texel's `sobol_02` points (`sobol_shift` of `lightmap.rs`).
fn sobol_shift(seed: u32, key: u32) -> vec2<u32> {
    let a = pcg(key ^ pcg(seed ^ 0x5bd1e995u));
    return vec2<u32>(a, pcg(a ^ 0x68e31da4u));
}

fn normalize_or(v: vec3<f32>, fallback: vec3<f32>) -> vec3<f32> {
    let len = length(v);
    if (len > 0.0 && len < 1e30) {
        return v / len;
    }
    return fallback;
}

// Orthonormal basis around unit `n` (Duff et al. 2017).
fn basis_t(n: vec3<f32>, sgn: f32, a: f32, b: f32) -> vec3<f32> {
    return vec3<f32>(1.0 + sgn * n.x * n.x * a, sgn * b, -sgn * n.x);
}

fn basis_b(n: vec3<f32>, sgn: f32, a: f32, b: f32) -> vec3<f32> {
    return vec3<f32>(b, sgn + n.y * n.y * a, -n.y);
}

fn copysign_one(z: f32) -> f32 {
    // `glam` keeps the sign of -0, as `f32::copysign` does.
    return select(1.0, -1.0, (bitcast<u32>(z) & 0x80000000u) != 0u);
}

// Cosine-weighted direction around `n` (Malley's method).
fn cosine_sample(n: vec3<f32>, u1: f32, u2: f32) -> vec3<f32> {
    let sgn = copysign_one(n.z);
    let a = -1.0 / (sgn + n.z);
    let b = n.x * n.y * a;
    let r = sqrt(u1);
    let phi = TAU * u2;
    let d = basis_t(n, sgn, a, b) * (r * cos(phi))
        + basis_b(n, sgn, a, b) * (r * sin(phi))
        + n * sqrt(max(1.0 - u1, 0.0));
    return normalize_or(d, n);
}

// Uniform direction in the cone of `cos_max` around `axis`.
fn sample_cone(axis: vec3<f32>, cos_max: f32, u1: f32, u2: f32) -> vec3<f32> {
    let sgn = copysign_one(axis.z);
    let a = -1.0 / (sgn + axis.z);
    let b = axis.x * axis.y * a;
    let c = 1.0 - u1 * (1.0 - cos_max);
    let s = sqrt(max(1.0 - c * c, 0.0));
    let phi = TAU * u2;
    let d = basis_t(axis, sgn, a, b) * (s * cos(phi))
        + basis_b(axis, sgn, a, b) * (s * sin(phi))
        + axis * c;
    return normalize_or(d, axis);
}

// A ray origin just off the surface (Wächter & Binder 2019, Ray Tracing Gems ch. 6), as
// `offset_origin` in `lightmap.rs`.
fn offset_coord(p: f32, n: f32) -> f32 {
    if (abs(p) < 1.0 / 32.0) {
        return p + (1.0 / 65536.0) * n;
    }
    let ulps = i32(256.0 * n);
    return bitcast<f32>(bitcast<i32>(p) + select(ulps, -ulps, p < 0.0));
}

fn offset_origin(p: vec3<f32>, n: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(offset_coord(p.x, n.x), offset_coord(p.y, n.y), offset_coord(p.z, n.z));
}

// The next origin of a path that hit the triangle (p0, p1, p2) at barycentrics (u, v), as
// `bounce_origin` in `lightmap.rs` (ADR-3287): the hit point moved off the triangle's edges by
// the texel border inset (`inset_from_border` with every edge a border), then offset along the
// face. On an edge the interpolated point can lie a rounding step behind the neighbour's plane,
// where a one-sided wall no longer stops the next ray.
fn bounce_origin(p0: vec3<f32>, p1: vec3<f32>, p2: vec3<f32>, u: f32, v: f32, face: vec3<f32>) -> vec3<f32> {
    let b = vec3<f32>(1.0 - u - v, u, v);
    let at = p0 * b.x + p1 * b.y + p2 * b.z;
    let area2 = length(cross(p1 - p0, p2 - p0));
    if (area2 <= 0.0) {
        return offset_origin(at, face);
    }
    let big = max(max(abs(at.x), abs(at.y)), max(abs(at.z), 1.0));
    let inset = max(512.0 * 1.1920929e-7 * big, 1e-3);
    // Edge i is the one opposite corner i.
    let len = vec3<f32>(length(p1 - p2), length(p2 - p0), length(p0 - p1));
    let least = min(inset * len / area2, vec3<f32>(1.0 / 3.0));
    let raised = (b < least) & (len > vec3<f32>(0.0));
    if (!any(raised)) {
        return offset_origin(at, face);
    }
    let zero = vec3<f32>(0.0);
    let held = dot(select(zero, least, raised), vec3<f32>(1.0));
    let kept = select(max(b, zero), zero, raised);
    let rest = dot(kept, vec3<f32>(1.0));
    let n_free = dot(select(vec3<f32>(1.0), zero, raised), vec3<f32>(1.0));
    var nb: vec3<f32>;
    if (n_free == 0.0) {
        nb = least / held;
    } else if (rest > 0.0) {
        nb = select(kept * (1.0 - held) / rest, least, raised);
    } else {
        nb = select(vec3<f32>((1.0 - held) / n_free), least, raised);
    }
    return offset_origin(p0 * nb.x + p1 * nb.y + p2 * nb.z, face);
}

// 1 / d with a large finite value in place of the CPU's infinity (a shader may not rely on one).
fn safe_inv(d: vec3<f32>) -> vec3<f32> {
    let big = vec3<f32>(1e30);
    let signed = select(big, -big, d < vec3<f32>(0.0));
    return select(1.0 / d, signed, abs(d) < vec3<f32>(1e-30));
}

fn slab(o: vec3<f32>, inv: vec3<f32>, bmin: vec3<f32>, bmax: vec3<f32>, t_max: f32) -> bool {
    let t0 = (bmin - o) * inv;
    let t1 = (bmax - o) * inv;
    let lo = min(t0, t1);
    let hi = max(t0, t1);
    let near = max(max(max(lo.x, lo.y), lo.z), 0.0);
    let far = min(min(min(hi.x, hi.y), hi.z), t_max);
    return near <= far;
}

struct Hit {
    tri: u32,
    t: f32,
    u: f32,
    v: f32,
    found: bool,
}

// Möller–Trumbore, both faces. `found` is false for a miss.
fn ray_triangle(o: vec3<f32>, d: vec3<f32>, p0: vec3<f32>, p1: vec3<f32>, p2: vec3<f32>) -> Hit {
    let miss = Hit(0u, 0.0, 0.0, 0.0, false);
    let e1 = p1 - p0;
    let e2 = p2 - p0;
    let pv = cross(d, e2);
    let det = dot(e1, pv);
    if (abs(det) < 1e-12) {
        return miss;
    }
    let inv = 1.0 / det;
    let tv = o - p0;
    // `EDGE_SLACK` in `lightmap.rs` (ADR-3287): a ray through a shared edge hits one of its
    // triangles instead of rounding past both.
    let slack = 1e-5;
    let u = dot(tv, pv) * inv;
    if (!(u >= -slack && u <= 1.0 + slack)) {
        return miss;
    }
    let qv = cross(tv, e1);
    let v = dot(d, qv) * inv;
    if (v < -slack || u + v > 1.0 + slack) {
        return miss;
    }
    let t = dot(e2, qv) * inv;
    if (t > 0.0) {
        return Hit(0u, t, u, v, true);
    }
    return miss;
}

// Closest hit along `o + t·d`, `t` in `(0, t_max)`; with `first_hit` the first one found.
// The best hit is one variable per member of `Hit` (ADR-7879).
fn intersect(o: vec3<f32>, d: vec3<f32>, t_max: f32, first_hit: bool) -> Hit {
    var best_tri = 0u;
    var best_t = 0.0;
    var best_u = 0.0;
    var best_v = 0.0;
    var best_found = false;
    var limit = t_max;
    let inv = safe_inv(d);
    var top = 1u;
    bvh_stack[lane] = 0u;
    while (top > 0u) {
        top -= 1u;
        let node = nodes[bvh_stack[top * GROUP + lane]];
        if (!slab(o, inv, node.min, node.max, limit)) {
            continue;
        }
        if (node.count > 0u) {
            for (var j = node.a; j < node.a + node.count; j++) {
                let ti = order[j];
                let tri = tris[ti];
                let h = ray_triangle(o, d, tri.p0.xyz, tri.p1.xyz, tri.p2.xyz);
                if (h.found && h.t < limit) {
                    limit = h.t;
                    best_tri = ti;
                    best_t = h.t;
                    best_u = h.u;
                    best_v = h.v;
                    best_found = h.found;
                    if (first_hit) {
                        return Hit(best_tri, best_t, best_u, best_v, best_found);
                    }
                }
            }
        } else {
            bvh_stack[top * GROUP + lane] = node.a + 1u;
            bvh_stack[(top + 1u) * GROUP + lane] = node.a;
            top += 2u;
        }
    }
    return Hit(best_tri, best_t, best_u, best_v, best_found);
}

fn visible(o: vec3<f32>, d: vec3<f32>, dist: f32) -> bool {
    rays_traced += 1u;
    return !intersect(o, d, dist * (1.0 - 1e-4), true).found;
}

struct Direct {
    e: vec3<f32>,
    // What this estimate adds to the texel's direction: luminance × direction, and luminance.
    dir: vec3<f32>,
    weight: f32,
}

// `Direct`'s running sums are one variable per member (ADR-7879): `e`, `dir`, `weight`.
fn add_light(
    out_e: ptr<function, vec3<f32>>,
    out_dir: ptr<function, vec3<f32>>,
    out_weight: ptr<function, f32>,
    c: vec3<f32>,
    dir: vec3<f32>,
) {
    *out_e += c;
    let l = lum(c);
    *out_dir += dir * l;
    *out_weight += l;
}

fn spot_falloff(to_light: vec3<f32>, spot_dir: vec3<f32>, inner_cos: f32, outer_cos: f32) -> f32 {
    let len = length(spot_dir);
    let axis = select(vec3<f32>(0.0), spot_dir / len, len > 0.0);
    let c = -dot(to_light, axis);
    return clamp((c - outer_cos) / max(inner_cos - outer_cos, 1e-4), 0.0, 1.0);
}

// The light at `o` from analytic light `i`, added to the sums (`add_light`).
fn local_light(
    i: u32,
    o: vec3<f32>,
    n: vec3<f32>,
    out_e: ptr<function, vec3<f32>>,
    out_dir: ptr<function, vec3<f32>>,
    out_weight: ptr<function, f32>,
) {
    let light = lights[i];
    var intensity = light.color.rgb;
    if (light.kind == 2u) {
        let inside = light.position.xyz - o;
        if (dot(inside, inside) <= light.radius * light.radius) {
            return;
        }
        intensity = light.color.rgb * (PI * light.radius * light.radius);
    }
    let to = light.position.xyz - o;
    let d2 = max(dot(to, to), 1e-8);
    let dist = sqrt(d2);
    let dir = to / dist;
    let c = dot(n, dir);
    if (c <= 0.0 || !visible(o, dir, dist)) {
        return;
    }
    var e = intensity * c / d2;
    if (light.kind == 1u) {
        e *= spot_falloff(dir, light.direction.xyz, light.inner_cos, light.outer_cos);
    }
    add_light(out_e, out_dir, out_weight, e, dir);
}

// The first emitter whose running power passes `pick`.
fn pick_emitter(pick: f32) -> u32 {
    var lo = 0u;
    var hi = params.emitter_count;
    while (lo < hi) {
        let mid = lo + (hi - lo) / 2u;
        if (emitters[mid].cdf <= pick) {
            lo = mid + 1u;
        } else {
            hi = mid;
        }
    }
    return min(lo, params.emitter_count - 1u);
}

// Next-event estimation at `o` (normal `n`, origin already offset).
fn direct(o: vec3<f32>, n: vec3<f32>, sun: bool) -> Direct {
    var out_e = vec3<f32>(0.0);
    var out_dir = vec3<f32>(0.0);
    var out_weight = 0.0;
    if (sun && params.sun_toward.w > 0.5) {
        let toward = params.sun_toward.xyz;
        var dir = toward;
        if (params.sun_light.w < 1.5) {
            let u1 = uniform_01();
            let u2 = uniform_01();
            dir = sample_cone(toward, params.sun_light.w, u1, u2);
        }
        let c = dot(n, dir);
        if (c > 0.0 && params.sun_lit == 1u && visible(o, dir, FAR)) {
            add_light(&out_e, &out_dir, &out_weight, params.sun_light.rgb * c, dir);
        }
    }
    // The frame's extra directional lights, as the sun (none in a one-sun scene: no samples drawn).
    if (sun) {
        for (var xi = 0u; xi < 3u; xi++) {
            let x_toward = params.extra_toward[xi];
            if (x_toward.w > 0.5) {
                var dir = x_toward.xyz;
                if (params.extra_light[xi].w < 1.5) {
                    let u1 = uniform_01();
                    let u2 = uniform_01();
                    dir = sample_cone(x_toward.xyz, params.extra_light[xi].w, u1, u2);
                }
                let c = dot(n, dir);
                if (c > 0.0 && visible(o, dir, FAR)) {
                    add_light(&out_e, &out_dir, &out_weight, params.extra_light[xi].rgb * c, dir);
                }
            }
        }
    }
    for (var i = 0u; i < params.light_count; i++) {
        local_light(i, o, n, &out_e, &out_dir, &out_weight);
    }
    if (params.emitter_count > 0u && params.emitter_total > 0.0) {
        let pick = uniform_01() * params.emitter_total;
        let em = emitters[pick_emitter(pick)];
        let two = select(1.0, 2.0, em.two_sided == 1u);
        let weight = em.area * lum(em.radiance) * two;
        let p_pick = weight / params.emitter_total;
        let t = tris[em.tri];
        let r1 = sqrt(uniform_01());
        let r2 = uniform_01();
        let q = t.p0.xyz * (1.0 - r1) + t.p1.xyz * (r1 * (1.0 - r2)) + t.p2.xyz * (r1 * r2);
        let to = q - o;
        let d2 = dot(to, to);
        if (d2 > 1e-12 && p_pick > 0.0) {
            let dist = sqrt(d2);
            let dir = to / dist;
            let cos_r = dot(n, dir);
            let facing = -dot(t.face.xyz, dir);
            let cos_l = select(facing, abs(facing), em.two_sided == 1u);
            if (cos_r > 0.0 && cos_l > 0.0 && visible(o, dir, dist)) {
                add_light(&out_e, &out_dir, &out_weight, em.radiance * (cos_r * cos_l / d2 * em.area / p_pick), dir);
            }
        }
    }
    return Direct(out_e, out_dir, out_weight);
}

// The texel's running sums are one variable per member of `Accum` (ADR-7879), written back with
// its constructor.
@compute @workgroup_size(32)
fn bake(@builtin(global_invocation_id) id: vec3<u32>, @builtin(local_invocation_index) local: u32) {
    if (id.x >= params.task_count) {
        return;
    }
    lane = local;
    let slot = params.first_task + id.x;
    let task = tasks[slot];
    let a0 = accum[slot];
    var a_sum = a0.sum;
    var a_lum_sum = a0.lum_sum;
    var a_dir = a0.dir;
    var a_dir_weight = a0.dir_weight;
    var a_lum_sq = a0.lum_sq;
    var a_back = a0.back;
    var a_taken = a0.taken;
    var a_rays = a0.rays;
    rays_traced = 0u;
    let o0 = offset_origin(task.p, task.face.xyz);
    let n0 = task.n.xyz;
    let f0 = task.face.xyz;
    let bounces = max(params.bounces, 1u);
    let shift = sobol_shift(params.seed, task.key);
    for (var s = params.first_sample; s < params.first_sample + params.sample_count; s++) {
        rng_new(params.seed, task.key, s);
        let first = direct(o0, n0, params.direct_sun == 1u);
        var e = first.e;
        var dir_sum = first.dir;
        var dir_weight = first.weight;
        var first_dir = vec3<f32>(0.0);
        var carried = vec3<f32>(0.0);
        var throughput = vec3<f32>(1.0);
        var o = o0;
        var nn = n0;
        var nf = f0;
        for (var depth = 0u; depth < bounces; depth++) {
            var u1 = uniform_01();
            var u2 = uniform_01();
            if (depth == 0u) {
                let q = sobol_02(s, shift);
                u1 = q.x;
                u2 = q.y;
            }
            let d = cosine_sample(nn, u1, u2);
            if (depth == 0u) {
                first_dir = d;
            }
            if (dot(d, nf) <= 0.0) {
                break;
            }
            rays_traced += 1u;
            let h = intersect(o, d, FAR, false);
            if (!h.found) {
                if (params.has_sky == 1u) {
                    let c = throughput * params.sky.rgb * PI;
                    e += c;
                    carried += c;
                }
                break;
            }
            let tri = tris[h.tri];
            if (dot(tri.face.xyz, d) > 0.0) {
                if (depth == 0u) {
                    a_back += 1u;
                }
                break;
            }
            if (depth + 1u >= params.bounces) {
                break;
            }
            throughput *= tri.albedo.rgb;
            if (max(max(throughput.x, throughput.y), throughput.z) <= 0.0) {
                break;
            }
            let w = 1.0 - h.u - h.v;
            var sn = normalize_or(tri.n0.xyz * w + tri.n1.xyz * h.u + tri.n2.xyz * h.v, tri.face.xyz);
            if (dot(sn, tri.face.xyz) < 0.0) {
                sn = tri.face.xyz;
            }
            o = bounce_origin(tri.p0.xyz, tri.p1.xyz, tri.p2.xyz, h.u, h.v, tri.face.xyz);
            nn = sn;
            nf = tri.face.xyz;
            let bounced = direct(o, nn, true);
            let c = throughput * bounced.e;
            e += c;
            carried += c;
        }
        let l = lum(carried);
        dir_sum += first_dir * l;
        dir_weight += l;
        a_dir += dir_sum;
        a_dir_weight += dir_weight;
        a_sum += e;
        let le = lum(e);
        a_lum_sum += le;
        a_lum_sq += le * le;
    }
    a_taken += params.sample_count;
    a_rays += rays_traced;
    accum[slot] = Accum(a_sum, a_lum_sum, a_dir, a_dir_weight, a_lum_sq, a_back, a_taken, a_rays);
}
