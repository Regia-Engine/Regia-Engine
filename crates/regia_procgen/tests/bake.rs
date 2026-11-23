// SPDX-License-Identifier: MPL-2.0
//! Spec 62 § 10 test 13: bake writes `.pgout` atomically, a reload shows the same output hash
//! without running the graph, verify passes, and a parameter edit without a re-bake fails verify.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_procgen::world::{
    ProcBounds, ProcGenPlugin, ProcGenSettings, ProcGenState, ProcGenTerrain, ProcGenerator, ProcGraphSource,
    ProcOutput, bake, bake_path, load_bake, verify,
};
use regia_procgen::{Edge, NodeEntry, ProcGraph, SettingValue};
use regia_scene::components::EntityGuid;
use regia_scene::{GlobalTransform, Transform};

fn graph_text() -> String {
    let n = |ty: &str, s: &[(&str, &str)]| {
        let mut e = NodeEntry::new(ty);
        for (k, v) in s {
            e = e.with(k, SettingValue::parse(v).expect("setting"));
        }
        e
    };
    let mut g = ProcGraph {
        nodes: [
            (1, n("in.self", &[])),
            (
                2,
                n(
                    "sample.ground",
                    &[("spacing_m", "Bind(\"spacing\")"), ("jitter", "0.7")],
                ),
            ),
            (3, n("spawn.mesh", &[("entries", "[(mesh: \"rock.glb\")]")])),
            (4, n("spawn.prefab", &[("prefab", "\"marker.prefab.ron\"")])),
            (5, n("points.keep_random", &[("ratio", "0.1")])),
        ]
        .into(),
        edges: vec![
            Edge::new(1, "out", 2, "region"),
            Edge::new(2, "out", 3, "in"),
            Edge::new(3, "out", 5, "in"),
            Edge::new(5, "out", 4, "in"),
        ],
        ..ProcGraph::default()
    };
    g.params.push(regia_procgen::graph::ParamDecl {
        name: "spacing".into(),
        ty: regia_procgen::AttrType::Float,
        default: SettingValue::Float(4.0),
        min: Some(0.5),
        max: Some(64.0),
        doc: String::new(),
    });
    g.to_ron()
}

/// Audit M18: an `OnLoad` generator does not bake or verify (its file would load as current if the
/// mode were flipped to `Baked` later).
#[test]
fn only_baked_generators_bake() {
    let dir = regia_test_utils::unique_temp_dir("procgen-bake-mode");
    let (mut app, e) = app(&dir);
    app.world_mut()
        .get_mut::<ProcGenerator>(e)
        .expect("generator")
        .mode = regia_procgen::world::ProcGenMode::OnLoad;
    let err = bake(app.world_mut(), e).expect_err("not baked");
    assert_eq!(err.0, "procgen_not_baked");
    assert_eq!(
        verify(app.world_mut(), e).map(|_| ()).expect_err("verify").0,
        "procgen_not_baked"
    );
    assert!(!bake_path(&dir, Guid(0xabc)).exists(), "no file written");
    let _ = std::fs::remove_dir_all(&dir);
}

fn app(dir: &std::path::Path) -> (App, Entity) {
    let mut app = App::new();
    let files: BTreeMap<String, String> = [("rocks.pgraph.ron".to_string(), graph_text())].into();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files),
        bake_dir: Some(dir.to_path_buf()),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    let heights: Vec<f32> = (0..129 * 129).map(|i| ((i % 129) as f32) * 0.05).collect();
    let map = regia_terrain::TerrainMap::from_heights(129, 129, heights, 256.0, 1.0, 0.0).expect("map");
    app.insert_resource(ProcGenTerrain(map));
    let e = app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(40.0, 50.0, 40.0),
                },
                ..ProcGenerator::new("rocks.pgraph.ron")
            },
            EntityGuid(Guid(0xabc)),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id();
    (app, e)
}

