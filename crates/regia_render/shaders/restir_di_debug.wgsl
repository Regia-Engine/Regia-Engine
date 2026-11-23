// SPDX-License-Identifier: MPL-2.0
// The evaluation path of the many-light sampler on real frames (ADR-2470): a G-buffer rebuilt
// from the scene depth (normals from neighboring depths, one flat material, camera motion
// from the previous view-projection), and the composite that shows the sampler's output in
// the frame's HDR target. Evaluation only: the production hookup reads the frame's own
// surfaces.

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

@group(0) @binding(0) var<uniform> dp: DebugParams;
@group(0) @binding(1) var scene_depth: texture_depth_2d;
@group(0) @binding(2) var out_depth: texture_storage_2d<r32float, write>;
@group(0) @binding(3) var out_normal: texture_storage_2d<rgba16float, write>;
@group(0) @binding(4) var out_albedo: texture_storage_2d<rgba16float, write>;
@group(0) @binding(5) var out_motion: texture_storage_2d<rgba16float, write>;

fn depth_at(p: vec2<i32>) -> f32 {
    let dims = vec2<f32>(textureDimensions(scene_depth));
    let uv_out = (vec2<f32>(p) + vec2<f32>(0.5)) / dp.sizes.zw;
    let color_px = dp.viewport.xy + uv_out * dp.viewport.zw;
    let t = vec2<i32>(clamp(floor(color_px / dp.sizes.xy * dims), vec2<f32>(0.0), dims - vec2<f32>(1.0)));
    return textureLoad(scene_depth, t, 0);
}

fn world_of(p: vec2<i32>, d: f32) -> vec3<f32> {
    let uv = (vec2<f32>(p) + vec2<f32>(0.5)) / dp.sizes.zw;
    let ndc = vec3<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, d);
    let w = dp.world_from_clip * vec4<f32>(ndc, 1.0);
    return w.xyz / select(w.w, 1e-6, abs(w.w) < 1e-6);
}

@compute @workgroup_size(8, 8, 1)
fn gbuffer_from_depth(@builtin(global_invocation_id) gid: vec3<u32>) {
    let size = vec2<u32>(dp.sizes.zw);
    if (gid.x >= size.x || gid.y >= size.y) {
        return;
    }
    let p = vec2<i32>(gid.xy);
    let d = depth_at(p);
    if (d >= 1.0) {
        textureStore(out_depth, p, vec4<f32>(0.0));
        textureStore(out_normal, p, vec4<f32>(0.0, 1.0, 0.0, 1.0));
        textureStore(out_albedo, p, vec4<f32>(0.0));
        textureStore(out_motion, p, vec4<f32>(0.0));
        return;
    }
    let c = world_of(p, d);
    let linear = dot(c - dp.eye.xyz, dp.forward.xyz);
    // Each axis takes the neighbor on the side with the smaller depth step, so a silhouette
    // does not bend the normal of the surface in front.
    let sz = vec2<i32>(size);
    let xl = max(p.x - 1, 0);
    let xr = min(p.x + 1, sz.x - 1);
    let yu = max(p.y - 1, 0);
    let yd = min(p.y + 1, sz.y - 1);
    let dl = depth_at(vec2<i32>(xl, p.y));
    let dr = depth_at(vec2<i32>(xr, p.y));
    let du = depth_at(vec2<i32>(p.x, yu));
    let dd = depth_at(vec2<i32>(p.x, yd));
    // On the image border one side is the pixel itself (a zero step): take the other (ADR-2652).
    var dx = world_of(vec2<i32>(xr, p.y), dr) - c;
    if (xr == p.x || (xl != p.x && abs(dl - d) < abs(dr - d))) {
        dx = c - world_of(vec2<i32>(xl, p.y), dl);
    }
    var dy = world_of(vec2<i32>(p.x, yd), dd) - c;
    if (yd == p.y || (yu != p.y && abs(du - d) < abs(dd - d))) {
        dy = c - world_of(vec2<i32>(p.x, yu), du);
    }
    var n = cross(dy, dx);
    let nl = length(n);
    n = select(-dp.forward.xyz, n / nl, nl > 1e-12);
    if (dot(n, dp.eye.xyz - c) < 0.0) {
        n = -n;
    }
    var motion = vec2<f32>(0.0);
    if (dp.forward.w > 0.5) {
        let pc = dp.prev_clip_from_world * vec4<f32>(c, 1.0);
        if (pc.w > 1e-4) {
            let pn = pc.xy / pc.w;
            let prev_uv = vec2<f32>(pn.x * 0.5 + 0.5, 0.5 - pn.y * 0.5);
            let uv = (vec2<f32>(p) + vec2<f32>(0.5)) / dp.sizes.zw;
            motion = prev_uv - uv;
        }
    }
    textureStore(out_depth, p, vec4<f32>(linear, 0.0, 0.0, 0.0));
    textureStore(out_normal, p, vec4<f32>(n, dp.material.w));
    textureStore(out_albedo, p, vec4<f32>(dp.material.rgb, 0.0));
    textureStore(out_motion, p, vec4<f32>(motion, 0.0, 0.0));
}
