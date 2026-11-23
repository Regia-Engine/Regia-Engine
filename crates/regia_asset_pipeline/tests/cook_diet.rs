// SPDX-License-Identifier: MPL-2.0
//! Cook diet rules (DO27-absorb row C-04, ADR-8461): per-glob texture edge caps (the first match
//! wins), a triangle ceiling that drops the finest LODs of the largest meshes and never a mesh's
//! last LOD, deterministic variant picks, and the catalog-path rule.
//!
//! Requirements source: DO27's release diet tool (`shell-regia/tools/pack_diet.py` and its tests
//! `tools/ops/test_pack_diet_paths.py`, regia-absorb-4 `c9513623f`, requirements only, not copied).
//! The three DO27 path tests are ported at the end with their assertions kept; DO27's folder and
//! tree names are replaced by neutral ones.

use std::path::{Path, PathBuf};

use regia_asset::{
    AssetId, COOKED_FORMAT_VERSION, CookedHeader, CookedIndexWidth, CookedMesh, CookedMeshLod, CookedSubmesh,
    cooked_file_name, decode_cooked_file, encode_cooked_file,
};
use regia_asset_pipeline::cook_diet::{
    DEFAULT_MAX_TRIANGLES, DietAsset, DietRules, MeshLods, PathAlias, VariantPick, catalog_path,
    collect_catalog, drop_finest_lods, mesh_lod_triangles, plan_diet, plan_lod_drops, variant_groups,
    variant_index,
};
use regia_core::id::{Guid, TypePathHash};

fn id(n: u128) -> AssetId {
    AssetId(Guid::from_u128(n))
}

fn diet(toml: &str) -> DietRules {
    DietRules::from_manifest_toml(toml, None)
        .expect("diet parses")
        .expect("a [content.diet] table")
}

// ---------------------------------------------------------------------------------------------
// The triangle ceiling.

/// The row's fail-first acceptance test: three meshes whose finest LODs total over the default
/// ceiling of 1 500 000 triangles. The diet drops the finest LOD of the largest mesh first and stops
/// as soon as the total fits.
#[test]
fn lod_dropped_over_1_5m_triangles() {
    let rules = diet("[content.diet]\n");
    assert_eq!(
        rules.max_triangles, DEFAULT_MAX_TRIANGLES,
        "the default ceiling is DO27's"
    );
    assert_eq!(rules.max_triangles, 1_500_000);
    let meshes = vec![
        MeshLods::new("assets/hulls/a.glb", id(1), vec![900_000, 200_000, 20_000]),
        MeshLods::new("assets/hulls/b.glb", id(2), vec![500_000, 120_000]),
        MeshLods::new("assets/props/c.glb", id(3), vec![150_000, 15_000]),
    ];
    let plan = plan_lod_drops(&meshes, rules.max_triangles);
    assert_eq!(plan.triangles_before, 1_550_000);
    assert!(plan.reached, "{plan:?}");
    assert_eq!(plan.drops.len(), 1, "one level is enough: {plan:?}");
    let drop = &plan.drops[0];
    assert_eq!(
        drop.path, "assets/hulls/a.glb",
        "the largest mesh loses its finest level"
    );
    assert_eq!(drop.dropped, 1);
    assert_eq!(drop.triangles_before, 900_000);
    assert_eq!(drop.triangles_after, 200_000);
    assert_eq!(plan.triangles_after, 200_000 + 500_000 + 150_000);
    assert!(plan.triangles_after <= 1_500_000);
}

/// Under the ceiling nothing drops; at exactly the ceiling nothing drops either.
#[test]
fn lod_ceiling_keeps_meshes_that_fit() {
    let meshes = vec![
        MeshLods::new("assets/a.glb", id(1), vec![1_000_000, 10]),
        MeshLods::new("assets/b.glb", id(2), vec![500_000, 10]),
    ];
    let plan = plan_lod_drops(&meshes, 1_500_000);
    assert!(plan.reached);
    assert!(plan.drops.is_empty(), "{plan:?}");
    assert_eq!(plan.triangles_after, 1_500_000);
    let none = plan_lod_drops(&meshes, 0);
    assert!(none.reached && none.drops.is_empty(), "0 is no ceiling");
}

