// SPDX-License-Identifier: MPL-2.0
//! Chunk rules, assignment, reference checks and chunk paks
//! (`docs/specs/40-content-chunks-dlc-asset-packs.md` § 8, tests 1-8).

use std::collections::BTreeMap;
use std::path::Path;

use regia_asset::content::{BASE_CHUNK, ChunkMap, ContentManifest, Delivery, hash_file};
use regia_asset::registry::{AssetRegistry, RegistryEntry};
use regia_asset::{AssetId, AssetManifest, ManifestEntry, cooked_file_name};
use regia_asset_pipeline::chunks::{
    Assignment, ChunkError, ChunkRule, ChunkRules, ChunkWriteOptions, Reason, SharedPolicy, assign,
    check_android_packs, check_references, why_table, write_chunks, write_dlc,
};
use regia_core::id::Guid;

fn id(n: u128) -> AssetId {
    AssetId(Guid::from_u128(n))
}

/// A registry record: `(n, path, deps, tags)`.
fn rec(n: u128, path: &str, deps: &[u128], tags: &[(&str, &str)]) -> RegistryEntry {
    RegistryEntry {
        id: id(n),
        path: format!("assets/{path}"),
        type_path: "regia_asset::CpuMesh".into(),
        dependencies: deps.iter().map(|d| id(*d)).collect(),
        tags: tags
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        size: 0,
    }
}

fn rules(toml: &str) -> ChunkRules {
    ChunkRules::from_manifest_toml(toml).expect("rules parse")
}

fn chunk(a: &Assignment, n: u128) -> &str {
    a.chunk_of.get(&id(n)).map_or("?", String::as_str)
}

/// Test 1: direct by glob and by tag; highest priority wins; equal priority conflicts; unselected
/// assets go to base.
#[test]
fn direct_selection_priority_conflict_and_default() {
    let reg = AssetRegistry::from_entries([
        rec(1, "levels/arena/arena.level.ron", &[], &[]),
        rec(2, "props/crate.glb", &[], &[("arena", "")]),
        rec(3, "levels/arena/boss.glb", &[], &[]),
        rec(4, "ui/menu.png", &[], &[]),
    ]);
    let r = rules(
        r#"
[cook.chunks.arena]
include = ["levels/arena/**"]
tags = ["arena"]
priority = 10
delivery = "on_demand"
[cook.chunks.boss]
include = ["levels/arena/boss.glb"]
priority = 20
"#,
    );
    let a = assign(&reg, &r, &[]).expect("assign");
    assert_eq!(chunk(&a, 1), "arena");
    assert_eq!(
        a.reason[&id(1)],
        Reason::Rule {
            chunk: "arena".into(),
            rule_index: 0
        }
    );
    assert_eq!(chunk(&a, 2), "arena");
    assert_eq!(
        a.reason[&id(2)],
        Reason::Rule {
            chunk: "arena".into(),
            rule_index: 1
        },
        "tag selector after globs"
    );
    assert_eq!(chunk(&a, 3), "boss", "higher priority wins");
    assert_eq!(chunk(&a, 4), BASE_CHUNK);
    assert_eq!(a.reason[&id(4)], Reason::Default);

    let tie = rules(
        r#"
[cook.chunks.arena]
include = ["levels/**"]
[cook.chunks.forest]
include = ["levels/arena/*"]
"#,
    );
    let err = assign(&reg, &tie, &[]).expect_err("conflict");
    assert_eq!(err.reason(), "chunk_rule_conflict");
    assert!(
        err.to_string().contains("arena") && err.to_string().contains("forest"),
        "{err}"
    );
}

/// Test 2: dependencies follow the highest-priority claim; equal claims are shared (base or
/// duplicated); `follow = false` leaves dependencies to base.
#[test]
fn dependencies_follow_claims_and_shared_policy() {
    // 1 (arena level) -> 10 (arena tex); 2 (forest level) -> 11 (shared tex); 1 -> 11;
    // 3 (cave level, priority 5) -> 11.
    let reg = AssetRegistry::from_entries([
        rec(1, "levels/arena.level.ron", &[10, 11], &[]),
        rec(2, "levels/forest.level.ron", &[11], &[]),
        rec(3, "levels/cave.level.ron", &[11, 12], &[]),
        rec(10, "tex/arena.png", &[], &[]),
        rec(11, "tex/shared.png", &[], &[]),
        rec(12, "tex/cave.png", &[], &[]),
    ]);
    let toml = |shared: &str, cave_follow: bool| {
        format!(
            r#"
[cook.chunks.arena]
include = ["levels/arena.level.ron"]
priority = 10
[cook.chunks.forest]
include = ["levels/forest.level.ron"]
priority = 10
[cook.chunks.cave]
include = ["levels/cave.level.ron"]
priority = 5
follow = {cave_follow}
[cook.chunk_options]
shared = "{shared}"
"#
        )
    };
    let a = assign(&reg, &rules(&toml("base", true)), &[]).expect("assign");
    assert_eq!(chunk(&a, 10), "arena");
    assert_eq!(a.reason[&id(10)], Reason::Follows { via: id(1) });
    assert_eq!(
        chunk(&a, 11),
        BASE_CHUNK,
        "arena and forest tie at 10, cave's 5 loses"
    );
    assert_eq!(a.reason[&id(11)], Reason::Shared);
    assert_eq!(chunk(&a, 12), "cave");

    let d = assign(&reg, &rules(&toml("duplicate", true)), &[]).expect("assign");
    assert_eq!(d.duplicates[&id(11)], ["arena", "forest"]);
    assert_eq!(d.members("forest"), [id(2), id(11)]);
    assert!(d.holds("arena", id(11)));

    let nf = assign(&reg, &rules(&toml("base", false)), &[]).expect("assign");
    assert_eq!(chunk(&nf, 12), BASE_CHUNK);
    assert_eq!(nf.reason[&id(12)], Reason::Default);
}

