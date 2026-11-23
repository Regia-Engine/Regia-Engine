// SPDX-License-Identifier: MPL-2.0
//! Plan row W-31 (tunnels-perf-low-tiers, ADR-8662): the § 4.7 counters that need no clock. The
//! times are `benches/mixed_calls.rs` with its budgets in `docs/perf/budgets.ron`; these tests pin
//! the call mix, its allocations and the sim-geometry bytes on the cooked Switchbacks map.
#![cfg(feature = "stage_b")]
#![allow(clippy::unwrap_used)]
pub mod mixed_calls;
pub mod native_mouths;
use mixed_calls::{Kind, MixedTick, TICK_CALLS};
use regia_world_geom::allocation_counter as alloc_counter;

#[global_allocator]
static GLOBAL: alloc_counter::CountingAlloc = alloc_counter::CountingAlloc::new();

/// Spec 121 § 11 (plan § 4.7, A-7): sim geometry on a map up to 2.56 km (Switchbacks is 1.28 km),
/// bytes: `checks::GROUND_BUDGET_SMALL`, 35 MB since ADR-8396. The plan row still says 34 MB, the
/// figure before that ADR; the measured bytes are printed against both (ADR-8662).
const SIM_GEOMETRY_BAR_BYTES: u64 = regia_world_geom::checks::GROUND_BUDGET_SMALL;
/// The plan's figure before ADR-8396, printed only.
const PLAN_34_MB: u64 = 34_000_000;

/// T3's ULTRA_LOW note: the sim kernel's own data (everything but the ground lattice) under 4 MB.
const KERNEL_BAR_BYTES: u64 = 4_000_000;

/// One tick is spec 121 § 11's mix: 200 `support`, `push_disc` and `clamp_under_ceiling` at 40
/// hulls, 64 `segment` + 64 `support` at the projectile cap, 100 `line_of_sight`: 508 calls.
#[test]
fn mixed_tick_is_the_plan_call_mix() {
    let (_, _, _, sampled) = native_mouths::world("switchbacks");
    let tick = MixedTick::new(&sampled, native_mouths::acceptance_ground_m("switchbacks"), 640.0);
    assert_eq!(TICK_CALLS, 508);
    assert_eq!(tick.calls(), 508);
    let per: Vec<(&str, usize)> = Kind::ALL.iter().map(|k| (k.name(), k.calls())).collect();
    assert_eq!(
        per,
        [
            ("support", 200),
            ("push_disc+clamp_under_ceiling", 80),
            ("segment", 64),
            ("shell_support", 64),
            ("line_of_sight", 100),
        ]
    );
}

/// Spec 121 § 11: the mixed calls of a tick allocate nothing (counting allocator, this thread).
#[test]
fn mixed_tick_allocates_nothing() {
    let (_, geom, _, sampled) = native_mouths::world("switchbacks");
    let state = geom.new_state();
    let tick = MixedTick::new(&sampled, native_mouths::acceptance_ground_m("switchbacks"), 640.0);
    tick.run(&geom, &state);
    let _exclusive = alloc_counter::exclusive();
    let ((), count) = alloc_counter::measure(|| {
        for _ in 0..8 {
            tick.run(&geom, &state);
        }
    });
    assert_eq!(count.allocs, 0, "{count:?}");
}

