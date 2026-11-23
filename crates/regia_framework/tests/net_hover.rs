// SPDX-License-Identifier: MPL-2.0
//! Spec 73 § 9.2 (ADR-8819): hover hulls in the predicted schedule over the in-memory
//! `LoopbackHub` (fixed steps, latency in ticks, seeded loss). One dedicated server, one client that
//! owns and predicts a hull flying a scripted 600-tick run over the rolling lockstep field.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_detsim::hover::RollingField;
use regia_detsim::hover::host::FloorSampler;
use regia_framework::det_hover::{
    DetHoverFloor, DetHoverHull, DetHoverId, DetHoverInput, DetHoverPlugin, DetHoverState, DetHoverStats,
};
use regia_framework::net_hover::{
    FAULT_HULL_UNKNOWN, HoverNetInput, NetHover, NetHoverEffectKind, NetHoverEffects, NetHoverHulls,
    NetHoverStats, TOLERANCE_POSITION_M, hover_state_matches, install_net_hover, net_hover_rows,
    net_hover_step,
};
use regia_net::predict::{
    ClientPrediction, InputCommand, NetVisualOffset, PredictionState, PredictionStats, add_predicted_systems,
    force_correction, set_transport_rtt,
};
use regia_net::replication::NetPredicted;
use regia_net::testing::{LinkConditions, LoopbackHub};
use regia_net::{NetMode, NetOwner, NetRole, PeerId, Replicated, TransformReplication};
use regia_scene::Transform;

/// 4 ticks each way plus the tick in between: 9 ticks, 150 ms at 60 Hz.
const DELAY_150MS: u64 = 4;
/// The configured smoothing (`prediction.smooth_half_life_ms` 40 ms) takes an offset below 1 mm
/// within 400 ms (spec 73 § 13 test 2): 24 ticks.
const SETTLE_TICKS: u32 = 24;

/// Client: the input of each client tick.
#[derive(Resource, Clone, Copy)]
struct Script(fn(u32) -> HoverNetInput);

/// Each side: the state its predicted schedule left for each tick (the newest run of a re-run tick)
/// and the input the hull stepped with.
#[derive(Resource, Default)]
struct StateLog(BTreeMap<u32, (DetHoverState, DetHoverInput)>);

fn feed(script: Res<Script>, cp: Res<ClientPrediction>, mut q: Query<&mut InputCommand<HoverNetInput>>) {
    let next = cp.client_tick().0 + 1;
    for mut c in &mut q {
        c.value = (script.0)(next);
    }
}

fn record(
    state: Res<PredictionState>,
    hulls: Query<(&DetHoverState, &DetHoverInput, Option<&NetRole>)>,
    mut log: ResMut<StateLog>,
) {
    for (s, input, role) in &hulls {
        if state.simulates(role) {
            log.0.insert(state.tick.0, (*s, *input));
        }
    }
}

fn setup(world: &mut World) {
    install_net_hover::<HoverNetInput>(world).expect("net hover");
    world.insert_resource(DetHoverFloor::new(RollingField));
    world.init_resource::<StateLog>();
    add_predicted_systems(world, record.after(net_hover_step::<HoverNetInput>));
}

/// Thrust, carve, strafe, two hops a second apart and a jump, then idle from tick 640.
fn driving(t: u32) -> HoverNetInput {
    if !(60..640).contains(&t) {
        return HoverNetInput::default();
    }
    HoverNetInput {
        throttle: if (t / 90).is_multiple_of(4) { 0.4 } else { 1.0 },
        strafe: if (t / 50).is_multiple_of(3) { -0.6 } else { 0.0 },
        steer: ((t as f32) * 0.021).sin() * 0.8,
        hop: t % 120 == 100,
        jump: t == 333,
    }
}

fn hull_bundle(hull: DetHoverHull) -> impl Bundle {
    let floor = RollingField.floor(0.0, 0.0);
    (
        Replicated {
            transform: TransformReplication::Full,
            ..Replicated::default()
        },
        Transform::from_translation(Vec3::new(0.0, floor, 0.0)),
        hull,
        NetHover::for_hull(&hull, 0),
        DetHoverState::at_rest([0.0, floor, 0.0], 0.0, &hull.params),
        NetPredicted,
        NetOwner(PeerId(1)),
    )
}

struct Rig {
    hub: LoopbackHub,
    hull: Entity,
}

