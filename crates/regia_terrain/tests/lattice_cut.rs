// SPDX-License-Identifier: MPL-2.0
//! Lane W-10 (spec 121 § 3, spec 26 § 2a.4, ADR-8214): the render side of the one ground. The sim's
//! lattice is the drawn grid, and a mouth's render hole is its lattice cut, cell for cell.
//!
//! Fixtures: DO27's Switchbacks layout (`regia_world_geom`'s `fixtures` feature) on flat synthetic
//! ground at the canyon floor; no DO27 map data.

// Test fixtures panic on a broken fixture.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use regia_terrain::{HoleShape, HolesMask, NamedHole, TerrainMap};
use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookError, CookInputs, CookOutput, GroundInput, cook};
use regia_world_geom::cut::{centroid3, neighbor};
use regia_world_geom::ground::{GroundLattice, vertex_m};
use regia_world_geom::passages::PassagesDocument;

/// DO27's drawn grid: 4096 samples over 1280 m.
const N: u32 = 4096;
const WIDTH_M: f32 = 1280.0;

fn canyon_terrain() -> TerrainSampler {
    TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), WIDTH_M).with_zero_counts(16384)
}

fn canyon_ground() -> GroundInput {
    GroundInput {
        lattice_n: N,
        width_m: WIDTH_M,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![8192; (N * N) as usize],
    }
}

fn cook_switchbacks(ground: &GroundInput, terrain: &TerrainSampler) -> CookOutput {
    let layout = for_map("switchbacks").unwrap();
    let mouths = regia_world_geom::cut::passage_mouths(&layout, &layout.sample(Some(terrain)));
    let fills: Vec<String> = mouths.iter().map(|m| format!("{:?}", m.id)).collect();
    let doc = format!(
        "(version: 1, ground_tile_cells: 64, layers: [(id: \"tubes\", source: Passages(path: \"p\"), fills: [{}])])",
        fills.join(", ")
    );
    let passages = PassagesDocument::from_layout(layout).to_ron().unwrap();
    let read = |_: &str| Ok(passages.clone().into_bytes());
    let run = |document: &str| {
        cook(&CookInputs {
            document: document.as_bytes(),
            ground,
            terrain: Some(terrain),
            read_file: &read,
        })
    };
    // ADR-8359: a test cook waives the two known Switchbacks ramp facets by hash and recooks (the
    // W-09 checks met this W-10 cook in the sim merge; ADR-8436).
    match run(&doc) {
        Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
            run(&waive_ramp_facets(&doc, &report).unwrap())
        }
        other => other,
    }
    .unwrap()
}

/// Plan § 4.3 / spec 121 § 13.3: the sim ground's sample at the origin against the drawn sample it
/// nests in. On the base the sim reads DO27's `i / n` 5 m grid (the Stage A sampler: sample 128 at
/// 0.0 m) while the drawn grid is `i / (n - 1)` (sample 2048 at 0.156 m): a 0.156 m shift, the
/// half texel. The lattice ground reads the drawn grid's own sample, within 0.5 mm (its integer
/// millimetre), so the shift reads 0 mm.
#[test]
fn visual_grid_shift_is_the_half_texel_at_the_origin() {
    let map = TerrainMap::from_heights(N, N, vec![0.0; (N * N) as usize], WIDTH_M, 1.0, 0.0).unwrap();
    let drawn = map.sample_to_world(N / 2, N / 2)[0];

    // The base: the Stage A sampler's sample at the origin (256 samples, 5 m, `i / n`), which the
    // drawn grid nests 16 to 1.
    let stage_a = canyon_terrain();
    let base_sim = -stage_a.half_x + 128.0 * (WIDTH_M / 256.0);
    let base_shift = (drawn - base_sim).abs();

    let out = cook_switchbacks(&canyon_ground(), &canyon_terrain());
    let lattice = out.file.canonical.lattice;
    let lattice_sim = vertex_m(&lattice, N / 2);
    let shift = (drawn - lattice_sim).abs();
    let shift_mm = (f64::from(shift) * 1000.0).round();
    println!(
        "drawn sample {drawn} m; base sim sample {base_sim} m (shift {base_shift} m); lattice vertex {lattice_sim} m (shift {shift} m = {shift_mm} mm)"
    );
    assert!(
        (base_shift - 0.156).abs() < 0.001,
        "the base reads the half texel"
    );
    assert_eq!(shift_mm, 0.0, "the lattice ground reads the drawn sample");
    // And so does every vertex of the row: the integer millimetre is within 0.5 mm of the exact
    // `-W/2 + i * W / (n - 1)`, and the drawn `f32` position within that plus its own `f32`
    // rounding (under 0.031 mm at 640 m).
    for i in (0..N).step_by(97).chain([N - 1]) {
        let exact = -640.0 + f64::from(i) * 1280.0 / f64::from(N - 1);
        let v = f64::from(vertex_m(&lattice, i));
        let drawn = f64::from(map.sample_to_world(i, 0)[0]);
        assert!(
            (v - exact).abs() <= 0.0005 + 0.000031,
            "vertex {i}: {} m off the exact position",
            (v - exact).abs()
        );
        assert!(
            (drawn - v).abs() <= 0.0005 + 2.0 * 0.000031,
            "vertex {i}: {} m off the drawn sample",
            (drawn - v).abs()
        );
    }
}

