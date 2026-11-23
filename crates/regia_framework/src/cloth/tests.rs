// SPDX-License-Identifier: MPL-2.0
use std::time::Duration;

use regia_animation::Skeleton;
use regia_core::id::Guid;
use regia_ecs::ecs::world::CommandQueue;
use regia_ecs::spawn_order::SpawnOrder;
use regia_physics::cloth::asset::{ClothSource, SimMeshInput, SoftSource};
use regia_render::{CpuMaterial, mesh_content_revision, mesh_key};
use regia_scene::Transform;
use regia_scene::prelude::EntityGuid;

use super::*;

/// A `cols x rows` sheet in the XY plane hanging down from y = 0, spacing `s`.
fn sheet(cols: u32, rows: u32, s: f32) -> SimMeshInput {
    let mut mesh = SimMeshInput::default();
    for r in 0..rows {
        for c in 0..cols {
            mesh.positions.push(Vec3::new(c as f32 * s, -(r as f32) * s, 0.0));
            mesh.uvs
                .push([c as f32 / (cols - 1) as f32, r as f32 / (rows - 1) as f32]);
        }
    }
    for r in 0..rows - 1 {
        for c in 0..cols - 1 {
            let i = r * cols + c;
            mesh.triangles.push([i, i + cols, i + 1]);
            mesh.triangles.push([i + 1, i + cols, i + cols + 1]);
        }
    }
    mesh
}

/// A 6 x 6 flag whose top row follows its pose (`reach 0`).
fn flag_asset() -> Arc<ClothAsset> {
    let source = ClothSource::from_ron_str(
        r#"(format_version: 1, name: "flag", sim_mesh: (source: "flag.glb", node: "Flag"),
            params: (substeps: 4, damping: 0.5),
            maps: { "reach": Sparse(default: 10.0, entries: [(0, 0.0), (1, 0.0), (2, 0.0), (3, 0.0), (4, 0.0), (5, 0.0)]) })"#,
    )
    .expect("parses");
    Arc::new(ClothAsset::cook(&source, &sheet(6, 6, 0.1), None, &[]).expect("cooks"))
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed);
    app.add_plugins(ClothGluePlugin);
    app
}

fn placed(at: Vec3) -> (Transform, GlobalTransform) {
    let t = Transform::from_translation(at);
    (t, GlobalTransform(Affine3A::from_translation(at)))
}

fn run(app: &mut App, frames: usize) {
    for _ in 0..frames {
        app.update();
    }
}

fn solver_name_of(app: &App, entity: Entity) -> String {
    app.world()
        .get::<ClothGlueState>(entity)
        .expect("glue state")
        .solver
        .clone()
}

#[test]
fn a_cloth_sim_gets_a_solver_that_follows_its_entity() {
    let mut app = app();
    let at = Vec3::new(0.0, 2.0, 0.0);
    let e = app
        .world_mut()
        .spawn((ClothSim::new(flag_asset()), placed(at).0, placed(at).1))
        .id();
    run(&mut app, 30);
    let name = solver_name_of(&app, e);
    assert_eq!(name, format!("cloth:{}", e.to_bits()));
    let set = app.world().resource::<ClothSet>();
    let solver = set.get(&name).expect("solver in the set");
    assert!(solver.stats().steps > 10, "stepped in FixedUpdate");
    // The pinned top row sits on the entity-placed rest pose; the bottom row hangs below it.
    assert!(
        solver.particles()[3]
            .position
            .distance(at + Vec3::new(0.3, 0.0, 0.0))
            < 1e-5
    );
    assert!(solver.particles()[33].position.y < at.y - 0.45);
    let output = app.world().get::<ClothOutput>(e).expect("output");
    assert_eq!(output.cur.len(), 36);
    assert!(output.steps > 0);

    // Move the entity: the pose targets move the pinned row with it.
    let moved = Vec3::new(1.0, 2.0, 0.0);
    let (t, g) = placed(moved);
    app.world_mut().entity_mut(e).insert((t, g));
    run(&mut app, 20);
    let set = app.world().resource::<ClothSet>();
    let solver = set.get(&name).expect("solver");
    assert!(solver.particles()[0].position.distance(moved) < 1e-5);
}

#[test]
fn publish_writes_a_new_section_mesh_in_local_space_each_frame() {
    let mut app = app();
    let material: Arc<CpuMaterial> = Arc::new(CpuMaterial::default());
    let first = Arc::new(CpuMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    });
    let section_at = Vec3::new(0.0, 1.0, 0.0);
    let section = app
        .world_mut()
        .spawn((
            MeshRenderer::new(first.clone(), material),
            placed(section_at).0,
            placed(section_at).1,
        ))
        .id();
    let mut sim = ClothSim::new(flag_asset());
    sim.section = Some(section);
    let at = Vec3::new(0.0, 2.0, 0.0);
    app.world_mut().spawn((sim, placed(at).0, placed(at).1));
    run(&mut app, 5);
    let mesh = app
        .world()
        .get::<MeshRenderer>(section)
        .expect("renderer")
        .mesh
        .clone();
    assert!(!Arc::ptr_eq(&mesh, &first));
    assert_eq!(mesh.vertices.len(), 36);
    assert_eq!(mesh.indices.len(), 25 * 2 * 3);
    // Vertex 0 is pinned at world (0, 2, 0): (0, 1, 0) in the section's frame.
    let p0 = Vec3::from_array(mesh.vertices[0].position);
    assert!(p0.distance(Vec3::new(0.0, 1.0, 0.0)) < 1e-4, "{p0}");
    run(&mut app, 1);
    let again = app
        .world()
        .get::<MeshRenderer>(section)
        .expect("renderer")
        .mesh
        .clone();
    let key = mesh_key(&mesh);
    assert_eq!(mesh_key(&again), key, "both render buffers share one cache key");
    let generation = mesh_content_revision(key);
    assert!(generation > 0, "a published cloth is dynamic");
    assert!(
        !app.world()
            .get::<MeshRenderer>(section)
            .expect("renderer")
            .gi_proxy
    );
    // Drop the samples: a spare strong `Arc` on the back buffer is what the renderer does
    // not hold, and the in-place rewrite refuses it.
    drop(mesh);
    drop(again);
    app.insert_resource(ClothRenderArcs::default());
    run(&mut app, 2);
    let third = app
        .world()
        .get::<MeshRenderer>(section)
        .expect("renderer")
        .mesh
        .clone();
    assert_eq!(mesh_key(&third), key, "frames share one cache key");
    assert!(
        mesh_content_revision(key) > generation,
        "content revision advanced ({generation} -> {})",
        mesh_content_revision(key)
    );
    assert_eq!(
        app.world().resource::<ClothRenderArcs>().arcs,
        0,
        "the back buffer was rewritten in place, no new arc"
    );
}

#[test]
fn invisible_cloth_pauses_and_resumes() {
    let mut app = app();
    let at = Vec3::new(0.0, 2.0, 0.0);
    let e = app
        .world_mut()
        .spawn((
            ClothSim::new(flag_asset()),
            placed(at).0,
            placed(at).1,
            InheritedVisibility(true),
        ))
        .id();
    run(&mut app, 3);
    let name = solver_name_of(&app, e);
    app.world_mut().entity_mut(e).insert(InheritedVisibility(false));
    run(&mut app, 2);
    let steps = app
        .world()
        .resource::<ClothSet>()
        .get(&name)
        .expect("solver")
        .stats()
        .steps;
    assert_eq!(
        app.world()
            .resource::<ClothSet>()
            .get(&name)
            .expect("solver")
            .state(),
        ClothState::Paused
    );
    run(&mut app, 10);
    assert_eq!(
        app.world()
            .resource::<ClothSet>()
            .get(&name)
            .expect("solver")
            .stats()
            .steps,
        steps,
        "paused solvers do not step"
    );
    app.world_mut().entity_mut(e).insert(InheritedVisibility(true));
    run(&mut app, 3);
    let solver = app.world().resource::<ClothSet>().get(&name).expect("solver");
    assert_ne!(solver.state(), ClothState::Paused);
    assert!(solver.stats().steps > steps);
}

#[test]
fn a_visible_cloth_keeps_a_pause_the_glue_did_not_make() {
    let mut app = app();
    let at = Vec3::new(0.0, 2.0, 0.0);
    let e = app
        .world_mut()
        .spawn((
            ClothSim::new(flag_asset()),
            placed(at).0,
            placed(at).1,
            InheritedVisibility(true),
        ))
        .id();
    run(&mut app, 3);
    let name = solver_name_of(&app, e);
    let state = |app: &App| {
        let solver = app.world().resource::<ClothSet>();
        solver.get(&name).expect("solver").state()
    };
    app.world_mut()
        .resource_mut::<ClothSet>()
        .get_mut(&name)
        .expect("solver")
        .set_paused(true);
    run(&mut app, 5);
    assert_eq!(state(&app), ClothState::Paused, "a host's pause stays");
    // Hiding and showing it again resumes it: the pause is the glue's from the moment it hides.
    app.world_mut().entity_mut(e).insert(InheritedVisibility(false));
    run(&mut app, 2);
    assert_eq!(state(&app), ClothState::Paused);
    app.world_mut().entity_mut(e).insert(InheritedVisibility(true));
    run(&mut app, 2);
    assert_ne!(state(&app), ClothState::Paused);
}

#[test]
fn replacing_the_asset_rebuilds_and_despawning_removes_the_solver() {
    let mut app = app();
    let at = Vec3::new(0.0, 2.0, 0.0);
    let e = app
        .world_mut()
        .spawn((ClothSim::new(flag_asset()), placed(at).0, placed(at).1))
        .id();
    run(&mut app, 20);
    let name = solver_name_of(&app, e);
    let before = app
        .world()
        .resource::<ClothSet>()
        .get(&name)
        .expect("solver")
        .stats()
        .steps;
    assert!(before > 10);
    app.world_mut().get_mut::<ClothSim>(e).expect("sim").asset = flag_asset();
    run(&mut app, 2);
    let after = app
        .world()
        .resource::<ClothSet>()
        .get(&name)
        .expect("solver")
        .stats()
        .steps;
    assert!(after < before, "rebuilt from the new asset ({after} < {before})");
    assert!(app.world_mut().despawn(e));
    run(&mut app, 2);
    assert!(app.world().resource::<ClothSet>().get(&name).is_none());
}

#[test]
fn body_proxies_keep_the_cloth_off_a_moving_sphere() {
    let mut app = app();
    let at = Vec3::new(0.0, 2.0, 0.0);
    let mut sim = ClothSim::new(flag_asset());
    sim.solver = "cape".into();
    app.world_mut().spawn((sim, placed(at).0, placed(at).1));
    // A sphere proxy right behind the flag's lower half.
    let ball_at = Vec3::new(0.25, 1.6, -0.12);
    let ball = app
        .world_mut()
        .spawn((
            ClothBodyProxy {
                name: "chest".into(),
                shape: ProxyShape::Sphere {
                    center: Vec3::ZERO,
                    radius: 0.1,
                },
                friction: 0.3,
            },
            placed(ball_at).0,
            placed(ball_at).1,
        ))
        .id();
    run(&mut app, 5);
    // Push the ball forward through the cloth plane over 30 frames.
    for k in 0..30 {
        let p = ball_at + Vec3::new(0.0, 0.0, 0.01 * k as f32);
        let (t, g) = placed(p);
        app.world_mut().entity_mut(ball).insert((t, g));
        run(&mut app, 1);
    }
    let set = app.world().resource::<ClothSet>();
    let solver = set.get("cape").expect("solver");
    assert_eq!(solver.colliders().len(), 1);
    let center = ball_at + Vec3::new(0.0, 0.0, 0.29);
    let thickness = solver.config().thickness;
    for p in solver.particles() {
        assert!(
            p.position.distance(center) >= 0.1 + thickness - 1e-3,
            "particle inside the proxy: {}",
            p.position.distance(center)
        );
    }
}

