// SPDX-License-Identifier: MPL-2.0
//! A node cut away by several hole shapes together (inside their union, inside none of them
//! alone) draws nothing, like a node one shape covers, and its empty cut is not rebuilt every
//! frame. DO27's tunnel mouths are a chain of 42 overlapping ellipses: before, such nodes drew
//! the uncut grid over the trench and rebuilt their hole indices every frame in every cascade
//! selection (most of Switchbacks' sun-shadow CPU cost).

use std::sync::OnceLock;

use regia_terrain::gpu::{TerrainCamera, TerrainFrame, TerrainGpu, TimeOfDay, render_offscreen};
use regia_terrain::{
    CdlodConfig, HoleShape, HolesMask, MinMaxPyramid, TerrainLayers, TerrainMap, select_nodes,
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

/// Flat ground 10 m up, 257² samples over 256 m: the finest nodes are 16 m squares on a
/// grid through the origin.
fn map() -> TerrainMap {
    let n = 257u32;
    TerrainMap::from_heights(n, n, vec![10.0; (n * n) as usize], 256.0, 32.0, 0.0).expect("flat map")
}

fn rect(x0: f32, x1: f32, z0: f32, z1: f32) -> HoleShape {
    HoleShape::Rect {
        center: [(x0 + x1) * 0.5, (z0 + z1) * 0.5],
        half_extents: [(x1 - x0) * 0.5, (z1 - z0) * 0.5],
        rotation_radians: 0.0,
    }
}

/// Two rectangles meeting over the 16 m node `[0, 16]²`: together they cover it with room
/// to spare, but neither covers it alone.
fn split_cut() -> HolesMask {
    HolesMask::from_shapes(vec![rect(-20.0, 9.0, -20.0, 36.0), rect(7.0, 36.0, -20.0, 36.0)]).expect("mask")
}

/// The same footprint as one rectangle.
fn whole_cut() -> HolesMask {
    HolesMask::from_shapes(vec![rect(-20.0, 36.0, -20.0, 36.0)]).expect("mask")
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

/// Renders the view twice (two frames on one terrain) and returns the second image and the
/// terrain.
fn render(mask: Option<&HolesMask>) -> (Vec<u8>, TerrainGpu) {
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
    let pyramid = MinMaxPyramid::build(&map, ROOT).expect("pyramid");
    let selection = select_nodes(&map, &pyramid, &config, ROOT, [8.0, 8.0], None);
    let frame = frame();
    render_offscreen(&mut gpu, device(), &frame, &selection, W, H, CLEAR).expect("first frame");
    let pixels = render_offscreen(&mut gpu, device(), &frame, &selection, W, H, CLEAR).expect("second frame");
    (pixels, gpu)
}

/// Mean absolute channel difference over the centre box (the view of the node `[0, 16]²`).
fn centre_difference(a: &[u8], b: &[u8]) -> f64 {
    let (mut sum, mut n) = (0.0, 0u32);
    for y in H / 2 - 12..H / 2 + 12 {
        for x in W / 2 - 12..W / 2 + 12 {
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
fn a_node_inside_two_shapes_together_is_cut_like_one_shape_and_kept() {
    let (split, gpu) = render(Some(&split_cut()));
    assert!(
        gpu.hole_cut_node_count() >= 1,
        "the node [0, 16]² lies inside the union only: it must be recorded as cut"
    );
    assert_eq!(
        gpu.last_hole_index_builds(),
        0,
        "the second frame of the same selection rebuilt hole indices"
    );
    let (whole, _) = render(Some(&whole_cut()));
    let (ground, _) = render(None);
    let same = centre_difference(&split, &whole);
    let lid = centre_difference(&split, &ground);
    assert!(
        same < 2.0,
        "the split cut must open the ground like the whole cut (mean |Δ| {same:.2})"
    );
    assert!(
        lid > 10.0,
        "the cut must differ from the uncut ground (mean |Δ| {lid:.2}): the view does not see the hole"
    );
}
