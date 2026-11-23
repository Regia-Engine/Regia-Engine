// SPDX-License-Identifier: MPL-2.0
//! The render-out time map (`render.out_start` `time_map`, ADR-1519, ADR-1524): how much
//! simulation time each delivered frame covers, so a job can ramp into slow motion or speed up
//! over a frame range.
//!
//! A job runs two clocks (ADR-1524). The render clock is the output's: delivered frame `i` sits
//! at `i · step` (`step = fps_den / fps_num`; `game_time_s`), and it drives `Time::delta`, so VFX,
//! shot players, HUD animation, the material clock and the shutter/TAA keep their speed. The
//! simulation clock (`Time::sim_delta`, set through `regia_app::SimStep`; `sim_time_s`) drives the
//! fixed loop (physics, gameplay) and sim bridges (a replay player): the map scales it. The map is
//! a list of keys `{frame, scale, ease}` sorted by frame. Before the first key and after the last
//! the scale holds; between two keys it ramps into the later key's scale with that key's `ease`:
//! `smooth` (the default: smoothstep `3t² − 2t³`, so the scale's slope is zero at both keys),
//! `linear`, or `hold` (the earlier scale up to the later key's frame, then a jump).
//!
//! Frame `j` covers `d(j) = round(step · s(j + ½))` nanoseconds of simulation time: the scale is
//! read at the frame's midpoint (the midpoint rule for the integral of the scale over the frame),
//! and frame `i` is at simulation time `G(i) = Σ_{j<i} d(j)` after frame 0. Scales are kept in
//! millionths and the ramp is a ratio of integer polynomials, so the timeline is exact integer
//! arithmetic: the same request gives the same simulation times on every platform, and no clock is
//! read. Warm-up frames and the step into frame 0 cover `round(step · s₀)`, `s₀` being the scale
//! before the first key.
//!
//! With no map (the default) every `d(j)` is `step` and the job sets no simulation step: the two
//! clocks are one, byte for byte the timeline before this map existed.
//!
//! The shutter (`subframes`, `shutter_angle`) samples on the render clock, at the same offsets
//! into each output frame as without a map; the simulation moves by the same share of the frame's
//! `d(j)` between subframes, so in simulation time the motion blur shortens with the scale, as a
//! high-speed camera's does. The fixed loop keeps its fixed step: a slowed frame runs fewer fixed
//! steps and presentation interpolates between them.

use serde_json::{Value, json};

/// Most keys a time map may have.
pub const MAX_TIME_KEYS: usize = 4096;
/// Largest scale a key may ask for (simulation time per unit of video time).
pub const MAX_KEY_SCALE: f64 = 100.0;
/// Scales are kept in millionths.
const PPM: u64 = 1_000_000;

/// How the scale ramps into a key from the key before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ease {
    /// Smoothstep: the scale's slope is zero at both keys (the default).
    Smooth,
    /// A straight ramp.
    Linear,
    /// The earlier key's scale up to this key's frame, then a jump.
    Hold,
}

impl Ease {
    /// Every ease, in the order the protocol lists them.
    pub const ALL: [Self; 3] = [Self::Smooth, Self::Linear, Self::Hold];

    /// Parses an `ease` value.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "smooth" => Some(Self::Smooth),
            "linear" => Some(Self::Linear),
            "hold" => Some(Self::Hold),
            _ => None,
        }
    }

    /// Stable name (protocol, manifest).
    pub fn name(self) -> &'static str {
        match self {
            Self::Smooth => "smooth",
            Self::Linear => "linear",
            Self::Hold => "hold",
        }
    }
}

/// One key of a time map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeKey {
    /// Delivered frame (0-based within the job) the scale is reached at.
    pub frame: u64,
    /// Simulation time per unit of video time, in millionths (1 000 000 = real time, 0 = frozen).
    pub scale_ppm: u64,
    /// How the scale ramps into this key from the previous one (ignored on the first key).
    pub ease: Ease,
}

