// SPDX-License-Identifier: MPL-2.0
//! A frame without IBL still binds the BRDF's own DFG table (ADR-359). GPU test (lavapipe in
//! CI), no golden: the same sunlit dielectric renders once with the multi-scatter compensation
//! and once without, and the two must agree to within the compensation's own small change.
//!
//! Before the fix the no-IBL view bound a 1×1 white texture as `brdf_lut`: the compensation
//! then read a single-scatter albedo of 1 (`es_o = f0 + 1`, clamped to 1) and took the whole
//! diffuse lobe away, so a multi-scatter material (every material graph by default) under the
//! sun alone rendered black.

use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabVertex, RenderDevice,
};

fn quad() -> Vec<LabVertex> {
    [[-2.0f32, -2.0], [2.0, -2.0], [2.0, 2.0], [-2.0, 2.0]]
        .into_iter()
        .map(|[x, z]| LabVertex {
            position: [x, 0.0, z],
            normal: [0.0, 1.0, 0.0],
            uv: [0.0, 0.0],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect()
}

/// The centre pixel of a floor lit by an overhead sun, seen from above, no IBL, no ambient.
fn render(device: &RenderDevice, material: &LabMaterial) -> [u8; 4] {
    let vertices = quad();
    let indices = [0u32, 2, 1, 0, 3, 2];
    let scene = FrameScene {
        width: 64,
        height: 64,
        eye: [0.0, 4.0, 0.01],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.1,
        far: 50.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        fog: FogSettings::default(),
        // Slightly off the view axis, so the sun's mirror highlight is not in the sampled pixel.
        directional: DirectionalLightUniform::new([0.3, -1.0, 0.2], [1.0, 1.0, 1.0], 0.8),
        local_lights: &[],
        quality: FrameQuality::LAB,
        meshes: vec![FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 1,
            object_key: 1,
            vertices: &vertices,
            indices: &indices,
            material,
            model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }],
    };
    // A fresh renderer per frame, so no bind group is shared between the two versions.
    let pixels = FrameRenderer::new()
        .render_offscreen(device, &scene)
        .expect("frame")
        .pixels;
    let i = ((32 * 64 + 32) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

#[test]
fn multi_scatter_keeps_the_diffuse_lobe_without_ibl() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let mut single = LabMaterial {
        base_color: [0.8, 0.3, 0.3, 1.0],
        roughness: 0.5,
        metallic: 0.0,
        ..LabMaterial::default()
    };
    single.hard_surface.multi_scatter = 0.0;
    let mut multi = single.clone();
    multi.hard_surface.multi_scatter = 1.0;

    let a = render(&device, &single);
    let b = render(&device, &multi);
    eprintln!("single-scatter {a:?}, multi-scatter {b:?}");
    assert!(a[0] > 40, "the control frame is too dark to compare: {a:?}");
    // A dielectric's multi-scatter compensation moves a few percent of the diffuse into the
    // specular (`1 − E_ss` with E_ss ≈ 0.07 here); it never removes the diffuse lobe.
    for c in 0..3 {
        let ratio = f64::from(b[c]) / f64::from(a[c].max(1));
        assert!(
            (0.8..=1.2).contains(&ratio),
            "channel {c}: multi-scatter {b:?} vs single-scatter {a:?} (ratio {ratio:.2})"
        );
    }
}
