// SPDX-License-Identifier: MPL-2.0
//! The bounded inbox between reader threads and the main thread (`specs/60` § 3.4, § 4.7).
//!
//! The inbox is a plain data structure: reader threads push validated messages into it behind a
//! lock, and the main thread drains it once per tick. It implements the byte-credit window and the
//! coalescing rules, and nothing here touches a socket or the clock, so tests drive it with counts.

use std::collections::HashMap;

use super::wire::{MeshSection, MeshUpdate, Message};

/// Session id as used by the inbox.
pub type SessionId = u64;

/// Overload outcomes in a row that close the session (spec § 4.7).
pub const OVERLOAD_CLOSE_STREAK: u32 = 3;

/// Inbox settings.
#[derive(Debug, Clone, Copy)]
pub struct InboxConfig {
    /// Byte window per session (`hello_ack.window_bytes`).
    pub window_bytes: u32,
    /// Whether `transform`, `camera`, `light`, `pose` and vertex-only `mesh` frames replace older
    /// unapplied ones (`snap` playout). `delayed` playout keeps every frame.
    pub coalesce_interpolated: bool,
}

impl Default for InboxConfig {
    fn default() -> Self {
        Self {
            window_bytes: 32 << 20,
            coalesce_interpolated: true,
        }
    }
}

/// One queued message.
#[derive(Debug, Clone, PartialEq)]
pub struct InboxEntry {
    /// The session it came from.
    pub session: SessionId,
    /// The wire `seq` of the frame that carried it (of the newest frame merged into it).
    pub seq: u32,
    /// Frame bytes charged against the window for this entry (merged updates add up, so wider than
    /// one frame's `u32`).
    pub bytes: u64,
    /// The decoded message.
    pub msg: Message,
}

/// What `push` did with a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// Queued (possibly replacing or merging into an older entry).
    Accepted,
    /// Over the window: the frame was dropped; the session continues (`overloaded`, not fatal).
    Overloaded,
    /// Over the window [`OVERLOAD_CLOSE_STREAK`] times in a row: close the session.
    CloseSession,
}

/// Credit to send back to one session after a drain (an `ack` message).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Credit {
    /// The session.
    pub session: SessionId,
    /// Highest `seq` applied or coalesced away.
    pub seq_applied: u32,
    /// Window bytes returned.
    pub credit_bytes: u32,
}

/// Result of [`FeedInbox::drain`].
#[derive(Debug, Default)]
pub struct Drained {
    /// Entries in arrival order.
    pub entries: Vec<InboxEntry>,
    /// One credit per session that had frames applied or coalesced away.
    pub credits: Vec<Credit>,
}

/// Counters of one session, readable for `dcc_feed.status`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SessionCounters {
    /// Frames accepted into the inbox.
    pub frames_in: u64,
    /// Bytes accepted into the inbox.
    pub bytes_in: u64,
    /// Frames replaced or merged away before apply.
    pub frames_coalesced: u64,
    /// Frames dropped as overload.
    pub dropped_overload: u64,
    /// Frames drained to the apply step.
    pub frames_drained: u64,
}

#[derive(Debug, Default)]
struct SessionState {
    /// Bytes accepted whose credit has not been returned.
    outstanding: u64,
    overload_streak: u32,
    /// Credit accumulated since the last drain.
    pending_credit: u64,
    pending_seq: u32,
    counters: SessionCounters,
}

/// The inbox. Not internally synchronized: wrap it in a `Mutex` to share it with reader threads.
#[derive(Debug, Default)]
pub struct FeedInbox {
    config: InboxConfig,
    queue: Vec<InboxEntry>,
    sessions: HashMap<SessionId, SessionState>,
}

impl FeedInbox {
    /// An empty inbox.
    #[must_use]
    pub fn new(config: InboxConfig) -> Self {
        Self {
            config,
            queue: Vec::new(),
            sessions: HashMap::new(),
        }
    }

