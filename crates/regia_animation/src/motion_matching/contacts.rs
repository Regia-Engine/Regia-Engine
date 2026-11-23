// SPDX-License-Identifier: MPL-2.0
//! Foot contact labels, gait phase and the foot-lock output (`docs/specs/52-motion-matching.md`
//! § 2.2a, ADR-2858).
//!
//! At build (cook) time every foot of every frame gets a **contact label**: the foot is planted
//! when it is low and slow, with hysteresis so a label does not chatter at the thresholds. A
//! foot enters contact when its height above its lowest point in the clip is at most
//! [`ContactSettings::height_on`] *and* its speed is at most [`ContactSettings::speed_on`]; it
//! leaves contact when the height exceeds [`ContactSettings::height_off`] *or* the speed exceeds
//! [`ContactSettings::speed_off`]. The labels are features of the group
//! [`super::FeatureGroup::FootContact`] (1 float per foot, 0 or 1).
//!
//! The **phase** of a foot is its position in its own step cycle, measured from one touchdown
//! (a frame whose label turns on) to the next: `phase = (frame − touchdown) / cycle length`, in
//! `[0, 1)`. It is written as the unit vector `(cos 2π·phase, sin 2π·phase)` so the wrap from
//! the end of one cycle to the start of the next is continuous (group
//! [`super::FeatureGroup::FootPhase`], 2 floats per foot). A looping clip measures its cycles
//! around the loop; a clip that does not loop extends its first and last cycle outward. A foot
//! with no defined cycle (no touchdown in a looping clip, fewer than two in one that does not)
//! gets `(0, 0)`: equally far from every phase.
//!
//! At runtime [`FootLocker`] turns the playing frame's contact labels into [`FootLock`]
//! targets: a foot that touches down is pinned where it landed in the world, and the pin is
//! released over [`FootLockSettings::release_time`] when the label turns off. The host blends its
//! foot-IK target toward the pin by [`FootLock::weight`].

// Sources: the idea of a gait phase measured from foot contact times (here phase 0 at a
// touchdown, advancing linearly to the next touchdown) is credited, from memory and not re-read
// for this code, to D. Holden, T. Komura and J. Saito, "Phase-Functioned Neural Networks for
// Character Control", ACM Trans. Graph. 36(4), 2017 (nothing learned here). Deriving the contact
// labels from height and speed thresholds with hysteresis, the cos/sin encoding of the phase as a search
// feature, the undefined-phase zero vector and the foot-lock pin and release are original
// design.
// Original code; nothing ported.

use std::f32::consts::TAU;
use std::ops::Range;

use regia_core::math::{Vec2, Vec3};
use regia_ecs::prelude::*;

use super::{FeatureGroup, FeatureSchema, MotionMatchError};
use crate::foot_ik::lock::{PlantLock, PlantRelease};
use crate::num::{det_atan2, det_sin_cos};

/// How contact labels (and the phase) are derived from the foot features at build time.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ContactSettings {
    /// A foot may enter contact at or below this height above its lowest point in the clip,
    /// meters (default 0.02).
    pub height_on: f32,
    /// A foot in contact leaves it above this height, meters (default 0.05; at least
    /// `height_on`).
    pub height_off: f32,
    /// A foot may enter contact at or below this speed, meters per second (default 0.15).
    pub speed_on: f32,
    /// A foot in contact leaves it above this speed, meters per second (default 0.35; at least
    /// `speed_on`).
    pub speed_off: f32,
    /// Also write the per-foot phase group ([`FeatureGroup::FootPhase`]).
    pub phase: bool,
}

impl Default for ContactSettings {
    fn default() -> Self {
        Self {
            height_on: 0.02,
            height_off: 0.05,
            speed_on: 0.15,
            speed_off: 0.35,
            phase: true,
        }
    }
}

