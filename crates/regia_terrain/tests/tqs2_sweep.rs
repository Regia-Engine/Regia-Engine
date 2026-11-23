// SPDX-License-Identifier: MPL-2.0
//! Test-quality sweep, second set (lane test-quality-sweep-2): deterministic tests for public
//! CPU-side `regia_terrain` functions that no test referenced before. No GPU, no goldens.

use regia_asset::{
    AssetId, COOKED_FORMAT_VERSION, CookedHeader, Heightfield, decode_cooked_file, encode_cooked_file,
};
use regia_core::id::TypePathHash;
use regia_render::RenderProfilePreset;
use regia_terrain::brush::BrushMode;
use regia_terrain::brush::raster::apply_hardness;
use regia_terrain::cdlod::{MinMaxPyramid, QuadNode, vertex_morph_t};
use regia_terrain::grass::select::{
    MAX_CANDIDATES_PER_FINE_PATCH, candidates_per_fine_patch, patch_hash, pcg_hash,
};
use regia_terrain::holes::{HoleEllipse, HoleShape, HolesMask, NamedHole, TerrainHoles};
use regia_terrain::runtime_patch::PatchTarget;
use regia_terrain::scatter::impostor::ImpostorSettings;
use regia_terrain::scatter::size_intent::uniform_scale;
use regia_terrain::scatter::{ScatterClass, ScatterMeshRef, ScatterRule, ScatterSet};
use regia_terrain::stream::{
    CookedHeightfieldTile, CookedTiledHeightfield, TERRAIN_INDEX_FORMAT_VERSION, TERRAIN_INDEX_TYPE_PATH,
    TILED_HEIGHTFIELD_FORMAT_VERSION, TILED_HEIGHTFIELD_TYPE_PATH, TerrainIndex, TerrainIndexError,
    decode_cooked_tiled_heightfield, load_terrain_index_bytes, load_tiled_heightfield_bytes,
};
use regia_terrain::water::bed_slope;
use regia_terrain::water_body::ocean::{OceanTables, frame_of};
use regia_terrain::water_body::swell::{SwellSample, rest_point_by, ring_fade_cell};
use regia_terrain::water_body::{FftOcean, Swell};
use regia_terrain::{TerrainMap, layer_mip_skip_for, ridged_fbm};

fn target() -> PatchTarget {
    PatchTarget {
        width: 9,
        height: 9,
        height_scale: 200.0,
        height_offset: -50.0,
        world_width: 64.0,
        world_depth: 64.0,
        grid_origin: [0.0; 2],
    }
}

/// `quantize` maps the height range onto 0..=65535, clamps outside it, rounds to the nearest
/// step, and sends NaN to 0 instead of panicking.
#[test]
fn patch_quantize_maps_clamps_and_rounds() {
    let t = target();
    assert_eq!(t.quantize(-50.0), 0);
    assert_eq!(t.quantize(150.0), 65535);
    assert_eq!(t.quantize(-1000.0), 0);
    assert_eq!(t.quantize(1.0e9), 65535);
    assert_eq!(t.quantize(f32::NAN), 0);
    let step = 200.0 / 65535.0;
    assert_eq!(t.quantize(-50.0 + 10.4 * step), 10);
    assert_eq!(t.quantize(-50.0 + 10.6 * step), 11);
    // A zero height scale has no range: every height is sample 0.
    let flat = PatchTarget {
        height_scale: 0.0,
        ..target()
    };
    assert_eq!(flat.quantize(123.0), 0);
}

/// `dequantize` is the inverse of `quantize` on every sample (bit for bit what loading gives),
/// and a height survives a round trip within half a step.
#[test]
fn patch_dequantize_inverts_quantize() {
    let t = target();
    for s in [0u16, 1, 2, 777, 32767, 65534, 65535] {
        assert_eq!(t.quantize(t.dequantize(s)), s, "sample {s}");
    }
    assert_eq!(t.dequantize(0), -50.0);
    assert_eq!(t.dequantize(65535), 150.0);
    let half_step = 0.5 * 200.0 / 65535.0;
    for h in [-49.9f32, 0.0, 12.345, 99.99] {
        let back = t.dequantize(t.quantize(h));
        assert!((back - h).abs() <= half_step * 1.01, "{h} -> {back}");
    }
    // The same mapping the map loader uses.
    let hf = Heightfield {
        width: 2,
        height: 2,
        samples: vec![0, 1, 32767, 65535],
    };
    let map = TerrainMap::new(&hf, 64.0, 200.0, -50.0).expect("map");
    let pt = PatchTarget::of(&map);
    for (i, &s) in hf.samples.iter().enumerate() {
        assert_eq!(pt.dequantize(s).to_bits(), map.heights()[i].to_bits());
    }
}

