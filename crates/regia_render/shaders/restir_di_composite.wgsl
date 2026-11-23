// SPDX-License-Identifier: MPL-2.0
// The composite of the many-light sampler's evaluation path (ADR-2470): the sampler's output
// (linear radiance) times the frame's pre-exposure over the view rectangle of the HDR target.

struct DebugParams {
    world_from_clip: mat4x4<f32>,
    prev_clip_from_world: mat4x4<f32>,
    // eye xyz, pre-exposure
    eye: vec4<f32>,
    // forward xyz, 1 when the previous matrix is valid
    forward: vec4<f32>,
    // viewport x, y, w, h in color pixels
    viewport: vec4<f32>,
    // color target width, height; output width, height
    sizes: vec4<f32>,
    // flat albedo rgb, roughness
    material: vec4<f32>,
}

@group(0) @binding(0) var<uniform> cp: DebugParams;
@group(0) @binding(1) var sampled: texture_2d<f32>;

@vertex
fn composite_vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

@fragment
fn composite_fs(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let local = frag.xy - cp.viewport.xy;
    let p = vec2<i32>(clamp(floor(local / cp.viewport.zw * cp.sizes.zw), vec2<f32>(0.0), cp.sizes.zw - vec2<f32>(1.0)));
    let c = textureLoad(sampled, p, 0);
    return vec4<f32>(c.rgb * cp.eye.w, 1.0);
}
