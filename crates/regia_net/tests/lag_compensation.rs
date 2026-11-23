// SPDX-License-Identifier: MPL-2.0
//! Spec 75 § 13 (build record spec 90 § 7): lag compensation, on a server world at fixed ticks and
//! end to end over the `LoopbackHub` with a client's real interpolation clock. No wall clock.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::Arc;

use glam::{Quat, Vec3};
use regia_core::math::Ray;
use regia_ecs::ecs::system::RunSystemOnce;
use regia_ecs::prelude::*;
use regia_net::interp::{
    NetInterpolation, ViewTickWire, install_interpolation, net_interpolate, set_view_tick,
};
use regia_net::lagcomp::{
    LagCompAnimSource, RecordedAnim, ShooterCounters, install_lag_compensation, lagcomp_record,
};
use regia_net::model::NetOwner;
use regia_net::runtime::{NetEntities, NetNode, ensure_resources, server_add_peer};
use regia_net::testing::LoopbackHub;
use regia_net::*;
use regia_scene::components::Name;
use regia_scene::transform::Transform;

const HZ: f64 = 60.0;
const MSPT: f64 = 1000.0 / HZ;

// ---- a server world driven tick by tick ---------------------------------------------------------

struct Server {
    world: World,
    next_id: u32,
}

const PEER: PeerId = PeerId(1);
const OTHER: PeerId = PeerId(2);

fn shooter() -> NetPlayerId {
    NetPlayerId { peer: PEER, local: 0 }
}

fn other_shooter() -> NetPlayerId {
    NetPlayerId {
        peer: OTHER,
        local: 0,
    }
}

impl Server {
    fn new() -> Self {
        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        ensure_resources(&mut world);
        install_lag_compensation(&mut world);
        server_add_peer(&mut world, PEER);
        server_add_peer(&mut world, OTHER);
        Self { world, next_id: 1 }
    }

    fn spawn(&mut self, at: Vec3, shapes: Vec<HitShape>) -> Entity {
        let id = NetId(self.next_id);
        self.next_id += 1;
        self.world
            .spawn((
                id,
                Transform::from_translation(at),
                HitShapes {
                    shapes,
                    pose_source: PoseSource::Root,
                },
            ))
            .id()
    }

    fn body(&mut self, at: Vec3) -> Entity {
        self.spawn(at, vec![body()])
    }

    fn move_to(&mut self, e: Entity, at: Vec3) {
        if let Some(mut t) = self.world.get_mut::<Transform>(e) {
            t.translation = at;
        }
    }

    /// Ends tick `tick`: records it, then the server moves on to `tick + 1`.
    fn end_tick(&mut self, tick: u32) {
        self.world.resource_mut::<NetNode>().tick = NetTick(tick);
        lagcomp_record(&mut self.world);
        self.world.resource_mut::<NetNode>().tick = NetTick(tick + 1);
    }

    /// The shooter's view tick (spawns its `Player` entity once).
    fn view(&mut self, who: NetPlayerId, tick: f64) {
        let mut q = self.world.query::<(Entity, &NetPlayer)>();
        let found = q.iter(&self.world).find(|(_, p)| p.id == who).map(|(e, _)| e);
        let e = found.unwrap_or_else(|| self.world.spawn(NetPlayer { id: who }).id());
        self.world.entity_mut(e).insert(NetViewTick {
            tick,
            delay_ms: 100.0,
        });
    }

    fn lc(&self) -> LagCompensation<'_> {
        LagCompensation::new(&self.world).expect("server view")
    }

    fn settings(&mut self) -> Mut<'_, NetSettings> {
        self.world.resource_mut::<NetSettings>()
    }
}

fn body() -> HitShape {
    HitShape::new(
        HitShapeKind::Capsule {
            radius: 0.4,
            half_height: 0.5,
        },
        "body",
    )
    .at(Vec3::new(0.0, 1.0, 0.0))
}

fn ray_z(x: f32) -> Ray {
    Ray::new(Vec3::new(x, 1.0, -10.0), Vec3::Z)
}

fn none() -> HitFilter {
    HitFilter::default()
}

// ---- § 13 test 1: the point of it, end to end -----------------------------------------------------

#[derive(Component)]
struct Target;

/// Shots the server consumes on a later tick: (deliver at hub step, view tick wire, ray).
#[derive(Resource, Default)]
struct Shots {
    step: u64,
    queue: VecDeque<(u64, ViewTickWire, Ray)>,
    hits: u32,
    misses: u32,
    body_hits: u32,
}

/// Triangle wave between -8 and 8 m at 6 m/s.
fn strafe_x(tick: u32) -> f32 {
    let leg = (16.0 / 6.0 * HZ) as u32; // ticks per leg
    let phase = tick % (2 * leg);
    let s = if phase < leg { phase } else { 2 * leg - phase };
    -8.0 + 16.0 * s as f32 / leg as f32
}

fn server_shots(world: &mut World) {
    let step = world.resource::<Shots>().step;
    let player = {
        let mut q = world.query_filtered::<Entity, With<NetPlayer>>();
        q.iter(world).next()
    };
    loop {
        let due = {
            let shots = world.resource::<Shots>();
            shots.queue.front().is_some_and(|(at, _, _)| *at <= step)
        };
        if !due {
            break;
        }
        let Some((_, wire, ray)) = world.resource_mut::<Shots>().queue.pop_front() else {
            break;
        };
        if let Some(p) = player {
            set_view_tick(world, p, Some(wire));
        }
        let hit = LagCompensation::new(world)
            .expect("server")
            .raycast(shooter(), ray, 100.0, &HitFilter::default())
            .expect("query");
        let mut shots = world.resource_mut::<Shots>();
        match hit {
            Some(h) => {
                shots.hits += 1;
                shots.body_hits += u32::from(h.tag.as_str() == "body");
            }
            None => shots.misses += 1,
        }
    }
}

fn server_strafe(world: &mut World) {
    let tick = world.resource::<NetNode>().tick.0;
    let mut q = world.query_filtered::<&mut Transform, With<Target>>();
    for mut t in q.iter_mut(world) {
        t.translation.x = strafe_x(tick);
    }
}

fn setup(_world: &mut World) {}

/// Runs 100 shots from a client at 6-tick round-trip latency, each aimed exactly at the target's
/// shown center; returns (hits, misses, body hits).
fn shoot_strafing_target(rewind: bool) -> (u32, u32, u32) {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 75, setup);
    hub.down.delay_ticks = 3;
    let up_ticks = 3u64;
    let c = hub.add_client();
    let peer = hub.clients[c].peer;
    install_interpolation(&mut hub.clients[c].world, HZ, 1.0);
    let sw = &mut hub.server.world;
    install_lag_compensation(sw);
    sw.resource_mut::<NetSettings>().lag_compensation.enabled = rewind;
    sw.init_resource::<Shots>();
    hub.server
        .gameplay
        .add_systems((server_shots, server_strafe, lagcomp_record).chain());
    let target = hub
        .server
        .world
        .spawn((
            Target,
            Replicated::default(),
            Transform::from_translation(Vec3::new(strafe_x(0), 0.0, 0.0)),
            HitShapes {
                shapes: vec![body()],
                pose_source: PoseSource::Root,
            },
        ))
        .id();
    hub.server.world.spawn(NetPlayer {
        id: NetPlayerId { peer, local: 0 },
    });
    let origin = Vec3::new(0.0, 1.0, -12.0);
    let mut fired = 0u32;
    for step in 1..=2000u64 {
        let now = step as f64 * MSPT;
        hub.clients[c].world.resource_mut::<NetInterpolation>().now_ms = now;
        hub.server.world.resource_mut::<Shots>().step = step;
        hub.step();
        let cw = &mut hub.clients[c].world;
        net_interpolate(cw, now);
        if step < 120 || step % 3 != 0 || fired >= 100 {
            continue;
        }
        let Some(id) = hub.server.world.get::<NetId>(target).copied() else {
            continue;
        };
        let cw = &hub.clients[c].world;
        let Some(copy) = cw.resource::<NetEntities>().entity(id) else {
            continue;
        };
        let ni = cw.resource::<NetInterpolation>();
        let (Some(interp), Some(delay)) = (ni.clock.interp_tick(), ni.clock.delay_ms(now)) else {
            continue;
        };
        let shown = cw.get::<Transform>(copy).expect("copy transform").translation + Vec3::Y;
        let ray = Ray::new(origin, shown - origin);
        hub.server.world.resource_mut::<Shots>().queue.push_back((
            step + up_ticks,
            ViewTickWire::new(interp, delay),
            ray,
        ));
        fired += 1;
    }
    let shots = hub.server.world.resource::<Shots>();
    assert!(shots.queue.is_empty(), "every shot was judged");
    assert_eq!(fired, 100);
    (shots.hits, shots.misses, shots.body_hits)
}

/// § 13 test 1: a target strafing at 6 m/s, a client at 6-tick latency (and its adaptive
/// interpolation delay) firing exactly at the target's shown center: with lag compensation the
/// server reports 100 of 100 hits on `body`; with rewind off the same shots miss at least 90 times.
#[test]
fn strafing_target_is_hit_where_the_shooter_saw_it() {
    let (hits, misses, body) = shoot_strafing_target(true);
    assert_eq!((hits, misses, body), (100, 0, 100), "with rewind");
    let (_, misses, _) = shoot_strafing_target(false);
    assert!(misses >= 90, "without rewind only {misses} of 100 shots missed");
}

