// SPDX-License-Identifier: MPL-2.0
//! Leashed photo mode through the plugin, without a GPU and without sleeps.
//!
//! The DO27 Python file is requirements only. Its constant-range, tick-format and
//! no-clock checks are these tests. Its key-default, controls-page and `do27_` name
//! checks are dropped: the brief sends actions through the input map and names stills
//! `<game>-<map>-t<tick>-<n>.png`.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::photo_mode::{
    EXIT_ACTION, FAST_ACTION, FREE_RADIUS_M, MOVE_ACTION, MatchAccess, ONLINE_RADIUS_M, PhotoMode,
    PhotoModePlugin, TOGGLE_ACTION, capture_name, offset_len, pauses_sim,
};
use regia_input::InputInjection;
use regia_scene::Transform;

#[derive(Resource, Debug)]
struct SimInput {
    throttle: f32,
}

fn harness(access: MatchAccess) -> App {
    let mode = PhotoMode::fixed_frame(access, 1.0, 2, 2, false);
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(mode)
        .add_plugins(PhotoModePlugin)
        .insert_resource(SimInput { throttle: 0.25 });
    enable_fixed_step(&mut app, 60.0);
    let mut injection = InputInjection::default();
    injection.hold(TOGGLE_ACTION, Vec3::X);
    injection.hold(MOVE_ACTION, Vec3::Y);
    injection.hold(FAST_ACTION, Vec3::X);
    app.insert_resource(injection);
    app
}

fn eye_offset(app: &App) -> f32 {
    offset_len(app.world().resource::<PhotoMode>().state.camera.offset)
}

#[test]
fn photo_camera_never_leaves_the_leash() {
    let mut app = harness(MatchAccess::default());
    let anchor = app.world_mut().spawn(Transform::IDENTITY).id();
    app.world_mut().resource_mut::<PhotoMode>().follow = Some(anchor);
    for _ in 0..4 {
        app.update();
        let len = eye_offset(&app);
        assert!(len <= FREE_RADIUS_M + 1.0e-3, "free eye left the leash: {len}");
        assert!(app.world().resource::<PhotoMode>().state.active);
    }
    assert!(
        eye_offset(&app) > ONLINE_RADIUS_M,
        "the free leash is the wide one"
    );
    let moved = Transform {
        translation: Vec3::new(80.0, 3.0, -20.0),
        ..Transform::IDENTITY
    };
    *app.world_mut().get_mut::<Transform>(anchor).expect("anchor") = moved;
    app.update();
    let mode = app.world().resource::<PhotoMode>();
    let eye = mode.state.camera.eye(mode.state.anchor);
    let from_anchor = offset_len([
        eye[0] - moved.translation.x,
        eye[1] - moved.translation.y,
        eye[2] - moved.translation.z,
    ]);
    assert!(
        from_anchor <= FREE_RADIUS_M + 1.0e-3,
        "the leash follows the anchor: {from_anchor}"
    );
    assert!(!app.world().resource::<Time>().paused);
}

#[test]
fn the_online_leash_is_shorter() {
    let mut online = harness(MatchAccess {
        networked: true,
        ..MatchAccess::default()
    });
    for _ in 0..5 {
        online.update();
        let len = eye_offset(&online);
        assert!(
            len <= ONLINE_RADIUS_M + 1.0e-3,
            "online eye left the short leash: {len}"
        );
        assert!(
            !online.world().resource::<PhotoMode>().status_json()["pause_sim"]
                .as_bool()
                .expect("bool")
        );
    }
    let mut free = harness(MatchAccess::default());
    free.update();
    let wide = eye_offset(&free);
    assert!(
        wide > ONLINE_RADIUS_M && wide <= FREE_RADIUS_M + 1.0e-3,
        "free roam is wider: {wide}"
    );
}

