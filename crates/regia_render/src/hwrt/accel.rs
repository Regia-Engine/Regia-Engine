// SPDX-License-Identifier: MPL-2.0
//! Acceleration-structure build abstraction (ADR-515).
//!
//! Two halves:
//!
//! - [`AccelPlanner`] (CPU, always available): turns the meshes and instances a host wants
//!   traceable into one bottom-level structure (BLAS) per instanced mesh and one top-level
//!   instance list, checks them against the device's acceleration-structure limits, and tracks
//!   which BLAS are already built so only new or changed meshes rebuild. This is the part every
//!   test runs: it needs no GPU.
//! - [`GpuAccel`] (GPU): executes a plan with wgpu 30's `create_blas` / `create_tlas` /
//!   `build_acceleration_structures`. It can only be constructed on a device that has
//!   `EXPERIMENTAL_RAY_QUERY` (the runtime gate: a profile that asks for it on an adapter that does),
//!   so nothing else in the engine can reach the experimental API by accident.
//!
//! The split mirrors the bottom-level / top-level model of the Vulkan and DirectX ray-tracing
//! specifications and of Pharr, Jakob, Humphreys, *Physically Based Rendering* 4th ed., ch. 7
//! (instancing over shared per-mesh hierarchies).

use std::collections::HashMap;

use super::probe::AccelLimits;

/// Largest `custom_data` a top-level instance can carry (24 bits).
pub const MAX_INSTANCE_CUSTOM_DATA: u32 = (1 << 24) - 1;

/// A mesh that instances may reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccelMesh {
    /// Stable identity (for example a mesh id's bits).
    pub key: u64,
    /// Content version: a different value than the last build rebuilds the BLAS.
    pub generation: u64,
    /// Vertices (`Float32x3` positions).
    pub vertex_count: u32,
    /// `u32` indices; `0` for a non-indexed triangle list.
    pub index_count: u32,
}

impl AccelMesh {
    /// Elements that form triangles: the index count, or the vertex count when non-indexed.
    #[must_use]
    pub fn element_count(&self) -> u32 {
        if self.index_count > 0 {
            self.index_count
        } else {
            self.vertex_count
        }
    }

    /// Triangles in the mesh.
    #[must_use]
    pub fn triangle_count(&self) -> u32 {
        self.element_count() / 3
    }
}

/// One placement of a mesh in the top-level structure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AccelInstance {
    /// [`AccelMesh::key`] of the placed mesh.
    pub mesh: u64,
    /// Object-to-world matrix, column-major (`glam::Mat4::to_cols_array_2d`). The bottom row
    /// is ignored: acceleration structures take affine transforms.
    pub transform: [[f32; 4]; 4],
    /// Value the shader reads back for a hit (24 bits, at most [`MAX_INSTANCE_CUSTOM_DATA`]).
    pub custom_data: u32,
    /// Visibility mask: a ray sees the instance when `ray_mask & mask != 0`.
    pub mask: u8,
}

/// One bottom-level structure of a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlasPlan {
    /// The mesh.
    pub key: u64,
    /// The mesh's content version.
    pub generation: u64,
    /// Vertices.
    pub vertex_count: u32,
    /// Indices (`0` = non-indexed).
    pub index_count: u32,
    /// The BLAS is missing or stale and must be (re)built.
    pub build: bool,
}

/// One top-level instance of a plan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TlasInstancePlan {
    /// Index into [`AccelPlan::blas`].
    pub blas: usize,
    /// Row-major 3×4 affine transform ([`tlas_transform`]).
    pub transform: [f32; 12],
    /// Custom data (24 bits).
    pub custom_data: u32,
    /// Visibility mask.
    pub mask: u8,
}

/// What one build does: the BLAS set, the TLAS contents and the BLAS to drop.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AccelPlan {
    /// One entry per instanced mesh, in order of first reference.
    pub blas: Vec<BlasPlan>,
    /// The top-level instances, in input order.
    pub instances: Vec<TlasInstancePlan>,
    /// Built meshes no longer in the mesh list: their BLAS is released. Sorted.
    pub evict: Vec<u64>,
}

impl AccelPlan {
    /// BLAS this plan (re)builds.
    #[must_use]
    pub fn builds(&self) -> usize {
        self.blas.iter().filter(|b| b.build).count()
    }