// ---- § 13 tests 2-11 on a server world ------------------------------------------------------------

/// § 13 test 2: a shape posed at tick 10.5 sits exactly halfway between its tick-10 and tick-11
/// poses, in position and rotation.
#[test]
fn fractional_tick_is_halfway() {
    let mut s = Server::new();
    let bar = s.spawn(
        Vec3::ZERO,
        vec![HitShape::new(
            HitShapeKind::Box {
                half_extents: Vec3::new(1.0, 0.1, 0.1),
            },
            "bar",
        )],
    );
    let ball = s.spawn(
        Vec3::new(0.0, 0.0, 20.0),
        vec![HitShape::new(HitShapeKind::Sphere { radius: 0.5 }, "ball")],
    );
    for t in 0..=10 {
        s.end_tick(t);
    }
    s.move_to(ball, Vec3::new(1.0, 0.0, 20.0));
    if let Some(mut t) = s.world.get_mut::<Transform>(bar) {
        t.rotation = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
    }
    s.end_tick(11);
    let lc = s.lc();
    let view = lc.at_tick(NetTickFrac(10.5)).expect("in history");
    // Position: straight down onto the ball's top at x = 0.5.
    let h = view
        .raycast(Ray::new(Vec3::new(0.5, 10.0, 20.0), -Vec3::Y), 100.0, &none())
        .expect("query")
        .expect("hit");
    assert!((h.distance - 9.5).abs() < 1e-4, "distance {}", h.distance);
    assert!(h.normal.distance(Vec3::Y) < 1e-4, "normal {:?}", h.normal);
    assert_eq!(h.rewound_tick, NetTickFrac(10.5));
    // Rotation: the bar points along 45 degrees about Y, (cos 45, 0, -sin 45).
    let d = Vec3::new(1.0, 0.0, -1.0).normalize() * 0.8;
    let down = |p: Vec3| Ray::new(p + Vec3::Y * 10.0, -Vec3::Y);
    assert!(
        view.raycast(down(d), 100.0, &none()).expect("q").is_some(),
        "on the 45-degree axis"
    );
    assert!(
        view.raycast(down(Vec3::new(0.8, 0.0, 0.0)), 100.0, &none())
            .expect("q")
            .is_none(),
        "not on the tick-10 axis"
    );
    assert!(
        view.raycast(down(Vec3::new(0.0, 0.0, -0.8)), 100.0, &none())
            .expect("q")
            .is_none(),
        "not on the tick-11 axis"
    );
}

/// § 13 test 3: a view tick older than the limit is clamped to it and counted; a view tick newer
/// than the server tick is clamped to the present and counted.
#[test]
fn view_ticks_are_clamped_and_counted() {
    let mut s = Server::new();
    // The window this test was written for (the default before spec 120 § 9.6 raised it to 1,000).
    s.settings().lag_compensation.max_rewind_ms = 250.0;
    s.spawn(
        Vec3::new(0.0, 0.0, 10.0),
        vec![HitShape::new(HitShapeKind::Sphere { radius: 50.0 }, "wall")],
    );
    for t in 0..=40 {
        s.end_tick(t);
    }
    // Current tick 41, window 15 ticks: the oldest judged view is 26.
    assert_eq!(s.world.resource::<LagCompHistory>().ticks_kept(), 17);
    s.view(shooter(), 5.0);
    let h = s
        .lc()
        .raycast(shooter(), ray_z(0.0), 100.0, &none())
        .expect("q")
        .expect("hit");
    assert_eq!(h.rewound_tick, NetTickFrac(26.0));
    s.view(shooter(), 99.0);
    let h = s
        .lc()
        .raycast(shooter(), ray_z(0.0), 100.0, &none())
        .expect("q")
        .expect("hit");
    assert_eq!(h.rewound_tick, NetTickFrac(41.0));
    s.view(shooter(), 33.25);
    let h = s
        .lc()
        .raycast(shooter(), ray_z(0.0), 100.0, &none())
        .expect("q")
        .expect("hit");
    assert_eq!(h.rewound_tick, NetTickFrac(33.25));
    let c = s.world.resource::<LagCompHistory>().shooter_counters(shooter());
    assert_eq!((c.queries, c.clamped), (3, 2));
    let log = s.world.resource::<LagCompHistory>().last_queries();
    assert_eq!(
        log.iter().map(|q| q.clamped).collect::<Vec<_>>(),
        vec![true, true, false]
    );
    // An explicit tick outside the history is an error, not a clamp.
    assert_eq!(
        s.lc().at_tick(NetTickFrac(10.0)).err(),
        Some(LagCompError::TickOutOfHistory)
    );
    // Offline and the listen host's own shots are judged at the present.
    s.view(
        NetPlayerId {
            peer: PeerId::SERVER,
            local: 0,
        },
        30.0,
    );
    let host = NetPlayerId {
        peer: PeerId::SERVER,
        local: 0,
    };
    assert_eq!(s.lc().rewind_tick(host), Ok(NetTickFrac(41.0)));
    assert_eq!(
        s.lc()
            .rewind_tick(NetPlayerId {
                peer: PeerId(9),
                local: 0
            })
            .err(),
        Some(LagCompError::PeerUnknown)
    );
}

thread_local! {
    /// The ignore list of the last hook call on this test's thread.
    static WALL_IGNORED: RefCell<Vec<Entity>> = const { RefCell::new(Vec::new()) };
}

#[derive(Resource)]
struct WallAt(f32);

/// A wall plane at `z = WallAt`, across everything.
fn wall_hook(world: &World, ray: &Ray, max_dist: f32, ignore: &[Entity]) -> Option<f32> {
    WALL_IGNORED.with(|v| *v.borrow_mut() = ignore.to_vec());
    let z = world.get_resource::<WallAt>()?.0;
    if ray.direction.z.abs() < 1e-6 {
        return None;
    }
    let t = (z - ray.origin.z) / ray.direction.z;
    (t >= 0.0 && t <= max_dist).then_some(t)
}

/// § 13 test 4: a wall built at the current tick between the shooter and the rewound target
/// blocks the hit; the hook is told to ignore the lag-compensated entities' own colliders.
#[test]
fn present_wall_occludes_rewound_target() {
    let mut s = Server::new();
    let target = s.body(Vec3::new(0.0, 0.0, 10.0));
    for t in 0..=10 {
        s.end_tick(t);
    }
    s.view(shooter(), 8.0);
    s.world.resource_mut::<LagCompOcclusion>().0 = Some(wall_hook);
    assert!(
        s.lc()
            .raycast(shooter(), ray_z(0.0), 100.0, &none())
            .expect("q")
            .is_some()
    );
    assert!(WALL_IGNORED.with(|v| v.borrow().contains(&target)));
    s.world.insert_resource(WallAt(3.0));
    assert!(
        s.lc()
            .raycast(shooter(), ray_z(0.0), 100.0, &none())
            .expect("q")
            .is_none()
    );
    assert!(
        s.lc()
            .sphere_cast(shooter(), ray_z(0.0), 0.2, 100.0, &none())
            .expect("q")
            .is_none()
    );
    // A wall behind the target does not block it.
    s.world.insert_resource(WallAt(20.0));
    assert!(
        s.lc()
            .raycast(shooter(), ray_z(0.0), 100.0, &none())
            .expect("q")
            .is_some()
    );
    // Overlaps drop hits whose line from the center is blocked.
    s.world.insert_resource(WallAt(9.0));
    let center = Vec3::new(0.0, 1.0, 8.0);
    assert!(
        s.lc()
            .overlap_sphere(shooter(), center, 3.0, &none())
            .expect("q")
            .is_empty()
    );
    s.world.remove_resource::<WallAt>();
    assert_eq!(
        s.lc()
            .overlap_sphere(shooter(), center, 3.0, &none())
            .expect("q")
            .len(),
        1
    );
}

/// § 13 test 5: a ray starting inside the shooter's own capsule hits the target behind it, never
/// the shooter; another player's shot does hit that capsule.
#[test]
fn shooter_never_hits_itself() {
    let mut s = Server::new();
    let pawn = s.body(Vec3::new(0.0, 0.0, -10.0));
    s.world.entity_mut(pawn).insert(NetOwner(PEER));
    let target = s.body(Vec3::new(0.0, 0.0, 10.0));
    for t in 0..=10 {
        s.end_tick(t);
    }
    s.view(shooter(), 9.0);
    s.view(other_shooter(), 9.0);
    let h = s
        .lc()
        .raycast(shooter(), ray_z(0.0), 100.0, &none())
        .expect("q")
        .expect("hit");
    assert_eq!(h.entity, target);
    let h = s
        .lc()
        .raycast(other_shooter(), ray_z(0.0), 100.0, &none())
        .expect("q")
        .expect("hit");
    assert_eq!(h.entity, pawn);
    assert_eq!(h.distance, 0.0, "starts inside the pawn");
    // The filter excludes, groups and predicates apply too.
    let skip_target = HitFilter {
        exclude: vec![target],
        ..HitFilter::default()
    };
    assert!(
        s.lc()
            .raycast(shooter(), ray_z(0.0), 100.0, &skip_target)
            .expect("q")
            .is_none()
    );
    let red = HitFilter {
        groups: Some(NetGroups(vec!["red".into()])),
        ..HitFilter::default()
    };
    assert!(
        s.lc()
            .raycast(shooter(), ray_z(0.0), 100.0, &red)
            .expect("q")
            .is_none()
    );
    // Groups are judged as recorded: at the rewound tick the target had not joined yet.
    s.world.entity_mut(target).insert(NetGroups(vec!["red".into()]));
    assert!(
        s.lc()
            .raycast(shooter(), ray_z(0.0), 100.0, &red)
            .expect("q")
            .is_none()
    );
    s.end_tick(11);
    s.end_tick(12);
    s.view(shooter(), 12.0);
    assert!(
        s.lc()
            .raycast(shooter(), ray_z(0.0), 100.0, &red)
            .expect("q")
            .is_some()
    );
}

