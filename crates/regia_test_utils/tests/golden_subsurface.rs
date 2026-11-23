// SPDX-License-Identifier: MPL-2.0
//! Golden for screen-space subsurface scattering (ADR-2049): a skin sphere and a leaf-tinted plate
//! under a low sun, with the blur on. The default frames keep their own goldens; this is the only
//! image the feature adds.
//!
//! Ignored until the lead blesses the image on the lavapipe reference. `REGIA_SUBSURFACE_EVIDENCE_DIR`
//! names a directory to leave the frames in, for a look by eye.
//!
//! ```text
//! REGIA_BLESS=1 cargo test -p regia_test_utils --test golden_subsurface -- --ignored
//! ```

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, PostSettings,
    RenderDevice, SubsurfaceFactors, SubsurfaceProfile, box_mesh, translate_mesh, uv_sphere,
};
use regia_test_utils::golden::{DEFAULT_SSIM_THRESHOLD, compare_rgba8_to_png, golden_path};
use regia_test_utils::scenes::{GOLDEN_HEIGHT, GOLDEN_WIDTH};

fn skin_plate() -> LabScene {
    let (sv, si) = uv_sphere(0.09, 128, 64);
    let (gv, gi) = box_mesh([0.4, 0.01, 0.4]);
    let (bv, bi) = box_mesh([0.03, 0.09, 0.03]);
    LabScene {
        width: GOLDEN_WIDTH,
        height: GOLDEN_HEIGHT,
        eye: [0.0, 0.14, 0.42],
        target: [0.0, 0.08, 0.0],
        fov_y: 0.6,
        near: 0.02,
        far: 10.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 13.0,
        ambient_lux: 1_500.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 13.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([-0.8, -0.45, -0.4], [1.0, 0.96, 0.9], 30_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.72, 0.53, 0.45, 1.0],
                roughness: 0.5,
                ior: 1.4,
                subsurface: SubsurfaceFactors::new(SubsurfaceProfile::Skin),
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.25, 0.5, 0.12, 1.0],
                roughness: 0.6,
                subsurface: SubsurfaceFactors {
                    radius_mm: 12.0,
                    ..SubsurfaceFactors::new(SubsurfaceProfile::Foliage)
                },
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.4, 0.4, 0.42, 1.0],
                roughness: 0.7,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![
            LabMesh {
                vertices: translate_mesh(&sv, [0.0, 0.1, 0.0]),
                indices: si,
                material: 0,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: translate_mesh(&gv, [0.0, -0.01, 0.0]),
                indices: gi,
                material: 1,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: translate_mesh(&bv, [0.17, 0.09, 0.05]),
                indices: bi,
                material: 2,
                ..LabMesh::unskinned()
            },
        ],
    }
}

fn render(device: &RenderDevice, scatter: bool) -> Vec<u8> {
    let lab = skin_plate();
    let quality = FrameQuality {
        subsurface_scattering: scatter,
        ..FrameQuality::LAB
    };
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    let pixels = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    assert_eq!(renderer.subsurface_stats().reason.is_none(), scatter);
    pixels
}

fn leave_evidence(name: &str, pixels: &[u8]) {
    if let Ok(dir) = std::env::var("REGIA_SUBSURFACE_EVIDENCE_DIR") {
        let path = std::path::Path::new(&dir).join(format!("{name}.png"));
        image::RgbaImage::from_raw(GOLDEN_WIDTH, GOLDEN_HEIGHT, pixels.to_vec())
            .expect("frame size")
            .save(&path)
            .expect("write evidence");
    }
}

#[test]
#[ignore = "the lead blesses this image first (book/agents/subsurface.md)"]
fn golden_subsurface_plate() {
    let device = RenderDevice::headless().expect("device");
    let on = render(&device, true);
    let off = render(&device, false);
    assert_ne!(on, off, "the blur must show");
    leave_evidence("subsurface_on", &on);
    leave_evidence("subsurface_off", &off);
    compare_rgba8_to_png(
        &on,
        GOLDEN_WIDTH,
        GOLDEN_HEIGHT,
        &golden_path("subsurface_plate"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .expect("golden compare");
}

/// The frames for a look by eye, compared with nothing.
#[test]
#[ignore = "only leaves the frames in REGIA_SUBSURFACE_EVIDENCE_DIR"]
fn subsurface_plate_evidence() {
    let device = RenderDevice::headless().expect("device");
    leave_evidence("subsurface_on", &render(&device, true));
    leave_evidence("subsurface_off", &render(&device, false));
}
