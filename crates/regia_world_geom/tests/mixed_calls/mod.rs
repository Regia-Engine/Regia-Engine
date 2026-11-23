// SPDX-License-Identifier: MPL-2.0
//! The mixed sim-query tick of spec 121 § 11 (plan § 4.7, row W-31): the calls a 40-seat DO27
//! tick makes on the native query, built once from a cooked map's sampled layout, the same set for
//! the bench (`benches/mixed_calls.rs`) and the allocation test (`tests/perf_low_tiers.rs`).
//!
//! One tick is [`TICK_CALLS`] calls:
//! - [`FLOOR_CALLS`] body `support` reads: 120 bodies in the tubes, 80 on the surface;
//! - [`HULLS`] hover-hull confines, each a `push_disc` (radius 3 m, DO27's band
//!   `[floor + 0.5, body + 2.0]`) and a `clamp_under_ceiling`: [`CONFINE_CALLS`] calls;
//! - at the projectile cap, [`SEGMENT_CALLS`] shell `segment` sweeps and [`SHELL_FLOOR_CALLS`]
//!   shell `support` reads, half in the tubes and half outdoors;
//! - [`LOS_CALLS`] AI `line_of_sight` checks between bodies.
//!
//! Positions are deterministic (the layout's samples and a fixed LCG), so every device runs the
//! same calls.
#![allow(dead_code)]

use regia_world_geom::compat::layout::SampledLayout;
use regia_world_geom::{Band, FaceMask, GeomState, WorldGeom};
use std::hint::black_box;

/// Body `support` calls per tick.
pub const FLOOR_CALLS: usize = 200;
/// Hover hulls confined per tick.
pub const HULLS: usize = 40;
/// Confine calls per tick: a `push_disc` and a `clamp_under_ceiling` per hull.
pub const CONFINE_CALLS: usize = 2 * HULLS;
/// Shell `segment` calls per tick.
pub const SEGMENT_CALLS: usize = 64;
/// Shell `support` calls per tick.
pub const SHELL_FLOOR_CALLS: usize = 64;
/// AI `line_of_sight` calls per tick.
pub const LOS_CALLS: usize = 100;
/// Calls in one tick.
pub const TICK_CALLS: usize = FLOOR_CALLS + CONFINE_CALLS + SEGMENT_CALLS + SHELL_FLOOR_CALLS + LOS_CALLS;

/// The hover hull radius of the confine calls, metres.
pub const HULL_RADIUS_M: f32 = 3.0;
/// The hover hull's half height for the lid clamp, metres.
pub const HULL_HALF_H_M: f32 = 1.0;

/// One call family, for the per-kind breakdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Body `support`.
    Floor,
    /// `push_disc` + `clamp_under_ceiling`.
    Confine,
    /// Shell `segment`.
    Segment,
    /// Shell `support`.
    ShellFloor,
    /// `line_of_sight`.
    Los,
}

impl Kind {
    /// Every kind, in tick order.
    pub const ALL: [Kind; 5] = [
        Kind::Floor,
        Kind::Confine,
        Kind::Segment,
        Kind::ShellFloor,
        Kind::Los,
    ];

    /// Stable name.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Floor => "support",
            Kind::Confine => "push_disc+clamp_under_ceiling",
            Kind::Segment => "segment",
            Kind::ShellFloor => "shell_support",
            Kind::Los => "line_of_sight",
        }
    }

    /// Calls of this kind per tick.
    pub fn calls(self) -> usize {
        match self {
            Kind::Floor => FLOOR_CALLS,
            Kind::Confine => CONFINE_CALLS,
            Kind::Segment => SEGMENT_CALLS,
            Kind::ShellFloor => SHELL_FLOOR_CALLS,
            Kind::Los => LOS_CALLS,
        }
    }
}

/// One hull's confine arguments.
#[derive(Clone, Copy, Debug)]
pub struct Hull {
    /// Centre XZ.
    pub xz: [f32; 2],
    /// Body centre height.
    pub y: f32,
    /// Floor height under it.
    pub floor_y: f32,
    /// The wall band.
    pub band: Band,
}

/// The call arguments of one tick.
pub struct MixedTick {
    /// Body `support` points (x, y, z).
    pub floors: Vec<[f32; 3]>,
    /// The hulls.
    pub hulls: Vec<Hull>,
    /// Shell `segment` endpoints.
    pub segments: Vec<([f32; 3], [f32; 3])>,
    /// Shell `support` points.
    pub shells: Vec<[f32; 3]>,
    /// `line_of_sight` endpoints.
    pub los: Vec<([f32; 3], [f32; 3])>,
}

/// A fixed 32-bit LCG (Numerical Recipes constants), as a unit float.
struct Lcg(u32);

impl Lcg {
    fn unit(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 8) as f32 / (1u32 << 24) as f32
    }
}

