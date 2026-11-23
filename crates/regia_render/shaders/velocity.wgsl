// SPDX-License-Identifier: MPL-2.0
// Per-object motion vectors for TAAU. Current clip is the jittered projection
// (so the vector lands on the raster that produced the colour); the UV
// difference uses the unjittered current matrix, so the jitter itself is not
// motion. Karis, SIGGRAPH 2014.
#ifdef MATERIAL_GRAPH_VERTEX
#import regia::pbr::surface
#else
#import regia::mesh
#endif

struct VelocityFrame {
    view_proj: mat4x4<f32>,
    unjittered: mat4x4<f32>,
    prev_view_proj: mat4x4<f32>,
    // Inverse of `view_proj` (the jittered matrix the depth buffer was written with).
    inv_view_proj: mat4x4<f32>,
    // x = 1 when `occlusion_depth` holds this frame's single-sample scene depth.
    occlusion: vec4<f32>,
    // Time, previous time, wind x, wind z (ADR-5160).
    clock: vec4<f32>,
    // x > 0.5: graphs read `clock` instead of material slot 0.
    clock_live: vec4<f32>,
}

struct MdiObject {
    model: mat4x4<f32>,
    prev_model: mat4x4<f32>,
    skin: vec4<u32>,
    pad: vec4<u32>,
}

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) tangent: vec4<f32>,
    @location(4) color: vec4<f32>,
    @location(5) joints: vec4<u32>,
    @location(6) weights: vec4<f32>,
#ifdef MATERIAL_GRAPH_UV1
    @location(7) uv1: vec2<f32>,
#endif
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) motion: vec2<f32>,
    // The object's draw-order key (`pad.x`, f32 bits), written as fragment depth: the
    // pass keeps the latest draw at each pixel, so one mesh's objects draw as a batch.
    @location(1) @interpolate(flat) order: f32,
    // View depth (clip w) of the surface that wrote this motion. TAAU reads only
    // `xy`; the TSR upscaler compares `z` with the scene depth, because this pass
    // keeps the latest draw, not the nearest surface.
    @location(2) view_depth: f32,
}

struct FragmentOutput {
    @location(0) motion: vec4<f32>,
    @builtin(frag_depth) order: f32,
}

@group(0) @binding(0) var<uniform> frame: VelocityFrame;
// The scene depth (standard Z, written with `frame.view_proj`). This pass's own depth attachment
// holds draw-order keys, not depth, so without this a moving mesh hidden behind a wall wrote its
// motion onto the wall's pixels, and TAAU fetched the wall's history from the wrong place.
@group(0) @binding(1) var occlusion_depth: texture_depth_2d;
@group(1) @binding(0) var<storage, read> objects: array<MdiObject>;

#ifdef MATERIAL_GRAPH_VERTEX
// A material graph's world position offset moves the surface: the vector is the difference of
// the displaced positions, this frame's offset at `graph[0].x` and last frame's at `graph[0].y`.
struct GraphMaterial {
    standard: array<vec4<f32>, 22>,
    graph: array<vec4<f32>, 16>,
}
// `MATERIAL_SSBO`: the material record in storage (`regia_render::bindless`, ADR-316).
#ifdef MATERIAL_SSBO
@group(2) @binding(0) var<storage, read> material: GraphMaterial;
#else
@group(2) @binding(0) var<uniform> material: GraphMaterial;
#endif
@group(2) @binding(1) var albedo_tex: texture_2d<f32>;
@group(2) @binding(2) var tex_samp: sampler;
@group(2) @binding(3) var normal_tex: texture_2d<f32>;
@group(2) @binding(4) var occlusion_tex: texture_2d<f32>;
@group(2) @binding(5) var emissive_tex: texture_2d<f32>;
@group(2) @binding(6) var detail_tex: texture_2d<f32>;
@group(2) @binding(7) var detail_normal: texture_2d<f32>;
@group(2) @binding(8) var aux_tex: texture_2d<f32>;
fn graph_clock() -> vec4<f32> {
    if (frame.clock_live.x > 0.5) {
        return frame.clock;
    }
    return material.graph[0];
}
// @material_graph_vertex_source

fn graph_offset(model: mat4x4<f32>, rest: vec3<f32>, normal: vec3<f32>, uv: vec2<f32>, uv1: vec2<f32>, color: vec4<f32>, time: f32) -> vec3<f32> {
    let nrm = (model * vec4<f32>(normal, 0.0)).xyz;
    return material_vertex_offset(regia::pbr::surface::VertexOffsetInput(
        rest,
        nrm * inverseSqrt(max(dot(nrm, nrm), 1e-12)),
        uv,
        uv1,
        color,
        model[3].xyz,
        time,
    ));
}
#endif

fn uv_of(clip: vec4<f32>) -> vec2<f32> {
    let ndc = clip.xy / max(abs(clip.w), 1.0e-6);
    return vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
}

