// SPDX-License-Identifier: MPL-2.0
//! Text cues (specs/50 § 2.8, ADR-2410): text tracks (`S_TEXT/UTF8`, `S_TEXT/WEBVTT` and the other
//! `S_TEXT/*` ids) are demuxed, never drawn. The decode session pushes each text packet into a
//! shared [`CueStore`] as it reads ahead; the player's [`CueTracker`] compares the store with the
//! media position every tick and queues `VideoEvent::TextCue` when playback enters a cue and
//! `VideoEvent::TextCueEnd` when it leaves one (by time, a seek, a loop wrap or a close).
//!
//! Bounds: at most [`MAX_CUES`] cues per session (later ones are counted and dropped) and
//! [`MAX_CUE_BYTES`] bytes of text per cue (cut at a character boundary). Invalid UTF-8 is replaced,
//! never refused. A cue read twice (a loop pass, a seek back) is kept once, keyed by track and
//! start. A cue without a block duration lasts until the next cue of its track starts, else to the
//! end of the clip.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use crate::player::VideoEvent;

/// Most cues one session keeps.
pub const MAX_CUES: usize = 4096;
/// Most bytes of text one cue keeps.
pub const MAX_CUE_BYTES: usize = 4096;

/// One text cue as demuxed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextCue {
    /// Index of the text track in `MediaInfo::tracks`.
    pub track: u32,
    /// Start in nanoseconds (within one pass of the clip).
    pub start_ns: i64,
    /// End in nanoseconds, when the block carried a duration.
    pub end_ns: Option<i64>,
    /// The raw text (UTF-8).
    pub text: String,
}

#[derive(Debug, Default)]
struct CueLog {
    cues: BTreeMap<(i64, u32), TextCue>,
    dropped: u64,
    generation: u64,
}

/// The cues read so far, shared between the decode session (writer) and the player (reader).
#[derive(Debug, Clone, Default)]
pub struct CueStore {
    inner: Arc<Mutex<CueLog>>,
}

