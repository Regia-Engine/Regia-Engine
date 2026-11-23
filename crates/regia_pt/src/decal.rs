// SPDX-License-Identifier: MPL-2.0
//! Projected decals in the reference trace (ADR-2951, `specs/27` § 2.2 "Decals").
//!
//! The frame draws a projected decal as an oriented box over the depth buffer (`regia_vfx`'s decal
//! pass): a surface point inside the box takes the decal's albedo (modulated or replaced), its
//! normal (a bump from the atlas luma along the decal's axes, then the authored tangent-space
//! normal by whiteout blending), its roughness and its emission, weighted by the opacity, the
//! atlas coverage, the box-edge feather, the grazing-angle fade and the camera-distance fade.
//! The tracer applies the same projection and weights to the hit's material inputs instead of to
//! the lit colour, so the decal is lit by the traced light and its normal and roughness reach
//! the `normal` and `roughness` AOVs.
//!
//! [`PtDecal`] carries the fields of `regia_vfx`'s `Decal` and `DecalSurface` the frame's shader
//! reads (the tracer does not depend on `regia_vfx`; a host copies them, see
//! `book/agents/decals.md`). A scene without decals packs nothing and traces with the base
//! kernels, so its image is unchanged bit for bit; a scene with decals composes the decal code
//! into its own kernel set (cached like the material-graph sets).

use regia_core::glam::{Quat, Vec3};

/// Material channel: replace or modulate the albedo (`regia_vfx::CHANNEL_ALBEDO`).
pub const CHANNEL_ALBEDO: u32 = 1;
/// Material channel: the decal's normal (`regia_vfx::CHANNEL_NORMAL`).
pub const CHANNEL_NORMAL: u32 = 1 << 1;
/// Material channel: the decal's roughness (`regia_vfx::CHANNEL_ROUGHNESS`).
pub const CHANNEL_ROUGHNESS: u32 = 1 << 2;
/// Material channel: the decal's emission (`regia_vfx::CHANNEL_EMISSIVE`).
pub const CHANNEL_EMISSIVE: u32 = 1 << 3;

/// Words of one packed decal in the tracer's word buffer.
pub(crate) const DECAL_WORDS: usize = 36;

/// First line of the decal function region in `pt.wgsl` (module scope).
pub(crate) const FNS_BEGIN: &str = "// @pt_decal_fns_begin";
/// Last line of the decal function region.
pub(crate) const FNS_END: &str = "// @pt_decal_fns_end";
/// First line of the decal call region in `pt.wgsl` (the hit's surface inputs).
pub(crate) const APPLY_BEGIN: &str = "// @pt_decal_apply_begin";
/// Last line of the decal call region.
pub(crate) const APPLY_END: &str = "// @pt_decal_apply_end";
/// First line of the decal emission region in `pt.wgsl` (after the shading is built).
pub(crate) const EMIT_BEGIN: &str = "// @pt_decal_emit_begin";
/// Last line of the decal emission region.
pub(crate) const EMIT_END: &str = "// @pt_decal_emit_end";

