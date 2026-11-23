// SPDX-License-Identifier: MPL-2.0
//! Transport v2 framing pieces (spec 120 § 4.1, wire 6): variable-length integers, truncated
//! packet numbers, acknowledgment frames with ranges, ack delay, the largest packet's arrival
//! stamp (wire 6, ADR-10050) and optional receive timestamps, and the receiver's set of received
//! packet numbers.
//!
//! Public sources, original code (see `docs/provenance.md`):
//!
//! - Variable-length integers: RFC 9000 § 16 (two-bit length prefix, 1, 2, 4 or 8 bytes,
//!   big-endian, values below 2^62).
//! - Packet numbers: RFC 9000 § 12.3, § 17.1 and Appendix A.2/A.3. Every packet gets the next
//!   62-bit number, never reused; it is sent in 1, 2 or 4 bytes, enough for twice the distance to
//!   the largest number the peer acknowledged, and decoded against the largest number received.
//! - Acknowledgment frames: RFC 9000 § 19.3 (largest acknowledged, ack delay scaled by
//!   `2^ACK_DELAY_EXPONENT` microseconds, first range, then gap/length pairs). The arrival stamp
//!   is the one-way delay measurement of RFC 6817 § 3.1 (LEDBAT: the receiver's clock at arrival,
//!   returned to the sender, no synchronised clocks) cut to 16 bits of milliseconds. Receive timestamps
//!   follow the per-packet feedback shape of RFC 8888 (packet offset and arrival time relative to
//!   the largest acknowledged packet).
//!
//! These functions are pure and never panic on input: a malformed frame is a [`WireError`].

use std::collections::VecDeque;

use crate::wire::{WireError, WireReader, WireWriter};

/// Largest value a variable-length integer carries (RFC 9000 § 16).
pub const VARINT_MAX: u64 = (1 << 62) - 1;
/// Ack delay is sent in units of `2^ACK_DELAY_EXPONENT` microseconds (RFC 9000 § 18.2 default).
pub const ACK_DELAY_EXPONENT: u32 = 3;
/// Most ranges one acknowledgment frame carries (spec 120 § 4.1 item 2).
pub const MAX_ACK_FRAME_RANGES: usize = 32;
/// Most receive timestamps one acknowledgment frame carries (spec 120 § 4.1 item 3).
pub const MAX_ACK_TIMESTAMPS: usize = 16;
/// Bytes of an acknowledgment frame's arrival stamp ([`AckFrame::arrival_ms`], wire 6).
pub const ARRIVAL_STAMP_BYTES: usize = 2;
/// Ranges of received packet numbers the receiver remembers for duplicate detection. Older
/// numbers are treated as already received.
pub const MAX_TRACKED_RANGES: usize = 256;

/// Bytes of the variable-length encoding of `v` (RFC 9000 § 16). Values past [`VARINT_MAX`]
/// are clamped to it.
pub fn varint_len(v: u64) -> usize {
    match v {
        0..=63 => 1,
        64..=16_383 => 2,
        16_384..=1_073_741_823 => 4,
        _ => 8,
    }
}

/// Writes `v` as a variable-length integer (clamped to [`VARINT_MAX`]).
pub fn write_varint(w: &mut WireWriter, v: u64) {
    let v = v.min(VARINT_MAX);
    match varint_len(v) {
        1 => w.u8(v as u8),
        2 => w.raw(&((v as u16) | 0x4000).to_be_bytes()),
        4 => w.raw(&((v as u32) | 0x8000_0000).to_be_bytes()),
        _ => w.raw(&(v | 0xC000_0000_0000_0000).to_be_bytes()),
    }
}

/// Reads a variable-length integer.
pub fn read_varint(r: &mut WireReader<'_>) -> Result<u64, WireError> {
    let first = r.u8()?;
    let len = 1usize << (first >> 6);
    let mut v = u64::from(first & 0x3F);
    for _ in 1..len {
        v = (v << 8) | u64::from(r.u8()?);
    }
    Ok(v)
}

