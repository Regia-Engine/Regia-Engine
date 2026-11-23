// SPDX-License-Identifier: MPL-2.0
//! Conservative ground height pyramid, built at load (spec 121 §5.5, ADR-8246).
use crate::cooked::{TileFold, WorldGeomCooked};
use crate::ground::GroundLattice;
use crate::units::mm_to_m;
use regia_detsim::float::{det_max, det_min};
#[derive(Clone, Debug)]
pub(crate) struct HeightLevel {
    pub(crate) side: u32,
    pub(crate) ranges: Vec<[f32; 2]>,
}
#[derive(Clone, Debug)]
pub(crate) struct HeightPyramid {
    pub(crate) levels: Vec<HeightLevel>,
}
impl HeightPyramid {
    /// Builds the pyramid of `c`. `folds` are the decoder's folds of `c`'s count blocks, one per
    /// ground tile, when it made them (ADR-8816); without them the blocks are read here.
    pub(crate) fn build(c: &WorldGeomCooked, folds: Option<&[Option<TileFold>]>) -> Self {
        let l = &c.lattice;
        let side = l.tiles_per_side();
        let lattice = GroundLattice::new(c);
        let folds = folds.filter(|f| f.len() == c.ground_counts.len());
        let mut ranges: Vec<_> = (0..c.ground_counts.len())
            .map(|tile| {
                tile_range_owned(c, tile, folds).unwrap_or_else(|| tile_range_walk(c, &lattice, tile))
            })
            .collect();
        // Include overrides in each touching tile. Keeping the overridden count in the interval is
        // deliberately conservative: it can only prevent a skip, never hide a hit.
        let g = u32::from(l.ground_tile_cells);
        for v in c.hanging_overrides.iter().chain(&c.ring_pins) {
            let i = v.vertex % l.lattice_n;
            let j = v.vertex / l.lattice_n;
            let xs = [i.saturating_sub(1) / g, (i / g).min(side - 1)];
            let zs = [j.saturating_sub(1) / g, (j / g).min(side - 1)];
            let y = mm_to_m(0, v.height_mm);
            for z in zs {
                for x in xs {
                    let b = &mut ranges[(z * side + x) as usize];
                    b[0] = det_min(b[0], y);
                    b[1] = det_max(b[1], y);
                }
            }
        }
        let mut levels = vec![HeightLevel { side, ranges }];
        while levels.last().is_some_and(|l| l.side > 1) {
            let last = &levels[levels.len() - 1];
            let side = last.side.div_ceil(2);
            let mut ranges = Vec::with_capacity((side * side) as usize);
            for z in 0..side {
                for x in 0..side {
                    let mut b = [f32::INFINITY, f32::NEG_INFINITY];
                    for dz in 0..2 {
                        for dx in 0..2 {
                            let (cx, cz) = (x * 2 + dx, z * 2 + dz);
                            if cx < last.side && cz < last.side {
                                let r = last.ranges[(cz * last.side + cx) as usize];
                                b[0] = det_min(b[0], r[0]);
                                b[1] = det_max(b[1], r[1]);
                            }
                        }
                    }
                    ranges.push(b);
                }
            }
            levels.push(HeightLevel { side, ranges });
        }
        Self { levels }
    }
    pub(crate) fn resident_bytes(&self) -> usize {
        self.levels
            .iter()
            .map(|l| l.ranges.capacity() * std::mem::size_of::<[f32; 2]>())
            .sum::<usize>()
            + self.levels.capacity() * std::mem::size_of::<HeightLevel>()
    }
}

