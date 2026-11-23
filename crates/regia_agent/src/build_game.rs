// SPDX-License-Identifier: MPL-2.0
//! `agent.build_game` (`docs/specs/23-agent-native.md` § 3.6, ADR-5707): builds the project with
//! `cargo build --message-format=json` on a worker thread and streams the compiler diagnostics
//! through the job it answers (`build:<n>`, polled with `agent.job_status`).
//!
//! The build compiles project code with the app's privileges, so the method needs the `build`
//! permission (specs/23 § 5), like `agent.run_tests`. Names that reach the command line are
//! checked against a narrow alphabet (`run_tests::check_name_as`): no spaces, no leading `-`,
//! so a request cannot add flags. `offline` (default true) keeps cargo off the network.
//!
//! The output parser ([`BuildTally`]) reads cargo's JSON message format (the public
//! `--message-format=json` interface, docs/provenance.md): `compiler-message` objects become
//! structured diagnostics (`file`, `line`, `column`, `message`, `code`, `suggestion`),
//! `compiler-artifact` names what built, `build-finished` carries the overall success. Lines
//! that are not cargo JSON are kept as a bounded raw tail, so a wrapper script's own output is
//! still visible when a build fails oddly. It is fuzzed (`agent.build_game_output`).

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::jobs::{JobRegistry, reason_error};
use crate::protocol::{INVALID_PARAMS, RpcError};
use crate::run_tests::{Run, Tally, check_name_as, host_command, spawn_run};

/// Most diagnostics kept (the rest are counted only).
pub const MAX_DIAGNOSTICS: usize = 256;

/// Most artifact names kept.
pub const MAX_ARTIFACTS: usize = 256;

/// Most non-JSON output lines kept in the raw tail.
pub const MAX_RAW_LINES: usize = 64;

/// Longest message, file or suggestion text kept, in bytes; longer text is cut on a char
/// boundary.
pub const MAX_MESSAGE_BYTES: usize = 4 * 1024;

/// The name of a build's worker thread (its output readers add `-io`).
pub const THREAD_NAME: &str = "regia-build-game";

/// One structured compiler diagnostic of a `cargo build` run (spec 23 § 3.6: cargo JSON with
/// file, line, message and suggestion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildDiagnostic {
    /// `error`, `warning` or another compiler level.
    pub level: String,
    /// The compiler message.
    pub message: String,
    /// The compiler error code when one applies (e.g. `E0308`).
    pub code: Option<String>,
    /// Source path of the primary span, when cargo reported one.
    pub file: Option<String>,
    /// 1-based line of the primary span, when cargo reported one.
    pub line: Option<u32>,
    /// 1-based column of the primary span, when cargo reported one.
    pub column: Option<u32>,
    /// The suggested replacement of the first help span, when the diagnostic offers one.
    pub suggestion: Option<String>,
}

impl BuildDiagnostic {
    /// The event streamed for this diagnostic.
    fn event(&self) -> Value {
        json!({
            "kind": "diagnostic",
            "level": self.level,
            "message": self.message,
            "code": self.code,
            "file": self.file,
            "line": self.line,
            "column": self.column,
            "suggestion": self.suggestion,
        })
    }
}

/// The running totals and structured diagnostics of one build, fed line by line.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BuildTally {
    /// Structured diagnostics, in the order cargo reported them (capped at [`MAX_DIAGNOSTICS`]).
    pub diagnostics: Vec<BuildDiagnostic>,
    /// Names of the artifacts cargo finished (capped at [`MAX_ARTIFACTS`]).
    pub artifacts: Vec<String>,
    /// The `build-finished` success flag, when cargo reached the end.
    pub finished_ok: Option<bool>,
    /// Non-JSON output lines (a wrapper script's own output; capped at [`MAX_RAW_LINES`]).
    pub raw_tail: Vec<String>,
    /// Diagnostics dropped over [`MAX_DIAGNOSTICS`] (the rest are counted only).
    pub dropped_diagnostics: u64,
    /// `error` diagnostics cargo reported, the dropped ones included.
    pub errors: u64,
    /// `warning` diagnostics cargo reported, the dropped ones included.
    pub warnings: u64,
}

