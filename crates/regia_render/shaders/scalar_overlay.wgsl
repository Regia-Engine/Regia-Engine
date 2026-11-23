// SPDX-License-Identifier: MPL-2.0
// Per-vertex scalar overlay (`crate::scalar_overlay`): a mesh coloured by one scalar per vertex
// through a ramp, drawn unlit over the final image. Regia Engine-original.
//
// The vertex stage skins with the entity's own joint palette and applies its model matrix. The
// scene depth is not an attachment: like the gizmos, the fragment stage loads it and discards
// occluded fragments, so one path serves every render scale, viewport and MSAA setting.

#import regia::mesh

struct OverlayView {
    // Unjittered view-projection of the frame (standard depth 0..1).
    clip_from_world: mat4x4<f32>,
    // Viewport rectangle in target pixels: x, y, width, height.
    viewport: vec4<f32>,
    // x: the target is sRGB, so the (display-referred) ramp colours are decoded to linear.
    flags: vec4<u32>,
}

struct OverlayObject {
    model: mat4x4<f32>,
    // x = range start, y = range end, z = opacity.
    params: vec4<f32>,
    // The ramp: 64 colours, evenly spaced over 0..1.
    lut: array<vec4<f32>, 64>,
}

@group(0) @binding(0) var<uniform> view: OverlayView;
#ifdef DEPTH_MULTISAMPLED
@group(0) @binding(1) var scene_depth: texture_depth_multisampled_2d;
#else
@group(0) @binding(1) var scene_depth: texture_depth_2d;
#endif
@group(1) @binding(0) var<uniform> object: OverlayObject;
@group(1) @binding(1) var<storage, read> joints: array<regia::mesh::Mat3x4>;

// Slack of the scene-depth test (standard Z) on top of twice the pixel's depth slope, so the
// mesh's own surface (jittered or differently rounded depth) is never taken for an occluder.
const DEPTH_SLACK: f32 = 1.0e-5;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(5) joint_ids: vec4<u32>,
    @location(6) weights: vec4<f32>,
    @location(7) value: f32,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) value: f32,
}

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    // Out-of-range joint ids clamp to the last joint instead of reading past the palette.
    let last = max(arrayLength(&joints), 1u) - 1u;
    let p = regia::mesh::skin_position(
        in.position,
        joints[min(in.joint_ids.x, last)],
        joints[min(in.joint_ids.y, last)],
        joints[min(in.joint_ids.z, last)],
        joints[min(in.joint_ids.w, last)],
        in.weights,
    );
    let world = object.model * vec4<f32>(p, 1.0);
    return VertexOutput(view.clip_from_world * world, in.value);
}

fn ramp_color(t: f32) -> vec3<f32> {
    let x = clamp(t, 0.0, 1.0) * 63.0;
    let i0 = u32(floor(x));
    let i1 = min(i0 + 1u, 63u);
    return mix(object.lut[i0].rgb, object.lut[i1].rgb, x - floor(x));
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // Derivatives first: they need uniform control flow, the discard below does not keep it.
    let slack = DEPTH_SLACK + 2.0 * fwidth(in.clip_position.z);
    let dims = vec2<i32>(textureDimensions(scene_depth));
    let uv = (in.clip_position.xy - view.viewport.xy) / view.viewport.zw;
    let texel = clamp(vec2<i32>(uv * vec2<f32>(dims)), vec2<i32>(0), dims - vec2<i32>(1));
    let scene = textureLoad(scene_depth, texel, 0);
    if in.clip_position.z > scene + slack {
        discard;
    }
    let lo = object.params.x;
    let hi = object.params.y;
    // Same mapping as `ramp_position`: linear and clamped, a step when the range is empty.
    var t = select(0.0, 1.0, in.value >= hi);
    if hi != lo {
        t = clamp((in.value - lo) / (hi - lo), 0.0, 1.0);
    }
    var color = ramp_color(t);
    if view.flags.x != 0u {
        color = srgb_to_linear(color);
    }
    return vec4<f32>(color, object.params.z);
}
