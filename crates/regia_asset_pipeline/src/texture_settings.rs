// SPDX-License-Identifier: MPL-2.0
//! Texture importer settings (`docs/specs/12-assets.md` § 7.1): the per-texture `.meta`
//! overrides and the usage a raster source cooks as when nothing overrides it. The file-name
//! suffixes and the texture-set slots they fill live in one shared table,
//! [`TEXTURE_SUFFIX_TABLE`] (spec 22 T2.14, ADR-3320).
//!
//! Every PNG/JPEG cooks block-compressed with a full mip chain by default — BC7 for colour and
//! linear data, BC5 for normal maps (desktop), ASTC on Android — so a game does not ship a large
//! uncompressed RGBA8 map. Two cases stay uncompressed (`CookedImage`): `compress: Some(false)`
//! and an image with an edge under 4 px; a 16-bit grayscale PNG cooks as a `Heightfield`. A standalone image's usage comes from its file name (`_n`, `_normal`,
//! `_nrm` → normal map; `_orm`, `_rough`, `_metal`, `_ao`, `_mask`, … → linear data; anything
//! else → sRGB colour); a glTF-referenced image takes the usage of the material slot it fills.
//! The image's own `.meta` (`settings`, RON) overrides either, for the standalone cook and for
//! every glTF that references the image:
//!
//! ```ron
//! (usage: Some(normal), max_size: Some(2048), compress: None, quality: None)
//! ```
//!
//! An alpha-tested texture (foliage, fences) sets `alpha_cutoff: Some(0.5)` (the material's
//! cutoff) so its mips keep mip 0's coverage (`crate::alpha_coverage`, ADR-3090).

use std::fmt::Write as _;
use std::path::Path;

use regia_asset::TextureCookUsage;
pub use regia_asset::texture_sets::{
    NormalGreenSetting, OrmChannelOrderSetting, TEXTURE_SUFFIX_TABLE, TextureSetSlot, TextureSuffixRow,
    apply_channel_transforms, orm_channel_order_from_file_name, suffix_word, texture_set_slot_from_file_name,
    usage_from_file_name,
};
use serde::Deserialize;

use crate::texture_compress::TextureCookQuality;

/// How a texture is sampled (the `.meta` spelling of [`TextureCookUsage`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextureUsageSetting {
    /// sRGB colour (base colour, emissive): BC7 sRGB / ASTC sRGB.
    Color,
    /// Linear data (ORM, masks with several channels, height): BC7 / ASTC linear.
    Linear,
    /// Tangent-space normal map: BC5 (RG, Z rebuilt) / ASTC.
    Normal,
    /// One channel: BC4 / ASTC.
    Mask,
    /// UI art (sRGB colour).
    Ui,
}

/// Color space a glTF material texture slot requires.
///
/// A standalone image still uses its file name (`usage_from_file_name`). A
/// glTF-referenced image takes the slot it fills: base color, emissive, sheen
/// color and specular color are sRGB; `normalTexture` and clearcoat normals are
/// normal maps; every other sampled slot is linear data. `REGIA_materials_detail`
/// is linear even when the key is `normalTexture`, because that layer is cooked
/// as linear data.
#[must_use]
pub fn gltf_slot_color_space(slot: &str) -> &'static str {
    match slot {
        "baseColorTexture" | "emissiveTexture" | "sheenColorTexture" | "specularColorTexture" => "srgb",
        "normalTexture" | "clearcoatNormalTexture" => "normal",
        _ => "linear",
    }
}

impl From<TextureUsageSetting> for TextureCookUsage {
    fn from(u: TextureUsageSetting) -> Self {
        match u {
            TextureUsageSetting::Color => Self::ColorSrgb,
            TextureUsageSetting::Linear => Self::ColorLinear,
            TextureUsageSetting::Normal => Self::NormalMap,
            TextureUsageSetting::Mask => Self::Mask,
            TextureUsageSetting::Ui => Self::Ui,
        }
    }
}

/// Compression quality override.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextureQualitySetting {
    /// BC7 (ASTC 4×4 on Android): the default.
    High,
    /// BC1/BC3 (ASTC 6×6 on Android for colour and linear data, 8×8 for masks, 4×4 for normals,
    /// each stepping down when the size does not divide): smaller, blockier.
    Low,
}

