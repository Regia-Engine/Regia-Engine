// SPDX-License-Identifier: MIT-0
//! The level: a floor, a dividing wall with a gate in it, a button pillar behind the gate and a
//! lamp that the button raises. All procedural boxes: no assets.
//!
//! Walls and the closed gate carry [`Solid`], the box the player cannot walk into
//! ([`blocked`] is the check). The gate is a `Door` that slides up when opened; the button is a
//! `Button`; both are `Interactable`s (`interact.rs`).

use std::sync::Arc;

use regia::prelude::*;
use regia_core::math::Vec3;
use regia_render::{CpuMaterial, CpuMesh, DirectionalLight, LabVertex, MeshRenderer};
use regia_scene::{GlobalTransform, Transform, components::Name};

use crate::interact::{Button, Door, DoorBase, Interactable, Lamp};

/// Half the floor's side (meters).
pub const FLOOR_HALF: f32 = 10.0;
/// Player radius for collisions (meters).
pub const PLAYER_RADIUS: f32 = 0.3;
/// Where the gate stands.
pub const GATE_AT: Vec3 = Vec3::new(0.0, 1.1, -4.0);
/// Where the button stands (behind the gate).
pub const BUTTON_AT: Vec3 = Vec3::new(0.0, 1.0, -8.5);

/// A box the player cannot walk into (half-extents; the entity's transform is its center).
#[derive(Component, Debug, Clone, Copy)]
pub struct Solid(pub Vec3);

/// `Startup`: the floor, the wall and gate, the button and its lamp.
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
    let (vertices, indices) = floor_mesh(FLOOR_HALF);
    let transform = Transform::default();
    commands.spawn((
        Name::new("floor"),
        transform,
        GlobalTransform(transform.compute_affine()),
        MeshRenderer::new(
            Arc::new(CpuMesh { vertices, indices }),
            solid([0.35, 0.38, 0.42, 1.0], 0.9),
        ),
    ));
    // The dividing wall at z = -4: two slabs either side of the 3 m gate.
    for (name, x) in [("WallLeft", -5.75), ("WallRight", 5.75)] {
        spawn_box(
            &mut commands,
            name,
            Vec3::new(x, 1.5, -4.0),
            Vec3::new(4.25, 1.5, 0.15),
            [0.55, 0.5, 0.45, 1.0],
            true,
        );
    }
    // The gate, a door that slides up.
    let gate = spawn_box(
        &mut commands,
        "Gate",
        GATE_AT,
        Vec3::new(1.5, 1.1, 0.15),
        [0.3, 0.45, 0.7, 1.0],
        true,
    );
    commands.entity(gate).insert((
        Door { id: 0, open: false },
        DoorBase(GATE_AT.y),
        Interactable {
            id: 0,
            prompt_off: "open the gate".into(),
            prompt_on: "close the gate".into(),
        },
    ));
    // The button pillar and the lamp it raises.
    let button = spawn_box(
        &mut commands,
        "ButtonPillar",
        BUTTON_AT,
        Vec3::new(0.2, 0.5, 0.2),
        [0.8, 0.25, 0.2, 1.0],
        true,
    );
    commands.entity(button).insert((
        Button { id: 0, on: false },
        Interactable {
            id: 0,
            prompt_off: "press the button".into(),
            prompt_on: "release the button".into(),
        },
    ));
    let lamp_at = Vec3::new(3.0, 0.4, -8.5);
    let lamp = spawn_box(
        &mut commands,
        "Lamp",
        lamp_at,
        Vec3::splat(0.35),
        [1.0, 0.85, 0.3, 1.0],
        false,
    );
    commands.entity(lamp).insert(Lamp {
        button: 0,
        base_y: lamp_at.y,
    });
}

