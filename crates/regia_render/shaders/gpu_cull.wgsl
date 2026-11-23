// SPDX-License-Identifier: MPL-2.0
// Frustum + two-pass Hi-Z occlusion culling into indexed indirect commands.
//
// Planes: Gribb & Hartmann 2001 (inside is distance >= 0).
// Hi-Z: Greene, Kass & Miller, "Hierarchical Z-Buffer Visibility", SIGGRAPH 1993,
// two-pass schedule as in Haar & Aaltonen, "GPU-Driven Rendering Pipelines",
// SIGGRAPH 2015. Standard-Z (0 near, 1 far): a mip stores the farthest depth
// in its footprint, and a sphere is occluded only when its nearest depth is
// still farther than that.

struct CullParams {
    view_proj: mat4x4<f32>,
    planes: array<vec4<f32>, 6>,
    // x = width, y = height, z = 1 when a Hi-Z pyramid is bound, w = mip count.
    screen: vec4<f32>,
    // xyz = camera forward (world, unit). The nearer sphere point is center - forward * radius.
    forward: vec4<f32>,
    instance_count: u32,
    // 1 = order-fixed compaction (`FrameRenderer::set_ordered_gpu_cull`): every sphere owns the
    // slot `batch_base + local`, so a batch draws its instances in submission order whatever
    // order the GPU threads finish in. 0 = the packed atomic compaction.
    // Three scalars, not a vec3: a uniform vec3 is aligned to 16 and would
    // make this struct 224 bytes instead of 208.
    ordered: u32,
    _pad1: u32,
    _pad2: u32,
}

struct Sphere {
    // xyz = world centre, w = radius.
    center_radius: vec4<f32>,
    index_count: u32,
    first_index: u32,
    base_vertex: u32,
    object_slot: u32,
    batch_id: u32,
    batch_base: u32,
    // This sphere's index inside its batch (submission order).
    local: u32,
    _pad1: u32,
}

@group(0) @binding(0) var<uniform> params: CullParams;
@group(0) @binding(1) var<storage, read> spheres: array<Sphere>;
// Tightly packed DrawIndexedIndirect (5 u32s), first_instance = 0 (single-draw path).
@group(0) @binding(2) var<storage, read_write> indirect: array<u32>;
@group(0) @binding(3) var hiz: texture_2d<f32>;
// Instanced path: visible object slots packed per batch (at batch_base + k), and one
// DrawIndexedIndirect per batch whose instance_count is incremented here (the CPU writes
// index_count and first_instance = batch_base, instance_count = 0, every frame). Ordered mode
// (`params.ordered`): slot `batch_base + local` holds the object, or `CULLED_SLOT` when the
// sphere is culled, and instance_count is the highest visible `local` + 1 (an `atomicMax`, which
// does not depend on thread order); the vertex stage collapses a culled slot to a point.
@group(0) @binding(7) var<storage, read_write> slots: array<u32>;
@group(0) @binding(8) var<storage, read_write> batch_cmds: array<atomic<u32>>;

// An ordered-mode slot whose sphere was culled (`lab_forward.wgsl` `current_object`).
const CULLED_SLOT: u32 = 0xffffffffu;

fn frustum_inside(center: vec3<f32>, radius: f32) -> bool {
    for (var i: u32 = 0u; i < 6u; i = i + 1u) {
        let plane = params.planes[i];
        let distance = dot(plane.xyz, center) + plane.w;
        if (distance < -radius) {
            return false;
        }
    }
    return true;
}

fn ndc_of(world: vec3<f32>) -> vec4<f32> {
    let clip = params.view_proj * vec4<f32>(world, 1.0);
    return vec4<f32>(clip.xyz / clip.w, clip.w);
}

// Mip-0 texels along one axis that level `mip` of the farthest-depth pyramid covers. Every
// level's size rounds down (`max(1, size >> mip)`) and a 2×2 reduction reads texels 2i and
// 2i + 1, so the last texel of an odd level never reaches the next level: level `mip` covers
// `(size >> mip) << mip` texels. A level clamped to one texel covers what the last level
// that still had a texel of its own covered.
fn hiz_cover(size: u32, mip: u32) -> u32 {
    let level = size >> mip;
    if (level == 0u) {
        return 1u << firstLeadingBit(max(size, 1u));
    }
    return level << mip;
}

