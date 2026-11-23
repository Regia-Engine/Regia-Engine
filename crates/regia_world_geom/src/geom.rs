// SPDX-License-Identifier: MPL-2.0
//! `WorldGeom`: the cooked, immutable world geometry of one map, loaded from the canonical section
//! of a cooked `<map>.meshlayers` file (spec 121 § 5.1, § 5.2).
//!
//! This lane (W-06) builds the load half: the decode, the checks, the `content_hash` the match
//! handshake compares, the settings, and the canonical triangles converted once to `f32` metres
//! with the pinned `mm_to_m`. The query index and the queries (`support`, `surface_at`, the wall
//! push, segments) are lane W-11's, on this same type.

use crate::cooked::{
    CookedTriangle, CountBodyCheck, GeomLoadError, TriKind, WorldGeomCooked, check_header_copies,
    check_section_version, decode_section, split_file_ref,
};
use crate::settings::GeomSettings;
use crate::units::mm_to_m;

/// A triangle reference: the layer and the triangle's index in that layer's canonical (source)
/// order. Total order = candidate order. The cooked tile is not part of it, so no answer depends on
/// the tiling (DO27 sign-off E3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TriRef {
    /// The layer.
    pub layer: u16,
    /// The triangle's index in the layer's canonical order.
    pub triangle: u32,
}

/// One canonical triangle at run time: `f32` world metres, converted once at load.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeomTriangle {
    /// The vertices `[x, y, z]`, metres.
    pub v: [[f32; 3]; 3],
    /// Its reference.
    pub tri: TriRef,
    /// Floor, roof, wall or blocker.
    pub kind: TriKind,
    /// `cooked::TRI_FLAG_*` bits.
    pub flags: u8,
    /// Its section.
    pub section: Option<u16>,
    /// Footstep material slot.
    pub material: u16,
}

impl GeomTriangle {
    fn from_cooked(origin: [i32; 2], t: &CookedTriangle) -> Self {
        let v = t.v.map(|p| {
            [
                mm_to_m(origin[0], p[0]),
                mm_to_m(0, p[1]),
                mm_to_m(origin[1], p[2]),
            ]
        });
        Self {
            v,
            tri: TriRef {
                layer: t.layer,
                triangle: t.index,
            },
            kind: t.kind,
            flags: t.flags,
            section: t.section,
            material: t.material,
        }
    }
}

/// The cooked, immutable world geometry of one map. Built by [`WorldGeom::from_cooked`]; shared
/// between threads (`Send + Sync`); never mutated after load.
#[derive(Clone, Debug)]
pub struct WorldGeom {
    cooked: WorldGeomCooked,
    content_hash: [u8; 32],
    #[cfg(feature = "stage_b")]
    pub(crate) query_index: crate::query_index::QueryIndex,
    #[cfg(feature = "stage_b")]
    pub(crate) height_pyramid: crate::query_pyramid::HeightPyramid,
    #[cfg(feature = "stage_b")]
    pub(crate) ground_coordinates: crate::query_ground::GroundCoordinates,
    /// The segment and disc-push tables (plan row W-11, ADR-8815).
    #[cfg(feature = "stage_b")]
    pub(crate) seg_index: crate::query_seg_index::SegIndex,
}

impl WorldGeom {
    /// Decodes the canonical section of a cooked `<map>.meshlayers` file. The header is read only to
    /// refuse a newer file and to check its copies; the presentation section is never decoded and
    /// no section is copied. A version 1 (v0.6.0) file loads through the migration step
    /// (`cooked::migrate_canonical`; slower, same answers and the same `content_hash` as the map
    /// cooked at the current version). Allocates; load time only.
    ///
    /// # Errors
    ///
    /// [`GeomLoadError`]: `geom_newer_version` for a newer file or canonical section,
    /// `geom_corrupt`, `geom_extent_too_large`.
    pub fn from_cooked(bytes: &[u8]) -> Result<WorldGeom, GeomLoadError> {
        Self::from_cooked_with(bytes, CountBodyCheck::AtLoad)
    }

    /// [`WorldGeom::from_cooked`] with the count-body check at the caller's choice (ADR-8816).
    /// With [`CountBodyCheck::Deferred`] the load leaves out the hashes of the count body (about a
    /// third of a 2.56 km map's load); the caller then owes `cooked::check_count_body` on the
    /// file's canonical section (`cooked::split_file_ref`) before the map joins a match.
    ///
    /// # Errors
    ///
    /// As [`WorldGeom::from_cooked`].
    pub fn from_cooked_with(bytes: &[u8], check: CountBodyCheck) -> Result<WorldGeom, GeomLoadError> {
        let parts = split_file_ref(bytes)?;
        check_section_version(&parts.header, parts.canonical)?;
        let geom = Self::from_canonical_bytes_with(parts.canonical, check)?;
        check_header_copies(&parts.header, &geom.cooked)?;
        Ok(geom)
    }

