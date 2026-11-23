// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask soak` — long-run stability sessions with budget checks (`docs/06` § 1 "Soak",
//! § 4 "Budgets", M18).
//!
//! One *session* launches the app, connects over the agent protocol (`specs/23`), samples
//! diagnostics and memory while an optional input script drives the app, then writes a JSON and
//! Markdown report and checks the measurements against a budgets file. `--sessions K` repeats the
//! session K times; the run passes only when every session meets every budget.
//!
//! Frame-time sampling: `play.get_diagnostics` reports windowed averages over the last 300 frames
//! (`frame_time_avg_ms`) and the average of the worst 1% of frames as `fps_1pct_low` (`specs/10`
//! § 3.3). The soak samples at `--sample-hz` and reports percentiles of each series. The *p99/max
//! frame* budgets are **not** taken from `fps_1pct_low` — that value is capped by the app's
//! `MAX_DELTA` (250 ms) and so censors a slow, software-rasterised run at exactly 250 ms. Instead
//! the soak derives an unclamped frame-time series from the frame counter and the wall clock
//! (`1000 · Δwall / Δframes` per sample) and checks the p99/max budgets against that. Warmup (the
//! hitch rule's "first 10 s of play") is excluded from the frame series; log counts and RSS are
//! whole-session.

pub mod budgets;
pub mod input_script;
pub mod report;
pub mod rss;
pub mod runner;
pub mod stats;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::soak::budgets::Budgets;
use crate::soak::input_script::InputScript;
use crate::soak::report::{SummaryReport, load_sessions, write_session, write_summary};
use crate::soak::runner::SessionConfig;

/// Arguments of `cargo xtask soak`.
#[derive(Debug, Clone, Args)]
pub struct SoakArgs {
    /// App executable to soak (absolute, or relative to the workspace root).
    #[arg(long)]
    pub exe: PathBuf,
    /// App arguments: everything after `--` on the command line, e.g.
    /// `cargo xtask soak --exe X --minutes 2 -- --exit-after-frames 10000`.
    #[arg(last = true, value_name = "APP_ARGS")]
    pub args: Vec<String>,
    /// Minutes per session.
    #[arg(long)]
    pub minutes: f64,
    /// Number of sessions to run, one after the other.
    #[arg(long, default_value_t = 1)]
    pub sessions: u32,
    /// Run the app on a visible window. Default is headless: a private Xvfb display on Linux
    /// (as CI's windowed smoke runs); Windows has no virtual display, so headless runs there
    /// open a window on the runner's desktop.
    #[arg(long)]
    pub windowed: bool,
    /// RON input script driving the session (press/hold/look patterns; see the module docs of
    /// [`input_script`]). Without a script the soak only samples.
    #[arg(long)]
    pub input: Option<PathBuf>,
    /// Budgets file to check against (default `budgets/default.ron`).
    #[arg(long)]
    pub budgets: Option<PathBuf>,
    /// Project directory whose `.regia/agent/` advertises the app's agent endpoint; where the
    /// app writes it is the app's choice (`AgentSettings::project_dir`). Default: the
    /// executable's directory. For `samples/sim_bridge` pass `--project-dir samples/sim_bridge`.
    #[arg(long)]
    pub project_dir: Option<PathBuf>,
    /// Environment variable passed to the app (`KEY=VALUE`, repeatable), e.g.
    /// `--env REGIA_ASSETS=<repo>/samples/sim_bridge/assets` for a binary run outside
    /// `cargo run` (which is how the development `assets://` root resolves otherwise).
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    /// Directory for the per-session and summary reports (default `docs/evidence/m18`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Diagnostics samples per second (default 2; one sample ≈ one protocol call).
    #[arg(long, default_value_t = 2.0)]
    pub sample_hz: f64,
}

/// Subcommands of `cargo xtask budgets`.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum BudgetsCommand {
    /// Check soak report(s) against a budgets file without running anything.
    Check {
        /// Budgets file (default `budgets/default.ron`).
        #[arg(long)]
        budgets: Option<PathBuf>,
        /// Report files to check: session JSONs and/or a summary JSON.
        #[arg(required = true)]
        reports: Vec<PathBuf>,
    },
}

