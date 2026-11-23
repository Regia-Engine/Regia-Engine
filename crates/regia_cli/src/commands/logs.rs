// SPDX-License-Identifier: MPL-2.0
//! Offline tester-upload analysis (`regia logs triage`, ADR-8035).

mod input;

use std::{path::PathBuf, process::ExitCode};

use clap::{Args, Subcommand};
use regia_agent::perf_gate::{CommandOutput, SummaryRequest, TraceRequest, summary_main, trace_main};
use serde_json::{Value, json};

/// Offline log commands.
#[derive(Debug, Subcommand)]
pub enum LogsCommand {
    /// Stitch split uploads and report per-session summaries, event rates and storms.
    Triage(TriageArgs),
    /// Judge session summaries, or a trace run, against a thresholds file.
    PerfGate(PerfGateArgs),
}

/// Settings and inputs of `regia logs perf-gate`.
#[derive(Debug, Args)]
pub struct PerfGateArgs {
    /// Session logs (JSONL or one JSON summary). Omit when `--run` is set.
    pub logs: Vec<PathBuf>,
    /// Thresholds TOML. Required unless `--run` is set.
    #[arg(long)]
    pub thresholds: Option<PathBuf>,
    /// Older summary, or a trace baseline JSON file.
    #[arg(long)]
    pub baseline: Option<PathBuf>,
    /// Resolution class override (`1080p`, `1440p`, `ultrawide`).
    #[arg(long)]
    pub res: Option<String>,
    /// Tier override (`LOW`, `HIGH`, `ULTRA`).
    #[arg(long)]
    pub tier: Option<String>,
    /// Allowed p50 regression, in percent. p99 may move twice this far.
    #[arg(long, default_value_t = 10.0)]
    pub regression_pct: f64,
    /// A visible frame at or above this many milliseconds is a hitch.
    #[arg(long, default_value_t = 25.0)]
    pub hitch_ms: f64,
    /// Print one JSON document instead of the text table.
    #[arg(long)]
    pub json: bool,
    /// Directory of `frames-` / `feel-` / `mem-` captures. Replaces the log list.
    #[arg(long)]
    pub run: Option<PathBuf>,
    /// Rewrite `--baseline` from this run and exit 0. Requires `--run`.
    #[arg(long)]
    pub update_baseline: bool,
}

/// Settings and inputs of `regia logs triage`.
#[derive(Debug, Args)]
pub struct TriageArgs {
    /// JSONL files, extracted bundle directories, ZIP files, or multipart ZIP pieces.
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
    /// Publish scrubbed stitched files and report.json into a NEW directory; refuses an existing path.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Print one JSON result (or structured error) instead of a text summary.
    #[arg(long)]
    pub json: bool,
    /// Event-rate bucket width, in seconds (at least 0.001).
    #[arg(long, default_value_t = 60.0)]
    pub bucket_seconds: f64,
    /// Report silence longer than this many seconds.
    #[arg(long, default_value_t = 60.0)]
    pub gap_seconds: f64,
    /// Storm threshold relative to the median bucket count, including empty buckets.
    #[arg(long, default_value_t = 10.0)]
    pub storm_factor: f64,
    /// Minimum storm size when the median bucket count is zero.
    #[arg(long, default_value_t = 5)]
    pub storm_min_count: u64,
    /// Maximum message templates per session.
    #[arg(long, default_value_t = 15)]
    pub top: usize,
    /// Combined input read budget in bytes, including compressed archive bytes and expanded text.
    #[arg(long, default_value_t = 64 * 1024 * 1024)]
    pub max_bytes: usize,
    /// Maximum visited directory entries and input files, including manifests,
    /// irrelevant files inside an extracted bundle, and ZIP directory entries.
    #[arg(long, default_value_t = 4096)]
    pub max_files: usize,
    /// Match frame-summary p50 threshold for the frame-rate verdict, in milliseconds.
    #[arg(long, default_value_t = 50.0)]
    pub slow_frame_ms: f64,
    /// RTT p95 threshold for the network verdict, in milliseconds.
    #[arg(long, default_value_t = 200.0)]
    pub slow_rtt_ms: f64,
    /// Loss p95 threshold for the network verdict, in percent.
    #[arg(long, default_value_t = 5.0)]
    pub loss_percent: f64,
}

#[derive(Debug)]
struct Failure {
    code: String,
    message: String,
}

type Result<T> = std::result::Result<T, Failure>;

fn fail(code: &str, message: impl Into<String>) -> Failure {
    Failure {
        code: code.to_owned(),
        message: message.into(),
    }
}

fn io_error(_: std::io::Error) -> Failure {
    fail(
        "logs_triage_io",
        "could not read or publish log files; check paths, permissions and free space",
    )
}

fn analyze(params: &Value) -> Result<Value> {
    regia_agent::log_triage::triage(params).map_err(|e| {
        let code = e
            .data
            .as_ref()
            .and_then(|v| v["code"].as_str())
            .unwrap_or("logs_triage_input");
        fail(code, e.message)
    })
}

