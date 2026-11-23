// SPDX-License-Identifier: MPL-2.0
//! Grass field on the runtime frame path (lavapipe, ADR-1400): a [`GrassLayer`] bound to the
//! shared terrain draw, the whole frame rendered by `FrameRenderer` (depth prepass, cascades,
//! SSAO, colour, post).
//!
//! - Off is free: a bound layer that is disabled, or on the `Off` tier, renders the same bytes
//!   as a draw that never heard of grass.
//! - Deterministic: two rigs built from the same inputs render the same bytes, and so does a
//!   held camera's second frame (the GPU generator writes its instances in a fixed order).
//! - On covers the ground near the camera, casts shadows where the tier allows, and a density
//!   edit (a burnt patch) clears the grass it covers.
//! - The cheap tiers walk fewer candidates than the full ones.
//!
//! `REGIA_WRITE_EVIDENCE=<dir>` writes the frames as PNGs under `<dir>/grass_field/`.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, GrassLayer, GrassMask, GrassMaskInputs, GrassQuality, GrassRequest, GrassSet,
    TerrainGrassHandle, TerrainLayers, TerrainMap, TerrainSharedDraw,
};

const W: u32 = 320;
const H: u32 = 180;
const FOV: f32 = 0.9;

fn layers() -> TerrainLayers {
    TerrainLayers::from_ron(
        r#"TerrainLayers(version: 1, wetness_default: 0.0, layers: [
            LayerDesc(name: "ground", albedo: (0.20, 0.16, 0.11), roughness: 0.92, tiling: 2.0,
                slope_min: 0.0, slope_max: 1.6, altitude_min: -1000.0, altitude_max: 5000.0,
                blend_sharpness: 0.15, triplanar: false),
        ])"#,
    )
    .expect("layers")
}

/// 512 m of gently rolling ground.
fn map() -> TerrainMap {
    let n = 257u32;
    let world = 512.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let z = (row as f32 / (n - 1) as f32 - 0.5) * world;
            let x = (col as f32 / (n - 1) as f32 - 0.5) * world;
            heights.push(1.5 * (x * 0.021).sin() * (z * 0.017).cos() + 0.8 * (x * 0.07 + z * 0.05).sin());
        }
    }
    TerrainMap::from_heights(n, n, heights, world, 1.0, 0.0).expect("map")
}

