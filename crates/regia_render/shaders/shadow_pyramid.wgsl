// SPDX-License-Identifier: MPL-2.0
// The coarse farthest-depth pyramid of a shadow map (`crate::shadow_occlusion`, ADR-2072).
//
// `cs_base` keeps the farthest depth of each `block`×`block` square of the map in level 0 (the
// pyramid is coarse: a 4096² cascade has a 512² base). Base texels past the map's edge cover no
// texel and store 0, which leaves the maximum of every coarser texel to the texels it covers.
// `cs_down` builds each further level from the one below (2×2, and the third row or column of
// an odd source on the last texel). With `block` 1 `cs_base` copies the map (test snapshots).

struct Reduce {
    // Map size in texels.
    src_size: vec2<u32>,
    // Map texels per base texel along each axis.
    block: u32,
    _pad: u32,
}

// `cs_base` binds 0, 1 and 2; `cs_down` binds 1 and 3 (each pipeline's layout names only the
// bindings its entry point reads).
@group(0) @binding(0) var src_depth: texture_depth_2d;
@group(0) @binding(1) var dst: texture_storage_2d<r32float, write>;
@group(0) @binding(2) var<uniform> reduce: Reduce;
@group(0) @binding(3) var src_level: texture_2d<f32>;

@compute @workgroup_size(8, 8)
fn cs_down(@builtin(global_invocation_id) id: vec3<u32>) {
    let size = textureDimensions(dst);
    if (id.x >= size.x || id.y >= size.y) {
        return;
    }
    let src = textureDimensions(src_level, 0);
    let x0 = id.x * 2u;
    let y0 = id.y * 2u;
    var x1 = min(x0 + 1u, src.x - 1u);
    var y1 = min(y0 + 1u, src.y - 1u);
    if (id.x + 1u == size.x && src.x > x1 + 1u) {
        x1 = src.x - 1u;
    }
    if (id.y + 1u == size.y && src.y > y1 + 1u) {
        y1 = src.y - 1u;
    }
    var farthest = 0.0;
    for (var y = y0; y <= y1; y = y + 1u) {
        for (var x = x0; x <= x1; x = x + 1u) {
            // i32 coordinates: naga's bounds-checked load of a u32 pair writes invalid SPIR-V (ADR-2140).
            farthest = max(farthest, textureLoad(src_level, vec2<i32>(i32(x), i32(y)), 0).x);
        }
    }
    textureStore(dst, id.xy, vec4<f32>(farthest, 0.0, 0.0, 0.0));
}

@compute @workgroup_size(8, 8)
fn cs_base(@builtin(global_invocation_id) id: vec3<u32>) {
    let size = textureDimensions(dst);
    if (id.x >= size.x || id.y >= size.y) {
        return;
    }
    let block = max(reduce.block, 1u);
    var farthest = 0.0;
    for (var dy = 0u; dy < block; dy = dy + 1u) {
        let y = id.y * block + dy;
        if (y >= reduce.src_size.y) {
            break;
        }
        for (var dx = 0u; dx < block; dx = dx + 1u) {
            let x = id.x * block + dx;
            if (x >= reduce.src_size.x) {
                break;
            }
            // i32 coordinates: naga's bounds-checked load of a u32 pair writes invalid SPIR-V (ADR-2140).
            farthest = max(farthest, textureLoad(src_depth, vec2<i32>(i32(x), i32(y)), 0));
        }
    }
    textureStore(dst, id.xy, vec4<f32>(farthest, 0.0, 0.0, 0.0));
}
