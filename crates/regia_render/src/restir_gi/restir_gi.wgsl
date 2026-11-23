// SPDX-License-Identifier: MPL-2.0
// ReSTIR GI (ADR-3302). Appended after ddgi_common.wgsl, ddgi_sample.wgsl and
// gi_denoise.wgsl. Group 0 is the existing denoiser; this pass uses group 1.
// Original WGSL from Ouyang et al. 2021 (path reservoirs and Equation 11), Bitterli et al.
// 2020 (reservoir update and contribution weight), and Duff et al. 2017 (orthonormal basis).

struct RestirParams {
    world_from_clip: mat4x4<f32>,
    // width, height, spatial taps, flags (1 history, 2 temporal, 4 periodic validation)
    size: vec4<u32>,
    // frame, validation interval, seed, material records
    cfg: vec4<u32>,
    // history cap, spatial cap, depth tolerance, normal tolerance
    tol: vec4<f32>,
    // ray offset, offset per eye metre, far, spatial radius in pixels
    ray: vec4<f32>,
    // validation relative luminance tolerance, unused
    validation: vec4<f32>,
    // eye xyz, unused
    eye: vec4<f32>,
}

struct HitMaterial {
    base_color: vec4<f32>,
    emissive: vec4<f32>,
}

struct Reservoir {
    // xyz = visible point, w = linear view depth
    visible: vec4<f32>,
    // xyz = path vertex, w = draw index + 1 (0 = the default material)
    hit: vec4<f32>,
    // octahedral visible normal xy and hit normal zw
    normals: vec4<f32>,
    // outgoing radiance rgb, unused w
    radiance: vec4<f32>,
    // Selected material point in object space (w = 1); zero for sky samples.
    object_hit: vec4<f32>,
    // weight sum, M, contribution W, valid
    weights: vec4<f32>,
}

@group(1) @binding(0) var<uniform> rp: RestirParams;
@group(1) @binding(1) var gi_depth_tex: texture_2d<f32>;
@group(1) @binding(2) var gi_normal_tex: texture_2d<f32>;
@group(1) @binding(3) var gi_motion_tex: texture_2d<f32>;
@group(1) @binding(4) var gi_scene_depth: texture_depth_2d;
@group(1) @binding(5) var gi_tlas: acceleration_structure<vertex_return>;
@group(1) @binding(6) var<uniform> ddgi: DdgiUniform;
@group(1) @binding(7) var<storage, read_write> ddgi_data: array<u32>;
@group(1) @binding(8) var<storage, read> gi_materials: array<HitMaterial>;
@group(1) @binding(9) var<storage, read_write> gi_previous: array<Reservoir>;
@group(1) @binding(10) var<storage, read_write> gi_temporal: array<Reservoir>;
@group(1) @binding(11) var<storage, read_write> gi_output: array<Reservoir>;
@group(1) @binding(12) var<storage, read_write> gi_frame_input: array<vec4<f32>>;
@group(1) @binding(13) var<storage, read_write> gi_stats: array<atomic<u32>>;

const GI_PI: f32 = 3.141592653589793;
const GI_TAU: f32 = 6.283185307179586;
const GI_STAT_RAYS: u32 = 0u;
const GI_STAT_INITIAL: u32 = 1u;
const GI_STAT_SHADOW: u32 = 2u;
const GI_STAT_VISIBILITY: u32 = 3u;
const GI_STAT_TEMPORAL: u32 = 4u;
const GI_STAT_SPATIAL: u32 = 5u;
const GI_STAT_HISTORY_REJECT: u32 = 6u;
const GI_STAT_VALIDATION_REJECT: u32 = 7u;
const GI_STAT_INITIAL_HITS: u32 = 8u;
const GI_STAT_SKY_SAMPLES: u32 = 9u;
const GI_STAT_SUN_VISIBLE: u32 = 10u;
const GI_STAT_PROBE_LUM_SUM: u32 = 11u;
const GI_STAT_MOTION_REJECT: u32 = 12u;
// A reused opaque hit is dropped when the surface at that world point has moved farther than
// this in world space (ADR-4001). Matches `HIT_MOTION_METRES` in `cpu.rs`.
const GI_HIT_MOTION_M: f32 = 0.05;

