// SPDX-License-Identifier: MPL-2.0
//! Host-driven LOD cross-fade on the runtime path: [`regia_render::MeshLodFade`] on a
//! [`regia_render::MeshRenderer`] entity reaches [`regia_render::FrameMesh::lod_dither`].
//! Two entities with opposite signs of the same `c` cover every pixel exactly once; `0` is the
//! old whole draw; a dithered draw leaves whole draws of the same mesh alone.

use regia_app::prelude::*;
use regia_render::{
    DirectionalLightUniform, ExtractedFrame, LabMaterial, LabMesh, LabScene, LabVertex, MeshLodFade,
    MeshRenderer, NotShadowCaster, RenderDevice, ShadowCast, extract_frame, spawn_lab_scene,
};
use regia_scene::{GlobalTransform, Transform};

const W: u32 = 160;
const H: u32 = 120;

/// A quad in the XY plane at z = 0, half-size `h`, facing +z.
fn quad(h: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let corners = [[-h, -h, 0.0], [h, -h, 0.0], [h, h, 0.0], [-h, h, 0.0]];
    let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let vertices = corners
        .into_iter()
        .zip(uvs)
        .map(|(position, uv)| LabVertex {
            position,
            normal: [0.0, 0.0, 1.0],
            uv,
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect();
    (vertices, vec![0, 1, 2, 0, 2, 3])
}

fn unlit(rgb: [f32; 3]) -> LabMaterial {
    LabMaterial {
        base_color: [rgb[0], rgb[1], rgb[2], 1.0],
        unlit: true,
        ..LabMaterial::default()
    }
}

/// One unlit quad of half-size `h` per entry of `colors`, all at the same place.
fn scene(h: f32, colors: &[[f32; 3]]) -> LabScene {
    let (vertices, indices) = quad(h);
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 45.0_f32.to_radians(),
        near: 0.1,
        far: 30.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.8,
        ambient_lux: 10_000.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.0, -1.0, -0.3], [1.0; 3], 100_000.0),
        local_lights: vec![],
        materials: colors.iter().map(|c| unlit(*c)).collect(),
        meshes: (0..colors.len())
            .map(|i| LabMesh {
                vertices: vertices.clone(),
                indices: indices.clone(),
                material: i,
                ..LabMesh::unskinned()
            })
            .collect(),
    }
}

const RED: [f32; 3] = [1.0, 0.0, 0.0];
const GREEN: [f32; 3] = [0.0, 1.0, 0.0];

/// The mesh entities in scene order. Entity ids are not allocated in spawn order, so each entity
/// is matched by its material color; the colors in a scene must differ.
fn mesh_entities(app: &mut App, scene: &LabScene) -> Vec<Entity> {
    let mut query = app.world_mut().query::<(Entity, &MeshRenderer)>();
    let found: Vec<_> = query
        .iter(app.world())
        .map(|(e, r)| (e, r.material.base_color))
        .collect();
    scene
        .meshes
        .iter()
        .map(|m| {
            let color = scene.materials[m.material].base_color;
            found
                .iter()
                .find(|(_, c)| *c == color)
                .expect("entity for mesh")
                .0
        })
        .collect()
}

fn render(device: &RenderDevice, app: &mut App) -> Vec<u8> {
    app.init_resource::<ExtractedFrame>();
    app.add_systems(Render, extract_frame);
    app.update();
    let mut extracted = app.world().resource::<ExtractedFrame>().clone();
    // No bloom, grading or fog: a pixel is the surface color or the background, nothing between.
    for view in &mut extracted.views {
        view.post = regia_render::PostSettings::golden_deterministic();
    }
    extracted.fog.enabled = false;
    let frame = extracted.frame_scene(0, W, H);
    regia_render::FrameRenderer::new()
        .render_offscreen(device, &frame)
        .expect("render")
        .pixels
}

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip mesh_lod_fade: {err}");
            None
        }
    }
}

/// Spawns `scene`, then applies `fades[i]` (if any) to the i-th mesh entity.
fn app_with(scene: &LabScene, fades: &[Option<f32>]) -> App {
    let mut app = App::new();
    spawn_lab_scene(app.world_mut(), scene);
    for (entity, fade) in mesh_entities(&mut app, scene).into_iter().zip(fades) {
        if let Some(c) = fade {
            app.world_mut().entity_mut(entity).insert(MeshLodFade(*c));
        }
    }
    app
}

