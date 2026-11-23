// SPDX-License-Identifier: MIT-0
//! Spec 28 `summary.json` for `do27_chase`.
//!
//! The chase is not an app world, so it does not run TelemetryPlugin's 60 s
//! writer thread. It writes the same [`SessionSummary`] (schema 1) at
//! exit, plus one partial when the run starts, through [`write_atomic`].
//! Home directories in the host section are scrubbed.

use std::path::Path;

use regia_telemetry::{
    DisplayInfo, ErrorsInfo, ExitInfo, FrameStats, RegiaInfo, SUMMARY_SCHEMA, SessionInfo, SessionSummary,
    ShadersInfo, SummaryPaths, SystemInfo, write_atomic,
};

use crate::LookdevError;
use crate::chase::{DT, percentile};

/// What one `do27_chase` process records.
#[derive(Clone, Debug)]
pub struct RunRecord {
    /// `--scenario`, or `chase`.
    pub scenario: String,
    /// The frames were presented to a window.
    pub window: bool,
    /// Map names the process loaded.
    pub maps: Vec<String>,
    /// Chase width, px.
    pub width: u32,
    /// Chase height, px.
    pub height: u32,
    /// Measured frame wall times, ms.
    pub wall_ms: Vec<f32>,
    /// Pipeline warm time, ms.
    pub prewarm_ms: f32,
    /// `hit` or `miss` from the pipeline-cache directory at process start.
    pub cache: String,
    /// Adapter name.
    pub gpu: String,
    /// Graphics backend (`Vulkan`).
    pub backend: String,
    /// Content directory, scrubbed before it is written.
    pub content: String,
    /// The process reached the end of `main_inner` without an error.
    pub clean: bool,
    /// `--crash-test` wrote this record and is about to panic.
    pub panic: bool,
    /// Free-form note (an error string on the failure path).
    pub note: String,
}

/// Measured frames for a replay of `seconds` at [`DT`].
///
/// `None` when `seconds` is not a positive finite duration. The count is clamped to
/// 1_000_000 so a bad flag cannot ask for an unbounded run.
#[must_use]
pub fn frames_for_seconds(seconds: f64) -> Option<u32> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    let frames = (seconds / f64::from(DT)).round();
    if !frames.is_finite() || frames < 1.0 {
        return None;
    }
    Some(frames.min(1_000_000.0) as u32)
}

/// `hit` when `dir` already contains a file, otherwise `miss` (empty, missing, or unset).
#[must_use]
pub fn cache_label(dir: Option<&Path>) -> &'static str {
    match dir {
        Some(dir) if dir_has_file(dir) => "hit",
        _ => "miss",
    }
}

/// [`cache_label`] of the process pipeline-cache directory, before this run compiles.
#[must_use]
pub fn cache_label_now() -> &'static str {
    cache_label(regia_render::pipeline_cache::directory().as_deref())
}

fn dir_has_file(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .ok()
        .and_then(|mut entries| entries.find_map(Result::ok))
        .is_some_and(|entry| entry.path().is_file())
}

