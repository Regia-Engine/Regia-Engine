// SPDX-License-Identifier: MPL-2.0
//! The cooked `<map>.meshlayers` v2 (spec 121 § 9.2; registry name `terrain-mesh-layers-cooked`).
//!
//! One file, three or four parts in this order (ADR-8112 for the byte layout, ADR-8816 for v2):
//!
//! ```text
//! COOKED_MAGIC (4 bytes)
//! postcard(MeshLayersHeader)                 version first: magic, version, input hash, copies
//! postcard(Vec<u8>) = the canonical section  (below), canonical_version first
//! postcard(Vec<u8>) = the presentation section (the render meshes, crate::render, ADR-8175)
//! postcard(Vec<u8>) = the chunk section, only when not empty (opaque here; the terrain crate's
//!                     mesh-layer chunks, ADR-8398, ADR-8576)
//! ```
//!
//! The canonical section, version 2 (ADR-8816):
//!
//! ```text
//! canonical_version         postcard u32 (2)
//! head_len                  u32, little-endian
//! postcard(CanonicalHead)   head_len bytes: everything but the counts, the stored length of
//!                           every tile's count block, and one BLAKE3 per count-body block
//! the count body            every ground tile's counts, ascending tile index, row major, each
//!                           count a little-endian u16 (fixed width, no length prefix)
//! ```
//!
//! The canonical section holds every byte the run time reads (ADR-8084, review W1).
//! `content_hash` is the BLAKE3 of the section's hashed prefix exactly as stored (the version, the
//! head length and the head). The head holds the BLAKE3 of every [`COUNT_BLOCK_BYTES`] block of
//! the count body, and the loader refuses a body block that does not match (`geom_corrupt`), so
//! the hash covers every byte of the section: two peers that differ in a setting, a lattice
//! constant, the ground mode, a tile level, a ring pin, an override, a cut bit, a triangle or a
//! count never pass the match handshake. The header's settings and ground-mode copies are
//! informational (tools read the header alone); the loader refuses a file whose copies differ from
//! the canonical section (`geom_corrupt`). A DO27 headless server reads the header and the
//! canonical section only and never decodes the presentation or chunk bytes.
//!
//! Version 1 (v0.6.0) stored the canonical section as one `postcard(WorldGeomCooked)` (every count
//! a varint) and hashed all of it. It still loads: the loader migrates it in memory
//! ([`migrate_canonical`]), and its `content_hash` is the hash of the migrated section, so a peer
//! with a version 1 file and a peer with the same map cooked at version 2 agree.
//! `regia upgrade-project` rewrites it ([`migrate_file`]).
//!
//! The file is a cache: the cook pipeline recooks on a version mismatch.

use serde::{Deserialize, Serialize};

use crate::meshlayers::GroundMode;
use crate::settings::GeomSettings;
use crate::units::MAX_EXTENT_MM;

/// The first four bytes of a cooked `<map>.meshlayers` file.
pub const COOKED_MAGIC: [u8; 4] = *b"RGML";

/// The file (header) version this engine writes (2) and the newest it reads. Version 2 (v0.7,
/// format batch 2) holds a version 2 canonical section (fixed-width counts behind a hashed head,
/// ADR-8816) and the terrain crate's chunk section version 3 (its layers' `collider` flag,
/// ADR-8872). It also reads version 1 ([`migrate_file`]; the chunk section migrates in the
/// terrain crate).
pub const COOKED_FORMAT_VERSION: u32 = 2;

/// The canonical section's own version (2, ADR-8816). Part of the hashed bytes. It moves with the
/// file version: a version 1 file holds a version 1 section.
pub const CANONICAL_VERSION: u32 = 2;

/// Bytes per hashed block of the canonical section's count body (the last block may be shorter).
/// Part of the format: the head stores one BLAKE3 per block.
pub const COUNT_BLOCK_BYTES: usize = 1 << 16;

/// When a load checks the canonical section's count body against the head's hashes (ADR-8816).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CountBodyCheck {
    /// During the load: what `WorldGeom::from_cooked` and `WorldGeom::from_canonical_bytes` do. A
    /// map that loaded has the `content_hash` of its bytes.
    #[default]
    AtLoad,
    /// Not by the load, which then never reads the count body twice. The caller runs
    /// [`check_count_body`] on the same bytes (it is pure; any thread) and must not let the map
    /// into a match handshake, or keep it, unless that returns `Ok`: until then `content_hash`
    /// vouches for the head only.
    Deferred,
}

/// The older file versions the loader and [`migrate_file`] read (for `cargo xtask check-formats`).
#[must_use]
pub fn cooked_migration_sources() -> Vec<u32> {
    vec![1]
}

/// The header: what tools read without the rest (spec 121 § 9.2 item 1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeshLayersHeader {
    /// [`COOKED_FORMAT_VERSION`].
    pub version: u32,
    /// BLAKE3 of the source document, the heightfield and every file the document names
    /// (`crate::cook::input_hash`): a cook whose inputs did not change may be skipped.
    pub input_hash: [u8; 32],
    /// A copy of the canonical settings (informational; must equal them).
    pub settings: GeomSettings,
    /// A copy of the canonical ground mode (informational; must equal it).
    pub ground: GroundMode,
}

/// The lattice constants (spec 121 § 3): independent of every render setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatticeConstants {
    /// The heightfield's sample count per side (`n`, at least 2).
    pub lattice_n: u32,
    /// The map width, millimetres (`m_to_mm(world_width_m)`).
    pub width_mm: i32,
    /// Lattice cells per ground tile side (a power of two, 16 to 256).
    pub ground_tile_cells: u16,
    /// The `f32` bits of the height of count 0, metres.
    pub offset_m_bits: u32,
    /// The `f32` bits of the height of one count, metres (`scale / 65535`).
    pub step_m_bits: u32,
}

impl LatticeConstants {
    /// The height offset, metres.
    #[must_use]
    pub fn offset_m(&self) -> f32 {
        f32::from_bits(self.offset_m_bits)
    }

    /// The height step per count, metres.
    #[must_use]
    pub fn step_m(&self) -> f32 {
        f32::from_bits(self.step_m_bits)
    }

    /// Lattice cells per side (`n - 1`).
    #[must_use]
    pub fn cells(&self) -> u32 {
        self.lattice_n.saturating_sub(1)
    }

    /// Ground tiles per side (`ceil((n - 1) / g)`).
    #[must_use]
    pub fn tiles_per_side(&self) -> u32 {
        let g = u32::from(self.ground_tile_cells.max(1));
        self.cells().div_ceil(g)
    }

