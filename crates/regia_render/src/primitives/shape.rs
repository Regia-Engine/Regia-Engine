// SPDX-License-Identifier: MPL-2.0
//! The [`Primitive`] scene component: a parametric greybox shape that the renderer meshes.
//!
//! Scene files (`.level.ron`, `.prefab.ron`) store it through reflection like any component.
//! `format_version` follows `specs/12` § 4a: a file written by a newer engine (a version above
//! [`PRIMITIVE_FORMAT_VERSION`]) is refused with an error instead of being misread, and any change
//! to the fields after this version ships comes with a migrator in [`Primitive::migrate`].

use std::collections::HashMap;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::{Vec2, Vec3};

use super::{Capsule, CapsuleUvProfile, Cone, ConeAnchor, ConicalFrustum, Cuboid, Cylinder, CylinderAnchor};
use super::{Extrusion, Plane, Profile, Sphere, Tetrahedron, Torus};
use crate::runtime::{CpuMaterial, CpuMesh, MeshRenderer};

/// Current version of the [`Primitive`] component's serialized form.
pub const PRIMITIVE_FORMAT_VERSION: u32 = 1;

/// Largest size along any axis a primitive accepts, metres (greybox levels, not planets).
const MAX_EXTENT: f32 = 10_000.0;
/// Largest resolution/subdivision count a primitive accepts.
const MAX_RESOLUTION: u32 = 512;
/// Most outline points a prism accepts.
const MAX_PRISM_POINTS: usize = 256;

/// Why a [`Primitive`] cannot be meshed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PrimitiveError {
    /// The component was written by a newer engine.
    #[error("primitive format_version {found} is newer than this engine's {PRIMITIVE_FORMAT_VERSION}")]
    NewerVersion {
        /// The version found in the data.
        found: u32,
    },
    /// A size is not finite, not positive where it must be, or too large.
    #[error("primitive parameter `{name}` = {value} is out of range ({rule})")]
    Range {
        /// Parameter name.
        name: &'static str,
        /// The offending value.
        value: f32,
        /// What the value must satisfy.
        rule: &'static str,
    },
    /// The shape's geometry is degenerate (flat tetrahedron, zero-area prism outline).
    #[error("primitive is degenerate: {0}")]
    Degenerate(&'static str),
    /// No shape has this name.
    #[error("unknown primitive shape `{0}` (expected one of {kinds})", kinds = PrimitiveShape::KINDS.join(", "))]
    UnknownKind(String),
}

/// The parametric shapes a [`Primitive`] can be. Every shape is centred on the entity's origin.
#[derive(Reflect, Debug, Clone, PartialEq)]
#[reflect(Default, Clone)]
#[type_path = "regia_render::primitives"]
pub enum PrimitiveShape {
    /// A box.
    Cuboid {
        /// Full size along X, Y, Z, metres.
        size: Vec3,
    },
    /// A UV sphere, poles on ±Y.
    Sphere {
        /// Radius, metres.
        radius: f32,
        /// Segments around Y.
        sectors: u32,
        /// Segments pole to pole.
        stacks: u32,
    },
    /// A capsule along Y.
    Capsule {
        /// Radius, metres.
        radius: f32,
        /// Length of the cylindrical middle, metres (total height adds `2 × radius`).
        length: f32,
        /// Columns around Y.
        longitudes: u32,
        /// Rows over both hemispheres.
        latitudes: u32,
    },
    /// A cylinder along Y.
    Cylinder {
        /// Radius, metres.
        radius: f32,
        /// Height, metres.
        height: f32,
        /// Sides.
        resolution: u32,
        /// Segments along Y.
        segments: u32,
        /// Close the ends.
        caps: bool,
    },
    /// A cone along Y, tip up.
    Cone {
        /// Base radius, metres.
        radius: f32,
        /// Height, metres.
        height: f32,
        /// Sides.
        resolution: u32,
    },
    /// A cone with its top cut off, along Y.
    ConicalFrustum {
        /// Top radius, metres.
        radius_top: f32,
        /// Bottom radius, metres.
        radius_bottom: f32,
        /// Height, metres.
        height: f32,
        /// Sides.
        resolution: u32,
    },
    /// A single-sided rectangle facing +Y (a floor).
    Plane {
        /// Size along X and Z, metres.
        size: Vec2,
        /// Extra vertex rows/columns each way.
        subdivisions: u32,
    },
    /// A ring around Y.
    Torus {
        /// Tube radius, metres.
        minor_radius: f32,
        /// Ring radius to the tube centre, metres.
        major_radius: f32,
        /// Segments around the tube.
        minor_resolution: u32,
        /// Segments around the ring.
        major_resolution: u32,
    },
    /// A tetrahedron given by its corners.
    Tetrahedron {
        /// Corner positions, metres.
        vertices: [Vec3; 4],
    },
    /// A wedge rising towards +X: full size along X (run), Y (rise) and Z (width).
    Ramp {
        /// Run, rise and width, metres.
        size: Vec3,
    },
    /// An extruded simple polygon: the outline lies in XY and is swept along Z.
    Prism {
        /// Outline points in XY, metres (either winding).
        points: Vec<Vec2>,
        /// Extrusion depth along Z, metres.
        depth: f32,
    },
}

