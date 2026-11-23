// SPDX-License-Identifier: MIT-0
//! Layer texture detail at 1 m (the "layer texture smear" of the first showcase frames).
//!
//! A flat scene with one photo-style layer at the orchestrator's scale (2048 texels over 2 m, about 1 mm a
//! texel), seen from 1 m above the ground at 1280x720 (about 1.3 mm a pixel):
//!
//! 1. a checker of 4-texel squares reaches the frame with its contrast (no downscale on load, no blur from the
//!    mip choice, no procedural overlay hiding it);
//! 2. a flat grey layer stays flat under the scene default ([`ReliefSpec::photo`]): nothing is drawn over the
//!    texture;
//! 3. the same grey layer under the engine's planetary defaults ([`ReliefSpec::engine`]) is not flat (its
//!    near-field grain and pebbles), so check 2 can tell the two apart.
//!
//! A second test paints two layers with splat packs (`terrain/macro/pack0.png`, ADR-1402) and checks each half of
//! the map shows its own layer.
//!
//! Lavapipe (or whatever adapter the machine has); without any GPU adapter the test prints a note and passes,
//! like the other render tests. No golden image: each check is a statistic of the frame.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use common::{JobSpec, run};
use regia_render::RenderDevice;
use regia_test_utils::unique_temp_dir;
use showcase::scene::{LayerMaterial, MANIFEST_NAME, Manifest, MaterialSet, ReliefSpec, SplatSpec};

/// Layer texture side, texels.
const TEXTURE: u32 = 2048;
/// Layer repeats per metre (2 m per repeat, as the showcase's grass layers).
const TILING: f32 = 0.5;
/// Checker square, texels.
const SQUARE: u32 = 4;
/// Ground height of the flat heightfield, metres.
const GROUND: f32 = 5.0;

fn write_png(path: &Path, width: u32, height: u32, color: png::ColorType, depth: png::BitDepth, data: &[u8]) {
    let file = std::fs::File::create(path).expect("png file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(color);
    encoder.set_depth(depth);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(data).expect("png data");
}

/// Splat packs of a test scene: their side and pack 0's RGBA at a texel (pack 1 is empty).
type TestSplat<'a> = Option<(u32, &'a dyn Fn(u32, u32) -> [u8; 4])>;

/// One layer of a test scene: its texture side (texels) and its grey value at a texel.
struct TestLayer<'a> {
    side: u32,
    texel: &'a dyn Fn(u32, u32) -> u8,
}

