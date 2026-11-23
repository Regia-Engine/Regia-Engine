// SPDX-License-Identifier: MPL-2.0
//! Tests of the model limits as data, the world value and the `step_with` entry point (ADR-8349).
//! Regia's own tests (no ported code).
//!
//! The mixed stream below drives every path of the step (both flight modes, four worlds, hops,
//! soft-landing presses, jumps, auto-level off, the modern aim, terraces and ledges) for 3600 ticks
//! from a seed. Its hash on the old API was taken on the base before `HoverLimits` existed and is
//! pinned here, so the wrappers are proven bit-neutral on the stream, not only against each other.

use super::*;
use crate::golden::scenarios::SplitMix64;

const DT: f32 = crate::tick::tick_dt();

/// The worlds of the mixed stream: `(gravity m/s2, air density kg/m3, vacuum)`, 600 ticks each.
const MIXED_WORLDS: [(f32, f32, bool); 4] = [
    (9.81, 1.2, false),
    (1.35, 5.3, false),
    (3.71, 0.6, false),
    (0.113, 0.0, true),
];

/// Ticks of the mixed stream.
const MIXED_TICKS: u32 = 3600;
/// Seed of the mixed stream.
const MIXED_SEED: u64 = 0x5eed_0327;

/// The hash of the mixed stream on the API as it stood before ADR-8349 (`step_tuned`,
/// `hop_launch_speed`, `apply_hull_hop`), taken on the base commit 59dac82b7 (`0xca37_32be_ace8_e85c`).
/// Re-pinned for ADR-8876 (`MATH_BITS_VERSION` 1, the canonical correctly rounded `math::atan2`): the same
/// stream on the same API; both tests below still compare the old and the new API against it.
const MIXED_STREAM_HASH: u64 = 0x3c6b_5d3f_ae97_a9d4;

/// One way of stepping the mixed stream: the old loose-argument API or `step_with`.
trait MixedPath {
    /// One tick on `world` with `climb`.
    fn step(
        &self,
        params: &HoverParams,
        climb: &ClimbTuning,
        world: (f32, f32, bool),
        state: &mut HoverState,
        input: &[f32; 3],
        floor: f32,
    );
    /// The hop launch speed of `world` (computed once per world).
    fn launch(&self, world: (f32, f32, bool)) -> f32;
    /// Press the hop key.
    fn hop(&self, state: &mut HoverState, launch: f32) -> bool;
}

/// The loose-argument API (`step_tuned`, `hop_launch_speed`, `apply_hull_hop`).
struct OldPath;

impl MixedPath for OldPath {
    fn step(
        &self,
        params: &HoverParams,
        climb: &ClimbTuning,
        world: (f32, f32, bool),
        state: &mut HoverState,
        input: &[f32; 3],
        floor: f32,
    ) {
        let air = if world.2 { 0.0 } else { world.1 };
        step_tuned(params, state, input, floor, DT, world.0, air, climb);
    }

    fn launch(&self, world: (f32, f32, bool)) -> f32 {
        let air = if world.2 { 0.0 } else { world.1 };
        hop_launch_speed(world.0, air, DT)
    }

    fn hop(&self, state: &mut HoverState, launch: f32) -> bool {
        apply_hull_hop(state, launch)
    }
}

/// Terrace heights of the mixed floor, one per 23 m of x. Even cells are reached by a step (1 to
/// 6 m: the grade and teleport limits see steps on both sides of their defaults), odd cells by a
/// ramp down.
const TERRACES: [f32; 10] = [0.0, -4.5, 0.5, -4.0, -3.0, -7.8, -3.0, -9.0, -4.0, -6.0];

/// Width of the ramp into an odd terrace, metres: the 4.5 and 4.8 m drops over it are 1.5 and
/// 1.6 faces (between the cliff limit and 1.5 times it), the 6 m drop a 2.0 face (past both), the
/// 2 m drop a gentle 0.67.
const RAMP_M: f32 = 3.0;

/// The mixed stream's floor at `(x, z)`: rolling hills plus terraces, reached by a step on even
/// cells and by a ramp on odd ones.
fn mixed_floor(x: f32, z: f32) -> f32 {
    let hills = 3.0 * math::sin(0.05 * x) + 2.0 * math::cos(0.07 * z);
    let n = TERRACES.len() as i32;
    // The cell under x, rounded down (a cast rounds toward zero).
    let cut = (x / 23.0) as i32;
    let cell = if cut as f32 * 23.0 > x { cut - 1 } else { cut };
    let here = TERRACES[cell.rem_euclid(n) as usize];
    let into = x - cell as f32 * 23.0;
    let terrace = if cell.rem_euclid(2) == 1 && (0.0..RAMP_M).contains(&into) {
        let prev = TERRACES[(cell - 1).rem_euclid(n) as usize];
        prev + (here - prev) * (into / RAMP_M)
    } else {
        here
    };
    hills + terrace
}

