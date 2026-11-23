// SPDX-License-Identifier: MPL-2.0
//! Acceptance tests of the ECS hover (plan row mp-hover-ecs-agent).

use std::time::Duration;

use regia_detsim::hover::RollingField;
use regia_detsim::hover::host::{FleetInput, HoverFleet};

use super::*;

const HULLS: u64 = 16;
const TICKS: usize = 600;

/// A repeatable input stream: axes for hull `id` on tick `tick`, redrawn every 20 ticks
/// (splitmix64 of the pair, three axes in -1..1 in steps of 1/8).
fn axes_for(id: u64, tick: u64) -> [f32; 3] {
    let mut z = (id << 32 ^ (tick / 20)).wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    let axis = |shift: u32| ((z >> shift) & 0xf) as f32 / 8.0 - 1.0;
    [axis(0), axis(8), axis(16)]
}

/// Hull type of hull `id`: odd ids are a heavier, higher-riding hull on a larger footprint.
fn hull_for(id: u64) -> DetHoverHull {
    if id.is_multiple_of(2) {
        DetHoverHull::DEFAULT
    } else {
        let params = HoverParams {
            ride_height: HoverParams::DEFAULT_HULL.ride_height * 1.5,
            max_forward_speed: HoverParams::DEFAULT_HULL.max_forward_speed * 0.8,
            ..HoverParams::DEFAULT_HULL
        };
        DetHoverHull::new(params, HoverLimits::DEFAULT, FlightMode::AirTime).with_footprint(3.0, 1.5)
    }
}

/// Start position of hull `id`: a 4 x 4 grid 12 m apart over the rolling field.
fn start_for(id: u64) -> HoverBody {
    let x = (id % 4) as f32 * 12.0 - 18.0;
    let z = (id / 4) as f32 * 12.0 - 18.0;
    let floor = RollingField.floor(x, z);
    HoverBody::at_rest([x, floor, z], id as f32 * 0.4, &hull_for(id).params)
}

/// Every hull's position and state words, in id order, as text: `{:?}` of an `f32` round-trips,
/// so two equal strings are equal bits (and `-0.0` prints apart from `0.0`).
fn snapshot(bodies: &[(u64, HoverBody)]) -> String {
    let mut sorted = bodies.to_vec();
    sorted.sort_by_key(|(id, _)| *id);
    format!("{sorted:?}")
}

/// The tick counter of an input script and the per-tick log of a recorder.
#[derive(Resource, Default)]
struct Script {
    tick: u64,
    log: Vec<String>,
}

/// `FixedUpdate`, before the step: every hull's input for this tick, from its id and the tick.
fn script_inputs(mut script: ResMut<Script>, mut hulls: Query<(&DetHoverId, &mut DetHoverInput)>) {
    let tick = script.tick;
    for (id, mut input) in &mut hulls {
        *input = DetHoverInput::from_axes(axes_for(id.0, tick));
    }
    script.tick += 1;
}

/// `FixedUpdate`, after the step: log every hull's state.
fn record(mut script: ResMut<Script>, hulls: Query<(&DetHoverId, &DetHoverState)>) {
    let bodies: Vec<(u64, HoverBody)> = hulls.iter().map(|(id, s)| (id.0, s.0)).collect();
    let line = snapshot(&bodies);
    script.log.push(line);
}

/// An app with the hover plugin over the rolling field, the input script and the recorder.
fn app(frame: Duration) -> App {
    let mut app = App::new();
    app.add_plugins(DetHoverPlugin);
    app.insert_resource(DetHoverFloor::new(RollingField));
    app.init_resource::<Script>();
    app.add_systems(FixedUpdate, script_inputs.before(DetHoverSet::Step));
    app.add_systems(FixedUpdate, record.after(DetHoverSet::Step));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(frame));
    app
}

fn spawn_hulls(app: &mut App, order: &[u64]) {
    for &id in order {
        app.world_mut().spawn((
            hull_for(id),
            DetHoverId(id),
            DetHoverState(start_for(id)),
            Transform::default(),
        ));
    }
}

