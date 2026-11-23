// SPDX-License-Identifier: MIT-0
//! `vfx_lab --deck-check` end to end without a window: the Deck profile, the unit field, the
//! scripted camera, frame statistics, sensor sampling from a fake sysfs and the report file.
//! The scripted screenshots need a window and are covered on the device.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use regia::platform::{KeyboardInput, MouseButtonInput, MouseMotion, MouseWheel};
use regia::prelude::*;
use regia_render::{ExtractedFrame, RenderDevice};
use vfx_lab::deck_check::{DeckCheck, DeckCheckPlugin};
use vfx_lab::window::WindowedVfxLab;

#[test]
fn a_short_run_writes_the_report() {
    let dir = std::env::temp_dir().join(format!("deck-check-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sys = dir.join("sys");
    std::fs::create_dir_all(sys.join("class/hwmon/hwmon0")).unwrap();
    std::fs::write(sys.join("class/hwmon/hwmon0/name"), "k10temp\n").unwrap();
    std::fs::write(sys.join("class/hwmon/hwmon0/temp1_input"), "68000\n").unwrap();

    let mut cfg = DeckCheck::new(Duration::from_millis(4000), dir.join("out"));
    cfg.sysfs = sys;
    let mut app = App::new();
    app.add_plugins((
        WindowedVfxLab {
            exit_after_frames: 0,
            screenshot: None,
        },
        DeckCheckPlugin(cfg),
    ));
    app.add_message::<MouseMotion>()
        .add_message::<MouseButtonInput>()
        .add_message::<MouseWheel>()
        .add_message::<KeyboardInput>();
    app.insert_resource(RenderDevice::headless().expect("headless GPU (lavapipe)"))
        .init_resource::<ExtractedFrame>()
        .init_resource::<regia_render::RuntimeRenderer>()
        .init_resource::<regia_render::ScreenshotRequests>()
        .add_systems(Render, regia_render::extract_frame);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(25)));

    let started = Instant::now();
    let mut exited = false;
    while !exited && started.elapsed() < Duration::from_secs(60) {
        app.update();
        // `should_exit` consumes the message: ask once per frame.
        exited = app.should_exit().is_some();
    }
    assert!(exited, "the run ends by itself after its duration");

    let text = std::fs::read_to_string(dir.join("out/deck_check.json")).expect("report written");
    let report: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(report["profile"], "Deck");
    assert!(report["frames"]["frames"].as_u64().unwrap() > 3, "{text}");
    assert!(report["frames"]["p50_ms"].as_f64().unwrap() > 0.0);
    assert_eq!(report["cap_fps"], 40.0);
    assert!(
        report["hidden_frames"].is_u64(),
        "the report carries the hidden-frame count"
    );
    let samples = report["sensors"]["samples"].as_array().unwrap();
    assert!(!samples.is_empty());
    assert_eq!(samples[0]["temps_c"]["k10temp/temp1"], 68.0);
    assert_eq!(report["sensors"]["peak_temp_c"], 68.0);
    // The deck profile reached the renderer settings.
    let settings = app.world().resource::<regia_render::RenderSettings>();
    assert!(
        (settings.profile.render_scale - 0.7).abs() < 1e-4,
        "Deck renders at 70 %"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