    /// The cell range `[start, end)` ground tile column (or row) `a` covers along one axis.
    #[must_use]
    pub fn tile_cells(&self, a: u32) -> (u32, u32) {
        let g = u32::from(self.ground_tile_cells);
        let start = a * g;
        (start, (start + g).min(self.cells()))
    }

    /// Vertices per side of ground tile `(a, b)` at `level`: its cells along the axis divided by
    /// `2^level`, plus one. `None` when the tile's side is not a multiple of `2^level`.
    #[must_use]
    pub fn tile_side_vertices(&self, a: u32, level: u8) -> Option<u32> {
        let (start, end) = self.tile_cells(a);
        let cells = end - start;
        let step = 1u32.checked_shl(u32::from(level))?;
        cells.is_multiple_of(step).then_some(cells / step + 1)
    }

    /// The world x (or z) of lattice vertex index `i`, millimetres (spec 121 § 3):
    /// `-width/2 (floor) + round_half_up(i * width / (n - 1))`, in `i64`.
    #[must_use]
    pub fn vertex_mm(&self, i: u32) -> i64 {
        let w = i64::from(self.width_mm);
        let d = i64::from(self.cells().max(1));
        -w.div_euclid(2) + (2 * i64::from(i) * w + d).div_euclid(2 * d)
    }
}

/// An absolute vertex height (a ring pin or a hanging-vertex override), millimetres.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VertexHeightMm {
    /// Lattice vertex index `j * n + i`.
    pub vertex: u32,
    /// The height, millimetres.
    pub height_mm: i32,
}

/// What a canonical triangle is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum TriKind {
    /// Up-facing walkable floor.
    Floor,
    /// Down-facing roof.
    Roof,
    /// Near-vertical wall.
    Wall,
    /// A blocker plug triangle (solid only while its blocker is active).
    Blocker,
}

/// [`CookedTriangle::flags`] bit: a floor that opens onto the outdoor ground (the Stage A `portal`).
pub const TRI_FLAG_PORTAL: u8 = 1;
/// [`CookedTriangle::flags`] bit: a deck only on-foot bodies use (the Stage A `pilot_only`).
pub const TRI_FLAG_PILOT_ONLY: u8 = 2;
/// [`CookedTriangle::flags`] bit: a floor the cook's trim step took out of support (spec 121 § 9.2,
/// E6: a strip under 2 m wide within 0.5 m of another floor carries nothing).
pub const TRI_FLAG_NO_SUPPORT: u8 = 4;
/// Every defined flag bit.
pub const TRI_FLAGS_ALL: u8 = TRI_FLAG_PORTAL | TRI_FLAG_PILOT_ONLY | TRI_FLAG_NO_SUPPORT;

/// One canonical triangle: vertices in millimetres local to its tile (y is world millimetres).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookedTriangle {
    /// The three vertices `[x, y, z]`, x and z local to the tile origin, y world.
    pub v: [[i32; 3]; 3],
    /// Floor, roof, wall or blocker.
    pub kind: TriKind,
    /// `TRI_FLAG_*` bits.
    pub flags: u8,
    /// The section it belongs to (`None` until the cook cuts sections).
    pub section: Option<u16>,
    /// Footstep material slot.
    pub material: u16,
    /// The layer (index in the document).
    pub layer: u16,
    /// The triangle's index in its layer's canonical (source) order.
    pub index: u32,
}

/// One 1024 m cooked tile: storage only, never a sim bit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookedTile {
    /// The tile's world origin `[x, z]`, millimetres, each a multiple of `units::TILE_MM`.
    pub origin_mm: [i32; 2],
    /// The tile's triangles sorted by `(layer, index)`.
    pub triangles: Vec<CookedTriangle>,
}

/// The mouth-cut bitmap of one ground tile: one bit per lattice triangle (two per cell, cell
/// `(ci, cj)` local to the tile, bit `2 * (cj * cols + ci) + t`); a set bit is cut away.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CutBitmap {
    /// The ground tile index `b * tiles_per_side + a`.
    pub tile: u32,
    /// The bits, 64 per word.
    pub bits: Vec<u64>,
}

/// A run of `(layer, triangle)` references, `[start, end)` in canonical order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriRange {
    /// The layer.
    pub layer: u16,
    /// First triangle index.
    pub start: u32,
    /// One past the last triangle index.
    pub end: u32,
}

/// A cooked section (spec 121 § 5.6).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookedSection {
    /// Dense id, equal to its position.
    pub id: u16,
    /// The passage or layer it came from.
    pub source: String,
    /// A roof over every floor triangle.
    pub covered: bool,
    /// Axis-aligned bounds `[min, max]`, world millimetres.
    pub bounds_mm: [[i32; 3]; 2],
    /// Its triangle ranges, sorted.
    pub triangles: Vec<TriRange>,
    /// Portals on its boundary, sorted.
    pub portals: Vec<u16>,
    /// The blocker plug that can close it.
    pub blocker: Option<u16>,
}

/// A cooked portal (spec 121 § 5.6).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookedPortal {
    /// Dense id, equal to its position.
    pub id: u16,
    /// The two cells it joins (`None` = outdoors).
    pub sides: [Option<u16>; 2],
    /// The polygon, world millimetres.
    pub polygon_mm: Vec<[i32; 3]>,
    /// The portal's nav node: the floor point a path crosses it at (the floor centre of the cut, the end or the
    /// ramp top), world millimetres (lane W-08; W-19/W-20 link their layers here).
    pub nav_mm: [i32; 3],
    /// The mouth hole it rings, for a mouth portal.
    pub mouth: Option<String>,
    /// The blocker that closes it.
    pub blocker: Option<u16>,
}

/// The shape of a section zone.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum CookedZoneShape {
    /// An oriented box: centre and half extents in world millimetres, yaw about +y in radians.
    OrientedBox {
        /// Centre.
        center_mm: [i32; 3],
        /// Half extents.
        half_mm: [i32; 3],
        /// Yaw, radians.
        yaw: f32,
    },
    /// A capsule between two points with a radius, world millimetres.
    Capsule {
        /// One end.
        a_mm: [i32; 3],
        /// The other end.
        b_mm: [i32; 3],
        /// Radius.
        radius_mm: i32,
    },
}

/// A cooked section zone (spec 121 § 5.6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookedZone {
    /// The section.
    pub section: u16,
    /// The shape.
    pub shape: CookedZoneShape,
    /// Covered exposure band low and high (EV100) and blend distance (m).
    pub exposure: [f32; 3],
    /// Reverb preset name.
    pub reverb_preset: String,
    /// Reverb cavity size, metres.
    pub reverb_size_m: f32,
}

/// A cooked blocker plug (spec 121 § 5.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookedPlug {
    /// Dense id, equal to its position.
    pub id: u16,
    /// The section it closes.
    pub section: u16,
    /// Its state in a fresh `GeomState`.
    pub initially_active: bool,
    /// The plug's canonical triangles.
    pub triangles: TriRange,
}