/// Test 3: `pin` and the startup closure stay in base even when a rule claims them.
#[test]
fn pin_and_startup_closure_stay_in_base() {
    let reg = AssetRegistry::from_entries([
        rec(1, "ui/menu.level.ron", &[5], &[]),
        rec(2, "levels/start.level.ron", &[6], &[]),
        rec(5, "levels/arena/banner.png", &[], &[]),
        rec(6, "levels/arena/floor.png", &[], &[]),
        rec(7, "levels/arena/arena.level.ron", &[5, 6], &[]),
    ]);
    let r = rules(
        r#"
[cook.chunks.base]
include = ["ui/**"]
pin = true
[cook.chunks.arena]
include = ["levels/arena/**"]
priority = 10
"#,
    );
    let a = assign(&reg, &r, &[id(2)]).expect("assign");
    assert_eq!(chunk(&a, 1), BASE_CHUNK);
    assert!(
        matches!(a.reason[&id(1)], Reason::Rule { .. }),
        "the base rule keeps its reason"
    );
    assert_eq!(
        (chunk(&a, 5), &a.reason[&id(5)]),
        (BASE_CHUNK, &Reason::Pinned),
        "pinned beats a rule"
    );
    assert_eq!(
        (chunk(&a, 6), &a.reason[&id(6)]),
        (BASE_CHUNK, &Reason::Pinned),
        "startup closure"
    );
    assert_eq!(chunk(&a, 7), "arena");
    assert!(
        check_references(&reg, &a, &r).is_empty(),
        "arena may reference base"
    );
}

/// Test 4: for a depth-3 chain the `via` list is the shortest path, ties by smaller id.
#[test]
fn reasons_follow_the_shortest_path_ties_by_smaller_id() {
    // Direct 1 -> {3, 2}; 2 -> 4; 3 -> 4; 4 -> 5; also 1 -> 9 -> 8 -> 7 -> 5 (longer).
    let reg = AssetRegistry::from_entries([
        rec(1, "levels/arena/a.level.ron", &[3, 2, 9], &[]),
        rec(2, "m/b.glb", &[4], &[]),
        rec(3, "m/c.glb", &[4], &[]),
        rec(4, "m/d.mat", &[5], &[]),
        rec(5, "t/e.png", &[], &[]),
        rec(9, "m/x.glb", &[8], &[]),
        rec(8, "m/y.glb", &[7], &[]),
        rec(7, "m/z.glb", &[5], &[]),
    ]);
    let r = rules("[cook.chunks.arena]\ninclude = [\"levels/arena/**\"]\n");
    let a = assign(&reg, &r, &[]).expect("assign");
    assert_eq!(a.via_chain(id(5)), [id(5), id(4), id(2), id(1)]);
    assert_eq!(chunk(&a, 5), "arena");
}

/// ADR-1876: the runtime reason table carries the cook's chain, rule selectors and duplicates,
/// so `content.why` in a running game answers what `regia assets why` prints.
#[test]
fn why_table_matches_the_assignment() {
    let reg = AssetRegistry::from_entries([
        rec(1, "levels/arena/a.level.ron", &[2], &[]),
        rec(2, "m/b.glb", &[4], &[]),
        rec(3, "props/c.glb", &[4], &[("arena", "")]),
        rec(4, "t/shared.png", &[], &[]),
        rec(5, "ui/hud.png", &[], &[]),
        rec(6, "levels/cave/a.level.ron", &[4], &[]),
    ]);
    let r = rules(
        "[cook.chunks.arena]\ninclude = [\"levels/arena/**\"]\ntags = [\"arena\"]\n\
         [cook.chunks.cave]\ninclude = [\"levels/cave/**\"]\n\
         [cook.chunk_options]\nshared = \"duplicate\"\n",
    );
    let a = assign(&reg, &r, &[]).expect("assign");
    let table =
        regia_asset::content::ChunkWhy::from_bytes(&why_table(&reg, &a, &r).to_bytes().expect("encode"))
            .expect("decode");
    assert_eq!(table.records.len(), a.chunk_of.len());
    // Follows chain back to the glob rule.
    let b = table.explain(id(2)).expect("b");
    assert_eq!((b.chunk.as_str(), b.reason), ("arena", "follows"));
    assert_eq!(b.via, ["assets/m/b.glb", "assets/levels/arena/a.level.ron"]);
    assert_eq!(b.rule, Some(("arena".into(), 0, "levels/arena/**".into())));
    // Direct by tag: the selector index counts the include globs first.
    let c = table.explain(id(3)).expect("c");
    assert_eq!(c.rule, Some(("arena".into(), 1, "tag:arena".into())));
    // Shared with `duplicate`: every holder listed; the reason matches the cook's.
    let shared = table.explain(id(4)).expect("shared");
    assert_eq!(shared.reason, a.reason[&id(4)].kind());
    assert_eq!(shared.duplicated_in, a.duplicates[&id(4)]);
    let hud = table
        .explain(table.resolve("ui/hud.png").expect("hud"))
        .expect("hud");
    assert_eq!((hud.chunk.as_str(), hud.reason), (BASE_CHUNK, "default"));
    assert_eq!(
        table.direct_assets("arena"),
        [
            (
                "assets/levels/arena/a.level.ron".to_owned(),
                "levels/arena/**".to_owned()
            ),
            ("assets/props/c.glb".to_owned(), "tag:arena".to_owned()),
        ]
    );
    // Deterministic bytes.
    assert_eq!(
        why_table(&reg, &a, &r).to_bytes().expect("a"),
        why_table(&reg, &a, &r).to_bytes().expect("b")
    );
}