impl BuildTally {
    /// Feeds one line of `--message-format=json` output; answers the event to stream, if any.
    pub fn feed(&mut self, line: &str) -> Option<Value> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            if self.raw_tail.len() < MAX_RAW_LINES {
                self.raw_tail.push(trimmed.chars().take(512).collect());
            }
            return None;
        };
        match value.get("reason").and_then(Value::as_str) {
            Some("compiler-message") => {
                let diagnostic = parse_diagnostic(&value)?;
                let event = diagnostic.event();
                match diagnostic.level.as_str() {
                    "error" => self.errors += 1,
                    "warning" => self.warnings += 1,
                    _ => {}
                }
                if self.diagnostics.len() < MAX_DIAGNOSTICS {
                    self.diagnostics.push(diagnostic);
                } else {
                    self.dropped_diagnostics += 1;
                }
                Some(event)
            }
            Some("compiler-artifact") => {
                let name = value
                    .get("target")
                    .and_then(|t| t.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if self.artifacts.len() < MAX_ARTIFACTS {
                    self.artifacts.push(name.clone());
                }
                Some(json!({ "kind": "artifact", "name": name }))
            }
            Some("build-finished") => {
                self.finished_ok = value.get("success").and_then(Value::as_bool);
                Some(json!({ "kind": "build-finished", "success": self.finished_ok }))
            }
            _ => None,
        }
    }

    /// The job's result once the process ended with `exit_code`.
    #[must_use]
    pub fn result(&self, exit_code: Option<i32>, profile: &str, command: &[String]) -> Value {
        json!({
            "ok": exit_code == Some(0) && self.finished_ok != Some(false),
            "exit_code": exit_code,
            "profile": profile,
            "diagnostics": self.diagnostics.iter().map(BuildDiagnostic::event).collect::<Vec<_>>(),
            "error_count": self.errors,
            "warning_count": self.warnings,
            "artifacts": self.artifacts,
            "raw_tail": self.raw_tail,
            "dropped_diagnostics": self.dropped_diagnostics,
            "command": command,
        })
    }
}

impl Tally for BuildTally {
    fn feed(&mut self, line: &str) -> Vec<Value> {
        self.feed(line).into_iter().collect()
    }

    fn progress(&self, event: &Value) -> Option<String> {
        if event["kind"] == "artifact" {
            Some(format!("built {}", event["name"].as_str().unwrap_or_default()))
        } else {
            None
        }
    }

    fn result(&mut self, exit_code: Option<i32>, _killed: bool, command: &[String]) -> Value {
        // `profile` is filled by the handler's finish closure; the tally's own result is
        // merged into it there.
        let mut result = BuildTally::result(self, exit_code, "", command);
        if let Some(object) = result.as_object_mut() {
            object.remove("profile");
        }
        result
    }
}

/// Cuts `text` to [`MAX_MESSAGE_BYTES`] on a char boundary.
fn cap_text(text: &str) -> String {
    if text.len() <= MAX_MESSAGE_BYTES {
        return text.to_string();
    }
    let mut cut = MAX_MESSAGE_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text[..cut].to_string()
}

/// Extracts one [`BuildDiagnostic`] from a `compiler-message` JSON object (cargo's public
/// message format; the primary span carries the location, the first help span the suggestion).
fn parse_diagnostic(message: &Value) -> Option<BuildDiagnostic> {
    let inner = message.get("message")?;
    let level = inner.get("level").and_then(Value::as_str)?.to_string();
    let text = inner.get("message").and_then(Value::as_str)?;
    let code = inner
        .get("code")
        .and_then(|c| c.get("code"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let primary = inner
        .get("spans")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|span| span.get("is_primary").and_then(Value::as_bool) == Some(true));
    let file = primary
        .and_then(|span| span.get("file_name"))
        .and_then(Value::as_str)
        .map(cap_text);
    let line = primary
        .and_then(|span| span.get("line_start"))
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    let column = primary
        .and_then(|span| span.get("column_start"))
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    // rustc attaches machine-applicable fixes as children whose spans carry a
    // `suggested_replacement`; the first one is the diagnostic's suggestion.
    let suggestion = inner
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|child| {
            child
                .get("spans")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|span| span.get("suggested_replacement").and_then(Value::as_str))
                .map(cap_text)
                .collect::<Vec<_>>()
        })
        .next();
    Some(BuildDiagnostic {
        level,
        message: cap_text(text),
        code,
        file,
        line,
        column,
        suggestion,
    })
}

