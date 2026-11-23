// SPDX-License-Identifier: MPL-2.0
//! Lane W-11 D1 (crater edits, ADR-8818): the one-pass-per-owner-tile edit walk against the
//! per-vertex walk it replaced (`GeomState::apply_ground_edit_reference`).
//!
//! The guard is equality, bit for bit: after every edit of a seeded book the two states are equal
//! (override blocks, promoted tiles, generation, state hash, snapshot bytes), and so are the dirty
//! rectangle, the error and the work counters. The books cover the cooked Switchbacks map (tube
//! roofs under the ground, mouth rings, ground-tile borders, the map's corners), a `Nested` map
//! (promotion, hanging overrides, coarse tiles) and a roofed gallery (the ceiling stop binding).
//! The speed itself is `benches/mixed_calls.rs` (`ground_edit_crater_6m`, spec 121 § 11); the
//! counter here is heap requests per crater, which needs no clock.
#![cfg(feature = "stats")]
#![allow(clippy::unwrap_used)]

pub mod mixed_calls;
pub mod native_mouths;

use regia_world_geom::allocation_counter as alloc_counter;
use regia_world_geom::compat::mesh::{TunnelFaceKind, TunnelTriangle};
use regia_world_geom::cook::{CookInputs, GroundInput, cook, quantize_mesh};
use regia_world_geom::cooked::{CookedTile, CookedTriangle, WorldGeomCooked, encode_canonical};
use regia_world_geom::edit::{HEIGHT_LIMIT_MM, MAX_EDIT_SIDE};
use regia_world_geom::{
    DirtyRect, EditStats, FaceMask, GeomState, GroundEdit, SegmentHit, WorldGeom, m_to_mm, mm_to_m,
};
use std::collections::BTreeMap;

#[global_allocator]
static GLOBAL: alloc_counter::CountingAlloc = alloc_counter::CountingAlloc::new();

// ---------------------------------------------------------------------------------------------
// Fixtures (the shapes of `tests/ground_edits.rs`).

fn cook_section(ground: &GroundInput, mode: &str) -> WorldGeomCooked {
    let doc = format!("(version:1, ground_tile_cells:16, {mode} layers:[])");
    cook(&CookInputs {
        document: doc.as_bytes(),
        ground,
        terrain: None,
        read_file: &|_| Err("no source".into()),
    })
    .unwrap()
    .file
    .canonical
}

fn load(c: &WorldGeomCooked) -> WorldGeom {
    WorldGeom::from_canonical_bytes(&encode_canonical(c).unwrap()).unwrap()
}

fn quad(y: [f32; 2], lo: [f32; 2], hi: [f32; 2], kind: TunnelFaceKind) -> [TunnelTriangle; 2] {
    let t = |a: [f32; 3], b: [f32; 3], c: [f32; 3]| TunnelTriangle {
        a,
        b,
        c,
        kind,
        portal: false,
        pilot_only: false,
    };
    // `y[0]` along the low-x edge, `y[1]` along the high-x edge: a sloped roof when they differ.
    [
        t([lo[0], y[0], lo[1]], [lo[0], y[0], hi[1]], [hi[0], y[1], lo[1]]),
        t([hi[0], y[1], lo[1]], [lo[0], y[0], hi[1]], [hi[0], y[1], hi[1]]),
    ]
}

fn with_triangles(mut c: WorldGeomCooked, tris: &[TunnelTriangle]) -> WorldGeom {
    let mut tiles: BTreeMap<(i32, i32), Vec<CookedTriangle>> = BTreeMap::new();
    for (i, t) in quantize_mesh(tris).unwrap().into_iter().enumerate() {
        let ox = t.v[0][0].div_euclid(1_024_000) * 1_024_000;
        let oz = t.v[0][2].div_euclid(1_024_000) * 1_024_000;
        tiles.entry((oz, ox)).or_default().push(CookedTriangle {
            v: t.v.map(|p| [p[0] - ox, p[1], p[2] - oz]),
            kind: t.kind,
            flags: t.flags,
            layer: 0,
            index: i as u32,
            section: None,
            material: 0,
        });
    }
    c.tiles = tiles
        .into_iter()
        .map(|((oz, ox), triangles)| CookedTile {
            origin_mm: [ox, oz],
            triangles,
        })
        .collect();
    load(&c)
}