/// Builds the schema-1 record. `partial` omits `exit` and sets `partial: true`.
#[must_use]
pub fn build(record: &RunRecord, partial: bool) -> SessionSummary {
    let content = regia_core::privacy::scrub_home(&record.content).into_owned();
    let host = serde_json::json!({
        "app": "do27_chase",
        "scenario": record.scenario,
        "window": record.window,
        "maps": record.maps,
        "content": content,
        "note": record.note,
    });
    let mut summary = SessionSummary {
        schema: SUMMARY_SCHEMA,
        partial: partial.then_some(true),
        regia: Some(RegiaInfo {
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            git: Some(option_env!("REGIA_GIT_HASH").unwrap_or("unknown").to_owned()),
            profile: Some(
                if cfg!(debug_assertions) {
                    "debug"
                } else {
                    "release"
                }
                .to_owned(),
            ),
        }),
        host: Some(host),
        system: Some(SystemInfo {
            gpu: Some(record.gpu.clone()),
            backend: Some(record.backend.clone()),
            ..SystemInfo::default()
        }),
        display: Some(DisplayInfo {
            width: Some(record.width),
            height: Some(record.height),
            mode: Some(if record.window { "window" } else { "headless" }.to_owned()),
            present: Some(if record.window { "fifo" } else { "offscreen" }.to_owned()),
            ..DisplayInfo::default()
        }),
        session: Some(SessionInfo {
            seconds: Some(record.wall_ms.iter().map(|ms| f64::from(*ms) / 1000.0).sum()),
            frames: Some(record.wall_ms.len() as u64),
            maps: Some(record.maps.clone()),
        }),
        frames: Some(frame_stats(&record.wall_ms)),
        shaders: Some(ShadersInfo {
            ms: Some(f64::from(record.prewarm_ms)),
            cache: Some(record.cache.clone()),
            backend: Some(record.backend.clone()),
            ..ShadersInfo::default()
        }),
        errors: record.panic.then(|| ErrorsInfo {
            panics: Some(1),
            ..ErrorsInfo::default()
        }),
        exit: if partial {
            None
        } else {
            Some(ExitInfo {
                clean: Some(record.clean && !record.panic),
                ms: Some(0.0),
                watchdog: Some(false),
                panic: Some(record.panic),
                ..ExitInfo::default()
            })
        },
        ..SessionSummary::default()
    };
    if record.gpu.is_empty() {
        summary.system = None;
    }
    summary
}

fn frame_stats(wall_ms: &[f32]) -> FrameStats {
    let frames = wall_ms.len() as u64;
    let share = |pred: fn(f32) -> bool| -> f64 {
        if frames == 0 {
            0.0
        } else {
            wall_ms.iter().filter(|ms| pred(**ms)).count() as f64 * 100.0 / frames as f64
        }
    };
    FrameStats {
        frames: Some(frames),
        p50_ms: Some(f64::from(percentile(wall_ms, 0.50))),
        p95_ms: Some(f64::from(percentile(wall_ms, 0.95))),
        p99_ms: Some(f64::from(percentile(wall_ms, 0.99))),
        over_33ms_pct: Some(share(|ms| ms > 33.3)),
        over_50ms_pct: Some(share(|ms| ms > 50.0)),
        bound: Some("unknown".to_owned()),
    }
}

/// Writes `summary.partial.json` beside the final path.
///
/// # Errors
///
/// The partial file could not be created.
pub fn write_partial(final_path: &Path, record: &RunRecord) -> Result<(), LookdevError> {
    let paths = SummaryPaths::from_final(final_path);
    write_text(&paths.partial_path, &build(record, true).to_json())
}

/// Writes the final `summary.json`.
///
/// # Errors
///
/// The file could not be created.
pub fn write_run(final_path: &Path, record: &RunRecord) -> Result<(), LookdevError> {
    write_text(final_path, &build(record, false).to_json())
}

/// The crash-test record: `exit.clean` false, `exit.panic` true, no map loaded.
///
/// # Errors
///
/// The file could not be created.
pub fn write_panic(final_path: &Path, scenario: &str) -> Result<(), LookdevError> {
    write_run(final_path, &panic_record(scenario))
}

/// A failed run that did not reach the normal writer. Skipped when the file already exists
/// so a crash record is not overwritten by the `main` error path.
///
/// # Errors
///
/// The file could not be created.
pub fn write_failure(final_path: &Path, scenario: &str, note: &str) -> Result<(), LookdevError> {
    if final_path.exists() {
        return Ok(());
    }
    let mut record = panic_record(scenario);
    record.panic = false;
    record.clean = false;
    record.note = note.to_owned();
    record.cache = cache_label_now().to_owned();
    write_run(final_path, &record)
}

