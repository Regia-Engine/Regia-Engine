// SPDX-License-Identifier: MPL-2.0
//! `regia assets import-rules check`: report every source's effective import settings, which rule
//! (if any) set each field, and the project's rule health (`docs/specs/12-assets.md` § 10,
//! ADR-3321, book `user-guide/import-rules.md`).
//!
//! Exit 0 when nothing is wrong, 1 on findings (`import_rule_unused`, `import_rule_unknown_field`
//! or a value the importer cannot parse), 2 when `config/import_rules.ron` itself cannot be read
//! (a [`UsageError`], like `regia assets validate`'s bad config).

use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde_json::json;

use super::assets_validate::UsageError;
use regia_asset_pipeline::import_rules::{
    CODE_RULE_UNUSED, CODE_UNKNOWN_FIELD, CheckReport, FieldSourceKind,
};

/// `regia assets import-rules` arguments (the subcommand group).
#[derive(Debug, Args)]
pub struct ImportRulesArgs {
    /// Import-rule operation to run.
    #[command(subcommand)]
    pub command: ImportRulesCommand,
}

/// `regia assets import-rules` subcommands.
#[derive(Debug, Subcommand)]
pub enum ImportRulesCommand {
    /// Report each source's effective import settings, which rule set each field, and the
    /// findings (`import_rule_unused`, `import_rule_unknown_field`, bad values). Exit 1 on
    /// findings, 2 on a broken config.
    Check(ImportRulesCheckArgs),
}

/// `regia assets import-rules check` arguments.
#[derive(Debug, Clone, Args)]
pub struct ImportRulesCheckArgs {
    /// Project directory (contains `assets/` and `config/import_rules.ron`).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Report only sources under this path glob (`assets/ui/**`); findings still count the
    /// whole project.
    #[arg(long, value_name = "GLOB")]
    pub path: Option<String>,
    /// Print JSON on stdout (agents and CI).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Runs an `import-rules` subcommand, printing to stdout.
///
/// # Errors
///
/// A broken `config/import_rules.ron` ([`UsageError`], exit 2) or findings (exit 1).
pub fn run(args: &ImportRulesArgs) -> Result<()> {
    match &args.command {
        ImportRulesCommand::Check(args) => check(args),
    }
}

fn check(args: &ImportRulesCheckArgs) -> Result<()> {
    let report = regia_asset_pipeline::import_rules::check_project(&args.project, args.path.as_deref());
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report_json(&report)).context("json")?
        );
    } else {
        print!("{}", report_text(&report));
    }
    if let Some(err) = &report.config_error {
        return Err(UsageError(format!("{IMPORT_RULES}: {err}")).into());
    }
    // `--path` narrows the report only: the findings, bad values included, count the whole project.
    let whole = args
        .path
        .is_some()
        .then(|| regia_asset_pipeline::import_rules::check_project(&args.project, None));
    let counted = whole.as_ref().unwrap_or(&report);
    let findings = counted.findings.len()
        + counted
            .assets
            .iter()
            .filter(|asset| asset.value_problem.is_some())
            .count();
    if findings > 0 {
        bail!("import rules: {findings} finding(s) (codes {CODE_RULE_UNUSED}, {CODE_UNKNOWN_FIELD})");
    }
    Ok(())
}

const IMPORT_RULES: &str = "config/import_rules.ron";

fn report_json(report: &CheckReport) -> serde_json::Value {
    json!({
        "rules": report.rules,
        "config_error": report.config_error,
        "findings": report.findings.iter().map(|f| json!({
            "code": f.code,
            "rule_index": f.rule_index,
            "glob": f.glob,
            "path": f.path,
            "field": f.field,
        })).collect::<Vec<_>>(),
        "assets": report.assets.iter().map(|a| json!({
            "path": a.path,
            "importer": a.importer,
            "settings": a.effective.settings,
            "matched_rules": a.effective.matched_rules,
            "fields": a.effective.fields.iter().map(|field| json!({
                "field": field.field,
                "value": field.value,
                "source": match &field.source {
                    FieldSourceKind::Meta => json!("meta"),
                    FieldSourceKind::Rule { index, glob } => json!({
                        "rule": index,
                        "glob": glob,
                    }),
                },
            })).collect::<Vec<_>>(),
            "value_problem": a.value_problem,
        })).collect::<Vec<_>>(),
    })
}