#[test]
fn skinned_cloth_follows_the_owner_palette() {
    let mut app = app();
    let skeleton = Arc::new(Skeleton {
        joint_names: vec!["root".into()],
        parents: vec![None],
        bind_pose: vec![regia_animation::skeleton::LocalBind {
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        }],
        inverse_bind_matrices: vec![Mat4::IDENTITY],
        sockets: Vec::new(),
    });
    let joint = app.world_mut().spawn(placed(Vec3::ZERO)).id();
    let owner_at = Vec3::new(0.0, 1.0, 0.0);
    let owner = app
        .world_mut()
        .spawn((
            SkinnedMesh {
                skeleton,
                joints: vec![joint],
            },
            SkinPalette {
                matrices: vec![Mat4::from_translation(Vec3::new(0.0, 0.5, 0.0))],
            },
            placed(owner_at).0,
            placed(owner_at).1,
        ))
        .id();
    let mut mesh = sheet(4, 4, 0.1);
    mesh.joints = vec![[0, 0, 0, 0]; 16];
    mesh.weights = vec![[1.0, 0.0, 0.0, 0.0]; 16];
    let source = ClothSource::from_ron_str(
        r#"(format_version: 1, name: "cape", skeleton: Some("hero.glb"), sim_mesh: (source: "cape.glb", node: "Cape"),
            maps: { "reach": Sparse(default: 0.05, entries: [(0, 0.0), (1, 0.0), (2, 0.0), (3, 0.0)]) })"#,
    )
    .expect("parses");
    let asset = Arc::new(ClothAsset::cook(&source, &mesh, None, &[]).expect("cooks"));
    assert!(asset.is_skinned());
    let mut sim = ClothSim::new(asset);
    sim.skinned_mesh = Some(owner);
    let e = app
        .world_mut()
        .spawn((sim, placed(Vec3::ZERO).0, placed(Vec3::ZERO).1))
        .id();
    run(&mut app, 30);
    let name = solver_name_of(&app, e);
    let set = app.world().resource::<ClothSet>();
    let solver = set.get(&name).expect("solver");
    // Rest vertex 2 at (0.2, 0, 0) -> palette +0.5 y -> owner world +1 y.
    assert!(solver.particles()[2].position.distance(Vec3::new(0.2, 1.5, 0.0)) < 1e-5);
    // Every free particle stays within its 5 cm reach of its skinned target.
    for (i, p) in solver.particles().iter().enumerate().skip(4) {
        let rest = mesh.positions[i] + Vec3::new(0.0, 1.5, 0.0);
        assert!(p.position.distance(rest) <= 0.05 + 1e-4, "particle {i}");
    }
    assert!(
        app.world()
            .get::<ClothGlueState>(e)
            .expect("state")
            .last_error
            .is_none()
    );
}

#[test]
fn soft_sim_gets_a_solver_and_an_output() {
    let mut app = app();
    // An octahedron (closed, outward).
    let positions = vec![Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z];
    let triangles = vec![
        [0, 2, 4],
        [2, 1, 4],
        [1, 3, 4],
        [3, 0, 4],
        [2, 0, 5],
        [1, 2, 5],
        [3, 1, 5],
        [0, 3, 5],
    ];
    let surface = SimMeshInput {
        positions,
        triangles,
        ..SimMeshInput::default()
    };
    let source = SoftSource::from_ron_str(
        r#"(format_version: 1, name: "ball", model: Pressure(pressure: 1.0, volume_compliance: 0.0),
            surface: (source: "ball.glb", node: "Ball"), params: (gravity_scale: 0.0))"#,
    )
    .expect("parses");
    let asset = Arc::new(SoftAsset::cook(&source, &surface, None).expect("cooks"));
    let e = app
        .world_mut()
        .spawn((SoftSim::new(asset), placed(Vec3::ZERO).0, placed(Vec3::ZERO).1))
        .id();
    run(&mut app, 10);
    let name = solver_name_of(&app, e);
    let softs = app.world().resource::<SoftSet>();
    let solver = softs.get(&name).expect("soft solver");
    assert!((solver.volume() / solver.rest_volume() - 1.0).abs() < 0.02);
    let output = app.world().get::<ClothOutput>(e).expect("output");
    assert_eq!(output.cur.len(), 6);
    assert!(output.cur.iter().all(|t| t.normal.length() > 0.9));
}

#[test]
fn blend_zero_outputs_the_pose_and_blend_one_the_simulation() {
    // Spec 55 test 19, headless half: the caped-character output at blend 0 and 1 (the
    // screenshot half is a lead TODO on the GPU reference).
    let mut app = app();
    let at = Vec3::new(0.0, 2.0, 0.0);
    let mut sim = ClothSim::new(flag_asset());
    sim.blend = 0.0;
    let e = app.world_mut().spawn((sim, placed(at).0, placed(at).1)).id();
    run(&mut app, 40);
    let rest = flag_asset();
    let output = app.world().get::<ClothOutput>(e).expect("output").cur.clone();
    for (i, t) in output.iter().enumerate() {
        let pose = rest.lod0().rest_positions[i] + at;
        assert!(
            t.position.distance(pose) < 1e-5,
            "blend 0 vertex {i} off the pose"
        );
    }
    app.world_mut().get_mut::<ClothSim>(e).expect("sim").blend = 1.0;
    run(&mut app, 60);
    let name = solver_name_of(&app, e);
    let set = app.world().resource::<ClothSet>();
    let solver = set.get(&name).expect("solver");
    let output = &app.world().get::<ClothOutput>(e).expect("output").cur;
    for (t, p) in output.iter().zip(solver.particles()) {
        assert!(
            t.position.distance(p.position) < 1e-5,
            "blend 1 output equals the simulation"
        );
    }
}

/// Keeps every cloth message the app wrote.
#[derive(Resource, Default)]
struct Log {
    changes: Vec<ClothStateChanged>,
    teleports: Vec<ClothTeleported>,
    tears: Vec<ClothTear>,
}

fn log_messages(
    mut log: ResMut<Log>,
    mut changes: MessageReader<ClothStateChanged>,
    mut teleports: MessageReader<ClothTeleported>,
    mut tears: MessageReader<ClothTear>,
) {
    log.changes.extend(changes.read().copied());
    log.teleports.extend(teleports.read().copied());
    log.tears.extend(tears.read().copied());
}

#[test]
fn state_changes_and_teleports_are_reported_for_the_entity() {
    let mut app = app();
    app.init_resource::<Log>().add_systems(Update, log_messages);
    let at = Vec3::new(0.0, 2.0, 0.0);
    let e = app
        .world_mut()
        .spawn((ClothSim::new(flag_asset()), placed(at).0, placed(at).1))
        .id();
    run(&mut app, 12);
    {
        let log = app.world().resource::<Log>();
        assert_eq!(
            log.changes,
            vec![ClothStateChanged {
                entity: e,
                from: ClothState::Settling,
                to: ClothState::Running,
            }],
            "settling ends after `settle_steps`"
        );
        assert!(log.teleports.is_empty());
    }
    // A jump past `teleport_distance` snaps the cloth and starts settling again.
    let far = at + Vec3::new(10.0, 0.0, 0.0);
    let (t, g) = placed(far);
    app.world_mut().entity_mut(e).insert((t, g));
    run(&mut app, 3);
    let log = app.world().resource::<Log>();
    assert_eq!(log.teleports, vec![ClothTeleported { entity: e }]);
    assert_eq!(
        log.changes.last(),
        Some(&ClothStateChanged {
            entity: e,
            from: ClothState::Running,
            to: ClothState::Settling,
        })
    );
}

#[test]
fn tears_are_reported_in_constraint_order() {
    let mut app = app();
    app.init_resource::<Log>().add_systems(Update, log_messages);
    let source = ClothSource::from_ron_str(
        r#"(format_version: 1, name: "weak", sim_mesh: (source: "flag.glb", node: "Flag"),
            params: (substeps: 4, stretch_compliance: 1.0, shear_compliance: 1.0, tear: Some((strain: 1.05, max_per_step: 2))),
            maps: { "reach": Sparse(default: 10.0, entries: [(0, 0.0), (5, 0.0)]) })"#,
    )
    .expect("parses");
    let asset = Arc::new(ClothAsset::cook(&source, &sheet(6, 6, 0.1), None, &[]).expect("cooks"));
    let at = Vec3::new(0.0, 2.0, 0.0);
    let e = app
        .world_mut()
        .spawn((ClothSim::new(asset), placed(at).0, placed(at).1))
        .id();
    // Pull hard on the free corner.
    let name = {
        run(&mut app, 2);
        solver_name_of(&app, e)
    };
    for _ in 0..60 {
        let mut set = app.world_mut().resource_mut::<ClothSet>();
        let solver = set.get_mut(&name).expect("solver");
        let p = solver.particles()[35].position;
        solver
            .set_position(35, p - Vec3::new(0.0, 0.05, 0.0))
            .expect("moves");
        run(&mut app, 1);
    }
    let log = app.world().resource::<Log>();
    assert!(!log.tears.is_empty(), "the pull cut links");
    assert!(log.tears.iter().all(|t| t.entity == e));
    for pair in log.tears.windows(2) {
        assert!(
            pair[0].step < pair[1].step || pair[0].constraint < pair[1].constraint,
            "ascending within a step: {pair:?}"
        );
    }
}

fn sphere_proxy(name: &str) -> ClothBodyProxy {
    ClothBodyProxy {
        name: name.into(),
        shape: ProxyShape::Sphere {
            center: Vec3::ZERO,
            radius: 0.05,
        },
        friction: 0.3,
    }
}

/// A skinned owner with one joint, and a proxy on that joint.
fn character(app: &mut App, proxy: &str) -> (Entity, Entity) {
    let skeleton = Arc::new(Skeleton {
        joint_names: vec!["root".into()],
        parents: vec![None],
        bind_pose: vec![regia_animation::skeleton::LocalBind {
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        }],
        inverse_bind_matrices: vec![Mat4::IDENTITY],
        sockets: Vec::new(),
    });
    let joint = app
        .world_mut()
        .spawn((placed(Vec3::ZERO), sphere_proxy(proxy)))
        .id();
    let owner = app
        .world_mut()
        .spawn((
            SkinnedMesh {
                skeleton,
                joints: vec![joint],
            },
            placed(Vec3::ZERO).0,
            placed(Vec3::ZERO).1,
        ))
        .id();
    (owner, joint)
}

fn skinned_cape() -> Arc<ClothAsset> {
    let mut mesh = sheet(4, 4, 0.1);
    mesh.joints = vec![[0, 0, 0, 0]; 16];
    mesh.weights = vec![[1.0, 0.0, 0.0, 0.0]; 16];
    let source = ClothSource::from_ron_str(
        r#"(format_version: 1, name: "cape", skeleton: Some("hero.glb"), sim_mesh: (source: "cape.glb", node: "Cape"),
            maps: { "reach": Sparse(default: 1.0, entries: [(0, 0.0), (3, 0.0)]) })"#,
    )
    .expect("parses");
    Arc::new(ClothAsset::cook(&source, &mesh, None, &[]).expect("cooks"))
}

