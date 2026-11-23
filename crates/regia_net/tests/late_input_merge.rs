// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 6.3 (N209, ADR-8217), the DO27 scenario on the prediction path: late inputs under
//! 0-300 ms per-peer latency with bursts, a blackout and duplicates lose or double no press. The
//! input counts its presses and overrides `NetInput::merge_late` to add them (two presses merged
//! into one tick stay two), as a game with one-shot fields does. Same rig and conditions as
//! `dilation_input_capture.rs` (bad-wifi and clock-drift shapes, seeded, no wall clock).

use std::collections::{BTreeMap, BTreeSet};

use regia_ecs::prelude::*;
use regia_net::predict::{ClientPrediction, PlayerInputStats, ServerPrediction, set_transport_rtt};
use regia_net::replication::{NetPredicted, register_replicated};
use regia_net::runtime::Datagram;
use regia_net::testing::{FrameFate, LinkConditions, LoopbackHub, SplitMix64};
use regia_net::*;
use regia_scene::transform::Transform;

// ---- the test game ------------------------------------------------------------------------------

/// Walk on `x`; `jumps` counts one-step presses.
#[derive(Clone, Debug, PartialEq, Default, Replicate)]
struct Pad {
    x: i8,
    jumps: u8,
}

impl NetInput for Pad {
    fn on_missing(last: &Self) -> Self {
        // Presses do not repeat.
        Self {
            jumps: 0,
            ..last.clone()
        }
    }

    fn merge_late(late: &Self, next: &Self) -> Self {
        // Presses add up; the newest analog value wins.
        Self {
            x: next.x,
            jumps: late.jumps.saturating_add(next.jumps),
        }
    }
}

/// Marks a hero (one per client).
#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Hero {
    id: u8,
}

/// Presses applied, by owning peer: on the server every simulated tick, on a client only the first
/// time a tick runs (a resimulation re-runs a press the player already saw).
#[derive(Resource, Default)]
struct Applied(BTreeMap<u32, u64>);

/// Client: the frame counter, the frames whose input holds a press, the presses issued.
#[derive(Resource, Default)]
struct Frames {
    frame: u64,
    presses: BTreeSet<u64>,
    issued: u64,
}

/// Client: one row per frame, sampled when the frame's gameplay runs (after the previous frame's
/// prediction step): the client tick and the resync and reset counters.
#[derive(Resource, Default)]
struct Trace(Vec<Row>);

#[derive(Clone, Copy, Debug)]
struct Row {
    tick: u32,
    resyncs: u64,
    resets: u64,
}

type Inputs<'w, 's> = Query<'w, 's, (&'static NetPlayer, &'static InputCommand<Pad>)>;
type Heroes<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut Transform,
        Option<&'static NetOwner>,
        Option<&'static NetRole>,
    ),
    With<Hero>,
>;
type RenderedHeroes<'w, 's> = Query<
    'w,
    's,
    (
        &'static Transform,
        Option<&'static NetVisualOffset>,
        &'static NetRole,
    ),
    With<Hero>,
>;

fn step_hero(cx: PredictionCx, inputs: Inputs, mut heroes: Heroes, mut applied: ResMut<Applied>) {
    for (mut t, owner, role) in &mut heroes {
        if !cx.simulates(role) {
            continue;
        }
        let Some(player) = cx.owner_player(owner) else {
            continue;
        };
        let pad = inputs
            .iter()
            .find(|(p, _)| p.id == player)
            .map(|(_, c)| c.value.clone())
            .unwrap_or_default();
        t.translation.x += f32::from(pad.x) * STEP_M;
        if pad.jumps > 0 && (cx.is_server() || cx.first_time()) {
            *applied.0.entry(player.peer.0).or_insert(0) += u64::from(pad.jumps);
        }
    }
}

/// One tick of walking.
const STEP_M: f32 = 0.1;

fn setup(world: &mut World) {
    register_replicated::<Hero>(world, ReplicateOptions::default());
    add_net_input::<Pad>(world).expect("one input type");
    predict::<Transform>(world, Tolerance::position(0.01)).expect("transform");
    add_predicted_systems(world, step_hero);
    world.init_resource::<Applied>();
}

/// Client gameplay, once per frame: sample the trace, then write this frame's input.
fn feed(
    mut frames: ResMut<Frames>,
    mut trace: ResMut<Trace>,
    cp: Res<ClientPrediction>,
    heroes: RenderedHeroes,
    mut q: Query<&mut InputCommand<Pad>>,
) {
    let rendered_x = heroes
        .iter()
        .find(|(_, _, r)| **r == NetRole::Predicted)
        .map(|(t, o, _)| o.map_or(*t, |o| o.apply(t)).translation.x);
    trace.0.push(Row {
        tick: cp.client_tick().0,
        resyncs: cp.stats.resyncs,
        resets: cp.stats.resets,
    });
    frames.frame += 1;
    let press = cp.started && rendered_x.is_some() && frames.presses.contains(&frames.frame);
    if press {
        frames.issued += 1;
    }
    for mut c in &mut q {
        c.value = Pad {
            x: 1,
            jumps: u8::from(press),
        };
    }
}