/// § 13 test 6: rewinding across a teleport uses the nearest side's pose, never a point between
/// the two places; without the teleport mark the same move interpolates.
#[test]
fn teleport_is_a_cut() {
    for teleport in [true, false] {
        let mut s = Server::new();
        let e = s.body(Vec3::ZERO);
        for t in 0..=10 {
            s.end_tick(t);
        }
        s.move_to(e, Vec3::new(100.0, 0.0, 0.0));
        if teleport {
            net_teleport(&mut s.world, e);
        }
        s.end_tick(11);
        s.end_tick(12);
        let lc = s.lc();
        let at = |tick: f64, x: f32| {
            lc.at_tick(NetTickFrac(tick))
                .expect("in history")
                .raycast(ray_z(x), 100.0, &none())
                .expect("q")
                .is_some()
        };
        if teleport {
            assert!(at(10.4, 0.0) && !at(10.4, 40.0));
            assert!(at(10.6, 100.0) && !at(10.6, 60.0));
            for f in [10.1, 10.3, 10.5, 10.7, 10.9] {
                for x in [10.0, 30.0, 50.0, 70.0, 90.0] {
                    assert!(!at(f, x), "nothing between the two places at {f}, x = {x}");
                }
            }
            let hist = s.world.resource::<LagCompHistory>().history_of(e);
            assert_eq!(
                hist.iter()
                    .filter(|(_, cut, _)| *cut)
                    .map(|(t, _, _)| t.0)
                    .collect::<Vec<_>>(),
                vec![11]
            );
        } else {
            assert!(
                at(10.4, 40.0) && at(10.5, 50.0) && at(10.6, 60.0),
                "an ordinary move interpolates"
            );
            assert!(!at(10.4, 0.0) && !at(10.6, 100.0));
        }
    }
}

/// § 13 test 7: a target despawned 3 ticks ago is hit at its rewound pose with `alive: false`.
#[test]
fn despawned_target_is_hit_but_not_alive() {
    let mut s = Server::new();
    let e = s.body(Vec3::new(0.0, 0.0, 10.0));
    for t in 0..=10 {
        s.end_tick(t);
    }
    s.world.despawn(e);
    for t in 11..=13 {
        s.end_tick(t);
    }
    s.view(shooter(), 10.0);
    let h = s
        .lc()
        .raycast(shooter(), ray_z(0.0), 100.0, &none())
        .expect("q")
        .expect("hit");
    assert_eq!(h.entity, e);
    assert!(!h.alive);
    // At a tick after the despawn nothing is there.
    s.view(shooter(), 12.0);
    assert!(
        s.lc()
            .raycast(shooter(), ray_z(0.0), 100.0, &none())
            .expect("q")
            .is_none()
    );
    // A live target reports alive.
    let live = s.body(Vec3::new(0.0, 0.0, 10.0));
    s.end_tick(14);
    s.view(shooter(), 14.0);
    let h = s
        .lc()
        .raycast(shooter(), ray_z(0.0), 100.0, &none())
        .expect("q")
        .expect("hit");
    assert_eq!(h.entity, live);
    assert!(h.alive);
}

/// § 13 test 8: with `PoseSource::Joints`, a head shape follows an animated head joint (a named
/// child in the scene hierarchy); a missing joint falls back to the root and logs once.
#[test]
fn joint_shapes_follow_the_joint() {
    let mut s = Server::new();
    let root = s
        .world
        .spawn((
            NetId(50),
            Transform::from_translation(Vec3::new(0.0, 0.0, 10.0)),
            HitShapes {
                shapes: vec![
                    HitShape::new(HitShapeKind::Sphere { radius: 0.2 }, "head").on_joint("head"),
                    HitShape::new(HitShapeKind::Sphere { radius: 0.2 }, "neck").on_joint("neck"),
                ],
                pose_source: PoseSource::Joints,
            },
        ))
        .id();
    let head = s
        .world
        .spawn((
            Name::new("head"),
            Transform::from_translation(Vec3::new(0.0, 1.7, 0.0)),
            ChildOf(root),
        ))
        .id();
    for t in 0..=10u32 {
        // The animation sways the head sideways; the root also walks forward.
        let sway = if t >= 5 { 0.5 } else { 0.0 };
        if let Some(mut tr) = s.world.get_mut::<Transform>(head) {
            tr.translation.x = sway;
        }
        s.move_to(root, Vec3::new(0.0, 0.0, 10.0 + t as f32 * 0.1));
        regia_scene::propagate::propagate_transforms(&mut s.world);
        s.end_tick(t);
    }
    let lc = s.lc();
    let hit_at = |tick: f64, p: Vec3| {
        lc.at_tick(NetTickFrac(tick))
            .expect("in history")
            .raycast(Ray::new(p - Vec3::Z * 5.0, Vec3::Z), 100.0, &none())
            .expect("q")
            .map(|h| h.tag.as_str().to_string())
    };
    assert_eq!(hit_at(3.0, Vec3::new(0.0, 1.7, 10.3)).as_deref(), Some("head"));
    assert_eq!(hit_at(3.0, Vec3::new(0.5, 1.7, 10.3)), None);
    assert_eq!(hit_at(8.0, Vec3::new(0.5, 1.7, 10.8)).as_deref(), Some("head"));
    assert_eq!(hit_at(8.0, Vec3::new(0.0, 1.7, 10.8)), None);
    // The missing neck sits on the root.
    assert_eq!(hit_at(8.0, Vec3::new(0.0, 0.0, 10.8)).as_deref(), Some("neck"));
    assert_eq!(s.world.resource::<LagCompHistory>().joint_missing_logged, 1);
}

/// A host joint source wins over the hierarchy.
#[test]
fn joint_source_hook_is_used() {
    fn above(world: &World, root: Entity, joint: &str) -> Option<glam::Affine3A> {
        let t = world.get::<Transform>(root)?;
        (joint == "head").then(|| glam::Affine3A::from_translation(t.translation + Vec3::Y * 3.0))
    }
    let mut s = Server::new();
    s.world.resource_mut::<LagCompJointSource>().0 = Some(above);
    s.world.spawn((
        NetId(7),
        Transform::from_translation(Vec3::new(0.0, 0.0, 10.0)),
        HitShapes {
            shapes: vec![HitShape::new(HitShapeKind::Sphere { radius: 0.2 }, "head").on_joint("head")],
            pose_source: PoseSource::Joints,
        },
    ));
    s.end_tick(0);
    let lc = s.lc();
    let view = lc.at_tick(NetTickFrac(0.0)).expect("history");
    let r = Ray::new(Vec3::new(0.0, 3.0, 0.0), Vec3::Z);
    assert!(view.raycast(r, 100.0, &none()).expect("q").is_some());
    assert_eq!(s.world.resource::<LagCompHistory>().joint_missing_logged, 0);
}

fn claim(target: Entity, tag: &str, origin: Vec3, at: Vec3, view_tick: f64) -> HitClaim {
    HitClaim {
        target,
        tag: tag.into(),
        origin,
        direction: at - origin,
        view_tick,
        weapon_seq: 1,
    }
}

