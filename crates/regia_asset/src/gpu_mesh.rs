// SPDX-License-Identifier: MPL-2.0
//! GPU-ready cooked meshes (desktop paks; `docs/formats/cooked-pack-guide.md`).
//!
//! The compact [`CookedMesh`](crate::CookedMesh) (§ 7.2: `f32×3` positions, octahedral
//! `snorm16` normals with the tangent sign only, `f16×2` UVs) needs a decode and a MikkTSpace
//! tangent pass at load. Desktop paks store each mesh in this form instead: the vertices already
//! in the renderer's 96-byte vertex layout (tangents included) and `u32` indices, so a load is a
//! decompress (or a view of the mapped pak) and one copy into the mesh. Android paks keep the
//! compact form (4× smaller vertices; the bandwidth matters more than the decode there).
//!
//! # Payload layout, version 3 (little-endian)
//!
//! | Offset | Field |
//! |---|---|
//! | 0 | magic `b"FGPM"` |
//! | 4 | `version: u32` = [`GPU_MESH_VERSION`] (version 1 payloads, which have no submesh fields, and version 2 payloads, which have no trailing block, still read) |
//! | 8 | `vertex_stride: u32` = [`GPU_VERTEX_STRIDE`] |
//! | 12 | `vertex_count: u32` |
//! | 16 | `index_count: u32` (every LOD) |
//! | 20 | `lod_count: u32` |
//! | 24 | `vertex_offset: u32`: start of the vertices in the payload |
//! | 28 | `aabb_min: f32×3`, `aabb_max: f32×3`, `bounding_sphere: f32×4` |
//! | 68 | `submesh_count: u32` (version 2 on; a version 1 payload has no such field and starts the LOD table here) |
//! | 72 | `lod_count` × (`screen_relative_size: f32`, `first_index: u32`, `index_count: u32`) |
//! | after the LODs | `submesh_count` × (`first_index: u32`, `index_count: u32`, `material_slot: u32`) (version 2 on) |
//! | `vertex_offset` | `vertex_count` × [`GPU_VERTEX_STRIDE`] bytes ([`gpu_vertex`] offsets) |
//! | after the vertices | `index_count` × `u32` |
//! | after the indices | optional (version 3): magic `b"FGPX"`, `extras_len: u32`, then `extras_len` bytes of postcard [`GpuMeshExtras`] (the skin and the morph targets) |
//!
//! Up to the indices, a version 3 payload is a version 2 payload with another version number: a
//! rigid mesh without morph targets has no trailing block. A version 1 or 2 payload must end
//! after the indices; a version 3 payload ends there or after exactly one trailing block
//! (ADR-10080).
//!
//! The writer ([`encode_gpu_mesh_file`]) pads `vertex_offset` so the vertices start 16-byte
//! aligned within the cooked file (and so within the 4096-aligned pak entry).
//!
//! Submesh ranges index the same `u32` index buffer as the LODs and keep the compact mesh's
//! material slots, so a consumer that splits a mesh by material (destruction cut faces) gets the
//! same ranges from a pak as from the loose cooked file (ADR-2788).
//!
//! The vertex [`gpu_vertex::COLOR`] slot carries the compact mesh's `COLOR_0` (white when it has
//! none), so a vertex-colored mesh is stored GPU-ready too and [`GpuMeshView::to_cpu_mesh`] gives
//! the colors back (ADR-7919).
//!
//! A skinned or morphing mesh keeps its skin (joint names, both influence sets, the
//! eight-influence opt-in) and its sparse morph targets in the trailing block, in the compact
//! mesh's packed forms ([`CookedSkin`], [`CookedMorphTarget`]), so [`GpuMeshView::to_cpu_mesh`]
//! returns them exactly as `CookedMesh::to_cpu_mesh` does (ADR-10080). The [`gpu_vertex::JOINTS`]
//! and [`gpu_vertex::WEIGHTS`] slots stay zero: the cooked draw path draws such a mesh rigid in
//! its bind pose, as it draws the compact form, and a skinned draw builds its influences from the
//! skin.
//!
//! A payload of an unknown version is an error ([`GpuMeshError::UnsupportedVersion`]); the pak
//! writer converts compact meshes with the renderer's own migrator, and a runtime that meets an
//! old compact pak decodes it the slow way, so either form loads on every runtime that knows v1.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::cooked::CpuMesh;
use crate::cooked_header::{COOKED_FORMAT_VERSION, CookedHeader, CookedIoError, encode_cooked_file};
use crate::id::AssetId;
use crate::mesh_skin::{CookedMorphTarget, CookedSkin};

/// Type path (and so the cooked header's type tag) of a GPU-ready mesh payload.
pub const GPU_MESH_TYPE_PATH: &str = "regia_asset::GpuMesh";

/// Magic bytes at the start of a GPU-ready mesh payload.
pub const GPU_MESH_MAGIC: [u8; 4] = *b"FGPM";

/// Current GPU-ready mesh layout version (3: the optional skin and morph block after the
/// indices, ADR-10080).
pub const GPU_MESH_VERSION: u32 = 3;

/// The oldest layout version the reader still accepts (no submesh table).
pub const GPU_MESH_MIN_VERSION: u32 = 1;

/// Bytes per vertex in layout versions 1 to 3 (the renderer's `LabVertex`).
pub const GPU_VERTEX_STRIDE: u32 = 96;

/// Fixed header bytes before the LOD table, layout version 1.
const FIXED_HEADER_V1: usize = 68;
/// Fixed header bytes before the LOD table, layout version 2 (adds `submesh_count`).
const FIXED_HEADER: usize = 72;
/// Bytes per LOD table row.
const LOD_ROW: usize = 12;
/// Alignment of the vertex data within the cooked file.
const VERTEX_ALIGN: usize = 16;
/// Magic of the optional trailing block of a version 3 payload.
pub const GPU_MESH_EXTRAS_MAGIC: [u8; 4] = *b"FGPX";
/// Bytes of the trailing block's own header (magic and length).
const EXTRAS_HEADER: usize = 8;
/// The first layout version with the trailing block.
const EXTRAS_VERSION: u32 = 3;

/// Attribute byte offsets within one vertex of layout version 1.
pub mod gpu_vertex {
    /// Object-space position, `f32×3`.
    pub const POSITION: usize = 0;
    /// Object-space normal, `f32×3`.
    pub const NORMAL: usize = 12;
    /// Texture coordinates, `f32×2`.
    pub const UV: usize = 24;
    /// MikkTSpace tangent xyz and bitangent sign in w, `f32×4`.
    pub const TANGENT: usize = 32;
    /// Linear vertex colour, `f32×4`: the mesh's `COLOR_0`, or white on every vertex when it has
    /// none (ADR-7919; the renderer's cooked draw path still draws white, `specs/13` § 8).
    pub const COLOR: usize = 48;
    /// Joint indices, `u32×4`.
    pub const JOINTS: usize = 64;
    /// Joint weights, `f32×4`.
    pub const WEIGHTS: usize = 80;
}

/// One LOD of a GPU-ready mesh: a range of its index buffer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GpuMeshLod {
    /// Relative screen size above which this LOD is chosen (larger = more detail).
    pub screen_relative_size: f32,
    /// First index of the LOD.
    pub first_index: u32,
    /// Number of indices of the LOD.
    pub index_count: u32,
}

/// One submesh of a GPU-ready mesh: a range of its index buffer and the material slot it draws with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuMeshSubmesh {
    /// First index of the range.
    pub first_index: u32,
    /// Number of indices of the range (whole triangles).
    pub index_count: u32,
    /// Material slot (0 is the surface; a destruction cut face uses another slot).
    pub material_slot: u32,
}

/// Bounds and LODs of a GPU-ready mesh.
#[derive(Clone, Debug, PartialEq)]
pub struct GpuMeshInfo {
    /// Axis-aligned bounds min corner (object space, metres).
    pub aabb_min: [f32; 3],
    /// Axis-aligned bounds max corner.
    pub aabb_max: [f32; 3],
    /// Bounding sphere: centre xyz + radius in w.
    pub bounding_sphere: [f32; 4],
    /// LODs, most detailed first. LOD 0 is what the runtime draws.
    pub lods: Vec<GpuMeshLod>,
}

