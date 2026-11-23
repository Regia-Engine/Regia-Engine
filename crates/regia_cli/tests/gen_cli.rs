// SPDX-License-Identifier: MPL-2.0
//! The real `regia gen` binary (ADR-2412): `gen run` twice leaves the project byte-identical,
//! `gen verify` passes after a run, and fails with exit 1 and a diff list after a hand edit.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const GROUND: &str = r#"
format_version = 1

[generator]
kind = "heightfield"
seed = 5
description = "cli ground"
outputs = ["assets/gen/ground.r16"]

[generator.heightfield]
width = 40
height = 24
"#;

/// A scratch project, removed at the end of the test.
struct Project(PathBuf);

impl Project {
    fn new(tag: &str) -> Self {
        let dir = regia_test_utils::unique_temp_dir(tag);
        std::fs::create_dir_all(dir.join("generators")).expect("generators");
        std::fs::write(
            dir.join("Regia.toml"),
            "[project]\nname = \"Gen\"\nid = \"com.example.gen\"\n",
        )
        .expect("manifest");
        std::fs::write(dir.join("generators/ground.toml"), GROUND).expect("generator");
        Self(dir)
    }

    /// Every file outside `.regia/`, sorted, with its bytes.
    fn snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            let mut entries: Vec<_> = std::fs::read_dir(dir)
                .expect("read dir")
                .filter_map(Result::ok)
                .collect();
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                let path = entry.path();
                if path.file_name().is_some_and(|n| n == ".regia") {
                    continue;
                }
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    let rel = path.strip_prefix(root).expect("under root").to_path_buf();
                    out.push((rel, std::fs::read(&path).expect("read")));
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.0, &self.0, &mut out);
        out
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn regia(project: &Project, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(args)
        .arg("--project")
        .arg(&project.0)
        .output()
        .expect("run regia")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn gen_run_twice_is_byte_identical() {
    let project = Project::new("cli-gen-twice");
    let first = regia(&project, &["gen", "run", "ground"]);
    assert!(first.status.success(), "{}", text(&first));
    assert!(text(&first).contains("1 written"), "{}", text(&first));
    let after_first = project.snapshot();
    assert!(
        after_first.iter().any(|(p, _)| p.ends_with("ground.r16.meta")),
        "the output has a .meta"
    );
    let second = regia(&project, &["gen", "run", "--all"]);
    assert!(second.status.success(), "{}", text(&second));
    assert!(
        text(&second).contains("0 written, 1 unchanged"),
        "{}",
        text(&second)
    );
    assert_eq!(
        after_first,
        project.snapshot(),
        "byte-identical after the second run"
    );
}

#[test]
fn gen_verify_catches_a_hand_edited_output() {
    let project = Project::new("cli-gen-verify");
    assert!(regia(&project, &["gen", "run", "ground"]).status.success());
    let ok = regia(&project, &["gen", "verify"]);
    assert!(ok.status.success(), "{}", text(&ok));
    assert!(text(&ok).contains("ground: ok (1 file(s))"), "{}", text(&ok));

    let output = project.0.join("assets/gen/ground.r16");
    let mut bytes = std::fs::read(&output).expect("read");
    bytes[10] = bytes[10].wrapping_add(1);
    std::fs::write(&output, &bytes).expect("hand edit");

    let failed = regia(&project, &["gen", "verify"]);
    assert_eq!(failed.status.code(), Some(1), "{}", text(&failed));
    let all = text(&failed);
    assert!(all.contains("ground: DIFFERS"), "{all}");
    assert!(
        all.contains("assets/gen/ground.r16 differs (first differing byte 10)"),
        "{all}"
    );
    assert!(all.contains("gen_verify_failed"), "{all}");

    let json = regia(&project, &["gen", "verify", "ground", "--json"]);
    assert_eq!(json.status.code(), Some(1));
    let line = String::from_utf8_lossy(&json.stdout);
    let report: serde_json::Value = serde_json::from_str(line.trim()).expect("one JSON document");
    assert_eq!(report["ok"], false);
    assert_eq!(report["diffs"][0]["kind"], "changed");
    assert_eq!(report["diffs"][0]["path"], "assets/gen/ground.r16");
    assert_eq!(
        std::fs::read(&output).expect("read"),
        bytes,
        "verify never writes"
    );
}

#[test]
fn gen_list_and_errors_use_the_stable_codes() {
    let project = Project::new("cli-gen-list");
    let listed = regia(&project, &["gen", "list", "--json"]);
    assert!(listed.status.success(), "{}", text(&listed));
    let value: serde_json::Value = serde_json::from_slice(&listed.stdout).expect("json");
    assert_eq!(value["generators"][0]["name"], "ground");
    assert_eq!(value["generators"][0]["seed"], 5);

    let missing = regia(&project, &["gen", "run", "nope"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(text(&missing).contains("gen_not_found"), "{}", text(&missing));
    let usage = regia(&project, &["gen", "run"]);
    assert!(!usage.status.success(), "a name or --all is required");
}
