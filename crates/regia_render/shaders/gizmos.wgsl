// SPDX-License-Identifier: MIT OR Apache-2.0
// Gizmo lines and line-strip joints.
//
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_gizmos_render/src/{lines,line_joints}.wesl, translated from WESL to naga_oil WGSL.
// Copyright (c) Bevy contributors, dual-licensed MIT OR Apache-2.0; used here under the MIT
// licence (see docs/ported-sources.toml). The instanced screen-space line expansion follows
// Rye Terrell, "Instanced Line Rendering" (https://wwwtyro.net/2019/11/18/instanced-lines.html);
// the sub-pixel thinness fade follows Freya Holmér's Shapes docs
// (https://acegikmo.com/shapes/docs/#anti-aliasing).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//
// Differences from Bevy:
// - Regia Engine's depth is standard Z (near 0, far 1, LessEqual), Bevy's is reversed. Near-plane
//   clipping and the depth bias are rewritten for it (the bias keeps Bevy's -1..1 meaning).
// - The scene depth is not bound as a depth attachment: the fragment shader loads it and
//   discards occluded fragments. That works at any render scale, inside split-screen
//   viewports and with MSAA (DEPTH_MULTISAMPLED reads sample 0), and "on top" is a flag.
// - Strip separators are removed on the CPU (no NaN vertices reach the GPU).
// - Colours are linear; ENCODE_SRGB targets get the sRGB transfer in the fragment shader.

struct GizmoView {
    clip_from_world: mat4x4<f32>,
    view_from_clip: mat4x4<f32>,
    world_from_clip: mat4x4<f32>,
    // Viewport rectangle in target pixels: x, y, width, height.
    viewport: vec4<f32>,
    // x: encode sRGB in the shader (non-sRGB target).
    flags: vec4<u32>,
}

struct GizmoLine {
    world_from_local: mat4x4<f32>,
    line_width: f32,
    depth_bias: f32,
    joints_resolution: u32,
    on_top: u32,
    gap_scale: f32,
    line_scale: f32,
    animation_offset: f32,
    pad: f32,
}

@group(0) @binding(0) var<uniform> view: GizmoView;
#ifdef DEPTH_MULTISAMPLED
@group(0) @binding(1) var scene_depth: texture_depth_multisampled_2d;
#else
@group(0) @binding(1) var scene_depth: texture_depth_2d;
#endif
@group(1) @binding(0) var<uniform> gizmo: GizmoLine;

const EPSILON: f32 = 4.88e-04;
// Slack of the scene-depth test (standard Z), so a line drawn exactly on a surface is kept.
const DEPTH_SLACK: f32 = 1.0e-6;

// Moves `a` onto the near plane (z = 0) when it is behind it and `b` is in front, so the
// perspective divide below never sees w <= 0.
fn clip_near_plane(a: vec4<f32>, b: vec4<f32>) -> vec4<f32> {
    if a.z < 0.0 && b.z >= 0.0 {
        // Nudge past the plane so rounding cannot leave the point just behind it.
        let t = a.z / (a.z - b.z) + EPSILON;
        return mix(a, b, t);
    }
    return a;
}

// Bevy's depth bias, expressed on reversed depth r = w - z: positive scales r toward the far
// plane, negative raises it toward the near plane exponentially (-1 = always in front).
fn biased_depth(clip: vec4<f32>) -> f32 {
    let reversed = max(clip.w - clip.z, 1.0e-12);
    var r: f32;
    if gizmo.depth_bias >= 0.0 {
        r = reversed * (1.0 - gizmo.depth_bias);
    } else {
        r = reversed * exp2(-gizmo.depth_bias * log2(max(clip.w / reversed - EPSILON, 1.0e-12)));
    }
    return clip.w - r;
}

// A clip-space position no triangle covers: every vertex of a culled segment or joint lands
// here, so its triangles have zero area and lie outside the depth range (z < 0).
const CULLED: vec4<f32> = vec4<f32>(0.0, 0.0, -1.0, 1.0);

