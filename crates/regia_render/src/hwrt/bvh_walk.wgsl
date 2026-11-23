// SPDX-License-Identifier: MPL-2.0

// The stackless BVH walk (`hwrt::bvh`, skip pointers, Smits 1998), shared by the sample-list
// fallback (`hwrt_shadow_fallback.wgsl`) and the frame's trace (`sun_trace_bvh.wgsl`). The includer
// declares `nodes: array<BvhNode>` and `tris: array<vec4<f32>>`. Same box and triangle tests, in
// the same order, as the CPU reference `ShadowBvh::occluded`. No function-local arrays or structs
// (ADR-026).

struct BvhNode {
    min_corner: vec3<f32>,
    // Next node after this subtree (the node count ends traversal).
    escape: u32,
    max_corner: vec3<f32>,
    // 0: interior (first child is the next node). Leaf: (first_triangle << 4) | count.
    prims: u32,
}

const MIN_DIRECTION_COMPONENT: f32 = 1e-9;
const PARALLEL_EPSILON: f32 = 1e-12;

fn inverse_direction(d: vec3<f32>) -> vec3<f32> {
    let tiny = abs(d) < vec3<f32>(MIN_DIRECTION_COMPONENT);
    return vec3<f32>(1.0) / select(d, vec3<f32>(MIN_DIRECTION_COMPONENT), tiny);
}

fn hits_box(o: vec3<f32>, inv_d: vec3<f32>, lo: vec3<f32>, hi: vec3<f32>, t_min: f32, t_max: f32) -> bool {
    let t0 = (lo - o) * inv_d;
    let t1 = (hi - o) * inv_d;
    let t_small = min(t0, t1);
    let t_big = max(t0, t1);
    let t_near = max(max(t_small.x, t_small.y), t_small.z);
    let t_far = min(min(t_big.x, t_big.y), t_big.z);
    return t_near <= t_far && t_far >= t_min && t_near <= t_max;
}

// Moller and Trumbore 1997, double-sided.
fn hits_triangle(o: vec3<f32>, d: vec3<f32>, a: vec3<f32>, b: vec3<f32>, c: vec3<f32>, t_min: f32, t_max: f32) -> bool {
    let e1 = b - a;
    let e2 = c - a;
    let p = cross(d, e2);
    let det = dot(e1, p);
    if abs(det) < PARALLEL_EPSILON {
        return false;
    }
    let inv_det = 1.0 / det;
    let s = o - a;
    let u = dot(s, p) * inv_det;
    if u < 0.0 || u > 1.0 {
        return false;
    }
    let q = cross(s, e1);
    let v = dot(d, q) * inv_det;
    if v < 0.0 || u + v > 1.0 {
        return false;
    }
    let t = dot(e2, q) * inv_det;
    return t > t_min && t < t_max;
}

// PCG output permutation (O'Neill 2014). The same function as `masked_hash` in
// `sun_trace_ray_query.wgsl` and `hwrt::bvh::masked_hash`.
fn masked_hash(v: u32) -> u32 {
    let state = v * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

// Any-hit alpha test (ADR-4557). `coverage_w` is the first corner's w (`tris[base].w`).
//
// 0, and anything outside (0, 1), is opaque: the any-hit is skipped and the hit commits.
// That is what an unused w used to be, so an opaque tree is unchanged. A value in (0, 1) is
// the per-draw coverage fraction (the same number `coverage_custom_data` packs for the
// ray-query path). The hit commits when the roll falls under that byte.
//
// No mask texture is bound on this walk, so there is no texel fetch. A checker or a
// leaf cutout that exists only as a texture is not read; the stored coverage is the whole mask.
// A rejected candidate does not end the ray. No confirmed hit is a miss.
//
// The instance term is 0: these triangles are already in world space. The ray-query any-hit
// xors the top-level instance index in the same place.
fn any_hit(o: vec3<f32>, primitive_index: u32, coverage_w: f32) -> bool {
    if !(coverage_w > 0.0 && coverage_w < 1.0) {
        return true;
    }
    let coverage = u32(floor(coverage_w * 255.0 + 0.5));
    // Byte 255 is fully covered, matching the CPU reference (not 255/256 coverage).
    if coverage >= 255u {
        return true;
    }
    let seed = masked_hash(bitcast<u32>(o.x) ^ masked_hash(bitcast<u32>(o.y) ^ masked_hash(bitcast<u32>(o.z))));
    let roll = masked_hash(seed ^ (primitive_index * 2654435761u) ^ 0u) & 0xFFu;
    return roll < coverage;
}

fn bvh_occluded(o: vec3<f32>, d: vec3<f32>, t_min: f32, t_max: f32, node_count: u32) -> bool {
    let inv_d = inverse_direction(d);
    let tri_count = arrayLength(&tris) / 3u;
    var i = 0u;
    // `i` only moves forward (a malformed escape ends the walk), so the loop ends within
    // node_count steps.
    loop {
        if i >= node_count {
            break;
        }
        let node = nodes[i];
        let next = node.escape;
        if !hits_box(o, inv_d, node.min_corner, node.max_corner, t_min, t_max) {
            if next <= i {
                break;
            }
            i = next;
            continue;
        }
        let count = node.prims & 15u;
        if count == 0u {
            i = i + 1u;
            continue;
        }
        let first = node.prims >> 4u;
        for (var k = 0u; k < count; k = k + 1u) {
            let t = first + k;
            if t >= tri_count {
                return false;
            }
            let base = 3u * t;
            if hits_triangle(o, d, tris[base].xyz, tris[base + 1u].xyz, tris[base + 2u].xyz, t_min, t_max) && any_hit(o, t, tris[base].w) {
                return true;
            }
        }
        if next <= i {
            break;
        }
        i = next;
    }
    return false;
}