/// How an HDR source (`.exr`, `.hdr`) is stored in the cooked KTX2 (ADR-2546).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HdrEncodingSetting {
    /// BC6H on desktop platforms, RGBA16F on Android: the default.
    Auto,
    /// BC6H unsigned float (16 bytes a 4×4 block) on every platform.
    Bc6h,
    /// Uncompressed RGBA16F (8 bytes a texel) on every platform.
    Rgba16f,
    /// ASTC 4×4 HDR (needs the device's ASTC HDR support) on every platform.
    AstcHdr,
}

/// Per-texture overrides from the image's `.meta` `settings` (RON). Every field is optional;
/// unset fields keep the defaults.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case", default)]
pub struct TextureImporterSettings {
    /// Sampling usage (default: the material slot, else the file name).
    pub usage: Option<TextureUsageSetting>,
    /// Largest edge cooked, texels: larger sources are box-filtered down a mip level at a time.
    pub max_size: Option<u32>,
    /// `false` keeps the image RGBA8 (pixel art that must not be block-compressed). Default on.
    pub compress: Option<bool>,
    /// Compression quality (default high).
    pub quality: Option<TextureQualitySetting>,
    /// `false` keeps the whole mip chain resident: for a texture that must never show a low mip.
    pub stream: Option<bool>,
    /// The smallest size a streamed texture drops to, texels on the largest edge. Above the
    /// tier's own floor it raises the floor; below it, it changes nothing.
    pub stream_min_px: Option<u32>,
    /// HDR sources only (`.exr`, `.hdr`): how the texels are stored (default [`HdrEncodingSetting::Auto`];
    /// `compress: Some(false)` means RGBA16F).
    pub hdr_encoding: Option<HdrEncodingSetting>,
    /// HDR equirect sources only: cook a six-face cube map of this face edge, texels (4 to
    /// 8192), converted at cook time; unset keeps the 2D equirect image.
    pub cube_face_px: Option<u32>,
    /// Normal maps only: the source's green-channel convention (default
    /// [`NormalGreenSetting::Opengl`]). A [`NormalGreenSetting::Directx`] source has its green
    /// flipped at cook, never on disk (spec 22 T2.14, ADR-3320).
    pub normal_green: Option<NormalGreenSetting>,
    /// Packed occlusion-roughness-metallic maps only: the channel order the source stores
    /// (default [`OrmChannelOrderSetting::Orm`]). Another order is swizzled to `ORM` at cook,
    /// never on disk (spec 22 T2.14, ADR-3320).
    pub orm_channel_order: Option<OrmChannelOrderSetting>,
    /// The alpha-test cutoff (0 to 1, normally the material's `alpha_cutoff`) whose coverage every
    /// mip keeps (ADR-3090): each level is scaled so the fraction of texels at or above the cutoff
    /// matches mip 0's. Unset (the default) leaves the mips plain box averages. Read from alpha,
    /// or from red for a `mask` texture; a normal map ignores it.
    pub alpha_cutoff: Option<f32>,
    /// Also cook a glTF-referenced L16 image as a standalone heightfield when `true`.
    /// Unreferenced L16 images default to heightfields; `false` cooks them as textures.
    pub heightfield: Option<bool>,
}

impl TextureImporterSettings {
    /// Parses a `.meta` `settings` string; missing or malformed settings are the defaults (a
    /// malformed string is logged).
    #[must_use]
    pub fn from_meta_string(settings: Option<&str>) -> Self {
        let Some(s) = settings.filter(|s| !s.trim().is_empty()) else {
            return Self::default();
        };
        ron::from_str::<Self>(s).unwrap_or_else(|e| {
            tracing::warn!(target: "regia_asset_pipeline", "texture .meta settings {s:?}: {e}; using defaults");
            Self::default()
        })
    }