/// `pcg_hash` is the published PCG hash (values computed independently from the paper's
/// constants), so the CPU and GPU grass streams agree.
#[test]
fn pcg_hash_matches_reference_values() {
    assert_eq!(pcg_hash(0), 129_708_002);
    assert_eq!(pcg_hash(1), 2_831_084_092);
    assert_eq!(pcg_hash(2), 2_055_130_248);
    assert_eq!(pcg_hash(u32::MAX), 3_861_530_882);
}

/// `patch_hash` nests the PCG hash over the seed and both patch coordinates: it is pinned,
/// asymmetric in `(fx, fz)`, defined for negative coordinates and sensitive to the seed.
#[test]
fn patch_hash_is_pinned_and_order_sensitive() {
    assert_eq!(patch_hash(7, 3, -2), 3_817_020_547);
    assert_eq!(patch_hash(7, -2, 3), 3_176_515_410);
    assert_ne!(patch_hash(7, 1, 0), patch_hash(7, 0, 1));
    assert_ne!(patch_hash(7, 5, 5), patch_hash(8, 5, 5));
    assert_eq!(
        patch_hash(9, i32::MIN, i32::MAX),
        patch_hash(9, i32::MIN, i32::MAX)
    );
}

/// `candidates_per_fine_patch` rounds density × patch area up, clamps the tier scale to 0..=1,
/// treats negative or NaN input as none and caps at the documented maximum.
#[test]
fn candidates_per_fine_patch_rounds_clamps_and_caps() {
    // 8 m patches: 64 m² at one clump per m².
    assert_eq!(candidates_per_fine_patch(1.0, 1.0), 64);
    assert_eq!(candidates_per_fine_patch(1.0, 0.5), 32);
    assert_eq!(candidates_per_fine_patch(0.01, 1.0), 1);
    assert_eq!(candidates_per_fine_patch(1.0, 3.0), 64);
    assert_eq!(candidates_per_fine_patch(1.0, -1.0), 0);
    assert_eq!(candidates_per_fine_patch(-4.0, 1.0), 0);
    assert_eq!(candidates_per_fine_patch(f32::NAN, 1.0), 0);
    assert_eq!(
        candidates_per_fine_patch(1.0e6, 1.0),
        MAX_CANDIDATES_PER_FINE_PATCH
    );
    assert_eq!(
        candidates_per_fine_patch(f32::INFINITY, 1.0),
        MAX_CANDIDATES_PER_FINE_PATCH
    );
}

/// `QuadNode::children` gives the four quadrants one level finer, and together they tile the
/// parent's world rectangle exactly.
#[test]
fn quad_children_tile_the_parent() {
    let parent = QuadNode { level: 3, x: 2, z: 1 };
    let kids = parent.children();
    let cells: Vec<(u32, u32, u32)> = kids.iter().map(|k| (k.level, k.x, k.z)).collect();
    assert_eq!(cells, vec![(2, 4, 2), (2, 5, 2), (2, 4, 3), (2, 5, 3)]);
    let (w, root) = (1024.0, 5);
    let (px, pz, ps) = parent.world_rect(w, root);
    let mut area = 0.0;
    for k in kids {
        let (x, z, s) = k.world_rect(w, root);
        assert_eq!(s * 2.0, ps);
        assert!(x >= px && z >= pz && x + s <= px + ps && z + s <= pz + ps);
        area += s * s;
    }
    assert_eq!(area, ps * ps);
}

/// `MinMaxPyramid::node_aabb` spans the node's world rectangle and its height range, and the
/// root box contains every child box (on a ramp rising one metre per metre along +X).
#[test]
fn node_aabb_bounds_rect_and_heights() {
    // 9 × 9 samples over 8 m: sample spacing 1 m, height = column index (0..=8 m).
    let heights: Vec<f32> = (0..81).map(|i| (i % 9) as f32).collect();
    let map = TerrainMap::from_heights(9, 9, heights, 8.0, 8.0, 0.0).expect("map");
    let root_level = 2;
    let pyramid = MinMaxPyramid::build(&map, root_level).expect("pyramid");
    let root = QuadNode::root(root_level);
    let bb = pyramid.node_aabb(root, 8.0, root_level);
    assert_eq!(bb.min.to_array(), [-4.0, 0.0, -4.0]);
    assert_eq!(bb.max.to_array(), [4.0, 8.0, 4.0]);
    // Leaf (0, 0) covers x in -4..-2: columns 0..=2.
    let leaf = pyramid.node_aabb(QuadNode { level: 0, x: 0, z: 0 }, 8.0, root_level);
    assert_eq!(leaf.min.to_array(), [-4.0, 0.0, -4.0]);
    assert_eq!(leaf.max.to_array(), [-2.0, 2.0, -2.0]);
    for kid in root.children() {
        let k = pyramid.node_aabb(kid, 8.0, root_level);
        assert!(k.min.cmpge(bb.min).all() && k.max.cmple(bb.max).all(), "{kid:?}");
        assert!(k.min.y <= k.max.y);
    }
}

