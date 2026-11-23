// SPDX-License-Identifier: MPL-2.0
//! Grass on the dynamic-caster shadow class (spec 88 § 2.4, ADR-2028), lavapipe: a swaying
//! [`GrassLayer`] bound to the shared terrain draw under a whole `FrameRenderer` frame.
//!
//! - A still sway clock keeps the rest-pose shadows: the class on renders the same bytes as
//!   the class off, and the grass reports no swaying cascade and no box.
//! - Once the clock runs, the cascades the class refreshes draw the swayed blade, the shadow
//!   patches join the class as dynamic boxes and their pages are redrawn every frame; the class
//!   off keeps the rest pose.
//! - Deterministic: two rigs with the same clock and wind render the same bytes.
//! - The scene wind reaches the grass: a wind across the field changes the frame.
//! - The terrain wind (`TerrainWind`, ADR-1853) drives the same swayed shadow, and a set scene wind
//!   replaces its direction: still air casts no box, and a 5 m/s scene wind along +Z renders the
//!   same bytes as a terrain wind along +Z.

use regia_render::{
    DirectionalLightUniform, DynamicCasterShadows, FrameQuality, FrameRenderer, LabScene, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, GrassLayer, GrassMask, GrassMaskInputs, GrassQuality, GrassRequest, GrassSet,
    TerrainGrassHandle, TerrainLayers, TerrainMap, TerrainSharedDraw, TerrainWind,
};

const W: u32 = 192;
const H: u32 = 108;

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

/// 256 m of gently rolling ground.
fn map() -> TerrainMap {
    let n = 129u32;
    let world = 256.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let z = (row as f32 / (n - 1) as f32 - 0.5) * world;
            let x = (col as f32 / (n - 1) as f32 - 0.5) * world;
            heights.push(0.8 * (x * 0.021).sin() * (z * 0.017).cos());
        }
    }
    TerrainMap::from_heights(n, n, heights, world, 1.0, 0.0).unwrap_or_else(|err| panic!("map: {err}"))
}

fn device() -> RenderDevice {
    RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"))
}

struct Rig {
    device: RenderDevice,
    renderer: FrameRenderer,
    handle: TerrainGrassHandle,
    map: TerrainMap,
    post: PostSettings,
}

impl Rig {
    /// Summer grass (it sways) on the Ultra tier, the class set to `class`.
    fn new(class: DynamicCasterShadows) -> Self {
        Self::with_grass(class, GrassQuality::Ultra)
    }

    /// [`Self::new`] on the grass tier `quality`.
    fn with_grass(class: DynamicCasterShadows, quality: GrassQuality) -> Self {
        let device = device();
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
        handle.set_request(GrassRequest {
            quality: Some(quality),
            // A strong sway, so the moving shadow is well above lavapipe's noise.
            sway_scale: 4.0,
            ..GrassRequest::default()
        });
        draw.set_grass(Some(layer));
        let mut renderer = FrameRenderer::new();
        renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
        renderer.set_dynamic_caster_shadows(class);
        let post = PostSettings {
            auto_exposure: false,
            exposure_ev: 14.0,
            bloom: false,
            taa: false,
            ..PostSettings::default()
        };
        Self {
            device,
            renderer,
            handle,
            map,
            post,
        }
    }

