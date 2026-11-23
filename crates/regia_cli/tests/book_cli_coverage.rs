// SPDX-License-Identifier: MPL-2.0
//! Spec 96 (ADR-1704): every top-level `regia` subcommand the real binary offers has a row in the
//! CLI table of `book/COVERAGE.md`. The runtime side of `cargo xtask book-coverage`, which reads
//! the same list from `enum CliCommand`.

use std::process::Command;

/// The subcommand names in the `Commands:` block of `regia -h` (clap's own `help` left out).
fn subcommands(help: &str) -> Vec<String> {
    help.lines()
        .skip_while(|line| line.trim() != "Commands:")
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        // An entry is indented two spaces; a wrapped description is indented further.
        .filter(|line| line.starts_with("  ") && !line.starts_with("   "))
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| *name != "help")
        .map(str::to_string)
        .collect()
}

#[test]
fn help_parsing_reads_the_commands_block() {
    let help = "Regia\n\nUsage: regia <COMMAND>\n\nCommands:\n  agent    Talk\n  install-android  Package,\n                   and more\n  help     Print\n\nOptions:\n  -h, --help\n";
    assert_eq!(subcommands(help), ["agent", "install-android"]);
}

#[test]
fn every_regia_subcommand_has_a_book_row() {
    let out = Command::new(env!("CARGO_BIN_EXE_regia"))
        .arg("-h")
        .output()
        .expect("run regia -h");
    assert!(
        out.status.success(),
        "regia -h failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let help = String::from_utf8_lossy(&out.stdout);
    let commands = subcommands(&help);
    assert!(
        commands.len() > 10,
        "parsed the command list: {commands:?}\n{help}"
    );
    let root = regia_test_utils::book::workspace_root();
    let coverage = std::fs::read_to_string(root.join("book/COVERAGE.md")).expect("read COVERAGE.md");
    let names: Vec<&str> = commands.iter().map(String::as_str).collect();
    let gaps = regia_test_utils::book::cli_gaps(&coverage, &names);
    assert!(gaps.is_empty(), "undocumented subcommands:\n{}", gaps.join("\n"));
}

/// The backticked words of `text` (`a`, `b` → `["a", "b"]`).
fn backticked(text: &str) -> Vec<String> {
    text.split('`').skip(1).step_by(2).map(str::to_string).collect()
}

/// audit-v035-docs H4: the CLI reference documented 10 of the subcommands (`regia run`, `server`,
/// `test`, `engine`, ... were nowhere). Every top-level subcommand of `regia -h` has a
/// `` `regia <name>`` heading or row in `book/reference/cli.md`.
#[test]
fn every_regia_subcommand_is_in_the_cli_reference() {
    let out = Command::new(env!("CARGO_BIN_EXE_regia"))
        .arg("-h")
        .output()
        .expect("run regia -h");
    let help = String::from_utf8_lossy(&out.stdout);
    let commands = subcommands(&help);
    assert!(commands.len() > 10, "parsed the command list: {commands:?}");
    let root = regia_test_utils::book::workspace_root();
    let reference = std::fs::read_to_string(root.join("book/reference/cli.md")).expect("read cli.md");
    let missing: Vec<&String> = commands
        .iter()
        .filter(|name| !reference.contains(&format!("`regia {name}")))
        .collect();
    assert!(
        missing.is_empty(),
        "subcommands missing from book/reference/cli.md: {missing:?}"
    );
}

/// `bake-lightmaps` always prints its structured JSON error. Invalid GPU arguments win over a
/// missing scene and leave an existing output untouched.
#[test]
fn bake_lightmaps_cli_rejects_before_scene_load_and_preserves_output() {
    let dir = regia_test_utils::unique_temp_dir("cli-lightmap-invalid-settings");
    let out_path = dir.join("existing.flmap");
    let sentinel = b"sentinel lightmap";
    std::fs::write(&out_path, sentinel).expect("write sentinel");
    let output = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args([
            "bake-lightmaps",
            "missing-scene.bake.ron",
            "--gpu",
            "--gpu-tile-rays",
            "4095",
            "--out",
        ])
        .arg(&out_path)
        .output()
        .expect("run bake-lightmaps");
    assert!(!output.status.success(), "invalid settings must fail");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is structured JSON: {error}: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(value["ok"], false);
    assert_eq!(value["code"], "lightmap_bake_settings");
    assert_eq!(value["path"], "gpu_tile_rays");
    assert!(
        value.get("report").is_none(),
        "a rejected command is not a completed bake"
    );
    assert!(value.get("message").and_then(serde_json::Value::as_str).is_some());
    assert_eq!(std::fs::read(&out_path).expect("sentinel remains"), sentinel);
    let names: Vec<_> = std::fs::read_dir(&dir)
        .expect("list output directory")
        .map(|entry| entry.expect("directory entry").file_name())
        .collect();
    assert_eq!(names, [out_path.file_name().expect("output name").to_os_string()]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// audit-v035-docs H1: the reference listed 3 of the 6 `regia new` templates. Every template id
/// the binary accepts (the list its unknown-template error prints) is named in the reference.
#[test]
fn every_new_template_is_in_the_cli_reference() {
    let dir = regia_test_utils::unique_temp_dir("cli-templates");
    let out = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["new", "Probe", "--template", "no-such-template", "--path"])
        .arg(&dir)
        .output()
        .expect("run regia new");
    assert!(!out.status.success(), "an unknown template is refused");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let available = stderr
        .split("available:")
        .nth(1)
        .unwrap_or_else(|| panic!("the error lists the templates: {stderr}"));
    let ids = backticked(available);
    assert!(ids.len() >= 6, "parsed the template ids: {ids:?} from {stderr}");
    let root = regia_test_utils::book::workspace_root();
    let reference = std::fs::read_to_string(root.join("book/reference/cli.md")).expect("read cli.md");
    let section = reference
        .split("### `regia new")
        .nth(1)
        .and_then(|rest| rest.split("\n### ").next())
        .expect("a `regia new` section");
    let missing: Vec<&String> = ids
        .iter()
        .filter(|id| !section.contains(&format!("`{id}`")))
        .collect();
    assert!(
        missing.is_empty(),
        "templates missing from the `regia new` reference: {missing:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The dynamically attached USDA subcommand has its own guide and coverage row.
#[test]
fn export_usda_has_help_and_a_book_row() {
    let out = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["export", "usda", "--help"])
        .output()
        .expect("run regia export usda --help");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--glb"), "{help}");
    assert!(help.contains("--out"), "{help}");
    let root = regia_test_utils::book::workspace_root();
    let coverage = std::fs::read_to_string(root.join("book/COVERAGE.md")).expect("coverage");
    assert!(
        coverage
            .lines()
            .any(|line| { line.contains("`regia export usda`") && line.contains("agents/export-usda.md") })
    );
    let guide = std::fs::read_to_string(root.join("book/agents/export-usda.md")).expect("guide");
    for code in ["usda_glb_missing", "usda_glb_invalid", "usda_empty", "usda_write"] {
        assert!(guide.contains(code), "guide must document {code}");
    }
}

/// ADR-8694: `regia rules check` exits non-zero on a bad rule set and prints each error with its
/// stable code and field path; a good file in the same call prints `ok` with its skeleton.
#[test]
fn rules_check_cli_reports_errors_with_codes() {
    let dir = regia_test_utils::unique_temp_dir("rules-check-cli");
    let write = |name: &str, text: &str| {
        let path = dir.join(name);
        let tmp = dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, text).expect("write");
        std::fs::rename(&tmp, &path).expect("rename");
        path
    };
    let good = write(
        "long.rules.ron",
        "(format_version: 1, id: \"ctf_long\", extends: \"ctf\", session: (objectives: (capture_limit: 5)))",
    );
    let bad = write(
        "bad.rules.ron",
        "(format_version: 1, id: \"bad\", skeleton: \"ctf\", options: [(key: \"limit\", kind: int, default: \"20\", min: \"1\", max: \"10\")])",
    );
    let cycle = write(
        "cycle.rules.ron",
        "(format_version: 1, id: \"loop_a\", extends: \"loop_a\")",
    );
    let run = |args: &[&std::path::Path], json: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_regia"));
        command.args(["rules", "check", "--project"]).arg(&dir);
        if json {
            command.arg("--json");
        }
        command.args(args).output().expect("run regia rules check")
    };
    let out = run(&[&good], false);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("ok ") && text.contains("ctf_long (ctf, content hash "),
        "{text}"
    );

    let out = run(&[&good, &bad, &cycle], false);
    assert!(!out.status.success(), "a bad file fails the command");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("ruleset_option_range at bad: options[0].default"),
        "{text}"
    );
    assert!(text.contains("ruleset_cycle at loop_a: extends"), "{text}");

    let out = run(&[&good, &bad, &cycle], true);
    assert!(!out.status.success());
    let reports: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    let codes: Vec<Option<&str>> = reports
        .as_array()
        .expect("array")
        .iter()
        .map(|r| r["code"].as_str())
        .collect();
    assert_eq!(codes, [None, Some("ruleset_option_range"), Some("ruleset_cycle")]);
    let _ = std::fs::remove_dir_all(&dir);
}
