// SPDX-License-Identifier: MPL-2.0
//! Spec 57 § 10: the numbers behind the performance table, for the lane report. Run with
//! `cargo test --release -p regia_framework --test extension_perf -- --ignored --nocapture`.
//! They print and assert nothing about time (timing belongs in a report, not in a gate); the
//! counters they do assert keep each measurement honest about what it measured.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use regia_app::prelude::*;
use regia_ecs::reflect::AppTypeRegistry;
use regia_framework::extension::discover::{self, MANIFEST_FILE};
use regia_framework::extension::prelude::*;
use regia_framework::extension::{
    ExtensionPlugin, ExtensionRegistry, ExtensionsConfig, RegistryInputs, resolve,
};
use regia_framework::extension::{ResolveInput, ResolvedSet};
use regia_scene::components::Tags;
use regia_test_utils::unique_temp_dir;
use semver::Version;

fn manifest_text(id: usize, deps: &[usize]) -> String {
    let mut text = format!("format_version = 1\nid = \"ext{id:03}\"\nversion = \"1.0.0\"\n");
    for dep in deps {
        write!(
            text,
            "\n[[dependency]]\nid = \"ext{dep:03}\"\nversion = \">=1\"\n"
        )
        .unwrap();
    }
    text
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Each extension depends on up to three lower ids, so `count` extensions carry about three edges.
fn deps_of(id: usize) -> Vec<usize> {
    (1..=3).filter(|d| *d <= id).map(|d| id - d).collect()
}

#[test]
#[ignore = "prints timings for the report"]
fn discovery_of_50_extensions() {
    let root = unique_temp_dir("ext-perf-discover");
    for id in 0..50 {
        write(
            &root.join(format!("ext{id:03}")).join(MANIFEST_FILE),
            &manifest_text(id, &deps_of(id)),
        );
    }
    let started = Instant::now();
    let found = discover::discover(std::slice::from_ref(&root));
    println!("extension_discover 50 manifests: {:?}", started.elapsed());
    assert_eq!(found.manifests.len(), 50);
    let _ = std::fs::remove_dir_all(&root);
}

fn resolve_all(found: &[regia_framework::extension::DiscoveredManifest], enabled: &[String]) -> ResolvedSet {
    resolve(&ResolveInput {
        discovered: found,
        enabled,
        disabled: &[],
        grants: &std::collections::BTreeMap::new(),
        engine_version: &Version::new(0, 2, 6),
        platform: "linux",
        profile: "dev",
        dynamic_code: true,
        reserved_ids: &[],
    })
}

#[test]
#[ignore = "prints timings for the report"]
fn resolution_of_100_extensions_and_300_edges() {
    let root = unique_temp_dir("ext-perf-resolve");
    for id in 0..100 {
        write(
            &root.join(format!("ext{id:03}")).join(MANIFEST_FILE),
            &manifest_text(id, &deps_of(id)),
        );
    }
    let found = discover::discover(std::slice::from_ref(&root));
    let enabled: Vec<String> = found.manifests.iter().map(|m| m.manifest.id.clone()).collect();
    let edges: usize = found
        .manifests
        .iter()
        .map(|m| m.manifest.dependencies.len())
        .sum();
    let started = Instant::now();
    let set = resolve_all(&found.manifests, &enabled);
    println!(
        "extension_resolve 100 extensions, {edges} edges: {:?}",
        started.elapsed()
    );
    assert_eq!(set.order.len(), 100);
    let _ = std::fs::remove_dir_all(&root);
}

#[derive(Component, Reflect, Default)]
#[reflect(Component, Default)]
struct Wet;

struct Rainy;
impl RegiaExtension for Rainy {
    fn id(&self) -> &str {
        "rainy"
    }
    fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
        cx.declare_feature_pack(FeaturePackDesc {
            id: "rain".into(),
            tag: "outdoor".into(),
            components: vec![ComponentSpec {
                type_path: Wet::get_type_registration().type_info().type_path().to_owned(),
            }],
            requires_level_feature: "rain_weather".into(),
        })
    }
}

#[test]
#[ignore = "prints timings for the report"]
fn pack_activation_over_1000_entities_and_the_idle_frame() {
    let manifest = regia_framework::extension::ExtensionManifest::parse(
        "format_version = 1\nid = \"rainy\"\nversion = \"1.0.0\"\ncapabilities = [\"feature_pack\"]\n\n[[code]]\ncrate = \"x\"\naudience = [\"game\"]\n",
    )
    .unwrap();
    let mut discovery = discover::Discovery::default();
    discovery
        .manifests
        .push(regia_framework::extension::DiscoveredManifest {
            manifest,
            path: String::new(),
            hash: "0".into(),
        });
    let config = ExtensionsConfig::from_project_toml(
        "[extensions]\nenabled = [\"rainy\"]\n[extensions.grants]\nrainy = [\"feature_pack\"]\n",
    )
    .unwrap();
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.world_mut()
        .resource_mut::<AppTypeRegistry>()
        .write()
        .register::<Wet>();
    app.add_plugins(ExtensionPlugin::from_inputs(
        RegistryInputs::for_host(None, config, discovery),
        vec![Arc::new(Rainy)],
    ));
    for _ in 0..1000 {
        app.world_mut().spawn(Tags::from_iter_str(["outdoor"]));
    }
    app.update();
    let idle = Instant::now();
    for _ in 0..1000 {
        regia_framework::extension::registry::extension_frame(app.world_mut());
    }
    println!("extension_frame idle, per frame: {:?}", idle.elapsed() / 1000);
    app.world_mut()
        .resource_mut::<ExtensionRegistry>()
        .enable_level_feature("rain_weather", "levels/a.level.ron");
    let started = Instant::now();
    regia_framework::extension::registry::extension_frame(app.world_mut());
    println!("extension_pack_activate 1000 entities: {:?}", started.elapsed());
    assert_eq!(
        app.world()
            .resource::<ExtensionRegistry>()
            .packs()
            .applied_count("rain"),
        1000
    );
}
