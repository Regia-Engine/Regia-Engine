// SPDX-License-Identifier: MPL-2.0
//! ADR-2413 (4): a replicated root's position crosses the wire on its profile's
//! `position_step_m` grid; a profile without a step sends raw `f32`; a child of a replicated
//! parent stays relative and unsnapped; motion under half a step does not reach the client.
//! In-process server and client over the loopback hub (no sockets).

use glam::Vec3;
use regia_ecs::prelude::*;
use regia_net::profile::{NetProfile, NetProfiles, PositionQuantizer};
use regia_net::replication::{Replicated, TransformReplication};
use regia_net::runtime::NetEntities;
use regia_net::testing::LoopbackHub;
use regia_net::*;
use regia_scene::transform::Transform;

fn copy_translation(hub: &LoopbackHub, e: Entity) -> Option<Vec3> {
    let id = hub.server.world.get::<NetId>(e).copied()?;
    let c = hub.clients[0].world.resource::<NetEntities>().entity(id)?;
    hub.clients[0].world.get::<Transform>(c).map(|t| t.translation)
}

fn replicated(profile: Option<&str>) -> Replicated {
    Replicated {
        profile: profile.map(str::to_string),
        transform: TransformReplication::PositionOnly,
        ..Replicated::default()
    }
}

#[test]
fn root_positions_arrive_on_the_profile_grid() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 0x2413, |_| {});
    hub.add_client();
    let mut profiles = NetProfiles::default();
    for (name, step) in [("coarse", Some(0.5_f32)), ("raw", None)] {
        let profile = NetProfile {
            position_step_m: step,
            ..NetProfile::default()
        };
        profiles.insert(name, profile).expect("valid profile");
    }
    hub.server.world.insert_resource(profiles);

    let p = Vec3::new(0.123_456, 7.777_7, -3.271_9);
    let spawn = |hub: &mut LoopbackHub, profile: Option<&str>| {
        hub.server
            .world
            .spawn((replicated(profile), Transform::from_translation(p)))
            .id()
    };
    let default = spawn(&mut hub, None);
    let coarse = spawn(&mut hub, Some("coarse"));
    let raw = spawn(&mut hub, Some("raw"));
    let child = hub
        .server
        .world
        .spawn((
            replicated(Some("coarse")),
            Transform::from_translation(p),
            ChildOf(coarse),
        ))
        .id();
    hub.run(4);

    let default_step = NetProfile::default().position_step_m.expect("default step");
    let mm = PositionQuantizer::new(default_step).expect("q").snap(p);
    assert_eq!(
        copy_translation(&hub, default),
        Some(mm),
        "built-in default: 1 mm grid"
    );
    assert_ne!(mm, p, "the grid moved this position");
    assert_eq!(copy_translation(&hub, coarse), Some(Vec3::new(0.0, 8.0, -3.5)));
    assert_eq!(copy_translation(&hub, raw), Some(p), "no step: raw f32");
    assert_eq!(
        copy_translation(&hub, child),
        Some(p),
        "a child stays relative and unsnapped"
    );

    // Under half a step: the client still holds the same grid point.
    hub.server
        .world
        .get_mut::<Transform>(coarse)
        .expect("t")
        .translation
        .x += 0.1;
    hub.run(4);
    assert_eq!(copy_translation(&hub, coarse), Some(Vec3::new(0.0, 8.0, -3.5)));
    // Past half a step: the next grid point.
    hub.server
        .world
        .get_mut::<Transform>(coarse)
        .expect("t")
        .translation
        .x = 0.3;
    hub.run(4);
    assert_eq!(copy_translation(&hub, coarse), Some(Vec3::new(0.5, 8.0, -3.5)));
}