#[test]
fn test_13_bake_reload_verify_and_stale_bake() {
    let dir = regia_test_utils::unique_temp_dir("procgen-bake");
    let (mut first, e) = app(&dir);
    let path = bake(first.world_mut(), e).expect("bake");
    assert_eq!(path, bake_path(&dir, Guid(0xabc)));
    assert!(path.exists());
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .expect("dir")
        .filter_map(Result::ok)
        .filter(|f| f.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "no temporary files left behind");
    let baked_hash = first
        .world()
        .get::<ProcGenState>(e)
        .and_then(|s| s.output_hash)
        .expect("hash");
    let instances = first
        .world()
        .get::<ProcOutput>(e)
        .map(|o| o.0.instance_count())
        .unwrap_or(0);
    assert!(instances > 50, "baked something: {instances}");

    // A fresh world (the level reloaded): the bake loads on the first update, no evaluation.
    let (mut second, e2) = app(&dir);
    second.update();
    let st = second.world().get::<ProcGenState>(e2).expect("state");
    assert_eq!(st.output_hash, Some(baked_hash), "same output from the file");
    assert_eq!(st.evaluations, 0, "the graph did not run");
    assert_eq!(
        second.world().get::<ProcOutput>(e2).map(|o| o.0.instance_count()),
        Some(instances)
    );
    assert_eq!(
        load_bake(second.world_mut(), e2).expect("explicit load"),
        baked_hash
    );
    assert!(
        !second.world().get::<ProcGenState>(e2).expect("state").stale,
        "a matching bake loads fresh"
    );

    let v = verify(second.world_mut(), e2).expect("verify");
    assert!(v.ok, "verify passes: {v:?}");

    // Edit a parameter and do not re-bake: verify fails, naming both hashes.
    second
        .world_mut()
        .get_mut::<ProcGenerator>(e2)
        .expect("gen")
        .params
        .insert("spacing".into(), "3.0".into());
    let v = verify(second.world_mut(), e2).expect("verify runs");
    assert!(!v.ok);
    assert_eq!(v.file_hash, Some(baked_hash));
    assert_ne!(v.run_hash, baked_hash);
    // Audit M11: loading the old bake after the edit serves it, marked stale.
    load_bake(second.world_mut(), e2).expect("load the old bake");
    assert!(
        second.world().get::<ProcGenState>(e2).expect("state").stale,
        "a bake of other parameters loads stale"
    );

    // A hand edit of the file is caught on load.
    let mut bytes = std::fs::read(&path).expect("read");
    let at = bytes.len() - 10;
    bytes[at] ^= 0x01;
    std::fs::write(&path, &bytes).expect("write");
    let (mut third, e3) = app(&dir);
    let code = load_bake(third.world_mut(), e3)
        .map(|_| ())
        .expect_err("corrupt")
        .0;
    assert!(code.starts_with("pgout_"), "a hand edit is refused: {code}");
    let v = verify(third.world_mut(), e3).expect("verify runs");
    assert!(
        !v.ok && v.file_error.is_some(),
        "verify fails on the edited file: {v:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn both_forms_of_a_level_share_a_bake_directory() {
    use std::path::Path;
    let expected = Path::new("assets/maps/forest.procgen");
    for level in [
        "assets/maps/forest.level.ron",
        "assets/maps/forest.level.d",
        "assets/maps/forest.level.d/",
    ] {
        assert_eq!(
            regia_procgen::world::level_bake_dir(Path::new(level)),
            expected,
            "{level}"
        );
    }
}

/// corr-low: the query dependency's hash arm concatenated `tags` and `any_tags` without a
/// separator, so `(["ab"], [])` and `(["a"], ["b"])` hashed identically even though the sort
/// key distinguishes them: swapping one dep for the other kept the old hash.
#[test]
fn query_input_hash_separates_tags_from_any_tags() {
    use regia_procgen::world::{ProcDependency, input_hash};
    let query = |tags: &[&str], any: &[&str]| {
        vec![ProcDependency::Query {
            tags: tags.iter().map(|s| (*s).to_string()).collect(),
            any_tags: any.iter().map(|s| (*s).to_string()).collect(),
            matched: Default::default(),
        }]
    };
    let left = input_hash(&query(&["ab"], &[]));
    let right = input_hash(&query(&["a"], &["b"]));
    assert_ne!(left, right, "distinct query deps must not collide");
    let mut deps = query(&["b"], &["a"]);
    deps.extend(query(&["ab"], &[]));
    let _ = input_hash(&deps);
}

/// Determinism audit finding 4: the `.pgout` input hash must not depend on the ECS query order
/// of the `BiomeArea` entities (spawn order, or a save/load cycle, changes it). Two areas with
/// guids A < B and different definitions, inserted in both orders: the baked files must be
/// identical, byte for byte.
#[test]
fn biome_input_hash_ignores_area_query_order() {
    use regia_procgen::biome::STARTER_BIOMES_GRAPH;
    use regia_scene::components::Name;
    const MEADOW: &str = r#"BiomeDef(
    name: "Meadow",
    priority: 10,
    layers: [(
        name: "flowers",
        source: Ground(spacing_m: 2.0, jitter: 0.5),
        entries: [(name: "flower", spawn: Mesh("flower.glb"), footprint: (core_m: Some(0.1), spread_m: Some(0.4)))],
    )],
    grass: [(class: "meadow", density: 0.8)],
)"#;
    const HEATH: &str = r#"BiomeDef(
    name: "Heath",
    priority: 10,
    layers: [(
        name: "heather",
        source: Ground(spacing_m: 3.0, jitter: 0.5),
        entries: [(name: "heather", spawn: Mesh("heather.glb"), footprint: (core_m: Some(0.1), spread_m: Some(0.4)))],
    )],
    grass: [(class: "heath", density: 0.6)],
)"#;

    let bake_in = |order_low_first: bool| {
        let dir = regia_test_utils::unique_temp_dir("procgen-biome-order");
        let files: BTreeMap<String, String> = [
            ("biomes.pgraph.ron".to_string(), STARTER_BIOMES_GRAPH.to_string()),
            ("meadow.biome.ron".to_string(), MEADOW.to_string()),
            ("heath.biome.ron".to_string(), HEATH.to_string()),
        ]
        .into();
        let mut app = App::new();
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(files),
            bake_dir: Some(dir.to_path_buf()),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        let map = regia_terrain::TerrainMap::from_heights(129, 129, vec![1.0; 129 * 129], 128.0, 1.0, 0.0)
            .expect("map");
        app.insert_resource(ProcGenTerrain(map));
        let e = app
            .world_mut()
            .spawn((
                ProcGenerator {
                    bounds: ProcBounds::Box {
                        half_extents: Vec3::new(60.0, 20.0, 60.0),
                    },
                    ..ProcGenerator::new("biomes.pgraph.ron")
                },
                EntityGuid(Guid(0x77)),
                Transform::default(),
                GlobalTransform::default(),
            ))
            .id();
        let area = |biome: &'static str, guid: u128| {
            (
                regia_procgen::biome::ecs::BiomeArea {
                    blend_m: 4.0,
                    ..regia_procgen::biome::ecs::BiomeArea::new(biome)
                },
                EntityGuid(Guid(guid)),
                Name::new(biome),
                Transform {
                    scale: Vec3::new(120.0, 10.0, 120.0),
                    ..Transform::from_translation(Vec3::new(0.0, 0.0, 0.0))
                },
            )
        };
        let low = area("heath.biome.ron", 0x100);
        let high = area("meadow.biome.ron", 0x200);
        if order_low_first {
            app.world_mut().spawn(low);
            app.world_mut().spawn(high);
        } else {
            app.world_mut().spawn(high);
            app.world_mut().spawn(low);
        }
        let path = bake(app.world_mut(), e).expect("bake");
        let bytes = std::fs::read(&path).expect("read the bake");
        let _ = std::fs::remove_dir_all(&dir);
        bytes
    };

    let low_first = bake_in(true);
    let high_first = bake_in(false);
    assert!(!low_first.is_empty());
    assert_eq!(
        low_first, high_first,
        "the input hash and the whole bake are independent of the area query order"
    );
}

