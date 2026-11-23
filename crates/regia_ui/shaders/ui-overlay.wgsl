// SPDX-License-Identifier: MPL-2.0
//! UI overlay shader: one uber-shader drawing SDF rounded rects, borders, soft shadows,
//! gradients, glyph-atlas samples and images (`specs/18` § 3).
//!
//! The overlay runs after tonemapping, on the display-referred frame. Output is premultiplied
//! linear colour (blend: one, one-minus-src-alpha); `screen.output_mode` encodes it for the
//! target (hardware sRGB, manual sRGB, scRGB or PQ). Kinds: 0 rect, 1 mono glyph,
//! 2 colour glyph, 3 image, 4 gradient, 5 shadow, 6 backdrop blur, 7 glyph effect layer
//! (outline / shadow, ADR-2045), 8 UI material (ADR-2045), 9 distance-field glyph (ADR-2045),
//! 10 group layer mask and 12 group layer composite (ADR-2866; 11, the layer begin marker, is
//! never drawn).

struct ScreenUniforms {
    viewport: vec2<f32>,
    /// 0: sRGB target (hardware encode), 1: UNORM target (shader sRGB encode),
    /// 2: scRGB (linear, 1.0 = 80 nits), 3: HDR10 PQ (BT.2020).
    output_mode: f32,
    /// SDR paper white in nits for the HDR modes.
    paper_white: f32,
    /// Blend gamma: alpha remap emulating sRGB-space (CSS / design-tool) compositing; 1 = off.
    blend_gamma: f32,
    _pad: vec3<f32>,
};

@group(0) @binding(0) var<uniform> screen: ScreenUniforms;
@group(1) @binding(0) var mono_sampler: sampler;
@group(1) @binding(1) var mono_atlas: texture_2d_array<f32>;
@group(2) @binding(0) var color_sampler: sampler;
@group(2) @binding(1) var color_atlas: texture_2d_array<f32>;
@group(3) @binding(0) var image_sampler: sampler;
@group(3) @binding(1) var image_tex: texture_2d<f32>;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) @interpolate(flat) size: vec2<f32>,
    @location(3) @interpolate(flat) color: vec4<f32>,
    @location(4) @interpolate(flat) color2: vec4<f32>,
    @location(5) @interpolate(flat) radii: vec4<f32>,
    @location(6) @interpolate(flat) borders: vec4<f32>,
    @location(7) @interpolate(flat) params: vec4<f32>, // kind, layer|margin, angle|font px, blur sigma
    @location(8) @interpolate(flat) uv_rect: vec4<f32>,
};

struct Instance {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) uv: vec4<f32>,
    @location(3) color: vec4<f32>,
    @location(4) color2: vec4<f32>,
    @location(5) corner_radius: vec4<f32>,
    @location(6) border_width: vec4<f32>,
    @location(7) kind_layer: vec4<f32>,
    @location(8) clip: vec4<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: Instance) -> VertexOut {
    // Triangle-strip order: TL, TR, BL, BR → two triangles covering the full quad.
    // Selects, not a function-local array or struct (ADR-026).
    let c = vec2<f32>(
        select(0.0, 1.0, vi == 1u || vi == 3u),
        select(0.0, 1.0, vi >= 2u),
    );
    // Draw only the part of the quad inside its clip rect; the SDF frame (`local`) and the uv
    // mapping stay those of the full quad, so clipped rounded panels keep their corners.
    let lo = max(inst.pos, inst.clip.xy);
    let hi = max(min(inst.pos + inst.size, inst.clip.zw), lo);
    let px = mix(lo, hi, c);
    let local = px - inst.pos;
    let t = local / max(inst.size, vec2<f32>(1e-4));
    // Physical px → NDC. UI space is y-down; NDC is y-up.
    let ndc = vec2<f32>(
        px.x / screen.viewport.x * 2.0 - 1.0,
        1.0 - px.y / screen.viewport.y * 2.0,
    );
    return VertexOut(
        vec4<f32>(ndc, 0.0, 1.0),
        local,
        mix(inst.uv.xy, inst.uv.zw, t),
        inst.size,
        inst.color,
        inst.color2,
        inst.corner_radius,
        inst.border_width,
        inst.kind_layer,
        inst.uv,
    );
}