@vertex
fn vs_main(in: VertexInput, @builtin(instance_index) instance: u32) -> VertexOutput {
    let object = objects[instance];
#ifdef MATERIAL_GRAPH_VERTEX
#ifdef MATERIAL_GRAPH_UV1
    let uv1 = in.uv1;
#else
    let uv1 = in.uv;
#endif
    let rest = (object.model * vec4<f32>(in.position, 1.0)).xyz;
    let prev_rest = (object.prev_model * vec4<f32>(in.position, 1.0)).xyz;
    let world = rest + graph_offset(object.model, rest, in.normal, in.uv, uv1, in.color, graph_clock().x);
    let prev_world = prev_rest
        + graph_offset(object.prev_model, prev_rest, in.normal, in.uv, uv1, in.color, graph_clock().y);
#else
    let world = (object.model * vec4<f32>(in.position, 1.0)).xyz;
    let prev_world = (object.prev_model * vec4<f32>(in.position, 1.0)).xyz;
#endif
    let clip = frame.view_proj * vec4<f32>(world, 1.0);
    let current_uv = uv_of(frame.unjittered * vec4<f32>(world, 1.0));
    let prev_uv = uv_of(frame.prev_view_proj * vec4<f32>(prev_world, 1.0));
    var motion = prev_uv - current_uv;
    let _keep = in.normal.x + in.uv.x + in.tangent.x + in.color.x + f32(in.joints.x) + in.weights.x + f32(object.skin.x);
    motion = motion + vec2<f32>(_keep * 0.0);
    return VertexOutput(clip, motion, bitcast<f32>(object.pad.x), clip.w);
}

#ifndef MATERIAL_GRAPH_VERTEX
// Joint palettes of the skinned objects drawn by `vs_skinned`: each object's palette of this
// frame starts at `skin.x`, last frame's at `skin.y` (the current one again on an object's first
// frame or after its joint count changed, so only the model matrix moves it).
@group(1) @binding(1) var<storage, read> velocity_joints: array<regia::mesh::Mat3x4>;

// Linear-blend skinning of the rest position with the palette at `base` (`lab_forward.wgsl`'s
// `skin_vertex`: a vertex with no weight keeps its rest position).
fn skin_at(base: u32, in: VertexInput) -> vec3<f32> {
    return regia::mesh::skin_position(
        in.position,
        velocity_joints[base + in.joints.x],
        velocity_joints[base + in.joints.y],
        velocity_joints[base + in.joints.z],
        velocity_joints[base + in.joints.w],
        in.weights,
    );
}

// A skinned mesh: the vector is the difference of the position skinned by this frame's palette
// and model and the position skinned by last frame's palette and model.
@vertex
fn vs_skinned(in: VertexInput, @builtin(instance_index) instance: u32) -> VertexOutput {
    let object = objects[instance];
    let world = (object.model * vec4<f32>(skin_at(object.skin.x, in), 1.0)).xyz;
    let prev_world = (object.prev_model * vec4<f32>(skin_at(object.skin.y, in), 1.0)).xyz;
    let clip = frame.view_proj * vec4<f32>(world, 1.0);
    let current_uv = uv_of(frame.unjittered * vec4<f32>(world, 1.0));
    let prev_uv = uv_of(frame.prev_view_proj * vec4<f32>(prev_world, 1.0));
    var motion = prev_uv - current_uv;
    let _keep = in.normal.x + in.uv.x + in.tangent.x + in.color.x;
    motion = motion + vec2<f32>(_keep * 0.0);
    return VertexOutput(clip, motion, bitcast<f32>(object.pad.x), clip.w);
}

// A mesh skinned by the compute pass (`skin_compute.wgsl`): this frame's skinned vertices in
// buffer 0, last frame's positions in buffer 1 (the same `LabVertex` stride), so the vector is
// the motion of the deformation without skinning again.
@vertex
fn vs_preskinned(
    in: VertexInput,
    @location(7) prev_position: vec3<f32>,
    @builtin(instance_index) instance: u32,
) -> VertexOutput {
    let object = objects[instance];
    let world = (object.model * vec4<f32>(in.position, 1.0)).xyz;
    let prev_world = (object.prev_model * vec4<f32>(prev_position, 1.0)).xyz;
    let clip = frame.view_proj * vec4<f32>(world, 1.0);
    let current_uv = uv_of(frame.unjittered * vec4<f32>(world, 1.0));
    let prev_uv = uv_of(frame.prev_view_proj * vec4<f32>(prev_world, 1.0));
    var motion = prev_uv - current_uv;
    let _keep = in.normal.x + in.uv.x + in.tangent.x + in.color.x + f32(in.joints.x) + in.weights.x;
    motion = motion + vec2<f32>(_keep * 0.0);
    return VertexOutput(clip, motion, bitcast<f32>(object.pad.x), clip.w);
}
#endif

