// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-core/tests/tunnel_t3.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 102-130 (`discontinuity`), 233-265 (mouth and passage routes), mark: requirements (Stage A: unchanged).
// Ranges re-checked against regia-absorb-2 e6c84ba6d and unmoved. Edits: module paths; the drive is the
// kinematic polyline of tests/stage_a_hop.rs (the host step stays a DO27 pin); packing is [x, z, y] as in
// the source; float sqrt through regia_detsim::math. No product names.

//! Shared hop meter for the Stage A proofs (plan row W-05).
//!
//! W-02's mouth-hop test, the canyon layer test, and later rows W-29 and W-31 call this. The
//! discontinuity is the source's: 17 samples of the tick, then ten halvings toward the larger half.
//! A slope shrinks with the interval and reads as zero; a step keeps its height.
//!
//! Integration tests are not crate modules. Each test file that needs this does
//! `#[path = "support/hop_meter.rs"]`. This file has no `#[test]` items, so that does not duplicate tests.

#![allow(
    dead_code,
    reason = "included by #[path] from more than one test; each test uses a subset of the exports"
)]

use regia_detsim::math;
use regia_world_geom::compat::state::TunnelsState;

/// Largest surface jump one tick may show (metres). The source's `HOP_LIMIT_M`.
pub const HOP_LIMIT_M: f32 = 0.05;
/// Samples of one tick's travel after the first (17 points).
pub const SUB: usize = 16;
/// Ticks per second of the source's sim.
pub const TICK_HZ: f32 = 60.0;

/// The discontinuity of `f` along the straight move `a -> b`.
///
/// Each point is `[x, z, y]`, and `f` is called as `f(x, z, y)`, in that order (the source's packing).
pub fn discontinuity(f: &dyn Fn(f32, f32, f32) -> f32, a: [f32; 3], b: [f32; 3]) -> f32 {
    let at = |t: f32| {
        f(
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
        )
    };
    let mut worst = (0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32);
    let mut prev = at(0.0);
    for k in 1..=SUB {
        let (t0, t1) = ((k - 1) as f32 / SUB as f32, k as f32 / SUB as f32);
        let cur = at(t1);
        if (cur - prev).abs() > worst.0 {
            worst = ((cur - prev).abs(), t0, t1, prev, cur);
        }
        prev = cur;
    }
    if worst.0 < 0.005 {
        return 0.0;
    }
    let (_, mut t0, mut t1, mut f0, mut f1) = worst;
    for _ in 0..10 {
        let tm = (t0 + t1) * 0.5;
        let fm = at(tm);
        if (fm - f0).abs() >= (f1 - fm).abs() {
            (t1, f1) = (tm, fm);
        } else {
            (t0, f0) = (tm, fm);
        }
    }
    (f1 - f0).abs()
}

/// One drive along a route: the worst per-tick surface jump, and the layer the body stayed in.
#[derive(Debug, Default)]
pub struct Meter {
    /// Largest discontinuity, metres.
    pub hop: f32,
    /// Where `hop` was measured.
    pub hop_at: String,
    /// Ticks stepped.
    pub ticks: u32,
    /// Ticks [`TunnelsState::underground`] was true for the body's position.
    pub underground: u32,
    /// Waypoints the polyline finished (the whole route, for this kinematic drive).
    pub reached: usize,
    /// Waypoints in the route.
    pub len: usize,
    /// Lowest surface height the body stood on, metres.
    pub low: f32,
    /// Highest surface height the body stood on, metres.
    pub high: f32,
}

