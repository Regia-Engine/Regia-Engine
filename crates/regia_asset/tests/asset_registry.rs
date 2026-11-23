// SPDX-License-Identifier: MPL-2.0
//! Asset registry queries without loading assets (`docs/specs/content-data.md` § 2).

use std::collections::BTreeMap;

use regia_asset::AssetId;
use regia_asset::registry::{
    AssetRegistry, LoadedAssetRegistry, REGISTRY_FORMAT_VERSION, REGISTRY_MAGIC, RegistryChange,
    RegistryEntry, RegistryError, RegistryQuery, TagFilter, TagRules, automatic_tags, cache_registry_path,
    pak_registry_path,
};
use regia_core::id::Guid;

fn id(n: u128) -> AssetId {
    AssetId(Guid::from_u128(n))
}

fn rec(n: u128, path: &str, ty: &str, deps: &[u128], tags: &[(&str, &str)]) -> RegistryEntry {
    let mut all = automatic_tags(path);
    for (k, v) in tags {
        all.insert((*k).to_string(), (*v).to_string());
    }
    RegistryEntry {
        id: id(n),
        path: path.to_string(),
        type_path: ty.to_string(),
        dependencies: deps.iter().map(|d| id(*d)).collect(),
        tags: all,
        size: 100 + n as u64,
    }
}

const TEX: &str = "regia_asset::CookedTexture";
const MAT: &str = "regia_asset::CookedStandardMaterial";
const MESH: &str = "regia_asset::CookedMesh";

/// A small project: a tree glTF (mesh → material → two textures), a rock sharing one texture, a UI icon.
fn sample() -> AssetRegistry {
    AssetRegistry::from_entries([
        rec(
            1,
            "assets/props/forest/tree.glb",
            MESH,
            &[2],
            &[("biome", "forest")],
        ),
        rec(
            2,
            "assets/props/forest/tree.glb#material/Bark",
            MAT,
            &[3, 4],
            &[("biome", "forest")],
        ),
        rec(3, "assets/textures/bark.png", TEX, &[], &[]),
        rec(4, "assets/textures/moss.png", TEX, &[], &[("biome", "forest")]),
        rec(5, "assets/props/rock.glb", MESH, &[6], &[("biome", "desert")]),
        rec(6, "assets/props/rock.glb#material/Stone", MAT, &[4, 99], &[]),
        rec(7, "assets/ui/icon.png", TEX, &[], &[("group", "ui")]),
    ])
}

fn paths(entries: &[&RegistryEntry]) -> Vec<String> {
    entries.iter().map(|e| e.path.clone()).collect()
}

#[test]
fn query_by_tag_type_and_path() {
    let reg = sample();
    let forest = reg.query(&RegistryQuery {
        tags: vec![TagFilter::parse("biome=forest").expect("filter")],
        ..Default::default()
    });
    assert_eq!(
        paths(&forest.entries),
        [
            "assets/props/forest/tree.glb",
            "assets/props/forest/tree.glb#material/Bark",
            "assets/textures/moss.png"
        ]
    );
    let textures = reg.query(&RegistryQuery {
        type_path: Some("CookedTexture".into()),
        ..Default::default()
    });
    assert_eq!(textures.total, 3);
    let has_biome_and_mesh = reg.query(&RegistryQuery {
        type_path: Some(MESH.into()),
        tags: vec![TagFilter::parse("biome").expect("filter")],
        ..Default::default()
    });
    assert_eq!(
        paths(&has_biome_and_mesh.entries),
        ["assets/props/forest/tree.glb", "assets/props/rock.glb"]
    );
    let by_glob = reg.query(&RegistryQuery {
        path: Some("assets/props/**".into()),
        limit: Some(2),
        ..Default::default()
    });
    assert_eq!(by_glob.total, 4, "the limit does not change the total");
    assert_eq!(by_glob.entries.len(), 2);
    let contains = reg.query(&RegistryQuery {
        path_contains: Some("MOSS".into()),
        ..Default::default()
    });
    assert_eq!(paths(&contains.entries), ["assets/textures/moss.png"]);
    let by_ext = reg.query(&RegistryQuery {
        tags: vec![TagFilter::parse("ext=png").expect("filter")],
        ..Default::default()
    });
    assert_eq!(by_ext.total, 3, "automatic ext tag");
}

