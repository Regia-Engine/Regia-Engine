// SPDX-License-Identifier: MPL-2.0
//! Screen-space contact shadows (`regia_render::contact_shadows`).
//!
//! A small rock sits on flat ground under a low sun. The sun cascade is coarse (one 256²
//! cascade over 200 m: texels of a metre or more), so the cascades alone lose the rock's
//! shadow. With contact shadows on, the ground at the rock's foot on the side away from the
//! sun darkens, and every pixel away from the rock stays bit-identical to the frame without
//! them (flat ground does not shadow itself, and every pixel of the pass is written).

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    RenderDevice, box_mesh, translate_mesh,
};

// The march is a fixed share of the target height (`contact_shadows::steps_for_height`):
// 13 pixels at 384 lines.
const W: u32 = 512;
const H: u32 = 384;

fn render(device: &RenderDevice, renderer: &mut FrameRenderer, contact: bool, eye: [f32; 3]) -> Vec<u8> {
    let (gv, gi) = box_mesh([10.0, 0.05, 10.0]);
    let (rv, ri) = box_mesh([0.06, 0.06, 0.06]);
    let meshes = vec![
        LabMesh {
            vertices: translate_mesh(&gv, [0.0, -0.05, 0.0]),
            indices: gi,
            material: 0,
            ..LabMesh::unskinned()
        },
        LabMesh {
            vertices: translate_mesh(&rv, [0.0, 0.06, 0.0]),
            indices: ri,
            material: 1,
            ..LabMesh::unskinned()
        },
    ];
    let mut quality = FrameQuality::LAB;
    quality.csm_cascade_count = 1;
    quality.csm_resolution = 256;
    quality.contact_shadows = contact;
    let lab = LabScene {
        width: W,
        height: H,
        eye,
        target: [0.0, 0.03, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 2_000.0,
        double_sided: false,
        post: None,
        // A low sun shining toward the camera: the rock's shadow falls toward the viewer.
        directional: DirectionalLightUniform::new([0.25, -0.45, 0.85], [1.0; 3], 60_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.5, 0.5, 0.5, 1.0],
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.45, 0.35, 0.3, 1.0],
                ..LabMaterial::default()
            },
        ],
        meshes,
    };
    let frame = lab.as_frame_scene_with_quality(quality);
    renderer.render_offscreen(device, &frame).expect("frame").pixels
}

fn luma(px: &[u8]) -> f32 {
    0.25 * f32::from(px[0]) + 0.5 * f32::from(px[1]) + 0.25 * f32::from(px[2])
}

/// Pixels of the central box around the rock (where its contact shadow may fall).
fn near_rock(x: u32, y: u32) -> bool {
    x.abs_diff(W / 2) < W / 4 && y.abs_diff(H / 2) < H * 3 / 10
}

fn check(eye: [f32; 3]) {
    let device = RenderDevice::shared_headless().expect("lavapipe");
    let mut renderer = FrameRenderer::new();
    let off = render(&device, &mut renderer, false, eye);
    let on = render(&device, &mut renderer, true, eye);
    // A second "on" frame: warm pipelines, same pixels (the pass is deterministic).
    let again = render(&device, &mut renderer, true, eye);
    assert_eq!(on, again, "contact shadows are not deterministic");

    let mut darkened = 0u32;
    let mut lit_ground = Vec::new();
    let mut changed_far = Vec::new();
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            let (a, b) = (luma(&off[i..i + 4]), luma(&on[i..i + 4]));
            if near_rock(x, y) {
                if b < a * 0.75 {
                    darkened += 1;
                    lit_ground.push(a);
                }
            } else if off[i..i + 4] != on[i..i + 4] {
                changed_far.push((x, y, a, b));
            }
        }
    }
    assert!(
        changed_far.is_empty(),
        "eye {eye:?}: {} pixels away from the rock changed, first {:?}",
        changed_far.len(),
        &changed_far[..changed_far.len().min(6)]
    );
    assert!(
        darkened >= 40,
        "eye {eye:?}: only {darkened} pixels at the rock's foot darkened by contact shadows"
    );
    // The cascades alone left those pixels in sun: without contact shadows they are about as
    // bright as the open ground.
    let open = luma(&off[(((H - 5) * W + 4) * 4) as usize..][..4]);
    let mean = lit_ground.iter().sum::<f32>() / lit_ground.len().max(1) as f32;
    assert!(
        mean > open * 0.8,
        "eye {eye:?}: the cascades already shadow the contact region (mean {mean:.1} vs open ground {open:.1})"
    );
}

#[test]
fn rock_on_ground_gets_a_contact_shadow_the_cascades_miss() {
    check([0.0, 0.35, 0.9]);
}

