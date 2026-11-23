// SPDX-License-Identifier: MPL-2.0

// ---- Visibility-buffer material resolve (`specs/27` § 4.2 stage 3) ----
// Appended to `lab_forward.wgsl` composed with `VISBUFFER`, so `shade_surface` is the forward
// fragment body itself. One pass: `fs_vis_material_depth` writes each pixel's material slot as
// depth (the material-ID depth trick of deferred texturing, N. Reed, "Deferred Texturing",
// 2014), then one full-screen triangle per material at that depth with an equal test shades
// only its own pixels. The pixel's triangle is re-fetched and its attributes interpolated with
// perspective-correct barycentrics from 2D homogeneous edge functions (Olano & Greer, "Triangle
// Scan Conversion using 2D Homogeneous Coordinates", HWWS 1997), which stay valid when a vertex
// is behind the eye; UV gradients come from the same functions one pixel over (Schied &
// Dachsbacher, "Deferred Attribute Interpolation for Memory-Efficient Deferred Shading", HPG 2015),
// taken across the pixel's 2×2 quad the way the forward pass's quads see them (see
// `fs_vis_resolve`). Under `VIS_SINGLE_PASS` (ADR-2071) one full-screen draw shades every material:
// no material depth, and each pixel picks its record and its table indices itself.

