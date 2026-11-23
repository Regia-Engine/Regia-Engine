// SPDX-License-Identifier: MPL-2.0
//! Exercises the stream report command's JSON and read-only CLI behavior.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        let dir = regia_test_utils::unique_temp_dir("stream-report-cli");
        std::fs::create_dir_all(dir.join("assets")).expect("assets directory");
        std::fs::write(
            dir.join("assets/open.level.ron"),
            format!(
                "(format_version: 1, entities: [(guid: \"{}\", components: {{ \
                 \"regia_scene::transform::Transform\": (translation: (10.0, 0.0, 10.0)) }})], resources: {{ \
                 \"regia_scene::streaming::LevelStreaming\": (enabled: true, hlod: None) }})",
                regia_core::id::Guid::from_u128(1)
            ),
        )
        .expect("streamed level");
        std::fs::write(dir.join("assets/plain.level.ron"), "(format_version: 1)").expect("plain level");
        Self(dir)
    }

    fn files(&self) -> Vec<PathBuf> {
        fn visit(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) {
            let mut entries: Vec<_> = std::fs::read_dir(dir)
                .expect("read directory")
                .map(Result::unwrap)
                .collect();
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                let path = entry.path();
                if path.is_dir() {
                    visit(root, &path, files);
                } else {
                    files.push(path.strip_prefix(root).expect("under project").to_path_buf());
                }
            }
        }
        let mut files = Vec::new();
        visit(&self.0, &self.0, &mut files);
        files
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

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn json_report_has_one_cell_and_does_not_write_files() {
    let project = Project::new();
    let before = project.files();
    let output = regia(
        &project,
        &["assets", "stream-report", "assets/open.level.ron", "--json"],
    );
    assert!(output.status.success(), "{}", output_text(&output));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON object");
    assert!(value.is_object(), "{value}");
    assert_eq!(value["cells"], 1);
    assert_eq!(project.files(), before, "stream-report must not write files");

    // Platform is only a cooked-cache directory name; an unknown value is accepted.
    let unknown_platform = regia(
        &project,
        &[
            "assets",
            "stream-report",
            "assets/open.level.ron",
            "--json",
            "--platform",
            "platform-that-does-not-exist",
        ],
    );
    assert!(
        unknown_platform.status.success(),
        "{}",
        output_text(&unknown_platform)
    );
    let value: serde_json::Value = serde_json::from_slice(&unknown_platform.stdout).expect("JSON object");
    assert_eq!(value["cells"], 1);
    assert_eq!(project.files(), before, "platform lookup must not write files");
}

#[test]
fn missing_level_fails_without_writing_files() {
    let project = Project::new();
    let before = project.files();
    let output = regia(
        &project,
        &["assets", "stream-report", "assets/missing.level.ron", "--json"],
    );
    assert!(!output.status.success(), "{}", output_text(&output));
    assert_eq!(project.files(), before, "failed report must not write files");
}
