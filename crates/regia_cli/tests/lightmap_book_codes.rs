// SPDX-License-Identifier: MPL-2.0
//! `docs/specs/lightmaps.md` § 9 and `AGENTS.md` rule 8: every `lightmap_*` and `bake_*` error or
//! warning code the lightmap path can print (the `regia bake-lightmaps` CLI, the `.flmap` reader,
//! the UV generators, the bakers, the cook and per-tier cook, the level plugin, the renderer's
//! receiver errors and the editor's Lightmaps panel) is listed on `book/agents/lightmaps.md` or on
//! a page it links for that topic.
//!
//! The agent methods' codes are checked by `cargo xtask agent-coverage` through their contracts;
//! the CLI and cook codes have no contract, so this test reads them from the sources: string
//! literals that are a whole code (`"lightmap_io"`) or start a `code: message` string
//! (`"lightmap_tier_rule: ..."`), outside comments, test modules, tracing span names, temp
//! directory names, GPU labels and file-name constants (ADR-5663).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The workspace root.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Source files (relative to the workspace root) and directories whose `*.rs` files are scanned.
const SOURCES: &[&str] = &[
    "crates/regia_cli/src/commands/bake_lightmaps.rs",
    "crates/regia_cli/src/commands/bake_lightmaps",
    "crates/regia_asset/src/lightmap.rs",
    "crates/regia_asset/src/lightmap",
    "crates/regia_asset_pipeline/src/lightmap_uv.rs",
    "crates/regia_asset_pipeline/src/lightmap_uv",
    "crates/regia_asset_pipeline/src/lightmap_cook.rs",
    "crates/regia_asset_pipeline/src/lightmap_tiers.rs",
    "crates/regia_asset_pipeline/src/lightmap_tiers",
    "crates/regia_pt/src/lightmap.rs",
    "crates/regia_pt/src/lightmap",
    "crates/regia_framework/src/lightmap_level.rs",
    "crates/regia_render/src/lightmap.rs",
    "crates/regia_editor/src/lightmap_panel.rs",
];

/// The book pages a code may be listed on: the lightmap page and the pages it links for shared
/// topics (bake descriptions, level export, the UV atlas, resumable bakes).
const PAGES: &[&str] = &[
    "book/agents/lightmaps.md",
    "book/agents/baked-gi-bake.md",
    "book/agents/baked-gi-export.md",
    "book/agents/atlas.md",
    "book/agents/bake-checkpoints.md",
];

/// Text just before a literal that marks it as a name, not a code.
const NOT_A_CODE: &[&str] = &[
    "span!(",
    "temp_dir(",
    "label",
    "name =",
    "tool:",
    "instrument(",
    "_DIR: &str =",
    "_FILE: &str =",
];

/// Every `.rs` file of [`SOURCES`], test-only files left out.
fn source_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for rel in SOURCES {
        let path = root().join(rel);
        if path.is_dir() {
            let mut dir: Vec<PathBuf> = std::fs::read_dir(&path)
                .unwrap_or_else(|e| panic!("read {rel}: {e}"))
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "rs"))
                .filter(|p| p.file_name().is_some_and(|n| n != "tests.rs"))
                .collect();
            dir.sort();
            out.extend(dir);
        } else {
            assert!(path.is_file(), "{rel} is gone: update SOURCES in this test");
            out.push(path);
        }
    }
    out
}

/// `text` up to its inline `#[cfg(test)] mod tests { ... }` (always the file's last item here).
fn without_test_module(text: &str) -> &str {
    let mut from = 0;
    while let Some(at) = text[from..].find("#[cfg(test)]") {
        let start = from + at;
        let rest = text[start + "#[cfg(test)]".len()..].trim_start();
        if let Some(after) = rest.strip_prefix("mod tests")
            && after.trim_start().starts_with('{')
        {
            return &text[..start];
        }
        from = start + 1;
    }
    text
}

