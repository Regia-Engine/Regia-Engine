// SPDX-License-Identifier: MPL-2.0
//! A terrain map matched to a cell-spaced simulation grid (ADR-8179, DO27's
//! REGIA_ADOPTION terrain-sim-grid request): spacing `world_width / n`, sample `n / 2` at the
//! world origin. The drawn surface, the collider, the holes and the runtime patches must all
//! sit on the simulation's samples, and the default origin must keep every map as it was.
//!
//! The simulation grid is described here from the request (positions only): sample `i` of `n`
//! at `(i − n / 2) · sim_width / n` on X and Z.

use regia_terrain::sampler::TerrainMapError;
use regia_terrain::{
    DirtyRect, HeightPatch, HoleShape, HolesMask, PatchTarget, TerrainHeightPatches, TerrainMap,
    resample_cell_grid_mm, terrain_surface_height,
};

/// World X (or Z) of simulation sample `i` of `n` over `sim_width` metres.
fn sim_x(i: u32, n: u32, sim_width: f32) -> f32 {
    (i as f32 - (n / 2) as f32) * (sim_width / n as f32)
}

/// Integer-indexed heights: exact in `f32`, different at every neighbour.
fn height_of(col: u32, row: u32) -> f32 {
    ((col * 7 + row * 13) % 97) as f32 * 0.25 - 4.0
}

/// A render map of `n × n` samples matched to an `n`-sample cell-spaced simulation grid over
/// `sim_width` metres (the recipe of `TerrainMap::with_grid_origin`).
fn matched_map(n: u32, sim_width: f32) -> TerrainMap {
    let heights = (0..n)
        .flat_map(|row| (0..n).map(move |col| height_of(col, row)))
        .collect();
    let world_width = sim_width * (n - 1) as f32 / n as f32;
    TerrainMap::from_heights(n, n, heights, world_width, 1.0, 0.0)
        .and_then(|m| m.with_grid_origin([-0.5 * sim_width / n as f32; 2]))
        .expect("a matched map")
}

