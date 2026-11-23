// SPDX-License-Identifier: MPL-2.0
//! Cooked photoreal terrain (ADR-8024, spec 26 § 7, `docs/formats/terrain.md` § 14): the exact
//! final heights, holes and photoreal material a host's package step cooks into a terrain map,
//! and the [`PakTileSource`] readers that hand them to [`crate::gpu::TerrainSharedDraw`] without
//! rebuilding anything at play.
//!
//! - `terrain/heights` (`terrain-final-heights` v1): the heights as f32 residuals over the R16
//!   heightfield, or raw; either way they decode to the cooked heights bit for bit.
//! - `terrain/holes`: the `.holes.ron` text (the frozen `terrain-holes` format and its migrator).
//! - `terrain/material/<kind>/<i>`: one KTX2 `CookedTexture` per blob of
//!   [`CookedMaterialKtx2`]; [`PakTileSource::load_cooked_material`] returns them as views of
//!   the mapped pak ([`PakPayload`]), so loading allocates per blob, never per byte.

use regia_asset::{AssetId, PakPayload};

use crate::holes::HolesMask;
use crate::material::{CookedMaterialKtx2, MaterialParams};
use crate::stream::{
    PakTileSource, TerrainHeightsEncoding, TerrainHeightsIndex, TerrainIndex, TerrainIndexError,
};

/// Version of the cooked final-heights payload (`terrain-final-heights`).
pub const TERRAIN_FINAL_HEIGHTS_FORMAT_VERSION: u32 = 1;

/// Magic of the final-heights payload.
pub const TERRAIN_FINAL_HEIGHTS_MAGIC: [u8; 4] = *b"RTFH";

/// The reflect type path of a cooked final-heights file (its `.fckd` type tag).
pub const TERRAIN_FINAL_HEIGHTS_TYPE_PATH: &str = "regia_terrain::stream::TerrainFinalHeights";

/// The reflect type path of a cooked holes file (its `.fckd` type tag).
pub const TERRAIN_COOKED_HOLES_TYPE_PATH: &str = "regia_terrain::stream::TerrainCookedHoles";

/// Bytes before the values of a final-heights payload.
pub const FINAL_HEIGHTS_HEADER_LEN: usize = 28;

/// Versions the final-heights reader migrates (none yet: v1 is the first).
#[must_use]
pub fn final_heights_migration_sources() -> &'static [u32] {
    &[]
}

/// Fingerprint of the final-heights layout (`cargo xtask check-formats`).
#[must_use]
pub fn final_heights_schema_fingerprint() -> String {
    concat!(
        "TerrainFinalHeights{magic:RTFH,version:u32le,width:u32le,height:u32le,",
        "encoding:u8{0:Residual,1:Raw},pad:[u8;3],height_scale:f32le,height_offset:f32le,",
        "values:[f32le;width*height]}"
    )
    .to_string()
}

/// The asset path of a map's exact heights.
#[must_use]
pub fn final_heights_asset_path() -> String {
    "terrain/heights".to_string()
}

/// The deterministic [`AssetId`] of a map's exact heights.
#[must_use]
pub fn final_heights_asset_id(map_id: AssetId) -> AssetId {
    AssetId::sub_asset(map_id, &final_heights_asset_path())
}

/// The asset path of a map's cooked holes.
#[must_use]
pub fn cooked_holes_asset_path() -> String {
    "terrain/holes".to_string()
}

/// The deterministic [`AssetId`] of a map's cooked holes.
#[must_use]
pub fn cooked_holes_asset_id(map_id: AssetId) -> AssetId {
    AssetId::sub_asset(map_id, &cooked_holes_asset_path())
}

/// The kinds of blob in a cooked photoreal material (one per field of [`CookedMaterialKtx2`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MaterialBlobKind {
    /// [`CookedMaterialKtx2::layer_albedo`].
    LayerAlbedo,
    /// [`CookedMaterialKtx2::layer_normal`].
    LayerNormal,
    /// [`CookedMaterialKtx2::layer_height`].
    LayerHeight,
    /// [`CookedMaterialKtx2::layer_orm`].
    LayerOrm,
    /// [`CookedMaterialKtx2::macro_albedo_tiles`].
    MacroAlbedo,
    /// [`CookedMaterialKtx2::macro_normal_tiles`].
    MacroNormal,
    /// [`CookedMaterialKtx2::macro_coarse_albedo`] (one blob, index ignored).
    MacroCoarseAlbedo,
    /// [`CookedMaterialKtx2::macro_coarse_normal`] (one blob, index ignored).
    MacroCoarseNormal,
}

