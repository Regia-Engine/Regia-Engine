// SPDX-License-Identifier: MPL-2.0
//! `regia clean [--cache|--all] [--dry-run]`: remove regenerable project state
//! (`docs/specs/21-tooling-and-packaging.md` § 1, ADR-2631).
//!
//! Three levels, each a superset of the one before. Paths are relative to the project root;
//! `<state>` is the per-project state directory (`.regia`):
//!
//! | Level | Removes |
//! |---|---|
//! | default | `<state>/cooked` (the cook cache), `<state>/assetdb` (the asset database) |
//! | `--cache` | the default set plus `<state>/shader-cache` and `<state>/pipeline-cache` |
//! | `--all` | the `--cache` set plus `build/` (paks, Gradle project), `dist/` (packages) and `target/` |
//!
//! Never removed at any level: sources, `assets/`, `Regia.toml`, `pipeline-caches/` (authored
//! caches a package ships), and the rest of `<state>` (agent tokens, autosaves, review state).
//!
//! Safety: the command refuses to run outside a project root (no `Regia.toml`), resolves the parent
//! of every target and refuses one that leaves the project (a `.regia` symlinked elsewhere), and
//! removes a symlink target as the link only, never what it points to. The whole plan is built and
//! checked before the first deletion, and `--dry-run` prints it without deleting anything.
//!
//! Error codes (first word of the message, stable): `clean_not_a_project` (exit 2),
//! `clean_outside_project` (exit 1), `clean_failed` (exit 1). Success exits 0, also when there is
//! nothing to remove.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::Args;

use super::assets_validate::UsageError;

/// Arguments of `regia clean`.
#[derive(Debug, Clone, Args)]
pub struct CleanArgs {
    /// Also remove the shader and pipeline caches under the project state directory.
    #[arg(long, conflicts_with = "all")]
    pub cache: bool,
    /// Remove everything `--cache` does, plus `build/`, `dist/` and `target/`.
    #[arg(long)]
    pub all: bool,
    /// List what would be removed and remove nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Print the plan as one JSON object on stdout instead of text (specs/21 § 1, ADR-5627).
    #[arg(long)]
    pub json: bool,
    /// Project directory (contains `Regia.toml`).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
}

/// How much `regia clean` removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// The cook and asset-database caches.
    Default,
    /// Plus the shader and pipeline caches.
    Cache,
    /// Plus `build/`, `dist/` and `target/`.
    All,
}

impl CleanArgs {
    /// The level the flags select.
    #[must_use]
    pub fn level(&self) -> Level {
        if self.all {
            Level::All
        } else if self.cache {
            Level::Cache
        } else {
            Level::Default
        }
    }
}

/// What a planned removal is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A directory tree.
    Dir,
    /// A single file.
    File,
    /// A symlink: only the link is removed.
    Symlink,
}

/// One path `regia clean` removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Path relative to the project root.
    pub rel: PathBuf,
    /// Absolute path.
    pub path: PathBuf,
    /// Directory, file or symlink.
    pub kind: Kind,
    /// Bytes removed (the link itself for a symlink).
    pub bytes: u64,
}

/// Relative targets per level; `<state>` entries are joined to the state directory name.
const STATE_TARGETS: &[(&str, Level)] = &[
    ("cooked", Level::Default),
    ("assetdb", Level::Default),
    ("shader-cache", Level::Cache),
    ("pipeline-cache", Level::Cache),
];
const ROOT_TARGETS: &[&str] = &["build", "dist", "target"];

/// Runs `regia clean`.
pub fn run(args: &CleanArgs) -> Result<()> {
    let root = args.project.clone();
    let plan = plan(&root, args.level())?;
    if !args.json {
        print!("{}", render_plan(&plan, args.dry_run));
    }
    if !args.dry_run {
        execute(&plan)?;
    }
    if args.json {
        println!("{}", render_json(&plan, args.level(), args.dry_run));
    }
    Ok(())
}

