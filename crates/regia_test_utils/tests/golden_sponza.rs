// SPDX-License-Identifier: MPL-2.0
//! Golden: Sponza atrium view (`specs/13` § 13).
//!
//! Loads Sponza from the Khronos glTF sample models, renders the interior of the
//! atrium from inside the nave, and compares against the blessed image.

use regia_render::{RenderDevice, render_lab_scene};
use regia_test_utils::golden::{
    DEFAULT_SSIM_THRESHOLD, GoldenGuard, compare_rgba8_to_png_on, golden_path, guard_golden,
};
use regia_test_utils::scene_loader::{LabView, ensure_model, load_gltf_model_with_view};

/// World-space bounds of every vertex in the scene (model-transformed).
fn scene_bounds(scene: &regia_render::LabScene) -> (glam::Vec3, glam::Vec3) {
    let mut lo = glam::Vec3::splat(f32::MAX);
    let mut hi = glam::Vec3::splat(f32::MIN);
    for mesh in &scene.meshes {
        let model = glam::Mat4::from_cols_array_2d(&mesh.model);
        for vertex in &mesh.vertices {
            let p = model.transform_point3(glam::Vec3::from_array(vertex.position));
            lo = lo.min(p);
            hi = hi.max(p);
        }
    }
    (lo, hi)
}

#[test]
fn golden_sponza() {
    let path = match ensure_model("Sponza") {
        Ok(p) => p,
        Err(err) => {
            eprintln!("skip golden_sponza: {err}");
            return;
        }
    };
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip golden_sponza: {err}");
            return;
        }
    };
    let mut scene = match load_gltf_model_with_view(&path, LabView::SponzaAtrium, None) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("skip golden_sponza: load failed: {err}");
            return;
        }
    };
    scene.width = 512;
    scene.height = 512;

    let result = render_lab_scene(&device, &scene).expect("render Sponza");
    // Nave view: the camera must be INSIDE the hall — within the horizontal
    // bounds, looking down the long axis (target farther from the eye than
    // half the nave length, level with the eye). A wall close-up or an outside
    // shot fails this before any pixel is compared.
    let bounds = scene_bounds(&scene);
    let eye = glam::Vec3::from_array(scene.eye);
    assert!(
        eye.x > bounds.0.x && eye.x < bounds.1.x && eye.z > bounds.0.z && eye.z < bounds.1.z,
        "sponza camera must be inside the atrium, got {eye:?} in {:?}..{:?}",
        bounds.0,
        bounds.1
    );
    let nave = (bounds.1.x - bounds.0.x).max(bounds.1.z - bounds.0.z);
    assert!(
        (eye - glam::Vec3::from_array(scene.target)).length() > 0.5 * nave,
        "sponza camera must look down the nave (nave {nave:.1} m)"
    );
    // Interior sun + sky view: walls, columns and floor should fill and expose
    // the frame; the shaded sides exist but must not dominate.
    guard_golden(
        &result.pixels,
        result.width,
        result.height,
        "sponza",
        GoldenGuard {
            min_lum: 30.0,
            max_lum: 190.0,
            max_white_pct: 3.0,
            max_black_pct: 30.0,
            min_object_pct: 90.0,
        },
    );
    compare_rgba8_to_png_on(
        &device.adapter.get_info(),
        &result.pixels,
        result.width,
        result.height,
        &golden_path("sponza"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .expect("golden compare");
}
