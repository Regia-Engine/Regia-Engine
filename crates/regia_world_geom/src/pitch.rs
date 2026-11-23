// SPDX-License-Identifier: MPL-2.0
//! Nested ground pitch (spec 121 § 3 "Memory and the ground mode" and the nested-pitch seam rule E1;
//! lane W-10, ADR-8214). Cook-only.
//!
//! In `GroundMode::Nested { max_level }` each ground tile takes its own level (pitch `s * 2^level`),
//! in this order:
//!
//! 1. **Level by error.** The coarsest level `<= max_level` whose ground stays within
//!    `pitch_error_m` of the full-pitch ground at every full-pitch vertex of the tile (the coarse
//!    and fine triangulations nest with the same diagonal, so the largest difference is at a
//!    full-pitch vertex). The error is measured in `f64` from the `f32` count heights.
//! 2. **Over budget** is a cook error that lists the tiles (`ground_over_budget`, a
//!    [`crate::checks`] finding since ADR-8436).
//! 3. **Neighbors differ by at most one level**: a tile two levels coarser than a neighbor is refined.
//! 4. **Hanging vertices take the coarse edge**: a border vertex of the finer tile that the coarser
//!    tile lacks gets an absolute override, `m_to_mm` of the coarse triangle's plane height there
//!    (spec 121 § 5.3 rule 2's `f32` arithmetic, [`crate::ground::plane_height`]).
//! 5. **No pitch border within `fine_band_m`** of a mouth ring or a layer's XZ footprint: tiles that
//!    touch the band stay at full pitch, and so does every tile whose border with a coarser tile
//!    would lie inside it, until no such border is left.
//!
//! [`check_ground_pitch_seam`] is the cook check `ground_pitch_seam` (§ 10): it re-measures rules 1,
//! 3, 4 and 5 on the cooked section, the step across every pitch border included (at most 1 mm,
//! C3 option (a)).

use std::collections::BTreeMap;

use crate::cooked::{LatticeConstants, VertexHeightMm, WorldGeomCooked};
use crate::ground::{GroundLattice, plane_height, vertex_m};
use crate::units::m_to_mm;

/// The largest step across a pitch border the cook accepts, metres (spec 121 § 3 rule 4, 1 mm:
/// 0.5 mm from the override's millimetre rounding plus `f32` rounding of the plane heights).
pub const PITCH_SEAM_STEP_MAX_M: f32 = 0.001;

/// The sim-geometry budget of spec 121 § 11 for a map `width_mm` wide, bytes: 35 MB up to 2.56 km
/// (34 MB before ADR-8396: a full-pitch 4096² lattice is 33,554,432 bytes and Cinder Steps' seal,
/// pins, cut bitmaps, index and pyramid need about 560 KB more), 64 MB beyond.
#[must_use]
pub fn sim_budget_bytes(width_mm: i32) -> u64 {
    // The same constants as `checks::ground_budget_bytes` (ADR-8436): one budget.
    if width_mm <= 2_560_000 {
        crate::checks::GROUND_BUDGET_SMALL
    } else {
        crate::checks::GROUND_BUDGET_LARGE
    }
}

/// An XZ rectangle, world millimetres, `[min, max]` inclusive.
pub type RectMm = [[i64; 2]; 2];

/// What the nested-level choice needs besides the heights.
#[derive(Clone, Debug, Default)]
pub struct PitchInputs {
    /// `pitch_error_m` (seam rule 1).
    pub pitch_error_m: f32,
    /// `fine_band_m` (seam rule 5).
    pub fine_band_m: f32,
    /// The XZ bounds of every mouth ring and every layer triangle (seam rule 5), world mm.
    pub footprints: Vec<RectMm>,
}

/// The rectangle of ground tile `(a, b)`, world mm.
#[must_use]
pub fn tile_rect(lattice: &LatticeConstants, a: u32, b: u32) -> RectMm {
    let (x0, x1) = lattice.tile_cells(a);
    let (z0, z1) = lattice.tile_cells(b);
    [
        [lattice.vertex_mm(x0), lattice.vertex_mm(z0)],
        [lattice.vertex_mm(x1), lattice.vertex_mm(z1)],
    ]
}

