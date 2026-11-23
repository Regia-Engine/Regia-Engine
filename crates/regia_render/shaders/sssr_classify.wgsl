// SPDX-License-Identifier: MIT
// Portions ported from the AMD FidelityFX SDK (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK,
// tag v1.1.4, commit c6efa6bf7f), sdk/include/FidelityFX/gpu/sssr/ffx_sssr_classify_tiles.h and
// ffx_sssr_prepare_indirect_args.h,
// Copyright (C) 2024 Advanced Micro Devices, Inc., MIT licence.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

// SSSR tile classification: decides per pixel whether a reflection ray is traced, compacts the
// 2×2 quads with rays into a list (one entry per quad, a mask of its traced pixels) and the 8×8
// tiles that need the denoiser into another, and writes the normal and roughness the later passes
// (and the next frame) read.
//
// Changes for Regia Engine: the wave intrinsics (QuadReadAcross*, WavePrefixCountBits,
// WaveActiveCountBits) are workgroup memory and workgroup atomics, so the pass runs the same on
// every backend; the normal is reconstructed from depth; the surface code is decoded from the HDR
// alpha; a pixel past the glossy threshold writes no radiance (the forward pass already holds its
// environment specular).

#import regia::sssr::{sssr, decode_alpha, is_glossy, is_mirror, world_from_uvz, pack_surface, unpack_pair, cost_bound_on, block_rate}
#ifdef SSSR_STATS
#import regia::sssr::SssrCost
#endif

struct Counters {
    rays: atomic<u32>,
    tiles: atomic<u32>,
}

@group(0) @binding(1) var hdr_tex: texture_2d<f32>;
// The frame's depth buffer (single sample, standard Z).
@group(0) @binding(2) var depth_tex: texture_2d<f32>;
@group(0) @binding(3) var normal_out: texture_storage_2d<r32uint, write>;
@group(0) @binding(4) var radiance_out: texture_storage_2d<rgba16float, write>;
@group(0) @binding(5) var<storage, read_write> ray_list: array<u32>;
@group(0) @binding(6) var<storage, read_write> tile_list: array<u32>;
@group(0) @binding(7) var<storage, read_write> counters: Counters;
// pack_pair(temporal variance, sample count) of the previous resolve.
@group(0) @binding(8) var variance_history: texture_2d<u32>;

var<workgroup> wg_rays: atomic<u32>;
var<workgroup> wg_glossy: atomic<u32>;
var<workgroup> wg_base: u32;
// Which pixels of the group trace a ray: bit 4·quad + (x & 1) + 2·(y & 1), quads 0..7 in `lo`,
// 8..15 in `hi` (quad = (y / 2) · 4 + x / 2 within the 8×8 group). Two scalars rather than an
// array: workgroup arrays carry an ArrayStride that newer SPIR-V validators reject.
var<workgroup> wg_trace_lo: atomic<u32>;
var<workgroup> wg_trace_hi: atomic<u32>;
// The cost bound's block rate (ADR-2557): which pixels of the group's four 4×4 blocks are block
// rate, bit (block % 2) · 16 + (y % 4) · 4 + x % 4, blocks 0 and 1 in `lo`, 2 and 3 in `hi`
// (block = (y / 4) · 2 + x / 4 within the 8×8 group).
var<workgroup> wg_block_lo: atomic<u32>;
var<workgroup> wg_block_hi: atomic<u32>;

#ifdef SSSR_STATS
// The cost probe (ADR-2557): per-class pixel counts and the rays before and after the tile budget.
@group(0) @binding(10) var<storage, read_write> cost: SssrCost;
var<workgroup> wg_mirror: atomic<u32>;
var<workgroup> wg_skipped: atomic<u32>;
var<workgroup> wg_variance: atomic<u32>;
var<workgroup> wg_own: atomic<u32>;
#endif

fn depth_at(p: vec2<i32>, size: vec2<i32>) -> f32 {
    return textureLoad(depth_tex, clamp(p, vec2<i32>(0), size - vec2<i32>(1)), 0).r;
}

fn world_at(p: vec2<i32>, z: f32) -> vec3<f32> {
    let uv = (vec2<f32>(p) + vec2<f32>(0.5)) * sssr.screen.zw;
    return world_from_uvz(uv, z);
}

// Normal from the depth buffer: per axis, the neighbour whose depth is closer to the centre's, so a
// silhouette does not bend the normal of the surface in front of it.
fn depth_normal(p: vec2<i32>, size: vec2<i32>, z: f32) -> vec3<f32> {
    let c = world_at(p, z);
    let zl = depth_at(p - vec2<i32>(1, 0), size);
    let zr = depth_at(p + vec2<i32>(1, 0), size);
    let zu = depth_at(p - vec2<i32>(0, 1), size);
    let zd = depth_at(p + vec2<i32>(0, 1), size);
    var dx: vec3<f32>;
    if abs(zr - z) < abs(zl - z) {
        dx = world_at(p + vec2<i32>(1, 0), zr) - c;
    } else {
        dx = c - world_at(p - vec2<i32>(1, 0), zl);
    }
    var dy: vec3<f32>;
    if abs(zd - z) < abs(zu - z) {
        dy = world_at(p + vec2<i32>(0, 1), zd) - c;
    } else {
        dy = c - world_at(p - vec2<i32>(0, 1), zu);
    }
    var n = cross(dy, dx);
    let len = length(n);
    if len < 1e-12 {
        return normalize(sssr.camera.xyz - c);
    }
    n = n / len;
    // Face the camera.
    if dot(n, sssr.camera.xyz - c) < 0.0 {
        n = -n;
    }
    return n;
}

