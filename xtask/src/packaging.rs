// SPDX-License-Identifier: MPL-2.0
//! `check-packaging`: fail when a crate's compiled code reaches outside its own folder (ADR-1485).
//!
//! `cargo package` and `cargo vendor` copy only a crate's own directory. A library, binary or build
//! script that pulls in a file from elsewhere in the repository (`include_str!("../../../docs/...")`,
//! `include_bytes!` of a sibling crate's data, `#[path = "../x.rs"]`) does not compile from such a
//! copy. The check scans every workspace member's non-test Rust source:
//!
//! - `include_str!`, `include_bytes!` and `include!` with a literal path that resolves outside the
//!   member's directory, or with a `concat!(env!("CARGO_MANIFEST_DIR"), "/../...")` path;
//! - `#[path = "..."]` attributes that resolve outside it;
//! - in `crates/` (samples are games run from the checkout, not packaged): `env!("CARGO_MANIFEST_DIR")`
//!   joined with a `..` segment, except at the places listed in [`RUNTIME_LOOKUPS`] (a runtime
//!   lookup that fails cleanly when the repository is absent).
//!
//! Test code is exempt: files under `tests/`, `examples/`, `benches/`, files named `tests.rs`, and
//! everything from a `#[cfg(test)] mod` to the end of its file. `cargo package` builds only the
//! library, binaries and build script, so tests may read repo-level docs and goldens.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;

use crate::preflight::{Member, join_relative, members, rust_files};

/// Files that join `CARGO_MANIFEST_DIR` with `..` on purpose, and why. Each is a runtime lookup of
/// the engine checkout (never a compile-time dependency) that returns an error when it is absent.
pub const RUNTIME_LOOKUPS: &[(&str, &str)] = &[
    (
        "crates/regia_cli/src/new_project.rs",
        "`regia new` points the generated project at the engine checkout it was built from",
    ),
    (
        "crates/regia_editor/src/scaffold.rs",
        "the editor's New Project does the same as `regia new`",
    ),
    (
        "crates/regia_editor/src/extension/host.rs",
        "`default_engine_dir` finds the bundled `extensions/` of the engine checkout the editor was built from \
         (ADR-9850 (a)); `None` when that directory is absent, and `REGIA_ENGINE_DIR` overrides it",
    ),
];

/// The part of `text` that is not a `#[cfg(test)] mod` tail.
pub fn non_test_prefix(text: &str) -> &str {
    let mut offset = 0;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "#[cfg(test)]" {
            let next = lines[i + 1..]
                .iter()
                .map(|l| l.trim())
                .find(|l| !l.starts_with("#["));
            if next.is_some_and(|l| {
                l.starts_with("mod ") || l.starts_with("pub mod ") || l.starts_with("pub(crate) mod ")
            }) {
                return &text[..offset];
            }
        }
        offset += line.len();
    }
    text
}

/// Whether `rel` (a path relative to its member's directory, `/`-separated) is test-only code.
pub fn is_test_path(rel: &str) -> bool {
    let first = rel.split('/').next().unwrap_or("");
    first == "tests"
        || first == "examples"
        || first == "benches"
        || rel.rsplit('/').next() == Some("tests.rs")
}

fn line_of(text: &str, at: usize) -> usize {
    text[..at].matches('\n').count() + 1
}

fn on_comment_line(text: &str, at: usize) -> bool {
    let start = text[..at].rfind('\n').map_or(0, |n| n + 1);
    text[start..].trim_start().starts_with("//")
}

