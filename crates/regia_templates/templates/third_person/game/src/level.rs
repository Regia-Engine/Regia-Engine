// SPDX-License-Identifier: MIT-0
//! The level: a ground plate, two blocks, and a short flight of stairs (foot IK has something to
//! plant on). Every piece is a static collider with a visible box mesh.

use std::sync::Arc;

use regia::physics::{Collider, ColliderShape, RigidBody};
use regia::prelude::*;
use regia_core::math::Vec3;
use regia_render::{CpuMaterial, CpuMesh, DirectionalLight, LabVertex, MeshRenderer};
use regia_scene::{GlobalTransform, Transform, components::Name};

use crate::collectible;

/// `Startup`: the ground, two blocks, the stairs and the collectibles.
pub fn setup_level(mut commands: Commands) {
    let sun_at = Transform::default();
    commands.spawn((
        Name::new("Sun"),
        sun_at,
        GlobalTransform(sun_at.compute_affine()),
        DirectionalLight {
            direction: [0.3, -1.0, -0.4],
            color: [1.0, 0.97, 0.92],
            illuminance: 90_000.0,
            cast_shadows: true,
            static_shadow: false,
        },
    ));
    block(
        &mut commands,
        "Ground",
        Vec3::new(0.0, -0.5, 0.0),
        Vec3::new(20.0, 0.5, 20.0),
    );
    block(
        &mut commands,
        "BlockA",
        Vec3::new(3.0, 0.5, -2.0),
        Vec3::new(1.0, 1.0, 1.0),
    );
    block(
        &mut commands,
        "BlockB",
        Vec3::new(-3.0, 1.0, 2.0),
        Vec3::new(2.0, 2.0, 2.0),
    );
    // Four steps of 0.2 m climbing toward -Z; the top is 0.8 m high.
    for step in 0..4_u8 {
        let top = 0.2 * f32::from(step + 1);
        block(
            &mut commands,
            &format!("Step{step}"),
            Vec3::new(8.0, top * 0.5, 4.0 - 0.8 * f32::from(step)),
            Vec3::new(1.5, top * 0.5, 0.4),
        );
    }
    collectible::spawn_all(&mut commands);
}

/// One static box: collider + cube mesh.
fn block(commands: &mut Commands, name: &str, translation: Vec3, half: Vec3) {
    commands.spawn((
        Name::new(name),
        Transform::from_translation(translation),
        GlobalTransform(Transform::from_translation(translation).compute_affine()),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box { half_extents: half },
            ..Collider::default()
        },
        MeshRenderer::new(Arc::new(cube_mesh(half)), solid([0.55, 0.6, 0.65, 1.0])),
    ));
}

/// A plain opaque material of one color.
pub fn solid(base_color: [f32; 4]) -> Arc<CpuMaterial> {
    Arc::new(CpuMaterial {
        base_color,
        ..CpuMaterial::default()
    })
}

/// An axis-aligned cube mesh with the given half-extents.
pub fn cube_mesh(half: Vec3) -> CpuMesh {
    let h = [half.x, half.y, half.z];
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    // Each face: normal, and the two axes its quad spans — ordered so u × v = n, which
    // keeps the triangles counter-clockwise seen from outside (front faces).
    let faces: [([f32; 3], usize, usize); 6] = [
        ([1.0, 0.0, 0.0], 1, 2),
        ([-1.0, 0.0, 0.0], 2, 1),
        ([0.0, 1.0, 0.0], 2, 0),
        ([0.0, -1.0, 0.0], 0, 2),
        ([0.0, 0.0, 1.0], 0, 1),
        ([0.0, 0.0, -1.0], 1, 0),
    ];
    for (normal, ua, va) in faces {
        let base = vertices.len() as u32;
        for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let mut position = [0.0_f32; 3];
            for (axis, value) in position.iter_mut().enumerate() {
                *value = normal[axis] * h[axis.min(2)]
                    + if axis == ua { su * h[ua] } else { 0.0 }
                    + if axis == va { sv * h[va] } else { 0.0 };
            }
            vertices.push(LabVertex {
                position,
                normal,
                uv: [su * 0.5 + 0.5, sv * 0.5 + 0.5],
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0, 1.0, 1.0, 1.0],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    CpuMesh { vertices, indices }
}
