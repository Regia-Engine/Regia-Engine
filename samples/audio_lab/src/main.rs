// SPDX-License-Identifier: MIT-0
//! Desktop entry point of `audio_lab`.
//!
//! `audio_lab` runs the guided tour in a window (default). `--headless` runs the same tour on the
//! offline audio backend with deterministic 60 Hz steps and no window; `--exit-after-frames N`
//! caps any run's length for automated smoke tests.

use std::process::ExitCode;
use std::time::Duration;

use audio_lab::{AudioLabPlugin, ExitAfterFrames, ExitAfterTour};
use regia::prelude::*;
use regia_audio::AudioSettings;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let headless = args.iter().any(|a| a == "--headless");
    let exit_after = args
        .iter()
        .position(|a| a == "--exit-after-frames")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u64>().ok());

    if headless {
        // A minimal subscriber so the scenario log reaches the console (run_with normally does
        // this; the headless mode builds its own app). RUST_LOG overrides; default info.
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .try_init();

        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)))
            .insert_resource(AudioSettings {
                headless: true,
                ..AudioSettings::default()
            })
            .insert_resource(ExitAfterTour)
            .add_plugins(AudioLabPlugin { auto_advance: true });
        if let Some(frames) = exit_after {
            app.insert_resource(ExitAfterFrames(frames));
        }
        match app.run() {
            AppExit::Success => ExitCode::SUCCESS,
            AppExit::Error(code) => ExitCode::from(code.get()),
        }
    } else {
        let exit = regia::run_with(AudioLabPlugin { auto_advance: true }, move |app| {
            app.insert_resource(WindowSettings {
                window: Window {
                    title: "Regia Engine - audio_lab".into(),
                    ..Default::default()
                },
            });
            if let Some(frames) = exit_after {
                app.insert_resource(ExitAfterFrames(frames));
            }
        });
        match exit {
            AppExit::Success => ExitCode::SUCCESS,
            AppExit::Error(code) => ExitCode::from(code.get()),
        }
    }
}
