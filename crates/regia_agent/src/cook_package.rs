// SPDX-License-Identifier: MPL-2.0
//! `agent.cook` and `agent.package` (`docs/specs/23-agent-native.md` § 3.7, ADR-7217): the
//! pipeline methods `cook(platform, quality)` and `package(platform, profile)` as jobs
//! (`cook:<n>`, `package:<n>`, polled with `agent.job_status`), on the same worker pattern as
//! `agent.build_game` (`run_tests::spawn_run`).
//!
//! Each job runs the `regia` command line as a child process in the project root:
//! `regia cook --platform P --quality Q --json` or `regia package --platform P --profile R --json`.
//! The child's human output (stderr) becomes the job's progress messages and `line` events; its
//! one `--json` result object (stdout, `docs/specs/21-tooling-and-packaging.md` § 1) becomes the
//! job's result, with the exit code and the command line. Lines are fed through [`CliTally`]
//! (fuzzed as `agent.cook_package_output`).
//!
//! Desktop only: on Android the methods answer `F0013` with reason `cook_desktop_only` (the
//! command line and the asset pipeline are not on the device). Both need the `build` permission,
//! like `agent.build_game` (they run project build scripts). `package` also needs
//! `confirm: true` (spec 23 § 3.7 "confirmation"), else it answers `needs_confirmation`.
//! A platform outside [`PLATFORMS`] answers `cook_bad_platform`; a quality outside
//! [`QUALITIES`] `cook_bad_quality`; a profile outside the name alphabet `package_bad_profile`.

use std::path::PathBuf;

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::jobs::{JobRegistry, reason_error};
use crate::protocol::{INVALID_PARAMS, RpcError, UNSUPPORTED};
use crate::run_tests::{Run, Tally, check_name_as, spawn_run};

/// The platforms `regia cook` and `regia package` build for.
pub const PLATFORMS: &[&str] = &["linux", "windows", "android"];

/// The cook qualities (`regia cook --quality`).
pub const QUALITIES: &[&str] = &["fast", "final"];

/// The worker thread name of a cook job (its output readers add `-io`).
pub const COOK_THREAD_NAME: &str = "regia-agent-cook";

/// The worker thread name of a package job.
pub const PACKAGE_THREAD_NAME: &str = "regia-agent-package";

/// Most output lines kept in a result's `tail`.
pub const MAX_TAIL_LINES: usize = 64;

/// Longest JSON result text collected, in bytes; a longer object is dropped (the result then
/// has no `report`).
pub const MAX_JSON_BYTES: usize = 1 << 20;

/// The `regia` program to run instead of the default (a host with its own install path; tests).
/// Without it the job runs `$REGIA_CLI`, else `regia` from `PATH`.
#[derive(Resource, Debug, Clone)]
pub struct RegiaCliProgram(pub PathBuf);

/// The `regia` program a job runs.
fn program(world: &World) -> PathBuf {
    world
        .get_resource::<RegiaCliProgram>()
        .map(|p| p.0.clone())
        .or_else(|| std::env::var_os("REGIA_CLI").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("regia"))
}

/// Brace depth of `line` outside JSON strings, added to `depth`; `None` when it goes negative.
fn brace_depth(line: &str, mut depth: i64) -> Option<i64> {
    let mut in_string = false;
    let mut escaped = false;
    for c in line.chars() {
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return None;
        }
    }
    Some(depth)
}

/// The tally of one `regia cook --json` / `regia package --json` run: human lines become
/// progress, the last complete JSON object (one line, or a pretty-printed object starting with a
/// `{` line) becomes the report.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CliTally {
    /// The last JSON object the child printed (its `--json` result).
    pub report: Option<Value>,
    /// The last [`MAX_TAIL_LINES`] human output lines.
    pub tail: Vec<String>,
    /// Human output lines seen (the dropped ones included).
    pub lines: u64,
    /// A JSON object being collected over several lines, with its brace depth.
    pending: Option<(String, i64)>,
}