impl MaterialBlobKind {
    /// Every kind, in the order a cook writes them.
    pub const ALL: [Self; 8] = [
        Self::LayerAlbedo,
        Self::LayerNormal,
        Self::LayerHeight,
        Self::LayerOrm,
        Self::MacroAlbedo,
        Self::MacroNormal,
        Self::MacroCoarseAlbedo,
        Self::MacroCoarseNormal,
    ];

    /// The stable name in the blob's asset path.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::LayerAlbedo => "layer_albedo",
            Self::LayerNormal => "layer_normal",
            Self::LayerHeight => "layer_height",
            Self::LayerOrm => "layer_orm",
            Self::MacroAlbedo => "macro_albedo",
            Self::MacroNormal => "macro_normal",
            Self::MacroCoarseAlbedo => "macro_coarse_albedo",
            Self::MacroCoarseNormal => "macro_coarse_normal",
        }
    }

    /// Whether the kind is a single blob (the coarse fallbacks).
    #[must_use]
    pub fn is_single(self) -> bool {
        matches!(self, Self::MacroCoarseAlbedo | Self::MacroCoarseNormal)
    }
}

/// The asset path of material blob `index` of `kind`: `terrain/material/<kind>/<index>`, or
/// `terrain/material/<kind>` for the single coarse blobs.
#[must_use]
pub fn material_blob_asset_path(kind: MaterialBlobKind, index: u32) -> String {
    if kind.is_single() {
        format!("terrain/material/{}", kind.name())
    } else {
        format!("terrain/material/{}/{index}", kind.name())
    }
}

/// The deterministic [`AssetId`] of material blob `index` of `kind`.
#[must_use]
pub fn material_blob_asset_id(map_id: AssetId, kind: MaterialBlobKind, index: u32) -> AssetId {
    AssetId::sub_asset(map_id, &material_blob_asset_path(kind, index))
}

/// The height of R16 sample `s`: the [`crate::TerrainMap::new`] arithmetic, bit for bit.
#[must_use]
pub fn r16_height(sample: u16, height_scale: f32, height_offset: f32) -> f32 {
    f32::from(sample) / 65535.0 * height_scale + height_offset
}

/// The R16 mapping that spans `heights`: offset = the lowest height, scale = the span (1 m for a
/// flat map). `None` when a height is not finite or the slice is empty.
#[must_use]
pub fn height_mapping_of(heights: &[f32]) -> Option<(f32, f32)> {
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for &h in heights {
        if !h.is_finite() {
            return None;
        }
        lo = lo.min(h);
        hi = hi.max(h);
    }
    if heights.is_empty() {
        return None;
    }
    let span = hi - lo;
    Some((if span > 0.0 && span.is_finite() { span } else { 1.0 }, lo))
}

/// Quantizes heights to R16 samples with a mapping: `round((h - offset) · 65535 / scale)`,
/// clamped to `0..=65535` (the heightmap export rule, `docs/formats/terrain.md` § 11).
#[must_use]
pub fn quantize_heights(heights: &[f32], height_scale: f32, height_offset: f32) -> Vec<u16> {
    heights
        .iter()
        .map(|&h| {
            if height_scale == 0.0 {
                return 0;
            }
            let s = ((h - height_offset) * 65535.0 / height_scale).round();
            s.clamp(0.0, 65535.0) as u16
        })
        .collect()
}

/// Encodes exact heights as a final-heights payload over the R16 samples `r16` (the grid
/// `terrain/heightfield` holds): residuals when `base + r` gives back the bits of every height,
/// raw heights otherwise. Returns the chosen encoding and the payload bytes.
///
/// # Errors
///
/// [`TerrainIndexError::BadHeights`] when the sizes disagree or a height is not finite.
pub fn encode_final_heights(
    width: u32,
    height: u32,
    heights: &[f32],
    r16: &[u16],
    height_scale: f32,
    height_offset: f32,
) -> Result<(TerrainHeightsEncoding, Vec<u8>), TerrainIndexError> {
    let count = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| TerrainIndexError::BadHeights("size overflows".into()))?;
    if heights.len() != count || r16.len() != count {
        return Err(TerrainIndexError::BadHeights(format!(
            "{} heights and {} samples for a {width}x{height} grid",
            heights.len(),
            r16.len()
        )));
    }
    if heights.iter().any(|h| !h.is_finite()) {
        return Err(TerrainIndexError::BadHeights("a height is not finite".into()));
    }
    let residual: Option<Vec<f32>> = heights
        .iter()
        .zip(r16)
        .map(|(&h, &s)| {
            let base = r16_height(s, height_scale, height_offset);
            let r = h - base;
            ((base + r).to_bits() == h.to_bits()).then_some(r)
        })
        .collect();
    let (encoding, values) = match residual {
        Some(values) => (TerrainHeightsEncoding::Residual, values),
        None => (TerrainHeightsEncoding::Raw, heights.to_vec()),
    };
    let payload = FinalHeightsPayload {
        width,
        height,
        encoding,
        height_scale,
        height_offset,
        values,
    };
    Ok((encoding, payload.to_bytes()))
}