/// The ceiling drops levels until the total fits, from whichever mesh is largest at its current
/// level, possibly several levels of one mesh; but a mesh's last LOD always stays, so a ceiling
/// below every last LOD drops every other level and reports that it is not reached.
#[test]
fn diet_never_drops_the_last_lod() {
    let meshes = vec![
        MeshLods::new("assets/a.glb", id(1), vec![800, 400, 300]),
        MeshLods::new("assets/b.glb", id(2), vec![700, 100]),
        MeshLods::new("assets/c.glb", id(3), vec![50]),
    ];
    let plan = plan_lod_drops(&meshes, 10);
    assert!(
        !plan.reached,
        "the last LODs alone are over the ceiling: {plan:?}"
    );
    assert_eq!(
        plan.triangles_after,
        300 + 100 + 50,
        "every mesh is at its last LOD"
    );
    let a = plan
        .drops
        .iter()
        .find(|d| d.path == "assets/a.glb")
        .expect("a drops");
    assert_eq!((a.levels, a.dropped), (3, 2), "a keeps its last LOD");
    let b = plan
        .drops
        .iter()
        .find(|d| d.path == "assets/b.glb")
        .expect("b drops");
    assert_eq!((b.levels, b.dropped), (2, 1), "b keeps its last LOD");
    assert!(
        plan.drops.iter().all(|d| d.path != "assets/c.glb"),
        "a one-level mesh never drops"
    );
    for d in &plan.drops {
        assert!(d.dropped < d.levels, "{d:?}");
    }

    // The ceiling 1000 needs two drops: a (800 → 400), then b (700 → 100): 400 + 100 + 50.
    let plan = plan_lod_drops(&meshes, 1000);
    assert!(plan.reached, "{plan:?}");
    assert_eq!(plan.triangles_after, 550);
    let dropped: Vec<(&str, usize)> = plan.drops.iter().map(|d| (d.path.as_str(), d.dropped)).collect();
    assert_eq!(dropped, vec![("assets/a.glb", 1), ("assets/b.glb", 1)]);

    // A cooked mesh: drop_finest_lods clamps to keep the last level.
    let mut mesh = cooked_mesh(&[4, 2, 1], false);
    let dropped = drop_finest_lods(&mut mesh, 10).expect("drops");
    assert_eq!(dropped, 2);
    assert_eq!(mesh_lod_triangles(&mesh), vec![1]);
    mesh.validate().expect("the compacted mesh validates");
    assert_eq!(
        drop_finest_lods(&mut mesh, 1).expect("no-op"),
        0,
        "the last LOD stays"
    );
}

/// Equal meshes break the tie by path, then id, so the order of the input never changes the plan.
#[test]
fn lod_plan_is_order_independent() {
    let a = MeshLods::new("assets/a.glb", id(1), vec![600, 100]);
    let b = MeshLods::new("assets/b.glb", id(2), vec![600, 100]);
    let c = MeshLods::new("assets/c.glb", id(3), vec![600, 100]);
    let one = plan_lod_drops(&[a.clone(), b.clone(), c.clone()], 1400);
    let two = plan_lod_drops(&[c, b, a], 1400);
    assert_eq!(one, two);
    assert_eq!(one.drops.len(), 1);
    assert_eq!(
        one.drops[0].path, "assets/a.glb",
        "the smaller path drops first on a tie"
    );
}

/// A mesh the diet may not touch (`droppable = 0`) keeps its levels; others drop instead.
#[test]
fn pinned_mesh_never_drops() {
    let mut big = MeshLods::new("assets/a.glb", id(1), vec![900, 10]);
    big.droppable = 0;
    let small = MeshLods::new("assets/b.glb", id(2), vec![300, 10]);
    let plan = plan_lod_drops(&[big, small], 1000);
    assert!(plan.reached);
    assert_eq!(plan.drops.len(), 1);
    assert_eq!(plan.drops[0].path, "assets/b.glb");
}

// ---------------------------------------------------------------------------------------------
// Texture caps.

const CAPS: &str = r#"
[content.diet]
default_max_edge = 4096
texture_caps = [
    { glob = "**/props/**", max_edge = 2048 },
    { glob = "**/props/hero/**", max_edge = 4096 },
    { glob = "**/damage/**", max_edge = 1024 },
    { glob = "ui/**", max_edge = 512 },
]
"#;

