// SPDX-License-Identifier: MPL-2.0
// TSR output sharpen (output resolution).
//
// An unsharp mask on the cross neighbourhood, evaluated in the reversible
// tonemapped domain c / (1 + luma) (Karis 2014), clamped to the min/max of the
// 3×3 neighbourhood so the result never overshoots a local extremum: no halos
// or ringing beside a hard edge. The history itself is never sharpened, so the
// sharpening does not feed back. Contrast-adaptive limiting follows the idea of
// AMD FidelityFX CAS / RCAS (GPUOpen documentation): flat, noisy areas get less.
// Under motion (`jitter.w` > 0) each axis adds half its 1-D Laplacian scaled by the
// pixel's own motion along it (the TAAU motion sharpen's idea, `shaders/taau_sharpen.wgsl`):
// resampling the history every frame blurs along the axes it moves on.

struct TsrFrame {
    render_size: vec4<f32>,
    output_size: vec4<f32>,
    jitter: vec4<f32>,
    tune: vec4<f32>,
    tune2: vec4<f32>,
    tune3: vec4<f32>,
    inv_jittered: mat4x4<f32>,
    cur_vp: mat4x4<f32>,
    prev_vp: mat4x4<f32>,
    velocity_prev_vp: mat4x4<f32>,
    prev_jitter: vec4<f32>,
}

@group(0) @binding(0) var history_tex: texture_2d<f32>;
@group(0) @binding(1) var<uniform> frame: TsrFrame;
@group(0) @binding(2) var aux_tex: texture_2d<f32>;
// Motion the resolve used, output pixels per frame (xy).
@group(0) @binding(3) var motion_tex: texture_2d<f32>;

// `TsrSettings::motion_sharpness` reaches full strength at this motion (output px / frame).
const MOTION_SHARPEN_FULL_PX: f32 = 1.0;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) id: u32) -> VertexOut {
    let uv = vec2<f32>(f32((id << 1u) & 2u), f32(id & 2u));
    return VertexOut(vec4<f32>(uv * 2.0 - vec2<f32>(1.0), 0.0, 1.0));
}

fn luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.25, 0.5, 0.25));
}

fn to_t(c: vec3<f32>) -> vec3<f32> {
    let e = max(c, vec3<f32>(0.0)) * frame.tune.x;
    return e / (1.0 + luma(e));
}

fn from_t(t: vec3<f32>) -> vec3<f32> {
    let l = min(luma(max(t, vec3<f32>(0.0))), 0.999);
    return max(t, vec3<f32>(0.0)) / (1.0 - l) / max(frame.tune.x, 1.0e-12);
}

fn load_t(p: vec2<i32>) -> vec3<f32> {
    let size = vec2<i32>(frame.output_size.xy);
    return to_t(textureLoad(history_tex, clamp(p, vec2<i32>(0), size - vec2<i32>(1)), 0).rgb);
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let p = vec2<i32>(in.position.xy);
    let debug = u32(frame.tune3.z + 0.5);
    if (debug > 0u) {
        // Debug views: 1 = accumulated weight / max, 2 = lock, 3 = disocclusion.
        let aux = textureLoad(aux_tex, p, 0);
        var v = 0.0;
        if (debug == 1u) {
            v = aux.w / max(frame.tune.w * 0.6 + 1.0, 1.0);
        } else if (debug == 2u) {
            v = aux.x;
        } else {
            v = aux.z;
        }
        return vec4<f32>(from_t(vec3<f32>(clamp(v, 0.0, 1.0) * 0.8)), 1.0);
    }
    let c = load_t(p);
    let amount = frame.tune.y;
    if (amount <= 0.0 && frame.jitter.w <= 0.0) {
        return vec4<f32>(from_t(c), 1.0);
    }
    let n = load_t(p + vec2<i32>(0, -1));
    let s = load_t(p + vec2<i32>(0, 1));
    let e = load_t(p + vec2<i32>(1, 0));
    let w = load_t(p + vec2<i32>(-1, 0));
    let a = load_t(p + vec2<i32>(-1, -1));
    let b = load_t(p + vec2<i32>(1, -1));
    let d = load_t(p + vec2<i32>(-1, 1));
    let f = load_t(p + vec2<i32>(1, 1));
    let lo = min(min(min(c, n), min(s, e)), min(min(w, a), min(b, min(d, f))));
    let hi = max(max(max(c, n), max(s, e)), max(max(w, a), max(b, max(d, f))));
    // Less sharpening where the neighbourhood already spans most of the range
    // (hard edges) or is pure noise near black.
    let range = luma(hi) - luma(lo);
    let adapt = clamp(1.0 - range, 0.25, 1.0);
    let blur = 0.25 * (n + s + e + w);
    var detail = amount * (c - blur);
    if (frame.jitter.w > 0.0) {
        let motion_px = abs(textureLoad(motion_tex, p, 0).xy);
        let m = frame.jitter.w * clamp(motion_px / MOTION_SHARPEN_FULL_PX, vec2<f32>(0.0), vec2<f32>(1.0));
        detail = detail + 0.5 * (m.x * (c - 0.5 * (e + w)) + m.y * (c - 0.5 * (n + s)));
    }
    let sharpened = c + adapt * detail;
    return vec4<f32>(from_t(clamp(sharpened, lo, hi)), 1.0);
}
