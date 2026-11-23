// SPDX-License-Identifier: MPL-2.0
//! The `Quick` system parameter's deferred actions and degraded behaviour, driven through a plain
//! app: run-mode changes, cursor grab, saving without a renderer, detached drawing handles and the
//! window-size fallbacks.

use std::sync::{Arc, Mutex};

use regia_app::prelude::*;
use regia_core::math::{Vec2, Vec3};
use regia_platform::{CursorGrab, PrimaryWindow, Window};
use regia_quick::{Draw, Draw3d, Quick, QuickLoop, RunMode};
use regia_test_utils::unique_temp_dir;

fn spawn_window(app: &mut App, physical: (u32, u32), logical: (f64, f64), scale: f64) -> Entity {
    app.world_mut()
        .spawn((
            Window {
                physical_size: physical,
                logical_size: logical,
                scale_factor: scale,
                ..Window::default()
            },
            PrimaryWindow,
        ))
        .id()
}

#[test]
fn set_run_mode_applies_after_the_frame_and_restarts_the_counters() {
    let mut app = App::new();
    app.init_resource::<QuickLoop>()
        .insert_resource(RunMode::Loop)
        .add_systems(Update, |q: Quick| q.set_run_mode(RunMode::loop_ntimes(4)));
    app.world_mut().resource_mut::<QuickLoop>().runs = 9;
    app.update();
    assert_eq!(*app.world().resource::<RunMode>(), RunMode::LoopNTimes(4));
    assert_eq!(app.world().resource::<QuickLoop>().runs, 0, "counters restart");
}

#[test]
fn set_run_mode_without_a_quick_loop_just_sets_the_mode() {
    let mut app = App::new();
    app.add_systems(Update, |q: Quick| q.set_run_mode(RunMode::Wait));
    app.update();
    assert_eq!(*app.world().resource::<RunMode>(), RunMode::Wait);
}

#[test]
fn run_mode_reads_the_resource_and_defaults_to_loop() {
    let seen: Arc<Mutex<Vec<RunMode>>> = Arc::default();
    let sink = seen.clone();
    let mut app = App::new();
    app.add_systems(Update, move |q: Quick| {
        sink.lock().expect("lock").push(q.run_mode())
    });
    app.update();
    app.insert_resource(RunMode::Wait);
    app.update();
    assert_eq!(*seen.lock().expect("lock"), vec![RunMode::Loop, RunMode::Wait]);
}

#[test]
fn grab_cursor_locks_and_hides_then_frees_and_shows() {
    let mut app = App::new();
    let window = spawn_window(&mut app, (100, 100), (100.0, 100.0), 1.0);
    app.insert_resource(GrabNow(true));
    app.add_systems(Update, |q: Quick, grab: Res<GrabNow>| q.grab_cursor(grab.0));
    app.update();
    let w = app.world().get::<Window>(window).expect("window");
    assert_eq!(w.cursor_grab, CursorGrab::Locked);
    assert!(!w.cursor_visible);
    app.insert_resource(GrabNow(false));
    app.update();
    let w = app.world().get::<Window>(window).expect("window");
    assert_eq!(w.cursor_grab, CursorGrab::None);
    assert!(w.cursor_visible);
}

#[derive(Resource)]
struct GrabNow(bool);

#[test]
fn grab_cursor_without_a_window_is_a_no_op() {
    let mut app = App::new();
    app.add_systems(Update, |q: Quick| q.grab_cursor(true));
    app.update();
}