fn near(r: &RectMm, f: &RectMm, band_mm: i64) -> bool {
    r[0][0] <= f[1][0] + band_mm
        && r[1][0] >= f[0][0] - band_mm
        && r[0][1] <= f[1][1] + band_mm
        && r[1][1] >= f[0][1] - band_mm
}

fn count_height(lattice: &LatticeConstants, counts: &[u16], i: u32, j: u32) -> f32 {
    crate::cut::count_height_m(lattice, counts, i, j)
}

/// The largest difference, metres, between ground tile `(a, b)` drawn at `level` and the full-pitch
/// ground, over the tile's full-pitch vertices (`f64`); stops early once over `stop`.
#[must_use]
pub fn tile_level_error(
    lattice: &LatticeConstants,
    counts: &[u16],
    a: u32,
    b: u32,
    level: u8,
    stop: f64,
) -> f64 {
    let (x0, x1) = lattice.tile_cells(a);
    let (z0, z1) = lattice.tile_cells(b);
    let s = 1u32 << level;
    let h = |i: u32, j: u32| f64::from(count_height(lattice, counts, i, j));
    let mut worst = 0.0f64;
    for cj in (z0..z1).step_by(s as usize) {
        for ci in (x0..x1).step_by(s as usize) {
            let (h00, h10, h01, h11) = (h(ci, cj), h(ci + s, cj), h(ci, cj + s), h(ci + s, cj + s));
            for dj in 0..=s {
                for di in 0..=s {
                    let (u, w) = (f64::from(di) / f64::from(s), f64::from(dj) / f64::from(s));
                    let coarse = if u + w <= 1.0 {
                        h00 + u * (h10 - h00) + w * (h01 - h00)
                    } else {
                        h11 + (1.0 - u) * (h01 - h11) + (1.0 - w) * (h10 - h11)
                    };
                    let e = (coarse - h(ci + di, cj + dj)).abs();
                    if e > worst {
                        worst = e;
                        if worst > stop {
                            return worst;
                        }
                    }
                }
            }
        }
    }
    worst
}

/// Seam rule 1 for one tile: the coarsest level `<= max_level` the tile's sides allow whose error
/// is within `pitch_error_m`.
#[must_use]
pub fn coarsest_level_within_error(
    lattice: &LatticeConstants,
    counts: &[u16],
    a: u32,
    b: u32,
    max_level: u8,
    pitch_error_m: f32,
) -> u8 {
    let limit = f64::from(pitch_error_m);
    for level in (1..=max_level).rev() {
        if lattice.tile_side_vertices(a, level).is_none() || lattice.tile_side_vertices(b, level).is_none() {
            continue;
        }
        if tile_level_error(lattice, counts, a, b, level, limit) <= limit {
            return level;
        }
    }
    0
}

/// The neighbors of tile `t` east and north (each pair once), with the shared border's rectangle.
fn borders(lattice: &LatticeConstants) -> Vec<(u32, u32, RectMm)> {
    let per_side = lattice.tiles_per_side();
    let mut out = Vec::new();
    for b in 0..per_side {
        for a in 0..per_side {
            let t = b * per_side + a;
            let r = tile_rect(lattice, a, b);
            if a + 1 < per_side {
                out.push((t, t + 1, [[r[1][0], r[0][1]], [r[1][0], r[1][1]]]));
            }
            if b + 1 < per_side {
                out.push((t, t + per_side, [[r[0][0], r[1][1]], [r[1][0], r[1][1]]]));
            }
        }
    }
    out
}

