// SPDX-License-Identifier: MPL-2.0
//! Macro tile residency on a 16 × 16 grid (ADR-3367).
//!
//! The residency mask used to travel as one `f32` uniform lane read as a `u32`: 16 bits, a
//! 4 × 4 grid, and bakes past it were refused. It is now 256 bits (eight words and their
//! running counts), so a 16 × 16 bake keeps residency per tile: a tile that is not streamed in
//! shades from the coarse whole-map fallback while each of its neighbours reads its own tile.
//!
//! The bake here is uniform: every tile is one grey, the coarse fallback a darker one, so in the
//! blended-albedo view (3) a resident tile tints the ground by `tile / coarse mean` and a
//! fallback tile by 1. One tile in the middle of the grid, (5, 9), is left out of the resident
//! set (passed out of order, with a duplicate, as a streamer may): its centre must come out
//! darker than itself with every tile resident and than its four neighbours, which read the
//! same as with every tile resident (their layers did not shift). Lavapipe, no goldens, no
//! timing.

use std::sync::OnceLock;

use regia_terrain::gpu::{TerrainCamera, TerrainFrame, TerrainGpu, TimeOfDay, render_offscreen};
use regia_terrain::{
    CdlodConfig, LayerDesc, MacroBake, MaterialParams, MinMaxPyramid, Rgba8Image, TerrainLayers, TerrainMap,
    TerrainMaterial, select_nodes,
};

const W: u32 = 256;
const H: u32 = 256;
const TILES: u32 = 16;
const WORLD: f32 = 128.0;
/// The tile left out of the resident set.
const MISSING: (u32, u32) = (5, 9);

fn device() -> &'static regia_render::RenderDevice {
    static DEVICE: OnceLock<regia_render::RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

fn flat_map() -> TerrainMap {
    TerrainMap::from_heights(129, 129, vec![5.0; 129 * 129], WORLD, 8.0, 0.0).expect("flat map")
}

fn layers() -> TerrainLayers {
    TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [0.5, 0.5, 0.5],
            slope_min: 0.0,
            slope_max: 2.0,
            altitude_min: -500.0,
            altitude_max: 500.0,
            ..LayerDesc::ground()
        }],
        ..TerrainLayers::default()
    }
}

/// A 16 × 16 bake of one grey per tile over a darker coarse fallback (neutral normals).
fn uniform_bake() -> MacroBake {
    let tile = 8;
    let count = (TILES * TILES) as usize;
    MacroBake {
        world_size: WORLD,
        tiles: TILES,
        tile_size: tile,
        albedo: vec![Rgba8Image::solid(tile, tile, [170, 170, 170, 255]); count],
        normal: vec![Rgba8Image::solid(tile, tile, [128, 128, 128, 255]); count],
        coarse_albedo: Rgba8Image::solid(tile, tile, [110, 110, 110, 255]),
        coarse_normal: Rgba8Image::solid(tile, tile, [128, 128, 128, 255]),
    }
}

fn material() -> TerrainMaterial {
    let mut material = TerrainMaterial::procedural_cc0(
        32,
        7,
        MaterialParams {
            anti_tiling_cell: 0.0,
            ..Default::default()
        },
    );
    material.layers.truncate(1);
    material.macro_set = Some(uniform_bake());
    material.params.macro_world_size = WORLD;
    material.params.macro_strength = 1.0;
    material.params.macro_normal_strength = 0.0;
    material
}

fn camera() -> TerrainCamera {
    TerrainCamera {
        position: [0.0, 5.0 + 150.0, 3.0],
        target: [0.0, 5.0, 0.0],
        fov_y_radians: 1.0,
        aspect: W as f32 / H as f32,
        near: 1.0,
        far: 1000.0,
    }
}

fn frame() -> TerrainFrame {
    let mut frame = TerrainFrame::from_camera(&camera(), &TimeOfDay::default(), 0.0, 1.0);
    frame.debug_view = 3;
    frame
}