#[test]
fn capture_name_carries_the_tick_not_the_time() {
    assert_eq!(
        capture_name("ridge", "north", 123, 1),
        "ridge-north-t000123-1.png"
    );
    assert_eq!(
        capture_name("ridge", "north", 1_234_567, 1),
        "ridge-north-t1234567-1.png"
    );
    let name = capture_name("ridge", "Switchbacks!", 7, 2);
    assert_eq!(name, "ridge-switchbacks-t000007-2.png");
    assert!(!name.contains(':'));
    let src = include_str!("../src/photo_mode.rs");
    assert!(src.contains("{tick:06}"), "the tick stays zero-padded");
    for banned in ["SystemTime", "Instant::now", "TOGGLE_DEFAULT", "PICTURES_SUBDIR"] {
        assert!(!src.contains(banned), "{banned} is not part of the port");
    }
    #[cfg(feature = "client")]
    {
        let dir = regia_test_utils::unique_temp_dir("photo_capture_name");
        let (written, path) =
            regia_framework::photo_mode::write_capture_png(&dir, "ridge", "north", 123, 2, 2).expect("write");
        assert_eq!(written, "ridge-north-t000123-1.png");
        let (second, _) = regia_framework::photo_mode::write_capture_png(&dir, "ridge", "north", 123, 2, 2)
            .expect("second");
        assert_eq!(second, "ridge-north-t000123-2.png");
        let bytes = std::fs::read(&path).expect("read");
        assert!(bytes.starts_with(b"\x89PNG"), "the still is a png");
        std::fs::remove_dir_all(&dir).expect("remove");
    }
}

#[test]
fn photo_mode_never_writes_sim_input() {
    let mut app = harness(MatchAccess {
        networked: true,
        ..MatchAccess::default()
    });
    let held = app.world().resource::<InputInjection>().scheduled_count();
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(app.world().resource::<SimInput>().throttle, 0.25);
    assert!(!app.world().resource::<Time>().paused);
    assert!(!pauses_sim(app.world().resource::<PhotoMode>().access));
    assert_eq!(app.world().resource::<InputInjection>().scheduled_count(), held);
    let status = app.world().resource::<PhotoMode>().status_json();
    assert_eq!(status["pause_sim"], false);
    assert_eq!(status["active"], true);
}

#[test]
fn do27_requirement_cases_hold_in_rust() {
    use regia_framework::photo_mode::{
        APERTURE_DEFAULT_F, APERTURE_MAX_F, APERTURE_MIN_F, CAPTURE_MAX_DIM, EXPOSURE_MAX_EV,
        EXPOSURE_MIN_EV, FALLBACK_SIZE, FOCUS_DEFAULT_M, FOCUS_MAX_M, FOCUS_MIN_M, FOV_DEFAULT_DEG,
        FOV_MAX_DEG, FOV_MIN_DEG, MOVE_SPEED_M_S, RADIUS_M, ROLL_MAX_DEG,
    };
    assert!((5.0..=100.0).contains(&RADIUS_M));
    assert!((30.0..=360.0).contains(&ROLL_MAX_DEG));
    assert!((1.0..=20.0).contains(&MOVE_SPEED_M_S));
    const {
        assert!(10.0 <= FOV_MIN_DEG && FOV_MIN_DEG < FOV_DEFAULT_DEG && FOV_DEFAULT_DEG < FOV_MAX_DEG);
        assert!(FOV_MAX_DEG <= 180.0);
        assert!(EXPOSURE_MIN_EV == -EXPOSURE_MAX_EV);
        assert!(EXPOSURE_MAX_EV <= 5.0);
        assert!(0.0 < FOCUS_MIN_M && FOCUS_MIN_M < FOCUS_DEFAULT_M && FOCUS_DEFAULT_M < FOCUS_MAX_M);
        assert!(1.0 <= APERTURE_MIN_F && APERTURE_MIN_F < APERTURE_DEFAULT_F);
        assert!(APERTURE_DEFAULT_F < APERTURE_MAX_F && APERTURE_MAX_F <= 32.0);
        assert!(CAPTURE_MAX_DIM <= 16_384);
        let _ = FALLBACK_SIZE;
    };
    let src = include_str!("../src/photo_mode.rs");
    let lib = include_str!("../src/lib.rs");
    assert!(lib.contains("pub mod photo_mode;"));
    for banned in [
        "HashMap",
        "SystemTime",
        "Instant::now",
        "unsafe",
        "ironreach",
        "do27_",
        "/home/player",
        "Pictures/DO27",
        "Shift+;",
    ] {
        assert!(!src.contains(banned), "{banned} must not be in the port");
    }
    for action in [
        "photo_toggle",
        "photo_capture",
        "photo_exit",
        "photo_move",
        "photo_look",
    ] {
        assert!(src.contains(action), "{action} is an input-map action");
    }
}