/// A 129-sample lattice over 40 m (pitch 312.5 mm, ground tiles of 16 cells), rolling ground
/// around 1 m, over a set of roofs at different depths: two stacked flat ones whose edges lie on
/// lattice lines (a vertex on a roof edge asks the edge-ownership rule), a sloped one that crosses
/// the ground, and floors under them. Roofs span ground-tile borders and index cells.
fn gallery() -> WorldGeom {
    let n = 129u32;
    let counts = (0..n * n)
        .map(|v| {
            let (i, j) = ((v % n) as i64, (v / n) as i64);
            (1000 + ((i * 7 + j * 13) % 11) * 30 + ((i - 64) * (i - 64) + (j - 70) * (j - 70)) / 16) as u16
        })
        .collect();
    let ground = GroundInput {
        lattice_n: n,
        width_m: 40.0,
        offset_m: 0.0,
        step_m: 0.001,
        counts,
    };
    let c = cook_section(&ground, "");
    let mut tris = Vec::new();
    // Edges on lattice lines: x and z at multiples of 0.3125 m.
    tris.extend(quad(
        [0.7, 0.7],
        [-5.0, -5.0],
        [5.0, 5.0],
        TunnelFaceKind::Ceiling,
    ));
    tris.extend(quad(
        [-1.2, -1.2],
        [-7.5, -2.5],
        [2.5, 7.5],
        TunnelFaceKind::Ceiling,
    ));
    // A sloped roof from 3 m under the ground to 1.5 m over it.
    tris.extend(quad(
        [-2.0, 2.5],
        [3.1, -14.3],
        [16.9, -6.2],
        TunnelFaceKind::Ceiling,
    ));
    // A deep one: the stop never binds.
    tris.extend(quad(
        [-9.0, -9.0],
        [-18.0, 8.2],
        [-6.1, 18.0],
        TunnelFaceKind::Ceiling,
    ));
    tris.extend(quad(
        [-3.5, -3.5],
        [-7.5, -5.0],
        [5.0, 7.5],
        TunnelFaceKind::Floor,
    ));
    tris.extend(quad(
        [-12.0, -12.0],
        [-18.0, 8.2],
        [-6.1, 18.0],
        TunnelFaceKind::Floor,
    ));
    with_triangles(c, &tris)
}

/// A `Nested` map of `n` samples at a 0.3125 m pitch: a rough west half (fine tiles) and a smooth
/// slope in the east (coarse tiles). With `n = 129` every ground tile has 16 cells; with `n = 131`
/// the last tile column and row have two.
fn nested_of(n: u32) -> WorldGeom {
    let counts = (0..n * n)
        .map(|v| {
            let (i, j) = ((v % n) as i64, (v / n) as i64);
            let slope = 20_000 + 40 * i + 25 * j;
            let rough = if i < 48 { ((i * 37 + j * 91) % 7) * 120 } else { 0 };
            let bowl = ((i - 64) * (i - 64) + (j - 64) * (j - 64)) / 8;
            (slope + rough + bowl) as u16
        })
        .collect();
    let ground = GroundInput {
        lattice_n: n,
        width_m: (n - 1) as f32 * 0.3125,
        offset_m: -20.0,
        step_m: 0.001,
        counts,
    };
    load(&cook_section(&ground, "ground: Nested(max_level: 3),"))
}

fn nested() -> WorldGeom {
    nested_of(129)
}

// ---------------------------------------------------------------------------------------------
// The book.

struct Rng(u32);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }
    fn below(&mut self, n: u32) -> u32 {
        self.next() % n.max(1)
    }
    fn pick<T: Copy>(&mut self, of: &[T]) -> T {
        of[self.below(of.len() as u32) as usize]
    }
    /// A value in `-span..=span`.
    fn around(&mut self, span: i32) -> i32 {
        self.below(2 * span as u32 + 1) as i32 - span
    }
}

const RADII_MM: [i32; 9] = [1, 150, 313, 1000, 3000, 3125, 6000, 12_000, 19_900];
const AMOUNTS_MM: [i32; 7] = [0, 1, 450, 451, 3000, 40_000, 2_000_000];

/// One seeded edit near `centre_mm`: a crater, a berm, or a rectangle of absolute heights around
/// the state's current ones (some exactly the current or the cooked height, some far below).
fn seeded_edit(geom: &WorldGeom, state: &GeomState, rng: &mut Rng, centre_mm: [i32; 2]) -> GroundEdit {
    let l = geom.cooked().lattice;
    let (x_mm, z_mm) = (centre_mm[0] + rng.around(700), centre_mm[1] + rng.around(700));
    match rng.below(5) {
        0 | 1 => GroundEdit::Crater {
            x_mm,
            z_mm,
            radius_mm: rng.pick(&RADII_MM),
            depth_mm: rng.pick(&AMOUNTS_MM),
        },
        2 => GroundEdit::Berm {
            x_mm,
            z_mm,
            radius_mm: rng.pick(&RADII_MM),
            height_mm: rng.pick(&AMOUNTS_MM),
        },
        _ => {
            let n = l.lattice_n;
            let nearest = |mm: i32| {
                (0..n)
                    .min_by_key(|&i| (l.vertex_mm(i) - i64::from(mm)).abs())
                    .unwrap()
            };
            let side = |rng: &mut Rng| {
                let most = if rng.below(4) == 0 { MAX_EDIT_SIDE } else { 24 };
                1 + rng.below(most)
            };
            let (width, height) = (side(rng), side(rng));
            let col = nearest(x_mm).saturating_sub(width / 2).min(n - width.min(n));
            let row = nearest(z_mm).saturating_sub(height / 2).min(n - height.min(n));
            let fresh = geom.new_state();
            let mut heights_mm = Vec::new();
            for j in row..row + height {
                for i in col..col + width {
                    let v = j * n + i;
                    let now = state.vertex_height_mm(geom, v).unwrap_or(0);
                    heights_mm.push(
                        match rng.below(6) {
                            0 => now,
                            1 => fresh.vertex_height_mm(geom, v).unwrap_or(0),
                            2 => now - 20_000,
                            3 => now + rng.around(3),
                            _ => now + rng.around(2500),
                        }
                        .clamp(-HEIGHT_LIMIT_MM, HEIGHT_LIMIT_MM),
                    );
                }
            }
            GroundEdit::Rect {
                col,
                row,
                width,
                height,
                heights_mm,
            }
        }
    }
}

