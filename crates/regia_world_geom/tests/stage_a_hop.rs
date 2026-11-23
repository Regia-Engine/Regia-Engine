// SPDX-License-Identifier: MPL-2.0
//! Plan row W-02 acceptance 1: the Stage A mouth hop bar (spec 121 § 13.1; replaces the plan's
//! `mouth_hop_at_full_speed_le_5cm_per_tick` as a Stage A test, see ADR-8167).
//!
//! On the mesh-only Switchbacks fixture (the layout meshed over a flat canyon floor with the mouths'
//! hole mask and no baked patches: the T3 handover's "without the gitignored map files" row) a body is driven in and out of
//! all four mouths and along every passage, and each tick the surface under it is measured the way
//! the source's host-step gate measures it (requirements from DO27 `ir-core/tests/tunnel_t3.rs` at
//! regia-absorb-2; original code here): the largest jump between 17 samples of the tick's travel,
//! then that interval halved ten times toward its larger half, so a slope reads as zero and a step
//! keeps its height. The floor is [`sample_move_floor`] (the floor the host's ground pass reads: the
//! one query asked at the body's waist).
//!
//! The drive is kinematic (a fixed speed along the route, the body standing on the floor it reads),
//! not the host's hover chain: that half is the source's own gate and stays a DO27-only pin. The
//! speeds bound the source's bodies from above: 50 m/s for the fastest hull class (the handling rows
//! top out at a 40.2 m/s turbo), 20 m/s for the plain throttle and 6 m/s for a walker, at 60 ticks
//! a second.
//!
//! On the regia-absorb-1 query (W-01's base) the same measure reads 0.30-0.32 m at the mouths (the
//! 0.3 m threshold sill and the lips 0.10 m proud of the ground); on the T3 copy at most 0.001 m.

#![allow(clippy::unwrap_used)]

#[path = "support/hop_meter.rs"]
mod hop_meter;

use hop_meter::{drive as drive_meter, mouth_routes as mouth_routes_of, passage_routes as passage_routes_of};
use regia_detsim::float::det_max;
use regia_world_geom::compat::fixtures::switchbacks;
use regia_world_geom::compat::ground::STEP_BOUND_MM;
use regia_world_geom::compat::hover_ground::{NoGroundDelta, sample_move_floor};
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::compat::state::TunnelsState;

/// The bodies driven: (name, metres per second).
const BODIES: [(&str, f32); 3] = [("fastest_hull", 50.0), ("plain_throttle", 20.0), ("walker", 6.0)];

/// The canyon floor, flat at -256 m, as the sim heightfield sees it (no baked patches).
fn canyon() -> TerrainSampler {
    TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).expect("grid"), 1280.0).with_zero_counts(16384)
}

struct World {
    terrain: TerrainSampler,
    tunnels: TunnelsState,
}

impl World {
    fn surface(&self, x: f32, z: f32, y: f32) -> f32 {
        sample_move_floor(Some(&self.terrain), &self.tunnels, &NoGroundDelta, x, z, y)
    }
}

/// Drive `route` at `speed` m/s from a body standing at `start_y` (its origin), metering every tick.
fn drive(w: &World, route: &[[f32; 2]], speed: f32, start_y: f32) -> hop_meter::Meter {
    drive_meter(
        &|x, z, y| w.surface(x, z, y),
        &|p| w.tunnels.underground(p),
        route,
        speed,
        start_y,
    )
}

/// Every mouth: from the plain past the top, down the ramp to its foot in the tube, back up and out.
fn mouth_routes(w: &World) -> Vec<(String, Vec<[f32; 2]>)> {
    mouth_routes_of(&w.tunnels)
}

/// Every passage that is not a mouth ramp, end to end and back, and its first floor height.
fn passage_routes(w: &World) -> Vec<(String, Vec<[f32; 2]>, f32)> {
    passage_routes_of(&w.tunnels)
}

#[test]
fn stage_a_mouth_hop_at_full_speed_le_5cm_per_tick() {
    // The mouths' hole mask on the sampler, the way the host's loader bakes it (the T3 query does not
    // read it; the regia-absorb-1 query did).
    let plain = canyon();
    let layout = switchbacks();
    let holes = layout.sample(Some(&plain)).mouth_holes(&layout.openings);
    assert!(!holes.is_empty(), "the mouths cut holes");
    let terrain = plain.with_holes(holes);
    let tunnels = TunnelsState::for_layout(layout, Some(&terrain));
    let w = World { terrain, tunnels };
    let limit = STEP_BOUND_MM as f32 / 1000.0;
    let mut failures = Vec::new();
    let (mut worst_mouth, mut worst_passage) = (0.0f32, 0.0f32);
    for (body, speed) in BODIES {
        for (name, route) in mouth_routes(&w) {
            let start = w.terrain.height_at_world(route[0][0], route[0][1]);
            let m = drive(&w, &route, speed, start);
            eprintln!(
                "STAGEA_HOP {body} mouth {name}: hop {:.4} m at {} ({} ticks, {} underground)",
                m.hop, m.hop_at, m.ticks, m.underground
            );
            assert!(m.underground > 0, "{body} {name}: never went underground");
            worst_mouth = det_max(worst_mouth, m.hop);
            if m.hop > limit {
                failures.push(format!("{body} mouth {name}: {:.3} m at {}", m.hop, m.hop_at));
            }
        }
        for (name, route, floor) in passage_routes(&w) {
            let m = drive(&w, &route, speed, floor + 0.5);
            eprintln!(
                "STAGEA_HOP {body} passage {name}: hop {:.4} m at {} ({} ticks, {} underground)",
                m.hop, m.hop_at, m.ticks, m.underground
            );
            worst_passage = det_max(worst_passage, m.hop);
            if m.hop > limit {
                failures.push(format!("{body} passage {name}: {:.3} m at {}", m.hop, m.hop_at));
            }
        }
    }
    eprintln!(
        "STAGEA_HOP worst surface hop per tick: mouths {worst_mouth:.4} m, passages {worst_passage:.4} m (limit {limit})"
    );
    assert!(
        failures.is_empty(),
        "{} hops over {limit} m:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
