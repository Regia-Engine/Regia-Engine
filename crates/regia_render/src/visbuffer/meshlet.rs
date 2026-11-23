// SPDX-License-Identifier: MPL-2.0
//! Cluster (meshlet) hierarchy: the cooked geometry the visibility-buffer path draws.
//!
//! A [`MeshletMesh`] indexes the mesh's own vertex buffer (no vertex duplication): every
//! cluster lists up to [`MAX_MESHLET_VERTICES`] source vertices and up to
//! [`MAX_MESHLET_TRIANGLES`] triangles over that list. Coarser clusters come from
//! simplifying groups of finer ones with the group border locked, so the clusters form a
//! DAG (Cignoni et al., "Batched Multi Triangulation", IEEE Visualization 2005). Each cluster
//! stores the bounding sphere and error of the simplification that produced it (`lod`) and of
//! the one that consumed it (`parent`); a view draws a cluster when its own projected error is
//! within the threshold and its parent's is not ([`MeshletMesh::lod_cut`]). Siblings share both
//! spheres, so the whole group switches together and the cut has no cracks.
//!
//! The cooker lives in `regia_asset_pipeline::meshlet` (meshoptimizer); this crate only
//! reads, validates and draws the result. [`MeshletMesh::single_level`] is an unsimplified
//! fallback for tools and tests that have no cooker.

use std::sync::Arc;

use thiserror::Error;

use super::residency::BandDisk;

/// Cooked format version ([`MeshletMesh::to_bytes`]). Loaders reject any other value.
pub const MESHLET_FORMAT_VERSION: u32 = 1;
/// Most source vertices one cluster references (local indices are one byte).
pub const MAX_MESHLET_VERTICES: usize = 64;
/// Most triangles in one cluster. The visibility ID stores the triangle in seven bits.
pub const MAX_MESHLET_TRIANGLES: usize = 124;
/// `parent_error` of a root cluster: no coarser version exists.
pub const ROOT_ERROR: f32 = f32::MAX;
/// Type path of cooked meshlet sub-assets: `regia cook` writes one per glTF mesh primitive
/// whose `.meta` settings say `meshlets: true` (the `.fmlt` payload is [`MeshletMesh::to_bytes`]).
pub const MESHLET_TYPE_PATH: &str = "regia_render::MeshletMesh";

/// LOD 0 of a cooked mesh as the runtime draws it: the decoded vertex streams and LOD 0's index
/// range. Cooked meshlets are built over exactly these buffers (the cook calls this too), so
/// [`MeshletMesh::source_vertex_count`] and [`MeshletMesh::source_index_count`] match.
///
/// # Errors
///
/// The cooked mesh does not decode.
pub fn cooked_mesh_lod0(mesh: &regia_asset::CookedMesh) -> Result<regia_asset::CpuMesh, String> {
    let mut cpu = mesh.to_cpu_mesh().map_err(|e| e.to_string())?;
    if let Some(lod0) = mesh.lods.first() {
        let width = match mesh.index_width {
            regia_asset::CookedIndexWidth::U16 => 2,
            regia_asset::CookedIndexWidth::U32 => 4,
        };
        let start = (lod0.index_byte_offset / width) as usize;
        let end = start + (lod0.index_byte_length / width) as usize;
        if end <= cpu.indices.len() {
            cpu.indices.truncate(end);
            cpu.indices.drain(..start);
        }
    }
    Ok(cpu)
}

/// The sub-asset path of the meshlets cooked for the mesh sub-asset `mesh_sub_path`
/// (`mesh/<node>/primitive_<i>` → `meshlets/<node>/primitive_<i>`). `None` for other paths.
#[must_use]
pub fn meshlet_sub_path(mesh_sub_path: &str) -> Option<String> {
    mesh_sub_path
        .strip_prefix("mesh/")
        .map(|rest| format!("meshlets/{rest}"))
}

const MAGIC: [u8; 4] = *b"FMLT";

/// One cluster, laid out as the GPU reads it (96 bytes, six `vec4`s).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Meshlet {
    /// Culling sphere: object-space centre (xyz) and radius (w).
    pub bounds: [f32; 4],
    /// Normal cone: axis (xyz) and cutoff (w). A cutoff of 1 or more never culls.
    pub cone: [f32; 4],
    /// Sphere of the simplification that produced this cluster (shared by its siblings).
    pub lod: [f32; 4],
    /// Sphere of the simplification that consumed this cluster (shared by its group).
    pub parent: [f32; 4],
    /// Object-space error of `lod`; 0 at full detail.
    pub lod_error: f32,
    /// Object-space error of `parent`; [`ROOT_ERROR`] when nothing consumed the cluster.
    pub parent_error: f32,
    /// First entry of this cluster in [`MeshletMesh::vertices`].
    pub vertex_offset: u32,
    /// First entry of this cluster in [`MeshletMesh::triangles`].
    pub triangle_offset: u32,
    /// Source vertices referenced (≤ [`MAX_MESHLET_VERTICES`]).
    pub vertex_count: u32,
    /// Triangles (≤ [`MAX_MESHLET_TRIANGLES`]).
    pub triangle_count: u32,
    /// Simplification level (0 = the source triangles).
    pub level: u32,
    /// Padding to 16 bytes.
    pub pad: u32,
}

