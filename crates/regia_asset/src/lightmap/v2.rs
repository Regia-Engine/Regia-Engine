// SPDX-License-Identifier: MPL-2.0
//! `.flmap` version 2 (ADR-1556): the entry and header shapes before the entity key and the seam
//! figures (ADR-2024). Postcard is not self-describing, so a version 1 or 2 file decodes with
//! these structs and comes back in the current shape, with [`super::LightmapEntry::key`] and
//! [`super::LightmapEntry::seams`] empty. Frozen: never change these structs.

use serde::Deserialize;

use super::{LIGHTMAP_SECTIONS, LightmapBakeInfo, LightmapEntry, LightmapHeader};
use crate::baked_gi::SectionRef;

/// One entry of a version 1 or 2 `.flmap`, field for field.
#[derive(Deserialize)]
pub(super) struct LightmapEntryV2 {
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
}

impl From<LightmapEntryV2> for LightmapEntry {
    fn from(e: LightmapEntryV2) -> Self {
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
            key: None,
            seams: None,
            page: 0,
        }
    }
}

/// The postcard header of a version 2 `.flmap`, field for field.
#[derive(Deserialize)]
pub(super) struct LightmapHeaderV2 {
    width: u32,
    height: u32,
    texel_encoding: u32,
    exposure: f32,
    density: f32,
    padding: u32,
    entries: Vec<LightmapEntryV2>,
    bake: LightmapBakeInfo,
    direction_encoding: u32,
    sections: [SectionRef; LIGHTMAP_SECTIONS],
    payload_hash: [u8; 32],
}

impl From<LightmapHeaderV2> for LightmapHeader {
    fn from(h: LightmapHeaderV2) -> Self {
        Self {
            width: h.width,
            height: h.height,
            texel_encoding: h.texel_encoding,
            exposure: h.exposure,
            density: h.density,
            padding: h.padding,
            entries: h.entries.into_iter().map(Into::into).collect(),
            bake: h.bake,
            direction_encoding: h.direction_encoding,
            sections: h.sections,
            meshlet_section: SectionRef::default(),
            meshlet_bytes: Vec::new(),
            payload_hash: h.payload_hash,
            denoise: None,
            source_hashes: Vec::new(),
            pages: 1,
        }
    }
}
