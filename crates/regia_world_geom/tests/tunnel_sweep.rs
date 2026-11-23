// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-core/tests/tunnel_sweep.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 1-536, mark: adapt (Stage A: unchanged). Ranges re-checked against regia-absorb-2 e6c84ba6d and unmoved.
// Edits: module paths; hull stems become the radius list (2.5, 2.6, 3.6, 3.9, 4.1, 4.5 m) and the 0.35 m
// walker is the pilot probe; load is W-01's fixture plus the mouth holes and a grid stamped from the
// collision mesh; the worker fold is sequential (same chunk order); float sqrt and min/max/clamp through
// regia_detsim. The Cinder Steps stand on a flat +36 m shield (the canyon plane leaves those tubes in
// open air: no lids, and each ramp lip falls to the plane across the core). An empty solid set, or a
// map with no mouth holes and no patches, or an empty on-foot grid, is an error. Asserts of the
// source's sweep (push, lid, lane) are not weakened.

//! Stage A sweep of the Switchbacks and The Cinder Steps fixtures (plan row W-05, spec 121 § 10, § 13.1).
//!
//! Every hull radius is a disc at every station of every passage. The sim walls must not push it, the
//! floor under it must not stand a wall over its body, the lid must clear it, and the clear lane must
//! be at least the largest hull's diameter plus 1 m. The walker capsule, across the same floor, must
//! keep a clear run of at least 2 m. The on-foot grid is the collision mesh stamped with the T3 floor
//! rule (no installed set pieces: those files are not in the tree).

#![allow(clippy::unwrap_used)]

use regia_detsim::float::{det_clamp, det_max, det_min};
use regia_detsim::math;
use regia_world_geom::compat::fixtures::{ON_FOOT_PARAMS, for_map};
use regia_world_geom::compat::hover_ground::HULL_WALL_M;
use regia_world_geom::compat::layout::SampledPassage;
use regia_world_geom::compat::onfoot::{PILOT_SKIN_M, PilotGrid};
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::compat::state::{TunnelsState, UNIT_COLLISION_RADIUS_M};

/// Station step along every centreline (m). Line 32 of the source; the value is 0.5 m.
const STATION_M: f32 = 0.5;
/// Lateral step across a lane (m).
const LATERAL_M: f32 = 0.25;
/// A disc is tested this far inside its lane edge (m).
const LANE_MARGIN_M: f32 = 0.05;
/// Largest wall push a disc inside its own lane may get (m).
const PUSH_TOL_M: f32 = 0.02;
/// Hover height of the body over its floor in the sweep (m).
const HOVER_M: f32 = 1.0;
/// Every hull class's clear lane must be at least the largest hull's diameter plus this (m).
const HULL_LANE_MARGIN_M: f32 = 1.0;
/// The pilot's clear lane (capsule centres plus the capsule) must be at least this wide (m).
const PILOT_LANE_MIN_M: f32 = 2.0;

/// Hull radii the pathing lets into a tunnel, metres, plus a label. The 0.35 m walker is the pilot
/// probe below, not a seventh hull class (spec 121 § 10).
const HULL_RADII: [(f32, &str); 6] = [
    (2.5, "r2.5"),
    (2.6, "r2.6"),
    (3.6, "r3.6"),
    (3.9, "r3.9"),
    (4.1, "r4.1"),
    (4.5, "r4.5"),
];

/// Why a sweep must not run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SweepError {
    /// No solid set was loaded. A sweep of nothing is not a pass.
    EmptySolids,
    /// The map has neither mouth holes nor terrain patches.
    MissingPatches,
    /// Mouth holes are loaded and the on-foot grid has no cells.
    MissingOnFootGrid,
}

struct Station {
    s: f32,
    p: [f32; 3],
    left: [f32; 2],
    /// The segment the station is on.
    k: usize,
}