#[test]
fn a_skinned_cloth_only_collides_with_its_own_character_and_sees_late_proxies() {
    let mut app = app();
    let (hero, _) = character(&mut app, "torso");
    let (_rival, _) = character(&mut app, "torso");
    let mut sim = ClothSim::new(skinned_cape());
    sim.skinned_mesh = Some(hero);
    sim.solver = "cape".into();
    app.world_mut()
        .spawn((sim, placed(Vec3::ZERO).0, placed(Vec3::ZERO).1));
    run(&mut app, 3);
    let colliders = |app: &App| {
        app.world()
            .resource::<ClothSet>()
            .get("cape")
            .expect("solver")
            .colliders()
            .len()
    };
    assert_eq!(colliders(&app), 1, "the rival's proxy is not this cloth's");

    // A proxy added under the hero after the cloth started is picked up; a stranger's is not.
    let hand = app
        .world_mut()
        .spawn((
            placed(Vec3::X).0,
            placed(Vec3::X).1,
            sphere_proxy("hand"),
            ChildOf(hero),
        ))
        .id();
    app.world_mut()
        .spawn((placed(Vec3::X).0, placed(Vec3::X).1, sphere_proxy("rock")));
    run(&mut app, 2);
    assert_eq!(colliders(&app), 2);
    // A proxy that leaves is parked: the indices of the others hold, and it is not added twice.
    assert!(app.world_mut().despawn(hand));
    run(&mut app, 2);
    assert_eq!(colliders(&app), 2);
}

#[test]
fn a_published_section_is_never_a_gi_proxy() {
    let mut app = app();
    let material: Arc<CpuMaterial> = Arc::new(CpuMaterial::default());
    let empty = Arc::new(CpuMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    });
    let section = app
        .world_mut()
        .spawn((
            MeshRenderer::new(empty, material),
            placed(Vec3::ZERO).0,
            placed(Vec3::ZERO).1,
        ))
        .id();
    assert!(
        app.world()
            .get::<MeshRenderer>(section)
            .expect("renderer")
            .gi_proxy
    );
    let mut sim = ClothSim::new(flag_asset());
    sim.section = Some(section);
    app.world_mut()
        .spawn((sim, placed(Vec3::ZERO).0, placed(Vec3::ZERO).1));
    run(&mut app, 3);
    assert!(
        !app.world()
            .get::<MeshRenderer>(section)
            .expect("renderer")
            .gi_proxy
    );
}

/// A 6 x 6 flag (LOD 0) with a 3 x 3 sim mesh of the same size as its coarser LOD.
fn flag_with_coarse_lod() -> Arc<ClothAsset> {
    let source = ClothSource::from_ron_str(
        r#"(format_version: 1, name: "flag", sim_mesh: (source: "flag.glb", node: "Flag"),
            params: (substeps: 4, damping: 0.5),
            maps: { "reach": Sparse(default: 10.0, entries: [(0, 0.0), (1, 0.0), (2, 0.0), (3, 0.0), (4, 0.0), (5, 0.0)]) },
            lods: [(sim_mesh: Some((source: "flag.glb", node: "Coarse")))])"#,
    )
    .expect("parses");
    let coarse = sheet(3, 3, 0.25);
    Arc::new(ClothAsset::cook(&source, &sheet(6, 6, 0.1), None, &[Some(coarse)]).expect("cooks"))
}

#[test]
fn far_cloth_moves_to_the_coarser_sim_mesh_and_back_without_a_settle() {
    let mut app = app();
    let at = Vec3::new(0.0, 2.0, 0.0);
    let section = app
        .world_mut()
        .spawn((
            MeshRenderer::new(
                Arc::new(CpuMesh {
                    vertices: Vec::new(),
                    indices: Vec::new(),
                }),
                Arc::new(CpuMaterial::default()),
            ),
            placed(Vec3::ZERO).0,
            placed(Vec3::ZERO).1,
        ))
        .id();
    let mut sim = ClothSim::new(flag_with_coarse_lod());
    sim.section = Some(section);
    sim.solver = "flag".into();
    let e = app.world_mut().spawn((sim, placed(at).0, placed(at).1)).id();
    let camera = app
        .world_mut()
        .spawn((Camera::default(), placed(at + Vec3::Z).0, placed(at + Vec3::Z).1))
        .id();
    run(&mut app, 60);
    let near = app.world().resource::<ClothSet>().get("flag").expect("solver");
    assert_eq!(near.particles().len(), 36);
    assert_eq!(near.state(), ClothState::Running);
    let steps = near.stats().steps;
    // Coarse vertex 6 continues fine vertex 30: both rest at the bottom left corner.
    let hanging = near.particles()[30].position;

    // 40 m away is level 2 (thresholds 15, 30, 60 m): the coarse mesh takes over.
    let far = at + Vec3::new(0.0, 0.0, 40.0);
    let (t, g) = placed(far);
    app.world_mut().entity_mut(camera).insert((t, g));
    run(&mut app, 3);
    let coarse = app.world().resource::<ClothSet>().get("flag").expect("solver");
    assert_eq!(coarse.particles().len(), 9, "the coarser sim mesh runs");
    assert_eq!(coarse.lod_level(), 2);
    assert_eq!(coarse.state(), ClothState::Running, "no settle");
    assert_eq!(coarse.diagnostics().teleports, 0);
    assert!(coarse.stats().steps >= steps, "the step count carries over");
    assert!(
        coarse.particles()[0].position.distance(at) < 1e-5,
        "the top row stays on the pose"
    );
    // It carries the fine cloth's motion.
    assert!(coarse.particles()[6].position.distance(hanging) < 0.25);
    run(&mut app, 2);
    assert_eq!(app.world().get::<ClothOutput>(e).expect("output").cur.len(), 9);
    assert_eq!(
        app.world()
            .get::<MeshRenderer>(section)
            .expect("section")
            .mesh
            .vertices
            .len(),
        9
    );

    // Back near: the authored mesh returns.
    let (t, g) = placed(at + Vec3::Z);
    app.world_mut().entity_mut(camera).insert((t, g));
    run(&mut app, 4);
    let back = app.world().resource::<ClothSet>().get("flag").expect("solver");
    assert_eq!(back.particles().len(), 36);
    assert_eq!(back.lod_level(), 0);
    assert_eq!(back.state(), ClothState::Running);
    assert_eq!(
        app.world()
            .get::<MeshRenderer>(section)
            .expect("section")
            .mesh
            .vertices
            .len(),
        36
    );
    assert!(
        app.world()
            .get::<ClothGlueState>(e)
            .expect("state")
            .last_error
            .is_none()
    );
}

#[test]
fn a_switched_solver_keeps_its_proxies() {
    let mut app = app();
    let at = Vec3::new(0.0, 2.0, 0.0);
    let mut sim = ClothSim::new(flag_with_coarse_lod());
    sim.solver = "flag".into();
    app.world_mut().spawn((sim, placed(at).0, placed(at).1));
    app.world_mut()
        .spawn((sphere_proxy("torso"), placed(at).0, placed(at).1));
    app.world_mut().spawn((
        Camera::default(),
        placed(at + Vec3::Z * 40.0).0,
        placed(at + Vec3::Z * 40.0).1,
    ));
    run(&mut app, 6);
    let set = app.world().resource::<ClothSet>();
    let solver = set.get("flag").expect("solver");
    assert_eq!(solver.particles().len(), 9);
    assert_eq!(solver.colliders().len(), 1);
}

#[test]
fn a_budget_pushed_cloth_does_not_flap_between_sim_meshes() {
    let mut app = app();
    app.insert_resource(ClothBudget {
        max_particles_per_step: 10,
    });
    let at = Vec3::new(0.0, 2.0, 0.0);
    let mut sim = ClothSim::new(flag_with_coarse_lod());
    sim.solver = "flag".into();
    app.world_mut().spawn((sim, placed(at).0, placed(at).1));
    app.world_mut()
        .spawn((Camera::default(), placed(at + Vec3::Z).0, placed(at + Vec3::Z).1));
    run(&mut app, 4);
    for _ in 0..10 {
        run(&mut app, 1);
        let solver = app.world().resource::<ClothSet>().get("flag").expect("solver");
        assert_eq!(
            solver.particles().len(),
            9,
            "36 LOD 0 particles cost 9 at level 2 and stay there"
        );
        assert_eq!(solver.lod_level(), 2);
    }
}

// ---------------------------------------------------------------------------
// The rigid world: snapshots and two-way reaction (spec 55 § 2.4, test 11)
// ---------------------------------------------------------------------------

fn physics_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed);
    app.add_plugins(regia_physics::PhysicsPlugin);
    app.add_plugins(ClothGluePlugin);
    app
}

/// A free horizontal `n x n` sheet of spacing `s` around the entity.
fn free_sheet(n: u32, s: f32) -> Arc<ClothAsset> {
    let half = (n - 1) as f32 * s * 0.5;
    let mut mesh = sheet(n, n, s);
    for p in &mut mesh.positions {
        *p = Vec3::new(p.x - half, 0.0, p.y + half);
    }
    let source = ClothSource::from_ron_str(
        r#"(format_version: 1, name: "free", sim_mesh: (source: "free.glb", node: "Free"),
            params: (substeps: 4, damping: 0.5))"#,
    )
    .expect("parses");
    Arc::new(ClothAsset::cook(&source, &mesh, None, &[]).expect("cooks"))
}

fn box_collider(half_extents: Vec3) -> Collider {
    Collider {
        shape: ColliderShape::Box { half_extents },
        ..Collider::default()
    }
}

fn lowest(app: &App, cloth: Entity) -> f32 {
    let name = solver_name_of(app, cloth);
    let set = app.world().resource::<ClothSet>();
    let solver = set.get(&name).expect("solver");
    solver
        .particles()
        .iter()
        .map(|p| p.position.y)
        .fold(f32::MAX, f32::min)
}

fn spawn_floor(app: &mut App, collider: Collider) {
    let at = Vec3::new(0.0, -0.5, 0.0);
    app.world_mut()
        .spawn((placed(at).0, placed(at).1, RigidBody::static_body(), collider));
}

#[test]
fn a_cloth_lands_on_a_rigid_box_through_its_world_snapshot() {
    let mut app = physics_app();
    spawn_floor(&mut app, box_collider(Vec3::new(2.0, 0.5, 2.0)));
    let at = Vec3::new(0.0, 0.2, 0.0);
    let cloth = app
        .world_mut()
        .spawn((ClothSim::new(free_sheet(8, 0.05)), placed(at).0, placed(at).1))
        .id();
    run(&mut app, 120);
    let low = lowest(&app, cloth);
    assert!(low > 0.0, "rests on the top face ({low})");
    assert!(low < 0.05, "fell onto it ({low})");
}

#[test]
fn sensors_and_layers_the_cloth_ignores_stay_out_of_the_snapshot() {
    // A sensor floor: nothing to land on.
    let mut app = physics_app();
    let mut sensor = box_collider(Vec3::new(2.0, 0.5, 2.0));
    sensor.sensor = true;
    spawn_floor(&mut app, sensor);
    let at = Vec3::new(0.0, 0.2, 0.0);
    let cloth = app
        .world_mut()
        .spawn((ClothSim::new(free_sheet(8, 0.05)), placed(at).0, placed(at).1))
        .id();
    run(&mut app, 60);
    assert!(lowest(&app, cloth) < -0.5, "fell through the sensor");

    // A floor on a layer the cloth's layer does not query.
    let mut app = physics_app();
    let layers = regia_physics::LayerFile::parse(
        r#"(layers: ["default", "cape", "floor"], collides: [("floor", "floor")], queries: [("default", "floor")])"#,
    )
    .and_then(|file| PhysicsLayers::from_file(&file))
    .expect("layers");
    let floor_layer = CollisionLayer::new(layers.index_of("floor").expect("floor"));
    app.insert_resource(layers);
    let mut floor = box_collider(Vec3::new(2.0, 0.5, 2.0));
    floor.layer = floor_layer;
    spawn_floor(&mut app, floor);
    let mut cape = ClothSim::new(free_sheet(8, 0.05));
    cape.layer = "cape".to_string();
    let cape = app.world_mut().spawn((cape, placed(at).0, placed(at).1)).id();
    let mut seen = ClothSim::new(free_sheet(8, 0.05));
    seen.layer = "default".to_string();
    let seen = app.world_mut().spawn((seen, placed(at).0, placed(at).1)).id();
    run(&mut app, 120);
    assert!(
        lowest(&app, cape) < -0.5,
        "the cape's layer does not see the floor"
    );
    assert!(lowest(&app, seen) > 0.0, "the default layer does");
}