// Unit `v`, or `fallback` when `v` is too short to normalize: `normalize` of a zero vector is
// NaN, and a NaN vertex position is undefined in the rasterizer (it can spray across the view).
// A NaN or infinite length (exponent bits all ones) takes the fallback too: v / ∞ is NaN
// for an infinite component. The sign bit is shifted out, not masked: the masked form on a
// `dot` result is folded to false by the NVIDIA 610.88 Windows Vulkan compiler (ADR-3150).
fn safe_dir(v: vec2<f32>, fallback: vec2<f32>) -> vec2<f32> {
    let len2 = dot(v, v);
    if (bitcast<u32>(len2) << 1u) >= 0xff000000u || len2 <= 1.0e-12 {
        return fallback;
    }
    return v * inverseSqrt(len2);
}

fn to_screen(clip: vec4<f32>, resolution: vec2<f32>) -> vec2<f32> {
    return resolution * (0.5 * clip.xy / clip.w + 0.5);
}

fn from_screen(screen: vec2<f32>, resolution: vec2<f32>, depth: f32, w: f32) -> vec4<f32> {
    return vec4(w * ((2.0 * screen) / resolution - 1.0), depth, w);
}

// Quad and joint corners by vertex index. Bevy indexes local arrays; naga gives a local array an
// explicit ArrayStride, which SPIR-V 1.6 rejects for function storage (VUID-StandaloneSpirv-
// None-10684, see shader_check.rs), so the tables are switches.

// Segment quad: x across the line (-0.5..0.5), y along it (0 at a, 1 at b).
fn line_corner(index: u32) -> vec2<f32> {
    switch index {
        case 0u, 3u: { return vec2(-0.5, 0.0); }
        case 1u: { return vec2(-0.5, 1.0); }
        case 2u, 4u: { return vec2(0.5, 1.0); }
        default: { return vec2(0.5, 0.0); }
    }
}

fn bevel_corner(index: u32) -> vec2<f32> {
    switch index {
        case 1u: { return vec2(0.0, 0.5); }
        case 2u: { return vec2(0.5, 0.0); }
        default: { return vec2(0.0, 0.0); }
    }
}

fn miter_corner(index: u32) -> vec3<f32> {
    switch index {
        case 1u: { return vec3(0.5, 0.0, 0.0); }
        case 2u, 4u: { return vec3(0.0, 0.5, 0.0); }
        case 5u: { return vec3(0.0, 0.0, 0.5); }
        default: { return vec3(0.0, 0.0, 0.0); }
    }
}

// ---- Lines ----

struct LineInput {
    @location(0) position_a: vec3<f32>,
    @location(1) position_b: vec3<f32>,
    @location(2) color_a: vec4<f32>,
    @location(3) color_b: vec4<f32>,
    @builtin(vertex_index) index: u32,
}

struct LineOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) uv: f32,
    @location(2) line_fraction: f32,
}

