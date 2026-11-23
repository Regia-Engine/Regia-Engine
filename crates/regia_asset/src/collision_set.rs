// SPDX-License-Identifier: MPL-2.0
//! Authored collision of an imported model (`docs/formats/collision-set.md`, ADR-2341).
//!
//! A glTF node named `collision_<kind>_<name>` is a collision proxy: the importer never draws it
//! (DO-6) and, since ADR-2341, fits a shape to its vertices and writes the shapes of every proxy of
//! the file into one [`CookedCollisionSet`], the glTF's `collision` sub-asset
//! ([`COLLISION_SET_SUB_PATH`]). A file without proxies gets no such sub-asset, so its cook is
//! byte for byte what it was before.
//!
//! Shapes are in the model's space: the space of the cooked meshes (node transforms and the
//! importer's forward-axis turn baked). `regia_physics::authored_collision` turns a set into a
//! collider when an entity asks for it.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::cooked_header::{CookedHeader, decode_cooked_file};
use crate::loader::AssetTypeTag;

/// [`CookedCollisionSet::format_version`] written by this build.
pub const COLLISION_SET_FORMAT_VERSION: u32 = 1;

/// Reflect type path of the cooked payload (the `FCKD` header's type tag hashes it).
pub const COLLISION_SET_TYPE_PATH: &str = "regia_asset::CookedCollisionSet";

/// Sub-asset path of a glTF's collision set (`AssetId::sub_asset(gltf, "collision")`).
pub const COLLISION_SET_SUB_PATH: &str = "collision";

/// Most shapes one set may hold.
pub const MAX_COLLISION_SHAPES: usize = 4096;

/// Most points of one convex shape.
pub const MAX_CONVEX_POINTS: usize = 4096;

/// Most vertices (and most triangles) of one triangle-mesh shape.
pub const MAX_TRIMESH_VERTICES: usize = 1 << 22;

/// Most bytes of one node name.
pub const MAX_NODE_NAME_BYTES: usize = 1024;

/// The kind a proxy's node name asks for (`collision_<kind>_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CollisionProxyKind {
    /// `collision_box_*`: an oriented box around the vertices, in the node's frame.
    Box,
    /// `collision_sphere_*`: the smallest sphere around the vertices centered on their box.
    Sphere,
    /// `collision_capsule_*`: a capsule along the longest axis of the node-frame box.
    Capsule,
    /// `collision_convex_*`: the convex hull of the vertices. Also every `collision_*` name with
    /// no known kind (`collision_hull`, `collision_<body>_<n>`: hand-made hull pieces, the
    /// convention the prefix had before kinds existed).
    Convex,
    /// `collision_mesh_*`: the triangles as a static triangle mesh.
    Mesh,
}

impl CollisionProxyKind {
    /// Every kind, in report order.
    pub const ALL: [Self; 5] = [Self::Box, Self::Sphere, Self::Capsule, Self::Convex, Self::Mesh];

    /// The kind's name in node names and reports (`box`, `sphere`, `capsule`, `convex`, `mesh`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Box => "box",
            Self::Sphere => "sphere",
            Self::Capsule => "capsule",
            Self::Convex => "convex",
            Self::Mesh => "mesh",
        }
    }

    /// The proxy kind of a node name: `None` when the name is not a `collision_` name; the kind and
    /// `true` for `collision_<kind>` or `collision_<kind>_<anything>`; [`Self::Convex`] and `false`
    /// for any other `collision_` name. Case-sensitive, like every reserved prefix.
    #[must_use]
    pub fn from_node_name(name: &str) -> Option<(Self, bool)> {
        let rest = name.strip_prefix("collision_")?;
        let token = rest.split('_').next().unwrap_or_default();
        let kind = Self::ALL.into_iter().find(|k| k.as_str() == token);
        Some(kind.map_or((Self::Convex, false), |k| (k, true)))
    }
}

/// A capsule's or a shape's long axis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CollisionAxis {
    /// The shape's local +X.
    X,
    /// The shape's local +Y.
    #[default]
    Y,
    /// The shape's local +Z.
    Z,
}

/// An oriented box.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CollisionBox {
    /// Half extents along the shape's local axes, meters, each above zero.
    pub half_extents: [f32; 3],
}

/// A sphere.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CollisionSphere {
    /// Radius, meters, above zero.
    pub radius: f32,
}

