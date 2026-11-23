// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 6.3 and § 9.3 (plan row P2, N209, ADR-8217): time dilation as a fractional tick
//! clock, input captured every frame and delivered on exactly one tick, late inputs merged into the
//! next unbuilt tick, and a burst of input packets accepted. Over the in-memory `LoopbackHub` with
//! seeded conditions: the bad-wifi shape of spec 120 § 15.1 (8 ms + 0-30 ms jitter, 1 % random loss
//! plus Gilbert-Elliott bursts, 1 % reorder, 0.5 % duplicates) and the clock-drift shape (even
//! clients' clocks 0.5 % fast, odd clients' 0.5 % slow), at 60 Hz; no wall clock, no sleeps.

use std::collections::{BTreeMap, BTreeSet};

use regia_ecs::prelude::*;
use regia_net::predict::{ClientPrediction, PlayerInputStats, ServerPrediction, set_transport_rtt};
use regia_net::replication::{NetPredicted, register_replicated};
use regia_net::runtime::Datagram;
use regia_net::settings::PredictionSettings;
use regia_net::testing::{FrameFate, LinkConditions, LoopbackHub, SplitMix64};
use regia_net::*;
use regia_scene::transform::Transform;

// ---- the test game ------------------------------------------------------------------------------

/// Walk on `x`; `jump` is a one-step press.
#[derive(Clone, Debug, PartialEq, Default, Replicate)]
struct Pad {
    x: i8,
    jump: bool,
}

