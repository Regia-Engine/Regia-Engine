// SPDX-License-Identifier: MPL-2.0
// Stochastic many-light sampling for direct lighting (ADR-2470): reservoir-based spatiotemporal
// importance resampling (Bitterli, Wyman, Pharr, Shirley, Lefohn, Jarosz, SIGGRAPH 2020).
// The GPU twin of `restir_di/cpu.rs`, pass for pass, with the same random streams.
//
// Passes (one dispatch each, 8 x 8 pixels per group):
//   initial_temporal  surfaces into the guide; candidates from the pixel's light list; the
//                     previous reservoir along the motion vector, rejected at disocclusions
//   spatial           neighbor reservoirs within a disk, rejected across depth and normal
//                     edges; unbiased 1/Z normalization; the result is kept for next frame
//   shade             the chosen light's contribution times its weight (optionally traced
//                     against the depth buffer)
//   reference         brute force over the pixel's list (the evaluation view)

struct Params {
    // width, height, light count, frame
    size: vec4<u32>,
    // candidates, spatial taps, flags (1 history, 2 clusters, 4 trace), max lights per cluster
    cfg: vec4<u32>,
    // depth tolerance, normal tolerance, history cap, spatial radius (px)
    tol: vec4<f32>,
    // eye xyz, trace thickness
    eye: vec4<f32>,
    // forward xyz, unused
    forward: vec4<f32>,
    // near, far, unused, unused
    planes: vec4<f32>,
    // seed, trace steps, unused, unused
    extra: vec4<u32>,
    world_from_clip: mat4x4<f32>,
    clip_from_world: mat4x4<f32>,
}

struct GpuLight {
    position_kind: vec4<f32>,
    color_intensity: vec4<f32>,
    direction_radius: vec4<f32>,
    cone_shadow: vec4<f32>,
}

