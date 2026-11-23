// SPDX-License-Identifier: MPL-2.0
//! Gait patterns (ADR-8653): per-leg phase offsets and a duty factor, preset constructors, and
//! speed-selected pattern sets with hysteresis.
//!
//! A pattern runs one cycle per stride. Foot `i`'s phase is `(cycle + phase_offset[i]) mod 1`:
//! phase 0 is touchdown, the foot is in stance while its phase is below `duty_factor`, and it
//! swings from lift-off (phase `duty_factor`) to its next touchdown. The presets are textbook
//! gait diagrams (Wilson 1966 for insect tripod, ripple and metachronal wave gaits; Hildebrand
//! 1965 for footfall order and duty factor of quadruped walk, trot, pace and bound), not
//! game tuning.
//!
//! Leg order for every preset: pairs front to back, left before right (`0` front left, `1`
//! front right, ...). Quadrupeds: `0` left fore, `1` right fore, `2` left hind, `3` right
//! hind. Bipeds: `0` left, `1` right.

use regia_ecs::prelude::{Reflect, bevy_reflect};

use super::GaitError;

/// Most legs a pattern can drive. Per-leg arrays in [`super::GaitParams`] and
/// [`super::GaitState`] have this length; entries past the pattern's `legs` are ignored.
pub const MAX_LEGS: usize = 16;

/// Leg count of [`GaitPattern::tripod`], the six-leg alternating tripod.
pub const TRIPOD_LEGS: usize = 6;

/// Most patterns in a [`GaitPatternSet`].
pub const MAX_PATTERNS: usize = 4;

/// Lift-off times closer than this (in cycles) are one event: those feet lift together, and
/// at a beat boundary a swing within this of its end lands before the next group lifts.
pub(super) const EVENT_EPS: f32 = 1e-5;

/// Largest duty-factor change between neighboring patterns of a set. With phase offsets
/// blended the short way around (at most half a cycle), every foot's phase then still
/// advances relative to lift-off during a switch, so no foot reverses or lands early.
pub const MAX_DUTY_STEP: f32 = 0.5;

/// Copy `values` into a [`MAX_LEGS`] array, filling the rest with `fill`: the per-leg fields
/// of [`super::GaitParams`] for a creature with `N` legs.
#[must_use]
pub fn leg_array<T: Copy, const N: usize>(values: [T; N], fill: T) -> [T; MAX_LEGS] {
    let mut out = [fill; MAX_LEGS];
    for (slot, value) in out.iter_mut().zip(values) {
        *slot = value;
    }
    out
}

/// A gait diagram: which feet are in stance at each point of the stride cycle.
///
/// Equality compares `legs`, `duty_factor` and the used offsets (`phase_offset[..legs]`)
/// only, bit for bit, so a value in an ignored entry never makes a pattern differ, and
/// every pattern (even an invalid NaN one) equals its own copy (ADR-8736).
#[derive(Clone, Copy, Debug, Reflect)]
pub struct GaitPattern {
    /// Number of feet driven, `1..=MAX_LEGS`.
    pub legs: u8,
    /// Per-foot phase offset in cycles, in `[0, 1)`; entries past `legs` are ignored.
    pub phase_offset: [f32; MAX_LEGS],
    /// Fraction of the cycle each foot is in stance, in `(0, 1)`.
    pub duty_factor: f32,
}

impl PartialEq for GaitPattern {
    fn eq(&self, other: &Self) -> bool {
        let legs = usize::from(self.legs).min(MAX_LEGS);
        self.legs == other.legs
            && same_bits(&[self.duty_factor], &[other.duty_factor])
            && same_bits(&self.phase_offset[..legs], &other.phase_offset[..legs])
    }
}

impl GaitPattern {
    /// A pattern from explicit offsets (one per foot) and a duty factor, validated.
    pub fn new(phase_offset: &[f32], duty_factor: f32) -> Result<Self, GaitError> {
        if phase_offset.is_empty() || phase_offset.len() > MAX_LEGS {
            return Err(GaitError::Pattern);
        }
        let mut offsets = [0.0; MAX_LEGS];
        offsets[..phase_offset.len()].copy_from_slice(phase_offset);
        let pattern = Self {
            legs: phase_offset.len() as u8,
            phase_offset: offsets,
            duty_factor,
        };
        pattern.validate()?;
        Ok(pattern)
    }