/// A capsule: a segment of length `2 × half_height` along `axis`, swept by `radius`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CollisionCapsule {
    /// Half the segment length, meters, zero or more (zero is a sphere).
    pub half_height: f32,
    /// Radius, meters, above zero.
    pub radius: f32,
    /// The segment's axis in the shape's frame.
    pub axis: CollisionAxis,
}

/// A convex hull.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CollisionConvex {
    /// The hull's vertices (4 to [`MAX_CONVEX_POINTS`]), in the shape's frame.
    pub points: Vec<[f32; 3]>,
}

/// A triangle mesh (static and kinematic bodies; a dynamic body uses its vertices' hull).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CollisionTriMesh {
    /// Vertices in the shape's frame.
    pub vertices: Vec<[f32; 3]>,
    /// Triangles, indices into `vertices`, counter-clockwise seen from outside.
    pub indices: Vec<[u32; 3]>,
}

/// The geometry of one cooked shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CookedCollisionGeometry {
    /// An oriented box.
    Box(CollisionBox),
    /// A sphere.
    Sphere(CollisionSphere),
    /// A capsule.
    Capsule(CollisionCapsule),
    /// A convex hull.
    Convex(CollisionConvex),
    /// A triangle mesh.
    TriMesh(CollisionTriMesh),
}

impl CookedCollisionGeometry {
    /// The proxy kind this geometry answers.
    #[must_use]
    pub fn kind(&self) -> CollisionProxyKind {
        match self {
            Self::Box(_) => CollisionProxyKind::Box,
            Self::Sphere(_) => CollisionProxyKind::Sphere,
            Self::Capsule(_) => CollisionProxyKind::Capsule,
            Self::Convex(_) => CollisionProxyKind::Convex,
            Self::TriMesh(_) => CollisionProxyKind::Mesh,
        }
    }
}

/// One cooked proxy: its node, its pose in the model and its geometry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookedCollisionShape {
    /// The glTF node the proxy came from (`collision_box_lid`).
    pub node: String,
    /// The kind the node's name asked for. Differs from [`CookedCollisionGeometry::kind`] only
    /// when the cook fell back (a convex proxy whose vertices are flat cooks as a box).
    pub requested: CollisionProxyKind,
    /// The shape frame's origin in the model, meters.
    pub translation: [f32; 3],
    /// The shape frame's rotation in the model, unit quaternion `[x, y, z, w]`.
    pub rotation: [f32; 4],
    /// The geometry in the shape frame.
    pub geometry: CookedCollisionGeometry,
}

/// Every collision proxy of an imported model (`collision` sub-asset of a glTF), in node order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookedCollisionSet {
    /// [`COLLISION_SET_FORMAT_VERSION`].
    pub format_version: u32,
    /// The shapes, in the order the importer met their nodes (document order, depth first).
    pub shapes: Vec<CookedCollisionShape>,
}

/// Errors from reading a [`CookedCollisionSet`].
#[derive(Debug, Error)]
pub enum CollisionSetError {
    /// The payload is not a postcard collision set.
    #[error("collision set decode error: {0}")]
    Postcard(#[from] postcard::Error),
    /// The cooked file wrapper is damaged.
    #[error("collision set file: {0}")]
    Cooked(#[from] crate::cooked_header::CookedIoError),
    /// The cooked file holds another asset type.
    #[error("cooked file is not a collision set (type tag {0:#x})")]
    WrongType(u64),
    /// A version this build does not read.
    #[error("collision set version {found} (this build reads {supported})")]
    UnsupportedVersion {
        /// Version in the payload.
        found: u32,
        /// [`COLLISION_SET_FORMAT_VERSION`].
        supported: u32,
    },
    /// The payload decoded but breaks a rule of the format.
    #[error("collision set shape {index}: {reason}")]
    Invalid {
        /// Index of the offending shape (0 for a set-level problem).
        index: usize,
        /// What is wrong.
        reason: String,
    },
}

impl CollisionSetError {
    /// Stable lowercase code for reports and the agent protocol.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Postcard(_) | Self::Cooked(_) => "collision_set_corrupt",
            Self::WrongType(_) => "collision_set_wrong_type",
            Self::UnsupportedVersion { .. } => "collision_set_version",
            Self::Invalid { .. } => "collision_set_invalid",
        }
    }
}