#[test]
fn heightfield_quad_span_clips_to_the_query() {
    // 3 samples, scale 2: sample k sits at k - 1. Quad 0 is [-1, 0], quad 1 is [0, 1].
    assert_eq!(heightfield_quad_span(-0.3, 0.3, 3, 2.0), 0..2);
    assert_eq!(heightfield_quad_span(0.9, 1.1, 3, 2.0), 1..2);
    assert_eq!(heightfield_quad_span(1.5, 2.0, 3, 2.0), 0..0);
    assert_eq!(heightfield_quad_span(-2.0, -1.5, 3, 2.0), 0..0);
    // A scale that is not positive visits every quad.
    assert_eq!(heightfield_quad_span(-0.3, 0.3, 3, -2.0), 0..2);
}

#[test]
fn cloth_snapshot_skips_heightfield_cells_outside_the_aabb() {
    use regia_ecs::ecs::schedule::{ScheduleLabel, Schedules, SingleThreadedExecutor};
    // 33 x 33 samples, 1 m cells: 32 x 32 quads. The cloth is a pinned 0.3 m patch, so the
    // query covers a handful of cells, not the tile.
    const N: usize = 33;
    let height_field = ColliderShape::HeightField {
        nx: N,
        nz: N,
        heights: vec![0.0; N * N],
        scale: Vec3::new(32.0, 1.0, 32.0),
    };
    let source = ClothSource::from_ron_str(
        r#"(format_version: 1, name: "pin", sim_mesh: (source: "pin.glb", node: "Pin"),
            params: (substeps: 1, damping: 1.0),
            maps: { "reach": Sparse(default: 0.0, entries: []) })"#,
    )
    .expect("parses");
    let mut mesh = sheet(3, 3, 0.15);
    for p in &mut mesh.positions {
        *p = Vec3::new(p.x - 0.15, 0.0, p.y + 0.15);
    }
    let asset = Arc::new(ClothAsset::cook(&source, &mesh, None, &[]).expect("cooks"));
    let mut app = physics_app();
    // On this thread, so the thread-local cell counter sees the snapshot walk.
    let label = regia_app::prelude::FixedUpdate.intern();
    if let Some(schedule) = app.world_mut().resource_mut::<Schedules>().get_mut(label) {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
    app.world_mut().spawn((
        placed(Vec3::ZERO).0,
        placed(Vec3::ZERO).1,
        RigidBody::static_body(),
        Collider {
            shape: height_field,
            ..Collider::default()
        },
    ));
    let at = Vec3::new(0.0, 0.4, 0.0);
    let cloth = app
        .world_mut()
        .spawn((ClothSim::new(asset), placed(at).0, placed(at).1))
        .id();
    let _ = take_heightfield_cells();
    run(&mut app, 1);
    let first = take_heightfield_cells();
    let full = ((N - 1) * (N - 1)) as u64;
    run(&mut app, 1);
    let second = take_heightfield_cells();
    let name = solver_name_of(&app, cloth);
    let triangles = app
        .world()
        .resource::<ClothSet>()
        .get(&name)
        .expect("solver")
        .snapshot()
        .triangle_count();
    assert!(triangles > 0, "the cloth still receives terrain triangles");
    assert!(
        first > 0 && first * 8 < full && second == 0,
        "visited {first} cells of {full} on the first step and {second} on the unchanged second step"
    );
}

#[test]
fn terrain_and_mesh_colliders_become_snapshot_triangles() {
    // A flat 3 x 3 heightfield and a two-triangle mesh, both 2 m wide at y = 0, under a cloth.
    let height_field = ColliderShape::HeightField {
        nx: 3,
        nz: 3,
        heights: vec![0.0; 9],
        scale: Vec3::new(2.0, 1.0, 2.0),
    };
    let tri_mesh = ColliderShape::TriMesh {
        vertices: vec![
            Vec3::new(-1.0, 0.0, -1.0),
            Vec3::new(-1.0, 0.0, 1.0),
            Vec3::new(1.0, 0.0, -1.0),
            Vec3::new(1.0, 0.0, 1.0),
        ],
        indices: vec![[0, 1, 2], [2, 1, 3]],
    };
    for shape in [height_field, tri_mesh] {
        let mut app = physics_app();
        app.world_mut().spawn((
            placed(Vec3::ZERO).0,
            placed(Vec3::ZERO).1,
            RigidBody::static_body(),
            Collider {
                shape,
                ..Collider::default()
            },
        ));
        let at = Vec3::new(0.0, 0.2, 0.0);
        let cloth = app
            .world_mut()
            .spawn((ClothSim::new(free_sheet(8, 0.05)), placed(at).0, placed(at).1))
            .id();
        run(&mut app, 120);
        let low = lowest(&app, cloth);
        assert!((-0.02..0.05).contains(&low), "rests on the triangles ({low})");
        let name = solver_name_of(&app, cloth);
        let set = app.world().resource::<ClothSet>();
        let snapshot = set.get(&name).expect("solver").snapshot();
        assert!(
            snapshot
                .shapes()
                .iter()
                .any(|s| matches!(s.kind, SnapshotKind::Triangles { .. }))
        );
    }
}

#[test]
fn a_small_convex_hull_becomes_planes_and_a_large_one_stays_a_box() {
    let tetra = ColliderShape::ConvexHull {
        points: vec![
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(0.0, 0.0, 1.0),
        ],
    };
    let mut out = Vec::new();
    snapshot_kinds(&tetra, Affine3A::IDENTITY, (Vec3::ZERO, 1.0), &mut out);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, Affine3A::IDENTITY, "planes stay in the collider frame");
    match &out[0].1 {
        SnapshotKind::Hull { planes } => assert!(
            (4..=regia_physics::cloth::snapshot::SNAPSHOT_HULL_PLANES_MAX).contains(&planes.len()),
            "{planes:?}"
        ),
        other => panic!("expected a hull, got {other:?}"),
    }

    let flat = ColliderShape::ConvexHull {
        points: vec![
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 1.0),
            Vec3::new(0.0, 0.0, 1.0),
        ],
    };
    out.clear();
    snapshot_kinds(&flat, Affine3A::IDENTITY, (Vec3::ZERO, 1.0), &mut out);
    assert!(
        matches!(out.first().map(|(_, kind)| kind), Some(SnapshotKind::Box { .. })),
        "a flat cloud has no hull: {out:?}"
    );

    let many = ColliderShape::ConvexHull {
        points: (0..65)
            .map(|i| Vec3::new(i as f32, (i % 7) as f32, (i % 5) as f32))
            .collect(),
    };
    out.clear();
    snapshot_kinds(&many, Affine3A::IDENTITY, (Vec3::ZERO, 1.0), &mut out);
    assert!(matches!(
        out.first().map(|(_, kind)| kind),
        Some(SnapshotKind::Box { .. })
    ));
}

#[test]
fn a_small_convex_hull_stays_a_hull_and_a_large_one_is_its_box() {
    let tetra = ColliderShape::ConvexHull {
        points: vec![Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::Z],
    };
    let mut out = Vec::new();
    snapshot_kinds(&tetra, Affine3A::IDENTITY, (Vec3::ZERO, 10.0), &mut out);
    let SnapshotKind::Hull { planes } = &out[0].1 else {
        panic!("a tetrahedron is a hull, got {:?}", out[0].1);
    };
    let corner = Vec3::new(0.9, 0.9, 0.9);
    let outside = planes
        .iter()
        .map(|(n, d)| n.dot(corner) - *d)
        .fold(f32::MIN, f32::max);
    assert!(
        outside > 0.5,
        "the cube corner is outside the tetrahedron, so cloth there is not pushed ({outside})"
    );

    let mut points = Vec::with_capacity(96);
    let golden = std::f32::consts::PI * (3.0 - 5.0_f32.sqrt());
    for i in 0..96 {
        let y = 1.0 - (i as f32 / 95.0) * 2.0;
        let radius = (1.0 - y * y).sqrt();
        let theta = golden * i as f32;
        points.push(Vec3::new(theta.cos() * radius, y, theta.sin() * radius));
    }
    let shape = ColliderShape::ConvexHull { points };
    out.clear();
    snapshot_kinds(&shape, Affine3A::IDENTITY, (Vec3::ZERO, 10.0), &mut out);
    assert!(
        matches!(out[0].1, SnapshotKind::Box { .. }),
        "a hull past 64 vertices is its bounding box, got {:?}",
        out[0].1
    );
}

/// The `(prev, cur)` translations of the last snapshot's shapes, as the solver got them.
#[derive(Resource, Default)]
struct PosesBeforeStep(Vec<(Vec3, Vec3)>);

fn capture_poses_before_step(
    set: Res<ClothSet>,
    states: Query<&ClothGlueState, With<ClothSim>>,
    mut out: ResMut<PosesBeforeStep>,
) {
    out.0.clear();
    for state in &states {
        if let Some(solver) = set.get(&state.solver) {
            let shapes = solver.snapshot().shapes();
            out.0
                .extend(shapes.iter().map(|s| (s.prev.translation, s.cur.translation)));
        }
    }
}

#[test]
fn a_snapshot_holds_a_body_where_the_rigid_step_left_it() {
    let mut app = physics_app();
    // No `ScenePlugin`: `GlobalTransform` stays at the spawn pose, as it does inside a frame.
    let at = Vec3::new(0.0, 1.0, 0.0);
    let body = app
        .world_mut()
        .spawn((
            placed(at).0,
            placed(at).1,
            RigidBody::dynamic(),
            box_collider(Vec3::splat(0.2)),
        ))
        .id();
    // A second collider on a child entity: same body, 0.5 m to the side.
    let side = Vec3::new(0.5, 0.0, 0.0);
    app.world_mut().spawn((
        Transform::from_translation(side),
        GlobalTransform(Affine3A::from_translation(at + side)),
        box_collider(Vec3::splat(0.1)),
        ChildOf(body),
    ));
    let cloth_at = Vec3::new(0.0, 0.6, 0.0);
    app.world_mut().spawn((
        ClothSim::new(free_sheet(8, 0.1)),
        placed(cloth_at).0,
        placed(cloth_at).1,
    ));
    app.init_resource::<PosesBeforeStep>().add_systems(
        FixedUpdate,
        capture_poses_before_step
            .after(build_cloth_snapshots)
            .before(regia_physics::cloth::step_cloth_set),
    );
    run(&mut app, 12);

    let pose = app.world().get::<PhysicsPose>(body).expect("pose");
    assert!(pose.translation.y < at.y - 1e-3, "the body fell");
    let cur = pose.translation;
    let prev = app
        .world()
        .get::<PreviousPhysicsPose>(body)
        .expect("previous")
        .translation;
    assert!(prev.distance(cur) > 1e-4, "the body moved during the last step");
    // Read before the solver stepped: a step rests every shape at its current pose.
    let mut seen = app.world().resource::<PosesBeforeStep>().0.clone();
    assert_eq!(seen.len(), 2, "both colliders of the body");
    seen.sort_by(|a, b| a.1.x.total_cmp(&b.1.x));
    assert!(seen[0].1.distance(cur) < 1e-5, "{:?} vs {cur}", seen[0]);
    assert!(seen[0].0.distance(prev) < 1e-5, "{:?} vs {prev}", seen[0]);
    assert!(
        seen[1].1.distance(cur + side) < 1e-5,
        "the child rides on its body"
    );
    // The child's previous pose comes from its body too, so its swept motion is the body's.
    assert!(
        seen[1].0.distance(prev + side) < 1e-5,
        "the child's previous pose is its body's: {:?} vs {}",
        seen[1].0,
        prev + side
    );
}

