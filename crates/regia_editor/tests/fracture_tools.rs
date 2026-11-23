// SPDX-License-Identifier: MPL-2.0
//! The Fracture panel's supports, test break, bond view and Place, end to end through the
//! registered editor methods (ADR-9750, `book/agents/fracture.md`).
//!
//! The acceptance wall: a 3x3x1 box cut on grid sites (nine bricks, the bottom row 0, 1, 2), the
//! bottom row held by the world. A hit on the top centre brick breaks exactly its three bonds and
//! knocks it loose; the panel's `structure_hash` equals a game's `DestructionWorld` fed the saved
//! file, the same pose, seed and hit, bit for bit and twice. The placed wall, loaded from the saved
//! level in a headless play with `DestructionPlugin`, breaks the same way, and undoing Place gives
//! back the level's bytes.

use std::sync::Arc;
use std::time::Duration;

use regia::physics::destruction::{
    Authority, Damage, DamageKind, DestructibleAsset, DestructiblePose, DestructibleSourceStatus,
    Destruction, DestructionPlugin, DestructionWorld,
};
use regia_agent::extension::AgentExtension;
use regia_agent::protocol::RpcError;
use regia_app::prelude::*;
use regia_app::{FixedTime, TimeUpdateStrategy};
use serde_json::{Value, json};

/// Half extents of the wall: 3 m x 3 m x 0.5 m, so each brick is 1 m x 1 m x 0.5 m.
const WALL_HALF: [f32; 3] = [1.5, 1.5, 0.25];
/// The top centre brick (x 1, y 2 on the grid) and its centre.
const TOP_CENTRE: u32 = 7;
const TOP_CENTRE_POINT: [f32; 3] = [0.0, 1.0, 0.0];
/// 500 kN/m^2 x 0.5 m^2 x 0.01 s = 2500 N s per bond; 9000 over three bonds is 3000 each.
const HIT_IMPULSE: f32 = 9000.0;
const HIT_RADIUS: f32 = 0.5;

/// An edit app with the scene and physics types registered and the editor methods installed.
fn edit_app(project: &std::path::Path) -> App {
    let mut app = App::new();
    app.add_plugins((regia_scene::ScenePlugin, regia::physics::PhysicsPlugin));
    let world = app.world_mut();
    world.init_resource::<regia_editor::command::UndoStack>();
    world.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(project.to_path_buf()),
    });
    regia_editor::agent_ext::install(world);
    app
}

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
    let extension = world.resource::<AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(world, &params)
}

fn reason(e: &RpcError) -> Option<String> {
    e.data
        .as_ref()
        .and_then(|d| d["reason"].as_str())
        .map(str::to_string)
}

/// Cuts and saves the wall with `supports`; returns the saved `.destructible.ron` path.
fn make_wall(world: &mut World, supports: Value) -> String {
    call(
        world,
        "editor.fracture_panel_set",
        json!({ "box": WALL_HALF, "grid": [3, 3, 1], "show_gizmo": true }),
    )
    .expect("set");
    let state = call(world, "editor.fracture_panel_run", json!({ "wait": true })).expect("run");
    assert_eq!(state["result"]["chunks"], 9, "{state}");
    assert_eq!(state["result"]["joints"], 12, "six across, six up: {state}");
    let state = call(
        world,
        "editor.fracture_panel_set",
        json!({ "supports": supports }),
    )
    .expect("supports");
    assert_eq!(state["supports"]["chunks"], json!([0, 1, 2]), "{state}");
    let saved = call(
        world,
        "editor.fracture_panel_save",
        json!({ "out": "assets/wall" }),
    )
    .expect("save");
    saved["written"][1].as_str().expect("path").to_string()
}

/// A game's break model fed the saved file, the panel's pose and seed, and the same hits.
fn runtime_hash(project: &std::path::Path, saved: &str, hits: &[([f32; 3], f32, f32)]) -> (u64, Vec<bool>) {
    let text = std::fs::read_to_string(project.join(saved)).expect("saved asset");
    let asset = Arc::new(DestructibleAsset::from_ron(&text).expect("asset"));
    let mut world = DestructionWorld::new(Authority::Server);
    let id = world
        .define(asset, DestructiblePose::at([0.0; 3]), 0)
        .expect("define");
    for &(center, impulse, radius) in hits {
        world
            .damage(Damage {
                target: Some(id),
                kind: DamageKind::Explosion {
                    center,
                    radius,
                    impulse,
                },
            })
            .expect("damage");
        for _ in 0..regia_editor::fracture_tools::TEST_TICKS {
            world.step(regia_editor::fracture_tools::TEST_DT);
            world.take_spawns(0);
        }
    }
    let broken = world.instance(id).expect("instance").bond_broken().to_vec();
    (world.structure_hash(), broken)
}