/// `cargo xtask soak`: runs the sessions, writes reports, and exits 0 only on a clean pass.
pub fn run(args: &SoakArgs, root: &Path) -> Result<ExitCode> {
    if args.minutes <= 0.0 || !args.minutes.is_finite() {
        bail!(
            "--minutes must be a positive number of minutes, got {}",
            args.minutes
        );
    }
    if args.sessions == 0 {
        bail!("--sessions must be at least 1");
    }
    if !args.sample_hz.is_finite() || args.sample_hz <= 0.0 || args.sample_hz > 10.0 {
        bail!("--sample-hz must be in (0, 10], got {}", args.sample_hz);
    }
    let tracing_env = std::env::var("RUST_LOG").unwrap_or_else(|_| "xtask=info".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new(&tracing_env)
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("xtask=info")),
        )
        .with_target(false)
        .try_init();

    let exe = resolve(root, &args.exe)?;
    if !exe.is_file() {
        bail!("--exe {} does not exist (build it first?)", exe.display());
    }
    let budgets_path = match &args.budgets {
        Some(p) => resolve(root, p)?,
        None => Budgets::default_path(root),
    };
    let budgets = Budgets::load(&budgets_path)?;
    let input = match &args.input {
        Some(p) => {
            let path = resolve(root, p)?;
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading input script {}", path.display()))?;
            Some(
                ron::from_str::<InputScript>(&text)
                    .with_context(|| format!("parsing input script {}", path.display()))?,
            )
        }
        None => None,
    };
    let mut env = Vec::with_capacity(args.env.len());
    for pair in &args.env {
        let Some((key, value)) = pair.split_once('=') else {
            bail!("--env wants KEY=VALUE, got {pair:?}");
        };
        if key.is_empty() {
            bail!("--env wants KEY=VALUE, got {pair:?}");
        }
        env.push((key.to_string(), value.to_string()));
    }
    let project_dir = match &args.project_dir {
        Some(p) => resolve(root, p)?,
        None => exe
            .parent()
            .map(Path::to_path_buf)
            .context("exe has no parent directory")?,
    };
    let out_dir = match &args.out {
        Some(p) => resolve(root, p)?,
        None => root.join("docs").join("evidence").join("m18"),
    };
    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    let started_utc = report::utc_now_rfc3339();
    let duration_secs = args.minutes * 60.0;
    let mut sessions = Vec::with_capacity(args.sessions as usize);
    println!(
        "soak: {} session(s) of {:.1} min — {} (windowed: {})",
        args.sessions,
        args.minutes,
        exe.display(),
        args.windowed
    );
    for index in 1..=args.sessions {
        println!("soak: session {}/{} starting", index, args.sessions);
        let cfg = SessionConfig {
            index,
            exe: exe.clone(),
            args: args.args.clone(),
            windowed: args.windowed,
            duration_secs,
            warmup_secs: budgets.warmup_secs,
            sample_hz: args.sample_hz,
            input: input.clone(),
            env: env.clone(),
            project_dir: project_dir.clone(),
            out_dir: out_dir.clone(),
            budgets: budgets.clone(),
        };
        let session = runner::run_session(&cfg)?;
        let json = write_session(&out_dir, &session)?;
        println!(
            "soak: session {} — {} (report: {})",
            index,
            if session.passed { "pass" } else { "FAIL" },
            json.display()
        );
        sessions.push(session);
    }

    let passed = sessions.iter().all(|s| s.passed);
    let summary = SummaryReport {
        started_utc,
        exe: sessions
            .first()
            .map(|s| s.exe.clone())
            .unwrap_or_else(|| exe.display().to_string()),
        minutes: args.minutes,
        sessions_count: args.sessions,
        sessions,
        passed,
    };
    let json = write_summary(&out_dir, &summary)?;
    println!(
        "soak: {} — {} session(s) (summary: {})",
        if passed { "PASS" } else { "FAIL" },
        args.sessions,
        json.display()
    );
    Ok(if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// `cargo xtask budgets check`: re-checks written reports against a budgets file; exits 0 when
/// every report meets every budget it carries measurements for.
pub fn budgets_command(command: &BudgetsCommand, root: &Path) -> Result<ExitCode> {
    let BudgetsCommand::Check { budgets, reports } = command;
    let budgets_path = match budgets {
        Some(p) => resolve(root, p)?,
        None => Budgets::default_path(root),
    };
    let budgets = Budgets::load(&budgets_path)?;
    let mut all_pass = true;
    for path in reports {
        let sessions = load_sessions(path)?;
        for session in &sessions {
            let violations = budgets.check(&session.measured);
            let crashed = session.measured.crashes > 0;
            let pass = violations.is_empty() && !crashed && session.connect.is_some();
            println!(
                "budgets: session {} of {} — {}",
                session.session,
                path.display(),
                if pass { "pass" } else { "FAIL" }
            );
            for v in &violations {
                println!("  {}: {} > {}", v.metric, v.actual, v.limit);
            }
            if crashed {
                println!("  crashed during the session");
            }
            if session.connect.is_none() {
                println!("  never connected — no evidence");
            }
            all_pass &= pass;
        }
    }
    Ok(if all_pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// `p` when absolute, else below the workspace root.
fn resolve(root: &Path, p: &Path) -> Result<PathBuf> {
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(root.join(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::soak::report::SessionReport;

    #[test]
    fn relative_paths_go_below_the_root() {
        let root = Path::new("/repo");
        assert_eq!(
            resolve(root, Path::new("target/debug/sim_bridge")).expect("resolve relative"),
            PathBuf::from("/repo/target/debug/sim_bridge")
        );
        assert_eq!(
            resolve(root, Path::new("/abs/sim_bridge")).expect("resolve absolute"),
            PathBuf::from("/abs/sim_bridge")
        );
    }

    #[test]
    fn budgets_check_of_a_written_report_round_trips() {
        // End to end through the public report types: a session JSON with one breach fails,
        // a clean one passes, and a summary unwraps into its sessions.
        let dir = std::env::temp_dir().join(format!("soak-budgets-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let mut session = clean_session();
        let clean = dir.join("clean.json");
        std::fs::write(
            &clean,
            serde_json::to_string_pretty(&session).expect("serialise session"),
        )
        .expect("write");
        session.measured.frame_p99_ms = Some(9.9);
        session.violations = session_budgets().check(&session.measured);
        session.passed = false;
        let breached = dir.join("breached.json");
        std::fs::write(
            &breached,
            serde_json::to_string_pretty(&session).expect("serialise session"),
        )
        .expect("write");
        let summary = SummaryReport {
            started_utc: "2026-10-04T12:00:00Z".into(),
            exe: "sim_bridge".into(),
            minutes: 2.0,
            sessions_count: 2,
            sessions: vec![clean_session(), session],
            passed: false,
        };
        let summary_path = dir.join("summary.json");
        std::fs::write(
            &summary_path,
            serde_json::to_string_pretty(&summary).expect("serialise summary"),
        )
        .expect("write");

        let b = session_budgets();
        let clean_sessions = load_sessions(&clean).expect("parse");
        assert_eq!(clean_sessions.len(), 1);
        assert!(b.check(&clean_sessions[0].measured).is_empty());
        let breached_sessions = load_sessions(&breached).expect("parse");
        assert_eq!(breached_sessions[0].violations.len(), 1);
        assert_eq!(breached_sessions[0].violations[0].metric, "frame_p99_ms");
        let summary_sessions = load_sessions(&summary_path).expect("parse");
        assert_eq!(summary_sessions.len(), 2, "a summary unwraps into sessions");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn session_budgets() -> Budgets {
        Budgets {
            frame_p99_ms: 6.9,
            frame_max_ms: 50.0,
            warmup_secs: 10.0,
            rss_mb: 1536.0,
            start_secs: 3.0,
            errors: 0,
            warns: 0,
            crashes: 0,
            check_frames: true,
        }
    }

    fn clean_session() -> SessionReport {
        use crate::soak::budgets::Measured;
        use crate::soak::report::{AppExitKind, ConnectInfo, CrashInfo, FrameStats, InputStats, LogCounts};
        let measured = Measured {
            frame_p99_ms: Some(6.9),
            frame_max_ms: Some(50.0),
            rss_max_mb: Some(900.0),
            start_secs: Some(1.0),
            errors: Some(0),
            warns: Some(0),
            crashes: 0,
        };
        SessionReport {
            session: 1,
            exe: "sim_bridge".into(),
            app_args: vec![],
            windowed: true,
            started_utc: "2026-10-04T12:00:00Z".into(),
            duration_secs: 120.0,
            warmup_secs: 10.0,
            sample_hz: 2.0,
            samples: 240,
            connect: Some(ConnectInfo {
                start_secs: 1.0,
                protocol: "0.2".into(),
                app: "sim_bridge".into(),
                os: "linux".into(),
                pid: 42,
                methods: 50,
            }),
            frames: FrameStats {
                frames_first: Some(10),
                frames_last: Some(14_500),
                avg_ms: None,
                frame_ms: None,
                rss_max_mb: Some(900.0),
                rss_first_mb: Some(800.0),
                rss_last_mb: Some(900.0),
                gpu_memory_mb: None,
                entity_count: Some([3, 30, 12]),
                draw_calls_max: Some(200),
                allocated_bytes_max: None,
            },
            logs: LogCounts {
                errors: 0,
                warns: 0,
                records_read: 40,
            },
            input: InputStats {
                entries: 4,
                injected: 4,
                errors: 0,
            },
            protocol_errors: 0,
            crash: CrashInfo {
                crashed: false,
                exit: AppExitKind::KilledByTool,
                at_secs: None,
                exit_code: None,
                panic_text: None,
            },
            measured,
            violations: vec![],
            passed: true,
        }
    }
}