    /// Entries waiting for the next drain.
    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether nothing is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// The counters of `session`.
    #[must_use]
    pub fn counters(&self, session: SessionId) -> SessionCounters {
        self.sessions
            .get(&session)
            .map(|s| s.counters)
            .unwrap_or_default()
    }

    /// Bytes accepted from `session` whose credit has not been returned yet.
    #[must_use]
    pub fn outstanding(&self, session: SessionId) -> u64 {
        self.sessions.get(&session).map_or(0, |s| s.outstanding)
    }

    /// Offers one decoded frame of `bytes` size.
    pub fn push(&mut self, session: SessionId, seq: u32, bytes: u32, msg: Message) -> PushOutcome {
        let window = u64::from(self.config.window_bytes);
        let state = self.sessions.entry(session).or_default();
        // "Exceeds the window by more than one frame": the frame that crosses the line is allowed.
        if state.outstanding > window {
            state.overload_streak += 1;
            state.counters.dropped_overload += 1;
            return if state.overload_streak >= OVERLOAD_CLOSE_STREAK {
                PushOutcome::CloseSession
            } else {
                PushOutcome::Overloaded
            };
        }
        state.overload_streak = 0;
        state.outstanding += u64::from(bytes);
        state.counters.frames_in += 1;
        state.counters.bytes_in += u64::from(bytes);

        let entry = InboxEntry {
            session,
            seq,
            bytes: u64::from(bytes),
            msg,
        };
        self.insert(entry);
        PushOutcome::Accepted
    }

    fn release(&mut self, session: SessionId, seq: u32, bytes: u64, coalesced: bool) {
        if let Some(s) = self.sessions.get_mut(&session) {
            s.pending_credit = s.pending_credit.saturating_add(bytes);
            s.pending_seq = s.pending_seq.max(seq);
            if coalesced {
                s.counters.frames_coalesced += 1;
            }
        }
    }

    fn insert(&mut self, entry: InboxEntry) {
        let session = entry.session;
        let handle = entry.msg.handle();
        match &entry.msg {
            Message::ItemDelete(h) => {
                // Delete discards every pending frame of the handle, declarations included.
                let h = *h;
                let mut removed = Vec::new();
                self.queue.retain(|e| {
                    let hit = e.session == session && e.msg.handle() == Some(h);
                    if hit {
                        removed.push((e.seq, e.bytes));
                    }
                    !hit
                });
                for (seq, bytes) in removed {
                    self.release(session, seq, bytes, true);
                }
                self.queue.push(entry);
            }
            Message::Transform(_) | Message::Camera(_) | Message::Light(_) | Message::Pose(_)
                if self.config.coalesce_interpolated =>
            {
                let ty = entry.msg.msg_type();
                if let Some(old) = self.position(session, handle, |m| m.msg_type() == ty) {
                    let old = std::mem::replace(&mut self.queue[old], entry);
                    self.release(session, old.seq, old.bytes, true);
                } else {
                    self.queue.push(entry);
                }
            }
            Message::Mesh(new) if new.update_mode == 1 => self.insert_vertex_only(entry),
            Message::Mesh(_) => {
                // A full update supersedes every older pending mesh update of the handle.
                let mut removed = Vec::new();
                self.queue.retain(|e| {
                    let hit = e.session == session
                        && matches!(&e.msg, Message::Mesh(_))
                        && e.msg.handle() == handle;
                    if hit {
                        removed.push((e.seq, e.bytes));
                    }
                    !hit
                });
                for (seq, bytes) in removed {
                    self.release(session, seq, bytes, true);
                }
                self.queue.push(entry);
            }
            _ => self.queue.push(entry),
        }
    }

