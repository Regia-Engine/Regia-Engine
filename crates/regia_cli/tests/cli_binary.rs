// SPDX-License-Identifier: MPL-2.0
//! Black-box tests of the `regia` binary: exit codes, error messages and on-disk effects of
//! the cheap commands (`new`, `upgrade-project`, `cook`, `agent call`, `steam upload`).
//!
//! Every test runs the real binary in a unique temp directory and asserts on the exit status and
//! on text that is part of the command's contract (the error names the thing to fix).

use std::path::Path;
use std::process::{Command, Output};

use regia_test_utils::{unique_temp_dir, write_atomic};

/// Runs `regia <args>` with a scrubbed Steam environment and returns the captured output.
fn regia(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(args)
        .env_remove("STEAM_USERNAME")
        .env_remove("STEAM_PASSWORD")
        .env_remove("STEAM_TOTP")
        .output()
        .expect("spawn the regia binary")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("temp paths are UTF-8")
}

#[test]
fn version_flag_succeeds_and_names_the_tool() {
    let out = regia(&["--version"]);
    assert!(out.status.success());
    assert!(stdout(&out).starts_with("regia "), "{}", stdout(&out));
}

#[test]
fn no_subcommand_and_unknown_subcommand_are_usage_errors() {
    let none = regia(&[]);
    assert_eq!(none.status.code(), Some(2), "{}", stderr(&none));
    let unknown = regia(&["frobnicate"]);
    assert_eq!(unknown.status.code(), Some(2), "{}", stderr(&unknown));
}

#[test]
fn new_prints_the_project_root_and_refuses_to_overwrite_it() {
    let parent = unique_temp_dir("cli_new_twice");
    let first = regia(&[
        "new",
        "Tidy Game",
        "--template",
        "quick",
        "--path",
        path_str(&parent),
    ]);
    assert!(first.status.success(), "{}", stderr(&first));
    let root = parent.join("tidy_game");
    assert_eq!(stdout(&first).trim(), root.display().to_string());
    assert!(root.join("Cargo.toml").is_file());

    let marker = root.join("Cargo.toml");
    let before = std::fs::read(&marker).expect("read manifest");
    let second = regia(&[
        "new",
        "Tidy Game",
        "--template",
        "quick",
        "--path",
        path_str(&parent),
    ]);
    assert_eq!(second.status.code(), Some(1));
    assert!(stderr(&second).contains("already exists"), "{}", stderr(&second));
    assert_eq!(
        std::fs::read(&marker).expect("read manifest again"),
        before,
        "a refused `new` leaves the project untouched"
    );
    let _ = std::fs::remove_dir_all(&parent);
}

#[test]
fn new_rejects_bad_templates_and_names_without_creating_anything() {
    let parent = unique_temp_dir("cli_new_bad");
    let template = regia(&["new", "Game", "--template", "nope", "--path", path_str(&parent)]);
    assert_eq!(template.status.code(), Some(1));
    assert!(
        stderr(&template).contains("unknown template"),
        "{}",
        stderr(&template)
    );

    let name = regia(&[
        "new",
        "9lives",
        "--template",
        "quick",
        "--path",
        path_str(&parent),
    ]);
    assert_eq!(name.status.code(), Some(1));
    assert!(
        stderr(&name).contains("not a valid crate name"),
        "{}",
        stderr(&name)
    );

    let entries = std::fs::read_dir(&parent).expect("read parent").count();
    assert_eq!(entries, 0, "failed `new` runs create no directories");
    let _ = std::fs::remove_dir_all(&parent);
}