    /// The settings in `source`'s existing `.meta` (this function creates none; the cook writes the
    /// `.meta` of a glTF's referenced images before it runs, ADR-8054): how a glTF honours the
    /// overrides of the images it references.
    #[must_use]
    pub fn for_source(source: &Path) -> Self {
        let meta_path = regia_asset::meta_path_for(source);
        let Ok(bytes) = std::fs::read(&meta_path) else {
            return Self::default();
        };
        match regia_asset::MetaSidecar::from_ron(&bytes) {
            Ok(meta) => Self::from_meta_string(meta.settings.as_deref()),
            Err(e) => {
                tracing::warn!(target: "regia_asset_pipeline", "{}: {e}; using defaults", meta_path.display());
                Self::default()
            }
        }
    }

    /// `true` unless the settings opt out of block compression.
    #[must_use]
    pub fn compress(&self) -> bool {
        self.compress.unwrap_or(true)
    }

    /// The usage to cook as: the override, else `default`.
    #[must_use]
    pub fn usage_or(&self, default: TextureCookUsage) -> TextureCookUsage {
        self.usage.map_or(default, TextureCookUsage::from)
    }

    /// The quality to cook at: the override, else `default`.
    #[must_use]
    pub fn quality_or(&self, default: TextureCookQuality) -> TextureCookQuality {
        match self.quality {
            Some(TextureQualitySetting::High) => TextureCookQuality::High,
            Some(TextureQualitySetting::Low) => TextureCookQuality::Low,
            None => default,
        }
    }

