// SPDX-License-Identifier: MPL-2.0
//! Load-time acceleration for `segment`, `line_of_sight`, `rock_between` and `push_disc`
//! (plan row W-11, ADR-8815). Original design from spec 121 § 5.4 and § 5.5.
//!
//! Three tables, none of which an answer depends on (they only choose which exact tests run):
//!
//! - **Boxes.** Every canonical triangle's bounds, quantized outward to 16 bits an axis
//!   ([`Quant`]): 12 bytes a triangle, in cooked tile order. A candidate is rejected on them before
//!   its cooked millimetre triangle is decoded. A quantized box contains the triangle, so a test
//!   against it can only keep a candidate the exact test would drop, never the reverse.
//! - **One triangle grid for segments.** Every triangle of every kind filed by its XZ bounds plus
//!   [`SEG_PAD_M`], with a dense cell directory; a cell's list is three parts (walls and plug
//!   faces, floors, roofs), each with its own height range. A segment walks it once. The column and
//!   wall grids of the query index stay as they are for the column queries; the wall grid's 8 m
//!   pad, which the disc push needed and a segment never did, is not paid per sight line any more.
//! - **Ground.** The lattice axis positions, and one bit per ground tile that says the tile reads
//!   its heights from its counts alone (full pitch, no ring pin, no hanging vertex, no mouth cut), so
//!   a segment can test a cell's four counts against its own height before any triangle is built.

use crate::cooked::{GeomLoadError, TriKind, WorldGeomCooked};
use crate::geom::GeomTriangle;
use crate::ground::vertex_m;
use regia_detsim::float::{det_max, det_min};

/// Pad of the segment grid, metres: a triangle is filed in every cell within this distance of its
/// XZ bounds, so a hit point a few ulps outside its own cell is still listed in the cell the walk
/// is in. The column grid's pad (ADR-8246).
pub(crate) const SEG_PAD_M: f32 = 0.25;

/// One triangle's bounds in quanta: `[min x, max x, min y, max y, min z, max z]` ([`Quant`]).
pub(crate) type QBox = [u16; 6];

/// A box or point in quanta, wide enough for values off the table: `[min, max]` per axis x, y, z.
pub(crate) type QWindow = [[i32; 3]; 2];

/// The quantization of the box table: a coordinate of `mm` world millimetres on `axis` is quantum
/// `floor((mm - origin[axis]) / quantum)`. The quantum is the smallest power of two (millimetres)
/// that fits the triangles' largest extent in 16 bits: 32 mm on a 1.28 km map.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Quant {
    origin_mm: [i64; 3],
    shift: u32,
    /// Quanta per metre.
    scale: f64,
    /// The origin in quanta, per axis.
    offset: [f64; 3],
}

impl Quant {
    fn new(origin_mm: [i64; 3], shift: u32) -> Self {
        let quantum = f64::from(1u32 << shift);
        Self {
            origin_mm,
            shift,
            scale: 1000.0 / quantum,
            offset: origin_mm.map(|mm| mm as f64 / quantum),
        }
    }
    /// The quantum holding world millimetre `mm` on `axis`.
    #[inline]
    fn of_mm(&self, axis: usize, mm: i64) -> i64 {
        (mm - self.origin_mm[axis]) >> self.shift
    }
    /// The quantum of the `f32` metre coordinate `m` on `axis`, or the one above it: the
    /// float-to-int cast of `m * 1000 / quantum - origin / quantum` in `f64` (exact to far below a
    /// quantum for any map coordinate), which truncates toward zero (no call into a rounding
    /// routine on the walk). The cast saturates, so a huge or infinite coordinate stays in range
    /// and NaN maps to 0 (a NaN window was refused before the walk).
    #[inline]
    fn of_m(&self, axis: usize, m: f32) -> i32 {
        let quantum = (f64::from(m) * self.scale - self.offset[axis]) as i32;
        // Kept well inside the integer range, so the callers' margins cannot overflow.
        quantum.clamp(-(1 << 30), 1 << 30)
    }
    /// A lower bound, in quanta, of the `f32` metre coordinate `m` on `axis`: three quanta under
    /// the truncated quantum (one for the truncation, two for the rounding between a vertex's
    /// millimetres and its `f32` metres and of the conversion).
    #[inline]
    pub(crate) fn below(&self, axis: usize, m: f32) -> i32 {
        self.of_m(axis, m) - 3
    }
    /// The matching upper bound: two quanta over.
    #[inline]
    pub(crate) fn above(&self, axis: usize, m: f32) -> i32 {
        self.of_m(axis, m) + 2
    }
    /// The quanta `[min, max]` per axis that cover the metre box `[min, max]` per axis.
    #[inline]
    pub(crate) fn window(&self, lo: [f32; 3], hi: [f32; 3]) -> QWindow {
        [
            [0, 1, 2].map(|k| self.below(k, lo[k])),
            [0, 1, 2].map(|k| self.above(k, hi[k])),
        ]
    }
    /// The quantum, metres.
    #[inline]
    pub(crate) fn quantum_m(&self) -> f32 {
        (1u32 << self.shift) as f32 / 1000.0
    }
    /// A length in metres as a count of quanta, rounded up, plus one.
    #[inline]
    pub(crate) fn span(&self, m: f32) -> i32 {
        let quanta = (f64::from(m) * self.scale) as i32;
        quanta.clamp(0, 1 << 30) + 2
    }
}

