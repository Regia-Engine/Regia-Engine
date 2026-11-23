// SPDX-License-Identifier: MPL-2.0
//! Startup playback of a `.frec` input recording (ADR-3968).
//!
//! A packaged smoke run sets [`PLAY_INPUT_ENV`] or passes [`PLAY_INPUT_FLAG`] with a path. On
//! Android an activity does not inherit either, so a debuggable smoke pushes a marker file into
//! the app's internal files directory and the plugin reads the recording beside it. A missing
//! file logs a warning and the game continues. The same path works in dev and shipping builds.
//!
//! [`SMOKE_SECONDS_ENV`] (or [`ANDROID_SMOKE_SECONDS_FILE`] on Android) asks the process to exit
//! cleanly after that many seconds following startup playback, so shader warmup cannot consume the run.
//!
//! Re-record the committed smoke files with `scripts/write-package-smoke-recordings.sh` (that
//! script runs `cargo xtask package-smoke --write-recordings`). The recordings are short and
//! deterministic: playback ends and the game keeps running until the smoke limit.

use std::path::{Path, PathBuf};
use std::time::Instant;

use regia_app::prelude::*;
use regia_core::math::Vec3;

use crate::demo::{DemoHeader, DemoRecording};
use crate::input_recording::{InputRecorder, InputRecording, load_frec, save_frec};

/// Environment variable naming a `.frec` file to play at startup. Overridden by [`PLAY_INPUT_FLAG`].
pub const PLAY_INPUT_ENV: &str = "REGIA_PLAY_INPUT";

/// Command-line flag naming a `.frec` file to play at startup (`--play-input PATH` or
/// `--play-input=PATH`). It wins over [`PLAY_INPUT_ENV`].
pub const PLAY_INPUT_FLAG: &str = "--play-input";

/// Environment variable: exit cleanly this many seconds after startup playback completes. Empty or `0` does not
/// exit. A non-integer is ignored, with a warning.
pub const SMOKE_SECONDS_ENV: &str = "REGIA_SMOKE_SECONDS";

/// Marker file in the Android app's internal files directory. Playback is considered only when
/// this file exists, so a normal launch does not warn about a missing recording.
pub const ANDROID_PLAY_MARKER: &str = "regia-play-input.on";

/// Recording copied next to [`ANDROID_PLAY_MARKER`].
pub const ANDROID_PLAY_FILE: &str = "regia-play-input.frec";

/// Android stand-in for [`SMOKE_SECONDS_ENV`]: a text file holding the integer, read when the
/// environment variable is unset.
pub const ANDROID_SMOKE_SECONDS_FILE: &str = "regia-smoke-seconds";

/// Info message written when a recording starts playing. The package smoke looks for it.
pub const PLAYING_MESSAGE: &str = "playing input recording";

/// Info message written once the first update and the requested playback have completed.
/// Smoke tests time their run from this point, excluding startup shader compilation.
pub const TIMING_STARTED_MESSAGE: &str = "input playback complete; starting smoke timer";

/// Warning written when the requested recording file is not there.
pub const MISSING_MESSAGE: &str = "input recording missing; playback skipped";

/// Warning prefix written when the file is not a usable recording.
pub const REJECTED_MESSAGE: &str = "input recording rejected; playback skipped";

/// One committed smoke recording: a path relative to the workspace root, the level name stored in
/// the header, and the action columns.
pub struct SmokeRecording {
    /// Path relative to the workspace root.
    pub relative: &'static str,
    /// Level name stored in the demo header.
    pub level: &'static str,
    /// Action columns, in recording order.
    pub actions: &'static [&'static str],
}

/// The recordings `cargo xtask package-smoke --write-recordings` writes.
pub const SMOKE_RECORDINGS: &[SmokeRecording] = &[
    SmokeRecording {
        relative: "samples/sim_bridge/smoke/playback.frec",
        level: "sim_bridge",
        actions: &["Move", "Look"],
    },
    SmokeRecording {
        relative: "crates/regia_templates/templates/third_person/smoke/playback.frec",
        level: "third_person",
        actions: &["Move", "Look", "Jump", "QuickSave", "QuickLoad", "Pause"],
    },
];

