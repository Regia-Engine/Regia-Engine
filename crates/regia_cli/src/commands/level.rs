// SPDX-License-Identifier: MPL-2.0
//! `regia level split|merge`: lossless conversion between the single-file `.level.ron` and the
//! directory form `.level.d/` (`docs/specs/37-per-entity-levels-layers-instances.md` § 3.5), and
//! `regia level validate`, the layer checks of § 4.5.
//!
//! The round trip `file -> dir -> file` is byte-identical after normalization (entities in guid
//! order, canonical RON). Converting a host level does not convert the levels it references.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use regia_scene::level_dir::{self, LevelForm, level_paths};

/// `regia level` subcommands.
#[derive(Debug, Subcommand)]
pub enum LevelCommand {
    /// Convert a `.level.ron` into the directory form (`<name>.level.d/`, one file per entity).
    Split(SplitArgs),
    /// Convert a `.level.d/` directory into a single `.level.ron`.
    Merge(MergeArgs),
    /// Check a level's layers: its bound definitions and its memberships (exit 1 on a finding).
    Validate(ValidateArgs),
}

/// `regia level validate`.
#[derive(Debug, Args)]
pub struct ValidateArgs {
    /// The level, either form.
    pub level: PathBuf,
    /// One JSON object instead of text.
    #[arg(long)]
    pub json: bool,
}

/// `regia level split`.
#[derive(Debug, Args)]
pub struct SplitArgs {
    /// The `.level.ron` file.
    pub file: PathBuf,
    /// Output directory (default: the sibling `<name>.level.d`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Keep the source file (by default it is removed after a successful split, because a level
    /// may exist in only one form).
    #[arg(long)]
    pub keep: bool,
}

/// `regia level merge`.
#[derive(Debug, Args)]
pub struct MergeArgs {
    /// The `.level.d` directory.
    pub dir: PathBuf,
    /// Output file (default: the sibling `<name>.level.ron`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Keep the source directory (by default it is removed after a successful merge).
    #[arg(long)]
    pub keep: bool,
}

/// Runs a `regia level` subcommand.
pub fn run(command: &LevelCommand) -> Result<()> {
    match command {
        LevelCommand::Split(args) => split(args),
        LevelCommand::Merge(args) => merge(args),
        LevelCommand::Validate(args) => validate(args),
    }
}

/// One finding per line (`<reason> <detail>`, or JSON); any finding fails the command.
fn validate(args: &ValidateArgs) -> Result<()> {
    let problems = regia_framework::level_assets::validate_level_source(&args.level)?;
    if args.json {
        let rows: Vec<serde_json::Value> = problems
            .iter()
            .map(|p| serde_json::json!({ "reason": p.reason, "detail": p.detail, "fatal": p.fatal }))
            .collect();
        println!("{}", serde_json::json!({ "level": args.level, "problems": rows }));
    } else {
        for problem in &problems {
            println!("{} {}", problem.reason, problem.detail);
        }
    }
    if !problems.is_empty() {
        bail!("{}: {} layer finding(s)", args.level.display(), problems.len());
    }
    eprintln!("level validate: {} is clean", args.level.display());
    Ok(())
}

fn split(args: &SplitArgs) -> Result<()> {
    if !args.file.is_file() {
        bail!("level_not_found: {}", args.file.display());
    }
    let out = args.out.clone().unwrap_or_else(|| level_paths(&args.file).1);
    let report = level_dir::split_level_file(&args.file, &out)?;
    eprintln!(
        "level split: {} -> {} ({} written, {} deleted, {} unchanged)",
        args.file.display(),
        out.display(),
        report.written.len(),
        report.deleted.len(),
        report.unchanged
    );
    remove_source_if_sibling(&args.file, &out, args.keep, LevelForm::File)
}

fn merge(args: &MergeArgs) -> Result<()> {
    if !args.dir.is_dir() {
        bail!("level_not_found: {}", args.dir.display());
    }
    let out = args.out.clone().unwrap_or_else(|| level_paths(&args.dir).0);
    level_dir::merge_level_dir(&args.dir, &out)?;
    eprintln!("level merge: {} -> {}", args.dir.display(), out.display());
    remove_source_if_sibling(&args.dir, &out, args.keep, LevelForm::Dir)
}

