// SPDX-License-Identifier: MPL-2.0
// Depth-only draw of one shadow job's selected clusters (`vis_shadow.wgsl` picks them): one
// instance per cluster, 124 × 3 vertices each, pulled from storage as `vis_raster.wgsl` does,
// placed with the instance's shadow model and the job's view-projection. No fragment stage.
// The job's double-sided clusters sit at the end of the list, counted backwards from there
// (`vis_shadow.wgsl`): `vs_shadow_reversed` draws them with the cull-none pipeline.

struct ShadowJob {
    view_proj: mat4x4<f32>,
    planes: array<vec4<f32>, 4>,
    lod: vec4<f32>,
    light: vec4<f32>,
    camera: vec4<f32>,
    splits: vec4<f32>,
    info: vec4<u32>,
    caps: vec4<u32>,
}

struct Instance {
    model: mat4x4<f32>,
    sphere: vec4<f32>,
    info: vec4<u32>,
    scale: vec4<f32>,
    shadow: vec4<f32>,
    flags: vec4<u32>,
}

struct Meshlet {
    bounds: vec4<f32>,
    cone: vec4<f32>,
    lod: vec4<f32>,
    parent: vec4<f32>,
    lod_error: f32,
    parent_error: f32,
    vertex_offset: u32,
    triangle_offset: u32,
    vertex_count: u32,
    triangle_count: u32,
    level: u32,
    pad: u32,
}

@group(0) @binding(0) var<uniform> job: ShadowJob;
@group(0) @binding(1) var<storage, read> instances: array<Instance>;
@group(0) @binding(2) var<storage, read> meshlets: array<Meshlet>;
@group(0) @binding(3) var<storage, read> geo: array<u32>;
@group(0) @binding(4) var<storage, read> clusters: array<vec2<u32>>;
// The job records of `vis_shadow.wgsl` (12 words after a 16-word header; word 6 of a record is
// its first cluster, word 7 its first double-sided cluster counted from the list's end).
@group(0) @binding(5) var<storage, read> jobs: array<u32>;
// The swaying materials of this frame (ADR-3916).
@group(0) @binding(6) var<uniform> sway_table: regia::foliage_sway::SwayTable;

@vertex
fn vs_shadow(@builtin(vertex_index) vi: u32, @builtin(instance_index) slot: u32) -> @builtin(position) vec4<f32> {
    return cluster_vertex(vi, clusters[jobs[16u + job.info.x * 12u + 6u] + slot]);
}

@vertex
fn vs_shadow_reversed(@builtin(vertex_index) vi: u32, @builtin(instance_index) slot: u32) -> @builtin(position) vec4<f32> {
    return cluster_vertex(vi, clusters[job.info.w - 1u - (jobs[16u + job.info.x * 12u + 7u] + slot)]);
}

fn cluster_vertex(vi: u32, entry: vec2<u32>) -> vec4<f32> {
    let m = meshlets[entry.y];
    let tri = vi / 3u;
    if (tri >= m.triangle_count) {
        return vec4<f32>(0.0, 0.0, -1.0, 1.0);
    }
    let packed = geo[m.triangle_offset + tri];
    let local = (packed >> ((vi % 3u) * 8u)) & 0xffu;
    let v = geo[m.vertex_offset + local];
    let position = vec3<f32>(bitcast<f32>(geo[v]), bitcast<f32>(geo[v + 1u]), bitcast<f32>(geo[v + 2u]));
    let inst = instances[entry.x];
    let world = (inst.model * vec4<f32>(position, 1.0)).xyz;
    let origin = inst.model[3].xyz;
    let shadow_world = origin + (world - origin) * inst.shadow.x + vec3<f32>(0.0, inst.shadow.y, 0.0);
    let record = inst.flags.y >> 8u;
    if (record == 0u || m.pad != 0u) {
        return job.view_proj * vec4<f32>(shadow_world, 1.0);
    }
    // As the forward shadow pass sways: from the shadow model's rest point and origin, with the
    // voxel fade of the main view, so a caster casts the pose the camera sees.
    let normal = vec3<f32>(bitcast<f32>(geo[v + 4u]), bitcast<f32>(geo[v + 5u]), bitcast<f32>(geo[v + 6u]));
    let moved = shadow_world + regia::foliage_sway::cluster_offset(
        sway_table.records[record - 1u],
        sway_table.frame,
        inst.sphere.xyz,
        inst.scale,
        sway_table.records[record - 1u].clock.x,
        shadow_world,
        (inst.model * vec4<f32>(normal, 0.0)).xyz,
        bitcast<f32>(geo[v + 12u]),
        origin + vec3<f32>(0.0, inst.shadow.y, 0.0),
    );
    return job.view_proj * vec4<f32>(moved, 1.0);
}
