// SPDX-License-Identifier: MIT-0
//! `procgen_biomes`: the acceptance sample of spec 65 (`book/agents/biomes.md`).
//!
//! Three biomes on rolling terrain: a pine forest east of the river, a meadow west of it, a riverbed
//! along it (a closed spline area around a ribbon of water, which the water-distance filters read),
//! and a road whose exclusion keeps every biome off it.

use std::path::{Path, PathBuf};

use procgen_common::{
    headless_app, hills, install_library, lump, procgen_settings, solid, spawn_ground, spawn_view,
    terrain_of, tree,
};
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_procgen::biome::ecs::{AreaShape, BiomeArea};
use regia_procgen::road::RoadOf;
use regia_procgen::world::{ProcBounds, ProcGenerator};
use regia_scene::components::{EntityGuid, Name, Tags};
use regia_scene::spline::{Spline, TangentMode};
use regia_scene::{GlobalTransform, Transform};
use regia_terrain::water_body::{RibbonPoint, WaterBody, WaterLook, WaterShape};
use regia_terrain::{TerrainLayers, TerrainMap};

/// The sample's generator (`procgen.list` name).
pub const GENERATOR: &str = "biomes";
/// A point deep in the pine forest.
pub const IN_FOREST: [f32; 3] = [230.0, 0.0, 120.0];
/// A point deep in the meadow.
pub const IN_MEADOW: [f32; 3] = [-230.0, 0.0, 120.0];
/// A point on the river.
pub const ON_RIVER: [f32; 3] = [0.0, 0.0, 0.0];

/// The directory of the sample's graph, biomes and road profile.
///
/// Resolved at runtime (ADR-10370): `env!("CARGO_MANIFEST_DIR")` would embed the
/// build machine's path, which rustc does not remap.
pub fn assets_dir() -> PathBuf {
    sample_manifest_dir("procgen_biomes").join("assets")
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

fn box_area(world: &mut World, biome: &str, at: Vec3, size: Vec3, guid: u128) {
    world.spawn((
        BiomeArea {
            blend_m: 12.0,
            ..BiomeArea::new(biome)
        },
        EntityGuid(Guid(guid)),
        Name::new(biome),
        Transform {
            scale: size,
            ..Transform::from_translation(at)
        },
        GlobalTransform::default(),
    ));
}

/// The river's center line: north to south, swinging around x = 0.
fn river_points() -> Vec<[f32; 2]> {
    (0..=12)
        .map(|k| {
            let z = -360.0 + 60.0 * k as f32;
            let x = 30.0 * (z * 0.012).sin();
            [x, z]
        })
        .collect()
}

/// The band around the river as a closed loop: down one bank, back up the other.
fn bank_outline(half: f32) -> Vec<Vec3> {
    let river = river_points();
    let mut out: Vec<Vec3> = river.iter().map(|p| Vec3::new(p[0] - half, 0.0, p[1])).collect();
    out.extend(river.iter().rev().map(|p| Vec3::new(p[0] + half, 0.0, p[1])));
    out
}

fn add_river(world: &mut World, map: &TerrainMap) {
    let points = river_points()
        .into_iter()
        .map(|p| RibbonPoint {
            position: [p[0], map.sample_bilinear(p[0], p[1]) + 0.2, p[1]],
            width: 16.0,
            depth: 1.2,
            speed: Some(0.8),
        })
        .collect();
    world.spawn((
        WaterBody {
            shape: WaterShape::Ribbon { points },
            look: WaterLook::default(),
            obstacles: Vec::new(),
            ice: None,
            manning_n: 0.03,
            ..WaterBody::plane([0.0; 3], [1.0; 2], WaterLook::default())
        },
        Name::new("river"),
        EntityGuid(Guid(0x400)),
    ));
    let mut outline = Spline::through(bank_outline(40.0)).closed();
    outline
        .points
        .iter_mut()
        .for_each(|p| p.mode = TangentMode::Linear);
    world.spawn((
        BiomeArea {
            shape: AreaShape::FromSpline,
            blend_m: 14.0,
            ..BiomeArea::new("riverbed.biome.ron")
        },
        outline,
        EntityGuid(Guid(0x401)),
        Name::new("riverbed.biome.ron"),
        Transform::default(),
        GlobalTransform::default(),
    ));
}

/// Fills `world`: terrain, ground, river, areas, a road, the generator, meshes and the view.
pub fn build(world: &mut World) -> Entity {
    let map = hills(385, 768.0, 18.0, 11);
    world.spawn(terrain_of(&map, TerrainLayers::default()));
    spawn_ground(world, 1, solid([0.34, 0.42, 0.22], 0.95));
    add_river(world, &map);
    box_area(
        world,
        "pine_forest.biome.ron",
        Vec3::new(180.0, 0.0, 0.0),
        Vec3::new(360.0, 400.0, 720.0),
        0x501,
    );
    box_area(
        world,
        "meadow.biome.ron",
        Vec3::new(-180.0, 0.0, 0.0),
        Vec3::new(360.0, 400.0, 720.0),
        0x502,
    );
    let road = Spline::through([
        Vec3::new(-350.0, 0.0, -160.0),
        Vec3::new(-120.0, 0.0, -130.0),
        Vec3::new(120.0, 0.0, -180.0),
        Vec3::new(350.0, 0.0, -150.0),
    ]);
    world.spawn((
        road,
        RoadOf::new("two_lane.road.ron"),
        Tags::from_iter_str(["road"]),
        EntityGuid(Guid(0x600)),
        Transform::default(),
        GlobalTransform::default(),
    ));
    let generator = world
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(360.0, 200.0, 360.0),
                },
                seed: Some(65),
                ..ProcGenerator::new("biomes.pgraph.ron")
            },
            Name::new(GENERATOR),
            EntityGuid(Guid(0x700)),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id();
    install_library(
        world,
        vec![
            ("trees/pine", tree(14.0, 3.0), solid([1.0, 1.0, 1.0], 0.9)),
            ("plants/fern", lump(0.6, 0.5), solid([0.2, 0.4, 0.16], 0.9)),
            ("plants/flower", lump(0.25, 0.35), solid([0.85, 0.75, 0.3], 0.8)),
            ("plants/shrub", lump(1.2, 1.3), solid([0.24, 0.38, 0.17], 0.9)),
            ("rocks/stone", lump(0.5, 0.35), solid([0.5, 0.47, 0.42], 0.9)),
        ],
        vec![
            ("materials/asphalt.material.ron", solid([0.09, 0.09, 0.1], 0.8)),
            ("materials/gravel.material.ron", solid([0.42, 0.38, 0.32], 0.95)),
        ],
    );
    spawn_view(
        world,
        Vec3::new(-30.0, 90.0, -330.0),
        Vec3::new(10.0, 0.0, 0.0),
        2_000.0,
    );
    generator
}

/// The headless app: `None` without a Vulkan device.
pub fn app(project: &Path) -> Option<App> {
    let mut app = headless_app(project, procgen_settings(assets_dir(), None, false))?;
    build(app.world_mut());
    Some(app)
}
