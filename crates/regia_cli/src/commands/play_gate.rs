// SPDX-License-Identifier: MPL-2.0
//! `regia play-gate replay|check|run` (ADR-8403, ADR-8452, `book/agents/play-gate.md`).
//!
//! `check` parses a `.track.ron` file. `replay` walks it on a virtual clock and matches expect
//! steps against the log. `run` installs a package into a fresh home and drives physical keys.
//! Exit 0 when the command passes, 1 when a play fails, 2 when the track or the arguments are
//! refused ([`PlayGateUsage`]).

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use regia_cli::key_track::{
    self, CODE_EXPECT_MISSED, CODE_LOG_ERROR, LogLine, PreparedTrack, Replay, TrackError,
};

/// `regia play-gate` subcommands.
#[derive(Debug, Subcommand)]
pub enum PlayGateCommand {
    /// Replay a key track and match its expect steps against a log.
    Replay(ReplayArgs),
    /// Parse a key track and report whether it is valid.
    Check(CheckArgs),
    /// Install a package into a fresh home and drive its physical keys.
    Run(RunArgs),
}

/// `regia play-gate replay` arguments.
#[derive(Debug, Args)]
pub struct ReplayArgs {
    /// Path of the `.track.ron` file.
    pub track: PathBuf,
    /// Log file. A line `MILLIS|text` is timed; any other line is visible at time 0.
    #[arg(long)]
    pub log: Option<PathBuf>,
    /// Fail when the log has a JSON line whose `level` is `ERROR`, or a `{` line that is not JSON.
    #[arg(long, default_value_t = false)]
    pub require_clean_log: bool,
    /// Print one JSON object on stdout.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `regia play-gate check` arguments.
#[derive(Debug, Args)]
pub struct CheckArgs {
    /// Path of the `.track.ron` file.
    pub track: PathBuf,
    /// Print one JSON object on stdout.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `regia play-gate run` arguments.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Ustar archive, or a directory that already holds `player` and `features.json`.
    pub archive: PathBuf,
    /// Path of the `.track.ron` file.
    pub track: PathBuf,
    /// Install into a new home under `--out`. Required. Without it, nothing is installed.
    #[arg(long, default_value_t = false)]
    pub fresh_home: bool,
    /// Directory for the fresh home and the reports (`play_gate.md`, `play_gate.json`).
    #[arg(long)]
    pub out: PathBuf,
    /// Keymap file. Each line is `name = SCANCODE`. A name is never sent as text.
    #[arg(long)]
    pub keymap: Option<PathBuf>,
    /// Print one JSON object on stdout.
    #[arg(long, default_value_t = false)]
    pub json: bool,
    /// Frame budget for expects taken from the track.
    ///
    /// A duration stored as milliseconds in the track counts as that many frames on this path.
    #[arg(long)]
    pub within_frames: Option<u64>,
}

/// A refused track or a bad argument. The process exits 2.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct PlayGateUsage {
    /// Stable code and reason (`track_parse`, `track_version`, `track_io`, `play_gate_fresh_home`
    /// or `play_gate_keymap`).
    pub message: String,
}

/// A replay that ran and failed. The process exits 1.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct PlayGateFailed {
    /// Stable code and reason (`track_expect_missed` or `track_log_error`).
    pub message: String,
}

fn usage(err: TrackError) -> anyhow::Error {
    PlayGateUsage {
        message: err.to_string(),
    }
    .into()
}

fn failed(err: TrackError) -> anyhow::Error {
    PlayGateFailed {
        message: err.to_string(),
    }
    .into()
}

