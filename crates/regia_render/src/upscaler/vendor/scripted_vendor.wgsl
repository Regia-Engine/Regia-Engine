// SPDX-License-Identifier: MPL-2.0
// The scripted vendor binding's two compute passes (ADR-2679, upscaler/vendor/scripted.rs).
//
// cs_capture copies what the binding was handed (color, motion, depth, reactive mask) into a
// storage buffer the conformance test reads back. cs_upscale writes the binding's known filter:
// the nearest render texel of each display pixel, times a gain. Test tooling: no frame runs either
// unless a test installs the binding.

struct Sizes {
    render: vec2<u32>,
    display: vec2<u32>,
    gain: f32,
    has_reactive: u32,
    pad: vec2<u32>,
}

struct Texel {
    color: vec4<f32>,
    motion: vec2<f32>,
    depth: f32,
    reactive: f32,
}

@group(0) @binding(0) var<uniform> sizes: Sizes;
@group(0) @binding(1) var in_color: texture_2d<f32>;
@group(0) @binding(2) var in_depth: texture_depth_2d;
@group(0) @binding(3) var in_motion: texture_2d<f32>;
@group(0) @binding(4) var in_reactive: texture_2d<f32>;
@group(0) @binding(5) var<storage, read_write> out_capture: array<Texel>;
@group(0) @binding(6) var out_image: texture_storage_2d<rgba16float, write>;

@compute @workgroup_size(8, 8)
fn cs_capture(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= sizes.render.x || id.y >= sizes.render.y) {
        return;
    }
    let texel = vec2<i32>(id.xy);
    let index = id.y * sizes.render.x + id.x;
    out_capture[index].color = textureLoad(in_color, texel, 0);
    out_capture[index].motion = textureLoad(in_motion, texel, 0).xy;
    out_capture[index].depth = textureLoad(in_depth, texel, 0);
    if (sizes.has_reactive != 0u) {
        out_capture[index].reactive = textureLoad(in_reactive, texel, 0).r;
    } else {
        out_capture[index].reactive = 0.0;
    }
}

@compute @workgroup_size(8, 8)
fn cs_upscale(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= sizes.display.x || id.y >= sizes.display.y) {
        return;
    }
    let source = id.xy * sizes.render / sizes.display;
    let color = textureLoad(in_color, vec2<i32>(source), 0);
    textureStore(out_image, vec2<i32>(id.xy), vec4<f32>(color.rgb * sizes.gain, color.a));
}
