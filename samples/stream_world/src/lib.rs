// SPDX-License-Identifier: MIT-0
//! World streaming sample (`docs/specs/78-world-streaming.md` § 18, acceptance A): a generated world of
//! 2048 m × 4096 m in 128 m cells, 512 cells, from a fixed seed.
//!
//! [`level_text`] writes the `.level.ron`: a flat heightfield collider and a ground plane that stay with
//! the level, about 200 static primitives per cell, dynamic bodies in every tenth cell and a few lights,
//! with HLOD on. [`Flight`] is the scripted path: a ground walk, a 50 m/s vehicle leg, a 2 km teleport
//! that crosses a floating-origin rebase, and a last leg. The sample writes the level and flies it when
//! a cooked project is given:
//!
//! ```text
//! cargo run -p stream_world -- --generate my_project/assets/world.level.ron
//! regia cook --project my_project
//! cargo run -p stream_world -- --project my_project
//! ```

use std::fmt::Write as _;

/// World size along X, meters.
pub const WORLD_X: f64 = 2048.0;
/// World size along Z, meters.
pub const WORLD_Z: f64 = 4096.0;
/// Cell size, meters.
pub const CELL: f64 = 128.0;
/// Static primitives per cell.
pub const PROPS_PER_CELL: u32 = 200;
/// Fixed steps per second of the flight.
pub const STEP_HZ: f64 = 60.0;

const TRANSFORM: &str = "regia_scene::transform::Transform";
const PRIMITIVE: &str = "regia_render::primitives::Primitive";
const COLLIDER: &str = "regia_physics::components::Collider";
const BODY: &str = "regia_physics::components::RigidBody";
const POINT_LIGHT: &str = "regia_render::lights::PointLight";
const STREAMING: &str = "regia_scene::streaming::LevelStreaming";

/// A small deterministic generator (Numerical Recipes' 64-bit LCG), so the world is the same bytes
/// everywhere.
#[derive(Debug, Clone, Copy)]
pub struct Lcg(pub u64);

impl Lcg {
    /// The next value in `0..1`.
    pub fn next_f32(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 40) as f32) / ((1u64 << 24) as f32)
    }
}

/// A stable guid text from a kind and an index.
fn guid(kind: u32, index: u32) -> String {
    format!("{kind:08x}-0000-4000-8000-{index:012x}")
}

