// SPDX-License-Identifier: MPL-2.0
//! Sun cascade placement and the cascade stability metrics (ADR-1236).
//!
//! The frame places its sun cascades here (`plan_shadows` in `frame.rs`), and the CPU tools
//! below measure how stable that placement is while the camera turns and moves:
//!
//! - [`sun_cascades`]: the cascades a view gets (the first-frame placement, without the
//!   paged-shadow anchors of [`crate::FrameRenderer`]).
//! - [`lattice_phase`] and [`phase_drift`]: where a fixed world point falls inside its shadow
//!   texel, and how far that spot moved between two placements.
//! - [`sweep_stability`]: a camera sweep (yaw, pitch or strafe) and the per-cascade drift over it.
//!
//! ## Why the lattice matters
//!
//! Each cascade is an orthographic sun view fitted to a sphere around the camera's frustum
//! slice. The sphere's radius depends only on the near and far distances, the field of view
//! and the aspect, so it does not change when the camera turns; its center does. When the
//! center is snapped to the cascade's own texel grid in light space, every placement shares
//! one world-fixed texel lattice: a static caster covers the same texels whichever placement
//! drew it, and its shadow is the same frame after frame. An unsnapped center slides the
//! lattice by a fraction of a texel every frame, and every caster detail narrower than a few
//! texels (a pole, a rail, a leaf, a hull edge) widens, thins and breaks up as the camera
//! turns: shimmer. Before ADR-1236 only paged sun shadows (Desktop High and Ultra) snapped;
//! Mobile, Desktop Low, Deck and the editor shimmered. [`crate::FrameQuality::stable_sun_cascades`]
//! now snaps them too.
//!
//! ## Provenance
//!
//! Sphere fit and texel snapping: Valient, "Stable Rendering of Cascaded Shadow Maps",
//! *ShaderX6* (2008); Wronski, "Stable Cascaded Shadow Maps" (SIGGRAPH 2014 talk); Microsoft,
//! "Common Techniques to Improve Shadow Depth Maps" (public documentation). The lattice-phase
//! drift metric is Regia Engine's.

use regia_core::glam::{Mat4, Vec3, Vec4};

use crate::FrameQuality;
use crate::cascade_anchor::CascadeAnchor;
use crate::forward::{CASCADE_COUNT, ShadowUniform, ShadowUniforms};

/// The camera a set of sun cascades is fitted to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SunCascadeView {
    /// Camera position, world meters.
    pub eye: [f32; 3],
    /// Unit view direction.
    pub forward: [f32; 3],
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Frame width over height.
    pub aspect: f32,
    /// Near plane, meters.
    pub near: f32,
    /// Far plane, meters (infinite is allowed: the cascades then end at 400 m).
    pub far: f32,
}

impl SunCascadeView {
    /// A view at `eye` looking at `target` on a `width` × `height` frame.
    #[must_use]
    pub fn look_at(
        eye: [f32; 3],
        target: [f32; 3],
        fov_y: f32,
        width: u32,
        height: u32,
        near: f32,
        far: f32,
    ) -> Self {
        let forward = (Vec3::from_array(target) - Vec3::from_array(eye)).normalize_or_zero();
        Self {
            eye,
            forward: forward.to_array(),
            fov_y,
            aspect: width.max(1) as f32 / height.max(1) as f32,
            near,
            far,
        }
    }
}

/// The texel grid the sun cascades snap to: the cascade resolution when `quality` keeps them
/// on a fixed lattice ([`FrameQuality::stable_sun_cascades`] or paged sun shadows), else `0`
/// (the center follows the camera continuously, as on [`FrameQuality::LAB`]).
#[must_use]
pub fn cascade_snap_texels(quality: &FrameQuality) -> u32 {
    if quality.paged_sun_shadows || quality.stable_sun_cascades {
        quality.csm_resolution.max(1)
    } else {
        0
    }
}

