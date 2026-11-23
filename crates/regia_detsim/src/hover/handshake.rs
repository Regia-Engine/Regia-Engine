// SPDX-License-Identifier: MPL-2.0
//! The lockstep handshake bytes of one hover hull (ADR-8512, amended by ADR-8632).
//!
//! A hull steps with three inputs a peer can set: the [`HoverParams`] numbers, the [`HoverLimits`]
//! numbers and the [`FlightMode`]. [`hull_handshake_bytes`] writes all three as raw bits, so two
//! peers whose bytes match step every tick of a hull the same, and two peers whose bytes differ are
//! refused before the first tick ([`hull_handshake_mismatch`] names the first field that differs).
//!
//! Layout ([`HULL_HANDSHAKE_BYTES`] = 273 bytes):
//!
//! | Bytes | Content |
//! |---|---|
//! | 0..180 | [`HoverParams::to_le_bytes`]: one little-endian `f32::to_bits` word per [`HoverParams::FIELD_RANGES`] field, in that order |
//! | 180 | [`FlightMode`]: 0 for `air_time`, 1 for `clamped` |
//! | 181..273 | one little-endian `f32::to_bits` word per [`HoverLimits::FIELD_RANGES`] field, in that order |
//!
//! Raw bits keep `-0.0` apart from `0.0`. The layout is fixed; a change to it is a sim-version
//! change (ADR-8019). A format built on a hull (a bike file) appends its own bytes after these 273
//! and compares the whole; [`hull_handshake_mismatch`] reports a difference at byte 273 or later
//! as [`HullHandshakeMismatch::Extension`].
//!
//! Integer and bit copies only: no float arithmetic, so the bytes are the same on every target.

use core::fmt;

use super::{FlightMode, HoverLimits, HoverParams};

/// Offset of the flight-mode byte in [`hull_handshake_bytes`].
const FLIGHT_AT: usize = HoverParams::BYTES;
/// Offset of the first [`HoverLimits`] word in [`hull_handshake_bytes`].
const LIMITS_AT: usize = FLIGHT_AT + 1;

/// Length of [`hull_handshake_bytes`]: 180 params bytes, one flight byte, 92 limits bytes.
pub const HULL_HANDSHAKE_BYTES: usize = LIMITS_AT + HoverLimits::FIELD_RANGES.len() * 4;

/// The flight mode's handshake byte: 0 for [`FlightMode::AirTime`], 1 for [`FlightMode::Clamped`].
const fn flight_byte(flight: FlightMode) -> u8 {
    match flight {
        FlightMode::AirTime => 0,
        FlightMode::Clamped => 1,
    }
}

/// Everything a hover hull's step reads that a peer can set, as the bytes a lockstep settings
/// handshake compares: `params`, then the `flight` byte, then every `limits` field (layout in the
/// module docs).
///
/// Pass the values the match will step, after the host resolves a file's omitted parts (an
/// omitted `limits` block is the host's limits, an omitted flight mode the host's mode). Compare
/// the bytes, never `==` on the numbers: IEEE `==` treats `-0.0` and `0.0` as equal and these
/// bytes do not. A format that adds its own step inputs extends these bytes (append after them)
/// instead of writing a second hull digest.
#[must_use]
pub fn hull_handshake_bytes(
    params: &HoverParams,
    limits: &HoverLimits,
    flight: FlightMode,
) -> [u8; HULL_HANDSHAKE_BYTES] {
    let mut out = [0_u8; HULL_HANDSHAKE_BYTES];
    out[..FLIGHT_AT].copy_from_slice(&params.to_le_bytes());
    out[FLIGHT_AT] = flight_byte(flight);
    for (i, field) in HoverLimits::FIELD_RANGES.iter().enumerate() {
        let at = LIMITS_AT + i * 4;
        out[at..at + 4].copy_from_slice(&(field.get)(limits).to_bits().to_le_bytes());
    }
    out
}