impl MixedTick {
    /// The tick on a map whose sampled layout is `sampled`, whose outdoor ground is at `ground_y`
    /// and whose square extent is `[-half_m, half_m]` on X and Z.
    pub fn new(sampled: &SampledLayout, ground_y: f32, half_m: f32) -> Self {
        // Every tube sample with its floor height, left normal and floor width.
        let tube: Vec<([f32; 3], [f32; 2], f32)> = sampled
            .passages
            .iter()
            .flat_map(|p| p.samples.iter().map(move |s| (s.p, s.left, p.width_m)))
            .collect();
        assert!(!tube.is_empty(), "the layout has no samples");
        let mut rng = Lcg(0x5713_0031);
        let pick = |i: usize, n: usize| tube[(i * tube.len()) / n.max(1)];
        let outdoor = |rng: &mut Lcg| {
            [
                (rng.unit() * 2.0 - 1.0) * half_m * 0.9,
                ground_y,
                (rng.unit() * 2.0 - 1.0) * half_m * 0.9,
            ]
        };
        // Bodies: 120 in the tubes (spread across the floor width), 80 on the surface.
        let mut bodies = Vec::with_capacity(FLOOR_CALLS);
        for i in 0..120 {
            let (p, left, w) = pick(i, 120);
            let off = (rng.unit() - 0.5) * w * 0.6;
            bodies.push([p[0] + left[0] * off, p[1], p[2] + left[1] * off]);
        }
        for _ in 0..(FLOOR_CALLS - 120) {
            bodies.push(outdoor(&mut rng));
        }
        let floors = bodies.iter().map(|b| [b[0], b[1] + 1.0, b[2]]).collect();
        let hulls: Vec<Hull> = (0..HULLS)
            .map(|i| {
                let (p, left, w) = pick(i * 3 + 1, HULLS * 3);
                let off = (rng.unit() - 0.5) * w * 0.4;
                Hull {
                    xz: [p[0] + left[0] * off, p[2] + left[1] * off],
                    y: p[1] + 1.0,
                    floor_y: p[1],
                    band: Band {
                        lo: p[1] + 0.5,
                        hi: p[1] + 1.0 + 2.0,
                    },
                }
            })
            .collect();
        let half_seg = SEGMENT_CALLS / 2;
        let mut segments = Vec::with_capacity(SEGMENT_CALLS);
        let mut shells = Vec::with_capacity(SHELL_FLOOR_CALLS);
        for i in 0..half_seg {
            // A shell along a tube: from a muzzle to 20 m down the passage; its floor read at the end.
            let j = (i * tube.len()) / half_seg;
            let (a, _, _) = tube[j];
            let (b, _, _) = tube[(j + 4).min(tube.len() - 1)];
            segments.push(([a[0], a[1] + 1.5, a[2]], [b[0], b[1] + 1.2, b[2]]));
            shells.push([b[0], b[1] + 1.2, b[2]]);
        }
        for _ in half_seg..SEGMENT_CALLS {
            // A falling shell outdoors: 40 m above the ground to 2 m under it, 15 m sideways.
            let p = outdoor(&mut rng);
            segments.push(([p[0], p[1] + 40.0, p[2]], [p[0] + 15.0, p[1] - 2.0, p[2] + 6.0]));
            shells.push([p[0] + 15.0, p[1] + 0.5, p[2] + 6.0]);
        }
        let los = (0..LOS_CALLS)
            .map(|i| {
                let a = bodies[i % bodies.len()];
                let b = bodies[(i * 7 + 3) % bodies.len()];
                ([a[0], a[1] + 1.7, a[2]], [b[0], b[1] + 1.7, b[2]])
            })
            .collect();
        let tick = MixedTick {
            floors,
            hulls,
            segments,
            shells,
            los,
        };
        assert_eq!(tick.calls(), TICK_CALLS);
        tick
    }

    /// Calls in the tick.
    pub fn calls(&self) -> usize {
        self.floors.len() + 2 * self.hulls.len() + self.segments.len() + self.shells.len() + self.los.len()
    }

    /// Runs one family of calls.
    pub fn run_kind(&self, kind: Kind, geom: &WorldGeom, state: &GeomState) {
        match kind {
            Kind::Floor => {
                for &[x, y, z] in &self.floors {
                    black_box(geom.support(state, black_box(x), z, y));
                }
            }
            Kind::Confine => {
                for h in &self.hulls {
                    let [x, z] = black_box(h.xz);
                    black_box(geom.push_disc(state, x, z, HULL_RADIUS_M, h.band));
                    black_box(geom.clamp_under_ceiling(state, x, h.y, z, h.floor_y, HULL_HALF_H_M));
                }
            }
            Kind::Segment => {
                for &(a, b) in &self.segments {
                    black_box(geom.segment(state, black_box(a), b, FaceMask::SOLID));
                }
            }
            Kind::ShellFloor => {
                for &[x, y, z] in &self.shells {
                    black_box(geom.support(state, black_box(x), z, y));
                }
            }
            Kind::Los => {
                for &(a, b) in &self.los {
                    black_box(geom.line_of_sight(state, black_box(a), b));
                }
            }
        }
    }

    /// Runs the whole tick, in [`Kind::ALL`] order.
    pub fn run(&self, geom: &WorldGeom, state: &GeomState) {
        for kind in Kind::ALL {
            self.run_kind(kind, geom, state);
        }
    }
}
