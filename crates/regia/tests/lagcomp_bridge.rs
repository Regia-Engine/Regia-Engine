// SPDX-License-Identifier: MPL-2.0
//! The lag-compensation bridge end to end: the present physics world occludes rewound shots
//! without the targets' own colliders getting in the way, joint shapes follow an animator's pose
//! without any named joint entity, and the `net_lagcomp` gizmo group draws what a query judged.

use std::sync::Arc;
use std::time::Duration;

use regia::lagcomp_bridge::{LagCompBridgePlugin, NetLagCompGizmos, animator_joint, draw_queries};
use regia::net::lagcomp::{
    HitFilter, HitShape, HitShapeKind, HitShapes, LagCompHistory, LagCompJointSource, LagCompOcclusion,
    LagCompensation, PoseSource, lagcomp_record,
};
use regia::net::model::{NetId, NetMode, NetOwner, NetPlayerId, NetTick, NetTickFrac, PeerId};
use regia::net::runtime::{NetNode, ensure_resources, server_add_peer};
use regia::net::settings::NetSettings;
use regia_animation::clip::{JointTrack, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::prelude::*;
use regia_animation::skeleton::LocalBind;
use regia_app::prelude::*;
use regia_app::{FixedTime, TimeUpdateStrategy};
use regia_core::math::{Affine3A, Quat, Ray, Vec3};
use regia_physics::{
    Collider, ColliderShape, CollisionLayer, LayerFile, PhysicsLayers, PhysicsPlugin, RigidBody,
};
use regia_render::ExtractedFrame;
use regia_render::gizmos::{GizmoBuffer, GizmoConfigGroup, GizmoConfigStore, GizmoPlugin};
use regia_scene::{ScenePlugin, Transform};

fn app(bridge: LagCompBridgePlugin) -> App {
    build(bridge, false)
}

/// A server world; `gizmos` adds what a renderer brings. Without it this is a dedicated server,
/// which the bridge must run on.
fn build(bridge: LagCompBridgePlugin, gizmos: bool) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed);
    app.add_plugins((
        regia_app::MinimalPlugins,
        ScenePlugin,
        PhysicsPlugin,
        AnimationPlugin,
    ));
    let world = app.world_mut();
    world.insert_resource(NetMode::DedicatedServer);
    ensure_resources(world);
    if gizmos {
        app.init_resource::<ExtractedFrame>();
        app.add_plugins(GizmoPlugin);
    }
    app.add_plugins(bridge);
    app.finish_plugins();
    app.update();
    app
}

fn collider(half_extents: Vec3) -> Collider {
    Collider {
        shape: ColliderShape::Box { half_extents },
        ..Collider::default()
    }
}

fn wall(app: &mut App, at: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(at),
            RigidBody::static_body(),
            collider(Vec3::new(3.0, 3.0, 0.2)),
        ))
        .id()
}

fn sphere_shapes() -> HitShapes {
    HitShapes {
        shapes: vec![HitShape::new(HitShapeKind::Sphere { radius: 0.5 }, "body")],
        pose_source: PoseSource::Root,
    }
}

/// Records ticks `0..=last`, leaving the server on `last + 1`.
fn record(app: &mut App, last: u32) {
    let world = app.world_mut();
    for t in 0..=last {
        world.resource_mut::<NetNode>().tick = NetTick(t);
        lagcomp_record(world);
    }
    world.resource_mut::<NetNode>().tick = NetTick(last + 1);
}

/// Updates until the physics scene queries see the change.
fn settle(app: &mut App) {
    for _ in 0..4 {
        app.update();
    }
}

fn rewound_hit(app: &App, tick: f64) -> Option<f32> {
    let lc = LagCompensation::new(app.world()).expect("server view");
    let view = lc.at_tick(NetTickFrac(tick)).expect("in history");
    let ray = Ray::new(Vec3::ZERO, Vec3::Z);
    view.raycast(ray, 100.0, &HitFilter::default())
        .expect("query")
        .map(|h| h.distance)
}

