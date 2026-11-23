// SPDX-License-Identifier: MPL-2.0
//! `NetPlugin`: wires the net runtime into an app's schedules (spec 70 § 8.1).
//!
//! | Set | Schedule | Work |
//! |---|---|---|
//! | `NetSet::Receive` | `PreUpdate` | read the inbox into per-tick queues |
//! | `NetSet::Apply` | start of `FixedUpdate` | apply server ticks (client) / deliver to-server and local messages (server) |
//! | `NetSet::Gameplay` | `FixedUpdate` | game systems: `.in_set(NetSet::Gameplay)`, or `.after(NetSet::Apply).before(NetSet::Predict)` |
//! | `NetSet::Predict` | `FixedUpdate` | after gameplay: the predicted schedule of spec 73 (`crate::predict`) |
//! | `NetSet::Assign`, `NetSet::Gather`, `NetSet::LagCompRecord`, `NetSet::Send` | end of `FixedUpdate` | ids, changes, history, frames |
//! | `NetSet::Flush` | `PostUpdate` | the transport sends the outbox |
//! | `NetSet::Interpolate` | `PostUpdate` | proxy interpolation of spec 74 (`NetInterpolationPlugin`) |
//!
//! A frame that runs no fixed tick applies and sends nothing; a frame that runs three applies and
//! sends three times.

use regia_app::{App, FixedUpdate, Plugin, PostUpdate, PreUpdate, Time};
use regia_ecs::prelude::*;

use crate::model::{NetId, NetMode, NetOwnedLocally, NetOwner, NetOwnerLink, NetPlayer, NetRole};
use crate::replication::{
    NetAsleep, NetGroups, NetOutOfScope, NetPredicted, NetTeleported, NetUnbound, NetVelocity, Replicated,
};
use crate::runtime::{ensure_resources, net_apply, net_receive, net_send};
use crate::settings::NetSettings;

/// The net system sets, in run order.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetSet {
    /// `PreUpdate`: read packets into per-tick queues.
    Receive,
    /// Start of `FixedUpdate`: apply queued server ticks / deliver queued messages.
    Apply,
    /// `FixedUpdate`, after [`NetSet::Apply`] and before [`NetSet::Predict`]: game systems.
    ///
    /// Order gameplay `.in_set(NetSet::Gameplay)` or `.after(NetSet::Apply).before(NetSet::Predict)`
    /// so this tick's input and movement are visible to prediction (spec 70 § 8.1).
    Gameplay,
    /// `FixedUpdate`, after [`NetSet::Gameplay`]: the predicted schedule (spec 73).
    Predict,
    /// End of `FixedUpdate`: assign `NetId`s.
    Assign,
    /// End of `FixedUpdate`: gather changes.
    Gather,
    /// End of `FixedUpdate`: record hit-shape history (spec 75).
    LagCompRecord,
    /// End of `FixedUpdate`: write this tick's frames.
    Send,
    /// `PostUpdate`: hand frames to the transport.
    Flush,
    /// `PostUpdate`: proxy interpolation (spec 74).
    Interpolate,
}

/// Adds the net runtime to an app. `mode` is the process's [`NetMode`] (the session changes it
/// between sessions, never mid-tick).
#[derive(Debug, Clone, Copy, Default)]
pub struct NetPlugin {
    /// The mode the app starts in.
    pub mode: NetMode,
}

impl NetPlugin {
    /// Opts the game in to replicating [`NetInterrupted`](crate::NetInterrupted), so proxies of
    /// an interrupted peer's entities freeze on other clients (spec 120 § 10.2). Without it the
    /// server still marks the entities on the authority for its own gameplay, and proxies keep
    /// interpolating.
    ///
    /// Call it on every process, at the same point of the replicated-component registration
    /// order: it adds a component to the protocol hash, so a peer (or a recorded replay) that did
    /// not opt in is refused, as are v0.5.0 builds.
    pub fn replicate_interrupted_marker(app: &mut App) {
        crate::replication::register_replicated::<crate::model::NetInterrupted>(
            app.world_mut(),
            crate::replication::ReplicateOptions::default(),
        );
    }
}