    /// A stable fingerprint of the settings for the cook's cache key.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let base = format!(
            "usage={:?};max={:?};compress={:?};quality={:?}",
            self.usage, self.max_size, self.compress, self.quality
        );
        // Only when set: a cook cache keyed on the string above stays valid for every other texture.
        let mut out = base;
        if self.stream.is_some() || self.stream_min_px.is_some() {
            out = format!(
                "{out};stream={:?};stream_min={:?}",
                self.stream, self.stream_min_px
            );
        }
        if self.hdr_encoding.is_some() || self.cube_face_px.is_some() {
            out = format!(
                "{out};hdr_encoding={:?};cube_face_px={:?}",
                self.hdr_encoding, self.cube_face_px
            );
        }
        // Only when set: the channel transforms of spec 22 T2.14 (ADR-3320).
        if self.normal_green.is_some() || self.orm_channel_order.is_some() {
            out = format!(
                "{out};normal_green={:?};orm_channel_order={:?}",
                self.normal_green, self.orm_channel_order
            );
        }
        if let Some(cutoff) = self.alpha_cutoff {
            // The bits, so 0.5 and 0.50000006 key apart and NaN keys the same each time.
            out = format!("{out};alpha_cutoff={:08x}", cutoff.to_bits());
        }
        if let Some(heightfield) = self.heightfield {
            out = format!("{out};heightfield={heightfield}");
        }
        out
    }

    /// The normal-map green convention to cook with: the `.meta` choice, else the OpenGL
    /// default (the source-file-name inference lives in the texture-set generator, which writes
    /// the `.meta`; the cook honours the `.meta` only).
    #[must_use]
    pub fn normal_green_or_default(&self) -> NormalGreenSetting {
        self.normal_green.unwrap_or_default()
    }

    /// The packed-ORM channel order to cook with: the `.meta` choice, else the native `ORM`.
    #[must_use]
    pub fn orm_channel_order_or_default(&self) -> OrmChannelOrderSetting {
        self.orm_channel_order.unwrap_or_default()
    }

    /// The `.meta` `settings` string spelling of these settings: only the set fields, in a
    /// stable order, so a generated or merged string stays small and diff-friendly. The empty
    /// settings spell as `"()"` (the parser reads them as the defaults).
    #[must_use]
    pub fn to_meta_string(&self) -> String {
        let mut out = String::from("(");
        let mut first = true;
        let field = |out: &mut String, name: &str, value: String, first: &mut bool| {
            if !*first {
                out.push_str(", ");
            }
            *first = false;
            let _ = write!(out, "{name}: Some({value})");
        };
        if let Some(usage) = self.usage {
            let label = match usage {
                TextureUsageSetting::Color => "color",
                TextureUsageSetting::Linear => "linear",
                TextureUsageSetting::Normal => "normal",
                TextureUsageSetting::Mask => "mask",
                TextureUsageSetting::Ui => "ui",
            };
            field(&mut out, "usage", label.to_string(), &mut first);
        }
        if let Some(max_size) = self.max_size {
            field(&mut out, "max_size", max_size.to_string(), &mut first);
        }
        if let Some(compress) = self.compress {
            field(&mut out, "compress", compress.to_string(), &mut first);
        }
        if let Some(quality) = self.quality {
            let label = match quality {
                TextureQualitySetting::High => "high",
                TextureQualitySetting::Low => "low",
            };
            field(&mut out, "quality", label.to_string(), &mut first);
        }
        if let Some(stream) = self.stream {
            field(&mut out, "stream", stream.to_string(), &mut first);
        }
        if let Some(stream_min_px) = self.stream_min_px {
            field(&mut out, "stream_min_px", stream_min_px.to_string(), &mut first);
        }
        if let Some(hdr) = self.hdr_encoding {
            let label = match hdr {
                HdrEncodingSetting::Auto => "auto",
                HdrEncodingSetting::Bc6h => "bc6h",
                HdrEncodingSetting::Rgba16f => "rgba16f",
                HdrEncodingSetting::AstcHdr => "astc_hdr",
            };
            field(&mut out, "hdr_encoding", label.to_string(), &mut first);
        }
        if let Some(cube_face_px) = self.cube_face_px {
            field(&mut out, "cube_face_px", cube_face_px.to_string(), &mut first);
        }
        if let Some(green) = self.normal_green {
            let label = match green {
                NormalGreenSetting::Opengl => "opengl",
                NormalGreenSetting::Directx => "directx",
            };
            field(&mut out, "normal_green", label.to_string(), &mut first);
        }
        if let Some(order) = self.orm_channel_order {
            let label = match order {
                OrmChannelOrderSetting::Orm => "orm",
                OrmChannelOrderSetting::Arm => "arm",
                OrmChannelOrderSetting::Rma => "rma",
                OrmChannelOrderSetting::Mra => "mra",
            };
            field(&mut out, "orm_channel_order", label.to_string(), &mut first);
        }
        if let Some(cutoff) = self.alpha_cutoff {
            field(&mut out, "alpha_cutoff", format!("{cutoff:?}"), &mut first);
        }
        if let Some(heightfield) = self.heightfield {
            field(&mut out, "heightfield", heightfield.to_string(), &mut first);
        }
        out.push(')');
        out
    }

    /// The alpha cutoff the cook keeps coverage for, for a texture of `usage`: the setting when it
    /// is a finite value in (0, 1] and the usage has a coverage channel
    /// ([`crate::alpha_coverage::coverage_channel`]); `None` otherwise (a value out of range is
    /// logged and ignored).
    #[must_use]
    pub fn coverage_cutoff(&self, usage: TextureCookUsage) -> Option<f32> {
        let cutoff = self.alpha_cutoff?;
        if !(cutoff.is_finite() && cutoff > 0.0 && cutoff <= 1.0) {
            tracing::warn!(target: "regia_asset_pipeline", "texture .meta alpha_cutoff {cutoff} is not in (0, 1]; ignored");
            return None;
        }
        if crate::alpha_coverage::coverage_channel(usage).is_none() {
            tracing::warn!(target: "regia_asset_pipeline", "texture .meta alpha_cutoff on a normal map; ignored");
            return None;
        }
        Some(cutoff)
    }

    /// `texture` with this file's streaming choices in its metadata
    /// ([`regia_asset::MipStreamMeta`]); unchanged when the file makes none.
    ///
    /// # Errors
    /// The cooked KTX2 cannot be rewritten.
    pub fn apply_stream(
        &self,
        mut texture: regia_asset::CookedTexture,
    ) -> Result<regia_asset::CookedTexture, crate::ktx2_write::Ktx2WriteError> {
        if self.stream.is_none() && self.stream_min_px.is_none() {
            return Ok(texture);
        }
        let mut meta = crate::mip_stream::cooked_mip_stream(&texture)
            .unwrap_or_else(|| regia_asset::MipStreamMeta::new(0));
        meta.stream = self.stream.unwrap_or(meta.stream);
        meta.min_resident_px = self.stream_min_px.unwrap_or(meta.min_resident_px);
        texture.ktx2 = crate::mip_stream::stamp_mip_stream(&texture.ktx2, &meta)?.into();
        Ok(texture)
    }

    /// `(width, height, rgba)` box-filtered a level at a time until the largest edge is at most
    /// [`Self::max_size`] (unchanged without one), or the cook diet's edge cap of the source being
    /// cooked when that is smaller ([`crate::cook_diet::with_texture_cap`], ADR-8461).
    #[must_use]
    pub fn fit(&self, width: u32, height: u32, rgba: Vec<u8>) -> (u32, u32, Vec<u8>) {
        let own = self.max_size.filter(|m| *m > 0);
        let diet = crate::cook_diet::texture_cap().filter(|m| *m > 0);
        let max = match (own, diet) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let Some(max) = max else {
            return (width, height, rgba);
        };
        if width.max(height) <= max {
            return (width, height, rgba);
        }
        let mut level = (width, height, rgba);
        while level.0.max(level.1) > max && (level.0 > 1 || level.1 > 1) {
            level = halve_rgba8(level.0, level.1, &level.2);
        }
        level
    }
}

