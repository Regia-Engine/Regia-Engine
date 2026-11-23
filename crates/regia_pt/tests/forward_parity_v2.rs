// SPDX-License-Identifier: MPL-2.0
//! `specs/27` § 1.5 item 4 for the v2 surface features: the forward pass's direct term equals
//! the path tracer's `direct` AOV on spheres with anisotropic GGX, a packed ORM texture and
//! detail albedo / roughness / normal maps (vertex tangents for both renderers).

use regia_core::glam::{Mat4, Vec3};
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtSettings, PtSky};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, HardSurfaceFactors,
    LabMaterial, LabTexture, LabVertex, PostSettings, RenderDevice, ShadowCast,
};

/// A smooth (low-frequency) RGBA8 image, so texture filtering differences (the forward's mips
/// and anisotropic sampler vs the tracer's bilinear) stay below the comparison's noise.
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

/// The sphere with tangents along +u (`∂P/∂θ`), the frame the normal map and anisotropy use.
fn sphere() -> (Vec<LabVertex>, Vec<u32>) {
    let (mut verts, idx) = regia_render::uv_sphere(1.0, 96, 48);
    for v in &mut verts {
        let [x, _, z] = v.position;
        let t = Vec3::new(-z, 0.0, x).normalize_or(Vec3::X);
        v.tangent = [t.x, t.y, t.z, 1.0];
    }
    (verts, idx)
}

#[test]
fn forward_direct_equals_pt_direct_with_anisotropy_orm_and_detail_maps() {
    let device = RenderDevice::headless().expect("headless device");
    let (verts, idx) = sphere();
    let tau = std::f32::consts::TAU;
    let orm = image(|u, v| {
        [
            255,
            byte(0.45 + 0.5 * (0.5 + 0.5 * (tau * u).sin())),
            byte(0.5 + 0.5 * (tau * v).cos()),
            255,
        ]
    });
    let detail_albedo = image(|u, v| {
        let w = 0.5 + 0.12 * (tau * u).sin() * (tau * v).cos();
        [byte(w), byte(w * 0.96), byte(w * 0.9), 255]
    });
    let detail_rough = image(|u, _| [byte(0.3 + 0.4 * (0.5 + 0.5 * (tau * u).cos())), 0, 0, 255]);
    let detail_normal = image(|u, v| {
        let (dx, dy) = (0.25 * (tau * u).cos(), 0.25 * (tau * v).sin());
        let n = Vec3::new(dx, dy, 1.0).normalize();
        [
            byte(n.x * 0.5 + 0.5),
            byte(n.y * 0.5 + 0.5),
            byte(n.z * 0.5 + 0.5),
            255,
        ]
    });
    let gold = [1.0, 0.78, 0.34, 1.0];
    let mut materials = Vec::new();
    // Anisotropic metals: two strengths and a rotation.
    for (aniso, rot, rough) in [(0.7f32, 0.0f32, 0.35f32), (-0.6, 0.8, 0.5), (0.9, 1.6, 0.6)] {
        materials.push(LabMaterial {
            base_color: gold,
            metallic: 1.0,
            roughness: rough,
            hard_surface: HardSurfaceFactors {
                anisotropy: aniso,
                anisotropy_rotation: rot,
                ..HardSurfaceFactors::OFF
            },
            ..LabMaterial::EMPTY
        });
    }
    // Packed ORM: G scales roughness, B scales metallic.
    materials.push(LabMaterial {
        base_color: [0.9, 0.5, 0.3, 1.0],
        metallic: 1.0,
        roughness: 0.9,
        occlusion_map: Some(orm),
        orm_packed: true,
        ..LabMaterial::EMPTY
    });
    // Detail albedo, roughness and normal.
    materials.push(LabMaterial {
        base_color: [0.35, 0.5, 0.7, 1.0],
        metallic: 0.0,
        roughness: 0.45,
        detail_albedo: Some(detail_albedo),
        detail_roughness: Some(detail_rough),
        detail_normal: Some(detail_normal),
        hard_surface: HardSurfaceFactors {
            detail_albedo_strength: 0.8,
            detail_normal_strength: 0.7,
            detail_roughness_strength: 0.6,
            detail_uv_scale: [2.0, 1.0],
            ..HardSurfaceFactors::OFF
        },
        ..LabMaterial::EMPTY
    });
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
            model: (Mat4::from_translation(Vec3::new((i as f32 - 2.0) * 2.5, 0.0, 0.0))
                * Mat4::from_rotation_y(0.4 * i as f32))
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
        height: 64,
        eye: [0.0, 1.2, 9.0],
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
        directional: DirectionalLightUniform::new([-0.45, -0.55, -0.7], [1.0, 0.98, 0.95], 20_000.0),
        local_lights: &[],
        // Specular AA widens the forward lobe by the screen-space normal variance (a filter
        // the 1-spp, unjittered trace does not apply); the BRDFs compared here are the
        // unfiltered ones (as in `forward_parity.rs`).
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
    // Per sphere: mean relative error over interior lit pixels.
    let sphere_w = w / materials.len();
    let mut worst = 0.0f64;
    for s in 0..materials.len() {
        let (mut rel, mut n) = (0.0f64, 0usize);
        for y in 1..h - 1 {
            for x in (s * sphere_w).max(1)..((s + 1) * sphere_w).min(w - 1) {
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
        eprintln!("sphere {s}: {n} px, mean relative error {mre:.5}");
        assert!(n > 100, "sphere {s}: {n} pixels");
        worst = worst.max(mre);
    }
    assert!(
        worst < 0.02,
        "forward vs PT direct: worst sphere mean relative error {worst}"
    );
}
