// SPDX-License-Identifier: MPL-2.0
#import regia::utils

// Fullscreen post: exposure, dual-filter bloom, tonemap, FXAA, single-frame TAA.
// params.xy = (weight or exposure, mode). params.zw = 1 / destination size.
// extra.xy = texel size of the lower-resolution input of a bloom pass: the source of a
// downsample, the lower level of an upsample, the bloom of the composite (0 uses params.zw).
// extra.x on the tonemap pass is the operator (0 AgX, 1 ACES, 2 neutral).
// extra.y = vignette, extra.z = film grain, extra.w = look saturation; wb.w = output dither
// in code values (0 = off, ADR-2486);
// params.x on the tonemap pass = look contrast.
// Composite (mode 3): params.x = bloom blend (an energy-conserving lerp toward the
// blurred chain, not an additive glow).
//
// Bloom levels are pixel-aligned (ADR-1152): level k + 1 is level k's size halved and rounded
// up, and texel i of level k + 1 covers texels 2i and 2i + 1 of level k, whatever the parity of
// the sizes. Each step then conserves the energy of a highlight away from the frame border and
// keeps its centroid, so the glow neither gains nor loses brightness, nor jumps, as the highlight
// moves by a pixel.

struct PostParams {
    params: vec4<f32>,
    extra: vec4<f32>,
    // The view's grade: white-balance gains (rgb), lift (rgb, w = 1: apply the LUT),
    // gamma (rgb, w = LUT edge), gain (rgb, w = 1 when lift/gamma/gain are not neutral).
    wb: vec4<f32>,
    lift: vec4<f32>,
    gamma: vec4<f32>,
    gain: vec4<f32>,
    // Local exposure: x = shadow contrast, y = highlight contrast, z = detail strength,
    // w = 1 when on; grid: xy = pixels to grid UV, z = log2 luminance of the first bin,
    // w = log2 span of the bins (`local_exposure.wgsl`).
    local: vec4<f32>,
    local_grid: vec4<f32>,
}

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_samp: sampler;
@group(0) @binding(2) var<uniform> post: PostParams;
@group(0) @binding(3) var add_tex: texture_2d<f32>;
// The view's 3D colour LUT (identity 2³ when none); `lift.w` = 1 enables it, `gamma.w` = edge.
@group(0) @binding(4) var lut_tex: texture_3d<f32>;
// Local exposure's bilateral grid: (mean log luminance · weight, weight), blurred.
@group(0) @binding(5) var grid_tex: texture_3d<f32>;

// Middle grey of the exposed image: local exposure pulls regions toward it.
const LOCAL_EXPOSURE_PIVOT: f32 = -2.4739312; // log2(0.18)

// Local exposure (Durand and Dorsey 2002, with the base layer from the bilateral grid of Chen,
// Paris and Durand 2007): the pixel's log luminance is its region's (the grid sliced at its
// position and luminance) plus detail; the region is scaled toward middle grey by the shadow or
// highlight contrast, the detail by the detail strength.
fn local_exposure(c: vec3<f32>, pos: vec2<f32>) -> vec3<f32> {
    let lum = regia::utils::luminance(c);
    if (lum <= 0.0) {
        return c;
    }
    let l = log2(lum);
    let t = clamp((l - post.local_grid.z) / post.local_grid.w, 0.0, 1.0);
    let g = textureSampleLevel(grid_tex, src_samp, vec3<f32>(pos * post.local_grid.xy, t), 0.0).rg;
    if (g.y <= 1e-4) {
        return c;
    }
    let base = post.local_grid.z + post.local_grid.w * (g.x / g.y);
    let contrast = select(post.local.y, post.local.x, base < LOCAL_EXPOSURE_PIVOT);
    let new_base = LOCAL_EXPOSURE_PIVOT + (base - LOCAL_EXPOSURE_PIVOT) * contrast;
    let new_l = new_base + (l - base) * post.local.z;
    return c * exp2(clamp(new_l - l, -8.0, 8.0));
}

@vertex
fn vs_main(@builtin(vertex_index) id: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((id << 1u) & 2u), f32(id & 2u));
    return vec4<f32>(uv * 2.0 - vec2<f32>(1.0), 0.0, 1.0);
}

fn uv_at(pos: vec4<f32>) -> vec2<f32> {
    return pos.xy * post.params.zw;
}

fn src_texel() -> vec2<f32> {
    if (post.extra.x > 0.0 && post.extra.y > 0.0) {
        return post.extra.xy;
    }
    return post.params.zw;
}

