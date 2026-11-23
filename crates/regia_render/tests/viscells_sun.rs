// SPDX-License-Identifier: MPL-2.0
//! Sun sealing, stage 3 of indoor cells (ADR-8414, plan row W-23): the draws of a cell the sun
//! cannot reach through any portal leave the sun's cascades, when the host vouches that the cell
//! lies under cover ([`SunCover`]).
//!
//! The scene: open ground (a slab, top at y 0) with an outdoor control box on it; a buried tube
//! (cell 1, 10 m down, along Z) whose only portal is a vertical mouth at z +20; a pit (cell 2, at
//! x +20) whose portal is a horizontal opening in its roof. The sun travels down and toward +X, so
//! the vertical mouth is edge-on to it (cell 1 is sun-sealed) and the roof opening lets it into
//! the pit (cell 2 is sun-reached).
//!
//! - `buried_chunk_casts_no_cascade`: the tube's chunk leaves every sun cascade's caster count
//!   (the renderer's `sun_casters_per_cascade`), and the cull's counters say so.
//! - `a_portal_lit_chunk_still_casts`: the pit's chunk keeps `ShadowCast::All`.
//! - `shadow_diff_unchanged_outdoors`: at ambient 0 and EV 10, the frame of an outdoor view is
//!   bit-identical with the seal on and off, and differs from the frame without the control box
//!   (so the test sees shadows).
//!
//! The ECS tests are CPU only; the frame tests need a headless device (lavapipe). No goldens.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_render::viscells::{
    SUN_SEALED_SHADOW, SunCover, VisCell, VisCellCulled, VisCellStats, VisCells, cull_unreachable_cells,
};
use regia_render::{
    Camera, CpuMaterial, CpuMesh, DirectionalLight, Environment, ExtractedFrame, FrameRenderer, LabVertex,
    LodViewport, MeshRenderer, PointLight, RenderDevice, ShadowCast, ViewVisibilityStats, extract_frame,
    update_view_visibility,
};
use regia_scene::Transform;
use regia_scene::components::Visibility;
use regia_scene::propagate::propagate_transforms;

