// SPDX-License-Identifier: MPL-2.0
//! Acceptance tests of the lockstep projectiles and the aim solver (ADR-8509, lane
//! buildout-mp-det-projectiles): the default projectile is the old debris step bit for bit, bounces,
//! timers and range behave as documented, a fast shot never passes through a thin wall, impacts come
//! out in slot order, the closed-form and drag aim solve their targets, the old mortar functions keep
//! their bits, and the `projectile_mix` golden.

use regia_detsim::aim::{self, AimArc, DragAim};
use regia_detsim::ballistic::{self, ArcLauncher, LowG};
use regia_detsim::golden::{self, DetScenario, GoldenMode, REPORT_PREFIX};
use regia_detsim::projectile::{
    self, HitBox, HitPlane, HitSphere, HitTarget, Impact, ImpactReason, LinearScanHits, NoHits,
    ProjectileMix, ProjectileParams, ProjectileParamsErrorKind, ProjectilePool, ProjectileState,
};
use regia_detsim::site_physics::{EARTH_G, WorldPhysics};
use regia_detsim::tick::{TickRate, tick_dt};
use regia_detsim::{math, tick};

const DT: f32 = tick_dt();

/// The four worlds of the hover limits lane's mixed stream (mp-02): `(gravity, air, vacuum)`.
const MP02_WORLDS: [(f32, f32, bool); 4] = [
    (9.81, 1.2, false),
    (1.35, 5.3, false),
    (3.71, 0.6, false),
    (0.113, 0.0, true),
];

fn world((gravity_m_s2, air_density_kg_m3, vacuum): (f32, f32, bool)) -> WorldPhysics {
    WorldPhysics {
        gravity_m_s2,
        air_density_kg_m3,
        vacuum,
        low_g: LowG::Governor,
    }
}

/// Earth gravity with no air.
const VACUUM: WorldPhysics = WorldPhysics {
    vacuum: true,
    ..WorldPhysics::EARTH
};

fn bits(v: [f32; 3]) -> [u32; 3] {
    v.map(f32::to_bits)
}

/// Step a lone projectile until it ends or `max` ticks pass: the impact and the ticks stepped.
fn fly(
    params: &ProjectileParams,
    world: &WorldPhysics,
    state: &mut ProjectileState,
    dt: f32,
    hits: &mut LinearScanHits,
    max: u32,
) -> Option<(Impact, u32)> {
    for n in 1..=max {
        if let Some(impact) = projectile::step(params, world, [0.0; 3], state, dt, hits) {
            return Some((impact, n));
        }
    }
    None
}

#[test]
fn default_projectile_velocity_equals_step_debris() {
    let params = ProjectileParams::DEFAULT;
    assert_eq!(params, ProjectileParams::default());
    let launches = [
        [30.0, 40.0, -10.0],
        [-250.0, 3.0, 90.0],
        [0.0, 0.0, 0.0],
        [1.0e-7, -2.0, 0.0],
    ];
    let mut ticks = 0_u32;
    for (w, launch) in MP02_WORLDS.into_iter().zip(launches) {
        let world = world(w);
        let mut state = ProjectileState::launch([0.0, 5.0, 0.0], launch, None);
        let mut reference = launch;
        for tick in 0..2500_u32 {
            // A wind the default projectile must ignore (wind_response 0).
            let wind = [tick as f32 * 0.01, -3.0, 7.0];
            let ended = projectile::step(&params, &world, wind, &mut state, DT, &mut NoHits);
            reference = ballistic::step_debris_in(reference, &world, DT);
            assert!(ended.is_none(), "the default projectile has no timer or range");
            assert_eq!(bits(state.vel), bits(reference), "world {w:?} tick {tick}");
            ticks += 1;
        }
        // step_velocity is the same operation.
        assert_eq!(
            bits(projectile::step_velocity(
                &params,
                world.gravity_m_s2,
                world.drag_air(),
                [9.0; 3],
                launch,
                DT
            )),
            bits(ballistic::step_debris(
                launch,
                world.gravity_m_s2,
                world.drag_air(),
                DT
            ))
        );
    }
    assert_eq!(ticks, 10_000);
    // A bad dt leaves the state unchanged, as step_debris leaves the velocity.
    let mut state = ProjectileState::launch([1.0, 2.0, 3.0], [4.0, 5.0, 6.0], None);
    for dt in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        let before = state;
        assert!(
            projectile::step(
                &params,
                &WorldPhysics::EARTH,
                [0.0; 3],
                &mut state,
                dt,
                &mut NoHits
            )
            .is_none()
        );
        assert_eq!(state, before);
    }
}

