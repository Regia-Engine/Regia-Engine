// SPDX-License-Identifier: MPL-2.0
//! The foliage subsurface profile on the terrain's grass (ADR-3170), lavapipe: a [`GrassLayer`]
//! bound to the shared terrain draw under a whole `FrameRenderer` frame, the sun low in front of
//! the camera so the blades are back-lit.
//!
//! - Debug view 4 writes the profile's transmission through each blade toward the sun, raw: every
//!   grass pixel lies on the CPU's [`slab_transmission`] curve for a path between the blade's
//!   thickness and ten times it.
//! - With the render profile's switch (`TerrainBudget::foliage_subsurface`) the back-lit frame
//!   changes and the report says so; off renders the same bytes before and after it was on.

use regia_render::subsurface::slab_transmission;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene, PostSettings, RenderDevice,
    SharedPassDraw, SubsurfaceProfile,
};
use regia_terrain::{
    CdlodConfig, GRASS_BLADE_THICKNESS_MM, GrassLayer, GrassMask, GrassMaskInputs, GrassQuality,
    GrassRequest, GrassSet, TerrainGrassHandle, TerrainLayers, TerrainMap, TerrainSharedDraw,
};

const W: u32 = 160;
const H: u32 = 96;

fn layers() -> TerrainLayers {
    TerrainLayers::from_ron(
        r#"TerrainLayers(version: 1, wetness_default: 0.0, layers: [
            LayerDesc(name: "ground", albedo: (0.20, 0.16, 0.11), roughness: 0.92, tiling: 2.0,
                slope_min: 0.0, slope_max: 1.6, altitude_min: -1000.0, altitude_max: 5000.0,
                blend_sharpness: 0.15, triplanar: false),
        ])"#,
    )
    .unwrap_or_else(|err| panic!("layers: {err}"))
}

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
    fn new() -> Self {
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
        let post = PostSettings {
            auto_exposure: false,
            exposure_ev: 14.0,
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
        rig.request(true, 0);
        rig
    }

    /// Grass on the High tier, still air, no grass shadows (so only the blades themselves change).
    fn request(&self, enabled: bool, debug_view: u32) {
        self.handle.set_request(GrassRequest {
            enabled,
            quality: Some(GrassQuality::High),
            sway_scale: 0.0,
            shadows: false,
            debug_view,
            ..GrassRequest::default()
        });
    }

    fn scene(&self) -> LabScene {
        let (x, z) = (12.0, 40.0);
        let ground = self.map.sample_bilinear(x, z);
        let target = self.map.sample_bilinear(x, z - 20.0);
        LabScene {
            width: W,
            height: H,
            eye: [x, ground + 0.6, z],
            target: [x, target + 0.1, z - 20.0],
            fov_y: 0.9,
            near: 0.05,
            far: 1000.0,
            depth_prepass: true,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 14.0,
            ambient_lux: 2_000.0,
            double_sided: false,
            post: Some(self.post.clone()),
            // A low sun ahead of the camera, shining back toward it: back-lit blades.
            directional: DirectionalLightUniform::new([0.0, -0.3, 0.95], [1.0, 0.95, 0.85], 60_000.0),
            local_lights: Vec::new(),
            materials: Vec::new(),
            meshes: Vec::new(),
        }
    }

    fn quality(foliage: bool) -> FrameQuality {
        let mut quality = FrameQuality::LAB;
        quality.terrain_budget.foliage_subsurface = foliage;
        quality
    }

    /// The frame's display bytes.
    fn frame(&mut self, foliage: bool) -> Vec<u8> {
        let lab = self.scene();
        let mut scene = lab.as_frame_scene_with_quality(Self::quality(foliage));
        scene.post = Some(&self.post);
        self.renderer
            .render_offscreen(&self.device, &scene)
            .unwrap_or_else(|err| panic!("frame: {err}"))
            .pixels
    }

    /// The frame's linear HDR image, pre-exposure removed.
    fn hdr(&mut self, foliage: bool) -> Vec<[f32; 3]> {
        let lab = self.scene();
        let mut scene = lab.as_frame_scene_with_quality(Self::quality(foliage));
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
}

/// Serialises device creation and teardown across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The CPU curve's point nearest `value`'s green: the path (mm) whose green transmission it is.
fn path_of_green(green: f32) -> f32 {
    let reach = SubsurfaceProfile::Foliage.reach_mm();
    let (mut lo, mut hi) = (0.0f32, 10.0 * GRASS_BLADE_THICKNESS_MM);
    for _ in 0..60 {
        let mid = 0.5 * (lo + hi);
        if slab_transmission(SubsurfaceProfile::Foliage, mid, reach)[1] > green {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

#[test]
fn every_grass_pixel_reads_the_profiles_transmission() {
    let _gpu = serial();
    let mut rig = Rig::new();
    rig.request(false, 4);
    let without = rig.hdr(true);
    rig.request(true, 4);
    let with = rig.hdr(true);
    let reach = SubsurfaceProfile::Foliage.reach_mm();
    let (mut blades, mut worst) = (0usize, 0.0f32);
    let (mut shortest, mut longest) = (f32::MAX, 0.0f32);
    for (a, b) in with.iter().zip(&without) {
        if a == b {
            continue;
        }
        blades += 1;
        let d = path_of_green(a[1]);
        assert!(
            (GRASS_BLADE_THICKNESS_MM * 0.99..=GRASS_BLADE_THICKNESS_MM * 10.01).contains(&d),
            "path {d} mm out of range: {a:?}"
        );
        let cpu = slab_transmission(SubsurfaceProfile::Foliage, d, reach);
        for ch in 0..3 {
            worst = worst.max((cpu[ch] - a[ch]).abs());
        }
        shortest = shortest.min(d);
        longest = longest.max(d);
    }
    eprintln!(
        "{blades} blade pixels; transmission against the CPU curve: worst {worst:.5}; paths {shortest:.3}..{longest:.3} mm"
    );
    assert!(
        blades > (W * H / 20) as usize,
        "the view must hold grass: {blades} pixels"
    );
    // Half-float storage of a value under 1 is about 5e-4; the path is read back from green, which
    // carries that error into red and blue by the ratio of their slopes. Measured on lavapipe: 0.00183.
    assert!(worst < 4.0e-3, "worst channel off the curve by {worst}");
    assert!(
        longest > 1.5 * shortest,
        "the path must vary with the blade's angle to the sun"
    );
}

#[test]
fn the_switch_moves_back_lit_grass_and_off_keeps_its_bytes() {
    let _gpu = serial();
    let mut rig = Rig::new();
    // Two warm-up frames: the field settles.
    rig.frame(false);
    let off = rig.frame(false);
    assert!(!rig.handle.report().subsurface);
    let on = rig.frame(true);
    let report = rig.handle.report();
    assert!(report.drawing && report.subsurface, "{report:?}");
    let changed = off
        .as_chunks::<4>()
        .0
        .iter()
        .zip(on.as_chunks::<4>().0)
        .filter(|(a, b)| a != b)
        .count();
    eprintln!("{changed} pixels changed by the foliage transmission");
    assert!(
        changed > (W * H / 50) as usize,
        "the back-lit grass did not change: {changed} pixels"
    );
    let again = rig.frame(false);
    assert!(again == off, "off after on is not the off frame");
    assert!(!rig.handle.report().subsurface);
    // A fresh rig that never had it on renders the same bytes.
    let mut fresh = Rig::new();
    fresh.frame(false);
    assert!(fresh.frame(false) == off, "a rig that never scattered differs");
}
