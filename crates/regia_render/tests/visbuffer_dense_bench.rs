// SPDX-License-Identifier: MPL-2.0
//! What each opaque geometry path costs on a dense scene (ADR-2071): classic forward, the
//! visibility buffer with its per-material resolve, the single-pass resolve, and the software
//! raster. Ignored in the gate: a wall-clock bench prints, it does not assert.
//!
//! ```sh
//! # lavapipe, the small sizes:
//! VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json cargo test --release -p regia_render \
//!     --test visbuffer_dense_bench -- --ignored --nocapture
//! # A card (book/agents/visbuffer.md has the commands for each machine):
//! REGIA_VIS_BENCH_SIZES=1920x1080 REGIA_VIS_BENCH_SCENES=1024x2048,4096x2048,4096x8192,16384x2048 \
//!     cargo test --release -p regia_render --test visbuffer_dense_bench -- --ignored --nocapture
//! ```
//!
//! A scene is `<instances>x<triangles per instance>` over 64 materials, half of them textured. Each
//! run prints one row per path: mean frame time with readback, the GPU stamps of the geometry span
//! and the forward pass, the resolve's draws, the clusters drawn and the instances on the path,
//! and the scene's density (`DensityMetrics`, ADR-2689): triangles per pixel, meshes per million
//! pixels and clusters per 16 px tile, the units `book/agents/visbuffer.md` chooses thresholds in.
//! After the scenes of each size it prints each resolve's crossover (`DenseCrossover`, ADR-3149):
//! the density from which the path wins every denser scene, and the gate thresholds it gives.
//! `REGIA_VIS_BENCH_ROUNDS` repeats the paths interleaved and reads the medians.
//!
//! `REGIA_VIS_BENCH_MASKED=1` makes all 64 materials masked (ADR-3915): each has a base color with a
//! holed alpha, drawn double-sided, so the rows price the alpha-tested raster against the forward
//! alpha test; the row's `masked` numbers say how many instances took it.
//!
//! `REGIA_VIS_BENCH_SWAY=1` adopts every material with `FoliageSway` and blows a wind with the clock
//! running (ADR-3916), alone or with the masked flag; the row's `sway` number says how many instances
//! took the path. With the frame hookup of ADR-3916 the visibility rows sway; without it they fall back
//! to the classic path's cost.
//!
//! `REGIA_VIS_BENCH_RESIDENCY=1` runs the visibility rows with cluster residency at the Desktop High
//! budget (`ClusterResidency::for_preset`, ADR-3118), as the dense-scene default would ship it
//! (ADR-4906); the row's `resident` numbers say what the cluster data held. Every visibility row runs
//! the LOD band of ADR-4631 (`VisBufferSettings::lod_hysteresis_px` unset); `lod switches` is the
//! last frame's count.

use std::sync::Arc;

use regia_asset_pipeline::voxel_far_cook::{
    VoxelCoverage, VoxelFarCookSettings, VoxelizeInput, voxelize_with_coverage,
};
use regia_asset_pipeline::{MeshletAttributes, MeshletSettings, build_meshlet_mesh_with_attributes};
use regia_render::visbuffer::{
    ClusterResidency, DENSE_CROSSOVER_MARGIN, DenseBenchRow, DenseCrossover, DensityMetrics, MeshletBuilder,
    MeshletMesh, VisBufferSettings, VoxelFarProxy, VoxelFarSettings,
};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial, LabMesh,
    LabScene, LabTexture, LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
    VisibilityPath, VisibilitySelect, uv_sphere,
};

fn checker(size: u32, tint: u8) -> LabTexture {
    let mut pixels: Vec<u8> = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let on = (x / 4 + y / 4).is_multiple_of(2);
            pixels.extend_from_slice(&if on {
                [200, tint, 80, 255]
            } else {
                [40, 80, tint, 255]
            });
        }
    }
    LabTexture {
        width: size,
        height: size,
        rgba: pixels,
    }
}

/// `checker` with the alpha of every dark cell at 0: the masked scene's holes.
fn holed_checker(size: u32, tint: u8) -> LabTexture {
    let mut texture = checker(size, tint);
    for texel in texture.rgba.as_chunks_mut::<4>().0 {
        if texel[0] == 40 {
            texel[3] = 0;
        }
    }
    texture
}

/// One mesh and the materials; [`instanced`] repeats the mesh, as a scatter shares one mesh.
fn dense_scene(width: u32, height: u32, instances: u32, triangles: u32, masked: bool) -> LabScene {
    // A sphere's triangle count is about 2 × segments × rings.
    let segments = ((f64::from(triangles)).sqrt() * 1.4).max(4.0) as u32;
    let rings = (segments / 2).max(3);
    let (vertices, indices) = uv_sphere(0.45, segments, rings);
    let side = f64::from(instances).sqrt().ceil() as u32;
    let materials: Vec<LabMaterial> = (0..64u32)
        .map(|m| {
            let opaque = LabMaterial {
                base_color: [0.3 + (m % 7) as f32 * 0.1, 0.3 + (m % 5) as f32 * 0.1, 0.5, 1.0],
                roughness: 0.3 + (m % 4) as f32 * 0.15,
                albedo: m
                    .is_multiple_of(2)
                    .then(|| checker(32 << (m % 3), 60 + (m * 3) as u8)),
                ..LabMaterial::default()
            };
            if masked {
                LabMaterial {
                    albedo: Some(holed_checker(32 << (m % 3), 60 + (m * 3) as u8)),
                    alpha_mode: regia_render::ALPHA_MASK,
                    alpha_cutoff: 0.5,
                    double_sided: true,
                    ..opaque
                }
            } else {
                opaque
            }
        })
        .collect();
    LabScene {
        width,
        height,
        eye: [0.0, side as f32 * 0.3 + 3.0, 6.0],
        target: [0.0, 0.0, -(side as f32) * 0.5],
        fov_y: 1.0,
        near: 0.1,
        far: 2000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: Some(PostSettings::golden_deterministic()),
        directional: DirectionalLightUniform::new([0.3, -1.0, -0.4], [1.0; 3], 100_000.0),
        local_lights: Vec::new(),
        materials,
        meshes: vec![LabMesh {
            vertices,
            indices,
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

/// `lab`'s one mesh `instances` times over a grid, cycling through the materials: one mesh key, so
/// both paths share one vertex buffer or one cluster range.
fn instanced<'a>(lab: &'a LabScene, quality: FrameQuality, instances: u32) -> FrameScene<'a> {
    let mut scene = lab.as_frame_scene_with_quality(quality);
    let template = scene.meshes[0];
    let side = f64::from(instances).sqrt().ceil() as u32;
    scene.meshes = (0..instances)
        .map(|i| {
            let mut model = [[0.0f32; 4]; 4];
            for (axis, row) in model.iter_mut().enumerate() {
                row[axis] = 1.0;
            }
            model[3] = [
                (i % side) as f32 - side as f32 * 0.5,
                0.5,
                -((i / side) as f32),
                1.0,
            ];
            let material = (i as usize) % lab.materials.len();
            FrameMesh {
                mesh_key: 1,
                material_key: 7000 + material as u64,
                object_key: 100_000 + u64::from(i),
                material: &lab.materials[material],
                model,
                ..template
            }
        })
        .collect();
    scene
}

/// The cooker's cluster hierarchy, as a game's cooked meshes have it.
fn cooked_builder() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
        let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
        build_meshlet_mesh_with_attributes(
            &positions,
            Some(MeshletAttributes {
                normals: &normals,
                uvs: &uvs,
            }),
            indices,
            &MeshletSettings::default(),
        )
        .ok()
    })
}

