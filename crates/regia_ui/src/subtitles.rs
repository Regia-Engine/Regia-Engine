// SPDX-License-Identifier: MPL-2.0
//! Subtitle queue (ADR-1570): timed lines with a speaker, a priority and a concurrency limit.
//!
//! A [`SubtitleLine`] says who speaks, what is shown (literal text or a localization key), when it
//! starts, how long it lasts and how important it is. The [`SubtitleQueue`] decides, for the time
//! it is advanced to, which lines are on screen:
//!
//! - a line is *active* from `start` (inclusive) to `start + duration` (exclusive);
//! - at most `max_concurrent` active lines are shown; when more are active, the highest priority
//!   wins, ties go to the earlier start, then to the line pushed first;
//! - shown lines are listed in display order: earlier start first, then push order;
//! - a line that loses its place is hidden (preempted) and returns when a place frees up while it
//!   is still active; a line whose time has passed is dropped for good.
//!
//! The queue holds no clock: [`SubtitleQueue::advance_to`] takes the time, so a game can drive it
//! from game time, audio time or a replay, and the result depends only on the pushes and the
//! times. [`SubtitleQueue::advance_to`] reports what changed as [`SubtitleEvent`]s, in a fixed
//! order (hidden lines by id, then shown lines in display order).

use std::collections::{BTreeMap, BTreeSet};

use regia_app::prelude::*;

use crate::localization::Localization;

/// Default number of subtitle lines shown at once.
pub const DEFAULT_MAX_CONCURRENT: usize = 3;

/// Default limit on lines waiting or showing.
pub const DEFAULT_MAX_PENDING: usize = 256;

/// The id of a queued line: increases with every push, never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubtitleId(pub u64);

/// One timed subtitle line.
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleLine {
    /// Who speaks (shown as a prefix by the game; may be empty for narration).
    pub speaker: String,
    /// Literal text; also the fallback when `key` is set but cannot be resolved.
    pub text: String,
    /// A localization key to resolve at display time instead of `text`.
    pub key: Option<String>,
    /// Fluent arguments for `key`.
    pub args: Vec<(String, String)>,
    /// When the line appears, in seconds on the caller's clock.
    pub start: f64,
    /// How long it stays, in seconds (greater than zero).
    pub duration: f64,
    /// Higher wins when more lines are active than fit.
    pub priority: i32,
}

impl SubtitleLine {
    /// A literal line with priority 0.
    pub fn new(speaker: impl Into<String>, text: impl Into<String>, start: f64, duration: f64) -> Self {
        Self {
            speaker: speaker.into(),
            text: text.into(),
            key: None,
            args: Vec::new(),
            start,
            duration,
            priority: 0,
        }
    }

    /// A line whose text is a localization key; `fallback` shows when the key has no text.
    pub fn localized(
        speaker: impl Into<String>,
        key: impl Into<String>,
        fallback: impl Into<String>,
        start: f64,
        duration: f64,
    ) -> Self {
        Self {
            key: Some(key.into()),
            ..Self::new(speaker, fallback, start, duration)
        }
    }

    /// Sets the priority.
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    /// Sets the Fluent arguments of the key.
    pub fn with_args(mut self, args: Vec<(String, String)>) -> Self {
        self.args = args;
        self
    }

    /// The time the line goes away.
    pub fn end(&self) -> f64 {
        self.start + self.duration
    }

    /// The text to show: the key resolved through `loc` when there is one and it exists in the
    /// selected or fallback locale, otherwise `text`.
    pub fn resolve(&self, loc: &Localization) -> String {
        match &self.key {
            Some(key) if loc.has(key) => loc.resolve(key, &self.args),
            _ => self.text.clone(),
        }
    }
}

/// Why a subtitle operation failed. Codes are lowercase ascii, digits and `_`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubtitleError {
    /// The line has no text and no key.
    #[error("subtitle has no text and no key")]
    EmptyText,
    /// `start` is not a finite number or is negative.
    #[error("subtitle start must be a finite number of seconds, 0 or more")]
    BadStart,
    /// `duration` is not finite or is not greater than zero, or `start + duration` is not finite.
    #[error("subtitle duration must be a finite number of seconds greater than 0")]
    BadDuration,
    /// The queue already holds the maximum number of lines.
    #[error("subtitle queue is full ({0} lines)")]
    QueueFull(usize),
    /// The time given to `advance_to` is not finite or is earlier than the current time.
    #[error("subtitle time must be finite and not go backwards")]
    BadTime,
}

