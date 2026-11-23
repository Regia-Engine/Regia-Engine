// SPDX-License-Identifier: MPL-2.0
//! Action-value injection (`docs/specs/11` § 7 and `docs/specs/23-agent-native.md` § 3.5):
//! the [`InputInjection`] resource through which *action-level* input can be driven
//! programmatically, without touching raw devices.
//!
//! Two users share one mechanism:
//!
//! * the **agent protocol** (`play.inject_input` with `actions: [{action, value, at_frame,
//!   duration}]`): [`InputInjection::schedule`] a value for a future frame, with a duration in
//!   frames — deterministic, replayable test input;
//! * **virtual controls** (`regia_ui`'s `VirtualStick` / `VirtualButton`, specs 11 § 7):
//!   [`InputInjection::hold`] a value for as long as the widget is touched
//!   ([`InputInjection::release`] on finger-up).
//!
//! Injected values are applied after the device bindings are combined: a scheduled (agent) entry
//! overrides a held (virtual) entry, which overrides device input for that action. Trigger and
//! phase evaluation then runs exactly as for physical input, so injected input exercises the
//! same gameplay paths (`Started`/`Ongoing`/`Triggered` edges and all).

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::math::Vec3;

/// One scheduled injection (agent protocol): `value` applied to `action` from
/// `start_frame` for `duration_frames` frames (inclusive start, exclusive end).
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduledInjection {
    /// The action to write (must be declared in the [`crate::InputConfig`]).
    pub action: String,
    /// The value the action takes; a zero value counts as "released".
    pub value: Vec3,
    /// The first frame (as reported by `Time::frame_count`) the value applies.
    pub start_frame: u64,
    /// How many frames the value applies for. [`InputInjection::schedule`] stores at least 1.
    pub duration_frames: u64,
}

/// The most scheduled windows kept pending; older ones are dropped first.
pub const MAX_SCHEDULED: usize = 4096;

impl ScheduledInjection {
    /// Whether `frame` falls inside the window. A window whose end would pass `u64::MAX` runs
    /// through `u64::MAX` instead of overflowing.
    fn covers(&self, frame: u64) -> bool {
        frame >= self.start_frame
            && match self.start_frame.checked_add(self.duration_frames) {
                Some(end) => frame < end,
                None => true,
            }
    }

    /// Whether the window has ended by `frame` (never, when it runs past `u64::MAX`).
    fn expired_at(&self, frame: u64) -> bool {
        self.start_frame
            .checked_add(self.duration_frames)
            .is_some_and(|end| frame >= end)
    }
}

/// Programmatically driven action values (see the module docs). Written from gameplay, the UI
/// lane's virtual controls and the agent protocol; consumed by [`crate::evaluate_actions`].
#[derive(Resource, Debug, Default)]
pub struct InputInjection {
    scheduled: Vec<ScheduledInjection>,
    held: BTreeMap<String, Vec3>,
}

impl InputInjection {
    /// Schedules an injection for a future (or current) frame. A `duration_frames` of 0 is stored
    /// as 1, so the start frame is always covered. Scheduled entries win over
    /// `InputInjection::held` and device input.
    pub fn schedule(
        &mut self,
        action: impl Into<String>,
        value: Vec3,
        start_frame: u64,
        duration_frames: u64,
    ) {
        let action = action.into();
        let injection = ScheduledInjection {
            action,
            value,
            start_frame,
            duration_frames: duration_frames.max(1),
        };
        // Re-scheduling the same action from the same frame replaces the earlier window.
        if let Some(existing) = self
            .scheduled
            .iter_mut()
            .find(|i| i.action == injection.action && i.start_frame == injection.start_frame)
        {
            *existing = injection;
            return;
        }
        if self.scheduled.len() >= MAX_SCHEDULED {
            // Bounded queue: drop the oldest window rather than growing without limit.
            self.scheduled.remove(0);
        }
        self.scheduled.push(injection);
    }

    /// Holds an action's value from this frame on, until [`InputInjection::release`] (or a
    /// scheduled entry overrides it). Used by virtual controls.
    pub fn hold(&mut self, action: impl Into<String>, value: Vec3) {
        self.held.insert(action.into(), value);
    }

    /// Stops holding an action (the held value is removed; the action reads devices again).
    pub fn release(&mut self, action: &str) {
        self.held.remove(action);
    }

    /// The value injected for `action` this frame, if any: a scheduled entry first, then a held
    /// one. `Some(Vec3::ZERO)` means "actively driven to zero".
    pub fn value_for(&self, action: &str, frame: u64) -> Option<Vec3> {
        // Several windows may cover one frame. The one that started latest wins, so a
        // pulse scheduled during a longer window is visible for its overlap and the
        // earlier window resumes after it. The same start frame is already one entry
        // (`schedule` replaces it).
        if let Some(value) = self
            .scheduled
            .iter()
            .filter(|injection| injection.action == action && injection.covers(frame))
            .max_by_key(|injection| injection.start_frame)
            .map(|injection| injection.value)
        {
            return Some(value);
        }
        self.held.get(action).copied()
    }

    /// Drops expired scheduled entries; called by [`crate::evaluate_actions`].
    pub fn prune(&mut self, frame: u64) {
        self.scheduled.retain(|i| !i.expired_at(frame));
    }

