// SPDX-License-Identifier: MPL-2.0
//! Spec 78 § 18 test 7 (the physics guard, moving members) and the physics half of test 9, with the
//! real physics plugin. Lockstep, fixed steps, no wall-clock.

mod common;

use common::*;
use regia_core::math::Vec3;
use regia_physics::{Collider, ColliderShape, PhysicsPlugin, RigidBody, Teleport, Velocity};
use regia_scene::Transform;
use regia_scene::level::LevelMember;
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::{
    CellMember, Readiness, StreamAutoMember, StreamFrozen, StreamGates, StreamGuarded, StreamingSource,
};
use regia_world::{CellState, WorldOrigin};

fn c(x: i64, z: i64) -> CellCoord {
    CellCoord::new(x, z)
}

fn physics_app() -> regia_app::App {
    let mut app = app();
    app.add_plugins(PhysicsPlugin);
    app
}

fn spawn_body(app: &mut regia_app::App, x: f32, y: f32, z: f32) -> regia_ecs::prelude::Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(x, y, z)),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Sphere { radius: 0.5 },
                ..Collider::default()
            },
        ))
        .id()
}

fn y_of(app: &regia_app::App, e: regia_ecs::prelude::Entity) -> f32 {
    app.world().get::<Transform>(e).expect("body").translation.y
}

#[test]
fn a_kinematic_body_is_never_guarded() {
    let mut app = physics_app();
    let world = build(&app, 2, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    let moving = spawn_body(&mut app, 150.0, 5.0, 50.0);
    let platform = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(150.0, 5.0, 50.0)),
            RigidBody::kinematic(),
        ))
        .id();
    steps(&mut app, 2);
    assert!(app.world().get::<StreamGuarded>(moving).is_some());
    assert!(app.world().get::<StreamGuarded>(platform).is_none());
    assert!(app.world().get::<StreamFrozen>(platform).is_none());
}

#[test]
fn a_body_at_a_cell_edge_stays_frozen_until_its_neighbor_is_ready_and_then_falls_with_its_velocity() {
    let mut app = physics_app();
    // Cell (1, 0) is out of the source's reach, and a body 2 m from its edge is inside the 4 m margin.
    let world = build(&app, 2, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 2);
    let source = spawn_source(&mut app, 50.0, 50.0);
    let body = spawn_body(&mut app, 98.0, 100.0, 50.0);
    app.world_mut().entity_mut(body).insert(Velocity {
        linear: Vec3::new(0.0, -3.0, 0.0),
        ..Velocity::default()
    });
    let start = y_of(&app, body);
    steps(&mut app, 8);
    assert_eq!(
        state(&app, c(0, 0)),
        Some(CellState::Visible),
        "the body's own cell is ready"
    );
    assert!(
        app.world().get::<StreamFrozen>(body).is_some(),
        "the neighbor is not"
    );
    assert_eq!(y_of(&app, body), start, "a frozen body does not integrate");
    assert!(stats(&app).frozen_bodies >= 1);

    move_to(&mut app, source, 95.0, 50.0);
    steps(&mut app, 12);
    assert_eq!(state(&app, c(1, 0)), Some(CellState::Visible));
    assert!(app.world().get::<StreamFrozen>(body).is_none());
    let fallen = start - y_of(&app, body);
    // Falling with the kept 3 m/s from the first step on gets it further than gravity alone.
    assert!(fallen > 0.0, "{fallen}");
    assert_eq!(stats(&app).frozen_bodies, 0);
}

#[test]
fn a_source_frozen_on_an_unready_cell_is_counted() {
    let mut app = physics_app();
    let world = build(&app, 1, 1, 1, 1, settings(40.0, 60.0));
    world.source.hold(cell_id(c(0, 0)));
    open(&mut app, &world, 1000);
    let source = spawn_body(&mut app, 50.0, 10.0, 50.0);
    app.world_mut()
        .entity_mut(source)
        .insert(StreamingSource::default());
    steps(&mut app, 4);
    assert!(stats(&app).source_frozen_steps >= 2, "{:?}", stats(&app));
}

#[test]
fn a_member_moving_into_a_spawned_cell_changes_membership() {
    let mut app = physics_app();
    let world = build(&app, 2, 1, 1, 1, settings(250.0, 300.0));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 100.0, 50.0);
    steps(&mut app, 6);
    assert_eq!(state(&app, c(1, 0)), Some(CellState::Visible));

    // A body that belongs to cell 0 crosses into cell 1.
    let body = spawn_body(&mut app, 95.0, 100.0, 50.0);
    app.world_mut().entity_mut(body).insert(CellMember(c(0, 0)));
    app.world_mut()
        .get_mut::<Transform>(body)
        .expect("t")
        .translation
        .x = 150.0;
    steps(&mut app, 2);
    assert_eq!(app.world().get::<CellMember>(body).map(|m| m.0), Some(c(1, 0)));
}