fn tap4(uv: vec2<f32>) -> vec3<f32> {
    let o = src_texel() * 0.5;
    let a = textureSampleLevel(src_tex, src_samp, uv + vec2<f32>(-o.x, -o.y), 0.0).rgb;
    let b = textureSampleLevel(src_tex, src_samp, uv + vec2<f32>(o.x, -o.y), 0.0).rgb;
    let c = textureSampleLevel(src_tex, src_samp, uv + vec2<f32>(-o.x, o.y), 0.0).rgb;
    let d = textureSampleLevel(src_tex, src_samp, uv + vec2<f32>(o.x, o.y), 0.0).rgb;
    return (a + b + c + d) * 0.25;
}

// Bloom downsample of the source around `pos` (a destination pixel centre): a separable
// [1 3 3 1] / 8 tent over the 4 × 4 source texels centred on the 2 × 2 block the destination
// texel covers, as four bilinear taps 0.75 texel from the block's centre. Each source texel's
// weights over the destination sum to 1/4 (energy kept) and the tent is symmetric about the
// block (centroid kept); a 2 × 2 box would keep the energy but snap the centroid to the block.
// Taps past the edge clamp, so a flat field stays flat up to the border.
fn bloom_down(pos: vec2<f32>) -> vec3<f32> {
    let t = src_texel();
    let c = pos * 2.0 * t;
    let o = t * 0.75;
    let a = textureSampleLevel(src_tex, src_samp, c + vec2<f32>(-o.x, -o.y), 0.0).rgb;
    let b = textureSampleLevel(src_tex, src_samp, c + vec2<f32>(o.x, -o.y), 0.0).rgb;
    let d = textureSampleLevel(src_tex, src_samp, c + vec2<f32>(-o.x, o.y), 0.0).rgb;
    let e = textureSampleLevel(src_tex, src_samp, c + vec2<f32>(o.x, o.y), 0.0).rgb;
    return (a + b + d + e) * 0.25;
}

// Interleaved gradient noise (Jimenez 2014) of a pixel position, in [0, 1).
fn output_ign(p: vec2<f32>) -> f32 {
    return fract(52.9829189 * fract(dot(floor(p), vec2<f32>(0.06711056, 0.00583715))));
}

fn hash12(p: vec2<f32>) -> f32 {
    return fract(sin(dot(p, vec2<f32>(127.1, 311.7))) * 43758.5453);
}

