// SPDX-License-Identifier: MPL-2.0
// Shadow rays with hardware ray queries (ADR-515): the same contract as
// `hwrt_shadow_fallback.wgsl` (one any-hit ray per sample toward a directional light, 1.0 lit,
// 0.0 shadowed), traced against a top-level acceleration structure. Needs a device with
// `EXPERIMENTAL_RAY_QUERY`; naga validates it without one.

enable wgpu_ray_query;

struct Params {
    // Unit direction toward the light.
    light_dir: vec3<f32>,
    t_min: f32,
    t_max: f32,
    sample_count: u32,
    node_count: u32,
    pad: u32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var scene: acceleration_structure;
// World-space sample points (w unused).
@group(0) @binding(2) var<storage, read> origins: array<vec4<f32>>;
// 1.0 lit, 0.0 shadowed.
@group(0) @binding(3) var<storage, read_write> visibility: array<f32>;

fn occluded(o: vec3<f32>, d: vec3<f32>) -> bool {
    var rq: ray_query;
    let flags = RAY_FLAG_TERMINATE_ON_FIRST_HIT | RAY_FLAG_FORCE_OPAQUE;
    rayQueryInitialize(&rq, scene, RayDesc(flags, 0xFFu, params.t_min, params.t_max, o, d));
    // Every instance is opaque, so the first proceed finishes the walk; the loop stays correct
    // if a candidate ever needs another step.
    while rayQueryProceed(&rq) {}
    let hit = rayQueryGetCommittedIntersection(&rq);
    return hit.kind != RAY_QUERY_INTERSECTION_NONE;
}

@compute @workgroup_size(64)
fn trace_shadows(
    @builtin(global_invocation_id) gid: vec3<u32>,
    @builtin(num_workgroups) groups: vec3<u32>,
) {
    let s = gid.y * groups.x * 64u + gid.x;
    if s >= params.sample_count {
        return;
    }
    let o = origins[s].xyz;
    visibility[s] = select(1.0, 0.0, occluded(o, params.light_dir));
}