/// The canonical section's value (`WorldGeomCooked`, spec 121 § 9.2 item 2). Every byte a query, an
/// edit, a blocker flip or a fresh state reads is here.
///
/// Stored through [`encode_canonical`] and [`decode_canonical`] (the version 2 layout). The derived
/// serde form in field order is the version 1 section, kept for the migrator: do not write it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorldGeomCooked {
    /// [`CANONICAL_VERSION`].
    pub canonical_version: u32,
    /// The settings (normalized: no `-0.0`, all finite).
    pub settings: GeomSettings,
    /// The lattice constants.
    pub lattice: LatticeConstants,
    /// The authored ground mode.
    pub ground: GroundMode,
    /// The level of every ground tile, ascending tile index (0 = full pitch).
    pub tile_levels: Vec<u8>,
    /// Hanging-vertex overrides (nested-pitch seam rule 4), ascending vertex.
    pub hanging_overrides: Vec<VertexHeightMm>,
    /// Mouth-ring pins with their heights (E5's set), ascending vertex.
    pub ring_pins: Vec<VertexHeightMm>,
    /// The cooked tiles, ascending `(z, x)` origin.
    pub tiles: Vec<CookedTile>,
    /// The ground lattice: `u16` counts per ground tile at its level, ascending tile index, row
    /// major inside the tile. A cooked file includes border vertices. After load the runtime keeps
    /// only the vertices the tile owns (east and north borders live on the next tile; the map edge
    /// stays). See ADR-8246 R7.
    pub ground_counts: Vec<Vec<u16>>,
    /// The mouth-cut bitmaps, ascending tile index.
    pub cut_bitmaps: Vec<CutBitmap>,
    /// Sections.
    pub sections: Vec<CookedSection>,
    /// Portals.
    pub portals: Vec<CookedPortal>,
    /// Zones.
    pub zones: Vec<CookedZone>,
    /// Blocker plugs.
    pub plugs: Vec<CookedPlug>,
}

/// The head of a version 2 canonical section as stored (postcard, in field order; ADR-8816):
/// every [`WorldGeomCooked`] field but the version (stored before it) and the counts (stored after
/// it, fixed width), then the stored length of each count block and the count body's hashes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanonicalHead {
    /// [`WorldGeomCooked::settings`].
    pub settings: GeomSettings,
    /// [`WorldGeomCooked::lattice`].
    pub lattice: LatticeConstants,
    /// [`WorldGeomCooked::ground`].
    pub ground: GroundMode,
    /// [`WorldGeomCooked::tile_levels`].
    pub tile_levels: Vec<u8>,
    /// [`WorldGeomCooked::hanging_overrides`].
    pub hanging_overrides: Vec<VertexHeightMm>,
    /// [`WorldGeomCooked::ring_pins`].
    pub ring_pins: Vec<VertexHeightMm>,
    /// [`WorldGeomCooked::tiles`].
    pub tiles: Vec<CookedTile>,
    /// [`WorldGeomCooked::cut_bitmaps`].
    pub cut_bitmaps: Vec<CutBitmap>,
    /// [`WorldGeomCooked::sections`].
    pub sections: Vec<CookedSection>,
    /// [`WorldGeomCooked::portals`].
    pub portals: Vec<CookedPortal>,
    /// [`WorldGeomCooked::zones`].
    pub zones: Vec<CookedZone>,
    /// [`WorldGeomCooked::plugs`].
    pub plugs: Vec<CookedPlug>,
    /// The number of counts each ground tile's block stores, ascending tile index (the lengths
    /// of [`WorldGeomCooked::ground_counts`]). The count body is twice their sum in bytes.
    pub count_lens: Vec<u32>,
    /// The BLAKE3 of each [`COUNT_BLOCK_BYTES`] block of the count body, in order (the last block
    /// may be shorter; a body of `n` bytes has `ceil(n / COUNT_BLOCK_BYTES)` hashes).
    pub count_hashes: Vec<[u8; 32]>,
}

/// [`CanonicalHead`] by reference, for the encoder: the same fields in the same order.
#[derive(Serialize)]
struct CanonicalHeadRef<'a> {
    settings: &'a GeomSettings,
    lattice: &'a LatticeConstants,
    ground: &'a GroundMode,
    tile_levels: &'a [u8],
    hanging_overrides: &'a [VertexHeightMm],
    ring_pins: &'a [VertexHeightMm],
    tiles: &'a [CookedTile],
    cut_bitmaps: &'a [CutBitmap],
    sections: &'a [CookedSection],
    portals: &'a [CookedPortal],
    zones: &'a [CookedZone],
    plugs: &'a [CookedPlug],
    count_lens: &'a [u32],
    count_hashes: &'a [[u8; 32]],
}

/// A whole cooked file: the header, the canonical section and the opaque presentation and chunk
/// bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct CookedMeshLayers {
    /// The header.
    pub header: MeshLayersHeader,
    /// The canonical section.
    pub canonical: WorldGeomCooked,
    /// The presentation section (the `Passages` render meshes, `crate::render::PresentationCooked`,
    /// ADR-8175), as stored.
    pub presentation: Vec<u8>,
    /// The chunk section (the terrain's mesh-layer chunks and their LOD meshes,
    /// `regia_terrain::TerrainMeshLayersCooked`, ADR-8398), as stored. Empty in a file without
    /// chunks; stored after the presentation section only when not empty (ADR-8576), so a file
    /// without chunks has the bytes it had before the section existed.
    pub chunks: Vec<u8>,
}

/// Why a cooked file did not load (spec 121 § 5.2). Codes are part of the agent protocol.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GeomLoadError {
    /// (`geom_newer_version`) written by a newer engine.
    #[error("cooked meshlayers version {found} is newer than this engine's {supported}")]
    NewerVersion {
        /// The version in the file.
        found: u32,
        /// This engine's version.
        supported: u32,
    },
    /// (`geom_corrupt`) the bytes do not decode, or a count or index is out of range.
    #[error("cooked meshlayers file is corrupt: {0}")]
    Corrupt(String),
    /// (`geom_extent_too_large`) a coordinate beyond 2^24 mm of the origin.
    #[error("cooked meshlayers coordinate beyond 2^24 mm of the origin")]
    ExtentTooLarge,
}

impl GeomLoadError {
    /// The agent error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NewerVersion { .. } => "geom_newer_version",
            Self::Corrupt(_) => "geom_corrupt",
            Self::ExtentTooLarge => "geom_extent_too_large",
        }
    }
}

fn corrupt(msg: impl Into<String>) -> GeomLoadError {
    GeomLoadError::Corrupt(msg.into())
}