fn panic_record(scenario: &str) -> RunRecord {
    RunRecord {
        scenario: scenario.to_owned(),
        window: false,
        maps: Vec::new(),
        width: 0,
        height: 0,
        wall_ms: Vec::new(),
        prewarm_ms: 0.0,
        cache: "miss".to_owned(),
        gpu: String::new(),
        backend: "Vulkan".to_owned(),
        content: String::new(),
        clean: false,
        panic: true,
        note: "do27_chase --crash-test".to_owned(),
    }
}

fn write_text(path: &Path, text: &str) -> Result<(), LookdevError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| LookdevError::Io {
            path: parent.display().to_string(),
            source,
        })?;
    }
    write_atomic(path, text).map_err(|source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RunRecord {
        RunRecord {
            scenario: "replay".to_owned(),
            window: false,
            maps: vec!["switchbacks".to_owned()],
            width: 2560,
            height: 1440,
            wall_ms: vec![10.0, 20.0, 30.0, 40.0],
            prewarm_ms: 12.5,
            cache: "miss".to_owned(),
            gpu: "Test GPU".to_owned(),
            backend: "Vulkan".to_owned(),
            content: {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp/regia-no-home".into());
                format!("{home}/ferrite-do27-content")
            },
            clean: true,
            panic: false,
            note: String::new(),
        }
    }

    #[test]
    fn ten_minutes_is_thirty_six_thousand_frames() {
        assert_eq!(frames_for_seconds(600.0), Some(36_000));
        assert_eq!(frames_for_seconds(1.0 / 60.0), Some(1));
        assert_eq!(frames_for_seconds(0.0), None);
        assert_eq!(frames_for_seconds(-1.0), None);
        assert_eq!(frames_for_seconds(f64::NAN), None);
        assert_eq!(frames_for_seconds(1.0e12), Some(1_000_000));
    }

    #[test]
    fn summary_is_schema_one_and_scrubs_a_home_path() {
        let summary = build(&sample(), false);
        let text = summary.to_json();
        let value: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(value["schema"], 1);
        assert!(value.get("partial").is_none());
        assert_eq!(value["host"]["app"], "do27_chase");
        assert_eq!(value["host"]["scenario"], "replay");
        assert_eq!(value["exit"]["clean"], true);
        assert_eq!(value["exit"]["panic"], false);
        assert_eq!(value["shaders"]["cache"], "miss");
        assert_eq!(value["display"]["mode"], "headless");
        assert_eq!(value["frames"]["frames"], 4);
        let content = value["host"]["content"].as_str().expect("content");
        if let Ok(home) = std::env::var("HOME")
            && home.len() >= 3
        {
            assert!(!content.contains(&home), "{content}");
            assert!(content.starts_with('~'), "{content}");
        }
        let partial = build(&sample(), true);
        assert_eq!(partial.partial, Some(true));
        assert!(partial.exit.is_none());
    }

    #[test]
    fn a_crash_record_is_not_a_clean_exit() {
        let dir = std::env::temp_dir().join(format!("regia-summary-{}", std::process::id()));
        let path = dir.join("summary.json");
        let _ = std::fs::remove_dir_all(&dir);
        write_panic(&path, "crash").expect("write");
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(value["schema"], 1);
        assert_eq!(value["exit"]["clean"], false);
        assert_eq!(value["exit"]["panic"], true);
        assert_eq!(value["errors"]["panics"], 1);
        let paths = SummaryPaths::from_final(&path);
        write_partial(&path, &sample()).expect("partial");
        assert!(paths.partial_path.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_cache_dir_is_a_miss_and_a_file_is_a_hit() {
        let dir = std::env::temp_dir().join(format!("regia-cache-label-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        assert_eq!(cache_label(Some(&dir)), "miss");
        std::fs::write(dir.join("pipelines.bin"), b"x").expect("file");
        assert_eq!(cache_label(Some(&dir)), "hit");
        assert_eq!(cache_label(None), "miss");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
