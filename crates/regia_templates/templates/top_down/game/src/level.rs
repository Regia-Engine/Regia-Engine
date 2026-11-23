// SPDX-License-Identifier: MIT-0
//! The level: a ground plate and four pillars. Every piece is a static collider with a visible box
//! mesh. `nav.rs` bakes the same colliders into the navmesh the enemy walks, so a box you add
//! here is also an obstacle for the enemy.

use std::sync::Arc;

use regia::physics::{Collider, ColliderShape, RigidBody};
use regia::prelude::*;
use regia_core::math::Vec3;
use regia_render::{CpuMaterial, CpuMesh, DirectionalLight, LabVertex, MeshRenderer};
use regia_scene::{GlobalTransform, Transform, components::Name};

/// `Startup`: the ground and the pillars.
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
        Vec3::new(25.0, 0.5, 25.0),
    );
    for (index, (x, z)) in [(6.0, 6.0), (-6.0, 6.0), (6.0, -6.0), (-6.0, -6.0)]
        .into_iter()
        .enumerate()
    {
        block(
            &mut commands,
            &format!("Pillar{index}"),
            Vec3::new(x, 1.0, z),
            Vec3::new(0.75, 1.0, 0.75),
        );
    }
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
        MeshRenderer::new(Arc::new(cube_mesh(half)), solid([0.5, 0.55, 0.5, 1.0])),
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