fn rig(seed: u64, delay: u64, script: fn(u32) -> HoverNetInput, hull: DetHoverHull) -> Rig {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, seed, setup);
    hub.down = LinkConditions {
        delay_ticks: delay,
        ..LinkConditions::default()
    };
    hub.up = hub.down;
    let c = hub.add_client();
    set_transport_rtt(
        &mut hub.clients[c].world,
        (2 * delay + 1) as f64 * 1000.0 / 60.0,
        0.0,
    );
    hub.clients[c].world.insert_resource(Script(script));
    hub.clients[c].gameplay.add_systems(feed);
    hub.server.world.resource_mut::<NetHoverHulls>().insert(hull);
    let hull = hub.server.world.spawn(hull_bundle(hull)).id();
    Rig { hub, hull }
}

fn stats(hub: &LoopbackHub) -> PredictionStats {
    hub.clients[0].world.resource::<ClientPrediction>().stats.clone()
}

fn predicted_copy(hub: &mut LoopbackHub) -> Option<Entity> {
    let w = &mut hub.clients[0].world;
    let mut q = w.query_filtered::<(Entity, &NetRole), With<NetHover>>();
    q.iter(w).find(|(_, r)| **r == NetRole::Predicted).map(|(e, _)| e)
}

fn offset(world: &World, e: Entity) -> Vec3 {
    world
        .get::<NetVisualOffset>(e)
        .copied()
        .unwrap_or_default()
        .translation
}

fn rendered(world: &World, e: Entity) -> Vec3 {
    world.get::<Transform>(e).map_or(Vec3::NAN, |t| t.translation) + offset(world, e)
}

fn pos(world: &World, e: Entity) -> Vec3 {
    world
        .get::<DetHoverState>(e)
        .map_or(Vec3::NAN, |s| Vec3::from_array(s.0.pos))
}

/// The largest second difference of a path: how hard it turns from one step to the next. A snap
/// shows as a spike the size of the jump.
fn roughness(path: &[Vec3]) -> f32 {
    path.windows(3)
        .map(|w| (w[2] - 2.0 * w[1] + w[0]).length())
        .fold(0.0, f32::max)
}

/// Commands the server did not have on time and repeated (spec 73 § 5.2).
fn server_missing(hub: &LoopbackHub) -> u64 {
    hub.server
        .world
        .resource::<regia_net::predict::ServerPrediction>()
        .players
        .values()
        .map(|p| p.missing)
        .sum()
}

/// What [`compare_logs`] found.
#[derive(Debug, Default)]
struct LogCompare {
    /// Ticks both sides logged.
    compared: usize,
    /// Ticks whose states differ although both sides stepped the same input: a misprediction the
    /// prediction should never make.
    bad: Vec<u32>,
    /// Ticks whose states differ after the server stepped another input since both sides last held
    /// the same bits (a lost or late command it repeated, spec 73 § 5.2): corrected by the
    /// rollback, expected under loss.
    repeated: Vec<u32>,
}

/// Every tick both sides logged inside `ticks`: the client's last prediction for the tick against
/// the server's state. A re-run overwrites the client's entries after the rollback tick, so a
/// difference left in the log is a tick the client never re-ran.
fn compare_logs(hub: &LoopbackHub, ticks: std::ops::Range<u32>) -> LogCompare {
    let server = &hub.server.world.resource::<StateLog>().0;
    let client = &hub.clients[0].world.resource::<StateLog>().0;
    let mut out = LogCompare::default();
    // An input the two sides stepped differently since they last held the same bits: its effect
    // may cross the tolerance a few ticks later, with equal inputs on the tick itself.
    let mut input_differed = false;
    for t in ticks {
        let (Some((s, si)), Some((c, ci))) = (server.get(&t), client.get(&t)) else {
            continue;
        };
        out.compared += 1;
        input_differed |= si != ci;
        if !hover_state_matches(c, s) {
            if input_differed {
                out.repeated.push(t);
            } else {
                out.bad.push(t);
            }
        }
        if c == s {
            input_differed = false;
        }
    }
    out
}

/// The server's newest logged state and the client's prediction for the same tick (the client
/// runs a lead ahead, so its current state is a later tick).
fn same_tick_end(hub: &LoopbackHub) -> (u32, DetHoverState, Option<DetHoverState>) {
    let server = &hub.server.world.resource::<StateLog>().0;
    let (t, s) = server
        .iter()
        .next_back()
        .map(|(t, s)| (*t, s.0))
        .expect("server log");
    let c = hub.clients[0].world.resource::<StateLog>().0.get(&t).map(|e| e.0);
    (t, s, c)
}

// ---- prediction under latency and loss ----------------------------------------------------------

