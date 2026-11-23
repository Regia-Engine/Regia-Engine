// SPDX-License-Identifier: MPL-2.0
//! Authored collision proxies as colliders (ADR-2341, `docs/formats/collision-set.md`).
//!
//! The glTF importer cooks every `collision_<kind>_*` node of a file into the file's `collision`
//! sub-asset ([`regia_asset::CookedCollisionSet`]). This module turns such a set into a
//! [`ColliderShape`] ([`collider_from_set`]) and, for an entity that carries
//! [`AuthoredCollision`], loads the set through the [`AssetServer`] and inserts the
//! [`Collider`] the first frame it can.
//!
//! Opt-in: nothing happens to an entity without [`AuthoredCollision`], and an explicit
//! [`Collider`] on the entity wins (the component is then ignored). The collider goes on the
//! entity itself, so it joins the entity's own [`RigidBody`], or the nearest ancestor body, or is
//! a fixed collider when there is neither (spec 15 § 2).
//!
//! A dynamic body cannot use a triangle mesh. Each `collision_mesh_*` shape becomes the convex
//! hull of its vertices when that hull exists (at least four points that enclose a volume),
//! counted in `trimesh_as_hull`, with a warning. A mesh that is too flat, or has fewer than four
//! points, stays a triangle mesh: `trimesh_as_hull` stays 0 and this attach does not warn. The
//! status is still [`AuthoredCollisionStatus::Attached`]. The fixed step then skips a dynamic
//! triangle mesh (it logs that the mesh was skipped) and inserts no rapier collider, so
//! `ColliderHandle` stays absent while the [`Collider`] component remains.

use regia_app::prelude::*;
use regia_asset::{
    AssetId, AssetServer, COLLISION_SET_SUB_PATH, CollisionProxyKind, CookedCollisionGeometry,
    CookedCollisionSet, CookedCollisionShape, load_collision_set_bytes,
};
use regia_core::math::{Quat, Vec3};
use regia_scene::Transform;

use crate::components::{BodyKind, Collider, ColliderShape, RigidBody, ShapeAxis};
use crate::convex_decomposition::convex_hull_of_points;

/// Asks for the authored collision of a cooked glTF: the first frame the [`AssetServer`] can read
/// the file's `collision` sub-asset, the entity gets a [`Collider`] built from it
/// ([`collider_from_set`]) and an [`AuthoredCollisionStatus`]. Saved in levels and prefabs.
///
/// # Example
///
/// ```
/// use regia_physics::{AuthoredCollision, RigidBody};
///
/// // A static prop whose glTF carries `collision_box_*` proxies.
/// let prop = (RigidBody::static_body(), AuthoredCollision::new("assets/props/crate.glb"));
/// assert_eq!(prop.1.mesh, "assets/props/crate.glb");
/// ```
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct AuthoredCollision {
    /// The glTF: a project-relative path (`assets/props/crate.glb`, as in `MeshSource`) or its
    /// asset id as text.
    pub mesh: String,
}

impl AuthoredCollision {
    /// Collision from the glTF at `mesh`.
    #[must_use]
    pub fn new(mesh: impl Into<String>) -> Self {
        Self { mesh: mesh.into() }
    }
}

/// What [`AuthoredCollision`] did (inserted once per entity; never saved).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub enum AuthoredCollisionStatus {
    /// A [`Collider`] was inserted with this many proxy shapes.
    Attached {
        /// Shapes in the collider (1 is a plain shape, more a compound).
        shapes: usize,
        /// `collision_mesh_*` shapes that became convex hulls because the body is dynamic.
        trimesh_as_hull: usize,
    },
    /// The glTF cooked without proxies: no collider was inserted.
    NoProxies,
    /// The glTF or its set could not be read: no collider was inserted.
    Failed {
        /// Stable lowercase code (`unknown_mesh`, `collision_set_corrupt`, ...).
        code: String,
        /// What went wrong.
        message: String,
    },
}

/// A cooked [`CollisionAxis`](regia_asset::collision_set::CollisionAxis) as a [`ShapeAxis`].
fn axis(axis: regia_asset::collision_set::CollisionAxis) -> ShapeAxis {
    use regia_asset::collision_set::CollisionAxis as A;
    match axis {
        A::X => ShapeAxis::X,
        A::Y => ShapeAxis::Y,
        A::Z => ShapeAxis::Z,
    }
}