#[cfg(feature = "client")]
#[test]
fn photo_hud_hides_only_while_open() {
    let mut app = harness(MatchAccess::default());
    app.world_mut()
        .resource_mut::<InputInjection>()
        .release(MOVE_ACTION);
    app.world_mut()
        .resource_mut::<InputInjection>()
        .release(FAST_ACTION);
    let hud = app
        .world_mut()
        .spawn((
            regia_framework::sequencer::cutscene::HudLayer("game".to_string()),
            regia_ui::NodeStyle::default(),
        ))
        .id();
    app.update();
    let hidden = app
        .world()
        .get::<regia_ui::NodeStyle>(hud)
        .expect("style")
        .0
        .display;
    assert_eq!(hidden, regia_ui::Display::None);
    app.world_mut()
        .resource_mut::<InputInjection>()
        .hold(EXIT_ACTION, Vec3::X);
    app.update();
    let shown = app
        .world()
        .get::<regia_ui::NodeStyle>(hud)
        .expect("style")
        .0
        .display;
    assert_eq!(shown, regia_ui::Display::Flex);
    assert!(!app.world().resource::<PhotoMode>().state.active);
}

#[cfg(feature = "client")]
#[test]
fn photo_camera_applies_exposure_once() {
    let mut app = harness(MatchAccess::default());
    app.world_mut()
        .resource_mut::<InputInjection>()
        .release(MOVE_ACTION);
    app.world_mut()
        .resource_mut::<InputInjection>()
        .release(FAST_ACTION);
    let camera = regia_render::Camera {
        exposure_ev: Some(10.0),
        fov_y: 0.5,
        ..regia_render::Camera::default()
    };
    let view = app
        .world_mut()
        .spawn((
            regia_framework::photo_mode::PhotoView,
            Transform::IDENTITY,
            camera,
            regia_render::PhysicalCamera::default(),
        ))
        .id();
    app.update();
    {
        let written = app.world().get::<regia_render::Camera>(view).expect("camera");
        assert!((written.fov_y - 55.0_f32.to_radians()).abs() < 1.0e-4);
        assert_eq!(written.exposure_ev, Some(10.0));
        let forward = app.world().get::<Transform>(view).expect("transform").rotation * Vec3::NEG_Z;
        assert!((forward - Vec3::Z).length() < 1.0e-4, "{forward:?}");
    }
    app.world_mut()
        .resource_mut::<PhotoMode>()
        .state
        .camera
        .exposure_ev = 1.0;
    app.update();
    app.update();
    let written = app.world().get::<regia_render::Camera>(view).expect("camera");
    assert_eq!(written.exposure_ev, Some(11.0), "the bias is applied once");
    let lens = app
        .world()
        .get::<regia_render::PhysicalCamera>(view)
        .expect("lens");
    assert_eq!(lens.focal_distance, regia_framework::photo_mode::FOCUS_DEFAULT_M);
    assert_eq!(
        lens.aperture_f_stops,
        regia_framework::photo_mode::APERTURE_DEFAULT_F
    );
    assert!(lens.focal_length.is_none());
    app.world_mut()
        .resource_mut::<InputInjection>()
        .hold(EXIT_ACTION, Vec3::X);
    app.update();
    let restored = app.world().get::<regia_render::Camera>(view).expect("camera");
    assert_eq!(restored.exposure_ev, Some(10.0));
    assert!((restored.fov_y - 0.5).abs() < 1.0e-5);
}