// ---- conditions ---------------------------------------------------------------------------------

/// The bad-wifi shape on one direction: 0-30 ms jitter (0 or 1 extra tick at 60 Hz), 1 % random
/// loss plus Gilbert-Elliott bursts (good -> bad 1 %, bad -> good 30 %, 60 % loss while bad).
/// `extra` adds a per-peer delay in ticks (the per-peer latency of the DO27 scenario).
struct BadWifi {
    rng: SplitMix64,
    bad: bool,
    extra: fn(PeerId, u64) -> u64,
}

impl BadWifi {
    fn fate(&mut self, d: &Datagram, step: u64) -> FrameFate {
        self.bad = if self.bad {
            self.rng.next_f64() >= 0.3
        } else {
            self.rng.next_f64() < 0.01
        };
        let loss = if self.bad { 0.6 } else { 0.01 };
        if self.rng.next_f64() < loss {
            return FrameFate::Drop;
        }
        let jitter = self.rng.next_u64() % 2;
        let extra = (self.extra)(d.peer, step);
        match jitter + extra {
            0 => FrameFate::Deliver,
            n => FrameFate::Delay(n),
        }
    }
}

fn no_extra(_: PeerId, _: u64) -> u64 {
    0
}

/// Clock drift: even clients run one extra frame every 200 hub steps (+0.5 %), odd clients skip one
/// (-0.5 %), each at its own seeded phase.
fn drift(seed: u64, clients: usize) -> Box<dyn FnMut(u64, usize) -> u32 + Send> {
    let mut rng = SplitMix64::new(seed ^ 0xd21f);
    let phases: Vec<u64> = (0..clients).map(|_| rng.next_u64() % 200).collect();
    Box::new(move |step, i| {
        if (step + phases[i]).is_multiple_of(200) {
            if i.is_multiple_of(2) { 2 } else { 0 }
        } else {
            1
        }
    })
}

/// Press frames: from frame 240 to `frames - 240`, one every 6..=40 frames.
fn press_frames(rng: &mut SplitMix64, frames: u64) -> BTreeSet<u64> {
    let mut out = BTreeSet::new();
    let mut f = 240 + rng.next_u64() % 40;
    while f + 240 < frames {
        out.insert(f);
        f += 6 + rng.next_u64() % 35;
    }
    out
}

// ---- rig ----------------------------------------------------------------------------------------

/// A dedicated server, `clients` clients at `delay` ticks each way, one predicted hero each.
fn rig(seed: u64, clients: usize, delay: u64, frames: u64) -> LoopbackHub {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, seed, setup);
    hub.down = LinkConditions {
        delay_ticks: delay,
        ..LinkConditions::default()
    };
    hub.up = hub.down;
    let mut rng = SplitMix64::new(seed);
    for i in 0..clients {
        let c = hub.add_client();
        let rtt_ticks = 2 * delay + 1;
        set_transport_rtt(&mut hub.clients[c].world, rtt_ticks as f64 * 1000.0 / 60.0, 0.0);
        hub.clients[c].world.insert_resource(Frames {
            presses: press_frames(&mut rng, frames),
            ..Frames::default()
        });
        hub.clients[c].world.init_resource::<Trace>();
        hub.clients[c].gameplay.add_systems(feed);
        let peer = hub.clients[c].peer;
        hub.server.world.spawn((
            Replicated {
                transform: TransformReplication::PositionOnly,
                ..Replicated::default()
            },
            Transform::default(),
            Hero { id: i as u8 },
            NetPredicted,
            NetOwner(peer),
        ));
    }
    hub
}

fn bad_wifi(hub: &mut LoopbackHub, seed: u64, extra_up: fn(PeerId, u64) -> u64) {
    for c in [&mut hub.down, &mut hub.up] {
        c.reorder = 0.01;
        c.duplicate = 0.005;
    }
    let mut up = BadWifi {
        rng: SplitMix64::new(seed ^ 0x0a),
        bad: false,
        extra: extra_up,
    };
    let mut down = BadWifi {
        rng: SplitMix64::new(seed ^ 0x0b),
        bad: false,
        extra: no_extra,
    };
    hub.up_filter = Some(Box::new(move |d, step| up.fate(d, step)));
    hub.down_filter = Some(Box::new(move |d, step| down.fate(d, step)));
}

