// SPDX-License-Identifier: MPL-2.0
// The per-eye composite after the tonemap (spec 58 §§ 2.6, 2.8): the comfort vignette darkens by the
// angle off the view axis, in radians of field of view, and the hidden area is painted black.

struct Params {
    strength: f32,
    inner: f32,
    outer: f32,
    pad: f32,
    // Tangents of the left, right, down and up edges of the eye's frustum.
    tangents: vec4<f32>,
}

@group(0) @binding(0) var<uniform> params: Params;

struct FullScreen {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_full(@builtin(vertex_index) index: u32) -> FullScreen {
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    // A constructor, not a struct `var` (ADR-026).
    return FullScreen(vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0), uv);
}

@fragment
fn fs_vignette(in: FullScreen) -> @location(0) vec4<f32> {
    let tangent = vec2<f32>(
        mix(params.tangents.x, params.tangents.y, in.uv.x),
        mix(params.tangents.w, params.tangents.z, in.uv.y),
    );
    let angle = atan(length(tangent));
    let t = smoothstep(params.inner, params.outer, angle);
    return vec4<f32>(0.0, 0.0, 0.0, params.strength * t);
}

@vertex
fn vs_hidden(@location(0) position: vec4<f32>) -> @builtin(position) vec4<f32> {
    return position;
}

@fragment
fn fs_hidden() -> @location(0) vec4<f32> {
    return vec4<f32>(0.0, 0.0, 0.0, 1.0);
}