/// Cuts `text` to at most `max` bytes at a character boundary.
fn cut(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

impl CueStore {
    fn lock(&self) -> MutexGuard<'_, CueLog> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the text packet of track `track` at `start_ns` (a second read of the same cue is
    /// ignored).
    pub fn push(&self, track: u32, start_ns: i64, duration_ns: Option<u64>, data: &[u8]) {
        let text = String::from_utf8_lossy(data);
        let text = cut(text.trim_end_matches('\0').to_string(), MAX_CUE_BYTES);
        let end_ns = duration_ns
            .filter(|d| *d > 0)
            .map(|d| start_ns.saturating_add(d.min(i64::MAX as u64) as i64));
        let mut log = self.lock();
        let key = (start_ns, track);
        if log.cues.contains_key(&key) {
            return;
        }
        if log.cues.len() >= MAX_CUES {
            log.dropped += 1;
            return;
        }
        log.cues.insert(
            key,
            TextCue {
                track,
                start_ns,
                end_ns,
                text,
            },
        );
        log.generation += 1;
    }

    /// Cues held.
    pub fn len(&self) -> usize {
        self.lock().cues.len()
    }

    /// True when no cue was read.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Cues refused because the store was full.
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    /// Every cue, sorted by start then track.
    pub fn cues(&self) -> Vec<TextCue> {
        self.lock().cues.values().cloned().collect()
    }

    fn generation(&self) -> u64 {
        self.lock().generation
    }
}

/// A cue with its resolved end.
#[derive(Debug, Clone)]
struct Resolved {
    cue: TextCue,
    end_ns: i64,
}

/// Enters and leaves cues as the media position moves (one per player session).
#[derive(Debug, Default)]
pub struct CueTracker {
    store: CueStore,
    seen: Option<u64>,
    cues: Vec<Resolved>,
    active: Vec<(i64, u32)>,
    clip_end: Option<i64>,
}

fn secs(ns: i64) -> f64 {
    ns as f64 / 1e9
}

impl CueTracker {
    /// A tracker over `store`.
    pub fn new(store: CueStore) -> Self {
        Self {
            store,
            ..Self::default()
        }
    }

    /// The store this tracker reads.
    pub fn store(&self) -> &CueStore {
        &self.store
    }

    /// Cues the position is inside now, as (start, track).
    pub fn active(&self) -> &[(i64, u32)] {
        &self.active
    }

    fn refresh(&mut self, clip_end: Option<i64>) {
        let generation = self.store.generation();
        if self.seen == Some(generation) && self.clip_end == clip_end {
            return;
        }
        self.seen = Some(generation);
        self.clip_end = clip_end;
        let cues = self.store.cues();
        let mut out = Vec::with_capacity(cues.len());
        for (i, cue) in cues.iter().enumerate() {
            let next_same_track = cues[i + 1..]
                .iter()
                .find(|c| c.track == cue.track)
                .map(|c| c.start_ns);
            let end_ns = cue
                .end_ns
                .or(next_same_track)
                .or(clip_end)
                .unwrap_or(i64::MAX)
                .max(cue.start_ns.saturating_add(1));
            out.push(Resolved {
                cue: cue.clone(),
                end_ns,
            });
        }
        self.cues = out;
    }

    /// Moves to media position `pos_ns` (within the pass): queues `TextCueEnd` for every active cue
    /// the position left, then `TextCue` for every cue it entered, in start order.
    pub fn update(&mut self, pos_ns: i64, clip_end: Option<i64>, events: &mut Vec<VideoEvent>) {
        self.refresh(clip_end);
        let inside = |r: &Resolved| r.cue.start_ns <= pos_ns && pos_ns < r.end_ns;
        let cues = &self.cues;
        let mut still = Vec::with_capacity(self.active.len());
        for key in std::mem::take(&mut self.active) {
            let keep = cues
                .iter()
                .find(|r| (r.cue.start_ns, r.cue.track) == key)
                .is_some_and(inside);
            if keep {
                still.push(key);
            } else {
                events.push(VideoEvent::TextCueEnd {
                    track: key.1,
                    start: secs(key.0),
                });
            }
        }
        self.active = still;
        let upto = cues.partition_point(|r| r.cue.start_ns <= pos_ns);
        for r in &cues[..upto] {
            let key = (r.cue.start_ns, r.cue.track);
            if inside(r) && !self.active.contains(&key) {
                self.active.push(key);
                events.push(VideoEvent::TextCue {
                    track: r.cue.track,
                    start: secs(r.cue.start_ns),
                    end: (r.end_ns != i64::MAX).then(|| secs(r.end_ns)),
                    text: r.cue.text.clone(),
                });
            }
        }
    }

    /// Leaves every active cue (close, failure).
    pub fn end_all(&mut self, events: &mut Vec<VideoEvent>) {
        for (start, track) in std::mem::take(&mut self.active) {
            events.push(VideoEvent::TextCueEnd {
                track,
                start: secs(start),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000;

    #[test]
    fn enter_leave_and_open_ended_cues() {
        let store = CueStore::default();
        store.push(0, S, Some(S as u64), b"one");
        store.push(0, 3 * S, None, b"two\0");
        store.push(0, 3 * S, None, b"duplicate is ignored");
        let mut t = CueTracker::new(store);
        let mut ev = Vec::new();
        t.update(0, Some(5 * S), &mut ev);
        assert!(ev.is_empty());
        t.update(S, Some(5 * S), &mut ev);
        assert_eq!(ev.len(), 1);
        assert!(
            matches!(&ev[0], VideoEvent::TextCue { text, end: Some(e), .. } if text == "one" && (*e - 2.0).abs() < 1e-9)
        );
        ev.clear();
        t.update(2 * S, Some(5 * S), &mut ev);
        assert_eq!(ev, vec![VideoEvent::TextCueEnd { track: 0, start: 1.0 }]);
        ev.clear();
        // Open-ended: lasts to the clip end.
        t.update(3 * S, Some(5 * S), &mut ev);
        assert!(
            matches!(&ev[0], VideoEvent::TextCue { text, end: Some(e), .. } if text == "two" && (*e - 5.0).abs() < 1e-9)
        );
        ev.clear();
        // A seek back leaves it and re-enters the first.
        t.update(S + 1, Some(5 * S), &mut ev);
        assert_eq!(ev.len(), 2);
        assert!(matches!(ev[0], VideoEvent::TextCueEnd { start, .. } if start == 3.0));
        assert!(matches!(&ev[1], VideoEvent::TextCue { text, .. } if text == "one"));
        ev.clear();
        t.end_all(&mut ev);
        assert_eq!(ev.len(), 1);
    }

    #[test]
    fn store_is_bounded() {
        let store = CueStore::default();
        for i in 0..(MAX_CUES as i64 + 3) {
            store.push(0, i, None, &[b'x'; MAX_CUE_BYTES + 10]);
        }
        assert_eq!(store.len(), MAX_CUES);
        assert_eq!(store.dropped(), 3);
        assert_eq!(store.cues()[0].text.len(), MAX_CUE_BYTES);
        // A multi-byte character is never split.
        let s = CueStore::default();
        let mut text = vec![b'a'];
        text.extend("é".repeat(MAX_CUE_BYTES).as_bytes());
        s.push(1, 0, None, &text);
        let got = &s.cues()[0].text;
        assert!(got.len() <= MAX_CUE_BYTES && got.is_char_boundary(got.len()));
    }
}