/// A mesh split into clusters with a simplification DAG over them.
#[derive(Clone, Default)]
pub struct MeshletMesh {
    /// Length of the source vertex buffer the clusters index.
    pub source_vertex_count: u32,
    /// Index count of the source mesh (a different count means the mesh changed).
    pub source_index_count: u32,
    /// Object-space bounding sphere of the whole mesh.
    pub bounds: [f32; 4],
    /// Clusters, finest level first.
    pub meshlets: Vec<Meshlet>,
    /// Source vertex index of every cluster-local vertex.
    pub vertices: Vec<u32>,
    /// One entry per triangle: three cluster-local vertex indices in bytes 0, 1 and 2.
    pub triangles: Vec<u32>,
    /// Vertices the clusters index past the mesh's own (ADR-2655): a voxel far DAG
    /// (`VoxelFarMesh::bridged`) draws the mesh's vertices `0..`[`Self::base_vertex_count`] from
    /// the draw and these after them, so a placed mesh keeps its mesh key and its forward
    /// vertex buffer. Empty for cooked `.fmlt` meshlets; runtime only ([`Self::to_bytes`] does
    /// not write them).
    pub appended_vertices: AppendedVertices,
    /// On-demand band payload source (ADR-3708): `Some` when the payload came from a pak and the
    /// cluster lists and triangles stay on disk until [`Self::materialize`] or a band read.
    /// Transient: never serialized ([`Self::to_bytes`]), never compared.
    pub disk: Option<Arc<dyn BandDisk>>,
}

impl PartialEq for MeshletMesh {
    fn eq(&self, other: &Self) -> bool {
        self.source_vertex_count == other.source_vertex_count
            && self.source_index_count == other.source_index_count
            && self.bounds == other.bounds
            && self.meshlets == other.meshlets
            && self.vertices == other.vertices
            && self.triangles == other.triangles
            && self.appended_vertices == other.appended_vertices
    }
}

impl std::fmt::Debug for MeshletMesh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeshletMesh")
            .field("source_vertex_count", &self.source_vertex_count)
            .field("source_index_count", &self.source_index_count)
            .field("bounds", &self.bounds)
            .field("meshlets", &self.meshlets)
            .field("vertices", &self.vertices)
            .field("triangles", &self.triangles)
            .field("appended_vertices", &self.appended_vertices)
            .field("disk", &self.disk.is_some())
            .finish()
    }
}

/// Vertices a cluster DAG appends past its mesh's own ([`MeshletMesh::appended_vertices`]);
/// compared byte for byte.
#[derive(Clone, Debug, Default)]
pub struct AppendedVertices(pub Vec<crate::LabVertex>);

impl PartialEq for AppendedVertices {
    fn eq(&self, other: &Self) -> bool {
        bytemuck::cast_slice::<crate::LabVertex, u8>(&self.0)
            == bytemuck::cast_slice::<crate::LabVertex, u8>(&other.0)
    }
}

/// The parsed head of a cooked meshlet blob ([`MeshletMesh::parse_head`]): everything but the
/// vertex and triangle arrays, which a stream-shaped mesh reads on demand (ADR-3708).
#[derive(Clone, Debug, PartialEq)]
pub struct MeshletStreamHead {
    /// Length of the source vertex buffer the clusters index.
    pub source_vertex_count: u32,
    /// Index count of the source mesh.
    pub source_index_count: u32,
    /// Object-space bounding sphere of the whole mesh.
    pub bounds: [f32; 4],
    /// Clusters, finest level first.
    pub meshlets: Vec<Meshlet>,
    /// Byte range of the vertex-index section in the cooked blob.
    pub vertices_range: std::ops::Range<usize>,
    /// Byte range of the triangle section in the cooked blob.
    pub triangles_range: std::ops::Range<usize>,
}