fn gi_count(kind: u32) {
    atomicAdd(&gi_stats[GI_STAT_RAYS], 1u);
    atomicAdd(&gi_stats[kind], 1u);
}

fn gi_pcg(v: u32) -> u32 {
    let state = v * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

fn gi_seed(pixel: u32, stream: u32) -> u32 {
    return gi_pcg(pixel ^ gi_pcg(rp.cfg.x ^ gi_pcg(stream ^ gi_pcg(rp.cfg.z))));
}

// A per-pixel stream without the frame counter: the spatial tap pattern stays fixed while the
// view holds, so reselection on a held frame comes only from the new initial sample. That is
// what keeps the converged estimate inside spec 27 § 2.3's frame-to-frame bound; the initial
// sample keeps the frame in its seed (a fresh candidate every frame is the point of RIS).
fn gi_seed_static(pixel: u32, stream: u32) -> u32 {
    return gi_pcg(pixel ^ gi_pcg(stream ^ gi_pcg(rp.cfg.z)));
}

fn gi_rand(state: ptr<function, u32>) -> f32 {
    *state = gi_pcg(*state);
    return f32(*state >> 8u) * (1.0 / 16777216.0);
}

fn gi_luma(c: vec3<f32>) -> f32 {
    return max(dot(c, vec3<f32>(0.2126, 0.7152, 0.0722)), 0.0);
}

fn gi_dimensions() -> vec2<i32> {
    return vec2<i32>(i32(rp.size.x), i32(rp.size.y));
}

fn gi_inside(p: vec2<i32>) -> bool {
    let d = gi_dimensions();
    return p.x >= 0 && p.y >= 0 && p.x < d.x && p.y < d.y;
}

fn gi_index(p: vec2<i32>) -> u32 {
    return u32(p.y * gi_dimensions().x + p.x);
}

fn gi_world(p: vec2<i32>) -> vec3<f32> {
    let dims = vec2<f32>(gi_dimensions());
    let uv = (vec2<f32>(p) + vec2<f32>(0.5)) / dims;
    let raw = textureLoad(gi_scene_depth, p, 0);
    let clip = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, raw, 1.0);
    let world = rp.world_from_clip * clip;
    return world.xyz / select(world.w, 1e-6, abs(world.w) < 1e-6);
}

fn gi_surface_normal(p: vec2<i32>) -> vec3<f32> {
    let n = textureLoad(gi_normal_tex, p, 0).xyz;
    let len = length(n);
    return select(vec3<f32>(0.0, 1.0, 0.0), n / len, len > 1e-12);
}

fn gi_empty() -> Reservoir {
    return Reservoir(vec4<f32>(0.0), vec4<f32>(0.0), vec4<f32>(0.0), vec4<f32>(0.0), vec4<f32>(0.0), vec4<f32>(0.0));
}

fn gi_update(r: ptr<function, Reservoir>, sample: Reservoir, weight: f32, count: f32, roll: f32) {
    if !(weight > 0.0 && weight <= 3.4e38 && count > 0.0 && count <= 3.4e38) {
        return;
    }
    (*r).weights.x += weight;
    (*r).weights.y += count;
    if roll * (*r).weights.x < weight {
        (*r).visible = sample.visible;
        (*r).hit = sample.hit;
        (*r).normals = sample.normals;
        (*r).radiance = sample.radiance;
        (*r).object_hit = sample.object_hit;
    }
}

fn gi_merge(r: ptr<function, Reservoir>, other: Reservoir, target_value: f32, roll: f32) {
    gi_update(r, other, max(target_value, 0.0) * other.weights.z * other.weights.y, other.weights.y, roll);
}

fn gi_finish(r: ptr<function, Reservoir>, target_value: f32, support: f32) {
    if target_value > 0.0 && target_value <= 3.4e38 && support > 0.0 && (*r).weights.x <= 3.4e38 {
        (*r).weights.z = (*r).weights.x / (support * target_value);
    } else {
        (*r).weights.z = 0.0;
    }
    (*r).weights.w = select(0.0, 1.0, (*r).weights.y > 0.0);
}

