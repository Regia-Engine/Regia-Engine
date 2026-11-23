// SPDX-License-Identifier: MPL-2.0
//! Sticky sun cascades: a cascade keeps last frame's placement while it still covers
//! everything this frame's receivers can sample.
//!
//! A paged cascade's centre is snapped to its own texel grid ([`crate::snap_ortho_center`]),
//! so every placement of a cascade with the same light direction, radius and resolution
//! shares one world-fixed texel lattice: a texel holds the same depth sample whichever
//! placement drew it. The fitted centre follows the camera's frustum slice, so a moving
//! camera crossed a texel almost every frame and the cache redrew the whole cascade
//! (every mesh caster and the terrain) even though the content in the overlap did not
//! change. Here the previous placement is kept while
//!
//! - every corner of the slice's receivers (the view depths whose fragments sample this
//!   cascade, including the previous cascade's 15 % blend band) projects inside it with
//!   [`RECEIVER_MARGIN_TEXELS`] to spare for the normal offset and the widest PCSS kernel,
//! - those corners stay inside its depth range, and
//! - its near plane is no closer to the receivers than the freshly fitted cascade's, so
//!   it holds at least every caster the fresh fit would.
//!
//! Under those conditions a receiver reads the same texels, holding the same casters, as
//! it would from the fresh fit, so the frame does not change; the cache only redraws when
//! the camera leaves the placement. A new placement sits [`Z_SLACK`] of the radius
//! sunward of the fresh fit (snapped to the lattice), so the camera can climb toward the
//! sun before the near-plane rule refits it.
//!
//! ## Provenance
//!
//! Texel snapping: Wronski, "Stable Cascaded Shadow Maps" (SIGGRAPH 2014 talk). Keeping a
//! cached shadow region until the view leaves it is the scrolling/caching idea of
//! clipmap-style shadow caches (e.g. Tanner et al., "The Clipmap", SIGGRAPH 1998, applied to
//! shadow maps); the coverage rule here is Regia Engine's.

use regia_core::glam::{Mat4, Vec3, Vec4};

/// Texels kept between a receiver corner and the cascade edge: the 1.5-texel normal offset,
/// the widest sun PCSS filter radius (9 texels, `shadows.wgsl`), a bilinear footprint and
/// the sub-pixel TAA jitter, rounded up.
pub(crate) const RECEIVER_MARGIN_TEXELS: f32 = 16.0;

/// How far sunward of the fresh fit a new placement sits, as a fraction of the radius.
pub(crate) const Z_SLACK: f32 = 0.25;

/// The placement a cascade keeps between frames.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CascadeAnchor {
    dir: [f32; 3],
    radius: f32,
    resolution: u32,
    center: [f32; 3],
}

/// One cascade's fresh fit for this frame.
pub(crate) struct CascadeFit {
    /// Light travel direction.
    pub dir: [f32; 3],
    /// Fitted (unsnapped) centre: the frustum-slice sphere centre.
    pub center: [f32; 3],
    /// Ortho half extent.
    pub radius: f32,
    /// Cascade edge in texels.
    pub resolution: u32,
    /// Corners of the receiver region, world metres.
    pub receivers: [[f32; 3]; 8],
}

/// The eight corners of the camera frustum between view depths `d0` and `d1`.
///
/// `inv_view_proj` is the inverse of the unjittered view-projection, `eye` the camera
/// position and `forward` its unit view direction. Corners come from the rays through the
/// four NDC corners, so any up vector or field of view (horizontal-plus included) is right.
#[must_use]
pub(crate) fn slice_corners(
    inv_view_proj: Mat4,
    eye: Vec3,
    forward: Vec3,
    d0: f32,
    d1: f32,
) -> [[f32; 3]; 8] {
    let mut out = [[0.0; 3]; 8];
    for (k, (x, y)) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)]
        .into_iter()
        .enumerate()
    {
        let p = inv_view_proj * Vec4::new(x, y, 0.5, 1.0);
        let p = p.truncate() / p.w;
        let ray = p - eye;
        let depth = ray.dot(forward).max(1.0e-6);
        out[k] = (eye + ray * (d0 / depth)).to_array();
        out[k + 4] = (eye + ray * (d1 / depth)).to_array();
    }
    out
}

/// Cosine of the angle between `forward` and the frustum's corner rays (the widest angle a
/// visible point makes with the view axis).
#[must_use]
pub(crate) fn corner_cosine(inv_view_proj: Mat4, eye: Vec3, forward: Vec3) -> f32 {
    let p = inv_view_proj * Vec4::new(1.0, 1.0, 0.5, 1.0);
    let ray = (p.truncate() / p.w - eye).normalize_or_zero();
    ray.dot(forward).clamp(0.05, 1.0)
}

