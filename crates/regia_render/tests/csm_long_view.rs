// SPDX-License-Identifier: MPL-2.0
//! A thin caster near the camera keeps its sun shadow when the far plane is kilometres out.
//!
//! DO27's tunnel plate renders with a 12 km far plane; the practical split over the whole
//! range ended cascade 0 about 750 m out, so a gate truss covered less than a texel and cast
//! nothing. The cascades now span at most 4 km and their splits are capped on a 16 m – 300 m
//! ladder (`csm_boundaries`), and the light eye reaches casters 2 km up-sun.

use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabVertex, RenderDevice, ShadowCast,
};

fn vertex(position: [f32; 3], normal: [f32; 3]) -> LabVertex {
    LabVertex {
        position,
        normal,
        uv: [0.0, 0.0],
        tangent: [1.0, 0.0, 0.0, 1.0],
        color: [1.0; 4],
        joints: [0; 4],
        weights: [0.0; 4],
    }
}

/// Axis-aligned box as 12 triangles (flat normals per face).
fn cuboid(min: [f32; 3], max: [f32; 3]) -> (Vec<LabVertex>, Vec<u32>) {
    let [x0, y0, z0] = min;
    let [x1, y1, z1] = max;
    let faces: [([f32; 3], [[f32; 3]; 4]); 6] = [
        (
            [0.0, 1.0, 0.0],
            [[x0, y1, z0], [x0, y1, z1], [x1, y1, z1], [x1, y1, z0]],
        ),
        (
            [0.0, -1.0, 0.0],
            [[x0, y0, z0], [x1, y0, z0], [x1, y0, z1], [x0, y0, z1]],
        ),
        (
            [1.0, 0.0, 0.0],
            [[x1, y0, z0], [x1, y1, z0], [x1, y1, z1], [x1, y0, z1]],
        ),
        (
            [-1.0, 0.0, 0.0],
            [[x0, y0, z0], [x0, y0, z1], [x0, y1, z1], [x0, y1, z0]],
        ),
        (
            [0.0, 0.0, 1.0],
            [[x0, y0, z1], [x1, y0, z1], [x1, y1, z1], [x0, y1, z1]],
        ),
        (
            [0.0, 0.0, -1.0],
            [[x0, y0, z0], [x0, y1, z0], [x1, y1, z0], [x1, y0, z0]],
        ),
    ];
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for (normal, corners) in faces {
        let base = vertices.len() as u32;
        vertices.extend(corners.iter().map(|&p| vertex(p, normal)));
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (vertices, indices)
}

/// Fraction of ground pixels the bar darkens by more than a third.
fn shadowed_fraction(far: f32) -> f32 {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    // Ground 60 × 60 m, top at y = 0.
    let (ground_v, ground_i) = cuboid([-30.0, -0.2, -30.0], [30.0, 0.0, 30.0]);
    // A 0.25 m truss member 2 m up, 8 m in front of the camera.
    let (bar_v, bar_i) = cuboid([-3.0, 1.9, -0.125], [3.0, 2.15, 0.125]);
    let material = LabMaterial {
        base_color: [0.6, 0.5, 0.4, 1.0],
        roughness: 0.8,
        ..LabMaterial::EMPTY
    };
    let quality = FrameQuality {
        csm_cascade_count: 4,
        csm_resolution: 2048,
        ..FrameQuality::LAB
    };
    let render = |bar_shadow: ShadowCast| {
        let scene = FrameScene {
            width: 160,
            height: 120,
            eye: [0.0, 5.0, 8.0],
            target: [0.0, 0.0, -2.0],
            fov_y: 0.9,
            near: 0.1,
            far,
            depth_prepass: false,
            directional_shadows: true,
            ibl: false,
            exposure_ev: 0.0,
            ambient_lux: 0.1,
            double_sided: true,
            post: None,
            fog: FogSettings::default(),
            // Sun high and behind the bar, so its shadow lands on open ground in view.
            directional: DirectionalLightUniform::new([0.15, -1.0, -0.6], [1.0, 1.0, 1.0], 1.0),
            local_lights: &[],
            quality,
            meshes: vec![
                FrameMesh {
                    is_static: false,
                    mesh_key: 1,
                    material_key: 1,
                    object_key: 1,
                    vertices: &ground_v,
                    indices: &ground_i,
                    material: &material,
                    model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                    joints: &[],
                    receiver_layer: 0,
                    shadow: ShadowCast::All,
                    uv1: &[],
                    lod_dither: 0.0,
                },
                FrameMesh {
                    is_static: false,
                    mesh_key: 2,
                    material_key: 1,
                    object_key: 2,
                    vertices: &bar_v,
                    indices: &bar_i,
                    material: &material,
                    model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                    joints: &[],
                    receiver_layer: 0,
                    shadow: bar_shadow,
                    uv1: &[],
                    lod_dither: 0.0,
                },
            ],
        };
        FrameRenderer::new()
            .render_offscreen(&device, &scene)
            .expect("frame")
            .pixels
    };
    let lit = render(ShadowCast::Off);
    let shadowed = render(ShadowCast::All);
    let luma = |p: &[u8]| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]);
    let mut dark = 0usize;
    for (a, b) in lit.as_chunks::<4>().0.iter().zip(shadowed.as_chunks::<4>().0) {
        let (la, lb) = (luma(a), luma(b));
        if la > 10.0 && lb < la * 0.67 {
            dark += 1;
        }
    }
    dark as f32 / (lit.len() / 4) as f32
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_thin_caster_keeps_its_shadow_with_a_12_km_far_plane() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let near_frame = shadowed_fraction(60.0);
    let vista = shadowed_fraction(12_000.0);
    eprintln!("shadowed fraction: far 60 m {near_frame:.4}, far 12 km {vista:.4}");
    assert!(
        near_frame > 0.004,
        "the bar must shadow the ground at all: {near_frame}"
    );
    assert!(
        vista > near_frame * 0.6,
        "a 12 km far plane must not erase a near shadow: {vista} vs {near_frame} at 60 m"
    );
}

