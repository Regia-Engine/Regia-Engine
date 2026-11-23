// SPDX-License-Identifier: MPL-2.0
//! Shadow views per cell (v0.7 item E, ADR-8874, KI-RN-007): a caster in a cell no view reaches,
//! whose sun shadow can land on nothing a view sees, leaves the sun's cascades. No cover needed.
//!
//! The scene: a buried tube (cell 1, along Z, 10 m down) with a vertical mouth at z +20 onto a
//! trench (outdoors); a deeper gallery (cell 2, 30 m down) under it; a small upper gallery (cell 3)
//! between the tube's roof and the ground. The sun travels down and toward +X.
//!
//! - `in_tube_the_deep_gallery_leaves_every_cascade`: the eye in the tube; the deep gallery's chunk
//!   leaves every active sun cascade (the renderer's `sun_casters_per_cascade` is exactly one lower
//!   than with the cull off), the upper gallery's chunk, whose shadow falls into the tube, stays.
//! - `outdoors_nothing_leaves`: the eye outdoors culls nothing (the cull stands down).
//! - `through_the_mouth_the_deep_gallery_still_leaves`: the eye in the tube facing the mouth (the
//!   outdoors seen through one frustum): still culled, its shadow runs away from that frustum.
//! - `a_local_light_keeps_the_draw`: with a point light in the frame the draw stays, with no sun.
//! - `the_switch_turns_it_off`: `VisCellShadowViews { cull: false }`.
//! - `a_mouth_in_the_edge_slack_keeps_the_caster_that_shadows_only_that_band` and
//!   `a_texture_view_seeing_the_outdoors_only_through_the_slack_stands_the_cull_down` (ADR-10161):
//!   outdoors seen only through the terrain's two-pixel edge slack keeps its casters.
//! - `shadow_diff_unchanged_through_the_mouth`: ambient 0, EV 10, sun 3000 lux: the frame from the
//!   tube looking out of the mouth with the cull on is bit-identical to the no-caster frame (the deep
//!   chunk casting nothing) and to the chunk moved 300 m deeper, and the control box's shadow on the
//!   trench floor shows (the frame shows shadows).
//!
//! The ECS tests are CPU only; the frame tests need a headless device (lavapipe). No goldens.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_render::view_math::{look_at_view, look_target, usable_projection};
use regia_render::viscells::{
    CellBox, OUTDOOR_CELL, OutdoorPortals, SHADOW_VIEW_MARGIN_M, SHADOW_VIEW_SPREAD, SUN_SEALED_SHADOW,
    TraversalScratch, VisCell, VisCellCulled, VisCellShadowViews, VisCellStats, VisCells,
    cull_unreachable_cells, outdoor_portals, shadow_sweep_touches_box, shadow_sweep_touches_planes, traverse,
};
use regia_render::{
    Camera, CpuMaterial, CpuMesh, DirectionalLight, Environment, ExtractedFrame, FrameRenderer, LabVertex,
    LodViewport, MeshRenderer, PointLight, RenderDevice, RenderTarget, ShadowCast, TextureViewSettings,
    ViewVisibilityStats, extract_frame, update_view_visibility,
};
use regia_render::{Frustum, horizontal_plus_fov_y};
use regia_scene::Transform;
use regia_scene::propagate::propagate_transforms;

