// SPDX-License-Identifier: MPL-2.0
//! Reflection probes and box-projected specular (M15b).
//!
//! Screen-space reflections run in `shaders/ssr.wgsl`. This module is the CPU
//! side: which probes a view may use, the parallax-corrected lookup direction,
//! the per-face camera basis, and the equirectangular image built from six
//! captured faces. [`crate::FrameRenderer::capture_reflection_probe`] renders
//! those faces.
//!
//! ## Provenance
//!
//! Box-projected cubemaps: Lagarde, "Local Image-Based Lighting With Parallax-Corrected
//! Cubemaps", SIGGRAPH 2012. Screen-space reflections: McGuire and Mara,
//! "Efficient GPU Screen-Space Ray Tracing", JCGT 2014 (the shader marches in
//! screen space; it does not build a Hi-Z pyramid).

use regia_core::glam::Vec3;

use crate::profile::{FrameQuality, GiMode, SsaoMode};

/// Square face size rendered for one cubemap side before [`equirect_from_faces`].
pub const PROBE_FACE_SIZE: u32 = 16;
/// Equirectangular width the renderer uploads for a probe.
pub const EQUIRECT_WIDTH: u32 = 64;
/// Equirectangular height the renderer uploads for a probe.
pub const EQUIRECT_HEIGHT: u32 = 32;

/// One captured reflection probe.
#[derive(Clone, Debug)]
pub struct ReflectionProbe {
    /// Stable id.
    pub id: u64,
    /// World centre of the box.
    pub center: [f32; 3],
    /// Half extents of the parallax box.
    pub half_extent: [f32; 3],
    /// Row-major RGBA8, sRGB-encoded, length `width * height * 4`.
    pub equirect: Vec<u8>,
    /// Equirectangular width.
    pub width: u32,
    /// Equirectangular height.
    pub height: u32,
}

/// A probe selected for one view, with a normalised blend weight.
#[derive(Clone, Copy, Debug)]
pub struct ProbeBlend {
    /// Index into the probe list passed to [`select_probes`].
    pub index: usize,
    /// Blend weight in `0..1`. The returned weights sum to 1.
    pub weight: f32,
}

/// Look direction and up vector of cubemap face `face` (`0..6`).
///
/// The order matches the renderer's point-light shadow faces: +X, −X, +Y, −Y, +Z, −Z.
#[must_use]
pub fn cube_face_axes(face: u32) -> ([f32; 3], [f32; 3]) {
    match face {
        0 => ([1.0, 0.0, 0.0], [0.0, -1.0, 0.0]),
        1 => ([-1.0, 0.0, 0.0], [0.0, -1.0, 0.0]),
        2 => ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
        3 => ([0.0, -1.0, 0.0], [0.0, 0.0, -1.0]),
        4 => ([0.0, 0.0, 1.0], [0.0, -1.0, 0.0]),
        _ => ([0.0, 0.0, -1.0], [0.0, -1.0, 0.0]),
    }
}

/// Weight applied to the SSR mix. `shaders/ssr.wgsl` uses the same thresholds.
///
/// Roughness 0.35 (the reflection golden's floor) returns 1. Roughness 0.7 and
/// above returns 0, so an outdoor ground and a sky dome keep the shaded colour.
#[must_use]
pub fn ssr_roughness_weight(roughness: f32) -> f32 {
    ((0.7 - roughness) / 0.35).clamp(0.0, 1.0)
}

/// Quality used while capturing a probe so the capture cannot recurse into GI or SSR.
///
/// The face is also forced to a native 90° square: temporal upscale, horizontal-plus
/// FOV, multisampling and screen-space AO would change the face size or the basis
/// [`equirect_from_faces`] assumes.
#[must_use]
pub fn capture_quality(mut quality: FrameQuality) -> FrameQuality {
    quality.gi = GiMode::Ambient;
    quality.screen_space_gi = false;
    quality.ssr = false;
    quality.reflection_probes = false;
    quality.paged_sun_shadows = false;
    quality.near_sun_tile = 0;
    quality.ssao = SsaoMode::Off;
    quality.taau = false;
    quality.horizontal_plus = false;
    quality.msaa_samples = 1;
    quality.render_scale_permille = 1000;
    // A shadow-factor debug view must not be baked into the cubemap.
    quality.shadow_debug = 0;
    // A capture is a view of its own: only the frame the player sees traces the sun (ADR-2035).
    quality.sun_shadow_trace = crate::hwrt::RtShadowRequest::Off;
    // A probe capture sees the surfaces from everywhere at once, at probe resolution.
    quality.subsurface_scattering = false;
    quality.blue_noise = false;
    quality.order_independent_transparency = false;
    quality
}

