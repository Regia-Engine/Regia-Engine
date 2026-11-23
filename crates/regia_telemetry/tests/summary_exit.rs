// SPDX-License-Identifier: MPL-2.0
//! The clean exit writes `summary.json` inside the sequence (before the world drops), adds the
//! step timings when it is over, and emits `quit_start`, `quit_done` and `regia::exit`.

use std::sync::{Arc, Mutex};

use regia_app::prelude::*;
use regia_platform::{ExitHooks, ExitStep, run_clean_exit};
use regia_telemetry::{SessionSummary, Telemetry, TelemetryHost, TelemetryOptions, TelemetryPlugin};
use serde_json::{Value, json};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;

#[derive(Clone, Default)]
struct Events(Arc<Mutex<Vec<(String, String)>>>);

struct Fields(String);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "data" {
            self.0 = format!("{value:?}");
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for Events {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let target = event.metadata().target();
        if target == "regia::phase" || target == "regia::exit" {
            let mut f = Fields(String::new());
            event.record(&mut f);
            self.0.lock().expect("lock").push((target.to_owned(), f.0));
        }
    }
}

#[test]
fn the_clean_exit_writes_the_final_summary_and_the_exit_events() {
    let dir = std::env::temp_dir().join(format!("regia-exit-summary-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    let path = dir.join("run.summary.json");

    let events = Events::default();
    let subscriber = tracing_subscriber::registry().with(events.clone());
    tracing::subscriber::with_default(subscriber, || {
        let mut app = App::new();
        app.add_plugins(TelemetryPlugin {
            options: TelemetryOptions {
                summary_path: Some(path.clone()),
                gpu_error_panics: false,
                ..TelemetryOptions::default()
            },
        });
        let mut host = TelemetryHost::default();
        host.set("app", "test-game");
        host.set_value("network", json!({ "role": "host" }));
        app.insert_resource(host);
        // Headless: the session header comes after 300 frames.
        for _ in 0..305 {
            app.update();
        }
        assert!(app.world().resource::<Telemetry>().summary_paths().is_some());
        // A hook of the exit sequence sees the final summary already on disk at its first step's end.
        let seen_at_save = Arc::new(Mutex::new(None::<bool>));
        let seen = seen_at_save.clone();
        let final_path = path.clone();
        // What the file said while the sequence was still running.
        let mid_exit_clean = Arc::new(Mutex::new(None::<bool>));
        let mid = mid_exit_clean.clone();
        let mid_path = path.clone();
        app.world_mut().resource_mut::<ExitHooks>().add(
            ExitStep::DropGpuResources,
            "check clean flag",
            move |_| {
                let on_disk = std::fs::read_to_string(&mid_path)
                    .ok()
                    .and_then(|t| serde_json::from_str::<SessionSummary>(&t).ok());
                *mid.lock().expect("lock") = on_disk.and_then(|s| s.exit).and_then(|e| e.clean);
            },
        );
        app.world_mut()
            .resource_mut::<ExitHooks>()
            .add(ExitStep::DropGpuResources, "check", move |world| {
                let before_drop = world.contains_resource::<Telemetry>();
                *seen.lock().expect("lock") = Some(before_drop && final_path.exists());
            });
        let report = run_clean_exit(&mut app, &[]);
        assert_eq!(report.steps.len(), ExitStep::ALL.len());
        assert_eq!(
            *seen_at_save.lock().expect("lock"),
            Some(true),
            "the final summary is written before the world drops"
        );
        assert_eq!(
            *mid_exit_clean.lock().expect("lock"),
            Some(false),
            "a teardown that never finishes must leave clean: false"
        );
    });

    let text = std::fs::read_to_string(&path).expect("summary.json exists");
    let summary: SessionSummary = serde_json::from_str(&text).expect("it parses");
    assert_eq!(summary.partial, None);
    let exit = summary.exit.expect("exit section");
    assert_eq!(exit.clean, Some(true));
    assert!(exit.ms.is_some_and(|ms| ms >= 0.0), "{exit:?}");
    let steps = exit.steps.expect("step timings");
    assert_eq!(steps.len(), ExitStep::ALL.len());
    assert_eq!(steps[0].step.as_deref(), Some("stop_background_work"));
    assert_eq!(
        summary.host.as_ref().map(|h| h["app"].clone()),
        Some(json!("test-game"))
    );
    assert_eq!(
        summary.host.as_ref().map(|h| h["network"]["role"].clone()),
        Some(json!("host"))
    );
    assert_eq!(summary.session.and_then(|s| s.frames), Some(305));

    let got = events.0.lock().expect("lock").clone();
    let phase_names: Vec<String> = got
        .iter()
        .filter(|(t, _)| t == "regia::phase")
        .filter_map(|(_, d)| serde_json::from_str::<Value>(d).ok())
        .filter_map(|d| d["phase"].as_str().map(str::to_owned))
        .collect();
    assert!(phase_names.contains(&"quit_start".to_owned()), "{phase_names:?}");
    assert!(phase_names.contains(&"quit_done".to_owned()), "{phase_names:?}");
    let exit_event = got
        .iter()
        .find(|(t, _)| t == "regia::exit")
        .expect("regia::exit event");
    let data: Value = serde_json::from_str(&exit_event.1).expect("exit data is JSON");
    assert_eq!(data["steps"].as_array().map(Vec::len), Some(ExitStep::ALL.len()));
    let _ = std::fs::remove_dir_all(&dir);
}