/// § 13 test 9: one claim per refusal reason, a valid claim, and the per-shooter counters.
#[test]
fn claims_are_validated_with_every_reason() {
    let mut s = Server::new();
    let e = s.body(Vec3::ZERO);
    for t in 0..=20u32 {
        s.move_to(e, Vec3::new(t as f32 * 0.1, 0.0, 10.0));
        s.end_tick(t);
    }
    // Current tick 21; the shooter saw tick 15 (target at x = 1.5).
    s.view(shooter(), 15.0);
    let muzzle = Vec3::new(1.5, 1.0, 0.0);
    let seen = Vec3::new(1.5, 1.0, 10.0);
    let verdict = |s: &Server, c: &HitClaim| s.lc().validate_claim(shooter(), c, muzzle).expect("q");
    match verdict(&s, &claim(e, "body", muzzle, seen, 15.0)) {
        ClaimVerdict::Valid(h) => {
            assert_eq!(h.entity, e);
            assert_eq!(h.tag.as_str(), "body");
            assert_eq!(h.rewound_tick, NetTickFrac(15.0));
        }
        other => panic!("valid claim refused: {other:?}"),
    }
    let cases = [
        (
            claim(e, "body", muzzle, seen, 2.0),
            ClaimInvalid::ViewTickOutOfRange,
        ),
        (
            claim(e, "body", muzzle, seen, 19.0),
            ClaimInvalid::ViewTickOutOfRange,
        ),
        (
            claim(e, "body", muzzle + Vec3::X * 2.0, seen, 15.0),
            ClaimInvalid::OriginTooFar,
        ),
        // Aimed where the target is now (x = 2.0 at tick 20), not where the shooter saw it.
        (
            claim(e, "body", muzzle, Vec3::new(2.4, 1.0, 10.0), 15.0),
            ClaimInvalid::MissedShape,
        ),
        (claim(e, "head", muzzle, seen, 15.0), ClaimInvalid::MissedShape),
        (
            claim(Entity::PLACEHOLDER, "body", muzzle, seen, 15.0),
            ClaimInvalid::TargetUnknown,
        ),
    ];
    for (c, want) in &cases {
        assert_eq!(verdict(&s, c), ClaimVerdict::Invalid(*want), "{c:?}");
    }
    s.world.resource_mut::<LagCompOcclusion>().0 = Some(wall_hook);
    s.world.insert_resource(WallAt(5.0));
    assert_eq!(
        verdict(&s, &claim(e, "body", muzzle, seen, 15.0)),
        ClaimVerdict::Invalid(ClaimInvalid::Occluded)
    );
    // The tolerance: 0.45 m off the capsule's side (radius 0.4) passes only with 0.1 m tolerance.
    s.world.remove_resource::<WallAt>();
    let edge = Vec3::new(1.5 + 0.45, 1.0, 10.0);
    let edge_claim = claim(e, "body", Vec3::new(1.95, 1.0, 0.0), edge, 15.0);
    let m2 = Vec3::new(1.95, 1.0, 0.0);
    assert!(matches!(
        s.lc().validate_claim(shooter(), &edge_claim, m2).expect("q"),
        ClaimVerdict::Valid(_)
    ));
    s.settings().lag_compensation.claim_tolerance_m = 0.0;
    assert_eq!(
        s.lc().validate_claim(shooter(), &edge_claim, m2).expect("q"),
        ClaimVerdict::Invalid(ClaimInvalid::MissedShape)
    );
    let c = s.world.resource::<LagCompHistory>().shooter_counters(shooter());
    assert_eq!(c.claims_valid, 2);
    assert_eq!(c.invalid(ClaimInvalid::ViewTickOutOfRange), 2);
    assert_eq!(c.invalid(ClaimInvalid::OriginTooFar), 1);
    assert_eq!(c.invalid(ClaimInvalid::MissedShape), 3);
    assert_eq!(c.invalid(ClaimInvalid::Occluded), 1);
    assert_eq!(c.invalid(ClaimInvalid::TargetUnknown), 1);
    assert_eq!(
        s.world
            .resource::<LagCompHistory>()
            .shooter_counters(other_shooter()),
        ShooterCounters::default()
    );
}

/// § 13 test 10: a fast projectile fired at a strafing target hits under
/// `projectile_rewind_ticks: 6` and misses under 0.
#[test]
fn projectile_rewind_window() {
    for (window, expect_hit) in [(6u32, true), (0, false)] {
        let mut s = Server::new();
        s.settings().lag_compensation.projectile_rewind_ticks = window;
        let e = s.body(Vec3::ZERO);
        let x = |t: u32| t as f32 * 0.1; // 6 m/s
        for t in 0..=20 {
            s.move_to(e, Vec3::new(x(t), 0.0, 10.0));
            s.end_tick(t);
        }
        // Tick 21: the shooter saw tick 13 (x = 1.3) and fires straight at it, 300 m/s.
        s.view(shooter(), 13.0);
        let lag = s.lc().projectile_view_lag(shooter()).expect("q");
        assert_eq!(lag, 8.0);
        let proj = LagCompProjectile {
            fire_tick: NetTick(21),
            shooter_view_lag: lag,
        };
        let mut pos = Vec3::new(x(13), 1.0, 0.0);
        let mut hit = false;
        for t in 21..=24u32 {
            s.move_to(e, Vec3::new(x(t), 0.0, 10.0));
            // Each tick the projectile moves 5 m; its impact test sweeps the segment.
            let lc = s.lc();
            let at = lc.projectile_tick(&proj);
            hit |= lc
                .at_tick(at)
                .expect("history")
                .raycast(Ray::new(pos, Vec3::Z), 5.0, &none())
                .expect("q")
                .is_some();
            drop(lc);
            pos.z += 5.0;
            s.end_tick(t);
        }
        assert_eq!(hit, expect_hit, "projectile_rewind_ticks {window}");
    }
}

/// § 13 test 11: 1,000 queries leave every component of every lag-compensated entity unchanged.
#[test]
fn queries_are_read_only() {
    let mut s = Server::new();
    let mut ents = Vec::new();
    for i in 0..8 {
        ents.push(s.body(Vec3::new(i as f32 * 2.0 - 8.0, 0.0, 10.0)));
    }
    for t in 0..=20u32 {
        for (i, e) in ents.iter().enumerate() {
            s.move_to(*e, Vec3::new(i as f32 * 2.0 - 8.0 + t as f32 * 0.05, 0.0, 10.0));
        }
        s.end_tick(t);
    }
    s.view(shooter(), 14.5);
    let snapshot = |w: &World| -> Vec<String> {
        ents.iter()
            .map(|e| {
                format!(
                    "{:?} {:?} {:?} {:?}",
                    w.get::<Transform>(*e),
                    w.get::<HitShapes>(*e),
                    w.get::<NetId>(*e),
                    w.get::<LagCompensated>(*e).is_some()
                )
            })
            .collect()
    };
    let before = snapshot(&s.world);
    let ticks_before = s.world.resource::<LagCompHistory>().ticks_kept();
    let lc = s.lc();
    let mut hits = 0;
    for i in 0..1000 {
        let x = (i % 40) as f32 * 0.5 - 10.0;
        match i % 3 {
            0 => {
                hits += lc
                    .raycast(shooter(), ray_z(x), 100.0, &none())
                    .expect("q")
                    .is_some() as u32
            }
            1 => {
                hits += lc
                    .sphere_cast(shooter(), ray_z(x), 0.3, 100.0, &none())
                    .expect("q")
                    .is_some() as u32
            }
            _ => {
                hits += lc
                    .overlap_sphere(shooter(), Vec3::new(x, 1.0, 10.0), 0.5, &none())
                    .expect("q")
                    .len() as u32
            }
        }
    }
    drop(lc);
    assert!(hits > 100, "the queries hit something ({hits})");
    assert_eq!(snapshot(&s.world), before);
    assert_eq!(s.world.resource::<LagCompHistory>().ticks_kept(), ticks_before);
    assert_eq!(s.world.resource::<LagCompHistory>().totals().0, 1000);
}

/// Determinism: the nearest hit wins and equal distances go to the lower `NetId`, whatever the
/// spawn order.
#[test]
fn ties_go_to_the_lower_net_id() {
    for flip in [false, true] {
        let mut s = Server::new();
        let shape = || vec![HitShape::new(HitShapeKind::Sphere { radius: 0.5 }, "s")];
        let (a, b) = if flip {
            let b = s
                .world
                .spawn((NetId(9), Transform::from_translation(Vec3::new(0.0, 1.0, 10.0))))
                .id();
            let a = s
                .world
                .spawn((NetId(3), Transform::from_translation(Vec3::new(0.0, 1.0, 10.0))))
                .id();
            (a, b)
        } else {
            let a = s
                .world
                .spawn((NetId(3), Transform::from_translation(Vec3::new(0.0, 1.0, 10.0))))
                .id();
            let b = s
                .world
                .spawn((NetId(9), Transform::from_translation(Vec3::new(0.0, 1.0, 10.0))))
                .id();
            (a, b)
        };
        for e in [a, b] {
            s.world.entity_mut(e).insert(HitShapes {
                shapes: shape(),
                pose_source: PoseSource::Root,
            });
        }
        s.end_tick(0);
        s.end_tick(1);
        s.view(shooter(), 1.0);
        let h = s
            .lc()
            .raycast(shooter(), ray_z(0.0), 100.0, &none())
            .expect("q")
            .expect("hit");
        assert_eq!(h.net_id, NetId(3));
        assert_eq!(h.entity, a);
    }
}

/// Clients cannot query; `NoLagCompensation` opts out; invalid shapes are skipped and logged once.
#[test]
fn client_opt_out_and_invalid_shapes() {
    let mut w = World::new();
    w.insert_resource(NetMode::Client);
    ensure_resources(&mut w);
    install_lag_compensation(&mut w);
    assert_eq!(LagCompensation::new(&w).err(), Some(LagCompError::NotServer));
    let mut bare = World::new();
    bare.insert_resource(NetMode::DedicatedServer);
    ensure_resources(&mut bare);
    assert_eq!(
        LagCompensation::new(&bare).err(),
        Some(LagCompError::NotInstalled)
    );

    let mut s = Server::new();
    let dummy = s.body(Vec3::new(0.0, 0.0, 10.0));
    s.world.entity_mut(dummy).insert(NoLagCompensation);
    let broken = s.spawn(
        Vec3::new(5.0, 0.0, 10.0),
        vec![
            HitShape::new(HitShapeKind::Sphere { radius: -1.0 }, "bad"),
            HitShape::new(HitShapeKind::Sphere { radius: 0.5 }, "good").at(Vec3::Y),
        ],
    );
    for t in 0..3 {
        s.end_tick(t);
    }
    let h = s.world.resource::<LagCompHistory>();
    assert!(!h.contains(dummy), "opted out");
    let rows = h.history_of(broken);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].2.len(), 1, "the invalid shape is skipped");
    assert_eq!(rows[0].2[0].tag, "good");
    assert!(s.world.get::<LagCompensated>(dummy).is_none());
}

