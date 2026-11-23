// SPDX-License-Identifier: MPL-2.0
//! How the cook stores a `.flmap`'s texels (ADR-2314): the `.meta` `settings` string of a lightmap
//! (RON, [`LightmapCookSettings`]) picks RGB9E5 (the default: the cooked lightmap keeps the
//! texels it was baked with), BC6H, ASTC 4×4 HDR, or `auto` (BC6H on desktop platforms, ASTC HDR
//! on Android). A block encoding cuts the atlas from 4 bytes a texel to 1; the renderer uploads
//! the blocks where the GPU samples them and decodes them on the CPU elsewhere
//! (`regia_render::lightmap`).
//!
//! ```ron
//! (encoding: auto)
//! ```
//!
//! `directions` does the same for the direction section (ADR-2635): RGBA8 (the default), BC7,
//! ASTC 4×4 LDR, or `auto` (BC7 on desktop platforms, ASTC on Android), 4 bytes a texel to 1.
//!
//! ```ron
//! (encoding: auto, directions: auto)
//! ```

use serde::{Deserialize, Serialize};

use regia_asset::lightmap::{Lightmap, LightmapDirectionEncoding, LightmapTexelEncoding};

/// The `encoding` of a lightmap's cook settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LightmapCookEncoding {
    /// Keep RGB9E5 texels (the default; cooks as before ADR-2314).
    #[default]
    Rgb9e5,
    /// BC6H blocks on every platform.
    Bc6h,
    /// ASTC 4×4 HDR blocks on every platform.
    AstcHdr,
    /// BC6H on desktop platforms, ASTC 4×4 HDR on Android.
    Auto,
}

impl LightmapCookEncoding {
    /// The texel encoding the cook writes for `platform` (`linux`, `windows`, `android`, ...).
    #[must_use]
    pub fn for_platform(self, platform: &str) -> LightmapTexelEncoding {
        match self {
            Self::Rgb9e5 => LightmapTexelEncoding::Rgb9e5,
            Self::Bc6h => LightmapTexelEncoding::Bc6h,
            Self::AstcHdr => LightmapTexelEncoding::AstcHdr,
            Self::Auto if platform == "android" => LightmapTexelEncoding::AstcHdr,
            Self::Auto => LightmapTexelEncoding::Bc6h,
        }
    }
}

/// The `directions` of a lightmap's cook settings (ADR-2635).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LightmapCookDirections {
    /// Keep RGBA8 directions (the default; cooks as before ADR-2635).
    #[default]
    Rgba8,
    /// BC7 blocks on every platform.
    Bc7,
    /// ASTC 4×4 LDR blocks on every platform.
    AstcLdr,
    /// BC7 on desktop platforms, ASTC 4×4 LDR on Android.
    Auto,
}

impl LightmapCookDirections {
    /// The direction encoding the cook writes for `platform`.
    #[must_use]
    pub fn for_platform(self, platform: &str) -> LightmapDirectionEncoding {
        match self {
            Self::Rgba8 => LightmapDirectionEncoding::Rgba8,
            Self::Bc7 => LightmapDirectionEncoding::Bc7,
            Self::AstcLdr => LightmapDirectionEncoding::AstcLdr,
            Self::Auto if platform == "android" => LightmapDirectionEncoding::AstcLdr,
            Self::Auto => LightmapDirectionEncoding::Bc7,
        }
    }
}

/// A lightmap's cook settings (the `.meta` `settings` string, RON).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LightmapCookSettings {
    /// How the cooked lightmap stores its texels.
    pub encoding: LightmapCookEncoding,
    /// How it stores its directions (ADR-2635; no effect on a lightmap without directions).
    pub directions: LightmapCookDirections,
}

/// Encodes `map`'s directions as `encoding`'s blocks and stores them
/// ([`Lightmap::set_direction_blocks`]; ADR-2635). Nothing to do for RGBA8 or a lightmap without
/// directions. Returns the largest per-channel change of a direction texel, 8-bit levels.
///
/// # Errors
///
/// A message (starting `lightmap_direction_encode:`) when the encoder fails or its blocks do not
/// decode.
pub fn encode_lightmap_directions(
    map: &mut Lightmap,
    encoding: LightmapDirectionEncoding,
) -> Result<u8, String> {
    if map.directions.is_empty() || encoding == LightmapDirectionEncoding::Rgba8 {
        return Ok(0);
    }
    let _span = tracing::info_span!("encode_lightmap_directions", encoding = encoding.name()).entered();
    let (w, h) = (map.width, map.height);
    let rgba: Vec<u8> = map.directions.iter().flat_map(|d| d.to_le_bytes()).collect();
    // Each atlas page is its own image (ADR-3300): blocks never span two pages.
    let page_bytes = (map.page_texels() * 4).max(1);
    let mut data = Vec::with_capacity(encoding.section_bytes(w, h) * map.pages() as usize);
    for page in rgba.chunks(page_bytes) {
        data.extend(encode_direction_page(encoding, w, h, page)?);
    }
    map.set_direction_blocks(encoding, data)
        .map_err(|e| format!("lightmap_direction_encode: {}: {e}", e.code()))
}

