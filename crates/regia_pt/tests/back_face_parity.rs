// SPDX-License-Identifier: MPL-2.0
//! `specs/27` § 1.5 item 4 on double-sided back faces: the forward pass's direct term equals the
//! path tracer's `direct` AOV on the back faces of normal-mapped, detail-normal-mapped and
//! anisotropic sheets. Both renderers shade a back face with the front's tangent frame negated
//! (its mapped normal is the front's reversed); flipping only the normal mirrors the map's X
//! slope and the anisotropy direction, which this comparison would catch in either renderer.

use regia_core::glam::{Mat4, Vec3};
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtSettings, PtSky};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, HardSurfaceFactors,
    LabMaterial, LabTexture, LabVertex, PostSettings, RenderDevice, ShadowCast,
};

/// A smooth RGBA8 image (low frequency, so filtering differences stay below the noise).
fn image(f: impl Fn(f32, f32) -> [u8; 4]) -> LabTexture {
    let n = 64u32;
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            rgba.extend_from_slice(&f((x as f32 + 0.5) / n as f32, (y as f32 + 0.5) / n as f32));
        }
    }
    LabTexture {
        width: n,
        height: n,
        rgba,
    }
}

fn byte(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

/// A tangent-space normal map tilted toward +T (the map's X slope), varying slowly.
fn tilted(strength: f32) -> LabTexture {
    let tau = std::f32::consts::TAU;
    image(move |u, v| {
        let n = Vec3::new(
            strength * (0.6 + 0.3 * (tau * v).cos()),
            0.15 * (tau * u).sin(),
            1.0,
        )
        .normalize();
        [
            byte(n.x * 0.5 + 0.5),
            byte(n.y * 0.5 + 0.5),
            byte(n.z * 0.5 + 0.5),
            255,
        ]
    })
}

/// A 2 × 2 m sheet in the XY plane, front face +Z (CCW seen from +Z), tangent +X.
fn sheet() -> (Vec<LabVertex>, Vec<u32>) {
    let corners = [
        ([-1.0, -1.0, 0.0], [0.0, 1.0]),
        ([1.0, -1.0, 0.0], [1.0, 1.0]),
        ([1.0, 1.0, 0.0], [1.0, 0.0]),
        ([-1.0, 1.0, 0.0], [0.0, 0.0]),
    ];
    let verts = corners
        .iter()
        .map(|&(p, uv)| LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], uv))
        .collect();
    (verts, vec![0, 1, 2, 0, 2, 3])
}

#[test]
fn forward_direct_equals_pt_direct_on_normal_mapped_back_faces() {
    let device = RenderDevice::headless().expect("headless device");
    let (verts, idx) = sheet();
    let materials = [
        // Base normal map on a dielectric.
        LabMaterial {
            base_color: [0.7, 0.5, 0.35, 1.0],
            roughness: 0.45,
            normal_map: Some(tilted(0.8)),
            normal_scale: 1.0,
            double_sided: true,
            ..LabMaterial::EMPTY
        },
        // Detail normal only.
        LabMaterial {
            base_color: [0.35, 0.5, 0.7, 1.0],
            roughness: 0.5,
            detail_normal: Some(tilted(0.7)),
            hard_surface: HardSurfaceFactors {
                detail_normal_strength: 1.0,
                detail_uv_scale: [1.0, 1.0],
                ..HardSurfaceFactors::OFF
            },
            double_sided: true,
            ..LabMaterial::EMPTY
        },
        // Anisotropic metal with a rotation.
        LabMaterial {
            base_color: [1.0, 0.78, 0.34, 1.0],
            metallic: 1.0,
            roughness: 0.4,
            hard_surface: HardSurfaceFactors {
                anisotropy: 0.8,
                anisotropy_rotation: 0.5,
                ..HardSurfaceFactors::OFF
            },
            double_sided: true,
            ..LabMaterial::EMPTY
        },
    ];
    // Seen from −Z: every sheet shows its back face. Each is turned a little about Y.
    let meshes: Vec<FrameMesh<'_>> = materials
        .iter()
        .enumerate()
        .map(|(i, m)| FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 100 + i as u64,
            object_key: 200 + i as u64,
            vertices: &verts,
            indices: &idx,
            material: m,
            model: (Mat4::from_translation(Vec3::new((1.0 - i as f32) * 2.4, 0.0, 0.0))
                * Mat4::from_rotation_y(0.25 * (i as f32 - 1.0)))
            .to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        })
        .collect();
    let post = PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        taa: false,
        ..PostSettings::default()
    };
    let scene = FrameScene {
        width: 240,
        height: 80,
        eye: [0.0, 0.8, -9.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.5,
        near: 0.1,
        far: 100.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 13.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(&post),
        fog: regia_render::FogSettings::default(),
        // The sun behind the camera, from the upper left: lights the back faces.
        directional: DirectionalLightUniform::new([0.5, -0.45, 0.75], [1.0, 0.98, 0.95], 20_000.0),
        local_lights: &[],
        // Unfiltered BRDFs, as in `forward_parity_v2.rs`.
        quality: FrameQuality {
            specular_aa: false,
            ..FrameQuality::LAB
        },
        meshes,
    };
    let mut renderer = FrameRenderer::new();
    renderer.request_hdr_capture();
    renderer.render_offscreen(&device, &scene).expect("forward frame");
    let hdr = renderer
        .take_hdr_capture(&device)
        .expect("hdr readback")
        .expect("the post stack ran");

    let pt_scene = from_frame(
        &scene,
        &ExtractOptions {
            sky: Some(PtSky::None),
            ..ExtractOptions::default()
        },
    );
    let tracer = PathTracer::new(&device).expect("compile");
    let settings = PtSettings {
        spp: 1,
        max_bounces: 1,
        jitter: false,
        ..PtSettings::default()
    };
    let img = tracer.render(&pt_scene, &settings).expect("trace");

    let (w, h) = (scene.width as usize, scene.height as usize);
    let lum = |c: [f32; 3]| 0.2126 * f64::from(c[0]) + 0.7152 * f64::from(c[1]) + 0.0722 * f64::from(c[2]);
    let lit: Vec<f64> = img.direct.iter().map(|c| lum(*c)).filter(|v| *v > 0.0).collect();
    let floor = 0.01 * lit.iter().sum::<f64>() / lit.len().max(1) as f64;
    let sheet_w = w / materials.len();
    let mut worst = 0.0f64;
    for s in 0..materials.len() {
        let (mut rel, mut n) = (0.0f64, 0usize);
        // Sheets run right to left in screen space (seen from −Z, +X is on the left).
        let col = materials.len() - 1 - s;
        for y in 1..h - 1 {
            for x in (col * sheet_w).max(1)..((col + 1) * sheet_w).min(w - 1) {
                let i = y * w + x;
                let inside = [i, i - 1, i + 1, i - w, i + w]
                    .iter()
                    .all(|&j| img.depth[j] > 0.0);
                if !inside || img.shadow[i] < 0.5 {
                    continue;
                }
                let p = lum(img.direct[i]);
                if p < floor {
                    continue;
                }
                rel += (lum(hdr.rgb[i]) - p).abs() / p;
                n += 1;
            }
        }
        let mre = rel / n.max(1) as f64;
        eprintln!("back-face sheet {s}: {n} px, mean relative error {mre:.5}");
        assert!(n > 100, "sheet {s}: {n} pixels");
        worst = worst.max(mre);
    }
    assert!(
        worst < 0.02,
        "forward vs PT direct on back faces: worst sheet mean relative error {worst}"
    );
}
