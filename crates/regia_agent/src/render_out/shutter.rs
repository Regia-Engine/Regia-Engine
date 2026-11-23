// SPDX-License-Identifier: MPL-2.0
//! Shutter accumulation timeline (`render.out_start` `subframes` / `shutter`, ADR-1404).
//!
//! A delivered frame `i` with `N > 1` subframes is the average, in linear light, of `N` renders
//! taken while a film-camera shutter is open: the shutter opens at the frame's own game time
//! `T_i = T_0 + i · step` and stays open for `open = step · angle / 360°` (180° = half a frame).
//! Subframe `k` is rendered at the midpoint of its slice of the open interval,
//! `T_i + offset(k)` with `offset(k) = round((2k + 1) · open / 2N)` (the midpoint rule for a box
//! shutter). Everything is whole nanoseconds computed from the request alone, so frame `i`'s
//! subframe times never depend on what was rendered before it (a job that starts at frame `i`
//! sees the same times), and every frame advances the game clock by exactly one `step`: the
//! clock waits `delay_after(k)` between subframes and closes the frame with the rest of the step.
//!
//! The fixed-step clock (`FixedUpdate`) still runs at its own rate: it steps when the subframe
//! deltas add up to a fixed step, and interpolated poses (`FixedTime::overstep_fraction`, physics)
//! move between subframes. `Update` systems, VFX and camera paths see each subframe's delta.
//!
//! `N = 1` (the default) is the old timeline exactly: one render at `T_i`, whatever the angle.
//!
//! With a time map (ADR-1519, ADR-1524) the render clock walks exactly the timeline above, and the
//! simulation clock has its own step `d_i` per frame: in simulation time the shutter opens over
//! `d_i · angle / 360°`, subframe `k` is at `G_i + offset_i(k)`, and the simulation clock's step
//! from the last subframe of frame `i` to the first of frame `i + 1` is
//! `d_i − offset_i(N − 1) + offset_{i+1}(0)` ([`Shutter::delay_between`]).

/// Most subframes per delivered frame.
pub const MAX_SUBFRAMES: u32 = 64;
/// Default shutter angle, degrees (a film camera's half-frame shutter).
pub const DEFAULT_SHUTTER_DEG: f64 = 180.0;
/// Shutter angles are kept in millidegrees so the timeline is integer arithmetic.
const FULL_TURN_MILLIDEG: u128 = 360_000;

/// How each delivered frame is sampled in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shutter {
    /// Renders averaged into one delivered frame (`1..=MAX_SUBFRAMES`).
    pub subframes: u32,
    /// Shutter angle in millidegrees (`1..=360000`); 180000 is half a frame open.
    pub angle_millideg: u32,
}

impl Default for Shutter {
    fn default() -> Self {
        Self::OFF
    }
}

impl Shutter {
    /// One render per frame at the frame's time (no motion blur).
    pub const OFF: Self = Self {
        subframes: 1,
        angle_millideg: 180_000,
    };

    /// A shutter of `subframes` renders over `angle_deg` degrees, or `None` when either is out of
    /// range (`subframes` `1..=MAX_SUBFRAMES`, `angle_deg` finite and in `(0, 360]`; the angle is
    /// rounded to the millidegree).
    pub fn new(subframes: u32, angle_deg: f64) -> Option<Self> {
        if !(1..=MAX_SUBFRAMES).contains(&subframes) || !angle_deg.is_finite() {
            return None;
        }
        let millideg = (angle_deg * 1000.0).round();
        if !(1.0..=360_000.0).contains(&millideg) || angle_deg <= 0.0 {
            return None;
        }
        Some(Self {
            subframes,
            angle_millideg: millideg as u32,
        })
    }

    /// True when a frame averages more than one render.
    pub fn is_on(self) -> bool {
        self.subframes > 1
    }

    /// The shutter angle in degrees.
    pub fn angle_deg(self) -> f64 {
        f64::from(self.angle_millideg) / 1000.0
    }

    /// How long the shutter is open in one frame of `step_ns`, nanoseconds (rounded).
    pub fn open_ns(self, step_ns: u64) -> u64 {
        let open = (u128::from(step_ns) * u128::from(self.angle_millideg) + FULL_TURN_MILLIDEG / 2)
            / FULL_TURN_MILLIDEG;
        open as u64
    }