    /// Builds from the canonical section's bytes alone (what a headless server reads).
    ///
    /// # Errors
    ///
    /// As [`WorldGeom::from_cooked`].
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<WorldGeom, GeomLoadError> {
        Self::from_canonical_bytes_with(bytes, CountBodyCheck::AtLoad)
    }

    /// [`WorldGeom::from_canonical_bytes`] with the count-body check at the caller's choice
    /// (ADR-8816): see [`WorldGeom::from_cooked_with`]. The geometry, the answers and
    /// `content_hash` are the same either way.
    ///
    /// # Errors
    ///
    /// As [`WorldGeom::from_cooked`].
    pub fn from_canonical_bytes_with(
        bytes: &[u8],
        check: CountBodyCheck,
    ) -> Result<WorldGeom, GeomLoadError> {
        let _span = tracing::info_span!("world_geom_load", bytes = bytes.len()).entered();
        // One pass over the count body: each block is checked against its hash and copied, already
        // without the borders the run time does not keep (ADR-8246 R7, ADR-8816). The section is
        // checked once, inside the decode.
        let (cooked, content_hash, folds) = decode_section(bytes, true, check)?;
        // One triangle store: the canonical millimetre tiles. The query index names each triangle
        // as `tile << 16 | local` in `(layer, index)` order (ADR-8246 R7). The f32 form is built
        // per candidate, not kept beside the cooked copy.
        #[cfg(feature = "stage_b")]
        let (query_index, seg_index) = {
            let mut order = Vec::new();
            for (tile_i, tile) in cooked.tiles.iter().enumerate() {
                for (local, tri) in tile.triangles.iter().enumerate() {
                    if tile_i > u16::MAX as usize || local > u16::MAX as usize {
                        return Err(GeomLoadError::Corrupt(
                            "a cooked tile has more triangles than the query locator holds".into(),
                        ));
                    }
                    let id = ((tile_i as u32) << 16) | (local as u32);
                    if id == u32::MAX {
                        return Err(GeomLoadError::Corrupt(
                            "a packed triangle id collides with the ground sentinel".into(),
                        ));
                    }
                    order.push((tri.layer, tri.index, id));
                }
            }
            order.sort_by_key(|row| (row.0, row.1));
            let ids: Vec<u32> = order.iter().map(|row| row.2).collect();
            let triangles: Vec<GeomTriangle> = order
                .iter()
                .map(|row| {
                    let tile_i = (row.2 >> 16) as usize;
                    let local = (row.2 & 0xffff) as usize;
                    let tile = &cooked.tiles[tile_i];
                    GeomTriangle::from_cooked(tile.origin_mm, &tile.triangles[local])
                })
                .collect();
            let query_index = crate::query_index::QueryIndex::build(&triangles, &ids, &cooked.settings)?;
            let seg_index = crate::query_seg_index::SegIndex::build(&triangles, &ids, &cooked)?;
            (query_index, seg_index)
        };
        #[cfg(feature = "stage_b")]
        let height_pyramid = crate::query_pyramid::HeightPyramid::build(&cooked, folds.as_deref());
        #[cfg(not(feature = "stage_b"))]
        drop(folds);
        #[cfg(feature = "stage_b")]
        let ground_coordinates = crate::query_ground::GroundCoordinates::new(&cooked);
        Ok(WorldGeom {
            cooked,
            content_hash,
            #[cfg(feature = "stage_b")]
            query_index,
            #[cfg(feature = "stage_b")]
            height_pyramid,
            #[cfg(feature = "stage_b")]
            ground_coordinates,
            #[cfg(feature = "stage_b")]
            seg_index,
        })
    }

    /// The BLAKE3 hash of the canonical section's hashed prefix exactly as stored, its version
    /// field first: what the match handshake compares. The prefix holds the hashes of the count
    /// body, which the load checked (or the caller owes, after a [`CountBodyCheck::Deferred`]
    /// load), so it covers every byte a query, an edit, a blocker flip or a fresh state reads (E6;
    /// ADR-8084, review W1; ADR-8816).
    #[must_use]
    pub fn content_hash(&self) -> [u8; 32] {
        self.content_hash
    }

    /// The settings cooked into the file.
    #[must_use]
    pub fn settings(&self) -> &GeomSettings {
        &self.cooked.settings
    }

    /// The cooked sections, in id order (spec 121 § 5.6; lane W-08).
    #[must_use]
    pub fn sections(&self) -> &[crate::cooked::CookedSection] {
        &self.cooked.sections
    }

    /// The cooked portals, in id order (spec 121 § 5.6).
    #[must_use]
    pub fn portals(&self) -> &[crate::cooked::CookedPortal] {
        &self.cooked.portals
    }

    /// The section zones, one per section in section order (spec 121 § 5.6).
    #[must_use]
    pub fn zones(&self) -> &[crate::cooked::CookedZone] {
        &self.cooked.zones
    }

    /// The cooked blocker plugs, in id order (spec 121 § 5.7).
    #[must_use]
    pub fn blockers(&self) -> &[crate::cooked::CookedPlug] {
        &self.cooked.plugs
    }

