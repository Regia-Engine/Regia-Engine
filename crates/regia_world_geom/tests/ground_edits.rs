// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/terrain_deform.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 119-197, mark: adapt (Stage B).
// Edits: asserts re-expressed in lattice millimetres; the Stage A crater book (ruins.rs 79-100, hover_ground.rs 37-60,
// requirements) rewritten as the fail-first control; acceptance pins original.
//! D1 ground edits on the lattice (spec 121 § 5.2 and § 6, lane W-12, ADR-8313).
//!
//! Ports of DO27's `ir-sim/src/terrain_deform.rs` tests (regia-absorb-3, lines 119-197, ADR-8012)
//! with their asserts re-expressed in lattice millimetres, and the Stage B acceptance pins. The
//! fail-first form: each bar also runs on the Stage A crater book (the source's flat 3 m, 0.45 m
//! disc added to the heightfield floor through the `GroundDelta` seam, requirements from DO27
//! `ir-sim/src/ruins.rs` 79-100 and `ir-core/src/hover_ground.rs` 37-60) and must fail there.
#![cfg(feature = "stage_b")]
#![allow(clippy::unwrap_used)]

pub mod native_mouths;

use regia_detsim::float::{det_max, det_min};
use regia_detsim::math;
use regia_world_geom::compat::hover_ground::GroundDelta;
use regia_world_geom::compat::mesh::{TunnelFaceKind, TunnelTriangle};
use regia_world_geom::cook::{CookInputs, GroundInput, cook, quantize_mesh};
use regia_world_geom::cooked::{CookedTile, CookedTriangle, WorldGeomCooked, encode_canonical};
use regia_world_geom::edit::{FRESH_STATE_HASH, HEIGHT_LIMIT_MM};
use regia_world_geom::{
    DirtyRect, EditStats, FaceMask, GeomEditError, GeomSnapshotError, GeomState, GroundEdit, SurfaceKind,
    WorldGeom, m_to_mm, mm_to_m,
};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------------------------
// Fixtures.

/// A 65-sample lattice over 20 m (pitch 312.5 mm), ground tiles of 16 cells, flat at `ground_mm`.
fn flat_ground(n: u32, width_m: f32, ground_count: u16) -> GroundInput {
    GroundInput {
        lattice_n: n,
        width_m,
        offset_m: 0.0,
        step_m: 0.001,
        counts: vec![ground_count; (n * n) as usize],
    }
}

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

