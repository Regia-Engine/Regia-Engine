// SPDX-License-Identifier: MPL-2.0
// Translucent shadow casters: the colored shadow map (McGuire & Enderton 2011, § 2's
// non-stochastic baseline; see docs/provenance.md). Alpha-blended and transmissive casters
// draw into a second map per sun cascade / local-light tile, separate from the opaque depth:
//   1. `fs_depth`: the nearest translucent depth (depth attachment, `Less`, no colour write);
//   2. `fs_color`: every translucent layer multiplies its RGB transmittance into the colour
//      target (blend dst × src, cleared to white), depth test off.
// The receiver (`lab_forward.wgsl` `sample_csm_transmittance`) multiplies its light by the
// colour where it lies behind the translucent depth. Opaque shadowing is untouched.
// Vertices match `LabVertex`; skinned the same way as the forward pass.

// The texture table variant (`TEXTURE_TABLE`, ADR-863) declares a `binding_array`, which naga
// accepts only with this extension enabled; naga_oil reads `enable` directives only at the top of
// a file (before any `#ifdef`), so it is on for every variant. The engine hands wgpu naga IR, which
// carries no directive.
enable wgpu_binding_array;

#import regia::mesh

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) tangent: vec4<f32>,
    @location(4) color: vec4<f32>,
    @location(5) joints: vec4<u32>,
    @location(6) weights: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) world: vec3<f32>,
}

struct ShadowVp {
    light_view_proj: mat4x4<f32>,
}

struct ObjectData {
    model: mat4x4<f32>,
    skin: vec4<u32>,
}

// The standard block of `LabMaterial` (`lab_forward.wgsl`). A graph material's parameter block
// follows it in the same buffer and is not read here.
struct CasterMaterial {
    base_color: vec4<f32>,
    factors: vec4<f32>,
    emissive: vec4<f32>,
    // x = alpha mode (2 blend, 3 premultiplied, 4 additive), y = cutoff
    alpha: vec4<f32>,
    detail: vec4<f32>,
    detail_fade: vec4<f32>,
    coat: vec4<f32>,
    response: vec4<f32>,
    wear: vec4<f32>,
    wear_tint: vec4<f32>,
    // z = transmission, w = dielectric F0 from the IOR (0 = default 0.04)
    pom: vec4<f32>,
    // rgb = volume transmittance at normal incidence
    transmit: vec4<f32>,
    // Shading model block (`shading_model.rs`; unused here): five vec4s before the hair block.
    shading: vec4<f32>,
    shading_a: vec4<f32>,
    shading_b: vec4<f32>,
    shading_c: vec4<f32>,
    sheen: vec4<f32>,
    // Hair cards (`hair.rs`): hair_b.w is the strategy (0 = not hair), hair_c.w the tip alpha,
    // hair_d = ramp start, ramp end, root alpha, shift jitter.
    hair_a: vec4<f32>,
    hair_b: vec4<f32>,
    hair_c: vec4<f32>,
    hair_d: vec4<f32>,
    hair_e: vec4<f32>,
#ifdef TEXTURE_TABLE
    // Texture table indices (`regia_render::texture_table`, ADR-863); slot 0 (base colour) is
    // the low half of the first word.
    texture_indices: vec4<u32>,
#endif
}

@group(0) @binding(0) var<uniform> shadow_vp: ShadowVp;
@group(1) @binding(0) var<uniform> object: ObjectData;
@group(1) @binding(1) var<storage, read> skin_joints: array<regia::mesh::Mat3x4>;
// `MATERIAL_SSBO`: the material record in storage (`regia_render::bindless`, ADR-316).
#ifdef MATERIAL_SSBO
@group(2) @binding(0) var<storage, read> material: CasterMaterial;
#else
@group(2) @binding(0) var<uniform> material: CasterMaterial;
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

#ifdef GLASS
// Per-instance glass (`frame/glass_gpu.rs`): the same parameters and crack page as the forward
// pass's `GLASS` variant. Holes cast nothing; cracks and crazing scatter light out of the beam.
struct GlassParams {
    mask_from_world: mat4x4<f32>,
    axis_u: vec4<f32>,
    axis_v: vec4<f32>,
    scatter: vec4<f32>,
    shade: vec4<f32>,
    holes: vec4<f32>,
    inner: vec4<f32>,
    view: vec4<f32>,
}
@group(3) @binding(0) var<uniform> glass: GlassParams;
@group(3) @binding(1) var glass_mask: texture_2d<f32>;
@group(3) @binding(2) var glass_samp: sampler;

fn glass_hash(p: vec2<f32>) -> f32 {
    let q = fract(p * vec2<f32>(0.1031, 0.1030));
    let r = q + dot(q, q.yx + vec2<f32>(33.33));
    return fract((r.x + r.y) * r.x);
}

