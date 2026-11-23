// SPDX-License-Identifier: MPL-2.0
// Depth-only pass for cascaded sun maps and the local-light shadow atlas.
// Vertices match `LabVertex`. Skinned the same way as the forward pass.
// `ALPHA_TEST` (`FrameRenderer::set_alpha_tested_shadows`): masked casters discard the texels
// their base-colour alpha cuts away, with the material's hard cutoff, so a foliage card casts its
// leaves and not its rectangle.

// The texture table variant (`ALPHA_TEST` + `TEXTURE_TABLE`, ADR-863) declares a `binding_array`,
// which naga accepts only with this extension enabled; naga_oil reads `enable` directives only at
// the top of a file (before any `#ifdef`), so it is on for every variant. The engine hands wgpu
// naga IR, which carries no directive.
enable wgpu_binding_array;

#import regia::mesh
#ifdef MATERIAL_GRAPH_VERTEX
#import regia::pbr::surface
#endif

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
#ifdef ALPHA_TEST
    @location(0) uv: vec2<f32>,
    @location(1) alpha: f32,
#endif
}

struct ShadowVp {
    light_view_proj: mat4x4<f32>,
    // Time, previous time, wind x, wind z (ADR-5160).
    clock: vec4<f32>,
    // x > 0.5: graphs read `clock` instead of material slot 0.
    clock_live: vec4<f32>,
}

struct ObjectData {
    model: mat4x4<f32>,
    skin: vec4<u32>,
}

@group(0) @binding(0) var<uniform> shadow_vp: ShadowVp;

#ifdef GPU_DRIVEN
@group(1) @binding(0) var<storage, read> objects: array<ObjectData>;
fn current_object(slot: u32) -> ObjectData {
    return objects[slot];
}
#else
@group(1) @binding(0) var<uniform> object: ObjectData;
fn current_object(slot: u32) -> ObjectData {
    let _unused = slot;
    return object;
}
#endif
@group(1) @binding(1) var<storage, read> skin_joints: array<regia::mesh::Mat3x4>;

#ifdef MATERIAL_GRAPH_VERTEX
// A material graph with a world position offset casts the displaced shadow: the material group
// of the forward pass (its standard block is opaque here; only the graph parameters are read).
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
    if (shadow_vp.clock_live.x > 0.5) {
        return shadow_vp.clock;
    }
    return material.graph[0];
}
// @material_graph_vertex_source
#endif

#ifdef ALPHA_TEST
// The standard block of `LabMaterial` (`lab_forward.wgsl`), as `translucent_shadow.wgsl` declares
// it: 22 vec4s, then the table indices on the texture table path.
struct MaskMaterial {
    base_color: vec4<f32>,
    factors: vec4<f32>,
    emissive: vec4<f32>,
    // x = alpha mode (1 mask), y = cutoff
    alpha: vec4<f32>,
    rest: array<vec4<f32>, 18>,
#ifdef TEXTURE_TABLE
    texture_indices: vec4<u32>,
#endif
}
#ifdef MATERIAL_SSBO
@group(2) @binding(0) var<storage, read> material: MaskMaterial;
#else
@group(2) @binding(0) var<uniform> material: MaskMaterial;
#endif
#ifdef TEXTURE_TABLE
// The device's material texture table (group 4, ADR-863; `lab_forward.wgsl` declares the same).
@group(4) @binding(0) var texture_table: binding_array<texture_2d<f32>, #{TEXTURE_TABLE_SIZE}>;
@group(4) @binding(1) var tex_samp: sampler;
fn table_index(slot: u32) -> u32 {
    let word = material.texture_indices[slot >> 1u];
    return (word >> ((slot & 1u) * 16u)) & 0xffffu;
}
// The shared sampler set (ADR-2071, `lab_forward.wgsl` declares the same).
@group(4) @binding(3) var tex_samp_clamp: sampler;
@group(4) @binding(4) var tex_samp_point: sampler;
fn table_sample(slot: u32, uv: vec2<f32>) -> vec4<f32> {
    let choice = (material.texture_indices.w >> (16u + slot * 2u)) & 3u;
    if (choice == 1u) {
        return textureSample(texture_table[table_index(slot)], tex_samp_clamp, uv);
    }
    if (choice == 2u) {
        return textureSample(texture_table[table_index(slot)], tex_samp_point, uv);
    }
    return textureSample(texture_table[table_index(slot)], tex_samp, uv);
}
#else
@group(2) @binding(1) var albedo_tex: texture_2d<f32>;
@group(2) @binding(2) var tex_samp: sampler;
#endif
#endif