/// A flat scene in `dir` with `layers` (every band open), `relief`, and optionally splat packs (`side`, then
/// the RGBA of pack 0 at a texel; pack 1 is empty).
fn flat_scene(dir: &Path, layers: &[TestLayer<'_>], relief: ReliefSpec, splat: TestSplat<'_>) {
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene");
    std::fs::create_dir_all(dir).expect("scene dir");
    std::fs::copy(example.join("sky.ron"), dir.join("sky.ron")).expect("sky");

    // 65x65 samples at mid grey: GROUND metres everywhere.
    let side = 65u32;
    let hf: Vec<u8> = (0..side * side).flat_map(|_| 32768u16.to_be_bytes()).collect();
    write_png(
        &dir.join("heightfield.png"),
        side,
        side,
        png::ColorType::Grayscale,
        png::BitDepth::Sixteen,
        &hf,
    );

    let mut descs = Vec::new();
    let mut materials = Vec::new();
    for (index, layer) in layers.iter().enumerate() {
        let n = layer.side;
        let albedo: Vec<u8> = (0..n * n)
            .flat_map(|i| {
                let v = (layer.texel)(i % n, i / n);
                [v, v, v]
            })
            .collect();
        let name = format!("layer{index}.png");
        write_png(
            &dir.join(&name),
            n,
            n,
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            &albedo,
        );
        descs.push(format!(
            "(name: \"l{index}\", albedo: (1.0, 1.0, 1.0), roughness: 0.9, tiling: {TILING}, slope_min: 0.0, \
             slope_max: 1.6, altitude_min: -1000.0, altitude_max: 1000.0, blend_sharpness: 0.15, triplanar: false, \
             albedo_texture: None, normal_texture: None, height_texture: None, orm_texture: None, \
             detail_normal_strength: 0.0, height_blend: 0.0)"
        ));
        materials.push(LayerMaterial::Files {
            albedo: name,
            normal: None,
            height: None,
            orm: None,
        });
    }
    let text = format!(
        "(version: 3, wetness_default: 0.0, layers: [{}])",
        descs.join(", ")
    );
    std::fs::write(dir.join("terrain.layers.ron"), text).expect("layers");

    let splat = splat.map(|(n, pack0)| {
        let data0: Vec<u8> = (0..n * n).flat_map(|i| pack0(i % n, i / n)).collect();
        write_png(
            &dir.join("pack0.png"),
            n,
            n,
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            &data0,
        );
        let data1 = vec![0u8; (n * n * 4) as usize];
        write_png(
            &dir.join("pack1.png"),
            n,
            n,
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            &data1,
        );
        SplatSpec {
            pack0: "pack0.png".into(),
            pack1: "pack1.png".into(),
        }
    });

    let text = std::fs::read_to_string(example.join(MANIFEST_NAME)).expect("example manifest");
    let mut manifest: Manifest = ron::from_str(&text).expect("example manifest parses");
    manifest.name = "texture_detail".into();
    manifest.heightfield = "heightfield.png".into();
    manifest.world_width = 128.0;
    manifest.height_scale = GROUND * 65535.0 / 32768.0;
    manifest.height_offset = 0.0;
    manifest.layers = "terrain.layers.ron".into();
    manifest.material = Some(MaterialSet {
        layers: materials,
        macro_world_size: 0.0,
        anti_tiling_cell: 0.0,
        relief,
    });
    manifest.splat = splat;
    manifest.scatter = None;
    manifest.stand_ins = Vec::new();
    manifest.track = None;
    manifest.spawns = None;
    manifest.vfx_dir = None;
    manifest.lead_in_frames = 0;
    let text = ron::ser::to_string_pretty(&manifest, regia_core::ron_pretty()).expect("manifest");
    std::fs::write(dir.join(MANIFEST_NAME), text).expect("manifest");
}

/// `(p90 − p10) / mean` of the luma of the central 128x128 pixels of an RGBA8 frame.
fn spread(frame: &(u32, u32, Vec<u8>)) -> f32 {
    let (w, h, pixels) = frame;
    let (cx, cy, half) = (w / 2, h / 2, 64u32);
    let mut luma: Vec<f32> = Vec::new();
    for y in cy - half..cy + half {
        for x in cx - half..cx + half {
            let i = ((y * w + x) * 4) as usize;
            let p = &pixels[i..i + 3];
            luma.push(0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]));
        }
    }
    luma.sort_by(f32::total_cmp);
    let at = |q: f32| luma[((luma.len() - 1) as f32 * q) as usize];
    let mean = luma.iter().sum::<f32>() / luma.len() as f32;
    (at(0.9) - at(0.1)) / mean.max(1.0)
}

/// Renders one frame of a scene from 1 m above `(x, z)`, looking almost straight down, and returns it.
fn render_at(name: &str, dir: &Path, x: f32, z: f32, size: (u32, u32)) -> (u32, u32, Vec<u8>) {
    let outcome = run(
        7,
        &JobSpec {
            width: size.0,
            height: size.1,
            frames: 1,
            warmup_frames: 2,
            scene: Some(dir.to_path_buf()),
            camera: Some(([x, GROUND + 1.0, z], [x, GROUND, z + 0.05])),
            ..JobSpec::default()
        },
    );
    eprintln!("texture detail {name}: rendered");
    outcome.first_frame
}

fn render(name: &str, texel: &dyn Fn(u32, u32) -> u8, relief: ReliefSpec) -> f32 {
    let dir = unique_temp_dir(&format!("showcase-detail-{name}"));
    flat_scene(&dir, &[TestLayer { side: TEXTURE, texel }], relief, None);
    let frame = render_at(name, &dir, 0.3, 0.2, (1280, 720));
    let _ = std::fs::remove_dir_all(&dir);
    let value = spread(&frame);
    eprintln!("texture detail {name}: spread {value:.3}");
    value
}

