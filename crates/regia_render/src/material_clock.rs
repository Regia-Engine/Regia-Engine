// SPDX-License-Identifier: MPL-2.0
//! The material clock: the seconds a material graph's `Time` node reads (world position
//! offsets such as wind sway, scrolling UVs, pulsing emissive), wrapped so that it stays exact in
//! `f32` however long a game runs (ADR-1238).
//!
//! Gameplay time is a [`Duration`] (whole nanoseconds, exact). Sent to the GPU as plain `f32`
//! seconds it loses resolution as it grows: the gap between neighboring `f32` values is 3.9 ms
//! after 9.1 hours (2^15 s), 15.6 ms after 36 hours and 0.25 s after 30 days. Wind then sways
//! in visible steps, and the frame-to-frame difference that the velocity pass turns into motion
//! vectors comes out as zero or as several frames' worth, which TAA shows as smearing.
//!
//! [`material_clock`] reduces the elapsed time modulo [`MATERIAL_CLOCK_PERIOD_SECS`] in integer
//! nanoseconds before converting, so the clock never passes 3600 (a gap of at most 0.25 ms), and
//! the result depends on nothing but its inputs: two runs fed the same gameplay time draw the
//! same offsets, frame for frame.
//!
//! The wrap is a jump back to zero once per period. A graph whose motion repeats with a period
//! that divides 3600 s (1, 2, 3, 4, 5, 6, 8, 10, 12, 15 s, ..., and 0.5, 0.75, 1.5, 2.5 s, ...)
//! loops across it without a seam. The previous-frame value is taken on the same lap as the
//! current one (negative just after a wrap), so motion vectors never see the jump.

use std::time::Duration;

/// Length of one lap of the material clock, in seconds. 3600 has many divisors, so periodic
/// motion with any of the usual periods loops across the wrap without a seam.
pub const MATERIAL_CLOCK_PERIOD_SECS: u64 = 3600;

const PERIOD_NANOS: u128 = MATERIAL_CLOCK_PERIOD_SECS as u128 * 1_000_000_000;

/// The material clock `(now, prev)` in seconds at gameplay time `elapsed`, where `delta` is the
/// frame's gameplay delta (so `prev` is the clock one frame earlier).
///
/// `now` is `elapsed` modulo [`MATERIAL_CLOCK_PERIOD_SECS`], reduced in integer nanoseconds.
/// `prev` is `now - delta` on the same lap: just after a wrap it is negative, so `now - prev`
/// equals `delta` on every frame and motion vectors stay right. A paused frame (`delta` zero)
/// has `prev == now`. Feed both to [`crate::FrameRenderer::set_material_clock`].
#[must_use]
pub fn material_clock(elapsed: Duration, delta: Duration) -> (f32, f32) {
    // Integer nanoseconds until the final division, so `prev` of one frame and `now` of the
    // frame before are the same number whenever `delta` is the step between them.
    let lap = elapsed.as_nanos() % PERIOD_NANOS;
    let prev = lap as i128 - delta.as_nanos() as i128;
    ((lap as f64 / 1.0e9) as f32, (prev as f64 / 1.0e9) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_nanos(16_666_667);

    #[test]
    fn the_clock_is_gameplay_time_before_the_first_wrap() {
        let (now, prev) = material_clock(Duration::from_millis(1500), FRAME);
        assert_eq!(now, 1.5);
        assert!((now - prev - FRAME.as_secs_f32()).abs() < 1.0e-6);
        let (now, prev) = material_clock(Duration::ZERO, Duration::ZERO);
        assert_eq!((now, prev), (0.0, 0.0));
    }

    #[test]
    fn the_clock_wraps_once_per_period_and_keeps_the_frame_delta() {
        let period = Duration::from_secs(MATERIAL_CLOCK_PERIOD_SECS);
        // One lap later the clock reads the same, bit for bit.
        for t in [Duration::from_millis(250), Duration::from_nanos(1_234_567_891)] {
            assert_eq!(material_clock(t, FRAME), material_clock(t + 7 * period, FRAME));
        }
        // 4 ms after the wrap: `prev` is on the same lap (negative), the delta is one frame.
        let (now, prev) = material_clock(period + Duration::from_millis(4), FRAME);
        assert!((now - 0.004).abs() < 1.0e-6, "now {now}");
        assert!(prev < 0.0, "prev {prev} is measured on the new lap");
        assert!((now - prev - FRAME.as_secs_f32()).abs() < 1.0e-6);
        // A paused frame does not move.
        let (now, prev) = material_clock(period + Duration::from_secs(5), Duration::ZERO);
        assert_eq!(now, prev);
    }

    /// The defect this module removes: plain `f32` seconds after 30 days of gameplay cannot
    /// tell two frames apart, the wrapped clock still resolves a frame to within 1 %.
    #[test]
    fn the_frame_delta_survives_a_month_of_gameplay() {
        let elapsed = Duration::from_secs(30 * 24 * 3600) + Duration::from_millis(123);
        let naive = elapsed.as_secs_f32() - (elapsed - FRAME).as_secs_f32();
        let frame = FRAME.as_secs_f32();
        assert!(
            (naive - frame).abs() > 0.5 * frame,
            "plain f32 seconds lose the frame ({naive} s against {frame} s)"
        );
        let (now, prev) = material_clock(elapsed, FRAME);
        assert!(now < MATERIAL_CLOCK_PERIOD_SECS as f32);
        assert!(
            ((now - prev) - frame).abs() < 0.01 * frame,
            "the wrapped clock keeps the frame: {} s",
            now - prev
        );
        // Every frame of a long run advances by the same delta, without steps, across a wrap
        // too (the run starts 5 s before one).
        let start = elapsed - Duration::from_secs(5);
        for k in 0..600u32 {
            let t = start + FRAME * k;
            let (a, _) = material_clock(t, FRAME);
            let (b, _) = material_clock(t + FRAME, FRAME);
            let step = if b < a {
                b + MATERIAL_CLOCK_PERIOD_SECS as f32 - a
            } else {
                b - a
            };
            assert!((step - frame).abs() < 0.03 * frame, "frame {k}: step {step}");
        }
    }
}
