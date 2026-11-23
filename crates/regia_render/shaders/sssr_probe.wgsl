// SPDX-License-Identifier: MPL-2.0
// Stochastic SSR's dormant probe: while the pass sleeps (its last readbacks found no glossy
// pixel, sssr-budget lane), one pixel per 2×2 of the frame is checked for a surface code that
// would be traced, and the 8×8 groups (16×16 pixels) that saw one are counted. The renderer
// reads the count back a few frames later and wakes the full pass when it is not zero. About
// 0.01 ms at 1440p, against the full pass's classification, pyramid and composite.

#import regia::sssr::{sssr, decode_alpha, is_glossy}

struct Counters {
    rays: atomic<u32>,
    tiles: atomic<u32>,
}

@group(0) @binding(1) var hdr_tex: texture_2d<f32>;
@group(0) @binding(2) var depth_tex: texture_2d<f32>;
@group(0) @binding(7) var<storage, read_write> counters: Counters;

var<workgroup> wg_glossy: atomic<u32>;

@compute @workgroup_size(8, 8, 1)
fn probe(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(local_invocation_index) li: u32) {
    if li == 0u {
        atomicStore(&wg_glossy, 0u);
    }
    workgroupBarrier();
    let size = vec2<i32>(textureDimensions(hdr_tex));
    let p = vec2<i32>(gid.xy) * 2;
    if all(p < size) {
        let z = textureLoad(depth_tex, p, 0).r;
        let code = decode_alpha(textureLoad(hdr_tex, p, 0).a);
        if code.reflective && z < 1.0 && is_glossy(code.roughness) {
            atomicAdd(&wg_glossy, 1u);
        }
    }
    workgroupBarrier();
    if li == 0u && atomicLoad(&wg_glossy) > 0u {
        atomicAdd(&counters.tiles, 1u);
    }
}
