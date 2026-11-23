// SPDX-License-Identifier: MPL-2.0
//! Stereo math (spec 58 § 2.6) against hand-computed frusta: the union cull matrix's side, near
//! and far planes, `eye_camera`, `world_from_tracking`, `eye_projection` fallbacks, the XR quality
//! profile and the stereo-mode table. CPU only.

use regia_core::glam::{Mat4, Quat, Vec3};
use regia_platform::xr::{Eye, Fov, Pose, View};
use regia_render::{DepthOfField, FrameQuality, MotionBlur, PostSettings};
use regia_xr::stereo::{eye_camera, world_from_tracking};
use regia_xr::{
    StereoMode, XR_DISABLED_EFFECTS, apply_xr_quality, eye_projection, resolve_stereo_mode, union_cull_matrix,
};

/// Standard-depth clip test: inside the clip volume.
fn inside(m: &Mat4, p: Vec3) -> bool {
    let c = *m * p.extend(1.0);
    c.w > 0.0 && c.x.abs() <= c.w && c.y.abs() <= c.w && c.z >= 0.0 && c.z <= c.w
}

fn view(eye: Eye, position: Vec3, fov: Fov) -> View {
    View {
        eye,
        pose: Pose::new(position, Quat::IDENTITY),
        fov,
    }
}

fn mono(fov: Fov) -> [View; 2] {
    [
        view(Eye::Left, Vec3::ZERO, fov),
        view(Eye::Right, Vec3::ZERO, fov),
    ]
}

fn unit_fov() -> Fov {
    // tan(pi/4) = 1: the edge rays are the diagonals.
    Fov::symmetric(std::f32::consts::FRAC_PI_4, std::f32::consts::FRAC_PI_4)
}

/// Catches the near plane, the far plane and the 1% side margin: coincident unit-tangent eyes,
/// far 100, so the boundaries are x = 1.02 d, near 0.01 and far 101.
#[test]
fn the_union_of_coincident_eyes_has_the_hand_computed_planes() {
    let cull = union_cull_matrix(&mono(unit_fov()), 100.0).expect("union");
    // Side planes at depth 10: |x| <= 10.2.
    assert!(inside(&cull, Vec3::new(10.15, 0.0, -10.0)));
    assert!(!inside(&cull, Vec3::new(10.25, 0.0, -10.0)));
    assert!(inside(&cull, Vec3::new(-10.15, 0.0, -10.0)));
    assert!(!inside(&cull, Vec3::new(-10.25, 0.0, -10.0)));
    assert!(inside(&cull, Vec3::new(0.0, 10.15, -10.0)));
    assert!(!inside(&cull, Vec3::new(0.0, -10.25, -10.0)));
    // Near 0.01 m, far 100 * 1.01 m.
    assert!(
        !inside(&cull, Vec3::new(0.0, 0.0, -0.005)),
        "in front of the apex, before near"
    );
    assert!(inside(&cull, Vec3::new(0.0, 0.0, -0.02)));
    assert!(inside(&cull, Vec3::new(0.0, 0.0, -100.9)));
    assert!(!inside(&cull, Vec3::new(0.0, 0.0, -101.2)));
    assert!(!inside(&cull, Vec3::new(0.0, 0.0, 1.0)), "behind the eyes");
}

/// Catches an apex that stays at the eye midpoint: with eyes 0.2 m apart the side planes meet
/// 0.2 / 2.04 = 0.098 m behind them, and the near plane sits 0.01 m in front of that apex.
#[test]
fn the_apex_moves_back_until_both_eyes_fit() {
    let fov = unit_fov();
    let views = [
        view(Eye::Left, Vec3::new(-0.1, 0.0, 0.0), fov),
        view(Eye::Right, Vec3::new(0.1, 0.0, 0.0), fov),
    ];
    let cull = union_cull_matrix(&views, 100.0).expect("union");
    // Apex z = 0.0980; near plane at z = 0.0880.
    assert!(
        inside(&cull, Vec3::new(0.0, 0.0, 0.08)),
        "between the eyes' plane and the near plane"
    );
    assert!(!inside(&cull, Vec3::new(0.0, 0.0, 0.09)), "behind the near plane");
    // The left eye's own left edge ray at 5 m: (-5.1, 0, -5) is inside, 5.35 m out is not
    // (the union reaches 1.02 * 5.098 = 5.2 m at that depth).
    assert!(inside(&cull, Vec3::new(-5.1, 0.0, -5.0)));
    assert!(inside(&cull, Vec3::new(5.1, 0.0, -5.0)));
    assert!(!inside(&cull, Vec3::new(-5.35, 0.0, -5.0)));
    assert!(!inside(&cull, Vec3::new(5.35, 0.0, -5.0)));
    // Vertical planes still come from the (equal) vertical tangents.
    assert!(inside(&cull, Vec3::new(0.0, 5.1, -5.0)));
    assert!(!inside(&cull, Vec3::new(0.0, 5.3, -5.0)));
}

