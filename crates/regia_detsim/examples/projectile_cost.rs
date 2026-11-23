// SPDX-License-Identifier: MPL-2.0
//! Time `ProjectilePool::step_all` with a full pool against ground, walls and bodies (prints,
//! asserts nothing): `cargo run --release -p regia_detsim --example projectile_cost -- 1024 600 [bodies]`.
//! The `LinearScanHits` cost grows with the shapes; a game's spatial index replaces it.

// A host-side tool, never sim code: it reads its arguments and the clock (ADR-8367).
#![expect(
    clippy::disallowed_methods,
    reason = "timing probe: command-line arguments and Instant::now, outside any sim tick (ADR-8367)"
)]

use std::time::Instant;

use regia_detsim::golden::scenarios::SplitMix64;
use regia_detsim::projectile::{
    HitBox, HitPlane, HitSphere, LinearScanHits, ProjectileParams, ProjectilePool, ProjectileState,
};
use regia_detsim::site_physics::WorldPhysics;
use regia_detsim::tick::tick_dt;

fn arg(n: usize, default: u32) -> u32 {
    std::env::args()
        .nth(n)
        .and_then(|a| a.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let capacity = arg(1, 1024).clamp(1, u32::from(u16::MAX)) as u16;
    let ticks = arg(2, 600);
    let bodies = arg(3, 16);
    let mut rng = SplitMix64::new(0x5eed_8509);
    let mut hits = LinearScanHits {
        planes: vec![HitPlane::ground(0.0)],
        boxes: (0..8)
            .map(|i| {
                let x = -80.0 + 20.0 * i as f32;
                HitBox {
                    min: [x, 0.0, -40.0],
                    max: [x + 0.2, 6.0, 40.0],
                }
            })
            .collect(),
        spheres: (0..bodies)
            .map(|i| HitSphere {
                center: [rng.signed() * 60.0, 1.0, rng.signed() * 60.0],
                radius_m: 0.8,
                body: i + 1,
            })
            .collect(),
    };
    let grenade = ProjectileParams {
        radius_m: 0.05,
        restitution: 0.4,
        friction: 0.3,
        max_bounces: 4,
        fuse_s: Some(3.0),
        wind_response: 0.3,
        ..ProjectileParams::DEFAULT
    };
    let mut pool = ProjectilePool::new(capacity);
    let mut impacts = Vec::with_capacity(usize::from(capacity));
    let world = WorldPhysics::EARTH;
    let mut stepped = 0_u64;
    let mut ended = 0_u64;
    let start = Instant::now();
    for _ in 0..ticks {
        while pool.len() < usize::from(capacity) {
            let vel = [rng.signed() * 30.0, 5.0 + rng.unit() * 20.0, rng.signed() * 30.0];
            if pool
                .spawn(grenade, ProjectileState::launch([0.0, 1.5, 0.0], vel, None))
                .is_none()
            {
                break;
            }
        }
        stepped += pool.len() as u64;
        impacts.clear();
        pool.step_all(&world, [4.0, 0.0, -1.0], tick_dt(), &mut hits, &mut impacts);
        ended += impacts.len() as u64;
    }
    let elapsed = start.elapsed();
    let per = elapsed.as_secs_f64() * 1.0e9 / stepped.max(1) as f64;
    println!(
        "capacity {capacity}, {ticks} ticks, {} shapes: {stepped} projectile steps, {ended} impacts, \
         {:.3} ms total, {per:.1} ns per projectile step, {:.3} ms per tick",
        hits.planes.len() + hits.boxes.len() + hits.spheres.len(),
        elapsed.as_secs_f64() * 1.0e3,
        elapsed.as_secs_f64() * 1.0e3 / f64::from(ticks.max(1)),
    );
}