/// The level of every ground tile under seam rules 1, 3 and 5 (ascending tile index).
#[must_use]
pub fn choose_levels(
    lattice: &LatticeConstants,
    counts: &[u16],
    max_level: u8,
    inputs: &PitchInputs,
) -> Vec<u8> {
    let per_side = lattice.tiles_per_side();
    let band_mm = i64::from(m_to_mm(inputs.fine_band_m));
    let mut levels = Vec::with_capacity((per_side * per_side) as usize);
    for b in 0..per_side {
        for a in 0..per_side {
            let r = tile_rect(lattice, a, b);
            if inputs.footprints.iter().any(|f| near(&r, f, band_mm)) {
                levels.push(0);
            } else {
                levels.push(coarsest_level_within_error(
                    lattice,
                    counts,
                    a,
                    b,
                    max_level,
                    inputs.pitch_error_m,
                ));
            }
        }
    }
    let pairs = borders(lattice);
    loop {
        let mut changed = false;
        for &(p, q, edge) in &pairs {
            let (lp, lq) = (levels[p as usize], levels[q as usize]);
            if lp == lq {
                continue;
            }
            let (fine, coarse) = if lp < lq { (p, q) } else { (q, p) };
            let lf = levels[fine as usize];
            // Rule 5: a pitch border inside the band goes; rule 3: at most one level apart.
            let target = if inputs.footprints.iter().any(|f| near(&edge, f, band_mm)) {
                lf
            } else if levels[coarse as usize] > lf + 1 {
                lf + 1
            } else {
                continue;
            };
            levels[coarse as usize] = target;
            changed = true;
        }
        if !changed {
            break;
        }
    }
    levels
}

/// The count blocks of every ground tile at its level (ascending tile index, row major, border
/// vertices included).
#[must_use]
pub fn count_blocks(lattice: &LatticeConstants, counts: &[u16], levels: &[u8]) -> Vec<Vec<u16>> {
    let n = lattice.lattice_n as usize;
    let per_side = lattice.tiles_per_side();
    let mut blocks = Vec::with_capacity(levels.len());
    for b in 0..per_side {
        let (z0, z1) = lattice.tile_cells(b);
        for a in 0..per_side {
            let (x0, x1) = lattice.tile_cells(a);
            let s = 1usize << levels[(b * per_side + a) as usize];
            let mut block = Vec::new();
            for j in (z0 as usize..=z1 as usize).step_by(s) {
                for i in (x0 as usize..=x1 as usize).step_by(s) {
                    block.push(counts[j * n + i]);
                }
            }
            blocks.push(block);
        }
    }
    blocks
}

/// The `f32` vertex `(i, j)` at its count height.
fn vertex_f32(lattice: &LatticeConstants, counts: &[u16], i: u32, j: u32) -> [f32; 3] {
    [
        vertex_m(lattice, i),
        count_height(lattice, counts, i, j),
        vertex_m(lattice, j),
    ]
}

/// Seam rule 4: the hanging-vertex overrides of every pitch border, ascending vertex index.
#[must_use]
pub fn hanging_overrides(lattice: &LatticeConstants, counts: &[u16], levels: &[u8]) -> Vec<VertexHeightMm> {
    let per_side = lattice.tiles_per_side();
    let n = lattice.lattice_n;
    let mut out: BTreeMap<u32, i32> = BTreeMap::new();
    let v = |i: u32, j: u32| vertex_f32(lattice, counts, i, j);
    for b in 0..per_side {
        for a in 0..per_side {
            let t = b * per_side + a;
            let (x0, x1) = lattice.tile_cells(a);
            let (z0, z1) = lattice.tile_cells(b);
            // East border (x = x1, between t and t + 1) and north border (z = z1, t and t + per_side).
            for (east, other) in [(true, a + 1 < per_side), (false, b + 1 < per_side)] {
                if !other {
                    continue;
                }
                let q = if east { t + 1 } else { t + per_side };
                let (lt, lq) = (levels[t as usize], levels[q as usize]);
                if lt == lq {
                    continue;
                }
                let (lf, lc) = (lt.min(lq), lt.max(lq));
                // The coarse tile lies east/north of the border when it is q.
                let coarse_after = lq > lt;
                let (sf, sc) = (1u32 << lf, 1u32 << lc);
                let (start, end) = if east { (z0, z1) } else { (x0, x1) };
                let mut k = start;
                while k <= end {
                    if (k - start).is_multiple_of(sf) && !(k - start).is_multiple_of(sc) {
                        let c0 = start + ((k - start) / sc) * sc;
                        let c1 = c0 + sc;
                        // The coarse triangle holding the border edge: its west or south edge is in
                        // the lower triangle [v00, v01, v10]; its east or north edge in the upper.
                        let (x, z, tri) = if east {
                            let bx = x1;
                            let tri = if coarse_after {
                                [v(bx, c0), v(bx, c1), v(bx + sc, c0)]
                            } else {
                                [v(bx, c0), v(bx - sc, c1), v(bx, c1)]
                            };
                            (bx, k, tri)
                        } else {
                            let bz = z1;
                            let tri = if coarse_after {
                                [v(c0, bz), v(c0, bz + sc), v(c1, bz)]
                            } else {
                                [v(c1, bz - sc), v(c0, bz), v(c1, bz)]
                            };
                            (k, bz, tri)
                        };
                        let y = plane_height(tri, vertex_m(lattice, x), vertex_m(lattice, z))
                            .unwrap_or_else(|| count_height(lattice, counts, x, z));
                        out.insert(z * n + x, m_to_mm(y));
                    }
                    k += sf;
                }
            }
        }
    }
    out.into_iter()
        .map(|(vertex, height_mm)| VertexHeightMm { vertex, height_mm })
        .collect()
}