// Conservative Hi-Z test (ADR-1238). The sphere's screen rectangle is bounded from the
// view-projection itself (no field-of-view or aspect assumption), the mip is the one whose
// texels are at least half the rectangle's longer side (so it spans at most 3×3 of them), and
// every one of those texels is read. A rectangle that reaches pixels the chosen mip does not
// cover (the dropped last row or column of an odd level), or that still spans more than 3×3
// texels on the coarsest mip, is never culled.
fn hiz_occluded(center: vec3<f32>, radius: f32) -> bool {
    if (params.screen.z < 0.5) {
        return false;
    }
    let m = params.view_proj;
    let clip = m * vec4<f32>(center, 1.0);
    // Rows x, y and w of the matrix's linear part (column-major storage).
    let ax = vec3<f32>(m[0].x, m[1].x, m[2].x);
    let ay = vec3<f32>(m[0].y, m[1].y, m[2].y);
    let aw = vec3<f32>(m[0].w, m[1].w, m[2].w);
    let reach = radius * length(aw);
    // A sphere that reaches the camera plane cannot be tested: its nearer point (and
    // possibly its centre) project from behind the eye, the sign of the projected
    // depth flips, and the test culled it. That dropped a cockpit roof from the colour
    // pass while the depth prepass still wrote it, leaving clear-colour black (the
    // black cockpit interiors in the DO27 frames). Such objects are always drawn.
    if (clip.w - radius <= 0.0 || clip.w - reach <= 0.0) {
        return false;
    }
    // For a sphere point p = c + v (|v| <= r):
    //   ndc(p) - ndc(c) = (a - ndc(c) aw) · v / (w + aw · v),
    // so |Δndc| <= r |a - ndc(c) aw| / (w - r |aw|) on each axis.
    let ndc = clip.xy / clip.w;
    let denom = clip.w - reach;
    let half_ndc = vec2<f32>(length(ax - ndc.x * aw), length(ay - ndc.y * aw)) * (radius / denom);
    let size = textureDimensions(hiz, 0);
    let dims = vec2<f32>(size);
    let center_px = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5) * dims;
    let half_px = half_ndc * 0.5 * dims;
    let lo_f = center_px - half_px;
    let hi_f = center_px + half_px;
    // Entirely off the pyramid (the frustum planes passed it): nothing to test against.
    if (hi_f.x < 0.0 || hi_f.y < 0.0 || lo_f.x >= dims.x || lo_f.y >= dims.y) {
        return false;
    }
    let lo = vec2<u32>(max(floor(lo_f), vec2<f32>(0.0)));
    let hi = vec2<u32>(min(floor(hi_f), dims - vec2<f32>(1.0)));
    let span = max(hi.x - lo.x, hi.y - lo.y) + 1u;
    // 2^mip >= span / 2: the pixel range touches at most three texels per axis on this level.
    // One level finer than the 2×2 choice: 9 loads instead of 4, and about half again as many
    // hidden spheres culled (ADR-1238).
    var mip = 0u;
    if (span > 2u) {
        mip = firstLeadingBit(span - 1u);
    }
    mip = min(mip, textureNumLevels(hiz) - 1u);
    if (hi.x >= hiz_cover(size.x, mip) || hi.y >= hiz_cover(size.y, mip)) {
        return false;
    }
    let t_lo = lo >> vec2<u32>(mip);
    let t_hi = hi >> vec2<u32>(mip);
    if (t_hi.x - t_lo.x > 2u || t_hi.y - t_lo.y > 2u) {
        return false;
    }
    let level = i32(mip);
    // Fixed 3×3 trip count with the range as an `if` (FXC unrolls loops holding texture
    // operations; ADR-201).
    var farthest = 0.0;
    for (var j = 0u; j < 3u; j = j + 1u) {
        for (var i = 0u; i < 3u; i = i + 1u) {
            let t = t_lo + vec2<u32>(i, j);
            if (t.x <= t_hi.x && t.y <= t_hi.y) {
                farthest = max(farthest, textureLoad(hiz, vec2<i32>(t), level).r);
            }
        }
    }
    // The nearest depth of the sphere: its point closest to the camera plane (along the w row;
    // `params.forward` for a projection without one, such as the tests' identity).
    var toward = params.forward.xyz;
    if (dot(aw, aw) > 1.0e-12) {
        toward = normalize(aw);
    }
    let nearest_z = min(ndc_of(center).z, ndc_of(center - toward * radius).z);
    // Fully behind the farthest depth under the rectangle. A small bias keeps
    // grazing surfaces (the occluder itself) from culling themselves.
    return nearest_z > farthest + 0.002;
}

// Naga rejects a storage pointer as a function argument, so each command buffer
// is written in place. Five u32s, DrawIndexedIndirect order.

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let index = gid.x;
    if (index >= params.instance_count) {
        return;
    }
    let sphere = spheres[index];
    let center = sphere.center_radius.xyz;
    let radius = sphere.center_radius.w;
    var instances = 0u;
    let visible = frustum_inside(center, radius) && !hiz_occluded(center, radius);
    if (params.ordered != 0u) {
        if (visible) {
            instances = 1u;
            atomicMax(&batch_cmds[sphere.batch_id * 5u + 1u], sphere.local + 1u);
            slots[sphere.batch_base + sphere.local] = sphere.object_slot;
        } else {
            slots[sphere.batch_base + sphere.local] = CULLED_SLOT;
        }
    } else if (visible) {
        instances = 1u;
        let k = atomicAdd(&batch_cmds[sphere.batch_id * 5u + 1u], 1u);
        slots[sphere.batch_base + k] = sphere.object_slot;
    }
    let base = index * 5u;
    indirect[base + 0u] = sphere.index_count;
    indirect[base + 1u] = instances;
    indirect[base + 2u] = sphere.first_index;
    indirect[base + 3u] = sphere.base_vertex;
    indirect[base + 4u] = 0u;
}