/// The first glob that matches sets the cap, even when a later, more specific glob also matches;
/// a texture no glob matches takes `default_max_edge`.
#[test]
fn texture_cap_first_match_wins() {
    let rules = diet(CAPS);
    assert_eq!(rules.cap_for("assets/props/rock/rock_bc.png"), Some(2048));
    assert_eq!(
        rules.cap_for("assets/props/hero/statue.gltf"),
        Some(2048),
        "`**/props/**` comes first, so the later `**/props/hero/**` never applies"
    );
    assert_eq!(
        rules.cap_for("assets/props/tower/damage/d1/x.gltf"),
        Some(2048),
        "props before damage"
    );
    assert_eq!(
        rules.cap_for("assets/buildings/tower/damage/d1/x.gltf"),
        Some(1024)
    );
    assert_eq!(
        rules.cap_for("ui/hud.png"),
        Some(512),
        "paths with or without assets/"
    );
    assert_eq!(
        rules.cap_for("assets/UI/Hud.PNG"),
        Some(512),
        "ASCII case is ignored"
    );
    assert_eq!(
        rules.cap_for("assets/myprops/x.png"),
        Some(4096),
        "a glob folder is a whole folder"
    );
    assert_eq!(
        rules.cap_rule("assets/props/a.png"),
        Some((2048, Some("**/props/**")))
    );
    assert_eq!(rules.cap_rule("assets/terrain/a.png"), Some((4096, None)));

    let none = diet("[content.diet]\ndefault_max_edge = 0\n");
    assert_eq!(none.cap_for("assets/terrain/a.png"), None, "0 is no cap");
}

/// A bad diet table fails with `diet_invalid` and names the key.
#[test]
fn bad_diet_tables_are_errors() {
    for (toml, needle) in [
        ("[content.diet]\nmax_triangle = 5\n", "unknown key `max_triangle`"),
        ("[content.diet]\nmax_triangles = -1\n", "max_triangles"),
        ("[content.diet]\ntexture_caps = [{ glob = \"a\" }]\n", "max_edge"),
        (
            "[content.diet]\ntexture_caps = [{ glob = \"a\", max_edge = 0 }]\n",
            "at least 1",
        ),
        (
            "[content.diet]\nvariants = [{ glob = \"a\", pick = \"random\" }]\n",
            "pick",
        ),
        ("[content]\ndiet = 3\n", "must be a table"),
    ] {
        let err = DietRules::from_manifest_toml(toml, None).expect_err(toml);
        assert_eq!(err.code(), "diet_invalid", "{toml}");
        assert!(err.to_string().contains(needle), "{toml}: {err}");
        assert!(err.to_string().contains("[diet_invalid]"), "{err}");
    }
    assert!(
        DietRules::from_manifest_toml("[content]\nbase_url = \"x\"\n", None)
            .expect("no diet")
            .is_none()
    );
}

/// A profile's `[profiles.<name>.diet]` replaces the keys it names.
#[test]
fn profile_diet_overrides_keys() {
    let toml = r#"
[content.diet]
max_triangles = 1000
seed = 1
texture_caps = [{ glob = "**", max_edge = 256 }]

[profiles.mobile.diet]
max_triangles = 500
seed = 9
"#;
    let base = DietRules::from_manifest_toml(toml, None)
        .expect("ok")
        .expect("diet");
    let mobile = DietRules::from_manifest_toml(toml, Some("mobile"))
        .expect("ok")
        .expect("diet");
    let other = DietRules::from_manifest_toml(toml, Some("desktop"))
        .expect("ok")
        .expect("diet");
    assert_eq!((base.max_triangles, base.seed), (1000, 1));
    assert_eq!((mobile.max_triangles, mobile.seed), (500, 9));
    assert_eq!(
        mobile.texture_caps, base.texture_caps,
        "keys the profile does not name stay"
    );
    assert_eq!(other, base);
    let only_profile = DietRules::from_manifest_toml("[profiles.mobile.diet]\nseed = 3\n", Some("mobile"))
        .expect("ok")
        .expect("a profile diet alone is a diet");
    assert_eq!(only_profile.seed, 3);
    assert_eq!(only_profile.max_triangles, DEFAULT_MAX_TRIANGLES);
}