/// Plan W-10: the cooked cut bitmap and the render `HolesMask` built from the mouths' rings agree
/// cell for cell: every lattice triangle near a mouth is cut in the bitmap exactly when the render
/// mask contains its centroid. A collar triangle is only ever a cut triangle that touches a ring.
/// The seal (ADR-8334's filtered collar, ADR-8396's and ADR-8533's approach strips, the zipper
/// bands) no longer writes every ring cell, so the ring check is spec 121 § 3's rule (the ring
/// pin, item 3; ADR-8533): every ring edge is an edge of exactly one canonical triangle, at the
/// ring pins' millimetre heights.
#[test]
fn mouth_holes_match_the_render_mask() {
    let ground = canyon_ground();
    let out = cook_switchbacks(&ground, &canyon_terrain());
    let c = &out.file.canonical;
    let l = c.lattice;
    let lg = GroundLattice::new(c);
    let mut mask = HolesMask::default();
    let mut lo = [u32::MAX; 2];
    let mut hi = [0u32; 2];
    for mouth in &out.report.ground.mouths {
        let loops: Vec<Vec<[f32; 2]>> = mouth
            .rings
            .iter()
            .map(|ring| {
                ring.iter()
                    .map(|&(i, j)| {
                        lo = [lo[0].min(i), lo[1].min(j)];
                        hi = [hi[0].max(i), hi[1].max(j)];
                        [vertex_m(&l, i), vertex_m(&l, j)]
                    })
                    .collect()
            })
            .collect();
        mask.set_named(NamedHole {
            id: mouth.id.clone(),
            open: true,
            shape: HoleShape::lattice_rings(&loops).unwrap(),
        })
        .unwrap();
    }
    assert!(!out.report.ground.mouths.is_empty());
    // The collar's XZ footprints (world mm), from the canonical triangles of the layer past its
    // mesh triangles.
    let mesh_count = out.report.layers[0].triangles
        - out
            .report
            .ground
            .mouths
            .iter()
            .map(|m| m.collar_triangles)
            .sum::<u32>();
    let mut collar: BTreeMap<[[i64; 2]; 3], u32> = BTreeMap::new();
    for tile in &c.tiles {
        let [ox, oz] = tile.origin_mm;
        for t in tile.triangles.iter().filter(|t| t.index >= mesh_count) {
            let key =
                t.v.map(|p| [i64::from(ox) + i64::from(p[0]), i64::from(oz) + i64::from(p[2])]);
            *collar.entry(key).or_default() += 1;
        }
    }
    let cells = l.cells();
    let (mut checked, mut cut, mut ring_cells, mut mismatches) = (0, 0, 0, 0);
    for j in lo[1].saturating_sub(4)..(hi[1] + 4).min(cells) {
        for i in lo[0].saturating_sub(4)..(hi[0] + 4).min(cells) {
            for t in 0..2u8 {
                let c3 = centroid3(&l, i, j, t);
                let (x, z) = ((c3[0] as f64 / 3000.0) as f32, (c3[1] as f64 / 3000.0) as f32);
                let in_mask = mask.contains(x, z);
                let in_cut = lg.is_cut(i, j, t);
                checked += 1;
                if in_mask != in_cut {
                    mismatches += 1;
                }
                let touches_ring = in_cut
                    && (0..3u8).any(|e| {
                        neighbor(cells, i, j, t, e).is_none_or(|(ni, nj, nt)| !lg.is_cut(ni, nj, nt))
                    });
                cut += u32::from(in_cut);
                ring_cells += u32::from(touches_ring);
                let v = regia_world_geom::cut::tri_vertices(i, j, t)
                    .map(|(vi, vj)| [l.vertex_mm(vi), l.vertex_mm(vj)]);
                let written = collar.get(&v).copied().unwrap_or(0);
                assert!(
                    written <= u32::from(touches_ring),
                    "cell ({i}, {j}, {t}): {written} collar triangles; a collar triangle is a cut triangle on a ring, once"
                );
            }
        }
    }
    // Spec 121 § 3, the ring pin, item 3: every ring edge (axis edge or diagonal) is an edge of exactly
    // one canonical triangle, with the ring pin heights (item 2), so no T-junction, no sliver and no gap.
    let pins: BTreeMap<u32, i32> = c.ring_pins.iter().map(|p| (p.vertex, p.height_mm)).collect();
    type Edge = ([i64; 3], [i64; 3]);
    let key = |a: [i64; 3], b: [i64; 3]| -> Edge { if a <= b { (a, b) } else { (b, a) } };
    let mut edges: BTreeMap<Edge, u32> = BTreeMap::new();
    for tile in &c.tiles {
        let [ox, oz] = tile.origin_mm;
        for t in &tile.triangles {
            let v = t.v.map(|p| {
                [
                    i64::from(ox) + i64::from(p[0]),
                    i64::from(p[1]),
                    i64::from(oz) + i64::from(p[2]),
                ]
            });
            for e in 0..3 {
                *edges.entry(key(v[e], v[(e + 1) % 3])).or_default() += 1;
            }
        }
    }
    let (mut ring_edges, mut bad) = (0u32, Vec::new());
    for mouth in &out.report.ground.mouths {
        for ring in &mouth.rings {
            let world = |(i, j): (u32, u32)| -> [i64; 3] {
                let pin = pins
                    .get(&(j * l.lattice_n + i))
                    .copied()
                    .unwrap_or_else(|| panic!("ring vertex ({i}, {j}) of `{}` has no ring pin", mouth.id));
                [l.vertex_mm(i), i64::from(pin), l.vertex_mm(j)]
            };
            for k in 0..ring.len() {
                let (a, b) = (ring[k], ring[(k + 1) % ring.len()]);
                ring_edges += 1;
                let n = edges.get(&key(world(a), world(b))).copied().unwrap_or(0);
                if n != 1 {
                    bad.push((mouth.id.clone(), a, b, n));
                }
            }
        }
    }
    println!(
        "{checked} lattice triangles checked, {cut} cut, {ring_cells} on a ring, {mismatches} mask mismatches, {} collar triangles, {ring_edges} ring edges, {} not on exactly one canonical triangle",
        collar.len(),
        bad.len()
    );
    assert_eq!(mismatches, 0);
    assert!(ring_edges > 0);
    assert!(
        bad.is_empty(),
        "ring edges not an edge of exactly one canonical triangle (mouth, a, b, triangles): {:?}",
        &bad[..bad.len().min(12)]
    );
}