/// The problems in one Rust file. `file_dir` and `member_dir` are workspace-relative and
/// `/`-separated; `rel_in_member` is the file's path inside the member; `file_name` is used in
/// messages.
pub fn scan_file(
    member_dir: &str,
    file_dir: &str,
    rel_in_member: &str,
    file_name: &str,
    text: &str,
) -> Vec<String> {
    if is_test_path(rel_in_member) {
        return Vec::new();
    }
    let code = non_test_prefix(text);
    let inside =
        |resolved: Option<String>| resolved.is_some_and(|p| p.starts_with(&format!("{member_dir}/")));
    let mut out = Vec::new();
    for mac in ["include_str!", "include_bytes!", "include!"] {
        let mut from = 0;
        while let Some(found) = code[from..].find(mac) {
            let at = from + found;
            from = at + mac.len();
            if on_comment_line(code, at) {
                continue;
            }
            let Some(args) = code[from..].trim_start().strip_prefix('(') else {
                continue;
            };
            let args = args.trim_start();
            if let Some(literal) = args.strip_prefix('"') {
                let Some(end) = literal.find('"') else { continue };
                let path = &literal[..end];
                if !path.is_empty() && !inside(join_relative(file_dir, path)) {
                    out.push(format!(
                        "{file_name}:{}: {mac}(\"{path}\") reads outside {member_dir}/",
                        line_of(code, at)
                    ));
                }
            } else if args.starts_with("concat!") {
                let head: String = args.chars().take(200).collect();
                if head.contains("CARGO_MANIFEST_DIR") && head.contains("..") {
                    out.push(format!(
                        "{file_name}:{}: {mac}(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"..\")) reads outside {member_dir}/",
                        line_of(code, at)
                    ));
                }
            }
        }
    }
    let mut from = 0;
    while let Some(found) = code[from..].find("#[path") {
        let at = from + found;
        from = at + 6;
        let Some(rest) = code[from..].trim_start().strip_prefix('=') else {
            continue;
        };
        let Some(literal) = rest.trim_start().strip_prefix('"') else {
            continue;
        };
        let Some(end) = literal.find('"') else { continue };
        let path = &literal[..end];
        if !inside(join_relative(file_dir, path)) {
            out.push(format!(
                "{file_name}:{}: #[path = \"{path}\"] points outside {member_dir}/",
                line_of(code, at)
            ));
        }
    }
    let file_path = format!("{file_dir}/{file_name}");
    if member_dir.starts_with("crates/") && !RUNTIME_LOOKUPS.iter().any(|(allowed, _)| *allowed == file_path)
    {
        let lines: Vec<&str> = code.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if line.trim_start().starts_with("//") || !line.contains("CARGO_MANIFEST_DIR") {
                continue;
            }
            let window = lines[i..lines.len().min(i + 4)].join("\n");
            if window.contains("\"..") || window.contains("/..") {
                out.push(format!(
                    "{file_name}:{}: CARGO_MANIFEST_DIR joined with `..` in non-test code",
                    i + 1
                ));
            }
        }
    }
    out
}

/// Every problem in the workspace, as `path:line: message`, sorted.
pub fn check(root: &Path, members: &[Member]) -> Vec<String> {
    let mut problems = BTreeSet::new();
    for member in members {
        if member.dir.is_empty() || member.name == "xtask" {
            continue;
        }
        let dir = root.join(&member.dir);
        let nested: Vec<PathBuf> = members
            .iter()
            .filter(|m| m.dir.starts_with(&format!("{}/", member.dir)))
            .map(|m| root.join(&m.dir))
            .collect();
        let mut files = Vec::new();
        rust_files(&dir, &nested, &mut files);
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let (Ok(rel_file), Some(name)) = (file.strip_prefix(root), file.file_name()) else {
                continue;
            };
            let rel_file = rel_file.to_string_lossy().replace('\\', "/");
            let file_dir = rel_file.rsplit_once('/').map_or("", |(d, _)| d).to_string();
            let rel_in_member = rel_file
                .strip_prefix(&format!("{}/", member.dir))
                .unwrap_or(&rel_file)
                .to_string();
            for p in scan_file(
                &member.dir,
                &file_dir,
                &rel_in_member,
                &name.to_string_lossy(),
                &text,
            ) {
                problems.insert(format!("{file_dir}/{p}"));
            }
        }
    }
    problems.into_iter().collect()
}