#[test]
fn a_bounce_keeps_restitution_and_stops_after_max_bounces() {
    let params = ProjectileParams {
        restitution: 0.5,
        friction: 0.25,
        max_bounces: 3,
        ..ProjectileParams::DEFAULT
    };
    let mut ground = LinearScanHits::default();
    ground.planes.push(HitPlane::ground(0.0));
    let mut state = ProjectileState::launch([0.0, 10.0, 0.0], [2.0, 0.0, 0.0], None);
    let mut bounces_seen = 0;
    let mut end = None;
    for _ in 0..2000 {
        let before = state.bounces;
        let pre = projectile::step_velocity(
            &params,
            VACUUM.gravity_m_s2,
            VACUUM.drag_air(),
            [0.0; 3],
            state.vel,
            DT,
        );
        let impact = projectile::step(&params, &VACUUM, [0.0; 3], &mut state, DT, &mut ground);
        if state.bounces > before {
            bounces_seen += 1;
            assert!(pre[1] < 0.0);
            // Normal speed: reflected and scaled by the restitution, bit for bit.
            assert_eq!(state.vel[1].to_bits(), (-(pre[1] * 0.5)).to_bits());
            // Tangential speed: friction takes a quarter.
            assert_eq!(state.vel[0].to_bits(), (pre[0] * 0.75).to_bits());
            assert!(
                state.pos[1].abs() < 1.0e-4,
                "it stops at the contact: {}",
                state.pos[1]
            );
        }
        if let Some(impact) = impact {
            end = Some(impact);
            break;
        }
    }
    let end = end.expect("the fourth contact ends it");
    assert_eq!(bounces_seen, 3);
    assert_eq!(state.bounces, 3);
    assert!(!state.alive);
    assert_eq!(end.reason, ImpactReason::Hit);
    assert_eq!(end.target, Some(HitTarget::World));
    assert_eq!(end.normal, [0.0, 1.0, 0.0]);
    assert!(end.point[1].abs() < 1.0e-4);

    // With a fuse, the projectile rests after its last bounce and the fuse ends it there.
    let fused = ProjectileParams {
        fuse_s: Some(8.0),
        ..params
    };
    let mut state = ProjectileState::launch([0.0, 10.0, 0.0], [2.0, 0.0, 0.0], None);
    let (impact, n) = fly(&fused, &VACUUM, &mut state, DT, &mut ground, 2000).expect("the fuse");
    assert_eq!(impact.reason, ImpactReason::Fuse);
    assert_eq!(n, 480);
    assert_eq!(state.bounces, 3);
    assert!(state.resting);
    assert!(impact.point[1].abs() < 1.0e-4);
    assert_eq!(impact.speed, 0.0);

    // A bounce at or under the stop speed ends an unfused projectile.
    let soft = ProjectileParams {
        stop_speed_m_s: 1.0e3,
        ..params
    };
    let mut state = ProjectileState::launch([0.0, 10.0, 0.0], [2.0, 0.0, 0.0], None);
    let (impact, _) = fly(&soft, &VACUUM, &mut state, DT, &mut ground, 2000).expect("first contact");
    assert_eq!((impact.reason, state.bounces), (ImpactReason::Hit, 1));
}

