// SPDX-License-Identifier: MIT-0
//! Stage 3 acceptance plates on the floor tier, and the owner's plates for the dense-scene default
//! (ADR-3149, `specs/27` § 4.2 row 3: "scatter plates at 10× the rock count at the same frame
//! time"; `specs/98` § 8 item 3).
//!
//! The floor tier is DO27's 1080p60 on Desktop High (`docs/master-plan.md` § 5.1). Every frame here
//! draws with that row and the dense-scene default switched on, the way a game would ship it: an
//! outdoor scene (terrain, physical sky, height fog) with rocks scattered over it, once at the base
//! rock count and once at ten times as many over the same ground. At the base count the gate keeps
//! the classic path; at ten times it hands the frame to the visibility buffer.
//!
//! - `the_floor_tier_routes_ten_times_the_rocks_to_the_path` (lavapipe, in the gate): the routing
//!   above, on small frames. No golden image is read or written.
//! - `stage3_scatter_plates` (ignored, a card): GPU frame time of the base scene, of ten times the
//!   rocks on the classic path, and of ten times the rocks with the default; prints the stage 3
//!   verdict and writes the plates under `REGIA_STAGE3_OUT`.
//! - `dense_default_owner_plates` (ignored, any device): the ten-times scene through the classic path
//!   and through the default side by side, with their difference times eight, for the owner's look
//!   decision. Writes under `REGIA_STAGE3_OUT`.
//!
//! ```sh
//! REGIA_STAGE3_OUT=/tmp/stage3 cargo test --release -p lookdev --test visbuffer_default_plates \
//!     -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Knobs: `REGIA_STAGE3_SIZE` (default 1920x1080), `REGIA_STAGE3_ROCKS` (the base count, default
//! 410: ten times is 4100, over the default's 4096-mesh threshold), `REGIA_STAGE3_ROCK_SEGMENTS`
//! (rock detail, default 40: 1,600 triangles), `REGIA_STAGE3_FRAMES` (timed frames, default 32).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_asset_pipeline::{MeshletAttributes, MeshletSettings, build_meshlet_mesh_with_attributes};
use regia_render::visbuffer::MeshletBuilder;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    OutdoorParams, PostSettings, RenderDevice, RenderProfilePreset, RenderSettings, SharedPassDraw,
    VisibilityDefault, VisibilityDefaultState, VisibilityPath, VolumetricFogSettings,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerTextures, MaterialParams, Rgba8Image, TerrainLayers, TerrainMap,
    TerrainMaterial, TerrainRelief, TerrainSharedDraw,
};

/// Heightfield samples per side.
const N: usize = 129;
/// World size of the heightfield, meters.
const WORLD: f32 = 192.0;

/// Gentle hills: the ground's height at `(x, z)`.
fn height(x: f32, z: f32) -> f32 {
    1.6 * (x * 0.11).sin() * (z * 0.07).cos() + 0.8 * (x * 0.23 + z * 0.17).sin()
}

/// A hash of `i` in `[0, 1)` (integer mixing, so the scatter is the same on every machine).
fn hash(i: u32, salt: u32) -> f32 {
    let mut h = i.wrapping_mul(0x9E37_79B9) ^ salt.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7FEB_352D);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846C_A68B);
    h ^= h >> 16;
    (h >> 8) as f32 / (1u32 << 24) as f32
}

/// The cooker's cluster hierarchy, as a game's cooked rocks have it (the LOD cut is part of what
/// stage 3 measures).
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