/// FNV-1a over one word.
fn fnv(h: &mut u64, word: u32) {
    for b in word.to_le_bytes() {
        *h ^= u64::from(b);
        *h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

/// Every word of the state and the pose into `h`.
fn hash_tick(h: &mut u64, s: &HoverState, pose: [f32; 3]) {
    for w in [
        pose[0],
        pose[1],
        pose[2],
        s.velocity[0],
        s.velocity[1],
        s.velocity[2],
        s.vertical_vel,
        s.yaw,
        s.yaw_rate,
        s.altitude,
        s.prev_terrain.unwrap_or(f32::from_bits(0x7fc0_0bad)),
        s.hop_cooldown,
        s.ground_climb,
        s.slope,
        s.roll,
        s.roll_rate,
        s.pitch,
        s.coast_drop_left,
        s.ground_pitch,
        s.ground_roll,
        s.cushion_pitch,
        s.cushion_pitch_rate,
    ] {
        fnv(h, w.to_bits());
    }
    let flags = u32::from(s.airborne)
        | u32::from(s.jump_active) << 1
        | u32::from(s.level_off) << 2
        | u32::from(s.yaw_modern) << 3
        | u32::from(s.thrusting) << 4;
    fnv(h, flags);
}

/// What one mixed run saw: the hash, and counts that show the stream reached each path.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct MixedRun {
    hash: u64,
    hops: u32,
    soft_landings: u32,
    jumps: u32,
    airborne_ticks: u32,
    free_air_ticks: u32,
}

/// The mixed stream on `path` with `params`; `each` sees the state after every tick.
fn run_mixed(path: &dyn MixedPath, params: &HoverParams, mut each: impl FnMut(u32, &HoverState)) -> MixedRun {
    let mut rng = SplitMix64::new(MIXED_SEED);
    // Facing +x, across the terraces.
    let mut state = HoverState {
        altitude: params.ride_height,
        yaw: core::f32::consts::FRAC_PI_2,
        ..HoverState::default()
    };
    let (mut x, mut z) = (0.0_f32, 0.0_f32);
    let mut input = [0.0_f32; 3];
    let mut launch = 0.0_f32;
    let mut run = MixedRun {
        hash: 0xcbf2_9ce4_8422_2325,
        ..MixedRun::default()
    };
    for tick in 0..MIXED_TICKS {
        let world = MIXED_WORLDS[(tick / 600) as usize % MIXED_WORLDS.len()];
        if tick.is_multiple_of(600) {
            launch = path.launch(world);
        }
        let climb = if (tick / 300) % 2 == 0 {
            &ClimbTuning::AIR_TIME
        } else {
            &ClimbTuning::CLAMPED_1014
        };
        if tick.is_multiple_of(30) {
            // Mostly full throttle with a light steer, so the hull reaches its caps and crosses the
            // terraces; otherwise axes a little past full scale (the step clamps them), near the 0.05
            // and 0.1 thresholds too.
            input = [rng.signed() * 1.1, rng.signed() * 1.1, rng.signed() * 1.1];
            if !rng.next_u64().is_multiple_of(3) {
                input[0] = 1.0;
                input[2] *= 0.3;
            }
            if rng.next_u64().is_multiple_of(5) {
                input[1] = 0.0;
            }
            // A creep just over the thrust threshold (and under 1.5 times it), strafe off.
            if tick % 300 == 150 {
                input = [0.12, 0.0, 0.0];
            }
            // Five seconds flat out with full strafe: past the speed cap (the overspeed bleed).
            if (1200..1500).contains(&tick) {
                input = [1.0, 1.0, 0.0];
            }
            state.level_off = rng.next_u64().is_multiple_of(3);
            state.yaw_modern = rng.next_u64().is_multiple_of(2);
            state.pitch = rng.signed() * 0.5;
        }
        if tick % 291 == 50 && path.hop(&mut state, launch) {
            run.hops += 1;
        }
        if state.airborne && state.vertical_vel < -1.0 && tick % 291 == 125 && path.hop(&mut state, launch) {
            run.soft_landings += 1;
        }
        if tick % 523 == 400 && !state.airborne {
            apply_jump(&mut state, params);
            run.jumps += 1;
        }
        // The host's ground pass: the ground's pitch and roll from samples a metre around the hull.
        let (s, c) = (math::sin(state.yaw), math::cos(state.yaw));
        let ahead = mixed_floor(x + s, z + c) - mixed_floor(x - s, z - c);
        let right = mixed_floor(x + c, z - s) - mixed_floor(x - c, z + s);
        state.ground_pitch = math::atan2(ahead, 2.0);
        state.ground_roll = math::atan2(right, 2.0);
        if state.level_off && hull_in_air(params, &state) {
            run.free_air_ticks += 1;
        }
        let floor = mixed_floor(x, z);
        // A boost (the host steps a copy of the hull with raised caps and thrust), then it expires:
        // the hull is far over its cap and the overspeed bleed brings it back.
        let boosted = HoverParams {
            max_forward_speed: params.max_forward_speed * 1.6,
            max_strafe_speed: params.max_strafe_speed * 1.6,
            thrust_accel: params.thrust_accel * 1.6,
            ..*params
        };
        let hull = if (1200..1380).contains(&tick) {
            &boosted
        } else {
            params
        };
        path.step(hull, climb, world, &mut state, &input, floor);
        x += state.velocity[0] * DT;
        z += state.velocity[2] * DT;
        if state.airborne {
            run.airborne_ticks += 1;
        }
        hash_tick(&mut run.hash, &state, [x, floor + state.altitude, z]);
        each(tick, &state);
    }
    run
}

#[test]
fn mixed_stream_on_the_old_api_matches_the_base_hash() {
    let run = run_mixed(&OldPath, &HoverParams::DEFAULT_HULL, |_, _| {});
    eprintln!("mixed stream: {run:?} hash={:#018x}", run.hash);
    assert!(run.hops > 5 && run.soft_landings > 0 && run.jumps >= 3, "{run:?}");
    assert!(run.airborne_ticks > 300 && run.free_air_ticks > 30, "{run:?}");
    assert_eq!(run.hash, MIXED_STREAM_HASH, "{:#018x}", run.hash);
}

/// [`step_with`], [`hop_launch_speed_for`] and [`apply_hull_hop_with`] under `limits`.
struct NewPath(HoverLimits);

/// The mixed stream's world tuple as a [`WorldPhysics`].
fn world_of((gravity_m_s2, air_density_kg_m3, vacuum): (f32, f32, bool)) -> WorldPhysics {
    WorldPhysics {
        gravity_m_s2,
        air_density_kg_m3,
        vacuum,
        low_g: crate::ballistic::LowG::Governor,
    }
}

impl MixedPath for NewPath {
    fn step(
        &self,
        params: &HoverParams,
        climb: &ClimbTuning,
        world: (f32, f32, bool),
        state: &mut HoverState,
        input: &[f32; 3],
        floor: f32,
    ) {
        let report = step_with(params, &self.0, climb, &world_of(world), state, input, floor, DT);
        assert!(!report.recovered, "the mixed stream never poisons the state");
    }

    fn launch(&self, world: (f32, f32, bool)) -> f32 {
        hop_launch_speed_for(&self.0, &world_of(world), DT)
    }

    fn hop(&self, state: &mut HoverState, launch: f32) -> bool {
        apply_hull_hop_with(&self.0, state, launch)
    }
}

/// Every bit of a state as one array, for bit comparisons (`f32 ==` says `-0.0 == 0.0`).
fn state_bits(s: &HoverState) -> Vec<u32> {
    let mut h = Vec::new();
    let mut push = |w: f32| h.push(w.to_bits());
    for w in [
        s.velocity[0],
        s.velocity[1],
        s.velocity[2],
        s.vertical_vel,
        s.yaw,
        s.yaw_rate,
        s.altitude,
        s.prev_terrain.unwrap_or(f32::from_bits(0x7fc0_0bad)),
        s.hop_cooldown,
        s.ground_climb,
        s.slope,
        s.roll,
        s.roll_rate,
        s.pitch,
        s.coast_drop_left,
        s.ground_pitch,
        s.ground_roll,
        s.cushion_pitch,
        s.cushion_pitch_rate,
    ] {
        push(w);
    }
    h.push(
        u32::from(s.airborne)
            | u32::from(s.jump_active) << 1
            | u32::from(s.level_off) << 2
            | u32::from(s.yaw_modern) << 3
            | u32::from(s.thrusting) << 4,
    );
    h
}

#[test]
#[expect(
    deprecated,
    reason = "the test compares the deprecated aliases with the DEFAULT fields"
)]
fn limits_default_equals_the_old_constants() {
    let d = HoverLimits::DEFAULT;
    // The literal values the constants held before ADR-8349, field by field, bit for bit.
    let rows: [(&str, f32, f32); 23] = [
        ("hop_apex_m", d.hop_apex_m, 3.0),
        ("hop_cooldown_s", d.hop_cooldown_s, 0.8),
        ("hop_land_keep", d.hop_land_keep, 0.25),
        ("jump_feel_gravity_min", d.jump_feel_gravity_min, 6.0),
        ("aim_modern_yaw_alpha", d.aim_modern_yaw_alpha, 25.0),
        ("lat_straighten_grip", d.lat_straighten_grip, 25.0),
        ("airborne_steer_authority", d.airborne_steer_authority, 0.4),
        ("airborne_eps_m", d.airborne_eps_m, 0.25),
        ("drag_input_threshold", d.drag_input_threshold, 0.1),
        ("crest_fall_min_m_s2", d.crest_fall_min_m_s2, 6.0),
        ("crest_soft_band_m", d.crest_soft_band_m, 0.5),
        ("slope_sin_max", d.slope_sin_max, 0.574),
        ("hover_lead_smooth_s", d.hover_lead_smooth_s, 0.25),
        ("hover_cliff_slope", d.hover_cliff_slope, 1.25),
        ("grade_smooth_s", d.grade_smooth_s, 0.12),
        ("grade_teleport_m", d.grade_teleport_m, 4.0),
        ("overspeed_bleed", d.overspeed_bleed, 1.2),
        ("air_attitude_m", d.air_attitude_m, 1.0),
        ("air_lift_max_g", d.air_lift_max_g, 0.6),
        ("free_roll_rate_rad_s", d.free_roll_rate_rad_s, 2.5),
        ("free_roll_track", d.free_roll_track, 6.0),
        ("roll_lean_max", d.roll_lean_max, 0.35),
        ("cushion_pitch_max", d.cushion_pitch_max, 0.35),
    ];
    for (name, field, old) in rows {
        assert_eq!(field.to_bits(), old.to_bits(), "{name}: {field} vs the old {old}");
    }
    // Every field is in the table, in declaration order.
    let names: Vec<&str> = HoverLimits::FIELD_RANGES.iter().map(|f| f.name).collect();
    let row_names: Vec<&str> = rows.iter().map(|r| r.0).collect();
    assert_eq!(names, row_names);
    // The deprecated aliases read the DEFAULT fields.
    let aliases: [(f32, f32); 19] = [
        (HOP_APEX_M, d.hop_apex_m),
        (HOP_COOLDOWN_S, d.hop_cooldown_s),
        (HOP_LAND_KEEP, d.hop_land_keep),
        (JUMP_FEEL_GRAVITY_MIN, d.jump_feel_gravity_min),
        (AIM_MODERN_YAW_ALPHA, d.aim_modern_yaw_alpha),
        (tuning::CREST_FALL_MIN_M_S2, d.crest_fall_min_m_s2),
        (tuning::CREST_SOFT_BAND_M, d.crest_soft_band_m),
        (tuning::SLOPE_SIN_MAX, d.slope_sin_max),
        (tuning::HOVER_LEAD_SMOOTH_S, d.hover_lead_smooth_s),
        (tuning::HOVER_CLIFF_SLOPE, d.hover_cliff_slope),
        (tuning::GRADE_SMOOTH_S, d.grade_smooth_s),
        (tuning::GRADE_TELEPORT_M, d.grade_teleport_m),
        (tuning::OVERSPEED_BLEED, d.overspeed_bleed),
        (tuning::AIR_ATTITUDE_M, d.air_attitude_m),
        (tuning::AIR_LIFT_MAX_G, d.air_lift_max_g),
        (tuning::FREE_ROLL_RATE_RAD_S, d.free_roll_rate_rad_s),
        (tuning::FREE_ROLL_TRACK, d.free_roll_track),
        (tuning::ROLL_LEAN_MAX, d.roll_lean_max),
        (tuning::CUSHION_PITCH_MAX, d.cushion_pitch_max),
    ];
    for (alias, field) in aliases {
        assert_eq!(alias.to_bits(), field.to_bits());
    }
    assert_eq!(HoverLimits::default(), d);
    assert_eq!(jump_gravity(1.35).to_bits(), d.jump_gravity(1.35).to_bits());
    assert_eq!(jump_gravity(1.35), 6.0);
    // The hover step's gravity stays 9.8 (the pins), the world default is Earth's 9.81.
    assert_eq!(GRAVITY, 9.8);
    assert_eq!(DEFAULT_WORLD.gravity_m_s2, GRAVITY);
    assert_eq!(WorldPhysics::EARTH.gravity_m_s2, crate::site_physics::EARTH_G);
}

