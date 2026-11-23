// SPDX-License-Identifier: MPL-2.0
// Source: C. Ericson, "Real-Time Collision Detection", Morgan Kaufmann 2005, § 4.2 (AABBs from
// vertices, in the shape's own frame) and § 4.3 (bounding sphere about the box center); C. B.
// Barber, D. P. Dobkin, H. Huhdanpaa, "The Quickhull Algorithm for Convex Hulls", ACM TOMS 22(4),
// 1996 (the hull, through `regia_physics::convex_decomposition`). Original code.

//! Authored collision proxies at import (ADR-2341, `docs/formats/gltf-conventions.md` § Collision
//! proxies): every glTF node named `collision_<kind>_*` with a mesh becomes one shape of the file's
//! [`CookedCollisionSet`] (the `collision` sub-asset). The node is still never drawn.
//!
//! | Name | Shape | How it is fitted |
//! |---|---|---|
//! | `collision_box_*` | oriented box | the vertices' box in the node's frame (its scale applied), posed by the node's rotation |
//! | `collision_sphere_*` | sphere | centered on that box, radius to the farthest vertex |
//! | `collision_capsule_*` | capsule | along the box's longest axis; radius to the farthest vertex from that axis; half height the rest |
//! | `collision_convex_*` | convex hull | quickhull of the vertices in model space, capped at [`MAX_PROXY_HULL_VERTICES`] |
//! | `collision_mesh_*` | triangle mesh | the triangles in model space (static and kinematic bodies) |
//! | any other `collision_*` (`collision_hull`, `collision_<body>_<n>`) | convex hull | as `collision_convex_*`: the hand-made hull pieces the prefix always meant |
//!
//! The fit is deterministic (no sampling, no thread order). A convex proxy whose vertices are flat
//! falls back to a box; a box or sphere that would be thinner than [`MIN_HALF_EXTENT`] is
//! thickened to it; both with a warning.

use regia_asset::collision_set::{
    CollisionAxis, CollisionBox, CollisionCapsule, CollisionConvex, CollisionSphere, CollisionTriMesh,
    MAX_COLLISION_SHAPES, MAX_TRIMESH_VERTICES,
};
use regia_asset::{CollisionProxyKind, CookedCollisionGeometry, CookedCollisionSet, CookedCollisionShape};
use regia_core::math::{Mat4, Quat, Vec3};

/// Smallest half extent (and radius) a fitted primitive gets, meters: a flat proxy (a single
/// quad) still encloses a volume.
pub const MIN_HALF_EXTENT: f32 = 0.0005;

/// Most vertices of a `collision_convex_*` hull (farthest points first).
pub const MAX_PROXY_HULL_VERTICES: u32 = 255;

/// One proxy node as the importer read it: its name, its world matrix (column-major, the
/// importer's forward-axis turn included) and its mesh's positions and triangles, in the node's
/// own space, every primitive merged.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProxySource {
    /// The node name (`collision_box_lid`).
    pub node: String,
    /// The node's world matrix, column-major.
    pub world: [f32; 16],
    /// Positions in the node's space.
    pub positions: Vec<[f32; 3]>,
    /// Triangles, indices into `positions` (empty when no primitive is a triangle list).
    pub triangles: Vec<[u32; 3]>,
}

/// The set of every proxy (`None` when no proxy produced a shape) and the warnings, one line each.
#[must_use]
pub fn cook_proxies(proxies: &[ProxySource]) -> (Option<CookedCollisionSet>, Vec<String>) {
    let _span = tracing::info_span!("cook_collision_proxies", proxies = proxies.len()).entered();
    let mut shapes = Vec::new();
    let mut warnings = Vec::new();
    for proxy in proxies {
        if shapes.len() == MAX_COLLISION_SHAPES {
            warnings.push(format!(
                "more than {MAX_COLLISION_SHAPES} collision proxies: {} and later ones are dropped",
                proxy.node
            ));
            break;
        }
        // A fitted shape is checked against the format before it is written (huge or degenerate
        // input can still overflow a size), so a cooked set always loads.
        match fit_proxy(proxy, &mut warnings).and_then(|shape| shape.validate().map(|()| shape)) {
            Ok(shape) => shapes.push(shape),
            Err(reason) => warnings.push(format!("{}: {reason}; the proxy is skipped", proxy.node)),
        }
    }
    let set = (!shapes.is_empty()).then(|| CookedCollisionSet::new(shapes));
    (set, warnings)
}