/// The optional trailing block of a version 3 payload: what a skinned or morphing compact mesh
/// carries beyond its vertex streams, in the compact mesh's packed forms (ADR-10080).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GpuMeshExtras {
    /// The packed skin: joint names, influence set 0 and, for an eight-influence skin, set 1.
    pub skin: Option<CookedSkin>,
    /// Influences per vertex the renderer draws: 4, or 8 (needs a skin with a second set). The
    /// compact mesh's `skin_influences`.
    pub skin_influences: u8,
    /// Sparse morph targets, in source order.
    pub morph_targets: Vec<CookedMorphTarget>,
}

impl Default for GpuMeshExtras {
    fn default() -> Self {
        Self {
            skin: None,
            skin_influences: crate::cooked::DEFAULT_SKIN_INFLUENCES,
            morph_targets: Vec::new(),
        }
    }
}

impl GpuMeshExtras {
    /// No skin and no morph targets: the writer writes no trailing block for it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skin.is_none() && self.morph_targets.is_empty()
    }

    /// Checks the skin and every morph target against `vertex_count`, and `skin_influences`
    /// (4, or 8 with a skin that has a second influence set).
    ///
    /// # Errors
    ///
    /// [`GpuMeshError::Malformed`] naming what is wrong.
    pub fn validate(&self, vertex_count: usize) -> Result<(), GpuMeshError> {
        let bad = |what: String| GpuMeshError::Malformed(format!("trailing block: {what}"));
        if let Some(skin) = &self.skin {
            skin.validate(vertex_count).map_err(|e| bad(e.to_string()))?;
        }
        for target in &self.morph_targets {
            target.validate(vertex_count).map_err(|e| bad(e.to_string()))?;
        }
        match self.skin_influences {
            crate::cooked::DEFAULT_SKIN_INFLUENCES => Ok(()),
            crate::cooked::EIGHT_SKIN_INFLUENCES
                if self.skin.as_ref().is_some_and(|s| !s.stream_joints1.is_empty()) =>
            {
                Ok(())
            }
            crate::cooked::EIGHT_SKIN_INFLUENCES => Err(bad(
                "skin_influences 8 needs a skin with a second influence set".to_owned(),
            )),
            other => Err(bad(format!("skin_influences {other} (4 or 8)"))),
        }
    }

    /// The trailing block's bytes (magic, length, postcard body).
    fn encode(&self) -> Result<Vec<u8>, GpuMeshError> {
        let body = postcard::to_allocvec(self).map_err(|e| GpuMeshError::Wrapper(e.to_string()))?;
        let len = u32::try_from(body.len()).map_err(|_| {
            GpuMeshError::Malformed(format!("trailing block of {} bytes does not fit u32", body.len()))
        })?;
        let mut out = Vec::with_capacity(EXTRAS_HEADER + body.len());
        out.extend_from_slice(&GPU_MESH_EXTRAS_MAGIC);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Parses and validates the trailing block `bytes` (everything after the indices).
    fn parse(bytes: &[u8], vertex_count: usize) -> Result<Self, GpuMeshError> {
        let bad = |what: &str| GpuMeshError::Malformed(format!("trailing block: {what}"));
        if bytes.len() < EXTRAS_HEADER || bytes[..4] != GPU_MESH_EXTRAS_MAGIC {
            return Err(bad("bad magic or truncated header"));
        }
        let len = u32_at(bytes, 4) as usize;
        if bytes.len() - EXTRAS_HEADER != len {
            return Err(GpuMeshError::Malformed(format!(
                "trailing block: header says {len} bytes, {} follow",
                bytes.len() - EXTRAS_HEADER
            )));
        }
        let (extras, rest): (Self, &[u8]) =
            postcard::take_from_bytes(&bytes[EXTRAS_HEADER..]).map_err(|e| bad(&e.to_string()))?;
        if !rest.is_empty() {
            return Err(bad("bytes left after the block"));
        }
        extras.validate(vertex_count)?;
        Ok(extras)
    }
}

/// What one GPU-ready mesh payload holds, for the writers that take a trailing block
/// ([`encode_gpu_mesh_parts`], [`encode_gpu_mesh_file_parts`]).
#[derive(Clone, Copy, Debug)]
pub struct GpuMeshParts<'a> {
    /// Bounds and LODs.
    pub info: &'a GpuMeshInfo,
    /// Submesh ranges (may be empty).
    pub submeshes: &'a [GpuMeshSubmesh],
    /// `vertex_count × GPU_VERTEX_STRIDE` bytes in the [`gpu_vertex`] layout.
    pub vertices: &'a [u8],
    /// Every LOD's indices.
    pub indices: &'a [u32],
    /// The skin and morph targets; `None` (or an empty block) writes no trailing block.
    pub extras: Option<&'a GpuMeshExtras>,
}

/// Errors parsing or writing a GPU-ready mesh payload.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum GpuMeshError {
    /// Not a GPU-ready mesh payload, or too short for its header.
    #[error("not a GPU-ready mesh payload (bad magic or truncated header)")]
    BadMagic,
    /// A layout version this runtime does not read.
    #[error("GPU-ready mesh layout version {found} is not supported (this runtime reads {supported})")]
    UnsupportedVersion {
        /// Version in the payload.
        found: u32,
        /// Version this runtime reads.
        supported: u32,
    },
    /// The header's sizes do not match the payload, a LOD lies outside the index buffer, or a
    /// LOD or submesh is not whole triangles.
    #[error("GPU-ready mesh payload is malformed: {0}")]
    Malformed(String),
    /// An index addresses a vertex the mesh does not have.
    #[error("index {index} at position {at} is out of range ({vertex_count} vertices)")]
    IndexOutOfRange {
        /// The index value.
        index: u32,
        /// Its position in the index buffer.
        at: usize,
        /// Vertices in the mesh.
        vertex_count: u32,
    },
    /// The cooked wrapper failed to encode.
    #[error("cooked wrapper: {0}")]
    Wrapper(String),
}

/// The `from` versions the reader still accepts (`cargo xtask check-formats`, format `gpu-mesh`):
/// [`GpuMeshView::parse`] reads a version 1 payload (no submesh table) and a version 2 payload (no
/// trailing block) in memory, and cooking writes [`GPU_MESH_VERSION`]. A cache, so nothing rewrites old files: the pak is cooked again.
#[must_use]
pub fn gpu_mesh_migration_sources() -> Vec<u32> {
    (GPU_MESH_MIN_VERSION..GPU_MESH_VERSION).collect()
}

/// The shape of the payload, for the format registry (`cargo xtask check-formats`, format `gpu-mesh`).
#[must_use]
pub fn gpu_mesh_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    let extras = join_field_lists(&[
        serde_field_names::<GpuMeshExtras>(),
        serde_field_names::<CookedSkin>(),
        serde_variant_names::<crate::mesh_skin::CookedJointWidth>(),
        serde_field_names::<CookedMorphTarget>(),
    ]);
    format!(
        "GpuMesh{{magic:FGPM,version:u32le,stride:u32le={GPU_VERTEX_STRIDE},vertex_count:u32le,index_count:u32le,\
         lod_count:u32le,vertex_offset:u32le,aabb_min:f32x3,aabb_max:f32x3,sphere:f32x4,submesh_count:u32le,\
         lods:[{{screen_relative_size:f32,first_index:u32,index_count:u32}}],\
         submeshes:[{{first_index:u32,index_count:u32,material_slot:u32}}],vertices:[u8;stride],indices:[u32le],\
         extras?:{{magic:FGPX,len:u32le,postcard:[{extras}]}}}}"
    )
}

