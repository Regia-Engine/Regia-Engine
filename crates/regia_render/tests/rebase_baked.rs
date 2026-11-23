// SPDX-License-Identifier: MPL-2.0
//! Spec 78 § 12.2, the GI row: a baked volume stays in the space it was baked in. After a
//! floating-origin rebase the local scene is kilometers from the volume's footprint, and the frame
//! must still take the same light. The control moves the scene without telling the renderer, so it
//! leaves the footprint and goes dark.

use std::sync::{Arc, OnceLock};

use regia_asset::baked_gi::{
    BakeInfo, BakeLights, BakedGiVolume, DetailBrick, ProbeData, ProbeFlags, ProbeMeta, SH_Y00, ShRgb,
    TerrainLayer, VolumeLayout,
};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, box_mesh, translate_mesh,
};

const W: u32 = 128;
const H: u32 = 96;
/// A rebase by (1, 0, -1) cells of 1024 m: exact in `f32`.
const SHIFT: [f32; 3] = [1024.0, 0.0, -1024.0];
const SKY: f32 = 3000.0;

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| RenderDevice::headless().expect("baked GI needs a GPU; lavapipe is a GPU here"))
}

fn uniform(l: f32) -> ShRgb {
    let k = 4.0 * std::f32::consts::PI * SH_Y00 * l;
    [[k, 0.0, 0.0, 0.0], [k, 0.0, 0.0, 0.0], [k, 0.0, 0.0, 0.0]]
}

/// 8 m columns over 32 m and a detail brick, every probe lit by `SKY`.
fn volume() -> Arc<BakedGiVolume> {
    let counts = [5u32, 5u32];
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [-16.0, -16.0],
            spacing: [8.0, 8.0],
            counts,
            levels: vec![1.0, 3.0, 6.0],
            ground: vec![0.0; (counts[0] * counts[1]) as usize],
        }),
        bricks: vec![DetailBrick {
            origin: [-4.5, -1.0, -2.5],
            spacing: [0.5; 3],
            counts: [11, 8, 11],
            first_probe: 0,
            flags: 0,
        }],
        moment_resolution: 4,
        max_distance: 16.0,
    };
    layout.assign_brick_offsets();
    let far = 16.0;
    let probes: Vec<ProbeData> = (0..layout.probe_count())
        .map(|_| ProbeData {
            sh: [uniform(0.0), uniform(SKY), uniform(0.0), uniform(0.0)],
            moments: vec![[far, far * far]; 16],
            meta: ProbeMeta {
                offset: [0.0; 3],
                flags: ProbeFlags::VALID,
            },
        })
        .collect();
    Arc::new(
        BakedGiVolume::from_probes(
            &layout,
            BakeLights {
                sky_scale: 1.0,
                ..BakeLights::default()
            },
            BakeInfo::default(),
            &probes,
        )
        .expect("synthetic volume"),
    )
}

/// The floor and a wall with the baked volume as the only light, everything moved by `-offset`.
fn frame(renderer: &mut FrameRenderer, offset: [f32; 3]) -> Vec<u8> {
    let at = |p: [f32; 3]| std::array::from_fn(|a| p[a] - offset[a]);
    let mesh = |centre: [f32; 3], half: [f32; 3]| {
        let (v, indices) = box_mesh(half);
        LabMesh {
            vertices: translate_mesh(&v, at(centre)),
            indices,
            material: 0,
            ..LabMesh::unskinned()
        }
    };
    let lab = LabScene {
        width: W,
        height: H,
        eye: at([0.0, 3.0, 7.0]),
        target: at([0.0, 0.3, -0.5]),
        fov_y: 0.9,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 13.5,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 13.5,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.45, 0.45, 0.45, 1.0],
            roughness: 0.95,
            ..LabMaterial::default()
        }],
        meshes: vec![
            mesh([0.0, -0.25, 0.0], [16.0, 0.25, 16.0]),
            mesh([-2.0, 1.0, -1.0], [1.5, 1.0, 0.15]),
        ],
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.render_scale_permille = 1000;
    let scene = lab.as_frame_scene_with_quality(quality);
    renderer
        .render_offscreen(device(), &scene)
        .expect("render")
        .pixels
}

fn mean_difference(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let sum: u64 = a
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .map(|(pa, pb)| (0..3).map(|c| u64::from(pa[c].abs_diff(pb[c]))).sum::<u64>())
        .sum();
    sum as f64 / ((a.len() / 4) as f64 * 3.0)
}

fn lit_renderer() -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_baked_gi(Some(volume()));
    renderer.set_baked_gi_enabled(Some(true));
    renderer
}

#[test]
fn a_rebase_keeps_the_baked_light() {
    let mut still = lit_renderer();
    let mut before = Vec::new();
    for _ in 0..3 {
        before = frame(&mut still, [0.0; 3]);
    }

    let mut moved = lit_renderer();
    frame(&mut moved, [0.0; 3]);
    moved.shift_history(SHIFT);
    let mut after = Vec::new();
    for _ in 0..2 {
        after = frame(&mut moved, SHIFT);
    }

    let mut control = lit_renderer();
    frame(&mut control, [0.0; 3]);
    let mut wrong = Vec::new();
    for _ in 0..2 {
        wrong = frame(&mut control, SHIFT);
    }

    let (kept, lost) = (mean_difference(&before, &after), mean_difference(&before, &wrong));
    eprintln!("baked light across a rebase: {kept:.3} levels; control {lost:.3}");
    assert!(lost > 8.0, "the control must leave the volume: {lost}");
    assert!(kept < 1.0, "{kept} levels");
}
