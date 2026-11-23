// SPDX-License-Identifier: MPL-2.0
//! The drawn terrain follows the map's grid origin (ADR-8179): a map moved by `O` on X and Z,
//! seen from a camera moved by the same `O`, prints the frame the unmoved map printed from the
//! unmoved camera. The control keeps the camera where it was: the ground then moved under it.
//! A grid origin of `[0, 0]` set explicitly prints the default frame byte for byte.
//!
//! Lavapipe (or any adapter `RenderDevice::headless` finds); no golden image.

use std::sync::OnceLock;

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{CdlodConfig, LayerDesc, TerrainLayers, TerrainMap, TerrainRelief, TerrainSharedDraw};

const W: u32 = 192;
const H: u32 = 108;
const SAMPLES: u32 = 257;
const WORLD: f32 = 2048.0;
/// The grid origin under test: 12.5 samples of 8 m (a whole number of samples plus the half
/// sample a cell-spaced simulation grid needs), exact in `f32`.
const ORIGIN: f32 = -100.0;
/// Where the camera looks, in the map's own frame.
const SITE: [f32; 2] = [300.0, 200.0];

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// Rolling ground in the map's own frame, different everywhere.
fn height(x: f32, z: f32) -> f32 {
    18.0 * (x / 60.0).sin() * (z / 47.0).cos() + 6.0 * (x / 13.0 + z / 17.0).sin()
}

/// The map, its heights by sample index, centred on `origin` (`None`: the default constructor).
fn ground(origin: Option<f32>) -> TerrainSharedDraw {
    let spacing = WORLD / (SAMPLES - 1) as f32;
    let half = WORLD * 0.5;
    let heights: Vec<f32> = (0..SAMPLES)
        .flat_map(|j| (0..SAMPLES).map(move |i| (i, j)))
        .map(|(i, j)| height(i as f32 * spacing - half, j as f32 * spacing - half))
        .collect();
    let mut map = TerrainMap::from_heights(SAMPLES, SAMPLES, heights, WORLD, 120.0, -60.0).expect("map");
    if let Some(o) = origin {
        map = map.with_grid_origin([o, o]).expect("grid origin");
    }
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [0.4, 0.4, 0.4],
            roughness: 1.0,
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
    let mut draw = TerrainSharedDraw::new(device(), map, &stack, CdlodConfig::default(), 4).expect("terrain");
    draw.set_relief(TerrainRelief::off());
    draw
}

/// Exposure of the frames (a 100 000 lux sun).
const LIT_EV: f32 = 14.5;

fn renderer(origin: Option<f32>) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_shared_draw(Some(
        Box::new(ground(origin)) as Box<dyn SharedPassDraw + Send + Sync>
    ));
    renderer
}