fn terrain(device: &RenderDevice) -> TerrainSharedDraw {
    let heights: Vec<f32> = (0..N * N)
        .map(|i| {
            let x = ((i % N) as f32 / (N - 1) as f32 - 0.5) * WORLD;
            let z = ((i / N) as f32 / (N - 1) as f32 - 0.5) * WORLD;
            height(x, z)
        })
        .collect();
    let map = TerrainMap::from_heights(N as u32, N as u32, heights, WORLD, 40.0, -20.0).expect("heightfield");
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [1.0, 1.0, 1.0],
            roughness: 0.9,
            tiling: 1.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            blend_sharpness: 0.15,
            triplanar: false,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    let mut draw = TerrainSharedDraw::new(device, map, &stack, CdlodConfig::default(), 3).expect("terrain");
    let g = regia_terrain::linear_to_srgb8(0.22);
    draw.set_material(
        &TerrainMaterial {
            layers: vec![Some(LayerTextures {
                albedo: Rgba8Image::solid(64, 64, [g, regia_terrain::linear_to_srgb8(0.19), g, 255]),
                normal: Rgba8Image::neutral_normal(64),
                height: Rgba8Image::neutral_height(64),
                orm: Rgba8Image::solid(64, 64, [255, 230, 0, 255]),
                use_orm: false,
            })],
            macro_set: None,
            params: MaterialParams::default(),
        },
        &[],
    )
    .expect("terrain material");
    draw.set_relief(TerrainRelief::off());
    draw
}

/// A lumpy rock: a sphere pushed in and out along its normals by a seeded hash per vertex column.
fn rock(segments: u32) -> (Vec<LabVertex>, Vec<u32>) {
    let segments = segments.max(6);
    let (mut vertices, indices) = regia_render::uv_sphere(1.0, segments, (segments / 2).max(3));
    for (k, v) in vertices.iter_mut().enumerate() {
        // Seam vertices share a column so the surface stays closed.
        let column = (k as u32) % (segments + 1) % segments;
        let ring = (k as u32) / (segments + 1);
        let bump = 0.82 + 0.3 * hash(column * 131 + ring, 7);
        for axis in 0..3 {
            v.position[axis] *= bump;
        }
    }
    (vertices, indices)
}

/// The scatter: `count` rocks over the ground in front of the eye, the same patch of ground at
/// every count (ten times the rocks is ten times as dense).
fn scene(width: u32, height_px: u32, count: u32, segments: u32, post: &PostSettings) -> LabScene {
    let (vertices, indices) = rock(segments);
    let meshes = (0..count)
        .map(|i| {
            let x = (hash(i, 1) - 0.5) * 80.0;
            let z = -4.0 - hash(i, 2) * 70.0;
            let s = 0.3 + 0.7 * hash(i, 3) * hash(i, 4);
            let turn = hash(i, 5) * std::f32::consts::TAU;
            let (sin, cos) = turn.sin_cos();
            let mut model = [[0.0f32; 4]; 4];
            model[0] = [cos * s * 1.3, 0.0, -sin * s * 1.3, 0.0];
            model[1] = [0.0, s * 0.7, 0.0, 0.0];
            model[2] = [sin * s, 0.0, cos * s, 0.0];
            model[3] = [x, height(x, z) + s * 0.3, z, 1.0];
            LabMesh {
                vertices: vertices.clone(),
                indices: indices.clone(),
                material: (i % 3) as usize,
                model,
                ..LabMesh::unskinned()
            }
        })
        .collect();
    LabScene {
        width,
        height: height_px,
        eye: [0.0, height(0.0, 6.0) + 3.5, 6.0],
        target: [0.0, height(0.0, -30.0) + 0.5, -30.0],
        fov_y: 0.9,
        near: 0.1,
        far: 2000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: post.exposure_ev,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([-0.5, -0.45, -0.4], [1.0, 0.95, 0.88], 90_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.42, 0.38, 0.33, 1.0],
                roughness: 0.85,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.3, 0.32, 0.35, 1.0],
                roughness: 0.6,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.36, 0.33, 0.27, 1.0],
                roughness: 0.75,
                ..LabMaterial::default()
            },
        ],
        meshes,
    }
}

/// The floor tier's frame quality with the dense-scene default on (or `classic`: the same row with
/// the path off, the control).
fn floor_quality(classic: bool) -> FrameQuality {
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    if !classic {
        assert_eq!(
            settings.set_visibility_default(VisibilityDefault::Dense),
            VisibilityDefaultState::Applied
        );
    }
    settings.quality()
}