fn px(pixels: &[u8], i: usize) -> [u8; 4] {
    [
        pixels[i * 4],
        pixels[i * 4 + 1],
        pixels[i * 4 + 2],
        pixels[i * 4 + 3],
    ]
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn two_entity_cross_fade_covers_every_pixel_exactly_once() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    // A quad that fills the view, red (outgoing) and green (incoming), over the sky.
    let s = scene(20.0, &[RED, GREEN]);
    let background = {
        let empty = scene(20.0, &[]);
        render(&device, &mut app_with(&empty, &[]))
    };
    let both = render(&device, &mut app_with(&s, &[Some(-0.5), Some(0.5)]));
    // Each LOD alone: the pixels it keeps are those that differ from the background.
    let outgoing = render(&device, &mut app_with(&scene(20.0, &[RED]), &[Some(-0.5)]));
    let incoming = render(&device, &mut app_with(&scene(20.0, &[GREEN]), &[Some(0.5)]));
    let n = (W * H) as usize;
    let (mut out_n, mut in_n) = (0, 0);
    for i in 0..n {
        let bg = px(&background, i);
        let o = px(&outgoing, i) != bg;
        let c = px(&incoming, i) != bg;
        assert!(
            o ^ c,
            "pixel {i}: out {o} {:?} in {c} {:?} bg {bg:?}",
            px(&outgoing, i),
            px(&incoming, i)
        );
        // The combined frame shows the LOD that owns the pixel (red outgoing, green incoming).
        let b = px(&both, i);
        assert!(
            if o { b[0] > b[1] } else { b[1] > b[0] },
            "pixel {i}: {b:?} (outgoing owns: {o})"
        );
        out_n += usize::from(o);
        in_n += usize::from(c);
    }
    // c = 0.5 splits the screen about evenly.
    let share = in_n as f32 / n as f32;
    assert!(
        (share - 0.5).abs() < 0.1,
        "incoming share {share} (out {out_n}, in {in_n})"
    );
}

#[test]
fn zero_fade_is_bit_identical_to_no_component() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let s = scene(0.8, &[RED]);
    let plain = render(&device, &mut app_with(&s, &[None]));
    let zero = render(&device, &mut app_with(&s, &[Some(0.0)]));
    assert_eq!(plain, zero);
}

#[test]
fn dithered_draw_leaves_whole_draws_of_the_same_mesh_alone() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let s = scene(0.5, &[RED]);
    // Two instances of one shared mesh: left whole, right (optionally) faded.
    let build = |c: Option<f32>| {
        let mut app = app_with(&s, &[None]);
        let base = mesh_entities(&mut app, &s)[0];
        let (renderer, transform) = {
            let e = app.world().entity(base);
            (
                e.get::<MeshRenderer>().cloned().expect("renderer"),
                *e.get::<Transform>().expect("transform"),
            )
        };
        let place = |x: f32| {
            let t = Transform {
                translation: regia_core::math::Vec3::new(x, 0.0, 0.0),
                ..transform
            };
            (t, GlobalTransform(t.compute_affine()))
        };
        let (t, g) = place(-0.8);
        app.world_mut().entity_mut(base).insert((t, g));
        let (t, g) = place(0.8);
        let right = app.world_mut().spawn((t, g, renderer)).id();
        if let Some(c) = c {
            app.world_mut().entity_mut(right).insert(MeshLodFade(c));
        }
        app
    };
    let whole = render(&device, &mut build(None));
    let mixed = render(&device, &mut build(Some(0.5)));
    let bg = px(&whole, 0);
    let (mut left_diff, mut kept, mut dropped) = (0, 0, 0);
    for y in 0..H as usize {
        for x in 0..W as usize {
            let i = y * W as usize + x;
            if x < W as usize / 2 {
                left_diff += usize::from(px(&whole, i) != px(&mixed, i));
            } else if px(&whole, i) != bg {
                if px(&mixed, i) == px(&whole, i) {
                    kept += 1;
                } else {
                    dropped += 1;
                }
            }
        }
    }
    assert_eq!(left_diff, 0, "the whole draw is untouched");
    assert!(
        kept > 100 && dropped > 100,
        "right half is dithered (kept {kept}, dropped {dropped})"
    );
}

#[test]
fn extraction_carries_the_fade_and_the_outgoing_lod_casts_no_shadow() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let s = scene(0.5, &[RED, GREEN, [0.0, 0.0, 1.0], [1.0, 1.0, 0.0]]);
    let mut app = app_with(&s, &[None, Some(0.25), Some(-0.25), Some(0.25)]);
    let entities = mesh_entities(&mut app, &s);
    app.world_mut().entity_mut(entities[3]).insert(NotShadowCaster);
    app.init_resource::<ExtractedFrame>();
    app.add_systems(Render, extract_frame);
    app.update();
    let frame = app.world().resource::<ExtractedFrame>();
    let by = |i: usize| {
        frame
            .meshes
            .iter()
            .find(|m| m.entity == entities[i])
            .expect("mesh")
    };
    assert_eq!((by(0).lod_dither, by(0).shadow), (0.0, ShadowCast::All));
    assert_eq!((by(1).lod_dither, by(1).shadow), (0.25, ShadowCast::All));
    assert_eq!((by(2).lod_dither, by(2).shadow), (-0.25, ShadowCast::Off));
    assert_eq!((by(3).lod_dither, by(3).shadow), (0.25, ShadowCast::Off));
    let scene = frame.frame_scene(0, W, H);
    let mut got: Vec<f32> = scene.meshes.iter().map(|m| m.lod_dither).collect();
    got.sort_by(f32::total_cmp);
    assert_eq!(got, vec![-0.25, 0.0, 0.25, 0.25]);
}