#[test]
fn fuse_and_lifetime_expire_on_their_tick() {
    assert_eq!(projectile::timer_ticks(0.5, DT), 30);
    assert_eq!(projectile::timer_ticks(0.1, DT), 6);
    assert_eq!(projectile::timer_ticks(1.0 / 3.0, DT), 20);
    assert_eq!(projectile::timer_ticks(0.001, DT), 1);
    assert_eq!(projectile::timer_ticks(0.51, DT), 31);
    let dt120 = TickRate::new(120).expect("120 Hz").dt();
    assert_eq!(projectile::timer_ticks(0.5, dt120), 60);

    let mut none = LinearScanHits::default();
    let cases = [
        (Some(0.5), 0.0, DT, ImpactReason::Fuse, 30),
        (None, 0.1, DT, ImpactReason::Expired, 6),
        (Some(0.25), 0.25, DT, ImpactReason::Fuse, 15),
        (Some(0.75), 0.5, DT, ImpactReason::Expired, 30),
        (Some(0.5), 0.0, dt120, ImpactReason::Fuse, 60),
    ];
    for (fuse_s, lifetime_s, dt, reason, tick) in cases {
        let params = ProjectileParams {
            fuse_s,
            lifetime_s,
            ..ProjectileParams::DEFAULT
        };
        let mut state = ProjectileState::launch([0.0, 100.0, 0.0], [5.0, 20.0, 0.0], None);
        let (impact, n) =
            fly(&params, &WorldPhysics::EARTH, &mut state, dt, &mut none, 10_000).expect("a timer");
        assert_eq!(
            (impact.reason, n, impact.age_ticks),
            (reason, tick, tick),
            "{fuse_s:?} {lifetime_s}"
        );
        assert_eq!(bits(impact.point), bits(state.pos));
        assert_eq!(impact.target, None);
        // Ended: further steps do nothing.
        let after = state;
        assert!(
            projectile::step(&params, &WorldPhysics::EARTH, [0.0; 3], &mut state, dt, &mut none).is_none()
        );
        assert_eq!(state, after);
    }
}

#[test]
fn range_limit_uses_travelled_distance() {
    // Straight flight: ends at exactly 25.05 m of path, on the tick that crosses it (151).
    let params = ProjectileParams {
        gravity_scale: 0.0,
        max_range_m: 25.05,
        ..ProjectileParams::DEFAULT
    };
    let mut none = LinearScanHits::default();
    let mut state = ProjectileState::launch([0.0; 3], [10.0, 0.0, 0.0], None);
    let (impact, n) = fly(&params, &VACUUM, &mut state, DT, &mut none, 1000).expect("range");
    assert_eq!(impact.reason, ImpactReason::Range);
    assert_eq!(n, 151);
    assert!((impact.point[0] - 25.05).abs() < 1.0e-3, "{:?}", impact.point);
    assert_eq!(state.travelled_m, 25.05);

    // Bounced back off a wall 5.05 m away: 8 m of path, but only 2.1 m from the start.
    let bouncy = ProjectileParams {
        restitution: 1.0,
        max_bounces: 5,
        max_range_m: 8.0,
        ..params
    };
    let mut wall = LinearScanHits::default();
    wall.boxes.push(HitBox {
        min: [5.05, -10.0, -10.0],
        max: [6.0, 10.0, 10.0],
    });
    let mut state = ProjectileState::launch([0.0; 3], [10.0, 0.0, 0.0], None);
    let (impact, _) = fly(&bouncy, &VACUUM, &mut state, DT, &mut wall, 1000).expect("range");
    assert_eq!(impact.reason, ImpactReason::Range);
    assert_eq!(state.bounces, 1);
    assert!((impact.point[0] - 2.1).abs() < 1.0e-3, "{:?}", impact.point);
    assert!((state.travelled_m - 8.0).abs() < 1.0e-5);
}