/// A level may exist in only one form (`ambiguous_level_form`): when the output is the sibling of
/// the source, the source is removed unless `--keep`.
fn remove_source_if_sibling(source: &Path, out: &Path, keep: bool, form: LevelForm) -> Result<()> {
    let (file, dir) = level_paths(source);
    let sibling = match form {
        LevelForm::File => dir,
        LevelForm::Dir => file,
    };
    if keep || out != sibling {
        return Ok(());
    }
    match form {
        LevelForm::File => std::fs::remove_file(source)?,
        LevelForm::Dir => std::fs::remove_dir_all(source)?,
    }
    eprintln!(
        "removed {} (a level exists in one form; --keep keeps it)",
        source.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::format::SceneFile;

    const LEVEL: &str = r#"SceneFile(
    format_version: 10,
    entities: [
        (guid: "00000000-0000-0000-0000-0000000000b2", name: "B", components: {}),
        (guid: "00000000-0000-0000-0000-0000000000a1", name: "A", parent: Some("00000000-0000-0000-0000-0000000000b2"), components: {}),
    ],
)
"#;

    #[test]
    fn validate_is_clean_on_a_plain_level_and_fails_on_a_finding() {
        let tmp = regia_test_utils::unique_temp_dir("cli-level-validate");
        let file = tmp.join("yard.level.ron");
        std::fs::write(&file, LEVEL).expect("write");
        let args = |level: &Path| {
            LevelCommand::Validate(ValidateArgs {
                level: level.to_path_buf(),
                json: true,
            })
        };
        run(&args(&file)).expect("clean");

        let stray = LEVEL.replace(
            "name: \"B\", components: {}",
            "name: \"B\", components: { \"regia_scene::layers::LayerMembership\": (layers: [\"ghost\"]) }",
        );
        std::fs::write(&file, stray).expect("write");
        let err = run(&args(&file)).expect_err("a finding fails");
        // The unbound layer, and A under a member while in no layer itself.
        assert!(err.to_string().contains("2 layer finding"), "{err}");

        let missing = run(&args(&tmp.join("none.level.ron"))).expect_err("missing");
        assert!(missing.to_string().contains("level_not_found"), "{missing}");
        std::fs::remove_dir_all(tmp).expect("cleanup");
    }

    #[test]
    fn split_then_merge_round_trips_and_keeps_one_form() {
        let tmp = regia_test_utils::unique_temp_dir("cli-level");
        let file = tmp.join("yard.level.ron");
        std::fs::write(&file, LEVEL).expect("write");
        run(&LevelCommand::Split(SplitArgs {
            file: file.clone(),
            out: None,
            keep: false,
        }))
        .expect("split");
        let dir = tmp.join("yard.level.d");
        assert!(dir.join("level.ron").is_file());
        assert!(!file.exists(), "source removed: one form only");
        run(&LevelCommand::Merge(MergeArgs {
            dir: dir.clone(),
            out: None,
            keep: false,
        }))
        .expect("merge");
        assert!(!dir.exists());
        let text = std::fs::read_to_string(&file).expect("merged");
        let mut original = SceneFile::from_ron_str(LEVEL).expect("parse");
        original.entities.sort_by_key(|e| e.guid);
        assert_eq!(text, original.to_ron_string().expect("ron"));
    }

    #[test]
    fn keep_and_missing_inputs() {
        let tmp = regia_test_utils::unique_temp_dir("cli-level-keep");
        let file = tmp.join("k.level.ron");
        std::fs::write(&file, LEVEL).expect("write");
        run(&LevelCommand::Split(SplitArgs {
            file: file.clone(),
            out: None,
            keep: true,
        }))
        .expect("split");
        assert!(file.is_file() && tmp.join("k.level.d").is_dir());
        let err = run(&LevelCommand::Merge(MergeArgs {
            dir: tmp.join("missing.level.d"),
            out: None,
            keep: false,
        }))
        .expect_err("missing");
        assert!(err.to_string().starts_with("level_not_found"), "{err}");
    }
}