impl CliTally {
    /// Feeds one output line; answers the event to stream, if any.
    pub fn feed(&mut self, line: &str) -> Option<Value> {
        let trimmed = line.trim_end();
        if let Some((mut text, depth)) = self.pending.take() {
            text.push('\n');
            text.push_str(trimmed);
            match brace_depth(trimmed, depth) {
                Some(0) => return self.finish_object(&text),
                Some(depth) if text.len() <= MAX_JSON_BYTES => {
                    self.pending = Some((text, depth));
                    return None;
                }
                // Too long or unbalanced: what was collected is human output after all.
                _ => return text.lines().fold(None, |_, part| self.human(part)),
            }
        }
        if trimmed.trim().is_empty() {
            return None;
        }
        if trimmed.starts_with('{') {
            match brace_depth(trimmed, 0) {
                Some(0) => return self.finish_object(trimmed),
                Some(depth) if trimmed.len() <= MAX_JSON_BYTES => {
                    self.pending = Some((trimmed.to_string(), depth));
                    return None;
                }
                _ => {}
            }
        }
        self.human(trimmed)
    }

    /// A complete object's text: the report when it parses as a JSON object.
    fn finish_object(&mut self, text: &str) -> Option<Value> {
        match serde_json::from_str::<Value>(text) {
            Ok(value @ Value::Object(_)) => {
                let ok = value.get("ok").cloned().unwrap_or(Value::Null);
                self.report = Some(value);
                Some(json!({ "kind": "report", "ok": ok }))
            }
            _ => text.lines().fold(None, |_, part| self.human(part)),
        }
    }

    /// One human output line.
    fn human(&mut self, line: &str) -> Option<Value> {
        let line: String = line.chars().take(512).collect();
        self.lines += 1;
        if self.tail.len() == MAX_TAIL_LINES {
            self.tail.remove(0);
        }
        self.tail.push(line.clone());
        Some(json!({ "kind": "line", "text": line }))
    }

    /// The job's result once the process ended with `exit_code`.
    #[must_use]
    pub fn result(&self, exit_code: Option<i32>, killed: bool, command: &[String]) -> Value {
        let report_ok = self
            .report
            .as_ref()
            .and_then(|r| r.get("ok"))
            .and_then(Value::as_bool);
        let artifact = self
            .report
            .as_ref()
            .and_then(|r| r.get("artifact").or_else(|| r.get("pak_dir")))
            .cloned()
            .unwrap_or(Value::Null);
        json!({
            "ok": exit_code == Some(0) && !killed && report_ok != Some(false),
            "exit_code": exit_code,
            "cancelled": killed,
            "artifact": artifact,
            "report": self.report,
            "tail": self.tail,
            "lines": self.lines,
            "command": command,
        })
    }
}

impl Tally for CliTally {
    fn feed(&mut self, line: &str) -> Vec<Value> {
        self.feed(line).into_iter().collect()
    }

    fn progress(&self, event: &Value) -> Option<String> {
        match event["kind"].as_str() {
            Some("line") => event["text"].as_str().map(str::to_string),
            Some("report") => Some("finished; reading the result".to_string()),
            _ => None,
        }
    }

    fn result(&mut self, exit_code: Option<i32>, killed: bool, command: &[String]) -> Value {
        CliTally::result(self, exit_code, killed, command)
    }
}

/// Which pipeline method a call is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineMethod {
    /// `agent.cook`.
    Cook,
    /// `agent.package`.
    Package,
}

impl PipelineMethod {
    /// The job kind (`cook:<n>`, `package:<n>`).
    #[must_use]
    pub fn kind(self) -> &'static str {
        match self {
            Self::Cook => "cook",
            Self::Package => "package",
        }
    }
}

/// `container` values of `agent.package`: the build-container profiles of
/// `regia package --container` (ADR-8835).
pub const CONTAINERS: &[&str] = &["steam-sniper", "debian12"];

