// SPDX-License-Identifier: MPL-2.0
// Grass generator (ADR-1400): three compute passes at the start of the frame.
//
// 1. `cs_count`, one workgroup per patch: evaluate every candidate of the patch (the density
//    law, the kind mask, the frustum, the shadow distance) and count the survivors of each
//    region (near, mid, far LOD for the camera; the shadow set).
// 2. `cs_scan`, one workgroup: exclusive prefix sums of the counts over the patches, in patch
//    order, and the four indirect draws.
// 3. `cs_write`, one workgroup per patch: evaluate again and write each survivor at its
//    patch's offset plus its rank among the patch's survivors (a workgroup prefix sum in
//    candidate order).
//
// No global atomics: the instance order is a pure function of the inputs, so two renders of
// the same frame write the same buffer (the workgroup counters only sum).
// Prepended at load: `grass_common.wgsl`, `grass_place.wgsl`.

@group(0) @binding(0) var<uniform> params: GrassParams;
@group(0) @binding(1) var<uniform> classes: GrassClasses;
@group(0) @binding(2) var<storage, read> patches: array<GrassPatch>;
@group(0) @binding(3) var height_texture: texture_2d<f32>;
@group(0) @binding(4) var mask_texture: texture_2d_array<f32>;
@group(0) @binding(5) var mask_samp: sampler;
// Words 0..20: four `draw_indexed_indirect` argument blocks (5 words each). From word 64:
// four counts (then offsets) per patch.
@group(0) @binding(6) var<storage, read_write> state: array<u32>;
@group(0) @binding(7) var<storage, read_write> instances: array<GrassInst>;

const STATE_COUNTS: u32 = 64u;
const WG: u32 = 64u;

// The LOD sections of the static index buffer: index count and first index per region (the
// scan copies them into the indirect draws).
struct GrassDrawArgs {
    index_count: vec4<u32>,
    first_index: vec4<u32>,
}
@group(0) @binding(8) var<uniform> draw_args: GrassDrawArgs;

// Manual bilinear height (`height_at` in `terrain.wgsl`): the exact base surface.
fn grass_height(xz: vec2<f32>) -> f32 {
    let s = (xz + vec2<f32>(params.map.x)) / params.map.y * params.map.zw;
    let dims = vec2<i32>(params.map.zw) + vec2<i32>(1);
    let p = clamp(s, vec2<f32>(0.0), params.map.zw);
    let i = vec2<i32>(floor(p));
    let f = p - vec2<f32>(i);
    let i1 = min(i + vec2<i32>(1, 1), dims - vec2<i32>(1, 1));
    let h00 = textureLoad(height_texture, i, 0).r;
    let h10 = textureLoad(height_texture, vec2<i32>(i1.x, i.y), 0).r;
    let h01 = textureLoad(height_texture, vec2<i32>(i.x, i1.y), 0).r;
    let h11 = textureLoad(height_texture, i1, 0).r;
    let a = mix(h00, h10, f.x);
    let b = mix(h01, h11, f.x);
    return mix(a, b, f.y) * params.heights.x + params.heights.y;
}

struct GrassEval {
    // Bit r set: the clump goes to region r.
    regions: u32,
    inst: GrassInst,
}

fn grass_none() -> GrassEval {
    return GrassEval(0u, GrassInst(vec3<f32>(0.0), 0u, 0u, 0u, 0u, 0u));
}

// Lane `r` of `v` (select chain: no dynamic vector index, the Mali rule of `terrain.wgsl`).
fn grass_lane(v: vec4<u32>, r: u32) -> u32 {
    return select(select(select(v.w, v.z, r == 2u), v.y, r == 1u), v.x, r == 0u);
}

