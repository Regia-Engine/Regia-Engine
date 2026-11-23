// SPDX-License-Identifier: MPL-2.0
//! Plates for the owner's look-pack sheet (docs/perf/look-pack-v040.md): the default-off look
//! features that already have a lab scene, each drawn off and on with the same scene, the same camera
//! and the same frame number. Compared with nothing; `REGIA_LOOK_PACK_DIR` names the directory the PNGs
//! go to. Ignored: it exists only to leave frames for a look by eye.
//!
//! ```text
//! REGIA_LOOK_PACK_DIR=/tmp/plates cargo test -p regia_test_utils --test look_pack_sheet -- --ignored
//! ```

use regia_render::{
    FrameQuality, FrameRenderer, GiMode, IndirectStrategy, RenderDevice, RenderProfile, RenderProfilePreset,
};
use regia_test_utils::gi_scenes::{GI_HEIGHT, GI_WIDTH, color_room};
use regia_test_utils::scenes::{GOLDEN_HEIGHT, GOLDEN_WIDTH, light_shadows_scene};

fn leave(name: &str, pixels: &[u8], width: u32, height: u32) {
    let Ok(dir) = std::env::var("REGIA_LOOK_PACK_DIR") else {
        return;
    };
    let path = std::path::Path::new(&dir).join(format!("{name}.png"));
    let Some(img) = image::RgbaImage::from_raw(width, height, pixels.to_vec()) else {
        eprintln!("look_pack_sheet: {name}: frame size mismatch");
        return;
    };
    if let Err(err) = img.save(&path) {
        eprintln!("look_pack_sheet: {name}: {err}");
    }
}

fn desktop_high() -> FrameQuality {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.volumetric_clouds = false;
    quality.volumetric_fog = false;
    quality
}

/// Blue noise: the light-shadow scene on Desktop High, three frames, the table off and on.
#[test]
#[ignore = "only leaves frames in REGIA_LOOK_PACK_DIR"]
fn blue_noise_pair() {
    let device = RenderDevice::headless().expect("device");
    for on in [false, true] {
        let lab = light_shadows_scene();
        let mut quality = desktop_high();
        quality.blue_noise = on;
        let frame = lab.as_frame_scene_with_quality(quality);
        let mut renderer = FrameRenderer::new();
        let mut pixels = Vec::new();
        for _ in 0..3 {
            pixels = renderer.render_offscreen(&device, &frame).expect("frame").pixels;
        }
        leave(
            if on { "blue_noise_on" } else { "blue_noise_off" },
            &pixels,
            GOLDEN_WIDTH,
            GOLDEN_HEIGHT,
        );
    }
}

/// Radiance cache: the color room, probes settled over 80 frames, the cache off and on.
#[test]
#[ignore = "only leaves frames in REGIA_LOOK_PACK_DIR"]
fn radiance_cache_pair() {
    let device = RenderDevice::headless().expect("device");
    for on in [false, true] {
        let scene = color_room();
        let mut quality = desktop_high();
        quality.taau = false;
        quality.msaa_samples = 1;
        quality.render_scale_permille = 1000;
        quality.horizontal_plus = false;
        quality.ssr = false;
        quality.reflection_probes = false;
        quality.gi = GiMode::Probes;
        quality.gi_probes = true;
        quality.screen_space_gi = true;
        quality.baked_gi = false;
        let mut renderer = FrameRenderer::new();
        renderer.set_ddgi_settings(scene.ddgi.clone());
        renderer.set_ddgi_enabled(Some(true));
        renderer.set_radiance_cache(on);
        let frame = scene.lab.as_frame_scene_with_quality(quality);
        let mut pixels = Vec::new();
        for _ in 0..80 {
            pixels = renderer.render_offscreen(&device, &frame).expect("frame").pixels;
        }
        leave(
            if on {
                "radiance_cache_on"
            } else {
                "radiance_cache_off"
            },
            &pixels,
            GI_WIDTH,
            GI_HEIGHT,
        );
    }
}

/// Shadow occlusion: light-shadow scene on the GPU-driven path, frame two, cull off and on.
#[test]
#[ignore = "only leaves frames in REGIA_LOOK_PACK_DIR"]
fn shadow_occlusion_pair() {
    let device = RenderDevice::headless().expect("device");
    if !IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
        eprintln!("skip shadow_occlusion_pair: no GPU-driven shadow path on this device");
        return;
    }
    for on in [false, true] {
        let lab = light_shadows_scene();
        let quality = FrameQuality {
            gpu_driven: true,
            ..FrameQuality::LAB
        };
        let frame = lab.as_frame_scene_with_quality(quality);
        let mut renderer = FrameRenderer::new();
        renderer.set_shadow_occlusion(on);
        renderer.set_shadow_occlusion_min_casters(Some(1));
        renderer.render_offscreen(&device, &frame).expect("first frame");
        let pixels = renderer
            .render_offscreen(&device, &frame)
            .expect("second frame")
            .pixels;
        leave(
            if on {
                "shadow_occlusion_on"
            } else {
                "shadow_occlusion_off"
            },
            &pixels,
            GOLDEN_WIDTH,
            GOLDEN_HEIGHT,
        );
    }
}