/// Fits one proxy to the shape its name asks for (see the module table). Warnings about a
/// fallback go to `warnings`; an `Err` means the proxy gives no shape at all.
pub fn fit_proxy(proxy: &ProxySource, warnings: &mut Vec<String>) -> Result<CookedCollisionShape, String> {
    let (kind, named) =
        CollisionProxyKind::from_node_name(&proxy.node).unwrap_or((CollisionProxyKind::Convex, false));
    if !named {
        tracing::debug!(node = %proxy.node, "collision proxy without a kind in its name: cooked as a convex hull");
    }
    let world = Mat4::from_cols_array(&proxy.world);
    if !world.is_finite() {
        return Err("the node transform is not finite".into());
    }
    let positions: Vec<Vec3> = proxy.positions.iter().map(|p| Vec3::from_array(*p)).collect();
    if positions.is_empty() {
        return Err("the mesh has no vertices".into());
    }
    if positions.iter().any(|p| !p.is_finite()) {
        return Err("a vertex position is not finite".into());
    }
    let shape = |requested, translation: Vec3, rotation: Quat, geometry| CookedCollisionShape {
        node: proxy.node.clone(),
        requested,
        translation: translation.to_array(),
        rotation: rotation.to_array(),
        geometry,
    };
    match kind {
        CollisionProxyKind::Box | CollisionProxyKind::Sphere | CollisionProxyKind::Capsule => {
            let fit = NodeFrame::fit(&world, &positions);
            let geometry = match kind {
                CollisionProxyKind::Box => CookedCollisionGeometry::Box(fit.cuboid(&proxy.node, warnings)),
                CollisionProxyKind::Sphere => {
                    CookedCollisionGeometry::Sphere(fit.sphere(&proxy.node, warnings))
                }
                _ => CookedCollisionGeometry::Capsule(fit.capsule(&proxy.node, warnings)),
            };
            Ok(shape(kind, fit.center_in_model(), fit.rotation, geometry))
        }
        CollisionProxyKind::Convex => {
            let model: Vec<Vec3> = positions.iter().map(|p| world.transform_point3(*p)).collect();
            match regia_physics::convex_decomposition::convex_hull_of_points(&model, MAX_PROXY_HULL_VERTICES)
            {
                Some(hull) => Ok(shape(
                    kind,
                    Vec3::ZERO,
                    Quat::IDENTITY,
                    CookedCollisionGeometry::Convex(CollisionConvex {
                        points: hull.vertices.iter().map(|p| p.to_array()).collect(),
                    }),
                )),
                None => {
                    warnings.push(format!(
                        "{}: the vertices are flat (or fewer than four), so they have no hull; cooked as a box",
                        proxy.node
                    ));
                    let fit = NodeFrame::fit(&world, &positions);
                    let geometry = CookedCollisionGeometry::Box(fit.cuboid(&proxy.node, warnings));
                    Ok(shape(kind, fit.center_in_model(), fit.rotation, geometry))
                }
            }
        }
        CollisionProxyKind::Mesh => {
            if positions.len() > MAX_TRIMESH_VERTICES || proxy.triangles.len() > MAX_TRIMESH_VERTICES {
                return Err(format!("more than {MAX_TRIMESH_VERTICES} vertices or triangles"));
            }
            let n = positions.len() as u32;
            let mirrored = world.determinant() < 0.0;
            let indices: Vec<[u32; 3]> = proxy
                .triangles
                .iter()
                .filter(|t| t.iter().all(|&i| i < n))
                .map(|&[a, b, c]| if mirrored { [a, c, b] } else { [a, b, c] })
                .collect();
            if indices.len() < proxy.triangles.len() {
                warnings.push(format!(
                    "{}: {} triangles name vertices past the mesh and are dropped",
                    proxy.node,
                    proxy.triangles.len() - indices.len()
                ));
            }
            if indices.is_empty() {
                return Err("the mesh has no triangles (a triangle mesh proxy needs a triangle list)".into());
            }
            let vertices = positions
                .iter()
                .map(|p| world.transform_point3(*p).to_array())
                .collect();
            Ok(shape(
                kind,
                Vec3::ZERO,
                Quat::IDENTITY,
                CookedCollisionGeometry::TriMesh(CollisionTriMesh { vertices, indices }),
            ))
        }
    }
}