/// Whether box `b` misses window `w` on any axis. All six comparisons, no short circuit: one
/// predictable branch a candidate.
#[inline]
pub(crate) fn misses(b: &QBox, w: &QWindow) -> bool {
    (i32::from(b[0]) > w[1][0])
        | (i32::from(b[1]) < w[0][0])
        | (i32::from(b[2]) > w[1][1])
        | (i32::from(b[3]) < w[0][1])
        | (i32::from(b[4]) > w[1][2])
        | (i32::from(b[5]) < w[0][2])
}

/// The parts of a cell's list: walls and plug faces, floors, roofs.
pub(crate) const PARTS: usize = 3;

/// The part a triangle of kind `kind` is filed in.
fn part_of(kind: TriKind) -> usize {
    match kind {
        TriKind::Wall | TriKind::Blocker => 0,
        TriKind::Floor => 1,
        TriKind::Roof => 2,
    }
}

/// The low 24 bits of an entry are the triangle's slot; the flags above them describe it.
const SLOT_MASK: u32 = (1 << 24) - 1;
/// Entry flag: a plug face (part 0 holds walls and plug faces).
const FLAG_BLOCKER: u32 = 1 << 24;
/// Entry flag: `push_one` never applies this wall (`near_factors` gave `-inf`).
const FLAG_NEVER: u32 = 1 << 25;
/// Entry flag: a wall whose reach factors are in the skew table (not plumb).
const FLAG_SKEW: u32 = 1 << 26;

/// The reach factors `[k1, k2]` used for every plumb wall: the largest a wall may have and still
/// be filed as plumb (`query_walls::near_factors` gives 1.001 and about 0 for a vertical wall).
pub(crate) const PLUMB_REACH: [f32; 2] = [1.002, 0.001];

/// One entry of a cell's list.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Entry(u32);

impl Entry {
    /// The triangle's slot in the box table.
    #[inline]
    pub(crate) fn slot(self) -> u32 {
        self.0 & SLOT_MASK
    }
    /// A plug face (only in the wall part).
    #[inline]
    pub(crate) fn is_blocker(self) -> bool {
        self.0 & FLAG_BLOCKER != 0
    }
    /// A wall `push_one` never applies.
    #[inline]
    pub(crate) fn never_pushes(self) -> bool {
        self.0 & FLAG_NEVER != 0
    }
}

/// One non-empty cell of the segment grid: three parts, each a run of `entries` with the lowest and
/// highest quantum of its triangles' heights.
#[derive(Clone, Copy, Debug)]
struct SegCell {
    /// Part `p` is `entries[bounds[p]..bounds[p + 1]]`.
    bounds: [u32; PARTS + 1],
    heights: [[u16; 2]; PARTS],
}

/// The dense cell directory: an index into the cells per grid slot, 16 bits wide while it fits.
#[derive(Clone, Debug)]
enum Directory {
    Small(Vec<u16>),
    Large(Vec<u32>),
}

