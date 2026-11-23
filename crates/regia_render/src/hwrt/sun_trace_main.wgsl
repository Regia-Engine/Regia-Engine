// SPDX-License-Identifier: MPL-2.0
// Screen-sized sun visibility (ADR-2035, `hwrt::sun_trace`): each pixel's world position is
// reconstructed from the single-sample scene depth and one ray goes toward the sun, through
// whichever tracer this module was concatenated with (`sun_trace_ray_query.wgsl`,
// `sun_trace_bvh.wgsl` or `sun_trace_screen.wgsl`, each defining `sun_visibility`). Writes the
// texture contact shadows write: R = sun visibility, 1 = lit (forward binding 22).

struct SunTraceParams {
    inv_view_proj: mat4x4<f32>,
    view_proj: mat4x4<f32>,
    // xyz: unit direction toward the sun. w: the longest ray, world units.
    to_sun: vec4<f32>,
    // xyz: the eye. w: near plane.
    eye: vec4<f32>,
    // The pixel rectangle NDC maps to: x, y, width, height.
    viewport: vec4<f32>,
    // x: ray start offset, metres. y: the same per metre of distance to the eye. z: far plane.
    tune: vec4<f32>,
    // x: BVH nodes. y: screen march steps.
    counts: vec4<u32>,
}

@group(0) @binding(0) var<uniform> params: SunTraceParams;
@group(0) @binding(1) var depth_tex: texture_depth_2d;
@group(0) @binding(2) var out_vis: texture_storage_2d<rgba8unorm, write>;

fn world_at(pixel: vec2<i32>, depth: f32) -> vec3<f32> {
    let uv = (vec2<f32>(pixel) + vec2<f32>(0.5) - params.viewport.xy) / params.viewport.zw;
    let ndc = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, depth, 1.0);
    let world = params.inv_view_proj * ndc;
    return world.xyz / world.w;
}

// The surface normal from the depth buffer: on each axis the neighbour nearer in depth, so a
// silhouette does not bend the normal of the surface in front of it.
fn depth_normal(pixel: vec2<i32>, depth: f32, p: vec3<f32>, dims: vec2<i32>) -> vec3<f32> {
    let lo = vec2<i32>(0);
    let hi = dims - vec2<i32>(1);
    let xa = clamp(pixel + vec2<i32>(-1, 0), lo, hi);
    let xb = clamp(pixel + vec2<i32>(1, 0), lo, hi);
    let ya = clamp(pixel + vec2<i32>(0, -1), lo, hi);
    let yb = clamp(pixel + vec2<i32>(0, 1), lo, hi);
    let dxa = textureLoad(depth_tex, xa, 0);
    let dxb = textureLoad(depth_tex, xb, 0);
    let dya = textureLoad(depth_tex, ya, 0);
    let dyb = textureLoad(depth_tex, yb, 0);
    // At the screen's edge one neighbour is the pixel itself: the other is the only choice.
    var ex = world_at(xb, dxb) - p;
    if xb.x == pixel.x || (xa.x != pixel.x && abs(dxa - depth) < abs(dxb - depth)) {
        ex = p - world_at(xa, dxa);
    }
    var ey = world_at(yb, dyb) - p;
    if yb.y == pixel.y || (ya.y != pixel.y && abs(dya - depth) < abs(dyb - depth)) {
        ey = p - world_at(ya, dya);
    }
    let to_eye = params.eye.xyz - p;
    let n = cross(ey, ex);
    let len = length(n);
    if !(len > 1e-12) {
        return normalize(to_eye);
    }
    let unit = n / len;
    return select(unit, -unit, dot(unit, to_eye) < 0.0);
}

@compute @workgroup_size(8, 8)
fn trace_sun(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = vec2<i32>(textureDimensions(out_vis));
    let pixel = vec2<i32>(gid.xy);
    if pixel.x >= dims.x || pixel.y >= dims.y {
        return;
    }
    let depth = textureLoad(depth_tex, pixel, 0);
    // The far plane: no surface, so nothing reads this pixel's sun.
    var vis = 1.0;
    if depth < 1.0 {
        let p = world_at(pixel, depth);
        let dist = length(params.eye.xyz - p);
        let offset = params.tune.x + params.tune.y * dist;
        let o = p + depth_normal(pixel, depth, p, dims) * offset;
        vis = sun_visibility(o, params.to_sun.xyz, offset * 0.5, params.to_sun.w);
    }
    textureStore(out_vis, pixel, vec4<f32>(vis, vis, vis, 1.0));
}
