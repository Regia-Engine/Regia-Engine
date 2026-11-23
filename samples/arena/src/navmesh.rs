// SPDX-License-Identifier: MIT-0
//! Navmesh generation for the arena (`docs/specs/22-tier2.md` § T2.2): extracting a triangle soup
//! from the level's static colliders and building a [`regia_nav::NavMesh`] from it, behind the
//! `use_navmesh` flag in [`crate::config::ArenaConfig`].
//!
//! Only static colliders (the floor and walls) are baked; the dynamic crates are left out (they
//! move under explosion impulses and are not part of the walkable surface).

use regia_app::prelude::*;
use regia_core::math::{Affine3A, Vec3};
use regia_nav::{NavMesh, NavMeshBuilder, NavObstacles};
use regia_physics::{BodyKind, Collider, ColliderShape, RigidBody};
use regia_scene::Transform;

/// Extracts a triangle soup from every static box/triangle-mesh collider in the world.
///
/// Sensor colliders (the pawn capsules) and dynamic bodies are skipped.
pub fn triangles_from_colliders(world: &mut World) -> Vec<[Vec3; 3]> {
    let mut triangles = Vec::new();
    let mut query = world.query::<(Entity, &Collider, &RigidBody, &Transform)>();
    for (entity, collider, body, transform) in query.iter(world) {
        if collider.sensor || body.kind != BodyKind::Static {
            continue;
        }
        let world_affine = transform.compute_affine() * collider.offset.compute_affine();
        match &collider.shape {
            ColliderShape::Box { half_extents } => {
                box_triangles(world_affine, *half_extents, &mut triangles);
            }
            ColliderShape::TriMesh { vertices, indices } => {
                for &[a, b, c] in indices {
                    triangles.push([
                        world_affine.transform_point3(vertices[a as usize]),
                        world_affine.transform_point3(vertices[b as usize]),
                        world_affine.transform_point3(vertices[c as usize]),
                    ]);
                }
            }
            _ => {} // spheres/capsules/heightfields/compounds are not navmesh geometry
        }
        let _ = entity;
    }
    triangles
}

/// The 12 triangles of an axis-aligned box at `affine` (6 faces × 2).
fn box_triangles(affine: Affine3A, half: Vec3, out: &mut Vec<[Vec3; 3]>) {
    let [hx, hy, hz] = [half.x, half.y, half.z];
    // 8 corners (indexed by bit: x=1, y=2, z=4).
    let corners = [
        Vec3::new(-hx, -hy, -hz),
        Vec3::new(hx, -hy, -hz),
        Vec3::new(hx, -hy, hz),
        Vec3::new(-hx, -hy, hz),
        Vec3::new(-hx, hy, -hz),
        Vec3::new(hx, hy, -hz),
        Vec3::new(hx, hy, hz),
        Vec3::new(-hx, hy, hz),
    ];
    let w: Vec<Vec3> = corners.iter().map(|c| affine.transform_point3(*c)).collect();
    // Faces, each two triangles (winding arbitrary: the builder flips normals to face up).
    let faces: [[usize; 4]; 6] = [
        [0, 1, 2, 3], // -Y
        [4, 7, 6, 5], // +Y
        [0, 3, 7, 4], // -X
        [1, 5, 6, 2], // +X
        [0, 4, 5, 1], // -Z
        [3, 2, 6, 7], // +Z
    ];
    for [a, b, c, d] in faces {
        out.push([w[a], w[b], w[c]]);
        out.push([w[a], w[c], w[d]]);
    }
}

/// Builds the arena navmesh from the world's static colliders and inserts it (plus the obstacle
/// set) as resources. No-op (returns `false`) when there is no walkable geometry.
pub fn build_navmesh(world: &mut World) -> bool {
    let triangles = triangles_from_colliders(world);
    let builder = NavMeshBuilder::default();
    let Ok(mesh) = builder.build(&triangles) else {
        tracing::warn!(target: "arena", "navmesh build failed (no walkable geometry)");
        return false;
    };
    tracing::info!(
        target: "arena",
        polys = mesh.poly_count(),
        regions = mesh.region_count,
        "built arena navmesh"
    );
    world.insert_resource(mesh);
    world.init_resource::<NavObstacles>();
    true
}

/// The resources the navmesh path needs, for headless tests to assert against.
pub fn navmesh_resource(world: &World) -> Option<&NavMesh> {
    world.get_resource::<NavMesh>()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A world with a static floor box and a static wall box (and one dynamic crate to ignore).
    fn collider_world() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(6.0, 0.5, 6.0),
                },
                ..Collider::default()
            },
        ));
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.0, 1.5, -6.0)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(6.0, 1.5, 0.5),
                },
                ..Collider::default()
            },
        ));
        // A dynamic crate (ignored).
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(3.0, 0.5, 3.0)),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(0.5),
                },
                ..Collider::default()
            },
        ));
        app
    }

    #[test]
    fn extracts_static_boxes_only() {
        let mut app = collider_world();
        let triangles = triangles_from_colliders(app.world_mut());
        // Two static boxes → 24 triangles; the dynamic crate is excluded.
        assert_eq!(triangles.len(), 24, "2 static boxes × 12 triangles");
    }

    #[test]
    fn builds_a_navmesh_over_the_floor() {
        let mut app = collider_world();
        assert!(build_navmesh(app.world_mut()));
        let mesh = navmesh_resource(app.world()).expect("inserted");
        assert!(mesh.poly_count() > 0);
        assert!(mesh.region_count >= 1);
    }
}