// The cost bound's block rate (ADR-2557): the bit (0..15, (y % 4) · 4 + x % 4) of the pixel of
// `mask` (a 4×4 block's block-rate pixels, not empty) that traces this frame. The start slot steps
// by 11 (coprime with 16) each frame and is offset per block, so over 16 frames every pixel of a
// full block traces once and the temporal resolve accumulates them.
fn block_lead(mask: u32, block: vec2<u32>) -> u32 {
    let slot = (u32(sssr.camera.w) * 11u + block.x * 5u + block.y * 3u) % 16u;
    let rotated = ((mask >> slot) | (mask << (16u - slot))) & 0xffffu;
    return (countTrailingZeros(rotated) + slot) % 16u;
}

fn base_ray(p: vec2<u32>) -> bool {
    // FidelityFX samples-per-quad 1: one ray per 2×2 quad (mirrors always trace).
    return ((p.x & 1u) | (p.y & 1u)) == 0u;
}

// Ray list entry: one per 2×2 quad with a ray: quad x (14 bits), quad y (13 bits), and the 4-bit
// mask of the quad's pixels that trace their own ray (bit (x & 1) + 2·(y & 1)). The intersect
// copies the even-even pixel's ray to the quad's glossy pixels without one (FidelityFX's quad
// sharing); it reads that from the surface records, so the entry needs no copy bits.
fn pack_quad(q: vec2<u32>, mask: u32) -> u32 {
    return (q.x & 16383u) | ((q.y & 8191u) << 14u) | (mask << 27u);
}