/// The centreline every [`STATION_M`], interpolated between the samples, plus the last sample.
fn stations(passage: &SampledPassage) -> Vec<Station> {
    let mut out = Vec::new();
    for (k, w) in passage.samples.windows(2).enumerate() {
        let (a, b) = (&w[0], &w[1]);
        let seg = b.s - a.s;
        let n = (seg / STATION_M).ceil() as usize;
        let n = n.max(1);
        for j in 0..n {
            let f = j as f32 / n as f32;
            let p = [
                a.p[0] + (b.p[0] - a.p[0]) * f,
                a.p[1] + (b.p[1] - a.p[1]) * f,
                a.p[2] + (b.p[2] - a.p[2]) * f,
            ];
            let l = [
                a.left[0] + (b.left[0] - a.left[0]) * f,
                a.left[1] + (b.left[1] - a.left[1]) * f,
            ];
            let ln = det_max(math::sqrt(l[0] * l[0] + l[1] * l[1]), 1e-6);
            out.push(Station {
                s: a.s + seg * f,
                p,
                left: [l[0] / ln, l[1] / ln],
                k,
            });
        }
    }
    if let Some(last) = passage.samples.last() {
        let k = passage.samples.len().saturating_sub(2);
        out.push(Station {
            s: last.s,
            p: last.p,
            left: last.left,
            k,
        });
    }
    out
}

fn seg_dist_xz(q: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let (abx, abz) = (b[0] - a[0], b[1] - a[1]);
    let len2 = abx * abx + abz * abz;
    let t = if len2 < 1e-9 {
        0.0
    } else {
        det_clamp(((q[0] - a[0]) * abx + (q[1] - a[1]) * abz) / len2, 0.0, 1.0)
    };
    let dx = q[0] - a[0] - abx * t;
    let dz = q[1] - a[1] - abz * t;
    math::sqrt(dx * dx + dz * dz)
}

/// The lane of a disc of radius `r` at a station, as lateral offsets `(right, left)`.
fn lane(passage: &SampledPassage, st: &Station, r: f32) -> (f32, f32) {
    let half = passage.width_m * 0.5;
    let need = r + LANE_MARGIN_M;
    let nseg = passage.samples.len().saturating_sub(1);
    let edge = |i: usize, sign: f32| -> [f32; 2] {
        let q = &passage.samples[i];
        [q.p[0] + q.left[0] * half * sign, q.p[2] + q.left[1] * half * sign]
    };
    let mut walls: Vec<([f32; 2], [f32; 2])> = Vec::new();
    let last = nseg.saturating_sub(1);
    let from = st.k.saturating_sub(1);
    let to = if st.k + 1 < last { st.k + 1 } else { last };
    for j in from..=to {
        if passage.wall_left.get(j).copied().unwrap_or(false) {
            walls.push((edge(j, 1.0), edge(j + 1, 1.0)));
        }
        if passage.wall_right.get(j).copied().unwrap_or(false) {
            walls.push((edge(j, -1.0), edge(j + 1, -1.0)));
        }
    }
    let clear = |o: f32| {
        let c = [st.p[0] + st.left[0] * o, st.p[2] + st.left[1] * o];
        walls.iter().all(|(a, b)| seg_dist_xz(c, *a, *b) >= need)
    };
    let floor_edge = det_max(half - need, 0.0);
    let reach = |dir: f32| -> f32 {
        if !clear(0.0) {
            return 0.0;
        }
        if clear(dir * floor_edge) {
            return dir * floor_edge;
        }
        let (mut lo, mut hi) = (0.0f32, floor_edge);
        for _ in 0..24 {
            let mid = (lo + hi) * 0.5;
            if clear(dir * mid) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        dir * lo
    };
    (reach(-1.0), reach(1.0))
}

/// Offsets every [`LATERAL_M`] from `lo` to `hi`, plus both ends.
fn offsets(lo: f32, hi: f32) -> Vec<f32> {
    if hi - lo <= 0.0 {
        return vec![(lo + hi) * 0.5];
    }
    let a = (lo / LATERAL_M).ceil() as i32;
    let b = (hi / LATERAL_M).floor() as i32;
    let mut out: Vec<f32> = vec![lo];
    out.extend(
        (a..=b)
            .map(|i| i as f32 * LATERAL_M)
            .filter(|o| *o > lo && *o < hi),
    );
    out.push(hi);
    out
}

/// True when a capped dead end is within `r` (plus a hair) of station `s`.
fn by_a_cap(passage: &SampledPassage, s: f32, r: f32) -> bool {
    let total = passage.samples.last().map_or(0.0, |q| q.s);
    (passage.cap_start && s < r + 0.6) || (passage.cap_end && s > total - r - 0.6)
}

struct Probe {
    floor_y: f32,
    push: f32,
    wall_rule: bool,
    lid: Option<f32>,
}

fn probe(
    tunnels: &TunnelsState,
    terrain: &TerrainSampler,
    x: f32,
    z: f32,
    floor_hint: f32,
    hover: f32,
    r: f32,
) -> Probe {
    let body = floor_hint + hover;
    let floor = tunnels.move_floor(Some(terrain), x, z, body);
    let floor_y = floor.point[1];
    let wall_rule = floor_y - body > HULL_WALL_M;
    let pos = [x, floor_y + hover, z];
    let push = match tunnels.confine(Some(terrain), pos, floor_y, r) {
        Some(q) => {
            let dx = q[0] - x;
            let dz = q[2] - z;
            math::sqrt(dx * dx + dz * dz)
        }
        None => 0.0,
    };
    let lid = tunnels.mesh.as_ref().and_then(|m| m.ceiling_above(x, z, floor_y));
    Probe {
        floor_y,
        push,
        wall_rule,
        lid,
    }
}

#[derive(Clone)]
struct Tally {
    stations: usize,
    probes: usize,
    blocked: usize,
    blocked_full: usize,
    worst: f32,
    worst_at: String,
    min_lid: f32,
    narrowest: f32,
    narrow_at: String,
}

impl Default for Tally {
    fn default() -> Self {
        Self {
            stations: 0,
            probes: 0,
            blocked: 0,
            blocked_full: 0,
            worst: 0.0,
            worst_at: String::new(),
            min_lid: f32::INFINITY,
            narrowest: f32::INFINITY,
            narrow_at: String::new(),
        }
    }
}

impl Tally {
    fn merge(&mut self, o: Tally) {
        self.stations += o.stations;
        self.probes += o.probes;
        self.blocked += o.blocked;
        self.blocked_full += o.blocked_full;
        if o.worst > self.worst {
            self.worst = o.worst;
            self.worst_at = o.worst_at;
        }
        self.min_lid = det_min(self.min_lid, o.min_lid);
        if o.narrowest < self.narrowest {
            self.narrowest = o.narrowest;
            self.narrow_at = o.narrow_at;
        }
    }
}

struct Chunk {
    hulls: Vec<Tally>,
    pilot: Tally,
    failures: Vec<String>,
}

struct World<'a> {
    map: &'a str,
    tunnels: &'a TunnelsState,
    terrain: &'a TerrainSampler,
    grid: &'a PilotGrid,
    radii: &'a [(f32, &'a str)],
}