/// Frames whose prediction step ran other than one tick (0 or 2), with no resync or reset around
/// them: dilation events.
fn dilation_events(trace: &[Row]) -> usize {
    trace
        .windows(2)
        .filter(|w| w[1].resyncs == w[0].resyncs && w[1].resets == w[0].resets && w[0].tick > 0)
        .filter(|w| w[1].tick.wrapping_sub(w[0].tick) != 1)
        .count()
}

fn player_stats(hub: &LoopbackHub, peer: u32) -> PlayerInputStats {
    hub.server
        .world
        .resource::<ServerPrediction>()
        .players
        .get(&NetPlayerId {
            peer: PeerId(peer),
            local: 0,
        })
        .cloned()
        .unwrap_or_default()
}

fn is_input_frame(d: &Datagram) -> bool {
    d.bytes.first() == Some(&5)
}

/// Checks every client of the hub: presses issued == presses applied on the client (first time) ==
/// presses applied on the server. Returns the dilation events seen.
fn check_presses(hub: &LoopbackHub, label: &str) -> usize {
    let server = &hub.server.world.resource::<Applied>().0;
    let mut events = 0;
    for c in &hub.clients {
        let issued = c.world.resource::<Frames>().issued;
        let client = c
            .world
            .resource::<Applied>()
            .0
            .get(&c.peer.0)
            .copied()
            .unwrap_or(0);
        let on_server = server.get(&c.peer.0).copied().unwrap_or(0);
        let ev = dilation_events(&c.world.resource::<Trace>().0);
        events += ev;
        assert!(issued > 100, "{label} peer {}: {issued} presses issued", c.peer.0);
        assert_eq!(
            client, issued,
            "{label} peer {}: presses applied on the client (first time) vs issued; {ev} dilation events",
            c.peer.0
        );
        assert_eq!(
            on_server, issued,
            "{label} peer {}: presses applied on the server vs issued; {ev} dilation events",
            c.peer.0
        );
    }
    events
}

// ---- the DO27 scenario -------------------------------------------------------------------------

/// Per-peer latency: peer 1 none, peer 2 6 ticks, peer 3 12, peer 4 18 (300 ms); plus a one-second
/// blackout from step 5,000 whose input is queued and flushed at its end.
fn latency_and_blackout(peer: PeerId, step: u64) -> u64 {
    let latency = u64::from(peer.0.saturating_sub(1)) * 6;
    let blackout = if (5_000..5_060).contains(&step) {
        5_060 - step
    } else {
        0
    };
    latency + blackout
}

/// The DO27 scenario (spec 120 § 6.3) on the prediction path: no press is lost or doubled, with
/// the late copies merged into later ticks. At the base a command that missed its tick was dropped.
#[test]
fn late_inputs_under_per_peer_latency_bursts_a_blackout_and_duplicates_keep_every_press() {
    const STEPS: u64 = 12_000;
    for seed in [47u64, 48] {
        let mut hub = rig(seed, 4, 1, STEPS);
        bad_wifi(&mut hub, seed, latency_and_blackout);
        hub.up.duplicate = 0.05;
        hub.client_frames = Some(drift(seed, 4));
        hub.run(STEPS as usize);
        check_presses(&hub, &format!("seed {seed}"));
        let late: u64 = (1..=4).map(|p| player_stats(&hub, p).late).sum();
        assert!(late > 0, "seed {seed}: the blackout made inputs late");
    }
}

/// A blackout of one second with presses inside it: every press arrives after its tick and all of
/// them merge into the first tick after the flush, counted, not collapsed.
#[test]
fn presses_inside_a_blackout_all_merge_into_the_next_tick() {
    let mut hub = rig(49, 1, 2, 2_000);
    hub.clients[0].world.resource_mut::<Frames>().presses = [400u64, 410, 425, 440].into_iter().collect();
    hub.up_filter = Some(Box::new(|d, step| {
        if is_input_frame(d) && (395..455).contains(&step) {
            FrameFate::Delay(455 - step)
        } else {
            FrameFate::Deliver
        }
    }));
    hub.up.duplicate = 1.0;
    hub.run(800);
    assert_eq!(hub.clients[0].world.resource::<Frames>().issued, 4);
    check_one(&hub);
}

fn check_one(hub: &LoopbackHub) {
    let st = player_stats(hub, 1);
    let on_server = hub
        .server
        .world
        .resource::<Applied>()
        .0
        .get(&1)
        .copied()
        .unwrap_or(0);
    let client = hub.clients[0]
        .world
        .resource::<Applied>()
        .0
        .get(&1)
        .copied()
        .unwrap_or(0);
    assert!(st.late > 0, "{st:?}");
    assert_eq!((client, on_server), (4, 4), "{st:?}");
}