@vertex
fn vertex(vertex: LineInput) -> LineOutput {
    let position = line_corner(vertex.index);
    let clip_from_local = view.clip_from_world * gizmo.world_from_local;

    var clip_a = clip_from_local * vec4(vertex.position_a, 1.0);
    var clip_b = clip_from_local * vec4(vertex.position_b, 1.0);
    clip_a = clip_near_plane(clip_a, clip_b);
    clip_b = clip_near_plane(clip_b, clip_a);
    // Both ends behind the near plane: nothing of the segment is visible, and the divide by w
    // below would mirror it through the eye (w <= 0).
    if clip_a.z < 0.0 && clip_b.z < 0.0 {
        return LineOutput(CULLED, vertex.color_a, 0.0, 1.0);
    }
    let clip = mix(clip_a, clip_b, position.y);

    let resolution = view.viewport.zw;
    let screen_a = to_screen(clip_a, resolution);
    let screen_b = to_screen(clip_b, resolution);
    // A segment seen end-on projects to one point; any width axis gives a zero-area quad.
    let y_basis = safe_dir(screen_b - screen_a, vec2(0.0, 1.0));
    let x_basis = vec2(-y_basis.y, y_basis.x);

    var color = mix(vertex.color_a, vertex.color_b, position.y);
    var line_width = gizmo.line_width;

    var uv: f32;
#ifdef PERSPECTIVE
    line_width /= clip.w;
    // Height of the near plane in world units.
    let pos0 = view.view_from_clip * vec4(0.0, -1.0, 0.0, 1.0);
    let pos1 = view.view_from_clip * vec4(0.0, 1.0, 0.0, 1.0);
    let near_clipping_plane_height = length(pos0.xyz / pos0.w - pos1.xyz / pos1.w);
    // The clipped end points, not the inputs (clip_near_plane may have moved them).
    let world_a = view.world_from_clip * clip_a;
    let world_b = view.world_from_clip * clip_b;
    let position_a = world_a.xyz / world_a.w;
    let position_b = world_b.xyz / world_b.w;
    let world_distance = length(position_a - position_b);
    // Keeps dots from sliding when the start point is clipped.
    let local_a = gizmo.world_from_local * vec4(vertex.position_a, 1.0);
    let clipped_offset = length(position_a - local_a.xyz);
    uv = (clipped_offset + position.y * world_distance) * resolution.y / near_clipping_plane_height
        / gizmo.line_width;
#else
    // Distance of b along the camera axis; 1 for orthographic views.
    let camera_b = view.view_from_clip * clip_b;
    var depth_adaptment: f32;
    if clip_b.w == 1.0 {
        depth_adaptment = 1.0;
    } else {
        depth_adaptment = -camera_b.z / camera_b.w;
    }
    uv = position.y * depth_adaptment * length(screen_b - screen_a) / gizmo.line_width;
#endif

    // Thin lines fade instead of shimmering.
    if line_width > 0.0 && line_width < 1.0 {
        color.a *= line_width;
        line_width = 1.0;
    }

    let x_offset = line_width * position.x * x_basis;
    let screen = mix(screen_a, screen_b, position.y) + x_offset;
    let clip_position = from_screen(screen, resolution, biased_depth(clip), clip.w);

    let line_fraction = 2.0 * gizmo.line_scale / (gizmo.gap_scale + gizmo.line_scale);
    uv /= (gizmo.gap_scale + gizmo.line_scale) / 2.0;
    return LineOutput(clip_position, color, uv, line_fraction);
}

// ---- Joints ----

struct JointInput {
    @location(0) position_a: vec3<f32>,
    @location(1) position_b: vec3<f32>,
    @location(2) position_c: vec3<f32>,
    @location(3) color: vec4<f32>,
    @builtin(vertex_index) index: u32,
}

struct JointOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

struct JointFrame {
    clip_b: vec4<f32>,
    screen_b: vec2<f32>,
    ab: vec2<f32>,
    cb: vec2<f32>,
    color: vec4<f32>,
    line_width: f32,
}

fn joint_frame(vertex: JointInput) -> JointFrame {
    let clip_from_local = view.clip_from_world * gizmo.world_from_local;
    var clip_a = clip_from_local * vec4(vertex.position_a, 1.0);
    var clip_b = clip_from_local * vec4(vertex.position_b, 1.0);
    var clip_c = clip_from_local * vec4(vertex.position_c, 1.0);
    clip_a = clip_near_plane(clip_a, clip_c);
    clip_b = clip_near_plane(clip_b, clip_a);
    clip_c = clip_near_plane(clip_c, clip_b);
    clip_a = clip_near_plane(clip_a, clip_c);
    // The joint vertex itself behind the near plane is never visible (its segments are culled
    // or clipped there); `culled` makes the joint draw nothing.
    let culled = clip_b.z < 0.0;

    let resolution = view.viewport.zw;
    let screen_a = to_screen(clip_a, resolution);
    let screen_b = to_screen(clip_b, resolution);
    let screen_c = to_screen(clip_c, resolution);

    var color = vertex.color;
    var line_width = gizmo.line_width;
#ifdef PERSPECTIVE
    line_width /= clip_b.w;
#endif
    if line_width > 0.0 && line_width < 1.0 {
        color.a *= line_width;
        line_width = 1.0;
    }
    // A zero-length arm (a or c on top of b on screen) takes the other arm's direction reversed,
    // as if the strip ran straight through b; both zero leave a joint of no width.
    let ab = safe_dir(screen_b - screen_a, vec2(0.0));
    let cb = safe_dir(screen_b - screen_c, -ab);
    return JointFrame(
        clip_b,
        screen_b,
        safe_dir(ab, -cb),
        cb,
        color,
        select(line_width, 0.0, culled || dot(cb, cb) == 0.0),
    );
}