/// Steps per doubling of the cascade radius ladder ([`quantize_cascade_radius`]): radii
/// 2^(k/32) apart, about 2.2 %.
pub const RADIUS_STEPS_PER_OCTAVE: f32 = 32.0;

/// `radius` rounded up onto a geometric ladder of [`RADIUS_STEPS_PER_OCTAVE`] steps per
/// doubling. The cascade radius (and with it the texel size and the lattice spacing) follows
/// the field of view; a field of view that drifts every frame (the decaying tail of a camera's
/// FOV kick, a speed-dependent FOV) rescaled the lattice every frame, and every static shadow
/// crawled although the camera did not turn. On the ladder a small FOV drift keeps the radius,
/// at the cost of texels up to 2.2 % (1.1 % on average) coarser. Never smaller than `radius`;
/// a non-finite or non-positive radius comes back unchanged.
#[must_use]
pub fn quantize_cascade_radius(radius: f32) -> f32 {
    if !radius.is_finite() || radius <= 0.0 {
        return radius;
    }
    // The small allowance keeps a radius already on the ladder there (log2 of a ladder value
    // may round a hair above its step); the check below steps up when it rounded under.
    let mut k = radius.log2().mul_add(RADIUS_STEPS_PER_OCTAVE, -1.0e-3).ceil();
    let mut q = (k / RADIUS_STEPS_PER_OCTAVE).exp2();
    if q < radius {
        k += 1.0;
        q = (k / RADIUS_STEPS_PER_OCTAVE).exp2();
    }
    if q.is_finite() { q.max(radius) } else { radius }
}

/// The renderer's per-cascade placement memory (`cascade_anchor`), lent to
/// [`place_sun_cascades`] for one frame.
pub(crate) struct CascadeAnchors<'s> {
    /// Last placement per cascade.
    pub anchors: &'s mut [Option<CascadeAnchor>; 4],
    /// Bit `i` set: cascade `i` may keep its placement (paged sun shadows only).
    pub mask: u8,
    /// Inverse of the unjittered camera view-projection.
    pub inv_view_proj: Mat4,
}

