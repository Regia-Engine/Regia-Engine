// SPDX-License-Identifier: MPL-2.0
//! `regia package` smoke test of the generated character-template projects (Linux; ADR-2559,
//! ADR-2738): Third Person, First Person and Top Down, one test each over a shared helper.
//!
//! Generates a project with `regia new --template <id>`, packages it for Linux
//! (`regia package --platform linux --format tar --profile dev`), extracts the tarball and checks
//! the package layout of `docs/specs/21-tooling-and-packaging.md` § 4 "Linux":
//! `<name>` (the launcher binary, executable), `content/*.fpak` (the assets pak, non-empty),
//! `content/permutations.json`, `<slug>.desktop` (its `Exec` names the launcher) and
//! `build_info.json` (version, build number, engine, date).
//!
//! The three tests share one target dir, so the engine is compiled once. The dev profile keeps the build to the engine's debug profile (a shipping build is a fat-LTO
//! build of the whole engine). It does not start the game: the `templates` CI job runs the
//! generated launcher under `xvfb-run`.
//!
//! Ignored by default (a full engine build, several minutes); run with
//! `cargo test -p regia_cli --test package_third_person_smoke -- --ignored`.
#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Root of this repo (the test lives in `crates/regia_cli/tests`).
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// The `regia` CLI binary (cargo builds it for this test).
fn regia_cli() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_regia"))
}

/// The target dir of the dev build this test starts, inside this test's own target dir (cached
/// between runs; in the target dir itself it would wait on the `debug` build lock every other
/// cargo takes).
fn dev_target_dir() -> PathBuf {
    // <target>/debug/deps/package_third_person_smoke-<hash> -> <target>/package-templates
    std::env::current_exe()
        .expect("test binary path")
        .ancestors()
        .nth(3)
        .expect("the test binary is under a target dir")
        .join("package-templates")
}

/// Removes a directory tree when dropped (even on assertion failure).
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Files directly in `dir` whose name ends with `suffix`.
fn files_ending(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_file() && path.to_string_lossy().ends_with(suffix))
                .collect()
        })
        .unwrap_or_default()
}

/// Generates `template`, packages it for Linux and checks the layout.
fn package_template(template: &str) {
    let name = "tpl_pkg";
    let work = regia_test_utils::unique_temp_dir(&format!("pkg_{template}"));
    let _cleanup = Cleanup(work.clone());
    let out_dir = work.join("out");

    // 1. regia new --template <id> (the engine path it writes is this checkout).
    let created = Command::new(regia_cli())
        .args(["new", name, "--template", template, "--path"])
        .arg(&work)
        .stderr(Stdio::inherit())
        .output()
        .expect("run regia new");
    assert!(
        created.status.success(),
        "regia new failed: {}{}",
        String::from_utf8_lossy(&created.stdout),
        String::from_utf8_lossy(&created.stderr)
    );
    let project = work.join(name);
    for file in [
        "Regia.toml",
        "launcher/Cargo.toml",
        "game/src/hud.rs",
        "assets/levels/main.level.ron",
    ] {
        assert!(project.join(file).is_file(), "generated project has {file}");
    }
    // The generated project is a workspace of its own; the repo's lockfile keeps it on the
    // dependency versions this checkout is tested with.
    std::fs::copy(repo_root().join("Cargo.lock"), project.join("Cargo.lock")).expect("Cargo.lock");

    // 2. regia package --platform linux (auto-cooks the platform first).
    let package = Command::new(regia_cli())
        .args([
            "package",
            "--platform",
            "linux",
            "--format",
            "tar",
            "--profile",
            "dev",
            "--project",
        ])
        .arg(&project)
        .arg("--out")
        .arg(&out_dir)
        .env("CARGO_TARGET_DIR", dev_target_dir())
        .stderr(Stdio::inherit())
        .output()
        .expect("run regia package");
    assert!(
        package.status.success(),
        "regia package failed: {}{}",
        String::from_utf8_lossy(&package.stdout),
        String::from_utf8_lossy(&package.stderr)
    );
    let stdout = String::from_utf8_lossy(&package.stdout);
    let tarball = stdout
        .lines()
        .rev()
        .find(|line| line.ends_with(".tar.gz"))
        .map_or_else(|| PathBuf::from(stdout.trim_end()), PathBuf::from);
    assert!(tarball.is_file(), "tarball missing at {}", tarball.display());

    // 3. Extract and find the one packaged game directory.
    let extract = work.join("extracted");
    std::fs::create_dir_all(&extract).expect("extract dir");
    let tar = Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&extract)
        .output()
        .expect("tar");
    assert!(tar.status.success(), "tar failed: {tar:?}");
    let game_dirs: Vec<PathBuf> = std::fs::read_dir(&extract)
        .expect("extract dir readable")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    assert_eq!(
        game_dirs.len(),
        1,
        "one game directory in the tarball: {game_dirs:?}"
    );
    let game_dir = &game_dirs[0];

    // 4. The layout of specs/21 § 4 "Linux".
    let binary = game_dir.join(name);
    assert!(binary.is_file(), "the launcher binary is {}", binary.display());
    let mode = std::fs::metadata(&binary)
        .expect("binary metadata")
        .permissions()
        .mode();
    assert!(
        mode & 0o111 != 0,
        "the launcher binary is executable (mode {mode:o})"
    );

    let paks = files_ending(&game_dir.join("content"), ".fpak");
    assert!(!paks.is_empty(), "content/ holds the assets pak");
    for pak in &paks {
        let len = std::fs::metadata(pak).expect("pak metadata").len();
        assert!(len > 0, "{} is not empty", pak.display());
    }
    assert!(
        game_dir.join("content/permutations.json").is_file(),
        "content/permutations.json beside the pak"
    );

    let desktops = files_ending(game_dir, ".desktop");
    assert_eq!(desktops.len(), 1, "one .desktop file: {desktops:?}");
    let desktop = std::fs::read_to_string(&desktops[0]).expect(".desktop readable");
    assert!(desktop.contains("[Desktop Entry]"), "{desktop}");
    assert!(
        desktop.contains(&format!("Exec={name}")),
        "Exec names the launcher: {desktop}"
    );

    let info_text = std::fs::read_to_string(game_dir.join("build_info.json")).expect("build_info.json");
    let info: serde_json::Value = serde_json::from_str(&info_text).expect("build_info.json is JSON");
    assert_eq!(info["version"], "0.1.0", "{info}");
    assert_eq!(info["build_number"], 1, "{info}");
    for key in ["engine", "date", "git"] {
        assert!(info[key].is_string(), "build_info.json has {key}: {info}");
    }
}

#[test]
#[ignore = "full engine build of a generated project; invoke with -- --ignored"]
fn a_generated_third_person_project_packages_to_the_linux_layout() {
    package_template("third_person");
}

#[test]
#[ignore = "full engine build of a generated project; invoke with -- --ignored"]
fn a_generated_first_person_project_packages_to_the_linux_layout() {
    package_template("first_person");
}

#[test]
#[ignore = "full engine build of a generated project; invoke with -- --ignored"]
fn a_generated_top_down_project_packages_to_the_linux_layout() {
    package_template("top_down");
}
