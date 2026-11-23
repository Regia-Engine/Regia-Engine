// SPDX-License-Identifier: MPL-2.0
//! Terrain material cook: bakes a [`regia_terrain::TerrainMaterial`] into KTX2/BCn blobs
//! (`docs/specs/12-assets.md` § 7.1) that the runtime uploads as hardware-sampled arrays
//! (`regia_terrain::gpu::TerrainGpu::set_material_cooked`).
//!
//! There is one terrain texture cook: [`crate::cook_tiled_texture`] (the terrain-data lane's
//! tiled KTX2 path). A layer texture or a macro tile is cooked as a single tile covering the
//! whole image, whose KTX2 carries the full mip chain — the same bytes the streaming cook
//! produces for a texture of that size, so the two paths cannot drift.
//!
//! Usage mapping ([`TileFormat`]): layer/macro albedo → BC7 sRGB (alpha keeps the macro AO),
//! normals → BC5, heights → BC4, ORM → BC7 linear.
//!
//! `platform == "android"` cooks the same images to ASTC with full mip chains (the block per
//! usage is `crate::texture_compress::astc_block_for`: 4×4 at the default quality), which the
//! runtime uploads as they are on ASTC devices instead of decoding BC to RGBA8. That whole-image
//! path stays on the generic Android encoder (and on the thread's ETC2 policy). The tiled
//! streaming cook (`cook_tiled_texture`) writes ASTC 4×4 tiles with the same astcenc encoder, and
//! this function does not call it on Android: a whole-image material array is the image's mip
//! chain, and the tests here pin that encoder. Normals are encoded as two channels decoded like BC5 (`.rg`, the layout the
//! terrain shader reads): 1.9° mean / 7.6° p99 angle error at 4×4 on DO27's layer normals,
//! against 1.6° / 5.9° for astcenc's `X X X Y` normal mode and 1.4° / 5.2° for BC5.
//!
//! The thread's [`crate::TexturePolicy`] ([`crate::with_texture_policy`], ADR-2063) applies as it
//! does to the generic texture cook: ETC2 / EAC instead of ASTC (normals EAC RG11, also `.rg`;
//! heights EAC R11), or the ASTC block ladder of the project's profile.

use regia_terrain::CookedMaterialKtx2;
use thiserror::Error;

use crate::terrain_data::{TerrainDataError, TileFormat, cook_tiled_texture};
use crate::texture_compress::{
    AndroidTextureFormat, AstcContent, TextureCookQuality, cook_ktx2_astc_as, cook_ktx2_etc2,
    generate_mips_rgba8,
};