    /// The canonical section as decoded.
    #[must_use]
    pub fn cooked(&self) -> &WorldGeomCooked {
        &self.cooked
    }

    /// Every canonical triangle in `TriRef` order, in `f32` metres.
    ///
    /// This builds a fresh vec for tools and tests. The sim path uses [`WorldGeom::tri`] and does
    /// not allocate.
    #[must_use]
    pub fn triangles(&self) -> Vec<GeomTriangle> {
        self.cooked
            .triangles_sorted()
            .iter()
            .map(|(origin, tri)| GeomTriangle::from_cooked(*origin, tri))
            .collect()
    }

    /// The triangle named by a query-index id (`tile << 16 | local`), in `f32` metres.
    #[must_use]
    pub fn tri(&self, id: u32) -> GeomTriangle {
        let tile = &self.cooked.tiles[(id >> 16) as usize];
        GeomTriangle::from_cooked(tile.origin_mm, &tile.triangles[(id & 0xffff) as usize])
    }

    /// Pieces of [`WorldGeom::resident_bytes`], in order: lattice counts, cooked triangles,
    /// runtime triangle mirror (always zero; f32 triangles are rebuilt per candidate), query
    /// index (with the segment tables of ADR-8815; the column directory included), height pyramid,
    /// ground axis table and tile flags (ADR-8817).
    #[must_use]
    pub fn resident_parts(&self) -> [u64; 6] {
        use std::mem::size_of;
        let c = &self.cooked;
        let counts = c
            .ground_counts
            .iter()
            .map(|b| b.capacity() * size_of::<u16>())
            .sum::<usize>() as u64;
        let cooked_tris = c
            .tiles
            .iter()
            .map(|t| t.triangles.capacity() * size_of::<CookedTriangle>())
            .sum::<usize>() as u64;
        let runtime = 0u64;
        #[cfg(feature = "stage_b")]
        let (index, pyramid, axis) = (
            (self.query_index.resident_bytes() + self.seg_index.resident_bytes()) as u64,
            self.height_pyramid.resident_bytes() as u64,
            self.ground_coordinates.resident_bytes() as u64,
        );
        #[cfg(not(feature = "stage_b"))]
        let (index, pyramid, axis) = (0, 0, 0);
        [counts, cooked_tris, runtime, index, pyramid, axis]
    }

    /// Diagnostics: the segment tables' sizes (boxes, directory, cells, entries, skew walls, axis,
    /// plain bits, tile starts; bytes) and the largest wall reach factors.
    #[cfg(all(feature = "stage_b", any(test, feature = "stats")))]
    #[must_use]
    pub fn segment_table_parts(&self) -> ([usize; 8], [f32; 2]) {
        (self.seg_index.part_bytes(), self.seg_index.wall_reach())
    }

    /// Resident bytes of the geometry and the lattice (for the memory budget and
    /// `terrain.bake_report`).
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        use std::mem::size_of;
        let c = &self.cooked;
        let tris = c
            .tiles
            .iter()
            .map(|t| t.triangles.capacity() * size_of::<CookedTriangle>())
            .sum::<usize>();
        let counts: usize = c
            .ground_counts
            .iter()
            .map(|b| b.capacity() * size_of::<u16>())
            .sum();
        let overrides = (c.hanging_overrides.capacity() + c.ring_pins.capacity())
            * size_of::<crate::cooked::VertexHeightMm>();
        let cuts: usize = c
            .cut_bitmaps
            .iter()
            .map(|b| b.bits.capacity() * size_of::<u64>())
            .sum();
        let bytes = (size_of::<Self>() + tris + counts + overrides + cuts + c.tile_levels.capacity()) as u64;
        #[cfg(feature = "stage_b")]
        let bytes = bytes
            + self.query_index.resident_bytes() as u64
            + self.seg_index.resident_bytes() as u64
            + self.height_pyramid.resident_bytes() as u64
            + self.ground_coordinates.resident_bytes() as u64;
        bytes
    }
}

/// The run-time query structures' share of [`WorldGeom::resident_bytes`] for a canonical section
/// (`resident_parts` index, pyramid and axis table): built from `canonical_bytes` exactly as a load
/// builds them, so the cook's spec 121 § 11 estimate counts them as they are (ADR-10394). Zero
/// without `stage_b`, where a load builds none. The count body's hashes are left to the load.
///
/// # Errors
///
/// As [`WorldGeom::from_canonical_bytes`].
pub(crate) fn query_resident_bytes(canonical_bytes: &[u8]) -> Result<u64, GeomLoadError> {
    #[cfg(feature = "stage_b")]
    {
        let geom = WorldGeom::from_canonical_bytes_with(canonical_bytes, CountBodyCheck::Deferred)?;
        let [_, _, _, index, pyramid, axis] = geom.resident_parts();
        Ok(index + pyramid + axis)
    }
    #[cfg(not(feature = "stage_b"))]
    {
        let _ = canonical_bytes;
        Ok(0)
    }
}