impl ContactSettings {
    /// Check the thresholds: each finite and not negative, every `*_off` at least its `*_on`.
    ///
    /// # Errors
    ///
    /// [`MotionMatchError::BadContacts`] naming the bad threshold.
    pub fn validate(&self) -> Result<(), MotionMatchError> {
        for (name, value) in [
            ("height_on", self.height_on),
            ("height_off", self.height_off),
            ("speed_on", self.speed_on),
            ("speed_off", self.speed_off),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(MotionMatchError::BadContacts(format!(
                    "{name} {value} must be finite and not negative"
                )));
            }
        }
        if self.height_off < self.height_on {
            return Err(MotionMatchError::BadContacts(format!(
                "height_off {} is below height_on {}",
                self.height_off, self.height_on
            )));
        }
        if self.speed_off < self.speed_on {
            return Err(MotionMatchError::BadContacts(format!(
                "speed_off {} is below speed_on {}",
                self.speed_off, self.speed_on
            )));
        }
        Ok(())
    }
}

/// The contact labels of one foot over consecutive frames: `heights[i]` above the foot's lowest
/// point and `speeds[i]`, with hysteresis. A looping clip runs one warm-up lap first, so the
/// label of frame 0 continues the end of the loop.
pub fn label_contacts(
    heights: &[f32],
    speeds: &[f32],
    looping: bool,
    settings: &ContactSettings,
) -> Vec<bool> {
    let n = heights.len().min(speeds.len());
    let next = |planted: bool, h: f32, v: f32| {
        if planted {
            !(h > settings.height_off || v > settings.speed_off)
        } else {
            h <= settings.height_on && v <= settings.speed_on
        }
    };
    let samples = || heights.iter().zip(speeds).take(n);
    let mut planted = false;
    if looping {
        for (&h, &v) in samples() {
            planted = next(planted, h, v);
        }
    }
    let mut out = Vec::with_capacity(n);
    for (&h, &v) in samples() {
        planted = next(planted, h, v);
        out.push(planted);
    }
    out
}

/// Touchdown frames: where a label turns on (cyclic for a looping clip; frame 0 of a clip that
/// does not loop is never a touchdown).
fn touchdowns(labels: &[bool], looping: bool) -> Vec<usize> {
    let n = labels.len();
    (0..n)
        .filter(|&i| {
            let before = if i == 0 {
                if looping { labels[n - 1] } else { true }
            } else {
                labels[i - 1]
            };
            labels[i] && !before
        })
        .collect()
}

/// The phase in `[0, 1)` of every frame from the contact labels, `None` where the cycle is not
/// defined (no touchdown in a looping clip, fewer than two in one that does not).
pub fn phases_from_contacts(labels: &[bool], looping: bool) -> Vec<Option<f32>> {
    let n = labels.len();
    let downs = touchdowns(labels, looping);
    let mut out = vec![None; n];
    if n == 0 {
        return out;
    }
    if looping {
        if downs.is_empty() {
            return out;
        }
        for (i, slot) in out.iter_mut().enumerate() {
            // The last touchdown at or before `i`, cyclically, and the one after it.
            let k = downs.partition_point(|&d| d <= i);
            let (start, end) = if k == 0 {
                (downs[downs.len() - 1] as isize - n as isize, downs[0] as isize)
            } else if k == downs.len() {
                (downs[k - 1] as isize, downs[0] as isize + n as isize)
            } else {
                (downs[k - 1] as isize, downs[k] as isize)
            };
            let length = (end - start).max(1) as f32;
            *slot = Some((((i as isize - start) as f32) / length).rem_euclid(1.0));
        }
        return out;
    }
    if downs.len() < 2 {
        return out;
    }
    for (i, slot) in out.iter_mut().enumerate() {
        let k = downs.partition_point(|&d| d <= i);
        let (start, length) = if k == 0 {
            (downs[0], downs[1] - downs[0])
        } else if k == downs.len() {
            (downs[k - 1], downs[k - 1] - downs[k - 2])
        } else {
            (downs[k - 1], downs[k] - downs[k - 1])
        };
        let offset = i as f32 - start as f32;
        *slot = Some((offset / length.max(1) as f32).rem_euclid(1.0));
    }
    out
}

/// The phase feature of a phase: `(cos 2π·phase, sin 2π·phase)`, or `(0, 0)` when undefined.
pub fn phase_feature(phase: Option<f32>) -> Vec2 {
    match phase {
        Some(p) if p.is_finite() => {
            let (sin, cos) = det_sin_cos(TAU * p);
            Vec2::new(cos, sin)
        }
        _ => Vec2::ZERO,
    }
}

