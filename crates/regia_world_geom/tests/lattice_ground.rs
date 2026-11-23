// SPDX-License-Identifier: MPL-2.0
//! Lane W-10 (spec 121 § 3, ADR-8214): one ground for render, holes and the sim. The heightfield's
//! own lattice with integer-millimetre vertices, mouths cut as named holes on lattice edges, rings
//! pinned bit for bit, the `Nested` ground mode with the seam rule E1.
//!
//! Fixtures: DO27's three tunnel layouts (the crate's `fixtures` feature) over a synthetic rolling
//! ground generated here, and a synthetic Washboard-sized map (2050 m, 6561 samples) generated here
//! too. No DO27 map data is copied.

// Test fixtures panic on a broken fixture, as meshlayers_formats.rs does.
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};

use regia_detsim::math::{cos, sin, sqrt, sqrt_f64};
use regia_world_geom::checks::{
    GROUND_TILES_LISTED, ground_budget_bytes, only_ramp_facet_failures, waive_ramp_facets,
};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::layout::{HOLE_END_M, LIP_W_M, PassageKind, SampledLayout, TunnelLayout};
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::compat::terrain_patch::{NONE_M, TerrainPatch};
use regia_world_geom::cook::{CookError, CookInputs, CookOutput, GroundInput, cook};
use regia_world_geom::cooked::{LatticeConstants, WorldGeomCooked};
use regia_world_geom::cut::{centroid3, neighbor, tri_vertices};
use regia_world_geom::ground::{GroundAt, GroundLattice, vertex_m};
use regia_world_geom::passages::PassagesDocument;
use regia_world_geom::pitch::{PitchInputs, check_ground_pitch_seam, count_blocks};
use regia_world_geom::{CheckFinding, GeomSettings, GroundMode, TerrainBakeReport, mm_to_m};

const MAPS: [&str; 3] = ["switchbacks", "cinder_steps", "atacama_rail"];

/// The Switchbacks render grid: 4096 samples over 1280 m.
const N: u32 = 4096;
const WIDTH_M: f32 = 1280.0;

/// Rolling ground around the -256 m canyon floor, metres.
fn rolling_m(x: f64, z: f64) -> f64 {
    -256.0 + 3.0 * f64::from(sin((x / 53.0) as f32)) * f64::from(cos((z / 41.0) as f32))
}

/// Counts at offset -512 m, 1/32 m a count (the Stage A height scale).
fn count_of(h: f64) -> u16 {
    ((h + 512.0) * 32.0).round() as u16
}

fn rolling_ground(n: u32) -> GroundInput {
    let mut counts = Vec::with_capacity((n * n) as usize);
    for j in 0..n {
        for i in 0..n {
            let (x, z) = (
                -640.0 + f64::from(i) * 1280.0 / f64::from(n - 1),
                -640.0 + f64::from(j) * 1280.0 / f64::from(n - 1),
            );
            counts.push(count_of(rolling_m(x, z)));
        }
    }
    GroundInput {
        lattice_n: n,
        width_m: WIDTH_M,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts,
    }
}

/// The same rolling ground as DO27's Stage A sampler reads it: 256 samples on the `i / n` 5 m grid.
fn rolling_terrain() -> TerrainSampler {
    let side = 256usize;
    let heights = (0..side * side)
        .map(|k| {
            let (i, j) = (k % side, k / side);
            count_of(rolling_m(-640.0 + 5.0 * i as f64, -640.0 + 5.0 * j as f64))
        })
        .collect();
    TerrainSampler::new(HeightGrid::square(side, heights).unwrap(), WIDTH_M).with_zero_counts(16384)
}

fn opening_ids(layout: &TunnelLayout) -> Vec<String> {
    layout.openings.iter().map(|o| o.id.clone()).collect()
}

fn cook_map(map: &str, ground: &GroundInput, terrain: &TerrainSampler, mode: &str) -> CookOutput {
    try_cook_map(map, ground, terrain, mode).unwrap_or_else(|e| panic!("{map}: {e}"))
}

fn try_cook_map(
    map: &str,
    ground: &GroundInput,
    terrain: &TerrainSampler,
    mode: &str,
) -> Result<CookOutput, CookError> {
    let layout = for_map(map).unwrap();
    let fills: Vec<String> = opening_ids(&layout)
        .into_iter()
        .filter(|id| {
            regia_world_geom::cut::passage_mouths(&layout, &layout.sample(Some(terrain)))
                .iter()
                .any(|m| &m.id == id)
        })
        .map(|id| format!("{id:?}"))
        .collect();
    let doc = format!(
        "(version: 1, ground_tile_cells: 64, {mode} layers: [(id: \"tubes\", source: Passages(path: \"map.passages.ron\"), fills: [{}])])",
        fills.join(", ")
    );
    let passages = PassagesDocument::from_layout(layout).to_ron().unwrap();
    let files: BTreeMap<String, Vec<u8>> = [("map.passages.ron".to_string(), passages.into_bytes())].into();
    let read = |p: &str| files.get(p).cloned().ok_or_else(|| format!("{p}: not found"));
    let run = |document: &str| {
        cook(&CookInputs {
            document: document.as_bytes(),
            ground,
            terrain: Some(terrain),
            read_file: &read,
        })
    };
    // ADR-8359: a test cook waives the two known Switchbacks ramp facets by hash and recooks.
    match run(&doc) {
        Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
            run(&waive_ramp_facets(&doc, &report).map_err(CookError::Document)?)
        }
        other => other,
    }
}