/// How many fixed steps a smoke recording contains.
const SMOKE_STEPS: usize = 32;

/// Where startup playback should read, if anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackRequest {
    /// Play this `.frec` path.
    Path(PathBuf),
    /// [`PLAY_INPUT_FLAG`] was present without a path.
    FlagMissingValue,
    /// Nothing asked for playback.
    None,
}

/// The recording loaded for playback, or why it was not.
#[derive(Debug)]
pub enum PlaybackLoad {
    /// A non-empty recording.
    Play(InputRecording),
    /// The path is not a file.
    Missing,
    /// The file exists but is empty or not a valid `.frec`.
    Rejected(String),
}

/// Wall-clock limit installed when a smoke run asked the process to exit.
#[derive(Resource)]
struct SmokeClock {
    started: Option<Instant>,
    saw_update: bool,
    required_steps: usize,
    limit_secs: u64,
}

impl SmokeClock {
    fn advance(&mut self, completed_steps: usize, now: Instant) -> bool {
        if !self.saw_update {
            self.saw_update = true;
            return false;
        }
        let Some(started) = self.started else {
            if completed_steps >= self.required_steps {
                self.started = Some(now);
            }
            return false;
        };
        smoke_should_exit(
            now.saturating_duration_since(started).as_secs(),
            Some(self.limit_secs),
        )
    }
}

/// Plays [`PLAY_INPUT_FLAG`] / [`PLAY_INPUT_ENV`] (or the Android marker) once at startup, and
/// exits the app when a smoke-seconds limit elapses.
pub struct StartupPlaybackPlugin;

impl Plugin for StartupPlaybackPlugin {
    fn build(&self, app: &mut App) {
        if app.world().get_resource::<InputRecorder>().is_none() {
            app.add_plugins(crate::input_recording::InputRecordingPlugin);
        }
        app.add_systems(Startup, install_startup_playback)
            .add_systems(Update, smoke_exit_system);
    }
}

fn install_startup_playback(world: &mut World) {
    let args = process_args();
    let env_path = std::env::var(PLAY_INPUT_ENV).ok();
    let marker = android_files_dir(world).and_then(|dir| android_playback_file(&dir));
    let mut required_steps = 0;
    match resolve_playback_path(&args, env_path.as_deref(), marker.as_deref()) {
        PlaybackRequest::None => {}
        PlaybackRequest::FlagMissingValue => {
            tracing::warn!(
                target: "regia_framework",
                "{PLAY_INPUT_FLAG} needs a path; playback skipped"
            );
        }
        PlaybackRequest::Path(path) => match load_playback(&path) {
            PlaybackLoad::Play(recording) => {
                if let Some(mut recorder) = world.get_resource_mut::<InputRecorder>() {
                    required_steps = recording.len();
                    recorder.play(recording);
                    tracing::info!(target: "regia_framework", "{PLAYING_MESSAGE}");
                } else {
                    tracing::warn!(
                        target: "regia_framework",
                        "input recorder is not installed; playback skipped"
                    );
                }
            }
            PlaybackLoad::Missing => {
                tracing::warn!(target: "regia_framework", "{MISSING_MESSAGE}");
            }
            PlaybackLoad::Rejected(reason) => {
                tracing::warn!(target: "regia_framework", "{REJECTED_MESSAGE}: {reason}");
            }
        },
    }

    let file_text = android_files_dir(world)
        .and_then(|dir| read_smoke_seconds_file(&dir.join(ANDROID_SMOKE_SECONDS_FILE)));
    let env_text = std::env::var(SMOKE_SECONDS_ENV).ok();
    if let Some(limit) = smoke_limit_from_sources(env_text.as_deref(), file_text.as_deref()) {
        world.insert_resource(SmokeClock {
            started: None,
            saw_update: false,
            required_steps,
            limit_secs: limit,
        });
    }
}