/// The height range of ground tile `tile`, its east and north borders included, read vertex by
/// vertex through the lattice: any block layout, any mix of tile levels.
fn tile_range_walk(c: &WorldGeomCooked, lattice: &GroundLattice<'_>, tile: usize) -> [f32; 2] {
    let l = &c.lattice;
    let side = l.tiles_per_side();
    let block = &c.ground_counts[tile];
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    let fold = |lo: &mut f32, hi: &mut f32, count: u16| {
        let y = l.offset_m() + count as f32 * l.step_m();
        *lo = det_min(*lo, y);
        *hi = det_max(*hi, y);
    };
    // `y(count) = offset + count * step` rounds monotonically in `count` for a positive
    // step, so the block's lowest and highest heights are those of its lowest and
    // highest counts: fold the integers and convert two (the same bits as converting
    // every count; ADR-8662, load time).
    if l.step_m() > 0.0 {
        let mut counts = block.iter().copied();
        if let Some(first) = counts.next() {
            let (cmin, cmax) = counts.fold((first, first), |(a, b), v| (a.min(v), b.max(v)));
            fold(&mut lo, &mut hi, cmin);
            fold(&mut lo, &mut hi, cmax);
        }
    } else {
        for &count in block {
            fold(&mut lo, &mut hi, count);
        }
    }
    // Owned blocks omit the east and north borders. Those heights still bound the tile.
    let level = c.tile_levels.get(tile).copied().unwrap_or(0);
    let (a, b) = ((tile as u32) % side, (tile as u32) / side);
    let (x0, x1) = l.tile_cells(a);
    let (z0, z1) = l.tile_cells(b);
    let step = 1u32 << u32::from(level);
    if step > 0 {
        let mut v = z0;
        while v <= z1 {
            if let Some(count) = lattice.count_in_tile(tile as u32, x1, v) {
                fold(&mut lo, &mut hi, count);
            }
            v = v.saturating_add(step);
        }
        let mut u = x0;
        while u <= x1 {
            if let Some(count) = lattice.count_in_tile(tile as u32, u, z1) {
                fold(&mut lo, &mut hi, count);
            }
            u = u.saturating_add(step);
        }
    }
    [lo, hi]
}

/// The owned shape `(columns, rows)` of ground tile `(a, b)` at `level`: its vertices without the
/// east and north borders, which the next tile stores (the map's own edge stays; ADR-8246 R7).
fn owned_shape(c: &WorldGeomCooked, a: u32, b: u32, level: u8) -> Option<(u32, u32)> {
    let l = &c.lattice;
    let side = l.tiles_per_side();
    let (sx, sz) = (l.tile_side_vertices(a, level)?, l.tile_side_vertices(b, level)?);
    let cols = if a + 1 == side { sx } else { sx - 1 };
    let rows = if b + 1 == side { sz } else { sz - 1 };
    Some((cols, rows))
}

