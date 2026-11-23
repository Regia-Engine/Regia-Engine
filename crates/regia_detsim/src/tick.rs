// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/tick.rs, commit f7e8f4740), ADR-8012.
// Mark: as-is. The constant and the three functions are DO27's bytes; the doc comments name Regia's
// records instead of DO27's files. `TickRate` is new: a host that runs another rate passes it as a
// setting (ADR-8012 A-1: DO27's caps become settings), and at 60 Hz it returns the same bits.

//! Single authoritative sim tick rate and `dt`.
//!
//! `dt = 1 / TICK_RATE_HZ`. Every duration stored as a tick count is
//! `seconds / dt` (equivalently `seconds * TICK_RATE_HZ`). Call sites that
//! still hardcode another rate or its `dt` are bugs.

use core::num::NonZeroU32;

/// Authoritative simulation tick rate in Hz: the default lockstep rate. A host that runs another
/// rate uses [`TickRate`].
pub const TICK_RATE_HZ: u32 = 60;

/// `dt = 1 / tick_rate_hz` for one sim tick.
#[inline]
pub const fn tick_dt() -> f32 {
    1.0 / TICK_RATE_HZ as f32
}

/// Convert a real-time duration in seconds to a tick count at [`TICK_RATE_HZ`].
#[inline]
pub const fn ticks_from_secs(secs: f32) -> u64 {
    (secs * TICK_RATE_HZ as f32) as u64
}

/// Convert a real-time duration in seconds to a `u32` tick count.
#[inline]
pub const fn ticks_from_secs_u32(secs: f32) -> u32 {
    (secs * TICK_RATE_HZ as f32) as u32
}

/// A sim tick rate chosen by the host, with the same arithmetic as the [`TICK_RATE_HZ`] functions.
///
/// `TickRate::DEFAULT` is [`TICK_RATE_HZ`]; at that rate [`TickRate::dt`],
/// [`TickRate::ticks_from_secs`] and [`TickRate::ticks_from_secs_u32`] return the same bits as
/// [`tick_dt`], [`ticks_from_secs`] and [`ticks_from_secs_u32`]. Every peer of a lockstep match must
/// run the same rate; it belongs in the match settings the handshake compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TickRate(NonZeroU32);

impl TickRate {
    /// The default lockstep rate, [`TICK_RATE_HZ`].
    pub const DEFAULT: TickRate = match NonZeroU32::new(TICK_RATE_HZ) {
        Some(hz) => TickRate(hz),
        None => panic!("TICK_RATE_HZ is not zero"),
    };

    /// A rate of `hz` ticks per second; `None` for zero.
    #[inline]
    pub const fn new(hz: u32) -> Option<TickRate> {
        match NonZeroU32::new(hz) {
            Some(hz) => Some(TickRate(hz)),
            None => None,
        }
    }

    /// Ticks per second.
    #[inline]
    pub const fn hz(self) -> u32 {
        self.0.get()
    }

    /// `dt = 1 / hz` for one sim tick.
    #[inline]
    pub const fn dt(self) -> f32 {
        1.0 / self.0.get() as f32
    }

    /// Convert a real-time duration in seconds to a tick count at this rate.
    #[inline]
    pub const fn ticks_from_secs(self, secs: f32) -> u64 {
        (secs * self.0.get() as f32) as u64
    }

    /// Convert a real-time duration in seconds to a `u32` tick count at this rate.
    #[inline]
    pub const fn ticks_from_secs_u32(self, secs: f32) -> u32 {
        (secs * self.0.get() as f32) as u32
    }
}

impl Default for TickRate {
    fn default() -> Self {
        TickRate::DEFAULT
    }
}

/// Saves the rate in Hz; `load` refuses 0 (`bad_value`).
impl crate::state::DetState for TickRate {
    const STATE_TAG: [u8; 8] = *b"tickrate";
    const STATE_VERSION: u32 = 1;

    fn save(&self, w: &mut crate::state::DetWriter) {
        w.u32(self.hz());
    }

    fn load(r: &mut crate::state::DetReader<'_>) -> Result<Self, crate::state::DetStateError> {
        let hz = r.u32()?;
        TickRate::new(hz).ok_or_else(|| crate::state::DetStateError::bad_value("tick rate 0 Hz"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_rate_returns_the_constant_functions_bits() {
        let rate = TickRate::default();
        assert_eq!(rate.hz(), TICK_RATE_HZ);
        assert_eq!(rate.dt().to_bits(), tick_dt().to_bits());
        for secs in [
            0.0_f32,
            0.016_666_668,
            0.05,
            0.5,
            1.0,
            2.5,
            3.333_333_3,
            60.0,
            3600.0,
        ] {
            assert_eq!(rate.ticks_from_secs(secs), ticks_from_secs(secs), "{secs}");
            assert_eq!(
                rate.ticks_from_secs_u32(secs),
                ticks_from_secs_u32(secs),
                "{secs}"
            );
        }
    }

    #[test]
    fn tick_conversions_truncate_toward_zero() {
        assert_eq!(tick_dt().to_bits(), (1.0_f32 / 60.0).to_bits());
        assert_eq!(ticks_from_secs(1.0), 60);
        assert_eq!(ticks_from_secs(0.999), 59);
        assert_eq!(ticks_from_secs_u32(0.5), 30);
        // A negative duration saturates to zero (an `as` cast), never wraps.
        assert_eq!(ticks_from_secs(-1.0), 0);
        let thirty = TickRate::new(30).expect("30 Hz is a rate");
        assert_eq!(thirty.ticks_from_secs(1.0), 30);
        assert_eq!(thirty.dt().to_bits(), (1.0_f32 / 30.0).to_bits());
        assert_eq!(TickRate::new(0), None);
    }
}
