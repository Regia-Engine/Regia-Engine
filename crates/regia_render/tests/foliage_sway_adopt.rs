// SPDX-License-Identifier: MPL-2.0
//! Foliage sway on a plain textured material (`specs/88` § 2.5, ADR-2304), on lavapipe.
//!
//! A textured card (a plain `LabMaterial`, as a glTF import gives it) stands on a ground plane
//! under a low sun with paged sun cascades. [`FoliageSway::adopt`] turns it into a graph material:
//! in calm air it draws like the plain material, in a gale it bends, and with the dynamic-caster
//! class on its cached shadow follows the bend (a warm renderer matches a fresh one).

use regia_render::{
    DirectionalLightUniform, DynamicCasterShadows, FoliageSway, FrameMesh, FrameQuality, FrameRenderer,
    LabMaterial, LabMesh, LabScene, LabTexture, LabVertex, RenderDevice,
};

const SIZE: u32 = 96;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn sway() -> FoliageSway {
    FoliageSway {
        stiffness: 0.08,
        height_m: 1.6,
        flutter_m: 0.0,
        gust_period_s: 4.0,
        max_wind_m_per_s: 15.0,
        ..FoliageSway::default()
    }
}

/// A 4×4 checker of two greens (sRGB bytes), like a leaf atlas.
fn leaf_texture() -> LabTexture {
    let mut rgba = Vec::new();
    for i in 0..16u32 {
        let dark = (i + i / 4) % 2 == 0;
        rgba.extend_from_slice(if dark {
            &[60, 140, 50, 255]
        } else {
            &[120, 200, 90, 255]
        });
    }
    LabTexture {
        width: 4,
        height: 4,
        rgba,
    }
}

fn plain_leaf() -> LabMaterial {
    LabMaterial {
        base_color: [0.9, 1.0, 0.8, 1.0],
        roughness: 0.8,
        albedo: Some(leaf_texture()),
        double_sided: true,
        ..LabMaterial::default()
    }
}

fn ground() -> LabMesh {
    let n = [0.0, 1.0, 0.0];
    let e = 4.0;
    LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([-e, 0.0, -e], n, [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([e, 0.0, -e], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([e, 0.0, e], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([-e, 0.0, e], n, [0.0, 1.0]),
        ],
        indices: vec![0, 2, 1, 0, 3, 2],
        ..LabMesh::unskinned()
    }
}

/// The card in the plane x = 0, facing the sun, three rows so the bend has a middle.
fn card() -> (Vec<LabVertex>, Vec<u32>) {
    let n = [-1.0, 0.0, 0.0];
    let h = sway().height_m;
    let mut vertices = Vec::new();
    for (row, y) in [0.0, 0.5 * h, h].into_iter().enumerate() {
        for z in [-0.6f32, 0.6] {
            vertices.push(LabVertex::from_pos_normal_uv(
                [0.0, y, z],
                n,
                [f32::from(z > 0.0), row as f32 * 0.5],
            ));
        }
    }
    (vertices, vec![0, 1, 3, 0, 3, 2, 2, 3, 5, 2, 5, 4])
}

fn lab() -> LabScene {
    LabScene {
        width: SIZE,
        height: SIZE,
        eye: [0.5, 4.5, 4.5],
        target: [1.2, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 30.0,
        depth_prepass: false,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 2_000.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.8, -0.6, 0.1], [1.0; 3], 40_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes: vec![ground()],
    }
}

fn paged() -> FrameQuality {
    FrameQuality {
        paged_sun_shadows: true,
        ..FrameQuality::LAB
    }
}

fn render(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    material: &LabMaterial,
    wind: [f32; 2],
) -> Vec<u8> {
    let lab = lab();
    let (vertices, indices) = card();
    let mut scene = lab.as_frame_scene_with_quality(paged());
    let template = scene.meshes[0];
    scene.meshes.push(FrameMesh {
        mesh_key: 0x0AD0_97ED << 3,
        object_key: 0x0AD0_97ED,
        material_key: 0x0AD0_97EE,
        material,
        vertices: &vertices,
        indices: &indices,
        ..template
    });
    renderer.set_foliage_wind(Some(wind));
    renderer.set_material_clock(1.0, 1.0 - 1.0 / 60.0);
    match renderer.render_offscreen(device, &scene) {
        Ok(frame) => frame.pixels,
        Err(err) => panic!("frame: {err}"),
    }
}

fn differing(a: &[u8], b: &[u8], tolerance: u8) -> usize {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .filter(|(p, q)| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > tolerance))
        .count()
}

const CALM: [f32; 2] = [0.0, 0.0];
const BREEZE: [f32; 2] = [2.0, 0.0];
const GALE: [f32; 2] = [12.0, 0.0];

#[test]
fn an_adopted_plain_material_draws_like_the_plain_one_in_calm_air() {
    let device = device();
    let plain = plain_leaf();
    let adopted = sway().adopt(&plain).expect("plain material adopts");
    let a = render(&mut FrameRenderer::new(), &device, &plain, CALM);
    let b = render(&mut FrameRenderer::new(), &device, &adopted, CALM);
    let off = differing(&a, &b, 3);
    // The same metallic-roughness surface through the graph host: a few edge pixels may round
    // differently, the image may not.
    assert!(
        off * 100 <= (SIZE * SIZE) as usize,
        "{off} of {} pixels differ by more than 3",
        SIZE * SIZE
    );
    // The still copy draws the same surface with no vertex stage: identical in calm air.
    let still = FoliageSway::still(&adopted);
    let c = render(&mut FrameRenderer::new(), &device, &still, CALM);
    assert_eq!(
        differing(&b, &c, 0),
        0,
        "still and calm adopted frames are the same"
    );
}

#[test]
fn an_adopted_plain_material_sways_and_its_cached_shadow_follows() {
    let device = device();
    let adopted = sway().adopt(&plain_leaf()).expect("plain material adopts");
    let truth = render(&mut FrameRenderer::new(), &device, &adopted, GALE);
    let breeze = render(&mut FrameRenderer::new(), &device, &adopted, BREEZE);
    assert!(differing(&truth, &breeze, 2) > 20, "the gale bends the card");

    let mut renderer = FrameRenderer::new();
    renderer.set_dynamic_caster_shadows(DynamicCasterShadows::On {
        cascades: 2,
        local_lights: false,
    });
    render(&mut renderer, &device, &adopted, BREEZE);
    render(&mut renderer, &device, &adopted, BREEZE);
    let warm = render(&mut renderer, &device, &adopted, GALE);
    let report = renderer.dynamic_caster_report();
    assert_eq!(report.animated_casters, 1, "{report:?}");
    assert!(report.dynamic_pages > 0, "{report:?}");
    assert_eq!(
        differing(&warm, &truth, 2),
        0,
        "the cached shadow follows the adopted card"
    );
    // The plain material is not animated: no dynamic caster, nothing refreshed.
    let mut plain_renderer = FrameRenderer::new();
    plain_renderer.set_dynamic_caster_shadows(DynamicCasterShadows::On {
        cascades: 2,
        local_lights: false,
    });
    render(&mut plain_renderer, &device, &plain_leaf(), GALE);
    assert_eq!(plain_renderer.dynamic_caster_report().animated_casters, 0);
}