#[test]
fn a_member_moving_into_a_cell_that_is_not_spawned_keeps_its_cell_and_freezes() {
    let mut app = physics_app();
    let world = build(&app, 2, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 6);
    assert_eq!(state(&app, c(1, 0)), Some(CellState::Unloaded));
    let body = spawn_body(&mut app, 40.0, 100.0, 50.0);
    app.world_mut().entity_mut(body).insert(CellMember(c(0, 0)));
    steps(&mut app, 2);
    assert!(app.world().get::<StreamFrozen>(body).is_none());
    // Moved through physics, so the next step does not write the old pose back.
    app.world_mut()
        .get_mut::<Transform>(body)
        .expect("t")
        .translation
        .x = 150.0;
    app.world_mut().entity_mut(body).insert(Teleport);
    steps(&mut app, 3);
    assert_eq!(app.world().get::<CellMember>(body).map(|m| m.0), Some(c(0, 0)));
    assert!(app.world().get::<StreamFrozen>(body).is_some());
}

#[test]
fn an_auto_member_joins_the_cell_it_stands_in_and_unloads_with_it() {
    let mut app = physics_app();
    let world = build(&app, 3, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    let source = spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 6);
    let auto = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(50.0, 0.0, 50.0)),
            StreamAutoMember,
        ))
        .id();
    let plain = app
        .world_mut()
        .spawn(Transform::from_translation(Vec3::new(51.0, 0.0, 50.0)))
        .id();
    steps(&mut app, 2);
    assert_eq!(app.world().get::<CellMember>(auto).map(|m| m.0), Some(c(0, 0)));
    assert!(app.world().get::<CellMember>(plain).is_none());
    move_to(&mut app, source, 5000.0, 50.0);
    steps(&mut app, 6);
    assert!(app.world().get_entity(auto).is_err(), "unloaded with its cell");
    assert!(
        app.world().get_entity(plain).is_ok(),
        "streaming never despawns a stranger"
    );
}

/// Review finding: a body with no cell must not be adopted by the cell it stands in.
#[test]
fn a_body_that_belongs_to_no_cell_survives_every_unload() {
    let mut app = physics_app();
    let world = build(&app, 3, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    let source = spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 6);
    let vehicle = spawn_body(&mut app, 50.0, 100.0, 50.0);
    steps(&mut app, 3);
    assert!(app.world().get::<CellMember>(vehicle).is_none());
    move_to(&mut app, source, 5000.0, 50.0);
    steps(&mut app, 6);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Unloaded));
    assert!(app.world().get_entity(vehicle).is_ok());
}

/// Review finding: a terrain gate must not outlive the level that needed it.
#[test]
fn a_level_without_terrain_does_not_wait_for_a_terrain_gate_left_by_another() {
    let mut app = physics_app();
    let terrain_level = regia_scene::level::LevelId(regia_core::id::Guid::from_u128(0xbeef));
    {
        let mut gates = app.world_mut().resource_mut::<StreamGates>();
        let gate = gates.register_for_claimed_levels("terrain_collision", Readiness::SimReady);
        gates.claim_level(gate, terrain_level);
    }
    let world = build(&app, 1, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 6);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Visible));
}

#[test]
fn closing_the_level_wakes_every_body() {
    let mut app = physics_app();
    let world = build(&app, 1, 1, 1, 1, settings(40.0, 60.0));
    world.source.hold(cell_id(c(0, 0)));
    open(&mut app, &world, 1000);
    let body = spawn_body(&mut app, 50.0, 10.0, 50.0);
    steps(&mut app, 3);
    assert!(app.world().get::<StreamFrozen>(body).is_some());
    regia_scene::level::unload_level(app.world_mut(), level()).expect("unload");
    steps(&mut app, 3);
    assert!(app.world().get::<StreamFrozen>(body).is_none());
}