struct VisInstance {
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

struct VisMeshlet {
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

struct VisResolveParams {
    // The visibility pass's (jittered) view-projection.
    view_proj: mat4x4<f32>,
    // x = cluster capacity per phase.
    info: vec4<u32>,
    // xy = visibility target size in pixels.
    screen: vec4<f32>,
}

@group(1) @binding(8) var<uniform> vis_params: VisResolveParams;
@group(1) @binding(9) var<storage, read> vis_instances: array<VisInstance>;
@group(1) @binding(10) var<storage, read> vis_meshlets: array<VisMeshlet>;
@group(1) @binding(11) var<storage, read> vis_geo: array<u32>;
@group(1) @binding(12) var<storage, read> vis_clusters: array<vec2<u32>>;
@group(1) @binding(13) var vis_ids: texture_2d<u32>;
// The swaying materials of this frame (ADR-3916).
@group(1) @binding(14) var<uniform> vis_sway: regia::foliage_sway::SwayTable;

const VIS_EMPTY: u32 = 0xffffffffu;

#ifdef LIGHTMAP
// Per visibility instance (ADR-2107, `frame/lightmap_paths.rs`): the entry's UV2 → atlas transform,
// zero for an instance that is not a receiver (it keeps the probe light), and where its mesh's UV2
// words are in `vis_geo` (two per source vertex, a range of their own beside the mesh's).
// `uv2_count` is the mesh's own vertices. A voxel-far cluster indexes vertices appended past it.
struct VisLightmap {
    scale_offset: vec4<f32>,
    uv2_base: u32,
    vertex_base: u32,
    uv2_count: u32,
    pad1: u32,
}

@group(3) @binding(17) var<storage, read> vis_lightmap: array<VisLightmap>;

// Source vertex index of a triangle corner. A cluster's vertex list holds each vertex's first
// word, sixteen words apart (`VERTEX_WORDS`).
fn vis_vertex_index(lm: VisLightmap, m: VisMeshlet, packed: u32, corner: u32) -> u32 {
    let local = (packed >> (corner * 8u)) & 0xffu;
    return (vis_geo[m.vertex_offset + local] - lm.vertex_base) / 16u;
}

// UV2 of one charted source vertex. `index` must be below `lm.uv2_count`.
fn vis_chart_uv(lm: VisLightmap, index: u32) -> vec2<f32> {
    let at = lm.uv2_base + index * 2u;
    return vec2<f32>(vis_word_f(at), vis_word_f(at + 1u));
}
#endif

fn vis_material_depth(slot: u32) -> f32 {
    return f32(slot + 1u) / 65535.0;
}

fn vis_cluster(id: u32) -> vec2<u32> {
    return vis_clusters[(id >> 31u) * vis_params.info.x + ((id >> 7u) & 0xffffffu)];
}

@vertex
fn vs_vis_fullscreen(@builtin(vertex_index) vi: u32, @builtin(instance_index) slot: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    return vec4<f32>(uv * 2.0 - vec2<f32>(1.0), vis_material_depth(slot), 1.0);
}

struct VisDepthOut {
    @location(0) color: vec4<f32>,
    @builtin(frag_depth) depth: f32,
}

@fragment
fn fs_vis_material_depth(@builtin(position) pos: vec4<f32>) -> VisDepthOut {
    // Constructor results, no function-local struct `var` (ADR-026).
    var depth = 0.0;
    let id = textureLoad(vis_ids, vec2<i32>(pos.xy), 0).r;
    if (id != VIS_EMPTY) {
        depth = vis_material_depth(vis_instances[vis_cluster(id).x].info.w);
    }
    return VisDepthOut(vec4<f32>(0.0), depth);
}

struct VisCorner {
    world: vec3<f32>,
    clip: vec4<f32>,
    normal: vec3<f32>,
    uv: vec2<f32>,
    tangent: vec4<f32>,
    color: vec4<f32>,
}

fn vis_word_f(at: u32) -> f32 {
    return bitcast<f32>(vis_geo[at]);
}

fn vis_corner(inst: VisInstance, m: VisMeshlet, packed: u32, corner: u32) -> VisCorner {
    let model = inst.model;
    let local = (packed >> (corner * 8u)) & 0xffu;
    let v = vis_geo[m.vertex_offset + local];
    let position = vec3<f32>(vis_word_f(v), vis_word_f(v + 1u), vis_word_f(v + 2u));
    let normal = vec3<f32>(vis_word_f(v + 4u), vis_word_f(v + 5u), vis_word_f(v + 6u));
    let tangent = vec4<f32>(vis_word_f(v + 8u), vis_word_f(v + 9u), vis_word_f(v + 10u), vis_word_f(v + 11u));
    // The forward vertex shader's transforms, term for term: its normal stays the rest normal.
    var world = (model * vec4<f32>(position, 1.0)).xyz;
    let record = inst.flags.y >> 8u;
    if (record != 0u && m.pad == 0u) {
        // The sway (ADR-3916): the raster moved this corner, so the barycentrics and the shaded
        // position must see it moved too.
        world = world + regia::foliage_sway::cluster_offset(
            vis_sway.records[record - 1u],
            vis_sway.frame,
            inst.sphere.xyz,
            inst.scale,
            vis_sway.records[record - 1u].clock.x,
            world,
            (model * vec4<f32>(normal, 0.0)).xyz,
            vis_word_f(v + 12u),
            model[3].xyz,
        );
    }
    return VisCorner(
        world,
        vis_params.view_proj * vec4<f32>(world, 1.0),
        (model * vec4<f32>(normal, 0.0)).xyz,
        vec2<f32>(vis_word_f(v + 3u), vis_word_f(v + 7u)),
        vec4<f32>((model * vec4<f32>(tangent.xyz, 0.0)).xyz, tangent.w),
        vec4<f32>(vis_word_f(v + 12u), vis_word_f(v + 13u), vis_word_f(v + 14u), vis_word_f(v + 15u)),
    );
}

// Perspective-correct barycentrics of the NDC point `p`: the rows of the adjugate of the
// clip-space (x, y, w) matrix are the triangle's homogeneous edge functions.
fn vis_barycentrics(e0: vec3<f32>, e1: vec3<f32>, e2: vec3<f32>, p: vec2<f32>) -> vec3<f32> {
    let q = vec3<f32>(p, 1.0);
    let b = vec3<f32>(dot(e0, q), dot(e1, q), dot(e2, q));
    return b / (b.x + b.y + b.z);
}

fn vis_ndc(pixel: vec2<f32>) -> vec2<f32> {
    let size = max(vis_params.screen.xy, vec2<f32>(1.0));
    return vec2<f32>(pixel.x / size.x * 2.0 - 1.0, 1.0 - pixel.y / size.y * 2.0);
}

@fragment
fn fs_vis_resolve(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    // The equal test against the material depth admits only this material's covered pixels.
    let pixel = vec2<i32>(pos.xy);
    let id = textureLoad(vis_ids, pixel, 0).r;
#ifdef VIS_SINGLE_PASS
    // One draw covers the screen and no material depth rejects: this pixel's record comes from
    // its instance's material slot, and pixels with no geometry or a material that left this
    // frame stay what the pass loaded, as they do under the per-material draws.
    if (id == VIS_EMPTY) {
        discard;
    }
    let slot_record = vis_slot_records[vis_instances[vis_cluster(id).x].info.w];
    if (slot_record == VIS_EMPTY) {
        discard;
    }
    vis_material_record = slot_record;
#endif
    let entry = vis_cluster(id);
    let inst = vis_instances[entry.x];
    let m = vis_meshlets[entry.y];
    let packed = vis_geo[m.triangle_offset + (id & 127u)];
    let a = vis_corner(inst, m, packed, 0u);
    let b = vis_corner(inst, m, packed, 1u);
    let c = vis_corner(inst, m, packed, 2u);
    let ca = a.clip.xyw;
    let cb = b.clip.xyw;
    let cc = c.clip.xyw;
    let e0 = cross(cb, cc);
    let e1 = cross(cc, ca);
    let e2 = cross(ca, cb);
    let p = vis_ndc(pos.xy);
    let w = vis_barycentrics(e0, e1, e2, p);
    // Screen gradients across the pixel's 2×2 quad, from its top-left pixel one pixel right and
    // one pixel down on this triangle: what an implicit-LOD sample in the forward pass reads (the
    // quad's coarse derivatives of the triangle's attribute plane), not a forward difference into
    // whichever triangle the next pixel shows.
    let size = max(vis_params.screen.xy, vec2<f32>(1.0));
    let step_x = vec2<f32>(2.0 / size.x, 0.0);
    let step_y = vec2<f32>(0.0, -2.0 / size.y);
    let odd = vec2<f32>(vec2<u32>(pixel) & vec2<u32>(1u));
    let p0 = p - step_x * odd.x - step_y * odd.y;
    let w0 = vis_barycentrics(e0, e1, e2, p0);
    let wx = vis_barycentrics(e0, e1, e2, p0 + step_x) - w0;
    let wy = vis_barycentrics(e0, e1, e2, p0 + step_y) - w0;
    let uv = a.uv * w.x + b.uv * w.y + c.uv * w.z;
    vis_uv_ddx = a.uv * wx.x + b.uv * wx.y + c.uv * wx.z;
    vis_uv_ddy = a.uv * wy.x + b.uv * wy.y + c.uv * wy.z;
    // The specular-AA term's normal derivatives, the same way (the unit geometric normal).
    let normal = a.normal * w.x + b.normal * w.y + c.normal * w.z;
    let n0 = a.normal * w0.x + b.normal * w0.y + c.normal * w0.z;
    let n0_unit = normalize(n0);
    vis_n_ddx = normalize(n0 + a.normal * wx.x + b.normal * wx.y + c.normal * wx.z) - n0_unit;
    vis_n_ddy = normalize(n0 + a.normal * wy.x + b.normal * wy.y + c.normal * wy.z) - n0_unit;
    vis_face_n = cross(b.world - a.world, c.world - a.world);
#ifdef LIGHTMAP
    let lm = vis_lightmap[entry.x];
    var lightmap_uv = vec2<f32>(0.0);
    var lightmap_so = lm.scale_offset;
    if (lm.scale_offset.x > 0.0) {
        let i0 = vis_vertex_index(lm, m, packed, 0u);
        let i1 = vis_vertex_index(lm, m, packed, 1u);
        let i2 = vis_vertex_index(lm, m, packed, 2u);
        // An appended vertex (a voxel-far cluster) is past the chart. Reading its UV2 word
        // is past the allocation, and a zero UV would sample the chart origin and replace
        // the probe term. A zero scale keeps the probe light.
        if (i0 >= lm.uv2_count || i1 >= lm.uv2_count || i2 >= lm.uv2_count) {
            lightmap_so = vec4<f32>(0.0);
        } else {
            lightmap_uv = vis_chart_uv(lm, i0) * w.x
                + vis_chart_uv(lm, i1) * w.y
                + vis_chart_uv(lm, i2) * w.z;
        }
    }
#endif
    // `VertexOutput` by constructor (ADR-026). Graph materials never take this path, so the
    // MATERIAL_GRAPH fields are not compiled in; a meshlet draw is never a LOD cross-fade and
    // never carries a render override (those draw forward).
    let in = VertexOutput(
        pos,
        a.world * w.x + b.world * w.y + c.world * w.z,
        normal,
        uv,
        a.tangent * w.x + b.tangent * w.y + c.tangent * w.z,
        a.color * w.x + b.color * w.y + c.color * w.z,
        0.0,
        vec2<u32>(0u),
#ifdef LIGHTMAP
        lightmap_uv,
        lightmap_so,
#endif
#ifdef FRAME_FORWARD_DECALS
        // Meshlet draws are rigid: the static-mesh receiver layer (ADR-3255).
        1u,
#endif
    );
    // Counter-clockwise in NDC is front facing (the forward pipelines' `FrontFace::Ccw`); the
    // homogeneous determinant keeps that sign when a vertex is behind the eye.
    let front_facing = dot(ca, e0) > 0.0;
    return shade_surface(in, front_facing);
}