/// Test 5: base → chunk is `chunk_base_reference`; chunk → unlisted chunk errors; a cycle is
/// `chunk_cycle`.
#[test]
fn reference_checks() {
    let reg = AssetRegistry::from_entries([
        rec(1, "ui/menu.level.ron", &[3], &[]),
        rec(2, "levels/forest/f.level.ron", &[3, 4], &[]),
        rec(3, "levels/arena/a.glb", &[], &[]),
        rec(4, "levels/cave/c.glb", &[], &[]),
    ]);
    let r = rules(
        r#"
[cook.chunks.arena]
include = ["levels/arena/**"]
[cook.chunks.forest]
include = ["levels/forest/**"]
requires = ["cave"]
[cook.chunks.cave]
include = ["levels/cave/**"]
"#,
    );
    let a = assign(&reg, &r, &[]).expect("assign");
    let problems = check_references(&reg, &a, &r);
    let codes: Vec<(&str, &str)> = problems.iter().map(|p| (p.chunk.as_str(), p.reason)).collect();
    assert_eq!(
        codes,
        [
            ("base", "chunk_base_reference"),
            ("forest", "chunk_unlisted_reference")
        ]
    );
    assert_eq!(
        problems[1].target,
        id(3),
        "forest may use cave (requires) but not arena"
    );

    let cyc = ChunkRules::from_manifest_toml(
        r#"
[cook.chunks.a]
requires = ["b"]
[cook.chunks.b]
requires = ["a"]
"#,
    )
    .expect_err("cycle");
    assert_eq!(cyc.reason(), "chunk_cycle");
    let unknown =
        ChunkRules::from_manifest_toml("[cook.chunks.a]\nrequires = [\"zz\"]\n").expect_err("unknown");
    assert_eq!(unknown.reason(), "chunk_not_found");
    let mut manual = ChunkRules::default();
    let mut ra = ChunkRule::new("a");
    ra.requires = vec!["b".into()];
    let mut rb = ChunkRule::new("b");
    rb.requires = vec!["a".into()];
    manual.rules.insert("a".into(), ra);
    manual.rules.insert("b".into(), rb);
    assert_eq!(
        check_references(&reg, &Assignment::default(), &manual)[0].reason,
        "chunk_cycle"
    );
}

/// Test 7: the old shorthand parses with defaults; rule keys are validated; unknown keys warn.
#[test]
fn shorthand_and_rule_validation() {
    let r = rules("[cook]\nchunks = { base = [\"**\"] }\ntexture_quality = \"final\"\n");
    let base = &r.rules["base"];
    assert_eq!(base.include, ["**"]);
    assert_eq!(
        (base.priority, base.delivery, base.follow, base.pin),
        (1, Delivery::Install, true, false)
    );
    assert_eq!(r.shared, SharedPolicy::Base);
    assert_eq!(r.android_pack_limit, 50);
    assert!(rules("[project]\nname = \"x\"\n").is_empty());
    let warn = rules("[cook.chunks.arena]\ninclude = []\ncolour = 3\n");
    assert_eq!(warn.warnings.len(), 1, "{:?}", warn.warnings);
    for bad in [
        "[cook.chunks.arena]\npriority = 0\n",
        "[cook.chunks.arena]\npriority = 1001\n",
        "[cook.chunks.arena]\ndelivery = \"later\"\n",
        "[cook.chunks.Arena]\ninclude = []\n",
        "[cook.chunk_options]\nshared = \"split\"\n",
    ] {
        let err = ChunkRules::from_manifest_toml(bad).expect_err(bad);
        assert_eq!(err.reason(), "bad_params", "{bad}");
    }
}

