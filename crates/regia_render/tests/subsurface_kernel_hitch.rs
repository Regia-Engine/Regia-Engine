// SPDX-License-Identifier: MPL-2.0
//! Switching the subsurface kernel creates nothing (ADR-3170). Alone in its binary: the pipeline
//! counter is process-wide, so another test's renderer would count here.
//!
//! (Plate helpers as in `subsurface_kernel_frame.rs`.)
//!
//! A plate tilted away from the view by 0, 45 and 70 degrees carries a perfectly hard light edge
//! across its slope (its upper half's normals face the sun, its lower half's face away, on one
//! flat, depth-continuous surface). Each row of the frame is a known distance along the surface
//! from the edge, so the frame compares with the radial profile's half-plane response
//! ([`regia_render::subsurface::edge_response`]) measured on the surface. The screen kernel blurs
//! a slanted plate over more of itself than the profile says; the tangent-plane kernel does not.

use regia_core::glam::Vec3;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    PostSettings, RenderDevice, SubsurfaceFactors, SubsurfaceKernel, SubsurfaceProfile,
};

const WIDTH: u32 = 160;
const HEIGHT: u32 = 160;
const EXTENT: f32 = 1.0;
const DISTANCE: f32 = 0.06;
const FOV_Y: f32 = 0.5;

fn device() -> RenderDevice {
    RenderDevice::headless().expect("headless device")
}

/// The plate's normal and its in-plane direction up the slope, tilted `degrees` about x.
fn frame_of(degrees: f32) -> (Vec3, Vec3) {
    let theta = degrees.to_radians();
    (
        Vec3::new(0.0, theta.sin(), theta.cos()),
        Vec3::new(0.0, theta.cos(), -theta.sin()),
    )
}

/// The half of the plate between `s0` and `s1` up the slope, its normal `normal`.
fn half(degrees: f32, s0: f32, s1: f32, normal: Vec3, material: usize) -> LabMesh {
    let (_, up) = frame_of(degrees);
    let corner = |x: f32, s: f32| {
        let p = Vec3::X * x + up * s;
        LabVertex::from_pos_normal_uv(p.to_array(), normal.to_array(), [0.5 + 0.5 * x, 0.5 + 0.5 * s])
    };
    LabMesh {
        vertices: vec![
            corner(-EXTENT, s0),
            corner(EXTENT, s0),
            corner(EXTENT, s1),
            corner(-EXTENT, s1),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

fn matte(subsurface: SubsurfaceFactors) -> LabMaterial {
    LabMaterial {
        base_color: [0.8, 0.6, 0.5, 1.0],
        roughness: 1.0,
        ior: 1.001,
        subsurface,
        ..LabMaterial::default()
    }
}

fn plate(degrees: f32, materials: Vec<LabMaterial>, extra: Vec<LabMesh>) -> LabScene {
    let (normal, _) = frame_of(degrees);
    let mut meshes = vec![
        half(degrees, 0.0, EXTENT, normal, 0),
        half(degrees, -EXTENT, 0.0, -normal, 0),
    ];
    meshes.extend(extra);
    LabScene {
        width: WIDTH,
        height: HEIGHT,
        eye: [0.0, 0.0, DISTANCE],
        target: [0.0, 0.0, 0.0],
        fov_y: FOV_Y,
        near: 0.005,
        far: 10.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 13.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            taa: false,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new((-normal).to_array(), [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

fn quality(scatter: bool, kernel: SubsurfaceKernel) -> FrameQuality {
    FrameQuality {
        subsurface_scattering: scatter,
        subsurface_kernel: kernel,
        specular_aa: false,
        ..FrameQuality::LAB
    }
}

/// Switching the kernel is a uniform: it creates no pipeline and no pooled target.
#[test]
fn switching_the_kernel_creates_nothing() {
    let device = device();
    let lab = plate(
        45.0,
        vec![matte(SubsurfaceFactors::new(SubsurfaceProfile::Skin))],
        Vec::new(),
    );
    let mut renderer = FrameRenderer::new();
    for _ in 0..2 {
        renderer
            .render_offscreen(
                &device,
                &lab.as_frame_scene_with_quality(quality(true, SubsurfaceKernel::Screen)),
            )
            .expect("frame");
    }
    let pipelines = regia_render::pipeline_cache::pipelines_created();
    let pool = renderer.graph_report().graph.transients.created;
    for kernel in [
        SubsurfaceKernel::Tangent,
        SubsurfaceKernel::Screen,
        SubsurfaceKernel::Tangent,
    ] {
        renderer
            .render_offscreen(&device, &lab.as_frame_scene_with_quality(quality(true, kernel)))
            .expect("frame");
    }
    assert_eq!(
        regia_render::pipeline_cache::pipelines_created() - pipelines,
        0,
        "pipelines"
    );
    assert_eq!(
        renderer.graph_report().graph.transients.created,
        pool,
        "pooled targets"
    );
}