#[test]
fn step_with_default_limits_is_bit_identical_to_step_tuned() {
    let params = HoverParams::DEFAULT_HULL;
    let mut old_states = Vec::with_capacity(MIXED_TICKS as usize);
    let old = run_mixed(&OldPath, &params, |_, s| old_states.push(state_bits(s)));
    let new = run_mixed(&NewPath(HoverLimits::DEFAULT), &params, |tick, s| {
        assert_eq!(
            state_bits(s),
            old_states[tick as usize],
            "tick {tick}: step_with under HoverLimits::DEFAULT left step_tuned's bits"
        );
    });
    assert_eq!(old, new);
    // Both equal the stream's hash on the base, before HoverLimits existed.
    assert_eq!(new.hash, MIXED_STREAM_HASH, "{:#018x}", new.hash);
    // `step` is `step_with` on the default world, bit for bit.
    let mut a = HoverState::default();
    let mut b = HoverState::default();
    for tick in 0..600_u32 {
        let input = [1.0, if tick < 300 { 0.5 } else { -0.5 }, 0.3];
        let floor = math::sin(tick as f32 * 0.05) * 2.0;
        if tick == 200 {
            apply_jump(&mut a, &params);
            apply_jump(&mut b, &params);
        }
        step(&params, &mut a, &input, floor, DT);
        let _ = step_with(
            &params,
            &HoverLimits::DEFAULT,
            &ClimbTuning::AIR_TIME,
            &DEFAULT_WORLD,
            &mut b,
            &input,
            floor,
            DT,
        );
        assert_eq!(state_bits(&a), state_bits(&b), "tick {tick}");
    }
}

