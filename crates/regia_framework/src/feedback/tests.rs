// SPDX-License-Identifier: MPL-2.0
use std::path::PathBuf;
use std::time::Duration;

use regia_render::{CapturedImage, RuntimeRenderer, ScreenshotRequests};
use regia_ui::interaction::{UiEvent, UiEventKind};

use super::*;
use crate::settings::GameSettings;

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("regia-feedback-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn app(dir: &std::path::Path) -> App {
    let mut app = App::new();
    // Zero deltas: these apps record no frames into the process-wide ring.
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::ZERO));
    app.add_plugins(regia_ui::UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.add_plugins(FeedbackPlugin {
        config: FeedbackConfig {
            dir: Some(dir.to_path_buf()),
            crash_bundles: false,
            ..FeedbackConfig::default()
        },
    });
    app.insert_resource(GameSettings::default())
        .insert_resource(ScreenshotRequests::default())
        .insert_resource(RuntimeRenderer::default());
    app
}

fn click(app: &mut App, target: Entity) {
    app.world_mut()
        .write_message(UiEvent::new(target, UiEventKind::Click));
}

fn button(app: &mut App, which: PromptButton) -> Entity {
    let world = app.world_mut();
    let mut q = world.query::<(Entity, &PromptButton)>();
    q.iter(world)
        .find(|(_, b)| **b == which)
        .map(|(e, _)| e)
        .expect("prompt button")
}

fn note_field(app: &mut App) -> Entity {
    let world = app.world_mut();
    let mut q = world.query_filtered::<Entity, With<PromptNote>>();
    q.iter(world).next().expect("note field")
}

/// Serves the pending window screenshot with a 4 × 2 test image.
fn serve_screenshot(app: &mut App) {
    let callbacks = app
        .world_mut()
        .resource_mut::<ScreenshotRequests>()
        .take_for(Entity::PLACEHOLDER);
    assert_eq!(callbacks.len(), 1, "one screenshot was requested");
    for cb in callbacks {
        cb(Ok(CapturedImage {
            width: 4,
            height: 2,
            rgba: (0..32).map(|i| (i * 8) as u8).collect(),
        }));
    }
}

