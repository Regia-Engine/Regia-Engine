// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-core/tests/tunnel_t3.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 409-447 (`a_body_in_the_tube_and_a_body_on_the_canyon_floor_over_it_keep_their_layers`), mark:
// requirements (Stage A: unchanged). Ranges re-checked against regia-absorb-2 e6c84ba6d and unmoved.
// Edits: the two bodies are kinematic drives through tests/support/hop_meter.rs over compat::state
// (move_floor for the tube, the heightfield for the canyon). No product names. Asserts are the source's.

//! Stage A layer pin on the Switchbacks canyon fixture (plan row W-05, spec 121 § 13.1).

#![allow(clippy::unwrap_used)]

#[path = "support/hop_meter.rs"]
mod hop_meter;

use hop_meter::{HOP_LIMIT_M, Meter, drive};
use regia_world_geom::compat::fixtures::switchbacks;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::compat::state::TunnelsState;

/// The tube sample nearest this XZ is the spot under the canyon floor.
const SPOT_XZ: [f32; 2] = [-407.8, 72.4];
/// Walker speed, metres per second. The floor is level here, so the hop stays at zero.
const SPEED: f32 = 6.0;

fn canyon() -> TerrainSampler {
    TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16384)
}

fn world() -> (TerrainSampler, TunnelsState) {
    let plain = canyon();
    let layout = switchbacks();
    let holes = layout.sample(Some(&plain)).mouth_holes(&layout.openings);
    let terrain = plain.with_holes(holes);
    let tunnels = TunnelsState::for_layout(layout, Some(&terrain));
    (terrain, tunnels)
}

fn dist2(a: [f32; 2], b: [f32; 2]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[1] - b[1];
    dx * dx + dz * dz
}

/// Out along the tube window and back, and the tube floor at the spot.
fn tube_route(tunnels: &TunnelsState, terrain: &TerrainSampler) -> (Vec<[f32; 2]>, f32) {
    let tube = tunnels.layout.as_ref().unwrap().sampled.passage("tube").unwrap();
    let mut k = 0usize;
    let mut best = f32::INFINITY;
    for (i, q) in tube.samples.iter().enumerate() {
        let d = dist2([q.p[0], q.p[2]], SPOT_XZ);
        if d < best {
            best = d;
            k = i;
        }
    }
    assert!(
        k >= 12 && k + 12 < tube.samples.len(),
        "window around the spot, k {k}"
    );
    let line: Vec<[f32; 2]> = tube.samples[k - 12..=k + 12]
        .iter()
        .map(|q| [q.p[0], q.p[2]])
        .collect();
    let spot = tube.samples[k].p;
    let tube_floor = tunnels
        .move_floor(Some(terrain), spot[0], spot[2], spot[1] + 0.5)
        .point[1];
    let mut route = line.clone();
    route.extend(line.iter().rev().skip(1));
    (route, tube_floor)
}

fn report(label: &str, m: &Meter) {
    eprintln!(
        "LAYER {label}: reached {}/{} underground {}/{} ticks hop {:.4} at '{}' low {:.2} high {:.2}",
        m.reached, m.len, m.underground, m.ticks, m.hop, m.hop_at, m.low, m.high
    );
}

#[test]
fn stage_a_layers_hold_on_the_canyon_fixture() {
    let (terrain, tunnels) = world();
    let (route, tube_floor) = tube_route(&tunnels, &terrain);
    let lid = tube_floor + 12.0;
    let under = drive(
        &|x, z, y| tunnels.move_floor(Some(&terrain), x, z, y).point[1],
        &|p| tunnels.underground(p),
        &route,
        SPEED,
        tube_floor + 0.5,
    );
    report("tube", &under);
    assert_eq!(under.reached, under.len, "the tube body finishes the window");
    assert_eq!(
        under.underground, under.ticks,
        "the tube body is underground every tick"
    );
    assert!(
        under.high < lid && under.low > tube_floor - 0.5,
        "tube body left its layer: low {:.2} high {:.2} floor {:.2} lid {:.2}",
        under.low,
        under.high,
        tube_floor,
        lid
    );
    assert!(
        under.hop <= HOP_LIMIT_M,
        "tube hop {:.4} m at {}",
        under.hop,
        under.hop_at
    );

    let start = route[0];
    let canyon_y = terrain.height_at_world(start[0], start[1]);
    let over = drive(
        &|x, z, _y| terrain.height_at_world(x, z),
        &|p| tunnels.underground(p),
        &route,
        SPEED,
        canyon_y,
    );
    report("canyon", &over);
    assert_eq!(over.underground, 0, "the canyon body is never in the tube");
    assert!(
        over.low > lid,
        "canyon body dropped through the lid: low {:.2} lid {:.2}",
        over.low,
        lid
    );
    assert!(
        over.hop <= HOP_LIMIT_M,
        "canyon hop {:.4} m at {}",
        over.hop,
        over.hop_at
    );
}