#[test]
fn save_frame_without_a_renderer_writes_nothing_and_does_not_panic() {
    let dir = unique_temp_dir("quick-save-frame");
    let path = dir.join("nested").join("frame.png");
    let target = path.clone();
    let mut app = App::new();
    app.add_systems(Update, move |q: Quick| q.save_frame(&target));
    app.update();
    assert!(!path.exists());
    assert!(
        !dir.join("nested").exists(),
        "no directories are created before a capture arrives"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn commands_run_at_the_end_of_the_system_and_return_the_closure_value() {
    let spawned: Arc<Mutex<Option<Entity>>> = Arc::default();
    let sink = spawned.clone();
    let mut app = App::new();
    app.add_systems(Update, move |q: Quick| {
        let id = q.commands(|mut commands| commands.spawn_empty().id());
        *sink.lock().expect("lock") = Some(id);
    });
    app.update();
    let id = spawned.lock().expect("lock").expect("system ran");
    assert!(
        app.world().get_entity(id).is_ok(),
        "the deferred spawn was applied"
    );
}

#[test]
fn draw_handles_are_detached_without_the_plugin_and_shared_with_it() {
    let mut app = App::new();
    app.add_systems(Update, |q: Quick| {
        q.draw().rect().w_h(10.0, 10.0);
        q.draw_3d().cube(Vec3::ZERO, Vec3::ONE);
        assert!(q.draw().is_empty(), "every call returns a fresh detached Draw");
        assert!(q.draw_3d().is_empty());
    });
    app.update();

    let mut app = App::new();
    app.init_resource::<Draw>().init_resource::<Draw3d>();
    app.add_systems(Update, |q: Quick| {
        q.draw().rect().w_h(10.0, 10.0);
        q.draw_3d().cube(Vec3::ZERO, Vec3::ONE);
    });
    app.update();
    assert!(
        !app.world().resource::<Draw>().is_empty(),
        "the resource is shared"
    );
    assert!(!app.world().resource::<Draw3d>().is_empty());
}

#[test]
fn without_resources_everything_reads_as_zero_or_empty() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = seen.clone();
    let mut app = App::new();
    app.add_systems(Update, move |q: Quick| {
        let mut lines = sink.lock().expect("lock");
        lines.push(format!("{} {} {} {}", q.time(), q.delta(), q.frame(), q.runs()));
        lines.push(format!(
            "{:?} {:?} {:?}",
            q.mouse_screen().to_array(),
            q.mouse_delta().to_array(),
            q.mouse_wheel().to_array()
        ));
        lines.push(format!(
            "{} {} {} {}",
            q.key_down("Space"),
            q.key_pressed("Space"),
            q.key_released("Space"),
            q.touches().len()
        ));
        lines.push(format!("{}", q.scale_factor()));
    });
    app.update();
    let lines = seen.lock().expect("lock");
    assert_eq!(lines[0], "0 0 0 0");
    assert_eq!(lines[1], "[0.0, 0.0] [0.0, 0.0] [0.0, 0.0]");
    assert_eq!(lines[2], "false false false 0");
    assert_eq!(lines[3], "1");
}

#[test]
fn window_size_falls_back_to_the_logical_size_while_the_surface_is_zero() {
    let sizes: Arc<Mutex<Vec<Vec2>>> = Arc::default();
    let sink = sizes.clone();
    let mut app = App::new();
    spawn_window(&mut app, (0, 0), (300.0, 200.0), 2.0);
    app.add_systems(Update, move |q: Quick| {
        sink.lock().expect("lock").push(q.window_size());
        let (w, h) = q.window_rect().w_h();
        sink.lock().expect("lock").push(Vec2::new(w, h));
    });
    app.update();
    let sizes = sizes.lock().expect("lock");
    assert_eq!(sizes[0], Vec2::new(300.0, 200.0));
    assert_eq!(sizes[1], Vec2::new(300.0, 200.0));
}

#[test]
fn scale_factor_and_conversions_follow_the_window() {
    let out: Arc<Mutex<Vec<Vec2>>> = Arc::default();
    let sink = out.clone();
    let mut app = App::new();
    spawn_window(&mut app, (800, 400), (400.0, 200.0), 2.0);
    app.add_systems(Update, move |q: Quick| {
        assert_eq!(q.scale_factor(), 2.0);
        let mut v = sink.lock().expect("lock");
        v.push(q.window_size());
        // Physical (0, 0) is the top-left corner; physical (800, 400) the bottom-right.
        v.push(q.screen_to_world(Vec2::new(0.0, 0.0)));
        v.push(q.screen_to_world(Vec2::new(800.0, 400.0)));
        v.push(q.world_to_screen(Vec2::ZERO));
    });
    app.update();
    let v = out.lock().expect("lock");
    assert_eq!(v[0], Vec2::new(400.0, 200.0));
    assert_eq!(v[1], Vec2::new(-200.0, 100.0));
    assert_eq!(v[2], Vec2::new(200.0, -100.0));
    assert_eq!(
        v[3],
        Vec2::new(400.0, 200.0),
        "the origin is the window centre in pixels"
    );
}