/// Spec 121 § 11 (plan § 4.7, A-7) and T3's ULTRA_LOW row, by counters: the resident sim geometry
/// of the cooked Switchbacks map (lattice, triangles, query index, height pyramid) is within 35 MB,
/// and everything but the lattice (the sim kernel's data) under 4 MB.
#[test]
fn sim_geometry_bytes_within_budget() {
    let (output, geom, _, _) = native_mouths::world("switchbacks");
    let resident = geom.resident_bytes();
    let [lattice, triangles, runtime, index, pyramid, axis] = geom.resident_parts();
    let kernel = resident - lattice;
    eprintln!(
        "SIM_GEOMETRY switchbacks resident={resident} lattice={lattice} triangles={triangles} runtime={runtime} index={index} pyramid={pyramid} axis={axis} kernel={kernel} cook_budget={} triangle_count={} over_plan_34mb={}",
        output.report.ground.budget_bytes,
        geom.triangles().len(),
        resident.saturating_sub(PLAN_34_MB)
    );
    assert!(lattice > 0 && triangles > 0 && index > 0, "a part is not counted");
    assert!(
        resident <= SIM_GEOMETRY_BAR_BYTES,
        "switchbacks: resident sim geometry {resident} B over spec 121 § 11's {SIM_GEOMETRY_BAR_BYTES} B"
    );
    assert!(
        kernel <= KERNEL_BAR_BYTES,
        "switchbacks: sim kernel data {kernel} B over T3's {KERNEL_BAR_BYTES} B"
    );
}

/// ADR-8662: the segment early out (front-to-back walks that stop past the closest hit plus 5 cm)
/// returns the same hit, bit for bit, as the full walk of every cell (`segment_without_pyramid`), on
/// the mixed tick's shells and sight lines and on 2000 seeded rays across the cooked Switchbacks
/// map: in the tubes, from the tubes to the surface, along and under the ground, short and long.
#[cfg(feature = "stats")]
#[test]
fn early_out_segments_equal_the_full_walk() {
    use regia_world_geom::{FaceMask, SegmentHit};
    let (_, geom, _, sampled) = native_mouths::world("switchbacks");
    let state = geom.new_state();
    let ground = native_mouths::acceptance_ground_m("switchbacks");
    let tick = MixedTick::new(&sampled, ground, 640.0);
    let tube: Vec<[f32; 3]> = sampled
        .passages
        .iter()
        .flat_map(|p| p.samples.iter().map(|s| s.p))
        .collect();
    let mut rays: Vec<([f32; 3], [f32; 3])> = tick.segments.clone();
    rays.extend(tick.los.iter().copied());
    let mut seed = 0x9e37_79b9u32;
    let mut unit = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed >> 8) as f32 / (1u32 << 24) as f32
    };
    for i in 0..2000 {
        let a = tube[(unit() * tube.len() as f32) as usize % tube.len()];
        let from = [a[0], a[1] + 0.2 + unit() * 3.0, a[2]];
        let to = match i % 4 {
            // Along the tube nearby.
            0 => {
                let b = tube[(unit() * tube.len() as f32) as usize % tube.len()];
                [b[0], b[1] + unit() * 3.0, b[2]]
            }
            // Up to the surface, up to 600 m away.
            1 => [
                from[0] + (unit() - 0.5) * 1200.0,
                ground + unit() * 30.0,
                from[2] + (unit() - 0.5) * 1200.0,
            ],
            // Short, any direction.
            2 => [
                from[0] + (unit() - 0.5) * 8.0,
                from[1] + (unit() - 0.5) * 8.0,
                from[2] + (unit() - 0.5) * 8.0,
            ],
            // Grazing the ground outdoors.
            _ => [
                (unit() - 0.5) * 1200.0,
                ground + (unit() - 0.5) * 2.0,
                (unit() - 0.5) * 1200.0,
            ],
        };
        rays.push((from, to));
    }
    let bits = |h: Option<SegmentHit>| h.map(|h| (h.t.to_bits(), h.point.map(f32::to_bits), h.kind, h.tri));
    let mut hits = 0usize;
    for (k, &(from, to)) in rays.iter().enumerate() {
        for mask in [FaceMask::SOLID, FaceMask::GROUND, FaceMask::NO_FLOORS] {
            let fast = geom.segment(&state, from, to, mask);
            let full = geom.segment_without_pyramid(&state, from, to, mask);
            hits += usize::from(full.is_some());
            assert_eq!(bits(fast), bits(full), "ray {k} {from:?} -> {to:?} mask {mask:?}");
        }
    }
    eprintln!(
        "EARLY_OUT rays={} checks={} hits={hits}",
        rays.len(),
        rays.len() * 3
    );
    assert!(hits > rays.len(), "the rays hit too little to test the early out");
}