/// A validated time map; empty is scale 1 everywhere.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TimeMap {
    keys: Vec<TimeKey>,
}

/// `n / d` rounded to the nearest integer, halves away from zero (`d > 0`).
fn div_round(n: i128, d: i128) -> i128 {
    if n >= 0 {
        (n + d / 2) / d
    } else {
        -((-n + d / 2) / d)
    }
}

/// `step_ns` scaled by `ppm` millionths, rounded to the nanosecond (saturating).
fn scaled(step_ns: u64, ppm: u64) -> u64 {
    let ns = (u128::from(step_ns) * u128::from(ppm) + u128::from(PPM / 2)) / u128::from(PPM);
    u64::try_from(ns).unwrap_or(u64::MAX)
}

impl TimeMap {
    /// Parses the `time_map` parameter: `null`/absent or an array of `{frame, scale, ease?}`.
    /// Keys are sorted by frame; two keys on one frame, a frame past `max_frame`, a scale outside
    /// `0..=MAX_KEY_SCALE` or an unknown `ease` is an error (the message names the problem).
    pub fn parse(value: Option<&Value>, max_frame: u64) -> Result<Self, String> {
        let list = match value {
            None | Some(Value::Null) => return Ok(Self::default()),
            Some(Value::Array(list)) => list,
            Some(_) => return Err("`time_map` must be an array of {frame, scale, ease?} keys".into()),
        };
        if list.len() > MAX_TIME_KEYS {
            return Err(format!(
                "`time_map` has {} keys; the limit is {MAX_TIME_KEYS}",
                list.len()
            ));
        }
        let mut keys = Vec::with_capacity(list.len());
        for entry in list {
            let frame = entry
                .get("frame")
                .and_then(Value::as_u64)
                .filter(|f| *f <= max_frame)
                .ok_or_else(|| format!("`time_map` keys need a `frame`, an integer in 0..={max_frame}"))?;
            let scale = entry
                .get("scale")
                .and_then(Value::as_f64)
                .filter(|s| s.is_finite() && (0.0..=MAX_KEY_SCALE).contains(s))
                .ok_or_else(|| {
                    format!("`time_map` keys need a `scale`, a number in 0..={MAX_KEY_SCALE} (1 = real time)")
                })?;
            let ease = match entry.get("ease") {
                None | Some(Value::Null) => Ease::Smooth,
                Some(v) => v.as_str().and_then(Ease::parse).ok_or_else(|| {
                    "`time_map` `ease` must be \"smooth\" (default), \"linear\" or \"hold\"".to_string()
                })?,
            };
            keys.push(TimeKey {
                frame,
                scale_ppm: (scale * PPM as f64).round() as u64,
                ease,
            });
        }
        keys.sort_by_key(|k| k.frame);
        if let Some(pair) = keys.windows(2).find(|w| w[0].frame == w[1].frame) {
            return Err(format!("`time_map` has two keys at frame {}", pair[0].frame));
        }
        Ok(Self { keys })
    }

    /// The keys, sorted by frame.
    #[cfg(test)]
    pub fn keys(&self) -> &[TimeKey] {
        &self.keys
    }

    /// True when every frame covers exactly one step (no keys, or every key at scale 1).
    pub fn is_identity(&self) -> bool {
        self.keys.iter().all(|k| k.scale_ppm == PPM)
    }

    /// True when the map holds the simulation on every frame: it has keys and every one is scale 0
    /// (before the first key and after the last the scale stays at that key's).
    pub fn holds_the_world(&self) -> bool {
        !self.keys.is_empty() && self.keys.iter().all(|k| k.scale_ppm == 0)
    }