#[test]
fn swept_hit_never_tunnels() {
    let fast = ProjectileParams {
        gravity_scale: 0.0,
        ..ProjectileParams::DEFAULT
    };
    for radius_m in [0.0, 0.1] {
        let params = ProjectileParams { radius_m, ..fast };
        let mut wall = LinearScanHits::default();
        wall.boxes.push(HitBox {
            min: [100.0, -50.0, -50.0],
            max: [100.05, 50.0, 50.0],
        });
        // 2000 m/s at 60 Hz is 33 m per tick: point samples would skip a 5 cm wall almost always.
        for k in 0..200 {
            let x0 = -(k as f32) * 0.37;
            let mut state = ProjectileState::launch([x0, 1.0, 0.0], [2000.0, 0.0, 0.0], None);
            let (impact, _) =
                fly(&params, &WorldPhysics::EARTH, &mut state, DT, &mut wall, 100).expect("hits");
            assert_eq!(impact.reason, ImpactReason::Hit, "x0 {x0}");
            assert_eq!(impact.target, Some(HitTarget::World));
            assert_eq!(impact.normal, [-1.0, 0.0, 0.0]);
            assert!(
                (impact.point[0] - (100.0 - radius_m)).abs() < 1.0e-3,
                "x0 {x0}: {:?}",
                impact.point
            );
        }
        // A thin body is not skipped either, and the shooter's own body is.
        let mut body = LinearScanHits::default();
        body.spheres.push(HitSphere {
            center: [100.0, 1.0, 0.0],
            radius_m: 0.02,
            body: 7,
        });
        let mut state = ProjectileState::launch([0.0, 1.0, 0.0], [2000.0, 0.0, 0.0], None);
        let (impact, _) = fly(&params, &WorldPhysics::EARTH, &mut state, DT, &mut body, 100).expect("hits");
        assert_eq!(impact.target, Some(HitTarget::Body(7)));
        let mut state = ProjectileState::launch([0.0, 1.0, 0.0], [2000.0, 0.0, 0.0], Some(7));
        assert!(fly(&params, &WorldPhysics::EARTH, &mut state, DT, &mut body, 100).is_none());
    }
}

/// Ten shots that end on a handful of shared ticks (lifetimes, the ground), spawned into fixed slots.
fn slot_order_run(order: &[u16]) -> Vec<Vec<Impact>> {
    let mut pool = ProjectilePool::new(16);
    let mut hits = LinearScanHits::default();
    hits.planes.push(HitPlane::ground(0.0));
    for &slot in order {
        let k = f32::from(slot);
        let params = ProjectileParams {
            lifetime_s: if slot.is_multiple_of(3) { 0.5 } else { 0.0 },
            ..ProjectileParams::DEFAULT
        };
        let state = ProjectileState::launch([k, 10.0 + f32::from(slot % 2), 0.0], [1.0, 0.0, k], None);
        assert!(pool.spawn_in(slot, params, state));
    }
    let mut ticks = Vec::new();
    let mut impacts = Vec::with_capacity(16);
    while !pool.is_empty() {
        impacts.clear();
        pool.step_all(&WorldPhysics::EARTH, [0.0; 3], DT, &mut hits, &mut impacts);
        if !impacts.is_empty() {
            ticks.push(impacts.clone());
        }
        assert!(ticks.len() < 1000);
    }
    ticks
}

