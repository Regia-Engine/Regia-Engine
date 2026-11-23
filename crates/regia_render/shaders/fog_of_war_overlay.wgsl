// SPDX-License-Identifier: MPL-2.0
// Fog of war world overlay (audit gap FW-1, `fog_overlay.rs`). Regia Engine-original.
//
// A fullscreen triangle at the `before_post` slot. Each pixel reconstructs its world position from
// the scene depth, samples the viewer's fog texture (one R8 texel per fog cell: 0 unexplored,
// 128 explored, 255 visible) at world XZ with a 3 x 3 tent filter whose tap spacing is the edge
// softness in cells, and outputs a colour the blend state multiplies into the HDR image
// (dst * src). The sky (depth at the far plane) is left alone; ground outside the grid reads as
// unexplored. A multisampled scene depth uses the same source with `texture_depth_2d` replaced by
// `texture_depth_multisampled_2d` (`fog_overlay.rs`; `textureLoad`'s third argument is then the
// sample index).

struct Uniforms {
    world_from_clip: mat4x4<f32>,
    // x, y, w, h of the view inside the target, in pixels.
    viewport: vec4<f32>,
    // origin x, origin z, size x, size z of the grid in metres.
    grid: vec4<f32>,
    // cols, rows, softness (cells), strength (0..1).
    texel: vec4<f32>,
    // explored tint rgb, explored darkness (0..1).
    explored: vec4<f32>,
    // unexplored tint rgb, unexplored darkness (0..1).
    unexplored: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var fog_texture: texture_2d<f32>;
@group(0) @binding(2) var fog_sampler: sampler;
@group(0) @binding(3) var scene_depth: texture_depth_2d;

struct VsOut {
    @builtin(position) position: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let x = f32((index << 1u) & 2u);
    let y = f32(index & 2u);
    return VsOut(vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0));
}

// One fog tap at grid uv `uv`: the filtered texel, 0 off the grid.
fn tap(uv: vec2<f32>) -> f32 {
    if (any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0))) {
        return 0.0;
    }
    return textureSampleLevel(fog_texture, fog_sampler, uv, 0.0).r;
}

// The multiplier for a pixel whose view uv is `view_uv` and depth `depth`.
fn fog_color(view_uv: vec2<f32>, depth: f32) -> vec4<f32> {
    if (depth >= 1.0) {
        return vec4<f32>(1.0);
    }
    let ndc = vec4<f32>(view_uv.x * 2.0 - 1.0, 1.0 - view_uv.y * 2.0, depth, 1.0);
    let world_h = u.world_from_clip * ndc;
    let world = world_h.xyz / world_h.w;
    let uv = (world.xz - u.grid.xy) / max(u.grid.zw, vec2<f32>(1e-6));
    let spacing = vec2<f32>(u.texel.z) / max(u.texel.xy, vec2<f32>(1.0));
    // 3 x 3 tent: weights 1 2 1 per axis, 16 in all.
    var sum = 0.0;
    for (var j = -1; j <= 1; j = j + 1) {
        for (var i = -1; i <= 1; i = i + 1) {
            let w = f32(2 - abs(i)) * f32(2 - abs(j));
            sum = sum + w * tap(uv + vec2<f32>(f32(i), f32(j)) * spacing);
        }
    }
    let v = sum / 16.0;
    let strength = clamp(u.texel.w, 0.0, 1.0);
    let unexplored = mix(vec3<f32>(1.0), u.unexplored.rgb * (1.0 - u.unexplored.w), strength);
    let explored = mix(vec3<f32>(1.0), u.explored.rgb * (1.0 - u.explored.w), strength);
    let mid = 128.0 / 255.0;
    var m: vec3<f32>;
    if (v < mid) {
        m = mix(unexplored, explored, v / mid);
    } else {
        m = mix(explored, vec3<f32>(1.0), (v - mid) / (1.0 - mid));
    }
    return vec4<f32>(m, 1.0);
}

fn view_uv_of(position: vec4<f32>) -> vec2<f32> {
    return (position.xy - u.viewport.xy) / max(u.viewport.zw, vec2<f32>(1.0));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let view_uv = view_uv_of(in.position);
    let dims = vec2<f32>(textureDimensions(scene_depth));
    let px = vec2<i32>(clamp(view_uv * dims, vec2<f32>(0.0), dims - vec2<f32>(1.0)));
    return fog_color(view_uv, textureLoad(scene_depth, px, 0));
}
