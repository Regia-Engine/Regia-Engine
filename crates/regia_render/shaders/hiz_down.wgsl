// SPDX-License-Identifier: MPL-2.0
// Hi-Z coarser mip: farthest standard-Z of a 2×2 footprint.
// Greene, Kass & Miller, "Hierarchical Z-Buffer Visibility", SIGGRAPH 1993.

@group(0) @binding(0) var src_mip: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<r32float, write>;

@compute @workgroup_size(8, 8)
fn downsample(@builtin(global_invocation_id) gid: vec3<u32>) {
    let size = textureDimensions(dst);
    if (gid.x >= size.x || gid.y >= size.y) {
        return;
    }
    let src_size = vec2<i32>(textureDimensions(src_mip));
    let base = vec2<i32>(gid.xy) * 2;
    var farthest = 0.0;
    for (var y: i32 = 0; y < 2; y = y + 1) {
        for (var x: i32 = 0; x < 2; x = x + 1) {
            let c = clamp(base + vec2<i32>(x, y), vec2<i32>(0), src_size - vec2<i32>(1));
            farthest = max(farthest, textureLoad(src_mip, c, 0).r);
        }
    }
    textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(farthest, 0.0, 0.0, 1.0));
}