/// Renders the blended-albedo view with the macro tiles `resident` streamed in.
fn render(resident: &[u32]) -> Vec<u8> {
    let map = flat_map();
    let layers = layers();
    let config = CdlodConfig::with_grid_size(16);
    let mut gpu = TerrainGpu::new(device(), &map, &layers, &config, 4, None, None).expect("terrain gpu");
    gpu.set_material(&material(), resident)
        .expect("a 16 x 16 macro bake");
    let pyramid = MinMaxPyramid::build(&map, 4).expect("pyramid");
    let cam = camera();
    let selection = select_nodes(
        &map,
        &pyramid,
        &config,
        4,
        [cam.position[0], cam.position[2]],
        None,
    );
    render_offscreen(
        &mut gpu,
        device(),
        &frame(),
        &selection,
        W,
        H,
        [0.0, 0.0, 0.0, 1.0],
    )
    .expect("render")
}

/// Mean luma of a 5 × 5 patch around the centre of macro tile `(tx, ty)` on screen.
fn tile_luma(pixels: &[u8], tx: u32, ty: u32) -> f64 {
    let tile_m = WORLD / TILES as f32;
    let x = -WORLD * 0.5 + (tx as f32 + 0.5) * tile_m;
    let z = -WORLD * 0.5 + (ty as f32 + 0.5) * tile_m;
    let m = frame().view_proj;
    let p = [x, 5.0, z, 1.0];
    let clip: Vec<f32> = (0..4).map(|r| (0..4).map(|c| m[c][r] * p[c]).sum()).collect();
    let (nx, ny) = (clip[0] / clip[3], clip[1] / clip[3]);
    let px = ((nx * 0.5 + 0.5) * W as f32) as i64;
    let py = ((0.5 - ny * 0.5) * H as f32) as i64;
    assert!(
        (2..W as i64 - 2).contains(&px) && (2..H as i64 - 2).contains(&py),
        "tile ({tx}, {ty}) is off screen at ({px}, {py})"
    );
    let mut sum = 0.0;
    for dy in -2..=2 {
        for dx in -2..=2 {
            let i = (((py + dy) as u32 * W + (px + dx) as u32) * 4) as usize;
            sum += 0.2126 * f64::from(pixels[i])
                + 0.7152 * f64::from(pixels[i + 1])
                + 0.0722 * f64::from(pixels[i + 2]);
        }
    }
    sum / 25.0
}

#[test]
fn a_sixteen_grid_keeps_residency_per_tile() {
    let missing = MISSING.0 + MISSING.1 * TILES;
    // Every tile but one, out of order, with a duplicate and an index past the grid.
    let mut resident: Vec<u32> = (0..TILES * TILES).rev().filter(|&i| i != missing).collect();
    resident.extend([17, 4096]);
    let all = render(&[]);
    let streamed = render(&resident);

    let (mx, my) = MISSING;
    let neighbours = [(mx - 1, my), (mx + 1, my), (mx, my - 1), (mx, my + 1)];
    let hole = tile_luma(&streamed, mx, my);
    let hole_all = tile_luma(&all, mx, my);
    let lumas: Vec<f64> = neighbours
        .iter()
        .map(|&(x, y)| tile_luma(&streamed, x, y))
        .collect();
    let lumas_all: Vec<f64> = neighbours.iter().map(|&(x, y)| tile_luma(&all, x, y)).collect();
    // The missing tile falls back to the coarse map: darker than itself resident, and than
    // every neighbour. (Tiles differ a few percent among themselves: the layer texture varies.)
    assert!(
        hole < 0.85 * hole_all,
        "the missing tile must fall back: {hole:.1} streamed vs {hole_all:.1} resident"
    );
    for (&(x, y), &l) in neighbours.iter().zip(&lumas) {
        assert!(
            l > hole * 1.15,
            "neighbour ({x}, {y}) luma {l:.1} must stay on its own (brighter) tile, missing tile {hole:.1}"
        );
    }
    // The neighbours read the same tile data in both frames (their layers did not shift).
    for ((&(x, y), &a), &b) in neighbours.iter().zip(&lumas).zip(&lumas_all) {
        assert!(
            (a - b).abs() < 0.04 * b,
            "neighbour ({x}, {y}): {a:.1} streamed vs {b:.1} all"
        );
    }
    // Far corners of the grid (bits in the first and last mask words) stay resident too.
    for (x, y) in [(1, 1), (14, 14), (14, 1), (1, 14)] {
        let (a, b) = (tile_luma(&streamed, x, y), tile_luma(&all, x, y));
        assert!(
            (a - b).abs() < 0.04 * b,
            "tile ({x}, {y}): {a:.1} streamed vs {b:.1} all"
        );
    }
}
