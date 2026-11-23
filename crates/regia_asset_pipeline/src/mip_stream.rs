// SPDX-License-Identifier: MPL-2.0
//! The cook's half of mip streaming metadata (T2.14, ADR-1942): which levels of a chain add
//! detail, and the KTX2 key/value entry that carries the answer ([`regia_asset::MipStreamMeta`]).
//!
//! A level adds detail when it differs from the bilinear magnification of the next smaller level:
//! that magnification is what the sampler shows if the level is not resident. The test is on the
//! largest difference of any channel of any texel, not on an average, so a small logo on a flat
//! field keeps its levels; only smooth or flat maps are capped.

use std::collections::BTreeMap;

use ktx2::{Header, LevelIndex};
use regia_asset::{CookedTexture, MIP_STREAM_KEY, MipStreamMeta};

use crate::ktx2_write::Ktx2WriteError;

/// 8-bit steps a texel may differ by and still count as no detail: the mip filter's own rounding.
pub const DETAIL_PEAK: f32 = 2.0;

/// The metadata never caps a texture below this edge: too few texels to be worth a level, and
/// block-compressed chains stop near it.
pub const MIN_DETAIL_PX: u32 = 16;

/// Level data starts on this alignment: the largest block size in the KTX2 rule
/// `lcm(texel block size, 4)` (BC7).
const LEVEL_ALIGN: usize = 16;

fn adds_detail(fine: &(u32, u32, Vec<u8>), coarse: &(u32, u32, Vec<u8>)) -> bool {
    let (fw, fh, fine) = (fine.0 as usize, fine.1 as usize, &fine.2);
    let (cw, ch, coarse) = (coarse.0 as usize, coarse.1 as usize, &coarse.2);
    let texel = |x: usize, y: usize, channel: usize| f32::from(coarse[(y * cw + x) * 4 + channel]);
    // Source position and weight of one axis, texel centers to texel centers, clamped at the edge.
    let axis = |i: usize, size: usize, csize: usize| {
        let at = ((i as f32 + 0.5) * csize as f32 / size as f32 - 0.5).max(0.0);
        let low = (at as usize).min(csize - 1);
        (low, (low + 1).min(csize - 1), at - low as f32)
    };
    for y in 0..fh {
        let (y0, y1, wy) = axis(y, fh, ch);
        for x in 0..fw {
            let (x0, x1, wx) = axis(x, fw, cw);
            for channel in 0..4 {
                let top = texel(x0, y0, channel) * (1.0 - wx) + texel(x1, y0, channel) * wx;
                let bottom = texel(x0, y1, channel) * (1.0 - wx) + texel(x1, y1, channel) * wx;
                let shown = top * (1.0 - wy) + bottom * wy;
                if (f32::from(fine[(y * fw + x) * 4 + channel]) - shown).abs() > DETAIL_PEAK {
                    return true;
                }
            }
        }
    }
    false
}

/// The largest edge at which a level of `mips` (level 0 first, `(width, height, RGBA8)`) adds
/// detail, at least [`MIN_DETAIL_PX`]; the top level's edge when it adds detail itself, so most
/// real textures stop at the first texels they look at.
#[must_use]
pub fn detail_px(mips: &[(u32, u32, Vec<u8>)]) -> u32 {
    for pair in mips.windows(2) {
        let edge = pair[0].0.max(pair[0].1);
        if edge <= MIN_DETAIL_PX || adds_detail(&pair[0], &pair[1]) {
            return edge;
        }
    }
    mips.last().map_or(0, |m| m.0.max(m.1))
}

