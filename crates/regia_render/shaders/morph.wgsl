// SPDX-License-Identifier: MPL-2.0
// Morph target application (Tincal engine item B1, specs/13 § 8, ADR-241).
//
// One invocation per moved vertex (a "row" of `morph_data`): base + Σ weight × delta over the
// targets that move it, in target order, zero weights skipped. A changed normal or tangent is
// renormalized (the tangent's handedness is not touched). The result goes into the instance's
// deformed span (`out_vertices`, LabVertex layout: 24 floats per vertex). Gathering per vertex
// means no two invocations write the same vertex, so no atomics are needed.
//
// The CPU reference is `MorphTargets::apply` (crates/regia_render/src/morph.rs): keep the
// two in step.

const ROW_WORDS: u32 = 16u;
const ENTRY_WORDS: u32 = 6u;
const VERTEX_FLOATS: u32 = 24u;
const PARAM_WORDS: u32 = 4u;
const WORKGROUP: u32 = 64u;

// Rows (vertex, entry begin, entry end, pad, base position xyz, normal xyz, tangent xyzw, pad),
// then entries (target, f16 position xyz, normal xyz, tangent xyz, pad).
@group(0) @binding(0) var<storage, read> morph_data: array<u32>;
// [row count, first entry word, first vertex of the span, target count], then one f32 weight
// per target.
@group(0) @binding(1) var<storage, read> instance: array<u32>;
@group(0) @binding(2) var<storage, read_write> out_vertices: array<f32>;

fn row_vec3(word: u32) -> vec3<f32> {
    return vec3<f32>(
        bitcast<f32>(morph_data[word]),
        bitcast<f32>(morph_data[word + 1u]),
        bitcast<f32>(morph_data[word + 2u]),
    );
}

// IEEE 754 binary16 bits → f32, exact (subnormals included). Integer decoding keeps the pass off
// the optional f16 capability and bit-identical to the CPU's `f16_bits_to_f32`.
fn half_to_f32(h: u32) -> f32 {
    let sign = (h & 0x8000u) << 16u;
    let exponent = (h >> 10u) & 0x1fu;
    let mantissa = h & 0x3ffu;
    if (exponent == 0u) {
        // mantissa × 2^-24 is exact in f32.
        return bitcast<f32>(sign | bitcast<u32>(f32(mantissa) * 5.9604644775390625e-8));
    }
    if (exponent == 31u) {
        return bitcast<f32>(sign | 0x7f800000u | (mantissa << 13u));
    }
    return bitcast<f32>(sign | ((exponent + 112u) << 23u) | (mantissa << 13u));
}

// The low (x) and high (y) halves of a word.
fn halves(word: u32) -> vec2<f32> {
    return vec2<f32>(half_to_f32(word & 0xffffu), half_to_f32(word >> 16u));
}

fn renormalized(v: vec3<f32>, base: vec3<f32>) -> vec3<f32> {
    if (all(v == base)) {
        return base;
    }
    let len2 = dot(v, v);
    if (len2 > 1e-12) {
        return v * (1.0 / sqrt(len2));
    }
    return base;
}

@compute @workgroup_size(64)
fn cs_morph(
    @builtin(global_invocation_id) gid: vec3<u32>,
    @builtin(num_workgroups) groups: vec3<u32>,
) {
    let row = gid.y * groups.x * WORKGROUP + gid.x;
    if (row >= instance[0]) {
        return;
    }
    let entry_base = instance[1];
    let span_first = instance[2];
    let r = row * ROW_WORDS;
    let vertex = morph_data[r];
    let begin = morph_data[r + 1u];
    let end = morph_data[r + 2u];
    let base_p = row_vec3(r + 4u);
    let base_n = row_vec3(r + 7u);
    let base_t = row_vec3(r + 10u);
    var p = base_p;
    var n = base_n;
    var t = base_t;
    for (var e = begin; e < end; e = e + 1u) {
        let w0 = entry_base + e * ENTRY_WORDS;
        let weight = bitcast<f32>(instance[PARAM_WORDS + morph_data[w0]]);
        if (weight == 0.0) {
            continue;
        }
        let a = halves(morph_data[w0 + 1u]);
        let b = halves(morph_data[w0 + 2u]);
        let c = halves(morph_data[w0 + 3u]);
        let d = halves(morph_data[w0 + 4u]);
        let f = halves(morph_data[w0 + 5u]);
        p = p + weight * vec3<f32>(a.x, a.y, b.x);
        n = n + weight * vec3<f32>(b.y, c.x, c.y);
        t = t + weight * vec3<f32>(d.x, d.y, f.x);
    }
    let nn = renormalized(n, base_n);
    let tt = renormalized(t, base_t);
    let o = (vertex - span_first) * VERTEX_FLOATS;
    out_vertices[o] = p.x;
    out_vertices[o + 1u] = p.y;
    out_vertices[o + 2u] = p.z;
    out_vertices[o + 3u] = nn.x;
    out_vertices[o + 4u] = nn.y;
    out_vertices[o + 5u] = nn.z;
    // Floats 6–7 are the UV; 8–11 the tangent (w = handedness, kept).
    out_vertices[o + 8u] = tt.x;
    out_vertices[o + 9u] = tt.y;
    out_vertices[o + 10u] = tt.z;
}