/// What one configuration drew.
struct Drawn {
    pixels: Vec<u8>,
    gpu_ms: Vec<f64>,
    on_path: bool,
    instances: u32,
}

/// Draws `lab` at `quality`: `settle` frames, then `timed` frames with GPU stamps.
fn draw(device: &RenderDevice, lab: &LabScene, quality: FrameQuality, settle: u32, timed: u32) -> Drawn {
    let post = lab
        .post
        .clone()
        .unwrap_or_else(PostSettings::golden_deterministic);
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    // One rock mesh shared by every instance, as a scatter shares it.
    if let Some(key) = frame.meshes.first().map(|m| m.mesh_key) {
        for mesh in &mut frame.meshes {
            mesh.mesh_key = key;
        }
    }
    let mut renderer = FrameRenderer::new();
    if device.adapter.get_info().device_type == wgpu::DeviceType::Cpu {
        // llvmpipe's anisotropic taps need the quad derivatives a full-screen resolve cannot give it
        // (`visbuffer_differential.rs`).
        renderer.set_material_anisotropy_for_tests(1);
    }
    renderer.set_meshlet_builder(Some(cooked_builder()));
    renderer.set_gpu_timing(device, timed > 0);
    renderer.set_outdoor(OutdoorParams {
        fog: VolumetricFogSettings {
            density: 0.006,
            height_falloff: 0.08,
            base_height: 0.0,
            ..VolumetricFogSettings::default()
        },
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(
        Box::new(terrain(device)) as Box<dyn SharedPassDraw + Send + Sync>
    ));
    let mut pixels = Vec::new();
    for _ in 0..settle.max(1) {
        pixels = renderer
            .render_offscreen(device, &frame)
            .expect("settling frame")
            .pixels;
    }
    let mut gpu_ms = Vec::new();
    for _ in 0..timed {
        pixels = renderer
            .render_offscreen(device, &frame)
            .expect("timed frame")
            .pixels;
        gpu_ms.push(f64::from(renderer.gpu_timings().frame_ms));
    }
    Drawn {
        pixels,
        gpu_ms,
        on_path: renderer.visbuffer_active(),
        instances: renderer.visbuffer_stats().instances,
    }
}

fn env_u32(name: &str, default: u32) -> u32 {
    regia_core::brand::env_var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn size() -> (u32, u32) {
    regia_core::brand::env_var("REGIA_STAGE3_SIZE")
        .ok()
        .and_then(|v| {
            let (w, h) = v.trim().split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((1920, 1080))
}

fn out_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(regia_core::brand::env_var("REGIA_STAGE3_OUT").ok()?);
    match std::fs::create_dir_all(&dir) {
        Ok(()) => Some(dir),
        Err(err) => {
            eprintln!("{}: {err}", dir.display());
            None
        }
    }
}

/// Encodes RGBA8 into `tmp` and renames it to `path`.
fn try_write_png(tmp: &Path, path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(tmp).map_err(|e| e.to_string())?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    std::fs::rename(tmp, path).map_err(|e| e.to_string())
}

/// Writes RGBA8 as a PNG through a temporary file and a rename.
fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) {
    match try_write_png(&path.with_extension("png.tmp"), path, width, height, rgba) {
        Ok(()) => eprintln!("wrote {}", path.display()),
        Err(err) => eprintln!("{}: {err}", path.display()),
    }
}

/// `|a - b| × 8` per channel, opaque.
fn diff_x8(a: &[u8], b: &[u8]) -> Vec<u8> {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .flat_map(|(pa, pb)| {
            let c = |i: usize| (u16::from(pa[i].abs_diff(pb[i])) * 8).min(255) as u8;
            [c(0), c(1), c(2), 255]
        })
        .collect()
}

/// Images of one size side by side.
fn side_by_side(images: &[&[u8]], width: u32, height: u32) -> Vec<u8> {
    let row = width as usize * 4;
    let mut out = Vec::with_capacity(row * images.len() * height as usize);
    for y in 0..height as usize {
        for image in images {
            out.extend_from_slice(&image[y * row..(y + 1) * row]);
        }
    }
    out
}

fn median(values: &[f64]) -> f64 {
    let mut v: Vec<f64> = values
        .iter()
        .copied()
        .filter(|x| x.is_finite() && *x > 0.0)
        .collect();
    v.sort_by(f64::total_cmp);
    match v.len() {
        0 => f64::NAN,
        n if !n.is_multiple_of(2) => v[n / 2],
        n => (v[n / 2 - 1] + v[n / 2]) / 2.0,
    }
}

/// A frame worth looking at: sky, ground and rocks, not one flat color or a clipped one.
fn frame_problem(pixels: &[u8]) -> Option<String> {
    let texels = pixels.as_chunks::<4>().0;
    let clipped = texels.iter().filter(|p| p[..3].iter().any(|c| *c >= 250)).count();
    if clipped * 5 >= texels.len() {
        return Some(format!("{clipped} of {} pixels clipped", texels.len()));
    }
    let range = (0..3)
        .map(|c| {
            let (lo, hi) = texels
                .iter()
                .fold((255u8, 0u8), |(lo, hi), p| (lo.min(p[c]), hi.max(p[c])));
            hi - lo
        })
        .max()
        .unwrap_or(0);
    (range < 32).then(|| format!("the frame is nearly flat: range {range}"))
}

fn post() -> PostSettings {
    PostSettings {
        exposure_ev: 14.5,
        ..PostSettings::golden_deterministic()
    }
}

#[test]
fn the_floor_tier_routes_ten_times_the_rocks_to_the_path() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip: {err}");
            return;
        }
    };
    let quality = floor_quality(false);
    assert_eq!(quality.visibility_path, VisibilityPath::DENSE_DEFAULT);
    let regia_render::VisibilitySelect::Dense(select) = quality.visibility_path.select else {
        panic!("the default selects by density");
    };
    // Small frames and low-detail rocks keep lavapipe quick; the counts are the plates'.
    let base = 410u32;
    assert!(base < select.enter_instances / 2 && base * 10 >= select.enter_instances);
    let post = post();
    let one = draw(&device, &scene(160, 90, base, 8, &post), quality, 6, 0);
    assert!(!one.on_path, "the base scatter stays on the classic path");
    let (_, rock_indices) = rock(8);
    assert!(
        u64::from(base * 10) * ((rock_indices.len() / 3) as u64) < select.enter_triangles,
        "the mesh count, not the triangles, is what enters here"
    );
    let ten = draw(
        &device,
        &scene(160, 90, base * 10, 8, &post),
        quality,
        select.enter_frames + 2,
        0,
    );
    assert!(ten.on_path, "ten times the rocks takes the path");
    assert_eq!(ten.instances, base * 10, "every rock is a visibility instance");
    if let Some(problem) = frame_problem(&ten.pixels) {
        panic!("the ten-times frame cannot be judged: {problem}");
    }
}