/// Every canonical vertex of a cooked section as `f32` bits (`mm_to_m` of its world millimetre).
fn canonical_vertex_bits(c: &WorldGeomCooked) -> BTreeSet<[u32; 3]> {
    let mut out = BTreeSet::new();
    for tile in &c.tiles {
        let [ox, oz] = tile.origin_mm;
        for t in &tile.triangles {
            for p in t.v {
                out.insert([
                    mm_to_m(ox, p[0]).to_bits(),
                    mm_to_m(0, p[1]).to_bits(),
                    mm_to_m(oz, p[2]).to_bits(),
                ]);
            }
        }
    }
    out
}

/// Ring vertices whose lattice position (by `position`) is not bit-equal to a canonical vertex.
fn ring_vertices_not_shared(
    c: &WorldGeomCooked,
    position: impl Fn(&LatticeConstants, u32) -> f32,
) -> (usize, usize) {
    let canon = canonical_vertex_bits(c);
    let n = c.lattice.lattice_n;
    let ground = GroundLattice::new(c);
    let mut missing = 0;
    for pin in &c.ring_pins {
        let (i, j) = (pin.vertex % n, pin.vertex / n);
        let tile = ground.tile_of_cell(i.min(n - 2), j.min(n - 2));
        let y = ground.vertex_height_in_tile(tile, i, j).unwrap();
        assert_eq!(
            y.to_bits(),
            mm_to_m(0, pin.height_mm).to_bits(),
            "the lattice reads the pin"
        );
        let p = [
            position(&c.lattice, i).to_bits(),
            y.to_bits(),
            position(&c.lattice, j).to_bits(),
        ];
        if !canon.contains(&p) {
            missing += 1;
        }
    }
    (missing, c.ring_pins.len())
}

/// The f32 lattice `-W/2 + i * W / (n - 1)` that the integer lattice replaces (spec 121 § 13.3).
fn f32_lattice(l: &LatticeConstants, i: u32) -> f32 {
    let w = mm_to_m(0, l.width_mm);
    -w / 2.0 + i as f32 * w / (l.lattice_n - 1) as f32
}

/// Spec 121 § 13.2 (review W2): every ring vertex of the fixtures has equal `f32` bits as a lattice
/// vertex and as a canonical vertex. The control shows the same check fails on the `f32` lattice
/// `-W/2 + i * W / (n - 1)` (the fail-first form).
#[test]
fn lattice_vertices_are_integer_mm_and_shared_with_the_rings() {
    let ground = rolling_ground(N);
    let terrain = rolling_terrain();
    let mut f32_missing = 0;
    for map in MAPS {
        let out = cook_map(map, &ground, &terrain, "");
        let c = &out.file.canonical;
        assert!(!c.ring_pins.is_empty(), "{map}: the fixture has mouths");
        let (missing, total) = ring_vertices_not_shared(c, vertex_m);
        println!("{map}: {total} ring vertices, {missing} not shared on the integer lattice");
        assert_eq!(missing, 0, "{map}");
        // Every vertex is on integer millimetres and within 0.5 mm of the f32 sample position.
        for i in [0, 1, N / 2, N - 2, N - 1] {
            let mm = c.lattice.vertex_mm(i);
            assert_eq!(vertex_m(&c.lattice, i), mm_to_m(0, mm as i32));
            assert!(
                (f64::from(vertex_m(&c.lattice, i)) - f64::from(f32_lattice(&c.lattice, i))).abs() <= 0.0005
            );
        }
        f32_missing += ring_vertices_not_shared(c, f32_lattice).0;
    }
    println!("control: {f32_missing} ring vertices not shared on the f32 lattice");
    assert!(
        f32_missing > 0,
        "the f32 lattice must miss ring vertices (the fail-first form)"
    );
}

/// The adjacency of two lattice vertices along a lattice edge (axis edges and the
/// `(x0, z1)-(x1, z0)` diagonal).
fn lattice_edge(a: (u32, u32), b: (u32, u32)) -> bool {
    let d = (i64::from(b.0) - i64::from(a.0), i64::from(b.1) - i64::from(a.1));
    matches!(d, (1, 0) | (-1, 0) | (0, 1) | (0, -1) | (1, -1) | (-1, 1))
}