#[test]
fn a_game_can_change_the_hop_apex_and_cooldown() {
    let limits = HoverLimits {
        hop_apex_m: 1.5,
        hop_cooldown_s: 0.3,
        ..HoverLimits::DEFAULT
    };
    assert!(limits.validate().is_ok());
    let cooldown_ticks = |s: f32| (s / DT + 0.5) as usize;
    for (name, g, air) in [
        ("earth", 9.81_f32, 1.2_f32),
        ("mars_thin_air", 3.71, 0.6),
        ("light_vacuum", 1.315, 0.0),
    ] {
        let default = super::tests::fly_hop_with(&HoverLimits::DEFAULT, g, air, 0.0);
        let game = super::tests::fly_hop_with(&limits, g, air, 0.0);
        eprintln!("{name}: default {default:?}, game {game:?}");
        assert!(
            (default.apex_m / 3.0 - 1.0).abs() <= 0.02,
            "{name}: default apex {}",
            default.apex_m
        );
        assert!(
            (game.apex_m / 1.5 - 1.0).abs() <= 0.02,
            "{name}: game apex {}",
            game.apex_m
        );
        // The cooldown runs out within a tick of the limit: 0.3 s (18 ticks at 60 Hz), not 0.8 s.
        let want = cooldown_ticks(0.3);
        assert!(
            (want..=want + 1).contains(&game.cooldown_ticks),
            "{name}: game cooldown {} ticks, want {want}",
            game.cooldown_ticks
        );
        let want = cooldown_ticks(0.8);
        assert!(
            (want..=want + 1).contains(&default.cooldown_ticks),
            "{name}: default cooldown {} ticks, want {want}",
            default.cooldown_ticks
        );
        assert!(game.ticks < default.ticks, "{name}: a lower hop lands sooner");
    }
    // The cooldown gates a second press: refused at 0.25 s, accepted from 0.3 s on (a falling hull).
    let mut s = HoverState {
        airborne: true,
        altitude: 30.0,
        vertical_vel: -4.0,
        ..HoverState::default()
    };
    assert!(apply_hull_hop_with(&limits, &mut s, 0.0));
    assert_eq!(s.vertical_vel, -4.0 * limits.hop_land_keep);
    assert_eq!(s.hop_cooldown, 0.3);
    s.vertical_vel = -4.0;
    s.hop_cooldown = 0.05;
    assert!(
        !apply_hull_hop_with(&limits, &mut s, 0.0),
        "the cooldown still runs"
    );
    s.hop_cooldown = 0.0;
    let keep = HoverLimits {
        hop_land_keep: 0.5,
        ..limits
    };
    assert!(apply_hull_hop_with(&keep, &mut s, 0.0));
    assert_eq!(s.vertical_vel, -2.0, "a game's own land keep");
}

