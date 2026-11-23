// SPDX-License-Identifier: MPL-2.0
//! Cue sets on the caption board (spec 36 § 8, ADR-1882): a set is one caption whose pieces are its
//! cues, so priority, overlap, wrapping, pages and layout are the board's. [`SubtitleSets`] adds what
//! a set needs on top: validation, owners and the list of live sets. There is no second presenter.

use std::collections::BTreeMap;

use regia_app::caption::{
    CaptionId, CaptionIds, CaptionKind, CaptionPiece, CaptionSource, CaptionStart, CaptionStop,
    CaptionStyleHint, CaptionText, CaptionTime,
};
use regia_app::prelude::*;
use regia_asset::dialogue::{Cue, TimedCue, cue_problem, expand_cues};

use super::{CaptionBoard, CaptionClock, EntryState};
use crate::localization::Localization;

/// What a set shows as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubtitleKind {
    /// Spoken text ([`CaptionKind::Speech`]).
    #[default]
    Dialogue,
    /// Sound-effect text ([`CaptionKind::Sound`]): shown only in caption mode.
    Caption,
}

/// A cue set to show.
#[derive(Debug, Clone, PartialEq)]
pub struct SetRequest {
    /// The cues, in time order.
    pub cues: Vec<Cue>,
    /// `> 0` to show; larger wins. 0 is never shown.
    pub priority: i32,
    /// Seconds the set lives (the sound's length).
    pub duration: f32,
    /// Seconds before the first cue counts.
    pub start_delay: f32,
    /// Keep author line breaks.
    pub keep_breaks: bool,
    /// Show a long cue one line at a time, in sequence over the cue's time, instead of stacked.
    pub one_at_a_time: bool,
    /// Dialogue or caption.
    pub kind: SubtitleKind,
    /// The speaker id, shown as the row's label.
    pub speaker: Option<String>,
    /// The entity the set belongs to, for [`stop_owner`].
    pub owner: Option<Entity>,
}

impl Default for SetRequest {
    fn default() -> Self {
        Self {
            cues: Vec::new(),
            priority: 1,
            duration: 0.0,
            start_delay: 0.0,
            keep_breaks: false,
            one_at_a_time: false,
            kind: SubtitleKind::default(),
            speaker: None,
            owner: None,
        }
    }
}

/// A live set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveSet {
    /// The set's id (its caption).
    pub id: CaptionId,
    /// Its priority.
    pub priority: i32,
    /// Its owner.
    pub owner: Option<Entity>,
    /// When its first cue counts, on the caption clock.
    pub start_us: u64,
}

/// What [`sync_set`] needs to send a set's caption again from another playhead.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SetPlan {
    /// The caption as first sent; pieces are offsets from the set's time 0.
    caption: CaptionStart,
    /// The clock value of set time 0 (may be before clock 0 after a sync far into the sound).
    base_us: i128,
}

/// How far the playhead given to [`sync_set`] may be from the set's own time before the set is
/// re-based: 40 ms, under one frame at 25 fps, so a caller that syncs every frame from a voice
/// playhead re-lays nothing while the two agree.
pub const SYNC_TOLERANCE_US: u64 = 40_000;

/// The live sets, by id. A set leaves when the board is done with its caption.
#[derive(Resource, Debug, Default, Clone, PartialEq, Eq)]
pub struct SubtitleSets {
    active: BTreeMap<CaptionId, ActiveSet>,
    plans: BTreeMap<CaptionId, SetPlan>,
}

impl SubtitleSets {
    /// The live sets in id order.
    pub fn iter(&self) -> impl Iterator<Item = &ActiveSet> {
        self.active.values()
    }

    /// Whether a set is live.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.active.is_empty()
    }
}

/// The first problem of a request, for the error text; `None` when it can be queued.
#[must_use]
pub fn set_problem(req: &SetRequest) -> Option<String> {
    if !req.duration.is_finite() || req.duration <= 0.0 {
        return Some(format!("duration {} must be above 0 seconds", req.duration));
    }
    if !req.start_delay.is_finite() || req.start_delay < 0.0 {
        return Some(format!(
            "start_delay {} must be 0 or more seconds",
            req.start_delay
        ));
    }
    if req.priority < 0 {
        return Some(format!("priority {} must be 0 or more", req.priority));
    }
    if req.cues.is_empty() {
        return Some("a set needs at least one cue".to_owned());
    }
    cue_problem(&req.cues)
}