@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let mode = u32(post.params.y + 0.5);
    let uv = uv_at(pos);
    if (mode == 0u) {
        // Exposure and white balance, both scene-referred (before bloom and the tonemap).
        // Bloom and the tonemap read this: a non-finite texel must not spread through the blur.
        var c = regia::utils::finite_or_zero(textureSampleLevel(src_tex, src_samp, uv, 0.0).rgb) * post.params.x * post.wb.rgb;
        if (post.local.w > 0.5) {
            c = local_exposure(c, pos.xy);
        }
        return vec4<f32>(c, 1.0);
    }
    if (mode == 1u) {
        return vec4<f32>(bloom_down(pos.xy), 1.0);
    }
    if (mode == 2u) {
        // The lower level is addressed at half this pixel's position (not by this level's uv:
        // with odd sizes the two levels' uv spaces differ).
        let low = textureSampleLevel(src_tex, src_samp, pos.xy * 0.5 * src_texel(), 0.0).rgb;
        let high = textureSampleLevel(add_tex, src_samp, uv, 0.0).rgb;
        return vec4<f32>(low * 0.5 + high * 0.5, 1.0);
    }
    if (mode == 3u) {
        let base = textureSampleLevel(src_tex, src_samp, uv, 0.0).rgb;
        let bloom = textureSampleLevel(add_tex, src_samp, pos.xy * 0.5 * src_texel(), 0.0).rgb;
        return vec4<f32>(mix(base, bloom, post.params.x), 1.0);
    }
    if (mode == 4u) {
        let c = textureSampleLevel(src_tex, src_samp, uv, 0.0).rgb;
        let op = u32(post.extra.x + 0.5);
        // AgX returns display-encoded values; the two linear-output operators encode here.
        var color = regia::utils::tonemap_agx_vec(c);
        if (op == 1u) {
            color = regia::utils::encode_srgb(regia::utils::tonemap_aces_vec(c));
        } else if (op == 2u) {
            color = regia::utils::encode_srgb(regia::utils::tonemap_neutral_vec(c));
        }
        color = regia::utils::display_look(color, post.params.x, post.extra.w);
        if (post.gain.w > 0.5) {
            // Lift / gamma / gain on the display image: (gain · (x + lift · (1 − x)))^(1/gamma).
            let lifted = post.gain.rgb * (color + post.lift.rgb * (vec3<f32>(1.0) - color));
            color = pow(max(lifted, vec3<f32>(0.0)), vec3<f32>(1.0) / max(post.gamma.rgb, vec3<f32>(0.05)));
            color = clamp(color, vec3<f32>(0.0), vec3<f32>(1.0));
        }
        if (post.lift.w > 0.5) {
            // Texel centres: the LUT's grid point i sits at (i + 0.5) / n.
            let n = max(post.gamma.w, 2.0);
            let uvw = clamp(color, vec3<f32>(0.0), vec3<f32>(1.0)) * ((n - 1.0) / n) + vec3<f32>(0.5 / n);
            color = clamp(textureSampleLevel(lut_tex, src_samp, uvw, 0.0).rgb, vec3<f32>(0.0), vec3<f32>(1.0));
        }
        if (post.extra.y > 0.0) {
            let d = length(uv - vec2<f32>(0.5));
            let vig = mix(1.0, smoothstep(0.85, 0.35, d), post.extra.y);
            color *= vig;
        }
        if (post.extra.z > 0.0) {
            let n = hash12(pos.xy);
            color = clamp(color + vec3<f32>(n - 0.5) * post.extra.z, vec3<f32>(0.0), vec3<f32>(1.0));
        }
        if (post.wb.w > 0.0) {
            // Opt-in output dither (ADR-2486): triangular noise of +-wb.w code values before the 8-bit
            // store, so a smooth gradient does not quantize into bands. Two interleaved-gradient-noise
            // samples at offset positions sum to a triangular distribution.
            let a = output_ign(pos.xy);
            let b = output_ign(pos.xy + vec2<f32>(5.588238, 3.1234));
            color = clamp(color + vec3<f32>(a + b - 1.0) * (post.wb.w / 255.0), vec3<f32>(0.0), vec3<f32>(1.0));
        }
        return vec4<f32>(color, 1.0);
    }
    if (mode == 5u) {
        let c = textureSampleLevel(src_tex, src_samp, uv, 0.0).rgb;
        let o = post.params.zw;
        let l = regia::utils::luminance(c);
        let ll = regia::utils::luminance(textureSampleLevel(src_tex, src_samp, uv + vec2<f32>(-o.x, 0.0), 0.0).rgb);
        let lr = regia::utils::luminance(textureSampleLevel(src_tex, src_samp, uv + vec2<f32>(o.x, 0.0), 0.0).rgb);
        let lu = regia::utils::luminance(textureSampleLevel(src_tex, src_samp, uv + vec2<f32>(0.0, -o.y), 0.0).rgb);
        let ld = regia::utils::luminance(textureSampleLevel(src_tex, src_samp, uv + vec2<f32>(0.0, o.y), 0.0).rgb);
        let hi = max(l, max(max(ll, lr), max(lu, ld)));
        let lo = min(l, min(min(ll, lr), min(lu, ld)));
        if (hi - lo < 0.0312) {
            return vec4<f32>(c, 1.0);
        }
        let horiz = abs(ll - lr);
        let vert = abs(lu - ld);
        var edge = c;
        if (horiz > vert) {
            edge = (textureSampleLevel(src_tex, src_samp, uv + vec2<f32>(-o.x, 0.0), 0.0).rgb
                + textureSampleLevel(src_tex, src_samp, uv + vec2<f32>(o.x, 0.0), 0.0).rgb) * 0.5;
        } else {
            edge = (textureSampleLevel(src_tex, src_samp, uv + vec2<f32>(0.0, -o.y), 0.0).rgb
                + textureSampleLevel(src_tex, src_samp, uv + vec2<f32>(0.0, o.y), 0.0).rgb) * 0.5;
        }
        return vec4<f32>(mix(c, edge, 0.5), 1.0);
    }
    if (mode == 7u) {
        // Blit into a window viewport. `extra.yz` is the viewport origin in
        // destination pixels (0 for a full-target blit). The source is the
        // view-sized image, so UVs are relative to that rect — a right-hand
        // split view must not sample with the window's absolute pixel position.
        // `extra.w` > 1 is the HDR headroom (linear paper-white output, highlights expanded).
        let blit_uv = (pos.xy - post.extra.yz) * post.params.zw;
        var c = textureSampleLevel(src_tex, src_samp, blit_uv, 0.0).rgb;
        if (post.extra.x > 0.5) {
            c = regia::utils::decode_srgb(c);
        }
        if (post.extra.w > 1.0) {
            // HDR display stage (ADR-092): expand highlights above the knee up to the headroom
            // (peak / paper white), C1 at the knee. Mirror of `calibration::hdr_highlight_expand`.
            let k = 0.5;
            let t = clamp((c - vec3<f32>(k)) / (1.0 - k), vec3<f32>(0.0), vec3<f32>(1.0));
            let expanded = vec3<f32>(k) + (1.0 - k) * t + (post.extra.w - 1.0) * t * t;
            c = select(c, expanded, c > vec3<f32>(k));
        }
        return vec4<f32>(c, 1.0);
    }
    let c = textureSampleLevel(src_tex, src_samp, uv, 0.0).rgb;
    let blur = tap4(uv);
    return vec4<f32>(mix(c, blur, 0.12), 1.0);
}