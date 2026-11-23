// SPDX-License-Identifier: MPL-2.0
// Visibility pass (Burns & Hunt, "The Visibility Buffer: A Cache-Friendly Approach to Deferred
// Shading", JCGT 2013): one instanced draw per cull phase, one instance per selected cluster,
// 124 × 3 vertices each. The vertex shader pulls the cluster's triangle from storage; slots past
// the cluster's triangle count collapse to a point outside the clip volume. The fragment writes
// `phase << 31 | list slot << 7 | triangle` into an R32Uint target with the scene depth test.
// Double-sided clusters sit at the end of the phase's list (`vis_cull.wgsl`): a second draw
// with the cull-none pipeline reads them backwards from there (`vs_vis_reversed`).

struct RasterParams {
    view_proj: mat4x4<f32>,
    // x = cluster capacity per phase (offset of the phase-2 list), y = phase (0 or 1), z = the
    // material arena's record stride in words, w = the masked lists' header entry in `clusters`
    // (`vis_cull.wgsl`, ADR-3915).
    info: vec4<u32>,
    // The alpha test of `vis_raster_masked.wgsl`: x = the texture-index word of an arena record,
    // y = the frame's texture mip bias (f32 bits).
    mask: vec4<u32>,
}

struct Instance {
    model: mat4x4<f32>,
    sphere: vec4<f32>,
    info: vec4<u32>,
    // x = largest axis scale, y = sway reach, z and w = where its voxel far levels start (ADR-3916).
    scale: vec4<f32>,
    // x = shadow scale, y = shadow lift (world Y), z = shadow reach.
    shadow: vec4<f32>,
    // x = shadow flags (`vis_shadow.wgsl`), y from bit 8 = the sway record + 1 (0: no sway).
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

@group(0) @binding(0) var<uniform> params: RasterParams;
@group(0) @binding(1) var<storage, read> instances: array<Instance>;
@group(0) @binding(2) var<storage, read> meshlets: array<Meshlet>;
// Vertices (16 words each: position, uv.x, normal, uv.y, tangent, colour), cluster vertex
// lists (absolute word offsets of vertices) and packed triangles, all as u32 words.
@group(0) @binding(3) var<storage, read> geo: array<u32>;
@group(0) @binding(4) var<storage, read> clusters: array<vec2<u32>>;
// The swaying materials of this frame (ADR-3916).
@group(0) @binding(5) var<uniform> sway_table: regia::foliage_sway::SwayTable;

// `world` (a vertex at geometry word `v`) with its instance's sway. A voxel far cluster (`pad`) and an
// instance without sway stay put.
fn swayed(inst: Instance, m: Meshlet, v: u32, world: vec3<f32>) -> vec3<f32> {
    let record = inst.flags.y >> 8u;
    if (record == 0u || m.pad != 0u) {
        return world;
    }
    let normal = vec3<f32>(bitcast<f32>(geo[v + 4u]), bitcast<f32>(geo[v + 5u]), bitcast<f32>(geo[v + 6u]));
    return world + regia::foliage_sway::cluster_offset(
        sway_table.records[record - 1u],
        sway_table.frame,
        inst.sphere.xyz,
        inst.scale,
        sway_table.records[record - 1u].clock.x,
        world,
        (inst.model * vec4<f32>(normal, 0.0)).xyz,
        bitcast<f32>(geo[v + 12u]),
        inst.model[3].xyz,
    );
}

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) @interpolate(flat) id: u32,
}

@vertex
fn vs_vis(@builtin(vertex_index) vi: u32, @builtin(instance_index) slot: u32) -> VsOut {
    return cluster_vertex(vi, slot);
}

// The double-sided clusters: instance `slot` is list slot `capacity − 1 − slot`.
@vertex
fn vs_vis_reversed(@builtin(vertex_index) vi: u32, @builtin(instance_index) slot: u32) -> VsOut {
    return cluster_vertex(vi, params.info.x - 1u - slot);
}