/// `vertex_morph_t` ramps linearly from 0 at the band start to 1 at its end, clamps outside,
/// and is 0 for a degenerate or inverted band (the root level).
#[test]
fn vertex_morph_t_ramps_and_clamps() {
    assert_eq!(vertex_morph_t(10.0, 20.0, 5.0), 0.0);
    assert_eq!(vertex_morph_t(10.0, 20.0, 10.0), 0.0);
    assert_eq!(vertex_morph_t(10.0, 20.0, 15.0), 0.5);
    assert_eq!(vertex_morph_t(10.0, 20.0, 20.0), 1.0);
    assert_eq!(vertex_morph_t(10.0, 20.0, 1.0e9), 1.0);
    assert_eq!(vertex_morph_t(20.0, 20.0, 25.0), 0.0);
    assert_eq!(vertex_morph_t(30.0, 20.0, 25.0), 0.0);
    let mut last = 0.0;
    for i in 0..=100 {
        let t = vertex_morph_t(10.0, 20.0, 10.0 + i as f32 * 0.1);
        assert!(t >= last && (0.0..=1.0).contains(&t));
        last = t;
    }
}

fn ellipse(cx: f32, cz: f32, r: f32) -> HoleEllipse {
    HoleEllipse {
        center: [cx, cz],
        radius: [r, r],
        rotation_radians: 0.0,
    }
}

/// `HolesMask::intersects_rect` is true for rectangles touching any hole's bounds (ellipses,
/// shapes, open named holes) and false for distant ones, an empty mask and closed named holes.
#[test]
fn holes_intersects_rect_covers_every_kind() {
    assert!(!HolesMask::default().intersects_rect(-1.0e6, -1.0e6, 2.0e6));
    let mut mask = HolesMask::new(vec![ellipse(0.0, 0.0, 1.0)]).expect("mask");
    assert!(mask.intersects_rect(-0.5, -0.5, 1.0));
    assert!(mask.intersects_rect(1.0, -0.5, 1.0), "touching the bound counts");
    assert!(!mask.intersects_rect(1.5, 1.5, 1.0));
    mask.shapes.push(HoleShape::Rect {
        center: [50.0, 50.0],
        half_extents: [2.0, 2.0],
        rotation_radians: 0.0,
    });
    assert!(mask.intersects_rect(51.0, 51.0, 0.5));
    mask.set_named(NamedHole {
        id: "door".into(),
        open: false,
        shape: HoleShape::Ellipse(ellipse(-80.0, 0.0, 3.0)),
    })
    .expect("named");
    assert!(
        !mask.intersects_rect(-81.0, -1.0, 2.0),
        "a closed named hole does not cut"
    );
    mask.set_open("door", true).expect("open");
    assert!(mask.intersects_rect(-81.0, -1.0, 2.0));
}

/// `HolesMask::all_shapes` lists ellipses, then shapes, then the open named holes in id order.
#[test]
fn holes_all_shapes_order() {
    let mut mask = HolesMask::new(vec![ellipse(1.0, 0.0, 1.0)]).expect("mask");
    let rect = HoleShape::Rect {
        center: [2.0, 0.0],
        half_extents: [1.0, 1.0],
        rotation_radians: 0.0,
    };
    mask.shapes.push(rect.clone());
    for (id, x, open) in [("b", 4.0, true), ("a", 3.0, true), ("c", 5.0, false)] {
        mask.set_named(NamedHole {
            id: id.into(),
            open,
            shape: HoleShape::Ellipse(ellipse(x, 0.0, 1.0)),
        })
        .expect("named");
    }
    let shapes: Vec<HoleShape> = mask.all_shapes().collect();
    assert_eq!(
        shapes,
        vec![
            HoleShape::Ellipse(ellipse(1.0, 0.0, 1.0)),
            rect,
            HoleShape::Ellipse(ellipse(3.0, 0.0, 1.0)),
            HoleShape::Ellipse(ellipse(4.0, 0.0, 1.0)),
        ]
    );
}