impl Directory {
    #[inline]
    fn get(&self, slot: usize) -> Option<usize> {
        match self {
            Self::Small(v) => v.get(slot).copied().filter(|&c| c != u16::MAX).map(usize::from),
            Self::Large(v) => v
                .get(slot)
                .copied()
                .filter(|&c| c != u32::MAX)
                .map(|c| c as usize),
        }
    }
    fn resident_bytes(&self) -> usize {
        match self {
            Self::Small(v) => v.capacity() * 2,
            Self::Large(v) => v.capacity() * 4,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SegIndex {
    quant: Quant,
    /// Quantized bounds per slot (cooked tile order: `tile_start[tile] + local`).
    boxes: Vec<QBox>,
    /// The first slot of every cooked tile, and the slot count last.
    tile_start: Vec<u32>,
    /// Cell size, metres (`GeomSettings::index_cell_m`).
    pub(crate) cell: f32,
    /// Inclusive cell bounds `[[min x, min z], [max x, max z]]`; `None` without triangles.
    pub(crate) bounds: Option<[[i32; 2]; 2]>,
    /// Dense directory over `bounds`, row-major in z.
    directory: Directory,
    cells: Vec<SegCell>,
    /// Cell after cell: a cell's walls and plug faces, then its floors, then its roofs.
    entries: Vec<Entry>,
    /// `(slot, [k1, k2])` of every wall that is neither plumb nor never applied, ascending slot.
    skew: Vec<(u32, [f32; 2])>,
    /// The largest reach factors `[k1, k2]` of any wall or plug face that can push
    /// (`query_walls::near_factors`); infinite when one is.
    wall_reach: [f32; 2],
    /// `vertex_m(lattice, i)` for every lattice vertex index along one axis.
    axis: Vec<f32>,
    /// The lattice pitch, metres (the width over the cell count): a cell estimate only.
    pitch_m: f32,
    /// One bit per ground tile: see [`SegIndex::tile_is_plain`].
    plain: Vec<u64>,
    /// Every roof triangle is within 60 degrees of level, so the height at which one crosses a
    /// vertical line stays inside its vertices' height range to well under a centimetre of
    /// rounding (`WorldGeom::clear_of_lids`).
    gentle_roofs: bool,
}

impl SegIndex {
    /// Builds the tables. `tris[i]` has packed id `ids[i]` (`tile << 16 | local`).
    ///
    /// # Errors
    ///
    /// `GeomLoadError::Corrupt` when an id names no cooked triangle, or a cell coordinate or the
    /// reference count leaves its bound (the query index's own limits).
    pub(crate) fn build(
        tris: &[GeomTriangle],
        ids: &[u32],
        cooked: &WorldGeomCooked,
    ) -> Result<Self, GeomLoadError> {
        let n = tris.len();
        let corrupt = |what: &str| GeomLoadError::Corrupt(format!("segment index: {what}"));
        if n > SLOT_MASK as usize {
            return Err(corrupt("more than 16 million triangles"));
        }
        let mut tile_start = Vec::with_capacity(cooked.tiles.len() + 1);
        let mut total = 0u32;
        for tile in &cooked.tiles {
            tile_start.push(total);
            total += tile.triangles.len() as u32;
        }
        tile_start.push(total);
        if total as usize != n {
            return Err(corrupt("the id list does not cover the cooked triangles"));
        }
        // The world millimetre box of every triangle, by slot, and the quantization that fits.
        let mut mm_boxes = vec![[0i64; 6]; n];
        let mut kinds = vec![TriKind::Floor; n];
        let mut seen = vec![false; n];
        for (tri, &id) in tris.iter().zip(ids) {
            let (tile_i, local) = ((id >> 16) as usize, (id & 0xffff) as usize);
            let tile = cooked
                .tiles
                .get(tile_i)
                .ok_or_else(|| corrupt("an id names no tile"))?;
            let cooked_tri = tile
                .triangles
                .get(local)
                .ok_or_else(|| corrupt("an id names no triangle"))?;
            let slot = tile_start[tile_i] as usize + local;
            if std::mem::replace(&mut seen[slot], true) {
                return Err(corrupt("an id is listed twice"));
            }
            let origin = [i64::from(tile.origin_mm[0]), 0, i64::from(tile.origin_mm[1])];
            for k in 0..3 {
                let c = [0, 1, 2].map(|v| origin[k] + i64::from(cooked_tri.v[v][k]));
                mm_boxes[slot][2 * k] = c[0].min(c[1]).min(c[2]);
                mm_boxes[slot][2 * k + 1] = c[0].max(c[1]).max(c[2]);
            }
            kinds[slot] = tri.kind;
        }
        let mut origin_mm = [0i64; 3];
        let mut extent = 0i64;
        for k in 0..3 {
            let lo = mm_boxes.iter().map(|b| b[2 * k]).min().unwrap_or(0);
            let hi = mm_boxes.iter().map(|b| b[2 * k + 1]).max().unwrap_or(0);
            origin_mm[k] = lo;
            extent = extent.max(hi - lo);
        }
        let mut shift = 0u32;
        while (extent >> shift) > i64::from(u16::MAX) {
            shift += 1;
        }
        let quant = Quant::new(origin_mm, shift);
        let boxes: Vec<QBox> = mm_boxes
            .iter()
            .map(|b| std::array::from_fn(|i| quant.of_mm(i / 2, b[i]) as u16))
            .collect();
        // Reach factors of the walls and plug faces, by slot.
        let mut flags = vec![0u32; n];
        let mut skew = Vec::new();
        let mut wall_reach = [0.0f32; 2];
        for (tri, &id) in tris.iter().zip(ids) {
            if part_of(tri.kind) != 0 {
                continue;
            }
            let slot = tile_start[(id >> 16) as usize] + (id & 0xffff);
            let mut flag = if tri.kind == TriKind::Blocker {
                FLAG_BLOCKER
            } else {
                0
            };
            let [k1, k2] = crate::query_walls::near_factors(tri.v);
            if k1 == f32::NEG_INFINITY {
                flag |= FLAG_NEVER;
            } else if k1 <= PLUMB_REACH[0] && k2 <= PLUMB_REACH[1] {
                wall_reach = [
                    det_max(wall_reach[0], PLUMB_REACH[0]),
                    det_max(wall_reach[1], PLUMB_REACH[1]),
                ];
            } else {
                flag |= FLAG_SKEW;
                skew.push((slot, [k1, k2]));
                wall_reach = [det_max(wall_reach[0], k1), det_max(wall_reach[1], k2)];
            }
            flags[slot as usize] = flag;
        }
        skew.sort_unstable_by_key(|s| s.0);
        skew.shrink_to_fit();
        // A roof is gentle when its unnormalized normal `n` has `n.y^2 >= n^2 / 4`.
        let gentle_roofs = tris.iter().filter(|t| t.kind == TriKind::Roof).all(|t| {
            let [a, b, c] = t.v.map(|p| p.map(f64::from));
            let (u, v) = (
                [b[0] - a[0], b[1] - a[1], b[2] - a[2]],
                [c[0] - a[0], c[1] - a[1], c[2] - a[2]],
            );
            let n = [
                u[1] * v[2] - u[2] * v[1],
                u[2] * v[0] - u[0] * v[2],
                u[0] * v[1] - u[1] * v[0],
            ];
            4.0 * n[1] * n[1] >= n[0] * n[0] + n[1] * n[1] + n[2] * n[2]
        });
        // The cell range of every triangle, from the bounds of its `f32` vertices (what a segment
        // and the disc push see), then a counting sort into the cells.
        let cell = cooked.settings.index_cell_m;
        let mut ranges = vec![([0i32; 2], [0i32; 2]); n];
        let mut bounds: Option<[[i32; 2]; 2]> = None;
        let mut total = 0u64;
        for (tri, &id) in tris.iter().zip(ids) {
            let [a, b, c] = tri.v;
            let lo = |k: usize| det_min(a[k], det_min(b[k], c[k]));
            let hi = |k: usize| det_max(a[k], det_max(b[k], c[k]));
            let lo = [
                ((lo(0) - SEG_PAD_M) / cell).floor(),
                ((lo(2) - SEG_PAD_M) / cell).floor(),
            ];
            let hi = [
                ((hi(0) + SEG_PAD_M) / cell).floor(),
                ((hi(2) + SEG_PAD_M) / cell).floor(),
            ];
            if lo
                .iter()
                .chain(hi.iter())
                .any(|v| !v.is_finite() || v.abs() > 1_000_000.0)
            {
                return Err(corrupt("a cell coordinate is out of bounds"));
            }
            let (lo, hi) = (lo.map(|v| v as i32), hi.map(|v| v as i32));
            total += (i64::from(hi[0]) - i64::from(lo[0]) + 1) as u64
                * (i64::from(hi[1]) - i64::from(lo[1]) + 1) as u64;
            if total > 16_000_000 {
                return Err(corrupt("more than 16 million references"));
            }
            match &mut bounds {
                Some(all) => {
                    for axis in 0..2 {
                        all[0][axis] = all[0][axis].min(lo[axis]);
                        all[1][axis] = all[1][axis].max(hi[axis]);
                    }
                }
                None => bounds = Some([lo, hi]),
            }
            ranges[(tile_start[(id >> 16) as usize] + (id & 0xffff)) as usize] = (lo, hi);
        }
        let mut directory = Directory::Small(Vec::new());
        let (mut cells, mut entries) = (Vec::new(), Vec::new());
        if let Some(all) = bounds {
            let width = (i64::from(all[1][0]) - i64::from(all[0][0]) + 1) as u64;
            let depth = (i64::from(all[1][1]) - i64::from(all[0][1]) + 1) as u64;
            if width * depth > 16_000_000 {
                return Err(corrupt("the directory exceeds 16 million cells"));
            }
            let slot_of = |x: i32, z: i32| {
                ((i64::from(z) - i64::from(all[0][1])) as u64 * width
                    + (i64::from(x) - i64::from(all[0][0])) as u64) as usize
            };
            // Counts per part and directory slot.
            let mut counts = vec![[0u32; PARTS]; (width * depth) as usize];
            for (slot, (lo, hi)) in ranges.iter().enumerate() {
                for z in lo[1]..=hi[1] {
                    for x in lo[0]..=hi[0] {
                        counts[slot_of(x, z)][part_of(kinds[slot])] += 1;
                    }
                }
            }
            let mut wide = vec![u32::MAX; counts.len()];
            let mut at = 0u32;
            for (i, per_part) in counts.iter().enumerate() {
                if per_part.iter().sum::<u32>() > 0 {
                    wide[i] = cells.len() as u32;
                    // Until the fill below, `bounds[p + 1]` is the fill cursor of part `p`.
                    let mut bounds = [at; PARTS + 1];
                    for (p, count) in per_part.iter().enumerate() {
                        bounds[p + 1] = at;
                        at += count;
                    }
                    cells.push(SegCell {
                        bounds,
                        heights: [[u16::MAX, 0]; PARTS],
                    });
                }
            }
            entries = vec![Entry(0); at as usize];
            // A part's cursor ends at the next part's start, which is the bound it has to be.
            for (slot, (lo, hi)) in ranges.iter().enumerate() {
                let b = &boxes[slot];
                let part = part_of(kinds[slot]);
                for z in lo[1]..=hi[1] {
                    for x in lo[0]..=hi[0] {
                        let c = &mut cells[wide[slot_of(x, z)] as usize];
                        entries[c.bounds[part + 1] as usize] = Entry(slot as u32 | flags[slot]);
                        c.bounds[part + 1] += 1;
                        let h = &mut c.heights[part];
                        *h = [h[0].min(b[2]), h[1].max(b[3])];
                    }
                }
            }
            directory = if cells.len() < usize::from(u16::MAX) {
                Directory::Small(wide.iter().map(|&c| c as u16).collect())
            } else {
                Directory::Large(wide)
            };
        }
        let l = &cooked.lattice;
        let axis: Vec<f32> = (0..=l.cells()).map(|i| vertex_m(l, i)).collect();
        let lattice_cells = l.cells().max(1);
        Ok(Self {
            quant,
            boxes,
            tile_start,
            cell,
            bounds,
            directory,
            cells,
            entries,
            skew,
            wall_reach,
            axis,
            pitch_m: crate::mm_to_m(0, l.width_mm) / lattice_cells as f32,
            plain: plain_tiles(cooked),
            gentle_roofs,
        })
    }

    /// The quantization of the boxes.
    #[inline]
    pub(crate) fn quant(&self) -> &Quant {
        &self.quant
    }
    /// The quantized bounds of the triangle at `slot`.
    #[inline]
    pub(crate) fn tri_box(&self, slot: u32) -> &QBox {
        &self.boxes[slot as usize]
    }
    /// The packed id (`tile << 16 | local`) of the triangle at `slot`.
    #[inline]
    pub(crate) fn id(&self, slot: u32) -> u32 {
        // A handful of 1024 m tiles: the tile is the last one starting at or before the slot.
        let tile = self
            .tile_start
            .partition_point(|&start| start <= slot)
            .saturating_sub(1);
        ((tile as u32) << 16) | (slot - self.tile_start[tile])
    }
    /// The reach factors `[k1, k2]` of the wall or plug face of entry `e` when it is not plumb
    /// (`query_walls::near_factors`); `None` for a plumb wall, whose factors are [`PLUMB_REACH`].
    #[inline]
    pub(crate) fn skew_reach(&self, e: Entry) -> Option<[f32; 2]> {
        if e.0 & FLAG_SKEW == 0 {
            return None;
        }
        Some(
            self.skew
                .binary_search_by_key(&e.slot(), |s| s.0)
                .map_or([f32::INFINITY; 2], |i| self.skew[i].1),
        )
    }
    /// The walls and plug faces filed in cell `key`.
    #[inline]
    pub(crate) fn walls_at(&self, key: [i32; 2]) -> &[Entry] {
        self.cell(key).map_or(&[], |cell| {
            &self.entries[cell.bounds[0] as usize..cell.bounds[1] as usize]
        })
    }
    /// The parts of cell `key` (walls and plug faces, floors, roofs): each its entries and the
    /// lowest and highest quantum of their heights (`[max, 0]` for an empty part). `None` for an
    /// empty cell.
    #[inline]
    pub(crate) fn cell_parts(&self, key: [i32; 2]) -> Option<[(&[Entry], [u16; 2]); PARTS]> {
        let cell = self.cell(key)?;
        Some(std::array::from_fn(|p| {
            (
                &self.entries[cell.bounds[p] as usize..cell.bounds[p + 1] as usize],
                cell.heights[p],
            )
        }))
    }
    /// The largest reach factors `[k1, k2]` of the map's walls and plug faces.
    #[inline]
    pub(crate) fn wall_reach(&self) -> [f32; 2] {
        self.wall_reach
    }
    #[inline]
    fn cell(&self, key: [i32; 2]) -> Option<&SegCell> {
        let b = self.bounds?;
        if key[0] < b[0][0] || key[0] > b[1][0] || key[1] < b[0][1] || key[1] > b[1][1] {
            return None;
        }
        let width = (b[1][0] - b[0][0] + 1) as usize;
        let slot = (key[1] - b[0][1]) as usize * width + (key[0] - b[0][0]) as usize;
        self.cells.get(self.directory.get(slot)?)
    }
    /// `vertex_m(lattice, i)` from the table (the same bits), or `None` past the last vertex.
    #[inline]
    pub(crate) fn axis_m(&self, i: u32) -> Option<f32> {
        self.axis.get(i as usize).copied()
    }
    /// The lattice pitch, metres: for a cell estimate, corrected against the axis table.
    #[inline]
    pub(crate) fn pitch_m(&self) -> f32 {
        self.pitch_m
    }
    /// The whole axis table.
    #[inline]
    pub(crate) fn axis(&self) -> &[f32] {
        &self.axis
    }
    /// Whether ground tile `tile` reads every vertex of its cells as `offset + count * step` from
    /// compact full-pitch blocks: the tile and its east, north and north-east neighbors are at
    /// level 0 with owned-layout blocks, no ring pin or hanging vertex touches it, no mouth cut
    /// lies in it and the height step is positive. A D1 edit is the caller's to rule out.
    #[inline]
    pub(crate) fn tile_is_plain(&self, tile: u32) -> bool {
        self.plain
            .get((tile / 64) as usize)
            .is_some_and(|w| (w >> (tile % 64)) & 1 == 1)
    }
    /// Table sizes for diagnostics, bytes: boxes, directory, cells, entries, skew walls, axis,
    /// plain bits, tile starts.
    #[cfg(any(test, feature = "stats"))]
    pub(crate) fn part_bytes(&self) -> [usize; 8] {
        use std::mem::size_of;
        [
            self.boxes.capacity() * size_of::<QBox>(),
            self.directory.resident_bytes(),
            self.cells.capacity() * size_of::<SegCell>(),
            self.entries.capacity() * size_of::<Entry>(),
            self.skew.capacity() * size_of::<(u32, [f32; 2])>(),
            self.axis.capacity() * 4,
            self.plain.capacity() * 8,
            self.tile_start.capacity() * 4,
        ]
    }
    pub(crate) fn resident_bytes(&self) -> usize {
        use std::mem::size_of;
        self.boxes.capacity() * size_of::<QBox>()
            + self.tile_start.capacity() * 4
            + self.directory.resident_bytes()
            + self.cells.capacity() * size_of::<SegCell>()
            + self.entries.capacity() * size_of::<Entry>()
            + self.skew.capacity() * size_of::<(u32, [f32; 2])>()
            + self.axis.capacity() * 4
            + self.plain.capacity() * 8
    }
}

/// The count of lattice vertex `(vi, vj)` read from its owner tile's compact full-pitch block:
/// what `GroundLattice::count_in_tile` returns for a cell of a plain tile
/// ([`SegIndex::tile_is_plain`]). `shift` is `log2(ground_tile_cells)`, `per` the tiles per side.
#[inline]
pub(crate) fn plain_count(c: &WorldGeomCooked, shift: u32, per: u32, vi: u32, vj: u32) -> Option<u16> {
    let a = (vi >> shift).min(per - 1);
    let b = (vj >> shift).min(per - 1);
    let (x0, x1) = c.lattice.tile_cells(a);
    let cols = if a + 1 == per { x1 - x0 + 1 } else { x1 - x0 };
    c.ground_counts
        .get((b * per + a) as usize)?
        .get(((vj - (b << shift)) * cols + (vi - x0)) as usize)
        .copied()
}

/// Height slack of [`crate::WorldGeom::clear_of_lids`], metres, on top of the quanta the window
/// conversion adds.
const LID_MARGIN_M: f32 = 0.05;

impl crate::WorldGeom {
    /// The segment and disc-push tables built at load.
    #[inline]
    pub(crate) fn seg_index(&self) -> &SegIndex {
        &self.seg_index
    }
    /// Whether no lid can cross the vertical line at `(x, z)` at a height from `lo` to `hi`
    /// (finite, metres), read from the segment grid alone: `clamp_under_ceiling` then has nothing
    /// to clamp against in that range and skips its column query (ADR-8815).
    ///
    /// A lid is a roof crossing, or the down face of an active plug. A roof that crosses the line
    /// has the point inside its XZ bounds, so it is filed in the point's cell, and its crossing
    /// height lies inside the height range of the cell's roofs (every roof is gentle, or this
    /// answers `false`). With any plug active this answers `false`: plug faces are not ranged.
    #[inline]
    pub(crate) fn clear_of_lids(
        &self,
        state: &crate::edit::GeomState,
        x: f32,
        z: f32,
        lo: f32,
        hi: f32,
    ) -> bool {
        let seg = &self.seg_index;
        if !seg.gentle_roofs || state.any_blocker_active() {
            return false;
        }
        let key = [(x / seg.cell).floor() as i32, (z / seg.cell).floor() as i32];
        let Some(parts) = seg.cell_parts(key) else {
            return true;
        };
        let (roofs, heights) = parts[2];
        roofs.is_empty()
            || seg.quant.above(1, hi + LID_MARGIN_M) < i32::from(heights[0])
            || seg.quant.below(1, lo - LID_MARGIN_M) > i32::from(heights[1])
    }
}

/// The plain bit of every ground tile ([`SegIndex::tile_is_plain`]).
fn plain_tiles(c: &WorldGeomCooked) -> Vec<u64> {
    let l = &c.lattice;
    let per = l.tiles_per_side();
    let tiles = (per as usize) * (per as usize);
    let mut plain = vec![0u64; tiles.div_ceil(64)];
    let g = u32::from(l.ground_tile_cells);
    let step_positive = l.step_m() > 0.0;
    if !step_positive || !g.is_power_of_two() || c.ground_counts.len() != tiles {
        return plain;
    }
    // A tile whose block is the compact (owned) full-pitch layout.
    let owned = |tile: usize| {
        let (a, b) = ((tile as u32) % per, (tile as u32) / per);
        let (x0, x1) = l.tile_cells(a);
        let (z0, z1) = l.tile_cells(b);
        let cols = if a + 1 == per { x1 - x0 + 1 } else { x1 - x0 };
        let rows = if b + 1 == per { z1 - z0 + 1 } else { z1 - z0 };
        // A full tile only: the count lookup shifts by the tile size.
        (a + 1 == per || x1 - x0 == g)
            && (b + 1 == per || z1 - z0 == g)
            && c.tile_levels.get(tile).copied() == Some(0)
            && c.ground_counts[tile].len() == (cols as usize) * (rows as usize)
    };
    let mut ok: Vec<bool> = (0..tiles).map(&owned).collect();
    // An override vertex rules out every tile it touches (the pyramid's rule), a cut its tile.
    for v in c.hanging_overrides.iter().chain(&c.ring_pins) {
        let (i, j) = (v.vertex % l.lattice_n, v.vertex / l.lattice_n);
        for z in [j.saturating_sub(1) / g, (j / g).min(per - 1)] {
            for x in [i.saturating_sub(1) / g, (i / g).min(per - 1)] {
                if let Some(slot) = ok.get_mut((z * per + x) as usize) {
                    *slot = false;
                }
            }
        }
    }
    for cut in &c.cut_bitmaps {
        if let Some(slot) = ok.get_mut(cut.tile as usize) {
            *slot = false;
        }
    }
    // The east, north and north-east neighbors hold the border vertices: they must be compact
    // full-pitch blocks too. Their overrides matter only on the shared border, and an override
    // there already cleared this tile above.
    for (tile, &free) in ok.iter().enumerate() {
        let (a, b) = ((tile as u32) % per, (tile as u32) / per);
        let (last_x, last_z) = (a + 1 == per, b + 1 == per);
        let neighbors = (last_x || owned(tile + 1))
            && (last_z || owned(tile + per as usize))
            && (last_x || last_z || owned(tile + per as usize + 1));
        if free && neighbors {
            plain[tile / 64] |= 1 << (tile % 64);
        }
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-8815: a quantized box contains its triangle, and the quantized window of a metre box
    /// contains the quanta of every millimetre inside it, so `misses` can only be true when the
    /// metre boxes are apart. Checked across quanta of 1 to 256 mm and both signs of coordinate.
    #[test]
    fn quantized_tests_never_drop_an_overlap() {
        let mut seed = 0x3c6e_f372u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for shift in [0u32, 3, 5, 8] {
            let quant = Quant::new([-700_000, -300_000, 12_345], shift);
            let reach = i64::from(u16::MAX) << shift;
            let mut overlaps = 0u32;
            for _ in 0..40_000 {
                // A triangle box in millimetres inside the table, and a window in metres near it.
                let mut b = [0i64; 6];
                let mut lo = [0.0f32; 3];
                let mut hi = [0.0f32; 3];
                let mut apart = false;
                for k in 0..3 {
                    let start = quant.origin_mm[k] + i64::from(next()) % (reach - 20_000).max(1);
                    let size = i64::from(next() % 9_000);
                    b[2 * k] = start;
                    b[2 * k + 1] = (start + size).min(quant.origin_mm[k] + reach);
                    let from = start + i64::from(next() % 12_000) - 6_000;
                    let to = from + i64::from(next() % 5_000);
                    lo[k] = crate::mm_to_m(0, from as i32);
                    hi[k] = crate::mm_to_m(0, to as i32);
                    // The `f32` vertices of the triangle against the `f32` window.
                    let vmin = crate::mm_to_m(0, b[2 * k] as i32);
                    let vmax = crate::mm_to_m(0, b[2 * k + 1] as i32);
                    apart |= vmin > hi[k] || vmax < lo[k];
                }
                let q: QBox = std::array::from_fn(|i| quant.of_mm(i / 2, b[i]) as u16);
                let w = quant.window(lo, hi);
                if !apart {
                    overlaps += 1;
                    assert!(!misses(&q, &w), "shift {shift} box {b:?} window {lo:?}..{hi:?}");
                }
            }
            assert!(overlaps > 1_000, "shift {shift}: {overlaps} overlaps");
        }
        // Far off the table and non-finite coordinates saturate instead of wrapping.
        let quant = Quant::new([0; 3], 5);
        assert!(quant.below(0, -1e30) < -1_000_000 && quant.above(0, 1e30) > 1_000_000);
        assert!(quant.below(1, f32::NEG_INFINITY) < 0 && quant.above(1, f32::INFINITY) > 0);
        assert!(quant.span(3.0) > 3_000 / 32 && quant.span(f32::INFINITY) > 1_000_000);
    }
}
