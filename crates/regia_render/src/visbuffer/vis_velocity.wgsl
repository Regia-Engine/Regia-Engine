// SPDX-License-Identifier: MPL-2.0
// Motion vectors of the visibility-buffer pixels for TAAU, as `velocity.wgsl` computes them for
// forward meshes (Karis, SIGGRAPH 2014): the surface point under the pixel this frame (model)
// and last frame (previous model), projected with the unjittered current and the previous
// view-projection. One full-screen pass instead of one draw per instance; the point comes from
// the pixel's triangle with perspective-correct barycentrics (Olano & Greer 1997), as in
// `vis_resolve.wgsl`.

struct VelocityParams {
    // The visibility pass's (jittered) view-projection: the barycentrics of this pixel.
    view_proj: mat4x4<f32>,
    unjittered: mat4x4<f32>,
    prev_view_proj: mat4x4<f32>,
    // x = cluster capacity per phase.
    info: vec4<u32>,
    // xy = target size in pixels.
    screen: vec4<f32>,
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

@group(0) @binding(0) var<uniform> params: VelocityParams;
@group(0) @binding(1) var<storage, read> instances: array<Instance>;
@group(0) @binding(2) var<storage, read> meshlets: array<Meshlet>;
@group(0) @binding(3) var<storage, read> geo: array<u32>;
@group(0) @binding(4) var<storage, read> clusters: array<vec2<u32>>;
@group(0) @binding(5) var<storage, read> prev_models: array<mat4x4<f32>>;
@group(0) @binding(6) var ids: texture_2d<u32>;
// The swaying materials of this frame (ADR-3916).
@group(0) @binding(7) var<uniform> sway_table: regia::foliage_sway::SwayTable;
// The frame's scene depth (standard Z). The ids keep a visibility pixel that a forward mesh drew
// over later; that pixel keeps the mesh's motion (`velocity.wgsl`, same relative tolerance).
@group(1) @binding(0) var scene_depth: texture_depth_2d;
const OCCLUSION_TOLERANCE: f32 = 0.99;
// Absolute slack in (1 - z): at multi-km range 1 % of (1 - z) is only a few Depth32 ULPs, so rounding alone could
// discard a visible fragment (review v014, finding 1).
const OCCLUSION_ABS_SLACK: f32 = 2.0e-6;

@vertex
fn vs_fullscreen(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    return vec4<f32>(uv * 2.0 - vec2<f32>(1.0), 0.0, 1.0);
}

fn position_of(m: Meshlet, packed: u32, corner: u32) -> vec3<f32> {
    let v = geo[m.vertex_offset + ((packed >> (corner * 8u)) & 0xffu)];
    return vec3<f32>(bitcast<f32>(geo[v]), bitcast<f32>(geo[v + 1u]), bitcast<f32>(geo[v + 2u]));
}

// The offset of the rest point `world` of corner `corner` under `model`, as `velocity.wgsl`'s
// `graph_offset` moves it: this frame's wind and fade, the clock and the model of the frame asked for.
// Zero in calm air, so a calm plant's motion is a still plant's to the bit (ADR-3916).
fn sway_offset(
    inst: Instance,
    m: Meshlet,
    packed: u32,
    corner: u32,
    model: mat4x4<f32>,
    world: vec3<f32>,
    time: f32,
) -> vec3<f32> {
    let record = inst.flags.y >> 8u;
    if (record == 0u || m.pad != 0u) {
        return vec3<f32>(0.0);
    }
    let v = geo[m.vertex_offset + ((packed >> (corner * 8u)) & 0xffu)];
    let normal = vec3<f32>(bitcast<f32>(geo[v + 4u]), bitcast<f32>(geo[v + 5u]), bitcast<f32>(geo[v + 6u]));
    return regia::foliage_sway::cluster_offset(
        sway_table.records[record - 1u],
        sway_table.frame,
        inst.sphere.xyz,
        inst.scale,
        time,
        world,
        (model * vec4<f32>(normal, 0.0)).xyz,
        bitcast<f32>(geo[v + 12u]),
        model[3].xyz,
    );
}

fn uv_of(clip: vec4<f32>) -> vec2<f32> {
    let ndc = clip.xy / max(abs(clip.w), 1.0e-6);
    return vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
}

@fragment
fn fs_velocity(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let id = textureLoad(ids, vec2<i32>(pos.xy), 0).r;
    if (id == 0xffffffffu) {
        discard;
    }
    let entry = clusters[(id >> 31u) * params.info.x + ((id >> 7u) & 0xffffffu)];
    let inst = instances[entry.x];
    let model = inst.model;
    let prev = prev_models[entry.x];
    let m = meshlets[entry.y];
    let packed = geo[m.triangle_offset + (id & 127u)];
    let p0 = position_of(m, packed, 0u);
    let p1 = position_of(m, packed, 1u);
    let p2 = position_of(m, packed, 2u);
    let record = inst.flags.y >> 8u;
    var now = 0.0;
    var then = 0.0;
    if (record != 0u) {
        now = sway_table.records[record - 1u].clock.x;
        then = sway_table.records[record - 1u].clock.y;
    }
    let r0 = (model * vec4<f32>(p0, 1.0)).xyz;
    let r1 = (model * vec4<f32>(p1, 1.0)).xyz;
    let r2 = (model * vec4<f32>(p2, 1.0)).xyz;
    let w0 = r0 + sway_offset(inst, m, packed, 0u, model, r0, now);
    let w1 = r1 + sway_offset(inst, m, packed, 1u, model, r1, now);
    let w2 = r2 + sway_offset(inst, m, packed, 2u, model, r2, now);
    let c0 = (params.view_proj * vec4<f32>(w0, 1.0)).xyw;
    let c1 = (params.view_proj * vec4<f32>(w1, 1.0)).xyw;
    let c2 = (params.view_proj * vec4<f32>(w2, 1.0)).xyw;
    let size = max(params.screen.xy, vec2<f32>(1.0));
    let q = vec3<f32>(pos.x / size.x * 2.0 - 1.0, 1.0 - pos.y / size.y * 2.0, 1.0);
    let e = vec3<f32>(dot(cross(c1, c2), q), dot(cross(c2, c0), q), dot(cross(c0, c1), q));
    let b = e / (e.x + e.y + e.z);
    let world = w0 * b.x + w1 * b.y + w2 * b.z;
    let clip = params.view_proj * vec4<f32>(world, 1.0);
    let dsize = vec2<i32>(textureDimensions(scene_depth));
    let scene = textureLoad(scene_depth, clamp(vec2<i32>(pos.xy), vec2<i32>(0), dsize - 1), 0);
    if (1.0 - clip.z / clip.w < (1.0 - scene) * OCCLUSION_TOLERANCE - OCCLUSION_ABS_SLACK) {
        discard;
    }
    var prev_world = (prev * vec4<f32>(p0 * b.x + p1 * b.y + p2 * b.z, 1.0)).xyz;
    if (record != 0u) {
        // The surface point last frame: each corner at its own previous offset, added to the rest
        // point so that a zero offset leaves it as it is without sway.
        prev_world = prev_world
            + sway_offset(inst, m, packed, 0u, prev, (prev * vec4<f32>(p0, 1.0)).xyz, then) * b.x
            + sway_offset(inst, m, packed, 1u, prev, (prev * vec4<f32>(p1, 1.0)).xyz, then) * b.y
            + sway_offset(inst, m, packed, 2u, prev, (prev * vec4<f32>(p2, 1.0)).xyz, then) * b.z;
    }
    let motion = uv_of(params.prev_view_proj * vec4<f32>(prev_world, 1.0))
        - uv_of(params.unjittered * vec4<f32>(world, 1.0));
    return vec4<f32>(motion, 0.0, 1.0);
}