/// `ktx2` with `key` set to `value`: the other entries stay, sorted by key as the format asks,
/// and every level moves by what the key/value section grew. The level bytes are untouched.
///
/// # Errors
/// `ktx2` does not parse, is supercompressed, or its sections are not laid out the way this crate
/// writes them.
pub fn set_key_value(ktx2_bytes: &[u8], key: &str, value: &[u8]) -> Result<Vec<u8>, Ktx2WriteError> {
    let malformed = |what: &str| Ktx2WriteError::Dfd(what.to_owned());
    let reader = ktx2::Reader::new(ktx2_bytes).map_err(|e| Ktx2WriteError::Dfd(e.to_string()))?;
    let mut header = reader.header();
    if header.supercompression_scheme.is_some() || header.index.sgd_byte_length != 0 {
        return Err(malformed("supercompressed files are not rewritten"));
    }
    let mut entries: BTreeMap<&str, &[u8]> = reader.key_value_data().collect();
    entries.insert(key, value);
    let mut kvd = Vec::new();
    for (key, value) in &entries {
        let length = key.len() + 1 + value.len();
        kvd.extend_from_slice(&(length as u32).to_le_bytes());
        kvd.extend_from_slice(key.as_bytes());
        kvd.push(0);
        kvd.extend_from_slice(value);
        kvd.resize(kvd.len().next_multiple_of(4), 0);
    }

    let levels = header.level_count.max(1) as usize;
    let index_end = Header::LENGTH + levels * LevelIndex::LENGTH;
    let dfd = header.index.dfd_byte_offset as usize;
    let dfd_end = dfd + header.index.dfd_byte_length as usize;
    let index_bytes = ktx2_bytes
        .get(Header::LENGTH..index_end)
        .ok_or_else(|| malformed("level index past the end"))?;
    let mut level_index: Vec<LevelIndex> = Vec::with_capacity(levels);
    for entry in index_bytes.as_chunks::<{ LevelIndex::LENGTH }>().0 {
        level_index.push(LevelIndex::from_bytes(entry));
    }
    let first = level_index
        .iter()
        .map(|l| l.byte_offset)
        .min()
        .ok_or_else(|| malformed("no levels"))? as usize;
    let dfd_bytes = ktx2_bytes
        .get(dfd..dfd_end)
        .ok_or_else(|| malformed("DFD past the end"))?;
    let payload = ktx2_bytes
        .get(first..)
        .ok_or_else(|| malformed("levels past the end"))?;
    if dfd != index_end {
        return Err(malformed("the DFD does not follow the level index"));
    }

    let data = (dfd_end + kvd.len()).next_multiple_of(LEVEL_ALIGN);
    header.index.kvd_byte_offset = dfd_end as u32;
    header.index.kvd_byte_length = kvd.len() as u32;
    for level in &mut level_index {
        level.byte_offset = level.byte_offset - first as u64 + data as u64;
    }
    let mut out = Vec::with_capacity(data + payload.len());
    out.extend_from_slice(&header.as_bytes());
    for level in &level_index {
        out.extend_from_slice(&level.as_bytes());
    }
    out.extend_from_slice(dfd_bytes);
    out.extend_from_slice(&kvd);
    out.resize(data, 0);
    out.extend_from_slice(payload);
    Ok(out)
}

/// `ktx2` carrying `meta`.
///
/// # Errors
/// As [`set_key_value`].
pub fn stamp_mip_stream(ktx2_bytes: &[u8], meta: &MipStreamMeta) -> Result<Vec<u8>, Ktx2WriteError> {
    set_key_value(ktx2_bytes, MIP_STREAM_KEY, &meta.to_value())
}

/// The metadata a cooked texture carries.
#[must_use]
pub fn cooked_mip_stream(texture: &CookedTexture) -> Option<MipStreamMeta> {
    let reader = ktx2::Reader::new(&texture.ktx2[..]).ok()?;
    MipStreamMeta::from_reader(&reader)
}

#[cfg(test)]
mod tests {
    use regia_asset::TextureCookUsage;

    use super::*;
    use crate::ktx2_write::{BcnFormat, write_ktx2_bcn};
    use crate::texture_compress::{TextureCookQuality, cook_texture_rgba8, generate_mips_rgba8};

    fn solid(size: u32, value: u8) -> Vec<u8> {
        vec![value; (size * size * 4) as usize]
    }

    /// Texel-to-texel noise from a small multiplicative hash: every level adds detail.
    fn noise(size: u32) -> Vec<u8> {
        (0..size * size * 4)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect()
    }

    /// A ramp over the whole image, a texel at a time at most 1 step: smooth at every level.
    fn ramp(size: u32) -> Vec<u8> {
        let mut out = Vec::new();
        for y in 0..size {
            for x in 0..size {
                let v = (x + y) * 64 / (2 * size);
                out.extend_from_slice(&[v as u8, v as u8, v as u8, 255]);
            }
        }
        out
    }