#ifdef GPU_DRIVEN
@vertex
fn vs_main(in: VertexInput, @builtin(instance_index) instance: u32) -> VertexOutput {
    let object = current_object(instance);
#else
@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    let object = current_object(0u);
#endif
    let wsum = in.weights.x + in.weights.y + in.weights.z + in.weights.w;
    var local = in.position;
    if (wsum > 1e-5) {
        let base = object.skin.x;
        let j0 = skin_joints[base + in.joints.x];
        let j1 = skin_joints[base + in.joints.y];
        let j2 = skin_joints[base + in.joints.z];
        let j3 = skin_joints[base + in.joints.w];
        local = regia::mesh::skin_position(in.position, j0, j1, j2, j3, in.weights);
    }
#ifdef MATERIAL_GRAPH_VERTEX
    var local_n = in.normal;
    if (wsum > 1e-5) {
        let base = object.skin.x;
        local_n = regia::mesh::skin_direction(
            in.normal,
            skin_joints[base + in.joints.x],
            skin_joints[base + in.joints.y],
            skin_joints[base + in.joints.z],
            skin_joints[base + in.joints.w],
            in.weights,
        );
    }
#ifdef MATERIAL_GRAPH_UV1
    let uv1 = in.uv1;
#else
    let uv1 = in.uv;
#endif
    let rest = (object.model * vec4<f32>(local, 1.0)).xyz;
    let nrm = (object.model * vec4<f32>(local_n, 0.0)).xyz;
    let world = vec4<f32>(rest + material_vertex_offset(regia::pbr::surface::VertexOffsetInput(
        rest,
        nrm * inverseSqrt(max(dot(nrm, nrm), 1e-12)),
        in.uv,
        uv1,
        in.color,
        object.model[3].xyz,
        graph_clock().x,
    )), 1.0);
#else
    let world = object.model * vec4<f32>(local, 1.0);
#endif
    var clip = shadow_vp.light_view_proj * world;
    let _keep = in.normal.x + in.uv.x + in.tangent.x + in.color.x;
    clip.z = clip.z + _keep * 0.0;
#ifdef ALPHA_TEST
    return VertexOutput(clip, in.uv, in.color.a);
#else
    return VertexOutput(clip);
#endif
}

#ifdef ALPHA_TEST
// Base-colour alpha × material alpha × vertex alpha against the material's cutoff: the forward
// pass's plain test (`alpha_rejects` off TSR), never hashed, so the shadow holds still.
#ifdef SOFT_ALPHA
// Interleaved gradient noise (Jimenez, "Next Generation Post Processing in Call of Duty:
// Advanced Warfare", SIGGRAPH 2014) of the shadow-map texel: neighbouring texels hold well
// spread values, so the shadow filter averages them into a smooth penumbra instead of a blotchy
// one. A fixed pattern, the same every frame and every run.
fn texel_hash(p: vec2<f32>) -> f32 {
    return fract(52.9829189 * fract(dot(floor(p), vec2<f32>(0.06711056, 0.00583715))));
}
#endif

@fragment
fn fs_main(in: VertexOutput) {
    #ifdef TEXTURE_TABLE
    let alb = table_sample(0u, in.uv);
    #else
    let alb = textureSample(albedo_tex, tex_samp, in.uv);
    #endif
    let alpha = alb.a * material.base_color.a * in.alpha;
#ifdef SOFT_ALPHA
    // Soft foliage shadow: the coverage rises from 0 to 1 over a band around the cutoff and is
    // compared with a per-texel hash, so a leaf edge writes a fraction of the texels and the
    // shadow filter averages them into a penumbra (Wyman and McGuire, "Hashed Alpha Testing",
    // I3D 2017, with the soft band in place of the alpha itself).
    if (u32(material.alpha.x + 0.5) == 1u) {
        let band = max(f32(#{FOLIAGE_SOFT_Q}) * 0.01, 0.002);
        let coverage = smoothstep(material.alpha.y - band, material.alpha.y + band, alpha);
        if (coverage <= texel_hash(in.clip_position.xy)) {
            discard;
        }
    }
#else
    if (u32(material.alpha.x + 0.5) == 1u && alpha < material.alpha.y) {
        discard;
    }
#endif
}
#else
@fragment
fn fs_main() {}
#endif
