// SPDX-License-Identifier: MPL-2.0
//! The motion-matching debug view's data (`docs/specs/52-motion-matching.md` § 2.11): the cost
//! of a frame split per feature group, and a candidate report for the latest search that says
//! why each of the cheapest frames was or was not chosen.

use super::{FLAG_PASS_THROUGH, FeatureGroup, MotionDatabase, MotionMatcher, SearchMethod, clean_bias};

/// One frame's cost against a query, split per feature group.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CostParts {
    /// The canonical distance (the search's cost before biases).
    pub total: f32,
    /// The sum of squared normalized differences over each group's slice, indexed by
    /// [`FeatureGroup::index`]; they add up to `total` within rounding.
    pub groups: [f32; FeatureGroup::COUNT],
    /// The frame's landing bias.
    pub bias: f32,
}

/// Why a candidate was or was not chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateStatus {
    /// The latest search's winner.
    Best,
    /// The playing frame (its continuation).
    Continuing,
    /// Eligible but more expensive than the best.
    Candidate,
    /// In a pass-through span: played through, never a target.
    PassThrough,
    /// Chosen as a jump target within the lock-out window.
    LockedOut,
    /// Within the exclusion window around the playing frame.
    NearPlaying,
    /// Rejected by the tag filter.
    Filtered,
}

impl CandidateStatus {
    /// The snake_case name (`best`, `continuing`, `candidate`, `pass_through`, `locked_out`,
    /// `near_playing`, `filtered`).
    pub fn name(self) -> &'static str {
        match self {
            CandidateStatus::Best => "best",
            CandidateStatus::Continuing => "continuing",
            CandidateStatus::Candidate => "candidate",
            CandidateStatus::PassThrough => "pass_through",
            CandidateStatus::LockedOut => "locked_out",
            CandidateStatus::NearPlaying => "near_playing",
            CandidateStatus::Filtered => "filtered",
        }
    }
}

/// One row of a [`CandidateReport`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CandidateRow {
    /// Database frame.
    pub frame: usize,
    /// Clip index.
    pub clip: usize,
    /// Clip time, seconds.
    pub time: f32,
    /// The frame belongs to a mirrored range.
    pub mirrored: bool,
    /// What the jump rule compares: distance + landing bias + loop-clip bias + transition cost
    /// for a jump target; distance + stay bias for the continuation.
    pub total: f32,
    /// The distance split per group, and the landing bias.
    pub parts: CostParts,
    /// The transition cost a jump to this frame pays (0 for the continuation).
    pub transition: f32,
    /// The loop-clip bias (frames of looping clips), or the stay bias for the continuation.
    pub extra_bias: f32,
    /// Why it was or was not chosen.
    pub status: CandidateStatus,
}

/// The cheapest indexed frames of the latest search and the continuation, each with its cost
/// split and status. Built on request ([`MotionMatcher::candidate_report`]) because it costs one
/// extra top-k search.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CandidateReport {
    /// Up to `k` frames, cheapest by distance plus landing bias first (ties by frame index).
    /// Unindexed frames never appear.
    pub rows: Vec<CandidateRow>,
    /// The playing frame, when something played and its clip had not ended.
    pub continuation: Option<CandidateRow>,
    /// The method the latest search ran.
    pub method_used: Option<SearchMethod>,
}

impl MotionMatcher {
    /// The candidate report for the latest search: the `k` (1 to 32) cheapest indexed frames
    /// against the latest query, plus the continuation. `None` before the first search or when
    /// the database changed shape since.
    pub fn candidate_report(&self, database: &MotionDatabase, k: usize) -> Option<CandidateReport> {
        let report = self.last?;
        let query = &self.last_query;
        if query.len() != database.dimensions() {
            return None;
        }
        let settings = self.settings.sanitized();
        let loop_bias = clean_bias(settings.loop_clip_bias);
        let best = report.best.map(|b| b.frame);
        let playing = self.playing.map(|p| p.frame);
        let unindexed = |row: usize| {
            database
                .meta
                .get(row)
                .is_some_and(|m| m.flags & super::FLAG_UNINDEXED != 0)
        };
        let bias = |row: usize| database.meta.get(row).map_or(0.0, |m| m.bias);
        let (nearest, _) = database.tree.nearest_k_biased(
            query,
            unindexed,
            bias,
            database.min_bias.min(0.0),
            k.clamp(1, 32),
        );
        let row_for = |frame: usize, continuing: bool| -> Option<CandidateRow> {
            let at = database.frame(frame)?;
            let parts = database.cost_parts(query, frame)?;
            let looping = database.clips().get(at.clip).is_some_and(|c| c.looping);
            let meta = database.frame_meta(frame).unwrap_or_default();
            let (total, transition, extra_bias) = if continuing {
                (parts.total + settings.stay_bias, 0.0, settings.stay_bias)
            } else {
                let extra = if looping { loop_bias } else { 0.0 };
                (
                    parts.total + parts.bias + extra + settings.transition_cost,
                    settings.transition_cost,
                    extra,
                )
            };
            let status = if continuing {
                CandidateStatus::Continuing
            } else if Some(frame) == best {
                CandidateStatus::Best
            } else if meta.flags & FLAG_PASS_THROUGH != 0 {
                CandidateStatus::PassThrough
            } else if self.last_exclude.contains(frame) {
                CandidateStatus::NearPlaying
            } else if self.recent_targets.iter().any(|&(f, _)| f == frame) {
                CandidateStatus::LockedOut
            } else if !self.effective_filter().accepts(meta.tags) {
                CandidateStatus::Filtered
            } else {
                CandidateStatus::Candidate
            };
            Some(CandidateRow {
                frame,
                clip: at.clip,
                time: at.time,
                mirrored: database.clips().get(at.clip).is_some_and(|c| c.mirrored),
                total,
                parts,
                transition,
                extra_bias,
                status,
            })
        };
        let rows = nearest.iter().filter_map(|n| row_for(n.row, false)).collect();
        let continuation = report
            .continuation_cost
            .and(playing)
            .and_then(|frame| row_for(frame, true));
        Some(CandidateReport {
            rows,
            continuation,
            method_used: Some(report.method_used),
        })
    }
}