fn wait_written(app: &mut App) -> BundleInfo {
    for _ in 0..400 {
        app.update();
        if let Some(info) = app.world().resource::<Feedback>().last_written() {
            return info.clone();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the bundle was not written");
}

fn entry(entries: &[zip::ZipEntry], name: &str) -> String {
    let e = entries
        .iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("missing {name}"));
    String::from_utf8(e.data.clone()).expect("utf8")
}

#[test]
fn prompt_writes_a_bundle_with_every_part() {
    let dir = tmp("prompt");
    let mut app = app(&dir);
    {
        let mut host = app.world_mut().resource_mut::<FeedbackHost>();
        host.set("match_id", "m-42");
        host.set("map", "mars_outpost");
        host.set("player_count", "6");
        host.set_provider(|_world| vec![("position".to_owned(), "1.0 2.0 3.0".to_owned())]);
    }
    {
        let mut ring = crash::frames().lock().expect("ring");
        for i in 0..120 {
            ring.push(FrameSample {
                t_s: 1000.0 + f64::from(i) / 60.0,
                frame_ms: if i == 100 { 48.0 } else { 16.6 },
                cpu_ms: 4.0,
                gpu_ms: 9.0,
                pipelines: u32::from(i == 100),
                ..FrameSample::default()
            });
        }
    }
    app.update();
    assert!(!app.world().resource::<Feedback>().is_prompt_open());

    // The menu entry opens the prompt; the screenshot comes first, then the panel.
    let entry_button = {
        let world = app.world_mut();
        let parent = world.spawn(regia_ui::node::UiNode).id();
        let mut commands = world.commands();
        let e = spawn_feedback_button(&mut commands, parent);
        world.flush();
        e
    };
    click(&mut app, entry_button);
    app.update();
    assert!(app.world().resource::<Feedback>().is_prompt_open());
    assert!(
        app.world().resource::<Feedback>().prompt.is_none(),
        "the panel waits for the screenshot so it is not in the picture"
    );
    serve_screenshot(&mut app);
    app.update();
    assert!(
        app.world().resource::<Feedback>().prompt.is_some(),
        "the panel is showing"
    );

    // Type a note, pick "visual" (the note survives the rebuild), send.
    let field = note_field(&mut app);
    app.world_mut().get_mut::<TextInput>(field).expect("field").text = "the hull flickers".into();
    let visual = button(&mut app, PromptButton::Category(FeedbackCategory::Visual));
    click(&mut app, visual);
    app.update();
    let field = note_field(&mut app);
    assert_eq!(
        app.world().get::<TextInput>(field).expect("field").text,
        "the hull flickers"
    );
    let send = button(&mut app, PromptButton::Send);
    click(&mut app, send);
    let info = wait_written(&mut app);
    assert!(!app.world().resource::<Feedback>().is_prompt_open());
    assert_eq!(info.category, FeedbackCategory::Visual);
    assert!(info.path.starts_with(&dir));
    assert!(info.name.ends_with("-visual.zip"), "{}", info.name);

    let entries = read_bundle(&info.path).expect("readable");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    for part in [
        "feedback.json",
        "note.txt",
        "screenshot.png",
        "frame_times.csv",
        "hitch_report.txt",
        "system.txt",
        "settings.ron",
        "render_problems.txt",
        "log_tail.txt",
        "build_info.json",
        "host.json",
    ] {
        assert!(names.contains(&part), "missing {part} in {names:?}");
    }
    assert!(!names.contains(&"crash.txt"));
    assert_eq!(entry(&entries, "note.txt"), "the hull flickers");
    let meta: serde_json::Value = serde_json::from_str(&entry(&entries, "feedback.json")).expect("json");
    assert_eq!(meta["category"], "visual");
    assert_eq!(meta["note"], "the hull flickers");
    assert_eq!(meta["map"], "mars_outpost");
    assert_eq!(meta["position"], "1.0 2.0 3.0");
    assert_eq!(meta["screenshot"], "screenshot.png");
    assert_eq!(meta["frames"], 120);
    let host: serde_json::Value = serde_json::from_str(&entry(&entries, "host.json")).expect("json");
    assert_eq!(host["match_id"], "m-42");
    assert_eq!(host["player_count"], "6");
    let png = entries.iter().find(|e| e.name == "screenshot.png").expect("png");
    let decoder = png::Decoder::new(std::io::Cursor::new(png.data.clone()));
    let reader = decoder.read_info().expect("png decodes");
    assert_eq!((reader.info().width, reader.info().height), (4, 2));
    let csv = entry(&entries, "frame_times.csv");
    assert_eq!(csv.lines().count(), 121, "header + 120 frames");
    assert!(csv.contains(",48.000,"));
    let hitch = entry(&entries, "hitch_report.txt");
    assert!(hitch.contains("120 frames"), "{hitch}");
    assert!(hitch.contains("1 pipelines"), "{hitch}");
    assert!(entry(&entries, "settings.ron").contains("preset"));
    assert!(entry(&entries, "render_problems.txt").contains("ok: no problems"));
    assert!(entry(&entries, "system.txt").contains("cpu:"));
    assert!(entry(&entries, "build_info.json").contains("\"git\""));

    // The message went out, and the folder lists the bundle.
    let written: Vec<FeedbackWritten> = app
        .world_mut()
        .resource_mut::<Messages<FeedbackWritten>>()
        .drain()
        .collect();
    assert!(written.iter().any(|w| w.from_prompt && w.info == info));
    let listed = pending_bundles(app.world());
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].category, Some(FeedbackCategory::Visual));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn escape_cancels_and_nothing_is_written() {
    let dir = tmp("cancel");
    let mut app = app(&dir);
    open_prompt(app.world_mut());
    app.update();
    serve_screenshot(&mut app);
    app.update();
    assert!(app.world().resource::<Feedback>().prompt.is_some());
    app.world_mut().write_message(KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: "Escape".into(),
        logical_key: "Escape".into(),
        state: ButtonState::Pressed,
        repeat: false,
        timestamp: Default::default(),
    });
    app.update();
    assert!(!app.world().resource::<Feedback>().is_prompt_open());
    for _ in 0..5 {
        app.update();
    }
    assert!(list_bundles(&dir).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nothing_is_bound_by_default() {
    let config = FeedbackConfig::default();
    assert_eq!(config.hotkey, None, "no F-key by default (DO27 uses them)");
    assert_eq!(config.action, FEEDBACK_ACTION);
    let keyboard = Keyboard::default();
    let actions = Actions::default();
    assert!(!hotkey_pressed(&config, Some(&keyboard), Some(&actions)));
    assert_eq!(FEEDBACK_MENU_ITEM.label, "Send feedback");
}

#[test]
fn direct_capture_without_renderer_or_ui() {
    let dir = tmp("direct");
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::ZERO));
    app.add_plugins(FeedbackPlugin {
        config: FeedbackConfig {
            dir: Some(dir.clone()),
            crash_bundles: false,
            ..FeedbackConfig::default()
        },
    });
    let (tx, rx) = std::sync::mpsc::channel();
    app.world_mut().resource_mut::<Feedback>().capture(
        CaptureOptions {
            category: FeedbackCategory::Performance,
            note: "stutter at the gate".into(),
            screenshot: true,
        },
        Some(Box::new(move |r| {
            let _ = tx.send(r);
        })),
    );
    let info = wait_written(&mut app);
    let from_callback = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("callback")
        .expect("ok");
    assert_eq!(from_callback, info);
    let entries = read_bundle(&info.path).expect("readable");
    let meta: serde_json::Value = serde_json::from_str(&entry(&entries, "feedback.json")).expect("json");
    assert_eq!(meta["category"], "performance");
    assert_eq!(meta["screenshot_missing"], "no renderer in this app");
    assert!(entry(&entries, "render_problems.txt").contains("no runtime renderer"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A main-loop stall is sampled at its real length, not gameplay's 250 ms clamp (lane
/// v018-stalls: DO27 testers' 4.7 s and 8.8 s frames were reported as 250 ms).
#[test]
fn the_sampler_reads_a_stall_at_its_real_length() {
    let start = std::time::Instant::now();
    let mut real = RealTime::default();
    real.tick(start);
    real.tick(start + Duration::from_millis(16));
    let mut sampler = FrameSampler::default();
    let first = sampler.sample(Some(&real), None).expect("a 16 ms frame");
    assert!((first.frame_ms - 16.0).abs() < 0.01, "{}", first.frame_ms);
    real.tick(start + Duration::from_millis(16 + 4670));
    let stall = sampler.sample(Some(&real), None).expect("the stalled frame");
    assert!((stall.frame_ms - 4670.0).abs() < 0.01, "{}", stall.frame_ms);
}

/// Quitting waits for running writer threads: after the join their work is complete and the
/// handle list is empty.
#[test]
fn exit_joins_running_writers() {
    use std::sync::atomic::{AtomicU32, Ordering};

    let done = Arc::new(AtomicU32::new(0));
    let mut feedback = Feedback::default();
    for _ in 0..4 {
        let done = done.clone();
        feedback.writers.push(std::thread::spawn(move || {
            for _ in 0..1000 {
                done.fetch_add(1, Ordering::SeqCst);
                std::thread::yield_now();
            }
        }));
    }
    join_writers(&mut feedback);
    assert_eq!(done.load(Ordering::SeqCst), 4000);
    assert!(feedback.writers.is_empty());
}
