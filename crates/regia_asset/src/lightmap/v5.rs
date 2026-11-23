// SPDX-License-Identifier: MPL-2.0
//! `.flmap` version 5 (ADR-2314, ADR-2635): the header shape before the source hashes (ADR-2656).
//! Postcard is not self-describing, so a version 5 file decodes with this struct and comes back in
//! the current shape with no source hash. Entries did not change. Frozen: never change this struct.

use serde::Deserialize;

use super::{LIGHTMAP_SECTIONS, LightmapBakeInfo, LightmapDenoiseInfo, LightmapHeader};
use crate::baked_gi::SectionRef;

/// The postcard header of a version 5 `.flmap`, field for field.
#[derive(Deserialize)]
pub(super) struct LightmapHeaderV5 {
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
    denoise: Option<LightmapDenoiseInfo>,
}

impl From<LightmapHeaderV5> for LightmapHeader {
    fn from(h: LightmapHeaderV5) -> Self {
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
            denoise: h.denoise,
            source_hashes: Vec::new(),
            pages: 1,
        }
    }
}
