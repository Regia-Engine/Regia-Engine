// SPDX-License-Identifier: MPL-2.0
//! Sculpting end to end: brush strokes on a `Terrain`, the live GPU update through the
//! deformation path (`TerrainEdits::take_gpu_dirty` → `TerrainGpu::update_heights`), and an
//! offscreen render of the sculpted hill.
//!
//! With `REGIA_WRITE_EVIDENCE=1` the before/after frames are written as PNGs (to
//! `REGIA_EVIDENCE_DIR`, default the test's target tmp dir) for review by eye.

use std::sync::OnceLock;

use glam::Vec2;
use regia_terrain::gpu::{TerrainCamera, TerrainFrame, TerrainGpu, TimeOfDay, render_offscreen};
use regia_terrain::{
    Brush, BrushMode, BrushShape, CdlodConfig, LayerDesc, MinMaxPyramid, Terrain, TerrainEdits,
    TerrainLayers, TerrainMap, apply_stroke, select_nodes,
};

const W: u32 = 480;
const H: u32 = 270;
const CLEAR: [f64; 4] = [0.02, 0.02, 0.05, 1.0];

fn device() -> &'static regia_render::RenderDevice {
    static DEVICE: OnceLock<regia_render::RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// A gently rolling meadow: 257² samples over 256 m, heights within a few metres.
fn meadow() -> TerrainMap {
    let n = 257u32;
    let width = 256.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
            let z = row as f32 / (n - 1) as f32 * width - width * 0.5;
            heights
                .push(4.0 + 1.2 * (x * 0.05).sin() * (z * 0.043).cos() + 0.4 * (x * 0.21 + z * 0.17).sin());
        }
    }
    TerrainMap::from_heights(n, n, heights, width, 80.0, -10.0).expect("meadow")
}

/// Grass on the flats, dirt on moderate slopes, rock on steep faces, pale stone up high.
fn layers() -> TerrainLayers {
    TerrainLayers {
        layers: vec![
            LayerDesc {
                name: "grass".into(),
                albedo: [0.14, 0.27, 0.08],
                roughness: 0.95,
                slope_min: 0.0,
                slope_max: 0.35,
                altitude_min: -20.0,
                altitude_max: 36.0,
                blend_sharpness: 0.2,
                ..LayerDesc::ground()
            },
            LayerDesc {
                name: "dirt".into(),
                albedo: [0.33, 0.24, 0.15],
                roughness: 0.9,
                slope_min: 0.25,
                slope_max: 0.75,
                altitude_min: -20.0,
                altitude_max: 70.0,
                blend_sharpness: 0.2,
                ..LayerDesc::ground()
            },
            LayerDesc {
                name: "rock".into(),
                albedo: [0.36, 0.35, 0.33],
                roughness: 0.8,
                slope_min: 0.6,
                slope_max: 1.6,
                altitude_min: -20.0,
                altitude_max: 70.0,
                blend_sharpness: 0.2,
                triplanar: true,
                ..LayerDesc::ground()
            },
            LayerDesc {
                name: "stone".into(),
                albedo: [0.55, 0.53, 0.5],
                roughness: 0.85,
                slope_min: 0.0,
                slope_max: 0.7,
                altitude_min: 32.0,
                altitude_max: 70.0,
                blend_sharpness: 0.2,
                ..LayerDesc::ground()
            },
        ],
        ..Default::default()
    }
}

fn config() -> CdlodConfig {
    CdlodConfig::with_grid_size(16)
}

fn frame() -> TerrainFrame {
    let camera = TerrainCamera {
        position: [-100.0, 50.0, 100.0],
        target: [0.0, 16.0, 0.0],
        fov_y_radians: 0.9,
        aspect: W as f32 / H as f32,
        near: 0.5,
        far: 3000.0,
    };
    let tod = TimeOfDay::default();
    TerrainFrame::from_camera(&camera, &tod, 0.0, 1.0)
}

fn new_gpu(map: &TerrainMap) -> TerrainGpu {
    TerrainGpu::new(device(), map, &layers(), &config(), 4, None, None).expect("terrain gpu")
}

fn draw(gpu: &mut TerrainGpu, map: &TerrainMap, pyramid: &MinMaxPyramid, frame: &TerrainFrame) -> Vec<u8> {
    let selection = select_nodes(
        map,
        pyramid,
        &config(),
        4,
        [frame.camera_pos[0], frame.camera_pos[2]],
        None,
    );
    render_offscreen(gpu, device(), frame, &selection, W, H, CLEAR).expect("offscreen render")
}

fn write_evidence(name: &str, pixels: &[u8]) {
    if regia_core::brand::env_var_os("REGIA_WRITE_EVIDENCE").is_none() {
        return;
    }
    let dir = regia_core::brand::env_var_os("REGIA_EVIDENCE_DIR").map_or_else(
        || std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("sculpt"),
        std::path::PathBuf::from,
    );
    let _ = std::fs::create_dir_all(&dir);
    let Ok(file) = std::fs::File::create(dir.join(name)) else {
        return;
    };
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(pixels);
    }
}