/// Catches a far plane that forgets the eyes' depth offset: the eye 0.3 m ahead of the other must
/// still have its whole 100 m held.
#[test]
fn the_far_plane_covers_the_eye_that_is_further_forward() {
    let fov = unit_fov();
    let views = [
        view(Eye::Left, Vec3::new(0.0, 0.0, 0.0), fov),
        view(Eye::Right, Vec3::new(0.0, 0.0, -0.3), fov),
    ];
    let cull = union_cull_matrix(&views, 100.0).expect("union");
    let far_of_front_eye = Vec3::new(0.0, 0.0, -0.3 - 100.0);
    assert!(
        inside(&cull, far_of_front_eye),
        "the forward eye's far point is culled"
    );
    assert!(inside(&cull, Vec3::new(0.0, 0.0, -101.2)));
    assert!(!inside(&cull, Vec3::new(0.0, 0.0, -101.6)));
    // The apex is the rear eye (z = 0): near plane 0.01 ahead of it.
    assert!(!inside(&cull, Vec3::new(0.0, 0.0, -0.005)));
    assert!(inside(&cull, Vec3::new(0.0, 0.0, -0.02)));
}

/// Catches a missing floor on `far_m`: zero, negative and sub-meter distances all act as 1 m.
#[test]
fn a_tiny_far_distance_is_floored_at_one_meter() {
    for far in [0.0_f32, -50.0, 0.5, 1.0] {
        let cull = union_cull_matrix(&mono(unit_fov()), far).expect("union");
        assert!(inside(&cull, Vec3::new(0.0, 0.0, -1.005)), "far {far}");
        assert!(!inside(&cull, Vec3::new(0.0, 0.0, -1.1)), "far {far}");
    }
}

/// Catches up/down or left/right swapped in the tangent bounds, using a lopsided field of view
/// whose boundaries (tan plus the 1% margin) are computed in the test.
#[test]
fn an_asymmetric_field_of_view_keeps_its_four_sides() {
    let fov = Fov {
        left: -0.5,
        right: 0.9,
        up: 0.4,
        down: -0.6,
    };
    let cull = union_cull_matrix(&mono(fov), 100.0).expect("union");
    let (tl, tr, tu, td) = (fov.left.tan(), fov.right.tan(), fov.up.tan(), fov.down.tan());
    let right = tr + 0.01 * (tr - tl);
    let left = tl - 0.01 * (tr - tl);
    let up = tu + 0.01 * (tu - td);
    let down = td - 0.01 * (tu - td);
    let d = 10.0;
    for (axis, edge) in [(0, right), (0, left), (1, up), (1, down)] {
        let mut inner = Vec3::new(0.0, 0.0, -d);
        let mut outer = inner;
        inner[axis] = edge * d * 0.999;
        outer[axis] = edge * d * 1.001;
        assert!(inside(&cull, inner), "axis {axis} edge {edge}");
        assert!(!inside(&cull, outer), "axis {axis} edge {edge}");
    }
}

/// Catches the pose being ignored: both eyes moved and turned a quarter turn left, the frustum
/// follows (forward is -X) and the space behind the head is outside.
#[test]
fn the_union_follows_the_head_pose() {
    let pose = Pose::new(
        Vec3::new(5.0, 2.0, -3.0),
        Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
    );
    let v = |eye| View {
        eye,
        pose,
        fov: unit_fov(),
    };
    let cull = union_cull_matrix(&[v(Eye::Left), v(Eye::Right)], 100.0).expect("union");
    assert!(inside(&cull, Vec3::new(5.0 - 10.0, 2.0, -3.0)), "ahead along -X");
    assert!(!inside(&cull, Vec3::new(5.0 + 10.0, 2.0, -3.0)), "behind");
    assert!(
        !inside(&cull, Vec3::new(5.0, 2.0, -3.0 - 10.0)),
        "to the side, wider than the 45 degree cone"
    );
    assert!(
        inside(&cull, Vec3::new(5.0 - 10.0, 2.0 + 9.0, -3.0)),
        "inside the vertical cone"
    );
}

/// Catches a bad field of view in either eye being accepted.
#[test]
fn an_invalid_field_of_view_in_either_eye_is_refused() {
    let good = unit_fov();
    let bad = [
        Fov {
            left: f32::NAN,
            ..good
        },
        Fov {
            left: 0.2,
            right: 0.1,
            ..good
        },
        Fov {
            up: -0.1,
            down: 0.1,
            ..good
        },
        Fov { right: 1.6, ..good },
    ];
    for fov in bad {
        let first = [
            view(Eye::Left, Vec3::ZERO, fov),
            view(Eye::Right, Vec3::ZERO, good),
        ];
        let second = [first[1], first[0]];
        assert_eq!(
            union_cull_matrix(&first, 100.0).map_err(|e| e.code()),
            Err("xr_invalid_fov"),
            "{fov:?}"
        );
        assert_eq!(
            union_cull_matrix(&second, 100.0).map_err(|e| e.code()),
            Err("xr_invalid_fov"),
            "{fov:?}"
        );
    }
}

