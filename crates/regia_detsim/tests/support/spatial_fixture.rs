// SPDX-License-Identifier: MPL-2.0
//! Shared deterministic spatial fixtures (test-only).
use regia_detsim::broadphase::{BroadBody, BroadphaseConfig, FloorRule, bodies_on_different_floors};
use regia_detsim::spatial::{DetSpatial, SpatialRow};
pub fn row(slot: usize, x: f32, z: f32) -> SpatialRow {
    SpatialRow {
        slot,
        body: BroadBody::new([x, 0.0, z], 0.0, [-1.0, 1.0]),
        group_mask: 1 << (slot % 40),
        flags: 0,
    }
}
pub fn distance(a: &BroadBody, b: &BroadBody) -> f32 {
    let dx = b.pos[0] - a.pos[0];
    let dz = b.pos[2] - a.pos[2];
    dx * dx + dz * dz
}
pub fn accepted(q: &BroadBody, r: f32, mask: u64, b: &SpatialRow, rule: FloorRule) -> bool {
    if b.group_mask & mask == 0 || bodies_on_different_floors(rule, q, &b.body) {
        return false;
    }
    let reach = r + b.body.radius;
    // This game's NaN policy deliberately accepts unordered distances.
    distance(q, &b.body).partial_cmp(&(reach * reach)) != Some(std::cmp::Ordering::Greater)
}
pub fn field() -> Vec<SpatialRow> {
    let mut seed = 0x1234_9876_u64;
    (0..900)
        .map(|slot| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let x = (seed >> 40) as f32 * (900.0 / 16777216.0) - 450.0;
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let z = (seed >> 40) as f32 * (900.0 / 16777216.0) - 450.0;
            row(slot, x, z)
        })
        .collect()
}
pub fn grid(rule: FloorRule) -> DetSpatial {
    DetSpatial::new(BroadphaseConfig {
        cell_size: 40.0,
        floor_rule: rule,
        ..Default::default()
    })
}
pub fn move_rows(rows: &mut [SpatialRow], tick: usize) {
    for b in rows {
        b.body.pos[0] += ((b.slot + tick) % 7) as f32 * 0.125 - 0.375;
        b.body.pos[2] += ((b.slot + tick) % 5) as f32 * 0.125 - 0.25;
    }
}