fn hull_lane_wanted(radii: &[(f32, &str)]) -> f32 {
    let mut big = 0.0f32;
    for (r, _) in radii {
        big = det_max(big, *r);
    }
    2.0 * big + HULL_LANE_MARGIN_M
}

fn sweep_chunk(w: &World, passage: &SampledPassage, stations: &[Station]) -> Chunk {
    let mut out = Chunk {
        hulls: vec![Tally::default(); w.radii.len()],
        pilot: Tally::default(),
        failures: Vec::new(),
    };
    let want_hull = hull_lane_wanted(w.radii);
    for (ci, (r, stems)) in w.radii.iter().enumerate() {
        let rr = det_clamp(*r, 0.3, 6.0);
        let t = &mut out.hulls[ci];
        for st in stations {
            if by_a_cap(passage, st.s, rr) {
                continue;
            }
            t.stations += 1;
            let mut clear = 0usize;
            let (lo, hi) = lane(passage, st, rr);
            let width = hi - lo + 2.0 * rr;
            if width < t.narrowest {
                t.narrowest = width;
                t.narrow_at = format!("s {:.1} at ({:.1}, {:.1})", st.s, st.p[0], st.p[2]);
            }
            if width < want_hull {
                out.failures.push(format!(
                    "{}/{}: hull r {rr} ({stems}) lane {width:.2} m (want {want_hull:.2}) at s {:.1} ({:.1}, {:.1})",
                    w.map, passage.id, st.s, st.p[0], st.p[2]
                ));
            }
            for o in offsets(lo, hi) {
                let (x, z) = (st.p[0] + st.left[0] * o, st.p[2] + st.left[1] * o);
                let q = probe(w.tunnels, w.terrain, x, z, st.p[1], HOVER_M, rr);
                t.probes += 1;
                if let Some(lid) = q.lid {
                    t.min_lid = det_min(t.min_lid, lid - q.floor_y);
                }
                let low_lid = q
                    .lid
                    .is_some_and(|lid| lid - q.floor_y < HOVER_M + UNIT_COLLISION_RADIUS_M);
                if q.push > t.worst {
                    t.worst = q.push;
                    t.worst_at = format!("s {:.1} offset {o:+.2} at ({x:.1}, {z:.1})", st.s);
                }
                if q.push > PUSH_TOL_M || q.wall_rule || low_lid {
                    t.blocked += 1;
                    out.failures.push(format!(
                        "{}/{}: hull r {rr} ({stems}) at s {:.1} offset {o:+.2} ({x:.1}, {:.1}, {z:.1}): wall push {:.2} m{}{}",
                        w.map,
                        passage.id,
                        st.s,
                        q.floor_y,
                        q.push,
                        if q.wall_rule { ", the floor is a wall over the body" } else { "" },
                        if low_lid { ", lid too low" } else { "" },
                    ));
                } else {
                    clear += 1;
                }
            }
            if clear == 0 {
                t.blocked_full += 1;
            }
        }
    }
    let reach = ON_FOOT_PARAMS.capsule_radius_m + PILOT_SKIN_M;
    let want = PILOT_LANE_MIN_M;
    let t = &mut out.pilot;
    for st in stations {
        if by_a_cap(passage, st.s, reach) {
            continue;
        }
        t.stations += 1;
        let (mut run, mut best, mut prev): (f32, f32, Option<f32>) = (0.0, 0.0, None);
        let mut blockers: Vec<String> = Vec::new();
        let mut clear = 0usize;
        let (lo, hi) = lane(passage, st, reach);
        for o in offsets(lo, hi) {
            let (x, z) = (st.p[0] + st.left[0] * o, st.p[2] + st.left[1] * o);
            let q = probe(
                w.tunnels,
                w.terrain,
                x,
                z,
                st.p[1],
                0.0,
                ON_FOOT_PARAMS.capsule_radius_m,
            );
            t.probes += 1;
            let feet = q.floor_y;
            let grid = w
                .grid
                .slide(&ON_FOOT_PARAMS, x, z, feet, ON_FOOT_PARAMS.capsule_radius_m)
                != [x, z];
            // Fixtures carry no scatter rocks: the host's rock push is the identity.
            let rock = false;
            let wall = q.push > PUSH_TOL_M || q.wall_rule;
            if grid || rock || wall {
                t.blocked += 1;
                if blockers.len() < 4 {
                    blockers.push(format!(
                        "{o:+.2} ({x:.1}, {feet:.1}, {z:.1}){}{}",
                        if grid { " drawn-grid" } else { "" },
                        if wall { " sim-wall" } else { "" }
                    ));
                }
                prev = None;
                run = 0.0;
                continue;
            }
            clear += 1;
            run = match prev {
                Some(p) => run + (o - p),
                None => 0.0,
            };
            prev = Some(o);
            best = det_max(best, run + 2.0 * reach);
        }
        if clear == 0 {
            t.blocked_full += 1;
        }
        if best < t.narrowest {
            t.narrowest = best;
            t.narrow_at = format!(
                "s {:.1} at ({:.1}, {:.1}), blocked at {}",
                st.s,
                st.p[0],
                st.p[2],
                blockers.join(", ")
            );
        }
        if best < want {
            out.failures.push(format!(
                "{}/{}: pilot lane {best:.2} m (want {want:.2}) at s {:.1} ({:.1}, {:.1}, {:.1}); blocked at {}",
                w.map,
                passage.id,
                st.s,
                st.p[0],
                st.p[1],
                st.p[2],
                blockers.join(", ")
            ));
        }
    }
    out
}