/// One frame (exposure `ev`) with the camera and the box moved by `shift` on X and Z.
fn frame(renderer: &mut FrameRenderer, shift: f32, ev: f32) -> Vec<u8> {
    let at = |dx: f32, up: f32, dz: f32| {
        let (x, z) = (SITE[0] + dx, SITE[1] + dz);
        [x + shift, height(x, z) + up, z + shift]
    };
    let post = PostSettings {
        exposure_ev: ev,
        ..PostSettings::golden_deterministic()
    };
    let (verts, indices) = regia_render::box_mesh([1.0, 1.0, 1.0]);
    let scene = LabScene {
        width: W,
        height: H,
        eye: at(0.0, 14.0, -24.0),
        target: at(0.0, 0.0, 0.0),
        fov_y: 0.9,
        near: 0.1,
        far: 2000.0,
        depth_prepass: true,
        // Cascade texels snap to a world grid: off, so the moved frame differs only where the
        // terrain does.
        directional_shadows: false,
        ibl: true,
        exposure_ev: ev,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([-0.5, -0.6, 0.3], [1.0, 1.0, 1.0], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![regia_render::LabMaterial {
            base_color: [0.5, 0.2, 0.2, 1.0],
            metallic: 0.0,
            roughness: 1.0,
            ..regia_render::LabMaterial::default()
        }],
        meshes: vec![regia_render::LabMesh {
            vertices: verts,
            indices,
            material: 0,
            model: regia_core::glam::Mat4::from_translation(at(3.0, 1.0, 0.0).into()).to_cols_array_2d(),
            ..regia_render::LabMesh::unskinned()
        }],
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.gi_probes = false;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    renderer.render_offscreen(device(), &frame).expect("frame").pixels
}

/// Several frames, so temporal passes settle the same way in every run.
fn settled(renderer: &mut FrameRenderer, shift: f32, ev: f32) -> Vec<u8> {
    let mut last = Vec::new();
    for _ in 0..4 {
        last = frame(renderer, shift, ev);
    }
    last
}

/// Mean absolute difference per channel (8-bit levels) and the share of pixels off by more than 8.
fn difference(a: &[u8], b: &[u8]) -> (f64, f64) {
    assert_eq!(a.len(), b.len());
    let mut sum = 0u64;
    let mut off = 0u64;
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let worst = (0..3).map(|c| pa[c].abs_diff(pb[c])).max().unwrap_or(0);
        sum += (0..3).map(|c| u64::from(pa[c].abs_diff(pb[c]))).sum::<u64>();
        off += u64::from(worst > 8);
    }
    let pixels = (a.len() / 4) as f64;
    (sum as f64 / (pixels * 3.0), off as f64 / pixels)
}

fn mean_level(a: &[u8]) -> f64 {
    let sum: u64 = a
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| (0..3).map(|c| u64::from(p[c])).sum::<u64>())
        .sum();
    sum as f64 / ((a.len() / 4) as f64 * 3.0)
}

/// The moved map seen from the moved camera is the unmoved frame; the control (moved map,
/// unmoved camera) is not. Held: the share of pixels off by more than 8 levels (the terrain's
/// silhouettes and relief, the box standing on it). Not held: the frame's mean. The layer tiling
/// and the procedural ground detail stay anchored to world XZ (ADR-8179: the grid origin does not
/// move them), which leaves an even shading change (1.87 levels mean with no pixel past 8 levels
/// on lavapipe, against 6.9 levels and 43 % of the pixels for the control).
#[test]
fn the_drawn_terrain_follows_the_grid_origin() {
    let unmoved = settled(&mut renderer(None), 0.0, LIT_EV);
    assert!(mean_level(&unmoved) > 5.0, "the frame must show something");
    let moved = settled(&mut renderer(Some(ORIGIN)), ORIGIN, LIT_EV);
    let control = settled(&mut renderer(Some(ORIGIN)), 0.0, LIT_EV);
    let (mean, share) = difference(&unmoved, &moved);
    let (wrong_mean, wrong_share) = difference(&unmoved, &control);
    eprintln!(
        "grid origin {ORIGIN} m: moved mean {mean:.3} levels, {:.2} % of pixels off; control mean {wrong_mean:.3}, {:.2} %",
        share * 100.0,
        wrong_share * 100.0
    );
    assert!(
        wrong_mean > 4.0 && wrong_share > 0.2,
        "the control must see the ground moved: {wrong_mean}, {wrong_share}"
    );
    assert!(
        share < 0.02,
        "{share} of the pixels moved by more than 8 levels (mean {mean})"
    );
}

/// `with_grid_origin([0, 0])` is the default map: the same bytes.
#[test]
fn a_zero_grid_origin_prints_the_default_frame_byte_for_byte() {
    let default = settled(&mut renderer(None), 0.0, LIT_EV);
    let zero = settled(&mut renderer(Some(0.0)), 0.0, LIT_EV);
    assert!(mean_level(&default) > 5.0, "the frame must show something");
    assert!(
        default == zero,
        "a zero grid origin changed the frame: {:?}",
        difference(&default, &zero)
    );
}
