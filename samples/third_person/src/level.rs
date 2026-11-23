// SPDX-License-Identifier: MIT-0
//! The playable level: ground, stairs, a slope, platforms and walls — rapier colliders with
//! visible box meshes — plus `PlayerStart`s and the collectibles.

use regia::prelude::*;
use regia_core::math::Vec3;
use regia_framework::player::PlayerStart;
use regia_physics::{BodyKind, Collider, ColliderShape, RigidBody};

use regia_render::{CpuMesh, LabVertex, MeshRenderer};
use regia_scene::Transform;
use std::sync::Arc;

use crate::collectible;

/// Ground extent (half-size, metres).
const GROUND: f32 = 30.0;

/// Spawns the level: static colliders + their visuals + collectibles + player starts.
pub fn setup(world: &mut World) {
    let state = world
        .spawn((
            crate::SampleState {
                total: 0,
                ..Default::default()
            },
            regia_scene::components::Name::new("sample-state"),
            Name::new("sample-state"),
        ))
        .id();
    world.insert_resource(crate::collectible::StateHandle(Some(state)));

    // The sun (shadows on for depth in screenshots).
    world.spawn((
        regia_render::DirectionalLight {
            direction: [-0.4, -1.0, -0.3],
            color: [1.0, 0.96, 0.9],
            illuminance: 20000.0,
            cast_shadows: true,
            static_shadow: false,
        },
        Transform::default(),
    ));

    // --- ground ------------------------------------------------------------
    spawn_box(
        world,
        Vec3::ZERO,
        Vec3::new(GROUND, 0.25, GROUND),
        [0.24, 0.32, 0.26, 1.0],
        false,
    );

    // --- stairs (5 × 20 cm rises) -------------------------------------------
    // Boxes sit on the ground: centre and half-height are both `top / 2`, so each step's
    // top face rises 20 cm over the previous one (the character's 35 cm step height
    // clears it with margin; making the half-extent grow with the centre doubled the
    // rise to 40 cm, which the controller correctly refused to climb).
    for step in 0..5u32 {
        let top = 0.35 + step as f32 * 0.2;
        spawn_box(
            world,
            Vec3::new(8.0 + step as f32 * 0.9, top * 0.5, 0.0),
            Vec3::new(0.45, top * 0.5, 3.0),
            [0.42, 0.38, 0.30, 1.0],
            false,
        );
    }

    // --- the slope, climbing +X off the last stair --------------------------
    // A positive rotation about Z (right-handed, Y up) lifts local +X:
    // world top y = centre.y + local_x·sin θ + half.y·cos θ.
    // θ = 5°, half (3, 0.12, 2.5), centre (14.9, 1.262, 0):
    // the −X lip is world x ≈ 11.9, y ≈ 1.12 (just under the last stair, top
    // 1.15, which ends at x 12.05), and world x = 14.5 is y ≈ 1.35, so the
    // capsule (surface + 0.8) stands above y = 2.1 there. 5° is walkable
    // (the controller's max slope is ~45°).
    spawn_ramp(
        world,
        Vec3::new(14.9, 1.262, 0.0),
        Vec3::new(3.0, 0.12, 2.5),
        5.0_f32.to_radians(),
        [0.36, 0.42, 0.30, 1.0],
    );

    // --- terrace + gap-jump platforms ---------------------------------------
    spawn_box(
        world,
        Vec3::new(22.0, 0.85, 0.0),
        Vec3::new(4.0, 0.35, 3.0),
        [0.30, 0.36, 0.44, 1.0],
        false,
    );
    spawn_box(
        world,
        Vec3::new(22.0, 1.55, -8.0),
        Vec3::new(2.0, 0.2, 2.0),
        [0.30, 0.36, 0.44, 1.0],
        false,
    );
    spawn_box(
        world,
        Vec3::new(22.0, 2.25, -15.0),
        Vec3::new(2.0, 0.2, 2.0),
        [0.30, 0.36, 0.44, 1.0],
        false,
    );

    // --- boundary walls (also exercise the camera boom probe) ---------------
    spawn_box(
        world,
        Vec3::new(0.0, 2.0, GROUND),
        Vec3::new(GROUND, 2.0, 0.5),
        [0.2, 0.22, 0.24, 1.0],
        false,
    );
    spawn_box(
        world,
        Vec3::new(0.0, 2.0, -GROUND),
        Vec3::new(GROUND, 2.0, 0.5),
        [0.2, 0.22, 0.24, 1.0],
        false,
    );
    spawn_box(
        world,
        Vec3::new(GROUND, 2.0, 0.0),
        Vec3::new(0.5, 2.0, GROUND),
        [0.2, 0.22, 0.24, 1.0],
        false,
    );
    spawn_box(
        world,
        Vec3::new(-GROUND, 2.0, 0.0),
        Vec3::new(0.5, 2.0, GROUND),
        [0.2, 0.22, 0.24, 1.0],
        false,
    );

    // --- player starts ------------------------------------------------------
    // Yaw 0 looks down −Z, and the boom sits on +Z (camera = pivot − forward·length
    // = pivot + Z·4.5). A second start on that ray (same x, larger z) puts player 0's
    // camera inside player 1. Player 1 is off to the side and behind that camera:
    // x = 10 is 14 m off the x = −4 boom, z = 10 is behind the camera at z ≈ 4.5.
    // Sorted by x then z, so index 0 stays the start at x = −4.
    world.spawn((
        Transform {
            translation: Vec3::new(-4.0, 1.5, 0.0),
            ..Default::default()
        },
        PlayerStart,
    ));
    world.spawn((
        Transform {
            translation: Vec3::new(10.0, 1.5, 10.0),
            ..Default::default()
        },
        PlayerStart,
    ));

    // --- collectibles -------------------------------------------------------
    let count = collectible::spawn_collectibles(world);
    if let Some(mut s) = world.get_mut::<crate::SampleState>(state) {
        s.total = count;
    }
}

