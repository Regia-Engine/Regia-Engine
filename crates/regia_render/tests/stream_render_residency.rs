// SPDX-License-Identifier: MPL-2.0
//! The streaming `render` gate with residency (spec 78 § 10.3, ADR-2317): off, a cell is `visible`
//! as soon as its members' meshes resolved; on, only once the renderer drew them, so their meshes
//! and materials are on the GPU. Members an HLOD proxy hides are not waited for.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_render::{
    Camera, CpuMaterial, CpuMesh, LabVertex, MeshRenderer, MeshSource, RenderDevice, RenderPlugin,
    RenderTarget, StreamRenderResidency, TextureViewSettings,
};
use regia_scene::level::{LevelId, LevelMember};
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::{CellMember, CellRenderHidden, Readiness, StreamGates};
use regia_scene::{GlobalTransform, Transform};

/// One GPU test at a time.
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip stream_render_residency: {err}");
            None
        }
    }
}

fn quad() -> Arc<CpuMesh> {
    let corners = [
        [-1.0, -1.0, 0.0],
        [1.0, -1.0, 0.0],
        [1.0, 1.0, 0.0],
        [-1.0, 1.0, 0.0],
    ];
    let vertices = corners
        .into_iter()
        .map(|position| LabVertex {
            position,
            normal: [0.0, 0.0, 1.0],
            uv: [0.0, 0.0],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect();
    Arc::new(CpuMesh {
        vertices,
        indices: vec![0, 1, 2, 0, 2, 3],
    })
}

fn level() -> LevelId {
    LevelId(Guid::from_u128(0x5EED))
}

fn level_a() -> LevelId {
    LevelId(Guid::from_u128(0xA11A))
}

fn level_b() -> LevelId {
    LevelId(Guid::from_u128(0xB22B))
}

fn at(x: f32) -> (Transform, GlobalTransform) {
    let transform = Transform::from_translation(regia_core::math::Vec3::new(x, 0.0, -5.0));
    (transform, GlobalTransform(transform.compute_affine()))
}

/// An app that draws a 32² texture view of one mesh outside any cell, warmed until frames draw.
fn app(device: RenderDevice, residency: bool) -> App {
    let mut app = App::new();
    app.insert_resource(device);
    app.add_plugins((
        regia_app::MinimalPlugins,
        RenderPlugin { headless: true },
        regia_scene::ScenePlugin,
    ));
    app.world_mut().resource_mut::<StreamRenderResidency>().enabled = residency;
    let (transform, global) = at(0.0);
    app.world_mut().spawn((
        Transform::default(),
        GlobalTransform::default(),
        Camera {
            target: RenderTarget::Texture { asset_id: 0x5E },
            ..Camera::default()
        },
        TextureViewSettings {
            size: [32, 32],
            ..TextureViewSettings::default()
        },
    ));
    app.world_mut().spawn((
        transform,
        global,
        MeshRenderer::new(quad(), Arc::new(CpuMaterial::default())),
    ));
    for _ in 0..3 {
        app.update();
    }
    app
}

/// Spawns one member of `cell` with its own mesh and material, and tells the gates it spawned.
fn spawn_member(app: &mut App, cell: CellCoord, hidden: bool) {
    spawn_member_of(app, level(), cell, hidden, true);
}

/// Spawns one resolved member of `level_id` / `cell`. `drawn` places it where the camera extracts
/// it; without a transform the extractor never sees it, so residency treats it as not yet drawn.
fn spawn_member_of(app: &mut App, level_id: LevelId, cell: CellCoord, hidden: bool, drawn: bool) {
    let mut entity = app.world_mut().spawn((
        MeshRenderer::new(quad(), Arc::new(CpuMaterial::default())),
        CellMember(cell),
        LevelMember::from_level(level_id),
    ));
    if drawn {
        let (transform, global) = at(1.5);
        entity.insert((transform, global));
    }
    if hidden {
        entity.insert(CellRenderHidden);
    }
    app.world_mut()
        .resource_mut::<StreamGates>()
        .note_spawned(level_id, cell);
}

/// A mesh reference with no [`MeshRenderer`]: the render gate's unresolved set.
fn spawn_unresolved(app: &mut App, level_id: LevelId, cell: CellCoord) {
    app.world_mut().spawn((
        MeshSource::new("assets/props/unresolved.gltf", ""),
        CellMember(cell),
        LevelMember::from_level(level_id),
    ));
    app.world_mut()
        .resource_mut::<StreamGates>()
        .note_spawned(level_id, cell);
}

fn visible_at(app: &App, level_id: LevelId, cell: CellCoord) -> bool {
    app.world()
        .resource::<StreamGates>()
        .is_ready(level_id, cell, Readiness::Visible)
}

fn visible(app: &App, cell: CellCoord) -> bool {
    visible_at(app, level(), cell)
}

#[test]
fn off_a_cell_is_visible_as_soon_as_its_meshes_resolved() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let mut app = app(device, false);
    let cell = CellCoord::new(0, 0);
    spawn_member(&mut app, cell, false);
    app.update();
    assert!(visible(&app, cell));
    assert_eq!(app.world().resource::<StreamRenderResidency>().waits, 0);
}

#[test]
fn on_a_cell_waits_until_the_renderer_drew_its_members() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let mut app = app(device, true);
    let cell = CellCoord::new(0, 0);
    spawn_member(&mut app, cell, false);
    // The gate runs before this frame draws: the new mesh is not in the renderer's cache yet.
    app.update();
    assert!(!visible(&app, cell), "visible before its mesh was drawn");
    let residency = *app.world().resource::<StreamRenderResidency>();
    assert_eq!(residency.waiting_cells, 1);
    assert!(residency.waits >= 1);
    // The frame drew it; the next gate run marks the cell.
    for _ in 0..3 {
        app.update();
    }
    assert!(visible(&app, cell));
    let residency = *app.world().resource::<StreamRenderResidency>();
    assert_eq!(residency.waiting_cells, 0);
    assert_eq!(residency.timeouts, 0);
}