fn emit(json_mode: bool, body: &Value, human: &str, err: Option<anyhow::Error>) -> Result<()> {
    if json_mode {
        println!("{body}");
    } else if err.is_none() {
        println!("{human}");
    }
    match err {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Runs `regia play-gate`.
///
/// # Errors
///
/// [`PlayGateUsage`] for a bad track or arguments (exit 2). [`PlayGateFailed`] when an expect
/// misses, the clean-log scan fails, install fails, or the build has no agent (exit 1).
pub fn run(command: &PlayGateCommand) -> Result<()> {
    match command {
        PlayGateCommand::Check(args) => check(args),
        PlayGateCommand::Replay(args) => replay(args),
        PlayGateCommand::Run(args) => run_install(args),
    }
}

/// FNV-1a of the archive path. The same path is the same per-run token seed.
fn token_seed(path: &std::path::Path) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in path.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn run_install(args: &RunArgs) -> Result<()> {
    let track_text = std::fs::read_to_string(&args.track).map_err(|err| {
        usage(key_track::TrackError::Io(format!(
            "reading {}: {err}",
            args.track.display()
        )))
    })?;
    let keymap_text = match &args.keymap {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                return Err(PlayGateUsage {
                    message: format!("play_gate_keymap: reading {}: {err}", path.display()),
                }
                .into());
            }
        },
        None => String::new(),
    };
    let mut request = regia_cli::play_install::GateRequest::new(&args.archive, &args.out);
    request.fresh_home = args.fresh_home;
    request.track_text = Some(track_text);
    request.keymap_text = keymap_text;
    request.token_seed = token_seed(&args.archive);
    request.parent_env = std::env::vars().collect();
    if let Some(frames) = args.within_frames {
        request.within_frames = frames;
    }
    let report = regia_cli::play_install::run_gate(&request);
    let body = regia_cli::play_install::report_json(&report);
    let human = if report.ok {
        "play-gate: ok".to_string()
    } else {
        format!(
            "{}: {}",
            report.code.as_deref().unwrap_or("play_gate"),
            report.message
        )
    };
    let err = match report.exit_code {
        0 => None,
        2 => Some(
            PlayGateUsage {
                message: human.clone(),
            }
            .into(),
        ),
        _ => Some(
            PlayGateFailed {
                message: human.clone(),
            }
            .into(),
        ),
    };
    emit(args.json, &body, &human, err)
}

fn check(args: &CheckArgs) -> Result<()> {
    let prepared = PreparedTrack::load(&args.track).map_err(usage)?;
    let steps = prepared.track().steps.len();
    let body = json!({
        "ok": true,
        "steps": steps,
    });
    emit(args.json, &body, &format!("play-gate: ok, {steps} steps"), None)
}

fn replay(args: &ReplayArgs) -> Result<()> {
    if args.require_clean_log && args.log.is_none() {
        return Err(usage(TrackError::Parse(
            "--require-clean-log needs --log".to_string(),
        )));
    }
    let prepared = PreparedTrack::load(&args.track).map_err(usage)?;
    let log_text = match &args.log {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .map_err(|e| usage(TrackError::Io(format!("reading {}: {e}", path.display()))))?,
        ),
        None => None,
    };
    let lines: Vec<LogLine> = log_text
        .as_deref()
        .unwrap_or("")
        .lines()
        .map(key_track::log_line_from_text)
        .collect();
    let played = prepared.replay(&lines);
    if let Some(pattern) = &played.failed {
        let err = TrackError::ExpectMissed {
            pattern: pattern.clone(),
        };
        let body = json!({
            "ok": false,
            "code": CODE_EXPECT_MISSED,
            "pattern": pattern,
            "message": err.to_string(),
        });
        return emit(args.json, &body, "", Some(failed(err)));
    }
    if args.require_clean_log
        && let Some(text) = log_text.as_deref()
        && let Err(hits) = key_track::scan_error_log(text)
    {
        let named = hits
            .first()
            .map(|hit| format!("line {}: {}", hit.line, key_track::name_log_hit(hit)))
            .unwrap_or_else(|| "empty hit".to_string());
        let err = TrackError::Log(format!("{named} ({} hits)", hits.len()));
        let body = json!({
            "ok": false,
            "code": CODE_LOG_ERROR,
            "message": err.to_string(),
        });
        return emit(args.json, &body, "", Some(failed(err)));
    }
    let body = success_json(&played);
    let human = format!(
        "play-gate: ok, {} keys, {} expects",
        played.keys.len(),
        played.expects.len()
    );
    emit(args.json, &body, &human, None)
}