#[test]
fn upgrade_project_dry_run_lists_files_and_changes_nothing() {
    let project = unique_temp_dir("cli_upgrade_dry");
    let cue = project.join("impact.cue.ron");
    write_atomic(&cue, b"( name: \"impact\", sounds: [] )").expect("write cue");
    let before = std::fs::read(&cue).expect("read cue");

    let out = regia(&["upgrade-project", "--project", path_str(&project)]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stderr(&out);
    assert!(text.contains("impact.cue.ron"), "{text}");
    assert!(text.contains("v1 -> v3"), "{text}");
    assert!(text.contains("1 need migration"), "{text}");
    assert_eq!(std::fs::read(&cue).expect("read cue again"), before);
    assert!(
        !project.join("impact.cue.ron.bak").exists(),
        "dry run writes no backup"
    );
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn upgrade_project_write_migrates_with_a_backup_and_is_idempotent() {
    let project = unique_temp_dir("cli_upgrade_write");
    let cue = project.join("impact.cue.ron");
    let original = b"// kept\n( name: \"impact\", sounds: [] )";
    write_atomic(&cue, original).expect("write cue");

    let first = regia(&["upgrade-project", "--project", path_str(&project), "--write"]);
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(
        stderr(&first).contains("migrated 1 file(s), backed up 1"),
        "{}",
        stderr(&first)
    );
    let migrated = std::fs::read_to_string(&cue).expect("read migrated cue");
    assert!(migrated.starts_with("// kept\n("), "{migrated}");
    assert!(migrated.contains("version: 3"), "{migrated}");
    assert_eq!(
        std::fs::read(project.join("impact.cue.ron.bak")).expect("backup exists"),
        original,
        "the backup is the pre-migration bytes"
    );

    let second = regia(&["upgrade-project", "--project", path_str(&project), "--write"]);
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(
        stderr(&second).contains("migrated 0 file(s), backed up 0"),
        "{}",
        stderr(&second)
    );
    assert_eq!(
        std::fs::read_to_string(&cue).expect("read cue after rerun"),
        migrated,
        "a current file is not rewritten"
    );
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn upgrade_project_skips_files_from_a_newer_engine_without_touching_them() {
    let project = unique_temp_dir("cli_upgrade_newer");
    let cue = project.join("future.cue.ron");
    let future = b"( version: 99, name: \"future\", sounds: [] )";
    write_atomic(&cue, future).expect("write cue");

    let out = regia(&["upgrade-project", "--project", path_str(&project), "--write"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stderr(&out);
    assert!(text.contains("v99 is newer than engine v3; skipped"), "{text}");
    assert!(text.contains("1 newer version(s) skipped"), "{text}");
    assert_eq!(std::fs::read(&cue).expect("read cue"), future);
    assert!(!project.join("future.cue.ron.bak").exists());
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn upgrade_project_ignores_unrelated_files_and_target_directories() {
    let project = unique_temp_dir("cli_upgrade_ignore");
    write_atomic(&project.join("notes.txt"), b"not a format").expect("write notes");
    let target = project.join("target");
    std::fs::create_dir_all(&target).expect("create target");
    write_atomic(&target.join("stale.cue.ron"), b"( name: \"stale\" )").expect("write stale cue");

    let out = regia(&["upgrade-project", "--project", path_str(&project)]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("0 file(s), 0 need migration"),
        "{}",
        stderr(&out)
    );
    let _ = std::fs::remove_dir_all(&project);
}

/// ADR-5617 (review finding 5): a launcher that lost its `build.rs` but kept the
/// build-dependencies gets the script back for the icon and version info (ADR-2645), and the
/// stderr line says so; it is not the earlier-script update of ADR-5181.
#[test]
fn upgrade_project_names_the_icon_reason_when_it_adds_a_missing_build_script() {
    let project = unique_temp_dir("cli_upgrade_build_reason");
    write_atomic(
        &project.join("Regia.toml"),
        b"[project]\nname = \"G\"\nid = \"com.example.g\"\n",
    )
    .expect("write Regia.toml");
    write_atomic(
        &project.join("Cargo.toml"),
        b"[workspace]\nmembers = [\"launcher\"]\n",
    )
    .expect("write workspace");
    std::fs::create_dir_all(project.join("launcher/src")).expect("create launcher/src");
    let launcher = format!(
        "[package]\nname = \"g_launcher\"\nversion = \"0.1.0\"\n\n[dependencies]\nregia = {{ path = \"../engine/crates/regia\" }}\n\n{}",
        regia_templates::windows_build_dependencies_table()
    );
    write_atomic(&project.join("launcher/Cargo.toml"), launcher.as_bytes()).expect("write launcher");
    write_atomic(&project.join("launcher/src/main.rs"), b"fn main() {}\n").expect("write main");

    let out = regia(&["upgrade-project", "--project", path_str(&project), "--write"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stderr(&out);
    assert!(
        text.contains("added launcher/build.rs (the Windows exe icon and version info, ADR-2645)"),
        "{text}"
    );
    assert!(!text.contains("ADR-5181"), "{text}");
    assert_eq!(
        std::fs::read_to_string(project.join("launcher/build.rs")).expect("read build.rs"),
        regia_templates::WINDOWS_BUILD_SCRIPT
    );
    assert_eq!(
        std::fs::read_to_string(project.join("launcher/Cargo.toml")).expect("read launcher"),
        launcher,
        "the manifest already had its build-dependencies"
    );
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn upgrade_project_on_a_missing_directory_fails() {
    let project = unique_temp_dir("cli_upgrade_missing").join("does_not_exist");
    let out = regia(&["upgrade-project", "--project", path_str(&project)]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).starts_with("error:"), "{}", stderr(&out));
}

#[test]
fn cook_rejects_an_unknown_quality_before_touching_the_project() {
    let project = unique_temp_dir("cli_cook_quality");
    let out = regia(&["cook", "--project", path_str(&project), "--quality", "ultra"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("--quality must be `fast` or `final`, got `ultra`"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_dir(&project).expect("read project").count(),
        0,
        "no cache or build directory is created"
    );
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn cook_stats_reports_budgets_without_a_cooked_cache() {
    let project = unique_temp_dir("cli_cook_stats");
    let out = regia(&["cook", "--project", path_str(&project), "--stats"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("heightfield 4096x4096"),
        "the first block is the canonical reference budget, not the project's terrain:\n{text}"
    );
    assert!(
        stderr(&out).contains("stats: no cooked terrain"),
        "the project's own cache is the following line:\n{}",
        stderr(&out)
    );
    let _ = std::fs::remove_dir_all(&project);
}

/// The MiB figure on the `--stats` budget line that contains `label`.
fn budget_row_mib(text: &str, label: &str) -> String {
    let line = text
        .lines()
        .find(|l| l.contains(label))
        .unwrap_or_else(|| panic!("no `{label}` row in:\n{text}"));
    line.split_whitespace()
        .rev()
        .nth(1)
        .unwrap_or_else(|| panic!("no MiB figure in `{line}`"))
        .to_string()
}

/// fixrev-terrain-tiles-g (review finding 5 on ADR-5294): `--platform android` counts every
/// terrain slot at ASTC 4×4's 16 bytes per block. Desktop keeps BC4's 8 for AO and height.
#[test]
fn cook_stats_budget_counts_android_slots_at_astc_block_size() {
    use regia_asset_pipeline::{DEFAULT_TILE_SIZE, tiled_texture_bytes};
    let mib = |b: u64| format!("{:.2}", b as f64 / (1024.0 * 1024.0));
    let macro16 = tiled_texture_bytes(8192, DEFAULT_TILE_SIZE, 16);
    let macro8 = tiled_texture_bytes(8192, DEFAULT_TILE_SIZE, 8);
    let layer16 = tiled_texture_bytes(4096, DEFAULT_TILE_SIZE, 16);
    let layer8 = tiled_texture_bytes(4096, DEFAULT_TILE_SIZE, 8);
    for (platform, macros, layers) in [
        ("android", 3 * macro16, 6 * 4 * layer16),
        ("linux", 2 * macro16 + macro8, 6 * (3 * layer16 + layer8)),
    ] {
        let project = unique_temp_dir("cli_cook_stats_platform");
        let out = regia(&[
            "cook",
            "--project",
            path_str(&project),
            "--stats",
            "--platform",
            platform,
        ]);
        assert!(out.status.success(), "{platform}: {}", stderr(&out));
        let text = stdout(&out);
        assert_eq!(
            budget_row_mib(&text, "8K macro albedo/normal/AO"),
            mib(macros),
            "{platform} macro row:\n{text}"
        );
        assert_eq!(
            budget_row_mib(&text, "layers x 4 textures"),
            mib(layers),
            "{platform} layer row:\n{text}"
        );
        let _ = std::fs::remove_dir_all(&project);
    }
}

#[test]
fn agent_call_reports_bad_json_and_missing_projects_and_missing_apps() {
    let project = unique_temp_dir("cli_agent_call");

    let bad_json = regia(&[
        "agent",
        "call",
        "agent.hello",
        "{not json",
        "--project",
        path_str(&project),
    ]);
    assert_eq!(bad_json.status.code(), Some(1));
    assert!(
        stderr(&bad_json).contains("invalid PARAMS_JSON"),
        "{}",
        stderr(&bad_json)
    );

    let missing = project.join("nope");
    let no_dir = regia(&["agent", "call", "agent.hello", "--project", path_str(&missing)]);
    assert_eq!(no_dir.status.code(), Some(1));
    assert!(stderr(&no_dir).contains("not found"), "{}", stderr(&no_dir));

    let no_app = regia(&["agent", "call", "agent.hello", "--project", path_str(&project)]);
    assert_eq!(no_app.status.code(), Some(1));
    assert!(
        stderr(&no_app).contains("cannot connect to a running Regia Engine app"),
        "{}",
        stderr(&no_app)
    );
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn steam_upload_without_credentials_names_the_missing_variable() {
    let project = unique_temp_dir("cli_steam_creds");
    let out = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["steam", "upload", "--branch", "beta"])
        .current_dir(&project)
        .env_remove("STEAM_USERNAME")
        .env_remove("STEAM_PASSWORD")
        .output()
        .expect("spawn regia");
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("STEAM_USERNAME"), "{}", stderr(&out));

    let no_password = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["steam", "upload", "--branch", "beta"])
        .current_dir(&project)
        .env("STEAM_USERNAME", "builder")
        .env_remove("STEAM_PASSWORD")
        .output()
        .expect("spawn regia");
    assert_eq!(no_password.status.code(), Some(1));
    assert!(
        stderr(&no_password).contains("STEAM_PASSWORD"),
        "{}",
        stderr(&no_password)
    );
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn package_in_a_directory_without_a_manifest_names_the_missing_file() {
    let project = unique_temp_dir("cli_package_no_manifest");
    let out = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["package", "--platform", "linux"])
        .current_dir(&project)
        .output()
        .expect("spawn regia");
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("Regia.toml"), "{}", stderr(&out));
    let _ = std::fs::remove_dir_all(&project);
}

/// Spec 76 § 11 test 13: refused before anything is cooked or built.
#[test]
fn a_server_package_for_android_or_macos_is_refused_with_its_code() {
    let project = unique_temp_dir("cli_package_server_platform");
    for platform in ["android", "macos"] {
        let out = Command::new(env!("CARGO_BIN_EXE_regia"))
            .args(["package", "--server", "--platform", platform])
            .current_dir(&project)
            .output()
            .expect("spawn regia");
        assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
        assert!(
            stderr(&out).contains("server_platform_unsupported"),
            "{}",
            stderr(&out)
        );
        assert!(
            !stderr(&out).contains("Regia.toml"),
            "the platform is checked before the project: {}",
            stderr(&out)
        );
    }
    assert_eq!(
        std::fs::read_dir(&project).expect("dir").count(),
        0,
        "nothing was written"
    );
    let _ = std::fs::remove_dir_all(&project);
}

/// ADR-2710: `regia assets chunks --max-size` is a CI gate: under budget exit 0, over budget a
/// non-zero exit with the JSON report on stdout and `content_size_budget` on stderr; a bad size is
/// an error too. Measured as cooked (memory) bytes, so the fixture needs no pak.
#[test]
fn assets_chunks_max_size_gates_with_exit_codes_and_json() {
    let root = unique_temp_dir("cli-chunks-budget");
    write_atomic(
        &root.join("Regia.toml"),
        b"[project]\nname = \"T\"\nid = \"com.example.t\"\nversion = \"1.0.0\"\n\
          [cook.chunks.arena]\ninclude = [\"levels/arena/**\"]\ndelivery = \"on_demand\"\n",
    )
    .expect("toml");
    let cache = regia_asset::cooked_cache_dir(&root, "linux");
    std::fs::create_dir_all(&cache).expect("cache");
    let mut manifest = regia_asset::AssetManifest::default();
    for (path, bytes) in [
        ("assets/ui/menu.png", 300usize),
        ("assets/levels/arena/a.glb", 5000),
    ] {
        let id = regia_asset::AssetId::new_random();
        std::fs::write(cache.join(regia_asset::cooked_file_name(id)), vec![7u8; bytes]).expect("cooked");
        manifest.entries.push(regia_asset::ManifestEntry {
            path: path.into(),
            id,
            type_path: "regia_asset::CookedData".into(),
        });
    }
    manifest.write_to_cache(&cache).expect("manifest");
    let run = |max: &str| {
        regia(&[
            "assets",
            "chunks",
            "--project",
            path_str(&root),
            "--platform",
            "linux",
            "--measure",
            "memory",
            "--max-size",
            max,
            "--json",
        ])
    };
    let ok = run("1MiB");
    assert!(ok.status.success(), "{}", stderr(&ok));
    let report: serde_json::Value = serde_json::from_str(&stdout(&ok)).expect("json");
    assert_eq!(report["ok"], serde_json::json!(true));
    let over = run("4KiB");
    assert_eq!(over.status.code(), Some(1), "{}", stderr(&over));
    let report: serde_json::Value = serde_json::from_str(&stdout(&over)).expect("json on stdout");
    assert_eq!(report["over_budget"], serde_json::json!(["arena"]));
    assert_eq!(report["measure"], serde_json::json!("memory"));
    assert!(stderr(&over).contains("content_size_budget"), "{}", stderr(&over));
    let bad = run("lots");
    assert!(!bad.status.success());
    assert!(stderr(&bad).contains("--max-size"), "{}", stderr(&bad));
    let _ = std::fs::remove_dir_all(&root);
}