/// `input_hash` must tell `Query` dependencies apart exactly: the tag lists are framed, not
/// concatenated, so `["ab"]` ≠ `["a"] + any ["b"]` ≠ `["a", "b"]` at the same matched set (the
/// sort key gets the same framing, so equal deps still sort adjacently and the hash stays
/// capture-order independent).
#[test]
fn input_hash_distinguishes_query_deps_that_differ_only_in_tag_split() {
    use regia_procgen::world::{ProcDependency, input_hash};
    let matched: std::collections::BTreeSet<Guid> = [Guid::from_u128(0x51)].into_iter().collect();
    let dep = |tags: &[&str], any_tags: &[&str]| ProcDependency::Query {
        tags: tags.iter().map(|t| t.to_string()).collect(),
        any_tags: any_tags.iter().map(|t| t.to_string()).collect(),
        matched: matched.clone(),
    };
    let ab = dep(&["ab"], &[]);
    let a_any_b = dep(&["a"], &["b"]);
    let a_b = dep(&["a", "b"], &[]);
    assert_ne!(
        input_hash(std::slice::from_ref(&ab)),
        input_hash(std::slice::from_ref(&a_any_b)),
        "the sort frames tags; the hash must too"
    );
    assert_ne!(
        input_hash(std::slice::from_ref(&a_any_b)),
        input_hash(std::slice::from_ref(&a_b)),
    );
    assert_ne!(
        input_hash(std::slice::from_ref(&ab)),
        input_hash(std::slice::from_ref(&a_b)),
    );
    // A mixed list hashes the same whatever order the capture walked it in.
    assert_eq!(input_hash(&[ab.clone(), a_b.clone()]), input_hash(&[a_b, ab]));
}
