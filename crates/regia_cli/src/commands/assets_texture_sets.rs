// SPDX-License-Identifier: MPL-2.0
//! `regia assets texture-sets`: detect PBR texture sets in a folder and generate one
//! `.material.ron` per set (spec 22 T2.14, ADR-3320, `book/user-guide/materials.md`).
//!
//! A texture set is a group of raster maps named `<base>_<suffix>.<ext>` (the shared suffix
//! table `regia_asset::texture_sets::TEXTURE_SUFFIX_TABLE`, extended by `Regia.toml`
//! `[import.texture_sets]`). The command is a dry run by default: it prints what it detected
//! and what it would write; `--write` updates the members' `.meta` and writes the materials
//! (never over an existing one).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::json;

/// `regia assets texture-sets` arguments.
#[derive(Debug, Args)]
pub struct AssetsTextureSetsArgs {
    /// Project directory (contains `assets/` and `Regia.toml`).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// The folder to scan, relative to the project root (e.g. `assets/textures/rocks`).
    pub folder: String,
    /// Write the materials and the members' `.meta` (default: dry run, print only).
    #[arg(long, default_value_t = false)]
    pub write: bool,
    /// Descend into sub-folders (a set never spans folders).
    #[arg(long, default_value_t = false)]
    pub recursive: bool,
    /// Print the report as JSON on stdout.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Runs `regia assets texture-sets`, returning the report text (or JSON with `--json`).
///
/// # Errors
/// The project folder is unreadable, `Regia.toml` `[import.texture_sets]` is invalid, or a
/// write fails.
pub fn run(args: &AssetsTextureSetsArgs) -> Result<String> {
    let settings = regia_asset_pipeline::texture_set::TextureSetSettings::for_project(&args.project)
        .map_err(|e| anyhow::anyhow!(e))
        .with_context(|| format!("reading {}", args.project.join("Regia.toml").display()))?;
    let folder = args.project.join(&args.folder);
    // The folder is project-relative: no absolute path, no `..`, and no symlink out of it
    // (`--write` writes materials and `.meta` files there).
    let relative = std::path::Path::new(&args.folder)
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir));
    let inside = match (folder.canonicalize(), args.project.canonicalize()) {
        (Ok(folder), Ok(project)) => folder.starts_with(project),
        _ => false,
    };
    if !relative || !inside || !folder.is_dir() {
        bail!(
            "texture_sets_no_folder: {} is not a folder under the project",
            args.folder
        );
    }
    let scan = scan_folder(&folder, args.recursive, &settings)?;
    let outcomes = regia_asset_pipeline::texture_set::generate(&scan, &settings, args.write)?;
    if args.json {
        Ok(serde_json::to_string_pretty(&json!({
            "folder": args.folder,
            "write": args.write,
            "sets": outcomes,
            "leftovers": scan.leftovers,
        }))?)
    } else {
        Ok(regia_asset_pipeline::texture_set::format_report(
            &outcomes,
            &scan.leftovers,
        ))
    }
}