/// The phase in `[0, 1)` a phase feature encodes, `None` for the undefined `(0, 0)`.
pub fn phase_of_feature(feature: Vec2) -> Option<f32> {
    if !feature.is_finite() || feature.length_squared() < 0.25 {
        return None;
    }
    Some((det_atan2(feature.y, feature.x) / TAU).rem_euclid(1.0))
}

/// The shortest distance between two phases around the cycle, in `[0, 0.5]`.
pub fn phase_distance(a: f32, b: f32) -> f32 {
    let d = (a - b).rem_euclid(1.0);
    d.min(1.0 - d)
}

/// Fill the contact (and phase) dimensions of one clip range's raw rows from their foot
/// position and velocity features (`rows` is the range's slice of the raw array).
pub(crate) fn label_range(schema: &FeatureSchema, rows: &mut [f32], looping: bool) {
    let Some(settings) = schema.contacts else {
        return;
    };
    let dims = schema.dimensions();
    if dims == 0 || rows.is_empty() {
        return;
    }
    let frames = rows.len() / dims;
    let positions = schema.group_range(FeatureGroup::FootPosition);
    let velocities = schema.group_range(FeatureGroup::FootVelocity);
    let contacts = schema.group_range(FeatureGroup::FootContact);
    let phases = schema.group_range(FeatureGroup::FootPhase);
    for foot in 0..schema.feet.len() {
        let row = |frame: usize| frame * dims;
        let height_at = |frame: usize| rows[row(frame) + positions.start + 3 * foot + 1];
        let lowest = (0..frames).map(height_at).fold(f32::INFINITY, f32::min);
        let heights: Vec<f32> = (0..frames).map(|f| height_at(f) - lowest).collect();
        let speeds: Vec<f32> = (0..frames)
            .map(|f| {
                let v = row(f) + velocities.start + 3 * foot;
                Vec3::new(rows[v], rows[v + 1], rows[v + 2]).length()
            })
            .collect();
        let labels = label_contacts(&heights, &speeds, looping, &settings);
        for (frame, &planted) in labels.iter().enumerate() {
            if let Some(slot) = rows.get_mut(row(frame) + contacts.start + foot) {
                *slot = if planted { 1.0 } else { 0.0 };
            }
        }
        if !phases.is_empty() {
            for (frame, phase) in phases_from_contacts(&labels, looping).into_iter().enumerate() {
                let feature = phase_feature(phase);
                let at = row(frame) + phases.start + 2 * foot;
                if let Some(slot) = rows.get_mut(at..at + 2) {
                    slot.copy_from_slice(&feature.to_array());
                }
            }
        }
    }
}

/// Contact label of every foot in a raw row (`true` = planted); empty without contacts.
pub(crate) fn row_contacts(schema: &FeatureSchema, raw: &[f32]) -> Vec<bool> {
    let range: Range<usize> = schema.group_range(FeatureGroup::FootContact);
    raw.get(range)
        .map_or_else(Vec::new, |values| values.iter().map(|&v| v > 0.5).collect())
}

/// Phase of every foot in a raw row; empty without the phase group.
pub(crate) fn row_phases(schema: &FeatureSchema, raw: &[f32]) -> Vec<Option<f32>> {
    let range = schema.group_range(FeatureGroup::FootPhase);
    raw.get(range).map_or_else(Vec::new, |values| {
        values
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&[c, s]| phase_of_feature(Vec2::new(c, s)))
            .collect()
    })
}

/// Settings of a [`FootLocker`].
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct FootLockSettings {
    /// Seconds over which a released pin's weight falls from 1 to 0 (default 0.15; 0 releases at
    /// once).
    pub release_time: f32,
    /// A planted foot whose animated position drifts farther than this from its pin, meters, is
    /// pinned again where it is (after a jump to another clip, so the leg is never stretched;
    /// default 0.3).
    pub max_drift: f32,
}

impl Default for FootLockSettings {
    fn default() -> Self {
        Self {
            release_time: 0.15,
            max_drift: 0.3,
        }
    }
}

/// One foot's lock target for foot IK.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FootLock {
    /// The playing frame labels the foot planted.
    pub planted: bool,
    /// Where the foot is pinned, world space (the animated foot position when no pin is held).
    pub target: Vec3,
    /// How much the IK target should follow `target` instead of the animated foot, `[0, 1]`.
    pub weight: f32,
}