/// A client predicts its hull for 760 ticks at 150 ms round trip with 2 % loss and reordering both
/// ways: no snap, the rendered path is no rougher than the server's own path beyond the position
/// tolerance, every tick the client predicted matches the server's state for that tick, and both
/// end on the same state.
#[test]
fn predicts_a_hull_at_150ms_and_2_percent_loss_without_a_visible_snap() {
    let mut r = rig(31, DELAY_150MS, driving, DetHoverHull::DEFAULT);
    for link in [&mut r.hub.down, &mut r.hub.up] {
        link.loss = 0.02;
        link.reorder = 0.05;
    }
    let mut client_path = Vec::new();
    let mut server_path = Vec::new();
    for _ in 0..760 {
        r.hub.step();
        server_path.push(pos(&r.hub.server.world, r.hull));
        if let Some(e) = predicted_copy(&mut r.hub) {
            client_path.push(rendered(&r.hub.clients[0].world, e));
        }
    }
    let s = stats(&r.hub);
    assert_eq!((s.snaps, s.resets), (0, 0), "{s:?}");
    assert!(client_path.len() > 600, "the client predicted the hull");
    let (rough_client, rough_server) = (roughness(&client_path), roughness(&server_path));
    assert!(
        rough_client <= rough_server + TOLERANCE_POSITION_M,
        "rendered path roughness {rough_client} vs the server's {rough_server}: a visible snap"
    );
    let logs = compare_logs(&r.hub, 0..760);
    assert!(logs.compared > 600, "{logs:?}");
    assert!(
        logs.bad.is_empty(),
        "mispredicted with the server's input: {logs:?} {s:?}"
    );
    assert!(logs.repeated.len() <= 2 * logs.compared / 100, "{logs:?}");
    let (t, a, b) = same_tick_end(&r.hub);
    let b = b.expect("the client predicted the server's last tick");
    assert!(hover_state_matches(&b, &a), "tick {t}: {a:?} vs {b:?}");
    assert!(Vec3::from_array(a.0.pos).length() > 20.0, "it flew: {a:?}");
    let hs = *r.hub.clients[0].world.resource::<NetHoverStats>();
    assert!(hs.hops + hs.jumps >= 3, "the script hopped and jumped: {hs:?}");
    assert_eq!(hs.unknown_hull, 0);
    eprintln!("150ms/2%: {s:?} {hs:?} roughness client {rough_client} server {rough_server}");
}

/// Without loss the client and the server step the same bits: no correction at all, and the
/// client's prediction for the server's last tick is bit-identical to it (one platform, spec 73 § 11).
#[test]
fn a_clean_link_never_corrects_and_ends_on_the_same_bits() {
    let mut r = rig(32, DELAY_150MS, driving, DetHoverHull::DEFAULT);
    r.hub.run(760);
    let s = stats(&r.hub);
    assert_eq!((s.corrections, s.resets, s.snaps), (0, 0, 0), "{s:?}");
    let (t, a, b) = same_tick_end(&r.hub);
    assert_eq!(Some(a), b, "bit-identical state at the server's last tick {t}");
    let logs = compare_logs(&r.hub, 0..760);
    assert!(logs.compared > 600, "{logs:?}");
    assert!(logs.bad.is_empty() && logs.repeated.is_empty(), "{logs:?}");
}

/// The same link with a 20-tick outage of everything the client sends (ticks 300..320) while the
/// hull carves: more than the 16-command redundancy, so the server repeats commands and the client
/// mispredicts for real. The correction smooths (no snap, the rendered path stays within a quarter
/// of the error of the server's own roughness) and from 40 ticks after the outage every prediction
/// matches the server again.
#[test]
fn an_input_outage_under_loss_corrects_smoothly() {
    let mut r = rig(36, DELAY_150MS, driving, DetHoverHull::DEFAULT);
    for link in [&mut r.hub.down, &mut r.hub.up] {
        link.loss = 0.02;
        link.reorder = 0.05;
    }
    r.hub.up_drop = Some(Box::new(|step, _, _| (300..320).contains(&step)));
    let mut client_path = Vec::new();
    let mut server_path = Vec::new();
    for _ in 0..760 {
        r.hub.step();
        server_path.push(pos(&r.hub.server.world, r.hull));
        if let Some(e) = predicted_copy(&mut r.hub) {
            client_path.push(rendered(&r.hub.clients[0].world, e));
        }
    }
    let missing = server_missing(&r.hub);
    let s = stats(&r.hub);
    assert!(missing > 0, "the outage cost the server commands");
    assert!(s.corrections >= 1, "{s:?}");
    assert_eq!((s.snaps, s.resets), (0, 0), "{s:?}");
    let (rough_client, rough_server) = (roughness(&client_path), roughness(&server_path));
    assert!(
        rough_client <= rough_server + 0.25 * s.max_error_m + TOLERANCE_POSITION_M,
        "rendered roughness {rough_client} vs the server's {rough_server} with a {} m error",
        s.max_error_m
    );
    let logs = compare_logs(&r.hub, 0..760);
    assert!(logs.compared > 600, "{logs:?}");
    assert!(
        logs.bad.is_empty(),
        "mispredicted with the server's input: {logs:?} {s:?}"
    );
    assert!(
        !logs.repeated.is_empty(),
        "the outage made the server repeat: {logs:?}"
    );
    assert!(
        logs.repeated.len() <= 2 * logs.compared / 100,
        "{logs:?} vs {missing} missing"
    );
    eprintln!(
        "outage: missing {missing}, repeated ticks {:?}, {s:?}, roughness client {rough_client} server {rough_server}",
        logs.repeated
    );
}