/// A parsed GPU-ready mesh payload, borrowing its bytes.
#[derive(Clone, Debug)]
pub struct GpuMeshView<'a> {
    /// Vertex count.
    pub vertex_count: u32,
    /// Bounds and LODs.
    pub info: GpuMeshInfo,
    /// `vertex_count × GPU_VERTEX_STRIDE` bytes of vertices.
    pub vertices: &'a [u8],
    /// Every LOD's indices, `u32` little-endian.
    pub indices: &'a [u8],
    /// Submesh ranges over the index buffer; empty for a version 1 payload or a mesh without any.
    pub submeshes: Vec<GpuMeshSubmesh>,
    /// The trailing block (skin and morph targets, version 3), validated against the vertex
    /// count; `None` for a payload without one (every version 1 and 2 payload, and a rigid
    /// version 3 mesh without morph targets).
    pub extras: Option<GpuMeshExtras>,
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn f32_at(bytes: &[u8], at: usize) -> f32 {
    f32::from_bits(u32_at(bytes, at))
}

fn f32s<const N: usize>(bytes: &[u8], at: usize) -> [f32; N] {
    std::array::from_fn(|i| f32_at(bytes, at + i * 4))
}

/// A LOD (or submesh) range error shared by the writer and the parser so both report a
/// non-triangle range with the same message.
fn not_a_triangle_list(what: &str, i: usize, index_count: u32) -> GpuMeshError {
    GpuMeshError::Malformed(format!(
        "{what} {i} is not a triangle list (index_count {index_count})"
    ))
}

impl<'a> GpuMeshView<'a> {
    /// Parses and validates a payload: header, sizes, LOD ranges (not the index values; see
    /// [`Self::lod_indices`]).
    ///
    /// # Errors
    ///
    /// A bad magic, an unknown version or stride, or sizes that do not match the payload.
    pub fn parse(payload: &'a [u8]) -> Result<Self, GpuMeshError> {
        if payload.len() < FIXED_HEADER_V1 || payload[..4] != GPU_MESH_MAGIC {
            return Err(GpuMeshError::BadMagic);
        }
        let version = u32_at(payload, 4);
        if !(GPU_MESH_MIN_VERSION..=GPU_MESH_VERSION).contains(&version) {
            return Err(GpuMeshError::UnsupportedVersion {
                found: version,
                supported: GPU_MESH_VERSION,
            });
        }
        let (header_len, submesh_count) = if version >= 2 {
            if payload.len() < FIXED_HEADER {
                return Err(GpuMeshError::BadMagic);
            }
            (FIXED_HEADER, u32_at(payload, 68) as usize)
        } else {
            (FIXED_HEADER_V1, 0)
        };
        let stride = u32_at(payload, 8);
        if stride != GPU_VERTEX_STRIDE {
            return Err(GpuMeshError::Malformed(format!(
                "vertex stride {stride}, layout 1 has {GPU_VERTEX_STRIDE}"
            )));
        }
        let vertex_count = u32_at(payload, 12);
        let index_count = u32_at(payload, 16);
        let lod_count = u32_at(payload, 20) as usize;
        let vertex_offset = u32_at(payload, 24) as usize;
        let bad = |what: &str| GpuMeshError::Malformed(what.to_string());
        let lod_end = lod_count
            .checked_mul(LOD_ROW)
            .and_then(|n| n.checked_add(header_len))
            .ok_or_else(|| bad("LOD table size overflows"))?;
        let table_end = submesh_count
            .checked_mul(LOD_ROW)
            .and_then(|n| n.checked_add(lod_end))
            .ok_or_else(|| bad("submesh table size overflows"))?;
        if table_end > vertex_offset {
            return Err(bad("the LOD and submesh tables run into the vertices"));
        }
        let vertex_bytes = (vertex_count as usize)
            .checked_mul(GPU_VERTEX_STRIDE as usize)
            .ok_or_else(|| bad("vertex size overflows"))?;
        let index_bytes = (index_count as usize)
            .checked_mul(4)
            .ok_or_else(|| bad("index size overflows"))?;
        let end = vertex_offset
            .checked_add(vertex_bytes)
            .and_then(|n| n.checked_add(index_bytes))
            .ok_or_else(|| bad("payload size overflows"))?;
        let extras = if end == payload.len() {
            None
        } else if version >= EXTRAS_VERSION && end < payload.len() {
            Some(GpuMeshExtras::parse(&payload[end..], vertex_count as usize)?)
        } else {
            return Err(GpuMeshError::Malformed(format!(
                "header describes {end} bytes, the payload has {}",
                payload.len()
            )));
        };
        let mut lods = Vec::with_capacity(lod_count);
        for i in 0..lod_count {
            let at = header_len + i * LOD_ROW;
            let lod = GpuMeshLod {
                screen_relative_size: f32_at(payload, at),
                first_index: u32_at(payload, at + 4),
                index_count: u32_at(payload, at + 8),
            };
            if u64::from(lod.first_index) + u64::from(lod.index_count) > u64::from(index_count) {
                return Err(GpuMeshError::Malformed(format!(
                    "LOD {i} lies outside the index buffer"
                )));
            }
            if !lod.index_count.is_multiple_of(3) {
                return Err(not_a_triangle_list("LOD", i, lod.index_count));
            }
            lods.push(lod);
        }
        let mut submeshes = Vec::with_capacity(submesh_count);
        for i in 0..submesh_count {
            let at = lod_end + i * LOD_ROW;
            let sub = GpuMeshSubmesh {
                first_index: u32_at(payload, at),
                index_count: u32_at(payload, at + 4),
                material_slot: u32_at(payload, at + 8),
            };
            if u64::from(sub.first_index) + u64::from(sub.index_count) > u64::from(index_count)
                || !sub.index_count.is_multiple_of(3)
            {
                return Err(GpuMeshError::Malformed(format!(
                    "submesh {i} lies outside the index buffer"
                )));
            }
            submeshes.push(sub);
        }
        let vertices_end = vertex_offset + vertex_bytes;
        Ok(Self {
            vertex_count,
            info: GpuMeshInfo {
                aabb_min: f32s(payload, 28),
                aabb_max: f32s(payload, 40),
                bounding_sphere: f32s(payload, 52),
                lods,
            },
            vertices: &payload[vertex_offset..vertices_end],
            indices: &payload[vertices_end..end],
            submeshes,
            extras,
        })
    }

    /// The indices of LOD `lod` (all indices when the mesh lists no LOD), each checked against
    /// the vertex count.
    ///
    /// # Errors
    ///
    /// [`GpuMeshError::IndexOutOfRange`] for an index past the last vertex;
    /// [`GpuMeshError::Malformed`] for a LOD the mesh does not have.
    pub fn lod_indices(&self, lod: usize) -> Result<Vec<u32>, GpuMeshError> {
        let bytes = if self.info.lods.is_empty() && lod == 0 {
            self.indices
        } else {
            let l = self
                .info
                .lods
                .get(lod)
                .ok_or_else(|| GpuMeshError::Malformed(format!("no LOD {lod}")))?;
            let start = l.first_index as usize * 4;
            &self.indices[start..start + l.index_count as usize * 4]
        };
        let mut out = Vec::with_capacity(bytes.len() / 4);
        for (at, chunk) in bytes.as_chunks::<4>().0.iter().enumerate() {
            let index = u32::from_le_bytes(*chunk);
            if index >= self.vertex_count {
                return Err(GpuMeshError::IndexOutOfRange {
                    index,
                    at,
                    vertex_count: self.vertex_count,
                });
            }
            out.push(index);
        }
        Ok(out)
    }

    /// The indices of submesh `submesh`, each checked against the vertex count.
    ///
    /// # Errors
    ///
    /// [`GpuMeshError::IndexOutOfRange`] for an index past the last vertex;
    /// [`GpuMeshError::Malformed`] for a submesh the mesh does not have.
    pub fn submesh_indices(&self, submesh: usize) -> Result<Vec<u32>, GpuMeshError> {
        let s = self
            .submeshes
            .get(submesh)
            .ok_or_else(|| GpuMeshError::Malformed(format!("no submesh {submesh}")))?;
        let start = s.first_index as usize * 4;
        let bytes = &self.indices[start..start + s.index_count as usize * 4];
        let mut out = Vec::with_capacity(bytes.len() / 4);
        for (at, chunk) in bytes.as_chunks::<4>().0.iter().enumerate() {
            let index = u32::from_le_bytes(*chunk);
            if index >= self.vertex_count {
                return Err(GpuMeshError::IndexOutOfRange {
                    index,
                    at: s.first_index as usize + at,
                    vertex_count: self.vertex_count,
                });
            }
            out.push(index);
        }
        Ok(out)
    }