    /// How many scheduled entries are pending (agent diagnostics).
    pub fn scheduled_count(&self) -> usize {
        self.scheduled.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflowing_window_does_not_panic() {
        let mut injection = InputInjection::default();
        injection.schedule("Jump", Vec3::X, u64::MAX, 1);
        assert_eq!(injection.value_for("Jump", u64::MAX - 1), None);
        injection.prune(0);
        assert_eq!(injection.scheduled_count(), 1);
    }

    #[test]
    fn scheduled_windows_apply_then_expire() {
        let mut injection = InputInjection::default();
        injection.schedule("Jump", Vec3::X, 5, 2);
        assert_eq!(injection.value_for("Jump", 4), None, "not yet started");
        assert_eq!(injection.value_for("Jump", 5), Some(Vec3::X));
        assert_eq!(injection.value_for("Jump", 6), Some(Vec3::X));
        assert_eq!(injection.value_for("Jump", 7), None, "exclusive end");
        injection.prune(7);
        assert_eq!(injection.scheduled_count(), 0, "pruned once expired");
    }

    #[test]
    fn huge_durations_saturate_instead_of_overflowing() {
        let mut injection = InputInjection::default();
        injection.schedule("Jump", Vec3::X, u64::MAX - 1, u64::MAX);
        assert_eq!(injection.value_for("Jump", u64::MAX - 1), Some(Vec3::X));
        injection.prune(5);
        assert_eq!(injection.scheduled_count(), 1);
    }

    #[test]
    fn rescheduling_the_same_start_replaces_and_the_queue_is_bounded() {
        let mut injection = InputInjection::default();
        injection.schedule("Jump", Vec3::X, 5, 2);
        injection.schedule("Jump", Vec3::Y, 5, 3);
        assert_eq!(injection.scheduled_count(), 1);
        assert_eq!(injection.value_for("Jump", 7), Some(Vec3::Y));
        for i in 0..(MAX_SCHEDULED as u64 + 10) {
            injection.schedule("Fire", Vec3::X, 1000 + i, 1);
        }
        assert_eq!(injection.scheduled_count(), MAX_SCHEDULED);
        assert_eq!(injection.value_for("Fire", 1000), None, "oldest dropped");
        assert_eq!(
            injection.value_for("Fire", 1000 + MAX_SCHEDULED as u64 + 9),
            Some(Vec3::X)
        );
    }

    #[test]
    fn scheduled_wins_over_held_and_release_stops_holding() {
        let mut injection = InputInjection::default();
        injection.hold("Move", Vec3::new(1.0, 0.0, 0.0));
        assert_eq!(injection.value_for("Move", 0), Some(Vec3::new(1.0, 0.0, 0.0)));
        injection.schedule("Move", Vec3::new(0.0, 1.0, 0.0), 0, 1);
        assert_eq!(
            injection.value_for("Move", 0),
            Some(Vec3::new(0.0, 1.0, 0.0)),
            "scheduled overrides held"
        );
        assert_eq!(
            injection.value_for("Move", 1),
            Some(Vec3::new(1.0, 0.0, 0.0)),
            "held continues after the scheduled window"
        );
        injection.release("Move");
        assert_eq!(injection.value_for("Move", 2), None);
    }

    #[test]
    fn overflowing_window_covers_the_last_frame_without_panicking() {
        let mut injection = InputInjection::default();
        injection.schedule("Fire", Vec3::X, u64::MAX, 1);
        injection.schedule("Jump", Vec3::Y, u64::MAX - 1, u64::MAX);
        assert_eq!(injection.value_for("Fire", u64::MAX), Some(Vec3::X));
        assert_eq!(injection.value_for("Fire", u64::MAX - 1), None);
        assert_eq!(injection.value_for("Jump", u64::MAX), Some(Vec3::Y));
        injection.prune(u64::MAX);
        assert_eq!(
            injection.scheduled_count(),
            2,
            "windows reaching u64::MAX never expire"
        );
    }

    #[test]
    fn a_window_ending_past_u64_max_does_not_overflow() {
        let mut injection = InputInjection::default();
        injection.schedule("Jump", Vec3::X, u64::MAX - 1, u64::MAX);
        assert_eq!(injection.value_for("Jump", u64::MAX - 1), Some(Vec3::X));
        injection.prune(u64::MAX - 1);
        assert_eq!(injection.scheduled_count(), 1, "still running");
    }

    #[test]
    fn durations_are_at_least_one_frame() {
        let mut injection = InputInjection::default();
        injection.schedule("Fire", Vec3::X, 3, 0);
        assert_eq!(injection.value_for("Fire", 3), Some(Vec3::X));
        assert_eq!(injection.value_for("Fire", 4), None);
    }

    #[test]
    fn a_later_overlapping_window_wins_then_the_earlier_one_resumes() {
        let mut injection = InputInjection::default();
        injection.schedule("Jump", Vec3::X, 0, 10);
        injection.schedule("Jump", Vec3::Y, 5, 2);
        assert_eq!(injection.value_for("Jump", 4), Some(Vec3::X));
        assert_eq!(
            injection.value_for("Jump", 5),
            Some(Vec3::Y),
            "the later window covers frames 5 and 6"
        );
        assert_eq!(injection.value_for("Jump", 6), Some(Vec3::Y));
        assert_eq!(
            injection.value_for("Jump", 7),
            Some(Vec3::X),
            "the earlier window resumes after the later one ends"
        );
        assert_eq!(injection.value_for("Jump", 9), Some(Vec3::X));
        assert_eq!(injection.value_for("Jump", 10), None);
    }
}
