// SPDX-License-Identifier: MPL-2.0
//! Lane W-15 (spec 26 § 2a.4 items 1-2, ADR-8470): the GPU draws a filled mouth's hole from the
//! ground-lattice cut, not a coverage raster.
//!
//! - `lattice_windows_match_the_cpu_cut`: `terrain.wgsl`'s own `in_hole` (its text, cut out of the
//!   shader, run in a compute pass on lavapipe) against the CPU mirror
//!   [`LatticeHoleCut::cuts`] at 4096 synthetic points on a C-shaped cut wider than one window,
//!   and at points around every mouth of the cooked Switchbacks fixture, whose cut is W-10's
//!   cooked bitmap (`GroundLattice::is_cut`), read through `TerrainMeshLayers::lattice_hole_cut`.
//!   At every lattice triangle centroid near a mouth the mirror equals the cooked bit.
//! - `maps_without_layers_draw_the_same_words`: a mask with no lattice cut, or one whose cut fills
//!   none of its holes, uploads exactly [`HolesMask::gpu_words`].
//! - `index_cull_never_drops_uncut_lattice_ground`: a C-shaped mouth whose coverage shape is its
//!   whole box: at every level no node triangle over an uncut lattice triangle is dropped and no
//!   node over one is skipped as covered.
//!
//! Fixtures: DO27's Switchbacks layout (`regia_world_geom`'s `fixtures` feature) on flat
//! synthetic ground; no DO27 map data, no goldens, no timing.

// Test fixtures panic on a broken fixture.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, OnceLock};

use regia_render::RenderDevice;
use regia_terrain::{
    BandMesh, HoleEllipse, HoleShape, HolesMask, LatticeHoleCut, LayerMouth, NamedHole, NodeHoleState,
    QuadNode, TerrainMap, TerrainMeshLayers, build_node_indices_lattice_at, node_hole_state_lattice_at,
};
use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookError, CookInputs, CookOutput, GroundInput, cook};
use regia_world_geom::cut::centroid3;
use regia_world_geom::geom::WorldGeom;
use regia_world_geom::ground::{GroundLattice, vertex_m};
use regia_world_geom::passages::PassagesDocument;
use wgpu::util::DeviceExt;

const TERRAIN_WGSL: &str = include_str!("../src/gpu/terrain.wgsl");

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// The text of WGSL function `name` in `source` (from `fn name(` to its closing brace).
fn wgsl_fn(source: &str, name: &str) -> String {
    let start = source
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("terrain.wgsl has no fn {name}"));
    let mut depth = 0i32;
    for (i, c) in source[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return source[start..=start + i].to_owned();
                }
            }
            _ => {}
        }
    }
    panic!("fn {name} is not closed");
}

/// A compute shader around the shader's own hole test: `out[i] = in_hole(p[i], p[i])`.
fn probe_source() -> String {
    format!(
        "const HOLE_TEXELS: u32 = 256u;
@group(0) @binding(0) var<storage, read> holes: array<u32>;
@group(0) @binding(1) var<storage, read> points: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read_write> out: array<u32>;

{}

{}

{}

@compute @workgroup_size(64)
fn probe(@builtin(global_invocation_id) id: vec3<u32>) {{
    if (id.x >= arrayLength(&points)) {{
        return;
    }}
    let p = points[id.x];
    out[id.x] = select(0u, 1u, in_hole(p, p));
}}
",
        wgsl_fn(TERRAIN_WGSL, "hole_texel"),
        wgsl_fn(TERRAIN_WGSL, "lattice_byte"),
        wgsl_fn(TERRAIN_WGSL, "in_hole"),
    )
}