/// The `--json` report: `{ dry_run, level, removed, paths: [{ path, kind, bytes }], bytes }`.
/// `removed` is false on a dry run; `path` is project-relative with `/` separators on every OS.
/// It is printed after the removal, so a failure prints no report (the error goes to stderr).
#[must_use]
pub fn render_json(plan: &[Item], level: Level, dry_run: bool) -> serde_json::Value {
    let paths: Vec<serde_json::Value> = plan
        .iter()
        .map(|item| {
            let path: Vec<String> = item
                .rel
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect();
            serde_json::json!({
                "path": path.join("/"),
                "kind": match item.kind {
                    Kind::Dir => "dir",
                    Kind::File => "file",
                    Kind::Symlink => "symlink",
                },
                "bytes": item.bytes,
            })
        })
        .collect();
    serde_json::json!({
        "dry_run": dry_run,
        "level": match level {
            Level::Default => "default",
            Level::Cache => "cache",
            Level::All => "all",
        },
        "removed": !dry_run,
        "paths": paths,
        "bytes": plan.iter().map(|item| item.bytes).sum::<u64>(),
    })
}

fn code_error(code: &str, message: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{code}: {message}")
}

/// Builds the removal plan for `level` and checks it: a project root, no target outside it.
pub fn plan(project: &Path, level: Level) -> Result<Vec<Item>> {
    if !regia_core::brand::is_project_root(project) {
        return Err(UsageError(format!(
            "clean_not_a_project: {} has no {}; run `regia clean` in a project root or pass --project",
            project.display(),
            regia_core::brand::MANIFEST_FILE
        ))
        .into());
    }
    let root = project.canonicalize().map_err(|e| {
        code_error(
            "clean_failed",
            format!("project directory {}: {e}", project.display()),
        )
    })?;
    let state = regia_core::brand::project_state_dir(&root);
    let state_name = state
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".regia"));

    let mut candidates: Vec<PathBuf> = Vec::new();
    for (name, needed) in STATE_TARGETS {
        if *needed <= level {
            candidates.push(state_name.join(name));
        }
    }
    if level == Level::All {
        candidates.extend(ROOT_TARGETS.iter().map(PathBuf::from));
    }

    let mut items = Vec::new();
    for rel in candidates {
        let path = root.join(&rel);
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue; // not there: nothing to remove
        };
        // The parent must resolve inside the project: a symlinked `.regia` would otherwise lead
        // the deletion out of the tree.
        let parent = path.parent().unwrap_or(&root);
        let resolved = parent
            .canonicalize()
            .map_err(|e| code_error("clean_failed", format!("{}: {e}", parent.display())))?;
        if !resolved.starts_with(&root) {
            return Err(code_error(
                "clean_outside_project",
                format!(
                    "{} resolves to {}, outside the project {}; nothing was removed",
                    rel.display(),
                    resolved.display(),
                    root.display()
                ),
            ));
        }
        let (kind, bytes) = if meta.file_type().is_symlink() {
            (Kind::Symlink, 0)
        } else if meta.is_dir() {
            (Kind::Dir, dir_bytes(&path))
        } else {
            (Kind::File, meta.len())
        };
        items.push(Item {
            rel,
            path,
            kind,
            bytes,
        });
    }
    Ok(items)
}