impl Default for PrimitiveShape {
    fn default() -> Self {
        Self::Cuboid { size: Vec3::ONE }
    }
}

impl PrimitiveShape {
    /// Snake-case names of every shape, in menu order (the agent protocol's `shape` values).
    pub const KINDS: &'static [&'static str] = &[
        "cuboid",
        "sphere",
        "capsule",
        "cylinder",
        "cone",
        "conical_frustum",
        "plane",
        "torus",
        "tetrahedron",
        "ramp",
        "prism",
    ];

    /// This shape's snake-case name (one of [`Self::KINDS`]).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Cuboid { .. } => "cuboid",
            Self::Sphere { .. } => "sphere",
            Self::Capsule { .. } => "capsule",
            Self::Cylinder { .. } => "cylinder",
            Self::Cone { .. } => "cone",
            Self::ConicalFrustum { .. } => "conical_frustum",
            Self::Plane { .. } => "plane",
            Self::Torus { .. } => "torus",
            Self::Tetrahedron { .. } => "tetrahedron",
            Self::Ramp { .. } => "ramp",
            Self::Prism { .. } => "prism",
        }
    }

    /// The default (roughly one-metre) shape of a kind.
    ///
    /// # Errors
    /// [`PrimitiveError::UnknownKind`] for a name not in [`Self::KINDS`].
    pub fn default_of(kind: &str) -> Result<Self, PrimitiveError> {
        Ok(match kind {
            "cuboid" => Self::default(),
            "sphere" => Self::Sphere {
                radius: 0.5,
                sectors: 32,
                stacks: 18,
            },
            "capsule" => Self::Capsule {
                radius: 0.5,
                length: 1.0,
                longitudes: 32,
                latitudes: 16,
            },
            "cylinder" => Self::Cylinder {
                radius: 0.5,
                height: 1.0,
                resolution: 32,
                segments: 1,
                caps: true,
            },
            "cone" => Self::Cone {
                radius: 0.5,
                height: 1.0,
                resolution: 32,
            },
            "conical_frustum" => Self::ConicalFrustum {
                radius_top: 0.25,
                radius_bottom: 0.5,
                height: 1.0,
                resolution: 32,
            },
            "plane" => Self::Plane {
                size: Vec2::splat(4.0),
                subdivisions: 0,
            },
            "torus" => Self::Torus {
                minor_radius: 0.15,
                major_radius: 0.5,
                minor_resolution: 24,
                major_resolution: 32,
            },
            "tetrahedron" => Self::Tetrahedron {
                vertices: Tetrahedron::default().vertices,
            },
            "ramp" => Self::Ramp {
                size: Vec3::new(2.0, 1.0, 1.0),
            },
            "prism" => Self::Prism {
                // A hexagonal column.
                points: (0..6)
                    .map(|i| Vec2::from_angle(i as f32 * std::f32::consts::TAU / 6.0) * 0.5)
                    .collect(),
                depth: 1.0,
            },
            other => return Err(PrimitiveError::UnknownKind(other.to_string())),
        })
    }

    /// Checks every parameter. Extent axes are finite, greater than 0 and at most 10 km, except
    /// a capsule's cylindrical length and either conical-frustum radius, which may be 0. Both
    /// frustum radii 0 is rejected. Resolutions are at most 512. Tetrahedra and prism outlines
    /// must not be degenerate.
    ///
    /// # Errors
    /// The first violated rule.
    pub fn validate(&self) -> Result<(), PrimitiveError> {
        fn size(name: &'static str, value: f32) -> Result<(), PrimitiveError> {
            if value.is_finite() && value > 0.0 && value <= MAX_EXTENT {
                Ok(())
            } else {
                Err(PrimitiveError::Range {
                    name,
                    value,
                    rule: "finite, > 0 and <= 10000 m",
                })
            }
        }
        fn length(name: &'static str, value: f32) -> Result<(), PrimitiveError> {
            if value.is_finite() && (0.0..=MAX_EXTENT).contains(&value) {
                Ok(())
            } else {
                Err(PrimitiveError::Range {
                    name,
                    value,
                    rule: "finite, >= 0 and <= 10000 m",
                })
            }
        }
        fn count(name: &'static str, value: u32, min: u32) -> Result<(), PrimitiveError> {
            if (min..=MAX_RESOLUTION).contains(&value) {
                Ok(())
            } else {
                Err(PrimitiveError::Range {
                    name,
                    value: value as f32,
                    rule: "an integer within the shape's minimum and 512",
                })
            }
        }
        match self {
            Self::Cuboid { size: s } | Self::Ramp { size: s } => {
                size("size.x", s.x)?;
                size("size.y", s.y)?;
                size("size.z", s.z)
            }
            Self::Sphere {
                radius,
                sectors,
                stacks,
            } => {
                size("radius", *radius)?;
                count("sectors", *sectors, 3)?;
                count("stacks", *stacks, 2)
            }
            Self::Capsule {
                radius,
                length: l,
                longitudes,
                latitudes,
            } => {
                size("radius", *radius)?;
                length("length", *l)?;
                count("longitudes", *longitudes, 3)?;
                count("latitudes", *latitudes, 4)
            }
            Self::Cylinder {
                radius,
                height,
                resolution,
                segments,
                ..
            } => {
                size("radius", *radius)?;
                size("height", *height)?;
                count("resolution", *resolution, 3)?;
                count("segments", *segments, 1)
            }
            Self::Cone {
                radius,
                height,
                resolution,
            } => {
                size("radius", *radius)?;
                size("height", *height)?;
                count("resolution", *resolution, 3)
            }
            Self::ConicalFrustum {
                radius_top,
                radius_bottom,
                height,
                resolution,
            } => {
                length("radius_top", *radius_top)?;
                length("radius_bottom", *radius_bottom)?;
                if radius_top.max(*radius_bottom) <= 0.0 {
                    return Err(PrimitiveError::Degenerate("both frustum radii are zero"));
                }
                size("height", *height)?;
                count("resolution", *resolution, 3)
            }
            Self::Plane {
                size: s,
                subdivisions,
            } => {
                size("size.x", s.x)?;
                size("size.y", s.y)?;
                count("subdivisions", *subdivisions, 0)
            }
            Self::Torus {
                minor_radius,
                major_radius,
                minor_resolution,
                major_resolution,
            } => {
                size("minor_radius", *minor_radius)?;
                size("major_radius", *major_radius)?;
                count("minor_resolution", *minor_resolution, 3)?;
                count("major_resolution", *major_resolution, 3)
            }
            Self::Tetrahedron { vertices } => {
                for v in vertices {
                    for c in v.to_array() {
                        if !c.is_finite() || c.abs() > MAX_EXTENT {
                            return Err(PrimitiveError::Range {
                                name: "vertices",
                                value: c,
                                rule: "finite and within 10000 m",
                            });
                        }
                    }
                }
                let volume = Tetrahedron { vertices: *vertices }.signed_volume().abs();
                if volume < 1.0e-9 {
                    return Err(PrimitiveError::Degenerate("tetrahedron has no volume"));
                }
                Ok(())
            }
            Self::Prism { points, depth } => {
                size("depth", *depth)?;
                if points.len() < 3 || points.len() > MAX_PRISM_POINTS {
                    return Err(PrimitiveError::Range {
                        name: "points",
                        value: points.len() as f32,
                        rule: "3 to 256 outline points",
                    });
                }
                if points
                    .iter()
                    .any(|p| !p.is_finite() || p.abs().max_element() > MAX_EXTENT)
                {
                    return Err(PrimitiveError::Degenerate(
                        "prism outline point not finite or beyond 10000 m",
                    ));
                }
                if super::extrusion::signed_area(points).abs() < 1.0e-9 {
                    return Err(PrimitiveError::Degenerate("prism outline has no area"));
                }
                Ok(())
            }
        }
    }

    /// Builds the mesh after [`Self::validate`].
    ///
    /// # Errors
    /// The validation error.
    pub fn mesh(&self) -> Result<CpuMesh, PrimitiveError> {
        self.validate()?;
        Ok(match self {
            Self::Cuboid { size } => Cuboid::new(*size).build(),
            Self::Sphere {
                radius,
                sectors,
                stacks,
            } => Sphere {
                radius: *radius,
                sectors: *sectors,
                stacks: *stacks,
            }
            .build(),
            Self::Capsule {
                radius,
                length,
                longitudes,
                latitudes,
            } => Capsule {
                radius: *radius,
                half_length: length * 0.5,
                rings: 0,
                longitudes: *longitudes,
                latitudes: *latitudes,
                uv_profile: CapsuleUvProfile::Aspect,
            }
            .build(),
            Self::Cylinder {
                radius,
                height,
                resolution,
                segments,
                caps,
            } => Cylinder {
                radius: *radius,
                half_height: height * 0.5,
                resolution: *resolution,
                segments: *segments,
                caps: *caps,
                anchor: CylinderAnchor::MidPoint,
            }
            .build(),
            Self::Cone {
                radius,
                height,
                resolution,
            } => Cone {
                radius: *radius,
                height: *height,
                resolution: *resolution,
                anchor: ConeAnchor::MidPoint,
            }
            .build(),
            Self::ConicalFrustum {
                radius_top,
                radius_bottom,
                height,
                resolution,
            } => ConicalFrustum {
                radius_top: *radius_top,
                radius_bottom: *radius_bottom,
                height: *height,
                resolution: *resolution,
                segments: 1,
            }
            .build(),
            Self::Plane { size, subdivisions } => Plane {
                normal: Vec3::Y,
                half_size: *size * 0.5,
                subdivisions_x: *subdivisions,
                subdivisions_z: *subdivisions,
            }
            .build(),
            Self::Torus {
                minor_radius,
                major_radius,
                minor_resolution,
                major_resolution,
            } => Torus {
                minor_radius: *minor_radius,
                major_radius: *major_radius,
                minor_resolution: *minor_resolution,
                major_resolution: *major_resolution,
                angle_range: (0.0, std::f32::consts::TAU),
            }
            .build(),
            Self::Tetrahedron { vertices } => Tetrahedron { vertices: *vertices }.build(),
            Self::Ramp { size } => {
                let h = *size * 0.5;
                Extrusion {
                    profile: Profile::Polygon(vec![
                        Vec2::new(-h.x, -h.y),
                        Vec2::new(h.x, -h.y),
                        Vec2::new(h.x, h.y),
                    ]),
                    half_depth: h.z,
                    segments: 1,
                }
                .build()
            }
            Self::Prism { points, depth } => Extrusion {
                profile: Profile::Polygon(points.clone()),
                half_depth: depth * 0.5,
                segments: 1,
            }
            .build(),
        })
    }

    /// A cache key: equal for equal parameters (Rust's `Debug` for floats round-trips exactly).
    fn cache_key(&self) -> String {
        format!("{self:?}")
    }
}