/// Runs the shader's `in_hole` over `words` at `points` (each point is both the base and the
/// reference position).
fn gpu_in_hole(words: &[u32], points: &[[f32; 2]]) -> Vec<bool> {
    let rd = device();
    let (d, q) = (&rd.device, &rd.queue);
    let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("lattice hole probe"),
        source: wgpu::ShaderSource::Wgsl(probe_source().into()),
    });
    let entry = |binding, read_only| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };
    let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("lattice hole probe"),
        entries: &[entry(0, true), entry(1, true), entry(2, false)],
    });
    let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("lattice hole probe"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("lattice hole probe"),
        layout: Some(&pipeline_layout),
        module: &module,
        entry_point: Some("probe"),
        compilation_options: Default::default(),
        cache: None,
    });
    let holes = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("probe holes"),
        contents: bytemuck::cast_slice(words),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let flat: Vec<f32> = points.iter().flat_map(|p| [p[0], p[1]]).collect();
    let point_buf = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("probe points"),
        contents: bytemuck::cast_slice(&flat),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let size = (points.len() * 4) as u64;
    let out = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("probe out"),
        size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("probe readback"),
        size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("probe bind"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: holes.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: point_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: out.as_entire_binding(),
            },
        ],
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups((points.len() as u32).div_ceil(64), 1, 1);
    }
    encoder.copy_buffer_to_buffer(&out, 0, &readback, 0, size);
    q.submit([encoder.finish()]);
    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    d.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    rx.recv().expect("map callback").expect("map");
    let data = slice.get_mapped_range().expect("mapped range");
    let values: Vec<u32> = bytemuck::cast_slice(&data).to_vec();
    drop(data);
    readback.unmap();
    values.into_iter().map(|v| v != 0).collect()
}

/// A deterministic generator (no wall clock, no OS entropy).
struct Lcg(u64);

impl Lcg {
    fn unit(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 40) as f32) / (1u64 << 24) as f32
    }
}

/// A C-shaped cut on lattice cells `[20, 320) x [10, 90)` of a 400-cell lattice: the outer box
/// cut, minus an uncut arm `[60, 320) x [40, 60)` open to the east, and the lower triangle of
/// cell (100, 20) kept (a single uncut lattice triangle inside the cut).
fn c_cut(i: u32, j: u32, t: u8) -> bool {
    let outer = (20..320).contains(&i) && (10..90).contains(&j);
    let arm = (60..320).contains(&i) && (40..60).contains(&j);
    let corner = i == 100 && j == 20 && t == 0;
    outer && !arm && !corner
}

const SYN_ORIGIN: [f32; 2] = [-62.5, -40.0];
const SYN_STEP: f32 = 1280.0 / 4095.0;
const SYN_CELLS: u32 = 400;

fn synthetic_cut() -> LatticeHoleCut {
    let mut cut = LatticeHoleCut::new(SYN_ORIGIN, SYN_STEP, [SYN_CELLS, SYN_CELLS]);
    cut.set_hole("west_mouth", [20, 10], [320, 90], c_cut);
    cut
}

/// The mask of the synthetic case: the mouth's named hole (its coverage shape is its whole box,
/// so every difference from the box is the lattice's), an ellipse far away and a closed hole.
fn synthetic_mask() -> HolesMask {
    let mut mask = HolesMask::new(vec![HoleEllipse {
        center: [100.0, 100.0],
        radius: [3.0, 2.0],
        rotation_radians: 0.3,
    }])
    .unwrap();
    let lo = [SYN_ORIGIN[0] + 20.0 * SYN_STEP, SYN_ORIGIN[1] + 10.0 * SYN_STEP];
    let hi = [SYN_ORIGIN[0] + 320.0 * SYN_STEP, SYN_ORIGIN[1] + 90.0 * SYN_STEP];
    let rect = HoleShape::Rect {
        center: [(lo[0] + hi[0]) * 0.5, (lo[1] + hi[1]) * 0.5],
        half_extents: [(hi[0] - lo[0]) * 0.5, (hi[1] - lo[1]) * 0.5],
        rotation_radians: 0.0,
    };
    mask.set_named(NamedHole {
        id: "west_mouth".into(),
        open: true,
        shape: rect.clone(),
    })
    .unwrap();
    mask.set_named(NamedHole {
        id: "closed_one".into(),
        open: false,
        shape: rect,
    })
    .unwrap();
    mask
}