// The chosen light, its contribution weight W and the candidate count M.
struct Reservoir {
    light: u32,
    w: f32,
    m: f32,
    pad: f32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var depth_tex: texture_2d<f32>;
@group(0) @binding(2) var normal_tex: texture_2d<f32>;
@group(0) @binding(3) var albedo_tex: texture_2d<f32>;
@group(0) @binding(4) var motion_tex: texture_2d<f32>;
@group(0) @binding(5) var<storage, read> lights: array<GpuLight>;
@group(0) @binding(6) var<storage, read> cluster_counts: array<u32>;
@group(0) @binding(7) var<storage, read> cluster_indices: array<u32>;
// Two vec4 per pixel: position + view depth (0 = none), normal + 0.
@group(0) @binding(8) var<storage, read_write> guide_cur: array<vec4<f32>>;
@group(0) @binding(9) var<storage, read> guide_prev: array<vec4<f32>>;
@group(0) @binding(10) var<storage, read_write> res_tmp: array<Reservoir>;
@group(0) @binding(11) var<storage, read_write> res_cur: array<Reservoir>;
@group(0) @binding(12) var<storage, read> res_prev: array<Reservoir>;
@group(0) @binding(13) var out_tex: texture_storage_2d<rgba16float, write>;

const NONE: u32 = 0xffffffffu;
const PI: f32 = 3.14159265358979;
const TAU: f32 = 6.28318530717959;
const FLAG_HISTORY: u32 = 1u;
const FLAG_CLUSTERS: u32 = 2u;
const FLAG_TRACE: u32 = 4u;
const STREAM_INITIAL: u32 = 0u;
const STREAM_TEMPORAL: u32 = 1u;
const STREAM_SPATIAL: u32 = 2u;
const MAX_TAPS: u32 = 8u;

fn pcg(v: u32) -> u32 {
    let state = v * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

fn rng_seed(pixel: u32, stream: u32) -> u32 {
    return pcg(pixel ^ pcg(params.size.w ^ pcg(stream ^ pcg(params.extra.x))));
}

fn rng_next(state: ptr<function, u32>) -> f32 {
    *state = pcg(*state);
    return f32(*state >> 8u) * (1.0 / 16777216.0);
}

fn luminance(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
}

struct Surface {
    position: vec3<f32>,
    normal: vec3<f32>,
    depth: f32,
}

fn pixel_uv(p: vec2<u32>) -> vec2<f32> {
    return (vec2<f32>(p) + vec2<f32>(0.5)) / vec2<f32>(f32(params.size.x), f32(params.size.y));
}

// The view ray through `uv` scaled to view depth `depth` (`restir_di::world_at_uv`).
fn world_at(uv: vec2<f32>, depth: f32) -> vec3<f32> {
    let ndc = vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    let a4 = params.world_from_clip * vec4<f32>(ndc, 0.25, 1.0);
    let b4 = params.world_from_clip * vec4<f32>(ndc, 0.75, 1.0);
    let a = a4.xyz / a4.w;
    let b = b4.xyz / b4.w;
    let fwd = params.forward.xyz;
    var dir = b - a;
    let len = length(dir);
    dir = select(fwd, dir / len, len > 1e-12);
    if (dot(dir, fwd) < 0.0) {
        dir = -dir;
    }
    return params.eye.xyz + dir * (depth / max(dot(dir, fwd), 1e-4));
}

// The guide entry of pixel `i`; depth 0 means no surface there. Returned by value: the SPIR-V gate
// refuses function-scope variables of struct and array types (VUID-StandaloneSpirv-None-10684).
fn guide_surface(i: u32, prev: bool) -> Surface {
    var a: vec4<f32>;
    var b: vec4<f32>;
    if (prev) {
        a = guide_prev[2u * i];
        b = guide_prev[2u * i + 1u];
    } else {
        a = guide_cur[2u * i];
        b = guide_cur[2u * i + 1u];
    }
    return Surface(a.xyz, b.xyz, a.w);
}

fn similar(p: Surface, q: Surface) -> bool {
    return distance(p.position, q.position) <= params.tol.x * p.depth
        && dot(p.normal, q.normal) >= params.tol.y;
}

fn distance_attenuation(d_in: f32, radius: f32, source_radius: f32) -> f32 {
    let r = max(radius, 1e-4);
    let d = max(d_in, 0.0);
    let x = d / r;
    let num = clamp(1.0 - x * x * x * x, 0.0, 1.0);
    return (num * num) / (d * d + max(source_radius * source_radius, 1.0));
}

fn spot_attenuation(l: vec3<f32>, spot_dir: vec3<f32>, inner_cos: f32, outer_cos: f32) -> f32 {
    let sl = length(spot_dir);
    let sd = select(vec3<f32>(0.0, -1.0, 0.0), spot_dir / sl, sl > 1e-12);
    let cos_angle = dot(-sd, l);
    let denom = max(inner_cos - outer_cos, 1e-4);
    return clamp((cos_angle - outer_cos) / denom, 0.0, 1.0);
}

struct Reach {
    l: vec3<f32>,
    att: f32,
    nol: f32,
    dist: f32,
}

fn light_kind(li: u32) -> u32 {
    return u32(max(lights[li].position_kind.w + 0.5, 0.0));
}

// Direction, attenuation and n.l of light `li` at `s`; att = 0 when it does not reach.
fn reach(li: u32, s: Surface) -> Reach {
    let r = reach_position(li, s);
    if (r.nol <= 0.0) {
        return Reach(vec3<f32>(0.0), 0.0, 0.0, 0.0);
    }
    return r;
}

// `reach` without the n.l test (`restir_di::reach_position`, ADR-2652): the target's support.
fn reach_position(li: u32, s: Surface) -> Reach {
    let unreached = Reach(vec3<f32>(0.0), 0.0, 0.0, 0.0);
    if (li >= params.size.z) {
        return unreached;
    }
    let kind = light_kind(li);
    if (kind != 1u && kind != 2u) {
        return unreached;
    }
    let to = lights[li].position_kind.xyz - s.position;
    let d = length(to);
    var att = distance_attenuation(d, lights[li].direction_radius.w, lights[li].cone_shadow.w);
    if (att <= 0.0) {
        return unreached;
    }
    let l = to / max(d, 1e-4);
    if (kind == 2u) {
        att = att * spot_attenuation(l, lights[li].direction_radius.xyz, lights[li].cone_shadow.x, lights[li].cone_shadow.y);
        if (att <= 0.0) {
            return unreached;
        }
    }
    return Reach(l, att, dot(s.normal, l), d);
}

struct Mat {
    albedo: vec3<f32>,
    roughness: f32,
    metallic: f32,
}

fn material_at(p: vec2<u32>) -> Mat {
    let ar = textureLoad(albedo_tex, vec2<i32>(p), 0);
    let rough = textureLoad(normal_tex, vec2<i32>(p), 0).w;
    return Mat(ar.rgb, rough, ar.a);
}

// `restir_di::SUPPORT_WEIGHT`.
const SUPPORT_WEIGHT: f32 = 0.001;

// Luminance arriving at the point (no cosine; `restir_di::arriving`).
fn arriving_of(li: u32, r: Reach) -> f32 {
    let c = lights[li].color_intensity;
    return luminance(c.rgb) * c.w * r.att;
}

// The target's support (`restir_di::supports`).
fn support_at(li: u32, s: Surface) -> bool {
    if (li == NONE) {
        return false;
    }
    let r = reach_position(li, s);
    return r.att > 0.0 && arriving_of(li, r) > 0.0;
}

// Lambert + GGX (height-correlated Smith, Schlick) times the light (`restir_di::shade`).
fn radiance(li: u32, s: Surface, view: vec3<f32>, mat: Mat, r: Reach) -> vec3<f32> {
    let n = s.normal;
    let l = r.l;
    let nol = r.nol;
    let nov = max(dot(n, view), 1e-4);
    let hv = l + view;
    let hl = length(hv);
    let h = select(n, hv / hl, hl > 1e-12);
    let noh = max(dot(n, h), 0.0);
    let loh = max(dot(l, h), 0.0);
    let rough = clamp(mat.roughness, 0.045, 1.0);
    let a = rough * rough;
    let a2 = a * a;
    let dd = noh * noh * (a2 - 1.0) + 1.0;
    let d = a2 / max(PI * dd * dd, 1e-8);
    let gv = nol * sqrt(nov * nov * (1.0 - a2) + a2);
    let gl = nov * sqrt(nol * nol * (1.0 - a2) + a2);
    let vis = 0.5 / max(gv + gl, 1e-6);
    let fc = pow(1.0 - loh, 5.0);
    let m = clamp(mat.metallic, 0.0, 1.0);
    let albedo = clamp(mat.albedo, vec3<f32>(0.0), vec3<f32>(1.0));
    let f0 = vec3<f32>(0.04 * (1.0 - m)) + albedo * m;
    let f = f0 + (vec3<f32>(1.0) - f0) * fc;
    let diffuse = albedo * (1.0 - m) / PI;
    let le = lights[li].color_intensity.rgb * lights[li].color_intensity.w;
    return (diffuse + vec3<f32>(d * vis) * f) * le * r.att * nol;
}

fn shade(li: u32, s: Surface, view: vec3<f32>, mat: Mat) -> vec3<f32> {
    if (li == NONE) {
        return vec3<f32>(0.0);
    }
    let r = reach(li, s);
    if (r.att <= 0.0) {
        return vec3<f32>(0.0);
    }
    return radiance(li, s, view, mat, r);
}

// The target function (`restir_di::target`): shaded luminance plus a small term for the light
// arriving at the point, so every light that reaches the position stays possible.
fn target_at(li: u32, s: Surface, view: vec3<f32>, mat: Mat) -> f32 {
    if (li == NONE) {
        return 0.0;
    }
    let r = reach_position(li, s);
    if (r.att <= 0.0) {
        return 0.0;
    }
    let arrive = arriving_of(li, r);
    if (arrive <= 0.0) {
        return 0.0;
    }
    var shaded = 0.0;
    if (r.nol > 0.0) {
        shaded = luminance(radiance(li, s, view, mat, r));
    }
    return shaded + SUPPORT_WEIGHT * arrive;
}

fn cluster_of(uv: vec2<f32>, depth: f32) -> u32 {
    let cx = min(u32(uv.x * 16.0), 15u);
    let cy = min(u32(uv.y * 9.0), 8u);
    let near = params.planes.x;
    let far = params.planes.y;
    var cz = 0u;
    if (depth >= far) {
        cz = 23u;
    } else if (depth > near) {
        let t = log(depth / near) / log(far / near);
        cz = min(u32(floor(t * 24.0)), 23u);
    }
    return cx + 16u * (cy + 9u * cz);
}

fn in_bounds(p: vec2<u32>) -> bool {
    return p.x < params.size.x && p.y < params.size.y;
}

@compute @workgroup_size(8, 8, 1)
fn initial_temporal(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = gid.xy;
    if (!in_bounds(p)) {
        return;
    }
    let w = params.size.x;
    let i = p.y * w + p.x;
    let depth = textureLoad(depth_tex, vec2<i32>(p), 0).r;
    let uv = pixel_uv(p);
    var s_position = vec3<f32>(0.0);
    var s_normal = vec3<f32>(0.0, 1.0, 0.0);
    var s_depth = 0.0;
    if (depth > 0.0) {
        s_position = world_at(uv, depth);
        let n = textureLoad(normal_tex, vec2<i32>(p), 0).xyz;
        let nl = length(n);
        s_normal = select(vec3<f32>(0.0, 1.0, 0.0), n / nl, nl > 1e-12);
        s_depth = depth;
    }
    let s = Surface(s_position, s_normal, s_depth);
    guide_cur[2u * i] = vec4<f32>(s.position, s.depth);
    guide_cur[2u * i + 1u] = vec4<f32>(s.normal, 0.0);
    if (depth <= 0.0) {
        res_tmp[i] = Reservoir(NONE, 0.0, 0.0, 0.0);
        return;
    }
    // Candidate list.
    var count = params.size.z;
    var base = 0u;
    let clustered = (params.cfg.z & FLAG_CLUSTERS) != 0u;
    if (clustered) {
        let cid = cluster_of(uv, depth);
        count = min(cluster_counts[cid], params.cfg.w);
        base = cid * params.cfg.w;
    }
    let view = normalize(params.eye.xyz - s.position);
    let mat = material_at(p);
    var rng = rng_seed(i, STREAM_INITIAL);
    var w_sum = 0.0;
    var chosen = NONE;
    let m_initial = f32(params.cfg.x);
    for (var k = 0u; k < params.cfg.x; k = k + 1u) {
        let u = rng_next(&rng);
        let u_pick = rng_next(&rng);
        if (count == 0u) {
            continue;
        }
        let j = min(u32(u * f32(count)), count - 1u);
        var li = j;
        if (clustered) {
            li = cluster_indices[base + j];
        }
        let wt = target_at(li, s, view, mat) * f32(count);
        w_sum = w_sum + wt;
        if (wt > 0.0 && u_pick * w_sum < wt) {
            chosen = li;
        }
    }
    let pc = target_at(chosen, s, view, mat);
    var r_light = chosen;
    var r_w = select(0.0, w_sum / (m_initial * pc), pc > 0.0);
    var r_m = m_initial;
    if ((params.cfg.z & FLAG_HISTORY) != 0u) {
        var trng = rng_seed(i, STREAM_TEMPORAL);
        let u_pick = rng_next(&trng);
        let mv = textureLoad(motion_tex, vec2<i32>(p), 0).xy;
        let prev_uv = uv + mv;
        let pp = floor(prev_uv * vec2<f32>(f32(params.size.x), f32(params.size.y)));
        if (pp.x >= 0.0 && pp.y >= 0.0 && pp.x < f32(params.size.x) && pp.y < f32(params.size.y)) {
            let q = u32(pp.y) * w + u32(pp.x);
            let ps = guide_surface(q, true);
            if (ps.depth > 0.0 && similar(s, ps)) {
                let rp = res_prev[q];
                let mp = min(rp.m, params.tol.z * r_m);
                let wc = pc * r_w * r_m;
                let wp = target_at(rp.light, s, view, mat) * rp.w * mp;
                let sum = wc + wp;
                var y_sel = r_light;
                if (wp > 0.0 && u_pick * sum < wp) {
                    y_sel = rp.light;
                }
                let p_sel = target_at(y_sel, s, view, mat);
                var z = 0.0;
                if (p_sel > 0.0) {
                    z = z + r_m;
                }
                if (support_at(y_sel, ps)) {
                    z = z + mp;
                }
                r_light = y_sel;
                r_w = select(0.0, sum / (z * p_sel), p_sel > 0.0 && z > 0.0);
                r_m = r_m + mp;
            }
        }
    }
    res_tmp[i] = Reservoir(r_light, r_w, r_m, 0.0);
}

// Entry `k` of the accepted-pixel list. Three vectors, not an array: the SPIR-V gate refuses
// function-scope arrays (VUID-StandaloneSpirv-None-10684).
fn accepted_at(a: vec4<u32>, b: vec4<u32>, c: u32, k: u32) -> u32 {
    if (k < 4u) {
        return a[k];
    }
    if (k < 8u) {
        return b[k - 4u];
    }
    return c;
}

// `v` with component `k` set to `q`. Rebuilt whole: FXC cannot address a store to a vector
// component at a dynamic index inside a loop (X3550).
fn accepted_with(v: vec4<u32>, k: u32, q: u32) -> vec4<u32> {
    return select(v, vec4<u32>(q), vec4<u32>(0u, 1u, 2u, 3u) == vec4<u32>(k));
}

@compute @workgroup_size(8, 8, 1)
fn spatial(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = gid.xy;
    if (!in_bounds(p)) {
        return;
    }
    let w = params.size.x;
    let i = p.y * w + p.x;
    let s = guide_surface(i, false);
    if (s.depth <= 0.0) {
        res_cur[i] = Reservoir(NONE, 0.0, 0.0, 0.0);
        return;
    }
    let view = normalize(params.eye.xyz - s.position);
    let mat = material_at(p);
    let own = res_tmp[i];
    var w_sum = target_at(own.light, s, view, mat) * own.w * own.m;
    var chosen = own.light;
    var m_total = own.m;
    // The accepted pixels (own + at most MAX_TAPS): `accepted_at`.
    var accepted_a = vec4<u32>(i, 0u, 0u, 0u);
    var accepted_b = vec4<u32>(0u);
    var accepted_c = 0u;
    var n_acc = 1u;
    var rng = rng_seed(i, STREAM_SPATIAL);
    let x = i32(p.x);
    let y = i32(p.y);
    for (var k = 0u; k < params.cfg.y; k = k + 1u) {
        let angle = rng_next(&rng) * TAU;
        let radius = params.tol.w * sqrt(rng_next(&rng));
        let u_pick = rng_next(&rng);
        let qx = x + i32(round(radius * cos(angle)));
        let qy = y + i32(round(radius * sin(angle)));
        if ((qx == x && qy == y) || qx < 0 || qy < 0 || qx >= i32(params.size.x) || qy >= i32(params.size.y)) {
            continue;
        }
        let q = u32(qy) * w + u32(qx);
        let qs = guide_surface(q, false);
        if (qs.depth <= 0.0 || !similar(s, qs)) {
            continue;
        }
        var dup = false;
        for (var a = 0u; a < n_acc; a = a + 1u) {
            if (accepted_at(accepted_a, accepted_b, accepted_c, a) == q) {
                dup = true;
            }
        }
        if (dup) {
            continue;
        }
        let rq = res_tmp[q];
        let wq = target_at(rq.light, s, view, mat) * rq.w * rq.m;
        w_sum = w_sum + wq;
        m_total = m_total + rq.m;
        if (n_acc < 4u) {
            accepted_a = accepted_with(accepted_a, n_acc, q);
        } else if (n_acc < 8u) {
            accepted_b = accepted_with(accepted_b, n_acc - 4u, q);
        } else {
            accepted_c = q;
        }
        n_acc = n_acc + 1u;
        if (wq > 0.0 && u_pick * w_sum < wq) {
            chosen = rq.light;
        }
    }
    let p_sel = target_at(chosen, s, view, mat);
    var z = 0.0;
    if (p_sel > 0.0) {
        for (var a = 0u; a < n_acc; a = a + 1u) {
            let q = accepted_at(accepted_a, accepted_b, accepted_c, a);
            let qs = guide_surface(q, false);
            if (qs.depth > 0.0 && support_at(chosen, qs)) {
                z = z + res_tmp[q].m;
            }
        }
    }
    res_cur[i] = Reservoir(chosen, select(0.0, w_sum / (z * p_sel), p_sel > 0.0 && z > 0.0), m_total, 0.0);
}

fn project_uv(world: vec3<f32>) -> vec3<f32> {
    let c = params.clip_from_world * vec4<f32>(world, 1.0);
    let ndc = c.xyz / max(c.w, 1e-6);
    return vec3<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5, c.w);
}

// Screen-space shadow ray toward the light: occluded when a step lands behind a depth sample
// by less than the sample's thickness.
fn screen_visibility(s: Surface, li: u32) -> f32 {
    let steps = params.extra.y;
    if ((params.cfg.z & FLAG_TRACE) == 0u || steps == 0u) {
        return 1.0;
    }
    let lp = lights[li].position_kind.xyz;
    let start = s.position + s.normal * (0.01 * s.depth);
    let size = vec2<f32>(f32(params.size.x), f32(params.size.y));
    for (var k = 1u; k <= steps; k = k + 1u) {
        let t = (f32(k) - 0.5) / f32(steps);
        let pt = mix(start, lp, t);
        let ruv = project_uv(pt);
        if (ruv.x < 0.0 || ruv.y < 0.0 || ruv.x >= 1.0 || ruv.y >= 1.0 || ruv.z <= params.planes.x) {
            return 1.0;
        }
        let pix = vec2<i32>(floor(ruv.xy * size));
        let scene = textureLoad(depth_tex, pix, 0).r;
        let ray_depth = dot(pt - params.eye.xyz, params.forward.xyz);
        if (scene > 0.0 && ray_depth > scene * 1.002 && ray_depth < scene * (1.0 + params.eye.w)) {
            return 0.0;
        }
    }
    return 1.0;
}

@compute @workgroup_size(8, 8, 1)
fn shade_pass(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = gid.xy;
    if (!in_bounds(p)) {
        return;
    }
    let i = p.y * params.size.x + p.x;
    let s = guide_surface(i, false);
    if (s.depth <= 0.0) {
        textureStore(out_tex, vec2<i32>(p), vec4<f32>(0.0));
        return;
    }
    let r = res_cur[i];
    var c = vec3<f32>(0.0);
    if (r.light != NONE && r.w > 0.0) {
        let view = normalize(params.eye.xyz - s.position);
        c = shade(r.light, s, view, material_at(p)) * r.w * screen_visibility(s, r.light);
    }
    textureStore(out_tex, vec2<i32>(p), vec4<f32>(c, 1.0));
}

@compute @workgroup_size(8, 8, 1)
fn reference_pass(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = gid.xy;
    if (!in_bounds(p)) {
        return;
    }
    let i = p.y * params.size.x + p.x;
    let s = guide_surface(i, false);
    if (s.depth <= 0.0) {
        textureStore(out_tex, vec2<i32>(p), vec4<f32>(0.0));
        return;
    }
    var count = params.size.z;
    var base = 0u;
    let clustered = (params.cfg.z & FLAG_CLUSTERS) != 0u;
    if (clustered) {
        let cid = cluster_of(pixel_uv(p), s.depth);
        count = min(cluster_counts[cid], params.cfg.w);
        base = cid * params.cfg.w;
    }
    let view = normalize(params.eye.xyz - s.position);
    let mat = material_at(p);
    var c = vec3<f32>(0.0);
    for (var j = 0u; j < count; j = j + 1u) {
        var li = j;
        if (clustered) {
            li = cluster_indices[base + j];
        }
        c = c + shade(li, s, view, mat);
    }
    textureStore(out_tex, vec2<i32>(p), vec4<f32>(c, 1.0));
}