    /// One frame at sway clock `time` (seconds).
    fn frame(&mut self, time: f32) -> Vec<u8> {
        self.handle.set_time(time);
        let (x, z) = (12.0, 40.0);
        let ground = self.map.sample_bilinear(x, z);
        let target = self.map.sample_bilinear(x, z - 30.0);
        let lab = LabScene {
            width: W,
            height: H,
            eye: [x, ground + 1.2, z],
            target: [x, target + 0.2, z - 30.0],
            fov_y: 0.9,
            near: 0.05,
            far: 1000.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: true,
            exposure_ev: 14.0,
            ambient_lux: 9_000.0,
            double_sided: false,
            post: Some(self.post.clone()),
            // A low sun from the side: long blade shadows.
            directional: DirectionalLightUniform::new([0.75, -0.21, -0.62], [1.0, 0.93, 0.84], 60_000.0),
            local_lights: Vec::new(),
            materials: Vec::new(),
            meshes: Vec::new(),
        };
        let mut quality =
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
        quality.taau = false;
        quality.render_scale_permille = 1000;
        let mut scene = lab.as_frame_scene_with_quality(quality);
        scene.post = Some(&self.post);
        self.renderer
            .render_offscreen(&self.device, &scene)
            .unwrap_or_else(|err| panic!("frame: {err}"))
            .pixels
    }
}

const ON: DynamicCasterShadows = DynamicCasterShadows::On {
    cascades: 1,
    local_lights: false,
};

/// Serialises device creation and teardown across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn a_still_clock_keeps_the_rest_pose_shadows() {
    let _gpu = serial();
    let mut on = Rig::new(ON);
    let mut off = Rig::new(DynamicCasterShadows::Off);
    for _ in 0..2 {
        let a = on.frame(2.0);
        let b = off.frame(2.0);
        assert!(a == b, "the class changed a still clock's frame");
    }
    let report = on.handle.report();
    assert!(report.drawing && report.shadow_patches > 0, "{report:?}");
    assert_eq!(report.shadow_sway_cascades, 0, "{report:?}");
    assert_eq!(report.dynamic_boxes, 0, "{report:?}");
    assert_eq!(on.renderer.dynamic_caster_report().shared_boxes, 0);
}

#[test]
fn a_running_clock_sways_the_refreshed_cascades_shadows() {
    let _gpu = serial();
    let mut on = Rig::new(ON);
    let mut off = Rig::new(DynamicCasterShadows::Off);
    for step in 0..4 {
        let t = 1.0 + step as f32 * 0.25;
        on.frame(t);
        off.frame(t);
    }
    let report = on.handle.report();
    assert_eq!(report.shadow_sway_cascades, 1, "{report:?}");
    assert!(report.dynamic_boxes > 0, "{report:?}");
    let class = on.renderer.dynamic_caster_report();
    assert_eq!(class.shared_boxes, report.dynamic_boxes, "{class:?}");
    assert!(class.dynamic_pages > 0, "{class:?}");
    // The class off: rest-pose shadows, no boxes, nothing refreshed.
    let report = off.handle.report();
    assert_eq!(report.shadow_sway_cascades, 0, "{report:?}");
    assert_eq!(report.dynamic_boxes, 0, "{report:?}");
    assert_eq!(off.renderer.dynamic_caster_report().dynamic_pages, 0);
    // The same clock on both rigs, but only the class's shadow sways: the frames differ.
    let a = on.frame(2.1);
    let b = off.frame(2.1);
    assert!(a != b, "the swayed shadow changed nothing");
}

#[test]
fn swaying_shadows_are_deterministic_and_follow_the_scene_wind() {
    let _gpu = serial();
    let run = |wind: Option<[f32; 2]>| {
        let mut rig = Rig::new(ON);
        rig.renderer.set_foliage_wind(wind);
        let mut last = Vec::new();
        for step in 0..3 {
            last = rig.frame(1.0 + step as f32 * 0.3);
        }
        last
    };
    let a = run(None);
    let b = run(None);
    assert!(a == b, "two identical swaying rigs rendered different frames");
    let windy = run(Some([0.0, 9.0]));
    assert!(windy != a, "the scene wind did not reach the grass");
}