/// A cooked cache: `asset_manifest.bin` plus one cooked file per asset with `payload` bytes.
fn cache(root: &Path, assets: &[(u128, &str, &str)]) -> std::path::PathBuf {
    let cache = root.join(".regia/cooked/linux");
    std::fs::create_dir_all(&cache).expect("mkdir");
    let mut manifest = AssetManifest::default();
    for (n, path, payload) in assets {
        std::fs::write(cache.join(cooked_file_name(id(*n))), payload.repeat(64)).expect("cooked");
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

fn three_chunk_rules() -> ChunkRules {
    rules(
        r#"
[cook.chunks.arena]
include = ["levels/arena/**"]
delivery = "on_demand"
[cook.chunks.forest]
include = ["levels/forest/**"]
delivery = "external"
"#,
    )
}

/// Test 6: N chunks give N paks and a manifest whose sizes and hashes match; two cooks are equal;
/// an unchanged chunk keeps its pak when another chunk changes.
#[test]
fn writing_chunks_is_deterministic_and_incremental() {
    let root = regia_test_utils::unique_temp_dir("chunks-write");
    let assets = [
        (1, "ui/menu.png", "menu"),
        (2, "levels/arena/a.glb", "arena"),
        (3, "levels/forest/f.glb", "forest"),
    ];
    let cache = cache(&root, &assets);
    let reg = AssetRegistry::from_entries(assets.iter().map(|(n, p, _)| rec(*n, p, &[], &[])));
    let r = three_chunk_rules();
    let a = assign(&reg, &r, &[]).expect("assign");
    let out = root.join("paks");
    let (manifest, report) = write_chunks(&cache, &out, &a, &r, &opts()).expect("write");
    let names: Vec<&str> = manifest.chunks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["base", "arena", "forest"]);
    assert_eq!(report.written, ["base", "arena", "forest"]);
    for c in &manifest.chunks {
        let pak = out.join(&c.file);
        assert_eq!(std::fs::metadata(&pak).expect("pak").len(), c.size);
        assert_eq!(hash_file(&pak).expect("hash"), c.hash);
        assert_eq!(c.assets, 1);
    }
    assert_eq!(
        manifest.chunk("arena").expect("arena").delivery,
        Delivery::OnDemand
    );
    assert_eq!(
        ContentManifest::read_from_dir(&out)
            .expect("read")
            .expect("present"),
        manifest
    );
    let map = ChunkMap::read_from_dir(&out).expect("map").expect("present");
    assert_eq!(map.chunk_of(id(3)), Some("forest"));

    // A second cook of the same inputs into another directory: equal hashes.
    let (again, _) = write_chunks(&cache, &root.join("paks2"), &a, &r, &opts()).expect("write 2");
    assert_eq!(again, manifest);

    // Change forest's cooked bytes: forest is rewritten, base and arena are kept.
    std::fs::write(cache.join(cooked_file_name(id(3))), b"changed").expect("recook");
    let (third, report) = write_chunks(&cache, &out, &a, &r, &opts()).expect("write 3");
    assert_eq!(report.kept, ["base", "arena"]);
    assert_eq!(report.written, ["forest"]);
    assert_eq!(third.chunks[0], manifest.chunks[0]);
    assert_ne!(
        third.chunk("forest").expect("f").hash,
        manifest.chunk("forest").expect("f").hash
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// audit F6: a chunk dropped from the rules leaves no pak behind (the runtime mounts every pak in
/// the folder); a chunk still in the rules keeps its files.
#[test]
fn a_chunk_removed_from_the_rules_loses_its_files() {
    let root = regia_test_utils::unique_temp_dir("chunks-removed");
    let assets = [
        (1, "ui/menu.png", "menu"),
        (2, "levels/arena/a.glb", "arena"),
        (3, "levels/forest/f.glb", "forest"),
    ];
    let cache = cache(&root, &assets);
    let reg = AssetRegistry::from_entries(assets.iter().map(|(n, p, _)| rec(*n, p, &[], &[])));
    let r = three_chunk_rules();
    let a = assign(&reg, &r, &[]).expect("assign");
    let out = root.join("paks");
    write_chunks(&cache, &out, &a, &r, &opts()).expect("write");
    assert!(out.join("arena.fpak").is_file());
    let fewer = rules(
        r#"
[cook.chunks.forest]
include = ["levels/forest/**"]
delivery = "external"
"#,
    );
    let a2 = assign(&reg, &fewer, &[]).expect("assign");
    let (manifest, report) = write_chunks(&cache, &out, &a2, &fewer, &opts()).expect("write 2");
    assert_eq!(report.removed, ["arena"]);
    assert!(!out.join("arena.fpak").exists());
    assert!(out.join("forest.fpak").is_file());
    assert!(manifest.chunk("arena").is_none());
    let _ = std::fs::remove_dir_all(&root);
}

/// Test 6 (last clause): no rules → `base.fpak` byte-identical to today's single-pak output and a
/// one-chunk manifest.
#[test]
fn no_rules_writes_todays_base_pak() {
    let root = regia_test_utils::unique_temp_dir("chunks-base-identical");
    let assets = [(1, "a.png", "a"), (2, "b/c.glb", "c"), (3, "d.ogg", "d")];
    let cache = cache(&root, &assets);
    let today = root.join("today");
    regia_asset_pipeline::pack_cooked_directory_with(&cache, &today, &BTreeMap::new(), true, false)
        .expect("today");
    let reg = AssetRegistry::from_entries(assets.iter().map(|(n, p, _)| rec(*n, p, &[], &[])));
    let r = ChunkRules::default();
    let a = assign(&reg, &r, &[]).expect("assign");
    let out = root.join("chunked");
    let (manifest, _) = write_chunks(&cache, &out, &a, &r, &opts()).expect("write");
    assert_eq!(manifest.chunks.len(), 1);
    assert_eq!(manifest.chunks[0].name, "base");
    assert_eq!(
        std::fs::read(out.join("base.fpak")).expect("new"),
        std::fs::read(today.join("base.fpak")).expect("old"),
        "byte-identical base pak"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Test 8: DLC against a frozen base: collisions refused unless `overrides`; dependencies already
/// in base are not copied; `requires` records the base build id.
#[test]
fn dlc_against_a_frozen_base() {
    let root = regia_test_utils::unique_temp_dir("chunks-dlc");
    let base_assets = [(1, "ui/menu.png", "menu"), (2, "tex/rock.png", "rock")];
    let base_cache = cache(&root.join("v1"), &base_assets);
    let base_reg =
        AssetRegistry::from_entries([rec(1, "ui/menu.png", &[], &[]), rec(2, "tex/rock.png", &[], &[])]);
    let (base_manifest, _) = write_chunks(
        &base_cache,
        &root.join("v1/paks"),
        &assign(&base_reg, &ChunkRules::default(), &[]).expect("a"),
        &ChunkRules::default(),
        &opts(),
    )
    .expect("base");

    // The DLC project: a new level using the base rock texture and a new texture.
    let assets = [
        (1, "ui/menu.png", "menu"),
        (2, "tex/rock.png", "rock"),
        (20, "dlc/island.level.ron", "island"),
        (21, "dlc/sand.png", "sand"),
    ];
    let dlc_cache = cache(&root.join("v2"), &assets);
    let reg = AssetRegistry::from_entries([
        rec(1, "ui/menu.png", &[], &[]),
        rec(2, "tex/rock.png", &[], &[]),
        rec(20, "dlc/island.level.ron", &[2, 21], &[]),
        rec(21, "dlc/sand.png", &[], &[]),
    ]);
    let r = rules("[cook.chunks.island]\ninclude = [\"dlc/**\"]\ndelivery = \"external\"\n");
    let out = root.join("dlc");
    let entry = write_dlc(
        &dlc_cache,
        &out,
        &reg,
        &r,
        "island",
        (&base_manifest, &base_reg),
        &opts(),
    )
    .expect("dlc");
    assert_eq!(entry.requires, ["base@1.0-1-test"]);
    assert_eq!(entry.assets, 2, "the base rock texture is not copied");
    assert!(out.join("island.fpak").is_file());
    assert!(out.join("island.chunk.ron").is_file());

    // A DLC rule that selects a base asset collides unless it overrides (a patch).
    let patch = rules("[cook.chunks.patch]\ninclude = [\"tex/rock.png\"]\n");
    let err = write_dlc(
        &dlc_cache,
        &out,
        &reg,
        &patch,
        "patch",
        (&base_manifest, &base_reg),
        &opts(),
    )
    .expect_err("collision");
    assert_eq!(err.reason(), "dlc_id_collision");
    let patch = rules("[cook.chunks.patch]\ninclude = [\"tex/rock.png\"]\noverrides = true\n");
    let entry = write_dlc(
        &dlc_cache,
        &out,
        &reg,
        &patch,
        "patch",
        (&base_manifest, &base_reg),
        &opts(),
    )
    .expect("override patch");
    assert_eq!(entry.assets, 1);
    let _ = std::fs::remove_dir_all(&root);
}

/// Audit v0.2.12 (content M3): an asset the registry assigns but the cook never wrote was still
/// listed in `chunk-map.bin`, so with its chunk mounted the runtime fell through to a missing cache
/// file instead of reporting it.
#[test]
fn the_chunk_map_lists_only_assets_a_pak_holds() {
    let root = regia_test_utils::unique_temp_dir("chunks-map-unwritten");
    let c = cache(&root, &[(1, "ui/menu.png", "menu"), (2, "tex/rock.png", "rock")]);
    // Asset 3 is in the registry (a failed cook) but has no cooked file or cache manifest entry;
    // asset 2 is in the cache manifest but its cooked file is gone.
    std::fs::remove_file(c.join(cooked_file_name(id(2)))).expect("remove");
    let reg = AssetRegistry::from_entries([
        rec(1, "ui/menu.png", &[], &[]),
        rec(2, "tex/rock.png", &[], &[]),
        rec(3, "dlc/broken.glb", &[], &[]),
    ]);
    let r = rules("[cook.chunks.arena]\ninclude = [\"dlc/**\", \"tex/**\"]\n");
    let a = assign(&reg, &r, &[]).expect("assign");
    assert_eq!(chunk(&a, 3), "arena");
    let out = root.join("paks");
    let (_, report) = write_chunks(&c, &out, &a, &r, &opts()).expect("write");
    assert_eq!(report.unwritten, [id(2), id(3)]);
    let map = ChunkMap::read_from_dir(&out).expect("read").expect("map");
    assert_eq!(map.chunk_of(id(1)), Some("base"));
    assert_eq!(map.chunk_of(id(2)), None, "no cooked file: in no chunk");
    assert_eq!(map.chunk_of(id(3)), None, "never cooked: in no chunk");
    let _ = std::fs::remove_dir_all(&root);
}

/// Audit v0.2.12 (content H1): `regia content dlc` wrote `<chunk>.chunk.ron` but the runtime only
/// read `content-manifest.ron`, so a DLC dropped next to the game was `chunk_not_found` and its pak
/// mounted as an unlisted one with no base check. The entry and the DLC's own chunk map now join
/// the manifest; a DLC built against another base is listed but not mounted.
#[test]
fn a_dlc_dropped_next_to_the_game_joins_the_content_manifest() {
    use regia_asset::content::{ChunkState, ContentError, ContentManager, ContentSettings};
    use regia_asset::{AssetServer, AssetServerSettings};
    let root = regia_test_utils::unique_temp_dir("chunks-dlc-runtime");
    let base_assets = [(1, "ui/menu.png", "menu"), (2, "tex/rock.png", "rock")];
    let base_cache = cache(&root.join("v1"), &base_assets);
    let base_reg =
        AssetRegistry::from_entries([rec(1, "ui/menu.png", &[], &[]), rec(2, "tex/rock.png", &[], &[])]);
    let content = root.join("content");
    let (base_manifest, _) = write_chunks(
        &base_cache,
        &content,
        &assign(&base_reg, &ChunkRules::default(), &[]).expect("a"),
        &ChunkRules::default(),
        &opts(),
    )
    .expect("base");
    let assets = [
        (1, "ui/menu.png", "menu"),
        (2, "tex/rock.png", "rock"),
        (20, "dlc/island.level.ron", "island"),
        (21, "dlc/sand.png", "sand"),
    ];
    let dlc_cache = cache(&root.join("v2"), &assets);
    let reg = AssetRegistry::from_entries([
        rec(1, "ui/menu.png", &[], &[]),
        rec(2, "tex/rock.png", &[], &[]),
        rec(20, "dlc/island.level.ron", &[2, 21], &[]),
        rec(21, "dlc/sand.png", &[], &[]),
    ]);
    let r = rules("[cook.chunks.island]\ninclude = [\"dlc/**\"]\ndelivery = \"external\"\n");
    // The DLC lands straight in the game's content directory.
    write_dlc(
        &dlc_cache,
        &content,
        &reg,
        &r,
        "island",
        (&base_manifest, &base_reg),
        &opts(),
    )
    .expect("dlc");
    assert!(
        content.join("island.chunk-map.bin").is_file(),
        "the DLC's own map"
    );

    let settings = |content: &Path| {
        let mut s = ContentSettings::for_content_dir(content);
        s.install_dir = root.join("install");
        s.base_url = None;
        s
    };
    let manager = ContentManager::open(&settings(&content)).expect("open");
    assert!(
        manager.manifest().chunk("island").is_some(),
        "the entry joined the manifest"
    );
    assert_eq!(manager.state("island"), Some(ChunkState::Installed));
    let server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: "linux".into(),
        content_dir: Some(content.clone()),
    })
    .expect("server");
    assert_eq!(
        server.mounted_pak_names(),
        ["base", "island"],
        "listed, in mount order"
    );
    let map = ChunkMap::read_from_dir_with_dlc(&content, manager.manifest()).expect("map");
    assert_eq!(
        map.chunk_of(id(21)),
        Some("island"),
        "a DLC-only id has its chunk"
    );
    assert_eq!(map.chunk_of(id(20)), Some("island"));
    assert_eq!(map.chunk_of(id(2)), Some("base"), "the base keeps its ids");

    // Built against another base: listed (so the game can say why), never mounted.
    let entry_file = content.join("island.chunk.ron");
    let text = std::fs::read_to_string(&entry_file).expect("entry");
    std::fs::write(&entry_file, text.replace("base@1.0-1-test", "base@0.9-1-other")).expect("rewrite");
    let mut manager = ContentManager::open(&settings(&content)).expect("open");
    let mut server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: "linux".into(),
        content_dir: Some(content.clone()),
    })
    .expect("server");
    assert_eq!(
        server.mounted_pak_names(),
        ["base"],
        "a mismatched DLC is not mounted"
    );
    let err = manager.mount(&mut server, "island").expect_err("mismatch");
    assert!(matches!(err, ContentError::ChunkBaseMismatch { .. }), "{err:?}");

    // A bad entry (name not its file's, or shadowing a listed chunk) is skipped, not fatal.
    std::fs::write(&entry_file, text.replace("name: \"island\"", "name: \"base\"")).expect("rewrite");
    let manager = ContentManager::open(&settings(&content)).expect("open");
    assert!(manager.manifest().chunk("island").is_none());
    assert_eq!(manager.manifest().chunks.len(), 1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn android_pack_checks() {
    let mut m = ContentManifest::new("b", "android");
    let mk = |name: &str, delivery| regia_asset::content::ChunkEntry {
        name: name.into(),
        file: format!("{name}.fpak"),
        size: 1,
        hash: String::new(),
        delivery,
        requires: Vec::new(),
        overrides: false,
        mount_order: 0,
        assets: 0,
        blocks: Vec::new(),
    };
    m.chunks.push(mk("base", Delivery::Install));
    m.chunks.push(mk("dlc-a", Delivery::OnDemand));
    assert!(check_android_packs(&m, 50).is_ok());
    m.chunks.push(mk("dlc_a", Delivery::OnDemand));
    assert_eq!(
        check_android_packs(&m, 50).expect_err("collision").reason(),
        "pack_name_collision"
    );
    m.chunks.pop();
    m.chunks.push(mk("b", Delivery::OnDemand));
    assert_eq!(
        check_android_packs(&m, 1).expect_err("limit").reason(),
        "pack_limit"
    );
    m.chunks.push(mk("ff1", Delivery::FastFollow));
    m.chunks.push(mk("ff2", Delivery::FastFollow));
    assert!(matches!(
        check_android_packs(&m, 50),
        Err(ChunkError::PackLimit { limit: 1, .. })
    ));
}

/// Spec 40 § 10 budget: assignment over 100,000 assets and 500,000 dependency edges (hand-run:
/// `cargo test -p regia_asset_pipeline --release --test content_chunks -- --ignored --nocapture`).
/// Prints the time; asserts only the result, never the clock.
#[test]
#[ignore = "hand-run perf measurement"]
fn assignment_perf_100k_assets() {
    let n: u128 = 100_000;
    let entries = (0..n).map(|i| {
        // Up to ten dependencies each, pointing at higher ids (a DAG), spread over the range.
        let deps: Vec<u128> = (1..=10).map(|k| (i + k * 7919) % n).filter(|d| *d > i).collect();
        let folder = if i % 50 == 0 {
            format!("levels/l{}", i % 20)
        } else {
            "props".to_owned()
        };
        rec(
            i + 1,
            &format!("{folder}/a{i}.glb"),
            &deps.iter().map(|d| d + 1).collect::<Vec<_>>(),
            &[],
        )
    });
    let reg = AssetRegistry::from_entries(entries);
    let mut toml = String::new();
    for l in 0..20 {
        toml.push_str(&format!(
            "[cook.chunks.l{l}]\ninclude = [\"levels/l{l}/**\"]\npriority = {}\n",
            1 + l % 3
        ));
    }
    let r = rules(&toml);
    let start = std::time::Instant::now();
    let a = assign(&reg, &r, &[]).expect("assign");
    let elapsed = start.elapsed();
    let edges: usize = reg.entries().map(|e| e.dependencies.len()).sum();
    println!(
        "assign: {} assets, {edges} edges, {} chunks in {:.1} ms",
        reg.len(),
        a.chunk_names().len(),
        elapsed.as_secs_f64() * 1e3
    );
    assert_eq!(a.chunk_of.len(), reg.len());
}

/// `[content] base_url` is parsed and checked when the rules load, so a bad URL stops the cook.
#[test]
fn base_url_is_read_and_checked() {
    let ok = ChunkRules::from_manifest_toml("[content]\nbase_url = \"https://cdn.example.com/game\"\n")
        .expect("rules");
    assert_eq!(ok.base_url.as_deref(), Some("https://cdn.example.com/game"));
    assert_eq!(ChunkRules::from_manifest_toml("").expect("empty").base_url, None);
    for bad in ["base_url = 3", "base_url = \"ftp://x/y\""] {
        let error = ChunkRules::from_manifest_toml(&format!("[content]\n{bad}\n")).expect_err(bad);
        assert_eq!(error.reason(), "bad_params", "{bad}");
    }
}

/// Chunks that transfer carry 1 MiB block hashes; install chunks and single-block paks do not.
#[test]
fn transferred_chunks_get_block_hashes() {
    use regia_asset::content::{RESUME_BLOCK, block_hashes};
    let dir = regia_test_utils::unique_temp_dir("chunks-blocks");
    let path = dir.join("big.bin");
    let bytes = vec![3u8; RESUME_BLOCK as usize * 2 + 10];
    std::fs::write(&path, &bytes).expect("write");
    let hashes = block_hashes(&mut std::fs::File::open(&path).expect("open")).expect("hashes");
    assert_eq!(hashes.len(), 3);
    assert_eq!(hashes[0], hashes[1]);
    assert_eq!(
        hashes[2],
        blake3::hash(&bytes[..10]).to_hex().to_string(),
        "the last block is short"
    );
    assert!(
        block_hashes(&mut std::io::empty()).expect("empty").is_empty(),
        "no bytes, no blocks"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spec 40 § 2.6 (ADR-1585 b): above 40 store packs the cook warns; within the limit it still
/// passes; one more than the limit is `pack_limit`. Every pass carries the "verify the store's
/// current limits" note (lane spec-17-39, ADR-5636).
#[test]
fn android_pack_count_warns_above_forty() {
    let mk = |name: String| regia_asset::content::ChunkEntry {
        file: format!("{name}.fpak"),
        name,
        size: 1,
        hash: String::new(),
        delivery: Delivery::OnDemand,
        requires: Vec::new(),
        overrides: false,
        mount_order: 0,
        assets: 0,
        blocks: Vec::new(),
    };
    let manifest_of = |packs: usize| {
        let mut m = ContentManifest::new("b", "android");
        m.chunks.push(regia_asset::content::ChunkEntry {
            delivery: Delivery::Install,
            ..mk("base".into())
        });
        for i in 0..packs {
            m.chunks.push(mk(format!("p{i}")));
        }
        m
    };
    let forty = check_android_packs(&manifest_of(40), 50).expect("40 packs pass");
    assert_eq!(forty.len(), 1, "only the verify-limits note at 40: {forty:?}");
    assert!(forty[0].contains("verify"), "{forty:?}");
    let forty_one = check_android_packs(&manifest_of(41), 50).expect("41 packs pass");
    assert_eq!(forty_one.len(), 2, "{forty_one:?}");
    assert!(forty_one[1].contains("41 asset packs"), "{forty_one:?}");
    assert!(check_android_packs(&manifest_of(50), 50).is_ok());
    assert_eq!(
        check_android_packs(&manifest_of(51), 50)
            .expect_err("limit")
            .reason(),
        "pack_limit"
    );
}

/// Bytes lz4 cannot shrink, except that every `repeat_every`-th 256-byte segment repeats the one
/// before it: lz4 saves about `1 / repeat_every` of them.
fn payload_saving_about(len: usize, repeat_every: usize) -> Vec<u8> {
    const SEGMENT: usize = 256;
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut out = Vec::with_capacity(len + SEGMENT);
    let mut segment = 0usize;
    while out.len() < len {
        if segment.is_multiple_of(repeat_every) && out.len() >= SEGMENT {
            let start = out.len() - SEGMENT;
            out.extend_from_within(start..start + SEGMENT);
        } else {
            for _ in 0..SEGMENT {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                out.push((state >> 24) as u8);
            }
        }
        segment += 1;
    }
    out.truncate(len);
    out
}

/// Whether every entry of the pak at `path` is stored raw (no block table).
fn all_raw(path: &Path) -> bool {
    let pak = regia_asset::PakReader::open(path).expect("open pak");
    pak.toc
        .entries
        .iter()
        .all(|e| e.block_table_offset == 0 && e.compressed_size == e.size)
}

/// Whether every entry of the pak at `path` is stored lz4 (a block table).
fn all_lz4(path: &Path) -> bool {
    let pak = regia_asset::PakReader::open(path).expect("open pak");
    !pak.toc.entries.is_empty() && pak.toc.entries.iter().all(|e| e.block_table_offset != 0)
}

const TEXTURES_AND_UNITS: &str = r#"
[cook.chunks.textures]
include = ["textures/**"]
compress = false
[cook.chunks.units]
include = ["units/**"]
"#;

/// DO27 CP_COOK per-chunk-compress (ADR-7971): `compress = false` on `[cook.chunks.textures]`
/// parses without a warning and writes that chunk raw while `units` and `base` stay lz4; turning
/// the key back on rewrites only that chunk (the choice is in its input hash); a non-bool value is
/// a rule error, and an unknown key still warns.
#[test]
fn a_chunk_with_compress_false_is_written_raw() {
    let root = regia_test_utils::unique_temp_dir("chunks-compress-key");
    let assets = [
        (1, "ui/menu.png", "menu"),
        (2, "textures/rock.ktx2", "texture"),
        (3, "units/tank.glb", "unit"),
    ];
    let cache = cache(&root, &assets);
    let reg = AssetRegistry::from_entries(assets.iter().map(|(n, p, _)| rec(*n, p, &[], &[])));
    let r = rules(TEXTURES_AND_UNITS);
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    assert!(!r.rules["textures"].compress);
    assert!(r.rules["units"].compress, "the key defaults to true");
    let a = assign(&reg, &r, &[]).expect("assign");
    let out = root.join("paks");
    let (manifest, _) = write_chunks(&cache, &out, &a, &r, &opts()).expect("write");
    assert_eq!(manifest.chunk("textures").expect("textures").assets, 1);
    assert!(all_raw(&out.join("textures.fpak")), "textures.fpak is raw");
    let textures = regia_asset::PakReader::open(&out.join("textures.fpak")).expect("open");
    assert!(!textures.header.is_compressed());
    assert!(all_lz4(&out.join("units.fpak")), "units.fpak stays lz4");
    assert!(all_lz4(&out.join("base.fpak")), "base.fpak stays lz4");

    // Omitting the key is today's behavior: only the textures pak changes.
    let on = rules(&TEXTURES_AND_UNITS.replace("compress = false\n", ""));
    let a_on = assign(&reg, &on, &[]).expect("assign");
    let (_, report) = write_chunks(&cache, &out, &a_on, &on, &opts()).expect("write 2");
    assert_eq!(report.written, ["textures"]);
    assert_eq!(report.kept, ["base", "units"]);
    assert!(all_lz4(&out.join("textures.fpak")));

    let bad = ChunkRules::from_manifest_toml("[cook.chunks.textures]\ncompress = \"no\"\n")
        .expect_err("compress must be a bool");
    assert_eq!(bad.reason(), "bad_params");
    let warn = rules("[cook.chunks.textures]\ncompresss = false\n");
    assert_eq!(warn.warnings.len(), 1, "{:?}", warn.warnings);
    let _ = std::fs::remove_dir_all(&root);
}

/// DO27 COOKED_PACKS_DESIGN pak-min-ratio (ADR-7971): `ChunkWriteOptions::min_ratio` reaches the
/// chunk paks: at 0.15 a blob lz4 saves about 8 % of is stored raw and one it saves about 33 % of
/// stays lz4; changing the ratio rewrites the compressed paks.
#[test]
fn min_ratio_reaches_the_chunk_paks() {
    let root = regia_test_utils::unique_temp_dir("chunks-min-ratio");
    let assets = [
        (1, "ui/menu.png", "menu"),
        (2, "textures/rock.ktx2", "texture"),
        (3, "units/tank.glb", "unit"),
    ];
    let cache = cache(&root, &assets);
    std::fs::write(
        cache.join(cooked_file_name(id(2))),
        payload_saving_about(600_000, 12),
    )
    .expect("bc7");
    std::fs::write(
        cache.join(cooked_file_name(id(3))),
        payload_saving_about(600_000, 3),
    )
    .expect("mesh");
    let reg = AssetRegistry::from_entries(assets.iter().map(|(n, p, _)| rec(*n, p, &[], &[])));
    let r = rules(&TEXTURES_AND_UNITS.replace("compress = false\n", ""));
    let a = assign(&reg, &r, &[]).expect("assign");
    let out = root.join("paks");
    write_chunks(&cache, &out, &a, &r, &opts()).expect("write");
    assert!(
        all_lz4(&out.join("textures.fpak")),
        "ratio 0: an 8 % saving stays lz4"
    );

    let ship = ChunkWriteOptions {
        min_ratio: 0.15,
        ..opts()
    };
    let (_, report) = write_chunks(&cache, &out, &a, &r, &ship).expect("write ship");
    assert_eq!(
        report.written,
        ["base", "textures", "units"],
        "the ratio is in the input hash"
    );
    assert!(
        all_raw(&out.join("textures.fpak")),
        "ratio 0.15: an 8 % saving is raw"
    );
    assert!(
        all_lz4(&out.join("units.fpak")),
        "ratio 0.15: a 33 % saving stays lz4"
    );
    let pak = std::sync::Arc::new(regia_asset::PakReader::open(&out.join("textures.fpak")).expect("open"));
    let shared = pak.read_shared(id(2)).expect("read");
    assert!(shared.is_mapped(), "the raw blob is a view of the mapping");
    let _ = std::fs::remove_dir_all(&root);
}
