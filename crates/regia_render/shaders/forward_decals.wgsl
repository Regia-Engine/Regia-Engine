// SPDX-License-Identifier: MPL-2.0
#define_import_path regia::forward::decals

// Forward decals (`regia_render::forward_decals`, ADR-3255): the pure helpers of the gather in
// `lab_forward.wgsl` (`shade_forward`, before lighting). The bindings and the loop over the tile
// mask live in `lab_forward.wgsl`; these functions take plain values, so the module carries no
// binding and any shader may import it.

// Hermite smoothstep with the decal pass's degenerate-range rule.
fn fd_smooth(e0: f32, e1: f32, x: f32) -> f32 {
    if (e1 <= e0) {
        return select(1.0, 0.0, x < e0);
    }
    let t = clamp((x - e0) / (e1 - e0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

// IEC 61966-2-1 decode of an atlas albedo byte triple (the atlas is a linear `Rgba8Unorm` array,
// so its normal and material pages read raw; the albedo page holds sRGB-encoded bytes).
fn fd_srgb_decode(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / vec3<f32>(1.055), vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

// The weight of one decal at a surface point inside its box: the lifetime opacity, the grazing
// fade on |n · projection axis|, the box-edge feather and the camera-distance fade (the same
// terms as the projected decal pass and `regia_pt::PtDecal`). `local` is the point in the box's
// unit frame, `fades` = (angle start, angle end, distance start, distance end).
fn fd_weight(opacity: f32, local: vec3<f32>, facing: f32, edge_softness: f32, fades: vec4<f32>, distance: f32) -> f32 {
    var grazing = 1.0;
    if (fades.y > fades.x) {
        grazing = fd_smooth(fades.x, fades.y, facing);
    }
    let feather = (1.0 - fd_smooth(0.7, 1.0, abs(local.x))) * (1.0 - fd_smooth(0.7, 1.0, abs(local.y)));
    let edge = mix(1.0, feather, clamp(edge_softness, 0.0, 1.0));
    var dist_fade = 1.0;
    if (fades.w > fades.z) {
        dist_fade = 1.0 - fd_smooth(fades.z, fades.w, distance);
    }
    return opacity * grazing * edge * dist_fade;
}

// The decal's tangent-space normal `nts` (unit, +Z out of the surface) in the receiver frame:
// t = the decal's right axis projected on the receiver normal `n` (its up axis when the right
// axis is along `n`), b = n × t. Whiteout over a flat base (Barré-Brisebois and Hill, "Blending
// in Detail", 2012) reduces to the decal's own normal in that frame.
fn fd_to_receiver(n: vec3<f32>, right: vec3<f32>, up: vec3<f32>, nts: vec3<f32>) -> vec3<f32> {
    var t = right - n * dot(right, n);
    if (dot(t, t) < 1e-6) {
        t = up - n * dot(up, n);
    }
    t = normalize(t);
    let b = cross(n, t);
    return normalize(t * nts.x + b * nts.y + n * nts.z);
}

// Whiteout blend of two tangent-space normals (the atlas page's, then the authored one).
fn fd_whiteout(n1: vec3<f32>, n2: vec3<f32>) -> vec3<f32> {
    return normalize(vec3<f32>(n1.xy + n2.xy, n1.z * n2.z));
}

// A tangent-space normal from its xy (z reconstructed, clamped to the hemisphere).
fn fd_from_xy(xy: vec2<f32>) -> vec3<f32> {
    let c = clamp(xy, vec2<f32>(-1.0), vec2<f32>(1.0));
    return vec3<f32>(c, sqrt(max(0.0, 1.0 - dot(c, c))));
}

// Whether the tile masks in the buffer were built for this view: the same eye, view axis and size
// (`view_a` = eye, width; `view_b` = axis, height). Another view of the frame tests every decal.
fn fd_view_matches(view_a: vec4<f32>, view_b: vec4<f32>, eye: vec3<f32>, axis: vec3<f32>, size: vec2<f32>) -> bool {
    let d_eye = abs(view_a.xyz - eye);
    let d_axis = abs(view_b.xyz - axis);
    return max(d_eye.x, max(d_eye.y, d_eye.z)) < 1e-3
        && max(d_axis.x, max(d_axis.y, d_axis.z)) < 1e-4
        && abs(view_a.w - size.x) < 0.5
        && abs(view_b.w - size.y) < 0.5;
}

// The all-decals mask of `count` decals (the fallback when the tiles are for another view).
fn fd_all_mask(count: u32) -> vec2<u32> {
    let lo = select((1u << min(count, 31u)) - 1u, 0xffffffffu, count >= 32u);
    let hi_n = select(0u, count - 32u, count > 32u);
    let hi = select((1u << min(hi_n, 31u)) - 1u, 0xffffffffu, hi_n >= 32u);
    return vec2<u32>(lo, hi);
}