fn finite3(v: &[f32; 3]) -> bool {
    v.iter().all(|c| c.is_finite())
}

impl CookedCollisionShape {
    /// Checks one shape against the format's rules (finite numbers, positive sizes, a unit
    /// rotation, point and size limits, index ranges); the error names the broken rule.
    pub fn validate(&self) -> Result<(), String> {
        let reason = |text: &str| text.to_string();
        if self.node.len() > MAX_NODE_NAME_BYTES {
            return Err(reason("node name longer than MAX_NODE_NAME_BYTES"));
        }
        if !finite3(&self.translation) || !self.rotation.iter().all(|c| c.is_finite()) {
            return Err(reason("pose is not finite"));
        }
        let norm: f32 = self.rotation.iter().map(|c| c * c).sum::<f32>().sqrt();
        if (norm - 1.0).abs() > 1e-3 {
            return Err(reason("rotation is not a unit quaternion"));
        }
        match &self.geometry {
            CookedCollisionGeometry::Box(b) => {
                if !finite3(&b.half_extents) || b.half_extents.iter().any(|h| *h <= 0.0) {
                    return Err(reason("box half extents must be finite and above zero"));
                }
            }
            CookedCollisionGeometry::Sphere(s) => {
                if !s.radius.is_finite() || s.radius <= 0.0 {
                    return Err(reason("sphere radius must be finite and above zero"));
                }
            }
            CookedCollisionGeometry::Capsule(c) => {
                if !c.radius.is_finite()
                    || c.radius <= 0.0
                    || !c.half_height.is_finite()
                    || c.half_height < 0.0
                {
                    return Err(reason(
                        "capsule radius must be above zero and half height zero or more",
                    ));
                }
            }
            CookedCollisionGeometry::Convex(c) => {
                if c.points.len() < 4 || c.points.len() > MAX_CONVEX_POINTS {
                    return Err(reason("a convex shape needs 4 to MAX_CONVEX_POINTS points"));
                }
                if !c.points.iter().all(finite3) {
                    return Err(reason("convex points are not finite"));
                }
            }
            CookedCollisionGeometry::TriMesh(m) => {
                if m.vertices.is_empty() || m.indices.is_empty() {
                    return Err(reason("a triangle mesh needs vertices and triangles"));
                }
                if m.vertices.len() > MAX_TRIMESH_VERTICES || m.indices.len() > MAX_TRIMESH_VERTICES {
                    return Err(reason("triangle mesh larger than MAX_TRIMESH_VERTICES"));
                }
                if !m.vertices.iter().all(finite3) {
                    return Err(reason("triangle mesh vertices are not finite"));
                }
                let n = m.vertices.len();
                if m.indices.iter().flatten().any(|&v| v as usize >= n) {
                    return Err(reason("triangle index out of range"));
                }
            }
        }
        Ok(())
    }
}

impl CookedCollisionSet {
    /// An empty set at the current version.
    #[must_use]
    pub fn new(shapes: Vec<CookedCollisionShape>) -> Self {
        Self {
            format_version: COLLISION_SET_FORMAT_VERSION,
            shapes,
        }
    }

    /// How many shapes of `kind` the set holds (by geometry, after any cook fallback).
    #[must_use]
    pub fn count(&self, kind: CollisionProxyKind) -> usize {
        self.shapes.iter().filter(|s| s.geometry.kind() == kind).count()
    }

