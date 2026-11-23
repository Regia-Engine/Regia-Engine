// SPDX-License-Identifier: MPL-2.0
//! The `natural` wind style on the grass (ADR-2446), lavapipe: a swaying [`GrassLayer`] under a
//! whole `FrameRenderer` frame.
//!
//! - `classic` (the default) is the unset wind: setting the style to `classic` explicitly renders
//!   the same bytes as never touching it.
//! - `natural` is deterministic (two rigs, same clock, same bytes), differs from `classic` at the
//!   same clock, and moves with the clock.

use regia_render::{
    DirectionalLightUniform, DynamicCasterShadows, FrameQuality, FrameRenderer, LabScene, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, GrassLayer, GrassMask, GrassMaskInputs, GrassQuality, GrassRequest, GrassSet,
    TerrainGrassHandle, TerrainLayers, TerrainMap, TerrainSharedDraw, TerrainWind, WindStyle,
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

    /// [`Self::new`] with the terrain wind's style set to `style`.
    fn styled(style: WindStyle) -> Self {
        let rig = Self::new(ON);
        rig.handle.set_wind(TerrainWind {
            style,
            ..TerrainWind::default()
        });
        rig
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
fn classic_is_the_default_and_unchanged() {
    let _gpu = serial();
    let mut unset = Rig::new(ON);
    let mut classic = Rig::styled(WindStyle::Classic);
    for t in [1.0f32, 1.4, 6.3] {
        let a = unset.frame(t);
        let b = classic.frame(t);
        assert!(a == b, "an explicit classic style changed the frame at {t}");
    }
}

#[test]
fn natural_differs_from_classic_is_deterministic_and_moves() {
    let _gpu = serial();
    let run = |style: WindStyle, times: &[f32]| {
        let mut rig = Rig::styled(style);
        times.iter().map(|t| rig.frame(*t)).collect::<Vec<_>>()
    };
    let times = [1.0, 1.3, 9.0];
    let classic = run(WindStyle::Classic, &times);
    let natural = run(WindStyle::Natural, &times);
    let again = run(WindStyle::Natural, &times);
    assert!(natural == again, "natural is not deterministic");
    assert!(natural[2] != classic[2], "natural renders as classic");
    assert!(natural[0] != natural[2], "natural did not move with the clock");
}

/// Plates for the owner's look-pack sheet (docs/perf/look-pack-v040.md): the grass with the sway scale at
/// 0 (still) and at 4 (the setting above), the same camera, at two sway clocks; evidence only.
/// `REGIA_LOOK_PACK_DIR` names the directory the PNGs go to.
#[test]
#[ignore = "only leaves frames in REGIA_LOOK_PACK_DIR"]
fn sway_plates_evidence() {
    let Ok(dir) = std::env::var("REGIA_LOOK_PACK_DIR") else {
        return;
    };
    let _gpu = serial();
    let save = |name: &str, pixels: &[u8]| {
        let path = std::path::Path::new(&dir).join(format!("{name}.png"));
        let Ok(file) = std::fs::File::create(&path) else {
            eprintln!("sway_plates_evidence: cannot create {}", path.display());
            return;
        };
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        if let Ok(mut writer) = encoder.write_header() {
            let _ = writer.write_image_data(pixels);
        }
    };
    for (scale, label) in [(0.0f32, "off"), (4.0, "on")] {
        let mut rig = Rig::new(ON);
        rig.handle.set_request(GrassRequest {
            quality: Some(GrassQuality::Ultra),
            sway_scale: scale,
            ..GrassRequest::default()
        });
        for (time, tag) in [(1.0f32, "t1"), (1.6, "t2")] {
            let pixels = rig.frame(time);
            save(&format!("sway_{label}_{tag}"), &pixels);
        }
    }
}