#[test]
fn impacts_come_out_in_slot_order() {
    let a = slot_order_run(&[3, 1, 7, 2, 9, 4, 10, 5, 8, 6]);
    let b = slot_order_run(&[10, 9, 8, 7, 6, 5, 4, 3, 2, 1]);
    let c = slot_order_run(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    assert_eq!(a, b);
    assert_eq!(a, c);
    assert!(
        a.iter().any(|tick| tick.len() >= 3),
        "several impacts share a tick: {a:?}"
    );
    assert_eq!(a.iter().map(Vec::len).sum::<usize>(), 10);
    for tick in &a {
        for (i, impact) in tick.iter().enumerate() {
            assert_eq!(usize::from(impact.order), i);
            if i > 0 {
                assert!(tick[i - 1].slot < impact.slot);
            }
        }
    }
    // Spawn takes the lowest free slot, and a freed slot is reused oldest first.
    let mut pool = ProjectilePool::new(4);
    let s = ProjectileState::launch([0.0; 3], [0.0; 3], None);
    let p = ProjectileParams::DEFAULT;
    assert_eq!(
        [pool.spawn(p, s), pool.spawn(p, s), pool.spawn(p, s)],
        [Some(1), Some(2), Some(3)]
    );
    assert!(pool.despawn(2) && pool.despawn(1));
    assert_eq!(
        [pool.spawn(p, s), pool.spawn(p, s), pool.spawn(p, s)],
        [Some(4), Some(2), Some(1)]
    );
    assert_eq!(pool.spawn(p, s), None, "full");
    assert!(!pool.spawn_in(3, p, s), "live");
}

/// The height at horizontal distance `x` of a drag-free shot, by an exact constant-gravity
/// integration in f64 (each step adds `v dt - g dt^2 / 2` to the height), up to the crossing time.
fn simulated_height(vel: [f32; 3], x: f64, g: f64) -> f64 {
    let flat = math::sqrt_f64(f64::from(vel[0]) * f64::from(vel[0]) + f64::from(vel[2]) * f64::from(vel[2]));
    let t_cross = x / flat;
    let dt = 1.0 / 60.0;
    let steps = (t_cross / dt) as u64;
    let (mut y, mut vy) = (0.0_f64, f64::from(vel[1]));
    for _ in 0..steps {
        y += vy * dt - 0.5 * g * dt * dt;
        vy -= g * dt;
    }
    let rest = t_cross - steps as f64 * dt;
    y + vy * rest - 0.5 * g * rest * rest
}

#[test]
fn launch_angles_hit_the_target_without_drag() {
    let targets: [([f32; 3], f32, f32); 6] = [
        ([100.0, 10.0, 0.0], 45.0, 9.81),
        ([30.0, -5.0, 40.0], 35.0, 9.81),
        ([-120.0, 0.0, 90.0], 30.0, 3.71),
        ([100.0, 2.0, -30.0], 15.0, 1.35),
        ([500.0, 50.0, 0.0], 90.0, 9.81),
        ([0.0, 30.0, 200.0], 25.0, 1.62),
    ];
    for (delta, speed, g) in targets {
        let (low, high) = aim::launch_angles(delta, speed, g).expect("reachable");
        assert!(low <= high);
        let x = f64::from(math::sqrt(delta[0] * delta[0] + delta[2] * delta[2]));
        for pitch in [low, high] {
            let vel = aim::launch_velocity(delta, speed, pitch);
            let y = simulated_height(vel, x, f64::from(g));
            assert!(
                (y - f64::from(delta[1])).abs() < 1.0e-3,
                "{delta:?} {speed} {g} pitch {pitch}: y {y}"
            );
            let t = aim::time_of_flight(delta, speed, pitch, g).expect("time");
            let flat = f64::from(math::sqrt(vel[0] * vel[0] + vel[2] * vel[2]));
            assert!((f64::from(t) * flat - x).abs() < 1.0e-3 * regia_detsim::float::det_max_f64(x, 1.0));
            // The heading points at the target.
            let yaw = aim::yaw_to(delta);
            assert!((math::atan2(vel[2], vel[0]) - yaw).abs() < 1.0e-5);
        }
    }
    // Out of reach, bad inputs, straight up.
    assert_eq!(aim::launch_angles([1000.0, 0.0, 0.0], 60.0, 9.81), None);
    assert_eq!(aim::launch_angles([10.0, 0.0, 0.0], 0.0, 9.81), None);
    assert_eq!(aim::launch_angles([10.0, f32::NAN, 0.0], 10.0, 9.81), None);
    assert_eq!(aim::launch_angles([10.0, 0.0, 0.0], 10.0, 0.0), None);
    let up = aim::launch_angles([0.0, 10.0, 0.0], 20.0, 9.81).expect("reaches 10 m");
    assert_eq!(up.0, up.1);
    assert_eq!(aim::launch_angles([0.0, 30.0, 0.0], 20.0, 9.81), None);
}

fn solution_bits(s: &aim::AimSolution) -> [u32; 9] {
    [
        s.pitch.to_bits(),
        s.yaw.to_bits(),
        s.velocity[0].to_bits(),
        s.velocity[1].to_bits(),
        s.velocity[2].to_bits(),
        s.time_s.to_bits(),
        s.miss_m.to_bits(),
        s.probes,
        0,
    ]
}

#[test]
fn drag_solver_is_deterministic_and_bounded() {
    let params = ProjectileParams {
        drag_per_m: 0.004,
        ..ProjectileParams::DEFAULT
    };
    let earth = WorldPhysics::EARTH;
    let delta = [150.0, 5.0, 40.0];
    for arc in [AimArc::Low, AimArc::High] {
        let opts = DragAim {
            arc,
            ..DragAim::DEFAULT
        };
        let a = aim::solve_with_drag(&params, &earth, delta, 80.0, 8, &opts).expect("solves");
        let b = aim::solve_with_drag(&params, &earth, delta, 80.0, 8, &opts).expect("solves");
        assert_eq!(solution_bits(&a), solution_bits(&b), "same inputs, same bits");
        assert!(a.probes <= 10, "{a:?}");
        assert!(a.miss_m.abs() <= opts.tolerance_m);
        // Fly it: the shot passes within the tolerance of the target.
        let mut state = ProjectileState::launch([0.0; 3], a.velocity, None);
        let x = math::sqrt(delta[0] * delta[0] + delta[2] * delta[2]);
        let (hx, hz) = (delta[0] / x, delta[2] / x);
        let mut prev = state.pos;
        let mut miss = None;
        for _ in 0..3600 {
            projectile::step_free(&params, &earth, &mut state, DT);
            let along = state.pos[0] * hx + state.pos[2] * hz;
            if along >= x {
                let before = prev[0] * hx + prev[2] * hz;
                let f = (x - before) / (along - before);
                miss = Some(prev[1] + (state.pos[1] - prev[1]) * f - delta[1]);
                break;
            }
            prev = state.pos;
        }
        let miss = miss.expect("reaches the target's distance");
        assert!(miss.abs() <= opts.tolerance_m, "{arc:?} flown miss {miss}");
        let drag_free = aim::launch_angles(delta, 80.0, EARTH_G).expect("reachable");
        match arc {
            AimArc::Low => assert!(a.pitch > drag_free.0, "drag needs more pitch on the low arc"),
            AimArc::High => assert!(a.pitch < drag_free.1, "and less on the high arc"),
        }
    }
    // The iteration count bounds the probes.
    for iterations in [0_u8, 1, 3] {
        let opts = DragAim {
            tolerance_m: 1.0e9,
            ..DragAim::DEFAULT
        };
        let s =
            aim::solve_with_drag(&params, &earth, delta, 80.0, iterations, &opts).expect("any miss counts");
        assert!(
            s.probes <= u32::from(iterations) + 2,
            "{iterations}: {}",
            s.probes
        );
    }
    // Out of reach without drag, and out of reach only with drag: None.
    assert!(aim::solve_with_drag(&params, &earth, [5000.0, 0.0, 0.0], 80.0, 8, &DragAim::DEFAULT).is_none());
    assert!(aim::launch_angles([600.0, 0.0, 0.0], 80.0, EARTH_G).is_some());
    assert!(aim::solve_with_drag(&params, &earth, [600.0, 0.0, 0.0], 80.0, 8, &DragAim::DEFAULT).is_none());
    // Bad inputs.
    assert!(aim::solve_with_drag(&params, &earth, [0.0, 5.0, 0.0], 80.0, 8, &DragAim::DEFAULT).is_none());
    let no_ticks = DragAim {
        max_ticks: 0,
        ..DragAim::DEFAULT
    };
    assert!(aim::solve_with_drag(&params, &earth, delta, 80.0, 8, &no_ticks).is_none());
}

#[test]
#[expect(
    deprecated,
    reason = "the deprecated mortar wrappers are what this test pins (ADR-8509)"
)]
fn legacy_mortar_range_is_unchanged() {
    let legacy = ArcLauncher::LEGACY_MORTAR;
    assert_eq!(ballistic::EARTH_MORTAR_G, 32.0);
    assert_eq!(ballistic::MORTAR_MUZZLE_M_S, 120.0);
    assert_eq!((legacy.loft_share, legacy.forward_share), (0.4, 0.9));
    let gravities = [
        9.81_f32,
        1.315,
        1.428,
        3.71,
        1.35,
        0.223,
        0.113,
        24.8,
        0.0,
        -1.0,
        f32::NAN,
    ];
    for g in gravities {
        for mode in [LowG::Governor, LowG::Physical] {
            // The old body of mortar_range_m, written out.
            let v = 120.0 * ballistic::charge_scale(mode, g);
            let gravity = 32.0 * (ballistic::map_g(g) / EARTH_G);
            let old = (v * 0.9) * (2.0 * (v * 0.4) / gravity);
            assert_eq!(legacy.range_m(mode, g).to_bits(), old.to_bits(), "{g} {mode:?}");
            assert_eq!(ballistic::mortar_range_m(mode, g).to_bits(), old.to_bits());
            assert_eq!(
                legacy.charge_scale(mode, g).to_bits(),
                ballistic::charge_scale(mode, g).to_bits()
            );
            let w = WorldPhysics {
                gravity_m_s2: g,
                low_g: mode,
                ..WorldPhysics::EARTH
            };
            assert_eq!(
                ballistic::mortar_range_in(&w).to_bits(),
                legacy.range_in(&w).to_bits()
            );
        }
        assert_eq!(legacy.gravity(g).to_bits(), gravity_bits(g));
        assert_eq!(ballistic::mortar_gravity(g).to_bits(), gravity_bits(g));
    }
    // Another launcher is just other numbers.
    let light = ArcLauncher {
        muzzle_m_s: 60.0,
        ..legacy
    };
    assert!(light.range_m(LowG::Governor, EARTH_G) < legacy.range_m(LowG::Governor, EARTH_G));
}