/// One page of RGBA8 directions as `encoding`'s blocks.
fn encode_direction_page(
    encoding: LightmapDirectionEncoding,
    w: u32,
    h: u32,
    rgba: &[u8],
) -> Result<Vec<u8>, String> {
    Ok(match encoding {
        LightmapDirectionEncoding::Rgba8 => rgba.to_vec(),
        // The slow presets: a direction section is small next to the textures of a level, and its
        // error turns straight into shading error (8-bit levels of `d · 0.5 + 0.5`).
        LightmapDirectionEncoding::Bc7 => {
            let (pw, ph, padded) = crate::texture_compress::pad_to_blocks(w, h, rgba, 4);
            intel_tex_2::bc7::compress_blocks(
                &intel_tex_2::bc7::alpha_slow_settings(),
                &intel_tex_2::RgbaSurface {
                    width: pw,
                    height: ph,
                    data: padded.as_ref(),
                    stride: pw * 4,
                },
            )
        }
        LightmapDirectionEncoding::AstcLdr => crate::astc_compress::compress_astc_rgba8_preset(
            w,
            h,
            rgba,
            (4, 4),
            false,
            crate::astc_compress::AstcContent::Rgba,
            ctt_astcenc::Preset::Thorough,
        )
        .map_err(|e| format!("lightmap_direction_encode: {e}"))?,
    })
}