/// Direction from a world position through `dir`, corrected onto the probe box: the CPU
/// reference of the frame shaders' `box_lookup` (`ssr.wgsl`, `sssr_common.wgsl`).
///
/// `dir` is the reflection direction. The ray leaves the box at the nearest of its three exit
/// planes (the `+` or `−` face per axis, by the direction's sign); the result is the vector
/// from the box centre to that exit point, which is the cubemap lookup. From inside the box
/// that is the face the ray hits. From outside, an exit plane behind the origin clamps the
/// distance to 0 as in the shaders, and the lookup is from the origin itself (`origin −
/// centre`).
#[must_use]
pub fn box_project(origin: [f32; 3], dir: [f32; 3], box_min: [f32; 3], box_max: [f32; 3]) -> [f32; 3] {
    let rd = Vec3::from_array(dir);
    let rd = if rd.length_squared() > 1.0e-10 {
        rd.normalize()
    } else {
        Vec3::Y
    };
    // The shaders' bias: no component is exactly 0, so an axis the ray runs along has its
    // `+` plane far away (or behind, from beyond that plane).
    let rd = (rd + Vec3::splat(1.0e-6)).normalize();
    let mut exit = f32::INFINITY;
    for axis in 0..3 {
        let component = rd[axis];
        let plane = if component > 0.0 {
            box_max[axis]
        } else {
            box_min[axis]
        };
        // `f32::min` skips the NaN of a 0 / 0 (origin on the plane, component 0).
        exit = exit.min((plane - origin[axis]) / component);
    }
    let distance = if exit.is_finite() { exit.max(0.0) } else { 0.0 };
    let hit = Vec3::from_array(origin) + rd * distance;
    let center = (Vec3::from_array(box_min) + Vec3::from_array(box_max)) * 0.5;
    (hit - center).to_array()
}

/// Picks up to `budget` probes by inverse-square distance to each probe's box.
///
/// A point inside a box has distance 0 and wins. Weights are renormalised.
#[must_use]
pub fn select_probes(probes: &[ReflectionProbe], point: [f32; 3], budget: u32) -> Vec<ProbeBlend> {
    if budget == 0 || probes.is_empty() {
        return Vec::new();
    }
    let mut ranked: Vec<(usize, f32)> = probes
        .iter()
        .enumerate()
        .map(|(index, probe)| {
            let half = probe.half_extent;
            let min = [
                probe.center[0] - half[0].abs(),
                probe.center[1] - half[1].abs(),
                probe.center[2] - half[2].abs(),
            ];
            let max = [
                probe.center[0] + half[0].abs(),
                probe.center[1] + half[1].abs(),
                probe.center[2] + half[2].abs(),
            ];
            let d = distance_to_aabb(point, min, max);
            (index, 1.0 / (d * d + 0.25))
        })
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked.truncate(budget as usize);
    let sum: f32 = ranked.iter().map(|(_, w)| *w).sum();
    if sum <= 1.0e-8 {
        return Vec::new();
    }
    ranked
        .into_iter()
        .map(|(index, weight)| ProbeBlend {
            index,
            weight: weight / sum,
        })
        .collect()
}

fn distance_to_aabb(p: [f32; 3], min: [f32; 3], max: [f32; 3]) -> f32 {
    let mut d2 = 0.0;
    for axis in 0..3 {
        let over = (min[axis] - p[axis]).max(p[axis] - max[axis]).max(0.0);
        d2 += over * over;
    }
    d2.sqrt()
}

/// Builds an equirectangular RGBA8 image from six square cubemap faces.
///
/// `faces` is +X, −X, +Y, −Y, +Z, −Z, each tightly packed RGBA8 of `face_size`².
/// Sampling uses the same basis as [`cube_face_axes`]. A short buffer becomes black.
#[must_use]
pub fn equirect_from_faces(faces: &[&[u8]; 6], face_size: u32, width: u32, height: u32) -> Vec<u8> {
    let mut out = vec![0u8; width as usize * height as usize * 4];
    if face_size == 0 || width == 0 || height == 0 {
        return out;
    }
    let needed = face_size as usize * face_size as usize * 4;
    if faces.iter().any(|face| face.len() < needed) {
        return out;
    }
    for y in 0..height {
        for x in 0..width {
            let u = (x as f32 + 0.5) / width as f32;
            let v = (y as f32 + 0.5) / height as f32;
            let dir = equirect_dir(u, v);
            let (face, su, sv) = project_to_face(dir);
            let px = (su * face_size as f32).clamp(0.0, face_size as f32 - 1.0) as u32;
            let py = (sv * face_size as f32).clamp(0.0, face_size as f32 - 1.0) as u32;
            let src = ((py * face_size + px) * 4) as usize;
            let dst = ((y * width + x) * 4) as usize;
            out[dst..dst + 4].copy_from_slice(&faces[face as usize][src..src + 4]);
        }
    }
    out
}

