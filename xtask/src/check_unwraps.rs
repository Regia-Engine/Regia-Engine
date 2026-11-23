// SPDX-License-Identifier: MPL-2.0
//! `check-unwraps`: fail the build when a panic-capable construct appears on the *untrusted-input
//! boundary* without an `// invariant:` justification (M18 robustness, `AGENTS.md` § 3).
//!
//! The boundary is the set of source files that parse bytes or text arriving from disk, a pak,
//! the network or the agent protocol — the loaders of every frozen content format, cooked assets,
//! saves, the FPAK reader, the input/rebind configuration and the whole `regia_agent` crate.
//! Those paths must return `Err` on bad input, never panic. A genuine invariant (a slice whose
//! length is proven by an earlier check, an entity that was just spawned) may stay, but only when
//! the same or the preceding line carries `// invariant:` (or `// SAFETY:`).
//!
//! The check is a source scan, not a proof: it flags `unwrap()`, `expect(`, `panic!`,
//! `unreachable!`, `todo!` and `unimplemented!`. Index-out-of-bounds on untrusted bytes cannot be
//! seen statically, so it is covered by the deterministic mutation tests in each loader crate
//! (which `catch_unwind` and fail on panic) — see `docs/evidence/m18/robustness.md`.

use std::path::{Path, PathBuf};

use anyhow::Context;

/// Panic-capable constructs the boundary must not contain without a justification.
const MARKERS: &[&str] = &[
    ".unwrap()",
    ".expect(",
    "panic!",
    "unreachable!",
    "todo!",
    "unimplemented!",
    "unreachable_unchecked",
];

/// Substrings that count as an `// invariant:` / `// SAFETY:` justification.
const JUSTIFICATIONS: &[&str] = &["invariant:", "safety:"];

/// The untrusted-input boundary: the loaders of every frozen format, the asset server and pak
/// reader, the input/rebind configuration, the platform VFS, the glTF importer, and the whole
/// agent crate.
///
/// A file is listed here because it is reachable from a `ron::from_str` / `postcard::from_bytes` /
/// `serde_json::from_str` / `fs::read` / `gltf::Gltf::from_slice` / socket read of bytes the game
/// did not author. New loaders must be added to this list (and `docs/evidence/m18/robustness.md`)
/// when they land; a listed file that no longer exists is a hard error, so a renamed or removed
/// loader cannot silently drop out of the scan.
fn boundary_files(root: &Path) -> Vec<PathBuf> {
    let exact = [
        "crates/regia_asset/src/loader.rs",
        "crates/regia_asset/src/material.rs",
        "crates/regia_asset/src/meta.rs",
        "crates/regia_asset/src/pak.rs",
        "crates/regia_asset/src/pak_verify.rs",
        "crates/regia_asset/src/cooked.rs",
        "crates/regia_asset/src/cooked_header.rs",
        "crates/regia_asset/src/manifest.rs",
        "crates/regia_asset/src/migrator.rs",
        "crates/regia_asset/src/storage.rs",
        "crates/regia_asset/src/server.rs",
        "crates/regia_scene/src/format.rs",
        "crates/regia_scene/src/cooked.rs",
        "crates/regia_scene/src/level.rs",
        "crates/regia_scene/src/prefab.rs",
        "crates/regia_scene/src/spawn.rs",
        "crates/regia_ui/src/authoring.rs",
        "crates/regia_vfx/src/format.rs",
        "crates/regia_animation/src/graph.rs",
        "crates/regia_animation/src/cooked.rs",
        "crates/regia_animation/src/authoring.rs",
        "crates/regia_audio/src/cue.rs",
        "crates/regia_audio/src/patch.rs",
        "crates/regia_audio/src/music.rs",
        "crates/regia_audio/src/bus.rs",
        "crates/regia_audio/src/settings.rs",
        "crates/regia_terrain/src/layers.rs",
        "crates/regia_terrain/src/scatter/mod.rs",
        "crates/regia_terrain/src/scatter/set.rs",
        "crates/regia_terrain/src/scatter/painted.rs",
        "crates/regia_terrain/src/water.rs",
        "crates/regia_framework/src/save_game.rs",
        "crates/regia_framework/src/settings.rs",
        "crates/regia_framework/src/sim_bridge.rs",
        "crates/regia_input/src/config.rs",
        "crates/regia_input/src/rebind.rs",
        "crates/regia_platform/src/vfs/dir.rs",
        "crates/regia_platform/src/vfs/mod.rs",
        "crates/regia_platform/src/vfs/path.rs",
        // glTF import (the pipeline reads untrusted glTF/GLB bytes from disk).
        "crates/regia_asset_pipeline/src/gltf_import.rs",
        "crates/regia_asset_pipeline/src/gltf_anim.rs",
        "crates/regia_asset_pipeline/src/cook.rs",
        "crates/regia_asset_pipeline/src/gltf_settings.rs",
        "crates/regia_asset_pipeline/src/db.rs",
        // Version-peek and physics-layer config parsers (disk/agent-authored text).
        "crates/regia_core/src/version.rs",
        "crates/regia_physics/src/layers.rs",
        // The `.fluidcache` reader (ADR-2441): baked files from disk or `fluid.play { path }`.
        "crates/regia_fluid/src/cache.rs",
    ];
    let mut files: Vec<PathBuf> = exact.iter().map(|p| root.join(p)).collect();
    // The agent crate in full, subdirectories included (`render_out/` parses `params` too): its
    // server reads the socket and every handler reads `params`.
    let agent_dir = root.join("crates/regia_agent/src");
    if agent_dir.is_dir() {
        collect_rs(&agent_dir, &mut files);
    } else {
        // `run` reports a listed path that does not exist as a stale boundary list.
        files.push(agent_dir);
    }
    files.sort();
    files.dedup();
    files
}