fn quad(y: f32, lo: f32, hi: f32, kind: TunnelFaceKind) -> [TunnelTriangle; 2] {
    let t = |a: [f32; 3], b: [f32; 3], c: [f32; 3]| TunnelTriangle {
        a,
        b,
        c,
        kind,
        portal: false,
        pilot_only: false,
    };
    [
        t([lo, y, lo], [lo, y, hi], [hi, y, lo]),
        t([hi, y, lo], [lo, y, hi], [hi, y, hi]),
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

/// The gallery: flat ground at 0, a tube roof `roof_m` under it over [-5, 5]², the tube floor
/// 3 m under the roof.
fn gallery(roof_m: f32) -> WorldGeom {
    let c = cook_section(&flat_ground(65, 20.0, 0), "");
    let mut tris = Vec::new();
    tris.extend(quad(roof_m, -5.0, 5.0, TunnelFaceKind::Ceiling));
    tris.extend(quad(roof_m - 3.0, -5.0, 5.0, TunnelFaceKind::Floor));
    with_triangles(c, &tris)
}

fn plain(n: u32, width_m: f32) -> WorldGeom {
    load(&cook_section(&flat_ground(n, width_m, 1000), ""))
}

/// The source's crater book: flat discs of 3.0 m radius and 0.45 m depth, the deepest under the
/// point added (requirements: `ruins.rs` `crater` and `height_delta`).
struct StageACraterBook(Vec<[f32; 2]>);
impl GroundDelta for StageACraterBook {
    fn height_delta(&self, x: f32, z: f32) -> f32 {
        let mut drop = 0.0f32;
        for c in &self.0 {
            let (dx, dz) = (x - c[0], z - c[1]);
            if dx * dx + dz * dz <= 3.0 * 3.0 {
                drop = det_min(drop, -0.45);
            }
        }
        drop
    }
}

/// The Stage A floor: the support floor plus the book's delta on a heightfield floor only (T3).
fn stage_a_floor(geom: &WorldGeom, book: &StageACraterBook, x: f32, z: f32, y: f32) -> f32 {
    let s = geom.support(&geom.new_state(), x, z, y).floor;
    if s.kind == SurfaceKind::Ground {
        s.y + book.height_delta(x, z)
    } else {
        s.y
    }
}

fn crater(x_mm: i32, z_mm: i32, radius_mm: i32, depth_mm: i32) -> GroundEdit {
    GroundEdit::Crater {
        x_mm,
        z_mm,
        radius_mm,
        depth_mm,
    }
}

fn vertex_xz(geom: &WorldGeom, v: u32) -> [f32; 2] {
    let l = &geom.cooked().lattice;
    let n = l.lattice_n;
    [
        mm_to_m(0, l.vertex_mm(v % n) as i32),
        mm_to_m(0, l.vertex_mm(v / n) as i32),
    ]
}

/// The hop meter of `native_hop.rs` (17 samples, ten halvings toward the larger half).
fn discontinuity(f: impl Fn(f32) -> f32) -> f32 {
    let mut worst = (0.0, 0.0, 0.0, 0.0, 0.0);
    let mut previous = f(0.0);
    for i in 1..=16 {
        let (a, b) = ((i - 1) as f32 / 16.0, i as f32 / 16.0);
        let current = f(b);
        if (current - previous).abs() > worst.0 {
            worst = ((current - previous).abs(), a, b, previous, current);
        }
        previous = current;
    }
    if worst.0 < 0.005 {
        return 0.0;
    }
    let (_, mut a, mut b, mut fa, mut fb) = worst;
    for _ in 0..10 {
        let middle = (a + b) * 0.5;
        let fm = f(middle);
        if (fm - fa).abs() >= (fb - fm).abs() {
            (b, fb) = (middle, fm);
        } else {
            (a, fa) = (middle, fm);
        }
    }
    (fb - fa).abs()
}

/// Drives a body standing on `floor` straight from `a` to `b` at `speed` m/s (60 ticks a second)
/// and returns the worst per-tick discontinuity.
fn drive(floor: &dyn Fn(f32, f32, f32) -> f32, a: [f32; 2], b: [f32; 2], speed: f32, ground_y: f32) -> f32 {
    let (dx, dz) = (b[0] - a[0], b[1] - a[1]);
    let distance = math::sqrt(dx * dx + dz * dz);
    let step = speed / 60.0;
    let count = (distance / step).ceil() as usize;
    let mut position = a;
    let mut feet = floor(a[0], a[1], ground_y + 0.9);
    let mut worst = 0.0f32;
    for tick in 1..=count.max(1) {
        let t = if tick >= count {
            1.0
        } else {
            tick as f32 * step / distance
        };
        let next = [a[0] + dx * t, a[1] + dz * t];
        let next_feet = floor(next[0], next[1], feet + 0.9);
        let jump = discontinuity(|u| {
            floor(
                position[0] + (next[0] - position[0]) * u,
                position[1] + (next[1] - position[1]) * u,
                feet + (next_feet - feet) * u + 0.9,
            )
        });
        worst = det_max(worst, jump);
        position = next;
        feet = next_feet;
    }
    worst
}

// ---------------------------------------------------------------------------------------------
// Ported DO27 tests (terrain_deform.rs 119-197), asserts in lattice millimetres.

/// Ported `crater_is_a_cone_with_centre_depth`. Vertex 32 is at 0 mm; 10 pitches is 3125 mm.
#[test]
fn crater_is_a_cone_with_centre_depth() {
    let geom = plain(65, 20.0);
    let mut s = geom.new_state();
    let n = 65u32;
    let at = |s: &GeomState, i: u32, j: u32| s.vertex_height_mm(&geom, j * n + i).unwrap();
    let base = at(&s, 32, 32);
    assert_eq!(base, 1000);
    let dirty = s.apply_ground_edit(&geom, &crater(0, 0, 3125, 3000)).unwrap();
    assert_eq!(at(&s, 32, 32), base - 3000, "the centre drops by the full depth");
    // The rim vertex (d == r) is untouched and outside the dirty rect.
    assert_eq!(at(&s, 42, 32), base);
    assert_eq!(
        dirty,
        DirtyRect {
            col0: 23,
            row0: 23,
            col1: 42,
            row1: 42
        }
    );
    // Symmetry at even offsets (the lattice rounds odd half-pitches up, so x(31) = -312 and
    // x(33) = 313 are not mirror images; x(30) = -625 and x(34) = 625 are).
    assert_eq!(at(&s, 30, 32), at(&s, 34, 32));
    assert_eq!(at(&s, 32, 30), at(&s, 32, 34));
    assert!(at(&s, 30, 32) > base - 3000 && at(&s, 30, 32) < base);
}

/// Ported `same_ops_twice_give_identical_grids_and_dirty_lists`: the overrides, the dirty rects,
/// the snapshot bytes and the hash.
#[test]
fn same_ops_twice_give_identical_grids_and_dirty_lists() {
    let geom = plain(65, 20.0);
    let ops = [
        crater(-1234, 2345, 2500, 2500),
        GroundEdit::Berm {
            x_mm: -1234,
            z_mm: 2345,
            radius_mm: 4000,
            height_mm: 2500,
        },
        crater(3001, -2999, 1777, 450),
    ];
    let run = || {
        let mut s = geom.new_state();
        let rects: Vec<DirtyRect> = ops
            .iter()
            .map(|e| s.apply_ground_edit(&geom, e).unwrap())
            .collect();
        let mut bytes = Vec::new();
        s.encode(&mut bytes);
        (s, rects, bytes)
    };
    let (a, ra, ba) = run();
    let (b, rb, bb) = run();
    assert_eq!(a, b);
    assert_eq!(ra, rb);
    assert_eq!(ba, bb);
    assert_eq!(a.state_hash(), b.state_hash());
    assert_eq!(a.generation().0, 3);
}

/// Ported `berm_rises_outside_the_crater_only`, re-expressed for spec 121 § 6's berm (the same
/// profile raised, not the source's outer ring): a berm raises vertices inside its radius only, and
/// a berm of the crater's profile puts the crater's vertices back to their millimetre heights.
#[test]
fn berm_rises_inside_its_radius_only_and_undoes_the_same_crater() {
    let geom = plain(65, 20.0);
    let n = 65u32;
    let mut s = geom.new_state();
    s.apply_ground_edit(&geom, &crater(0, 0, 3125, 3000)).unwrap();
    assert_eq!(s.vertex_height_mm(&geom, 32 * n + 32), Some(-2000));
    s.apply_ground_edit(
        &geom,
        &GroundEdit::Berm {
            x_mm: 0,
            z_mm: 0,
            radius_mm: 3125,
            height_mm: 3000,
        },
    )
    .unwrap();
    for v in 0..n * n {
        assert_eq!(s.vertex_height_mm(&geom, v), Some(1000), "vertex {v}");
    }
    // A berm on fresh ground: inside rises, the rim and the far field stay.
    let mut b = geom.new_state();
    b.apply_ground_edit(
        &geom,
        &GroundEdit::Berm {
            x_mm: 0,
            z_mm: 0,
            radius_mm: 3125,
            height_mm: 1500,
        },
    )
    .unwrap();
    assert_eq!(b.vertex_height_mm(&geom, 32 * n + 32), Some(2500));
    assert_eq!(
        b.vertex_height_mm(&geom, 32 * n + 36),
        Some(1000 + 1500 * 84 / 100)
    );
    assert_eq!(b.vertex_height_mm(&geom, 32 * n + 42), Some(1000));
    assert_eq!(b.vertex_height_mm(&geom, 0), Some(1000));
}

/// Ported `degenerate_inputs_edit_nothing`: a zero radius is refused, a zero depth changes nothing,
/// an off-map centre is refused, a radius between vertices changes nothing; the state, the
/// generation and the hash stay.
#[test]
fn degenerate_inputs_edit_nothing() {
    let geom = plain(65, 20.0);
    let mut s = geom.new_state();
    let fresh = s.clone();
    assert_eq!(
        s.apply_ground_edit(&geom, &crater(0, 0, 0, 1000)),
        Err(GeomEditError::OutOfRange)
    );
    assert!(s.apply_ground_edit(&geom, &crater(0, 0, 3000, 0)).is_ok());
    assert_eq!(
        s.apply_ground_edit(&geom, &crater(99_000, 99_000, 3000, 1000)),
        Err(GeomEditError::OutOfMap)
    );
    // x(32) = 0 and x(33) = 313: a 100 mm crater at 150 mm holds no vertex.
    assert!(
        s.apply_ground_edit(&geom, &crater(150, 150, 100, 1000))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        s.apply_ground_edit(&geom, &crater(0, 0, -5, 1000)),
        Err(GeomEditError::OutOfRange)
    );
    assert_eq!(
        s.apply_ground_edit(&geom, &crater(0, 0, 3000, -1)),
        Err(GeomEditError::OutOfRange)
    );
    assert_eq!(
        s.apply_ground_edit(&geom, &crater(0, 0, 3000, 2_000_001)),
        Err(GeomEditError::OutOfRange)
    );
    // A footprint clipped to a 65-sample map fits one rectangle; 30 m at 312.5 mm is 192 samples.
    assert!(s.apply_ground_edit(&geom, &crater(0, 0, 1_000_000, 0)).is_ok());
    let wide = plain(257, 80.0);
    assert_eq!(
        wide.new_state()
            .apply_ground_edit(&wide, &crater(0, 0, 30_000, 10)),
        Err(GeomEditError::TooLarge)
    );
    let rect = |w: u32, h: u32, heights: Vec<i32>| GroundEdit::Rect {
        col: 60,
        row: 60,
        width: w,
        height: h,
        heights_mm: heights,
    };
    assert_eq!(
        s.apply_ground_edit(&geom, &rect(6, 1, vec![0; 6])),
        Err(GeomEditError::OutOfMap)
    );
    assert_eq!(
        s.apply_ground_edit(&geom, &rect(0, 1, vec![])),
        Err(GeomEditError::OutOfMap)
    );
    assert_eq!(
        s.apply_ground_edit(&geom, &rect(2, 2, vec![0; 3])),
        Err(GeomEditError::OutOfRange)
    );
    assert_eq!(
        s.apply_ground_edit(&geom, &rect(1, 1, vec![HEIGHT_LIMIT_MM + 1])),
        Err(GeomEditError::OutOfRange)
    );
    assert_eq!(s, fresh);
    assert_eq!(s.state_hash(), FRESH_STATE_HASH);
    for (e, code) in [
        (GeomEditError::OutOfMap, "edit_out_of_map"),
        (GeomEditError::TooLarge, "edit_too_large"),
        (GeomEditError::OutOfRange, "edit_out_of_range"),
    ] {
        assert_eq!(e.code(), code);
    }
}

/// Ported `repeated_strikes_clamp_instead_of_running_away`: ±1 000 000 mm.
#[test]
fn repeated_strikes_clamp_instead_of_running_away() {
    let geom = plain(65, 20.0);
    let mut s = geom.new_state();
    for _ in 0..50 {
        s.apply_ground_edit(&geom, &crater(0, 0, 2000, 900_000)).unwrap();
    }
    assert_eq!(s.vertex_height_mm(&geom, 32 * 65 + 32), Some(-HEIGHT_LIMIT_MM));
    let mut up = geom.new_state();
    for _ in 0..50 {
        up.apply_ground_edit(
            &geom,
            &GroundEdit::Berm {
                x_mm: 0,
                z_mm: 0,
                radius_mm: 2000,
                height_mm: 900_000,
            },
        )
        .unwrap();
    }
    assert_eq!(up.vertex_height_mm(&geom, 32 * 65 + 32), Some(HEIGHT_LIMIT_MM));
}

// ---------------------------------------------------------------------------------------------
// Stage B acceptance (spec 121 § 13.2 and § 13.3).

/// Fail-first 1: a crater over a gallery whose roof is 0.3 m under the ground never lowers the
/// ground under the roof plus `min_cover_mm`. The cover there is already thinner than 2 m, so the
/// ground keeps its height (a crater never raises ground to reach the cover). A deeper roof (2.2 m
/// under) lets the crater dig to exactly the cover. The Stage A book drops it 0.45 m.
#[test]
fn crater_never_lowers_ground_under_roof_plus_min_cover() {
    let edit = crater(0, 0, 3000, 450);
    for roof_m in [-0.3f32, -2.2] {
        let geom = gallery(roof_m);
        let min_cover = geom.settings().min_cover_mm;
        let roof_mm = m_to_mm(roof_m);
        let fresh = geom.new_state();
        let mut s = geom.new_state();
        s.apply_ground_edit(&geom, &edit).unwrap();
        let n = geom.cooked().lattice.lattice_n;
        let mut lowest_cover = i32::MAX;
        for v in 0..n * n {
            let [x, z] = vertex_xz(&geom, v);
            if x.abs() >= 5.0 || z.abs() >= 5.0 {
                continue;
            }
            let before = fresh.vertex_height_mm(&geom, v).unwrap();
            let after = s.vertex_height_mm(&geom, v).unwrap();
            assert!(after <= before, "a crater never raises ground");
            assert!(
                after >= before.min(roof_mm + min_cover),
                "vertex {v}: {after} mm under the cover of a {roof_mm} mm roof"
            );
            lowest_cover = lowest_cover.min(after - roof_mm);
        }
        println!("roof {roof_m} m: thinnest cover after the crater {lowest_cover} mm");
        if roof_m == -2.2 {
            assert_eq!(
                lowest_cover, min_cover,
                "the deep roof stops the crater at the cover"
            );
        } else {
            assert_eq!(lowest_cover, 300, "the shallow roof keeps its 0.3 m cover");
        }
        // The surface the query reads agrees with the vertices.
        let y = geom.support(&s, 0.0, 0.0, 1.0).floor.y;
        assert!(
            y >= mm_to_m(0, roof_mm + min_cover.min(-roof_mm)) - 1e-4,
            "support {y}"
        );
    }
    // Stage A control: the flat disc digs 0.45 m through the 0.3 m cover.
    let geom = gallery(-0.3);
    let book = StageACraterBook(vec![[0.0, 0.0]]);
    let y = stage_a_floor(&geom, &book, 0.0, 0.0, 1.0);
    println!("stage A ground over the 0.3 m roof: {y} m");
    assert!(
        y < -0.3 + 2.0,
        "the Stage A book must break the cover (fail-first form)"
    );
    assert!(y < -0.3, "the Stage A book puts the ground under the roof");
}

/// Kept as a Stage B regression (passes on Stage A too, W-02's
/// `a_crater_moves_the_ground_but_never_a_tunnel_floor_stage_a`): the tube floor moves by zero.
#[test]
fn crater_over_a_covered_tube_moves_its_floor_by_zero() {
    let geom = gallery(-2.2);
    let fresh = geom.new_state();
    let mut s = geom.new_state();
    s.apply_ground_edit(&geom, &crater(0, 0, 3000, 450)).unwrap();
    for [x, z] in [[0.0, 0.0], [1.0, -1.5], [2.9, 0.1]] {
        let a = geom.support(&fresh, x, z, -5.2 + 0.9).floor;
        let b = geom.support(&s, x, z, -5.2 + 0.9).floor;
        assert_eq!(a.kind, SurfaceKind::Floor);
        assert_eq!(a.y.to_bits(), b.y.to_bits(), "tube floor at ({x}, {z})");
    }
}

/// Fail-first 3: a hull driven across a fresh crater rim at full speed sees at most 5 cm per tick
/// (the smooth integer profile over continuous lattice triangles). The flat disc's rim is a
/// 0.45 m step.
#[test]
fn crater_rim_step_le_5cm_per_tick() {
    let geom = plain(65, 20.0);
    let mut s = geom.new_state();
    s.apply_ground_edit(&geom, &crater(0, 0, 3000, 450)).unwrap();
    let stage_b = |x: f32, z: f32, y: f32| geom.support(&s, x, z, y).floor.y;
    let book = StageACraterBook(vec![[0.0, 0.0]]);
    let stage_a = |x: f32, z: f32, y: f32| stage_a_floor(&geom, &book, x, z, y);
    let routes = [
        ([-8.0, 0.0], [8.0, 0.0]),
        ([0.0, -8.0], [0.0, 8.0]),
        ([-7.0, -6.0], [6.5, 7.25]),
        ([-8.0, 2.9], [8.0, 2.9]),
    ];
    let mut worst_b = 0.0f32;
    let mut worst_a = 0.0f32;
    for speed in [50.0, 20.0, 6.0] {
        for (a, b) in routes {
            worst_b = det_max(worst_b, drive(&stage_b, a, b, speed, 1.0));
            worst_a = det_max(worst_a, drive(&stage_a, a, b, speed, 1.0));
        }
    }
    println!("rim hop: stage B {worst_b} m, stage A book {worst_a} m");
    assert!(worst_b <= 0.05, "stage B rim hop {worst_b} m");
    assert!(
        worst_a > 0.05,
        "the Stage A book must fail the bar (fail-first form)"
    );
}

/// The Switchbacks mouth fixture, a crater centred on a ring vertex near the middle of its pin
/// list, and the ring edges (lattice neighbors that are both pins) whose midpoint is within 2.5 m
/// of the centre: `(midpoint, unit normal)`.
struct RingCrater {
    geom: WorldGeom,
    fresh: GeomState,
    edited: GeomState,
    centre: [f32; 2],
    edges: Vec<([f32; 2], [f32; 2])>,
}

fn ring_crater() -> RingCrater {
    let (_, geom, _, _) = native_mouths::world("switchbacks");
    let pins = geom.cooked().ring_pins.clone();
    assert!(!pins.is_empty());
    let l = geom.cooked().lattice;
    let n = l.lattice_n;
    let pin = pins[pins.len() / 2];
    let centre_mm = [
        l.vertex_mm(pin.vertex % n) as i32,
        l.vertex_mm(pin.vertex / n) as i32,
    ];
    let fresh = geom.new_state();
    let mut edited = geom.new_state();
    edited
        .apply_ground_edit(&geom, &crater(centre_mm[0], centre_mm[1], 3000, 450))
        .unwrap();
    let centre = [mm_to_m(0, centre_mm[0]), mm_to_m(0, centre_mm[1])];
    let is_pin = |v: u32| pins.binary_search_by_key(&v, |p| p.vertex).is_ok();
    let mut edges = Vec::new();
    for p in &pins {
        // E5: every ring vertex keeps its pin, bit for bit.
        assert_eq!(
            edited.vertex_height_mm(&geom, p.vertex),
            Some(p.height_mm),
            "ring vertex {}",
            p.vertex
        );
        let (i, j) = (p.vertex % n, p.vertex / n);
        for (di, dj) in [(1i64, 0i64), (0, 1), (1, -1)] {
            let (i2, j2) = (i as i64 + di, j as i64 + dj);
            if i2 < 0 || j2 < 0 || i2 >= n as i64 || j2 >= n as i64 || !is_pin(j2 as u32 * n + i2 as u32) {
                continue;
            }
            let [x0, z0] = vertex_xz(&geom, p.vertex);
            let [x1, z1] = vertex_xz(&geom, j2 as u32 * n + i2 as u32);
            let m = [(x0 + x1) * 0.5, (z0 + z1) * 0.5];
            if (m[0] - centre[0]) * (m[0] - centre[0]) + (m[1] - centre[1]) * (m[1] - centre[1]) >= 2.5 * 2.5
            {
                continue;
            }
            let (ex, ez) = (x1 - x0, z1 - z0);
            let len = math::sqrt(ex * ex + ez * ez);
            edges.push((m, [-ez / len, ex / len]));
        }
    }
    assert!(edges.len() > 4, "the crater covers ring edges ({})", edges.len());
    assert!(
        edited.ground_edits().override_count() > 0,
        "the crater moved the ground around the ring"
    );
    RingCrater {
        geom,
        fresh,
        edited,
        centre,
        edges,
    }
}

/// Fail-first 2 (E5, spec 121 § 13.3): a crater centred on a Switchbacks mouth ring leaves every
/// ring vertex at its pin, and the ground beside every ring edge in the crater still meets the
/// band's fixed edge: the step between the ground and the cooked edge height is at most 5 cm (it
/// reads 0: the edge's two vertices do not move). The flat disc drops the ground 0.45 m off it.
///
/// The full-speed crossing of the mouth (ground to band) needs W-14's zipper first: on this base
/// the cooked state already falls through ring edges (`native_rings` R5, ignored); it is pinned by
/// `crater_on_a_mouth_ring_full_speed_crossing_le_5cm` below.
#[test]
fn crater_on_a_mouth_ring_keeps_the_step_le_5cm() {
    let rc = ring_crater();
    let geom = &rc.geom;
    let book = StageACraterBook(vec![rc.centre]);
    let mut worst = [0.0f32; 2];
    let mut measured = 0;
    for (m, normal) in &rc.edges {
        // The uncut side: the side whose support at the edge is the ground.
        for side in [1.0f32, -1.0] {
            let p = [m[0] + normal[0] * side * 0.002, m[1] + normal[1] * side * 0.002];
            let cooked = geom.support(&rc.fresh, p[0], p[1], -256.0 + 0.9).floor;
            if cooked.kind != SurfaceKind::Ground {
                continue;
            }
            let edited = geom.support(&rc.edited, p[0], p[1], -256.0 + 0.9).floor;
            let stage_a = stage_a_floor(geom, &book, p[0], p[1], -256.0 + 0.9);
            worst[0] = det_max(worst[0], (edited.y - cooked.y).abs());
            worst[1] = det_max(worst[1], (stage_a - cooked.y).abs());
            measured += 1;
        }
    }
    println!(
        "{} ring edges, {measured} ground sides: stage B step {} m, stage A book step {} m",
        rc.edges.len(),
        worst[0],
        worst[1]
    );
    assert!(measured > 4);
    assert!(
        worst[0] <= 0.05,
        "stage B crater steps {} m off the band edge",
        worst[0]
    );
    assert!(
        worst[1] > 0.05,
        "the Stage A book must fail the bar (fail-first form)"
    );
}

/// The full-speed crossing of every ring edge in the crater (one 50 m/s tick from the ground side
/// to the band side and back) reads at most 5 cm per tick on W-05's hop meter.
#[test]
#[ignore = "needs W-14 zipper (lane do27-mesh-layers-seam-cook): the cooked state falls through ring edges"]
fn crater_on_a_mouth_ring_full_speed_crossing_le_5cm() {
    let rc = ring_crater();
    let geom = &rc.geom;
    let ground_y = native_mouths::acceptance_ground_m("switchbacks");
    let before = |x: f32, z: f32, y: f32| geom.support(&rc.fresh, x, z, y).floor.y;
    let after = |x: f32, z: f32, y: f32| geom.support(&rc.edited, x, z, y).floor.y;
    let mut worst = [0.0f32; 2];
    for (m, normal) in &rc.edges {
        let a = [m[0] - normal[0] * 0.415, m[1] - normal[1] * 0.415];
        let b = [m[0] + normal[0] * 0.415, m[1] + normal[1] * 0.415];
        worst[0] = det_max(
            worst[0],
            det_max(
                drive(&before, a, b, 50.0, ground_y),
                drive(&before, b, a, 50.0, ground_y),
            ),
        );
        worst[1] = det_max(
            worst[1],
            det_max(
                drive(&after, a, b, 50.0, ground_y),
                drive(&after, b, a, 50.0, ground_y),
            ),
        );
    }
    println!(
        "ring crossing: cooked {} m, stage B crater {} m",
        worst[0], worst[1]
    );
    assert!(worst[1] <= 0.05, "stage B crater crossing hops {} m", worst[1]);
}

/// Fail-first 4 (spec 121 § 13.2, W2/W9): the committed table of `(edit, vertex, new_mm)` for
/// craters and berms at the centre, the edge and the corner of a ground tile, and an independent
/// `i128` evaluation of the profile for every vertex.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "test bless switch: REGIA_WORLD_GEOM_BLESS rewrites the committed table, read once outside any sim tick (ADR-8367)"
)]
fn crater_profile_matches_the_integer_table() {
    let geom = plain(65, 20.0);
    let l = geom.cooked().lattice;
    let n = l.lattice_n;
    // Ground tiles are 16 cells (5 m): tile (1, 1) spans vertices 16..=32. Its centre is vertex
    // 24 (-2500 mm), an edge midpoint (16, 24), a corner (16, 16); off-lattice centres too.
    let edits = [
        crater(-2500, -2500, 1900, 450),
        crater(-5000, -2500, 2600, 1234),
        crater(-5000, -5000, 3000, 450),
        crater(-4321, -3777, 1500, 2_000_000),
        GroundEdit::Berm {
            x_mm: -2500,
            z_mm: -2500,
            radius_mm: 1900,
            height_mm: 450,
        },
        GroundEdit::Berm {
            x_mm: -5000,
            z_mm: -5000,
            radius_mm: 2222,
            height_mm: 777,
        },
    ];
    let mut table = String::from("# edit\tvertex\tnew_mm (spec 121 § 6 integer profile; ADR-8313)\n");
    for (k, edit) in edits.iter().enumerate() {
        let mut s = geom.new_state();
        let base = 1000i128;
        s.apply_ground_edit(&geom, edit).unwrap();
        let (cx, cz, r, amount, sign) = match *edit {
            GroundEdit::Crater {
                x_mm,
                z_mm,
                radius_mm,
                depth_mm,
            } => (x_mm, z_mm, radius_mm, depth_mm, -1i128),
            GroundEdit::Berm {
                x_mm,
                z_mm,
                radius_mm,
                height_mm,
            } => (x_mm, z_mm, radius_mm, height_mm, 1),
            GroundEdit::Rect { .. } => unreachable!(),
        };
        for v in 0..n * n {
            let dx = l.vertex_mm(v % n) as i128 - cx as i128;
            let dz = l.vertex_mm(v / n) as i128 - cz as i128;
            let (r2, d2) = ((r as i128) * (r as i128), dx * dx + dz * dz);
            let delta = if d2 < r2 {
                (amount as i128 * (r2 - d2)).div_euclid(r2)
            } else {
                0
            };
            let want = (base + sign * delta).clamp(-1_000_000, 1_000_000) as i32;
            let got = s.vertex_height_mm(&geom, v).unwrap();
            assert_eq!(got, want, "edit {k} vertex {v}");
            if delta != 0 {
                table.push_str(&format!("{k}\t{v}\t{got}\n"));
            }
        }
    }
    if std::env::var_os("REGIA_WORLD_GEOM_BLESS").is_some() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/regia/crater-profile-table.tsv");
        std::fs::write(&path, &table).unwrap();
        return;
    }
    // Embedded, so a det-golden leg (a device, the Deck, Windows) needs no source tree (ADR-8838).
    let committed = include_str!("fixtures/regia/crater-profile-table.tsv");
    assert_eq!(committed, table, "the integer profile moved: a § 14 bit change");
}

