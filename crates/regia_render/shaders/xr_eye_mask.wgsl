// SPDX-License-Identifier: MPL-2.0
// Hidden-area depth prefill of an XR eye (spec 58 § 2.6). The vertices are already in clip space
// with z = 0: the near plane of the frame's standard depth, so every later fragment there fails
// the depth test before it shades (Vlachos, "Advanced VR Rendering", GDC 2015).

@vertex
fn vs_main(@location(0) position: vec4<f32>) -> @builtin(position) vec4<f32> {
    return position;
}