/// The box of the lattice vertices strictly inside a disc, by testing every vertex of the map.
fn disc_box(geom: &WorldGeom, x_mm: i32, z_mm: i32, radius_mm: i32) -> DirtyRect {
    let l = geom.cooked().lattice;
    let r2 = i64::from(radius_mm) * i64::from(radius_mm);
    let inside = |i: u32, c: i32, rest: i64| {
        let d = l.vertex_mm(i) - i64::from(c);
        d * d + rest < r2
    };
    // A vertex is inside when its column offset and its row offset fit together; the nearest row
    // (column) is the one that leaves the most room.
    let nearest = |c: i32| {
        (0..l.lattice_n)
            .map(|i| {
                let d = l.vertex_mm(i) - i64::from(c);
                d * d
            })
            .min()
            .unwrap()
    };
    let (best_x, best_z) = (nearest(x_mm), nearest(z_mm));
    let cols: Vec<u32> = (0..l.lattice_n).filter(|&i| inside(i, x_mm, best_z)).collect();
    let rows: Vec<u32> = (0..l.lattice_n).filter(|&j| inside(j, z_mm, best_x)).collect();
    match (cols.first(), cols.last(), rows.first(), rows.last()) {
        (Some(&c0), Some(&c1), Some(&r0), Some(&r1)) => DirtyRect {
            col0: c0,
            row0: r0,
            col1: c1 + 1,
            row1: r1 + 1,
        },
        _ => DirtyRect::default(),
    }
}

/// Applies `edit` by both walks and requires the same answer and the same state. Returns the
/// vertices it wrote.
fn apply_both(
    geom: &WorldGeom,
    walk: &mut GeomState,
    reference: &mut GeomState,
    edit: &GroundEdit,
    what: &str,
) -> u64 {
    let shared = walk.clone();
    let (mut walk_stats, mut reference_stats) = (EditStats::default(), EditStats::default());
    let by_walk = walk.apply_ground_edit_with_stats(geom, edit, &mut walk_stats);
    let by_reference = reference.apply_ground_edit_reference(geom, edit, &mut reference_stats);
    assert_eq!(
        by_walk, by_reference,
        "{what}: dirty rectangle or error of {edit:?}"
    );
    assert_eq!(walk_stats, reference_stats, "{what}: work counters of {edit:?}");
    assert_eq!(
        walk.state_hash(),
        reference.state_hash(),
        "{what}: state hash after {edit:?}"
    );
    assert_eq!(
        walk.generation(),
        reference.generation(),
        "{what}: generation after {edit:?}"
    );
    assert!(*walk == *reference, "{what}: override blocks after {edit:?}");
    let (mut a, mut b) = (Vec::new(), Vec::new());
    walk.encode(&mut a);
    reference.encode(&mut b);
    assert!(a == b, "{what}: snapshot bytes after {edit:?}");
    // A no-op leaves the state alone, and a clone taken before the edit never moves.
    if walk_stats.vertices_written == 0 && walk_stats.tiles_promoted == 0 {
        assert!(*walk == shared, "{what}: a no-op changed the state ({edit:?})");
    }
    if let (
        Ok(dirty),
        GroundEdit::Crater {
            x_mm,
            z_mm,
            radius_mm,
            ..
        }
        | GroundEdit::Berm {
            x_mm,
            z_mm,
            radius_mm,
            ..
        },
    ) = (&by_walk, edit)
        && walk_stats.tiles_promoted == 0
        && geom.cooked().ground == regia_world_geom::GroundMode::Full
    {
        assert_eq!(
            *dirty,
            disc_box(geom, *x_mm, *z_mm, *radius_mm),
            "{what}: the footprint box of {edit:?}"
        );
    }
    // The rectangles a host forwards hold each vertex's current height (`height_rects` reads rows;
    // `vertex_height_mm` reads one vertex).
    if let Ok(dirty) = by_walk {
        heights_equal_the_vertex_reads(geom, walk, dirty, what);
    }
    u64::from(walk_stats.vertices_written)
}