/// E4: the hash update touches a bounded number of terms per edit (two per vertex written, one per
/// promoted tile), independent of how many edits came before, and `state_hash` is a fold of two
/// words. Ten thousand overlapping craters.
#[test]
fn state_hash_is_o1_per_edit() {
    let geom = plain(129, 40.0);
    let mut s = geom.new_state();
    let mut first_terms = None;
    let mut max_terms = 0;
    for k in 0..10_000i32 {
        let x = (k * 7919).rem_euclid(30_000) - 15_000;
        let z = (k * 104_729).rem_euclid(30_000) - 15_000;
        let mut stats = EditStats::default();
        s.apply_ground_edit_with_stats(&geom, &crater(x, z, 1000, 30), &mut stats)
            .unwrap();
        // A 1 m crater at 312.5 mm pitch holds about 37 vertices (45 bounds every centre).
        assert!(
            stats.hash_terms <= 2 * stats.vertices_written,
            "edit {k}: {stats:?}"
        );
        assert!(stats.vertices_written <= 45, "edit {k}: {stats:?}");
        max_terms = max_terms.max(stats.hash_terms);
        first_terms.get_or_insert(stats.hash_terms);
    }
    println!(
        "first edit {} terms, worst {max_terms} terms, {} overrides after 10k edits",
        first_terms.unwrap(),
        s.ground_edits().override_count()
    );
    assert!(max_terms <= 90);
    // The running hash equals a hash rebuilt from the snapshot (state, not history).
    let mut bytes = Vec::new();
    s.encode(&mut bytes);
    assert_eq!(
        GeomState::decode(&geom, &bytes).unwrap().state_hash(),
        s.state_hash()
    );
}