    /// Subframe `k`'s time after the frame's own time, nanoseconds. Zero when the shutter is off.
    pub fn offset_ns(self, step_ns: u64, k: u32) -> u64 {
        if !self.is_on() {
            return 0;
        }
        let n = u128::from(self.subframes);
        let k = u128::from(k.min(self.subframes - 1));
        let open = u128::from(self.open_ns(step_ns));
        // round((2k + 1) · open / 2N); never above `open`, which is at most one step.
        (((2 * k + 1) * open + n) / (2 * n)) as u64
    }

    /// How far the clock moves before the first subframe of frame 0, beyond the frame's own time:
    /// `offset(0)`.
    pub fn lead_ns(self, step_ns: u64) -> u64 {
        self.offset_ns(step_ns, 0)
    }

    /// The clock step after subframe `k` is rendered: to subframe `k + 1`, or after the last one,
    /// on to the next frame's first subframe. The delays of one frame add up to `step_ns`. The
    /// render clock always walks these (one output step per frame).
    pub fn delay_after(self, step_ns: u64, k: u32) -> u64 {
        self.delay_between(step_ns, step_ns, k)
    }

    /// `delay_after` when the frame's game step is `step_ns` and the next frame's is
    /// `next_step_ns` (a time map): after the last subframe the clock moves to the next frame's
    /// first subframe, `offset(0)` of *its* step past the next frame's time.
    pub fn delay_between(self, step_ns: u64, next_step_ns: u64, k: u32) -> u64 {
        if !self.is_on() {
            return step_ns;
        }
        let last = self.subframes - 1;
        if k < last {
            self.offset_ns(step_ns, k + 1) - self.offset_ns(step_ns, k)
        } else {
            step_ns - self.offset_ns(step_ns, last) + self.offset_ns(next_step_ns, 0)
        }
    }

