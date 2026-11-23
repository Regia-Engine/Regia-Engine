// SPDX-License-Identifier: MPL-2.0
//! The `natural` foliage sway style (ADR-2446), on lavapipe: the stage's GPU branch agrees with
//! the CPU reference at the calm and gusty ends.
//!
//! A card with the natural sway stands at the origin. Where the CPU gust field says the air is
//! calm at the card, the frame equals the frame without any wind (the card is at rest); where it
//! says gust, the card moves.

use std::sync::Arc;

use regia_render::{
    DirectionalLightUniform, FoliageSway, FrameMesh, FrameQuality, FrameRenderer, LabMaterial, LabMesh,
    LabScene, LabVertex, RenderDevice, WindStyle, wind_natural,
};

/// Opaque green, lit (the shadow on the ground is what the test reads).
const SURFACE: &str = r"
fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {
    return regia::pbr::surface::Surface(
        vec3<f32>(0.2, 0.6, 0.15),
        1.0,
        0.0,
        0.8,
        in.world_normal,
        1.0,
        vec3<f32>(0.0),
        0.0,
    );
}
";

const SIZE: u32 = 96;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A 1.6 m tall card that bends 0.16-0.8 m at its top between 2 and 12 m/s of wind.
fn sway() -> FoliageSway {
    FoliageSway {
        style: WindStyle::Natural,
        stiffness: 0.08,
        height_m: 1.6,
        flutter_m: 0.0,
        gust_period_s: 4.0,
        max_wind_m_per_s: 15.0,
    }
}

fn ground() -> (Vec<LabVertex>, Vec<u32>) {
    let n = [0.0, 1.0, 0.0];
    let e = 4.0;
    (
        vec![
            LabVertex::from_pos_normal_uv([-e, 0.0, -e], n, [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([e, 0.0, -e], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([e, 0.0, e], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([-e, 0.0, e], n, [0.0, 1.0]),
        ],
        vec![0, 2, 1, 0, 3, 2],
    )
}

/// The card in the plane x = 0, z in [-0.6, 0.6], y in [0, 1.6]: it faces the sun (which
/// travels toward +x), so its shadow falls along +x, the way the wind bends it.
fn card() -> (Vec<LabVertex>, Vec<u32>) {
    let n = [-1.0, 0.0, 0.0];
    let h = sway().height_m;
    // Two rows so the bend (h²) has vertices in the middle of the card.
    let mut vertices = Vec::new();
    for (row, y) in [0.0, 0.5 * h, h].into_iter().enumerate() {
        for z in [-0.6, 0.6] {
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
    let (vertices, indices) = ground();
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
        meshes: vec![LabMesh {
            vertices,
            indices,
            ..LabMesh::unskinned()
        }],
    }
}

fn paged() -> FrameQuality {
    FrameQuality {
        paged_sun_shadows: true,
        ..FrameQuality::LAB
    }
}

struct Fixture {
    lab: LabScene,
    card_vertices: Vec<LabVertex>,
    card_indices: Vec<u32>,
    material: LabMaterial,
}

impl Fixture {
    fn new() -> Self {
        let (card_vertices, card_indices) = card();
        Self {
            lab: lab(),
            card_vertices,
            card_indices,
            material: LabMaterial {
                surface_graph: Some(Arc::new(sway().graph(SURFACE))),
                ..LabMaterial::default()
            },
        }
    }

    fn render(
        &self,
        renderer: &mut FrameRenderer,
        device: &RenderDevice,
        wind: [f32; 2],
        time: f32,
    ) -> Vec<u8> {
        let mut scene = self.lab.as_frame_scene_with_quality(paged());
        let template = scene.meshes[0];
        scene.meshes.push(FrameMesh {
            mesh_key: 0x0F01_1A6E << 3,
            object_key: 0x0F01_1A6E,
            material_key: 0x0F01_1A6F,
            material: &self.material,
            vertices: &self.card_vertices,
            indices: &self.card_indices,
            ..template
        });
        renderer.set_foliage_wind(Some(wind));
        renderer.set_material_clock(time, time - 1.0 / 60.0);
        match renderer.render_offscreen(device, &scene) {
            Ok(frame) => frame.pixels,
            Err(err) => panic!("frame: {err}"),
        }
    }
}

/// Pixels (RGBA8) whose largest channel difference is above `tolerance`.
fn differing(a: &[u8], b: &[u8], tolerance: u8) -> usize {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .filter(|(p, q)| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > tolerance))
        .count()
}

const TIME_CLASSIC_HINT: f32 = 0.0;
const WIND: [f32; 2] = [10.0, 0.0];

/// First time at which the gust field at the origin is `calm` (exactly 0) or above 0.8.
fn find_time(calm: bool) -> f32 {
    (0..100_000)
        .map(|i| TIME_CLASSIC_HINT + i as f32 * 0.05)
        .find(|t| {
            let g = wind_natural::gust_intensity([0.0, 0.0], [1.0, 0.0], *t);
            if calm { g == 0.0 } else { g > 0.8 }
        })
        .unwrap_or_else(|| panic!("no such moment"))
}

#[test]
fn natural_foliage_rests_in_calm_spells_and_moves_in_gusts() {
    let device = device();
    let fixture = Fixture::new();
    let render = |wind: [f32; 2], time: f32| fixture.render(&mut FrameRenderer::new(), &device, wind, time);
    let calm_t = find_time(true);
    let gust_t = find_time(false);
    let rest = render([0.0, 0.0], calm_t);
    let calm = render(WIND, calm_t);
    assert_eq!(
        differing(&rest, &calm, 0),
        0,
        "a calm spell must leave the card at rest"
    );
    // A sway moves the card only by what the gust field allows: probe a few gusty moments.
    let gusty = render(WIND, gust_t);
    let rest_g = render([0.0, 0.0], gust_t);
    assert!(
        differing(&gusty, &rest_g, 2) > 5,
        "a gust must move the card (t = {gust_t})"
    );
}