fn receive_system(world: &mut World) {
    let _span = tracing::debug_span!("receive_system").entered();
    // Stall forgiveness (spec 70 § 6.6): a frame whose simulation step reached
    // `stall_forgive_ms` (a level load, a debugger stop) forgives the burst the next tick
    // receives. The decision reads the simulation clock (`Time::sim_delta`), never `RealTime`:
    // flood admission must be a pure function of the session inputs, so two runs fed the same
    // packet log with the same time strategy reach the same flood state (spec 70 § 8, ADR-4772).
    // Under `TimeUpdateStrategy::ManualDuration` or a `SimStep` (replays, captures, headless
    // servers) the wall clock cannot influence it at all.
    let forgive_ms = world
        .get_resource::<NetSettings>()
        .map_or(250, |s| s.messages.stall_forgive_ms);
    // Under `TimeUpdateStrategy::Automatic` the frame delta is clamped to `MAX_DELTA`, so a
    // threshold above the clamp can never be reached and a long stall (a 2 s level load) would
    // breach instead of forgiving. A frame that hit the clamp stands in for any threshold at or
    // above it: the clamped frame is at least `MAX_DELTA` long, and may be far longer
    // (audit fixrev-15-k net3-determinism M3, ADR-5444). `0` disables forgiveness.
    let clamp_ms = regia_app::MAX_DELTA.as_millis();
    let stalled = forgive_ms > 0
        && world.get_resource::<Time>().is_some_and(|t| {
            let sim_ms = t.sim_delta().as_millis();
            sim_ms >= u128::from(forgive_ms) || (sim_ms >= clamp_ms && u128::from(forgive_ms) >= clamp_ms)
        });
    if stalled && let Some(mut node) = world.get_resource_mut::<crate::runtime::NetNode>() {
        node.stalled = true;
    }
    net_receive(world);
}

fn apply_system(world: &mut World) {
    net_apply(world);
}

fn predict_system(world: &mut World) {
    #[cfg(test)]
    if let Some(mut probe) = world.get_resource_mut::<PredictProbe>() {
        probe.0.push("predict");
    }
    crate::predict::net_predict(world);
}

fn send_system(world: &mut World) {
    net_send(world);
}

impl Plugin for NetPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.mode);
        ensure_resources(app.world_mut());
        // Profiles through the asset server: built-ins as engine:// files, the project's cooked
        // `.netprofile.ron` files (Startup read, PreUpdate apply; ADR-7232).
        crate::profile_assets::install(app);
        // Reflection only: the type registry does not feed the protocol hash. Replicating the
        // marker is opt-in ([`NetPlugin::replicate_interrupted_marker`]), so the default plugin
        // keeps the v0.5.0 hash and component indices.
        app.register_type::<crate::model::NetInterrupted>();
        // Reflected so levels and prefabs save them and the inspector shows them.
        app.register_type::<Replicated>()
            .register_type::<NetGroups>()
            .register_type::<NetAsleep>()
            .register_type::<NetPredicted>()
            .register_type::<NetVelocity>()
            .register_type::<NetOwner>()
            .register_type::<NetOwnerLink>()
            .register_type::<NetPlayer>()
            .register_type::<NetId>()
            .register_type::<NetRole>()
            .register_type::<NetOwnedLocally>()
            .register_type::<NetUnbound>()
            .register_type::<NetOutOfScope>()
            .register_type::<NetTeleported>()
            .register_type::<NetMode>()
            .register_type::<crate::predict::TickDrivenMotion>()
            // Hit shapes are level and prefab content on every process (spec 75 § 3, ADR-5650);
            // `LagCompPlugin` adds the recorder on servers only.
            .register_type::<crate::lagcomp::HitShapes>()
            .register_type::<crate::lagcomp::NoLagCompensation>();
        app.configure_sets(
            FixedUpdate,
            (
                NetSet::Apply,
                NetSet::Gameplay,
                NetSet::Predict,
                NetSet::Assign,
                NetSet::Gather,
                NetSet::LagCompRecord,
                NetSet::Send,
            )
                .chain(),
        );
        app.configure_sets(PostUpdate, (NetSet::Flush, NetSet::Interpolate).chain());
        app.add_systems(PreUpdate, receive_system.in_set(NetSet::Receive));
        app.add_systems(FixedUpdate, apply_system.in_set(NetSet::Apply));
        app.add_systems(FixedUpdate, send_system.in_set(NetSet::Send));
        app.add_systems(FixedUpdate, predict_system.in_set(NetSet::Predict));
        // A local floating-origin rebase moves the buffers kept in the local frame (spec 78 § 12.2).
        app.add_message::<regia_scene::streaming::OriginShifted>()
            .add_systems(
                FixedUpdate,
                crate::origin::shift_net_origin
                    .after(regia_scene::streaming::StreamSet::Rebase)
                    .before(regia_scene::streaming::StreamSet::Plan),
            );
    }
}