// ---------------------------------------------------------------------------------------------
// Variant picks.

fn variant_paths() -> Vec<String> {
    [
        "assets/props/rocks/rock_c.glb",
        "assets/props/rocks/rock_a.glb",
        "assets/props/rocks/rock_b.glb",
        "assets/props/rocks/rock_b.glb#mesh0",
        "assets/props/trees/oak_2.glb",
        "assets/props/trees/oak_1.glb",
        "assets/levels/arena.level.ron",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

/// The same rules, seed and paths give the same picks, whatever order the paths come in; `first`
/// keeps the first path in byte order; `seeded` with seed 0 is DO27's FNV-1a pick, and a different
/// seed is still deterministic.
#[test]
fn variant_pick_is_deterministic() {
    let rules = diet(
        "[content.diet]\nvariants = [{ glob = \"props/rocks/*\", pick = \"seeded\" }, { glob = \"props/**\" }]\n",
    );
    assert_eq!(rules.variants[0].pick, VariantPick::Seeded);
    assert_eq!(
        rules.variants[1].pick,
        VariantPick::First,
        "pick defaults to first"
    );
    let paths = variant_paths();
    let groups = variant_groups(&rules, &paths);
    let mut reversed = paths.clone();
    reversed.reverse();
    assert_eq!(
        groups,
        variant_groups(&rules, &reversed),
        "input order does not matter"
    );
    for _ in 0..3 {
        assert_eq!(
            groups,
            variant_groups(&rules, &paths),
            "the same input, the same picks"
        );
    }

    assert_eq!(groups.len(), 2, "{groups:?}");
    let rocks = &groups[0];
    assert_eq!(rocks.group, "props/rocks");
    assert_eq!(
        rocks.members,
        vec![
            "assets/props/rocks/rock_a.glb",
            "assets/props/rocks/rock_b.glb",
            "assets/props/rocks/rock_c.glb"
        ],
        "a sub-asset goes with its file"
    );
    // FNV-1a 32 of "props/rocks" = 0x9cdda988; % 3 = 1.
    assert_eq!(
        regia_asset_pipeline::cook_diet::fnv1a32(b"props/rocks"),
        0x9cdd_a988
    );
    assert_eq!(variant_index("props/rocks", 3, VariantPick::Seeded, 0), Some(1));
    assert_eq!(rocks.picked, "assets/props/rocks/rock_b.glb");
    assert_eq!(rocks.left_out.len(), 2);

    let trees = &groups[1];
    assert_eq!(trees.group, "props/trees");
    assert_eq!(trees.pick, VariantPick::First);
    assert_eq!(
        trees.picked, "assets/props/trees/oak_1.glb",
        "first in byte order"
    );

    // A seed changes the hash input, not the determinism.
    let mut seeded = rules.clone();
    seeded.seed = 42;
    let a = variant_groups(&seeded, &paths);
    assert_eq!(a, variant_groups(&seeded, &reversed));
    assert_eq!(
        variant_index("g", 0, VariantPick::Seeded, 42),
        None,
        "an empty group picks nothing"
    );
    assert_eq!(variant_index("g", 5, VariantPick::First, 42), Some(0));
}

// ---------------------------------------------------------------------------------------------
// The profile plan over a cooked cache.

/// A cooked mesh with one LOD per entry of `tris` (triangles), all over three vertices; one submesh
/// covering LOD0, or two when `split`.
fn cooked_mesh(tris: &[u32], split: bool) -> CookedMesh {
    let mut indices = Vec::new();
    let mut lods = Vec::new();
    for (level, t) in tris.iter().enumerate() {
        let offset = indices.len() as u32;
        for _ in 0..*t {
            for i in [0u16, 1, 2] {
                indices.extend_from_slice(&i.to_le_bytes());
            }
        }
        lods.push(CookedMeshLod {
            screen_relative_size: if level + 1 == tris.len() {
                0.0
            } else {
                1.0 / (level as f32 + 2.0)
            },
            index_byte_offset: offset,
            index_byte_length: indices.len() as u32 - offset,
        });
    }
    let first = lods[0].clone();
    let submeshes = if split {
        let half = (first.index_byte_length / 12) * 6;
        vec![
            CookedSubmesh {
                index_byte_offset: 0,
                index_byte_length: half,
                material_slot: 0,
            },
            CookedSubmesh {
                index_byte_offset: half,
                index_byte_length: first.index_byte_length - half,
                material_slot: 1,
            },
        ]
    } else {
        vec![CookedSubmesh {
            index_byte_offset: first.index_byte_offset,
            index_byte_length: first.index_byte_length,
            material_slot: 0,
        }]
    };
    CookedMesh {
        aabb_min: [0.0; 3],
        aabb_max: [1.0; 3],
        bounding_sphere: [0.5, 0.5, 0.5, 0.9],
        stream_positions: vec![0; 36],
        stream_normals_tangents: vec![0; 24],
        stream_uv0: vec![0; 12],
        indices,
        index_width: CookedIndexWidth::U16,
        vertex_count: 3,
        lods,
        submeshes,
        lod_metric: regia_asset::LOD_METRIC_ERROR,
        format_version: regia_asset::COOKED_MESH_FORMAT_VERSION,
        skin: None,
        stream_color0: Vec::new(),
        morph_targets: Vec::new(),
        skin_influences: regia_asset::DEFAULT_SKIN_INFLUENCES,
    }
}

fn write_cooked(cache: &Path, asset: &DietAsset, payload: &[u8], deps: &[AssetId]) {
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: TypePathHash::of(&asset.type_path),
        asset_id: asset.id,
        importer_version: 1,
        dependencies: deps.to_vec(),
        payload_len: payload.len() as u64,
    };
    let file = encode_cooked_file(&header, payload).expect("encode");
    std::fs::write(cache.join(cooked_file_name(asset.id)), file).expect("write cooked");
}

fn mesh_asset(n: u128, path: &str) -> DietAsset {
    DietAsset {
        path: path.into(),
        id: id(n),
        type_path: "regia_asset::CookedMesh".into(),
    }
}

/// A profile pak over a cooked cache: the ceiling rewrites the largest mesh with its finest LOD
/// gone (the override decodes, validates and keeps the coarser levels' indices), a mesh with two
/// submeshes is pinned, a variant that is not picked is left out unless a shipped asset depends on
/// it, and the report's totals agree with the plan.
#[test]
fn plan_diet_rewrites_meshes_and_leaves_out_variants() {
    let root = regia_test_utils::unique_temp_dir("cook-diet-plan");
    let cache = root.join("cooked");
    std::fs::create_dir_all(&cache).expect("mkdir");

    let big = mesh_asset(1, "assets/hulls/big.glb");
    let pinned = mesh_asset(2, "assets/hulls/pinned.glb");
    let rock_a = mesh_asset(3, "assets/props/rocks/rock_a.glb");
    let rock_b = mesh_asset(4, "assets/props/rocks/rock_b.glb");
    let tree_a = mesh_asset(5, "assets/props/trees/tree_a.glb");
    let tree_b = mesh_asset(6, "assets/props/trees/tree_b.glb");
    let level = DietAsset {
        path: "assets/levels/arena.level.ron".into(),
        id: id(7),
        type_path: "regia_asset::CookedData".into(),
    };
    let big_mesh = cooked_mesh(&[60, 20, 5], false);
    write_cooked(&cache, &big, &big_mesh.to_postcard().expect("pc"), &[]);
    write_cooked(
        &cache,
        &pinned,
        &cooked_mesh(&[50, 10], true).to_postcard().expect("pc"),
        &[],
    );
    for a in [&rock_a, &rock_b, &tree_a, &tree_b] {
        write_cooked(
            &cache,
            a,
            &cooked_mesh(&[2], false).to_postcard().expect("pc"),
            &[],
        );
    }
    // The level depends on tree_b, which `first` does not pick.
    write_cooked(&cache, &level, b"level", &[tree_b.id]);

    let rules =
        diet("[content.diet]\nmax_triangles = 100\nvariants = [{ glob = \"props/**\", pick = \"first\" }]\n");
    let assets = vec![
        level.clone(),
        tree_b.clone(),
        tree_a.clone(),
        rock_b.clone(),
        rock_a.clone(),
        pinned.clone(),
        big.clone(),
    ];
    let plan = plan_diet(&cache, &assets, &rules, "release", "linux").expect("plan");
    let report = &plan.report;

    // Variants: rock_b left out; tree_b kept because the level depends on it.
    assert_eq!(plan.dropped.iter().copied().collect::<Vec<_>>(), vec![rock_b.id]);
    let trees = report
        .variants
        .iter()
        .find(|g| g.group == "props/trees")
        .expect("trees");
    assert_eq!(trees.kept_referenced, vec![tree_b.path.clone()]);
    assert!(trees.left_out.is_empty());
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.starts_with("diet_variant_referenced:")),
        "{:?}",
        report.warnings
    );

    // Ceiling: 60 + 50 + 2 + 2 + 2 = 116 > 100 once rock_b is left out. `pinned` (two submeshes)
    // never drops, so big loses LOD0 (60 → 20): 76. `before` is before the whole diet, rock_b
    // included.
    assert_eq!(report.before.triangles, 60 + 50 + 2 + 2 + 2 + 2);
    assert_eq!(report.after.triangles, 20 + 50 + 2 + 2 + 2);
    assert!(report.ceiling_reached);
    assert!(
        report.warnings.iter().any(|w| w.starts_with("diet_mesh_pinned:")),
        "{:?}",
        report.warnings
    );
    assert_eq!(report.lods.len(), 1);
    assert_eq!(report.lods[0].path, big.path);
    assert_eq!(report.lods[0].dropped, 1);

    let file = plan.overrides.get(&big.id).expect("the big mesh is rewritten");
    let (header, payload) = decode_cooked_file(file).expect("override decodes");
    assert_eq!(header.asset_id, big.id);
    let rewritten = CookedMesh::from_postcard(payload).expect("rewritten mesh decodes and validates");
    assert_eq!(mesh_lod_triangles(&rewritten), vec![20, 5]);
    assert_eq!(rewritten.lods[0].index_byte_offset, 0);
    assert_eq!(
        rewritten.indices.len(),
        (20 + 5) * 6,
        "the dropped level's indices are gone"
    );
    assert_eq!(rewritten.submeshes.len(), 1);
    assert_eq!(
        rewritten.submeshes[0].index_byte_length,
        rewritten.lods[0].index_byte_length
    );
    assert_eq!(rewritten.lods.last().expect("last").screen_relative_size, 0.0);
    assert_eq!(
        rewritten.lod_indices(0).expect("lod 0"),
        big_mesh.lod_indices(1).expect("old lod 1"),
        "the new finest level is the old LOD1"
    );

    // Totals.
    assert_eq!(report.before.assets, 7);
    assert_eq!(report.after.assets, 6);
    assert!(report.after.bytes < report.before.bytes);
    assert_eq!(
        (report.profile.as_str(), report.platform.as_str()),
        ("release", "linux")
    );

    // The same inputs in another order give the same report and overrides.
    let mut shuffled = assets.clone();
    shuffled.rotate_left(3);
    let again = plan_diet(&cache, &shuffled, &rules, "release", "linux").expect("plan");
    assert_eq!(again.report, plan.report);
    assert_eq!(again.overrides, plan.overrides);

    // The report round-trips through its JSON file.
    let path = root.join(regia_asset_pipeline::cook_diet::DIET_REPORT_FILE);
    regia_asset_pipeline::cook_diet::write_report(&path, report).expect("write");
    let read: regia_asset_pipeline::cook_diet::DietReport =
        serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
    assert_eq!(&read, report);
    let _ = std::fs::remove_dir_all(&root);
}