/// Spec 78 § 18 test 14.
#[test]
fn a_body_on_a_terrain_only_cell_wakes_once_terrain_collision_is_ready() {
    let mut app = physics_app();
    let world = build_with(&app, 3, 1, settings(40.0, 60.0), |cell| {
        // Cell (1, 0) has no record: only terrain there.
        if cell == c(1, 0) { (0, 0) } else { (1, 1) }
    });
    open(&mut app, &world, 1);
    spawn_source(&mut app, 150.0, 50.0);
    let body = spawn_body(&mut app, 150.0, 100.0, 50.0);
    // No gate yet: an empty cell has nothing to wait for.
    steps(&mut app, 2);
    assert!(app.world().get::<StreamFrozen>(body).is_none());
    // A gate that covers empty cells holds the cell back until it answers.
    let gate = app
        .world_mut()
        .resource_mut::<StreamGates>()
        .register_covering_empty("terrain_collision", Readiness::SimReady);
    steps(&mut app, 2);
    assert!(app.world().get::<StreamFrozen>(body).is_some());
    // The terrain collider of the level opens the gate.
    app.world_mut().spawn((
        Transform::default(),
        LevelMember::from_level(level()),
        Collider {
            shape: ColliderShape::HeightField {
                nx: 2,
                nz: 2,
                heights: vec![0.0; 4],
                scale: Vec3::new(1000.0, 1.0, 1000.0),
            },
            ..Collider::default()
        },
    ));
    steps(&mut app, 2);
    assert!(app.world().get::<StreamFrozen>(body).is_none());
    let gates = app.world().resource::<StreamGates>();
    assert!(gates.is_empty_cell_ready(level(), c(1, 0), Readiness::SimReady));
    let _ = gate;
}

#[test]
fn a_rebase_keeps_a_body_where_the_world_says_it_is() {
    let mut app = physics_app();
    let mut s = settings(250.0, 300.0);
    s.origin.threshold_m = 512.0;
    // Far content is simply absent here: an always-loaded body tracked through a rebase.
    let world = build_with(&app, 1, 1, s, |_| (0, 0));
    open(&mut app, &world, 1);
    let source = spawn_source(&mut app, 0.0, 0.0);
    app.world_mut()
        .get_mut::<StreamingSource>(source)
        .expect("s")
        .drives_origin = true;
    let body = spawn_body(&mut app, 3000.0, 100.0, 7.0);
    steps(&mut app, 3);
    let before = app.world().get::<Transform>(body).expect("t").translation;
    move_to(&mut app, source, 3000.0, 0.0);
    steps(&mut app, 1);
    let now = stats(&app);
    assert_eq!(now.rebases, 1);
    let origin = *app.world().resource::<WorldOrigin>();
    assert_eq!(origin.0.cell(), [3, 0, 0]);
    let after = app.world().get::<Transform>(body).expect("t").translation;
    assert_eq!(after.x, before.x - 3072.0);
    // Falls on: one more step keeps the same fall, no jump in world space.
    let world_x = origin.to_world(after.to_array())[0];
    assert!((world_x - 3000.0).abs() < 1.0e-3, "{world_x}");
    steps(&mut app, 3);
    let later = app.world().get::<Transform>(body).expect("t").translation;
    assert!((later.x - after.x).abs() < 1.0e-3, "{} {}", later.x, after.x);
    assert!(later.y < after.y, "the body keeps falling across the rebase");
}

/// A cell that spawns while a proxy covers it hides its members as they arrive: drawing both would
/// double the content.
#[test]
fn members_of_a_cell_spawned_behind_its_proxy_are_hidden() {
    use regia_scene::hlod::{CellProxy, HlodConfig};
    use regia_scene::streaming::{CellRenderHidden, HlodProxyResidency};
    use regia_world::format::HlodIndex;

    let mut app = physics_app();
    let mut world = build(&app, 2, 1, 4, 1, settings(250.0, 300.0));
    let proxies: Vec<CellProxy> = (0..2)
        .map(|x| CellProxy {
            cell: c(x, 0),
            min_y: 0.0,
            max_y: 1.0,
            bytes: 10,
        })
        .collect();
    // A threshold that makes every node a proxy from any distance.
    let config = HlodConfig {
        screen_size_threshold: 1.0,
        ..HlodConfig::default()
    };
    let grid = world.index.settings.grid().expect("grid");
    let index = HlodIndex {
        config,
        proxies,
        node_proxies: Vec::new(),
        node_anchors: Vec::new(),
        node_colors: Vec::new(),
        node_atlases: Vec::new(),
    };
    let nodes = index.tree(&grid).expect("tree").nodes().len();
    world.index.hlod = Some(HlodIndex {
        node_proxies: vec![Some(cell_id(c(0, 0))); nodes],
        node_anchors: vec![[0; 3]; nodes],
        node_colors: vec![Some([0.5, 0.25, 0.125, 1.0]); nodes],
        // Node 0 has a baked atlas (ADR-2483); the others draw flat.
        node_atlases: vec![Some(regia_world::format::HlodAtlas {
            material: atlas_material(),
            textures: [atlas_material(); 3],
            size: [64, 64],
            texels_per_m: 2.0,
            asked_texels_per_m: 2.0,
            capped: false,
            bake_error_m: 0.0,
            miss_ratio: 0.0,
        })],
        ..index
    });
    open(&mut app, &world, 1);
    app.world_mut()
        .resource_mut::<HlodProxyResidency>()
        .resident
        .extend(0..nodes as u32);
    // Far enough for the leaves to read as proxies, near enough to want the cells.
    spawn_source(&mut app, 100.0, 245.0);
    steps(&mut app, 8);
    let hidden = app
        .world_mut()
        .query_filtered::<&regia_scene::streaming::CellMember, regia_ecs::prelude::With<CellRenderHidden>>()
        .iter(app.world())
        .count();
    assert_eq!(count_members(&mut app), 8);
    assert_eq!(hidden, 8, "every member behind a proxy");
    assert_eq!(stats(&app).holes, 0);
    // Every proxy entity carries its node's cooked color for the renderer (ADR-2022).
    let colors: Vec<_> = app
        .world_mut()
        .query::<&regia_scene::streaming::HlodProxyMesh>()
        .iter(app.world())
        .map(|proxy| proxy.color)
        .collect();
    assert!(!colors.is_empty());
    assert!(
        colors.iter().all(|c| *c == Some([0.5, 0.25, 0.125, 1.0])),
        "{colors:?}"
    );
    // The atlas material goes with its node only; a node past the atlas list draws flat.
    let materials: Vec<_> = app
        .world_mut()
        .query::<&regia_scene::streaming::HlodProxyMesh>()
        .iter(app.world())
        .map(|proxy| (proxy.node, proxy.material))
        .collect();
    for (node, material) in &materials {
        let want = (*node == 0).then_some(atlas_material().0);
        assert_eq!(*material, want, "node {node}: {materials:?}");
    }
}