/// Bytes needed to send packet number `full` when the peer has acknowledged up to
/// `largest_acked`: 2 or 4. RFC 9000 Appendix A.2 asks for more than twice the distance to the
/// largest acknowledged number and allows 1 byte; Regia keeps four times the distance and at least
/// 2 bytes, because a QUIC receiver's packet protection rejects a misdecoded number while an
/// unsealed Regia link has nothing that would: a packet that arrives after more than half a
/// window of newer ones must not decode as another packet and acknowledge it (ADR-8184).
pub fn packet_number_len(full: u64, largest_acked: Option<u64>) -> usize {
    let unacked = match largest_acked {
        Some(acked) => full.saturating_sub(acked),
        None => full.saturating_add(1),
    };
    // 4 * unacked must be below 2^(8 * len).
    if unacked < 1 << 14 { 2 } else { 4 }
}

/// Writes the low `len` bytes of `full` (1, 2 or 4), little-endian.
pub fn write_packet_number(w: &mut WireWriter, full: u64, len: usize) {
    match len {
        1 => w.u8(full as u8),
        2 => w.u16(full as u16),
        _ => w.u32(full as u32),
    }
}

/// Recovers a full packet number from its `len`-byte truncation, given the largest packet number
/// received so far (RFC 9000 Appendix A.3).
pub fn decode_packet_number(largest_received: Option<u64>, truncated: u64, len: usize) -> u64 {
    let expected = largest_received.map_or(0, |l| l.saturating_add(1));
    let bits = 8 * len as u32;
    let win = 1u64 << bits;
    let half = win / 2;
    let mask = win - 1;
    let candidate = (expected & !mask) | (truncated & mask);
    if candidate.saturating_add(half) <= expected && candidate < (1u64 << 62).saturating_sub(win) {
        candidate + win
    } else if candidate > expected.saturating_add(half) && candidate >= win {
        candidate - win
    } else {
        candidate
    }
}

/// One receive timestamp carried in an acknowledgment frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckTimestamp {
    /// The packet number.
    pub packet: u64,
    /// When it arrived, in microseconds after the arrival of the frame's largest acknowledged
    /// packet (negative: before it).
    pub after_largest_us: i64,
}

/// One acknowledgment frame (RFC 9000 § 19.3).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AckFrame {
    /// The largest packet number acknowledged.
    pub largest: u64,
    /// Time the receiver held the largest packet before this acknowledgment, microseconds.
    pub delay_us: u64,
    /// When the largest packet arrived: the low 16 bits of the receiver's clock, milliseconds
    /// (wire 6, ADR-10050). The sender reads its packets' one-way delay from it
    /// ([`crate::cc::OneWayDelay`]); the clocks need no common origin. Meaningless (0) in a frame
    /// of older ranges.
    pub arrival_ms: u16,
    /// Acknowledged ranges, inclusive `(smallest, largest)`, newest first, disjoint and
    /// separated by at least one missing number. The first range ends at [`Self::largest`].
    pub ranges: Vec<(u64, u64)>,
    /// Receive timestamps (empty unless the peer asked for them).
    pub timestamps: Vec<AckTimestamp>,
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

impl AckFrame {
    /// True when `packet` is inside one of the ranges.
    pub fn contains(&self, packet: u64) -> bool {
        self.ranges.iter().any(|(lo, hi)| (*lo..=*hi).contains(&packet))
    }

    /// Writes the frame. When `with_timestamps` is set a timestamp section follows (its count
    /// may be zero). Ranges past [`MAX_ACK_FRAME_RANGES`] and timestamps past
    /// [`MAX_ACK_TIMESTAMPS`] are dropped, oldest last; ranges are also dropped from the old end
    /// until the frame fits `limit` bytes (the first range always stays). Returns the bytes written.
    pub fn encode(&self, w: &mut WireWriter, with_timestamps: bool, limit: usize) -> usize {
        self.encode_counted(w, with_timestamps, limit).0
    }