/// Test-only record of which net set ran. Absent unless a test inserts it.
#[cfg(test)]
#[derive(Resource, Default)]
struct PredictProbe(Vec<&'static str>);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BitWriter;
    use crate::flood::FloodState;
    use crate::message::{Channel, Direction, NetMessage, NetMessageRegistry, register_net_message};
    use crate::model::{NetTick, PeerId};
    use crate::runtime::{Datagram, NetNode, server_add_peer};
    use regia_app::RealTime;
    use std::time::Duration;

    /// A to-server message with an empty payload: the burst filler of the flood tests.
    #[derive(Clone, Debug, PartialEq, regia_macros::Replicate)]
    struct FloodPing;

    impl NetMessage for FloodPing {
        const DIRECTION: Direction = Direction::ToServer;
    }

    /// `count` `FloodPing`s in one reliable client-to-server frame (`TAG_CMSG` = 3, the layout of
    /// `runtime::write_messages`).
    fn cmsg_frame(tick: NetTick, type_index: u16, count: u32) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write_bits(3, 8);
        w.write_varint(u64::from(tick.0));
        w.write_varint(u64::from(count));
        for _ in 0..count {
            w.write_varint(u64::from(type_index));
            w.write_bool(false);
            w.write_varint(0);
        }
        w.write_varint(0);
        w.finish()
    }

    /// One receive + one apply of a 600-message burst (over the default 512 per-peer per-tick
    /// cap) from one peer, on a world whose wall clock says `real_delta` and whose simulation
    /// clock says `sim_delta`. Returns the flood state and the delivered/rate-limited counts.
    fn burst_outcome(real_delta: Duration, sim_delta: Duration) -> (FloodState, u64, u64) {
        burst_outcome_with_forgive(real_delta, sim_delta, 250)
    }

    /// [`burst_outcome`] with an explicit `stall_forgive_ms` in the world's `NetSettings`.
    fn burst_outcome_with_forgive(
        real_delta: Duration,
        sim_delta: Duration,
        forgive_ms: u32,
    ) -> (FloodState, u64, u64) {
        let mut world = World::new();
        world.insert_resource(NetMode::ListenServer);
        crate::runtime::ensure_resources(&mut world);
        world.resource_mut::<NetSettings>().messages.stall_forgive_ms = forgive_ms;
        register_net_message::<FloodPing>(&mut world);
        server_add_peer(&mut world, PeerId(1));
        let type_index = world
            .resource::<NetMessageRegistry>()
            .index_of::<FloodPing>()
            .expect("registered");
        world.resource_mut::<NetNode>().inbox.push(Datagram {
            peer: PeerId(1),
            channel: Channel::ReliableOrdered,
            bytes: cmsg_frame(NetTick(0), type_index, 600),
        });
        let mut real = RealTime::default();
        real.advance_by(real_delta);
        world.insert_resource(real);
        let mut time = regia_app::Time::default();
        time.advance(sim_delta);
        world.insert_resource(time);
        receive_system(&mut world);
        crate::runtime::net_apply(&mut world);
        let (state, _, _) = world
            .resource::<NetNode>()
            .peer_flood(PeerId(1))
            .expect("flood state");
        let mut registry = world.resource_mut::<NetMessageRegistry>();
        let counters = registry.counters_mut(type_index).expect("counters");
        (state, counters.delivered, counters.rate_limited)
    }

    /// Audit fix-net3-determinism H1 (ADR-4772): stall forgiveness used to read `RealTime`, so
    /// the same inbox gave a `Slowed` peer on a 16 ms frame and a forgiven burst on a 300 ms
    /// frame. The wall clock must not influence flood admission: both worlds now breach.
    #[test]
    fn flood_state_ignores_the_wall_clock() {
        let fast = burst_outcome(Duration::from_millis(16), Duration::from_millis(16));
        let hitched = burst_outcome(Duration::from_millis(300), Duration::from_millis(16));
        assert_eq!(fast, hitched, "the same inbox, the same flood state");
        assert_eq!(fast.0, FloodState::Slowed, "an over-cap burst breaches");
    }

    /// The forgiveness follows the simulation clock: a frame whose `Time::sim_delta` reached
    /// `stall_forgive_ms` forgives the burst (spec 70 § 10 test 12's simulated stall), whatever
    /// the wall clock reports.
    #[test]
    fn a_simulated_stall_forgives_the_burst() {
        let stalled = burst_outcome(Duration::from_millis(16), Duration::from_millis(300));
        assert_eq!(stalled.0, FloodState::Clear, "the burst is forgiven");
        let smooth = burst_outcome(Duration::from_millis(16), Duration::from_millis(16));
        assert_eq!(smooth.0, FloodState::Slowed, "no stall, no forgiveness");
        assert_eq!(stalled.1, smooth.1, "forgiveness never admits more messages");
    }

    /// Audit fixrev-15-k net3-determinism M3 (ADR-5444): under `TimeUpdateStrategy::Automatic`
    /// the frame delta is clamped to `MAX_DELTA` (250 ms), so a threshold above the clamp was
    /// unreachable: a project that raised `stall_forgive_ms` to forgive only long level-load
    /// stalls forgave nothing, and a stalled peer escalated to a `Flood` disconnect. A frame at
    /// the clamp now reaches any threshold at or above it, and the comparison stays a pure
    /// function of the simulation clock.
    #[test]
    fn a_threshold_above_the_sim_clamp_is_reached_by_a_clamped_frame() {
        let stalled = burst_outcome_with_forgive(Duration::from_millis(16), Duration::from_millis(250), 1000);
        assert_eq!(
            stalled.0,
            FloodState::Clear,
            "a frame at the clamp forgives a threshold above it"
        );
        let smooth = burst_outcome_with_forgive(Duration::from_millis(16), Duration::from_millis(16), 1000);
        assert_eq!(smooth.0, FloodState::Slowed, "no stall, no forgiveness");
        let disabled = burst_outcome_with_forgive(Duration::from_millis(16), Duration::from_millis(250), 0);
        assert_eq!(
            disabled.0,
            FloodState::Slowed,
            "0 disables stall forgiveness entirely"
        );
    }

    /// Each `App::update` with fixed stepping runs exactly one fixed tick, so the server sends one
    /// frame per update and the tick counter advances by one.
    #[test]
    fn plugin_runs_one_net_tick_per_fixed_step() {
        let mut app = App::new();
        regia_app::enable_fixed_step(&mut app, 60.0);
        app.add_plugins(NetPlugin {
            mode: NetMode::ListenServer,
        });
        app.world_mut()
            .resource_mut::<NetSettings>()
            .replication
            .send_every_ticks = 1;
        crate::runtime::server_add_peer(app.world_mut(), crate::model::PeerId(1));
        for _ in 0..3 {
            app.update();
        }
        let node = app.world().resource::<NetNode>();
        assert_eq!(node.tick.0, 3);
        assert_eq!(node.outbox.len(), 3, "one tick frame per fixed step");
        let registry = app
            .world()
            .resource::<regia_ecs::prelude::AppTypeRegistry>()
            .read();
        assert!(
            registry.get(std::any::TypeId::of::<Replicated>()).is_some(),
            "Replicated is reflected"
        );
    }

    /// Spec 70 § 8.1: a system in `NetSet::Gameplay` runs after apply and before prediction.
    #[test]
    fn gameplay_runs_before_predict() {
        fn mark_gameplay(mut probe: ResMut<PredictProbe>) {
            probe.0.push("gameplay");
        }

        let mut app = App::new();
        regia_app::enable_fixed_step(&mut app, 60.0);
        app.add_plugins(NetPlugin {
            mode: NetMode::Offline,
        });
        app.insert_resource(PredictProbe::default());
        app.add_systems(FixedUpdate, mark_gameplay.in_set(NetSet::Gameplay));
        app.update();
        let order = &app.world().resource::<PredictProbe>().0;
        let gameplay = order.iter().position(|s| *s == "gameplay").expect("gameplay");
        let predict = order.iter().position(|s| *s == "predict").expect("predict");
        assert!(gameplay < predict, "{order:?}");
    }
}
