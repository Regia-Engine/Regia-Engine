// SPDX-License-Identifier: MPL-2.0
//! Spec 75 § 13 test 1 on spec 73's real input stream: a client's commands carry the view tick over
//! the hub, the server's command consumption sets `NetViewTick`, and shots at the target's shown
//! center are judged at that tick. Nothing here sets a view tick by hand.

use glam::Vec3;
use regia_core::math::Ray;
use regia_ecs::prelude::*;
use regia_net::interp::{NetInterpolation, install_interpolation, net_interpolate};
use regia_net::lagcomp::{install_lag_compensation, lagcomp_record};
use regia_net::predict::{ClientPrediction, set_transport_rtt};
use regia_net::replication::NetPredicted;
use regia_net::runtime::{NetEntities, NetNode};
use regia_net::testing::LoopbackHub;
use regia_net::*;
use regia_scene::transform::Transform;

const HZ: f64 = 60.0;
const MSPT: f64 = 1000.0 / HZ;
const ORIGIN: Vec3 = Vec3::new(0.0, 1.0, -12.0);

#[derive(Clone, Debug, PartialEq, Default, Replicate)]
struct Pad {
    fire: bool,
    aim: Vec3,
}

impl NetInput for Pad {
    fn on_missing(last: &Self) -> Self {
        Self {
            fire: false,
            aim: last.aim,
        }
    }
}

#[derive(Component)]
struct Target;

/// Client: the next command.
#[derive(Resource, Default)]
struct Aim(Pad);

/// Server: what the shots did, and how far back they were judged.
#[derive(Resource, Default)]
struct Tally {
    last_tick: u32,
    hits: u32,
    misses: u32,
    body_hits: u32,
    rewound: u32,
}

/// Triangle wave between -8 and 8 m at 6 m/s.
fn strafe_x(tick: u32) -> f32 {
    let leg = (16.0 / 6.0 * HZ) as u32;
    let phase = tick % (2 * leg);
    let s = if phase < leg { phase } else { 2 * leg - phase };
    -8.0 + 16.0 * s as f32 / leg as f32
}

fn server_strafe(world: &mut World) {
    let tick = world.resource::<NetNode>().tick.0;
    let mut q = world.query_filtered::<&mut Transform, With<Target>>();
    for mut t in q.iter_mut(world) {
        t.translation.x = strafe_x(tick);
    }
}

/// Predicted schedule, server side: each fired command becomes one rewound raycast.
fn judge(world: &mut World) {
    if !world.resource::<NetMode>().is_server() {
        return;
    }
    let mut q = world.query::<(&NetPlayer, &InputCommand<Pad>)>();
    let shots: Vec<(NetPlayerId, u32, Vec3)> = q
        .iter(world)
        .filter(|(_, c)| c.value.fire)
        .map(|(p, c)| (p.id, c.tick.0, c.value.aim))
        .collect();
    for (shooter, tick, aim) in shots {
        if tick <= world.resource::<Tally>().last_tick {
            continue;
        }
        let (hit, rewound) = {
            let lc = LagCompensation::new(world).expect("server");
            let at = lc.rewind_tick(shooter).expect("connected");
            let ray = Ray::new(ORIGIN, aim - ORIGIN);
            let hit = lc
                .raycast(shooter, ray, 100.0, &HitFilter::default())
                .expect("query");
            (hit, at.0 < f64::from(lc.current_tick().0))
        };
        let mut t = world.resource_mut::<Tally>();
        t.last_tick = tick;
        t.rewound += u32::from(rewound);
        match hit {
            Some(h) => {
                t.hits += 1;
                t.body_hits += u32::from(h.tag.as_str() == "body");
            }
            None => t.misses += 1,
        }
    }
}

fn feed(aim: Res<Aim>, _cp: Res<ClientPrediction>, mut q: Query<&mut InputCommand<Pad>>) {
    for mut c in &mut q {
        c.value = aim.0.clone();
    }
}

fn setup(world: &mut World) {
    add_net_input::<Pad>(world).expect("one input type");
    predict::<Transform>(world, Tolerance::position(0.01)).expect("transform");
    add_predicted_systems(world, judge);
    world.init_resource::<Tally>();
}

fn shoot_strafing_target(rewind: bool) -> Tally {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 75, setup);
    hub.down.delay_ticks = 3;
    hub.up.delay_ticks = 3;
    let c = hub.add_client();
    let peer = hub.clients[c].peer;
    let cw = &mut hub.clients[c].world;
    install_interpolation(cw, HZ, 1.0);
    set_transport_rtt(cw, 7.0 * MSPT, 0.0);
    cw.init_resource::<Aim>();
    hub.clients[c].gameplay.add_systems(feed);
    let sw = &mut hub.server.world;
    install_lag_compensation(sw);
    sw.resource_mut::<NetSettings>().lag_compensation.enabled = rewind;
    hub.server
        .gameplay
        .add_systems((server_strafe, lagcomp_record).chain());
    hub.server.world.spawn((
        Replicated {
            transform: TransformReplication::PositionOnly,
            ..Replicated::default()
        },
        Transform::default(),
        NetPredicted,
        NetOwner(peer),
    ));
    let target = hub
        .server
        .world
        .spawn((
            Target,
            Replicated::default(),
            Transform::from_translation(Vec3::new(strafe_x(0), 0.0, 0.0)),
            HitShapes {
                shapes: vec![
                    HitShape::new(
                        HitShapeKind::Capsule {
                            radius: 0.4,
                            half_height: 0.5,
                        },
                        "body",
                    )
                    .at(Vec3::new(0.0, 1.0, 0.0)),
                ],
                pose_source: PoseSource::Root,
            },
        ))
        .id();
    let mut fired = 0u32;
    for step in 1..=1600u64 {
        let now = step as f64 * MSPT;
        let cw = &mut hub.clients[c].world;
        cw.resource_mut::<NetInterpolation>().now_ms = now;
        net_interpolate(cw, now);
        let mut pad = Pad::default();
        if step >= 240 && step.is_multiple_of(3) && fired < 100 {
            let copy = hub
                .server
                .world
                .get::<NetId>(target)
                .copied()
                .and_then(|id| hub.clients[c].world.resource::<NetEntities>().entity(id));
            let shown = copy.and_then(|e| hub.clients[c].world.get::<Transform>(e));
            if let Some(t) = shown {
                pad.fire = true;
                pad.aim = t.translation + Vec3::Y;
                fired += 1;
            }
        }
        hub.clients[c].world.resource_mut::<Aim>().0 = pad;
        hub.step();
    }
    assert_eq!(fired, 100);
    let t = hub.server.world.resource_mut::<Tally>();
    Tally {
        last_tick: t.last_tick,
        hits: t.hits,
        misses: t.misses,
        body_hits: t.body_hits,
        rewound: t.rewound,
    }
}

/// With rewind on, 100 of 100 shots hit the body, every one judged in the past from the view tick
/// the input stream carried; with rewind off at least 90 of the same shots miss.
#[test]
fn strafing_target_is_hit_through_the_input_stream() {
    let on = shoot_strafing_target(true);
    assert_eq!((on.hits, on.misses, on.body_hits), (100, 0, 100), "with rewind");
    assert_eq!(on.rewound, 100, "every shot used the view tick from the wire");
    let off = shoot_strafing_target(false);
    assert_eq!(off.hits + off.misses, 100);
    assert!(
        off.misses >= 90,
        "without rewind only {} of 100 missed",
        off.misses
    );
}