/// The canonical section's bytes in the version 2 layout (ADR-8816): exactly what the file stores.
/// `content_hash` hashes their prefix ([`content_hash_of_bytes`]). The leading version is the
/// value's `canonical_version`.
///
/// # Errors
///
/// [`GeomLoadError::Corrupt`] for a value whose `canonical_version` is 1 (the version 1 layout is
/// read, never written), whose head is 4 GiB or more, or whose count block is longer than `u32`.
pub fn encode_canonical(canonical: &WorldGeomCooked) -> Result<Vec<u8>, GeomLoadError> {
    if canonical.canonical_version == 1 {
        return Err(corrupt(
            "canonical_version 1 is the v0.6.0 layout: this engine reads it and writes version 2",
        ));
    }
    let mut count_lens = Vec::with_capacity(canonical.ground_counts.len());
    let mut body_len = 0usize;
    for block in &canonical.ground_counts {
        let len = u32::try_from(block.len()).map_err(|_| corrupt("a count block is longer than u32"))?;
        count_lens.push(len);
        body_len += block.len() * 2;
    }
    let mut body = vec![0u8; body_len];
    let mut at = 0usize;
    for block in &canonical.ground_counts {
        let (pairs, _) = body[at..at + block.len() * 2].as_chunks_mut::<2>();
        for (pair, count) in pairs.iter_mut().zip(block) {
            *pair = count.to_le_bytes();
        }
        at += block.len() * 2;
    }
    let count_hashes: Vec<[u8; 32]> = body
        .chunks(COUNT_BLOCK_BYTES)
        .map(|block| *blake3::hash(block).as_bytes())
        .collect();
    let head = postcard::to_allocvec(&CanonicalHeadRef {
        settings: &canonical.settings,
        lattice: &canonical.lattice,
        ground: &canonical.ground,
        tile_levels: &canonical.tile_levels,
        hanging_overrides: &canonical.hanging_overrides,
        ring_pins: &canonical.ring_pins,
        tiles: &canonical.tiles,
        cut_bitmaps: &canonical.cut_bitmaps,
        sections: &canonical.sections,
        portals: &canonical.portals,
        zones: &canonical.zones,
        plugs: &canonical.plugs,
        count_lens: &count_lens,
        count_hashes: &count_hashes,
    })
    .map_err(|e| corrupt(e.to_string()))?;
    let head_len = u32::try_from(head.len()).map_err(|_| corrupt("the canonical head is 4 GiB or more"))?;
    let mut out = postcard::to_allocvec(&canonical.canonical_version).map_err(|e| corrupt(e.to_string()))?;
    out.reserve_exact(4 + head.len() + body.len());
    out.extend_from_slice(&head_len.to_le_bytes());
    out.extend_from_slice(&head);
    out.extend_from_slice(&body);
    Ok(out)
}

/// The parts of a version 2 canonical section, borrowed.
struct SectionV2<'a> {
    /// The hashed prefix: the version, the head length and the head.
    hashed: &'a [u8],
    /// The head's postcard bytes.
    head: &'a [u8],
    /// The count body.
    body: &'a [u8],
}

/// Splits version 2 section bytes at the head's stored length.
fn split_section_v2(bytes: &[u8]) -> Result<SectionV2<'_>, GeomLoadError> {
    let (_, rest) = take_u32(bytes)?;
    let (len, rest) = rest
        .split_first_chunk::<4>()
        .ok_or_else(|| corrupt("canonical section: no head length"))?;
    let head_len = u32::from_le_bytes(*len) as usize;
    if head_len > rest.len() {
        return Err(corrupt("canonical section: the head length runs past the end"));
    }
    let (head, body) = rest.split_at(head_len);
    Ok(SectionV2 {
        hashed: &bytes[..bytes.len() - body.len()],
        head,
        body,
    })
}

/// `content_hash` of version 2 canonical section bytes (spec 121 § 5.2): the BLAKE3 of the hashed
/// prefix exactly as stored (the version, the head length and the head; the head holds the hashes
/// of the count body, which [`decode_canonical`] checks). It reads the prefix only.
///
/// Bytes without a version 2 prefix hash whole; they do not load. A version 1 section's
/// `content_hash` is that of its migrated bytes ([`migrate_canonical`]).
#[must_use]
pub fn content_hash_of_bytes(canonical_bytes: &[u8]) -> [u8; 32] {
    let hashed = match take_u32(canonical_bytes) {
        Ok((version, _)) if version != 1 => {
            split_section_v2(canonical_bytes).map_or(canonical_bytes, |s| s.hashed)
        }
        _ => canonical_bytes,
    };
    *blake3::hash(hashed).as_bytes()
}

/// `content_hash` of a canonical section value (the hash of [`encode_canonical`]'s bytes).
///
/// # Errors
///
/// As [`encode_canonical`].
pub fn content_hash_of(canonical: &WorldGeomCooked) -> Result<[u8; 32], GeomLoadError> {
    encode_canonical(canonical).map(|b| content_hash_of_bytes(&b))
}

/// Reads a leading postcard `u32` (the version), returning it and the rest.
fn take_u32(bytes: &[u8]) -> Result<(u32, &[u8]), GeomLoadError> {
    postcard::take_from_bytes::<u32>(bytes).map_err(|e| corrupt(format!("version: {e}")))
}

/// Reads a postcard byte string without copying it, returning it and the rest.
fn take_bytes<'a>(bytes: &'a [u8], what: &str) -> Result<(&'a [u8], &'a [u8]), GeomLoadError> {
    postcard::take_from_bytes::<&[u8]>(bytes).map_err(|e| corrupt(format!("{what}: {e}")))
}

/// The version of a cooked file, for `regia upgrade-project`: `Ok(None)` when the bytes are not a
/// cooked meshlayers file (no magic).
///
/// # Errors
///
/// The magic is there but the version does not decode.
pub fn cooked_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    match bytes.strip_prefix(&COOKED_MAGIC) {
        None => Ok(None),
        Some(rest) => take_u32(rest).map(|(v, _)| Some(v)).map_err(|e| e.to_string()),
    }
}

/// The parts of a cooked file as stored: the header and the two sections' bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct CookedParts {
    /// The decoded header.
    pub header: MeshLayersHeader,
    /// The canonical section bytes, exactly as stored.
    pub canonical: Vec<u8>,
    /// The presentation section bytes, exactly as stored.
    pub presentation: Vec<u8>,
    /// The chunk section bytes, exactly as stored (empty when the file has none, ADR-8576).
    pub chunks: Vec<u8>,
}

