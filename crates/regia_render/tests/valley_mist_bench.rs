// SPDX-License-Identifier: MPL-2.0
//! Hand-run cost of the valley mist (ADR-1394): the froxel fog pass's GPU time with and
//! without the mist at 1440p and 4K, for the full tier (Desktop High) and the reduced tier
//! (Desktop Low). Prints median `fog_ms` of warm frames; asserts nothing about time (no
//! wall-clock budgets in tests). Run on the target card:
//!
//! `cargo test -p regia_render --release --test valley_mist_bench -- --ignored --nocapture`
//!
//! On lavapipe set `VK_ICD_FILENAMES` to its ICD file; on a real card pick it with
//! `REGIA_GPU` as the other benches do (never card 6 on the build server).

use std::sync::Arc;

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    MistHeightGrid, OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
    ValleyMist, VolumetricFogSettings,
};

const WARM: usize = 3;
const SAMPLES: usize = 9;

fn scene(width: u32, height: u32) -> LabScene {
    let ground = [
        LabVertex::from_pos_normal_uv([-300.0, 0.0, 20.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
        LabVertex::from_pos_normal_uv([300.0, 0.0, 20.0], [0.0, 1.0, 0.0], [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([300.0, 0.0, -300.0], [0.0, 1.0, 0.0], [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([-300.0, 0.0, -300.0], [0.0, 1.0, 0.0], [0.0, 1.0]),
    ];
    let sun_y = -(10.0_f32.to_radians()).sin();
    let sun_z = (10.0_f32.to_radians()).cos();
    LabScene {
        width,
        height,
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
        meshes: vec![LabMesh {
            vertices: ground.to_vec(),
            indices: vec![0, 2, 1, 0, 3, 2],
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

fn median(mut values: Vec<f32>) -> f32 {
    values.sort_by(f32::total_cmp);
    values[values.len() / 2]
}

#[test]
#[ignore = "hand-run cost measurement; needs the target GPU"]
fn the_valley_mist_fog_cost() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    eprintln!(
        "adapter {} ({:?}), timestamps {}",
        device.capabilities.adapter_name, device.capabilities.backend, device.capabilities.timestamp_query
    );
    let grid = MistHeightGrid::from_fn([-300.0, -300.0], 5.0, 121, 121, 200.0, |x, z| {
        let r = (x * x + (z + 70.0) * (z + 70.0)).sqrt();
        -18.0 * (1.0 - (r / 90.0).min(1.0)).powi(2)
    });
    let mist = ValleyMist {
        grid: grid.map(Arc::new),
        ..ValleyMist::default()
    };
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        bloom: false,
        ..PostSettings::golden_deterministic()
    };
    for (preset, tier) in [
        (RenderProfilePreset::DesktopHigh, "full"),
        (RenderProfilePreset::DesktopLow, "reduced"),
    ] {
        for (width, height) in [(2560_u32, 1440_u32), (3840, 2160)] {
            let quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
            let lab = scene(width, height);
            let mut frame = lab.as_frame_scene_with_quality(quality);
            frame.post = Some(&post);
            for (label, with_mist) in [("no mist", false), ("mist", true)] {
                let mut renderer = FrameRenderer::new();
                renderer.set_gpu_timing(&device, true);
                renderer.set_outdoor(OutdoorParams {
                    fog: VolumetricFogSettings {
                        density: 0.0001,
                        ..VolumetricFogSettings::default()
                    },
                    ..OutdoorParams::default()
                });
                renderer.set_valley_mist(with_mist.then(|| mist.clone()));
                let mut fog = Vec::new();
                let mut scene_ms = Vec::new();
                for i in 0..WARM + SAMPLES {
                    renderer.render_offscreen(&device, &frame).expect("bench frame");
                    if i >= WARM {
                        let gpu = renderer.gpu_timings();
                        fog.push(gpu.fog_ms);
                        scene_ms.push(gpu.scene_ms);
                    }
                }
                eprintln!(
                    "{tier:8} {width}x{height} {label:8} fog {:.3} ms (median of {SAMPLES}), scene {:.3} ms",
                    median(fog),
                    median(scene_ms)
                );
            }
        }
    }
}
