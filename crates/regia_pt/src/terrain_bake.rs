// SPDX-License-Identifier: MPL-2.0
//! The terrain's albedo as the real-time shader computes it, baked over the whole map for the
//! tracer's heightfield (`specs/27` § 1.1 "same inputs").
//!
//! The runtime terrain shader composes its albedo from the splat layers, the macro map, the
//! strata bands and ledges, talus, mid-range regolith, crust and the detail textures
//! (`TerrainRelief`), all functions of position, slope, altitude and the pixel footprint. Rather
//! than re-implement them, the bake renders the map top-down through the same `TerrainGpu` with
//! its "blended albedo (pre-lighting)" debug view (`TerrainFrame::debug_view == 3`), tile by
//! tile in orthographic projection, into the sRGB offscreen target: the texels are exactly what
//! the shader multiplies the light by at that footprint.
//!
//! Limits: one footprint for the whole map (`texel_m`), so near-field detail finer than a texel
//! (crust plates, grain, pebbles) is averaged; distance-faded terms are evaluated as seen from
//! `view_height_m` above each tile.

use regia_render::RenderDevice;
use regia_terrain::TerrainSharedDraw;

use crate::PtError;
use crate::scene::PtTexture;

/// How to bake.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AlbedoBake {
    /// World metres per texel.
    pub texel_m: f32,
    /// Tile edge in texels (one orthographic render each).
    pub tile_px: u32,
    /// Height of the shading eye above each tile centre (distance fades), metres.
    pub view_height_m: f32,
}

impl Default for AlbedoBake {
    fn default() -> Self {
        Self {
            texel_m: 2.0,
            tile_px: 256,
            view_height_m: 150.0,
        }
    }
}

/// Bakes `draw`'s albedo over its whole map, row 0 at the map's −Z edge, column 0 at −X (the
/// UV layout [`crate::PtHeightfield`] maps over the heightfield). Call before the renderer's
/// first frame with this draw (it leaves the draw's GPU uniforms set for the bake; the next
/// `prepare` rewrites them).
///
/// # Errors
///
/// The pyramid or an offscreen render failed.
pub fn bake_terrain_albedo(
    draw: &mut TerrainSharedDraw,
    device: &RenderDevice,
    bake: &AlbedoBake,
) -> Result<PtTexture, PtError> {
    // The bake lives with the terrain (the engine's irradiance probes use it too).
    let image = draw
        .bake_albedo_map(device, bake.texel_m, bake.tile_px, bake.view_height_m)
        .map_err(|e| PtError::Gpu(e.to_string()))?;
    Ok(PtTexture {
        width: image.width,
        height: image.height,
        rgba: std::sync::Arc::from(image.data),
        srgb: true,
    })
}

/// Bakes `draw`'s albedo ([`bake_terrain_albedo`], same timing rule: before the renderer's
/// first frame with this draw) and installs it as the albedo the draw reports to the
/// reference trace (`TerrainSharedDraw::set_reference_albedo`, read through
/// `FrameRenderer::reference_heightfield` by [`crate::extract::from_export`]). Without it the
/// trace uses the draw's first-order layer-stack estimate.
///
/// # Errors
///
/// See [`bake_terrain_albedo`].
pub fn install_reference_albedo(
    draw: &mut TerrainSharedDraw,
    device: &RenderDevice,
    bake: &AlbedoBake,
) -> Result<(), PtError> {
    let t = bake_terrain_albedo(draw, device, bake)?;
    draw.set_reference_albedo(Some(regia_render::LabTexture {
        width: t.width,
        height: t.height,
        rgba: t.rgba.to_vec(),
    }));
    Ok(())
}
