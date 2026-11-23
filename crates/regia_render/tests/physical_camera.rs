// SPDX-License-Identifier: MPL-2.0
//! Physical camera exposure (`docs/specs/30-post-volumes-and-physical-camera.md` § 5, ADR-1464):
//! EV100 from ISO, shutter and aperture, the exposure mode switch, the shutter-driven motion blur,
//! the focal length's field of view, and the order against `Camera::exposure_ev` in
//! `extract_frame`. CPU only.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_render::{
    Camera, CameraExposureMode, DepthOfFieldMode, ExtractedFrame, PhysicalCamera, PostSettings,
    RenderProfile, RenderProfilePreset, RenderSettings, ev100_from_camera, exposure_from_ev100,
    extract_frame, focal_length, vertical_fov_from_focal_length,
};
use regia_scene::{GlobalTransform, Transform};

#[test]
fn ev100_follows_the_iso_2720_relation() {
    // Sunny 16: f/16, 1/125 s, ISO 100 is about EV100 15.
    let sunny = PhysicalCamera {
        aperture_f_stops: 16.0,
        shutter_seconds: 1.0 / 125.0,
        iso: 100.0,
        ..PhysicalCamera::default()
    };
    assert!((sunny.ev100() - 14.97).abs() < 0.01, "{}", sunny.ev100());
    // One stop each way: double the ISO, halve the shutter, open the aperture by sqrt(2).
    let base = sunny.ev100();
    let iso = PhysicalCamera { iso: 200.0, ..sunny };
    assert!((iso.ev100() - (base - 1.0)).abs() < 1.0e-4);
    let slower = PhysicalCamera {
        shutter_seconds: 2.0 / 125.0,
        ..sunny
    };
    assert!((slower.ev100() - (base - 1.0)).abs() < 1.0e-4);
    let wider = PhysicalCamera {
        aperture_f_stops: 16.0 / std::f32::consts::SQRT_2,
        ..sunny
    };
    assert!((wider.ev100() - (base - 1.0)).abs() < 1.0e-4);
    assert_eq!(sunny.ev100(), ev100_from_camera(16.0, 1.0 / 125.0, 100.0));
}

#[test]
fn inherit_leaves_the_post_alone() {
    let lens = PhysicalCamera::default();
    assert_eq!(lens.exposure, CameraExposureMode::Inherit);
    let mut post = PostSettings::default();
    lens.apply_exposure(&mut post);
    assert_eq!(post, PostSettings::default());
}

#[test]
fn manual_pins_a_fixed_ev() {
    let lens = PhysicalCamera {
        exposure: CameraExposureMode::Manual { ev100: 9.5 },
        ..PhysicalCamera::default()
    };
    let mut post = PostSettings::default();
    lens.apply_exposure(&mut post);
    assert!(!post.auto_exposure && !post.use_physical_camera);
    assert_eq!(post.exposure_ev, 9.5);
    assert_eq!(post.exposure_scale(), exposure_from_ev100(9.5));
    // A non-finite manual EV is ignored (the post keeps its own exposure).
    let bad = PhysicalCamera {
        exposure: CameraExposureMode::Manual { ev100: f32::NAN },
        ..PhysicalCamera::default()
    };
    let mut post = PostSettings::default();
    bad.apply_exposure(&mut post);
    assert_eq!(post, PostSettings::default());
}

#[test]
fn physical_exposure_comes_from_the_lens_and_keeps_compensation() {
    let lens = PhysicalCamera {
        exposure: CameraExposureMode::Physical,
        aperture_f_stops: 8.0,
        shutter_seconds: 1.0 / 250.0,
        iso: 400.0,
        ..PhysicalCamera::default()
    };
    let mut post = PostSettings {
        exposure_compensation: 1.0,
        ..PostSettings::default()
    };
    lens.apply_exposure(&mut post);
    assert!(!post.auto_exposure && post.use_physical_camera);
    assert_eq!(
        (post.aperture, post.shutter_seconds, post.iso),
        (8.0, 1.0 / 250.0, 400.0)
    );
    assert_eq!(
        post.exposure_ev,
        lens.ev100(),
        "the fixed EV the no-post paths read"
    );
    let expected = exposure_from_ev100(lens.ev100() - 1.0);
    assert!((post.exposure_scale() - expected).abs() <= expected * 1.0e-6);
    // Stopping down two stops darkens the frame by four.
    let stopped = PhysicalCamera {
        aperture_f_stops: 16.0,
        ..lens
    };
    let mut darker = PostSettings::default();
    stopped.apply_exposure(&mut darker);
    let mut brighter = PostSettings::default();
    lens.apply_exposure(&mut brighter);
    let ratio = brighter.exposure_scale() / darker.exposure_scale();
    assert!((ratio - 4.0).abs() < 1.0e-3, "{ratio}");
}