/// Catches `world_from_tracking` composing in the wrong order: the offset is applied in the rig's
/// frame, so a +X offset on a rig turned a quarter turn lands on -Z.
#[test]
fn world_from_tracking_applies_the_offset_in_the_rig_frame() {
    let rig = Pose::new(
        Vec3::new(10.0, 0.0, 0.0),
        Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
    );
    let offset = Pose::new(Vec3::new(1.0, 0.0, 0.0), Quat::from_rotation_y(0.25));
    let world = world_from_tracking(&rig, &offset);
    assert!(
        (world.position - Vec3::new(10.0, 0.0, -1.0)).length() < 1e-5,
        "{:?}",
        world.position
    );
    let expected = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2 + 0.25);
    assert!(world.orientation.angle_between(expected) < 1e-5);
}

/// Catches an eye view that drops the eye's own offset or the world transform: the eye 3 cm to
/// the right of a head standing 2 m up sees a point straight ahead at the origin of its space.
#[test]
fn eye_camera_view_projection_and_cull_are_composed_with_the_world() {
    let fov = Fov::symmetric(0.8, 0.7);
    let eye = view(Eye::Right, Vec3::new(0.03, 0.0, 0.0), fov);
    let world = Pose::new(Vec3::new(0.0, 2.0, 0.0), Quat::IDENTITY);
    let camera = eye_camera(&eye, &world, 0.05, Mat4::IDENTITY).expect("camera");
    let in_view = camera.view * Vec3::new(0.03, 2.0, -1.0).extend(1.0);
    assert!(
        (in_view.truncate() - Vec3::new(0.0, 0.0, -1.0)).length() < 1e-5,
        "{in_view:?}"
    );
    let expected = eye_projection(&fov, 0.05).expect("projection");
    assert_eq!(camera.projection.to_cols_array(), expected.to_cols_array());
    // With an identity tracking-space cull matrix the cull is just the inverse world.
    let culled = camera.cull * Vec3::new(1.0, 5.0, 2.0).extend(1.0);
    assert!(
        (culled.truncate() - Vec3::new(1.0, 3.0, 2.0)).length() < 1e-5,
        "{culled:?}"
    );
    let bad = View {
        fov: Fov {
            left: 1.0,
            right: -1.0,
            ..fov
        },
        ..eye
    };
    assert_eq!(
        eye_camera(&bad, &world, 0.05, Mat4::IDENTITY)
            .map(|_| ())
            .map_err(|e| e.code()),
        Err("xr_invalid_fov")
    );
}

/// Catches a bad near plane reaching the projection: NaN, zero, negative and infinite fall back to
/// the renderer's near; a good one is used as given.
#[test]
fn eye_projection_falls_back_on_a_bad_near_plane() {
    let fov = Fov::symmetric(0.8, 0.8);
    let fallback = eye_projection(&fov, regia_render::view_math::FALLBACK_NEAR)
        .expect("p")
        .to_cols_array();
    for near in [f32::NAN, 0.0, -1.0, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(
            eye_projection(&fov, near).expect("p").to_cols_array(),
            fallback,
            "{near}"
        );
    }
    assert_ne!(eye_projection(&fov, 0.2).expect("p").to_cols_array(), fallback);
    let bad = Fov {
        left: 0.0,
        right: 0.0,
        ..fov
    };
    assert_eq!(
        eye_projection(&bad, 0.1).map(|_| ()).map_err(|e| e.code()),
        Err("xr_invalid_fov")
    );
}

/// Catches a mode that does not survive its own name, and the full two-by-two fallback table.
#[test]
fn stereo_mode_names_and_the_resolution_table() {
    use StereoMode::{Multiview, TwoPass};
    assert_eq!(StereoMode::default(), TwoPass);
    assert_eq!(StereoMode::ALL, [TwoPass, Multiview]);
    assert_eq!((TwoPass.name(), Multiview.name()), ("two_pass", "multiview"));
    for bad in ["", "Multiview", "multi_view", "two-pass", "single_pass"] {
        assert_eq!(StereoMode::from_name(bad), None, "{bad:?}");
    }
    assert_eq!(resolve_stereo_mode(TwoPass, false), TwoPass);
    assert_eq!(resolve_stereo_mode(TwoPass, true), TwoPass);
    assert_eq!(resolve_stereo_mode(Multiview, false), TwoPass);
    assert_eq!(resolve_stereo_mode(Multiview, true), Multiview);
}