/// Signed distance to a rounded box centred at the origin. `radii` = tl, tr, br, bl.
fn rounded_box_sdf(p: vec2<f32>, half: vec2<f32>, radii: vec4<f32>) -> f32 {
    let r_max = max(min(half.x, half.y), 0.0);
    let r4 = clamp(radii, vec4<f32>(0.0), vec4<f32>(r_max));
    let r = select(
        select(r4.y, r4.x, p.x < 0.0),
        select(r4.z, r4.w, p.x < 0.0),
        p.y > 0.0,
    );
    let q = abs(p) - (half - vec2<f32>(r));
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

/// Pixel coverage of the inside (d < 0) of a distance field in pixel units: a one-pixel box
/// filter across the edge (axis-aligned UI shapes, no rotation).
fn coverage(d: f32) -> f32 {
    return clamp(0.5 - d, 0.0, 1.0);
}

fn premul(c: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(c.rgb * c.a, c.a);
}

fn srgb_encode(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

fn srgb_decode(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((max(c, vec3<f32>(0.0)) + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

/// SMPTE ST 2084 (PQ) inverse EOTF; `y` is luminance / 10 000 nits.
fn pq_encode(y: vec3<f32>) -> vec3<f32> {
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let p = pow(clamp(y, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(m1));
    return pow((c1 + c2 * p) / (1.0 + c3 * p), vec3<f32>(m2));
}

/// Encodes premultiplied linear (display-referred, SDR) colour for the target.
fn encode_output(c: vec4<f32>) -> vec4<f32> {
    let mode = u32(screen.output_mode + 0.5);
    if (mode == 0u || c.a <= 0.0) {
        return c;
    }
    let straight = c.rgb / c.a;
    var rgb = straight;
    if (mode == 1u) {
        rgb = srgb_encode(straight);
    } else if (mode == 2u) {
        rgb = straight * (screen.paper_white / 80.0);
    } else {
        // BT.709 → BT.2020 primaries (ITU-R BT.2087), then PQ at paper white.
        let r2020 = mat3x3<f32>(
            vec3<f32>(0.6274, 0.0691, 0.0164),
            vec3<f32>(0.3293, 0.9195, 0.0880),
            vec3<f32>(0.0433, 0.0114, 0.8956),
        ) * straight;
        rgb = pq_encode(r2020 * (screen.paper_white / 10000.0));
    }
    return vec4<f32>(rgb * c.a, c.a);
}

/// Interleaved gradient noise (Jimenez 2014), in [0, 1).
fn ign(p: vec2<f32>) -> f32 {
    return fract(52.9829189 * fract(dot(p, vec2<f32>(0.06711056, 0.00583715))));
}

/// Adds ±0.5 LSB of 8-bit sRGB noise to a premultiplied linear colour (gradients and shadows
/// otherwise band on 8-bit targets).
fn dither(c: vec4<f32>, frag: vec2<f32>) -> vec4<f32> {
    if (c.a <= 0.0) {
        return c;
    }
    let n = (ign(frag) - 0.5) / 255.0;
    let s = srgb_encode(c.rgb / c.a) + vec3<f32>(n);
    let a = clamp(c.a + n, 0.0, 1.0);
    return vec4<f32>(srgb_decode(clamp(s, vec3<f32>(0.0), vec3<f32>(1.0))) * a, a);
}

/// Abramowitz & Stegun 7.1.27 rational approximation of erf (|error| < 5e-4).
fn erf2(x: vec2<f32>) -> vec2<f32> {
    let s = sign(x);
    let a = abs(x);
    var y = 1.0 + (0.278393 + (0.230389 + (0.000972 + 0.078108 * a) * a) * a) * a;
    y = y * y;
    return s - s / (y * y);
}

/// Horizontal integral of a Gaussian (σ) across the row `y` of a rounded box (half size `h`,
/// corner `r`), evaluated at `x`. All coordinates relative to the box centre.
fn shadow_row(x: f32, y: f32, sigma: f32, r: f32, h: vec2<f32>) -> f32 {
    let dy = max(abs(y) - (h.y - r), 0.0);
    let extent = h.x - r + sqrt(max(r * r - dy * dy, 0.0));
    let e = erf2((vec2<f32>(x) + vec2<f32>(extent, -extent)) * (0.70710678 / sigma));
    return 0.5 * (e.x - e.y);
}

/// Coverage of a rounded box convolved with an isotropic Gaussian of std-dev `sigma`: exact erf
/// along x, a 6-tap Gaussian-weighted sum along y over the box's support (Wallace, "Fast Rounded
/// Rectangle Shadows").
fn rounded_box_shadow(p: vec2<f32>, h: vec2<f32>, r: f32, sigma: f32) -> f32 {
    let lo = clamp(-3.0 * sigma, p.y - h.y, p.y + h.y);
    let hi = clamp(3.0 * sigma, p.y - h.y, p.y + h.y);
    let step = (hi - lo) / 6.0;
    var y = lo + step * 0.5;
    var sum = 0.0;
    for (var i = 0; i < 6; i = i + 1) {
        let g = exp(-(y * y) / (2.0 * sigma * sigma)) / (2.50662827 * sigma);
        sum = sum + shadow_row(p.x, p.y - y, sigma, r, h) * g * step;
        y = y + step;
    }
    return clamp(sum, 0.0, 1.0);
}

/// Remaps alpha so linear-light blending reproduces sRGB-space ("gamma") compositing — what
/// CSS, design tools and native text rasterizers show, and what `#rrggbbaa` colours are
/// authored against. Over a background B, gamma-space compositing of colour F at alpha a gives
/// lin(a·srgb(F) + (1-a)·srgb(B)). With a pure power curve γ, a black F gives (1-a)^γ·B, i.e.
/// linear blending with a' = 1 - (1-a)^γ for any B, and a white F over black gives a^γ.
/// Between the two the remap is weighted by F's perceived lightness (the background is unknown
/// in the shader). Without it dark translucent panels wash out over bright scenes, dark text
/// thins and light text bolds.
fn blend_alpha(a: f32, color: vec3<f32>) -> f32 {
    let g = screen.blend_gamma;
    if (g <= 1.0 || a <= 0.0 || a >= 1.0) {
        return a;
    }
    let lum = dot(color, vec3<f32>(0.2126, 0.7152, 0.0722));
    let lightness = srgb_encode(vec3<f32>(lum)).x;
    let light = pow(a, g);
    let dark = 1.0 - pow(1.0 - a, g);
    return mix(dark, light, lightness);
}

/// Final step for every kind: blend-alpha remap of the premultiplied result, optional dither,
/// output encoding.
fn finish(c: vec4<f32>, frag: vec2<f32>, dithered: bool) -> vec4<f32> {
    if (c.a <= 0.0) {
        return vec4<f32>(0.0);
    }
    let straight = c.rgb / c.a;
    let a = blend_alpha(c.a, straight);
    var out = vec4<f32>(straight * a, a);
    if (dithered) {
        out = dither(out, frag);
    }
    return encode_output(out);
}

/// Text effect layer (kind 7, ADR-2045): the glyph's atlas coverage dilated (outline) or
/// blurred (shadow). `radii` is the glyph's own uv rect (taps outside it read 0, so atlas
/// neighbors never bleed in), `borders.x` the outline width and `borders.y` the blur, in px.
/// The quad is the glyph grown by the reach on each side, so `in.uv` walks texel centers 1:1.
/// Mirrors `regia_ui::text_effects::effect_coverage`.
fn glyph_effect(in: VertexOut) -> vec4<f32> {
    let layer = u32(in.params.y + 0.5);
    let texel = 1.0 / vec2<f32>(textureDimensions(mono_atlas).xy);
    let width = in.borders.x;
    let blur = in.borders.y;
    let blur_only = width <= 0.0 && blur > 0.0;
    let reach = i32(min(ceil(select(width + blur, blur * 1.5, width <= 0.0)), 8.0));
    let sigma = max(blur * 0.5, 1e-3);
    var best = 0.0;
    var acc = 0.0;
    var total = 0.0;
    for (var dy = -reach; dy <= reach; dy = dy + 1) {
        for (var dx = -reach; dx <= reach; dx = dx + 1) {
            let o = vec2<f32>(f32(dx), f32(dy));
            let uv = in.uv + o * texel;
            let inside = all(uv >= in.radii.xy) && all(uv <= in.radii.zw);
            var a = 0.0;
            if (inside) {
                a = textureSampleLevel(mono_atlas, mono_sampler, uv, layer, 0.0).r;
            }
            let d = length(o);
            if (blur_only) {
                let w = exp(-(d * d) / (2.0 * sigma * sigma));
                acc = acc + a * w;
                total = total + w;
            } else {
                let k = clamp((width + blur + 1.0 - d) / (blur + 1.0), 0.0, 1.0);
                best = max(best, a * k);
            }
        }
    }
    var cov = best;
    if (blur_only) {
        cov = clamp(acc / max(total, 1e-6), 0.0, 1.0);
    }
    let alpha = cov * in.color.a;
    return vec4<f32>(in.color.rgb * alpha, alpha);
}

/// Hash of a 2D cell, in [0, 1) (integer-free value noise for the dissolve material).
fn hash21(p: vec2<f32>) -> f32 {
    return fract(sin(dot(p, vec2<f32>(127.1, 311.7))) * 43758.5453);
}

/// Bilinear value noise over unit cells.
fn value_noise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = hash21(i);
    let b = hash21(i + vec2<f32>(1.0, 0.0));
    let c = hash21(i + vec2<f32>(0.0, 1.0));
    let d = hash21(i + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

/// Procedural UI material (kind 8, ADR-2045), premultiplied. `params.w` = material kind,
/// `params.z` = time (s), `borders` = the four material parameters, `color`/`color2` its two
/// colors. Mirrors `regia_ui::ui_material::UiMaterialKind`; `p` is relative to the quad centre.
fn ui_material(in: VertexOut, p: vec2<f32>) -> vec4<f32> {
    let kind = u32(in.params.w + 0.5);
    let t = in.params.z;
    let k = in.borders;
    let uv = in.local / max(in.size, vec2<f32>(1e-4));
    var mixf = 0.0;
    var alpha_mul = 1.0;
    if (kind == 0u) {
        // Shine: a soft diagonal band sweeping left to right. k.x speed (sweeps/s), k.y band
        // width (fraction), k.z angle (degrees), k.w pause fraction of each cycle.
        let ang = radians(k.z);
        let dir = vec2<f32>(cos(ang), sin(ang));
        let extent = abs(dir.x) * in.size.x + abs(dir.y) * in.size.y;
        let s = dot(p, dir) / max(extent, 1e-4) + 0.5;
        let cycle = fract(t * k.x) * (1.0 + k.w);
        let pos = cycle * (1.0 + 2.0 * k.y) - k.y;
        mixf = clamp(1.0 - abs(s - pos) / max(k.y, 1e-3), 0.0, 1.0);
        mixf = mixf * mixf * (3.0 - 2.0 * mixf);
    } else if (kind == 1u) {
        // Radial wipe: color2 fills the sector from 12 o'clock clockwise to k.x (0..1); the rest
        // is color. k.y edge softness in radians.
        let a = atan2(p.x, -p.y);
        let frac = fract(a / 6.28318530718 + 1.0);
        let soft = max(k.y, 1e-4);
        mixf = clamp((k.x - frac) / soft * 6.28318530718 + 0.5, 0.0, 1.0);
        if (k.x >= 1.0) {
            mixf = 1.0;
        }
    } else if (kind == 2u) {
        // Dissolve: value noise of cell size k.y px against threshold k.x; k.z edge glow width.
        // Pixels under the threshold vanish, the edge band takes color2.
        let n = value_noise(in.local / max(k.y, 1.0));
        let edge = max(k.z, 1e-4);
        alpha_mul = select(0.0, 1.0, n >= k.x);
        mixf = clamp(1.0 - (n - k.x) / edge, 0.0, 1.0) * alpha_mul;
    } else if (kind == 3u) {
        // Scanlines: horizontal bands every k.x px scrolling at k.y px/s, strength k.z.
        let period = max(k.x, 1.0);
        let phase = fract((in.local.y + t * k.y) / period);
        mixf = clamp(k.z, 0.0, 1.0) * (0.5 - 0.5 * cos(phase * 6.28318530718));
    } else {
        // Pulse: color → color2 → color with period k.x seconds.
        mixf = 0.5 - 0.5 * cos(t * 6.28318530718 / max(k.x, 1e-3));
    }
    let c = mix(premul(in.color), premul(in.color2), mixf);
    return c * alpha_mul;
}

@fragment
fn fs(in: VertexOut) -> @location(0) vec4<f32> {
    let half_ = in.size * 0.5;
    let kind = u32(in.params.x + 0.5);
    let p = in.local - half_;
    let has_radius = max(max(in.radii.x, in.radii.y), max(in.radii.z, in.radii.w)) > 0.0;

    if (kind == 12u) {
        // Group layer composite (ADR-2866): the layer's texels 1:1 from the quad's top-left
        // (whole-pixel origin), premultiplied linear, times the group opacity.
        let dims = vec2<f32>(textureDimensions(image_tex));
        let t = textureSampleLevel(image_tex, image_sampler, in.local / dims, 0.0);
        return finish(t * in.color.a, in.position.xy, false);
    }
    if (kind == 10u) {
        // Group layer mask (ADR-2866), drawn destination-in: the layer keeps `m` of itself.
        // `borders` = masked rect (x0, y0, x1, y1), `color2` = placed image rect, both layer
        // space; flags 1 image, 2 tile, 4 rounded.
        let flags = u32(in.params.y + 0.5);
        var m = 1.0;
        let lo = in.borders.xy;
        let hi = in.borders.zw;
        if ((flags & 4u) != 0u) {
            m = m * coverage(rounded_box_sdf(in.local - (lo + hi) * 0.5, (hi - lo) * 0.5, in.radii));
        } else {
            let inside_rect = all(in.local >= lo) && all(in.local < hi);
            m = select(0.0, m, inside_rect);
        }
        if ((flags & 1u) != 0u) {
            let ilo = in.color2.xy;
            let span = max(in.color2.zw - ilo, vec2<f32>(1e-4));
            var t = (in.local - ilo) / span;
            let tile = (flags & 2u) != 0u;
            let inside = all(t >= vec2<f32>(0.0)) && all(t < vec2<f32>(1.0));
            if (tile) {
                t = fract(t);
            }
            let uv = mix(in.uv_rect.xy, in.uv_rect.zw, t);
            let duv = (in.uv_rect.zw - in.uv_rect.xy) / span;
            let a = textureSampleGrad(image_tex, image_sampler, uv, vec2<f32>(duv.x, 0.0), vec2<f32>(0.0, duv.y)).a;
            m = m * select(0.0, a, inside || tile);
        }
        return vec4<f32>(m);
    }

    if (kind == 1u) {
        let a = textureSampleLevel(mono_atlas, mono_sampler, in.uv, u32(in.params.y + 0.5), 0.0).r;
        let alpha = a * in.color.a;
        return finish(vec4<f32>(in.color.rgb * alpha, alpha), in.position.xy, false);
    }
    if (kind == 9u) {
        // Distance-field glyph: the field's 0.5 level is the edge; `borders.x` screen px per
        // reference px, `borders.y` the encoded spread (reference px). Mirrors
        // `regia_ui::sdf_text::sdf_coverage`.
        let v = textureSampleLevel(mono_atlas, mono_sampler, in.uv, u32(in.params.y + 0.5), 0.0).r;
        let d = (v - 0.5) * 2.0 * in.borders.y * in.borders.x;
        let alpha = clamp(d + 0.5, 0.0, 1.0) * in.color.a;
        return finish(vec4<f32>(in.color.rgb * alpha, alpha), in.position.xy, false);
    }
    if (kind == 7u) {
        return finish(glyph_effect(in), in.position.xy, true);
    }
    if (kind == 8u) {
        var m = ui_material(in, p);
        if (has_radius) {
            m = m * coverage(rounded_box_sdf(p, half_, in.radii));
        }
        return finish(m, in.position.xy, true);
    }
    if (kind == 2u) {
        // Colour glyph (emoji): premultiplied bitmap, instance alpha.
        let t = textureSampleLevel(color_atlas, color_sampler, in.uv, u32(in.params.y + 0.5), 0.0);
        return finish(t * in.color.a, in.position.xy, false);
    }
    if (kind == 3u) {
        // Image (premultiplied, mipmapped), tinted; explicit gradients pick the mip without
        // derivative ops (flat-branch control flow is non-uniform).
        let duv = (in.uv_rect.zw - in.uv_rect.xy) / max(in.size, vec2<f32>(1e-4));
        let t = textureSampleGrad(image_tex, image_sampler, in.uv, vec2<f32>(duv.x, 0.0), vec2<f32>(0.0, duv.y));
        var out = t * premul(in.color);
        if (has_radius) {
            out = out * coverage(rounded_box_sdf(p, half_, in.radii));
        }
        return finish(out, in.position.xy, false);
    }

    if (kind == 6u) {
        // Backdrop blur: the blurred frame (group 3, target space already, so no output
        // encode) through the rounded rect, faded by the instance alpha.
        let cov = coverage(rounded_box_sdf(p, half_, in.radii)) * in.color.a;
        let t = textureSampleLevel(image_tex, image_sampler, in.position.xy / screen.viewport, 0.0);
        return vec4<f32>(t.rgb * cov, cov);
    }

    if (kind == 5u) {
        // Soft shadow: Gaussian-blurred caster box, clipped out under the caster itself (CSS
        // box-shadow semantics, so translucent panels are not darkened from below).
        let margin = in.params.y;
        let sigma = in.params.w;
        let shape_half = max(half_ - vec2<f32>(margin), vec2<f32>(0.0));
        let r = min(in.radii.x, min(shape_half.x, shape_half.y));
        var s = 0.0;
        if (sigma < 0.25) {
            s = coverage(rounded_box_sdf(p, shape_half, vec4<f32>(r)));
        } else {
            s = rounded_box_shadow(p, shape_half, r, sigma);
        }
        let caster = coverage(rounded_box_sdf(p + in.uv_rect.xy, shape_half, vec4<f32>(r)));
        let a = in.color.a * s * (1.0 - caster);
        return finish(vec4<f32>(in.color.rgb * a, a), in.position.xy, true);
    }

    // Fill (premultiplied).
    var fill = premul(in.color);
    if (kind == 4u) {
        // Linear gradient `color`→`color2`: 0° = top→bottom, 90° = left→right.
        let angle = radians(in.params.z);
        let dir = vec2<f32>(sin(angle), cos(angle));
        let extent = abs(dir.x) * in.size.x + abs(dir.y) * in.size.y;
        let t = clamp(dot(p, dir) / max(extent, 1e-4) + 0.5, 0.0, 1.0);
        // Interpolate premultiplied sRGB-encoded colour, like CSS and design tools (a linear-
        // light ramp from black reaches mid grey a third of the way along).
        let a0 = vec4<f32>(srgb_encode(in.color.rgb) * in.color.a, in.color.a);
        let a1 = vec4<f32>(srgb_encode(in.color2.rgb) * in.color2.a, in.color2.a);
        let m = mix(a0, a1, t);
        if (m.a > 0.0) {
            fill = vec4<f32>(srgb_decode(m.rgb / m.a) * m.a, m.a);
        } else {
            fill = vec4<f32>(0.0);
        }
    }
    let outer = coverage(rounded_box_sdf(p, half_, in.radii));
    var out = fill * outer;

    // Border band: outer shape minus the shape inset by each side's own width.
    let bw = in.borders;
    if (max(max(bw.x, bw.y), max(bw.z, bw.w)) > 0.0) {
        let inner_min = bw.xy;
        let inner_max = in.size - bw.zw;
        let inner_half = (inner_max - inner_min) * 0.5;
        var inner = 0.0;
        if (inner_half.x > 0.0 && inner_half.y > 0.0) {
            let inner_r = max(
                in.radii - vec4<f32>(max(bw.x, bw.y), max(bw.z, bw.y), max(bw.z, bw.w), max(bw.x, bw.w)),
                vec4<f32>(0.0),
            );
            inner = coverage(rounded_box_sdf(in.local - (inner_min + inner_max) * 0.5, inner_half, inner_r));
        }
        let band = outer * (1.0 - inner);
        let border = premul(in.color2) * band;
        out = border + out * (1.0 - border.a);
    }
    return finish(out, in.position.xy, kind == 4u);
}

// ---- Backdrop blur (ADR-1836) ----------------------------------------------------------
// Separable Gaussian over a copy of the frame; `fs` kind 6 then composites the result through
// a quad's rounded rect. The blur reads group 3 (the source image) and its own uniform.

struct BlurUniforms {
    /// UV step between taps (one axis is zero).
    step: vec2<f32>,
    /// Gaussian σ in tap units.
    sigma: f32,
    /// Taps on each side of the centre.
    taps: f32,
};

@group(0) @binding(1) var<uniform> blur: BlurUniforms;

struct BlurOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_blur(@builtin(vertex_index) vi: u32) -> BlurOut {
    // One oversized triangle; selects, not an array (ADR-026).
    let x = select(-1.0, 3.0, vi == 1u);
    let y = select(-1.0, 3.0, vi == 2u);
    return BlurOut(vec4<f32>(x, y, 0.0, 1.0), vec2<f32>((x + 1.0) * 0.5, 1.0 - (y + 1.0) * 0.5));
}

@fragment
fn fs_blur(in: BlurOut) -> @location(0) vec4<f32> {
    var acc = vec4<f32>(0.0);
    var total = 0.0;
    let n = i32(blur.taps);
    let inv = 1.0 / (2.0 * blur.sigma * blur.sigma);
    for (var i = -n; i <= n; i = i + 1) {
        let f = f32(i);
        let w = exp(-f * f * inv);
        acc = acc + textureSampleLevel(image_tex, image_sampler, in.uv + blur.step * f, 0.0) * w;
        total = total + w;
    }
    return acc / total;
}

// Box prefilter of the largest downsample (factor 4): four bilinear taps one source texel off the
// centre read the 4x4 block under the downsampled texel, so a thin line cannot fall between taps.
@fragment
fn fs_blur_down(in: BlurOut) -> @location(0) vec4<f32> {
    let dx = vec2<f32>(blur.step.x, 0.0);
    let dy = vec2<f32>(0.0, blur.step.y);
    let a = textureSampleLevel(image_tex, image_sampler, in.uv - dx - dy, 0.0);
    let b = textureSampleLevel(image_tex, image_sampler, in.uv + dx - dy, 0.0);
    let c = textureSampleLevel(image_tex, image_sampler, in.uv - dx + dy, 0.0);
    let d = textureSampleLevel(image_tex, image_sampler, in.uv + dx + dy, 0.0);
    return (a + b + c + d) * 0.25;
}