/// Every `.rs` file under `dir`, recursively.
fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// One line of source, tagged as code (scanned) or non-code (skipped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    /// A line that can contain a panic construct.
    Code,
    /// Blank, a comment, an attribute, or inside a test block — never scanned.
    Skipped,
}

/// Classifies every line of `content` as [`Line::Code`] or [`Line::Skipped`], removing comment
/// lines, `#[cfg(test)]` items and `mod tests { … }` blocks so test-only `unwrap`s do not trip the
/// boundary check. A file whose header (before its first item) holds `#![cfg(test)]` is compiled
/// only into tests, so every line of it is skipped (ADR-1589).
fn classify(content: &str) -> Vec<Line> {
    if is_test_only_file(content) {
        return vec![Line::Skipped; content.lines().count()];
    }
    let mut out = Vec::new();
    let mut test_depth = 0isize;
    let mut pending_cfg_test = false;
    let mut in_block_comment = false;
    for raw in content.lines() {
        let line = raw.trim();
        // A block comment swallows everything until its closing `*/`, including `mod tests {`
        // and panic constructs; only the line that closes it resumes normal scanning.
        if in_block_comment {
            out.push(Line::Skipped);
            if line.contains("*/") {
                in_block_comment = false;
            }
            continue;
        }
        if test_depth > 0 {
            test_depth += brace_delta(line);
            out.push(Line::Skipped);
            continue;
        }
        if pending_cfg_test {
            // Chained attributes on the test item (`#[cfg(test)] #[must_use] fn …`).
            if line.starts_with('#') {
                out.push(Line::Skipped);
                continue;
            }
            let item = strip_visibility(line);
            if is_braced_item(item) {
                test_depth = brace_delta(line);
                out.push(Line::Skipped);
                pending_cfg_test = false;
                continue;
            }
            if item.starts_with("mod ")
                || item.starts_with("fn ")
                || item.starts_with("use ")
                || item.starts_with("const ")
                || item.starts_with("static ")
                || item.starts_with("type ")
                || item.starts_with("struct ")
                || item.starts_with("enum ")
                || item.starts_with("trait ")
                || item.starts_with("impl")
            {
                pending_cfg_test = false;
                out.push(Line::Skipped);
                continue;
            }
            // Not an item after all: treat as ordinary code.
            pending_cfg_test = false;
        }
        if line == "#[cfg(test)]" {
            pending_cfg_test = true;
            out.push(Line::Skipped);
            continue;
        }
        if line.starts_with("//") || line.starts_with("/*") || line.starts_with("//!") {
            if line.starts_with("/*") && !line.contains("*/") {
                in_block_comment = true;
            }
            out.push(Line::Skipped);
            continue;
        }
        if line.is_empty() {
            out.push(Line::Skipped);
            continue;
        }
        // A test module at depth 0 (with or without a `#[cfg(test)]` attribute above).
        if is_test_module(line) {
            test_depth = brace_delta(line);
            out.push(Line::Skipped);
            continue;
        }
        out.push(Line::Code);
    }
    out
}