fn all_on_quality() -> FrameQuality {
    FrameQuality {
        taau: true,
        render_scale_permille: 700,
        msaa_samples: 4,
        ssr: true,
        screen_space_gi: true,
        gi_probes: true,
        reflection_probes: true,
        volumetric_fog: true,
        volumetric_clouds: true,
        screen_space_refraction: true,
        visibility_buffer: true,
        paged_sun_shadows: true,
        near_sun_tile: 3,
        smaa: true,
        translucent_shadow_cascades: 2,
        translucent_local_shadows: true,
        horizontal_plus: true,
        ..FrameQuality::LAB
    }
}

fn all_on_post() -> PostSettings {
    PostSettings {
        auto_exposure: true,
        taa: true,
        fxaa: false,
        film_grain: 0.4,
        motion_blur: Some(MotionBlur::default()),
        depth_of_field: Some(DepthOfField::default()),
        ..PostSettings::default()
    }
}

/// Catches an effect left on by the XR profile, a disabled list that drifted from the code
/// (every listed name must be switched off), and collateral changes to unrelated settings.
#[test]
fn the_xr_profile_switches_off_exactly_what_it_lists() {
    let before = all_on_quality();
    let mut quality = before;
    let mut post = all_on_post();
    let post_before = post.clone();
    apply_xr_quality(&mut quality, Some(&mut post), true);
    assert!(quality.taau, "per-eye history keeps temporal AA on");
    assert_eq!((quality.render_scale_permille, quality.msaa_samples), (1000, 1));
    assert_eq!(
        (quality.near_sun_tile, quality.translucent_shadow_cascades),
        (0, 0)
    );
    assert!(!quality.smaa && !quality.translucent_local_shadows && !quality.horizontal_plus);
    for (name, reason) in XR_DISABLED_EFFECTS {
        assert!(!reason.is_empty(), "{name} has no reason");
        let off = match name {
            "ssr" => !quality.ssr,
            "ssgi" => !quality.screen_space_gi,
            "gi_probes" => !quality.gi_probes,
            "reflection_probes" => !quality.reflection_probes,
            "volumetric_fog" => !quality.volumetric_fog,
            "volumetric_clouds" => !quality.volumetric_clouds,
            "refraction" => !quality.screen_space_refraction,
            "visibility_buffer" => !quality.visibility_buffer,
            "paged_sun_shadows" => !quality.paged_sun_shadows,
            "motion_blur" => post.motion_blur.is_none(),
            "depth_of_field" => post.depth_of_field.is_none(),
            other => {
                panic!("XR_DISABLED_EFFECTS lists `{other}`, which this test does not know how to check")
            }
        };
        assert!(off, "{name} is listed as disabled but still on");
    }
    let mut names: Vec<_> = XR_DISABLED_EFFECTS.iter().map(|(n, _)| *n).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), XR_DISABLED_EFFECTS.len(), "duplicate names");
    assert!(
        !post.auto_exposure && !post.taa && !post.fxaa,
        "TAA history is on, so FXAA stays off"
    );
    assert_eq!(post.film_grain, 0.0);
    // Nothing unrelated moved.
    assert_eq!(quality.csm_cascade_count, before.csm_cascade_count);
    assert_eq!(quality.csm_resolution, before.csm_resolution);
    assert_eq!(quality.gpu_driven, before.gpu_driven);
    assert_eq!(post.exposure_ev, post_before.exposure_ev);
    assert_eq!(post.bloom, post_before.bloom);
    assert_eq!(post.tonemap, post_before.tonemap);
}

/// Catches `taau &= per_eye_history` turning into an assignment: temporal AA is never switched
/// on by the profile, and without per-eye history FXAA stands in.
#[test]
fn per_eye_history_only_keeps_taau_it_never_creates_it() {
    let mut quality = FrameQuality {
        taau: false,
        ..all_on_quality()
    };
    let mut post = all_on_post();
    apply_xr_quality(&mut quality, Some(&mut post), true);
    assert!(!quality.taau, "stays off");
    assert!(post.fxaa, "FXAA covers for a frame without temporal AA");
    let mut quality = FrameQuality {
        taau: true,
        ..all_on_quality()
    };
    let mut post = all_on_post();
    apply_xr_quality(&mut quality, Some(&mut post), false);
    assert!(!quality.taau && post.fxaa);
    assert!(!post.taa, "the post stack's own TAA is never used in XR");
}

/// Catches a missing post stack panicking, and the quality half not running without it.
#[test]
fn the_xr_profile_works_without_a_post_stack() {
    let mut quality = all_on_quality();
    apply_xr_quality(&mut quality, None, true);
    assert!(!quality.ssr && !quality.volumetric_fog && quality.msaa_samples == 1);
}