impl SubtitleError {
    /// The stable error code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyText => "subtitle_empty_text",
            Self::BadStart => "subtitle_bad_start",
            Self::BadDuration => "subtitle_bad_duration",
            Self::QueueFull(_) => "subtitle_queue_full",
            Self::BadTime => "subtitle_bad_time",
        }
    }
}

/// Why a shown line was hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HideReason {
    /// Its time ran out.
    Expired,
    /// More important lines took its place; it may return.
    Preempted,
    /// It was cancelled or cleared.
    Removed,
}

impl HideReason {
    /// The protocol name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Preempted => "preempted",
            Self::Removed => "removed",
        }
    }
}

/// A change in what is on screen, from [`SubtitleQueue::advance_to`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleEvent {
    /// The line is now on screen.
    Shown(SubtitleId),
    /// The line left the screen.
    Hidden(SubtitleId, HideReason),
}

/// The subtitle queue.
#[derive(Resource, Debug, Clone)]
pub struct SubtitleQueue {
    max_concurrent: usize,
    max_pending: usize,
    next_id: u64,
    now: f64,
    lines: BTreeMap<SubtitleId, SubtitleLine>,
    shown: BTreeSet<SubtitleId>,
    removed_shown: Vec<SubtitleId>,
}

impl Default for SubtitleQueue {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_CONCURRENT)
    }
}