fn gi_cap(r: ptr<function, Reservoir>, cap: f32) {
    if (*r).weights.y > cap && (*r).weights.y > 0.0 {
        (*r).weights.x *= cap / (*r).weights.y;
        (*r).weights.y = cap;
    }
}

// The target function luminance at a surface point: outgoing radiance toward the point
// times the cosine-weighted diffuse pdf there (p̂ ∝ f, the integrand). With p̂ = f every
// RIS weight is p̂/pdf, the sample's own luminance, whatever its angle: a grazing sample
// (tiny pdf) can no longer dominate w_sum and explode the generalized RIS weight when it
// loses the reservoir.
fn gi_target(radiance_luma: f32, normal: vec3<f32>, surface: vec3<f32>, hit: vec3<f32>) -> f32 {
    let delta = hit - surface;
    let len = length(delta);
    if !(len > 1e-6) {
        return 0.0;
    }
    let cosine = max(dot(normal, delta / len), 0.0);
    return radiance_luma * cosine / GI_PI;
}

fn gi_cosine_direction(n: vec3<f32>, u1: f32, u2: f32) -> vec3<f32> {
    let sign_z = select(-1.0, 1.0, n.z >= 0.0);
    let a = -1.0 / (sign_z + n.z);
    let b = n.x * n.y * a;
    let tangent = vec3<f32>(1.0 + sign_z * n.x * n.x * a, sign_z * b, -sign_z * n.x);
    let bitangent = vec3<f32>(b, sign_z + n.y * n.y * a, -n.y);
    let phi = GI_TAU * u2;
    let radial = sqrt(clamp(u1, 0.0, 1.0));
    return normalize(tangent * (cos(phi) * radial) + bitangent * (sin(phi) * radial) + n * sqrt(max(1.0 - u1, 0.0)));
}

fn gi_jacobian(source: vec3<f32>, destination: vec3<f32>, sample: Reservoir) -> f32 {
    let hit_normal = ddgi_oct_decode(sample.normals.zw);
    let a = source - sample.hit.xyz;
    let b = destination - sample.hit.xyz;
    let la = length(a);
    let lb = length(b);
    if !(la > 1e-6 && lb > 1e-6 && la <= 3.4e38 && lb <= 3.4e38) {
        return 0.0;
    }
    let ca = abs(dot(hit_normal, a / la));
    let cb = abs(dot(hit_normal, b / lb));
    let denominator = ca * lb * lb;
    if denominator <= 1e-12 {
        return 0.0;
    }
    return clamp(cb * la * la / denominator, 1e-4, 1e4);
}

fn gi_any_hit(origin: vec3<f32>, direction: vec3<f32>, t_min: f32, t_max: f32) -> bool {
    if t_max <= t_min {
        return false;
    }
    gi_count(GI_STAT_VISIBILITY);
    var query: ray_query<vertex_return>;
    rayQueryInitialize(&query, gi_tlas, RayDesc(RAY_FLAG_FORCE_OPAQUE | RAY_FLAG_TERMINATE_ON_FIRST_HIT, 0x01u, t_min, t_max, origin, direction));
    while rayQueryProceed(&query) {}
    return rayQueryGetCommittedIntersection(&query).kind != RAY_QUERY_INTERSECTION_NONE;
}

fn gi_visible(origin: vec3<f32>, normal: vec3<f32>, destination: vec3<f32>) -> bool {
    let delta = destination - origin;
    let ray_distance = length(delta);
    if ray_distance <= 4.0 * rp.ray.x {
        return true;
    }
    let offset = rp.ray.x + rp.ray.y * distance(origin, rp.eye.xyz);
    return !gi_any_hit(origin + normal * offset, delta / ray_distance, offset, max(ray_distance - 2.0 * offset, offset));
}