/// One projected decal: an oriented box and the surface it writes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PtDecal {
    /// Centre of the box, world meters.
    pub center: [f32; 3],
    /// Orientation: local +X right, +Y up, +Z the projection axis (into the surface).
    pub rotation: [f32; 4],
    /// Half extents `(right, up, depth)`, meters.
    pub half_size: [f32; 3],
    /// Linear albedo.
    pub albedo: [f32; 3],
    /// Opacity, the lifetime fade already multiplied in (`Decal::fade() × DecalSurface::opacity`).
    pub opacity: f32,
    /// Perceptual roughness written with [`CHANNEL_ROUGHNESS`].
    pub roughness: f32,
    /// Tangent-space normal xy (z reconstructed); zero is flat.
    pub normal_xy: [f32; 2],
    /// Strength of the decal's normal (the atlas bump's scale and the normal's weight), `0..=1`.
    pub normal_strength: f32,
    /// Linear emission added with [`CHANNEL_EMISSIVE`] (cooled already), in scene radiance like
    /// a material's emission. The frame's decal pass adds `DecalSurface::emissive` in exposed
    /// light (after the lit colour is multiplied by the exposure), so a host copying it to match
    /// the frame divides it by that frame's exposure.
    pub emissive: [f32; 3],
    /// `smoothstep` range on `|dot(receiver normal, projection axis)|`; `end <= start` disables it.
    pub angle_fade: [f32; 2],
    /// Camera distance range (meters) over which the decal fades out; `end <= start` disables it.
    pub distance_fade: [f32; 2],
    /// Draw order: a higher priority applies later (on top).
    pub priority: i16,
    /// Receiver layer bits (`1` terrain, `2` static mesh); `0` and `u32::MAX` accept every layer.
    /// Every tracer mesh is a static receiver.
    pub receiver_mask: u32,
    /// Index into [`crate::PtScene::textures`] of the decal's atlas cell (sRGB colour, coverage in
    /// alpha, sampled clamped over the whole image); `None` = a solid rectangle.
    pub atlas: Option<usize>,
    /// [`CHANNEL_ALBEDO`] | [`CHANNEL_NORMAL`] | [`CHANNEL_ROUGHNESS`] | [`CHANNEL_EMISSIVE`].
    pub channels: u32,
    /// `0` hard box edge, `1` the smoothstep feather over the outer 30 %.
    pub edge_softness: f32,
    /// `0` modulates the albedo, `1` replaces it.
    pub blend: f32,
}

impl PtDecal {
    /// A decal box with `regia_vfx::DecalSurface::from_decal`'s defaults: albedo and opacity
    /// from `color`, roughness 1, flat normal, angle fade `0.25..0.6`, no distance fade, every
    /// layer, no atlas, the albedo channel, modulated, feathered.
    #[must_use]
    pub fn from_box(center: Vec3, rotation: Quat, half_size: Vec3, color: [f32; 4]) -> Self {
        Self {
            center: center.to_array(),
            rotation: rotation.to_array(),
            half_size: half_size.to_array(),
            albedo: [color[0], color[1], color[2]],
            opacity: color[3],
            roughness: 1.0,
            normal_xy: [0.0, 0.0],
            normal_strength: 0.0,
            emissive: [0.0; 3],
            angle_fade: [0.25, 0.6],
            distance_fade: [0.0, 0.0],
            priority: 0,
            receiver_mask: u32::MAX,
            atlas: None,
            channels: CHANNEL_ALBEDO,
            edge_softness: 1.0,
            blend: 0.0,
        }
    }

    /// The box axes `(right, up, forward)` (unit vectors).
    #[must_use]
    pub fn axes(&self) -> (Vec3, Vec3, Vec3) {
        let q = Quat::from_array(self.rotation).normalize();
        (q * Vec3::X, q * Vec3::Y, q * Vec3::Z)
    }
}

impl PtDecal {
    /// Whether the decal can be packed: every number finite, a rotation of non-zero length and
    /// a box of positive size. A non-finite box test is false on every comparison, so such a
    /// decal would apply everywhere with a NaN weight and black out the whole trace.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        let finite = |v: &[f32]| v.iter().all(|x| x.is_finite());
        let q = Quat::from_array(self.rotation);
        finite(&self.center)
            && finite(&self.rotation)
            && finite(&self.half_size)
            && finite(&self.albedo)
            && finite(&[self.opacity, self.roughness, self.normal_strength])
            && finite(&self.normal_xy)
            && finite(&self.emissive)
            && finite(&self.angle_fade)
            && finite(&self.distance_fade)
            && finite(&[self.edge_softness, self.blend])
            && q.length_squared() > 1e-12
            && self.half_size.iter().all(|h| *h > 0.0)
    }
}