    /// Positions, normals, UVs, tangents, `COLOR_0` and LOD 0 indices as a [`CpuMesh`] (the asset
    /// server's mesh type, for tools and `AssetServer::load_mesh`).
    ///
    /// [`CpuMesh::colors`] holds the [`gpu_vertex::COLOR`] slot of every vertex when any vertex is
    /// not white, and stays empty otherwise (a mesh cooked without `COLOR_0`; a `COLOR_0` stream
    /// that is white on every vertex reads back as none, which multiplies the same). The skin
    /// (with `eight_influences` from the block's `skin_influences`) and the morph targets come from
    /// the trailing block, unpacked as `CookedMesh::to_cpu_mesh` unpacks them; both stay empty
    /// without one (ADR-10080). The tangents are the stored MikkTSpace tangents.
    ///
    /// # Errors
    ///
    /// As [`Self::lod_indices`].
    pub fn to_cpu_mesh(&self) -> Result<CpuMesh, GpuMeshError> {
        let n = self.vertex_count as usize;
        let mut mesh = CpuMesh {
            positions: Vec::with_capacity(n),
            normals: Vec::with_capacity(n),
            uvs: Vec::with_capacity(n),
            tangents: Vec::with_capacity(n),
            colors: Vec::with_capacity(n),
            indices: self.lod_indices(0)?,
            ..CpuMesh::default()
        };
        for v in self.vertices.as_chunks::<{ GPU_VERTEX_STRIDE as usize }>().0 {
            mesh.positions.push(f32s(v, gpu_vertex::POSITION));
            mesh.normals.push(f32s(v, gpu_vertex::NORMAL));
            mesh.uvs.push(f32s(v, gpu_vertex::UV));
            mesh.tangents.push(f32s(v, gpu_vertex::TANGENT));
            mesh.colors.push(f32s(v, gpu_vertex::COLOR));
        }
        if mesh.colors.iter().all(|c| *c == [1.0; 4]) {
            mesh.colors = Vec::new();
        }
        if let Some(extras) = &self.extras {
            let bad = |e: crate::cooked::CookedMeshValidationError| {
                GpuMeshError::Malformed(format!("trailing block: {e}"))
            };
            if let Some(skin) = &extras.skin {
                let mut skin = skin.unpack(n).map_err(bad)?;
                skin.eight_influences = extras.skin_influences == crate::cooked::EIGHT_SKIN_INFLUENCES;
                mesh.skin = Some(skin);
            }
            mesh.morph_targets = extras
                .morph_targets
                .iter()
                .map(|t| t.unpack(n))
                .collect::<Result<_, _>>()
                .map_err(bad)?;
        }
        Ok(mesh)
    }
}

/// Encodes a GPU-ready mesh payload whose vertices start at a multiple of 16 bytes from
/// `payload_start` (the payload's offset within the file it will sit in).
///
/// `vertices` must be `vertex_count × GPU_VERTEX_STRIDE` bytes in the [`gpu_vertex`] layout.
///
/// # Errors
///
/// [`GpuMeshError::Malformed`] when `vertices` is not a whole number of vertices, a count does not
/// fit `u32`, a LOD lies outside `indices`, or a LOD is not whole triangles.
pub fn encode_gpu_mesh(
    info: &GpuMeshInfo,
    vertices: &[u8],
    indices: &[u32],
    payload_start: usize,
) -> Result<Vec<u8>, GpuMeshError> {
    encode_gpu_mesh_with_submeshes(info, &[], vertices, indices, payload_start)
}

/// [`encode_gpu_mesh`] with a submesh table (layout version 2).
///
/// # Errors
///
/// As [`encode_gpu_mesh`], or [`GpuMeshError::Malformed`] when a submesh lies outside `indices` or
/// is not whole triangles.
pub fn encode_gpu_mesh_with_submeshes(
    info: &GpuMeshInfo,
    submeshes: &[GpuMeshSubmesh],
    vertices: &[u8],
    indices: &[u32],
    payload_start: usize,
) -> Result<Vec<u8>, GpuMeshError> {
    encode_gpu_mesh_parts(
        &GpuMeshParts {
            info,
            submeshes,
            vertices,
            indices,
            extras: None,
        },
        payload_start,
    )
}

/// [`encode_gpu_mesh_with_submeshes`] with the optional trailing block (`parts.extras`, layout
/// version 3): a skin or morph targets are written after the indices; `None` or an empty block
/// writes none.
///
/// # Errors
///
/// As [`encode_gpu_mesh_with_submeshes`], or [`GpuMeshError::Malformed`] when the block does not
/// validate against the vertex count.
pub fn encode_gpu_mesh_parts(
    parts: &GpuMeshParts<'_>,
    payload_start: usize,
) -> Result<Vec<u8>, GpuMeshError> {
    let extras = extras_bytes(parts)?;
    encode_gpu_mesh_padded(parts, &extras, payload_start, 0)
}

/// The trailing block bytes of `parts` (empty when it has none), validated.
fn extras_bytes(parts: &GpuMeshParts<'_>) -> Result<Vec<u8>, GpuMeshError> {
    match parts.extras.filter(|x| !x.is_empty()) {
        None => Ok(Vec::new()),
        Some(extras) => {
            let vertex_count = parts.vertices.len() / GPU_VERTEX_STRIDE as usize;
            extras.validate(vertex_count)?;
            extras.encode()
        }
    }
}

