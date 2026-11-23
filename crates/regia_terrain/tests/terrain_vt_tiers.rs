// SPDX-License-Identifier: MPL-2.0
//! Terrain virtual texture beyond Desktop Low on Linux Vulkan (ADR-2122).
//!
//! - **The late-depth path** (DX12 and Metal under wgpu 30 offer no forced early depth tests):
//!   a lavapipe device that withholds `SHADER_EARLY_DEPTH_TEST` stands in for them. Settings
//!   without `late_depth_fallback` stay off there and the frame is the uncached one, bit for
//!   bit; with it, the texture draws (debug view 96 recolours what it serves), the held view
//!   converges, and the frame matches both the uncached frame and the early-depth frame.
//! - **Desktop High's budget**: the render profile's lever on a Desktop High frame turns on
//!   `VirtualTextureSettings::for_profile()`, every frame bakes within its per-frame caps, and
//!   the texture's GPU memory, read from the allocator report, stays within
//!   `QualityTier::TERRAIN_VT_HIGH_BUDGET_BYTES`.
//!
//! - **The profile's late-depth bit**: the lever alone keeps a late-depth device's frame the
//!   uncached one, bit for bit; `TerrainBudget::vt_late_depth` turns the late path on.
//!
//! One test at a time ([`GPU`]): the budget test reads the shared device's allocator report,
//! which another test's texture would add to.

use std::sync::{Mutex, OnceLock};

use regia_render::quality::QualityTier;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerTextures, MaterialParams, Rgba8Image, TerrainLayers, TerrainMap,
    TerrainMaterial, TerrainSharedDraw, TerrainVtHandle, VirtualTextureSettings, VtCache, VtDepthTest,
};

const W: u32 = 320;
const H: u32 = 180;
const EYE: [f32; 3] = [10.0, 15.0, -60.0];

static GPU: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    GPU.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The full-featured device (forced early depth tests on lavapipe).
fn early_device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// Whether the full-featured device really has forced early depth tests. Metal does not, so the
/// "early" device is the late one there and the early-path checks have nothing to compare
/// (ADR-2398, as ADR-2010).
fn has_early_depth() -> bool {
    early_device()
        .device
        .features()
        .contains(wgpu::Features::SHADER_EARLY_DEPTH_TEST)
}

/// The same adapter without forced early depth tests: the DX12 / Metal path.
fn late_device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(
        || match RenderDevice::headless_withholding(wgpu::Features::SHADER_EARLY_DEPTH_TEST) {
            Ok(device) => device,
            Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
        },
    )
}

/// 512 m rolling hills with a 30 m cliff across the view (the `terrain_vt` map).
fn map() -> TerrainMap {
    let n = 513u32;
    let world = 512.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * world - world * 0.5;
            let z = row as f32 / (n - 1) as f32 * world - world * 0.5;
            let t = ((x - 20.0) / 12.0).clamp(0.0, 1.0);
            let cliff = 30.0 * t * t * (3.0 - 2.0 * t);
            heights
                .push(8.0 * (x / 23.0).sin() * (z / 31.0).cos() + 3.0 * (x / 7.0 + z / 11.0).sin() + cliff);
        }
    }
    TerrainMap::from_heights(n, n, heights, world, 40.0, -20.0).expect("map")
}

/// A 64² albedo with texel-scale value noise around `v`.
fn noise_albedo(v: u8) -> Rgba8Image {
    let mut img = Rgba8Image::solid(64, 64, [v, v, v, 255]);
    let mut seed = 0x9e37_79b9u32;
    for px in img.data.as_chunks_mut::<4>().0 {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let d = (seed % 61) as i32 - 30;
        let c = (i32::from(v) + d).clamp(0, 255) as u8;
        px[0] = c;
        px[1] = c;
        px[2] = c;
    }
    img
}

