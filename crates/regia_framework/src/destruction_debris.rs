// SPDX-License-Identifier: MPL-2.0
//! Cap on secondary rigid debris a break may spawn (ADR-3929).
//!
//! [`DebrisBudget::push`] answers how many of a requested count the caller may
//! spawn. This module does not spawn bodies. Calls are admitted in order: each
//! break may spawn at most its per-break cap across frames, and every break
//! together may spawn at most the per-frame cap. A request past either cap
//! spawns only what remains (or nothing) and the refused count is added to
//! [`DebrisBudget::dropped`]. [`DebrisBudget::reset_frame`] restores the frame
//! allowance and leaves each break's total where it is.
//!
//! The public break message is `regia_physics::destruction::DestructionBroke`
//! (a `regia_physics::destruction::BreakEvent`). `BreakEvent::seq` is a
//! suitable `break_id`. This module does not read that message: a break event
//! carries no requested debris count, and the path that spawns bodies belongs
//! to the caller. The caller passes the id and spawns only the granted count.
//!
//! A per-frame system is not installed here. A caller that drains breaks every
//! frame and may cost more than 0.1 ms owns the `tracing` span around that work.
//! The pure functions do not take one.

use std::collections::BTreeMap;

use regia_app::prelude::*;

/// `data.code` when a cap is zero, negative, fractional, out of `u32` range, or not finite.
pub const DEBRIS_BUDGET_BAD_CAP: &str = "debris_budget_bad_cap";

/// A cap was not a finite integer of at least 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebrisBudgetError {
    /// Always [`DEBRIS_BUDGET_BAD_CAP`].
    pub code: &'static str,
}

impl std::fmt::Display for DebrisBudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code)
    }
}

impl std::error::Error for DebrisBudgetError {}

fn bad_cap() -> DebrisBudgetError {
    DebrisBudgetError {
        code: DEBRIS_BUDGET_BAD_CAP,
    }
}

/// A finite whole number from 1 through `u32::MAX`.
///
/// Zero, negatives, fractions, values above `u32::MAX`, and non-finite numbers
/// (NaN, infinities) return [`DEBRIS_BUDGET_BAD_CAP`].
pub fn positive_cap(value: f64) -> Result<u32, DebrisBudgetError> {
    if !value.is_finite() {
        return Err(bad_cap());
    }
    let max = f64::from(u32::MAX);
    if value < 1.0 || value > max {
        return Err(bad_cap());
    }
    let whole = value.floor();
    if whole != value {
        return Err(bad_cap());
    }
    // `whole` is an integer in `1..=u32::MAX`, and every such integer is exact in f64.
    u32::try_from(whole as u64).map_err(|_| bad_cap())
}

/// How many secondary rigid bodies a break may still spawn.
///
/// Stored as a resource by the agent method `physics.debris_budget`. The spawn
/// path reads [`Self::push`] and creates that many bodies itself.
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub struct DebrisBudget {
    per_break: u32,
    per_frame: u32,
    frame_used: u32,
    dropped: u64,
    /// Spawned so far for each break that received a grant. Refused-only ids are absent.
    by_break: BTreeMap<u64, u32>,
}

impl DebrisBudget {
    /// A budget with both caps. See [`positive_cap`].
    ///
    /// # Errors
    ///
    /// [`DebrisBudgetError`] with [`DEBRIS_BUDGET_BAD_CAP`] when either cap is
    /// zero, negative, fractional, above `u32::MAX`, or not finite.
    pub fn try_new(per_break_cap: f64, per_frame_cap: f64) -> Result<Self, DebrisBudgetError> {
        Ok(Self {
            per_break: positive_cap(per_break_cap)?,
            per_frame: positive_cap(per_frame_cap)?,
            frame_used: 0,
            dropped: 0,
            by_break: BTreeMap::new(),
        })
    }

    /// Replace both caps. On error the budget is unchanged, counters included.
    ///
    /// # Errors
    ///
    /// [`DebrisBudgetError`] with [`DEBRIS_BUDGET_BAD_CAP`] when either cap is
    /// not a finite integer of at least 1.
    pub fn set_caps(&mut self, per_break_cap: f64, per_frame_cap: f64) -> Result<(), DebrisBudgetError> {
        let per_break = positive_cap(per_break_cap)?;
        let per_frame = positive_cap(per_frame_cap)?;
        self.per_break = per_break;
        self.per_frame = per_frame;
        Ok(())
    }

