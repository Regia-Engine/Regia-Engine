// SPDX-License-Identifier: MPL-2.0
//! Digest of a procedural evaluation (ADR-4593).
//!
//! A caller that already holds quantized instance records compares two bakes by this 64-bit
//! digest instead of reading the whole output. The digest is a non-cryptographic mix: records are
//! ordered by a total key, then each field is folded with the SplitMix64 output function. Input
//! order does not matter. It is not a collision-resistant hash.
//!
//! The mixer is the same four-line SplitMix64 step already used by
//! `regia_asset_pipeline::generators` (Steele, Lea and Flood, OOPSLA 2014; the constants are
//! public). The sort and the per-field fold are original.
//!
//! Quantization is the caller's. Position components are millimeters (`round(meters * 1000)` as
//! `i32`, the same grid as [`crate::seeds::q`], kept signed). Yaw is millidegrees, not wrapped.

use std::cmp::Ordering;

/// Most records [`fingerprint`] accepts.
///
/// A longer list returns [`FingerprintError::Overflow`] and does not allocate the mix buffer.
pub const FINGERPRINT_CAP: usize = 1_048_576;

/// Domain word of the mix. The bytes of `REGIAPG1`, high byte first.
const DOMAIN: u64 = 0x5245_4749_4150_4731;

/// One instance the caller has already quantized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EvalRecord {
    /// Graph node that produced the instance.
    pub node_id: u32,
    /// Index of this instance among that node's outputs.
    pub instance_index: u32,
    /// Position in millimeters, one `i32` per axis (x, y, z).
    pub position_mm: [i32; 3],
    /// Yaw in millidegrees. The caller quantized it; this module does not wrap it.
    pub yaw_mdeg: i32,
}

/// Why a digest was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FingerprintError {
    /// `records.len()` is greater than [`FINGERPRINT_CAP`].
    Overflow,
}

impl std::fmt::Display for FingerprintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Overflow => write!(
                f,
                "procedural evaluation fingerprint refuses more than {FINGERPRINT_CAP} records"
            ),
        }
    }
}

impl std::error::Error for FingerprintError {}

/// SplitMix64 output function (Steele, Lea and Flood, OOPSLA 2014).
const fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Digest of an empty record list.
///
/// This is the SplitMix64 step of the domain word, with no record folded in. Two calls on an
/// empty list both return this value. The hex text is `f7d710e043e5193e`.
pub const EMPTY_DIGEST: u64 = splitmix64(DOMAIN);

/// Sixteen lowercase hex digits of `digest`.
#[must_use]
pub fn digest_hex(digest: u64) -> String {
    format!("{digest:016x}")
}

/// Digest of `records`.
///
/// Records are ordered by `(node_id, instance_index, position_mm, yaw_mdeg)` so the digest is a
/// function of the multiset: the same records in any input order, including two records that share
/// a node and an instance index, produce the same value. Each integer is then folded through
/// SplitMix64. A change of one millimeter, or of one millidegree, changes the digest when every
/// other field stays the same.
///
/// Returns [`FingerprintError::Overflow`] before allocating the index buffer when `records` is
/// longer than [`FINGERPRINT_CAP`]. An empty list returns [`EMPTY_DIGEST`] and allocates nothing.
pub fn fingerprint(records: &[EvalRecord]) -> Result<u64, FingerprintError> {
    if records.len() > FINGERPRINT_CAP {
        return Err(FingerprintError::Overflow);
    }
    if records.is_empty() {
        return Ok(EMPTY_DIGEST);
    }
    Ok(mix_sorted(records))
}

/// Sort index buffer plus the fold. The only place that allocates.
fn mix_sorted(records: &[EvalRecord]) -> u64 {
    let _span = tracing::info_span!("procgen.fingerprint", records = records.len()).entered();
    #[cfg(test)]
    MIX_BUFFERS.with(|n| n.set(n.get() + 1));
    let mut order = Vec::with_capacity(records.len());
    order.extend(0..records.len());
    order.sort_unstable_by(|&a, &b| cmp_records(&records[a], &records[b]));
    let mut state = DOMAIN;
    for index in order {
        state = mix_record(state, &records[index]);
    }
    splitmix64(state)
}

fn cmp_records(a: &EvalRecord, b: &EvalRecord) -> Ordering {
    (a.node_id, a.instance_index, a.position_mm, a.yaw_mdeg).cmp(&(
        b.node_id,
        b.instance_index,
        b.position_mm,
        b.yaw_mdeg,
    ))
}