/// Run `app` until `ticks` fixed ticks are logged (or a frame bound runs out) and return the log.
fn run(app: &mut App, ticks: usize) -> Vec<String> {
    for _ in 0..ticks * 8 {
        if app.world().resource::<Script>().log.len() >= ticks {
            break;
        }
        app.update();
    }
    let log = &app.world().resource::<Script>().log;
    assert!(log.len() >= ticks, "only {} of {ticks} ticks ran", log.len());
    log[..ticks].to_vec()
}

const FRAME_60: Duration = Duration::from_nanos(16_666_667);

#[test]
fn det_hover_system_matches_the_fleet() {
    // The fleet: the same hulls in id order, the inputs handed over in descending slot order.
    let types: Vec<_> = [hull_for(0), hull_for(1)]
        .iter()
        .map(DetHoverHull::fleet_hull)
        .collect();
    let mut fleet = HoverFleet::new(types);
    for id in 0..HULLS {
        let slot = fleet.push(start_for(id), (id % 2) as u32);
        assert_eq!(u64::from(slot), id);
    }
    let mut expected = Vec::with_capacity(TICKS);
    for tick in 0..TICKS as u64 {
        let inputs: Vec<FleetInput> = (0..HULLS)
            .rev()
            .map(|id| FleetInput::new(id as u32, axes_for(id, tick)))
            .collect();
        let report = fleet.step_all(
            &inputs,
            &RollingField,
            &WorldPhysics::EARTH,
            &HoverLimits::DEFAULT,
            &ClimbTuning::AIR_TIME,
            tick_dt(),
        );
        assert_eq!((report.stepped, report.recovered), (HULLS as u32, 0));
        let bodies: Vec<(u64, HoverBody)> = fleet
            .bodies()
            .iter()
            .enumerate()
            .map(|(i, b)| (i as u64, *b))
            .collect();
        expected.push(snapshot(&bodies));
    }

    // Two ECS worlds that spawn the hulls in two different orders.
    let forward: Vec<u64> = (0..HULLS).collect();
    let shuffled: Vec<u64> = vec![11, 3, 14, 0, 7, 9, 15, 2, 5, 12, 1, 8, 13, 6, 10, 4];
    for order in [forward, shuffled] {
        let mut app = app(FRAME_60);
        spawn_hulls(&mut app, &order);
        let log = run(&mut app, TICKS);
        for (tick, (got, want)) in log.iter().zip(&expected).enumerate() {
            assert_eq!(
                got, want,
                "spawn order {order:?}: tick {tick} differs from the fleet"
            );
        }
        let stats = *app.world().resource::<DetHoverStats>();
        assert_eq!(stats.hulls, HULLS as u32);
        assert_eq!((stats.duplicate_ids, stats.unordered, stats.recovered), (0, 0, 0));
    }
}

#[test]
fn det_hover_ignores_frame_rate() {
    let order: Vec<u64> = (0..HULLS).collect();
    let mut logs = Vec::new();
    for fps in [30_u64, 60, 144] {
        let mut app = app(Duration::from_nanos(1_000_000_000 / fps));
        spawn_hulls(&mut app, &order);
        logs.push((fps, run(&mut app, TICKS)));
    }
    let (_, reference) = &logs[1];
    for (fps, log) in &logs {
        for (tick, (got, want)) in log.iter().zip(reference).enumerate() {
            assert_eq!(got, want, "{fps} fps: tick {tick} differs from 60 fps");
        }
    }
}