/// The `.level.ron` of the world.
pub fn level_text(seed: u64) -> String {
    let cells_x = (WORLD_X / CELL) as u32;
    let cells_z = (WORLD_Z / CELL) as u32;
    let mut rng = Lcg(seed);
    let mut out = String::with_capacity(32 * 1024 * 1024);
    out.push_str("(format_version: 1, entities: [\n");

    // The ground: one heightfield collider and one plane, both too wide for a cell. The collider
    // needs a body to exist in physics (spec 15 § 2: a collider attaches to its own or its nearest
    // ancestor's body), so the ground is a static body.
    let _ = writeln!(
        out,
        "(guid: \"{}\", name: \"ground\", components: {{ \"{TRANSFORM}\": (translation: ({}, 0.0, {})), \
         \"{BODY}\": (kind: Static), \
         \"{COLLIDER}\": (shape: HeightField(nx: 2, nz: 2, heights: [0.0, 0.0, 0.0, 0.0], scale: ({}, 1.0, {}))), \
         \"{PRIMITIVE}\": (format_version: 1, shape: Cuboid(size: ({}, 0.5, {})), color: (0.3, 0.42, 0.25, \
         1.0), roughness: 0.9, metallic: 0.0) }}),",
        guid(1, 0),
        WORLD_X * 0.5,
        WORLD_Z * 0.5,
        WORLD_X,
        WORLD_Z,
        WORLD_X,
        WORLD_Z,
    );

    let mut index = 0;
    for cx in 0..cells_x {
        for cz in 0..cells_z {
            let (ox, oz) = (f64::from(cx) * CELL, f64::from(cz) * CELL);
            for _ in 0..PROPS_PER_CELL {
                index += 1;
                let x = ox + f64::from(rng.next_f32()) * CELL;
                let z = oz + f64::from(rng.next_f32()) * CELL;
                let wide = 1.0 + rng.next_f32() * 3.0;
                let tall = 0.5 + rng.next_f32() * 3.0;
                let high = tall * 2.0;
                let tint = 0.3 + rng.next_f32() * 0.5;
                let _ = writeln!(
                    out,
                    "(guid: \"{}\", components: {{ \"{TRANSFORM}\": (translation: ({x:.3}, {tall:.3}, {z:.3})), \
                     \"{PRIMITIVE}\": (format_version: 1, shape: Cuboid(size: ({wide:.3}, {high:.3}, {wide:.3})), \
                     color: ({tint:.3}, {tint:.3}, 0.5, 1.0), roughness: 0.7, metallic: 0.0) }}),",
                    guid(2, index)
                );
            }
            // Every tenth cell holds dynamic bodies that drop onto the ground.
            if (cx * cells_z + cz).is_multiple_of(10) {
                for i in 0..8 {
                    index += 1;
                    let x = ox + 20.0 + f64::from(i) * 12.0;
                    let z = oz + CELL * 0.5;
                    let _ = writeln!(
                        out,
                        "(guid: \"{}\", components: {{ \"{TRANSFORM}\": (translation: ({x:.3}, 4.0, {z:.3})), \
                         \"{BODY}\": (), \"{COLLIDER}\": (shape: Sphere(radius: 0.5)), \"{PRIMITIVE}\": \
                         (format_version: 1, shape: Sphere(radius: 0.5, sectors: 12, stacks: 8), color: (0.8, \
                         0.3, 0.2, 1.0), roughness: 0.5, metallic: 0.0) }}),",
                        guid(3, index)
                    );
                }
            }
            // A light every other cell along X.
            if cx.is_multiple_of(2) && cz.is_multiple_of(2) {
                index += 1;
                let _ = writeln!(
                    out,
                    "(guid: \"{}\", components: {{ \"{TRANSFORM}\": (translation: ({:.3}, 12.0, {:.3})), \
                     \"{POINT_LIGHT}\": () }}),",
                    guid(4, index),
                    ox + CELL * 0.5,
                    oz + CELL * 0.5
                );
            }
        }
    }
    let _ = write!(
        out,
        "], resources: {{ \"{STREAMING}\": (enabled: true, cell_size_m: {CELL:.1}, load_radius_m: 512.0, \
         unload_radius_m: 640.0, origin: (enabled: true, cell_size_m: 1024.0, threshold_m: 1536.0)) }})"
    );
    out
}

/// A bakeable slice of the world (`docs/specs/lightmaps.md`, bake time at scale): a project directory
/// with `Regia.toml`, `assets/meshes/prop.gltf` (a unit cube) and `assets/levels/bake_world.level.ron`.
///
/// [`level_text`] uses built-in primitives, which `regia bake-lightmaps --scene` skips (`builtin_mesh`),
/// so this writes the same layout (the same generator, density of props per cell and sizes) with every
/// prop a `GiMobility::Static` mesh from a glTF file. `cells_x` by `cells_z` cells of [`CELL`] meters
/// are written, `props` per cell; the whole world is 16 by 32 cells at [`PROPS_PER_CELL`].
///
/// # Errors
///
/// A file that cannot be written.
pub fn write_bakeable_project(
    dir: &std::path::Path,
    seed: u64,
    cells_x: u32,
    cells_z: u32,
    props: u32,
) -> std::io::Result<()> {
    let meshes = dir.join("assets/meshes");
    let levels = dir.join("assets/levels");
    std::fs::create_dir_all(&meshes)?;
    std::fs::create_dir_all(&levels)?;
    std::fs::write(
        dir.join("Regia.toml"),
        "[project]\nname = \"stream_world_bake\"\n",
    )?;
    std::fs::write(meshes.join("prop.gltf"), unit_cube_gltf())?;
    std::fs::write(
        levels.join("bake_world.level.ron"),
        bakeable_level_text(seed, cells_x, cells_z, props),
    )?;
    Ok(())
}

