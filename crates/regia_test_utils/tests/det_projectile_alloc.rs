// SPDX-License-Identifier: MPL-2.0
//! The lockstep projectile pool allocates nothing on the heap after construction (ADR-8509): spawns,
//! steps with hits, bounces and ends, slot reuse and impact lists, counted over fixed ticks on this
//! thread with no wall clock.
//!
//! Its own test binary: it installs the shared counting global allocator (ADR-5718). It lives in
//! `regia_test_utils` because `regia_detsim` has no dev-dependency on it (the lockstep crate stays
//! light to build).

use regia_detsim::golden::scenarios::SplitMix64;
use regia_detsim::projectile::{
    HitBox, HitPlane, HitSphere, Impact, LinearScanHits, ProjectileParams, ProjectilePool, ProjectileState,
};
use regia_detsim::site_physics::WorldPhysics;
use regia_detsim::tick::tick_dt;
use regia_test_utils::alloc_counter::{self, CountingAlloc};

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

const CAPACITY: u16 = 64;

/// One tick: up to two spawns (dropped when the pool is full), one despawn, a step of every shot.
fn tick(
    t: u32,
    pool: &mut ProjectilePool,
    hits: &mut LinearScanHits,
    impacts: &mut Vec<Impact>,
    rng: &mut SplitMix64,
) -> usize {
    let kinds = [
        ProjectileParams {
            lifetime_s: 0.5,
            ..ProjectileParams::DEFAULT
        },
        ProjectileParams {
            radius_m: 0.1,
            restitution: 0.6,
            max_bounces: 3,
            fuse_s: Some(1.5),
            wind_response: 0.5,
            ..ProjectileParams::DEFAULT
        },
        ProjectileParams {
            max_range_m: 30.0,
            ..ProjectileParams::DEFAULT
        },
    ];
    for _ in 0..2 {
        let kind = kinds[(rng.next_u64() % 3) as usize];
        let vel = [rng.signed() * 40.0, rng.unit() * 20.0, rng.signed() * 40.0];
        let _ = pool.spawn(kind, ProjectileState::launch([0.0, 2.0, 0.0], vel, Some(1)));
    }
    if t.is_multiple_of(5) {
        let _ = pool.despawn((rng.next_u64() % u64::from(CAPACITY)) as u16 + 1);
    }
    impacts.clear();
    let world = if t.is_multiple_of(2) {
        WorldPhysics::EARTH
    } else {
        WorldPhysics {
            gravity_m_s2: 1.62,
            ..WorldPhysics::EARTH
        }
    };
    pool.step_all(&world, [3.0, 0.0, -2.0], tick_dt(), hits, impacts);
    impacts.len()
}

#[test]
fn pool_allocates_nothing_after_warmup() {
    let mut pool = ProjectilePool::new(CAPACITY);
    let mut hits = LinearScanHits {
        planes: vec![HitPlane::ground(0.0)],
        boxes: vec![HitBox {
            min: [10.0, 0.0, -10.0],
            max: [10.05, 5.0, 10.0],
        }],
        spheres: vec![HitSphere {
            center: [-5.0, 1.0, 0.0],
            radius_m: 1.0,
            body: 2,
        }],
    };
    // At most one impact per slot per tick: room for the capacity once.
    let mut impacts = Vec::with_capacity(usize::from(CAPACITY));
    let mut rng = SplitMix64::new(0x5eed_8509);
    // Warm-up: fill the pool past its capacity so every slot has been issued and freed.
    let mut ended = 0;
    for t in 0..600 {
        ended += tick(t, &mut pool, &mut hits, &mut impacts, &mut rng);
    }
    assert!(ended > 100, "the warm-up ended shots: {ended}");
    let (ended, counted) = alloc_counter::measure(|| {
        let mut ended = 0;
        for t in 600..3600 {
            ended += tick(t, &mut pool, &mut hits, &mut impacts, &mut rng);
        }
        ended
    });
    assert!(
        ended > 1000,
        "the measured ticks spawned, bounced and ended shots: {ended}"
    );
    assert_eq!(counted.allocs, 0, "{counted:?}");

    // A new pool allocates only in `new`: its first fill costs nothing either.
    let mut fresh = None;
    let ((), made) = alloc_counter::measure(|| fresh = Some(ProjectilePool::new(CAPACITY)));
    assert!(made.allocs > 0);
    let mut fresh = fresh.expect("built");
    let (_, first) = alloc_counter::measure(|| {
        for t in 0..300 {
            tick(t, &mut fresh, &mut hits, &mut impacts, &mut rng);
        }
    });
    assert_eq!(first.allocs, 0, "{first:?}");
}
