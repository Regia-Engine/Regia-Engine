// SPDX-License-Identifier: MPL-2.0
//! Per-instance render overrides in the frame renderer (Tincal engine request F5).
//!
//! Four fins share one mesh and one material. A tint rides the instance's object data, so the
//! tinted fin stays in the GPU-driven instanced batch, and the instanced and per-object paths
//! draw the same picture. A ghost moves its instance to the blend pass. Only the targeted fin
//! changes, clearing the overrides restores the frame exactly, and steady or re-tinted frames
//! create no GPU buffers, bind groups or pipelines.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    PostSettings, RenderDevice, RenderOverride, frame_path_counters,
};

const W: u32 = 192;
const H: u32 = 128;
const FIN_MESH: u64 = 0xF1D0_0B1F;
/// Fin centres on X (the camera looks down −Z at x = 0).
const FIN_X: [f32; 4] = [-2.4, -0.8, 0.8, 2.4];

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn gpu_driven(device: &RenderDevice) -> bool {
    device.capabilities.multi_draw_indirect && device.capabilities.indirect_first_instance
}

fn translate(x: f32, y: f32, z: f32) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [x, y, z, 1.0],
    ]
}

/// A quad in the XY plane, front face +Z.
fn fin(half: f32) -> LabMesh {
    let corners = [
        ([-half, -half, 0.0], [0.0, 1.0]),
        ([half, -half, 0.0], [1.0, 1.0]),
        ([half, half, 0.0], [1.0, 0.0]),
        ([-half, half, 0.0], [0.0, 0.0]),
    ];
    LabMesh {
        vertices: corners
            .iter()
            .map(|&(position, uv)| LabVertex::from_pos_normal_uv(position, [0.0, 0.0, 1.0], uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        ..LabMesh::unskinned()
    }
}

fn scene() -> LabScene {
    let mut scene = LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.6, 6.0],
        target: [0.0, 0.6, 0.0],
        fov_y: 0.8,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 2_000.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.2, -0.4, -1.0], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.6, 0.6, 0.6, 1.0],
            roughness: 0.7,
            ..LabMaterial::default()
        }],
        meshes: Vec::new(),
    };
    for x in FIN_X {
        scene.meshes.push(LabMesh {
            model: translate(x, 0.6, 0.0),
            ..fin(0.6)
        });
    }
    scene
}

/// Pixel column band of fin `i` (its projected half width, inset by two pixels).
fn band(i: usize) -> std::ops::Range<u32> {
    let half_w = 6.0 * 0.4_f32.tan() * W as f32 / H as f32;
    let centre = (FIN_X[i] / half_w + 1.0) * 0.5 * W as f32;
    let half = 0.6 / half_w * 0.5 * W as f32;
    (centre - half + 2.0) as u32..(centre + half - 2.0) as u32
}

/// Pixels differing by more than 1/255 inside and outside fin `i`'s column band (with a
/// two-pixel margin outside it for edge filtering).
fn diff_by_band(a: &[u8], b: &[u8], i: usize) -> (usize, usize) {
    let inside = band(i);
    let outside = inside.start.saturating_sub(4)..inside.end + 4;
    let (mut ins, mut out) = (0, 0);
    for (n, (pa, pb)) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0).enumerate() {
        let d = pa.iter().zip(pb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        if d <= 1 {
            continue;
        }
        let x = n as u32 % W;
        if inside.contains(&x) {
            ins += 1;
        } else if !outside.contains(&x) {
            out += 1;
        }
    }
    (ins, out)
}

/// Mean RGB inside fin `i`'s band at mid height.
fn fin_rgb(px: &[u8], i: usize) -> [f64; 3] {
    let (mut sum, mut n) = ([0.0; 3], 0.0);
    for y in H / 2 - 4..H / 2 + 4 {
        for x in band(i) {
            let k = ((y * W + x) * 4) as usize;
            for c in 0..3 {
                sum[c] += f64::from(px[k + c]);
            }
            n += 1.0;
        }
    }
    sum.map(|s| s / n)
}