/// A wall at the present tick blocks the rewound shot; removing it clears the shot.
#[test]
fn a_present_wall_occludes_a_rewound_target() {
    let mut app = app(LagCompBridgePlugin::default());
    app.world_mut().spawn((
        NetId(1),
        Transform::from_translation(Vec3::new(0.0, 0.0, 10.0)),
        sphere_shapes(),
    ));
    let w = wall(&mut app, Vec3::new(0.0, 0.0, 5.0));
    settle(&mut app);
    record(&mut app, 10);
    assert_eq!(rewound_hit(&app, 5.0), None, "the wall stands between");
    app.world_mut().despawn(w);
    settle(&mut app);
    let d = rewound_hit(&app, 5.0).expect("nothing in the way");
    assert!((d - 9.5).abs() < 1e-3, "{d}");
}

/// A sensor and a target's own colliders (on it or on a child) never occlude: the target's
/// present body is not where it was at the rewound tick.
#[test]
fn sensors_and_the_targets_own_colliders_do_not_occlude() {
    let mut app = app(LagCompBridgePlugin::default());
    let target = app
        .world_mut()
        .spawn((
            NetId(1),
            Transform::from_translation(Vec3::new(0.0, 0.0, 10.0)),
            sphere_shapes(),
            RigidBody::kinematic(),
            collider(Vec3::splat(0.5)),
        ))
        .id();
    app.world_mut()
        .spawn((Transform::default(), collider(Vec3::splat(0.5)), ChildOf(target)));
    let mut sensor = collider(Vec3::new(3.0, 3.0, 0.2));
    sensor.sensor = true;
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, 0.0, 2.0)),
        RigidBody::static_body(),
        sensor,
    ));
    settle(&mut app);
    record(&mut app, 10);
    // The target has run in front of where it was; its colliders are now in the shot's way.
    if let Some(mut t) = app.world_mut().get_mut::<Transform>(target) {
        t.translation.z = 5.0;
    }
    settle(&mut app);
    let d = rewound_hit(&app, 5.0).expect("nothing but its own body in the way");
    assert!((d - 9.5).abs() < 1e-3, "{d}");
}

/// A solid ray starts inside the shooter's own collider: that body has no hit shapes to record
/// it by, and the shot still gets out. The same collider blocks another player.
#[test]
fn the_shooters_own_collider_does_not_occlude_it() {
    let mut app = app(LagCompBridgePlugin::default());
    server_add_peer(app.world_mut(), PeerId(1));
    server_add_peer(app.world_mut(), PeerId(2));
    app.world_mut().spawn((
        NetId(1),
        Transform::from_translation(Vec3::new(0.0, 0.0, 10.0)),
        sphere_shapes(),
    ));
    app.world_mut().spawn((
        Transform::default(),
        RigidBody::kinematic(),
        collider(Vec3::splat(0.5)),
        NetOwner(PeerId(1)),
    ));
    settle(&mut app);
    record(&mut app, 10);
    let lc = LagCompensation::new(app.world()).expect("server view");
    let ray = Ray::new(Vec3::ZERO, Vec3::Z);
    let filter = HitFilter::default();
    let me = NetPlayerId {
        peer: PeerId(1),
        local: 0,
    };
    let other = NetPlayerId {
        peer: PeerId(2),
        local: 0,
    };
    assert!(lc.raycast(me, ray, 100.0, &filter).expect("q").is_some());
    assert!(lc.raycast(other, ray, 100.0, &filter).expect("q").is_none());
}

/// `occlusion_layers` names the layers that stop a shot.
#[test]
fn occlusion_layers_pick_what_stops_a_shot() {
    let mut app = app(LagCompBridgePlugin {
        occlusion_layers: Some(0b10),
        ..LagCompBridgePlugin::default()
    });
    let layers = LayerFile {
        layers: vec!["glass".into(), "world".into()],
        collides: vec![("glass".into(), "glass".into()), ("world".into(), "world".into())],
        queries: Vec::new(),
    };
    app.insert_resource(PhysicsLayers::from_file(&layers).expect("two layers"));
    app.world_mut().spawn((
        NetId(1),
        Transform::from_translation(Vec3::new(0.0, 0.0, 10.0)),
        sphere_shapes(),
    ));
    // Layer 0, not in the mask: glass.
    wall(&mut app, Vec3::new(0.0, 0.0, 5.0));
    settle(&mut app);
    record(&mut app, 10);
    assert!(rewound_hit(&app, 5.0).is_some(), "passes through layer 0");
    let mut on_layer_1 = collider(Vec3::new(3.0, 3.0, 0.2));
    on_layer_1.layer = CollisionLayer::new(1);
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, 0.0, 6.0)),
        RigidBody::static_body(),
        on_layer_1,
    ));
    settle(&mut app);
    assert_eq!(rewound_hit(&app, 5.0), None, "stopped by layer 1");
}