/// Source window `[start, end)` of output sample `i` (of `out_size`) along an edge of `size`
/// texels: two texels, except the last sample of an odd edge, which also takes the remainder.
pub(crate) fn box_window(i: u32, out_size: u32, size: u32) -> (u32, u32) {
    let start = (2 * i).min(size - 1);
    let end = if i + 1 == out_size {
        size
    } else {
        (2 * i + 2).min(size)
    };
    (start, end.max(start + 1))
}

/// One 2×2 box-filter step. An odd edge folds its last texel into the last output sample (that
/// sample averages a 3-wide or 3-tall window), so no texel is dropped.
fn halve_rgba8(width: u32, height: u32, rgba: &[u8]) -> (u32, u32, Vec<u8>) {
    let (w, h) = ((width / 2).max(1), (height / 2).max(1));
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let (y0, y1) = box_window(y, h, height);
        for x in 0..w {
            let (x0, x1) = box_window(x, w, width);
            let n = (x1 - x0) * (y1 - y0);
            for c in 0..4 {
                let mut sum = 0u32;
                for sy in y0..y1 {
                    for sx in x0..x1 {
                        sum += u32::from(
                            rgba.get(((sy * width + sx) * 4) as usize + c)
                                .copied()
                                .unwrap_or(0),
                        );
                    }
                }
                out.push(((sum + n / 2) / n) as u8);
            }
        }
    }
    (w, h, out)
}

#[cfg(test)]
mod stream_tests {
    use regia_asset::{CookedTexture, MipStreamMeta, TextureCookUsage};

    use super::*;
    use crate::mip_stream::cooked_mip_stream;
    use crate::texture_compress::{TextureCookQuality, cook_texture_rgba8};

    fn cooked() -> CookedTexture {
        cook_texture_rgba8(
            32,
            32,
            &[60u8; 32 * 32 * 4],
            TextureCookUsage::ColorSrgb,
            "linux",
            TextureCookQuality::Low,
        )
        .expect("cook")
    }

    #[test]
    fn the_meta_file_reaches_the_cooked_texture() {
        let settings = TextureImporterSettings::from_meta_string(Some(
            "(stream: Some(false), stream_min_px: Some(512))",
        ));
        assert_eq!(settings.stream, Some(false));
        let stamped = settings.apply_stream(cooked()).expect("stamp");
        let meta = cooked_mip_stream(&stamped).expect("metadata");
        assert!(!meta.stream);
        assert_eq!(meta.min_resident_px, 512);
        assert_eq!(meta.detail_px, 16, "the cook's own finding stays");
    }

    #[test]
    fn no_choice_leaves_the_cook_output_and_its_cache_key_alone() {
        let settings = TextureImporterSettings::default();
        let before = cooked();
        let after = settings.apply_stream(before.clone()).expect("no-op");
        assert_eq!(before, after);
        assert_eq!(
            settings.fingerprint(),
            "usage=None;max=None;compress=None;quality=None",
            "the string existing cook caches were keyed on"
        );
        let chosen = TextureImporterSettings {
            stream: Some(false),
            ..TextureImporterSettings::default()
        };
        assert_ne!(chosen.fingerprint(), settings.fingerprint());
    }