#[derive(Clone, Copy)]
enum Path {
    Classic,
    PerMaterial,
    SinglePass,
    Software,
}

impl Path {
    fn name(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::PerMaterial => "vis_per_material",
            Self::SinglePass => "vis_single_pass",
            Self::Software => "vis_single_software",
        }
    }
}

/// Runs `path` on the scene and prints its row; returns the mean GPU frame milliseconds and the
/// scene's density.
fn run(
    device: &RenderDevice,
    lab: &LabScene,
    instances: u32,
    path: Path,
    frames: u32,
    residency: Option<ClusterResidency>,
) -> (f64, DensityMetrics) {
    let mut renderer = FrameRenderer::new();
    renderer.set_gpu_timing(device, true);
    // The tier this path is for. Classic draws every instance at full detail: no LOD chain here, so
    // read its rows as the unmanaged cost, not a game's.
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.visibility_buffer = !matches!(path, Path::Classic);
    quality.visibility_path = VisibilityPath {
        select: VisibilitySelect::Always,
        single_pass_resolve: matches!(path, Path::SinglePass | Path::Software),
    };
    if !matches!(path, Path::Classic) {
        renderer.set_meshlet_builder(Some(cooked_builder()));
    }
    if !matches!(path, Path::Classic) {
        renderer.set_visbuffer_settings(VisBufferSettings {
            software_raster: matches!(path, Path::Software),
            residency,
            ..VisBufferSettings::default()
        });
    }
    let scene = instanced(lab, quality, instances);
    // A fixed step, so the rows sway the same; a still material ignores both.
    renderer.set_foliage_wind(Some([6.0, 2.0]));
    let mut clock = 0.0f32;
    let mut tick = |renderer: &mut FrameRenderer| {
        renderer.set_material_clock(clock + 1.0 / 60.0, clock);
        clock += 1.0 / 60.0;
    };
    tick(&mut renderer);
    let first = std::time::Instant::now();
    renderer.render_offscreen(device, &scene).expect("first frame");
    let first_ms = first.elapsed().as_secs_f64() * 1000.0;
    // Settle (temporal histories, late timestamp slots), then time.
    for _ in 0..4 {
        tick(&mut renderer);
        renderer.render_offscreen(device, &scene).expect("frame");
    }
    let (mut wall, mut geometry, mut forward, mut gpu) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for _ in 0..frames {
        tick(&mut renderer);
        let start = std::time::Instant::now();
        renderer.render_offscreen(device, &scene).expect("frame");
        wall += start.elapsed().as_secs_f64() * 1000.0;
        let timings = renderer.gpu_timings();
        geometry += f64::from(timings.cull_ms);
        forward += f64::from(timings.forward_ms);
        gpu += f64::from(timings.frame_ms);
    }
    let n = f64::from(frames);
    let resolve = renderer.visbuffer_resolve_report();
    let counters = renderer
        .visbuffer_counters(device)
        .ok()
        .flatten()
        .unwrap_or_default();
    let stats = renderer.visbuffer_stats();
    let arena = renderer.visbuffer_arena_stats();
    // The density the row ran at, in the units a preset's thresholds are chosen in (ADR-2689).
    let index_count = scene.meshes.first().map_or(0, |mesh| mesh.indices.len());
    let density = regia_render::visbuffer::SceneDensity {
        triangles: (index_count / 3) as u64 * u64::from(instances),
        instances,
    };
    let on_path = !matches!(path, Path::Classic);
    let metrics = DensityMetrics::of_frame(density, on_path.then_some(counters), scene.width, scene.height);
    println!(
        "{:>20}: first {first_ms:9.1} ms, frame {:8.2} ms, gpu {:8.3} ms, geometry {:8.3} ms, forward {:8.3} ms; \
         resolve draws {}, single {}, fallback {:?}; clusters {}+{}, software {}; instances {}; \
         density {:.3} tri/px, {:.1} inst/Mpx, {:.2} clusters/tile; \
         masked {} instances, {} clusters, tested {}, sway {} instances; \
         lod switches {}, resident {:.1} MiB (peak {:.1} MiB, budget {:.0} MiB)",
        path.name(),
        wall / n,
        gpu / n,
        geometry / n,
        forward / n,
        resolve.draws,
        resolve.single_pass,
        resolve.fallback.map(regia_render::SinglePassFallback::as_str),
        counters.phase1_clusters,
        counters.phase2_clusters,
        counters.software_clusters,
        stats.instances,
        metrics.triangles_per_pixel,
        metrics.instances_per_megapixel,
        metrics.clusters_per_tile,
        stats.masked_instances,
        counters.masked_clusters,
        stats.masked_tested,
        stats.sway_instances,
        stats.lod_switches,
        mib(arena.residency.resident_bytes),
        mib(arena.residency.peak_resident_bytes),
        mib(arena.residency.config.budget_bytes),
    );
    (gpu / n, metrics)
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1u32 << 20)
}

/// The median of `values` (the mean of the middle two for an even count); 0 for none.
fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    match values.len() {
        0 => 0.0,
        n if !n.is_multiple_of(2) => values[n / 2],
        n => (values[n / 2 - 1] + values[n / 2]) / 2.0,
    }
}