#[test]
fn det_hover_rollback_replays_bit_for_bit() {
    // The pure tick: save at tick 100, run to 200, roll back, replay the same inputs.
    let hull = hull_for(1);
    let world = WorldPhysics::EARTH;
    let hop = hop_speed(&hull, &world, tick_dt());
    let input_at = |tick: u64| DetHoverInput {
        hop: tick % 90 == 45,
        jump: tick == 150,
        ..DetHoverInput::from_axes(axes_for(1, tick))
    };
    let mut body = start_for(1);
    let mut saved = body;
    let mut first = Vec::new();
    for tick in 0..200 {
        if tick == 100 {
            saved = body;
        }
        tick_hull(
            &hull,
            &mut body,
            &input_at(tick),
            &RollingField,
            &world,
            hop,
            tick_dt(),
        );
        if tick >= 100 {
            first.push(format!("{body:?}"));
        }
    }
    let mut replay = saved;
    for (i, tick) in (100..200).enumerate() {
        tick_hull(
            &hull,
            &mut replay,
            &input_at(tick),
            &RollingField,
            &world,
            hop,
            tick_dt(),
        );
        assert_eq!(format!("{replay:?}"), first[i], "replayed tick {tick}");
    }

    // The same through the system: restore the saved components and run again.
    let mut app = app(FRAME_60);
    spawn_hulls(&mut app, &(0..HULLS).collect::<Vec<_>>());
    run(&mut app, 50);
    let mut q = app.world_mut().query::<(Entity, &DetHoverState)>();
    let saved: Vec<(Entity, DetHoverState)> = q.iter(app.world()).map(|(e, s)| (e, *s)).collect();
    let after = run(&mut app, 100)[50..].to_vec();
    for (e, s) in &saved {
        if let Some(mut state) = app.world_mut().get_mut::<DetHoverState>(*e) {
            *state = *s;
        }
    }
    {
        let mut script = app.world_mut().resource_mut::<Script>();
        script.tick = 50;
        script.log.truncate(50);
    }
    let again = run(&mut app, 100)[50..].to_vec();
    assert_eq!(after, again, "a rollback to tick 50 replays the same 50 ticks");
}

#[test]
fn transforms_and_lean_are_written_and_never_read_back() {
    let mut a = app(FRAME_60);
    spawn_hulls(&mut a, &[0, 1]);
    let mut b = app(FRAME_60);
    spawn_hulls(&mut b, &[0, 1]);
    for _ in 0..120 {
        a.update();
        // Scribble on b's presentation between ticks: the sim must not see it.
        let mut q = b.world_mut().query::<(&mut Transform, &mut DetHoverLean)>();
        for (mut t, mut lean) in q.iter_mut(b.world_mut()) {
            t.translation = Vec3::splat(1.0e6);
            lean.pitch = 9.0;
            lean.roll = -9.0;
        }
        b.update();
    }
    assert_eq!(
        a.world().resource::<Script>().log,
        b.world().resource::<Script>().log
    );
    let mut q = a.world_mut().query::<(
        &DetHoverState,
        &Transform,
        &DetHoverLean,
        &DetHoverHull,
        &DetHoverInput,
    )>();
    let mut seen = 0;
    for (state, transform, lean, hull, input) in q.iter(a.world()) {
        let [x, y, z] = state.0.pos;
        assert_eq!(transform.translation, Vec3::new(x, y, z));
        let (pitch, roll) = compute_lean(&hull.params, &state.0.state, &input.axes());
        assert_eq!((lean.pitch, lean.roll), (pitch, roll));
        assert_eq!(transform.rotation, hull_rotation(&state.0.state, lean));
        seen += 1;
    }
    assert_eq!(seen, 2);
}

#[test]
fn hop_and_jump_fire_from_the_input() {
    let mut app = App::new();
    app.add_plugins(DetHoverPlugin);
    enable_fixed_step(&mut app, 60.0);
    let hull = app
        .world_mut()
        .spawn((
            DetHoverHull::DEFAULT,
            DetHoverId(7),
            DetHoverState::at_rest([0.0; 3], 0.0, &HoverParams::DEFAULT_HULL),
            DetHoverInput {
                hop: true,
                ..DetHoverInput::default()
            },
        ))
        .id();
    let mut airborne_ticks = 0;
    for _ in 0..120 {
        app.update();
        if app
            .world()
            .get::<DetHoverState>(hull)
            .is_some_and(|s| s.0.state.airborne)
        {
            airborne_ticks += 1;
        }
    }
    let stats = *app.world().resource::<DetHoverStats>();
    assert!(stats.hops >= 1, "a held hop fires: {stats:?}");
    assert!(airborne_ticks > 10, "the hull flew after the hop");
    // The cooldown (0.8 s by default) spaces a held hop: two seconds give at most three.
    assert!(stats.hops <= 3, "{stats:?}");

    if let Some(mut input) = app.world_mut().get_mut::<DetHoverInput>(hull) {
        *input = DetHoverInput {
            jump: true,
            ..DetHoverInput::default()
        };
    }
    for _ in 0..240 {
        app.update();
    }
    assert!(app.world().resource::<DetHoverStats>().jumps >= 1);
}