    /// [`Self::encode`], also returning how many ranges (from the newest) were written.
    pub fn encode_counted(&self, w: &mut WireWriter, with_timestamps: bool, limit: usize) -> (usize, usize) {
        let start = w.len();
        let Some(first) = self.ranges.first().copied() else {
            return (0, 0);
        };
        let mut ranges = self.ranges.len().min(MAX_ACK_FRAME_RANGES);
        let mut stamps = if with_timestamps {
            self.timestamps.len().min(MAX_ACK_TIMESTAMPS)
        } else {
            0
        };
        while self.encoded_len(ranges, stamps, with_timestamps) > limit {
            if stamps > 0 {
                stamps -= 1;
            } else if ranges > 1 {
                ranges -= 1;
            } else {
                break;
            }
        }
        write_varint(w, self.largest);
        write_varint(w, self.delay_us >> ACK_DELAY_EXPONENT);
        w.u16(self.arrival_ms);
        write_varint(w, (ranges - 1) as u64);
        // Saturating: a hand-built frame with unordered ranges encodes something, never panics.
        write_varint(w, first.1.saturating_sub(first.0));
        let mut smallest = first.0;
        for (lo, hi) in self.ranges.iter().take(ranges).skip(1) {
            write_varint(w, smallest.saturating_sub(*hi).saturating_sub(2));
            write_varint(w, hi.saturating_sub(*lo));
            smallest = *lo;
        }
        if with_timestamps {
            w.u8(stamps as u8);
            for ts in self.timestamps.iter().take(stamps) {
                write_varint(w, self.largest.saturating_sub(ts.packet));
                write_varint(w, zigzag(ts.after_largest_us));
            }
        }
        (w.len() - start, ranges)
    }

    /// Bytes [`Self::encode`] writes with the first `ranges` ranges and `stamps` timestamps.
    fn encoded_len(&self, ranges: usize, stamps: usize, with_timestamps: bool) -> usize {
        let Some(first) = self.ranges.first() else {
            return 0;
        };
        let mut len = varint_len(self.largest)
            + varint_len(self.delay_us >> ACK_DELAY_EXPONENT)
            + ARRIVAL_STAMP_BYTES
            + varint_len(ranges.saturating_sub(1) as u64)
            + varint_len(first.1.saturating_sub(first.0));
        let mut smallest = first.0;
        for (lo, hi) in self.ranges.iter().take(ranges).skip(1) {
            len += varint_len(smallest.saturating_sub(*hi).saturating_sub(2))
                + varint_len(hi.saturating_sub(*lo));
            smallest = *lo;
        }
        if with_timestamps {
            len += 1;
            for ts in self.timestamps.iter().take(stamps) {
                len += varint_len(self.largest.saturating_sub(ts.packet))
                    + varint_len(zigzag(ts.after_largest_us));
            }
        }
        len
    }

    /// The smallest encoding: the largest acknowledged, its delay, its arrival stamp and the
    /// first range.
    pub fn min_encoded_len(&self) -> usize {
        self.encoded_len(1, 0, false)
    }

    /// Reads a frame. Ranges that would run below zero, more than [`MAX_ACK_FRAME_RANGES`]
    /// ranges or more than [`MAX_ACK_TIMESTAMPS`] timestamps are errors.
    pub fn decode(r: &mut WireReader<'_>, with_timestamps: bool) -> Result<Self, WireError> {
        let largest = read_varint(r)?;
        let delay_us = read_varint(r)?.saturating_mul(1 << ACK_DELAY_EXPONENT);
        let arrival_ms = r.u16()?;
        let extra = read_varint(r)?;
        if extra >= MAX_ACK_FRAME_RANGES as u64 {
            return Err(WireError::Invalid("too many ack ranges"));
        }
        let first_len = read_varint(r)?;
        let mut lo = largest
            .checked_sub(first_len)
            .ok_or(WireError::Invalid("ack range below zero"))?;
        let mut ranges = Vec::with_capacity(extra as usize + 1);
        ranges.push((lo, largest));
        for _ in 0..extra {
            let gap = read_varint(r)?;
            let len = read_varint(r)?;
            let hi = lo
                .checked_sub(gap)
                .and_then(|v| v.checked_sub(2))
                .ok_or(WireError::Invalid("ack gap below zero"))?;
            lo = hi
                .checked_sub(len)
                .ok_or(WireError::Invalid("ack range below zero"))?;
            ranges.push((lo, hi));
        }
        let mut timestamps = Vec::new();
        if with_timestamps {
            let count = usize::from(r.u8()?);
            if count > MAX_ACK_TIMESTAMPS {
                return Err(WireError::Invalid("too many ack timestamps"));
            }
            for _ in 0..count {
                let back = read_varint(r)?;
                let packet = largest
                    .checked_sub(back)
                    .ok_or(WireError::Invalid("ack timestamp below zero"))?;
                timestamps.push(AckTimestamp {
                    packet,
                    after_largest_us: unzigzag(read_varint(r)?),
                });
            }
        }
        Ok(Self {
            largest,
            delay_us,
            arrival_ms,
            ranges,
            timestamps,
        })
    }
}