    #[test]
    fn an_older_cook_gets_metadata_from_the_settings_alone() {
        let plain =
            crate::ktx2_write::write_ktx2_bcn(4, 4, crate::ktx2_write::BcnFormat::Bc7Srgb, &[vec![1u8; 16]])
                .expect("ktx2");
        let texture = CookedTexture {
            width: 4,
            height: 4,
            usage: TextureCookUsage::ColorSrgb,
            ktx2: plain.into(),
        };
        assert_eq!(cooked_mip_stream(&texture), None);
        let settings = TextureImporterSettings {
            stream_min_px: Some(64),
            ..TextureImporterSettings::default()
        };
        let stamped = settings.apply_stream(texture).expect("stamp");
        assert_eq!(
            cooked_mip_stream(&stamped),
            Some(MipStreamMeta {
                min_resident_px: 64,
                ..MipStreamMeta::new(0)
            })
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halving_an_odd_edge_keeps_the_last_texel_in_the_average() {
        // 3x1: reds 0, 0, 90 -> one sample averaging all three (the old filter read only 0, 0).
        let px = |r: u8| [r, 0, 0, 255];
        let rgba: Vec<u8> = [px(0), px(0), px(90)].concat();
        let (w, h, out) = halve_rgba8(3, 1, &rgba);
        assert_eq!((w, h), (1, 1));
        assert_eq!(out[0], 30);
        // 5x1: the first sample is texels 0..2, the last takes texels 2..5.
        let rgba: Vec<u8> = [px(10), px(10), px(0), px(0), px(60)].concat();
        let (w, _, out) = halve_rgba8(5, 1, &rgba);
        assert_eq!(w, 2);
        assert_eq!((out[0], out[4]), (10, 20));
    }

    #[test]
    fn file_names_pick_the_usage() {
        let u = |n: &str| usage_from_file_name(Path::new(n));
        assert_eq!(u("textures/hull_albedo.png"), TextureCookUsage::ColorSrgb);
        assert_eq!(u("hull_BaseColor.png"), TextureCookUsage::ColorSrgb);
        assert_eq!(u("hull_normal.png"), TextureCookUsage::NormalMap);
        assert_eq!(u("hull_N.png"), TextureCookUsage::NormalMap);
        assert_eq!(u("T_Rock-nrm.jpg"), TextureCookUsage::NormalMap);
        assert_eq!(u("hull_ORM.png"), TextureCookUsage::ColorLinear);
        assert_eq!(u("hull_roughness.png"), TextureCookUsage::ColorLinear);
        assert_eq!(u("hull ao.png"), TextureCookUsage::ColorLinear);
        assert_eq!(u("normal.png"), TextureCookUsage::NormalMap);
        // "n" inside a word is not a suffix.
        assert_eq!(u("canyon.png"), TextureCookUsage::ColorSrgb);
        assert_eq!(u(".png"), TextureCookUsage::ColorSrgb);
    }

    #[test]
    fn the_shared_table_covers_the_set_naming_variants() {
        let slot = |n: &str| texture_set_slot_from_file_name(Path::new(n));
        use TextureSetSlot::*;
        // Every documented variant lands in its slot.
        for (name, want) in [
            ("hull_BaseColor.png", BaseColor),
            ("hull_Albedo.png", BaseColor),
            ("hull_Diffuse.png", BaseColor),
            ("hull_Color.png", BaseColor),
            ("hull_Normal.png", Normal),
            ("hull_N.png", Normal),
            ("hull_Nrm.png", Normal),
            ("hull_NormalGL.png", Normal),
            ("hull_NormalDX.png", Normal),
            ("hull_Roughness.png", Roughness),
            ("hull_Rough.png", Roughness),
            ("hull_Metallic.png", Metallic),
            ("hull_Metal.png", Metallic),
            ("hull_AO.png", AmbientOcclusion),
            ("hull_Occlusion.png", AmbientOcclusion),
            ("hull_ORM.png", Orm),
            ("hull_ARM.png", Orm),
            ("hull_RMA.png", Orm),
            ("hull_MRA.png", Orm),
            ("hull_Height.png", Height),
            ("hull_Displacement.png", Height),
            ("hull_Emissive.png", Emissive),
            ("hull_Emission.png", Emissive),
            ("hull_Opacity.png", Opacity),
            ("hull_Alpha.png", Opacity),
            ("hull_Mask.png", Opacity),
        ] {
            assert_eq!(slot(name), Some(want), "{name}");
        }
        // Cook-only words never join a set; unknown words have no slot.
        assert_eq!(slot("hull_RM.png"), None);
        assert_eq!(slot("hull_Specular.png"), None);
        assert_eq!(slot("hull.png"), None);
        // Usage inference is unchanged for cook-only words and follows the table elsewhere.
        let u = |n: &str| usage_from_file_name(Path::new(n));
        assert_eq!(u("hull_RM.png"), TextureCookUsage::ColorLinear);
        assert_eq!(u("hull_Mask.png"), TextureCookUsage::ColorLinear);
        assert_eq!(u("hull_Emissive.png"), TextureCookUsage::ColorSrgb);
        assert_eq!(u("hull_NormalDX.png"), TextureCookUsage::NormalMap);
    }

    #[test]
    fn packed_suffixes_name_their_channel_order() {
        let order = |n: &str| orm_channel_order_from_file_name(Path::new(n));
        assert_eq!(order("hull_ORM.png"), OrmChannelOrderSetting::Orm);
        assert_eq!(order("hull_ARM.png"), OrmChannelOrderSetting::Arm);
        assert_eq!(order("hull_RMA.png"), OrmChannelOrderSetting::Rma);
        assert_eq!(order("hull_MRA.png"), OrmChannelOrderSetting::Mra);
        assert_eq!(order("hull_Roughness.png"), OrmChannelOrderSetting::Orm);
    }

    #[test]
    fn channel_transforms_flip_green_and_swizzle_at_cook() {
        let px = |r: u8, g: u8, b: u8| [r, g, b, 255];
        // DirectX green flip on a normal map.
        let mut rgba: Vec<u8> = [px(128, 10, 200), px(128, 250, 200)].concat();
        apply_channel_transforms(
            &mut rgba,
            TextureCookUsage::NormalMap,
            NormalGreenSetting::Directx,
            OrmChannelOrderSetting::Orm,
        );
        assert_eq!(rgba, [px(128, 245, 200), px(128, 5, 200)].concat());
        // OpenGL leaves the texels alone.
        let mut rgba: Vec<u8> = [px(128, 10, 200)].concat();
        apply_channel_transforms(
            &mut rgba,
            TextureCookUsage::NormalMap,
            NormalGreenSetting::Opengl,
            OrmChannelOrderSetting::Orm,
        );
        assert_eq!(rgba, [px(128, 10, 200)].concat());
        // RMA (R roughness, G metallic, B AO) -> ORM (R AO, G roughness, B metallic).
        let mut rgba: Vec<u8> = [px(10, 20, 30)].concat();
        apply_channel_transforms(
            &mut rgba,
            TextureCookUsage::ColorLinear,
            NormalGreenSetting::Opengl,
            OrmChannelOrderSetting::Rma,
        );
        assert_eq!(rgba, [px(30, 10, 20)].concat());
        // MRA (R metallic, G roughness, B AO) -> ORM.
        let mut rgba: Vec<u8> = [px(10, 20, 30)].concat();
        apply_channel_transforms(
            &mut rgba,
            TextureCookUsage::ColorLinear,
            NormalGreenSetting::Opengl,
            OrmChannelOrderSetting::Mra,
        );
        assert_eq!(rgba, [px(30, 20, 10)].concat());
        // ARM and ORM are the native layout: untouched.
        let mut rgba: Vec<u8> = [px(10, 20, 30)].concat();
        apply_channel_transforms(
            &mut rgba,
            TextureCookUsage::ColorLinear,
            NormalGreenSetting::Opengl,
            OrmChannelOrderSetting::Arm,
        );
        assert_eq!(rgba, [px(10, 20, 30)].concat());
        // The swizzle only applies to linear data: a colour map is untouched.
        let mut rgba: Vec<u8> = [px(10, 20, 30)].concat();
        apply_channel_transforms(
            &mut rgba,
            TextureCookUsage::ColorSrgb,
            NormalGreenSetting::Opengl,
            OrmChannelOrderSetting::Rma,
        );
        assert_eq!(rgba, [px(10, 20, 30)].concat());
    }

    #[test]
    fn meta_settings_carry_the_channel_conventions() {
        let s = TextureImporterSettings::from_meta_string(Some(
            "(usage: Some(normal), normal_green: Some(directx))",
        ));
        assert_eq!(s.normal_green, Some(NormalGreenSetting::Directx));
        assert_eq!(s.normal_green_or_default(), NormalGreenSetting::Directx);
        assert_eq!(s.orm_channel_order_or_default(), OrmChannelOrderSetting::Orm);
        let s = TextureImporterSettings::from_meta_string(Some(
            "(usage: Some(linear), orm_channel_order: Some(rma))",
        ));
        assert_eq!(s.orm_channel_order, Some(OrmChannelOrderSetting::Rma));
        assert_eq!(s.orm_channel_order_or_default(), OrmChannelOrderSetting::Rma);
        // Unset keeps the defaults and the default fingerprint (existing cook caches stay valid).
        let none = TextureImporterSettings::default();
        assert_eq!(none.normal_green, None);
        assert_eq!(none.orm_channel_order, None);
        assert_eq!(none.normal_green_or_default(), NormalGreenSetting::Opengl);
        assert_ne!(s.fingerprint(), none.fingerprint());
    }

    #[test]
    fn meta_settings_override_and_default() {
        let none = TextureImporterSettings::from_meta_string(None);
        assert_eq!(none, TextureImporterSettings::default());
        assert!(none.compress());
        assert_eq!(
            none.usage_or(TextureCookUsage::NormalMap),
            TextureCookUsage::NormalMap
        );
        assert_eq!(
            none.quality_or(TextureCookQuality::High),
            TextureCookQuality::High
        );
        let s = TextureImporterSettings::from_meta_string(Some(
            "(usage: Some(linear), max_size: Some(1024), compress: Some(false), quality: Some(low))",
        ));
        assert_eq!(
            s.usage_or(TextureCookUsage::ColorSrgb),
            TextureCookUsage::ColorLinear
        );
        assert_eq!(s.max_size, Some(1024));
        assert!(!s.compress());
        assert_eq!(s.quality_or(TextureCookQuality::High), TextureCookQuality::Low);
        assert_ne!(s.fingerprint(), none.fingerprint());
        // Partial settings; a malformed string falls back to the defaults.
        let partial = TextureImporterSettings::from_meta_string(Some("(usage: Some(normal))"));
        assert_eq!(partial.usage, Some(TextureUsageSetting::Normal));
        assert!(partial.compress());
        assert_eq!(
            TextureImporterSettings::from_meta_string(Some("(usage: 7")),
            TextureImporterSettings::default()
        );
    }

    #[test]
    fn max_size_halves_until_it_fits() {
        let s = TextureImporterSettings {
            max_size: Some(2),
            ..Default::default()
        };
        // 8×4 → 4×2 → 2×1.
        let rgba: Vec<u8> = (0..8 * 4).flat_map(|i| [i as u8, 0, 0, 255]).collect();
        let (w, h, out) = s.fit(8, 4, rgba.clone());
        assert_eq!((w, h), (2, 1));
        assert_eq!(out.len(), 2 * 4);
        assert!(out.chunks(4).all(|p| p[3] == 255));
        // Already small enough, or no cap: untouched.
        assert_eq!(s.fit(2, 2, vec![1; 16]).2, vec![1; 16]);
        assert_eq!(TextureImporterSettings::default().fit(8, 4, rgba.clone()).2, rgba);
    }

    #[test]
    fn a_source_without_meta_has_the_defaults() {
        let dir = std::env::temp_dir().join(format!("regia_tex_settings_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let png = dir.join("hull_normal.png");
        assert_eq!(
            TextureImporterSettings::for_source(&png),
            TextureImporterSettings::default()
        );
        let mut meta = regia_asset::MetaSidecar::new("png", regia_asset::AssetId::new_random());
        meta.settings = Some("(max_size: Some(512))".into());
        std::fs::write(
            regia_asset::meta_path_for(&png),
            meta.to_ron_string().expect("ron"),
        )
        .expect("meta");
        assert_eq!(TextureImporterSettings::for_source(&png).max_size, Some(512));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