/// Whether the file's header (blank lines, comments and inner attributes before the first item)
/// holds `#![cfg(test)]`.
fn is_test_only_file(content: &str) -> bool {
    for raw in content.lines() {
        let line = raw.trim();
        if line == "#![cfg(test)]" {
            return true;
        }
        if line.is_empty() || line.starts_with("//") || line.starts_with("#![") {
            continue;
        }
        return false;
    }
    false
}

/// Strips a leading `pub ` / `pub(crate) ` / `pub(super) ` visibility modifier.
fn strip_visibility(line: &str) -> &str {
    if let Some(rest) = line.strip_prefix("pub ") {
        return rest.trim_start();
    }
    if line.starts_with("pub(")
        && let Some(close) = line.find(')')
    {
        return line[close + 1..].trim_start();
    }
    line
}

/// Whether a line opens a test module block (`mod tests {`, `pub(crate) mod m9_tests {`,
/// `mod fuzz {`, `mod fixtures {`, …): a braced `mod` whose name suggests test-only code.
fn is_test_module(line: &str) -> bool {
    let item = strip_visibility(line);
    let Some(rest) = item.strip_prefix("mod ") else {
        return false;
    };
    if !item.contains('{') {
        return false;
    }
    let name = rest.split(['{', ' ', '\t']).next().unwrap_or("");
    name.contains("test") || name.contains("fuzz") || name.contains("fixture") || name.contains("spec")
}

/// Whether an item keyword introduces a braced block.
fn is_braced_item(item: &str) -> bool {
    (item.starts_with("mod ")
        || item.starts_with("fn ")
        || item.starts_with("struct ")
        || item.starts_with("enum ")
        || item.starts_with("trait ")
        || item.starts_with("impl"))
        && item.contains('{')
}

/// Net brace depth contribution of a line (`{` count minus `}` count).
fn brace_delta(line: &str) -> isize {
    let open = line.bytes().filter(|b| *b == b'{').count() as isize;
    let close = line.bytes().filter(|b| *b == b'}').count() as isize;
    open - close
}

/// Scan one file and return the violations: `path:line: construct` for every unjustified marker.
fn audit_file(path: &Path) -> anyhow::Result<Vec<String>> {
    let content = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let lines: Vec<&str> = content.lines().collect();
    let classes = classify(&content);
    let mut errors = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if classes[i] != Line::Code {
            continue;
        }
        if !MARKERS.iter().any(|m| line.contains(m)) {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        let prev_justified = (0..i)
            .rev()
            .find(|&p| !lines[p].trim().is_empty())
            .map(|p| lines[p].to_ascii_lowercase())
            .is_some_and(|prev| JUSTIFICATIONS.iter().any(|j| prev.contains(j)));
        let self_justified = JUSTIFICATIONS.iter().any(|j| lower.contains(j));
        if !self_justified && !prev_justified {
            let construct = MARKERS
                .iter()
                .find(|m| line.contains(**m))
                .copied()
                .unwrap_or("?");
            errors.push(format!(
                "{}:{}: {} on the untrusted-input boundary needs an `// invariant:` justification",
                path.display(),
                i + 1,
                construct
            ));
        }
    }
    Ok(errors)
}