fn joint_output(frame: JointFrame, screen: vec2<f32>) -> JointOutput {
    if frame.line_width <= 0.0 {
        return JointOutput(CULLED, frame.color);
    }
    let clip_position = from_screen(screen, view.viewport.zw, biased_depth(frame.clip_b), frame.clip_b.w);
    return JointOutput(clip_position, frame.color);
}

// Unit bisector tangent of a joint. When the strip doubles back on itself (ab == cb) it is the
// arm's normal, which points along the fold.
fn joint_tangent(f: JointFrame) -> vec2<f32> {
    return safe_dir(f.ab - f.cb, vec2(-f.ab.y, f.ab.x));
}

// Miter limit 4 (the SVG `stroke-miterlimit` default): the miter offset is
// line_width / dot(normal, ab_norm), which grows without bound as the turn tightens and divides by
// zero when the strip doubles back. dot(normal, ab_norm) is sin(turn / 2) >= 0.
const MITER_MIN_SIN: f32 = 0.25;

@vertex
fn vertex_bevel(vertex: JointInput) -> JointOutput {
    let position = bevel_corner(vertex.index);
    let f = joint_frame(vertex);
    let ab_norm = vec2(-f.ab.y, f.ab.x);
    let cb_norm = vec2(f.cb.y, -f.cb.x);
    let tangent = joint_tangent(f);
    let normal = vec2(-tangent.y, tangent.x);
    let sigma = sign(dot(f.ab + f.cb, normal));
    let p0 = f.line_width * sigma * ab_norm;
    let p1 = f.line_width * sigma * cb_norm;
    return joint_output(f, f.screen_b + position.x * p0 + position.y * p1);
}

@vertex
fn vertex_miter(vertex: JointInput) -> JointOutput {
    let position = miter_corner(vertex.index);
    let f = joint_frame(vertex);
    let ab_norm = vec2(-f.ab.y, f.ab.x);
    let cb_norm = vec2(f.cb.y, -f.cb.x);
    let tangent = joint_tangent(f);
    let normal = vec2(-tangent.y, tangent.x);
    let sigma = sign(dot(f.ab + f.cb, normal));
    let p0 = f.line_width * sigma * ab_norm;
    let p1 = f.line_width * sigma * normal / max(dot(normal, ab_norm), MITER_MIN_SIN);
    let p2 = f.line_width * sigma * cb_norm;
    return joint_output(f, f.screen_b + position.x * p0 + position.y * p1 + position.z * p2);
}

@vertex
fn vertex_round(vertex: JointInput) -> JointOutput {
    let f = joint_frame(vertex);
    let ab_norm = vec2(-f.ab.y, f.ab.x);
    let cb_norm = vec2(f.cb.y, -f.cb.x);
    // `joints_resolution` triangles: vertex 0 of each is the centre at screen_b, vertex 1 is
    // nearer the ab line and vertex 2 nearer the cb line.
    let in_triangle_index = f32(vertex.index % 3u);
    let tri_index = f32(vertex.index / 3u);
    let radius = sign(in_triangle_index) * 0.5 * f.line_width;
    let theta = acos(clamp(dot(ab_norm, cb_norm), -1.0, 1.0));
    let sigma = sign(dot(ab_norm, f.cb));
    let angle = theta * (tri_index + in_triangle_index - 1.0) / f32(max(gizmo.joints_resolution, 1u));
    let position_x = sigma * radius * cos(angle);
    let position_y = radius * sin(angle);
    return joint_output(f, f.screen_b + position_x * ab_norm + position_y * f.ab);
}

