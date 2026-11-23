// SPDX-License-Identifier: MPL-2.0
//! Valley mist in the froxel fog (ADR-1394): it pools where the terrain grid hollows, a frame
//! is a function of the mist clock, the Low-tier fallback draws, and removing the mist
//! restores the plain fog image exactly. No golden images: every check compares frames
//! rendered here.
//!
//! Set `REGIA_MIST_SHEET_DIR` to write each frame as a PPM (the lane's frame sheets).

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    MistHeightGrid, OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
    ValleyMist, VolumetricFogSettings,
};

const W: u32 = 160;
const H: u32 = 90;

fn quad(corners: [[f32; 3]; 4], normal: [f32; 3]) -> LabMesh {
    LabMesh {
        vertices: corners
            .iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(*p, normal, uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material: 0,
        ..LabMesh::unskinned()
    }
}

/// A flat ground and a far wall, the sun low (10 degrees) and in front of the camera.
fn scene() -> LabScene {
    let sun_y = -(10.0_f32.to_radians()).sin();
    let sun_z = (10.0_f32.to_radians()).cos();
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 1.8, 0.0],
        target: [0.0, 6.0, -60.0],
        fov_y: 0.9,
        near: 0.1,
        far: 2_000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.0, sun_y, sun_z], [1.0, 0.95, 0.9], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.4, 0.3, 0.25, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        }],
        meshes: vec![
            quad(
                [
                    [-300.0, 0.0, 20.0],
                    [300.0, 0.0, 20.0],
                    [300.0, 0.0, -300.0],
                    [-300.0, 0.0, -300.0],
                ],
                [0.0, 1.0, 0.0],
            ),
            quad(
                [
                    [-250.0, 0.0, -200.0],
                    [250.0, 0.0, -200.0],
                    [250.0, 80.0, -200.0],
                    [-250.0, 80.0, -200.0],
                ],
                [0.0, 0.0, 1.0],
            ),
        ],
    }
}

/// A bowl 18 m deep and 70 m wide centred 30 m to the right of and 40 m ahead of the camera,
/// flat ground elsewhere.
fn bowl() -> Arc<MistHeightGrid> {
    let grid = MistHeightGrid::from_fn([-300.0, -300.0], 5.0, 121, 121, 200.0, |x, z| {
        let r = ((x - 30.0) * (x - 30.0) + (z + 40.0) * (z + 40.0)).sqrt();
        -18.0 * (1.0 - (r / 35.0).min(1.0)).powi(2)
    });
    Arc::new(grid.expect("bowl grid"))
}

fn mist(time_s: f32) -> ValleyMist {
    ValleyMist {
        density: 0.02,
        open_ground_fraction: 0.02,
        noise_strength: 0.0,
        time_s,
        grid: Some(bowl()),
        ..ValleyMist::default()
    }
}

fn fog() -> OutdoorParams {
    OutdoorParams {
        // Height fog off: only the mist adds extinction.
        fog: VolumetricFogSettings {
            density: 0.0001,
            ..VolumetricFogSettings::default()
        },
        ..OutdoorParams::default()
    }
}

fn render(
    device: &RenderDevice,
    preset: RenderProfilePreset,
    mist: Option<ValleyMist>,
    name: &str,
) -> Vec<u8> {
    render_scene(device, &scene(), preset, mist, name)
}

fn render_scene(
    device: &RenderDevice,
    lab: &LabScene,
    preset: RenderProfilePreset,
    mist: Option<ValleyMist>,
    name: &str,
) -> Vec<u8> {
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        bloom: false,
        ..PostSettings::golden_deterministic()
    };
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(fog());
    renderer.set_valley_mist(mist);
    let image = renderer.render_offscreen(device, &frame).expect("mist frame");
    if let Ok(dir) = regia_core::brand::env_var("REGIA_MIST_SHEET_DIR") {
        let mut ppm = format!("P6\n{} {}\n255\n", image.width, image.height).into_bytes();
        ppm.extend(
            image
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[1], p[2]]),
        );
        let path = std::path::Path::new(&dir).join(format!("{name}.ppm"));
        if let Err(err) = std::fs::write(&path, ppm) {
            eprintln!("could not write {}: {err}", path.display());
        }
    }
    image.pixels
}

fn hash(pixels: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    pixels.hash(&mut hasher);
    hasher.finish()
}

fn luma(pixels: &[u8], x: u32, y: u32) -> f32 {
    let i = ((y * W + x) * 4) as usize;
    0.2126 * f32::from(pixels[i]) + 0.7152 * f32::from(pixels[i + 1]) + 0.0722 * f32::from(pixels[i + 2])
}

/// Mean luma over columns `x0..x1` of the rows below the horizon.
fn band(pixels: &[u8], x0: u32, x1: u32) -> f32 {
    let (mut sum, mut n) = (0.0, 0.0);
    for y in H / 2..H {
        for x in x0..x1 {
            sum += luma(pixels, x, y);
            n += 1.0;
        }
    }
    sum / n
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    }
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn mist_pools_in_the_hollow_and_rerenders_to_the_same_hash() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let preset = RenderProfilePreset::DesktopHigh;
    let plain = render(&device, preset, None, "high_plain");
    let first = render(&device, preset, Some(mist(0.0)), "high_mist");
    let second = render(&device, preset, Some(mist(0.0)), "high_mist_again");
    assert_eq!(
        hash(&first),
        hash(&second),
        "two runs of the same frame must hash alike"
    );
    assert_ne!(hash(&plain), hash(&first), "the mist must change the image");
    // The right of the frame looks into the bowl; the left looks over flat ground that holds
    // only the open-ground share of the mist.
    let lift = |x0, x1| band(&first, x0, x1) - band(&plain, x0, x1);
    let right = lift(W / 2 + 10, W);
    let left = lift(0, W / 2 - 10);
    eprintln!("mist lift: right {right:.2}, left {left:.2}");
    assert!(right > 1.0, "mist over the bowl lifts the frame: {right}");
    assert!(
        right > 1.5 * left,
        "mist pools in the bowl: right {right} left {left}"
    );
}