fn head_rig() -> Arc<Skeleton> {
    let at = |y: f32| LocalBind {
        translation: Vec3::new(0.0, y, 0.0),
        ..LocalBind::default()
    };
    Arc::new(
        Skeleton::from_local_bind(
            vec!["root".into(), "head".into()],
            vec![None, Some(0)],
            vec![at(0.5), at(1.0)],
        )
        .expect("valid skeleton"),
    )
}

/// The head slides 2 m along +X over 2 s.
fn head_slide() -> Animator {
    let mut clip = AnimationClip::empty("slide", 2.0);
    clip.tracks.push(JointTrack {
        joint: 1,
        translations: TrackCurve::Animated {
            times: vec![0.0, 2.0],
            values: vec![Vec3::new(0.0, 1.0, 0.0), Vec3::new(2.0, 1.0, 0.0)],
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    Animator::single(clip, true)
}

fn newest_position(app: &App, e: Entity) -> Vec3 {
    let rows = app.world().resource::<LagCompHistory>().history_of(e);
    let (_, _, shapes) = rows.last().expect("recorded");
    Vec3::from_array(shapes[0].position)
}

/// A joint-attached shape follows the animator's pose under the root's fresh transform, with no
/// `Name` on any joint entity.
#[test]
fn joint_shapes_follow_the_animators_pose() {
    let mut app = app(LagCompBridgePlugin::default());
    let binding = spawn_skeleton(app.world_mut(), head_rig());
    let shapes = HitShapes {
        shapes: vec![HitShape::new(HitShapeKind::Sphere { radius: 0.2 }, "head").on_joint("head")],
        pose_source: PoseSource::Joints,
    };
    let e = app
        .world_mut()
        .spawn((
            NetId(1),
            Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)),
            shapes,
            binding,
            head_slide(),
        ))
        .id();
    for _ in 0..30 {
        app.update();
    }
    record(&mut app, 0);
    let pose = app.world().get::<Animator>(e).expect("rig").evaluated_pose();
    let x = pose.translations[1].x;
    assert!(x > 0.1, "the clip has advanced: {x}");
    let at = newest_position(&app, e);
    assert!((at - Vec3::new(10.0 + x, 1.5, 0.0)).length() < 1e-4, "{at}");
    assert_eq!(app.world().resource::<LagCompHistory>().joint_missing_logged, 0);
    // The root moved in this fixed step; the shape comes along at once.
    if let Some(mut t) = app.world_mut().get_mut::<Transform>(e) {
        t.translation.x = 20.0;
    }
    record(&mut app, 1);
    let at = newest_position(&app, e);
    assert!((at - Vec3::new(20.0 + x, 1.5, 0.0)).length() < 1e-4, "{at}");
}

/// The rig may sit on a child of the entity with the hit shapes; a joint the skeleton lacks falls
/// back to the root and logs once.
#[test]
fn a_child_rig_is_found_and_a_missing_joint_falls_back() {
    let mut app = app(LagCompBridgePlugin::default());
    let binding = spawn_skeleton(app.world_mut(), head_rig());
    let shapes = HitShapes {
        shapes: vec![
            HitShape::new(HitShapeKind::Sphere { radius: 0.2 }, "head").on_joint("head"),
            HitShape::new(HitShapeKind::Sphere { radius: 0.2 }, "tail").on_joint("tail"),
        ],
        pose_source: PoseSource::Joints,
    };
    let e = app
        .world_mut()
        .spawn((
            NetId(1),
            Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)),
            shapes,
        ))
        .id();
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, 0.0, 3.0)),
        binding,
        head_slide(),
        ChildOf(e),
    ));
    for _ in 0..4 {
        app.update();
    }
    record(&mut app, 1);
    let rows = app.world().resource::<LagCompHistory>().history_of(e);
    let (_, _, shapes) = rows.last().expect("recorded");
    let head = Vec3::from_array(shapes[0].position);
    assert_eq!(head.z, 3.0, "under the child rig: {head}");
    assert!((head.y - 1.5).abs() < 1e-4, "{head}");
    assert_eq!(Vec3::from_array(shapes[1].position), Vec3::new(10.0, 0.0, 0.0));
    assert_eq!(app.world().resource::<LagCompHistory>().joint_missing_logged, 1);
}