/// The draw on `device` before any host choice about the texture.
fn draw(device: &RenderDevice, debug_view: u32) -> TerrainSharedDraw {
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [1.0, 1.0, 1.0],
            roughness: 1.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    let v = regia_terrain::linear_to_srgb8(0.4);
    let mut draw = TerrainSharedDraw::new(device, map(), &stack, CdlodConfig::default(), 3).expect("terrain");
    draw.set_material(
        &TerrainMaterial {
            layers: vec![Some(LayerTextures {
                albedo: noise_albedo(v),
                normal: Rgba8Image::neutral_normal(64),
                height: Rgba8Image::neutral_height(64),
                orm: Rgba8Image::solid(64, 64, [255, 255, 0, 255]),
                use_orm: false,
            })],
            macro_set: None,
            params: MaterialParams::default(),
        },
        &[],
    )
    .expect("material");
    draw.set_debug_view(debug_view);
    draw
}

/// The small, fast-converging settings of `terrain_vt` (whole-surface cache).
fn small(late_depth_fallback: bool) -> VirtualTextureSettings {
    VirtualTextureSettings {
        lod0_texel_m: 0.05,
        page_texels: 30,
        atlas_slots: 24,
        near_m: 0.0,
        bakes_per_frame: 600,
        rebakes_per_frame: 0,
        stochastic_lod: false,
        cache: VtCache::Surface,
        late_depth_fallback,
        ..VirtualTextureSettings::default()
    }
}

