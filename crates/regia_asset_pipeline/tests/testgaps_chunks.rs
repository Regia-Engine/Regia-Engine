// SPDX-License-Identifier: MPL-2.0
//! Test-gap audit: chunk assignment and chunk paks must not depend on the order assets were
//! registered in, and an unrelated new asset must not move the others.

use std::path::Path;

use regia_asset::content::BASE_CHUNK;
use regia_asset::registry::{AssetRegistry, RegistryEntry};
use regia_asset::{AssetId, AssetManifest, ManifestEntry, cooked_file_name};
use regia_asset_pipeline::chunks::{ChunkRules, ChunkWriteOptions, assign, write_chunks};
use regia_core::id::Guid;

fn id(n: u128) -> AssetId {
    AssetId(Guid::from_u128(n))
}

fn rec(n: u128, path: &str, deps: &[u128]) -> RegistryEntry {
    RegistryEntry {
        id: id(n),
        path: format!("assets/{path}"),
        type_path: "regia_asset::CpuMesh".into(),
        dependencies: deps.iter().map(|d| id(*d)).collect(),
        tags: Default::default(),
        size: 0,
    }
}

fn rules() -> ChunkRules {
    ChunkRules::from_manifest_toml(
        r#"
[cook.chunks.arena]
include = ["levels/arena/**"]
priority = 10
delivery = "on_demand"
[cook.chunks.forest]
include = ["levels/forest/**"]
delivery = "on_demand"
"#,
    )
    .expect("rules parse")
}

fn base_entries() -> Vec<RegistryEntry> {
    vec![
        rec(1, "levels/arena/a.level.ron", &[10]),
        rec(10, "tex/arena_floor.png", &[]),
        rec(2, "levels/forest/f.level.ron", &[11]),
        rec(11, "tex/bark.png", &[]),
        rec(4, "ui/menu.png", &[]),
    ]
}

/// Assignment is a pure function of the asset set: reversing the registration order or adding an
/// asset no rule selects and nothing references changes nothing for the others.
#[test]
fn assignment_ignores_registration_order_and_unrelated_additions() {
    let r = rules();
    let reference = assign(&AssetRegistry::from_entries(base_entries()), &r, &[]).expect("assign");
    assert_eq!(
        reference.chunk_of[&id(10)],
        "arena",
        "a dependency follows its level"
    );
    assert_eq!(reference.chunk_of[&id(4)], BASE_CHUNK);

    let mut reversed = base_entries();
    reversed.reverse();
    let rev = assign(&AssetRegistry::from_entries(reversed), &r, &[]).expect("assign reversed");
    assert_eq!(rev.chunk_of, reference.chunk_of);
    assert_eq!(rev.reason, reference.reason);

    let mut more = base_entries();
    more.push(rec(99, "audio/unrelated.ogg", &[]));
    let with = assign(&AssetRegistry::from_entries(more), &r, &[]).expect("assign + unrelated");
    for (asset, chunk) in &reference.chunk_of {
        assert_eq!(with.chunk_of.get(asset), Some(chunk), "asset {asset:?} moved");
        assert_eq!(with.reason.get(asset), reference.reason.get(asset));
    }
    assert_eq!(with.chunk_of[&id(99)], BASE_CHUNK);
}

fn cache(root: &Path, assets: &[(u128, &str)]) -> std::path::PathBuf {
    let cache = root.join(".regia/cooked/linux");
    std::fs::create_dir_all(&cache).expect("mkdir");
    let mut manifest = AssetManifest::default();
    for (n, path) in assets {
        std::fs::write(
            cache.join(cooked_file_name(id(*n))),
            format!("payload-{n}").repeat(64),
        )
        .expect("cooked");
        manifest.entries.push(ManifestEntry {
            path: format!("assets/{path}"),
            id: id(*n),
            type_path: "regia_asset::CookedData".into(),
        });
    }
    manifest.entries.sort_by(|a, b| a.path.cmp(&b.path));
    manifest.write_to_cache(&cache).expect("manifest");
    cache
}

fn opts() -> ChunkWriteOptions {
    ChunkWriteOptions {
        compress: true,
        min_ratio: 0.0,
        gpu_meshes: false,
        build_id: "1.0-1-test".into(),
        platform: "linux".into(),
        compatible_with: Vec::new(),
    }
}

/// Adding an asset to one chunk rewrites only that chunk's pak; the other paks keep their
/// hashes, and the pak bytes do not depend on registration order.
#[test]
fn an_added_asset_only_changes_its_own_chunk_pak() {
    let root = regia_test_utils::unique_temp_dir("testgaps-chunks");
    let r = rules();
    let assets = [
        (1u128, "levels/arena/a.level.ron"),
        (2, "levels/forest/f.level.ron"),
        (4, "ui/menu.png"),
    ];
    let entries = |extra: bool, rev: bool| {
        let mut v: Vec<RegistryEntry> = assets.iter().map(|(n, p)| rec(*n, p, &[])).collect();
        if extra {
            v.push(rec(3, "levels/forest/g.glb", &[]));
        }
        if rev {
            v.reverse();
        }
        AssetRegistry::from_entries(v)
    };
    let mut all = assets.to_vec();
    all.push((3, "levels/forest/g.glb"));
    let cache = cache(&root, &all);

    let a0 = assign(&entries(false, false), &r, &[]).expect("a0");
    let (m0, _) = write_chunks(&cache, &root.join("p0"), &a0, &r, &opts()).expect("w0");
    let a0r = assign(&entries(false, true), &r, &[]).expect("a0r");
    let (m0r, _) = write_chunks(&cache, &root.join("p0r"), &a0r, &r, &opts()).expect("w0r");
    assert_eq!(m0, m0r, "registration order must not change any pak");

    let a1 = assign(&entries(true, false), &r, &[]).expect("a1");
    let (m1, _) = write_chunks(&cache, &root.join("p1"), &a1, &r, &opts()).expect("w1");
    for stable in [BASE_CHUNK, "arena"] {
        assert_eq!(m1.chunk(stable), m0.chunk(stable), "{stable} pak changed");
    }
    assert_ne!(
        m1.chunk("forest").expect("f").hash,
        m0.chunk("forest").expect("f").hash
    );
    assert_eq!(m1.chunk("forest").expect("f").assets, 2);
    let _ = std::fs::remove_dir_all(&root);
}