/// A parametric greybox shape on an entity. [`sync_primitive_meshes`] meshes it into a
/// [`MeshRenderer`] whenever it changes.
///
/// # Example
///
/// ```
/// use regia_render::primitives::{Primitive, PrimitiveShape};
///
/// let shape = PrimitiveShape::default_of("capsule").expect("known shape");
/// let primitive = Primitive::new(shape);
/// let mesh = primitive.shape.mesh().expect("valid parameters");
/// assert!(!mesh.indices.is_empty());
/// ```
///
/// Scene files name it `regia_render::primitives::Primitive` (a pinned reflection path, so moving
/// the source file never renames it in saved levels).
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
// `Clone` here: scene snapshots copy components through `reflect_clone`, which the derive
// cannot do field-wise for every shape (a silent drop from saved levels without it).
#[reflect(Component, Default, Clone)]
#[type_path = "regia_render::primitives"]
pub struct Primitive {
    /// Serialized-form version ([`PRIMITIVE_FORMAT_VERSION`] when written by this engine).
    pub format_version: u32,
    /// The shape and its parameters.
    pub shape: PrimitiveShape,
    /// Linear RGBA base colour.
    pub color: [f32; 4],
    /// Perceptual roughness stored on the component.
    ///
    /// [`Self::material`] clamps the value it renders to 0.02–1, so `0.0` is drawn as `0.02`.
    pub roughness: f32,
    /// Metalness, 0–1.
    pub metallic: f32,
}