/// The `regia` arguments of a request (without the program), with the job's label.
///
/// # Errors
/// `cook_bad_platform`, `cook_bad_quality`, `package_bad_profile` or `package_bad_container`.
pub fn command_args(method: PipelineMethod, params: &Value) -> Result<(Vec<String>, String), RpcError> {
    let platform = match crate::params::opt_str(params, "platform")? {
        Some(platform) => platform,
        None => host_platform(),
    };
    if !PLATFORMS.contains(&platform) {
        return Err(reason_error(
            INVALID_PARAMS,
            "F0002",
            "cook_bad_platform",
            format!("unknown platform {platform:?} (linux, windows or android)"),
        ));
    }
    match method {
        PipelineMethod::Cook => {
            let quality = crate::params::opt_str(params, "quality")?.unwrap_or("final");
            if !QUALITIES.contains(&quality) {
                return Err(reason_error(
                    INVALID_PARAMS,
                    "F0002",
                    "cook_bad_quality",
                    format!("unknown cook quality {quality:?} (fast or final)"),
                ));
            }
            let mut args: Vec<String> = ["cook", "--platform", platform, "--quality", quality, "--json"]
                .map(str::to_string)
                .to_vec();
            if crate::params::opt_bool(params, "clean")?.unwrap_or(false) {
                args.push("--clean".into());
            }
            Ok((args, format!("{quality} cook for {platform}")))
        }
        PipelineMethod::Package => {
            let profile = crate::params::opt_str(params, "profile")?.unwrap_or("shipping");
            check_name_as("profile", profile, "package_bad_profile")?;
            let mut args: Vec<String> = ["package", "--platform", platform, "--profile", profile, "--json"]
                .map(str::to_string)
                .to_vec();
            if let Some(container) = crate::params::opt_str(params, "container")? {
                if !CONTAINERS.contains(&container) {
                    return Err(reason_error(
                        INVALID_PARAMS,
                        "F0002",
                        "package_bad_container",
                        format!("unknown build container {container:?} (steam-sniper or debian12)"),
                    ));
                }
                args.push("--container".into());
                args.push(container.into());
            }
            if let Some(path) = crate::params::opt_str(params, "audit_dir")? {
                args.push(format!("--audit-dir={path}"));
            }
            if let Some(path) = crate::params::opt_str(params, "audit_artifact")? {
                args.push(format!("--audit-artifact={path}"));
            }
            Ok((args, format!("{profile} package for {platform}")))
        }
    }
}

/// The platform tag of the machine the app runs on (the default `platform`).
fn host_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "android") {
        "android"
    } else {
        "linux"
    }
}

/// `agent.cook` / `agent.package`: starts the job and answers its id (or, with `dry_run`, the
/// command).
pub(crate) fn start(world: &mut World, method: PipelineMethod, params: &Value) -> Result<Value, RpcError> {
    if cfg!(target_os = "android") {
        return Err(reason_error(
            UNSUPPORTED,
            "F0013",
            "cook_desktop_only",
            format!(
                "agent.{} runs the regia command line and the asset pipeline, which are desktop tools: \
                 call it on the desktop editor or game, then install the package with `regia install-android`",
                method.kind()
            ),
        ));
    }
    let (args, label) = command_args(method, params)?;
    let root = crate::req_path::project_root(world);
    if method == PipelineMethod::Package
        && let Some(path) = crate::params::opt_str(params, "audit_dir")?
    {
        let resolved = crate::req_path::project_file_in(&root, "audit_dir", path)?;
        if !resolved.is_dir() {
            return Err(RpcError::not_found(
                "audit_dir is not a directory",
                "choose an extracted package directory",
            ));
        }
    }
    if method == PipelineMethod::Package
        && let Some(path) = crate::params::opt_str(params, "audit_artifact")?
    {
        // Confined like audit_dir: an existing path inside the project, never an escaping one.
        crate::req_path::project_file_in(&root, "audit_artifact", path)?;
    }
    if !root.join("Regia.toml").is_file() {
        return Err(reason_error(
            INVALID_PARAMS,
            "F0002",
            "cook_no_manifest",
            "the project root has no Regia.toml, so there is nothing to cook or package",
        ));
    }
    let program = program(world);
    let mut command_line = vec![program.display().to_string()];
    command_line.extend(args.iter().cloned());
    if crate::params::opt_bool(params, "dry_run")?.unwrap_or(false) {
        return Ok(json!({ "dry_run": true, "command": command_line, "cwd": root.display().to_string() }));
    }
    if method == PipelineMethod::Package && crate::params::opt_bool(params, "confirm")? != Some(true) {
        return Err(reason_error(
            INVALID_PARAMS,
            "F0002",
            "needs_confirmation",
            "packaging builds and writes a shippable game: repeat with `confirm: true` once the owner agreed",
        ));
    }
    let kind = method.kind();
    let mut registry = world.get_resource_or_insert_with::<JobRegistry>(Default::default);
    // One pipeline job at a time: a cook and a package share the cooked cache and the paks.
    for busy in ["cook", "package"] {
        if let Some(running) = registry.running(busy) {
            return Err(reason_error(
                INVALID_PARAMS,
                "F0002",
                "cook_busy",
                format!(
                    "{running} is still running; poll it with agent.job_status or stop it with agent.job_cancel"
                ),
            ));
        }
    }
    let (job, handle) = registry.start(kind, label, true);
    let run = Run {
        program: program.clone(),
        args,
        root,
        env: Vec::new(),
        command_line: command_line.clone(),
        start_message: match method {
            PipelineMethod::Cook => "cooking",
            PipelineMethod::Package => "packaging",
        },
        thread_name: match method {
            PipelineMethod::Cook => COOK_THREAD_NAME,
            PipelineMethod::Package => PACKAGE_THREAD_NAME,
        },
        tally: Box::new(CliTally::default()),
        finish: Box::new(|result| result),
    };
    if let Err(error) = spawn_run(run, handle.clone()) {
        handle.fail(
            "cook_spawn_failed",
            format!(
                "cannot start {}: {error} (install the regia CLI or set REGIA_CLI)",
                program.display()
            ),
        );
    }
    Ok(json!({ "job": job, "command": command_line }))
}