/// DO27's drawn grid: 4096 samples over 1280 m.
const N: u32 = 4096;
const WIDTH_M: f32 = 1280.0;

fn cook_switchbacks() -> CookOutput {
    let terrain =
        TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), WIDTH_M).with_zero_counts(16384);
    let ground = GroundInput {
        lattice_n: N,
        width_m: WIDTH_M,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![8192; (N * N) as usize],
    };
    let layout = for_map("switchbacks").unwrap();
    let mouths = regia_world_geom::cut::passage_mouths(&layout, &layout.sample(Some(&terrain)));
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
            ground: &ground,
            terrain: Some(&terrain),
            read_file: &read,
        })
    };
    // The Switchbacks test cook authors ADR-8359's ramp-facet waiver, as the other test cooks do (ADR-8436).
    match run(&doc) {
        Ok(out) => out,
        Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
            run(&waive_ramp_facets(&doc, &report).unwrap()).unwrap()
        }
        Err(e) => panic!("{e} ({})", e.code()),
    }
}

/// Plan W-15 / brief A1, A3: the GPU in_hole verdict equals the CPU cut exactly, on a synthetic
/// cut (4096 points) and on the cooked Switchbacks bitmap.
#[test]
fn lattice_windows_match_the_cpu_cut() {
    // ---- Synthetic: a C-shaped cut 300 cells wide (two windows), mixed with coverage windows.
    let cut = synthetic_cut();
    assert_eq!(
        cut.hole("west_mouth").unwrap().windows.len(),
        2,
        "300 + 4 cells take two windows"
    );
    let mask = synthetic_mask();
    let words = mask.gpu_words_with_lattice(Some(&cut)).unwrap();
    assert_ne!(
        words,
        mask.gpu_words(),
        "the filled hole is drawn from lattice windows"
    );
    let mut rng = Lcg(0x5eed_1a77);
    let (lo, span) = (
        [SYN_ORIGIN[0] + 10.0 * SYN_STEP, SYN_ORIGIN[1]],
        [320.0 * SYN_STEP, 100.0 * SYN_STEP],
    );
    let mut points: Vec<[f32; 2]> = (0..4096 - 64)
        .map(|_| [lo[0] + rng.unit() * span[0], lo[1] + rng.unit() * span[1]])
        .collect();
    // Lattice vertices and diagonal points along the arm's mouth: the edge rules agree too.
    for k in 0..32u32 {
        let (i, j) = (58 + k % 8, 38 + k / 8 * 6);
        let v = [
            SYN_ORIGIN[0] + i as f32 * SYN_STEP,
            SYN_ORIGIN[1] + j as f32 * SYN_STEP,
        ];
        points.push(v);
        points.push([v[0] + 0.5 * SYN_STEP, v[1] + 0.5 * SYN_STEP]);
    }
    assert_eq!(points.len(), 4096);
    let gpu = gpu_in_hole(&words, &points);
    let mut cut_count = 0;
    let mut mismatches = Vec::new();
    for (p, &g) in points.iter().zip(&gpu) {
        let c = cut.cuts(p[0], p[1]);
        cut_count += u32::from(c);
        if c != g {
            mismatches.push((*p, c, g));
        }
    }
    println!(
        "synthetic: {} points, {cut_count} cut, {} mismatches",
        points.len(),
        mismatches.len()
    );
    assert!(
        mismatches.is_empty(),
        "GPU vs CPU cut: {:?}",
        &mismatches[..mismatches.len().min(8)]
    );
    assert!(
        cut_count > 1000 && cut_count < 3500,
        "the points straddle the cut ({cut_count})"
    );
    // The arm and the kept corner triangle are ground on both sides.
    let arm = [SYN_ORIGIN[0] + 200.5 * SYN_STEP, SYN_ORIGIN[1] + 50.5 * SYN_STEP];
    let corner = [SYN_ORIGIN[0] + 100.2 * SYN_STEP, SYN_ORIGIN[1] + 20.2 * SYN_STEP];
    assert!(!cut.cuts(arm[0], arm[1]) && !cut.cuts(corner[0], corner[1]));
    assert_eq!(gpu_in_hole(&words, &[arm, corner]), vec![false, false]);
    // The coverage mask cuts both (its shape is the whole box): the lattice is what keeps them.
    assert!(gpu_in_hole(&mask.gpu_words(), &[arm, corner]).iter().all(|&b| b));

    // ---- The cooked Switchbacks bitmap through TerrainMeshLayers.
    let out = cook_switchbacks();
    let canonical = regia_world_geom::cooked::encode_canonical(&out.file.canonical).unwrap();
    let geom = Arc::new(WorldGeom::from_canonical_bytes(&canonical).unwrap());
    let l = out.file.canonical.lattice;
    let mut layers = TerrainMeshLayers::with_geom(geom.clone());
    // Each mouth's ring box, in lattice cells, grown by 4.
    let mut boxes: Vec<([u32; 2], [u32; 2])> = Vec::new();
    for mouth in &out.report.ground.mouths {
        let mut box_lo = [u32::MAX; 2];
        let mut box_hi = [0u32; 2];
        let rings: Vec<Vec<[f32; 3]>> = mouth
            .rings
            .iter()
            .map(|ring| {
                ring.iter()
                    .map(|&(i, j)| {
                        box_lo = [box_lo[0].min(i), box_lo[1].min(j)];
                        box_hi = [box_hi[0].max(i), box_hi[1].max(j)];
                        [vertex_m(&l, i), 0.0, vertex_m(&l, j)]
                    })
                    .collect()
            })
            .collect();
        let cells = l.cells();
        boxes.push((
            [box_lo[0].saturating_sub(4), box_lo[1].saturating_sub(4)],
            [(box_hi[0] + 4).min(cells), (box_hi[1] + 4).min(cells)],
        ));
        layers.push_mouth(LayerMouth {
            hole: mouth.id.clone(),
            rings,
            band: BandMesh::default(),
            collar: Vec::new(),
            throat: Vec::new(),
            body: Vec::new(),
            open: true,
        });
    }
    assert!(!out.report.ground.mouths.is_empty());
    let map = TerrainMap::from_heights(N, N, vec![0.0; (N * N) as usize], WIDTH_M, 1.0, 0.0).unwrap();
    let lattice = layers
        .lattice_hole_cut(&map)
        .expect("open mouths on the map's lattice");
    // At every lattice triangle centroid around the mouths, the mirror is the cooked bit.
    let lg = GroundLattice::new(geom.cooked());
    let (mut checked, mut cut_bits, mut centroid_points) = (0u32, 0u32, Vec::new());
    for &(lo, hi) in &boxes {
        for (j, i) in (lo[1]..hi[1]).flat_map(|j| (lo[0]..hi[0]).map(move |i| (j, i))) {
            for t in 0..2u8 {
                let c3 = centroid3(&l, i, j, t);
                let (x, z) = ((c3[0] as f64 / 3000.0) as f32, (c3[1] as f64 / 3000.0) as f32);
                let bit = lg.is_cut(i, j, t);
                assert_eq!(
                    lattice.cuts(x, z),
                    bit,
                    "cell ({i}, {j}, {t}): the lattice windows carry the cooked cut bit"
                );
                checked += 1;
                cut_bits += u32::from(bit);
                if (i + j) % 7 == 0 {
                    centroid_points.push([x, z]);
                }
            }
        }
    }
    assert!(cut_bits > 0, "the cooked cut is not empty");
    // The GPU at 4096 random points around the mouths plus a sample of the centroids.
    let mut rng = Lcg(0x5b17_c4ba);
    let mut points: Vec<[f32; 2]> = (0..4096usize)
        .map(|k| {
            let (lo, hi) = boxes[k % boxes.len()];
            let (wlo, whi) = (
                [vertex_m(&l, lo[0]), vertex_m(&l, lo[1])],
                [vertex_m(&l, hi[0]), vertex_m(&l, hi[1])],
            );
            [
                wlo[0] + rng.unit() * (whi[0] - wlo[0]),
                wlo[1] + rng.unit() * (whi[1] - wlo[1]),
            ]
        })
        .collect();
    points.extend(centroid_points);
    let mut sw_mask = HolesMask::default();
    for mouth in &out.report.ground.mouths {
        let loops: Vec<Vec<[f32; 2]>> = mouth
            .rings
            .iter()
            .map(|ring| {
                ring.iter()
                    .map(|&(i, j)| [vertex_m(&l, i), vertex_m(&l, j)])
                    .collect()
            })
            .collect();
        sw_mask
            .set_named(NamedHole {
                id: mouth.id.clone(),
                open: true,
                shape: HoleShape::lattice_rings(&loops).unwrap(),
            })
            .unwrap();
    }
    // `TerrainMeshLayers::cuts` is the same windows (it builds them per call: a few points only).
    for p in points.iter().step_by(512) {
        assert_eq!(layers.cuts(&map, p[0], p[1]), lattice.cuts(p[0], p[1]));
    }
    let words = sw_mask.gpu_words_with_lattice(Some(&lattice)).unwrap();
    let gpu = gpu_in_hole(&words, &points);
    let mut mismatches = Vec::new();
    let mut cut_count = 0u32;
    for (p, &g) in points.iter().zip(&gpu) {
        let c = lattice.cuts(p[0], p[1]);
        cut_count += u32::from(c);
        if c != g {
            mismatches.push((*p, c, g));
        }
    }
    println!(
        "switchbacks: {checked} centroids checked against the cooked bitmap ({cut_bits} cut); {} GPU points, {cut_count} cut, {} mismatches",
        points.len(),
        mismatches.len()
    );
    assert!(
        mismatches.is_empty(),
        "GPU vs CPU cut: {:?}",
        &mismatches[..mismatches.len().min(8)]
    );
}

