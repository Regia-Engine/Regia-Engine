// SPDX-License-Identifier: MPL-2.0
//! Spec 57 § 4.4 and § 8 test 8: a cook takes the referenced closure of extension content and
//! nothing else, the project shadows an extension's file, and extension directories stay untouched.

use std::path::{Path, PathBuf};

use regia_asset::{AssetManifest, MaterialAssetRon, cooked_cache_dir};
use regia_asset_pipeline::extension_cook::{
    ExtensionCookOptions, ExtensionCookReport, ExtensionRoot, cook_extension_content,
};
use regia_asset_pipeline::{CookOptions, cook_project, host_platform};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(path, text).expect("write");
}

fn material(name: &str) -> String {
    let mut mat = MaterialAssetRon::default_standard();
    mat.name = Some(name.to_owned());
    mat.to_ron_string().expect("material ron")
}

struct Project(PathBuf);

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Project {
    /// `dialogue` provides a referenced graph (one file naming the next) and an unreferenced file.
    fn new(tag: &str) -> Self {
        let root = regia_test_utils::unique_temp_dir(tag);
        let ext = root.join("extensions/dialogue/content");
        write(
            &ext.join("talk/one.ron"),
            "(next: \"ext://dialogue/talk/two.dlg\")",
        );
        write(&ext.join("talk/two.dlg"), "HELLO");
        write(&ext.join("unused.dlg"), "NOBODY NAMES ME");
        write(&ext.join("rock.material.ron"), &material("rock"));
        write(
            &ext.join("shadowed.material.ron"),
            &material("from the extension"),
        );
        write(
            &root.join("assets/shadowed.material.ron"),
            &material("from the project"),
        );
        write(
            &root.join("src/game.rs"),
            "const A: &str = \"ext://dialogue/talk/one.ron\";\n\
             const B: &str = \"ext://dialogue/rock.material.ron\";\n\
             const C: &str = \"ext://dialogue/shadowed.material.ron\";\n\
             const D: &str = \"ext://ghost/none.png\";\n",
        );
        Self(root)
    }

    fn options(&self) -> ExtensionCookOptions {
        ExtensionCookOptions {
            project_root: self.0.clone(),
            platform: host_platform().to_owned(),
            incremental: true,
            roots: vec![ExtensionRoot {
                id: "dialogue".into(),
                root: self.0.join("extensions/dialogue/content"),
            }],
            record: Some(("ext://regia/extensions.ron".into(), "(format_version: 1)".into())),
        }
    }

    fn cook(&self) -> ExtensionCookReport {
        let report = cook_project(CookOptions::for_host(self.0.clone())).expect("cook");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        cook_extension_content(&self.options()).expect("extension cook")
    }

    fn manifest(&self) -> AssetManifest {
        AssetManifest::read_from_cache(&cooked_cache_dir(&self.0, host_platform()))
            .expect("read")
            .expect("manifest")
    }

    fn cooked_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self
            .manifest()
            .entries
            .into_iter()
            .map(|e| e.path)
            .filter(|p| p.starts_with("ext://"))
            .collect();
        paths.sort();
        paths
    }
}

#[test]
fn only_the_referenced_closure_is_cooked() {
    let p = Project::new("ext-cook-closure");
    let report = p.cook();
    assert_eq!(
        p.cooked_paths(),
        [
            "ext://dialogue/rock.material.ron",
            "ext://dialogue/shadowed.material.ron",
            "ext://dialogue/talk/one.ron",
            "ext://dialogue/talk/two.dlg",
            "ext://regia/extensions.ron",
        ],
        "`two.dlg` comes in through `one.ron`; `unused.dlg` stays out"
    );
    assert_eq!(report.missing, ["ext://ghost/none.png"]);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
}

#[test]
fn the_project_shadows_an_extension_file() {
    let p = Project::new("ext-cook-shadow");
    let report = p.cook();
    assert_eq!(report.shadowed, ["ext://dialogue/shadowed.material.ron"]);
    let manifest = p.manifest();
    assert_eq!(
        manifest.id_for_path("ext://dialogue/shadowed.material.ron"),
        manifest.id_for_path("assets/shadowed.material.ron"),
    );
    assert!(manifest.id_for_path("assets/shadowed.material.ron").is_some());
}

#[test]
fn the_extension_directory_is_left_as_it_was() {
    let p = Project::new("ext-cook-readonly");
    let before = list(&p.0.join("extensions"));
    p.cook();
    assert_eq!(
        list(&p.0.join("extensions")),
        before,
        "no `.meta` written beside the sources"
    );
}

#[test]
fn a_dropped_reference_leaves_the_cook_and_ids_stay_put() {
    let p = Project::new("ext-cook-stale");
    p.cook();
    let id = p.manifest().id_for_path("ext://dialogue/rock.material.ron");
    assert!(id.is_some());
    p.cook();
    assert_eq!(
        p.manifest().id_for_path("ext://dialogue/rock.material.ron"),
        id,
        "a second cook keeps the id"
    );
    write(
        &p.0.join("src/game.rs"),
        "const A: &str = \"ext://dialogue/talk/one.ron\";\n",
    );
    p.cook();
    assert_eq!(
        p.cooked_paths(),
        [
            "ext://dialogue/talk/one.ron",
            "ext://dialogue/talk/two.dlg",
            "ext://regia/extensions.ron",
        ]
    );
}

/// Audit finding 2: a referenced file that cooked before but fails now keeps its previous
/// manifest row, id and cooked file; the failure is still reported.
#[test]
fn a_failed_recook_keeps_the_previous_row_and_cooked_file() {
    let p = Project::new("ext-cook-fail-keeps");
    p.cook();
    let id = p
        .manifest()
        .id_for_path("ext://dialogue/rock.material.ron")
        .expect("row after the first cook");
    let cache = cooked_cache_dir(&p.0, host_platform());
    let before: Vec<PathBuf> = list(&cache);

    // The importer rejects the replacement bytes.
    write(
        &p.0.join("extensions/dialogue/content/rock.material.ron"),
        "this is not a material",
    );
    let report = cook_extension_content(&p.options()).expect("extension cook");
    assert!(
        report
            .errors
            .iter()
            .any(|(path, _)| path == "ext://dialogue/rock.material.ron"),
        "the failure is reported: {:?}",
        report.errors
    );
    assert_eq!(
        p.manifest().id_for_path("ext://dialogue/rock.material.ron"),
        Some(id),
        "the previous manifest row and id stay"
    );
    let after: Vec<PathBuf> = list(&cache);
    for file in &before {
        assert!(after.contains(file), "cooked file {} was swept", file.display());
    }
}

#[test]
fn a_disabled_extension_takes_its_content_out() {
    let p = Project::new("ext-cook-disabled");
    p.cook();
    let mut options = p.options();
    options.roots.clear();
    options.record = None;
    let report = cook_extension_content(&options).expect("cook");
    assert!(p.cooked_paths().is_empty(), "{:?}", p.cooked_paths());
    assert_eq!(report.missing.len(), 4, "{:?}", report.missing);
}

#[test]
fn a_project_with_no_extensions_is_not_touched() {
    let root = regia_test_utils::unique_temp_dir("ext-cook-none");
    let options = ExtensionCookOptions {
        project_root: root.clone(),
        platform: host_platform().to_owned(),
        incremental: true,
        roots: Vec::new(),
        record: None,
    };
    assert_eq!(
        cook_extension_content(&options).expect("cook"),
        ExtensionCookReport::default()
    );
    let _ = std::fs::remove_dir_all(&root);
}

fn list(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .map(|e| e.into_path())
        .collect();
    out.sort();
    out
}