/// E4 rule 1: two different edit sequences that reach the same overrides hash the same.
#[test]
fn same_overrides_by_different_edits_hash_the_same() {
    let geom = plain(65, 20.0);
    let rect = |col: u32, heights: Vec<i32>| GroundEdit::Rect {
        col,
        row: 10,
        width: heights.len() as u32,
        height: 1,
        heights_mm: heights,
    };
    let mut a = geom.new_state();
    a.apply_ground_edit(&geom, &rect(10, vec![5, 6, 7, 8])).unwrap();
    let mut b = geom.new_state();
    b.apply_ground_edit(&geom, &rect(12, vec![7, 8])).unwrap();
    b.apply_ground_edit(&geom, &rect(10, vec![5, 6])).unwrap();
    // The generations differ (one edit against two), so the accumulators are compared through a
    // third state that reaches b's overrides in one edit after one no-op-free extra edit.
    let mut c = geom.new_state();
    c.apply_ground_edit(&geom, &rect(10, vec![9, 9, 9, 9])).unwrap();
    c.apply_ground_edit(&geom, &rect(10, vec![5, 6, 7, 8])).unwrap();
    assert_eq!(b.ground_edits(), c.ground_edits());
    assert_eq!(b.generation(), c.generation());
    assert_eq!(b.state_hash(), c.state_hash());
    assert_eq!(a.ground_edits(), b.ground_edits());
    assert_ne!(
        a.state_hash(),
        b.state_hash(),
        "the generation is part of the state"
    );
}