/// The packed words of `decals`, in application order (ascending priority, stable); the caller
/// appends the trailer (`push_trailer`) with `words.len() / DECAL_WORDS` as the count.
/// Decals that are not [`PtDecal::is_well_formed`] are left out. `textures` is the scene's
/// texture count (an atlas index past it samples as a solid rectangle).
pub(crate) fn pack_decals(decals: &[PtDecal], textures: usize) -> Vec<u32> {
    let mut order: Vec<&PtDecal> = decals.iter().filter(|d| d.is_well_formed()).collect();
    order.sort_by_key(|d| d.priority);
    let mut words = Vec::with_capacity(order.len() * DECAL_WORDS);
    let f = |v: f32| v.to_bits();
    for d in order {
        // A zero quaternion normalises to NaN axes (`abs(NaN) > 1` is false, so the box
        // test would keep it). A non-finite centre, extent or colour does the same.
        if !decal_packed_finite(d) {
            continue;
        }
        let (r, u, fw) = d.axes();
        let mask = if d.receiver_mask == u32::MAX {
            0
        } else {
            d.receiver_mask
        };
        let atlas = match d.atlas {
            Some(i) if i < textures => i as u32,
            _ => crate::pack::NONE,
        };
        words.extend_from_slice(&[
            f(r.x),
            f(r.y),
            f(r.z),
            f(d.half_size[0]),
            f(u.x),
            f(u.y),
            f(u.z),
            f(d.half_size[1]),
            f(fw.x),
            f(fw.y),
            f(fw.z),
            f(d.half_size[2]),
            f(d.center[0]),
            f(d.center[1]),
            f(d.center[2]),
            f(d.opacity),
            f(d.albedo[0]),
            f(d.albedo[1]),
            f(d.albedo[2]),
            f(d.blend),
            f(d.normal_xy[0]),
            f(d.normal_xy[1]),
            f(d.roughness),
            f(d.normal_strength),
            f(d.emissive[0]),
            f(d.emissive[1]),
            f(d.emissive[2]),
            f(d.edge_softness),
            f(d.angle_fade[0]),
            f(d.angle_fade[1]),
            f(d.distance_fade[0]),
            f(d.distance_fade[1]),
            mask,
            d.channels,
            atlas,
            0,
        ]);
    }
    words
}

/// False when the packed floats would be non-finite or the axes degenerate: a zero-length,
/// non-finite or overflowing rotation (its normalised axes are NaN or zero).
fn decal_packed_finite(d: &PtDecal) -> bool {
    // A NaN quaternion would pack NaN axes; one so large its squared length overflows to
    // infinity normalises to zero axes, which put every point inside the box.
    let len2 = Quat::from_array(d.rotation).length_squared();
    if !(len2.is_finite() && len2 > 1e-12) {
        return false;
    }
    [
        d.center[0],
        d.center[1],
        d.center[2],
        d.half_size[0],
        d.half_size[1],
        d.half_size[2],
        d.albedo[0],
        d.albedo[1],
        d.albedo[2],
        d.opacity,
        d.roughness,
        d.normal_xy[0],
        d.normal_xy[1],
        d.normal_strength,
        d.emissive[0],
        d.emissive[1],
        d.emissive[2],
        d.edge_softness,
        d.blend,
        d.angle_fade[0],
        d.angle_fade[1],
        d.distance_fade[0],
        d.distance_fade[1],
    ]
    .iter()
    .all(|v| v.is_finite())
}

/// Marks the decal trailer (`push_trailer`).
pub(crate) const TRAILER_MAGIC: u32 = 0x5eca_1d27;

/// Pads `words` to a whole number of `vec4`s and appends the trailer `[first, count, magic, 0]`,
/// so the kernels find it from the buffer's end whatever padding a backend adds to a 16-byte
/// boundary (they look for the magic in the last 4 or 8 words).
pub(crate) fn push_trailer(words: &mut Vec<u32>, first: u32, count: u32) {
    while !words.len().is_multiple_of(4) {
        words.push(0);
    }
    words.extend_from_slice(&[first, count, TRAILER_MAGIC, 0]);
}