/// Plan § 4.1 "Ground" and spec 121 § 3 ring pin item 3: every ring is a closed loop of lattice edges;
/// each ring edge separates a cut lattice triangle from an uncut one, is an edge of exactly one
/// canonical (collar) triangle with equal integer endpoints `(x_mm(i), ring_mm, z_mm(j))`, and every
/// boundary edge of the cut is in exactly one ring.
#[test]
fn rings_are_the_cut_boundary() {
    let ground = rolling_ground(N);
    let terrain = rolling_terrain();
    for map in MAPS {
        let out = cook_map(map, &ground, &terrain, "");
        let c = &out.file.canonical;
        let lg = GroundLattice::new(c);
        let l = c.lattice;
        let cells = l.cells();
        let pin: BTreeMap<u32, i32> = c.ring_pins.iter().map(|p| (p.vertex, p.height_mm)).collect();
        // Canonical edges by integer endpoints (unordered), counted.
        let mut canon_edges: BTreeMap<([i64; 3], [i64; 3]), u32> = BTreeMap::new();
        for tile in &c.tiles {
            let [ox, oz] = tile.origin_mm;
            for t in &tile.triangles {
                let w = t.v.map(|p| {
                    [
                        i64::from(ox) + i64::from(p[0]),
                        i64::from(p[1]),
                        i64::from(oz) + i64::from(p[2]),
                    ]
                });
                for k in 0..3 {
                    let (a, b) = (w[k], w[(k + 1) % 3]);
                    *canon_edges
                        .entry(if a <= b { (a, b) } else { (b, a) })
                        .or_default() += 1;
                }
            }
        }
        let world = |v: (u32, u32)| -> [i64; 3] {
            [
                l.vertex_mm(v.0),
                i64::from(pin[&(v.1 * l.lattice_n + v.0)]),
                l.vertex_mm(v.1),
            ]
        };
        let mut ring_edges = BTreeSet::new();
        let mut rings = 0;
        let mouth_vertices: usize = out
            .report
            .ground
            .mouths
            .iter()
            .flat_map(|m| &m.rings)
            .map(Vec::len)
            .sum();
        {
            for ring in &out.report.ground.rings {
                rings += 1;
                assert!(ring.len() >= 3, "{map}: a ring has at least three vertices");
                for k in 0..ring.len() {
                    let (a, b) = (ring[k], ring[(k + 1) % ring.len()]);
                    assert!(lattice_edge(a, b), "{map}: {a:?} -> {b:?} is a lattice edge");
                    let (pa, pb) = (world(a), world(b));
                    let key = if pa <= pb { (pa, pb) } else { (pb, pa) };
                    assert_eq!(
                        canon_edges.get(&key),
                        Some(&1),
                        "{map}: ring edge {a:?}-{b:?} is one canonical edge"
                    );
                    assert!(
                        ring_edges.insert(if a <= b { (a, b) } else { (b, a) }),
                        "{map}: a ring edge is in one ring"
                    );
                }
            }
        }
        // Every boundary edge of the cut is a ring edge, and it has exactly one uncut lattice
        // triangle across it.
        let mut boundary = 0;
        for j in 0..cells {
            for i in 0..cells {
                for t in 0..2u8 {
                    if !lg.is_cut(i, j, t) {
                        continue;
                    }
                    let v = tri_vertices(i, j, t);
                    for e in 0..3u8 {
                        let (ni, nj, nt) = neighbor(cells, i, j, t, e).expect("cut inside the map");
                        if lg.is_cut(ni, nj, nt) {
                            continue;
                        }
                        boundary += 1;
                        let (a, b) = (v[e as usize], v[(e as usize + 1) % 3]);
                        assert!(
                            ring_edges.contains(&if a <= b { (a, b) } else { (b, a) }),
                            "{map}: boundary edge {a:?}-{b:?} is on a ring"
                        );
                    }
                }
            }
        }
        assert_eq!(boundary, ring_edges.len(), "{map}");
        println!(
            "{map}: {rings} rings, {boundary} ring edges, {} ring pins ({mouth_vertices} ring vertices over the mouths' own rings)",
            c.ring_pins.len()
        );
    }
}

/// The open-cut floor quads of a sampled layout's ramps (the canonical floor of those segments, at
/// half width) or, with `extra_m > 0`, the lip zone (half width plus the lip, and the mouth cap
/// `HOLE_END_M` past the ramp top), world mm, per segment.
fn open_cut_quads(sampled: &SampledLayout, extra_m: f64) -> Vec<[[i64; 2]; 4]> {
    let mm = |v: f64| (v * 1000.0).round() as i64;
    let mut out = Vec::new();
    for p in sampled.passages.iter().filter(|p| p.kind == PassageKind::Ramp) {
        let hw = f64::from(p.width_m) * 0.5 + extra_m;
        for k in 0..p.samples.len().saturating_sub(1) {
            if p.covered[k] {
                continue;
            }
            let (a, mut b) = (p.samples[k], p.samples[k + 1]);
            if extra_m > 0.0 && k + 2 == p.samples.len() {
                let (dx, dz) = (b.p[0] - a.p[0], b.p[2] - a.p[2]);
                let len = sqrt(dx * dx + dz * dz);
                b.p[0] += dx / len * HOLE_END_M;
                b.p[2] += dz / len * HOLE_END_M;
            }
            let side = |s: &regia_world_geom::compat::layout::Sample, sign: f64| {
                [
                    mm(f64::from(s.p[0]) + f64::from(s.left[0]) * hw * sign),
                    mm(f64::from(s.p[2]) + f64::from(s.left[1]) * hw * sign),
                ]
            };
            out.push([side(&a, 1.0), side(&a, -1.0), side(&b, -1.0), side(&b, 1.0)]);
        }
    }
    out
}

fn inside(poly: &[[i64; 2]], p: [f64; 2]) -> bool {
    let mut c = false;
    for k in 0..poly.len() {
        let (a, b) = (poly[k], poly[(k + 1) % poly.len()]);
        let (ax, az, bx, bz) = (a[0] as f64, a[1] as f64, b[0] as f64, b[1] as f64);
        if (az > p[1]) != (bz > p[1]) && p[0] < ax + (bx - ax) * (p[1] - az) / (bz - az) {
            c = !c;
        }
    }
    c
}

/// The requirements' one-rectangle-per-ramp rule (DO27 `open_cut_holes`, read as requirements, not
/// copied): from the first open sample to the ramp top, ramp width + 1.2 m, length + 3 m.
fn one_rect_per_ramp(sampled: &SampledLayout) -> Vec<[[i64; 2]; 4]> {
    let mm = |v: f64| (v * 1000.0).round() as i64;
    let mut out = Vec::new();
    for p in sampled.passages.iter().filter(|p| p.kind == PassageKind::Ramp) {
        let Some(first) = p.covered.iter().position(|c| !c) else {
            continue;
        };
        let (s0, s1) = (p.samples[first].p, p.samples[p.samples.len() - 1].p);
        let (dx, dz) = (f64::from(s1[0] - s0[0]), f64::from(s1[2] - s0[2]));
        let len = sqrt_f64(dx * dx + dz * dz);
        if len < 1.0 {
            continue;
        }
        let (ux, uz) = (dx / len, dz / len);
        let (cx, cz) = (f64::from(s0[0] + s1[0]) * 0.5, f64::from(s0[2] + s1[2]) * 0.5);
        let (hl, hw) = (len * 0.5 + 3.0, f64::from(p.width_m) * 0.5 + 1.2);
        let corner = |a: f64, b: f64| [mm(cx + ux * a - uz * b), mm(cz + uz * a + ux * b)];
        out.push([corner(-hl, -hw), corner(hl, -hw), corner(hl, hw), corner(-hl, hw)]);
    }
    out
}

