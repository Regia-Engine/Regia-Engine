// SPDX-License-Identifier: MPL-2.0
// Shadow rays without hardware ray queries (ADR-515): one any-hit ray per sample point toward a
// directional light, traced through the stackless BVH built by `hwrt::bvh`. The walk itself is
// `src/hwrt/bvh_walk.wgsl`, which `FALLBACK_SHADOW_WGSL` appends to this file.

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
@group(0) @binding(1) var<storage, read> nodes: array<BvhNode>;
// Three vec4 per triangle (w unused), in leaf order.
@group(0) @binding(2) var<storage, read> tris: array<vec4<f32>>;
// World-space sample points (w unused).
@group(0) @binding(3) var<storage, read> origins: array<vec4<f32>>;
// 1.0 lit, 0.0 shadowed.
@group(0) @binding(4) var<storage, read_write> visibility: array<f32>;

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
    visibility[s] = select(1.0, 0.0, bvh_occluded(o, params.light_dir, params.t_min, params.t_max, params.node_count));
}