fn vec3(v: [f32; 3]) -> Vec3 {
    Vec3::new(v[0], v[1], v[2])
}

/// One cooked shape as a collider shape and its pose in the model. With `dynamic`, a triangle mesh
/// becomes its vertices' convex hull (the second value is then `true`); a mesh too flat for a hull
/// stays a triangle mesh.
#[must_use]
pub fn shape_from_cooked(shape: &CookedCollisionShape, dynamic: bool) -> (Transform, ColliderShape, bool) {
    let rotation = Quat::from_xyzw(
        shape.rotation[0],
        shape.rotation[1],
        shape.rotation[2],
        shape.rotation[3],
    )
    .normalize();
    let pose = Transform {
        translation: vec3(shape.translation),
        rotation,
        ..Transform::IDENTITY
    };
    let (collider, as_hull) = match &shape.geometry {
        CookedCollisionGeometry::Box(b) => (
            ColliderShape::Box {
                half_extents: vec3(b.half_extents),
            },
            false,
        ),
        CookedCollisionGeometry::Sphere(s) => (ColliderShape::Sphere { radius: s.radius }, false),
        CookedCollisionGeometry::Capsule(c) => (
            ColliderShape::Capsule {
                half_height: c.half_height,
                radius: c.radius,
                axis: axis(c.axis),
            },
            false,
        ),
        CookedCollisionGeometry::Convex(c) => (
            ColliderShape::ConvexHull {
                points: c.points.iter().copied().map(vec3).collect(),
            },
            false,
        ),
        CookedCollisionGeometry::TriMesh(m) => {
            let vertices: Vec<Vec3> = m.vertices.iter().copied().map(vec3).collect();
            match dynamic.then(|| convex_hull_of_points(&vertices, 0)).flatten() {
                Some(hull) => (hull.to_collider_shape(), true),
                None => (
                    ColliderShape::TriMesh {
                        vertices,
                        indices: m.indices.clone(),
                    },
                    false,
                ),
            }
        }
    };
    (pose, collider, as_hull)
}

/// The collider of a whole set: `None` for an empty set; one shape as the collider's shape with
/// its pose as the offset; several as a [`ColliderShape::Compound`] at an identity offset. The
/// last value counts triangle meshes turned into hulls for a `dynamic` body.
#[must_use]
pub fn collider_from_set(set: &CookedCollisionSet, dynamic: bool) -> Option<(Collider, usize)> {
    let mut hulls = 0;
    let mut shapes: Vec<(Transform, ColliderShape)> = set
        .shapes
        .iter()
        .map(|shape| {
            let (pose, collider, as_hull) = shape_from_cooked(shape, dynamic);
            hulls += usize::from(as_hull);
            (pose, collider)
        })
        .collect();
    let collider = match shapes.len() {
        0 => return None,
        1 => {
            let (offset, shape) = shapes.remove(0);
            Collider {
                shape,
                offset,
                ..Collider::default()
            }
        }
        _ => Collider {
            shape: ColliderShape::Compound { shapes },
            ..Collider::default()
        },
    };
    Some((collider, hulls))
}

