// SPDX-License-Identifier: MPL-2.0
//! Time `HoverFleet::step_all` (the hover host kit, ADR-8508): per hull and per tick, for fleets of
//! 1, 100, 900 and 2000 hulls on the procedural `RollingField` and on a 512 x 512 `HeightGrid` of it.
//! Prints numbers and asserts nothing; the clock is in this example only, never in the step.
//!
//! `cargo run --release -p regia_detsim --example hover_fleet_cost -- [ticks] [sizes...]`
//! (default 600 ticks, sizes 1 100 900 2000). Each size warms 60 ticks first, then times every tick
//! on a monotonic clock and prints the mean ns per hull, the mean, p50, p99 and max tick in ms.

// A host-side tool, never sim code: it reads its arguments and the clock (ADR-8367).
#![expect(
    clippy::disallowed_methods,
    reason = "timing probe: command-line arguments and Instant::now, outside any sim tick (ADR-8367)"
)]

use std::time::Instant;

use regia_detsim::hover::host::{FleetInput, FloorSampler, HeightGrid, HoverBody, HoverFleet};
use regia_detsim::hover::{ClimbTuning, HoverFleetTerrain, HoverLimits, RollingField, WorldPhysics};
use regia_detsim::tick::tick_dt;

fn fleet(n: u32) -> HoverFleet {
    let mut fleet = HoverFleet::new(HoverFleetTerrain::hull_types().to_vec());
    let side = 64;
    for i in 0..n {
        let x = (i % side) as f32 * 6.0 - 190.0;
        let z = (i / side) as f32 * 6.0 - 190.0;
        let hull = u32::from(i.is_multiple_of(3));
        let params = fleet.hulls()[hull as usize].params;
        fleet.push(
            HoverBody::at_rest([x, RollingField.floor(x, z), z], i as f32 * 0.37, &params),
            hull,
        );
    }
    fleet
}

fn fill(inputs: &mut Vec<FleetInput>, n: u32, tick: u32) {
    inputs.clear();
    for slot in 0..n {
        let k = ((slot.wrapping_mul(2_654_435_761) >> 7).wrapping_add(tick / 30) % 9) as f32;
        inputs.push(FleetInput::new(slot, [k / 8.0, k / 9.0 - 0.45, k / 4.0 - 1.0]));
    }
}

fn time<S: FloorSampler>(label: &str, n: u32, ticks: u32, sampler: &S) {
    let mut f = fleet(n);
    let mut inputs = Vec::with_capacity(n as usize);
    let world = WorldPhysics::EARTH;
    let limits = HoverLimits::DEFAULT;
    for tick in 0..60 {
        fill(&mut inputs, n, tick);
        f.step_all(
            &inputs,
            sampler,
            &world,
            &limits,
            &ClimbTuning::AIR_TIME,
            tick_dt(),
        );
    }
    let mut ns = Vec::with_capacity(ticks as usize);
    let mut stepped = 0_u64;
    for tick in 60..60 + ticks {
        fill(&mut inputs, n, tick);
        let start = Instant::now();
        let report = f.step_all(
            &inputs,
            sampler,
            &world,
            &limits,
            &ClimbTuning::AIR_TIME,
            tick_dt(),
        );
        ns.push(start.elapsed().as_nanos() as u64);
        stepped += u64::from(report.stepped);
    }
    let total: u64 = ns.iter().sum();
    ns.sort_unstable();
    let pick = |q: usize| ns[(ns.len() - 1) * q / 100] as f64 / 1e6;
    // Keep the result alive so the work is not optimized out.
    let check: f32 = f.bodies().iter().map(|b| b.pos[0] + b.pos[2]).sum();
    println!(
        "hover_fleet_cost sampler={label} hulls={n} ticks={ticks} stepped={stepped} ns_per_hull={:.1} \
         mean_ms={:.4} p50_ms={:.4} p99_ms={:.4} max_ms={:.4} check={check:.3}",
        total as f64 / stepped.max(1) as f64,
        total as f64 / f64::from(ticks) / 1e6,
        pick(50),
        pick(99),
        ns.last().copied().unwrap_or(0) as f64 / 1e6,
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let ticks: u32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(600).max(1);
    let mut sizes: Vec<u32> = args.filter_map(|a| a.parse().ok()).collect();
    if sizes.is_empty() {
        sizes = vec![1, 100, 900, 2000];
    }
    let grid = match HeightGrid::from_fn([-400.0, -400.0], 1.5625, 512, 512, |x, z| {
        RollingField.floor(x, z)
    }) {
        Ok(grid) => grid,
        Err(e) => {
            eprintln!("hover_fleet_cost: {e}");
            std::process::exit(2);
        }
    };
    println!(
        "hover_fleet_cost target={}-{} (budget line: 900 hulls inside 1.0 ms of a 4.0 ms p99 tick)",
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    for &n in &sizes {
        time("rolling_field", n, ticks, &RollingField);
        time("height_grid_512", n, ticks, &grid);
    }
}