    /// The scale in millionths at `x2`, twice a position on the frame axis (frame `j`'s midpoint
    /// is `2j + 1`; keys sit at even positions, so a midpoint is never on a key).
    fn scale_ppm_at(&self, x2: i128) -> u64 {
        let Some(first) = self.keys.first() else {
            return PPM;
        };
        let after = self.keys.partition_point(|k| 2 * i128::from(k.frame) <= x2);
        if after == 0 {
            return first.scale_ppm;
        }
        let a = self.keys[after - 1];
        let Some(b) = self.keys.get(after) else {
            return a.scale_ppm;
        };
        let (sa, sb) = (i128::from(a.scale_ppm), i128::from(b.scale_ppm));
        let num = x2 - 2 * i128::from(a.frame);
        let den = 2 * i128::from(b.frame - a.frame);
        let delta = sb - sa;
        let scale = match b.ease {
            Ease::Hold => sa,
            Ease::Linear => sa + div_round(delta * num, den),
            // t²(3 − 2t) with t = num / den, as one integer ratio.
            Ease::Smooth => sa + div_round(delta * num * num * (3 * den - 2 * num), den * den * den),
        };
        u64::try_from(scale.max(0)).unwrap_or(0)
    }

    /// The scale of delivered frame `frame` (read at its midpoint), in millionths.
    pub fn frame_scale_ppm(&self, frame: u64) -> u64 {
        self.scale_ppm_at(2 * i128::from(frame) + 1)
    }

    /// The scale before the first key (warm-up frames and the step into frame 0), in millionths.
    pub fn pre_scale_ppm(&self) -> u64 {
        self.keys.first().map_or(PPM, |k| k.scale_ppm)
    }

    /// Simulation time delivered frame `frame` covers, nanoseconds: `round(step · s(frame + ½))`.
    pub fn frame_step_ns(&self, step_ns: u64, frame: u64) -> u64 {
        if self.keys.is_empty() {
            return step_ns;
        }
        scaled(step_ns, self.frame_scale_ppm(frame))
    }

    /// Simulation time a warm-up frame (and the step into frame 0) covers, nanoseconds.
    pub fn pre_step_ns(&self, step_ns: u64) -> u64 {
        scaled(step_ns, self.pre_scale_ppm())
    }

    /// Simulation time of delivered frame `index` after frame 0, nanoseconds (saturating):
    /// `Σ_{j<index} frame_step_ns(j)`. Constant stretches before the first key and after the last
    /// are multiplied, so the cost is the span between the keys.
    pub fn sim_ns(&self, step_ns: u64, index: u64) -> u64 {
        let (Some(first), Some(last)) = (self.keys.first(), self.keys.last()) else {
            return step_ns.saturating_mul(index);
        };
        // Frames before the first key have their midpoint before it; frames from the last key on
        // have it after.
        let head = index.min(first.frame);
        let mut total = scaled(step_ns, first.scale_ppm).saturating_mul(head);
        for j in head..index.min(last.frame) {
            total = total.saturating_add(self.frame_step_ns(step_ns, j));
        }
        let tail = index.saturating_sub(last.frame.max(head));
        total.saturating_add(scaled(step_ns, last.scale_ppm).saturating_mul(tail))
    }

    /// The longest simulation step of a job of `frames` frames (the warm-up step included).
    pub fn peak_step_ns(&self, step_ns: u64, frames: u64) -> u64 {
        let (Some(first), Some(last)) = (self.keys.first(), self.keys.last()) else {
            return step_ns;
        };
        let mut peak = self.pre_step_ns(step_ns);
        for j in first.frame.min(frames)..last.frame.min(frames) {
            peak = peak.max(self.frame_step_ns(step_ns, j));
        }
        if frames > last.frame {
            peak = peak.max(scaled(step_ns, last.scale_ppm));
        }
        peak
    }

    /// The keys as the request echo and the manifest show them.
    pub fn to_json(&self) -> Value {
        Value::Array(
            self.keys
                .iter()
                .map(|k| {
                    json!({
                        "frame": k.frame,
                        "scale": k.scale_ppm as f64 / PPM as f64,
                        "ease": k.ease.name(),
                    })
                })
                .collect(),
        )
    }
}

