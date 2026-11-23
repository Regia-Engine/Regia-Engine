// SPDX-License-Identifier: MPL-2.0
// Depth-aware bilateral upsample of the low-resolution soft-particle layer.
//
// The soft emitters (dust, smoke, billows; `scene::is_soft_emitter`) render into a 1/2 or 1/4
// resolution Rgba16Float layer (premultiplied colour + alpha, cleared to transparent). This pass
// upsamples that layer and blends it over the frame (premultiplied "over": the pipeline blends
// One / OneMinusSrcAlpha), inside the frame's transparent pass.
//
// The upsample is joint-bilateral: each of the four nearest low-resolution texels is weighted by
// its bilinear weight and by how close the scene depth at that texel is to the depth of this
// full-resolution pixel, in *linear* view depth relative to this pixel's depth (raw depth is
// non-linear: far surfaces would all look alike and smoke would bleed across a hull's
// silhouette). When every tap is rejected (a silhouette pixel whose neighbours all lie on the
// other surface) the tap closest in depth is used alone ("nearest-depth" upsampling).
// Provenance: Kopf, Cohen, Lischinski & Uyttendaele, "Joint Bilateral Upsampling", SIGGRAPH
// 2007; low-resolution off-screen particles with depth-aware upsampling: Cantlay, "High-Speed,
// Off-Screen Particles", GPU Gems 3, ch. 23, 2007.

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

struct Params {
    // (near, far, relative depth tolerance, unused)
    depth: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    // Full-screen triangle (-1,-1) (3,-1) (-1,3) from the vertex index, not an indexed local
    // array: naga 30 gives that array an ArrayStride, which SPIR-V 1.6 forbids on Function
    // storage (VUID-StandaloneSpirv-None-10684).
    let p = vec2<f32>(f32((vi & 1u) * 4u) - 1.0, f32((vi >> 1u) * 4u) - 1.0);
    return VsOut(vec4<f32>(p, 1.0, 1.0), p * 0.5 + vec2<f32>(0.5));
}

@group(0) @binding(0) var low_tex: texture_2d<f32>;       // low-res premultiplied particle layer
@group(0) @binding(1) var depth_tex: texture_depth_2d;    // full-res scene depth
@group(0) @binding(2) var<uniform> params: Params;

// Linear view depth from a [0, 1] DirectX-style perspective depth.
fn linear_depth(d: f32) -> f32 {
    let n = params.depth.x;
    let f = params.depth.y;
    return n * f / max(f - d * (f - n), 1e-6);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let px = vec2<i32>(in.position.xy);
    let full = vec2<i32>(textureDimensions(depth_tex));
    let low = vec2<i32>(textureDimensions(low_tex));
    let ratio = vec2<f32>(full) / vec2<f32>(low);
    let center = linear_depth(textureLoad(depth_tex, px, 0));

    // Low-resolution texel coordinate of this pixel's centre, then the four nearest taps.
    let lp = (vec2<f32>(px) + vec2<f32>(0.5)) / ratio - vec2<f32>(0.5);
    let base = vec2<i32>(floor(lp));
    let f = fract(lp);

    var sum = vec4<f32>(0.0);
    var wsum = 0.0;
    var best = vec4<f32>(0.0);
    var best_dz = 1e30;
    for (var dy = 0; dy <= 1; dy = dy + 1) {
        for (var dx = 0; dx <= 1; dx = dx + 1) {
            let tap = clamp(base + vec2<i32>(dx, dy), vec2<i32>(0), low - vec2<i32>(1));
            let c = textureLoad(low_tex, tap, 0);
            // The full-res pixel the low-res pass sampled for this texel (its centre).
            let probe = clamp(vec2<i32>((vec2<f32>(tap) + vec2<f32>(0.5)) * ratio), vec2<i32>(0), full - vec2<i32>(1));
            let dz = abs(linear_depth(textureLoad(depth_tex, probe, 0)) - center) / max(center, 1e-3);
            let depth_w = exp(-dz * params.depth.z);
            let wx = select(1.0 - f.x, f.x, dx == 1);
            let wy = select(1.0 - f.y, f.y, dy == 1);
            let w = depth_w * wx * wy;
            sum = sum + c * w;
            wsum = wsum + w;
            if (dz < best_dz) {
                best_dz = dz;
                best = c;
            }
        }
    }
    let particle = select(best, sum / max(wsum, 1e-6), wsum > 1e-3);
    let a = clamp(particle.a, 0.0, 1.0);
    return vec4<f32>(particle.rgb, a);
}
