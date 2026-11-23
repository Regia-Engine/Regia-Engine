// SPDX-License-Identifier: MPL-2.0
//! Before/after frame identity across cluster growth and DI history resizing (ADR-8065).
//! Uses only pre-existing public APIs so the same test can run on the integration base.

fn scene() -> regia_render::LabScene {
    let (vertices, indices) = regia_render::box_mesh([1.0; 3]);
    regia_render::LabScene {
        width: 48,
        height: 32,
        eye: [0.0, 2.0, -6.0],
        target: [0.0; 3],
        fov_y: 1.0,
        near: 0.1,
        far: 30.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 4.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(regia_render::PostSettings {
            exposure_ev: 4.0,
            ..regia_render::PostSettings::golden_deterministic()
        }),
        directional: regia_render::DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: vec![regia_render::LabLocalLight::Point(regia_render::LabPointLight {
            position: [0.0, 3.0, -3.0],
            color: [1.0; 3],
            intensity: 400.0,
            radius: 12.0,
            cast_shadows: false,
            source_radius: None,
        })],
        materials: vec![regia_render::LabMaterial::default()],
        meshes: vec![regia_render::LabMesh {
            vertices,
            indices,
            material: 0,
            ..regia_render::LabMesh::unskinned()
        }],
    }
}

#[test]
#[ignore = "compare printed frame hashes between the integration base and lane head"]
fn graph_buffer_frame_hashes() {
    let device = regia_render::RenderDevice::headless().expect("lavapipe device required");
    let mut renderer = regia_render::FrameRenderer::new();
    let mut lab = scene();
    for step in 0..12 {
        if step == 4 {
            lab.width = 64;
            lab.height = 40;
        }
        if step == 6 {
            lab.eye[0] += 0.4;
        }
        let quality = regia_render::FrameQuality {
            many_light_sampling: step != 8 && step != 9,
            max_lights_per_cluster: if step < 3 { 16 } else { 256 },
            ..regia_render::FrameQuality::LAB
        };
        let image = renderer
            .render_offscreen(&device, &lab.as_frame_scene_with_quality(quality))
            .expect("frame");
        if quality.many_light_sampling {
            assert!(renderer.many_light_report().active);
        }
        let (pixels, _) = image.pixels.as_chunks::<4>();
        assert!(
            pixels.iter().any(|p| p[0].max(p[1]).max(p[2]) > 8),
            "the probe must shade a lit surface"
        );
        assert!(
            pixels.windows(2).any(|pair| pair[0] != pair[1]),
            "the probe must not be a flat image"
        );
        let hash = image.pixels.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
        });
        println!("graph_buffer_frame {step:2} {hash:016x}");
    }
}
