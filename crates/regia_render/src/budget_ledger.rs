// SPDX-License-Identifier: MPL-2.0
//! Named integer counters for the `specs/27` § 2.4 create gate (ADR-3943).
//!
//! [`crate::HitchReport`] keeps the frame-time rows (percentiles, frames over 50 ms, frames over
//! 2 × p50). This ledger stores a documented counter name as an `i64` and compares that sum to an
//! integer cap. A test fails the create gate with fixed deltas. Nothing here reads a clock.
//!
//! # Counter names
//!
//! | Name | Default cap after the warm frame index |
//! |---|---|
//! | [`PIPELINE_CREATES`] | 0 |
//! | [`BIND_GROUP_CREATES`] | 0 |
//! | [`ALLOCATIONS`] | none, until [`BudgetLedger::set_cap`] |
//! | [`IDLE_FRAMES`] | none, until a caller installs a cap |
//! | [`PROBE_PREPARES`] | none, until a caller installs a cap |
//!
//! [`IDLE_FRAMES`] and [`PROBE_PREPARES`] are reserved for counts later lanes already measure.
//! This module does not fill them. Any other name starts uncapped the same way.
//!
//! `warm` on [`BudgetLedger::misses`] is the first measured frame, the same index
//! [`crate::HitchRecorder::report`] takes. Deltas on earlier frames are the warm-up.

use std::collections::BTreeMap;

use crate::HitchFrame;

/// Pipeline creations on the frame path. Cap 0 after the warm frame index (`specs/27` § 2.4).
pub const PIPELINE_CREATES: &str = "pipeline_creates";

/// Bind-group creations on the frame path. Cap 0 after the warm frame index (`specs/27` § 2.4).
pub const BIND_GROUP_CREATES: &str = "bind_group_creates";

/// A caller's allocation count (heap or device). Uncapped until [`BudgetLedger::set_cap`].
pub const ALLOCATIONS: &str = "allocations";

/// Idle-frame count a later lane records. Uncapped until a caller installs a cap.
pub const IDLE_FRAMES: &str = "idle_frames";

/// Probe-prepare count a later lane records. Uncapped until a caller installs a cap.
pub const PROBE_PREPARES: &str = "probe_prepares";

/// Names that start at cap 0, matching the § 2.4 create rows.
pub const CREATE_COUNTERS: &[&str] = &[PIPELINE_CREATES, BIND_GROUP_CREATES];

/// A map from a counter name to an `i64`, plus the integer cap of each capped name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetLedger {
    /// Summed deltas per frame index, then per name.
    frames: BTreeMap<u64, BTreeMap<String, i64>>,
    /// Caps. A name absent from this map is uncapped and never a miss.
    caps: BTreeMap<String, i64>,
}

impl Default for BudgetLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl BudgetLedger {
    /// An empty ledger. The create counters are capped at 0. Every other name is uncapped.
    #[must_use]
    pub fn new() -> Self {
        let mut caps = BTreeMap::new();
        for name in CREATE_COUNTERS {
            caps.insert((*name).to_owned(), 0);
        }
        Self {
            frames: BTreeMap::new(),
            caps,
        }
    }

    /// Adds `delta` to counter `name` on frame `frame`.
    ///
    /// Several calls for the same frame and name add. A `delta` of 0 leaves the counter
    /// unchanged. An empty `name` is ignored. The sum saturates at [`i64::MIN`] and [`i64::MAX`].
    pub fn record(&mut self, frame: u64, name: &str, delta: i64) {
        if name.is_empty() || delta == 0 {
            return;
        }
        let empty = {
            let counters = self.frames.entry(frame).or_default();
            let total = counters.get(name).copied().unwrap_or(0).saturating_add(delta);
            if total == 0 {
                counters.remove(name);
            } else if let Some(slot) = counters.get_mut(name) {
                *slot = total;
            } else {
                counters.insert(name.to_owned(), total);
            }
            counters.is_empty()
        };
        if empty {
            self.frames.remove(&frame);
        }
    }

    /// Records the pipeline creates and bind-group creates of one hitch frame.
    ///
    /// The two counts are read through `HitchFrame`'s crate-private create read. Allocation,
    /// idle-frame and probe-prepare counters stay at whatever the caller recorded.
    pub fn record_hitch_frame(&mut self, frame: u64, hitch: &HitchFrame) {
        let (pipelines, bind_groups) = hitch.create_deltas();
        self.record(frame, PIPELINE_CREATES, pipelines);
        self.record(frame, BIND_GROUP_CREATES, bind_groups);
    }

    /// Records each hitch frame at its index in `frames` (the first sample is frame 0).
    pub fn record_hitch_frames(&mut self, frames: &[HitchFrame]) {
        for (index, hitch) in frames.iter().enumerate() {
            self.record_hitch_frame(index as u64, hitch);
        }
    }

    /// Installs an integer cap for `name`, including a create counter whose default is 0.
    ///
    /// An empty `name` is ignored. A later [`Self::clear_cap`] removes the cap again.
    pub fn set_cap(&mut self, name: &str, cap: i64) {
        if name.is_empty() {
            return;
        }
        self.caps.insert(name.to_owned(), cap);
    }

    /// Drops the cap on `name`. The name is then never a miss, whatever its value.
    pub fn clear_cap(&mut self, name: &str) {
        self.caps.remove(name);
    }

    /// The installed cap, or `None` when `name` is uncapped.
    #[must_use]
    pub fn cap(&self, name: &str) -> Option<i64> {
        self.caps.get(name).copied()
    }

    /// The summed deltas of `name` on frames at or after `warm`.
    #[must_use]
    pub fn value(&self, name: &str, warm: u64) -> i64 {
        let mut total = 0i64;
        for (_, counters) in self.frames.range(warm..) {
            if let Some(delta) = counters.get(name) {
                total = total.saturating_add(*delta);
            }
        }
        total
    }

    /// Names whose value on frames at or after `warm` is greater than the cap.
    ///
    /// `warm` is the first measured frame. Uncapped names are omitted. Equal to the cap is not a
    /// miss. The vector is sorted by name.
    #[must_use]
    pub fn misses(&self, warm: u64) -> Vec<String> {
        let mut names = Vec::new();
        for (name, cap) in &self.caps {
            if self.value(name, warm) > *cap {
                names.push(name.clone());
            }
        }
        names
    }
}
