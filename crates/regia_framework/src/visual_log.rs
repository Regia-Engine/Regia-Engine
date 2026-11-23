// SPDX-License-Identifier: MPL-2.0
//! Development-only per-entity shapes and text, retained in a bounded frame ring.
//!
//! Enable `visual-log` and install [`VisualLogPlugin`] outside the editor. Systems log through
//! [`crate::debug_log!`] with a mutable [`VisualLog`] resource. Frames advance in `First`, before
//! gameplay updates; logging in `First` itself must run after [`VisualLogFrame`]. No persistence.

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_RECORDING: AtomicU64 = AtomicU64::new(1);

use regia_app::prelude::*;
use regia_core::math::Vec3;

/// Maximum bytes retained per text entry (truncated at a UTF-8 boundary).
pub const MAX_TEXT_BYTES: usize = 512;
/// Hard upper bound on retained frames, including empty frames.
pub const MAX_FRAMES: usize = 4096;
/// Hard upper bound on entries in one frame, across all entities.
pub const MAX_ENTRIES_PER_FRAME: usize = 1024;

/// One world-space primitive. Colors are chosen by the editor's theme.
#[derive(Clone, Debug, PartialEq)]
pub enum VisualShape {
    /// A line segment.
    Line {
        /// Start in world space.
        from: Vec3,
        /// End in world space.
        to: Vec3,
    },
    /// A message anchored in world space.
    Text {
        /// Anchor in world space.
        at: Vec3,
        /// UTF-8 message; the recorder bounds its retained length.
        text: String,
    },
}

/// An entry belonging to one entity generation and one frame.
#[derive(Clone, Debug, PartialEq)]
pub struct VisualEntry {
    /// Full entity id, including generation; despawn does not discard historical entries.
    pub entity: Entity,
    /// Recorded frame number.
    pub frame: u64,
    /// World-space payload.
    pub shape: VisualShape,
}

#[derive(Clone, Debug)]
struct Frame {
    number: u64,
    entries: Vec<VisualEntry>,
}

/// An ECS-owned ring with independent bounds on frames, entries and message bytes.
///
/// Defaults to 120 frames and 128 entries per frame. It keeps empty frames, drops excess
/// entries in a full frame, and never retains more than `frame_limit * entry_limit` entries.
/// Recording makes no queries of the world, so it also accepts entities since despawned.
/// Clones share immutable frame storage; writing a shared current frame copies only that frame.
#[derive(Resource, Clone, Debug)]
pub struct VisualLog {
    recording_id: u64,
    frames: VecDeque<Arc<Frame>>,
    frame_limit: usize,
    entry_limit: usize,
    entries: usize,
    revision: u64,
    dropped: u64,
}

impl Default for VisualLog {
    fn default() -> Self {
        Self::new(120, 128)
    }
}

impl VisualLog {
    /// Creates a ring. Limits are clamped to `1..=MAX_FRAMES` and
    /// `1..=MAX_ENTRIES_PER_FRAME`; no allocation is made until recording begins.
    #[must_use]
    pub fn new(frames: usize, entries_per_frame: usize) -> Self {
        Self {
            recording_id: NEXT_RECORDING.fetch_add(1, Ordering::Relaxed),
            frames: VecDeque::new(),
            frame_limit: frames.clamp(1, MAX_FRAMES),
            entry_limit: entries_per_frame.clamp(1, MAX_ENTRIES_PER_FRAME),
            entries: 0,
            revision: 0,
            dropped: 0,
        }
    }

    /// Starts `frame`. Repeating a frame is a no-op; a backwards frame clears the old session.
    /// Gaps remain gaps; this does not allocate intermediate frames.
    #[tracing::instrument(skip_all, level = "trace", name = "visual_log_begin_frame")]
    pub fn begin_frame(&mut self, frame: u64) {
        if let Some(last) = self.frame() {
            if frame == last {
                return;
            }
            if frame < last {
                self.frames.clear();
                self.entries = 0;
                self.dropped = 0;
            }
        }
        if self.frames.len() == self.frame_limit
            && let Some(old) = self.frames.pop_front()
        {
            self.entries -= old.entries.len();
        }
        self.frames.push_back(Arc::new(Frame {
            number: frame,
            entries: Vec::new(),
        }));
        self.revision = self.revision.wrapping_add(1);
    }

