// SPDX-License-Identifier: MPL-2.0
//! A fresh clone's first import (spec addendum § 8.9, ADR-7788): the fixture project under
//! `tests/fixtures/first_import` has sources and `.meta` sidecars but no `.regia/` (no asset database, no
//! cooked cache), as a new checkout has. Opening it cooks it; that first cook must report no missing
//! references, and every reference in the cooked registry must resolve.

use std::path::{Path, PathBuf};

/// Copies `from` into `to` recursively, leaving out any `.regia` folder (a fresh clone has none).
fn copy_fresh(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("dir");
    for entry in std::fs::read_dir(from).expect("read").filter_map(Result::ok) {
        let name = entry.file_name();
        if name == ".regia" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            copy_fresh(&path, &to.join(&name));
        } else {
            std::fs::copy(&path, to.join(&name)).expect("copy");
        }
    }
}

fn fresh_clone(tag: &str) -> PathBuf {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/first_import");
    let root = regia_test_utils::unique_temp_dir(tag);
    copy_fresh(&fixture, &root);
    assert!(!root.join(".regia").exists());
    root
}

/// Errors and warnings of a cook that speak of a reference that does not resolve.
fn missing_reference_lines(report: &regia_asset_pipeline::CookReport) -> Vec<String> {
    let suspicious = |text: &str| {
        let lower = text.to_ascii_lowercase();
        [
            "missing",
            "unresolved",
            "not found",
            "no asset",
            "unknown asset",
            "does not exist",
        ]
        .iter()
        .any(|word| lower.contains(word))
    };
    report
        .errors
        .iter()
        .map(|(path, message)| format!("error {path}: {message}"))
        .chain(report.warnings.iter().map(|line| format!("warning {line}")))
        .filter(|line| suspicious(line))
        .collect()
}

#[test]
fn a_fresh_clone_cooks_in_dependency_order_with_no_false_missing_references() {
    let root = fresh_clone("first_import");
    let report =
        regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions::for_host(root.clone()))
            .expect("the first cook runs");
    assert!(
        report.errors.is_empty(),
        "first cook errors: {:#?}",
        report.errors
    );
    assert_eq!(missing_reference_lines(&report), Vec::<String>::new());
    assert!(report.cooked >= 4, "{report:?}");

    // Every reference in the cooked registry resolves (no `unresolved:<id>`, no missing path).
    let platform = regia_asset_pipeline::host_platform();
    let cache = regia_asset::cooked_cache_dir(&root, platform);
    let loaded = regia_asset::registry::LoadedAssetRegistry::load(&cache, None)
        .expect("the registry reads")
        .expect("the first cook wrote a registry");
    let set = regia_asset_pipeline::asset_graph::AssetSet::from_registry(&loaded.registry);
    let graph = regia_asset_pipeline::asset_graph::DependencyGraph::build(&set);
    let missing: Vec<(String, Vec<String>)> = graph
        .paths()
        .map(|path| (path.to_string(), graph.missing_references(path)))
        .filter(|(_, missing)| !missing.is_empty())
        .collect();
    assert!(
        missing.is_empty(),
        "missing references after the first cook: {missing:#?}"
    );
    // The normal map named by id is a registry dependency of the material, resolved on the first cook
    // even though `materials/` cooks before `textures/`. The color map named by path is resolved through
    // the cook manifest at load time, not recorded as a dependency (ADR-3320), so it only has to be there.
    let material = "assets/materials/bark.material.ron";
    let deps = graph.dependencies(material);
    assert_eq!(deps, ["assets/textures/bark_n.png"], "{material}");
    assert!(graph.contains("assets/textures/bark.png"));
    // The color map's path slot names a file that is there and that the cook registered (review
    // sprint-w12-rev-g5 #13: a slot pointing at a missing path left the cook silent).
    let bytes = std::fs::read(root.join(material)).expect("the material source");
    let parsed = regia_asset::MaterialAssetRon::from_ron(&bytes).expect("the material parses");
    let Some(regia_asset::TextureSlot::Path(color)) = parsed.base.base_color_map.clone() else {
        panic!(
            "the fixture names its color map by path: {:?}",
            parsed.base.base_color_map
        );
    };
    assert_eq!(color, "textures/bark.png");
    assert!(
        root.join("assets").join(&color).is_file(),
        "the color map's file exists: {color}"
    );
    assert!(
        graph.contains(&format!("assets/{color}")),
        "the color map is in the cooked registry: {color}"
    );

    // The ids the clone brought are the ids the cook kept (nothing was re-created).
    for (rel, id) in [
        ("assets/textures/bark.png", "3d0f7a52-6c1e-4b8a-9f2d-71a5c0e4b901"),
        ("assets/props/crate.glb", "c51a9e08-7b3f-4d2c-a16e-90f8b4d37c25"),
        (
            "assets/materials/bark.material.ron",
            "61b7d3e9-2f48-4a0c-8d5b-e39c07a1f6d4",
        ),
    ] {
        assert_eq!(super::asset_id(&root, rel).as_deref(), Some(id), "{rel}");
    }

    // A second open cooks nothing: the first import left a complete database.
    let again = regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions::for_host(root.clone()))
        .expect("the second cook runs");
    assert_eq!(again.cooked, 0, "{again:?}");
    assert!(again.errors.is_empty(), "{:#?}", again.errors);
}