// Object motion of a stored opaque hit. Camera motion stays in the receiver reprojection:
// a world-space hit on a still surface does not move when only the camera does. The confirm ray
// starts `GI_HIT_MOTION_M` outside the stored point along its outward normal and returns along
// that normal, so a flat or curved surface is met head-on. A miss, a hit farther than the
// threshold, or a different instance means the surface left. Sky samples (hit.w < 0.5) stay with
// the camera test.
fn gi_hit_motion_ok(previous: Reservoir) -> bool {
    if previous.hit.w < 0.5 {
        return true;
    }
    let stored = previous.hit.xyz;
    let outward = ddgi_oct_decode(previous.normals.zw);
    let nlen = length(outward);
    if !(nlen > 0.5 && nlen < 1.5) {
        return false;
    }
    let dir = -outward;
    let origin = stored + outward * GI_HIT_MOTION_M;
    gi_count(GI_STAT_VISIBILITY);
    var query: ray_query<vertex_return>;
    rayQueryInitialize(&query, gi_tlas, RayDesc(RAY_FLAG_FORCE_OPAQUE | RAY_FLAG_TERMINATE_ON_FIRST_HIT, 0x01u, 0.001, GI_HIT_MOTION_M * 2.0, origin, dir));
    while rayQueryProceed(&query) {}
    let intersection = rayQueryGetCommittedIntersection(&query);
    if intersection.kind == RAY_QUERY_INTERSECTION_NONE {
        return false;
    }
    let found = origin + dir * intersection.t;
    let gap = distance(found, stored);
    let same = abs(f32(intersection.instance_custom_data) - previous.hit.w) < 0.5;
    // Occupancy alone misses tangential motion of an overlapping instance. Follow the original
    // material point through the current instance transform, independently of camera motion.
    let material_point = intersection.object_to_world * previous.object_hit;
    let displacement = distance(material_point, stored);
    return gap <= GI_HIT_MOTION_M && same && displacement <= GI_HIT_MOTION_M;
}

fn gi_material(index: u32) -> HitMaterial {
    if index > 0u && index <= rp.cfg.w {
        return gi_materials[index - 1u];
    }
    return HitMaterial(vec4<f32>(0.8, 0.8, 0.8, 1.0), vec4<f32>(0.0));
}

fn gi_hit_radiance(hit: vec3<f32>, normal: vec3<f32>, outgoing: vec3<f32>, material_index: u32) -> vec3<f32> {
    let material = gi_material(material_index);
    let probe = ddgi_irradiance(hit, normal, outgoing);
    atomicAdd(&gi_stats[GI_STAT_PROBE_LUM_SUM], u32(max(gi_luma(probe.rgb), 0.0)));
    var radiance = probe.rgb;
    if probe.a < 0.0 {
        atomicAdd(&gi_stats[GI_STAT_SKY_SAMPLES], 1u);
        radiance = ddgi.flat_sky.rgb;
    }
    let to_sun = ddgi.to_sun.xyz;
    let nol = dot(normal, to_sun);
    if ddgi.to_sun.w > 0.0 && nol > 0.0 {
        let offset = rp.ray.x + rp.ray.y * distance(hit, rp.eye.xyz);
        gi_count(GI_STAT_SHADOW);
        var query: ray_query<vertex_return>;
        rayQueryInitialize(&query, gi_tlas, RayDesc(RAY_FLAG_FORCE_OPAQUE | RAY_FLAG_TERMINATE_ON_FIRST_HIT, 0x01u, offset, rp.ray.z, hit + normal * offset, to_sun));
        while rayQueryProceed(&query) {}
        let blocked = rayQueryGetCommittedIntersection(&query).kind != RAY_QUERY_INTERSECTION_NONE;
        if !blocked {
            atomicAdd(&gi_stats[GI_STAT_SUN_VISIBLE], 1u);
            radiance += ddgi.sun.rgb * material.base_color.rgb * (nol / GI_PI);
        }
    }
    radiance += material.emissive.rgb * material.emissive.w;
    return max(radiance, vec3<f32>(0.0));
}