/// The decal code `pt.wgsl`'s three decal regions take when a scene has decals.
const FNS: &str = r"
// ---- Projected decals (ADR-2951, `regia_pt::decal`) ----
// The word buffer ends with the trailer [first decal word, decal count, magic, 0]; each decal
// is 36 words (see `pack_decals`, `push_trailer`). Mirrors `regia_vfx`'s `fs_decal` on the hit's material inputs.
var<private> pt_decal_emissive: vec3<f32>;

fn pt_decal_f(o: u32) -> f32 {
    return bitcast<f32>(textures[o]);
}

fn pt_decal_v3(o: u32) -> vec3<f32> {
    return vec3<f32>(pt_decal_f(o), pt_decal_f(o + 1u), pt_decal_f(o + 2u));
}

fn pt_decal_smooth(e0: f32, e1: f32, x: f32) -> f32 {
    return smoothstep(e0, e1, x);
}

// Bilinear, clamped to the edge (the frame's atlas sampler), top level.
fn pt_decal_tex(index: u32, uv: vec2<f32>) -> vec4<f32> {
    let w = max(textures[index * 4u + 1u], 1u);
    let h = max(textures[index * 4u + 2u], 1u);
    let offset = textures[index * 4u];
    let srgb = (textures[index * 4u + 3u] & 1u) != 0u;
    let fu = clamp(uv.x, 0.0, 1.0) * f32(w) - 0.5;
    let fv = clamp(uv.y, 0.0, 1.0) * f32(h) - 0.5;
    let x0 = floor(fu);
    let y0 = floor(fv);
    let tx = fu - x0;
    let ty = fv - y0;
    let xa = clamp(i32(x0), 0, i32(w) - 1);
    let xb = clamp(i32(x0) + 1, 0, i32(w) - 1);
    let ya = clamp(i32(y0), 0, i32(h) - 1);
    let yb = clamp(i32(y0) + 1, 0, i32(h) - 1);
    let a = texel(offset, w, h, xa, ya, srgb);
    let b = texel(offset, w, h, xb, ya, srgb);
    let c = texel(offset, w, h, xa, yb, srgb);
    let d = texel(offset, w, h, xb, yb, srgb);
    return mix(mix(a, b, tx), mix(c, d, tx), ty);
}