/// Sun cascades for `view`: the practical split scheme over the view range (capped on the
/// 16 m – 300 m ladder of [`crate::csm_boundaries`]), each cascade an orthographic sphere fit
/// around its frustum slice, snapped to its texel lattice when [`cascade_snap_texels`] is not
/// zero, its radius on the [`quantize_cascade_radius`] ladder when
/// [`FrameQuality::stable_sun_cascades`] is on. With `anchors`, a paged cascade keeps last frame's placement while it still covers
/// every receiver that samples it (`cascade_anchor`).
pub(crate) fn place_sun_cascades(
    view: &SunCascadeView,
    sun_travel: [f32; 3],
    quality: &FrameQuality,
    mut anchors: Option<CascadeAnchors<'_>>,
) -> ShadowUniforms {
    let near = view.near.max(0.05);
    let shadow_far = crate::frame::csm_far(view.far);
    let count = quality.csm_cascade_count.clamp(1, CASCADE_COUNT) as usize;
    // The cascades span at most 4 km, and their splits are capped on a 16 m .. 300 m ladder
    // so a long view range does not stretch the near cascades (`csm_boundaries`; ADR-061).
    let boundaries = crate::csm_boundaries(near, shadow_far, count as u32);
    let eye = Vec3::from_array(view.eye);
    let forward = Vec3::from_array(view.forward);
    let aspect = view.aspect;
    let tan_half = (view.fov_y * 0.5).tan();
    let snap = cascade_snap_texels(quality);
    let corner_cos = anchors.as_ref().map_or(1.0, |kept| {
        crate::cascade_anchor::corner_cosine(kept.inv_view_proj, eye, forward)
    });
    let mut cascades = [ShadowUniform::IDENTITY; CASCADE_COUNT as usize];
    for i in 0..count {
        let b0 = boundaries[i];
        let b1 = boundaries[i + 1];
        let mid = (b0 + b1) * 0.5;
        let center = eye + forward * mid;
        // Sphere around the frustum slice: half depth plus the far slice's corner extent. It
        // depends on the distances, the field of view and the aspect only, so a turning
        // camera keeps the radius (and with it the texel size and the lattice spacing). Stable
        // cascades also round it up onto the radius ladder, so a drifting FOV keeps it too
        // (paged lab qualities snap without the ladder, so their goldens stay put).
        let lateral = b1 * tan_half * aspect.mul_add(aspect, 1.0).sqrt();
        let radius = ((b1 - mid).mul_add(b1 - mid, lateral * lateral) * 1.05).sqrt();
        let radius = radius.max(0.25);
        let radius = if quality.stable_sun_cascades {
            quantize_cascade_radius(radius)
        } else {
            radius
        };
        let back = crate::frame::cascade_back(radius, shadow_far);
        let center = match anchors.as_mut() {
            // A paged cascade keeps last frame's placement while it covers every fragment
            // that samples it (same texel lattice, so the same depths): the page cache then
            // redraws only what moved instead of the whole cascade on every camera step.
            Some(kept) if snap > 0 && kept.mask & (1u8 << i) != 0 => {
                // Receivers pick a cascade by distance to the eye (`sample_csm`); cascade `i`
                // also serves the last 15 % of cascade `i - 1` (the blend band).
                let lo = if i == 0 {
                    near
                } else {
                    let start = if i >= 2 { boundaries[i - 1] } else { 0.0 };
                    b0 - 0.15 * (b0 - start)
                };
                let fit = crate::cascade_anchor::CascadeFit {
                    dir: sun_travel,
                    center: center.to_array(),
                    radius,
                    resolution: snap,
                    receivers: crate::cascade_anchor::slice_corners(
                        kept.inv_view_proj,
                        eye,
                        forward,
                        (lo * corner_cos).max(near),
                        b1,
                    ),
                };
                Vec3::from_array(crate::cascade_anchor::anchored_center(
                    &mut kept.anchors[i],
                    &fit,
                    |c| crate::frame::ortho_light_vp(sun_travel, c, radius, back, snap),
                ))
            }
            Some(kept) => {
                kept.anchors[i] = None;
                center
            }
            None => center,
        };
        cascades[i] = ShadowUniform::new(
            crate::frame::ortho_light_vp(sun_travel, center.to_array(), radius, back, snap),
            b1,
            2.0 * radius / quality.csm_resolution.max(64) as f32,
            back + radius * 2.0,
        );
    }
    ShadowUniforms {
        cascades,
        count: [count as f32, 0.0, 0.0, 0.0],
    }
}

/// The sun cascades `quality` gives `view` for a sun traveling along `sun_travel`: what the
/// renderer draws on a frame without paged sun shadows, and on the first frame with them
/// (later paged frames may keep an older placement on the same lattice). `count[0]` holds the
/// cascade count; the other `count` slots are the renderer's and stay `0` here.
#[must_use]
pub fn sun_cascades(view: &SunCascadeView, sun_travel: [f32; 3], quality: &FrameQuality) -> ShadowUniforms {
    place_sun_cascades(view, sun_travel, quality, None)
}

/// Where `world` falls inside its texel of `cascade` drawn at `resolution`²: the fractional
/// texel coordinates `(u, v)`, each in `[0, 1)` (shadow-map texel space: +u right, +v down).
/// `None` when the point is outside the cascade or the matrix is degenerate.
#[must_use]
pub fn lattice_phase(cascade: &ShadowUniform, resolution: u32, world: [f32; 3]) -> Option<[f32; 2]> {
    let vp = Mat4::from_cols_array_2d(&cascade.light_view_proj);
    let clip = vp * Vec4::new(world[0], world[1], world[2], 1.0);
    if !clip.is_finite() || clip.w.abs() < 1.0e-9 {
        return None;
    }
    let ndc = clip.truncate() / clip.w;
    if ndc.x.abs() > 1.0 || ndc.y.abs() > 1.0 {
        return None;
    }
    let res = resolution.max(1) as f32;
    let u = (ndc.x * 0.5 + 0.5) * res;
    let v = (0.5 - ndc.y * 0.5) * res;
    Some([u - u.floor(), v - v.floor()])
}