/// `TerrainHoles::with_applied` runs only on a linked handle, sees the applied (not the merely
/// requested) mask, and leaves the request pending.
#[test]
fn terrain_holes_with_applied_needs_a_link() {
    let unlinked = TerrainHoles::default();
    assert_eq!(unlinked.with_applied(|m| m.holes.len()), None);
    let linked = TerrainHoles::new_linked();
    assert_eq!(linked.with_applied(|m| m.holes.len()), Some(0));
    linked.request(HolesMask::new(vec![ellipse(0.0, 0.0, 2.0)]).expect("mask"));
    assert_eq!(linked.with_applied(|m| m.holes.len()), Some(0), "not applied yet");
    assert!(linked.state().requested.is_some());
}

/// `ridged_fbm` is deterministic, stays in 0..=1 and depends on the seed; zero octaves gives 0
/// rather than NaN (0 / 0 before the fix).
#[test]
fn ridged_fbm_range_determinism_and_zero_octaves() {
    for i in 0..200 {
        let (x, y) = (i as f32 * 0.37 - 20.0, i as f32 * 0.11 + 3.0);
        let v = ridged_fbm(x, y, 9, 6);
        assert!((0.0..=1.0).contains(&v), "{v} at {x},{y}");
        assert_eq!(v.to_bits(), ridged_fbm(x, y, 9, 6).to_bits());
    }
    let differs =
        (0..50).any(|i| ridged_fbm(i as f32 * 0.7, 0.3, 1, 4) != ridged_fbm(i as f32 * 0.7, 0.3, 2, 4));
    assert!(differs);
    assert_eq!(ridged_fbm(1.5, 2.5, 3, 0), 0.0);
}

/// `BrushMode::noise_at` stays in -1..=1, is deterministic, and clamps a tiny or non-finite
/// wavelength to half a sample instead of dividing by zero.
#[test]
fn brush_noise_at_range_and_wavelength_clamp() {
    for x in -20..20 {
        for y in [-7, 0, 13] {
            let p = glam::IVec2::new(x, y);
            let n = BrushMode::noise_at(p, 4.0, 11);
            assert!((-1.0..=1.0).contains(&n));
            assert_eq!(n, BrushMode::noise_at(p, 4.0, 11));
            assert_eq!(BrushMode::noise_at(p, 0.0, 11), BrushMode::noise_at(p, 0.5, 11));
            assert_eq!(
                BrushMode::noise_at(p, f32::NAN, 11),
                BrushMode::noise_at(p, 0.5, 11)
            );
            assert_eq!(
                BrushMode::noise_at(p, f32::INFINITY, 11),
                BrushMode::noise_at(p, 0.5, 11)
            );
        }
    }
}

/// `apply_hardness` keeps zero at zero, saturates at `1 - hardness`, scales linearly below it,
/// and a fully hard brush is full strength wherever it touches.
#[test]
fn apply_hardness_curve() {
    assert_eq!(apply_hardness(0.5, 0.0), 0.0);
    assert_eq!(apply_hardness(0.5, 0.25), 0.5);
    assert_eq!(apply_hardness(0.5, 0.5), 1.0);
    assert_eq!(apply_hardness(0.5, 0.9), 1.0);
    assert_eq!(apply_hardness(0.0, 0.3), 0.3);
    assert_eq!(apply_hardness(1.0, 0.001), 1.0);
    assert_eq!(apply_hardness(1.0, 0.0), 0.0);
}

/// `bed_slope` measures rise over run: 0 on flat ground and `hypot(a, b)` on the plane
/// `h = a x + b z` (bilinear sampling is exact on a plane).
#[test]
fn bed_slope_of_flat_and_tilted_planes() {
    let n = 17u32;
    let w = 64.0f32;
    let flat = TerrainMap::from_heights(n, n, vec![5.0; (n * n) as usize], w, 10.0, 0.0).expect("flat");
    assert_eq!(bed_slope(&flat, 3.0, -7.0), 0.0);
    let (a, b) = (0.3f32, 0.4f32);
    let mut heights = Vec::new();
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * w - w * 0.5;
            let z = row as f32 / (n - 1) as f32 * w - w * 0.5;
            heights.push(a * x + b * z + 30.0);
        }
    }
    let tilted = TerrainMap::from_heights(n, n, heights, w, 100.0, 0.0).expect("tilted");
    for (x, z) in [(0.0, 0.0), (5.5, -9.25), (-20.0, 17.0)] {
        let s = bed_slope(&tilted, x, z);
        assert!((s - 0.5).abs() < 1.0e-4, "{s} at {x},{z}");
    }
}