#[test]
fn collider_poses_follow_the_rigid_step_and_keep_the_place_on_the_body() {
    let at = |p: Vec3| Isometry::new(Quat::IDENTITY, p);
    let side = Vec3::new(0.5, 0.0, 0.0);
    // The propagated transforms are a frame old; the collider's place on its body still holds.
    let stale = GlobalTransform(Affine3A::from_translation(Vec3::Y));
    let world = at(Vec3::Y + side);
    let current = PhysicsPose {
        translation: Vec3::new(0.0, 0.8, 0.0),
        rotation: Quat::IDENTITY,
    };
    let previous = PreviousPhysicsPose {
        translation: Vec3::new(0.0, 0.9, 0.0),
        rotation: Quat::IDENTITY,
    };
    let (prev, cur) = collider_poses(world, Some((&stale, Some(&current), Some(&previous))));
    assert!(cur.translation.distance(current.translation + side) < 1e-6);
    assert!(prev.translation.distance(previous.translation + side) < 1e-6);
    // A body the rigid step has not seen stays put; a collider with no body too.
    let (prev, cur) = collider_poses(world, Some((&stale, None, None)));
    assert_eq!(
        (prev.translation, cur.translation),
        (world.translation, world.translation)
    );
    let (prev, cur) = collider_poses(world, None);
    assert_eq!(
        (prev.translation, cur.translation),
        (world.translation, world.translation)
    );
}

/// A floating (zero gravity) dynamic box of 1 kg with a free cloth falling onto it.
fn cloth_on_a_floating_box(reaction: bool, cap: f32) -> (App, Entity) {
    let mut app = physics_app();
    let body = app
        .world_mut()
        .spawn((
            placed(Vec3::new(0.0, -0.5, 0.0)).0,
            placed(Vec3::new(0.0, -0.5, 0.0)).1,
            RigidBody {
                gravity_scale: 0.0,
                ..RigidBody::dynamic()
            },
            Collider {
                density: 1.0 / 8.0,
                ..box_collider(Vec3::splat(1.0))
            },
        ))
        .id();
    let at = Vec3::new(0.0, 0.52, 0.0);
    let mut sim = ClothSim::new(free_sheet(8, 0.05));
    sim.reaction = reaction;
    sim.max_reaction_impulse = cap;
    app.world_mut().spawn((sim, placed(at).0, placed(at).1));
    (app, body)
}

fn speed_down(app: &App, body: Entity) -> f32 {
    let pose = app.world().get::<PhysicsPose>(body).expect("pose");
    let prev = app.world().get::<PreviousPhysicsPose>(body).expect("previous");
    (prev.translation.y - pose.translation.y) * 60.0
}

#[test]
fn two_way_reaction_pushes_a_dynamic_body_and_is_off_by_default() {
    let (mut off, body) = cloth_on_a_floating_box(false, MAX_REACTION_IMPULSE);
    run(&mut off, 90);
    assert_eq!(speed_down(&off, body), 0.0, "one-way: the box never moves");

    let (mut on, body) = cloth_on_a_floating_box(true, MAX_REACTION_IMPULSE);
    run(&mut on, 90);
    // A 25 g cloth on a 1 kg box: momentum `m g t` plus the landing, 0.24 N over 1.5 s at most.
    let speed = speed_down(&on, body);
    assert!(speed > 0.1, "the cloth landed and pushed the box ({speed})");
    assert!(speed < 0.4, "no more than the cloth's weight ({speed})");
}

#[test]
fn two_way_reaction_reaches_a_body_through_a_child_collider() {
    // As `cloth_on_a_floating_box`, with the box collider on a child entity of the body.
    let mut app = physics_app();
    let body = app
        .world_mut()
        .spawn((
            placed(Vec3::new(0.0, -0.5, 0.0)).0,
            placed(Vec3::new(0.0, -0.5, 0.0)).1,
            RigidBody {
                gravity_scale: 0.0,
                ..RigidBody::dynamic()
            },
        ))
        .id();
    app.world_mut().spawn((
        Transform::default(),
        GlobalTransform(Affine3A::from_translation(Vec3::new(0.0, -0.5, 0.0))),
        Collider {
            density: 1.0 / 8.0,
            ..box_collider(Vec3::splat(1.0))
        },
        ChildOf(body),
    ));
    let at = Vec3::new(0.0, 0.52, 0.0);
    let mut sim = ClothSim::new(free_sheet(8, 0.05));
    sim.reaction = true;
    app.world_mut().spawn((sim, placed(at).0, placed(at).1));
    run(&mut app, 90);
    let speed = speed_down(&app, body);
    assert!(
        speed > 0.1,
        "the cloth pushed the body its child collider belongs to ({speed})"
    );
    assert!(speed < 0.4, "no more than the cloth's weight ({speed})");
}

#[test]
fn two_way_reaction_is_capped_per_body_per_step() {
    let cap = 1e-5;
    let (mut capped, body) = cloth_on_a_floating_box(true, cap);
    run(&mut capped, 90);
    let speed = speed_down(&capped, body);
    assert!(speed > 0.0, "the capped impulse still arrives ({speed})");
    // 1 kg, at most `cap` per step over 90 steps.
    assert!(speed <= cap * 90.0 * 1.01, "{speed}");
}

#[test]
fn two_way_reaction_leaves_static_bodies_alone() {
    let mut app = physics_app();
    let floor = app
        .world_mut()
        .spawn((
            placed(Vec3::new(0.0, -0.5, 0.0)).0,
            placed(Vec3::new(0.0, -0.5, 0.0)).1,
            RigidBody::static_body(),
            box_collider(Vec3::new(2.0, 0.5, 2.0)),
        ))
        .id();
    let at = Vec3::new(0.0, 0.05, 0.0);
    let mut sim = ClothSim::new(free_sheet(8, 0.05));
    sim.reaction = true;
    app.world_mut().spawn((sim, placed(at).0, placed(at).1));
    for _ in 0..60 {
        app.update();
        assert!(app.world().get::<ExternalImpulse>(floor).is_none());
    }
}

/// A closed octahedron of radius 1 (vertex 2 is the top), cooked as a pressure ball.
fn ball_mesh() -> SimMeshInput {
    SimMeshInput {
        positions: vec![Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z],
        triangles: vec![
            [0, 2, 4],
            [2, 1, 4],
            [1, 3, 4],
            [3, 0, 4],
            [2, 0, 5],
            [1, 2, 5],
            [3, 1, 5],
            [0, 3, 5],
        ],
        ..SimMeshInput::default()
    }
}

fn ball_on(mesh: &SimMeshInput, rest: &str) -> Arc<SoftAsset> {
    let source = SoftSource::from_ron_str(&format!(
        r#"(format_version: 1, name: "ball", model: Pressure(pressure: 1.0, volume_compliance: 0.0),
            surface: (source: "ball.glb", node: "Ball"), {rest})"#,
    ))
    .expect("parses");
    Arc::new(SoftAsset::cook(&source, mesh, None).expect("cooks"))
}

fn ball(rest: &str) -> Arc<SoftAsset> {
    ball_on(&ball_mesh(), rest)
}

fn soft<'a>(app: &'a App, name: &str) -> &'a ClothSolver {
    app.world()
        .resource::<SoftSet>()
        .get(name)
        .expect("soft solver")
        .cloth()
}

#[test]
fn a_hidden_soft_body_pauses_and_wakes_where_it_was() {
    let mut app = app();
    let at = Vec3::new(0.0, 50.0, 0.0);
    let e = app
        .world_mut()
        .spawn((
            SoftSim::new(ball("")),
            placed(at).0,
            placed(at).1,
            InheritedVisibility(true),
        ))
        .id();
    run(&mut app, 30);
    let name = solver_name_of(&app, e);
    assert!(soft(&app, &name).centroid().y < at.y - 0.1, "it fell");

    app.world_mut().entity_mut(e).insert(InheritedVisibility(false));
    run(&mut app, 2);
    assert_eq!(soft(&app, &name).state(), ClothState::Paused);
    let steps = soft(&app, &name).stats().steps;
    let hidden_at = soft(&app, &name).centroid();

    // Past the resume limit (600 steps) a cloth goes back to its pose. A free soft body has none.
    run(&mut app, 620);
    assert_eq!(
        soft(&app, &name).stats().steps,
        steps,
        "paused bodies do not step"
    );
    app.world_mut().entity_mut(e).insert(InheritedVisibility(true));
    run(&mut app, 1);
    let solver = soft(&app, &name);
    assert_ne!(solver.state(), ClothState::Paused);
    assert!(
        solver.centroid().distance(hidden_at) < 0.05,
        "woke at {} after hiding at {hidden_at}",
        solver.centroid()
    );
}

#[test]
fn a_far_soft_body_stops_and_wakes_where_it_stopped() {
    let mut app = app();
    let at = Vec3::new(0.0, 50.0, 0.0);
    let mut sim = SoftSim::new(ball(""));
    sim.solver = "ball".into();
    sim.lod_distances = [100.0, 200.0, 300.0];
    app.world_mut().spawn((sim, placed(at).0, placed(at).1));
    let camera = app
        .world_mut()
        .spawn((Camera::default(), placed(at).0, placed(at).1))
        .id();
    run(&mut app, 30);
    assert_eq!(soft(&app, "ball").lod_level(), 0);

    let (t, g) = placed(at + Vec3::new(0.0, 0.0, 150.0));
    app.world_mut().entity_mut(camera).insert((t, g));
    run(&mut app, 2);
    assert_eq!(soft(&app, "ball").lod_level(), 1);

    let (t, g) = placed(at + Vec3::new(0.0, 0.0, 400.0));
    app.world_mut().entity_mut(camera).insert((t, g));
    run(&mut app, 40);
    let off = soft(&app, "ball");
    assert_eq!(off.lod_level(), LOD_OFF);
    assert_eq!(off.state(), ClothState::Inactive);
    let stopped_at = off.centroid();
    assert!(stopped_at.y < at.y - 0.1, "it fell before it stopped");

    let (t, g) = placed(at);
    app.world_mut().entity_mut(camera).insert((t, g));
    run(&mut app, 1);
    let solver = soft(&app, "ball");
    assert_eq!(solver.lod_level(), 0);
    assert_ne!(solver.state(), ClothState::Inactive);
    assert!(solver.centroid().distance(stopped_at) < 0.05);
}

#[test]
fn cloth_and_soft_bodies_share_the_budget() {
    let mut app = app();
    // 36 + 6 particles over a budget of 40: the farther one moves down, whichever kind it is.
    app.insert_resource(ClothBudget {
        max_particles_per_step: 40,
    });
    let mut cloth = ClothSim::new(flag_asset());
    cloth.solver = "flag".into();
    let mut body = SoftSim::new(ball("params: (gravity_scale: 0.0)"));
    body.solver = "ball".into();
    app.world_mut()
        .spawn((cloth, placed(Vec3::ZERO).0, placed(Vec3::ZERO).1));
    let at = Vec3::new(0.0, 0.0, -8.0);
    app.world_mut().spawn((body, placed(at).0, placed(at).1));
    let camera = Vec3::new(0.0, 0.0, 2.0);
    app.world_mut()
        .spawn((Camera::default(), placed(camera).0, placed(camera).1));
    run(&mut app, 3);
    let flag = app.world().resource::<ClothSet>().get("flag").expect("solver");
    assert_eq!(flag.lod_level(), 0);
    assert_eq!(soft(&app, "ball").lod_level(), 1, "the soft body counts");
}

