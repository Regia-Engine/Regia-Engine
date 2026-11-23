// SPDX-License-Identifier: MPL-2.0
//! Sun shadow shimmer under a turning and moving camera, measured on the CPU (ADR-1236).
//!
//! The cascades come from the renderer's own placement (`regia_render::sun_cascades`).
//! A small software shadow map stands in for the GPU pass: a texel is in shadow when the sun
//! ray through its center hits a caster before the ground (the ground itself casts nothing, so
//! the flat receiver needs no bias, like the receiver-plane compare in `shadows.wgsl`). The
//! casters are pebbles and thin poles narrower than a texel: exactly the detail that shimmers
//! when the texel lattice slides. The ground under the camera is shaded through a bilinear
//! 3×3 PCF (the `Pcf3x3` row, `csm_filtered` radius 1) into an image anchored in the world,
//! so the camera's own turn does not move the pixels and any change between frames is the
//! shadow's. The engine's shimmer metric ([`sequence_flicker`]) and the mean absolute change
//! of visibility score each sweep.

use std::collections::HashSet;

use regia_core::glam::{Mat4, Quat, Vec3, Vec4};
use regia_render::sun_cascades::{SunCascadeView, sun_cascades};
use regia_render::{FrameQuality, RenderProfile, RenderProfilePreset, ShadowUniform, sequence_flicker};

/// Sun travel direction (about 46° elevation).
const SUN: [f32; 3] = [-0.45, -0.72, -0.53];
/// Ground image edge, pixels.
const IMAGE: u32 = 96;
/// The world-anchored ground patch: x in [-3, 3], z in [-13, -7] (6.25 cm pixels).
const PATCH_MIN: [f32; 2] = [-3.0, -13.0];
const PATCH_SIZE: f32 = 6.0;
/// Frames per sweep.
const FRAMES: u32 = 12;

#[derive(Clone, Copy)]
struct Caster {
    min: Vec3,
    max: Vec3,
}

/// A fixed field of pebbles (3–5 cm cubes 0.2–1.2 m up) and thin poles (2 cm, 1.5 m tall)
/// around and up-sun of the patch, from a fixed-seed generator.
fn casters() -> Vec<Caster> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 40) as f32 / (1u64 << 24) as f32
    };
    let mut out = Vec::new();
    for _ in 0..520 {
        let x = -5.0 + 10.0 * next();
        let z = -16.0 + 13.0 * next();
        let y = 0.2 + next();
        let half = 0.015 + 0.01 * next();
        out.push(Caster {
            min: Vec3::new(x - half, y - half, z - half),
            max: Vec3::new(x + half, y + half, z + half),
        });
    }
    for _ in 0..40 {
        let x = -5.0 + 10.0 * next();
        let z = -16.0 + 13.0 * next();
        out.push(Caster {
            min: Vec3::new(x - 0.01, 0.0, z - 0.01),
            max: Vec3::new(x + 0.01, 1.5, z + 0.01),
        });
    }
    out
}

/// Entry and exit of the segment `origin + t · dir`, `t` in `[0, t_max]`, through a box.
fn segment_hits(origin: Vec3, dir: Vec3, t_max: f32, b: &Caster) -> bool {
    let mut lo = 0.0f32;
    let mut hi = t_max;
    let axes = origin
        .to_array()
        .into_iter()
        .zip(dir.to_array())
        .zip(b.min.to_array().into_iter().zip(b.max.to_array()));
    for ((o, d), (bmin, bmax)) in axes {
        if d.abs() < 1.0e-12 {
            if o < bmin || o > bmax {
                return false;
            }
            continue;
        }
        let (a, c) = ((bmin - o) / d, (bmax - o) / d);
        lo = lo.max(a.min(c));
        hi = hi.min(a.max(c));
        if lo > hi {
            return false;
        }
    }
    true
}

/// Shadow-map texel coordinates (continuous, +u right, +v down) of a world point.
fn texel_coords(vp: Mat4, res: f32, p: Vec3) -> (f32, f32) {
    let clip = vp * p.extend(1.0);
    let ndc = clip.truncate() / clip.w;
    ((ndc.x * 0.5 + 0.5) * res, (0.5 - ndc.y * 0.5) * res)
}