#[test]
fn tints_stay_instanced_ghosts_blend_and_clearing_restores() {
    let device = device();
    let lab = scene();
    let gpu = gpu_driven(&device);
    let mut frame = lab.as_frame_scene_with_quality(FrameQuality {
        gpu_driven: true,
        ..FrameQuality::LAB
    });
    for mesh in &mut frame.meshes {
        mesh.mesh_key = FIN_MESH;
    }
    let keys: Vec<u64> = frame.meshes.iter().map(|m| m.object_key).collect();
    let mut renderer = FrameRenderer::new();
    let render = |renderer: &mut FrameRenderer| {
        let image = renderer.render_offscreen(&device, &frame).expect("frame").pixels;
        (image, renderer.draw_stats())
    };
    let (base, base_stats) = render(&mut renderer);
    let _ = render(&mut renderer);
    eprintln!("gpu-driven device: {gpu}; base {base_stats:?}");

    // Red tint on fin 1.
    renderer.set_render_overrides([(keys[1], RenderOverride::tinted([1.0, 0.1, 0.1, 1.0]))]);
    assert_eq!(renderer.render_override_count(), 1);
    let (red, red_stats) = render(&mut renderer);
    let (inside, outside) = diff_by_band(&base, &red, 1);
    assert!(inside > 50, "fin 1 changed ({inside} px)");
    assert_eq!(outside, 0, "nothing else changed");
    let [r, g, b] = fin_rgb(&red, 1);
    assert!(r > g + 30.0 && r > b + 30.0, "fin 1 is red: {r:.0} {g:.0} {b:.0}");
    assert_eq!(fin_rgb(&red, 2), fin_rgb(&base, 2));
    if gpu {
        assert_eq!(
            red_stats.instanced, base_stats.instanced,
            "a tint keeps the instance in its batch"
        );
        assert_eq!(red_stats.per_object, 0, "{red_stats:?}");
    }

    // Steady tinted and re-tinted frames create nothing.
    let before = frame_path_counters();
    let _ = render(&mut renderer);
    renderer.set_render_overrides([(keys[1], RenderOverride::tinted([0.1, 1.0, 0.1, 1.0]))]);
    let (green, _) = render(&mut renderer);
    let spent = frame_path_counters().since(before);
    eprintln!("steady + re-tint frames: {spent:?}");
    assert_eq!(spent.buffers, 0);
    assert_eq!(spent.bind_groups, 0);
    assert_eq!(spent.pipelines, 0);
    let [r, g, _] = fin_rgb(&green, 1);
    assert!(g > r + 30.0, "re-tinted green: {r:.0} {g:.0}");

    // The per-object path draws the same tinted picture.
    let mut cpu_frame = lab.as_frame_scene_with_quality(FrameQuality::LAB);
    for mesh in &mut cpu_frame.meshes {
        mesh.mesh_key = FIN_MESH;
    }
    let mut cpu_renderer = FrameRenderer::new();
    cpu_renderer.set_render_overrides([(keys[1], RenderOverride::tinted([0.1, 1.0, 0.1, 1.0]))]);
    let cpu = cpu_renderer
        .render_offscreen(&device, &cpu_frame)
        .expect("frame")
        .pixels;
    let max = cpu
        .iter()
        .zip(&green)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    assert!(max <= 1, "instanced and per-object tints agree (max {max}/255)");

    // Ghost on fin 2: blended over the backdrop, out of the instanced batch.
    renderer.set_render_overrides([(keys[2], RenderOverride::ghost(0.3))]);
    let (ghost, ghost_stats) = render(&mut renderer);
    let (inside, outside) = diff_by_band(&base, &ghost, 2);
    assert!(inside > 50, "fin 2 ghosted ({inside} px)");
    assert_eq!(outside, 0, "nothing else changed");
    // The backdrop (no IBL, no sky mesh) is darker than the lit fin: the ghost is dimmer.
    let lum = |c: [f64; 3]| c[0] + c[1] + c[2];
    assert!(
        lum(fin_rgb(&ghost, 2)) < lum(fin_rgb(&base, 2)) - 10.0,
        "ghost {:?} vs solid {:?}",
        fin_rgb(&ghost, 2),
        fin_rgb(&base, 2)
    );
    if gpu {
        // `DrawStats` counts the opaque draws: the ghost left the batch for the blend pass.
        assert_eq!(
            ghost_stats.instanced,
            base_stats.instanced - 1,
            "the ghost leaves the instanced batch: {ghost_stats:?}"
        );
    }

    // Clearing restores the frame exactly.
    renderer.set_render_overrides(std::iter::empty());
    assert_eq!(renderer.render_override_count(), 0);
    let (restored, restored_stats) = render(&mut renderer);
    assert_eq!(restored, base, "clearing the overrides restores the frame");
    assert_eq!(restored_stats.instanced, base_stats.instanced);
    // Identity overrides are dropped.
    renderer.set_render_overrides([(keys[0], RenderOverride::default())]);
    assert_eq!(renderer.render_override_count(), 0);
}
