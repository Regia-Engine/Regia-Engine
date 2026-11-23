// SPDX-License-Identifier: MPL-2.0
//! `regia clean` through the real binary: exit codes, the dry run and what survives (ADR-2631).

use std::path::Path;
use std::process::{Command, Output};

fn regia(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .arg("clean")
        .args(args)
        .arg("--project")
        .arg(dir)
        .output()
        .expect("run regia clean")
}

fn project(name: &str) -> std::path::PathBuf {
    let dir = regia_test_utils::unique_temp_dir(name);
    std::fs::write(
        dir.join("Regia.toml"),
        "[project]\nname = \"T\"\nid = \"com.t.t\"\n",
    )
    .expect("manifest");
    for file in [
        ".regia/cooked/linux/a.bin",
        "build/paks/x.fpak",
        "dist/linux/p.tar.gz",
        "assets/keep.txt",
    ] {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, b"data").expect("write");
    }
    dir
}

#[test]
fn outside_a_project_exits_2_with_the_code_and_removes_nothing() {
    let dir = regia_test_utils::unique_temp_dir("clean-cli-none");
    std::fs::create_dir_all(dir.join("build")).expect("mkdir");
    let out = regia(&dir, &["--all"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("clean_not_a_project:"));
    assert!(dir.join("build").is_dir());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn dry_run_lists_everything_and_exits_0_without_deleting() {
    let dir = project("clean-cli-dry");
    let out = regia(&dir, &["--all", "--dry-run"]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    // The plan prints native paths (`.regia\cooked` on Windows, ADR-4924).
    let cooked = std::path::Path::new(".regia").join("cooked");
    for line in [
        format!("would remove {}", cooked.display()),
        "would remove build".to_string(),
        "would remove dist".to_string(),
    ] {
        assert!(text.contains(&line), "{line} in\n{text}");
    }
    assert!(dir.join("build/paks/x.fpak").is_file() && dir.join(".regia/cooked/linux/a.bin").is_file());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn default_level_keeps_build_and_dist_and_all_removes_them() {
    let dir = project("clean-cli-levels");
    assert_eq!(regia(&dir, &[]).status.code(), Some(0));
    assert!(!dir.join(".regia/cooked").exists());
    assert!(dir.join("build").is_dir() && dir.join("dist").is_dir());
    assert_eq!(regia(&dir, &["--all"]).status.code(), Some(0));
    assert!(!dir.join("build").exists() && !dir.join("dist").exists());
    assert!(dir.join("assets/keep.txt").is_file() && dir.join("Regia.toml").is_file());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn cache_and_all_together_are_a_usage_error() {
    let dir = project("clean-cli-conflict");
    let out = regia(&dir, &["--cache", "--all"]);
    assert_eq!(out.status.code(), Some(2), "clap usage errors exit 2");
    assert!(dir.join("build").is_dir());
    let _ = std::fs::remove_dir_all(dir);
}

/// Specs/21 § 1 (ADR-5627): `--json` prints one JSON object on stdout (no text lines), with
/// `/`-separated project-relative paths, and removes exactly what the text mode removes.
#[test]
fn json_prints_one_object_with_the_plan() {
    let dir = project("clean-cli-json");
    let out = regia(&dir, &["--all", "--dry-run", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout is one JSON object");
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["removed"], false);
    assert_eq!(report["level"], "all");
    let paths: Vec<&str> = report["paths"]
        .as_array()
        .expect("paths")
        .iter()
        .filter_map(|p| p["path"].as_str())
        .collect();
    for expected in [".regia/cooked", "build", "dist"] {
        assert!(paths.contains(&expected), "{expected} in {paths:?}");
    }
    assert!(report["bytes"].as_u64().is_some_and(|b| b >= 12), "{report}");
    assert!(dir.join("build").is_dir(), "a dry run removes nothing");

    let out = regia(&dir, &["--json"]);
    assert_eq!(out.status.code(), Some(0));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(report["removed"], true);
    assert_eq!(report["level"], "default");
    assert!(!dir.join(".regia/cooked").exists());
    assert!(dir.join("build").is_dir(), "the default level keeps build/");
    let _ = std::fs::remove_dir_all(dir);
}