fn scan_folder(
    folder: &std::path::Path,
    recursive: bool,
    settings: &regia_asset_pipeline::texture_set::TextureSetSettings,
) -> Result<regia_asset_pipeline::texture_set::TextureSetScan> {
    regia_asset_pipeline::texture_set::scan(folder, recursive, settings).map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::path::Path;

    /// A project with `assets/rocks/rock_{BaseColor,Normal}.png` and one nested set in
    /// `assets/rocks/moss/`. The maps are not decoded, so their bytes are placeholders.
    fn project(tag: &str) -> PathBuf {
        let root = regia_test_utils::unique_temp_dir(tag);
        let folder = root.join("assets/rocks");
        let nested = folder.join("moss");
        std::fs::create_dir_all(&nested).expect("folders");
        for (dir, name) in [
            (&folder, "rock_BaseColor.png"),
            (&folder, "rock_Normal.png"),
            (&nested, "moss_BaseColor.png"),
        ] {
            std::fs::write(dir.join(name), b"not-a-real-png").expect("map");
        }
        root
    }

    fn args(root: &Path, write: bool, recursive: bool, json: bool) -> AssetsTextureSetsArgs {
        AssetsTextureSetsArgs {
            project: root.to_path_buf(),
            folder: "assets/rocks".into(),
            write,
            recursive,
            json,
        }
    }

    fn report(root: &Path, write: bool, recursive: bool) -> Value {
        let text = run(&args(root, write, recursive, true)).expect("runs");
        serde_json::from_str(&text).expect("--json prints JSON")
    }

    /// Spec 22 § T2.14a: the command is a dry run unless `--write`; `--json` carries the folder,
    /// the write flag, the sets and the leftovers (ADR-5628).
    #[test]
    fn a_dry_run_reports_the_set_and_writes_nothing() {
        let root = project("texsets-cli-dry");
        let out = report(&root, false, false);
        assert_eq!(out["folder"], "assets/rocks");
        assert_eq!(out["write"], false);
        assert!(out["leftovers"].is_array(), "{out}");
        let sets = out["sets"].as_array().expect("sets");
        assert_eq!(sets.len(), 1, "the nested set needs --recursive: {out}");
        assert_eq!(sets[0]["name"], "rock");
        assert_eq!(sets[0]["written"], false);
        assert!(!root.join("assets/rocks/rock.material.ron").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recursive_finds_the_nested_set_and_write_never_overwrites() {
        let root = project("texsets-cli-write");
        let deep = report(&root, false, true);
        let mut names: Vec<&str> = deep["sets"]
            .as_array()
            .expect("sets")
            .iter()
            .filter_map(|s| s["name"].as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["moss", "rock"]);

        let written = report(&root, true, false);
        assert_eq!(written["write"], true);
        assert_eq!(written["sets"][0]["written"], true, "{written}");
        let material = root.join("assets/rocks/rock.material.ron");
        let first = std::fs::read_to_string(&material).expect("material written");
        assert!(
            !root.join("assets/rocks/moss/moss.material.ron").exists(),
            "not recursive"
        );

        // A second --write finds the material and leaves it as it is.
        let again = report(&root, true, false);
        assert_eq!(again["sets"][0]["material_exists"], true, "{again}");
        assert_eq!(again["sets"][0]["written"], false, "{again}");
        assert_eq!(std::fs::read_to_string(&material).expect("material"), first);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_text_report_names_the_set_and_a_missing_folder_is_an_error() {
        let root = project("texsets-cli-text");
        let text = run(&args(&root, false, false, false)).expect("runs");
        assert!(text.contains("rock"), "{text}");
        assert!(
            serde_json::from_str::<Value>(&text).is_err(),
            "plain text without --json"
        );
        let mut missing = args(&root, false, false, false);
        missing.folder = "assets/none".into();
        let err = run(&missing).expect_err("no folder");
        assert!(err.to_string().contains("texture_sets_no_folder"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Corr-low (cli LOW): the folder is "relative to the project root", but `../x` (or an
    /// absolute path) passed and `--write` wrote materials and `.meta` files outside the project.
    #[test]
    fn corr_low_a_folder_outside_the_project_is_refused() {
        let root = regia_test_utils::unique_temp_dir("texture_sets_outside");
        let project = root.join("project");
        std::fs::create_dir_all(project.join("assets")).expect("project");
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(outside.join("rock_albedo.png"), b"x").expect("map");
        std::fs::write(outside.join("rock_normal.png"), b"x").expect("map");
        for folder in ["../outside".to_owned(), outside.display().to_string()] {
            let args = AssetsTextureSetsArgs {
                project: project.clone(),
                folder: folder.clone(),
                write: true,
                recursive: false,
                json: true,
            };
            let err = run(&args).expect_err("a folder outside the project is refused");
            assert!(
                err.to_string().contains("texture_sets_no_folder"),
                "{folder}: {err:#}"
            );
        }
        let written: Vec<_> = std::fs::read_dir(&outside)
            .expect("outside")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".meta") || name.ends_with(".material.ron"))
            .collect();
        assert!(
            written.is_empty(),
            "files written outside the project: {written:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