/// The codes in one file's text: `"lightmap_x"` / `"bake_x"` literals, or literals that start
/// `"lightmap_x: "`, outside comment lines and away from [`NOT_A_CODE`] markers.
fn codes_in(text: &str) -> BTreeSet<String> {
    let text = without_test_module(text);
    let mut out = BTreeSet::new();
    for (at, _) in text.match_indices('"') {
        let ident_start = at + 1;
        let tail = &text[ident_start..];
        if !(tail.starts_with("lightmap_") || tail.starts_with("bake_")) {
            continue;
        }
        let len = tail
            .bytes()
            .take_while(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
            .count();
        let after = &tail[len..];
        if !(after.starts_with('"') || after.starts_with(": ")) {
            continue;
        }
        let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
        if text[line_start..at].trim_start().starts_with("//") {
            continue;
        }
        // The text before the literal, across a line break (`info_span!(\n "name"`).
        let mut ctx_start = at.saturating_sub(48);
        while !text.is_char_boundary(ctx_start) {
            ctx_start -= 1;
        }
        let context: String = text[ctx_start..at]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if NOT_A_CODE.iter().any(|m| context.contains(m)) {
            continue;
        }
        out.insert(tail[..len].to_string());
    }
    out
}

/// Every code in [`SOURCES`] with the files it appears in.
fn all_codes() -> BTreeMap<String, BTreeSet<String>> {
    let mut codes: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for file in source_files() {
        let text = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        let name = file.strip_prefix(root()).unwrap_or(&file).display().to_string();
        for code in codes_in(&text) {
            codes.entry(code).or_default().insert(name.clone());
        }
    }
    codes
}

#[test]
fn the_scanner_reads_codes_and_skips_names() {
    let text = r#"
        // "lightmap_in_a_comment" is not read
        let _span = tracing::info_span!(
            "lightmap_span_name",
        );
        let dir = unique_temp_dir("lightmap_temp_dir");
        pub const TIER_DIR: &str = "lightmap_dir_name";
        let e = LightmapCommandError::new("lightmap_real", None, "m");
        let m = format!("lightmap_prefixed: {e}");
        let w = "lightmap_not:a code, since no space follows the colon";
        let x = "lightmap_partial_word and more";
        failure("bake_failed_here", m);
        #[cfg(test)]
        mod tests {
            fn t() { let _ = "lightmap_in_tests"; }
        }
    "#;
    let got: Vec<String> = codes_in(text).into_iter().collect();
    assert_eq!(got, ["bake_failed_here", "lightmap_prefixed", "lightmap_real"]);
}

#[test]
fn every_lightmap_code_is_on_its_book_page() {
    let codes = all_codes();
    // The scan still finds the codes it is meant to (a moved or renamed source would empty it).
    for known in [
        "lightmap_bake_settings",
        "lightmap_uv_settings",
        "lightmap_payload",
        "lightmap_tier_rule",
        "lightmap_cook_settings",
        "lightmap_busy",
        "lightmap_not_found",
        "lightmap_receiver_lod",
        "lightmap_gpu_device_lost",
        "bake_scene_unknown",
    ] {
        assert!(
            codes.contains_key(known),
            "the scan lost `{known}`: {:?}",
            codes.keys()
        );
    }
    assert!(
        codes.len() >= 40,
        "only {} codes found: {:?}",
        codes.len(),
        codes.keys()
    );

    let book: String = PAGES
        .iter()
        .map(|page| std::fs::read_to_string(root().join(page)).unwrap_or_else(|e| panic!("read {page}: {e}")))
        .collect::<Vec<_>>()
        .join("\n");
    let missing: Vec<String> = codes
        .iter()
        .filter(|(code, _)| !book.contains(&format!("`{code}`")))
        .map(|(code, files)| {
            format!(
                "`{code}` (from {})",
                files.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        })
        .collect();
    assert!(
        missing.is_empty(),
        "lightmap error codes with no row on book/agents/lightmaps.md (or the pages it links):\n  {}",
        missing.join("\n  ")
    );
}