#[test]
fn hulls_without_an_id_wait_and_shared_ids_are_counted() {
    let mut app = App::new();
    app.add_plugins(DetHoverPlugin);
    enable_fixed_step(&mut app, 60.0);
    let input = DetHoverInput::from_axes([1.0, 0.0, 0.0]);
    let lone = app
        .world_mut()
        .spawn((DetHoverHull::DEFAULT, DetHoverState::default(), input))
        .id();
    app.world_mut()
        .spawn((DetHoverHull::DEFAULT, DetHoverId(3), input));
    app.world_mut()
        .spawn((DetHoverHull::DEFAULT, DetHoverId(3), input));
    app.world_mut()
        .spawn((DetHoverHull::DEFAULT, DetHoverId(4), input));
    for _ in 0..30 {
        app.update();
    }
    let stats = *app.world().resource::<DetHoverStats>();
    assert!(stats.ticks > 0);
    assert_eq!((stats.hulls, stats.duplicate_ids, stats.unordered), (3, 2, 1));
    assert_eq!(
        app.world().get::<DetHoverState>(lone).map(|s| s.0),
        Some(DetHoverState::default().0),
        "a hull without an id is not stepped"
    );
}

#[test]
fn a_bad_dt_skips_the_tick() {
    let mut body = start_for(0);
    let before = body;
    let tick = tick_hull(
        &DetHoverHull::DEFAULT,
        &mut body,
        &DetHoverInput::from_axes([1.0, 0.0, 0.0]),
        &RollingField,
        &WorldPhysics::EARTH,
        0.0,
        f32::NAN,
    );
    assert_eq!(tick, DetHoverTick::default());
    assert_eq!(format!("{body:?}"), format!("{before:?}"));

    let mut app = App::new();
    app.add_plugins(DetHoverPlugin);
    enable_fixed_step(&mut app, 60.0);
    app.world_mut().resource_mut::<DetHoverConfig>().dt = 0.0;
    app.world_mut().spawn((DetHoverHull::DEFAULT, DetHoverId(1)));
    for _ in 0..10 {
        app.update();
    }
    let stats = *app.world().resource::<DetHoverStats>();
    assert_eq!(stats.skipped, stats.ticks);
    assert!(stats.ticks > 0);
}

#[test]
fn the_tick_matches_the_fleet_step_for_one_hull() {
    // tick_hull without hop or jump is one fleet step, bit for bit, for a hull with its own limits.
    let hull = hull_for(1);
    let mut fleet = HoverFleet::new(vec![hull.fleet_hull()]);
    let slot = fleet.push(start_for(1), 0);
    let mut body = start_for(1);
    for tick in 0..300 {
        let axes = axes_for(1, tick);
        tick_hull(
            &hull,
            &mut body,
            &DetHoverInput::from_axes(axes),
            &RollingField,
            &WorldPhysics::EARTH,
            0.0,
            tick_dt(),
        );
        fleet.step_all(
            &[FleetInput::new(slot, axes)],
            &RollingField,
            &WorldPhysics::EARTH,
            &hull.limits,
            hull.climb(),
            tick_dt(),
        );
        assert_eq!(
            format!("{body:?}"),
            format!("{:?}", fleet.body(slot)),
            "tick {tick}"
        );
    }
}

#[test]
fn hull_validation_names_the_field() {
    assert_eq!(DetHoverHull::DEFAULT.validate(), Ok(()));
    let bad = DetHoverHull::DEFAULT.with_footprint(f32::NAN, 1.0);
    let err = bad.validate().err();
    assert_eq!(
        err.map(|e| (e.field, e.kind.code())),
        Some(("half_length", "not_finite"))
    );
    let bad = DetHoverHull::DEFAULT.with_footprint(2.0, -1.0);
    assert_eq!(bad.validate().err().map(|e| e.kind.code()), Some("negative"));
    let bad = DetHoverHull::DEFAULT.with_footprint(2.0, 101.0);
    assert_eq!(bad.validate().err().map(|e| e.kind.code()), Some("out_of_range"));
    let mut bad = DetHoverHull::DEFAULT;
    bad.params.ride_height = f32::INFINITY;
    assert_eq!(bad.validate().err().map(|e| e.field), Some("ride_height"));
}