fn spawn_box(
    commands: &mut Commands,
    name: &str,
    at: Vec3,
    half: Vec3,
    color: [f32; 4],
    solid_box: bool,
) -> Entity {
    let transform = Transform::from_translation(at);
    let mut entity = commands.spawn((
        Name::new(name),
        transform,
        GlobalTransform(transform.compute_affine()),
        MeshRenderer::new(Arc::new(cube_mesh(half)), solid(color, 0.7)),
    ));
    if solid_box {
        entity.insert(Solid(half));
    }
    entity.id()
}

/// A plain opaque material of one color.
pub fn solid(base_color: [f32; 4], roughness: f32) -> Arc<CpuMaterial> {
    Arc::new(CpuMaterial {
        base_color,
        roughness,
        ..CpuMaterial::default()
    })
}

/// Whether a player circle at `position` overlaps `solid_box` (centered at `center`).
#[must_use]
pub fn overlaps(position: Vec3, center: Vec3, half: Vec3) -> bool {
    // The camera point is the player's head. A slab above it or below the feet cannot block.
    if center.y - half.y >= position.y || center.y + half.y <= position.y - 1.6 {
        return false;
    }
    let dx = (position.x - center.x).abs() - half.x;
    let dz = (position.z - center.z).abs() - half.z;
    dx < PLAYER_RADIUS && dz < PLAYER_RADIUS && dx.max(0.0).hypot(dz.max(0.0)) < PLAYER_RADIUS
}

/// Whether the player at `position` would be inside a wall, a closed gate or off the floor.
#[must_use]
pub fn blocked<'a>(position: Vec3, solids: impl IntoIterator<Item = (&'a Transform, &'a Solid)>) -> bool {
    let limit = FLOOR_HALF - PLAYER_RADIUS;
    position.x.abs() > limit
        || position.z.abs() > limit
        || solids
            .into_iter()
            .any(|(transform, solid_box)| overlaps(position, transform.translation, solid_box.0))
}

/// A flat quad of `half` meters on a side, centered at the origin in the XZ plane.
fn floor_mesh(half: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let vertex = |x: f32, z: f32| LabVertex {
        position: [x, 0.0, z],
        normal: [0.0, 1.0, 0.0],
        uv: [x / (2.0 * half) + 0.5, z / (2.0 * half) + 0.5],
        tangent: [1.0, 0.0, 0.0, 1.0],
        color: [1.0; 4],
        joints: [0; 4],
        weights: [0.0; 4],
    };
    let vertices = vec![
        vertex(-half, -half),
        vertex(half, -half),
        vertex(half, half),
        vertex(-half, half),
    ];
    (vertices, vec![0, 3, 2, 2, 1, 0])
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_winding_faces_up() {
        let (vertices, indices) = floor_mesh(FLOOR_HALF);
        for triangle in indices.as_chunks::<3>().0 {
            let [a, b, c] = triangle.map(|i| Vec3::from_array(vertices[i as usize].position));
            assert!((b - a).cross(c - a).y > 0.0);
        }
    }

    #[test]
    fn gate_collision_follows_the_current_slab_height() {
        let half = Vec3::new(1.5, 1.1, 0.15);
        let player = Vec3::new(0.0, 1.6, -4.0);
        assert!(!overlaps(player, Vec3::new(0.0, 3.4, -4.0), half));
        assert!(overlaps(player, Vec3::new(0.0, 1.1, -4.0), half));
    }

    #[test]
    fn walls_block_and_the_gate_gap_does_not() {
        let wall = (
            Transform::from_translation(Vec3::new(-5.75, 1.5, -4.0)),
            Solid(Vec3::new(4.25, 1.5, 0.15)),
        );
        let solids = [(&wall.0, &wall.1)];
        assert!(blocked(Vec3::new(-5.0, 1.6, -4.0), solids));
        assert!(!blocked(Vec3::new(0.0, 1.6, -4.0), solids), "the gap is open");
        assert!(
            blocked(Vec3::new(12.0, 1.6, 0.0), std::iter::empty()),
            "off the floor"
        );
        assert!(!blocked(Vec3::new(0.0, 1.6, 4.0), std::iter::empty()));
    }
}