/// `GeomState::height_rects` of `rect` against `GeomState::vertex_height_mm` of every vertex in it.
fn heights_equal_the_vertex_reads(geom: &WorldGeom, state: &GeomState, rect: DirtyRect, what: &str) {
    let n = geom.cooked().lattice.lattice_n;
    let mut rects = Vec::new();
    state.height_rects(geom, rect, &mut rects);
    let mut covered = 0u64;
    for piece in &rects {
        assert!(piece.width <= MAX_EDIT_SIDE && piece.height <= MAX_EDIT_SIDE);
        assert_eq!(piece.heights_mm.len() as u32, piece.width * piece.height);
        for (k, &mm) in piece.heights_mm.iter().enumerate() {
            let (i, j) = (
                piece.col + k as u32 % piece.width,
                piece.row + k as u32 / piece.width,
            );
            let read = state.vertex_height_mm(geom, j * n + i).unwrap_or(0);
            assert!(
                mm == read,
                "{what}: height_rects at ({i}, {j}) is {mm}, the vertex reads {read}"
            );
        }
        covered += u64::from(piece.width) * u64::from(piece.height);
    }
    if !rect.is_empty() {
        assert_eq!(
            covered,
            u64::from(rect.col1 - rect.col0) * u64::from(rect.row1 - rect.row0),
            "{what}: the pieces cover the rectangle"
        );
    }
}

/// Runs a seeded book of `count` edits around `centres` on one pair of states that keeps every
/// edit (so later edits meet earlier overrides), and each edit again on a fresh pair. Returns the
/// vertices the book wrote.
fn run_book(geom: &WorldGeom, centres: &[[i32; 2]], seed: u32, count: usize, what: &str) -> u64 {
    let mut rng = Rng(seed);
    let (mut walk, mut reference) = (geom.new_state(), geom.new_state());
    let mut written = 0u64;
    for k in 0..count {
        let centre = rng.pick(centres);
        let edit = seeded_edit(geom, &walk, &mut rng, centre);
        written += apply_both(
            geom,
            &mut walk,
            &mut reference,
            &edit,
            &format!("{what} edit {k}"),
        );
        let (mut fresh_walk, mut fresh_reference) = (geom.new_state(), geom.new_state());
        written += apply_both(
            geom,
            &mut fresh_walk,
            &mut fresh_reference,
            &edit,
            &format!("{what} edit {k} on a fresh state"),
        );
        // Undo part of the book now and then, so blocks shrink and empty out too.
        if k % 9 == 8 {
            let n = geom.cooked().lattice.lattice_n;
            let fresh = geom.new_state();
            let side = (1 + rng.below(40)).min(n);
            let (col, row) = (rng.below(n - side + 1), rng.below(n - side + 1));
            let heights_mm = (0..side * side)
                .map(|v| {
                    fresh
                        .vertex_height_mm(geom, (row + v / side) * n + col + v % side)
                        .unwrap_or(0)
                })
                .collect();
            let undo = GroundEdit::Rect {
                col,
                row,
                width: side,
                height: side,
                heights_mm,
            };
            written += apply_both(
                geom,
                &mut walk,
                &mut reference,
                &undo,
                &format!("{what} undo {k}"),
            );
        }
    }
    written
}

/// Every vertex position of the map's ground-tile lines and edges, as centres, plus `extra`.
fn lattice_centres(geom: &WorldGeom, extra: &[[i32; 2]]) -> Vec<[i32; 2]> {
    let l = geom.cooked().lattice;
    let g = u32::from(l.ground_tile_cells);
    let lines: Vec<i32> = (0..=l.cells())
        .filter(|i| i.is_multiple_of(g) || *i == l.cells())
        .map(|i| l.vertex_mm(i) as i32)
        .collect();
    let mut centres = extra.to_vec();
    for &x in &lines {
        for &z in &lines {
            centres.push([x, z]);
        }
    }
    centres
}

// ---------------------------------------------------------------------------------------------
// The tests.

/// ADR-8818: on a full-pitch map with roofs at several depths (two with edges on lattice lines,
/// one sloped through the ground), the tile walk leaves the state the per-vertex walk leaves,
/// after every edit of a 400-edit book centred on every ground-tile corner and the roofs.
#[test]
fn tile_walk_equals_the_reference_walk_under_roofs() {
    let geom = gallery();
    let centres = lattice_centres(
        &geom,
        &[[0, 0], [-2500, 2500], [10_000, -10_250], [-12_000, 13_100]],
    );
    let written = run_book(&geom, &centres, 0x9e37_79b9, 400, "gallery");
    println!("book: {written} vertex writes");
    assert!(
        written > 100_000,
        "the book moved the ground ({written} vertex writes)"
    );
}