#[test]
fn a_posed_soft_body_follows_its_entity_and_a_free_one_does_not() {
    let mut app = app();
    let at = Vec3::new(0.0, 2.0, 0.0);
    let mut posed = SoftSim::new(ball(
        r#"params: (gravity_scale: 0.0), maps: { "reach": Sparse(default: 10.0, entries: [(2, 0.0)]) }"#,
    ));
    posed.solver = "posed".into();
    assert!(posed.asset.is_posed());
    let posed = app.world_mut().spawn((posed, placed(at).0, placed(at).1)).id();
    let mut free = SoftSim::new(ball(
        r#"params: (gravity_scale: 0.0), maps: { "mass_scale": Sparse(default: 2.0, entries: []) }"#,
    ));
    free.solver = "free".into();
    assert!(!free.asset.is_posed(), "mass_scale alone is no attachment");
    let free_at = Vec3::new(5.0, 2.0, 0.0);
    let free = app
        .world_mut()
        .spawn((free, placed(free_at).0, placed(free_at).1))
        .id();
    run(&mut app, 5);
    let top = |app: &App| soft(app, "posed").particles()[2].position;
    assert!(top(&app).distance(at + Vec3::Y) < 1e-5);

    let moved = Vec3::new(0.5, 2.0, 0.0);
    let (t, g) = placed(moved);
    app.world_mut().entity_mut(posed).insert((t, g));
    let (t, g) = placed(free_at + Vec3::X);
    app.world_mut().entity_mut(free).insert((t, g));
    run(&mut app, 5);
    assert!(top(&app).distance(moved + Vec3::Y) < 1e-5, "{}", top(&app));
    assert!(
        app.world()
            .get::<ClothGlueState>(posed)
            .expect("state")
            .last_error
            .is_none()
    );
    assert!(
        soft(&app, "free").centroid().distance(free_at) < 1e-3,
        "a free body stays where the simulation has it"
    );
}

#[test]
fn a_skinned_soft_body_follows_the_owner_palette() {
    let mut app = app();
    let (hero, _) = character(&mut app, "torso");
    let at = Vec3::new(0.0, 1.0, 0.0);
    let (t, g) = placed(at);
    app.world_mut().entity_mut(hero).insert((
        t,
        g,
        SkinPalette {
            matrices: vec![Mat4::from_translation(Vec3::new(0.0, 0.5, 0.0))],
        },
    ));
    let mut mesh = ball_mesh();
    mesh.joints = vec![[0, 0, 0, 0]; 6];
    mesh.weights = vec![[1.0, 0.0, 0.0, 0.0]; 6];
    let asset = ball_on(
        &mesh,
        r#"params: (gravity_scale: 0.0), maps: { "reach": Sparse(default: 10.0, entries: [(2, 0.0)]) }"#,
    );
    assert!(asset.is_skinned());
    let mut sim = SoftSim::new(asset);
    sim.solver = "flesh".into();
    sim.skinned_mesh = Some(hero);
    let e = app
        .world_mut()
        .spawn((sim, placed(Vec3::ZERO).0, placed(Vec3::ZERO).1))
        .id();
    run(&mut app, 3);
    let top = |app: &App| soft(app, "flesh").particles()[2].position;
    assert!(
        top(&app).distance(Vec3::new(0.0, 2.5, 0.0)) < 1e-5,
        "{}",
        top(&app)
    );

    app.world_mut()
        .get_mut::<SkinPalette>(hero)
        .expect("palette")
        .matrices[0] = Mat4::from_translation(Vec3::new(0.3, 0.5, 0.0));
    run(&mut app, 3);
    assert!(
        top(&app).distance(Vec3::new(0.3, 2.5, 0.0)) < 1e-5,
        "{}",
        top(&app)
    );
    assert!(
        app.world()
            .get::<ClothGlueState>(e)
            .expect("state")
            .last_error
            .is_none()
    );
}

#[test]
fn a_soft_body_collides_with_body_proxies_of_its_own_character() {
    let mut app = app();
    // A resting 1 m sphere under a free ball; a free body takes every proxy in the world.
    let shelf = ClothBodyProxy {
        name: "shelf".into(),
        shape: ProxyShape::Sphere {
            center: Vec3::ZERO,
            radius: 1.0,
        },
        friction: 0.5,
    };
    app.world_mut()
        .spawn((placed(Vec3::ZERO).0, placed(Vec3::ZERO).1, shelf));
    let at = Vec3::new(0.0, 2.5, 0.0);
    let mut free = SoftSim::new(ball(""));
    free.solver = "free".into();
    app.world_mut().spawn((free, placed(at).0, placed(at).1));
    run(&mut app, 180);
    let solver = soft(&app, "free");
    assert_eq!(solver.colliders().len(), 1);
    assert!(
        solver.particles()[3].position.y > 0.9,
        "the bottom vertex rests on the proxy: {}",
        solver.particles()[3].position
    );

    // A skinned soft body takes its own character's proxies only.
    let (hero, _) = character(&mut app, "torso");
    let (_rival, _) = character(&mut app, "torso");
    let mut mesh = ball_mesh();
    mesh.joints = vec![[0, 0, 0, 0]; 6];
    mesh.weights = vec![[1.0, 0.0, 0.0, 0.0]; 6];
    let mut flesh = SoftSim::new(ball_on(
        &mesh,
        r#"params: (gravity_scale: 0.0), maps: { "reach": Sparse(default: 10.0, entries: [(2, 0.0)]) }"#,
    ));
    flesh.solver = "flesh".into();
    flesh.skinned_mesh = Some(hero);
    app.world_mut()
        .spawn((flesh, placed(Vec3::ZERO).0, placed(Vec3::ZERO).1));
    run(&mut app, 3);
    assert_eq!(soft(&app, "flesh").colliders().len(), 1);
}

/// One flag on a root of this scale drawn into an identity section: (world winding z, vertex
/// normal z, output normal z) of the first triangle.
fn flag_facing(scale: Vec3) -> (f32, f32, f32) {
    let mut app = app();
    let section = app
        .world_mut()
        .spawn((
            MeshRenderer::new(
                Arc::new(CpuMesh {
                    vertices: Vec::new(),
                    indices: Vec::new(),
                }),
                Arc::new(CpuMaterial::default()),
            ),
            placed(Vec3::ZERO).0,
            placed(Vec3::ZERO).1,
        ))
        .id();
    let mut sim = ClothSim::new(flag_asset());
    sim.section = Some(section);
    let root = Transform {
        scale,
        ..Transform::default()
    };
    let e = app
        .world_mut()
        .spawn((sim, root, GlobalTransform(Affine3A::from_scale(scale))))
        .id();
    run(&mut app, 3);
    let mesh = app
        .world()
        .get::<MeshRenderer>(section)
        .expect("renderer")
        .mesh
        .clone();
    let corner = |i: usize| Vec3::from_array(mesh.vertices[mesh.indices[i] as usize].position);
    let (a, b, c) = (corner(0), corner(1), corner(2));
    let drawn = (b - a).cross(c - a).normalize_or_zero().z;
    let vertex = Vec3::from_array(mesh.vertices[mesh.indices[0] as usize].normal).z;
    let output = app.world().get::<ClothOutput>(e).expect("output").cur[7].normal.z;
    (drawn, vertex, output)
}

#[test]
fn a_mirrored_root_keeps_the_front_of_the_cloth_outward() {
    // The sheet faces +z, and mirroring x keeps that side outward.
    let (drawn, vertex, output) = flag_facing(Vec3::ONE);
    assert!(
        drawn > 0.9 && vertex > 0.9 && output > 0.9,
        "{drawn} {vertex} {output}"
    );
    let (drawn, vertex, output) = flag_facing(Vec3::new(-1.0, 1.0, 1.0));
    assert!(drawn > 0.9, "winding flipped: {drawn}");
    assert!(vertex > 0.9, "published normal flipped: {vertex}");
    assert!(output > 0.9, "output normal flipped: {output}");
}

#[test]
fn pull_scale_reaches_the_solver_on_change_and_across_a_sim_mesh_switch() {
    let mut app = app();
    let at = Vec3::new(0.0, 2.0, 0.0);
    let mut sim = ClothSim::new(flag_with_coarse_lod());
    sim.solver = "flag".into();
    sim.pull_scale = 0.25;
    let e = app.world_mut().spawn((sim, placed(at).0, placed(at).1)).id();
    let pull = |app: &App| {
        let set = app.world().resource::<ClothSet>();
        set.get("flag").expect("solver").tuning().pull_scale
    };
    run(&mut app, 2);
    assert_eq!(pull(&app), 0.25);

    // Only a change is pushed: an agent's edit holds until the game changes the component.
    let solver = app.world_mut().resource_mut::<ClothSet>().into_inner();
    let solver = solver.get_mut("flag").expect("solver");
    let edited = ClothTuning {
        pull_scale: 0.9,
        ..*solver.tuning()
    };
    solver.set_tuning(edited).expect("edit");
    run(&mut app, 2);
    assert_eq!(pull(&app), 0.9);
    app.world_mut().get_mut::<ClothSim>(e).expect("sim").pull_scale = 0.5;
    run(&mut app, 2);
    assert_eq!(pull(&app), 0.5);

    // The coarser sim mesh is another solver: the component still holds.
    let far = at + Vec3::new(0.0, 0.0, 40.0);
    app.world_mut()
        .spawn((Camera::default(), placed(far).0, placed(far).1));
    run(&mut app, 3);
    let coarse = app.world().resource::<ClothSet>().get("flag").expect("solver");
    assert_eq!(coarse.particles().len(), 9);
    assert_eq!(pull(&app), 0.5);

    // A bad value is a typed error on the instance and changes nothing.
    app.world_mut().get_mut::<ClothSim>(e).expect("sim").pull_scale = f32::NAN;
    run(&mut app, 2);
    assert_eq!(pull(&app), 0.5);
    let state = app.world().get::<ClothGlueState>(e).expect("state");
    assert!(state.last_error.is_some());
}

#[test]
fn a_mirrored_soft_ball_keeps_its_normals_outward() {
    let mut app = app();
    let scale = Vec3::new(-1.0, 1.0, 1.0);
    let root = Transform {
        scale,
        ..Transform::default()
    };
    let mut sim = SoftSim::new(ball("params: (gravity_scale: 0.0)"));
    sim.solver = "ball".into();
    let e = app
        .world_mut()
        .spawn((sim, root, GlobalTransform(Affine3A::from_scale(scale))))
        .id();
    run(&mut app, 5);
    let state = app.world().get::<ClothGlueState>(e).expect("state");
    assert_eq!(state.last_error, None);
    let solver = app.world().resource::<SoftSet>().get("ball").expect("solver");
    assert!((solver.volume() / solver.rest_volume() - 1.0).abs() < 0.02);
    let output = app.world().get::<ClothOutput>(e).expect("output");
    assert!(output.cur.iter().all(|t| t.normal.dot(t.position) > 0.5));
}

/// ADR-2425: two-way reaction also carries the contacts' torque about the body's center of
/// mass. A cloth resting off-center on a free box tips it toward the cloth's side; a centered
/// cloth barely turns it.
#[test]
fn two_way_reaction_turns_a_body_under_an_off_center_cloth() {
    let turn_with_cloth_at = |x: f32| {
        let mut app = physics_app();
        let body = app
            .world_mut()
            .spawn((
                placed(Vec3::new(0.0, -0.5, 0.0)).0,
                placed(Vec3::new(0.0, -0.5, 0.0)).1,
                RigidBody {
                    gravity_scale: 0.0,
                    ..RigidBody::dynamic()
                },
                Collider {
                    density: 1.0 / 8.0,
                    ..box_collider(Vec3::splat(1.0))
                },
            ))
            .id();
        let at = Vec3::new(x, 0.52, 0.0);
        let mut sim = ClothSim::new(free_sheet(8, 0.05));
        sim.reaction = true;
        app.world_mut().spawn((sim, placed(at).0, placed(at).1));
        run(&mut app, 90);
        let pose = app.world().get::<PhysicsPose>(body).expect("pose");
        let (axis, angle) = pose.rotation.to_axis_angle();
        axis.z * angle
    };
    let centered = turn_with_cloth_at(0.0);
    let offset = turn_with_cloth_at(0.6);
    assert!(offset < -1e-4, "the +x side went down: turn about z {offset}");
    assert!(
        centered.abs() < 0.1 * offset.abs(),
        "centered {centered} vs offset {offset}"
    );
}

