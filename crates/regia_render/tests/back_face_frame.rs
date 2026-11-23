// SPDX-License-Identifier: MPL-2.0
//! Double-sided back faces shade with the front's tangent frame negated (glTF `doubleSided`).
//!
//! The back of a thin sheet is the front surface seen from behind: its mapped normal is the
//! front's reversed. Flipping only the normal (not the tangent) mirrors the normal map's X slope.

use regia_render::{
    DirectionalLightUniform, FoliageSway, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    LabTexture, LabVertex, PostSettings, RenderDevice,
};

const W: u32 = 128;
const H: u32 = 96;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A quad in the XY plane through the origin, front face +Z (CCW seen from +Z), tangent +X.
fn fin(half: f32) -> LabMesh {
    let corners = [
        ([-half, -half, 0.0], [0.0, 1.0]),
        ([half, -half, 0.0], [1.0, 1.0]),
        ([half, half, 0.0], [1.0, 0.0]),
        ([-half, half, 0.0], [0.0, 0.0]),
    ];
    LabMesh {
        vertices: corners
            .iter()
            .map(|&(position, uv)| LabVertex::from_pos_normal_uv(position, [0.0, 0.0, 1.0], uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        ..LabMesh::unskinned()
    }
}

/// A constant tangent-space normal tilted toward +T (the map's X slope only).
fn tilted_normal_map() -> LabTexture {
    let (x, z) = (0.6f32, 0.8f32);
    let px = [
        ((x + 1.0) * 0.5 * 255.0).round() as u8,
        128,
        ((z + 1.0) * 0.5 * 255.0).round() as u8,
        255,
    ];
    LabTexture {
        width: 4,
        height: 4,
        rgba: px.iter().copied().cycle().take(4 * 4 * 4).collect(),
    }
}

/// One double-sided, normal-mapped fin at the origin seen along ∓Z. `from_front`: camera on +Z
/// (front face) with the sun behind it; otherwise the whole setup turned 180° about Y (camera on
/// −Z, the back face).
fn single_fin(from_front: bool) -> LabScene {
    let (mut eye, mut sun) = ([0.0f32, 0.0, 3.0], [-0.55f32, -0.25, -0.8]);
    if !from_front {
        // 180° about Y: (x, y, z) → (−x, y, −z).
        eye = [-eye[0], eye[1], -eye[2]];
        sun = [-sun[0], sun[1], -sun[2]];
    }
    LabScene {
        width: W,
        height: H,
        eye,
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        // The frame culls. Double-sided is a property of the fin material.
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new(sun, [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.7, 0.45, 0.3, 1.0],
            roughness: 0.6,
            normal_scale: 1.0,
            normal_map: Some(tilted_normal_map()),
            double_sided: true,
            ..LabMaterial::default()
        }],
        meshes: vec![fin(1.2)],
    }
}

fn render(scene: &LabScene) -> Vec<u8> {
    let device = device();
    let frame = scene.as_frame_scene_with_quality(FrameQuality::LAB);
    let mut renderer = FrameRenderer::new();
    renderer.render_offscreen(&device, &frame).expect("frame").pixels
}

fn centre_luma(px: &[u8]) -> f64 {
    let (x0, x1) = ((0.4 * W as f32) as u32, (0.6 * W as f32) as u32);
    let (y0, y1) = ((0.4 * H as f32) as u32, (0.6 * H as f32) as u32);
    let (mut sum, mut n) = (0.0, 0.0);
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            sum += 0.2126 * f64::from(px[i]) + 0.7152 * f64::from(px[i + 1]) + 0.0722 * f64::from(px[i + 2]);
            n += 1.0;
        }
    }
    sum / n
}

#[test]
fn normal_mapped_back_face_is_the_front_reversed() {
    // Turning camera and sun 180° about Y shades the back face like the front. Flipping only the
    // normal mirrors the map's X slope: with this sun N·L would drop from ~0.98 to ~0.25.
    let front = render(&single_fin(true));
    let back = render(&single_fin(false));
    let (front_l, back_l) = (centre_luma(&front), centre_luma(&back));
    eprintln!("normal-mapped fin: front {front_l:.1}, back {back_l:.1}");
    assert!(front_l > 40.0, "the front face must be lit: {front_l:.1}");
    assert!(
        (back_l - front_l).abs() <= 2.0,
        "the back face must shade with the front's normal reversed: back {back_l:.1} vs front {front_l:.1}"
    );
}

/// Audit G render5 #13: a plain material adopted for foliage sway ([`FoliageSway::adopt`], a graph
/// material) shades its back face like the standard material does. The graph surface input used
/// to flip only the normal, which mirrors the map's X slope on the back of a leaf card.
#[test]
fn an_adopted_foliage_back_face_is_the_front_reversed() {
    let adopted = |mut scene: LabScene| {
        scene.materials[0] = FoliageSway::default()
            .adopt(&scene.materials[0])
            .expect("a plain material adopts");
        scene
    };
    let front = render(&adopted(single_fin(true)));
    let back = render(&adopted(single_fin(false)));
    let (front_l, back_l) = (centre_luma(&front), centre_luma(&back));
    eprintln!("adopted foliage fin: front {front_l:.1}, back {back_l:.1}");
    assert!(front_l > 40.0, "the front face must be lit: {front_l:.1}");
    assert!(
        (back_l - front_l).abs() <= 2.0,
        "the adopted back face must shade with the front's normal reversed: back {back_l:.1} vs front {front_l:.1}"
    );
    // At rest the adopted material is the standard one (module docs of `foliage_sway_material`).
    let standard = centre_luma(&render(&single_fin(false)));
    assert!(
        (back_l - standard).abs() <= 2.0,
        "adopted back {back_l:.1} vs standard back {standard:.1}"
    );
    // A map with a Y slope as well: the bitangent of the back face's frame must be the standard
    // one too (the graph rebuilds it as `cross(n, t) * w`, so the sign `w` flips with `t`).
    let sloped = |mut scene: LabScene| {
        scene.materials[0].normal_map = Some(sloped_normal_map(0.45, 0.5));
        scene
    };
    for from_front in [true, false] {
        let standard = centre_luma(&render(&sloped(single_fin(from_front))));
        let adopted = centre_luma(&render(&adopted(sloped(single_fin(from_front)))));
        eprintln!("X+Y slope, front {from_front}: standard {standard:.1}, adopted {adopted:.1}");
        assert!(
            (adopted - standard).abs() <= 2.0,
            "front {from_front}: adopted {adopted:.1} vs standard {standard:.1}"
        );
    }
}

/// A constant tangent-space normal with slopes `x` (toward +T) and `y` (toward +B).
fn sloped_normal_map(x: f32, y: f32) -> LabTexture {
    let z = (1.0 - x * x - y * y).max(0.0).sqrt();
    let byte = |v: f32| ((v + 1.0) * 0.5 * 255.0).round() as u8;
    let px = [byte(x), byte(y), byte(z), 255];
    LabTexture {
        width: 4,
        height: 4,
        rgba: px.iter().copied().cycle().take(4 * 4 * 4).collect(),
    }
}