    /// Triangles in the BLAS this plan (re)builds: the build's cost driver.
    #[must_use]
    pub fn build_triangles(&self) -> u64 {
        self.blas
            .iter()
            .filter(|b| b.build)
            .map(|b| {
                let elements = if b.index_count > 0 {
                    b.index_count
                } else {
                    b.vertex_count
                };
                u64::from(elements / 3)
            })
            .sum()
    }
}

/// Why a plan was refused. Nothing is built for a refused plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccelPlanError {
    /// An instance references a mesh that is not in the mesh list.
    MissingMesh {
        /// Index of the instance.
        instance: usize,
        /// The key it references.
        mesh: u64,
    },
    /// Two meshes share a key.
    DuplicateMesh {
        /// The key.
        mesh: u64,
    },
    /// The mesh is not a triangle list: no elements, or a count that is not a multiple of 3.
    NotTriangles {
        /// The mesh.
        mesh: u64,
        /// Its index (or vertex) count.
        elements: u32,
    },
    /// The mesh has more triangles than one BLAS may hold.
    TooManyPrimitives {
        /// The mesh.
        mesh: u64,
        /// Its triangles.
        triangles: u32,
        /// The device limit.
        max: u32,
    },
    /// More instances than one TLAS may hold.
    TooManyInstances {
        /// Instances asked for.
        count: usize,
        /// The device limit.
        max: u32,
    },
    /// An instance's custom data does not fit in 24 bits.
    CustomDataTooWide {
        /// Index of the instance.
        instance: usize,
        /// The value.
        value: u32,
    },
    /// An instance's transform has a NaN or infinite element.
    NonFiniteTransform {
        /// Index of the instance.
        instance: usize,
    },
}

impl AccelPlanError {
    /// Stable lowercase code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingMesh { .. } => "accel_missing_mesh",
            Self::DuplicateMesh { .. } => "accel_duplicate_mesh",
            Self::NotTriangles { .. } => "accel_not_triangles",
            Self::TooManyPrimitives { .. } => "accel_too_many_primitives",
            Self::TooManyInstances { .. } => "accel_too_many_instances",
            Self::CustomDataTooWide { .. } => "accel_custom_data_too_wide",
            Self::NonFiniteTransform { .. } => "accel_non_finite_transform",
        }
    }
}

impl std::fmt::Display for AccelPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingMesh { instance, mesh } => {
                write!(
                    f,
                    "instance {instance} references mesh {mesh:#x}, which is not in the mesh list"
                )
            }
            Self::DuplicateMesh { mesh } => write!(f, "mesh {mesh:#x} is listed twice"),
            Self::NotTriangles { mesh, elements } => {
                write!(
                    f,
                    "mesh {mesh:#x} has {elements} elements, not a non-empty triangle list"
                )
            }
            Self::TooManyPrimitives { mesh, triangles, max } => {
                write!(
                    f,
                    "mesh {mesh:#x} has {triangles} triangles; one BLAS holds at most {max}"
                )
            }
            Self::TooManyInstances { count, max } => {
                write!(f, "{count} instances; one TLAS holds at most {max}")
            }
            Self::CustomDataTooWide { instance, value } => {
                write!(
                    f,
                    "instance {instance} custom data {value:#x} does not fit in 24 bits"
                )
            }
            Self::NonFiniteTransform { instance } => {
                write!(f, "instance {instance} has a non-finite transform")
            }
        }
    }
}

impl std::error::Error for AccelPlanError {}

/// Row-major 3×4 affine transform (the TLAS instance layout) of a column-major 4×4 matrix.
#[must_use]
pub fn tlas_transform(m: &[[f32; 4]; 4]) -> [f32; 12] {
    [
        m[0][0], m[1][0], m[2][0], m[3][0], //
        m[0][1], m[1][1], m[2][1], m[3][1], //
        m[0][2], m[1][2], m[2][2], m[3][2],
    ]
}

/// Plans acceleration-structure builds and remembers what was built.
#[derive(Debug, Clone, Default)]
pub struct AccelPlanner {
    built: HashMap<u64, u64>,
}

