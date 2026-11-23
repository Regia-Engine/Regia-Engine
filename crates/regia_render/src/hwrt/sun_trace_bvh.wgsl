// SPDX-License-Identifier: MPL-2.0

// The compute BVH over the frame's casters: the reference path, on every adapter.

@group(0) @binding(3) var<storage, read> nodes: array<BvhNode>;
// Three vec4 per triangle, in leaf order. The first corner's w is the any-hit coverage
// (`bvh_walk.wgsl`); 0 is opaque.
@group(0) @binding(4) var<storage, read> tris: array<vec4<f32>>;

fn sun_visibility(o: vec3<f32>, d: vec3<f32>, t_min: f32, t_max: f32) -> f32 {
    return select(1.0, 0.0, bvh_occluded(o, d, t_min, t_max, params.counts.x));
}
