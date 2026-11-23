// SPDX-License-Identifier: MPL-2.0
//! The one foot-lock state machine of the crate (`docs/specs/17-animation.md` § 6): a planted foot
//! is pinned in world space where it touched down, pinned again where it is when the caller says
//! the pin drifted too far (a teleport, a jump to another clip), and released over a time once the
//! stance ends.
//!
//! [`PlantLock`] owns only those transitions. Each caller keeps its own rules for *when* a foot is
//! in stance and *how far* is too far, because they differ by design:
//!
//! * [`super::FootPlanter`] decides stance from a contact weight or the clip foot's lift and speed,
//!   measures drift horizontally, and releases with [`PlantRelease::Follow`] (the held offset from
//!   the moving clip foot fades, so the foot never catches up faster than it swings).
//! * The motion-matching [`crate::motion_matching::FootLocker`] takes the cooked contact labels,
//!   measures drift in 3D and releases with [`PlantRelease::Fixed`] (the host blends from the
//!   animated foot toward a fixed pin by the weight).
//! * The crowd's exact-tier lock takes its gait contact table, measures drift against a fraction of
//!   the hip height and releases over a whole number of fixed steps (`dt` 1, release time in
//!   steps) with [`PlantRelease::Fixed`].
//!
//! Each of the three produces the same bits it did before it shared this machine (their tests
//! pin that).
//!
//! The machine advances only by the supplied `dt`; it uses no transcendental function, so the same
//! inputs give the same bits on every target that rounds `f32` arithmetic the same way.
//!
//! Provenance: pinning planted feet and releasing the constraint over a short blend follows the
//! footplant constraints of Kovar, Schreiner and Gleicher, "Footskate Cleanup for Motion Capture
//! Editing", SCA 2002 (public paper). The state machine, its teleport re-pin and both release
//! modes are original code (ADR-8308).

use regia_core::math::Vec3;

/// How a released pin hands the foot back to the animation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlantRelease {
    /// The pin stays where it was; the caller blends from the animated foot toward
    /// [`PlantLock::pin`] by [`PlantLock::weight`].
    #[default]
    Fixed,
    /// The pin rides with the animated foot: its horizontal offset from the foot when the stance
    /// ended fades with the weight ([`PlantLock::pin`] is updated every frame of the release; its
    /// height stays the touchdown height).
    Follow,
}

/// What [`PlantLock::update`] did this frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlantLockStep {
    /// The pin was set where the foot is: a touchdown, or a drift the caller reported.
    pub repinned: bool,
    /// The stance ended this frame (the foot was down last frame and is not now).
    pub lifted: bool,
}

/// One foot's lock state (module docs). `Default` is a foot in the air with no pin.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlantLock {
    pin: Vec3,
    down: bool,
    pinned: bool,
    weight: f32,
    release_offset: Vec3,
}

impl PlantLock {
    /// The pin, world space (only meaningful while [`Self::pinned`]).
    pub fn pin(&self) -> Vec3 {
        self.pin
    }

    /// Whether the foot was in stance after the latest update.
    pub fn is_down(&self) -> bool {
        self.down
    }

    /// Whether a pin holds the foot (in stance, or still releasing with a weight above 0).
    pub fn pinned(&self) -> bool {
        self.pinned
    }

    /// How much the foot follows the pin, `[0, 1]`: 1 in stance, falling to 0 over the release.
    pub fn weight(&self) -> f32 {
        self.weight
    }

    /// The foot is in stance and `distance` (the caller's measure from [`Self::pin`] to the
    /// animated foot) exceeds `max_drift`: pass the result as the `repin` argument of
    /// [`Self::update`]. Never true for a foot that is not in stance.
    pub fn drifted(&self, distance: f32, max_drift: f32) -> bool {
        self.down && distance > max_drift
    }

    /// Advance one frame.
    ///
    /// * `down`: the caller's stance decision for this frame.
    /// * `repin`: pin again where the foot is even though the stance continues (from
    ///   [`Self::drifted`] or the caller's own drift test).
    /// * `now`: the animated foot, world space (the pin's new value on a touchdown or re-pin).
    /// * `dt` and `release_time`: the release lowers the weight by `dt / release_time`; a
    ///   `release_time` of 0 (or less) releases at once. Pass sanitized values: they are used as
    ///   they are.
    ///
    /// In stance the weight is 1. When the stance ends, a [`PlantRelease::Follow`] lock records
    /// the pin's horizontal offset from `now`; every release frame then lowers the weight and, for
    /// `Follow`, moves the pin to `now` plus that offset times the weight.
    pub fn update(
        &mut self,
        down: bool,
        repin: bool,
        now: Vec3,
        dt: f32,
        release_time: f32,
        release: PlantRelease,
    ) -> PlantLockStep {
        let mut step = PlantLockStep::default();
        if down {
            // A touchdown always pins where the foot landed, also while an older pin is still
            // releasing: keeping that pin would pull the foot back to its last footfall.
            if !self.down || repin {
                self.pin = now;
                step.repinned = true;
            }
            self.pinned = true;
            self.weight = 1.0;
        } else if self.pinned {
            if self.down {
                step.lifted = true;
                self.release_offset = Vec3::new(self.pin.x - now.x, 0.0, self.pin.z - now.z);
            }
            self.weight = if release_time > 0.0 {
                (self.weight - dt / release_time).max(0.0)
            } else {
                0.0
            };
            self.pinned = self.weight > 0.0;
        } else {
            self.pinned = false;
            self.weight = 0.0;
        }
        self.down = down;
        if release == PlantRelease::Follow && self.weight > 0.0 && !down {
            let anchor = now + self.release_offset * self.weight;
            self.pin = Vec3::new(anchor.x, self.pin.y, anchor.z);
        }
        step
    }

