// SPDX-License-Identifier: MPL-2.0
//! An interior volume fitted to a room: the walls, floor and roof lie exactly on the box's faces.
//!
//! The per-pixel interior-shade test compared the interpolated world position against the box
//! with no margin, and a surface on a face lands a few ulps either side of it: the shade flipped
//! between the interior floor and the sky IBL pixel by pixel, drawing one-pixel stripes of
//! constant depth along the walls (vertical) and the floor (horizontal) that changed with every
//! view (the tunnel acceptance scene, `regia_terrain/tests/tunnel_scene`). A fitted volume is
//! how a level marks a tunnel, so the faces must shade as inside.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, INTERIOR_AMBIENT_FLOOR_LUX, INTERIOR_EXPOSURE_EV,
    InteriorVolume, LabMaterial, LabMesh, LabScene, LabVertex, OutdoorParams, PostSettings, RenderDevice,
};

const W: u32 = 192;
const H: u32 = 108;
/// Half extents of the room and of its interior volume, metres.
const HALF: [f32; 3] = [2.0, 1.5, 6.0];

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip interior faces: {err}");
            None
        }
    }
}

fn quad(corners: [[f32; 3]; 4]) -> LabMesh {
    let [a, b, _, d] = corners.map(regia_core::glam::Vec3::from_array);
    let normal = (b - a).cross(d - a).normalize_or_zero().to_array();
    LabMesh {
        vertices: corners
            .iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(*p, normal, uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material: 0,
        ..LabMesh::unskinned()
    }
}

/// A closed room whose six faces are exactly the volume's faces.
fn room() -> Vec<LabMesh> {
    let [x, y, z] = HALF;
    vec![
        quad([[-x, -y, -z], [x, -y, -z], [x, -y, z], [-x, -y, z]]),
        quad([[-x, y, -z], [-x, y, z], [x, y, z], [x, y, -z]]),
        quad([[-x, -y, -z], [-x, -y, z], [-x, y, z], [-x, y, -z]]),
        quad([[x, -y, -z], [x, y, -z], [x, y, z], [x, -y, z]]),
        quad([[-x, -y, -z], [-x, y, -z], [x, y, -z], [x, -y, -z]]),
        quad([[-x, -y, z], [x, -y, z], [x, y, z], [-x, y, z]]),
    ]
}

fn render(interior_ambient: bool, eye: [f32; 3], target: [f32; 3]) -> Vec<u8> {
    let device = device().expect("caller checks the device");
    let mut quality = FrameQuality::LAB;
    quality.interior_ambient = interior_ambient;
    let post = PostSettings {
        exposure_ev: INTERIOR_EXPOSURE_EV,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        eye,
        target,
        fov_y: 1.0,
        near: 0.05,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: INTERIOR_EXPOSURE_EV,
        ambient_lux: INTERIOR_AMBIENT_FLOOR_LUX,
        double_sided: true,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.2, -1.0, 0.15], [1.0, 1.0, 1.0], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.6, 0.6, 0.6, 1.0],
            roughness: 0.8,
            ..LabMaterial::default()
        }],
        meshes: room(),
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        interiors: vec![InteriorVolume::with_floor([0.0; 3], HALF)],
        ..OutdoorParams::default()
    });
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    renderer.render_offscreen(&device, &frame).expect("render").pixels
}

fn luma(px: &[u8], x: u32, y: u32) -> f32 {
    let i = ((y * W + x) * 4) as usize;
    0.2126 * f32::from(px[i]) + 0.7152 * f32::from(px[i + 1]) + 0.0722 * f32::from(px[i + 2])
}

/// Pixels brighter or darker than both horizontal, or both vertical, neighbours by more than
/// `step` levels: a one-pixel stripe. A flat-lit room has none (its corners are steps, not
/// ridges).
fn ridge_pixels(px: &[u8], step: f32) -> u32 {
    let mut count = 0;
    for y in 1..H - 1 {
        for x in 1..W - 1 {
            let c = luma(px, x, y);
            let ridge = |a: f32, b: f32| (c - a > step && c - b > step) || (a - c > step && b - c > step);
            if ridge(luma(px, x - 1, y), luma(px, x + 1, y)) || ridge(luma(px, x, y - 1), luma(px, x, y + 1))
            {
                count += 1;
            }
        }
    }
    count
}

fn mean_luma(px: &[u8]) -> f32 {
    let mut sum = 0.0;
    for y in 0..H {
        for x in 0..W {
            sum += luma(px, x, y);
        }
    }
    sum / (W * H) as f32
}

#[test]
fn surfaces_on_the_interior_box_faces_shade_as_inside() {
    if device().is_none() {
        return;
    }
    // Two views: down the room at the far wall, and across it at a side wall and the floor.
    for (eye, target) in [
        ([0.3, 0.2, 4.5], [-0.4, -0.3, -5.0]),
        ([1.2, 0.6, 2.0], [-2.0, -1.2, -1.0]),
    ] {
        let inside = render(true, eye, target);
        let sky = render(false, eye, target);
        let (l_in, l_sky) = (mean_luma(&inside), mean_luma(&sky));
        let ridges = ridge_pixels(&inside, 12.0);
        eprintln!(
            "eye {eye:?}: mean luma interior {l_in:.1}, sky-lit {l_sky:.1}; one-pixel stripes {ridges} of {}",
            W * H
        );
        // The control: the sky-lit room is a different picture, so a pixel that took the sky
        // would show (the two shades differ by far more than the 12-level stripe step).
        assert!(
            (l_in - l_sky).abs() > 20.0,
            "the interior and sky shades must differ for this test to see a flip: {l_in:.1} vs {l_sky:.1}"
        );
        // Before the face margin a fitted room drew hundreds of stripe pixels a view; a uniform
        // shade has none. One or two are allowed for rasterization at the room's corners.
        assert!(
            ridges <= 2,
            "{ridges} one-pixel stripes: surfaces on the volume's faces flip between the interior \
             and the sky shade (eye {eye:?})"
        );
    }
}