// ---- Fragments ----

fn occluded(position: vec4<f32>) -> bool {
    if gizmo.on_top != 0u {
        return false;
    }
    let dims = vec2<i32>(textureDimensions(scene_depth));
    let uv = (position.xy - view.viewport.xy) / view.viewport.zw;
    let texel = clamp(vec2<i32>(uv * vec2<f32>(dims)), vec2<i32>(0), dims - vec2<i32>(1));
    let scene = textureLoad(scene_depth, texel, 0);
    return position.z > scene + DEPTH_SLACK;
}

fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.0031308 {
        return 12.92 * c;
    }
    return 1.055 * pow(c, 1.0 / 2.4) - 0.055;
}

fn finish(color: vec4<f32>) -> vec4<f32> {
    if view.flags.x != 0u {
        let rgb = clamp(color.rgb, vec3(0.0), vec3(1.0));
        return vec4(linear_to_srgb(rgb.r), linear_to_srgb(rgb.g), linear_to_srgb(rgb.b), color.a);
    }
    return color;
}

struct LineFragment {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) uv: f32,
    @location(2) line_fraction: f32,
}

fn non_solid_uv(in: LineFragment) -> f32 {
#ifdef PERSPECTIVE
    let uv = in.uv;
#else
    let uv = in.uv * in.position.w;
#endif
    // Subtracting the offset marches the pattern from the start of the line to its end.
    let animated = uv - gizmo.animation_offset;
    let period = 2.0;
    return animated - floor(animated / period) * period;
}

@fragment
fn fragment_solid(in: LineFragment) -> @location(0) vec4<f32> {
    if occluded(in.position) {
        discard;
    }
    return finish(in.color);
}

@fragment
fn fragment_dotted(in: LineFragment) -> @location(0) vec4<f32> {
    if occluded(in.position) {
        discard;
    }
    let alpha = 1.0 - floor(non_solid_uv(in));
    return finish(vec4(in.color.xyz, in.color.w * alpha));
}

@fragment
fn fragment_dashed(in: LineFragment) -> @location(0) vec4<f32> {
    if occluded(in.position) {
        discard;
    }
    let alpha = 1.0 - floor(min(non_solid_uv(in) / in.line_fraction, 1.0));
    return finish(vec4(in.color.xyz, in.color.w * alpha));
}

struct JointFragment {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

@fragment
fn fragment_joint(in: JointFragment) -> @location(0) vec4<f32> {
    if occluded(in.position) {
        discard;
    }
    return finish(in.color);
}

// ---- Debug labels from a glyph atlas (ADR-7254, Regia Engine-original) ----
//
// Glyph quads the CPU placed in world space (`gizmos::GizmoLabelBatch`), six vertices each, that
// sample one single-channel coverage page of the installed font atlas (`DebugLabelGlyphSource`).
// The batch uniform's `on_top` and the scene-depth test are the lines'.

@group(2) @binding(0) var label_page: texture_2d<f32>;
@group(2) @binding(1) var label_samp: sampler;

struct LabelInput {
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
}

struct LabelOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
}

@vertex
fn vertex_label(vertex: LabelInput) -> LabelOutput {
    // A constructor, not a function-scope struct variable (the layout scan refuses those).
    let clip = view.clip_from_world * vec4<f32>(vertex.position, 1.0);
    return LabelOutput(vec4<f32>(clip.xy, biased_depth(clip), clip.w), vertex.uv, vertex.color);
}

@fragment
fn fragment_label(in: LabelOutput) -> @location(0) vec4<f32> {
    // Sampled before the depth test: implicit derivatives stay in uniform control flow.
    let coverage = textureSample(label_page, label_samp, in.uv).r;
    if occluded(in.position) || coverage <= 0.0 {
        discard;
    }
    return finish(vec4<f32>(in.color.rgb, in.color.a * coverage));
}