/// Texels whose center ray meets a caster above the ground (the software shadow pass).
fn shadowed_texels(cascade: &ShadowUniform, res: u32, casters: &[Caster]) -> HashSet<(i32, i32)> {
    let vp = Mat4::from_cols_array_2d(&cascade.light_view_proj);
    let inv = vp.inverse();
    let resf = res as f32;
    let mut out = HashSet::new();
    for c in casters {
        let (mut u0, mut v0, mut u1, mut v1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for k in 0..8 {
            let corner = Vec3::new(
                if k & 1 == 0 { c.min.x } else { c.max.x },
                if k & 2 == 0 { c.min.y } else { c.max.y },
                if k & 4 == 0 { c.min.z } else { c.max.z },
            );
            let (u, v) = texel_coords(vp, resf, corner);
            u0 = u0.min(u);
            v0 = v0.min(v);
            u1 = u1.max(u);
            v1 = v1.max(v);
        }
        for j in (v0.floor() as i32 - 1)..=(v1.floor() as i32 + 1) {
            for i in (u0.floor() as i32 - 1)..=(u1.floor() as i32 + 1) {
                let x = (i as f32 + 0.5) / resf * 2.0 - 1.0;
                let y = 1.0 - (j as f32 + 0.5) / resf * 2.0;
                let near = inv * Vec4::new(x, y, 0.0, 1.0);
                let far = inv * Vec4::new(x, y, 1.0, 1.0);
                let (near, far) = (near.truncate() / near.w, far.truncate() / far.w);
                let dir = far - near;
                if dir.y >= 0.0 {
                    continue;
                }
                // The ground (y = 0) ends the ray: only casters above it shadow it.
                let t_ground = near.y / -dir.y;
                if segment_hits(near, dir, t_ground, c) {
                    out.insert((i, j));
                }
            }
        }
    }
    out
}

/// The ground patch's sun visibility through a bilinear (2r+1)² PCF, row-major.
fn ground_visibility(
    cascade: &ShadowUniform,
    res: u32,
    shadowed: &HashSet<(i32, i32)>,
    radius: i32,
) -> Vec<f32> {
    let vp = Mat4::from_cols_array_2d(&cascade.light_view_proj);
    let lit = |i: i32, j: i32| if shadowed.contains(&(i, j)) { 0.0 } else { 1.0 };
    let pixel = PATCH_SIZE / IMAGE as f32;
    let mut out = Vec::with_capacity((IMAGE * IMAGE) as usize);
    for py in 0..IMAGE {
        for px in 0..IMAGE {
            let p = Vec3::new(
                PATCH_MIN[0] + (px as f32 + 0.5) * pixel,
                0.0,
                PATCH_MIN[1] + (py as f32 + 0.5) * pixel,
            );
            let (u, v) = texel_coords(vp, res as f32, p);
            let mut sum = 0.0;
            for oy in -radius..=radius {
                for ox in -radius..=radius {
                    let (qu, qv) = (u + ox as f32 - 0.5, v + oy as f32 - 0.5);
                    let (bu, bv) = (qu.floor(), qv.floor());
                    let (fu, fv) = (qu - bu, qv - bv);
                    let (i, j) = (bu as i32, bv as i32);
                    let top = lit(i, j) * (1.0 - fu) + lit(i + 1, j) * fu;
                    let bottom = lit(i, j + 1) * (1.0 - fu) + lit(i + 1, j + 1) * fu;
                    sum += top * (1.0 - fv) + bottom * fv;
                }
            }
            let n = (2 * radius + 1) as f32;
            out.push(sum / (n * n));
        }
    }
    out
}

fn to_rgba(visibility: &[f32]) -> Vec<u8> {
    visibility
        .iter()
        .flat_map(|&v| {
            let g = (40.0 + 190.0 * v).round().clamp(0.0, 255.0) as u8;
            [g, g, g, 255]
        })
        .collect()
}

/// Camera of frame `k`: at `eye` + `strafe` · t along +x, looking down the patch, turned by
/// `yaw_deg` · (t − ½) about the vertical.
fn camera(k: u32, yaw_deg: f32, strafe: f32) -> SunCascadeView {
    let t = k as f32 / (FRAMES - 1) as f32;
    let base = Vec3::new(0.0, -0.25, -1.0).normalize();
    let forward = Quat::from_rotation_y((yaw_deg * (t - 0.5)).to_radians()) * base;
    let eye = Vec3::new(strafe * t, 1.7, 0.0);
    SunCascadeView {
        eye: eye.to_array(),
        forward: forward.to_array(),
        fov_y: 60f32.to_radians(),
        aspect: 16.0 / 9.0,
        near: 0.1,
        far: 400.0,
    }
}

struct Score {
    /// [`sequence_flicker`] over the frames.
    flicker: f32,
    /// Mean absolute change of visibility between consecutive frames.
    change: f32,
}

fn sweep(quality: &FrameQuality, yaw_deg: f32, strafe: f32, radius: i32) -> Score {
    let casters = casters();
    let sun = Vec3::from_array(SUN).normalize().to_array();
    let res = quality.csm_resolution;
    let mut images = Vec::new();
    let mut visibility = Vec::new();
    for k in 0..FRAMES {
        let csm = sun_cascades(&camera(k, yaw_deg, strafe), sun, quality);
        // The patch is 7–14 m out: cascade 0 on every preset with two or more cascades.
        let cascade = &csm.cascades[0];
        let shadowed = shadowed_texels(cascade, res, &casters);
        let vis = ground_visibility(cascade, res, &shadowed, radius);
        images.push(to_rgba(&vis));
        visibility.push(vis);
    }
    let frames: Vec<&[u8]> = images.iter().map(Vec::as_slice).collect();
    let mut change = 0.0;
    for pair in visibility.windows(2) {
        let diff: f32 = pair[0].iter().zip(&pair[1]).map(|(a, b)| (a - b).abs()).sum();
        change += diff / pair[0].len() as f32;
    }
    Score {
        flicker: sequence_flicker(&frames, IMAGE, IMAGE),
        change: change / (FRAMES - 1) as f32,
    }
}

fn mobile_high() -> FrameQuality {
    FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::MobileHigh))
}