    /// Records a finite shape in the current frame (frame 0 if none was begun).
    /// Returns false for a non-finite coordinate or a full frame, counting the dropped entry.
    pub fn record(&mut self, entity: Entity, mut shape: VisualShape) -> bool {
        let valid = match &shape {
            VisualShape::Line { from, to } => from.is_finite() && to.is_finite(),
            VisualShape::Text { at, .. } => at.is_finite(),
        };
        if !valid
            || self
                .frames
                .back()
                .is_some_and(|f| f.entries.len() >= self.entry_limit)
        {
            self.dropped = self.dropped.saturating_add(1);
            self.revision = self.revision.wrapping_add(1);
            return false;
        }
        if let VisualShape::Text { text, .. } = &mut shape {
            let mut end = text.len().min(MAX_TEXT_BYTES);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            // Copy into a bounded allocation, instead of keeping the caller's large capacity.
            *text = text[..end].to_owned();
        }
        if self.frames.is_empty() {
            self.begin_frame(0);
        }
        if let Some(frame) = self.frames.back_mut() {
            let frame = Arc::make_mut(frame);
            frame.entries.push(VisualEntry {
                entity,
                frame: frame.number,
                shape,
            });
            self.entries += 1;
            self.revision = self.revision.wrapping_add(1);
            return true;
        }
        false
    }

    /// Entries of exactly `frame` and only `entities`, in recording order. Empty selection
    /// draws nothing; it never implicitly selects every entity.
    #[must_use]
    pub fn entries(&self, frame: u64, entities: &[Entity]) -> Vec<VisualEntry> {
        let entities: BTreeSet<_> = entities.iter().copied().collect();
        self.frames
            .iter()
            .find(|f| f.number == frame)
            .into_iter()
            .flat_map(|f| &f.entries)
            .filter(|e| entities.contains(&e.entity))
            .cloned()
            .collect()
    }

    /// Entities with retained entries, sorted by full id; includes historical entities.
    #[must_use]
    pub fn entities(&self) -> Vec<Entity> {
        self.frames
            .iter()
            .flat_map(|f| &f.entries)
            .map(|e| e.entity)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Retained frame numbers, oldest first, including empty frames.
    #[must_use]
    pub fn frames(&self) -> Vec<u64> {
        self.frames.iter().map(|f| f.number).collect()
    }

    /// Current frame, or none before any frame/entry was recorded.
    #[must_use]
    pub fn frame(&self) -> Option<u64> {
        self.frames.back().map(|f| f.number)
    }

    /// Retained frames (constant time).
    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Retained entries (constant time).
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.entries
    }

    /// Entries refused because a frame was full or coordinates were non-finite.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Process-local recording identity. Clones preserve it; a new recorder gets a new id.
    /// This is view-cache identity only, never gameplay or persisted data.
    #[must_use]
    pub fn recording_id(&self) -> u64 {
        self.recording_id
    }

    /// Changes to frames or entries, for views that skip unchanged data.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

/// Ordering point for the frame advance in `First`.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct VisualLogFrame;

/// Installs the recorder and advances its frame once per app update. The editor installs it
/// in play sessions; standalone games opt in explicitly. Existing ring limits are preserved.
#[derive(Default)]
pub struct VisualLogPlugin;

impl Plugin for VisualLogPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VisualLog>()
            .add_systems(First, advance_frame.in_set(VisualLogFrame));
    }
}

fn advance_frame(mut log: ResMut<VisualLog>) {
    let next = log.frame().unwrap_or(0).saturating_add(1);
    log.begin_frame(next);
}