fn smoke_exit_system(
    clock: Option<ResMut<SmokeClock>>,
    recorder: Option<Res<InputRecorder>>,
    mut wrote: Local<bool>,
    mut exit: MessageWriter<AppExit>,
) {
    let Some(mut clock) = clock else {
        return;
    };
    if *wrote {
        return;
    }
    let completed_steps = recorder.as_ref().map_or(0, |recorder| recorder.cursor);
    let was_started = clock.started.is_some();
    let should_exit = clock.advance(completed_steps, Instant::now());
    if !was_started && clock.started.is_some() {
        tracing::info!(target: "regia_framework", "{TIMING_STARTED_MESSAGE}");
    }
    if should_exit {
        *wrote = true;
        tracing::info!(target: "regia_framework", "smoke seconds elapsed; exiting");
        exit.write(AppExit::Success);
    }
}

fn process_args() -> Vec<String> {
    std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

/// The `.frec` path to play when `files_dir` contains [`ANDROID_PLAY_MARKER`].
///
/// The marker gates the lookup: a launch without it returns `None` and does not warn.
#[must_use]
pub fn android_playback_file(files_dir: &Path) -> Option<PathBuf> {
    let marker = files_dir.join(ANDROID_PLAY_MARKER);
    if marker.is_file() {
        Some(files_dir.join(ANDROID_PLAY_FILE))
    } else {
        None
    }
}

fn read_smoke_seconds_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

#[cfg(target_os = "android")]
fn android_files_dir(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<regia_platform::AndroidAppHandle>()
        .and_then(|handle| handle.0.internal_data_path())
}

#[cfg(not(target_os = "android"))]
fn android_files_dir(_world: &World) -> Option<PathBuf> {
    None
}

/// Resolves the playback path. The flag wins over `env_path`, which wins over `marker`.
///
/// `marker` is the recording path already chosen because the Android marker file exists, not the
/// marker file itself. An empty or all-whitespace `env_path` is treated as unset.
#[must_use]
pub fn resolve_playback_path(
    args: &[String],
    env_path: Option<&str>,
    marker: Option<&Path>,
) -> PlaybackRequest {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some(rest) = arg.strip_prefix("--play-input=") {
            if rest.is_empty() {
                return PlaybackRequest::FlagMissingValue;
            }
            return PlaybackRequest::Path(PathBuf::from(rest));
        }
        if arg == PLAY_INPUT_FLAG {
            return match args.get(index + 1) {
                Some(next) if !next.starts_with('-') => PlaybackRequest::Path(PathBuf::from(next)),
                _ => PlaybackRequest::FlagMissingValue,
            };
        }
        index += 1;
    }
    if let Some(env_path) = env_path {
        let env_path = env_path.trim();
        if !env_path.is_empty() {
            return PlaybackRequest::Path(PathBuf::from(env_path));
        }
    }
    if let Some(marker) = marker {
        return PlaybackRequest::Path(marker.to_path_buf());
    }
    PlaybackRequest::None
}

/// Loads `path` for playback. A missing file is [`PlaybackLoad::Missing`]. An empty recording is
/// [`PlaybackLoad::Rejected`], not a silent no-op.
#[must_use]
pub fn load_playback(path: &Path) -> PlaybackLoad {
    if !path.is_file() {
        return PlaybackLoad::Missing;
    }
    match load_frec(path) {
        Ok(demo) => {
            let recording = demo.to_input_recording();
            if recording.is_empty() {
                PlaybackLoad::Rejected("recording has no steps".to_string())
            } else {
                PlaybackLoad::Play(recording)
            }
        }
        Err(err) => PlaybackLoad::Rejected(err.to_string()),
    }
}

/// Whether a smoke run should exit. `limit` of `None` never exits. Comparison only: callers pass
/// the elapsed seconds they already measured.
#[must_use]
pub fn smoke_should_exit(elapsed_secs: u64, limit: Option<u64>) -> bool {
    limit.is_some_and(|limit| elapsed_secs >= limit)
}

/// A smoke-seconds value was not an integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmokeSecondsError;

/// Parses a smoke-seconds value. Empty or `0` is "do not exit". Anything else that is not an
/// integer is an error.
///
/// # Errors
/// [`SmokeSecondsError`] when `text` is not empty and not an integer.
pub fn parse_smoke_seconds(text: &str) -> Result<Option<u64>, SmokeSecondsError> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    match text.parse::<u64>() {
        Ok(0) => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(_) => Err(SmokeSecondsError),
    }
}