impl Default for Primitive {
    fn default() -> Self {
        Self::new(PrimitiveShape::default())
    }
}

impl Primitive {
    /// A primitive of `shape` with the greybox look: light grey, rough, dielectric.
    #[must_use]
    pub fn new(shape: PrimitiveShape) -> Self {
        Self {
            format_version: PRIMITIVE_FORMAT_VERSION,
            shape,
            color: [0.72, 0.72, 0.72, 1.0],
            roughness: 0.65,
            metallic: 0.0,
        }
    }

    /// Brings data written by an older engine up to [`PRIMITIVE_FORMAT_VERSION`].
    ///
    /// Version 1 is the first published form, so only `0` (a hand-written file that left the
    /// field out) is upgraded, to 1.
    ///
    /// # Errors
    /// [`PrimitiveError::NewerVersion`] for data from a newer engine.
    pub fn migrate(mut self) -> Result<Self, PrimitiveError> {
        match self.format_version {
            0 => {
                self.format_version = 1;
                Ok(self)
            }
            PRIMITIVE_FORMAT_VERSION => Ok(self),
            found => Err(PrimitiveError::NewerVersion { found }),
        }
    }

    /// The render material: [`Self::color`], [`Self::roughness`] and [`Self::metallic`], clamped.
    #[must_use]
    pub fn material(&self) -> CpuMaterial {
        CpuMaterial {
            base_color: self
                .color
                .map(|c| if c.is_finite() { c.clamp(0.0, 1.0) } else { 1.0 }),
            roughness: self.roughness.clamp(0.02, 1.0),
            metallic: self.metallic.clamp(0.0, 1.0),
            ..CpuMaterial::default()
        }
    }
}