/// The atlas material id of the guard tests (ADR-2483).
fn atlas_material() -> regia_asset::AssetId {
    regia_asset::AssetId(regia_core::id::Guid::from_u128(0xa71a5))
}

/// `StreamStats.holes` is a gauge of the last cut: holes from an earlier frame do not stay in it
/// once proxies are resident and the cut is covered.
#[test]
fn the_holes_stat_returns_to_zero_when_the_cut_is_covered() {
    use regia_scene::hlod::{CellProxy, HlodConfig};
    use regia_scene::streaming::HlodProxyResidency;
    use regia_world::format::HlodIndex;

    let mut app = physics_app();
    let mut world = build(&app, 2, 1, 4, 1, settings(250.0, 300.0));
    let proxies: Vec<CellProxy> = (0..2)
        .map(|x| CellProxy {
            cell: c(x, 0),
            min_y: 0.0,
            max_y: 1.0,
            bytes: 10,
        })
        .collect();
    let config = HlodConfig {
        screen_size_threshold: 1.0,
        ..HlodConfig::default()
    };
    let grid = world.index.settings.grid().expect("grid");
    let index = HlodIndex {
        config,
        proxies,
        node_proxies: Vec::new(),
        node_anchors: Vec::new(),
        node_colors: Vec::new(),
        node_atlases: Vec::new(),
    };
    let nodes = index.tree(&grid).expect("tree").nodes().len();
    world.index.hlod = Some(HlodIndex {
        node_proxies: vec![Some(cell_id(c(0, 0))); nodes],
        node_anchors: vec![[0; 3]; nodes],
        node_colors: vec![None; nodes],
        ..index
    });
    open(&mut app, &world, 1);
    app.world_mut()
        .resource_mut::<HlodProxyResidency>()
        .resident
        .extend(0..nodes as u32);
    spawn_source(&mut app, 100.0, 245.0);
    steps(&mut app, 8);
    assert_eq!(stats(&app).holes, 0);
    // Proxies leave residency: the cut can only fall back, and nothing covers the far leaves.
    app.world_mut()
        .resource_mut::<HlodProxyResidency>()
        .resident
        .clear();
    steps(&mut app, 2);
    let during = stats(&app).holes;
    // Proxies come back: the gauge follows the cut, whatever the earlier frames counted.
    app.world_mut()
        .resource_mut::<HlodProxyResidency>()
        .resident
        .extend(0..nodes as u32);
    steps(&mut app, 4);
    assert_eq!(
        stats(&app).holes,
        0,
        "holes counted while uncovered ({during}) must not stay"
    );
}