/// Brief item 1: a map without layers (no lattice cut, or one that fills none of its holes)
/// uploads the words it uploaded before, byte for byte.
#[test]
fn maps_without_layers_draw_the_same_words() {
    let mut masks = vec![
        HolesMask::default(),
        HolesMask::new(vec![HoleEllipse {
            center: [1.0, 2.0],
            radius: [3.0, 4.0],
            rotation_radians: 0.5,
        }])
        .unwrap(),
        HolesMask::from_shapes(vec![
            HoleShape::segment([0.0, 0.0], [10.0, 3.0], 4.0),
            HoleShape::Polygon {
                points: vec![[0.0, 0.0], [5.0, 0.0], [5.0, 5.0], [2.0, 1.0]],
            },
        ])
        .unwrap(),
        synthetic_mask(),
    ];
    // Every holes fixture in the crate's test data.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut fixtures = 0;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            if path.to_string_lossy().ends_with(".holes.ron")
                && let Ok(text) = std::fs::read_to_string(&path)
                && let Ok(mask) = HolesMask::from_holes_ron(&text)
            {
                masks.push(mask);
                fixtures += 1;
            }
        }
    }
    println!("{} masks ({fixtures} holes fixtures)", masks.len());
    // A cut that fills a hole none of these masks has.
    let mut other = LatticeHoleCut::new(SYN_ORIGIN, SYN_STEP, [SYN_CELLS, SYN_CELLS]);
    other.set_hole("no_such_hole", [20, 10], [320, 90], c_cut);
    for mask in &masks {
        let base = mask.gpu_words();
        assert_eq!(mask.gpu_words_with_lattice(None).unwrap(), base);
        assert_eq!(mask.gpu_words_with_lattice(Some(&other)).unwrap(), base);
    }
    // With the mouth filled, every other window keeps its words, and the mouth's window turns
    // into two lattice windows (mode 1) in its place.
    let mask = synthetic_mask();
    let base = mask.gpu_words();
    let with = mask.gpu_words_with_lattice(Some(&synthetic_cut())).unwrap();
    assert_eq!(base[0], 2, "ellipse + open named hole");
    assert_eq!(with[0], 3, "ellipse + two lattice windows");
    assert_eq!(with[4..12], base[4..12], "the ellipse's header is unchanged");
    assert_eq!(with[10], 0, "a coverage window's mode word is 0");
    assert_eq!(with[12 + 6], regia_terrain::LATTICE_WINDOW_MODE);
    assert_eq!(with[20 + 6], regia_terrain::LATTICE_WINDOW_MODE);
    let block = (256 * 256 / 4) as usize;
    let (b0, w0) = (4 + 2 * 8, 4 + 3 * 8);
    assert_eq!(
        with[w0..w0 + block],
        base[b0..b0 + block],
        "the ellipse's coverage is unchanged"
    );
    // Too many windows is refused, never truncated.
    let mut wide = LatticeHoleCut::new([0.0, 0.0], 0.25, [256 * 9, 256 * 9]);
    wide.set_hole("west_mouth", [0, 0], [256 * 9 - 4, 256 * 9 - 4], |_, _, _| true);
    assert!(matches!(
        mask.gpu_words_with_lattice(Some(&wide)),
        Err(regia_terrain::HolesError::TooManyShapes(_))
    ));
}

