// SPDX-License-Identifier: MPL-2.0
//! Cooked asset data types for the runtime.
//!
//! These types are produced by `regia_asset_pipeline` during the cook step and
//! loaded at runtime by the asset server. Every type supports postcard
//! round-trip serialization so it can be stored in pak files and the dev cache.
//!
//! # Validation
//!
//! `CpuMesh` carries structural invariants (positions, normals, UVs must have
//! the same vertex count; all indices must be in range). The [`CpuMesh::validate`]
//! method checks these. Pipeline functions call it before writing cooked data;
//! the runtime loader calls it after deserializing from a pak.

use crate::AssetId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// CpuMesh
// ---------------------------------------------------------------------------

/// CPU-side mesh data ready for GPU upload.
///
/// Vertex attributes are stored in separate arrays (structure-of-arrays layout).
/// All per-vertex arrays must have the same length. Triangles are indexed: every
/// three consecutive `u32` values in `indices` form one triangle.
///
/// The skin, colour and morph fields (cooked mesh format v2) come only from a [`CookedMesh`]:
/// they are not part of the legacy `CpuMesh` postcard payload ([`Self::to_postcard`] drops them,
/// [`Self::from_postcard`] leaves them empty). Build a mesh with `..Default::default()` to stay
/// source compatible when fields are added.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CpuMesh {
    /// Vertex positions in object space, metres.
    pub positions: Vec<[f32; 3]>,
    /// Per-vertex normals (unit-length or zero if no normal was computed).
    pub normals: Vec<[f32; 3]>,
    /// Per-vertex texture coordinates, channel 0.
    pub uvs: Vec<[f32; 2]>,
    /// Tangent xyz with handedness in w (±1). Empty when none were stored.
    ///
    /// The importer and a GPU-ready mesh ([`crate::gpu_mesh::GpuMeshView::to_cpu_mesh`]) store
    /// MikkTSpace tangents. [`CookedMesh::to_cpu_mesh`] does not rebuild them: the cooked stream
    /// keeps an octahedral normal and a tangent sign, and that expansion writes xyz `(1, 0, 0)`
    /// and w from the sign (`1.0` when the sign is ≥ 0, otherwise `-1.0`) for every vertex. The
    /// renderer rebuilds MikkTSpace when it draws a compact mesh and does not read this xyz.
    #[serde(default)]
    pub tangents: Vec<[f32; 4]>,
    /// Triangle indices. Length must be a multiple of 3.
    pub indices: Vec<u32>,
    /// Joint influences, `None` for a rigid mesh.
    #[serde(skip)]
    pub skin: Option<crate::CpuSkin>,
    /// Linear vertex colour (`COLOR_0`), RGBA. Empty when the source has none.
    #[serde(skip)]
    pub colors: Vec<[f32; 4]>,
    /// Sparse morph targets, in source order.
    #[serde(skip)]
    pub morph_targets: Vec<crate::CpuMorphTarget>,
}

/// Errors returned by [`CpuMesh::validate`] and [`mesh_from_heightfield`].
#[derive(Clone, Debug, Error)]
pub enum MeshValidationError {
    /// Positions, normals, UVs, and tangents must have consistent lengths.
    #[error("mismatched vertex-attribute lengths: positions={pos}, normals={nrm}, uvs={uv}, tangents={tan}")]
    MismatchedAttributeLengths {
        /// Number of position entries.
        pos: usize,
        /// Number of normal entries.
        nrm: usize,
        /// Number of UV entries.
        uv: usize,
        /// Number of tangent entries (0 or pos).
        tan: usize,
    },

    /// Every index must be < vertex count.
    #[error("index out of range: index={index} at offset {offset} (vertex count={vertex_count})")]
    IndexOutOfRange {
        /// The out-of-bounds index value.
        index: u32,
        /// Byte offset within the index buffer.
        offset: usize,
        /// Total number of vertices.
        vertex_count: usize,
    },

    /// The index buffer length must be a multiple of 3.
    #[error("index count is not a multiple of 3: {count} indices")]
    InvalidIndexCount {
        /// Number of indices found.
        count: usize,
    },

    /// Heightfield-to-mesh conversion parameters are invalid: the sample grid
    /// must have at least two samples along each axis and the world width must
    /// be finite and positive.
    #[error(
        "cannot mesh heightfield: grid is {w}x{h} (needs at least 2x2 samples), \
         world_width is {world_width} (must be finite and > 0)"
    )]
    InvalidHeightfieldParams {
        /// Heightfield width in samples.
        w: u32,
        /// Heightfield height in samples.
        h: u32,
        /// The rejected world width.
        world_width: f32,
    },

    /// The heightfield's sample buffer does not match its declared dimensions,
    /// so its samples cannot be safely indexed.
    #[error("heightfield samples length {actual} does not match dimensions {w}x{h} (expected {expected})")]
    MismatchedHeightfieldSamples {
        /// Declared width.
        w: u32,
        /// Declared height.
        h: u32,
        /// Expected count = w * h.
        expected: usize,
        /// Actual count.
        actual: usize,
    },

    /// The grid would have more vertices than `u32` indices can address.
    #[error("heightfield grid needs {vertex_count} vertices, more than u32 indices can address")]
    TooManyVertices {
        /// Number of grid vertices.
        vertex_count: usize,
    },

    /// A side of the heightfield is over [`HEIGHTFIELD_MAX_EDGE`] (meshing costs about 56 bytes
    /// per sample).
    #[error("cannot mesh heightfield: grid is {w}x{h}, a side over the {max}-sample limit")]
    HeightfieldTooLarge {
        /// Heightfield width in samples.
        w: u32,
        /// Heightfield height in samples.
        h: u32,
        /// [`HEIGHTFIELD_MAX_EDGE`].
        max: u32,
    },

    /// The skin's arrays do not match the vertex count, or a joint index is out of range.
    #[error("invalid skin: {reason}")]
    InvalidSkin {
        /// What is wrong.
        reason: String,
    },

    /// A morph target's index list or deltas are inconsistent.
    #[error("invalid morph target {name:?}: {reason}")]
    InvalidMorphTarget {
        /// Target name.
        name: String,
        /// What is wrong.
        reason: String,
    },

    /// The colour array is neither empty nor one entry per vertex.
    #[error("{colors} vertex colours for {vertices} vertices")]
    MismatchedColorCount {
        /// Colour entries.
        colors: usize,
        /// Vertex count.
        vertices: usize,
    },
}

impl CpuMesh {
    /// Validate the mesh invariants:
    ///
    /// - `positions`, `normals`, and `uvs` all have the same length.
    /// - Every index in `indices` is < `positions.len()`.
    /// - `indices.len()` is a multiple of 3.
    /// - The skin, colours and morph targets (when present) match the vertex count
    ///   ([`crate::CpuSkin::validate`], [`crate::CpuMorphTarget::validate`]).
    ///
    /// Returns `Ok(())` or the first violation found.
    pub fn validate(&self) -> Result<(), MeshValidationError> {
        let vc = self.positions.len();

        let tan = self.tangents.len();
        if self.normals.len() != vc || self.uvs.len() != vc || (tan != 0 && tan != vc) {
            return Err(MeshValidationError::MismatchedAttributeLengths {
                pos: vc,
                nrm: self.normals.len(),
                uv: self.uvs.len(),
                tan,
            });
        }

        if !self.indices.len().is_multiple_of(3) {
            return Err(MeshValidationError::InvalidIndexCount {
                count: self.indices.len(),
            });
        }

        for (byte_offset, &idx) in self.indices.iter().enumerate() {
            if idx as usize >= vc {
                return Err(MeshValidationError::IndexOutOfRange {
                    index: idx,
                    offset: byte_offset * 4,
                    vertex_count: vc,
                });
            }
        }

        if let Some(skin) = &self.skin {
            skin.validate(vc)?;
        }
        if !self.colors.is_empty() && self.colors.len() != vc {
            return Err(MeshValidationError::MismatchedColorCount {
                colors: self.colors.len(),
                vertices: vc,
            });
        }
        for target in &self.morph_targets {
            target.validate(vc)?;
        }

        Ok(())
    }

    /// Serialize to the legacy `CpuMesh` postcard payload.
    ///
    /// Lossy: [`Self::skin`], [`Self::colors`] and [`Self::morph_targets`] are not part of this
    /// payload, so a skinned, coloured or morphing mesh comes back from [`Self::from_postcard`]
    /// without them (and compares unequal). Persist such a mesh as a [`CookedMesh`]
    /// (`regia_asset_pipeline::pack_cooked_mesh`) instead.
    ///
    /// # Errors
    ///
    /// Returns a postcard error if serialization fails (only possible for
    /// extremely large meshes exceeding postcard's internal limits).
    pub fn to_postcard(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    /// Deserialize from postcard bytes and validate.
    ///
    /// # Errors
    ///
    /// Returns `PostcardError` if the bytes are not valid postcard, or
    /// `MeshValidationError` if the decoded mesh violates invariants.
    pub fn from_postcard(bytes: &[u8]) -> Result<Self, MeshLoadError> {
        let mesh: Self = postcard::from_bytes(bytes)?;
        mesh.validate()?;
        Ok(mesh)
    }
}

/// Combined error for [`CpuMesh::from_postcard`].
#[derive(Debug, Error)]
pub enum MeshLoadError {
    /// The bytes could not be decoded as postcard.
    #[error("postcard decode error: {0}")]
    Postcard(#[from] postcard::Error),
    /// The decoded mesh failed validation.
    #[error("mesh validation error: {0}")]
    Validation(#[from] MeshValidationError),
}

// ---------------------------------------------------------------------------
// CookedImage
// ---------------------------------------------------------------------------

/// A cooked 2D image in RGBA8 format (sRGB colour space, straight alpha).
///
/// This is the runtime representation produced by the PNG importer. It holds
/// raw pixel data ready for GPU upload; no further decoding is needed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookedImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Packed RGBA8 pixel data, row-major, top-to-bottom.
    /// Length must equal `width * height * 4`.
    pub rgba8: Vec<u8>,
}

/// Errors returned by [`CookedImage::validate`].
#[derive(Clone, Debug, Error)]
pub enum ImageValidationError {
    /// A side is zero (an image has at least one pixel; GPU uploads need non-zero extents).
    #[error("image dimensions {w}x{h} have a zero side")]
    ZeroSize {
        /// Declared width.
        w: u32,
        /// Declared height.
        h: u32,
    },
    /// The pixel buffer does not match the declared dimensions.
    #[error("rgba8 length {actual} does not match dimensions {w}x{h} (expected {expected})")]
    MismatchedPixelCount {
        /// Declared width.
        w: u32,
        /// Declared height.
        h: u32,
        /// Expected byte count = w * h * 4.
        expected: usize,
        /// Actual byte count.
        actual: usize,
    },
}

impl CookedImage {
    /// Validate that `rgba8.len() == width * height * 4`.
    ///
    /// The expected byte count is computed with saturating arithmetic so a hostile `width`/`height`
    /// pair (e.g. `u32::MAX`) cannot overflow and panic; it saturates to an unreachable count and
    /// falls through to the normal mismatch error.
    pub fn validate(&self) -> Result<(), ImageValidationError> {
        if self.width == 0 || self.height == 0 {
            return Err(ImageValidationError::ZeroSize {
                w: self.width,
                h: self.height,
            });
        }
        let expected = (self.width as usize)
            .saturating_mul(self.height as usize)
            .saturating_mul(4);
        let actual = self.rgba8.len();
        if actual != expected {
            return Err(ImageValidationError::MismatchedPixelCount {
                w: self.width,
                h: self.height,
                expected,
                actual,
            });
        }
        Ok(())
    }

