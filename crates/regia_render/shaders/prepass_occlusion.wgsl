// SPDX-License-Identifier: MPL-2.0
// Two-phase occlusion cull of the forward depth prepass (`docs/specs/98-visibility-buffer.md`,
// ADR-1887).
//
// Schedule: Haar & Aaltonen, "GPU-Driven Rendering Pipelines", SIGGRAPH 2015. Phase 1 draws what
// last frame's depth pyramid does not hide; the pyramid is rebuilt from that depth; phase 2 draws
// what phase 1 skipped and the new pyramid does not hide. Phase 2's pyramid holds a subset of
// this frame's opaque depth, so it is never nearer than the full prepass depth: an instance it
// hides is hidden by the full prepass too, and the two phases together write the depth an
// unculled prepass writes (Less test, opaque and alpha-tested writes only).
//
// Hi-Z: Greene, Kass & Miller, "Hierarchical Z-Buffer Visibility", SIGGRAPH 1993. The sphere test
// is `gpu_cull.wgsl`'s conservative one (ADR-1238), with the view-projection as an argument:
// phase 1 projects with the matrix last frame's pyramid was built with.
//
// Standard Z (0 near, 1 far): a pyramid texel holds the farthest depth of its footprint.

struct Params {
    // This frame's unjittered view-projection (phase 2's pyramid).
    view_proj: mat4x4<f32>,
    // The view-projection last frame's pyramid was built with (phase 1).
    prev_view_proj: mat4x4<f32>,
    planes: array<vec4<f32>, 6>,
    // x = width, y = height, z = 1 when phase 1 tests last frame's pyramid, w = 1 when phase 2
    // runs (set together: without last frame's pyramid phase 1 draws every instance in the
    // frustum and phase 2 is not recorded).
    screen: vec4<f32>,
    // xyz = camera forward (world, unit), for a projection without a w row.
    forward: vec4<f32>,
    instance_count: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

struct Instance {
    // xyz = world centre, w = radius; w < 0: no bound, always drawn in phase 1.
    center_radius: vec4<f32>,
    index_count: u32,
    first_index: u32,
    base_vertex: u32,
    first_instance: u32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> instances: array<Instance>;
// One DrawIndexedIndirect (5 u32s) per instance and phase; instance_count is 0 or 1.
@group(0) @binding(2) var<storage, read_write> phase1: array<u32>;
@group(0) @binding(3) var<storage, read_write> phase2: array<u32>;
@group(0) @binding(4) var hiz: texture_2d<f32>;

fn frustum_inside(center: vec3<f32>, radius: f32) -> bool {
    for (var i: u32 = 0u; i < 6u; i = i + 1u) {
        let plane = params.planes[i];
        if (dot(plane.xyz, center) + plane.w < -radius) {
            return false;
        }
    }
    return true;
}

fn ndc_z(m: mat4x4<f32>, world: vec3<f32>) -> f32 {
    let clip = m * vec4<f32>(world, 1.0);
    return clip.z / clip.w;
}

// Mip-0 texels along one axis that level `mip` covers (`gpu_cull.wgsl`).
fn hiz_cover(size: u32, mip: u32) -> u32 {
    let level = size >> mip;
    if (level == 0u) {
        return 1u << firstLeadingBit(max(size, 1u));
    }
    return level << mip;
}

// `gpu_cull.wgsl` `hiz_occluded` (ADR-1238) under the view-projection `m`.
fn hiz_occluded(m: mat4x4<f32>, center: vec3<f32>, radius: f32) -> bool {
    let clip = m * vec4<f32>(center, 1.0);
    let ax = vec3<f32>(m[0].x, m[1].x, m[2].x);
    let ay = vec3<f32>(m[0].y, m[1].y, m[2].y);
    let aw = vec3<f32>(m[0].w, m[1].w, m[2].w);
    let reach = radius * length(aw);
    // A sphere that reaches the camera plane is never culled.
    if (clip.w - radius <= 0.0 || clip.w - reach <= 0.0) {
        return false;
    }
    let ndc = clip.xy / clip.w;
    let denom = clip.w - reach;
    let half_ndc = vec2<f32>(length(ax - ndc.x * aw), length(ay - ndc.y * aw)) * (radius / denom);
    let size = textureDimensions(hiz, 0);
    let dims = vec2<f32>(size);
    let center_px = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5) * dims;
    // One pixel wider on every side: the depth is rasterized with the temporal jitter (up to
    // half a pixel, `taau::halton_jitter`) and this test projects without it, so a sphere's
    // jittered coverage can reach a pixel past its unjittered rectangle. Without the margin,
    // phase 2 could hide an instance the jittered prepass shows (ADR-1887).
    let half_px = half_ndc * 0.5 * dims + vec2<f32>(1.0);
    let lo_f = center_px - half_px;
    let hi_f = center_px + half_px;
    if (hi_f.x < 0.0 || hi_f.y < 0.0 || lo_f.x >= dims.x || lo_f.y >= dims.y) {
        return false;
    }
    let lo = vec2<u32>(max(floor(lo_f), vec2<f32>(0.0)));
    let hi = vec2<u32>(min(floor(hi_f), dims - vec2<f32>(1.0)));
    let span = max(hi.x - lo.x, hi.y - lo.y) + 1u;
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
    // Fixed 3×3 trip count with the range as an `if` (ADR-201).
    var farthest = 0.0;
    for (var j = 0u; j < 3u; j = j + 1u) {
        for (var i = 0u; i < 3u; i = i + 1u) {
            let t = t_lo + vec2<u32>(i, j);
            if (t.x <= t_hi.x && t.y <= t_hi.y) {
                farthest = max(farthest, textureLoad(hiz, vec2<i32>(t), level).r);
            }
        }
    }
    var toward = params.forward.xyz;
    if (dot(aw, aw) > 1.0e-12) {
        toward = normalize(aw);
    }
    let nearest_z = min(ndc_z(m, center), ndc_z(m, center - toward * radius));
    return nearest_z > farthest + 0.002;
}

// Naga rejects a storage pointer as a function argument, so each phase writes its own buffer.

@compute @workgroup_size(64)
fn cs_phase1(@builtin(global_invocation_id) gid: vec3<u32>) {
    let index = gid.x;
    if (index >= params.instance_count) {
        return;
    }
    let inst = instances[index];
    let center = inst.center_radius.xyz;
    let radius = inst.center_radius.w;
    var visible = true;
    if (radius >= 0.0) {
        visible = frustum_inside(center, radius);
        if (visible && params.screen.z > 0.5) {
            visible = !hiz_occluded(params.prev_view_proj, center, radius);
        }
    }
    let base = index * 5u;
    phase1[base + 0u] = inst.index_count;
    phase1[base + 1u] = select(0u, 1u, visible);
    phase1[base + 2u] = inst.first_index;
    phase1[base + 3u] = inst.base_vertex;
    phase1[base + 4u] = inst.first_instance;
}

@compute @workgroup_size(64)
fn cs_phase2(@builtin(global_invocation_id) gid: vec3<u32>) {
    let index = gid.x;
    if (index >= params.instance_count) {
        return;
    }
    let inst = instances[index];
    let center = inst.center_radius.xyz;
    let radius = inst.center_radius.w;
    let base = index * 5u;
    var visible = false;
    if (phase1[base + 1u] == 0u && radius >= 0.0 && params.screen.w > 0.5) {
        visible = frustum_inside(center, radius) && !hiz_occluded(params.view_proj, center, radius);
    }
    phase2[base + 0u] = inst.index_count;
    phase2[base + 1u] = select(0u, 1u, visible);
    phase2[base + 2u] = inst.first_index;
    phase2[base + 3u] = inst.base_vertex;
    phase2[base + 4u] = inst.first_instance;
}