    /// Serialize to postcard bytes.
    pub fn to_postcard(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    /// Deserialize and validate. The version is read and checked before the rest.
    pub fn from_postcard(bytes: &[u8]) -> Result<Self, CollisionSetError> {
        let (version, _): (u32, _) = postcard::take_from_bytes(bytes)?;
        if version != COLLISION_SET_FORMAT_VERSION {
            return Err(CollisionSetError::UnsupportedVersion {
                found: version,
                supported: COLLISION_SET_FORMAT_VERSION,
            });
        }
        let set: Self = postcard::from_bytes(bytes)?;
        set.validate()?;
        Ok(set)
    }

    /// Checks every rule of `docs/formats/collision-set.md`: finite numbers, positive sizes, a
    /// unit rotation, convex shapes with 4 to [`MAX_CONVEX_POINTS`] points, triangle meshes with
    /// in-range indices, and the size limits.
    pub fn validate(&self) -> Result<(), CollisionSetError> {
        if self.format_version != COLLISION_SET_FORMAT_VERSION {
            return Err(CollisionSetError::UnsupportedVersion {
                found: self.format_version,
                supported: COLLISION_SET_FORMAT_VERSION,
            });
        }
        if self.shapes.len() > MAX_COLLISION_SHAPES {
            return Err(CollisionSetError::Invalid {
                index: 0,
                reason: "more shapes than MAX_COLLISION_SHAPES".into(),
            });
        }
        for (i, shape) in self.shapes.iter().enumerate() {
            shape
                .validate()
                .map_err(|reason| CollisionSetError::Invalid { index: i, reason })?;
        }
        Ok(())
    }
}

impl AssetTypeTag {
    /// Tag for [`CookedCollisionSet`].
    pub fn collision_set() -> Self {
        Self(regia_core::id::TypePathHash::of(COLLISION_SET_TYPE_PATH))
    }
}

/// Decodes a cooked collision-set file (`FCKD` header plus payload) and validates it.
pub fn load_collision_set_bytes(
    bytes: &[u8],
) -> Result<(CookedHeader, CookedCollisionSet), CollisionSetError> {
    let (header, payload) = decode_cooked_file(bytes)?;
    if header.type_tag != AssetTypeTag::collision_set().0 {
        return Err(CollisionSetError::WrongType(header.type_tag.0));
    }
    let set = CookedCollisionSet::from_postcard(payload)?;
    Ok((header, set))
}

/// The version of a cooked collision-set file, `Ok(None)` for a cooked file of another type
/// (`regia upgrade-project` probe; `.fckd` is shared by every cooked type).
pub fn collision_set_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let (header, payload) = decode_cooked_file(bytes).map_err(|e| e.to_string())?;
    if header.type_tag != AssetTypeTag::collision_set().0 {
        return Ok(None);
    }
    postcard::take_from_bytes::<u32>(payload)
        .map(|(version, _)| Some(version))
        .map_err(|e| e.to_string())
}