/// Cells (lattice triangles) where the canonical open-cut floor is under the centroid but the mask
/// keeps ground (`wedge`), and cells the mask cuts with neither floor nor lip under them (`void`).
fn wedges_and_voids(
    l: &LatticeConstants,
    sampled: &SampledLayout,
    cut: impl Fn(u32, u32, u8) -> bool,
) -> (usize, usize) {
    let floors = open_cut_quads(sampled, 0.0);
    let lips = open_cut_quads(sampled, f64::from(LIP_W_M));
    let mut lo = [i64::MAX; 2];
    let mut hi = [i64::MIN; 2];
    for q in &lips {
        for p in q {
            lo = [lo[0].min(p[0] - 10_000), lo[1].min(p[1] - 10_000)];
            hi = [hi[0].max(p[0] + 10_000), hi[1].max(p[1] + 10_000)];
        }
    }
    let cells = l.cells();
    let first = |v: i64| (0..cells).find(|&i| l.vertex_mm(i + 1) >= v).unwrap_or(0);
    let (mut wedge, mut void) = (0, 0);
    for j in first(lo[1])..first(hi[1]).min(cells - 1) {
        for i in first(lo[0])..first(hi[0]).min(cells - 1) {
            for t in 0..2u8 {
                let c3 = centroid3(l, i, j, t);
                let p = [c3[0] as f64 / 3.0, c3[1] as f64 / 3.0];
                let on_floor = floors.iter().any(|q| inside(q, p));
                let in_lip = lips.iter().any(|q| inside(q, p));
                let is_cut = cut(i, j, t);
                if on_floor && !is_cut {
                    wedge += 1;
                }
                if is_cut && !in_lip {
                    void += 1;
                }
            }
        }
    }
    (wedge, void)
}

/// K10 (absorb plan § 4.1 "Ground", reconciliation R5): along every ramp of the Switchbacks fixture,
/// no lattice cell where the canonical open-cut floor is under the ground the mask keeps (a wedge),
/// and none the mask cuts with no floor or lip under it (a void). The controls run the same
/// measure on the Stage A hole mask (one rect per open-cut segment along its chord), which leaves
/// wedges at the bends (the fail-first form), and on the requirements' one-rectangle-per-ramp
/// rule, which leaves voids.
///
/// The open-cut floors are the layout as the cook meshes it: K8 opens every skylight climb
/// (`sections::open_skylight_shafts`). Measured against the layout before K8 this test passed while
/// the climbs K8 opened past the cut sat under uncut ground (`west`, `mid_east`); against the K8
/// layout that cook leaves wedges there, and the cook that cuts the mouth from the meshed layout
/// leaves none (ADR-8898).
#[test]
fn ramp_bend_mask_has_no_wedge() {
    let ground = rolling_ground(N);
    let terrain = rolling_terrain();
    let out = cook_map("switchbacks", &ground, &terrain, "");
    let c = &out.file.canonical;
    let lg = GroundLattice::new(c);
    let layout = for_map("switchbacks").unwrap();
    let mut sampled = layout.sample(Some(&terrain));
    let opened = regia_world_geom::sections::open_skylight_shafts(&mut sampled, &layout.openings);
    println!("K8 opened {opened} skylight climb segments");
    assert!(opened > 0, "the fixture has skylight climbs for K8 to open");
    let (wedge, void) = wedges_and_voids(&c.lattice, &sampled, |i, j, t| lg.is_cut(i, j, t));
    println!("lattice cut: {wedge} wedge cells, {void} void cells");
    assert_eq!((wedge, void), (0, 0));
    let rects = one_rect_per_ramp(&sampled);
    let (old_wedge, old_void) = wedges_and_voids(&c.lattice, &sampled, |i, j, t| {
        let c3 = centroid3(&c.lattice, i, j, t);
        rects
            .iter()
            .any(|q| inside(q, [c3[0] as f64 / 3.0, c3[1] as f64 / 3.0]))
    });
    println!("control (one rect per ramp): {old_wedge} wedge cells, {old_void} void cells");
    assert!(
        old_wedge + old_void > 0,
        "the one-rect rule must leave wedges or voids at the bends"
    );
    // The Stage A hole mask: one oriented rect per open-cut segment, along the segment's chord.
    let holes = sampled.mouth_holes(&for_map("switchbacks").unwrap().openings);
    let (seg_wedge, seg_void) = wedges_and_voids(&c.lattice, &sampled, |i, j, t| {
        let c3 = centroid3(&c.lattice, i, j, t);
        let (x, z) = ((c3[0] as f64 / 3000.0) as f32, (c3[1] as f64 / 3000.0) as f32);
        holes.iter().any(|h| h.contains(x, z))
    });
    println!("control (Stage A, one rect per segment): {seg_wedge} wedge cells, {seg_void} void cells");
    assert!(
        seg_wedge > 0,
        "the one-rect-per-segment rule must leave wedges at the bends (the fail-first form)"
    );
}