/// The centre to build this frame's cascade from, updating `anchor`.
///
/// `vp_of(centre)` must build the cascade view-projection exactly as the renderer does
/// (snapping included). Without an anchor, or when the anchor no longer covers the
/// receivers, a new anchor is placed; if even that cannot cover them (a radius too small
/// for the slice), the anchor is dropped and the fresh centre is returned unchanged.
pub(crate) fn anchored_center(
    anchor: &mut Option<CascadeAnchor>,
    fit: &CascadeFit,
    vp_of: impl Fn([f32; 3]) -> [[f32; 4]; 4],
) -> [f32; 3] {
    let fresh_vp = Mat4::from_cols_array_2d(&vp_of(fit.center));
    let Some(fresh_zmin) = depth_min(fresh_vp, &fit.receivers) else {
        *anchor = None;
        return fit.center;
    };
    let margin = RECEIVER_MARGIN_TEXELS * 2.0 / fit.resolution.max(1) as f32;
    if let Some(kept) = anchor.as_ref()
        && kept.dir == fit.dir
        && kept.radius == fit.radius
        && kept.resolution == fit.resolution
        && covers(
            Mat4::from_cols_array_2d(&vp_of(kept.center)),
            &fit.receivers,
            margin,
            fresh_zmin,
        )
    {
        return kept.center;
    }
    let dir = Vec3::from_array(fit.dir).normalize_or_zero();
    let snapped = Vec3::from_array(crate::snap_ortho_center(
        fit.dir,
        fit.center,
        fit.radius,
        fit.resolution,
    ));
    let texel = 2.0 * fit.radius / fit.resolution.max(1) as f32;
    let slack = (Z_SLACK * fit.radius / texel).round() * texel;
    let center = (snapped - dir * slack).to_array();
    if dir != Vec3::ZERO
        && center.iter().all(|c| c.is_finite())
        && covers(
            Mat4::from_cols_array_2d(&vp_of(center)),
            &fit.receivers,
            margin,
            fresh_zmin,
        )
    {
        *anchor = Some(CascadeAnchor {
            dir: fit.dir,
            radius: fit.radius,
            resolution: fit.resolution,
            center,
        });
        center
    } else {
        *anchor = None;
        fit.center
    }
}

/// Smallest NDC depth of `points` under an orthographic `vp`.
fn depth_min(vp: Mat4, points: &[[f32; 3]; 8]) -> Option<f32> {
    let mut lo = f32::INFINITY;
    for p in points {
        let clip = vp * Vec4::new(p[0], p[1], p[2], 1.0);
        if !clip.is_finite() || clip.w.abs() < 1.0e-6 {
            return None;
        }
        lo = lo.min(clip.z / clip.w);
    }
    Some(lo)
}

