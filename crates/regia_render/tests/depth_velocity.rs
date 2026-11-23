// SPDX-License-Identifier: MPL-2.0
//! `RenderProfile::depth_velocity` (look-r2 round 9): static geometry's TAAU motion from the
//! depth buffer instead of a velocity draw per static mesh. While the camera moves, its frames
//! stay as close to a non-temporal reference as the per-mesh pass (closer: the per-mesh pass
//! interpolates motion linearly across large triangles, the depth pass is exact per pixel).

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, box_mesh, translate_mesh,
};

const W: u32 = 192;
const H: u32 = 108;

fn frames(
    device: &RenderDevice,
    preset: RenderProfilePreset,
    depth_velocity: bool,
    taau: bool,
) -> Vec<Vec<u8>> {
    let mut meshes = Vec::new();
    let (gv, gi) = box_mesh([20.0, 0.1, 20.0]);
    meshes.push(LabMesh {
        vertices: gv,
        indices: gi,
        material: 0,
        ..LabMesh::unskinned()
    });
    for (i, x) in [-4.0f32, -1.5, 1.0, 3.5].iter().enumerate() {
        let (v, idx) = box_mesh([0.5, 0.5 + i as f32 * 0.3, 0.5]);
        meshes.push(LabMesh {
            vertices: translate_mesh(&v, [*x, 0.6 + i as f32 * 0.3, -2.0 + i as f32]),
            indices: idx,
            material: 1,
            ..LabMesh::unskinned()
        });
    }
    let mut quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
    quality.depth_velocity = depth_velocity;
    quality.taau = taau;
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        ..PostSettings::default()
    };
    let mut renderer = FrameRenderer::new();
    // Motion vectors only: the irradiance probes (Desktop High's Depth default) re-converge
    // as the camera moves, which changes the indirect light frame to frame independently of
    // the velocity under test.
    renderer.set_ddgi_enabled(Some(false));
    let mut out = Vec::new();
    for f in 0..6 {
        let lab = LabScene {
            width: W,
            height: H,
            eye: [-3.0 + f as f32 * 0.4, 2.5, 7.0],
            target: [0.0, 0.5, 0.0],
            fov_y: 0.9,
            near: 0.1,
            far: 200.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: true,
            exposure_ev: 14.0,
            ambient_lux: 8_000.0,
            double_sided: false,
            post: Some(post.clone()),
            directional: DirectionalLightUniform::new([-0.4, -0.8, -0.3], [1.0; 3], 60_000.0),
            local_lights: Vec::new(),
            materials: vec![
                LabMaterial {
                    base_color: [0.4, 0.4, 0.4, 1.0],
                    ..LabMaterial::default()
                },
                LabMaterial {
                    base_color: [0.7, 0.3, 0.2, 1.0],
                    ..LabMaterial::default()
                },
            ],
            meshes: meshes.clone(),
        };
        let frame = lab.as_frame_scene_with_quality(quality);
        out.push(renderer.render_offscreen(device, &frame).expect("frame").pixels);
    }
    out
}

fn error(a: &[u8], reference: &[u8]) -> f64 {
    a.iter()
        .zip(reference)
        .map(|(x, y)| f64::from(x.abs_diff(*y)))
        .sum::<f64>()
        / a.len() as f64
}

#[test]
fn depth_velocity_tracks_the_moving_camera_at_least_as_well_as_per_mesh_velocity() {
    let device = RenderDevice::headless().expect("lavapipe");
    // Deck (where it shipped first, round 9) and Desktop High (on since round 10).
    for preset in [RenderProfilePreset::Deck, RenderProfilePreset::DesktopHigh] {
        let drawn = frames(&device, preset, false, true);
        let depth = frames(&device, preset, true, true);
        let reference = frames(&device, preset, false, false);
        for f in 1..drawn.len() {
            let (e_drawn, e_depth) = (error(&drawn[f], &reference[f]), error(&depth[f], &reference[f]));
            eprintln!(
                "{preset:?} frame {f}: mean error vs non-temporal — per-mesh {e_drawn:.3}, depth {e_depth:.3}"
            );
            assert!(
                e_depth <= e_drawn * 1.05 + 0.05,
                "{preset:?} frame {f}: depth velocity {e_depth:.3} vs per-mesh {e_drawn:.3}"
            );
        }
    }
}