// ---------------------------------------------------------------------------
// Determinism of cloth ordering (audit framework3, ADR-4764)
// ---------------------------------------------------------------------------

/// Spawns `a_bundle` and `b_bundle` through two command queues, as two parallel systems would
/// record them (ADR-1347): when `b_reserved_first`, the `b` queue reserves its id first but the
/// `a` queue still applies first, so `a` arrives earlier with the higher id.
fn spawn_with_reversed_ids(
    app: &mut App,
    b_reserved_first: bool,
    a_bundle: impl Bundle,
    b_bundle: impl Bundle,
) -> (Entity, Entity) {
    let mut first = CommandQueue::default();
    let mut second = CommandQueue::default();
    let (a, b) = if b_reserved_first {
        let b = Commands::new(&mut second, app.world()).spawn(b_bundle).id();
        let a = Commands::new(&mut first, app.world()).spawn(a_bundle).id();
        (a, b)
    } else {
        let a = Commands::new(&mut first, app.world()).spawn(a_bundle).id();
        let b = Commands::new(&mut second, app.world()).spawn(b_bundle).id();
        (a, b)
    };
    first.apply(app.world_mut());
    second.apply(app.world_mut());
    (a, b)
}

fn solver_level_of(app: &App, entity: Entity) -> u8 {
    let name = solver_name_of(app, entity);
    app.world()
        .resource::<ClothSet>()
        .get(&name)
        .expect("solver")
        .lod_level()
}

/// Two cloths at the same camera distance, ids reserved in the opposite order of spawn arrival,
/// once without and once with `EntityGuid`s that order the opposite way: the particle-budget tie
/// must demote the later arrival — never the higher entity id, never the larger session GUID
/// (framework3-audit finding 1).
#[test]
fn budget_tie_follows_spawn_arrival() {
    for with_guids in [false, true] {
        let mut app = app();
        // 64 + 64 particles at LOD 0; one demotion to level 1 (32 particles) fits exactly.
        app.insert_resource(ClothBudget {
            max_particles_per_step: 96,
        });
        app.world_mut().spawn((
            Camera::default(),
            placed(Vec3::new(2.5, 2.0, 2.5)).0,
            placed(Vec3::new(2.5, 2.0, 2.5)).1,
        ));
        // `free_sheet` is centered on the entity, so both centroids sit exactly on the spawn
        // position: the distances are equal and the budget tie is decided by the key alone.
        let bundle = |pos: Vec3| (ClothSim::new(free_sheet(8, 0.05)), placed(pos).0, placed(pos).1);
        let (a, b) = if with_guids {
            spawn_with_reversed_ids(
                &mut app,
                true,
                (
                    bundle(Vec3::new(5.0, 2.0, 0.0)),
                    EntityGuid(Guid::from_u128(0x200)),
                ),
                (
                    bundle(Vec3::new(0.0, 2.0, 5.0)),
                    EntityGuid(Guid::from_u128(0x100)),
                ),
            )
        } else {
            spawn_with_reversed_ids(
                &mut app,
                true,
                bundle(Vec3::new(5.0, 2.0, 0.0)),
                bundle(Vec3::new(0.0, 2.0, 5.0)),
            )
        };
        run(&mut app, 30);
        assert_eq!(
            solver_level_of(&app, a),
            0,
            "the earlier arrival keeps LOD 0 (guids: {with_guids})"
        );
        assert_eq!(
            solver_level_of(&app, b),
            1,
            "the later arrival loses the budget tie (guids: {with_guids})"
        );
    }
}

/// One cloth and one soft body at the same camera distance, ids reserved in the opposite order
/// of spawn arrival, in both directions: the particle-budget tie must demote the later arrival —
/// whichever type it is. Pre-fix the two types keyed by two independent counters that both
/// start at 0, so an equal-distance mixed tie fell through to the entity id, which parallel
/// `Commands` reserve in thread-timing order (framework3 review high 1).
#[test]
fn mixed_budget_tie_follows_spawn_arrival_across_types() {
    // A free sheet with gravity off: the solver stays at its spawn centroid, so the cloth and
    // the soft body keep bit-equal camera distances and the tie is decided by the key alone.
    let still_sheet = || {
        let half = 7.0f32 * 0.05 * 0.5;
        let mut mesh = sheet(8, 8, 0.05);
        for p in &mut mesh.positions {
            *p = Vec3::new(p.x - half, 0.0, p.y + half);
        }
        let source = ClothSource::from_ron_str(
            r#"(format_version: 1, name: "free", sim_mesh: (source: "free.glb", node: "Free"),
                params: (substeps: 4, damping: 0.5, gravity_scale: 0.0))"#,
        )
        .expect("parses");
        Arc::new(ClothAsset::cook(&source, &mesh, None, &[]).expect("cooks"))
    };
    let soft_level_of = |app: &App, entity: Entity| {
        let name = solver_name_of(app, entity);
        app.world()
            .resource::<SoftSet>()
            .get(&name)
            .expect("soft solver")
            .cloth()
            .lod_level()
    };
    for cloth_first in [true, false] {
        let mut app = app();
        // 64 cloth particles + 6 soft particles at LOD 0 (70); one demotion fits either way
        // (32 + 6 or 64 + 3), so exactly one solver loses the tie.
        app.insert_resource(ClothBudget {
            max_particles_per_step: 69,
        });
        app.world_mut().spawn((
            Camera::default(),
            placed(Vec3::new(2.5, 2.0, 2.5)).0,
            placed(Vec3::new(2.5, 2.0, 2.5)).1,
        ));
        let cloth_bundle = |pos: Vec3| (ClothSim::new(still_sheet()), placed(pos).0, placed(pos).1);
        let soft_bundle = |pos: Vec3| {
            (
                SoftSim::new(ball("params: (gravity_scale: 0.0)")),
                placed(pos).0,
                placed(pos).1,
            )
        };
        // `a` always arrives first, with the higher id.
        let ((first, first_is_cloth), (second, second_is_cloth)) = if cloth_first {
            let (a, b) = spawn_with_reversed_ids(
                &mut app,
                true,
                cloth_bundle(Vec3::new(5.0, 2.0, 0.0)),
                soft_bundle(Vec3::new(0.0, 2.0, 5.0)),
            );
            ((a, true), (b, false))
        } else {
            let (a, b) = spawn_with_reversed_ids(
                &mut app,
                true,
                soft_bundle(Vec3::new(0.0, 2.0, 5.0)),
                cloth_bundle(Vec3::new(5.0, 2.0, 0.0)),
            );
            ((a, false), (b, true))
        };
        run(&mut app, 30);
        let level_of = |app: &App, entity: Entity, is_cloth: bool| {
            if is_cloth {
                solver_level_of(app, entity)
            } else {
                soft_level_of(app, entity)
            }
        };
        assert_eq!(
            level_of(&app, first, first_is_cloth),
            0,
            "the earlier arrival keeps LOD 0 (cloth_first: {cloth_first})"
        );
        assert_eq!(
            level_of(&app, second, second_is_cloth),
            1,
            "the later arrival loses the budget tie (cloth_first: {cloth_first})"
        );
    }
}

/// Two order-0 cameras at different distances, ids reserved in both orders: the distance-LOD eye
/// is the earlier-spawned camera on every run, not the camera with the lower entity id
/// (framework3-audit finding 2).
#[test]
fn lod_eye_tie_follows_spawn_order() {
    for b_reserved_first in [true, false] {
        let mut app = app();
        let at = Vec3::new(0.0, 2.0, 0.0);
        let cloth = app
            .world_mut()
            .spawn((ClothSim::new(flag_asset()), placed(at).0, placed(at).1))
            .id();
        let camera = |pos: Vec3| (Camera::default(), placed(pos).0, placed(pos).1);
        // The 9 m camera arrives first either way: 9 m is LOD 0, 25 m is LOD 1.
        let (_near, _far) = spawn_with_reversed_ids(
            &mut app,
            b_reserved_first,
            camera(at + Vec3::new(9.0, 0.0, 0.0)),
            camera(at + Vec3::new(0.0, 0.0, 25.0)),
        );
        run(&mut app, 30);
        assert_eq!(
            solver_level_of(&app, cloth),
            0,
            "the earlier-spawned (near) camera is the eye (b_reserved_first: {b_reserved_first})"
        );
        assert_eq!(app.world().get::<ClothGlueState>(cloth).expect("state").lod(), 0);
    }
}

/// The default authoritative particle budget is one value on every target: a scene must step
/// identically on desktop and Android (framework3-audit finding 5).
#[test]
fn default_particle_budget_is_not_target_os() {
    assert_eq!(
        ClothBudget::default().max_particles_per_step,
        lod::BUDGET_DESKTOP,
        "the fixed-step budget must not depend on the target OS"
    );
}

/// Snapshot shapes are keyed and solved in spawn-arrival order, never entity-id or session-GUID
/// order (framework3-audit finding 1): two overlapping colliders under one cloth, ids reserved in
/// the opposite order of arrival, once without and once with `EntityGuid`s that order the other
/// way.
#[test]
fn snapshot_shapes_follow_spawn_arrival() {
    for with_guids in [false, true] {
        let mut app = physics_app();
        let body = |at: Vec3| {
            (
                placed(at).0,
                placed(at).1,
                RigidBody::static_body(),
                box_collider(Vec3::splat(0.5)),
            )
        };
        let (a, b) = if with_guids {
            spawn_with_reversed_ids(
                &mut app,
                true,
                (
                    body(Vec3::new(-0.2, -0.5, 0.0)),
                    EntityGuid(Guid::from_u128(0x200)),
                ),
                (
                    body(Vec3::new(0.2, -0.5, 0.0)),
                    EntityGuid(Guid::from_u128(0x100)),
                ),
            )
        } else {
            spawn_with_reversed_ids(
                &mut app,
                true,
                body(Vec3::new(-0.2, -0.5, 0.0)),
                body(Vec3::new(0.2, -0.5, 0.0)),
            )
        };
        let at = Vec3::new(0.0, 0.2, 0.0);
        let cloth = app
            .world_mut()
            .spawn((ClothSim::new(free_sheet(8, 0.05)), placed(at).0, placed(at).1))
            .id();
        run(&mut app, 5);
        let name = solver_name_of(&app, cloth);
        let shapes = app
            .world()
            .resource::<ClothSet>()
            .get(&name)
            .expect("solver")
            .snapshot()
            .shapes();
        assert_eq!(shapes.len(), 2);
        assert_eq!(
            shapes[0].key,
            app.world().resource::<SpawnOrder<Collider>>().key(a),
            "the earlier arrival sorts first (guids: {with_guids})"
        );
        assert_eq!(
            shapes[1].key,
            app.world().resource::<SpawnOrder<Collider>>().key(b),
            "the later arrival sorts second (guids: {with_guids})"
        );
    }
}

#[test]
fn solver_lookups_stay_linear_in_the_cloth_count() {
    let mut app = app();
    let n = 12usize;
    for i in 0..n {
        let at = Vec3::new(i as f32, 2.0, 0.0);
        app.world_mut()
            .spawn((ClothSim::new(flag_asset()), placed(at).0, placed(at).1));
    }
    run(&mut app, 3);
    let sum = |app: &mut App| {
        let mut total = 0u64;
        let mut q = app.world_mut().query::<&mut ClothGlueState>();
        for mut state in q.iter_mut(app.world_mut()) {
            total += state.solver_probes;
            state.solver_probes = 0;
        }
        total
    };
    sum(&mut app);
    run(&mut app, 1);
    let probes = sum(&mut app);
    assert!(probes >= 8 * n as u64, "lookups ran ({probes})");
    // A slot hit charges 1; a name scan charges the set length (n). About two dozen glue
    // systems look a solver up per cloth per frame, so the linear bound is 32n; the quadratic
    // scan cost is at least that many lookups times n, which passes 32n for n = 12 as soon as
    // a share of the lookups misses the slot.
    assert!(
        probes <= 32 * n as u64,
        "a slot hit is one probe, not a scan of every cloth ({probes})"
    );
}

