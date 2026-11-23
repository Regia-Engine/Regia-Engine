// SPDX-License-Identifier: MPL-2.0
//! Xiph helpers shared by the Vorbis and Opus decoders, in safe Rust (always built, so the parsers are
//! tested without the codec features): Xiph lacing of the Vorbis CodecPrivate (Matroska codec mapping
//! for `A_VORBIS`) and the Vorbis-to-WAVE channel reorder (Vorbis I specification § 4.3.9; Opus mapping
//! family 1 uses the same order, RFC 7845 § 5.1.1.2).

use crate::error::MediaError;

/// Splits a Xiph-laced block into its packets: one byte `count - 1`, then `count - 1` sizes as runs of
/// 255-valued bytes ended by a byte below 255, then the packets back to back (the last takes the rest).
pub fn xiph_unlace(b: &[u8]) -> Result<Vec<&[u8]>, MediaError> {
    let bad = |why: &str| MediaError::invalid(format!("Xiph lacing {why}"));
    let (&n, mut rest) = b.split_first().ok_or_else(|| bad("empty"))?;
    let mut sizes = Vec::with_capacity(usize::from(n));
    for _ in 0..n {
        let mut size = 0usize;
        loop {
            let (&v, tail) = rest.split_first().ok_or_else(|| bad("truncated in the sizes"))?;
            rest = tail;
            size += usize::from(v);
            if v < 255 {
                break;
            }
        }
        sizes.push(size);
    }
    let mut packets = Vec::with_capacity(sizes.len() + 1);
    for size in sizes {
        if size > rest.len() {
            return Err(bad("with a packet past the end"));
        }
        let (p, tail) = rest.split_at(size);
        packets.push(p);
        rest = tail;
    }
    packets.push(rest);
    Ok(packets)
}

/// Output position `i` takes Vorbis channel `ORDER[channels][i]` (3 to 8 channels).
const ORDER: [&[usize]; 9] = [
    &[],
    &[0],
    &[0, 1],
    &[0, 2, 1],
    &[0, 1, 2, 3],
    &[0, 2, 1, 3, 4],
    &[0, 2, 1, 5, 3, 4],
    &[0, 2, 1, 6, 5, 3, 4],
    &[0, 2, 1, 7, 5, 6, 3, 4],
];

/// Reorders interleaved frames in place from the Vorbis channel order to the WAVE order
/// (front left, front right, center, LFE, back, side). Counts outside 3..=8 are left as they are.
pub fn vorbis_to_wave_order(samples: &mut [f32], channels: usize) {
    let Some(order) = ORDER.get(channels).filter(|_| channels >= 3) else {
        return;
    };
    let mut frame = [0.0f32; 8];
    for chunk in samples.chunks_exact_mut(channels) {
        frame[..channels].copy_from_slice(chunk);
        for (dst, &src) in chunk.iter_mut().zip(order.iter()) {
            *dst = frame[src];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlace_three_headers() {
        let mut b = vec![2, 255, 45, 3];
        b.extend(std::iter::repeat_n(7u8, 300));
        b.extend([1, 2, 3]);
        b.extend([9, 9]);
        let p = xiph_unlace(&b).expect("laced");
        assert_eq!(p.len(), 3);
        assert_eq!((p[0].len(), p[1].len(), p[2].len()), (300, 3, 2));
        assert_eq!(p[1], &[1, 2, 3]);
    }

    #[test]
    fn unlace_rejects_bad_input() {
        assert!(xiph_unlace(&[]).is_err());
        assert!(xiph_unlace(&[2, 255]).is_err());
        assert!(xiph_unlace(&[1, 10, 1, 2]).is_err());
        assert_eq!(xiph_unlace(&[0, 5, 6]).expect("one packet"), vec![&[5u8, 6][..]]);
    }

    #[test]
    fn five_one_moves_to_wave_order() {
        // Vorbis 5.1: FL C FR RL RR LFE -> WAVE: FL FR C LFE RL RR.
        let mut s = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        vorbis_to_wave_order(&mut s, 6);
        assert_eq!(s[..6], [1.0, 3.0, 2.0, 6.0, 4.0, 5.0]);
        assert_eq!(s[6..], [1.0, 3.0, 2.0, 6.0, 4.0, 5.0]);
        let mut stereo = [1.0, 2.0];
        vorbis_to_wave_order(&mut stereo, 2);
        assert_eq!(stereo, [1.0, 2.0]);
    }
}
