// SPDX-License-Identifier: MPL-2.0
//! The two-eye render profile (spec 58 § 2.6, ADR-1918): the stereo strategy, the frustum both eyes
//! cull with, each eye's matrices, and the fixed XR quality set.
//!
//! Spec 58 calls for a fixed XR quality profile rather than parity with the desktop feature set.
//! [`XR_DISABLED_EFFECTS`] lists what the profile turns off and why; `xr.get` reports it, and a
//! game that asks for one of these gets the diagnostic instead of an image that differs between
//! the eyes.

use regia_core::glam::{Mat4, Quat, Vec3};
use regia_platform::xr::{DepthMode, DepthRange, Fov, Pose, View, XrError};
use regia_render::{EyeCamera, FrameQuality, PostSettings};

use crate::plugin::eye_projection;

/// How the two eyes are drawn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum StereoMode {
    /// The whole frame once per eye: the baseline every device can run, with the full XR quality
    /// profile.
    #[default]
    TwoPass,
    /// One pass over a two-layer target: the reduced forward set for standalone headsets (no
    /// shadows, no post stack). Needs the device's multiview feature; without it the frame falls
    /// back to two passes.
    Multiview,
}

impl StereoMode {
    /// Every mode, in order.
    pub const ALL: [StereoMode; 2] = [StereoMode::TwoPass, StereoMode::Multiview];

    /// The lowercase name the agent protocol uses.
    pub fn name(self) -> &'static str {
        match self {
            StereoMode::TwoPass => "two_pass",
            StereoMode::Multiview => "multiview",
        }
    }

    /// The mode named `name`.
    pub fn from_name(name: &str) -> Option<StereoMode> {
        StereoMode::ALL.into_iter().find(|m| m.name() == name)
    }
}

/// The strategy a frame runs: `requested`, or two passes when the device has no multiview.
pub fn resolve_stereo_mode(requested: StereoMode, multiview_supported: bool) -> StereoMode {
    match requested {
        StereoMode::Multiview if multiview_supported => StereoMode::Multiview,
        _ => StereoMode::TwoPass,
    }
}

/// Every effect the XR profile disables and the reason, as `xr.get` lists them (spec 58 § 2.6).
pub const XR_DISABLED_EFFECTS: [(&str, &str); 11] = [
    ("ssr", "screen-space reflections differ between the eyes"),
    ("ssgi", "screen-space indirect light differs between the eyes"),
    ("gi_probes", "the probe update follows one camera"),
    ("reflection_probes", "the probe update follows one camera"),
    ("volumetric_fog", "the froxel grid follows one camera"),
    ("volumetric_clouds", "the cloud history follows one camera"),
    ("refraction", "a screen-space refraction differs between the eyes"),
    ("visibility_buffer", "the resolve is built for one camera"),
    ("paged_sun_shadows", "an eye redraws its cascades whole"),
    ("motion_blur", "not comfortable in a headset"),
    ("depth_of_field", "not comfortable in a headset"),
];

/// Turns `quality` and `post` into the XR profile: the effects of [`XR_DISABLED_EFFECTS`] off, no
/// internal scaling (the render scale sizes the eye target), single-sample, fixed exposure so the
/// eyes can not meter apart. `per_eye_history` keeps temporal AA, whose history is per eye;
/// otherwise FXAA stands in.
pub fn apply_xr_quality(quality: &mut FrameQuality, post: Option<&mut PostSettings>, per_eye_history: bool) {
    quality.taau &= per_eye_history;
    quality.render_scale_permille = 1000;
    quality.msaa_samples = 1;
    quality.ssr = false;
    quality.screen_space_gi = false;
    quality.gi_probes = false;
    quality.reflection_probes = false;
    quality.volumetric_fog = false;
    quality.volumetric_clouds = false;
    quality.screen_space_refraction = false;
    quality.visibility_buffer = false;
    quality.paged_sun_shadows = false;
    quality.near_sun_tile = 0;
    quality.smaa = false;
    quality.translucent_shadow_cascades = 0;
    quality.translucent_local_shadows = false;
    quality.horizontal_plus = false;
    if let Some(post) = post {
        post.auto_exposure = false;
        post.taa = false;
        post.fxaa = !quality.taau;
        post.film_grain = 0.0;
        post.motion_blur = None;
        post.depth_of_field = None;
    }
}