    /// Game time of subframe `k` of delivered frame `frame`, nanoseconds after frame 0's own time
    /// (the reference the clock walk is tested against).
    #[cfg(test)]
    pub fn sample_ns(self, step_ns: u64, frame: u64, k: u32) -> u64 {
        step_ns
            .saturating_mul(frame)
            .saturating_add(self.offset_ns(step_ns, k))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEP_60: u64 = 16_666_667;

    #[test]
    fn construction_validates_the_count_and_the_angle() {
        assert_eq!(Shutter::new(1, 180.0), Some(Shutter::OFF));
        assert!(Shutter::new(0, 180.0).is_none());
        assert!(Shutter::new(MAX_SUBFRAMES + 1, 180.0).is_none());
        assert!(Shutter::new(8, 0.0).is_none());
        assert!(Shutter::new(8, -90.0).is_none());
        assert!(Shutter::new(8, 360.1).is_none());
        assert!(Shutter::new(8, f64::NAN).is_none());
        assert!(Shutter::new(8, f64::INFINITY).is_none());
        assert!(Shutter::new(8, 0.0004).is_none(), "rounds to zero millidegrees");
        let s = Shutter::new(8, 172.8).expect("valid");
        assert_eq!(s.angle_millideg, 172_800);
        assert_eq!(s.angle_deg(), 172.8);
        assert!(Shutter::new(MAX_SUBFRAMES, 360.0).is_some());
    }

    #[test]
    fn the_shutter_off_is_the_old_timeline() {
        let off = Shutter::OFF;
        assert!(!off.is_on());
        assert_eq!(off.lead_ns(STEP_60), 0);
        assert_eq!(off.delay_after(STEP_60, 0), STEP_60);
        assert_eq!(off.sample_ns(STEP_60, 3, 0), 3 * STEP_60);
        // One subframe at any angle is still one render at the frame's time.
        let one = Shutter::new(1, 90.0).expect("valid");
        assert_eq!(one.offset_ns(STEP_60, 0), 0);
        assert_eq!(one.delay_after(STEP_60, 0), STEP_60);
    }

    #[test]
    fn subframes_sit_at_the_midpoints_of_the_open_interval() {
        // 60 fps, 180 degrees, 4 subframes: open 8.333 ms, slices of 2.083 ms.
        let s = Shutter::new(4, 180.0).expect("valid");
        assert_eq!(s.open_ns(STEP_60), 8_333_334);
        let offsets: Vec<u64> = (0..4).map(|k| s.offset_ns(STEP_60, k)).collect();
        assert_eq!(offsets, [1_041_667, 3_125_000, 5_208_334, 7_291_667]);
        // Every sample lies inside the open interval, in order, one slice apart (± 1 ns rounding).
        assert!(offsets.windows(2).all(|w| w[0] < w[1]));
        assert!(offsets.iter().all(|o| *o < s.open_ns(STEP_60)));
        for w in offsets.windows(2) {
            assert!((w[1] - w[0]).abs_diff(2_083_333) <= 1, "{w:?}");
        }
    }

    #[test]
    fn every_frame_advances_the_clock_by_exactly_one_step() {
        for (step, n, deg) in [
            (STEP_60, 8, 180.0),
            (33_366_667, 5, 172.8),
            (41_666_667, 64, 360.0),
            (1_000_000, 64, 1.0),
            (STEP_60, 2, 45.0),
            (3, 7, 359.0),
        ] {
            let s = Shutter::new(n, deg).expect("valid");
            let total: u64 = (0..n).map(|k| s.delay_after(step, k)).sum();
            assert_eq!(total, step, "{n} subframes at {deg} degrees, step {step}");
        }
    }

    #[test]
    fn walking_the_delays_reaches_every_sample_time_exactly() {
        // The clock the render-out drives: start at lead, then add each delay. It must hit
        // `sample_ns(frame, k)` for every frame, so a long job never drifts and any frame's times
        // equal those of a job that starts at that frame (seek-safe).
        let s = Shutter::new(6, 180.0).expect("valid");
        let step = 33_366_667;
        let mut clock = s.lead_ns(step);
        for frame in 0..500u64 {
            for k in 0..s.subframes {
                assert_eq!(clock, s.sample_ns(step, frame, k), "frame {frame} subframe {k}");
                clock += s.delay_after(step, k);
            }
        }
        assert_eq!(clock, s.sample_ns(step, 500, 0));
    }

    #[test]
    fn varying_frame_steps_reach_every_sample_time_exactly() {
        // Frame i covers its own game step; subframe k of frame i must be at
        // G(i) + offset_i(k), G the running sum of the steps.
        let s = Shutter::new(5, 172.8).expect("valid");
        let steps: Vec<u64> = (0..300u64)
            .map(|i| 4_000_000 + (i * 7_919) % 33_000_000)
            .collect();
        let mut clock = s.lead_ns(steps[0]);
        let mut game = 0;
        for (i, pair) in steps.windows(2).enumerate() {
            let (step, next) = (pair[0], pair[1]);
            for k in 0..s.subframes {
                assert_eq!(clock, game + s.offset_ns(step, k), "frame {i} subframe {k}");
                clock += s.delay_between(step, next, k);
            }
            game += step;
        }
        // Equal steps are the old walk.
        for k in 0..s.subframes {
            assert_eq!(s.delay_between(STEP_60, STEP_60, k), s.delay_after(STEP_60, k));
        }
        assert_eq!(
            Shutter::OFF.delay_between(STEP_60, 1, 0),
            STEP_60,
            "off: one frame step"
        );
        // A frozen frame (step 0) holds the clock between its subframes.
        assert_eq!(s.delay_between(0, STEP_60, 1), 0);
    }

    #[test]
    fn a_full_turn_spreads_the_samples_over_the_whole_frame() {
        let s = Shutter::new(2, 360.0).expect("valid");
        assert_eq!(s.open_ns(1_000), 1_000);
        assert_eq!((s.offset_ns(1_000, 0), s.offset_ns(1_000, 1)), (250, 750));
        assert_eq!(s.delay_after(1_000, 0), 500);
        assert_eq!(s.delay_after(1_000, 1), 500);
    }

    #[test]
    fn out_of_range_subframe_indices_clamp_to_the_last() {
        let s = Shutter::new(3, 180.0).expect("valid");
        assert_eq!(s.offset_ns(STEP_60, 9), s.offset_ns(STEP_60, 2));
        assert_eq!(s.sample_ns(u64::MAX, 2, 0), u64::MAX, "saturates");
    }
}