#[test]
fn on_hidden_members_are_not_waited_for() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let mut app = app(device, true);
    let cell = CellCoord::new(2, 0);
    spawn_member(&mut app, cell, true);
    app.update();
    assert!(
        visible(&app, cell),
        "a member behind a proxy cannot upload and must not hold the cell"
    );
    assert_eq!(app.world().resource::<StreamRenderResidency>().timeouts, 0);
}

/// Audit fix-render4 finding 1: additive levels share cell coordinates. An unresolved mesh on
/// level A must not keep level B's cell at that coordinate off `Visible`.
#[test]
fn an_unresolved_mesh_holds_only_its_own_level() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let mut app = app(device, true);
    let cell = CellCoord::new(0, 0);
    spawn_unresolved(&mut app, level_a(), cell);
    spawn_member_of(&mut app, level_b(), cell, false, true);
    for _ in 0..6 {
        app.update();
    }
    assert!(
        !visible_at(&app, level_a(), cell),
        "level A stays waiting while its mesh has no renderer"
    );
    assert!(
        visible_at(&app, level_b(), cell),
        "level B's resolved cell must not wait on level A's mesh"
    );
}

/// Audit fix-render4 finding 2: residency is per level and cell. Level B's member is never
/// extracted, so a coordinate-wide set would charge A's wait and, once that counter passed the
/// cap, mark a newly spawned member of A visible on its first frame.
#[test]
fn residency_wait_is_per_level_not_per_coordinate() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let mut app = app(device, true);
    let cell = CellCoord::new(0, 0);
    spawn_member_of(&mut app, level_a(), cell, false, true);
    spawn_member_of(&mut app, level_b(), cell, false, false);
    for _ in 0..8 {
        app.update();
    }
    let residency = *app.world().resource::<StreamRenderResidency>();
    assert!(
        visible_at(&app, level_a(), cell),
        "A is visible once its own members are resident"
    );
    assert_eq!(residency.timeouts, 0, "A must not spend the residency cap on B");
    assert!(!visible_at(&app, level_b(), cell), "B stays waiting");

    // Send A back to the gate and add a member the extractor has not seen. A counter already
    // past the cap (charged while A itself was resident) would mark A visible on this frame.
    let gate = app
        .world_mut()
        .resource_mut::<StreamGates>()
        .register("render", Readiness::Visible);
    app.world_mut()
        .resource_mut::<StreamGates>()
        .clear(gate, level_a(), cell);
    spawn_member_of(&mut app, level_a(), cell, false, false);
    app.update();
    assert!(
        !visible_at(&app, level_a(), cell),
        "a new member of A waits; the cap was not already spent by B"
    );
    assert_eq!(app.world().resource::<StreamRenderResidency>().timeouts, 0);
}