fn mix_record(mut state: u64, record: &EvalRecord) -> u64 {
    state = mix_word(state, u64::from(record.node_id));
    state = mix_word(state, u64::from(record.instance_index));
    state = mix_word(state, i32_bits(record.position_mm[0]));
    state = mix_word(state, i32_bits(record.position_mm[1]));
    state = mix_word(state, i32_bits(record.position_mm[2]));
    mix_word(state, i32_bits(record.yaw_mdeg))
}

fn mix_word(state: u64, word: u64) -> u64 {
    splitmix64(state ^ word)
}

/// Two's-complement bits of `value` in the low 32 bits. The high half stays zero, so a negative
/// millimeter is distinct from the same magnitude with the sign bit dropped.
fn i32_bits(value: i32) -> u64 {
    u64::from(value.cast_unsigned())
}

#[cfg(test)]
thread_local! {
    static MIX_BUFFERS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(node: u32, instance: u32, position_mm: [i32; 3], yaw_mdeg: i32) -> EvalRecord {
        EvalRecord {
            node_id: node,
            instance_index: instance,
            position_mm,
            yaw_mdeg,
        }
    }

    fn digest(records: &[EvalRecord]) -> u64 {
        fingerprint(records).expect("under the cap")
    }

    #[test]
    fn empty_digest_is_stable() {
        let before = MIX_BUFFERS.with(std::cell::Cell::get);
        assert_eq!(digest(&[]), EMPTY_DIGEST);
        assert_eq!(fingerprint(&[]).expect("empty"), fingerprint(&[]).expect("again"));
        assert_eq!(digest_hex(EMPTY_DIGEST), "f7d710e043e5193e");
        assert_eq!(
            MIX_BUFFERS.with(std::cell::Cell::get),
            before,
            "an empty list does not mix"
        );
    }

    #[test]
    fn same_records_in_two_orders_share_a_digest() {
        let a = rec(2, 0, [10, 20, 30], 90_000);
        let b = rec(1, 5, [-4, 0, 8], 0);
        let c = rec(1, 5, [-4, 1, 8], 0);
        let d = rec(2, 0, [10, 20, 30], 90_001);
        let forward = digest(&[a, b, c, d]);
        assert_eq!(forward, digest(&[d, c, b, a]));
        assert_eq!(forward, digest(&[b, d, a, c]));
        assert_eq!(forward, digest(&[c, a, d, b]));
        assert_ne!(forward, EMPTY_DIGEST);
    }

    #[test]
    fn one_millimeter_changes_the_digest() {
        let base = [rec(7, 1, [1_000, 2_000, 3_000], 45_000)];
        let base_digest = digest(&base);
        for (axis, yaw) in [([1, 0, 0], 0), ([0, 1, 0], 0), ([0, 0, 1], 0), ([0, 0, 0], 1)] {
            let changed = [rec(
                7,
                1,
                [
                    base[0].position_mm[0] + axis[0],
                    base[0].position_mm[1] + axis[1],
                    base[0].position_mm[2] + axis[2],
                ],
                base[0].yaw_mdeg + yaw,
            )];
            assert_ne!(digest(&changed), base_digest, "delta {axis:?} yaw {yaw}");
        }
        assert_ne!(
            digest(&[rec(7, 1, [-1, 2_000, 3_000], 45_000)]),
            digest(&[rec(7, 1, [1, 2_000, 3_000], 45_000)]),
            "a negative millimeter is its own value"
        );
    }

    #[test]
    fn one_past_the_cap_errors_before_the_mix() {
        let before = MIX_BUFFERS.with(std::cell::Cell::get);
        let records = vec![
            EvalRecord {
                node_id: 0,
                instance_index: 0,
                position_mm: [0, 0, 0],
                yaw_mdeg: 0,
            };
            FINGERPRINT_CAP + 1
        ];
        let error = fingerprint(&records).expect_err("over the cap");
        assert_eq!(error, FingerprintError::Overflow);
        assert_eq!(
            MIX_BUFFERS.with(std::cell::Cell::get),
            before,
            "overflow returns before the mix buffer is allocated"
        );
    }

    #[test]
    fn the_cap_itself_mixes() {
        let before = MIX_BUFFERS.with(std::cell::Cell::get);
        let records = vec![
            EvalRecord {
                node_id: 3,
                instance_index: 1,
                position_mm: [4, 5, 6],
                yaw_mdeg: 7,
            };
            FINGERPRINT_CAP
        ];
        let digest = fingerprint(&records).expect("the cap is accepted");
        assert_ne!(digest, EMPTY_DIGEST);
        assert_eq!(MIX_BUFFERS.with(std::cell::Cell::get), before + 1);
    }
}