/// The smoke limit from the environment text and, when that is unset or invalid, a file's text.
///
/// An explicit `0` or empty environment value means "do not exit" and does not fall through.
/// Invalid environment text warns and falls through to `file_text`.
#[must_use]
pub fn smoke_limit_from_sources(env_text: Option<&str>, file_text: Option<&str>) -> Option<u64> {
    if let Some(text) = env_text {
        match parse_smoke_seconds(text) {
            Ok(limit) => return limit,
            Err(SmokeSecondsError) => {
                tracing::warn!(
                    target: "regia_framework",
                    "{SMOKE_SECONDS_ENV} ignored: not an integer"
                );
            }
        }
    }
    if let Some(text) = file_text {
        match parse_smoke_seconds(text) {
            Ok(limit) => return limit,
            Err(SmokeSecondsError) => {
                tracing::warn!(
                    target: "regia_framework",
                    "{ANDROID_SMOKE_SECONDS_FILE} ignored: not an integer"
                );
            }
        }
    }
    None
}

/// A short deterministic demo for `level` with one column per `actions` entry.
///
/// Thirty-two steps, seed 1, 60 Hz, no keyframes and no timestamps. Playback of it ends quickly;
/// the smoke run's length is the smoke-seconds limit, not the recording.
#[must_use]
pub fn smoke_demo(level: &str, actions: &[&str]) -> DemoRecording {
    let width = actions.len();
    let mut frames = Vec::with_capacity(SMOKE_STEPS);
    for step in 0..SMOKE_STEPS {
        let mut row = vec![Vec3::ZERO; width];
        if let Some(move_axis) = row.first_mut() {
            let forward = if step.is_multiple_of(2) { 1.0 } else { 0.0 };
            *move_axis = Vec3::new(0.0, forward, 0.0);
        }
        if let Some(look_axis) = row.get_mut(1) {
            *look_axis = Vec3::new(0.0, step as f32 * 0.01, 0.0);
        }
        frames.push(row);
    }
    let recording = InputRecording {
        seed: 1,
        actions: actions.iter().map(|name| (*name).to_string()).collect(),
        frames,
        timestamps: Vec::new(),
    };
    let header = DemoHeader::new(level, 1, 60, 0);
    DemoRecording::from_input_recording(header, &recording)
}