/// Synthetic baked patches in DO27's shape (a lowered funnel round each mouth top on a 1 m grid,
/// unset outside, so the patch has a ragged edge), generated here from the rolling ground.
fn synthetic_patches(layout: &TunnelLayout) -> Vec<TerrainPatch> {
    layout
        .openings
        .iter()
        .map(|o| {
            let (cols, rows, spacing) = (41usize, 41usize, 1.0f32);
            let origin = [o.top[0] - 20.0, o.top[2] - 20.0];
            let heights = (0..cols * rows)
                .map(|k| {
                    let (c, r) = ((k % cols) as f32, (k / cols) as f32);
                    let (x, z) = (origin[0] + c * spacing, origin[1] + r * spacing);
                    let (dx, dz) = (x - o.top[0], z - o.top[2]);
                    let d = sqrt(dx * dx + dz * dz);
                    if d < 16.0 {
                        rolling_m(f64::from(x), f64::from(z)) as f32 - 0.5 - 2.0 * (1.0 - d / 16.0)
                    } else {
                        NONE_M
                    }
                })
                .collect();
            TerrainPatch {
                id: format!("{}_mouth", o.id),
                origin,
                spacing,
                cols,
                rows,
                heights,
            }
        })
        .collect()
}

/// The patch edges: midpoints between a set patch cell and an unset neighbor, with the unit normal
/// from the set cell to the unset one.
fn patch_edges(p: &TerrainPatch) -> Vec<([f32; 2], [f32; 2])> {
    let set = |c: usize, r: usize| p.heights[r * p.cols + c] > NONE_M + 1.0;
    let mut out = Vec::new();
    for r in 0..p.rows {
        for c in 0..p.cols {
            if !set(c, r) {
                continue;
            }
            for (dc, dr) in [(1i64, 0i64), (-1, 0), (0, 1), (0, -1)] {
                let (nc, nr) = (c as i64 + dc, r as i64 + dr);
                if nc < 0
                    || nr < 0
                    || nc >= p.cols as i64
                    || nr >= p.rows as i64
                    || set(nc as usize, nr as usize)
                {
                    continue;
                }
                let at = [
                    p.origin[0] + (c as f32 + dc as f32 * 0.5) * p.spacing,
                    p.origin[1] + (r as f32 + dr as f32 * 0.5) * p.spacing,
                ];
                out.push((at, [dc as f32, dr as f32]));
            }
        }
    }
    out
}

/// K9 (reconciliation R5): along every baked patch edge of the Switchbacks fixture the lattice
/// ground steps at most 1 mm (one lattice, no patches in the query). The control measures the same
/// edges on the Stage A sampler with the patches laid over it, which steps there (the fail-first
/// form).
#[test]
fn ground_is_continuous_across_former_patch_edges() {
    let ground = rolling_ground(N);
    let terrain = rolling_terrain();
    let layout = for_map("switchbacks").unwrap();
    let out = cook_map("switchbacks", &ground, &terrain, "");
    let lg = GroundLattice::new(&out.file.canonical);
    let patches = synthetic_patches(&layout);
    let patched = rolling_terrain().with_patches(patches.clone());
    let (mut worst_lattice, mut worst_compat, mut measured) = (0.0f32, 0.0f32, 0);
    for p in &patches {
        for (at, nrm) in patch_edges(p) {
            // The lattice: half a millimetre each side of the edge.
            let e = 0.0005;
            let (a, b) = (
                lg.ground_at(at[0] - nrm[0] * e, at[1] - nrm[1] * e),
                lg.ground_at(at[0] + nrm[0] * e, at[1] + nrm[1] * e),
            );
            if let (GroundAt::Ground(ya), GroundAt::Ground(yb)) = (a, b) {
                worst_lattice = regia_detsim::float::det_max(worst_lattice, (ya - yb).abs());
                measured += 1;
            }
            // Stage A: the patch's ragged edge against the grid, a quarter cell each side.
            let e = 0.3;
            let ya = patched.height_at_world(at[0] - nrm[0] * e, at[1] - nrm[1] * e);
            let yb = patched.height_at_world(at[0] + nrm[0] * e, at[1] + nrm[1] * e);
            worst_compat = regia_detsim::float::det_max(worst_compat, (ya - yb).abs());
        }
    }
    println!(
        "{measured} patch-edge points on the ground: lattice step {worst_lattice} m, Stage A sampler step {worst_compat} m"
    );
    assert!(measured > 100);
    assert!(worst_lattice <= 0.001, "lattice step {worst_lattice}");
    assert!(worst_compat > 0.001, "the control must step (fail-first form)");
}

/// Spec 121 § 3: the cell estimate needs at most one correction step on either axis.
#[test]
fn point_location_takes_at_most_one_step() {
    let ground = rolling_ground(N);
    let terrain = rolling_terrain();
    let out = cook_map("switchbacks", &ground, &terrain, "");
    let lg = GroundLattice::new(&out.file.canonical);
    let l = out.file.canonical.lattice;
    let mut worst = 0;
    // Every vertex position, each nudged one ulp either way, along both axes.
    for i in 0..N {
        let v = vertex_m(&l, i);
        for x in [
            v,
            f32::from_bits(v.to_bits().wrapping_add(1)),
            f32::from_bits(v.to_bits().wrapping_sub(1)),
        ] {
            if let Some(loc) = lg.locate(x, x) {
                worst = worst.max(loc.steps[0]).max(loc.steps[1]);
                assert!(x >= vertex_m(&l, loc.i) && (x < vertex_m(&l, loc.i + 1) || loc.i == N - 2));
            }
        }
    }
    assert!(worst <= 1, "{worst} steps");
}

/// The Washboard-sized synthetic map: 2050 m, 6561 samples (pitch 312.5 mm), 1 mm counts. Gentle
/// hills with two corrugated bands 40 m wide (2.5 m ripples, 0.25 m amplitude) running east-west.
const WB_N: u32 = 6561;
const WB_WIDTH_M: f32 = 2050.0;