/// A final-heights payload as stored: the header fields and the stored values (heights for
/// [`TerrainHeightsEncoding::Raw`], residuals for [`TerrainHeightsEncoding::Residual`]).
#[derive(Clone, Debug, PartialEq)]
pub struct FinalHeightsPayload {
    /// Width in samples.
    pub width: u32,
    /// Height in samples.
    pub height: u32,
    /// How [`Self::values`] are stored.
    pub encoding: TerrainHeightsEncoding,
    /// Metres of a full-scale R16 sample.
    pub height_scale: f32,
    /// Metres of a zero R16 sample.
    pub height_offset: f32,
    /// The stored values, row-major.
    pub values: Vec<f32>,
}

impl FinalHeightsPayload {
    /// The payload bytes (`docs/formats/terrain.md` § 14).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(FINAL_HEIGHTS_HEADER_LEN + self.values.len() * 4);
        out.extend_from_slice(&TERRAIN_FINAL_HEIGHTS_MAGIC);
        out.extend_from_slice(&TERRAIN_FINAL_HEIGHTS_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.push(match self.encoding {
            TerrainHeightsEncoding::Residual => 0,
            TerrainHeightsEncoding::Raw => 1,
        });
        out.extend_from_slice(&[0; 3]);
        out.extend_from_slice(&self.height_scale.to_le_bytes());
        out.extend_from_slice(&self.height_offset.to_le_bytes());
        for v in &self.values {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    /// Parses payload bytes. The version is read before the body: a newer payload is refused
    /// with its version.
    ///
    /// # Errors
    ///
    /// [`TerrainIndexError::BadHeights`] for a bad magic, a newer or unknown version, an unknown
    /// encoding, non-zero padding, a body that is not `width × height` values, or a non-finite
    /// value, scale or offset.
    pub fn parse(payload: &[u8]) -> Result<Self, TerrainIndexError> {
        let bad = TerrainIndexError::BadHeights;
        if payload.len() < FINAL_HEIGHTS_HEADER_LEN || payload[..4] != TERRAIN_FINAL_HEIGHTS_MAGIC {
            return Err(bad("not a final-heights payload (magic RTFH)".into()));
        }
        let version = u32_at(payload, 4);
        if version != TERRAIN_FINAL_HEIGHTS_FORMAT_VERSION {
            return Err(bad(format!(
                "final heights version {version} (this build reads {TERRAIN_FINAL_HEIGHTS_FORMAT_VERSION}); upgrade the engine or re-cook"
            )));
        }
        let width = u32_at(payload, 8);
        let height = u32_at(payload, 12);
        let encoding = match payload[16] {
            0 => TerrainHeightsEncoding::Residual,
            1 => TerrainHeightsEncoding::Raw,
            other => return Err(bad(format!("unknown encoding {other}"))),
        };
        if payload[17..20] != [0; 3] {
            return Err(bad("padding is not zero".into()));
        }
        let height_scale = f32::from_le_bytes([payload[20], payload[21], payload[22], payload[23]]);
        let height_offset = f32::from_le_bytes([payload[24], payload[25], payload[26], payload[27]]);
        if !height_scale.is_finite() || !height_offset.is_finite() {
            return Err(bad("height scale and offset must be finite".into()));
        }
        let count = (width as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| bad("size overflows".into()))?;
        let body = &payload[FINAL_HEIGHTS_HEADER_LEN..];
        if !body.len().is_multiple_of(4) || body.len() / 4 != count {
            return Err(bad(format!("{} body bytes for {count} heights", body.len())));
        }
        let (words, _) = body.as_chunks::<4>();
        let values: Vec<f32> = words.iter().map(|w| f32::from_le_bytes(*w)).collect();
        if values.iter().any(|v| !v.is_finite()) {
            return Err(bad("a stored value is not finite".into()));
        }
        Ok(Self {
            width,
            height,
            encoding,
            height_scale,
            height_offset,
            values,
        })
    }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

/// Decodes a final-heights payload to the exact heights, checking it against the index entry
/// `expected` (size, encoding, mapping) and, for residuals, adding them to the R16 samples `r16`.
///
/// # Errors
///
/// [`TerrainIndexError::BadHeights`] for a bad magic, a newer or unknown version, an unknown
/// encoding, a size or mapping that disagrees with the index or the R16 grid, a short or long
/// body, or a height that is not finite.
pub fn decode_final_heights(
    payload: &[u8],
    expected: &TerrainHeightsIndex,
    r16: Option<&[u16]>,
) -> Result<Vec<f32>, TerrainIndexError> {
    let bad = TerrainIndexError::BadHeights;
    let stored = FinalHeightsPayload::parse(payload)?;
    if stored.width != expected.width
        || stored.height != expected.height
        || stored.encoding != expected.encoding
        || stored.height_scale.to_bits() != expected.height_scale.to_bits()
        || stored.height_offset.to_bits() != expected.height_offset.to_bits()
    {
        return Err(bad("the payload does not match its terrain index entry".into()));
    }
    let heights: Vec<f32> = match stored.encoding {
        TerrainHeightsEncoding::Raw => stored.values,
        TerrainHeightsEncoding::Residual => {
            let r16 = r16.ok_or_else(|| bad("residual heights need the R16 heightfield".into()))?;
            if r16.len() != stored.values.len() {
                return Err(bad(format!(
                    "{} R16 samples for {} heights",
                    r16.len(),
                    stored.values.len()
                )));
            }
            stored
                .values
                .iter()
                .zip(r16)
                .map(|(r, &s)| r16_height(s, stored.height_scale, stored.height_offset) + r)
                .collect()
        }
    };
    if heights.iter().any(|h| !h.is_finite()) {
        return Err(bad("a height is not finite".into()));
    }
    Ok(heights)
}

/// A cooked photoreal material read from a terrain map: its blobs (views of the mapped pak when
/// the pak stores them uncompressed), its knobs and its macro grid side, ready for
/// [`crate::gpu::TerrainSharedDraw::set_material_cooked`].
#[derive(Clone, Debug)]
pub struct CookedTerrainMaterial {
    /// The KTX2 blobs.
    pub blobs: CookedMaterialKtx2<PakPayload>,
    /// The material knobs.
    pub params: MaterialParams,
    /// Macro tile grid side (`MacroBake::tiles`); 0 = no macro set.
    pub macro_tiles: u32,
}

impl PakTileSource {
    /// Reads `terrain/heights` and decodes the exact final heights (ADR-8024), or `None` when the
    /// index has none (a standard cook). `heightfield` is the stitched R16 grid
    /// ([`Self::load_heightfield`]); residual heights need it.
    ///
    /// # Errors
    ///
    /// [`TerrainIndexError`] when the file is missing, has the wrong type or does not decode.
    pub fn load_final_heights(
        &self,
        index: &TerrainIndex,
        heightfield: Option<&regia_asset::Heightfield>,
    ) -> Result<Option<Vec<f32>>, TerrainIndexError> {
        let Some(entry) = &index.heights else {
            return Ok(None);
        };
        let bytes = self.read_bytes(final_heights_asset_id(index.map_id))?;
        let payload = cooked_payload(&bytes, TERRAIN_FINAL_HEIGHTS_TYPE_PATH)?;
        decode_final_heights(payload, entry, heightfield.map(|h| h.samples.as_slice())).map(Some)
    }

    /// Reads `terrain/holes` (the `.holes.ron` text, migrated by its own reader), or `None` when
    /// the index has none.
    ///
    /// # Errors
    ///
    /// [`TerrainIndexError`] when the file is missing, has the wrong type or does not parse.
    pub fn load_holes(&self, index: &TerrainIndex) -> Result<Option<HolesMask>, TerrainIndexError> {
        if index.holes.is_none() {
            return Ok(None);
        }
        let bytes = self.read_bytes(cooked_holes_asset_id(index.map_id))?;
        let payload = cooked_payload(&bytes, TERRAIN_COOKED_HOLES_TYPE_PATH)?;
        let text = std::str::from_utf8(payload).map_err(|e| TerrainIndexError::BadHoles(e.to_string()))?;
        HolesMask::from_holes_ron(text)
            .map(Some)
            .map_err(|e| TerrainIndexError::BadHoles(e.to_string()))
    }

    /// Reads every blob of the cooked photoreal material (`terrain/material/...`), or `None` when
    /// the index has none. A blob stored uncompressed in a mapped pak is a view of the pak
    /// ([`PakPayload::is_mapped`]): only the slot vectors are allocated, never a blob's bytes.
    ///
    /// # Errors
    ///
    /// [`TerrainIndexError::BadMaterial`] when a blob is missing or is not a cooked texture.
    pub fn load_cooked_material(
        &self,
        index: &TerrainIndex,
    ) -> Result<Option<CookedTerrainMaterial>, TerrainIndexError> {
        let Some(entry) = &index.material else {
            return Ok(None);
        };
        let blob = |kind: MaterialBlobKind, i: u32| -> Result<PakPayload, TerrainIndexError> {
            let path = material_blob_asset_path(kind, i);
            let bytes = self
                .read_bytes(material_blob_asset_id(index.map_id, kind, i))
                .map_err(|e| TerrainIndexError::BadMaterial(format!("{path}: {e}")))?;
            let (_, texture) = regia_asset::load_texture_shared(&bytes)
                .map_err(|e| TerrainIndexError::BadMaterial(format!("{path}: {e}")))?;
            Ok(texture.ktx2)
        };
        let list = |kind: MaterialBlobKind, count: u32| -> Result<Vec<PakPayload>, TerrainIndexError> {
            (0..count).map(|i| blob(kind, i)).collect()
        };
        let single =
            |kind: MaterialBlobKind, present: bool| -> Result<Option<PakPayload>, TerrainIndexError> {
                present.then(|| blob(kind, 0)).transpose()
            };
        let blobs = CookedMaterialKtx2 {
            layer_albedo: list(MaterialBlobKind::LayerAlbedo, entry.layer_albedo)?,
            layer_normal: list(MaterialBlobKind::LayerNormal, entry.layer_normal)?,
            layer_height: list(MaterialBlobKind::LayerHeight, entry.layer_height)?,
            layer_orm: list(MaterialBlobKind::LayerOrm, entry.layer_orm)?,
            macro_albedo_tiles: list(MaterialBlobKind::MacroAlbedo, entry.macro_albedo_tiles)?,
            macro_normal_tiles: list(MaterialBlobKind::MacroNormal, entry.macro_normal_tiles)?,
            macro_coarse_albedo: single(MaterialBlobKind::MacroCoarseAlbedo, entry.macro_coarse_albedo)?,
            macro_coarse_normal: single(MaterialBlobKind::MacroCoarseNormal, entry.macro_coarse_normal)?,
        };
        Ok(Some(CookedTerrainMaterial {
            blobs,
            params: entry.params.into(),
            macro_tiles: entry.macro_tiles,
        }))
    }
}

/// The payload of a cooked `.fckd` file whose type tag is `type_path`.
fn cooked_payload<'a>(bytes: &'a [u8], type_path: &str) -> Result<&'a [u8], TerrainIndexError> {
    let (header, payload) = regia_asset::decode_cooked_file(bytes)?;
    let expected = regia_core::id::TypePathHash::of(type_path);
    if header.type_tag != expected {
        return Err(TerrainIndexError::WrongType {
            id: header.asset_id,
            found: header.type_tag.get(),
        });
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        width: u32,
        height: u32,
        encoding: TerrainHeightsEncoding,
        scale: f32,
        offset: f32,
    ) -> TerrainHeightsIndex {
        TerrainHeightsIndex {
            width,
            height,
            encoding,
            height_scale: scale,
            height_offset: offset,
            bytes: 0,
        }
    }

    /// Fractional heights (a sunk shell, a mirrored ridge) that R16 cannot hold come back bit
    /// for bit through the residual encoding.
    #[test]
    fn residual_heights_round_trip_bit_exactly() {
        let (w, h) = (17u32, 9u32);
        let heights: Vec<f32> = (0..w * h)
            .map(|i| 12.0 + (i as f32 * 0.731).sin() * 7.3 + (i % 5) as f32 * 0.0001234)
            .collect();
        let (scale, offset) = height_mapping_of(&heights).expect("mapping");
        let r16 = quantize_heights(&heights, scale, offset);
        let (encoding, bytes) = encode_final_heights(w, h, &heights, &r16, scale, offset).expect("encode");
        assert_eq!(encoding, TerrainHeightsEncoding::Residual);
        let back =
            decode_final_heights(&bytes, &entry(w, h, encoding, scale, offset), Some(&r16)).expect("decode");
        let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&back), bits(&heights));
        // The R16 grid alone does not hold them: that is what the residuals add.
        let r16_only: Vec<f32> = r16.iter().map(|&s| r16_height(s, scale, offset)).collect();
        assert_ne!(bits(&r16_only), bits(&heights));
    }

    /// A height whose residual does not restore its bits (-0.0 over a 0.0 base) makes the whole
    /// map raw, and raw is exact too.
    #[test]
    fn heights_a_residual_cannot_restore_are_stored_raw() {
        let heights = vec![-0.0f32, 1.0, 2.0, 3.0];
        let r16 = quantize_heights(&heights, 3.0, 0.0);
        let (encoding, bytes) = encode_final_heights(2, 2, &heights, &r16, 3.0, 0.0).expect("encode");
        assert_eq!(encoding, TerrainHeightsEncoding::Raw);
        let back = decode_final_heights(&bytes, &entry(2, 2, encoding, 3.0, 0.0), None).expect("decode");
        assert_eq!(back[0].to_bits(), (-0.0f32).to_bits());
        assert_eq!(&back[1..], &heights[1..]);
    }

    /// The reader refuses every malformed payload instead of guessing.
    #[test]
    fn malformed_final_heights_are_refused() {
        let heights = vec![1.0f32, 2.0, 3.0, 4.0];
        let r16 = quantize_heights(&heights, 3.0, 1.0);
        let (encoding, bytes) = encode_final_heights(2, 2, &heights, &r16, 3.0, 1.0).expect("encode");
        let ok = entry(2, 2, encoding, 3.0, 1.0);
        assert!(decode_final_heights(&bytes, &ok, Some(&r16)).is_ok());
        let refuse = |b: &[u8], e: &TerrainHeightsIndex, r: Option<&[u16]>| {
            assert!(matches!(
                decode_final_heights(b, e, r),
                Err(TerrainIndexError::BadHeights(_))
            ));
        };
        let mut magic = bytes.clone();
        magic[0] = b'X';
        refuse(&magic, &ok, Some(&r16));
        let mut newer = bytes.clone();
        newer[4..8].copy_from_slice(&2u32.to_le_bytes());
        refuse(&newer, &ok, Some(&r16));
        let mut unknown = bytes.clone();
        unknown[16] = 7;
        refuse(&unknown, &ok, Some(&r16));
        refuse(&bytes[..bytes.len() - 1], &ok, Some(&r16));
        refuse(&bytes[..10], &ok, Some(&r16));
        refuse(&bytes, &entry(4, 1, encoding, 3.0, 1.0), Some(&r16));
        refuse(&bytes, &entry(2, 2, encoding, 3.5, 1.0), Some(&r16));
        refuse(&bytes, &ok, None);
        refuse(&bytes, &ok, Some(&r16[..3]));
        assert!(encode_final_heights(2, 2, &[1.0, f32::NAN, 0.0, 0.0], &r16, 3.0, 1.0).is_err());
        assert!(encode_final_heights(3, 2, &heights, &r16, 3.0, 1.0).is_err());
    }

    /// Blob paths are stable: the cooker and the reader agree on every id.
    #[test]
    fn material_blob_paths_are_stable() {
        assert_eq!(
            material_blob_asset_path(MaterialBlobKind::LayerOrm, 3),
            "terrain/material/layer_orm/3"
        );
        assert_eq!(
            material_blob_asset_path(MaterialBlobKind::MacroCoarseNormal, 9),
            "terrain/material/macro_coarse_normal"
        );
        assert_eq!(final_heights_asset_path(), "terrain/heights");
        assert_eq!(cooked_holes_asset_path(), "terrain/holes");
        let map = AssetId::sub_asset(AssetId::NIL, "map");
        assert_ne!(
            material_blob_asset_id(map, MaterialBlobKind::LayerAlbedo, 0),
            material_blob_asset_id(map, MaterialBlobKind::LayerAlbedo, 1)
        );
    }
}