/// Whether a packet number was new to [`RecvRanges::insert`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inserted {
    /// Not seen before.
    New,
    /// Already received, or older than everything remembered.
    Duplicate,
}

/// The packet numbers a receiver has seen, as disjoint inclusive ranges, newest first, bounded to
/// [`MAX_TRACKED_RANGES`]. Numbers below the oldest remembered range count as duplicates.
#[derive(Debug, Clone, Default)]
pub struct RecvRanges {
    ranges: VecDeque<(u64, u64)>,
    /// Everything below this is treated as received.
    floor: u64,
}

impl RecvRanges {
    /// Records `packet`.
    pub fn insert(&mut self, packet: u64) -> Inserted {
        if packet < self.floor {
            return Inserted::Duplicate;
        }
        // Newest first: find the first range whose smallest is at or below `packet`.
        let mut i = 0;
        while i < self.ranges.len() && self.ranges[i].0 > packet {
            i += 1;
        }
        if let Some((lo, hi)) = self.ranges.get(i).copied()
            && (lo..=hi).contains(&packet)
        {
            return Inserted::Duplicate;
        }
        // `packet` lies above range i (if any) and below range i - 1 (if any).
        let joins_below = self.ranges.get(i).is_some_and(|(_, hi)| *hi + 1 == packet);
        let joins_above = i > 0 && self.ranges[i - 1].0 == packet + 1;
        match (joins_above, joins_below) {
            (true, true) => {
                let lo = self.ranges[i].0;
                self.ranges[i - 1].0 = lo;
                self.ranges.remove(i);
            }
            (true, false) => self.ranges[i - 1].0 = packet,
            (false, true) => self.ranges[i].1 = packet,
            (false, false) => {
                self.ranges.insert(i, (packet, packet));
                if self.ranges.len() > MAX_TRACKED_RANGES
                    && let Some((_, hi)) = self.ranges.pop_back()
                {
                    self.floor = self.floor.max(hi + 1);
                }
            }
        }
        Inserted::New
    }

    /// The largest number received.
    pub fn largest(&self) -> Option<u64> {
        self.ranges.front().map(|(_, hi)| *hi)
    }

    /// True when `packet` was received (or is older than everything remembered).
    pub fn contains(&self, packet: u64) -> bool {
        packet < self.floor || self.ranges.iter().any(|(lo, hi)| (*lo..=*hi).contains(&packet))
    }