/// Writes [`SMOKE_RECORDINGS`] under `root` (the workspace root).
///
/// # Errors
/// A filesystem or encode error, with the path it was writing.
pub fn write_smoke_recordings(root: &Path) -> Result<(), String> {
    for spec in SMOKE_RECORDINGS {
        let demo = smoke_demo(spec.level, spec.actions);
        let path = root.join(spec.relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| format!("creating {}: {err}", parent.display()))?;
        }
        save_frec(&demo, &path).map_err(|err| format!("writing {}: {err}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_wins_over_env_and_marker() {
        let args = vec![
            "game".to_string(),
            "--play-input".to_string(),
            "flag.frec".to_string(),
        ];
        let request = resolve_playback_path(&args, Some("env.frec"), Some(Path::new("marker.frec")));
        assert_eq!(request, PlaybackRequest::Path(PathBuf::from("flag.frec")));
    }

    #[test]
    fn equals_form_and_missing_flag_value() {
        let args = vec!["--play-input=/tmp/a.frec".to_string()];
        assert_eq!(
            resolve_playback_path(&args, None, None),
            PlaybackRequest::Path(PathBuf::from("/tmp/a.frec"))
        );
        let bare = vec!["--play-input".to_string()];
        assert_eq!(
            resolve_playback_path(&bare, Some("env.frec"), None),
            PlaybackRequest::FlagMissingValue
        );
        let empty = vec!["--play-input=".to_string()];
        assert_eq!(
            resolve_playback_path(&empty, Some("env.frec"), None),
            PlaybackRequest::FlagMissingValue
        );
    }

    #[test]
    fn env_wins_over_marker_and_blank_env_falls_through() {
        assert_eq!(
            resolve_playback_path(&[], Some("env.frec"), Some(Path::new("marker.frec"))),
            PlaybackRequest::Path(PathBuf::from("env.frec"))
        );
        assert_eq!(
            resolve_playback_path(&[], Some("  "), Some(Path::new("marker.frec"))),
            PlaybackRequest::Path(PathBuf::from("marker.frec"))
        );
        assert_eq!(resolve_playback_path(&[], None, None), PlaybackRequest::None);
        assert_eq!(resolve_playback_path(&[], Some(""), None), PlaybackRequest::None);
    }

    #[test]
    fn shader_warmup_and_incomplete_playback_do_not_consume_the_run_budget() {
        let base = Instant::now();
        let at = |seconds| base + std::time::Duration::from_secs(seconds);
        let mut clock = SmokeClock {
            started: None,
            saw_update: false,
            required_steps: 32,
            limit_secs: 60,
        };
        assert!(!clock.advance(0, at(0)));
        // A slow first frame took longer than the entire requested run, but playback is incomplete.
        assert!(!clock.advance(2, at(120)));
        assert!(clock.started.is_none());
        assert!(!clock.advance(32, at(121)));
        assert_eq!(clock.started, Some(at(121)));
        assert!(!clock.advance(32, at(180)));
        assert!(clock.advance(32, at(181)));

        let mut no_recording = SmokeClock {
            started: None,
            saw_update: false,
            required_steps: 0,
            limit_secs: 60,
        };
        assert!(!no_recording.advance(0, at(0)));
        assert!(no_recording.started.is_none());
        assert!(!no_recording.advance(0, at(120)));
        assert!(!no_recording.advance(0, at(179)));
        assert!(no_recording.advance(0, at(180)));
    }

    #[test]
    fn smoke_limit_has_no_wall_clock() {
        assert!(!smoke_should_exit(59, Some(60)));
        assert!(smoke_should_exit(60, Some(60)));
        assert!(smoke_should_exit(61, Some(60)));
        assert!(!smoke_should_exit(10_000, None));
        assert_eq!(parse_smoke_seconds("").expect("empty"), None);
        assert_eq!(parse_smoke_seconds("0").expect("zero"), None);
        assert_eq!(parse_smoke_seconds(" 60 ").expect("sixty"), Some(60));
        assert!(parse_smoke_seconds("nope").is_err());
        assert_eq!(smoke_limit_from_sources(Some("60"), Some("10")), Some(60));
        assert_eq!(smoke_limit_from_sources(None, Some("15")), Some(15));
        assert_eq!(smoke_limit_from_sources(Some("0"), Some("15")), None);
        assert_eq!(smoke_limit_from_sources(Some("nope"), Some("15")), Some(15));
        assert_eq!(smoke_limit_from_sources(None, None), None);
    }

    #[test]
    fn marker_file_gates_the_android_recording_path() {
        let dir = regia_test_utils::unique_temp_dir("startup-playback-marker");
        assert!(android_playback_file(&dir).is_none());
        std::fs::write(dir.join(ANDROID_PLAY_MARKER), b"").expect("marker");
        assert_eq!(
            android_playback_file(&dir).as_deref(),
            Some(dir.join(ANDROID_PLAY_FILE).as_path())
        );
    }

    #[test]
    fn smoke_recording_roundtrips_and_a_missing_file_is_missing() {
        let dir = regia_test_utils::unique_temp_dir("startup-playback-frec");
        let demo = smoke_demo("sim_bridge", &["Move", "Look"]);
        let path = dir.join("playback.frec");
        save_frec(&demo, &path).expect("save");
        let loaded = load_frec(&path).expect("load");
        assert_eq!(loaded.header.seed, 1);
        assert_eq!(loaded.len(), SMOKE_STEPS as u64);
        assert_eq!(loaded.actions, vec!["Move".to_string(), "Look".to_string()]);
        assert!(loaded.keyframes.is_empty());
        assert!(loaded.timestamps.is_empty());
        match load_playback(&path) {
            PlaybackLoad::Play(recording) => {
                assert_eq!(recording.len(), SMOKE_STEPS);
                assert_eq!(recording.seed, 1);
            }
            PlaybackLoad::Missing => panic!("file should exist"),
            PlaybackLoad::Rejected(reason) => panic!("rejected: {reason}"),
        }
        assert!(matches!(
            load_playback(&dir.join("nope.frec")),
            PlaybackLoad::Missing
        ));
    }
}
