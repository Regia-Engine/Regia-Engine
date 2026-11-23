// SPDX-License-Identifier: MPL-2.0
//! The frame's extra directional lights on the terrain's shared draw and its grass (ADR-7211,
//! ADR-7254), lavapipe: a [`TerrainSharedDraw`] with a [`GrassLayer`] under a whole
//! `FrameRenderer` frame, no ambient (`ambient_lux` 0, no IBL), no shadows, the exposure fixed.
//!
//! Each check diffs two renderers that drew the same number of frames: a control that only ever
//! had the sun, and one that also had an extra directional light from its first frame.
//!
//! - Ground: an extra light with the sun's direction, colour and illuminance doubles the ground's
//!   light (its Lambert term and lobe are the sun's, unshadowed; the sun is unshadowed here too).
//! - Grass: the blades take the extra light as well (a clear increase on the grass pixels).
//! - An extra light from below the horizon adds nothing to the ground.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene, LightsViews, PostSettings, RenderDevice,
    SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, GrassLayer, GrassMask, GrassMaskInputs, GrassQuality, GrassRequest, GrassSet,
    TerrainGrassHandle, TerrainLayers, TerrainMap, TerrainSharedDraw,
};

const W: u32 = 96;
const H: u32 = 64;
/// Frames each renderer draws before the compared one.
const FRAMES: usize = 3;
/// The sun's travel direction: high, so the ground is well lit.
const SUN: [f32; 3] = [0.3, -1.0, 0.2];
const SUN_LUX: f32 = 20_000.0;

fn layers() -> TerrainLayers {
    TerrainLayers::from_ron(
        r#"TerrainLayers(version: 1, wetness_default: 0.0, layers: [
            LayerDesc(name: "ground", albedo: (0.30, 0.26, 0.20), roughness: 0.92, tiling: 2.0,
                slope_min: 0.0, slope_max: 1.6, altitude_min: -1000.0, altitude_max: 5000.0,
                blend_sharpness: 0.15, triplanar: false),
        ])"#,
    )
    .unwrap_or_else(|err| panic!("layers: {err}"))
}

/// Gentle ground (half a metre of relief over 256 m): no slope shadows itself.
fn map() -> TerrainMap {
    let n = 129u32;
    let world = 256.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let z = (row as f32 / (n - 1) as f32 - 0.5) * world;
            let x = (col as f32 / (n - 1) as f32 - 0.5) * world;
            heights.push(0.5 * (x * 0.021).sin() * (z * 0.017).cos());
        }
    }
    TerrainMap::from_heights(n, n, heights, world, 1.0, 0.0).unwrap_or_else(|err| panic!("map: {err}"))
}

struct Rig {
    device: RenderDevice,
    renderer: FrameRenderer,
    handle: TerrainGrassHandle,
    map: TerrainMap,
    post: PostSettings,
}

impl Rig {
    /// A terrain rig whose frames carry `extra` as their extra directional lights.
    fn new(extra: &[DirectionalLightUniform], grass: bool) -> Self {
        let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
        let map = map();
        let layers = layers();
        let mut draw = TerrainSharedDraw::new(&device, map.clone(), &layers, CdlodConfig::default(), 4)
            .unwrap_or_else(|err| panic!("terrain: {err}"));
        let set = GrassSet::summer();
        let mask = GrassMask::build(
            &set,
            &GrassMaskInputs {
                layers: Some(&layers),
                texel_m: 1.0,
                ..GrassMaskInputs::new(&map)
            },
        )
        .unwrap_or_else(|err| panic!("mask: {err}"));
        let layer = GrassLayer::new(&device.device, &device.queue, set, mask)
            .unwrap_or_else(|err| panic!("grass layer: {err}"));
        let handle = layer.handle();
        draw.set_grass(Some(layer));
        let mut renderer = FrameRenderer::new();
        renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
        renderer.set_lights_views(&LightsViews {
            extra_directional: extra.to_vec(),
            ..LightsViews::default()
        });
        let post = PostSettings {
            auto_exposure: false,
            exposure_ev: 13.0,
            bloom: false,
            taa: false,
            ..PostSettings::default()
        };
        let rig = Self {
            device,
            renderer,
            handle,
            map,
            post,
        };
        rig.grass(grass);
        rig
    }

    /// Grass on the High tier, still air, no grass shadows.
    fn grass(&self, enabled: bool) {
        self.handle.set_request(GrassRequest {
            enabled,
            quality: Some(GrassQuality::High),
            sway_scale: 0.0,
            shadows: false,
            ..GrassRequest::default()
        });
    }