/// Sweep every passage. Chunks of 120 stations, folded in order on this thread (the source's
/// worker fold; sequential is the same bits).
fn sweep_map(w: &World) -> (Vec<String>, Vec<Tally>, Tally) {
    const CHUNK: usize = 120;
    let layout = w.tunnels.layout.as_ref().expect("layout");
    let all: Vec<(&SampledPassage, Vec<Station>)> =
        layout.sampled.passages.iter().map(|p| (p, stations(p))).collect();
    let mut failures = Vec::new();
    let mut class_rows = 0usize;
    let mut hulls_all: Vec<Tally> = vec![Tally::default(); w.radii.len()];
    let mut pilot_all = Tally::default();
    for (passage, st) in &all {
        let mut hulls = vec![Tally::default(); w.radii.len()];
        let mut pilot = Tally::default();
        for chunk in st.chunks(CHUNK) {
            let part = sweep_chunk(w, passage, chunk);
            for (h, c) in hulls.iter_mut().zip(&part.hulls) {
                h.merge(c.clone());
            }
            pilot.merge(part.pilot.clone());
            failures.extend(part.failures.iter().cloned());
        }
        for ((r, stems), t) in w.radii.iter().zip(&hulls) {
            let rr = det_clamp(*r, 0.3, 6.0);
            eprintln!(
                "SWEEP {}/{} hull r {:.2} ({}): {} stations, {} probes, {} blocked, {} stations blocked across the lane, worst push {:.3} m{}, min lid {:.2} m, narrowest lane {:.2} m ({})",
                w.map,
                passage.id,
                rr,
                stems,
                t.stations,
                t.probes,
                t.blocked,
                t.blocked_full,
                t.worst,
                if t.worst > 0.0 {
                    format!(" ({})", t.worst_at)
                } else {
                    String::new()
                },
                t.min_lid,
                t.narrowest,
                t.narrow_at,
            );
            class_rows += 1;
        }
        eprintln!(
            "SWEEP {}/{} pilot: {} stations, {} probes, {} blocked, {} stations blocked across the floor, narrowest lane {:.2} m (want {PILOT_LANE_MIN_M:.2}; {})",
            w.map,
            passage.id,
            pilot.stations,
            pilot.probes,
            pilot.blocked,
            pilot.blocked_full,
            pilot.narrowest,
            pilot.narrow_at
        );
        class_rows += 1;
        for (h, c) in hulls_all.iter_mut().zip(hulls) {
            h.merge(c);
        }
        pilot_all.merge(pilot);
    }
    eprintln!("SWEEP_CLASS_ROWS {} {class_rows}", w.map);
    (failures, hulls_all, pilot_all)
}