/// `FftOcean::unit_wind` normalizes the wind direction and falls back to +x for a zero or
/// non-finite one.
#[test]
fn ocean_unit_wind_normalizes_and_falls_back() {
    let mut o = FftOcean {
        wind_direction: [3.0, -4.0],
        ..FftOcean::default()
    };
    assert_eq!(o.unit_wind(), [0.6, -0.8]);
    o.wind_direction = [0.0, 0.0];
    assert_eq!(o.unit_wind(), [1.0, 0.0]);
    o.wind_direction = [f32::NAN, 1.0];
    assert_eq!(o.unit_wind(), [1.0, 0.0]);
    o.wind_direction = [f32::INFINITY, 0.0];
    assert_eq!(o.unit_wind(), [1.0, 0.0]);
}

/// `OceanTables::twiddles` holds `cos` and `sin` of `2π j / size` for `j < size / 2`.
#[test]
fn ocean_twiddles_are_the_fft_roots() {
    let size = 16u32;
    let tables = OceanTables::new(&FftOcean::default(), size);
    let (c, s) = tables.twiddles();
    assert_eq!(c.len(), 8);
    assert_eq!(s.len(), 8);
    for j in 0..8 {
        let a = std::f64::consts::TAU * j as f64 / f64::from(size);
        assert!((f64::from(c[j]) - a.cos()).abs() < 1.0e-6, "cos {j}");
        assert!((f64::from(s[j]) - a.sin()).abs() < 1.0e-6, "sin {j}");
    }
}

/// `OceanFrame::grid_offset` with no grid fade (cell 0) equals the unfaded sample's offset bit
/// for bit, and a cell far coarser than every band fades the ocean to nothing.
#[test]
fn ocean_grid_offset_fades_with_the_cell() {
    let frame = frame_of(&FftOcean::default(), 3.5, 16);
    for (x, z) in [(0.0f32, 0.0f32), (12.3, -40.7), (500.0, 250.0)] {
        let full = frame.grid_offset(f64::from(x), f64::from(z), 0.0);
        let s = frame.sample(x, z);
        assert_eq!(full.map(f32::to_bits), s.offset.map(f32::to_bits));
        assert_eq!(frame.grid_offset(f64::from(x), f64::from(z), 1.0e6), [0.0; 3]);
    }
}

/// `OceanFrame::compressed_share` is a share in 0..=1: all texels under an enormous limit,
/// none under a hugely negative one, and zero samples are treated as one.
#[test]
fn ocean_compressed_share_bounds() {
    let frame = frame_of(&FftOcean::default(), 1.0, 16);
    assert_eq!(frame.compressed_share(1.0e9, 8), 1.0);
    assert_eq!(frame.compressed_share(-1.0e9, 8), 0.0);
    let one = frame.compressed_share(1.0e9, 0);
    assert_eq!(one, 1.0);
    let mid = frame.compressed_share(1.0, 8);
    assert!((0.0..=1.0).contains(&mid));
}

/// `SwellInstant::grid_offset` (and its local and depth forms) equals the deep-water sample's
/// offset with no fade, and fades every wave out on a very coarse grid.
#[test]
fn swell_grid_offset_matches_sample_without_fade() {
    let swell = Swell::open_sea([1.0, 0.3], 2.0, 40.0, 5);
    let origin = [100.0f64, -50.0];
    let inst = swell.instant(12.25, origin, 8);
    for rest in [[100.0f32, -50.0], [133.3, -20.5], [-7.0, 9.0]] {
        let s = inst.sample(rest);
        let g = inst.grid_offset(rest, 0.0);
        for (axis, (a, b)) in g.iter().zip(s.offset).enumerate() {
            assert!((a - b).abs() < 1.0e-4, "{rest:?} axis {axis}: {a} vs {b}");
        }
        let local = [
            (f64::from(rest[0]) - origin[0]) as f32,
            (f64::from(rest[1]) - origin[1]) as f32,
        ];
        assert_eq!(inst.grid_offset_local(local, 0.0), g);
        assert_eq!(inst.grid_offset_local_on(local, 0.0, None), g);
        assert_eq!(inst.grid_offset(rest, 1.0e6), [0.0; 3]);
    }
}