/// The hill: stacked soft Raise strokes of shrinking radius (each stroke lifts a sample at
/// most once), a ridge dragged off one shoulder, a smoothing pass, a flattened lookout on top
/// and a lowered pond at the foot.
fn sculpt(terrain: &mut Terrain, edits: &mut TerrainEdits, gpu: &mut TerrainGpu) -> usize {
    let raise = |radius: f32, amount: f32| Brush {
        shape: BrushShape::Circle { radius },
        mode: BrushMode::Raise { amount },
        hardness: 0.1,
        ..Brush::default()
    };
    let strokes: Vec<(Brush, Vec<Vec2>)> = vec![
        (raise(60.0, 14.0), vec![Vec2::ZERO]),
        (raise(42.0, 12.0), vec![Vec2::new(4.0, -3.0)]),
        (raise(26.0, 10.0), vec![Vec2::new(6.0, -6.0)]),
        (
            raise(14.0, 6.0),
            vec![
                Vec2::new(10.0, -8.0),
                Vec2::new(32.0, 10.0),
                Vec2::new(52.0, 30.0),
            ],
        ),
        (
            Brush {
                shape: BrushShape::Circle { radius: 30.0 },
                mode: BrushMode::Smooth { kernel_radius: 2 },
                alpha: 0.8,
                ..Brush::default()
            },
            vec![Vec2::new(-10.0, 0.0), Vec2::new(30.0, 10.0)],
        ),
        (
            // Flatten from the shoulder: the peak is cut down to a lookout plateau.
            Brush {
                shape: BrushShape::Circle { radius: 9.0 },
                mode: BrushMode::Flatten,
                hardness: 0.6,
                ..Brush::default()
            },
            vec![Vec2::new(13.0, -1.0), Vec2::new(4.0, -8.0)],
        ),
        (
            Brush {
                shape: BrushShape::Circle { radius: 16.0 },
                mode: BrushMode::Lower { amount: 4.0 },
                hardness: 0.4,
                ..Brush::default()
            },
            vec![Vec2::new(-70.0, 55.0)],
        ),
    ];
    let mut uploads = 0;
    for (brush, path) in strokes {
        let undo = apply_stroke(terrain, None, edits, &brush, &path).expect("stroke");
        assert!(!undo.is_empty(), "{:?} changed nothing", brush.mode);
        // The live update: only the dirty rectangle goes to the GPU.
        if let Some(dirty) = edits.take_gpu_dirty() {
            gpu.update_heights(&terrain.map, &dirty);
            uploads += 1;
        }
    }
    uploads
}

#[test]
fn sculpted_hill_renders_through_the_live_update_path() {
    let map = meadow();
    let mut terrain =
        Terrain::new(&map.to_heightfield(), 256.0, 80.0, -10.0, config(), 4, layers()).expect("terrain");
    let frame = frame();
    let mut gpu = new_gpu(&terrain.map);
    let before = draw(&mut gpu, &terrain.map, &terrain.minmax, &frame);
    write_evidence("sculpt_before.png", &before);

    let mut edits = TerrainEdits::default();
    let uploads = sculpt(&mut terrain, &mut edits, &mut gpu);
    assert_eq!(uploads, 7, "every stroke uploads its dirty rectangle");
    let peak = terrain.map.min_max_height().1;
    assert!(peak > 30.0, "the hill rises well above the meadow: {peak}");
    let live = draw(&mut gpu, &terrain.map, &terrain.minmax, &frame);
    write_evidence("sculpt_after.png", &live);

    // The incrementally updated GPU terrain draws exactly what a fresh upload of the sculpted
    // map draws: the dirty-rect path missed nothing (heights, pyramid levels, LOD bounds).
    let mut fresh = new_gpu(&terrain.map);
    let rebuilt = MinMaxPyramid::build(&terrain.map, 4).expect("pyramid");
    let reference = draw(&mut fresh, &terrain.map, &rebuilt, &frame);
    let mismatched = live
        .as_chunks::<4>()
        .0
        .iter()
        .zip(reference.as_chunks::<4>().0.iter())
        .filter(|(a, b)| a.iter().zip(b.iter()).any(|(x, y)| x.abs_diff(*y) > 2))
        .count();
    assert!(
        mismatched * 1000 < (W * H) as usize,
        "live-updated frame differs from a fresh upload in {mismatched} pixels"
    );

    // The hill changes the picture a lot: the meadow's horizon is now a hill.
    let changed = before
        .as_chunks::<4>()
        .0
        .iter()
        .zip(live.as_chunks::<4>().0.iter())
        .filter(|(a, b)| {
            a.iter()
                .zip(b.iter())
                .take(3)
                .map(|(x, y)| u32::from(x.abs_diff(*y)))
                .sum::<u32>()
                > 30
        })
        .count();
    assert!(changed > (W * H / 20) as usize, "only {changed} pixels changed");
}