/// The two winds meet here: the terrain wind alone sways the class's shadow, a scene wind replaces
/// its direction (at the 5 m/s where the factor is 1, a terrain wind along +Z and a scene wind
/// along +Z are the same frame), and a terrain wind of strength 0 stays still under either.
#[test]
fn the_scene_wind_overrides_the_terrain_winds_direction_in_the_swaying_shadow() {
    let _gpu = serial();
    let run = |terrain: TerrainWind, scene: Option<[f32; 2]>| {
        let mut rig = Rig::new(ON);
        rig.handle.set_wind(terrain);
        rig.renderer.set_foliage_wind(scene);
        let mut last = Vec::new();
        for step in 0..3 {
            last = rig.frame(1.0 + step as f32 * 0.3);
        }
        (last, rig.handle.report())
    };
    let along_z = TerrainWind {
        direction: [0.0, 1.0],
        ..TerrainWind::default()
    };
    let (default_wind, _) = run(TerrainWind::default(), None);
    let (terrain_z, report) = run(along_z, None);
    assert!(
        report.shadow_sway_cascades > 0 && report.dynamic_boxes > 0,
        "{report:?}"
    );
    assert!(
        terrain_z != default_wind,
        "the terrain wind did not reach the grass"
    );
    let (scene_z, _) = run(TerrainWind::default(), Some([0.0, 5.0]));
    assert!(
        scene_z == terrain_z,
        "the scene wind did not replace the direction"
    );
    let still = TerrainWind {
        strength: 0.0,
        ..TerrainWind::default()
    };
    for scene in [None, Some([0.0, 5.0])] {
        let (_, report) = run(still, scene);
        assert_eq!(report.shadow_sway_cascades, 0, "{report:?}");
        assert_eq!(report.dynamic_boxes, 0, "{report:?}");
    }
}

/// The High grass tier casts into cascade 0 alone: a class that refreshes two cascades must
/// redraw exactly the pages a one-cascade class does, or cascade 1 repaints for nothing.
#[test]
fn only_the_cascades_the_grass_draws_into_refresh() {
    let _gpu = serial();
    let pages = |cascades: u8| {
        let mut rig = Rig::with_grass(
            DynamicCasterShadows::On {
                cascades,
                local_lights: false,
            },
            GrassQuality::High,
        );
        for step in 0..4 {
            rig.frame(1.0 + step as f32 * 0.25);
        }
        let report = rig.handle.report();
        assert_eq!(report.shadow_cascades, 1, "{report:?}");
        assert_eq!(report.shadow_sway_cascades, 1, "{report:?}");
        rig.renderer.dynamic_caster_report().dynamic_pages
    };
    let one = pages(1);
    assert!(one > 0);
    assert_eq!(pages(2), one, "cascade 1 refreshed with no grass in it");
}

/// Frame cost of the swaying grass shadow with the class on and off while the clock runs (CPU
/// wall time of whole offscreen frames on lavapipe: correctness grade only; the GPU numbers
/// need a real card). Run with
/// `cargo test -p regia_terrain --test grass_shadow_sway -- --ignored --nocapture`.
#[test]
#[ignore = "bench: prints the lavapipe frame cost of grass on the dynamic class"]
fn bench_grass_on_the_dynamic_class() {
    let _gpu = serial();
    for (name, class) in [
        ("off", DynamicCasterShadows::Off),
        ("on", ON),
        ("tier", DynamicCasterShadows::Tier),
    ] {
        let mut rig = Rig::new(class);
        for step in 0..3 {
            rig.frame(step as f32 * 0.1);
        }
        let frames = 12u32;
        let mut pages = 0u32;
        let start = std::time::Instant::now();
        for step in 0..frames {
            rig.frame(1.0 + step as f32 * (1.0 / 60.0));
            pages += rig.renderer.dynamic_caster_report().dynamic_pages;
        }
        let ms = start.elapsed().as_secs_f64() * 1000.0 / f64::from(frames);
        let report = rig.handle.report();
        eprintln!(
            "grass class {name}: {ms:.2} ms per {W}x{H} frame (lavapipe), {:.1} dynamic pages per frame, \
             {} boxes, {} swaying cascades",
            f64::from(pages) / f64::from(frames),
            report.dynamic_boxes,
            report.shadow_sway_cascades
        );
    }
}
