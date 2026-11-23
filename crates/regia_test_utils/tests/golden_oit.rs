// SPDX-License-Identifier: MPL-2.0
//! Golden for order-independent transparency (ADR-2065): translucent plates through each other and a
//! sphere inside a translucent shell, over a lit floor, with the pass on. The default frames keep
//! their own goldens; this is the only image the feature adds.
//!
//! Ignored until the lead blesses the image on the lavapipe reference. `REGIA_OIT_EVIDENCE_DIR` names a
//! directory to leave the frames in, for a look by eye.
//!
//! ```text
//! REGIA_BLESS=1 cargo test -p regia_test_utils --test golden_oit -- --ignored
//! ```

use regia_render::{
    ALPHA_BLEND, ALPHA_PREMULTIPLIED, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial,
    LabMesh, LabScene, LabVertex, PostSettings, RenderDevice, box_mesh, translate_mesh, uv_sphere,
};
use regia_test_utils::golden::{DEFAULT_SSIM_THRESHOLD, compare_rgba8_to_png, golden_path};
use regia_test_utils::scenes::{GOLDEN_HEIGHT, GOLDEN_WIDTH};

/// A vertical plate through the origin, turned so its z moves by `slope` per unit of x.
fn plate(slope: f32, material: usize) -> LabMesh {
    let n = [-slope, 0.0, 1.0];
    let corner = |x: f32, y: f32| LabVertex::from_pos_normal_uv([x, y, slope * x], n, [0.0, 0.0]);
    LabMesh {
        vertices: vec![
            corner(-1.0, 0.0),
            corner(1.0, 0.0),
            corner(1.0, 1.4),
            corner(-1.0, 1.4),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

fn scene() -> LabScene {
    let (gv, gi) = box_mesh([3.0, 0.05, 3.0]);
    let (inner_v, inner_i) = uv_sphere(0.22, 48, 24);
    let (shell_v, shell_i) = uv_sphere(0.4, 64, 32);
    let translucent = |color: [f32; 3], alpha: f32, mode: u32| LabMaterial {
        base_color: [color[0], color[1], color[2], alpha],
        alpha_mode: mode,
        roughness: 0.4,
        double_sided: true,
        ..LabMaterial::default()
    };
    LabScene {
        width: GOLDEN_WIDTH,
        height: GOLDEN_HEIGHT,
        eye: [0.0, 1.1, 3.6],
        target: [0.0, 0.6, 0.0],
        fov_y: 0.6,
        near: 0.05,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 13.0,
        ambient_lux: 4_000.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 13.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([-0.5, -0.8, -0.4], [1.0, 0.97, 0.92], 30_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.5, 0.5, 0.52, 1.0],
                roughness: 0.8,
                ..LabMaterial::default()
            },
            translucent([0.9, 0.15, 0.1], 0.55, ALPHA_BLEND),
            translucent([0.1, 0.6, 0.9], 0.55, ALPHA_BLEND),
            LabMaterial {
                base_color: [0.9, 0.75, 0.2, 1.0],
                roughness: 0.3,
                ..LabMaterial::default()
            },
            translucent([0.1, 0.3, 0.15], 0.35, ALPHA_PREMULTIPLIED),
        ],
        meshes: vec![
            LabMesh {
                vertices: translate_mesh(&gv, [0.0, -0.05, 0.0]),
                indices: gi,
                material: 0,
                ..LabMesh::unskinned()
            },
            plate(0.45, 1),
            plate(-0.45, 2),
            LabMesh {
                vertices: translate_mesh(&inner_v, [1.2, 0.45, 0.4]),
                indices: inner_i,
                material: 3,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: translate_mesh(&shell_v, [1.2, 0.45, 0.4]),
                indices: shell_i,
                material: 4,
                ..LabMesh::unskinned()
            },
        ],
    }
}

fn render(device: &RenderDevice, oit: bool) -> Vec<u8> {
    let lab = scene();
    let quality = FrameQuality {
        order_independent_transparency: oit,
        ..FrameQuality::LAB
    };
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    let pixels = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    assert_eq!(renderer.oit_stats().reason.is_none(), oit);
    pixels
}

fn leave_evidence(name: &str, pixels: &[u8]) {
    if let Ok(dir) = std::env::var("REGIA_OIT_EVIDENCE_DIR") {
        let path = std::path::Path::new(&dir).join(format!("{name}.png"));
        image::RgbaImage::from_raw(GOLDEN_WIDTH, GOLDEN_HEIGHT, pixels.to_vec())
            .expect("frame size")
            .save(&path)
            .expect("write evidence");
    }
}

/// The frame with the pass on, against its reference. Blessed by the lead, not by this lane.
#[test]
#[ignore = "no reference yet: the lead blesses it on the lavapipe reference after an owner look"]
fn oit_plates_match_their_golden() {
    let device = RenderDevice::headless().expect("headless device");
    let on = render(&device, true);
    let off = render(&device, false);
    leave_evidence("oit_on", &on);
    leave_evidence("oit_off", &off);
    assert!(on != off, "the crossing plates must show the difference");
    compare_rgba8_to_png(
        &on,
        GOLDEN_WIDTH,
        GOLDEN_HEIGHT,
        &golden_path("oit_plates"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .expect("the frame matches its golden");
}

/// The frames for a look by eye, compared with nothing.
#[test]
#[ignore = "only leaves the frames in REGIA_OIT_EVIDENCE_DIR"]
fn oit_plates_evidence() {
    let device = RenderDevice::headless().expect("headless device");
    leave_evidence("oit_on", &render(&device, true));
    leave_evidence("oit_off", &render(&device, false));
}