fn equirect_dir(u: f32, v: f32) -> [f32; 3] {
    let phi = (u - 0.5) * std::f32::consts::TAU;
    let theta = v * std::f32::consts::PI;
    let st = theta.sin();
    [st * phi.cos(), theta.cos(), st * phi.sin()]
}

/// Returns `(face, uv.x, uv.y)` with the texture y-down that a rendered face uses.
pub(crate) fn project_to_face(dir: [f32; 3]) -> (u32, f32, f32) {
    let d = Vec3::from_array(dir);
    let d = if d.length_squared() > 1.0e-10 {
        d.normalize()
    } else {
        Vec3::X
    };
    let mut best_face = 0u32;
    let mut best_dot = f32::NEG_INFINITY;
    for face in 0..6 {
        let (forward, _) = cube_face_axes(face);
        let dot = d.dot(Vec3::from_array(forward));
        if dot > best_dot {
            best_dot = dot;
            best_face = face;
        }
    }
    let (forward, up) = cube_face_axes(best_face);
    let f = Vec3::from_array(forward).normalize();
    let up = Vec3::from_array(up).normalize();
    let right = f.cross(up).normalize();
    let cam_up = right.cross(f);
    let ahead = d.dot(f).max(1.0e-4);
    let ndc_x = d.dot(right) / ahead;
    let ndc_y = d.dot(cam_up) / ahead;
    let u = (ndc_x * 0.5 + 0.5).clamp(0.0, 1.0);
    let v = (0.5 - ndc_y * 0.5).clamp(0.0, 1.0);
    (best_face, u, v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_at(id: u64, x: f32) -> ReflectionProbe {
        ReflectionProbe {
            id,
            center: [x, 0.0, 0.0],
            half_extent: [0.5, 0.5, 0.5],
            equirect: Vec::new(),
            width: 0,
            height: 0,
        }
    }

    #[test]
    fn ssr_weight_is_full_on_the_golden_floor_and_zero_on_rough_ground() {
        assert!((ssr_roughness_weight(0.35) - 1.0).abs() < 1.0e-5);
        assert!((ssr_roughness_weight(0.525) - 0.5).abs() < 1.0e-5);
        assert_eq!(ssr_roughness_weight(0.7), 0.0);
        assert_eq!(ssr_roughness_weight(0.95), 0.0);
        assert_eq!(ssr_roughness_weight(1.0), 0.0);
        assert_eq!(ssr_roughness_weight(0.0), 1.0);
    }

    #[test]
    fn budget_one_keeps_only_the_nearest_of_three_probes() {
        let probes = [probe_at(1, 0.0), probe_at(2, 10.0), probe_at(3, 20.0)];
        let picked = select_probes(&probes, [0.2, 0.0, 0.0], 1);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].index, 0);
        assert!((picked[0].weight - 1.0).abs() < 1.0e-4);
        let two = select_probes(&probes, [0.2, 0.0, 0.0], 2);
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].index, 0);
        assert!(two[0].weight > two[1].weight);
    }

    #[test]
    fn box_projection_bends_an_offset_ray() {
        let dir = box_project(
            [-0.5, 0.5, 0.0],
            [1.0, 0.0, 0.0],
            [-1.0, -1.0, -1.0],
            [1.0, 1.0, 1.0],
        );
        let v = Vec3::from_array(dir).normalize();
        assert!(v.x > 0.8, "the hit stays on the +X face, got {dir:?}");
        assert!(
            v.y > 0.3,
            "the offset origin must show up in the lookup, got {dir:?}"
        );
        let straight = box_project([0.0, 0.0, 0.0], [0.0, 1.0, 0.0], [-1.0; 3], [1.0; 3]);
        assert!(straight[1] > 0.9);
        // An origin outside the box whose exit plane lies behind it: the shaders' `box_lookup`
        // (`ssr.wgsl`, `sssr_common.wgsl`) clamps the distance to 0 and looks up from the
        // origin. The CPU reference must agree, and never return `f32::MAX` (audit render10 #9).
        let outside = box_project([2.0, 0.0, 0.0], [1.0, 1.0, 0.0], [-1.0; 3], [1.0; 3]);
        assert_eq!(outside, [2.0, 0.0, 0.0]);
        let away = box_project([2.0, 0.0, 0.0], [1.0, 0.0, 0.0], [-1.0; 3], [1.0; 3]);
        assert_eq!(away, [2.0, 0.0, 0.0]);
        // An offset box: the result is relative to its centre.
        let offset = box_project(
            [5.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [2.0, -1.0, -1.0],
            [4.0, 1.0, 1.0],
        );
        assert_eq!(offset, [2.0, 0.0, 0.0]);
        // Interior origins still land on the exit face.
        let corner = box_project([0.0, 0.0, 0.0], [1.0, 1.0, 0.0], [-1.0; 3], [1.0; 3]);
        assert!(
            (corner[0] - 1.0).abs() < 1.0e-5 && (corner[1] - 1.0).abs() < 1.0e-5,
            "{corner:?}"
        );
    }

    #[test]
    fn equirect_centre_reads_the_positive_x_face() {
        let size = 4u32;
        let mut faces = vec![vec![0u8; (size * size * 4) as usize]; 6];
        for px in faces[0].chunks_mut(4) {
            px.copy_from_slice(&[255, 0, 0, 255]);
        }
        let refs: [&[u8]; 6] = [&faces[0], &faces[1], &faces[2], &faces[3], &faces[4], &faces[5]];
        let image = equirect_from_faces(&refs, size, 16, 8);
        // u = 0.5, v = 0.5 is +X (phi = 0, theta = π/2).
        let x = 8u32;
        let y = 4u32;
        let i = ((y * 16 + x) * 4) as usize;
        assert_eq!(&image[i..i + 3], &[255, 0, 0], "center pixel {i}");
    }

    #[test]
    fn capture_quality_disables_recursive_lighting() {
        let mut quality = FrameQuality::from_profile(&crate::profile::RenderProfile::from_preset(
            crate::profile::RenderProfilePreset::DesktopHigh,
        ));
        assert!(quality.ssr && quality.reflection_probes);
        quality = capture_quality(quality);
        assert_eq!(quality.gi, GiMode::Ambient);
        assert!(!quality.ssr && !quality.reflection_probes && !quality.screen_space_gi);
        assert!(!quality.paged_sun_shadows);
        assert!(!quality.horizontal_plus && !quality.taau);
        assert_eq!(quality.msaa_samples, 1);
        assert_eq!(quality.render_scale_permille, 1000);
        let traced = FrameQuality {
            sun_shadow_trace: crate::hwrt::RtShadowRequest::On,
            ..quality
        };
        assert_eq!(
            capture_quality(traced).sun_shadow_trace,
            crate::hwrt::RtShadowRequest::Off
        );
        assert_eq!(quality.ssao, SsaoMode::Off);
    }

    #[test]
    fn vertical_cube_faces_do_not_use_world_up() {
        let (up_dir, up_up) = cube_face_axes(2);
        let (down_dir, down_up) = cube_face_axes(3);
        assert_eq!(up_dir, [0.0, 1.0, 0.0]);
        assert_eq!(down_dir, [0.0, -1.0, 0.0]);
        assert!(
            up_up[1].abs() < 1.0e-5,
            "looking +Y with world up is degenerate, got {up_up:?}"
        );
        assert!(
            down_up[1].abs() < 1.0e-5,
            "looking −Y with world up is degenerate, got {down_up:?}"
        );
        for face in 0..6 {
            let (dir, up) = cube_face_axes(face);
            let dot = dir[0] * up[0] + dir[1] * up[1] + dir[2] * up[2];
            assert!(dot.abs() < 1.0e-5, "face {face} up is parallel to the look");
            let len = |v: [f32; 3]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            assert!((len(dir) - 1.0).abs() < 1.0e-5);
            assert!((len(up) - 1.0).abs() < 1.0e-5);
        }
    }

    #[test]
    fn equirect_poles_read_the_vertical_faces() {
        let size = 4u32;
        let mut faces = vec![vec![0u8; (size * size * 4) as usize]; 6];
        for px in faces[2].chunks_mut(4) {
            px.copy_from_slice(&[0, 255, 0, 255]);
        }
        for px in faces[3].chunks_mut(4) {
            px.copy_from_slice(&[0, 0, 255, 255]);
        }
        let refs: [&[u8]; 6] = [&faces[0], &faces[1], &faces[2], &faces[3], &faces[4], &faces[5]];
        let image = equirect_from_faces(&refs, size, 16, 8);
        let top = (8 * 4) as usize;
        let bottom = ((7 * 16 + 8) * 4) as usize;
        assert_eq!(&image[top..top + 3], &[0, 255, 0], "the top row is the +Y face");
        assert_eq!(
            &image[bottom..bottom + 3],
            &[0, 0, 255],
            "the bottom row is the −Y face"
        );
    }
}