/// A child rig's local moved inside this fixed step shows at once: the pose does not wait for a
/// `PostUpdate` to refresh the globals.
#[test]
fn a_child_rig_moved_this_step_is_recorded_at_its_new_place() {
    let mut app = app(LagCompBridgePlugin::default());
    let binding = spawn_skeleton(app.world_mut(), head_rig());
    let shapes = HitShapes {
        shapes: vec![HitShape::new(HitShapeKind::Sphere { radius: 0.2 }, "head").on_joint("head")],
        pose_source: PoseSource::Joints,
    };
    let e = app
        .world_mut()
        .spawn((
            NetId(1),
            Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)),
            shapes,
        ))
        .id();
    let rig = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 0.0, 3.0)),
            binding,
            head_slide(),
            ChildOf(e),
        ))
        .id();
    for _ in 0..4 {
        app.update();
    }
    record(&mut app, 0);
    let before = newest_position(&app, e);
    assert_eq!(before.z, 3.0);
    if let Some(mut t) = app.world_mut().get_mut::<Transform>(rig) {
        t.translation.z = 7.0;
    }
    record(&mut app, 1);
    let after = newest_position(&app, e);
    assert_eq!(after.z, 7.0, "the new local, not last frame's global: {after}");
}

/// The plugin keeps a host's own hooks.
#[test]
fn host_hooks_are_kept() {
    fn mine(_: &World, _: &Ray, _: f32, _: &[Entity]) -> Option<f32> {
        Some(0.25)
    }
    fn my_joint(_: &World, _: Entity, _: &str) -> Option<Affine3A> {
        Some(Affine3A::IDENTITY)
    }
    let mut host = App::new();
    let world = host.world_mut();
    world.insert_resource(LagCompOcclusion(Some(mine)));
    world.insert_resource(LagCompJointSource(Some(my_joint)));
    host.add_plugins(LagCompBridgePlugin::default());
    let world = host.world();
    let occlusion = world.resource::<LagCompOcclusion>().0.expect("a hook");
    let ray = Ray::new(Vec3::ZERO, Vec3::Z);
    assert_eq!(occlusion(world, &ray, 1.0, &[]), Some(0.25));
    let joint = world.resource::<LagCompJointSource>().0.expect("a hook");
    assert_eq!(joint(world, Entity::PLACEHOLDER, "x"), Some(Affine3A::IDENTITY));
}

/// The gizmo group is registered under its protocol name, off by default, and draws a hit's
/// rewound and current shapes, a miss as a ray alone.
#[test]
fn the_gizmo_group_draws_what_a_query_judged() {
    assert_eq!(NetLagCompGizmos::NAME, "net_lagcomp");
    let server = app(LagCompBridgePlugin::default());
    assert!(!server.world().contains_resource::<GizmoConfigStore>());
    let mut app = build(LagCompBridgePlugin::default(), true);
    let store = app.world().resource::<GizmoConfigStore>();
    let (config, _) = store.config::<NetLagCompGizmos>().expect("registered");
    assert!(!config.enabled, "off until debug.set_gizmos turns it on");

    let e = app
        .world_mut()
        .spawn((
            NetId(1),
            Transform::from_translation(Vec3::new(0.0, 0.0, 10.0)),
            sphere_shapes(),
        ))
        .id();
    record(&mut app, 10);
    {
        let lc = LagCompensation::new(app.world()).expect("server view");
        let view = lc.at_tick(NetTickFrac(5.0)).expect("in history");
        let filter = HitFilter::default();
        assert!(
            view.raycast(Ray::new(Vec3::ZERO, Vec3::Z), 100.0, &filter)
                .expect("q")
                .is_some()
        );
        let away = Ray::new(Vec3::new(9.0, 0.0, 0.0), Vec3::Z);
        assert!(view.raycast(away, 100.0, &filter).expect("q").is_none());
    }
    let history = app.world().resource::<LagCompHistory>();
    let queries = history.last_queries();
    assert_eq!(queries.len(), 2);

    let mut hit = GizmoBuffer::<NetLagCompGizmos>::default();
    draw_queries(&mut hit, &queries[..1], |bits| {
        assert_eq!(bits, e.to_bits());
        history.history_of(e).pop().map(|r| r.2).unwrap_or_default()
    });
    let mut rewound_only = GizmoBuffer::<NetLagCompGizmos>::default();
    draw_queries(&mut rewound_only, &queries[..1], |_| Vec::new());
    let vertices = |b: &GizmoBuffer<NetLagCompGizmos>| b.list_positions.len() + b.strip_positions.len();
    assert!(
        vertices(&hit) > vertices(&rewound_only),
        "the current shapes are drawn too"
    );
    assert!(hit.list_positions.len() >= 2, "the ray and the hit cross");

    let mut miss = GizmoBuffer::<NetLagCompGizmos>::default();
    draw_queries(&mut miss, &queries[1..], |_| panic!("a miss has no entity"));
    assert_eq!(miss.list_positions.len(), 2, "a ray");
    assert!(miss.strip_positions.is_empty());
}

