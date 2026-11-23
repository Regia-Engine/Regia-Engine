// SPDX-License-Identifier: MPL-2.0
// The optional soft contact shadow: a radial falloff over the piece's footprint on a plane behind it, drawn
// in the same forward pass after the piece (the depth test skips what the piece covers). Black with a soft
// alpha that reaches zero inside the quad, so the quad's edge never shows.

struct ShadowOut {
    @builtin(position) clip: vec4<f32>,
    // Offset from the footprint's center in mm.
    @location(0) offset: vec2<f32>,
}

@vertex
fn vs_shadow(@builtin(vertex_index) index: u32) -> ShadowOut {
    let corner = vec2<f32>(f32(index & 1u) * 2.0 - 1.0, f32((index >> 1u) & 1u) * 2.0 - 1.0);
    let reach = frame.shadow_extent.xy + vec2<f32>(frame.shadow_center.w * 2.0);
    let offset = corner * reach;
    // Built with the constructor, not a `var` (naga gives every struct an explicit layout, which
    // VUID-StandaloneSpirv-None-10684 forbids in Function storage; ADR-7587).
    return ShadowOut(frame.view_proj * vec4<f32>(frame.shadow_center.xyz + vec3<f32>(offset, 0.0), 1.0), offset);
}

@fragment
fn fs_shadow(in: ShadowOut) -> @location(0) vec4<f32> {
    let reach = frame.shadow_extent.xy + vec2<f32>(frame.shadow_center.w * 2.0);
    // 0 at the footprint's center, 1 where the shadow ends.
    let r = length(in.offset / max(reach, vec2<f32>(0.001)));
    let t = clamp(r, 0.0, 1.0);
    let falloff = 1.0 - t * t * (3.0 - 2.0 * t);
    var alpha = falloff * frame.env.z;
    // A target without hardware sRGB holds encoded values, so the blend scales an encoded backdrop: darken it by
    // (1 - alpha) ^ (1 / 2.2) instead, which encodes the linear darkening to within a step of 8-bit on light
    // backdrops (the sRGB curve is close to that power there), so the shadow matches an sRGB target's.
    if frame.env.y > 0.5 {
        alpha = 1.0 - pow(1.0 - alpha, 1.0 / 2.2);
    }
    return vec4<f32>(0.0, 0.0, 0.0, alpha);
}