/// The `from` versions with a registered migration step (none: version 1 is the first).
#[must_use]
pub fn collision_set_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Serde field-list fingerprint of the collision set (for `cargo xtask check-formats`).
#[must_use]
pub fn collision_set_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<CookedCollisionSet>(),
        serde_field_names::<CookedCollisionShape>(),
        serde_variant_names::<CookedCollisionGeometry>(),
        serde_variant_names::<CollisionProxyKind>(),
        serde_variant_names::<CollisionAxis>(),
        serde_field_names::<CollisionBox>(),
        serde_field_names::<CollisionSphere>(),
        serde_field_names::<CollisionCapsule>(),
        serde_field_names::<CollisionConvex>(),
        serde_field_names::<CollisionTriMesh>(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(geometry: CookedCollisionGeometry) -> CookedCollisionShape {
        CookedCollisionShape {
            node: "collision_box_a".into(),
            requested: geometry.kind(),
            translation: [0.0, 1.0, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            geometry,
        }
    }

    fn every_kind() -> CookedCollisionSet {
        CookedCollisionSet::new(vec![
            shape(CookedCollisionGeometry::Box(CollisionBox {
                half_extents: [0.5, 0.25, 1.0],
            })),
            shape(CookedCollisionGeometry::Sphere(CollisionSphere { radius: 0.3 })),
            shape(CookedCollisionGeometry::Capsule(CollisionCapsule {
                half_height: 0.4,
                radius: 0.2,
                axis: CollisionAxis::Z,
            })),
            shape(CookedCollisionGeometry::Convex(CollisionConvex {
                points: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            })),
            shape(CookedCollisionGeometry::TriMesh(CollisionTriMesh {
                vertices: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
                indices: vec![[0, 2, 1]],
            })),
        ])
    }

    #[test]
    fn node_names_parse_to_kinds() {
        use CollisionProxyKind as K;
        assert_eq!(K::from_node_name("collision_box_lid"), Some((K::Box, true)));
        assert_eq!(K::from_node_name("collision_sphere"), Some((K::Sphere, true)));
        assert_eq!(
            K::from_node_name("collision_capsule_leg_2"),
            Some((K::Capsule, true))
        );
        assert_eq!(
            K::from_node_name("collision_convex_body"),
            Some((K::Convex, true))
        );
        assert_eq!(K::from_node_name("collision_mesh_floor"), Some((K::Mesh, true)));
        // A name with no known kind keeps the old meaning: a hand-made hull piece.
        assert_eq!(K::from_node_name("collision_hull"), Some((K::Convex, false)));
        assert_eq!(K::from_node_name("collision_turret_0"), Some((K::Convex, false)));
        assert_eq!(K::from_node_name("collision_boxy"), Some((K::Convex, false)));
        assert_eq!(K::from_node_name("collision_"), Some((K::Convex, false)));
        // Case-sensitive, like the other reserved prefixes.
        assert_eq!(K::from_node_name("collision_Box_a"), Some((K::Convex, false)));
        assert_eq!(K::from_node_name("Collision_box_a"), None);
        assert_eq!(K::from_node_name("crate"), None);
    }

    #[test]
    fn a_set_round_trips_and_counts_by_kind() {
        let set = every_kind();
        let bytes = set.to_postcard().expect("encode");
        let back = CookedCollisionSet::from_postcard(&bytes).expect("decode");
        assert_eq!(back, set);
        for kind in CollisionProxyKind::ALL {
            assert_eq!(back.count(kind), 1, "{kind:?}");
        }
    }

    #[test]
    fn a_newer_version_is_refused_before_the_body() {
        let mut set = every_kind();
        set.format_version = 2;
        let bytes = set.to_postcard().expect("encode");
        let err = CookedCollisionSet::from_postcard(&bytes).expect_err("v2");
        assert_eq!(err.code(), "collision_set_version");
    }

    #[test]
    fn invalid_shapes_are_refused() {
        let cases: Vec<CookedCollisionGeometry> = vec![
            CookedCollisionGeometry::Box(CollisionBox {
                half_extents: [0.0, 1.0, 1.0],
            }),
            CookedCollisionGeometry::Sphere(CollisionSphere { radius: f32::NAN }),
            CookedCollisionGeometry::Capsule(CollisionCapsule {
                half_height: -1.0,
                radius: 0.1,
                axis: CollisionAxis::Y,
            }),
            CookedCollisionGeometry::Convex(CollisionConvex {
                points: vec![[0.0; 3]; 3],
            }),
            CookedCollisionGeometry::TriMesh(CollisionTriMesh {
                vertices: vec![[0.0; 3]],
                indices: vec![[0, 0, 1]],
            }),
        ];
        for geometry in cases {
            let set = CookedCollisionSet::new(vec![shape(geometry.clone())]);
            let err = set.validate().expect_err("invalid");
            assert_eq!(err.code(), "collision_set_invalid", "{geometry:?}");
        }
        let mut bad_pose = shape(CookedCollisionGeometry::Sphere(CollisionSphere { radius: 1.0 }));
        bad_pose.rotation = [0.0, 0.0, 0.0, 2.0];
        assert!(CookedCollisionSet::new(vec![bad_pose]).validate().is_err());
    }

    #[test]
    fn the_file_probe_skips_other_cooked_types() {
        let payload = every_kind().to_postcard().expect("encode");
        let file = |tag: AssetTypeTag| {
            let header = CookedHeader {
                format_version: crate::COOKED_FORMAT_VERSION,
                type_tag: tag.0,
                asset_id: crate::AssetId(regia_core::id::Guid::from_u128(7)),
                importer_version: crate::pack_importer_version(1, 11, 0),
                dependencies: vec![],
                payload_len: payload.len() as u64,
            };
            crate::encode_cooked_file(&header, &payload).expect("encode file")
        };
        assert_eq!(
            collision_set_file_version(&file(AssetTypeTag::collision_set())),
            Ok(Some(1))
        );
        assert_eq!(collision_set_file_version(&file(AssetTypeTag::model())), Ok(None));
        let (_, set) = load_collision_set_bytes(&file(AssetTypeTag::collision_set())).expect("load");
        assert_eq!(set.shapes.len(), 5);
        let err = load_collision_set_bytes(&file(AssetTypeTag::model())).expect_err("wrong type");
        assert_eq!(err.code(), "collision_set_wrong_type");
    }

    #[test]
    fn the_fingerprint_names_every_struct() {
        let print = collision_set_schema_fingerprint();
        for name in [
            "half_extents",
            "radius",
            "half_height",
            "points",
            "indices",
            "requested",
            "TriMesh",
        ] {
            assert!(print.contains(name), "{name} missing from {print}");
        }
    }
}
