// SPDX-License-Identifier: MPL-2.0
//! Alpha-coverage-preserving mips (`docs/specs/12-assets.md` § 7.1, ADR-3090).
//!
//! An alpha-tested texture (foliage cards, fences, hair) draws a texel where its alpha reaches
//! the material's cutoff. A box-filtered mip averages opaque leaves with the transparent gaps
//! between them, so the averages fall under the cutoff and each smaller mip passes fewer texels:
//! a far canopy thins out and finally disappears.
//!
//! With `alpha_cutoff` set in a texture's `.meta`, the cook keeps the fraction of texels that pass
//! the cutoff the same in every mip as in mip 0 (Castano, "Computing Alpha Mipmaps", 2010): each
//! level is scaled so that the texels at or above a reference value `r` pass and the others do
//! not, with `r` chosen from the level's alpha histogram so its passing fraction is the closest
//! to mip 0's. The mips are box-filtered from the unscaled chain, then each level is scaled once,
//! so the error does not compound down the chain. Scaled values stay [`GUARD_BAND`] steps clear of the
//! cutoff on their side, so a block encoder's alpha error does not flip them. The scaling runs on the RGBA8 chain before any
//! encoder sees it, so BC7, BC3, ASTC and ETC2 / EAC all compress the same corrected levels.
//!
//! Off by default: without the setting the chain is untouched, byte for byte.

use regia_asset::TextureCookUsage;

/// The smallest cutoff byte for `cutoff` (0..=1]: a texel passes when `byte / 255 >= cutoff`,
/// the comparison an alpha-test shader makes on the sampled UNORM value. `None` for a cutoff that
/// passes every texel (0 or less) or is not finite.
#[must_use]
pub fn cutoff_byte(cutoff: f32) -> Option<u8> {
    if !cutoff.is_finite() || cutoff <= 0.0 {
        return None;
    }
    (1..=255u8)
        .find(|&b| f32::from(b) / 255.0 >= cutoff)
        .or(Some(255))
}

/// The channel an alpha test reads for `usage`: alpha (3), or red (0) for a one-channel mask.
/// `None` for a normal map, which carries no coverage.
#[must_use]
pub fn coverage_channel(usage: TextureCookUsage) -> Option<usize> {
    match usage {
        TextureCookUsage::NormalMap => None,
        TextureCookUsage::Mask => Some(0),
        TextureCookUsage::ColorSrgb | TextureCookUsage::ColorLinear | TextureCookUsage::Ui => Some(3),
    }
}

/// The fraction of the RGBA8 texels of `rgba` whose `channel` passes `cutoff` (0 for an empty
/// image; 1 for a cutoff that passes everything).
#[must_use]
pub fn coverage(rgba: &[u8], channel: usize, cutoff: f32) -> f32 {
    let texels = rgba.as_chunks::<4>().0;
    if texels.is_empty() {
        return 0.0;
    }
    let Some(t) = cutoff_byte(cutoff) else {
        return 1.0;
    };
    let pass = texels.iter().filter(|px| px[channel & 3] >= t).count();
    pass as f32 / texels.len() as f32
}

/// [`coverage`] of every level of a mip chain (`(width, height, RGBA8)`, level 0 first).
#[must_use]
pub fn chain_coverage(mips: &[(u32, u32, Vec<u8>)], channel: usize, cutoff: f32) -> Vec<f32> {
    mips.iter()
        .map(|(_, _, px)| coverage(px, channel, cutoff))
        .collect()
}

/// Scales `channel` of every level after the first so its coverage at `cutoff` matches level 0's
/// as closely as the level's texel count allows (see the module docs). A cutoff that passes every
/// texel, or a level 0 that passes none or all of them, leaves the chain unchanged.
pub fn preserve_coverage(mips: &mut [(u32, u32, Vec<u8>)], channel: usize, cutoff: f32) {
    let Some(t) = cutoff_byte(cutoff) else {
        return;
    };
    let Some(((_, _, base), rest)) = mips.split_first_mut() else {
        return;
    };
    let _span = tracing::info_span!("alpha_coverage_mips", levels = rest.len() + 1).entered();
    let channel = channel & 3;
    let base_texels = base.as_chunks::<4>().0;
    if base_texels.is_empty() {
        return;
    }
    let target = base_texels.iter().filter(|px| px[channel] >= t).count() as f64 / base_texels.len() as f64;
    if target <= 0.0 || target >= 1.0 {
        // Nothing passes (or everything does) in mip 0: the box filter keeps that true below.
        return;
    }
    for (_, _, level) in rest {
        scale_level(level, channel, t, target);
    }
}