fn washboard_m(x: f64, z: f64) -> f64 {
    let s = |v: f64| f64::from(sin(v as f32));
    let hills = 8.0 * s(x / 150.0) * f64::from(cos((z / 170.0) as f32)) + 0.002 * x;
    // The ripple phase is reduced to one period first, so the f32 sine stays accurate.
    let band = |zc: f64| {
        if (z - zc).abs() < 20.0 {
            0.25 * s(std::f64::consts::TAU * (x / 2.5).rem_euclid(1.0))
        } else {
            0.0
        }
    };
    hills + band(-400.0) + band(300.0)
}

fn washboard_ground() -> GroundInput {
    let n = WB_N;
    let mut counts = Vec::with_capacity((n * n) as usize);
    for j in 0..n {
        for i in 0..n {
            let x = -1025.0 + f64::from(i) * 2050.0 / f64::from(n - 1);
            let z = -1025.0 + f64::from(j) * 2050.0 / f64::from(n - 1);
            counts.push(((washboard_m(x, z) + 30.0) * 1000.0).round() as u16);
        }
    }
    GroundInput {
        lattice_n: n,
        width_m: WB_WIDTH_M,
        offset_m: -30.0,
        step_m: 0.001,
        counts,
    }
}

fn cook_ground_only(ground: &GroundInput, mode: &str) -> Result<CookOutput, CookError> {
    let doc = format!("(version: 1, ground_tile_cells: 64, {mode} layers: [])");
    let read = |p: &str| Err(format!("{p}: not found"));
    cook(&CookInputs {
        document: doc.as_bytes(),
        ground,
        terrain: None,
        read_file: &read,
    })
}

/// The full-pitch ground of `ground` as a canonical section (no budget check; the reference the
/// nested ground is measured against).
fn full_reference(ground: &GroundInput, levels: Vec<u8>) -> WorldGeomCooked {
    let lattice = ground.lattice(64).unwrap();
    WorldGeomCooked {
        canonical_version: regia_world_geom::cooked::CANONICAL_VERSION,
        settings: GeomSettings::default(),
        lattice,
        ground: GroundMode::Full,
        ground_counts: count_blocks(&lattice, &ground.counts, &levels),
        tile_levels: levels,
        hanging_overrides: Vec::new(),
        ring_pins: Vec::new(),
        tiles: Vec::new(),
        cut_bitmaps: Vec::new(),
        sections: Vec::new(),
        portals: Vec::new(),
        zones: Vec::new(),
        plugs: Vec::new(),
    }
}

/// Spec 121 § 13.2 (W4): a `Full` map over the budget fails with `ground_over_budget` naming the
/// `Nested { max_level }` that fits, and is not cooked; that mode then cooks within the budget.
/// The refusal is the bake check's (`CookError::Checks`), whose finding carries the mode (ADR-8436).
#[test]
fn full_mode_over_budget_names_the_nested_mode() {
    let ground = washboard_ground();
    let err = cook_ground_only(&ground, "").expect_err("a Full Washboard is over the budget");
    assert_eq!(err.code(), "ground_over_budget");
    let CookError::Checks(report) = &err else {
        panic!("expected the bake check's refusal: {err}");
    };
    println!("{err}");
    assert!(!report.passed);
    assert_eq!(report.ground_mode, "Full");
    let budget = ground_budget_bytes(f64::from(WB_WIDTH_M));
    assert!(report.memory_bytes > budget);
    let found = over_budget_finding(report);
    let Some(GroundMode::Nested { max_level }) = found.fits else {
        panic!("names no Nested mode: {err}");
    };
    assert!(
        found
            .measured
            .contains(&format!("Nested(max_level: {max_level})")),
        "{}",
        found.measured
    );
    let out = cook_ground_only(&ground, &format!("ground: Nested(max_level: {max_level}),")).unwrap();
    assert!(out.report.ground.sim_bytes <= out.report.ground.budget_bytes);
    assert_eq!(out.report.ground.budget_bytes, budget);
    if max_level > 1 {
        let finer = cook_ground_only(&ground, &format!("ground: Nested(max_level: {}),", max_level - 1));
        let finer = finer.err();
        assert_eq!(
            finer.as_ref().map(CookError::code),
            Some("ground_over_budget"),
            "the smallest that fits"
        );
        // A `Nested` refusal names no mode and lists the tiles it holds fine.
        if let Some(CookError::Checks(report)) = &finer {
            let found = over_budget_finding(report);
            assert_eq!(found.fits, None);
            assert!(!found.tiles.is_empty(), "{}", found.measured);
            assert!(found.tiles.windows(2).all(|w| w[0] < w[1]));
            assert!(found.tiles.len() <= GROUND_TILES_LISTED);
        } else {
            panic!("expected the bake check's refusal: {finer:?}");
        }
    }
}

/// The failed `ground_over_budget` finding of a refusal.
fn over_budget_finding(report: &TerrainBakeReport) -> &CheckFinding {
    report
        .checks
        .iter()
        .find(|c| c.code == "ground_over_budget" && !c.passed && !c.waived)
        .unwrap_or_else(|| panic!("ground_over_budget did not fail in {report}"))
}

const FULL_SPEED_M_PER_TICK: f32 = 2.0;