/// [`encode_gpu_mesh_with_submeshes`] with `extra_pad` additional padding bytes before the
/// vertices (on top of the minimal 16-byte alignment pad; the extra bytes keep the same
/// 16-byte residue, so the vertices stay aligned). The cooked-file writer uses one extra
/// alignment block to bridge a postcard varint boundary in the wrapper header (module docs).
fn encode_gpu_mesh_padded(
    parts: &GpuMeshParts<'_>,
    extras: &[u8],
    payload_start: usize,
    extra_pad: usize,
) -> Result<Vec<u8>, GpuMeshError> {
    let GpuMeshParts {
        info,
        submeshes,
        vertices,
        indices,
        ..
    } = *parts;
    let stride = GPU_VERTEX_STRIDE as usize;
    if !vertices.len().is_multiple_of(stride) {
        return Err(GpuMeshError::Malformed(format!(
            "{} vertex bytes are not a multiple of the {stride}-byte stride",
            vertices.len()
        )));
    }
    let count = |n: usize, what: &str| {
        u32::try_from(n).map_err(|_| GpuMeshError::Malformed(format!("{what} {n} does not fit u32")))
    };
    let vertex_count = count(vertices.len() / stride, "vertex count")?;
    let index_count = count(indices.len(), "index count")?;
    for (i, lod) in info.lods.iter().enumerate() {
        if u64::from(lod.first_index) + u64::from(lod.index_count) > u64::from(index_count) {
            return Err(GpuMeshError::Malformed(format!(
                "LOD {i} lies outside the index buffer"
            )));
        }
        // The reader refuses a LOD that is not whole triangles: never write one (corr-asset;
        // fix-asset2 shares the reader's message).
        if !lod.index_count.is_multiple_of(3) {
            return Err(not_a_triangle_list("LOD", i, lod.index_count));
        }
    }
    for (i, sub) in submeshes.iter().enumerate() {
        if u64::from(sub.first_index) + u64::from(sub.index_count) > u64::from(index_count)
            || !sub.index_count.is_multiple_of(3)
        {
            return Err(GpuMeshError::Malformed(format!(
                "submesh {i} lies outside the index buffer"
            )));
        }
    }
    let table_end = FIXED_HEADER + (info.lods.len() + submeshes.len()) * LOD_ROW;
    let vertex_offset = aligned_vertex_offset(payload_start, table_end) + extra_pad;
    let mut out = Vec::with_capacity(vertex_offset + vertices.len() + indices.len() * 4 + extras.len());
    out.extend_from_slice(&GPU_MESH_MAGIC);
    for word in [
        GPU_MESH_VERSION,
        GPU_VERTEX_STRIDE,
        vertex_count,
        index_count,
        count(info.lods.len(), "LOD count")?,
        count(vertex_offset, "vertex offset")?,
    ] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    for f in info
        .aabb_min
        .iter()
        .chain(&info.aabb_max)
        .chain(&info.bounding_sphere)
    {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out.extend_from_slice(&count(submeshes.len(), "submesh count")?.to_le_bytes());
    for lod in &info.lods {
        out.extend_from_slice(&lod.screen_relative_size.to_le_bytes());
        out.extend_from_slice(&lod.first_index.to_le_bytes());
        out.extend_from_slice(&lod.index_count.to_le_bytes());
    }
    for sub in submeshes {
        out.extend_from_slice(&sub.first_index.to_le_bytes());
        out.extend_from_slice(&sub.index_count.to_le_bytes());
        out.extend_from_slice(&sub.material_slot.to_le_bytes());
    }
    out.resize(vertex_offset, 0);
    out.extend_from_slice(vertices);
    for index in indices {
        out.extend_from_slice(&index.to_le_bytes());
    }
    out.extend_from_slice(extras);
    Ok(out)
}

/// A whole cooked file (wrapper header + GPU-ready payload, type [`GPU_MESH_TYPE_PATH`]) for
/// `asset_id`, with the vertices 16-byte aligned from the start of the file.
///
/// # Errors
///
/// As [`encode_gpu_mesh`], or [`GpuMeshError::Wrapper`] when the wrapper does not encode.
pub fn encode_gpu_mesh_file(
    asset_id: AssetId,
    importer_version: u32,
    dependencies: Vec<AssetId>,
    info: &GpuMeshInfo,
    vertices: &[u8],
    indices: &[u32],
) -> Result<Vec<u8>, GpuMeshError> {
    encode_gpu_mesh_file_with_submeshes(
        asset_id,
        importer_version,
        dependencies,
        info,
        &[],
        vertices,
        indices,
    )
}

/// [`encode_gpu_mesh_file`] with a submesh table (layout version 2).
///
/// # Errors
///
/// As [`encode_gpu_mesh_with_submeshes`], or [`GpuMeshError::Wrapper`].
pub fn encode_gpu_mesh_file_with_submeshes(
    asset_id: AssetId,
    importer_version: u32,
    dependencies: Vec<AssetId>,
    info: &GpuMeshInfo,
    submeshes: &[GpuMeshSubmesh],
    vertices: &[u8],
    indices: &[u32],
) -> Result<Vec<u8>, GpuMeshError> {
    encode_gpu_mesh_file_parts(
        asset_id,
        importer_version,
        dependencies,
        &GpuMeshParts {
            info,
            submeshes,
            vertices,
            indices,
            extras: None,
        },
    )
}

/// [`encode_gpu_mesh_file_with_submeshes`] with the optional trailing block (`parts.extras`,
/// layout version 3: a skinned or morphing mesh).
///
/// # Errors
///
/// As [`encode_gpu_mesh_parts`], or [`GpuMeshError::Wrapper`].
pub fn encode_gpu_mesh_file_parts(
    asset_id: AssetId,
    importer_version: u32,
    dependencies: Vec<AssetId>,
    parts: &GpuMeshParts<'_>,
) -> Result<Vec<u8>, GpuMeshError> {
    let GpuMeshParts {
        info,
        submeshes,
        vertices,
        indices,
        ..
    } = *parts;
    let extras = extras_bytes(parts)?;
    let wrap = |e: CookedIoError| GpuMeshError::Wrapper(e.to_string());
    let mut header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: regia_core::id::TypePathHash::of(GPU_MESH_TYPE_PATH),
        asset_id,
        importer_version,
        dependencies,
        payload_len: 0,
    };
    let mut header_len_for = |payload_len: u64| -> Result<usize, GpuMeshError> {
        header.payload_len = payload_len;
        Ok(4 + postcard::to_allocvec(&header)
            .map_err(|e| GpuMeshError::Wrapper(e.to_string()))?
            .len())
    };
    // The wrapper header is postcard (varint lengths): its size depends on the payload length,
    // which depends on the padding. The payload is the table, up to two alignment blocks of
    // padding and the vertex and index bytes, so its length stays within 32 bytes of `unpadded`
    // and crosses at most one varint boundary (127 / 16383 / 2097151): the header length takes
    // at most two values over the possible payloads. A naive fixed-point iteration can bounce
    // between the two values across a boundary forever, so solve the alignment directly: for
    // each candidate header length, pad the vertices to it (the optional extra block keeps the
    // same 16-byte residue) and accept the candidate when the payload it yields serializes the
    // header back to the same length. At least one candidate always accepts, and the winner
    // uses the minimal pad whenever a minimal-pad solution exists.
    let table_end = FIXED_HEADER + (info.lods.len() + submeshes.len()) * LOD_ROW;
    let unpadded = table_end + vertices.len() + indices.len() * 4 + extras.len();
    let upper = (unpadded + 2 * (VERTEX_ALIGN - 1)) as u64;
    let candidates = [header_len_for(unpadded as u64)?, header_len_for(upper)?];
    let mut solved = None;
    'candidates: for &candidate in &candidates {
        for extra_pad in [0usize, VERTEX_ALIGN] {
            let pad = (VERTEX_ALIGN - (candidate + table_end) % VERTEX_ALIGN) % VERTEX_ALIGN + extra_pad;
            let payload_len = (unpadded + pad) as u64;
            if header_len_for(payload_len)? == candidate {
                let payload = encode_gpu_mesh_padded(parts, &extras, candidate, extra_pad)?;
                debug_assert_eq!(payload.len() as u64, payload_len);
                solved = Some((payload_len, payload));
                break 'candidates;
            }
        }
    }
    let (payload_len, payload) = solved.ok_or_else(|| {
        GpuMeshError::Wrapper("no cooked header length pads the vertices to alignment".to_owned())
    })?;
    header.payload_len = payload_len;
    encode_cooked_file(&header, &payload).map_err(wrap)
}

