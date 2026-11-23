// SPDX-License-Identifier: MPL-2.0
//! Save and cook triggers of the asset validators (spec 32 § 4.5, ADR-1668).
//!
//! Both are pure state machines so a test (or an agent) can drive them without a clock or a disk:
//! [`SaveValidationGate`] batches saved paths by editor tick and defers while a hold guard lives, and
//! [`cook_gate`] decides what a finished report means for the cook or package step.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::validation_rules::CookGate;
use crate::validators::ValidationReport;

/// Batches the paths saved in the editor into one validation run per tick.
///
/// Call [`SaveValidationGate::note_saved`] when a save lands and [`SaveValidationGate::tick`] once per
/// editor tick; `tick` hands back everything saved since the previous call as one sorted batch (to run
/// with `Trigger::Save` and `Scope::Changed`). While any [`SaveHold`] lives, `tick` returns nothing and
/// the paths keep queuing; they run on the first tick after the last hold drops.
#[derive(Debug)]
pub struct SaveValidationGate {
    enabled: bool,
    queue: BTreeSet<String>,
    holds: Arc<AtomicUsize>,
}

impl Default for SaveValidationGate {
    fn default() -> Self {
        Self::new(true)
    }
}

impl SaveValidationGate {
    /// A gate; `enabled` is the `validate_on_save` setting.
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            queue: BTreeSet::new(),
            holds: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Turn the gate on or off (turning it off also drops what was queued).
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.queue.clear();
        }
    }

    /// Whether saves queue a run.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// A save of `path` landed.
    pub fn note_saved(&mut self, path: impl Into<String>) {
        if self.enabled {
            self.queue.insert(path.into());
        }
    }

    /// Defer runs until the returned guard (and every other guard) is dropped. Batch tools such as
    /// importers hold one.
    #[must_use]
    pub fn hold(&self) -> SaveHold {
        self.holds.fetch_add(1, Ordering::SeqCst);
        SaveHold {
            holds: Arc::clone(&self.holds),
        }
    }

    /// How many guards are alive.
    #[must_use]
    pub fn holds(&self) -> usize {
        self.holds.load(Ordering::SeqCst)
    }

    /// How many paths wait.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// One editor tick: the sorted batch to validate now, or `None` (nothing queued, or held).
    pub fn tick(&mut self) -> Option<Vec<String>> {
        if self.queue.is_empty() || self.holds() > 0 {
            return None;
        }
        Some(std::mem::take(&mut self.queue).into_iter().collect())
    }
}

/// A guard from [`SaveValidationGate::hold`]; dropping it releases the hold.
#[derive(Debug)]
pub struct SaveHold {
    holds: Arc<AtomicUsize>,
}

impl Drop for SaveHold {
    fn drop(&mut self) {
        self.holds.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What a cook or package step does with a validation report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CookGateOutcome {
    /// Validation was off; nothing ran.
    Skipped,
    /// The step goes on (clean, or findings under `Warn`).
    Continue {
        /// Assets that failed (reported, not blocking).
        failed: usize,
    },
    /// The step stops (`Fail` mode and the report failed). The cook output already written stays.
    Stop {
        /// Assets that failed.
        failed: usize,
    },
}

/// Decide the cook or package step's fate from `mode` and the report (`None` when validation did
/// not run). `strict` makes warnings fail too.
#[must_use]
pub fn cook_gate(mode: CookGate, report: Option<&ValidationReport>, strict: bool) -> CookGateOutcome {
    if !mode.runs() {
        return CookGateOutcome::Skipped;
    }
    let Some(report) = report else {
        return CookGateOutcome::Skipped;
    };
    let failed = report.summary.failed;
    if mode == CookGate::Fail && report.exit_failed(strict) {
        CookGateOutcome::Stop { failed }
    } else {
        CookGateOutcome::Continue { failed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_graph::{AssetEntry, AssetKind, AssetSet};
    use crate::validators::{ValidationConfig, ValidatorRegistry};

    #[test]
    fn three_saves_in_one_tick_make_one_run() {
        let mut gate = SaveValidationGate::new(true);
        gate.note_saved("b.png");
        gate.note_saved("a.png");
        gate.note_saved("b.png");
        assert_eq!(gate.pending(), 2);
        assert_eq!(gate.tick(), Some(vec!["a.png".to_owned(), "b.png".to_owned()]));
        assert_eq!(gate.tick(), None);
    }

    #[test]
    fn a_live_hold_defers_and_nested_holds_need_all_dropped() {
        let mut gate = SaveValidationGate::new(true);
        let h1 = gate.hold();
        let h2 = gate.hold();
        gate.note_saved("a");
        assert_eq!(gate.tick(), None);
        drop(h1);
        gate.note_saved("b");
        assert_eq!(gate.tick(), None, "one hold is still alive");
        drop(h2);
        assert_eq!(gate.holds(), 0);
        assert_eq!(gate.tick(), Some(vec!["a".to_owned(), "b".to_owned()]));
    }

    #[test]
    fn disabled_gate_queues_nothing() {
        let mut gate = SaveValidationGate::new(false);
        gate.note_saved("a");
        assert_eq!(gate.tick(), None);
        gate.set_enabled(true);
        gate.note_saved("a");
        gate.set_enabled(false);
        assert_eq!(gate.pending(), 0);
    }

    fn failing_report() -> ValidationReport {
        let mut set = AssetSet::new();
        set.push(AssetEntry::new("Bad Name.png", AssetKind::Texture));
        ValidatorRegistry::with_builtins(&ValidationConfig::default()).run(&set)
    }

    #[test]
    fn cook_gate_modes() {
        let bad = failing_report();
        assert!(bad.summary.failed > 0);
        assert_eq!(
            cook_gate(CookGate::Off, Some(&bad), false),
            CookGateOutcome::Skipped
        );
        assert_eq!(cook_gate(CookGate::Warn, None, false), CookGateOutcome::Skipped);
        assert!(matches!(
            cook_gate(CookGate::Warn, Some(&bad), false),
            CookGateOutcome::Continue { failed } if failed > 0
        ));
        assert!(matches!(
            cook_gate(CookGate::Fail, Some(&bad), false),
            CookGateOutcome::Stop { failed } if failed > 0
        ));
        let clean = ValidatorRegistry::with_builtins(&ValidationConfig::default()).run(&AssetSet::new());
        assert_eq!(
            cook_gate(CookGate::Fail, Some(&clean), true),
            CookGateOutcome::Continue { failed: 0 }
        );
    }
}