impl FootLock {
    /// The foot-IK target: the animated position `animated` blended toward the pin by the
    /// weight.
    pub fn ik_target(&self, animated: Vec3) -> Vec3 {
        let weight = if self.weight.is_finite() {
            self.weight.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if weight <= 0.0 {
            return animated;
        }
        animated.lerp(self.target, weight)
    }
}

/// Turns per-frame contact labels into foot-lock targets (pin on touchdown, hold while planted,
/// release over [`FootLockSettings::release_time`]). Deterministic: it advances only by the
/// supplied `dt`. A thin wrapper over the crate's one lock state machine,
/// [`crate::foot_ik::lock::PlantLock`], with a fixed pin on release and drift measured in 3D.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FootLocker {
    /// The settings.
    pub settings: FootLockSettings,
    locks: Vec<FootLock>,
    plants: Vec<PlantLock>,
}

impl FootLocker {
    /// A locker with `settings` and no foot pinned yet.
    pub fn new(settings: FootLockSettings) -> Self {
        Self {
            settings,
            ..Self::default()
        }
    }

    /// The latest lock per foot.
    pub fn locks(&self) -> &[FootLock] {
        &self.locks
    }

    /// Forget every pin (after a teleport).
    pub fn reset(&mut self) {
        self.locks.clear();
        self.plants.clear();
    }

    /// Advance by `dt` seconds with this frame's labels (`planted`, one per foot) and animated
    /// world-space foot positions (`animated`, one per foot). Feet missing from either list are
    /// unplanted. Non-finite positions never become pins.
    pub fn update(&mut self, planted: &[bool], animated: &[Vec3], dt: f32) -> &[FootLock] {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        let feet = animated.len();
        self.locks.resize(feet, FootLock::default());
        self.plants.resize(feet, PlantLock::default());
        let release = if self.settings.release_time.is_finite() {
            self.settings.release_time.max(0.0)
        } else {
            0.0
        };
        let drift = if self.settings.max_drift.is_finite() {
            self.settings.max_drift.max(0.0)
        } else {
            f32::INFINITY
        };
        let feet_state = self.locks.iter_mut().zip(self.plants.iter_mut()).zip(animated);
        for (foot, ((lock, plant), &now)) in feet_state.enumerate() {
            let down = planted.get(foot).copied().unwrap_or(false) && now.is_finite();
            let repin = plant.drifted(plant.pin().distance(now), drift);
            lock.planted = down;
            plant.update(down, repin, now, dt, release, PlantRelease::Fixed);
            lock.weight = plant.weight();
            if plant.weight() > 0.0 {
                lock.target = plant.pin();
            } else if now.is_finite() {
                lock.target = now;
            }
        }
        &self.locks
    }
}

/// The foot-lock output of an entity's motion matching: every frame, [`run_foot_locks`] reads
/// the playing frame's contact labels and the [`super::MotionMatchPlayer`]'s output pose (feet
/// in world space through the entity's `GlobalTransform`, or in character space without one) and
/// updates [`MotionMatchFootLocks::locks`], one per schema foot, for the host's foot IK
/// ([`FootLock::ik_target`]). Needs a schema with [`FeatureSchema::contacts`]; without, every
/// foot reads unplanted.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct MotionMatchFootLocks {
    /// The pin state.
    pub locker: FootLocker,
    /// Contact bits of the latest frame. Reused across frames.
    planted: Vec<bool>,
    /// Animated foot positions of the latest frame, world space when a transform is present.
    animated: Vec<Vec3>,
}

impl MotionMatchFootLocks {
    /// The latest lock per schema foot.
    pub fn locks(&self) -> &[FootLock] {
        self.locker.locks()
    }
}

/// Animated foot positions of a player's output pose, character space (schema foot order).
pub fn player_feet(database: &super::MotionDatabase, player: &super::MotionMatchPlayer) -> Vec<Vec3> {
    let globals = player.skeleton.globals(player.pose(), None);
    database
        .schema()
        .feet
        .iter()
        .map(|&j| globals.get(j as usize).map_or(Vec3::NAN, |m| m.w_axis.truncate()))
        .collect()
}