#[test]
fn reaction_sums_do_not_run_when_nothing_touches() {
    let mut app = app();
    let mut sim = ClothSim::new(flag_asset());
    sim.reaction = true;
    let at = Vec3::new(0.0, 2.0, 0.0);
    app.world_mut().spawn((sim, placed(at).0, placed(at).1));
    run(&mut app, 2);
    let runs = |app: &mut App| {
        let mut total = 0u64;
        let mut q = app.world_mut().query::<&mut ClothGlueState>();
        for mut state in q.iter_mut(app.world_mut()) {
            total += state.reaction_runs;
            state.reaction_runs = 0;
        }
        total
    };
    runs(&mut app);
    run(&mut app, 1);
    assert_eq!(runs(&mut app), 0, "an empty reaction wrench processes no sums");
}

/// A flag with a render section in `app`; returns the section entity.
fn published_flag(app: &mut App) -> Entity {
    let material: Arc<CpuMaterial> = Arc::new(CpuMaterial::default());
    let section_at = Vec3::new(0.0, 1.0, 0.0);
    let section = app
        .world_mut()
        .spawn((
            MeshRenderer::new(
                Arc::new(CpuMesh {
                    vertices: Vec::new(),
                    indices: Vec::new(),
                }),
                material,
            ),
            placed(section_at).0,
            placed(section_at).1,
        ))
        .id();
    let mut sim = ClothSim::new(flag_asset());
    sim.section = Some(section);
    let at = Vec3::new(0.0, 2.0, 0.0);
    app.world_mut().spawn((sim, placed(at).0, placed(at).1));
    section
}

/// The count of new render arcs belongs to its app: a second app publishing a cloth leaves the
/// first app's count where it was, so parallel tests cannot move each other's numbers.
#[test]
fn render_arc_counts_belong_to_their_app() {
    let mut quiet = app();
    let mut busy = app();
    quiet.init_resource::<ClothRenderArcs>();
    busy.init_resource::<ClothRenderArcs>();
    published_flag(&mut busy);
    run(&mut quiet, 1);
    run(&mut busy, 3);
    assert!(
        busy.world().resource::<ClothRenderArcs>().arcs > 0,
        "the busy app published new arcs"
    );
    let quiet_count = quiet.world().resource::<ClothRenderArcs>().arcs;
    assert_eq!(quiet_count, 0, "the quiet app published nothing");
}

// --- Scene wind (ADR-2884) ---------------------------------------------------------------------

/// The flag hanging at (0, 2, 0) with an optional scene wind and parameter override; returns
/// the digest after every frame and the final centroid.
fn flag_in_wind(
    scene: Option<SceneWind>,
    params: Option<ClothParams>,
    frames: usize,
) -> (Vec<u64>, Vec3, Entity, App) {
    let mut app = app();
    if let Some(scene) = scene {
        app.insert_resource(scene);
    }
    let at = Vec3::new(0.0, 2.0, 0.0);
    let mut sim = ClothSim::new(flag_asset());
    sim.params = params;
    let e = app.world_mut().spawn((sim, placed(at).0, placed(at).1)).id();
    let mut digests = Vec::with_capacity(frames);
    for _ in 0..frames {
        app.update();
        let name = solver_name_of(&app, e);
        digests.push(
            app.world()
                .resource::<ClothSet>()
                .get(&name)
                .map_or(0, ClothSolver::state_digest),
        );
    }
    let name = solver_name_of(&app, e);
    let centroid = app
        .world()
        .resource::<ClothSet>()
        .get(&name)
        .map_or(Vec3::ZERO, ClothSolver::centroid);
    (digests, centroid, e, app)
}

/// No `SceneWind`, a calm one, and a windy one in `Off` mode give the cloth exactly today's
/// result: equal digests every frame.
#[test]
fn zero_scene_wind_leaves_the_cloth_bit_identical() {
    let (base, base_c, _, _) = flag_in_wind(None, None, 90);
    let (calm, calm_c, _, _) = flag_in_wind(Some(SceneWind::CALM), None, 90);
    let off = SceneWind {
        mode: SceneWindMode::Off,
        velocity_m_per_s: [0.0, 9.0],
        mean_m_per_s: [0.0, 9.0],
    };
    let (off_d, _, _, _) = flag_in_wind(Some(off), None, 90);
    let scaled_out = ClothParams {
        wind_scale: 0.0,
        ..flag_asset().lod0().params
    };
    let (zero_scale, _, _, _) = flag_in_wind(Some(SceneWind::manual([0.0, 5.0])), Some(scaled_out), 90);
    // The same parameter override with no scene wind (an override can build the solver differently).
    let (overridden, _, _, _) = flag_in_wind(None, Some(flag_asset().lod0().params), 90);
    assert!(base.iter().any(|&d| d != 0), "the solver ran");
    assert_eq!(base, calm);
    assert_eq!(base, off_d);
    assert_eq!(overridden, zero_scale);
    assert_eq!(
        base_c.to_array().map(f32::to_bits),
        calm_c.to_array().map(f32::to_bits)
    );
}

/// A 5 m/s scene wind across the hanging flag (along +Z, its normal) moves its centroid
/// downwind; the same run twice gives the same bits, and twice the `wind_scale` moves it further.
#[test]
fn a_scene_wind_blows_a_hanging_cloth_downwind_deterministically() {
    let wind = SceneWind::manual([0.0, 5.0]);
    let (_, still, _, _) = flag_in_wind(None, None, 120);
    let (d1, c1, e, app) = flag_in_wind(Some(wind), None, 120);
    let (d2, c2, _, _) = flag_in_wind(Some(wind), None, 120);
    assert_eq!(d1, d2, "deterministic");
    assert_eq!(c1.to_array().map(f32::to_bits), c2.to_array().map(f32::to_bits));
    let shift = c1.z - still.z;
    assert!(shift > 0.01, "downwind shift {shift}");
    assert!(
        (c1.x - still.x).abs() < 0.5 * shift,
        "mostly along the wind: {c1:?} vs {still:?}"
    );
    // What the cloth received is recorded on its glue state.
    let state = app.world().get::<ClothGlueState>(e).expect("glue");
    assert_eq!(state.scene_wind(), Vec3::new(0.0, 0.0, 5.0));
    let doubled = ClothParams {
        wind_scale: 2.0,
        ..flag_asset().lod0().params
    };
    let (_, c3, e3, app3) = flag_in_wind(Some(wind), Some(doubled), 120);
    assert!(c3.z - still.z > shift, "scale 2: {} vs {shift}", c3.z - still.z);
    assert_eq!(
        app3.world().get::<ClothGlueState>(e3).expect("glue").scene_wind(),
        Vec3::new(0.0, 0.0, 10.0)
    );
}

/// The scene wind adds to the host's `ClothSet::set_global_wind` and goes away with the
/// resource: the solver's term is back to the host's value alone.
#[test]
fn the_scene_wind_adds_to_the_host_wind_and_leaves_with_the_resource() {
    let (_, _, e, mut app) = flag_in_wind(Some(SceneWind::manual([1.0, 0.0])), None, 3);
    app.world_mut()
        .resource_mut::<ClothSet>()
        .set_global_wind(Vec3::new(0.0, 0.0, 2.0))
        .expect("host wind");
    run(&mut app, 2);
    let name = solver_name_of(&app, e);
    let term = |app: &App| {
        app.world()
            .resource::<ClothSet>()
            .get(&name)
            .map(ClothSolver::global_wind)
            .expect("solver")
    };
    assert_eq!(term(&app), Vec3::new(1.0, 0.0, 2.0));
    app.world_mut().remove_resource::<SceneWind>();
    run(&mut app, 2);
    assert_eq!(term(&app), Vec3::new(0.0, 0.0, 2.0));
    assert_eq!(
        app.world().get::<ClothGlueState>(e).expect("glue").scene_wind(),
        Vec3::ZERO
    );
}

/// `wind_scale` is optional in a `.cloth.ron`: absent reads as 1 and 1 is never written, so a
/// file without it round-trips byte-identically and cooks to the same bytes.
#[test]
fn wind_scale_is_optional_and_unwritten_at_one() {
    let text = r#"(format_version: 1, name: "flag", sim_mesh: (source: "flag.glb", node: "Flag"), params: (damping: 0.5))"#;
    let source = ClothSource::from_ron_str(text).expect("parses");
    assert_eq!(source.params.wind_scale, 1.0);
    let canonical = source.to_ron_string().expect("ron");
    assert!(!canonical.contains("wind_scale"), "{canonical}");
    let mut windy = source.clone();
    windy.params.wind_scale = 0.25;
    let written = windy.to_ron_string().expect("ron");
    assert!(written.contains("wind_scale: 0.25"), "{written}");
    assert_eq!(
        ClothSource::from_ron_str(&written)
            .expect("reads")
            .params
            .wind_scale,
        0.25
    );
    windy.params.wind_scale = 11.0;
    assert!(windy.params.validate().is_err(), "past 10 is refused");
    let mesh = sheet(3, 3, 0.1);
    let has = |bytes: &[u8]| bytes.windows(10).any(|w| w == b"wind_scale");
    let plain = ClothAsset::cook(&source, &mesh, None, &[]).expect("cooks");
    assert!(
        !has(&plain.to_bytes().expect("bytes")),
        "cooked bytes unchanged at 1"
    );
    windy.params.wind_scale = 0.25;
    let cooked = ClothAsset::cook(&windy, &mesh, None, &[]).expect("cooks");
    let bytes = cooked.to_bytes().expect("bytes");
    assert!(has(&bytes));
    let back = ClothAsset::from_bytes(&bytes).expect("loads");
    assert_eq!(back.lod0().params.wind_scale, 0.25);
}

/// corr-framework-a F6: a sim-LOD switch hands the cloth a new solver with an empty world
/// snapshot, so the snapshot pass must not reuse the old solver's snapshot key. Without physics
/// the snapshot pass does nothing, so the key the test planted shows what the switch left.
#[test]
fn a_sim_lod_switch_drops_the_snapshot_reuse_key() {
    let mut app = app();
    let at = Vec3::new(0.0, 2.0, 0.0);
    let mut sim = ClothSim::new(flag_with_coarse_lod());
    sim.solver = "flag".into();
    let e = app.world_mut().spawn((sim, placed(at).0, placed(at).1)).id();
    let camera = app
        .world_mut()
        .spawn((Camera::default(), placed(at + Vec3::Z).0, placed(at + Vec3::Z).1))
        .id();
    run(&mut app, 10);
    app.world_mut()
        .get_mut::<ClothGlueState>(e)
        .expect("state")
        .snapshot_reuse = Some(SnapshotReuse {
        lo: Vec3::ZERO,
        hi: Vec3::ONE,
        hits: Vec::new(),
    });
    let (t, g) = placed(at + Vec3::new(0.0, 0.0, 40.0));
    app.world_mut().entity_mut(camera).insert((t, g));
    run(&mut app, 3);
    let coarse = app.world().resource::<ClothSet>().get("flag").expect("solver");
    assert_eq!(coarse.particles().len(), 9, "the coarser sim mesh runs");
    assert!(
        app.world()
            .get::<ClothGlueState>(e)
            .expect("state")
            .snapshot_reuse
            .is_none(),
        "the old solver's snapshot key must not survive the switch"
    );
}