const CELLS: &str = "(
    version: 1,
    cells: [
        (id: 0, name: \"outdoor\", outdoor: true, bounds: []),
        (id: 1, name: \"tube\", bounds: [(center: (0.0, -10.0, 0.0), half_extents: (3.0, 3.0, 20.0))]),
        (id: 2, name: \"deep\", bounds: [(center: (0.0, -30.0, 0.0), half_extents: (3.0, 3.0, 10.0))]),
        (id: 3, name: \"upper\", bounds: [(center: (0.0, -3.5, 0.0), half_extents: (2.0, 1.5, 2.0))]),
    ],
    portals: [
        (a: 0, b: 1, quad: [(-3.0, -13.0, 20.0), (3.0, -13.0, 20.0), (3.0, -7.0, 20.0), (-3.0, -7.0, 20.0)]),
        (a: 0, b: 2, quad: [(-3.0, -33.0, -10.0), (3.0, -33.0, -10.0), (3.0, -27.0, -10.0), (-3.0, -27.0, -10.0)]),
    ],
)";

/// The sun's travel: down and toward +X.
const SUN: [f32; 3] = [0.3, -1.0, 0.0];

/// In the tube, looking down −Z (away from the mouth): only the tube is reached.
const TUBE_AWAY: (Vec3, f32, f32) = (Vec3::new(0.0, -10.0, 0.0), 0.0, 0.0);
/// In the tube at z 10, looking up +Z out of the mouth and a little down at the trench.
const TUBE_MOUTH: (Vec3, f32, f32) = (Vec3::new(0.0, -10.0, 10.0), std::f32::consts::PI, -0.25);
/// Above the ground, looking down at the origin.
const OUTDOORS: (Vec3, f32, f32) = (Vec3::new(0.0, 9.0, 12.0), 0.0, -0.6435);

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

/// An app with propagation in `PostUpdate` and extraction, the cell cull and the `ViewVisibility`
/// write in `Render`; one camera at `eye`; the sun; ambient 0. The frame is 96 × 64.
fn app(eye: (Vec3, f32, f32)) -> App {
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
    let (at, yaw, pitch) = eye;
    app.world_mut().spawn((
        Transform {
            translation: at,
            rotation: Quat::from_rotation_y(yaw) * Quat::from_rotation_x(pitch),
            scale: Vec3::ONE,
        },
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
    app.insert_resource(cells());
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

/// The trench floor outside the mouth (top at y −13, z 20 to 60), the control box on it, the
/// deep gallery's chunk and the upper gallery's chunk.
struct Scene {
    control: Entity,
    deep_chunk: Entity,
    upper_chunk: Entity,
}

fn scene(app: &mut App) -> Scene {
    mesh(
        app,
        Vec3::new(0.0, -13.5, 40.0),
        Vec3::new(20.0, 1.0, 40.0),
        [0.6, 0.6, 0.6, 1.0],
        None,
    );
    let control = mesh(
        app,
        Vec3::new(0.0, -12.0, 30.0),
        Vec3::splat(1.5),
        [0.8, 0.3, 0.2, 1.0],
        None,
    );
    let deep_chunk = mesh(
        app,
        Vec3::new(0.0, -30.0, 0.0),
        Vec3::new(4.0, 4.0, 10.0),
        [0.4, 0.4, 0.5, 1.0],
        Some(2),
    );
    let upper_chunk = mesh(
        app,
        Vec3::new(0.0, -3.5, 0.0),
        Vec3::splat(2.0),
        [0.4, 0.5, 0.4, 1.0],
        Some(3),
    );
    Scene {
        control,
        deep_chunk,
        upper_chunk,
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

fn built(eye: (Vec3, f32, f32), cull: bool) -> (App, Scene) {
    let mut app = app(eye);
    let scene = scene(&mut app);
    app.insert_resource(VisCellShadowViews { cull });
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

/// The brief's counter bar: casters per cascade with the eye inside the tube against the same
/// frame with the cull off.
#[test]
fn in_tube_the_deep_gallery_leaves_every_cascade() {
    let device = device();
    let (mut on, scene) = built(TUBE_AWAY, true);
    let (_, culled_counts) = render(&device, &mut on);
    let s = stats(&on);
    assert_eq!(s.reachable, vec![1], "the eye sees only the tube: {s:?}");
    assert!(s.shadow_view_active, "{s:?}");
    assert_eq!(
        s.shadow_view_tested, 2,
        "the deep and the upper gallery's chunks: {s:?}"
    );
    assert_eq!(s.shadow_view_culled_draws, 1, "the deep gallery's chunk: {s:?}");
    assert_eq!(
        s.shadow_view_dropped_draws, 1,
        "no local light: the shadow-only draw leaves the frame: {s:?}"
    );
    assert!(shadows_of(&on, scene.deep_chunk).is_empty());
    let upper = shadows_of(&on, scene.upper_chunk);
    assert!(!upper.is_empty(), "the upper gallery shadows the tube: it stays");
    assert!(upper.iter().all(|s| *s == ShadowCast::All), "{upper:?}");

    let (mut off, scene_off) = built(TUBE_AWAY, false);
    let (_, open_counts) = render(&device, &mut off);
    let s = stats(&off);
    assert!(!s.shadow_view_active && s.shadow_view_culled_draws == 0, "{s:?}");
    assert!(
        shadows_of(&off, scene_off.deep_chunk)
            .iter()
            .all(|s| s.casts_sun())
    );
    let active = open_counts.iter().filter(|c| **c > 0).count();
    assert!(active > 0, "the frame draws sun cascades: {open_counts:?}");
    eprintln!("sun casters per cascade, eye in the tube: cull off {open_counts:?}, on {culled_counts:?}");
    for c in 0..active {
        assert_eq!(
            open_counts[c],
            culled_counts[c] + 1,
            "cascade {c}: the deep chunk left it ({open_counts:?} vs {culled_counts:?})"
        );
    }
}

/// An eye outdoors sees the outdoors unrestricted: nothing leaves, the counts match the cull off.
#[test]
fn outdoors_nothing_leaves() {
    let device = device();
    let (mut on, scene) = built(OUTDOORS, true);
    let (_, on_counts) = render(&device, &mut on);
    let s = stats(&on);
    assert!(s.reachable.contains(&0), "{s:?}");
    assert!(!s.shadow_view_active, "{s:?}");
    assert_eq!(s.shadow_view_culled_draws, 0);
    assert!(
        shadows_of(&on, scene.deep_chunk)
            .iter()
            .all(|s| *s == ShadowCast::All)
    );
    let (mut off, _) = built(OUTDOORS, false);
    let (_, off_counts) = render(&device, &mut off);
    eprintln!("sun casters per cascade, eye outdoors: cull off {off_counts:?}, on {on_counts:?}");
    assert_eq!(on_counts, off_counts);
}

/// The eye in the tube facing the mouth sees the outdoors through one frustum; the deep gallery's
/// shadow runs down and away from it, so it still leaves; the upper gallery's stays.
#[test]
fn through_the_mouth_the_deep_gallery_still_leaves() {
    let (mut app, scene) = built(TUBE_MOUTH, true);
    app.update();
    let s = stats(&app);
    assert_eq!(s.reachable, vec![0, 1], "{s:?}");
    assert!(
        s.shadow_view_active,
        "the outdoors is seen through the mouth only: {s:?}"
    );
    assert_eq!(s.shadow_view_culled_draws, 1, "{s:?}");
    assert!(shadows_of(&app, scene.deep_chunk).is_empty());
    assert!(
        shadows_of(&app, scene.upper_chunk)
            .iter()
            .all(|s| *s == ShadowCast::All)
    );
}

/// A culled draw keeps its local-light shadows: with a point light in the frame it stays, with no
/// sun cascade.
#[test]
fn a_local_light_keeps_the_draw() {
    let (mut app, scene) = built(TUBE_AWAY, true);
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -9.0, -5.0)),
        PointLight {
            radius: 8.0,
            ..PointLight::default()
        },
        VisCell(1),
    ));
    app.update();
    let shadows = shadows_of(&app, scene.deep_chunk);
    assert!(!shadows.is_empty(), "the chunk stays in the frame");
    assert!(shadows.iter().all(|s| *s == SUN_SEALED_SHADOW), "{shadows:?}");
    let s = stats(&app);
    assert_eq!(
        (s.shadow_view_culled_draws, s.shadow_view_dropped_draws),
        (1, 0),
        "{s:?}"
    );
}

/// `VisCellShadowViews { cull: false }` turns the cull off; a frame with no sun shadow does not run
/// it either.
#[test]
fn the_switch_turns_it_off() {
    let (mut app, scene) = built(TUBE_AWAY, false);
    app.update();
    assert!(!stats(&app).shadow_view_active);
    assert!(shadows_of(&app, scene.deep_chunk).iter().all(|s| s.casts_sun()));
    app.insert_resource(VisCellShadowViews { cull: true });
    app.update();
    assert_eq!(stats(&app).shadow_view_culled_draws, 1);
    let mut suns = app.world_mut().query::<&mut DirectionalLight>();
    suns.single_mut(app.world_mut()).expect("one sun").cast_shadows = false;
    app.update();
    let s = stats(&app);
    assert!(!s.shadow_view_active && s.shadow_view_culled_draws == 0, "{s:?}");
}

/// The memory rule for GPU shadow tests: ambient 0, a low EV, a control caster, a diff against a
/// no-caster frame. The view from the tube out of the mouth with the cull on is bit-identical to the
/// same view where the deep chunk casts no shadow (`NotShadowCaster`) and to one where it lies 300 m
/// deeper: the cull removes exactly a shadow nobody sees. The control box's own shadow shows (the
/// frame without it differs), so the frames do show shadows.
///
/// The frame with the cull off differs from these by a few bytes of at most 3 (printed): an extra
/// caster anywhere inside a cascade's footprint changes the unshadowed texels' filtering a little,
/// wherever it lies (moving it 300 m deeper removes the change too). ADR-8874 records the number.
#[test]
fn shadow_diff_unchanged_through_the_mouth() {
    let device = device();
    let (mut on, _) = built(TUBE_MOUTH, true);
    let (culled, on_counts) = render(&device, &mut on);
    assert_eq!(stats(&on).shadow_view_culled_draws, 1, "the cull ran");
    // The no-caster frame: the cull off, the deep chunk casting no shadow.
    let (mut no_cast, scene) = built(TUBE_MOUTH, false);
    no_cast
        .world_mut()
        .entity_mut(scene.deep_chunk)
        .insert(regia_render::NotShadowCaster);
    let (no_caster, _) = render(&device, &mut no_cast);
    assert_eq!(
        differing(&culled, &no_caster),
        0,
        "the cull equals the frame where the deep chunk casts nothing"
    );
    // The deep chunk 300 m deeper, still casting with the cull off: its shadow cannot reach the view.
    let (mut deeper, scene) = built(TUBE_MOUTH, false);
    deeper.world_mut().entity_mut(scene.deep_chunk).insert(Transform {
        translation: Vec3::new(0.0, -300.0, 0.0),
        rotation: Quat::IDENTITY,
        scale: Vec3::new(4.0, 4.0, 10.0),
    });
    let (moved, _) = render(&device, &mut deeper);
    assert_eq!(
        differing(&culled, &moved),
        0,
        "the cull equals the chunk moved out of reach"
    );
    // Control: the same world drawn twice by fresh renderers is the same frame.
    let (mut open_app, _) = built(TUBE_MOUTH, false);
    let (open, off_counts) = render(&device, &mut open_app);
    let (mut again, _) = built(TUBE_MOUTH, false);
    let (open_again, _) = render(&device, &mut again);
    assert_eq!(differing(&open, &open_again), 0, "rendering is deterministic");
    let max_delta = culled
        .iter()
        .zip(&open)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    eprintln!(
        "through the mouth, cull on against cull off: {} of {} bytes differ (max delta {max_delta}); \
         sun casters on {on_counts:?}, off {off_counts:?}",
        differing(&culled, &open),
        open.len()
    );
    // The control box's shadow: the same frame with the box casting none differs.
    let (mut shadowless, scene) = built(TUBE_MOUTH, true);
    shadowless
        .world_mut()
        .entity_mut(scene.control)
        .insert(regia_render::NotShadowCaster);
    let (no_control_shadow, _) = render(&device, &mut shadowless);
    let shadowed = differing(&culled, &no_control_shadow);
    assert!(
        shadowed > 64,
        "the control box's shadow shows: {shadowed} bytes differ"
    );
}

/// The outdoor walk looks two pixels past the screen (`OUTDOOR_EDGE_SLACK_PX`), so a mouth the
/// narrow frustum clips out can still record a restricted outdoor frustum. Terrain, grass,
/// impostors and water draw in that band. A rigid caster whose sun shadow lands only there must
/// stay in the cascades (ADR-8874). An 8×8 view makes the two-pixel slack half the narrow
/// tangent, so the mouth below sits past the narrow edge and inside the widened one.
#[test]
fn a_mouth_in_the_edge_slack_keeps_the_caster_that_shadows_only_that_band() {
    // Eye at (0, 1, 0) looking down −Z. The mouth is a 2 m × 1.2 m window at z −10, x 10..12:
    // horizontal-plus at 8×8 puts the narrow edge near tan 0.86 and the widened edge near tan
    // 1.29, and this window is tan 1.0..1.2.
    let cells = VisCells::from_ron(
        "(
            version: 1,
            cells: [
                (id: 0, name: \"outdoor\", outdoor: true, bounds: []),
                (id: 1, name: \"tube\", bounds: [(center: (0.0, 1.0, 2.0), half_extents: (15.0, 4.0, 8.0))]),
                (id: 2, name: \"side\", bounds: [(center: (80.0, 1.0, -40.0), half_extents: (2.0, 2.0, 2.0))]),
            ],
            portals: [
                (a: 0, b: 1, quad: [(10.0, 0.4, -10.0), (12.0, 0.4, -10.0), (12.0, 1.6, -10.0), (10.0, 1.6, -10.0)]),
            ],
        )",
    )
    .expect("the slack-band cells load");
    let eye = Vec3::new(0.0, 1.0, 0.0);
    let travel = Vec3::new(0.0, -1.0, 0.0);
    let tube = CellBox {
        center: [0.0, 1.0, 2.0],
        half_extents: [15.0, 4.0, 8.0],
        rotation_radians: 0.0,
    };
    let frustum = slack_view_frustum(eye);
    let mut scratch = TraversalScratch::default();
    traverse(&cells, eye, frustum, &mut scratch);
    let outdoor = cells.index_of(OUTDOOR_CELL).expect("cell 0");
    assert!(
        !scratch.reachable().get(outdoor).copied().unwrap_or(true),
        "the narrow walk must miss the mouth"
    );
    let mut wide = OutdoorPortals::default();
    outdoor_portals(&cells, eye.to_array(), frustum, [8, 8], &mut scratch, &mut wide);
    assert!(
        wide.is_restricted() && wide.frusta().len() == 1,
        "the two-pixel slack still records the mouth: {wide:?}"
    );
    let at = caster_on_the_slack_ground(&wide, &tube, travel);
    let mut app = app((eye, 0.0, 0.0));
    app.insert_resource(LodViewport { width: 8, height: 8 });
    app.insert_resource(cells);
    // A vertical sun, so the sweep stays on the caster's column and cannot walk into the tube.
    {
        let mut suns = app.world_mut().query::<&mut DirectionalLight>();
        suns.single_mut(app.world_mut()).expect("one sun").direction = [0.0, -1.0, 0.0];
    }
    let caster = mesh(&mut app, at, Vec3::splat(0.8), [0.7, 0.4, 0.2, 1.0], Some(2));
    app.insert_resource(VisCellShadowViews { cull: true });
    app.update();
    let s = stats(&app);
    assert_eq!(
        s.reachable,
        vec![1],
        "the narrow union still misses the outdoors: {s:?}"
    );
    let recorded = app
        .world()
        .resource::<ExtractedFrame>()
        .outdoor_portal_views
        .get(0)
        .map(|list| list.frusta().len())
        .unwrap_or(0);
    assert_eq!(recorded, 1, "the frame kept the slack frustum");
    assert!(s.shadow_view_active, "the shadow-view cull ran: {s:?}");
    let got = shadows_of(&app, caster);
    assert!(
        !got.is_empty() && got.iter().all(|cast| cast.casts_sun()),
        "the outdoor ground in the slack band is drawn, so this caster stays in the sun cascades: {got:?} stats {s:?}"
    );
}

/// The same slack band seen by a render-to-texture view (ADR-10161): a texture view records no
/// outdoor frusta, so outdoors it sees only through the terrain's two-pixel slack still stands the
/// cull down. The window camera at the same eye looks the other way (it sees no outdoors), the
/// 8×8 texture camera looks at the mouth its narrow walk misses.
#[test]
fn a_texture_view_seeing_the_outdoors_only_through_the_slack_stands_the_cull_down() {
    let cells = VisCells::from_ron(
        "(
            version: 1,
            cells: [
                (id: 0, name: \"outdoor\", outdoor: true, bounds: []),
                (id: 1, name: \"tube\", bounds: [(center: (0.0, 1.0, 2.0), half_extents: (15.0, 4.0, 8.0))]),
                (id: 2, name: \"side\", bounds: [(center: (80.0, 1.0, -40.0), half_extents: (2.0, 2.0, 2.0))]),
            ],
            portals: [
                (a: 0, b: 1, quad: [(10.0, 0.4, -10.0), (12.0, 0.4, -10.0), (12.0, 1.6, -10.0), (10.0, 1.6, -10.0)]),
            ],
        )",
    )
    .expect("the slack-band cells load");
    let eye = Vec3::new(0.0, 1.0, 0.0);
    let travel = Vec3::new(0.0, -1.0, 0.0);
    let tube = CellBox {
        center: [0.0, 1.0, 2.0],
        half_extents: [15.0, 4.0, 8.0],
        rotation_radians: 0.0,
    };
    let frustum = slack_view_frustum(eye);
    let mut scratch = TraversalScratch::default();
    traverse(&cells, eye, frustum, &mut scratch);
    let outdoor = cells.index_of(OUTDOOR_CELL).expect("cell 0");
    assert!(
        !scratch.reachable().get(outdoor).copied().unwrap_or(true),
        "the texture view's narrow walk must miss the mouth"
    );
    let mut wide = OutdoorPortals::default();
    outdoor_portals(&cells, eye.to_array(), frustum, [8, 8], &mut scratch, &mut wide);
    assert!(
        wide.is_restricted() && wide.frusta().len() == 1,
        "its two-pixel slack records the mouth: {wide:?}"
    );
    let at = caster_on_the_slack_ground(&wide, &tube, travel);
    // The window camera at the same eye looks up +Z, away from the mouth.
    let mut app = app((eye, std::f32::consts::PI, 0.0));
    app.insert_resource(LodViewport { width: 8, height: 8 });
    app.insert_resource(cells);
    {
        let mut suns = app.world_mut().query::<&mut DirectionalLight>();
        suns.single_mut(app.world_mut()).expect("one sun").direction = [0.0, -1.0, 0.0];
    }
    const TEXTURE: u64 = 0x5ac7;
    app.world_mut().spawn((
        Transform {
            translation: eye,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        },
        Camera {
            fov_y: 0.9,
            exposure_ev: Some(10.0),
            target: RenderTarget::Texture { asset_id: TEXTURE },
            ..Camera::default()
        },
        TextureViewSettings {
            size: [8, 8],
            ..TextureViewSettings::default()
        },
    ));
    let caster = mesh(&mut app, at, Vec3::splat(0.8), [0.7, 0.4, 0.2, 1.0], Some(2));
    app.insert_resource(VisCellShadowViews { cull: true });
    app.update();
    let extracted = app.world().resource::<ExtractedFrame>();
    assert_eq!(extracted.views.len(), 2, "the window and the texture view");
    assert_eq!(extracted.texture_views().len(), 1, "one render-to-texture view");
    let s = stats(&app);
    assert_eq!(
        s.reachable,
        vec![1],
        "neither narrow walk reaches the outdoors: {s:?}"
    );
    assert!(
        !s.shadow_view_active,
        "the texture view sees outdoors through its slack: the cull stands down: {s:?}"
    );
    let got = shadows_of(&app, caster);
    assert!(
        !got.is_empty() && got.iter().all(|cast| cast.casts_sun()),
        "the ground the texture view draws in its slack keeps this caster's shadow: {got:?} stats {s:?}"
    );
}

/// The frustum `cull_unreachable_cells` builds for an 8×8 window at `eye` looking down −Z,
/// fov 0.9, the camera defaults for near and far.
fn slack_view_frustum(eye: Vec3) -> [[f32; 4]; 6] {
    let aspect = 1.0;
    let fov_y = horizontal_plus_fov_y(0.9, aspect);
    let (fov, near, far) = usable_projection(fov_y, 0.1, 1000.0);
    let target = look_target(eye, -Vec3::Z);
    let view = look_at_view(eye, target, Vec3::Y);
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(fov, aspect, near, far);
    Frustum::from_view_proj(&(proj * view)).planes()
}

/// A 0.8 m cube on the outdoor ground the slack frustum admits, clear of the tube's box.
fn caster_on_the_slack_ground(wide: &OutdoorPortals, tube: &CellBox, travel: Vec3) -> Vec3 {
    let eye = Vec3::new(0.0, 1.0, 0.0);
    let mouth = Vec3::new(11.0, 1.0, -10.0);
    let dir = (mouth - eye).normalize_or_zero();
    for steps in [2.0_f32, 2.5, 3.0, 4.0, 5.0] {
        let at = eye + dir * (mouth - eye).length() * steps;
        let lo = (at - Vec3::splat(0.4)).to_array();
        let hi = (at + Vec3::splat(0.4)).to_array();
        let on_ground = wide.frusta().iter().any(|frustum| {
            shadow_sweep_touches_planes(lo, hi, travel, frustum, SHADOW_VIEW_MARGIN_M, SHADOW_VIEW_SPREAD)
        });
        let on_tube =
            shadow_sweep_touches_box(lo, hi, travel, tube, SHADOW_VIEW_MARGIN_M, SHADOW_VIEW_SPREAD);
        if on_ground && !on_tube {
            return at;
        }
    }
    panic!("no caster sits in the slack frustum and clear of the tube");
}