/// A scale in millionths as a number (1.0 = real time).
pub fn scale_from_ppm(ppm: u64) -> f64 {
    ppm as f64 / PPM as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEP_30: u64 = 33_333_333;

    fn map(keys: Value) -> TimeMap {
        TimeMap::parse(Some(&keys), 1_000_000).expect("time map")
    }

    #[test]
    fn only_a_map_of_all_zero_scales_holds_the_world() {
        assert!(!TimeMap::parse(None, 10).expect("none").holds_the_world());
        assert!(map(json!([{ "frame": 0, "scale": 0, "ease": "hold" }])).holds_the_world());
        assert!(
            map(json!([{ "frame": 0, "scale": 0, "ease": "hold" }, { "frame": 8, "scale": 0, "ease": "hold" }]))
                .holds_the_world()
        );
        assert!(!map(json!([{ "frame": 0, "scale": 1, "ease": "hold" }])).holds_the_world());
        assert!(
            !map(json!([{ "frame": 0, "scale": 0, "ease": "hold" }, { "frame": 8, "scale": 0.5, "ease": "hold" }]))
                .holds_the_world()
        );
    }

    /// The reference the closed forms are checked against: the scale in floating point.
    fn reference_scale(keys: &[(u64, f64, &str)], x: f64) -> f64 {
        let first = keys[0];
        if x < first.0 as f64 {
            return first.1;
        }
        for w in keys.windows(2) {
            let ((fa, sa, _), (fb, sb, ease)) = (w[0], w[1]);
            if x < fb as f64 {
                let t = (x - fa as f64) / (fb - fa) as f64;
                return match ease {
                    "hold" => sa,
                    "linear" => sa + (sb - sa) * t,
                    _ => sa + (sb - sa) * t * t * (3.0 - 2.0 * t),
                };
            }
        }
        keys[keys.len() - 1].1
    }

    #[test]
    fn no_map_is_the_old_timeline() {
        let none = TimeMap::parse(None, 10).expect("none");
        assert!(none.is_identity());
        assert_eq!(none, TimeMap::parse(Some(&Value::Null), 10).expect("null"));
        assert_eq!(none, map(json!([])));
        for j in [0, 1, 7, 999_999] {
            assert_eq!(none.frame_step_ns(STEP_30, j), STEP_30);
        }
        assert_eq!(none.pre_step_ns(STEP_30), STEP_30);
        assert_eq!(none.sim_ns(STEP_30, 90), STEP_30 * 90);
        assert_eq!(none.sim_ns(STEP_30, u64::MAX), u64::MAX, "saturates");
        assert_eq!(none.peak_step_ns(STEP_30, 10), STEP_30);
        // A map of ones is the identity too, frame for frame.
        let ones = map(json!([{ "frame": 0, "scale": 1 }, { "frame": 40, "scale": 1.0 }]));
        assert!(ones.is_identity());
        assert_eq!(ones.sim_ns(STEP_30, 77), STEP_30 * 77);
    }

    #[test]
    fn the_scale_holds_outside_the_keys_and_ramps_between_them() {
        let m = map(json!([
            { "frame": 10, "scale": 1.0 },
            { "frame": 20, "scale": 0.25, "ease": "linear" },
            { "frame": 30, "scale": 0.25 },
            { "frame": 40, "scale": 2.0, "ease": "hold" },
        ]));
        assert_eq!(m.pre_scale_ppm(), 1_000_000);
        assert_eq!(m.frame_scale_ppm(0), 1_000_000);
        assert_eq!(
            m.frame_scale_ppm(9),
            1_000_000,
            "midpoint 9.5 is before the first key"
        );
        // Linear: frame 10's midpoint is 0.5 of 10 frames into the ramp.
        assert_eq!(m.frame_scale_ppm(10), 1_000_000 - 750_000 / 20);
        assert_eq!(m.frame_scale_ppm(19), 250_000 + 750_000 / 20);
        assert_eq!(
            m.frame_scale_ppm(25),
            250_000,
            "smooth ramp between equal scales stays put"
        );
        assert_eq!(
            m.frame_scale_ppm(39),
            250_000,
            "hold keeps the earlier scale up to the key"
        );
        assert_eq!(m.frame_scale_ppm(40), 2_000_000);
        assert_eq!(
            m.frame_scale_ppm(1_000),
            2_000_000,
            "after the last key the scale holds"
        );
        assert!(!m.is_identity());
    }

    #[test]
    fn the_integer_ramps_match_the_real_valued_curves() {
        let keys = [
            (0, 1.0, "smooth"),
            (48, 0.2, "smooth"),
            (60, 0.2, "smooth"),
            (96, 3.5, "linear"),
            (97, 0.0, "smooth"),
        ];
        let m = map(Value::Array(
            keys.iter()
                .map(|(frame, scale, ease)| json!({ "frame": frame, "scale": scale, "ease": ease }))
                .collect(),
        ));
        for j in 0..120u64 {
            let want = reference_scale(&keys, j as f64 + 0.5) * 1e6;
            let got = m.frame_scale_ppm(j) as f64;
            assert!((got - want).abs() <= 1.0, "frame {j}: {got} vs {want}");
        }
    }

    #[test]
    fn a_smooth_ramp_is_flat_at_both_keys_and_monotone_between() {
        let m = map(json!([{ "frame": 0, "scale": 1.0 }, { "frame": 100, "scale": 0.1 }]));
        let scales: Vec<u64> = (0..100).map(|j| m.frame_scale_ppm(j)).collect();
        assert!(scales.windows(2).all(|w| w[0] >= w[1]), "monotone");
        // Near the keys the per-frame change is tiny; in the middle it is the largest.
        let step = |j: usize| scales[j] - scales[j + 1];
        assert!(step(0) < 1_000 && step(98) < 1_000, "{} {}", step(0), step(98));
        assert!(step(49) > 10_000, "{}", step(49));
    }

    #[test]
    fn sim_time_is_the_sum_of_the_frame_steps() {
        let m = map(json!([
            { "frame": 5, "scale": 1.0 },
            { "frame": 29, "scale": 0.125 },
            { "frame": 61, "scale": 4.0, "ease": "linear" },
        ]));
        let mut sum = 0;
        for i in 0..200 {
            assert_eq!(m.sim_ns(STEP_30, i), sum, "frame {i}");
            sum += m.frame_step_ns(STEP_30, i);
        }
        assert_eq!(m.frame_step_ns(STEP_30, 40), {
            let ppm = m.frame_scale_ppm(40);
            (STEP_30 * ppm + 500_000) / 1_000_000
        });
        // A quarter-speed stretch covers a quarter of the simulation time, to the nanosecond.
        let quarter = map(json!([{ "frame": 0, "scale": 0.25 }]));
        assert_eq!(quarter.frame_step_ns(40_000_000, 3), 10_000_000);
        assert_eq!(quarter.sim_ns(40_000_000, 100), 1_000_000_000);
        assert_eq!(quarter.pre_step_ns(40_000_000), 10_000_000);
    }

    #[test]
    fn a_frozen_stretch_covers_no_simulation_time() {
        let m = map(json!([
            { "frame": 0, "scale": 1 },
            { "frame": 10, "scale": 0, "ease": "hold" },
            { "frame": 20, "scale": 1, "ease": "hold" },
        ]));
        assert_eq!(m.frame_step_ns(STEP_30, 9), STEP_30);
        assert_eq!(m.frame_step_ns(STEP_30, 10), 0);
        assert_eq!(m.frame_step_ns(STEP_30, 19), 0);
        assert_eq!(m.sim_ns(STEP_30, 20), 10 * STEP_30);
        assert_eq!(m.sim_ns(STEP_30, 30), 20 * STEP_30);
    }

    #[test]
    fn the_peak_step_covers_the_warm_up_and_only_the_job_frames() {
        let m = map(json!([
            { "frame": 0, "scale": 0.5 },
            { "frame": 10, "scale": 8.0, "ease": "hold" },
        ]));
        assert_eq!(
            m.peak_step_ns(1_000_000, 10),
            500_000,
            "the job ends before the jump"
        );
        assert_eq!(m.peak_step_ns(1_000_000, 11), 8_000_000);
        let start_fast = map(json!([{ "frame": 50, "scale": 3.0 }, { "frame": 60, "scale": 1.0 }]));
        assert_eq!(
            start_fast.peak_step_ns(1_000_000, 1),
            3_000_000,
            "warm-up runs at the first scale"
        );
    }

    #[test]
    fn keys_are_sorted_and_validated() {
        let m = map(json!([{ "frame": 30, "scale": 2 }, { "frame": 0, "scale": 0.5 }]));
        assert_eq!(m.keys()[0].frame, 0);
        assert_eq!(
            m.to_json(),
            json!([
                { "frame": 0, "scale": 0.5, "ease": "smooth" },
                { "frame": 30, "scale": 2.0, "ease": "smooth" },
            ])
        );
        for (bad, why) in [
            (json!("fast"), "not an array"),
            (json!([{ "scale": 1 }]), "no frame"),
            (json!([{ "frame": -1, "scale": 1 }]), "negative frame"),
            (json!([{ "frame": 1.5, "scale": 1 }]), "fractional frame"),
            (json!([{ "frame": 11, "scale": 1 }]), "frame past the limit"),
            (json!([{ "frame": 0 }]), "no scale"),
            (json!([{ "frame": 0, "scale": -0.5 }]), "negative scale"),
            (json!([{ "frame": 0, "scale": 100.5 }]), "scale above the limit"),
            (json!([{ "frame": 0, "scale": "1" }]), "string scale"),
            (
                json!([{ "frame": 0, "scale": 1, "ease": "cubic" }]),
                "unknown ease",
            ),
            (json!([{ "frame": 0, "scale": 1, "ease": 2 }]), "numeric ease"),
            (
                json!([{ "frame": 3, "scale": 1 }, { "frame": 3, "scale": 2 }]),
                "two keys on one frame",
            ),
            (json!([5]), "not an object"),
        ] {
            assert!(TimeMap::parse(Some(&bad), 10).is_err(), "{why}");
        }
        let many: Vec<Value> = (0..=MAX_TIME_KEYS as u64)
            .map(|f| json!({ "frame": f, "scale": 1 }))
            .collect();
        assert!(
            TimeMap::parse(Some(&Value::Array(many)), 1_000_000).is_err(),
            "too many keys"
        );
        for (ok, why) in [
            (json!([{ "frame": 0, "scale": 0 }]), "frozen"),
            (json!([{ "frame": 10, "scale": 100 }]), "the limits"),
            (json!([{ "frame": 0, "scale": 1, "ease": null }]), "null ease"),
        ] {
            assert!(TimeMap::parse(Some(&ok), 10).is_ok(), "{why}");
        }
        assert_eq!(scale_from_ppm(250_000), 0.25);
    }

    #[test]
    fn rounding_is_to_the_nearest_and_symmetric() {
        assert_eq!(div_round(5, 2), 3);
        assert_eq!(div_round(-5, 2), -3);
        assert_eq!(div_round(4, 3), 1);
        assert_eq!(div_round(-4, 3), -1);
        assert_eq!(scaled(3, 500_000), 2, "1.5 rounds up");
        assert_eq!(scaled(u64::MAX, 100_000_000), u64::MAX, "saturates");
    }
}