/// The vertices in the node's frame (its scale applied, its rotation and translation not), and
/// their box there.
struct NodeFrame {
    rotation: Quat,
    translation: Vec3,
    scaled: Vec<Vec3>,
    center: Vec3,
    half: Vec3,
}

impl NodeFrame {
    fn fit(world: &Mat4, positions: &[Vec3]) -> Self {
        let (scale, rotation, translation) = world.to_scale_rotation_translation();
        let rotation = if rotation.is_finite() && rotation.length_squared() > 0.0 {
            rotation.normalize()
        } else {
            Quat::IDENTITY
        };
        let scaled: Vec<Vec3> = positions.iter().map(|p| *p * scale).collect();
        let (lo, hi) = scaled
            .iter()
            .fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(lo, hi), p| {
                (lo.min(*p), hi.max(*p))
            });
        Self {
            rotation,
            translation,
            center: (lo + hi) * 0.5,
            half: (hi - lo) * 0.5,
            scaled,
        }
    }

    /// The box center in model space.
    fn center_in_model(&self) -> Vec3 {
        self.translation + self.rotation * self.center
    }

    fn cuboid(&self, node: &str, warnings: &mut Vec<String>) -> CollisionBox {
        if self.half.min_element() < MIN_HALF_EXTENT {
            warnings.push(format!(
                "{node}: the box is flat on an axis; thickened to {MIN_HALF_EXTENT} m"
            ));
        }
        CollisionBox {
            half_extents: self.half.max(Vec3::splat(MIN_HALF_EXTENT)).to_array(),
        }
    }

    fn sphere(&self, node: &str, warnings: &mut Vec<String>) -> CollisionSphere {
        let radius = self
            .scaled
            .iter()
            .map(|p| p.distance(self.center))
            .fold(0.0_f32, f32::max);
        if radius < MIN_HALF_EXTENT {
            warnings.push(format!(
                "{node}: the sphere has no size; radius set to {MIN_HALF_EXTENT} m"
            ));
        }
        CollisionSphere {
            radius: radius.max(MIN_HALF_EXTENT),
        }
    }

    fn capsule(&self, node: &str, warnings: &mut Vec<String>) -> CollisionCapsule {
        // The longest axis; a tie keeps Y, then X, then Z (Y is the default capsule axis).
        let mut along = 1;
        for axis in [0, 2] {
            if self.half[axis] > self.half[along] {
                along = axis;
            }
        }
        let radius = self
            .scaled
            .iter()
            .map(|p| {
                let mut d = *p - self.center;
                d[along] = 0.0;
                d.length()
            })
            .fold(0.0_f32, f32::max);
        if radius < MIN_HALF_EXTENT {
            warnings.push(format!(
                "{node}: the capsule has no radius; set to {MIN_HALF_EXTENT} m"
            ));
        }
        let radius = radius.max(MIN_HALF_EXTENT);
        CollisionCapsule {
            half_height: (self.half[along] - radius).max(0.0),
            radius,
            axis: match along {
                0 => CollisionAxis::X,
                1 => CollisionAxis::Y,
                _ => CollisionAxis::Z,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY: [f32; 16] = [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ];

    /// The eight corners of a box `lo..hi` and its twelve triangles (outward, counter-clockwise).
    fn cuboid(lo: [f32; 3], hi: [f32; 3]) -> (Vec<[f32; 3]>, Vec<[u32; 3]>) {
        let mut corners = Vec::new();
        for i in 0..8u32 {
            corners.push([
                if i & 1 == 0 { lo[0] } else { hi[0] },
                if i & 2 == 0 { lo[1] } else { hi[1] },
                if i & 4 == 0 { lo[2] } else { hi[2] },
            ]);
        }
        let faces = [
            [0, 2, 3, 1],
            [4, 5, 7, 6],
            [0, 1, 5, 4],
            [2, 6, 7, 3],
            [0, 4, 6, 2],
            [1, 3, 7, 5],
        ];
        let tris = faces
            .iter()
            .flat_map(|[a, b, c, d]| [[*a, *b, *c], [*a, *c, *d]])
            .collect();
        (corners, tris)
    }

    fn proxy(
        node: &str,
        world: [f32; 16],
        (positions, triangles): (Vec<[f32; 3]>, Vec<[u32; 3]>),
    ) -> ProxySource {
        ProxySource {
            node: node.into(),
            world,
            positions,
            triangles,
        }
    }

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5)
    }

    #[test]
    fn a_box_proxy_fits_its_vertices_in_the_node_frame() {
        // A 2 × 1 × 4 box offset by (1, 0.5, 0) locally, on a node scaled by 2 in Y, turned 90°
        // about Y and moved by (10, 0, 0).
        let world = Mat4::from_scale_rotation_translation(
            Vec3::new(1.0, 2.0, 1.0),
            Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
            Vec3::new(10.0, 0.0, 0.0),
        );
        let p = proxy(
            "collision_box_lid",
            world.to_cols_array(),
            cuboid([0.0, 0.0, -2.0], [2.0, 1.0, 2.0]),
        );
        let mut warnings = Vec::new();
        let shape = fit_proxy(&p, &mut warnings).expect("box");
        assert!(warnings.is_empty(), "{warnings:?}");
        let CookedCollisionGeometry::Box(b) = &shape.geometry else {
            panic!("{:?}", shape.geometry)
        };
        assert!(close(b.half_extents, [1.0, 1.0, 2.0]), "{:?}", b.half_extents);
        // Local center (1, 0.5, 0) scaled to (1, 1, 0), turned to (0, 1, -1), moved by 10 in X.
        assert!(
            close(shape.translation, [10.0, 1.0, -1.0]),
            "{:?}",
            shape.translation
        );
        let q = Quat::from_array(shape.rotation);
        assert!(q.angle_between(Quat::from_rotation_y(std::f32::consts::FRAC_PI_2)) < 1e-5);
    }

    #[test]
    fn sphere_and_capsule_proxies_fit_their_vertices() {
        let mut warnings = Vec::new();
        // An octahedron of radius 0.5 at (0, 2, 0).
        let octa = vec![
            [0.5, 2.0, 0.0],
            [-0.5, 2.0, 0.0],
            [0.0, 2.5, 0.0],
            [0.0, 1.5, 0.0],
            [0.0, 2.0, 0.5],
            [0.0, 2.0, -0.5],
        ];
        let sphere = fit_proxy(
            &proxy("collision_sphere_head", IDENTITY, (octa, vec![])),
            &mut warnings,
        )
        .expect("sphere");
        assert_eq!(
            sphere.geometry,
            CookedCollisionGeometry::Sphere(CollisionSphere { radius: 0.5 })
        );
        assert!(close(sphere.translation, [0.0, 2.0, 0.0]));
        // A prism 0.6 wide in X and Y, 2 long in Z: a capsule along Z, radius to its corners.
        let capsule = fit_proxy(
            &proxy(
                "collision_capsule_leg",
                IDENTITY,
                cuboid([-0.3, -0.3, -1.0], [0.3, 0.3, 1.0]),
            ),
            &mut warnings,
        )
        .expect("capsule");
        let CookedCollisionGeometry::Capsule(c) = &capsule.geometry else {
            panic!("{:?}", capsule.geometry)
        };
        assert_eq!(c.axis, CollisionAxis::Z);
        let corner = (0.3_f32 * 0.3 * 2.0).sqrt();
        assert!((c.radius - corner).abs() < 1e-5, "{}", c.radius);
        assert!((c.half_height - (1.0 - corner)).abs() < 1e-5, "{}", c.half_height);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn convex_and_mesh_proxies_keep_model_space_geometry() {
        let mut warnings = Vec::new();
        let moved = Mat4::from_translation(Vec3::new(0.0, 5.0, 0.0)).to_cols_array();
        let mut cube = cuboid([0.0; 3], [1.0; 3]);
        cube.0.push([0.5, 0.5, 0.5]); // interior: not a hull vertex
        let convex = fit_proxy(&proxy("collision_convex_body", moved, cube), &mut warnings).expect("hull");
        let CookedCollisionGeometry::Convex(c) = &convex.geometry else {
            panic!("{:?}", convex.geometry)
        };
        assert_eq!(c.points.len(), 8);
        assert!(c.points.iter().all(|p| p[1] >= 5.0 - 1e-6 && p[1] <= 6.0 + 1e-6));
        let mesh = fit_proxy(
            &proxy("collision_mesh_floor", moved, cuboid([0.0; 3], [1.0; 3])),
            &mut warnings,
        )
        .expect("mesh");
        let CookedCollisionGeometry::TriMesh(m) = &mesh.geometry else {
            panic!("{:?}", mesh.geometry)
        };
        assert_eq!((m.vertices.len(), m.indices.len()), (8, 12));
        assert!(close(m.vertices[7], [1.0, 6.0, 1.0]));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_mirrored_mesh_proxy_keeps_its_winding_outward() {
        let mirror = Mat4::from_scale(Vec3::new(-1.0, 1.0, 1.0)).to_cols_array();
        let mut warnings = Vec::new();
        let plain = fit_proxy(
            &proxy("collision_mesh_a", IDENTITY, cuboid([0.0; 3], [1.0; 3])),
            &mut warnings,
        )
        .expect("plain");
        let flipped = fit_proxy(
            &proxy("collision_mesh_a", mirror, cuboid([0.0; 3], [1.0; 3])),
            &mut warnings,
        )
        .expect("mirrored");
        let (CookedCollisionGeometry::TriMesh(a), CookedCollisionGeometry::TriMesh(b)) =
            (&plain.geometry, &flipped.geometry)
        else {
            panic!("meshes")
        };
        assert_eq!(a.indices[0], [0, 2, 3]);
        assert_eq!(b.indices[0], [0, 3, 2]);
    }

    #[test]
    fn fallbacks_and_skips_are_warned_never_silent() {
        let mut warnings = Vec::new();
        // A flat quad as a convex proxy: no hull, so a (thickened) box.
        let quad = (
            vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 0.0, 1.0], [0.0, 0.0, 1.0]],
            vec![[0, 1, 2], [0, 2, 3]],
        );
        let flat = fit_proxy(
            &proxy("collision_convex_plate", IDENTITY, quad.clone()),
            &mut warnings,
        )
        .expect("box fallback");
        assert_eq!(flat.requested, CollisionProxyKind::Convex);
        assert_eq!(flat.geometry.kind(), CollisionProxyKind::Box);
        assert_eq!(warnings.len(), 2, "{warnings:?}"); // no hull, then thickened
        // A name with no kind: a hull piece, like `collision_convex_*`; flat, so a box too.
        warnings.clear();
        let legacy = fit_proxy(
            &proxy("collision_turret_0", IDENTITY, cuboid([0.0; 3], [1.0; 3])),
            &mut warnings,
        )
        .expect("hull");
        assert_eq!(legacy.geometry.kind(), CollisionProxyKind::Convex);
        assert_eq!(legacy.requested, CollisionProxyKind::Convex);
        assert!(warnings.is_empty(), "{warnings:?}");
        let legacy_flat = fit_proxy(&proxy("collision_hull", IDENTITY, quad), &mut warnings).expect("box");
        assert_eq!(legacy_flat.geometry.kind(), CollisionProxyKind::Box);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        // A mesh proxy with points only: skipped with a reason.
        let (set, warnings) = cook_proxies(&[proxy(
            "collision_mesh_dots",
            IDENTITY,
            (vec![[0.0; 3]; 3], vec![]),
        )]);
        assert!(set.is_none());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    #[test]
    fn the_cook_is_deterministic_and_in_node_order() {
        let proxies = [
            proxy("collision_box_a", IDENTITY, cuboid([0.0; 3], [1.0; 3])),
            proxy("collision_convex_b", IDENTITY, cuboid([2.0; 3], [3.0; 3])),
        ];
        let (first, _) = cook_proxies(&proxies);
        let (second, _) = cook_proxies(&proxies);
        let first = first.expect("set");
        assert_eq!(Some(&first), second.as_ref());
        let names: Vec<&str> = first.shapes.iter().map(|s| s.node.as_str()).collect();
        assert_eq!(names, ["collision_box_a", "collision_convex_b"]);
        first.validate().expect("valid");
        assert_eq!(
            first.to_postcard().expect("encode"),
            second.expect("set").to_postcard().expect("encode")
        );
    }
}
