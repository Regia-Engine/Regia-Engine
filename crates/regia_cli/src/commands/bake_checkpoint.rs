// SPDX-License-Identifier: MPL-2.0
//! The checkpoint options `regia bake-gi` and `regia bake-lightmaps` share (ADR-3095,
//! `book/agents/bake-checkpoints.md`): `--checkpoint` writes a `.bakeck` beside the output while
//! baking and continues from it when the bake is run again; `--resume <file>` continues from a
//! named checkpoint. The bake removes its checkpoint when it finishes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use regia_pt::PtError;
use regia_pt::checkpoint::{
    BakeCheckpointKind, BakeCheckpointPlan, BakeCheckpointStatus, bake_checkpoint_error_code,
    default_checkpoint_path,
};

/// The agent code of checkpoint options out of range.
pub const CHECKPOINT_SETTINGS_CODE: &str = "bake_checkpoint_settings";

/// `--checkpoint`, `--resume` and their cadence.
#[derive(Debug, Clone, Args)]
pub struct CheckpointArgs {
    /// Checkpoint the bake to `.<out>.bakeck` beside the output (ADR-3095), and when that file is
    /// there, continue from it (the bake's inputs, settings and seed must match:
    /// `bake_checkpoint_mismatch` otherwise). The checkpoint is removed when the bake finishes.
    #[arg(long)]
    pub checkpoint: bool,
    /// Continue the bake from this checkpoint (and keep checkpointing to it); a missing file is
    /// `bake_checkpoint_io`, another bake's `bake_checkpoint_mismatch`, a damaged one
    /// `bake_checkpoint_damaged`.
    #[arg(long, value_name = "BAKECK")]
    pub resume: Option<PathBuf>,
    /// Write the checkpoint every N batches (with `--checkpoint` or `--resume`).
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub checkpoint_every: u32,
    /// ... and at most every S seconds of baking (0 = every N batches).
    #[arg(long, value_name = "S", default_value_t = 60.0)]
    pub checkpoint_seconds: f64,
    /// Texels (lightmaps) or probes (the probe bake's main trace) per batch; 0 = the baker's
    /// default (65536 texels, 4096 probes).
    #[arg(long, value_name = "UNITS", default_value_t = 0)]
    pub checkpoint_batch: usize,
    /// Test hook standing for a crash: after batch K write the checkpoint and stop with
    /// `bake_interrupted`.
    #[arg(long, value_name = "K", hide = true)]
    pub checkpoint_stop_after: Option<u64>,
}

impl Default for CheckpointArgs {
    fn default() -> Self {
        Self {
            checkpoint: false,
            resume: None,
            checkpoint_every: 1,
            checkpoint_seconds: 60.0,
            checkpoint_batch: 0,
            checkpoint_stop_after: None,
        }
    }
}

impl CheckpointArgs {
    /// `--checkpoint` or `--resume` was given.
    #[must_use]
    pub fn active(&self) -> bool {
        self.checkpoint || self.resume.is_some()
    }

    /// The plan of a bake writing `out`: `None` without `--checkpoint` / `--resume`.
    ///
    /// # Errors
    ///
    /// `(code, field, message)`: [`CHECKPOINT_SETTINGS_CODE`] for an option out of range,
    /// `bake_checkpoint_io` for a `--resume` file that is not there.
    pub fn plan(&self, out: &Path) -> Result<Option<BakeCheckpointPlan>, (&'static str, String, String)> {
        if !self.active() {
            return Ok(None);
        }
        if !(self.checkpoint_seconds.is_finite() && (0.0..=86_400.0).contains(&self.checkpoint_seconds)) {
            return Err((
                CHECKPOINT_SETTINGS_CODE,
                "checkpoint_seconds".into(),
                format!(
                    "--checkpoint-seconds {} is outside 0..=86400",
                    self.checkpoint_seconds
                ),
            ));
        }
        if self.checkpoint_every == 0 {
            return Err((
                CHECKPOINT_SETTINGS_CODE,
                "checkpoint_every".into(),
                "--checkpoint-every must be at least 1".into(),
            ));
        }
        let path = match &self.resume {
            Some(file) if !file.is_file() => {
                return Err((
                    regia_pt::checkpoint::BAKE_CHECKPOINT_IO,
                    file.display().to_string(),
                    format!("--resume {}: no checkpoint there", file.display()),
                ));
            }
            Some(file) => file.clone(),
            None => default_checkpoint_path(out),
        };
        Ok(Some(BakeCheckpointPlan {
            path,
            every_batches: self.checkpoint_every,
            min_interval: Duration::from_secs_f64(self.checkpoint_seconds),
            batch_units: self.checkpoint_batch,
            resume: true,
            stop_after_batches: self.checkpoint_stop_after,
            // The command removes it once the baked output is on disk ([`finish_checkpoint`]).
            keep_on_finish: true,
        }))
    }
}

