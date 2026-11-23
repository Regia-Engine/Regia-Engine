// SPDX-License-Identifier: MPL-2.0
//! Spec 54 §§ 2.4, 3, 4.2 (ADR-9070): `TracePlugin`, the `--record-trace` launch argument and
//! the trace folder (the Android external files directory on Android).
//!
//! Its own test binary: the recorder is process-wide.

use std::path::{Path, PathBuf};

use regia_app::App;
use regia_telemetry::trace::{
    self, LaunchRecording, RECORD_TRACE_FLAG, RecorderState, TraceConfig, TracePlugin, default_trace_dir,
    record_trace_request,
};

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn the_launch_argument_env_and_marker_ask_for_a_recording() {
    assert_eq!(RECORD_TRACE_FLAG, "--record-trace");
    assert_eq!(record_trace_request(&args(&["game"]), None, None), None);
    assert_eq!(
        record_trace_request(&args(&["game", "--record-trace"]), None, None),
        Some(LaunchRecording { file: None })
    );
    assert_eq!(
        record_trace_request(&args(&["game", "--record-trace=boot"]), None, None),
        Some(LaunchRecording {
            file: Some("boot".into())
        })
    );
    assert_eq!(
        record_trace_request(&args(&["game", "--record-trace="]), None, None),
        Some(LaunchRecording { file: None }),
        "an empty name is the default name"
    );
    // A later positional argument is never taken as the name (`--record-trace level.ron`).
    assert_eq!(
        record_trace_request(&args(&["game", "--record-trace", "level.ron"]), None, None),
        Some(LaunchRecording { file: None })
    );
    for on in ["1", "on", "true", " yes "] {
        assert_eq!(
            record_trace_request(&args(&["game"]), Some(on), None),
            Some(LaunchRecording { file: None }),
            "{on:?}"
        );
    }
    for off in ["", "0", "off", "false", "no"] {
        assert_eq!(
            record_trace_request(&args(&["game"]), Some(off), None),
            None,
            "{off:?}"
        );
    }
    assert_eq!(
        record_trace_request(&args(&["game"]), Some("night"), None),
        Some(LaunchRecording {
            file: Some("night".into())
        })
    );
    // The flag wins over the environment, which wins over the Android marker.
    assert_eq!(
        record_trace_request(&args(&["game", "--record-trace=a"]), Some("b"), Some("c")),
        Some(LaunchRecording {
            file: Some("a".into())
        })
    );
    assert_eq!(
        record_trace_request(&args(&["game"]), Some("b"), Some("c")),
        Some(LaunchRecording {
            file: Some("b".into())
        })
    );
    assert_eq!(
        record_trace_request(&args(&["game"]), None, Some("c\n")),
        Some(LaunchRecording {
            file: Some("c".into())
        })
    );
    assert_eq!(
        record_trace_request(&args(&["game"]), None, Some("  \n")),
        Some(LaunchRecording { file: None }),
        "an empty marker records under the default name"
    );
}

#[test]
fn trace_files_go_to_profiling_or_the_android_external_files_directory() {
    assert_eq!(default_trace_dir(None), PathBuf::from("profiling"));
    let external = Path::new("/storage/emulated/0/Android/data/com.example.game/files");
    assert_eq!(default_trace_dir(Some(external)), external.join("profiling"));
    // The recorder's default folder is the desktop one off Android.
    #[cfg(not(target_os = "android"))]
    assert_eq!(TraceConfig::default().dir, PathBuf::from("profiling"));
}

#[test]
fn trace_plugin_starts_a_launch_recording_and_the_clean_exit_closes_it() {
    let dir = regia_test_utils::unique_temp_dir("trace-plugin");
    let mut app = App::new();
    app.add_plugins(TracePlugin {
        config: Some(TraceConfig {
            dir: dir.clone(),
            ..TraceConfig::default()
        }),
        args: Some(args(&["game", "--record-trace=launch"])),
    });
    app.update();
    let recorder = trace::recorder();
    assert_eq!(
        recorder.state(),
        RecorderState::Writing,
        "the launch argument started it"
    );
    assert!(trace::mark("launch_mark", None).recorded);
    // No lifecycle transitions: this app has no platform plugin (as `summary_exit.rs`).
    let report = regia_platform::run_clean_exit(&mut app, &[]);
    assert!(!report.steps.is_empty());
    assert_eq!(
        recorder.state(),
        RecorderState::Idle,
        "the clean exit stops the recording"
    );
    let read = trace::read_trace(&dir.join("launch.regiatrace")).unwrap_or_else(|e| panic!("reads: {e}"));
    assert!(
        read.events.iter().any(|e| matches!(
            &e.kind,
            trace::EventKind::Mark { name, .. } if &**name == "launch_mark"
        )),
        "the mark is in the closed file"
    );

    // Without the argument the plugin leaves the recorder idle.
    let mut quiet = App::new();
    quiet.add_plugins(TracePlugin {
        config: Some(TraceConfig {
            dir,
            ..TraceConfig::default()
        }),
        args: Some(args(&["game"])),
    });
    quiet.update();
    assert_eq!(recorder.state(), RecorderState::Idle);
}