    /// Serialize to postcard bytes.
    pub fn to_postcard(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    /// Deserialize from postcard bytes and validate.
    pub fn from_postcard(bytes: &[u8]) -> Result<Self, ImageLoadError> {
        let img: Self = postcard::from_bytes(bytes)?;
        img.validate()?;
        Ok(img)
    }
}

/// Combined error for [`CookedImage::from_postcard`].
#[derive(Debug, Error)]
pub enum ImageLoadError {
    /// The bytes could not be decoded as postcard.
    #[error("postcard decode error: {0}")]
    Postcard(#[from] postcard::Error),
    /// The decoded image failed validation.
    #[error("image validation error: {0}")]
    Validation(#[from] ImageValidationError),
}

// ---------------------------------------------------------------------------
// Heightfield
// ---------------------------------------------------------------------------

/// Longest side of a cooked single [`Heightfield`] file and of a heightfield meshed by
/// [`mesh_from_heightfield`], in samples: `2^14 + 1`, the usual `2^n + 1` terrain grid (16 km at
/// 1 m spacing). Larger terrains are tiled (`regia_terrain`'s tiled heightfield, which stitches an
/// in-memory `Heightfield` of up to 65 536 per side; [`Heightfield::validate`] does not apply this
/// cap). The PNG heightfield cook refuses a larger side, and so does the loader
/// ([`Heightfield::from_postcard`]) before it decodes any sample: meshing allocates about 56 bytes
/// per sample, and a sample takes one or two bytes of the file (threat-model finding 33, ADR-4922).
pub const HEIGHTFIELD_MAX_EDGE: u32 = 16_385;

/// A cooked heightfield from a 16-bit grayscale PNG.
///
/// Samples are row-major (top-to-bottom), where 0 represents the lowest point
/// and 65535 the highest. The physical height range is stored separately (not
/// in this type); this type only stores the normalised samples.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Heightfield {
    /// Width in sample columns.
    pub width: u32,
    /// Height in sample rows.
    pub height: u32,
    /// Row-major 16-bit samples, length = `width * height`.
    pub samples: Vec<u16>,
}

/// Errors returned by [`Heightfield::validate`].
#[derive(Clone, Debug, Error)]
pub enum HeightfieldValidationError {
    /// A side is zero (a heightfield has at least one sample).
    #[error("heightfield dimensions {w}x{h} have a zero side")]
    ZeroSize {
        /// Declared width.
        w: u32,
        /// Declared height.
        h: u32,
    },
    /// The sample buffer does not match the declared dimensions.
    #[error("samples length {actual} does not match dimensions {w}x{h} (expected {expected})")]
    MismatchedSampleCount {
        /// Declared width.
        w: u32,
        /// Declared height.
        h: u32,
        /// Expected count = w * h.
        expected: usize,
        /// Actual count.
        actual: usize,
    },
    /// A cooked heightfield file's side is over [`HEIGHTFIELD_MAX_EDGE`]
    /// ([`Heightfield::from_postcard`]).
    #[error("heightfield dimensions {w}x{h} have a side over the {max}-sample limit")]
    TooLarge {
        /// Declared width.
        w: u32,
        /// Declared height.
        h: u32,
        /// [`HEIGHTFIELD_MAX_EDGE`].
        max: u32,
    },
    /// The payload holds fewer bytes than its header's sample count (every sample takes at least
    /// one byte), so it cannot be a whole heightfield; refused before decoding.
    #[error("heightfield header declares {samples} samples but only {bytes} bytes follow")]
    PayloadTooShort {
        /// Samples the header declares.
        samples: u64,
        /// Bytes after the header.
        bytes: usize,
    },
}

/// Reads one postcard varint (LEB128, at most 10 bytes) from the front of `bytes`.
fn read_postcard_varint(bytes: &mut &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for (i, &byte) in bytes.iter().enumerate().take(10) {
        value |= u64::from(byte & 0x7f).checked_shl(7 * i as u32)?;
        if byte & 0x80 == 0 {
            *bytes = &bytes[i + 1..];
            return Some(value);
        }
    }
    None
}

impl Heightfield {
    /// Validate that no side is zero and that `samples.len() == width * height`. The
    /// [`HEIGHTFIELD_MAX_EDGE`] cap is the loader's and the mesher's, not this check's: a stitched
    /// tiled terrain is larger.
    ///
    /// The expected count is computed with saturating arithmetic (see [`CookedImage::validate`]) so a
    /// hostile dimension pair cannot overflow and panic.
    pub fn validate(&self) -> Result<(), HeightfieldValidationError> {
        if self.width == 0 || self.height == 0 {
            return Err(HeightfieldValidationError::ZeroSize {
                w: self.width,
                h: self.height,
            });
        }
        let expected = (self.width as usize).saturating_mul(self.height as usize);
        let actual = self.samples.len();
        if actual != expected {
            return Err(HeightfieldValidationError::MismatchedSampleCount {
                w: self.width,
                h: self.height,
                expected,
                actual,
            });
        }
        Ok(())
    }

    /// Serialize to postcard bytes.
    pub fn to_postcard(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    /// Deserialize from postcard bytes and validate. The header (`width`, `height`, sample count) is
    /// checked first, before any sample is decoded: each side within [`HEIGHTFIELD_MAX_EDGE`], the
    /// count equal to `width * height`, and at least one payload byte per declared sample
    /// (finding 33, ADR-4922). A header that does not decode is left to postcard's error.
    pub fn from_postcard(bytes: &[u8]) -> Result<Self, HeightfieldLoadError> {
        Self::check_postcard_header(bytes)?;
        let hf: Self = postcard::from_bytes(bytes)?;
        hf.validate()?;
        Ok(hf)
    }

    fn check_postcard_header(bytes: &[u8]) -> Result<(), HeightfieldValidationError> {
        let mut rest = bytes;
        let (Some(w), Some(h), Some(count)) = (
            read_postcard_varint(&mut rest),
            read_postcard_varint(&mut rest),
            read_postcard_varint(&mut rest),
        ) else {
            return Ok(());
        };
        let (Ok(w), Ok(h)) = (u32::try_from(w), u32::try_from(h)) else {
            return Ok(());
        };
        if w == 0 || h == 0 {
            return Err(HeightfieldValidationError::ZeroSize { w, h });
        }
        if w > HEIGHTFIELD_MAX_EDGE || h > HEIGHTFIELD_MAX_EDGE {
            return Err(HeightfieldValidationError::TooLarge {
                w,
                h,
                max: HEIGHTFIELD_MAX_EDGE,
            });
        }
        let expected = u64::from(w) * u64::from(h);
        if count != expected {
            return Err(HeightfieldValidationError::MismatchedSampleCount {
                w,
                h,
                expected: expected as usize,
                actual: usize::try_from(count).unwrap_or(usize::MAX),
            });
        }
        if (rest.len() as u64) < count {
            return Err(HeightfieldValidationError::PayloadTooShort {
                samples: count,
                bytes: rest.len(),
            });
        }
        Ok(())
    }
}

/// Combined error for [`Heightfield::from_postcard`].
#[derive(Debug, Error)]
pub enum HeightfieldLoadError {
    /// The bytes could not be decoded as postcard.
    #[error("postcard decode error: {0}")]
    Postcard(#[from] postcard::Error),
    /// The decoded heightfield failed validation.
    #[error("heightfield validation error: {0}")]
    Validation(#[from] HeightfieldValidationError),
}

// ---------------------------------------------------------------------------
// Heightfield to CpuMesh
// ---------------------------------------------------------------------------

/// Build a regular-grid [`CpuMesh`] from a [`Heightfield`].
///
/// One vertex per sample, in row-major order matching `samples`. Vertex
/// `(col, row)` (with `col` in `0..width`, `row` in `0..height`) is placed at
///
/// - `x = col / (width - 1) * world_width - world_width / 2`
/// - `z = row / (height - 1) * world_width - world_width / 2`
/// - `y = sample / 65535 * height_scale + height_offset`
///
/// and carries UV `(col / (width - 1), row / (height - 1))`. The mesh is
/// centred on the origin and spans `[-world_width / 2, +world_width / 2]` on
/// both X and Z: row 0 is the `-Z` edge, column 0 the `-X` edge.
///
/// Each grid cell is split into two triangles wound counter-clockwise when
/// viewed from above (+Y), so the surface faces up. Vertex normals are
/// area-weighted: every triangle contributes its unnormalised cross product
/// (whose length is twice the triangle area) to each of its three vertices,
/// and the accumulated sums are then normalised.
///
/// # Errors
///
/// - [`MeshValidationError::InvalidHeightfieldParams`] if the grid has fewer
///   than two samples along either axis, `world_width` is not finite and
///   positive, or `height_scale` / `height_offset` is not finite.
/// - [`MeshValidationError::MismatchedHeightfieldSamples`] if
///   `samples.len()` does not match `width * height`.
/// - [`MeshValidationError::HeightfieldTooLarge`] if a side is over
///   [`HEIGHTFIELD_MAX_EDGE`].
/// - [`MeshValidationError::TooManyVertices`] if the grid has more vertices
///   than `u32` indices can address.
///
/// The returned mesh is [`CpuMesh::validate`]d before being returned.
pub fn mesh_from_heightfield(
    field: &Heightfield,
    world_width: f32,
    height_scale: f32,
    height_offset: f32,
) -> Result<CpuMesh, MeshValidationError> {
    let (w, h) = (field.width, field.height);
    let world_width_ok = world_width.is_finite() && world_width > 0.0;
    if w < 2 || h < 2 || !world_width_ok || !height_scale.is_finite() || !height_offset.is_finite() {
        return Err(MeshValidationError::InvalidHeightfieldParams { w, h, world_width });
    }
    if w > HEIGHTFIELD_MAX_EDGE || h > HEIGHTFIELD_MAX_EDGE {
        return Err(MeshValidationError::HeightfieldTooLarge {
            w,
            h,
            max: HEIGHTFIELD_MAX_EDGE,
        });
    }

    // `u64` first: on a 32-bit `usize` a grid like 65536×65536 wraps the product to 0 and the
    // sample loop below panics on the mismatched buffer. `usize::MAX` stands in for a count a
    // 32-bit `usize` cannot represent.
    let vertex_count = u64::from(w) * u64::from(h);
    let Ok(vertex_count) = usize::try_from(vertex_count) else {
        return Err(MeshValidationError::TooManyVertices {
            vertex_count: usize::MAX,
        });
    };
    if field.samples.len() != vertex_count {
        return Err(MeshValidationError::MismatchedHeightfieldSamples {
            w,
            h,
            expected: vertex_count,
            actual: field.samples.len(),
        });
    }
    if vertex_count - 1 > u32::MAX as usize {
        return Err(MeshValidationError::TooManyVertices { vertex_count });
    }

    let cols = (w - 1) as f32;
    let rows = (h - 1) as f32;
    let half_width = world_width / 2.0;

    let mut positions = Vec::with_capacity(vertex_count);
    let mut uvs = Vec::with_capacity(vertex_count);
    // Start from zero vectors; the triangle pass accumulates into these.
    let mut normals = vec![[0.0f32; 3]; vertex_count];

    for row in 0..h {
        let row_base = row as usize * w as usize;
        for col in 0..w {
            let u = col as f32 / cols;
            let v = row as f32 / rows;
            let sample = field.samples[row_base + col as usize];
            let y = sample as f32 / 65535.0 * height_scale + height_offset;
            positions.push([u * world_width - half_width, y, v * world_width - half_width]);
            uvs.push([u, v]);
        }
    }

    // Two counter-clockwise triangles per cell, seen from above (+Y).
    let mut indices = Vec::with_capacity((w as usize - 1) * (h as usize - 1) * 6);
    for row in 0..(h - 1) {
        let row_base = row as usize * w as usize;
        for col in 0..(w - 1) {
            let v00 = (row_base + col as usize) as u32;
            let v01 = v00 + w;
            let v10 = v00 + 1;
            let v11 = v01 + 1;
            indices.extend_from_slice(&[v00, v01, v10, v10, v01, v11]);
        }
    }

    // Area-weighted normals: accumulate each triangle's unnormalised cross
    // product (length = twice its area) on its three vertices.
    for &[a, b, c] in indices.as_chunks::<3>().0 {
        let (a, b, c) = (a as usize, b as usize, c as usize);
        let (pa, pb, pc) = (positions[a], positions[b], positions[c]);
        let (ux, uy, uz) = (pb[0] - pa[0], pb[1] - pa[1], pb[2] - pa[2]);
        let (vx, vy, vz) = (pc[0] - pa[0], pc[1] - pa[1], pc[2] - pa[2]);
        let n = [uy * vz - uz * vy, uz * vx - ux * vz, ux * vy - uy * vx];
        for i in [a, b, c] {
            normals[i][0] += n[0];
            normals[i][1] += n[1];
            normals[i][2] += n[2];
        }
    }

    for acc in &mut normals {
        let len = (acc[0] * acc[0] + acc[1] * acc[1] + acc[2] * acc[2]).sqrt();
        if len.is_finite() && len > 0.0 {
            *acc = [acc[0] / len, acc[1] / len, acc[2] / len];
        } else {
            // Unreachable for well-formed grids (every face normal of an
            // up-facing grid has a positive Y component, so no sum can be
            // zero), but never divide by zero or emit NaN: fall back to
            // straight up.
            *acc = [0.0, 1.0, 0.0];
        }
    }

    let mesh = CpuMesh {
        positions,
        normals,
        uvs,
        tangents: Vec::new(),
        indices,
        ..Default::default()
    };
    mesh.validate()?;
    Ok(mesh)
}

// ---------------------------------------------------------------------------
// CookedTexture (KTX2 payload)
// ---------------------------------------------------------------------------

/// Texture usage for compression and sampling (`docs/specs/12-assets.md` § 7.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextureCookUsage {
    /// sRGB colour map (base colour, emissive).
    ColorSrgb,
    /// Linear colour data (ORM masks, data).
    ColorLinear,
    /// Tangent-space normal map.
    NormalMap,
    /// Single-channel mask.
    Mask,
    /// UI texture (often left uncompressed).
    Ui,
}

/// Cooked 2D texture: KTX2 container bytes (`docs/specs/12-assets.md` § 4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookedTexture {
    /// Pixel width of mip 0.
    pub width: u32,
    /// Pixel height of mip 0.
    pub height: u32,
    /// Intended sampling usage.
    pub usage: TextureCookUsage,
    /// Full KTX2 file bytes. Loaded from a pak, they are a view of the pak's own bytes (an entry
    /// stored uncompressed) or of the decompressed entry: never a copy. Dereferences to `[u8]`
    /// and builds from a `Vec<u8>` with `.into()`.
    pub ktx2: crate::pak::PakPayload,
}

/// [`CookedTexture`]'s wire layout with the KTX2 bytes borrowed from the input
/// ([`CookedTexture::from_postcard_shared`]). Postcard encodes a `Vec<u8>` and a borrowed byte
/// slice identically (a varint length, then the bytes).
#[derive(Deserialize)]
struct CookedTextureView<'a> {
    width: u32,
    height: u32,
    usage: TextureCookUsage,
    #[serde(borrow)]
    ktx2: &'a [u8],
}

/// Errors returned by [`CookedTexture::validate`].
#[derive(Clone, Debug, Error)]
pub enum TextureValidationError {
    /// Dimensions are zero.
    #[error("texture dimensions must be non-zero")]
    ZeroDimensions,