    /// Move the pin horizontally by `delta` (its height is kept): the ground under the foot
    /// moved and carried the planted foot with it (a moving platform). Nothing else changes.
    pub fn carry(&mut self, delta: Vec3) {
        self.pin = Vec3::new(self.pin.x + delta.x, self.pin.y, self.pin.z + delta.z);
    }

    /// Drop the pin but keep tracking the stance (a lock switched off): the weight is 0 and
    /// nothing is pinned; a later [`Self::update`] in the same stance does not count as a
    /// touchdown.
    pub fn unlock(&mut self, down: bool) {
        self.pinned = false;
        self.weight = 0.0;
        self.down = down;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 0.05;

    #[test]
    fn plant_lock_unit_cases() {
        // Touchdown: pins where the foot is, weight 1.
        let mut lock = PlantLock::default();
        let a = Vec3::new(1.0, 0.1, 2.0);
        let step = lock.update(true, false, a, DT, 0.2, PlantRelease::Fixed);
        assert!(step.repinned && !step.lifted);
        assert_eq!(
            (lock.pin(), lock.weight(), lock.pinned(), lock.is_down()),
            (a, 1.0, true, true)
        );

        // Held: the animated foot slides, the pin stays.
        let slid = a + Vec3::X * 0.05;
        let step = lock.update(
            true,
            lock.drifted(slid.distance(a), 0.3),
            slid,
            DT,
            0.2,
            PlantRelease::Fixed,
        );
        assert!(!step.repinned);
        assert_eq!(lock.pin(), a);

        // Teleport: a drift past the limit re-pins where the foot is, the stance continues.
        let far = a + Vec3::X * 2.0;
        assert!(lock.drifted(far.distance(lock.pin()), 0.3));
        let step = lock.update(true, true, far, DT, 0.2, PlantRelease::Fixed);
        assert!(step.repinned);
        assert_eq!((lock.pin(), lock.weight()), (far, 1.0));
        // Not in stance: never drifted.
        assert!(!PlantLock::default().drifted(10.0, 0.3));

        // Release with a nonzero time, fixed pin: the weight falls by dt / time.
        let step = lock.update(false, false, far + Vec3::Z * 0.1, DT, 0.2, PlantRelease::Fixed);
        assert!(step.lifted && !step.repinned);
        assert_eq!(lock.weight(), 1.0 - DT / 0.2);
        assert_eq!(lock.pin(), far);
        assert!(lock.pinned());
        for _ in 0..3 {
            lock.update(false, false, far, DT, 0.2, PlantRelease::Fixed);
        }
        assert_eq!(lock.weight(), 0.0);
        assert!(!lock.pinned());
        // Idle in the air: stays released.
        let step = lock.update(false, false, far, DT, 0.2, PlantRelease::Fixed);
        assert_eq!(step, PlantLockStep::default());
        assert_eq!(lock.weight(), 0.0);

        // Release with zero time: the pin drops in the same frame the stance ends.
        let mut lock = PlantLock::default();
        lock.update(true, false, a, DT, 0.0, PlantRelease::Fixed);
        let step = lock.update(false, false, a, DT, 0.0, PlantRelease::Fixed);
        assert!(step.lifted);
        assert_eq!((lock.weight(), lock.pinned()), (0.0, false));

        // Follow release: the pin rides with the foot by the fading offset (height kept).
        let mut lock = PlantLock::default();
        lock.update(true, false, a, DT, 0.2, PlantRelease::Follow);
        let swing = a + Vec3::new(0.2, 0.05, 0.0);
        lock.update(false, false, swing, DT, 0.2, PlantRelease::Follow);
        let w = 1.0 - DT / 0.2;
        let anchor = swing + Vec3::new(a.x - swing.x, 0.0, a.z - swing.z) * w;
        assert_eq!(lock.pin(), Vec3::new(anchor.x, a.y, anchor.z));

        // A touchdown during a release pins where the foot landed, not at the old pin.
        let landed = a + Vec3::Z * 0.1;
        let step = lock.update(true, false, landed, DT, 0.2, PlantRelease::Follow);
        assert!(step.repinned);
        assert_eq!((lock.pin(), lock.weight()), (landed, 1.0));

        // Unlock keeps the stance: the next down frame is not a touchdown.
        lock.unlock(true);
        assert_eq!((lock.weight(), lock.pinned(), lock.is_down()), (0.0, false, true));
        let step = lock.update(true, false, landed + Vec3::X, DT, 0.2, PlantRelease::Follow);
        assert!(!step.repinned);
    }

    #[test]
    fn identical_inputs_give_identical_bits() {
        let run = || {
            let mut lock = PlantLock::default();
            let mut out = Vec::new();
            for i in 0..40 {
                let t = i as f32 * DT;
                let foot = Vec3::new(t * 1.3, 0.1 + 0.05 * (i % 7) as f32, t * 0.2);
                let down = (i / 6) % 2 == 0;
                let repin = lock.drifted(foot.distance(lock.pin()), 0.3);
                lock.update(down, repin, foot, DT, 0.15, PlantRelease::Follow);
                out.push((lock.pin().to_array().map(f32::to_bits), lock.weight().to_bits()));
            }
            out
        };
        assert_eq!(run(), run());
    }
}