// ---- forced misprediction -----------------------------------------------------------------------

/// `net.force_correction` moves the server's hull by 1 m at tick 300 (the step takes the outside
/// move into its state): one correction, no snap, the client's predictions match the server's
/// states again from the corrected tick on, and the rendered offset is under 1 mm within the
/// configured 400 ms.
#[test]
fn a_server_push_corrects_within_the_configured_frames() {
    let mut r = rig(33, DELAY_150MS, driving, DetHoverHull::DEFAULT);
    let mut pushed = None;
    let mut corrected_at = None;
    let mut settled_at = None;
    let mut client_path = Vec::new();
    let mut server_path = Vec::new();
    for step in 0..720u32 {
        r.hub.step();
        if pushed.is_none() && r.hub.server_tick().0 >= 300 {
            let tick =
                force_correction(&mut r.hub.server.world, r.hull, Vec3::new(1.0, 0.0, 0.0)).expect("push");
            pushed = Some(tick.0);
        }
        server_path.push(pos(&r.hub.server.world, r.hull));
        let Some(e) = predicted_copy(&mut r.hub) else {
            continue;
        };
        let w = &r.hub.clients[0].world;
        client_path.push(rendered(w, e));
        let corrections = w.resource::<ClientPrediction>().stats.corrections;
        if corrected_at.is_none() && corrections > 0 {
            corrected_at = Some(step);
            assert!(offset(w, e).length() > 0.5, "the jump went into the offset");
        }
        if let Some(at) = corrected_at
            && settled_at.is_none()
            && offset(w, e).length() < 0.001
        {
            settled_at = Some(step - at);
        }
    }
    let s = stats(&r.hub);
    let pushed = pushed.expect("pushed");
    assert_eq!(s.corrections, 1, "{s:?}");
    assert_eq!((s.snaps, s.resets), (0, 0), "{s:?}");
    assert!(s.max_error_m > 0.9 && s.max_error_m < 1.1, "{s:?}");
    let settled = settled_at.expect("the offset decayed");
    assert!(settled <= SETTLE_TICKS, "offset under 1 mm after {settled} ticks");
    let logs = compare_logs(&r.hub, pushed + 1..720);
    assert!(logs.compared > 300, "{logs:?}");
    assert!(
        logs.bad.is_empty() && logs.repeated.is_empty(),
        "after the correction the predictions match the server: {logs:?}"
    );
    assert_eq!(r.hub.server.world.resource::<NetHoverStats>().adopted_moves, 1);
    // The rendered path pays the metre back over the half-life: it bends, it never jumps by it.
    assert!(roughness(&client_path) < 0.5, "{}", roughness(&client_path));
    assert!(roughness(&server_path) >= 0.9, "the server's own path did jump");
    eprintln!(
        "push: {s:?} settled after {settled} ticks, client roughness {}",
        roughness(&client_path)
    );
}

// ---- first-time effects (§ 8) -------------------------------------------------------------------

fn hop_at_200(t: u32) -> HoverNetInput {
    HoverNetInput {
        throttle: 0.5,
        hop: t == 200,
        ..HoverNetInput::default()
    }
}