    /// The newest `max` ranges, newest first.
    pub fn newest(&self, max: usize) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.ranges.iter().copied().take(max)
    }

    /// The newest `max` ranges at or below `top`, newest first, the first one clipped to `top`.
    pub fn at_or_below(&self, top: u64, max: usize) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.ranges
            .iter()
            .filter(move |(lo, _)| *lo <= top)
            .map(move |(lo, hi)| (*lo, (*hi).min(top)))
            .take(max)
    }

    /// The largest number remembered below `bound`.
    pub fn largest_below(&self, bound: u64) -> Option<u64> {
        self.ranges
            .iter()
            .find(|(lo, _)| *lo < bound)
            .map(|(_, hi)| (*hi).min(bound.saturating_sub(1)))
    }

    /// Forgets every number in `lo..=hi` (the floor is unchanged).
    pub fn remove(&mut self, lo: u64, hi: u64) {
        if lo > hi {
            return;
        }
        if hi == u64::MAX {
            // Everything from `lo` up (the common case, after each frame): no allocation.
            while self.ranges.front().is_some_and(|(a, _)| *a >= lo) {
                self.ranges.pop_front();
            }
            if let Some(front) = self.ranges.front_mut()
                && front.1 >= lo
            {
                front.1 = lo - 1;
            }
            return;
        }
        // In place, newest first: ranges wholly above `hi` are skipped, the ones that overlap are
        // cut (a range that spans the whole hole splits in two), and the walk stops at the first
        // range wholly below `lo`.
        let mut i = 0;
        while let Some((a, b)) = self.ranges.get(i).copied() {
            if a > hi {
                i += 1;
                continue;
            }
            if b < lo {
                break;
            }
            match (b > hi, a < lo) {
                (false, false) => {
                    self.ranges.remove(i);
                }
                (true, false) => {
                    self.ranges[i].0 = hi + 1;
                    i += 1;
                }
                (false, true) => {
                    self.ranges[i].1 = lo - 1;
                    i += 1;
                }
                (true, true) => {
                    self.ranges[i] = (hi + 1, b);
                    self.ranges.insert(i + 1, (a, lo - 1));
                    if self.ranges.len() > MAX_TRACKED_RANGES
                        && let Some((_, top)) = self.ranges.pop_back()
                    {
                        self.floor = self.floor.max(top + 1);
                    }
                    break;
                }
            }
        }
    }

    /// Ranges remembered.
    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    /// True before the first packet.
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip_at_every_length_boundary() {
        for v in [
            0,
            63,
            64,
            16_383,
            16_384,
            1_073_741_823,
            1_073_741_824,
            VARINT_MAX,
        ] {
            let mut w = WireWriter::new();
            write_varint(&mut w, v);
            let bytes = w.finish();
            assert_eq!(bytes.len(), varint_len(v), "{v}");
            let mut r = WireReader::new(&bytes);
            assert_eq!(read_varint(&mut r).expect("varint"), v);
            assert!(r.is_empty());
        }
        // RFC 9000 § A.1 examples.
        let mut r = WireReader::new(&[0x9d, 0x7f, 0x3e, 0x7d]);
        assert_eq!(read_varint(&mut r).expect("varint"), 494_878_333);
        let mut r = WireReader::new(&[0x7b, 0xbd]);
        assert_eq!(read_varint(&mut r).expect("varint"), 15_293);
    }

    /// RFC 9000 Appendix A.2 and A.3 examples, plus a sweep across every truncation length.
    #[test]
    fn packet_numbers_truncate_and_decode_per_rfc_9000_appendix_a() {
        // A.3: largest 0xa82f30ea, truncated 0x9b32 (2 bytes) -> 0xa82f9b32.
        assert_eq!(decode_packet_number(Some(0xa82f_30ea), 0x9b32, 2), 0xa82f_9b32);
        // A.2: full 0xac5c02 with largest acked 0xabe8b3 is 29,519 ahead (RFC: 2 bytes; with
        // Regia's 4x margin, 4); 0xace8fe needs 3 (4 here); small distances take 2, never 1.
        assert_eq!(packet_number_len(0xac_5c02, Some(0xab_e8b3)), 4);
        assert_eq!(packet_number_len(0xac_e8fe, Some(0xab_e8b3)), 4);
        assert_eq!(packet_number_len(0xab_e8b3 + 16_383, Some(0xab_e8b3)), 2);
        assert_eq!(packet_number_len(5, None), 2);
        // 1-byte numbers still decode (the format allows them).
        assert_eq!(decode_packet_number(Some(300), 0x2d, 1), 301);
        let mut largest_received: Option<u64> = None;
        let mut largest_acked: Option<u64> = None;
        for full in (0..300_000u64).step_by(7) {
            let len = packet_number_len(full, largest_acked);
            let mut w = WireWriter::new();
            write_packet_number(&mut w, full, len);
            let bytes = w.finish();
            let truncated = bytes.iter().rev().fold(0u64, |v, b| (v << 8) | u64::from(*b));
            assert_eq!(decode_packet_number(largest_received, truncated, len), full);
            largest_received = Some(full);
            // The peer acknowledges with a lag of up to 100 packets.
            largest_acked = full.checked_sub(100).or(largest_acked);
        }
    }

    #[test]
    fn ack_frames_round_trip_ranges_delay_and_timestamps() {
        let frame = AckFrame {
            largest: 1_000,
            delay_us: 8_000,
            arrival_ms: 0xfffe,
            ranges: vec![(990, 1_000), (900, 980), (5, 5)],
            timestamps: vec![
                AckTimestamp {
                    packet: 1_000,
                    after_largest_us: 0,
                },
                AckTimestamp {
                    packet: 999,
                    after_largest_us: -3_000,
                },
            ],
        };
        for with_ts in [false, true] {
            let mut w = WireWriter::new();
            frame.encode(&mut w, with_ts, usize::MAX);
            let bytes = w.finish();
            let back = AckFrame::decode(&mut WireReader::new(&bytes), with_ts).expect("frame");
            assert_eq!(back.ranges, frame.ranges);
            assert_eq!(back.delay_us, frame.delay_us);
            assert_eq!(back.arrival_ms, frame.arrival_ms);
            assert_eq!(
                back.timestamps,
                if with_ts { frame.timestamps.clone() } else { vec![] }
            );
        }
        // A tight limit keeps the first range only.
        let mut w = WireWriter::new();
        let written = frame.encode(&mut w, false, frame.min_encoded_len());
        assert_eq!(written, frame.min_encoded_len());
        let back = AckFrame::decode(&mut WireReader::new(&w.finish()), false).expect("frame");
        assert_eq!(back.ranges, vec![(990, 1_000)]);
    }

    #[test]
    fn malformed_ack_frames_are_errors_not_panics() {
        // A first range longer than the largest acknowledged.
        // largest, ack delay, the arrival stamp, then the ranges.
        fn frame(largest: u64, rest: &[u64]) -> Vec<u8> {
            let mut w = WireWriter::new();
            write_varint(&mut w, largest);
            write_varint(&mut w, 0);
            w.u16(0);
            for v in rest {
                write_varint(&mut w, *v);
            }
            w.finish()
        }
        assert!(AckFrame::decode(&mut WireReader::new(&frame(3, &[0, 3])), false).is_ok());
        assert!(AckFrame::decode(&mut WireReader::new(&frame(3, &[0, 9])), false).is_err());
        // A gap that runs below zero.
        assert!(AckFrame::decode(&mut WireReader::new(&frame(10, &[1, 2, 7, 0])), false).is_err());
        // Too many ranges.
        assert!(AckFrame::decode(&mut WireReader::new(&frame(10_000, &[32, 0])), false).is_err());
        // Every short prefix of a valid frame.
        let frame = AckFrame {
            largest: 70_000,
            delay_us: 80,
            arrival_ms: 513,
            ranges: vec![(69_000, 70_000), (10, 20)],
            timestamps: vec![],
        };
        let mut w = WireWriter::new();
        frame.encode(&mut w, true, usize::MAX);
        let bytes = w.finish();
        for cut in 0..bytes.len() {
            assert!(AckFrame::decode(&mut WireReader::new(&bytes[..cut]), true).is_err());
        }
    }

    #[test]
    fn received_ranges_merge_detect_duplicates_and_stay_bounded() {
        let mut set = RecvRanges::default();
        for p in [5, 7, 6, 9, 3, 4] {
            assert_eq!(set.insert(p), Inserted::New, "{p}");
        }
        assert_eq!(set.insert(6), Inserted::Duplicate);
        assert_eq!(set.newest(8).collect::<Vec<_>>(), vec![(9, 9), (3, 7)]);
        assert_eq!(set.insert(8), Inserted::New);
        assert_eq!(set.newest(8).collect::<Vec<_>>(), vec![(3, 9)]);
        let mut set = RecvRanges::default();
        for p in 0..(MAX_TRACKED_RANGES as u64 + 10) {
            set.insert(p * 2);
        }
        assert_eq!(set.len(), MAX_TRACKED_RANGES);
        assert_eq!(set.insert(0), Inserted::Duplicate, "below the floor");
        assert_eq!(set.largest(), Some((MAX_TRACKED_RANGES as u64 + 9) * 2));
        let mut set = RecvRanges::default();
        for p in [1, 2, 3, 5, 6, 9, 10, 11] {
            set.insert(p);
        }
        assert_eq!(set.at_or_below(5, 8).collect::<Vec<_>>(), vec![(5, 5), (1, 3)]);
        assert_eq!(set.largest_below(5), Some(3));
        assert_eq!(set.largest_below(10), Some(9));
        set.remove(2, 10);
        assert_eq!(set.newest(8).collect::<Vec<_>>(), vec![(11, 11), (1, 1)]);
        set.insert(20);
        set.insert(21);
        set.remove(11, u64::MAX);
        assert_eq!(set.newest(8).collect::<Vec<_>>(), vec![(1, 1)]);
    }
}