    /// A pattern from each foot's lift-off time in cycles (`lift[i]` in `[0, 1)`).
    fn from_lifts(lift: &[f32], duty_factor: f32) -> Self {
        let mut offsets = [0.0; MAX_LEGS];
        for (offset, &t) in offsets.iter_mut().zip(lift) {
            // Phase at lift-off is the duty factor: cycle t + offset = duty.
            let mut value = duty_factor - t;
            if value < 0.0 {
                value += 1.0;
            }
            // An offset within EVENT_EPS of a whole cycle is touchdown at cycle 0 (f32
            // rounding of `duty - t + 1`), not a phase just under 1 (ADR-8736, review N1).
            *offset = if value >= 1.0 - EVENT_EPS { 0.0 } else { value };
        }
        Self {
            legs: lift.len() as u8,
            phase_offset: offsets,
            duty_factor,
        }
    }

    /// Check `legs` in `1..=MAX_LEGS`, every used offset finite in `[0, 1)` and the duty
    /// factor finite in `(0, 1)`; [`GaitError::Pattern`] otherwise.
    pub fn validate(&self) -> Result<(), GaitError> {
        let legs = usize::from(self.legs);
        if legs == 0
            || legs > MAX_LEGS
            || !self.duty_factor.is_finite()
            || self.duty_factor <= 0.0
            || self.duty_factor >= 1.0
            || self.phase_offset[..legs]
                .iter()
                .any(|o| !o.is_finite() || !(0.0..1.0).contains(o))
        {
            return Err(GaitError::Pattern);
        }
        Ok(())
    }

    /// Number of feet as a `usize`.
    #[must_use]
    pub fn leg_count(&self) -> usize {
        usize::from(self.legs)
    }

    /// Foot `leg`'s phase at `cycle` (both in cycles): `(cycle + phase_offset[leg]) mod 1`.
    #[must_use]
    pub fn foot_phase(&self, leg: usize, cycle: f32) -> f32 {
        let offset = self.phase_offset.get(leg).copied().unwrap_or(0.0);
        wrap_unit(cycle + offset)
    }

    /// Whether foot `leg` is in stance at `cycle`: its phase is below the duty factor.
    #[must_use]
    pub fn in_stance(&self, leg: usize, cycle: f32) -> bool {
        self.foot_phase(leg, cycle) < self.duty_factor
    }

    /// Six-leg alternating tripod (duty 0.5): feet 0, 3, 4 lift together, then 1, 2, 5.
    /// Bit for bit the stepper of ADR-8060.
    #[must_use]
    pub fn tripod() -> Self {
        Self::from_lifts(&[0.0, 0.5, 0.5, 0.0, 0.0, 0.5], 0.5)
    }

    /// Ripple gait for `legs` feet (even, `4..=MAX_LEGS`): on each side a metachronal wave
    /// runs back to front, one foot lifting every `1 / pairs` of a cycle; the right side runs
    /// half a cycle after the left. Duty factor `1 - 1 / pairs`: each side has one foot in the
    /// air at a time, and swings on the two sides overlap.
    pub fn ripple(legs: usize) -> Result<Self, GaitError> {
        if !(4..=MAX_LEGS).contains(&legs) || !legs.is_multiple_of(2) {
            return Err(GaitError::Pattern);
        }
        let pairs = legs / 2;
        let mut lift = [0.0; MAX_LEGS];
        for pair in 0..pairs {
            let t = (pairs - 1 - pair) as f32 / pairs as f32;
            lift[2 * pair] = t;
            lift[2 * pair + 1] = wrap_unit(t + 0.5);
        }
        let pattern = Self::from_lifts(&lift[..legs], 1.0 - 1.0 / pairs as f32);
        pattern.validate()?;
        Ok(pattern)
    }

    /// Metachronal wave gait for `legs` feet (even, `2..=MAX_LEGS`): one foot swings at a
    /// time, in order left side back to front, then right side back to front; duty factor
    /// `(legs - 1) / legs`, so `legs - 1` feet are always planted. The slowest, most stable
    /// gait (McGhee and Frank 1968 for quadrupeds).
    pub fn wave(legs: usize) -> Result<Self, GaitError> {
        if !(2..=MAX_LEGS).contains(&legs) || !legs.is_multiple_of(2) {
            return Err(GaitError::Pattern);
        }
        let pairs = legs / 2;
        let n = legs as f32;
        let mut lift = [0.0; MAX_LEGS];
        for pair in 0..pairs {
            let order = pairs - 1 - pair;
            lift[2 * pair] = order as f32 / n;
            lift[2 * pair + 1] = (pairs + order) as f32 / n;
        }
        let pattern = Self::from_lifts(&lift[..legs], (n - 1.0) / n);
        pattern.validate()?;
        Ok(pattern)
    }