/// Drive `route` at `speed` metres per second from a body whose surface hint is `start_y`.
///
/// `surface(x, z, y)` is the floor under a body at that point (the Stage A mouth test passes
/// `sample_move_floor`). `underground` is the tunnel arm's covered-gallery test at `[x, y, z]`.
/// The body stands on the surface it just read. The route is finished; `reached` equals `len`.
pub fn drive(
    surface: &dyn Fn(f32, f32, f32) -> f32,
    underground: &dyn Fn([f32; 3]) -> bool,
    route: &[[f32; 2]],
    speed: f32,
    start_y: f32,
) -> Meter {
    let step = speed / TICK_HZ;
    let mut m = Meter {
        len: route.len(),
        reached: route.len(),
        low: f32::INFINITY,
        high: f32::NEG_INFINITY,
        ..Meter::default()
    };
    if route.is_empty() {
        m.reached = 0;
        return m;
    }
    let mut feet = surface(route[0][0], route[0][1], start_y);
    m.low = feet;
    m.high = feet;
    let mut pos = route[0];
    for leg in route.windows(2) {
        let (a, b) = (leg[0], leg[1]);
        let (dx, dz) = (b[0] - a[0], b[1] - a[1]);
        let len = math::sqrt(dx * dx + dz * dz);
        let ticks = (len / step).ceil() as usize;
        let ticks = ticks.max(1);
        for k in 1..=ticks {
            let t = if k >= ticks || len < 1e-6 {
                1.0
            } else {
                k as f32 * step / len
            };
            let next = [a[0] + dx * t, a[1] + dz * t];
            let next_feet = surface(next[0], next[1], feet);
            let jump = discontinuity(surface, [pos[0], pos[1], feet], [next[0], next[1], next_feet]);
            if jump > m.hop {
                m.hop = jump;
                m.hop_at = format!(
                    "({:.2}, {:.2}) -> ({:.2}, {:.2}), feet {feet:.3} -> {next_feet:.3}",
                    pos[0], pos[1], next[0], next[1]
                );
            }
            if underground([next[0], next_feet, next[1]]) {
                m.underground += 1;
            }
            m.ticks += 1;
            if next_feet < m.low {
                m.low = next_feet;
            }
            if next_feet > m.high {
                m.high = next_feet;
            }
            pos = next;
            feet = next_feet;
        }
    }
    m
}

fn passage_xz(tunnels: &TunnelsState, id: &str) -> Vec<[f32; 2]> {
    let map = tunnels.layout.as_ref().expect("layout");
    map.sampled
        .passage(id)
        .unwrap_or_else(|| panic!("{id}"))
        .samples
        .iter()
        .map(|q| [q.p[0], q.p[2]])
        .collect()
}

/// `d` metres past a ramp's top along its own line.
pub fn past_top(l: &[[f32; 2]], d: f32) -> [f32; 2] {
    let (a, b) = (l[l.len() - 2], l[l.len() - 1]);
    let n = math::sqrt((b[0] - a[0]) * (b[0] - a[0]) + (b[1] - a[1]) * (b[1] - a[1]));
    [b[0] + (b[0] - a[0]) / n * d, b[1] + (b[1] - a[1]) / n * d]
}

/// Every mouth: from the plain past the top, down the ramp, back up and out.
pub fn mouth_routes(tunnels: &TunnelsState) -> Vec<(String, Vec<[f32; 2]>)> {
    let map = tunnels.layout.as_ref().expect("layout");
    map.layout
        .openings
        .iter()
        .map(|o| {
            let ramp = passage_xz(tunnels, &o.ramp);
            let mut route = vec![past_top(&ramp, 10.0)];
            route.extend(ramp.iter().rev());
            route.extend(ramp.iter().skip(1));
            route.push(past_top(&ramp, 10.0));
            (o.id.clone(), route)
        })
        .collect()
}

/// Every passage that is not a mouth ramp, end to end and back, and its first floor height.
pub fn passage_routes(tunnels: &TunnelsState) -> Vec<(String, Vec<[f32; 2]>, f32)> {
    let map = tunnels.layout.as_ref().expect("layout");
    let ramps: Vec<&str> = map.layout.openings.iter().map(|o| o.ramp.as_str()).collect();
    map.sampled
        .passages
        .iter()
        .filter(|p| !ramps.contains(&p.id.as_str()))
        .map(|p| {
            let line = passage_xz(tunnels, &p.id);
            let mut route = line.clone();
            route.extend(line.iter().rev().skip(1));
            (p.id.clone(), route, p.samples[0].p[1])
        })
        .collect()
}