/// The sim-ground bytes of a canonical section (spec 121 § 11), as the run time holds them: 2 bytes
/// per distinct lattice vertex (each tile owns its vertices but its east and north borders, plus
/// the map's own east and north edges at full pitch; the cooked count blocks repeat the borders,
/// which the run time does not), 8 per override and pin, the cut bitmap words, the canonical
/// triangles and one byte per tile level. A `Full` map holds `2 * n^2` bytes of counts.
#[must_use]
pub fn sim_ground_bytes(c: &WorldGeomCooked) -> u64 {
    let l = &c.lattice;
    let per_side = l.tiles_per_side();
    let mut vertices = 2 * u64::from(l.lattice_n) - 1;
    for (t, &level) in c.tile_levels.iter().enumerate() {
        let (a, b) = (t as u32 % per_side, t as u32 / per_side);
        let ((x0, x1), (z0, z1)) = (l.tile_cells(a), l.tile_cells(b));
        let s = 1u32 << level;
        vertices += u64::from((x1 - x0) / s) * u64::from((z1 - z0) / s);
    }
    let counts = vertices * 2;
    let overrides = (c.hanging_overrides.len() + c.ring_pins.len()) as u64 * 8;
    let cuts: u64 = c.cut_bitmaps.iter().map(|b| b.bits.len() as u64 * 8 + 4).sum();
    let tris: u64 = c.tiles.iter().map(|t| t.triangles.len() as u64).sum::<u64>()
        * std::mem::size_of::<crate::cooked::CookedTriangle>() as u64;
    counts + overrides + cuts + tris + c.tile_levels.len() as u64
}

/// Why the `ground_pitch_seam` check failed.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum PitchSeamError {
    /// Rule 1: a tile is coarser than its error allows.
    #[error(
        "ground tile {tile} at level {level} is {error_m} m off the full-pitch ground (limit {limit_m} m)"
    )]
    Error {
        /// The tile.
        tile: u32,
        /// Its level.
        level: u8,
        /// Measured, metres.
        error_m: f64,
        /// `pitch_error_m`.
        limit_m: f32,
    },
    /// Rule 3: two neighbors more than one level apart.
    #[error("ground tiles {a} and {b} differ by more than one level")]
    LevelJump {
        /// One tile.
        a: u32,
        /// Its neighbor.
        b: u32,
    },
    /// Rule 4: the ground steps across a pitch border.
    #[error("the ground steps {step_m} m across the border of ground tiles {a} and {b} (limit 0.001 m)")]
    Step {
        /// One tile.
        a: u32,
        /// Its neighbor.
        b: u32,
        /// Measured, metres.
        step_m: f32,
    },
    /// Rule 5: a pitch border within `fine_band_m` of a mouth or a layer.
    #[error("the pitch border of ground tiles {a} and {b} is within fine_band_m of a mouth or a layer")]
    InBand {
        /// One tile.
        a: u32,
        /// Its neighbor.
        b: u32,
    },
}

