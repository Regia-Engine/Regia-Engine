// SPDX-License-Identifier: MPL-2.0
// Hardware ray queries against the frame's top-level acceleration structure.
//
// Opaque casters are built with the opaque geometry flag: their hits commit at once and never
// call `any_hit`. Alpha-masked casters (ADR-2310, ADR-4557) are built non-opaque, and carry
// their coverage (0..255) in the instance's custom data. `any_hit` confirms a candidate when a
// hash of the ray origin, the triangle and the instance falls under that byte, so a canopy stops
// that share of the rays and casts a dappled shadow of its density.
//
// Limit: no mask texture is bound on this trace, so `any_hit` does not fetch one. The mask is
// the per-draw coverage already stored on the instance. A cutout that exists only as a texture
// is not read. A candidate `any_hit` rejects is not committed; if nothing else confirms, the
// ray is a miss.

@group(0) @binding(3) var scene_tlas: acceleration_structure;

// PCG-style integer hash (O'Neill, "PCG: A Family of Simple Fast Space-Efficient Statistically
// Good Algorithms for Random Number Generation", 2014; the output permutation alone).
// The same function as `masked_hash` in `bvh_walk.wgsl` and `hwrt::bvh::masked_hash`.
fn masked_hash(v: u32) -> u32 {
    let state = v * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

// Any-hit alpha test for one non-opaque candidate. Opaque geometry never reaches here.
// `coverage` is the low 8 bits of the instance custom data (`coverage_custom_data`).
fn any_hit(o: vec3<f32>, primitive_index: u32, instance_index: u32, coverage: u32) -> bool {
    // Byte 255 is fully covered, including when the geometry is non-opaque.
    if coverage >= 255u {
        return true;
    }
    let seed = masked_hash(bitcast<u32>(o.x) ^ masked_hash(bitcast<u32>(o.y) ^ masked_hash(bitcast<u32>(o.z))));
    let roll = masked_hash(seed ^ (primitive_index * 2654435761u) ^ instance_index) & 0xFFu;
    return roll < coverage;
}

fn sun_visibility(o: vec3<f32>, d: vec3<f32>, t_min: f32, t_max: f32) -> f32 {
    var rq: ray_query;
    rayQueryInitialize(&rq, scene_tlas, RayDesc(RAY_FLAG_TERMINATE_ON_FIRST_HIT, 0xFFu, t_min, t_max, o, d));
    while rayQueryProceed(&rq) {
        // Only non-opaque (masked) geometry reaches here as a candidate. Opaque casters skip any_hit.
        let candidate = rayQueryGetCandidateIntersection(&rq);
        if candidate.kind == RAY_QUERY_INTERSECTION_TRIANGLE {
            let coverage = candidate.instance_custom_data & 0xFFu;
            if any_hit(o, candidate.primitive_index, candidate.instance_index, coverage) {
                rayQueryConfirmIntersection(&rq);
            }
        }
    }
    let hit = rayQueryGetCommittedIntersection(&rq);
    return select(1.0, 0.0, hit.kind != RAY_QUERY_INTERSECTION_NONE);
}