#[test]
fn dependencies_and_referencers() {
    let reg = sample();
    let direct = reg.dependencies(id(1), false);
    assert_eq!(
        paths(&direct.entries),
        ["assets/props/forest/tree.glb#material/Bark"]
    );
    let all = reg.dependencies(id(1), true);
    assert_eq!(
        paths(&all.entries),
        [
            "assets/props/forest/tree.glb#material/Bark",
            "assets/textures/bark.png",
            "assets/textures/moss.png"
        ]
    );
    let rock = reg.dependencies(id(5), true);
    assert_eq!(
        rock.missing,
        vec![id(99)],
        "unknown ids are reported, not dropped"
    );
    let moss_refs = reg.referencers(id(4), false);
    assert_eq!(
        paths(&moss_refs.entries),
        [
            "assets/props/forest/tree.glb#material/Bark",
            "assets/props/rock.glb#material/Stone"
        ]
    );
    let moss_all = reg.referencers(id(4), true);
    assert_eq!(moss_all.entries.len(), 4, "both materials and both meshes");
    assert!(reg.referencers(id(7), true).entries.is_empty());
}

#[test]
fn walks_terminate_on_cycles() {
    let reg = AssetRegistry::from_entries([
        rec(1, "assets/a.ron", TEX, &[2], &[]),
        rec(2, "assets/b.ron", TEX, &[3], &[]),
        rec(3, "assets/c.ron", TEX, &[1], &[]),
    ]);
    assert_eq!(reg.dependencies(id(1), true).entries.len(), 2);
    assert_eq!(reg.referencers(id(1), true).entries.len(), 2);
}

#[test]
fn file_round_trip_and_header_checks() {
    let reg = sample();
    let bytes = reg.to_bytes().expect("encode");
    assert_eq!(bytes[..8], REGISTRY_MAGIC);
    assert_eq!(
        u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        REGISTRY_FORMAT_VERSION
    );
    let back = AssetRegistry::from_bytes(&bytes).expect("decode");
    assert_eq!(back, reg);
    assert!(matches!(
        AssetRegistry::from_bytes(b"nope"),
        Err(RegistryError::BadMagic)
    ));
    let mut newer = bytes.clone();
    newer[8..12].copy_from_slice(&(REGISTRY_FORMAT_VERSION + 1).to_le_bytes());
    assert!(matches!(
        AssetRegistry::from_bytes(&newer),
        Err(RegistryError::UnsupportedVersion { .. })
    ));
    let truncated = &bytes[..bytes.len() - 3];
    assert!(AssetRegistry::from_bytes(truncated).is_err());
}

#[test]
fn encoding_is_independent_of_insertion_order() {
    let a = sample();
    let mut entries: Vec<RegistryEntry> = a.entries().cloned().collect();
    entries.reverse();
    let b = AssetRegistry::from_entries(entries);
    assert_eq!(a.to_bytes().expect("a"), b.to_bytes().expect("b"));
}

/// A deterministic pseudo-random edit sequence (xorshift), applied incrementally, must leave the
/// registry equal (records and reverse index) to a full rebuild from the final records.
#[test]
fn incremental_updates_equal_a_full_rebuild() {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut live = AssetRegistry::default();
    let mut truth: BTreeMap<u128, RegistryEntry> = BTreeMap::new();
    for step in 0..2_000u32 {
        let n = u128::from(next() % 40);
        match next() % 4 {
            0 => {
                if let Some(old) = truth.remove(&n) {
                    live.apply_changes([RegistryChange::RemovePath(old.path)]);
                }
            }
            _ => {
                let deps: Vec<u128> = (0..next() % 4).map(|_| u128::from(next() % 45)).collect();
                // Paths stay unique per id; an id sometimes moves to a second path.
                let path = format!("assets/{}/{n}.png", if next() % 5 == 0 { "moved" } else { "a" });
                let entry = rec(n, &path, TEX, &deps, &[("step", &step.to_string())]);
                truth.insert(n, entry.clone());
                live.apply_changes([RegistryChange::Upsert(entry)]);
            }
        }
    }
    let full = AssetRegistry::from_entries(truth.into_values());
    assert_eq!(live, full);
    assert!(live.generation > 0);
}

#[test]
fn replace_source_drops_stale_sub_assets() {
    let mut reg = sample();
    let changed = reg.replace_source(
        "assets/props/rock.glb",
        vec![rec(5, "assets/props/rock.glb", MESH, &[3], &[])],
    );
    assert_eq!(changed, 2, "the material is gone and the mesh changed");
    assert!(reg.by_path("assets/props/rock.glb#material/Stone").is_none());
    assert_eq!(
        paths(&reg.referencers(id(3), false).entries),
        [
            "assets/props/forest/tree.glb#material/Bark",
            "assets/props/rock.glb"
        ]
    );
    let removed = reg.apply_changes([RegistryChange::RemoveSource(
        "assets/props/forest/tree.glb".into(),
    )]);
    assert_eq!(removed, 2);
    assert!(
        reg.by_path("assets/props/rock.glb").is_some(),
        "other sources stay"
    );
}