/// `SwellInstant::sample_on` with no depth is the deep sample (no breaking), and very shallow
/// water flattens the waves and reports a positive breaking ratio.
#[test]
fn swell_sample_on_shoals_in_shallow_water() {
    let swell = Swell::open_sea([0.0, 1.0], 2.0, 60.0, 4);
    let inst = swell.instant(0.0, [0.0, 0.0], 8);
    let rest = [3.0f32, 7.0];
    let (deep, ratio) = inst.sample_on(rest, None);
    assert_eq!(deep, inst.sample(rest));
    assert_eq!(ratio, 0.0);
    let (_, shallow_ratio) = inst.sample_on(rest, Some(0.2));
    assert!(shallow_ratio > 0.0, "{shallow_ratio}");
    // The rest-point inverse of the shoaled surface lands as closely as the deep one.
    let (r, miss) = inst.rest_point_on(3.0, 7.0, |_| Some(5.0));
    assert!(r.iter().all(|v| v.is_finite()));
    assert!(miss < 0.05, "{miss}");
}

/// `rest_point_by` inverts a known displacement: for `offset = 0.5 · rest` the rest point under
/// `(x, z)` is `(x, z) / 1.5`, and a zero displacement returns the point itself.
#[test]
fn rest_point_by_inverts_a_linear_displacement() {
    let linear = |r: [f32; 2]| SwellSample {
        offset: [0.5 * r[0], 0.0, 0.5 * r[1]],
        dpdx: [1.5, 0.0, 0.0],
        dpdz: [0.0, 0.0, 1.5],
        velocity: [0.0; 3],
        jacobian: 2.25,
    };
    let (r, miss) = rest_point_by(3.0, -6.0, linear);
    assert!(
        (r[0] - 2.0).abs() < 1.0e-5 && (r[1] + 4.0).abs() < 1.0e-5,
        "{r:?}"
    );
    assert!(miss < 1.0e-4);
    let still = |_: [f32; 2]| SwellSample {
        offset: [0.0; 3],
        dpdx: [1.0, 0.0, 0.0],
        dpdz: [0.0, 0.0, 1.0],
        velocity: [0.0; 3],
        jacobian: 1.0,
    };
    assert_eq!(rest_point_by(4.5, 1.25, still), ([4.5, 1.25], 0.0));
}

/// `ring_fade_cell` keeps the grid cell inside half the inner ring and grows linearly with the
/// Chebyshev distance beyond it (continuous, never finer than the grid).
#[test]
fn ring_fade_cell_grows_with_chebyshev_distance() {
    assert_eq!(ring_fade_cell([0.0, 0.0], 2.0, 100.0), 2.0);
    assert_eq!(ring_fade_cell([50.0, -10.0], 2.0, 100.0), 2.0);
    assert_eq!(ring_fade_cell([10.0, -100.0], 2.0, 100.0), 4.0);
    assert_eq!(ring_fade_cell([-200.0, 30.0], 2.0, 100.0), 8.0);
    // A zero inner size does not divide by zero.
    assert!(ring_fade_cell([1.0, 1.0], 2.0, 0.0).is_finite());
}

/// `ImpostorSettings::size_limit_distance` is the closed-form pinhole distance, linear in the
/// radius (default settings: 1 m projects to 0.75 px at about 2058.7 m).
#[test]
fn impostor_size_limit_distance_closed_form() {
    let s = ImpostorSettings::default();
    let d = s.size_limit_distance(1.0);
    assert!((d - 2058.7266).abs() < 0.01, "{d}");
    assert!((s.size_limit_distance(3.0) - 3.0 * d).abs() < 0.01);
    assert_eq!(s.size_limit_distance(0.0), 0.0);
    let wide = ImpostorSettings {
        reference_fov_y: 90f32.to_radians(),
        ..s
    };
    assert!(wide.size_limit_distance(1.0) < d);
}

/// `uniform_scale` is the horizontal geometric mean of the scale, ignores the flattened Y and
/// the sign of a mirrored axis.
#[test]
fn uniform_scale_is_horizontal_geometric_mean() {
    assert_eq!(uniform_scale([2.0, 0.1, 8.0]), 4.0);
    assert_eq!(uniform_scale([-2.0, 5.0, 8.0]), 4.0);
    assert_eq!(uniform_scale([1.5, 1.5, 1.5]), 1.5);
    assert_eq!(uniform_scale([0.0, 1.0, 9.0]), 0.0);
}

fn class(name: &str, meshes: &[&str], density_map: Option<&str>) -> ScatterClass {
    ScatterClass {
        name: name.into(),
        meshes: meshes
            .iter()
            .map(|m| ScatterMeshRef {
                mesh: (*m).into(),
                weight: 1.0,
                min_scale: 1.0,
                max_scale: 1.0,
                scale_bias: 1.0,
            })
            .collect(),
        density_map: density_map.map(str::to_owned),
        ..ScatterClass::default()
    }
}

