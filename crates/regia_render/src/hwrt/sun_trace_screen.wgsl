// SPDX-License-Identifier: MPL-2.0

// The screen-space path, where hardware ray queries are unavailable: the ray marches through the
// depth buffer in steps that grow geometrically (a pixel near the receiver matters more than one
// far along the ray). A sample shadows the ray when it lies in front of the ray by more than the
// bias and less than the assumed thickness. A ray that leaves the screen is lit: what it would
// have hit is the shadow map's, and this variant keeps the shadow map (ADR-2035).

// Must match `hwrt::sun_trace::SCREEN_*`.
const SCREEN_FIRST_STEP_M: f32 = 0.03;
const SCREEN_RAY_M: f32 = 120.0;
const SCREEN_BIAS: f32 = 0.0015;
const SCREEN_THICKNESS: f32 = 0.04;
const SCREEN_THICKNESS_MIN_M: f32 = 0.25;
const SCREEN_THICKNESS_MAX_M: f32 = 4.0;

fn view_depth(depth: f32) -> f32 {
    let near = params.eye.w;
    let far = params.tune.z;
    return near * far / (far - depth * (far - near));
}

fn sun_visibility(o: vec3<f32>, d: vec3<f32>, t_min: f32, t_max: f32) -> f32 {
    let steps = max(params.counts.y, 2u);
    let reach = min(SCREEN_RAY_M, t_max);
    let growth = pow(reach / SCREEN_FIRST_STEP_M, 1.0 / f32(steps - 1u));
    let dims = vec2<i32>(textureDimensions(depth_tex));
    var t = max(SCREEN_FIRST_STEP_M, t_min);
    for (var i = 0u; i < steps; i = i + 1u) {
        let clip = params.view_proj * vec4<f32>(o + d * t, 1.0);
        if clip.w <= 0.0 {
            return 1.0;
        }
        let ndc = clip.xyz / clip.w;
        let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
        if uv.x < 0.0 || uv.x >= 1.0 || uv.y < 0.0 || uv.y >= 1.0 {
            return 1.0;
        }
        let pixel = clamp(
            vec2<i32>(params.viewport.xy + uv * params.viewport.zw),
            vec2<i32>(0),
            dims - vec2<i32>(1),
        );
        let scene = textureLoad(depth_tex, pixel, 0);
        if scene < 1.0 {
            let gap = clip.w - view_depth(scene);
            let thickness = clamp(clip.w * SCREEN_THICKNESS, SCREEN_THICKNESS_MIN_M, SCREEN_THICKNESS_MAX_M);
            if gap > clip.w * SCREEN_BIAS && gap < thickness {
                return 0.0;
            }
        }
        t = t * growth;
    }
    return 1.0;
}
