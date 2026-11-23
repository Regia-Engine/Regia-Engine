// SPDX-License-Identifier: MPL-2.0
//! One soak session: launch the app, connect over the agent protocol, sample diagnostics and
//! memory while an input script drives the app, terminate, and collect the report data.
//!
//! Windows notes: `Xvfb` has no equivalent, so headless runs open a window on the runner's
//! desktop; everything else (paths, process handling, RSS via `tasklist`) is platform-neutral
//! or split by `cfg`. The child is always a *direct* child of the tool (no wrapper scripts), so
//! `kill()` cannot leave orphans behind.

use std::fs::File;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use regia_agent::client::Client;
use regia_agent::endpoint;
use serde_json::{Value, json};

use crate::soak::budgets::{Budgets, Measured};
use crate::soak::input_script::{self, InputScript, Scheduled};
use crate::soak::report::{
    AppExitKind, ConnectInfo, CrashInfo, FrameStats, InputStats, LogCounts, SeriesStats, SessionReport,
    round3, utc_now_rfc3339,
};
use crate::soak::rss;
use crate::soak::stats;

/// How long the app gets to advertise its endpoint and answer `agent.hello` (the start budget
/// itself is checked against the *first answer*, not this cap — this is only when to give up).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// Per-call protocol timeout: how long any single agent-protocol answer may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Consecutive call timeouts after which the app counts as frozen and the session ends early.
const FROZEN_AFTER: u32 = 5;
/// How long a killed app gets to disappear before the session fails as a hang.
const KILL_TIMEOUT: Duration = Duration::from_secs(10);
/// Seconds between `logs.read` polls (records are filtered to warn-and-above server-side).
const LOG_POLL_SECS: f64 = 4.0;
/// `logs.read` page size per poll (the ring holds 4096 records).
const LOG_PAGE: u64 = 500;

/// Everything one session needs to know.
pub struct SessionConfig {
    /// 1-based session index.
    pub index: u32,
    /// Executable to run.
    pub exe: PathBuf,
    /// App arguments.
    pub args: Vec<String>,
    /// Visible window (true) or the tool's virtual display (false).
    pub windowed: bool,
    /// Session duration in seconds.
    pub duration_secs: f64,
    /// Warmup excluded from frame statistics (hitch rule).
    pub warmup_secs: f64,
    /// Diagnostics samples per second.
    pub sample_hz: f64,
    /// Optional input script.
    pub input: Option<InputScript>,
    /// `KEY=VALUE` environment overrides for the app.
    pub env: Vec<(String, String)>,
    /// Directory whose `.regia/agent/` advertises the app's endpoint.
    pub project_dir: PathBuf,
    /// Directory for this session's output files.
    pub out_dir: PathBuf,
    /// Budgets to check the session against.
    pub budgets: Budgets,
}

