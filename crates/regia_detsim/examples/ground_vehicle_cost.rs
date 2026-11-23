// SPDX-License-Identifier: MPL-2.0
//! Time the lockstep ground-vehicle steps (ADR-8551): ns per vehicle step for 1, 100 and 1000
//! tracked and wheeled vehicles on the procedural `GroundField` (four floor samples for the attitude
//! plus one for Y per step). Prints numbers and asserts nothing; the clock is in this example only.
//!
//! `cargo run --release -p regia_detsim --example ground_vehicle_cost -- [ticks] [sizes...]`
//! (default 600 ticks, sizes 1 100 1000). Each size warms 60 ticks, then times every tick.

// A host-side timing probe, never sim code: it reads its arguments and the clock (ADR-8367).
#![expect(
    clippy::disallowed_methods,
    reason = "timing probe: command-line arguments and Instant::now, outside any sim tick (ADR-8367)"
)]

use std::time::Instant;

use regia_detsim::ground_vehicle::tracked::{self, TrackedInput, TrackedParams, TrackedState};
use regia_detsim::ground_vehicle::wheeled::{self, WheeledInput, WheeledParams, WheeledState};
use regia_detsim::ground_vehicle::{GroundField, WorldPhysics};
use regia_detsim::hover::host::FloorSampler;
use regia_detsim::tick::tick_dt;

fn spot(i: u32) -> (f32, f32) {
    ((i % 32) as f32 * 7.0 - 110.0, (i / 32) as f32 * 7.0 - 110.0)
}

fn stats(label: &str, n: u32, ns: &mut [u64], check: f32) {
    let total: u64 = ns.iter().sum();
    ns.sort_unstable();
    let pick = |q: usize| ns[(ns.len() - 1) * q / 100] as f64 / 1e6;
    println!(
        "ground_vehicle_cost model={label} vehicles={n} ticks={} ns_per_vehicle={:.1} mean_ms={:.4} p50_ms={:.4} \
         p99_ms={:.4} check={check:.3}",
        ns.len(),
        total as f64 / (ns.len() as f64 * f64::from(n)),
        total as f64 / ns.len() as f64 / 1e6,
        pick(50),
        pick(99),
    );
}

fn time_tracked(n: u32, ticks: u32) {
    let params = TrackedParams::DEFAULT;
    let world = WorldPhysics::EARTH;
    let mut tanks: Vec<TrackedState> = (0..n)
        .map(|i| {
            let (x, z) = spot(i);
            TrackedState::at_rest([x, GroundField.floor(x, z), z], i as f32 * 0.37)
        })
        .collect();
    let mut ns = Vec::with_capacity(ticks as usize);
    for tick in 0..60 + ticks {
        let start = Instant::now();
        for (i, s) in tanks.iter_mut().enumerate() {
            let k = ((i as u32).wrapping_mul(2_654_435_761).wrapping_add(tick / 30) % 9) as f32;
            let input = TrackedInput {
                left_throttle: k / 8.0,
                right_throttle: 1.0 - k / 6.0,
            };
            tracked::step(&params, &world, &GroundField, s, &input, tick_dt());
        }
        if tick >= 60 {
            ns.push(start.elapsed().as_nanos() as u64);
        }
    }
    let check: f32 = tanks.iter().map(|s| s.pos[0] + s.pos[2]).sum();
    stats("tracked", n, &mut ns, check);
}

fn time_wheeled(n: u32, ticks: u32) {
    let params = WheeledParams::DEFAULT;
    let world = WorldPhysics::EARTH;
    let mut cars: Vec<WheeledState> = (0..n)
        .map(|i| {
            let (x, z) = spot(i);
            WheeledState::at_rest([x, GroundField.floor(x, z), z], i as f32 * 0.37)
        })
        .collect();
    let mut ns = Vec::with_capacity(ticks as usize);
    for tick in 0..60 + ticks {
        let start = Instant::now();
        for (i, s) in cars.iter_mut().enumerate() {
            let k = ((i as u32).wrapping_mul(2_654_435_761).wrapping_add(tick / 30) % 9) as f32;
            let input = WheeledInput {
                throttle: k / 8.0,
                brake: 0.0,
                steer: k / 4.0 - 1.0,
                handbrake: false,
            };
            wheeled::step(&params, &world, &GroundField, s, &input, tick_dt());
        }
        if tick >= 60 {
            ns.push(start.elapsed().as_nanos() as u64);
        }
    }
    let check: f32 = cars.iter().map(|s| s.pos[0] + s.pos[2]).sum();
    stats("wheeled", n, &mut ns, check);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let ticks: u32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(600).max(1);
    let mut sizes: Vec<u32> = args.filter_map(|a| a.parse().ok()).collect();
    if sizes.is_empty() {
        sizes = vec![1, 100, 1000];
    }
    println!(
        "ground_vehicle_cost target={}-{}",
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    for &n in &sizes {
        time_tracked(n, ticks);
        time_wheeled(n, ticks);
    }
}