#[test]
fn the_shutter_sets_the_motion_blur_only_in_physical_mode() {
    let manual_angle = PhysicalCamera {
        shutter_angle: 0.25,
        shutter_seconds: 1.0 / 30.0,
        ..PhysicalCamera::default()
    };
    assert_eq!(manual_angle.shutter_fraction(), 0.25);
    assert_eq!(manual_angle.motion_blur().map(|b| b.shutter_angle), Some(0.25));
    let physical = PhysicalCamera {
        exposure: CameraExposureMode::Physical,
        ..manual_angle
    };
    // 1/30 s against the 30 fps reference: the shutter is open the whole frame.
    assert!((physical.shutter_fraction() - 1.0).abs() < 1.0e-6);
    let default_physical = PhysicalCamera {
        exposure: CameraExposureMode::Physical,
        ..PhysicalCamera::default()
    };
    // The defaults (1/60 s at 30 fps) match the default 180 degree shutter.
    assert!((default_physical.shutter_fraction() - PhysicalCamera::default().shutter_angle).abs() < 1.0e-6);
    let mut post = PostSettings::default();
    physical.apply(
        &mut post,
        &RenderProfile::from_preset(RenderProfilePreset::DesktopHigh),
    );
    assert!((post.motion_blur.map_or(0.0, |b| b.shutter_angle) - 1.0).abs() < 1.0e-6);
    // A broken shutter turns the blur off instead of poisoning it.
    let broken = PhysicalCamera {
        shutter_seconds: f32::NAN,
        ..physical
    };
    assert_eq!(broken.shutter_fraction(), 0.0);
    assert!(broken.motion_blur().is_none());
}

#[test]
fn the_focal_length_sets_the_field_of_view() {
    let lens = PhysicalCamera {
        focal_length: Some(0.035),
        sensor_height: 0.024,
        ..PhysicalCamera::default()
    };
    let fov = lens.vertical_fov().expect("a focal length");
    let expected = 2.0 * (0.024f32 / (2.0 * 0.035)).atan();
    assert!((fov - expected).abs() < 1.0e-6);
    // The inverse of `focal_length`.
    assert!((focal_length(0.024, fov) - 0.035).abs() < 1.0e-6);
    assert!((vertical_fov_from_focal_length(0.024, 0.035) - expected).abs() < 1.0e-6);
    assert!(PhysicalCamera::default().vertical_fov().is_none());
    for bad in [0.0, -1.0, f32::NAN] {
        let lens = PhysicalCamera {
            focal_length: Some(bad),
            ..PhysicalCamera::default()
        };
        assert!(lens.vertical_fov().is_none(), "{bad}");
    }
}

#[test]
fn the_aperture_drives_both_focus_and_exposure() {
    let lens = PhysicalCamera {
        exposure: CameraExposureMode::Physical,
        depth_of_field: Some(DepthOfFieldMode::Bokeh),
        aperture_f_stops: 2.0,
        focal_distance: 5.0,
        ..PhysicalCamera::default()
    };
    let stopped = PhysicalCamera {
        aperture_f_stops: 8.0,
        ..lens
    };
    let f = focal_length(lens.sensor_height, 0.6);
    let coc = |l: &PhysicalCamera| l.depth_of_field().map_or(0.0, |d| d.circle_of_confusion(f, 20.0));
    assert!(
        coc(&stopped) < coc(&lens) * 0.3,
        "stopping down deepens the focus"
    );
    assert!(stopped.ev100() > lens.ev100() + 3.9, "and darkens the frame");
}

fn extract_one(camera: Camera, lens: Option<PhysicalCamera>) -> (PostSettings, f32) {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
    let world = GlobalTransform(Transform::from_translation(Vec3::new(0.0, 1.0, 5.0)).compute_affine());
    let mut entity = app.world_mut().spawn((world, camera));
    if let Some(lens) = lens {
        entity.insert(lens);
    }
    app.add_systems(Render, extract_frame);
    app.update();
    let frame = app.world().resource::<ExtractedFrame>();
    let view = &frame.views[0];
    (view.post.clone(), view.camera.fov_y)
}

#[test]
fn a_lens_mode_wins_over_the_camera_ev_pin_and_inherit_keeps_it() {
    let pinned = Camera {
        exposure_ev: Some(12.0),
        ..Camera::default()
    };
    let (post, _) = extract_one(pinned.clone(), Some(PhysicalCamera::default()));
    assert_eq!(post.exposure_ev, 12.0, "inherit keeps the camera's pin");
    assert!(!post.auto_exposure && !post.use_physical_camera);

    let physical = PhysicalCamera {
        exposure: CameraExposureMode::Physical,
        ..PhysicalCamera::default()
    };
    let (post, _) = extract_one(pinned.clone(), Some(physical));
    assert!(post.use_physical_camera && !post.auto_exposure);
    assert_eq!(post.exposure_ev, physical.ev100());

    let manual = PhysicalCamera {
        exposure: CameraExposureMode::Manual { ev100: 7.0 },
        ..PhysicalCamera::default()
    };
    let (post, _) = extract_one(pinned, Some(manual));
    assert_eq!(post.exposure_ev, 7.0);
}

#[test]
fn extraction_applies_the_focal_length() {
    let camera = Camera::default();
    let (_, fov) = extract_one(camera.clone(), None);
    assert_eq!(fov, camera.fov_y);
    let lens = PhysicalCamera {
        focal_length: Some(0.05),
        ..PhysicalCamera::default()
    };
    let (_, fov) = extract_one(camera, Some(lens));
    assert_eq!(Some(fov), lens.vertical_fov());
}
