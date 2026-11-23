// SPDX-License-Identifier: MPL-2.0
//! The screen-noise phase a shared draw dithers by counts from the last camera cut, like the mesh
//! path's it stays the complement of: a render-out job's frames do not depend on how many frames the
//! renderer drew before its opening cut (ADR-1881).

use std::sync::{Arc, Mutex};

use regia_render::{
    AntiAliasing, DirectionalLightUniform, FrameRenderer, LabMaterial, LabMesh, LabScene, RenderDevice,
    RenderProfilePreset, RenderSettings, SharedFrameInfo, SharedPass, SharedPassDraw, SharedUpload,
};

struct Recorder(Arc<Mutex<Vec<f32>>>);

impl SharedPassDraw for Recorder {
    fn prepare(&mut self, info: &SharedFrameInfo, _upload: &mut SharedUpload<'_>) {
        self.0.lock().expect("log").push(info.noise_index);
    }
    fn draw<'a>(&'a self, _pass: &mut wgpu::RenderPass<'a>, _kind: SharedPass) {}
}

#[test]
fn the_noise_index_restarts_at_a_camera_cut() {
    let Ok(device) = RenderDevice::headless() else {
        eprintln!("skip: no adapter");
        return;
    };
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.profile.anti_aliasing = AntiAliasing::Taa;
    let (vertices, indices) = regia_render::uv_sphere(0.5, 12, 6);
    let lab = LabScene {
        width: 64,
        height: 36,
        eye: [0.0, 0.5, 3.0],
        target: [0.0; 3],
        fov_y: 1.0,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 10_000.0,
        double_sided: false,
        post: Some(settings.resolve_post()),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.3], [1.0; 3], 45_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes: vec![LabMesh {
            vertices,
            indices,
            material: 0,
            ..LabMesh::unskinned()
        }],
    };
    let frame = lab.as_frame_scene_with_quality(settings.quality());
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(Recorder(log.clone()))));
    for _ in 0..5 {
        renderer.render_offscreen(&device, &frame).expect("frame");
    }
    let before = std::mem::take(&mut *log.lock().expect("log"));
    assert_eq!(before.len(), 5);
    assert!(before.iter().any(|index| *index != 0.0), "TAA is on: {before:?}");
    renderer.camera_cut();
    for _ in 0..5 {
        renderer.render_offscreen(&device, &frame).expect("frame");
    }
    assert_eq!(*log.lock().expect("log"), before);
}