#[test]
fn rock_contact_shadow_from_a_side_view() {
    // The sun's screen point lands off to the side: other sectors and line directions.
    check([0.8, 0.3, 0.45]);
}

#[test]
fn rock_contact_shadow_with_the_sun_behind_the_camera() {
    // The camera looks along the light: rays march away from the sun's anti-point.
    check([0.1, 0.35, -0.9]);
}

/// A 160×90 frame of one sunlit surface facing away from the camera: a ceiling seen from
/// below whose normal points up, toward the sun (wound so the culling keeps its underside, as
/// an inside-out mesh is). Nothing else is in the frame.
fn ceiling(device: &RenderDevice, renderer: &mut FrameRenderer, contact: bool, to_sun: [f32; 3]) -> Vec<u8> {
    let n = [0.0, 1.0, 0.0];
    let (x, z0, z1, y) = (30.0, 4.0, 90.0, 3.0);
    let vertices = vec![
        LabVertex::from_pos_normal_uv([-x, y, z0], n, [0.0, 0.0]),
        LabVertex::from_pos_normal_uv([x, y, z0], n, [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([x, y, z1], n, [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([-x, y, z1], n, [0.0, 1.0]),
    ];
    let mut quality = FrameQuality::LAB;
    quality.contact_shadows = contact;
    let len = to_sun.iter().map(|c| c * c).sum::<f32>().sqrt();
    let lab = LabScene {
        width: 160,
        height: 90,
        eye: [0.0, 0.0, 0.0],
        target: [0.0, 2.0, 40.0],
        fov_y: 55.0_f32.to_radians(),
        near: 0.3,
        far: 2_000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 2_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new(to_sun.map(|c| -c / len), [1.0; 3], 60_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.5, 0.5, 0.5, 1.0],
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices,
            indices: vec![0, 1, 2, 0, 2, 3],
            material: 0,
            ..LabMesh::unskinned()
        }],
    };
    renderer
        .render_offscreen(device, &lab.as_frame_scene_with_quality(quality))
        .expect("frame")
        .pixels
}

fn surface_does_not_shadow_itself(to_sun: [f32; 3]) {
    let device = RenderDevice::shared_headless().expect("lavapipe");
    let mut renderer = FrameRenderer::new();
    let off = ceiling(&device, &mut renderer, false, to_sun);
    let on = ceiling(&device, &mut renderer, true, to_sun);
    // The sun reaches the ceiling: it is brighter than with the sun below the horizon, so a
    // contact shadow on it would show.
    let unlit = ceiling(&device, &mut renderer, false, [to_sun[0], -to_sun[1], to_sun[2]]);
    let lit_px = (0..off.len() / 4)
        .filter(|&i| luma(&off[i * 4..i * 4 + 4]) > luma(&unlit[i * 4..i * 4 + 4]) * 1.5 + 4.0)
        .count();
    assert!(
        lit_px > 160 * 20,
        "the sun does not light the ceiling ({lit_px} pixels)"
    );
    let changed: Vec<(usize, usize, f32, f32)> = (0..off.len() / 4)
        .filter(|&i| off[i * 4..i * 4 + 4] != on[i * 4..i * 4 + 4])
        .map(|i| {
            (
                i % 160,
                i / 160,
                luma(&off[i * 4..i * 4 + 4]),
                luma(&on[i * 4..i * 4 + 4]),
            )
        })
        .collect();
    assert!(
        changed.is_empty(),
        "sun {to_sun:?}: the ceiling shadows itself on {} pixels, first {:?}",
        changed.len(),
        &changed[..changed.len().min(6)]
    );
}

#[test]
fn surface_facing_away_from_the_camera_does_not_shadow_itself() {
    // High sun slightly ahead of the camera: rays march toward its screen point, far above.
    surface_does_not_shadow_itself([0.25, 0.95, 0.2]);
}

#[test]
fn surface_facing_away_does_not_shadow_itself_with_the_sun_behind() {
    // High sun behind the camera: rays march away from the anti-point below the screen.
    surface_does_not_shadow_itself([0.25, 0.95, -0.3]);
}

/// A 640×360 frame of a sunlit wall 130 m away with two rows of slats in front of it (ADR-1449):
/// on the screen's left 3 m in front of the wall (a tree's twig cards in front of the ground behind
/// them), on its right 0.4 m in front (contact scale). The sun is behind the camera and to the
/// side, so the march runs across the slats. The pass runs only with sun shadows; one coarse
/// cascade (as the rock frames) draws the same shadow-map shadows with and without it, so the
/// difference between the two frames is the contact pass alone.
fn slat_wall(device: &RenderDevice, renderer: &mut FrameRenderer, contact: bool) -> Vec<u8> {
    let (wv, wi) = box_mesh([40.0, 20.0, 0.1]);
    let mut meshes = vec![LabMesh {
        vertices: translate_mesh(&wv, [0.0, 2.0, 130.0]),
        indices: wi,
        material: 0,
        ..LabMesh::unskinned()
    }];
    // The camera looks along +z, so +x is the screen's left.
    for (x, z) in [
        (4.0, 127.0),
        (7.0, 127.0),
        (10.0, 127.0),
        (-4.0, 129.4),
        (-7.0, 129.4),
        (-10.0, 129.4),
    ] {
        let (sv, si) = box_mesh([0.3, 2.0, 0.1]);
        meshes.push(LabMesh {
            vertices: translate_mesh(&sv, [x, 2.0, z]),
            indices: si,
            material: 1,
            ..LabMesh::unskinned()
        });
    }
    let mut quality = FrameQuality::LAB;
    quality.csm_cascade_count = 1;
    quality.csm_resolution = 256;
    quality.contact_shadows = contact;
    let to_sun = [-0.85f32, 0.35, -0.4];
    let len = to_sun.iter().map(|c| c * c).sum::<f32>().sqrt();
    let lab = LabScene {
        width: 640,
        height: 360,
        eye: [0.0, 2.0, 0.0],
        target: [0.0, 2.0, 130.0],
        fov_y: 0.2,
        near: 0.5,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 2_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new(to_sun.map(|c| -c / len), [1.0; 3], 60_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.5, 0.5, 0.5, 1.0],
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.6, 0.15, 0.1, 1.0],
                ..LabMaterial::default()
            },
        ],
        meshes,
    };
    renderer
        .render_offscreen(device, &lab.as_frame_scene_with_quality(quality))
        .expect("frame")
        .pixels
}