/// [`CookedParts`] borrowed from the file's bytes: what a loader uses, so no section is copied.
#[derive(Clone, Debug, PartialEq)]
pub struct CookedPartsRef<'a> {
    /// The decoded header.
    pub header: MeshLayersHeader,
    /// The canonical section bytes, exactly as stored.
    pub canonical: &'a [u8],
    /// The presentation section bytes, exactly as stored.
    pub presentation: &'a [u8],
    /// The chunk section bytes, exactly as stored (empty when the file has none, ADR-8576).
    pub chunks: &'a [u8],
}

/// Splits a cooked file into its header and the sections' bytes. Refuses a newer file version
/// before decoding anything else, and trailing bytes. The chunk section is optional: a file that
/// ends after the presentation section has none, and a stored empty one is refused (ADR-8576).
///
/// # Errors
///
/// [`GeomLoadError`].
pub fn split_file(bytes: &[u8]) -> Result<CookedParts, GeomLoadError> {
    let parts = split_file_ref(bytes)?;
    Ok(CookedParts {
        header: parts.header,
        canonical: parts.canonical.to_vec(),
        presentation: parts.presentation.to_vec(),
        chunks: parts.chunks.to_vec(),
    })
}

/// [`split_file`] without copying the sections.
///
/// # Errors
///
/// [`GeomLoadError`].
pub fn split_file_ref(bytes: &[u8]) -> Result<CookedPartsRef<'_>, GeomLoadError> {
    let rest = bytes
        .strip_prefix(&COOKED_MAGIC)
        .ok_or_else(|| corrupt("not a cooked meshlayers file (bad magic)"))?;
    let (version, _) = take_u32(rest)?;
    if version > COOKED_FORMAT_VERSION {
        return Err(GeomLoadError::NewerVersion {
            found: version,
            supported: COOKED_FORMAT_VERSION,
        });
    }
    if version == 0 {
        return Err(corrupt("version 0"));
    }
    let (header, rest) =
        postcard::take_from_bytes::<MeshLayersHeader>(rest).map_err(|e| corrupt(format!("header: {e}")))?;
    let (canonical, rest) = take_bytes(rest, "canonical section")?;
    let (presentation, rest) = take_bytes(rest, "presentation section")?;
    let (chunks, rest) = if rest.is_empty() {
        (rest, rest)
    } else {
        let (chunks, rest) = take_bytes(rest, "chunk section")?;
        // An empty chunk section is never stored, so one byte layout per file value.
        if chunks.is_empty() {
            return Err(corrupt("empty chunk section stored"));
        }
        (chunks, rest)
    };
    if !rest.is_empty() {
        return Err(corrupt(format!("{} trailing bytes", rest.len())));
    }
    Ok(CookedPartsRef {
        header,
        canonical,
        presentation,
        chunks,
    })
}

/// Decodes and checks canonical section bytes of version 2, or of version 1 migrated (the value's
/// `canonical_version` is then 2). Refuses a newer `canonical_version` before decoding the rest,
/// trailing bytes, a count-body block that does not match its hash, and any structural problem
/// ([`WorldGeomCooked::check`]).
///
/// # Errors
///
/// [`GeomLoadError`].
pub fn decode_canonical(bytes: &[u8]) -> Result<WorldGeomCooked, GeomLoadError> {
    decode_section(bytes, false, CountBodyCheck::AtLoad).map(|(canonical, _, _)| canonical)
}

/// The check a [`CountBodyCheck::Deferred`] load left out: every block of the count body of
/// version 2 canonical section bytes against the head's hashes. Pure; reads the head and the body
/// once. A version 1 section has nothing deferred (its load decodes every count and hashes the
/// migrated section) and is `Ok`.
///
/// # Errors
///
/// [`GeomLoadError`]: `geom_corrupt` naming the first block that does not match, or what
/// [`decode_canonical`] refuses in the head.
pub fn check_count_body(canonical_bytes: &[u8]) -> Result<(), GeomLoadError> {
    if section_version(canonical_bytes)? == 1 {
        return Ok(());
    }
    let section = split_section_v2(canonical_bytes)?;
    let head = decode_head(&section)?;
    BodyCheck {
        body: section.body,
        hashes: &head.count_hashes,
        next: 0,
    }
    .through(section.body.len())
}

/// The canonical section's leading version, refusing a newer one.
fn section_version(bytes: &[u8]) -> Result<u32, GeomLoadError> {
    let (version, _) = take_u32(bytes)?;
    if version > CANONICAL_VERSION {
        return Err(GeomLoadError::NewerVersion {
            found: version,
            supported: CANONICAL_VERSION,
        });
    }
    Ok(version)
}

/// Decodes and checks a canonical section of either version and returns it with its
/// `content_hash`. With `owned`, each count block comes back holding only the vertices its tile
/// owns (the run time's form, ADR-8246 R7) instead of what the file stores, and a version 2
/// section also brings each block's [`TileFold`].
pub(crate) fn decode_section(
    bytes: &[u8],
    owned: bool,
    check: CountBodyCheck,
) -> Result<DecodedSection, GeomLoadError> {
    if section_version(bytes)? == 1 {
        // The migration step: the value at the current version, hashed as its version 2 bytes.
        let mut canonical = decode_canonical_v1(bytes)?;
        let hash = content_hash_of(&canonical)?;
        if owned {
            crate::ground::compact_border_counts(&mut canonical);
        }
        return Ok((canonical, hash, None));
    }
    decode_canonical_v2(bytes, owned, check)
}

/// The version 1 (v0.6.0) section: `postcard(WorldGeomCooked)` in field order. Returns the value at
/// the current version.
fn decode_canonical_v1(bytes: &[u8]) -> Result<WorldGeomCooked, GeomLoadError> {
    let (mut canonical, rest) = postcard::take_from_bytes::<WorldGeomCooked>(bytes)
        .map_err(|e| corrupt(format!("canonical section: {e}")))?;
    if !rest.is_empty() {
        return Err(corrupt(format!(
            "canonical section: {} trailing bytes",
            rest.len()
        )));
    }
    canonical.canonical_version = CANONICAL_VERSION;
    canonical.check()?;
    Ok(canonical)
}

/// The lowest and highest count of an owned count block and of the parts a neighbor's border
/// reads, folded by the decoder while the block is in the cache (ADR-8816). The height pyramid
/// builds from these and does not read the counts a second time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TileFold {
    /// The owned shape the fold was made for: columns and rows.
    pub(crate) shape: (u32, u32),
    /// Lowest and highest of every count.
    pub(crate) all: (u16, u16),
    /// Lowest and highest of column 0 (the west tile's east border).
    pub(crate) west: (u16, u16),
    /// Lowest and highest of row 0 (the south tile's north border).
    pub(crate) south: (u16, u16),
    /// The first count (the corner of the tile to the south-west).
    pub(crate) first: u16,
}