/// E4 rule 3: the fresh state hashes to the documented constant on every map.
#[test]
fn fresh_state_hash_is_the_documented_value() {
    for geom in [plain(65, 20.0), plain(129, 40.0), gallery(-0.3)] {
        let s = geom.new_state();
        println!("fresh state hash 0x{:016x}", s.state_hash());
        assert_eq!(s.state_hash(), FRESH_STATE_HASH);
    }
}

/// E4 rule 5: encode, decode: the same state, the same hash and the same answers.
#[test]
fn snapshot_restores_the_same_state_hash() {
    let geom = gallery(-2.2);
    let mut s = geom.new_state();
    s.apply_ground_edit(&geom, &crater(0, 0, 3000, 450)).unwrap();
    s.apply_ground_edit(&geom, &crater(4000, -3500, 2000, 1500))
        .unwrap();
    s.apply_ground_edit(
        &geom,
        &GroundEdit::Berm {
            x_mm: -6000,
            z_mm: 6000,
            radius_mm: 2500,
            height_mm: 800,
        },
    )
    .unwrap();
    let mut bytes = Vec::new();
    s.encode(&mut bytes);
    let restored = GeomState::decode(&geom, &bytes).unwrap();
    assert_eq!(restored, s);
    assert_eq!(restored.state_hash(), s.state_hash());
    let mut again = Vec::new();
    restored.encode(&mut again);
    assert_eq!(again, bytes, "byte-stable");
    // The same query table: support, surface_at and segments over a grid.
    let mut differences = 0;
    for i in 0..41 {
        for j in 0..41 {
            let (x, z) = (-9.5 + i as f32 * 0.475, -9.5 + j as f32 * 0.475);
            for y in [1.0f32, -4.3] {
                let a = geom.support(&s, x, z, y);
                let b = geom.support(&restored, x, z, y);
                differences += usize::from(a != b);
                let a = geom.segment(&s, [x, 5.0, z], [x + 0.3, -6.0, z - 0.2], FaceMask::SOLID);
                let b = geom.segment(&restored, [x, 5.0, z], [x + 0.3, -6.0, z - 0.2], FaceMask::SOLID);
                differences += usize::from(a != b);
            }
        }
    }
    assert_eq!(differences, 0);
    // Refusals.
    let other = plain(65, 20.0);
    assert_eq!(
        GeomState::decode(&other, &bytes),
        Err(GeomSnapshotError::OtherMap)
    );
    let mut newer = bytes.clone();
    newer[0] = 2;
    assert_eq!(
        GeomState::decode(&geom, &newer).unwrap_err().code(),
        "geom_snapshot_newer"
    );
    let n = bytes.len();
    assert_eq!(
        GeomState::decode(&geom, &bytes[..n - 1]),
        Err(GeomSnapshotError::Corrupt)
    );
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert_eq!(
        GeomState::decode(&geom, &trailing),
        Err(GeomSnapshotError::Corrupt)
    );
    let mut flipped = bytes.clone();
    flipped[n - 12] ^= 1; // the last override's height: the stored hash no longer matches
    assert_eq!(
        GeomState::decode(&geom, &flipped),
        Err(GeomSnapshotError::Corrupt)
    );
}