/// Distance in texels between two [`lattice_phase`]s, each axis wrapped to `[-0.5, 0.5]`:
/// `0` when a world point sits at the same spot of its texel in both placements. A cascade
/// that moved by whole texels has no drift: its texels hold the same casters.
#[must_use]
pub fn phase_drift(a: [f32; 2], b: [f32; 2]) -> f32 {
    let wrap = |d: f32| d - d.round();
    let du = wrap(a[0] - b[0]);
    let dv = wrap(a[1] - b[1]);
    du.hypot(dv)
}

/// The world point at the center of `cascade` (its NDC origin at mid depth).
fn cascade_center(cascade: &ShadowUniform) -> Option<Vec3> {
    let vp = Mat4::from_cols_array_2d(&cascade.light_view_proj);
    if vp.determinant().abs() < 1.0e-20 {
        return None;
    }
    let p = vp.inverse() * Vec4::new(0.0, 0.0, 0.5, 1.0);
    if p.is_finite() && p.w.abs() > 1.0e-9 {
        Some(p.truncate() / p.w)
    } else {
        None
    }
}

/// Lattice drift (texels) of one cascade between two placements of the same radius: the
/// [`phase_drift`] of the world point halfway between their centers, which both cover. The
/// two matrices differ by a translation only (same sun, same radius), so every point drifts
/// alike. `None` when either matrix is degenerate or the point falls outside one of them.
#[must_use]
pub fn cascade_drift(a: &ShadowUniform, b: &ShadowUniform, resolution: u32) -> Option<f32> {
    let probe = (cascade_center(a)? + cascade_center(b)?) * 0.5;
    let pa = lattice_phase(a, resolution, probe.to_array())?;
    let pb = lattice_phase(b, resolution, probe.to_array())?;
    Some(phase_drift(pa, pb))
}

/// Largest lattice drift (texels) between frames that still counts as a still cascade: float
/// rounding of the matrices on a 1 cm texel is about a hundredth of a texel, a sliding
/// lattice a few tenths.
pub const STILL_DRIFT_TEXELS: f32 = 0.02;

/// A camera motion to measure cascade stability over ([`sweep_stability`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CameraSweep {
    /// Turn about the world vertical through the eye by `degrees` in total.
    Yaw {
        /// Total turn, degrees (sign = direction).
        degrees: f32,
    },
    /// Tilt about the camera's right axis by `degrees` in total (clamped short of straight
    /// up or down).
    Pitch {
        /// Total tilt, degrees (positive = up).
        degrees: f32,
    },
    /// Move the eye along the camera's right axis by `meters` in total.
    Strafe {
        /// Total move, meters.
        meters: f32,
    },
    /// Change the vertical field of view by `degrees` in total, easing out like a decaying
    /// FOV kick (fast at first, then ever smaller steps); clamped to 1°–170°.
    Zoom {
        /// Total change, degrees (positive = wider).
        degrees: f32,
    },
}

/// Stability of one sun cascade over a [`CameraSweep`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CascadeStability {
    /// Cascade index.
    pub cascade: u32,
    /// World size of one cascade texel, meters.
    pub texel_m: f32,
    /// Largest lattice drift between consecutive frames, texels (0 = static shadows stay put).
    pub max_drift_texels: f32,
    /// Mean lattice drift between consecutive frames, texels.
    pub mean_drift_texels: f32,
    /// Frame pairs whose cascade matrix changed (a snapped cascade moves in whole texels).
    pub moves: u32,
    /// Frame pairs whose cascade radius (texel size) changed: every texel moves, so static
    /// shadows shimmer on those frames whatever the drift says.
    pub rescales: u32,
    /// Frame pairs measured for drift (pairs that rescaled or whose probe left a cascade are
    /// skipped; a turn, tilt or strafe keeps the radius, so there this is `frames - 1`).
    pub pairs: u32,
}