#[test]
fn every_limit_reaches_the_step() {
    let params = HoverParams::DEFAULT_HULL;
    let base = run_mixed(&NewPath(HoverLimits::DEFAULT), &params, |_, _| {}).hash;
    let mut dead = Vec::new();
    for field in HoverLimits::FIELD_RANGES {
        let mut limits = HoverLimits::DEFAULT;
        (field.set)(&mut limits, (field.get)(&HoverLimits::DEFAULT) * 1.5);
        assert!(limits.validate().is_ok(), "{}", field.name);
        let moved = run_mixed(&NewPath(limits), &params, |_, _| {}).hash;
        if moved == base {
            dead.push(field.name);
        }
    }
    assert!(
        dead.is_empty(),
        "x1.5 changed nothing on the mixed stream: {dead:?}"
    );
}

#[test]
fn limits_validate_and_sanitize() {
    assert!(HoverLimits::DEFAULT.validate().is_ok());
    let d = HoverLimits::DEFAULT;
    assert_eq!(
        state_bits_of_limits(&d.sanitized()),
        state_bits_of_limits(&d),
        "a valid value sanitizes to itself"
    );
    for field in HoverLimits::FIELD_RANGES {
        for (bad, code) in [
            (f32::NAN, "not_finite"),
            (f32::INFINITY, "not_finite"),
            (-1.0, "negative"),
            (field.max * 2.0 + 1.0, "out_of_range"),
        ] {
            let mut l = d;
            (field.set)(&mut l, bad);
            let err = l.validate().expect_err(field.name);
            assert_eq!(
                (err.field, err.kind.code()),
                (field.name, code),
                "{} = {bad}",
                field.name
            );
            let fixed = l.sanitized();
            assert!(fixed.validate().is_ok(), "{} = {bad} sanitized", field.name);
        }
        if field.rate {
            let mut l = d;
            (field.set)(&mut l, 0.0);
            assert_eq!(
                l.validate().map_err(|e| e.kind.code()),
                Err("zero_rate"),
                "{}",
                field.name
            );
            assert_eq!((field.get)(&l.sanitized()), field.min, "{}", field.name);
        }
    }
}

