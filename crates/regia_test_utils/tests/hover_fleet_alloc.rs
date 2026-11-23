// SPDX-License-Identifier: MPL-2.0
//! `HoverFleet::step_all` (the hover host kit, ADR-8508) allocates nothing per tick after the hulls
//! are pushed. Here rather than in `regia_detsim` so the lockstep crate keeps no dev-dependency on
//! the engine: this crate already depends on it. Its own test binary: it installs the shared
//! counting allocator (ADR-3291).

use regia_detsim::hover::host::{FleetInput, FloorSampler, HeightGrid, HoverBody, HoverFleet};
use regia_detsim::hover::{ClimbTuning, HoverFleetTerrain, HoverLimits, RollingField, WorldPhysics};
use regia_detsim::tick::tick_dt;
use regia_test_utils::alloc_counter::{self, CountingAlloc};

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

fn fleet(n: u32) -> HoverFleet {
    let mut fleet = HoverFleet::new(HoverFleetTerrain::hull_types().to_vec());
    for i in 0..n {
        let x = (i % 32) as f32 * 7.0;
        let z = (i / 32) as f32 * 7.0;
        let hull = i % 2;
        let params = fleet.hulls()[hull as usize].params;
        fleet.push(
            HoverBody::at_rest([x, RollingField.floor(x, z), z], i as f32 * 0.3, &params),
            hull,
        );
    }
    fleet
}

fn run<S: FloorSampler>(fleet: &mut HoverFleet, sampler: &S, inputs: &mut Vec<FleetInput>, ticks: u32) {
    let n = fleet.len() as u32;
    for tick in 0..ticks {
        inputs.clear();
        for slot in (0..n).rev() {
            let k = ((slot + tick / 20) % 7) as f32;
            inputs.push(FleetInput::new(slot, [k / 6.0, k / 7.0 - 0.4, k / 3.0 - 1.0]));
        }
        // A duplicate and an unknown slot take the report's other paths too.
        inputs.push(FleetInput::new(0, [1.0, 0.0, 0.0]));
        inputs.push(FleetInput::new(n + 3, [1.0, 0.0, 0.0]));
        let climb = if tick % 200 < 100 {
            &ClimbTuning::AIR_TIME
        } else {
            &ClimbTuning::CLAMPED_1014
        };
        let report = fleet.step_all(
            inputs,
            sampler,
            &WorldPhysics::EARTH,
            &HoverLimits::DEFAULT,
            climb,
            tick_dt(),
        );
        assert_eq!(report.stepped, n);
    }
}

#[test]
fn fleet_steps_allocate_nothing_after_warmup() {
    let grid = HeightGrid::from_fn([-50.0, -50.0], 2.0, 256, 256, |x, z| RollingField.floor(x, z))
        .expect("a valid grid");
    for n in [1_u32, 64, 900] {
        let mut f = fleet(n);
        let mut inputs = Vec::with_capacity(n as usize + 2);
        // Warm-up: the first tick.
        run(&mut f, &RollingField, &mut inputs, 1);
        let ((), counted) = alloc_counter::measure(|| {
            run(&mut f, &RollingField, &mut inputs, 300);
            run(&mut f, &grid, &mut inputs, 300);
        });
        assert_eq!(
            counted.allocs, 0,
            "{n} hulls: 600 ticks of step_all made {} heap requests ({} bytes)",
            counted.allocs, counted.bytes
        );
        assert!(f.bodies().iter().all(|b| b.pos.iter().all(|v| v.is_finite())));
    }
    // The counter sees allocations at all (the check is live).
    let (v, counted) = alloc_counter::measure(|| vec![0_u8; 64]);
    assert_eq!(v.len(), 64);
    assert!(counted.allocs >= 1);
}