    #[test]
    fn a_flat_map_is_capped_at_the_small_edge() {
        let mips = generate_mips_rgba8(256, 256, &solid(256, 90));
        assert_eq!(detail_px(&mips), MIN_DETAIL_PX);
    }

    #[test]
    fn a_smooth_ramp_is_capped_and_noise_is_not() {
        let mips = generate_mips_rgba8(256, 256, &ramp(256));
        assert!(detail_px(&mips) < 256, "{}", detail_px(&mips));
        let mips = generate_mips_rgba8(256, 256, &noise(256));
        assert_eq!(detail_px(&mips), 256);
    }

    #[test]
    fn one_bright_texel_keeps_every_level() {
        let mut pixels = solid(256, 20);
        let at = (100 * 256 + 37) * 4;
        pixels[at] = 255;
        let mips = generate_mips_rgba8(256, 256, &pixels);
        assert_eq!(detail_px(&mips), 256, "an average would have called this flat");
    }

    #[test]
    fn a_non_square_chain_uses_its_largest_edge() {
        let pixels = vec![128u8; (64 * 16 * 4) as usize];
        let mips = generate_mips_rgba8(64, 16, &pixels);
        assert_eq!(detail_px(&mips), MIN_DETAIL_PX);
        let noisy: Vec<u8> = (0..64 * 16 * 4u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();
        assert_eq!(detail_px(&generate_mips_rgba8(64, 16, &noisy)), 64);
    }

    #[test]
    fn a_short_chain_answers_without_a_pair() {
        assert_eq!(detail_px(&[]), 0);
        assert_eq!(detail_px(&[(8, 8, solid(8, 1))]), 8);
    }

    fn sample_ktx2() -> Vec<u8> {
        write_ktx2_bcn(
            8,
            8,
            BcnFormat::Bc7Srgb,
            &[vec![7u8; 64], vec![8u8; 16], vec![9u8; 16]],
        )
        .expect("ktx2")
    }

    #[test]
    fn setting_a_key_moves_the_levels_and_keeps_their_bytes() {
        let plain = sample_ktx2();
        let stamped = set_key_value(&plain, "b.key", b"value").expect("stamp");
        let reader = ktx2::Reader::new(&stamped[..]).expect("reads");
        let levels: Vec<Vec<u8>> = reader.levels().map(|l| l.data.to_vec()).collect();
        assert_eq!(levels, [vec![7u8; 64], vec![8u8; 16], vec![9u8; 16]]);
        let entries: Vec<(&str, &[u8])> = reader.key_value_data().collect();
        assert_eq!(entries, [("b.key", &b"value"[..])]);
        let first = reader
            .levels()
            .map(|l| l.data.as_ptr() as usize)
            .min()
            .expect("a level");
        assert!((first - stamped.as_ptr() as usize).is_multiple_of(LEVEL_ALIGN));
    }

    #[test]
    fn setting_a_key_twice_replaces_it_and_keeps_the_others_sorted() {
        let once = set_key_value(&sample_ktx2(), "b.key", b"one").expect("stamp");
        let twice = set_key_value(&once, "a.key", b"other").expect("second key");
        let thrice = set_key_value(&twice, "b.key", b"longer value").expect("replace");
        let reader = ktx2::Reader::new(&thrice[..]).expect("reads");
        let entries: Vec<(&str, &[u8])> = reader.key_value_data().collect();
        assert_eq!(
            entries,
            [("a.key", &b"other"[..]), ("b.key", &b"longer value"[..])]
        );
        assert_eq!(reader.levels().next().expect("level").data, [7u8; 64]);
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(set_key_value(b"not a ktx2 file", "k", b"v").is_err());
    }

    #[test]
    fn a_cooked_texture_carries_its_metadata() {
        for platform in ["linux", "android"] {
            let cooked = cook_texture_rgba8(
                64,
                64,
                &solid(64, 40),
                TextureCookUsage::ColorSrgb,
                platform,
                TextureCookQuality::High,
            )
            .expect("cook");
            let meta = cooked_mip_stream(&cooked).expect("metadata");
            assert_eq!(meta, MipStreamMeta::new(MIN_DETAIL_PX), "{platform}");
            cooked.validate().expect("the cooked file still validates");
        }
    }
}