/// Every field of `l` as bits.
fn state_bits_of_limits(l: &HoverLimits) -> Vec<u32> {
    HoverLimits::FIELD_RANGES
        .iter()
        .map(|f| (f.get)(l).to_bits())
        .collect()
}

#[test]
fn world_physics_bytes_round_trip_raw_bits() {
    use crate::ballistic::LowG;
    use crate::site_physics::WorldPhysicsErrorKind as K;
    let worlds = [
        WorldPhysics::EARTH,
        DEFAULT_WORLD,
        WorldPhysics {
            gravity_m_s2: 1.35,
            air_density_kg_m3: 5.3,
            vacuum: false,
            low_g: LowG::Physical,
        },
        WorldPhysics {
            gravity_m_s2: 0.113,
            air_density_kg_m3: 0.0,
            vacuum: true,
            low_g: LowG::Governor,
        },
        WorldPhysics {
            gravity_m_s2: f32::from_bits(1),
            air_density_kg_m3: 1000.0,
            vacuum: true,
            low_g: LowG::Physical,
        },
    ];
    for w in worlds {
        assert!(w.validate().is_ok(), "{w:?}");
        let bytes = w.to_le_bytes();
        assert_eq!(bytes.len(), WorldPhysics::BYTES);
        let back = WorldPhysics::from_le_bytes(bytes).expect("a valid world decodes");
        assert_eq!(back.gravity_m_s2.to_bits(), w.gravity_m_s2.to_bits());
        assert_eq!(back.air_density_kg_m3.to_bits(), w.air_density_kg_m3.to_bits());
        assert_eq!((back.vacuum, back.low_g), (w.vacuum, w.low_g));
        assert_eq!(back.to_le_bytes(), bytes);
    }
    // The fixed field order: gravity, air, vacuum, low_g, little-endian raw bits.
    let earth = WorldPhysics::EARTH.to_le_bytes();
    assert_eq!(earth[0..4], 9.81_f32.to_bits().to_le_bytes());
    assert_eq!(earth[4..8], 1.2_f32.to_bits().to_le_bytes());
    assert_eq!(earth[8..], [0, 0]);
    // Refused: -0.0 and NaN gravity, -0.0 air, NaN air, a gravity of 0 or less, flags other than 0/1.
    let bad = |w: WorldPhysics| w.validate().map_err(|e| (e.field, e.kind));
    let e = WorldPhysics::EARTH;
    assert_eq!(
        bad(WorldPhysics {
            gravity_m_s2: -0.0,
            ..e
        }),
        Err(("gravity_m_s2", K::NegativeZero))
    );
    assert_eq!(
        bad(WorldPhysics {
            gravity_m_s2: 0.0,
            ..e
        }),
        Err(("gravity_m_s2", K::OutOfRange))
    );
    assert_eq!(
        bad(WorldPhysics {
            gravity_m_s2: -9.81,
            ..e
        }),
        Err(("gravity_m_s2", K::OutOfRange))
    );
    for nan in [f32::NAN, f32::from_bits(0xffc0_1234), f32::INFINITY] {
        assert_eq!(
            bad(WorldPhysics {
                gravity_m_s2: nan,
                ..e
            }),
            Err(("gravity_m_s2", K::NotFinite))
        );
        assert_eq!(
            bad(WorldPhysics {
                air_density_kg_m3: nan,
                ..e
            }),
            Err(("air_density_kg_m3", K::NotFinite))
        );
        // The bytes keep the payload, and the decoder refuses it.
        let bytes = WorldPhysics {
            gravity_m_s2: nan,
            ..e
        }
        .to_le_bytes();
        assert_eq!(bytes[0..4], nan.to_bits().to_le_bytes());
        assert_eq!(
            WorldPhysics::from_le_bytes(bytes).map_err(|e| e.kind),
            Err(K::NotFinite)
        );
    }
    assert_eq!(
        bad(WorldPhysics {
            air_density_kg_m3: -0.0,
            ..e
        }),
        Err(("air_density_kg_m3", K::NegativeZero))
    );
    assert_eq!(
        bad(WorldPhysics {
            air_density_kg_m3: 1000.5,
            ..e
        }),
        Err(("air_density_kg_m3", K::OutOfRange))
    );
    let minus_zero = WorldPhysics {
        gravity_m_s2: -0.0,
        ..e
    }
    .to_le_bytes();
    assert_eq!(minus_zero[0..4], [0, 0, 0, 0x80]);
    let err = WorldPhysics::from_le_bytes(minus_zero).expect_err("-0.0 gravity");
    assert_eq!((err.kind.code(), err.bits), ("negative_zero", 0x8000_0000));
    for (at, field) in [(8, "vacuum"), (9, "low_g")] {
        let mut b = earth;
        b[at] = 2;
        let err = WorldPhysics::from_le_bytes(b).expect_err(field);
        assert_eq!((err.field, err.kind.code(), err.bits), (field, "bad_flag", 2));
    }
    // Codes are lowercase ascii, digits and `_`.
    for k in K::ALL {
        assert!(
            k.code()
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        );
    }
    assert!(format!("{err}").starts_with("negative_zero: world field `gravity_m_s2`"));
}