// rgb = what the damage passes (`lab_forward.wgsl` `glass_transmittance` without the view-
// dependent ghost), a = 1 in a hole.
fn glass_damage(world: vec3<f32>, uv0: vec2<f32>) -> vec4<f32> {
    let mapped = (glass.mask_from_world * vec4<f32>(world, 1.0)).xy;
    let uv = select(mapped, uv0, glass.axis_u.w > 0.5);
    // Implicit derivatives in light space pick the page mip for the shadow texel.
    let m = textureSample(glass_mask, glass_samp, uv);
    if glass.inner.w < 0.5 || any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0)) {
        return vec4<f32>(1.0, 1.0, 1.0, 0.0);
    }
    let page = glass.view.w;
    let jitter = (glass_hash(floor(uv * page * 0.5)) - 0.5) * glass.holes.y;
    let hole = select(0.0, 1.0, m.b > glass.holes.x + jitter);
    let crack = 1.0 - m.r * (1.0 - glass.scatter.a);
    let haze = 1.0 - m.g * glass.shade.y;
    var inner = vec3<f32>(1.0);
    if glass.holes.z > 0.5 {
        inner = glass.inner.rgb;
    }
    return vec4<f32>(vec3<f32>(crack * haze) * inner, hole);
}
#endif

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
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
    let world = object.model * vec4<f32>(local, 1.0);
    var clip = shadow_vp.light_view_proj * world;
    let _keep = in.normal.x + in.tangent.x;
    clip.z = clip.z + _keep * 0.0;
    return VertexOutput(clip, in.uv, in.color, world.xyz);
}

// Light that crosses one translucent layer (RGB). Coverage first (glTF): `1 − α` passes
// untouched; the covered part transmits `transmission · (1 − F0) · base · volume` (the forward
// pass's `fs_transmit` at normal incidence). A plain blended surface (transmission 0) passes
// `1 − α`; additive surfaces add light and never reach this pass.
fn layer_transmittance(in: VertexOutput) -> vec4<f32> {
    #ifdef TEXTURE_TABLE
    let alb = table_sample(0u, in.uv);
    #else
    let alb = textureSample(albedo_tex, tex_samp, in.uv);
    #endif
#ifdef GLASS
    let damage = glass_damage(in.world, in.uv);
#else
    let damage = vec4<f32>(1.0, 1.0, 1.0, 0.0);
#endif
    var alpha = clamp(alb.a * material.base_color.a * in.color.a, 0.0, 1.0);
    if (material.hair_b.w > 0.5) {
        // A hair card casts through its alpha test: the root-to-tip alpha ramp, then the
        // material's cutoff (the forward pass's hashed test is the same threshold on average).
        // A sorted (blended) card keeps its soft coverage.
        let span = material.hair_d.y - material.hair_d.x;
        var ramp_t = 0.0;
        if (abs(span) > 1e-6) {
            ramp_t = clamp((in.uv.y - material.hair_d.x) / span, 0.0, 1.0);
        }
        alpha = alpha * mix(material.hair_d.z, material.hair_c.w, ramp_t);
        if (material.alpha.x < 1.5) {
            alpha = select(0.0, 1.0, alpha >= material.alpha.y);
        }
    }
    let base = clamp(alb.rgb * material.base_color.rgb * in.color.rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    let t = clamp(material.pom.z, 0.0, 1.0);
    let f0 = select(0.04, material.pom.w, material.pom.w > 0.0);
    let volume = clamp(material.transmit.rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    // A transmissive surface lets light through whatever its alpha says (the forward pass
    // blends it by transmission, not coverage).
    let coverage = select(alpha, 1.0, t > 0.0);
    let layer = vec3<f32>(1.0 - coverage) + coverage * t * (1.0 - f0) * base * volume;
    // a = 1 where a damaged pane has a hole (nothing there to cast).
    return vec4<f32>(layer * damage.rgb, damage.a);
}

// Pass 1: the nearest translucent depth. A fully clear texel (nothing absorbed) is not a
// caster, so what lies in front of real glass is not tinted by the clear part of its texture.
@fragment
fn fs_depth(in: VertexOutput) {
    let tr = layer_transmittance(in);
    if (tr.a > 0.5 || min(tr.r, min(tr.g, tr.b)) >= 0.996) {
        discard;
    }
}

// Pass 2: the product of every layer's transmittance (blend: dst × src).
@fragment
fn fs_color(in: VertexOutput) -> @location(0) vec4<f32> {
    let tr = layer_transmittance(in);
    if (tr.a > 0.5) {
        discard;
    }
    return vec4<f32>(tr.rgb, 1.0);
}