#[test]
fn lookahead_aim_rests_at_the_ride_height_on_flat_ground() {
    let hull = DetHoverHull::DEFAULT;
    let state = HoverState::default();
    assert_eq!(lookahead_aim(&hull, &state), hull.params.ride_height);
    let samples = attitude_samples(
        &hull,
        [0.0, 0.0, 0.0],
        0.0,
        &regia_detsim::hover::host::FlatFloor(2.0),
    );
    assert_eq!(samples[0], [0.0, 2.0, 2.0], "nose");
    assert_eq!(samples[2], [1.0, 2.0, 0.0], "right");
}

#[test]
fn hop_speed_cache_solves_once_per_key() {
    let mut cache = HopSpeedCache::default();
    let a = cache.get(&DetHoverHull::DEFAULT, &WorldPhysics::EARTH, tick_dt());
    let b = cache.get(&DetHoverHull::DEFAULT, &WorldPhysics::EARTH, tick_dt());
    assert_eq!(a.to_bits(), b.to_bits());
    assert_eq!(
        a.to_bits(),
        hop_speed(&DetHoverHull::DEFAULT, &WorldPhysics::EARTH, tick_dt()).to_bits()
    );
    assert_eq!(cache.0.len(), 1);
}

#[cfg(feature = "client")]
#[test]
fn the_terrain_floor_is_the_terrain_sampler() {
    let heights: Vec<f32> = (0..16 * 16)
        .map(|i| (i % 16) as f32 * 0.25 + (i / 16) as f32 * 0.1)
        .collect();
    let map = regia_terrain::TerrainMap::from_heights(16, 16, heights, 60.0, 1.0, 0.0);
    let Ok(map) = map else {
        panic!("a valid map");
    };
    let floor = DetHoverFloor::from_terrain(&map);
    for (x, z) in [(0.0, 0.0), (-12.3, 7.7), (29.0, -29.0), (100.0, 0.0)] {
        assert_eq!(floor.floor(x, z).to_bits(), map.sample_bilinear(x, z).to_bits());
    }
}

#[test]
fn hover_probe_numbers_are_stable() {
    use super::probe::{ProbeScript, probe};
    for script in ProbeScript::ALL {
        let a = probe(&DetHoverHull::DEFAULT, &WorldPhysics::EARTH, tick_dt(), script);
        let b = probe(&DetHoverHull::DEFAULT, &WorldPhysics::EARTH, tick_dt(), script);
        assert_eq!(format!("{a:?}"), format!("{b:?}"), "{}", script.name());
        assert_eq!(ProbeScript::from_name(script.name()), Some(script));
        assert!(a.ticks > 0 && a.ticks <= 100_000);
        // The default hull's numbers, for the book (`--nocapture`).
        eprintln!("regia-hover-probe {} {a:?}", script.name());
    }
    let accel = probe(
        &DetHoverHull::DEFAULT,
        &WorldPhysics::EARTH,
        tick_dt(),
        ProbeScript::Accel,
    );
    let top = accel.top_speed_m_s.unwrap_or(0.0);
    assert!(top > 5.0, "the default hull has a top speed: {accel:?}");
    assert!(
        accel.time_to_90_s.is_some_and(|t| t > 0.0 && t < 30.0),
        "{accel:?}"
    );
    let brake = probe(
        &DetHoverHull::DEFAULT,
        &WorldPhysics::EARTH,
        tick_dt(),
        ProbeScript::Brake,
    );
    assert!(brake.stop_distance_m.is_some_and(|d| d > 0.0), "{brake:?}");
    let turn = probe(
        &DetHoverHull::DEFAULT,
        &WorldPhysics::EARTH,
        tick_dt(),
        ProbeScript::Turn,
    );
    assert!(
        turn.turn_radius_m.is_some_and(|r| r > 0.0 && r.is_finite()),
        "{turn:?}"
    );
    let hop = probe(
        &DetHoverHull::DEFAULT,
        &WorldPhysics::EARTH,
        tick_dt(),
        ProbeScript::Hop,
    );
    let apex = hop.hop_apex_m.unwrap_or(0.0);
    assert!(
        (apex - HoverLimits::DEFAULT.hop_apex_m).abs() < 0.5,
        "the hop peaks near the limits' apex: {hop:?}"
    );
    assert_eq!(
        probe(
            &DetHoverHull::DEFAULT,
            &WorldPhysics::EARTH,
            f32::NAN,
            ProbeScript::Accel
        ),
        Default::default()
    );
}