/// `StreamStats.holes` stays a gauge on a steady camera: once the uncovered cut is cached (the
/// instant-fade fast path), more frames of the same cut report the same count instead of adding
/// it again every frame (corr-input-world, a `+=` on the cached path).
#[test]
fn the_holes_stat_does_not_grow_while_the_uncovered_cut_is_steady() {
    use regia_scene::hlod::{CellProxy, HlodConfig};
    use regia_scene::streaming::HlodProxyResidency;
    use regia_world::format::HlodIndex;

    let mut app = physics_app();
    // Eight cells in a row: the ones past the load radius never load, so with no proxy resident
    // their leaves are holes.
    let mut world = build(&app, 8, 1, 4, 1, settings(250.0, 300.0));
    let proxies: Vec<CellProxy> = (0..8)
        .map(|x| CellProxy {
            cell: c(x, 0),
            min_y: 0.0,
            max_y: 1.0,
            bytes: 10,
        })
        .collect();
    let config = HlodConfig {
        screen_size_threshold: 1.0,
        ..HlodConfig::default()
    };
    let grid = world.index.settings.grid().expect("grid");
    let index = HlodIndex {
        config,
        proxies,
        node_proxies: Vec::new(),
        node_anchors: Vec::new(),
        node_colors: Vec::new(),
        node_atlases: Vec::new(),
    };
    let nodes = index.tree(&grid).expect("tree").nodes().len();
    world.index.hlod = Some(HlodIndex {
        node_proxies: vec![Some(cell_id(c(0, 0))); nodes],
        node_anchors: vec![[0; 3]; nodes],
        node_colors: vec![None; nodes],
        ..index
    });
    open(&mut app, &world, 1);
    app.world_mut()
        .resource_mut::<HlodProxyResidency>()
        .resident
        .extend(0..nodes as u32);
    spawn_source(&mut app, 100.0, 245.0);
    steps(&mut app, 8);
    // No proxy is resident: the cut cannot cover the far leaves, and nothing changes after.
    app.world_mut()
        .resource_mut::<HlodProxyResidency>()
        .resident
        .clear();
    steps(&mut app, 2);
    let first = stats(&app).holes;
    assert!(first > 0, "the uncovered cut has holes");
    steps(&mut app, 5);
    assert_eq!(
        stats(&app).holes,
        first,
        "five more frames of the same cut report the same gauge"
    );
}

/// ADR-2022, the opt-in cross-fade: when the cut switches from two leaf proxies to their parent,
/// the parent dithers in while the leaves keep drawing and dither out, for `hlod_fade_frames`
/// frames; then the leaves hide and every dither comes off. The cut's `fade` names the incoming
/// share.
#[test]
fn an_hlod_switch_cross_fades_for_the_authored_frames() {
    use regia_scene::hlod::{CellProxy, HlodConfig};
    use regia_scene::streaming::{CellRenderHidden, HlodCut, HlodFade, HlodProxyMesh, HlodProxyResidency};
    use regia_world::format::HlodIndex;

    let mut app = physics_app();
    let mut s = settings(250.0, 300.0);
    s.tuning.hlod_fade_frames = Some(4);
    let mut world = build(&app, 2, 1, 4, 1, s);
    let proxies: Vec<CellProxy> = (0..2)
        .map(|x| CellProxy {
            cell: c(x, 0),
            min_y: 0.0,
            max_y: 1.0,
            bytes: 10,
        })
        .collect();
    let config = HlodConfig {
        screen_size_threshold: 1.0,
        ..HlodConfig::default()
    };
    let grid = world.index.settings.grid().expect("grid");
    let index = HlodIndex {
        config,
        proxies,
        node_proxies: Vec::new(),
        node_anchors: Vec::new(),
        node_colors: Vec::new(),
        node_atlases: Vec::new(),
    };
    let tree = index.tree(&grid).expect("tree");
    let nodes = tree.nodes().len();
    let root = tree.roots()[0];
    let leaves: Vec<u32> = (0..nodes as u32).filter(|n| *n != root).collect();
    assert_eq!(leaves.len(), 2);
    world.index.hlod = Some(HlodIndex {
        node_proxies: vec![Some(cell_id(c(0, 0))); nodes],
        node_anchors: vec![[0; 3]; nodes],
        node_colors: vec![None; nodes],
        ..index
    });
    open(&mut app, &world, 1);
    // Only the leaves are resident: the cut opens the root into them.
    app.world_mut()
        .resource_mut::<HlodProxyResidency>()
        .resident
        .extend(leaves.iter().copied());
    // Far from both cells: nothing loads, the proxies draw.
    spawn_source(&mut app, 100.0, 2000.0);
    steps(&mut app, 4);
    let drawn = |app: &regia_app::App| -> Vec<u32> {
        let mut nodes: Vec<u32> = app
            .world()
            .resource::<HlodCut>()
            .entries
            .iter()
            .map(|e| e.node)
            .collect();
        nodes.sort_unstable();
        nodes
    };
    assert_eq!(drawn(&app), leaves);
    // (node, fade, hidden) of every proxy entity.
    let proxies_now = |app: &mut regia_app::App| -> Vec<(u32, Option<f32>, bool)> {
        let mut rows: Vec<(u32, Option<f32>, bool)> = app
            .world_mut()
            .query::<(&HlodProxyMesh, Option<&HlodFade>, Option<&CellRenderHidden>)>()
            .iter(app.world())
            .map(|(p, f, h)| (p.node, f.map(|f| f.0), h.is_some()))
            .collect();
        rows.sort_by_key(|r| r.0);
        rows
    };
    assert!(
        proxies_now(&mut app).iter().all(|(_, fade, _)| fade.is_none()),
        "settled"
    );

    // The root becomes resident: the cut switches to it.
    app.world_mut()
        .resource_mut::<HlodProxyResidency>()
        .resident
        .insert(root);
    for frame in 1..=3u32 {
        app.update();
        assert_eq!(drawn(&app), vec![root]);
        let c = frame as f32 / 4.0;
        let cut = app.world().resource::<HlodCut>().clone();
        assert_eq!(cut.entries[0].fade, c, "frame {frame}");
        let mut leaving: Vec<(u32, f32)> = cut.leaving.iter().map(|e| (e.node, e.fade)).collect();
        leaving.sort_by_key(|l| l.0);
        assert_eq!(leaving, leaves.iter().map(|n| (*n, 1.0 - c)).collect::<Vec<_>>());
        for (node, fade, hidden) in proxies_now(&mut app) {
            let want = if node == root { c } else { -c };
            assert_eq!(fade, Some(want), "node {node} frame {frame}");
            assert!(!hidden, "node {node} draws through the fade");
        }
    }
    app.update();
    let cut = app.world().resource::<HlodCut>().clone();
    assert_eq!(cut.entries[0].fade, 0.0);
    assert!(cut.leaving.is_empty());
    for (node, fade, hidden) in proxies_now(&mut app) {
        assert_eq!(fade, None, "node {node}: settled");
        assert_eq!(hidden, node != root, "node {node}");
    }
}