/// The lowest and highest of `counts` (`(u16::MAX, 0)` for none).
fn low_high(counts: impl Iterator<Item = u16>) -> (u16, u16) {
    let (mut low, mut high) = (u16::MAX, u16::MIN);
    for count in counts {
        low = low.min(count);
        high = high.max(count);
    }
    (low, high)
}

impl TileFold {
    /// The fold of an owned `block` of `cols` by `rows` counts; `None` for another length or an
    /// empty block.
    pub(crate) fn of(block: &[u16], cols: u32, rows: u32) -> Option<Self> {
        let (c, r) = (cols as usize, rows as usize);
        (c > 0 && r > 0 && block.len() == c * r).then(|| Self {
            shape: (cols, rows),
            all: low_high(block.iter().copied()),
            west: low_high(block.iter().step_by(c).copied()),
            south: low_high(block[..c].iter().copied()),
            first: block[0],
        })
    }
}

/// What a run-time decode returns: the section, its `content_hash`, and the fold of every count
/// block when the decode made owned blocks (one `Option` per ground tile).
pub(crate) type DecodedSection = (WorldGeomCooked, [u8; 32], Option<Vec<Option<TileFold>>>);

/// Checks count-body blocks against the head's hashes, in order, as the decoder reaches them, so
/// a block is copied while it is still in the cache.
struct BodyCheck<'a> {
    body: &'a [u8],
    hashes: &'a [[u8; 32]],
    /// The next block to check.
    next: usize,
}

impl BodyCheck<'_> {
    /// Checks every block that starts before byte `end`.
    fn through(&mut self, end: usize) -> Result<(), GeomLoadError> {
        while self.next * COUNT_BLOCK_BYTES < end {
            let start = self.next * COUNT_BLOCK_BYTES;
            let stop = (start + COUNT_BLOCK_BYTES).min(self.body.len());
            let matches = self
                .hashes
                .get(self.next)
                .is_some_and(|hash| blake3::hash(&self.body[start..stop]).as_bytes() == hash);
            if !matches {
                return Err(corrupt(format!(
                    "count body block {} does not match its hash",
                    self.next
                )));
            }
            self.next += 1;
        }
        Ok(())
    }
}

/// Little-endian `u16` counts appended to `out`.
fn push_counts(out: &mut Vec<u16>, bytes: &[u8]) {
    let (pairs, _) = bytes.as_chunks::<2>();
    out.extend(pairs.iter().map(|pair| u16::from_le_bytes(*pair)));
}

/// Decodes a version 2 section's head and checks it against the body's length: `count_lens` sum
/// to the body's counts and there is one hash per body block.
fn decode_head(section: &SectionV2<'_>) -> Result<CanonicalHead, GeomLoadError> {
    let (head, rest) = postcard::take_from_bytes::<CanonicalHead>(section.head)
        .map_err(|e| corrupt(format!("canonical section: {e}")))?;
    if !rest.is_empty() {
        return Err(corrupt(format!(
            "canonical section: {} trailing head bytes",
            rest.len()
        )));
    }
    let body = section.body;
    let counts: u64 = head.count_lens.iter().map(|&n| u64::from(n)).sum();
    if counts.checked_mul(2) != Some(body.len() as u64) {
        return Err(corrupt(format!(
            "canonical section: the count body is {} bytes, the head names {counts} counts",
            body.len()
        )));
    }
    if head.count_hashes.len() != body.len().div_ceil(COUNT_BLOCK_BYTES) {
        return Err(corrupt(format!(
            "canonical section: {} count hashes for a body of {} bytes",
            head.count_hashes.len(),
            body.len()
        )));
    }
    Ok(head)
}

fn decode_canonical_v2(
    bytes: &[u8],
    owned: bool,
    body_check: CountBodyCheck,
) -> Result<DecodedSection, GeomLoadError> {
    let (version, _) = take_u32(bytes)?;
    let section = split_section_v2(bytes)?;
    let head = decode_head(&section)?;
    let body = section.body;
    let mut canonical = WorldGeomCooked {
        canonical_version: version,
        settings: head.settings,
        lattice: head.lattice,
        ground: head.ground,
        tile_levels: head.tile_levels,
        hanging_overrides: head.hanging_overrides,
        ring_pins: head.ring_pins,
        tiles: head.tiles,
        ground_counts: Vec::new(),
        cut_bitmaps: head.cut_bitmaps,
        sections: head.sections,
        portals: head.portals,
        zones: head.zones,
        plugs: head.plugs,
    };
    // Every structural check, with the stored lengths standing for the count blocks: after it each
    // length is the full or the owned size of its tile.
    let lens = &head.count_lens;
    canonical.check_with_counts(lens.len(), |t| lens[t] as usize)?;
    let l = canonical.lattice;
    let per_side = l.tiles_per_side();
    let mut check = BodyCheck {
        body,
        hashes: &head.count_hashes,
        // A deferred check starts past the last block: `through` then has nothing to do.
        next: match body_check {
            CountBodyCheck::AtLoad => 0,
            CountBodyCheck::Deferred => head.count_hashes.len(),
        },
    };
    let mut blocks = Vec::with_capacity(lens.len());
    let mut folds = owned.then(|| Vec::with_capacity(lens.len()));
    let mut at = 0usize;
    for (t, &len) in lens.iter().enumerate() {
        let end = at + len as usize * 2;
        check.through(end)?;
        let stored = &body[at..end];
        at = end;
        let (a, b) = ((t as u32) % per_side, (t as u32) / per_side);
        let level = canonical.tile_levels[t];
        // What the tile owns: all but its east and north borders, which the next tile stores (the
        // map's own edge stays). `check_with_counts` passed, so the sides exist.
        let shape = match (l.tile_side_vertices(a, level), l.tile_side_vertices(b, level)) {
            (Some(sx), Some(sz)) if owned => {
                let owned_x = if a + 1 == per_side { sx } else { sx - 1 };
                let owned_z = if b + 1 == per_side { sz } else { sz - 1 };
                Some((sx, owned_x, owned_z))
            }
            _ => None,
        };
        let block = match shape {
            Some((sx, owned_x, owned_z)) if (owned_x * owned_z) as usize != len as usize => {
                let mut block = Vec::with_capacity((owned_x * owned_z) as usize);
                let (row_bytes, keep) = (sx as usize * 2, owned_x as usize * 2);
                for row in stored.chunks_exact(row_bytes).take(owned_z as usize) {
                    push_counts(&mut block, &row[..keep]);
                }
                block
            }
            _ => {
                let mut block = Vec::with_capacity(len as usize);
                push_counts(&mut block, stored);
                block
            }
        };
        if let Some(folds) = &mut folds {
            // Folded here, while the block is in the cache: the pyramid then reads no count.
            folds.push(shape.and_then(|(_, owned_x, owned_z)| TileFold::of(&block, owned_x, owned_z)));
        }
        blocks.push(block);
    }
    check.through(body.len())?;
    canonical.ground_counts = blocks;
    Ok((canonical, *blake3::hash(section.hashed).as_bytes(), folds))
}

