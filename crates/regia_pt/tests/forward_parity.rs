// SPDX-License-Identifier: MPL-2.0
//! `specs/27` § 1.5 item 4: the forward pass's direct term equals the path tracer's `direct`
//! AOV on a material-sphere grid under the sun — the "same BRDF by construction" check. The
//! forward frame's linear HDR comes from `FrameRenderer::request_hdr_capture`.

use regia_core::glam::{Mat4, Vec3};
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{Aov, PathTracer, PtSettings, PtSky};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, HardSurfaceFactors,
    LabMaterial, PostSettings, RenderDevice, ShadowCast,
};

#[test]
fn forward_direct_equals_pt_direct_on_the_material_grid() {
    let device = RenderDevice::headless().expect("headless device");
    let (verts, idx) = regia_render::uv_sphere(1.0, 64, 32);
    let mut materials = Vec::new();
    for row in 0..3 {
        for col in 0..5 {
            let roughness = [0.1, 0.3, 0.5, 0.7, 1.0][col];
            let mut m = LabMaterial {
                roughness,
                ..LabMaterial::EMPTY
            };
            match row {
                0 => m.base_color = [0.8, 0.2, 0.15, 1.0],
                1 => {
                    m.base_color = [1.0, 0.78, 0.34, 1.0];
                    m.metallic = 1.0;
                }
                _ => {
                    m.base_color = [0.1, 0.25, 0.6, 1.0];
                    m.hard_surface = HardSurfaceFactors {
                        clearcoat: 1.0,
                        clearcoat_roughness: 0.2,
                        ..HardSurfaceFactors::OFF
                    };
                }
            }
            materials.push(m);
        }
    }
    let meshes: Vec<FrameMesh<'_>> = materials
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let (row, col) = ((i / 5) as f32, (i % 5) as f32);
            FrameMesh {
                is_static: false,
                // Non-zero, distinct keys like `LabScene::as_frame_scene` (key 0 shares one object slot).
                mesh_key: 1,
                material_key: 100 + i as u64,
                object_key: 200 + i as u64,
                vertices: &verts,
                indices: &idx,
                material: m,
                model: Mat4::from_translation(Vec3::new((col - 2.0) * 2.6, (1.0 - row) * 2.6, 0.0))
                    .to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }
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
        width: 160,
        height: 96,
        eye: [0.0, 0.0, 16.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.62,
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
        directional: DirectionalLightUniform::new([-0.35, -0.5, -0.8], [1.0, 0.98, 0.95], 20_000.0),
        local_lights: &[],
        // Specular AA widens the forward lobe by the normal variance one pixel covers (a
        // screen-space filter the 1-spp, unjittered trace does not apply); the BRDFs compared
        // here are the unfiltered ones.
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
        .expect("the post stack ran, so the HDR copy exists");
    assert_eq!((hdr.width, hdr.height), (scene.width, scene.height));

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
    let (mut abs_err, mut sum, mut rel_sum, mut n) = (0.0f64, 0.0f64, 0.0f64, 0usize);
    // Skip the terminator (relative error of a near-zero value is noise): 1 % of the mean lit
    // value. The peak is useless here — a 0.1-roughness highlight is thousands of times brighter.
    let lit: Vec<f64> = img.direct.iter().map(|c| lum(*c)).filter(|v| *v > 0.0).collect();
    let floor = 0.01 * lit.iter().sum::<f64>() / lit.len().max(1) as f64;
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let i = y * w + x;
            // Interior sphere pixels with the sun visible (the forward frame has no shadows here).
            let inside = [i, i - 1, i + 1, i - w, i + w]
                .iter()
                .all(|&j| img.depth[j] > 0.0);
            if !inside || img.shadow[i] < 0.5 {
                continue;
            }
            let p = lum(img.direct[i]);
            let f = lum(hdr.rgb[i]);
            if p < floor {
                continue;
            }
            abs_err += (f - p).abs();
            sum += p;
            rel_sum += (f - p).abs() / p;
            n += 1;
        }
    }
    assert!(n > 2000, "{n} compared pixels");
    let l1 = abs_err / sum;
    let mre = rel_sum / n as f64;
    // The relative L1 is dominated by the 0.1-roughness highlights, which exceed the forward
    // frame's half-float HDR range (65504) in physical units; the per-pixel mean is the metric.
    eprintln!("forward vs PT direct: {n} px, relative L1 {l1:.5}, mean relative error {mre:.5}");
    assert!(
        mre < 0.01,
        "forward direct vs PT direct: mean relative error {mre}"
    );
    assert!(img.mean(Aov::Sky)[0] == 0.0);
}