/// Spec 78 § 18 test 9, the parts in this crate's reach: a 20 km walk across many rebases. Static
/// markers near the walker stay within 1 mm of the truth, a moving body crosses every rebase with no
/// jump in world space, and a nav mesh and an agent's plan near the start keep their world positions
/// (ADR-2022). Audio, decals, warp targets, effects and the GPU particle pool have their own tests.
#[test]
fn a_20_km_walk_keeps_positions_within_a_millimeter_across_every_rebase() {
    let mut app = physics_app();
    let mut s = settings(250.0, 300.0);
    s.origin.threshold_m = 1536.0;
    let world = build_with(&app, 1, 1, s, |_| (0, 0));
    open(&mut app, &world, 1);
    let source = spawn_source(&mut app, 0.0, 0.0);
    app.world_mut()
        .get_mut::<StreamingSource>(source)
        .expect("s")
        .drives_origin = true;

    const SPEED: f32 = 30.0;
    let body = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 10.0, 3.0)),
            RigidBody {
                gravity_scale: 0.0,
                can_sleep: false,
                ..RigidBody::dynamic()
            },
            Velocity {
                linear: Vec3::new(SPEED, 0.0, 0.0),
                ..Velocity::default()
            },
        ))
        .id();
    let mut markers: Vec<(regia_ecs::prelude::Entity, f64)> = Vec::new();
    let origin_of = |app: &regia_app::App| *app.world().resource::<WorldOrigin>();

    // Nav near the start: a floor around (600, 0, 0) and an agent's plan across it.
    app.add_plugins(regia_nav::NavPlugin);
    let nav_center = Vec3::new(600.0, 0.0, 0.0);
    let corner = |x: f32, z: f32| nav_center + Vec3::new(x, 0.0, z);
    let mut nav_params = regia_nav::NavMeshBuildParams {
        cell_size: 0.5,
        ..Default::default()
    };
    nav_params.agent.radius = 0.0;
    let nav_mesh = regia_nav::NavMeshBuilder::new(nav_params)
        .build(&[
            [corner(-10.0, -10.0), corner(10.0, -10.0), corner(10.0, 10.0)],
            [corner(-10.0, -10.0), corner(10.0, 10.0), corner(-10.0, 10.0)],
        ])
        .expect("nav mesh");
    let nav_truth = (nav_mesh.min.as_dvec3(), nav_mesh.max.as_dvec3());
    app.insert_resource(nav_mesh);
    let plan_truth = [
        nav_center + Vec3::new(-5.0, 0.0, 1.0),
        nav_center + Vec3::new(5.0, 0.0, 1.0),
    ];
    let agent = app
        .world_mut()
        .spawn(regia_nav::NavAgentState {
            target: Some(plan_truth[1]),
            waypoints: plan_truth.to_vec(),
            ..Default::default()
        })
        .id();
    let near = |origin: &WorldOrigin, local: Vec3, truth: regia_core::glam::DVec3| {
        let world = origin.to_world(local.to_array());
        (0..3).all(|a| (world[a] - truth[a]).abs() < 1.0e-3)
    };

    let dt = 1.0 / 60.0;
    let mut walked = 0.0f64;
    let mut last_body_x = 0.0f64;
    let mut rebases = 0;
    let mut steps_done = 0u32;
    let mut nav_checks = 0u32;
    while walked < 20_000.0 {
        walked += f64::from(SPEED) * dt;
        let local = origin_of(&app).from_world([walked, 0.0, 0.0]).expect("local");
        app.world_mut()
            .get_mut::<Transform>(source)
            .expect("source")
            .translation = Vec3::from_array(local);
        app.update();
        steps_done += 1;
        // A marker every 500 m, placed in local space like any spawned entity.
        if steps_done.is_multiple_of(1000) {
            let at = origin_of(&app).from_world([walked, 0.0, 1.0]).expect("local");
            let marker = app
                .world_mut()
                .spawn(Transform::from_translation(Vec3::from_array(at)))
                .id();
            markers.push((marker, walked));
        }

        let origin = origin_of(&app);
        let now = stats(&app).rebases;
        let body_local = app.world().get::<Transform>(body).expect("body").translation;
        let body_x = origin.to_world(body_local.to_array())[0];
        // The first steps create the body in rapier; it moves from then on.
        if steps_done > 4 {
            let moved = body_x - last_body_x;
            let expected = f64::from(SPEED) * dt;
            assert!(
                (moved - expected).abs() < 1.0e-3,
                "step {steps_done}: the body moved {moved} m, expected {expected} m"
            );
        }
        last_body_x = body_x;
        if now != rebases {
            rebases = now;
            // Everything near the focus still sits where the world says.
            for (marker, truth) in &markers {
                if (truth - walked).abs() > 3000.0 {
                    continue;
                }
                let t = app.world().get::<Transform>(*marker).expect("marker").translation;
                let x = origin.to_world(t.to_array())[0];
                assert!((x - truth).abs() < 1.0e-3, "marker at {truth}: {x}");
            }
            // Nav moved with the origin, checked while it is near enough for f32 to hold 1 mm.
            if walked < 3000.0 {
                let mesh = app.world().resource::<regia_nav::NavMesh>();
                assert!(
                    near(&origin, mesh.min, nav_truth.0),
                    "nav mesh min after {walked} m"
                );
                assert!(
                    near(&origin, mesh.max, nav_truth.1),
                    "nav mesh max after {walked} m"
                );
                let plan = app.world().get::<regia_nav::NavAgentState>(agent).expect("agent");
                for (local, truth) in plan.waypoints.iter().zip(plan_truth) {
                    assert!(
                        near(&origin, *local, truth.as_dvec3()),
                        "waypoint after {walked} m"
                    );
                }
                assert!(near(
                    &origin,
                    plan.target.expect("target"),
                    plan_truth[1].as_dvec3()
                ));
                nav_checks += 1;
            }
        }
    }
    // About one rebase per 1.5 km: the threshold, less what the last rebase left over.
    assert!(rebases >= 10, "{rebases} rebases over 20 km");
    assert!(nav_checks >= 1, "nav was checked after a rebase");
}