    /// How many of `requested_count` the caller may spawn for `break_id`.
    ///
    /// The grant is the minimum of the request, the room left on this break,
    /// and the room left in the frame. The remainder is added to [`Self::dropped`].
    /// A later call sees the earlier grants. `break_id` is opaque; `0` is a valid id.
    #[must_use = "spawn only the granted count"]
    pub fn push(&mut self, break_id: u64, requested_count: u32) -> u32 {
        if requested_count == 0 {
            return 0;
        }
        let already = self.by_break.get(&break_id).copied().unwrap_or(0);
        let break_room = self.per_break.saturating_sub(already);
        let frame_room = self.per_frame.saturating_sub(self.frame_used);
        let granted = requested_count.min(break_room).min(frame_room);
        let refused = u64::from(requested_count - granted);
        self.dropped = self.dropped.saturating_add(refused);
        if granted > 0 {
            self.frame_used += granted;
            self.by_break.insert(break_id, already + granted);
        }
        granted
    }

    /// Restore the frame allowance. Per-break totals and [`Self::dropped`] stay.
    pub fn reset_frame(&mut self) {
        self.frame_used = 0;
    }

    /// The per-break cap.
    #[must_use]
    pub fn per_break_cap(&self) -> u32 {
        self.per_break
    }

    /// The per-frame cap.
    #[must_use]
    pub fn per_frame_cap(&self) -> u32 {
        self.per_frame
    }

    /// Bodies granted since the last [`Self::reset_frame`].
    #[must_use]
    pub fn frame_spawned(&self) -> u32 {
        self.frame_used
    }

    /// Room left in the frame.
    #[must_use]
    pub fn frame_remaining(&self) -> u32 {
        self.per_frame.saturating_sub(self.frame_used)
    }

    /// Bodies refused so far, across frames.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Bodies granted to `break_id` so far, across frames.
    #[must_use]
    pub fn break_spawned(&self, break_id: u64) -> u32 {
        self.by_break.get(&break_id).copied().unwrap_or(0)
    }

    /// FNV-1a 64 over the caps, the frame counter, the dropped counter and each
    /// break's granted total in break-id order. Two budgets that saw the same
    /// calls have the same digest.
    #[must_use]
    pub fn digest(&self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        fnv_mix(&mut hash, &self.per_break.to_le_bytes());
        fnv_mix(&mut hash, &self.per_frame.to_le_bytes());
        fnv_mix(&mut hash, &self.frame_used.to_le_bytes());
        fnv_mix(&mut hash, &self.dropped.to_le_bytes());
        for (id, spawned) in &self.by_break {
            fnv_mix(&mut hash, &id.to_le_bytes());
            fnv_mix(&mut hash, &spawned.to_le_bytes());
        }
        hash
    }
}

