// SPDX-License-Identifier: MPL-2.0
//! Closure chunks (DO27-absorb row C-03, ADR-8092): with `[cook.chunk_options] content = "closure"`
//! the cook ships only what the declared entry points (`entry` in a chunk rule, and the startup
//! level) reach through the dependency lists. `include` and `tags` then only place shipped assets;
//! an asset reached from no entry point is in no pak and no chunk map.
//!
//! Requirements source: DO27's release diet tool (`shell-regia/tools/pack_diet.py` at d6ed37677,
//! requirements only, not copied) shipped "the catalog heroes and what those loaders open", and
//! DO27's `Regia.toml:70` listed every directory in `include` because the profile closure was a
//! heuristic.

use std::path::{Path, PathBuf};

use regia_asset::content::{BASE_CHUNK, ChunkMap};
use regia_asset::registry::{AssetRegistry, RegistryEntry};
use regia_asset::{AssetId, AssetManifest, ManifestEntry, cooked_file_name};
use regia_asset_pipeline::chunks::{
    Assignment, ChunkRules, ChunkWriteOptions, Reason, assign, check_references, why_table, write_chunks,
};
use regia_core::id::Guid;

fn id(n: u128) -> AssetId {
    AssetId(Guid::from_u128(n))
}

/// A registry record: `(n, path under assets/, dependencies)`.
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

fn rules(toml: &str) -> ChunkRules {
    ChunkRules::from_manifest_toml(toml).expect("rules parse")
}

fn chunk(a: &Assignment, n: u128) -> Option<&str> {
    a.chunk_of.get(&id(n)).map(String::as_str)
}

