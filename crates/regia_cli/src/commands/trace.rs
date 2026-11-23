// SPDX-License-Identifier: MPL-2.0
//! `regia trace export <file.regiatrace> --format jsonl|trace-event` (spec 54 § 4.3, book
//! `agents/gpu-breadcrumbs-and-trace.md`): reads a trace with `regia_telemetry::trace::read_trace`
//! and writes JSON lines (names resolved, for the planned editor profiler) or the public Trace
//! Event Format (`traceEvents`, for any generic viewer).

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use regia_telemetry::trace::{export, read_trace};

/// `regia trace` subcommands.
#[derive(Debug, Subcommand)]
pub enum TraceCommand {
    /// Convert a `.regiatrace` file to JSON lines or the Trace Event Format.
    Export(ExportArgs),
}

/// Output formats of `regia trace export`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ExportFormat {
    /// One JSON object per event, with a header line first.
    Jsonl,
    /// The public Trace Event Format: marks as instants, ranges as `B`/`E`, GPU intervals as `X`.
    TraceEvent,
}

/// `regia trace export`.
#[derive(Debug, Args)]
pub struct ExportArgs {
    /// The `.regiatrace` file.
    pub file: PathBuf,
    /// Output format.
    #[arg(long, value_enum, default_value = "jsonl")]
    pub format: ExportFormat,
    /// Write here instead of standard output.
    #[arg(short, long)]
    pub out: Option<PathBuf>,
}

/// Runs a `regia trace` subcommand.
pub fn run(command: &TraceCommand) -> Result<()> {
    match command {
        TraceCommand::Export(args) => export_file(args),
    }
}

fn export_file(args: &ExportArgs) -> Result<()> {
    let trace = read_trace(&args.file).with_context(|| format!("reading {}", args.file.display()))?;
    if trace.truncated {
        eprintln!(
            "warning: {} ends inside a chunk; exporting the {} events before it",
            args.file.display(),
            trace.events.len()
        );
    }
    let mut buf = Vec::new();
    match args.format {
        ExportFormat::Jsonl => export::export_jsonl(&trace, &mut buf)?,
        ExportFormat::TraceEvent => export::export_trace_event(&trace, &mut buf)?,
    }
    match &args.out {
        Some(path) => std::fs::write(path, &buf).with_context(|| format!("writing {}", path.display()))?,
        None => std::io::stdout().write_all(&buf)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_telemetry::trace::{Recorder, TraceConfig};

    fn trace_file() -> (PathBuf, PathBuf) {
        let dir = regia_test_utils::unique_temp_dir("cli-trace");
        let r = Recorder::new(TraceConfig {
            window_seconds: 5.0,
            dir: dir.clone(),
            ..TraceConfig::default()
        });
        r.start(Some("t"), None).expect("start");
        r.mark("hello", None);
        let id = r.open_range("work", regia_telemetry::trace::Lane::Main).id;
        let _ = r.close_range(id);
        r.stop().expect("stop");
        (dir.join("t.regiatrace"), dir)
    }

    #[test]
    fn exports_both_formats_to_a_file() {
        let (file, dir) = trace_file();
        let jsonl = dir.join("out.jsonl");
        run(&TraceCommand::Export(ExportArgs {
            file: file.clone(),
            format: ExportFormat::Jsonl,
            out: Some(jsonl.clone()),
        }))
        .expect("jsonl");
        let text = std::fs::read_to_string(&jsonl).expect("read");
        assert_eq!(text.lines().count(), 1 + 3, "header, mark, open, close");
        let te = dir.join("out.json");
        run(&TraceCommand::Export(ExportArgs {
            file,
            format: ExportFormat::TraceEvent,
            out: Some(te.clone()),
        }))
        .expect("trace event");
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&te).expect("read")).expect("json");
        assert!(doc["traceEvents"].as_array().is_some_and(|a| a.len() >= 3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_panic() {
        let err = run(&TraceCommand::Export(ExportArgs {
            file: PathBuf::from("/definitely/not/here.regiatrace"),
            format: ExportFormat::Jsonl,
            out: None,
        }))
        .expect_err("fails");
        assert!(format!("{err:#}").contains("reading"), "{err:#}");
    }
}
