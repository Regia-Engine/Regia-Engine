// SPDX-License-Identifier: MPL-2.0
//! Announcer helpers: per-line rotation without repeats, priority pre-emption and ducking
//! (`docs/specs/16-audio.md` § 3a).
//!
//! Pure decision logic ([`Announcer`]); the winner is handed back to [`crate::Audio`] which
//! starts the sound on the Voice bus, counts it under the `"Announcer"` activity key (so the
//! duck rules engage) and owns the playing handle.

use std::collections::{HashMap, VecDeque};

use regia_core::math::Rng;

/// Priority class of a voice line. Higher pre-empts lower; at most one `Low` line is ever queued.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum AnnouncePriority {
    /// flavour lines; dropped unless the announcer is idle, never queued behind more than one.
    #[default]
    Low,
    /// ordinary callouts.
    Normal,
    /// important lines; pre-empt `Low`/`Normal`.
    High,
    /// match-critical lines; pre-empt everything.
    Critical,
}

/// The outcome of an `announce` call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnnounceOutcome {
    /// The line started now.
    Started,
    /// The line started now and pre-empted the line that was playing.
    Preempted,
    /// The line was queued (only ever one `Low` line).
    Queued,
    /// The announcer is busy with something at least as important; the line is dropped.
    Rejected,
}

/// One bank of interchangeable lines for an id (`"first_blood"`, `"double_kill"`).
pub struct AnnouncerBank {
    /// The interchangeable sounds.
    pub sounds: Vec<crate::sound::SoundAsset>,
    /// Priority of every line in this bank.
    pub priority: AnnouncePriority,
    last: VecDeque<usize>,
}

impl AnnouncerBank {
    /// A bank of `sounds` at `priority`.
    pub fn new(sounds: Vec<crate::sound::SoundAsset>, priority: AnnouncePriority) -> Self {
        Self {
            sounds,
            priority,
            last: VecDeque::new(),
        }
    }

    /// Picks the next variation without repeating any of the last 2 picks (or the bank size − 1,
    /// whichever is smaller).
    pub fn pick(&mut self, rng: &mut Rng) -> Option<usize> {
        let len = self.sounds.len();
        if len == 0 {
            return None;
        }
        let window = self.last.len().min(len.saturating_sub(1));
        // The most recent picks are the forbidden ones (the deque is oldest first).
        let forbidden: Vec<usize> = self.last.iter().rev().copied().take(window).collect();
        let candidates: Vec<usize> = (0..len).filter(|i| !forbidden.contains(i)).collect();
        let index = if candidates.len() == 1 {
            candidates[0]
        } else {
            candidates[((rng.f32() * candidates.len() as f32) as usize).min(candidates.len() - 1)]
        };
        self.last.push_back(index);
        const NO_REPEAT: usize = 2;
        while self.last.len() > NO_REPEAT {
            self.last.pop_front();
        }
        Some(index)
    }
}

/// Pure announcer state: bank priorities, the current line, the single low-priority queue slot.
/// The banks themselves (sounds + rotation) live with [`crate::Audio`], which needs to hand
/// their assets to the mixer.
pub struct Announcer {
    priorities: HashMap<String, AnnouncePriority>,
    /// id of the bank currently playing.
    current: Option<String>,
    /// priority of the current line.
    current_priority: AnnouncePriority,
    /// the single queued low-priority line.
    queued_low: Option<String>,
    /// True while a line is playing (cleared by `note_finished`).
    active: bool,
}

impl Announcer {
    /// An announcer with no banks.
    pub fn new() -> Self {
        Self {
            priorities: HashMap::new(),
            current: None,
            current_priority: AnnouncePriority::Low,
            queued_low: None,
            active: false,
        }
    }

    /// Records the priority of a bank (called when the bank is registered).
    pub fn set_bank_priority(&mut self, id: &str, priority: AnnouncePriority) {
        self.priorities.insert(id.to_string(), priority);
    }

    /// The priority a bank was registered with.
    pub fn bank_priority(&self, id: &str) -> Option<AnnouncePriority> {
        self.priorities.get(id).copied()
    }

    /// Decides what an `announce(id)` call should do. Does not start anything: the caller
    /// executes the decision and reports back through [`Announcer::note_started`] /
    /// [`Announcer::note_finished`].
    pub fn decide(&self, id: &str) -> AnnounceOutcome {
        let Some(&priority) = self.priorities.get(id) else {
            return AnnounceOutcome::Rejected;
        };
        match self.current.as_deref() {
            None => AnnounceOutcome::Started,
            Some(current_id) => {
                if priority > self.current_priority {
                    AnnounceOutcome::Preempted
                } else if priority == AnnouncePriority::Low && self.queued_low.is_none() {
                    let current_is_low = self.current_priority == AnnouncePriority::Low;
                    if current_is_low && current_id == id {
                        AnnounceOutcome::Rejected
                    } else {
                        AnnounceOutcome::Queued
                    }
                } else {
                    AnnounceOutcome::Rejected
                }
            }
        }
    }