/// DO27's numbers: 1280 m, 4096 samples. The sampler reads the stored sample, bit for bit, at
/// every simulation sample position it is asked about, and the grid maps those positions to
/// whole sample indices.
#[test]
fn a_cell_spaced_sim_grid_reads_the_stored_samples_bit_exact() {
    let (n, w) = (4096u32, 1280.0f32);
    let map = matched_map(n, w);
    assert_eq!(map.grid_origin(), [-0.15625, -0.15625]);
    assert_eq!(map.sample_spacing(), 0.3125);
    let lines = [0u32, 1, 17, 2047, 2048, 2049, 4000, 4095];
    let mut checked = 0u32;
    for &row in &lines {
        for col in 0..n {
            for (c, r) in [(col, row), (row, col)] {
                let (x, z) = (sim_x(c, n, w), sim_x(r, n, w));
                assert_eq!(map.sample_to_world(c, r), [x, z], "sample ({c}, {r}) position");
                assert_eq!(
                    map.world_to_grid(x, z),
                    [c as f32, r as f32],
                    "grid of ({x}, {z})"
                );
                let got = map.sample_bilinear(x, z);
                let want = map.height_at_sample(i64::from(c), i64::from(r));
                assert_eq!(got.to_bits(), want.to_bits(), "sample ({c}, {r}) at ({x}, {z})");
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 2 * lines.len() as u32 * n);
    // Sample n / 2 is the world origin.
    assert_eq!(map.sample_to_world(n / 2, n / 2), [0.0, 0.0]);
}

/// Without the origin the matched-spacing grid is half a sample toward +X and +Z: the shift DO27
/// measured (0.156 m on a 1280 m map at 4096 samples). With it the shift is gone.
#[test]
fn the_origin_removes_the_half_sample_shift() {
    let (n, w) = (4096u32, 1280.0f32);
    let heights = vec![0.0; (n * n) as usize];
    let unshifted =
        TerrainMap::from_heights(n, n, heights, w * (n - 1) as f32 / n as f32, 1.0, 0.0).expect("map");
    let [x, z] = unshifted.sample_to_world(n / 2, n / 2);
    // Half a sample (0.15625 m), up to the default grid's historical f32 rounding (0.1563110 m).
    assert!(
        (x - 0.15625).abs() < 1e-4 && (z - 0.15625).abs() < 1e-4,
        "the default grid's half-sample shift: {x}, {z}"
    );
    let matched = unshifted.with_grid_origin([-0.15625; 2]).expect("origin");
    assert_eq!(matched.sample_to_world(n / 2, n / 2), [0.0, 0.0]);
    assert_eq!(matched.world_min(), [-640.0, -640.0]);
    assert_eq!(matched.world_max(), [639.6875, 639.6875]);
}

/// The default origin is the mapping every earlier release used, bit for bit, and setting it
/// to `[0, 0]` explicitly changes nothing.
#[test]
fn the_default_origin_keeps_the_old_mapping_bit_for_bit() {
    for (n, w) in [
        (3u32, 4.0f32),
        (65, 64.0),
        (257, 1000.0),
        (513, 256.0),
        (2049, 1280.0),
    ] {
        let heights = (0..n)
            .flat_map(|row| (0..n).map(move |col| height_of(col, row)))
            .collect();
        let map = TerrainMap::from_heights(n, n, heights, w, 1.0, 0.0).expect("map");
        assert_eq!(map.grid_origin(), [0.0, 0.0]);
        let half = w * 0.5;
        for i in (0..n).step_by((n as usize / 7).max(1)).chain([n - 1]) {
            let legacy = i as f32 / (n - 1) as f32 * w - half;
            assert_eq!(
                map.sample_to_world(i, i)[0].to_bits(),
                legacy.to_bits(),
                "{n}: sample {i}"
            );
            let x = legacy + 0.37 * map.sample_spacing();
            let legacy_grid = (x + half) / w * (n - 1) as f32;
            assert_eq!(
                map.world_to_grid(x, x)[0].to_bits(),
                legacy_grid.to_bits(),
                "{n}: grid of {x}"
            );
        }
        let explicit = map.clone().with_grid_origin([0.0, 0.0]).expect("zero origin");
        for &(x, z) in &[
            (0.0f32, 0.0f32),
            (0.3, -0.7),
            (-half, half),
            (half * 0.9, half * 0.1),
        ] {
            assert_eq!(
                map.sample_bilinear(x, z).to_bits(),
                explicit.sample_bilinear(x, z).to_bits()
            );
            assert_eq!(
                terrain_surface_height(&map, map.sample_spacing(), x, z).to_bits(),
                terrain_surface_height(&explicit, explicit.sample_spacing(), x, z).to_bits()
            );
        }
        assert_eq!(map.collision_heightfield(None).center, [0.0, 0.0, 0.0]);
        assert_eq!(PatchTarget::of(&map).grid_origin, [0.0, 0.0]);
    }
}

/// A grid origin must be finite and the same on X and Z (the GPU folds it into one lane).
#[test]
fn a_bad_grid_origin_is_refused() {
    let map = || TerrainMap::from_heights(2, 2, vec![0.0; 4], 8.0, 1.0, 0.0).expect("map");
    for bad in [
        [f32::NAN, f32::NAN],
        [f32::INFINITY, f32::INFINITY],
        [1.0, 2.0],
        [0.0, -0.5],
    ] {
        assert!(
            matches!(
                map().with_grid_origin(bad),
                Err(TerrainMapError::BadGridOrigin(_))
            ),
            "{bad:?}"
        );
    }
    let moved = map().with_grid_origin([3.0, 3.0]).expect("origin");
    assert_eq!(
        moved.clone().grid_origin(),
        [3.0, 3.0],
        "a clone keeps the origin"
    );
}

/// The collider (CPU mirror of the physics heightfield placed at `center`) and the finest drawn
/// grid are the same triangles on a moved grid, and both read the stored sample exactly at
/// every simulation sample.
#[test]
fn the_collider_and_the_drawn_surface_agree_on_a_moved_grid() {
    let (n, w) = (256u32, 1280.0f32);
    let map = matched_map(n, w);
    let c = map.collision_heightfield(None);
    assert_eq!(c.center, [-2.5, 0.0, -2.5]);
    assert_eq!(c.scale, [1275.0, 1.0, 1275.0]);
    let step = map.sample_spacing();
    assert_eq!(step, 5.0);
    for row in (0..n).step_by(5) {
        for col in (0..n).step_by(3) {
            let (x, z) = (sim_x(col, n, w), sim_x(row, n, w));
            let exact = map.height_at_sample(i64::from(col), i64::from(row));
            // The collider mirror maps a world point to its cell in its own float order (as the
            // physics heightfield does), so it meets the sample within float rounding.
            let collider = c.height_at(x, z).expect("inside the moved field");
            // The same 1 mm the collider-at-sample checks of `collision.rs` allow.
            assert!(
                (collider - exact).abs() < 1e-3,
                "collider at sample ({col}, {row}): {collider} vs {exact}"
            );
            assert_eq!(
                terrain_surface_height(&map, step, x, z),
                exact,
                "drawn at sample ({col}, {row})"
            );
        }
    }
    // Between the samples: the same triangles (the diagonal from (x0, z1) to (x1, z0)), within
    // the 1 mm `collision.rs` allows; the same map on the world origin is measured alongside, so
    // a gap the origin opened would show as a difference between the two.
    let unmoved =
        TerrainMap::from_heights(n, n, map.heights().to_vec(), map.world_width(), 1.0, 0.0).expect("map");
    let unmoved_c = unmoved.collision_heightfield(None);
    let (mut worst, mut worst_unmoved) = (0.0f32, 0.0f32);
    for k in 0..4000u32 {
        let fx = (k.wrapping_mul(2_654_435_761) % 100_000) as f32 / 100_000.0;
        let fz = (k.wrapping_mul(40_503) % 100_000) as f32 / 100_000.0;
        let x = -640.0 + fx * 1274.9;
        let z = -640.0 + fz * 1274.9;
        let collider = c.height_at(x, z).expect("inside the moved field");
        let drawn = terrain_surface_height(&map, step, x, z);
        worst = worst.max((collider - drawn).abs());
        let (ux, uz) = (x + 2.5, z + 2.5);
        let collider = unmoved_c.height_at(ux, uz).expect("inside the unmoved field");
        worst_unmoved = worst_unmoved.max((collider - terrain_surface_height(&unmoved, step, ux, uz)).abs());
    }
    eprintln!("collider vs drawn: moved grid {worst} m, same map on the world origin {worst_unmoved} m");
    assert!(worst < 1e-3, "collider vs drawn surface: {worst} m");
    // The moved field ends where the moved map ends.
    assert!(c.height_at(-640.0, -640.0).is_some());
    assert!(c.height_at(-640.1, 0.0).is_none(), "west of the moved map");
    assert!(c.height_at(635.0, 635.0).is_some());
    assert!(c.height_at(635.1, 0.0).is_none(), "east of the moved map");
    // Chunk colliders move with it and still tile the map.
    let chunk = c.chunk(64, [0, 0]).expect("chunk");
    assert_eq!(chunk.center[0] - chunk.scale[0] * 0.5, -640.0);
    assert_eq!(chunk.center[2] - chunk.scale[2] * 0.5, -640.0);
}

/// A hole cut over a simulation-grid rectangle cuts exactly that rectangle's samples on the
/// collider, and the collider removes exactly the cells touching them.
#[test]
fn a_hole_at_a_sim_grid_rectangle_cuts_exactly_those_samples() {
    let (n, w) = (256u32, 1280.0f32);
    let map = matched_map(n, w);
    let sp = w / n as f32;
    let (i0, i1, k0, k1) = (120u32, 131u32, 60u32, 66u32);
    // The rectangle reaches a quarter sample past the outermost samples it holds: the next
    // samples out are three quarters of a sample away, past the collider's half-sample reach.
    let (x0, x1) = (sim_x(i0, n, w) - 0.25 * sp, sim_x(i1, n, w) + 0.25 * sp);
    let (z0, z1) = (sim_x(k0, n, w) - 0.25 * sp, sim_x(k1, n, w) + 0.25 * sp);
    let mask = HolesMask::from_shapes(vec![HoleShape::Rect {
        center: [(x0 + x1) * 0.5, (z0 + z1) * 0.5],
        half_extents: [(x1 - x0) * 0.5, (z1 - z0) * 0.5],
        rotation_radians: 0.0,
    }])
    .expect("mask");
    let c = map.collision_heightfield(Some(&mask));
    let cut = |col: u32, row: u32| c.heights[row as usize + col as usize * c.nx].is_nan();
    let mut wrong = Vec::new();
    for row in 0..n {
        for col in 0..n {
            let inside = (i0..=i1).contains(&col) && (k0..=k1).contains(&row);
            if cut(col, row) != inside {
                wrong.push((col, row));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "samples cut wrongly (first 8): {:?}",
        &wrong[..wrong.len().min(8)]
    );
    assert_eq!(c.cut_samples, ((i1 - i0 + 1) * (k1 - k0 + 1)) as usize);
    // Cell (i, k) spans samples i..=i+1, k..=k+1: removed when any corner is cut.
    for k in (k0 - 3)..(k1 + 3) {
        for i in (i0 - 3)..(i1 + 3) {
            let centre = (sim_x(i, n, w) + 0.5 * sp, sim_x(k, n, w) + 0.5 * sp);
            let removed =
                (i0.saturating_sub(1)..=i1).contains(&i) && (k0.saturating_sub(1)..=k1).contains(&k);
            assert_eq!(
                c.height_at(centre.0, centre.1).is_none(),
                removed,
                "cell ({i}, {k})"
            );
        }
    }
}

/// A world-space rectangle on the simulation grid names the same samples on the render map,
/// and a patch over them writes exactly those samples.
#[test]
fn a_world_space_patch_lands_on_the_sim_samples() {
    let (n, w) = (256u32, 1280.0f32);
    let mut map = matched_map(n, w);
    let target = PatchTarget::of(&map);
    assert_eq!(target.grid_origin, [-2.5, -2.5]);
    let (i0, i1, k0, k1) = (40u32, 52u32, 200u32, 203u32);
    let min = [sim_x(i0, n, w), sim_x(k0, n, w)];
    let max = [sim_x(i1, n, w), sim_x(k1, n, w)];
    let rect = target.samples_within(min, max);
    assert_eq!(
        rect,
        DirtyRect {
            col_start: i0,
            col_end: i1 + 1,
            row_start: k0,
            row_end: k1 + 1,
        }
    );
    assert_eq!(
        target.footprint(&rect),
        (min, max),
        "the footprint is the world box back"
    );
    let queue = TerrainHeightPatches::default();
    queue.set_target(target);
    let (pw, ph) = (rect.col_end - rect.col_start, rect.row_end - rect.row_start);
    queue
        .request(HeightPatch {
            col: rect.col_start,
            row: rect.row_start,
            width: pw,
            height: ph,
            heights: vec![0.5; (pw * ph) as usize],
        })
        .expect("queued");
    let before = map.clone();
    for band in queue.take_frame() {
        band.apply(&mut map).expect("same grid");
    }
    for row in 0..n {
        for col in 0..n {
            let inside = (i0..=i1).contains(&col) && (k0..=k1).contains(&row);
            let (now, was) = (
                map.height_at_sample(i64::from(col), i64::from(row)),
                before.height_at_sample(i64::from(col), i64::from(row)),
            );
            if inside {
                assert_eq!(
                    now,
                    target.dequantize(target.quantize(0.5)),
                    "patched ({col}, {row})"
                );
            } else {
                assert_eq!(now.to_bits(), was.to_bits(), "untouched ({col}, {row})");
            }
        }
    }
    // A box between two samples holds none.
    let between = [sim_x(i0, n, w) + 0.1, sim_x(k0, n, w) + 0.1];
    assert!(
        target
            .samples_within(between, [between[0] + 1.0, between[1] + 1.0])
            .is_empty()
    );
}

/// The integer resample rule: DO27's 256-sample, 5 m simulation grid on its 4096-sample render
/// map. Render samples on coarse samples copy them; the rest are the integer bilinear.
#[test]
fn a_coarse_crater_resamples_bit_exact_at_the_coarse_samples() {
    let (nc, nr) = (256u32, 4096u32);
    // A crater: a paraboloid bowl in millimetres, with an odd ripple so no two neighbours agree.
    let coarse: Vec<i32> = (0..nc)
        .flat_map(|r| {
            (0..nc).map(move |c| {
                let (dx, dz) = (c as i32 - 100, r as i32 - 90);
                -5000 + 37 * (dx * dx + dz * dz) + (c as i32 * 13 + r as i32 * 7) % 11
            })
        })
        .collect();
    // A render rectangle around the crater, ending on the clamped east border.
    let rect = DirtyRect {
        col_start: 1500,
        col_end: nr,
        row_start: 1400,
        row_end: 1500,
    };
    let out = resample_cell_grid_mm(&coarse, nc, nr, &rect).expect("resampled");
    let width = (rect.col_end - rect.col_start) as usize;
    assert_eq!(out.len(), width * (rect.row_end - rect.row_start) as usize);
    let at =
        |col: u32, row: u32| out[(row - rect.row_start) as usize * width + (col - rect.col_start) as usize];
    let coarse_at = |c: u32, r: u32| coarse[(r * nc + c) as usize];
    let mut copies = 0;
    for row in rect.row_start..rect.row_end {
        for col in rect.col_start..rect.col_end {
            if col % 16 == 0 && row % 16 == 0 {
                assert_eq!(
                    at(col, row),
                    coarse_at(col / 16, row / 16),
                    "coincident ({col}, {row})"
                );
                copies += 1;
            }
        }
    }
    assert!(copies > 100, "{copies} coincident samples");
    // Halfway along X between coarse samples 100 and 101 (render column 1608) on coarse row 88
    // (render row 1408): the mean, rounded half up.
    let (a, b) = (coarse_at(100, 88), coarse_at(101, 88));
    assert_eq!(
        at(1608, 1408),
        (a + b).div_euclid(2) + i32::from((a + b).rem_euclid(2) == 1)
    );
    // A quarter of the way along Z between coarse rows 88 and 89 at coarse column 100.
    let (p, q) = (i64::from(coarse_at(100, 88)), i64::from(coarse_at(100, 89)));
    let want = (2 * (3 * p + q) + 4).div_euclid(8);
    assert_eq!(i64::from(at(1600, 1412)), want);
    // Past the last coarse sample (render 4080 = coarse 255) the border repeats.
    for col in 4080..nr {
        assert_eq!(at(col, 1408), coarse_at(255, 88), "clamped column {col}");
    }
    // The same rule through the map: the matched render map and the coarse grid agree in world
    // space at every coincident sample.
    let render = matched_map(64, 1280.0);
    let sim = matched_map(4, 1280.0);
    for i in 0..4u32 {
        let ([rx, rz], [sx, sz]) = (render.sample_to_world(16 * i, 16 * i), sim.sample_to_world(i, i));
        assert!(
            (rx - sx).abs() < 1e-3 && (rz - sz).abs() < 1e-3,
            "sample {i}: {rx} vs {sx}"
        );
    }
}

/// Bad inputs are refused with the patch errors.
#[test]
fn the_resample_refuses_bad_grids_and_rects() {
    let rect = |c0, c1, r0, r1| DirtyRect {
        col_start: c0,
        col_end: c1,
        row_start: r0,
        row_end: r1,
    };
    assert!(
        resample_cell_grid_mm(&[0; 3], 2, 8, &rect(0, 1, 0, 1)).is_err(),
        "short coarse grid"
    );
    assert!(
        resample_cell_grid_mm(&[], 0, 8, &rect(0, 1, 0, 1)).is_err(),
        "zero side"
    );
    assert!(
        resample_cell_grid_mm(&[0; 4], 2, 8, &rect(0, 0, 0, 1)).is_err(),
        "empty rect"
    );
    assert!(
        resample_cell_grid_mm(&[0; 4], 2, 8, &rect(4, 9, 0, 1)).is_err(),
        "rect leaves the map"
    );
    let flat = resample_cell_grid_mm(&[7; 4], 2, 8, &rect(0, 8, 0, 8)).expect("flat");
    assert!(flat.iter().all(|&h| h == 7), "a flat grid stays flat: {flat:?}");
}