    /// Quadruped lateral-sequence walk (Hildebrand): lift-off order left hind, left fore,
    /// right hind, right fore, a quarter cycle apart; duty factor 0.75 (three feet planted).
    #[must_use]
    pub fn quadruped_walk() -> Self {
        Self::from_lifts(&[0.25, 0.75, 0.0, 0.5], 0.75)
    }

    /// Trot: diagonal pairs (left fore with right hind, right fore with left hind) half a
    /// cycle apart; duty factor 0.5. Statically unstable: two feet on a diagonal line.
    #[must_use]
    pub fn trot() -> Self {
        Self::from_lifts(&[0.0, 0.5, 0.5, 0.0], 0.5)
    }

    /// Pace: lateral pairs (both left feet, both right feet) half a cycle apart; duty
    /// factor 0.5. Statically unstable.
    #[must_use]
    pub fn pace() -> Self {
        Self::from_lifts(&[0.0, 0.5, 0.0, 0.5], 0.5)
    }

    /// Bound: both fore feet, then both hind feet half a cycle later; duty factor 0.4, so
    /// each pair's swings overlap and there are flight phases. Statically unstable.
    #[must_use]
    pub fn bound() -> Self {
        Self::from_lifts(&[0.0, 0.0, 0.5, 0.5], 0.4)
    }

    /// Biped walk: left and right half a cycle apart; duty factor 0.6 (double support).
    #[must_use]
    pub fn biped_walk() -> Self {
        Self::from_lifts(&[0.0, 0.5], 0.6)
    }

    /// The pattern `amount` of the way from `self` to `to`: duty linear, each offset along
    /// the shorter way around the cycle.
    pub(super) fn blend(&self, to: &Self, amount: f32) -> Self {
        let mut out = *to;
        out.duty_factor = self.duty_factor + (to.duty_factor - self.duty_factor) * amount;
        for i in 0..to.leg_count() {
            let delta = offset_delta(self.phase_offset[i], to.phase_offset[i]);
            out.phase_offset[i] = wrap_unit(self.phase_offset[i] + delta * amount);
        }
        out
    }
}

/// Whether two float slices hold the same bits (NaN equals itself; `0.0` and `-0.0` differ).
fn same_bits(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
}

/// The signed offset change from `from` to `to` the shorter way around, in `(-0.5, 0.5]`.
pub(super) fn offset_delta(from: f32, to: f32) -> f32 {
    let mut delta = to - from;
    if delta > 0.5 {
        delta -= 1.0;
    } else if delta <= -0.5 {
        delta += 1.0;
    }
    delta
}

/// `value` wrapped into `[0, 1)`.
pub(super) fn wrap_unit(value: f32) -> f32 {
    let wrapped = value - value.floor();
    if wrapped >= 1.0 { 0.0 } else { wrapped }
}

/// Patterns chosen by ground speed with hysteresis (ADR-8653), slowest first.
///
/// Pattern `k + 1` takes over from pattern `k` once speed exceeds `switch_speeds[k] +
/// hysteresis`; pattern `k` comes back once speed drops below `switch_speeds[k] -
/// hysteresis`. A switch blends offsets and duty over one stride cycle.
///
/// Equality compares `count`, `hysteresis` and the used patterns and switch speeds only,
/// bit for bit; ignored entries never make two sets differ.
#[derive(Clone, Copy, Debug, Reflect)]
pub struct GaitPatternSet {
    /// Patterns in use, `1..=MAX_PATTERNS`.
    pub count: u8,
    /// The patterns, slowest first; entries past `count` are ignored. All drive the same
    /// number of legs.
    pub patterns: [GaitPattern; MAX_PATTERNS],
    /// Ground speeds in meters per second between neighboring patterns, strictly
    /// increasing and positive; entries past `count - 1` are ignored.
    pub switch_speeds: [f32; MAX_PATTERNS - 1],
    /// Half-width in meters per second of the band around each switch speed in which the
    /// current pattern holds, nonnegative.
    pub hysteresis: f32,
}

impl PartialEq for GaitPatternSet {
    fn eq(&self, other: &Self) -> bool {
        let count = usize::from(self.count).min(MAX_PATTERNS);
        let speeds = count.saturating_sub(1);
        self.count == other.count
            && same_bits(&[self.hysteresis], &[other.hysteresis])
            && self.patterns[..count] == other.patterns[..count]
            && same_bits(&self.switch_speeds[..speeds], &other.switch_speeds[..speeds])
    }
}

