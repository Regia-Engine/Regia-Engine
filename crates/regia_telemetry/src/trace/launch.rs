// SPDX-License-Identifier: MPL-2.0
//! [`TracePlugin`] (spec 54 § 3, ADR-9070): applies a [`TraceConfig`] to the process recorder,
//! puts trace files in the Android app's external files directory (so `adb pull` reaches them),
//! starts a recording at launch when asked ([`RECORD_TRACE_FLAG`], [`RECORD_TRACE_ENV`] or, on
//! Android, the [`ANDROID_RECORD_TRACE_MARKER`] file) and closes a running recording in the
//! clean exit.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;

use super::{RecorderState, TraceConfig, recorder};

/// Launch argument that starts a recording at boot: `--record-trace` (the default
/// `trace-<unix_ms>` name) or `--record-trace=<bare name>`. A following argument is never taken
/// as the name, so `game --record-trace level.ron` keeps `level.ron` for the game.
pub const RECORD_TRACE_FLAG: &str = "--record-trace";

/// Environment variable that starts a recording at boot when no [`RECORD_TRACE_FLAG`] is given:
/// `1`, `on`, `true` or `yes` record under the default name, `0`, `off`, `false`, `no` or an empty
/// value do nothing, any other value is the file name.
pub const RECORD_TRACE_ENV: &str = "REGIA_RECORD_TRACE";

/// On Android (no launch arguments or environment): a file of this name in the app's internal
/// files directory starts a recording at boot (`adb shell run-as <package> touch
/// files/record-trace`); its trimmed content, when not empty, is the file name.
pub const ANDROID_RECORD_TRACE_MARKER: &str = "record-trace";

/// The folder trace files go to under the platform's base: `profiling` (relative to the working
/// directory, the project's log directory in a dev run) or, given the Android app's external
/// files directory, `<external>/profiling`.
#[must_use]
pub fn default_trace_dir(android_external: Option<&Path>) -> PathBuf {
    match android_external {
        Some(external) => external.join("profiling"),
        None => PathBuf::from("profiling"),
    }
}

/// A recording asked for at launch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchRecording {
    /// The bare file name (`None`: `trace-<unix_ms>`).
    pub file: Option<String>,
}

fn named(value: &str) -> LaunchRecording {
    let value = value.trim();
    LaunchRecording {
        file: (!value.is_empty()).then(|| value.to_owned()),
    }
}

/// Whether the launch asks for a recording. [`RECORD_TRACE_FLAG`] in `args` wins over `env` (the
/// value of [`RECORD_TRACE_ENV`]), which wins over `android_marker` (the content of the
/// [`ANDROID_RECORD_TRACE_MARKER`] file, when it exists).
#[must_use]
pub fn record_trace_request(
    args: &[String],
    env: Option<&str>,
    android_marker: Option<&str>,
) -> Option<LaunchRecording> {
    for arg in args {
        if arg == RECORD_TRACE_FLAG {
            return Some(LaunchRecording::default());
        }
        if let Some(rest) = arg
            .strip_prefix(RECORD_TRACE_FLAG)
            .and_then(|rest| rest.strip_prefix('='))
        {
            return Some(named(rest));
        }
    }
    if let Some(value) = env {
        let value = value.trim();
        return match value.to_ascii_lowercase().as_str() {
            "" | "0" | "off" | "false" | "no" => None,
            "1" | "on" | "true" | "yes" => Some(LaunchRecording::default()),
            _ => Some(named(value)),
        };
    }
    android_marker.map(named)
}

/// Applies the trace settings, puts trace files where the platform wants them and starts a
/// launch recording (spec 54 § 3). Part of `regia::DefaultPlugins` (client builds).
///
/// The recorder stays `Idle` unless [`Self::config`] asks for a window or the launch asks for a
/// recording ([`record_trace_request`]); a recording still running at the clean exit is stopped
/// in [`regia_platform::ExitStep::StopBackgroundWork`], so its file ends with its index chunk.
/// A host that wants other settings adds its own `TracePlugin` before `DefaultPlugins` (the
/// second one is skipped) or calls [`super::configure`].
#[derive(Clone, Debug, Default)]
pub struct TracePlugin {
    /// Settings applied at startup (`None`: the recorder's own, with the Android folder on
    /// Android).
    pub config: Option<TraceConfig>,
    /// The launch arguments to read (`None`: the process's, plus [`RECORD_TRACE_ENV`] and the
    /// Android marker). Given arguments are the only source, so tests see no environment.
    pub args: Option<Vec<String>>,
}

/// What the startup system of [`TracePlugin`] applies.
#[derive(Resource, Clone)]
struct TraceLaunch(TracePlugin);

impl Plugin for TracePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(TraceLaunch(self.clone()))
            .add_systems(Startup, start_launch_recording);
        let mut hooks = app
            .world_mut()
            .get_resource_or_init::<regia_platform::ExitHooks>();
        hooks.add(
            regia_platform::ExitStep::StopBackgroundWork,
            "trace recorder",
            |_world| {
                let r = recorder();
                if r.state() == RecorderState::Writing
                    && let Err(err) = r.stop()
                {
                    tracing::warn!(target: "regia_telemetry", "trace recording not closed at exit: {err}");
                }
            },
        );
    }
}

/// `Startup`: the folder, then the launch recording.
fn start_launch_recording(world: &mut World) {
    let Some(TraceLaunch(plugin)) = world.remove_resource::<TraceLaunch>() else {
        return;
    };
    let (external, internal) = android_dirs(world);
    let r = recorder();
    match plugin.config {
        Some(config) => r.configure(config),
        None => {
            if let Some(external) = external.as_deref() {
                let mut config = r.config();
                config.dir = default_trace_dir(Some(external));
                r.configure(config);
            }
        }
    }
    let request = match plugin.args {
        Some(args) => record_trace_request(&args, None, None),
        None => {
            let args: Vec<String> = std::env::args_os()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            let env = regia_core::brand::env_var(RECORD_TRACE_ENV).ok();
            let marker = internal
                .map(|dir| dir.join(ANDROID_RECORD_TRACE_MARKER))
                .and_then(|path| std::fs::read_to_string(path).ok());
            record_trace_request(&args, env.as_deref(), marker.as_deref())
        }
    };
    let Some(request) = request else {
        return;
    };
    if r.state() == RecorderState::Writing {
        return;
    }
    match r.start(request.file.as_deref(), None) {
        Ok(summary) => tracing::info!(
            target: "regia_telemetry",
            file = %summary.file.as_deref().unwrap_or_else(|| Path::new("")).display(),
            "trace recording started at launch"
        ),
        Err(err) => tracing::warn!(target: "regia_telemetry", "launch trace recording not started: {err}"),
    }
}

/// The Android app's external and internal files directories (both `None` elsewhere).
#[cfg(target_os = "android")]
fn android_dirs(world: &World) -> (Option<PathBuf>, Option<PathBuf>) {
    world
        .get_resource::<regia_platform::AndroidAppHandle>()
        .map_or((None, None), |handle| {
            (handle.0.external_data_path(), handle.0.internal_data_path())
        })
}

/// The Android app's external and internal files directories (both `None` elsewhere).
#[cfg(not(target_os = "android"))]
fn android_dirs(_world: &World) -> (Option<PathBuf>, Option<PathBuf>) {
    (None, None)
}