/// A cooked meshlet blob or hierarchy is malformed.
#[derive(Debug, Error, PartialEq)]
pub enum MeshletFormatError {
    /// The blob does not start with `FMLT`.
    #[error("not a meshlet blob (bad magic)")]
    BadMagic,
    /// The blob was cooked by another format version.
    #[error("meshlet format version {found}, expected {MESHLET_FORMAT_VERSION}")]
    Version {
        /// Version in the blob.
        found: u32,
    },
    /// The blob ends before its declared arrays.
    #[error("meshlet blob truncated")]
    Truncated,
    /// The cooked file's wrapper (`FCKD` header) is unreadable.
    #[error("cooked meshlet file: {0}")]
    Cooked(String),
    /// The cooked file holds another asset type.
    #[error("cooked file is not a meshlet sub-asset")]
    WrongType,
    /// A cluster's ranges or counts are out of bounds.
    #[error("meshlet {index}: {reason}")]
    Invalid {
        /// Cluster index.
        index: usize,
        /// What is wrong.
        reason: &'static str,
    },
}

/// Why a cooked meshlet sub-asset did not load ([`MeshletMesh::load`]). The mesh it was cooked
/// for stays on the forward path.
#[derive(Debug, Error, PartialEq)]
pub enum MeshletLoadError {
    /// Neither a mounted pak nor the cooked cache holds the asset (not cooked yet, or the
    /// `.meta` does not ask for `meshlets: true`).
    #[error("meshlets not cooked: {0}")]
    NotCooked(String),
    /// The payload is unreadable, of another version or invalid.
    #[error(transparent)]
    Format(#[from] MeshletFormatError),
}

impl MeshletLoadError {
    /// A stable code for the agent protocol: `meshlets_not_cooked`, `meshlets_unreadable` (a bad
    /// wrapper, another asset type or a bad magic), `meshlets_old_version` (cooked by another
    /// format version: `regia cook` rebuilds it) or `meshlets_invalid` (truncated or out of
    /// range).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotCooked(_) => "meshlets_not_cooked",
            Self::Format(MeshletFormatError::Version { .. }) => "meshlets_old_version",
            Self::Format(
                MeshletFormatError::BadMagic | MeshletFormatError::Cooked(_) | MeshletFormatError::WrongType,
            ) => "meshlets_unreadable",
            Self::Format(MeshletFormatError::Truncated | MeshletFormatError::Invalid { .. }) => {
                "meshlets_invalid"
            }
        }
    }
}

/// The view a LOD cut is taken for (all in the mesh's object space for [`MeshletMesh::lod_cut`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LodView {
    /// Eye position.
    pub eye: [f32; 3],
    /// Pixels per unit of size at unit distance: `viewport height / (2 tan(fov_y / 2))`.
    pub pixels_per_unit: f32,
    /// Largest projected error a drawn cluster may have, pixels.
    pub threshold_px: f32,
    /// Distances below this count as this (the near plane).
    pub near: f32,
}

impl LodView {
    /// Projected error in pixels of a simplification with `error` over the sphere `sphere`.
    #[must_use]
    pub fn projected_error(&self, sphere: [f32; 4], error: f32) -> f32 {
        if error >= ROOT_ERROR {
            return f32::INFINITY;
        }
        let d = [
            sphere[0] - self.eye[0],
            sphere[1] - self.eye[1],
            sphere[2] - self.eye[2],
        ];
        let distance = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() - sphere[3];
        error * self.pixels_per_unit / distance.max(self.near)
    }

    /// True when `meshlet` belongs to the cut: fine enough itself, its parent too coarse.
    #[must_use]
    pub fn selects(&self, meshlet: &Meshlet) -> bool {
        self.projected_error(meshlet.lod, meshlet.lod_error) <= self.threshold_px
            && self.projected_error(meshlet.parent, meshlet.parent_error) > self.threshold_px
    }
}