    /// KTX2 header could not be parsed.
    #[error("invalid ktx2 payload: {0}")]
    Ktx2(String),

    /// Empty payload.
    #[error("ktx2 payload is empty")]
    Empty,
}

/// Reject a KTX2 payload whose DFD `Basic` block carries a texel-block-dimension byte of `0xFF`
/// before [`ktx2::Reader::new`] parses it.
///
/// The `ktx2` crate (0.5) reads each of the four texel-block-dimension bytes and computes
/// `NonZeroU8::new(dim + 1)`, which panics when `dim == 0xFF` (`0xFF + 1` overflows `u8` to zero).
/// A `catch_unwind` cannot contain that panic under `panic = "abort"` (shipping), so the overflow
/// is rejected up front by reading the DFD section directly (Khronos KTX 2.0 / DFD 1.4 layout:
/// the header's `dfd_byte_offset`/`dfd_byte_length` point at a 4-byte total length followed by
/// blocks, each an 8-byte block header then its body; a `Basic` block — descriptor type 0 — stores
/// its four texel-block dimensions 4 bytes into the body).
///
/// Public so that every caller that hands KTX2 bytes of unknown origin to `ktx2::Reader::new` runs
/// the same check first (ADR-5869). `None` means the parser will not hit this overflow; it does not
/// mean the container is valid.
#[must_use]
pub fn hostile_ktx2_dfd(ktx2: &[u8]) -> Option<&'static str> {
    if ktx2.len() < ktx2::Header::LENGTH || !ktx2.starts_with(&ktx2::MAGIC) {
        // Not a KTX2 header the parser would reach; `Reader::new` reports it as an error, no panic.
        return None;
    }
    let dfd_offset = u32::from_le_bytes(ktx2[48..52].try_into().ok()?) as usize;
    let dfd_length = u32::from_le_bytes(ktx2[52..56].try_into().ok()?) as usize;
    let dfd = ktx2.get(dfd_offset..dfd_offset.checked_add(dfd_length)?)?;
    if dfd.len() < 4 {
        return None;
    }
    let mut pos = 4usize; // skip the 4-byte total DFD length
    while pos + 8 <= dfd.len() {
        let header = &dfd[pos..pos + 8];
        let descriptor_type = u32::from_le_bytes(header[0..4].try_into().ok()?) >> 17;
        let block_size = u16::from_le_bytes(header[6..8].try_into().ok()?) as usize;
        if block_size < 8 {
            break;
        }
        // Basic block: the four texel-block dimensions sit 4 bytes into the body (after
        // color_model/color_primaries/transfer_function/flags).
        if descriptor_type == 0
            && let Some(dims) = dfd.get(pos + 8 + 4..pos + 8 + 8)
            && dims.contains(&0xFF)
        {
            return Some("ktx2 DFD texel-block dimension is 0xFF (would overflow the parser)");
        }
        pos += block_size;
        if pos > dfd.len() {
            break;
        }
    }
    None
}

impl CookedTexture {
    /// Validate dimensions and KTX2 container.
    pub fn validate(&self) -> Result<(), TextureValidationError> {
        if self.width == 0 || self.height == 0 {
            return Err(TextureValidationError::ZeroDimensions);
        }
        if self.ktx2.is_empty() {
            return Err(TextureValidationError::Empty);
        }
        // Reject the specific DFD overflow up front, before `ktx2::Reader::new` parses it. The
        // `catch_unwind` below is a belt-and-suspenders for any *other* parser panic in debug/test
        // builds, but it is a no-op under `panic = "abort"` (shipping), so the overflow itself must
        // be caught by this explicit check (M18 robustness).
        if let Some(reason) = hostile_ktx2_dfd(&self.ktx2) {
            return Err(TextureValidationError::Ktx2(reason.to_string()));
        }
        match std::panic::catch_unwind(|| ktx2::Reader::new(&self.ktx2)) {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(TextureValidationError::Ktx2(e.to_string())),
            Err(_) => Err(TextureValidationError::Ktx2(
                "ktx2 payload is malformed (parser panicked)".to_string(),
            )),
        }
    }

    /// Serialize to postcard bytes.
    pub fn to_postcard(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    /// Deserialize and validate.
    pub fn from_postcard(bytes: &[u8]) -> Result<Self, TextureLoadError> {
        let tex: Self = postcard::from_bytes(bytes)?;
        tex.validate()?;
        Ok(tex)
    }

    /// [`Self::from_postcard`] without copying the KTX2 bytes: [`Self::ktx2`] becomes a view of
    /// `bytes` (a mapped pak entry stays in the mapping).
    ///
    /// # Errors
    ///
    /// As [`Self::from_postcard`].
    pub fn from_postcard_shared(bytes: &crate::pak::PakPayload) -> Result<Self, TextureLoadError> {
        let view: CookedTextureView<'_> = postcard::from_bytes(bytes)?;
        // A borrowed field always lies inside its input; the copy is only a defensive fallback.
        let ktx2 = bytes
            .subslice(view.ktx2)
            .unwrap_or_else(|| crate::pak::PakPayload::from(view.ktx2));
        let tex = Self {
            width: view.width,
            height: view.height,
            usage: view.usage,
            ktx2,
        };
        tex.validate()?;
        Ok(tex)
    }
}

/// Combined error for [`CookedTexture::from_postcard`].
#[derive(Debug, Error)]
pub enum TextureLoadError {
    /// Postcard failure.
    #[error("postcard decode error: {0}")]
    Postcard(#[from] postcard::Error),
    /// Validation failure.
    #[error("texture validation error: {0}")]
    Validation(#[from] TextureValidationError),
}

// ---------------------------------------------------------------------------
// CookedMesh (packed vertex streams, § 7.2)
// ---------------------------------------------------------------------------

/// Index element width in the cooked mesh index buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CookedIndexWidth {
    /// 16-bit indices.
    U16,
    /// 32-bit indices.
    U32,
}

/// One LOD: an index sub-range and the minimum projected screen size at which it is chosen.
///
/// The first level whose [`Self::screen_relative_size`] is at most the projected size is
/// drawn (1 = the bounding diameter fills the viewport height). The last level is 0, the
/// fallback. Generated levels store the size at which the next coarser level's error is one
/// pixel on a 1080p display (ADR-208).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookedMeshLod {
    /// Minimum projected screen size for this LOD (larger = kept only when closer).
    pub screen_relative_size: f32,
    /// Byte offset into [`CookedMesh::indices`].
    pub index_byte_offset: u32,
    /// Number of index bytes for this LOD.
    pub index_byte_length: u32,
}

/// Submesh draw range (material slot index only in M4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookedSubmesh {
    /// Index byte offset into [`CookedMesh::indices`].
    pub index_byte_offset: u32,
    /// Index byte length.
    pub index_byte_length: u32,
    /// Material slot.
    pub material_slot: u32,
}

/// Cooked mesh with separate vertex streams (`docs/specs/12-assets.md` § 7.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookedMesh {
    /// Axis-aligned bounds (object space, metres).
    pub aabb_min: [f32; 3],
    /// Axis-aligned bounds max corner.
    pub aabb_max: [f32; 3],
    /// Bounding sphere: centre xyz + radius in w.
    pub bounding_sphere: [f32; 4],
    /// Stream 0: `f32×3` positions, tightly packed.
    pub stream_positions: Vec<u8>,
    /// Stream 1: `snorm16×4` octahedral normal + tangent sign per vertex.
    pub stream_normals_tangents: Vec<u8>,
    /// Stream 2: `f16×2` UV0 per vertex.
    pub stream_uv0: Vec<u8>,
    /// Index buffer (width given by [`CookedMesh::index_width`]).
    pub indices: Vec<u8>,
    /// Index element size.
    pub index_width: CookedIndexWidth,
    /// Vertex count (all streams).
    pub vertex_count: u32,
    /// Generated or authored LOD chain.
    #[serde(default)]
    pub lods: Vec<CookedMeshLod>,
    /// Submesh ranges.
    #[serde(default)]
    pub submeshes: Vec<CookedSubmesh>,
    /// Which rule produced [`Self::lods`]' screen sizes. See [`crate::LOD_METRIC_LEGACY`].
    ///
    /// Appended last so a payload from before this field fails to decode and is read as the
    /// previous layout. Postcard is not self-describing; `serde(default)` would not fill it.
    pub lod_metric: u32,
    /// Payload format version ([`crate::COOKED_MESH_FORMAT_VERSION`]). Fields from here on are
    /// v2 (ADR-226); a v1 payload ends at [`Self::lod_metric`] and loads with them empty.
    pub format_version: u32,
    /// Skin streams (joints and weights), `None` for a rigid mesh.
    pub skin: Option<crate::CookedSkin>,
    /// Stream 3: `COLOR_0` as linear `unorm16x4` RGBA per vertex, empty when the source has none.
    pub stream_color0: Vec<u8>,
    /// Sparse morph targets, in source order.
    pub morph_targets: Vec<crate::CookedMorphTarget>,
    /// Joint influences per vertex the renderer draws (v3, ADR-1622): 4, the default (the
    /// vertex-shader path's four strongest), or 8, the opt-in (importer setting
    /// `skin_influences: 8`): both influence sets on the compute skinning path. 8 needs a skin
    /// with a second influence set. A v1 or v2 payload loads as 4, which is how it drew.
    pub skin_influences: u8,
}

/// [`CookedMesh::skin_influences`] by default, and for every v1 and v2 payload.
pub const DEFAULT_SKIN_INFLUENCES: u8 = 4;

/// [`CookedMesh::skin_influences`] of a mesh that opted into eight influences.
pub const EIGHT_SKIN_INFLUENCES: u8 = 8;

/// [`CookedMesh`] format version 2 (ADR-226): the current layout without `skin_influences`.
#[derive(Deserialize, Serialize)]
struct CookedMeshV2 {
    aabb_min: [f32; 3],
    aabb_max: [f32; 3],
    bounding_sphere: [f32; 4],
    stream_positions: Vec<u8>,
    stream_normals_tangents: Vec<u8>,
    stream_uv0: Vec<u8>,
    indices: Vec<u8>,
    index_width: CookedIndexWidth,
    vertex_count: u32,
    lods: Vec<CookedMeshLod>,
    submeshes: Vec<CookedSubmesh>,
    lod_metric: u32,
    format_version: u32,
    skin: Option<crate::CookedSkin>,
    stream_color0: Vec<u8>,
    morph_targets: Vec<crate::CookedMorphTarget>,
}

impl CookedMeshV2 {
    fn into_current(self) -> CookedMesh {
        CookedMesh {
            aabb_min: self.aabb_min,
            aabb_max: self.aabb_max,
            bounding_sphere: self.bounding_sphere,
            stream_positions: self.stream_positions,
            stream_normals_tangents: self.stream_normals_tangents,
            stream_uv0: self.stream_uv0,
            indices: self.indices,
            index_width: self.index_width,
            vertex_count: self.vertex_count,
            lods: self.lods,
            submeshes: self.submeshes,
            lod_metric: self.lod_metric,
            format_version: crate::COOKED_MESH_FORMAT_VERSION,
            skin: self.skin,
            stream_color0: self.stream_color0,
            morph_targets: self.morph_targets,
            skin_influences: DEFAULT_SKIN_INFLUENCES,
        }
    }
}

/// [`CookedMesh`] format version 1 with `lod_metric` (importer 1.7.0).
#[derive(Deserialize, Serialize)]
struct CookedMeshV1 {
    aabb_min: [f32; 3],
    aabb_max: [f32; 3],
    bounding_sphere: [f32; 4],
    stream_positions: Vec<u8>,
    stream_normals_tangents: Vec<u8>,
    stream_uv0: Vec<u8>,
    indices: Vec<u8>,
    index_width: CookedIndexWidth,
    vertex_count: u32,
    lods: Vec<CookedMeshLod>,
    submeshes: Vec<CookedSubmesh>,
    lod_metric: u32,
}

impl CookedMeshV1 {
    fn into_current(self) -> CookedMesh {
        CookedMesh {
            aabb_min: self.aabb_min,
            aabb_max: self.aabb_max,
            bounding_sphere: self.bounding_sphere,
            stream_positions: self.stream_positions,
            stream_normals_tangents: self.stream_normals_tangents,
            stream_uv0: self.stream_uv0,
            indices: self.indices,
            index_width: self.index_width,
            vertex_count: self.vertex_count,
            lods: self.lods,
            submeshes: self.submeshes,
            lod_metric: self.lod_metric,
            format_version: crate::COOKED_MESH_FORMAT_VERSION,
            skin: None,
            stream_color0: Vec::new(),
            morph_targets: Vec::new(),
            skin_influences: DEFAULT_SKIN_INFLUENCES,
        }
    }
}

/// [`CookedMesh`] as written before `lod_metric` (importer builds through 1.6.0; format v1).
#[derive(Deserialize)]
struct CookedMeshPreLodMetric {
    aabb_min: [f32; 3],
    aabb_max: [f32; 3],
    bounding_sphere: [f32; 4],
    stream_positions: Vec<u8>,
    stream_normals_tangents: Vec<u8>,
    stream_uv0: Vec<u8>,
    indices: Vec<u8>,
    index_width: CookedIndexWidth,
    vertex_count: u32,
    #[serde(default)]
    lods: Vec<CookedMeshLod>,
    #[serde(default)]
    submeshes: Vec<CookedSubmesh>,
}

impl CookedMeshPreLodMetric {
    fn into_current(self) -> CookedMesh {
        CookedMeshV1 {
            aabb_min: self.aabb_min,
            aabb_max: self.aabb_max,
            bounding_sphere: self.bounding_sphere,
            stream_positions: self.stream_positions,
            stream_normals_tangents: self.stream_normals_tangents,
            stream_uv0: self.stream_uv0,
            indices: self.indices,
            index_width: self.index_width,
            vertex_count: self.vertex_count,
            lods: self.lods,
            submeshes: self.submeshes,
            lod_metric: crate::LOD_METRIC_LEGACY,
        }
        .into_current()
    }
}

/// Errors from [`CookedMesh::validate`] / decode.
#[derive(Clone, Debug, Error)]
pub enum CookedMeshValidationError {
    /// No vertices.
    #[error("cooked mesh has zero vertices")]
    Empty,

