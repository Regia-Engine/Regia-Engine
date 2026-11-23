// SPDX-License-Identifier: MIT-0
//! The interactive demo (`showcase --demo`, ADR-3013) in an app.
//!
//! The first tests need no GPU: the demo on the example scene's level, weather and camera, driven through its
//! agent handlers, its panel and its benchmark on a fixed frame clock. The last runs the headless demo on the
//! headless device (lavapipe on the CI hosts) and calls every `demo.*` method through the agent server, with an
//! offscreen screenshot; without any adapter it prints a note and passes, like the other render tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_app::TimeUpdateStrategy;
use regia_core::math::{Quat, Vec3};
use regia_platform::{
    ButtonState, GamepadAxis, GamepadAxisChanged, GamepadButton, GamepadButtonChanged, GamepadConnected,
    InputTimestamp, KeyboardInput, MouseButton, MouseButtonInput, MouseMotion,
};
use regia_render::{Camera, DirectionalLight, QualityTier, RenderSettings};
use regia_scene::level::LevelSettings;
use regia_scene::{GlobalTransform, Transform};
use regia_test_utils::unique_temp_dir;
use regia_vfx::Weather;
use serde_json::{Value, json};
use showcase::app::{Options, ShowcaseScene, StartOptions, look_rotation};
use showcase::demo::fly::DemoCamera;
use showcase::demo::fps::FrameClock;
use showcase::demo::ui::{ControlsPanel, CreditsPanel, DemoWidget};
use showcase::demo::{CaptureMode, DemoLastError, DemoOptions, DemoPage, DemoPlugin, DemoState, agent};
use showcase::scene::{SceneData, load_scene};

fn example_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene")
}

fn example() -> Arc<SceneData> {
    Arc::new(load_scene(&example_dir()).expect("example scene"))
}

/// The demo over what the showcase plugin inserts (scene, level, weather, a camera), without a renderer.
fn cpu_app(out: &Path) -> App {
    cpu_app_on(example(), out)
}

/// [`cpu_app`] on a given scene.
fn cpu_app_on(scene: Arc<SceneData>, out: &Path) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(ShowcaseScene(Arc::clone(&scene)))
        .insert_resource(LevelSettings {
            atmosphere: scene.sky.atmosphere.clone(),
            ..LevelSettings::default()
        })
        .insert_resource(scene.sky.initial_weather())
        .insert_resource(StartOptions(Options::default()));
    let eye = Vec3::new(0.0, 60.0, 40.0);
    let transform = Transform {
        translation: eye,
        rotation: look_rotation(eye, Vec3::new(0.0, 30.0, -60.0)),
        scale: Vec3::ONE,
    };
    app.world_mut().spawn((
        transform,
        GlobalTransform(transform.compute_affine()),
        Camera::default(),
    ));
    app.add_plugins(DemoPlugin {
        options: DemoOptions {
            out_dir: out.to_path_buf(),
            tier: Some(QualityTier::High),
            capture: CaptureMode::Offscreen(64, 36),
            ..DemoOptions::default()
        },
    });
    app.insert_resource(FrameClock::Fixed(10.0));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_nanos(
        16_666_667,
    )));
    app.finish_plugins();
    app.update();
    app
}

fn call(app: &mut App, method: &str, params: Value) -> Value {
    agent::call(app.world_mut(), method, &params).unwrap_or_else(|e| panic!("{method}: {e:?}"))
}

