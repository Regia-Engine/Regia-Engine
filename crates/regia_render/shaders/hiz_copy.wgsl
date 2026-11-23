// SPDX-License-Identifier: MPL-2.0
// Hi-Z mip 0: copy the depth buffer into an r32float image.
// Greene, Kass & Miller, SIGGRAPH 1993. Standard-Z, so later mips take the max.

@group(0) @binding(0) var src_depth: texture_depth_2d;
@group(0) @binding(1) var dst: texture_storage_2d<r32float, write>;

@compute @workgroup_size(8, 8)
fn copy_depth(@builtin(global_invocation_id) gid: vec3<u32>) {
    let size = textureDimensions(dst);
    if (gid.x >= size.x || gid.y >= size.y) {
        return;
    }
    let d = textureLoad(src_depth, vec2<i32>(gid.xy), 0);
    textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(d, 0.0, 0.0, 1.0));
}
