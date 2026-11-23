// SPDX-License-Identifier: MPL-2.0
// Compute skinning (Tincal B2, `crate::skin_compute`): skins one mesh instance's vertices
// once per frame into a vertex buffer that every pass then draws as is (shadow cascades, depth
// prepass, velocity, colour), instead of each pass skinning in its vertex shader.
//
// One invocation per vertex. The input is the mesh's `LabVertex` array (96 bytes, 24 words);
// the output is the same layout with the skinned position, normal and tangent and zero joint
// weights, so the forward and shadow shaders' own skinning (`wsum < 1e-5`) leaves it alone.
//
// Linear blend skinning repeats `regia::mesh::skin_position` / `skin_direction` exactly (no
// weight normalisation, directions by the linear part), over four influences or eight.
// Dual quaternion skinning follows Kavan, Collins, Zara, O'Sullivan, "Geometric Skinning with
// Approximate Dual Quaternion Blending" (ACM TOG 27(4), 2008): blend the joints' unit dual
// quaternions with their signs aligned to the heaviest influence (antipodality, § 4), normalise
// by the real part's length, and apply the resulting rigid transform. Joint scale or shear,
// when the palette has any, goes first as a linear blend of each joint's `R⁻¹ A` part
// (the paper's two-phase skinning for non-rigid joints, § 5.1).

struct Params {
    // Vertices of this instance.
    vertex_count: u32,
    // Bit 0: dual quaternion palette. Bit 1: eight influences from `influences`.
    // Bit 2: the dual quaternion palette carries scale / shear rows.
    flags: u32,
    // Joints in `palette`.
    joint_count: u32,
    pad: u32,
}

struct Influences8 {
    joints: array<u32, 8>,
    weights: array<f32, 8>,
}

const VERTEX_WORDS: u32 = 24u;
const FLAG_DQ: u32 = 1u;
const FLAG_EIGHT: u32 = 2u;
const FLAG_DQ_SCALE: u32 = 4u;

@group(0) @binding(0) var<uniform> params: Params;
// Source vertices (`LabVertex` words).
@group(0) @binding(1) var<storage, read> src: array<u32>;
// Eight influences per vertex (bit 1), else a one-element stand-in.
@group(0) @binding(2) var<storage, read> influences: array<Influences8>;
// Linear: three rows per joint (`Mat3x4`). Dual quaternion: real, dual, then three scale rows.
@group(0) @binding(3) var<storage, read> palette: array<vec4<f32>>;
// Skinned vertices (`LabVertex` words).
@group(0) @binding(4) var<storage, read_write> dst: array<u32>;

fn load3(base: u32) -> vec3<f32> {
    return vec3<f32>(bitcast<f32>(src[base]), bitcast<f32>(src[base + 1u]), bitcast<f32>(src[base + 2u]));
}

fn store3(base: u32, v: vec3<f32>) {
    dst[base] = bitcast<u32>(v.x);
    dst[base + 1u] = bitcast<u32>(v.y);
    dst[base + 2u] = bitcast<u32>(v.z);
}

struct Influence {
    joint: u32,
    weight: f32,
}

// Influence `k` (0..8) of vertex `v`: the eight-influence table, or the vertex's own four.
fn influence(v: u32, k: u32, eight: bool) -> Influence {
    var joint = 0u;
    var weight = 0.0;
    if eight {
        joint = influences[v].joints[k];
        weight = influences[v].weights[k];
    } else if k < 4u {
        let base = v * VERTEX_WORDS;
        joint = src[base + 16u + k];
        weight = bitcast<f32>(src[base + 20u + k]);
    }
    return Influence(min(joint, max(params.joint_count, 1u) - 1u), weight);
}

fn mat_point(j: u32, p: vec3<f32>) -> vec3<f32> {
    let x = palette[j * 3u];
    let y = palette[j * 3u + 1u];
    let z = palette[j * 3u + 2u];
    return vec3<f32>(dot(x.xyz, p) + x.w, dot(y.xyz, p) + y.w, dot(z.xyz, p) + z.w);
}

fn mat_dir(j: u32, d: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(dot(palette[j * 3u].xyz, d), dot(palette[j * 3u + 1u].xyz, d), dot(palette[j * 3u + 2u].xyz, d));
}