fn fnv_mix(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> DebrisBudget {
        DebrisBudget::try_new(4.0, 6.0).expect("caps 4 and 6")
    }

    #[test]
    fn one_break_three_requests_spawn_3_1_0_and_drop_5() {
        let mut budget = budget();
        let break_id = 1u64;
        let spawned = [
            budget.push(break_id, 3),
            budget.push(break_id, 3),
            budget.push(break_id, 3),
        ];
        assert_eq!(spawned, [3, 1, 0]);
        assert_eq!(budget.dropped(), 5);
        assert_eq!(budget.frame_spawned(), 4);
        assert_eq!(budget.break_spawned(break_id), 4);
        assert_eq!(budget.frame_remaining(), 2);
    }

    #[test]
    fn the_next_frame_admits_a_new_break_up_to_its_cap() {
        let mut budget = budget();
        let first = 1u64;
        assert_eq!(
            [
                budget.push(first, 3),
                budget.push(first, 3),
                budget.push(first, 3)
            ],
            [3, 1, 0]
        );
        assert_eq!(budget.dropped(), 5);

        budget.reset_frame();
        assert_eq!(budget.frame_spawned(), 0);
        assert_eq!(budget.frame_remaining(), 6);
        assert_eq!(budget.per_frame_cap(), 6);
        assert_eq!(budget.break_spawned(first), 4);
        assert_eq!(budget.dropped(), 5);
        assert_eq!(budget.push(first, 1), 0, "the first break stays at its cap");
        assert_eq!(budget.dropped(), 6);

        let second = 2u64;
        assert_eq!(budget.push(second, 4), 4);
        assert_eq!(budget.break_spawned(second), 4);
        assert_eq!(budget.frame_spawned(), 4);
        assert_eq!(budget.frame_remaining(), 2);
    }

    #[test]
    fn without_a_reset_the_frame_cap_still_binds_a_new_break() {
        let mut budget = budget();
        assert_eq!(budget.push(1, 3), 3);
        assert_eq!(budget.push(1, 3), 1);
        assert_eq!(budget.push(1, 3), 0);
        assert_eq!(budget.push(2, 4), 2, "two of the six frame slots are left");
        assert_eq!(budget.frame_remaining(), 0);
        assert_eq!(budget.dropped(), 5 + 2);
    }

    #[test]
    fn distinct_breaks_share_the_frame_cap() {
        let mut budget = budget();
        assert_eq!(
            [budget.push(1, 3), budget.push(2, 3), budget.push(3, 3)],
            [3, 3, 0]
        );
        assert_eq!(budget.dropped(), 3);
        assert_eq!(budget.frame_remaining(), 0);
        budget.reset_frame();
        assert_eq!(budget.push(3, 4), 4);
        assert_eq!(budget.push(1, 3), 1);
        assert_eq!(budget.break_spawned(1), 4);
    }

    #[test]
    fn call_order_decides_which_break_receives_the_frame() {
        let mut early = budget();
        let mut late = budget();
        assert_eq!(early.push(10, 4), 4);
        assert_eq!(early.push(11, 4), 2);
        assert_eq!(late.push(11, 4), 4);
        assert_eq!(late.push(10, 4), 2);
        assert_eq!(early.break_spawned(10), 4);
        assert_eq!(late.break_spawned(10), 2);
    }

    #[test]
    fn a_zero_or_non_finite_cap_leaves_the_counters() {
        let mut budget = budget();
        assert_eq!(budget.push(1, 3), 3);
        let before = budget.clone();
        let bad = [
            0.0,
            -1.0,
            -0.0,
            0.5,
            4.5,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::from(u32::MAX) + 1.0,
        ];
        for value in bad {
            let err = budget.set_caps(value, 6.0).expect_err("per-break cap");
            assert_eq!(err.code, DEBRIS_BUDGET_BAD_CAP);
            assert_eq!(budget, before);
            let err = budget.set_caps(4.0, value).expect_err("per-frame cap");
            assert_eq!(err.code, DEBRIS_BUDGET_BAD_CAP);
            assert_eq!(budget, before);
            assert!(DebrisBudget::try_new(value, 6.0).is_err());
            assert!(DebrisBudget::try_new(4.0, value).is_err());
        }
        assert_eq!(budget.dropped(), before.dropped());
        assert_eq!(budget.digest(), before.digest());
    }

    #[test]
    fn two_runs_match() {
        fn run() -> (u64, [u32; 4], u64, u32) {
            let mut budget = budget();
            let spawned = [budget.push(1, 3), budget.push(1, 3), budget.push(1, 3)];
            let dropped = budget.dropped();
            budget.reset_frame();
            let fresh = budget.push(2, 4);
            (
                budget.digest(),
                [spawned[0], spawned[1], spawned[2], fresh],
                dropped,
                budget.frame_spawned(),
            )
        }
        assert_eq!(run(), run());
    }

    #[test]
    fn a_zero_request_changes_nothing() {
        let mut budget = budget();
        assert_eq!(budget.push(1, 0), 0);
        assert_eq!(budget.dropped(), 0);
        assert_eq!(budget.frame_spawned(), 0);
        assert_eq!(budget.break_spawned(1), 0);
    }

    #[test]
    fn the_largest_u32_cap_is_accepted() {
        let cap = f64::from(u32::MAX);
        let budget = DebrisBudget::try_new(cap, cap).expect("u32::MAX caps");
        assert_eq!(budget.per_break_cap(), u32::MAX);
        assert_eq!(budget.per_frame_cap(), u32::MAX);
    }
}