// ---- exact-tick joints (ADR-2551) ------------------------------------------------------------

/// The demo character's eight hit shapes: a body capsule on the root, the rest on joints.
fn demo_shapes() -> HitShapes {
    let sphere = |r: f32| HitShapeKind::Sphere { radius: r };
    let capsule = |r: f32, h: f32| HitShapeKind::Capsule {
        radius: r,
        half_height: h,
    };
    HitShapes {
        shapes: vec![
            HitShape::new(capsule(0.3, 0.6), "body").at(Vec3::new(0.0, 0.9, 0.0)),
            HitShape::new(sphere(0.12), "head")
                .on_joint("head")
                .at(Vec3::new(0.0, 0.08, 0.0)),
            HitShape::new(capsule(0.16, 0.15), "chest").on_joint("chest"),
            HitShape::new(sphere(0.15), "hips").on_joint("hips"),
            HitShape::new(capsule(0.05, 0.12), "limb").on_joint("left_forearm"),
            HitShape::new(capsule(0.05, 0.12), "limb").on_joint("right_forearm"),
            HitShape::new(capsule(0.07, 0.2), "limb").on_joint("left_calf"),
            HitShape::new(capsule(0.07, 0.2), "limb").on_joint("right_calf"),
        ],
        pose_source: PoseSource::Joints,
    }
}

fn demo_target(app: &mut App, at: Vec3) -> Entity {
    let (skeleton, graph, clips) = regia_animation::pose_eval::demo::demo_net_character().expect("demo rig");
    let binding = spawn_skeleton(app.world_mut(), Arc::new(skeleton));
    app.world_mut()
        .spawn((
            NetId(7),
            Transform::from_translation(at),
            demo_shapes(),
            binding,
            Animator::graph(graph, clips),
        ))
        .id()
}

fn drive(app: &mut App, e: Entity, f: u32) {
    use regia_animation::pose_eval::demo::{AIM, JUMP, SPEED};
    let mut animator = app.world_mut().get_mut::<Animator>(e).expect("animator");
    animator.set(SPEED, (f as f32 * 0.05).min(3.2));
    animator.set(AIM, if (40..70).contains(&f) { 1.0 } else { 0.0 });
    if f == 50 {
        animator.trigger(JUMP);
    }
}

/// The shapes the live animator's pose of this frame puts in the world, in definition order.
fn live_shapes(app: &App, e: Entity) -> Vec<Vec3> {
    let world = app.world();
    let animator = world.get::<Animator>(e).expect("animator");
    let skinned = world.get::<SkinnedMesh>(e).expect("rig");
    let t = world.get::<Transform>(e).expect("transform");
    let root = Affine3A::from_scale_rotation_translation(t.scale, t.rotation, t.translation);
    let globals = skinned.skeleton.globals(animator.evaluated_pose(), None);
    demo_shapes()
        .shapes
        .iter()
        .map(|s| {
            let at = match &s.attach {
                regia::net::lagcomp::HitAttach::Joint(name) => {
                    let j = skinned.skeleton.joint_index(name.as_str()).expect("joint") as usize;
                    root * Affine3A::from_mat4(globals[j])
                }
                regia::net::lagcomp::HitAttach::Root => root,
            };
            let (_, rot, pos) = at.to_scale_rotation_translation();
            pos + rot.normalize() * s.offset.translation
        })
        .collect()
}