    /// Stream lengths do not match vertex count.
    #[error("vertex stream size mismatch (vertices={vertices})")]
    StreamSize {
        /// Declared vertices.
        vertices: u32,
    },

    /// Index buffer not aligned to index width.
    #[error("index buffer length {len} is not a multiple of index width {width}")]
    IndexAlignment {
        /// Byte length.
        len: usize,
        /// 2 or 4.
        width: usize,
    },

    /// A LOD's index count is not a whole number of triangles.
    #[error("LOD {lod} has {count} indices, not a multiple of 3")]
    NotTriangles {
        /// LOD index within the mesh.
        lod: usize,
        /// Index count.
        count: u32,
    },

    /// The skin streams do not match the vertex count or joint list.
    #[error("cooked skin: {reason}")]
    Skin {
        /// What is wrong.
        reason: String,
    },

    /// A morph target's streams do not match its index list, or an index is invalid.
    #[error("cooked morph target {name:?}: {reason}")]
    Morph {
        /// Target name.
        name: String,
        /// What is wrong.
        reason: String,
    },

    /// The colour stream is neither empty nor `unorm16x4` per vertex.
    #[error("colour stream is {len} bytes for {vertices} vertices")]
    ColorStream {
        /// Byte length.
        len: usize,
        /// Declared vertices.
        vertices: u32,
    },

    /// [`CookedMesh::skin_influences`] is neither 4 nor 8, or is 8 without a skin that has a
    /// second influence set (ADR-1622).
    #[error("skin_influences {value}: {reason}")]
    SkinInfluences {
        /// The stored value.
        value: u8,
        /// What is wrong.
        reason: &'static str,
    },
}

impl CookedMesh {
    /// Validate packed layout invariants.
    pub fn validate(&self) -> Result<(), CookedMeshValidationError> {
        if self.vertex_count == 0 {
            return Err(CookedMeshValidationError::Empty);
        }
        let pos_bytes = self.vertex_count as usize * 12;
        let nt_bytes = self.vertex_count as usize * 8;
        let uv_bytes = self.vertex_count as usize * 4;
        if self.stream_positions.len() != pos_bytes
            || self.stream_normals_tangents.len() != nt_bytes
            || self.stream_uv0.len() != uv_bytes
        {
            return Err(CookedMeshValidationError::StreamSize {
                vertices: self.vertex_count,
            });
        }
        let idx_width = match self.index_width {
            CookedIndexWidth::U16 => 2,
            CookedIndexWidth::U32 => 4,
        };
        if !self.indices.len().is_multiple_of(idx_width) {
            return Err(CookedMeshValidationError::IndexAlignment {
                len: self.indices.len(),
                width: idx_width,
            });
        }
        let vc = self.vertex_count as usize;
        for lod in &self.lods {
            let end = u64::from(lod.index_byte_offset) + u64::from(lod.index_byte_length);
            if end > self.indices.len() as u64 {
                return Err(CookedMeshValidationError::StreamSize {
                    vertices: self.vertex_count,
                });
            }
            // A range that starts or ends inside an index would decode indices made of the
            // bytes of two (audit wave 5, stream H5).
            for edge in [lod.index_byte_offset, lod.index_byte_length] {
                if !(edge as usize).is_multiple_of(idx_width) {
                    return Err(CookedMeshValidationError::IndexAlignment {
                        len: edge as usize,
                        width: idx_width,
                    });
                }
            }
        }
        // Submesh ranges obey the same bounds and alignment as LOD ranges (audit-v035-cook L2):
        // a consumer slicing `indices` by a validated submesh must not run past it.
        for sub in &self.submeshes {
            let end = u64::from(sub.index_byte_offset) + u64::from(sub.index_byte_length);
            if end > self.indices.len() as u64 {
                return Err(CookedMeshValidationError::StreamSize {
                    vertices: self.vertex_count,
                });
            }
            for edge in [sub.index_byte_offset, sub.index_byte_length] {
                if !(edge as usize).is_multiple_of(idx_width) {
                    return Err(CookedMeshValidationError::IndexAlignment {
                        len: edge as usize,
                        width: idx_width,
                    });
                }
            }
        }
        if let Some(skin) = &self.skin {
            skin.validate(vc)?;
        }
        if !self.stream_color0.is_empty() && self.stream_color0.len() != vc * 8 {
            return Err(CookedMeshValidationError::ColorStream {
                len: self.stream_color0.len(),
                vertices: self.vertex_count,
            });
        }
        for target in &self.morph_targets {
            target.validate(vc)?;
        }
        match self.skin_influences {
            DEFAULT_SKIN_INFLUENCES => {}
            EIGHT_SKIN_INFLUENCES => {
                if !self
                    .skin
                    .as_ref()
                    .is_some_and(|skin| !skin.stream_joints1.is_empty())
                {
                    return Err(CookedMeshValidationError::SkinInfluences {
                        value: self.skin_influences,
                        reason: "eight influences need a skin with a second influence set",
                    });
                }
            }
            value => {
                return Err(CookedMeshValidationError::SkinInfluences {
                    value,
                    reason: "must be 4 or 8",
                });
            }
        }
        Ok(())
    }

    /// Expand to [`CpuMesh`] for runtime upload and agent preview.
    ///
    /// Tangents are not MikkTSpace. Each vertex's xyz is `(1, 0, 0)` and w is the stored tangent
    /// sign (`1.0` when the sign is ≥ 0, otherwise `-1.0`), including when UV0 runs along +Y.
    /// UV0 is the two `f16` values in [`Self::stream_uv0`].
    pub fn to_cpu_mesh(&self) -> Result<CpuMesh, CookedMeshValidationError> {
        self.validate()?;
        let vc = self.vertex_count as usize;
        let mut positions = Vec::with_capacity(vc);
        for i in 0..vc {
            let o = i * 12;
            let p = [
                f32::from_le_bytes(le_chunk::<4>(&self.stream_positions, o)?),
                f32::from_le_bytes(le_chunk::<4>(&self.stream_positions, o + 4)?),
                f32::from_le_bytes(le_chunk::<4>(&self.stream_positions, o + 8)?),
            ];
            positions.push(p);
        }
        let mut normals = Vec::with_capacity(vc);
        let mut tangents = Vec::with_capacity(vc);
        for i in 0..vc {
            let o = i * 8;
            let enc = i16::from_le_bytes(le_chunk::<2>(&self.stream_normals_tangents, o)?);
            let enc_b = i16::from_le_bytes(le_chunk::<2>(&self.stream_normals_tangents, o + 2)?);
            let sign = i16::from_le_bytes(le_chunk::<2>(&self.stream_normals_tangents, o + 6)?);
            let n = decode_octahedral_normal(enc, enc_b);
            normals.push(n);
            tangents.push([1.0, 0.0, 0.0, if sign >= 0 { 1.0 } else { -1.0 }]);
        }
        let mut uvs = Vec::with_capacity(vc);
        for i in 0..vc {
            let o = i * 4;
            let u =
                crate::mesh_skin::f16_bits_to_f32(u16::from_le_bytes(le_chunk::<2>(&self.stream_uv0, o)?));
            let v = crate::mesh_skin::f16_bits_to_f32(u16::from_le_bytes(le_chunk::<2>(
                &self.stream_uv0,
                o + 2,
            )?));
            uvs.push([u, v]);
        }
        // LOD 0 only: the generated LODs follow it in the same index buffer, and drawing the
        // whole buffer drew every LOD on top of each other.
        // `validate` guarantees every LOD range lies inside the buffer.
        let indices = if self.lods.is_empty() {
            decode_indices(&self.indices, self.index_width, vc)?
        } else {
            self.lod_indices(0)?
        };
        let mut skin = self.skin.as_ref().map(|s| s.unpack(vc)).transpose()?;
        if let Some(skin) = skin.as_mut() {
            skin.eight_influences = self.skin_influences == EIGHT_SKIN_INFLUENCES;
        }
        let morph_targets = self
            .morph_targets
            .iter()
            .map(|t| t.unpack(vc))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(CpuMesh {
            positions,
            normals,
            uvs,
            tangents,
            indices,
            skin,
            colors: crate::mesh_skin::unpack_colors_unorm16(&self.stream_color0),
            morph_targets,
        })
    }

    /// Triangle indices of LOD `lod` (LOD 0, or the whole buffer when no chain was stored).
    pub fn lod_indices(&self, lod: usize) -> Result<Vec<u32>, CookedMeshValidationError> {
        let bytes = if self.lods.is_empty() {
            if lod != 0 {
                return Err(CookedMeshValidationError::StreamSize {
                    vertices: self.vertex_count,
                });
            }
            self.indices.as_slice()
        } else {
            let level = self.lods.get(lod).ok_or(CookedMeshValidationError::StreamSize {
                vertices: self.vertex_count,
            })?;
            let start = level.index_byte_offset as usize;
            let end = start.saturating_add(level.index_byte_length as usize);
            self.indices
                .get(start..end)
                .ok_or(CookedMeshValidationError::StreamSize {
                    vertices: self.vertex_count,
                })?
        };
        decode_indices(bytes, self.index_width, self.vertex_count as usize)
    }