/// Records a line, text, or a `VisualShape` into a mutable recorder.
///
/// Forms: `debug_log!(entity, line, log, from, to)`,
/// `debug_log!(entity, text, log, at, message)` and `debug_log!(entity, shape, log, shape)`.
/// Returns unit. Without `visual-log`, arguments are neither evaluated nor type-checked.
#[macro_export]
macro_rules! debug_log {
    ($entity:expr, line, $log:expr, $from:expr, $to:expr $(,)?) => {{
        let _ = ($log).record(
            $entity,
            $crate::visual_log::VisualShape::Line { from: $from, to: $to },
        );
    }};
    ($entity:expr, text, $log:expr, $at:expr, $text:expr $(,)?) => {{
        let _ = ($log).record(
            $entity,
            $crate::visual_log::VisualShape::Text {
                at: $at,
                text: ($text).to_string(),
            },
        );
    }};
    ($entity:expr, shape, $log:expr, $shape:expr $(,)?) => {{
        let _ = ($log).record($entity, $shape);
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_log_frames_ten_to_twenty_and_bounded_ring() {
        let mut world = World::new();
        let entity = world.spawn_empty().id();
        let mut log = VisualLog::new(16, 4);
        for frame in 0..=25 {
            log.begin_frame(frame);
            if (10..=20).contains(&frame) {
                crate::debug_log!(entity, line, log, Vec3::ZERO, Vec3::X);
            }
        }
        let entries = log.entries(15, &[entity]);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].shape,
            VisualShape::Line {
                from: Vec3::ZERO,
                to: Vec3::X
            }
        );
        assert!(log.entries(21, &[entity]).is_empty());
        assert_eq!(log.frame_count(), 16);
        for frame in 26..100 {
            log.begin_frame(frame);
            for _ in 0..10 {
                crate::debug_log!(entity, line, log, Vec3::ZERO, Vec3::X);
            }
        }
        assert_eq!(log.frame_count(), 16);
        assert_eq!(log.entry_count(), 64);
        assert!(log.entries(15, &[entity]).is_empty());
    }

    #[test]
    fn visual_log_bounds_utf8_and_rejects_non_finite_shapes() {
        let mut world = World::new();
        let entity = world.spawn_empty().id();
        let mut log = VisualLog::new(0, 0);
        log.begin_frame(15);
        crate::debug_log!(entity, text, log, Vec3::ZERO, "€".repeat(1000));
        let entries = log.entries(15, &[entity]);
        let VisualShape::Text { text, .. } = &entries[0].shape else {
            panic!("text");
        };
        assert_eq!(text.len(), 510);
        assert!(text.capacity() <= MAX_TEXT_BYTES);
        log.begin_frame(15);
        assert_eq!(log.entry_count(), 1, "repeated frame retains entries");
        crate::debug_log!(entity, line, log, Vec3::ZERO, Vec3::X);
        assert_eq!(log.dropped(), 1);
        log.begin_frame(14);
        assert_eq!(log.entry_count(), 0, "backwards frame resets session");
        crate::debug_log!(entity, line, log, Vec3::NAN, Vec3::X);
        assert_eq!(log.entry_count(), 0);
        assert_eq!(log.dropped(), 1);
    }

    #[test]
    fn visual_log_snapshot_shares_frames_but_later_writes_do_not_change_history() {
        let mut world = World::new();
        let entity = world.spawn_empty().id();
        let mut live = VisualLog::new(2, 2);
        live.begin_frame(15);
        crate::debug_log!(entity, line, live, Vec3::ZERO, Vec3::X);
        let snapshot = live.clone();
        assert!(Arc::ptr_eq(&live.frames[0], &snapshot.frames[0]));
        crate::debug_log!(entity, text, live, Vec3::ZERO, "second");
        assert_eq!(snapshot.entry_count(), 1);
        assert_eq!(snapshot.entries(15, &[entity]).len(), 1);
        assert_eq!(live.entry_count(), 2);
        live.begin_frame(16);
        live.begin_frame(17);
        assert!(live.entries(15, &[entity]).is_empty());
        assert_eq!(snapshot.entries(15, &[entity]).len(), 1);
    }
}