fn report_text(report: &CheckReport) -> String {
    let mut out = String::new();
    if let Some(err) = &report.config_error {
        let _ = writeln!(out, "{IMPORT_RULES}: {err}");
        return out;
    }
    if report.rules == 0 {
        let _ = writeln!(out, "{IMPORT_RULES}: no rules configured (nothing to check)");
        return out;
    }
    let _ = writeln!(out, "{IMPORT_RULES}: {} rule(s)", report.rules);
    for asset in &report.assets {
        let _ = writeln!(out, "\n{} ({})", asset.path, asset.importer);
        let _ = writeln!(out, "  effective: {}", asset.effective.settings);
        for field in &asset.effective.fields {
            let source = match &field.source {
                FieldSourceKind::Meta => "meta".to_string(),
                FieldSourceKind::Rule { index, glob } => format!("rule[{index}] ({glob})"),
            };
            let _ = writeln!(out, "    {}: {} <- {source}", field.field, field.value);
        }
        if let Some(problem) = &asset.value_problem {
            let _ = writeln!(out, "  value problem: {problem}");
        }
    }
    if !report.findings.is_empty() {
        let _ = writeln!(out, "\nfindings:");
        for finding in &report.findings {
            match finding.code {
                CODE_RULE_UNUSED => {
                    let _ = writeln!(
                        out,
                        "  {} rule[{}] ({}): matches no source in the project",
                        finding.code, finding.rule_index, finding.glob
                    );
                }
                CODE_UNKNOWN_FIELD => {
                    let _ = writeln!(
                        out,
                        "  {} rule[{}] ({}): `{}` on {} is not a field its importer knows",
                        finding.code, finding.rule_index, finding.glob, finding.field, finding.path
                    );
                }
                _ => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(rules: &str, files: &[&str]) -> PathBuf {
        let dir = regia_test_utils::unique_temp_dir("cli-import-rules");
        std::fs::create_dir_all(dir.join("assets/ui")).expect("assets");
        std::fs::create_dir_all(dir.join("config")).expect("config");
        std::fs::write(dir.join(IMPORT_RULES), rules).expect("rules");
        for file in files {
            std::fs::write(dir.join(file), b"x").expect("source");
        }
        dir
    }

    #[test]
    fn check_passes_a_clean_project_and_fails_findings() {
        let dir = project(
            "(rules: [(glob: \"assets/ui/**\", importer: \"png\", settings: \"(compress: Some(false))\")])",
            &["assets/ui/icon.png"],
        );
        let args = ImportRulesCheckArgs {
            project: dir.clone(),
            path: None,
            json: false,
        };
        run(&ImportRulesArgs {
            command: ImportRulesCommand::Check(args.clone()),
        })
        .expect("clean project passes");

        std::fs::write(
            dir.join(IMPORT_RULES),
            "(rules: [(glob: \"assets/none/**\", settings: \"()\")])",
        )
        .expect("rules");
        let err = run(&ImportRulesArgs {
            command: ImportRulesCommand::Check(args.clone()),
        })
        .expect_err("an unused rule fails");
        assert!(err.to_string().contains(CODE_RULE_UNUSED), "{err}");

        std::fs::write(
            dir.join(IMPORT_RULES),
            "(rules: [(glob: \"assets/ui/**\", importer: \"png\", settings: \"(meshlets: true)\")])",
        )
        .expect("rules");
        let err = run(&ImportRulesArgs {
            command: ImportRulesCommand::Check(args),
        })
        .expect_err("an unknown field fails");
        assert!(err.to_string().contains(CODE_UNKNOWN_FIELD), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_exits_2_on_a_broken_config_and_json_runs() {
        let dir = project("(rules: [(glob: \"\", settings: \"()\")])", &[]);
        let args = ImportRulesCheckArgs {
            project: dir.clone(),
            path: None,
            json: false,
        };
        let err = run(&ImportRulesArgs {
            command: ImportRulesCommand::Check(args),
        })
        .expect_err("a broken config is a usage error");
        let usage = err
            .downcast_ref::<UsageError>()
            .unwrap_or_else(|| panic!("UsageError, not {err:?}"));
        assert!(usage.0.contains("rule 0"), "{}", usage.0);

        std::fs::write(
            dir.join(IMPORT_RULES),
            "(rules: [(glob: \"assets/**\", importer: \"png\", settings: \"(compress: Some(false))\")])",
        )
        .expect("rules");
        std::fs::write(dir.join("assets/ui/icon.png"), b"x").expect("source");
        run(&ImportRulesArgs {
            command: ImportRulesCommand::Check(ImportRulesCheckArgs {
                project: dir.clone(),
                path: Some("assets/ui/**".into()),
                json: true,
            }),
        })
        .expect("json output runs");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_flags_a_value_the_importer_cannot_parse() {
        let dir = project(
            "(rules: [(glob: \"assets/ui/**\", importer: \"png\", settings: \"(compress: flase)\")])",
            &["assets/ui/icon.png"],
        );
        let args = ImportRulesCheckArgs {
            project: dir.clone(),
            path: None,
            json: false,
        };
        let err = run(&ImportRulesArgs {
            command: ImportRulesCommand::Check(args),
        })
        .expect_err("a bad value fails the check");
        assert!(err.to_string().contains("finding"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--path` narrows the report, not the findings: a bad value outside the glob still fails.
    #[test]
    fn audit_a_bad_value_outside_the_path_glob_still_fails() {
        let dir = project(
            "(rules: [(glob: \"assets/env/**\", importer: \"png\", settings: \"(compress: flase)\")])",
            &[],
        );
        std::fs::create_dir_all(dir.join("assets/env")).expect("env");
        std::fs::write(dir.join("assets/env/a.png"), b"x").expect("env source");
        std::fs::write(dir.join("assets/ui/b.png"), b"x").expect("ui source");
        let err = run(&ImportRulesArgs {
            command: ImportRulesCommand::Check(ImportRulesCheckArgs {
                project: dir.clone(),
                path: Some("assets/ui/**".into()),
                json: true,
            }),
        })
        .expect_err("the bad value under assets/env counts");
        assert!(err.to_string().contains("1 finding"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
