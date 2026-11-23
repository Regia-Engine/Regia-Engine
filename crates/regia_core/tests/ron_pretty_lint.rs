// SPDX-License-Identifier: MPL-2.0
//! Workspace lint: every pretty RON writer goes through `regia_core::ron_pretty()`.
//!
//! `ron::ser::PrettyConfig::default()` writes `\r\n` on Windows, which breaks content hashes,
//! diffs and golden comparisons across hosts. A direct `PrettyConfig::default`/`::new` outside
//! `crates/regia_core/src/ron_fmt.rs` fails this test, so new code cannot regress. So does a
//! `PrettyConfig` from `Default::default()` that does not pin `new_line` on the same or the next line.

use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &["PrettyConfig::default", "PrettyConfig::new"];
const ROOTS: &[&str] = &["crates", "samples", "xtask", "tools", "fuzz"];

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name == "target" || name == "vendor" || name.starts_with('.') {
                continue;
            }
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Zero-based indexes of the lines of `text` that build a `PrettyConfig` the lint refuses: a direct
/// `PrettyConfig::default`/`::new`, or a `PrettyConfig` from `Default::default()` (the type on the
/// binding, or `<PrettyConfig as Default>::default`) whose line and next line never call `new_line`.
/// The lockstep crates use the second form with `new_line` because they cannot depend on
/// `regia_core`.
fn offending_lines(text: &str) -> Vec<usize> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if FORBIDDEN.iter().any(|p| line.contains(p)) {
            out.push(i);
            continue;
        }
        let via_default = line.contains("PrettyConfig")
            && (line.contains("Default::default()") || line.contains("as Default>::default"));
        if via_default {
            let pinned =
                line.contains("new_line(") || lines.get(i + 1).is_some_and(|next| next.contains("new_line("));
            if !pinned {
                out.push(i);
            }
        }
    }
    out
}

#[test]
fn no_direct_pretty_config_construction_outside_the_helper() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let helper = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ron_fmt.rs");
    let this = Path::new(file!()).file_name().map(|n| n.to_os_string());
    let mut files = Vec::new();
    for r in ROOTS {
        collect(&root.join(r), &mut files);
    }
    let mut offenders = Vec::new();
    for file in files {
        if file.canonicalize().ok() == helper.canonicalize().ok()
            || file.file_name().map(|n| n.to_os_string()) == this
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for i in offending_lines(&text) {
            let line = text.lines().nth(i).unwrap_or_default();
            offenders.push(format!("{}:{}: {}", file.display(), i + 1, line.trim()));
        }
    }
    assert!(
        offenders.is_empty(),
        "use regia_core::ron_pretty() instead of a direct PrettyConfig (it writes \\r\\n on Windows):\n{}",
        offenders.join("\n")
    );
}

/// CRLF follow-up (report sprint-fix-net3-determinism M3, ADR-10062): the lint also sees a config
/// built through `Default::default()` with the type written on the binding (the form the lockstep
/// crates use because they cannot depend on `regia_core`), unless the same or the next line pins
/// `new_line`. Proven on planted text, because the CRLF itself only shows on a Windows host.
#[test]
fn the_lint_sees_a_default_typed_config_without_new_line() {
    let planted = "let pretty: ron::ser::PrettyConfig = Default::default();\n\
                   ron::ser::to_string_pretty(&value, pretty).unwrap_or_default()\n";
    assert_eq!(offending_lines(planted), vec![0], "{planted}");
    let pinned_next = "let pretty: ron::ser::PrettyConfig = Default::default();\n\
                       ron::ser::to_string_pretty(&value, pretty.new_line(\"\\n\")).unwrap_or_default()\n";
    assert!(offending_lines(pinned_next).is_empty(), "{pinned_next}");
    let pinned_same = "let config: PrettyConfig = Default::default(); config.new_line(\"\\n\")\n";
    assert!(offending_lines(pinned_same).is_empty(), "{pinned_same}");
    let qualified = "let p = <ron::ser::PrettyConfig as Default>::default();\n\
                     ron::ser::to_string_pretty(&value, p)\n";
    assert_eq!(offending_lines(qualified), vec![0], "{qualified}");
    let direct = "let p = PrettyConfig::default();\n";
    assert_eq!(offending_lines(direct), vec![0], "{direct}");
}