    /// Serialize to postcard bytes.
    pub fn to_postcard(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    /// Deserialize and validate.
    ///
    /// Reads every layout this format has had, newest first:
    ///
    /// - v3 ([`crate::COOKED_MESH_FORMAT_VERSION`]): `skin_influences` follows the morph targets
    ///   (ADR-1622). A larger `format_version` is rejected.
    /// - v2: `format_version` and the skin, colour and morph blocks follow `lod_metric`; loads
    ///   with `skin_influences` 4, as it drew.
    /// - v1 with `lod_metric` (importer 1.7.0): loads with no skin, colours or morph targets.
    /// - v1 without `lod_metric` (importer 1.6.0 and earlier): a multi-LOD chain then takes
    ///   [`crate::legacy_screen_sizes`] because the geometric error was not stored; a single LOD
    ///   is left unchanged.
    ///
    /// An older layout must consume the whole payload, so a damaged v2 or v3 payload is an
    /// error rather than an older mesh that silently lost its skin; a v3 payload with bytes
    /// after it is an error too.
    pub fn from_postcard(bytes: &[u8]) -> Result<Self, CookedMeshLoadError> {
        let mut mesh = match postcard::take_from_bytes::<Self>(bytes) {
            Ok((mesh, rest)) => {
                if mesh.format_version != crate::COOKED_MESH_FORMAT_VERSION {
                    return Err(CookedMeshLoadError::UnsupportedVersion {
                        found: mesh.format_version,
                        supported: crate::COOKED_MESH_FORMAT_VERSION,
                    });
                }
                // Like the older layouts, the current one must consume the whole payload.
                if !rest.is_empty() {
                    return Err(CookedMeshLoadError::TrailingBytes { extra: rest.len() });
                }
                mesh
            }
            Err(primary) => match whole::<CookedMeshV2>(bytes) {
                Some(v2) if v2.format_version == 2 => v2.into_current(),
                _ => match whole::<CookedMeshV1>(bytes) {
                    Some(v1) => v1.into_current(),
                    None => match whole::<CookedMeshPreLodMetric>(bytes) {
                        Some(old) => old.into_current(),
                        None => return Err(CookedMeshLoadError::Postcard(primary)),
                    },
                },
            },
        };
        if mesh.lod_metric == crate::LOD_METRIC_LEGACY {
            migrate_legacy_lod_sizes(&mut mesh);
        }
        mesh.validate()?;
        Ok(mesh)
    }

    /// The format version of a cooked mesh payload without validating its streams: 1 for
    /// either v1 layout, the stored `format_version` otherwise.
    pub fn payload_version(bytes: &[u8]) -> Result<u32, CookedMeshLoadError> {
        match postcard::take_from_bytes::<Self>(bytes) {
            Ok((mesh, _)) => Ok(mesh.format_version),
            Err(primary) => {
                if let Some(v2) = whole::<CookedMeshV2>(bytes) {
                    Ok(v2.format_version)
                } else if whole::<CookedMeshV1>(bytes).is_some()
                    || whole::<CookedMeshPreLodMetric>(bytes).is_some()
                {
                    Ok(1)
                } else {
                    Err(CookedMeshLoadError::Postcard(primary))
                }
            }
        }
    }

    /// Encodes the mesh in the v2 layout (ADR-226, as builds before ADR-1622 wrote it), dropping
    /// `skin_influences`. For migration tests and fixtures only.
    #[doc(hidden)]
    pub fn to_postcard_v2(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(&CookedMeshV2 {
            aabb_min: self.aabb_min,
            aabb_max: self.aabb_max,
            bounding_sphere: self.bounding_sphere,
            stream_positions: self.stream_positions.clone(),
            stream_normals_tangents: self.stream_normals_tangents.clone(),
            stream_uv0: self.stream_uv0.clone(),
            indices: self.indices.clone(),
            index_width: self.index_width,
            vertex_count: self.vertex_count,
            lods: self.lods.clone(),
            submeshes: self.submeshes.clone(),
            lod_metric: self.lod_metric,
            format_version: 2,
            skin: self.skin.clone(),
            stream_color0: self.stream_color0.clone(),
            morph_targets: self.morph_targets.clone(),
        })
    }

    /// Encodes the mesh in the v1 layout (with `lod_metric`, as importer 1.7.0 wrote it),
    /// dropping the v2 blocks. For migration tests and fixtures only.
    #[doc(hidden)]
    pub fn to_postcard_v1(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(&CookedMeshV1 {
            aabb_min: self.aabb_min,
            aabb_max: self.aabb_max,
            bounding_sphere: self.bounding_sphere,
            stream_positions: self.stream_positions.clone(),
            stream_normals_tangents: self.stream_normals_tangents.clone(),
            stream_uv0: self.stream_uv0.clone(),
            indices: self.indices.clone(),
            index_width: self.index_width,
            vertex_count: self.vertex_count,
            lods: self.lods.clone(),
            submeshes: self.submeshes.clone(),
            lod_metric: self.lod_metric,
        })
    }
}

/// Decodes `bytes` as `T` only when `T` consumes all of them.
fn whole<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Option<T> {
    match postcard::take_from_bytes::<T>(bytes) {
        Ok((value, [])) => Some(value),
        _ => None,
    }
}

/// Rewrite a legacy multi-LOD chain onto the scatter default curve.
fn migrate_legacy_lod_sizes(mesh: &mut CookedMesh) {
    if mesh.lods.len() < 2 {
        return;
    }
    let sizes = crate::legacy_screen_sizes(mesh.lods.len());
    for (lod, size) in mesh.lods.iter_mut().zip(sizes) {
        lod.screen_relative_size = size;
    }
    mesh.lod_metric = crate::LOD_METRIC_MIGRATED;
}

/// Combined error for [`CookedMesh::from_postcard`].
#[derive(Debug, Error)]
pub enum CookedMeshLoadError {
    /// Postcard failure.
    #[error("postcard decode error: {0}")]
    Postcard(#[from] postcard::Error),
    /// Validation failure.
    #[error("cooked mesh validation error: {0}")]
    Validation(#[from] CookedMeshValidationError),
    /// A payload written by a newer engine.
    #[error("cooked mesh format version {found} (this build reads up to {supported})")]
    UnsupportedVersion {
        /// Version in the payload.
        found: u32,
        /// [`crate::COOKED_MESH_FORMAT_VERSION`].
        supported: u32,
    },
    /// Bytes after a current-version mesh: the payload is damaged.
    #[error("{extra} bytes after the cooked mesh")]
    TrailingBytes {
        /// How many bytes followed the mesh.
        extra: usize,
    },
}

fn le_chunk<const N: usize>(bytes: &[u8], off: usize) -> Result<[u8; N], CookedMeshValidationError> {
    bytes
        .get(off..off + N)
        .and_then(|s| s.try_into().ok())
        .ok_or(CookedMeshValidationError::StreamSize {
            vertices: (off / N).max(1) as u32,
        })
}

fn decode_indices(
    bytes: &[u8],
    width: CookedIndexWidth,
    vertex_count: usize,
) -> Result<Vec<u32>, CookedMeshValidationError> {
    let mut out = Vec::new();
    match width {
        CookedIndexWidth::U16 => {
            for chunk in bytes.as_chunks::<2>().0 {
                let idx = u16::from_le_bytes(*chunk) as u32;
                if idx as usize >= vertex_count {
                    return Err(CookedMeshValidationError::StreamSize {
                        vertices: vertex_count as u32,
                    });
                }
                out.push(idx);
            }
        }
        CookedIndexWidth::U32 => {
            for chunk in bytes.as_chunks::<4>().0 {
                let idx = u32::from_le_bytes(*chunk);
                if idx as usize >= vertex_count {
                    return Err(CookedMeshValidationError::StreamSize {
                        vertices: vertex_count as u32,
                    });
                }
                out.push(idx);
            }
        }
    }
    Ok(out)
}

/// Encode a unit normal to octahedral `snorm16` pair (for mesh cooking).
///
/// Reference: Cigolle et al., "A Survey of Efficient Representations for Independent Unit Vectors"
/// (JCGT 2014), octahedral encoding — same formulation as Khronos glTF sample code (MIT).
pub fn encode_octahedral_normal(n: [f32; 3]) -> (i16, i16) {
    let (x, y) = octahedral_encode_xy(n);
    let sx = (x.clamp(-1.0, 1.0) * 32767.0).round() as i32;
    let sy = (y.clamp(-1.0, 1.0) * 32767.0).round() as i32;
    (sx as i16, sy as i16)
}

fn octahedral_encode_xy(n: [f32; 3]) -> (f32, f32) {
    let l1 = n[0].abs() + n[1].abs() + n[2].abs();
    if l1 <= 1e-8 {
        return (0.0, 0.0);
    }
    let mut x = n[0] / l1;
    let mut y = n[1] / l1;
    let z = n[2] / l1;
    if z < 0.0 {
        let ox = x;
        let oy = y;
        x = (1.0 - oy.abs()) * sign_not_zero(ox);
        y = (1.0 - ox.abs()) * sign_not_zero(oy);
    }
    (x, y)
}

fn decode_octahedral_normal(ex: i16, ey: i16) -> [f32; 3] {
    let x = ex as f32 / 32767.0;
    let y = ey as f32 / 32767.0;
    let mut n = [x, y, 1.0 - x.abs() - y.abs()];
    if n[2] < 0.0 {
        n[0] = (1.0 - y.abs()) * sign_not_zero(x);
        n[1] = (1.0 - x.abs()) * sign_not_zero(y);
    }
    let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    if len > 1e-8 {
        [n[0] / len, n[1] / len, n[2] / len]
    } else {
        [0.0, 1.0, 0.0]
    }
}

fn sign_not_zero(v: f32) -> f32 {
    if v > 0.0 {
        1.0
    } else if v < 0.0 {
        -1.0
    } else {
        1.0
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // -- octahedral normals ------------------------------------------------

    fn normalized(v: [f32; 3]) -> [f32; 3] {
        let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        [v[0] / len, v[1] / len, v[2] / len]
    }

    #[test]
    fn octahedral_normals_round_trip_in_both_hemispheres() {
        let inputs = [
            [0.0, 1.0, 0.0],
            [0.0, -1.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.0, 0.0, -1.0],
            [1.0, 0.0, 0.0],
            [-1.0, 0.0, 0.0],
            normalized([1.0, 2.0, 3.0]),
            normalized([-1.0, 2.0, -3.0]),
            normalized([0.3, -0.8, -0.5]),
            normalized([-0.6, -0.1, -0.9]),
        ];
        for n in inputs {
            let (x, y) = encode_octahedral_normal(n);
            let d = decode_octahedral_normal(x, y);
            let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
            assert!(
                (len - 1.0).abs() < 1e-4,
                "{n:?} decodes to a unit vector, got {d:?}"
            );
            let dot = n[0] * d[0] + n[1] * d[1] + n[2] * d[2];
            assert!(dot > 0.999, "{n:?} -> ({x}, {y}) -> {d:?}, dot {dot}");
        }
    }

    #[test]
    fn octahedral_zero_vector_encodes_to_the_origin_and_decodes_to_a_unit_vector() {
        assert_eq!(encode_octahedral_normal([0.0, 0.0, 0.0]), (0, 0));
        let d = decode_octahedral_normal(0, 0);
        let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        assert!((len - 1.0).abs() < 1e-5, "{d:?}");
    }

    #[test]
    fn octahedral_encoding_scales_the_input_to_the_unit_diamond() {
        // An unnormalised input encodes the same as its direction.
        assert_eq!(
            encode_octahedral_normal([0.0, 0.0, 5.0]),
            encode_octahedral_normal([0.0, 0.0, 1.0])
        );
        // +Z is the center of the map; the extremes use the full snorm range.
        assert_eq!(encode_octahedral_normal([0.0, 0.0, 1.0]), (0, 0));
        assert_eq!(encode_octahedral_normal([1.0, 0.0, 0.0]), (32767, 0));
    }

    // -- CpuMesh -----------------------------------------------------------

    #[test]
    fn cpu_mesh_round_trip_postcard() {
        let mesh = CpuMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0], [0.0, 0.0, 1.0], [0.0, 0.0, 1.0]],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.5, 1.0]],
            tangents: Vec::new(),
            indices: vec![0, 1, 2],
            ..Default::default()
        };
        mesh.validate().expect("valid mesh");

        let bytes = mesh.to_postcard().expect("postcard encode");
        let mesh2 = CpuMesh::from_postcard(&bytes).expect("postcard decode");
        assert_eq!(mesh, mesh2);
    }

    #[test]
    fn cpu_mesh_rejects_mismatched_attributes() {
        let mesh = CpuMesh {
            positions: vec![[0.0; 3]; 3],
            normals: vec![[0.0; 3]; 2], // one short
            uvs: vec![[0.0; 2]; 3],
            tangents: Vec::new(),
            indices: vec![0, 1, 2],
            ..Default::default()
        };
        let err = mesh.validate().expect_err("should reject mismatched lengths");
        assert!(matches!(
            err,
            MeshValidationError::MismatchedAttributeLengths { .. }
        ));
    }

    #[test]
    fn cpu_mesh_rejects_index_out_of_range() {
        let mesh = CpuMesh {
            positions: vec![[0.0; 3]; 3],
            normals: vec![[0.0; 3]; 3],
            uvs: vec![[0.0; 2]; 3],
            tangents: Vec::new(),
            indices: vec![0, 1, 42], // 42 >= 3
            ..Default::default()
        };
        let err = mesh.validate().expect_err("should reject out-of-range index");
        assert!(matches!(err, MeshValidationError::IndexOutOfRange { .. }));
    }

    #[test]
    fn cpu_mesh_rejects_non_multiple_of_3_indices() {
        let mesh = CpuMesh {
            positions: vec![[0.0; 3]; 3],
            normals: vec![[0.0; 3]; 3],
            uvs: vec![[0.0; 2]; 3],
            tangents: Vec::new(),
            indices: vec![0, 1, 2, 3], // 4 indices
            ..Default::default()
        };
        let err = mesh.validate().expect_err("should reject non-triangle indices");
        assert!(matches!(err, MeshValidationError::InvalidIndexCount { .. }));
    }

    #[test]
    fn cpu_mesh_rejects_invalid_postcard() {
        let result = CpuMesh::from_postcard(b"not valid postcard");
        assert!(result.is_err());
    }

    // -- CookedImage -------------------------------------------------------

    #[test]
    fn cooked_image_round_trip_postcard() {
        let img = CookedImage {
            width: 2,
            height: 2,
            rgba8: vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 128, 128, 128, 255],
        };
        img.validate().expect("valid image");

        let bytes = img.to_postcard().expect("postcard encode");
        let img2 = CookedImage::from_postcard(&bytes).expect("postcard decode");
        assert_eq!(img, img2);
    }

    #[test]
    fn cooked_image_rejects_bad_dimensions() {
        let img = CookedImage {
            width: 2,
            height: 2,
            rgba8: vec![255; 3], // only 3 bytes
        };
        let err = img.validate().expect_err("should reject mismatched pixels");
        assert!(matches!(err, ImageValidationError::MismatchedPixelCount { .. }));
    }

    // -- Heightfield -------------------------------------------------------

    #[test]
    fn heightfield_round_trip_postcard() {
        let hf = Heightfield {
            width: 2,
            height: 2,
            samples: vec![0, 65535, 32768, 16384],
        };
        hf.validate().expect("valid heightfield");

        let bytes = hf.to_postcard().expect("postcard encode");
        let hf2 = Heightfield::from_postcard(&bytes).expect("postcard decode");
        assert_eq!(hf, hf2);
    }

    #[test]
    fn heightfield_rejects_bad_dimensions() {
        let hf = Heightfield {
            width: 2,
            height: 2,
            samples: vec![0; 3], // only 3 samples
        };
        let err = hf.validate().expect_err("should reject mismatched samples");
        assert!(matches!(
            err,
            HeightfieldValidationError::MismatchedSampleCount { .. }
        ));
    }

    // -- mesh_from_heightfield ---------------------------------------------