/// The fixture of the `agent.cook_package_output` fuzz target: progress lines, a pretty-printed
/// result object, and a compact one (the last wins).
pub const FUZZ_FIXTURE: &str = "\
cook: importing assets/props/crate.glb
cook: 3 assets cooked, 1 from cache
{
  \"ok\": false,
  \"note\": \"a brace } inside a string\"
}
pak: wrote 1 pak file(s) to build/paks/linux
{\"ok\":true,\"platform\":\"linux\",\"quality\":\"final\",\"pak_dir\":\"build/paks/linux\",\"paks\":[\"build/paks/linux/base.fpak\"]}
";

/// Feeds every line to the tally (cut-tolerant). Invariants: the tail cap holds and a report,
/// when present, is a JSON object.
///
/// # Errors
/// `BUG_PREFIX`-prefixed messages on a broken invariant (the fuzz harness reports them).
pub fn fuzz_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use crate::fuzz_targets::BUG_PREFIX;
    let text = String::from_utf8_lossy(bytes);
    if text.trim().is_empty() {
        return Err("no regia output".into());
    }
    let mut tally = CliTally::default();
    for line in text.lines() {
        tally.feed(line);
    }
    if tally.tail.len() > MAX_TAIL_LINES {
        return Err(format!("{BUG_PREFIX}the tail cap was exceeded"));
    }
    if tally.report.as_ref().is_some_and(|r| !r.is_object()) {
        return Err(format!("{BUG_PREFIX}a report that is not an object"));
    }
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dry_run reply's `cwd` is the real project root: the command carries no `--project`
    /// flag, so running it in the caller's own directory would cook the wrong project (the
    /// review finding on the `cwd: "."` answer).
    #[test]
    fn dry_run_answers_the_real_project_root_as_cwd() {
        let root = regia_test_utils::unique_temp_dir("cook_package_dry_run_cwd");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::write(root.join("Regia.toml"), "[project]\nname = \"dry\"\n").expect("Regia.toml");
        let mut world = World::new();
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(root.clone()),
        });
        for method in [PipelineMethod::Cook, PipelineMethod::Package] {
            let answer = start(&mut world, method, &json!({ "dry_run": true })).expect("dry run");
            assert_eq!(answer["dry_run"], json!(true));
            assert_eq!(
                answer["cwd"],
                json!(root.display().to_string()),
                "{method:?}: {answer}"
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_last_json_object_is_the_report_and_lines_are_progress() {
        let mut tally = CliTally::default();
        let events: Vec<Value> = FUZZ_FIXTURE.lines().filter_map(|l| tally.feed(l)).collect();
        let report = tally.report.clone().expect("report");
        assert_eq!(report["ok"], true);
        assert_eq!(report["pak_dir"], "build/paks/linux");
        assert_eq!(tally.lines, 3, "{:?}", tally.tail);
        let kinds: Vec<&str> = events.iter().filter_map(|e| e["kind"].as_str()).collect();
        assert_eq!(kinds, ["line", "line", "report", "line", "report"]);
        assert_eq!(
            tally.progress(&events[0]).as_deref(),
            Some("cook: importing assets/props/crate.glb")
        );
        let result = tally.result(Some(0), false, &["regia".into()]);
        assert_eq!(result["ok"], true);
        assert_eq!(result["artifact"], "build/paks/linux");
        assert_eq!(result["exit_code"], 0);
        // A failed exit or a report that says not ok is not ok.
        assert_eq!(tally.result(Some(1), false, &[])["ok"], false);
        let mut failed = CliTally::default();
        failed.feed("{\"ok\":false,\"error\":{\"reason\":\"package_build_failed\"}}");
        assert_eq!(failed.result(Some(0), false, &[])["ok"], false);
        assert_eq!(failed.result(Some(0), false, &[])["artifact"], Value::Null);
    }

    #[test]
    fn package_reports_name_the_artifact() {
        let mut tally = CliTally::default();
        tally.feed("package: building launcher");
        tally.feed("{\"ok\":true,\"platform\":\"linux\",\"profile\":\"shipping\",\"artifact\":\"dist/linux/game.tar.gz\"}");
        let result = tally.result(Some(0), false, &[]);
        assert_eq!(result["artifact"], "dist/linux/game.tar.gz");
        assert_eq!(result["report"]["profile"], "shipping");
    }

    #[test]
    fn cook_and_package_reports_keep_lint_status_and_report_link() {
        let mut tally = CliTally::default();
        tally.feed(
            r#"{"ok":true,"pak_dir":"build/paks/linux","lint":{"status":"warnings","report":"build/source-lint/report.json"}}"#,
        );
        let result = tally.result(Some(0), false, &[]);
        assert_eq!(result["report"]["lint"]["status"], "warnings");
        assert_eq!(
            result["report"]["lint"]["report"],
            "build/source-lint/report.json"
        );
        let mut failed = CliTally::default();
        failed.feed(
            r#"{"ok":false,"error":{"code":"content_lint_failed","message":"blocked"},"lint":{"status":"failed","report":"build/source-lint/report.json"}}"#,
        );
        let failed = failed.result(Some(1), false, &[]);
        assert_eq!(failed["ok"], false);
        assert_eq!(failed["report"]["lint"]["status"], "failed");
        assert_eq!(failed["report"]["error"]["code"], "content_lint_failed");
    }

    #[test]
    fn a_brace_line_that_is_not_json_stays_human_output() {
        let mut tally = CliTally::default();
        tally.feed("{ not json }");
        tally.feed("{ unbalanced");
        tally.feed("still going");
        assert!(tally.report.is_none());
        // The unbalanced object never closes; the cut run keeps no report.
        assert_eq!(tally.lines, 1);
        assert_eq!(tally.result(None, true, &[])["cancelled"], true);
    }

    #[test]
    fn requests_shape_the_command_and_refuse_bad_values() {
        let (args, label) = command_args(
            PipelineMethod::Cook,
            &json!({ "platform": "android", "quality": "fast" }),
        )
        .expect("cook");
        assert_eq!(
            args,
            ["cook", "--platform", "android", "--quality", "fast", "--json"]
        );
        assert_eq!(label, "fast cook for android");
        let (args, _) = command_args(
            PipelineMethod::Package,
            &json!({ "platform": "windows", "profile": "dev" }),
        )
        .expect("package");
        assert_eq!(
            args,
            ["package", "--platform", "windows", "--profile", "dev", "--json"]
        );
        let reason = |e: RpcError| e.data.and_then(|d| d["reason"].as_str().map(str::to_string));
        for bad in ["playstation", "Linux", "", "--linux"] {
            assert_eq!(
                reason(command_args(PipelineMethod::Cook, &json!({ "platform": bad })).expect_err(bad))
                    .as_deref(),
                Some("cook_bad_platform"),
                "{bad}"
            );
        }
        assert_eq!(
            reason(command_args(PipelineMethod::Cook, &json!({ "quality": "best" })).expect_err("q"))
                .as_deref(),
            Some("cook_bad_quality")
        );
        for bad in ["-x", "a b", "a;b", ""] {
            assert_eq!(
                reason(command_args(PipelineMethod::Package, &json!({ "profile": bad })).expect_err(bad))
                    .as_deref(),
                Some("package_bad_profile"),
                "{bad}"
            );
        }
    }

    /// C-26 (ADR-8835): `container` picks a build-container profile of `regia package`; any other
    /// value (a flag included) is refused before a process starts.
    #[test]
    fn package_container_param_maps_to_the_cli_flag() {
        let (args, _) = command_args(
            PipelineMethod::Package,
            &json!({ "platform": "linux", "container": "steam-sniper" }),
        )
        .expect("package");
        assert_eq!(
            args,
            [
                "package",
                "--platform",
                "linux",
                "--profile",
                "shipping",
                "--json",
                "--container",
                "steam-sniper"
            ]
        );
        let (args, _) =
            command_args(PipelineMethod::Package, &json!({ "container": "debian12" })).expect("package");
        assert!(args.ends_with(&["--container".to_string(), "debian12".to_string()]));
        let reason = |e: RpcError| e.data.and_then(|d| d["reason"].as_str().map(str::to_string));
        for bad in ["sniper", "latest", "--glibc=host", "", "Debian12"] {
            assert_eq!(
                reason(command_args(PipelineMethod::Package, &json!({ "container": bad })).expect_err(bad))
                    .as_deref(),
                Some("package_bad_container"),
                "{bad}"
            );
        }
    }

    /// The tally takes arbitrary text (fuzz target `agent.cook_package_output`).
    #[test]
    fn mutated_output_never_panics_and_caps_hold() {
        let mut rng = regia_core::fuzz::FuzzRng::new(0x7217_0001);
        for _ in 0..2000 {
            let text = regia_core::fuzz::mutate_text(FUZZ_FIXTURE, &mut rng);
            let mut tally = CliTally::default();
            for line in text.lines() {
                tally.feed(line);
            }
            assert!(tally.tail.len() <= MAX_TAIL_LINES);
            assert!(tally.report.as_ref().is_none_or(Value::is_object));
        }
        let many: String = (0..(MAX_TAIL_LINES * 2)).map(|i| format!("line {i}\n")).collect();
        let mut tally = CliTally::default();
        for line in many.lines() {
            tally.feed(line);
        }
        assert_eq!(tally.tail.len(), MAX_TAIL_LINES);
        assert_eq!(tally.lines, (MAX_TAIL_LINES * 2) as u64);
    }

    #[test]
    fn audit_directory_is_forwarded_and_confined_before_spawn() {
        let root = regia_test_utils::unique_temp_dir("package-audit-path");
        regia_test_utils::write_atomic(&root.join("Regia.toml"), b"[project]\nname='sample'\n")
            .expect("manifest");
        std::fs::create_dir_all(root.join("dist/player")).expect("staging");
        let mut world = World::new();
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(root.clone()),
        });
        let reply = start(
            &mut world,
            PipelineMethod::Package,
            &json!({"audit_dir": "dist/player", "dry_run": true}),
        )
        .expect("audit preview");
        assert!(
            reply["command"]
                .as_array()
                .expect("command")
                .iter()
                .any(|arg| arg == "--audit-dir=dist/player")
        );
        for path in ["../outside", "/outside"] {
            let error = start(
                &mut world,
                PipelineMethod::Package,
                &json!({"audit_dir": path, "dry_run": true}),
            )
            .expect_err("outside refused");
            assert_eq!(error.data.expect("reason")["reason"], "path_outside_project");
        }
        assert!(
            start(
                &mut world,
                PipelineMethod::Package,
                &json!({"audit_dir": "Regia.toml", "dry_run": true})
            )
            .is_err()
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
