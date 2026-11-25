// SPDX-License-Identifier: MIT-0
//! `procgen_open_world`: the acceptance sample of spec 66 ("Tiles and anchors" in
//! `book/agents/procgen.md`).
//!
//! 4 km × 4 km of procedural terrain. A generator in `Anchored` mode builds cells around the primary
//! view: a biome area on a 256 m tier and ground clutter on a 32 m tier. The camera follows
//! [`CameraRoute`] on the game clock, so `play.step` flies it; nothing else moves it.

use std::path::{Path, PathBuf};

use procgen_common::{
    headless_app, hills, install_library, look_rotation, lump, procgen_settings, solid, spawn_ground,
    spawn_view, terrain_of, tree,
};
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_procgen::biome::ecs::BiomeArea;
use regia_procgen::tiles::{AnchorSettings, AnchorSource, TierReach, TileSettings};
use regia_procgen::world::{ProcBounds, ProcGenMode, ProcGenerator};
use regia_render::Camera;
use regia_scene::components::{EntityGuid, Name};
use regia_scene::{GlobalTransform, Transform};
use regia_terrain::{TerrainLayers, TerrainMap};

/// The sample's generator (`procgen.list` name).
pub const GENERATOR: &str = "open world";
/// The route's waypoints, XZ meters, flown at [`SPEED_M_S`].
pub const WAYPOINTS: [[f32; 2]; 4] = [
    [-1200.0, -1000.0],
    [-400.0, -300.0],
    [500.0, 200.0],
    [1200.0, 1100.0],
];
/// Camera speed along the route, meters per second of game time.
pub const SPEED_M_S: f32 = 60.0;
/// Height of the camera over the terrain, meters.
pub const CLEARANCE_M: f32 = 12.0;
/// The largest tile memory the settings allow, megabytes.
pub const MEMORY_CAP_MB: u32 = 256;

/// The directory of the sample's graph and biome.
///
/// Resolved at runtime (ADR-10370): `env!("CARGO_MANIFEST_DIR")` would embed the
/// build machine's path, which rustc does not remap.
pub fn assets_dir() -> PathBuf {
    sample_manifest_dir("procgen_open_world").join("assets")
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

/// Flies the camera along [`WAYPOINTS`] on the game clock.
#[derive(Resource, Clone)]
pub struct CameraRoute {
    /// The terrain, for the flight height.
    pub map: TerrainMap,
}

/// The route's total length, meters.
pub fn route_length() -> f32 {
    WAYPOINTS
        .windows(2)
        .map(|w| (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]))
        .sum()
}

/// The route position at `d` meters from the start (clamped), XZ, and the heading point ahead.
pub fn route_at(d: f32) -> ([f32; 2], [f32; 2]) {
    let mut left = d.max(0.0);
    for w in WAYPOINTS.windows(2) {
        let seg = (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]);
        if left <= seg {
            let t = left / seg;
            return (
                [
                    w[0][0] + (w[1][0] - w[0][0]) * t,
                    w[0][1] + (w[1][1] - w[0][1]) * t,
                ],
                w[1],
            );
        }
        left -= seg;
    }
    (WAYPOINTS[3], [WAYPOINTS[3][0] + 1.0, WAYPOINTS[3][1] + 1.0])
}

/// `Update`: places the camera for the game time.
pub fn fly_camera(
    route: Res<CameraRoute>,
    time: Res<Time>,
    mut cameras: Query<(&mut Transform, &mut GlobalTransform), With<Camera>>,
) {
    let ([x, z], ahead) = route_at(time.elapsed_secs() * SPEED_M_S);
    let eye = Vec3::new(x, route.map.sample_bilinear(x, z) + CLEARANCE_M, z);
    let target = Vec3::new(ahead[0], route.map.sample_bilinear(ahead[0], ahead[1]), ahead[1]);
    for (mut tf, mut gt) in &mut cameras {
        tf.translation = eye;
        tf.rotation = look_rotation(eye, target);
        *gt = GlobalTransform(tf.compute_affine());
    }
}

/// Fills `world`: terrain, ground, a forest area, the anchored generator, meshes and the view.
pub fn build(world: &mut World) -> Entity {
    let map = hills(513, 4096.0, 90.0, 3);
    world.spawn(terrain_of(&map, TerrainLayers::default()));
    spawn_ground(world, 1, solid([0.3, 0.4, 0.2], 0.95));
    world.spawn((
        BiomeArea {
            blend_m: 20.0,
            ..BiomeArea::new("forest.biome.ron")
        },
        EntityGuid(Guid(0x801)),
        Name::new("forest"),
        Transform {
            scale: Vec3::new(4000.0, 1000.0, 4000.0),
            ..Transform::default()
        },
        GlobalTransform::default(),
    ));
    let runtime = AnchorSettings {
        anchors: AnchorSource::PrimaryView,
        tiers: vec![
            TierReach {
                cell_m: 256.0,
                build_m: 420.0,
                ..TierReach::default()
            },
            TierReach {
                cell_m: 32.0,
                build_m: 110.0,
                ..TierReach::default()
            },
        ],
        max_new_per_frame: Some(16),
        max_resident_mb: Some(MEMORY_CAP_MB),
        ..AnchorSettings::default()
    };
    let generator = world
        .spawn((
            ProcGenerator {
                mode: ProcGenMode::Anchored,
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(2040.0, 400.0, 2040.0),
                },
                seed: Some(66),
                tiles: Some(TileSettings {
                    cell_m: 64.0,
                    runtime: Some(runtime),
                    ..TileSettings::default()
                }),
                ..ProcGenerator::new("open_world.pgraph.ron")
            },
            Name::new(GENERATOR),
            EntityGuid(Guid(0x802)),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id();
    install_library(
        world,
        vec![
            ("trees/pine", tree(14.0, 3.0), solid([1.0, 1.0, 1.0], 0.9)),
            ("plants/tuft", lump(0.3, 0.4), solid([0.25, 0.42, 0.18], 0.9)),
            ("rocks/pebble", lump(0.2, 0.15), solid([0.48, 0.46, 0.42], 0.9)),
        ],
        Vec::new(),
    );
    let [x, z] = WAYPOINTS[0];
    let eye = Vec3::new(x, map.sample_bilinear(x, z) + CLEARANCE_M, z);
    spawn_view(world, eye, Vec3::new(0.0, 0.0, 0.0), 3_000.0);
    world.insert_resource(CameraRoute { map });
    generator
}

/// The headless app: `None` without a Vulkan device.
pub fn app(project: &Path) -> Option<App> {
    let mut app = headless_app(project, procgen_settings(assets_dir(), None, false))?;
    build(app.world_mut());
    app.add_systems(Update, fly_camera);
    Some(app)
}