impl GaitPatternSet {
    /// One pattern at every speed.
    #[must_use]
    pub fn single(pattern: GaitPattern) -> Self {
        Self {
            count: 1,
            patterns: [pattern; MAX_PATTERNS],
            switch_speeds: [0.0; MAX_PATTERNS - 1],
            hysteresis: 0.0,
        }
    }

    /// The six-leg tripod at every speed (the stepper of ADR-8060).
    #[must_use]
    pub fn tripod() -> Self {
        Self::single(GaitPattern::tripod())
    }

    /// Patterns slowest first with the speeds between them (`patterns.len() - 1` of them),
    /// validated.
    pub fn new(patterns: &[GaitPattern], switch_speeds: &[f32], hysteresis: f32) -> Result<Self, GaitError> {
        let Some(first) = patterns.first() else {
            return Err(GaitError::Pattern);
        };
        if patterns.len() > MAX_PATTERNS || switch_speeds.len() + 1 != patterns.len() {
            return Err(GaitError::Pattern);
        }
        let mut set = Self::single(*first);
        set.count = patterns.len() as u8;
        set.patterns[..patterns.len()].copy_from_slice(patterns);
        set.switch_speeds[..switch_speeds.len()].copy_from_slice(switch_speeds);
        set.hysteresis = hysteresis;
        set.validate()?;
        Ok(set)
    }

    /// Check the set: `count` in range, every pattern valid with the same leg count,
    /// neighboring duty factors within [`MAX_DUTY_STEP`], switch speeds finite, positive and
    /// more than `2 * hysteresis` apart, hysteresis finite and nonnegative.
    pub fn validate(&self) -> Result<(), GaitError> {
        let count = usize::from(self.count);
        if count == 0 || count > MAX_PATTERNS {
            return Err(GaitError::Pattern);
        }
        let legs = self.patterns[0].legs;
        for (k, pattern) in self.patterns[..count].iter().enumerate() {
            pattern.validate()?;
            if pattern.legs != legs {
                return Err(GaitError::Pattern);
            }
            if k > 0 && (pattern.duty_factor - self.patterns[k - 1].duty_factor).abs() > MAX_DUTY_STEP {
                return Err(GaitError::Pattern);
            }
        }
        if !self.hysteresis.is_finite() || self.hysteresis < 0.0 {
            return Err(GaitError::Pattern);
        }
        let speeds = &self.switch_speeds[..count - 1];
        if speeds
            .iter()
            .any(|s| !s.is_finite() || *s - self.hysteresis <= 0.0)
            || speeds.windows(2).any(|w| w[1] - w[0] <= 2.0 * self.hysteresis)
        {
            return Err(GaitError::Pattern);
        }
        Ok(())
    }

    /// Number of feet the set drives.
    #[must_use]
    pub fn leg_count(&self) -> usize {
        self.patterns[0].leg_count()
    }

    /// The pattern index to use at `speed` when `current` is in use: one neighbor up or down
    /// past the hysteresis band, else `current` (clamped to the set). Validate first.
    #[must_use]
    pub fn select(&self, current: usize, speed: f32) -> usize {
        let count = usize::from(self.count).max(1);
        let current = current.min(count - 1);
        if current + 1 < count && speed > self.switch_speeds[current] + self.hysteresis {
            current + 1
        } else if current > 0 && speed < self.switch_speeds[current - 1] - self.hysteresis {
            current - 1
        } else {
            current
        }
    }

    /// The pattern index for a body starting at `speed`: the highest whose switch speed is
    /// below it.
    #[must_use]
    pub fn select_start(&self, speed: f32) -> usize {
        let count = usize::from(self.count).max(1);
        (0..count - 1)
            .take_while(|&k| speed > self.switch_speeds[k])
            .count()
    }
}

/// A pattern's lift-off schedule: feet grouped by lift-off time, the groups in cycle order,
/// and each group's beat (the interval until the next group lifts). The stepper's clock is
/// a beat index plus progress through that beat, so each beat boundary is exactly one
/// lift-off event; for the tripod a beat is the tripod period of ADR-8060.
#[derive(Clone, Copy, Debug)]
pub(super) struct Schedule {
    /// Number of lift-off groups (beats per cycle).
    pub groups: usize,
    /// Lift-off time of each group in cycles, ascending in `[0, 1)`.
    pub start: [f32; MAX_LEGS],
    /// Length of each group's beat in cycles.
    pub len: [f32; MAX_LEGS],
    /// Each foot's group.
    pub group: [u8; MAX_LEGS],
    /// Swing duration in cycles: `1 - duty`.
    pub swing: f32,
    /// Stance fraction.
    pub duty: f32,
}