fn grass_eval(gp: GrassPatch, k: u32) -> GrassEval {
    if (k >= gp.candidates) {
        return grass_none();
    }
    let slot = grass_walk_slot(gp.fine, gp.level, k);
    if (slot.index >= gp.per_fine) {
        return grass_none();
    }
    let cls = classes.items[min(gp.kind, 7u)];
    let cand = grass_candidate(cls.seed.x, slot.fine, slot.index, gp.per_fine, params.heights.z);
    // `map.x` is the map-extent lane: the map spans `-map.x .. map.y - map.x` (its grid origin
    // folded in, ADR-8179; `map.y - map.x` is `map.x` exactly for a map on the world origin).
    let half = params.map.x;
    if (any(cand.xz < vec2<f32>(-half)) || any(cand.xz >= vec2<f32>(params.map.y - half))) {
        return grass_none();
    }
    let uv = (cand.xz + vec2<f32>(half)) / params.map.y;
    let m = textureSampleLevel(mask_texture, mask_samp, uv, i32(cls.seed.y), 0.0);
    let y = grass_height(cand.xz);
    let root = vec3<f32>(cand.xz.x, y, cand.xz.y);
    let eye = params.eye.xyz;
    let d = distance(root, eye);
    let f_dist = grass_density_fraction(d, params.law.x, cls.misc.x);
    let f = f_dist * m.r * params.law.y;
    if (cand.rank >= f) {
        return grass_none();
    }
    // Clumps shrink into the ground over the last fifth of their rank band (no pop as the
    // law or the mask thins them).
    let fade = clamp((f - cand.rank) / max(0.2 * f, 1.0e-6), 0.0, 1.0);
    let height_mul = m.b * 2.0;
    let tall = cls.shape.y * max(height_mul, 0.05) * 1.3 + cls.misc.z;
    var regions = 0u;
    // Camera: the clump's bounding sphere against the frustum.
    let centre = root + vec3<f32>(0.0, tall * 0.5, 0.0);
    let radius = tall * 0.6 + cls.shape.w + cls.misc.z;
    // A patch behind the walls of the eye's cell (the outdoor-portal cull) draws no camera blade.
    var inside = (gp.flags & GRASS_PATCH_NO_CAMERA) == 0u;
    for (var p = 0u; p < 6u; p = p + 1u) {
        let plane = params.planes[p];
        if (dot(plane.xyz, centre) + plane.w < -radius) {
            inside = false;
        }
    }
    let jitter = 1.0 + (grass_unit(grass_pcg(cand.hash ^ 0x51ed27u)) - 0.5) * 0.3;
    if (inside) {
        if (d < params.lod.x * jitter) {
            regions = regions | 1u;
        } else if (d < params.lod.y * jitter) {
            regions = regions | 2u;
        } else {
            regions = regions | 4u;
        }
    }
    if (params.counts.y != 0u && (gp.flags & GRASS_PATCH_SHADOW) != 0u && d < cls.misc.y) {
        regions = regions | 8u;
    }
    if (regions == 0u) {
        return grass_none();
    }
    // Terrain normal (central differences over one heightfield sample).
    let step = params.map.y / max(params.map.z, 1.0);
    let hx = grass_height(cand.xz + vec2<f32>(step, 0.0)) - grass_height(cand.xz - vec2<f32>(step, 0.0));
    let hz = grass_height(cand.xz + vec2<f32>(0.0, step)) - grass_height(cand.xz - vec2<f32>(0.0, step));
    let n = normalize(vec3<f32>(-hx, 2.0 * step, -hz));
    // Sink the root with distance: the drawn terrain LOD can sit up to ~0.2 m off the base
    // surface far away, and a floating clump reads worse than a buried one.
    let sink = clamp(d * 0.001, 0.0, 0.2);
    let widen = clamp(inverseSqrt(max(f_dist, 1.0e-6)), 1.0, params.law.z);
    let inst = GrassInst(
        root - vec3<f32>(0.0, sink, 0.0),
        cand.hash,
        pack2x16unorm(vec2<f32>(widen * 0.125, fade)),
        pack4x8unorm(vec4<f32>(m.g, m.b, m.a, 0.0)),
        pack2x16snorm(n.xz),
        (gp.kind & 0xffu) | (gp.level << 8u),
    );
    return GrassEval(regions, inst);
}

var<workgroup> wg_counts: array<atomic<u32>, 4>;

@compute @workgroup_size(64)
fn cs_count(@builtin(workgroup_id) wid: vec3<u32>, @builtin(local_invocation_index) lid: u32) {
    let index = wid.x;
    // Workgroup memory starts zeroed; the loop bound is uniform (a read-only load at the
    // workgroup's own index).
    let gp = patches[index];
    let n = gp.candidates;
    for (var k = lid; k < n; k = k + WG) {
        let e = grass_eval(gp, k);
        for (var r = 0u; r < GRASS_REGIONS; r = r + 1u) {
            if ((e.regions & (1u << r)) != 0u) {
                atomicAdd(&wg_counts[r], 1u);
            }
        }
    }
    workgroupBarrier();
    if (lid < 4u) {
        state[STATE_COUNTS + index * 4u + lid] = atomicLoad(&wg_counts[lid]);
    }
}