// A velocity fragment more than ~1 % of its view depth behind the visible surface is hidden, and
// one as far in front of it was not drawn (ADR-1449): with standard Z, `1 − z` is proportional to
// `(far − d) / d`, so the test is relative in depth.
const OCCLUSION_TOLERANCE: f32 = 0.99;
// Absolute slack in (1 - z): at multi-km range 1 % of (1 - z) is only a few Depth32 ULPs, so rounding alone could
// discard a visible fragment (review v014, finding 1).
const OCCLUSION_ABS_SLACK: f32 = 2.0e-6;

@fragment
fn fs_main(in: VertexOutput) -> FragmentOutput {
    if (frame.occlusion.x > 0.5) {
        let size = vec2<i32>(textureDimensions(occlusion_depth));
        let p = clamp(vec2<i32>(in.clip_position.xy), vec2<i32>(0), size - 1);
        let scene = textureLoad(occlusion_depth, p, 0);
        if (1.0 - in.clip_position.z < (1.0 - scene) * OCCLUSION_TOLERANCE - OCCLUSION_ABS_SLACK) {
            discard;
        }
        // In front of the visible surface by the same margin: the forward pass did not keep this
        // fragment (an alpha-tested, hashed or LOD-dithered texel), so what shows there is the
        // surface behind it and the motion stays that surface's. This pass has no material, so a
        // twig card wrote its tree's motion over its whole quad, onto the ground seen through it,
        // and TAAU fetched that ground's history from where the tree moved (look-2 finding 3:
        // vertical smears under every crown while the scatter seat followed the terrain morph).
        if (1.0 - in.clip_position.z > (1.0 - scene) / OCCLUSION_TOLERANCE + OCCLUSION_ABS_SLACK) {
            discard;
        }
    }
    return FragmentOutput(vec4<f32>(in.motion, in.view_depth, 1.0), in.order);
}

// Camera motion of static geometry from the depth buffer (`RenderProfile::depth_velocity`):
// one fullscreen pass instead of a velocity draw per static mesh. Moving meshes draw on top.
@group(2) @binding(0) var scene_depth: texture_depth_2d;

struct FullOut {
    @builtin(position) pos: vec4<f32>,
}

@vertex
fn vs_full(@builtin(vertex_index) i: u32) -> FullOut {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return FullOut(vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0));
}

// Motion of the sky at `ndc_xy`: the view ray reprojected as a direction (w = 0), so the
// view's rotation moves it and its translation does not (the sky is infinitely far; the
// direction is the reprojection of a point at infinity, Hartley & Zisserman, "Multiple View
// Geometry", 2nd ed., § 8.6). The ray runs from the near-plane point to the far-plane point
// of the pixel, cross-multiplied by their w so that an infinite far plane (w = 0) works too.
// Zero (the old sky value) for a ray with no perspective w (orthographic) or one behind
// either view.
fn sky_motion(ndc_xy: vec2<f32>) -> vec2<f32> {
    let near_h = frame.inv_view_proj * vec4<f32>(ndc_xy, 0.0, 1.0);
    let far_h = frame.inv_view_proj * vec4<f32>(ndc_xy, 1.0, 1.0);
    let dir = vec4<f32>(far_h.xyz * near_h.w - near_h.xyz * far_h.w, 0.0);
    let cur = frame.unjittered * dir;
    let prev = frame.prev_view_proj * dir;
    if (cur.w <= 1.0e-6 * length(dir.xyz) || prev.w <= 1.0e-6 * length(dir.xyz)) {
        return vec2<f32>(0.0);
    }
    return uv_of(prev) - uv_of(cur);
}

@fragment
fn fs_camera(in: FullOut) -> @location(0) vec4<f32> {
    let dims = vec2<f32>(textureDimensions(scene_depth));
    let texel = vec2<i32>(in.pos.xy);
    let depth = textureLoad(scene_depth, texel, 0);
    let uv = in.pos.xy / dims;
    if (depth >= 1.0) {
        // Sky: no surface (z and alpha 0, so TSR and FSR 3 still take their own camera
        // motion here), but it moves with the view's rotation. It used to write no motion:
        // TAAU fetched the sky's history from where the sky had been, and while the camera
        // turned a star or a moon's limb kept 4-11 % of its brightness (ADR-1151).
        return vec4<f32>(sky_motion(vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0)), 0.0, 0.0);
    }
    let ndc = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, depth, 1.0);
    let h = frame.inv_view_proj * ndc;
    let world = h.xyz / h.w;
    let current_uv = uv_of(frame.unjittered * vec4<f32>(world, 1.0));
    let prev_uv = uv_of(frame.prev_view_proj * vec4<f32>(world, 1.0));
    return vec4<f32>(prev_uv - current_uv, 0.0, 1.0);
}