/// The plugin records in `NetSet::LagCompRecord` inside an app's fixed step, marks replicated
/// entities with `HitShapes` as `LagCompensated`, and keeps `ceil(max_rewind_ms / step) + 2` ticks
/// (250 ms here: 17).
#[test]
fn plugin_records_each_fixed_step() {
    let mut app = regia_app::App::new();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins(NetPlugin {
        mode: NetMode::DedicatedServer,
    });
    app.add_plugins(LagCompPlugin);
    app.world_mut()
        .resource_mut::<NetSettings>()
        .lag_compensation
        .max_rewind_ms = 250.0;
    let e = app
        .world_mut()
        .spawn((
            Replicated::default(),
            Transform::default(),
            HitShapes {
                shapes: vec![body()],
                pose_source: PoseSource::Root,
            },
        ))
        .id();
    for _ in 0..40 {
        app.update();
    }
    let w = app.world();
    assert!(w.get::<LagCompensated>(e).is_some());
    let h = w.resource::<LagCompHistory>();
    assert_eq!(h.ticks_kept(), 17);
    assert!(h.contains(e));
}

// ---- a claim over the wire ------------------------------------------------------------------------

#[derive(Resource, Default)]
struct Verdicts(Vec<ClaimVerdict>);

fn claim_setup(world: &mut World) {
    regia_net::message::register_net_message::<HitClaim>(world);
}

fn judge_claims(world: &mut World) {
    let claims: Vec<(PeerId, HitClaim)> = world
        .get_resource::<regia_net::message::NetMessages<HitClaim>>()
        .map(|m| m.iter().map(|r| (r.from, r.msg.clone())).collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    if let Ok(lc) = LagCompensation::new(world) {
        for (from, c) in &claims {
            let shooter = NetPlayerId {
                peer: *from,
                local: 0,
            };
            if let Ok(v) = lc.validate_claim(shooter, c, c.origin) {
                out.push(v);
            }
        }
    }
    world.resource_mut::<Verdicts>().0.extend(out);
}

/// Spec 75 § 8 end to end: a client sends `HitClaim`s (targeted at its own pawn, naming the hit
/// entity by its client copy); the server receives the entity as its own and validates them.
#[test]
fn claims_travel_and_validate_on_the_server() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 90, claim_setup);
    let c = hub.add_client();
    let peer = hub.clients[c].peer;
    install_lag_compensation(&mut hub.server.world);
    hub.server.world.insert_resource(HitClaimPolicy::HandlerValidates);
    hub.server.world.init_resource::<Verdicts>();
    hub.server
        .gameplay
        .add_systems((judge_claims, lagcomp_record).chain());
    let pawn = hub
        .server
        .world
        .spawn((Replicated::default(), Transform::default(), NetOwner(peer)))
        .id();
    let target = hub
        .server
        .world
        .spawn((
            Replicated::default(),
            Transform::from_translation(Vec3::new(0.0, 0.0, 10.0)),
            HitShapes {
                shapes: vec![body()],
                pose_source: PoseSource::Root,
            },
        ))
        .id();
    hub.run(5);
    let id = |e: Entity| *hub.server.world.get::<NetId>(e).expect("net id");
    let (pawn_id, target_id) = (id(pawn), id(target));
    let cw = &hub.clients[c].world;
    let copy = |id: NetId| cw.resource::<NetEntities>().entity(id).expect("copy");
    let (pawn_copy, target_copy) = (copy(pawn_id), copy(target_id));
    let view = f64::from(cw.resource::<NetNode>().tick.0);
    // The view tick travels with the shooter's input (spec 74 § 8); the server records it.
    hub.server.world.spawn((
        NetPlayer {
            id: NetPlayerId { peer, local: 0 },
        },
        NetViewTick {
            tick: view,
            delay_ms: 0.0,
        },
    ));
    for tag in ["body", "head"] {
        let msg = HitClaim {
            target: target_copy,
            tag: tag.into(),
            origin: Vec3::new(0.0, 1.0, 0.0),
            direction: Vec3::Z,
            view_tick: view,
            weapon_seq: 1,
        };
        let world = &mut hub.clients[c].world;
        world
            .run_system_once(move |mut net: NetSender| net.send_to_server(pawn_copy, msg.clone()))
            .expect("system runs")
            .expect("queued");
    }
    hub.run(3);
    let v = &hub.server.world.resource::<Verdicts>().0;
    assert_eq!(v.len(), 2, "{v:?}");
    match &v[0] {
        ClaimVerdict::Valid(h) => assert_eq!(h.entity, target),
        other => panic!("valid claim refused: {other:?}"),
    }
    assert_eq!(v[1], ClaimVerdict::Invalid(ClaimInvalid::MissedShape));
}

/// A joined shooter, its pawn, one target with history, and a claim on it.
fn claim_rig(policy: Option<HitClaimPolicy>) -> (LoopbackHub, usize, Entity, Entity, Entity) {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 91, claim_setup);
    let c = hub.add_client();
    let peer = hub.clients[c].peer;
    install_lag_compensation(&mut hub.server.world);
    if let Some(policy) = policy {
        hub.server.world.insert_resource(policy);
    }
    hub.server.gameplay.add_systems(lagcomp_record);
    let pawn = hub
        .server
        .world
        .spawn((Replicated::default(), Transform::default(), NetOwner(peer)))
        .id();
    let target = hub
        .server
        .world
        .spawn((
            Replicated::default(),
            Transform::from_translation(Vec3::new(0.0, 0.0, 10.0)),
            HitShapes {
                shapes: vec![body()],
                pose_source: PoseSource::Root,
            },
        ))
        .id();
    hub.run(5);
    let id = |e: Entity| *hub.server.world.get::<NetId>(e).expect("net id");
    let (pawn_id, target_id) = (id(pawn), id(target));
    let cw = &hub.clients[c].world;
    let copy = |id: NetId| cw.resource::<NetEntities>().entity(id).expect("copy");
    let (pawn_copy, target_copy) = (copy(pawn_id), copy(target_id));
    let view = f64::from(hub.server_tick().0) - 1.0;
    hub.server.world.spawn((
        NetPlayer {
            id: NetPlayerId { peer, local: 0 },
        },
        NetViewTick {
            tick: view,
            delay_ms: 0.0,
        },
    ));
    (hub, c, pawn_copy, target_copy, target)
}

fn send_claim(hub: &mut LoopbackHub, c: usize, pawn: Entity, target: Entity, origin: Vec3) {
    let view = f64::from(hub.server_tick().0) - 1.0;
    let msg = HitClaim {
        target,
        tag: "body".into(),
        origin,
        direction: Vec3::Z,
        view_tick: view,
        weapon_seq: 1,
    };
    hub.clients[c]
        .world
        .run_system_once(move |mut net: NetSender| net.send_to_server(pawn, msg.clone()))
        .expect("system runs")
        .expect("queued");
    hub.run(3);
}

fn claim_counters(hub: &LoopbackHub) -> regia_net::message::MessageCounters {
    let reg = hub
        .server
        .world
        .resource::<regia_net::message::NetMessageRegistry>();
    reg.counters[usize::from(reg.index_of::<HitClaim>().expect("registered"))]
}

/// Audit M8 (grok): a game that registers `HitClaim` and forgets the rest does not trust clients: no
/// policy, no delivery.
#[test]
fn claims_are_rejected_without_a_policy() {
    let (mut hub, c, pawn, target, _) = claim_rig(None);
    send_claim(&mut hub, c, pawn, target, Vec3::new(0.0, 1.0, 0.0));
    let n = claim_counters(&hub);
    assert_eq!((n.delivered, n.rejected), (0, 1));
    assert!(hub.clients[c].connected, "a rejected claim is not a kick");
}

/// With the `Validate` policy the engine judges each claim and handlers see only the valid ones.
#[test]
fn the_validate_policy_delivers_only_valid_claims() {
    fn muzzle(_: &World, _: NetPlayerId) -> Option<Vec3> {
        Some(Vec3::new(0.0, 1.0, 0.0))
    }
    let (mut hub, c, pawn, target, _) = claim_rig(Some(HitClaimPolicy::Validate(muzzle)));
    send_claim(&mut hub, c, pawn, target, Vec3::new(0.0, 1.0, 0.0));
    let n = claim_counters(&hub);
    assert_eq!((n.delivered, n.rejected), (1, 0), "a good claim");
    // Fired from somewhere the shooter is not.
    send_claim(&mut hub, c, pawn, target, Vec3::new(0.0, 1.0, 5.0));
    let n = claim_counters(&hub);
    assert_eq!((n.delivered, n.rejected), (1, 1), "origin too far");
}

/// A shooter the game has no muzzle for cannot land a claim.
#[test]
fn the_validate_policy_rejects_a_shooter_with_no_muzzle() {
    fn muzzle(_: &World, _: NetPlayerId) -> Option<Vec3> {
        None
    }
    let (mut hub, c, pawn, target, _) = claim_rig(Some(HitClaimPolicy::Validate(muzzle)));
    send_claim(&mut hub, c, pawn, target, Vec3::new(0.0, 1.0, 0.0));
    let n = claim_counters(&hub);
    assert_eq!((n.delivered, n.rejected), (0, 1));
}

// ---- edge cases found on review -------------------------------------------------------------------