/// ADR-8818: the same on a `Nested` map, where an edit first promotes its coarse tiles (the fills
/// by `promotion_buckets` against the reference's `promotion_fills`, one tile or four at a corner)
/// and the profile then reads promotion fills, hanging overrides and coarse counts. Run on a map
/// whose tiles are all whole and on one whose last tile column and row are two cells wide.
#[test]
fn tile_walk_equals_the_reference_walk_on_a_nested_map() {
    let mut written = 0;
    for (n, seed, count) in [(129, 0x2545_f491, 300), (131, 0x6c07_8965, 160)] {
        let geom = nested_of(n);
        assert!(
            !geom.cooked().hanging_overrides.is_empty(),
            "the map has hanging vertices"
        );
        assert!(
            geom.cooked().tile_levels.iter().any(|&level| level > 0),
            "the map has coarse tiles"
        );
        let centres = lattice_centres(&geom, &[[0, 0], [7000, -3000], [15_000, 15_000]]);
        written += run_book(&geom, &centres, seed, count, &format!("nested {n}"));
    }
    println!("book: {written} vertex writes");
    assert!(
        written > 100_000,
        "the books moved the ground ({written} vertex writes)"
    );
}

/// ADR-8818: the same on the cooked Switchbacks map (the 4096-sample lattice, every mouth filled):
/// craters, berms and rectangles on the tube's line (roofs under the ground), on mouth rings (E5),
/// on ground-tile borders and in the map's corners.
#[test]
fn tile_walk_equals_the_reference_walk_on_switchbacks() {
    let (_, geom, _, sampled) = native_mouths::world("switchbacks");
    let l = geom.cooked().lattice;
    let (n, g) = (l.lattice_n, u32::from(l.ground_tile_cells));
    let mut centres: Vec<[i32; 2]> = sampled
        .passages
        .iter()
        .flat_map(|p| {
            p.samples
                .iter()
                .step_by(7)
                .map(|s| [m_to_mm(s.p[0]), m_to_mm(s.p[2])])
        })
        .collect();
    let tube = centres.len();
    assert!(tube > 30, "{tube} tube centres");
    // Mouth rings, and the ground-tile corners nearest the tube.
    let pins = &geom.cooked().ring_pins;
    assert!(!pins.is_empty());
    for pin in pins.iter().step_by(pins.len() / 24 + 1) {
        centres.push([
            l.vertex_mm(pin.vertex % n) as i32,
            l.vertex_mm(pin.vertex / n) as i32,
        ]);
    }
    for k in 0..tube.min(24) {
        let [x, z] = centres[k * (tube / tube.min(24))];
        let line = |mm: i32| {
            let i = (0..=l.cells() / g)
                .map(|t| (t * g).min(l.cells()))
                .min_by_key(|&i| (l.vertex_mm(i) - i64::from(mm)).abs())
                .unwrap();
            l.vertex_mm(i) as i32
        };
        centres.push([line(x), line(z)]);
    }
    // The map's corners and edges.
    let (first, last) = (l.vertex_mm(0) as i32, l.vertex_mm(l.cells()) as i32);
    centres.extend([
        [first, first],
        [last, last],
        [first, last],
        [last, first],
        [0, last],
    ]);
    let written = run_book(&geom, &centres, 0x1234_5678, 260, "switchbacks");
    println!("book: {written} vertex writes");
    assert!(
        written > 100_000,
        "the book moved the ground ({written} vertex writes)"
    );
}

/// ADR-8818: a 6 m crater on the cooked Switchbacks map asks the heap a bounded number of times,
/// whatever its vertex count: each owner tile's block once, the roofs of each index cell once, the
/// state's ground ranges once. Counted on this thread, on fresh states at the bench's tube points
/// (the per-vertex walk's count is printed beside it: it grew two lists a vertex at a time).
#[test]
fn a_crater_asks_the_heap_a_bounded_number_of_times() {
    /// The most heap requests of one 6 m crater on a fresh state (measured: 19 at most).
    const WALK_BAR: u64 = 20;
    let (_, geom, _, sampled) = native_mouths::world("switchbacks");
    let centres: Vec<[i32; 2]> = sampled
        .passages
        .iter()
        .flat_map(|p| {
            p.samples
                .iter()
                .step_by(11)
                .map(|s| [m_to_mm(s.p[0]), m_to_mm(s.p[2])])
        })
        .take(40)
        .collect();
    let _exclusive = alloc_counter::exclusive();
    let (mut most_walk, mut least_reference) = (0u64, u64::MAX);
    for [x_mm, z_mm] in centres {
        let edit = GroundEdit::Crater {
            x_mm,
            z_mm,
            radius_mm: 6000,
            depth_mm: 450,
        };
        let (mut walk, mut reference) = (geom.new_state(), geom.new_state());
        let (ok, by_walk) = alloc_counter::measure(|| walk.apply_ground_edit(&geom, &edit).is_ok());
        assert!(ok);
        let mut stats = EditStats::default();
        let (ok, by_reference) = alloc_counter::measure(|| {
            reference
                .apply_ground_edit_reference(&geom, &edit, &mut stats)
                .is_ok()
        });
        assert!(ok);
        assert!(
            stats.vertices_written > 1000,
            "a 6 m crater writes about 1160 vertices"
        );
        most_walk = most_walk.max(by_walk.allocs);
        least_reference = least_reference.min(by_reference.allocs);
    }
    println!(
        "heap requests per 6 m crater: tile walk at most {most_walk}, per-vertex walk at least {least_reference}"
    );
    assert!(
        most_walk <= WALK_BAR,
        "the tile walk asked {most_walk} times (bar {WALK_BAR})"
    );
}