/// Update every [`MotionMatchFootLocks`] from its entity's playing frame and player pose. The
/// [`super::MotionMatchPlugin`] runs it after the players.
#[tracing::instrument(target = "regia_animation", skip_all)]
pub fn run_foot_locks(
    time: Res<regia_app::Time>,
    mut locks: Query<(
        &super::MotionMatching,
        &super::MotionMatchPlayer,
        &mut MotionMatchFootLocks,
        Option<&regia_scene::GlobalTransform>,
    )>,
) {
    let dt = time.delta_secs();
    for (matching, player, mut lock, world) in &mut locks {
        let database = &matching.database;
        let feet_n = database.schema().feet.len();
        lock.animated.clear();
        if player.output_feet().len() == feet_n {
            lock.animated.extend_from_slice(player.output_feet());
        } else {
            lock.animated.extend(player_feet(database, player));
        }
        if let Some(world) = world {
            for foot in &mut lock.animated {
                *foot = world.0.transform_point3(*foot);
            }
        }
        lock.planted.clear();
        if let Some(playback) = matching.matcher.playback() {
            fill_contact_bits(database, playback.frame, &mut lock.planted);
        }
        let MotionMatchFootLocks {
            locker,
            planted,
            animated,
        } = &mut *lock;
        locker.update(planted, animated, dt);
    }
}