/// The camera of frame `k` of `frames` along `sweep`, starting from `view`.
fn swept_view(view: &SunCascadeView, sweep: CameraSweep, k: u32, frames: u32) -> SunCascadeView {
    let t = if frames > 1 {
        k as f32 / (frames - 1) as f32
    } else {
        0.0
    };
    let forward = Vec3::from_array(view.forward).normalize_or_zero();
    let right = forward.cross(Vec3::Y).normalize_or_zero();
    let mut out = *view;
    match sweep {
        CameraSweep::Yaw { degrees } => {
            let turned = regia_core::glam::Quat::from_rotation_y((degrees * t).to_radians()) * forward;
            out.forward = turned.to_array();
        }
        CameraSweep::Pitch { degrees } => {
            let pitch0 = forward.y.clamp(-1.0, 1.0).asin();
            let limit = 85f32.to_radians();
            let pitch = (pitch0 + (degrees * t).to_radians()).clamp(-limit, limit);
            let flat = Vec3::new(forward.x, 0.0, forward.z).normalize_or_zero();
            let flat = if flat == Vec3::ZERO { Vec3::NEG_Z } else { flat };
            out.forward = (flat * pitch.cos() + Vec3::Y * pitch.sin()).to_array();
        }
        CameraSweep::Strafe { meters } => {
            out.eye = (Vec3::from_array(view.eye) + right * (meters * t)).to_array();
        }
        CameraSweep::Zoom { degrees } => {
            // Ease-out: 1 - (1 - t)^3, the shape of an exponential decay's tail.
            let eased = 1.0 - (1.0 - t).powi(3);
            out.fov_y =
                (view.fov_y + (degrees * eased).to_radians()).clamp(1f32.to_radians(), 170f32.to_radians());
        }
    }
    out
}

