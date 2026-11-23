// SPDX-License-Identifier: MPL-2.0
//! Custom decal image preparation (ADR-8203). No IO or resampling in the draw loop.

use std::sync::Arc;

use regia_asset::CookedImage;
use regia_render::forward_decals::{FORWARD_DECAL_ATLAS_LAYERS, ForwardDecalAtlas};

use crate::decal_atlas::{DECAL_ATLAS_LAYERS, DECAL_ATLAS_SIZE, generate_decal_atlas};
use crate::decal_format::DecalFormatError;

/// Immutable, matching projected and forward decal atlas pages.
///
/// The four built-ins retain their indices and bytes. Custom images use the remaining
/// layers of the renderer's existing atlas; each image is sampled at 64×64 texel centers
/// using nearest-neighbor resampling. RGB is sRGB and alpha is straight coverage.
#[derive(Debug, Clone)]
pub struct DecalTextureAtlas {
    pub(crate) rgba: Vec<u8>,
    pub(crate) forward: Arc<ForwardDecalAtlas>,
}

impl Default for DecalTextureAtlas {
    fn default() -> Self {
        Self {
            rgba: generate_decal_atlas(),
            forward: Arc::new(crate::decal_forward::forward_atlas()),
        }
    }
}

impl DecalTextureAtlas {
    /// Number of occupied layers, including the four built-ins.
    pub fn layers(&self) -> u32 {
        self.forward.layers
    }

    /// Matching pages for a host that feeds the forward renderer directly.
    pub fn forward_atlas(&self) -> Arc<ForwardDecalAtlas> {
        Arc::clone(&self.forward)
    }

    /// The projected path's layer-major RGBA8 bytes.
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    pub(crate) fn with_image(&self, layer: u32, image: &CookedImage) -> Result<Self, DecalFormatError> {
        let _span = tracing::debug_span!("prepare_decal_texture").entered();
        image
            .validate()
            .map_err(|e| DecalFormatError::Invalid(e.to_string()))?;
        if !(DECAL_ATLAS_LAYERS..FORWARD_DECAL_ATLAS_LAYERS).contains(&layer) || layer > self.layers() {
            return Err(DecalFormatError::Invalid(format!(
                "decal texture atlas is full ({} custom images)",
                FORWARD_DECAL_ATLAS_LAYERS - DECAL_ATLAS_LAYERS
            )));
        }
        let page = ForwardDecalAtlas::PAGE_BYTES;
        let layers = self.layers().max(layer + 1);
        let mut rgba = self.rgba.clone();
        rgba.resize(layers as usize * page, 0);
        let mut forward = (*self.forward).clone();
        forward.layers = layers;
        forward.rgba.resize(layers as usize * 3 * page, 0);
        let base = layer as usize * page;
        let fbase = base * 3;
        for y in 0..DECAL_ATLAS_SIZE {
            for x in 0..DECAL_ATLAS_SIZE {
                // Integer texel-center mapping: independent of platform float rounding.
                let sx = ((u64::from(x) * 2 + 1) * u64::from(image.width) / u64::from(DECAL_ATLAS_SIZE * 2))
                    as usize;
                let sy = ((u64::from(y) * 2 + 1) * u64::from(image.height) / u64::from(DECAL_ATLAS_SIZE * 2))
                    as usize;
                let src = (sy * image.width as usize + sx) * 4;
                let dst = ((y * DECAL_ATLAS_SIZE + x) * 4) as usize;
                rgba[base + dst..base + dst + 4].copy_from_slice(&image.rgba8[src..src + 4]);
                forward.rgba[fbase + dst..fbase + dst + 4].copy_from_slice(&image.rgba8[src..src + 4]);
                forward.rgba[fbase + page + dst..fbase + page + dst + 4]
                    .copy_from_slice(&[128, 128, 255, 255]);
                forward.rgba[fbase + 2 * page + dst..fbase + 2 * page + dst + 4]
                    .copy_from_slice(&[128, 128, 255, 255]);
            }
        }
        Ok(Self {
            rgba,
            forward: Arc::new(forward),
        })
    }
}

pub(crate) fn validate_texture_path(path: &str) -> Result<(), DecalFormatError> {
    // Check portable path spelling, including Windows prefixes on a Unix host.
    if path.is_empty()
        || path.contains(['\\', ':', '\0'])
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(DecalFormatError::Invalid(
            "texture must be a relative image path without empty, dot or parent components".into(),
        ));
    }
    Ok(())
}