/// The first part of two handshakes that differs, as [`hull_handshake_mismatch`] reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HullHandshakeMismatch {
    /// A [`HoverParams`] field (its [`HoverParams::FIELD_RANGES`] name).
    Params(&'static str),
    /// The flight-mode byte.
    Flight,
    /// A [`HoverLimits`] field (its [`HoverLimits::FIELD_RANGES`] name).
    Limits(&'static str),
    /// A byte past [`HULL_HANDSHAKE_BYTES`], in bytes a format appended after the hull's.
    Extension {
        /// Offset of the first differing byte.
        offset: usize,
    },
    /// The two handshakes have different lengths and agree on every byte the shorter one has.
    Length {
        /// Length of the local bytes.
        local: usize,
        /// Length of the peer's bytes.
        peer: usize,
    },
}

impl HullHandshakeMismatch {
    /// The error code a host reports when it refuses the peer: `handshake_mismatch`.
    pub const CODE: &'static str = "handshake_mismatch";
}

impl fmt::Display for HullHandshakeMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HullHandshakeMismatch::Params(name) => write!(f, "{}: hull field `{name}` differs", Self::CODE),
            HullHandshakeMismatch::Flight => write!(f, "{}: flight mode differs", Self::CODE),
            HullHandshakeMismatch::Limits(name) => {
                write!(f, "{}: hull field `limits.{name}` differs", Self::CODE)
            }
            HullHandshakeMismatch::Extension { offset } => {
                write!(f, "{}: appended byte {offset} differs", Self::CODE)
            }
            HullHandshakeMismatch::Length { local, peer } => {
                write!(f, "{}: {local} bytes here, {peer} from the peer", Self::CODE)
            }
        }
    }
}

/// `None` when the two handshakes are the same bytes (the match may start); otherwise the first
/// field that differs, in byte order, and the host refuses the peer.
///
/// `local` and `peer` are [`hull_handshake_bytes`] output, optionally with a format's own bytes
/// appended.
#[must_use]
pub fn hull_handshake_mismatch(local: &[u8], peer: &[u8]) -> Option<HullHandshakeMismatch> {
    let Some(offset) = local.iter().zip(peer).position(|(a, b)| a != b) else {
        return (local.len() != peer.len()).then_some(HullHandshakeMismatch::Length {
            local: local.len(),
            peer: peer.len(),
        });
    };
    Some(if offset < FLIGHT_AT {
        HullHandshakeMismatch::Params(HoverParams::FIELD_RANGES[offset / 4].name)
    } else if offset == FLIGHT_AT {
        HullHandshakeMismatch::Flight
    } else if offset < HULL_HANDSHAKE_BYTES {
        HullHandshakeMismatch::Limits(HoverLimits::FIELD_RANGES[(offset - LIMITS_AT) / 4].name)
    } else {
        HullHandshakeMismatch::Extension { offset }
    })
}

#[cfg(test)]
mod tests {
    use super::super::FieldRange;
    use super::*;