/// Flat heightfield. Raw counts are 1/32 m, and 16384 is height 0, so 8192 is the -256 m canyon.
fn flat_ground(counts: u16) -> TerrainSampler {
    TerrainSampler::new(HeightGrid::square(4, vec![counts; 16]).unwrap(), 1280.0).with_zero_counts(16384)
}

/// The Switchbacks canyon floor, -256 m. The tubes sit under it, so they are covered and keep lids.
fn canyon() -> TerrainSampler {
    flat_ground(8192)
}

/// Shield over The Cinder Steps, +36 m (16384 + 36 * 32 counts).
///
/// The layout's floors run from about -11 m to +13 m. On the canyon plane they are open air: the
/// mesher emits no lids, and each uncovered ramp grows a lip down to that plane. The mid ramps leave
/// the core at z = 268, so that lip is a wall across the core near z = 255 and the pilot lane there
/// is 0 m. +36 m is above every tube lid (the north core's lid is about 24 m, plus 3.5 m of rock) and
/// below the ramp-mouth lids (about 44 m and 55 m), so the tubes stay covered and the mouths stay
/// open cuts. The lip then meets the shield at the mouth, not across the core.
fn cinder_shield() -> TerrainSampler {
    flat_ground(17536)
}

/// The heightfield the sweep stands this map on.
fn ground_for(map_id: &str) -> TerrainSampler {
    if map_id == "cinder_steps" {
        cinder_shield()
    } else {
        canyon()
    }
}

/// A sweep may run only with a solid set, mouth holes or patches, and a non-empty on-foot grid.
fn gate(terrain: &TerrainSampler, solids: Option<&PilotGrid>) -> Result<(), SweepError> {
    let Some(grid) = solids else {
        return Err(SweepError::EmptySolids);
    };
    if grid.cells.is_empty() {
        return Err(SweepError::MissingOnFootGrid);
    }
    if terrain.holes.is_empty() && terrain.patches.is_empty() {
        return Err(SweepError::MissingPatches);
    }
    Ok(())
}