/// Without a streamed level nothing is tagged, so an app that never streams keeps its components and
/// archetypes (the determinism matrix pins them). A level that opens tags the bodies already there.
#[test]
fn bodies_are_guarded_only_while_a_streamed_level_is_open() {
    let mut app = physics_app();
    let early = spawn_body(&mut app, 150.0, 5.0, 50.0);
    steps(&mut app, 3);
    assert!(
        app.world().get::<StreamGuarded>(early).is_none(),
        "no streamed level: untouched"
    );
    assert!(
        !app.world()
            .resource::<regia_scene::streaming::StreamingActive>()
            .0
    );

    let world = build(&app, 2, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    assert!(
        app.world()
            .resource::<regia_scene::streaming::StreamingActive>()
            .0
    );
    steps(&mut app, 2);
    assert!(
        app.world().get::<StreamGuarded>(early).is_some(),
        "a body spawned before the open is tagged by the sweep"
    );

    regia_scene::level::unload_level(app.world_mut(), level()).expect("unload");
    steps(&mut app, 1);
    assert!(
        !app.world()
            .resource::<regia_scene::streaming::StreamingActive>()
            .0
    );
}

/// Audit v033b (High): with two levels open, a member moving to another cell was always moved
/// inside the first level (lowest `LevelId`): its own level kept listing it in the old cell, and
/// the other level listed it too. It moves inside the level that lists it.
#[test]
fn a_member_moves_inside_its_own_level_when_two_levels_are_open() {
    use regia_scene::level::LevelId;
    let mut app = app();
    let world = build(&app, 2, 1, 1, 1, settings(250.0, 300.0));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 100.0, 50.0);
    steps(&mut app, 6);
    let member = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(50.0, 0.0, 50.0)),
            StreamAutoMember,
        ))
        .id();
    steps(&mut app, 2);
    assert_eq!(app.world().get::<CellMember>(member).map(|m| m.0), Some(c(0, 0)));
    fn streaming(app: &regia_app::App) -> &regia_world::WorldStreaming {
        app.world().resource::<regia_world::WorldStreaming>()
    }
    assert!(streaming(&app).entities(level(), c(0, 0)).contains(&member));

    // A second level that sorts first, with both of its cells spawned too.
    let other = LevelId(regia_core::id::Guid::from_u128(1));
    assert!(other < level());
    open_additive(
        &mut app,
        other,
        &build_empty_level(other, 2, 1, settings(250.0, 300.0)),
    );
    steps(&mut app, 6);
    assert!(
        streaming(&app)
            .state(other, c(1, 0))
            .is_some_and(|s| s.is_spawned())
    );

    move_to(&mut app, member, 150.0, 50.0);
    steps(&mut app, 2);
    assert_eq!(app.world().get::<CellMember>(member).map(|m| m.0), Some(c(1, 0)));
    let ws = streaming(&app);
    assert!(
        !ws.entities(level(), c(0, 0)).contains(&member),
        "its own level no longer lists it in the old cell"
    );
    assert!(
        ws.entities(level(), c(1, 0)).contains(&member),
        "it moved inside its own level"
    );
    assert!(
        !ws.entities(other, c(0, 0)).contains(&member) && !ws.entities(other, c(1, 0)).contains(&member),
        "the other level never lists it"
    );
}