/// Three server pushes force three re-runs across the hop at tick 200: the hop is played once on
/// the client and once on the server, while the client re-ran the hop tick more than once.
#[test]
fn a_rerun_never_replays_the_hop() {
    let mut r = rig(34, DELAY_150MS, hop_at_200, DetHoverHull::DEFAULT);
    let mut pushes = vec![196u32, 199, 201];
    let mut resims_at_hop = 0;
    for _ in 0..360 {
        r.hub.step();
        if pushes.first().is_some_and(|t| r.hub.server_tick().0 >= *t) {
            pushes.remove(0);
            let mut s = r
                .hub
                .server
                .world
                .get_mut::<DetHoverState>(r.hull)
                .expect("state");
            s.0.state.velocity[0] += 0.5;
        }
        let w = &r.hub.clients[0].world;
        let cp = w.resource::<ClientPrediction>();
        if cp.stats.resim_ticks_last > 0 && cp.client_tick().0 >= 200 {
            resims_at_hop += 1;
        }
    }
    assert!(pushes.is_empty());
    let s = stats(&r.hub);
    assert!(s.corrections >= 3, "{s:?}");
    assert!(resims_at_hop >= 3, "{resims_at_hop}");
    let client_hops: Vec<u32> = r.hub.clients[0]
        .world
        .resource::<NetHoverEffects>()
        .events
        .iter()
        .filter(|e| e.kind == NetHoverEffectKind::Hop)
        .map(|e| e.tick.0)
        .collect();
    assert_eq!(client_hops, vec![200], "played once, on its tick");
    let server_hops = r.hub.server.world.resource::<NetHoverEffects>().events.len();
    assert_eq!(server_hops, 1);
    let hs = *r.hub.clients[0].world.resource::<NetHoverStats>();
    assert_eq!(hs.hops, 1, "{hs:?}");
    assert!(hs.resim_steps > 0, "{hs:?}");
}

// ---- hull tuning --------------------------------------------------------------------------------

/// A hull whose tuning the client never listed is not stepped there and reports
/// `hover_hull_unknown`; listing it fixes it and the run predicts without a correction.
#[test]
fn an_unlisted_hull_is_not_guessed() {
    let wide = DetHoverHull::DEFAULT.with_footprint(3.0, 1.5);
    let mut r = rig(35, 2, driving, wide);
    r.hub.run(200);
    let hs = *r.hub.clients[0].world.resource::<NetHoverStats>();
    assert!(hs.unknown_hull > 0, "{hs:?}");
    assert_eq!(hs.steps, 0);
    let rows = net_hover_rows(&mut r.hub.clients[0].world);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].fault, Some(FAULT_HULL_UNKNOWN));
    assert_eq!(rows[0].role, "predicted");

    let mut r = rig(35, 2, driving, wide);
    r.hub.clients[0]
        .world
        .resource_mut::<NetHoverHulls>()
        .insert(wide);
    r.hub.run(400);
    let hs = *r.hub.clients[0].world.resource::<NetHoverStats>();
    assert_eq!(hs.unknown_hull, 0, "{hs:?}");
    assert!(hs.steps > 300);
    let s = stats(&r.hub);
    assert_eq!(s.corrections, 0, "{s:?}");
    let e = predicted_copy(&mut r.hub).expect("copy");
    assert_eq!(
        r.hub.clients[0].world.get::<DetHoverHull>(e),
        Some(&wide),
        "the copy took the listed tuning"
    );
    let rows = net_hover_rows(&mut r.hub.clients[0].world);
    assert_eq!(rows[0].fault, None);
    assert!(rows[0].hull_known);
}

// ---- the offline plugin leaves networked hulls alone --------------------------------------------

/// `DetHoverPlugin` steps its own hulls and skips a `NetHover` hull (it is stepped by the predicted
/// schedule), neither moving it nor writing its `Transform`.
#[test]
fn the_offline_plugin_skips_networked_hulls() {
    let mut app = App::new();
    app.add_plugins(DetHoverPlugin);
    enable_fixed_step(&mut app, 60.0);
    let hull = DetHoverHull::DEFAULT;
    let input = DetHoverInput {
        throttle: 1.0,
        ..DetHoverInput::default()
    };
    let start = DetHoverState::at_rest([0.0, 0.0, 0.0], 0.0, &hull.params);
    let own = app.world_mut().spawn((hull, DetHoverId(1), start, input)).id();
    let net = app
        .world_mut()
        .spawn((
            hull,
            DetHoverId(2),
            start,
            input,
            NetHover::for_hull(&hull, 0),
            Transform::from_translation(Vec3::new(5.0, 5.0, 5.0)),
        ))
        .id();
    for _ in 0..30 {
        app.update();
    }
    let w = app.world();
    assert!(w.get::<DetHoverState>(own).is_some_and(|s| s.0.pos[2] > 0.5));
    assert_eq!(w.get::<DetHoverState>(net).copied(), Some(start));
    assert_eq!(
        w.get::<Transform>(net).map(|t| t.translation),
        Some(Vec3::new(5.0, 5.0, 5.0))
    );
    let stats = *w.resource::<DetHoverStats>();
    assert_eq!(stats.hulls, 1, "only the offline hull stepped: {stats:?}");
    assert_eq!(stats.unordered, 0, "{stats:?}");
}
