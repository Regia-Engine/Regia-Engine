// SPDX-License-Identifier: MPL-2.0
// Maximum-height pyramid for the probe ray tracer (Tevs, Ihrke, Seidel, I3D 2008).
// Level 0 texel (i, j) is the highest of the four samples around heightfield cell (i, j);
// level k is the highest of its 2×2 children. Heights are world metres.
//
// Each dispatch re-reduces a list of texel rectangles of one level (ADR-7659): workgroup row z
// takes `region.rects[z]` (x, y = first texel, z, w = end, exclusive). A whole-level rebuild
// is the one rectangle covering the level, so both paths run the same per-texel arithmetic.

struct Heightfield {
    map: vec4<f32>,
    size: vec4<u32>,
    albedo: vec4<f32>,
}

struct Region {
    rects: array<vec4<u32>, 16>,
}

@group(0) @binding(0) var<uniform> hf: Heightfield;
@group(0) @binding(1) var src_heights: texture_2d<f32>;
@group(0) @binding(2) var dst: texture_storage_2d<r32float, write>;
@group(0) @binding(3) var<uniform> region: Region;

// The texel this invocation writes, or (-1, -1) when it is outside its rectangle or the level.
fn region_texel(gid: vec3<u32>) -> vec2<i32> {
    let r = region.rects[min(gid.z, 15u)];
    let t = r.xy + gid.xy;
    let size = textureDimensions(dst);
    if (t.x >= r.z || t.y >= r.w || t.x >= size.x || t.y >= size.y) {
        return vec2<i32>(-1);
    }
    return vec2<i32>(t);
}

@compute @workgroup_size(8, 8)
fn build_level0(@builtin(global_invocation_id) gid: vec3<u32>) {
    let c = region_texel(gid);
    if (c.x < 0) {
        return;
    }
    let last = vec2<i32>(i32(hf.size.x) - 1);
    let a = textureLoad(src_heights, min(c, last), 0).r;
    let b = textureLoad(src_heights, min(c + vec2<i32>(1, 0), last), 0).r;
    let d = textureLoad(src_heights, min(c + vec2<i32>(0, 1), last), 0).r;
    let e = textureLoad(src_heights, min(c + vec2<i32>(1, 1), last), 0).r;
    let h = max(max(a, b), max(d, e)) * hf.map.z + hf.map.w;
    textureStore(dst, c, vec4<f32>(h, 0.0, 0.0, 0.0));
}

// `src_heights` is the previous pyramid level here (world metres).
@compute @workgroup_size(8, 8)
fn build_level(@builtin(global_invocation_id) gid: vec3<u32>) {
    let t = region_texel(gid);
    if (t.x < 0) {
        return;
    }
    let last = vec2<i32>(textureDimensions(src_heights)) - vec2<i32>(1);
    let c = t * 2;
    let a = textureLoad(src_heights, min(c, last), 0).r;
    let b = textureLoad(src_heights, min(c + vec2<i32>(1, 0), last), 0).r;
    let d = textureLoad(src_heights, min(c + vec2<i32>(0, 1), last), 0).r;
    let e = textureLoad(src_heights, min(c + vec2<i32>(1, 1), last), 0).r;
    textureStore(dst, t, vec4<f32>(max(max(a, b), max(d, e)), 0.0, 0.0, 0.0));
}