#[test]
#[ignore = "GPU timing and plates: run on the card under test, --nocapture"]
fn stage3_scatter_plates() {
    let device = RenderDevice::headless().expect("device");
    let info = device.adapter.get_info();
    let (w, h) = size();
    let base = env_u32("REGIA_STAGE3_ROCKS", 410);
    let segments = env_u32("REGIA_STAGE3_ROCK_SEGMENTS", 40);
    let timed = env_u32("REGIA_STAGE3_FRAMES", 32).max(1);
    let (_, rock_indices) = rock(segments);
    println!(
        "adapter: {} ({:?}, {:?}); {w}x{h} Desktop High (floor tier) with the dense default; \
         {base} rocks x {} triangles, x10 = {}",
        info.name,
        info.backend,
        info.driver,
        rock_indices.len() / 3,
        base * 10
    );
    let post = post();
    let base_scene = scene(w, h, base, segments, &post);
    let ten_scene = scene(w, h, base * 10, segments, &post);
    let default = floor_quality(false);
    let classic = floor_quality(true);
    let runs = [
        ("base_default", &base_scene, default),
        ("x10_classic", &ten_scene, classic),
        ("x10_default", &ten_scene, default),
    ];
    let mut medians = Vec::new();
    let mut images = Vec::new();
    for (name, lab, quality) in runs {
        let drawn = draw(&device, lab, quality, 12, timed);
        let ms = median(&drawn.gpu_ms);
        println!(
            "{name:>13}: gpu median {ms:.3} ms over {timed} frames, on path {}, instances {}",
            drawn.on_path, drawn.instances
        );
        if let Some(problem) = frame_problem(&drawn.pixels) {
            println!("{name}: {problem}");
        }
        medians.push(ms);
        images.push(drawn.pixels);
    }
    // Stage 3: ten times the rocks with the default holds the base scene's frame time (within 5 %,
    // the spread between two runs of one binary on one card is about 3 %).
    let holds = medians[2] <= medians[0] * 1.05;
    println!(
        "stage3 verdict: x10 with the default {:.3} ms vs base {:.3} ms ({:+.1} %), x10 classic {:.3} ms; holds {holds}",
        medians[2],
        medians[0],
        (medians[2] / medians[0] - 1.0) * 100.0,
        medians[1],
    );
    if let Some(dir) = out_dir() {
        for ((name, ..), pixels) in runs.iter().zip(&images) {
            write_png(&dir.join(format!("stage3_{name}.png")), w, h, pixels);
        }
        let sheet = side_by_side(&[&images[0], &images[2]], w, h);
        write_png(
            &dir.join("stage3_sheet_base_vs_x10_default.png"),
            w * 2,
            h,
            &sheet,
        );
        let row = format!(
            "{}\t{w}x{h}\t{base}\t{}\t{:.3}\t{:.3}\t{:.3}\t{holds}\n",
            info.name,
            rock_indices.len() / 3,
            medians[0],
            medians[1],
            medians[2]
        );
        if let Err(err) = std::fs::write(dir.join("stage3_row.tsv"), row) {
            eprintln!("stage3_row.tsv: {err}");
        }
    }
}

