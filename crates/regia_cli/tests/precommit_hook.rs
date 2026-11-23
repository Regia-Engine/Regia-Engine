// SPDX-License-Identifier: MPL-2.0
//! P-153 acceptance (ADR-7789): `regia hooks install` in a temporary git repository; a commit
//! with a staged broken asset is stopped by the hook, the staged fix commits. Runs the real
//! `regia` binary and the real `git commit`.

use std::path::Path;
use std::process::{Command, Output};

fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .args([
            "-c",
            "user.name=Regia Test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .expect("git runs (git is required by the workspace)")
}

fn ok(dir: &Path, args: &[&str]) {
    let out = git(dir, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn the_pre_commit_hook_stops_a_broken_staged_asset_and_lets_the_fix_through() {
    let dir = regia_test_utils::unique_temp_dir("precommit-hook-e2e");
    ok(&dir, &["init", "-q"]);
    std::fs::write(dir.join("README.md"), "test\n").expect("readme");
    ok(&dir, &["add", "README.md"]);
    ok(&dir, &["commit", "-q", "-m", "init"]);
    let project = dir.join("game");
    std::fs::create_dir_all(project.join("assets/mat")).expect("dirs");

    let install = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["hooks", "install", "--project"])
        .arg(&project)
        .output()
        .expect("regia runs");
    assert!(
        install.status.success(),
        "install: {}",
        String::from_utf8_lossy(&install.stderr)
    );

    // A clean asset commits.
    let path = project.join("assets/mat/rock.material.ron");
    std::fs::write(&path, "Material(base_color: (1.0, 1.0, 1.0, 1.0))").expect("good");
    ok(&dir, &["add", "-A"]);
    ok(&dir, &["commit", "-q", "-m", "clean asset"]);

    // A broken staged asset is stopped, with the finding on stderr or stdout.
    std::fs::write(&path, "Material(base_color: (1.0, ").expect("broken");
    ok(&dir, &["add", "-A"]);
    let out = git(&dir, &["commit", "-q", "-m", "broken asset"]);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success(), "the hook stops the commit: {said}");
    assert!(said.contains("staged_parse_error"), "{said}");
    let head = git(&dir, &["log", "-1", "--format=%s"]);
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "clean asset");

    // `git commit -a` checks the temporary index git builds for it: fixed and staged by -a, it passes.
    std::fs::write(&path, "Material(base_color: (0.5, 0.5, 0.5, 1.0))").expect("fixed");
    ok(&dir, &["commit", "-q", "-a", "-m", "fixed asset"]);
    let head = git(&dir, &["log", "-1", "--format=%s"]);
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "fixed asset");

    // Uninstall removes the hook: a broken file then commits.
    let uninstall = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["hooks", "uninstall", "--project"])
        .arg(&project)
        .output()
        .expect("regia runs");
    assert!(uninstall.status.success());
    std::fs::write(&path, "Material(").expect("broken");
    ok(&dir, &["commit", "-q", "-a", "-m", "no hook"]);
    let _ = std::fs::remove_dir_all(&dir);
}
