// SPDX-License-Identifier: MPL-2.0
//! Shipped templates ask only for engine features that exist: every `regia` or `regia_dylib`
//! feature a template manifest names must be in the engine crates' `[features]` tables.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn feature_names(manifest: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut in_features = false;
    for line in manifest.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.starts_with('[') {
            in_features = line == "[features]";
        } else if in_features && let Some((name, _)) = line.split_once('=') {
            names.insert(name.trim().to_owned());
        }
    }
    names
}

fn dependencies_with_features(manifest: &str) -> Vec<(String, Vec<String>)> {
    let mut dependencies = Vec::new();
    let mut pending: Option<(String, String)> = None;
    for line in manifest.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if let Some((dependency, mut declaration)) = pending.take() {
            declaration.push_str(line);
            if declaration.matches('{').count() == declaration.matches('}').count() {
                add_dependency_features(&mut dependencies, &dependency, &declaration);
            } else {
                pending = Some((dependency, declaration));
            }
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let dependency = name.trim();
        if !matches!(dependency, "regia" | "regia_dylib") || !value.trim_start().starts_with('{') {
            continue;
        }
        if value.matches('{').count() == value.matches('}').count() {
            add_dependency_features(&mut dependencies, dependency, value);
        } else {
            pending = Some((dependency.to_owned(), value.to_owned()));
        }
    }
    dependencies
}

fn add_dependency_features(
    dependencies: &mut Vec<(String, Vec<String>)>,
    dependency: &str,
    declaration: &str,
) {
    let Some((_, features)) = declaration.split_once("features") else {
        return;
    };
    let Some((_, list)) = features.split_once('[') else {
        return;
    };
    let Some((list, _)) = list.split_once(']') else {
        return;
    };
    let names = list.split('"').skip(1).step_by(2).map(str::to_owned).collect();
    dependencies.push((dependency.to_owned(), names));
}

fn template_manifests(directory: &Path, manifests: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            template_manifests(&path, manifests)?;
        } else if path.extension().is_some_and(|extension| extension == "toml")
            || path.to_string_lossy().ends_with(".toml.tmpl")
        {
            manifests.push(path);
        }
    }
    Ok(())
}

fn feature_errors(
    path: &Path,
    contents: &str,
    regia_features: &BTreeSet<String>,
    dylib_features: &BTreeSet<String>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for (dependency, features) in dependencies_with_features(contents) {
        for feature in features {
            if !regia_features.contains(&feature) {
                errors.push(format!("{}: unknown regia feature `{feature}`", path.display()));
            }
            if !dylib_features.contains(&feature) {
                errors.push(format!(
                    "{}: regia_dylib does not forward feature `{feature}` (dependency {dependency})",
                    path.display()
                ));
            }
        }
    }
    errors
}

#[test]
fn template_engine_features_exist_and_are_forwarded() {
    let regia = feature_names(include_str!("../../regia/Cargo.toml"));
    let dylib = feature_names(include_str!("../../regia_dylib/Cargo.toml"));
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
    let mut manifests = Vec::new();
    template_manifests(&root, &mut manifests).expect("read template manifests");

    let errors: Vec<_> = manifests
        .iter()
        .flat_map(|path| {
            let contents = fs::read_to_string(path).expect("read template manifest as text");
            feature_errors(path, &contents, &regia, &dylib)
        })
        .collect();
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn rejects_unknown_engine_feature_in_fixture_text() {
    let fixture = r#"[dependencies]
regia = { path = "{{engine}}/crates/regia", features = ["not-a-real-feature"] }
"#;
    let errors = feature_errors(
        Path::new("fixture/Cargo.toml.tmpl"),
        fixture,
        &BTreeSet::from(["client".to_owned()]),
        &BTreeSet::from(["client".to_owned()]),
    );
    assert!(
        errors
            .iter()
            .any(|error| error.contains("fixture/Cargo.toml.tmpl") && error.contains("not-a-real-feature")),
        "fixture feature should fail with path and name: {errors:?}"
    );
}

#[test]
fn every_3d_template_spawns_a_directional_light() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
    for (template, source) in [
        ("top_down", "level.rs"),
        ("first_person", "level.rs"),
        ("third_person", "level.rs"),
        ("net_game", "view.rs"),
    ] {
        let path = root.join(template).join("game/src").join(source);
        let text = fs::read_to_string(&path).expect("read 3D template scene setup");
        assert_eq!(
            text.matches("DirectionalLight {").count(),
            1,
            "{} must spawn exactly one sun",
            path.display()
        );
        if template != "net_game" {
            assert!(
                text.contains("cast_shadows: true"),
                "{template}: sun casts shadows"
            );
        }
    }
}
