// SPDX-License-Identifier: MPL-2.0
//! Cook a material image into the shared / compressed form the renderer uploads once.
//!
//! [`cook_texture_rgba8`] is the encoder. These helpers wrap its output in the handles
//! [`regia_render`] binds by image identity: one [`SharedImage`] for a graph or fixed slot,
//! and the packed detail / aux chains on [`CompressedMaps`].

use std::sync::Arc;

use regia_asset::TextureCookUsage;
use regia_render::compressed_texture::CompressedTexture;
use regia_render::{CompressedMaps, GraphTexture, LabTexture, SharedImage, SharedImageError};
use thiserror::Error;

use crate::astc_compress::AstcContent;
use crate::texture_compress::{
    TextureCookError, TextureCookQuality, cook_texture_rgba8, cook_texture_rgba8_content,
};

/// A shared or packed image could not be cooked.
#[derive(Debug, Error)]
pub enum CookedImageError {
    /// Block compression rejected the image.
    #[error(transparent)]
    Cook(#[from] TextureCookError),
    /// The cooked container did not parse.
    #[error(transparent)]
    Compressed(#[from] regia_render::CompressedTextureError),
    /// The shared image rejected its buffers.
    #[error(transparent)]
    Image(#[from] SharedImageError),
}

/// Cook `rgba` and keep both the RGBA8 bytes and the cooked chain in one [`SharedImage`].
///
/// Devices that sample the format upload the chain (an sRGB view when `srgb` is set, the same
/// choice as a cooked base color). Devices that do not sample it upload the RGBA8 bytes.
/// `usage` selects the codec: [`TextureCookUsage::ColorSrgb`] for color (pass `srgb: true`),
/// [`TextureCookUsage::ColorLinear`] for linear color and packed detail, [`TextureCookUsage::NormalMap`]
/// for a two-channel map whose shader rebuilds Z, [`TextureCookUsage::Mask`] for a single channel.
/// A graph normal that the shader reads as XYZ, not as a rebuilt Z, is [`TextureCookUsage::ColorLinear`].
///
/// # Errors
///
/// [`CookedImageError`] when the image is below 4×4 or the cook does not parse.
pub fn cook_shared_image(
    width: u32,
    height: u32,
    rgba: &[u8],
    usage: TextureCookUsage,
    srgb: bool,
    platform: &str,
    quality: TextureCookQuality,
) -> Result<SharedImage, CookedImageError> {
    let cooked = cook_texture_rgba8(width, height, rgba, usage, platform, quality)?;
    let compressed = CompressedTexture::from_cooked(&cooked)?;
    Ok(SharedImage::new(
        width,
        height,
        rgba.to_vec(),
        Some(Arc::new(compressed)),
        srgb,
    )?)
}

/// [`cook_shared_image`] as a graph slot. The RGBA8 pixels stay in the [`SharedImage`]; the
/// [`GraphTexture`] does not clone them.
///
/// # Errors
///
/// As [`cook_shared_image`].
pub fn cook_graph_texture(
    width: u32,
    height: u32,
    rgba: &[u8],
    usage: TextureCookUsage,
    srgb: bool,
    platform: &str,
    quality: TextureCookQuality,
) -> Result<GraphTexture, CookedImageError> {
    Ok(GraphTexture::from_shared(cook_shared_image(
        width, height, rgba, usage, srgb, platform, quality,
    )?))
}

fn cook_packed(
    image: &LabTexture,
    usage: TextureCookUsage,
    platform: &str,
    quality: TextureCookQuality,
    content: Option<AstcContent>,
) -> Result<Arc<CompressedTexture>, CookedImageError> {
    let cooked = match content {
        Some(content) => cook_texture_rgba8_content(
            image.width,
            image.height,
            &image.rgba,
            usage,
            platform,
            quality,
            content,
        )?,
        None => cook_texture_rgba8(image.width, image.height, &image.rgba, usage, platform, quality)?,
    };
    Ok(Arc::new(CompressedTexture::from_cooked(&cooked)?))
}

/// Cook the packed detail and aux images the forward shader samples.
///
/// Detail albedo and the detail normal are linear BC7. The detail normal is not BC5: the shader
/// reads XYZ and the mask in A, and does not rebuild Z. Height and curvature are BC5 (R and G;
/// the packed alpha is 255, which is what a BC5 sample returns for alpha). Absent inputs leave
/// that slot empty. The returned chains are already fitted to the resident cap, so cloning the
/// [`Arc`] into each material keeps one chain.
///
/// # Errors
///
/// [`CookedImageError`] when a packed image is below 4×4 or the cook does not parse.
#[allow(clippy::too_many_arguments)]
pub fn cook_surface_maps(
    detail_albedo: Option<&LabTexture>,
    detail_roughness: Option<&LabTexture>,
    detail_normal: Option<&LabTexture>,
    detail_mask: Option<&LabTexture>,
    height: Option<&LabTexture>,
    curvature: Option<&LabTexture>,
    platform: &str,
    quality: TextureCookQuality,
) -> Result<CompressedMaps, CookedImageError> {
    let mut maps = CompressedMaps::default();
    if let Some(packed) = regia_render::hard_surface::pack_detail_image(detail_albedo, detail_roughness) {
        maps.detail = Some(cook_packed(
            &packed,
            TextureCookUsage::ColorLinear,
            platform,
            quality,
            None,
        )?);
    }
    if let Some(packed) = regia_render::hard_surface::pack_detail_normal(detail_normal, detail_mask) {
        maps.detail_normal = Some(cook_packed(
            &packed,
            TextureCookUsage::ColorLinear,
            platform,
            quality,
            None,
        )?);
    }
    if let Some(packed) = regia_render::hard_surface::pack_aux(height, curvature) {
        maps.aux = Some(cook_packed(
            &packed,
            TextureCookUsage::NormalMap,
            platform,
            quality,
            // The aux shader reads height and curvature as `.rg` (BC5's layout) on every platform.
            Some(AstcContent::Rg),
        )?);
    }
    Ok(maps.fit_to_resident_cap())
}
