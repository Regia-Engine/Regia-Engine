// SPDX-License-Identifier: MPL-2.0
//! `ProbeSpecular::LobeHue` is a uniform bit (ADR-2996): after warm frames, switching the mode creates
//! no pipeline and no pooled target, with and without a ground plane. One test in its own binary,
//! so the process-wide creation counters see only it.

use regia_render::pipeline_cache::pipelines_created;
use regia_render::{
    DdgiSettings, DirectionalLightUniform, EnvGroundPlane, EnvironmentAuthority, FrameQuality, FrameRenderer,
    LabMaterial, LabMesh, LabScene, PostSettings, ProbeSpecular, RenderDevice, RenderProfile,
    RenderProfilePreset, SsaoMode, StudioGradient, translate_mesh, uv_sphere,
};

fn scene() -> LabScene {
    let (v, i) = uv_sphere(1.0, 32, 16);
    LabScene {
        width: 64,
        height: 64,
        eye: [0.0, 1.0, 4.0],
        target: [0.0, 1.0, 0.0],
        fov_y: 0.75,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 13.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            auto_exposure: false,
            exposure_ev: 13.0,
            bloom: false,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [1.0, 1.0, 1.0, 1.0],
            metallic: 1.0,
            roughness: 0.05,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: translate_mesh(&v, [0.0, 1.0, 0.0]),
            indices: i,
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

#[test]
fn switching_the_mode_on_a_warm_frame_creates_nothing() {
    let device = RenderDevice::headless().expect("lavapipe");
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.ssao = SsaoMode::Off;
    quality.taau = false;
    quality.screen_space_gi = false;
    quality.ssr = false;
    let lab = scene();
    let frame = lab.as_frame_scene_with_quality(quality);
    let plane = EnvGroundPlane {
        height: -0.1,
        albedo: [0.05, 0.9, 0.05],
        sun_bounce: false,
        ..EnvGroundPlane::default()
    };
    let gradient = StudioGradient::new(
        [2400.0, 4200.0, 8000.0],
        [2600.0, 2800.0, 3000.0],
        [600.0, 600.0, 600.0],
    );
    for ground in [Some(plane), None] {
        let mut renderer = FrameRenderer::new();
        renderer.set_environment_authority(EnvironmentAuthority::Studio);
        assert_eq!(renderer.set_studio_gradient(&device, Some(&gradient)), Ok(true));
        renderer.set_environment_ground(ground).expect("plane");
        for _ in 0..3 {
            renderer.render_offscreen(&device, &frame).expect("warm frame");
        }
        let pipelines = pipelines_created();
        let pool = renderer.graph_report().graph.transients.created;
        for mode in [
            ProbeSpecular::LobeHue,
            ProbeSpecular::Lobe,
            ProbeSpecular::LobeHue,
        ] {
            renderer.set_ddgi_settings(DdgiSettings {
                specular: mode,
                ..renderer.ddgi_settings().clone()
            });
            for _ in 0..2 {
                renderer.render_offscreen(&device, &frame).expect("frame");
            }
        }
        assert_eq!(
            pipelines_created() - pipelines,
            0,
            "pipelines, plane {:?}",
            ground.is_some()
        );
        assert_eq!(
            renderer.graph_report().graph.transients.created,
            pool,
            "pooled targets, plane {:?}",
            ground.is_some()
        );
    }
}