/// `true` when every point is inside `vp` laterally with `margin` (NDC) to spare, inside
/// its depth range, and no nearer its near plane than `zmin_floor`.
fn covers(vp: Mat4, points: &[[f32; 3]; 8], margin: f32, zmin_floor: f32) -> bool {
    let limit = 1.0 - margin;
    let mut zmin = f32::INFINITY;
    for p in points {
        let clip = vp * Vec4::new(p[0], p[1], p[2], 1.0);
        if !clip.is_finite() || clip.w.abs() < 1.0e-6 {
            return false;
        }
        let ndc = clip.truncate() / clip.w;
        if ndc.x.abs() > limit || ndc.y.abs() > limit || ndc.z > 1.0 - 1.0e-4 {
            return false;
        }
        zmin = zmin.min(ndc.z);
    }
    zmin >= zmin_floor - 1.0e-6
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::glam::camera::rh::proj::directx::{orthographic, perspective};
    use regia_core::glam::camera::rh::view::look_at_mat4;

    const RES: u32 = 2048;
    const SUN: [f32; 3] = [-0.55, -0.62, -0.56];

    /// The renderer's cascade matrix (`frame.rs` `ortho_light_vp`).
    fn vp_of(center: [f32; 3], radius: f32) -> [[f32; 4]; 4] {
        let dir = Vec3::from_array(SUN).normalize();
        let c = Vec3::from_array(crate::snap_ortho_center(dir.to_array(), center, radius, RES));
        let view = look_at_mat4(c - dir * radius, c, Vec3::Y);
        (orthographic(-radius, radius, -radius, radius, 0.001, radius * 3.0) * view).to_cols_array_2d()
    }

    /// The renderer's first-cascade fit for a camera at `eye` looking along `forward`.
    fn fit(eye: Vec3, forward: Vec3) -> CascadeFit {
        let (near, b1, fov_y, aspect) = (0.1f32, 60.0f32, 1.0f32, 16.0 / 9.0);
        let view = look_at_mat4(eye, eye + forward, Vec3::Y);
        let proj = perspective(fov_y, aspect, near, 4000.0);
        let inv = (proj * view).inverse();
        let mid = (near + b1) * 0.5;
        let tan_half = (fov_y * 0.5).tan();
        let lateral = b1 * tan_half * (1.0 + aspect * aspect).sqrt();
        let radius = ((b1 - mid).mul_add(b1 - mid, lateral * lateral) * 1.05).sqrt();
        CascadeFit {
            dir: Vec3::from_array(SUN).normalize().to_array(),
            center: (eye + forward * mid).to_array(),
            radius,
            resolution: RES,
            receivers: slice_corners(inv, eye, forward, near, b1),
        }
    }

    fn run(anchor: &mut Option<CascadeAnchor>, fit: &CascadeFit) -> [f32; 3] {
        let radius = fit.radius;
        anchored_center(anchor, fit, |c| vp_of(c, radius))
    }

    #[test]
    fn a_strafing_camera_keeps_its_placement_for_many_frames() {
        let forward = Vec3::new(0.0, -0.2, -1.0).normalize();
        let mut anchor = None;
        let mut refits = 0;
        let mut last = None;
        // 60 frames at 15 m/s sideways: 15 m.
        for frame in 0..60 {
            let eye = Vec3::new(frame as f32 * 0.25, 4.0, 0.0);
            let center = run(&mut anchor, &fit(eye, forward));
            if last != Some(center) {
                refits += 1;
            }
            last = Some(center);
        }
        assert!(refits <= 3, "{refits} placements over a 15 m strafe");
    }

    #[test]
    fn the_kept_placement_covers_every_receiver_with_the_margin() {
        let forward = Vec3::new(0.3, -0.25, -1.0).normalize();
        let mut anchor = None;
        for frame in 0..200 {
            let eye = Vec3::new(frame as f32 * 0.4, 4.0 + frame as f32 * 0.05, frame as f32 * -0.3);
            let f = fit(eye, forward);
            let center = run(&mut anchor, &f);
            let vp = Mat4::from_cols_array_2d(&vp_of(center, f.radius));
            let fresh = Mat4::from_cols_array_2d(&vp_of(f.center, f.radius));
            let margin = RECEIVER_MARGIN_TEXELS * 2.0 / RES as f32;
            let floor = depth_min(fresh, &f.receivers).expect("fresh depth");
            assert!(covers(vp, &f.receivers, margin, floor), "frame {frame}");
        }
    }

    #[test]
    fn placements_share_the_fresh_texel_lattice() {
        let forward = Vec3::new(0.0, -0.2, -1.0).normalize();
        let mut anchor = None;
        let first = run(&mut anchor, &fit(Vec3::new(0.0, 4.0, 0.0), forward));
        let f = fit(Vec3::new(3.0, 4.0, -2.0), forward);
        let kept = run(&mut anchor, &f);
        assert_eq!(first, kept, "a 3.6 m move must keep the placement");
        // A world point lands on the same texel centre offset in both matrices.
        let a = Mat4::from_cols_array_2d(&vp_of(kept, f.radius));
        let b = Mat4::from_cols_array_2d(&vp_of(f.center, f.radius));
        let p = Vec4::new(1.3, 0.2, -7.9, 1.0);
        let texel = |m: Mat4| {
            let c = m * p;
            ((c.x * 0.5 + 0.5) * RES as f32, (0.5 - c.y * 0.5) * RES as f32)
        };
        let (ax, ay) = texel(a);
        let (bx, by) = texel(b);
        assert!(((ax - bx) - (ax - bx).round()).abs() < 1.0e-2, "x {ax} vs {bx}");
        assert!(((ay - by) - (ay - by).round()).abs() < 1.0e-2, "y {ay} vs {by}");
    }

    #[test]
    fn a_changed_radius_refits() {
        let forward = Vec3::new(0.0, -0.2, -1.0).normalize();
        let mut anchor = None;
        let eye = Vec3::new(0.0, 4.0, 0.0);
        let first = run(&mut anchor, &fit(eye, forward));
        let mut wider = fit(eye, forward);
        wider.radius *= 1.5;
        let radius = wider.radius;
        let second = anchored_center(&mut anchor, &wider, |c| vp_of(c, radius));
        assert_ne!(first, second);
        let kept = anchor.expect("anchor");
        assert_eq!(kept.radius, radius);
    }
}