    /// FNV-1a 64 of the bytes, for the pinned cross-target vector.
    fn fnv1a(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, &b| {
            (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
    }

    fn word(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }

    /// A valid value of `field` other than its default: its max, or its min when the max is the
    /// default.
    fn other_value<T>(field: &FieldRange<T>, base: &T) -> f32 {
        if (field.get)(base).to_bits() == field.max.to_bits() {
            field.min
        } else {
            field.max
        }
    }

    #[test]
    fn layout_is_params_then_flight_then_limits_as_raw_bits() {
        assert_eq!(HULL_HANDSHAKE_BYTES, 273);
        assert_eq!(
            HULL_HANDSHAKE_BYTES,
            HoverParams::BYTES + 1 + HoverLimits::FIELD_RANGES.len() * 4
        );
        let params = HoverParams::DEFAULT_HULL;
        let limits = HoverLimits::DEFAULT;
        let bytes = hull_handshake_bytes(&params, &limits, FlightMode::Clamped);
        assert_eq!(bytes[..HoverParams::BYTES], params.to_le_bytes());
        assert_eq!(bytes[180], 1);
        assert_eq!(
            hull_handshake_bytes(&params, &limits, FlightMode::AirTime)[180],
            0
        );
        for (i, field) in HoverLimits::FIELD_RANGES.iter().enumerate() {
            assert_eq!(
                word(&bytes, 181 + i * 4),
                (field.get)(&limits).to_bits(),
                "{}",
                field.name
            );
        }
        assert_eq!(hull_handshake_mismatch(&bytes, &bytes), None);
    }

    #[test]
    fn each_limits_field_changes_the_handshake_and_is_named() {
        let params = HoverParams::DEFAULT_HULL;
        let base = HoverLimits::DEFAULT;
        let local = hull_handshake_bytes(&params, &base, FlightMode::AirTime);
        for field in HoverLimits::FIELD_RANGES {
            let mut limits = base;
            (field.set)(&mut limits, other_value(field, &base));
            assert!(limits.validate().is_ok(), "{}", field.name);
            let peer = hull_handshake_bytes(&params, &limits, FlightMode::AirTime);
            assert_ne!(local, peer, "{}", field.name);
            assert_eq!(
                hull_handshake_mismatch(&local, &peer),
                Some(HullHandshakeMismatch::Limits(field.name)),
                "{}",
                field.name
            );
            // The params-only bytes cannot see it.
            assert_eq!(
                local[..HoverParams::BYTES],
                peer[..HoverParams::BYTES],
                "{}",
                field.name
            );
        }
    }

    #[test]
    fn each_params_field_and_the_flight_mode_are_named() {
        let base = HoverParams::DEFAULT_HULL;
        let limits = HoverLimits::DEFAULT;
        let local = hull_handshake_bytes(&base, &limits, FlightMode::AirTime);
        for field in HoverParams::FIELD_RANGES {
            let mut params = base;
            (field.set)(&mut params, f32::from_bits((field.get)(&base).to_bits() ^ 1));
            let peer = hull_handshake_bytes(&params, &limits, FlightMode::AirTime);
            assert_eq!(
                hull_handshake_mismatch(&local, &peer),
                Some(HullHandshakeMismatch::Params(field.name)),
                "{}",
                field.name
            );
        }
        let clamped = hull_handshake_bytes(&base, &limits, FlightMode::Clamped);
        assert_eq!(
            hull_handshake_mismatch(&local, &clamped),
            Some(HullHandshakeMismatch::Flight)
        );
        assert_eq!(local[..HoverParams::BYTES], clamped[..HoverParams::BYTES]);
        assert_eq!(local[181..], clamped[181..]);
    }

    #[test]
    fn signed_zero_differs_in_limits_and_params() {
        let params = HoverParams::DEFAULT_HULL;
        let mut plus = HoverLimits::DEFAULT;
        plus.hop_apex_m = 0.0;
        let mut minus = plus;
        minus.hop_apex_m = -0.0;
        assert!(minus.validate().is_ok());
        assert_eq!(plus.hop_apex_m, minus.hop_apex_m, "IEEE == cannot see it");
        let a = hull_handshake_bytes(&params, &plus, FlightMode::AirTime);
        let b = hull_handshake_bytes(&params, &minus, FlightMode::AirTime);
        assert_eq!(
            hull_handshake_mismatch(&a, &b),
            Some(HullHandshakeMismatch::Limits("hop_apex_m"))
        );
        assert_eq!(word(&b, 181), (-0.0_f32).to_bits());

        let mut p_minus = params;
        p_minus.ride_height = -0.0;
        let mut p_plus = params;
        p_plus.ride_height = 0.0;
        let a = hull_handshake_bytes(&p_plus, &plus, FlightMode::AirTime);
        let b = hull_handshake_bytes(&p_minus, &plus, FlightMode::AirTime);
        assert_eq!(
            hull_handshake_mismatch(&a, &b),
            Some(HullHandshakeMismatch::Params("ride_height"))
        );
    }

    #[test]
    fn appended_bytes_and_lengths_are_reported() {
        let hull = hull_handshake_bytes(
            &HoverParams::DEFAULT_HULL,
            &HoverLimits::DEFAULT,
            FlightMode::AirTime,
        );
        let mut local = hull.to_vec();
        local.extend_from_slice(&[7, 8, 9]);
        let mut peer = local.clone();
        peer[HULL_HANDSHAKE_BYTES + 1] = 0;
        assert_eq!(
            hull_handshake_mismatch(&local, &peer),
            Some(HullHandshakeMismatch::Extension {
                offset: HULL_HANDSHAKE_BYTES + 1
            })
        );
        assert_eq!(
            hull_handshake_mismatch(&local, &hull),
            Some(HullHandshakeMismatch::Length {
                local: HULL_HANDSHAKE_BYTES + 3,
                peer: HULL_HANDSHAKE_BYTES
            })
        );
        let shown = HullHandshakeMismatch::Limits("hop_apex_m").to_string();
        assert!(
            shown.starts_with("handshake_mismatch: ") && shown.contains("limits.hop_apex_m"),
            "{shown}"
        );
    }

    /// The same values give the same 273 bytes on every target: this hash is pinned on
    /// x86_64-linux and checked on aarch64 (ADR-8632).
    #[test]
    fn handshake_bytes_are_pinned_across_targets() {
        let mut params = HoverParams::DEFAULT_HULL;
        params.ride_height = 1.5;
        params.height_damper = 7.5;
        let mut limits = HoverLimits::DEFAULT;
        limits.hop_apex_m = 10.0;
        limits.roll_lean_max = -0.0;
        let default = hull_handshake_bytes(
            &HoverParams::DEFAULT_HULL,
            &HoverLimits::DEFAULT,
            FlightMode::AirTime,
        );
        let tuned = hull_handshake_bytes(&params, &limits, FlightMode::Clamped);
        let got = (fnv1a(&default), fnv1a(&tuned));
        assert_eq!(
            got,
            (0x00db_6559_9367_4191, 0xe21c_8da6_209b_5bb3),
            "pin: {:#018x}, {:#018x}",
            got.0,
            got.1
        );
    }
}