    /// Looking steeply down at the ground, so every pixel is ground (or grass on it).
    fn scene(&self) -> LabScene {
        let (x, z) = (12.0, 40.0);
        let ground = self.map.sample_bilinear(x, z);
        let target = self.map.sample_bilinear(x, z - 1.5);
        LabScene {
            width: W,
            height: H,
            eye: [x, ground + 2.5, z],
            target: [x, target, z - 1.5],
            fov_y: 0.7,
            near: 0.05,
            far: 1000.0,
            depth_prepass: true,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 13.0,
            ambient_lux: 0.0,
            double_sided: false,
            post: Some(self.post.clone()),
            directional: DirectionalLightUniform::new(SUN, [1.0, 1.0, 1.0], SUN_LUX),
            local_lights: Vec::new(),
            materials: Vec::new(),
            meshes: Vec::new(),
        }
    }

    /// The linear HDR image of the next frame, pre-exposure removed.
    fn hdr(&mut self) -> Vec<[f32; 3]> {
        let lab = self.scene();
        let mut scene = lab.as_frame_scene_with_quality(FrameQuality::LAB);
        scene.post = Some(&self.post);
        self.renderer.request_hdr_capture();
        self.renderer
            .render_offscreen(&self.device, &scene)
            .unwrap_or_else(|err| panic!("frame: {err}"));
        self.renderer
            .take_hdr_capture(&self.device)
            .unwrap_or_else(|err| panic!("hdr readback: {err}"))
            .unwrap_or_else(|| panic!("the post stack ran, so the HDR copy exists"))
            .rgb
    }

    /// The HDR image of frame [`FRAMES`] (the earlier ones are drawn and dropped).
    fn settled(&mut self) -> Vec<[f32; 3]> {
        for _ in 0..FRAMES {
            let _ = self.hdr();
        }
        self.hdr()
    }
}

/// Serialises device creation and teardown across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn mean_luma(img: &[[f32; 3]], mask: &[bool]) -> f32 {
    let (sum, n) = img
        .iter()
        .zip(mask)
        .filter(|(_, m)| **m)
        .fold((0.0f64, 0usize), |(s, n), (c, _)| {
            (s + f64::from(luma(*c)), n + 1)
        });
    assert!(n > 0, "the mask selects no pixel");
    (sum / n as f64) as f32
}

fn the_sun_again() -> DirectionalLightUniform {
    DirectionalLightUniform::new(SUN, [1.0, 1.0, 1.0], SUN_LUX)
}

#[test]
fn a_second_directional_light_adds_its_lambert_term_on_the_ground() {
    let _gpu = serial();
    let control = Rig::new(&[], false).settled();
    let doubled = Rig::new(&[the_sun_again()], false).settled();
    let all = vec![true; control.len()];
    let c = mean_luma(&control, &all);
    let d = mean_luma(&doubled, &all);
    eprintln!(
        "ground: sun only {c:.4}, sun + the same light again {d:.4}, ratio {:.4}",
        d / c
    );
    assert!(c > 1.0e-3, "the sun lights the ground ({c})");
    assert!(
        (d / c - 2.0).abs() < 0.12,
        "a second light equal to the sun doubles the ground's light: {c} -> {d} ({:.3}x)",
        d / c
    );
}

#[test]
fn an_extra_light_from_below_the_horizon_adds_nothing() {
    let _gpu = serial();
    let control = Rig::new(&[], false).settled();
    let below = Rig::new(
        &[DirectionalLightUniform::new([0.0, 1.0, 0.0], [1.0; 3], SUN_LUX)],
        false,
    )
    .settled();
    let all = vec![true; control.len()];
    let c = mean_luma(&control, &all);
    let b = mean_luma(&below, &all);
    eprintln!("ground: sun only {c:.5}, + a light shining upward {b:.5}");
    assert!((b - c).abs() <= c * 1.0e-3, "{c} vs {b}");
}

#[test]
fn a_second_directional_light_lights_the_grass() {
    let _gpu = serial();
    let mut control_rig = Rig::new(&[], true);
    let control = control_rig.settled();
    // Where grass draws: the same control renderer with the grass off, a few frames later.
    control_rig.grass(false);
    let bare = control_rig.settled();
    let mask: Vec<bool> = control
        .iter()
        .zip(&bare)
        .map(|(g, b)| (luma(*g) - luma(*b)).abs() > 0.05 * luma(*b).max(1.0e-4))
        .collect();
    let blades = mask.iter().filter(|m| **m).count();
    eprintln!("grass pixels: {blades} of {}", mask.len());
    assert!(
        blades > mask.len() / 20,
        "grass covers part of the frame ({blades})"
    );
    let lit = Rig::new(&[the_sun_again()], true).settled();
    let c = mean_luma(&control, &mask);
    let l = mean_luma(&lit, &mask);
    eprintln!(
        "grass: sun only {c:.4}, sun + the same light again {l:.4}, ratio {:.4}",
        l / c
    );
    assert!(c > 1.0e-3, "the sun lights the grass ({c})");
    assert!(
        l / c > 1.4 && l / c < 4.0,
        "the blades take the extra light: {c} -> {l} ({:.3}x)",
        l / c
    );
}