/// World from tracking space: the rig's pose followed by the rig-local offset.
pub fn world_from_tracking(rig: &Pose, offset: &Pose) -> Pose {
    rig.compose(offset)
}

/// Margin on the union frustum's tangents, so a surface on the edge of both eyes stays in.
const UNION_MARGIN: f32 = 0.01;

/// Near plane of the union frustum, meters in front of its apex.
const UNION_NEAR_M: f32 = 0.01;

/// The view-projection (in tracking space) of one frustum that holds both eyes' frusta out to
/// `far_m`, for the single cull both eyes share (spec 58 § 2.6). It takes the bounds of both eyes'
/// field-of-view tangents around their mid orientation and moves the apex back until both eye
/// positions are inside: the side planes meet behind the eyes. It holds canted displays too.
pub fn union_cull_matrix(views: &[View; 2], far_m: f32) -> Result<Mat4, XrError> {
    let center_q: Quat = views[0].pose.orientation.slerp(views[1].pose.orientation, 0.5);
    let center_p = (views[0].pose.position + views[1].pose.position) * 0.5;
    let to_center = center_q.inverse();
    let eyes = views.map(|v| to_center * (v.pose.position - center_p));
    let mut tangent_x = (f32::INFINITY, f32::NEG_INFINITY);
    let mut tangent_y = (f32::INFINITY, f32::NEG_INFINITY);
    // How far along the center axis an eye's far plane reaches: past 1 on a canted corner.
    let mut reach = 1.0_f32;
    for view in views {
        view.fov.validate()?;
        let rotation = to_center * view.pose.orientation;
        for (x, y) in [
            (view.fov.left, view.fov.down),
            (view.fov.left, view.fov.up),
            (view.fov.right, view.fov.down),
            (view.fov.right, view.fov.up),
        ] {
            let dir = rotation * Vec3::new(x.tan(), y.tan(), -1.0);
            if dir.z >= -1e-4 {
                return Err(XrError::InvalidFov {
                    reason: "an edge of a canted view reaches past the center view's plane",
                });
            }
            reach = reach.max(-dir.z);
            let (tx, ty) = (dir.x / -dir.z, dir.y / -dir.z);
            tangent_x = (tangent_x.0.min(tx), tangent_x.1.max(tx));
            tangent_y = (tangent_y.0.min(ty), tangent_y.1.max(ty));
        }
    }
    let widen = |(lo, hi): (f32, f32)| {
        let margin = (hi - lo) * UNION_MARGIN;
        (lo - margin, hi + margin)
    };
    let (tangent_x, tangent_y) = (widen(tangent_x), widen(tangent_y));
    let span = |axis: fn(&Vec3) -> f32| {
        let (a, b) = (axis(&eyes[0]), axis(&eyes[1]));
        (a.min(b), a.max(b))
    };
    let (x_span, y_span, z_span) = (span(|v| v.x), span(|v| v.y), span(|v| v.z));
    let back = ((x_span.1 - x_span.0) / (tangent_x.1 - tangent_x.0))
        .max((y_span.1 - y_span.0) / (tangent_y.1 - tangent_y.0));
    // The apex that centers the eyes between the planes at distance `back`.
    let apex_axis = |span: (f32, f32), tangent: (f32, f32)| {
        0.5 * ((span.0 - tangent.0 * back) + (span.1 - tangent.1 * back))
    };
    let apex = Vec3::new(
        apex_axis(x_span, tangent_x),
        apex_axis(y_span, tangent_y),
        z_span.1 + back,
    );
    let fov = Fov {
        left: tangent_x.0.atan(),
        right: tangent_x.1.atan(),
        up: tangent_y.1.atan(),
        down: tangent_y.0.atan(),
    };
    let depth = DepthRange {
        near: UNION_NEAR_M,
        far: far_m.max(1.0) * reach * (1.0 + UNION_MARGIN) + back + (z_span.1 - z_span.0),
    };
    let pose = Pose {
        position: center_p + center_q * apex,
        orientation: center_q,
    };
    Ok(fov.projection(depth, DepthMode::Standard)? * pose.view_matrix())
}