#[test]
fn two_worlds_that_differ_in_one_bit_have_different_bytes() {
    use crate::ballistic::LowG;
    let base = WorldPhysics::EARTH;
    let bytes = base.to_le_bytes();
    // One bit of each f32 word, every bit position.
    for bit in 0..32 {
        let g = WorldPhysics {
            gravity_m_s2: f32::from_bits(base.gravity_m_s2.to_bits() ^ (1 << bit)),
            ..base
        };
        let a = WorldPhysics {
            air_density_kg_m3: f32::from_bits(base.air_density_kg_m3.to_bits() ^ (1 << bit)),
            ..base
        };
        assert_ne!(g.to_le_bytes(), bytes, "gravity bit {bit}");
        assert_ne!(a.to_le_bytes(), bytes, "air bit {bit}");
        assert_ne!(g.to_le_bytes(), a.to_le_bytes(), "bit {bit}");
    }
    let vac = WorldPhysics { vacuum: true, ..base };
    let phys = WorldPhysics {
        low_g: LowG::Physical,
        ..base
    };
    assert_ne!(vac.to_le_bytes(), bytes);
    assert_ne!(phys.to_le_bytes(), bytes);
    assert_ne!(vac.to_le_bytes(), phys.to_le_bytes());
    // `==` cannot tell +0 from -0; the bytes can (a lobby compares the bytes).
    let zero = WorldPhysics {
        air_density_kg_m3: 0.0,
        ..base
    };
    let minus = WorldPhysics {
        air_density_kg_m3: -0.0,
        ..base
    };
    assert_eq!(zero, minus);
    assert_ne!(zero.to_le_bytes(), minus.to_le_bytes());
}

