// SPDX-License-Identifier: MPL-2.0
//! `regia rules check <file>...`: check `.rules.ron` rule sets (ADR-8694, `book/agents/match-rules.md`
//! § Rule sets).
//!
//! Each file is parsed (size cap, version, unknown fields), checked, then resolved against the
//! shipped defaults, the project's `assets/rules/` and the other files named on the command line, so an
//! `extends` chain is checked as the game would load it. Every problem prints its stable code and field
//! path; the command exits non-zero when any file has one.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use regia::framework::ruleset_file::{MAX_RULES_RON_BYTES, RuleSetRegistry, parse_rules_ron};
use serde::Serialize;

/// `regia rules` subcommands.
#[derive(Debug, Subcommand)]
pub enum RulesCommand {
    /// Check `.rules.ron` files: parse, check and resolve each; exits non-zero on errors.
    Check(CheckArgs),
}

/// `regia rules check`.
#[derive(Debug, Args)]
pub struct CheckArgs {
    /// The `.rules.ron` files.
    #[arg(required = true)]
    pub files: Vec<PathBuf>,
    /// The project whose `assets/rules/` parents resolve against (default: the current directory).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Print one JSON report instead of lines.
    #[arg(long)]
    pub json: bool,
}

/// One file's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileReport {
    /// The file as given.
    pub file: String,
    /// The rule set's id, once it parsed.
    pub id: Option<String>,
    /// The skeleton it resolves to (`ctf`, ...).
    pub mode: Option<String>,
    /// The resolved content hash, 16 hex digits.
    pub content_hash: Option<String>,
    /// The error code (`ruleset_*`), when the file failed.
    pub code: Option<String>,
    /// The field path of the error.
    pub path: Option<String>,
    /// The error message.
    pub message: Option<String>,
}

impl FileReport {
    fn failed(file: &str, code: &str, path: &str, message: &str) -> Self {
        Self {
            file: file.to_owned(),
            id: None,
            mode: None,
            content_hash: None,
            code: Some(code.to_owned()),
            path: Some(path.to_owned()),
            message: Some(message.to_owned()),
        }
    }

    /// True when the file has no error.
    pub fn ok(&self) -> bool {
        self.code.is_none()
    }
}

/// Check `files` against the defaults and `project`'s `assets/rules/`.
pub fn check(files: &[PathBuf], project: &std::path::Path) -> Vec<FileReport> {
    let mut registry = RuleSetRegistry::with_defaults();
    registry.load_project(project);
    let mut texts: Vec<(String, Result<String, String>)> = Vec::new();
    for file in files {
        let shown = file.display().to_string();
        let text = std::fs::metadata(file)
            .map_err(|e| e.to_string())
            .and_then(|meta| {
                if usize::try_from(meta.len()).unwrap_or(usize::MAX) > MAX_RULES_RON_BYTES {
                    Err(format!(
                        "ruleset_too_large: {} bytes; a rule set holds at most {MAX_RULES_RON_BYTES}",
                        meta.len()
                    ))
                } else {
                    std::fs::read_to_string(file).map_err(|e| e.to_string())
                }
            });
        texts.push((shown, text));
    }
    // Load every named file first (replacing a project file of the same id), so a chain between
    // named files resolves.
    let mut library = registry.library().clone();
    let mut parsed: Vec<(String, Result<String, FileReport>)> = Vec::new();
    for (shown, text) in texts {
        let outcome = match text {
            Err(message) if message.starts_with("ruleset_too_large") => Err(FileReport::failed(
                &shown,
                "ruleset_too_large",
                "",
                message.trim_start_matches("ruleset_too_large: "),
            )),
            Err(message) => Err(FileReport::failed(&shown, "ruleset_parse", "", &message)),
            Ok(text) => match parse_rules_ron(&text).and_then(|set| {
                let id = set.id.clone();
                library.replace(set).map(|_| id)
            }) {
                Ok(id) => Ok(id),
                Err(e) => Err(FileReport::failed(&shown, &e.code, &e.path, &e.message)),
            },
        };
        parsed.push((shown, outcome));
    }
    parsed
        .into_iter()
        .map(|(shown, outcome)| match outcome {
            Err(report) => report,
            Ok(id) => match library.resolve(&id, &BTreeMap::new()) {
                Ok(resolved) => FileReport {
                    file: shown,
                    id: Some(id),
                    mode: Some(resolved.mode.as_lobby_str().to_owned()),
                    content_hash: Some(format!("{:016x}", resolved.content_hash())),
                    code: None,
                    path: None,
                    message: None,
                },
                Err(e) => FileReport {
                    id: Some(id),
                    ..FileReport::failed(&shown, &e.code, &e.path, &e.message)
                },
            },
        })
        .collect()
}

/// Run `regia rules`.
///
/// # Errors
///
/// When any file has an error (after printing every report).
pub fn run(command: &RulesCommand) -> Result<()> {
    let RulesCommand::Check(args) = command;
    let reports = check(&args.files, &args.project);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
    } else {
        for report in &reports {
            match (&report.code, &report.id) {
                (None, Some(id)) => println!(
                    "ok {}: {id} ({}, content hash {})",
                    report.file,
                    report.mode.as_deref().unwrap_or("?"),
                    report.content_hash.as_deref().unwrap_or("?")
                ),
                (Some(code), _) => println!(
                    "error {}: {code} at {}: {}",
                    report.file,
                    report.path.as_deref().unwrap_or(""),
                    report.message.as_deref().unwrap_or("")
                ),
                (None, None) => {}
            }
        }
    }
    let failed = reports.iter().filter(|r| !r.ok()).count();
    if failed > 0 {
        bail!("{failed} of {} rule sets have errors", reports.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_reports_codes_and_paths_and_resolves_chains_between_files() {
        let dir = regia_test_utils::unique_temp_dir("rules-check");
        let write = |name: &str, text: &str| {
            let path = dir.join(name);
            let tmp = dir.join(format!("{name}.tmp"));
            std::fs::write(&tmp, text).expect("write");
            std::fs::rename(&tmp, &path).expect("rename");
            path
        };
        let base = write(
            "base.rules.ron",
            "(format_version: 1, id: \"base\", skeleton: \"koth\", session: (objectives: (hill_score_limit: 90)))",
        );
        let child = write(
            "child.rules.ron",
            "(format_version: 1, id: \"child\", extends: \"base\")",
        );
        let bad = write(
            "bad.rules.ron",
            "(format_version: 1, id: \"bad\", skeleton: \"ctf\", options: [(key: \"limit\", kind: int, default: \"20\", min: \"1\", max: \"10\")])",
        );
        let orphan = write(
            "orphan.rules.ron",
            "(format_version: 1, id: \"orphan\", extends: \"gone\")",
        );
        let reports = check(&[base, child, bad, orphan], &dir);
        assert!(reports[0].ok() && reports[1].ok(), "{reports:#?}");
        assert_eq!(reports[1].mode.as_deref(), Some("koth"));
        assert_eq!(reports[2].code.as_deref(), Some("ruleset_option_range"));
        assert_eq!(reports[2].path.as_deref(), Some("bad: options[0].default"));
        assert_eq!(reports[3].code.as_deref(), Some("ruleset_unknown"));
        assert_eq!(reports[3].path.as_deref(), Some("orphan: extends"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
