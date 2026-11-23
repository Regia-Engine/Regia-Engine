// SPDX-License-Identifier: MPL-2.0
//! `.flmap` version 4 (ADR-2107): the header shape before the denoise record and the block
//! encodings (ADR-2314). Postcard is not self-describing, so a version 4 file decodes with this
//! struct and comes back in the current shape with no denoise record. Entries did not change.
//! Frozen: never change this struct.

use serde::Deserialize;

use super::{LIGHTMAP_SECTIONS, LightmapBakeInfo, LightmapHeader};
use crate::baked_gi::SectionRef;

/// The postcard header of a version 4 `.flmap`, field for field.
#[derive(Deserialize)]
pub(super) struct LightmapHeaderV4 {
    width: u32,
    height: u32,
    texel_encoding: u32,
    exposure: f32,
    density: f32,
    padding: u32,
    entries: Vec<super::v6::LightmapEntryV6>,
    bake: LightmapBakeInfo,
    direction_encoding: u32,
    sections: [SectionRef; LIGHTMAP_SECTIONS],
    meshlet_section: SectionRef,
    meshlet_bytes: Vec<u64>,
    payload_hash: [u8; 32],
}

impl From<LightmapHeaderV4> for LightmapHeader {
    fn from(h: LightmapHeaderV4) -> Self {
        Self {
            width: h.width,
            height: h.height,
            texel_encoding: h.texel_encoding,
            exposure: h.exposure,
            density: h.density,
            padding: h.padding,
            entries: super::v6::entries(h.entries),
            bake: h.bake,
            direction_encoding: h.direction_encoding,
            sections: h.sections,
            meshlet_section: h.meshlet_section,
            meshlet_bytes: h.meshlet_bytes,
            payload_hash: h.payload_hash,
            denoise: None,
            source_hashes: Vec::new(),
            pages: 1,
        }
    }
}