impl Schedule {
    /// Build the schedule of a valid pattern; no allocation.
    pub fn of(pattern: &GaitPattern) -> Self {
        let legs = pattern.leg_count().clamp(1, MAX_LEGS);
        let duty = pattern.duty_factor;
        let mut lift = [0.0_f32; MAX_LEGS];
        for (t, &offset) in lift.iter_mut().zip(&pattern.phase_offset[..legs]) {
            let mut value = duty - offset;
            if value < 0.0 {
                value += 1.0;
            }
            *t = if value >= 1.0 { 0.0 } else { value };
        }
        // Stable insertion sort of the feet by lift-off time.
        let mut order = [0_usize; MAX_LEGS];
        for (i, slot) in order.iter_mut().enumerate() {
            *slot = i;
        }
        for k in 1..legs {
            let mut m = k;
            while m > 0 && lift[order[m - 1]] > lift[order[m]] {
                order.swap(m - 1, m);
                m -= 1;
            }
        }
        let mut start = [0.0_f32; MAX_LEGS];
        let mut group = [0_u8; MAX_LEGS];
        let mut groups = 0_usize;
        for &leg in &order[..legs] {
            let t = lift[leg];
            if groups == 0 || t - start[groups - 1] > EVENT_EPS {
                start[groups] = t;
                groups += 1;
            }
            group[leg] = (groups - 1) as u8;
        }
        // A lift-off just before the cycle wraps belongs to the first group.
        if groups > 1 && start[0] + 1.0 - start[groups - 1] <= EVENT_EPS {
            let last = (groups - 1) as u8;
            for g in &mut group[..legs] {
                if *g == last {
                    *g = 0;
                }
            }
            groups -= 1;
        }
        let mut len = [0.0_f32; MAX_LEGS];
        for (j, length) in len.iter_mut().enumerate().take(groups) {
            *length = if j + 1 < groups {
                start[j + 1] - start[j]
            } else {
                1.0 - start[j] + start[0]
            };
        }
        Self {
            groups,
            start,
            len,
            group,
            swing: 1.0 - duty,
            duty,
        }
    }

    /// Cycles since foot `leg`'s group lifted off, at `phase` through beat `beat`.
    pub fn elapsed(&self, leg: usize, beat: usize, phase: f32) -> f32 {
        let lifted = usize::from(self.group[leg]);
        let mut base = self.start[beat] - self.start[lifted];
        if base < 0.0 {
            base += 1.0;
        }
        base + phase * self.len[beat]
    }

    /// Swing progress `[0, 1]` of foot `leg` at the clock, if the pattern has it swinging.
    pub fn swing_progress(&self, leg: usize, beat: usize, phase: f32) -> Option<f32> {
        let elapsed = self.elapsed(leg, beat, phase);
        (elapsed < self.swing).then(|| (elapsed / self.swing).min(1.0))
    }

    /// Foot `leg`'s phase (0 touchdown, `duty` lift-off) at the clock. A phase within
    /// [`EVENT_EPS`] of a whole cycle is touchdown, 0: the foot landed at that boundary
    /// (ADR-8736, f32 rounding of the beat clock).
    pub fn foot_phase(&self, leg: usize, beat: usize, phase: f32) -> f32 {
        let value = wrap_unit(self.duty + self.elapsed(leg, beat, phase));
        if value >= 1.0 - EVENT_EPS { 0.0 } else { value }
    }

    /// Cycle position of the clock, in `[0, 1)`.
    pub fn cycle(&self, beat: usize, phase: f32) -> f32 {
        wrap_unit(self.start[beat] + phase * self.len[beat])
    }

    /// The clock (beat, progress in `[0, 1)`) at cycle position `cycle`.
    pub fn locate(&self, cycle: f32) -> (usize, f32) {
        let cycle = wrap_unit(cycle);
        let mut beat = self.groups - 1;
        for j in 0..self.groups {
            if self.start[j] <= cycle {
                beat = j;
            }
        }
        let mut into = cycle - self.start[beat];
        if into < 0.0 {
            into += 1.0;
        }
        let phase = (into / self.len[beat]).max(0.0);
        (beat, if phase >= 1.0 { 1.0 - f32::EPSILON } else { phase })
    }
}
