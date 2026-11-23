// SPDX-License-Identifier: MPL-2.0
//! The references of `.ui.ron` layouts (`docs/specs/111-ui-components.md` § 5, ADR-3147).
//!
//! A layout names the component documents it instantiates (`use: "ui/health_bar.ui.ron"`) and the
//! mask images it samples (`mask: (asset: "ui/star.png")`). Layouts are raw files (not cooked), so
//! the cooked registry never records these edges; [`AssetSet::with_ui_layouts`] adds every layout
//! with them, and the dependency graph, the missing-reference and orphan validators and the size
//! map then treat a component as a dependency of each layout that uses it. The `use` and `asset`
//! scan is a plain lexical pass over the RON text (strings and comments skipped); the picture paths
//! of the typed `image` and `mask` fields come from the loader as well (ADR-3148), and a layout the
//! loader refuses is tagged rather than dropped.

use std::path::Path;

use crate::asset_graph::{AssetEntry, AssetKind, AssetSet};
use crate::registry_bridge::{UI_LAYOUT_ERROR_TAG, UI_LAYOUT_TAG, ui_layout_dependencies};

/// The keys whose string value is a reference: a component instance and a mask image.
const REFERENCE_KEYS: [&str; 2] = ["use", "asset"];

/// The asset paths a `.ui.ron` text references (`use:` and mask `asset:` values), in text order,
/// without duplicates. `assets/` and `assets://` prefixes are removed, so the paths are relative to
/// the project's `assets/` like every other [`AssetEntry`] path.
#[must_use]
pub fn ui_layout_references(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => i = skip_string(b, i).0,
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => i = skip_block_comment(b, i),
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                let ident = &text[start..i];
                if !REFERENCE_KEYS.contains(&ident) {
                    continue;
                }
                let colon = skip_trivia(b, i);
                if b.get(colon) != Some(&b':') {
                    continue;
                }
                let open = skip_trivia(b, colon + 1);
                if b.get(open) != Some(&b'"') {
                    continue;
                }
                let (end, value) = skip_string(b, open);
                i = end;
                let path = normalize(&value);
                if !path.is_empty() && !out.contains(&path) {
                    out.push(path);
                }
            }
            _ => i += 1,
        }
    }
    out
}

fn normalize(path: &str) -> String {
    let mut p = path.replace('\\', "/");
    for prefix in ["assets://", "assets/", "./"] {
        if let Some(rest) = p.strip_prefix(prefix) {
            p = rest.to_owned();
        }
    }
    p
}

/// Index past the string opening at `open` and its decoded value (`\"` and `\\` escapes).
fn skip_string(b: &[u8], open: usize) -> (usize, String) {
    let mut i = open + 1;
    let mut value = Vec::new();
    while i < b.len() && b[i] != b'"' {
        if b[i] == b'\\' && i + 1 < b.len() {
            value.push(b[i + 1]);
            i += 2;
        } else {
            value.push(b[i]);
            i += 1;
        }
    }
    (i + 1, String::from_utf8_lossy(&value).into_owned())
}

fn skip_block_comment(b: &[u8], open: usize) -> usize {
    let mut nest = 1;
    let mut i = open + 2;
    while i < b.len() && nest > 0 {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            nest += 1;
            i += 2;
        } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
            nest -= 1;
            i += 2;
        } else {
            i += 1;
        }
    }
    i
}

fn skip_trivia(b: &[u8], mut i: usize) -> usize {
    loop {
        match b.get(i) {
            Some(c) if c.is_ascii_whitespace() => i += 1,
            Some(b'/') if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            Some(b'/') if b.get(i + 1) == Some(&b'*') => i = skip_block_comment(b, i),
            _ => return i,
        }
    }
}