fn execute(args: &TriageArgs) -> Result<Value> {
    let settings = json!({"bucket_seconds":args.bucket_seconds,"gap_seconds":args.gap_seconds,
        "storm_factor":args.storm_factor,"storm_min_count":args.storm_min_count,"top":args.top,
        "max_bytes":args.max_bytes,"max_files":args.max_files,"slow_frame_ms":args.slow_frame_ms,
        "slow_rtt_ms":args.slow_rtt_ms,"loss_percent":args.loss_percent});
    // Validate settings before touching inputs. An empty input is the expected result here.
    if let Err(error) = analyze(&json!({"bundles":[], "settings":settings}))
        && error.code != "logs_triage_empty"
    {
        return Err(error);
    }
    let bundles = input::load(&args.paths, args.max_bytes, args.max_files)?;
    let mut report =
        analyze(&json!({"bundles":bundles,"settings":settings,"include_stitched":args.out.is_some()}))?;
    if let Some(out) = &args.out {
        input::publish(out, &report)?;
    }
    if let Some(files) = report["files"].as_array_mut() {
        for file in files {
            if let Some(file) = file.as_object_mut() {
                file.remove("text");
            }
        }
    }
    report["ok"] = json!(true);
    Ok(report)
}

fn read_text(path: &std::path::Path) -> std::result::Result<String, CommandOutput> {
    std::fs::read_to_string(path).map_err(|_| bad("could not read an input file"))
}

fn bad(message: &str) -> CommandOutput {
    CommandOutput {
        code: 2,
        stdout: String::new(),
        stderr: format!("perf_gate: error: {message}\n"),
    }
}

fn execute_perf(args: &PerfGateArgs) -> CommandOutput {
    if args.update_baseline && args.run.is_none() {
        return bad("update-baseline requires --run");
    }
    if args.run.is_some() && !args.logs.is_empty() {
        return bad("pass logs or --run, not both");
    }
    if let Some(run) = &args.run {
        if args.update_baseline && args.baseline.is_none() {
            return bad("update-baseline requires --baseline");
        }
        let baseline_path = args.baseline.clone().unwrap_or_default();
        return trace_main(&TraceRequest {
            run_dir: run.clone(),
            baseline_path,
            update_baseline: args.update_baseline,
            regression_pct: args.regression_pct,
            hitch_ms: args.hitch_ms,
            json: args.json,
        });
    }
    let Some(thresholds_path) = &args.thresholds else {
        return bad("summary mode requires --thresholds");
    };
    if args.logs.is_empty() {
        return bad("summary mode requires at least one log");
    }
    let Ok(thresholds) = read_text(thresholds_path) else {
        return bad("could not read the thresholds file");
    };
    let mut logs = Vec::new();
    for path in &args.logs {
        match read_text(path) {
            Ok(text) => logs.push(text),
            Err(output) => return output,
        }
    }
    let baseline = match &args.baseline {
        None => None,
        Some(path) => match read_text(path) {
            Ok(text) => Some(text),
            Err(output) => return output,
        },
    };
    summary_main(&SummaryRequest {
        logs,
        thresholds,
        baseline,
        res: args.res.clone(),
        tier: args.tier.clone(),
        regression_pct: args.regression_pct,
        hitch_ms: args.hitch_ms,
        json: args.json,
    })
}

/// Runs the command.
///
/// Triage exit 0 means analysis completed, even when it found an unhealthy session.
/// Perf-gate exit 0 is a pass, 1 is a failed gate, and 2 is bad input.
pub fn run(command: &LogsCommand) -> ExitCode {
    match command {
        LogsCommand::Triage(args) => run_triage(args),
        LogsCommand::PerfGate(args) => {
            let output = execute_perf(args);
            if !output.stdout.is_empty() {
                print!("{}", output.stdout);
            }
            if !output.stderr.is_empty() {
                eprint!("{}", regia_core::privacy::scrub_report(&output.stderr));
            }
            let code = match output.code {
                0 => 0,
                1 => 1,
                _ => 2,
            };
            ExitCode::from(code)
        }
    }
}

fn run_triage(args: &TriageArgs) -> ExitCode {
    match execute(args) {
        Ok(report) => {
            if args.json {
                outln!("{report}");
            } else if let Some(sessions) = report["sessions"].as_array() {
                for session in sessions {
                    outln!(
                        "{}: {} records, {} malformed, {} source gaps; exit {}; {}; {} storm(s)",
                        session["session"].as_str().unwrap_or("unknown"),
                        session["records"],
                        session["malformed_lines"],
                        session["source_gaps"],
                        session["exit"].as_str().unwrap_or("unknown"),
                        session["verdict"].as_str().unwrap_or("unknown"),
                        session["rates"]["storms"].as_array().map_or(0, Vec::len)
                    );
                }
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            let message = regia_core::privacy::scrub_report(&error.message);
            if args.json {
                outln!(
                    "{}",
                    json!({"ok":false,"error":{"code":error.code,"message":message}})
                );
            } else {
                eprintln!("{}: {message}", error.code);
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests;