impl LightmapCookSettings {
    /// Parses the `.meta` settings string; missing or empty is the defaults.
    ///
    /// # Errors
    ///
    /// A message (starting `lightmap_cook_settings:`) when the string is not valid RON for these
    /// settings: the cook reports it rather than guess.
    pub fn from_meta_string(settings: Option<&str>) -> Result<Self, String> {
        let Some(text) = settings.filter(|s| !s.trim().is_empty()) else {
            return Ok(Self::default());
        };
        ron::from_str(text)
            .map_err(|e| format!("lightmap_cook_settings: {text:?} is not lightmap cook settings: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse_and_pick_the_platform_encoding() {
        assert_eq!(
            LightmapCookSettings::from_meta_string(None),
            Ok(LightmapCookSettings::default())
        );
        assert_eq!(
            LightmapCookSettings::from_meta_string(Some("  ")),
            Ok(LightmapCookSettings::default())
        );
        let auto = LightmapCookSettings::from_meta_string(Some("(encoding: auto)")).expect("auto");
        assert_eq!(auto.encoding.for_platform("linux"), LightmapTexelEncoding::Bc6h);
        assert_eq!(auto.encoding.for_platform("windows"), LightmapTexelEncoding::Bc6h);
        assert_eq!(
            auto.encoding.for_platform("android"),
            LightmapTexelEncoding::AstcHdr
        );
        let bc = LightmapCookSettings::from_meta_string(Some("(encoding: bc6h)")).expect("bc6h");
        assert_eq!(bc.encoding.for_platform("android"), LightmapTexelEncoding::Bc6h);
        assert_eq!(
            LightmapCookSettings::default().encoding.for_platform("android"),
            LightmapTexelEncoding::Rgb9e5
        );
        let dirs =
            LightmapCookSettings::from_meta_string(Some("(encoding: auto, directions: auto)")).expect("dirs");
        assert_eq!(
            dirs.directions.for_platform("linux"),
            LightmapDirectionEncoding::Bc7
        );
        assert_eq!(
            dirs.directions.for_platform("android"),
            LightmapDirectionEncoding::AstcLdr
        );
        assert_eq!(
            LightmapCookSettings::default().directions.for_platform("android"),
            LightmapDirectionEncoding::Rgba8
        );
        for bad in ["(encoding: dxt1)", "(colour: auto)", "nope", "(directions: dxt5)"] {
            let e = LightmapCookSettings::from_meta_string(Some(bad)).expect_err(bad);
            assert!(e.starts_with("lightmap_cook_settings:"), "{e}");
        }
    }

    /// A directional lightmap's directions as BC7 and ASTC LDR blocks (ADR-2635): 1 byte a texel,
    /// the decode within 16 8-bit levels of the RGBA8 directions, and the file round-trips with
    /// the blocks (a reader decodes them into the same directions).
    #[test]
    fn directions_encode_as_blocks_and_round_trip() {
        use regia_asset::lightmap::{
            Coverage, LightmapBakeInfo, LightmapEntry, LightmapLayout, MeshUnwrap, decode_direction,
        };
        let (w, h) = (12u32, 8u32);
        let n = (w * h) as usize;
        let mut base = Lightmap::from_irradiance(
            LightmapLayout {
                width: w,
                height: h,
                density: 1.0,
                padding: 1,
                entries: vec![LightmapEntry {
                    name: "e".into(),
                    source_vertices: 3,
                    rect: [0, 0, w, h],
                    scale_offset: [1.0, 1.0, 0.0, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 1,
                    area_m2: 1.0,
                    texels_per_metre: 1.0,
                    key: None,
                    seams: None,
                    page: 0,
                }],
                unwraps: vec![MeshUnwrap {
                    xref: vec![0, 1, 2],
                    uv: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
                    indices: vec![0, 1, 2],
                }],
            },
            &vec![[100.0; 3]; n],
            vec![Coverage::BAKED; n],
            LightmapBakeInfo::default(),
        )
        .expect("valid");
        // A smooth field of directions, as a bake gives across a wall.
        let dirs: Vec<[f32; 4]> = (0..n)
            .map(|i| {
                let (x, y) = ((i as u32 % w) as f32 / w as f32, (i as u32 / w) as f32 / h as f32);
                let v = [x - 0.5, 0.6, y - 0.5];
                let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
                [
                    v[0] / len * 0.8,
                    v[1] / len * 0.8,
                    v[2] / len * 0.8,
                    0.5 + 0.4 * x,
                ]
            })
            .collect();
        base.set_directions(&dirs).expect("one per texel");
        for encoding in [LightmapDirectionEncoding::Bc7, LightmapDirectionEncoding::AstcLdr] {
            let mut map = base.clone();
            let worst = encode_lightmap_directions(&mut map, encoding).expect("encodes");
            eprintln!("{encoding:?}: worst direction texel change {worst} levels");
            // Measured on this smooth field (the build server, 2026-11-04): BC7 `alpha_slow` 12 levels, ASTC 4×4
            // `thorough` 13 (8 bits a texel both); a level is 1/127.5 of a direction component.
            assert!(
                worst <= 16,
                "{encoding:?}: a direction texel moved {worst} levels"
            );
            assert_eq!(map.direction_encoding(), Some(encoding));
            let blocks = map.direction_blocks.as_ref().expect("blocks");
            assert_eq!(blocks.data.len(), (w * h) as usize, "1 byte a texel");
            let back = Lightmap::from_bytes(&map.to_bytes().expect("writes")).expect("reads");
            assert_eq!(back, map, "{encoding:?}: the blocks and their decode round-trip");
            let (a, b) = (
                decode_direction(base.directions[40]),
                decode_direction(back.directions[40]),
            );
            assert!((0..4).all(|k| (a[k] - b[k]).abs() < 0.1), "{a:?} vs {b:?}");
        }
        // RGBA8 and a lightmap without directions are left alone.
        let mut same = base.clone();
        assert_eq!(
            encode_lightmap_directions(&mut same, LightmapDirectionEncoding::Rgba8),
            Ok(0)
        );
        assert_eq!(same, base);
        let mut plain = base.clone();
        plain.directions.clear();
        assert_eq!(
            encode_lightmap_directions(&mut plain, LightmapDirectionEncoding::Bc7),
            Ok(0)
        );
        assert_eq!(plain.direction_encoding(), None);

        // Two pages (ADR-3300): each page encodes on its own, so page 0's blocks are the blocks of
        // the one-page lightmap, and the file reads back as written.
        let mut paged = base.clone();
        let mut second = paged.entries[0].clone();
        second.name = "e2".into();
        second.page = 1;
        paged.entries.push(second);
        paged.unwraps.push(paged.unwraps[0].clone());
        paged.texels.extend(base.texels.clone());
        paged.coverage.extend(base.coverage.clone());
        paged.directions.extend(base.directions.iter().rev().copied());
        paged.assign_geometry_offsets();
        paged.validate().expect("two pages");
        for encoding in [LightmapDirectionEncoding::Bc7, LightmapDirectionEncoding::AstcLdr] {
            let (mut one, mut two) = (base.clone(), paged.clone());
            encode_lightmap_directions(&mut one, encoding).expect("one page");
            encode_lightmap_directions(&mut two, encoding).expect("two pages");
            let page0 = one.direction_blocks.as_ref().expect("blocks").data.clone();
            let both = &two.direction_blocks.as_ref().expect("blocks").data;
            assert_eq!(both.len(), 2 * page0.len(), "{encoding:?}");
            assert_eq!(both[..page0.len()], page0[..], "{encoding:?}: page 0 alone");
            let back = Lightmap::from_bytes(&two.to_bytes().expect("writes")).expect("reads");
            assert_eq!(back, two, "{encoding:?}");
        }
    }
}