impl NetInput for Pad {
    fn on_missing(last: &Self) -> Self {
        // The press does not repeat.
        Self {
            jump: false,
            ..last.clone()
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
/// prediction step): the client tick and the hero's rendered x.
#[derive(Resource, Default)]
struct Trace(Vec<Row>);

#[derive(Clone, Copy, Debug)]
struct Row {
    tick: u32,
    rendered_x: Option<f32>,
    corrections: u64,
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
        if pad.jump && (cx.is_server() || cx.first_time()) {
            *applied.0.entry(player.peer.0).or_insert(0) += 1;
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
        rendered_x,
        corrections: cp.stats.corrections,
        resyncs: cp.stats.resyncs,
        resets: cp.stats.resets,
    });
    frames.frame += 1;
    let press = cp.started && rendered_x.is_some() && frames.presses.contains(&frames.frame);
    if press {
        frames.issued += 1;
    }
    for mut c in &mut q {
        c.value = Pad { x: 1, jump: press };
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
        eprintln!(
            "{label} peer {}: {issued} presses issued, {client} applied on the client, {on_server} on the server; {ev} dilation events",
            c.peer.0
        );
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

// ---- acceptance ---------------------------------------------------------------------------------

/// P2 acceptance (spec 120 § 9.3): over more than 1,000 seeded dilation events on bad-wifi with
/// clock drift, every one-step press is applied exactly once on the client and on the server. At
/// the base a 0-tick frame never captured its input (the press was lost on both sides) and a 2-tick
/// frame captured it twice (the press fired twice).
#[test]
fn every_one_step_press_applies_exactly_once_over_1000_dilation_events() {
    const STEPS: u64 = 36_000;
    let mut events = 0;
    for seed in [41u64, 42] {
        let mut hub = rig(seed, 4, 1, STEPS);
        bad_wifi(&mut hub, seed, no_extra);
        hub.client_frames = Some(drift(seed, 4));
        hub.run(STEPS as usize);
        events += check_presses(&hub, &format!("seed {seed}"));
    }
    eprintln!("presses: {events} dilation events over two seeds, four clients each");
    assert!(events >= 1_000, "{events} dilation events");
}

/// P2 acceptance: during dilation the pawn's rendered per-frame displacement stays within 0.5x to
/// 1.5x of its steady per-tick step (no 0-tick or 2-tick hitch). Clock drift on a clean link, so
/// every non-unit frame is a dilation and nothing else moves the pawn.
#[test]
fn dilation_moves_the_pawn_without_a_hitch() {
    const STEPS: u64 = 24_000;
    let mut events = 0;
    for seed in [43u64, 44] {
        let mut hub = rig(seed, 2, 2, STEPS);
        for c in &mut hub.clients {
            c.world.resource_mut::<Frames>().presses.clear();
        }
        hub.client_frames = Some(drift(seed, 2));
        hub.run(STEPS as usize);
        for c in &hub.clients {
            let trace = &c.world.resource::<Trace>().0;
            let last = trace.last().expect("frames ran");
            assert_eq!(
                (last.corrections, last.resyncs, last.resets),
                (0, 0, 0),
                "seed {seed} peer {}: a clean drift run corrects nothing",
                c.peer.0
            );
            events += dilation_events(trace);
            // From 2 s after the first rendered frame.
            let start = trace.iter().position(|r| r.rendered_x.is_some()).expect("hero") + 120;
            for (i, w) in trace[start..].windows(2).enumerate() {
                let (Some(a), Some(b)) = (w[0].rendered_x, w[1].rendered_x) else {
                    continue;
                };
                let moved = b - a;
                assert!(
                    (0.5 * STEP_M..=1.5 * STEP_M).contains(&moved),
                    "seed {seed} peer {} frame {}: the pawn moved {moved} m in one frame (steady {STEP_M}); ticks {} -> {}",
                    c.peer.0,
                    start + i + 1,
                    w[0].tick,
                    w[1].tick
                );
            }
        }
    }
    eprintln!("displacement: {events} dilation events over two seeds, two clients each");
    assert!(events >= 200, "{events} dilation events");
}

/// P2 acceptance: a burst of 8 input packets that reaches the server in one tick from a good peer
/// (a Wi-Fi stall that flushed) is kept whole. At the base the per-tick cap of 4 dropped the newest
/// 4 packets as flood.
#[test]
fn a_burst_of_eight_input_packets_in_one_server_tick_is_kept() {
    let mut hub = rig(45, 1, 2, 2_000);
    hub.clients[0].world.resource_mut::<Frames>().presses.clear();
    hub.run(300);
    let start = hub.step_count() + 1;
    // The 8 input frames sent in steps start..start+7 all arrive together.
    hub.up_filter = Some(Box::new(move |d, step| {
        if is_input_frame(d) && (start..start + 8).contains(&step) {
            FrameFate::Delay(start + 7 - step)
        } else {
            FrameFate::Deliver
        }
    }));
    let before = player_stats(&hub, 1);
    hub.run(60);
    let after = player_stats(&hub, 1);
    assert_eq!(
        after.flood, before.flood,
        "every packet of the burst was kept: {after:?}"
    );
    // The packets held back for up to 7 ticks miss their ticks; each missed tick's command is in
    // a kept packet, arrives late and is merged into the next tick.
    assert_eq!(
        after.late - before.late,
        after.missing - before.missing,
        "every missed tick's command arrived: {after:?}"
    );
}

/// Spec 120 § 6.3 on the prediction path: a press whose every copy reaches the server after it
/// consumed that tick (a 12-tick stall, sent twice by duplication) is merged into the next unbuilt
/// tick once. At the base the server repeated the tick without the press and dropped the late copy.
#[test]
fn a_late_press_merges_into_the_next_unbuilt_tick_once() {
    let mut hub = rig(46, 1, 2, 2_000);
    {
        let mut f = hub.clients[0].world.resource_mut::<Frames>();
        f.presses = [400u64].into_iter().collect();
    }
    hub.up.duplicate = 1.0;
    // Stall every input frame of steps 395..=420 until step 432 (well past the press tick).
    hub.up_filter = Some(Box::new(|d, step| {
        if is_input_frame(d) && (395..=420).contains(&step) {
            FrameFate::Delay(432 - step)
        } else {
            FrameFate::Deliver
        }
    }));
    hub.run(600);
    let issued = hub.clients[0].world.resource::<Frames>().issued;
    assert_eq!(issued, 1);
    let st = player_stats(&hub, 1);
    assert!(st.missing > 0, "the stall made the server repeat ticks: {st:?}");
    let on_server = hub
        .server
        .world
        .resource::<Applied>()
        .0
        .get(&1)
        .copied()
        .unwrap_or(0);
    assert_eq!(on_server, 1, "the late press applied once on the server: {st:?}");
    let client = hub.clients[0]
        .world
        .resource::<Applied>()
        .0
        .get(&1)
        .copied()
        .unwrap_or(0);
    assert_eq!(client, 1, "and once on the client");
}

/// The settings the rig runs with are the defaults (the bars are measured on what ships).
#[test]
fn the_rig_uses_the_default_prediction_settings() {
    let hub = rig(48, 1, 1, 1_000);
    let s = hub.clients[0].world.resource::<NetSettings>().prediction.clone();
    assert_eq!(s, PredictionSettings::default());
}