/// [`tile_range_walk`] for the layout a loaded map has, without a lookup per border vertex
/// (ADR-8816; the walk was most of the load's time after the decode): the tile and the neighbors
/// that store its borders are at one level and hold owned blocks, and the count step is positive.
/// The border vertices are then the east neighbor's first column, the north neighbor's first row
/// and the first count of the tile across the corner. Their lowest and highest counts come from
/// the decoder's folds (`folds`, made while it copied each block) or, without them, from the
/// blocks. The range is that of the same counts as the walk folds, so the same bits (`y(count)` is
/// monotone in `count` for a positive step and never `-0.0`). `None` when the layout is any other;
/// the caller walks.
fn tile_range_owned(
    c: &WorldGeomCooked,
    tile: usize,
    folds: Option<&[Option<TileFold>]>,
) -> Option<[f32; 2]> {
    let l = &c.lattice;
    if l.step_m().partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        return None;
    }
    let side = l.tiles_per_side();
    let level = *c.tile_levels.get(tile)?;
    let (a, b) = ((tile as u32) % side, (tile as u32) / side);
    // The fold of tile `(a, b)` when it is at this tile's level and holds an owned block.
    let fold_of = |a: u32, b: u32| -> Option<TileFold> {
        let t = (b * side + a) as usize;
        if *c.tile_levels.get(t)? != level {
            return None;
        }
        let (cols, rows) = owned_shape(c, a, b, level)?;
        let block = c.ground_counts.get(t)?;
        if block.len() != cols as usize * rows as usize {
            return None;
        }
        match folds {
            Some(folds) => folds
                .get(t)
                .copied()
                .flatten()
                .filter(|f| f.shape == (cols, rows)),
            None => TileFold::of(block, cols, rows),
        }
    };
    let own = fold_of(a, b)?;
    let (east, north) = (a + 1 < side, b + 1 < side);
    let mut range = own.all;
    let mut include = |part: (u16, u16)| range = (range.0.min(part.0), range.1.max(part.1));
    if east {
        // The east border: column 0 of the next tile, every row it owns.
        include(fold_of(a + 1, b)?.west);
    }
    if north {
        // The north border: row 0 of the tile above, every column it owns (as many as this tile).
        let above = fold_of(a, b + 1)?;
        if above.shape.0 != own.shape.0 {
            return None;
        }
        include(above.south);
    }
    if east && north {
        // The corner both borders end at: the first count of the tile across it.
        let first = fold_of(a + 1, b + 1)?.first;
        include((first, first));
    }
    let y = |count: u16| l.offset_m() + count as f32 * l.step_m();
    Some([y(range.0), y(range.1)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cook::{CookInputs, GroundInput, cook};

    /// A ground-only cook loaded the way the run time loads it (owned count blocks).
    fn loaded(n: u32, document: &str, count: impl Fn(u32, u32) -> u16) -> WorldGeomCooked {
        let ground = GroundInput {
            lattice_n: n,
            width_m: (n - 1) as f32,
            offset_m: -40.0,
            step_m: 1.0 / 32.0,
            counts: (0..n * n).map(|k| count(k % n, k / n)).collect(),
        };
        let mut cooked = match cook(&CookInputs {
            document: document.as_bytes(),
            ground: &ground,
            terrain: None,
            read_file: &|_| Err("no source".into()),
        }) {
            Ok(out) => out.file.canonical,
            Err(e) => panic!("cook: {e}"),
        };
        crate::ground::compact_border_counts(&mut cooked);
        cooked
    }

    fn bits(r: [f32; 2]) -> [u32; 2] {
        r.map(f32::to_bits)
    }

    /// ADR-8816: the direct border fold gives the walk's bits on every tile it takes, and it takes
    /// every tile of a loaded one-level map (edge tiles, a partial last tile, one tile alone).
    #[test]
    fn owned_tile_ranges_equal_the_vertex_walk() {
        // Heights that differ at every vertex, so a missed or extra border vertex moves a range.
        let rough = |i: u32, j: u32| ((i * 7919 + j * 104_729 + i * j * 31) % 60_000) as u16;
        let full = "(version:1, ground_tile_cells:16, layers:[])";
        for n in [17u32, 33, 41, 65, 97] {
            let c = loaded(n, full, rough);
            let lattice = GroundLattice::new(&c);
            for tile in 0..c.ground_counts.len() {
                let fast = tile_range_owned(&c, tile, None).map(bits);
                assert_eq!(
                    fast,
                    Some(bits(tile_range_walk(&c, &lattice, tile))),
                    "n {n} tile {tile}"
                );
            }
        }
    }

    /// ADR-8816: the pyramid built from the decoder's folds (made while it copied each block) is,
    /// bit for bit, the pyramid built by reading the blocks, on one-level and mixed-level grounds;
    /// each fold is the fold of its block; a v0.6.0 section (no folds) builds the same pyramid.
    #[test]
    fn decoder_folds_build_the_same_pyramid() {
        use crate::cooked::{CountBodyCheck, decode_section, encode_canonical};
        let rough = |i: u32, j: u32| ((i * 7919 + j * 104_729 + i * j * 31) % 60_000) as u16;
        let patch = |i: u32, j: u32| {
            if i < 40 && j < 40 {
                rough(i, j) % 4_000 + 20_000
            } else {
                20_000
            }
        };
        let level_bits = |p: &HeightPyramid| -> Vec<(u32, Vec<[u32; 2]>)> {
            p.levels
                .iter()
                .map(|l| (l.side, l.ranges.iter().map(|r| bits(*r)).collect()))
                .collect()
        };
        let full = "(version:1, ground_tile_cells:16, layers:[])";
        let nested = "(version:1, ground_tile_cells:16, ground: Nested(max_level: 3), layers:[])";
        for (n, document) in [(17u32, full), (41, full), (97, full), (129, nested)] {
            let compacted = if document == nested {
                loaded(n, document, patch)
            } else {
                loaded(n, document, rough)
            };
            // The file's bytes (borders stored) of the same cook, decoded the way the load does.
            let ground = GroundInput {
                lattice_n: n,
                width_m: (n - 1) as f32,
                offset_m: -40.0,
                step_m: 1.0 / 32.0,
                counts: (0..n * n)
                    .map(|k| {
                        if document == nested {
                            patch(k % n, k / n)
                        } else {
                            rough(k % n, k / n)
                        }
                    })
                    .collect(),
            };
            let file = match cook(&CookInputs {
                document: document.as_bytes(),
                ground: &ground,
                terrain: None,
                read_file: &|_| Err("no source".into()),
            }) {
                Ok(out) => out.file.canonical,
                Err(e) => panic!("cook: {e}"),
            };
            let bytes = match encode_canonical(&file) {
                Ok(b) => b,
                Err(e) => panic!("encode: {e}"),
            };
            let (c, _, folds) = match decode_section(&bytes, true, CountBodyCheck::AtLoad) {
                Ok(d) => d,
                Err(e) => panic!("decode: {e}"),
            };
            assert_eq!(
                c, compacted,
                "n {n}: the one-pass decode is the compacted section"
            );
            let folds = folds.unwrap_or_default();
            assert_eq!(folds.len(), c.ground_counts.len());
            for (t, fold) in folds.iter().enumerate() {
                let fold = fold.unwrap_or_else(|| panic!("n {n} tile {t}: no fold"));
                assert_eq!(
                    TileFold::of(&c.ground_counts[t], fold.shape.0, fold.shape.1),
                    Some(fold),
                    "n {n} tile {t}"
                );
            }
            let with_folds = HeightPyramid::build(&c, Some(&folds));
            assert_eq!(
                level_bits(&with_folds),
                level_bits(&HeightPyramid::build(&c, None)),
                "n {n}"
            );
            // Folds of another map's shape are not used.
            assert_eq!(
                level_bits(&HeightPyramid::build(&c, Some(&folds[1..]))),
                level_bits(&with_folds),
                "n {n}"
            );
        }
    }

    /// The same on a `Nested` ground with mixed tile levels: a tile whose neighbors are at
    /// another level is left to the walk, and every tile the direct fold takes equals it.
    #[test]
    fn mixed_level_tiles_fall_back_to_the_walk() {
        // Flat except one rough corner, so the cook picks coarse levels elsewhere.
        let patch = |i: u32, j: u32| {
            if i < 40 && j < 40 {
                (20_000 + (i * 7919 + j * 104_729) % 4_000) as u16
            } else {
                20_000
            }
        };
        let c = loaded(
            129,
            "(version:1, ground_tile_cells:16, ground: Nested(max_level: 3), layers:[])",
            patch,
        );
        let levels: std::collections::BTreeSet<u8> = c.tile_levels.iter().copied().collect();
        assert!(levels.len() > 1, "the fixture has one level only: {levels:?}");
        let lattice = GroundLattice::new(&c);
        let (mut taken, mut walked) = (0, 0);
        for tile in 0..c.ground_counts.len() {
            match tile_range_owned(&c, tile, None) {
                Some(fast) => {
                    taken += 1;
                    assert_eq!(
                        bits(fast),
                        bits(tile_range_walk(&c, &lattice, tile)),
                        "tile {tile}"
                    );
                }
                None => walked += 1,
            }
        }
        assert!(taken > 0 && walked > 0, "taken {taken} walked {walked}");
        // A block still holding its borders (a section that was not compacted) is walked.
        let full = match cook(&CookInputs {
            document: b"(version:1, ground_tile_cells:16, layers:[])",
            ground: &GroundInput {
                lattice_n: 33,
                width_m: 32.0,
                offset_m: 0.0,
                step_m: 1.0 / 32.0,
                counts: vec![7; 33 * 33],
            },
            terrain: None,
            read_file: &|_| Err("no source".into()),
        }) {
            Ok(out) => out.file.canonical,
            Err(e) => panic!("cook: {e}"),
        };
        assert_eq!(tile_range_owned(&full, 0, None), None);
    }
}