/// The fixture, its mouth holes on the canyon sampler, and the collision mesh stamped into the
/// on-foot grid. `solids` `None` is an empty solid set. `Some(false)` loads the holes and an empty grid.
fn prepare(
    map_id: &str,
    solids: Option<bool>,
) -> Result<(TerrainSampler, TunnelsState, PilotGrid), SweepError> {
    let Some(stamp) = solids else {
        return Err(SweepError::EmptySolids);
    };
    let layout = for_map(map_id).unwrap_or_else(|| panic!("{map_id}"));
    let plain = ground_for(map_id);
    let holes = layout.sample(Some(&plain)).mouth_holes(&layout.openings);
    let terrain = if holes.is_empty() {
        plain
    } else {
        plain.with_holes(holes)
    };
    if !stamp {
        let grid = PilotGrid::default();
        return match gate(&terrain, Some(&grid)) {
            Err(e) => Err(e),
            Ok(()) => Err(SweepError::MissingOnFootGrid),
        };
    }
    let tunnels = TunnelsState::for_layout(layout, Some(&terrain));
    let grid = stamp_mesh(&tunnels, &terrain);
    gate(&terrain, Some(&grid))?;
    Ok((terrain, tunnels, grid))
}

fn stamp_mesh(tunnels: &TunnelsState, terrain: &TerrainSampler) -> PilotGrid {
    let mut grid = PilotGrid::default();
    let sim_floor = |x: f32, z: f32, y: f32| {
        let ground = terrain.height_at_world(x, z);
        let span = tunnels.nearest_floor(Some(terrain), x, z, y);
        if (span - y).abs() < (ground - y).abs() {
            span
        } else {
            ground
        }
    };
    for tri in &tunnels.mesh.as_ref().expect("mesh").triangles {
        grid.stamp_triangle(tri.a, tri.b, tri.c, Some(&sim_floor));
    }
    grid
}

fn assert_green(map_id: &str) {
    let (terrain, tunnels, grid) = prepare(map_id, Some(true)).unwrap_or_else(|e| panic!("{map_id}: {e:?}"));
    assert!(!grid.cells.is_empty(), "{map_id}: the stamped grid is empty");
    assert!(!terrain.holes.is_empty(), "{map_id}: the mouths cut no holes");
    let w = World {
        map: map_id,
        tunnels: &tunnels,
        terrain: &terrain,
        grid: &grid,
        radii: &HULL_RADII,
    };
    let (failures, hulls, pilot) = sweep_map(&w);
    let shown: Vec<&String> = failures.iter().take(40).collect();
    assert!(
        failures.is_empty(),
        "{} blocked probes or stations (first {}):\n{}",
        failures.len(),
        shown.len(),
        shown.iter().map(|f| f.as_str()).collect::<Vec<_>>().join("\n")
    );
    let want = hull_lane_wanted(&HULL_RADII);
    for (t, (r, _)) in hulls.iter().zip(HULL_RADII) {
        assert_eq!(
            t.blocked, 0,
            "{map_id} hull {r}: {} blocked probes at {}",
            t.blocked, t.worst_at
        );
        assert!(
            t.worst < 0.0005,
            "{map_id} hull {r}: worst push {:.4} m at {}",
            t.worst,
            t.worst_at
        );
        assert!(
            t.min_lid.is_finite() && t.min_lid >= 10.0,
            "{map_id} hull {r}: min lid {:.3} m",
            t.min_lid
        );
        assert!(
            t.narrowest + 1e-3 >= want,
            "{map_id} hull {r}: lane {:.3} m, want {want:.2}, at {}",
            t.narrowest,
            t.narrow_at
        );
    }
    assert_eq!(
        pilot.blocked_full, 0,
        "{map_id}: a station blocked across the floor"
    );
    assert!(
        pilot.narrowest + 1e-3 >= PILOT_LANE_MIN_M,
        "{map_id}: pilot lane {:.3} m at {}",
        pilot.narrowest,
        pilot.narrow_at
    );
}

#[test]
fn an_empty_solid_set_fails_the_sweep() {
    let err = prepare("switchbacks", None).unwrap_err();
    assert_eq!(err, SweepError::EmptySolids);
}

#[test]
fn sweep_without_patches_or_onfoot_grid_is_an_error() {
    let no_grid = prepare("switchbacks", Some(false)).unwrap_err();
    assert_eq!(no_grid, SweepError::MissingOnFootGrid);
    let plain = canyon();
    let mut grid = PilotGrid::default();
    grid.stamp_wall(0.0, 0.0, 0.0, 2.0);
    assert!(!grid.cells.is_empty());
    assert_eq!(gate(&plain, Some(&grid)), Err(SweepError::MissingPatches));
}

#[test]
fn sweep_switchbacks_and_cinder_steps_are_clear() {
    assert_green("switchbacks");
    assert_green("cinder_steps");
}
