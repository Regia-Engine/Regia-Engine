// SPDX-License-Identifier: MPL-2.0
// Compute software rasterizer for sub-pixel clusters (`docs/specs/98-visibility-buffer.md` § 8,
// ADR-1887). Written from the edge-function formulation of Pineda, "A Parallel Algorithm for
// Polygon Rasterization", SIGGRAPH 1988; Laine & Karras, "High-Performance Software Rasterization
// on GPUs", HPG 2011, give the case for it on tiny triangles.
//
// wgpu 30 has no 64-bit atomics on every adapter, so depth and ID cannot be one atomic word.
// Each pixel has two 32-bit atomics in storage buffers and every cluster is rasterized twice
// through this one entry point, `mode.x` picking the write:
//
// 1. Depth pass: `atomicMax` of the inverted bits of the float depth. Inverted, so that a cleared
//    word (0) means "no coverage", and a nearer (smaller) depth is a larger word.
// 2. ID pass: the same computation (same compiled code, so the same bits); where a triangle's
//    depth equals the stored word, `atomicMax` of its visibility ID.
//
// `vis_software_merge.wgsl` then writes the survivors into the ID target and the scene depth with
// the hardware pass's depth test, so everything downstream reads one visibility buffer.
//
// IDs use the hardware format `phase << 31 | slot << 7 | triangle`. A software cluster is phase 1
// with slot `capacity + k`, and the resolve's `phase × capacity + slot` lands on list entry
// `2 × capacity + k`: the third range of the cluster buffer (`vis_cull.wgsl` `emit`). The resolve
// and the velocity pass need no change.
//
// A double-sided instance's clusters (`Instance.flags.y` bit 0) keep their back faces whatever the
// round's cull mode says, as the hardware's cull-none raster draws them (ADR-2689); the winding
// swap below gives a back face the same coverage and fill rule as its front.
//
// Coverage follows the hardware rules: positions snapped to 1/256 pixel, samples at pixel
// centres, the top-left fill rule. A selected cluster is in front of the near plane and spans at
// most 64 pixels (`takes_software` in `vis_cull.wgsl`), so nothing is clipped and the edge
// products stay inside an `i32`.

struct SoftwareParams {
    view_proj: mat4x4<f32>,
    // x = width, y = height, z = list index of the first software cluster (2 × capacity),
    // w = first software slot of an ID (capacity).
    info: vec4<u32>,
    // x = 0 depth pass, 1 ID pass; y = 1 to cull the back faces of single-sided instances.
    mode: vec4<u32>,
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

// `state` words (`vis_cull.wgsl`): the range of the list this round rasterizes.
const SOFT_BASE: u32 = 30u;
const SOFT_END: u32 = 31u;
const CLUSTERS_PER_GROUP: u32 = 16u;
// `Instance.flags.y`: back faces drawn (`vis_cull.wgsl`).
const DOUBLE_SIDED: u32 = 1u;

@group(0) @binding(0) var<uniform> params: SoftwareParams;
@group(0) @binding(1) var<storage, read> instances: array<Instance>;
@group(0) @binding(2) var<storage, read> meshlets: array<Meshlet>;
@group(0) @binding(3) var<storage, read> geo: array<u32>;
@group(0) @binding(4) var<storage, read> clusters: array<vec2<u32>>;
@group(0) @binding(5) var<storage, read> state: array<u32>;
@group(0) @binding(6) var<storage, read_write> depth_words: array<atomic<u32>>;
@group(0) @binding(7) var<storage, read_write> id_words: array<atomic<u32>>;
// The swaying materials of this frame (ADR-3916).
@group(0) @binding(8) var<uniform> sway_table: regia::foliage_sway::SwayTable;

// `world` with its instance's sway: the hardware raster's `swayed` (`vis_raster.wgsl`), term for
// term, so a vertex two clusters share lands on the same 1/256 pixel in either rasterizer and no
// crack opens between them.
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

// Screen position in 1/256 pixels (`xy`, rounded) and depth (`z`). The transform order is the
// hardware vertex shader's, so the depth matches it.
fn screen_vertex(m: Meshlet, inst: Instance, local: u32) -> vec3<f32> {
    let v = geo[m.vertex_offset + local];
    let position = vec3<f32>(bitcast<f32>(geo[v]), bitcast<f32>(geo[v + 1u]), bitcast<f32>(geo[v + 2u]));
    let world = swayed(inst, m, v, (inst.model * vec4<f32>(position, 1.0)).xyz);
    let clip = params.view_proj * vec4<f32>(world, 1.0);
    let ndc = clip.xyz / clip.w;
    let size = vec2<f32>(f32(params.info.x), f32(params.info.y));
    let pixels = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5) * size;
    return vec3<f32>(round(pixels * 256.0), ndc.z);
}