fn read_u32(bytes: &[u8], at: &mut usize) -> Result<u32, MeshletFormatError> {
    let end = *at + 4;
    let slice = bytes.get(*at..end).ok_or(MeshletFormatError::Truncated)?;
    *at = end;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_array<T: bytemuck::Pod>(
    bytes: &[u8],
    at: &mut usize,
    count: usize,
) -> Result<Vec<T>, MeshletFormatError> {
    let size = count
        .checked_mul(std::mem::size_of::<T>())
        .ok_or(MeshletFormatError::Truncated)?;
    let end = at.checked_add(size).ok_or(MeshletFormatError::Truncated)?;
    let slice = bytes.get(*at..end).ok_or(MeshletFormatError::Truncated)?;
    *at = end;
    // The blob is not aligned for `T`; copy element by element.
    Ok(slice
        .chunks_exact(std::mem::size_of::<T>())
        .map(bytemuck::pod_read_unaligned)
        .collect())
}

impl MeshletMesh {
    /// Total triangles over every level.
    #[must_use]
    pub fn triangle_count(&self) -> usize {
        self.triangles.len()
    }

    /// Triangles of level 0 (the source mesh).
    #[must_use]
    pub fn base_triangle_count(&self) -> usize {
        self.meshlets
            .iter()
            .filter(|m| m.level == 0)
            .map(|m| m.triangle_count as usize)
            .sum()
    }

    /// Number of simplification levels (1 = no LOD).
    #[must_use]
    pub fn level_count(&self) -> u32 {
        self.meshlets.iter().map(|m| m.level + 1).max().unwrap_or(0)
    }

    /// Indices (into `meshlets`) of the clusters drawn for `view` (object space).
    pub fn lod_cut<'a>(&'a self, view: &'a LodView) -> impl Iterator<Item = usize> + 'a {
        self.meshlets
            .iter()
            .enumerate()
            .filter(|(_, m)| view.selects(m))
            .map(|(i, _)| i)
    }

    /// Source-vertex triangle list of one cluster.
    #[must_use]
    pub fn cluster_indices(&self, index: usize) -> Vec<[u32; 3]> {
        let Some(m) = self.meshlets.get(index) else {
            return Vec::new();
        };
        let start = m.triangle_offset as usize;
        let end = start + m.triangle_count as usize;
        self.triangles
            .get(start..end)
            .unwrap_or(&[])
            .iter()
            .map(|&packed| {
                let local = |shift: u32| (packed >> shift) & 0xff;
                let vertex = |l: u32| {
                    self.vertices
                        .get(m.vertex_offset as usize + l as usize)
                        .copied()
                        .unwrap_or(0)
                };
                [vertex(local(0)), vertex(local(8)), vertex(local(16))]
            })
            .collect()
    }

    /// Length of the vertex buffer the draw supplies: [`Self::source_vertex_count`] less the
    /// [`Self::appended_vertices`] (all of it for cooked meshlets). A mesh whose vertex count
    /// differs from this was not the one the clusters were built for.
    #[must_use]
    pub fn base_vertex_count(&self) -> usize {
        (self.source_vertex_count as usize).saturating_sub(self.appended_vertices.0.len())
    }

    /// Checks every range and count against the arrays and the source vertex count.
    ///
    /// # Errors
    ///
    /// [`MeshletFormatError::Invalid`] naming the first bad cluster.
    pub fn validate(&self) -> Result<(), MeshletFormatError> {
        if self.appended_vertices.0.len() > self.source_vertex_count as usize {
            return Err(MeshletFormatError::Invalid {
                index: 0,
                reason: "more appended vertices than source vertices",
            });
        }
        for (index, m) in self.meshlets.iter().enumerate() {
            let invalid = |reason| Err(MeshletFormatError::Invalid { index, reason });
            if m.vertex_count as usize > MAX_MESHLET_VERTICES {
                return invalid("too many vertices");
            }
            if m.triangle_count as usize > MAX_MESHLET_TRIANGLES || m.triangle_count == 0 {
                return invalid("triangle count out of range");
            }
            let v_end = m.vertex_offset as usize + m.vertex_count as usize;
            let t_end = m.triangle_offset as usize + m.triangle_count as usize;
            if v_end > self.vertices.len() || t_end > self.triangles.len() {
                return invalid("range past the arrays");
            }
            if self.vertices[m.vertex_offset as usize..v_end]
                .iter()
                .any(|&v| v >= self.source_vertex_count)
            {
                return invalid("vertex past the source buffer");
            }
            for &packed in &self.triangles[m.triangle_offset as usize..t_end] {
                for shift in [0, 8, 16] {
                    if (packed >> shift) & 0xff >= m.vertex_count {
                        return invalid("triangle index past the cluster's vertices");
                    }
                }
            }
            if m.parent_error < m.lod_error {
                return invalid("parent error below own error");
            }
        }
        Ok(())
    }

    /// Serializes to the cooked blob: `FMLT`, version, counts, bounds, then the arrays
    /// (little-endian, as every Regia Engine target).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            40 + self.meshlets.len() * std::mem::size_of::<Meshlet>()
                + (self.vertices.len() + self.triangles.len()) * 4,
        );
        out.extend_from_slice(&MAGIC);
        for value in [
            MESHLET_FORMAT_VERSION,
            self.source_vertex_count,
            self.source_index_count,
            self.meshlets.len() as u32,
            self.vertices.len() as u32,
            self.triangles.len() as u32,
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(bytemuck::cast_slice(&self.bounds));
        out.extend_from_slice(bytemuck::cast_slice(&self.meshlets));
        out.extend_from_slice(bytemuck::cast_slice(&self.vertices));
        out.extend_from_slice(bytemuck::cast_slice(&self.triangles));
        out
    }

    /// Parses and validates a cooked blob.
    ///
    /// # Errors
    ///
    /// Bad magic, another version, truncation, or an invalid hierarchy.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MeshletFormatError> {
        if bytes.get(0..4) != Some(&MAGIC[..]) {
            return Err(MeshletFormatError::BadMagic);
        }
        let mut at = 4;
        let version = read_u32(bytes, &mut at)?;
        if version != MESHLET_FORMAT_VERSION {
            return Err(MeshletFormatError::Version { found: version });
        }
        let source_vertex_count = read_u32(bytes, &mut at)?;
        let source_index_count = read_u32(bytes, &mut at)?;
        let meshlet_count = read_u32(bytes, &mut at)? as usize;
        let vertex_count = read_u32(bytes, &mut at)? as usize;
        let triangle_count = read_u32(bytes, &mut at)? as usize;
        let bounds: Vec<f32> = read_array(bytes, &mut at, 4)?;
        let mesh = Self {
            source_vertex_count,
            source_index_count,
            bounds: [bounds[0], bounds[1], bounds[2], bounds[3]],
            meshlets: read_array(bytes, &mut at, meshlet_count)?,
            vertices: read_array(bytes, &mut at, vertex_count)?,
            triangles: read_array(bytes, &mut at, triangle_count)?,
            appended_vertices: AppendedVertices::default(),
            disk: None,
        };
        mesh.validate()?;
        Ok(mesh)
    }

    /// Parses the head of a cooked blob (everything but the vertex and triangle arrays) and
    /// checks it structurally: the magic and version, every cluster's ranges against the
    /// declared section sizes, and the per-record error order. The array contents are validated
    /// at [`Self::materialize`] or per band on delivery (ADR-3708).
    ///
    /// # Errors
    ///
    /// Bad magic, another version, truncation, or a structurally invalid hierarchy.
    pub fn parse_head(bytes: &[u8]) -> Result<MeshletStreamHead, MeshletFormatError> {
        if bytes.get(0..4) != Some(&MAGIC[..]) {
            return Err(MeshletFormatError::BadMagic);
        }
        let mut at = 4;
        let version = read_u32(bytes, &mut at)?;
        if version != MESHLET_FORMAT_VERSION {
            return Err(MeshletFormatError::Version { found: version });
        }
        let source_vertex_count = read_u32(bytes, &mut at)?;
        let source_index_count = read_u32(bytes, &mut at)?;
        let meshlet_count = read_u32(bytes, &mut at)? as usize;
        let vertex_count = read_u32(bytes, &mut at)? as usize;
        let triangle_count = read_u32(bytes, &mut at)? as usize;
        let bounds: Vec<f32> = read_array(bytes, &mut at, 4)?;
        let meshlets: Vec<Meshlet> = read_array(bytes, &mut at, meshlet_count)?;
        let vertex_bytes = vertex_count.checked_mul(4).ok_or(MeshletFormatError::Truncated)?;
        let vertices_end = at
            .checked_add(vertex_bytes)
            .ok_or(MeshletFormatError::Truncated)?;
        if vertices_end > bytes.len() {
            return Err(MeshletFormatError::Truncated);
        }
        let vertices_range = at..vertices_end;
        at = vertices_end;
        let triangle_bytes = triangle_count
            .checked_mul(4)
            .ok_or(MeshletFormatError::Truncated)?;
        let triangles_end = at
            .checked_add(triangle_bytes)
            .ok_or(MeshletFormatError::Truncated)?;
        if triangles_end > bytes.len() {
            return Err(MeshletFormatError::Truncated);
        }
        let triangles_range = at..triangles_end;
        for (index, m) in meshlets.iter().enumerate() {
            let invalid = |reason| Err(MeshletFormatError::Invalid { index, reason });
            if m.vertex_count as usize > MAX_MESHLET_VERTICES {
                return invalid("too many vertices");
            }
            if m.triangle_count as usize > MAX_MESHLET_TRIANGLES || m.triangle_count == 0 {
                return invalid("triangle count out of range");
            }
            if m.vertex_offset as usize + m.vertex_count as usize > vertex_count {
                return invalid("vertex range past the section");
            }
            if m.triangle_offset as usize + m.triangle_count as usize > triangle_count {
                return invalid("triangle range past the section");
            }
            if m.parent_error < m.lod_error {
                return invalid("parent error below own error");
            }
        }
        Ok(MeshletStreamHead {
            source_vertex_count,
            source_index_count,
            bounds: [bounds[0], bounds[1], bounds[2], bounds[3]],
            meshlets,
            vertices_range,
            triangles_range,
        })
    }

    /// A stream-shaped mesh from a parsed head: cluster records in RAM, the vertex and triangle
    /// sections read on demand through `disk` (ADR-3708).
    #[must_use]
    pub fn from_head(head: MeshletStreamHead, disk: Arc<dyn BandDisk>) -> Self {
        Self {
            source_vertex_count: head.source_vertex_count,
            source_index_count: head.source_index_count,
            bounds: head.bounds,
            meshlets: head.meshlets,
            vertices: Vec::new(),
            triangles: Vec::new(),
            appended_vertices: AppendedVertices::default(),
            disk: Some(disk),
        }
    }

    /// Whether `vertices`/`triangles` hold the cooked sections (a full or materialized mesh;
    /// stream-shaped meshes carry them on the disk instead).
    #[must_use]
    pub fn is_materialized(&self) -> bool {
        self.disk.is_none() || !self.vertices.is_empty() || !self.triangles.is_empty()
    }

    /// Fills `vertices`/`triangles` from the disk (stream-shaped meshes only). Idempotent.
    ///
    /// # Errors
    ///
    /// The disk read failed (`BandDiskError` mapped to [`MeshletLoadError::NotCooked`]).
    pub fn materialize(&mut self) -> Result<(), MeshletLoadError> {
        let Some(disk) = self.disk.clone() else {
            return Ok(());
        };
        let sections = disk
            .read_sections()
            .map_err(|e| MeshletLoadError::NotCooked(e.message))?;
        if self.meshlets.windows(2).any(|w| w[1].level < w[0].level) {
            return Err(MeshletLoadError::NotCooked(
                "clusters are not stored level by level".to_string(),
            ));
        }
        // Validate before the sections stick. A failure used to leave the bad arrays in place
        // with the disk still set, and `meshlet_bridge` then inserts that mesh (`insert_meshlets`).
        let vertices = sections.vertices;
        let triangles = sections.triangles;
        let saved_vertices = std::mem::replace(&mut self.vertices, vertices);
        let saved_triangles = std::mem::replace(&mut self.triangles, triangles);
        if let Err(err) = self.validate() {
            self.vertices = saved_vertices;
            self.triangles = saved_triangles;
            return Err(MeshletLoadError::Format(err));
        }
        self.disk = None;
        Ok(())
    }

    /// Reads a cooked meshlet sub-asset as `regia cook` writes it (cache file or pak entry):
    /// the cooked header, a [`MESHLET_TYPE_PATH`] type tag, then the `.fmlt` blob. Links no
    /// cooker.
    ///
    /// # Errors
    ///
    /// A bad wrapper, another asset type, or any [`Self::from_bytes`] error (an older format
    /// version is rejected; `regia cook` rebuilds it from the source).
    pub fn from_cooked_file(bytes: &[u8]) -> Result<Self, MeshletFormatError> {
        let (header, payload) =
            regia_asset::decode_cooked_file(bytes).map_err(|e| MeshletFormatError::Cooked(e.to_string()))?;
        if header.type_tag != regia_core::id::TypePathHash::of(MESHLET_TYPE_PATH) {
            return Err(MeshletFormatError::WrongType);
        }
        Self::from_bytes(payload)
    }

    /// Loads the cooked meshlet sub-asset `id` (a [`MESHLET_TYPE_PATH`] `.fmlt`, see
    /// [`meshlet_sub_path`]) through `server`: the latest mounted pak that holds it, else the
    /// cooked cache. The id is marked as decoded outside the asset server
    /// ([`regia_asset::AssetServer::track_external`]), so a hot reload of it bumps
    /// [`regia_asset::AssetServer::generation`] for the caller to load it again.
    ///
    /// # Errors
    ///
    /// [`MeshletLoadError::NotCooked`] when no pak or cache file holds it, else the
    /// [`Self::from_cooked_file`] error.
    pub fn load(
        server: &mut regia_asset::AssetServer,
        id: regia_asset::AssetId,
    ) -> Result<Self, MeshletLoadError> {
        let _span = tracing::info_span!("asset_load", kind = "meshlets").entered();
        server.track_external(id);
        let (bytes, _) = server
            .read_cooked(id)
            .map_err(|e| MeshletLoadError::NotCooked(e.to_string()))?;
        Ok(Self::from_cooked_file(&bytes)?)
    }

    /// Clusters `indices` in order, [`MAX_MESHLET_TRIANGLES`] at a time (fewer when the
    /// vertex budget fills first), with no simplification: one level, every cluster a root.
    /// For tools and tests without the cooker; the cooker's clusters are tighter.
    #[must_use]
    pub fn single_level(positions: &[[f32; 3]], indices: &[u32]) -> Self {
        let mut mesh = Self {
            source_vertex_count: positions.len() as u32,
            source_index_count: indices.len() as u32,
            bounds: sphere_of_points(positions.iter().copied()),
            ..Self::default()
        };
        let mut local: Vec<u32> = Vec::with_capacity(MAX_MESHLET_VERTICES);
        let mut tris: Vec<u32> = Vec::with_capacity(MAX_MESHLET_TRIANGLES);
        let flush = |mesh: &mut Self, local: &mut Vec<u32>, tris: &mut Vec<u32>| {
            if tris.is_empty() {
                return;
            }
            let sphere = sphere_of_points(local.iter().map(|&v| positions[v as usize]));
            mesh.meshlets.push(Meshlet {
                bounds: sphere,
                cone: [0.0, 0.0, 1.0, 1.0],
                lod: sphere,
                parent: sphere,
                lod_error: 0.0,
                parent_error: ROOT_ERROR,
                vertex_offset: mesh.vertices.len() as u32,
                triangle_offset: mesh.triangles.len() as u32,
                vertex_count: local.len() as u32,
                triangle_count: tris.len() as u32,
                level: 0,
                pad: 0,
            });
            mesh.vertices.append(local);
            mesh.triangles.append(tris);
        };
        for tri in indices.as_chunks::<3>().0 {
            if tri.iter().any(|&v| v as usize >= positions.len()) {
                continue;
            }
            let new = tri.iter().filter(|v| !local.contains(v)).count();
            if local.len() + new > MAX_MESHLET_VERTICES || tris.len() == MAX_MESHLET_TRIANGLES {
                flush(&mut mesh, &mut local, &mut tris);
            }
            let mut packed = 0u32;
            for (corner, &v) in tri.iter().enumerate() {
                let slot = match local.iter().position(|&l| l == v) {
                    Some(slot) => slot,
                    None => {
                        local.push(v);
                        local.len() - 1
                    }
                };
                packed |= (slot as u32) << (corner * 8);
            }
            tris.push(packed);
        }
        flush(&mut mesh, &mut local, &mut tris);
        mesh
    }
}