/// The largest per-tick change of (ground - full-pitch ground) along straight drives across the
/// map at [`FULL_SPEED_M_PER_TICK`] (an assumed 60 m/s at a 30 Hz tick).
fn worst_hop(nested: &GroundLattice<'_>, full: &GroundLattice<'_>) -> (f32, usize) {
    let mut worst = 0.0f32;
    let mut ticks = 0;
    let half = WB_WIDTH_M / 2.0 - 1.0;
    let mut drive = |from: [f32; 2], dir: [f32; 2]| {
        let mut prev: Option<f32> = None;
        let mut s = 0.0f32;
        while s < 2.0 * half {
            let (x, z) = (from[0] + dir[0] * s, from[1] + dir[1] * s);
            if let (GroundAt::Ground(a), GroundAt::Ground(b)) = (nested.ground_at(x, z), full.ground_at(x, z))
            {
                let e = a - b;
                if let Some(p) = prev {
                    worst = regia_detsim::float::det_max(worst, (e - p).abs());
                }
                prev = Some(e);
                ticks += 1;
            }
            s += FULL_SPEED_M_PER_TICK;
        }
    };
    for k in 0..12 {
        let o = -half + (k as f32 + 0.37) * (2.0 * half / 12.0);
        drive([o, -half], [0.0, 1.0]);
        drive([-half, o], [1.0, 0.0]);
        drive([-half, -half + o.abs() * 0.1], [0.8, 0.6]);
    }
    (worst, ticks)
}

/// Spec 121 § 13.3 (E1): on a Washboard-sized map authored `Nested`, the ground's departure from the
/// full-pitch ground changes by at most 5 cm per tick at full speed, pitch borders included. The
/// control cooks the fallback without the seam rule (one uniform level, no per-tile error, no
/// stitch: the review's superseded alternative) and fails the same bar (the fail-first form).
#[test]
fn pitch_seam_hop_at_full_speed_le_5cm_per_tick() {
    let ground = washboard_ground();
    let out = cook_ground_only(&ground, "ground: Nested(max_level: 6),").unwrap();
    let c = &out.file.canonical;
    let seam = out
        .report
        .ground
        .pitch_seam
        .expect("the cook ran ground_pitch_seam");
    println!(
        "levels: {:?}; seam: {seam:?}; {} hanging vertices; {} bytes of {}",
        histogram(&c.tile_levels),
        c.hanging_overrides.len(),
        out.report.ground.sim_bytes,
        out.report.ground.budget_bytes
    );
    assert!(
        seam.borders > 0 && seam.hanging > 0,
        "the fixture has pitch borders"
    );
    let full = full_reference(&ground, vec![0; c.tile_levels.len()]);
    let (nested_g, full_g) = (GroundLattice::new(c), GroundLattice::new(&full));
    let (hop, ticks) = worst_hop(&nested_g, &full_g);
    println!("seam rule: worst hop {hop} m over {ticks} ticks");
    assert!(hop <= 0.05, "hop {hop} m");

    let fallback = full_reference(&ground, vec![3; c.tile_levels.len()]);
    let (hop_fallback, _) = worst_hop(&GroundLattice::new(&fallback), &full_g);
    println!("fallback without the seam rule (uniform level 3): worst hop {hop_fallback} m");
    assert!(
        hop_fallback > 0.05,
        "the fallback must fail the bar (fail-first form)"
    );
}

fn histogram(levels: &[u8]) -> BTreeMap<u8, usize> {
    let mut h = BTreeMap::new();
    for &l in levels {
        *h.entry(l).or_default() += 1;
    }
    h
}

/// The `ground_pitch_seam` cook check (spec 121 § 10; E1 rules 1, 3 and 4): it passes on the cooked
/// Washboard, measures the step across every pitch border (at most 1 mm, C3 option (a)), and fails
/// on a section whose hanging vertex is off the coarse edge, on a two-level jump and on a tile
/// coarser than its error allows.
#[test]
fn ground_pitch_seam_check_measures_the_step() {
    let ground = washboard_ground();
    let out = cook_ground_only(&ground, "ground: Nested(max_level: 6),").unwrap();
    let c = out.file.canonical;
    let inputs = PitchInputs {
        pitch_error_m: c.settings.pitch_error_m,
        fine_band_m: c.settings.fine_band_m,
        footprints: Vec::new(),
    };
    let report = check_ground_pitch_seam(&c, &ground.counts, &inputs).unwrap();
    println!("{report:?}");
    assert!(report.max_step_m <= 0.001);

    // Without the stitch: the hanging vertices back at their own heights.
    let mut unstitched = c.clone();
    unstitched.hanging_overrides.clear();
    let err = check_ground_pitch_seam(&unstitched, &ground.counts, &inputs).unwrap_err();
    println!("unstitched: {err}");
    assert!(matches!(
        err,
        regia_world_geom::pitch::PitchSeamError::Step { .. }
    ));

    // A two-level jump.
    let per_side = c.lattice.tiles_per_side() as usize;
    let mut jump = c.clone();
    let t = (0..jump.tile_levels.len() - 1)
        .find(|&t| jump.tile_levels[t] >= 2 && jump.tile_levels[t + 1] >= 2 && (t + 1) % per_side != 0)
        .expect("two coarse neighbors");
    jump.tile_levels[t + 1] = jump.tile_levels[t] - 2;
    jump.ground_counts = count_blocks(&jump.lattice, &ground.counts, &jump.tile_levels);
    jump.hanging_overrides =
        regia_world_geom::pitch::hanging_overrides(&jump.lattice, &ground.counts, &jump.tile_levels);
    let err = check_ground_pitch_seam(&jump, &ground.counts, &inputs).unwrap_err();
    println!("jump: {err}");

    // A rippled tile forced coarse.
    let mut coarse = c.clone();
    let t = coarse
        .tile_levels
        .iter()
        .position(|&l| l == 0)
        .expect("a full-pitch tile");
    let mut levels = coarse.tile_levels.clone();
    levels[t] = 1;
    coarse.ground_counts = count_blocks(&coarse.lattice, &ground.counts, &levels);
    coarse.tile_levels = levels;
    let err = check_ground_pitch_seam(&coarse, &ground.counts, &inputs).unwrap_err();
    println!("coarse: {err}");
}