@compute @workgroup_size(8, 8, 1)
fn classify(
    @builtin(global_invocation_id) gid: vec3<u32>,
    @builtin(local_invocation_index) li: u32,
    @builtin(workgroup_id) wg: vec3<u32>,
) {
    if li == 0u {
        atomicStore(&wg_rays, 0u);
        atomicStore(&wg_glossy, 0u);
        atomicStore(&wg_trace_lo, 0u);
        atomicStore(&wg_trace_hi, 0u);
        atomicStore(&wg_block_lo, 0u);
        atomicStore(&wg_block_hi, 0u);
#ifdef SSSR_STATS
        atomicStore(&wg_mirror, 0u);
        atomicStore(&wg_skipped, 0u);
        atomicStore(&wg_variance, 0u);
        atomicStore(&wg_own, 0u);
#endif
    }
    workgroupBarrier();
    let size = vec2<i32>(i32(sssr.screen.x), i32(sssr.screen.y));
    let p = vec2<i32>(gid.xy);
    let inside = all(p < size);
    var needs_ray = false;
    var glossy_reflective = false;
    var blocky = false;
    if inside {
        let z = depth_at(p, size);
        let code = decode_alpha(textureLoad(hdr_tex, p, 0).a);
        let reflective = code.reflective && z < 1.0;
        var n = vec3<f32>(0.0, 1.0, 0.0);
        if z < 1.0 {
            n = depth_normal(p, size, z);
        }
        // Roughness < 0 marks a pixel without a reflection (sky, rough, unpacked) for the
        // denoiser and the quad copy.
        textureStore(normal_out, p, vec4<u32>(pack_surface(n, select(-1.0, code.roughness, reflective)), 0u, 0u, 0u));
        glossy_reflective = reflective && is_glossy(code.roughness);
        let needs_denoiser = glossy_reflective && !is_mirror(code.roughness);
        // A block-rate pixel's ray is decided below, once the group knows its block.
        blocky = glossy_reflective && block_rate(code.roughness);
        needs_ray = glossy_reflective && !blocky && (!needs_denoiser || base_ray(gid.xy));
        if needs_denoiser && !blocky && !needs_ray && sssr.temporal.w > 0.5 && sssr.budget.z < 0.5 {
            // Temporal-variance-guided tracing: a noisy history buys this pixel its own ray.
            needs_ray = unpack_pair(textureLoad(variance_history, p, 0).r).x > sssr.temporal.y;
#ifdef SSSR_STATS
            if needs_ray {
                atomicAdd(&wg_variance, 1u);
            }
#endif
        }
        textureStore(radiance_out, p, vec4<f32>(0.0));
#ifdef SSSR_STATS
        if glossy_reflective && is_mirror(code.roughness) {
            atomicAdd(&wg_mirror, 1u);
        }
        if !glossy_reflective {
            atomicAdd(&wg_skipped, 1u);
        }
#endif
    }
    let lx = li % 8u;
    let ly = li / 8u;
    let quad = (ly / 2u) * 4u + lx / 2u;
    let bit = quad * 4u + (lx & 1u) + 2u * (ly & 1u);
    let block = (ly / 4u) * 2u + lx / 4u;
    let block_bit = (ly % 4u) * 4u + (lx % 4u);
    if blocky {
        if block < 2u {
            atomicOr(&wg_block_lo, 1u << ((block % 2u) * 16u + block_bit));
        } else {
            atomicOr(&wg_block_hi, 1u << ((block % 2u) * 16u + block_bit));
        }
    }
    workgroupBarrier();
    // This pixel's 4×4 block: its block-rate pixels (16 bits).
    let block_mask = (select(atomicLoad(&wg_block_hi), atomicLoad(&wg_block_lo), block < 2u) >> ((block % 2u) * 16u)) & 0xffffu;
    if blocky {
        // One block-rate pixel of the block traces (`block_lead`); the intersect shares its ray.
        needs_ray = block_lead(block_mask, gid.xy / 4u) == block_bit;
    }
    if needs_ray {
        if bit < 32u {
            atomicOr(&wg_trace_lo, 1u << bit);
        } else {
            atomicOr(&wg_trace_hi, 1u << (bit - 32u));
        }
    }
    if glossy_reflective {
        atomicAdd(&wg_glossy, 1u);
    }
    workgroupBarrier();
    let base = base_ray(gid.xy);
    var mask = 0u;
    if base {
        let lo = atomicLoad(&wg_trace_lo);
        let hi = atomicLoad(&wg_trace_hi);
        let bits = select(hi, lo, quad < 8u);
        mask = (bits >> ((quad % 8u) * 4u)) & 15u;
        // The cost bound's tile budget (ADR-2557): a tile whose pixels ask for more own rays than
        // the budget traces one ray per quad (its first traced pixel), shared with the quad's other
        // glossy pixels. Every thread sees the same tile total, so the choice is deterministic.
        if cost_bound_on() && sssr.bound.y > 0.0 && f32(countOneBits(lo) + countOneBits(hi)) > sssr.bound.y {
            var keep = mask & (~mask + 1u);
            // A block lead in this quad keeps its ray too: its whole block shares it.
            if block_mask != 0u {
                let lead = block_lead(block_mask, gid.xy / 4u);
                let at = vec2<u32>(lead % 4u, lead / 4u);
                if all(at / 2u == vec2<u32>((lx % 4u) / 2u, (ly % 4u) / 2u)) {
                    keep = keep | (1u << ((at.x & 1u) + 2u * (at.y & 1u)));
                }
            }
            mask = mask & keep;
        }
#ifdef SSSR_STATS
        atomicAdd(&wg_own, countOneBits(mask));
#endif
    }
    var local = 0u;
    if mask != 0u {
        local = atomicAdd(&wg_rays, 1u);
    }
    workgroupBarrier();
    if li == 0u {
        let count = atomicLoad(&wg_rays);
        wg_base = 0u;
        if count > 0u {
            wg_base = atomicAdd(&counters.rays, count);
        }
        if atomicLoad(&wg_glossy) > 0u {
            let tile = atomicAdd(&counters.tiles, 1u);
            tile_list[tile] = (wg.x * 8u) | ((wg.y * 8u) << 16u);
        }
#ifdef SSSR_STATS
        let mirror = atomicLoad(&wg_mirror);
        let wanted = countOneBits(atomicLoad(&wg_trace_lo)) + countOneBits(atomicLoad(&wg_trace_hi));
        let own = atomicLoad(&wg_own);
        atomicAdd(&cost.mirror_pixels, mirror);
        atomicAdd(&cost.glossy_pixels, atomicLoad(&wg_glossy) - mirror);
        atomicAdd(&cost.skipped_pixels, atomicLoad(&wg_skipped));
        atomicAdd(&cost.variance_rays, atomicLoad(&wg_variance));
        atomicAdd(&cost.rays, own);
        atomicAdd(&cost.budget_dropped_rays, wanted - min(own, wanted));
#endif
    }
    workgroupBarrier();
    if mask != 0u {
        ray_list[wg_base + local] = pack_quad(gid.xy / 2u, mask);
    }
}

// Indirect dispatch sizes: 64 quad entries per intersect group, one denoiser group per tile. Rows of
// GROUP_ROW groups keep every dimension under the 65535 limit at 4K.
@group(0) @binding(9) var<storage, read_write> args: array<u32, 6>;

const GROUP_ROW: u32 = 32768u;

@compute @workgroup_size(1, 1, 1)
fn prepare_args() {
    let ray_groups = (atomicLoad(&counters.rays) + 63u) / 64u;
    let tiles = atomicLoad(&counters.tiles);
    args[0] = min(ray_groups, GROUP_ROW);
    args[1] = (ray_groups + GROUP_ROW - 1u) / GROUP_ROW;
    args[2] = 1u;
    args[3] = min(tiles, GROUP_ROW);
    args[4] = (tiles + GROUP_ROW - 1u) / GROUP_ROW;
    args[5] = 1u;
}