var<workgroup> scan_sums: array<vec4<u32>, 256>;

@compute @workgroup_size(256)
fn cs_scan(@builtin(local_invocation_index) lid: u32) {
    let count = params.counts.x;
    let per = (count + 255u) / 256u;
    let first = lid * per;
    let last = min(first + per, count);
    var sum = vec4<u32>(0u);
    for (var p = first; p < last; p = p + 1u) {
        let b = STATE_COUNTS + p * 4u;
        sum = sum + vec4<u32>(state[b], state[b + 1u], state[b + 2u], state[b + 3u]);
    }
    scan_sums[lid] = sum;
    workgroupBarrier();
    if (lid == 0u) {
        var running = vec4<u32>(0u);
        for (var i = 0u; i < 256u; i = i + 1u) {
            let s = scan_sums[i];
            scan_sums[i] = running;
            running = running + s;
        }
        for (var r = 0u; r < GRASS_REGIONS; r = r + 1u) {
            let a = r * 5u;
            state[a] = draw_args.index_count[r];
            state[a + 1u] = min(grass_lane(running, r), params.caps[r]);
            state[a + 2u] = draw_args.first_index[r];
            state[a + 3u] = 0u;
            state[a + 4u] = 0u;
        }
    }
    workgroupBarrier();
    var offset = scan_sums[lid];
    for (var p = first; p < last; p = p + 1u) {
        let b = STATE_COUNTS + p * 4u;
        let c = vec4<u32>(state[b], state[b + 1u], state[b + 2u], state[b + 3u]);
        state[b] = offset.x;
        state[b + 1u] = offset.y;
        state[b + 2u] = offset.z;
        state[b + 3u] = offset.w;
        offset = offset + c;
    }
}

// Packed survivor flags of one chunk: region r's flag in byte r (≤ 64 per byte).
var<workgroup> wg_flags: array<u32, 64>;
var<workgroup> wg_total: u32;

fn region_base(r: u32) -> u32 {
    var base = 0u;
    for (var i = 0u; i < r; i = i + 1u) {
        base = base + params.caps[i];
    }
    return base;
}

@compute @workgroup_size(64)
fn cs_write(@builtin(workgroup_id) wid: vec3<u32>, @builtin(local_invocation_index) lid: u32) {
    let index = wid.x;
    let gp = patches[index];
    let n = gp.candidates;
    let b = STATE_COUNTS + index * 4u;
    let start = vec4<u32>(state[b], state[b + 1u], state[b + 2u], state[b + 3u]);
    var running = vec4<u32>(0u);
    let chunks = (n + WG - 1u) / WG;
    for (var chunk = 0u; chunk < chunks; chunk = chunk + 1u) {
        let e = grass_eval(gp, chunk * WG + lid);
        var packed = 0u;
        for (var r = 0u; r < GRASS_REGIONS; r = r + 1u) {
            if ((e.regions & (1u << r)) != 0u) {
                packed = packed | (1u << (8u * r));
            }
        }
        wg_flags[lid] = packed;
        workgroupBarrier();
        // Hillis–Steele inclusive scan of the packed flags (bytes never carry: ≤ 64 each).
        for (var stride = 1u; stride < WG; stride = stride * 2u) {
            var add = 0u;
            if (lid >= stride) {
                add = wg_flags[lid - stride];
            }
            workgroupBarrier();
            wg_flags[lid] = wg_flags[lid] + add;
            workgroupBarrier();
        }
        let inclusive = wg_flags[lid];
        let exclusive = inclusive - packed;
        for (var r = 0u; r < GRASS_REGIONS; r = r + 1u) {
            if ((e.regions & (1u << r)) != 0u) {
                let rank = (exclusive >> (8u * r)) & 0xffu;
                let slot = grass_lane(start, r) + grass_lane(running, r) + rank;
                if (slot < params.caps[r]) {
                    instances[region_base(r) + slot] = e.inst;
                }
            }
        }
        if (lid == WG - 1u) {
            wg_total = inclusive;
        }
        workgroupBarrier();
        // Read before any lane reaches the next chunk's barriers (and its write).
        let total = wg_total;
        running = running + vec4<u32>(total & 0xffu, (total >> 8u) & 0xffu, (total >> 16u) & 0xffu, total >> 24u);
    }
}