/// Spec 121 § 13.2: the cook is byte-stable, for a `Full` and a `Nested` fixture.
#[test]
fn cook_twice_gives_identical_bytes() {
    let ground = rolling_ground(N);
    let terrain = rolling_terrain();
    for mode in ["", "ground: Nested(max_level: 4),"] {
        let a = cook_map("switchbacks", &ground, &terrain, mode)
            .file
            .encode()
            .unwrap();
        let b = cook_map("switchbacks", &ground, &terrain, mode)
            .file
            .encode()
            .unwrap();
        assert_eq!(a, b, "{mode}");
        assert_eq!(blake3::hash(&a), blake3::hash(&b));
    }
}

/// A layer that fills a hole its layout has no mouth for is refused by name.
#[test]
fn filling_an_unknown_mouth_is_refused() {
    let ground = rolling_ground(65);
    let passages = PassagesDocument::from_layout(for_map("switchbacks").unwrap())
        .to_ron()
        .unwrap();
    let doc = "(version: 1, ground_tile_cells: 16, layers: [(id: \"tubes\", source: Passages(path: \"p\"), fills: [\"nope\"])])";
    let read = |_: &str| Ok(passages.clone().into_bytes());
    let err = cook(&CookInputs {
        document: doc.as_bytes(),
        ground: &ground,
        terrain: None,
        read_file: &read,
    })
    .err();
    assert_eq!(err.map(|e| e.code()), Some("mouth_unknown"));
}

/// The cooked ground is the same bytes on every runner (spec 121 § 13.2 byte stability): the
/// `content_hash` of the Switchbacks fixture cooked `Full` and `Nested(max_level: 4)` on the
/// synthetic rolling ground, recorded on x86-64 Linux. A different value on aarch64 (the Mac, the
/// Android test device, qemu-user) or Windows is a determinism bug in the cook.
#[test]
fn lattice_cook_hashes_are_pinned() {
    let ground = rolling_ground(N);
    let terrain = rolling_terrain();
    let hex = |h: [u8; 32]| h.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let mut got = Vec::new();
    let mut v1 = Vec::new();
    for mode in ["", "ground: Nested(max_level: 4),"] {
        let out = cook_map("switchbacks", &ground, &terrain, mode);
        got.push(hex(regia_world_geom::cooked::content_hash_of(
            &out.file.canonical,
        )
        .unwrap()));
        // The same value as v0.6.0 stored it (the version 1 section: `postcard(WorldGeomCooked)`,
        // whose BLAKE3 was `content_hash` until ADR-8816).
        let mut old = out.file.canonical.clone();
        old.canonical_version = 1;
        v1.push(hex(
            *blake3::hash(&postcard::to_allocvec(&old).unwrap()).as_bytes()
        ));
    }
    println!("content hashes: {got:?} (v1 layout: {v1:?})");
    // The cooked value did not move with the format: its version 1 bytes still hash to the pins
    // v0.6.0 shipped.
    assert_eq!(v1, PINNED_V1_HASHES);
    assert_eq!(got, PINNED_HASHES);
}

/// Recorded on aarch64 macOS (the Mac, rustc 1.99.1) and x86-64 Linux (the build server, rustc 1.99.0), lane
/// W-14c: the combined cook of integ-v06q-world (sections and viscells in the canonical, ADR-8421) and
/// ADR-8533's seal of the open skylight climbs (Switchbacks `west` and `mid_east`) changed the bytes.
/// Re-pinned by lane v06-section-yaw-canon (ADR-8579): section zone half extents cross through
/// `m_to_mm` (x86-64 Linux the test machine and aarch64 Android, the test Pixel 9 Pro; rustc 1.99.0). The
/// canonical zone yaw alone kept the W-14c bytes. Re-pinned by lane tunnels-stage-b-correctness
/// (ADR-8898, x86-64 Linux the test machine, rustc 1.99.0; aarch64 rerun owed): the mouths are cut from the
/// layout as meshed, so the whole skylight climb K8 opens (`west`, `mid_east`) is cut from the ground
/// (intended; the previous pins 4ee3fd6f and 11ff0ef2 cut only the authored open run). Earlier pins:
/// lane W-10 (first), lane W-14b (ADR-8334 and ADR-8396), lane W-14c (ADR-8533), lane
/// v06-section-yaw-canon (ADR-8579).
///
/// Since ADR-8816 (lane w11-index-build, cooked format v2) these are the hashes of the cooked value
/// in the version 1 section layout, which is what `content_hash` was through v0.6.0: the format
/// change left the cooked value alone.
const PINNED_V1_HASHES: [&str; 2] = [
    "d594e549cb6c04a7511a21b39f47a64de101eb33ff1a923d7ec9b3bdb5256953",
    "1fdd5b7ee9697284fdd2e60fc93909f8327cc015385d157f6e1627ad2f6dd958",
];

/// `content_hash` of the same two cooks in the version 2 section layout (ADR-8816: the BLAKE3 of
/// the hashed head, which holds the hashes of the count body). Re-pinned by lane w11-index-build
/// because the definition of the hash changed with the format, not the cook: `PINNED_V1_HASHES`
/// holds the unchanged value. Recorded on x86-64 Linux (the build server, rustc 1.99.0). Re-pinned by
/// integ-v07a (ADR-8895, x86-64 Linux the test machine, rustc 1.99.0; aarch64 rerun owed): the cook of
/// tunnels-stage-b-correctness (ADR-8898, the `PINNED_V1_HASHES` above) in this layout; lane
/// w11-index-build's 14087122 and 4412cda9 hashed the v0.6.0 cook.
const PINNED_HASHES: [&str; 2] = [
    "003cbe4b07093859e9b256603371e59d1e3d58e846e9921c320068b578f80d8e",
    "a96bc485375e6381b13d98b779d622036da144c6c375b9d735334da7cae20f9f",
];