/// A missing cooked file is `diet_cooked_unreadable`, not a panic.
#[test]
fn plan_diet_missing_cooked_file_is_an_error() {
    let root = regia_test_utils::unique_temp_dir("cook-diet-missing");
    let err = plan_diet(
        &root,
        &[mesh_asset(9, "assets/x.glb")],
        &DietRules::default(),
        "p",
        "linux",
    )
    .expect_err("missing file");
    assert_eq!(err.code(), "diet_cooked_unreadable");
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// DO27's three path tests (`tools/ops/test_pack_diet_paths.py`), ported.

/// DO27 `CapFor.test_damage_and_wreck_are_2k_intact_building_is_4k`: with DO27's caps written as
/// diet rules, damage stages, wrecks and props are 2048 and an intact building is 4096.
#[test]
fn do27_damage_and_wreck_are_2k_intact_building_is_4k() {
    let rules = diet(
        r#"
[content.diet]
default_max_edge = 4096
texture_caps = [
    { glob = "**/props/**", max_edge = 2048 },
    { glob = "**/scatter/**", max_edge = 2048 },
    { glob = "**/wreck/**", max_edge = 2048 },
    { glob = "**/damage/**", max_edge = 2048 },
]
"#,
    );
    for rel in [
        "buildings/tower/damage/d1/x.gltf",
        "buildings/silo/wreck/x.gltf",
        "vehicles/v1/wreck/x.gltf",
        "walkers/a/damage/x.gltf",
        "props/p/x.gltf",
    ] {
        assert_eq!(rules.cap_for(&format!("tree/{rel}")), Some(2048), "{rel}");
    }
    assert_eq!(
        rules.cap_for("tree/buildings/tower/SM_Tower_LOD0.gltf"),
        Some(4096)
    );
}

/// DO27 `CatalogPaths.test_legacy_catalog_and_dressing_are_collected_without_legacy_tree`: catalog
/// rows and dressing sources that name the pre-rename tree are collected from the current tree
/// (which alone exists), and none is reported stale.
#[test]
fn do27_legacy_catalog_and_dressing_are_collected_without_legacy_tree() {
    let tmp = regia_test_utils::unique_temp_dir("cook-diet-catalog");
    let repo = tmp.join("repo");
    let current = repo.join("current_tree");
    for rel in ["vehicles/test/hull.gltf", "props/test/prop.gltf"] {
        let path = current.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, r#"{"asset":{"version":"2.0"}}"#).expect("write");
    }
    let aliases = vec![PathAlias {
        from: "legacy_tree/".into(),
        to: "current_tree/".into(),
    }];
    // The catalog row and the dressing source, both written against the legacy tree.
    let rows = vec![
        "legacy_tree/vehicles/test/hull.gltf".to_string(),
        "legacy_tree/props/test/prop.gltf".to_string(),
    ];
    let found = collect_catalog(&repo, &rows, &aliases);
    let got: std::collections::BTreeSet<&str> = found.paths.iter().map(String::as_str).collect();
    assert_eq!(
        got,
        [
            "current_tree/vehicles/test/hull.gltf",
            "current_tree/props/test/prop.gltf"
        ]
        .into_iter()
        .collect()
    );
    assert!(
        !found.notes.iter().any(|n| n.contains("stale")),
        "{:?}",
        found.notes
    );

    // A row whose file exists under neither tree is a stale note, not a failure.
    let stale = collect_catalog(&repo, &["legacy_tree/gone.gltf".to_string()], &aliases);
    assert!(stale.paths.is_empty());
    assert_eq!(stale.notes, vec!["stale catalog row legacy_tree/gone.gltf"]);
    let _ = std::fs::remove_dir_all(&tmp);
}

/// DO27 `CatalogPaths.test_current_and_unrelated_paths_are_unchanged`: only a leading legacy prefix
/// is rewritten; a current path and a nested folder of the legacy name are left alone.
#[test]
fn do27_current_and_unrelated_paths_are_unchanged() {
    let root = PathBuf::from("/example");
    let aliases = vec![PathAlias {
        from: "legacy_tree/".into(),
        to: "current_tree/".into(),
    }];
    for rel in ["current_tree/a.gltf", "nested/legacy_tree/a.gltf"] {
        assert_eq!(catalog_path(&root, rel, &aliases), root.join(rel), "{rel}");
    }
    assert_eq!(
        catalog_path(&root, "legacy_tree/a.gltf", &aliases),
        root.join("current_tree/a.gltf")
    );
    // The same rule applies before the diet's globs.
    let rules = DietRules {
        path_aliases: aliases,
        texture_caps: vec![regia_asset_pipeline::cook_diet::TextureCap {
            glob: "current_tree/**".into(),
            max_edge: 64,
        }],
        ..DietRules::default()
    };
    assert_eq!(rules.cap_for("legacy_tree/a.png"), Some(64));
    assert_eq!(rules.cap_for("nested/legacy_tree/a.png"), Some(4096));
}

// ---------------------------------------------------------------------------------------------
// Texture caps in the cook, end to end.

/// A 64×64 opaque RGBA PNG.
fn png64() -> Vec<u8> {
    use image::ImageEncoder;
    let mut pixels = Vec::with_capacity(64 * 64 * 4);
    for y in 0..64u32 {
        for x in 0..64u32 {
            pixels.extend_from_slice(&[(x * 4) as u8, (y * 4) as u8, ((x + y) * 2) as u8, 255]);
        }
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&pixels, 64, 64, image::ExtendedColorType::Rgba8)
        .expect("png encode");
    out
}

const PROP_TEX: &str = "assets/props/crate_albedo.png";
const TERRAIN_TEX: &str = "assets/terrain/ground_albedo.png";

fn diet_project(root: &Path, diet_table: &str) {
    std::fs::write(
        root.join("Regia.toml"),
        format!("[project]\nname = \"diet\"\nid = \"com.example.diet\"\n{diet_table}"),
    )
    .expect("Regia.toml");
}

fn cook_linux(root: &Path) -> regia_asset_pipeline::CookReport {
    regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
        project_root: root.to_path_buf(),
        platform: "linux".into(),
        incremental: true,
    })
    .expect("cook")
}