/// The `cargo build` arguments for a request (without the program).
///
/// # Errors
/// `build_game_bad_name` for a profile or package outside the allowed alphabet.
pub fn command_args(params: &Value) -> Result<(Vec<String>, String), RpcError> {
    let profile = crate::params::opt_str(params, "profile")?.unwrap_or("debug");
    check_name_as("profile", profile, "build_game_bad_name")?;
    let mut args = vec!["build".to_string(), "--message-format=json".to_string()];
    match profile {
        "debug" | "dev" => {}
        "release" => args.push("--release".to_string()),
        custom => {
            args.push("--profile".to_string());
            args.push(custom.to_string());
        }
    }
    if let Some(package) = crate::params::opt_str(params, "package")? {
        check_name_as("package", package, "build_game_bad_name")?;
        args.extend(["--package".to_string(), package.to_string()]);
    }
    if crate::params::opt_bool(params, "offline")?.unwrap_or(true) {
        args.push("--offline".to_string());
    }
    Ok((args, profile.to_string()))
}

/// `agent.build_game`: starts the build and answers its job id (or, with `dry_run`, the command).
pub(crate) fn build_game(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (args, profile) = command_args(params)?;
    let root = crate::req_path::project_root(world);
    if !root.join("Cargo.toml").is_file() {
        return Err(reason_error(
            INVALID_PARAMS,
            "F0002",
            "build_game_no_manifest",
            "the project root has no Cargo.toml, so there is nothing to build",
        ));
    }
    let (program, env) = host_command(world);
    let mut command_line = vec![program.display().to_string()];
    command_line.extend(args.iter().cloned());
    if crate::params::opt_bool(params, "dry_run")?.unwrap_or(false) {
        return Ok(json!({ "dry_run": true, "command": command_line, "cwd": root.display().to_string() }));
    }
    let mut registry = world.get_resource_or_insert_with::<JobRegistry>(Default::default);
    if let Some(running) = registry.running("build") {
        return Err(reason_error(
            INVALID_PARAMS,
            "F0002",
            "build_game_busy",
            format!(
                "{running} is still running; poll it with agent.job_status or stop it with agent.job_cancel"
            ),
        ));
    }
    let (job, handle) = registry.start("build", format!("{profile} build"), true);
    let run = Run {
        program: program.clone(),
        args,
        root,
        env,
        command_line: command_line.clone(),
        start_message: "compiling",
        thread_name: THREAD_NAME,
        tally: Box::new(BuildTally::default()),
        finish: Box::new(move |mut result| {
            if let Some(object) = result.as_object_mut() {
                object.insert("profile".into(), json!(profile));
            }
            result
        }),
    };
    if let Err(error) = spawn_run(run, handle.clone()) {
        handle.fail(
            "build_game_spawn_failed",
            format!("cannot start {}: {error}", program.display()),
        );
    }
    Ok(json!({ "job": job, "command": command_line }))
}

