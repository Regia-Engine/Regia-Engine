// SPDX-License-Identifier: MPL-2.0
//! Lane W-15 (ADR-8470): the terrain draw cuts a filled mouth on its lattice, not on its coverage
//! shape. The named hole's coverage shape is a 56 m square; its lattice cut is the same square
//! minus an uncut island over the view centre. With the lattice set, the island is drawn ground
//! (as on the uncut map) and the rest of the square stays open; without it, the island is cut.
//! Lavapipe, no goldens, no timing.

// Test fixtures panic on a broken fixture.
#![allow(clippy::expect_used)]

use std::sync::OnceLock;

use regia_terrain::gpu::{TerrainCamera, TerrainFrame, TerrainGpu, TimeOfDay, render_offscreen};
use regia_terrain::{
    CdlodConfig, HoleShape, HolesMask, LatticeHoleCut, MinMaxPyramid, NamedHole, TerrainLayers, TerrainMap,
    select_nodes,
};

const W: u32 = 160;
const H: u32 = 120;
const CLEAR: [f64; 4] = [0.02, 0.02, 0.05, 1.0];
const ROOT: u32 = 4;

fn device() -> &'static regia_render::RenderDevice {
    static DEVICE: OnceLock<regia_render::RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// Flat ground 10 m up, 257² samples over 256 m (1 m pitch, sample 0 at -128 m).
fn map() -> TerrainMap {
    let n = 257u32;
    TerrainMap::from_heights(n, n, vec![10.0; (n * n) as usize], 256.0, 32.0, 0.0).expect("flat map")
}

/// The mouth: coverage shape the square `[-20, 36]²`.
fn mask() -> HolesMask {
    let mut mask = HolesMask::default();
    mask.set_named(NamedHole {
        id: "mouth".into(),
        open: true,
        shape: HoleShape::Rect {
            center: [8.0, 8.0],
            half_extents: [28.0, 28.0],
            rotation_radians: 0.0,
        },
    })
    .expect("named hole");
    mask
}

/// The mouth's lattice cut: every cell of the square but the island `[-2, 18] x [1, 15]`.
fn lattice(map: &TerrainMap) -> LatticeHoleCut {
    let origin = map.sample_to_world(0, 0);
    let mut cut = LatticeHoleCut::new(origin, 1.0, [256, 256]);
    // World x = -128 + i.
    let cell = |w: f32| (w + 128.0) as u32;
    let (lo, hi) = ([cell(-20.0), cell(-20.0)], [cell(36.0), cell(36.0)]);
    let island =
        |i: u32, j: u32| (cell(-2.0)..cell(18.0)).contains(&i) && (cell(1.0)..cell(15.0)).contains(&j);
    cut.set_hole("mouth", lo, hi, |i, j, _| !island(i, j));
    cut
}

fn frame() -> TerrainFrame {
    let camera = TerrainCamera {
        position: [8.0, 60.0, 30.0],
        target: [8.0, 10.0, 8.0],
        fov_y_radians: 0.5,
        aspect: W as f32 / H as f32,
        near: 0.5,
        far: 2000.0,
    };
    TerrainFrame::from_camera(&camera, &TimeOfDay::default(), 0.1, 1.7)
}

/// The second frame of the view on one terrain.
fn render(mask: Option<&HolesMask>, lattice: Option<LatticeHoleCut>) -> Vec<u8> {
    let map = map();
    let config = CdlodConfig::with_grid_size(16);
    let mut gpu = TerrainGpu::new(
        device(),
        &map,
        &TerrainLayers::default(),
        &config,
        ROOT,
        mask,
        None,
    )
    .expect("terrain gpu");
    gpu.set_hole_lattice(lattice);
    let pyramid = MinMaxPyramid::build(&map, ROOT).expect("pyramid");
    let selection = select_nodes(&map, &pyramid, &config, ROOT, [8.0, 8.0], None);
    let frame = frame();
    render_offscreen(&mut gpu, device(), &frame, &selection, W, H, CLEAR).expect("first frame");
    render_offscreen(&mut gpu, device(), &frame, &selection, W, H, CLEAR).expect("second frame")
}

/// Mean absolute channel difference over a box of half size `r` around pixel `(cx, cy)`.
fn box_difference(a: &[u8], b: &[u8], cx: u32, cy: u32, r: u32) -> f64 {
    let (mut sum, mut n) = (0.0, 0u32);
    for y in cy - r..cy + r {
        for x in cx - r..cx + r {
            let i = ((y * W + x) * 4) as usize;
            for c in 0..3 {
                sum += f64::from((i32::from(a[i + c]) - i32::from(b[i + c])).unsigned_abs());
            }
            n += 3;
        }
    }
    sum / f64::from(n)
}

#[test]
fn a_filled_mouth_draws_its_lattice_cut() {
    let ground = render(None, None);
    let coverage = render(Some(&mask()), None);
    let lattice_cut = render(Some(&mask()), Some(lattice(&map())));
    // The island under the view centre: cut by the coverage shape, ground with the lattice.
    let island_cov = box_difference(&coverage, &ground, W / 2, H / 2, 8);
    let island_lat = box_difference(&lattice_cut, &ground, W / 2, H / 2, 8);
    // A box near the top edge of the view (far side of the square, past the island): open in both.
    let open_cov = box_difference(&coverage, &ground, W / 2, 8, 6);
    let open_lat = box_difference(&lattice_cut, &coverage, W / 2, 8, 6);
    println!(
        "island vs ground: coverage {island_cov:.2}, lattice {island_lat:.2}; far box: coverage vs ground {open_cov:.2}, lattice vs coverage {open_lat:.2}"
    );
    assert!(
        island_cov > 10.0,
        "the coverage shape cuts the island ({island_cov:.2})"
    );
    assert!(
        island_lat < 2.0,
        "the lattice draws the uncut island as ground ({island_lat:.2})"
    );
    assert!(open_cov > 10.0, "the far box is inside the hole ({open_cov:.2})");
    assert!(
        open_lat < 2.0,
        "the lattice keeps the rest of the hole open ({open_lat:.2})"
    );
}
