// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::pbr::surface

// Material surface contract (`specs/13` § 7). Custom materials override `material_surface`.

struct SurfaceInput {
    world_position: vec3<f32>,
    // Geometric normal, normalized and flipped toward the viewer on a back face.
    world_normal: vec3<f32>,
    uv: vec2<f32>,
    // Unit vector from the surface toward the eye.
    view_dir: vec3<f32>,
    vertex_color: vec4<f32>,
    // Interpolated tangent (xyz, not normalized) and bitangent sign (w). Zero when the mesh has none.
    // Negated with the normal on a back face, the sign w too (ADR-4950), so a frame built as
    // `(t, cross(world_normal, t) * w, world_normal)` is the front frame negated.
    world_tangent: vec4<f32>,
    // Distance from the eye to the surface, metres.
    camera_distance: f32,
    // Second UV set (the first set when the mesh has none).
    second_uv: vec2<f32>,
    // World-space origin of the object (its model matrix translation).
    object_position: vec3<f32>,
    // 1 on a front face, -1 on a back face. A tangent-space normal multiplies the tangent by
    // this so a double-sided back face is the front frame negated.
    facing: f32,
}

// Inputs of a material graph's vertex stage (`material_vertex_offset`, world position offset).
// The same values in the forward, depth, shadow and velocity passes, so a displaced surface
// shadows and moves where it is drawn. No eye-dependent inputs: a shadow pass has another eye.
struct VertexOffsetInput {
    // World position before the offset.
    world_position: vec3<f32>,
    // Unit world normal (before the offset; the offset does not bend normals).
    world_normal: vec3<f32>,
    uv: vec2<f32>,
    // Second UV set.
    second_uv: vec2<f32>,
    vertex_color: vec4<f32>,
    object_position: vec3<f32>,
    // Seconds (the material's time slot; the velocity pass passes the previous frame's).
    time: f32,
}

struct Surface {
    base_color: vec3<f32>,
    alpha: f32,
    metallic: f32,
    roughness: f32,
    normal: vec3<f32>,
    occlusion: f32,
    emissive: vec3<f32>,
    unlit: f32,
}

fn default_surface() -> Surface {
    // Constructor, not `var s: Surface`. naga 30 emits Offset on every struct
    // member and then an OpVariable in the Function class; SPIR-V 1.6 rejects
    // that (VUID-StandaloneSpirv-None-10684). A composite construct does not.
    return Surface(
        vec3<f32>(0.8),
        1.0,
        0.0,
        0.5,
        vec3<f32>(0.0, 1.0, 0.0),
        1.0,
        vec3<f32>(0.0),
        0.0,
    );
}