/// A teleport on the tick being simulated has no record yet: a rewind into the present is a cut
/// all the same (the live pose is the far side of it).
#[test]
fn teleport_on_the_present_tick_is_a_cut() {
    let mut s = Server::new();
    let e = s.body(Vec3::ZERO);
    for t in 0..=10 {
        s.end_tick(t);
    }
    // Tick 11 is being simulated: moved and marked, not recorded.
    s.move_to(e, Vec3::new(100.0, 0.0, 0.0));
    net_teleport(&mut s.world, e);
    let lc = s.lc();
    let at = |tick: f64, x: f32| {
        lc.at_tick(NetTickFrac(tick))
            .expect("in history")
            .raycast(ray_z(x), 100.0, &none())
            .expect("q")
            .is_some()
    };
    assert!(at(10.4, 0.0), "nearer the old side");
    assert!(at(10.6, 100.0), "nearer the new side");
    for x in [10.0, 30.0, 50.0, 70.0, 90.0] {
        assert!(!at(10.5, x), "nothing between the two places, x = {x}");
    }
}

/// A claim's direction comes off the wire. No direction is a miss, even from inside the shape,
/// where a ray of any length reports a hit at distance 0.
#[test]
fn claims_without_a_direction_miss() {
    let mut s = Server::new();
    let e = s.body(Vec3::ZERO);
    for t in 0..=20 {
        s.end_tick(t);
    }
    s.view(shooter(), 15.0);
    let inside = Vec3::new(0.0, 1.0, 0.0);
    for direction in [Vec3::ZERO, Vec3::NAN, Vec3::INFINITY] {
        let c = HitClaim {
            target: e,
            tag: "body".into(),
            origin: inside,
            direction,
            view_tick: 15.0,
            weapon_seq: 1,
        };
        assert_eq!(
            s.lc().validate_claim(shooter(), &c, inside).expect("q"),
            ClaimVerdict::Invalid(ClaimInvalid::MissedShape),
            "{direction:?}"
        );
    }
}

/// A fast turn: between two ticks a box turns 90°, so its pose at the half tick has corners
/// outside both recorded boxes. The broad phase must not lose them.
#[test]
fn a_fast_turn_keeps_its_corners_in_the_broad_phase() {
    let mut s = Server::new();
    let e = s.spawn(
        Vec3::ZERO,
        vec![HitShape::new(
            HitShapeKind::Box {
                half_extents: Vec3::ONE,
            },
            "crate",
        )],
    );
    for t in 0..=10 {
        s.end_tick(t);
    }
    if let Some(mut t) = s.world.get_mut::<Transform>(e) {
        t.rotation = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
    }
    s.end_tick(11);
    s.end_tick(12);
    let lc = s.lc();
    let view = lc.at_tick(NetTickFrac(10.5)).expect("in history");
    // At 45° the corner reaches x = 1.414; both recorded boxes stop at 1.0.
    let r = Ray::new(Vec3::new(1.3, 0.0, -10.0), Vec3::Z);
    assert!(view.raycast(r, 100.0, &none()).expect("q").is_some());
    let far = Ray::new(Vec3::new(1.5, 0.0, -10.0), Vec3::Z);
    assert!(view.raycast(far, 100.0, &none()).expect("q").is_none());
}

// ---- spec 75 § 11: the debug echo -----------------------------------------------------------------

#[derive(Resource, Default)]
struct EchoShots {
    rays: VecDeque<Ray>,
}

fn echo_setup(world: &mut World) {
    regia_net::message::register_net_message::<LagCompEcho>(world);
}

/// Fires one queued ray a tick as peer `Shooter`.
fn echo_fire(world: &mut World) {
    let ray = world.resource_mut::<EchoShots>().rays.pop_front();
    let Some(ray) = ray else {
        return;
    };
    let peer = world.resource::<Shooter>().0;
    if let Ok(lc) = LagCompensation::new(world) {
        let who = NetPlayerId { peer, local: 0 };
        lc.raycast(who, ray, 100.0, &HitFilter::default()).expect("query");
    }
}

#[derive(Resource)]
struct Shooter(PeerId);

/// Runs one shot and returns the echoes the client has.
fn echoes_after(hub: &mut LoopbackHub, ray: Ray) -> usize {
    hub.server.world.resource_mut::<EchoShots>().rays.push_back(ray);
    hub.run(6);
    hub.clients[0].world.resource::<LagCompEchoLog>().len()
}

/// A hit's rewound shapes reach the shooter with `debug_echo` on (dev builds); a miss or the
/// setting off sends nothing.
#[test]
fn the_debug_echo_reaches_the_shooter() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 90, echo_setup);
    let c = hub.add_client();
    let peer = hub.clients[c].peer;
    install_lag_compensation(&mut hub.server.world);
    hub.server.world.init_resource::<EchoShots>();
    hub.server.world.insert_resource(Shooter(peer));
    hub.server.gameplay.add_systems(
        (
            echo_fire,
            lagcomp_record,
            regia_net::lagcomp::echo::lagcomp_echo_send,
        )
            .chain(),
    );
    hub.clients[c].world.init_resource::<LagCompEchoLog>();
    hub.clients[c]
        .gameplay
        .add_systems(regia_net::lagcomp::echo::lagcomp_echo_receive);
    hub.server.world.spawn((
        Replicated::default(),
        Transform::from_translation(Vec3::new(0.0, 0.0, 10.0)),
        HitShapes {
            shapes: vec![body()],
            pose_source: PoseSource::Root,
        },
    ));
    hub.run(5);
    let at_body = Ray::new(Vec3::new(0.0, 1.0, 0.0), Vec3::Z);
    assert_eq!(echoes_after(&mut hub, at_body), 0, "off by default");
    hub.server
        .world
        .resource_mut::<NetSettings>()
        .lag_compensation
        .debug_echo = true;
    let away = Ray::new(Vec3::new(9.0, 1.0, 0.0), Vec3::Z);
    assert_eq!(echoes_after(&mut hub, away), 0, "a miss has no shapes to show");
    let want = usize::from(cfg!(debug_assertions));
    assert_eq!(echoes_after(&mut hub, at_body), want, "dev builds only");
    if want == 1 {
        let log = hub.clients[c].world.resource::<LagCompEchoLog>();
        let echo = log.iter().next().expect("one echo");
        assert_eq!(echo.origin, at_body.origin);
        assert_eq!(echo.direction, Vec3::Z);
        assert_eq!(echo.shapes.len(), 1);
        assert_eq!(echo.shapes[0].tag, "body");
        assert_eq!(echo.shapes[0].kind, body().kind);
        assert_eq!(echo.shapes[0].position, Vec3::new(0.0, 1.0, 10.0));
        let report = regia_net::lagcomp::echo::echoes_report(&hub.clients[c].world, 16).expect("client");
        assert_eq!(report[0].shapes[0].kind, "capsule");
        assert_eq!(report[0].shapes[0].size, vec![0.4, 0.5]);
    }
}

/// The client keeps the newest 16 echoes; the listen host's own hits are not echoed.
#[test]
fn echoes_are_bounded_and_skip_the_host() {
    let mut log = LagCompEchoLog::default();
    let echo = |tick: u32| LagCompEcho {
        server_tick: tick,
        rewound_tick: 0.0,
        origin: Vec3::ZERO,
        direction: Vec3::Z,
        shapes: Vec::new(),
    };
    for t in 0..20 {
        log.push(echo(t));
    }
    assert_eq!(log.len(), 16);
    assert_eq!(log.iter().next().map(|e| e.server_tick), Some(4));

    let mut s = Server::new();
    s.world.insert_resource(NetMode::ListenServer);
    s.body(Vec3::ZERO);
    s.settings().lag_compensation.debug_echo = true;
    for t in 0..=3 {
        s.end_tick(t);
    }
    let host = NetPlayerId {
        peer: PeerId::SERVER,
        local: 0,
    };
    let lc = s.lc();
    assert!(lc.raycast(host, ray_z(0.0), 100.0, &none()).expect("q").is_some());
    assert!(s.world.resource::<LagCompHistory>().take_echoes().is_empty());
}

// ---- found comparing against the reviewed spec's edge cases ---------------------------------------

/// § 4: a target that died a moment ago still resolves, for a team-filtered shot too (its groups
/// are recorded with the history), and a game's predicate never sees a dead entity.
#[test]
fn a_dead_target_passes_group_and_predicate_filters() {
    fn exists(world: &World, e: Entity) -> bool {
        // What a game's filter does; on a despawned entity this panics.
        world.entity(e).contains::<NetId>()
    }
    let mut s = Server::new();
    let e = s.body(Vec3::ZERO);
    s.world.entity_mut(e).insert(NetGroups(vec!["team_a".into()]));
    for t in 0..=10 {
        s.end_tick(t);
    }
    s.world.despawn(e);
    s.end_tick(11);
    s.end_tick(12);
    s.view(shooter(), 9.0);
    let lc = s.lc();
    let team = |name: &str| HitFilter {
        groups: Some(NetGroups(vec![name.into()])),
        ..HitFilter::default()
    };
    let hit = lc
        .raycast(shooter(), ray_z(0.0), 100.0, &team("team_a"))
        .expect("q");
    assert!(!hit.expect("the dead target resolves").alive);
    let other = lc
        .raycast(shooter(), ray_z(0.0), 100.0, &team("team_b"))
        .expect("q");
    assert!(other.is_none(), "it was never on that team");
    let filter = HitFilter {
        predicate: Some(exists),
        ..HitFilter::default()
    };
    let hit = lc.raycast(shooter(), ray_z(0.0), 100.0, &filter).expect("q");
    assert!(hit.is_some(), "the predicate is not asked about the dead");
}

