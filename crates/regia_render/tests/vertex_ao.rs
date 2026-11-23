// SPDX-License-Identifier: MPL-2.0
//! Per-vertex ambient occlusion on opaque lab materials (`LabMaterial::vertex_ao`, ADR-1512).
//!
//! A quad facing the camera carries a 0 → 1 occlusion ramp in its vertex color alpha. With the
//! flag on, the indirect light (IBL sky) ramps smoothly across the quad; with it off, the frame
//! is bit-identical to a quad without the ramp (the alpha stays an opacity that an opaque
//! material ignores); and a sun-only frame is the same with the flag on and off, because the
//! factor scales only the indirect light. No golden: the frames are compared with each other.

use regia_render::{
    DirectionalLightUniform, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex, PostSettings,
    RenderDevice, vertex_ao,
};

const W: u32 = 320;
const H: u32 = 240;

/// Serialises device creation and teardown across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A 2 × 2 m quad in the XY plane facing +Z; the occlusion ramps from `left` to `right`.
fn ramp_quad(left: f32, right: f32) -> LabMesh {
    let corner = |x: f32, y: f32, ao: f32| {
        let mut v = LabVertex::from_pos_normal_uv([x, y, 0.0], [0.0, 0.0, 1.0], [0.0; 2]);
        v.color[3] = ao;
        v
    };
    LabMesh {
        vertices: vec![
            corner(-1.0, -1.0, left),
            corner(1.0, -1.0, right),
            corner(1.0, 1.0, right),
            corner(-1.0, 1.0, left),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        material: 0,
        ..LabMesh::unskinned()
    }
}

fn scene(mesh: LabMesh, flag: bool, ibl: bool, sun_lux: f32) -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl,
        exposure_ev: 13.5,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 13.5,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0; 3], sun_lux),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.55, 0.55, 0.55, 1.0],
            roughness: 0.8,
            vertex_ao: flag,
            ..LabMaterial::default()
        }],
        meshes: vec![mesh],
    }
}

fn render(device: &RenderDevice, lab: &LabScene) -> Vec<u8> {
    let frame = lab.as_frame_scene();
    let mut renderer = FrameRenderer::new();
    renderer.render_offscreen(device, &frame).expect("frame").pixels
}

fn luma_at(px: &[u8], x: u32, y: u32) -> f64 {
    let i = ((y * W + x) * 4) as usize;
    0.2126 * f64::from(px[i]) + 0.7152 * f64::from(px[i + 1]) + 0.0722 * f64::from(px[i + 2])
}

#[test]
fn the_flag_ramps_the_indirect_light_smoothly_across_a_quad() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("headless device");
    let on = render(&device, &scene(ramp_quad(0.0, 1.0), true, true, 0.0));
    let flat = render(&device, &scene(ramp_quad(1.0, 1.0), true, true, 0.0));
    let y = H / 2;
    // The quad covers about columns 77..243; stay inside it.
    let row: Vec<f64> = (90..230).map(|x| luma_at(&on, x, y)).collect();
    let mut worst_step = 0.0f64;
    for pair in row.windows(2) {
        // Never darker toward the open side (allow one level of rounding).
        assert!(
            pair[1] + 1.0 >= pair[0],
            "ramp must not fall: {} then {}",
            pair[0],
            pair[1]
        );
        worst_step = worst_step.max(pair[1] - pair[0]);
    }
    assert!(
        worst_step <= 4.0,
        "a smooth ramp has no step; the largest was {worst_step}"
    );
    let (dark, lit) = (row[0], row[row.len() - 1]);
    assert!(
        lit - dark > 40.0,
        "left {dark} should be much darker than right {lit}"
    );
    // The open side (ao = 1) matches the same quad with no occlusion at all.
    let open_flat = luma_at(&flat, 229, y);
    assert!(
        (lit - open_flat).abs() <= 6.0,
        "ao 1 end {lit} vs unoccluded {open_flat}"
    );
}

#[test]
fn with_the_flag_off_the_ramp_changes_nothing() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("headless device");
    let ramp = render(&device, &scene(ramp_quad(0.0, 1.0), false, true, 40_000.0));
    let plain = render(&device, &scene(ramp_quad(1.0, 1.0), false, true, 40_000.0));
    assert_eq!(
        ramp, plain,
        "an opaque material ignores the vertex alpha when the flag is off"
    );
}

#[test]
fn direct_light_is_untouched_by_the_factor() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("headless device");
    // No IBL and no ambient: the sun is the only light.
    let on = render(&device, &scene(ramp_quad(0.0, 1.0), true, false, 60_000.0));
    let off = render(&device, &scene(ramp_quad(0.0, 1.0), false, false, 60_000.0));
    assert_eq!(on, off, "the occlusion factor scales the indirect light only");
}

#[test]
fn a_uniform_factor_orders_the_brightness_like_the_cpu_reference() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("headless device");
    let levels = [0.0f32, 0.25, 0.5, 1.0];
    let lumas: Vec<f64> = levels
        .iter()
        .map(|&ao| {
            luma_at(
                &render(&device, &scene(ramp_quad(ao, ao), true, true, 0.0)),
                160,
                H / 2,
            )
        })
        .collect();
    for (pair, ao) in lumas.windows(2).zip(levels.windows(2)) {
        assert!(
            pair[1] > pair[0],
            "ao {} -> {} must brighten: {pair:?}",
            ao[0],
            ao[1]
        );
        assert!(
            vertex_ao::scale_indirect([1.0; 3], ao[1])[0] > vertex_ao::scale_indirect([1.0; 3], ao[0])[0]
        );
    }
    // ao 0 removes the indirect light entirely: nothing else lights this frame.
    assert!(
        lumas[0] <= 2.0,
        "ao 0 with no direct light is black, got {}",
        lumas[0]
    );
}
