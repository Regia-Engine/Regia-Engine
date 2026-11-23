// SPDX-License-Identifier: MPL-2.0
//! `.flmap` version 6 (ADR-2656): the entry and header shapes before atlas pages (ADR-3300).
//! Postcard is not self-describing, so a version 3 to 6 file decodes its entries with
//! [`LightmapEntryV6`] and a version 6 header with [`LightmapHeaderV6`], and both come back in the
//! current shape as one page: every entry on page 0. Frozen: never change these structs.

use serde::Deserialize;

use super::{
    LIGHTMAP_SECTIONS, LightmapBakeInfo, LightmapDenoiseInfo, LightmapEntry, LightmapEntryKey,
    LightmapHeader, LightmapSeams,
};
use crate::baked_gi::SectionRef;

/// One entry of a version 3 to 6 `.flmap`, field for field.
#[derive(Deserialize)]
pub(super) struct LightmapEntryV6 {
    name: String,
    source_vertices: u32,
    rect: [u32; 4],
    scale_offset: [f32; 4],
    vertex_count: u32,
    index_count: u32,
    first_vertex: u32,
    first_index: u32,
    charts: u32,
    area_m2: f32,
    texels_per_metre: f32,
    key: Option<LightmapEntryKey>,
    seams: Option<LightmapSeams>,
}

impl From<LightmapEntryV6> for LightmapEntry {
    fn from(e: LightmapEntryV6) -> Self {
        Self {
            name: e.name,
            source_vertices: e.source_vertices,
            rect: e.rect,
            scale_offset: e.scale_offset,
            vertex_count: e.vertex_count,
            index_count: e.index_count,
            first_vertex: e.first_vertex,
            first_index: e.first_index,
            charts: e.charts,
            area_m2: e.area_m2,
            texels_per_metre: e.texels_per_metre,
            key: e.key,
            seams: e.seams,
            page: 0,
        }
    }
}

/// The entries of an older header in the current shape (all on page 0).
pub(super) fn entries(old: Vec<LightmapEntryV6>) -> Vec<LightmapEntry> {
    old.into_iter().map(Into::into).collect()
}

/// The postcard header of a version 6 `.flmap`, field for field.
#[derive(Deserialize)]
pub(super) struct LightmapHeaderV6 {
    width: u32,
    height: u32,
    texel_encoding: u32,
    exposure: f32,
    density: f32,
    padding: u32,
    entries: Vec<LightmapEntryV6>,
    bake: LightmapBakeInfo,
    direction_encoding: u32,
    sections: [SectionRef; LIGHTMAP_SECTIONS],
    meshlet_section: SectionRef,
    meshlet_bytes: Vec<u64>,
    payload_hash: [u8; 32],
    denoise: Option<LightmapDenoiseInfo>,
    source_hashes: Vec<u64>,
}

impl From<LightmapHeaderV6> for LightmapHeader {
    fn from(h: LightmapHeaderV6) -> Self {
        Self {
            width: h.width,
            height: h.height,
            texel_encoding: h.texel_encoding,
            exposure: h.exposure,
            density: h.density,
            padding: h.padding,
            entries: entries(h.entries),
            bake: h.bake,
            direction_encoding: h.direction_encoding,
            sections: h.sections,
            meshlet_section: h.meshlet_section,
            meshlet_bytes: h.meshlet_bytes,
            payload_hash: h.payload_hash,
            denoise: h.denoise,
            source_hashes: h.source_hashes,
            pages: 1,
        }
    }
}
