// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/front/gpu.rs, commit c9513623f), ADR-8012.
// Adapted (docs/ported-sources.toml): rounded boxes and outlines by signed distance, glyphs from
// paged texture arrays, colour glyphs, four image slots, and the sRGB encode moved from the
// composite pass into this one (the list draws straight into the frame).
//
// The 2D draw list (`regia_quick::draw2d`): one instanced quad per instance, coverage per pixel.
// Coordinates are physical pixels, origin top-left, +Y down. Colours are premultiplied linear;
// `u.encode` = 1 writes sRGB-encoded premultiplied values for an 8-bit unorm target.

struct U {
    size: vec2<f32>,
    encode: f32,
    pad: f32,
};

@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var mono_pages: texture_2d_array<f32>;
@group(0) @binding(3) var color_pages: texture_2d_array<f32>;
@group(0) @binding(4) var img0: texture_2d<f32>;
@group(0) @binding(5) var img1: texture_2d<f32>;
@group(0) @binding(6) var img2: texture_2d<f32>;
@group(0) @binding(7) var img3: texture_2d<f32>;

const KIND_BOX: f32 = 0.0;
const KIND_SEGMENT: f32 = 1.0;
const KIND_GLYPH: f32 = 2.0;
const KIND_COLOR_GLYPH: f32 = 3.0;
const KIND_IMAGE: f32 = 4.0;
const KIND_OUTLINE: f32 = 5.0;

struct In {
    @location(0) geo: vec4<f32>,
    @location(1) uv: vec4<f32>,
    @location(2) color: vec4<f32>,
    @location(3) clip: vec4<f32>,
    @location(4) params: vec4<f32>,
};

struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) geo: vec4<f32>,
    @location(2) @interpolate(flat) color: vec4<f32>,
    @location(3) @interpolate(flat) clip: vec4<f32>,
    @location(4) @interpolate(flat) params: vec4<f32>,
    @location(5) @interpolate(flat) uv4: vec4<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, i: In) -> Out {
    // Two triangles: (0,0) (1,0) (0,1) and (1,0) (1,1) (0,1), without a function-local array
    // (spirv-val's layout rule rejects one).
    let c = vec2<f32>(
        select(0.0, 1.0, vi == 1u || vi == 3u || vi == 4u),
        select(0.0, 1.0, vi == 2u || vi == 4u || vi == 5u));
    let kind = i.params.x;
    var p: vec2<f32>;
    if (kind == KIND_SEGMENT) {
        // An oriented quad: half width plus one pixel of anti-aliasing margin.
        let a = i.geo.xy;
        let b = i.geo.zw;
        let len = max(length(b - a), 0.0001);
        let d = (b - a) / len;
        let n = vec2<f32>(-d.y, d.x);
        let hw = i.params.y + 1.0;
        let ext = select(1.0, i.params.y + 1.0, i.params.z > 0.5);
        let along = mix(-ext, len + ext, c.x);
        let across = mix(-hw, hw, c.y);
        p = a + d * along + n * across;
    } else if (kind == KIND_BOX || kind == KIND_OUTLINE) {
        // Margin for the anti-aliased (or feathered) edge.
        let m = select(1.0, max(i.params.z, 1.0) * 0.5 + 1.0, kind == KIND_BOX);
        p = mix(i.geo.xy - vec2<f32>(m), i.geo.zw + vec2<f32>(m), c);
    } else {
        p = mix(i.geo.xy, i.geo.zw, c);
    }
    return Out(
        vec4<f32>(p.x / u.size.x * 2.0 - 1.0, 1.0 - p.y / u.size.y * 2.0, 0.0, 1.0),
        mix(i.uv.xy, i.uv.zw, c),
        i.geo,
        i.color,
        i.clip,
        i.params,
        i.uv,
    );
}