/// Prints where `path` starts to win (ADR-3149) and the gate thresholds that density gives at
/// the floor (1080p), mid (1440p) and 4K frame sizes. The lead pastes these lines into
/// `docs/perf-log.md`; `book/agents/visbuffer.md` says how two cards' lines become the default.
fn print_crossover(label: &str, rows: &[DenseBenchRow]) {
    match DenseCrossover::of_rows(rows, DENSE_CROSSOVER_MARGIN) {
        None => println!("crossover {label}: none (the path does not win the densest row)"),
        Some(c) => {
            let at = |w, h| {
                let s = c.select_at(w, h);
                format!(
                    "{w}x{h} {} tris / {} meshes",
                    s.enter_triangles, s.enter_instances
                )
            };
            println!(
                "crossover {label}: {:.3} tri/px, {:.1} inst/Mpx, {} rows won, bracketed {}; thresholds {}, {}, {}",
                c.triangles_per_pixel,
                c.instances_per_megapixel,
                c.rows_won,
                c.bracketed,
                at(1920, 1080),
                at(2560, 1440),
                at(3840, 2160),
            );
        }
    }
}

fn pairs(name: &str, default: &str) -> Vec<(u32, u32)> {
    regia_core::brand::env_var(name)
        .unwrap_or_else(|_| default.to_string())
        .split(',')
        .filter_map(|pair| {
            let (a, b) = pair.trim().split_once('x')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .collect()
}

#[test]
#[ignore = "wall-clock bench: run by hand on the machine under test, --nocapture"]
fn visbuffer_dense_bench() {
    regia_render::texture_table::set_device_level_request(regia_render::texture_table::TEXTURE_TABLE_LEVEL);
    let device = RenderDevice::headless().expect("device");
    let info = device.adapter.get_info();
    println!("adapter: {} ({:?}, {:?})", info.name, info.backend, info.driver);
    println!(
        "table: {:?}",
        regia_render::texture_table::device_support(&device.device)
    );
    let sizes = pairs("REGIA_VIS_BENCH_SIZES", "640x360");
    let scenes = pairs("REGIA_VIS_BENCH_SCENES", "64x2048,256x2048,1024x512");
    let frames = regia_core::brand::env_var("REGIA_VIS_BENCH_FRAMES")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(8);
    // Rounds run the paths interleaved (classic, per material, single pass, software, then again),
    // so a card's clock or a neighbor's load moves every path alike; the crossover reads the
    // median of the rounds.
    let rounds: u32 = regia_core::brand::env_var("REGIA_VIS_BENCH_ROUNDS")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(1)
        .max(1);
    const PATHS: [Path; 4] = [Path::Classic, Path::PerMaterial, Path::SinglePass, Path::Software];

    let masked = regia_core::brand::env_var("REGIA_VIS_BENCH_MASKED").is_ok_and(|text| text == "1");
    let sway = regia_core::brand::env_var("REGIA_VIS_BENCH_SWAY").is_ok_and(|text| text == "1");
    let residency = regia_core::brand::env_var("REGIA_VIS_BENCH_RESIDENCY")
        .is_ok_and(|text| text == "1")
        .then(|| ClusterResidency::for_preset(RenderProfilePreset::DesktopHigh));
    println!("residency: {residency:?}");
    for (width, height) in sizes {
        // Per path after classic: one row per scene for the crossover.
        let mut rows: [Vec<DenseBenchRow>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        for &(instances, triangles) in &scenes {
            let mut lab = dense_scene(width, height, instances, triangles, masked);
            if sway {
                for material in &mut lab.materials {
                    *material = regia_render::FoliageSway::default()
                        .adopt(material)
                        .expect("a plain material");
                }
            }
            let total = lab.meshes[0].indices.len() / 3 * instances as usize;
            println!(
                "--- {width}x{height}, {instances} instances, {total} triangles, {} {}materials, {frames} frames, {rounds} rounds",
                lab.materials.len(),
                match (sway, masked) {
                    (true, true) => "swaying masked ",
                    (true, false) => "swaying ",
                    (false, true) => "masked ",
                    (false, false) => "",
                },
            );
            let mut gpu: [Vec<f64>; 4] = Default::default();
            let mut density = DensityMetrics::default();
            for _ in 0..rounds {
                for (k, path) in PATHS.into_iter().enumerate() {
                    let (ms, metrics) = run(&device, &lab, instances, path, frames, residency);
                    gpu[k].push(ms);
                    if k == 0 {
                        density = metrics;
                    }
                }
            }
            let medians: Vec<f64> = gpu.iter_mut().map(|v| median(v.as_mut_slice())).collect();
            println!(
                "median gpu ms: classic {:.3}, per material {:.3}, single pass {:.3}, software {:.3}",
                medians[0], medians[1], medians[2], medians[3]
            );
            for (k, row) in rows.iter_mut().enumerate() {
                row.push(DenseBenchRow {
                    density,
                    classic_ms: medians[0],
                    path_ms: medians[k + 1],
                });
            }
        }
        println!(
            "=== {width}x{height} crossovers (margin {:.0} %)",
            DENSE_CROSSOVER_MARGIN * 100.0
        );
        for (label, row) in ["per_material", "single_pass", "software"].iter().zip(&rows) {
            print_crossover(label, row);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// A synthetic forest (ADR-3108): procedural trunk and leaf-card trees, no external assets, the
// crown masked and cooked with its voxel far levels (alpha-tested voxelization), scattered at
// 100 000 instances by default. `crown_coverage_*` and `forest_cost_table_counts` are counted CPU
// checks; `forest_voxel_far_bench` is the card run (ignored, prints).
// ---------------------------------------------------------------------------------------------

/// Leaf mask texels per side.
const LEAF_TEX: u32 = 64;
/// The leaf material's alpha cutoff.
const LEAF_CUTOFF: f32 = 0.5;
/// Cluster LOD threshold of the forest rows, pixels (the visibility buffer's default).
const FOREST_THRESHOLD_PX: f32 = 1.0;
const FOREST_FOV: f32 = 1.0;
const TRUNK_KEY: u64 = 9101;
const CROWN_KEY: u64 = 9102;

/// A fixed LCG in 0..1.
struct Lcg(u32);

impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 8) as f32 / (1u32 << 24) as f32
    }
}

fn cross3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize3(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-6);
    v.map(|c| c / l)
}

/// The leaf card texture: six elliptic leaves around a stem, RGB green, alpha 255 on a leaf and 0
/// between them (about 40 % covered).
fn leaf_texture() -> LabTexture {
    let n = LEAF_TEX as usize;
    let mut rgba = Vec::with_capacity(n * n * 4);
    for y in 0..n {
        for x in 0..n {
            let (u, v) = ((x as f32 + 0.5) / n as f32, (y as f32 + 0.5) / n as f32);
            let mut inside = (u - 0.5).abs() < 0.015 && v > 0.3;
            for k in 0..6 {
                let side = if k % 2 == 0 { -1.0 } else { 1.0 };
                let (cx, cy) = (0.5 + side * 0.19, 0.18 + 0.27 * (k / 2) as f32);
                let (a, b) = (u - cx, v - cy);
                // Leaves lean 35 degrees out from the stem.
                let (s, c) = (0.574 * side, 0.819);
                let (p, q) = (a * c - b * s, a * s + b * c);
                inside |= (p / 0.17).powi(2) + (q / 0.09).powi(2) < 1.0;
            }
            let shade = (90.0 + 60.0 * v) as u8;
            rgba.extend_from_slice(&[40, shade, 30, if inside { 255 } else { 0 }]);
        }
    }
    LabTexture {
        width: LEAF_TEX,
        height: LEAF_TEX,
        rgba,
    }
}

/// One procedural tree: a tapered trunk (opaque) and a crown of bent leaf cards (masked).
struct Tree {
    trunk: (Vec<LabVertex>, Vec<u32>),
    crown: (Vec<LabVertex>, Vec<u32>),
}

fn tree(cards: u32) -> Tree {
    // Trunk: 10 sides, 8 rings, 5 m, radius 0.28 to 0.08.
    let (sides, rings, height) = (10u32, 8u32, 5.0f32);
    let mut trunk_v = Vec::new();
    let mut trunk_i = Vec::new();
    for r in 0..=rings {
        let t = r as f32 / rings as f32;
        let radius = 0.28 + (0.08 - 0.28) * t;
        for s in 0..=sides {
            let a = std::f32::consts::TAU * s as f32 / sides as f32;
            let n = [a.cos(), 0.05, a.sin()];
            trunk_v.push(LabVertex::from_pos_normal_uv(
                [a.cos() * radius, t * height, a.sin() * radius],
                normalize3(n),
                [s as f32 / sides as f32, t * 4.0],
            ));
        }
    }
    for r in 0..rings {
        for s in 0..sides {
            let a = r * (sides + 1) + s;
            let b = a + sides + 1;
            trunk_i.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
        }
    }
    // Crown: cards of 0.9 m (2 x 2 quads, bent) in an ellipsoid around (0, 5.6, 0).
    let mut rng = Lcg(0x5eed_7ee5);
    let center = CROWN_CENTER;
    let radii = CROWN_RADII;
    let mut crown_v = Vec::new();
    let mut crown_i = Vec::new();
    for _ in 0..cards {
        let (mut d, mut l2) = ([0.0f32; 3], 2.0f32);
        while l2 > 1.0 || l2 < 0.09 {
            d = [
                rng.next() * 2.0 - 1.0,
                rng.next() * 2.0 - 1.0,
                rng.next() * 2.0 - 1.0,
            ];
            l2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
        }
        let c: [f32; 3] = std::array::from_fn(|k| center[k] + d[k] * radii[k]);
        let jitter = [rng.next() - 0.5, rng.next() - 0.5, rng.next() - 0.5];
        let normal = normalize3(std::array::from_fn(|k| d[k] + jitter[k] * 0.8));
        let a = if normal[1].abs() < 0.9 {
            [0.0, 1.0, 0.0]
        } else {
            [1.0, 0.0, 0.0]
        };
        let t = normalize3(cross3(normal, a)).map(|v| v * 0.45);
        let b = cross3(normal, t);
        let base = crown_v.len() as u32;
        for j in 0..=2u32 {
            for i in 0..=2u32 {
                let (u, v) = (i as f32 / 2.0, j as f32 / 2.0);
                let (su, sv) = (u * 2.0 - 1.0, v * 2.0 - 1.0);
                let bend = 0.08 * (1.0 - su * su);
                let p = std::array::from_fn(|k| c[k] + t[k] * su + b[k] * sv + normal[k] * bend);
                crown_v.push(LabVertex::from_pos_normal_uv(p, normal, [u, 1.0 - v]));
            }
        }
        for j in 0..2u32 {
            for i in 0..2u32 {
                let a = base + j * 3 + i;
                let b = a + 3;
                crown_i.extend_from_slice(&[a, a + 1, b, a + 1, b + 1, b]);
            }
        }
    }
    Tree {
        trunk: (trunk_v, trunk_i),
        crown: (crown_v, crown_i),
    }
}

/// The cooker's cluster DAG of a mesh.
fn cook_dag(vertices: &[LabVertex], indices: &[u32]) -> MeshletMesh {
    let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
    let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
    let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
    build_meshlet_mesh_with_attributes(
        &positions,
        Some(MeshletAttributes {
            normals: &normals,
            uvs: &uvs,
        }),
        indices,
        &MeshletSettings::default(),
    )
    .expect("cluster DAG")
}

fn leaf_alpha(texture: &LabTexture) -> Vec<u8> {
    texture.rgba.as_chunks::<4>().0.iter().map(|px| px[3]).collect()
}

fn leaf_coverage(alpha: &[u8]) -> VoxelCoverage<'_> {
    VoxelCoverage {
        width: LEAF_TEX,
        height: LEAF_TEX,
        alpha,
        factor: 1.0,
        cutoff: LEAF_CUTOFF,
        vertex_alpha: true,
        uv_transform: regia_asset_pipeline::gltf_extensions::TextureTransform::IDENTITY,
    }
}

/// The crown's voxel proxy: alpha-tested (`masked`) or by triangle area (the cook before
/// ADR-3108).
fn cook_crown_proxy(crown: &(Vec<LabVertex>, Vec<u32>), alpha: &[u8], masked: bool) -> VoxelFarProxy {
    let (vertices, indices) = crown;
    let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
    let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
    let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
    let colors: Vec<[f32; 4]> = vertices.iter().map(|v| v.color).collect();
    let input = VoxelizeInput {
        positions: &positions,
        normals: &normals,
        uvs: &uvs,
        colors: &colors,
        indices,
    };
    let coverage = leaf_coverage(alpha);
    voxelize_with_coverage(
        "crown",
        &input,
        masked.then_some(&coverage),
        &VoxelFarCookSettings { resolution: 32 },
    )
    .expect("voxelize the crown")
}

/// Triangles (positions, uvs) of the cut `lod_error <= t < parent_error`.
fn cut_at(mesh: &MeshletMesh, vertices: &[LabVertex], t: f32) -> Vec<[([f32; 3], [f32; 2]); 3]> {
    let mut out = Vec::new();
    for m in mesh
        .meshlets
        .iter()
        .filter(|m| m.lod_error <= t && m.parent_error > t)
    {
        for k in 0..m.triangle_count {
            let packed = mesh.triangles[(m.triangle_offset + k) as usize];
            out.push(std::array::from_fn(|corner| {
                let local = (packed >> (corner * 8)) & 0xff;
                let v = &vertices[mesh.vertices[(m.vertex_offset + local) as usize] as usize];
                (v.position, v.uv)
            }));
        }
    }
    out
}

/// The crown's ellipsoid (centre and radii) the cards are scattered in.
const CROWN_CENTER: [f32; 3] = [0.0, 5.6, 0.0];
const CROWN_RADII: [f32; 3] = [2.3, 2.0, 2.3];

/// The covered samples of an orthographic view along `axis` of the crown's square (`center` ±
/// `half`), `samples`² of them, where a triangle passes the alpha test at the sample's interpolated
/// UV (`tested`) or anywhere (`!tested`).
fn ortho_mask(
    triangles: &[[([f32; 3], [f32; 2]); 3]],
    test: &VoxelCoverage<'_>,
    tested: bool,
    axis: usize,
    (center, half, samples): ([f32; 3], f32, usize),
) -> Vec<bool> {
    let (ax, ay) = ((axis + 1) % 3, (axis + 2) % 3);
    let mut covered = vec![false; samples * samples];
    let scale = samples as f32 / (2.0 * half);
    let to_px = |v: f32, k: usize| (v - center[k] + half) * scale;
    for tri in triangles {
        let p = tri.map(|(pos, _)| [to_px(pos[ax], ax), to_px(pos[ay], ay)]);
        let area = (p[1][0] - p[0][0]) * (p[2][1] - p[0][1]) - (p[2][0] - p[0][0]) * (p[1][1] - p[0][1]);
        if area.abs() < 1e-12 {
            continue;
        }
        let lo = |k: usize| p.iter().map(|q| q[k]).fold(f32::MAX, f32::min).floor().max(0.0) as usize;
        let hi = |k: usize| {
            (p.iter().map(|q| q[k]).fold(f32::MIN, f32::max).ceil().max(0.0) as usize).min(samples)
        };
        for y in lo(1)..hi(1) {
            for x in lo(0)..hi(0) {
                if covered[y * samples + x] {
                    continue;
                }
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let w = |a: [f32; 2], b: [f32; 2]| {
                    ((b[0] - a[0]) * (py - a[1]) - (px - a[0]) * (b[1] - a[1])) / area
                };
                let (w0, w1, w2) = (w(p[1], p[2]), w(p[2], p[0]), w(p[0], p[1]));
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let uv: [f32; 2] =
                    std::array::from_fn(|k| tri[0].1[k] * w0 + tri[1].1[k] * w1 + tri[2].1[k] * w2);
                if !tested || test.passes(uv, 1.0) {
                    covered[y * samples + x] = true;
                }
            }
        }
    }
    covered
}

/// (covered share of the whole view, covered share of the crown's interior) over the three axis
/// views. The interior is the crown ellipse's silhouette shrunk by `shrink` world units on each
/// radius: where a level's own error (its silhouette allowance) cannot reach, so what remains is
/// how see-through the crown is. `None` for the interior when the shrunk ellipse is too small.
fn crown_coverage(
    triangles: &[[([f32; 3], [f32; 2]); 3]],
    test: &VoxelCoverage<'_>,
    tested: bool,
    view: ([f32; 3], f32, usize),
    shrink: f32,
) -> (f64, Option<f64>) {
    let (center, half, samples) = view;
    let (mut all, mut inside, mut inside_n) = (0usize, 0usize, 0usize);
    let mut interior_ok = true;
    for axis in 0..3 {
        let (ax, ay) = ((axis + 1) % 3, (axis + 2) % 3);
        let (rx, ry) = (CROWN_RADII[ax] - shrink, CROWN_RADII[ay] - shrink);
        interior_ok &= rx > 0.5 && ry > 0.5;
        let mask = ortho_mask(triangles, test, tested, axis, view);
        for (i, &covered) in mask.iter().enumerate() {
            all += usize::from(covered);
            let step = 2.0 * half / samples as f32;
            let wx = center[ax] - half + ((i % samples) as f32 + 0.5) * step - CROWN_CENTER[ax];
            let wy = center[ay] - half + ((i / samples) as f32 + 0.5) * step - CROWN_CENTER[ay];
            if interior_ok && (wx / rx).powi(2) + (wy / ry).powi(2) <= 1.0 {
                inside_n += 1;
                inside += usize::from(covered);
            }
        }
    }
    let whole = all as f64 / (3 * samples * samples) as f64;
    let interior = (interior_ok && inside_n >= 64).then(|| inside as f64 / inside_n as f64);
    (whole, interior)
}

/// How far a voxel level's coverage of the crown's interior may stray from the full-detail crown's
/// at the level's switch distance: 25 %, either way (ADR-3108).
const CROWN_COVERAGE_BOUND: f64 = 0.25;

/// A see-through crown (300 leaf cards) at each voxel level's switch distance: inside the crown's
/// silhouette shrunk by the level's error (the silhouette ring is the level's allowed error, which
/// on a crown a few pixels across is most of the crown), the voxel level covers what the
/// full-detail crown covers within [`CROWN_COVERAGE_BOUND`], so the alpha-tested cook neither fills
/// the crown into a blob nor thins it away. The cook by triangle area (before ADR-3108) fails the
/// same bound: untested it fills the interior, alpha-tested at its representative texture
/// coordinate it thins it. Views: orthographic along the three axes at the pixel size where the
/// level's error is `FOREST_THRESHOLD_PX` (its switch), 4 × 4 samples per pixel; the near crown is
/// alpha-tested at every sample, the voxel faces at their representative texture coordinate (the
/// masked raster) and untested (without it). Levels whose shrunk crown is too small to measure are
/// printed, not asserted.
#[test]
fn crown_coverage_at_the_switch_distance_matches_the_near_level() {
    let tree = tree(300);
    let texture = leaf_texture();
    let alpha = leaf_alpha(&texture);
    let test = leaf_coverage(&alpha);
    let (vertices, indices) = &tree.crown;
    let dag = cook_dag(vertices, indices);
    let half = 3.0f32;
    let near = cut_at(&dag, vertices, 0.0);
    let mut worst = 0.0f64;
    let mut measured = 0;
    let mut area_worst = 0.0f64;
    for (masked, name) in [(true, "alpha-tested"), (false, "by area")] {
        let proxy = cook_crown_proxy(&tree.crown, &alpha, masked);
        let far = proxy
            .extend_meshlets(&dag, vertices, &VoxelFarSettings::default())
            .expect("extend");
        assert!(!far.levels.is_empty(), "{name}: the crown has voxel levels");
        for (index, level) in far.levels.iter().enumerate() {
            let pixel = level.error / FOREST_THRESHOLD_PX;
            let samples = ((2.0 * half / pixel).ceil() as usize * 4).clamp(16, 1024);
            let view = (CROWN_CENTER, half, samples);
            let shrink = level.error;
            let (near_all, near_in) = crown_coverage(&near, &test, true, view, shrink);
            let voxels = cut_at(&far.meshlets, &far.vertices, level.error);
            let (tested_all, tested_in) = crown_coverage(&voxels, &test, true, view, shrink);
            let (untested_all, untested_in) = crown_coverage(&voxels, &test, false, view, shrink);
            let ratio = |v: Option<f64>| v.zip(near_in).map(|(v, n)| v / n.max(1e-9));
            println!(
                "{name}: voxel level {index} (error {:.3} m, {} px across): whole view near {:.3}, tested {:.1} %, \
                 untested {:.1} %; interior near {:?}, tested {:?}, untested {:?}",
                level.error,
                samples / 4,
                near_all,
                tested_all / near_all * 100.0,
                untested_all / near_all * 100.0,
                near_in,
                ratio(tested_in).map(|r| (r * 1000.0).round() / 10.0),
                ratio(untested_in).map(|r| (r * 1000.0).round() / 10.0),
            );
            let (Some(t), Some(u)) = (ratio(tested_in), ratio(untested_in)) else {
                continue;
            };
            if masked {
                worst = worst.max((t - 1.0).abs()).max((u - 1.0).abs());
                measured += 1;
            } else {
                area_worst = area_worst.max((t - 1.0).abs()).max((u - 1.0).abs());
            }
        }
    }
    assert!(
        measured >= 2,
        "at least two levels have a measurable interior ({measured})"
    );
    assert!(
        worst <= CROWN_COVERAGE_BOUND,
        "a voxel level's crown interior strays {:.1} % from the near crown's",
        worst * 100.0
    );
    assert!(
        area_worst > CROWN_COVERAGE_BOUND,
        "the area cook should fail the bound (blob or thinning): worst {:.1} %",
        area_worst * 100.0
    );
}

/// `count` tree transforms on a jittered 7 m grid in front of the camera (z from -4 m away),
/// random yaw, scale 0.8 to 1.2: the scatter a forest tool places.
fn forest_models(count: u32) -> Vec<[[f32; 4]; 4]> {
    let mut rng = Lcg(0xf0_2e57);
    let side = f64::from(count).sqrt().ceil() as u32;
    (0..count)
        .map(|i| {
            let (row, col) = ((i / side) as f32, (i % side) as f32);
            let x = (col - side as f32 * 0.5 + rng.next() * 0.8) * 7.0;
            let z = -(row + rng.next() * 0.8) * 7.0 - 4.0;
            let yaw = rng.next() * std::f32::consts::TAU;
            let s = 0.8 + rng.next() * 0.4;
            let (sin, cos) = yaw.sin_cos();
            [
                [cos * s, 0.0, -sin * s, 0.0],
                [0.0, s, 0.0, 0.0],
                [sin * s, 0.0, cos * s, 0.0],
                [x, 0.0, z, 1.0],
            ]
        })
        .collect()
}

/// The forest camera: eye at the scatter's front edge, looking down the rows.
const FOREST_EYE: [f32; 3] = [0.0, 3.0, 2.0];
const FOREST_TARGET: [f32; 3] = [0.0, 6.0, -60.0];

/// The tiers of the cost table: (name, preset, output size).
const FOREST_TIERS: [(&str, RenderProfilePreset, u32, u32); 6] = [
    ("mobile_low", RenderProfilePreset::MobileLow, 1280, 720),
    ("mobile_high", RenderProfilePreset::MobileHigh, 1920, 1080),
    ("deck", RenderProfilePreset::Deck, 1280, 800),
    ("desktop_low", RenderProfilePreset::DesktopLow, 1920, 1080),
    ("desktop_high", RenderProfilePreset::DesktopHigh, 1920, 1080),
    ("desktop_ultra", RenderProfilePreset::DesktopUltra, 3840, 2160),
];

/// The render height the cull projects errors with: the output height at the tier's render scale.
fn tier_render_height(preset: RenderProfilePreset, height: u32) -> u32 {
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
    ((u64::from(height) * u64::from(quality.render_scale_permille) / 1000) as u32).max(1)
}

/// The forest's meshes, cooked once: trunk DAG, crown DAG, and the crown DAG with its alpha-tested
/// voxel levels.
struct Forest {
    tree: Tree,
    texture: LabTexture,
    trunk_dag: MeshletMesh,
    crown_dag: MeshletMesh,
    far: regia_render::visbuffer::VoxelFarMesh,
}

fn forest() -> Forest {
    let tree = tree(900);
    let texture = leaf_texture();
    let alpha = leaf_alpha(&texture);
    let trunk_dag = cook_dag(&tree.trunk.0, &tree.trunk.1);
    let crown_dag = cook_dag(&tree.crown.0, &tree.crown.1);
    let far = cook_crown_proxy(&tree.crown, &alpha, true)
        .extend_meshlets(&crown_dag, &tree.crown.0, &VoxelFarSettings::default())
        .expect("extend");
    Forest {
        tree,
        texture,
        trunk_dag,
        crown_dag,
        far,
    }
}

/// Resident bytes of a cluster mesh in the visibility buffer's arenas, as the meshlet bridge counts
/// them: 64 B per vertex, 4 B per vertex and triangle list entry, one cluster record each.
fn resident_bytes(dag: &MeshletMesh, vertices: usize) -> u64 {
    vertices as u64 * 64
        + (dag.vertices.len() + dag.triangles.len()) as u64 * 4
        + dag.meshlets.len() as u64 * std::mem::size_of::<regia_render::visbuffer::Meshlet>() as u64
}

/// One counted row: the LOD cut over every tree whose bounding sphere is in the view frustum.
#[derive(Default, Debug, PartialEq)]
struct ForestCount {
    trees: u64,
    clusters: u64,
    triangles: u64,
    voxel_clusters: u64,
    voxel_triangles: u64,
}

/// The CPU LOD cut of the trunk and `crown` over `models` at `height` render pixels (16:9 or the
/// tier's aspect), counting voxel clusters (levels at or above `first_voxel_level`).
fn forest_count(
    f: &Forest,
    crown: &MeshletMesh,
    first_voxel_level: Option<u32>,
    models: &[[[f32; 4]; 4]],
    (width, height): (u32, u32),
) -> ForestCount {
    let tan_y = (FOREST_FOV * 0.5).tan();
    let tan_x = tan_y * width as f32 / height as f32;
    let ppu = height as f32 / (2.0 * tan_y);
    let forward = normalize3(std::array::from_fn(|k| FOREST_TARGET[k] - FOREST_EYE[k]));
    let right = normalize3(cross3(forward, [0.0, 1.0, 0.0]));
    let up = cross3(right, forward);
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let mut count = ForestCount::default();
    for m in models {
        let scale = (m[0][0] * m[0][0] + m[0][2] * m[0][2]).sqrt();
        let center = [m[3][0], m[3][1] + 4.0 * scale, m[3][2]];
        let radius = 4.6 * scale;
        let d: [f32; 3] = std::array::from_fn(|k| center[k] - FOREST_EYE[k]);
        let z = dot(d, forward);
        let inside = z > -radius
            && dot(d, right).abs() - tan_x * z <= radius * (1.0 + tan_x * tan_x).sqrt()
            && dot(d, up).abs() - tan_y * z <= radius * (1.0 + tan_y * tan_y).sqrt();
        if !inside {
            continue;
        }
        count.trees += 1;
        // The eye in the tree's space: rotate back and divide out the uniform scale (projected
        // error is scale-free: error and distance scale together).
        let rel: [f32; 3] = std::array::from_fn(|k| FOREST_EYE[k] - m[3][k]);
        let local = [
            (m[0][0] * rel[0] + m[0][1] * rel[1] + m[0][2] * rel[2]) / (scale * scale),
            (m[1][0] * rel[0] + m[1][1] * rel[1] + m[1][2] * rel[2]) / (scale * scale),
            (m[2][0] * rel[0] + m[2][1] * rel[1] + m[2][2] * rel[2]) / (scale * scale),
        ];
        let view = regia_render::visbuffer::LodView {
            eye: local,
            pixels_per_unit: ppu,
            threshold_px: FOREST_THRESHOLD_PX,
            near: 0.1,
        };
        for dag in [&f.trunk_dag, crown] {
            for i in dag.lod_cut(&view) {
                let meshlet = &dag.meshlets[i];
                count.clusters += 1;
                count.triangles += u64::from(meshlet.triangle_count);
                if first_voxel_level.is_some_and(|level| meshlet.level >= level) && std::ptr::eq(dag, crown) {
                    count.voxel_clusters += 1;
                    count.voxel_triangles += u64::from(meshlet.triangle_count);
                }
            }
        }
    }
    count
}

fn forest_instances() -> u32 {
    regia_core::brand::env_var("REGIA_FOREST_INSTANCES")
        .ok()
        .and_then(|text| text.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(100_000)
}

/// The counted cost table of the synthetic forest per quality tier (ADR-3108; the input of the
/// M15d stage 3 acceptance row, `docs/specs/26` § 6): the LOD cut of 100 000 trees (trunk +
/// 900 masked leaf cards) with the crown's alpha-tested voxel far levels against the plain cluster
/// DAG: trees in view, clusters and triangles of the cut, the voxel clusters among them, and the
/// resident cluster-arena bytes of both meshes. Counts only (no device); the frame's own counters
/// come from `forest_voxel_far_bench` on a card.
#[test]
fn forest_cost_table_counts() {
    let f = forest();
    let models = forest_models(forest_instances());
    let first_voxel = f.far.levels.first().map(|l| l.dag_level);
    let plain_bytes = resident_bytes(&f.trunk_dag, f.tree.trunk.0.len())
        + resident_bytes(&f.crown_dag, f.tree.crown.0.len());
    let voxel_bytes = resident_bytes(&f.trunk_dag, f.tree.trunk.0.len())
        + resident_bytes(&f.far.meshlets, f.far.vertices.len());
    println!(
        "forest: {} trees, trunk {} triangles / {} clusters, crown {} triangles / {} clusters, crown voxel levels {:?}",
        models.len(),
        f.tree.trunk.1.len() / 3,
        f.trunk_dag.meshlets.len(),
        f.tree.crown.1.len() / 3,
        f.crown_dag.meshlets.len(),
        f.far
            .levels
            .iter()
            .map(|l| (l.dag_level, l.triangles, l.shape))
            .collect::<Vec<_>>()
    );
    println!(
        "| Tier | Render px | Trees in view | DAG | Clusters | Triangles | Voxel clusters | Voxel triangles | Arena bytes |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    for (name, preset, width, height) in FOREST_TIERS {
        let render_h = tier_render_height(preset, height);
        let render_w = (u64::from(width) * u64::from(render_h) / u64::from(height)) as u32;
        let plain = forest_count(&f, &f.crown_dag, None, &models, (render_w, render_h));
        let voxel = forest_count(&f, &f.far.meshlets, first_voxel, &models, (render_w, render_h));
        for (dag, c, bytes) in [("plain", &plain, plain_bytes), ("voxel", &voxel, voxel_bytes)] {
            println!(
                "| {name} | {render_w}x{render_h} | {} | {dag} | {} | {} | {} | {} | {bytes} |",
                c.trees, c.clusters, c.triangles, c.voxel_clusters, c.voxel_triangles
            );
        }
        assert_eq!(plain.trees, voxel.trees, "{name}: the same trees in view");
        assert!(voxel.voxel_clusters > 0, "{name}: far trees draw voxel levels");
        assert!(
            voxel.triangles < plain.triangles,
            "{name}: the voxel levels cut triangles ({} vs {})",
            voxel.triangles,
            plain.triangles
        );
    }
    // The counts are a pure function of the inputs.
    let again = forest_count(
        &f,
        &f.far.meshlets,
        first_voxel,
        &models[..1000.min(models.len())],
        (1920, 1080),
    );
    let once = forest_count(
        &f,
        &f.far.meshlets,
        first_voxel,
        &models[..1000.min(models.len())],
        (1920, 1080),
    );
    assert_eq!(again, once);
}

/// The ground of the forest frames.
fn forest_scene<'a>(
    ground: &'a (Vec<LabVertex>, Vec<u32>),
    floor: &'a LabMaterial,
    post: &'a PostSettings,
    preset: RenderProfilePreset,
    (width, height): (u32, u32),
) -> FrameScene<'a> {
    let mut quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
    quality.visibility_buffer = true;
    quality.visibility_path = VisibilityPath {
        select: VisibilitySelect::Always,
        single_pass_resolve: true,
    };
    FrameScene {
        width,
        height,
        eye: FOREST_EYE,
        target: FOREST_TARGET,
        fov_y: FOREST_FOV,
        near: 0.1,
        far: 3000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: Some(post),
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new([0.3, -1.0, -0.4], [1.0; 3], 100_000.0),
        local_lights: &[],
        quality,
        meshes: vec![FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 1001,
            object_key: 1,
            vertices: &ground.0,
            indices: &ground.1,
            material: floor,
            model: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, -0.05, -1100.0, 1.0],
            ],
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }],
    }
}

