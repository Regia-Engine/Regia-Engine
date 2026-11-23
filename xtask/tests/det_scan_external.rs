// SPDX-License-Identifier: MPL-2.0
//! External-workspace determinism scanning through the real command-line entry point.

use std::path::PathBuf;
use std::process::Command;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/det_scan_external")
}

#[test]
fn external_scan_finds_planted_min_max() {
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["det-scan", "--external"])
        .arg(fixture())
        .args(["--crate", "planted", "--json"])
        .output()
        .expect("launch xtask");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("std_float_fn") && stdout.contains("min") && stdout.contains("max"),
        "external scan must report planted min/max; status={}, stdout={stdout}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(1));
}

fn scan(root: &std::path::Path, args: &[&str]) -> (std::process::Output, serde_json::Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["det-scan", "--external"])
        .arg(root)
        .args(args)
        .arg("--json")
        .output()
        .expect("launch xtask");
    let report = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON: {error}; stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output, report)
}

#[test]
fn external_scan_finds_every_rule_and_preserves_lines() {
    let (output, report) = scan(&fixture(), &["--crate", "planted"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(report["closure"], serde_json::Value::Null);
    assert_eq!(report["files"], 1);
    assert_eq!(report["problems"], serde_json::json!([]));
    let findings = report["findings"].as_array().expect("findings");
    assert_eq!(findings.len(), 18, "{report}");
    for rule in xtask::det_scan::RULES {
        assert!(
            findings.iter().any(|f| f["rule"] == *rule),
            "missing {rule}: {report}"
        );
    }
    for (index, finding) in findings.iter().enumerate() {
        assert_eq!(finding["line"], index + 3);
        assert_eq!(finding["file"], "planted/src/lib.rs");
    }
    for method in ["min", "max", "clamp", "sin"] {
        assert!(
            findings
                .iter()
                .any(|f| f["rule"] == "std_float_fn" && f["what"] == method)
        );
    }
}

#[test]
fn external_scan_honors_reasoned_waivers_for_every_rule() {
    let (output, report) = scan(&fixture(), &["--crate", "waived"]);
    assert!(output.status.success(), "{report}");
    assert_eq!(report["ok"], true);
    assert_eq!(report["files"], 1);
    assert_eq!(report["findings"], serde_json::json!([]));
}

#[test]
fn repeated_crate_selection_is_deduplicated_and_sorted() {
    let (output, report) = scan(
        &fixture(),
        &["--crate", "waived", "--crate", "planted", "--crate", "waived"],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(report["crates"], serde_json::json!(["planted", "waived"]));
    assert_eq!(report["files"], 2);
    assert_eq!(report["findings"].as_array().expect("findings").len(), 18);
}

fn write(root: &std::path::Path, relative: &str, text: &str) {
    let file = root.join(relative);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    regia_test_utils::write_atomic(&file, text.as_bytes()).expect("write fixture");
}

fn workspace() -> regia_test_utils::TempDir {
    let dir = regia_test_utils::TempDir::new("det-scan-external");
    write(&dir, "Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
    write(
        &dir,
        "crates/tick/Cargo.toml",
        "[package]\nname = \"game-tick\"\nversion = \"0.0.0\"\n",
    );
    write(&dir, "crates/tick/src/lib.rs", "pub fn tick() {}\n");
    dir
}

#[test]
fn selects_package_name_from_member_glob_not_directory_name() {
    let dir = workspace();
    let (output, report) = scan(&dir, &["--crate", "game-tick"]);
    assert!(output.status.success(), "{report}");
    assert_eq!(report["crates"], serde_json::json!(["crates/tick"]));
    let (output, report) = scan(&dir, &["--crate", "tick"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(report["problems"].to_string().contains("tick"));
}

#[test]
fn math_module_exempts_only_direct_libm_in_that_file() {
    let dir = workspace();
    write(
        &dir,
        "crates/tick/src/math.rs",
        "fn sin(x: f32) { libm::sinf(x); }\nfn bad(x: f32) { x.sin(); }\n",
    );
    write(
        &dir,
        "crates/tick/src/other.rs",
        "fn sin(x: f32) { libm::sinf(x); }\n",
    );
    let (output, report) = scan(
        &dir,
        &["--crate", "game-tick", "--math-module", "crates/tick/src/math.rs"],
    );
    assert_eq!(output.status.code(), Some(1));
    let findings = report["findings"].as_array().expect("findings");
    assert_eq!(findings.len(), 2, "{report}");
    assert!(
        findings
            .iter()
            .any(|f| f["file"] == "crates/tick/src/math.rs" && f["rule"] == "std_float_fn")
    );
    assert!(
        findings
            .iter()
            .any(|f| f["file"] == "crates/tick/src/other.rs" && f["rule"] == "direct_libm")
    );
}

#[test]
fn missing_waiver_reason_still_fails() {
    let dir = workspace();
    write(
        &dir,
        "crates/tick/src/lib.rs",
        "fn tick() { let _ = 0.0f32.max(-0.0); } // det-scan: allow(std_float_fn)\n",
    );
    let (output, report) = scan(&dir, &["--crate", "game-tick"]);
    assert_eq!(output.status.code(), Some(1));
    let findings = report["findings"].as_array().expect("findings");
    assert_eq!(findings.len(), 2);
    assert_eq!(findings[0]["rule"], "allow_without_reason");
    assert_eq!(findings[1]["rule"], "std_float_fn");
}

#[test]
fn relative_external_root_is_resolved_from_the_process_working_directory() {
    let dir = workspace();
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .current_dir(&*dir)
        .args(["det-scan", "--external", ".", "--crate", "game-tick", "--json"])
        .output()
        .expect("launch xtask");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("report");
    assert_eq!(report["crates"], serde_json::json!(["crates/tick"]));
    assert_eq!(report["files"], 1);
}

#[test]
fn with_closure_is_opt_in_and_checks_clippy_dependencies_and_pins() {
    let dir = workspace();
    write(
        &dir,
        "crates/tick/Cargo.toml",
        "[package]\nname = \"game-tick\"\n[dependencies]\nhelper = { path = \"../helper\" }\n",
    );
    write(&dir, "crates/helper/Cargo.toml", "[package]\nname = \"helper\"\n");
    write(&dir, "crates/helper/src/lib.rs", "");
    write(&dir, "crates/tick/src/lib.rs", "fn tick(x: f32) { x.sin(); }\n");
    write(
        &dir,
        "docs/ci/lockstep-closure.toml",
        "math_module = \"crates/tick/src/lib.rs\"\n[[crate]]\npath = \"crates/tick\"\nwhy = \"tick\"\n[[pin]]\npath = \"crates/tick/src/lib.rs\"\ntext = \"fn tick(x: f32) { x.sin(); }\"\nrule = \"std_float_fn\"\nwhy = \"synthetic as-is pin\"\n",
    );
    let (_, report) = scan(&dir, &["--crate", "game-tick"]);
    assert_eq!(report["problems"], serde_json::json!([]));
    assert_eq!(report["findings"].as_array().expect("findings").len(), 1);
    let (output, report) = scan(&dir, &["--crate", "game-tick", "--with-closure"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(report["findings"], serde_json::json!([]));
    assert!(report["problems"].to_string().contains("helper"), "{report}");
    assert!(report["problems"].to_string().contains("clippy.toml"), "{report}");
    assert_eq!(report["closure"], xtask::det_scan::CLOSURE);
    let methods = xtask::det_scan::clippy_paths()
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(",");
    write(
        &dir,
        "crates/tick/clippy.toml",
        &format!(
            "disallowed-methods = [{methods}]\ndisallowed-types = [\"std::collections::hash_map::RandomState\", \"std::thread::Builder\"]\n"
        ),
    );
    write(
        &dir,
        "crates/tick/Cargo.toml",
        "[package]\nname = \"game-tick\"\n",
    );
    let (output, report) = scan(&dir, &["--crate", "game-tick", "--with-closure"]);
    assert!(output.status.success(), "{report}");
    write(&dir, "crates/tick/src/lib.rs", "fn tick() {}\n");
    let (output, report) = scan(&dir, &["--crate", "game-tick", "--with-closure"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(report["problems"].to_string().contains("matched 0"), "{report}");
}

#[test]
fn path_escape_in_member_or_math_module_is_refused() {
    let dir = workspace();
    for path in ["../outside.rs", "/outside.rs", "crates/tick/src/missing.rs"] {
        let (output, report) = scan(&dir, &["--crate", "game-tick", "--math-module", path]);
        assert_eq!(output.status.code(), Some(1), "{path}: {report}");
        assert_eq!(report["ok"], false);
    }
    write(&dir, "Cargo.toml", "[workspace]\nmembers = [\"../outside\"]\n");
    let (output, report) = scan(&dir, &["--crate", "game-tick"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(report["problems"].to_string().contains("outside"), "{report}");
}

#[cfg(unix)]
#[test]
fn symlinked_sources_manifests_and_directories_are_refused_without_reading() {
    use std::os::unix::fs::symlink;
    for path in [
        "Cargo.toml",
        "crates/tick/Cargo.toml",
        "crates/tick/src/lib.rs",
        "crates/tick/src/escape",
    ] {
        let dir = workspace();
        let destination = dir.join(path);
        if destination.is_file() {
            std::fs::remove_file(&destination).expect("replace fixture");
        }
        // A nonexistent target is deliberate: refusal must inspect the link itself, not its target.
        symlink("/det-scan-outside-root-must-never-be-opened", &destination).expect("symlink");
        let (output, report) = scan(&dir, &["--crate", "game-tick"]);
        assert_eq!(output.status.code(), Some(1));
        assert!(
            report["problems"].to_string().contains("symlink"),
            "{path}: {report}"
        );
    }
}

#[test]
fn root_package_implicit_members_and_workspace_dependencies() {
    let dir = workspace();
    write(
        &dir,
        "Cargo.toml",
        "[package]\nname = \"root-sim\"\n[workspace]\nmembers = []\n[workspace.dependencies]\ntick = { path = \"crates/tick\", package = \"game-tick\" }\n[dependencies]\ntick.workspace = true\nforeign = { path = \"../must-not-read\" }\n",
    );
    write(&dir, "src/lib.rs", "fn step() {}\n");
    let (output, report) = scan(&dir, &["--crate", "root-sim", "--crate", "game-tick"]);
    assert!(output.status.success(), "{report}");
    assert_eq!(report["crates"], serde_json::json!([".", "crates/tick"]));
    assert_eq!(report["files"], 2);
}

#[test]
fn member_excludes_character_classes_and_recursive_globs() {
    let dir = workspace();
    write(
        &dir,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/**/t[ia]c?\"]\nexclude = [\"crates/deep/tack\"]\n",
    );
    write(
        &dir,
        "crates/deep/tack/Cargo.toml",
        "[package]\nname = \"excluded\"\n",
    );
    let (output, report) = scan(&dir, &["--crate", "game-tick"]);
    assert!(output.status.success(), "{report}");
    let (output, report) = scan(&dir, &["--crate", "excluded"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(report["problems"].to_string().contains("unknown workspace crate"));
}

#[test]
fn custom_target_paths_and_no_sources_are_not_silently_skipped() {
    let dir = workspace();
    std::fs::remove_dir_all(dir.join("crates/tick/src")).expect("remove src");
    write(
        &dir,
        "crates/tick/Cargo.toml",
        "[package]\nname = \"game-tick\"\n[lib]\npath = \"tick.rs\"\n",
    );
    write(&dir, "crates/tick/tick.rs", "fn tick() { let _ = f32::sin; }\n");
    let (output, report) = scan(&dir, &["--crate", "game-tick"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(report["findings"][0]["file"], "crates/tick/tick.rs");
    write(
        &dir,
        "crates/tick/Cargo.toml",
        "[package]\nname = \"game-tick\"\n",
    );
    let (output, report) = scan(&dir, &["--crate", "game-tick"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(report["problems"].to_string().contains("no Rust source"));
}

#[test]
fn blanking_test_items_comments_literals_and_preceding_waivers_is_shared() {
    let dir = workspace();
    write(
        &dir,
        "crates/tick/src/lib.rs",
        r#"
// f32::sin
/* f32::max */
const WORDS: &str = "HashMap f32::sin";
#[cfg(test)]
mod tests { fn test() { f32::sin(0.5); } }
// det-scan: allow(std_float_fn): synthetic preceding-line waiver
fn waived() { f32::sin(0.5); }
fn live() { f32::sin(0.5); } // a trailing comment cannot hide this
"#,
    );
    let (output, report) = scan(&dir, &["--crate", "game-tick"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(report["findings"].as_array().expect("findings").len(), 1);
    assert_eq!(report["findings"][0]["line"], 9);
}

#[test]
fn invalid_flags_are_usage_errors() {
    for args in [
        vec!["det-scan", "--external", "."],
        vec!["det-scan", "--crate", "tick"],
        vec!["det-scan", "--with-closure"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(args)
            .output()
            .expect("launch");
        assert_eq!(output.status.code(), Some(2));
    }
}

#[cfg(unix)]
#[test]
fn default_scan_never_opens_closure_or_clippy_configs() {
    let dir = workspace();
    std::fs::create_dir_all(dir.join("docs/ci")).expect("mkdir");
    for path in [
        "clippy.toml",
        "crates/tick/clippy.toml",
        "docs/ci/lockstep-closure.toml",
    ] {
        std::os::unix::fs::symlink("/det-scan-outside-root-must-never-be-opened", dir.join(path))
            .expect("symlink");
    }
    let (output, report) = scan(&dir, &["--crate", "game-tick"]);
    assert!(output.status.success(), "{report}");
    let (output, report) = scan(&dir, &["--crate", "game-tick", "--with-closure"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(report["problems"].to_string().contains("symlink"));
}

#[test]
fn book_rule_table_is_generated_from_the_scanner_catalog() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace")
        .to_owned();
    let page = std::fs::read_to_string(root.join("book/agents/external-det-scan.md")).expect("book page");
    let table = page
        .split_once("<!-- det-scan-rules:start -->\n")
        .expect("start marker")
        .1
        .split_once("<!-- det-scan-rules:end -->")
        .expect("end marker")
        .0;
    assert_eq!(table, xtask::det_scan::rules_markdown());
    let lockstep = std::fs::read_to_string(root.join("book/agents/lockstep-math.md")).expect("lockstep page");
    let lockstep_table = lockstep
        .split_once("<!-- det-scan-rules:start -->\n")
        .expect("start marker")
        .1
        .split_once("<!-- det-scan-rules:end -->")
        .expect("end marker")
        .0;
    assert_eq!(
        lockstep_table, table,
        "both guides must use the generated catalog"
    );
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["det-scan", "--list-rules"])
        .output()
        .expect("launch");
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).expect("utf8"), table);
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["det-scan", "--list-rules", "--json"])
        .output()
        .expect("launch");
    assert!(output.status.success());
    let catalog: serde_json::Value = serde_json::from_slice(&output.stdout).expect("catalog");
    let names: Vec<_> = catalog["rules"]
        .as_array()
        .expect("rules")
        .iter()
        .map(|rule| rule["rule"].as_str().expect("name"))
        .collect();
    assert_eq!(names, xtask::det_scan::RULES);
}

#[test]
fn closure_checks_resolve_inherited_dependency_aliases() {
    let dir = workspace();
    write(
        &dir,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/tick\"]\n[workspace.dependencies]\nrenamed = { package = \"helper\", path = \"crates/helper\" }\n",
    );
    write(
        &dir,
        "crates/tick/Cargo.toml",
        "[package]\nname = \"game-tick\"\n[target.'cfg(unix)'.dependencies]\nrenamed.workspace = true\n",
    );
    write(&dir, "crates/helper/Cargo.toml", "[package]\nname = \"helper\"\n");
    write(&dir, "crates/helper/src/lib.rs", "fn helper() {}\n");
    write(
        &dir,
        "docs/ci/lockstep-closure.toml",
        "math_module = \"crates/tick/src/lib.rs\"\n[[crate]]\npath = \"crates/tick\"\nwhy = \"tick\"\n",
    );
    let (output, report) = scan(&dir, &["--crate", "helper"]);
    assert!(output.status.success(), "{report}");
    let (output, report) = scan(&dir, &["--crate", "game-tick", "--with-closure"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(report["problems"].to_string().contains("helper"), "{report}");
}

#[test]
fn duplicate_names_and_invalid_toml_fail_instead_of_skipping_packages() {
    let dir = workspace();
    write(
        &dir,
        "crates/duplicate/Cargo.toml",
        "[package]\nname = \"game-tick\"\n",
    );
    let (output, report) = scan(&dir, &["--crate", "game-tick"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        report["problems"]
            .to_string()
            .contains("duplicate workspace package")
    );
    write(&dir, "crates/duplicate/Cargo.toml", "[package\n");
    let (output, report) = scan(&dir, &["--crate", "game-tick"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(report["problems"].to_string().contains("parsing"));
}

#[test]
fn standalone_package_is_an_implicit_workspace_root() {
    let dir = regia_test_utils::TempDir::new("det-scan-standalone");
    write(
        &dir,
        "Cargo.toml",
        "[package]\nname = \"standalone\"\nversion = \"0.0.0\"\n",
    );
    write(&dir, "src/lib.rs", "pub fn tick() {}\n");
    let (output, report) = scan(&dir, &["--crate", "standalone"]);
    assert!(output.status.success(), "{report}");
    assert_eq!(report["crates"], serde_json::json!(["."]));
}

#[cfg(unix)]
#[test]
fn special_source_files_are_refused_before_opening() {
    let dir = workspace();
    let _socket =
        std::os::unix::net::UnixListener::bind(dir.join("crates/tick/src/socket.rs")).expect("socket");
    let (output, report) = scan(&dir, &["--crate", "game-tick"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        report["problems"].to_string().contains("not a regular file"),
        "{report}"
    );
}

#[cfg(unix)]
#[test]
fn closure_checks_do_not_follow_policy_or_clippy_symlinks() {
    for path in [
        "clippy.toml",
        "crates/tick/clippy.toml",
        "docs/ci/lockstep-closure.toml",
    ] {
        let dir = workspace();
        write(
            &dir,
            "docs/ci/lockstep-closure.toml",
            "math_module = \"crates/tick/src/lib.rs\"\n[[crate]]\npath = \"crates/tick\"\nwhy = \"tick\"\n",
        );
        let path = dir.join(path);
        if path.is_file() {
            std::fs::remove_file(&path).expect("replace fixture");
        }
        std::os::unix::fs::symlink("/det-scan-outside-root-must-never-be-opened", path).expect("symlink");
        let (output, report) = scan(&dir, &["--crate", "game-tick", "--with-closure"]);
        assert_eq!(output.status.code(), Some(1));
        assert!(report["problems"].to_string().contains("symlink"), "{report}");
    }
}

#[test]
fn closure_paths_cannot_escape_the_external_root() {
    let dir = workspace();
    write(
        &dir,
        "docs/ci/lockstep-closure.toml",
        "math_module = \"crates/tick/src/lib.rs\"\n[[crate]]\npath = \"../outside\"\nwhy = \"invalid fixture\"\n",
    );
    let (output, report) = scan(&dir, &["--crate", "game-tick", "--with-closure"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        report["problems"].to_string().contains("path must stay relative"),
        "{report}"
    );
}

#[test]
fn source_references_are_not_executed_or_followed() {
    let dir = workspace();
    write(
        &dir,
        "crates/tick/src/lib.rs",
        "#[path = \"/det-scan-outside-root-must-never-be-opened.rs\"]\nmod foreign;\nconst WORDS: &str = include_str!(\"/det-scan-outside-root-must-never-be-opened\");\n",
    );
    write(
        &dir,
        "crates/tick/build.rs",
        "compile_error!(\"the scanner must never build this crate\");\n",
    );
    let (output, report) = scan(&dir, &["--crate", "game-tick"]);
    assert!(output.status.success(), "{report}");
    assert_eq!(report["files"], 1);
}

#[cfg(unix)]
#[test]
fn excluded_members_are_pruned_before_symlink_inspection() {
    let dir = workspace();
    write(
        &dir,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/excluded\"]\n",
    );
    std::os::unix::fs::symlink(
        "/det-scan-outside-root-must-never-be-opened",
        dir.join("crates/excluded"),
    )
    .expect("symlink");
    let (output, report) = scan(&dir, &["--crate", "game-tick"]);
    assert!(output.status.success(), "{report}");
    assert_eq!(report["files"], 1);
}

#[test]
fn external_host_rules_have_json_names_and_reasonless_allows_keep_findings() {
    let dir = workspace();
    for (rule, source) in [
        ("wall_clock", "Instant::now();"),
        ("os_random", "RandomState::new();"),
        ("env_read", "std::env::args();"),
        ("address_order", "let key = x.as_ptr() as usize;"),
        ("thread_spawn", "tokio::spawn(step());"),
        ("arch_cfg", "let enabled = cfg!(target_feature = \"sse2\");"),
        ("float_sort", "v.sort_by(|a,b| a.partial_cmp(b).unwrap());"),
    ] {
        write(
            &dir,
            "crates/tick/src/lib.rs",
            &format!("// det-scan: allow({rule}):\n{source}\n"),
        );
        let (output, report) = scan(&dir, &["--crate", "game-tick"]);
        assert_eq!(output.status.code(), Some(1));
        let findings = report["findings"].as_array().expect("findings");
        assert!(findings.iter().any(|f| f["rule"] == rule), "{rule}: {report}");
        assert!(
            findings.iter().any(|f| f["rule"] == "allow_without_reason"),
            "{rule}: {report}"
        );
    }
}

#[test]
fn external_closure_rejects_missing_host_type_bans() {
    let dir = workspace();
    write(
        &dir,
        "docs/ci/lockstep-closure.toml",
        "math_module = \"crates/tick/src/lib.rs\"\n[[crate]]\npath = \"crates/tick\"\nwhy = \"synthetic tick\"\n",
    );
    let methods = xtask::det_scan::clippy_paths()
        .iter()
        .map(|p| format!("{p:?}"))
        .collect::<Vec<_>>()
        .join(",");
    write(
        &dir,
        "crates/tick/clippy.toml",
        &format!("disallowed-methods = [{methods}]\n"),
    );
    let (output, report) = scan(&dir, &["--crate", "game-tick", "--with-closure"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        report["problems"]
            .to_string()
            .contains("std::collections::hash_map::RandomState"),
        "{report}"
    );
    assert!(
        report["problems"].to_string().contains("std::thread::Builder"),
        "{report}"
    );
}