#[test]
fn the_scene_has_shadow_detail_to_shimmer() {
    // Guard against a vacuous pass: the casters do shadow the patch, in sub-texel specks.
    let quality = mobile_high();
    let csm = sun_cascades(
        &camera(0, 12.0, 0.0),
        Vec3::from_array(SUN).normalize().to_array(),
        &quality,
    );
    let shadowed = shadowed_texels(&csm.cascades[0], quality.csm_resolution, &casters());
    let vis = ground_visibility(&csm.cascades[0], quality.csm_resolution, &shadowed, 1);
    let dark = vis.iter().filter(|&&v| v < 0.95).count();
    assert!(dark > vis.len() / 100, "{dark} of {} pixels shadowed", vis.len());
    assert!(vis.iter().any(|&v| v > 0.999), "some ground is fully lit");
}

#[test]
fn a_turning_camera_leaves_static_shadows_still() {
    // Mobile High: 2048² cascades, Pcf3x3. A 12° turn over 12 frames from a fixed eye.
    let stable = sweep(&mobile_high(), 12.0, 0.0, 1);
    assert!(
        stable.change < 1.0e-3 && stable.flicker < 1.0e-3,
        "stable cascades: change {} flicker {}",
        stable.change,
        stable.flicker
    );
}

#[test]
fn a_strafing_and_turning_camera_leaves_static_shadows_still() {
    let deck = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::Deck));
    // Deck: 2048² cascades, the 5×5 `Pcf` row (radius 2), a turn while strafing 1.3 m.
    let stable = sweep(&deck, -9.0, 1.3, 2);
    assert!(
        stable.change < 1.0e-3 && stable.flicker < 1.0e-3,
        "stable cascades: change {} flicker {}",
        stable.change,
        stable.flicker
    );
}

#[test]
fn the_free_sliding_fit_shimmers_and_the_metric_sees_it() {
    // The same sweep on the fit before ADR-1236 (center not snapped): the shadow specks
    // move from frame to frame although nothing in the world did.
    let sliding = FrameQuality {
        stable_sun_cascades: false,
        ..mobile_high()
    };
    let before = sweep(&sliding, 12.0, 0.0, 1);
    let after = sweep(&mobile_high(), 12.0, 0.0, 1);
    assert!(
        before.change > 0.001,
        "free-sliding cascades should shimmer: change {}",
        before.change
    );
    assert!(
        before.change > 10.0 * after.change,
        "before {} after {}",
        before.change,
        after.change
    );
    assert!(
        before.flicker >= after.flicker,
        "flicker before {} after {}",
        before.flicker,
        after.flicker
    );
}
