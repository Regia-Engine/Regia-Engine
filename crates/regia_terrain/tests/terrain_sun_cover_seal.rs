// SPDX-License-Identifier: MPL-2.0
//! The heightfield fills the sun cover end to end (v0.7 item E, ADR-8874, KI-RN-004): with
//! `TerrainSunCover` inserted and no host cover, a buried tube under a flat terrain is sun-sealed
//! and its chunk leaves the sun's cascades; a hole over the tube, or no opt-in, seals nothing.
//!
//! The scene: a flat 128 m terrain at height 0; a tube (cell 1, 10 m down, along Z) whose only
//! portal is a vertical mouth at z +10, edge-on to a sun straight down; the tube's chunk; an outdoor
//! camera. CPU only (no device): extraction, the terrain's cover and the cell cull in `Render`.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_render::viscells::{
    SunCover, VisCell, VisCellCulled, VisCellStats, VisCells, cull_unreachable_cells,
};
use regia_render::{
    Camera, CpuMaterial, CpuMesh, DirectionalLight, ExtractedFrame, LabVertex, LodViewport, MeshRenderer,
    ViewVisibilityStats, extract_frame,
};
use regia_scene::Transform;
use regia_scene::propagate::propagate_transforms;
use regia_terrain::holes::{HoleEllipse, HolesMask};
use regia_terrain::{
    CdlodConfig, Terrain, TerrainLayers, TerrainMap, TerrainSunCover, TerrainSunCoverStats,
    update_terrain_sun_cover,
};

const CELLS: &str = "(
    cells: [
        (id: 1, name: \"tube\", bounds: [(center: (0.0, -10.0, 0.0), half_extents: (3.0, 3.0, 10.0))]),
    ],
    portals: [
        (a: 0, b: 1, quad: [(-3.0, -13.0, 10.0), (3.0, -13.0, 10.0), (3.0, -7.0, 10.0), (-3.0, -7.0, 10.0)]),
    ],
)";

fn flat() -> Terrain {
    let n = 33u32;
    let map = TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], 128.0, 10.0, 0.0).expect("map");
    Terrain::new(
        &map.to_heightfield(),
        128.0,
        10.0,
        0.0,
        CdlodConfig::default(),
        4,
        TerrainLayers::default(),
    )
    .expect("terrain")
}

fn cube() -> Arc<CpuMesh> {
    let mut vertices = Vec::new();
    for i in 0..8u32 {
        let p = [
            if i & 1 == 0 { -0.5 } else { 0.5 },
            if i & 2 == 0 { -0.5 } else { 0.5 },
            if i & 4 == 0 { -0.5 } else { 0.5 },
        ];
        vertices.push(LabVertex::from_pos_normal_uv(p, [0.0, 1.0, 0.0], [0.0, 0.0]));
    }
    let indices = vec![0, 1, 2, 1, 3, 2, 4, 6, 5, 5, 6, 7];
    Arc::new(CpuMesh { vertices, indices })
}

/// The app, the tube chunk's entity. `terrain` spawns the terrain; `opt_in` inserts
/// `TerrainSunCover`.
fn app(terrain: Terrain, opt_in: bool) -> (App, Entity) {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<ViewVisibilityStats>();
    app.init_resource::<VisCellStats>();
    app.init_resource::<VisCellCulled>();
    app.insert_resource(LodViewport {
        width: 96,
        height: 64,
    });
    app.insert_resource(VisCells::from_ron(CELLS).expect("cells"));
    if opt_in {
        app.insert_resource(TerrainSunCover::default());
    }
    app.add_systems(PostUpdate, propagate_transforms);
    app.add_systems(
        Render,
        (extract_frame, update_terrain_sun_cover, cull_unreachable_cells).chain(),
    );
    app.world_mut().spawn(terrain);
    app.world_mut().spawn((
        Transform {
            translation: Vec3::new(0.0, 9.0, 12.0),
            rotation: Quat::from_rotation_x(-(9.0f32).atan2(12.0)),
            scale: Vec3::ONE,
        },
        Camera::default(),
    ));
    app.world_mut().spawn(DirectionalLight {
        direction: [0.0, -1.0, 0.0],
        color: [1.0; 3],
        illuminance: 3000.0,
        cast_shadows: true,
        static_shadow: false,
    });
    let chunk = app
        .world_mut()
        .spawn((
            Transform {
                translation: Vec3::new(0.0, -10.0, 0.0),
                rotation: Quat::IDENTITY,
                scale: Vec3::new(5.0, 5.0, 18.0),
            },
            MeshRenderer::new(cube(), Arc::new(CpuMaterial::default())),
            VisCell(1),
        ))
        .id();
    (app, chunk)
}

fn chunk_casts_sun(app: &App, chunk: Entity) -> Option<bool> {
    let frame = app.world().resource::<ExtractedFrame>();
    let draws: Vec<_> = frame.meshes.iter().filter(|m| m.entity == chunk).collect();
    (!draws.is_empty()).then(|| draws.iter().any(|m| m.shadow.casts_sun()))
}

/// Two frames: the first fills the cover (its insert lands after the cull), the second seals.
fn run(app: &mut App) -> VisCellStats {
    app.update();
    app.update();
    app.world().resource::<VisCellStats>().clone()
}

#[test]
fn the_terrain_cover_seals_a_buried_tube() {
    let (mut app, chunk) = app(flat(), true);
    let s = run(&mut app);
    let cover = app.world().resource::<SunCover>().clone();
    assert_eq!(cover.cells(), &[1], "the terrain covers the tube");
    assert_eq!(s.sun_reached, vec![0], "the mouth is edge-on to the sun: {s:?}");
    assert_eq!((s.sun_sealed_cells, s.sun_sealed_draws), (1, 1), "{s:?}");
    // The chunk was shadow-only (out of the outdoor view) with no local light: it leaves the frame.
    assert_ne!(chunk_casts_sun(&app, chunk), Some(true));
    assert_eq!(app.world().resource::<TerrainSunCoverStats>().fills, 1);
}

#[test]
fn without_the_opt_in_nothing_is_sealed() {
    let (mut app, chunk) = app(flat(), false);
    let s = run(&mut app);
    assert!(app.world().get_resource::<SunCover>().is_none());
    assert_eq!(s.sun_sealed_draws, 0, "{s:?}");
    assert_eq!(
        chunk_casts_sun(&app, chunk),
        Some(true),
        "the chunk keeps casting"
    );
}

#[test]
fn a_hole_over_the_tube_seals_nothing() {
    let mut terrain = flat();
    terrain.holes = Some(
        HolesMask::new(vec![HoleEllipse {
            center: [0.0, 0.0],
            radius: [2.0, 2.0],
            rotation_radians: 0.0,
        }])
        .expect("mask"),
    );
    let (mut app, chunk) = app(terrain, true);
    let s = run(&mut app);
    assert!(
        app.world().resource::<SunCover>().cells().is_empty(),
        "the hole is a gap"
    );
    assert_eq!(s.sun_sealed_draws, 0, "{s:?}");
    assert_eq!(chunk_casts_sun(&app, chunk), Some(true));
}