/// A cooked cache holding one file per record.
fn cache(root: &Path, records: &[RegistryEntry]) -> PathBuf {
    let cache = root.join(".regia/cooked/linux");
    std::fs::create_dir_all(&cache).expect("mkdir");
    let mut manifest = AssetManifest::default();
    for r in records {
        std::fs::write(cache.join(cooked_file_name(r.id)), r.path.repeat(16)).expect("cooked");
        manifest.entries.push(ManifestEntry {
            path: r.path.clone(),
            id: r.id,
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
        build_id: "1.0-1-closure".into(),
        platform: "linux".into(),
        compatible_with: Vec::new(),
    }
}

/// A level reaching a mesh and its texture, a menu reaching its picture, and two assets nothing
/// references: an old mesh under the level's folder and an unused texture under `textures/`.
fn records() -> Vec<RegistryEntry> {
    vec![
        rec(1, "levels/arena/arena.level.ron", &[2]),
        rec(2, "levels/arena/rock.glb", &[3]),
        rec(3, "textures/rock.png", &[]),
        rec(4, "textures/unused.png", &[]),
        rec(5, "levels/arena/old_rock.glb", &[]),
        rec(6, "ui/menu.ui", &[7]),
        rec(7, "ui/menu.png", &[]),
    ]
}

const CLOSURE_RULES: &str = r#"
[cook.chunk_options]
content = "closure"

[cook.chunks.base]
entry = ["ui/menu.ui"]

[cook.chunks.textures]
include = ["textures/**"]
priority = 5

[cook.chunks.arena]
entry = ["levels/arena/arena.level.ron"]
include = ["levels/arena/**"]
requires = ["textures"]
delivery = "on_demand"
"#;

/// The acceptance test of row C-03: an asset no entry point reaches is in no chunk, no pak and no
/// chunk map, even when an `include` glob of a chunk matches it.
#[test]
fn unreferenced_asset_not_shipped() {
    let root = regia_test_utils::unique_temp_dir("cook-closure-unreferenced");
    let recs = records();
    let cache = cache(&root, &recs);
    let reg = AssetRegistry::from_entries(recs);
    let r = rules(CLOSURE_RULES);
    let a = assign(&reg, &r, &[]).expect("assign");

    assert_eq!(chunk(&a, 1), Some("arena"), "the entry level");
    assert_eq!(chunk(&a, 2), Some("arena"), "the mesh the level uses");
    assert_eq!(
        chunk(&a, 3),
        Some("textures"),
        "its texture, placed by the include glob"
    );
    assert_eq!(chunk(&a, 6), Some(BASE_CHUNK));
    assert_eq!(chunk(&a, 7), Some(BASE_CHUNK));
    assert_eq!(
        chunk(&a, 4),
        None,
        "an unused texture under an include glob ships nowhere"
    );
    assert_eq!(
        chunk(&a, 5),
        None,
        "an unreferenced mesh under an include glob ships nowhere"
    );
    assert!(check_references(&reg, &a, &r).is_empty());

    let out = root.join("paks");
    let (manifest, report) = write_chunks(&cache, &out, &a, &r, &opts()).expect("write");
    assert!(report.unwritten.is_empty(), "{:?}", report.unwritten);
    let counts: Vec<(&str, u32)> = manifest
        .chunks
        .iter()
        .map(|c| (c.name.as_str(), c.assets))
        .collect();
    assert_eq!(counts, [("base", 2), ("textures", 1), ("arena", 2)]);
    let map = ChunkMap::read_from_dir(&out).expect("map").expect("present");
    assert_eq!(map.chunk_of(id(4)), None);
    assert_eq!(map.chunk_of(id(5)), None);
    assert_eq!(map.chunk_of(id(3)), Some("textures"));
    let why = why_table(&reg, &a, &r);
    assert!(why.explain(id(4)).is_none() && why.explain(id(5)).is_none());
    let _ = std::fs::remove_dir_all(&root);
}

/// Without `content = "closure"` nothing changes: unreferenced assets still go to their include
/// chunk or to base, and an `entry` selects its asset as `include` does (its selector index comes
/// after the include globs and tags).
#[test]
fn the_default_content_mode_ships_every_asset() {
    let reg = AssetRegistry::from_entries(records());
    let r = rules(&CLOSURE_RULES.replace("content = \"closure\"", "shared = \"base\""));
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    let a = assign(&reg, &r, &[]).expect("assign");
    assert_eq!(chunk(&a, 4), Some("textures"));
    assert_eq!(chunk(&a, 5), Some("arena"));
    assert_eq!(
        a.reason[&id(1)],
        Reason::Rule {
            chunk: "arena".into(),
            rule_index: 1
        },
        "the entry selector, after the one include glob"
    );
    assert_eq!(
        a.reason[&id(6)],
        Reason::Rule {
            chunk: BASE_CHUNK.into(),
            rule_index: 0
        }
    );
}

/// A misspelled entry would silently drop content in closure mode, so an entry that matches no
/// asset fails the cook (`chunk_entry_missing`), in either mode.
#[test]
fn an_entry_that_matches_no_asset_fails_the_cook() {
    let reg = AssetRegistry::from_entries(records());
    for text in [
        CLOSURE_RULES.replace("levels/arena/arena.level.ron", "levels/arena/arean.level.ron"),
        CLOSURE_RULES
            .replace("content = \"closure\"", "")
            .replace("ui/menu.ui", "ui/missing/*.ui"),
    ] {
        let err = assign(&reg, &rules(&text), &[]).expect_err("missing entry");
        assert_eq!(err.reason(), "chunk_entry_missing", "{err}");
        assert!(
            err.to_string().contains("arean") || err.to_string().contains("ui/missing"),
            "the message names the entry: {err}"
        );
    }
}

/// Closure mode with no entry point and no startup level would ship nothing: an error
/// (`chunk_no_entry`), not an empty game.
#[test]
fn closure_mode_needs_an_entry_point() {
    let reg = AssetRegistry::from_entries(records());
    let r = rules(
        r#"
[cook.chunk_options]
content = "closure"
[cook.chunks.arena]
include = ["levels/arena/**"]
"#,
    );
    let err = assign(&reg, &r, &[]).expect_err("no entry");
    assert_eq!(err.reason(), "chunk_no_entry", "{err}");
}

/// An unknown content mode is a rule error, not a warning that falls back to shipping everything.
#[test]
fn an_unknown_content_mode_is_a_rule_error() {
    let err = ChunkRules::from_manifest_toml("[cook.chunk_options]\ncontent = \"everything\"\n")
        .expect_err("bad mode");
    assert_eq!(err.reason(), "bad_params", "{err}");
    assert!(err.to_string().contains("content"), "{err}");
}

/// The startup level is an entry point: its closure ships (pinned in base) without an `entry`.
#[test]
fn the_startup_level_is_an_entry_point() {
    let reg = AssetRegistry::from_entries(records());
    let r = rules(
        r#"
[cook.chunk_options]
content = "closure"
[cook.chunks.arena]
include = ["levels/arena/**"]
"#,
    );
    let a = assign(&reg, &r, &[id(1)]).expect("assign");
    for n in [1, 2, 3] {
        assert_eq!(
            chunk(&a, n),
            Some(BASE_CHUNK),
            "{n}: startup closure is pinned in base"
        );
    }
    for n in [4, 5, 6, 7] {
        assert_eq!(chunk(&a, n), None, "{n} is reached from no entry point");
    }
}

/// `follow = false` changes placement only: a dependency of an entry still ships (in base, as an
/// unclaimed asset does), it is never dropped.
#[test]
fn a_dependency_of_a_no_follow_chunk_still_ships() {
    let reg = AssetRegistry::from_entries(records());
    let r = rules(
        r#"
[cook.chunk_options]
content = "closure"
[cook.chunks.arena]
entry = ["levels/arena/arena.level.ron"]
follow = false
"#,
    );
    let a = assign(&reg, &r, &[]).expect("assign");
    assert_eq!(chunk(&a, 1), Some("arena"));
    assert_eq!(chunk(&a, 2), Some(BASE_CHUNK));
    assert_eq!(chunk(&a, 3), Some(BASE_CHUNK));
    assert_eq!(a.reason[&id(2)], Reason::Default);
    for n in [4, 5, 6, 7] {
        assert_eq!(chunk(&a, n), None, "{n}");
    }
}

/// A dependency cycle and a dependency outside the registry neither loop nor ship anything extra;
/// the result does not depend on the registry's record order.
#[test]
fn cycles_and_dangling_dependencies_are_walked_once_in_any_order() {
    let mut recs = vec![
        rec(1, "levels/loop.level.ron", &[2, 99]),
        rec(2, "props/a.glb", &[3]),
        rec(3, "props/b.glb", &[2, 1]),
        rec(4, "props/orphan.glb", &[2]),
    ];
    let r = rules(
        r#"
[cook.chunk_options]
content = "closure"
[cook.chunks.level]
entry = ["levels/*.level.ron"]
"#,
    );
    let a = assign(&AssetRegistry::from_entries(recs.clone()), &r, &[]).expect("assign");
    recs.reverse();
    let b = assign(&AssetRegistry::from_entries(recs), &r, &[]).expect("assign reversed");
    assert_eq!(a, b);
    for n in [1, 2, 3] {
        assert_eq!(chunk(&a, n), Some("level"), "{n}");
    }
    assert_eq!(
        chunk(&a, 4),
        None,
        "an asset that references shipped ones is not reached by them"
    );
    assert_eq!(chunk(&a, 99), None);
}

/// With every shipped asset in `base` the cook still writes one pak, but its table of contents
/// lists the shipped assets only (the all-content single pak lists the whole cache).
#[test]
fn a_single_base_pak_lists_only_shipped_assets() {
    let root = regia_test_utils::unique_temp_dir("cook-closure-single");
    let recs = records();
    let cache = cache(&root, &recs);
    let reg = AssetRegistry::from_entries(recs);
    let r =
        rules("[cook.chunk_options]\ncontent = \"closure\"\n[cook.chunks.base]\nentry = [\"ui/menu.ui\"]\n");
    let a = assign(&reg, &r, &[]).expect("assign");
    let out = root.join("paks");
    let (manifest, _) = write_chunks(&cache, &out, &a, &r, &opts()).expect("write");
    assert_eq!(manifest.chunks.len(), 1);
    let pak = regia_asset::pak::PakReader::open(&out.join("base.fpak")).expect("open pak");
    let mut listed: Vec<AssetId> = pak.manifest().iter().map(|e| e.id).collect();
    listed.sort();
    assert_eq!(listed, [id(6), id(7)]);
    let _ = std::fs::remove_dir_all(&root);
}