fn scene() -> (LabScene, PostSettings) {
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.5,
        far: 4000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([-0.5, -0.6, 0.3], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    (scene, post)
}

struct Run {
    pixels: Vec<u8>,
    handle: TerrainVtHandle,
    depth_test: VtDepthTest,
}

/// `frames` frames of the held view on `device` with `vt` chosen by the host (`None` = off),
/// Desktop High quality without TAA and with isotropic filtering (as `terrain_vt` measures).
fn run(device: &RenderDevice, vt: Option<VirtualTextureSettings>, debug_view: u32, frames: usize) -> Run {
    let mut draw = draw(device, debug_view);
    draw.set_virtual_texture(vt).expect("virtual texture settings");
    let handle = draw.vt_handle();
    let depth_test = draw.gpu().vt_depth_test();
    let (scene, post) = scene();
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.terrain_budget.anisotropy = 0;
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let mut pixels = Vec::new();
    for _ in 0..frames {
        let mut frame = scene.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        pixels = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    }
    Run {
        pixels,
        handle,
        depth_test,
    }
}

fn rgb(px: &[u8]) -> Vec<[f32; 3]> {
    px.as_chunks::<4>()
        .0
        .iter()
        .map(|c| {
            [
                f32::from(c[0]) / 255.0,
                f32::from(c[1]) / 255.0,
                f32::from(c[2]) / 255.0,
            ]
        })
        .collect()
}

fn ssim(a: &[u8], b: &[u8]) -> f32 {
    regia_render::compare::ssim(&rgb(a), &rgb(b), W, H)
}

#[test]
fn the_late_depth_path_draws_the_texture_only_when_asked() {
    let _gpu = serial();
    let late = late_device();
    assert!(
        !late
            .device
            .features()
            .contains(wgpu::Features::SHADER_EARLY_DEPTH_TEST),
        "the stand-in device must lack forced early depth tests"
    );
    let off = run(late, None, 0, 20);
    assert_eq!(off.depth_test, VtDepthTest::Late);
    assert!(
        !off.handle.is_supported(),
        "without the fallback a late-depth device reads unsupported, as before ADR-2122"
    );
    assert_eq!(off.handle.depth_test(), VtDepthTest::Late);

    // Without the fallback the setting is kept and the terrain shades in full: the frame is
    // the uncached one, bit for bit, and no page exists.
    let kept = run(late, Some(small(false)), 0, 20);
    assert_eq!(kept.handle.settings(), Some(small(false)));
    assert!(!kept.handle.is_supported());
    assert!(kept.handle.stats().is_none(), "{:?}", kept.handle.stats());
    assert!(
        kept.pixels == off.pixels,
        "a texture without the fallback changed the frame"
    );

    // With it, the texture's own debug view recolours every pixel it serves: the late-depth
    // colour pipeline draws.
    let probe = run(late, Some(small(true)), 96, 20);
    assert!(probe.handle.is_supported());
    let recoloured = off
        .pixels
        .iter()
        .zip(&probe.pixels)
        .filter(|(a, b)| a.abs_diff(**b) > 8)
        .count();
    assert!(
        recoloured > 20_000,
        "debug view 96 changed only {recoloured} channels"
    );

    // The held view converges and matches the uncached frame as the early path does (the
    // whole-surface floor of `terrain_vt`).
    let on = run(late, Some(small(true)), 0, 20);
    let stats = on.handle.stats().expect("late-depth stats");
    println!("late depth: {stats:?}");
    assert_eq!(stats.missing, 0, "requested pages left unbaked: {stats:?}");
    assert_eq!(
        stats.baked + stats.rebaked,
        0,
        "a held view still bakes: {stats:?}"
    );
    assert!(stats.resident > 0 && stats.hit_rate > 0.99, "{stats:?}");
    let vs_off = ssim(&off.pixels, &on.pixels);

    if !has_early_depth() {
        eprintln!("skipped the early-depth half: this device has no early depth tests");
        return;
    }
    // And the early-depth frame of the same settings: the depth test's timing changes which
    // pages are requested (occluded fragments ask too), not what a visible pixel reads.
    let early = run(early_device(), Some(small(true)), 0, 20);
    assert_eq!(early.depth_test, VtDepthTest::Early);
    let early_stats = early.handle.stats().expect("early-depth stats");
    println!("early depth: {early_stats:?}");
    let vs_early = ssim(&early.pixels, &on.pixels);
    let max_early = early
        .pixels
        .iter()
        .zip(&on.pixels)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    println!("late vs uncached ssim {vs_off:.5}; late vs early ssim {vs_early:.5}, max |d| {max_early}");
    assert!(vs_off > 0.95, "late vs uncached: ssim {vs_off}");
    assert!(
        vs_early > 0.995,
        "late vs early: ssim {vs_early}, max |d| {max_early}"
    );
    assert!(
        stats.resident >= early_stats.resident,
        "late depth requests at least the visible pages: {} < {}",
        stats.resident,
        early_stats.resident
    );
}

/// `frames` frames of the held view on `device` with the texture left to the render profile's
/// lever (`TerrainBudget::virtual_texture` and `vt_late_depth`), as `run` otherwise.
fn run_profile(device: &RenderDevice, lever: bool, late_depth: bool, frames: usize) -> Run {
    let draw = draw(device, 0);
    let handle = draw.vt_handle();
    let depth_test = draw.gpu().vt_depth_test();
    let (scene, post) = scene();
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.terrain_budget.anisotropy = 0;
    quality.terrain_budget.virtual_texture = lever;
    quality.terrain_budget.vt_late_depth = late_depth;
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let mut pixels = Vec::new();
    for _ in 0..frames {
        let mut frame = scene.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        pixels = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    }
    Run {
        pixels,
        handle,
        depth_test,
    }
}

/// The profile's lever on a device without forced early depth tests: without `vt_late_depth`
/// the texture stays off and the frame is the lever-off frame, bit for bit (the behaviour before
/// the late path existed, which the default policy keeps); with it the profile's settings carry
/// the fallback and the texture runs. On an early-depth device the bit changes nothing.
#[test]
fn the_profile_lever_takes_the_late_path_only_with_its_bit() {
    let _gpu = serial();
    let late = late_device();
    let profile = VirtualTextureSettings::for_profile();
    assert!(!profile.late_depth_fallback);
    let with_fallback = VirtualTextureSettings {
        late_depth_fallback: true,
        ..profile
    };

    let off = run_profile(late, false, false, 3);
    assert_eq!(off.depth_test, VtDepthTest::Late);
    assert_eq!(off.handle.settings(), None);

    let asked = run_profile(late, true, false, 3);
    assert_eq!(asked.handle.settings(), Some(profile));
    assert!(!asked.handle.is_supported());
    assert!(asked.handle.stats().is_none(), "{:?}", asked.handle.stats());
    assert!(
        asked.pixels == off.pixels,
        "the lever without its late-depth bit changed a late-depth frame"
    );

    let late_on = run_profile(late, true, true, 4);
    assert_eq!(late_on.handle.settings(), Some(with_fallback));
    assert!(late_on.handle.is_supported());
    let stats = late_on
        .handle
        .stats()
        .expect("the late-depth bit turned the texture on");
    println!("profile lever, late depth: {stats:?}");
    assert!(stats.resident > 0, "{stats:?}");

    // Early depth: the profile's own settings run whatever the bit says.
    if !has_early_depth() {
        eprintln!("skipped the early-depth half: this device has no early depth tests");
        return;
    }
    for bit in [false, true] {
        let early = run_profile(early_device(), true, bit, 2);
        assert_eq!(early.depth_test, VtDepthTest::Early);
        assert!(early.handle.is_supported());
        assert!(early.handle.stats().is_some(), "bit {bit}");
    }
}

/// Bytes of every live GPU allocation whose label starts with "terrain vt" on `device`.
fn vt_bytes(device: &RenderDevice) -> Option<u64> {
    let report = device.device.generate_allocator_report()?;
    Some(
        report
            .allocations
            .iter()
            .filter(|a| a.name.starts_with("terrain vt"))
            .map(|a| a.size)
            .sum(),
    )
}

#[test]
fn desktop_high_turns_the_profile_texture_on_within_its_budget() {
    let _gpu = serial();
    if !has_early_depth() {
        eprintln!("skipped: this device cannot run the terrain virtual texture early");
        return;
    }
    let device = early_device();
    let before = vt_bytes(device);
    let draw = draw(device, 0);
    let handle = draw.vt_handle();
    let (scene, post) = scene();
    // Desktop High exactly as the detector leaves it under `TerrainVtPolicy { high: true }`:
    // the preset's frame quality with the profile lever on.
    let mut profile = RenderProfile::from_preset(RenderProfilePreset::DesktopHigh);
    profile.terrain_budget.virtual_texture = true;
    let quality = FrameQuality::from_profile(&profile);
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let settings = VirtualTextureSettings::for_profile();
    let mut resident = Vec::new();
    for i in 0..6 {
        let mut frame = scene.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        renderer.render_offscreen(device, &frame).expect("frame");
        let stats = handle.stats().expect("the profile lever turned the texture on");
        assert!(
            stats.baked <= settings.bakes_per_frame && stats.rebaked <= settings.rebakes_per_frame,
            "frame {i} baked past the per-frame caps: {stats:?}"
        );
        resident.push(stats.resident);
    }
    assert_eq!(handle.settings(), Some(settings));
    println!("resident pages per frame: {resident:?}");
    assert!(
        resident.last().is_some_and(|&n| n > 0),
        "no page baked: {resident:?}"
    );

    let atlas = settings.atlas_bytes();
    assert_eq!(atlas, 671_088_640);
    match (before, vt_bytes(device)) {
        (Some(before), Some(after)) => {
            let used = after.saturating_sub(before);
            println!(
                "terrain vt GPU memory: {used} bytes (atlas {atlas}, budget {})",
                QualityTier::TERRAIN_VT_HIGH_BUDGET_BYTES
            );
            assert!(
                used >= atlas,
                "the atlas is not in the allocator report: {used} < {atlas}"
            );
            assert!(
                used <= QualityTier::TERRAIN_VT_HIGH_BUDGET_BYTES,
                "{used} bytes over the Desktop High budget {}",
                QualityTier::TERRAIN_VT_HIGH_BUDGET_BYTES
            );
        }
        _ => eprintln!("SKIP memory: this backend has no allocator report"),
    }
}