    #[test]
    fn heightfield_mesh_flat_2x2_grid() {
        let field = Heightfield {
            width: 2,
            height: 2,
            samples: vec![0, 0, 0, 0],
        };
        let mesh = mesh_from_heightfield(&field, 10.0, 8.0, 1.0).expect("valid parameters");
        mesh.validate().expect("mesh invariants");

        assert_eq!(mesh.positions.len(), 4);
        assert_eq!(mesh.indices.len(), 6);

        // Row-major: row 0 is the -Z edge, column 0 the -X edge; the corners
        // sit at +/-5 on both axes and every y is height_offset (samples are 0).
        assert_eq!(
            mesh.positions,
            vec![
                [-5.0, 1.0, -5.0],
                [5.0, 1.0, -5.0],
                [-5.0, 1.0, 5.0],
                [5.0, 1.0, 5.0],
            ]
        );
        for p in &mesh.positions {
            assert_eq!(p[1], 1.0);
        }

        // UVs span the full 0..1 range.
        assert_eq!(mesh.uvs, vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]]);

        // Two counter-clockwise triangles for the single cell.
        assert_eq!(mesh.indices, vec![0, 2, 1, 1, 2, 3]);

        // Flat field: every normal is straight up.
        assert_eq!(mesh.normals, vec![[0.0, 1.0, 0.0]; 4]);
    }

    #[test]
    fn heightfield_mesh_full_scale_corner_reaches_offset_plus_scale() {
        // The last sample (col=1, row=1) is at full scale.
        let field = Heightfield {
            width: 2,
            height: 2,
            samples: vec![0, 0, 0, 65535],
        };
        let mesh = mesh_from_heightfield(&field, 10.0, 8.0, 1.0).expect("valid parameters");

        assert_eq!(mesh.positions[3][1], 9.0); // height_offset + height_scale
        assert_eq!(mesh.positions[0][1], 1.0); // the other corners stay at the
        assert_eq!(mesh.positions[1][1], 1.0); // offset
        assert_eq!(mesh.positions[2][1], 1.0);
    }

    #[test]
    fn heightfield_mesh_rejects_1x1_grid() {
        let field = Heightfield {
            width: 1,
            height: 1,
            samples: vec![0],
        };
        let err =
            mesh_from_heightfield(&field, 10.0, 8.0, 0.0).expect_err("a 1x1 grid cannot form a triangle");
        assert!(matches!(
            err,
            MeshValidationError::InvalidHeightfieldParams { .. }
        ));
    }

    #[test]
    fn heightfield_mesh_rejects_thin_grid() {
        let field = Heightfield {
            width: 1,
            height: 5,
            samples: vec![0; 5],
        };
        let err = mesh_from_heightfield(&field, 10.0, 1.0, 0.0)
            .expect_err("a grid one sample wide cannot form a triangle");
        assert!(matches!(
            err,
            MeshValidationError::InvalidHeightfieldParams { .. }
        ));
    }

    #[test]
    fn heightfield_mesh_rejects_bad_world_width() {
        let field = Heightfield {
            width: 4,
            height: 4,
            samples: vec![0; 16],
        };
        for world_width in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let err = mesh_from_heightfield(&field, world_width, 1.0, 0.0)
                .expect_err("non-positive or non-finite world width is invalid");
            assert!(matches!(
                err,
                MeshValidationError::InvalidHeightfieldParams { .. }
            ));
        }
    }

    #[test]
    fn heightfield_mesh_rejects_non_finite_height_params() {
        let field = Heightfield {
            width: 4,
            height: 4,
            samples: vec![100; 16],
        };
        for (scale, offset) in [
            (f32::NAN, 0.0),
            (f32::INFINITY, 0.0),
            (1.0, f32::NAN),
            (1.0, f32::NEG_INFINITY),
        ] {
            let err =
                mesh_from_heightfield(&field, 10.0, scale, offset).expect_err("non-finite height parameter");
            assert!(matches!(
                err,
                MeshValidationError::InvalidHeightfieldParams { .. }
            ));
        }
    }

    #[test]
    fn heightfield_mesh_rejects_mismatched_sample_buffer() {
        let field = Heightfield {
            width: 3,
            height: 3,
            samples: vec![0; 5], // too short for 3x3
        };
        let err = mesh_from_heightfield(&field, 10.0, 1.0, 0.0)
            .expect_err("sample buffer must match the declared dimensions");
        assert!(matches!(
            err,
            MeshValidationError::MismatchedHeightfieldSamples { .. }
        ));
    }

    #[test]
    fn heightfield_mesh_rejects_a_grid_that_overflows_addressing() {
        // Audit (asset1): on a 32-bit `usize` (wasm32) `65536 * 65536` wrapped to 0 and the
        // sample loop panicked on the empty buffer. The product is checked now; on 64-bit the
        // empty buffer mismatches the real product instead. integ-v048c: integ-v048's side limit
        // (`HEIGHTFIELD_MAX_EDGE`) is checked before the product and refuses this grid first.
        let field = Heightfield {
            width: 65536,
            height: 65536,
            samples: Vec::new(),
        };
        let err = mesh_from_heightfield(&field, 1.0, 1.0, 0.0).expect_err("an overflowing grid");
        assert!(
            matches!(
                err,
                MeshValidationError::HeightfieldTooLarge { .. }
                    | MeshValidationError::TooManyVertices { .. }
                    | MeshValidationError::MismatchedHeightfieldSamples { .. }
            ),
            "{err}"
        );
    }

    #[test]
    fn heightfield_mesh_sloped_ramp_normals_and_winding() {
        // A ramp rising along +X (columns), constant along rows.
        let field = Heightfield {
            width: 3,
            height: 3,
            samples: vec![0, 32768, 65535, 0, 32768, 65535, 0, 32768, 65535],
        };
        let mesh = mesh_from_heightfield(&field, 10.0, 4.0, 0.0).expect("valid parameters");
        mesh.validate().expect("mesh invariants");
        assert_eq!(mesh.indices.len(), 6 * 2 * 2); // 4 cells, 2 triangles each

        // Every triangle is counter-clockwise seen from above: the Y component
        // of (b - a) x (c - a) must be positive.
        for &[a, b, c] in mesh.indices.as_chunks::<3>().0 {
            let ux = mesh.positions[b as usize][0] - mesh.positions[a as usize][0];
            let uz = mesh.positions[b as usize][2] - mesh.positions[a as usize][2];
            let vx = mesh.positions[c as usize][0] - mesh.positions[a as usize][0];
            let vz = mesh.positions[c as usize][2] - mesh.positions[a as usize][2];
            let ny = uz * vx - ux * vz;
            assert!(ny > 0.0, "triangle is not counter-clockwise from above");
        }

        // Area-weighted normals on an upslope tilt towards -X while pointing
        // generally up.
        for n in &mesh.normals {
            assert!(n[0].is_finite() && n[1].is_finite() && n[2].is_finite());
            assert!(n[1] > 0.0, "normal must point up");
            assert!(n[0] < 0.0, "ramp rising towards +X must tilt normals towards -X");
        }
    }

    // -- CookedTexture ------------------------------------------------------

    /// A minimal valid KTX2 blob (one 4×4 BC7 block) built with the ktx2 crate's own types, so
    /// [`CookedTexture::validate`] accepts it and the hostile-DFD test below can corrupt a known
    /// byte.
    pub(crate) fn minimal_ktx2() -> Vec<u8> {
        use ktx2::dfd::{Basic, Block};
        use ktx2::{Format, Header, Index, LevelIndex};
        let (basic, type_size) = Basic::from_format(Format::BC7_SRGB_BLOCK).expect("dfd");
        let dfd_body = Block::Basic(basic).to_vec();
        let dfd_total_len = 4u32 + dfd_body.len() as u32;
        let level_count = 1u32;
        let level_index_len = level_count as usize * LevelIndex::LENGTH;
        let dfd_offset = Header::LENGTH + level_index_len;
        let kvd_offset = dfd_offset + dfd_total_len as usize;
        let data_offset = kvd_offset;
        let payload: Vec<u8> = vec![0u8; 16]; // one 4x4 BC7 block
        let level_index = LevelIndex {
            byte_offset: data_offset as u64,
            byte_length: payload.len() as u64,
            uncompressed_byte_length: payload.len() as u64,
        };
        let header = Header {
            format: Some(Format::BC7_SRGB_BLOCK),
            type_size,
            pixel_width: 4,
            pixel_height: 4,
            pixel_depth: 0,
            layer_count: 0,
            face_count: 1,
            level_count,
            supercompression_scheme: None,
            index: Index {
                dfd_byte_offset: dfd_offset as u32,
                dfd_byte_length: dfd_total_len,
                kvd_byte_offset: kvd_offset as u32,
                kvd_byte_length: 0,
                sgd_byte_offset: 0,
                sgd_byte_length: 0,
            },
        };
        let mut out = Vec::new();
        out.extend_from_slice(&header.as_bytes());
        out.extend_from_slice(&level_index.as_bytes());
        out.extend_from_slice(&dfd_total_len.to_le_bytes());
        out.extend_from_slice(&dfd_body);
        out.extend_from_slice(&payload);
        out
    }

    #[test]
    fn cooked_texture_round_trip_postcard() {
        let texture = CookedTexture {
            width: 4,
            height: 4,
            usage: TextureCookUsage::ColorSrgb,
            ktx2: minimal_ktx2().into(),
        };
        texture.validate().expect("valid texture");

        let bytes = texture.to_postcard().expect("postcard encode");
        let tex2 = CookedTexture::from_postcard(&bytes).expect("postcard decode");
        assert_eq!(tex2, texture);
    }

    #[test]
    fn shared_decode_matches_the_copying_decode_and_borrows_the_input() {
        let texture = CookedTexture {
            width: 4,
            height: 4,
            usage: TextureCookUsage::NormalMap,
            ktx2: minimal_ktx2().into(),
        };
        let bytes = crate::pak::PakPayload::from(texture.to_postcard().expect("encode"));
        let shared = CookedTexture::from_postcard_shared(&bytes).expect("shared decode");
        assert_eq!(shared, CookedTexture::from_postcard(&bytes).expect("decode"));
        assert!(
            bytes.as_ptr_range().contains(&shared.ktx2.as_ptr()),
            "the KTX2 bytes are a view of the input, not a copy"
        );
        // Truncated input is an error, as for the copying decode.
        let short = bytes.slice(0..bytes.len() - 1).expect("slice");
        assert!(CookedTexture::from_postcard_shared(&short).is_err());
        assert!(CookedTexture::from_postcard(&short).is_err());
    }

    #[test]
    fn cooked_texture_rejects_a_hostile_ktx2_dfd_without_panicking() {
        // Corrupt the DFD Basic block's texel-block-dimension byte to 0xFF. The ktx2 crate (0.5)
        // computes `dim + 1` in `u8` and panics on overflow (`NonZeroU8::new(dim + 1)`); `validate`
        // must reject it *explicitly* (before any catch_unwind) so the check holds under
        // `panic = "abort"` (M18 robustness). The message must name the DFD dimension, so this
        // fails on a catch_unwind-only rejection (which reports "parser panicked").
        let mut bytes = minimal_ktx2();
        // Layout: 80-byte header, 24-byte level index, 4-byte DFD length, then the DFD body (one
        // Basic block = 8-byte block header + fields). The texel-block-dimension byte is the 5th
        // field byte, i.e. `BlockHeader::LENGTH + 4` into the DFD body.
        let dfd_body = ktx2::Header::LENGTH + ktx2::LevelIndex::LENGTH + 4;
        let texel_dim_byte = dfd_body + ktx2::dfd::BlockHeader::LENGTH + 4;
        bytes[texel_dim_byte] = 0xFF;
        let texture = CookedTexture {
            width: 4,
            height: 4,
            usage: TextureCookUsage::ColorSrgb,
            ktx2: bytes.into(),
        };
        let err = texture
            .validate()
            .expect_err("a hostile DFD must be rejected, not panic");
        let msg = err.to_string();
        assert!(
            msg.contains("texel-block dimension"),
            "the rejection must be the explicit DFD guard, got: {msg}"
        );
    }

    fn triangle_mesh(lods: Vec<CookedMeshLod>, lod_metric: u32) -> CookedMesh {
        let positions: Vec<u8> = [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        CookedMesh {
            aabb_min: [0.0; 3],
            aabb_max: [1.0; 3],
            bounding_sphere: [0.5, 0.5, 0.0, 1.0],
            stream_positions: positions,
            stream_normals_tangents: vec![0u8; 3 * 8],
            stream_uv0: vec![0u8; 3 * 4],
            indices: [0u16, 1, 2].iter().flat_map(|i| i.to_le_bytes()).collect(),
            index_width: CookedIndexWidth::U16,
            vertex_count: 3,
            lods,
            submeshes: vec![],
            lod_metric,
            format_version: crate::COOKED_MESH_FORMAT_VERSION,
            skin: None,
            stream_color0: Vec::new(),
            morph_targets: Vec::new(),
            skin_influences: DEFAULT_SKIN_INFLUENCES,
        }
    }

    /// Audit wave 5 (stream H5): a LOD range that starts or ends inside a 16-bit index is
    /// refused by validation, and so by loading; an aligned one passes.
    #[test]
    fn a_lod_range_inside_an_index_is_refused() {
        for (offset, length) in [(1, 4), (0, 3), (2, 3)] {
            let mesh = triangle_mesh(
                vec![CookedMeshLod {
                    screen_relative_size: 0.0,
                    index_byte_offset: offset,
                    index_byte_length: length,
                }],
                crate::LOD_METRIC_ERROR,
            );
            assert!(
                matches!(
                    mesh.validate(),
                    Err(CookedMeshValidationError::IndexAlignment { width: 2, .. })
                ),
                "{offset}+{length}"
            );
            let bytes = mesh.to_postcard().expect("encode");
            assert!(CookedMesh::from_postcard(&bytes).is_err(), "{offset}+{length}");
        }
        let aligned = triangle_mesh(
            vec![CookedMeshLod {
                screen_relative_size: 0.0,
                index_byte_offset: 0,
                index_byte_length: 6,
            }],
            crate::LOD_METRIC_ERROR,
        );
        assert!(aligned.validate().is_ok());
    }

    /// Audit wave 10c (stream H6): a current-layout mesh payload must consume every byte, like
    /// the older layouts already must, so bytes appended to a payload are an error instead of
    /// being ignored.
    #[test]
    fn a_current_mesh_payload_with_trailing_bytes_is_refused() {
        let mesh = triangle_mesh(
            vec![CookedMeshLod {
                screen_relative_size: 0.0,
                index_byte_offset: 0,
                index_byte_length: 6,
            }],
            crate::LOD_METRIC_ERROR,
        );
        let bytes = mesh.to_postcard().expect("encode");
        assert!(
            CookedMesh::from_postcard(&bytes).is_ok(),
            "the exact payload loads"
        );
        let extras: [&[u8]; 3] = [&[0], &[1, 2, 3, 4], &[0xff; 64]];
        for extra in extras {
            let mut longer = bytes.clone();
            longer.extend_from_slice(extra);
            assert!(
                CookedMesh::from_postcard(&longer).is_err(),
                "{} trailing bytes were ignored",
                extra.len()
            );
        }
    }

    #[test]
    fn out_of_range_lod_is_rejected_not_replaced_by_the_whole_buffer() {
        let mesh = triangle_mesh(
            vec![CookedMeshLod {
                screen_relative_size: 0.0,
                index_byte_offset: 100,
                index_byte_length: 6,
            }],
            crate::LOD_METRIC_ERROR,
        );
        assert!(mesh.validate().is_err());
        assert!(mesh.to_cpu_mesh().is_err());
        let mut long = mesh.clone();
        long.lods[0].index_byte_offset = 0;
        long.lods[0].index_byte_length = 8;
        assert!(long.validate().is_err());
        long.lods[0].index_byte_length = 6;
        assert!(long.to_cpu_mesh().is_ok());
    }

    #[test]
    fn to_cpu_mesh_decodes_subnormal_uvs_exactly() {
        let mut mesh = triangle_mesh(
            vec![CookedMeshLod {
                screen_relative_size: 1.0,
                index_byte_offset: 0,
                index_byte_length: 6,
            }],
            crate::LOD_METRIC_MIGRATED,
        );
        let halves = [0x0200u16, 0x8001, 0x03ff, 0, 0x3c00, 0x0001];
        mesh.stream_uv0 = halves.iter().flat_map(|h| h.to_le_bytes()).collect();
        let cpu = mesh.to_cpu_mesh().expect("unpack");
        let tiny = 2f32.powi(-24);
        assert_eq!(cpu.uvs[0], [512.0 * tiny, -tiny]);
        assert_eq!(cpu.uvs[1], [1023.0 * tiny, 0.0]);
        assert_eq!(cpu.uvs[2], [1.0, tiny]);
    }

    /// UV0 along object +Y would be a geometric tangent near `(0, 1, 0)`. The expansion still
    /// writes `(1, 0, 0)` and the stored sign, so the field docs cannot call that vector MikkTSpace.
    #[test]
    fn to_cpu_mesh_writes_plus_x_tangent_with_stored_sign() {
        let mut mesh = triangle_mesh(
            vec![CookedMeshLod {
                screen_relative_size: 1.0,
                index_byte_offset: 0,
                index_byte_length: 6,
            }],
            crate::LOD_METRIC_ERROR,
        );
        // Positions are (0,0,0), (1,0,0), (0,1,0). U increases on the +Y vertex.
        let uv = |u: u16, v: u16| {
            let mut bytes = [0u8; 4];
            bytes[..2].copy_from_slice(&u.to_le_bytes());
            bytes[2..].copy_from_slice(&v.to_le_bytes());
            bytes
        };
        let one = 0x3c00u16;
        mesh.stream_uv0 = [uv(0, 0), uv(0, one), uv(one, 0)].concat();
        mesh.stream_normals_tangents[6..8].copy_from_slice(&1i16.to_le_bytes());
        mesh.stream_normals_tangents[14..16].copy_from_slice(&(-1i16).to_le_bytes());
        let cpu = mesh.to_cpu_mesh().expect("unpack");
        assert_eq!(cpu.uvs[2][0], 1.0);
        assert_eq!(cpu.tangents.len(), 3);
        assert_eq!(cpu.tangents[0], [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(cpu.tangents[1], [1.0, 0.0, 0.0, -1.0]);
        assert_eq!(cpu.tangents[2], [1.0, 0.0, 0.0, 1.0]);
    }

    /// Stream 2 is four bytes a vertex. A two-submesh mesh has one sphere, not one per slot.
    #[test]
    fn stream_uv0_is_four_bytes_and_submeshes_have_no_bounds() {
        let mut wide = triangle_mesh(
            vec![CookedMeshLod {
                screen_relative_size: 0.0,
                index_byte_offset: 0,
                index_byte_length: 6,
            }],
            crate::LOD_METRIC_ERROR,
        );
        wide.stream_uv0 = vec![0u8; 3 * 8];
        assert!(matches!(
            wide.validate(),
            Err(CookedMeshValidationError::StreamSize { .. })
        ));

        let mut mesh = triangle_mesh(
            vec![CookedMeshLod {
                screen_relative_size: 0.0,
                index_byte_offset: 0,
                index_byte_length: 6,
            }],
            crate::LOD_METRIC_ERROR,
        );
        mesh.bounding_sphere = [0.25, 0.5, 0.0, 1.5];
        mesh.submeshes = vec![
            CookedSubmesh {
                index_byte_offset: 0,
                index_byte_length: 6,
                material_slot: 0,
            },
            CookedSubmesh {
                index_byte_offset: 0,
                index_byte_length: 6,
                material_slot: 3,
            },
        ];
        assert_eq!(mesh.stream_uv0.len(), mesh.vertex_count as usize * 4);
        let bytes = mesh.to_postcard().expect("encode");
        let loaded = CookedMesh::from_postcard(&bytes).expect("decode");
        assert_eq!(loaded.bounding_sphere, [0.25, 0.5, 0.0, 1.5]);
        assert_eq!(loaded.submeshes.len(), 2);
        assert_eq!(loaded.submeshes[1].material_slot, 3);
        assert_eq!((loaded.aabb_min, loaded.aabb_max), (mesh.aabb_min, mesh.aabb_max));
    }

    #[test]
    fn legacy_multi_lod_postcard_migrates_to_the_scatter_curve() {
        let mesh = triangle_mesh(
            vec![
                CookedMeshLod {
                    screen_relative_size: 1.0,
                    index_byte_offset: 0,
                    index_byte_length: 6,
                },
                CookedMeshLod {
                    screen_relative_size: 0.5,
                    index_byte_offset: 0,
                    index_byte_length: 6,
                },
            ],
            crate::LOD_METRIC_LEGACY,
        );
        let bytes = mesh.to_postcard_v1().expect("encode");
        // Drop the trailing lod_metric varint (0) so the bytes are a 1.6.0 payload.
        let v1 = &bytes[..bytes.len() - 1];
        let loaded = CookedMesh::from_postcard(v1).expect("v1 decode");
        assert_eq!(loaded.lod_metric, crate::LOD_METRIC_MIGRATED);
        assert_eq!(loaded.lods[0].screen_relative_size, 0.06);
        assert_eq!(loaded.lods[1].screen_relative_size, 0.0);
    }

    #[test]
    fn error_metric_thresholds_round_trip_unchanged() {
        let mesh = triangle_mesh(
            vec![CookedMeshLod {
                screen_relative_size: 0.42,
                index_byte_offset: 0,
                index_byte_length: 6,
            }],
            crate::LOD_METRIC_ERROR,
        );
        let bytes = mesh.to_postcard().expect("encode");
        let loaded = CookedMesh::from_postcard(&bytes).expect("decode");
        assert_eq!(loaded, mesh);
    }

    #[test]
    fn single_lod_legacy_payload_is_left_alone() {
        let mesh = triangle_mesh(
            vec![CookedMeshLod {
                screen_relative_size: 1.0,
                index_byte_offset: 0,
                index_byte_length: 6,
            }],
            crate::LOD_METRIC_LEGACY,
        );
        let bytes = mesh.to_postcard_v1().expect("encode");
        let v1 = &bytes[..bytes.len() - 1];
        let loaded = CookedMesh::from_postcard(v1).expect("v1 decode");
        assert_eq!(loaded.lod_metric, crate::LOD_METRIC_LEGACY);
        assert_eq!(loaded.lods[0].screen_relative_size, 1.0);
    }

    // -- cooked mesh format v2 (skin, colour, morph targets) -------------------

    fn skinned(joint_count: usize, eight: bool) -> CpuMesh {
        let names: Vec<String> = (0..joint_count).map(|i| format!("joint_{i}")).collect();
        let last = (joint_count - 1) as u16;
        let set1 = |v: [u16; 4]| if eight { vec![v; 3] } else { Vec::new() };
        let weights = if eight {
            [10000, 10000, 10000, 10000]
        } else {
            [40000, 20000, 5535, 0]
        };
        CpuMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            uvs: vec![[0.0, 0.0]; 3],
            tangents: vec![[1.0, 0.0, 0.0, 1.0]; 3],
            indices: vec![0, 1, 2],
            skin: Some(crate::CpuSkin {
                joint_names: names,
                joints: vec![[0, 1, last, 2]; 3],
                weights: vec![weights; 3],
                joints1: set1([3, 4, 5, last]),
                weights1: set1([10000, 10000, 5535, 0]),
                eight_influences: false,
            }),
            colors: vec![[1.0, 0.5, 0.25, 1.0], [0.0, 0.0, 0.0, 0.0], [0.2, 0.4, 0.6, 0.8]],
            morph_targets: vec![crate::CpuMorphTarget {
                name: "smile".into(),
                vertex_indices: vec![0, 2],
                position_deltas: vec![[0.5, -0.25, 0.125], [0.0, 1.0, -2.0]],
                normal_deltas: vec![[0.0, 0.5, 0.0], [0.25, 0.0, 0.0]],
                tangent_deltas: Vec::new(),
            }],
        }
    }

    /// Packs a CPU mesh the way the cooker does (one LOD, v2 blocks).
    fn pack(cpu: &CpuMesh) -> CookedMesh {
        let mut mesh = triangle_mesh(
            vec![CookedMeshLod {
                screen_relative_size: 0.0,
                index_byte_offset: 0,
                index_byte_length: 6,
            }],
            crate::LOD_METRIC_ERROR,
        );
        mesh.skin = cpu.skin.as_ref().map(|s| s.pack(3).expect("skin"));
        mesh.stream_color0 = crate::pack_colors_unorm16(&cpu.colors);
        mesh.morph_targets = cpu
            .morph_targets
            .iter()
            .map(|t| t.pack(3).expect("morph"))
            .collect();
        mesh
    }

    #[test]
    fn skinned_payload_round_trips_bit_exact() {
        for (joints, eight, width) in [
            (67, false, crate::CookedJointWidth::U8),
            (82, true, crate::CookedJointWidth::U8),
            (256, false, crate::CookedJointWidth::U8),
            (300, true, crate::CookedJointWidth::U16),
        ] {
            let cpu = skinned(joints, eight);
            let mesh = pack(&cpu);
            assert_eq!(
                mesh.skin.as_ref().map(|s| s.joint_width),
                Some(width),
                "{joints} joints"
            );
            let bytes = mesh.to_postcard().expect("encode");
            let loaded = CookedMesh::from_postcard(&bytes).expect("decode");
            assert_eq!(loaded, mesh);
            let back = loaded.to_cpu_mesh().expect("unpack");
            assert_eq!(
                back.skin, cpu.skin,
                "{joints} joints: joints and weights bit-exact"
            );
            for (got, want) in back.colors.iter().flatten().zip(cpu.colors.iter().flatten()) {
                assert!(
                    (got - want).abs() <= 0.5 / 65535.0,
                    "unorm16 colour {got} vs {want}"
                );
            }
            assert_eq!(
                crate::pack_colors_unorm16(&back.colors),
                mesh.stream_color0,
                "colour bits stable"
            );
            assert_eq!(back.morph_targets, cpu.morph_targets, "deltas are exact in f16");
        }
    }

    #[test]
    fn morph_bounds_cover_the_position_deltas() {
        let mesh = pack(&skinned(67, false));
        let target = &mesh.morph_targets[0];
        assert_eq!(target.delta_min, [0.0, -0.25, -2.0]);
        assert_eq!(target.delta_max, [0.5, 1.0, 0.125]);
    }

    #[test]
    fn v1_payload_loads_with_no_v2_blocks_and_the_same_vertices() {
        let v2 = pack(&skinned(67, false));
        let v1_bytes = v2.to_postcard_v1().expect("v1 encode");
        assert_eq!(CookedMesh::payload_version(&v1_bytes).expect("version"), 1);
        let loaded = CookedMesh::from_postcard(&v1_bytes).expect("v1 decode");
        assert_eq!(loaded.format_version, crate::COOKED_MESH_FORMAT_VERSION);
        assert!(loaded.skin.is_none() && loaded.stream_color0.is_empty() && loaded.morph_targets.is_empty());
        let rigid = CookedMesh {
            skin: None,
            stream_color0: Vec::new(),
            morph_targets: Vec::new(),
            ..v2
        };
        assert_eq!(loaded, rigid);
        let cpu = loaded.to_cpu_mesh().expect("unpack");
        assert_eq!(cpu, rigid.to_cpu_mesh().expect("unpack rigid"));
        assert!(cpu.skin.is_none());
    }

    #[test]
    fn newer_payload_version_is_rejected() {
        let mut mesh = pack(&skinned(67, false));
        mesh.format_version = crate::COOKED_MESH_FORMAT_VERSION + 1;
        let bytes = mesh.to_postcard().expect("encode");
        assert!(matches!(
            CookedMesh::from_postcard(&bytes),
            Err(CookedMeshLoadError::UnsupportedVersion { found, .. })
                if found == crate::COOKED_MESH_FORMAT_VERSION + 1
        ));
    }

    /// audit-v034b-formats L6: the current layout ignored bytes after the mesh, while the older
    /// layouts must consume the whole payload, so a damaged current file loaded as valid.
    #[test]
    fn trailing_bytes_after_a_current_payload_are_an_error() {
        let mut bytes = pack(&skinned(67, false)).to_postcard().expect("encode");
        CookedMesh::from_postcard(&bytes).expect("the intact payload loads");
        bytes.extend_from_slice(&[0xAB; 5]);
        assert!(
            CookedMesh::from_postcard(&bytes).is_err(),
            "five garbage bytes appended"
        );
    }

    #[test]
    fn truncated_v2_payload_is_an_error_not_a_v1_mesh() {
        let bytes = pack(&skinned(67, false)).to_postcard().expect("encode");
        for cut in 1..40 {
            let short = &bytes[..bytes.len() - cut];
            assert!(CookedMesh::from_postcard(short).is_err(), "cut {cut} bytes");
        }
    }

    #[test]
    fn invalid_v2_blocks_are_rejected() {
        let good = pack(&skinned(67, false));
        let mut bad_joint = good.clone();
        if let Some(skin) = bad_joint.skin.as_mut() {
            skin.joint_names.truncate(10);
        }
        assert!(CookedMesh::from_postcard(&bad_joint.to_postcard().expect("encode")).is_err());
        let mut wrong_width = good.clone();
        if let Some(skin) = wrong_width.skin.as_mut() {
            skin.joint_width = crate::CookedJointWidth::U16;
        }
        assert!(wrong_width.validate().is_err());
        let mut short_color = good.clone();
        short_color.stream_color0.pop();
        assert!(short_color.validate().is_err());
        let mut unordered = good.clone();
        unordered.morph_targets[0].vertex_indices = vec![2, 0];
        assert!(unordered.validate().is_err());
        let mut out_of_range = good;
        out_of_range.morph_targets[0].vertex_indices = vec![0, 3];
        assert!(out_of_range.validate().is_err());
    }

    #[test]
    fn migrator_rewrites_a_v1_cooked_file_at_v2() {
        use crate::cooked_header::{CookedHeader, decode_cooked_file, encode_cooked_file};
        let v2 = pack(&skinned(82, false));
        let payload = v2.to_postcard_v1().expect("v1 encode");
        let header = CookedHeader {
            format_version: crate::COOKED_FORMAT_VERSION,
            type_tag: crate::AssetTypeTag::mesh().0,
            asset_id: crate::AssetId::new_random(),
            importer_version: crate::pack_importer_version(1, 7, 0),
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).expect("file");
        assert_eq!(crate::cooked_mesh_file_version(&file).expect("version"), Some(1));
        let before = crate::load_mesh_bytes(&file).expect("v1 load").mesh;
        let migrated = crate::migrate_cooked_mesh_file(&file)
            .expect("migrate")
            .expect("v1 needs migration");
        assert_eq!(
            crate::cooked_mesh_file_version(&migrated).expect("version"),
            Some(crate::COOKED_MESH_FORMAT_VERSION)
        );
        let (new_header, _) = decode_cooked_file(&migrated).expect("decode");
        assert_eq!(
            CookedHeader {
                payload_len: header.payload_len,
                ..new_header
            },
            header
        );
        let after = crate::load_mesh_bytes(&migrated).expect("v2 load").mesh;
        assert_eq!(after, before, "a migrated mesh loads to the same vertices");
        assert_eq!(crate::migrate_cooked_mesh_file(&migrated).expect("again"), None);
        // Other cooked types are not touched.
        let image = encode_cooked_file(
            &CookedHeader {
                type_tag: crate::AssetTypeTag::image().0,
                payload_len: 3,
                ..header
            },
            b"abc",
        )
        .expect("image");
        assert_eq!(crate::migrate_cooked_mesh_file(&image).expect("image"), None);
    }

    /// A cooked file around `payload` (cooked-mesh type tag, importer 1.7.0).
    fn mesh_file(payload: &[u8]) -> Vec<u8> {
        use crate::cooked_header::{CookedHeader, encode_cooked_file};
        let header = CookedHeader {
            format_version: crate::COOKED_FORMAT_VERSION,
            type_tag: crate::AssetTypeTag::mesh().0,
            asset_id: crate::AssetId(regia_core::id::Guid::from_u128(0x1622)),
            importer_version: crate::pack_importer_version(1, 7, 0),
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        encode_cooked_file(&header, payload).expect("file")
    }

    #[test]
    fn v2_payload_loads_at_four_influences_and_unpacks_as_before() {
        assert_eq!(crate::COOKED_MESH_FORMAT_VERSION, 3);
        for eight in [false, true] {
            let v3 = pack(&skinned(82, eight));
            assert_eq!(
                v3.skin_influences, DEFAULT_SKIN_INFLUENCES,
                "the cook default is 4"
            );
            let v2_bytes = v3.to_postcard_v2().expect("v2 encode");
            assert_eq!(CookedMesh::payload_version(&v2_bytes).expect("version"), 2);
            let loaded = CookedMesh::from_postcard(&v2_bytes).expect("v2 decode");
            assert_eq!(
                loaded, v3,
                "eight {eight}: a v2 payload is the v3 mesh at 4 influences"
            );
            let cpu = loaded.to_cpu_mesh().expect("unpack");
            assert_eq!(cpu, v3.to_cpu_mesh().expect("unpack v3"));
            assert_eq!(cpu.skin.as_ref().map(|s| s.eight_influences), Some(false));
            // One byte longer than v2, and only at the end.
            let v3_bytes = v3.to_postcard().expect("v3 encode");
            assert_eq!(v3_bytes.len(), v2_bytes.len() + 1);
            assert_eq!(v3_bytes.last(), Some(&DEFAULT_SKIN_INFLUENCES));
        }
    }

    #[test]
    fn eight_influences_round_trip_and_reach_the_cpu_skin() {
        let mut mesh = pack(&skinned(82, true));
        mesh.skin_influences = EIGHT_SKIN_INFLUENCES;
        let bytes = mesh.to_postcard().expect("encode");
        assert_eq!(CookedMesh::payload_version(&bytes).expect("version"), 3);
        let loaded = CookedMesh::from_postcard(&bytes).expect("decode");
        assert_eq!(loaded, mesh);
        let eight = loaded.to_cpu_mesh().expect("unpack");
        let skin = eight.skin.clone().expect("skin");
        assert!(skin.eight_influences);
        assert_eq!(skin.influences_per_vertex(), 8);
        // The flag is the only difference from the default mesh's skin.
        mesh.skin_influences = DEFAULT_SKIN_INFLUENCES;
        let four = mesh.to_cpu_mesh().expect("unpack");
        let four_skin = four.skin.clone().expect("skin");
        assert!(!four_skin.eight_influences);
        assert_eq!(
            crate::CpuSkin {
                eight_influences: false,
                ..skin
            },
            four_skin
        );
        assert_eq!(eight.positions, four.positions);
        assert_eq!(eight.indices, four.indices);
    }

    #[test]
    fn skin_influences_must_be_four_or_eight_with_a_second_set() {
        let mut odd = pack(&skinned(82, true));
        odd.skin_influences = 5;
        assert!(matches!(
            odd.validate(),
            Err(CookedMeshValidationError::SkinInfluences { value: 5, .. })
        ));
        assert!(CookedMesh::from_postcard(&odd.to_postcard().expect("encode")).is_err());
        let mut four_only = pack(&skinned(67, false));
        four_only.skin_influences = EIGHT_SKIN_INFLUENCES;
        assert!(four_only.validate().is_err(), "8 without a second influence set");
        let mut rigid = pack(&skinned(67, false));
        rigid.skin = None;
        assert!(rigid.validate().is_ok());
        rigid.skin_influences = EIGHT_SKIN_INFLUENCES;
        assert!(rigid.validate().is_err(), "8 without a skin");
        rigid.skin_influences = 0;
        assert!(rigid.validate().is_err());
    }

    #[test]
    fn truncated_v3_payload_is_never_read_as_v2() {
        // Cutting the trailing `skin_influences` byte leaves a v2-shaped payload that still says
        // version 3: it is an error, not a v2 mesh.
        let bytes = pack(&skinned(82, true)).to_postcard().expect("encode");
        let short = &bytes[..bytes.len() - 1];
        assert!(CookedMesh::from_postcard(short).is_err());
    }

    #[test]
    fn migrator_rewrites_a_v2_cooked_file_at_v3_byte_for_byte() {
        let mesh = pack(&skinned(82, true));
        let file = mesh_file(&mesh.to_postcard_v2().expect("v2 encode"));
        assert_eq!(crate::cooked_mesh_file_version(&file).expect("version"), Some(2));
        let before = crate::load_mesh_bytes(&file).expect("v2 load").mesh;
        let migrated = crate::migrate_cooked_mesh_file(&file)
            .expect("migrate")
            .expect("v2 needs migration");
        assert_eq!(
            crate::cooked_mesh_file_version(&migrated).expect("version"),
            Some(3)
        );
        assert_eq!(
            migrated,
            mesh_file(&mesh.to_postcard().expect("v3 encode")),
            "a migrated v2 file is the file a fresh cook writes"
        );
        let after = crate::load_mesh_bytes(&migrated).expect("v3 load").mesh;
        assert_eq!(after, before, "a migrated mesh loads to the same vertices");
        assert_eq!(crate::migrate_cooked_mesh_file(&migrated).expect("again"), None);
        assert_eq!(crate::cooked_mesh_migration_sources(), vec![1, 2]);
    }

    #[test]
    fn legacy_cpu_mesh_postcard_is_unchanged_by_the_v2_fields() {
        // The legacy payload has no room for the v2 fields: it must stay byte-identical so old
        // `regia_asset::CpuMesh` cooks still decode.
        let mut mesh = skinned(67, false);
        let rigid_bytes = CpuMesh {
            skin: None,
            colors: Vec::new(),
            morph_targets: Vec::new(),
            ..mesh.clone()
        }
        .to_postcard()
        .expect("encode");
        assert_eq!(mesh.to_postcard().expect("encode"), rigid_bytes);
        mesh.skin = None;
        mesh.colors.clear();
        mesh.morph_targets.clear();
        assert_eq!(CpuMesh::from_postcard(&rigid_bytes).expect("decode"), mesh);
    }
}

// ---------------------------------------------------------------------------
// CookedModel (which material each mesh of an imported model draws with)
// ---------------------------------------------------------------------------

/// [`CookedModel::version`] written by this build.
pub const COOKED_MODEL_VERSION: u32 = 1;

/// One drawable part of an imported model: a cooked mesh sub-asset and the cooked material it
/// draws with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookedModelPart {
    /// The [`CookedMesh`] sub-asset (one glTF primitive, node transform baked).
    pub mesh: AssetId,
    /// The [`crate::CookedStandardMaterial`] sub-asset, `None` for a primitive without one
    /// (glTF's default material).
    pub material: Option<AssetId>,
    /// The node name the primitive came from (for diagnostics and LOD filtering).
    pub node: String,
}

/// The parts of an imported model (`model` sub-asset of a glTF, primitives in document order),
/// so a runtime that only has the pak can rebuild the model's draws: mesh sub-assets do not name
/// their material.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookedModel {
    /// [`COOKED_MODEL_VERSION`].
    pub version: u32,
    /// Every drawn primitive (sockets and collision nodes are not parts).
    pub parts: Vec<CookedModelPart>,
}

/// Errors from [`CookedModel::from_postcard`].
#[derive(Debug, Error)]
pub enum CookedModelLoadError {
    /// Postcard failure.
    #[error("postcard decode error: {0}")]
    Postcard(#[from] postcard::Error),
    /// A version this build does not read (the cooker rebuilds it).
    #[error("cooked model version {found} (this build reads {supported})")]
    UnsupportedVersion {
        /// Version in the payload.
        found: u32,
        /// [`COOKED_MODEL_VERSION`].
        supported: u32,
    },
}

impl CookedModel {
    /// Serialize to postcard bytes.
    pub fn to_postcard(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    /// Deserialize; rejects any version other than [`COOKED_MODEL_VERSION`] before the rest.
    pub fn from_postcard(bytes: &[u8]) -> Result<Self, CookedModelLoadError> {
        let (version, _): (u32, _) = postcard::take_from_bytes(bytes)?;
        if version != COOKED_MODEL_VERSION {
            return Err(CookedModelLoadError::UnsupportedVersion {
                found: version,
                supported: COOKED_MODEL_VERSION,
            });
        }
        Ok(postcard::from_bytes(bytes)?)
    }
}