const CELLS: &str = "(
    version: 1,
    cells: [
        (id: 0, name: \"outdoor\", outdoor: true, bounds: []),
        (id: 1, name: \"tube\", bounds: [(center: (0.0, -10.0, 0.0), half_extents: (3.0, 3.0, 20.0))]),
        (id: 2, name: \"pit\", bounds: [(center: (20.0, -10.0, 0.0), half_extents: (3.0, 3.0, 3.0))]),
    ],
    portals: [
        (a: 0, b: 1, quad: [(-3.0, -13.0, 20.0), (3.0, -13.0, 20.0), (3.0, -7.0, 20.0), (-3.0, -7.0, 20.0)]),
        (a: 0, b: 2, quad: [(18.0, -7.0, -2.0), (22.0, -7.0, -2.0), (22.0, -7.0, 2.0), (18.0, -7.0, 2.0)]),
    ],
)";

/// The sun's travel: down and toward +X. Edge-on to the tube's mouth (a Z plane).
const SUN: [f32; 3] = [0.3, -1.0, 0.0];

fn cells() -> VisCells {
    VisCells::from_ron(CELLS).expect("the scene's cells load")
}

/// A unit cube (±0.5 m) around the origin.
fn cube() -> Arc<CpuMesh> {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
    ];
    for (n, u, v) in faces {
        let base = vertices.len() as u32;
        for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let p = [
                0.5 * (n[0] + su * u[0] + sv * v[0]),
                0.5 * (n[1] + su * u[1] + sv * v[1]),
                0.5 * (n[2] + su * u[2] + sv * v[2]),
            ];
            vertices.push(LabVertex::from_pos_normal_uv(
                p,
                n,
                [0.5 * (su + 1.0), 0.5 * (sv + 1.0)],
            ));
        }
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    Arc::new(CpuMesh { vertices, indices })
}

fn material(color: [f32; 4]) -> Arc<CpuMaterial> {
    Arc::new(CpuMaterial {
        base_color: color,
        ..CpuMaterial::default()
    })
}

/// An app with propagation in `PostUpdate` and extraction, the cell cull and the
/// `ViewVisibility` write in `Render`; an outdoor camera above the ground looking down at the origin; the
/// sun; ambient 0. The frame is 96 × 64.
fn app() -> App {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<ViewVisibilityStats>();
    app.init_resource::<VisCellStats>();
    app.init_resource::<VisCellCulled>();
    app.insert_resource(Environment {
        ibl: false,
        ambient_lux: 0.0,
        double_sided: false,
    });
    app.insert_resource(LodViewport {
        width: 96,
        height: 64,
    });
    app.add_systems(PostUpdate, propagate_transforms);
    app.add_systems(
        Render,
        (extract_frame, cull_unreachable_cells, update_view_visibility).chain(),
    );
    // From (0, 9, 12) toward the origin: a pure pitch down (the camera looks down −Z).
    let look = Transform {
        translation: Vec3::new(0.0, 9.0, 12.0),
        rotation: Quat::from_rotation_x(-(9.0f32).atan2(12.0)),
        scale: Vec3::ONE,
    };
    app.world_mut().spawn((
        look,
        Camera {
            fov_y: 0.9,
            exposure_ev: Some(10.0),
            ..Camera::default()
        },
    ));
    app.world_mut().spawn(DirectionalLight {
        direction: Vec3::from_array(SUN).normalize().to_array(),
        color: [1.0; 3],
        illuminance: 3000.0,
        cast_shadows: true,
        static_shadow: false,
    });
    app
}

fn mesh(app: &mut App, at: Vec3, scale: Vec3, color: [f32; 4], cell: Option<u16>) -> Entity {
    let mut entity = app.world_mut().spawn((
        Transform {
            translation: at,
            rotation: Quat::IDENTITY,
            scale,
        },
        MeshRenderer::new(cube(), material(color)),
    ));
    if let Some(cell) = cell {
        entity.insert(VisCell(cell));
    }
    entity.id()
}

/// The ground slab (top at y 0), the outdoor control box, the tube's chunk and the pit's chunk.
struct Scene {
    control: Entity,
    tube_chunk: Entity,
    pit_chunk: Entity,
}

fn scene(app: &mut App) -> Scene {
    mesh(
        app,
        Vec3::new(0.0, -0.5, 0.0),
        Vec3::new(60.0, 1.0, 60.0),
        [0.6, 0.6, 0.6, 1.0],
        None,
    );
    let control = mesh(
        app,
        Vec3::new(1.0, 1.0, 2.0),
        Vec3::splat(1.5),
        [0.8, 0.3, 0.2, 1.0],
        None,
    );
    let tube_chunk = mesh(
        app,
        Vec3::new(0.0, -10.0, 0.0),
        Vec3::new(5.0, 5.0, 30.0),
        [0.4, 0.4, 0.5, 1.0],
        Some(1),
    );
    let pit_chunk = mesh(
        app,
        Vec3::new(20.0, -11.0, 0.0),
        Vec3::new(4.0, 2.0, 4.0),
        [0.4, 0.5, 0.4, 1.0],
        Some(2),
    );
    Scene {
        control,
        tube_chunk,
        pit_chunk,
    }
}

fn stats(app: &App) -> VisCellStats {
    app.world().resource::<VisCellStats>().clone()
}

/// Every draw of `entity` in the extracted frame, with its shadow.
fn shadows_of(app: &App, entity: Entity) -> Vec<ShadowCast> {
    app.world()
        .resource::<ExtractedFrame>()
        .meshes
        .iter()
        .filter(|m| m.entity == entity)
        .map(|m| m.shadow)
        .collect()
}

fn sealed_app(cover: bool) -> (App, Scene) {
    let mut app = app();
    let scene = scene(&mut app);
    app.insert_resource(cells());
    if cover {
        app.insert_resource(SunCover::new(
            [1, 2],
            Vec3::from_array(SUN).normalize().to_array(),
        ));
    }
    (app, scene)
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// The world's first frame by a fresh renderer, and that renderer's sun caster counts.
fn render(device: &RenderDevice, app: &mut App) -> (Vec<u8>, [u32; 4]) {
    app.update();
    let extracted = app.world().resource::<ExtractedFrame>();
    let scene = extracted.frame_scene(0, 96, 64);
    let mut renderer = FrameRenderer::new();
    let pixels = renderer.render_offscreen(device, &scene).expect("frame").pixels;
    (pixels, renderer.shadow_cache_report().sun_casters_per_cascade)
}

fn differing(a: &[u8], b: &[u8]) -> usize {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).filter(|(x, y)| x != y).count()
}

/// Plan row W-23's acceptance: a layer chunk fully inside a sealed cell is in no sun cascade's
/// caster list.
#[test]
fn buried_chunk_casts_no_cascade() {
    let device = device();
    let (mut on, scene) = sealed_app(true);
    let (_, sealed_counts) = render(&device, &mut on);
    let s = stats(&on);
    assert_eq!(
        s.sun_reached,
        vec![0, 2],
        "the pit is lit, the tube is not: {s:?}"
    );
    assert_eq!(s.sun_sealed_cells, 1, "{s:?}");
    assert_eq!(s.sun_sealed_draws, 1, "the tube's chunk: {s:?}");
    for shadow in shadows_of(&on, scene.tube_chunk) {
        assert!(!shadow.casts_sun(), "{shadow:?}");
    }

    let (mut off, _) = sealed_app(false);
    let (_, open_counts) = render(&device, &mut off);
    assert_eq!(stats(&off).sun_sealed_draws, 0, "no cover, no seal");
    let active = open_counts.iter().filter(|c| **c > 0).count();
    assert!(active > 0, "the frame draws sun cascades: {open_counts:?}");
    for c in 0..active {
        assert_eq!(
            open_counts[c],
            sealed_counts[c] + 1,
            "cascade {c}: the tube's chunk left it ({open_counts:?} vs {sealed_counts:?})"
        );
    }
}

/// Plan row W-23: a chunk the sun reaches through an open mouth stays a caster.
#[test]
fn a_portal_lit_chunk_still_casts() {
    let (mut app, scene) = sealed_app(true);
    app.update();
    let shadows = shadows_of(&app, scene.pit_chunk);
    assert!(!shadows.is_empty(), "the pit's chunk is in the frame");
    for shadow in shadows {
        assert_eq!(shadow, ShadowCast::All, "lit through its roof opening");
    }
    // Close the opening's view of the sun: a sun travelling along X sees the opening edge-on.
    let mut suns = app.world_mut().query::<&mut DirectionalLight>();
    suns.single_mut(app.world_mut()).expect("one sun").direction = [1.0, 0.0, 0.0];
    app.insert_resource(SunCover::new([1, 2], [1.0, 0.0, 0.0]));
    app.update();
    let s = stats(&app);
    assert_eq!(s.sun_reached, vec![0], "{s:?}");
    assert_eq!(s.sun_sealed_cells, 2);
    assert!(shadows_of(&app, scene.pit_chunk).iter().all(|s| !s.casts_sun()));
}

/// The memory rule for GPU shadow tests: ambient 0, a low EV, a control caster, a diff against a
/// no-caster frame. The outdoor view is bit-identical with the seal on and off.
#[test]
fn shadow_diff_unchanged_outdoors() {
    let device = device();
    let (mut on, _) = sealed_app(true);
    let (sealed, _) = render(&device, &mut on);
    assert_eq!(stats(&on).sun_sealed_draws, 1, "the seal ran");
    let (mut off, _) = sealed_app(false);
    let (open, _) = render(&device, &mut off);
    assert_eq!(
        differing(&sealed, &open),
        0,
        "the outdoor frame does not change when the buried chunk leaves the cascades"
    );
    // The control box casts: hide it and the frame changes, so the frames above show shadows.
    let (mut bare, scene) = sealed_app(true);
    bare.world_mut()
        .entity_mut(scene.control)
        .insert(Visibility::Hidden);
    let (no_caster, _) = render(&device, &mut bare);
    let shadowed = differing(&open, &no_caster);
    assert!(
        shadowed > 64,
        "the control box's shadow shows: {shadowed} bytes differ"
    );
}

/// A cover computed for another sun vouches for nothing.
#[test]
fn a_cover_for_another_sun_seals_nothing() {
    let (mut app, scene) = sealed_app(false);
    app.insert_resource(SunCover::new([1, 2], [0.0, -1.0, 0.0]));
    app.update();
    let s = stats(&app);
    assert_eq!(s.sun_sealed_draws, 0, "{s:?}");
    assert!(s.sun_reached.is_empty());
    assert!(shadows_of(&app, scene.tube_chunk).iter().all(|s| s.casts_sun()));
}

/// A draw that sticks out of its cell's box keeps casting: it might be the cover itself.
#[test]
fn a_chunk_poking_out_of_its_box_keeps_casting() {
    let (mut app, scene) = sealed_app(true);
    app.world_mut().entity_mut(scene.tube_chunk).insert(Transform {
        translation: Vec3::new(0.0, -8.0, 0.0),
        rotation: Quat::IDENTITY,
        scale: Vec3::new(5.0, 5.0, 30.0),
    });
    app.update();
    assert_eq!(stats(&app).sun_sealed_draws, 0);
    assert!(shadows_of(&app, scene.tube_chunk).iter().all(|s| s.casts_sun()));
}

/// A sealed draw keeps its local-light shadows: with a point light in the frame it stays, with no
/// sun cascade.
#[test]
fn a_sealed_draw_keeps_local_light_shadows() {
    let (mut app, scene) = sealed_app(true);
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -9.0, 5.0)),
        PointLight {
            radius: 8.0,
            ..PointLight::default()
        },
        VisCell(1),
    ));
    app.update();
    let shadows = shadows_of(&app, scene.tube_chunk);
    assert!(!shadows.is_empty(), "the chunk stays in the frame");
    assert!(shadows.iter().all(|s| *s == SUN_SEALED_SHADOW), "{shadows:?}");
    assert_eq!(stats(&app).sun_dropped_draws, 0);
}
