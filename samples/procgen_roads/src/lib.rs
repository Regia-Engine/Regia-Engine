// SPDX-License-Identifier: MIT-0
//! `procgen_roads`: the acceptance sample of spec 64 (`book/agents/procgen.md`, "Roads and fences").
//!
//! Three roads (one crossing, one T), curbs from the ribbon edges, markings, fences around the blocks
//! the roads cut from a field polygon, rocks scattered with the road exclusion removing them from
//! the road, and terrain flattened under the roads at bake time.

use std::path::{Path, PathBuf};

use procgen_common::{
    cuboid, headless_app, hills, install_library, lump, procgen_settings, solid, spawn_ground, spawn_view,
    standing_box, terrain_of,
};
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_procgen::road::RoadOf;
use regia_procgen::world::{ProcBounds, ProcGenMode, ProcGenerator};
use regia_scene::components::{EntityGuid, Name, Tags};
use regia_scene::spline::{Spline, TangentMode};
use regia_scene::{GlobalTransform, Transform};
use regia_terrain::TerrainLayers;

/// The sample's generator (`procgen.list` name).
pub const GENERATOR: &str = "road network";

/// The directory of the sample's graph and road profile.
///
/// Resolved at runtime (ADR-10370): `env!("CARGO_MANIFEST_DIR")` would embed the
/// build machine's path, which rustc does not remap.
pub fn assets_dir() -> PathBuf {
    sample_manifest_dir("procgen_roads").join("assets")
}

fn sample_manifest_dir(sample: &str) -> PathBuf {
    if let Some(dir) = std::env::var_os("CARGO_MANIFEST_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(root) = exe
            .parent()
            .and_then(|dir| dir.parent())
            .and_then(|dir| dir.parent())
    {
        let project = root.join("samples").join(sample);
        if project.is_dir() {
            return project;
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn spline_entity(world: &mut World, points: &[[f32; 2]], tag: &str, guid: u128, closed: bool) -> Entity {
    let mut s = Spline::through(points.iter().map(|p| Vec3::new(p[0], 0.0, p[1])));
    if closed {
        s = s.closed();
        s.points.iter_mut().for_each(|p| p.mode = TangentMode::Linear);
    }
    world
        .spawn((
            s,
            Tags::from_iter_str([tag]),
            EntityGuid(Guid(guid)),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id()
}

/// Fills `world`: terrain, ground, roads, the field, the generator, meshes and the view.
pub fn build(world: &mut World) -> Entity {
    let map = hills(257, 512.0, 16.0, 7);
    world.spawn(terrain_of(&map, TerrainLayers::default()));
    spawn_ground(world, 1, solid([0.32, 0.42, 0.2], 0.95));
    let roads: [&[[f32; 2]]; 3] = [
        &[[-230.0, -20.0], [-80.0, 8.0], [60.0, -8.0], [230.0, 16.0]],
        &[[-40.0, -220.0], [-14.0, -60.0], [10.0, 60.0], [24.0, 220.0]],
        // Ends at the first road: a T.
        &[[190.0, 210.0], [170.0, 110.0], [130.0, 40.0], [118.0, 1.5]],
    ];
    for (k, points) in roads.iter().enumerate() {
        let e = spline_entity(world, points, "road", 0x100 + k as u128, false);
        world.entity_mut(e).insert(RoadOf::new("two_lane.road.ron"));
    }
    spline_entity(
        world,
        &[[-180.0, -170.0], [180.0, -170.0], [180.0, 170.0], [-180.0, 170.0]],
        "field",
        0x200,
        true,
    );
    let generator = world
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(240.0, 100.0, 240.0),
                },
                mode: ProcGenMode::Baked,
                seed: Some(64),
                allow_terrain_edits: true,
                ..ProcGenerator::new("road_network.pgraph.ron")
            },
            Name::new(GENERATOR),
            EntityGuid(Guid(0x300)),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id();
    let concrete = solid([0.6, 0.6, 0.58], 0.9);
    let wood = solid([0.42, 0.3, 0.18], 0.85);
    install_library(
        world,
        vec![
            ("props/curb_stone", standing_box([0.2, 0.12, 0.2]), concrete),
            ("props/fence_post", standing_box([0.08, 0.7, 0.08]), wood.clone()),
            ("props/fence_panel", cuboid([1.4, 0.04, 0.5]), wood),
            ("props/rock", lump(1.0, 0.9), solid([0.45, 0.43, 0.4], 0.9)),
        ],
        vec![
            ("materials/asphalt.material.ron", solid([0.09, 0.09, 0.1], 0.8)),
            ("materials/gravel.material.ron", solid([0.42, 0.38, 0.32], 0.95)),
            ("materials/paint.material.ron", solid([1.0, 1.0, 1.0], 0.6)),
        ],
    );
    spawn_view(
        world,
        Vec3::new(120.0, 110.0, -210.0),
        Vec3::new(40.0, 0.0, 10.0),
        2_000.0,
    );
    generator
}

/// The headless app: `None` without a Vulkan device.
pub fn app(project: &Path, bake_dir: Option<PathBuf>) -> Option<App> {
    let mut app = headless_app(project, procgen_settings(assets_dir(), bake_dir, true))?;
    build(app.world_mut());
    Some(app)
}