#[test]
fn removing_the_mist_restores_the_plain_fog_image() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let preset = RenderProfilePreset::DesktopHigh;
    let plain = render(&device, preset, None, "restore_plain");
    // A mist set and removed before the first frame leaves no trace.
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(fog());
    renderer.set_valley_mist(Some(mist(0.0)));
    renderer.set_valley_mist(None);
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        bloom: false,
        ..PostSettings::golden_deterministic()
    };
    let lab = scene();
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let after = renderer.render_offscreen(&device, &frame).expect("without mist");
    assert_eq!(hash(&plain), hash(&after.pixels), "no mist, no change");
}

#[test]
fn the_mist_clock_moves_the_banks_and_a_frame_is_a_function_of_it() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let preset = RenderProfilePreset::DesktopHigh;
    let banks = |time_s| ValleyMist {
        noise_strength: 0.9,
        noise_scale_m: 40.0,
        ..mist(time_s)
    };
    let at_0 = render(&device, preset, Some(banks(0.0)), "clock_0");
    let at_90 = render(
        &device,
        preset,
        Some(banks(ValleyMist::time_for_frame(5400, 60.0))),
        "clock_5400",
    );
    let at_90_again = render(
        &device,
        preset,
        Some(banks(ValleyMist::time_for_frame(5400, 60.0))),
        "clock_5400_again",
    );
    assert_ne!(hash(&at_0), hash(&at_90), "the drift moves the mist");
    assert_eq!(
        hash(&at_90),
        hash(&at_90_again),
        "the same frame number renders the same mist"
    );
}

#[test]
fn the_low_tier_draws_a_reduced_mist_unless_switched_off() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    for preset in [RenderProfilePreset::DesktopLow, RenderProfilePreset::Deck] {
        let plain = render(&device, preset, None, &format!("{preset:?}_plain"));
        let reduced = render(&device, preset, Some(mist(0.0)), &format!("{preset:?}_reduced"));
        let off = render(
            &device,
            preset,
            Some(ValleyMist {
                reduced_on_low_tiers: false,
                ..mist(0.0)
            }),
            &format!("{preset:?}_off"),
        );
        assert_ne!(
            hash(&plain),
            hash(&reduced),
            "{preset:?}: the fallback draws the mist"
        );
        assert_eq!(
            hash(&plain),
            hash(&off),
            "{preset:?}: switched off leaves the frame alone"
        );
    }
}

/// The scene with a row of tall panels 12 m ahead: under a low sun they throw long shadow
/// bars across the air the mist fills.
fn fence(shadows: bool) -> LabScene {
    let mut lab = scene();
    for k in -2..=2 {
        let x = k as f32 * 6.0;
        lab.meshes.push(quad(
            [
                [x - 1.5, 0.0, -12.0],
                [x + 1.5, 0.0, -12.0],
                [x + 1.5, 14.0, -12.0],
                [x - 1.5, 14.0, -12.0],
            ],
            [0.0, 0.0, 1.0],
        ));
    }
    lab.directional_shadows = shadows;
    // The sun behind the camera, 10 degrees up: the panels' shadows fall ahead, across the bowl.
    let sun_y = -(10.0_f32.to_radians()).sin();
    let sun_z = -(10.0_f32.to_radians()).cos();
    lab.directional = DirectionalLightUniform::new([0.0, sun_y, sun_z], [1.0, 0.95, 0.9], 100_000.0);
    lab
}

/// Frame sheets of the fence with and without the sun shadow, mist on and off. It asserts
/// nothing: in this crude scene the fence hides most of the mist, so a pixel count does not
/// separate shaft shadows from the ground's (measured 455 vs 429 changed pixels). A shaft
/// assertion needs a purpose-built scene; see the lane report.
#[test]
#[ignore = "frame sheets only; set REGIA_MIST_SHEET_DIR"]
fn a_low_sun_throws_shadow_bars_through_the_mist() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let preset = RenderProfilePreset::DesktopHigh;
    let changed = |a: &[u8], b: &[u8]| {
        a.as_chunks::<4>()
            .0
            .iter()
            .zip(b.as_chunks::<4>().0)
            .filter(|(p, q)| (0..3).any(|c| p[c].abs_diff(q[c]) > 3))
            .count()
    };
    let lit = render_scene(
        &device,
        &fence(false),
        preset,
        Some(mist(0.0)),
        "fence_mist_unshadowed",
    );
    let shadowed = render_scene(
        &device,
        &fence(true),
        preset,
        Some(mist(0.0)),
        "fence_mist_shadowed",
    );
    let bare_lit = render_scene(&device, &fence(false), preset, None, "fence_plain_unshadowed");
    let bare_shadowed = render_scene(&device, &fence(true), preset, None, "fence_plain_shadowed");
    eprintln!(
        "pixels the sun shadow changes: with mist {}, without {}",
        changed(&lit, &shadowed),
        changed(&bare_lit, &bare_shadowed)
    );
}