// Twice the signed area of (a, b, p), in 1/256 pixels squared. Positive when p is on the inner
// side of a → b for a triangle that winds positively in the y-down pixel grid.
fn edge(a: vec2<i32>, b: vec2<i32>, p: vec2<i32>) -> i32 {
    return (b.x - a.x) * (p.y - a.y) - (b.y - a.y) * (p.x - a.x);
}

// Top-left rule in the y-down grid: a left edge (going up) or a top edge (going right) owns the
// samples exactly on it; the others give them up.
fn edge_bias(a: vec2<i32>, b: vec2<i32>) -> i32 {
    let d = b - a;
    if (d.y < 0 || (d.y == 0 && d.x > 0)) {
        return 0;
    }
    return -1;
}

fn rasterize_triangle(va: vec3<f32>, vb: vec3<f32>, vc: vec3<f32>, id: u32, cull_back: bool) {
    let a = vec2<i32>(va.xy);
    let b = vec2<i32>(vb.xy);
    let c = vec2<i32>(vc.xy);
    let signed_area = edge(a, b, c);
    if (signed_area == 0) {
        return;
    }
    // Counter-clockwise in NDC (front) is negative in the y-down grid.
    let front = signed_area < 0;
    if (!front && cull_back) {
        return;
    }
    // Wind positively: swap the last two when front-facing.
    let p1 = select(b, c, front);
    let p2 = select(c, b, front);
    let z1 = select(vb.z, vc.z, front);
    let z2 = select(vc.z, vb.z, front);
    let area = f32(abs(signed_area));
    let last = vec2<i32>(i32(params.info.x) - 1, i32(params.info.y) - 1);
    let low = min(a, min(p1, p2));
    let high = max(a, max(p1, p2));
    // Pixel `x` samples at 256 x + 128.
    let first_px = max((low - vec2<i32>(128)) >> vec2<u32>(8u), vec2<i32>(0));
    let last_px = min((high - vec2<i32>(128) + vec2<i32>(255)) >> vec2<u32>(8u), last);
    let bias0 = edge_bias(p1, p2);
    let bias1 = edge_bias(p2, a);
    let bias2 = edge_bias(a, p1);
    for (var y = first_px.y; y <= last_px.y; y = y + 1) {
        for (var x = first_px.x; x <= last_px.x; x = x + 1) {
            let sample = vec2<i32>(x, y) * 256 + vec2<i32>(128);
            let w0 = edge(p1, p2, sample);
            let w1 = edge(p2, a, sample);
            let w2 = edge(a, p1, sample);
            if (w0 + bias0 < 0 || w1 + bias1 < 0 || w2 + bias2 < 0) {
                continue;
            }
            let z = (f32(w0) * va.z + f32(w1) * z1 + f32(w2) * z2) / area;
            if (!(z > 0.0 && z <= 1.0)) {
                continue;
            }
            let word = ~bitcast<u32>(z);
            let pixel = u32(y) * params.info.x + u32(x);
            if (params.mode.x == 0u) {
                atomicMax(&depth_words[pixel], word);
            } else if (atomicLoad(&depth_words[pixel]) == word) {
                atomicMax(&id_words[pixel], id);
            }
        }
    }
}

// `dispatch_workgroups_indirect` caps each dimension at 65535.
@compute @workgroup_size(16)
fn rasterize_clusters(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {
    let index = state[SOFT_BASE] + gid.x + gid.y * nwg.x * CLUSTERS_PER_GROUP;
    if (index >= state[SOFT_END]) {
        return;
    }
    let entry = clusters[params.info.z + index];
    let m = meshlets[entry.y];
    let inst = instances[entry.x];
    let cull_back = params.mode.y == 1u && (inst.flags.y & DOUBLE_SIDED) == 0u;
    let id_base = (1u << 31u) | ((params.info.w + index) << 7u);
    for (var tri = 0u; tri < m.triangle_count; tri = tri + 1u) {
        let packed = geo[m.triangle_offset + tri];
        rasterize_triangle(
            screen_vertex(m, inst, packed & 0xffu),
            screen_vertex(m, inst, (packed >> 8u) & 0xffu),
            screen_vertex(m, inst, (packed >> 16u) & 0xffu),
            id_base | tri,
            cull_back,
        );
    }
}