fn rewound_positions(app: &App, e: Entity, tick: f64) -> (Vec<Vec3>, &'static str) {
    let lc = LagCompensation::new(app.world()).expect("server view");
    let r = lc.rewound_shapes(e, NetTickFrac(tick)).expect("rewound");
    (
        r.shapes.iter().map(|s| Vec3::from_array(s.position)).collect(),
        r.source,
    )
}

/// Runs 90 frames; each one records tick `f` (the end of the fixed step), then evaluates the
/// animation of the frame. Returns, per tick from 2 on, the largest distance between a rewound
/// shape at that exact tick and the shape the live animator put there that frame.
fn exact_tick_errors(exact: bool) -> (Vec<f32>, &'static str) {
    let mut app = app(LagCompBridgePlugin {
        exact_tick_joints: exact,
        ..LagCompBridgePlugin::default()
    });
    let e = demo_target(&mut app, Vec3::new(1.0, 0.0, 5.0));
    let mut errors = Vec::new();
    let mut source = "";
    for f in 0..90u32 {
        drive(&mut app, e, f);
        {
            let world = app.world_mut();
            world.resource_mut::<NetNode>().tick = NetTick(f);
            lagcomp_record(world);
            world.resource_mut::<NetNode>().tick = NetTick(f + 1);
        }
        app.update();
        if f < 2 {
            continue;
        }
        let live = live_shapes(&app, e);
        let (rewound, s) = rewound_positions(&app, e, f64::from(f));
        source = s;
        let worst = live
            .iter()
            .zip(&rewound)
            .map(|(a, b)| a.distance(*b))
            .fold(0.0f32, f32::max);
        errors.push(worst);
    }
    (errors, source)
}

/// Spec 75 § 4, ADR-2551: with exact-tick joints a rewind poses bone-attached shapes where the
/// animation had the bones on that server tick (within float composition, 10 µm), every tick,
/// through walk, run, a jump and an eased upper-body layer. The default history poses them from
/// the last evaluated pose, a frame behind.
#[test]
fn exact_tick_joints_match_the_live_animation_of_the_tick() {
    let (exact, source) = exact_tick_errors(true);
    assert_eq!(source, "anim_state");
    let worst = exact.iter().copied().fold(0.0f32, f32::max);
    assert!(
        worst < 1e-5,
        "exact-tick joints differ from the live pose by {worst} m"
    );
    let (stale, source) = exact_tick_errors(false);
    assert_eq!(source, "recorded");
    let worst_stale = stale.iter().copied().fold(0.0f32, f32::max);
    assert!(
        worst_stale > 5e-3,
        "the default history is a frame behind on a running rig: {worst_stale} m"
    );
}

/// Records `frames` ticks of the demo target with animation state, the rewind window capped at
/// `max_rewind_ms` (`None`: the default), and returns (ticks kept, anim entities, the target's bytes).
fn anim_history(max_rewind_ms: Option<f64>, frames: u32) -> (usize, usize, usize) {
    let mut app = app(LagCompBridgePlugin {
        exact_tick_joints: true,
        ..LagCompBridgePlugin::default()
    });
    if let Some(ms) = max_rewind_ms {
        app.world_mut()
            .resource_mut::<NetSettings>()
            .lag_compensation
            .max_rewind_ms = ms;
    }
    let e = demo_target(&mut app, Vec3::ZERO);
    for f in 0..frames {
        drive(&mut app, e, f);
        let world = app.world_mut();
        world.resource_mut::<NetNode>().tick = NetTick(f);
        lagcomp_record(world);
        app.update();
    }
    let history = app.world().resource::<LagCompHistory>();
    (
        history.ticks_kept(),
        history.anim_entities(),
        history.entity_bytes(e),
    )
}

