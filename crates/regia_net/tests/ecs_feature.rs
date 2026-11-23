// SPDX-License-Identifier: MPL-2.0
//! The ECS-free core of `regia_net` (spec 120 § 3.2, ADR-8032): the ECS, app, scene, asset and
//! platform crates are optional and come in only through the default-on `ecs` feature, so
//! `cargo check -p regia_net --no-default-features` builds the core without them (a headless server
//! that keeps its own simulation links no ECS). The dependency-tree assertion over
//! `cargo tree --no-default-features` is the gate check of spec 120 § 15.5.

use std::path::Path;

/// The crates only the `ecs` feature may bring in.
const ECS_CRATES: [&str; 5] = [
    "regia_ecs",
    "regia_app",
    "regia_scene",
    "regia_asset",
    "regia_platform",
];

fn manifest() -> toml::Table {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    text.parse::<toml::Table>()
        .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn feature(manifest: &toml::Table, name: &str) -> Vec<String> {
    manifest
        .get("features")
        .and_then(|f| f.get(name))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

#[test]
fn the_ecs_crates_are_optional_and_only_the_ecs_feature_enables_them() {
    let manifest = manifest();
    let deps = manifest
        .get("dependencies")
        .and_then(|d| d.as_table())
        .expect("[dependencies]");
    for name in ECS_CRATES {
        let optional = deps
            .get(name)
            .and_then(|d| d.get("optional"))
            .and_then(toml::Value::as_bool);
        assert_eq!(optional, Some(true), "{name} must be an optional dependency");
    }
    let ecs = feature(&manifest, "ecs");
    for name in ECS_CRATES {
        assert!(
            ecs.contains(&format!("dep:{name}")),
            "the ecs feature must enable {name}: {ecs:?}"
        );
    }
    // No other feature may pull an ECS crate in on its own (voice goes through `ecs`).
    let features = manifest
        .get("features")
        .and_then(|f| f.as_table())
        .expect("[features]");
    for (other, list) in features {
        if other == "ecs" {
            continue;
        }
        let list: Vec<&str> = list
            .as_array()
            .map(|a| a.iter().filter_map(toml::Value::as_str).collect())
            .unwrap_or_default();
        for name in ECS_CRATES {
            assert!(
                !list.contains(&format!("dep:{name}").as_str()),
                "feature {other} enables {name} directly; route it through `ecs`"
            );
        }
    }
}

#[test]
fn ecs_is_on_by_default_so_hosts_see_no_change() {
    let manifest = manifest();
    assert_eq!(feature(&manifest, "default"), ["ecs"], "default features");
    assert!(
        feature(&manifest, "voice").iter().any(|f| f == "ecs"),
        "voice runs on the ECS session, so it needs ecs"
    );
}

/// The default build still has the ECS half at its old paths (a compile check that also runs).
#[test]
#[cfg(feature = "ecs")]
fn the_default_build_keeps_the_ecs_types_at_their_paths() {
    use regia_ecs::prelude::{Component, Resource};
    fn component<T: Component>() {}
    fn resource<T: Resource>() {}
    component::<regia_net::NetId>();
    component::<regia_net::NetOwner>();
    component::<regia_net::NetRole>();
    resource::<regia_net::NetMode>();
    resource::<regia_net::LocalPeer>();
    resource::<regia_net::NetSettings>();
    resource::<regia_net::NetAgentState>();
    resource::<regia_net::NetSession>();
    let _ = regia_net::lobby::plugin::LobbySettings::default();
    let _ = regia_net::lobby::LobbySettings::default();
    let _ = regia_net::codec::CodecContext::detached().without_loading();
}
