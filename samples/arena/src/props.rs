// SPDX-License-Identifier: MIT-0
//! Dynamic props (`docs/specs/14` M20 `samples/arena`): crates and a jointed pendulum that the
//! explosion impulses shove around.
//!
//! Crates are dynamic rigid bodies with box colliders; the pendulum is a dynamic crate hinged to
//! a static anchor with a [`Joint`] (a revolute joint), so it swings under gravity. Explosions
//! (from [`crate::weapons`]) apply an `ExternalImpulse` that knocks these props across the
//! floor — the "explosion impulse" the acceptance names.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_physics::{Collider, ColliderShape, Joint, JointKind, RigidBody, Velocity};
use regia_render::MeshRenderer;
use regia_scene::{GlobalTransform, Transform};

use crate::meshes::ArenaAssets;

/// Marker: this entity is a dynamic prop (explosions shove it, it never fights back).
#[derive(Component, Reflect, Debug, Clone, Copy, Default)]
#[reflect(Component, Default)]
pub struct Prop;

/// Installs the prop type. Props are spawned explicitly by
/// [`crate::match_flow::spawn_world`]; the physics plugin drives them.
pub struct PropsPlugin;

impl Plugin for PropsPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<Prop>();
    }
}

/// The fixed crate positions (deterministic — a spawn order the replay matches).
const CRATE_POSITIONS: [(f32, f32); 8] = [
    (3.0, 3.0),
    (-4.0, 2.0),
    (5.0, -3.0),
    (-3.0, -5.0),
    (2.0, -4.0),
    (-5.0, 4.0),
    (6.0, 2.0),
    (-2.0, 6.0),
];

/// Spawns the arena's default props: eight crates and one jointed pendulum.
pub fn spawn_props(world: &mut World) {
    let mesh_material = world
        .get_resource::<ArenaAssets>()
        .map(|a| (a.crate_mesh.clone(), a.crate_material.clone()));

    for (i, (x, z)) in CRATE_POSITIONS.iter().enumerate() {
        spawn_crate(world, i, *x, *z, mesh_material.as_ref());
    }
    spawn_pendulum(world, mesh_material.as_ref());
}

/// Spawns a single dynamic crate at `(x, z)`.
pub fn spawn_crate(
    world: &mut World,
    index: usize,
    x: f32,
    z: f32,
    mesh_material: Option<&(
        std::sync::Arc<regia_render::CpuMesh>,
        std::sync::Arc<regia_render::CpuMaterial>,
    )>,
) -> Entity {
    let transform = Transform::from_translation(Vec3::new(x, 0.5, z));
    let mut entity = world.spawn((
        regia_scene::components::Name::new(format!("crate_{index}")),
        transform,
        GlobalTransform(transform.compute_affine()),
        RigidBody::dynamic(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(0.5, 0.5, 0.5),
            },
            friction: 0.6,
            restitution: 0.2,
            ..Collider::default()
        },
        Velocity::default(),
        Prop,
    ));
    if let Some((mesh, material)) = mesh_material {
        entity.insert(MeshRenderer::new(mesh.clone(), material.clone()));
    }
    entity.id()
}

/// Spawns a crate hinged to a static anchor (a swinging pendulum).
pub fn spawn_pendulum(
    world: &mut World,
    mesh_material: Option<&(
        std::sync::Arc<regia_render::CpuMesh>,
        std::sync::Arc<regia_render::CpuMaterial>,
    )>,
) {
    let anchor = world
        .spawn((
            regia_scene::components::Name::new("pendulum_anchor"),
            Transform::from_translation(Vec3::new(6.0, 4.0, 6.0)),
            RigidBody::static_body(),
        ))
        .id();

    let bob_transform = Transform::from_translation(Vec3::new(6.0, 2.0, 6.0));
    let mut bob = world.spawn((
        regia_scene::components::Name::new("pendulum_bob"),
        bob_transform,
        GlobalTransform(bob_transform.compute_affine()),
        RigidBody::dynamic(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(0.4, 0.4, 0.4),
            },
            friction: 0.5,
            restitution: 0.1,
            ..Collider::default()
        },
        Velocity::default(),
        Prop,
        Joint {
            kind: JointKind::Revolute {
                axis: Vec3::X,
                limits: None,
                motor: None,
            },
            connected: anchor,
            anchor: Vec3::new(0.0, 0.5, 0.0),
            connected_anchor: Vec3::new(0.0, -2.0, 0.0),
            break_force: 0.0,
        },
    ));
    if let Some((mesh, material)) = mesh_material {
        bob.insert(MeshRenderer::new(mesh.clone(), material.clone()));
    }
}

/// Spawns `count` extra crates in a grid — the "200 active rigid bodies" budget probe.
pub fn spawn_many_props(world: &mut World, count: usize) {
    let mesh_material = world
        .get_resource::<ArenaAssets>()
        .map(|a| (a.crate_mesh.clone(), a.crate_material.clone()));
    let side = (count as f32).sqrt().ceil() as i32;
    for i in 0..count {
        let x = (i as i32 % side - side / 2) as f32 * 1.5;
        let z = (i as i32 / side - side / 2) as f32 * 1.5;
        spawn_crate(world, 1000 + i, x, z, mesh_material.as_ref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_positions_are_within_the_arena() {
        for (x, z) in CRATE_POSITIONS {
            assert!(x.abs() < crate::match_flow::ARENA_HALF);
            assert!(z.abs() < crate::match_flow::ARENA_HALF);
        }
    }

    #[test]
    fn spawn_props_creates_crates_and_a_jointed_pendulum() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, regia_physics::PhysicsPlugin));
        app.insert_resource(ArenaAssets::build());
        spawn_props(app.world_mut());

        let world = app.world_mut();
        let mut props = world.query::<&Prop>();
        let prop_count = props.iter(world).count();
        assert_eq!(prop_count, CRATE_POSITIONS.len() + 1, "8 crates + 1 pendulum bob");

        let mut joints = world.query::<&Joint>();
        assert_eq!(joints.iter(world).count(), 1, "exactly one joint");
        let joint = joints.iter(world).next().expect("joint");
        assert!(matches!(joint.kind, JointKind::Revolute { .. }));
    }
}