/// Scales one level so the fraction of texels at or above the cutoff byte `t` is the closest it
/// can be to `target`.
fn scale_level(level: &mut [u8], channel: usize, t: u8, target: f64) {
    let texels = level.as_chunks_mut::<4>().0;
    let n = texels.len();
    if n == 0 {
        return;
    }
    let mut histogram = [0usize; 256];
    for px in texels.iter() {
        histogram[usize::from(px[channel])] += 1;
    }
    // at_least[r] = texels with a value >= r.
    let mut at_least = [0usize; 257];
    for r in (0..256).rev() {
        at_least[r] = at_least[r + 1] + histogram[r];
    }
    // The reference r (1..=255) whose passing fraction is the closest to the target; on a tie the
    // one nearest the cutoff byte (the smallest change). r = 0 would pass everything, and the
    // coverage of r = 256 (nothing) is never closer than r = 255's tie-break allows.
    let error = |r: usize| (at_least[r] as f64 / n as f64 - target).abs();
    let mut best = usize::from(t);
    for r in 1..=255usize {
        let (e, eb) = (error(r), error(best));
        if e < eb - 1e-12
            || ((e - eb).abs() <= 1e-12 && r.abs_diff(usize::from(t)) < best.abs_diff(usize::from(t)))
        {
            best = r;
        }
    }
    // Castano's scale t / r, kept exact at the boundary: a value at or above r lands at or above
    // t, a value below r below t (the rounding of a scale under 1/2 would otherwise lift r - 1).
    // Values are also kept GUARD_BAND away from the cutoff on their side, so a block encoder's
    // small alpha error does not flip the texels the scaling put right at it.
    let scale = f64::from(t) / best as f64;
    let (t, best) = (u32::from(t), best as u32);
    let pass_floor = (t + GUARD_BAND).min(255);
    let fail_ceiling = t.saturating_sub(1 + GUARD_BAND);
    for px in texels.iter_mut() {
        let a = u32::from(px[channel]);
        let scaled = ((f64::from(a) * scale).round() as u32).min(255);
        let value = if a >= best {
            scaled.max(pass_floor)
        } else {
            scaled.min(fail_ceiling)
        };
        px[channel] = value as u8;
    }
}

/// How far (in 8-bit steps) a scaled level keeps its values from the cutoff on their side of it:
/// larger than the alpha error of BC7, BC3, ASTC 4×4 and ETC2 / EAC on smooth alpha, small enough
/// that the alpha-blended edge of a card barely moves.
pub const GUARD_BAND: u32 = 8;

/// The RGBA8 mip chain the cook compresses for a texture of `usage` with `alpha_cutoff`: box
/// filtered from `rgba`, then coverage-preserved when the cutoff is set and the usage has a
/// coverage channel. The same chain every encoder (BC7, BC3, ASTC, ETC2 / EAC) receives, and the
/// uncompressed reference the decode tests compare against.
#[must_use]
pub fn coverage_mip_chain(
    width: u32,
    height: u32,
    rgba: &[u8],
    usage: TextureCookUsage,
    alpha_cutoff: Option<f32>,
) -> Vec<(u32, u32, Vec<u8>)> {
    let mut mips = crate::texture_compress::generate_mips_rgba8(width, height, rgba);
    if let (Some(cutoff), Some(channel)) = (alpha_cutoff, coverage_channel(usage)) {
        preserve_coverage(&mut mips, channel, cutoff);
    }
    mips
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cutoff_byte_is_the_first_value_a_shader_passes() {
        assert_eq!(cutoff_byte(0.5), Some(128));
        assert_eq!(cutoff_byte(1.0), Some(255));
        assert_eq!(cutoff_byte(1.0 / 255.0), Some(1));
        assert_eq!(cutoff_byte(0.0), None);
        assert_eq!(cutoff_byte(f32::NAN), None);
        assert_eq!(cutoff_byte(2.0), Some(255));
    }

    #[test]
    fn a_level_is_scaled_to_the_target_fraction_exactly_when_its_values_allow() {
        // 8 texels with alphas 10, 20, ... 80; target 3/8 at cutoff byte 128.
        let mut level: Vec<u8> = (1..=8u8).flat_map(|i| [0, 0, 0, i * 10]).collect();
        scale_level(&mut level, 3, 128, 3.0 / 8.0);
        let pass = level.as_chunks::<4>().0.iter().filter(|p| p[3] >= 128).count();
        assert_eq!(pass, 3);
        // Monotone: the order of the alphas is kept.
        let alphas: Vec<u8> = level.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
        assert!(alphas.windows(2).all(|w| w[0] <= w[1]), "{alphas:?}");
        // The colour channels are untouched.
        assert!(level.as_chunks::<4>().0.iter().all(|p| p[..3] == [0, 0, 0]));
    }

    #[test]
    fn scaling_down_keeps_the_boundary_exact() {
        // Values 200..=255 with a low target: r is high, the scale under 1/2 for small t.
        let mut level: Vec<u8> = (200..=255u8).flat_map(|a| [0, 0, 0, a]).collect();
        let n = level.len() / 4;
        scale_level(&mut level, 3, 50, 10.0 / n as f64);
        let pass = level.as_chunks::<4>().0.iter().filter(|p| p[3] >= 50).count();
        assert_eq!(pass, 10);
    }

    #[test]
    fn mip_zero_with_nothing_or_everything_passing_is_left_alone() {
        let px = [9u8, 9, 9, 40].repeat(64);
        let mut mips = crate::texture_compress::generate_mips_rgba8(8, 8, &px);
        let before = mips.clone();
        preserve_coverage(&mut mips, 3, 0.5);
        assert_eq!(mips, before);
        let px = [9u8, 9, 9, 255].repeat(64);
        let mut mips = crate::texture_compress::generate_mips_rgba8(8, 8, &px);
        let before = mips.clone();
        preserve_coverage(&mut mips, 3, 0.5);
        assert_eq!(mips, before);
    }

    #[test]
    fn a_normal_map_has_no_coverage_channel_and_a_mask_reads_red() {
        assert_eq!(coverage_channel(TextureCookUsage::NormalMap), None);
        assert_eq!(coverage_channel(TextureCookUsage::Mask), Some(0));
        assert_eq!(coverage_channel(TextureCookUsage::ColorSrgb), Some(3));
        let px = [1u8, 2, 3, 4].repeat(16 * 16);
        assert_eq!(
            coverage_mip_chain(16, 16, &px, TextureCookUsage::NormalMap, Some(0.5)),
            crate::texture_compress::generate_mips_rgba8(16, 16, &px)
        );
    }
}