/// `ScatterSet::mesh_paths` and `density_map_paths` list each referenced path once, in first-use
/// order, and skip classes without a density map.
#[test]
fn scatter_set_paths_dedupe_in_first_use_order() {
    let set = ScatterSet {
        classes: vec![
            class("a", &["rock_b.glb", "rock_a.glb"], Some("maps/a.png")),
            class("b", &["rock_a.glb", "bush.glb"], None),
            class("c", &["rock_b.glb"], Some("maps/a.png")),
            class("d", &["tree.glb"], Some("maps/d.png")),
        ],
        ..ScatterSet::default()
    };
    assert_eq!(
        set.mesh_paths(),
        vec!["rock_b.glb", "rock_a.glb", "bush.glb", "tree.glb"]
    );
    assert_eq!(set.density_map_paths(), vec!["maps/a.png", "maps/d.png"]);
    assert!(ScatterSet::default().mesh_paths().is_empty());
    assert!(ScatterSet::default().density_map_paths().is_empty());
}

/// `ScatterSet::from_v1` carries a v1 rule's mesh and filters into one class, and turns the
/// layer index into a one-bit mask (0 for an index past the 8-bit mask, not a panic).
#[test]
fn scatter_set_from_v1_maps_the_rule() {
    let rule = ScatterRule {
        mesh: "props/rock.glb".into(),
        cell_size: 6.0,
        instances_per_cell: 2.5,
        min_scale: 0.5,
        max_scale: 1.5,
        slope_max: 0.7,
        height_min: -3.0,
        height_max: 90.0,
        layer_index: Some(3),
        layer_min_weight: 0.4,
        avoid_holes: true,
        align_to_normal: true,
        ..ScatterRule::default()
    };
    let set = ScatterSet::from_v1(&rule);
    assert_eq!(set.classes.len(), 1);
    let c = &set.classes[0];
    assert_eq!(set.mesh_paths(), vec!["props/rock.glb"]);
    assert_eq!((c.meshes[0].min_scale, c.meshes[0].max_scale), (0.5, 1.5));
    assert_eq!((c.cell_size, c.density), (6.0, 2.5));
    assert_eq!((c.slope_max, c.height_min, c.height_max), (0.7, -3.0, 90.0));
    assert_eq!(c.layer_mask, 0b1000);
    assert_eq!(c.layer_min_weight, 0.4);
    assert!(c.avoid_holes);
    assert_eq!(c.align_to_normal, 1.0);
    let none = ScatterSet::from_v1(&ScatterRule {
        layer_index: None,
        ..rule.clone()
    });
    assert_eq!(none.classes[0].layer_mask, 0);
    let past = ScatterSet::from_v1(&ScatterRule {
        layer_index: Some(8),
        ..rule
    });
    assert_eq!(past.classes[0].layer_mask, 0);
}

fn cooked(type_path: &str, payload: &[u8]) -> Vec<u8> {
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: TypePathHash::of(type_path),
        asset_id: AssetId::default(),
        importer_version: 1_000_000,
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    encode_cooked_file(&header, payload).expect("encode")
}

fn tiled_3x3() -> CookedTiledHeightfield {
    // 3 × 3 samples in 2 × 2 tiles: the right and bottom tiles are border-padded.
    let tile = |x: u32, y: u32, s: [u16; 4]| CookedHeightfieldTile {
        x,
        y,
        min: *s.iter().min().unwrap_or(&0),
        max: *s.iter().max().unwrap_or(&0),
        samples: s.to_vec(),
    };
    CookedTiledHeightfield {
        version: TILED_HEIGHTFIELD_FORMAT_VERSION,
        width: 3,
        height: 3,
        tile_size: 2,
        tiles: vec![
            tile(0, 0, [0, 1, 3, 4]),
            tile(1, 0, [2, 2, 5, 5]),
            tile(0, 1, [6, 7, 6, 7]),
            tile(1, 1, [8, 8, 8, 8]),
        ],
    }
}

/// `decode_cooked_tiled_heightfield` returns the cooked tiles unchanged, and
/// `load_tiled_heightfield_bytes` stitches them into the map, dropping the border padding.
#[test]
fn tiled_heightfield_bytes_decode_and_stitch() {
    let tiled = tiled_3x3();
    let bytes = cooked(
        TILED_HEIGHTFIELD_TYPE_PATH,
        &postcard::to_allocvec(&tiled).expect("postcard"),
    );
    assert_eq!(decode_cooked_tiled_heightfield(&bytes).expect("decode"), tiled);
    let hf = load_tiled_heightfield_bytes(&bytes).expect("stitch");
    assert_eq!((hf.width, hf.height), (3, 3));
    assert_eq!(hf.samples, (0u16..9).collect::<Vec<_>>());
}