/// Brief item 1: `build_node_indices` drops a node triangle over a filled hole only when every
/// lattice triangle under it is cut. A C-shaped mouth (an uncut arm and one kept triangle inside
/// its coverage box, which the coverage test alone calls hole) at every level of a 4096-cell
/// quadtree whose level-0 step is the lattice pitch.
#[test]
fn index_cull_never_drops_uncut_lattice_ground() {
    let mask = synthetic_mask();
    let cut = synthetic_cut();
    let grid = 16u32;
    let root = 8u32; // 2^8 * 16 = 4096 cells: level 0 is one lattice cell per grid step.
    let span = 4096.0 * SYN_STEP;
    let origin = [SYN_ORIGIN[0] + span * 0.5, SYN_ORIGIN[1] + span * 0.5];
    let cell_of = |v: f32, o: f32| ((v - o) / SYN_STEP).floor() as i64;
    let (mut kept, mut dropped, mut nodes) = (0u32, 0u32, 0u32);
    for level in 0..=root {
        let per_side = 1u32 << (root - level);
        let node_cells = 4096 / per_side;
        let (n0, n1) = (20 / node_cells, (320 / node_cells + 1).min(per_side - 1));
        let (m0, m1) = (10 / node_cells, (90 / node_cells + 1).min(per_side - 1));
        for nz in m0..=m1 {
            for nx in n0..=n1 {
                nodes += 1;
                let node = QuadNode { level, x: nx, z: nz };
                let (min_x, min_z, size) = node.world_rect_at(span, root, origin);
                let state = node_hole_state_lattice_at(&mask, Some(&cut), node, span, root, origin);
                let indices =
                    build_node_indices_lattice_at(&mask, Some(&cut), node, span, root, grid, origin);
                let drawn: std::collections::BTreeSet<[u32; 3]> =
                    indices.as_chunks::<3>().0.iter().copied().collect();
                let step = size / grid as f32;
                let line = grid + 1;
                for gz in 0..grid {
                    for gx in 0..grid {
                        let v00 = gz * line + gx;
                        let (v01, v10) = ((gz + 1) * line + gx, v00 + 1);
                        let v11 = v01 + 1;
                        for tri in [[v00, v01, v10], [v10, v01, v11]] {
                            if state != NodeHoleState::Full && drawn.contains(&tri) {
                                kept += 1;
                                continue;
                            }
                            dropped += 1;
                            // A dropped triangle's box (shrunk a hair off the cell borders) holds
                            // only cut lattice triangles, so no uncut one lies under it.
                            let e = step * 1e-3;
                            let (x0, x1) = (min_x + gx as f32 * step + e, min_x + (gx + 1) as f32 * step - e);
                            let (z0, z1) = (min_z + gz as f32 * step + e, min_z + (gz + 1) as f32 * step - e);
                            for j in cell_of(z0, SYN_ORIGIN[1])..=cell_of(z1, SYN_ORIGIN[1]) {
                                for i in cell_of(x0, SYN_ORIGIN[0])..=cell_of(x1, SYN_ORIGIN[0]) {
                                    let all_cut = i >= 0
                                        && j >= 0
                                        && c_cut(i as u32, j as u32, 0)
                                        && c_cut(i as u32, j as u32, 1);
                                    assert!(
                                        all_cut,
                                        "level {level} node ({nx}, {nz}) cell ({gx}, {gz}) triangle {tri:?}: dropped over lattice cell ({i}, {j}), which is not cut"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    println!("{nodes} nodes: {kept} node triangles drawn, {dropped} dropped over cut ground");
    assert!(dropped > 0, "the cull still drops triangles deep inside the cut");
    // The coverage test alone (the base rule) drops a triangle over the arm.
    let node = QuadNode {
        level: 0,
        x: 200 / 16,
        z: 48 / 16,
    };
    assert!(
        regia_terrain::build_node_indices_at(&mask, node, span, root, grid, origin).len()
            < build_node_indices_lattice_at(&mask, Some(&cut), node, span, root, grid, origin).len(),
        "the arm's node keeps more triangles with the lattice"
    );
}

/// Distance from `p` to the closed polygon's boundary.
fn boundary_distance(poly: &[[f32; 2]], p: [f32; 2]) -> f32 {
    let mut best = f32::INFINITY;
    for k in 0..poly.len() {
        let (a, b) = (poly[k], poly[(k + 1) % poly.len()]);
        let (dx, dz) = (b[0] - a[0], b[1] - a[1]);
        let t = (((p[0] - a[0]) * dx + (p[1] - a[1]) * dz) / (dx * dx + dz * dz)).clamp(0.0, 1.0);
        let (qx, qz) = (a[0] + t * dx - p[0], a[1] + t * dz - p[1]);
        best = best.min((qx * qx + qz * qz).sqrt());
    }
    best
}

/// Plan R5 K10 (brief A2): render hole rects leave wedges at ramp bends. A ramp mouth of two 20 m
/// segments, 6 m wide, bent 30 degrees: its floor is the mitred hexagon. Cut as one rect per
/// segment (the base), the two rects meet at their perpendicular ends and leave a wedge at the
/// bend; cut on the lattice (the triangles whose
/// centroid is on the floor, W-10's rule) no drawn pixel is open farther than one lattice diagonal
/// outside the floor, and none is ground farther than that inside it. `terrain.wgsl`'s `in_hole`
/// on lavapipe, 4096 points over the bend.
#[test]
fn no_void_pixel_at_a_ramp_bend() {
    let half_w = 3.0f32;
    let (a, b) = ([0.0f32, 0.0f32], [20.0f32, 0.0f32]);
    let bend = 30.0f32.to_radians();
    let dir2 = [bend.cos(), bend.sin()];
    let c = [b[0] + 20.0 * dir2[0], b[1] + 20.0 * dir2[1]];
    // Left normals (+90 degrees) of each segment, and the mitre at the bend.
    let n1 = [0.0f32, 1.0f32];
    let n2 = [-dir2[1], dir2[0]];
    let mid = [n1[0] + n2[0], n1[1] + n2[1]];
    let ml = (mid[0] * mid[0] + mid[1] * mid[1]).sqrt();
    let cos_half = (bend * 0.5).cos();
    let m = [mid[0] / ml * half_w / cos_half, mid[1] / ml * half_w / cos_half];
    let floor = vec![
        [a[0] - n1[0] * half_w, a[1] - n1[1] * half_w],
        [b[0] - m[0], b[1] - m[1]],
        [c[0] - n2[0] * half_w, c[1] - n2[1] * half_w],
        [c[0] + n2[0] * half_w, c[1] + n2[1] * half_w],
        [b[0] + m[0], b[1] + m[1]],
        [a[0] + n1[0] * half_w, a[1] + n1[1] * half_w],
    ];
    let floor_shape = HoleShape::Polygon {
        points: floor.clone(),
    };
    // The base: one rect per segment (DO27's per-segment hole rects).
    let rects = HolesMask::from_shapes(vec![
        HoleShape::segment(a, b, 2.0 * half_w),
        HoleShape::segment(b, c, 2.0 * half_w),
    ])
    .unwrap();
    // The lattice: the cut is the lattice triangles whose centroid lies on the floor.
    let step = SYN_STEP;
    let origin = [-10.0f32, -15.0f32];
    let cells = 160u32;
    let mut cut = LatticeHoleCut::new(origin, step, [cells, cells]);
    let centroid = |i: u32, j: u32, t: u8| -> [f32; 2] {
        let (fx, fz) = if t == 0 {
            (1.0 / 3.0, 1.0 / 3.0)
        } else {
            (2.0 / 3.0, 2.0 / 3.0)
        };
        [
            origin[0] + (i as f32 + fx) * step,
            origin[1] + (j as f32 + fz) * step,
        ]
    };
    cut.set_hole("bend_mouth", [0, 0], [cells - 4, cells - 4], |i, j, t| {
        let p = centroid(i, j, t);
        floor_shape.contains(p[0], p[1])
    });
    let mut lattice_mask = HolesMask::default();
    lattice_mask
        .set_named(NamedHole {
            id: "bend_mouth".into(),
            open: true,
            shape: floor_shape.clone(),
        })
        .unwrap();
    let lattice_words = lattice_mask.gpu_words_with_lattice(Some(&cut)).unwrap();
    // 4096 points over the bend (12 m around it).
    let mut rng = Lcg(0xb3e7_0010);
    let points: Vec<[f32; 2]> = (0..4096)
        .map(|_| [8.0 + rng.unit() * 24.0, -9.0 + rng.unit() * 24.0])
        .collect();
    let diagonal = step * std::f32::consts::SQRT_2;
    let count = |verdicts: &[bool]| -> (u32, u32) {
        let (mut void, mut lid) = (0u32, 0u32);
        for (p, &open) in points.iter().zip(verdicts) {
            let on_floor = floor_shape.contains(p[0], p[1]);
            let d = boundary_distance(&floor, *p);
            if open && !on_floor && d > diagonal {
                void += 1;
            }
            if !open && on_floor && d > diagonal {
                lid += 1;
            }
        }
        (void, lid)
    };
    let (rect_void, rect_lid) = count(&gpu_in_hole(&rects.gpu_words(), &points));
    let (lat_void, lat_lid) = count(&gpu_in_hole(&lattice_words, &points));
    println!(
        "ramp bend: one rect per segment: {rect_void} void, {rect_lid} lid points; lattice: {lat_void} void, {lat_lid} lid points"
    );
    assert!(
        rect_void + rect_lid > 0,
        "the control (one rect per segment) must leave a wedge at the bend (the rects meet at their perpendicular ends)"
    );
    assert_eq!(lat_void, 0, "a drawn pixel is open past the floor at the bend");
    assert_eq!(lat_lid, 0, "a drawn pixel covers the floor at the bend");
}