/// The forest on a device, per tier, with and without the crown's voxel levels: wall and GPU frame
/// times, cull work items, clusters drawn (masked among them), and the cluster arena's bytes.
/// Ignored in the gate (wall clock); the card command is in `docs/benchmark-scenes.md`
/// ("Synthetic forest"). Alpha testing on the visibility path needs the masked hookup
/// (ADR-3009's frame patch); without it the masked lists draw untested, which moves the frame
/// time but none of the counts.
#[test]
#[ignore = "wall-clock bench: run by hand on the machine under test, --nocapture"]
fn forest_voxel_far_bench() {
    regia_render::texture_table::set_device_level_request(regia_render::texture_table::TEXTURE_TABLE_LEVEL);
    let device = RenderDevice::headless().expect("device");
    let info = device.adapter.get_info();
    println!("adapter: {} ({:?}, {:?})", info.name, info.backend, info.driver);
    let f = forest();
    let models = forest_models(forest_instances());
    let frames = regia_core::brand::env_var("REGIA_VIS_BENCH_FRAMES")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(8u32);
    let tiers: Vec<String> = regia_core::brand::env_var("REGIA_FOREST_TIERS")
        .map(|t| t.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();
    let ground = regia_render::box_mesh([2400.0, 0.05, 2400.0]);
    let floor = LabMaterial {
        base_color: [0.25, 0.22, 0.15, 1.0],
        ..LabMaterial::default()
    };
    let bark = LabMaterial {
        base_color: [0.35, 0.25, 0.18, 1.0],
        roughness: 0.9,
        ..LabMaterial::default()
    };
    let leaf = LabMaterial {
        base_color: [1.0; 4],
        albedo: Some(f.texture.clone()),
        alpha_mode: regia_render::ALPHA_MASK,
        alpha_cutoff: LEAF_CUTOFF,
        double_sided: true,
        roughness: 0.7,
        ..LabMaterial::default()
    };
    let post = PostSettings::golden_deterministic();
    println!("--- forest: {} trees, {frames} frames", models.len());
    println!(
        "| Tier | Output | Crown DAG | Frame ms | GPU ms | Work items | Clusters drawn | Masked clusters | Masked tested | \
         Arena bytes |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|");
    for (name, preset, width, height) in FOREST_TIERS {
        if !tiers.is_empty() && !tiers.iter().any(|t| t == name) {
            continue;
        }
        for voxel in [false, true] {
            let mut renderer = FrameRenderer::new();
            renderer.set_gpu_timing(&device, true);
            renderer.set_visbuffer_settings(VisBufferSettings {
                lod_threshold_px: FOREST_THRESHOLD_PX,
                ..VisBufferSettings::default()
            });
            renderer.insert_meshlets(TRUNK_KEY, Arc::new(f.trunk_dag.clone()));
            let (crown_vertices, crown_dag) = if voxel {
                (&f.far.vertices, f.far.meshlets.clone())
            } else {
                (&f.tree.crown.0, f.crown_dag.clone())
            };
            renderer.insert_meshlets(CROWN_KEY, Arc::new(crown_dag));
            let trunk = regia_render::visbuffer::VisInstanceSet {
                mesh_key: TRUNK_KEY,
                vertices: &f.tree.trunk.0,
                indices: &f.tree.trunk.1,
                material_key: 3001,
                material: &bark,
                models: &models,
                shadow: regia_render::ShadowCast::All,
                instance_shadows: &[],
            };
            let crown = regia_render::visbuffer::VisInstanceSet {
                mesh_key: CROWN_KEY,
                vertices: crown_vertices,
                indices: &f.tree.crown.1,
                material_key: 3002,
                material: &leaf,
                models: &models,
                shadow: regia_render::ShadowCast::All,
                instance_shadows: &[],
            };
            assert!(renderer.set_instance_set(1, &trunk), "trunk set");
            assert!(renderer.set_instance_set(2, &crown), "crown set");
            let scene = forest_scene(&ground, &floor, &post, preset, (width, height));
            for _ in 0..3 {
                renderer.render_offscreen(&device, &scene).expect("frame");
            }
            let (mut wall, mut gpu) = (0.0f64, 0.0f64);
            for _ in 0..frames {
                let start = std::time::Instant::now();
                renderer.render_offscreen(&device, &scene).expect("frame");
                wall += start.elapsed().as_secs_f64() * 1000.0;
                gpu += f64::from(renderer.gpu_timings().frame_ms);
            }
            let n = f64::from(frames.max(1));
            let counters = renderer
                .visbuffer_counters(&device)
                .ok()
                .flatten()
                .unwrap_or_default();
            let stats = renderer.visbuffer_stats();
            println!(
                "| {name} | {width}x{height} | {} | {:.2} | {:.3} | {} | {} | {} | {} | {} |",
                if voxel { "voxel" } else { "plain" },
                wall / n,
                gpu / n,
                counters.work_items,
                counters.phase1_clusters + counters.phase2_clusters + counters.software_clusters,
                counters.masked_clusters,
                stats.masked_tested,
                renderer.visbuffer_arena_stats().used_bytes,
            );
        }
    }
}