/// What [`check_ground_pitch_seam`] measured.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PitchSeamReport {
    /// Pitch borders checked.
    pub borders: u32,
    /// Hanging vertices.
    pub hanging: u32,
    /// The largest step across a pitch border, metres.
    pub max_step_m: f32,
    /// The largest tile error against the full-pitch ground, metres (overrides included).
    pub max_error_m: f64,
}

/// The cook check `ground_pitch_seam` (spec 121 § 10; E1 rules 1, 3, 4 and 5).
///
/// # Errors
///
/// The first rule a tile or border breaks.
pub fn check_ground_pitch_seam(
    c: &WorldGeomCooked,
    counts: &[u16],
    inputs: &PitchInputs,
) -> Result<PitchSeamReport, PitchSeamError> {
    let lattice = &c.lattice;
    let ground = GroundLattice::new(c);
    let per_side = lattice.tiles_per_side();
    let band_mm = i64::from(m_to_mm(inputs.fine_band_m));
    let mut report = PitchSeamReport {
        hanging: c.hanging_overrides.len() as u32,
        ..PitchSeamReport::default()
    };
    // Rule 1, on the cooked surface (the overrides included): every full-pitch vertex of a nested
    // tile within pitch_error_m (+ 1 mm for the overrides' rounding).
    let limit = f64::from(inputs.pitch_error_m) + f64::from(PITCH_SEAM_STEP_MAX_M);
    for b in 0..per_side {
        for a in 0..per_side {
            let t = b * per_side + a;
            let level = c.tile_levels[t as usize];
            if level == 0 {
                continue;
            }
            let (x0, x1) = lattice.tile_cells(a);
            let (z0, z1) = lattice.tile_cells(b);
            for j in z0..=z1 {
                for i in x0..=x1 {
                    let (x, z) = (vertex_m(lattice, i), vertex_m(lattice, j));
                    let Some(y) = ground.height_in_tile(t, x, z) else {
                        continue;
                    };
                    let e = (f64::from(y) - f64::from(count_height(lattice, counts, i, j))).abs();
                    report.max_error_m = regia_detsim::float::det_max_f64(report.max_error_m, e);
                    if e > limit {
                        return Err(PitchSeamError::Error {
                            tile: t,
                            level,
                            error_m: e,
                            limit_m: inputs.pitch_error_m,
                        });
                    }
                }
            }
        }
    }
    for (p, q, edge) in borders(lattice) {
        let (lp, lq) = (c.tile_levels[p as usize], c.tile_levels[q as usize]);
        if lp == lq {
            continue;
        }
        report.borders += 1;
        if lp.abs_diff(lq) > 1 {
            return Err(PitchSeamError::LevelJump { a: p, b: q });
        }
        if inputs.footprints.iter().any(|f| near(&edge, f, band_mm)) {
            return Err(PitchSeamError::InBand { a: p, b: q });
        }
        // Rule 4: both sides' ground at every full-pitch vertex of the border and at every midpoint.
        let east = q == p + 1;
        let (fixed, run) = if east {
            let (a, b) = (p % per_side, p / per_side);
            (lattice.tile_cells(a).1, lattice.tile_cells(b))
        } else {
            let (a, b) = (p % per_side, p / per_side);
            (lattice.tile_cells(b).1, lattice.tile_cells(a))
        };
        for k in run.0..run.1 {
            let a = vertex_m(lattice, k);
            let b = vertex_m(lattice, k + 1);
            for along in [a, a + (b - a) * 0.5] {
                let f = vertex_m(lattice, fixed);
                let (x, z) = if east { (f, along) } else { (along, f) };
                let (Some(yp), Some(yq)) = (ground.height_in_tile(p, x, z), ground.height_in_tile(q, x, z))
                else {
                    continue;
                };
                let step = (yp - yq).abs();
                report.max_step_m = regia_detsim::float::det_max(report.max_step_m, step);
                if step > PITCH_SEAM_STEP_MAX_M {
                    return Err(PitchSeamError::Step {
                        a: p,
                        b: q,
                        step_m: step,
                    });
                }
            }
        }
    }
    Ok(report)
}
