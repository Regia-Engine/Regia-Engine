// SPDX-License-Identifier: MPL-2.0
//! A surface in front of the cloud layer takes no cloud radiance (ADR-5792).
//!
//! The cloud composite (`clouds.wgsl` `fs_cloud_composite`) upsamples a low-resolution march over
//! the whole frame. A low-resolution texel whose centre is on a near surface but whose footprint
//! reaches the sky holds cloud radiance, and the reconstruction's history fetch spreads it over
//! the surface texels beside it with a scatter distance of 0, which the composite's "the cloud is
//! behind this pixel" test read as "no cloud to drop". Trees against a cloudy sky carried
//! texel-sized specks of cloud (the forest demo, every tier that draws clouds).
//!
//! The scene: a black unlit post 12 m from the camera, against a cloudy sky, with the wind
//! blowing across it for a number of frames. Unlit black is 0 in the HDR target, so a pixel of the
//! post that is brighter than in the same frame without clouds is cloud radiance.
//!
//! Lavapipe-deterministic: fixed exposure, no temporal anti-aliasing, a fixed clock.

use regia_render::{
    AtmosphereParameters, CloudSettings, DirectionalLightUniform, FrameQuality, FrameRenderer, GiMode,
    LabMaterial, LabMesh, LabScene, LabVertex, OutdoorParams, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, SsaoMode,
};

const W: u32 = 320;
const H: u32 = 180;
/// Frames the wind blows for. The reprojection marches one texel in 16 a frame, so every texel
/// has been rebuilt from its history many times by the end.
const FRAMES: u32 = 40;

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip cloud near surface: {err}");
            None
        }
    }
}

/// Desktop High's sky and clouds with the reprojected low-resolution march, and none of the
/// screen-space, GI, shadow or temporal passes that would move pixels on their own.
fn quality() -> FrameQuality {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.gpu_driven = false;
    quality.ssao = SsaoMode::Off;
    quality.ssr = false;
    quality.screen_space_gi = false;
    quality.gi_probes = false;
    quality.gi = GiMode::Ambient;
    quality.contact_shadows = false;
    quality.volumetric_fog = false;
    quality.taau = false;
    quality.render_scale_permille = 1000;
    quality.csm_cascade_count = 1;
    quality.csm_resolution = 256;
    quality.local_shadow_atlas_size = 0;
    quality.reflection_probes = false;
    quality.parallax_occlusion = false;
    quality.screen_space_refraction = false;
    quality.volumetric_clouds = true;
    quality.physical_atmosphere = true;
    quality.near_sun_tile = 0;
    quality.translucent_shadow_cascades = 0;
    quality.translucent_local_shadows = false;
    quality.depth_velocity = false;
    quality.reflection_occlusion = false;
    quality.paged_sun_shadows = false;
    quality.cloud_reprojection = true;
    quality
}

/// World x range and height of the post, and its distance from the camera.
const POST_HALF_WIDTH: f32 = 1.5;
const POST_TOP: f32 = 40.0;
const POST_Z: f32 = 12.0;