fn device() -> RenderDevice {
    RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"))
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn write_evidence(name: &str, pixels: &[u8]) {
    let Some(dir) = regia_core::brand::env_var_os("REGIA_WRITE_EVIDENCE") else {
        return;
    };
    let dir = std::path::Path::new(&dir).join("grass_field");
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

/// How the rig binds grass.
#[derive(Clone, Copy, PartialEq)]
enum Bind {
    /// No layer bound.
    None,
    /// A layer bound with this request.
    Layer(Option<GrassQuality>, bool),
}

struct Rig {
    device: RenderDevice,
    renderer: FrameRenderer,
    handle: Option<TerrainGrassHandle>,
    map: TerrainMap,
    post: PostSettings,
}

impl Rig {
    fn new(device: RenderDevice, bind: Bind) -> Self {
        let map = map();
        let layers = layers();
        let mut draw = TerrainSharedDraw::new(&device, map.clone(), &layers, CdlodConfig::default(), 4)
            .expect("terrain");
        let mut handle = None;
        if let Bind::Layer(quality, enabled) = bind {
            let set = GrassSet::winter();
            let mask = GrassMask::build(
                &set,
                &GrassMaskInputs {
                    layers: Some(&layers),
                    texel_m: 1.0,
                    ..GrassMaskInputs::new(&map)
                },
            )
            .expect("mask");
            let layer = GrassLayer::new(&device.device, &device.queue, set, mask).expect("grass layer");
            let h = layer.handle();
            h.set_request(GrassRequest {
                enabled,
                quality,
                ..GrassRequest::default()
            });
            draw.set_grass(Some(layer));
            handle = Some(h);
        }
        let mut renderer = FrameRenderer::new();
        renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
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

    fn camera(&self) -> ([f32; 3], [f32; 3]) {
        let (x, z) = (12.0, 40.0);
        let ground = self.map.sample_bilinear(x, z);
        let target = self.map.sample_bilinear(x, z - 30.0);
        ([x, ground + 1.2, z], [x, target + 0.2, z - 30.0])
    }

    fn frame(&mut self) -> Vec<u8> {
        let (eye, target) = self.camera();
        let lab = LabScene {
            width: W,
            height: H,
            eye,
            target,
            fov_y: FOV,
            near: 0.05,
            far: 2000.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: true,
            exposure_ev: 14.0,
            ambient_lux: 9_000.0,
            double_sided: false,
            post: Some(self.post.clone()),
            // A low sun (about 12°) from the side: long blade shadows.
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

fn changed_fraction(a: &[u8], b: &[u8], rows: std::ops::Range<u32>) -> f32 {
    let mut changed = 0u32;
    let mut total = 0u32;
    for y in rows {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            let d: i32 = (0..3)
                .map(|k| (i32::from(a[i + k]) - i32::from(b[i + k])).abs())
                .sum();
            changed += u32::from(d > 12);
            total += 1;
        }
    }
    changed as f32 / total.max(1) as f32
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The first two frames of a fresh rig with no grass bound, rendered once per process and shared by
/// the tests that compare against it (ADR-2986). Each test still renders its own grass rigs on its
/// own device; this is only the no-grass reference, which the same inputs make the same bytes on any
/// rig. Call it while holding `SERIAL_GPU`.
fn plain_frames() -> &'static (Vec<u8>, Vec<u8>) {
    static PLAIN: std::sync::OnceLock<(Vec<u8>, Vec<u8>)> = std::sync::OnceLock::new();
    PLAIN.get_or_init(|| {
        let mut plain = Rig::new(device(), Bind::None);
        let first = plain.frame();
        let second = plain.frame();
        (first, second)
    })
}

#[test]
fn a_disabled_or_off_layer_renders_the_same_bytes_as_no_layer() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let plain = &plain_frames().0;
    let disabled = Rig::new(device(), Bind::Layer(Some(GrassQuality::Ultra), false)).frame();
    assert!(*plain == disabled, "a disabled grass layer changed the frame");
    let mut off = Rig::new(device(), Bind::Layer(Some(GrassQuality::Off), true));
    let off_frame = off.frame();
    assert!(*plain == off_frame, "the Off tier changed the frame");
    let report = off
        .handle
        .as_ref()
        .map(TerrainGrassHandle::report)
        .expect("handle");
    assert!(
        report.installed && !report.drawing && report.patches == 0,
        "{report:?}"
    );
}

#[test]
fn grass_renders_hash_identical_across_rigs_and_held_frames() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut a = Rig::new(device(), Bind::Layer(Some(GrassQuality::Ultra), true));
    let a1 = a.frame();
    let a2 = a.frame();
    let mut b = Rig::new(device(), Bind::Layer(Some(GrassQuality::Ultra), true));
    let b1 = b.frame();
    let b2 = b.frame();
    eprintln!(
        "grass frame hashes: rig a {:016x} then {:016x}, rig b {:016x} then {:016x}",
        fnv(&a1),
        fnv(&a2),
        fnv(&b1),
        fnv(&b2)
    );
    write_evidence("ultra.png", &a1);
    assert!(a1 == b1, "two identical rigs rendered different first frames");
    assert!(a2 == b2, "two identical rigs rendered different held frames");
    // A held camera re-renders the same grass, unless the renderer's own held frame moves
    // (checked on a rig without grass: then the grass is not the cause).
    let (p1, p2) = plain_frames();
    if p1 == p2 {
        assert!(a1 == a2, "grass changed a held camera's second frame");
    } else {
        eprintln!(
            "the renderer's held frame differs without grass too: the grass held-frame check is skipped"
        );
    }
}

#[test]
fn grass_covers_the_near_ground_and_reports_what_it_drew() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let plain = &plain_frames().0;
    let mut rig = Rig::new(device(), Bind::Layer(Some(GrassQuality::Ultra), true));
    let grass = rig.frame();
    write_evidence("none.png", plain);
    // The lower third of the frame is ground within a few metres: grass covers most of it.
    let near = changed_fraction(plain, &grass, (H * 2 / 3)..H);
    assert!(
        near > 0.5,
        "grass changed only {:.1} % of the near ground",
        near * 100.0
    );
    let report = rig
        .handle
        .as_ref()
        .map(TerrainGrassHandle::report)
        .expect("handle");
    assert!(report.installed && report.drawing, "{report:?}");
    assert_eq!(report.quality, "ultra");
    assert!(!report.quality_auto);
    assert!(
        report.patches > 0 && report.camera_patches > 0 && report.candidates > 0,
        "{report:?}"
    );
    assert!(
        report.patches_per_level.iter().filter(|&&c| c > 0).count() >= 3,
        "{report:?}"
    );
    assert!(
        report.shadow_patches > 0 && report.shadow_cascades > 0,
        "{report:?}"
    );
}

#[test]
fn grass_shadows_and_edits_change_the_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut rig = Rig::new(device(), Bind::Layer(Some(GrassQuality::Ultra), true));
    let shadowed = rig.frame();
    let handle = rig.handle.clone().expect("handle");
    handle.set_request(GrassRequest {
        quality: Some(GrassQuality::Ultra),
        shadows: false,
        ..GrassRequest::default()
    });
    let unshadowed = rig.frame();
    write_evidence("ultra_no_shadows.png", &unshadowed);
    assert!(
        changed_fraction(&shadowed, &unshadowed, (H / 2)..H) > 0.02,
        "grass shadows did not change the frame"
    );
    assert_eq!(handle.report().shadow_cascades, 0);
    // A burnt patch in front of the camera: the ground under it returns to bare terrain.
    let (eye, _) = rig.camera();
    handle.scale_density(eye[0], eye[2] - 4.0, 2.5, 0.0);
    let burnt = rig.frame();
    write_evidence("ultra_burnt_patch.png", &burnt);
    assert!(
        changed_fraction(&unshadowed, &burnt, (H / 2)..H) > 0.02,
        "the edit changed nothing"
    );
    assert!(handle.report().mask_generation > 1);
}

#[test]
fn cheap_tiers_walk_fewer_candidates() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut walked = Vec::new();
    let mut low_shadow_cascades = None;
    for quality in [
        GrassQuality::Minimal,
        GrassQuality::Low,
        GrassQuality::High,
        GrassQuality::Ultra,
    ] {
        let mut rig = Rig::new(device(), Bind::Layer(Some(quality), true));
        let pixels = rig.frame();
        write_evidence(&format!("tier_{}.png", quality.name()), &pixels);
        let report = rig
            .handle
            .as_ref()
            .map(TerrainGrassHandle::report)
            .expect("handle");
        assert!(report.drawing, "{} drew nothing: {report:?}", quality.name());
        eprintln!(
            "tier {:<8} patches {:>4} candidates {:>8} range {:>5.0} m shadows {} cascades",
            quality.name(),
            report.patches,
            report.candidates,
            report.range_m,
            report.shadow_cascades
        );
        walked.push(report.candidates);
        // The loop's Low rig is a fresh rig after one frame: the same rig and frame the shadow
        // check below used to rebuild (ADR-2986).
        if quality == GrassQuality::Low {
            low_shadow_cascades = Some(report.shadow_cascades);
        }
    }
    assert!(walked.windows(2).all(|w| w[0] < w[1]), "{walked:?}");
    // The fallback tiers cast no grass shadows.
    assert_eq!(low_shadow_cascades, Some(0));
}

#[test]
fn the_profile_picks_the_tier_when_the_request_does_not() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut rig = Rig::new(device(), Bind::Layer(None, true));
    rig.frame();
    let report = rig
        .handle
        .as_ref()
        .map(TerrainGrassHandle::report)
        .expect("handle");
    assert!(report.quality_auto);
    // Desktop High's terrain budget implies the High tier.
    assert_eq!(report.quality, "high", "{report:?}");
}