/// Contact label of every foot at `frame`, written into `out` (`true` = planted).
///
/// Empty when the schema has no contacts or `frame` is past the end. Same labels as
/// `MotionDatabase::contacts`.
pub(crate) fn fill_contact_bits(database: &super::MotionDatabase, frame: usize, out: &mut Vec<bool>) {
    out.clear();
    let Some(raw) = database.raw_features(frame) else {
        return;
    };
    let range = database.schema().group_range(super::FeatureGroup::FootContact);
    if let Some(values) = raw.get(range) {
        out.extend(values.iter().map(|&v| v > 0.5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hysteresis_keeps_a_label_between_the_thresholds() {
        let s = ContactSettings::default();
        // Height 0.03 is between on (0.02) and off (0.05): it keeps whatever the label was.
        let heights = [0.0, 0.03, 0.03, 0.06, 0.03, 0.01];
        let speeds = [0.0; 6];
        let labels = label_contacts(&heights, &speeds, false, &s);
        assert_eq!(labels, vec![true, true, true, false, false, true]);
        // Speed alone also releases.
        let labels = label_contacts(&[0.0; 3], &[0.0, 0.3, 0.4], false, &s);
        assert_eq!(labels, vec![true, true, false]);
    }

    #[test]
    fn a_looping_label_continues_across_the_seam() {
        let s = ContactSettings::default();
        // Planted at the end of the loop, height 0.03 at frame 0: still planted (warm-up lap).
        let heights = [0.03, 0.06, 0.06, 0.0];
        let labels = label_contacts(&heights, &[0.0; 4], true, &s);
        assert_eq!(labels, vec![true, false, false, true]);
        let open = label_contacts(&heights, &[0.0; 4], false, &s);
        assert_eq!(open, vec![false, false, false, true]);
    }

    #[test]
    fn phase_runs_from_touchdown_to_touchdown_and_wraps() {
        // Touchdowns at 2 and 6 of 8 (looping): cycles of 4 frames.
        let labels = [false, false, true, true, false, false, true, true];
        let phases = phases_from_contacts(&labels, true);
        let expect = [0.5, 0.75, 0.0, 0.25, 0.5, 0.75, 0.0, 0.25];
        for (got, want) in phases.iter().zip(expect) {
            assert!((got.expect("defined") - want).abs() < 1e-6, "{got:?} vs {want}");
        }
        // Not looping: the first and last cycles extend outward.
        let phases = phases_from_contacts(&labels, false);
        assert!((phases[0].expect("defined") - 0.5).abs() < 1e-6);
        assert!((phases[7].expect("defined") - 0.25).abs() < 1e-6);
        // Undefined without touchdowns.
        assert!(phases_from_contacts(&[true; 5], true).iter().all(Option::is_none));
        assert!(
            phases_from_contacts(&[false, true, true], false)
                .iter()
                .all(Option::is_none)
        );
    }

    #[test]
    fn the_phase_feature_is_continuous_across_the_wrap() {
        let a = phase_feature(Some(0.999));
        let b = phase_feature(Some(0.0));
        assert!(a.distance(b) < 0.01, "{a} {b}");
        assert_eq!(phase_feature(None), Vec2::ZERO);
        assert!((phase_of_feature(phase_feature(Some(0.3))).expect("phase") - 0.3).abs() < 1e-5);
        assert_eq!(phase_of_feature(Vec2::ZERO), None);
        assert!((phase_distance(0.95, 0.05) - 0.1).abs() < 1e-6);
    }

    /// ADR-4477 / audit animation1-determinism H1: phase bytes must not depend on the host libm.
    #[test]
    fn phase_feature_uses_the_portable_trig_oracle() {
        let phase = f32::from_bits(0x3d8e_110a);
        let (sin, cos) = crate::num::det_sin_cos(std::f32::consts::TAU * phase);
        let feature = phase_feature(Some(phase));
        assert_eq!(feature.x.to_bits(), cos.to_bits());
        assert_eq!(feature.y.to_bits(), sin.to_bits());
        assert_eq!(feature.y.to_bits(), 0x3ed8_28b0);
    }

    #[test]
    fn bad_thresholds_are_refused() {
        let ok = ContactSettings::default();
        assert!(ok.validate().is_ok());
        for bad in [
            ContactSettings {
                height_on: f32::NAN,
                ..ok
            },
            ContactSettings { speed_on: -1.0, ..ok },
            ContactSettings {
                height_off: 0.01,
                ..ok
            },
            ContactSettings { speed_off: 0.1, ..ok },
        ] {
            assert!(
                matches!(bad.validate(), Err(MotionMatchError::BadContacts(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_touchdown_during_the_release_pins_where_it_landed() {
        let mut locker = FootLocker::new(FootLockSettings {
            release_time: 0.2,
            max_drift: 0.3,
        });
        let first = Vec3::new(1.0, 0.0, 2.0);
        locker.update(&[true], &[first], 0.05);
        // Lifted, and still releasing the old pin.
        let lifted = first + Vec3::Z * 0.05;
        let locks = locker.update(&[false], &[lifted], 0.05).to_vec();
        assert!(locks[0].weight > 0.0 && locks[0].target == first, "{locks:?}");
        // The next footfall is inside max_drift of the old pin: the new position still wins.
        let landed = first + Vec3::Z * 0.1;
        let locks = locker.update(&[true], &[landed], 0.05).to_vec();
        assert_eq!(locks[0].target, landed);
        assert_eq!(locks[0].weight, 1.0);
        assert_eq!(locks[0].ik_target(landed), landed);
        // And that new pin holds from here on.
        let locks = locker.update(&[true], &[landed + Vec3::X * 0.05], 0.05).to_vec();
        assert_eq!(locks[0].target, landed);
    }

    #[test]
    fn the_locker_pins_holds_and_releases() {
        let mut locker = FootLocker::new(FootLockSettings {
            release_time: 0.1,
            max_drift: 0.3,
        });
        let p = Vec3::new(1.0, 0.0, 2.0);
        let locks = locker.update(&[true], &[p], 0.05).to_vec();
        assert_eq!(locks[0].target, p);
        // The animated foot slides 5 cm while planted: the pin holds.
        let locks = locker.update(&[true], &[p + Vec3::X * 0.05], 0.05).to_vec();
        assert_eq!(locks[0].target, p);
        assert_eq!(locks[0].ik_target(p + Vec3::X * 0.05), p);
        // Released: the weight falls over 0.1 s.
        let locks = locker.update(&[false], &[p + Vec3::X * 0.2], 0.05).to_vec();
        assert!((locks[0].weight - 0.5).abs() < 1e-6);
        let locks = locker.update(&[false], &[p + Vec3::X * 0.3], 0.05).to_vec();
        assert_eq!(locks[0].weight, 0.0);
        assert_eq!(locks[0].target, p + Vec3::X * 0.3);
        // A drift past max_drift re-pins.
        locker.update(&[true], &[p], 0.05);
        let locks = locker.update(&[true], &[p + Vec3::X], 0.05).to_vec();
        assert_eq!(locks[0].target, p + Vec3::X);
        // A non-finite foot never pins; a dt spike releases cleanly.
        let locks = locker.update(&[true], &[Vec3::NAN], 10.0).to_vec();
        assert!(!locks[0].planted);
        assert!(locks[0].weight == 0.0);
    }
}