/// Reads the collision set of the glTF `mesh` names (a path or an asset id): `Ok(None)` when the
/// glTF is known but cooked no proxies. Errors carry a stable code.
pub fn load_authored_collision(
    server: &mut AssetServer,
    mesh: &str,
) -> Result<Option<CookedCollisionSet>, (&'static str, String)> {
    let gltf = match mesh.parse::<AssetId>() {
        Ok(id) => id,
        Err(_) => {
            let path = mesh.replace('\\', "/");
            server
                .id_for_path(&path)
                .ok_or_else(|| ("unknown_mesh", format!("no manifest entry for {path}")))?
        }
    };
    let id = AssetId::sub_asset(gltf, COLLISION_SET_SUB_PATH);
    let bytes = match server.read_cooked(id) {
        Ok((bytes, _)) => bytes,
        Err(regia_asset::AssetLoadError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(e) => return Err(("collision_set_unreadable", e.to_string())),
    };
    load_collision_set_bytes(&bytes)
        .map(|(_, set)| Some(set))
        .map_err(|e| (e.code(), e.to_string()))
}

/// Entities that asked for authored collision and have neither a collider nor a status yet.
type Pending = (Without<Collider>, Without<AuthoredCollisionStatus>);

/// Inserts the authored [`Collider`] (and an [`AuthoredCollisionStatus`]) on each new
/// [`AuthoredCollision`] entity. Runs in `PreUpdate`, so the collider reaches the body sync in the
/// same frame's fixed step. Without an [`AssetServer`] it waits.
///
/// The list is processed in the arrival order of [`AuthoredCollision`]
/// (`regia_ecs::spawn_order::SpawnOrder`, tracked by `PhysicsPlugin`): the inserted `Collider` components are
/// numbered by their own hook in that order, and rapier colliders are inserted in
/// `SpawnOrder<Collider>` order — so this order is what the contact graph and
/// `state_digest` see. Entity ids would not do: parallel `Commands::spawn` reserves them in
/// thread-timing order (ADR-1347).
pub fn attach_authored_colliders(
    pending: Query<(Entity, &AuthoredCollision, Option<&RigidBody>), Pending>,
    server: Option<ResMut<AssetServer>>,
    order: Option<Res<regia_ecs::spawn_order::SpawnOrder<AuthoredCollision>>>,
    mut commands: Commands,
) {
    let Some(mut server) = server else {
        return;
    };
    if pending.is_empty() {
        return;
    }
    let _span = tracing::info_span!("attach_authored_colliders").entered();
    let mut list: Vec<_> = pending.iter().collect();
    list.sort_by_cached_key(|(entity, ..)| crate::step::spawn_key(order.as_deref(), *entity));
    for (entity, request, body) in list {
        let dynamic = body.is_some_and(|b| b.kind == BodyKind::Dynamic);
        let status = match load_authored_collision(&mut server, &request.mesh) {
            Ok(Some(set)) => match collider_from_set(&set, dynamic) {
                Some((collider, trimesh_as_hull)) => {
                    if trimesh_as_hull > 0 {
                        tracing::warn!(
                            target: "regia_physics",
                            mesh = %request.mesh,
                            trimesh_as_hull,
                            "a dynamic body cannot use a triangle mesh: collision_mesh_* proxies collide as their convex hulls"
                        );
                    }
                    let shapes = set.shapes.len();
                    commands.entity(entity).try_insert(collider);
                    AuthoredCollisionStatus::Attached {
                        shapes,
                        trimesh_as_hull,
                    }
                }
                None => AuthoredCollisionStatus::NoProxies,
            },
            Ok(None) => AuthoredCollisionStatus::NoProxies,
            Err((code, message)) => {
                tracing::warn!(target: "regia_physics", mesh = %request.mesh, code, "authored collision: {message}");
                AuthoredCollisionStatus::Failed {
                    code: code.to_string(),
                    message,
                }
            }
        };
        if status == AuthoredCollisionStatus::NoProxies {
            tracing::warn!(
                target: "regia_physics",
                mesh = %request.mesh,
                "AuthoredCollision on a glTF without collision_* proxies: no collider"
            );
        }
        commands.entity(entity).try_insert(status);
    }
}

/// Counts of a set's shapes by kind, in [`CollisionProxyKind::ALL`] order (for reports).
#[must_use]
pub fn kind_counts(set: &CookedCollisionSet) -> [(CollisionProxyKind, usize); 5] {
    CollisionProxyKind::ALL.map(|kind| (kind, set.count(kind)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::collision_set::{
        CollisionAxis, CollisionBox, CollisionCapsule, CollisionSphere, CollisionTriMesh,
    };

    fn shape(geometry: CookedCollisionGeometry, translation: [f32; 3]) -> CookedCollisionShape {
        CookedCollisionShape {
            node: format!("collision_{}_x", geometry.kind().as_str()),
            requested: geometry.kind(),
            translation,
            rotation: [0.0, 0.0, 0.0, 1.0],
            geometry,
        }
    }

    fn tetra_mesh() -> CookedCollisionGeometry {
        CookedCollisionGeometry::TriMesh(CollisionTriMesh {
            vertices: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            indices: vec![[0, 2, 1], [0, 1, 3], [0, 3, 2], [1, 2, 3]],
        })
    }

    #[test]
    fn one_shape_is_a_plain_collider_at_its_pose() {
        let set = CookedCollisionSet::new(vec![shape(
            CookedCollisionGeometry::Box(CollisionBox {
                half_extents: [0.5, 1.0, 0.25],
            }),
            [0.0, 1.0, 0.0],
        )]);
        let (collider, hulls) = collider_from_set(&set, false).expect("collider");
        assert_eq!(hulls, 0);
        assert_eq!(
            collider.shape,
            ColliderShape::Box {
                half_extents: Vec3::new(0.5, 1.0, 0.25)
            }
        );
        assert_eq!(collider.offset.translation, Vec3::Y);
    }

    #[test]
    fn several_shapes_are_a_compound_in_node_order() {
        let set = CookedCollisionSet::new(vec![
            shape(
                CookedCollisionGeometry::Sphere(CollisionSphere { radius: 0.5 }),
                [1.0, 0.0, 0.0],
            ),
            shape(
                CookedCollisionGeometry::Capsule(CollisionCapsule {
                    half_height: 0.3,
                    radius: 0.1,
                    axis: CollisionAxis::X,
                }),
                [0.0, 0.0, 2.0],
            ),
            shape(tetra_mesh(), [0.0; 3]),
        ]);
        let (collider, hulls) = collider_from_set(&set, false).expect("collider");
        assert_eq!(hulls, 0);
        let ColliderShape::Compound { shapes } = &collider.shape else {
            panic!("expected a compound, got {:?}", collider.shape);
        };
        assert_eq!(shapes.len(), 3);
        assert_eq!(shapes[0].0.translation, Vec3::X);
        assert_eq!(shapes[0].1, ColliderShape::Sphere { radius: 0.5 });
        assert!(matches!(
            shapes[1].1,
            ColliderShape::Capsule {
                axis: ShapeAxis::X,
                ..
            }
        ));
        assert!(matches!(shapes[2].1, ColliderShape::TriMesh { .. }));
        assert_eq!(collider.offset, Transform::IDENTITY);
    }

    #[test]
    fn a_flat_dynamic_mesh_stays_a_triangle_mesh() {
        let quad = CookedCollisionGeometry::TriMesh(CollisionTriMesh {
            vertices: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]],
            indices: vec![[0, 1, 2], [0, 2, 3]],
        });
        let (collider, hulls) =
            collider_from_set(&CookedCollisionSet::new(vec![shape(quad, [0.0; 3])]), true).expect("collider");
        assert_eq!(hulls, 0);
        assert!(
            matches!(collider.shape, ColliderShape::TriMesh { .. }),
            "a planar quad has no hull, got {:?}",
            collider.shape
        );
    }

    #[test]
    fn a_dynamic_body_gets_hulls_for_triangle_meshes() {
        let set = CookedCollisionSet::new(vec![shape(tetra_mesh(), [0.0; 3])]);
        let (collider, hulls) = collider_from_set(&set, true).expect("collider");
        assert_eq!(hulls, 1);
        let ColliderShape::ConvexHull { points } = &collider.shape else {
            panic!("expected a hull, got {:?}", collider.shape);
        };
        assert_eq!(points.len(), 4);
    }

    #[test]
    fn an_empty_set_has_no_collider() {
        assert!(collider_from_set(&CookedCollisionSet::new(Vec::new()), false).is_none());
    }

    #[test]
    fn the_hull_of_points_drops_interior_points() {
        let mut points = vec![
            Vec3::ZERO,
            Vec3::X,
            Vec3::Y,
            Vec3::Z,
            Vec3::new(1.0, 1.0, 0.0),
            Vec3::new(1.0, 0.0, 1.0),
            Vec3::new(0.0, 1.0, 1.0),
            Vec3::ONE,
        ];
        points.push(Vec3::splat(0.5));
        let hull = convex_hull_of_points(&points, 0).expect("cube hull");
        assert_eq!(hull.vertices.len(), 8);
        assert!((hull.volume() - 1.0).abs() < 1e-5);
        assert!(
            convex_hull_of_points(&[Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::new(1.0, 1.0, 0.0)], 0).is_none()
        );
    }
}
