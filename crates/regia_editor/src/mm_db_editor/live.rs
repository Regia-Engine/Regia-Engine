// SPDX-License-Identifier: MPL-2.0
//! Live stats and warnings (part B): after each document change the editor waits for the edits to
//! settle ([`DEBOUNCE`]) and then rebuilds through part A's rebuild job (`mm_db_rebuild:<n>`, an
//! ADR-7664 worker thread, never the UI thread), so the warnings list and the stats follow the
//! document. A live rebuild still running for an older revision is cancelled first. The window
//! calls [`live_tick`] every frame with the frame's time; tests pass their own instants.

use std::time::{Duration, Instant};

use regia_ecs::prelude::*;

use super::{MmDbEditor, MmDbRebuildJob};

/// How long the document must stay unchanged before a live rebuild starts.
pub const DEBOUNCE: Duration = Duration::from_millis(400);

/// Live rebuild state.
#[derive(Resource, Debug)]
pub struct MmDbLive {
    /// Rebuild after edits (the window's "Live" box, `editor.mm_db_rebuild { action: "live" }`).
    pub enabled: bool,
    /// The revision waiting to settle and when it was first seen.
    pending: Option<(u64, Instant)>,
    /// `(session, revision)` the last live rebuild started for (a failed start is not recorded).
    started: Option<(u64, u64)>,
    /// Why the last live rebuild could not start (`no_project` without a project), quietly shown.
    pub last_error: Option<String>,
    /// Live rebuilds started (tests and the agent state read it).
    pub runs: u64,
}

impl Default for MmDbLive {
    fn default() -> Self {
        Self {
            enabled: true,
            pending: None,
            started: None,
            last_error: None,
            runs: 0,
        }
    }
}

/// One live-rebuild step at time `now`: starts a rebuild of the open document once its revision
/// has been unchanged for [`DEBOUNCE`] and no rebuild was started for it yet. Returns the job id
/// when it started one.
pub fn live_tick(world: &mut World, now: Instant) -> Option<String> {
    let (session, revision) = {
        let editor = world.get_resource::<MmDbEditor>()?;
        if !editor.is_open() {
            return None;
        }
        (editor.session(), editor.revision)
    };
    // A rebuild (started by hand or by the agent) already covers this revision.
    let covered = world
        .get_resource::<MmDbRebuildJob>()
        .is_some_and(|j| j.session == session && j.revision == revision)
        || world.get_resource::<MmDbEditor>().is_some_and(
            |e| matches!(&e.rebuild, Some(super::RebuildView::Done { revision: r, .. }) if *r == revision),
        );
    let mut live = world.get_resource_or_insert_with(MmDbLive::default);
    if covered {
        live.started = Some((session, revision));
    }
    if !live.enabled || live.started == Some((session, revision)) {
        live.pending = None;
        return None;
    }
    match live.pending {
        Some((pending, since)) if pending == revision => {
            if now.saturating_duration_since(since) < DEBOUNCE {
                return None;
            }
        }
        _ => {
            live.pending = Some((revision, now));
            return None;
        }
    }
    live.pending = None;
    if world.contains_resource::<MmDbRebuildJob>() {
        // A rebuild of an older revision: its result would be stale at once.
        super::cancel_rebuild(world);
    }
    let started = super::start_rebuild(world);
    let mut live = world.get_resource_or_insert_with(MmDbLive::default);
    match started {
        Ok(id) => {
            live.started = Some((session, revision));
            live.runs += 1;
            live.last_error = None;
            Some(id)
        }
        Err(e) => {
            // Not latched: the next tick after the debounce tries this revision again (a project
            // opened since, or a worker that can start now), and Live stays meaningful.
            live.last_error = Some(format!("{} ({})", e.message, e.code));
            None
        }
    }
}