#[test]
#[ignore = "owner plates: writes images under REGIA_STAGE3_OUT, --nocapture"]
fn dense_default_owner_plates() {
    let device = RenderDevice::headless().expect("device");
    let Some(dir) = out_dir() else {
        panic!("set REGIA_STAGE3_OUT to the plate directory");
    };
    let (w, h) = size();
    let base = env_u32("REGIA_STAGE3_ROCKS", 410);
    let segments = env_u32("REGIA_STAGE3_ROCK_SEGMENTS", 40);
    let post = post();
    let ten_scene = scene(w, h, base * 10, segments, &post);
    let classic = draw(&device, &ten_scene, floor_quality(true), 12, 0);
    let default = draw(&device, &ten_scene, floor_quality(false), 12, 0);
    assert!(default.on_path, "the plate shows the default's path");
    let diff = diff_x8(&classic.pixels, &default.pixels);
    let close = classic
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .zip(default.pixels.as_chunks::<4>().0)
        .filter(|(a, b)| a.iter().zip(b.iter()).all(|(x, y)| x.abs_diff(*y) <= 2))
        .count() as f64
        / f64::from(w * h);
    println!(
        "owner plate {w}x{h}, {} rocks: {:.3} % of pixels within 2 levels",
        base * 10,
        close * 100.0
    );
    write_png(&dir.join("owner_x10_classic.png"), w, h, &classic.pixels);
    write_png(&dir.join("owner_x10_dense_default.png"), w, h, &default.pixels);
    write_png(&dir.join("owner_x10_diff_x8.png"), w, h, &diff);
    let sheet = side_by_side(&[&classic.pixels, &default.pixels, &diff], w, h);
    write_png(
        &dir.join("owner_sheet_classic_default_diff.png"),
        w * 3,
        h,
        &sheet,
    );
}
