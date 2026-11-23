// SPDX-License-Identifier: MPL-2.0
// Hi-Z pyramid for the visibility-buffer cull (Greene, Kass & Miller, "Hierarchical Z-Buffer
// Visibility", SIGGRAPH 1993). Standard Z, so a coarser texel keeps the farthest depth.
// Mip 0 is the next power of two over the depth target; texels past the screen hold the far
// plane, so every mip texel covers exactly 2^mip pixels and odd edges are not dropped.

@group(0) @binding(0) var src_depth: texture_depth_2d;
@group(0) @binding(1) var src_mip: texture_2d<f32>;
@group(0) @binding(2) var dst: texture_storage_2d<r32float, write>;

@compute @workgroup_size(8, 8)
fn copy_depth(@builtin(global_invocation_id) gid: vec3<u32>) {
    let size = textureDimensions(dst);
    if (gid.x >= size.x || gid.y >= size.y) {
        return;
    }
    let depth_size = textureDimensions(src_depth);
    var d = 1.0;
    if (gid.x < depth_size.x && gid.y < depth_size.y) {
        d = textureLoad(src_depth, vec2<i32>(gid.xy), 0);
    }
    textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(d, 0.0, 0.0, 1.0));
}

@compute @workgroup_size(8, 8)
fn downsample(@builtin(global_invocation_id) gid: vec3<u32>) {
    let size = textureDimensions(dst);
    if (gid.x >= size.x || gid.y >= size.y) {
        return;
    }
    let base = vec2<i32>(gid.xy) * 2;
    let farthest = max(
        max(textureLoad(src_mip, base, 0).r, textureLoad(src_mip, base + vec2<i32>(1, 0), 0).r),
        max(textureLoad(src_mip, base + vec2<i32>(0, 1), 0).r, textureLoad(src_mip, base + vec2<i32>(1, 1), 0).r),
    );
    textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(farthest, 0.0, 0.0, 1.0));
}