impl SubtitleQueue {
    /// A queue showing at most `max_concurrent` lines (at least 1).
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            max_concurrent: max_concurrent.max(1),
            max_pending: DEFAULT_MAX_PENDING,
            next_id: 1,
            now: 0.0,
            lines: BTreeMap::new(),
            shown: BTreeSet::new(),
            removed_shown: Vec::new(),
        }
    }

    /// How many lines can be on screen at once.
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// Sets the concurrency limit (at least 1); it applies at the next [`Self::advance_to`].
    pub fn set_max_concurrent(&mut self, max_concurrent: usize) {
        self.max_concurrent = max_concurrent.max(1);
    }

    /// Sets the limit on queued lines (at least 1).
    pub fn set_max_pending(&mut self, max_pending: usize) {
        self.max_pending = max_pending.max(1);
    }

    /// The time the queue was last advanced to.
    pub fn now(&self) -> f64 {
        self.now
    }

    /// Lines that have not expired yet (waiting, showing or preempted).
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether no line is queued.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Queues a line and returns its id. The line may start in the past; it then shows at the
    /// next [`Self::advance_to`] if it has not ended yet.
    pub fn push(&mut self, line: SubtitleLine) -> Result<SubtitleId, SubtitleError> {
        if line.text.trim().is_empty() && line.key.as_deref().is_none_or(|k| k.trim().is_empty()) {
            return Err(SubtitleError::EmptyText);
        }
        if !line.start.is_finite() || line.start < 0.0 {
            return Err(SubtitleError::BadStart);
        }
        // A finite start plus a finite duration can still overflow to an end that never comes.
        if !line.duration.is_finite() || line.duration <= 0.0 || !line.end().is_finite() {
            return Err(SubtitleError::BadDuration);
        }
        if self.lines.len() >= self.max_pending {
            return Err(SubtitleError::QueueFull(self.max_pending));
        }
        let id = SubtitleId(self.next_id);
        self.next_id += 1;
        self.lines.insert(id, line);
        Ok(id)
    }

    /// The queued line `id`.
    pub fn get(&self, id: SubtitleId) -> Option<&SubtitleLine> {
        self.lines.get(&id)
    }

    /// Removes a line; a shown line reports `Hidden(.., Removed)` at the next advance. Returns the
    /// line, or `None` when the id is unknown or already expired.
    pub fn cancel(&mut self, id: SubtitleId) -> Option<SubtitleLine> {
        let line = self.lines.remove(&id)?;
        if self.shown.remove(&id) {
            self.removed_shown.push(id);
        }
        Some(line)
    }

    /// Removes every line of `speaker`; returns how many.
    pub fn cancel_speaker(&mut self, speaker: &str) -> usize {
        let ids: Vec<SubtitleId> = self
            .lines
            .iter()
            .filter(|(_, l)| l.speaker == speaker)
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            self.cancel(*id);
        }
        ids.len()
    }

    /// Removes every line; returns how many.
    pub fn clear(&mut self) -> usize {
        let ids: Vec<SubtitleId> = self.lines.keys().copied().collect();
        for id in &ids {
            self.cancel(*id);
        }
        ids.len()
    }

    /// Advances the clock to `now` and returns what changed on screen. `now` must be finite and
    /// not earlier than the previous call (otherwise [`SubtitleError::BadTime`], and nothing
    /// changes). Lines whose end is at or before `now` are dropped.
    pub fn advance_to(&mut self, now: f64) -> Result<Vec<SubtitleEvent>, SubtitleError> {
        if !now.is_finite() || now < self.now {
            return Err(SubtitleError::BadTime);
        }
        let _span = tracing::trace_span!("subtitle_advance", lines = self.lines.len()).entered();
        self.now = now;
        let mut hidden: Vec<(SubtitleId, HideReason)> = self
            .removed_shown
            .drain(..)
            .map(|id| (id, HideReason::Removed))
            .collect();
        // Expired lines go for good.
        let expired: Vec<SubtitleId> = self
            .lines
            .iter()
            .filter(|(_, l)| l.end() <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.lines.remove(&id);
            if self.shown.remove(&id) {
                hidden.push((id, HideReason::Expired));
            }
        }
        // The best active lines win the places.
        let mut active: Vec<(SubtitleId, &SubtitleLine)> = self
            .lines
            .iter()
            .filter(|(_, l)| l.start <= now)
            .map(|(id, l)| (*id, l))
            .collect();
        active.sort_by(|(ia, a), (ib, b)| {
            b.priority
                .cmp(&a.priority)
                .then(a.start.total_cmp(&b.start))
                .then(ia.cmp(ib))
        });
        active.truncate(self.max_concurrent);
        let winners: BTreeSet<SubtitleId> = active.iter().map(|(id, _)| *id).collect();
        for id in &self.shown {
            if !winners.contains(id) {
                hidden.push((*id, HideReason::Preempted));
            }
        }
        let mut newly: Vec<SubtitleId> = winners.difference(&self.shown).copied().collect();
        self.shown = winners;
        hidden.sort_by_key(|(id, _)| *id);
        newly.sort_by(|a, b| self.display_order(*a, *b));
        let mut events: Vec<SubtitleEvent> = hidden
            .into_iter()
            .map(|(id, reason)| SubtitleEvent::Hidden(id, reason))
            .collect();
        events.extend(newly.into_iter().map(SubtitleEvent::Shown));
        Ok(events)
    }

    fn display_order(&self, a: SubtitleId, b: SubtitleId) -> std::cmp::Ordering {
        let (sa, sb) = (
            self.lines.get(&a).map_or(0.0, |l| l.start),
            self.lines.get(&b).map_or(0.0, |l| l.start),
        );
        sa.total_cmp(&sb).then(a.cmp(&b))
    }

    /// The lines on screen as of the last [`Self::advance_to`], in display order (earlier start
    /// first, then push order).
    pub fn visible(&self) -> Vec<(SubtitleId, &SubtitleLine)> {
        let mut ids: Vec<SubtitleId> = self.shown.iter().copied().collect();
        ids.sort_by(|a, b| self.display_order(*a, *b));
        ids.into_iter()
            .filter_map(|id| self.lines.get(&id).map(|l| (id, l)))
            .collect()
    }

    /// Every queued line in push order with whether it is on screen.
    pub fn queued(&self) -> Vec<(SubtitleId, &SubtitleLine, bool)> {
        self.lines
            .iter()
            .map(|(id, l)| (*id, l, self.shown.contains(id)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(speaker: &str, text: &str, start: f64, duration: f64) -> SubtitleLine {
        SubtitleLine::new(speaker, text, start, duration)
    }

    fn texts(q: &SubtitleQueue) -> Vec<&str> {
        q.visible().iter().map(|(_, l)| l.text.as_str()).collect()
    }

    #[test]
    fn a_line_is_active_from_start_to_end_exclusive() {
        let mut q = SubtitleQueue::default();
        let id = q.push(line("A", "hi", 1.0, 2.0)).expect("push");
        assert!(q.advance_to(0.5).expect("t").is_empty());
        assert!(q.visible().is_empty());
        assert_eq!(q.advance_to(1.0).expect("t"), [SubtitleEvent::Shown(id)]);
        assert_eq!(texts(&q), ["hi"]);
        assert!(q.advance_to(2.9).expect("t").is_empty());
        assert_eq!(
            q.advance_to(3.0).expect("t"),
            [SubtitleEvent::Hidden(id, HideReason::Expired)]
        );
        assert!(q.is_empty(), "expired lines are dropped");
    }

    #[test]
    fn a_late_push_shows_at_the_next_advance() {
        let mut q = SubtitleQueue::default();
        q.advance_to(10.0).expect("t");
        let past = q.push(line("A", "late", 9.0, 5.0)).expect("push");
        let gone = q.push(line("A", "gone", 1.0, 1.0)).expect("push");
        let events = q.advance_to(10.0).expect("t");
        assert_eq!(
            events,
            [SubtitleEvent::Shown(past)],
            "the expired one never shows (and is not reported hidden)"
        );
        assert!(q.get(gone).is_none());
    }

    #[test]
    fn max_concurrent_keeps_the_highest_priority() {
        let mut q = SubtitleQueue::new(2);
        let low = q.push(line("A", "low", 0.0, 10.0)).expect("push");
        let mid = q
            .push(line("B", "mid", 0.0, 10.0).with_priority(5))
            .expect("push");
        let high = q
            .push(line("C", "high", 0.0, 10.0).with_priority(9))
            .expect("push");
        let events = q.advance_to(0.0).expect("t");
        assert_eq!(events, [SubtitleEvent::Shown(mid), SubtitleEvent::Shown(high)]);
        assert_eq!(
            texts(&q),
            ["mid", "high"],
            "display order: start, then push order"
        );
        assert!(!q.queued().iter().any(|(id, _, shown)| *id == low && *shown));
    }

    #[test]
    fn ties_go_to_the_earlier_start_then_the_earlier_push() {
        let mut q = SubtitleQueue::new(1);
        let _b = q.push(line("B", "b", 2.0, 10.0)).expect("push");
        let a = q.push(line("A", "a", 1.0, 10.0)).expect("push");
        q.advance_to(3.0).expect("t");
        assert_eq!(q.visible()[0].0, a, "earlier start wins");
        let mut q = SubtitleQueue::new(1);
        let first = q.push(line("A", "first", 1.0, 10.0)).expect("push");
        let _second = q.push(line("B", "second", 1.0, 10.0)).expect("push");
        q.advance_to(1.0).expect("t");
        assert_eq!(q.visible()[0].0, first, "equal start: push order");
    }

    #[test]
    fn a_higher_priority_newcomer_preempts_and_the_loser_returns() {
        let mut q = SubtitleQueue::new(1);
        let calm = q.push(line("A", "calm", 0.0, 10.0)).expect("push");
        assert_eq!(q.advance_to(0.0).expect("t"), [SubtitleEvent::Shown(calm)]);
        let alarm = q
            .push(line("B", "alarm", 2.0, 2.0).with_priority(10))
            .expect("push");
        assert_eq!(
            q.advance_to(2.0).expect("t"),
            [
                SubtitleEvent::Hidden(calm, HideReason::Preempted),
                SubtitleEvent::Shown(alarm),
            ]
        );
        assert_eq!(texts(&q), ["alarm"]);
        assert_eq!(
            q.advance_to(4.0).expect("t"),
            [
                SubtitleEvent::Hidden(alarm, HideReason::Expired),
                SubtitleEvent::Shown(calm),
            ]
        );
        assert_eq!(texts(&q), ["calm"]);
    }

    #[test]
    fn display_order_is_by_start_not_priority() {
        let mut q = SubtitleQueue::new(3);
        q.push(line("A", "late-high", 2.0, 10.0).with_priority(9))
            .expect("push");
        q.push(line("B", "early-low", 1.0, 10.0)).expect("push");
        q.push(line("C", "mid", 1.5, 10.0).with_priority(3))
            .expect("push");
        q.advance_to(2.0).expect("t");
        assert_eq!(texts(&q), ["early-low", "mid", "late-high"]);
    }

    #[test]
    fn cancel_reports_removed_once_and_unknown_ids_are_none() {
        let mut q = SubtitleQueue::default();
        let id = q.push(line("A", "x", 0.0, 5.0)).expect("push");
        q.advance_to(0.0).expect("t");
        assert!(q.cancel(id).is_some());
        assert!(q.cancel(id).is_none());
        assert!(q.cancel(SubtitleId(999)).is_none());
        assert_eq!(
            q.advance_to(1.0).expect("t"),
            [SubtitleEvent::Hidden(id, HideReason::Removed)]
        );
        assert!(q.advance_to(2.0).expect("t").is_empty());
        assert!(q.visible().is_empty());
    }

    #[test]
    fn cancelling_a_waiting_line_is_silent() {
        let mut q = SubtitleQueue::default();
        let id = q.push(line("A", "x", 5.0, 1.0)).expect("push");
        assert!(q.cancel(id).is_some());
        assert!(q.advance_to(5.0).expect("t").is_empty());
    }

    #[test]
    fn cancel_speaker_and_clear() {
        let mut q = SubtitleQueue::default();
        q.push(line("A", "1", 0.0, 5.0)).expect("push");
        q.push(line("B", "2", 0.0, 5.0)).expect("push");
        q.push(line("A", "3", 9.0, 5.0)).expect("push");
        q.advance_to(0.0).expect("t");
        assert_eq!(q.cancel_speaker("A"), 2);
        assert_eq!(q.cancel_speaker("nobody"), 0);
        assert_eq!(q.len(), 1);
        assert_eq!(q.clear(), 1);
        assert!(q.is_empty());
    }

    #[test]
    fn invalid_lines_are_refused_with_codes() {
        let mut q = SubtitleQueue::default();
        let cases = [
            (line("A", "  ", 0.0, 1.0), "subtitle_empty_text"),
            (line("A", "x", -1.0, 1.0), "subtitle_bad_start"),
            (line("A", "x", f64::NAN, 1.0), "subtitle_bad_start"),
            (line("A", "x", f64::INFINITY, 1.0), "subtitle_bad_start"),
            (line("A", "x", 0.0, 0.0), "subtitle_bad_duration"),
            (line("A", "x", 0.0, -2.0), "subtitle_bad_duration"),
            (line("A", "x", 0.0, f64::NAN), "subtitle_bad_duration"),
            // Both finite, the end is not: the line would never expire (ADR-1660).
            (line("A", "x", 1e308, 1e308), "subtitle_bad_duration"),
        ];
        for (l, code) in cases {
            assert_eq!(q.push(l).expect_err("refused").code(), code);
        }
        assert!(q.is_empty());
        // Empty text is fine when a key is present.
        assert!(q.push(SubtitleLine::localized("A", "k", "", 0.0, 1.0)).is_ok());
        assert_eq!(
            q.push(SubtitleLine::localized("A", " ", "", 0.0, 1.0))
                .expect_err("blank key")
                .code(),
            "subtitle_empty_text"
        );
    }

    #[test]
    fn the_queue_limit_is_enforced() {
        let mut q = SubtitleQueue::default();
        q.set_max_pending(2);
        q.push(line("A", "1", 0.0, 1.0)).expect("push");
        q.push(line("A", "2", 0.0, 1.0)).expect("push");
        let err = q.push(line("A", "3", 0.0, 1.0)).expect_err("full");
        assert_eq!(err, SubtitleError::QueueFull(2));
        assert_eq!(err.code(), "subtitle_queue_full");
        q.advance_to(5.0).expect("t");
        assert!(
            q.push(line("A", "3", 5.0, 1.0)).is_ok(),
            "expired lines free room"
        );
    }

    #[test]
    fn time_cannot_go_backwards_or_be_non_finite() {
        let mut q = SubtitleQueue::default();
        q.push(line("A", "x", 0.0, 10.0)).expect("push");
        q.advance_to(5.0).expect("t");
        for bad in [4.9, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(q.advance_to(bad).expect_err("bad").code(), "subtitle_bad_time");
        }
        assert_eq!(q.now(), 5.0, "unchanged by a refused advance");
        assert_eq!(texts(&q), ["x"]);
        assert!(q.advance_to(5.0).expect("same time is fine").is_empty());
    }

    #[test]
    fn concurrency_limit_is_at_least_one_and_can_change() {
        let mut q = SubtitleQueue::new(0);
        assert_eq!(q.max_concurrent(), 1);
        q.push(line("A", "1", 0.0, 10.0)).expect("push");
        q.push(line("B", "2", 0.0, 10.0)).expect("push");
        q.advance_to(0.0).expect("t");
        assert_eq!(q.visible().len(), 1);
        q.set_max_concurrent(2);
        let events = q.advance_to(1.0).expect("t");
        assert_eq!(events.len(), 1);
        assert_eq!(q.visible().len(), 2);
        q.set_max_concurrent(0);
        let events = q.advance_to(2.0).expect("t");
        assert_eq!(events.len(), 1, "one line is hidden when the limit drops");
        assert!(matches!(
            events[0],
            SubtitleEvent::Hidden(_, HideReason::Preempted)
        ));
    }

    #[test]
    fn a_simulation_is_deterministic_and_never_exceeds_the_limit() {
        fn run() -> Vec<Vec<SubtitleEvent>> {
            let mut q = SubtitleQueue::new(3);
            // A fixed pseudo-random schedule from a small linear generator.
            let mut state = 12345u64;
            let mut next = || {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u32
            };
            for i in 0..40 {
                let start = f64::from(next() % 200) / 10.0;
                let duration = 0.5 + f64::from(next() % 50) / 10.0;
                let priority = i32::try_from(next() % 4).unwrap_or(0);
                q.push(line("S", &format!("l{i}"), start, duration).with_priority(priority))
                    .expect("push");
            }
            let mut all = Vec::new();
            for step in 0..300 {
                let events = q.advance_to(f64::from(step) * 0.1).expect("t");
                assert!(q.visible().len() <= 3, "step {step}");
                all.push(events);
            }
            assert!(q.is_empty(), "everything expires by t=30");
            all
        }
        assert_eq!(run(), run());
    }

    #[test]
    fn events_balance_shown_and_hidden() {
        let mut q = SubtitleQueue::new(2);
        for i in 0..6 {
            q.push(line("S", &format!("{i}"), f64::from(i) * 0.5, 2.0).with_priority(i % 3))
                .expect("push");
        }
        let mut on: BTreeSet<SubtitleId> = BTreeSet::new();
        for step in 0..40 {
            for event in q.advance_to(f64::from(step) * 0.25).expect("t") {
                match event {
                    SubtitleEvent::Shown(id) => assert!(on.insert(id), "shown twice without hide"),
                    SubtitleEvent::Hidden(id, _) => assert!(on.remove(&id), "hidden while not shown"),
                }
            }
            let visible: BTreeSet<SubtitleId> = q.visible().iter().map(|(id, _)| *id).collect();
            assert_eq!(on, visible);
        }
    }

    #[test]
    fn localized_lines_resolve_through_the_localization() {
        let loc = Localization::new(
            vec![("en".to_owned(), "greet = Hello, { $name }!\n".to_owned())],
            Some("en".to_owned()),
        )
        .expect("localization");
        let l = SubtitleLine::localized("Ana", "greet", "fallback", 0.0, 1.0)
            .with_args(vec![("name".to_owned(), "Zoé".to_owned())]);
        assert_eq!(l.resolve(&loc), "Hello, Zoé!");
        let missing = SubtitleLine::localized("Ana", "nope", "fallback text", 0.0, 1.0);
        assert_eq!(missing.resolve(&loc), "fallback text");
        let literal = line("Ana", "plain", 0.0, 1.0);
        assert_eq!(literal.resolve(&loc), "plain");
        assert_eq!(literal.end(), 1.0);
    }

    #[test]
    fn hide_reason_names() {
        assert_eq!(HideReason::Expired.name(), "expired");
        assert_eq!(HideReason::Preempted.name(), "preempted");
        assert_eq!(HideReason::Removed.name(), "removed");
    }
}