/// Total size of the regular files under `dir`, without following symlinks.
fn dir_bytes(dir: &Path) -> u64 {
    walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// The text `regia clean` prints for `plan`.
#[must_use]
pub fn render_plan(plan: &[Item], dry_run: bool) -> String {
    let verb = if dry_run { "would remove" } else { "removing" };
    let mut out = String::new();
    for item in plan {
        let note = match item.kind {
            Kind::Dir => String::new(),
            Kind::File => " (file)".to_owned(),
            Kind::Symlink => " (symlink: the link only)".to_owned(),
        };
        let _ = writeln!(
            out,
            "{verb} {} [{}]{note}",
            item.rel.display(),
            human_bytes(item.bytes)
        );
    }
    let total: u64 = plan.iter().map(|i| i.bytes).sum();
    if plan.is_empty() {
        out.push_str("nothing to clean\n");
    } else {
        let _ = writeln!(
            out,
            "{} {} path(s), {}",
            if dry_run { "dry run:" } else { "cleaned" },
            plan.len(),
            human_bytes(total)
        );
    }
    out
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Removes every item of `plan`. A symlink is unlinked; a directory is removed without following
/// links inside it.
pub fn execute(plan: &[Item]) -> Result<()> {
    for item in plan {
        let result = match item.kind {
            Kind::Dir => std::fs::remove_dir_all(&item.path),
            Kind::File | Kind::Symlink => std::fs::remove_file(&item.path).or_else(|e| {
                // A directory symlink is a directory on Windows.
                if item.kind == Kind::Symlink {
                    std::fs::remove_dir(&item.path)
                } else {
                    Err(e)
                }
            }),
        };
        if let Err(e) = result {
            bail!("clean_failed: removing {}: {e}", item.path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str) -> PathBuf {
        let dir = regia_test_utils::unique_temp_dir(name);
        std::fs::write(
            dir.join("Regia.toml"),
            "[project]\nname = \"T\"\nid = \"com.t.t\"\n",
        )
        .expect("manifest");
        dir
    }

    fn touch(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, vec![7u8; bytes]).expect("write");
    }

    fn populate(root: &Path) {
        touch(&root.join(".regia/cooked/linux/a.bin"), 10);
        touch(&root.join(".regia/assetdb/index.bin"), 20);
        touch(&root.join(".regia/shader-cache/s.bin"), 30);
        touch(&root.join(".regia/pipeline-cache/p.fpc"), 40);
        touch(&root.join(".regia/agent/token"), 5);
        touch(&root.join("build/paks/linux/base.fpak"), 50);
        touch(&root.join("dist/linux/game.tar.gz"), 60);
        touch(&root.join("target/debug/game"), 70);
        touch(&root.join("assets/keep.txt"), 1);
        touch(&root.join("pipeline-caches/authored.fpc"), 2);
    }

    fn rels(plan: &[Item]) -> Vec<String> {
        plan.iter()
            .map(|i| i.rel.to_string_lossy().replace('\\', "/"))
            .collect()
    }

    #[test]
    fn default_level_removes_cook_and_asset_db_only() {
        let root = project("clean-default");
        populate(&root);
        let plan = plan(&root, Level::Default).expect("plan");
        assert_eq!(rels(&plan), [".regia/cooked", ".regia/assetdb"]);
        assert_eq!(plan.iter().map(|i| i.bytes).sum::<u64>(), 30);
        execute(&plan).expect("execute");
        assert!(!root.join(".regia/cooked").exists());
        assert!(!root.join(".regia/assetdb").exists());
        assert!(root.join(".regia/shader-cache/s.bin").exists());
        assert!(root.join("build").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cache_level_adds_shader_and_pipeline_caches() {
        let root = project("clean-cache");
        populate(&root);
        let plan = plan(&root, Level::Cache).expect("plan");
        assert_eq!(
            rels(&plan),
            [
                ".regia/cooked",
                ".regia/assetdb",
                ".regia/shader-cache",
                ".regia/pipeline-cache"
            ]
        );
        execute(&plan).expect("execute");
        assert!(!root.join(".regia/pipeline-cache").exists());
        assert!(root.join("dist").exists() && root.join("target").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn all_level_adds_build_dist_target_and_keeps_sources() {
        let root = project("clean-all");
        populate(&root);
        let plan = plan(&root, Level::All).expect("plan");
        assert_eq!(plan.len(), 7);
        execute(&plan).expect("execute");
        for gone in ["build", "dist", "target", ".regia/cooked", ".regia/shader-cache"] {
            assert!(!root.join(gone).exists(), "{gone} removed");
        }
        for kept in [
            "assets/keep.txt",
            "pipeline-caches/authored.fpc",
            ".regia/agent/token",
            "Regia.toml",
        ] {
            assert!(root.join(kept).exists(), "{kept} kept");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn dry_run_lists_and_removes_nothing() {
        let root = project("clean-dry");
        populate(&root);
        run(&CleanArgs {
            cache: false,
            all: true,
            dry_run: true,
            json: false,
            project: root.clone(),
        })
        .expect("dry run");
        assert!(root.join("target/debug/game").exists());
        assert!(root.join(".regia/cooked/linux/a.bin").exists());
        let text = render_plan(&plan(&root, Level::All).expect("plan"), true);
        assert!(text.contains("would remove build [50 B]"), "{text}");
        // The plan prints native paths (`.regia\cooked` on Windows, ADR-4924).
        let cooked = std::path::Path::new(".regia").join("cooked");
        assert!(
            text.starts_with(&format!("would remove {}", cooked.display())),
            "{text}"
        );
        assert!(text.contains("dry run: 7 path(s)"), "{text}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn refuses_outside_a_project_root_with_a_usage_error() {
        let dir = regia_test_utils::unique_temp_dir("clean-no-project");
        touch(&dir.join("build/x"), 1);
        let err = run(&CleanArgs {
            cache: false,
            all: true,
            dry_run: false,
            json: false,
            project: dir.clone(),
        })
        .expect_err("not a project");
        assert!(err.is::<UsageError>(), "exit 2 type");
        assert!(err.to_string().starts_with("clean_not_a_project:"), "{err}");
        assert!(dir.join("build/x").exists(), "nothing removed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn nothing_to_clean_is_success() {
        let root = project("clean-empty");
        let plan = plan(&root, Level::All).expect("plan");
        assert!(plan.is_empty());
        assert_eq!(render_plan(&plan, false), "nothing to clean\n");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_target_is_unlinked_and_the_destination_survives() {
        let root = project("clean-link");
        let outside = regia_test_utils::unique_temp_dir("clean-link-outside");
        touch(&outside.join("precious.txt"), 9);
        std::os::unix::fs::symlink(&outside, root.join("target")).expect("symlink");
        touch(&root.join("build/inner/ok.bin"), 3);
        // A link inside a removed tree is not followed either.
        std::os::unix::fs::symlink(&outside, root.join("build/inner/escape")).expect("symlink");
        let plan = plan(&root, Level::All).expect("plan");
        let target = plan
            .iter()
            .find(|i| i.rel == Path::new("target"))
            .expect("target planned");
        assert_eq!(target.kind, Kind::Symlink);
        assert_eq!(target.bytes, 0, "the destination is not counted");
        execute(&plan).expect("execute");
        assert!(!root.join("target").exists() && std::fs::symlink_metadata(root.join("target")).is_err());
        assert!(!root.join("build").exists());
        assert!(
            outside.join("precious.txt").is_file(),
            "the link destination is untouched"
        );
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[cfg(unix)]
    #[test]
    fn state_dir_symlinked_out_of_the_project_is_refused_before_any_removal() {
        let root = project("clean-state-link");
        let outside = regia_test_utils::unique_temp_dir("clean-state-outside");
        touch(&outside.join("cooked/keep.bin"), 9);
        touch(&root.join("build/x"), 1);
        std::os::unix::fs::symlink(&outside, root.join(".regia")).expect("symlink");
        let err = plan(&root, Level::All).expect_err("must refuse");
        assert!(err.to_string().starts_with("clean_outside_project:"), "{err}");
        assert!(outside.join("cooked/keep.bin").is_file());
        assert!(root.join("build/x").is_file(), "no partial removal");
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[test]
    fn level_follows_the_flags() {
        let mut args = CleanArgs {
            cache: false,
            all: false,
            dry_run: false,
            json: false,
            project: PathBuf::from("."),
        };
        assert_eq!(args.level(), Level::Default);
        args.cache = true;
        assert_eq!(args.level(), Level::Cache);
        args.all = true;
        assert_eq!(args.level(), Level::All);
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(10), "10 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
