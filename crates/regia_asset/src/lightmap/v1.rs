// SPDX-License-Identifier: MPL-2.0
//! `.flmap` version 1 (ADR-860): the header shape before the direction section (ADR-1556). A
//! version 1 file has three sections (texels, coverage, geometry) and no `direction_encoding`;
//! [`super::Lightmap::read_header`] decodes it with this struct and converts it to the current
//! [`LightmapHeader`] with an empty fourth section. Frozen: never change this struct.

use serde::Deserialize;

use super::v2::LightmapEntryV2;
use super::{DIRECTION_NONE, LightmapBakeInfo, LightmapHeader};
use crate::baked_gi::SectionRef;

/// The postcard header of a version 1 `.flmap`, field for field.
#[derive(Deserialize)]
pub(super) struct LightmapHeaderV1 {
    width: u32,
    height: u32,
    texel_encoding: u32,
    exposure: f32,
    density: f32,
    padding: u32,
    entries: Vec<LightmapEntryV2>,
    bake: LightmapBakeInfo,
    sections: [SectionRef; 3],
    payload_hash: [u8; 32],
}

impl From<LightmapHeaderV1> for LightmapHeader {
    fn from(h: LightmapHeaderV1) -> Self {
        let [texels, coverage, geometry] = h.sections;
        Self {
            width: h.width,
            height: h.height,
            texel_encoding: h.texel_encoding,
            exposure: h.exposure,
            density: h.density,
            padding: h.padding,
            entries: h.entries.into_iter().map(Into::into).collect(),
            bake: h.bake,
            direction_encoding: DIRECTION_NONE,
            sections: [texels, coverage, geometry, SectionRef::default()],
            meshlet_section: SectionRef::default(),
            meshlet_bytes: Vec::new(),
            // The hash covers the sections in order; the empty fourth one adds no bytes.
            payload_hash: h.payload_hash,
            denoise: None,
            source_hashes: Vec::new(),
            pages: 1,
        }
    }
}