impl AccelPlanner {
    /// A planner with nothing built.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Plans a build of `instances` over `meshes` within `limits`. Pass every instance that
    /// should be traceable (not a view-culled list): the TLAS is rebuilt from this list each
    /// time. Meshes in `meshes` that no instance references keep their BLAS if built; meshes
    /// dropped from `meshes` are evicted.
    pub fn plan(
        &self,
        meshes: &[AccelMesh],
        instances: &[AccelInstance],
        limits: &AccelLimits,
    ) -> Result<AccelPlan, AccelPlanError> {
        if instances.len() > limits.max_tlas_instance_count as usize {
            return Err(AccelPlanError::TooManyInstances {
                count: instances.len(),
                max: limits.max_tlas_instance_count,
            });
        }
        let mut by_key: HashMap<u64, &AccelMesh> = HashMap::with_capacity(meshes.len());
        for mesh in meshes {
            if by_key.insert(mesh.key, mesh).is_some() {
                return Err(AccelPlanError::DuplicateMesh { mesh: mesh.key });
            }
        }
        let mut plan = AccelPlan::default();
        let mut slot: HashMap<u64, usize> = HashMap::new();
        for (i, instance) in instances.iter().enumerate() {
            if instance.custom_data > MAX_INSTANCE_CUSTOM_DATA {
                return Err(AccelPlanError::CustomDataTooWide {
                    instance: i,
                    value: instance.custom_data,
                });
            }
            if instance.transform.iter().flatten().any(|c| !c.is_finite()) {
                return Err(AccelPlanError::NonFiniteTransform { instance: i });
            }
            let blas = match slot.get(&instance.mesh) {
                Some(&b) => b,
                None => {
                    let mesh = by_key.get(&instance.mesh).ok_or(AccelPlanError::MissingMesh {
                        instance: i,
                        mesh: instance.mesh,
                    })?;
                    let elements = mesh.element_count();
                    if elements == 0 || !elements.is_multiple_of(3) {
                        return Err(AccelPlanError::NotTriangles {
                            mesh: mesh.key,
                            elements,
                        });
                    }
                    if mesh.triangle_count() > limits.max_blas_primitive_count {
                        return Err(AccelPlanError::TooManyPrimitives {
                            mesh: mesh.key,
                            triangles: mesh.triangle_count(),
                            max: limits.max_blas_primitive_count,
                        });
                    }
                    let build = self.built.get(&mesh.key) != Some(&mesh.generation);
                    plan.blas.push(BlasPlan {
                        key: mesh.key,
                        generation: mesh.generation,
                        vertex_count: mesh.vertex_count,
                        index_count: mesh.index_count,
                        build,
                    });
                    slot.insert(mesh.key, plan.blas.len() - 1);
                    plan.blas.len() - 1
                }
            };
            plan.instances.push(TlasInstancePlan {
                blas,
                transform: tlas_transform(&instance.transform),
                custom_data: instance.custom_data,
                mask: instance.mask,
            });
        }
        plan.evict = self
            .built
            .keys()
            .filter(|k| !by_key.contains_key(*k))
            .copied()
            .collect();
        plan.evict.sort_unstable();
        Ok(plan)
    }

    /// Records that `plan` was executed: its builds are current, its evictions gone.
    pub fn commit(&mut self, plan: &AccelPlan) {
        for key in &plan.evict {
            self.built.remove(key);
        }
        for b in plan.blas.iter().filter(|b| b.build) {
            self.built.insert(b.key, b.generation);
        }
    }

    /// BLAS currently recorded as built.
    #[must_use]
    pub fn built_count(&self) -> usize {
        self.built.len()
    }

    /// Forgets every build (after the device is lost, say).
    pub fn clear(&mut self) {
        self.built.clear();
    }
}