/// Run `check-unwraps`. Fails when any boundary file contains an unjustified panic construct.
pub fn run(root: &Path) -> anyhow::Result<std::process::ExitCode> {
    let files = boundary_files(root);
    let mut errors = Vec::new();
    let mut scanned = 0;
    for path in &files {
        if !path.exists() {
            // A listed boundary file that no longer exists means the list is stale (a loader was
            // renamed or removed) — that is a failure, not something to silently skip: a new
            // loader outside the list would otherwise stay invisible.
            errors.push(format!(
                "boundary file missing: {} (the untrusted-input list is stale — a loader was renamed or removed)",
                path.display()
            ));
            continue;
        }
        scanned += 1;
        errors.extend(audit_file(path)?);
    }
    if errors.is_empty() {
        println!("check-unwraps: {scanned} boundary files clean");
        Ok(std::process::ExitCode::SUCCESS)
    } else {
        for err in &errors {
            eprintln!("error: {err}");
        }
        eprintln!("check-unwraps: {} problem(s)", errors.len());
        Ok(std::process::ExitCode::FAILURE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_lines_are_skipped() {
        let classes = classify("//! .expect(\"x\")\n/// .unwrap()\nlet x = 1;\n");
        assert_eq!(classes, vec![Line::Skipped, Line::Skipped, Line::Code]);
    }

    #[test]
    fn block_comments_are_skipped_but_star_lines_are_not() {
        // A `*`-prefixed line inside a `/* … */` block comment is skipped; a `*`-prefixed line
        // outside a comment (a dereference) must still be scanned (round-2 fix: `classify` used to
        // skip every line starting with `*`, hiding panic constructs on deref lines).
        let src = "\
/* header
 * continued comment .unwrap()
 */
fn f(p: &mut u8) {
    *p = y.unwrap();
}
";
        let classes = classify(src);
        assert_eq!(classes[0], Line::Skipped); // /* header
        assert_eq!(classes[1], Line::Skipped); //  * continued …
        assert_eq!(classes[2], Line::Skipped); //  */
        assert_eq!(classes[3], Line::Code); // fn f(…
        assert_eq!(classes[4], Line::Code); // *p = y.unwrap();
        assert_eq!(classes[5], Line::Code); // }
    }

    #[test]
    fn test_modules_are_skipped() {
        let src = "\
pub fn load() -> Result<(), E> { Ok(()) }
#[cfg(test)]
mod tests {
    fn t() { let _ = (1, 2).0; }
}
pub fn after() {}
";
        let classes = classify(src);
        assert_eq!(classes[0], Line::Code);
        assert_eq!(classes[1], Line::Skipped); // #[cfg(test)]
        assert_eq!(classes[2], Line::Skipped); // mod tests {
        assert_eq!(classes[3], Line::Skipped); // body
        assert_eq!(classes[4], Line::Skipped); // }
        assert_eq!(classes[5], Line::Code);
    }

    #[test]
    fn a_cfg_test_file_is_skipped_whole_but_only_from_its_header() {
        let src = "\
//! Tests of the scoped dispatch.
#![cfg(test)]

use std::sync::Arc;
fn t() { let _ = v.expect(\"x\"); }
";
        let classes = classify(src);
        assert_eq!(classes.len(), 5);
        assert!(classes.iter().all(|c| *c == Line::Skipped));
        // The attribute after the first item is not a file header: the file is scanned.
        let late = "pub fn load() {}\n#![cfg(test)]\nfn t() { v.unwrap(); }\n";
        assert_eq!(classify(late)[2], Line::Code);
        // Another inner attribute does not make a file test-only.
        let other = "#![allow(dead_code)]\nfn t() { v.unwrap(); }\n";
        assert_eq!(classify(other)[1], Line::Code);
    }

    #[test]
    fn cfg_test_items_outside_a_module_are_skipped() {
        let src = "\
pub fn a() {}
#[cfg(test)]
#[must_use]
fn helper() -> usize { 0 }
pub fn b() {}
";
        let classes = classify(src);
        assert_eq!(classes[0], Line::Code);
        assert_eq!(classes[3], Line::Skipped); // fn helper() {
        assert_eq!(classes[4], Line::Code); // pub fn b()
    }

    #[test]
    fn justification_suppresses_the_marker() {
        let tmp = std::env::temp_dir().join(format!("regia-unwrap-justified-{}", std::process::id()));
        std::fs::write(
            &tmp,
            "let x = y.try_into().expect(\"4 bytes\"); // invariant: slice is fixed\n",
        )
        .expect("write");
        let errors = audit_file(&tmp).expect("audit");
        let _ = std::fs::remove_file(&tmp);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn unjustified_marker_is_reported() {
        let tmp = std::env::temp_dir().join(format!("regia-unwrap-bad-{}", std::process::id()));
        std::fs::write(&tmp, "let x = y.unwrap();\n").expect("write");
        let errors = audit_file(&tmp).expect("audit");
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains(".unwrap()"), "{}", errors[0]);
    }

    #[test]
    fn the_agent_crate_is_scanned_with_its_subdirectories_and_a_missing_one_is_listed() {
        let root = std::env::temp_dir().join(format!(
            "regia-check-unwraps-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let agent = root.join("crates/regia_agent/src");
        std::fs::create_dir_all(agent.join("render_out")).expect("mkdir");
        std::fs::write(agent.join("lib.rs"), "").expect("write");
        std::fs::write(agent.join("render_out/plan.rs"), "").expect("write");
        let files = boundary_files(&root);
        assert!(files.contains(&agent.join("lib.rs")), "{files:?}");
        assert!(files.contains(&agent.join("render_out/plan.rs")), "{files:?}");
        std::fs::remove_dir_all(&agent).expect("remove");
        let files = boundary_files(&root);
        let _ = std::fs::remove_dir_all(&root);
        // `run` fails on a listed path that does not exist.
        assert!(files.contains(&agent), "{files:?}");
    }
}
