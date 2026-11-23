// SPDX-License-Identifier: MIT AND (MIT OR Apache-2.0)
// Portions ported from the AMD FidelityFX SDK (https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK,
// tag v1.1.4, commit c6efa6bf7f), sdk/include/FidelityFX/gpu/spd/ffx_spd.h,
// Copyright (C) 2024 Advanced Micro Devices, Inc., MIT licence.
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063564),
// crates/bevy_core_pipeline/src/mip_generation/experimental/downsample_depth.wesl,
// Copyright (c) Bevy contributors, MIT OR Apache-2.0, used under the MIT licence.
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

// Single-pass depth pyramid (SPD): one dispatch reduces 64×64 tiles of a source level into the
// next six levels through workgroup memory, instead of one dispatch (and one barrier) per mip.
//
// Shader defs: `SPD_MIN` reduces with min (nearest standard-Z depth, for ray marching); without it
// the reduction is max (farthest depth, for occlusion culling). `SPD_FROM_DEPTH` reads the depth
// buffer and also copies it into level 0; without it the source is level `6p` of the pyramid.
//
// Changes for Regia Engine: no `globallycoherent` atomic counter (levels 7+ run as a second dispatch, as
// Bevy does); no wave intrinsics (workgroup memory only, so every backend runs one code path); every
// child coordinate past the end of its level is replaced by its in-range sibling, which is exactly
// the clamp of the per-mip `hiz_down.wgsl`, so a floor-sized wgpu mip chain gets the same texels.

struct SpdParams {
    // x = levels written after the source (1..=6).
    levels: vec4<u32>,
}

@group(0) @binding(0) var<uniform> params: SpdParams;
#ifdef SPD_FROM_DEPTH
@group(0) @binding(1) var src_depth: texture_depth_2d;
@group(0) @binding(2) var dst0: texture_storage_2d<r32float, write>;
#else
@group(0) @binding(1) var src_mip: texture_2d<f32>;
#endif
@group(0) @binding(3) var dst1: texture_storage_2d<r32float, write>;
@group(0) @binding(4) var dst2: texture_storage_2d<r32float, write>;
@group(0) @binding(5) var dst3: texture_storage_2d<r32float, write>;
@group(0) @binding(6) var dst4: texture_storage_2d<r32float, write>;
@group(0) @binding(7) var dst5: texture_storage_2d<r32float, write>;
@group(0) @binding(8) var dst6: texture_storage_2d<r32float, write>;

// Level 1 of a tile is 32×32; levels 2–6 ping-pong between the two arrays.
var<workgroup> lds_a: array<f32, 1024>;
var<workgroup> lds_b: array<f32, 256>;

fn reduce2(a: f32, b: f32) -> f32 {
#ifdef SPD_MIN
    return min(a, b);
#else
    return max(a, b);
#endif
}

fn reduce4(a: f32, b: f32, c: f32, d: f32) -> f32 {
    return reduce2(reduce2(a, b), reduce2(c, d));
}

fn src_size() -> vec2<i32> {
#ifdef SPD_FROM_DEPTH
    return vec2<i32>(textureDimensions(src_depth));
#else
    return vec2<i32>(textureDimensions(src_mip));
#endif
}

fn load_src(c: vec2<i32>, size: vec2<i32>) -> f32 {
    let q = clamp(c, vec2<i32>(0), size - vec2<i32>(1));
#ifdef SPD_FROM_DEPTH
    return textureLoad(src_depth, q, 0);
#else
    return textureLoad(src_mip, q, 0).r;
#endif
}

fn level_size(level: u32) -> vec2<i32> {
    switch level {
        case 1u: { return vec2<i32>(textureDimensions(dst1)); }
        case 2u: { return vec2<i32>(textureDimensions(dst2)); }
        case 3u: { return vec2<i32>(textureDimensions(dst3)); }
        case 4u: { return vec2<i32>(textureDimensions(dst4)); }
        case 5u: { return vec2<i32>(textureDimensions(dst5)); }
        default: { return vec2<i32>(textureDimensions(dst6)); }
    }
}