// ---------------------------------------------------------------------------------------------
// The ground index under edits.

fn same_hit(a: Option<SegmentHit>, b: Option<SegmentHit>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.t.to_bits() == b.t.to_bits()
                && a.point.map(f32::to_bits) == b.point.map(f32::to_bits)
                && a.kind == b.kind
                && a.tri == b.tri
        }
        _ => false,
    }
}

/// Every ray by the fast walk and by the full walk of every cell, on `state`: the same hit, bit
/// for bit. Returns the ground hits below `floor_m` and above `top_m`.
fn rays_equal_the_full_walk(
    geom: &WorldGeom,
    state: &GeomState,
    rays: &[([f32; 3], [f32; 3])],
    (floor_m, top_m): (f32, f32),
    what: &str,
) -> [usize; 2] {
    let mut beyond = [0usize; 2];
    for (k, &(from, to)) in rays.iter().enumerate() {
        for mask in [FaceMask::SOLID, FaceMask::GROUND] {
            let fast = geom.segment(state, from, to, mask);
            let full = geom.segment_without_pyramid(state, from, to, mask);
            assert!(
                same_hit(fast, full),
                "{what} ray {k} {from:?} -> {to:?} mask {}: {fast:?} vs the full walk {full:?}",
                mask.0
            );
            if let Some(hit) = fast.filter(|h| h.tri.is_none() && mask == FaceMask::GROUND) {
                beyond[0] += usize::from(hit.point[1] < floor_m);
                beyond[1] += usize::from(hit.point[1] > top_m);
            }
        }
    }
    beyond
}

/// Rays through and around edits at `centres` (metres) on ground near `ground_m`: level ones
/// inside the bowls and through the berms, steep ones from above and from below, long ones across
/// the map.
fn edit_rays(rng: &mut Rng, centres: &[[f32; 2]], ground_m: f32) -> Vec<([f32; 3], [f32; 3])> {
    let mut rays = Vec::new();
    let unit = |rng: &mut Rng| rng.below(1 << 16) as f32 / 65_536.0;
    for &[x, z] in centres {
        for _ in 0..12 {
            let (dx, dz) = (unit(rng) * 20.0 - 10.0, unit(rng) * 20.0 - 10.0);
            // Level, a little under or over the cooked ground: only an edit is in the way.
            let y = ground_m + unit(rng) * 3.0 - 0.6;
            rays.push(([x - dx, y, z - dz], [x + dx, y + unit(rng) * 0.2 - 0.1, z + dz]));
            // Down into the edit and up out of it.
            let high = [x + dx * 0.3, ground_m + 6.0, z + dz * 0.3];
            let low = [x - dx * 0.2, ground_m - 2.0 * unit(rng), z - dz * 0.2];
            rays.push((high, low));
            rays.push((low, high));
            // Across the map, grazing the ground.
            rays.push((
                [x + dx * 30.0, ground_m + unit(rng) * 4.0, z + dz * 30.0],
                [x, ground_m - 0.3 + unit(rng), z],
            ));
        }
    }
    rays
}