/// A hit shape on a child follows a parent that moved in this fixed step: `GlobalTransform` is not
/// propagated until after the step, so the recorder composes local transforms up the chain.
#[test]
fn a_child_follows_its_moving_parent_in_the_same_step() {
    let mut s = Server::new();
    let parent = s
        .world
        .spawn(Transform::from_translation(Vec3::new(50.0, 0.0, 0.0)))
        .id();
    let e = s.spawn(Vec3::new(0.0, 0.0, 2.0), vec![body()]);
    s.world.entity_mut(e).insert(ChildOf(parent));
    s.end_tick(0);
    let rows = s.world.resource::<LagCompHistory>().history_of(e);
    let (_, _, shapes) = rows.last().expect("recorded");
    assert_eq!(shapes[0].position, [50.0, 1.0, 2.0]);
    // Rotation and offset compose too: a quarter turn about Y sends +Z to +X.
    if let Some(mut t) = s.world.get_mut::<Transform>(parent) {
        t.translation.x = 60.0;
        t.rotation = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
    }
    s.end_tick(1);
    let rows = s.world.resource::<LagCompHistory>().history_of(e);
    let (_, _, shapes) = rows.last().expect("recorded");
    let at = Vec3::from_array(shapes[0].position);
    assert!((at - Vec3::new(62.0, 1.0, 0.0)).length() < 1e-4, "{at}");
}

#[derive(Resource, Default)]
struct IgnoreSeen(std::sync::Mutex<Vec<Entity>>);

fn record_ignore(world: &World, _: &Ray, _: f32, ignore: &[Entity]) -> Option<f32> {
    if let Some(seen) = world.get_resource::<IgnoreSeen>()
        && let Ok(mut v) = seen.0.lock()
    {
        v.extend_from_slice(ignore);
    }
    None
}

/// § 5 step 4: the shooter's own entities never occlude its shot, hit shapes or not: a solid ray
/// starts inside the shooter's own collider.
#[test]
fn the_occlusion_hook_ignores_the_shooters_own_entities() {
    let mut s = Server::new();
    s.world.init_resource::<IgnoreSeen>();
    s.world.resource_mut::<LagCompOcclusion>().0 = Some(record_ignore);
    let target = s.body(Vec3::ZERO);
    let pawn = s.world.spawn((Transform::default(), NetOwner(PEER))).id();
    let others = s.world.spawn((Transform::default(), NetOwner(OTHER))).id();
    for t in 0..=3 {
        s.end_tick(t);
    }
    s.view(shooter(), 2.0);
    let hit = s.lc().raycast(shooter(), ray_z(0.0), 100.0, &none()).expect("q");
    assert!(hit.is_some());
    let seen = s.world.resource::<IgnoreSeen>().0.lock().expect("lock").clone();
    assert!(
        seen.contains(&pawn),
        "the shooter's pawn has no shapes but is its own"
    );
    assert!(seen.contains(&target), "lag-compensated entities are ignored");
    assert!(!seen.contains(&others), "another player's body still blocks");
    assert!(seen.windows(2).all(|w| w[0] < w[1]), "sorted, for binary search");
}

/// Claims exist for shotgun patterns: a burst of pellets must fit the flood guard.
#[test]
fn the_claim_rate_limit_fits_a_shotgun() {
    let limit = HitClaim::RATE_LIMIT.expect("rate limited");
    let pellets_per_second = 8 * 10;
    assert!(limit.count >= pellets_per_second, "{limit:?}");
    assert_eq!(limit.per_ticks, 60);
}

// ---- perf: ignore lists, joint poses, shape buffers, broad phase --------------------------------

/// One overlap that hits several bodies builds the occlusion ignore list once.
#[test]
fn overlap_builds_one_ignore_list_for_every_hit() {
    let mut s = Server::new();
    let n = 6;
    for i in 0..n {
        s.body(Vec3::new(i as f32 * 0.5, 0.0, 0.0));
    }
    s.end_tick(0);
    s.view(shooter(), 0.0);
    s.world.resource_mut::<LagCompOcclusion>().0 = Some(wall_hook);
    let before = s.world.resource::<LagCompHistory>().perf().ignore_lists;
    let hits = s
        .lc()
        .overlap_sphere(shooter(), Vec3::new(1.0, 1.0, 0.0), 3.0, &none())
        .expect("q");
    assert_eq!(hits.len(), n, "every body in the sphere is a hit");
    let built = s.world.resource::<LagCompHistory>().perf().ignore_lists - before;
    assert_eq!(built, 1, "ignore lists built for one overlap: {built}, hits: {n}");
}

thread_local! {
    static JOINT_RESOLVES: Cell<u64> = const { Cell::new(0) };
}

fn counting_joint(world: &World, root: Entity, joint: &str) -> Option<glam::Affine3A> {
    JOINT_RESOLVES.with(|c| c.set(c.get() + 1));
    let t = world.get::<Transform>(root)?;
    let y = if joint == "head" { 1.7 } else { 1.2 };
    Some(glam::Affine3A::from_translation(t.translation + Vec3::Y * y))
}

/// A present-tick query and the record that follows resolve each joint name once.
#[test]
fn present_tick_query_and_record_resolve_each_joint_once() {
    let mut s = Server::new();
    s.world.resource_mut::<LagCompJointSource>().0 = Some(counting_joint);
    let e = s.spawn(
        Vec3::ZERO,
        vec![
            HitShape::new(HitShapeKind::Sphere { radius: 0.2 }, "s0").on_joint("head"),
            HitShape::new(HitShapeKind::Sphere { radius: 0.2 }, "s1").on_joint("head"),
            HitShape::new(HitShapeKind::Sphere { radius: 0.2 }, "s2").on_joint("chest"),
            HitShape::new(HitShapeKind::Sphere { radius: 0.2 }, "s3").on_joint("chest"),
        ],
    );
    s.world.entity_mut(e).insert(HitShapes {
        shapes: vec![
            HitShape::new(HitShapeKind::Sphere { radius: 0.2 }, "s0").on_joint("head"),
            HitShape::new(HitShapeKind::Sphere { radius: 0.2 }, "s1").on_joint("head"),
            HitShape::new(HitShapeKind::Sphere { radius: 0.2 }, "s2").on_joint("chest"),
            HitShape::new(HitShapeKind::Sphere { radius: 0.2 }, "s3").on_joint("chest"),
        ],
        pose_source: PoseSource::Joints,
    });
    s.end_tick(0);
    JOINT_RESOLVES.with(|c| c.set(0));
    let server = NetPlayerId {
        peer: PeerId::SERVER,
        local: 0,
    };
    let hit = s
        .lc()
        .raycast(
            server,
            Ray::new(Vec3::new(0.0, 1.7, -5.0), Vec3::Z),
            20.0,
            &none(),
        )
        .expect("q");
    assert!(hit.is_some(), "the live pose is the present tick");
    lagcomp_record(&mut s.world);
    let calls = JOINT_RESOLVES.with(|c| c.get());
    assert_eq!(
        calls, 2,
        "two joint names, posed once and reused by the record; hook calls: {calls}"
    );
}

#[derive(Debug)]
struct StillAnim;

impl RecordedAnim for StillAnim {
    fn joint_poses(
        &self,
        root: &glam::Affine3A,
        joints: &[&str],
        out: &mut Vec<Option<glam::Affine3A>>,
    ) -> bool {
        out.extend(std::iter::repeat_n(Some(*root), joints.len()));
        true
    }

    fn has_joint(&self, _: &str) -> bool {
        true
    }

    fn reach(&self) -> f32 {
        2.0
    }