/// Spawns a static axis-aligned box: collider + visible mesh.
pub fn spawn_box(
    world: &mut World,
    centre: Vec3,
    half_extents: Vec3,
    colour: [f32; 4],
    sensor: bool,
) -> Entity {
    world
        .spawn((
            Transform {
                translation: centre,
                ..Default::default()
            },
            RigidBody {
                kind: BodyKind::Static,
                ..Default::default()
            },
            Collider {
                shape: ColliderShape::Box { half_extents },
                sensor,
                ..Default::default()
            },
            MeshRenderer::new(box_mesh(half_extents), solid_material(colour)),
        ))
        .id()
}

/// Spawns a static box rotated about Z (a ramp that climbs along +X).
fn spawn_ramp(
    world: &mut World,
    centre: Vec3,
    half_extents: Vec3,
    tilt_radians: f32,
    colour: [f32; 4],
) -> Entity {
    use regia_core::math::Quat;
    world
        .spawn((
            Transform {
                translation: centre,
                rotation: Quat::from_rotation_z(tilt_radians),
                ..Default::default()
            },
            RigidBody {
                kind: BodyKind::Static,
                ..Default::default()
            },
            Collider {
                shape: ColliderShape::Box { half_extents },
                ..Default::default()
            },
            MeshRenderer::new(box_mesh(half_extents), solid_material(colour)),
        ))
        .id()
}

/// A unit-centred box mesh scaled to `half_extents`, with per-face normals and a stripe
/// pattern in the UVs so the boxes are easy to read in screenshots.
pub fn box_mesh(half_extents: Vec3) -> Arc<CpuMesh> {
    let h = half_extents;
    let faces = [
        // +X
        (
            [
                [h.x, -h.y, -h.z],
                [h.x, h.y, -h.z],
                [h.x, h.y, h.z],
                [h.x, -h.y, h.z],
            ],
            [1.0, 0.0, 0.0],
        ),
        // -X
        (
            [
                [-h.x, -h.y, h.z],
                [-h.x, h.y, h.z],
                [-h.x, h.y, -h.z],
                [-h.x, -h.y, -h.z],
            ],
            [-1.0, 0.0, 0.0],
        ),
        // +Y
        (
            [
                [-h.x, h.y, -h.z],
                [-h.x, h.y, h.z],
                [h.x, h.y, h.z],
                [h.x, h.y, -h.z],
            ],
            [0.0, 1.0, 0.0],
        ),
        // -Y
        (
            [
                [-h.x, -h.y, h.z],
                [-h.x, -h.y, -h.z],
                [h.x, -h.y, -h.z],
                [h.x, -h.y, h.z],
            ],
            [0.0, -1.0, 0.0],
        ),
        // +Z
        (
            [
                [-h.x, -h.y, h.z],
                [h.x, -h.y, h.z],
                [h.x, h.y, h.z],
                [-h.x, h.y, h.z],
            ],
            [0.0, 0.0, 1.0],
        ),
        // -Z
        (
            [
                [h.x, -h.y, -h.z],
                [-h.x, -h.y, -h.z],
                [-h.x, h.y, -h.z],
                [h.x, h.y, -h.z],
            ],
            [0.0, 0.0, -1.0],
        ),
    ];
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    for (i, (corners, normal)) in faces.iter().enumerate() {
        let base = (i * 4) as u32;
        let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        for (corner, uv) in corners.iter().zip(uvs) {
            vertices.push(LabVertex {
                position: *corner,
                normal: *normal,
                uv,
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0, 1.0, 1.0, 1.0],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    Arc::new(CpuMesh { vertices, indices })
}

/// An unlit-friendly PBR material with a flat colour.
pub fn solid_material(colour: [f32; 4]) -> Arc<regia_render::CpuMaterial> {
    Arc::new(regia_render::CpuMaterial {
        base_color: colour,
        metallic: 0.0,
        roughness: 0.9,
        ..Default::default()
    })
}