/// The serialized shape of [`Primitive`] for `cargo xtask check-formats`: every reflected field
/// (name and type) of the component and every variant and variant field of [`PrimitiveShape`], in
/// declaration order. A change here without a [`PRIMITIVE_FORMAT_VERSION`] bump fails the check.
#[must_use]
pub fn primitive_schema_fingerprint() -> String {
    use regia_ecs::reflect::TypeInfo;
    use regia_ecs::reflect::enums::VariantInfo;

    // Field types are spelled with the crate names from before the rename (ADR-1388), so the frozen
    // hash does not move with the crate names.
    let stable = regia_core::brand::fingerprint_type_path;
    let mut out = String::from("Primitive{");
    if let TypeInfo::Struct(info) = Primitive::type_info() {
        for field in info.iter() {
            out.push_str(&format!("{}:{},", field.name(), stable(field.type_path())));
        }
    }
    out.push_str("};PrimitiveShape{");
    if let TypeInfo::Enum(info) = PrimitiveShape::type_info() {
        for variant in info.iter() {
            out.push_str(variant.name());
            if let VariantInfo::Struct(fields) = variant {
                out.push('(');
                for field in fields.iter() {
                    out.push_str(&format!("{}:{},", field.name(), stable(field.type_path())));
                }
                out.push(')');
            }
            out.push('|');
        }
    }
    out.push('}');
    out
}

/// Versions the loader migrates from (`0`: a hand-written component without `format_version`).
#[must_use]
pub fn primitive_migration_sources() -> Vec<u32> {
    vec![0]
}

