// SPDX-License-Identifier: MPL-2.0
// The alpha-tested visibility pass (ADR-3915), appended to `vis_raster.wgsl` when composed: the
// masked clusters of each phase's lists (after the opaque ones, `vis_cull.wgsl`) draw with
// `vs_vis_masked` and this fragment stage, which samples the base color's alpha through the texture table and discards
// what the forward path's alpha test discards. One pipeline per cull mode serves every masked
// material: the record comes from the cluster's instance (material slot -> arena record), the
// texture from the table by index, so no draw binds a material. Only depth and the ID are
// written, so the Hi-Z pyramid built after phase 1 sees the holes.

// The material arena, whole (records `stride` words apart), and the frame's map from material
// slots to records (`visbuffer/single_pass.rs`, the single-pass resolve's group).
@group(1) @binding(0) var<storage, read> vis_material_words: array<u32>;
@group(1) @binding(1) var<storage, read> vis_slot_records: array<u32>;
// The texture table (`regia_render::texture_table`, group 4 as in the forward pipelines).
@group(4) @binding(0) var texture_table: binding_array<texture_2d<f32>, #{TEXTURE_TABLE_SIZE}>;
@group(4) @binding(1) var tex_samp: sampler;
@group(4) @binding(3) var tex_samp_clamp: sampler;
@group(4) @binding(4) var tex_samp_point: sampler;

// A slot with no record this frame (`NO_RECORD` in `single_pass.rs`).
const VIS_MASK_NO_RECORD: u32 = 0xffffffffu;
// Arena record words (`frame/gpu_types.rs` `GpuMaterial`): base color alpha, the alpha block
// (mode, cutoff, unlit, flags) and the detail block's `w` (the streaming minimum LOD).
const WORD_BASE_ALPHA: u32 = 3u;
const WORD_ALPHA_CUTOFF: u32 = 13u;
const WORD_ALPHA_FLAGS: u32 = 15u;
const WORD_MIN_LOD: u32 = 19u;

// The base color's table index and sampler choice of a record (`table_index`,
// `table_sampler_choice` in `lab_forward.wgsl`).
fn masked_base_index(base: u32) -> u32 {
    return vis_material_words[base + params.mask.x] & 0xffffu;
}

fn masked_sampler_choice(base: u32) -> u32 {
    return (vis_material_words[base + params.mask.x + 3u] >> 16u) & 3u;
}

// The forward color pass's base-color sample (`sample_slot_with`): the frame's mip bias, and
// with a streaming minimum LOD the gradients stretched to it. The gradients are taken once, in
// uniform control flow, and passed explicitly (a quad may span two materials).
fn masked_sample(index: u32, choice: u32, uv: vec2<f32>, dx: vec2<f32>, dy: vec2<f32>) -> vec4<f32> {
    if (choice == 1u) {
        return textureSampleGrad(texture_table[index], tex_samp_clamp, uv, dx, dy);
    }
    if (choice == 2u) {
        return textureSampleGrad(texture_table[index], tex_samp_point, uv, dx, dy);
    }
    return textureSampleGrad(texture_table[index], tex_samp, uv, dx, dy);
}

@fragment
fn fs_vis_masked(in: VsMasked) -> @location(0) u32 {
    let uv_dx = dpdx(in.uv);
    let uv_dy = dpdy(in.uv);
    let record = vis_slot_records[in.material];
    // A material without a record this frame cannot be tested: it draws nothing rather than a
    // solid card that would occlude what its holes show.
    let known = record != VIS_MASK_NO_RECORD;
    let base = select(0u, record, known) * params.info.z;
    let index = masked_base_index(base);
    // The frame's material bias (TSR plus the anisotropic-blend offset, ADR-5670).
    let bias = bitcast<f32>(params.mask.y);
    let k = exp2(bias);
    var dx = uv_dx * k;
    var dy = uv_dy * k;
    let min_lod = bitcast<f32>(vis_material_words[base + WORD_MIN_LOD]);
    if (min_lod > 0.0) {
        let size = vec2<f32>(textureDimensions(texture_table[index], 0));
        let lod = log2(max(max(length(dx * size), length(dy * size)), 1e-8));
        let stretch = exp2(max(min_lod - lod, 0.0));
        dx = dx * stretch;
        dy = dy * stretch;
    }
    let texel = masked_sample(index, masked_sampler_choice(base), in.uv, dx, dy);
    // Per-vertex ambient occlusion (`alpha.w` bit 1) reads the vertex alpha as occlusion, not
    // opacity, as the forward shader does.
    let flags = u32(bitcast<f32>(vis_material_words[base + WORD_ALPHA_FLAGS]) + 0.5);
    let vertex_alpha = select(in.color_a, 1.0, (flags & 2u) != 0u);
    let alpha = texel.a * bitcast<f32>(vis_material_words[base + WORD_BASE_ALPHA]) * vertex_alpha;
    if (!known || alpha < bitcast<f32>(vis_material_words[base + WORD_ALPHA_CUTOFF])) {
        discard;
    }
    return in.id;
}