/// Caption pieces of timed cues: one per cue, plus an empty piece where a cue ends before the next
/// starts, because a gap shows nothing. `cues` come from [`expand_cues`] with no sound length, so
/// a last cue without `until` has `end_us == start_us` and runs to the end of the caption.
pub fn cue_pieces(cues: &[TimedCue], mut text: impl FnMut(&TimedCue) -> CaptionText) -> Vec<CaptionPiece> {
    let mut pieces = Vec::with_capacity(cues.len() * 2);
    for (i, cue) in cues.iter().enumerate() {
        pieces.push(CaptionPiece {
            offset_us: cue.start_us,
            text: text(cue),
        });
        let gap = match cues.get(i + 1) {
            Some(next) => cue.end_us < next.start_us,
            None => cue.end_us > cue.start_us,
        };
        if gap {
            pieces.push(CaptionPiece::literal(cue.end_us, ""));
        }
    }
    pieces
}

fn micros(seconds: f32) -> u64 {
    (f64::from(seconds.max(0.0)) * 1_000_000.0).round() as u64
}

fn cue_text(cue: &TimedCue) -> CaptionText {
    match (&cue.text_key, &cue.text) {
        (Some(key), _) => CaptionText::Key {
            key: key.clone(),
            args: Vec::new(),
        },
        (None, raw) => CaptionText::Literal(raw.clone().unwrap_or_default()),
    }
}

/// The caption of a set: pure, the board resolves its keys.
#[must_use]
pub fn set_caption(req: &SetRequest, id: CaptionId, locale: &str, now_us: u64) -> CaptionStart {
    CaptionStart {
        id,
        source: CaptionSource::Game("subtitle_set".to_owned()),
        kind: match req.kind {
            SubtitleKind::Dialogue => CaptionKind::Speech,
            SubtitleKind::Caption => CaptionKind::Sound,
        },
        locale: locale.to_owned(),
        pieces: cue_pieces(&expand_cues(&req.cues, 0), cue_text),
        speaker: req.speaker.clone(),
        priority: req.priority.clamp(1, 255) as u8,
        start: CaptionTime::AtMicros(now_us.saturating_add(micros(req.start_delay))),
        planned_us: micros(req.duration),
        keep_breaks: req.keep_breaks,
        hint: CaptionStyleHint {
            one_at_a_time: req.one_at_a_time,
            ..CaptionStyleHint::default()
        },
    }
}

/// Queues a set. `None` for priority 0, which is never shown.
///
/// # Errors
///
/// [`set_problem`]'s text.
pub fn queue_set(world: &mut World, req: &SetRequest) -> Result<Option<CaptionId>, String> {
    if let Some(problem) = set_problem(req) {
        return Err(problem);
    }
    if req.priority == 0 {
        return Ok(None);
    }
    let now_us = world.get_resource::<CaptionClock>().map_or(0, |c| c.0);
    let locale = world
        .get_resource::<Localization>()
        .map_or_else(|| "en".to_owned(), |l| l.locale().to_owned());
    let id = world.get_resource_or_init::<CaptionIds>().next_id();
    let caption = set_caption(req, id, &locale, now_us);
    let start_us = match caption.start {
        CaptionTime::AtMicros(at) => at,
        CaptionTime::Now => now_us,
    };
    let plan = SetPlan {
        caption: caption.clone(),
        base_us: i128::from(start_us),
    };
    world.write_message(caption);
    let mut sets = world.get_resource_or_init::<SubtitleSets>();
    sets.active.insert(
        id,
        ActiveSet {
            id,
            priority: req.priority,
            owner: req.owner,
            start_us,
        },
    );
    sets.plans.insert(id, plan);
    Ok(Some(id))
}

/// What [`sync_set`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// The set's time was within [`SYNC_TOLERANCE_US`] of the playhead: nothing changed.
    InSync,
    /// The set was sent again from the playhead (same id).
    Rebased,
    /// The playhead is at or past the set's end: the set was stopped.
    Finished,
}

impl SyncOutcome {
    /// The lowercase name the agent reports.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::InSync => "in_sync",
            Self::Rebased => "rebased",
            Self::Finished => "finished",
        }
    }
}