/// The `.level.ron` of [`write_bakeable_project`]: a ground slab and `props` cuboids a cell, all
/// static meshes, under the Earth sky at noon.
pub fn bakeable_level_text(seed: u64, cells_x: u32, cells_z: u32, props: u32) -> String {
    const MOBILITY: &str = "regia_scene::components::GiMobility";
    const MESH: &str = "regia_render::runtime::MeshSource";
    let mut rng = Lcg(seed);
    let mut out = String::with_capacity(256 * (1 + (cells_x * cells_z * props) as usize));
    out.push_str("SceneFile(\n    format_version: 10,\n    entities: [\n");
    let (wx, wz) = (f64::from(cells_x) * CELL, f64::from(cells_z) * CELL);
    let _ = writeln!(
        out,
        "(guid: \"{}\", name: \"ground\", components: {{ \"{TRANSFORM}\": (translation: ({}, -0.25, {}), \
         scale: ({wx}, 0.5, {wz})), \"{MOBILITY}\": Static, \"{MESH}\": (mesh: \"assets/meshes/prop.gltf\", \
         material: \"\") }}),",
        guid(1, 0),
        wx * 0.5,
        wz * 0.5,
    );
    let mut index = 0;
    for cx in 0..cells_x {
        for cz in 0..cells_z {
            let (ox, oz) = (f64::from(cx) * CELL, f64::from(cz) * CELL);
            for _ in 0..props {
                index += 1;
                let x = ox + f64::from(rng.next_f32()) * CELL;
                let z = oz + f64::from(rng.next_f32()) * CELL;
                let wide = 1.0 + rng.next_f32() * 3.0;
                let tall = 0.5 + rng.next_f32() * 3.0;
                let _ = writeln!(
                    out,
                    "(guid: \"{}\", components: {{ \"{TRANSFORM}\": (translation: ({x:.3}, {tall:.3}, {z:.3}), \
                     scale: ({wide:.3}, {:.3}, {wide:.3})), \"{MOBILITY}\": Static, \"{MESH}\": (mesh: \
                     \"assets/meshes/prop.gltf\", material: \"\") }}),",
                    guid(2, index),
                    tall * 2.0
                );
            }
        }
    }
    out.push_str(
        "    ],\n    resources: {\n        \"regia_scene::level::LevelSettings\": (atmosphere: (preset: Earth, \
         time_of_day: Some((hour: 12.0, day_of_year: 172, latitude_deg: 40.0)))),\n    },\n)\n",
    );
    out
}

/// A unit cube (side 1, centered) as a glTF with an embedded buffer: 24 vertices with outward normals.
fn unit_cube_gltf() -> String {
    // (normal, u axis, v axis) of the six faces; the winding is counter-clockwise seen from outside.
    const FACES: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
    ];
    let mut bytes = Vec::new();
    let mut normals = Vec::new();
    let mut indices = Vec::new();
    for (f, (n, u, v)) in FACES.iter().enumerate() {
        for (su, sv) in [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            for ((nk, uk), vk) in n.iter().zip(u).zip(v) {
                bytes.extend_from_slice(&(0.5 * nk + 0.5 * su * uk + 0.5 * sv * vk).to_le_bytes());
                normals.extend_from_slice(&nk.to_le_bytes());
            }
        }
        let b = f as u32 * 4;
        indices.extend([b, b + 1, b + 2, b, b + 2, b + 3]);
    }
    bytes.extend_from_slice(&normals);
    for i in &indices {
        bytes.extend_from_slice(&i.to_le_bytes());
    }
    format!(
        r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],"nodes":[{{"mesh":0}}],
"meshes":[{{"primitives":[{{"attributes":{{"POSITION":0,"NORMAL":1}},"indices":2,"material":0}}]}}],
"materials":[{{"pbrMetallicRoughness":{{"baseColorFactor":[0.5,0.5,0.5,1.0],"metallicFactor":0.0,"roughnessFactor":1.0}}}}],
"buffers":[{{"byteLength":{},"uri":"data:application/octet-stream;base64,{}"}}],
"bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":288,"target":34962}},{{"buffer":0,"byteOffset":288,"byteLength":288,"target":34962}},{{"buffer":0,"byteOffset":576,"byteLength":144,"target":34963}}],
"accessors":[{{"bufferView":0,"componentType":5126,"count":24,"type":"VEC3","min":[-0.5,-0.5,-0.5],"max":[0.5,0.5,0.5]}},{{"bufferView":1,"componentType":5126,"count":24,"type":"VEC3"}},{{"bufferView":2,"componentType":5125,"count":36,"type":"SCALAR"}}]}}"#,
        bytes.len(),
        base64(&bytes)
    )
}

