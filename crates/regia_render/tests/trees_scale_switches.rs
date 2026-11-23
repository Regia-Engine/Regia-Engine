// SPDX-License-Identifier: MPL-2.0
//! The forest switches of ADR-1415 on the GPU-driven instanced path (lavapipe, no goldens):
//!
//! - `FrameRenderer::set_ordered_gpu_cull`: every instance keeps the slot of its submission
//!   order, so the picture is the packed cull's picture (a scene without depth ties draws the
//!   same pixels) and does not depend on GPU thread order;
//! - `FrameRenderer::set_alpha_tested_shadows`: a masked card casts the shadow of its opaque
//!   texels only, where it cast its whole rectangle before.

use regia_render::{
    ALPHA_MASK, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    LabTexture, LabVertex, PostSettings, RenderDevice,
};

const W: u32 = 128;
const H: u32 = 96;
/// Shared mesh key of the fins: one GPU mesh, so the instanced path batches them.
const FIN_MESH: u64 = 0x7EE5_0F1D;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// Instancing needs `INDIRECT_FIRST_INSTANCE` + multi-draw indirect.
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

/// A quad in the XY plane through the origin, front face +Z.
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

/// A horizontal quad facing +Y; `u` runs along +X (0 at −X, 1 at +X).
fn flat(half: f32) -> LabMesh {
    let corners = [
        ([-half, 0.0, -half], [0.0, 0.0]),
        ([half, 0.0, -half], [1.0, 0.0]),
        ([half, 0.0, half], [1.0, 1.0]),
        ([-half, 0.0, half], [0.0, 1.0]),
    ];
    LabMesh {
        vertices: corners
            .iter()
            .map(|&(position, uv)| LabVertex::from_pos_normal_uv(position, [0.0, 1.0, 0.0], uv))
            .collect(),
        indices: vec![0, 2, 1, 0, 3, 2],
        ..LabMesh::unskinned()
    }
}

fn scene(eye: [f32; 3], target: [f32; 3], sun_travel: [f32; 3]) -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye,
        target,
        fov_y: 0.8,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new(sun_travel, [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.5, 0.5, 0.5, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        }],
        meshes: vec![flat(12.0)],
    }
}

fn rect_luma(px: &[u8], x0: f32, x1: f32, y0: f32, y1: f32) -> f64 {
    let (x0, x1) = ((x0 * W as f32) as u32, (x1 * W as f32) as u32);
    let (y0, y1) = ((y0 * H as f32) as u32, (y1 * H as f32) as u32);
    let (mut sum, mut n) = (0.0, 0.0);
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            sum += 0.2126 * f64::from(px[i]) + 0.7152 * f64::from(px[i + 1]) + 0.0722 * f64::from(px[i + 2]);
            n += 1.0;
        }
    }
    sum / n
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A 5 × 5 field of fins on a ground, seen at an angle from above: no two fins meet at one depth
/// on screen, so the instance order inside the fin batch cannot change a pixel.
#[test]
fn ordered_gpu_cull_draws_the_packed_cull_picture() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    if !gpu_driven(&device) {
        eprintln!("device has no indirect first-instance: no instanced batches to order");
        return;
    }
    let mut lab = scene([1.5, 5.0, -9.0], [0.0, 0.4, 0.0], [-0.3, -0.7, 0.6]);
    // Double-sided: the camera sees the fins' back faces (they face +Z, the eye is at -Z), which a culled
    // material would drop in both cull modes alike.
    lab.materials.push(LabMaterial {
        base_color: [0.7, 0.45, 0.3, 1.0],
        roughness: 0.6,
        double_sided: true,
        ..LabMaterial::default()
    });
    for i in 0..25 {
        let (x, z) = ((i % 5) as f32 * 1.4 - 2.8, (i / 5) as f32 * 1.4 - 2.8);
        lab.meshes.push(LabMesh {
            material: 1,
            model: translate(x, 0.45, z),
            ..fin(0.4)
        });
    }
    let render = |ordered: bool| {
        let mut frame = lab.as_frame_scene_with_quality(FrameQuality {
            gpu_driven: true,
            ..FrameQuality::LAB
        });
        for mesh in frame.meshes.iter_mut().skip(1) {
            mesh.mesh_key = FIN_MESH;
        }
        let mut renderer = FrameRenderer::new();
        renderer.set_hiz_occlusion(false);
        renderer.set_ordered_gpu_cull(ordered);
        let image = renderer.render_offscreen(&device, &frame).expect("frame");
        (image.pixels, renderer.draw_stats())
    };
    let (packed, packed_stats) = render(false);
    let (ordered, ordered_stats) = render(true);
    eprintln!("packed {packed_stats:?}\nordered {ordered_stats:?}");
    assert_eq!(
        ordered_stats.instanced, 26,
        "ground + 25 fins in instanced batches"
    );
    assert_eq!(ordered_stats.per_object, 0);
    let differing = packed
        .as_chunks::<4>()
        .0
        .iter()
        .zip(ordered.as_chunks::<4>().0)
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(
        differing, 0,
        "the order-fixed cull must draw the packed cull's pixels"
    );
    // The fins are drawn at all (not every slot culled): the frame is not just the ground.
    let fins = rect_luma(&ordered, 0.3, 0.7, 0.3, 0.7);
    let ground_only = {
        let frame = scene([1.5, 5.0, -9.0], [0.0, 0.4, 0.0], [-0.3, -0.7, 0.6]);
        let mut renderer = FrameRenderer::new();
        renderer.set_ordered_gpu_cull(true);
        let frame = frame.as_frame_scene_with_quality(FrameQuality {
            gpu_driven: true,
            ..FrameQuality::LAB
        });
        let image = renderer.render_offscreen(&device, &frame).expect("frame");
        rect_luma(&image.pixels, 0.3, 0.7, 0.3, 0.7)
    };
    assert!(
        (fins - ground_only).abs() > 2.0,
        "fins missing under the ordered cull: {fins:.1} vs ground alone {ground_only:.1}"
    );
}