/// A segment and the support agree on an edited ground. The cooked pyramid bounds the ground at
/// 1.0 m, so a flat ray at 1.5 m would skip the tile; the edited state walks it and meets the berm.
#[test]
fn segment_finds_a_berm_over_the_cooked_ground() {
    let geom = plain(65, 20.0);
    let mut s = geom.new_state();
    s.apply_ground_edit(
        &geom,
        &GroundEdit::Berm {
            x_mm: 0,
            z_mm: 0,
            radius_mm: 3000,
            height_mm: 1500,
        },
    )
    .unwrap();
    let top = geom.support(&s, 0.1, 0.1, 5.0).floor.y;
    assert!(top > 2.4, "{top}");
    let (from, to) = ([-4.0, 1.5, 0.1], [4.0, 1.5, 0.1]);
    assert!(
        geom.segment(&geom.new_state(), from, to, FaceMask::GROUND)
            .is_none()
    );
    let hit = geom
        .segment(&s, from, to, FaceMask::GROUND)
        .expect("the ray meets the berm");
    assert!(hit.point[0] < -2.3 && hit.point[0] > -2.6, "{hit:?}");
    #[cfg(feature = "stats")]
    assert_eq!(
        geom.segment_without_pyramid(&s, from, to, FaceMask::GROUND),
        Some(hit)
    );
    let down = geom
        .segment(&s, [0.1, 5.0, 0.1], [0.1, -3.0, 0.1], FaceMask::GROUND)
        .unwrap();
    assert!((down.point[1] - top).abs() < 1e-3, "{down:?} vs {top}");
}