/// The cooked width of each test texture, `[PROP_TEX, TERRAIN_TEX]`.
fn widths(root: &Path) -> Vec<u32> {
    let mut server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
        project_root: root.to_path_buf(),
        platform: "linux".into(),
        content_dir: None,
    })
    .expect("server");
    [PROP_TEX, TERRAIN_TEX]
        .iter()
        .map(|rel| {
            let id = server.id_for_path(rel).expect("in the manifest");
            let handle = server.load_texture(id).expect("a cooked texture");
            server.textures().get(&handle).expect("texture").width
        })
        .collect()
}

/// `[content.diet] texture_caps` reaches the cook: a texture the first glob matches is cooked at
/// most that edge, a texture no glob matches keeps its size (`default_max_edge = 0`), a cap change
/// re-cooks the texture it covers, and a malformed diet fails the cook with `diet_invalid`.
#[test]
fn texture_cap_applies_in_the_cook() {
    let root = regia_test_utils::unique_temp_dir("cook-diet-cook");
    for rel in [PROP_TEX, TERRAIN_TEX] {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, png64()).expect("png");
    }
    diet_project(&root, "");
    let plain = cook_linux(&root);
    assert!(plain.errors.is_empty(), "{:?}", plain.errors);
    assert_eq!(widths(&root), vec![64, 64], "no diet: no cap");

    diet_project(
        &root,
        "[content.diet]\ndefault_max_edge = 0\ntexture_caps = [{ glob = \"props/**\", max_edge = 16 }, { glob = \"**\", max_edge = 8 }]\n",
    );
    let capped = cook_linux(&root);
    assert!(capped.errors.is_empty(), "{:?}", capped.errors);
    assert_eq!(
        widths(&root),
        vec![16, 8],
        "each texture takes the first glob that matches"
    );

    diet_project(
        &root,
        "[content.diet]\ndefault_max_edge = 0\ntexture_caps = [{ glob = \"props/**\", max_edge = 32 }]\n",
    );
    let changed = cook_linux(&root);
    assert!(changed.errors.is_empty(), "{:?}", changed.errors);
    assert_eq!(
        widths(&root),
        vec![32, 64],
        "the cap change re-cooks both textures it moved"
    );

    diet_project(&root, "[content.diet]\nmax_edge = 3\n");
    let bad = cook_linux(&root);
    assert!(
        bad.errors.iter().any(|(file, err)| file == "Regia.toml"
            && err.contains("[diet_invalid]")
            && err.contains("max_edge")),
        "{:?}",
        bad.errors
    );
    let _ = std::fs::remove_dir_all(&root);
}