/// The migration step of the canonical section: version 1 (v0.6.0) bytes to the current layout.
/// The value is unchanged; current bytes come back re-encoded.
///
/// # Errors
///
/// [`GeomLoadError`] when the bytes do not decode or fail a check.
pub fn migrate_canonical(bytes: &[u8]) -> Result<Vec<u8>, GeomLoadError> {
    encode_canonical(&decode_canonical(bytes)?)
}

/// The migration step of a whole cooked file (`regia upgrade-project`): a version 1 (v0.6.0) file
/// to the current version. The header's version and the canonical section's layout move; the
/// presentation and chunk sections are kept byte for byte.
///
/// # Errors
///
/// [`GeomLoadError`] when the file does not decode or fails a check.
pub fn migrate_file(bytes: &[u8]) -> Result<Vec<u8>, GeomLoadError> {
    CookedMeshLayers::decode(bytes)?.encode()
}

/// Whether two settings values have the same bytes (bit equality, so `NaN` and `-0.0` compare by
/// bits).
fn same_bits(a: &GeomSettings, b: &GeomSettings) -> bool {
    postcard::to_allocvec(a).ok() == postcard::to_allocvec(b).ok()
}

/// The file rule that the two versions move together: a version 1 file holds a version 1
/// canonical section, a version 2 file a version 2 one (one byte layout per file value).
pub(crate) fn check_section_version(
    header: &MeshLayersHeader,
    canonical: &[u8],
) -> Result<(), GeomLoadError> {
    let section = section_version(canonical)?;
    if (header.version == 1) != (section == 1) {
        return Err(corrupt(format!(
            "file version {} holds a version {section} canonical section",
            header.version
        )));
    }
    Ok(())
}

impl CookedMeshLayers {
    /// Encodes the file.
    ///
    /// # Errors
    ///
    /// As [`encode_canonical`].
    pub fn encode(&self) -> Result<Vec<u8>, GeomLoadError> {
        let canonical = encode_canonical(&self.canonical)?;
        let mut out = COOKED_MAGIC.to_vec();
        let mut push = |r: Result<Vec<u8>, postcard::Error>| -> Result<(), GeomLoadError> {
            out.extend_from_slice(&r.map_err(|e| corrupt(e.to_string()))?);
            Ok(())
        };
        push(postcard::to_allocvec(&self.header))?;
        push(postcard::to_allocvec(&canonical))?;
        push(postcard::to_allocvec(&self.presentation))?;
        if !self.chunks.is_empty() {
            push(postcard::to_allocvec(&self.chunks))?;
        }
        Ok(out)
    }

    /// Decodes and checks a whole file: the version first, then the header, the canonical section
    /// ([`decode_canonical`]) and the header copies against it. A version 1 (v0.6.0) file comes
    /// back migrated: its header and canonical section at the current version.
    ///
    /// # Errors
    ///
    /// [`GeomLoadError`].
    pub fn decode(bytes: &[u8]) -> Result<Self, GeomLoadError> {
        let parts = split_file_ref(bytes)?;
        check_section_version(&parts.header, parts.canonical)?;
        let canonical = decode_canonical(parts.canonical)?;
        check_header_copies(&parts.header, &canonical)?;
        let mut header = parts.header;
        header.version = COOKED_FORMAT_VERSION;
        Ok(Self {
            header,
            canonical,
            presentation: parts.presentation.to_vec(),
            chunks: parts.chunks.to_vec(),
        })
    }
}

/// The loader's `header_mismatch` rule: the header's settings and ground-mode copies equal the
/// canonical section's.
///
/// # Errors
///
/// [`GeomLoadError::Corrupt`] naming the copy that differs.
pub fn check_header_copies(
    header: &MeshLayersHeader,
    canonical: &WorldGeomCooked,
) -> Result<(), GeomLoadError> {
    if !same_bits(&header.settings, &canonical.settings) {
        return Err(corrupt("header settings copy differs from the canonical section"));
    }
    if header.ground != canonical.ground {
        return Err(corrupt(
            "header ground mode copy differs from the canonical section",
        ));
    }
    Ok(())
}

fn strictly_ascending(list: &[VertexHeightMm], what: &str, vertices: u64) -> Result<(), GeomLoadError> {
    for w in list.windows(2) {
        if w[0].vertex >= w[1].vertex {
            return Err(corrupt(format!("{what} not strictly ascending")));
        }
    }
    if list.iter().any(|v| u64::from(v.vertex) >= vertices) {
        return Err(corrupt(format!("{what} vertex out of range")));
    }
    Ok(())
}

fn in_extent(v: i64) -> bool {
    v.abs() < MAX_EXTENT_MM
}

impl WorldGeomCooked {
    /// Structural checks the loader runs, so every later index is in range: the versions, the
    /// settings normalized, the lattice constants, one level per ground tile (all 0 for `Full`, at
    /// most `max_level` and dividing the tile for `Nested`), the count blocks' sizes, sorted and
    /// in-range overrides and pins, tile origins on the tile grid and ascending, triangles sorted with
    /// unique `(layer, index)` and coordinates within 2^24 mm, cut bitmaps sized to their tiles, and
    /// dense, in-range section, portal and plug ids.
    ///
    /// # Errors
    ///
    /// [`GeomLoadError::Corrupt`] or [`GeomLoadError::ExtentTooLarge`].
    pub fn check(&self) -> Result<(), GeomLoadError> {
        self.check_with_counts(self.ground_counts.len(), |t| self.ground_counts[t].len())
    }