#[test]
fn resolve_accepts_ids_and_paths() {
    let reg = sample();
    assert_eq!(reg.resolve(&id(3).to_string()).map(|e| e.id), Some(id(3)));
    assert_eq!(reg.resolve("textures/bark.png").map(|e| e.id), Some(id(3)));
    assert_eq!(reg.resolve("assets/textures/bark.png").map(|e| e.id), Some(id(3)));
    assert!(reg.resolve("nope.png").is_none());
}

#[test]
fn tag_browsing() {
    let reg = sample();
    let keys = reg.tag_keys();
    assert_eq!(keys.get("biome"), Some(&4));
    let values = reg.tag_values("biome");
    assert_eq!(values.get("forest"), Some(&3));
    assert_eq!(values.get("desert"), Some(&1));
}

#[test]
fn tag_filters_reject_bad_keys() {
    assert!(TagFilter::parse("").is_err());
    assert!(TagFilter::parse("=x").is_err());
    assert_eq!(
        TagFilter::parse("biome = forest").expect("trimmed"),
        TagFilter::Equals("biome".into(), "forest".into())
    );
}

#[test]
fn loaded_registry_merges_paks_and_notices_rewrites() {
    let dir = regia_test_utils::unique_temp_dir("asset-registry-load");
    let cache = dir.join("cache");
    let content = dir.join("content");
    std::fs::create_dir_all(&cache).expect("cache");
    std::fs::create_dir_all(&content).expect("content");
    assert!(
        LoadedAssetRegistry::load(&cache, Some(&content))
            .expect("load")
            .is_none()
    );

    let base = AssetRegistry::from_entries([rec(1, "assets/a.png", TEX, &[], &[("v", "base")])]);
    let chunk = AssetRegistry::from_entries([
        rec(1, "assets/a.png", TEX, &[], &[("v", "chunk")]),
        rec(2, "assets/b.png", TEX, &[1], &[]),
    ]);
    base.write_to(&pak_registry_path(&content.join("base.fpak")))
        .expect("base");
    chunk
        .write_to(&pak_registry_path(&content.join("chunk_x.fpak")))
        .expect("chunk");
    assert!(content.join("base.registry.bin").is_file());
    let loaded = LoadedAssetRegistry::load(&cache, Some(&content))
        .expect("load")
        .expect("some");
    assert_eq!(loaded.registry.len(), 2);
    assert_eq!(
        loaded
            .registry
            .by_id(id(1))
            .and_then(|e| e.tags.get("v"))
            .map(String::as_str),
        Some("chunk"),
        "later paks override earlier ones"
    );
    assert!(!loaded.is_stale());

    // A cache registry on top; rewriting it (a different size) makes the loaded one stale.
    let dev = AssetRegistry::from_entries([rec(3, "assets/c.png", TEX, &[], &[])]);
    dev.write_to(&cache_registry_path(&cache)).expect("dev");
    let loaded = LoadedAssetRegistry::load(&cache, Some(&content))
        .expect("load")
        .expect("some");
    assert_eq!(loaded.registry.len(), 3);
    let bigger = AssetRegistry::from_entries([
        rec(3, "assets/c.png", TEX, &[], &[]),
        rec(4, "assets/d.png", TEX, &[3], &[]),
    ]);
    bigger.write_to(&cache_registry_path(&cache)).expect("rewrite");
    assert!(loaded.is_stale());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tag_rules_file_applies_to_sub_assets() {
    let rules = TagRules::from_ron(
        r#"(
            format_version: 1,
            rules: [
                (paths: ["assets/props/forest/**"], tags: { "biome": "forest" }),
                (paths: ["assets/ui/**"], tags: { "group": "ui", "streaming": "always" }),
            ],
        )"#,
    )
    .expect("rules");
    assert_eq!(
        rules
            .tags_for("assets/props/forest/tree.glb#material/Bark")
            .get("biome")
            .map(String::as_str),
        Some("forest")
    );
    assert_eq!(rules.tags_for("assets/ui/icon.png").len(), 2);
    let dir = regia_test_utils::unique_temp_dir("asset-tags-load");
    let (none, warnings) = TagRules::load(&dir);
    assert!(
        none.rules.is_empty() && warnings.is_empty(),
        "a missing file is no rules"
    );
    std::fs::create_dir_all(dir.join("config")).expect("config");
    std::fs::write(dir.join("config/asset_tags.ron"), "(format_version: 9)").expect("write");
    let (bad, warnings) = TagRules::load(&dir);
    assert!(bad.rules.is_empty());
    assert_eq!(warnings.len(), 1, "a bad file is one warning: {warnings:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