#[test]
fn fracture_a_test_hit_on_the_wall_matches_a_runtime_world_bit_for_bit() {
    let dir = regia_test_utils::unique_temp_dir("fracture-tools-wall");
    let mut app = edit_app(&dir);
    let world = app.world_mut();
    let saved = make_wall(world, json!([0, 1, 2]));
    assert_eq!(saved, "assets/wall.destructible.ron");

    let bonds = call(world, "editor.fracture_bonds", json!({ "top": 12 })).expect("bonds");
    assert_eq!(bonds["bonds"], 12);
    let impulses: Vec<f64> = bonds["weakest"]
        .as_array()
        .expect("list")
        .iter()
        .map(|b| b["break_impulse"].as_f64().expect("impulse"))
        .collect();
    assert!(
        impulses.windows(2).all(|w| w[0] <= w[1]),
        "weakest first: {impulses:?}"
    );
    assert!(impulses.iter().all(|i| (i - 2500.0).abs() < 1.0), "{impulses:?}");

    let hit = json!({ "point": TOP_CENTRE_POINT, "impulse": HIT_IMPULSE, "radius": HIT_RADIUS });
    let report = call(world, "editor.fracture_test_hit", hit.clone()).expect("hit");
    // Brick 7 is bonded to 4 below it and 6 and 8 beside it: those three break, nothing else.
    let mut broken: Vec<(u64, u64)> = report["broken_bonds"]
        .as_array()
        .expect("broken")
        .iter()
        .map(|b| (b["a"].as_u64().expect("a"), b["b"].as_u64().expect("b")))
        .collect();
    broken.sort_unstable();
    assert_eq!(broken, vec![(4, 7), (6, 7), (7, 8)], "{report}");
    assert_eq!(report["detached"], json!([TOP_CENTRE]), "{report}");
    assert_eq!(report["collapsed"], json!([]), "{report}");
    assert_eq!(report["falling"], json!([TOP_CENTRE]), "{report}");
    assert_eq!(report["attached"], 8);

    let hits = [(TOP_CENTRE_POINT, HIT_IMPULSE, HIT_RADIUS)];
    let (first, broken_flags) = runtime_hash(&dir, &saved, &hits);
    let (second, _) = runtime_hash(&dir, &saved, &hits);
    assert_eq!(first, second, "the runtime is deterministic");
    assert_eq!(report["structure_hash"], format!("{first:016x}"), "{report}");
    assert_eq!(broken_flags.iter().filter(|b| **b).count(), 3);

    // Reset and hit again: the same hash.
    let reset = call(world, "editor.fracture_test_reset", json!({})).expect("reset");
    assert_eq!(reset["reset"], true);
    let again = call(world, "editor.fracture_test_hit", hit).expect("hit again");
    assert_eq!(again["structure_hash"], report["structure_hash"]);

    // The box rule picks the same bottom row and gives the same structure.
    call(world, "editor.fracture_test_reset", json!({})).expect("reset");
    let state = call(
        world,
        "editor.fracture_panel_set",
        json!({ "supports": { "box": [[-2.0, -2.0, -1.0], [2.0, -1.0, 1.0]] } }),
    )
    .expect("box rule");
    assert_eq!(state["supports"]["chunks"], json!([0, 1, 2]), "{state}");
    let boxed = call(
        world,
        "editor.fracture_test_hit",
        json!({ "point": TOP_CENTRE_POINT, "impulse": HIT_IMPULSE, "radius": HIT_RADIUS }),
    )
    .expect("hit");
    assert_eq!(boxed["structure_hash"], report["structure_hash"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The agent path of the *Plane: chunks below y* checkbox off (the last agent-parity gap,
/// w7-agent-parity-gaps-z follow-up 1): `{ below_y: null }` clears the plane, and `state` reports
/// the plain rule with the default bottom row again.
#[test]
fn a_null_below_y_clears_the_support_plane_over_the_agent_path() {
    let dir = regia_test_utils::unique_temp_dir("fracture-tools-plane-clear");
    let mut app = edit_app(&dir);
    let world = app.world_mut();
    call(
        world,
        "editor.fracture_panel_set",
        json!({ "box": WALL_HALF, "grid": [3, 3, 1], "show_gizmo": true }),
    )
    .expect("set");
    let state = call(world, "editor.fracture_panel_run", json!({ "wait": true })).expect("run");
    assert_eq!(state["result"]["chunks"], 9, "{state}");
    // A plane at y = -0.2 crosses the middle row (y -0.5..0.5), so it holds the bottom two rows.
    let planed = call(
        world,
        "editor.fracture_panel_set",
        json!({ "supports": { "below_y": -0.2 } }),
    )
    .expect("set the plane");
    assert_eq!(
        planed["supports"]["pick"]["below_y"].as_f64(),
        Some(f64::from(-0.2_f32)),
        "{planed}"
    );
    assert_eq!(
        planed["supports"]["chunks"],
        json!([0, 1, 2, 3, 4, 5]),
        "{planed}"
    );
    let cleared = call(
        world,
        "editor.fracture_panel_set",
        json!({ "supports": { "below_y": null } }),
    )
    .expect("clear the plane");
    assert_eq!(cleared["supports"]["pick"], json!("rule"), "{cleared}");
    assert_eq!(
        cleared["supports"]["chunks"],
        json!([0, 1, 2]),
        "the rule picks the bottom row again"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fracture_cutting_the_only_support_loose_drops_the_rest_as_one_falling_set() {
    let dir = regia_test_utils::unique_temp_dir("fracture-tools-collapse");
    let mut app = edit_app(&dir);
    let world = app.world_mut();
    make_wall(world, json!([0, 1, 2]));
    // Only the bottom centre brick holds the wall; a hit on it breaks its three bonds and every
    // other brick loses its path to the ground. A chunk that leaves the structure takes its
    // remaining bonds with it (`sim.rs` `detach`), so all twelve end broken.
    call(world, "editor.fracture_panel_set", json!({ "supports": [1] })).expect("supports");
    let report = call(
        world,
        "editor.fracture_test_hit",
        json!({ "point": [0.0, -1.0, 0.0], "impulse": HIT_IMPULSE, "radius": HIT_RADIUS }),
    )
    .expect("hit");
    assert_eq!(report["bonds_broken"], 12, "{report}");
    assert_eq!(
        report["detached"],
        json!([]),
        "the support itself stays: {report}"
    );
    assert_eq!(report["collapsed"], json!([0, 2, 3, 4, 5, 6, 7, 8]), "{report}");
    assert_eq!(report["falling"], json!([0, 2, 3, 4, 5, 6, 7, 8]), "{report}");
    assert_eq!(report["attached"], 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fracture_place_spawns_one_undo_step_and_the_placed_wall_breaks_the_same_in_a_headless_play() {
    let dir = regia_test_utils::unique_temp_dir("fracture-tools-place");
    let mut app = edit_app(&dir);
    let open = regia_editor::level_io::new_level(app.world_mut());
    let world = app.world_mut();
    let before = regia_scene::level::snapshot_level(world, open.id)
        .expect("snapshot")
        .to_ron_string()
        .expect("text");

    let e = call(world, "editor.fracture_place", json!({})).expect_err("nothing saved");
    assert_eq!(reason(&e).as_deref(), Some("fracture_panel_not_saved"));
    let saved = make_wall(world, json!([0, 1, 2]));
    let placed = call(world, "editor.fracture_place", json!({ "at": [0.0, 0.0, 0.0] })).expect("place");
    assert_eq!(placed["asset"], saved.as_str());
    assert_eq!(placed["name"], "wall");
    let level = regia_scene::level::snapshot_level(world, open.id).expect("snapshot");
    let text = level.to_ron_string().expect("text");
    assert!(
        text.contains("DestructibleSource"),
        "saved with the level: {text}"
    );

    // A headless play of the saved level: the placed wall is defined and breaks like the test.
    let mut play = App::new();
    play.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    play.insert_resource(fixed);
    play.add_plugins((
        regia_scene::ScenePlugin,
        regia::physics::PhysicsPlugin,
        DestructionPlugin::default(),
    ));
    let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
        project_root: dir.clone(),
        platform: regia_asset::current_platform(),
        content_dir: None,
    })
    .expect("asset server");
    play.insert_resource(server);
    let parsed = regia_scene::level::Level::from_ron_str(open.id, &text).expect("level parses");
    regia_scene::prelude::spawn_scene(
        play.world_mut(),
        &parsed.scene,
        &regia_scene::prelude::SpawnOptions::default(),
    )
    .expect("level spawns");
    play.update();
    let status: Vec<DestructibleSourceStatus> = play
        .world_mut()
        .query::<&DestructibleSourceStatus>()
        .iter(play.world())
        .cloned()
        .collect();
    let [DestructibleSourceStatus::Defined(id)] = status.as_slice() else {
        panic!("one placed wall is defined: {status:?}");
    };
    let id = *id;
    // Ticks the play ran before the hit; the reference world runs as many empty ones.
    let ticks_before = play.world().resource::<Destruction>().world.tick();
    play.world_mut()
        .resource_mut::<Destruction>()
        .world
        .damage(Damage {
            target: Some(id),
            kind: DamageKind::Explosion {
                center: TOP_CENTRE_POINT,
                radius: HIT_RADIUS,
                impulse: HIT_IMPULSE,
            },
        })
        .expect("damage");
    let target = ticks_before + u64::from(regia_editor::fracture_tools::TEST_TICKS);
    let mut guard = 0;
    while play.world().resource::<Destruction>().world.tick() < target {
        play.update();
        guard += 1;
        assert!(guard < 100, "the fixed tick runs");
    }
    let d = play.world().resource::<Destruction>();
    let instance = d.world.instance(id).expect("instance");
    let broken: Vec<usize> = instance
        .bond_broken()
        .iter()
        .enumerate()
        .filter(|(_, b)| **b)
        .map(|(i, _)| i)
        .collect();

    let text_asset = std::fs::read_to_string(dir.join(&saved)).expect("saved asset");
    let asset = Arc::new(DestructibleAsset::from_ron(&text_asset).expect("asset"));
    let mut reference = DestructionWorld::new(Authority::Server);
    let rid = reference
        .define(Arc::clone(&asset), DestructiblePose::at([0.0; 3]), 0)
        .expect("define");
    for _ in 0..ticks_before {
        reference.step(1.0 / 60.0);
    }
    reference
        .damage(Damage {
            target: Some(rid),
            kind: DamageKind::Explosion {
                center: TOP_CENTRE_POINT,
                radius: HIT_RADIUS,
                impulse: HIT_IMPULSE,
            },
        })
        .expect("damage");
    for _ in 0..regia_editor::fracture_tools::TEST_TICKS {
        reference.step(1.0 / 60.0);
        reference.take_spawns(0);
    }
    let reference_broken: Vec<usize> = reference
        .instance(rid)
        .expect("instance")
        .bond_broken()
        .iter()
        .enumerate()
        .filter(|(_, b)| **b)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(broken, reference_broken, "the same bonds break");
    assert_eq!(broken.len(), 3);
    assert_eq!(
        d.world.structure_hash(),
        reference.structure_hash(),
        "the placed wall's structure matches"
    );

    // Undo of Place gives back the level's bytes.
    let world = app.world_mut();
    let undone = call(world, "editor.undo", json!({})).expect("undo");
    assert_eq!(undone["undone"], true, "{undone}");
    let after = regia_scene::level::snapshot_level(world, open.id)
        .expect("snapshot")
        .to_ron_string()
        .expect("text");
    assert_eq!(after, before, "undo of Place restores the level bytes");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fracture_bad_tool_parameters_answer_with_stable_reasons() {
    let dir = regia_test_utils::unique_temp_dir("fracture-tools-bad");
    let mut app = edit_app(&dir);
    let world = app.world_mut();
    let e = call(
        world,
        "editor.fracture_test_hit",
        json!({ "point": [0.0, 0.0, 0.0] }),
    )
    .expect_err("no run");
    assert_eq!(reason(&e).as_deref(), Some("fracture_panel_no_result"));
    let e = call(world, "editor.fracture_bonds", json!({})).expect_err("no run");
    assert_eq!(reason(&e).as_deref(), Some("fracture_panel_no_result"));
    for params in [
        json!({ "supports": "some" }),
        json!({ "supports": { "box": [[1.0, 1.0, 1.0], [0.0, 0.0, 0.0]] } }),
        json!({ "click": "paint" }),
        json!({ "hit_radius": 0.0 }),
        json!({ "sites": [[0.0, 0.0]] }),
        json!({ "grid": [3, 3] }),
    ] {
        let e = call(world, "editor.fracture_panel_set", params.clone()).expect_err("refused");
        assert_eq!(e.code, regia_agent::protocol::INVALID_PARAMS, "{params}");
    }
    let e = call(world, "editor.fracture_panel_set", json!({ "supports": [0] })).expect_err("no chunks yet");
    assert_eq!(reason(&e).as_deref(), Some("fracture_panel_bad_chunk"));
    let e = call(world, "editor.fracture_panel_set", json!({ "grid": [0, 3, 1] })).expect_err("empty grid");
    assert_eq!(reason(&e).as_deref(), Some("fracture_panel_bad_grid"));
    call(
        world,
        "editor.fracture_panel_set",
        json!({ "mesh": "assets/m.glb" }),
    )
    .expect("mesh");
    let e = call(world, "editor.fracture_panel_set", json!({ "grid": [3, 3, 1] })).expect_err("mesh");
    assert_eq!(reason(&e).as_deref(), Some("fracture_panel_grid_needs_box"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Impact-biased and clustered site sampling through the panel (ADR-9870): the saved
/// `.fracture.ron` of a biased cut crowds its sites at the hit, a clustered cut keeps its sites in
/// clumps, both are preview choices the `.meta` refuses, and bad parameters answer with stable
/// reasons.
#[test]
fn fracture_impact_and_clustered_sites_cut_fine_debris_and_clumps() {
    let dir = regia_test_utils::unique_temp_dir("fracture-sites");
    let mut app = edit_app(&dir);
    let world = app.world_mut();
    std::fs::create_dir_all(dir.join("assets")).expect("dir");

    // Impact-biased: bias 2 crowds the cut at the centre of the +x face.
    let state = call(
        world,
        "editor.fracture_panel_set",
        json!({ "box": [1.0, 1.0, 1.0], "chunks": 40, "seed": 7,
                "impact": { "at": [1.0, 0.0, 0.0], "bias": 2 } }),
    )
    .expect("set");
    assert_eq!(
        state["impact"],
        json!({ "at": [1.0, 0.0, 0.0], "bias": 2.0 }),
        "{state}"
    );
    assert_eq!(state["sites"], json!(null), "impact replaces explicit sites");
    let state = call(world, "editor.fracture_panel_run", json!({ "wait": true })).expect("run");
    // 40 asked: slivers below min_chunk_volume_fraction may drop (as with the seeded sampler).
    let chunks = state["result"]["chunks"].as_u64().unwrap_or_default();
    assert!((36..=40).contains(&chunks), "{state}");
    call(
        world,
        "editor.fracture_panel_save",
        json!({ "out": "assets/strong" }),
    )
    .expect("save");

    // bias 0 for contrast: the mean site distance to the hit grows.
    call(
        world,
        "editor.fracture_panel_set",
        json!({ "impact": { "bias": 0.0 } }),
    )
    .expect("set");
    call(world, "editor.fracture_panel_run", json!({ "wait": true })).expect("run");
    call(
        world,
        "editor.fracture_panel_save",
        json!({ "out": "assets/even" }),
    )
    .expect("save");

    let sites = |name: &str| -> Vec<[f32; 3]> {
        let text =
            std::fs::read_to_string(dir.join(format!("assets/{name}.fracture.ron"))).expect("saved set");
        regia_asset_pipeline::fracture::FractureSet::from_ron(&text)
            .expect("set")
            .chunks
            .iter()
            .map(|c| c.site)
            .collect()
    };
    let hit = [1.0, 0.0, 0.0];
    let mean_to_hit = |name: &str| {
        let s = sites(name);
        s.iter()
            .map(|p| {
                let d = [
                    f64::from(p[0] - hit[0]),
                    f64::from(p[1] - hit[1]),
                    f64::from(p[2] - hit[2]),
                ];
                (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
            })
            .sum::<f64>()
            / s.len() as f64
    };
    let (strong, even) = (mean_to_hit("strong"), mean_to_hit("even"));
    assert!(
        strong < 0.75 * even,
        "bias 2 crowds the sites at the hit: {strong} vs {even}"
    );

    // Clustered: 4 clumps of 10, sites nearer a neighbour than the even spread's.
    let state = call(
        world,
        "editor.fracture_panel_set",
        json!({ "clustered": { "clusters": 4, "spread": 0.3 } }),
    )
    .expect("set");
    assert_eq!(state["clustered"]["clusters"], json!(4), "{state}");
    // 0.3 f32 round-trips through JSON as 0.30000001192092896: compare in f32.
    assert!(
        state["clustered"]["spread"]
            .as_f64()
            .is_some_and(|s| (s as f32 - 0.3f32).abs() < 1e-6),
        "{state}"
    );
    assert_eq!(state["impact"], json!(null), "clustered replaces impact");
    let state = call(world, "editor.fracture_panel_run", json!({ "wait": true })).expect("run");
    let clumped_chunks = state["result"]["chunks"].as_u64().unwrap_or_default();
    assert!((36..=40).contains(&clumped_chunks), "{state}");
    call(
        world,
        "editor.fracture_panel_save",
        json!({ "out": "assets/clumped" }),
    )
    .expect("save");
    let nn = |name: &str| {
        let s = sites(name);
        s.iter()
            .map(|p| {
                s.iter()
                    .filter(|q| **q != *p)
                    .map(|q| {
                        let d = [
                            f64::from(p[0] - q[0]),
                            f64::from(p[1] - q[1]),
                            f64::from(p[2] - q[2]),
                        ];
                        (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
                    })
                    .fold(f64::INFINITY, f64::min)
            })
            .sum::<f64>()
            / s.len() as f64
    };
    let (clumped, spread) = (nn("clumped"), nn("even"));
    assert!(
        clumped < 0.6 * spread,
        "clumped sites sit nearer a neighbour: {clumped} vs {spread}"
    );

    // The choices are mutually exclusive and clearable.
    let state = call(world, "editor.fracture_panel_set", json!({ "clustered": null })).expect("set");
    assert_eq!(state["clustered"], json!(null), "{state}");
    let state = call(
        world,
        "editor.fracture_panel_set",
        json!({ "impact": { "at": [0.0, 0.0, 0.0], "bias": 1.0 } }),
    )
    .expect("set");
    assert!(state["impact"].is_object(), "{state}");
    let state = call(
        world,
        "editor.fracture_panel_set",
        json!({ "sites": [[0.0, 0.0, 0.0]] }),
    )
    .expect("set");
    assert_eq!(state["impact"], json!(null), "explicit sites clear impact");
    call(
        world,
        "editor.fracture_panel_set",
        json!({ "impact": { "bias": 1.0 } }),
    )
    .expect("set");
    let state = call(world, "editor.fracture_panel_set", json!({ "grid": [2, 2, 1] })).expect("set");
    assert_eq!(state["sites"], json!(4), "{state}");
    assert_eq!(state["impact"], json!(null), "grid clears impact");

    // The .meta does not store site choices: Apply to .meta refuses them.
    call(
        world,
        "editor.fracture_panel_set",
        json!({ "mesh": "assets/m.glb", "impact": { "bias": 1.0 } }),
    )
    .expect("set");
    let e = call(world, "editor.fracture_panel_apply_meta", json!({})).expect_err("refused");
    assert_eq!(reason(&e).as_deref(), Some("fracture_panel_meta_sites"));

    // Bad parameters answer with stable reasons.
    for params in [
        json!({ "impact": { "at": [1.0] } }),
        json!({ "impact": 3 }),
        json!({ "impact": { "at": [0.0, 0.0, 0.0], "bias": "x" } }),
        json!({ "impact": { "at": [1.0, 0.0, f64::NAN] } }),
        json!({ "clustered": { "clusters": "x" } }),
        json!({ "clustered": "x" }),
    ] {
        let e = call(world, "editor.fracture_panel_set", params.clone()).expect_err("refused");
        assert_eq!(e.code, regia_agent::protocol::INVALID_PARAMS, "{params}");
    }
    for params in [
        json!({ "impact": { "bias": 1.5 } }),
        json!({ "impact": { "bias": -1.0 } }),
        json!({ "clustered": { "clusters": 0, "spread": 0.3 } }),
        json!({ "clustered": { "clusters": 4, "spread": 0.0 } }),
        json!({ "clustered": { "clusters": 4, "spread": 2.0 } }),
    ] {
        let e = call(world, "editor.fracture_panel_set", params.clone()).expect_err("refused");
        assert!(
            e.data.as_ref().is_some_and(|d| d["reason"].is_string()),
            "a stable reason for {params}: {e:?}"
        );
    }
    // More clusters than chunks is refused.
    let e = call(
        world,
        "editor.fracture_panel_set",
        json!({ "box": [1.0, 1.0, 1.0], "chunks": 8, "clustered": { "clusters": 9, "spread": 0.3 } }),
    )
    .expect_err("refused");
    assert_eq!(
        reason(&e).as_deref(),
        Some("fracture_panel_bad_clusters"),
        "{e:?}"
    );
    let e = call(
        world,
        "editor.fracture_panel_set",
        json!({ "impact": { "bias": 4.0 } }),
    )
    .expect_err("refused");
    assert_eq!(reason(&e).as_deref(), Some("fracture_panel_bad_impact"), "{e:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