/// The CDLOD quadtree depth that draws every sample at level 0 (the shared draw's rule, as
/// `level_terrain.rs` computes it).
fn root_level_for(n: u32, grid_size: u32) -> u32 {
    let cells = (n - 1) as f32 / grid_size as f32;
    let mut root = 0u32;
    while root < 12 && ((1u32 << root) as f32) < cells {
        root += 1;
    }
    root
}

/// Spec 121 § 13.2 (W3) and spec 26 § 2a.4 item 1: the sim lattice ignores cliff relief. The cook
/// reads the heightfield's samples and the lattice constants only, so the canonical section is the
/// same byte for byte whatever `TerrainRelief::cliff_relief` the renderer draws with; and the drawn
/// grids nest in the lattice: CDLOD's level-0 vertex `k` and the cliff-relief dense grid's vertex
/// `2k` are lattice vertex `k` within 0.5 mm, on a map whose `n - 1` is a multiple of the node grid.
/// It also prints the level-0 pitch against the lattice pitch for DO27's 4096-sample maps, where
/// they differ (`n - 1 = 4095` is not `2^root * grid_size`; see the lane report).
#[test]
fn lattice_pitch_ignores_cliff_relief() {
    let n = 1025u32;
    let counts: Vec<u16> = (0..n * n)
        .map(|k| {
            let (i, j) = (k % n, k / n);
            (8192 + ((i * 7 + j * 13) % 97) * 3) as u16
        })
        .collect();
    let ground = GroundInput {
        lattice_n: n,
        width_m: WIDTH_M,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts,
    };
    let mut sections = Vec::new();
    for cliff_relief in [0.0f32, 1.0] {
        let relief = regia_terrain::TerrainRelief {
            cliff_relief,
            ..Default::default()
        };
        // The renderer's knob reaches no cook input: the cook reads the ground and the document.
        assert_eq!(relief.cliff_relief, cliff_relief);
        let doc = "(version: 1, ground_tile_cells: 64, ground: Nested(max_level: 3), layers: [])";
        let read = |p: &str| Err(format!("{p}: not found"));
        let out = cook(&CookInputs {
            document: doc.as_bytes(),
            ground: &ground,
            terrain: None,
            read_file: &read,
        })
        .unwrap();
        sections.push(regia_world_geom::cooked::encode_canonical(&out.file.canonical).unwrap());
    }
    assert_eq!(sections[0], sections[1]);

    let grid = regia_terrain::DEFAULT_GRID_SIZE;
    let lattice = ground.lattice(64).unwrap();
    let root = root_level_for(n, grid);
    let leaf = regia_terrain::node_world_size(WIDTH_M, root, 0);
    for k in (0..n).step_by(31).chain([n - 1]) {
        let lod0 = -WIDTH_M / 2.0 + k as f32 * (leaf / grid as f32);
        let dense = -WIDTH_M / 2.0 + (2 * k) as f32 * (leaf / (2 * grid) as f32);
        let v = vertex_m(&lattice, k);
        assert!((lod0 - v).abs() <= 0.0005, "level-0 vertex {k}: {lod0} vs {v}");
        assert!(
            (dense - v).abs() <= 0.0005,
            "dense vertex {}: {dense} vs {v}",
            2 * k
        );
    }
    let root_4096 = root_level_for(4096, grid);
    // ADR-8291: the full-detail root on 4096 samples draws level 0 at the lattice pitch
    // W/(n-1), not W/n. The base formula (node_world_size(WIDTH_M, 8, 0) / 16) was 0.3125 m
    // against the lattice's 1280/4095 m.
    let span_4096 = regia_terrain::quadtree_span(WIDTH_M, 4096, grid, root_4096);
    let pitch_4096 = regia_terrain::node_world_size(span_4096, root_4096, 0) / grid as f32;
    let lattice_4096 = WIDTH_M / 4095.0;
    assert!(
        (pitch_4096 - lattice_4096).abs() <= 1.0e-6,
        "n = 4096 level-0 pitch {pitch_4096} m, lattice pitch {lattice_4096} m"
    );
    println!(
        "n = 1025: level-0 pitch {} m = lattice pitch {} m; n = 4096: level-0 pitch {pitch_4096} m, lattice pitch {lattice_4096} m",
        leaf / grid as f32,
        WIDTH_M / (n - 1) as f32,
    );
}