/// Prints the "found a checkpoint" line of a plan whose file exists, and returns the units it had
/// done and the seconds it had baked (the start of the progress estimate); `(0, 0.0)` without one.
pub fn announce(plan: &BakeCheckpointPlan, what: &str) -> (u64, f64) {
    let status = BakeCheckpointStatus::of_file(&plan.path);
    if !status.exists {
        return (0, 0.0);
    }
    match (status.units_done, status.units_total, status.unit) {
        (Some(done), Some(total), Some(unit)) => {
            eprintln!(
                "  {what}: found a checkpoint at {} ({done}/{total} {unit} done): continuing it",
                plan.path.display()
            );
            (done, status.work_seconds.unwrap_or(0.0))
        }
        _ => (0, 0.0),
    }
}

/// A plan for the other baker after a fallback (the GPU failed, the CPU bakes): it continues the
/// file only when that baker wrote it, else starts over and overwrites it.
#[must_use]
pub fn plan_for_kind(plan: &BakeCheckpointPlan, kind: BakeCheckpointKind) -> BakeCheckpointPlan {
    let ours = regia_asset::bake_checkpoint::read_header(&plan.path).is_ok_and(|h| h.kind == kind);
    BakeCheckpointPlan {
        resume: plan.resume && ours,
        ..plan.clone()
    }
}

/// Removes a finished bake's checkpoint after its output was written, recording that in `stats`.
/// A bake whose output write failed never gets here and keeps its checkpoint to resume from.
pub fn finish_checkpoint(plan: &BakeCheckpointPlan, stats: &mut regia_pt::checkpoint::BakeCheckpointStats) {
    stats.removed = regia_pt::checkpoint::remove_checkpoint(&plan.path);
}

/// The error of a failed bake: its checkpoint code (with the checkpoint's path) when it has one,
/// else `other(e)`.
pub fn bake_error<E>(
    e: &PtError,
    plan: Option<&BakeCheckpointPlan>,
    new: impl Fn(&'static str, Option<String>, String) -> E,
    other: impl FnOnce(&PtError) -> E,
) -> E {
    match bake_checkpoint_error_code(e) {
        Some(code) => new(code, plan.map(|p| p.path.display().to_string()), e.to_string()),
        None => other(e),
    }
}

/// ` (N left, eta S s)` for a progress line: what the editor's progress reads
/// (`bake_runner::parse_progress`); empty before a rate exists.
#[must_use]
pub fn eta_suffix(done: u64, total: u64, work_seconds: f64) -> String {
    match regia_pt::checkpoint::bake_eta_seconds(done, total, work_seconds) {
        Some(eta) => format!(
            " ({} left, eta {} s)",
            total.saturating_sub(done),
            eta.round() as u64
        ),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_follow_the_flags() {
        let out = Path::new("maps/yard.fgiv");
        assert_eq!(CheckpointArgs::default().plan(out), Ok(None));
        let on = CheckpointArgs {
            checkpoint: true,
            ..CheckpointArgs::default()
        };
        let plan = on.plan(out).expect("ok").expect("plan");
        assert_eq!(plan.path, Path::new("maps/.yard.fgiv.bakeck"));
        assert!(plan.resume);
        let missing = CheckpointArgs {
            resume: Some("/nonexistent/x.bakeck".into()),
            ..CheckpointArgs::default()
        };
        assert_eq!(missing.plan(out).map_err(|e| e.0), Err("bake_checkpoint_io"));
        let bad = CheckpointArgs {
            checkpoint_seconds: -1.0,
            ..on.clone()
        };
        assert_eq!(bad.plan(out).map_err(|e| e.0), Err(CHECKPOINT_SETTINGS_CODE));
        let zero = CheckpointArgs {
            checkpoint_every: 0,
            ..on
        };
        assert_eq!(
            zero.plan(out).map_err(|e| e.1),
            Err("checkpoint_every".to_string())
        );
    }

    #[test]
    fn the_eta_suffix_reads_back() {
        assert_eq!(eta_suffix(500, 1000, 50.0), " (500 left, eta 50 s)");
        assert_eq!(eta_suffix(0, 1000, 50.0), "");
    }
}