fn gi_initial(p: vec2<i32>, rng: ptr<function, u32>) -> Reservoir {
    let visible = gi_world(p);
    let normal = gi_surface_normal(p);
    let direction = gi_cosine_direction(normal, gi_rand(rng), gi_rand(rng));
    let offset = rp.ray.x + rp.ray.y * distance(visible, rp.eye.xyz);
    let origin = visible + normal * offset;
    gi_count(GI_STAT_INITIAL);
    var query: ray_query<vertex_return>;
    rayQueryInitialize(&query, gi_tlas, RayDesc(RAY_FLAG_FORCE_OPAQUE | RAY_FLAG_TERMINATE_ON_FIRST_HIT, 0x01u, offset, rp.ray.z, origin, direction));
    while rayQueryProceed(&query) {}
    let intersection = rayQueryGetCommittedIntersection(&query);
    if intersection.kind == RAY_QUERY_INTERSECTION_NONE {
        atomicAdd(&gi_stats[GI_STAT_SKY_SAMPLES], 1u);
        let hit = origin + direction * rp.ray.z;
        let radiance = ddgi.flat_sky.rgb;
        return Reservoir(
            vec4<f32>(visible, textureLoad(gi_depth_tex, p, 0).r),
            vec4<f32>(hit, 0.0),
            vec4<f32>(ddgi_oct_encode(normal), ddgi_oct_encode(-direction)),
            vec4<f32>(radiance, 1.0),
            vec4<f32>(0.0),
            vec4<f32>(0.0),
        );
    }
    atomicAdd(&gi_stats[GI_STAT_INITIAL_HITS], 1u);
    let hit = origin + direction * intersection.t;
    let object_vertices = getCommittedHitVertexPositions(&query);
    let w0 = intersection.object_to_world * vec4<f32>(object_vertices[0], 1.0);
    let w1 = intersection.object_to_world * vec4<f32>(object_vertices[1], 1.0);
    let w2 = intersection.object_to_world * vec4<f32>(object_vertices[2], 1.0);
    var hit_normal = cross(w1 - w0, w2 - w0);
    let normal_len = length(hit_normal);
    hit_normal = select(-direction, hit_normal / normal_len, normal_len > 1e-12);
    if dot(hit_normal, direction) > 0.0 {
        hit_normal = -hit_normal;
    }
    let radiance = gi_hit_radiance(hit, hit_normal, -direction, intersection.instance_custom_data);
    return Reservoir(
        vec4<f32>(visible, textureLoad(gi_depth_tex, p, 0).r),
        vec4<f32>(hit, f32(intersection.instance_custom_data)),
        vec4<f32>(ddgi_oct_encode(normal), ddgi_oct_encode(hit_normal)),
        vec4<f32>(radiance, 1.0),
        vec4<f32>(intersection.world_to_object * vec4<f32>(hit, 1.0), 1.0),
        vec4<f32>(0.0),
    );
}