/// The renderer's matrices for `view` under `world` (world from tracking space), culled by
/// `cull_tracking` ([`union_cull_matrix`]).
pub fn eye_camera(view: &View, world: &Pose, near_m: f32, cull_tracking: Mat4) -> Result<EyeCamera, XrError> {
    let world_inverse = world.view_matrix();
    Ok(EyeCamera {
        view: view.view_matrix() * world_inverse,
        projection: eye_projection(&view.fov, near_m)?,
        cull: cull_tracking * world_inverse,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::glam::Vec4;
    use regia_platform::xr::{Eye, stereo_views};

    fn inside(m: &Mat4, p: Vec3) -> bool {
        let c = *m * p.extend(1.0);
        c.w > 0.0 && c.x.abs() <= c.w && c.y.abs() <= c.w && c.z >= 0.0 && c.z <= c.w
    }

    /// Points on the rays through a grid of the eye's frustum, near to far.
    fn frustum_points(view: &View, far: f32) -> Vec<Vec3> {
        let mut points = Vec::new();
        for i in 0..=8 {
            for j in 0..=8 {
                let tx = view.fov.left.tan() + (view.fov.right.tan() - view.fov.left.tan()) * i as f32 / 8.0;
                let ty = view.fov.down.tan() + (view.fov.up.tan() - view.fov.down.tan()) * j as f32 / 8.0;
                for depth in [0.05, 0.5, 3.0, 30.0, far] {
                    let local = Vec3::new(tx, ty, -1.0) * depth;
                    points.push(view.pose.position + view.pose.orientation * local);
                }
            }
        }
        points
    }

    fn assert_holds_both(views: &[View; 2], far: f32) {
        let cull = union_cull_matrix(views, far).expect("union");
        for view in views {
            for point in frustum_points(view, far) {
                assert!(inside(&cull, point), "{point:?} outside for {:?}", view.eye);
            }
        }
    }

    #[test]
    fn the_union_holds_both_eyes_of_a_symmetric_head() {
        let head = Pose {
            position: Vec3::new(1.0, 1.7, -2.0),
            orientation: Quat::from_rotation_y(0.7),
        };
        let views = stereo_views(&head, 0.063, Fov::symmetric(0.9, 0.8)).expect("views");
        assert_holds_both(&views, 500.0);
    }

    #[test]
    fn the_union_holds_mirrored_off_axis_lenses_and_canted_displays() {
        let mut views = stereo_views(&Pose::IDENTITY, 0.07, Fov::symmetric(0.9, 0.8)).expect("views");
        views[0].fov = Fov {
            left: -0.96,
            right: 0.71,
            up: 0.82,
            down: -0.9,
        };
        views[1].fov = Fov {
            left: -0.71,
            right: 0.96,
            up: 0.82,
            down: -0.9,
        };
        assert_holds_both(&views, 200.0);
        views[0].pose.orientation = Quat::from_rotation_y(0.17);
        views[1].pose.orientation = Quat::from_rotation_y(-0.17);
        assert_holds_both(&views, 200.0);
    }

    /// Both eyes at the origin.
    fn mono_views(fov: Fov) -> [View; 2] {
        let view = |eye| View {
            eye,
            pose: Pose::IDENTITY,
            fov,
        };
        [view(Eye::Left), view(Eye::Right)]
    }

    #[test]
    fn the_union_does_not_grow_beyond_its_margin_for_equal_eyes() {
        let views = mono_views(Fov::symmetric(0.8, 0.8));
        let cull = union_cull_matrix(&views, 100.0).expect("union");
        let edge = 0.8_f32.tan();
        assert!(inside(&cull, Vec3::new(edge, 0.0, -1.0) * 10.0));
        assert!(!inside(&cull, Vec3::new(edge * 1.1, 0.0, -1.0) * 10.0));
    }

    #[test]
    fn a_fov_past_ninety_degrees_after_canting_is_refused() {
        let mut views = stereo_views(&Pose::IDENTITY, 0.06, Fov::symmetric(1.4, 0.8)).expect("views");
        views[0].pose.orientation = Quat::from_rotation_y(0.4);
        views[1].pose.orientation = Quat::from_rotation_y(-0.4);
        let err = union_cull_matrix(&views, 100.0).expect_err("behind the center");
        assert_eq!(err.code(), "xr_invalid_fov");
    }

    #[test]
    fn eye_matrices_follow_the_rig_and_the_cull_moves_with_them() {
        let rig = Pose {
            position: Vec3::new(10.0, 0.0, 0.0),
            orientation: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
        };
        let offset = Pose {
            position: Vec3::new(0.0, 1.65, 0.0),
            orientation: Quat::IDENTITY,
        };
        let world = world_from_tracking(&rig, &offset);
        let views = mono_views(Fov::symmetric(0.8, 0.8));
        let cull = union_cull_matrix(&views, 100.0).expect("union");
        let camera = eye_camera(&views[0], &world, 0.05, cull).expect("camera");
        let eye = camera.view.inverse().w_axis.truncate();
        assert!((eye - Vec3::new(10.0, 1.65, 0.0)).length() < 1e-4, "{eye}");
        // A quarter turn about Y: the eye now looks down -X.
        let ahead = Vec3::new(5.0, 1.65, 0.0);
        let clip = camera.projection * camera.view * Vec4::new(ahead.x, ahead.y, ahead.z, 1.0);
        assert!(clip.x.abs() < 1e-4 && clip.w > 0.0, "{clip}");
        assert!(inside(&camera.cull, ahead));
        assert!(!inside(&camera.cull, Vec3::new(15.0, 1.65, 0.0)), "behind");
    }

    #[test]
    fn the_xr_profile_turns_off_what_it_lists_and_keeps_history_on_request() {
        let mut quality = FrameQuality::LAB;
        quality.ssr = true;
        quality.volumetric_fog = true;
        quality.taau = true;
        quality.msaa_samples = 4;
        quality.render_scale_permille = 700;
        let mut post = PostSettings {
            auto_exposure: true,
            taa: true,
            ..PostSettings::default()
        };
        apply_xr_quality(&mut quality, Some(&mut post), true);
        assert!(!quality.ssr && !quality.volumetric_fog);
        assert!(quality.taau, "per-eye history keeps temporal AA");
        assert_eq!((quality.msaa_samples, quality.render_scale_permille), (1, 1000));
        assert!(!post.auto_exposure && !post.taa && !post.fxaa);
        let mut quality = FrameQuality {
            taau: true,
            ..FrameQuality::LAB
        };
        apply_xr_quality(&mut quality, Some(&mut post), false);
        assert!(!quality.taau && post.fxaa, "the Android set drops the history");
    }

    #[test]
    fn multiview_falls_back_when_the_device_has_none() {
        use StereoMode::*;
        assert_eq!(resolve_stereo_mode(Multiview, true), Multiview);
        assert_eq!(resolve_stereo_mode(Multiview, false), TwoPass);
        assert_eq!(resolve_stereo_mode(TwoPass, true), TwoPass);
        for mode in StereoMode::ALL {
            assert_eq!(StereoMode::from_name(mode.name()), Some(mode));
        }
    }
}