// ---------------------------------------------------------------------------------------------
// E1 rule 6 and the dirty rect.

/// A `Nested` map: a rough west half (fine tiles) and a smooth slope in the east (coarse tiles).
fn nested() -> WorldGeom {
    let n = 129u32;
    let counts = (0..n * n)
        .map(|v| {
            let (i, j) = ((v % n) as i64, (v / n) as i64);
            let slope = 20_000 + 40 * i + 25 * j;
            let rough = if i < 48 { ((i * 37 + j * 91) % 7) * 120 } else { 0 };
            // A gentle bowl so the coarse surface is not exactly the fine one.
            let bowl = ((i - 64) * (i - 64) + (j - 64) * (j - 64)) / 8;
            (slope + rough + bowl) as u16
        })
        .collect();
    let ground = GroundInput {
        lattice_n: n,
        width_m: 40.0,
        offset_m: -20.0,
        step_m: 0.001,
        counts,
    };
    load(&cook_section(&ground, "ground: Nested(max_level: 3),"))
}

fn ground_y(geom: &WorldGeom, s: &GeomState, x: f32, z: f32) -> f32 {
    geom.support(s, x, z, 100.0).floor.y
}

/// The largest height step across every border of `tile`, sampled at both sides of the line.
fn border_step(geom: &WorldGeom, s: &GeomState, tile: u32) -> f32 {
    let l = geom.cooked().lattice;
    let per = l.tiles_per_side();
    let (x0, x1) = l.tile_cells(tile % per);
    let (z0, z1) = l.tile_cells(tile / per);
    let m = |i: u32| mm_to_m(0, l.vertex_mm(i) as i32);
    let mut worst = 0.0f32;
    for k in 0..=64 {
        let t = k as f32 / 64.0;
        let along_x = m(x0) + (m(x1) - m(x0)) * t;
        let along_z = m(z0) + (m(z1) - m(z0)) * t;
        for (x, z, vertical) in [
            (m(x0), along_z, true),
            (m(x1), along_z, true),
            (along_x, m(z0), false),
            (along_x, m(z1), false),
        ] {
            let (a, b) = if vertical {
                (
                    ground_y(geom, s, x.next_down(), z),
                    ground_y(geom, s, x.next_up(), z),
                )
            } else {
                (
                    ground_y(geom, s, x, z.next_down()),
                    ground_y(geom, s, x, z.next_up()),
                )
            };
            worst = det_max(worst, (a - b).abs());
        }
    }
    worst
}

/// Fail-first 6a (E1 rule 6): an edit on a `Nested` map promotes every tile it touches to full
/// pitch; promotion alone moves a height by at most 0.5 mm; the borders of the promoted tiles stay
/// within the 1 mm seam bound before and after the crater; the same edits give the same overrides
/// and bytes (the fixed order).
#[test]
fn edit_promotes_its_tiles_to_full_pitch() {
    let geom = nested();
    let c = geom.cooked();
    let per = c.lattice.tiles_per_side();
    println!(
        "levels {:?}, {} hanging vertices",
        c.tile_levels,
        c.hanging_overrides.len()
    );
    // The crater sits on the corner of four east tiles (vertex 96, 96 at 10 m).
    let corner = [10_000, 10_000];
    let touched = [5 * per + 5, 5 * per + 6, 6 * per + 5, 6 * per + 6];
    for t in touched {
        assert!(c.tile_levels[t as usize] > 0, "tile {t} is coarse in the fixture");
    }
    let fresh = geom.new_state();
    // A zero-height berm only promotes.
    let mut p = geom.new_state();
    let rect = p
        .apply_ground_edit(
            &geom,
            &GroundEdit::Berm {
                x_mm: corner[0],
                z_mm: corner[1],
                radius_mm: 1500,
                height_mm: 0,
            },
        )
        .unwrap();
    assert_eq!(p.ground_edits().promoted(), &touched);
    assert!(p.ground_edits().override_count() > 0);
    assert_eq!(p.generation().0, 1, "promotion is a state change");
    // The dirty rect covers every promoted tile.
    let l = c.lattice;
    assert_eq!(
        rect,
        DirtyRect {
            col0: l.tile_cells(5).0,
            row0: l.tile_cells(5).0,
            col1: l.tile_cells(6).1 + 1,
            row1: l.tile_cells(6).1 + 1
        }
    );
    let mut moved = 0.0f32;
    for i in 0..=96 {
        for j in 0..=96 {
            let (x, z) = (5.0 + i as f32 * 0.1, 5.0 + j as f32 * 0.1);
            moved = det_max(
                moved,
                (ground_y(&geom, &p, x, z) - ground_y(&geom, &fresh, x, z)).abs(),
            );
        }
    }
    println!("promotion moved the ground by at most {moved} m");
    assert!(moved <= 0.000_6, "promotion moved {moved} m");
    for t in touched {
        let step = border_step(&geom, &p, t);
        assert!(step <= 0.001, "tile {t} border step {step} m after promotion");
    }
    // Now a real crater on the same corner.
    let mut s = geom.new_state();
    s.apply_ground_edit(&geom, &crater(corner[0], corner[1], 1500, 600))
        .unwrap();
    assert_eq!(s.ground_edits().promoted(), &touched);
    assert!(ground_y(&geom, &s, 10.0, 10.0) < ground_y(&geom, &fresh, 10.0, 10.0) - 0.59);
    for t in touched {
        let step = border_step(&geom, &s, t);
        assert!(step <= 0.001, "tile {t} border step {step} m after the crater");
    }
    // The fixed order: the same edits give the same bytes.
    let mut t = geom.new_state();
    t.apply_ground_edit(&geom, &crater(corner[0], corner[1], 1500, 600))
        .unwrap();
    let (mut a, mut b) = (Vec::new(), Vec::new());
    s.encode(&mut a);
    t.encode(&mut b);
    assert_eq!(a, b);
    // A Full map never promotes.
    let full = plain(65, 20.0);
    let mut f = full.new_state();
    f.apply_ground_edit(&full, &crater(0, 0, 1500, 600)).unwrap();
    assert!(f.ground_edits().promoted().is_empty());
}