fn store_level(level: u32, p: vec2<i32>, v: f32) {
    if any(p >= level_size(level)) {
        return;
    }
    let value = vec4<f32>(v, 0.0, 0.0, 1.0);
    switch level {
        case 1u: { textureStore(dst1, p, value); }
        case 2u: { textureStore(dst2, p, value); }
        case 3u: { textureStore(dst3, p, value); }
        case 4u: { textureStore(dst4, p, value); }
        case 5u: { textureStore(dst5, p, value); }
        default: { textureStore(dst6, p, value); }
    }
}

// The second child along an axis, or the first when the second is past the level's end.
fn child1(origin: i32, local2: i32, size: i32) -> i32 {
    return select(local2 + 1, local2, origin + local2 + 1 >= size);
}

fn lds_read(level: u32, i: u32) -> f32 {
    // Odd levels live in `lds_a`, even levels in `lds_b`.
    if (level & 1u) == 1u {
        return lds_a[i];
    }
    return lds_b[i];
}

fn lds_write(level: u32, i: u32, v: f32) {
    if (level & 1u) == 1u {
        lds_a[i] = v;
    } else {
        lds_b[i] = v;
    }
}

@compute @workgroup_size(256, 1, 1)
fn spd(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_index) li: u32) {
    let levels = params.levels.x;
    let ssize = src_size();
    let tile = vec2<i32>(wg.xy);
    // Level 1: 32×32 texels per tile, four per thread, each from a 2×2 of the source.
    let origin0 = tile * 64;
    for (var k = 0u; k < 4u; k = k + 1u) {
        let idx = li + k * 256u;
        let l = vec2<i32>(i32(idx % 32u), i32(idx / 32u));
        let c = l * 2;
        let x1 = child1(origin0.x, c.x, ssize.x);
        let y1 = child1(origin0.y, c.y, ssize.y);
        let a = load_src(origin0 + c, ssize);
        let b = load_src(origin0 + vec2<i32>(x1, c.y), ssize);
        let d = load_src(origin0 + vec2<i32>(c.x, y1), ssize);
        let e = load_src(origin0 + vec2<i32>(x1, y1), ssize);
#ifdef SPD_FROM_DEPTH
        // Level 0 is a copy of the depth buffer (only the texels that exist).
        let g = origin0 + c;
        if all(g < ssize) {
            textureStore(dst0, g, vec4<f32>(a, 0.0, 0.0, 1.0));
        }
        if all(g + vec2<i32>(1, 0) < ssize) {
            textureStore(dst0, g + vec2<i32>(1, 0), vec4<f32>(b, 0.0, 0.0, 1.0));
        }
        if all(g + vec2<i32>(0, 1) < ssize) {
            textureStore(dst0, g + vec2<i32>(0, 1), vec4<f32>(d, 0.0, 0.0, 1.0));
        }
        if all(g + vec2<i32>(1, 1) < ssize) {
            textureStore(dst0, g + vec2<i32>(1, 1), vec4<f32>(e, 0.0, 0.0, 1.0));
        }
#endif
        let v = reduce4(a, b, d, e);
        store_level(1u, tile * 32 + l, v);
        lds_a[idx] = v;
    }
    // Levels 2..=6 from workgroup memory. `levels` is uniform, so every barrier is too.
    for (var level = 2u; level <= 6u; level = level + 1u) {
        if level > levels {
            break;
        }
        workgroupBarrier();
        let n_prev = 64u >> (level - 1u);
        let n = n_prev / 2u;
        let prev_size = level_size(level - 1u);
        let prev_origin = tile * i32(n_prev);
        var v = 0.0;
        let working = li < n * n;
        if working {
            let l = vec2<i32>(i32(li % n), i32(li / n));
            let c = l * 2;
            let x1 = child1(prev_origin.x, c.x, prev_size.x);
            let y1 = child1(prev_origin.y, c.y, prev_size.y);
            let a = lds_read(level - 1u, u32(c.y) * n_prev + u32(c.x));
            let b = lds_read(level - 1u, u32(c.y) * n_prev + u32(x1));
            let d = lds_read(level - 1u, u32(y1) * n_prev + u32(c.x));
            let e = lds_read(level - 1u, u32(y1) * n_prev + u32(x1));
            v = reduce4(a, b, d, e);
            store_level(level, tile * i32(n) + l, v);
        }
        // Reads of level − 1 finish before level + 1 overwrites that array.
        workgroupBarrier();
        if working {
            lds_write(level, li, v);
        }
    }
}