/// The GPU buffers of one mesh for a BLAS build. Both buffers need
/// `wgpu::BufferUsages::BLAS_INPUT`.
#[derive(Debug, Clone, Copy)]
pub struct GpuAccelMesh<'a> {
    /// [`AccelMesh::key`].
    pub key: u64,
    /// `Float32x3` positions.
    pub vertex_buffer: &'a wgpu::Buffer,
    /// First vertex in `vertex_buffer`.
    pub first_vertex: u32,
    /// Bytes between consecutive positions.
    pub vertex_stride: u64,
    /// `u32` indices; `None` for a non-indexed mesh (then [`AccelMesh::index_count`] is 0).
    pub index_buffer: Option<&'a wgpu::Buffer>,
    /// First index in `index_buffer`.
    pub first_index: u32,
    /// Build the structure opaque (every hit commits at once). `false` makes its triangles
    /// candidates a ray query must confirm: the alpha-masked caster proxy (ADR-2310).
    pub opaque: bool,
}

/// Why [`GpuAccel`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccelError {
    /// The device does not have `EXPERIMENTAL_RAY_QUERY` (see [`super::HwrtReason`]).
    RayQueryDisabled,
    /// The plan was refused.
    Plan(AccelPlanError),
    /// A mesh to build has no [`GpuAccelMesh`], or its index buffer disagrees with its
    /// index count.
    MeshBuffers {
        /// The mesh.
        mesh: u64,
    },
    /// [`GpuAccel::refit`] was asked for a scene that is not the last build's: another instance
    /// count, another mesh in some slot, or nothing built yet. The caller builds instead.
    RefitShape,
}

impl AccelError {
    /// Stable lowercase code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::RayQueryDisabled => "accel_ray_query_disabled",
            Self::Plan(e) => e.code(),
            Self::MeshBuffers { .. } => "accel_mesh_buffers",
            Self::RefitShape => "accel_refit_shape",
        }
    }
}

impl std::fmt::Display for AccelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RayQueryDisabled => f.write_str("the device has no ray queries (EXPERIMENTAL_RAY_QUERY)"),
            Self::Plan(e) => write!(f, "{e}"),
            Self::MeshBuffers { mesh } => {
                write!(f, "mesh {mesh:#x} has no matching GPU buffers for its BLAS build")
            }
            Self::RefitShape => f.write_str("a refit needs the last build's instances in the same slots"),
        }
    }
}

impl std::error::Error for AccelError {}

impl From<AccelPlanError> for AccelError {
    fn from(e: AccelPlanError) -> Self {
        Self::Plan(e)
    }
}

/// What one [`GpuAccel::build`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AccelBuildStats {
    /// BLAS built (new or changed meshes).
    pub blas_built: usize,
    /// BLAS released (meshes dropped from the list).
    pub blas_evicted: usize,
    /// BLAS held after the build.
    pub blas_resident: usize,
    /// TLAS instances.
    pub instances: usize,
    /// Triangles in the BLAS built.
    pub triangles_built: u64,
    /// The TLAS was recreated (first build, or more instances than it held).
    pub tlas_recreated: bool,
    /// This was a refit ([`GpuAccel::refit`]): the same instances in the same slots took new
    /// transforms, no bottom-level structure was touched and the top level kept its storage.
    pub refit: bool,
    /// Slots whose transform the refit rewrote (0 on a build).
    pub instances_moved: usize,
}

/// Acceleration structures on a ray-query device: the BLAS per mesh and one TLAS.
#[derive(Debug)]
pub struct GpuAccel {
    planner: AccelPlanner,
    blas: HashMap<u64, wgpu::Blas>,
    tlas: Option<wgpu::Tlas>,
    tlas_capacity: u32,
    limits: AccelLimits,
    /// The last build's slots: mesh key and instance (what [`Self::refit`] compares against).
    slots: Vec<(u64, TlasInstancePlan)>,
}

impl GpuAccel {
    /// Fewest instance slots a TLAS is created with.
    const MIN_TLAS_CAPACITY: u32 = 16;