/// Mean luma of the central 128x128 pixels of an RGBA8 frame.
fn mean_luma(frame: &(u32, u32, Vec<u8>)) -> f32 {
    let (w, h, pixels) = frame;
    let (cx, cy, half) = (w / 2, h / 2, 64u32);
    let mut sum = 0.0;
    for y in cy - half..cy + half {
        for x in cx - half..cx + half {
            let i = ((y * w + x) * 4) as usize;
            let p = &pixels[i..i + 3];
            sum += 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]);
        }
    }
    sum / (4 * half * half) as f32
}

#[test]
fn a_millimetre_checker_survives_at_one_metre_and_nothing_is_drawn_over_it() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase texture-detail test: {err}");
        return;
    }
    let checker = |x: u32, y: u32| -> u8 {
        if (x / SQUARE + y / SQUARE).is_multiple_of(2) {
            20
        } else {
            230
        }
    };
    let grey = |_: u32, _: u32| -> u8 { 128 };

    let checker_spread = render("checker", &checker, ReliefSpec::photo());
    let grey_photo = render("grey_photo", &grey, ReliefSpec::photo());
    let grey_engine = render("grey_engine", &grey, ReliefSpec::engine());

    assert!(
        checker_spread > 0.6,
        "the 4 mm checker reaches the frame at 1 m with its contrast: spread {checker_spread:.3}"
    );
    assert!(
        grey_photo < 0.08,
        "the photo preset draws nothing over a flat layer: spread {grey_photo:.3}"
    );
    assert!(
        grey_engine > grey_photo * 2.0 && grey_engine > 0.08,
        "the engine preset's procedural detail shows on the same layer (photo {grey_photo:.3}, engine {grey_engine:.3})"
    );
}

#[test]
fn splat_packs_paint_each_half_of_the_map_with_its_own_layer() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase splat test: {err}");
        return;
    }
    let dark = |_: u32, _: u32| 20u8;
    let bright = |_: u32, _: u32| 230u8;
    // 64 texels over the 128 m map: pack 0's red (layer 0, dark) on the −X half, green (layer 1, bright) on +X.
    let halves = |x: u32, _: u32| -> [u8; 4] { if x < 32 { [255, 0, 0, 0] } else { [0, 255, 0, 0] } };
    let layers = [
        TestLayer {
            side: 64,
            texel: &dark,
        },
        TestLayer {
            side: 64,
            texel: &bright,
        },
    ];
    let dir = unique_temp_dir("showcase-splat");
    flat_scene(&dir, &layers, ReliefSpec::photo(), Some((64, &halves)));
    let west = mean_luma(&render_at("splat_west", &dir, -30.0, 0.0, (640, 360)));
    let east = mean_luma(&render_at("splat_east", &dir, 30.0, 0.0, (640, 360)));
    let _ = std::fs::remove_dir_all(&dir);

    // The same two layers without packs: both bands are open, so both halves blend the two equally.
    let dir = unique_temp_dir("showcase-splat-off");
    flat_scene(&dir, &layers, ReliefSpec::photo(), None);
    let west_off = mean_luma(&render_at("nosplat_west", &dir, -30.0, 0.0, (640, 360)));
    let east_off = mean_luma(&render_at("nosplat_east", &dir, 30.0, 0.0, (640, 360)));
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("splat: west {west:.1} east {east:.1}; without packs west {west_off:.1} east {east_off:.1}");

    assert!(
        east > west * 2.0,
        "the bright layer covers the +X half and the dark one the -X half (west {west:.1}, east {east:.1})"
    );
    assert!(
        (west_off - east_off).abs() < 0.1 * west_off.max(east_off),
        "without packs both halves blend alike (west {west_off:.1}, east {east_off:.1})"
    );
    assert!(
        west < west_off && east > east_off,
        "the packs move each half away from the even blend"
    );
}