/// The pieces of `caption` from set time `playhead_us` on, offsets from the playhead: pieces that
/// ended before it are dropped and the running one starts at 0 with its remaining time. `None`
/// when nothing is left (the playhead is at or past the planned end).
#[must_use]
pub fn pieces_from(caption: &CaptionStart, playhead_us: u64) -> Option<(Vec<CaptionPiece>, u64)> {
    if playhead_us >= caption.planned_us {
        return None;
    }
    let mut pieces = Vec::with_capacity(caption.pieces.len());
    for (i, piece) in caption.pieces.iter().enumerate() {
        let end = caption
            .pieces
            .get(i + 1)
            .map_or(caption.planned_us, |next| next.offset_us);
        if end <= playhead_us {
            continue;
        }
        pieces.push(CaptionPiece {
            offset_us: piece.offset_us.saturating_sub(playhead_us),
            text: piece.text.clone(),
        });
    }
    (!pieces.is_empty()).then(|| (pieces, caption.planned_us - playhead_us))
}

/// Sets a live set's time to a voice playhead, in seconds of set time (cue time; `start_delay` is
/// taken as already passed). Within [`SYNC_TOLERANCE_US`] nothing changes; otherwise the set's
/// caption is sent again from the playhead under the same id (spec 36 § 8.2): cues that ended are
/// dropped, the running cue continues with its remaining time and the board keeps the set's row.
/// A playhead at or past the set's duration stops the set. `None` when no live set has that id.
///
/// # Errors
///
/// A playhead that is negative or not finite.
pub fn sync_set(world: &mut World, id: CaptionId, playhead: f32) -> Result<Option<SyncOutcome>, String> {
    if !playhead.is_finite() || playhead < 0.0 {
        return Err(format!("playhead {playhead} must be 0 or more seconds"));
    }
    let now_us = world.get_resource::<CaptionClock>().map_or(0, |c| c.0);
    let Some(plan) = world
        .get_resource::<SubtitleSets>()
        .filter(|sets| sets.active.contains_key(&id))
        .and_then(|sets| sets.plans.get(&id).cloned())
    else {
        return Ok(None);
    };
    let playhead_us = micros(playhead);
    let set_time = i128::from(now_us) - plan.base_us;
    if (set_time - i128::from(playhead_us)).unsigned_abs() <= u128::from(SYNC_TOLERANCE_US) {
        return Ok(Some(SyncOutcome::InSync));
    }
    let Some((pieces, planned_us)) = pieces_from(&plan.caption, playhead_us) else {
        stop_set(world, id);
        return Ok(Some(SyncOutcome::Finished));
    };
    let mut caption = plan.caption;
    caption.pieces = pieces;
    caption.planned_us = planned_us;
    caption.start = CaptionTime::AtMicros(now_us);
    world.write_message(caption);
    let base_us = i128::from(now_us) - i128::from(playhead_us);
    let mut sets = world.get_resource_or_init::<SubtitleSets>();
    if let Some(set) = sets.active.get_mut(&id) {
        set.start_us = u64::try_from(base_us.max(0)).unwrap_or(0);
    }
    if let Some(plan) = sets.plans.get_mut(&id) {
        plan.base_us = base_us;
    }
    Ok(Some(SyncOutcome::Rebased))
}

fn stop_where(world: &mut World, stops: impl Fn(&ActiveSet) -> bool) -> usize {
    let Some(mut sets) = world.get_resource_mut::<SubtitleSets>() else {
        return 0;
    };
    let ids: Vec<CaptionId> = sets.active.values().filter(|s| stops(s)).map(|s| s.id).collect();
    sets.active.retain(|id, _| !ids.contains(id));
    sets.plans.retain(|id, _| !ids.contains(id));
    for id in &ids {
        world.write_message(CaptionStop { id: *id });
    }
    ids.len()
}

/// Stops one set; `false` when it is not live.
pub fn stop_set(world: &mut World, id: CaptionId) -> bool {
    stop_where(world, |set| set.id == id) > 0
}

/// Stops every set and returns how many.
pub fn stop_all_sets(world: &mut World) -> usize {
    stop_where(world, |_| true)
}

/// Stops the sets of an owner and returns how many.
pub fn stop_owner(world: &mut World, owner: Entity) -> usize {
    stop_where(world, |set| set.owner == Some(owner))
}

/// `PostUpdate`, after the board: forgets the sets it is done with.
pub fn prune_subtitle_sets(mut sets: ResMut<SubtitleSets>, board: Res<CaptionBoard>) {
    if sets.is_active() {
        let live = |id: &CaptionId| {
            matches!(
                board.state_of(*id),
                Some(EntryState::Pending | EntryState::Waiting | EntryState::Visible)
            )
        };
        sets.active.retain(|id, _| live(id));
        sets.plans.retain(|id, _| live(id));
    }
}