fn gravity_bits(g: f32) -> u32 {
    (32.0 * (ballistic::map_g(g) / EARTH_G)).to_bits()
}

#[test]
fn validate_names_the_bad_field() {
    assert_eq!(ProjectileParams::DEFAULT.validate(), Ok(()));
    let cases = [
        (
            ProjectileParams {
                drag_per_m: f32::NAN,
                ..ProjectileParams::DEFAULT
            },
            "drag_per_m",
            ProjectileParamsErrorKind::NotFinite,
        ),
        (
            ProjectileParams {
                radius_m: -0.1,
                ..ProjectileParams::DEFAULT
            },
            "radius_m",
            ProjectileParamsErrorKind::Negative,
        ),
        (
            ProjectileParams {
                wind_response: 1.5,
                ..ProjectileParams::DEFAULT
            },
            "wind_response",
            ProjectileParamsErrorKind::OutOfRange,
        ),
        (
            ProjectileParams {
                fuse_s: Some(0.0),
                ..ProjectileParams::DEFAULT
            },
            "fuse_s",
            ProjectileParamsErrorKind::OutOfRange,
        ),
        (
            ProjectileParams {
                restitution: 2.0,
                friction: -1.0,
                ..ProjectileParams::DEFAULT
            },
            "restitution",
            ProjectileParamsErrorKind::OutOfRange,
        ),
    ];
    for (params, field, kind) in cases {
        let error = params.validate().expect_err("invalid");
        assert_eq!((error.field, error.kind), (field, kind), "{error}");
        assert!(error.to_string().starts_with(kind.code()));
    }
    for kind in ProjectileParamsErrorKind::ALL {
        assert!(kind.code().bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
    }
}

#[test]
fn projectile_mix_matches_its_committed_golden() {
    let mut scenario = ProjectileMix::new();
    let report = golden::check_golden(
        &mut scenario,
        &golden::engine_golden_dir(),
        GoldenMode::from_env(),
    );
    println!("{REPORT_PREFIX}{}", report.to_json());
    assert!(report.passed(), "{report}");
    // The run exercises every ending: hits on the world and on bodies, fuses, lifetimes and ranges.
    let mut s = ProjectileMix::new();
    let run = DetScenario::default_run(&s);
    assert_eq!(run.ticks, 2400);
    assert_eq!(tick::TICK_RATE_HZ, run.rate.hz());
    let mut seen = std::collections::BTreeSet::new();
    for t in 0..run.ticks {
        s.step(t);
        for impact in s.impacts() {
            seen.insert((
                impact.reason,
                impact.target.map(|t| matches!(t, HitTarget::Body(_))),
            ));
        }
    }
    for want in [
        (ImpactReason::Hit, Some(false)),
        (ImpactReason::Hit, Some(true)),
        (ImpactReason::Fuse, None),
        (ImpactReason::Expired, None),
        (ImpactReason::Range, None),
    ] {
        assert!(seen.contains(&want), "{want:?} missing from {seen:?}");
    }
}