/// Meshes and materials of live primitives, shared by equal parameters (the renderer uploads by
/// `Arc` identity, so equal shapes upload once). Entries nothing else references are dropped
/// when the map grows past a few hundred shapes.
#[derive(Resource, Default)]
pub struct PrimitiveCache {
    meshes: HashMap<String, Arc<CpuMesh>>,
    materials: HashMap<[u32; 6], Arc<CpuMaterial>>,
}

impl PrimitiveCache {
    /// Cached entries kept before unreferenced ones are dropped.
    const SOFT_LIMIT: usize = 256;

    /// The shared mesh for `shape`.
    ///
    /// # Errors
    /// The shape's validation error.
    pub fn mesh(&mut self, shape: &PrimitiveShape) -> Result<Arc<CpuMesh>, PrimitiveError> {
        let key = shape.cache_key();
        if let Some(mesh) = self.meshes.get(&key) {
            return Ok(mesh.clone());
        }
        if self.meshes.len() >= Self::SOFT_LIMIT {
            self.meshes.retain(|_, mesh| Arc::strong_count(mesh) > 1);
        }
        let mesh = Arc::new(shape.mesh()?);
        self.meshes.insert(key, mesh.clone());
        Ok(mesh)
    }

    /// The shared material for `primitive`'s colour, roughness and metalness.
    pub fn material(&mut self, primitive: &Primitive) -> Arc<CpuMaterial> {
        let material = primitive.material();
        let c = material.base_color;
        let key = [
            c[0].to_bits(),
            c[1].to_bits(),
            c[2].to_bits(),
            c[3].to_bits(),
            material.roughness.to_bits(),
            material.metallic.to_bits(),
        ];
        if self.materials.len() >= Self::SOFT_LIMIT {
            self.materials.retain(|_, m| Arc::strong_count(m) > 1);
        }
        self.materials
            .entry(key)
            .or_insert_with(|| Arc::new(material))
            .clone()
    }

    /// Number of cached meshes.
    #[must_use]
    pub fn mesh_count(&self) -> usize {
        self.meshes.len()
    }
}

/// Marks a [`MeshRenderer`] that [`sync_primitive_meshes`] owns (removed with the [`Primitive`]).
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct PrimitiveMeshMarker;

/// Meshes every added or changed [`Primitive`] into a [`MeshRenderer`] (keeping the renderer's
/// layers), and removes that renderer when the primitive is removed. Invalid parameters or a
/// newer `format_version` log a warning and drop the mesh this system inserted
/// ([`PrimitiveMeshMarker`]). A renderer this system did not insert stays.
#[tracing::instrument(target = "regia_render", skip_all, name = "sync_primitive_meshes")]
pub fn sync_primitive_meshes(
    mut commands: Commands,
    mut cache: ResMut<PrimitiveCache>,
    changed: Query<(Entity, &Primitive, Option<&MeshRenderer>), Changed<Primitive>>,
    owned: Query<(), With<PrimitiveMeshMarker>>,
    mut removed: RemovedComponents<Primitive>,
) {
    let _span = tracing::debug_span!("sync_primitive_meshes").entered();
    for entity in removed.read() {
        if owned.contains(entity)
            && let Ok(mut entity) = commands.get_entity(entity)
        {
            entity.remove::<(MeshRenderer, PrimitiveMeshMarker)>();
        }
    }
    for (entity, primitive, existing) in &changed {
        let built = primitive
            .clone()
            .migrate()
            .and_then(|primitive| cache.mesh(&primitive.shape).map(|mesh| (primitive, mesh)));
        match built {
            Ok((primitive, mesh)) => {
                let material = cache.material(&primitive);
                let mut renderer = MeshRenderer::new(mesh, material);
                if let Some(existing) = existing {
                    renderer.layer = existing.layer;
                    renderer.decal_layer = existing.decal_layer;
                }
                commands.entity(entity).insert((renderer, PrimitiveMeshMarker));
            }
            Err(err) => {
                tracing::warn!(target: "regia_render", "primitive on {entity:?} not meshed: {err}");
                // The previous mesh would keep drawing. Drop only the one this system inserted.
                if owned.contains(entity)
                    && let Ok(mut entity) = commands.get_entity(entity)
                {
                    entity.remove::<(MeshRenderer, PrimitiveMeshMarker)>();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::check;

    #[test]
    fn every_kind_has_a_valid_closed_default() {
        for kind in PrimitiveShape::KINDS {
            let shape = PrimitiveShape::default_of(kind).expect("default");
            assert_eq!(shape.kind(), *kind);
            let mesh = shape.mesh().expect("valid default");
            check::assert_well_formed(&mesh, kind);
            if *kind == "plane" {
                assert_eq!(check::boundary_edges(&mesh), 4);
            } else {
                check::assert_watertight(&mesh, kind);
            }
        }
        assert!(matches!(
            PrimitiveShape::default_of("blob"),
            Err(PrimitiveError::UnknownKind(_))
        ));
    }

    #[test]
    fn validation_rejects_bad_parameters() {
        let bad = [
            PrimitiveShape::Cuboid {
                size: Vec3::new(1.0, -1.0, 1.0),
            },
            PrimitiveShape::Sphere {
                radius: f32::NAN,
                sectors: 8,
                stacks: 8,
            },
            PrimitiveShape::Cylinder {
                radius: 1.0,
                height: 1.0,
                resolution: 100_000,
                segments: 1,
                caps: true,
            },
            PrimitiveShape::Tetrahedron {
                vertices: [Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::new(1.0, 1.0, 0.0)],
            },
            PrimitiveShape::Prism {
                points: vec![Vec2::ZERO, Vec2::X, Vec2::X * 2.0],
                depth: 1.0,
            },
            PrimitiveShape::ConicalFrustum {
                radius_top: 0.0,
                radius_bottom: 0.0,
                height: 1.0,
                resolution: 8,
            },
        ];
        for shape in bad {
            assert!(shape.mesh().is_err(), "{shape:?} should be rejected");
        }
    }

    #[test]
    fn ramp_rises_towards_plus_x() {
        let mesh = PrimitiveShape::Ramp {
            size: Vec3::new(4.0, 1.0, 2.0),
        }
        .mesh()
        .expect("ramp");
        let top = mesh
            .vertices
            .iter()
            .filter(|v| (v.position[1] - 0.5).abs() < 1.0e-6)
            .map(|v| v.position[0]);
        assert!(top.into_iter().all(|x| (x - 2.0).abs() < 1.0e-6));
        assert_eq!(check::triangles(&mesh), 8);
    }

    #[test]
    fn migrate_upgrades_zero_and_refuses_newer() {
        let old = Primitive {
            format_version: 0,
            ..Primitive::default()
        };
        assert_eq!(
            old.migrate().map(|p| p.format_version),
            Ok(PRIMITIVE_FORMAT_VERSION)
        );
        let newer = Primitive {
            format_version: PRIMITIVE_FORMAT_VERSION + 1,
            ..Primitive::default()
        };
        assert_eq!(
            newer.migrate(),
            Err(PrimitiveError::NewerVersion {
                found: PRIMITIVE_FORMAT_VERSION + 1
            })
        );
    }

    #[test]
    fn every_shape_reflect_clones() {
        use regia_ecs::reflect::PartialReflect;
        // Level snapshots copy components with `reflect_clone`; a failure drops them from saves.
        for kind in PrimitiveShape::KINDS {
            let primitive = Primitive::new(PrimitiveShape::default_of(kind).expect("kind"));
            let cloned = primitive.reflect_clone().expect("reflect_clone");
            assert_eq!(cloned.reflect_partial_eq(&primitive), Some(true), "{kind}");
        }
    }

    #[test]
    fn scene_files_use_the_pinned_type_paths() {
        use regia_ecs::reflect::TypePath;
        assert_eq!(Primitive::type_path(), "regia_render::primitives::Primitive");
        assert_eq!(
            PrimitiveShape::type_path(),
            "regia_render::primitives::PrimitiveShape"
        );
    }

    #[test]
    fn fingerprint_lists_fields_and_variants() {
        let fingerprint = primitive_schema_fingerprint();
        assert!(fingerprint.contains("format_version:u32"), "{fingerprint}");
        assert!(fingerprint.contains("Ramp(size:glam::Vec3,)"), "{fingerprint}");
        assert_eq!(fingerprint.matches('|').count(), PrimitiveShape::KINDS.len());
    }

    #[test]
    fn cache_shares_equal_shapes() {
        let mut cache = PrimitiveCache::default();
        let a = cache.mesh(&PrimitiveShape::default()).expect("mesh");
        let b = cache.mesh(&PrimitiveShape::default()).expect("mesh");
        assert!(Arc::ptr_eq(&a, &b));
        let c = cache
            .mesh(&PrimitiveShape::Cuboid {
                size: Vec3::splat(2.0),
            })
            .expect("mesh");
        assert!(!Arc::ptr_eq(&a, &c));
        assert_eq!(cache.mesh_count(), 2);
        let m1 = cache.material(&Primitive::default());
        let m2 = cache.material(&Primitive::default());
        assert!(Arc::ptr_eq(&m1, &m2));
    }

    #[test]
    fn sync_inserts_replaces_and_removes_the_renderer() {
        let mut app = App::new();
        app.init_resource::<PrimitiveCache>()
            .add_systems(Update, sync_primitive_meshes);
        let entity = app.world_mut().spawn(Primitive::default()).id();
        app.update();
        let first = app
            .world()
            .get::<MeshRenderer>(entity)
            .map(|r| r.mesh.clone())
            .expect("renderer inserted");
        assert_eq!(first.indices.len(), 36);

        if let Some(mut renderer) = app.world_mut().get_mut::<MeshRenderer>(entity) {
            renderer.layer = 3;
        }
        if let Some(mut primitive) = app.world_mut().get_mut::<Primitive>(entity) {
            primitive.shape = PrimitiveShape::default_of("sphere").expect("sphere");
        }
        app.update();
        let renderer = app.world().get::<MeshRenderer>(entity).expect("renderer kept");
        assert!(!Arc::ptr_eq(&renderer.mesh, &first), "re-meshed on change");
        assert_eq!(renderer.layer, 3, "layers survive a re-mesh");

        app.world_mut().entity_mut(entity).remove::<Primitive>();
        app.update();
        assert!(app.world().get::<MeshRenderer>(entity).is_none());
    }

    #[test]
    fn sync_skips_invalid_primitives() {
        let mut app = App::new();
        app.init_resource::<PrimitiveCache>()
            .add_systems(Update, sync_primitive_meshes);
        let bad = Primitive {
            format_version: 99,
            ..Primitive::default()
        };
        let entity = app.world_mut().spawn(bad).id();
        app.update();
        assert!(app.world().get::<MeshRenderer>(entity).is_none());
    }

    /// A valid mesh is dropped when a later edit cannot be meshed, and a following valid edit
    /// inserts one again (`docs/formats/primitive-v1.md`).
    #[test]
    fn sync_drops_the_mesh_when_an_update_is_invalid() {
        let mut app = App::new();
        app.init_resource::<PrimitiveCache>()
            .add_systems(Update, sync_primitive_meshes);
        let entity = app.world_mut().spawn(Primitive::default()).id();
        app.update();
        assert!(app.world().get::<MeshRenderer>(entity).is_some());

        if let Some(mut primitive) = app.world_mut().get_mut::<Primitive>(entity) {
            primitive.shape = PrimitiveShape::Cuboid {
                size: Vec3::new(f32::NAN, 1.0, 1.0),
            };
        }
        app.update();
        assert!(app.world().get::<MeshRenderer>(entity).is_none());
        assert!(app.world().get::<PrimitiveMeshMarker>(entity).is_none());

        if let Some(mut primitive) = app.world_mut().get_mut::<Primitive>(entity) {
            primitive.shape = PrimitiveShape::default();
        }
        app.update();
        assert!(app.world().get::<MeshRenderer>(entity).is_some());

        if let Some(mut primitive) = app.world_mut().get_mut::<Primitive>(entity) {
            primitive.format_version = 99;
        }
        app.update();
        assert!(app.world().get::<MeshRenderer>(entity).is_none());
        assert!(app.world().get::<PrimitiveMeshMarker>(entity).is_none());
    }
}