/// Mean luma of the frame with a wall 150 m behind the camera, toward a low sun, either
/// casting or not.
fn far_caster_frame(far: f32, wall_shadow: ShadowCast) -> f32 {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let (ground_v, ground_i) = cuboid([-200.0, -0.2, -200.0], [200.0, 0.0, 200.0]);
    // 200 m wide, 80 m tall, 150 m up-sun: its shadow reaches 50 m past the camera.
    let (wall_v, wall_i) = cuboid([-100.0, 0.0, 150.0], [100.0, 80.0, 160.0]);
    let material = LabMaterial {
        base_color: [0.6, 0.5, 0.4, 1.0],
        roughness: 0.8,
        ..LabMaterial::EMPTY
    };
    let quality = FrameQuality {
        csm_cascade_count: 4,
        csm_resolution: 2048,
        ..FrameQuality::LAB
    };
    let scene = FrameScene {
        width: 96,
        height: 64,
        eye: [0.0, 3.0, 8.0],
        target: [0.0, 0.0, -4.0],
        fov_y: 0.9,
        near: 0.1,
        far,
        depth_prepass: false,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.1,
        double_sided: true,
        post: None,
        fog: FogSettings::default(),
        // Low sun from +z, behind the camera: light travels toward -z.
        directional: DirectionalLightUniform::new([0.05, -0.4, -1.0], [1.0, 1.0, 1.0], 1.0),
        local_lights: &[],
        quality,
        meshes: vec![
            FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 1,
                object_key: 1,
                vertices: &ground_v,
                indices: &ground_i,
                material: &material,
                model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            },
            FrameMesh {
                is_static: false,
                mesh_key: 2,
                material_key: 1,
                object_key: 2,
                vertices: &wall_v,
                indices: &wall_i,
                material: &material,
                model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: wall_shadow,
                uv1: &[],
                lod_dither: 0.0,
            },
        ],
    };
    let pixels = FrameRenderer::new()
        .render_offscreen(&device, &scene)
        .expect("frame")
        .pixels;
    let px = pixels.as_chunks::<4>().0;
    // Lower half: the ground right in front of the camera (cascade 0).
    let lower = &px[px.len() / 2..];
    lower
        .iter()
        .map(|p| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]))
        .sum::<f32>()
        / lower.len() as f32
}

#[test]
fn a_near_cascade_takes_the_shadow_of_a_far_up_sun_caster() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let lit = far_caster_frame(12_000.0, ShadowCast::Off);
    let shadowed = far_caster_frame(12_000.0, ShadowCast::All);
    eprintln!("near ground luma: wall off {lit:.1}, wall casting {shadowed:.1}");
    assert!(lit > 20.0, "the ground must be sunlit without the wall: {lit}");
    assert!(
        shadowed < lit * 0.6,
        "the wall 150 m up-sun must shadow the near ground: {shadowed} vs {lit}"
    );
}