fn cluster_vertex(vi: u32, slot: u32) -> VsOut {
    let entry = clusters[params.info.y * params.info.x + slot];
    let m = meshlets[entry.y];
    let tri = vi / 3u;
    // Results are built with the constructor: no function-local struct `var` (ADR-026).
    if (tri >= m.triangle_count) {
        return VsOut(vec4<f32>(0.0, 0.0, -1.0, 1.0), 0xffffffffu);
    }
    let packed = geo[m.triangle_offset + tri];
    let local = (packed >> ((vi % 3u) * 8u)) & 0xffu;
    let v = geo[m.vertex_offset + local];
    let position = vec3<f32>(bitcast<f32>(geo[v]), bitcast<f32>(geo[v + 1u]), bitcast<f32>(geo[v + 2u]));
    // Same transform order as the forward vertex shader, so depth matches it.
    let inst = instances[entry.x];
    let world = swayed(inst, m, v, (inst.model * vec4<f32>(position, 1.0)).xyz);
    return VsOut(params.view_proj * vec4<f32>(world, 1.0), (params.info.y << 31u) | (slot << 7u) | tri);
}

@fragment
fn fs_vis(in: VsOut) -> @location(0) u32 {
    return in.id;
}

// The empty ID by a draw (ADR-5807): DX12 clears color targets through `f32` values, which cannot
// hold `0xffffffff`, so on such a backend phase 1 starts with this full-screen triangle (depth test
// always, no depth write) instead of a clear to the empty ID.
@vertex
fn vs_vis_fill(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    return vec4<f32>(uv * 2.0 - vec2<f32>(1.0), 0.0, 1.0);
}

@fragment
fn fs_vis_fill() -> @location(0) u32 {
    return 0xffffffffu;
}

// Masked clusters (ADR-3915): their vertex stages read the list after the opaque part (the header
// `vis_cull.wgsl` writes), and pass the UV, vertex alpha and material slot to the alpha test in
// `vis_raster_masked.wgsl`. Without the texture table bound in a frame, `fs_vis_untested` discards
// every fragment: an untested card must write neither a visibility ID nor occluding depth.
struct VsMasked {
    @builtin(position) clip: vec4<f32>,
    @location(0) @interpolate(flat) id: u32,
    @location(1) uv: vec2<f32>,
    // Vertex color alpha.
    @location(2) color_a: f32,
    // The instance's material slot.
    @location(3) @interpolate(flat) material: u32,
}

// The masked back-face-culled clusters: instance `slot` is list slot `opaque + slot`.
@vertex
fn vs_vis_masked(@builtin(vertex_index) vi: u32, @builtin(instance_index) slot: u32) -> VsMasked {
    let header = clusters[params.info.w + params.info.y];
    return masked_vertex(vi, header.x + slot);
}

// The masked double-sided clusters, read backwards from the end of the list after the opaque ones.
@vertex
fn vs_vis_masked_reversed(@builtin(vertex_index) vi: u32, @builtin(instance_index) slot: u32) -> VsMasked {
    let header = clusters[params.info.w + params.info.y];
    return masked_vertex(vi, params.info.x - 1u - (header.y + slot));
}

fn masked_vertex(vi: u32, slot: u32) -> VsMasked {
    let entry = clusters[params.info.y * params.info.x + slot];
    let m = meshlets[entry.y];
    let tri = vi / 3u;
    // Built with the constructor: no function-local struct `var` (ADR-026).
    if (tri >= m.triangle_count) {
        return VsMasked(vec4<f32>(0.0, 0.0, -1.0, 1.0), 0xffffffffu, vec2<f32>(0.0), 0.0, 0u);
    }
    let packed = geo[m.triangle_offset + tri];
    let local = (packed >> ((vi % 3u) * 8u)) & 0xffu;
    let v = geo[m.vertex_offset + local];
    let position = vec3<f32>(bitcast<f32>(geo[v]), bitcast<f32>(geo[v + 1u]), bitcast<f32>(geo[v + 2u]));
    let uv = vec2<f32>(bitcast<f32>(geo[v + 3u]), bitcast<f32>(geo[v + 7u]));
    let inst = instances[entry.x];
    // Same transform order as `cluster_vertex` and the forward vertex shader.
    let world = swayed(inst, m, v, (inst.model * vec4<f32>(position, 1.0)).xyz);
    return VsMasked(
        params.view_proj * vec4<f32>(world, 1.0),
        (params.info.y << 31u) | (slot << 7u) | tri,
        uv,
        bitcast<f32>(geo[v + 15u]),
        inst.info.w,
    );
}

@fragment
fn fs_vis_untested(in: VsMasked) -> @location(0) u32 {
    discard;
    return in.id;
}