@compute @workgroup_size(8, 8)
fn gi_initial_temporal(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if !gi_inside(p) {
        return;
    }
    let i = gi_index(p);
    if textureLoad(gi_scene_depth, p, 0) >= 1.0 {
        gi_temporal[i] = gi_empty();
        return;
    }
    var rng = gi_seed(i, 0u);
    var result = gi_empty();
    let initial = gi_initial(p, &rng);
    gi_update(&result, initial, gi_luma(initial.radiance.rgb), 1.0, gi_rand(&rng));

    let motion = textureLoad(gi_motion_tex, p, 0).xy * vec2<f32>(gi_dimensions());
    let previous_p = vec2<i32>(floor(vec2<f32>(p) + vec2<f32>(0.5) + motion));
    let has_history = (rp.size.w & 1u) != 0u && (rp.size.w & 2u) != 0u;
    if has_history {
        if !gi_inside(previous_p) {
            atomicAdd(&gi_stats[GI_STAT_HISTORY_REJECT], 1u);
        } else {
            let previous = gi_previous[gi_index(previous_p)];
            let visible_normal = ddgi_oct_decode(previous.normals.xy);
            let compatible = previous.weights.w > 0.0
                && distance(previous.visible.xyz, initial.visible.xyz) <= rp.tol.z * initial.visible.w
                && dot(visible_normal, gi_surface_normal(p)) >= rp.tol.w;
            if !compatible {
                atomicAdd(&gi_stats[GI_STAT_HISTORY_REJECT], 1u);
            } else if !gi_visible(initial.visible.xyz, gi_surface_normal(p), previous.hit.xyz) {
                atomicAdd(&gi_stats[GI_STAT_HISTORY_REJECT], 1u);
            } else if !gi_hit_motion_ok(previous) {
                atomicAdd(&gi_stats[GI_STAT_HISTORY_REJECT], 1u);
                atomicAdd(&gi_stats[GI_STAT_MOTION_REJECT], 1u);
            } else {
                var accepted = true;
                if (rp.size.w & 4u) != 0u {
                    let hit_normal = ddgi_oct_decode(previous.normals.zw);
                    let outgoing = normalize(previous.visible.xyz - previous.hit.xyz);
                    let revised = gi_hit_radiance(previous.hit.xyz, hit_normal, outgoing, u32(previous.hit.w + 0.5));
                    let old_luma = gi_luma(previous.radiance.rgb);
                    let new_luma = gi_luma(revised);
                    if abs(new_luma - old_luma) > rp.validation.x * max(new_luma, old_luma) {
                        atomicAdd(&gi_stats[GI_STAT_VALIDATION_REJECT], 1u);
                        accepted = false;
                    }
                }
                if accepted {
                    var history = previous;
                    gi_cap(&history, rp.tol.x);
                    let history_target = gi_target(
                        gi_luma(history.radiance.rgb),
                        gi_surface_normal(p),
                        initial.visible.xyz,
                        history.hit.xyz,
                    );
                    gi_merge(&result, history, history_target, gi_rand(&rng));
                    atomicAdd(&gi_stats[GI_STAT_TEMPORAL], 1u);
                }
            }
        }
    }
    gi_cap(&result, rp.tol.x);
    gi_finish(
        &result,
        gi_target(
            gi_luma(result.radiance.rgb),
            gi_surface_normal(p),
            initial.visible.xyz,
            result.hit.xyz,
        ),
        result.weights.y,
    );
    gi_temporal[i] = result;
}

@compute @workgroup_size(8, 8)
fn gi_spatial(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if !gi_inside(p) {
        return;
    }
    let i = gi_index(p);
    if textureLoad(gi_scene_depth, p, 0) >= 1.0 {
        gi_output[i] = gi_empty();
        return;
    }
    var rng = gi_seed_static(i, 1u);
    var result = gi_empty();
    let visible = gi_world(p);
    let normal = gi_surface_normal(p);
    let own = gi_temporal[i];
    gi_merge(
        &result,
        own,
        gi_target(gi_luma(own.radiance.rgb), normal, visible, own.hit.xyz),
        gi_rand(&rng),
    );
    let linear_depth = textureLoad(gi_depth_tex, p, 0).r;
    for (var tap = 0u; tap < rp.size.z; tap++) {
        let angle = gi_rand(&rng) * GI_TAU;
        let radius = sqrt(gi_rand(&rng)) * rp.ray.w;
        let q = p + vec2<i32>(round(vec2<f32>(cos(angle), sin(angle)) * radius));
        if !gi_inside(q) || (q.x == p.x && q.y == p.y) || textureLoad(gi_scene_depth, q, 0) >= 1.0 {
            continue;
        }
        let neighbor = gi_temporal[gi_index(q)];
        if neighbor.weights.w <= 0.0 {
            continue;
        }
        let neighbor_normal = gi_surface_normal(q);
        let neighbor_depth = textureLoad(gi_depth_tex, q, 0).r;
        let neighbor_visible = gi_world(q);
        if dot(normal, neighbor_normal) < rp.tol.w
            || abs(linear_depth - neighbor_depth) > rp.tol.z * linear_depth
            || distance(visible, neighbor_visible) > rp.tol.z * linear_depth {
            continue;
        }
        let jacobian = gi_jacobian(neighbor.visible.xyz, visible, neighbor);
        if jacobian <= 0.0 || !gi_visible(visible, normal, neighbor.hit.xyz) {
            continue;
        }
        let target_value = gi_target(
            gi_luma(neighbor.radiance.rgb),
            normal,
            visible,
            neighbor.hit.xyz,
        ) / jacobian;
        gi_merge(&result, neighbor, target_value, gi_rand(&rng));
        atomicAdd(&gi_stats[GI_STAT_SPATIAL], 1u);
    }
    gi_cap(&result, rp.tol.y);
    // The selected path's target also carries its source-to-destination Jacobian (Ouyang et al.
    // Algorithm 4, lines 11 and 20). Using raw radiance here loses that correction and can
    // over- or under-weight every spatially reused path.
    let selected_jacobian = gi_jacobian(result.visible.xyz, visible, result);
    gi_finish(
        &result,
        gi_target(gi_luma(result.radiance.rgb), normal, visible, result.hit.xyz) / selected_jacobian,
        result.weights.y,
    );
    gi_output[i] = result;
}