/// `cargo xtask check-packaging`.
pub fn run(root: &Path) -> Result<ExitCode> {
    let members = members(root)?;
    let problems = check(root, &members);
    if problems.is_empty() {
        println!("check-packaging: ok");
        return Ok(ExitCode::SUCCESS);
    }
    for p in &problems {
        eprintln!("error: {p}");
    }
    eprintln!(
        "check-packaging: {} problem(s): `cargo package` and `cargo vendor` copy only a crate's own folder; \
         move the file into the crate (and keep a byte-identity test if docs/ also needs it), see ADR-1485",
        problems.len()
    );
    Ok(ExitCode::FAILURE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(text: &str) -> Vec<String> {
        scan_file("crates/a", "crates/a/src", "src/x.rs", "x.rs", text)
    }

    #[test]
    fn includes_inside_the_crate_pass() {
        assert!(scan("const A: &str = include_str!(\"../assets/a.ron\");\nconst B: &[u8] = include_bytes!(\"b.bin\");").is_empty());
    }

    #[test]
    fn includes_that_leave_the_crate_fail() {
        let found = scan("\n\nconst A: &str = include_str!(\"../../../docs/formats/a.ron\");");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("x.rs:3:"), "{found:?}");
        assert_eq!(
            scan("const A: &[u8] = include_bytes!(\"../../b/data.bin\");").len(),
            1
        );
        assert_eq!(scan("include!(\"../../gen.rs\");").len(), 1);
    }

    #[test]
    fn a_multi_line_call_is_found() {
        assert_eq!(
            scan("const A: &str = include_str!(\n    \"../../x.txt\",\n);").len(),
            1
        );
    }

    #[test]
    fn computed_manifest_paths_with_dotdot_fail() {
        let text = "const A: &str = include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../x\"));";
        assert!(!scan(text).is_empty());
        let ok = "const A: &str = include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/data/x\"));";
        assert!(scan(ok).is_empty());
    }

    #[test]
    fn path_attributes_are_checked() {
        assert_eq!(scan("#[path = \"../../shared.rs\"]\nmod shared;").len(), 1);
        assert!(
            scan_file(
                "crates/a",
                "crates/a",
                "build.rs",
                "build.rs",
                "#[path = \"src/s.rs\"]\nmod s;"
            )
            .is_empty()
        );
    }

    #[test]
    fn test_code_is_exempt() {
        let outside = "const A: &str = include_str!(\"../../../docs/a.ron\");";
        assert!(scan_file("crates/a", "crates/a/tests", "tests/t.rs", "t.rs", outside).is_empty());
        assert!(scan_file("crates/a", "crates/a/examples", "examples/e.rs", "e.rs", outside).is_empty());
        assert!(
            scan_file(
                "crates/a",
                "crates/a/src/b",
                "src/b/tests.rs",
                "tests.rs",
                outside
            )
            .is_empty()
        );
        let tail = format!("pub fn f() {{}}\n\n#[cfg(test)]\nmod tests {{\n    {outside}\n}}\n");
        assert!(scan(&tail).is_empty());
        let before = format!("{outside}\n#[cfg(test)]\nmod tests {{}}\n");
        assert_eq!(scan(&before).len(), 1);
    }

    #[test]
    fn comments_are_ignored() {
        assert!(
            scan("// include_str!(\"../../x\") was the old way\n/// `include_str!(\"../../x\")`").is_empty()
        );
    }

    #[test]
    fn manifest_dir_joined_with_dotdot_fails_unless_listed() {
        let text = "let p = Path::new(env!(\"CARGO_MANIFEST_DIR\")).join(\"../../config/x.ron\");";
        assert_eq!(scan(text).len(), 1);
        assert!(
            scan_file(
                "crates/regia_cli",
                "crates/regia_cli/src",
                "src/new_project.rs",
                "new_project.rs",
                text
            )
            .is_empty()
        );
        assert!(scan("let p = Path::new(env!(\"CARGO_MANIFEST_DIR\")).join(\"shaders\");").is_empty());
    }

    #[test]
    fn every_listed_runtime_lookup_names_an_existing_file_shape() {
        for (path, why) in RUNTIME_LOOKUPS {
            assert!(path.ends_with(".rs") && !why.is_empty(), "{path}");
        }
    }
}
