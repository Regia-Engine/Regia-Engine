// SPDX-License-Identifier: MPL-2.0
// Two-phase occlusion culling of the shadow casters (`crate::shadow_occlusion`, ADR-2072;
// Haar & Aaltonen 2015, applied to light views).
//
// One dispatch per shadow-map kind (a sun cascade or the local-light atlas). Each entry is one
// caster instance of one shadow job; its command slot is the instance's slot in the shadow
// object buffer, so the shadow pass draws a batch `start..start + count` as one multi-draw over
// commands `start..start + count` (an instance count of 0 or 1 per command).
//
// Phase 1 tests the job's casters against the kind's pyramid as it was left by the kind's last
// redraw, projected with the matrix that redraw used (`Job::prev_view_proj`); a job with no such
// history draws every caster. Phase 2 runs after the shadow pass drew phase 1's survivors and the
// pyramid was rebuilt from that depth: it tests what phase 1 skipped with this frame's matrix.
//
// The test is conservative in every step, so a caster phase 2 skips writes no texel of the
// unculled map: the eight corners of the bounding sphere's box bound the projected footprint
// and its nearest depth (perspective and orthographic light views alike), the footprint grows
// by a texel on every side and is clipped to the job's scissor (a sun page or an atlas tile),
// and the pyramid keeps the farthest depth of each block. Depth writes under `Less` keep the
// nearest value whatever the order, so the map holds the unculled map's depth.