/// Bounding sphere of a point set: the AABB centre and the farthest point from it.
#[must_use]
pub fn sphere_of_points(points: impl Iterator<Item = [f32; 3]> + Clone) -> [f32; 4] {
    let mut lo = [f32::MAX; 3];
    let mut hi = [f32::MIN; 3];
    let mut any = false;
    for p in points.clone() {
        any = true;
        for k in 0..3 {
            lo[k] = lo[k].min(p[k]);
            hi[k] = hi[k].max(p[k]);
        }
    }
    if !any {
        return [0.0; 4];
    }
    let c = [
        (lo[0] + hi[0]) * 0.5,
        (lo[1] + hi[1]) * 0.5,
        (lo[2] + hi[2]) * 0.5,
    ];
    let r2 = points
        .map(|p| (p[0] - c[0]).powi(2) + (p[1] - c[1]).powi(2) + (p[2] - c[2]).powi(2))
        .fold(0.0f32, f32::max);
    [c[0], c[1], c[2], r2.sqrt()]
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "vis_cull.wgsl,vis_raster.wgsl,vis_raster_masked.wgsl,vis_shadow.wgsl,vis_shadow_raster.wgsl,vis_software.wgsl,vis_velocity.wgsl",
        "Meshlet",
        Meshlet {
            bounds,
            cone,
            lod,
            parent,
            lod_error,
            parent_error,
            vertex_offset,
            triangle_offset,
            vertex_count,
            triangle_count,
            level,
            pad
        }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(n: u32) -> (Vec<[f32; 3]>, Vec<u32>) {
        let mut p = Vec::new();
        for z in 0..=n {
            for x in 0..=n {
                p.push([x as f32, 0.0, z as f32]);
            }
        }
        let mut i = Vec::new();
        for z in 0..n {
            for x in 0..n {
                let a = z * (n + 1) + x;
                i.extend_from_slice(&[a, a + n + 1, a + 1, a + 1, a + n + 1, a + n + 2]);
            }
        }
        (p, i)
    }

    #[test]
    fn meshlet_is_six_vec4() {
        assert_eq!(std::mem::size_of::<Meshlet>(), 96);
    }

    #[test]
    fn single_level_covers_every_triangle_within_limits() {
        let (p, i) = grid(20);
        let mesh = MeshletMesh::single_level(&p, &i);
        mesh.validate().expect("valid");
        assert_eq!(mesh.base_triangle_count(), i.len() / 3);
        assert_eq!(mesh.level_count(), 1);
        let mut back: Vec<[u32; 3]> = (0..mesh.meshlets.len())
            .flat_map(|c| mesh.cluster_indices(c))
            .collect();
        let mut want: Vec<[u32; 3]> = i.as_chunks::<3>().0.to_vec();
        back.sort_unstable();
        want.sort_unstable();
        assert_eq!(back, want);
    }

    #[test]
    fn blob_round_trips_and_rejects_bad_input() {
        let (p, i) = grid(8);
        let mesh = MeshletMesh::single_level(&p, &i);
        let bytes = mesh.to_bytes();
        assert_eq!(MeshletMesh::from_bytes(&bytes), Ok(mesh));
        assert_eq!(
            MeshletMesh::from_bytes(b"XXXX"),
            Err(MeshletFormatError::BadMagic)
        );
        let mut other = bytes.clone();
        other[4] = 99;
        assert_eq!(
            MeshletMesh::from_bytes(&other),
            Err(MeshletFormatError::Version { found: 99 })
        );
        assert_eq!(
            MeshletMesh::from_bytes(&bytes[..bytes.len() - 3]),
            Err(MeshletFormatError::Truncated)
        );
        // The head parser must reject the same short blob. It does not copy the arrays, so a
        // range that runs past the buffer is still truncation: a later band read would slice
        // nothing and draw an empty cluster (`words` returns an empty vec).
        let head = MeshletMesh::parse_head(&bytes).expect("a whole blob has a head");
        assert_eq!(head.triangles_range.end, bytes.len());
        assert_eq!(
            MeshletMesh::parse_head(&bytes[..bytes.len() - 3]),
            Err(MeshletFormatError::Truncated)
        );
    }

    /// A disk that returns sections `validate` rejects (a vertex index past the source).
    struct CorruptDisk;

    impl super::super::residency::BandDisk for CorruptDisk {
        fn read_sections(
            &self,
        ) -> Result<super::super::residency::BandSections, super::super::residency::BandDiskError> {
            Ok(super::super::residency::BandSections {
                vertices: vec![u32::MAX; 64],
                triangles: vec![0; 64],
            })
        }

        fn request_band(&self, _tag: u64, _clusters: &[u32]) {}

        fn try_deliver(&self) -> Option<super::super::residency::BandDelivery> {
            None
        }
    }

    /// `materialize` validates before the sections stick. A failure used to leave the bad
    /// arrays in place with the disk still set, and `meshlet_bridge` then inserts that mesh
    /// as the plain meshlets (`insert_meshlets`).
    #[test]
    fn a_failed_materialize_drops_the_sections_and_stays_stream_shaped() {
        let (p, i) = grid(2);
        let mesh = MeshletMesh::single_level(&p, &i);
        let head = MeshletMesh::parse_head(&mesh.to_bytes()).expect("head");
        let mut streamed = MeshletMesh::from_head(head, Arc::new(CorruptDisk));
        assert!(streamed.vertices.is_empty());
        let err = streamed.materialize().expect_err("corrupt sections");
        assert_eq!(err.code(), "meshlets_invalid");
        assert!(
            streamed.vertices.is_empty() && streamed.triangles.is_empty(),
            "failed materialize published {} vertices and {} triangles",
            streamed.vertices.len(),
            streamed.triangles.len()
        );
        assert!(
            streamed.disk.is_some(),
            "the disk must stay so a later read can retry"
        );
        assert!(!streamed.is_materialized());
    }

    #[test]
    fn load_errors_have_stable_codes() {
        let codes = [
            MeshletLoadError::NotCooked("x".into()).code(),
            MeshletLoadError::Format(MeshletFormatError::Version { found: 0 }).code(),
            MeshletLoadError::Format(MeshletFormatError::BadMagic).code(),
            MeshletLoadError::Format(MeshletFormatError::WrongType).code(),
            MeshletLoadError::Format(MeshletFormatError::Truncated).code(),
        ];
        assert_eq!(
            codes,
            [
                "meshlets_not_cooked",
                "meshlets_old_version",
                "meshlets_unreadable",
                "meshlets_unreadable",
                "meshlets_invalid"
            ]
        );
        for code in codes {
            assert!(code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        }
    }

    #[test]
    fn root_clusters_are_always_in_the_cut_at_full_detail() {
        let (p, i) = grid(4);
        let mesh = MeshletMesh::single_level(&p, &i);
        let view = LodView {
            eye: [0.0, 1000.0, 0.0],
            pixels_per_unit: 1000.0,
            threshold_px: 1.0,
            near: 0.1,
        };
        assert_eq!(mesh.lod_cut(&view).count(), mesh.meshlets.len());
    }
}