    /// The builder for `device`; [`AccelError::RayQueryDisabled`] unless the device has
    /// `EXPERIMENTAL_RAY_QUERY`.
    pub fn new(device: &wgpu::Device) -> Result<Self, AccelError> {
        if !device.features().contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY) {
            return Err(AccelError::RayQueryDisabled);
        }
        Ok(Self {
            planner: AccelPlanner::new(),
            blas: HashMap::new(),
            tlas: None,
            tlas_capacity: 0,
            limits: AccelLimits::from_limits(&device.limits()),
            slots: Vec::new(),
        })
    }

    /// Plans and records the builds for `instances` into `encoder`: new or changed BLAS, then
    /// the TLAS. Submit `encoder` before (or with) the work that traces [`Self::tlas`].
    /// On an error nothing is recorded and the previous structures stay as they were.
    pub fn build(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        meshes: &[AccelMesh],
        buffers: &[GpuAccelMesh<'_>],
        instances: &[AccelInstance],
    ) -> Result<AccelBuildStats, AccelError> {
        let _span = tracing::info_span!("hwrt_accel_build").entered();
        let plan = self.planner.plan(meshes, instances, &self.limits)?;
        // Every mesh to build needs matching buffers before anything is created.
        let mut sources: Vec<(usize, GpuAccelMesh<'_>)> = Vec::with_capacity(plan.builds());
        for (i, b) in plan.blas.iter().enumerate().filter(|(_, b)| b.build) {
            let source = buffers
                .iter()
                .find(|m| m.key == b.key)
                .filter(|m| m.index_buffer.is_some() == (b.index_count > 0))
                .ok_or(AccelError::MeshBuffers { mesh: b.key })?;
            sources.push((i, *source));
        }
        for key in &plan.evict {
            self.blas.remove(key);
        }
        let sizes: Vec<wgpu::BlasTriangleGeometrySizeDescriptor> = sources
            .iter()
            .map(|(i, source)| {
                let b = &plan.blas[*i];
                let indexed = b.index_count > 0;
                wgpu::BlasTriangleGeometrySizeDescriptor {
                    vertex_format: wgpu::VertexFormat::Float32x3,
                    vertex_count: b.vertex_count,
                    index_format: indexed.then_some(wgpu::IndexFormat::Uint32),
                    index_count: indexed.then_some(b.index_count),
                    flags: if source.opaque {
                        wgpu::AccelerationStructureGeometryFlags::OPAQUE
                    } else {
                        wgpu::AccelerationStructureGeometryFlags::empty()
                    },
                }
            })
            .collect();
        for ((i, _), size) in sources.iter().zip(&sizes) {
            let blas = device.create_blas(
                &wgpu::CreateBlasDescriptor {
                    label: Some("hwrt blas"),
                    flags: Self::acceleration_structure_flags(device),
                    update_mode: wgpu::AccelerationStructureUpdateMode::Build,
                },
                wgpu::BlasGeometrySizeDescriptors::Triangles {
                    descriptors: vec![size.clone()],
                },
            );
            self.blas.insert(plan.blas[*i].key, blas);
        }
        let needed = (plan.instances.len() as u32).max(1);
        let tlas_recreated = self.tlas.is_none() || self.tlas_capacity < needed;
        if tlas_recreated {
            let capacity = needed
                .next_power_of_two()
                .max(Self::MIN_TLAS_CAPACITY)
                .min(self.limits.max_tlas_instance_count.max(needed));
            self.tlas = Some(device.create_tlas(&wgpu::CreateTlasDescriptor {
                label: Some("hwrt tlas"),
                max_instances: capacity,
                flags: Self::acceleration_structure_flags(device),
                update_mode: wgpu::AccelerationStructureUpdateMode::Build,
            }));
            self.tlas_capacity = capacity;
        }
        if let Some(tlas) = self.tlas.as_mut() {
            for (slot, instance) in plan.instances.iter().enumerate() {
                let key = plan.blas[instance.blas].key;
                tlas[slot] = self.blas.get(&key).map(|blas| {
                    wgpu::TlasInstance::new(blas, instance.transform, instance.custom_data, instance.mask)
                });
            }
            for slot in &mut tlas[plan.instances.len()..self.tlas_capacity as usize] {
                *slot = None;
            }
        }
        let entries: Vec<wgpu::BlasBuildEntry<'_>> = sources
            .iter()
            .zip(&sizes)
            .filter_map(|((i, source), size)| {
                let blas = self.blas.get(&plan.blas[*i].key)?;
                Some(wgpu::BlasBuildEntry {
                    blas,
                    geometry: wgpu::BlasGeometries::TriangleGeometries(vec![wgpu::BlasTriangleGeometry {
                        size,
                        vertex_buffer: source.vertex_buffer,
                        first_vertex: source.first_vertex,
                        vertex_stride: source.vertex_stride,
                        index_buffer: source.index_buffer,
                        first_index: source.index_buffer.map(|_| source.first_index),
                        transform_buffer: None,
                        transform_buffer_offset: None,
                    }]),
                })
            })
            .collect();
        encoder.build_acceleration_structures(entries.iter(), self.tlas.iter());
        self.planner.commit(&plan);
        self.slots.clear();
        self.slots
            .extend(plan.instances.iter().map(|i| (plan.blas[i.blas].key, *i)));
        Ok(AccelBuildStats {
            blas_built: sources.len(),
            blas_evicted: plan.evict.len(),
            blas_resident: self.blas.len(),
            instances: plan.instances.len(),
            triangles_built: plan.build_triangles(),
            tlas_recreated,
            refit: false,
            instances_moved: 0,
        })
    }

    /// Moves the last build's instances: `instances` must be that build's, slot for slot (the
    /// same count, the same mesh in every slot), with new transforms, custom data or masks. Only
    /// the slots that changed are rewritten, no bottom-level structure is touched, and the top
    /// level keeps its storage; the top level's build is recorded again into `encoder` (wgpu 30
    /// implements acceleration-structure updates as rebuilds, ADR-2310, so the GPU work is a
    /// top-level build over the instances, never a bottom-level one).
    /// [`AccelError::RefitShape`] when the scene is not the last build's: build instead.
    pub fn refit(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        instances: &[AccelInstance],
    ) -> Result<AccelBuildStats, AccelError> {
        let _span = tracing::info_span!("hwrt_accel_refit").entered();
        let Some(tlas) = self.tlas.as_mut() else {
            return Err(AccelError::RefitShape);
        };
        if instances.len() != self.slots.len()
            || instances
                .iter()
                .zip(&self.slots)
                .any(|(instance, (key, _))| instance.mesh != *key)
        {
            return Err(AccelError::RefitShape);
        }
        for (i, instance) in instances.iter().enumerate() {
            if instance.custom_data > MAX_INSTANCE_CUSTOM_DATA {
                return Err(AccelPlanError::CustomDataTooWide {
                    instance: i,
                    value: instance.custom_data,
                }
                .into());
            }
            if instance.transform.iter().flatten().any(|c| !c.is_finite()) {
                return Err(AccelPlanError::NonFiniteTransform { instance: i }.into());
            }
        }
        let mut moved = 0;
        for (slot, (instance, (key, last))) in instances.iter().zip(self.slots.iter_mut()).enumerate() {
            let next = TlasInstancePlan {
                blas: last.blas,
                transform: tlas_transform(&instance.transform),
                custom_data: instance.custom_data,
                mask: instance.mask,
            };
            if next == *last {
                continue;
            }
            tlas[slot] = self
                .blas
                .get(key)
                .map(|blas| wgpu::TlasInstance::new(blas, next.transform, next.custom_data, next.mask));
            *last = next;
            moved += 1;
        }
        if moved > 0 {
            encoder.build_acceleration_structures(
                std::iter::empty::<&wgpu::BlasBuildEntry<'_>>(),
                std::iter::once(&*tlas),
            );
        }
        Ok(AccelBuildStats {
            blas_built: 0,
            blas_evicted: 0,
            blas_resident: self.blas.len(),
            instances: instances.len(),
            triangles_built: 0,
            tlas_recreated: false,
            refit: true,
            instances_moved: moved,
        })
    }

    /// Instances in the top level of the last build or refit.
    #[must_use]
    pub fn instance_count(&self) -> usize {
        self.slots.len()
    }

    /// The top-level structure of the last build (`None` before the first).
    #[must_use]
    pub fn tlas(&self) -> Option<&wgpu::Tlas> {
        self.tlas.as_ref()
    }

    /// The planner's view of what is built.
    #[must_use]
    pub fn planner(&self) -> &AccelPlanner {
        &self.planner
    }

    /// Acceleration-structure flags for this device: fast tracing plus hit-vertex return when the
    /// device enabled it. `GpuAccel` already requires ray queries, so this changes no default device.
    pub(crate) fn acceleration_structure_flags(device: &wgpu::Device) -> wgpu::AccelerationStructureFlags {
        let mut flags = wgpu::AccelerationStructureFlags::PREFER_FAST_TRACE;
        if device
            .features()
            .contains(wgpu::Features::EXPERIMENTAL_RAY_HIT_VERTEX_RETURN)
        {
            flags |= wgpu::AccelerationStructureFlags::ALLOW_RAY_HIT_VERTEX_RETURN;
        }
        flags
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY: [[f32; 4]; 4] = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];

    fn limits() -> AccelLimits {
        AccelLimits {
            max_blas_primitive_count: 1000,
            max_blas_geometry_count: 16,
            max_tlas_instance_count: 8,
            max_acceleration_structures_per_shader_stage: 4,
        }
    }

    fn mesh(key: u64, generation: u64, triangles: u32) -> AccelMesh {
        AccelMesh {
            key,
            generation,
            vertex_count: triangles * 3,
            index_count: triangles * 3,
        }
    }

    fn at(mesh: u64, x: f32) -> AccelInstance {
        let mut transform = IDENTITY;
        transform[3][0] = x;
        AccelInstance {
            mesh,
            transform,
            custom_data: 7,
            mask: 0xFF,
        }
    }

    #[test]
    fn instances_share_one_blas_per_mesh_in_first_reference_order() {
        let meshes = [mesh(10, 1, 12), mesh(20, 1, 2), mesh(30, 1, 5)];
        let instances = [at(20, 0.0), at(10, 1.0), at(20, 2.0), at(20, 3.0)];
        let plan = AccelPlanner::new()
            .plan(&meshes, &instances, &limits())
            .expect("plan");
        let keys: Vec<u64> = plan.blas.iter().map(|b| b.key).collect();
        assert_eq!(keys, [20, 10], "mesh 30 is not instanced, so it gets no BLAS");
        let slots: Vec<usize> = plan.instances.iter().map(|i| i.blas).collect();
        assert_eq!(slots, [0, 1, 0, 0]);
        assert_eq!(plan.builds(), 2);
        assert_eq!(plan.build_triangles(), 14);
        assert!(plan.evict.is_empty());
        // The translation lands in the last column of the row-major 3×4.
        assert_eq!(plan.instances[2].transform[3], 2.0);
    }

    #[test]
    fn only_new_or_changed_meshes_rebuild_and_dropped_meshes_evict() {
        let mut planner = AccelPlanner::new();
        let first = planner
            .plan(
                &[mesh(1, 1, 4), mesh(2, 1, 4)],
                &[at(1, 0.0), at(2, 0.0)],
                &limits(),
            )
            .expect("plan");
        assert_eq!(first.builds(), 2);
        planner.commit(&first);
        assert_eq!(planner.built_count(), 2);

        // Same meshes, moved instances: nothing rebuilds (the TLAS always does).
        let moved = planner
            .plan(
                &[mesh(1, 1, 4), mesh(2, 1, 4)],
                &[at(1, 5.0), at(2, 6.0)],
                &limits(),
            )
            .expect("plan");
        assert_eq!(moved.builds(), 0);

        // Mesh 2's content changes; mesh 1 leaves the list; mesh 3 is new.
        let next = planner
            .plan(
                &[mesh(2, 2, 4), mesh(3, 1, 1)],
                &[at(2, 0.0), at(3, 0.0)],
                &limits(),
            )
            .expect("plan");
        let built: Vec<(u64, bool)> = next.blas.iter().map(|b| (b.key, b.build)).collect();
        assert_eq!(built, [(2, true), (3, true)]);
        assert_eq!(next.evict, [1]);
        planner.commit(&next);
        assert_eq!(planner.built_count(), 2);

        // A listed but un-instanced mesh keeps its BLAS.
        let idle = planner
            .plan(&[mesh(2, 2, 4), mesh(3, 1, 1)], &[], &limits())
            .expect("plan");
        assert!(idle.evict.is_empty() && idle.blas.is_empty());
        planner.clear();
        assert_eq!(planner.built_count(), 0);
    }

    #[test]
    fn every_refusal_is_reported_before_anything_is_built() {
        let p = AccelPlanner::new();
        let l = limits();
        let ok = [mesh(1, 1, 4)];
        assert_eq!(
            p.plan(&ok, &[at(9, 0.0)], &l),
            Err(AccelPlanError::MissingMesh { instance: 0, mesh: 9 })
        );
        assert_eq!(
            p.plan(&[mesh(1, 1, 4), mesh(1, 2, 4)], &[], &l),
            Err(AccelPlanError::DuplicateMesh { mesh: 1 })
        );
        let ragged = AccelMesh {
            index_count: 7,
            ..mesh(1, 1, 4)
        };
        assert_eq!(
            p.plan(&[ragged], &[at(1, 0.0)], &l),
            Err(AccelPlanError::NotTriangles { mesh: 1, elements: 7 })
        );
        let empty = AccelMesh {
            vertex_count: 0,
            index_count: 0,
            ..mesh(1, 1, 0)
        };
        assert!(matches!(
            p.plan(&[empty], &[at(1, 0.0)], &l),
            Err(AccelPlanError::NotTriangles { elements: 0, .. })
        ));
        assert_eq!(
            p.plan(&[mesh(1, 1, 1001)], &[at(1, 0.0)], &l),
            Err(AccelPlanError::TooManyPrimitives {
                mesh: 1,
                triangles: 1001,
                max: 1000
            })
        );
        let nine: Vec<AccelInstance> = (0..9).map(|i| at(1, i as f32)).collect();
        assert_eq!(
            p.plan(&ok, &nine, &l),
            Err(AccelPlanError::TooManyInstances { count: 9, max: 8 })
        );
        let wide = AccelInstance {
            custom_data: 1 << 24,
            ..at(1, 0.0)
        };
        assert_eq!(
            p.plan(&ok, &[wide], &l),
            Err(AccelPlanError::CustomDataTooWide {
                instance: 0,
                value: 1 << 24
            })
        );
        let nan = at(1, f32::NAN);
        assert_eq!(
            p.plan(&ok, &[nan], &l),
            Err(AccelPlanError::NonFiniteTransform { instance: 0 })
        );
    }

    #[test]
    fn non_indexed_meshes_count_vertices_as_elements() {
        let m = AccelMesh {
            key: 1,
            generation: 1,
            vertex_count: 9,
            index_count: 0,
        };
        assert_eq!(m.triangle_count(), 3);
        let plan = AccelPlanner::new()
            .plan(&[m], &[at(1, 0.0)], &limits())
            .expect("plan");
        assert_eq!(plan.build_triangles(), 3);
    }

    #[test]
    fn the_tlas_transform_is_the_row_major_affine_part() {
        // Columns: x axis, y axis, z axis, translation.
        let m = [
            [1.0, 2.0, 3.0, 0.0],
            [4.0, 5.0, 6.0, 0.0],
            [7.0, 8.0, 9.0, 0.0],
            [10.0, 11.0, 12.0, 1.0],
        ];
        assert_eq!(
            tlas_transform(&m),
            [1.0, 4.0, 7.0, 10.0, 2.0, 5.0, 8.0, 11.0, 3.0, 6.0, 9.0, 12.0]
        );
    }

    #[test]
    fn error_codes_are_lowercase_protocol_codes() {
        let all = [
            AccelError::RayQueryDisabled,
            AccelError::MeshBuffers { mesh: 1 },
            AccelError::RefitShape,
            AccelError::Plan(AccelPlanError::MissingMesh { instance: 0, mesh: 1 }),
            AccelError::Plan(AccelPlanError::DuplicateMesh { mesh: 1 }),
            AccelError::Plan(AccelPlanError::NotTriangles { mesh: 1, elements: 1 }),
            AccelError::Plan(AccelPlanError::TooManyPrimitives {
                mesh: 1,
                triangles: 2,
                max: 1,
            }),
            AccelError::Plan(AccelPlanError::TooManyInstances { count: 2, max: 1 }),
            AccelError::Plan(AccelPlanError::CustomDataTooWide {
                instance: 0,
                value: 0,
            }),
            AccelError::Plan(AccelPlanError::NonFiniteTransform { instance: 0 }),
        ];
        let mut codes: Vec<&str> = all.iter().map(AccelError::code).collect();
        for (e, code) in all.iter().zip(&codes) {
            assert!(
                code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{code}"
            );
            assert!(!e.to_string().is_empty());
        }
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), all.len());
    }
}