    fn insert_vertex_only(&mut self, entry: InboxEntry) {
        let session = entry.session;
        let handle = entry.msg.handle();
        let Message::Mesh(new) = &entry.msg else { return };
        let found = self.position(session, handle, |m| matches!(m, Message::Mesh(_)));
        let Some(at) = found else {
            self.queue.push(entry);
            return;
        };
        // Merge only when the vertex counts agree (dcc audit H2): a vertex-only update with another
        // count stays a separate entry, so the apply step checks it against the live mesh
        // (`check_vertex_only`) instead of the merged mesh pairing old indices with new vertices.
        let existing_full = match &self.queue[at].msg {
            Message::Mesh(m) if m.update_mode == 0 => Some(
                m.sections
                    .iter()
                    .find(|s| s.section_id == super::wire::section::POSITIONS)
                    .map_or(0, |s| s.count),
            ),
            _ => None,
        };
        if let Some(count) = existing_full
            && super::wire::check_vertex_only(new, count).is_err()
        {
            self.queue.push(entry);
            return;
        }
        if existing_full.is_some() {
            // Merge: the vertex-only sections replace the full update's matching sections.
            let (seq, bytes) = (entry.seq, entry.bytes);
            let Message::Mesh(full) = &mut self.queue[at].msg else {
                return;
            };
            merge_sections(full, new);
            full.info = new.info;
            self.queue[at].seq = self.queue[at].seq.max(seq);
            // The merged frame's bytes stay charged to the surviving entry until it is applied.
            self.queue[at].bytes = self.queue[at].bytes.saturating_add(bytes);
            if let Some(s) = self.sessions.get_mut(&session) {
                s.counters.frames_coalesced += 1;
            }
        } else {
            let old = std::mem::replace(&mut self.queue[at], entry);
            self.release(session, old.seq, old.bytes, true);
        }
    }

    fn position(
        &self,
        session: SessionId,
        handle: Option<u32>,
        pred: impl Fn(&Message) -> bool,
    ) -> Option<usize> {
        self.queue
            .iter()
            .position(|e| e.session == session && e.msg.handle() == handle && pred(&e.msg))
    }

    /// Takes everything queued, in arrival order, with the credits to send back.
    pub fn drain(&mut self) -> Drained {
        let entries = std::mem::take(&mut self.queue);
        for e in &entries {
            if let Some(s) = self.sessions.get_mut(&e.session) {
                s.pending_credit = s.pending_credit.saturating_add(e.bytes);
                s.pending_seq = s.pending_seq.max(e.seq);
                s.counters.frames_drained += 1;
            }
        }
        let mut credits: Vec<Credit> = Vec::new();
        for (session, s) in &mut self.sessions {
            if s.pending_credit > 0 {
                // A credit is a `u32` on the wire; what does not fit goes back on the next drain.
                let credit = u32::try_from(s.pending_credit).unwrap_or(u32::MAX);
                credits.push(Credit {
                    session: *session,
                    seq_applied: s.pending_seq,
                    credit_bytes: credit,
                });
                s.outstanding = s.outstanding.saturating_sub(u64::from(credit));
                s.pending_credit -= u64::from(credit);
            }
        }
        credits.sort_by_key(|c| c.session);
        Drained { entries, credits }
    }

    /// Forgets `session`: queued entries are discarded (spec § 2.1 `closing`) and its window.
    pub fn discard_session(&mut self, session: SessionId) {
        self.queue.retain(|e| e.session != session);
        // The counters stay: `dcc_feed.status` reports a closed session for a while.
        if let Some(state) = self.sessions.get_mut(&session) {
            *state = SessionState {
                counters: state.counters,
                ..SessionState::default()
            };
        }
    }

    /// Drops what is left of `session` once its record is gone.
    pub fn forget_session(&mut self, session: SessionId) {
        self.sessions.remove(&session);
    }
}

pub(super) fn merge_sections(full: &mut MeshUpdate, vertex_only: &MeshUpdate) {
    for s in &vertex_only.sections {
        replace_or_add(&mut full.sections, s);
    }
}

fn replace_or_add(sections: &mut Vec<MeshSection>, s: &MeshSection) {
    match sections
        .iter_mut()
        .find(|o| o.section_id == s.section_id && o.index == s.index)
    {
        Some(o) => *o = s.clone(),
        None => sections.push(s.clone()),
    }
}