/// Per-cascade lattice drift over `frames` frames of `sweep` from `view` (at least 2 frames
/// are measured). A stable placement scores `0` drift on every cascade whatever the motion:
/// the cascades may move, but only in whole texels. The CPU twin of what the shimmer metric
/// ([`crate::sequence_flicker`]) sees on screen.
#[must_use]
pub fn sweep_stability(
    view: &SunCascadeView,
    sun_travel: [f32; 3],
    quality: &FrameQuality,
    sweep: CameraSweep,
    frames: u32,
) -> Vec<CascadeStability> {
    let frames = frames.max(2);
    let resolution = quality.csm_resolution.max(1);
    let placements: Vec<ShadowUniforms> = (0..frames)
        .map(|k| sun_cascades(&swept_view(view, sweep, k, frames), sun_travel, quality))
        .collect();
    let count = (placements[0].count[0] as usize).clamp(1, CASCADE_COUNT as usize);
    (0..count)
        .map(|i| {
            let mut stats = CascadeStability {
                cascade: i as u32,
                texel_m: placements[0].cascades[i].bias[0],
                max_drift_texels: 0.0,
                mean_drift_texels: 0.0,
                moves: 0,
                rescales: 0,
                pairs: 0,
            };
            let mut sum = 0.0;
            for pair in placements.windows(2) {
                let (a, b) = (&pair[0].cascades[i], &pair[1].cascades[i]);
                if a.bias[0] != b.bias[0] {
                    stats.rescales += 1;
                    continue;
                }
                let Some(drift) = cascade_drift(a, b, resolution) else {
                    continue;
                };
                stats.pairs += 1;
                sum += drift;
                stats.max_drift_texels = stats.max_drift_texels.max(drift);
                if a.light_view_proj != b.light_view_proj {
                    stats.moves += 1;
                }
            }
            if stats.pairs > 0 {
                stats.mean_drift_texels = sum / stats.pairs as f32;
            }
            stats
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RenderProfile, RenderProfilePreset};

    const SUN: [f32; 3] = [-0.42, -0.71, -0.56];

    fn view() -> SunCascadeView {
        SunCascadeView::look_at(
            [3.0, 1.7, 5.0],
            [3.4, 1.2, -5.0],
            60f32.to_radians(),
            1920,
            1080,
            0.1,
            2000.0,
        )
    }

    fn presets() -> [RenderProfilePreset; 6] {
        [
            RenderProfilePreset::MobileLow,
            RenderProfilePreset::MobileHigh,
            RenderProfilePreset::DesktopLow,
            RenderProfilePreset::DesktopHigh,
            RenderProfilePreset::DesktopUltra,
            RenderProfilePreset::Deck,
        ]
    }

    #[test]
    fn every_preset_and_the_editor_keep_the_cascades_on_their_lattice() {
        let mut qualities: Vec<(String, FrameQuality)> = presets()
            .into_iter()
            .map(|p| {
                (
                    format!("{p:?}"),
                    FrameQuality::from_profile(&RenderProfile::from_preset(p)),
                )
            })
            .collect();
        qualities.push(("editor".into(), FrameQuality::EDITOR));
        for (name, quality) in qualities {
            assert!(quality.stable_sun_cascades, "{name}");
            assert_eq!(cascade_snap_texels(&quality), quality.csm_resolution, "{name}");
        }
        // The lab baseline keeps its blessed free-sliding fit.
        const { assert!(!FrameQuality::LAB.stable_sun_cascades) };
        assert_eq!(cascade_snap_texels(&FrameQuality::LAB), 0);
    }

    #[test]
    fn a_turning_camera_does_not_slide_any_cascade_lattice() {
        for preset in presets() {
            let quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
            for sweep in [
                CameraSweep::Yaw { degrees: 90.0 },
                CameraSweep::Yaw { degrees: -7.0 },
                CameraSweep::Pitch { degrees: 25.0 },
            ] {
                let report = sweep_stability(&view(), SUN, &quality, sweep, 48);
                assert_eq!(report.len() as u32, quality.csm_cascade_count.clamp(1, 4));
                for c in &report {
                    assert_eq!(c.pairs, 47, "{preset:?} {sweep:?} {c:?}");
                    assert_eq!(c.rescales, 0, "{preset:?} {sweep:?} {c:?}");
                    assert!(
                        c.max_drift_texels < STILL_DRIFT_TEXELS,
                        "{preset:?} {sweep:?}: cascade {} drifts {} texels",
                        c.cascade,
                        c.max_drift_texels
                    );
                }
                // The turn does move the cascades (in whole texels).
                assert!(report.iter().any(|c| c.moves > 0), "{preset:?} {sweep:?}");
            }
        }
    }

    /// The light basis comes from the sun direction alone: placements of one cascade with the
    /// same radius differ by a translation only, bit for bit, however far out the far cascade's
    /// center lies (Deck's 2.6 km cascade under a pitch sweep turned by ~1e-4 rad when the basis
    /// was taken from `(center + dir) − center`; ADR-1387).
    #[test]
    fn placements_of_one_radius_share_the_light_basis_exactly() {
        let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::Deck));
        let sun = [0.2, -0.9, 0.1];
        let eye = [0.0, 1.7, 0.0];
        let start = SunCascadeView::look_at(
            eye,
            [0.0, 1.45, -1.0],
            60f32.to_radians(),
            1920,
            1080,
            0.1,
            2000.0,
        );
        let placements: Vec<ShadowUniforms> = (0..60)
            .map(|k| {
                sun_cascades(
                    &swept_view(&start, CameraSweep::Pitch { degrees: -20.0 }, k, 60),
                    sun,
                    &quality,
                )
            })
            .collect();
        let count = placements[0].count[0] as usize;
        for pair in placements.windows(2) {
            for i in 0..count {
                let (a, b) = (&pair[0].cascades[i], &pair[1].cascades[i]);
                if a.bias[0] != b.bias[0] {
                    continue;
                }
                for col in 0..3 {
                    let bits = |m: &[[f32; 4]; 4]| m[col].map(f32::to_bits);
                    assert_eq!(
                        bits(&a.light_view_proj),
                        bits(&b.light_view_proj),
                        "cascade {i} column {col}"
                    );
                }
            }
        }
        let report = sweep_stability(&start, sun, &quality, CameraSweep::Pitch { degrees: -20.0 }, 60);
        for c in &report {
            assert!(c.max_drift_texels < STILL_DRIFT_TEXELS, "{c:?}");
        }
    }

    #[test]
    fn a_strafing_camera_does_not_slide_any_cascade_lattice() {
        let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::Deck));
        let report = sweep_stability(&view(), SUN, &quality, CameraSweep::Strafe { meters: 3.7 }, 40);
        for c in &report {
            assert!(c.max_drift_texels < STILL_DRIFT_TEXELS, "{c:?}");
        }
    }

    #[test]
    fn the_free_sliding_fit_drifts_so_the_metric_can_see_shimmer() {
        // The lab baseline's unsnapped cascades: a slow turn slides the lattice by a fraction
        // of a texel nearly every frame. A metric that read 0 here would hide the bug fixed
        // by ADR-1236.
        let quality = FrameQuality {
            csm_resolution: 2048,
            ..FrameQuality::LAB
        };
        let report = sweep_stability(&view(), SUN, &quality, CameraSweep::Yaw { degrees: 12.0 }, 48);
        for c in &report {
            assert!(
                c.mean_drift_texels > 0.05,
                "cascade {} mean drift {}",
                c.cascade,
                c.mean_drift_texels
            );
        }
        // The same quality with the lattice kept is still.
        let stable = FrameQuality {
            stable_sun_cascades: true,
            ..quality
        };
        let report = sweep_stability(&view(), SUN, &stable, CameraSweep::Yaw { degrees: 12.0 }, 48);
        assert!(
            report.iter().all(|c| c.max_drift_texels < STILL_DRIFT_TEXELS),
            "{report:?}"
        );
    }

    #[test]
    fn a_turn_keeps_the_cascade_radius_and_texel_size() {
        let quality =
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopLow));
        let a = sun_cascades(&view(), SUN, &quality);
        let turned = swept_view(&view(), CameraSweep::Yaw { degrees: 140.0 }, 1, 2);
        let b = sun_cascades(&turned, SUN, &quality);
        let count = quality.csm_cascade_count as usize;
        for (i, (x, y)) in a.cascades.iter().zip(&b.cascades).take(count).enumerate() {
            assert_eq!(x.bias, y.bias, "cascade {i}");
            assert_eq!(x.split_distance, y.split_distance, "cascade {i}");
        }
    }

    #[test]
    fn the_frame_and_the_public_placement_agree() {
        // Without anchors (or with an empty paging mask) the frame's placement is the public
        // one, so the agent's stability report describes what is drawn.
        let quality =
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::MobileHigh));
        let mut slots = [None; 4];
        let framed = place_sun_cascades(
            &view(),
            SUN,
            &quality,
            Some(CascadeAnchors {
                anchors: &mut slots,
                mask: 0,
                inv_view_proj: Mat4::IDENTITY,
            }),
        );
        let public = sun_cascades(&view(), SUN, &quality);
        for (x, y) in framed.cascades.iter().zip(&public.cascades) {
            assert_eq!(x.light_view_proj, y.light_view_proj);
        }
        assert!(slots.iter().all(Option::is_none));
    }

    /// The body of WGSL function `name` in `source` (from its signature to the next `\nfn `).
    fn wgsl_fn<'a>(source: &'a str, name: &str) -> &'a str {
        let start = source
            .find(&format!("fn {name}("))
            .unwrap_or_else(|| panic!("{name} not in the shader"));
        let rest = &source[start..];
        let end = rest[3..].find("\nfn ").map_or(rest.len(), |i| i + 3);
        &rest[..end]
    }

    #[test]
    fn receivers_past_the_shadow_distance_skip_the_filter() {
        // `mix(s0, 1.0, 1.0)` is 1: past the last split the filter's result is thrown away, so
        // the early return must come before the first cascade filter (ADR-1236).
        let forward = include_str!("../shaders/lab_forward.wgsl");
        let body = wgsl_fn(forward, "sample_csm_cascades");
        let early = body
            .find("if (t >= 1.0 && cascade + 1u >= count)")
            .expect("far early-out");
        let filter = body.find("sample_cascade(cascade,").expect("filter call");
        assert!(early < filter, "the early-out must precede the filter");
        assert!(body[early..filter].contains("return 1.0;"));
        // The blend band itself keeps both samples.
        assert!(body.contains("return mix(s0, 1.0, t);"));
    }

    #[test]
    fn the_radius_ladder_rounds_up_by_at_most_one_step() {
        let step = (1.0 / RADIUS_STEPS_PER_OCTAVE).exp2();
        let mut last = 0.0;
        for i in 0..2000 {
            let r = 0.25 * 1.0037f32.powi(i);
            let q = quantize_cascade_radius(r);
            assert!(q >= r, "{r} -> {q}");
            assert!(q <= r * step * 1.000_01, "{r} -> {q}");
            assert!(q >= last, "monotone: {r} -> {q} after {last}");
            assert_eq!(quantize_cascade_radius(q), q, "idempotent at {q}");
            last = q;
        }
        assert_eq!(quantize_cascade_radius(1.0), 1.0);
        assert_eq!(quantize_cascade_radius(2.0), 2.0);
        assert!(quantize_cascade_radius(f32::NAN).is_nan());
        assert_eq!(quantize_cascade_radius(0.0), 0.0);
    }

    #[test]
    fn a_decaying_fov_kick_rescales_the_cascades_a_few_times_not_every_frame() {
        // A 5° kick easing back over 60 frames. The free fit rescales every cascade on nearly
        // every frame; on the ladder each cascade steps a handful of times and holds its lattice
        // in between.
        let sweep = CameraSweep::Zoom { degrees: -5.0 };
        let mut kicked = view();
        kicked.fov_y = 65f32.to_radians();
        let free = FrameQuality {
            csm_resolution: 2048,
            ..FrameQuality::LAB
        };
        for c in sweep_stability(&kicked, SUN, &free, sweep, 60) {
            assert!(c.rescales >= 50, "free fit: {c:?}");
        }
        for preset in presets() {
            let quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
            for c in sweep_stability(&kicked, SUN, &quality, sweep, 60) {
                assert!(c.rescales <= 8, "{preset:?}: {c:?}");
                assert!(c.max_drift_texels < STILL_DRIFT_TEXELS, "{preset:?}: {c:?}");
            }
        }
    }

    #[test]
    fn phase_drift_wraps_whole_texels() {
        assert_eq!(phase_drift([0.25, 0.5], [0.25, 0.5]), 0.0);
        assert!((phase_drift([0.95, 0.5], [0.05, 0.5]) - 0.1).abs() < 1.0e-5);
        assert!((phase_drift([0.0, 0.0], [0.5, 0.0]) - 0.5).abs() < 1.0e-6);
        assert!((phase_drift([0.1, 0.1], [0.4, 0.5]) - 0.5).abs() < 1.0e-5);
    }

    #[test]
    fn lattice_phase_rejects_points_outside_the_cascade() {
        let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::MobileLow));
        let c = sun_cascades(&view(), SUN, &quality);
        assert!(lattice_phase(&c.cascades[0], 1024, [1.0e6, 0.0, 0.0]).is_none());
        let inside = cascade_center(&c.cascades[0]).expect("center");
        let phase = lattice_phase(&c.cascades[0], 1024, inside.to_array()).expect("inside");
        assert!(phase.iter().all(|p| (0.0..1.0).contains(p)), "{phase:?}");
        // An identity (unset) cascade is degenerate.
        assert!(cascade_drift(&ShadowUniform::IDENTITY, &c.cascades[0], 1024).is_none());
    }
}