fn refused(app: &mut App, method: &str, params: Value) -> String {
    let error = agent::call(app.world_mut(), method, &params).expect_err("must be refused");
    error
        .data
        .and_then(|d| d["code"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn camera(app: &mut App) -> (Vec3, Quat) {
    let mut q = app.world_mut().query_filtered::<&Transform, With<DemoCamera>>();
    let t = q
        .iter(app.world())
        .next()
        .expect("the demo flies the scene camera");
    (t.translation, t.rotation)
}

#[test]
fn the_sliders_and_the_picker_change_the_level_the_weather_and_the_renderer() {
    let out = unique_temp_dir("showcase-demo-controls");
    let mut app = cpu_app(&out);

    let tod = call(
        &mut app,
        "demo.set_time_of_day",
        json!({ "hour": 18.5, "day_of_year": 200 }),
    );
    assert_eq!(tod["hour"], 18.5);
    let level = app
        .world()
        .resource::<LevelSettings>()
        .atmosphere
        .time_of_day
        .expect("sun clock");
    assert_eq!((level.hour, level.day_of_year), (18.5, 200));
    assert_eq!(
        refused(&mut app, "demo.set_time_of_day", json!({ "hour": 30 })),
        "demo_bad_value"
    );
    assert_eq!(
        refused(&mut app, "demo.set_time_of_day", json!({ "hour": "noon" })),
        "demo_bad_value"
    );

    call(
        &mut app,
        "demo.set_weather",
        json!({ "preset": "overcast", "wind_m_per_s": 12.0 }),
    );
    let target = app.world().resource::<Weather>().target();
    let overcast = regia_vfx::WeatherState::preset("overcast").expect("preset");
    assert!((target.coverage - overcast.coverage).abs() < 1e-5);
    assert!((target.wind_m_per_s[0].hypot(target.wind_m_per_s[1]) - 12.0).abs() < 1e-3);
    let slid = call(
        &mut app,
        "demo.set_weather",
        json!({ "coverage": 0.25, "precipitation": 0.5 }),
    );
    assert_eq!(slid["preset"], Value::Null, "a slider clears the preset");
    assert!((app.world().resource::<Weather>().target().precipitation - 0.5).abs() < 1e-6);
    assert_eq!(
        refused(&mut app, "demo.set_weather", json!({ "preset": "hail_of_frogs" })),
        "demo_unknown_weather"
    );

    call(&mut app, "demo.set_quality", json!({ "tier": "Medium" }));
    let scale = app.world().resource::<RenderSettings>().profile.render_scale;
    assert!((scale - QualityTier::MEDIUM_RENDER_SCALE).abs() < 1e-6);
    assert_eq!(
        refused(&mut app, "demo.set_quality", json!({ "tier": "Ultra9000" })),
        "demo_unknown_tier"
    );
    assert_eq!(
        refused(&mut app, "demo.set_quality", json!({ "tier": "auto" })),
        "demo_no_renderer",
        "detection reads the GPU"
    );
    assert_eq!(
        refused(&mut app, "demo.screenshot", json!({})),
        "demo_no_renderer"
    );
    assert_eq!(
        refused(&mut app, "demo.screenshot", json!({ "path": "../x.png" })),
        "demo_bad_value"
    );

    let state = call(&mut app, "demo.state", json!({}));
    assert_eq!(state["quality"]["tier"], "Medium");
    assert_eq!(state["quality"]["source"], "picked");
    assert_eq!(state["time_of_day"]["hour"], 18.5);
    assert_eq!(state["credits"]["assets"], 12);
    assert_eq!(state["credits"]["unlisted"], 0);

    let credits = call(&mut app, "demo.credits", json!({}));
    assert_eq!(credits["assets"].as_array().map(Vec::len), Some(12));
    assert_eq!(credits["unlisted"], json!([]));
    assert!(
        credits["lines"][0]
            .as_str()
            .unwrap_or_default()
            .contains("Regia Engine")
    );

    call(&mut app, "demo.show", json!({ "page": "hidden" }));
    assert_eq!(app.world().resource::<DemoState>().page, DemoPage::Hidden);
    assert_eq!(
        refused(&mut app, "demo.show", json!({ "page": "menu" })),
        "demo_unknown_page"
    );
}

#[test]
fn the_authored_sun_follows_the_clock_and_the_clouds() {
    let out = unique_temp_dir("showcase-demo-sun");
    let mut app = cpu_app(&out);
    app.world_mut().spawn(DirectionalLight {
        direction: [0.3, -0.8, 0.5],
        color: [1.0, 0.96, 0.9],
        illuminance: 100_000.0,
        cast_shadows: true,
        static_shadow: false,
    });
    let sun = |app: &mut App| {
        let mut q = app.world_mut().query::<&DirectionalLight>();
        let light = q.single(app.world()).expect("one sun");
        (light.direction, light.illuminance)
    };
    call(&mut app, "demo.set_weather", json!({ "preset": "clear" }));
    call(&mut app, "demo.set_time_of_day", json!({ "hour": 8.0 }));
    updates(&mut app, 2);
    let (morning, morning_lux) = sun(&mut app);
    call(&mut app, "demo.set_time_of_day", json!({ "hour": 16.0 }));
    updates(&mut app, 2);
    let (afternoon, _) = sun(&mut app);
    assert!(
        morning[0] * afternoon[0] < 0.0,
        "the shadows swing: {morning:?} {afternoon:?}"
    );
    assert!(morning_lux > 1000.0, "{morning_lux}");
    call(&mut app, "demo.set_time_of_day", json!({ "hour": 12.0 }));
    updates(&mut app, 2);
    let (_, clear) = sun(&mut app);
    call(&mut app, "demo.set_weather", json!({ "coverage": 1.0 }));
    updates(&mut app, 2);
    let (_, overcast) = sun(&mut app);
    assert!(overcast < clear * 0.5, "clear {clear}, overcast {overcast}");
    call(&mut app, "demo.set_time_of_day", json!({ "hour": 23.0 }));
    updates(&mut app, 2);
    assert_eq!(sun(&mut app).1, 0.0, "no sun at night");
}

#[test]
fn the_demo_turns_on_the_rain_its_slider_promises_once() {
    let out = unique_temp_dir("showcase-demo-rain");
    let mut app = cpu_app(&out);
    let settings = regia_vfx::PrecipitationSettings {
        enabled: false,
        ..regia_vfx::PrecipitationSettings::default()
    };
    app.insert_resource(settings);
    app.update();
    let enabled = |app: &App| app.world().resource::<regia_vfx::PrecipitationSettings>().enabled;
    assert!(enabled(&app), "off by default for a project, on in the demo");
    app.world_mut()
        .resource_mut::<regia_vfx::PrecipitationSettings>()
        .enabled = false;
    updates(&mut app, 2);
    assert!(
        !enabled(&app),
        "an agent's render.set_precipitation has the last word"
    );
}

#[test]
fn the_camera_flies_where_it_looks_and_turns_with_the_look_input() {
    let out = unique_temp_dir("showcase-demo-fly");
    let mut app = cpu_app(&out);
    app.update();
    let (start, rotation) = camera(&mut app);
    let forward = rotation * Vec3::NEG_Z;
    call(&mut app, "demo.fly", json!({ "move": [0.0, 1.0], "frames": 60 }));
    for _ in 0..60 {
        app.update();
    }
    let (moved, _) = camera(&mut app);
    let travel = moved - start;
    assert!(travel.length() > 5.0, "one second at cruise speed: {travel:?}");
    assert!(
        travel.normalize().dot(forward) > 0.95,
        "forward is where the camera looks"
    );

    call(&mut app, "demo.fly", json!({ "look": [0.05, 0.0], "frames": 10 }));
    for _ in 0..12 {
        app.update();
    }
    let (_, turned) = camera(&mut app);
    let (yaw_before, _) = showcase::demo::fly::yaw_pitch(rotation);
    let (yaw_after, _) = showcase::demo::fly::yaw_pitch(turned);
    assert!(
        (yaw_before - yaw_after - 0.5).abs() < 1e-3,
        "ten frames of 0.05 rad turn right by 0.5 rad"
    );
    assert_eq!(
        refused(&mut app, "demo.fly", json!({ "move": [3.0, 0.0] })),
        "demo_bad_value"
    );
    assert_eq!(
        refused(&mut app, "demo.fly", json!({ "frames": 0 })),
        "demo_bad_value"
    );
}

#[test]
fn the_generated_example_scene_has_views_and_credits_without_the_source_tree() {
    let root = unique_temp_dir("showcase-demo-generated");
    let dir = root.join("example_scene");
    showcase::example::write_demo_example_scene(&dir).expect("write the scene");
    // Launches share the folder: writing it again over itself is fine and changes nothing.
    let manifest = std::fs::read(dir.join("asset_manifest.json")).expect("manifest");
    showcase::example::write_demo_example_scene(&dir).expect("write the scene again");
    assert_eq!(
        std::fs::read(dir.join("asset_manifest.json")).expect("manifest"),
        manifest
    );

    let scene = Arc::new(load_scene(&dir).expect("the generated scene loads"));
    let mut app = cpu_app_on(scene, &root.join("out"));

    let credits = call(&mut app, "demo.credits", json!({}));
    assert_eq!(credits["error"], Value::Null);
    assert_eq!(
        credits["unlisted"],
        json!([]),
        "every generated file has a credit"
    );
    let listed: Vec<&str> = credits["assets"]
        .as_array()
        .expect("assets")
        .iter()
        .map(|a| a["path"].as_str().unwrap_or_default())
        .collect();
    assert!(listed.contains(&"perf_views.json"), "{listed:?}");
    for path in &listed {
        assert!(
            dir.join(path).is_file(),
            "{path}: a row for a file the scene does not have"
        );
    }

    let started = call(&mut app, "demo.benchmark", json!({ "warmup": 0, "frames": 2 }));
    let views = started["views"].as_array().expect("views").len();
    assert!(views >= 3);
    updates(&mut app, u32::try_from(views * 2 + 2).expect("few views"));
    assert!(!app.world().resource::<DemoState>().bench.running());
    assert!(root.join("out/benchmark/showcase-table.md").is_file());
}

/// One key message, as the window's event loop writes it.
fn key(app: &mut App, name: &str, state: ButtonState) {
    app.world_mut().write_message(KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: name.to_owned(),
        logical_key: name.to_owned(),
        state,
        repeat: false,
        timestamp: InputTimestamp::ZERO,
    });
}

/// A pad button press and release on two updates.
fn tap_pad(app: &mut App, button: GamepadButton) {
    for (state, value) in [(ButtonState::Pressed, 1.0), (ButtonState::Released, 0.0)] {
        app.world_mut().write_message(GamepadButtonChanged {
            device: 1,
            button,
            state,
            value,
        });
        app.update();
    }
}

fn updates(app: &mut App, n: u32) {
    for _ in 0..n {
        app.update();
    }
}

#[test]
fn the_keyboard_and_the_mouse_fly_the_camera_through_the_demos_bindings() {
    let out = unique_temp_dir("showcase-demo-keys");
    let mut app = cpu_app(&out);
    app.update();
    let (start, rotation) = camera(&mut app);
    let forward = rotation * Vec3::NEG_Z;

    key(&mut app, "KeyW", ButtonState::Pressed);
    updates(&mut app, 60);
    key(&mut app, "KeyW", ButtonState::Released);
    updates(&mut app, 60);
    let (moved, _) = camera(&mut app);
    let travel = moved - start;
    assert!(travel.length() > 5.0, "W flies at cruise speed: {travel:?}");
    assert!(travel.normalize().dot(forward) > 0.95, "W flies forward");

    key(&mut app, "KeyE", ButtonState::Pressed);
    updates(&mut app, 30);
    key(&mut app, "KeyE", ButtonState::Released);
    updates(&mut app, 60);
    let (higher, _) = camera(&mut app);
    assert!(higher.y > moved.y + 1.0, "E flies up: {moved:?} to {higher:?}");

    // Mouse motion turns the camera only while the right button is held.
    let motion = |app: &mut App| {
        app.world_mut().write_message(MouseMotion {
            delta: (40.0, 0.0),
            timestamp: InputTimestamp::ZERO,
        });
    };
    let (_, before) = camera(&mut app);
    motion(&mut app);
    updates(&mut app, 2);
    assert_eq!(camera(&mut app).1, before, "no button, no turn");
    app.world_mut().write_message(MouseButtonInput {
        window: Entity::PLACEHOLDER,
        button: MouseButton::Right,
        state: ButtonState::Pressed,
        timestamp: InputTimestamp::ZERO,
    });
    app.update();
    for _ in 0..5 {
        motion(&mut app);
        app.update();
    }
    app.update();
    let (_, turned) = camera(&mut app);
    let (yaw_before, _) = showcase::demo::fly::yaw_pitch(before);
    let (yaw_after, _) = showcase::demo::fly::yaw_pitch(turned);
    let want = 5.0 * 40.0 * showcase::demo::fly::MOUSE_RADIANS_PER_PIXEL;
    assert!(
        (yaw_before - yaw_after - want).abs() < 1e-3,
        "five 40-pixel moves right turn right by {want} rad, got {}",
        yaw_before - yaw_after
    );
}

#[test]
fn the_gamepad_flies_the_camera_through_the_demos_bindings() {
    let out = unique_temp_dir("showcase-demo-pad");
    let mut app = cpu_app(&out);
    app.world_mut().write_message(GamepadConnected {
        device: 1,
        name: "test pad".to_owned(),
    });
    updates(&mut app, 2);
    let (start, rotation) = camera(&mut app);
    let forward = rotation * Vec3::NEG_Z;

    let axis = |app: &mut App, axis: GamepadAxis, value: f32| {
        app.world_mut().write_message(GamepadAxisChanged {
            device: 1,
            axis,
            value,
        });
    };
    axis(&mut app, GamepadAxis::LeftStickY, 1.0);
    updates(&mut app, 60);
    axis(&mut app, GamepadAxis::LeftStickY, 0.0);
    updates(&mut app, 60);
    let (moved, _) = camera(&mut app);
    let travel = moved - start;
    assert!(travel.length() > 5.0, "the stick flies: {travel:?}");
    assert!(travel.normalize().dot(forward) > 0.95, "pushed up is forward");

    // The right stick turns by time: 30 updates of 1/60 s at a full push.
    let (_, before) = camera(&mut app);
    axis(&mut app, GamepadAxis::RightStickX, 1.0);
    updates(&mut app, 30);
    axis(&mut app, GamepadAxis::RightStickX, 0.0);
    updates(&mut app, 2);
    let (_, turned) = camera(&mut app);
    let (yaw_before, _) = showcase::demo::fly::yaw_pitch(before);
    let (yaw_after, _) = showcase::demo::fly::yaw_pitch(turned);
    let want = 0.5 * showcase::demo::fly::PAD_RADIANS_PER_SECOND;
    assert!(
        (yaw_before - yaw_after - want).abs() < 0.05,
        "half a second at a full push turns right by {want} rad, got {}",
        yaw_before - yaw_after
    );

    for (button, name) in [
        (GamepadButton::RightShoulder, "right shoulder"),
        (GamepadButton::RightTrigger, "right trigger"),
    ] {
        let (low, _) = camera(&mut app);
        app.world_mut().write_message(GamepadButtonChanged {
            device: 1,
            button,
            state: ButtonState::Pressed,
            value: 1.0,
        });
        updates(&mut app, 30);
        app.world_mut().write_message(GamepadButtonChanged {
            device: 1,
            button,
            state: ButtonState::Released,
            value: 0.0,
        });
        updates(&mut app, 60);
        let (high, _) = camera(&mut app);
        assert!(high.y > low.y + 1.0, "the {name} flies up");
    }
}

#[test]
fn the_keys_and_the_pad_buttons_reach_the_same_controls() {
    let out = unique_temp_dir("showcase-demo-hotkeys");
    let mut app = cpu_app(&out);
    let tap = |app: &mut App, name: &str| {
        key(app, name, ButtonState::Pressed);
        app.update();
        key(app, name, ButtonState::Released);
        app.update();
    };
    let page = |app: &App| app.world().resource::<DemoState>().page;
    let running = |app: &App| app.world().resource::<DemoState>().bench.running();

    tap(&mut app, "F2");
    assert_eq!(page(&app), DemoPage::Credits);
    tap(&mut app, "F2");
    assert_eq!(page(&app), DemoPage::Controls);
    tap(&mut app, "F1");
    assert_eq!(page(&app), DemoPage::Hidden);
    tap(&mut app, "F1");
    assert_eq!(page(&app), DemoPage::Controls);

    // This app has no renderer: the key reaches the screenshot command, which refuses with the code.
    tap(&mut app, "F12");
    let refused_code = |app: &App| {
        app.world()
            .resource::<DemoLastError>()
            .0
            .as_ref()
            .map(|e| e.code.as_str())
    };
    assert_eq!(refused_code(&app), Some("demo_no_renderer"));

    tap(&mut app, "Escape");
    assert!(!running(&app), "Esc with no benchmark does nothing");
    tap(&mut app, "F9");
    assert!(running(&app), "F9 starts the benchmark");
    tap(&mut app, "Escape");
    assert!(!running(&app), "Esc stops it");
    assert!(out.join("benchmark/showcase-table.md").is_file());

    app.world_mut().write_message(GamepadConnected {
        device: 1,
        name: "test pad".to_owned(),
    });
    updates(&mut app, 2);
    tap_pad(&mut app, GamepadButton::Select);
    assert_eq!(page(&app), DemoPage::Hidden, "Select hides the panel");
    tap_pad(&mut app, GamepadButton::Start);
    assert!(running(&app), "Start starts the benchmark");
    tap_pad(&mut app, GamepadButton::Start);
    assert!(!running(&app), "Start stops it");
    app.world_mut().resource_mut::<DemoLastError>().0 = None;
    tap_pad(&mut app, GamepadButton::North);
    assert_eq!(
        refused_code(&app),
        Some("demo_no_renderer"),
        "North is the screenshot"
    );
}

#[test]
fn a_benchmark_plays_every_view_and_writes_the_showcase_table() {
    let out = unique_temp_dir("showcase-demo-bench");
    let mut app = cpu_app(&out);
    let (before, _) = camera(&mut app);
    let started = call(
        &mut app,
        "demo.benchmark",
        json!({ "warmup": 2, "frames": 5, "out": "run1" }),
    );
    let views: Vec<String> = started["views"]
        .as_array()
        .expect("views")
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(views.len() >= 3);
    assert_eq!(started["tier"], "high");
    assert_eq!(
        refused(&mut app, "demo.benchmark", json!({})),
        "demo_bench_running"
    );
    // Every view takes warm-up + frames updates; a few more let the last one finish.
    let mut positions = Vec::new();
    for _ in 0..(views.len() * 7 + 3) {
        app.update();
        positions.push(camera(&mut app).0);
    }
    assert!(!app.world().resource::<DemoState>().bench.running());
    assert!(
        positions.windows(2).any(|w| w[0] != w[1]),
        "the camera followed the views"
    );
    assert_eq!(camera(&mut app).0, before, "the camera came back");

    let md = std::fs::read_to_string(out.join("benchmark/run1/showcase-table.md")).expect("markdown");
    assert!(md.contains("`demo.benchmark`"), "{md}");
    assert!(md.contains("### high (`DesktopHigh`)"));
    // A fixed 10 ms frame clock: every measured view is exactly 100 fps over 5 frames.
    for view in &views {
        assert!(
            md.contains(&format!("| {view} | 5 | 100.0 | 100.0 | 100.0 | 10.00 | 10.00 |")),
            "{view}: {md}"
        );
    }
    let record: Value = serde_json::from_str(
        &std::fs::read_to_string(out.join("benchmark/run1/showcase-table.json")).expect("json"),
    )
    .expect("json parses");
    assert_eq!(record["generator"], "demo benchmark");
    assert_eq!(record["frames"], 5);
    assert!(
        record["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .all(|r| r["status"] == "measured")
    );
    let state = call(&mut app, "demo.state", json!({}));
    assert_eq!(state["benchmark"]["runs"], 1);
    assert_eq!(state["benchmark"]["last"]["cancelled"], false);

    // A stopped run writes its table with the unfinished views `not run`.
    call(&mut app, "demo.benchmark", json!({ "warmup": 0, "frames": 100 }));
    for _ in 0..3 {
        app.update();
    }
    let stopped = call(&mut app, "demo.benchmark", json!({ "action": "cancel" }));
    assert_eq!(stopped["cancelled"], true);
    let md = std::fs::read_to_string(out.join("benchmark/showcase-table.md")).expect("markdown");
    assert!(md.contains("| cancelled |"), "{md}");
    assert_eq!(
        refused(&mut app, "demo.benchmark", json!({ "action": "cancel" })),
        "demo_bench_idle"
    );
    assert_eq!(
        refused(&mut app, "demo.benchmark", json!({ "frames": 100_000 })),
        "demo_bad_value"
    );
    assert_eq!(
        refused(
            &mut app,
            "demo.benchmark",
            json!({ "warmup": 3000, "frames": 3000 })
        ),
        "demo_views_malformed",
        "the example views are shorter than 100 s"
    );
}

#[test]
fn the_panel_widgets_drive_the_same_controls() {
    let out = unique_temp_dir("showcase-demo-panel");
    let mut app = cpu_app(&out);
    let find = |app: &mut App, want: &DemoWidget| -> Entity {
        let mut q = app.world_mut().query::<(Entity, &DemoWidget)>();
        q.iter(app.world())
            .find(|(_, w)| *w == want)
            .map(|(e, _)| e)
            .expect("widget")
    };
    let credits = find(&mut app, &DemoWidget::Credits);
    app.world_mut()
        .write_message(UiEvent::new(credits, UiEventKind::Click));
    app.update();
    assert_eq!(app.world().resource::<DemoState>().page, DemoPage::Credits);
    let mut panels = app.world_mut().query_filtered::<&NodeStyle, With<CreditsPanel>>();
    assert_eq!(
        panels.single(app.world()).expect("credits").0.display,
        Display::Flex
    );
    let mut panels = app
        .world_mut()
        .query_filtered::<&NodeStyle, With<ControlsPanel>>();
    assert_eq!(
        panels.single(app.world()).expect("controls").0.display,
        Display::None
    );

    let hour = find(&mut app, &DemoWidget::Hour);
    app.world_mut().write_message(UiEvent::new(
        hour,
        UiEventKind::ValueChanged(UiValue::Number(6.25)),
    ));
    let low = find(&mut app, &DemoWidget::Tier(QualityTier::Low));
    app.world_mut()
        .write_message(UiEvent::new(low, UiEventKind::Click));
    app.update();
    let state = app.world().resource::<DemoState>();
    assert!((state.time_of_day.hour - 6.25).abs() < 1e-6);
    assert_eq!(state.tier, QualityTier::Low);

    // An agent's change moves the slider.
    call(&mut app, "demo.set_time_of_day", json!({ "hour": 20.0 }));
    app.update();
    let mut sliders = app.world_mut().query::<(&DemoWidget, &Slider)>();
    let value = sliders
        .iter(app.world())
        .find(|(w, _)| **w == DemoWidget::Hour)
        .map(|(_, s)| s.value);
    assert_eq!(value, Some(20.0));

    // ADR-5419: the player's panel stops at sunset and at a cloudy sky; agents keep the whole range.
    let hour = find(&mut app, &DemoWidget::Hour);
    app.world_mut().write_message(UiEvent::new(
        hour,
        UiEventKind::ValueChanged(UiValue::Number(23.0)),
    ));
    app.update();
    let clock = app.world().resource::<DemoState>().time_of_day;
    let (_, sunset) =
        showcase::demo::sun::daylight_hours(clock.day_of_year, clock.latitude_deg).expect("daylight");
    assert_eq!(clock.hour, sunset, "the slider holds 23:00 at sunset");
    let mut presets = app.world_mut().query::<&DemoWidget>();
    let mut names: Vec<String> = presets
        .iter(app.world())
        .filter_map(|w| match w {
            DemoWidget::Preset(name) => Some(name.clone()),
            _ => None,
        })
        .collect();
    names.sort();
    assert_eq!(names, ["clear", "cloudy", "fair"]);
    let mut sliders = app.world_mut().query::<(&DemoWidget, &Slider)>();
    let cover = sliders
        .iter(app.world())
        .find(|(w, _)| **w == DemoWidget::Coverage)
        .map(|(_, s)| s.max);
    assert_eq!(cover, Some(showcase::demo::sun::PLAYER_MAX_COVERAGE));
}

#[test]
fn every_demo_method_answers_through_the_agent_server() {
    let project = unique_temp_dir("showcase-demo-agent");
    let out = project.join("demo-out");
    let options = Options {
        size: (160, 90),
        plain: true,
        scatter_lod: 2,
        ..Options::default()
    };
    let mut app = match showcase::demo::build_headless_demo(
        example(),
        options,
        Some(project.clone()),
        DemoOptions {
            out_dir: out.clone(),
            ..DemoOptions::default()
        },
    ) {
        Ok(app) => app,
        Err(err) => {
            eprintln!("note: no GPU adapter ({err}); the agent demo test is skipped");
            return;
        }
    };
    app.insert_resource(FrameClock::Fixed(12.5));
    let agent_dir = endpoint::agent_dir(&project);
    let script = std::thread::spawn(move || -> Vec<(String, Result<Value, String>)> {
        let deadline = Instant::now() + Duration::from_secs(900);
        let mut client = None;
        while client.is_none() && Instant::now() < deadline {
            if let Ok((advertised, _)) = endpoint::read(&agent_dir) {
                let token = std::fs::read_to_string(&advertised.token_file).unwrap_or_default();
                client = Client::connect(&advertised.url, token.trim()).ok();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut client = client.expect("agent endpoint");
        client.set_read_timeout(Some(Duration::from_secs(600))).ok();
        // Lavapipe compiles the first frames' pipelines slowly: wait for the boot warm as a filming session does,
        // and give every call the long timeout the render tests use.
        let patient = |mut params: Value| {
            params["timeout_ms"] = json!(600_000);
            params
        };
        while Instant::now() < deadline {
            let warm = client
                .call("render.pipeline_warm", patient(json!({})))
                .unwrap_or(Value::Null);
            if warm["boot_background"]["idle"].as_bool().unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut answers = Vec::new();
        let mut ask = |client: &mut Client, method: &str, params: Value| {
            let answer = client.call(method, patient(params)).map_err(|e| e.to_string());
            answers.push((method.to_owned(), answer));
        };
        ask(&mut client, "demo.set_quality", json!({ "tier": "auto" }));
        ask(&mut client, "demo.set_time_of_day", json!({ "hour": 7.0 }));
        ask(&mut client, "demo.set_weather", json!({ "preset": "fair" }));
        ask(
            &mut client,
            "demo.fly",
            json!({ "move": [0.5, 0.5], "frames": 5 }),
        );
        ask(&mut client, "demo.screenshot", json!({ "path": "agent.png" }));
        // The picture follows the clock on a tier without the physical sky. The light is written in `Update` and
        // the shot renders the extracted frame, so let a few frames pass after each change.
        ask(&mut client, "demo.set_quality", json!({ "tier": "Low" }));
        for (hour, name) in [(12.0, "low-noon.png"), (23.0, "low-night.png")] {
            ask(&mut client, "demo.set_time_of_day", json!({ "hour": hour }));
            let frame = |client: &mut Client| {
                client
                    .call("demo.state", patient(json!({})))
                    .map_or(0, |s| s["frame"].as_u64().unwrap_or(0))
            };
            let start = frame(&mut client);
            while frame(&mut client) < start + 3 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            ask(&mut client, "demo.screenshot", json!({ "path": name }));
        }
        ask(&mut client, "demo.set_time_of_day", json!({ "hour": 7.0 }));
        ask(&mut client, "quality.get_detected", json!({}));
        ask(&mut client, "demo.credits", json!({}));
        ask(&mut client, "demo.show", json!({ "page": "credits" }));
        ask(&mut client, "demo.set_quality", json!({ "tier": "Ultra9000" }));
        ask(&mut client, "demo.benchmark", json!({ "warmup": 1, "frames": 2 }));
        // Wait for the benchmark to finish (a bounded poll of the state, the protocol's way to wait).
        let mut state = Value::Null;
        while Instant::now() < deadline {
            state = client
                .call("demo.state", patient(json!({})))
                .unwrap_or(Value::Null);
            if state["benchmark"]["runs"] == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        answers.push(("demo.state".to_owned(), Ok(state)));
        let _ = client.call("quit.request", json!({}));
        answers
    });
    app.finish_plugins();
    let deadline = Instant::now() + Duration::from_secs(1200);
    while !script.is_finished() && Instant::now() < deadline {
        app.update();
    }
    let answers = script.join().expect("script");
    let get = |method: &str, index: usize| -> &Result<Value, String> {
        &answers
            .iter()
            .filter(|(m, _)| m == method)
            .nth(index)
            .expect(method)
            .1
    };

    let detected = get("demo.set_quality", 0)
        .as_ref()
        .expect("auto detects with a device");
    assert_eq!(detected["source"], "detected");
    let why = detected["why"].as_str().unwrap_or_default();
    assert!(why.starts_with("Picked ") && why.contains("because"), "{why}");
    assert_eq!(get("demo.set_quality", 1).as_ref().expect("Low")["tier"], "Low");
    let refusal = get("demo.set_quality", 2).as_ref().expect_err("unknown tier");
    assert!(refusal.contains("demo_unknown_tier"), "{refusal}");
    assert_eq!(get("demo.set_time_of_day", 0).as_ref().expect("sun")["hour"], 7.0);
    assert!(get("demo.set_weather", 0).is_ok());
    assert_eq!(get("demo.fly", 0).as_ref().expect("fly")["frames"], 5);

    let mean = |name: &str| -> f64 {
        let image = image::open(out.join("screenshots").join(name))
            .expect("a PNG was written")
            .to_rgb8();
        let ground = image.height() / 2;
        let rows: Vec<&image::Rgb<u8>> = image.pixels().skip((ground * image.width()) as usize).collect();
        rows.iter().map(|p| f64::from(p.0[1])).sum::<f64>() / rows.len().max(1) as f64
    };
    let (noon, night) = (mean("low-noon.png"), mean("low-night.png"));
    assert!(
        night < noon * 0.8,
        "on a tier without the physical sky the ground darkens at night: noon {noon}, night {night}"
    );

    let shot = get("demo.screenshot", 0).as_ref().expect("offscreen screenshot");
    assert_eq!(shot["written"], true);
    let png = out.join("screenshots/agent.png");
    let image = image::open(&png).expect("a PNG was written").to_rgba8();
    assert_eq!(image.dimensions(), (160, 90));
    assert!(
        image.pixels().any(|p| p.0 != image.get_pixel(0, 0).0),
        "the shot holds a picture, not one color"
    );

    assert_eq!(
        get("demo.credits", 0).as_ref().expect("credits")["assets"]
            .as_array()
            .map(Vec::len),
        Some(12)
    );
    assert_eq!(get("demo.show", 0).as_ref().expect("show")["page"], "credits");
    // The generic quality method answers the demo's own pick as `host` (ADR-5131).
    let generic = get("quality.get_detected", 0)
        .as_ref()
        .expect("quality.get_detected");
    assert_eq!(generic["host"]["tier"], "Low", "{generic}");
    assert_eq!(generic["host"]["source"], "picked", "{generic}");
    assert!(
        generic["host"]["detection"]["why"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Picked "),
        "{generic}"
    );
    assert_eq!(get("demo.benchmark", 0).as_ref().expect("bench")["started"], true);

    let state = get("demo.state", 0).as_ref().expect("state");
    assert_eq!(state["benchmark"]["runs"], 1, "{state}");
    assert_eq!(
        state["screenshots"]["count"], 3,
        "agent.png, low-noon.png, low-night.png"
    );
    assert_eq!(state["quality"]["tier"], "Low", "the last tier the script picked");
    assert_eq!(state["quality"]["source"], "picked");
    assert_eq!(state["quality"]["draws_sky"], false);
    assert!(
        state["quality"]["detection"]["why"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Picked "),
        "the detection's words stay available after a pick: {state}"
    );
    assert_eq!(state["fps"]["measured"], true);
    let md = std::fs::read_to_string(out.join("benchmark/showcase-table.md")).expect("table");
    assert!(
        md.contains("| 2 | 80.0 | 80.0 | 80.0 |"),
        "a 12.5 ms frame clock: {md}"
    );
    assert!(md.contains("- GPU: "), "{md}");
}

#[test]
fn the_demos_pick_is_published_for_quality_get_detected() {
    let out = unique_temp_dir("showcase-demo-host-quality");
    let mut app = cpu_app(&out);
    updates(&mut app, 2);
    let state = call(&mut app, "demo.state", json!({}));
    let host = app
        .world()
        .resource::<regia_agent::HostQualityDetection>()
        .0
        .clone()
        .expect("published in Last");
    assert_eq!(host["tier"], state["quality"]["tier"]);
    assert_eq!(host["source"], state["quality"]["source"]);
    assert_eq!(host["detection"], state["quality"]["detection"]);
    call(&mut app, "demo.set_quality", json!({ "tier": "Low" }));
    updates(&mut app, 1);
    let host = app
        .world()
        .resource::<regia_agent::HostQualityDetection>()
        .0
        .clone();
    assert_eq!(host.expect("published")["tier"], "Low", "follows a pick");
}