/// ADR-8818: an edited state keeps its own ground height ranges, and `segment` on it returns the
/// hit of the full walk of every cell (`segment_without_pyramid`), bit for bit: on the cooked
/// Switchbacks map after craters, berms and rectangles along the tube (the mixed tick's shells and
/// sight lines, and rays through every edit: some hit ground the cooked pyramid's ranges do not
/// hold, under its lowest and over its highest), after a snapshot round trip, and on a `Nested`
/// map whose edits promoted tiles. A state the reference walk edited keeps no ranges and still
/// answers the same.
#[test]
fn edited_state_segments_equal_the_full_walk() {
    let (_, geom, _, sampled) = native_mouths::world("switchbacks");
    let ground_m = native_mouths::acceptance_ground_m("switchbacks");
    let tube: Vec<[f32; 3]> = sampled
        .passages
        .iter()
        .flat_map(|p| p.samples.iter().map(|s| s.p))
        .collect();
    let mut rng = Rng(0x0bad_5eed);
    let (mut state, mut reference) = (geom.new_state(), geom.new_state());
    assert!(!state.keeps_ground_ranges(), "a fresh state needs no ranges");
    let mut centres = Vec::new();
    for k in 0..36 {
        let p = tube[rng.below(tube.len() as u32) as usize];
        let (x_mm, z_mm) = (m_to_mm(p[0]) + rng.around(4000), m_to_mm(p[2]) + rng.around(4000));
        let edit = match k % 3 {
            0 => GroundEdit::Crater {
                x_mm,
                z_mm,
                radius_mm: 6000,
                depth_mm: 450 + rng.below(2000) as i32,
            },
            1 => GroundEdit::Berm {
                x_mm,
                z_mm,
                radius_mm: 2000 + rng.below(6000) as i32,
                height_mm: 300 + rng.below(2500) as i32,
            },
            _ => GroundEdit::Crater {
                x_mm,
                z_mm,
                radius_mm: 3000,
                depth_mm: 450,
            },
        };
        apply_both(
            &geom,
            &mut state,
            &mut reference,
            &edit,
            &format!("switchbacks edit {k}"),
        );
        centres.push([mm_to_m(0, x_mm), mm_to_m(0, z_mm)]);
    }
    assert!(state.keeps_ground_ranges(), "an edited state keeps its ranges");
    assert!(!reference.keeps_ground_ranges(), "the reference walk keeps none");
    let tick = mixed_calls::MixedTick::new(&sampled, ground_m, 640.0);
    let mut rays = tick.segments.clone();
    rays.extend(tick.los.iter().copied());
    rays.extend(edit_rays(&mut rng, &centres, ground_m));
    let bounds = (ground_m - 0.05, ground_m + 0.05);
    let beyond = rays_equal_the_full_walk(&geom, &state, &rays, bounds, "switchbacks");
    println!(
        "switchbacks: {} rays, ground hits under the cooked ground {}, over it {}",
        rays.len(),
        beyond[0],
        beyond[1]
    );
    assert!(
        beyond[0] > 20 && beyond[1] > 20,
        "rays hit edited ground the cooked ranges do not hold ({beyond:?})"
    );
    // The state without ranges gives the same answers by walking every cell.
    rays_equal_the_full_walk(&geom, &reference, &rays, bounds, "switchbacks, no ranges");
    for (k, &(from, to)) in rays.iter().enumerate() {
        assert!(
            same_hit(
                geom.segment(&state, from, to, FaceMask::SOLID),
                geom.segment(&reference, from, to, FaceMask::SOLID)
            ),
            "ray {k}: the two states answer alike"
        );
    }
    // A snapshot round trip rebuilds the ranges from the overrides.
    let mut bytes = Vec::new();
    state.encode(&mut bytes);
    let restored = GeomState::decode(&geom, &bytes).unwrap();
    assert!(restored == state && restored.keeps_ground_ranges());
    rays_equal_the_full_walk(&geom, &restored, &rays, bounds, "switchbacks, restored");
    // An undo: every vertex near an edit back at its cooked height, where the ceiling stop allows.
    let mut undone = state.clone();
    let l = geom.cooked().lattice;
    let fresh = geom.new_state();
    for [x, z] in &centres {
        let nearest = |m: f32| {
            (0..l.lattice_n)
                .min_by_key(|&i| (l.vertex_mm(i) - i64::from(m_to_mm(m))).abs())
                .unwrap()
        };
        let (col, row) = (nearest(*x).saturating_sub(30), nearest(*z).saturating_sub(30));
        let side = 61u32.min(l.lattice_n - col.max(row));
        let heights_mm = (0..side * side)
            .map(|v| {
                fresh
                    .vertex_height_mm(&geom, (row + v / side) * l.lattice_n + col + v % side)
                    .unwrap_or(0)
            })
            .collect();
        let rect = GroundEdit::Rect {
            col,
            row,
            width: side,
            height: side,
            heights_mm,
        };
        undone.apply_ground_edit(&geom, &rect).unwrap();
    }
    // The overrides stay (an override at its count height is still an override), and so do the
    // widened ranges: the answers are again the full walk's.
    assert_ne!(
        undone.state_hash(),
        state.state_hash(),
        "the undo moved the ground"
    );
    rays_equal_the_full_walk(&geom, &undone, &rays, bounds, "switchbacks, undone");

    // A `Nested` map: the edits promote tiles (fills on tile borders move the neighbors too).
    let geom = nested();
    let mut state = geom.new_state();
    let mut centres = Vec::new();
    for k in 0..24 {
        let (x_mm, z_mm) = (rng.around(18_000), rng.around(18_000));
        let edit = if k % 2 == 0 {
            GroundEdit::Crater {
                x_mm,
                z_mm,
                radius_mm: 1500 + rng.below(4000) as i32,
                depth_mm: 400 + rng.below(3000) as i32,
            }
        } else {
            GroundEdit::Berm {
                x_mm,
                z_mm,
                radius_mm: 1500 + rng.below(4000) as i32,
                height_mm: 400 + rng.below(3000) as i32,
            }
        };
        state.apply_ground_edit(&geom, &edit).unwrap();
        centres.push([mm_to_m(0, x_mm), mm_to_m(0, z_mm)]);
    }
    assert!(
        !state.ground_edits().promoted().is_empty(),
        "the edits promoted tiles"
    );
    assert!(state.keeps_ground_ranges());
    for ground in [3.0, 6.0, 9.0] {
        let rays = edit_rays(&mut rng, &centres, ground);
        rays_equal_the_full_walk(&geom, &state, &rays, (f32::MIN, f32::MAX), "nested");
    }
}

