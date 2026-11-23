// SPDX-License-Identifier: MPL-2.0
//! `.flmap` version 3 (ADR-2024): the header shape before the cluster section (ADR-2107). Postcard
//! is not self-describing, so a version 3 file decodes with this struct and comes back in the
//! current shape, with an empty cluster section. Entries did not change. Frozen: never change this
//! struct.

use serde::Deserialize;

use super::{LIGHTMAP_SECTIONS, LightmapBakeInfo, LightmapHeader};
use crate::baked_gi::SectionRef;

/// The postcard header of a version 3 `.flmap`, field for field.
#[derive(Deserialize)]
pub(super) struct LightmapHeaderV3 {
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
    payload_hash: [u8; 32],
}

impl From<LightmapHeaderV3> for LightmapHeader {
    fn from(h: LightmapHeaderV3) -> Self {
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
            meshlet_section: SectionRef::default(),
            meshlet_bytes: Vec::new(),
            payload_hash: h.payload_hash,
            denoise: None,
            source_hashes: Vec::new(),
            pages: 1,
        }
    }
}