/// Spec 75 § 14: the history of one entity with eight shapes stays within
/// `ticks × (8 × 40 + 24)` bytes when it holds animation state. At spec 75's 250 ms window that is
/// 17 ticks at 60 Hz (5,848 bytes). Spec 120 § 9.6 (ADR-8315) raised the default cap to 1,000 ms,
/// so the default history is 62 ticks deep; the per-tick budget holds there too. Before ADR-8315
/// this test ran on the default and read 17 ticks; it now sets spec 75's window for that figure.
#[test]
fn the_anim_state_history_stays_within_the_spec_75_budget() {
    const PER_TICK: usize = 8 * 40 + 24;
    let (ticks, anim, bytes) = anim_history(Some(250.0), 40);
    assert_eq!(ticks, 17, "ceil(250 ms / 16.7 ms) + 2");
    assert_eq!(anim, 1);
    let budget = 17 * PER_TICK;
    assert!(bytes <= budget, "{bytes} bytes for 17 ticks, budget {budget}");
    eprintln!("anim-state history: {bytes} bytes for 17 ticks (budget {budget})");

    let (ticks, anim, bytes) = anim_history(None, 80);
    assert_eq!(ticks, 62, "the default 1,000 ms cap: 60 + 2 ticks");
    assert_eq!(anim, 1);
    let budget = ticks * PER_TICK;
    assert!(
        bytes <= budget,
        "{bytes} bytes for {ticks} ticks, budget {budget}"
    );
    eprintln!("anim-state history: {bytes} bytes for {ticks} ticks (budget {budget})");
}

/// A target despawned since still poses from its recorded state (the record holds what the
/// evaluation needs), reported `alive: false`; between two ticks the shapes blend.
#[test]
fn a_despawned_target_poses_from_its_recorded_state() {
    let mut app = app(LagCompBridgePlugin {
        exact_tick_joints: true,
        ..LagCompBridgePlugin::default()
    });
    let e = demo_target(&mut app, Vec3::ZERO);
    for f in 0..20u32 {
        drive(&mut app, e, f);
        let world = app.world_mut();
        world.resource_mut::<NetNode>().tick = NetTick(f);
        lagcomp_record(world);
        world.resource_mut::<NetNode>().tick = NetTick(f + 1);
        app.update();
    }
    let (at_15, _) = rewound_positions(&app, e, 15.0);
    let (at_16, _) = rewound_positions(&app, e, 16.0);
    let (half, _) = rewound_positions(&app, e, 15.5);
    for ((a, b), m) in at_15.iter().zip(&at_16).zip(&half) {
        assert!((a.lerp(*b, 0.5) - *m).length() < 1e-5, "a half tick is halfway");
    }
    assert!(app.world_mut().despawn(e));
    let lc = LagCompensation::new(app.world()).expect("server view");
    let r = lc.rewound_shapes(e, NetTickFrac(15.0)).expect("still in history");
    assert!(!r.alive);
    assert_eq!(r.source, "anim_state");
    for (s, a) in r.shapes.iter().zip(&at_15) {
        assert_eq!(
            Vec3::from_array(s.position),
            *a,
            "the same pose after the despawn"
        );
    }
}

/// Eight joint hit shapes on a rig that sits on the root. One record's positions are the joint
/// hook's own translations (zero shape offset).
#[test]
fn eight_joint_shapes_match_animator_joint() {
    let mut app = app(LagCompBridgePlugin::default());
    let names: Vec<String> = (0..8).map(|i| format!("j{i}")).collect();
    let parents: Vec<Option<u32>> = (0..8).map(|i| if i == 0 { None } else { Some(0) }).collect();
    let binds: Vec<LocalBind> = (0..8)
        .map(|i| LocalBind {
            translation: Vec3::new(i as f32 * 0.4, 0.2, 0.0),
            ..LocalBind::default()
        })
        .collect();
    let skeleton = Arc::new(Skeleton::from_local_bind(names, parents, binds).expect("valid skeleton"));
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let shapes = HitShapes {
        shapes: (0..8)
            .map(|i| HitShape::new(HitShapeKind::Sphere { radius: 0.05 }, "joint").on_joint(&format!("j{i}")))
            .collect(),
        pose_source: PoseSource::Joints,
    };
    let entity = app
        .world_mut()
        .spawn((
            NetId(1),
            Transform::from_translation(Vec3::new(2.0, 0.0, 0.0)),
            shapes,
            binding,
            Animator::single(AnimationClip::empty("c", 1.0), true),
        ))
        .id();
    for _ in 0..4 {
        app.update();
    }
    record(&mut app, 0);
    let reports = app
        .world()
        .resource::<LagCompHistory>()
        .newest_shapes_of(entity)
        .expect("recorded");
    assert_eq!(reports.len(), 8);
    for (i, report) in reports.iter().enumerate() {
        let joint = animator_joint(app.world(), entity, &format!("j{i}")).expect("joint");
        let (_, _, pos) = joint.to_scale_rotation_translation();
        let recorded = Vec3::from_array(report.position);
        assert!(
            (recorded - pos).length() < 1e-5,
            "joint {i}: recorded {recorded} hook {pos}"
        );
    }
}