/// A black unlit post against the sky, seen from 2 m above the ground, the view tilted up.
fn lab() -> LabScene {
    let n = [0.0, 0.0, -1.0];
    let vertices = vec![
        LabVertex::from_pos_normal_uv([-POST_HALF_WIDTH, -20.0, POST_Z], n, [0.0, 0.0]),
        LabVertex::from_pos_normal_uv([POST_HALF_WIDTH, -20.0, POST_Z], n, [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([POST_HALF_WIDTH, POST_TOP, POST_Z], n, [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([-POST_HALF_WIDTH, POST_TOP, POST_Z], n, [0.0, 1.0]),
    ];
    let mut black = LabMaterial::EMPTY;
    black.base_color = [0.0, 0.0, 0.0, 1.0];
    black.unlit = true;
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 2.0, 0.0],
        target: [0.0, 14.0, 40.0],
        fov_y: 55.0_f32.to_radians(),
        near: 0.3,
        far: 40_000.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 14.5,
        ambient_lux: 0.0,
        double_sided: true,
        post: Some(PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            exposure_ev: 14.5,
            fxaa: false,
            taa: false,
            film_grain: 0.0,
            vignette: 0.0,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.3, -0.8, 0.5], [1.0; 3], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![black],
        meshes: vec![LabMesh {
            vertices,
            indices: vec![0, 2, 1, 0, 3, 2],
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

/// `frames` frames of the scene on one renderer under `coverage`, the wind blowing across the
/// post; the last frame's pixels.
fn render(device: &RenderDevice, coverage: f32, frames: u32) -> Vec<u8> {
    let lab = lab();
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    let mut pixels = Vec::new();
    for frame in 0..frames {
        renderer.set_outdoor(OutdoorParams {
            clouds: CloudSettings {
                coverage,
                wind_m_per_s: [10.0, 3.0],
                // A fixed 60 Hz clock: the wind drift is the same on every machine.
                time_s: frame as f32 / 60.0,
                ..CloudSettings::default()
            },
            planet: AtmosphereParameters::earth(),
            ..OutdoorParams::default()
        });
        let scene = lab.as_frame_scene_with_quality(quality());
        pixels = renderer
            .render_offscreen(device, &scene)
            .unwrap_or_else(|err| panic!("cloud near surface frame {frame}: {err}"))
            .pixels;
    }
    pixels
}

fn dump(name: &str, pixels: &[u8]) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_DUMP_DIR") else {
        return;
    };
    let path = std::path::Path::new(&dir).join(format!("{name}.png"));
    if let Err(err) = image::save_buffer(&path, pixels, W, H, image::ColorType::Rgba8) {
        eprintln!("dump {name}: {err}");
    }
}

/// The brightest channel of the pixel at (x, y).
fn peak(pixels: &[u8], x: u32, y: u32) -> u8 {
    let i = ((y * W + x) * 4) as usize;
    pixels[i].max(pixels[i + 1]).max(pixels[i + 2])
}

/// The post's interior, found on a frame without clouds: the pixels as dark as the frame centre whose
/// eight neighbours are as dark. The silhouette's own pixels are shared with the sky by the
/// rasterizer, and the post tapers in the tilted view, so its columns differ from row to row.
fn post_interior(clear: &[u8]) -> Vec<bool> {
    let floor = peak(clear, W / 2, H / 2);
    assert!(floor <= 8, "the post is not black at the frame centre: {floor}");
    let dark = |x: u32, y: u32| peak(clear, x, y) <= floor + 1;
    let mut inner = vec![false; (W * H) as usize];
    for y in 1..H - 1 {
        for x in 1..W - 1 {
            inner[(y * W + x) as usize] = (y - 1..=y + 1).all(|yy| (x - 1..=x + 1).all(|xx| dark(xx, yy)));
        }
    }
    inner
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Under a cloudy sky the post stays black over its whole face, to the pixel next to its
/// silhouette, while the sky beside it shows the clouds. On the old composite 309 of the post's
/// 7006 interior pixels carried cloud radiance, up to 95 of 255 (lavapipe; ADR-5792).
#[test]
fn a_surface_in_front_of_the_cloud_layer_takes_no_cloud_radiance() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let clear = render(&device, 0.0, 2);
    let cloudy = render(&device, 0.9, FRAMES);
    dump("cloud_near_surface_clear", &clear);
    dump("cloud_near_surface_cloudy", &cloudy);

    let inner = post_interior(&clear);
    let post_pixels = inner.iter().filter(|&&p| p).count();
    assert!(
        post_pixels > 4_000 && inner[((H / 2) * W + W / 2) as usize],
        "the post should stand in the middle of the frame: {post_pixels} interior pixels"
    );

    // The clouds are in the frame: the sky beside the post differs from the clear sky.
    let mut sky_changed = 0u32;
    for y in 4..H - 4 {
        for x in 4..W - 4 {
            let i = ((y * W + x) * 4) as usize;
            if peak(&clear, x, y) <= 8 {
                continue;
            }
            let delta = (0..3)
                .map(|c| (i32::from(cloudy[i + c]) - i32::from(clear[i + c])).abs())
                .max()
                .unwrap_or(0);
            if delta > 8 {
                sky_changed += 1;
            }
        }
    }
    assert!(
        sky_changed > (W * H) / 20,
        "the cloudy frame shows no clouds beside the post ({sky_changed} changed pixels)"
    );

    // The post's interior: no pixel is brighter than without clouds.
    let mut lit = 0u32;
    let mut brightest = 0u8;
    let mut worst = (0u32, 0u32);
    for y in 0..H {
        for x in 0..W {
            if !inner[(y * W + x) as usize] {
                continue;
            }
            let p = peak(&cloudy, x, y);
            if p > peak(&clear, x, y) + 1 {
                lit += 1;
                if p > brightest {
                    brightest = p;
                    worst = (x, y);
                }
            }
        }
    }
    assert!(
        lit == 0,
        "{lit} of {post_pixels} pixels inside a black post in front of the cloud layer carry cloud \
         radiance (brightest {brightest} at {worst:?})"
    );
}