/// Far away the assumed occluder thickness is capped at `THICKNESS_MAX_M`: slats 3 m in front of
/// the wall (the 4 % relative thickness alone is 5.2 m at 130 m) cast almost no contact shadow on
/// it (under a fiftieth of what the near slats cast; as much without the cap), while slats 0.4 m
/// in front still do (look-2 finding 3: false shadow curtains under far crowns).
#[test]
fn far_occluders_metres_in_front_of_a_surface_cast_no_contact_shadow() {
    let device = RenderDevice::shared_headless().expect("lavapipe");
    let mut renderer = FrameRenderer::new();
    let off = slat_wall(&device, &mut renderer, false);
    let on = slat_wall(&device, &mut renderer, true);
    let (w, h) = (640usize, 360usize);
    // Wall pixels: grey in the frame without contact shadows (the slats are red), 2 px or more
    // from any slat pixel (a silhouette's antialiased rim mixes both, and its contact sample may
    // be the slat itself).
    let grey = |x: usize, y: usize| {
        let p = &off[(y * w + x) * 4..(y * w + x) * 4 + 4];
        p[0].abs_diff(p[1]) <= 6 && p[1].abs_diff(p[2]) <= 6 && p[1] > 20
    };
    let wall = |x: usize, y: usize| {
        (y.saturating_sub(2)..(y + 3).min(h))
            .all(|v| (x.saturating_sub(2)..(x + 3).min(w)).all(|u| grey(u, v)))
    };
    let (mut far_dark, mut near_dark, mut wall_px) = (0u32, 0u32, 0u32);
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if !wall(x, y) {
                continue;
            }
            wall_px += 1;
            if luma(&on[i * 4..i * 4 + 4]) < luma(&off[i * 4..i * 4 + 4]) * 0.85 {
                if x < w / 2 {
                    far_dark += 1;
                } else {
                    near_dark += 1;
                }
            }
        }
    }
    eprintln!(
        "wall pixels {wall_px}: darkened by contact shadows {far_dark} (slats 3 m off) | {near_dark} (0.4 m off)"
    );
    assert!(
        wall_px > (w * h / 2) as u32,
        "the wall fills the frame ({wall_px} pixels)"
    );
    // A few pixels remain where the strip interpolates in 1/z across a slat's end (the 2 % depth
    // step at 130 m is no edge for the march), which lands a sample under 1 m from the ray. Without
    // the cap the far slats darkened as much wall as the near ones (1421 against 1377 pixels).
    assert!(
        far_dark * 50 <= near_dark,
        "slats 3 m in front of a wall 130 m away darkened {far_dark} wall pixels (contact-scale slats {near_dark})"
    );
    assert!(
        near_dark >= 20,
        "contact-scale slats (0.4 m) at 130 m must still shadow the wall ({near_dark} pixels)"
    );
}