@compute @workgroup_size(8, 8)
fn gi_resolve(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if !gi_inside(p) {
        return;
    }
    let i = gi_index(p);
    let normal = gi_surface_normal(p);
    let depth = textureLoad(gi_depth_tex, p, 0).r;
    let motion = textureLoad(gi_motion_tex, p, 0).xy * vec2<f32>(gi_dimensions());
    var color = vec3<f32>(0.0);
    if textureLoad(gi_scene_depth, p, 0) < 1.0 {
        let reservoir = gi_output[i];
        let delta = reservoir.hit.xyz - gi_world(p);
        let distance = length(delta);
        if reservoir.weights.z > 0.0 && distance > 1e-6 {
            color = reservoir.radiance.rgb * reservoir.weights.z * (max(dot(normal, delta / distance), 0.0) / GI_PI);
        }
        // Energy guard: the probe cache is this pass's only radiance source, so a single
        // diffuse bounce through it cannot exceed the primary surface's own cache lookup by
        // more than the guard factor. Probes embedded in thin walls leak direct light into
        // the volume; a lookup at a path vertex one cell away can read that leaked light
        // while the surface lookup (biased into the open volume) reads dark. Without the
        // guard a sealed room's indirect estimate runs ~100x hot; with it the leak stays at
        // the probe tier's level. This is a deliberate practical bias (ADR-3349).
        let surface = gi_world(p);
        let probe = ddgi_irradiance(surface, normal, normalize(rp.eye.xyz - surface));
        let bound = 2.0 * gi_luma(probe.rgb);
        let luma = gi_luma(color);
        if luma > bound && luma > 0.0 {
            color *= bound / luma;
        }
    }
    color = clamp(color, vec3<f32>(0.0), vec3<f32>(65504.0));
    gi_frame_input[3u * i] = vec4<f32>(color, 0.0);
    gi_frame_input[3u * i + 1u] = vec4<f32>(normal, depth);
    gi_frame_input[3u * i + 2u] = vec4<f32>(motion, 0.0, 0.0);
}

// Runs after the existing denoiser. Group 0's `src` is its final ping/pong result.
@compute @workgroup_size(8, 8)
fn gi_composite(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if p.x >= i32(rp.validation.z) || p.y >= i32(rp.validation.w) {
        return;
    }
    let i = u32(p.y * i32(rp.validation.z) + p.x);
    let full_p = p * i32(rp.validation.y);
    let word = ddgi.screen.x + i * DDGI_SCREEN_WORDS;
    if textureLoad(gi_scene_depth, full_p, 0) >= 1.0 {
        for (var j = 0u; j < 6u; j++) {
            ddgi_data[word + j] = 0u;
        }
        ddgi_data[word + 6u] = bitcast<u32>(1.0e30);
        ddgi_data[word + 7u] = bitcast<u32>(0.0);
        return;
    }
    let color = clamp(src[gi_index(full_p)].rgb, vec3<f32>(0.0), vec3<f32>(65408.0));
    for (var j = 0u; j < 6u; j++) {
        ddgi_data[word + j] = ddgi_pack_rgb9e5(color);
    }
    ddgi_data[word + 6u] = bitcast<u32>(distance(gi_world(full_p), rp.eye.xyz));
    ddgi_data[word + 7u] = bitcast<u32>(1.0);
}