/// Vertex offset of a payload whose vertices are 16-byte aligned from `payload_start`.
fn aligned_vertex_offset(payload_start: usize, table_end: usize) -> usize {
    let pad = (VERTEX_ALIGN - (payload_start + table_end) % VERTEX_ALIGN) % VERTEX_ALIGN;
    table_end + pad
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cooked_header::decode_cooked_file;

    fn vertex(p: [f32; 3]) -> Vec<u8> {
        let mut v = vec![0u8; GPU_VERTEX_STRIDE as usize];
        let mut put = |at: usize, fs: &[f32]| {
            for (i, f) in fs.iter().enumerate() {
                v[at + i * 4..at + i * 4 + 4].copy_from_slice(&f.to_le_bytes());
            }
        };
        put(gpu_vertex::POSITION, &p);
        put(gpu_vertex::NORMAL, &[0.0, 0.0, 1.0]);
        put(gpu_vertex::UV, &[p[0], p[1]]);
        put(gpu_vertex::TANGENT, &[1.0, 0.0, 0.0, -1.0]);
        put(gpu_vertex::COLOR, &[1.0; 4]);
        v
    }

    /// corr-asset: the writer refused a submesh that is not whole triangles but wrote such a LOD,
    /// which the reader then refuses: a cooked mesh the runtime cannot load.
    #[test]
    fn corr_asset_a_lod_of_partial_triangles_is_not_written() {
        let (mut info, vertices, indices) = quad();
        info.lods[0].index_count = 4;
        assert!(matches!(
            encode_gpu_mesh(&info, &vertices, &indices, 0),
            Err(GpuMeshError::Malformed(_))
        ));
    }

    fn quad() -> (GpuMeshInfo, Vec<u8>, Vec<u32>) {
        let vertices: Vec<u8> = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]]
            .into_iter()
            .flat_map(vertex)
            .collect();
        let indices = vec![0, 1, 2, 0, 2, 3, 0, 1, 2];
        let info = GpuMeshInfo {
            aabb_min: [0.0; 3],
            aabb_max: [1.0, 1.0, 0.0],
            bounding_sphere: [0.5, 0.5, 0.0, 0.75],
            lods: vec![
                GpuMeshLod {
                    screen_relative_size: 1.0,
                    first_index: 0,
                    index_count: 6,
                },
                GpuMeshLod {
                    screen_relative_size: 0.5,
                    first_index: 6,
                    index_count: 3,
                },
            ],
        };
        (info, vertices, indices)
    }

    #[test]
    fn round_trips_through_a_cooked_file_with_aligned_vertices() {
        let (info, vertices, indices) = quad();
        let id = AssetId::new_random();
        for deps in [0usize, 1, 7] {
            let file = encode_gpu_mesh_file(id, 1_000_000, vec![id; deps], &info, &vertices, &indices)
                .expect("encode");
            let (header, payload) = decode_cooked_file(&file).expect("wrapper");
            assert_eq!(
                header.type_tag,
                regia_core::id::TypePathHash::of(GPU_MESH_TYPE_PATH)
            );
            let view = GpuMeshView::parse(payload).expect("parse");
            let at = view.vertices.as_ptr() as usize - file.as_ptr() as usize;
            assert_eq!(
                at % 16,
                0,
                "vertices 16-aligned in the file ({deps} dependencies)"
            );
            assert_eq!(view.vertex_count, 4);
            assert_eq!(view.vertices, &vertices[..]);
            assert_eq!(view.info, info);
            assert_eq!(view.lod_indices(0).expect("lod 0"), vec![0, 1, 2, 0, 2, 3]);
            assert_eq!(view.lod_indices(1).expect("lod 1"), vec![0, 1, 2]);
            let cpu = view.to_cpu_mesh().expect("cpu");
            assert!(cpu.validate().is_ok());
            assert_eq!(cpu.positions[2], [1.0, 1.0, 0.0]);
            assert_eq!(cpu.tangents[3], [1.0, 0.0, 0.0, -1.0]);
            assert_eq!(cpu.indices.len(), 6);
        }
    }

    /// Builds a version 1 payload the way the previous writer did (68-byte header, no submesh
    /// table) from the same quad.
    fn v1_payload(info: &GpuMeshInfo, vertices: &[u8], indices: &[u32]) -> Vec<u8> {
        let table_end = FIXED_HEADER_V1 + info.lods.len() * LOD_ROW;
        let vertex_offset = table_end.next_multiple_of(16);
        let mut out = Vec::new();
        out.extend_from_slice(&GPU_MESH_MAGIC);
        for w in [
            1u32,
            GPU_VERTEX_STRIDE,
            (vertices.len() / GPU_VERTEX_STRIDE as usize) as u32,
            indices.len() as u32,
            info.lods.len() as u32,
            vertex_offset as u32,
        ] {
            out.extend_from_slice(&w.to_le_bytes());
        }
        for f in info
            .aabb_min
            .iter()
            .chain(&info.aabb_max)
            .chain(&info.bounding_sphere)
        {
            out.extend_from_slice(&f.to_le_bytes());
        }
        for l in &info.lods {
            out.extend_from_slice(&l.screen_relative_size.to_le_bytes());
            out.extend_from_slice(&l.first_index.to_le_bytes());
            out.extend_from_slice(&l.index_count.to_le_bytes());
        }
        out.resize(vertex_offset, 0);
        out.extend_from_slice(vertices);
        for i in indices {
            out.extend_from_slice(&i.to_le_bytes());
        }
        out
    }

    #[test]
    fn a_version_1_payload_still_reads_with_no_submeshes() {
        let (info, vertices, indices) = quad();
        let old = v1_payload(&info, &vertices, &indices);
        let view = GpuMeshView::parse(&old).expect("v1 reads");
        assert!(view.submeshes.is_empty());
        assert_eq!(view.info, info);
        assert_eq!(view.vertices, &vertices[..]);
        assert_eq!(view.lod_indices(0).expect("lod"), vec![0, 1, 2, 0, 2, 3]);
        assert!(matches!(view.submesh_indices(0), Err(GpuMeshError::Malformed(_))));
    }

    #[test]
    fn submesh_ranges_round_trip_and_are_checked() {
        let (info, vertices, indices) = quad();
        let subs = [
            GpuMeshSubmesh {
                first_index: 0,
                index_count: 3,
                material_slot: 0,
            },
            GpuMeshSubmesh {
                first_index: 3,
                index_count: 3,
                material_slot: 1,
            },
        ];
        let payload = encode_gpu_mesh_with_submeshes(&info, &subs, &vertices, &indices, 0).expect("encode");
        let view = GpuMeshView::parse(&payload).expect("parse");
        assert_eq!(view.submeshes, subs);
        assert_eq!(view.submesh_indices(0).expect("0"), vec![0, 1, 2]);
        assert_eq!(view.submesh_indices(1).expect("1"), vec![0, 2, 3]);
        assert_eq!(view.info, info);
        for bad in [(8, 3), (0, 4)] {
            let b = [GpuMeshSubmesh {
                first_index: bad.0,
                index_count: bad.1,
                material_slot: 0,
            }];
            assert!(encode_gpu_mesh_with_submeshes(&info, &b, &vertices, &indices, 0).is_err());
        }
        // A hostile count runs the table into the vertices.
        let mut hostile = payload.clone();
        hostile[68..72].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            GpuMeshView::parse(&hostile),
            Err(GpuMeshError::Malformed(_))
        ));
        for len in 0..payload.len() {
            assert!(GpuMeshView::parse(&payload[..len]).is_err(), "truncated to {len}");
        }
    }

    /// Review finding (asset2 correctness, finding 1): the wrapper header is postcard (varint
    /// lengths), so a payload length crossing 127 / 16383 / 2097151 grows the header by one byte.
    /// A fixed-point loop with too few rounds pads the vertices for the old header size and the
    /// vertices end up misaligned in the file.
    #[test]
    fn encode_gpu_mesh_file_vertices_stay_16_aligned_across_a_varint_boundary() {
        let info = GpuMeshInfo {
            aabb_min: [0.0; 3],
            aabb_max: [1.0, 1.0, 0.0],
            bounding_sphere: [0.5, 0.5, 0.0, 0.75],
            lods: vec![GpuMeshLod {
                screen_relative_size: 1.0,
                first_index: 0,
                index_count: 4050,
            }],
        };
        let vertices = vertex([0.0, 0.0, 0.0]);
        let indices: Vec<u32> = (0..4050).map(|i| i % 3).collect();
        // A large id keeps the id field at its biggest postcard varint size, so the header
        // length only moves when the payload length crosses a varint boundary.
        let id = AssetId(regia_core::id::Guid(1u128 << 126));
        let file = encode_gpu_mesh_file(
            id,
            crate::cooked_header::pack_importer_version(1, 0, 0),
            Vec::new(),
            &info,
            &vertices,
            &indices,
        )
        .expect("encode");
        let (header, payload) = decode_cooked_file(&file).expect("wrapper");
        assert_eq!(header.payload_len as usize, payload.len());
        let view = GpuMeshView::parse(payload).expect("parse");
        let at = view.vertices.as_ptr() as usize - file.as_ptr() as usize;
        assert_eq!(
            at % 16,
            0,
            "vertices 16-aligned in the file (vertex_offset {}, header {} bytes)",
            u32_at(payload, 24),
            file.len() - payload.len()
        );
    }

    /// Review finding (asset2 correctness, finding 2): a LOD whose `index_count` is not a
    /// multiple of 3 encodes fine but the payload the writer just produced fails to parse.
    #[test]
    fn encode_then_parse_rejects_a_non_triangle_lod_as_a_triangle_count() {
        let (info, vertices, _indices) = quad();
        let indices = vec![0u32, 1, 2];
        let mut bad = info;
        bad.lods = vec![GpuMeshLod {
            screen_relative_size: 1.0,
            first_index: 0,
            index_count: 2,
        }];
        // The range sits inside the 3-index buffer; only the triangle count is wrong.
        let err =
            encode_gpu_mesh(&bad, &vertices, &indices, 0).expect_err("a 2-index LOD is not a triangle list");
        assert!(
            err.to_string().contains("not a triangle list"),
            "says what is wrong: {err}"
        );
        // The parser reports the same failure with the same message.
        let good = encode_gpu_mesh(
            &GpuMeshInfo {
                lods: vec![GpuMeshLod {
                    screen_relative_size: 1.0,
                    first_index: 0,
                    index_count: 3,
                }],
                ..bad.clone()
            },
            &vertices,
            &indices,
            0,
        )
        .expect("encode");
        let mut hostile = good;
        hostile[FIXED_HEADER + 8..FIXED_HEADER + 12].copy_from_slice(&2u32.to_le_bytes());
        let err = GpuMeshView::parse(&hostile).expect_err("a 2-index LOD is not a triangle list");
        assert!(
            err.to_string().contains("not a triangle list"),
            "parse says what is wrong: {err}"
        );
    }

    #[test]
    fn rejects_hostile_or_foreign_payloads() {
        let (info, vertices, indices) = quad();
        let good = encode_gpu_mesh(&info, &vertices, &indices, 0).expect("encode");
        assert!(matches!(
            GpuMeshView::parse(&good[..10]),
            Err(GpuMeshError::BadMagic)
        ));
        let mut v4 = good.clone();
        v4[4] = 4;
        assert!(matches!(
            GpuMeshView::parse(&v4),
            Err(GpuMeshError::UnsupportedVersion { found: 4, .. })
        ));
        let mut v0 = good.clone();
        v0[4] = 0;
        assert!(matches!(
            GpuMeshView::parse(&v0),
            Err(GpuMeshError::UnsupportedVersion { found: 0, .. })
        ));
        // Every truncation errors and never panics.
        for len in 0..good.len() {
            assert!(GpuMeshView::parse(&good[..len]).is_err(), "truncated to {len}");
        }
        let mut huge = good.clone();
        huge[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            GpuMeshView::parse(&huge),
            Err(GpuMeshError::Malformed(_))
        ));
        let mut lod = good.clone();
        lod[FIXED_HEADER + 8..FIXED_HEADER + 12].copy_from_slice(&900u32.to_le_bytes());
        assert!(matches!(
            GpuMeshView::parse(&lod),
            Err(GpuMeshError::Malformed(_))
        ));
        assert!(matches!(
            GpuMeshView::parse(&good[..good.len() - 4]),
            Err(GpuMeshError::Malformed(_))
        ));
        let mut index = good;
        let last = index.len() - 4;
        index[last..].copy_from_slice(&4u32.to_le_bytes());
        let view = GpuMeshView::parse(&index).expect("sizes are fine");
        assert!(view.lod_indices(0).is_ok());
        assert!(matches!(
            view.lod_indices(1),
            Err(GpuMeshError::IndexOutOfRange { index: 4, .. })
        ));
    }

    /// ADR-7919: the payload's COLOR slot carries `COLOR_0`, so `to_cpu_mesh` (and so
    /// `AssetServer::load_mesh` of a GPU-ready pak entry) returns the colors; a mesh whose
    /// slot is white on every vertex (no `COLOR_0`) keeps `colors` empty, as before.
    #[test]
    fn to_cpu_mesh_returns_color0_from_the_color_slot() {
        let (info, mut vertices, indices) = quad();
        let white = GpuMeshView::parse(&encode_gpu_mesh(&info, &vertices, &indices, 0).expect("encode"))
            .expect("parse")
            .to_cpu_mesh()
            .expect("cpu");
        assert!(white.colors.is_empty(), "white slots are no COLOR_0 stream");

        let colors: [[f32; 4]; 4] = [
            [1.0, 0.5, 0.25, 1.0],
            [1.0; 4],
            [0.0, 0.0, 1.0, 0.5],
            [0.2, 0.4, 0.6, 0.8],
        ];
        for (v, c) in vertices
            .as_chunks_mut::<{ GPU_VERTEX_STRIDE as usize }>()
            .0
            .iter_mut()
            .zip(colors)
        {
            for (i, f) in c.iter().enumerate() {
                let at = gpu_vertex::COLOR + i * 4;
                v[at..at + 4].copy_from_slice(&f.to_le_bytes());
            }
        }
        let payload = encode_gpu_mesh(&info, &vertices, &indices, 0).expect("encode");
        let cpu = GpuMeshView::parse(&payload)
            .expect("parse")
            .to_cpu_mesh()
            .expect("cpu");
        assert_eq!(cpu.colors, colors.to_vec());
        assert!(cpu.validate().is_ok());
        assert!(cpu.skin.is_none() && cpu.morph_targets.is_empty());
    }

    /// An eight-influence skin over the quad's four vertices (both sets sum to 65535) and two
    /// sparse morph targets, in the packed forms the compact mesh stores (values exact in f16).
    fn skin_and_morphs() -> GpuMeshExtras {
        let skin = crate::CpuSkin {
            joint_names: vec!["root".into(), "spine".into(), "head".into()],
            joints: vec![[0, 1, 2, 0], [1, 2, 0, 1], [2, 0, 1, 2], [0, 0, 1, 1]],
            weights: vec![
                [20000, 10000, 5000, 535],
                [35535, 10000, 10000, 10000],
                [40000, 0, 0, 0],
                [10000, 10000, 10000, 10000],
            ],
            joints1: vec![[1, 2, 0, 1], [0, 0, 0, 0], [1, 2, 1, 2], [2, 2, 0, 0]],
            weights1: vec![
                [10000, 10000, 5000, 5000],
                [0, 0, 0, 0],
                [10000, 10000, 5000, 535],
                [10000, 10000, 5535, 0],
            ],
            eight_influences: true,
        };
        let morph = |name: &str, at: Vec<u32>, d: Vec<[f32; 3]>, normals: bool| crate::CpuMorphTarget {
            name: name.into(),
            vertex_indices: at,
            normal_deltas: if normals {
                d.iter().map(|v| [v[1], v[0], 0.0]).collect()
            } else {
                Vec::new()
            },
            position_deltas: d,
            tangent_deltas: Vec::new(),
        };
        GpuMeshExtras {
            skin: Some(skin.pack(4).expect("skin packs")),
            skin_influences: crate::cooked::EIGHT_SKIN_INFLUENCES,
            morph_targets: vec![
                morph(
                    "smile",
                    vec![1, 3],
                    vec![[0.5, 0.25, -0.125], [0.0, 1.0, 2.0]],
                    true,
                )
                .pack(4)
                .expect("morph packs"),
                morph("blink", vec![0, 1, 2], vec![[0.0, -0.5, 0.0]; 3], false)
                    .pack(4)
                    .expect("morph packs"),
            ],
        }
    }

    /// ADR-10080 acceptance: an eight-influence skinned mesh with morph targets round-trips through
    /// a version 3 cooked file: the trailing block reads back equal, `to_cpu_mesh` returns the
    /// skin (both sets, `eight_influences`) and the deltas exactly as the packed forms unpack,
    /// and the vertices stay 16-aligned with the block after the indices.
    #[test]
    fn a_skinned_morphing_mesh_round_trips_its_trailing_block() {
        let (info, vertices, indices) = quad();
        let extras = skin_and_morphs();
        let id = AssetId::new_random();
        for deps in [0usize, 1, 7] {
            let parts = GpuMeshParts {
                info: &info,
                submeshes: &[],
                vertices: &vertices,
                indices: &indices,
                extras: Some(&extras),
            };
            let file = encode_gpu_mesh_file_parts(id, 1, vec![id; deps], &parts).expect("encode");
            let (_, payload) = decode_cooked_file(&file).expect("wrapper");
            assert_eq!(u32_at(payload, 4), GPU_MESH_VERSION);
            let view = GpuMeshView::parse(payload).expect("parse");
            let at = view.vertices.as_ptr() as usize - file.as_ptr() as usize;
            assert_eq!(at % 16, 0, "vertices 16-aligned ({deps} dependencies)");
            assert_eq!(view.extras.as_ref(), Some(&extras));
            assert_eq!(view.vertices, &vertices[..]);
            let cpu = view.to_cpu_mesh().expect("cpu");
            let mut skin = extras.skin.as_ref().expect("skin").unpack(4).expect("unpack");
            skin.eight_influences = true;
            assert_eq!(cpu.skin.as_ref(), Some(&skin));
            assert_eq!(skin.influences_per_vertex(), 8);
            assert_eq!(
                skin.weights1[2],
                [10000, 10000, 5000, 535],
                "set 1 kept as stored"
            );
            assert_eq!(
                cpu.morph_targets,
                extras
                    .morph_targets
                    .iter()
                    .map(|t| t.unpack(4).expect("unpack"))
                    .collect::<Vec<_>>()
            );
            assert_eq!(cpu.morph_targets[0].position_deltas[0], [0.5, 0.25, -0.125]);
            assert_eq!(cpu.morph_targets[1].name, "blink");
            assert!(cpu.validate().is_ok());
        }
    }

    /// A morph-only mesh (no skin) writes a block with `skin: None`; a four-influence skin keeps
    /// `eight_influences` false; an empty block writes nothing (the bytes of a block-less mesh).
    #[test]
    fn morph_only_four_influence_and_empty_blocks() {
        let (info, vertices, indices) = quad();
        let full = skin_and_morphs();
        let parts = |extras: Option<&GpuMeshExtras>| {
            encode_gpu_mesh_parts(
                &GpuMeshParts {
                    info: &info,
                    submeshes: &[],
                    vertices: &vertices,
                    indices: &indices,
                    extras,
                },
                0,
            )
        };
        let morph_only = GpuMeshExtras {
            skin: None,
            skin_influences: crate::cooked::DEFAULT_SKIN_INFLUENCES,
            morph_targets: full.morph_targets.clone(),
        };
        let bytes = parts(Some(&morph_only)).expect("encode");
        let cpu = GpuMeshView::parse(&bytes)
            .expect("parse")
            .to_cpu_mesh()
            .expect("cpu");
        assert!(cpu.skin.is_none());
        assert_eq!(cpu.morph_targets.len(), 2);

        let four = crate::CpuSkin {
            joints1: Vec::new(),
            weights1: Vec::new(),
            eight_influences: false,
            weights: vec![[65535, 0, 0, 0]; 4],
            ..full.skin.as_ref().expect("skin").unpack(4).expect("unpack")
        };
        let four_block = GpuMeshExtras {
            skin: Some(four.pack(4).expect("pack")),
            skin_influences: crate::cooked::DEFAULT_SKIN_INFLUENCES,
            morph_targets: Vec::new(),
        };
        let bytes = parts(Some(&four_block)).expect("encode");
        let cpu = GpuMeshView::parse(&bytes)
            .expect("parse")
            .to_cpu_mesh()
            .expect("cpu");
        assert_eq!(
            cpu.skin
                .as_ref()
                .map(|s| (s.eight_influences, s.influences_per_vertex())),
            Some((false, 4))
        );

        let none = parts(None).expect("encode");
        assert_eq!(parts(Some(&GpuMeshExtras::default())).expect("encode"), none);
        assert!(GpuMeshView::parse(&none).expect("parse").extras.is_none());
    }

    /// ADR-10080 acceptance: the frozen version 2 corpus payload reads as before (no block, same
    /// vertices, LODs and submeshes), and written again it is the same bytes but the version word,
    /// so a v2 pak entry behaves exactly as it did.
    #[test]
    fn a_version_2_payload_reads_unchanged_and_rewrites_to_the_same_bytes() {
        let v2: &[u8] = include_bytes!("../../regia_agent/tests/fixtures/format-corpus/gpu-mesh/v2.bin");
        assert_eq!(u32_at(v2, 4), 2);
        let view = GpuMeshView::parse(v2).expect("v2 reads");
        assert!(view.extras.is_none());
        let indices: Vec<u32> = view
            .indices
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();
        let again = encode_gpu_mesh_parts(
            &GpuMeshParts {
                info: &view.info,
                submeshes: &view.submeshes,
                vertices: view.vertices,
                indices: &indices,
                extras: view.extras.as_ref(),
            },
            0,
        )
        .expect("encode");
        let mut expected = v2.to_vec();
        expected[4..8].copy_from_slice(&GPU_MESH_VERSION.to_le_bytes());
        assert_eq!(again, expected, "only the version word moves");
        let cpu_v2 = view.to_cpu_mesh().expect("cpu");
        let cpu_v3 = GpuMeshView::parse(&again)
            .expect("v3")
            .to_cpu_mesh()
            .expect("cpu");
        assert_eq!(cpu_v2, cpu_v3);
        assert!(cpu_v2.skin.is_none() && cpu_v2.morph_targets.is_empty());
    }

    /// A version 1 or 2 payload must still end after its indices; a version 3 trailing block that
    /// is truncated, has a wrong magic or length, extra bytes, a joint index past the names or an
    /// eight-influence opt-in without a second set is refused, never a panic.
    #[test]
    fn hostile_trailing_blocks_are_refused() {
        let (info, vertices, indices) = quad();
        let extras = skin_and_morphs();
        let good = encode_gpu_mesh_parts(
            &GpuMeshParts {
                info: &info,
                submeshes: &[],
                vertices: &vertices,
                indices: &indices,
                extras: Some(&extras),
            },
            0,
        )
        .expect("encode");
        let plain_len = encode_gpu_mesh(&info, &vertices, &indices, 0)
            .expect("encode")
            .len();
        assert!(GpuMeshView::parse(&good).is_ok());
        // A block under an older version word is the old "trailing bytes" refusal.
        for old in [1u8, 2] {
            let mut b = good.clone();
            b[4] = old;
            assert!(
                matches!(GpuMeshView::parse(&b), Err(GpuMeshError::Malformed(_))),
                "v{old}"
            );
        }
        for len in plain_len + 1..good.len() {
            assert!(
                matches!(GpuMeshView::parse(&good[..len]), Err(GpuMeshError::Malformed(_))),
                "truncated to {len}"
            );
        }
        let mut magic = good.clone();
        magic[plain_len] = b'X';
        assert!(GpuMeshView::parse(&magic).is_err());
        let mut long = good.clone();
        long.push(0);
        assert!(GpuMeshView::parse(&long).is_err(), "a byte past the block");
        let mut len = good.clone();
        len[plain_len + 4..plain_len + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(GpuMeshView::parse(&len).is_err());
        let encode = |x: &GpuMeshExtras| {
            let mut b = encode_gpu_mesh(&info, &vertices, &indices, 0).expect("encode");
            b.extend_from_slice(&x.encode().expect("block"));
            b
        };
        let mut joint = extras.clone();
        if let Some(skin) = joint.skin.as_mut() {
            skin.stream_joints0[0] = 9;
        }
        assert!(matches!(
            GpuMeshView::parse(&encode(&joint)),
            Err(GpuMeshError::Malformed(_))
        ));
        let mut eight = extras.clone();
        if let Some(skin) = eight.skin.as_mut() {
            skin.stream_joints1.clear();
            skin.stream_weights1.clear();
            skin.stream_weights0 = [65535u16, 0, 0, 0]
                .repeat(4)
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect();
        }
        assert!(matches!(
            GpuMeshView::parse(&encode(&eight)),
            Err(GpuMeshError::Malformed(_))
        ));
        let odd = GpuMeshExtras {
            skin_influences: 5,
            ..extras.clone()
        };
        assert!(GpuMeshView::parse(&encode(&odd)).is_err());
        // The writer refuses a block that does not validate instead of writing it.
        assert!(
            encode_gpu_mesh_parts(
                &GpuMeshParts {
                    info: &info,
                    submeshes: &[],
                    vertices: &vertices,
                    indices: &indices,
                    extras: Some(&joint),
                },
                0,
            )
            .is_err()
        );
    }
}