/// Runs one session and returns its report. Errors are setup failures (bad exe path, no out
/// dir); everything the app does wrong lands in the report.
pub fn run_session(cfg: &SessionConfig) -> Result<SessionReport> {
    std::fs::create_dir_all(&cfg.out_dir).with_context(|| format!("creating {}", cfg.out_dir.display()))?;
    let process_log = cfg.out_dir.join(format!("session-{}.process.log", cfg.index));
    let log_file =
        File::create(&process_log).with_context(|| format!("creating {}", process_log.display()))?;

    let span = tracing::info_span!("soak_session", index = cfg.index);
    let _span_guard = span.enter();

    // Headless on Linux: a private Xvfb display, so no window appears on any workspace and the
    // GPU stack still has a surface to present to (CI's lavapipe runs the same way).
    #[cfg(target_os = "linux")]
    let (mut xvfb, display): (Option<Child>, Option<String>) = if cfg.windowed {
        (None, None)
    } else {
        let (child, display) = start_xvfb()?;
        (Some(child), Some(display))
    };
    #[cfg(not(target_os = "linux"))]
    // No virtual display facility on this platform; a headless run opens a window.
    let display: Option<String> = None;

    let started_utc = utc_now_rfc3339();
    let launch = Instant::now();
    let mut child = spawn_app(cfg, &log_file, display.as_deref())
        .with_context(|| format!("launching {}", cfg.exe.display()))?;
    tracing::info!(pid = child.id(), "app launched");

    let agent_dir = endpoint::agent_dir(&cfg.project_dir);
    let connected = wait_connect(&agent_dir, &mut child, &launch)?;
    let (mut client, connect_info) = match connected {
        Some(pair) => pair,
        None => {
            // Nothing to sample; still stop what we started and report honestly.
            let died_early = child.try_wait().ok().flatten().is_some();
            let gone = terminate(&mut child);
            let status = if gone { child.wait().ok() } else { None };
            #[cfg(target_os = "linux")]
            if let Some(x) = xvfb.as_mut() {
                let _ = x.kill();
                let _ = x.wait();
            }
            let panic_text = find_panic(&process_log);
            // The app may be perfectly fine (wrong --project-dir, agent server off); a session
            // without a connection is a failed session, but not evidence of a crash unless the
            // app died by itself or panicked.
            let crashed = died_early || panic_text.is_some();
            return Ok(failed_no_endpoint(cfg, &started_utc, status, panic_text, crashed));
        }
    };
    tracing::info!(
        start_secs = round3(connect_info.start_secs),
        "agent endpoint connected"
    );

    // ---- sampling: everything below is relative to the first answer ------------------------
    let session_start = Instant::now();
    let deadline = session_start + Duration::from_secs_f64(cfg.duration_secs);
    let tick = Duration::from_secs_f64(1.0 / cfg.sample_hz);
    let mut next_log_poll = Instant::now();
    let schedule: Vec<Scheduled> = match &cfg.input {
        Some(script) => {
            input_script::expand(script, cfg.duration_secs).with_context(|| "expanding the input script")?
        }
        None => Vec::new(),
    };
    let mut next_input = 0usize;

    let mut avg_samples: Vec<f64> = Vec::new();
    // Wall-clock frame times (ms) derived from the frame counter and the wall clock, post-warmup.
    // This is the honest, unclamped frame cost: the app's own `fps_1pct_low` is capped by
    // `MAX_DELTA` (250 ms), so on a slow software-rasterised run its p99 saturates at exactly
    // 250 ms while the true wall-clock frame time is higher. One sample per diagnostics answer,
    // averaged over the ~500 ms sampling window (the frame counter delta divided by the wall
    // clock delta), so percentiles of this series are interval averages, not single frames.
    let mut frame_samples: Vec<f64> = Vec::new();
    // `(elapsed_secs, frames)` of the previous post-warmup diagnostics answer.
    let mut prev_frame: Option<(f64, u64)> = None;
    let mut samples_taken = 0usize;
    let mut frames: Option<(u64, u64)> = None; // first..last seen
    let mut entities: Option<(u64, u64, u64)> = None; // min, max, last
    let mut draw_calls_max: Option<u64> = None;
    let mut allocated_max: Option<u64> = None;
    let mut rss_max: Option<f64> = None;
    let mut rss_first: Option<f64> = None; // first post-warmup sample (leak baseline)
    let mut rss_last: Option<f64> = None; // last sample
    let mut logs = LogCounts {
        errors: 0,
        warns: 0,
        records_read: 0,
    };
    let mut log_cursor = 0u64;
    let mut input_stats = InputStats {
        entries: schedule.len() as u64,
        injected: 0,
        errors: 0,
    };
    let mut protocol_errors = 0u64;
    let mut frozen = 0u32;
    let mut early_exit: Option<ExitStatus> = None;

    loop {
        if Instant::now() >= deadline {
            break;
        }
        if let Ok(Some(status)) = child.try_wait() {
            early_exit = Some(status);
            break;
        }
        let elapsed = session_start.elapsed().as_secs_f64();
        let post_warmup = elapsed >= cfg.warmup_secs;

        // Input script entries whose time has come, batched into one protocol call per tick
        // (one blocking call per entry would serialise on the app's frame rate and starve
        // sampling; entries fire on the tick that contains their time).
        let mut due: Vec<Value> = Vec::new();
        let mut due_entries = 0u64;
        while next_input < schedule.len() && schedule[next_input].at <= elapsed {
            due.append(&mut schedule[next_input].events.clone());
            due_entries += 1;
            next_input += 1;
        }
        if !due.is_empty() {
            match client.call("play.inject_input", json!({ "events": due })) {
                Ok(_) => input_stats.injected += due_entries,
                Err(err) => {
                    input_stats.errors += due_entries;
                    tracing::warn!(entries = due_entries, %err, "inject_input failed");
                }
            }
        }

        // Logs (warn and above, paged by cursor so nothing is missed).
        if Instant::now() >= next_log_poll {
            match client.call(
                "logs.read",
                json!({ "since": log_cursor, "level": "warn", "limit": LOG_PAGE }),
            ) {
                Ok(answer) => {
                    let records = answer.get("records").and_then(Value::as_array);
                    if let Some(records) = records {
                        for record in records {
                            logs.records_read += 1;
                            let level = record.get("level").and_then(Value::as_str).unwrap_or("");
                            if level.eq_ignore_ascii_case("error") {
                                logs.errors += 1;
                            } else if level.eq_ignore_ascii_case("warn") {
                                logs.warns += 1;
                            }
                        }
                    }
                    if let Some(next) = answer.get("next").and_then(Value::as_u64) {
                        log_cursor = next;
                    }
                }
                Err(err) => {
                    protocol_errors += 1;
                    tracing::warn!(%err, "logs.read failed");
                }
            }
            next_log_poll = Instant::now() + Duration::from_secs_f64(LOG_POLL_SECS);
        }

        // Diagnostics + RSS.
        samples_taken += 1;
        match client.call("play.get_diagnostics", json!({})) {
            Ok(d) => {
                frozen = 0;
                if let Some(current) = d.get("frames").and_then(Value::as_u64) {
                    frames = Some(match frames {
                        Some((first, last)) => (first, last.max(current)),
                        None => (current, current),
                    });
                    if post_warmup {
                        let now_secs = session_start.elapsed().as_secs_f64();
                        if let Some((prev_t, prev_frames)) = prev_frame {
                            let dt = now_secs - prev_t;
                            let df = current.saturating_sub(prev_frames);
                            if dt > 0.0 && df > 0 {
                                frame_samples.push(1000.0 * dt / df as f64);
                            }
                        }
                        prev_frame = Some((now_secs, current));
                    }
                }
                if let Some(count) = d.get("entity_count").and_then(Value::as_u64) {
                    entities = Some(match entities {
                        Some((min, max, _)) => (min.min(count), max.max(count), count),
                        None => (count, count, count),
                    });
                }
                if let Some(draws) = d.get("draw_calls").and_then(Value::as_u64) {
                    draw_calls_max = Some(draws.max(draw_calls_max.unwrap_or(0)));
                }
                if let Some(bytes) = d.get("allocated_bytes").and_then(Value::as_u64) {
                    allocated_max = Some(bytes.max(allocated_max.unwrap_or(0)));
                }
                if post_warmup && let Some(avg) = d.get("frame_time_avg_ms").and_then(Value::as_f64) {
                    avg_samples.push(avg);
                }
            }
            Err(err) => {
                protocol_errors += 1;
                frozen += 1;
                tracing::warn!(%err, consecutive = frozen, "get_diagnostics failed");
                if frozen >= FROZEN_AFTER {
                    tracing::error!("app stopped answering the protocol; ending the session");
                    break;
                }
            }
        }
        if let Some(mb) = rss::rss_mb(child.id()) {
            rss_max = Some(rss_max.map_or(mb, |m| m.max(mb)));
            rss_last = Some(mb);
            if post_warmup && rss_first.is_none() {
                rss_first = Some(mb);
            }
        }

        // Pace to the next tick (drift-corrected against the session start; a slow protocol
        // answer merges ticks instead of bursting).
        let now = Instant::now();
        let target = session_start + Duration::from_secs_f64(samples_taken as f64 / cfg.sample_hz);
        if target > now {
            std::thread::sleep((target - now).min(tick));
        }
    }

    // ---- stop ----------------------------------------------------------------
    let (exit, exit_code, at_secs) = match early_exit {
        Some(status) => (
            AppExitKind::ExitedEarly,
            status.code(),
            Some(session_start.elapsed().as_secs_f64()),
        ),
        None => {
            // The loop above ends early only when the app stopped answering.
            let froze = frozen >= FROZEN_AFTER;
            let at_secs = session_start.elapsed().as_secs_f64();
            let gone = terminate(&mut child);
            if !gone {
                (AppExitKind::DidNotStop, None, None)
            } else if froze {
                (AppExitKind::Froze, None, Some(at_secs))
            } else {
                (AppExitKind::KilledByTool, None, None)
            }
        }
    };
    #[cfg(target_os = "linux")]
    if let Some(x) = xvfb.as_mut() {
        let _ = x.kill();
        let _ = x.wait();
    }
    let panic_text = find_panic(&process_log);
    let crashed = exit.is_crash(panic_text.is_some());

    // ---- stats and budgets ---------------------------------------------------
    let avg_series = stats::percentiles(&avg_samples).map(|p| SeriesStats::new(avg_samples.len(), &p));
    let frame_series = stats::percentiles(&frame_samples).map(|p| SeriesStats::new(frame_samples.len(), &p));
    let measured = Measured {
        frame_p99_ms: frame_series.as_ref().map(|s| s.p99),
        frame_max_ms: frame_series.as_ref().map(|s| s.max),
        rss_max_mb: rss_max,
        start_secs: Some(round3(connect_info.start_secs)),
        errors: Some(logs.errors),
        warns: Some(logs.warns),
        crashes: u64::from(crashed),
    };
    let violations = cfg.budgets.check(&measured);
    let passed = violations.is_empty() && !crashed;
    let frame_stats = FrameStats {
        frames_first: frames.map(|f| f.0),
        frames_last: frames.map(|f| f.1),
        avg_ms: avg_series,
        frame_ms: frame_series,
        rss_max_mb: rss_max.map(round3),
        rss_first_mb: rss_first.map(round3),
        rss_last_mb: rss_last.map(round3),
        gpu_memory_mb: None,
        entity_count: entities.map(|(min, max, last)| [min, max, last]),
        draw_calls_max,
        allocated_bytes_max: allocated_max,
    };
    Ok(SessionReport {
        session: cfg.index,
        exe: cfg
            .exe
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| cfg.exe.display().to_string()),
        app_args: cfg.args.clone(),
        windowed: cfg.windowed,
        started_utc,
        duration_secs: cfg.duration_secs,
        warmup_secs: cfg.warmup_secs,
        sample_hz: cfg.sample_hz,
        samples: samples_taken,
        connect: Some(connect_info),
        frames: frame_stats,
        logs,
        input: input_stats,
        protocol_errors,
        crash: CrashInfo {
            crashed,
            exit,
            at_secs: at_secs.map(round3),
            exit_code,
            panic_text,
        },
        measured,
        violations,
        passed,
    })
}