    fn heap_bytes(&self) -> usize {
        0
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn capture_still(
    _: &World,
    _: Entity,
    _: f32,
    _: Option<&Arc<dyn RecordedAnim>>,
) -> Result<Arc<dyn RecordedAnim>, &'static str> {
    Ok(Arc::new(StillAnim))
}

/// Two casts in one view share one posed shape buffer, and an animation entry is posed once.
#[test]
fn two_casts_share_one_shape_buffer() {
    let mut s = Server::new();
    s.world.resource_mut::<LagCompAnimSource>().0 = Some(capture_still);
    s.world.spawn((
        NetId(3),
        Transform::from_translation(Vec3::new(0.0, 0.0, 5.0)),
        HitShapes {
            shapes: vec![HitShape::new(HitShapeKind::Sphere { radius: 0.3 }, "body").on_joint("spine")],
            pose_source: PoseSource::Joints,
        },
    ));
    s.end_tick(0);
    s.view(shooter(), 0.0);
    let history = s.world.resource::<LagCompHistory>();
    let poses_before = history.anim_counters().0;
    let buffers_before = history.perf().shape_buffers;
    let lc = s.lc();
    let ray = Ray::new(Vec3::new(0.0, 0.0, -5.0), Vec3::Z);
    assert!(lc.raycast(shooter(), ray, 30.0, &none()).expect("q").is_some());
    assert!(lc.raycast(shooter(), ray, 30.0, &none()).expect("q").is_some());
    let history = s.world.resource::<LagCompHistory>();
    let poses = history.anim_counters().0 - poses_before;
    let buffers = history.perf().shape_buffers - buffers_before;
    assert_eq!(poses, 1, "animation poses for two casts of one entity: {poses}");
    assert_eq!(buffers, 1, "shape buffers for two casts of one entity: {buffers}");
}

/// A short ray does not visit bodies standing a hundred metres apart.
#[test]
fn a_short_ray_skips_distant_recorded_bodies() {
    let mut s = Server::new();
    let near = s.body(Vec3::ZERO);
    for i in 1..1000 {
        s.body(Vec3::new(i as f32 * 100.0, 0.0, 0.0));
    }
    s.end_tick(0);
    s.view(shooter(), 0.0);
    assert_eq!(s.world.resource::<LagCompHistory>().entities(), 1000);
    let before = s.world.resource::<LagCompHistory>().perf().broadphase_visits;
    let hit = s
        .lc()
        .raycast(
            shooter(),
            Ray::new(Vec3::new(-1.0, 1.0, 0.0), Vec3::X),
            4.0,
            &none(),
        )
        .expect("q")
        .expect("the body at the origin");
    assert_eq!(hit.entity, near);
    let visits = s.world.resource::<LagCompHistory>().perf().broadphase_visits - before;
    assert!(
        (1..8).contains(&visits),
        "broad-phase visits for one nearby body among 1000: {visits}"
    );
}

// ---- spec audit 2026-11-10: § 3, § 4, § 9 items with no test before ------------------------------

/// § 3 "at most 32 shapes per entity": shapes past `max_shapes_per_entity` are not recorded and
/// cannot be hit; a lower setting lowers the cap from the next record.
#[test]
fn shapes_past_the_per_entity_cap_are_not_recorded() {
    let mut s = Server::new();
    let shapes = (0..40)
        .map(|i| {
            HitShape::new(HitShapeKind::Sphere { radius: 0.2 }, &format!("s{i}"))
                .at(Vec3::new(i as f32, 1.0, 0.0))
        })
        .collect();
    let e = s.spawn(Vec3::new(0.0, 0.0, 10.0), shapes);
    assert_eq!(
        s.world
            .resource::<NetSettings>()
            .lag_compensation
            .max_shapes_per_entity,
        32,
        "spec 75 § 9 default"
    );
    s.end_tick(0);
    let rows = s.world.resource::<LagCompHistory>().history_of(e);
    assert_eq!(rows[0].2.len(), 32);
    assert_eq!(rows[0].2.last().map(|r| r.tag.as_str()), Some("s31"));
    {
        let lc = s.lc();
        let view = lc.at_tick(NetTickFrac(0.0)).expect("in history");
        let tag_at = |x: f32| {
            view.raycast(ray_z(x), 100.0, &none())
                .expect("q")
                .map(|h| h.tag.as_str().to_string())
        };
        assert_eq!(tag_at(31.0).as_deref(), Some("s31"));
        assert_eq!(tag_at(32.0), None, "the 33rd shape is not recorded");
    }
    s.settings().lag_compensation.max_shapes_per_entity = 4;
    s.end_tick(1);
    let rows = s.world.resource::<LagCompHistory>().history_of(e);
    let newest = rows.iter().find(|r| r.0 == NetTick(1)).expect("tick 1");
    assert_eq!(newest.2.len(), 4);
}

/// § 4 "entities that did not exist at a recorded tick have no entry for it": a target spawned at
/// tick 5 has no history before it and cannot be hit at an earlier tick; between ticks 4 and 5
/// it is used only where tick 5 is the nearer one (ADR-1752 (9)).
#[test]
fn an_entity_has_no_history_before_it_spawned() {
    let mut s = Server::new();
    for t in 0..5 {
        s.end_tick(t);
    }
    let e = s.body(Vec3::new(0.0, 0.0, 10.0));
    for t in 5..=8 {
        s.end_tick(t);
    }
    let rows = s.world.resource::<LagCompHistory>().history_of(e);
    assert_eq!(rows.iter().map(|r| r.0.0).collect::<Vec<_>>(), vec![5, 6, 7, 8]);
    let lc = s.lc();
    let hit_at = |tick: f64| {
        lc.at_tick(NetTickFrac(tick))
            .expect("in history")
            .raycast(ray_z(0.0), 100.0, &none())
            .expect("q")
            .is_some()
    };
    assert!(!hit_at(2.0), "no entry before the spawn tick");
    assert!(!hit_at(4.25), "tick 4 (no entry) is nearer");
    assert!(hit_at(4.75), "tick 5 is nearer");
    assert!(hit_at(5.0));
}

/// § 3 and § 9: `HitShapes` is a reflected component for "server and clients", saved in prefabs
/// and levels. A level saved on the server (or the editor) loads with its hit shapes on a client
/// process, which has `NetPlugin` but no `LagCompPlugin`, and on a server before `LagCompPlugin`.
#[test]
fn hit_shapes_save_in_a_level_and_load_on_every_process() {
    use regia_scene::components::EntityGuid;
    use regia_scene::format::SceneFile;
    use regia_scene::spawn::{SnapshotFilter, SpawnOptions, snapshot_scene, spawn_scene};

    let shapes = HitShapes {
        shapes: vec![
            body(),
            HitShape::new(
                HitShapeKind::Box {
                    half_extents: Vec3::new(0.1, 0.2, 0.3),
                },
                "limb",
            )
            .at(Vec3::X),
            HitShape::new(HitShapeKind::Sphere { radius: 0.15 }, "head").on_joint("head"),
        ],
        pose_source: PoseSource::Joints,
    };
    let mut saver = regia_app::App::new();
    saver.add_plugins(NetPlugin {
        mode: NetMode::DedicatedServer,
    });
    saver.add_plugins(LagCompPlugin);
    saver.world_mut().spawn((
        EntityGuid(regia_core::id::Guid(0x75)),
        Transform::default(),
        shapes.clone(),
        NoLagCompensation,
    ));
    let file = snapshot_scene(saver.world_mut(), &SnapshotFilter::default()).expect("snapshot");
    let text = file.to_ron_string().expect("ron");
    assert!(text.contains("HitShapes"), "{text}");

    for mode in [NetMode::Client, NetMode::DedicatedServer, NetMode::ListenServer] {
        let mut app = regia_app::App::new();
        app.add_plugins(NetPlugin { mode });
        let file = SceneFile::from_ron_str(&text).expect("parse");
        let spawned = spawn_scene(app.world_mut(), &file, &SpawnOptions::default()).expect("spawn");
        assert!(spawned.warnings.is_empty(), "{mode:?}: {:?}", spawned.warnings);
        let e = spawned.roots[0];
        assert_eq!(app.world().get::<HitShapes>(e), Some(&shapes), "{mode:?}");
        assert!(app.world().get::<NoLagCompensation>(e).is_some(), "{mode:?}");
    }
}

/// Spec 90 § 3.1 `max_shapes_per_entity`: only the first that many shapes are recorded and can
/// be hit; the rest are ignored (and logged once as `net.hitshape_invalid`).
#[test]
fn shapes_past_max_shapes_per_entity_are_not_recorded() {
    let mut s = Server::new();
    s.settings().lag_compensation.max_shapes_per_entity = 2;
    let sphere = |tag: &str, x: f32| {
        HitShape::new(HitShapeKind::Sphere { radius: 0.3 }, tag).at(Vec3::new(x, 1.0, 0.0))
    };
    let e = s.spawn(
        Vec3::new(0.0, 0.0, 10.0),
        vec![sphere("a", -2.0), sphere("b", 0.0), sphere("c", 2.0)],
    );
    for t in 0..3 {
        s.end_tick(t);
    }
    let rows = s.world.resource::<LagCompHistory>().history_of(e);
    assert_eq!(rows.len(), 3);
    let tags: Vec<&str> = rows[0].2.iter().map(|r| r.tag.as_str()).collect();
    assert_eq!(tags, ["a", "b"], "the first two shapes, in order");
    s.view(shooter(), 2.0);
    let hit_at = |s: &Server, x: f32| {
        s.lc()
            .raycast(shooter(), ray_z(x), 100.0, &none())
            .expect("query")
            .map(|h| h.tag.as_str().to_owned())
    };
    assert_eq!(hit_at(&s, -2.0).as_deref(), Some("a"));
    assert_eq!(hit_at(&s, 0.0).as_deref(), Some("b"));
    assert_eq!(hit_at(&s, 2.0), None, "the third shape was never recorded");

    // Raising the cap records all three from the next tick on.
    s.settings().lag_compensation.max_shapes_per_entity = 32;
    s.end_tick(3);
    let rows = s.world.resource::<LagCompHistory>().history_of(e);
    let newest = rows.iter().find(|r| r.0 == NetTick(3)).map_or(0, |r| r.2.len());
    assert_eq!(newest, 3);
}

/// Spec 90 § 3: `HitShapes` is reflected and saves in prefabs: every shape kind, a joint
/// attachment, an offset and the pose source survive the reflected RON round trip a prefab uses.
#[test]
fn hit_shapes_round_trip_through_reflected_ron() {
    use regia_ecs::reflect::TypeRegistry;
    let mut registry = TypeRegistry::default();
    regia_scene::components::register_primitives(&mut registry);
    registry.register::<HitShapes>();
    let shapes = HitShapes {
        shapes: vec![
            HitShape::new(HitShapeKind::Sphere { radius: 0.2 }, "head").on_joint("neck"),
            body(),
            HitShape::new(
                HitShapeKind::Box {
                    half_extents: Vec3::new(0.25, 0.5, 0.125),
                },
                "shield",
            )
            .at(Vec3::new(0.5, 1.0, 0.25)),
        ],
        pose_source: PoseSource::Joints,
    };
    let text = regia_ecs::serialize::to_ron(&shapes, &registry).expect("serialize");
    assert!(text.contains("HitShapes"), "keyed by its type path: {text}");
    let back: HitShapes = regia_ecs::serialize::from_ron_as(&text, &registry).expect("deserialize");
    assert_eq!(back, shapes);
}