/// The fixture of the `agent.build_game_output` fuzz target: one error with a code, a primary
/// span and a suggestion, one warning, an artifact and a failed finish.
pub const FUZZ_FIXTURE: &str = "\
{\"reason\":\"compiler-message\",\"message\":{\"message\":\"mismatched types\",\"level\":\"error\",\"code\":{\"code\":\"E0308\"},\"spans\":[{\"file_name\":\"src/lib.rs\",\"line_start\":3,\"column_start\":9,\"is_primary\":true}],\"children\":[{\"message\":\"try this\",\"spans\":[{\"suggested_replacement\":\"1u32\"}]}]}}
{\"reason\":\"compiler-message\",\"message\":{\"message\":\"unused variable\",\"level\":\"warning\",\"spans\":[{\"file_name\":\"src/lib.rs\",\"line_start\":2,\"column_start\":5,\"is_primary\":true}]}}
{\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"demo\"}}
{\"reason\":\"build-finished\",\"success\":false}
";

/// Feeds every line to the tally (the target is cut-tolerant: a run cut short is valid input).
/// Invariants: error/warning counts never disagree with the levels, caps hold.
///
/// # Errors
/// `BUG_PREFIX`-prefixed messages on a broken invariant (the fuzz harness reports them).
pub fn fuzz_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use crate::fuzz_targets::BUG_PREFIX;
    let text = String::from_utf8_lossy(bytes);
    if text.trim().is_empty() {
        return Err("no cargo build output".into());
    }
    let mut tally = BuildTally::default();
    for line in text.lines() {
        tally.feed(line);
    }
    let errors = tally.diagnostics.iter().filter(|d| d.level == "error").count();
    if errors > tally.diagnostics.len() {
        return Err(format!("{BUG_PREFIX}more errors than diagnostics"));
    }
    if tally.diagnostics.len() > MAX_DIAGNOSTICS {
        return Err(format!("{BUG_PREFIX}the diagnostics cap was exceeded"));
    }
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dry_run reply's `cwd` is the real project root, not "." (the command has no
    /// `--project` flag; running it in the caller's directory would build the wrong project).
    #[test]
    fn dry_run_answers_the_real_project_root_as_cwd() {
        let root = regia_test_utils::unique_temp_dir("build_game_dry_run_cwd");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"dry\"\n").expect("Cargo.toml");
        let mut world = World::new();
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(root.clone()),
        });
        let answer = build_game(&mut world, &json!({ "dry_run": true })).expect("dry run");
        assert_eq!(answer["dry_run"], json!(true));
        assert_eq!(answer["cwd"], json!(root.display().to_string()), "{answer}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn cargo_json_becomes_structured_diagnostics() {
        let mut tally = BuildTally::default();
        let events: Vec<Value> = FUZZ_FIXTURE.lines().filter_map(|l| tally.feed(l)).collect();
        assert_eq!(tally.diagnostics.len(), 2);
        assert_eq!(tally.artifacts, vec!["demo"]);
        assert_eq!(tally.finished_ok, Some(false));
        assert_eq!(tally.raw_tail, Vec::<String>::new());
        let error = &tally.diagnostics[0];
        assert_eq!(error.level, "error");
        assert_eq!(error.message, "mismatched types");
        assert_eq!(error.code.as_deref(), Some("E0308"));
        assert_eq!(error.file.as_deref(), Some("src/lib.rs"));
        assert_eq!(error.line, Some(3));
        assert_eq!(error.column, Some(9));
        assert_eq!(error.suggestion.as_deref(), Some("1u32"));
        let warning = &tally.diagnostics[1];
        assert_eq!(warning.level, "warning");
        assert_eq!(warning.suggestion, None);
        let kinds: Vec<&str> = events.iter().filter_map(|e| e["kind"].as_str()).collect();
        assert_eq!(kinds, ["diagnostic", "diagnostic", "artifact", "build-finished"]);
        let result = tally.result(Some(101), "debug", &["cargo".into()]);
        assert_eq!(result["ok"], false);
        assert_eq!(result["exit_code"], 101);
        assert_eq!(result["profile"], "debug");
        assert_eq!(result["error_count"], 1);
        assert_eq!(result["warning_count"], 1);
        assert_eq!(result["diagnostics"][0]["suggestion"], "1u32");
    }

    #[test]
    fn non_json_lines_are_kept_as_a_bounded_raw_tail() {
        let mut tally = BuildTally::default();
        for _ in 0..(MAX_RAW_LINES + 10) {
            tally.feed("a wrapper script says hello");
        }
        tally.feed("{\"reason\":\"build-finished\",\"success\":true}");
        assert_eq!(tally.raw_tail.len(), MAX_RAW_LINES);
        assert_eq!(tally.finished_ok, Some(true));
        let result = tally.result(Some(0), "release", &[]);
        assert_eq!(result["ok"], true);
    }

    #[test]
    fn diagnostics_are_capped_and_counted() {
        let mut tally = BuildTally::default();
        for _ in 0..(MAX_DIAGNOSTICS + 5) {
            tally.feed(
                "{\"reason\":\"compiler-message\",\"message\":{\"message\":\"x\",\"level\":\"error\",\"spans\":[{\"file_name\":\"a.rs\",\"line_start\":1,\"column_start\":1,\"is_primary\":true}]}}",
            );
        }
        assert_eq!(tally.diagnostics.len(), MAX_DIAGNOSTICS);
        assert_eq!(tally.dropped_diagnostics, 5);
        // The counts cover every diagnostic cargo reported, the dropped ones too (review rk9
        // finding 7): an agent reading `error_count` sees the real total.
        assert_eq!(
            tally.result(Some(1), "debug", &[])["error_count"],
            MAX_DIAGNOSTICS + 5
        );
        tally.feed(
            "{\"reason\":\"compiler-message\",\"message\":{\"message\":\"w\",\"level\":\"warning\",\"spans\":[]}}",
        );
        let result = tally.result(Some(1), "debug", &[]);
        assert_eq!(result["warning_count"], 1, "a dropped warning is counted");
        assert_eq!(result["dropped_diagnostics"], 6);
    }

    /// `suggestion` and `file` are capped like `message`: one huge `suggested_replacement` or
    /// `file_name` does not make an event (or the result) unbounded (review rk9 finding 7).
    #[test]
    fn huge_suggestion_and_file_are_capped() {
        let huge = "é".repeat(MAX_MESSAGE_BYTES);
        let line = json!({
            "reason": "compiler-message",
            "message": {
                "message": "m",
                "level": "error",
                "spans": [{ "file_name": huge, "line_start": 1, "column_start": 1, "is_primary": true }],
                "children": [{ "message": "try", "spans": [{ "suggested_replacement": huge }] }],
            },
        })
        .to_string();
        let mut tally = BuildTally::default();
        let event = tally.feed(&line).expect("a diagnostic event");
        let suggestion = event["suggestion"].as_str().expect("suggestion");
        let file = event["file"].as_str().expect("file");
        assert!(
            suggestion.len() <= MAX_MESSAGE_BYTES,
            "suggestion {} bytes",
            suggestion.len()
        );
        assert!(file.len() <= MAX_MESSAGE_BYTES, "file {} bytes", file.len());
        assert!(
            suggestion.starts_with('é') && file.starts_with('é'),
            "cut on a char boundary"
        );
    }

    #[test]
    fn a_missing_primary_span_still_reports_the_message() {
        let mut tally = BuildTally::default();
        tally.feed(
            "{\"reason\":\"compiler-message\",\"message\":{\"message\":\"cannot find macro\",\"level\":\"error\"}}",
        );
        let diagnostic = &tally.diagnostics[0];
        assert_eq!(diagnostic.file, None);
        assert_eq!(diagnostic.line, None);
        assert_eq!(diagnostic.column, None);
        assert_eq!(diagnostic.message, "cannot find macro");
    }

    #[test]
    fn names_that_could_add_flags_are_refused_and_args_shape_up() {
        let (args, profile) =
            command_args(&json!({ "profile": "release", "package": "game" })).expect("release args");
        assert_eq!(profile, "release");
        assert_eq!(
            args,
            [
                "build",
                "--message-format=json",
                "--release",
                "--package",
                "game",
                "--offline"
            ]
        );
        let (args, profile) = command_args(&json!({})).expect("default args");
        assert_eq!(profile, "debug");
        assert_eq!(args, ["build", "--message-format=json", "--offline"]);
        let (args, profile) =
            command_args(&json!({ "profile": "dev-opt", "offline": false })).expect("custom profile");
        assert_eq!(profile, "dev-opt");
        assert_eq!(args, ["build", "--message-format=json", "--profile", "dev-opt"]);
        for bad in ["", "-x", "--offline", "a b", "a;b", "a/b", "ü"] {
            assert!(
                command_args(&json!({ "profile": bad })).is_err(),
                "profile {bad:?}"
            );
            assert!(
                command_args(&json!({ "package": bad })).is_err(),
                "package {bad:?}"
            );
        }
    }

    /// The tally takes arbitrary text (fuzz target `agent.build_game_output`).
    #[test]
    fn mutated_output_never_panics_and_caps_hold() {
        let mut rng = regia_core::fuzz::FuzzRng::new(0x5707_0001);
        for _ in 0..2000 {
            let text = regia_core::fuzz::mutate_text(FUZZ_FIXTURE, &mut rng);
            let mut tally = BuildTally::default();
            for line in text.lines() {
                tally.feed(line);
            }
            assert!(tally.diagnostics.len() <= MAX_DIAGNOSTICS);
            assert!(tally.raw_tail.len() <= MAX_RAW_LINES);
            assert!(tally.artifacts.len() <= MAX_ARTIFACTS);
            assert!(
                tally
                    .diagnostics
                    .iter()
                    .all(|d| d.message.len() <= MAX_MESSAGE_BYTES)
            );
        }
    }
}