/// A member's descendants change cell lists with the root (spec 78 § 10.5).
#[test]
fn a_walking_member_takes_its_children_to_the_new_cell() {
    use regia_ecs::prelude::ChildOf;
    let mut app = app();
    let world = build(&app, 2, 1, 1, 1, settings(250.0, 300.0));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 8);
    let root = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(50.0, 0.0, 50.0)),
            StreamAutoMember,
        ))
        .id();
    let child = app.world_mut().spawn((Transform::default(), ChildOf(root))).id();
    steps(&mut app, 2);
    assert_eq!(app.world().get::<CellMember>(root).map(|m| m.0), Some(c(0, 0)));
    app.world_mut()
        .get_mut::<Transform>(root)
        .expect("root")
        .translation
        .x = 150.0;
    steps(&mut app, 2);
    assert_eq!(app.world().get::<CellMember>(root).map(|m| m.0), Some(c(1, 0)));
    assert_eq!(app.world().get::<CellMember>(child).map(|m| m.0), Some(c(1, 0)));
    let ws = app.world().resource::<regia_world::WorldStreaming>();
    assert!(ws.entities(level(), c(1, 0)).contains(&child));
    assert!(!ws.entities(level(), c(0, 0)).contains(&child));
}

/// Stationary dynamic roots must not scan the cell's root list. One body that crosses into a
/// spawned neighbour is an index update, not a walk of the cell (audit finding 4).
#[test]
fn perf_stationary_dynamic_roots_do_not_scan_roots() {
    let mut app = app();
    let world = build(&app, 2, 1, 32, 1, settings(250.0, 400.0));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    for _ in 0..40 {
        if state(&app, c(0, 0)) == Some(CellState::Visible)
            && state(&app, c(1, 0)) == Some(CellState::Visible)
        {
            break;
        }
        app.update();
    }
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Visible));
    assert_eq!(state(&app, c(1, 0)), Some(CellState::Visible));
    let members: Vec<regia_ecs::prelude::Entity> = app
        .world_mut()
        .query::<(regia_ecs::prelude::Entity, &CellMember)>()
        .iter(app.world())
        .filter(|(_, member)| member.0 == c(0, 0))
        .map(|(entity, _)| entity)
        .collect();
    assert_eq!(members.len(), 32);
    for entity in &members {
        app.world_mut().entity_mut(*entity).insert(StreamGuarded);
    }
    let before = app
        .world()
        .resource::<regia_world::WorldStreaming>()
        .guard_root_visits();
    steps(&mut app, 8);
    let idle = app
        .world()
        .resource::<regia_world::WorldStreaming>()
        .guard_root_visits();
    assert_eq!(idle, before, "stationary roots were scanned");
    move_to(&mut app, members[0], 150.0, 50.0);
    app.update();
    let moved = app
        .world()
        .resource::<regia_world::WorldStreaming>()
        .guard_root_visits();
    assert!(
        moved - idle <= 4,
        "moving one root examined {} root-list entries",
        moved - idle
    );
    assert_eq!(
        app.world().get::<CellMember>(members[0]).map(|member| member.0),
        Some(c(1, 0)),
        "the body joined the neighbour cell"
    );
}