impl AssetSet {
    /// Adds every `.ui.ron` under `assets_dir` (recursively) with its [`ui_layout_references`]
    /// and the picture paths its typed `image` and `mask` fields name (ADR-3148): a layout already
    /// in the set gains the references, a new one is added as [`AssetKind::Other`] with its file
    /// size. Every layout gets the [`UI_LAYOUT_TAG`] tag (`root` for a top-level `ui/*.ui.ron`,
    /// which the runtime loads at start, so it is an orphan-check root; `nested` otherwise); one
    /// that does not parse also gets [`UI_LAYOUT_ERROR_TAG`] with the loader's message.
    /// Unreadable files are skipped. Entries stay sorted by path.
    #[must_use]
    pub fn with_ui_layouts(mut self, assets_dir: &Path) -> Self {
        for entry in walkdir::WalkDir::new(assets_dir)
            .sort_by_file_name()
            .into_iter()
            .flatten()
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let is_layout = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".ui.ron"));
            if !is_layout {
                continue;
            }
            let Ok(rel) = path.strip_prefix(assets_dir) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            let mut references = ui_layout_references(&text);
            let parsed = ui_layout_dependencies(&text, &rel);
            if let Ok(pictures) = &parsed {
                for picture in pictures {
                    let picture = normalize(picture);
                    if !references.contains(&picture) {
                        references.push(picture);
                    }
                }
            }
            let top_level = rel.strip_prefix("ui/").is_some_and(|name| !name.contains('/'));
            let role = if top_level { "root" } else { "nested" };
            match self.assets.iter_mut().find(|a| a.path == rel) {
                Some(existing) => {
                    for r in references {
                        if !existing.references.contains(&r) {
                            existing.references.push(r);
                        }
                    }
                    existing.tags.insert(UI_LAYOUT_TAG.to_owned(), role.to_owned());
                    if let Err(e) = parsed {
                        existing.tags.insert(UI_LAYOUT_ERROR_TAG.to_owned(), e);
                    }
                }
                None => {
                    let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    let mut layout = AssetEntry::new(rel, AssetKind::Other)
                        .with_references(references)
                        .with_cooked_bytes(bytes)
                        .with_tag(UI_LAYOUT_TAG, role);
                    if let Err(e) = parsed {
                        layout = layout.with_tag(UI_LAYOUT_ERROR_TAG, e);
                    }
                    self.push(layout);
                }
            }
        }
        self.assets.sort_by(|a, b| a.path.cmp(&b.path));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_graph::DependencyGraph;

    #[test]
    fn uses_and_mask_assets_are_found_comments_and_other_strings_are_not() {
        let text = r#"// use: "ui/commented.ui.ron"
(version: 9, root: (children: [
    (use: "ui/bar.ui.ron", name: "use: \"ui/not.ui.ron\""),
    /* (use: "ui/block.ui.ron") */
    (use : "assets/ui/bar.ui.ron"),
    (mask: (asset: "assets://ui/star.png")),
    (text: (spans: ["asset: \"x.png\""])),
    (slots: {"s": [(use: "ui/inner.ui.ron")]}),
]))"#;
        assert_eq!(
            ui_layout_references(text),
            ["ui/bar.ui.ron", "ui/star.png", "ui/inner.ui.ron"]
        );
    }

    #[test]
    fn the_dependency_graph_sees_components() {
        let dir = regia_test_utils::unique_temp_dir("ui-refs");
        let assets = dir.join("assets");
        std::fs::create_dir_all(assets.join("ui/parts")).expect("mkdir");
        std::fs::write(
            assets.join("ui/hud.ui.ron"),
            r#"(version: 9, root: (children: [(use: "ui/parts/bar.ui.ron"), (use: "ui/gone.ui.ron")]))"#,
        )
        .expect("write");
        std::fs::write(
            assets.join("ui/parts/bar.ui.ron"),
            r#"(version: 9, root: (mask: (asset: "ui/star.png")))"#,
        )
        .expect("write");
        let set = AssetSet::new().with_ui_layouts(&assets);
        let graph = DependencyGraph::build(&set);
        assert_eq!(
            graph.dependencies("ui/hud.ui.ron"),
            ["ui/parts/bar.ui.ron".to_owned()]
        );
        assert_eq!(
            graph.missing_references("ui/hud.ui.ron"),
            ["ui/gone.ui.ron".to_owned()]
        );
        assert_eq!(
            graph.referrers("ui/parts/bar.ui.ron"),
            ["ui/hud.ui.ron".to_owned()]
        );
        assert!(!graph.transitive_dependencies("ui/hud.ui.ron").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