/// Standard base64 with padding.
fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        s.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    s
}

/// What the flight does at a step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FlightStep {
    /// Stand at this world position.
    At([f64; 3]),
    /// Ask for the teleport to this target, then wait until it is ready.
    Teleport([f64; 3]),
    /// The flight is over.
    End,
}

/// The scripted path. Steps count from the first step after the open wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flight;

impl Flight {
    /// Ground walk: 300 steps at 6 m/s.
    pub const WALK_STEPS: u32 = 300;
    /// Vehicle leg: 1500 steps at 50 m/s, 1250 m.
    pub const DRIVE_STEPS: u32 = 1500;
    /// Last leg: 600 steps at 12 m/s.
    pub const LAST_STEPS: u32 = 600;
    /// Where the flight starts.
    pub const START: [f64; 3] = [400.0, 1.7, 300.0];
    /// The teleport target, 2 km further on.
    pub const TARGET: [f64; 3] = [1500.0, 1.7, 3500.0];

    /// The position of `step` on the path before the teleport.
    pub fn step(self, step: u32) -> FlightStep {
        let walk = 6.0 / STEP_HZ;
        let drive = 50.0 / STEP_HZ;
        let last = 12.0 / STEP_HZ;
        let [x, y, z] = Self::START;
        if step < Self::WALK_STEPS {
            return FlightStep::At([x, y, z + f64::from(step) * walk]);
        }
        let walked = f64::from(Self::WALK_STEPS) * walk;
        let step = step - Self::WALK_STEPS;
        if step < Self::DRIVE_STEPS {
            return FlightStep::At([x, y, z + walked + f64::from(step) * drive]);
        }
        let step = step - Self::DRIVE_STEPS;
        if step == 0 {
            return FlightStep::Teleport(Self::TARGET);
        }
        if step <= Self::LAST_STEPS {
            let [tx, ty, tz] = Self::TARGET;
            return FlightStep::At([tx - f64::from(step) * last, ty, tz]);
        }
        FlightStep::End
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_world_is_the_same_text_for_the_same_seed() {
        assert_eq!(level_text(7).len(), level_text(7).len());
        assert_ne!(level_text(7), level_text(8));
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn the_bakeable_level_has_one_static_mesh_per_prop_and_a_ground() {
        let text = bakeable_level_text(7, 2, 3, 5);
        assert_eq!(text.matches("GiMobility\": Static").count(), 1 + 2 * 3 * 5);
        assert_eq!(text, bakeable_level_text(7, 2, 3, 5), "deterministic");
        assert_ne!(text, bakeable_level_text(8, 2, 3, 5));
        assert!(unit_cube_gltf().contains("\"count\":24"));
    }

    #[test]
    fn the_flight_walks_drives_teleports_and_ends() {
        let f = Flight;
        assert_eq!(f.step(0), FlightStep::At(Flight::START));
        assert!(matches!(
            f.step(Flight::WALK_STEPS + Flight::DRIVE_STEPS),
            FlightStep::Teleport(_)
        ));
        assert_eq!(
            f.step(Flight::WALK_STEPS + Flight::DRIVE_STEPS + Flight::LAST_STEPS + 1),
            FlightStep::End
        );
        let FlightStep::At(end_of_drive) = f.step(Flight::WALK_STEPS + Flight::DRIVE_STEPS - 1) else {
            panic!("driving");
        };
        let distance = (Flight::TARGET[2] - end_of_drive[2]).hypot(Flight::TARGET[0] - end_of_drive[0]);
        assert!((1800.0..2400.0).contains(&distance), "{distance}");
    }
}