/// ADR-8818: `height_rects` reads rows of heights; every height equals the one-vertex read, over
/// the whole edited map (pieces of 128), across ground-tile borders and mouth rings, on rectangles
/// that run off the lattice, and on a `Nested` map with promoted tiles and hanging vertices.
#[test]
fn height_rects_equal_the_vertex_reads() {
    let mut rng = Rng(0x7f4a_7c15);
    for geom in [gallery(), nested_of(129), nested_of(131)] {
        let n = geom.cooked().lattice.lattice_n;
        let mut state = geom.new_state();
        let centres = lattice_centres(&geom, &[[0, 0], [5000, -7000]]);
        for _ in 0..60 {
            let centre = rng.pick(&centres);
            let edit = seeded_edit(&geom, &state, &mut rng, centre);
            let _ = state.apply_ground_edit(&geom, &edit);
        }
        assert!(state.ground_edits().override_count() > 1000);
        let whole = DirtyRect {
            col0: 0,
            row0: 0,
            col1: n,
            row1: n,
        };
        heights_equal_the_vertex_reads(&geom, &state, whole, "the whole map");
        // Past the lattice's last column and row: the one-vertex answers, whatever they are.
        let off = DirtyRect {
            col0: n - 3,
            row0: n - 3,
            col1: n + 2,
            row1: n + 2,
        };
        let mut rects = Vec::new();
        state.height_rects(&geom, off, &mut rects);
        assert_eq!(rects.len(), 1);
        for (k, &mm) in rects[0].heights_mm.iter().enumerate() {
            let (i, j) = (off.col0 + k as u32 % 5, off.row0 + k as u32 / 5);
            assert_eq!(
                mm,
                state.vertex_height_mm(&geom, j * n + i).unwrap_or(0),
                "({i}, {j})"
            );
        }
        for _ in 0..200 {
            let (c0, r0) = (rng.below(n), rng.below(n));
            let rect = DirtyRect {
                col0: c0,
                row0: r0,
                col1: (c0 + 1 + rng.below(40)).min(n),
                row1: (r0 + 1 + rng.below(40)).min(n),
            };
            heights_equal_the_vertex_reads(&geom, &state, rect, "a seeded rectangle");
        }
    }
    // Switchbacks: mouth rings and the last, narrower tile.
    let (_, geom, _, _) = native_mouths::world("switchbacks");
    let l = geom.cooked().lattice;
    let n = l.lattice_n;
    let mut state = geom.new_state();
    let pins = geom.cooked().ring_pins.clone();
    for pin in pins.iter().step_by(pins.len() / 12 + 1) {
        let (x_mm, z_mm) = (
            l.vertex_mm(pin.vertex % n) as i32,
            l.vertex_mm(pin.vertex / n) as i32,
        );
        let crater = GroundEdit::Crater {
            x_mm,
            z_mm,
            radius_mm: 6000,
            depth_mm: 450,
        };
        let dirty = state.apply_ground_edit(&geom, &crater).unwrap();
        heights_equal_the_vertex_reads(&geom, &state, dirty, "a crater on a mouth ring");
        let around = DirtyRect {
            col0: dirty.col0.saturating_sub(70),
            row0: dirty.row0.saturating_sub(70),
            col1: (dirty.col1 + 70).min(n),
            row1: (dirty.row1 + 70).min(n),
        };
        heights_equal_the_vertex_reads(&geom, &state, around, "around a mouth ring");
    }
    let corner = DirtyRect {
        col0: n - 140,
        row0: n - 140,
        col1: n,
        row1: n,
    };
    heights_equal_the_vertex_reads(&geom, &state, corner, "the map's last corner");
}
