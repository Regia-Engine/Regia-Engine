// SPDX-License-Identifier: MPL-2.0
//! The cook's montage slot check (`docs/specs/34-montage-extensions.md` M1, ADR-7216).
//!
//! A montage track whose slot no `.slots.ron` table of the project names is a load *warning*
//! (the track plays full body) but a cook *error*: the cook reports `montage_unknown_slot` for
//! the montage's path and the cook fails. A montage file names no skeleton, so the check runs
//! against every slot table of the project ([`regia_animation::skeleton_slots::cook_slot_errors`]);
//! a project with no slot table defines its slots in code and nothing is checked. Montages still
//! ship raw: this pass only checks them.

use std::path::{Path, PathBuf};

use regia_animation::Montage;
use regia_animation::skeleton_slots::{SkeletonSlots, cook_slot_errors};
use regia_asset::normalise_asset_path;

use crate::cook::name_ends_with;

/// `(path, message)` per montage track with an unknown slot, plus one per slot table or montage
/// that does not parse (paths project-relative, `assets/...`). Empty when every montage's slots
/// are named or the project has no slot table.
pub fn check_montage_slots(sources: &[PathBuf], assets_root: &Path) -> Vec<(String, String)> {
    let _span = tracing::debug_span!(target: "regia_asset_pipeline", "montage_slot_check").entered();
    let relative = |path: &Path| {
        let rel = path
            .strip_prefix(assets_root)
            .map(normalise_asset_path)
            .unwrap_or_else(|_| normalise_asset_path(path));
        format!("assets/{rel}")
    };
    let name_of = |path: &Path| {
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned()
    };
    let mut errors = Vec::new();
    let mut tables = Vec::new();
    for path in sources
        .iter()
        .filter(|p| name_ends_with(&name_of(p), ".slots.ron"))
    {
        let parsed = std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|text| SkeletonSlots::from_ron_str(&text).map_err(|e| e.to_string()));
        match parsed {
            Ok(table) => tables.push(table),
            Err(e) => errors.push((relative(path), format!("slots_invalid: {e}"))),
        }
    }
    if tables.is_empty() {
        return errors;
    }
    for path in sources
        .iter()
        .filter(|p| name_ends_with(&name_of(p), ".montage.ron"))
    {
        let parsed = std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|text| Montage::from_ron_str(&text).map_err(|e| e.to_string()));
        match parsed {
            Ok(montage) => {
                for issue in cook_slot_errors(&montage, &tables) {
                    errors.push((relative(path), format!("{}: {}", issue.code, issue.detail)));
                }
            }
            // A montage that does not parse fails at load with its own error; the slot check
            // cannot read its tracks.
            Err(e) => errors.push((relative(path), format!("montage_invalid: {e}"))),
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = r#"(format_version: 1, slots: [(name: "full_body", group: "body", mask: None), (name: "upper", group: "upper", mask: None)])"#;

    fn montage(name: &str, slot: &str) -> String {
        Montage::single(name, slot, "strike")
            .to_ron_string()
            .unwrap_or_default()
    }

    fn write(root: &Path, rel: &str, text: &str) -> PathBuf {
        let path = root.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("dir");
        }
        std::fs::write(&path, text).expect("write");
        path
    }

    #[test]
    fn a_montage_with_an_unknown_slot_fails_the_cook_check() {
        let root = regia_test_utils::unique_temp_dir("montage-slot-cook");
        let assets = root.join("assets");
        let sources = vec![
            write(&assets, "rig/hero.slots.ron", TABLE),
            write(&assets, "anim/ok.montage.ron", &montage("ok", "upper")),
            write(&assets, "anim/stray.montage.ron", &montage("stray", "tail")),
        ];
        let errors = check_montage_slots(&sources, &assets);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].0, "assets/anim/stray.montage.ron");
        assert!(
            errors[0].1.starts_with("montage_unknown_slot: "),
            "{}",
            errors[0].1
        );
        assert!(errors[0].1.contains("tail"), "{}", errors[0].1);
        // Without a slot table the project defines its slots in code: nothing to check.
        assert!(check_montage_slots(&sources[1..], &assets).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