/// An 8 × 8 white texture, opaque on the `u < 0.5` half and fully transparent on the other.
fn half_mask() -> LabTexture {
    let mut rgba = Vec::with_capacity(8 * 8 * 4);
    for _row in 0..8 {
        for col in 0..8 {
            rgba.extend_from_slice(&[255, 255, 255, if col < 4 { 255 } else { 0 }]);
        }
    }
    LabTexture {
        width: 8,
        height: 8,
        rgba,
    }
}

/// A horizontal masked card 3 m over the ground under a near-vertical sun, seen from below
/// its height (the card itself is above the frame): the ground under the card's opaque half is in shadow either way; under its cut
/// half it is lit with alpha-tested shadows and shadowed (the card's whole rectangle) without.
#[test]
fn alpha_tested_shadows_cast_only_the_opaque_half_of_a_masked_card() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    if !gpu_driven(&device) {
        eprintln!("device has no indirect first-instance: alpha-tested shadows are instanced-only");
        return;
    }
    let mut lab = scene([0.0, 1.5, -4.0], [0.0, 0.0, 0.0], [0.05, -1.0, 0.08]);
    lab.materials.push(LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        albedo: Some(half_mask()),
        alpha_mode: ALPHA_MASK,
        alpha_cutoff: 0.5,
        double_sided: true,
        ..LabMaterial::default()
    });
    lab.meshes.push(LabMesh {
        material: 1,
        model: translate(0.0, 3.0, 0.0),
        ..flat(1.5)
    });
    let render = |tested: bool| {
        let frame = lab.as_frame_scene_with_quality(FrameQuality {
            gpu_driven: true,
            ..FrameQuality::LAB
        });
        let mut renderer = FrameRenderer::new();
        renderer.set_alpha_tested_shadows(tested);
        let image = renderer.render_offscreen(&device, &frame).expect("frame");
        // Ground under the card, either side of the screen's centre line (world x = 0).
        let left = rect_luma(&image.pixels, 0.3, 0.45, 0.45, 0.62);
        let right = rect_luma(&image.pixels, 0.55, 0.7, 0.45, 0.62);
        (left, right)
    };
    let (off_l, off_r) = render(false);
    let (on_l, on_r) = render(true);
    eprintln!("whole-card shadow: {off_l:.1} | {off_r:.1}; alpha-tested: {on_l:.1} | {on_r:.1}");
    let (on_lit, on_dark) = (on_l.max(on_r), on_l.min(on_r));
    assert!(
        on_lit > 1.5 * on_dark + 2.0,
        "alpha-tested: one side of the ground must be lit through the cut half ({on_l:.1} | {on_r:.1})"
    );
    let (off_hi, off_lo) = (off_l.max(off_r), off_l.min(off_r));
    assert!(
        off_hi < 1.3 * off_lo + 2.0,
        "without the test the whole card shadows both sides alike ({off_l:.1} | {off_r:.1})"
    );
    // The lit side is the one the card shadowed before.
    let lit_was = if on_l > on_r { off_l } else { off_r };
    assert!(
        on_lit > 1.5 * lit_was + 2.0,
        "the cut half's ground is lit only with the alpha test ({lit_was:.1} -> {on_lit:.1})"
    );
}