fn pt_decal_luma(index: u32, uv: vec2<f32>) -> f32 {
    return dot(pt_decal_tex(index, uv).rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
}

// Applies every decal over `p` to the surface inputs `shade_vertex` keeps one variable per member
// (`base`, `perceptual`, `n`; ADR-7879). `n_recv` is the receiver's interpolated normal (the
// frame's receiver target holds the mesh or heightfield normal, not the mapped one); `layer` is
// the receiver layer (0 terrain, 1 static mesh). Sets `pt_decal_emissive`.
fn pt_apply_decals(
    in_base: ptr<function, vec3<f32>>,
    in_perceptual: ptr<function, f32>,
    in_n: ptr<function, vec3<f32>>,
    p: vec3<f32>,
    n_recv: vec3<f32>,
    layer: u32,
) {
    pt_decal_emissive = vec3<f32>(0.0);
    // The trailer `[first, count, magic, 0]` ends the buffer, or the vec4 before a backend's
    // 16-byte padding.
    var tail = arrayLength(&textures);
    if (tail >= 8u && textures[tail - 2u] != 0x5eca1d27u) {
        tail = tail - 4u;
    }
    if (tail < 4u || textures[tail - 2u] != 0x5eca1d27u) {
        return;
    }
    let first_word = textures[tail - 4u];
    let decal_count = textures[tail - 3u];
    for (var k = 0u; k < decal_count; k = k + 1u) {
        let o = first_word + k * 36u;
        let right = pt_decal_v3(o);
        let up = pt_decal_v3(o + 4u);
        let fwd = pt_decal_v3(o + 8u);
        let center = pt_decal_v3(o + 12u);
        let rel = p - center;
        let x = dot(rel, right) / max(pt_decal_f(o + 3u), 1e-5);
        let y = dot(rel, up) / max(pt_decal_f(o + 7u), 1e-5);
        let z = dot(rel, fwd) / max(pt_decal_f(o + 11u), 1e-5);
        if (!(abs(x) <= 1.0 && abs(y) <= 1.0 && abs(z) <= 1.0)) {
            continue;
        }
        let mask = textures[o + 32u];
        if (mask != 0u && (mask & (1u << min(layer, 31u))) == 0u) {
            continue;
        }
        let channels = textures[o + 33u];
        let atlas = textures[o + 34u];
        let facing = abs(dot(n_recv, fwd));
        let a0 = pt_decal_f(o + 28u);
        let a1 = pt_decal_f(o + 29u);
        var grazing = 1.0;
        if (a1 > a0) {
            grazing = pt_decal_smooth(a0, a1, facing);
        }
        let feather = (1.0 - pt_decal_smooth(0.7, 1.0, abs(x))) * (1.0 - pt_decal_smooth(0.7, 1.0, abs(y)));
        let edge = mix(1.0, feather, clamp(pt_decal_f(o + 27u), 0.0, 1.0));
        let dist = length(globals.cam_pos.xyz - center);
        let d0 = pt_decal_f(o + 30u);
        let d1 = pt_decal_f(o + 31u);
        var dist_fade = 1.0;
        if (d1 > d0) {
            dist_fade = 1.0 - pt_decal_smooth(d0, d1, dist);
        }
        var tint = vec3<f32>(1.0);
        var coverage = 1.0;
        var auv = vec2<f32>(0.5);
        if (atlas != NONE) {
            auv = clamp(vec2<f32>(x * 0.5 + 0.5, 0.5 - y * 0.5), vec2<f32>(0.0), vec2<f32>(1.0));
            let texel_c = pt_decal_tex(atlas, auv);
            tint = texel_c.rgb;
            coverage = texel_c.a;
        }
        let albedo = pt_decal_v3(o + 16u) * tint;
        let w = pt_decal_f(o + 15u) * grazing * edge * dist_fade * coverage;
        if (!(w > 1e-4)) {
            continue;
        }
        let strength = clamp(pt_decal_f(o + 23u), 0.0, 1.0);
        if ((channels & 1u) != 0u) {
            if (pt_decal_f(o + 19u) < 0.5) {
                *in_base = *in_base * mix(vec3<f32>(1.0), albedo, w);
            } else {
                *in_base = mix(*in_base, albedo, w);
            }
        }
        // The decal's normal: the atlas luma as a height along the decal's axes, then the
        // authored tangent-space normal by whiteout (Barré-Brisebois & Hill 2012).
        var n_base = n_recv;
        if ((channels & 2u) != 0u && atlas != NONE) {
            let eps = 1.0 / 64.0;
            let h = dot(tint, vec3<f32>(0.2126, 0.7152, 0.0722));
            let hx = pt_decal_luma(atlas, clamp(auv + vec2<f32>(eps, 0.0), vec2<f32>(0.0), vec2<f32>(1.0)));
            let hy = pt_decal_luma(atlas, clamp(auv + vec2<f32>(0.0, eps), vec2<f32>(0.0), vec2<f32>(1.0)));
            let du = (hx - h) / eps;
            let dv = (hy - h) / eps;
            let scale = strength * 0.95;
            n_base = normalize(n_recv + right * (-du * scale) + up * (dv * scale));
        }
        var n_shade = n_base;
        let nxy = vec2<f32>(pt_decal_f(o + 20u), pt_decal_f(o + 21u));
        if (dot(nxy, nxy) > 1e-6) {
            let nz = sqrt(max(0.0, 1.0 - dot(nxy, nxy)));
            var t = right - n_base * dot(right, n_base);
            if (dot(t, t) < 1e-6) {
                t = up - n_base * dot(up, n_base);
            }
            t = normalize(t);
            let b = cross(n_base, t);
            let n_ts = normalize(vec3<f32>(nxy, nz));
            n_shade = normalize(t * n_ts.x + b * n_ts.y + n_base * n_ts.z);
        }
        if ((channels & 2u) != 0u) {
            let n_decal = normalize(mix(*in_n, n_shade, strength));
            *in_n = normalize(mix(*in_n, n_decal, w));
        }
        if ((channels & 4u) != 0u) {
            *in_perceptual = mix(*in_perceptual, clamp(pt_decal_f(o + 22u), 0.0, 1.0), w);
        }
        if ((channels & 8u) != 0u) {
            pt_decal_emissive = pt_decal_emissive + pt_decal_v3(o + 24u) * w;
        }
    }
}
";

/// The call the apply region takes (in `shade_vertex`, after the graph and view-albedo inputs).
const APPLY: &str = "    pt_apply_decals(&in_base, &in_perceptual, &in_n, s.p, ns, select(1u, 0u, instances[hit.inst].info.x == 1u));\n";

/// The line the emission region takes (after the shading is built, before the graph's and the
/// decals' emission replace its member).
const EMIT: &str = "    sh_emissive = sh_emissive + pt_decal_emissive;\n";

fn splice(src: &str, begin: &str, end: &str, body: &str) -> Result<String, String> {
    let b = src
        .find(begin)
        .ok_or_else(|| format!("pt.wgsl has no `{begin}`"))?;
    let e = src.find(end).ok_or_else(|| format!("pt.wgsl has no `{end}`"))?;
    if e < b {
        return Err(format!("pt.wgsl's `{begin}` and `{end}` are out of order"));
    }
    let mut out = String::with_capacity(src.len() + body.len() + 1);
    out.push_str(&src[..b + begin.len()]);
    out.push('\n');
    out.push_str(body);
    out.push_str(&src[e..]);
    Ok(out)
}

/// `source` with the decal code composed into its three regions.
pub(crate) fn composed_source(source: &str) -> Result<String, String> {
    let s = splice(source, FNS_BEGIN, FNS_END, FNS)?;
    let s = splice(&s, APPLY_BEGIN, APPLY_END, APPLY)?;
    splice(&s, EMIT_BEGIN, EMIT_END, EMIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_in_priority_order_with_axes_and_a_clamped_atlas() {
        let mut low = PtDecal::from_box(Vec3::ZERO, Quat::IDENTITY, Vec3::ONE, [0.5, 0.5, 0.5, 1.0]);
        low.priority = 1;
        low.atlas = Some(7);
        let mut high = low;
        high.priority = 5;
        high.atlas = Some(0);
        high.receiver_mask = 2;
        let words = pack_decals(&[high, low], 1);
        assert_eq!(words.len(), 2 * DECAL_WORDS);
        // Low first; its atlas index is past the texture count, so it is solid.
        assert_eq!(words[34], crate::pack::NONE);
        assert_eq!(words[DECAL_WORDS + 34], 0);
        assert_eq!(words[32], 0, "u32::MAX accepts every layer");
        assert_eq!(words[DECAL_WORDS + 32], 2);
        // Identity rotation: right +X, up +Y, forward +Z.
        assert_eq!(f32::from_bits(words[0]), 1.0);
        assert_eq!(f32::from_bits(words[5]), 1.0);
        assert_eq!(f32::from_bits(words[10]), 1.0);
    }

    /// A zero quaternion normalises to NaN axes. `abs(NaN) > 1` is false, so the box test
    /// keeps the decal and a NaN weight (`NaN <= 1e-4` is false) is mixed into the albedo.
    #[test]
    fn a_zero_rotation_is_omitted_and_nan_fails_the_box_test() {
        let mut zero = PtDecal::from_box(Vec3::ZERO, Quat::IDENTITY, Vec3::ONE, [0.2, 0.3, 0.4, 1.0]);
        zero.rotation = [0.0; 4];
        let words = pack_decals(&[zero], 0);
        assert!(
            words.is_empty()
                || words
                    .chunks(DECAL_WORDS)
                    .all(|d| d[..32].iter().all(|w| f32::from_bits(*w).is_finite())),
            "zero rotation packed non-finite axes"
        );
        assert_eq!(words.len(), 0, "zero rotation was packed");
        let composed = composed_source(crate::tracer::SOURCE).expect("regions");
        assert!(
            composed.contains("if (!(abs(x) <= 1.0 && abs(y) <= 1.0 && abs(z) <= 1.0))"),
            "NaN axes pass the box test"
        );
        assert!(composed.contains("if (!(w > 1e-4))"), "a NaN weight is applied");
    }

    /// A rotation that is NaN or so large its squared length overflows is omitted: the second
    /// normalised to zero axes, so every surface was inside the box (corr-low-render, corr-pt
    /// finding 8). A large but representable quaternion still packs unit axes.
    #[test]
    fn a_non_finite_or_overflowing_rotation_is_omitted() {
        for rotation in [
            [1e20, 0.0, 0.0, 0.0],
            [f32::NAN, 0.0, 0.0, 1.0],
            [0.0, f32::INFINITY, 0.0, 1.0],
        ] {
            let mut d = PtDecal::from_box(Vec3::ZERO, Quat::IDENTITY, Vec3::ONE, [0.2, 0.3, 0.4, 1.0]);
            d.rotation = rotation;
            d.angle_fade = [0.0, 0.0];
            assert_eq!(pack_decals(&[d], 0).len(), 0, "{rotation:?} was packed");
        }
        let mut d = PtDecal::from_box(Vec3::ZERO, Quat::IDENTITY, Vec3::ONE, [0.2, 0.3, 0.4, 1.0]);
        d.rotation = [0.0, 0.0, 0.0, 1e18];
        let words = pack_decals(&[d], 0);
        assert_eq!(words.len(), DECAL_WORDS);
        assert_eq!(f32::from_bits(words[0]), 1.0);
    }

    #[test]
    fn malformed_decals_are_left_out() {
        let good = PtDecal::from_box(Vec3::ZERO, Quat::IDENTITY, Vec3::ONE, [0.5, 0.5, 0.5, 1.0]);
        let mut zero_rotation = good;
        zero_rotation.rotation = [0.0; 4];
        let mut nan_center = good;
        nan_center.center[1] = f32::NAN;
        let mut inf_emission = good;
        inf_emission.emissive[0] = f32::INFINITY;
        let mut flat_box = good;
        flat_box.half_size[2] = 0.0;
        assert!(good.is_well_formed());
        for bad in [zero_rotation, nan_center, inf_emission, flat_box] {
            assert!(!bad.is_well_formed(), "{bad:?}");
        }
        let words = pack_decals(&[zero_rotation, good, nan_center, inf_emission, flat_box], 1);
        assert_eq!(words.len(), DECAL_WORDS, "only the well-formed decal is packed");
    }

    #[test]
    fn the_trailer_ends_a_whole_vec4() {
        let mut words = vec![7u32; 5];
        push_trailer(&mut words, 3, 2);
        assert_eq!(words.len(), 12);
        assert_eq!(&words[8..], &[3, 2, TRAILER_MAGIC, 0]);
        assert_eq!(&words[5..8], &[0, 0, 0]);
    }

    #[test]
    fn composes_into_every_region_and_leaves_the_rest() {
        let base = crate::tracer::SOURCE;
        let composed = composed_source(base).expect("regions");
        assert!(composed.contains("fn pt_apply_decals("));
        assert!(composed.contains("pt_apply_decals(&in_base, &in_perceptual, &in_n,"));
        assert!(composed.contains("sh_emissive = sh_emissive + pt_decal_emissive;"));
        // The kernels look for the same magic the packer writes.
        assert!(composed.contains(&format!("{TRAILER_MAGIC:#x}u")));
        // The base kernel text calls nothing: a decal-free scene traces the same module.
        assert!(!base.contains("pt_apply_decals("));
    }
}