fn quat_rotate(q: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    return v + 2.0 * cross(q.xyz, cross(q.xyz, v) + q.w * v);
}

@compute @workgroup_size(64)
fn skin(@builtin(global_invocation_id) id: vec3<u32>) {
    let v = id.x;
    if v >= params.vertex_count {
        return;
    }
    let base = v * VERTEX_WORDS;
    // Copy every word (UV, colour, tangent sign), then overwrite what skinning changes.
    for (var w = 0u; w < VERTEX_WORDS; w = w + 1u) {
        dst[base + w] = src[base + w];
    }
    let position = load3(base);
    let normal = load3(base + 3u);
    let tangent = load3(base + 8u);
    let eight = (params.flags & FLAG_EIGHT) != 0u;
    let count = select(4u, 8u, eight);
    var wsum = 0.0;
    // The heaviest influence (the first of equals) is the dual quaternion sign pivot: an unused
    // slot (weight 0, joint 0) must not pick the hemisphere. Kept as two scalars, not a
    // function-local `Influence` var (ADR-026; the matrix's `skin_compute.wgsl []` layout scan).
    let first = influence(v, 0u, eight);
    var heaviest_joint = first.joint;
    var heaviest_weight = first.weight;
    for (var k = 0u; k < count; k = k + 1u) {
        let inf = influence(v, k, eight);
        wsum = wsum + inf.weight;
        if inf.weight > heaviest_weight {
            heaviest_joint = inf.joint;
            heaviest_weight = inf.weight;
        }
    }
    // Joint lanes zeroed: the drawing shaders treat the result as already skinned.
    for (var w = 16u; w < 24u; w = w + 1u) {
        dst[base + w] = 0u;
    }
    if wsum < 1e-5 {
        return;
    }
    var p = vec3<f32>(0.0);
    var n = vec3<f32>(0.0);
    var t = vec3<f32>(0.0);
    if (params.flags & FLAG_DQ) == 0u {
        for (var k = 0u; k < count; k = k + 1u) {
            let inf = influence(v, k, eight);
            p = p + inf.weight * mat_point(inf.joint, position);
            n = n + inf.weight * mat_dir(inf.joint, normal);
            t = t + inf.weight * mat_dir(inf.joint, tangent);
        }
    } else {
        let pivot = palette[heaviest_joint * 5u];
        var real = vec4<f32>(0.0);
        var dual = vec4<f32>(0.0);
        var s0 = vec3<f32>(0.0);
        var s1 = vec3<f32>(0.0);
        var s2 = vec3<f32>(0.0);
        for (var k = 0u; k < count; k = k + 1u) {
            let inf = influence(v, k, eight);
            let j = inf.joint * 5u;
            // Shortest path: a quaternion and its negation are the same rotation.
            let flip = select(1.0, -1.0, dot(palette[j], pivot) < 0.0);
            real = real + (inf.weight * flip) * palette[j];
            dual = dual + (inf.weight * flip) * palette[j + 1u];
            s0 = s0 + inf.weight * palette[j + 2u].xyz;
            s1 = s1 + inf.weight * palette[j + 3u].xyz;
            s2 = s2 + inf.weight * palette[j + 4u].xyz;
        }
        let len = max(length(real), 1e-12);
        real = real / len;
        dual = dual / len;
        var sp = position;
        var sn = normal;
        var st = tangent;
        if (params.flags & FLAG_DQ_SCALE) != 0u {
            let inv = 1.0 / wsum;
            let r0 = s0 * inv;
            let r1 = s1 * inv;
            let r2 = s2 * inv;
            sp = vec3<f32>(dot(r0, position), dot(r1, position), dot(r2, position));
            sn = vec3<f32>(dot(r0, normal), dot(r1, normal), dot(r2, normal));
            st = vec3<f32>(dot(r0, tangent), dot(r1, tangent), dot(r2, tangent));
        }
        // Translation of a unit dual quaternion: 2 · (dual ⊗ conj(real)).xyz.
        let translation = 2.0 * (real.w * dual.xyz - dual.w * real.xyz + cross(real.xyz, dual.xyz));
        p = quat_rotate(real, sp) + translation;
        n = quat_rotate(real, sn);
        t = quat_rotate(real, st);
    }
    store3(base, p);
    store3(base + 3u, n);
    store3(base + 8u, t);
}