/// Two raycasts of one view, and two reads of the newest shapes, agree.
#[test]
fn two_queries_of_one_tick_return_the_same_hit() {
    let mut app = app(LagCompBridgePlugin {
        exact_tick_joints: true,
        ..LagCompBridgePlugin::default()
    });
    let entity = demo_target(&mut app, Vec3::new(0.0, 0.0, 4.0));
    for f in 0..8u32 {
        drive(&mut app, entity, f);
        app.update();
    }
    record(&mut app, 4);
    let lc = LagCompensation::new(app.world()).expect("server view");
    let view = lc.at_tick(NetTickFrac(4.0)).expect("in history");
    // The body capsule sits 0.9 m above the root. The ray meets that center.
    let ray = Ray::new(Vec3::new(0.0, 0.9, 0.0), Vec3::Z);
    let filter = HitFilter::default();
    let first = view.raycast(ray, 100.0, &filter).expect("query");
    let second = view.raycast(ray, 100.0, &filter).expect("query");
    assert_eq!(first, second, "two raycasts of one view");
    assert!(first.is_some(), "the demo body is on the ray");
    let history = app.world().resource::<LagCompHistory>();
    let once = history.newest_shapes_of(entity).expect("shapes");
    let twice = history.newest_shapes_of(entity).expect("shapes");
    assert_eq!(once.len(), 8);
    assert_eq!(
        once, twice,
        "a second read of the newest tick poses the same shapes"
    );
}

/// The `net_lagcomp` gizmo system opens its span while the group is on. The headless span audit
/// leaves the group off, so this name stays out of that list.
#[test]
fn the_lagcomp_gizmo_span_opens_when_the_group_is_on() {
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    #[derive(Clone, Default)]
    struct Recorder {
        names: Arc<Mutex<BTreeSet<&'static str>>>,
        next: Arc<AtomicU64>,
    }

    impl Subscriber for Recorder {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
            Some(tracing::level_filters::LevelFilter::TRACE)
        }
        fn new_span(&self, attrs: &Attributes<'_>) -> Id {
            if let Ok(mut names) = self.names.lock() {
                names.insert(attrs.metadata().name());
            }
            Id::from_u64(self.next.fetch_add(1, Ordering::Relaxed) + 1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, _: &Event<'_>) {}
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    let recorder = Recorder::default();
    let names = recorder.names.clone();
    tracing::subscriber::with_default(recorder, || {
        tracing::callsite::rebuild_interest_cache();
        let mut app = build(LagCompBridgePlugin::default(), true);
        {
            let mut store = app.world_mut().resource_mut::<GizmoConfigStore>();
            let (config, _) = store.config_mut::<NetLagCompGizmos>().expect("registered");
            config.enabled = true;
        }
        // `with_default` sets this thread's subscriber only. The multithreaded executor runs the
        // gizmo system on a worker thread, where its span goes to the global (no-op) subscriber,
        // so every schedule runs on this thread here.
        for (_, schedule) in app
            .world_mut()
            .resource_mut::<regia_ecs::ecs::schedule::Schedules>()
            .iter_mut()
        {
            schedule.set_executor(regia_ecs::ecs::schedule::SingleThreadedExecutor::new());
        }
        app.update();
    });
    let seen = names.lock().map(|n| n.clone()).unwrap_or_default();
    assert!(
        seen.contains("net_lagcomp_gizmos"),
        "the group was on and the span did not open; seen: {seen:?}"
    );
}