fn success_json(played: &Replay) -> Value {
    let keys: Vec<Value> = played
        .keys
        .iter()
        .map(|key| {
            json!({
                "at_millis": key.at_millis,
                "edge": match key.edge {
                    key_track::Edge::Down => "down",
                    key_track::Edge::Up => "up",
                },
                "target": key.target,
                "section": key.section,
            })
        })
        .collect();
    let matched: Vec<Value> = played
        .expects
        .iter()
        .map(|expect| {
            json!({
                "pattern": expect.pattern,
                "optional": expect.optional,
                "matched": expect.matched,
                "line_index": expect.line_index,
            })
        })
        .collect();
    let notes: Vec<Value> = played
        .notes
        .iter()
        .map(|note| json!({ "at_millis": note.at_millis, "text": note.text }))
        .collect();
    json!({
        "ok": true,
        "keys": keys,
        "matched": matched,
        "notes": notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn write_atomic(path: &Path, bytes: &[u8]) {
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, bytes).expect("write tmp");
        fs::rename(&tmp, path).expect("rename");
    }

    #[test]
    fn replay_require_clean_log_fails_on_an_error_record() {
        let dir = regia_test_utils::unique_temp_dir("play-gate-log");
        let track = dir.join("boot.track.ron");
        let log = dir.join("run.jsonl");
        write_atomic(
            &track,
            b"(format_version: 1, steps: [Expect(pattern: \"ready\", within_millis: 1000, optional: false)])",
        );
        write_atomic(
            &log,
            b"0|ready\n{\"level\":\"ERROR\",\"target\":\"app\",\"msg\":\"boom\"}\n",
        );
        let err = run(&PlayGateCommand::Replay(ReplayArgs {
            track,
            log: Some(log),
            require_clean_log: true,
            json: true,
        }))
        .expect_err("error record fails");
        assert!(err.downcast_ref::<PlayGateFailed>().is_some());
        assert!(err.downcast_ref::<PlayGateUsage>().is_none());
        assert!(err.to_string().contains("track_log_error"), "{err}");
    }

    #[test]
    fn a_bad_track_is_usage_and_a_miss_is_not() {
        let dir = regia_test_utils::unique_temp_dir("play-gate-miss");
        let track = dir.join("boot.track.ron");
        write_atomic(
            &track,
            b"(format_version: 1, steps: [Expect(pattern: \"ready\", within_millis: 1, optional: false)])",
        );
        let err = run(&PlayGateCommand::Replay(ReplayArgs {
            track: track.clone(),
            log: None,
            require_clean_log: false,
            json: false,
        }))
        .expect_err("miss");
        assert!(err.downcast_ref::<PlayGateFailed>().is_some());
        assert!(err.to_string().contains("track_expect_missed"));

        write_atomic(&track, b"this is not ron");
        let err = run(&PlayGateCommand::Check(CheckArgs { track, json: false })).expect_err("parse");
        assert!(err.downcast_ref::<PlayGateUsage>().is_some(), "{err}");
        assert!(err.to_string().contains("track_parse"), "{err}");
    }

    #[test]
    fn run_without_fresh_home_is_usage() {
        let dir = regia_test_utils::unique_temp_dir("play-gate-run");
        let track = dir.join("boot.track.ron");
        write_atomic(&track, b"(format_version: 1, steps: [])");
        let err = run(&PlayGateCommand::Run(RunArgs {
            archive: dir.join("missing.tar"),
            track,
            fresh_home: false,
            out: dir.join("out"),
            keymap: None,
            json: false,
            within_frames: None,
        }))
        .expect_err("fresh home");
        assert!(err.downcast_ref::<PlayGateUsage>().is_some(), "{err}");
        assert!(err.to_string().contains("play_gate_fresh_home"), "{err}");
        assert!(!dir.join("out").join("home").exists());
        assert!(dir.join("out").join("play_gate.json").is_file());
    }
}