    /// The caller executed a `Started`/`Preempted` decision.
    pub fn note_started(&mut self, id: &str) {
        self.current = Some(id.to_string());
        self.active = true;
        if let Some(priority) = self.priorities.get(id) {
            self.current_priority = *priority;
        }
    }

    /// The current line finished (or was pre-empted away). Returns the queued low-priority line
    /// to start next, if any.
    pub fn note_finished(&mut self) -> Option<String> {
        self.current = None;
        self.active = false;
        self.queued_low.take()
    }

    /// True while a line is playing (drives the `"Announcer"` ducking activity).
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// The id of the playing line.
    pub fn current_line(&self) -> Option<&str> {
        self.current.as_deref()
    }

    /// The queued low-priority line.
    pub fn queued(&self) -> Option<&str> {
        self.queued_low.as_deref()
    }

    /// Internal: records a queued line (called by the Audio facade after `decide` returned
    /// `Queued`).
    pub fn note_queued(&mut self, id: &str) {
        self.queued_low = Some(id.to_string());
    }
}

impl Default for Announcer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::SoundClip;
    use crate::sound::SoundAsset;

    fn sound() -> SoundAsset {
        let clip = SoundClip::new(vec![0.3; 4_800], 48_000).expect("clip");
        SoundAsset::from_clip(&clip)
    }

    fn bank(n: usize, priority: AnnouncePriority) -> AnnouncerBank {
        AnnouncerBank::new((0..n).map(|_| sound()).collect(), priority)
    }

    #[test]
    fn rotation_never_repeats_the_last_two_of_five() {
        let mut lines = bank(5, AnnouncePriority::Normal);
        let mut rng = Rng::new(99);
        let mut seen = [0usize; 5];
        let mut history: Vec<usize> = Vec::new();
        for _ in 0..1_000 {
            let pick = lines.pick(&mut rng).expect("non-empty bank");
            if history.len() >= 2 {
                assert_ne!(pick, history[history.len() - 1]);
                assert_ne!(pick, history[history.len() - 2]);
            }
            seen[pick] += 1;
            history.push(pick);
        }
        assert!(seen.iter().all(|&c| c > 50), "all variations rotate: {seen:?}");
    }

    #[test]
    fn empty_bank_picks_nothing() {
        let mut lines = bank(0, AnnouncePriority::Normal);
        let mut rng = Rng::new(1);
        assert!(lines.pick(&mut rng).is_none());
    }

    #[test]
    fn higher_priority_pre_empts_lower() {
        let mut announcer = Announcer::new();
        announcer.set_bank_priority("flavour", AnnouncePriority::Low);
        announcer.set_bank_priority("kill", AnnouncePriority::Normal);
        announcer.set_bank_priority("match_point", AnnouncePriority::Critical);

        assert_eq!(announcer.decide("flavour"), AnnounceOutcome::Started);
        announcer.note_started("flavour");

        // Low does not pre-empt low; it queues (one slot).
        assert_eq!(announcer.decide("kill"), AnnounceOutcome::Preempted);
        announcer.note_started("kill");

        assert_eq!(announcer.decide("match_point"), AnnounceOutcome::Preempted);
        announcer.note_started("match_point");

        // Equal priority while playing is rejected.
        assert_eq!(announcer.decide("match_point"), AnnounceOutcome::Rejected);
        assert!(announcer.is_active());
    }

    #[test]
    fn at_most_one_low_line_is_queued() {
        let mut announcer = Announcer::new();
        announcer.set_bank_priority("a", AnnouncePriority::Low);
        announcer.set_bank_priority("b", AnnouncePriority::Low);
        announcer.set_bank_priority("callout", AnnouncePriority::High);

        assert_eq!(announcer.decide("callout"), AnnounceOutcome::Started);
        announcer.note_started("callout");

        assert_eq!(announcer.decide("a"), AnnounceOutcome::Queued);
        announcer.note_queued("a");
        // Second low line while one is queued: rejected.
        assert_eq!(announcer.decide("b"), AnnounceOutcome::Rejected);

        // When the current line finishes, the queued line starts.
        let next = announcer.note_finished().expect("queued line");
        assert_eq!(next, "a");
        assert!(!announcer.is_active(), "ducking releases between lines");
        announcer.note_started("a");
        // While the low line plays, another low line queues again.
        assert_eq!(announcer.decide("b"), AnnounceOutcome::Queued);
        announcer.note_queued("b");
        assert_eq!(announcer.note_finished(), Some("b".into()));
        // Nothing queued now.
        assert_eq!(announcer.note_finished(), None);
    }

    #[test]
    fn unknown_ids_are_rejected() {
        let announcer = Announcer::new();
        assert_eq!(announcer.decide("ghost"), AnnounceOutcome::Rejected);
    }
}