    /// [`WorldGeomCooked::check`] with the count blocks given by their number and lengths, so the
    /// version 2 decoder checks a section before it copies a count.
    fn check_with_counts(&self, blocks: usize, len_of: impl Fn(usize) -> usize) -> Result<(), GeomLoadError> {
        if self.canonical_version != CANONICAL_VERSION {
            return Err(corrupt(format!("canonical_version {}", self.canonical_version)));
        }
        match self.settings.normalized() {
            Ok(n) if same_bits(&n, &self.settings) => {}
            Ok(_) => return Err(corrupt("settings carry -0.0")),
            Err(e) => return Err(corrupt(e.to_string())),
        }
        let l = &self.lattice;
        if l.lattice_n < 2 {
            return Err(corrupt("lattice_n below 2"));
        }
        if l.width_mm <= 0 {
            return Err(corrupt("width_mm not positive"));
        }
        if !in_extent(i64::from(l.width_mm) / 2 + 1) {
            return Err(GeomLoadError::ExtentTooLarge);
        }
        let g = l.ground_tile_cells;
        if !g.is_power_of_two() || !(16..=256).contains(&g) {
            return Err(corrupt("ground_tile_cells not a power of two from 16 to 256"));
        }
        if !l.offset_m().is_finite() || !l.step_m().is_finite() {
            return Err(corrupt("offset_m or step_m not finite"));
        }
        let per_side = l.tiles_per_side();
        let tiles = u64::from(per_side) * u64::from(per_side);
        if self.tile_levels.len() as u64 != tiles || blocks as u64 != tiles {
            return Err(corrupt(format!(
                "{} tile levels and {blocks} count blocks for {tiles} ground tiles",
                self.tile_levels.len()
            )));
        }
        for (t, &level) in self.tile_levels.iter().enumerate() {
            let counts = len_of(t) as u64;
            match self.ground {
                GroundMode::Full if level != 0 => {
                    return Err(corrupt(format!("ground tile {t} has level {level} in Full mode")));
                }
                GroundMode::Nested { max_level } if level > max_level => {
                    return Err(corrupt(format!(
                        "ground tile {t} level {level} over max_level {max_level}"
                    )));
                }
                _ => {}
            }
            let (a, b) = ((t as u32) % per_side, (t as u32) / per_side);
            let (Some(sx), Some(sz)) = (l.tile_side_vertices(a, level), l.tile_side_vertices(b, level))
            else {
                return Err(corrupt(format!("ground tile {t} cannot take level {level}")));
            };
            // The file stores every border. The loaded runtime may already have dropped the east
            // and north borders (ADR-8246 R7). Both lengths are valid.
            let owned_x = if a + 1 == per_side { sx } else { sx - 1 };
            let owned_z = if b + 1 == per_side { sz } else { sz - 1 };
            let full = u64::from(sx) * u64::from(sz);
            let owned = u64::from(owned_x) * u64::from(owned_z);
            if counts != full && counts != owned {
                return Err(corrupt(format!(
                    "ground tile {t} has {counts} counts, not {full} or {owned}"
                )));
            }
        }
        let vertices = u64::from(l.lattice_n) * u64::from(l.lattice_n);
        strictly_ascending(&self.hanging_overrides, "hanging overrides", vertices)?;
        strictly_ascending(&self.ring_pins, "ring pins", vertices)?;
        let tile_mm = crate::units::TILE_MM;
        let mut seen = std::collections::BTreeSet::new();
        for w in self.tiles.windows(2) {
            let (p, q) = (w[0].origin_mm, w[1].origin_mm);
            if (p[1], p[0]) >= (q[1], q[0]) {
                return Err(corrupt("tiles not in ascending (z, x) order"));
            }
        }
        for tile in &self.tiles {
            let [ox, oz] = tile.origin_mm;
            if ox % tile_mm != 0 || oz % tile_mm != 0 {
                return Err(corrupt("tile origin off the tile grid"));
            }
            for w in tile.triangles.windows(2) {
                if (w[0].layer, w[0].index) >= (w[1].layer, w[1].index) {
                    return Err(corrupt("tile triangles not sorted by (layer, index)"));
                }
            }
            for tri in &tile.triangles {
                if !seen.insert((tri.layer, tri.index)) {
                    return Err(corrupt(format!(
                        "triangle ({}, {}) stored twice",
                        tri.layer, tri.index
                    )));
                }
                if tri.flags & !TRI_FLAGS_ALL != 0 {
                    return Err(corrupt("unknown triangle flag"));
                }
                if let Some(s) = tri.section
                    && usize::from(s) >= self.sections.len()
                {
                    return Err(corrupt("triangle section out of range"));
                }
                for v in tri.v {
                    let x = i64::from(ox) + i64::from(v[0]);
                    let z = i64::from(oz) + i64::from(v[2]);
                    if !in_extent(x) || !in_extent(z) || !in_extent(i64::from(v[1])) {
                        return Err(GeomLoadError::ExtentTooLarge);
                    }
                }
            }
        }
        for w in self.cut_bitmaps.windows(2) {
            if w[0].tile >= w[1].tile {
                return Err(corrupt("cut bitmaps not ascending"));
            }
        }
        for cut in &self.cut_bitmaps {
            if u64::from(cut.tile) >= tiles {
                return Err(corrupt("cut bitmap tile out of range"));
            }
            let (a, b) = (cut.tile % per_side, cut.tile / per_side);
            let (x0, x1) = l.tile_cells(a);
            let (z0, z1) = l.tile_cells(b);
            let bits = 2 * u64::from(x1 - x0) * u64::from(z1 - z0);
            if cut.bits.len() as u64 != bits.div_ceil(64) {
                return Err(corrupt(format!(
                    "cut bitmap of tile {} has the wrong size",
                    cut.tile
                )));
            }
        }
        let sections = self.sections.len();
        let portals = self.portals.len();
        let plugs = self.plugs.len();
        for (i, s) in self.sections.iter().enumerate() {
            if usize::from(s.id) != i {
                return Err(corrupt("section ids not dense"));
            }
            if s.portals.iter().any(|&p| usize::from(p) >= portals)
                || s.blocker.is_some_and(|b| usize::from(b) >= plugs)
            {
                return Err(corrupt("section portal or blocker out of range"));
            }
        }
        for (i, p) in self.portals.iter().enumerate() {
            if usize::from(p.id) != i {
                return Err(corrupt("portal ids not dense"));
            }
            if p.sides.iter().flatten().any(|&s| usize::from(s) >= sections)
                || p.blocker.is_some_and(|b| usize::from(b) >= plugs)
            {
                return Err(corrupt("portal side or blocker out of range"));
            }
        }
        if self.zones.iter().any(|z| usize::from(z.section) >= sections) {
            return Err(corrupt("zone section out of range"));
        }
        for (i, p) in self.plugs.iter().enumerate() {
            if usize::from(p.id) != i || usize::from(p.section) >= sections {
                return Err(corrupt("plug id not dense or section out of range"));
            }
        }
        Ok(())
    }

    /// Every canonical triangle in `(layer, index)` order with its tile origin.
    #[must_use]
    pub fn triangles_sorted(&self) -> Vec<([i32; 2], CookedTriangle)> {
        let mut all: Vec<([i32; 2], CookedTriangle)> = self
            .tiles
            .iter()
            .flat_map(|t| t.triangles.iter().map(move |tri| (t.origin_mm, *tri)))
            .collect();
        all.sort_by_key(|(_, t)| (t.layer, t.index));
        all
    }
}