struct Entry {
    // World centre and radius; a negative radius has no bound (always drawn in phase 1).
    center_radius: vec4<f32>,
    index_count: u32,
    first_index: u32,
    // The command's `base_vertex` bits (an `i32`).
    base_vertex: u32,
    // Index into `jobs`.
    job: u32,
    // Command slot = the instance's slot in the shadow object buffer.
    slot: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

struct Job {
    view_proj: mat4x4<f32>,
    prev_view_proj: mat4x4<f32>,
    // Viewport in map texels: x, y, width, height.
    viewport: vec4<f32>,
    // Scissor in map texels: x0, y0, x1, y1 (exclusive).
    clip: vec4<f32>,
    // x: 1 when `prev_view_proj` describes the pyramid phase 1 reads.
    flags: vec4<u32>,
}

struct KindParams {
    entry_offset: u32,
    entry_count: u32,
    // Map texels per pyramid base texel, along each axis.
    block: u32,
    // Mip levels of the pyramid.
    levels: u32,
}

@group(0) @binding(0) var<uniform> kind: KindParams;
@group(0) @binding(1) var<storage, read> entries: array<Entry>;
@group(0) @binding(2) var<storage, read> jobs: array<Job>;
@group(0) @binding(3) var<storage, read_write> phase1: array<u32>;
@group(0) @binding(4) var<storage, read_write> phase2: array<u32>;
@group(0) @binding(5) var pyramid: texture_2d<f32>;

// NDC depth margin: the compute transform and the rasterizer round differently. No shadow
// pipeline uses a hardware depth bias, so the rasterized depth is the geometry's.
const DEPTH_MARGIN: f32 = 1e-5;

fn write_command(buffer: u32, e: Entry, count: u32) {
    let base = e.slot * 5u;
    if (buffer == 1u) {
        phase1[base] = e.index_count;
        phase1[base + 1u] = count;
        phase1[base + 2u] = e.first_index;
        phase1[base + 3u] = e.base_vertex;
        phase1[base + 4u] = e.slot;
    } else {
        phase2[base] = e.index_count;
        phase2[base + 1u] = count;
        phase2[base + 2u] = e.first_index;
        phase2[base + 3u] = e.base_vertex;
        phase2[base + 4u] = e.slot;
    }
}

// True unless the pyramid proves every texel the caster could write already holds a nearer depth.
fn visible(e: Entry, job: Job, vp: mat4x4<f32>) -> bool {
    let c = e.center_radius.xyz;
    let r = e.center_radius.w;
    if (r > 1.0e6) {
        return true;
    }
    var lo = vec3<f32>(1.0e30);
    var hi = vec2<f32>(-1.0e30);
    for (var i = 0u; i < 8u; i = i + 1u) {
        let corner = c + vec3<f32>(
            select(-r, r, (i & 1u) != 0u),
            select(-r, r, (i & 2u) != 0u),
            select(-r, r, (i & 4u) != 0u),
        );
        let clip = vp * vec4<f32>(corner, 1.0);
        // A corner at or behind the light's eye plane: no bound on the footprint.
        if (clip.w <= 1.0e-6) {
            return true;
        }
        let ndc = clip.xyz / clip.w;
        lo = min(lo, ndc);
        hi = max(hi, ndc.xy);
    }
    // NDC to map texels (rows grow downward).
    let x0 = job.viewport.x + (lo.x * 0.5 + 0.5) * job.viewport.z;
    let x1 = job.viewport.x + (hi.x * 0.5 + 0.5) * job.viewport.z;
    let y0 = job.viewport.y + (0.5 - hi.y * 0.5) * job.viewport.w;
    let y1 = job.viewport.y + (0.5 - lo.y * 0.5) * job.viewport.w;
    let fx0 = max(floor(x0) - 1.0, job.clip.x);
    let fy0 = max(floor(y0) - 1.0, job.clip.y);
    let fx1 = min(floor(x1) + 1.0, job.clip.z - 1.0);
    let fy1 = min(floor(y1) + 1.0, job.clip.w - 1.0);
    // Nothing of it lands inside the job's scissor: it writes no texel.
    if (fx1 < fx0 || fy1 < fy0) {
        return false;
    }
    let block = max(kind.block, 1u);
    var a = vec2<u32>(u32(fx0) / block, u32(fy0) / block);
    var b = vec2<u32>(u32(fx1) / block, u32(fy1) / block);
    // The finest level where the footprint covers at most 2×2 texels.
    var level = 0u;
    loop {
        if ((b.x - a.x <= 1u && b.y - a.y <= 1u) || level + 1u >= kind.levels) {
            break;
        }
        a = a >> vec2<u32>(1u);
        b = b >> vec2<u32>(1u);
        level = level + 1u;
    }
    let dims = textureDimensions(pyramid, level);
    let bx = min(b.x, dims.x - 1u);
    let by = min(b.y, dims.y - 1u);
    var farthest = 0.0;
    for (var y = a.y; y <= by; y = y + 1u) {
        for (var x = a.x; x <= bx; x = x + 1u) {
            // i32 coordinates: naga's bounds-checked load of a u32 pair writes invalid SPIR-V (ADR-2140).
            farthest = max(farthest, textureLoad(pyramid, vec2<i32>(i32(x), i32(y)), i32(level)).x);
        }
    }
    return lo.z <= farthest + DEPTH_MARGIN;
}

@compute @workgroup_size(64)
fn cs_phase1(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= kind.entry_count) {
        return;
    }
    let e = entries[kind.entry_offset + id.x];
    let job = jobs[e.job];
    var draw = true;
    if (e.center_radius.w >= 0.0 && job.flags.x == 1u) {
        draw = visible(e, job, job.prev_view_proj);
    }
    write_command(1u, e, select(0u, 1u, draw));
    // Phase 2's command starts empty: a kind whose phase 2 is not recorded draws nothing twice.
    write_command(2u, e, 0u);
}

@compute @workgroup_size(64)
fn cs_phase2(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= kind.entry_count) {
        return;
    }
    let e = entries[kind.entry_offset + id.x];
    let job = jobs[e.job];
    var draw = false;
    // Unbounded casters always draw in phase 1.
    if (phase1[e.slot * 5u + 1u] == 0u && e.center_radius.w >= 0.0) {
        draw = visible(e, job, job.view_proj);
    }
    write_command(2u, e, select(0u, 1u, draw));
}