/// The cooked decoders reject garbage, a wrong type tag and a tile set that misses part of the
/// map, with an error instead of a panic.
#[test]
fn tiled_heightfield_bytes_reject_bad_input() {
    assert!(matches!(
        decode_cooked_tiled_heightfield(b"nope"),
        Err(TerrainIndexError::Cooked(_))
    ));
    let tiled = tiled_3x3();
    let payload = postcard::to_allocvec(&tiled).expect("postcard");
    let wrong = cooked(TERRAIN_INDEX_TYPE_PATH, &payload);
    match decode_cooked_tiled_heightfield(&wrong) {
        Err(TerrainIndexError::WrongType { found, .. }) => {
            assert_eq!(found, TypePathHash::of(TERRAIN_INDEX_TYPE_PATH).get());
        }
        other => panic!("expected WrongType, got {other:?}"),
    }
    let mut short = tiled;
    short.tiles.pop();
    let bytes = cooked(
        TILED_HEIGHTFIELD_TYPE_PATH,
        &postcard::to_allocvec(&short).expect("postcard"),
    );
    assert!(
        decode_cooked_tiled_heightfield(&bytes).is_ok(),
        "decoding does not stitch"
    );
    assert!(matches!(
        load_tiled_heightfield_bytes(&bytes),
        Err(TerrainIndexError::BadHeightfield(_))
    ));
    let mut truncated = cooked(TILED_HEIGHTFIELD_TYPE_PATH, &payload);
    truncated.truncate(truncated.len() - 3);
    assert!(load_tiled_heightfield_bytes(&truncated).is_err());
}

/// `load_terrain_index_bytes` round-trips an index, and rejects a wrong type tag, an unknown
/// version and a corrupt payload.
#[test]
fn terrain_index_bytes_round_trip_and_rejects() {
    let index = TerrainIndex {
        version: TERRAIN_INDEX_FORMAT_VERSION,
        map_id: AssetId::default(),
        slots: Vec::new(),
        heightfield: None,
        layers: vec!["grass".into(), "rock".into()],
        world_size_m: Some(2048.0),
        world_origin: None,
        source_mapping: None,
        source_spacing_z_m: None,
        source_valid_mask: None,
        heights: None,
        holes: None,
        material: None,
    };
    let payload = index.to_postcard().expect("postcard");
    let bytes = cooked(TERRAIN_INDEX_TYPE_PATH, &payload);
    let (_, raw) = decode_cooked_file(&bytes).expect("wrapper");
    assert_eq!(raw, payload.as_slice());
    let back = load_terrain_index_bytes(&bytes).expect("load");
    assert_eq!(back.layers, index.layers);
    assert_eq!(back.world_size_m, Some(2048.0));
    assert!(matches!(
        load_terrain_index_bytes(&cooked(TILED_HEIGHTFIELD_TYPE_PATH, &payload)),
        Err(TerrainIndexError::WrongType { .. })
    ));
    let future = TerrainIndex {
        version: TERRAIN_INDEX_FORMAT_VERSION + 1,
        ..index
    };
    let bytes = cooked(TERRAIN_INDEX_TYPE_PATH, &future.to_postcard().expect("postcard"));
    assert!(matches!(
        load_terrain_index_bytes(&bytes),
        Err(TerrainIndexError::BadVersion { found, supported })
            if found == TERRAIN_INDEX_FORMAT_VERSION + 1 && supported == TERRAIN_INDEX_FORMAT_VERSION
    ));
    assert!(load_terrain_index_bytes(&cooked(TERRAIN_INDEX_TYPE_PATH, &[0xff; 5])).is_err());
    assert!(load_terrain_index_bytes(&[]).is_err());
}

/// `layer_mip_skip_for` drops one layer mip on the Deck and mobile presets and none on desktop.
#[test]
fn layer_mip_skip_by_preset() {
    use RenderProfilePreset as P;
    for p in [P::Deck, P::MobileLow, P::MobileHigh] {
        assert_eq!(layer_mip_skip_for(p), 1, "{p:?}");
    }
    for p in [P::DesktopLow, P::DesktopHigh, P::DesktopUltra, P::Cinematic] {
        assert_eq!(layer_mip_skip_for(p), 0, "{p:?}");
    }
}