/// Fail-first 6b: one generation per edit, and the dirty rect reaches the three consumers in a
/// fixed order: the `TerrainHeightEdits` rectangles (absolute mm equal to the state's heights, each
/// at most 128 samples a side), the nav and ground-index tiles it overlaps (ascending), and the
/// generation the query index and nav compare.
#[test]
fn dirty_rect_reaches_terrain_height_edits_nav_and_the_index_generation() {
    let geom = plain(129, 40.0);
    let l = geom.cooked().lattice;
    let n = l.lattice_n;
    let mut s = geom.new_state();
    let mut seen = Vec::new();
    for (k, edit) in [
        crater(0, 0, 3000, 450),
        crater(-15_000, 15_000, 2000, 300),
        GroundEdit::Rect {
            col: 0,
            row: 0,
            width: 128,
            height: 3,
            heights_mm: (0..384).collect(),
        },
    ]
    .iter()
    .enumerate()
    {
        let before = s.generation();
        let rect = s.apply_ground_edit(&geom, edit).unwrap();
        assert_eq!(s.generation().0, before.0 + 1, "edit {k}: one generation");
        let mut rects = Vec::new();
        s.height_rects(&geom, rect, &mut rects);
        let mut samples = 0;
        for r in &rects {
            assert!(r.width <= 128 && r.height <= 128);
            assert_eq!(r.heights_mm.len() as u32, r.width * r.height);
            for dj in 0..r.height {
                for di in 0..r.width {
                    let v = (r.row + dj) * n + r.col + di;
                    assert_eq!(
                        r.heights_mm[(dj * r.width + di) as usize],
                        s.vertex_height_mm(&geom, v).unwrap()
                    );
                }
            }
            samples += r.width * r.height;
        }
        assert_eq!(samples, (rect.col1 - rect.col0) * (rect.row1 - rect.row0));
        let tiles = rect.ground_tiles(&l);
        assert!(tiles.windows(2).all(|w| w[0] < w[1]));
        seen.push((rect, tiles, s.generation()));
    }
    // The crater at the origin straddles the four centre tiles of the 8 x 8 tile map.
    assert_eq!(seen[0].1, vec![27, 28, 35, 36]);
    // The west-north crater sits in the corner tile column 0, row 7 (and its neighbors).
    assert!(seen[1].1.contains(&56));
    // The rect along the south edge: tiles 0..8 of row 0.
    assert_eq!(seen[2].1, (0..8).collect::<Vec<_>>());
    // Every vertex outside the dirty rects kept its cooked height.
    let fresh = geom.new_state();
    for v in 0..n * n {
        let (i, j) = (v % n, v / n);
        let inside = seen
            .iter()
            .any(|(r, _, _)| i >= r.col0 && i < r.col1 && j >= r.row0 && j < r.row1);
        if !inside {
            assert_eq!(
                s.vertex_height_mm(&geom, v),
                fresh.vertex_height_mm(&geom, v),
                "vertex {v}"
            );
        }
    }
}

/// The query table (W-11) does not move with no edit applied: a fresh state answers as before, and
/// a state whose edits were all no-ops is equal to it.
#[test]
fn a_noop_edit_leaves_the_fresh_state() {
    let geom = gallery(-0.3);
    let mut s = geom.new_state();
    // The 0.3 m cover is under the 2 m minimum: the crater changes nothing.
    let rect = s.apply_ground_edit(&geom, &crater(0, 0, 2000, 450)).unwrap();
    assert!(
        !rect.is_empty(),
        "the footprint is reported even when the stop clips all of it"
    );
    assert_eq!(s, geom.new_state());
    assert_eq!(s.generation().0, 0);
}

/// A `Rect` edit takes the same ceiling stop and ring clip as a crater: over the 2.2 m roof it stops
/// at the 2 m cover, beside it it writes the asked height, and on a mouth ring it writes nothing.
#[test]
fn rect_edits_take_the_ceiling_stop_and_the_ring_clip() {
    let geom = gallery(-2.2);
    let n = geom.cooked().lattice.lattice_n;
    let mut s = geom.new_state();
    // Columns 12..20 straddle the roof's west edge at x = -5 m (vertex 16).
    let (col, row, width, height) = (12u32, 30u32, 8u32, 2u32);
    s.apply_ground_edit(
        &geom,
        &GroundEdit::Rect {
            col,
            row,
            width,
            height,
            heights_mm: vec![-5000; (width * height) as usize],
        },
    )
    .unwrap();
    for j in row..row + height {
        for i in col..col + width {
            let v = j * n + i;
            let x = vertex_xz(&geom, v)[0];
            let got = s.vertex_height_mm(&geom, v).unwrap();
            if x > -5.0 {
                assert_eq!(got, -200, "vertex {v} over the roof stops at the cover");
            } else if x < -5.0 {
                assert_eq!(got, -5000, "vertex {v} beside the roof takes the height");
            }
        }
    }
    // A rect raised over the roof is not stopped.
    s.apply_ground_edit(
        &geom,
        &GroundEdit::Rect {
            col: 32,
            row: 32,
            width: 1,
            height: 1,
            heights_mm: vec![1500],
        },
    )
    .unwrap();
    assert_eq!(s.vertex_height_mm(&geom, 32 * n + 32), Some(1500));

    // On the mouth fixture a rect over ring vertices leaves every pin.
    let (_, mouths, _, _) = native_mouths::world("switchbacks");
    let l = mouths.cooked().lattice;
    let pins = mouths.cooked().ring_pins.clone();
    let pin = pins[pins.len() / 3];
    let (pi, pj) = (pin.vertex % l.lattice_n, pin.vertex / l.lattice_n);
    let mut m = mouths.new_state();
    m.apply_ground_edit(
        &mouths,
        &GroundEdit::Rect {
            col: pi - 4,
            row: pj - 4,
            width: 9,
            height: 9,
            heights_mm: vec![-300_000; 81],
        },
    )
    .unwrap();
    let mut on_ring = 0;
    for p in &pins {
        let (i, j) = (p.vertex % l.lattice_n, p.vertex / l.lattice_n);
        if i + 4 >= pi && i <= pi + 4 && j + 4 >= pj && j <= pj + 4 {
            on_ring += 1;
            assert_eq!(m.vertex_height_mm(&mouths, p.vertex), Some(p.height_mm));
        }
    }
    assert!(on_ring > 2, "the rect covers ring vertices ({on_ring})");
}