/// Spawns the app with its output captured to `log`. `display` redirects the app to the
/// tool-owned Xvfb instance (headless Linux runs) and detaches it from the desktop's Wayland
/// session, so winit cannot open a window on the owner's workspace.
fn spawn_app(cfg: &SessionConfig, log: &File, display: Option<&str>) -> Result<Child> {
    let mut cmd = Command::new(&cfg.exe);
    cmd.args(&cfg.args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?));
    for (key, value) in &cfg.env {
        cmd.env(key, value);
    }
    if let Some(display) = display {
        cmd.env("DISPLAY", display).env_remove("WAYLAND_DISPLAY");
    }
    cmd.spawn()
        .with_context(|| format!("spawning {}", cfg.exe.display()))
}

/// A private `Xvfb` server for headless runs: probe display numbers `:99..` until one stays up.
/// Direct child of this process (unlike `xvfb-run`, a shell script that orphans Xvfb under a
/// kill), so `kill()` cleans everything up.
#[cfg(target_os = "linux")]
fn start_xvfb() -> Result<(Child, String)> {
    for n in 99u32..130 {
        let mut child = Command::new("Xvfb")
            .args([
                format!(":{n}"),
                "-screen".into(),
                "0".into(),
                "1280x720x24".into(),
                "-nolisten".into(),
                "tcp".into(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("headless soaks on Linux need Xvfb (package xorg-server-xvfb) — or pass --windowed")?;
        // Xvfb exits at once when the display is taken; 300 ms is enough to see that.
        std::thread::sleep(Duration::from_millis(300));
        if child.try_wait()?.is_none() {
            tracing::info!(display = format!(":{n}"), "Xvfb up");
            return Ok((child, format!(":{n}")));
        }
    }
    anyhow::bail!("no free X display between :99 and :129 for Xvfb")
}

/// Waits for the app to advertise *its own* endpoint and answer `agent.hello`. Endpoint files of
/// other processes (a stale file from a crashed run) are ignored via the pid.
fn wait_connect(
    agent_dir: &Path,
    child: &mut Child,
    launch: &Instant,
) -> Result<Option<(Client, ConnectInfo)>> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok((advertised, token)) = endpoint::read(agent_dir)
            && advertised.pid == child.id()
            && let Ok(mut client) = Client::connect(&advertised.url, &token)
            && let Ok(hello) = client.call("agent.hello", json!({ "client": "xtask-soak" }))
        {
            let connect = ConnectInfo {
                start_secs: launch.elapsed().as_secs_f64(),
                protocol: hello
                    .get("protocol")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                app: hello
                    .get("app")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                os: hello.get("os").and_then(Value::as_str).unwrap_or_default().into(),
                pid: advertised.pid,
                methods: hello.get("methods").and_then(Value::as_array).map_or(0, Vec::len),
            };
            client.set_read_timeout(Some(CALL_TIMEOUT))?;
            return Ok(Some((client, connect)));
        }
        // The app may have died before advertising anything.
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(None)
}

/// Terminates the child and waits up to [`KILL_TIMEOUT`]; true when it is gone.
fn terminate(child: &mut Child) -> bool {
    if child.kill().is_ok() {
        let deadline = Instant::now() + KILL_TIMEOUT;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    false
}

/// First line of the captured output that looks like a panic (the engine's panic hook logs
/// "panicked at ..."; the standard runtime prints the same words).
fn find_panic(process_log: &Path) -> Option<String> {
    let file = File::open(process_log).ok()?;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        if line.contains("panicked at") || line.contains("panicked with") {
            return Some(line);
        }
    }
    None
}

/// A session that never produced an endpoint: no sampling happened, so frame stats are empty and
/// log counts are unmeasured. The verdict is a fail; `crashed` is only true when the app died by
/// itself or panicked (a wrong `--project-dir` is not evidence of a crash).
fn failed_no_endpoint(
    cfg: &SessionConfig,
    started_utc: &str,
    status: Option<ExitStatus>,
    panic_text: Option<String>,
    crashed: bool,
) -> SessionReport {
    let measured = Measured {
        frame_p99_ms: None,
        frame_max_ms: None,
        rss_max_mb: None,
        start_secs: None,
        errors: None,
        warns: None,
        crashes: u64::from(crashed),
    };
    let violations = cfg.budgets.check(&measured);
    SessionReport {
        session: cfg.index,
        exe: cfg
            .exe
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| cfg.exe.display().to_string()),
        app_args: cfg.args.clone(),
        windowed: cfg.windowed,
        started_utc: started_utc.into(),
        duration_secs: cfg.duration_secs,
        warmup_secs: cfg.warmup_secs,
        sample_hz: cfg.sample_hz,
        samples: 0,
        connect: None,
        frames: FrameStats {
            frames_first: None,
            frames_last: None,
            avg_ms: None,
            frame_ms: None,
            rss_max_mb: None,
            rss_first_mb: None,
            rss_last_mb: None,
            gpu_memory_mb: None,
            entity_count: None,
            draw_calls_max: None,
            allocated_bytes_max: None,
        },
        logs: LogCounts {
            errors: 0,
            warns: 0,
            records_read: 0,
        },
        input: InputStats {
            entries: 0,
            injected: 0,
            errors: 0,
        },
        protocol_errors: 0,
        crash: CrashInfo {
            crashed,
            exit: AppExitKind::NoEndpoint,
            at_secs: None,
            exit_code: status.and_then(|s| s.code()),
            panic_text,
        },
        measured,
        violations,
        passed: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn panic_scan_finds_the_hook_line() {
        let dir = std::env::temp_dir().join(format!("soak-panic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("process.log");
        std::fs::write(
            &path,
            "info line\nthread 'main' panicked at src/main.rs:4:5:\nassertion failed\nnote: run with RUST_BACKTRACE=1\n",
        )
        .expect("written");
        let found = find_panic(&path).expect("panic found");
        assert!(found.contains("panicked at src/main.rs:4:5"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn panic_scan_ignores_clean_logs() {
        let dir = std::env::temp_dir().join(format!("soak-clean-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("process.log");
        std::fs::write(&path, "started\nrunning fine\n").expect("written");
        assert_eq!(find_panic(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_log_file_is_not_a_panic() {
        assert_eq!(find_panic(Path::new("/nonexistent/soak/none.log")), None);
    }
}