/// Errors cooking a terrain material.
#[derive(Debug, Error)]
pub enum TerrainMaterialCookError {
    /// A texture failed the tiled cook.
    #[error("terrain texture cook error: {0}")]
    Tiled(#[from] TerrainDataError),
    /// The material failed validation (mixed sizes, bad parameters).
    #[error("terrain material is invalid: {0}")]
    Material(#[from] regia_terrain::MaterialError),
}

/// Cooks the layer textures (albedo, normal, height and — when the layer opts in — ORM) and
/// the macro tiles of `material` for `platform` at `quality`, all through
/// [`cook_tiled_texture`].
///
/// Empty layer slots are skipped; the blob vectors line up with the material's slot order
/// (see [`CookedMaterialKtx2`]). `layer_orm` is filled when any layer sets `use_orm`; the
/// layers before it that do not get a neutral ORM (AO 1, roughness 1, metal 0) so slots stay
/// aligned. The macro tiles follow the [`regia_terrain::MacroBake`] grid order. A coarse
/// fallback smaller than 8×8 is left out (the tiled cook needs a tail mip).
///
/// # Errors
///
/// Returns [`TerrainMaterialCookError`] when the material is invalid or any texture fails to cook
/// (non-square, too small for block compression, encoder failure).
pub fn cook_terrain_material(
    material: &regia_terrain::TerrainMaterial,
    platform: &str,
    quality: TextureCookQuality,
) -> Result<CookedMaterialKtx2, TerrainMaterialCookError> {
    material.validate()?;
    let mut cooked = CookedMaterialKtx2::default();
    let wants_orm = material.layers.iter().flatten().any(|set| set.use_orm);
    for (slot, set) in material.layers.iter().enumerate() {
        let Some(set) = set else { continue };
        let name = |kind: &str| format!("layer{slot}_{kind}");
        cooked.layer_albedo.push(cook_whole(
            &name("albedo"),
            &set.albedo,
            TileFormat::Bc7Srgb,
            platform,
            quality,
        )?);
        cooked.layer_normal.push(cook_whole(
            &name("normal"),
            &set.normal,
            TileFormat::Bc5,
            platform,
            quality,
        )?);
        cooked.layer_height.push(cook_whole(
            &name("height"),
            &set.height,
            TileFormat::Bc4,
            platform,
            quality,
        )?);
        if wants_orm {
            let neutral;
            let orm = if set.use_orm {
                &set.orm
            } else {
                neutral = regia_terrain::Rgba8Image::solid(set.orm.width, set.orm.height, [255, 255, 0, 255]);
                &neutral
            };
            cooked.layer_orm.push(cook_whole(
                &name("orm"),
                orm,
                TileFormat::Bc7Linear,
                platform,
                quality,
            )?);
        }
    }
    if let Some(bake) = &material.macro_set {
        for (i, tile) in bake.albedo.iter().enumerate() {
            cooked.macro_albedo_tiles.push(cook_whole(
                &format!("macro_albedo_{i}"),
                tile,
                TileFormat::Bc7Srgb,
                platform,
                quality,
            )?);
        }
        for (i, tile) in bake.normal.iter().enumerate() {
            cooked.macro_normal_tiles.push(cook_whole(
                &format!("macro_normal_{i}"),
                tile,
                TileFormat::Bc5,
                platform,
                quality,
            )?);
        }
        // The tiled cook needs at least one tail mip below the tile (≥ 8×8 for a single
        // whole-image tile). A smaller coarse map belongs to a tiny bake whose tiles are all
        // resident anyway; the shader then falls back to the neutral coarse layer.
        if bake.coarse_albedo.width >= 8 {
            cooked.macro_coarse_albedo = Some(cook_whole(
                "macro_coarse_albedo",
                &bake.coarse_albedo,
                TileFormat::Bc7Srgb,
                platform,
                quality,
            )?);
            cooked.macro_coarse_normal = Some(cook_whole(
                "macro_coarse_normal",
                &bake.coarse_normal,
                TileFormat::Bc5,
                platform,
                quality,
            )?);
        }
    }
    Ok(cooked)
}

/// Cooks one RGBA8 image through [`cook_tiled_texture`] as a single tile covering the whole
/// image and returns that tile's KTX2 (full mip chain).
fn cook_whole(
    name: &str,
    image: &regia_terrain::Rgba8Image,
    format: TileFormat,
    platform: &str,
    quality: TextureCookQuality,
) -> Result<Vec<u8>, TerrainMaterialCookError> {
    if platform == "android" {
        let (width, height) = (image.width, image.height);
        if width != height {
            return Err(TerrainDataError::NotSquare { width, height }.into());
        }
        if width < 4 {
            return Err(TerrainDataError::TooSmall { width, height }.into());
        }
        if image.data.len() != width as usize * height as usize * 4 {
            return Err(TerrainDataError::BadBuffer {
                len: image.data.len(),
                width,
                height,
            }
            .into());
        }
        let _span = tracing::info_span!("terrain_android_cook", name, width).entered();
        let mips = generate_mips_rgba8(width, height, &image.data);
        let policy = crate::texture_policy::active_texture_policy();
        let cooked = match policy.android_format {
            AndroidTextureFormat::Etc2 => cook_ktx2_etc2(width, height, &mips, format.usage()),
            AndroidTextureFormat::Astc => {
                // The terrain shader reads normal arrays as `.rg` (BC5's layout): cook them so.
                let content = (format == TileFormat::Bc5).then_some(AstcContent::Rg);
                let quality = policy.astc_profile.quality(quality);
                cook_ktx2_astc_as(width, height, &mips, format.usage(), quality, content)
            }
        };
        return cooked.map_err(|e| TerrainDataError::from(e).into());
    }
    let cooked = cook_tiled_texture(
        name,
        image.width,
        image.height,
        &image.data,
        image.width,
        format,
        platform,
        quality,
    )?;
    cooked
        .tiles
        .into_iter()
        .next()
        .map(|tile| tile.ktx2)
        .ok_or(TerrainMaterialCookError::Tiled(TerrainDataError::TooSmall {
            width: image.width,
            height: image.height,
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_terrain::{MaterialParams, TerrainMaterial};

    /// Every blob is byte-identical to the tiled cook of the same image (one cook path), and
    /// ORM is cooked when a layer opts in. The old cook went through `cook_texture_rgba8`
    /// and never emitted ORM, so both assertions fail on it.
    #[test]
    fn cook_routes_through_the_tiled_cook_and_includes_orm() {
        let mut material = TerrainMaterial::procedural_cc0(64, 3, MaterialParams::default());
        if let Some(Some(set)) = material.layers.get_mut(1) {
            set.use_orm = true;
            set.orm = regia_terrain::Rgba8Image::solid(64, 64, [180, 140, 0, 255]);
        }
        let cooked = match cook_terrain_material(&material, "linux", TextureCookQuality::Low) {
            Ok(cooked) => cooked,
            Err(err) => panic!("cook: {err}"),
        };
        assert_eq!(cooked.layer_albedo.len(), 4);
        assert_eq!(cooked.layer_orm.len(), 4, "ORM must be cooked for every slot");
        let Some(Some(set)) = material.layers.get(1) else {
            panic!("layer 1 missing");
        };
        let tiled = |image: &regia_terrain::Rgba8Image, format| match cook_tiled_texture(
            "x",
            image.width,
            image.height,
            &image.data,
            image.width,
            format,
            "linux",
            TextureCookQuality::Low,
        ) {
            Ok(c) => c.tiles[0].ktx2.clone(),
            Err(err) => panic!("tiled cook: {err}"),
        };
        assert_eq!(cooked.layer_albedo[1], tiled(&set.albedo, TileFormat::Bc7Srgb));
        assert_eq!(cooked.layer_normal[1], tiled(&set.normal, TileFormat::Bc5));
        assert_eq!(cooked.layer_height[1], tiled(&set.height, TileFormat::Bc4));
        assert_eq!(cooked.layer_orm[1], tiled(&set.orm, TileFormat::Bc7Linear));
    }

    /// Android cooks every blob to ASTC with its whole mip chain (down to 1×1, partial
    /// blocks included), 4×4 for colour, normals, heights and ORM at the default quality;
    /// before this the cook refused Android and phones decoded the BC pack to RGBA8.
    #[test]
    fn android_cooks_astc_arrays_with_full_chains() {
        let mut material = TerrainMaterial::procedural_cc0(64, 3, MaterialParams::default());
        if let Some(Some(set)) = material.layers.get_mut(1) {
            set.use_orm = true;
        }
        let cooked = match cook_terrain_material(&material, "android", TextureCookQuality::High) {
            Ok(cooked) => cooked,
            Err(err) => panic!("android cook: {err}"),
        };
        let check = |blobs: &[Vec<u8>], format: ktx2::Format| {
            assert!(!blobs.is_empty());
            for blob in blobs {
                let reader = match ktx2::Reader::new(&blob[..]) {
                    Ok(r) => r,
                    Err(err) => panic!("{format:?}: {err}"),
                };
                assert_eq!(reader.header().format, Some(format));
                assert_eq!(reader.header().pixel_width, 64);
                assert_eq!(reader.levels().count(), 7, "64 → 1 is seven levels");
                assert!(reader.levels().all(|l| l.data.len() >= 16));
            }
        };
        check(&cooked.layer_albedo, ktx2::Format::ASTC_4x4_SRGB_BLOCK);
        check(&cooked.layer_normal, ktx2::Format::ASTC_4x4_UNORM_BLOCK);
        check(&cooked.layer_height, ktx2::Format::ASTC_4x4_UNORM_BLOCK);
        check(&cooked.layer_orm, ktx2::Format::ASTC_4x4_UNORM_BLOCK);
        // Normals decode in BC5's layout (X, Y, 0, 1), which the terrain shader reads as `.rg`.
        let Some(Some(set)) = material.layers.get(1) else {
            panic!("layer 1 missing");
        };
        let reader = match ktx2::Reader::new(&cooked.layer_normal[1][..]) {
            Ok(r) => r,
            Err(err) => panic!("normal: {err}"),
        };
        let Some(level0) = reader.levels().next() else {
            panic!("normal level 0");
        };
        let decoded = match crate::astc_compress::decompress_astc_rgba8(64, 64, level0.data, (4, 4), false) {
            Ok(d) => d,
            Err(err) => panic!("decode: {err}"),
        };
        let mean_xy_error = |dec: &[u8]| {
            let sum: u64 = set
                .normal
                .data
                .chunks(4)
                .zip(dec.chunks(4))
                .map(|(s, d)| u64::from(s[0].abs_diff(d[0])) + u64::from(s[1].abs_diff(d[1])))
                .sum();
            sum as f64 / (2.0 * 64.0 * 64.0)
        };
        assert!(
            decoded.chunks(4).all(|d| (d[2], d[3]) == (0, 255)),
            "terrain normals decode as (X, Y, 0, 1)"
        );
        // Within reach of the desktop BC5 cook of the same (high-frequency, procedural) map.
        let desktop = match cook_terrain_material(&material, "linux", TextureCookQuality::High) {
            Ok(cooked) => cooked,
            Err(err) => panic!("desktop cook: {err}"),
        };
        let bc5 = match ktx2::Reader::new(&desktop.layer_normal[1][..]) {
            Ok(r) => r.levels().next().map(|l| l.data.to_vec()).unwrap_or_default(),
            Err(err) => panic!("bc5: {err}"),
        };
        let bc5 =
            match regia_asset::bcn_decode::decode_bcn(regia_asset::bcn_decode::BcnKind::Bc5, 64, 64, &bc5) {
                Ok(d) => d,
                Err(err) => panic!("bc5 decode: {err}"),
            };
        let (astc_err, bc5_err) = (mean_xy_error(&decoded), mean_xy_error(&bc5));
        assert!(
            astc_err <= bc5_err * 1.6 + 1.0,
            "ASTC normal error {astc_err:.2} vs BC5 {bc5_err:.2}"
        );
        // Low quality: colour and ORM drop to 8×8 (64 is not a multiple of 6), normals stay 4×4.
        let low = match cook_terrain_material(&material, "android", TextureCookQuality::Low) {
            Ok(cooked) => cooked,
            Err(err) => panic!("android low cook: {err}"),
        };
        check(&low.layer_albedo, ktx2::Format::ASTC_8x8_SRGB_BLOCK);
        check(&low.layer_normal, ktx2::Format::ASTC_4x4_UNORM_BLOCK);
    }

    /// `[cook.textures.android]` reaches the terrain arrays: ETC2 / EAC with every mip, normals
    /// decoding in the `.rg` layout the terrain shader reads (the ASTC path's layout too), or the
    /// ASTC ladder of the profile. A desktop cook ignores the policy.
    #[test]
    fn the_texture_policy_applies_to_the_terrain_arrays() {
        use crate::{AstcProfile, TexturePolicy, with_texture_policy};
        let mut material = TerrainMaterial::procedural_cc0(64, 3, MaterialParams::default());
        if let Some(Some(set)) = material.layers.get_mut(1) {
            set.use_orm = true;
        }
        let etc2 = TexturePolicy {
            android_format: AndroidTextureFormat::Etc2,
            astc_profile: AstcProfile::PerTexture,
        };
        let cooked = match with_texture_policy(etc2, || {
            cook_terrain_material(&material, "android", TextureCookQuality::High)
        }) {
            Ok(cooked) => cooked,
            Err(err) => panic!("etc2 cook: {err}"),
        };
        let check = |blobs: &[Vec<u8>], formats: &[ktx2::Format]| {
            assert!(!blobs.is_empty());
            for blob in blobs {
                let reader = match ktx2::Reader::new(&blob[..]) {
                    Ok(r) => r,
                    Err(err) => panic!("{formats:?}: {err}"),
                };
                let format = reader.header().format;
                assert!(format.is_some_and(|f| formats.contains(&f)), "{format:?}");
                assert_eq!(reader.levels().count(), 7, "64 → 1 is seven levels");
            }
        };
        use ktx2::Format as F;
        check(
            &cooked.layer_albedo,
            &[F::ETC2_R8G8B8_SRGB_BLOCK, F::ETC2_R8G8B8A8_SRGB_BLOCK],
        );
        check(&cooked.layer_normal, &[F::EAC_R11G11_UNORM_BLOCK]);
        check(&cooked.layer_height, &[F::EAC_R11_UNORM_BLOCK]);
        check(
            &cooked.layer_orm,
            &[F::ETC2_R8G8B8_UNORM_BLOCK, F::ETC2_R8G8B8A8_UNORM_BLOCK],
        );

        // The normals round-trip through the runtime's decoder in the `.rg` layout.
        let Some(Some(set)) = material.layers.get(1) else {
            panic!("layer 1 missing");
        };
        let reader = match ktx2::Reader::new(&cooked.layer_normal[1][..]) {
            Ok(r) => r,
            Err(err) => panic!("normal: {err}"),
        };
        let Some(level0) = reader.levels().next() else {
            panic!("normal level 0");
        };
        let decoded = match regia_asset::etc2_decode::decode_etc2(
            regia_asset::etc2_decode::Etc2Format::Rg11,
            64,
            64,
            level0.data,
        ) {
            Ok(d) => d,
            Err(err) => panic!("decode: {err}"),
        };
        let mean_xy_error = |dec: &[u8]| {
            let sum: u64 = set
                .normal
                .data
                .chunks(4)
                .zip(dec.chunks(4))
                .map(|(s, d)| u64::from(s[0].abs_diff(d[0])) + u64::from(s[1].abs_diff(d[1])))
                .sum();
            sum as f64 / (2.0 * 64.0 * 64.0)
        };
        // Within reach of the desktop BC5 cook of the same (high-frequency, procedural) map, the
        // same bit rate and the bound the ASTC path meets.
        let desktop = match cook_terrain_material(&material, "linux", TextureCookQuality::High) {
            Ok(cooked) => cooked,
            Err(err) => panic!("desktop cook: {err}"),
        };
        let bc5 = match ktx2::Reader::new(&desktop.layer_normal[1][..]) {
            Ok(r) => r.levels().next().map(|l| l.data.to_vec()).unwrap_or_default(),
            Err(err) => panic!("bc5: {err}"),
        };
        let bc5 =
            match regia_asset::bcn_decode::decode_bcn(regia_asset::bcn_decode::BcnKind::Bc5, 64, 64, &bc5) {
                Ok(d) => d,
                Err(err) => panic!("bc5 decode: {err}"),
            };
        let (eac_err, bc5_err) = (mean_xy_error(&decoded), mean_xy_error(&bc5));
        eprintln!("EAC RG11 normal error {eac_err:.2}, BC5 {bc5_err:.2}");
        assert!(
            eac_err <= bc5_err * 1.6 + 1.0,
            "EAC RG11 normal error {eac_err:.2} vs BC5 {bc5_err:.2}"
        );
        assert!(decoded.chunks(4).all(|d| (d[2], d[3]) == (0, 255)));

        // The `low` profile overrides a high-quality request; ASTC normals stay 4×4.
        let low = TexturePolicy {
            android_format: AndroidTextureFormat::Astc,
            astc_profile: AstcProfile::Low,
        };
        let cooked = match with_texture_policy(low, || {
            cook_terrain_material(&material, "android", TextureCookQuality::High)
        }) {
            Ok(cooked) => cooked,
            Err(err) => panic!("low cook: {err}"),
        };
        check(&cooked.layer_albedo, &[F::ASTC_8x8_SRGB_BLOCK]);
        check(&cooked.layer_normal, &[F::ASTC_4x4_UNORM_BLOCK]);

        // Desktop writes the same bytes under any policy.
        let cook_linux = || match cook_terrain_material(&material, "linux", TextureCookQuality::Low) {
            Ok(cooked) => cooked,
            Err(err) => panic!("desktop cook: {err}"),
        };
        let plain = cook_linux();
        let under_policy = with_texture_policy(etc2, cook_linux);
        assert_eq!(plain.layer_albedo, under_policy.layer_albedo);
        assert_eq!(plain.layer_normal, under_policy.layer_normal);
    }

    /// No layer opts into ORM: no ORM blobs (the shader keeps the scalar roughness).
    #[test]
    fn orm_is_skipped_when_no_layer_uses_it() {
        let material = TerrainMaterial::procedural_cc0(64, 3, MaterialParams::default());
        let cooked = match cook_terrain_material(&material, "linux", TextureCookQuality::Low) {
            Ok(cooked) => cooked,
            Err(err) => panic!("cook: {err}"),
        };
        assert!(cooked.layer_orm.is_empty());
        assert_eq!(cooked.layer_height.len(), 4);
    }
}
