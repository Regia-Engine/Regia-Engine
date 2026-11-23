// SPDX-License-Identifier: MPL-2.0
//! GPU-driven colour must match the CPU path on the M5 golden scenes (SSIM ≥ 0.98).

use regia_render::PostSettings;
use regia_render::TonemapOperator;
use regia_render::{FrameQuality, FrameRenderer, LabScene, RenderDevice};
use regia_test_utils::golden::{DEFAULT_SSIM_THRESHOLD, ssim_rgba8};
use regia_test_utils::scene_loader::{LabView, ensure_model, load_gltf_model_with_view};
use regia_test_utils::scenes::SUN_EV100;
use regia_test_utils::scenes::{
    GOLDEN_HEIGHT, GOLDEN_WIDTH, alpha_modes_scene, clustered_lights_scene, ibl_spheres_scene,
    light_shadows_scene, material_sphere_grid_scene, post_demo_scene, surface_maps_scene,
};

fn render(device: &RenderDevice, scene: &LabScene, gpu: bool) -> Vec<u8> {
    let mut quality = FrameQuality::LAB;
    quality.gpu_driven = gpu;
    let mut renderer = FrameRenderer::new();
    renderer
        .render_offscreen(device, &scene.as_frame_scene_with_quality(quality))
        .expect("render")
        .pixels
}

fn assert_match(device: &RenderDevice, name: &str, scene: &LabScene) {
    let cpu = render(device, scene, false);
    let gpu = render(device, scene, true);
    assert_eq!(cpu.len(), gpu.len(), "{name} size");
    let score = ssim_rgba8(&cpu, &gpu, scene.width, scene.height).expect("ssim");
    assert!(
        score >= DEFAULT_SSIM_THRESHOLD,
        "{name} GPU vs CPU SSIM {score:.4} below {DEFAULT_SSIM_THRESHOLD}"
    );
}

#[test]
fn gpu_driven_matches_cpu_on_m5_scenes() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip gpu_driven_matches_cpu_on_m5_scenes: {err}");
            return;
        }
    };
    let post = PostSettings {
        enabled: true,
        bloom: true,
        bloom_mips: 4,
        auto_exposure: false,
        exposure_ev: SUN_EV100,
        tonemap: TonemapOperator::AgX,
        fxaa: true,
        taa: false,
        vignette: 0.55,
        film_grain: 0.0,
        ..PostSettings::default()
    };
    let scenes: Vec<(&str, LabScene)> = vec![
        ("material_sphere_grid", material_sphere_grid_scene()),
        ("clustered_lights", clustered_lights_scene()),
        ("light_shadows", light_shadows_scene()),
        ("surface_maps", surface_maps_scene()),
        ("alpha_modes", alpha_modes_scene()),
        ("ibl_spheres", ibl_spheres_scene()),
        ("post_off", post_demo_scene(None)),
        ("post_on", post_demo_scene(Some(post))),
    ];
    for (name, scene) in &scenes {
        assert_eq!(scene.width, GOLDEN_WIDTH);
        assert_eq!(scene.height, GOLDEN_HEIGHT);
        assert_match(&device, name, scene);
    }
    if let Some(sponza) = khronos_model("Sponza") {
        let scene = load_gltf_model_with_view(&sponza, LabView::SponzaAtrium, None).expect("sponza");
        assert_match(&device, "sponza", &scene);
    }
    if let Some(fox) = khronos_model("Fox") {
        let scene =
            load_gltf_model_with_view(&fox, LabView::ModelPortrait, Some(("Walk", 0.5))).expect("fox");
        assert_match(&device, "skinned_fox", &scene);
    }
}

/// A Khronos sample model, or `None` (with a note) when it was never fetched: the same policy as
/// the missing-GPU skip above, so a clean checkout is not red for an environment reason
/// (audit-v035-flaky H5). `REGIA_REQUIRE_ASSETS=1` (set by a job that must cover the samples)
/// turns a missing model back into a failure.
fn khronos_model(name: &str) -> Option<std::path::PathBuf> {
    match ensure_model(name) {
        Ok(path) => Some(path),
        Err(err) if std::env::var_os("REGIA_REQUIRE_ASSETS").is_some_and(|v| v == "1") => panic!(
            "{name} is required (REGIA_REQUIRE_ASSETS=1): {err}. Set REGIA_KHRONOS_ASSETS or run `cargo xtask fetch-test-assets`."
        ),
        Err(err) => {
            eprintln!(
                "skip the {name} compare: {err} (set REGIA_KHRONOS_ASSETS or run `cargo xtask fetch-test-assets`)"
            );
            None
        }
    }
}