// Signed distance from `p` to a box with corners `geo` rounded by `radius` (negative inside).
fn box_distance(p: vec2<f32>, geo: vec4<f32>, radius: f32) -> f32 {
    let centre = (geo.xy + geo.zw) * 0.5;
    let half = abs(geo.zw - geo.xy) * 0.5;
    let r = min(radius, min(half.x, half.y));
    let q = abs(p - centre) - half + vec2<f32>(r);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

fn to_srgb(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

// Premultiplied linear in, the target's encoding out.
fn finish(c: vec4<f32>) -> vec4<f32> {
    if (u.encode < 0.5 || c.a <= 0.0) {
        return c;
    }
    let straight = clamp(c.rgb / c.a, vec3<f32>(0.0), vec3<f32>(1.0));
    return vec4<f32>(to_srgb(straight) * c.a, c.a);
}

@fragment
fn fs(o: Out) -> @location(0) vec4<f32> {
    let px = o.pos.xy;
    if (px.x < o.clip.x || px.y < o.clip.y || px.x >= o.clip.z || px.y >= o.clip.w) {
        discard;
    }
    let kind = o.params.x;
    var cov = 1.0;
    var color = o.color;
    if (kind == KIND_BOX) {
        let feather = max(o.params.z, 1.0);
        cov = clamp(0.5 - box_distance(px, o.geo, o.params.y) / feather, 0.0, 1.0);
        if (o.params.w > 0.5) {
            let size = max(o.geo.zw - o.geo.xy, vec2<f32>(0.0001));
            let t = clamp(select((px.y - o.geo.y) / size.y, (px.x - o.geo.x) / size.x, o.params.w > 1.5), 0.0, 1.0);
            color = mix(o.color, o.uv4, t);
        }
    } else if (kind == KIND_OUTLINE) {
        let d = box_distance(px, o.geo, o.params.y);
        cov = clamp(0.5 - d, 0.0, 1.0) - clamp(0.5 - (d + o.params.z), 0.0, 1.0);
    } else if (kind == KIND_SEGMENT) {
        let a = o.geo.xy;
        let b = o.geo.zw;
        let len = max(length(b - a), 0.0001);
        let dir = (b - a) / len;
        let along = dot(px - a, dir);
        let across = dot(px - a, vec2<f32>(-dir.y, dir.x));
        let hw = o.params.y;
        if (o.params.z > 0.5) {
            let t = clamp(along, 0.0, len);
            cov = clamp(hw - length(vec2<f32>(along - t, across)) + 0.5, 0.0, 1.0);
        } else {
            let side = clamp(hw - abs(across) + 0.5, 0.0, 1.0);
            let ends = clamp(along + 0.5, 0.0, 1.0) * clamp(len - along + 0.5, 0.0, 1.0);
            cov = side * ends;
        }
    } else if (kind == KIND_GLYPH) {
        cov = textureSampleLevel(mono_pages, samp, o.uv, i32(o.params.z), 0.0).r;
    } else if (kind == KIND_COLOR_GLYPH) {
        return finish(textureSampleLevel(color_pages, samp, o.uv, i32(o.params.z), 0.0) * o.color.a);
    } else if (kind == KIND_IMAGE) {
        var t: vec4<f32>;
        if (o.params.w < 0.5) {
            t = textureSampleLevel(img0, samp, o.uv, 0.0);
        } else if (o.params.w < 1.5) {
            t = textureSampleLevel(img1, samp, o.uv, 0.0);
        } else if (o.params.w < 2.5) {
            t = textureSampleLevel(img2, samp, o.uv, 0.0);
        } else {
            t = textureSampleLevel(img3, samp, o.uv, 0.0);
        }
        return finish(t * o.color);
    }
    return finish(color * cov);
}

// ---------------------------------------------------------------------------------------------
// Dirty regions (P-06): the list draws into a retained UI target (premultiplied, sRGB-encoded
// 8-bit storage), one scissored run of dirty tiles at a time. `fs_clear` empties a run before its
// instances redraw; `fs_composite` blends the UI target over the frame (premultiplied over), and
// `fs_composite_encode` writes sRGB-encoded values for an 8-bit unorm frame.

@group(0) @binding(8) var ui_layer: texture_2d<f32>;

@vertex
fn vs_full(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    // One triangle covering the target: (-1,-1) (3,-1) (-1,3).
    let x = select(-1.0, 3.0, vi == 1u);
    let y = select(-1.0, 3.0, vi == 2u);
    return vec4<f32>(x, y, 0.0, 1.0);
}

@fragment
fn fs_clear() -> @location(0) vec4<f32> {
    return vec4<f32>(0.0);
}

@fragment
fn fs_composite(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    return textureLoad(ui_layer, vec2<i32>(pos.xy), 0);
}

@fragment
fn fs_composite_encode(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let c = textureLoad(ui_layer, vec2<i32>(pos.xy), 0);
    if (c.a <= 0.0) {
        return c;
    }
    let straight = clamp(c.rgb / c.a, vec3<f32>(0.0), vec3<f32>(1.0));
    return vec4<f32>(to_srgb(straight) * c.a, c.a);
}