#[test]
#[expect(
    deprecated,
    reason = "compares the deprecated mortar wrappers on purpose: they must keep their bits (ADR-8509)"
)]
fn vacuum_ignores_the_air_density_and_world_wrappers_match_the_loose_functions() {
    use crate::ballistic::{self, LowG};
    use crate::lowg;
    let params = HoverParams::DEFAULT_HULL;
    let vac_thick = WorldPhysics {
        gravity_m_s2: 1.35,
        air_density_kg_m3: 5.3,
        vacuum: true,
        low_g: LowG::Governor,
    };
    let vac_none = WorldPhysics {
        air_density_kg_m3: 0.0,
        ..vac_thick
    };
    let mut a = HoverState {
        airborne: true,
        altitude: 40.0,
        velocity: [10.0, 5.0, 3.0],
        vertical_vel: 5.0,
        ..HoverState::default()
    };
    let mut b = a;
    for _ in 0..300 {
        let _ = step_with(
            &params,
            &HoverLimits::DEFAULT,
            &ClimbTuning::AIR_TIME,
            &vac_thick,
            &mut a,
            &[0.5; 3],
            0.0,
            DT,
        );
        let _ = step_with(
            &params,
            &HoverLimits::DEFAULT,
            &ClimbTuning::AIR_TIME,
            &vac_none,
            &mut b,
            &[0.5; 3],
            0.0,
            DT,
        );
        assert_eq!(state_bits(&a), state_bits(&b));
    }
    for w in [
        WorldPhysics::EARTH,
        vac_thick,
        WorldPhysics {
            gravity_m_s2: 3.71,
            air_density_kg_m3: 0.6,
            vacuum: false,
            low_g: LowG::Physical,
        },
    ] {
        let v = [3.0, 20.0, -1.0];
        let air = w.drag_air();
        let bits = |x: [f32; 3]| x.map(f32::to_bits);
        assert_eq!(
            bits(ballistic::step_debris_in(v, &w, DT)),
            bits(ballistic::step_debris(v, w.gravity_m_s2, air, DT))
        );
        assert_eq!(
            bits(ballistic::step_per_tick_in(v, &w, DT)),
            bits(ballistic::step_per_tick(v, w.gravity_m_s2, air, DT))
        );
        assert_eq!(
            ballistic::mortar_range_in(&w).to_bits(),
            ballistic::mortar_range_m(w.low_g, w.gravity_m_s2).to_bits()
        );
        let (l0, p0) = ballistic::landing_speed_in(8.0, &w, DT);
        let (l1, p1) = ballistic::landing_speed(8.0, w.gravity_m_s2, air, DT);
        assert_eq!((l0.to_bits(), p0.to_bits()), (l1.to_bits(), p1.to_bits()));
        let pc = lowg::play_check_in(&w);
        let pl = lowg::play_check(w.gravity_m_s2, w.air_density_kg_m3, w.vacuum);
        assert_eq!(
            (pc.fan.to_bits(), pc.thrust.to_bits()),
            (pl.fan.to_bits(), pl.thrust.to_bits())
        );
        // A game's own suit apex goes through the world wrappers.
        for apex in [lowg::EARTH_JUMP_APEX_M, 1.0, 4.0] {
            assert_eq!(
                lowg::jump_velocity_in(&w, apex).to_bits(),
                lowg::jump_velocity(apex, w.gravity_m_s2, w.low_g).to_bits()
            );
            let r = lowg::arc_row_in(&w, apex);
            let o = lowg::arc_row_for(w.low_g, w.gravity_m_s2, apex);
            assert_eq!(
                [r.apex_m, r.hang_s, r.landing_m_s, r.mortar_range_m].map(f32::to_bits),
                [o.apex_m, o.hang_s, o.landing_m_s, o.mortar_range_m].map(f32::to_bits)
            );
        }
        assert_eq!(
            hop_launch_speed_for(&HoverLimits::DEFAULT, &w, DT).to_bits(),
            hop_launch_speed(w.gravity_m_s2, air, DT).to_bits()
        );
    }
}
