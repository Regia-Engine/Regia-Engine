// SPDX-License-Identifier: MPL-2.0
//! Montage playback as a pure function of elapsed steps (`docs/specs/73` § 6.7).
//!
//! [`MontagePlayer`](crate::montage_player::MontagePlayer) advances by frame time and answers to
//! live calls (`jump_to_section`, branch actions, warp targets), which is right for a rig on screen
//! and useless for a networked character: a tick re-run after a correction has to produce the same
//! motion as the first run, and the server has to produce it too. [`MontagePlayback`] follows the
//! same timeline rules (rate, rate curve, section links, end of montage) from a replicated start
//! (montage, section, rate) and answers "which montage time is step `n`" and "what root motion does
//! step `n` move". It reads no clock. The value of a step is a function of its index: a private
//! cache only remembers cursors already walked, so a sequential tick and a short resim do not
//! replay the whole play (ADR-5100).
//!
//! Differences from the player, by design:
//!
//! - Motion is at full weight (blends only fade the pose, so a vault covers the clip's whole
//!   distance on both sides).
//! - Motion warping does not apply (warp targets are live per-rig state).
//! - Only links from the asset are followed. A game that changes section from predicted code
//!   starts a new playback at that tick.
//! - There is no reverse playback. A rate that is not positive and finite is stored as 0: the
//!   cursor stays at the start and [`MontagePlayback::root_motion_of_step`] returns `None`.
//!   The player's `reverse: true` walks time backward; this type does not. The replicated play
//!   state stores `rate` and not a reverse flag, and playback is built from that rate, so a
//!   reversed montage's predicted root motion does not run backward.
//! - One step is not capped at the player's 64 section crossings. The walk follows links until
//!   the step's montage seconds are consumed. A section that jumps to itself can cross many
//!   more times than the player does in one frame.
//! - A `step` that is negative or not finite is stored as 0, so the playback does not advance.
//!   (`f32::max` keeps `+∞`, which would never return from a self-jump.)

use std::sync::{Arc, Mutex, MutexGuard};

use regia_core::math::{Quat, Vec3};

use crate::montage::{ClipSpan, Montage, MontageLayout, SectionNext};

/// Steps between stored cursors. A resim replays at most this many steps (the net cap is 32).
const CHECKPOINT_EVERY: u32 = 32;

/// Checkpoints kept on one playback. Eight covers a play much longer than one resim window.
const CHECKPOINT_KEEP: usize = 8;

/// Sections shorter than this cannot be looped through.
const TIME_EPSILON: f32 = 1.0e-5;

/// Fastest combined play rate a [`MontagePlayback`] steps at; a faster one plays at this rate.
///
/// The rate arrives replicated (`NetMontagePlay::rate`). Unbounded, a huge finite rate made one
/// step walk a self-jumping section billions of times, and past 2^24 laps `remaining -= lap`
/// stopped changing in f32, so the walk never ended (audit corr-animation).
pub const MAX_PLAYBACK_RATE: f32 = 1000.0;

/// Most timeline stretches one [`MontagePlayback`] walk crosses before it stops early, a
/// backstop for a rate curve that still asks for an unbounded walk.
const MAX_WALK_PIECES: u32 = 1 << 20;

/// Where a playback is on the montage timeline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MontageCursor {
    /// The section it is in.
    pub section: usize,
    /// Montage time, seconds.
    pub time: f32,
    /// The montage reached its end at or before this point.
    pub ended: bool,
}

/// One montage's playback from a fixed start, stepped by a fixed `step` of wall seconds.
///
/// The cache is private and is not part of the step's value. [`Clone`] copies the cursors and
/// drops the scratch vectors.
pub struct MontagePlayback {
    montage: Arc<Montage>,
    layout: Arc<MontageLayout>,
    start: MontageCursor,
    rate: f32,
    step: f32,
    cache: Mutex<PlaybackCache>,
}

/// Cursors already walked, plus the scratch a root-motion sample reuses.
struct PlaybackCache {
    /// Step whose cursor is `cursor`, once a query has stored one.
    cursor_step: Option<u32>,
    cursor: MontageCursor,
    /// `(step, cursor)` ascending, at most [`CHECKPOINT_KEEP`].
    checkpoints: Vec<(u32, MontageCursor)>,
    cuts: Vec<f32>,
    spans: Vec<ClipSpan>,
}

impl Default for PlaybackCache {
    fn default() -> Self {
        Self {
            cursor_step: None,
            cursor: MontageCursor {
                section: 0,
                time: 0.0,
                ended: false,
            },
            checkpoints: Vec::with_capacity(CHECKPOINT_KEEP),
            cuts: Vec::with_capacity(4),
            spans: Vec::with_capacity(4),
        }
    }
}

impl PlaybackCache {
    fn new() -> Self {
        Self::default()
    }
}

impl Clone for MontagePlayback {
    fn clone(&self) -> Self {
        let cache = self.lock();
        Self {
            montage: Arc::clone(&self.montage),
            layout: Arc::clone(&self.layout),
            start: self.start,
            rate: self.rate,
            step: self.step,
            cache: Mutex::new(PlaybackCache {
                cursor_step: cache.cursor_step,
                cursor: cache.cursor,
                checkpoints: cache.checkpoints.clone(),
                cuts: Vec::with_capacity(4),
                spans: Vec::with_capacity(4),
            }),
        }
    }
}

impl std::fmt::Debug for MontagePlayback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cache = self.lock();
        f.debug_struct("MontagePlayback")
            .field("montage", &self.montage)
            .field("layout", &self.layout)
            .field("start", &self.start)
            .field("rate", &self.rate)
            .field("step", &self.step)
            .field("cached_step", &cache.cursor_step)
            .finish()
    }
}

impl MontagePlayback {
    /// A playback of `montage` from the start of `section` (clamped to the last section), plus
    /// `start_offset` montage seconds, at the play call's `rate` on top of the montage's own,
    /// stepped by `step` seconds. A rate that is not positive and finite, or a `step` that is
    /// negative or not finite, plays nothing (the cursor stays at the start). A combined rate
    /// above [`MAX_PLAYBACK_RATE`] plays at that rate.
    ///
    /// A `start_offset` that reaches past the end of `section` follows the section links the
    /// way stepping does, so the start cursor's `section` and `time` name the same place: a
    /// `Continue` link carries on in the next section, a `Jump` link carries the rest of the
    /// offset into its target, and an offset that runs off the montage starts ended at the
    /// montage end.
    pub fn new(
        montage: Arc<Montage>,
        layout: Arc<MontageLayout>,
        section: usize,
        start_offset: f32,
        rate: f32,
        step: f32,
    ) -> Self {
        let section = section.min(montage.section_count().saturating_sub(1));
        let rate = rate * montage.rate;
        let mut playback = Self {
            start: MontageCursor {
                section,
                time: montage.section_start(section),
                ended: false,
            },
            montage,
            layout,
            rate: if rate.is_finite() && rate > 0.0 {
                rate.min(MAX_PLAYBACK_RATE)
            } else {
                0.0
            },
            step: if step.is_finite() { step.max(0.0) } else { 0.0 },
            cache: Mutex::new(PlaybackCache::new()),
        };
        let offset = start_offset.max(0.0);
        let inside = playback.start.time + offset;
        if inside < playback.layout.section_end(section) - TIME_EPSILON {
            // The common case, and the same sum as before the links were followed.
            playback.start.time = inside;
        } else {
            // audit-g animation3 finding 7: the offset reaches the section end, so the links
            // decide where it lands. `walk` is the rule stepping uses, and it is bounded.
            playback.start = playback.walk(playback.start, f64::from(offset), |_, _| {});
        }
        playback
    }

    /// The cache lock. A poisoned lock still holds the cursors a resim needs, so the data is kept.
    fn lock(&self) -> MutexGuard<'_, PlaybackCache> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Montage seconds one step covers from `time`.
    fn seconds_of_step(&self, time: f32) -> f32 {
        match &self.montage.rate_curve {
            Some(curve) => curve.advance(time, self.step, self.rate),
            None => self.step * self.rate,
        }
    }

    /// Walks `amount` montage seconds from `cursor` along the section links, calling `piece` with
    /// each stretch of the timeline crossed. `amount` is `f64`: past 2^24 steps the f32 products
    /// of the elapsed time quantize to the step (the cursor froze), and a huge finite `remaining`
    /// keeps enough precision to count down (audit-g animation3 findings 3 and 4).
    fn walk(&self, mut cursor: MontageCursor, amount: f64, mut piece: impl FnMut(f32, f32)) -> MontageCursor {
        let mut remaining = amount;
        let mut pieces = 0u32;
        while !cursor.ended && remaining > 0.0 && pieces < MAX_WALK_PIECES {
            pieces += 1;
            let section_end = f64::from(self.layout.section_end(cursor.section));
            let time = f64::from(cursor.time);
            let goal = (time + remaining).min(section_end);
            if goal > time {
                piece(cursor.time, goal as f32);
                remaining -= goal - time;
                cursor.time = goal as f32;
            }
            if f64::from(cursor.time) < section_end - f64::from(TIME_EPSILON) {
                break;
            }
            match self.montage.section_next_ref(cursor.section) {
                SectionNext::Continue if cursor.section + 1 < self.montage.section_count() => {
                    cursor.section += 1;
                }
                SectionNext::Continue | SectionNext::Stop => cursor.ended = true,
                SectionNext::Jump(target) => {
                    #[cfg(test)]
                    crate::montage::perf_work::note_jump();
                    match self.montage.section_index(target) {
                        Some(index)
                            if self.layout.section_end(index) - self.montage.section_start(index)
                                > TIME_EPSILON =>
                        {
                            cursor.section = index;
                            let start = f64::from(self.montage.section_start(index));
                            cursor.time = start as f32;
                            // A `remaining` past the walk backstop in whole loops of the target
                            // is consumed with one division, so a garbage huge step returns
                            // instead of spending [`MAX_WALK_PIECES`] crossings on every query
                            // (audit-g animation3 finding 4). Smaller steps walk every crossing
                            // so each loop's root motion is still sampled.
                            let len = f64::from(self.layout.section_end(index)) - start;
                            if len > 0.0 {
                                let loops = (remaining / len).floor();
                                if loops >= f64::from(MAX_WALK_PIECES) {
                                    remaining -= loops * len;
                                }
                            }
                        }
                        _ => cursor.ended = true,
                    }
                }
            }
        }
        cursor
    }

    /// Where the playback is after `steps` steps.
    ///
    /// Every step, with or without a rate curve, advances by the same per-step amount
    /// [`root_motion_of_step`](Self::root_motion_of_step) samples. A stored cursor or checkpoint
    /// at or before `steps` is the start of that walk, so a sequential tick costs one step and a
    /// resim costs the steps since the checkpoint (ADR-5100).
    pub fn cursor_after(&self, steps: u32) -> MontageCursor {
        if self.rate <= 0.0 || steps == 0 {
            return self.start;
        }
        let cache = self.lock();
        if cache.cursor_step == Some(steps) {
            return cache.cursor;
        }
        if cache.cursor.ended
            && let Some(n) = cache.cursor_step
            && steps >= n
        {
            return cache.cursor;
        }
        if let Some((_, cursor)) = cache.checkpoints.iter().rev().find(|(n, _)| *n == steps) {
            return *cursor;
        }
        let mut floor_step = 0u32;
        let mut floor = self.start;
        if let Some(n) = cache.cursor_step
            && n <= steps
            && !cache.cursor.ended
        {
            floor_step = n;
            floor = cache.cursor;
        }
        for &(n, cursor) in &cache.checkpoints {
            if cursor.ended && steps >= n {
                return cursor;
            }
            if n <= steps && n >= floor_step && !cursor.ended {
                floor_step = n;
                floor = cursor;
            }
        }
        drop(cache);
        let cursor = self.integrate(floor, floor_step, steps);
        self.store_cursor(steps, cursor);
        cursor
    }

    /// Walks steps `from..to` from `cursor`. One step's amount is what
    /// [`root_motion_of_step`](Self::root_motion_of_step) samples, so a cold query and a resumed
    /// one land on the same time.
    fn integrate(&self, mut cursor: MontageCursor, from: u32, to: u32) -> MontageCursor {
        if cursor.ended || from >= to {
            return cursor;
        }
        let scale = f64::from(self.step) * f64::from(self.rate);
        for i in from..to {
            if cursor.ended {
                break;
            }
            let amount = match &self.montage.rate_curve {
                Some(_) => f64::from(self.seconds_of_step(cursor.time)),
                None => {
                    // f64 so the step length survives past 2^24 steps, where the f32 products of
                    // the two ends collapse to the same value and the step would come out zero.
                    let done = f64::from(i) * scale;
                    (f64::from(i) + 1.0) * scale - done
                }
            };
            cursor = self.walk(cursor, amount, |_, _| {});
        }
        cursor
    }

    /// Remembers `cursor` as the answer for `steps`, and a checkpoint every
    /// `CHECKPOINT_EVERY` steps.
    fn store_cursor(&self, steps: u32, cursor: MontageCursor) {
        let mut cache = self.lock();
        cache.cursor_step = Some(steps);
        cache.cursor = cursor;
        if !steps.is_multiple_of(CHECKPOINT_EVERY) || cache.checkpoints.iter().any(|(n, _)| *n == steps) {
            return;
        }
        if cache.checkpoints.len() == CHECKPOINT_KEEP {
            cache.checkpoints.remove(0);
        }
        cache.checkpoints.push((steps, cursor));
    }

    /// The root motion of step `index` (from `index` steps after the start to `index + 1`), in rig
    /// object space. `None` once the montage has ended, and when it has no root motion.
    pub fn root_motion_of_step(&self, index: u32) -> Option<(Vec3, Quat)> {
        let _span = tracing::trace_span!("montage_root_motion_step", index).entered();
        if !self.montage.root_motion || self.rate <= 0.0 {
            return None;
        }
        let cursor = self.cursor_after(index);
        if cursor.ended {
            return None;
        }
        // The same per-step amount `integrate` walks, so the sample's end cursor is
        // `cursor_after(index + 1)`.
        let amount = match &self.montage.rate_curve {
            Some(_) => f64::from(self.seconds_of_step(cursor.time)),
            None => {
                // f64 so the step length survives past 2^24 steps, where the f32 products of
                // the two ends collapse to the same value and the step would come out zero.
                let scale = f64::from(self.step) * f64::from(self.rate);
                let done = f64::from(index) * scale;
                (f64::from(index) + 1.0) * scale - done
            }
        };
        let mut motion = (Vec3::ZERO, Quat::IDENTITY);
        let mut cache = self.lock();
        let mut cuts = std::mem::take(&mut cache.cuts);
        let mut spans = std::mem::take(&mut cache.spans);
        drop(cache);
        let end = self.walk(cursor, amount, |a, b| {
            let (t, r) = self.layout.root_motion_between_reuse(
                a,
                b,
                &self.montage.root_motion_off,
                &mut cuts,
                &mut spans,
            );
            motion = (motion.0 + motion.1 * t, (motion.1 * r).normalize());
        });
        let mut cache = self.lock();
        cache.cuts = cuts;
        cache.spans = spans;
        drop(cache);
        self.store_cursor(index + 1, end);
        Some(motion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::{AnimationClip, JointTrack, RootMotionMode, RootMotionTrack, TrackCurve};
    use crate::montage::{MontageSection, RateCurve, RateInterpolation};

    const STEP: f32 = 1.0 / 60.0;

    /// A clip whose root walks 2 m/s along -Z for 1 s (XZ root motion).
    fn walker() -> Arc<AnimationClip> {
        let mut clip = AnimationClip::empty("walk", 1.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![Vec3::ZERO, Vec3::new(0.0, 0.0, -2.0)],
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        clip.root_motion = Some(RootMotionTrack {
            joint: 0,
            mode: RootMotionMode::XZ,
        });
        Arc::new(clip)
    }

    fn playback(montage: Montage, section: usize, rate: f32) -> MontagePlayback {
        let clip = walker();
        let layout = montage.layout(|_| Some(clip.clone())).expect("layout");
        MontagePlayback::new(Arc::new(montage), Arc::new(layout), section, 0.0, rate, STEP)
    }

    fn sections(next_of_first: SectionNext) -> Montage {
        let mut m = Montage::single("vault", "full_body", "walk");
        m.sections = vec![
            MontageSection {
                name: "a".into(),
                start: 0.0,
                next: next_of_first,
                follow: None,
            },
            MontageSection {
                name: "b".into(),
                start: 0.5,
                next: SectionNext::Continue,
                follow: None,
            },
        ];
        m
    }

    fn total(p: &MontagePlayback, steps: u32) -> f32 {
        (0..steps)
            .filter_map(|i| p.root_motion_of_step(i))
            .map(|(t, _)| -t.z)
            .sum()
    }

    /// Sixty steps of a one-second clip cover its whole motion, and the montage then ends.
    #[test]
    fn steps_add_up_to_the_clip() {
        let p = playback(Montage::single("vault", "full_body", "walk"), 0, 1.0);
        assert!((total(&p, 60) - 2.0).abs() < 1e-3, "{}", total(&p, 60));
        assert!(p.cursor_after(61).ended);
        assert_eq!(p.root_motion_of_step(61), None);
    }

    /// Audit (corr-animation): a huge replicated rate over a self-jumping section never finished
    /// a step (the f32 `remaining` stopped shrinking). It now plays at the capped rate.
    #[test]
    fn a_huge_rate_on_a_looping_section_finishes_its_step_at_the_capped_rate() {
        let huge = playback(sections(SectionNext::Jump("a".into())), 0, 1.0e30);
        let capped = playback(sections(SectionNext::Jump("a".into())), 0, MAX_PLAYBACK_RATE);
        let cursor = huge.cursor_after(1);
        assert!(!cursor.ended);
        assert_eq!(cursor, capped.cursor_after(1));
        assert_eq!(huge.root_motion_of_step(0), capped.root_motion_of_step(0));
    }

    /// The same step gives the same bits whatever was asked before (a resimulated tick).
    #[test]
    fn a_step_is_a_pure_function_of_its_index() {
        let p = playback(Montage::single("vault", "full_body", "walk"), 0, 1.0);
        let first: Vec<_> = (0..70).map(|i| p.root_motion_of_step(i)).collect();
        for i in (0..70).rev() {
            assert_eq!(p.root_motion_of_step(i), first[i as usize]);
        }
    }

    /// The rate shortens the time and keeps the distance.
    #[test]
    fn rate_scales_time() {
        let p = playback(Montage::single("vault", "full_body", "walk"), 0, 2.0);
        assert!((total(&p, 30) - 2.0).abs() < 1e-3, "{}", total(&p, 30));
        assert!(p.cursor_after(31).ended);
    }

    /// audit-v040 H2: a `Yaw` root-motion step through a pitched (Z-up-style) rotation must
    /// extract the twist with the deterministic kernel, so the sampled quaternion is
    /// bit-identical on every platform's libm. The expected value is rebuilt from
    /// `regia_core::math::det` alone (never the platform libm).
    #[test]
    fn yaw_step_extraction_is_pinned_and_platform_stable() {
        use regia_core::math::det;

        // A root that carries a constant pitch (a Z-up import) and gains 0.7 rad of yaw.
        let pitch = det::quat_from_rotation_x(0.4);
        let end = pitch * det::quat_from_rotation_y(0.7);
        let mut clip = AnimationClip::empty("yaw", 1.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Static(Vec3::ZERO),
            rotations: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![pitch, end],
                exact_rotation: true,
            },
            scales: TrackCurve::Static(Vec3::ONE),
        });
        clip.root_motion = Some(RootMotionTrack {
            joint: 0,
            mode: RootMotionMode::Yaw,
        });
        let clip = Arc::new(clip);
        let mut montage = Montage::single("turn", "full_body", "yaw");
        montage.root_motion = true;
        let layout = montage.layout(|_| Some(clip.clone())).expect("layout");
        let p = MontagePlayback::new(Arc::new(montage), Arc::new(layout), 0, 0.0, 1.0, STEP);

        let (translation, rotation) = p.root_motion_of_step(0).expect("step 0");
        assert_eq!(translation, Vec3::ZERO, "Yaw mode reports no translation");

        // The same pipeline from the det kernel: sample the root track at the step end, take
        // the delta from the step start, extract the +Y twist, rebuild the yaw quaternion.
        let sampled = det::quat_slerp(pitch, end, (STEP / 1.0).min(1.0));
        let delta = (sampled * pitch.inverse()).normalize();
        let q = if delta.w < 0.0 { -delta } else { delta };
        let yaw = 2.0 * det::atan2(q.y, q.w);
        let expected = det::quat_from_rotation_y(yaw);
        assert_eq!(
            rotation, expected,
            "the Yaw step is exactly the det-pipeline quaternion: {rotation:?} vs {expected:?}"
        );
        // Sanity: the step advances a 60th of the twist the full pitched delta carries about
        // +Y. The delta `Rx(0.4)·Ry(0.7)` twists about a tilted axis, so the +Y component of
        // its twist is 0.7·cos(0.4), not 0.7 (a Z-up import, audit-v040 H2).
        assert!(
            (yaw * 60.0 - 0.7 * det::cos(0.4)).abs() < 1e-4,
            "the extracted yaw per step is the authored twist: {yaw}"
        );
    }

    /// `Continue` plays on, `Stop` ends at the section's end, a self-jump loops.
    #[test]
    fn section_links_are_followed() {
        let p = playback(sections(SectionNext::Continue), 0, 1.0);
        assert_eq!(p.cursor_after(45).section, 1);
        let p = playback(sections(SectionNext::Stop), 0, 1.0);
        assert!(p.cursor_after(31).ended);
        assert!((total(&p, 40) - 1.0).abs() < 1e-3);
        let p = playback(sections(SectionNext::Jump("a".into())), 0, 1.0);
        let c = p.cursor_after(600);
        assert_eq!(c.section, 0);
        assert!(!c.ended);
        assert!((total(&p, 90) - 3.0).abs() < 1e-2, "{}", total(&p, 90));

        // audit-g animation3 finding 3: past 2^24 steps the f32 elapsed-time product quantizes
        // to the step and the cursor freezes. The cursor must keep moving and the step must
        // carry its root motion.
        let frozen = playback(sections(SectionNext::Jump("a".into())), 0, 1.0);
        let c0 = frozen.cursor_after(16_777_216);
        let c1 = frozen.cursor_after(16_777_217);
        assert_ne!(
            c0, c1,
            "the cursor keeps moving past 2^24 steps: {c0:?} vs {c1:?}"
        );
        let step = frozen
            .root_motion_of_step(16_777_216)
            .expect("a looping section reports step motion");
        assert!(step.0.length() > 0.0, "the step is not zero: {step:?}");
    }

    /// audit-g animation3 finding 3 on the step integrator: every step of a playback without a
    /// rate curve covers the same montage time, however long the playback has run. Half an hour
    /// in (step 108 000 at 60 Hz) the `f32` difference of the elapsed times was already a few
    /// parts in a thousand off a step, and at step 2^24 it was zero.
    #[test]
    fn a_late_step_covers_the_same_time_as_the_first() {
        let p = playback(sections(SectionNext::Jump("a".into())), 0, 1.0);
        let first = p.root_motion_of_step(0).expect("step 0").0.length();
        assert!(first > 0.0);
        for index in [108_000_u32, 1_000_003, 16_777_216] {
            // Steps that cross the loop seam sample two stretches; pick one inside the section.
            let index = (index..index + 30)
                .find(|&i| {
                    let (a, b) = (p.cursor_after(i), p.cursor_after(i + 1));
                    b.time > a.time
                })
                .expect("a step inside the section");
            let late = p.root_motion_of_step(index).expect("a late step").0.length();
            assert!(
                (late - first).abs() <= first * 1.0e-3,
                "step {index} moves {late}, step 0 moves {first}"
            );
        }
    }

    /// audit-g animation3 finding 4: a non-finite step plays nothing and a huge finite step
    /// returns (the walk is bounded); neither hangs.
    #[test]
    fn a_non_finite_or_huge_step_does_not_hang() {
        let m = sections(SectionNext::Jump("a".into()));
        let clip = walker();
        let layout = m.layout(|_| Some(clip.clone())).expect("layout");
        let p = MontagePlayback::new(Arc::new(m), Arc::new(layout), 0, 0.0, 1.0, f32::INFINITY);
        assert_eq!(
            p.cursor_after(1),
            p.cursor_after(0),
            "an infinite step plays nothing"
        );
        let m = sections(SectionNext::Jump("a".into()));
        let clip = walker();
        let layout = m.layout(|_| Some(clip.clone())).expect("layout");
        let p = MontagePlayback::new(Arc::new(m), Arc::new(layout), 0, 0.0, 1.0, 1e20);
        let c = p.cursor_after(1);
        assert!(
            !c.ended && c.section == 0,
            "a huge finite step still lands inside the looping section: {c:?}"
        );
    }

    /// Re-landed audit-g animation3 finding 3 on the ADR-5100 cache: past 2^24 steps the f32
    /// products of the elapsed time quantize to the step and the cursor freezes. The cursor must
    /// keep moving and the step must carry its root motion.
    #[test]
    fn the_cursor_keeps_moving_past_2_pow_24_steps() {
        let p = playback(sections(SectionNext::Jump("a".into())), 0, 1.0);
        let c0 = p.cursor_after(16_777_216);
        let c1 = p.cursor_after(16_777_217);
        assert_ne!(
            c0, c1,
            "the cursor keeps moving past 2^24 steps: {c0:?} vs {c1:?}"
        );
        let step = p
            .root_motion_of_step(16_777_216)
            .expect("a looping section reports step motion");
        assert!(step.0.length() > 0.0, "the step is not zero: {step:?}");
    }

    /// Re-landed audit-g animation3 finding 4 on the ADR-5100 walk: a huge finite step must
    /// return without walking every loop of a self-jumping section (the f32 `remaining` never
    /// shrinks past the walk backstop, so the backstop is spent on every step).
    #[test]
    fn a_huge_finite_step_does_not_walk_every_loop() {
        let m = sections(SectionNext::Jump("a".into()));
        let clip = walker();
        let layout = m.layout(|_| Some(clip.clone())).expect("layout");
        let p = MontagePlayback::new(Arc::new(m), Arc::new(layout), 0, 0.0, 1.0, 1e20);
        crate::montage::perf_work::reset();
        let c = p.cursor_after(1);
        let jumps = crate::montage::perf_work::jumps();
        assert!(
            jumps <= 4,
            "a huge finite step consumed whole loops, not one backstop of links: {jumps} jumps"
        );
        assert!(
            !c.ended && c.section == 0,
            "a huge finite step still lands inside the looping section: {c:?}"
        );
    }

    /// Re-landed audit-g animation3 finding 7: a start offset that crosses a section boundary
    /// resolves through the links, so the step-0 cursor's `section` and `time` name the same
    /// place; an offset into a self-jumping section wraps into that section instead of
    /// overflowing the hop bound with a mismatched cursor; an offset past the montage end
    /// starts ended when no link loops.
    #[test]
    fn relanded_start_offset_past_a_section_end_resolves_the_section() {
        let m = sections(SectionNext::Continue);
        let clip = walker();
        let layout = m.layout(|_| Some(clip.clone())).expect("layout");
        let p = MontagePlayback::new(Arc::new(m), Arc::new(layout), 0, 0.6, 1.0, STEP);
        let start = p.cursor_after(0);
        assert_eq!(
            start,
            MontageCursor {
                section: 1,
                time: 0.6,
                ended: false
            },
            "time 0.6 lies in section 1"
        );

        // Section a ([0, 0.5]) jumps to itself: an offset of 0.7 wraps to 0.2 in section a.
        let m = sections(SectionNext::Jump("a".into()));
        let clip = walker();
        let layout = m.layout(|_| Some(clip.clone())).expect("layout");
        let p = MontagePlayback::new(Arc::new(m), Arc::new(layout), 0, 0.7, 1.0, STEP);
        let start = p.cursor_after(0);
        assert_eq!(start.section, 0, "the offset wraps into the looping section");
        assert!(
            (start.time - 0.2).abs() < 1e-5,
            "the wrapped offset stays inside section a: {start:?}"
        );

        // Past the montage end with only `Continue` links: clamped and already ended.
        let m = sections(SectionNext::Continue);
        let clip = walker();
        let layout = m.layout(|_| Some(clip.clone())).expect("layout");
        let p = MontagePlayback::new(Arc::new(m), Arc::new(layout), 0, 5.0, 1.0, STEP);
        assert_eq!(
            p.cursor_after(0),
            MontageCursor {
                section: 1,
                time: 1.0,
                ended: true
            }
        );
    }

    /// A negative rate is not reverse playback: the cursor stays and no root motion is sampled.
    #[test]
    fn a_negative_rate_plays_nothing() {
        let p = playback(Montage::single("vault", "full_body", "walk"), 0, -1.0);
        assert_eq!(p.cursor_after(60).time, 0.0);
        assert!(!p.cursor_after(60).ended);
        assert_eq!(p.root_motion_of_step(0), None);
        assert_eq!(total(&p, 60), 0.0);
    }

    /// `+∞` is not a step. It must return the start cursor instead of looping a self-jump forever.
    #[test]
    fn a_non_finite_step_plays_nothing() {
        let clip = walker();
        let montage = Montage::single("vault", "full_body", "walk");
        let layout = montage.layout(|_| Some(clip.clone())).expect("layout");
        let p = MontagePlayback::new(Arc::new(montage), Arc::new(layout), 0, 0.0, 1.0, f32::INFINITY);
        let start = p.cursor_after(0);
        assert_eq!(p.cursor_after(1), start);
        assert!(!start.ended);
        let (translation, rotation) = p
            .root_motion_of_step(0)
            .expect("a zero step is still on the montage");
        assert_eq!(translation, Vec3::ZERO);
        assert_eq!(rotation, Quat::IDENTITY);
    }

    /// The player stops after 64 section crossings. Playback consumes the whole step, so a short
    /// self-jump covers more montage time than 64 crossings of that section.
    #[test]
    fn a_short_self_jump_consumes_the_whole_step() {
        let clip = walker();
        let mut montage = Montage::single("vault", "full_body", "walk");
        montage.root_motion = true;
        // 1/8192 and 1/64 are exact in f32. One step is 128 crossings, past the player's 64.
        let section_len = 1.0 / 8192.0;
        let step = 1.0 / 64.0;
        montage.sections = vec![
            MontageSection {
                name: "a".into(),
                start: 0.0,
                next: SectionNext::Jump("a".into()),
                follow: None,
            },
            MontageSection {
                name: "b".into(),
                start: section_len,
                next: SectionNext::Stop,
                follow: None,
            },
        ];
        let layout = montage.layout(|_| Some(clip.clone())).expect("layout");
        let p = MontagePlayback::new(Arc::new(montage), Arc::new(layout), 0, 0.0, 1.0, step);
        let (translation, _) = p.root_motion_of_step(0).expect("step");
        let travelled = -translation.z;
        // The clip walks 2 m/s. The whole step is 2/64 m. Sixty-four crossings of this section
        // would be 2/128 m.
        assert!(
            (travelled - 2.0 / 64.0).abs() < 1e-3,
            "consumed the step ({travelled}), not 64 crossings"
        );
        assert!(
            (travelled - 2.0 / 128.0).abs() > 1e-3,
            "must not stop at the player's 64-crossing cap"
        );
    }

    /// Starting in a later section starts at its time.
    #[test]
    fn start_section_offsets_the_timeline() {
        let p = playback(sections(SectionNext::Continue), 1, 1.0);
        assert_eq!(p.cursor_after(0).time, 0.5);
        assert!((total(&p, 40) - 1.0).abs() < 1e-3);
    }

    /// audit-g animation3 finding 7: a start offset that crosses a section boundary resolves
    /// through the links, so the step-0 cursor's `section` and `time` name the same place, and
    /// an offset past the montage end starts ended when no link loops.
    #[test]
    fn a_start_offset_past_a_section_end_resolves_the_section() {
        let m = sections(SectionNext::Continue);
        let clip = walker();
        let layout = m.layout(|_| Some(clip.clone())).expect("layout");
        let p = MontagePlayback::new(Arc::new(m), Arc::new(layout), 0, 0.6, 1.0, STEP);
        assert_eq!(
            p.cursor_after(0),
            MontageCursor {
                section: 1,
                time: 0.6,
                ended: false
            },
            "time 0.6 lies in section 1"
        );
        // Past the montage end with only `Continue` links: clamped and already ended.
        let m = sections(SectionNext::Continue);
        let clip = walker();
        let layout = m.layout(|_| Some(clip.clone())).expect("layout");
        let p = MontagePlayback::new(Arc::new(m), Arc::new(layout), 0, 5.0, 1.0, STEP);
        assert_eq!(
            p.cursor_after(0),
            MontageCursor {
                section: 1,
                time: 1.0,
                ended: true
            }
        );
    }

    /// integ-v048c, the port of finding 7 onto the step integrator: a start offset that runs
    /// past a `Jump` link lands where stepping would have, in the jump's target with the rest
    /// of the offset, and never on montage time the jump skips. Three sections, the first jumps
    /// over the second: 0.1 s past the end of "a" is 0.1 s into "c", not 0.6 s (inside "b").
    #[test]
    fn a_start_offset_past_a_jump_lands_in_the_jump_target() {
        let jumping = || {
            let mut m = Montage::single("vault", "full_body", "walk");
            m.sections = vec![
                MontageSection {
                    name: "a".into(),
                    start: 0.0,
                    next: SectionNext::Jump("c".into()),
                    follow: None,
                },
                MontageSection {
                    name: "b".into(),
                    start: 0.25,
                    next: SectionNext::Continue,
                    follow: None,
                },
                MontageSection {
                    name: "c".into(),
                    start: 0.75,
                    next: SectionNext::Stop,
                    follow: None,
                },
            ];
            m
        };
        let start_at = |offset: f32| {
            let m = jumping();
            let clip = walker();
            let layout = m.layout(|_| Some(clip.clone())).expect("layout");
            MontagePlayback::new(Arc::new(m), Arc::new(layout), 0, offset, 1.0, STEP).cursor_after(0)
        };
        let c = start_at(0.35);
        assert_eq!(c.section, 2, "{c:?}");
        assert!((c.time - 0.85).abs() < 1.0e-5, "{c:?}");
        assert!(!c.ended);
        // The same place stepping reaches: 21 steps of 1/60 s from the start are 0.35 s.
        let stepped = playback(jumping(), 0, 1.0).cursor_after(21);
        assert_eq!(stepped.section, 2, "{stepped:?}");
        assert!((stepped.time - c.time).abs() < 1.0e-4, "{stepped:?} vs {c:?}");
        // An offset inside the first section is the plain sum, as before.
        assert_eq!(
            start_at(0.125),
            MontageCursor {
                section: 0,
                time: 0.125,
                ended: false
            }
        );
        // Past everything the links reach: ended at the end of "c".
        let c = start_at(3.0);
        assert!(c.ended && c.section == 2, "{c:?}");
        assert!((c.time - 1.0).abs() < 1.0e-5, "{c:?}");
    }

    /// A rate curve integrates step by step, the same with any query order.
    #[test]
    fn rate_curve_is_integrated_by_steps() {
        let mut m = Montage::single("vault", "full_body", "walk");
        m.rate_curve = Some(RateCurve {
            keys: vec![(0.0, 1.0), (1.0, 3.0)],
            interpolation: RateInterpolation::Linear,
        });
        let p = playback(m, 0, 1.0);
        let once = total(&p, 120);
        assert!((once - 2.0).abs() < 1e-2, "{once}");
        assert_eq!(total(&p, 120), once);
    }

    /// Audit M2 (read-only determinism audit of `animation3`): the yaw root-motion query is the
    /// lockstep contract, and every trig evaluation on its path — the slerp between rotation
    /// keys, the twist extraction, the yaw quaternion — runs on the software kernels, so the
    /// bits are the contract on every target (glibc, bionic, the Windows CRT).
    #[test]
    fn yaw_root_motion_step_is_pinned_to_software_bits() {
        // 1 s clip turning 0 → π/2 about +Y, sampled with slerp between its two keys. The end
        // key is written as literal bits (what an authored key carries), not
        // `Quat::from_rotation_y`, whose platform sin_cos would make the input itself
        // platform-dependent.
        let quarter = std::f32::consts::FRAC_PI_2;
        let sqrt_half = 0.707_106_77_f32; // nearest f32 to √2/2 (sin(π/4) == cos(π/4))
        let mut clip = AnimationClip::empty("turn", 1.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Static(Vec3::ZERO),
            rotations: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![Quat::IDENTITY, Quat::from_xyzw(0.0, sqrt_half, 0.0, sqrt_half)],
                exact_rotation: true,
            },
            scales: TrackCurve::Static(Vec3::ONE),
        });
        clip.root_motion = Some(RootMotionTrack {
            joint: 0,
            mode: RootMotionMode::Yaw,
        });
        let clip = Arc::new(clip);
        // One section that jumps to itself: the playback loops through the clip end.
        let mut m = Montage::single("turn", "full_body", "turn");
        m.sections = vec![MontageSection {
            name: "a".into(),
            start: 0.0,
            next: SectionNext::Jump("a".into()),
            follow: None,
        }];
        m.root_motion = true;
        let layout = m.layout(|_| Some(clip.clone())).expect("layout");
        // 1/64 s steps are exact in f32, so step 63 ends exactly on the clip end and step 64
        // walks across the wrap: the boundary handling is part of the pin.
        let step64 = 1.0 / 64.0;
        let p = MontagePlayback::new(Arc::new(m), Arc::new(layout), 0, 0.0, 1.0, step64);

        let bits = |q: Quat| [q.x.to_bits(), q.y.to_bits(), q.z.to_bits(), q.w.to_bits()];
        let mid = p.root_motion_of_step(30).expect("step 30").1;
        assert_eq!(
            bits(mid),
            [0x0000_0000, 0x3c49_0e9f, 0x0000_0000, 0x3f7f_fb11],
            "step 30 (0.9 s of the turn) must be bit-identical on every target"
        );
        let across = p.root_motion_of_step(64).expect("step 64").1;
        assert_eq!(
            bits(across),
            [0x0000_0000, 0x3c49_0e90, 0x0000_0000, 0x3f7f_fb11],
            "step 64 walks across the clip end and must be bit-identical on every target"
        );
        // Accuracy guards, so the pins cannot silently pin a wrong value: each step turns
        // quarter/64, and 64 steps make the quarter turn in total.
        let step_angle = quarter / 64.0;
        assert!(
            (crate::compress::rotation_error(mid, Quat::IDENTITY) - step_angle).abs() < 1e-5,
            "step 30 angle"
        );
        assert!(
            (crate::compress::rotation_error(across, Quat::IDENTITY) - step_angle).abs() < 1e-5,
            "step 64 angle"
        );
        let total: f32 = (0..64)
            .filter_map(|i| p.root_motion_of_step(i))
            .map(|(_, q)| crate::clip::twist_yaw(q))
            .sum();
        assert!(
            (total - quarter).abs() < 1e-4,
            "64 steps sum to the quarter turn: {total}"
        );
    }

    /// One predicted tick: root motion of step `index`, then the cursor after the next step.
    fn tick(p: &MontagePlayback, index: u32) {
        let _ = p.root_motion_of_step(index);
        let _ = p.cursor_after(index + 1);
    }

    fn rate_curve_playback() -> MontagePlayback {
        let mut m = Montage::single("vault", "full_body", "walk");
        m.rate_curve = Some(RateCurve {
            keys: vec![(0.0, 1.0), (1.0, 3.0)],
            interpolation: RateInterpolation::Linear,
        });
        playback(m, 0, 1.0)
    }

    /// Audit perf G finding 1: steps `0..N` in order do work linear in `N`, and the step after
    /// its predecessor does a constant amount. A self-jump follows a linear number of links, and
    /// root motion does not build a fresh cut/span vec per step.
    #[test]
    fn perf_g_sequential_steps_do_linear_work() {
        const N: u32 = 180;
        let p = rate_curve_playback();
        crate::montage::perf_work::reset();
        for i in 0..N - 1 {
            tick(&p, i);
        }
        let before_last = crate::montage::perf_work::advances();
        tick(&p, N - 1);
        let last = crate::montage::perf_work::advances() - before_last;
        let advances = crate::montage::perf_work::advances();
        let curve_vecs = crate::montage::perf_work::root_vecs();

        let p = playback(sections(SectionNext::Jump("a".into())), 0, 1.0);
        crate::montage::perf_work::reset();
        for i in 0..N - 1 {
            tick(&p, i);
        }
        let jumps_before = crate::montage::perf_work::jumps();
        tick(&p, N - 1);
        let last_jumps = crate::montage::perf_work::jumps() - jumps_before;
        let jumps = crate::montage::perf_work::jumps();
        let jump_vecs = crate::montage::perf_work::root_vecs();

        let mut problems = Vec::new();
        if last > 4 {
            problems.push(format!(
                "rate-curve step {} did {last} RateCurve::advance calls after step {}",
                N - 1,
                N - 2
            ));
        }
        if advances > 3 * u64::from(N) {
            problems.push(format!(
                "rate-curve play of {N} steps did {advances} RateCurve::advance calls"
            ));
        }
        if jumps > 2 * u64::from(N) {
            problems.push(format!(
                "self-jump play of {N} steps followed {jumps} section links"
            ));
        }
        if last_jumps > 4 {
            problems.push(format!(
                "self-jump step {} followed {last_jumps} links after step {}",
                N - 1,
                N - 2
            ));
        }
        if curve_vecs != 0 || jump_vecs != 0 {
            problems.push(format!(
                "root_motion_between built fresh vecs {curve_vecs} (rate curve) and {jump_vecs} (self-jump) times"
            ));
        }
        assert!(
            problems.is_empty(),
            "sequential montage work is not linear: {}",
            problems.join("; ")
        );
    }

    /// A 32-step resim after a long play replays from the checkpoint, not from the start.
    #[test]
    fn perf_g_resim_window_replays_from_the_checkpoint() {
        const PLAYED: u32 = 400;
        const WINDOW: u32 = 32;
        let from = PLAYED - WINDOW;

        let p = rate_curve_playback();
        for i in 0..PLAYED {
            tick(&p, i);
        }
        crate::montage::perf_work::reset();
        for i in from..PLAYED {
            tick(&p, i);
        }
        let advances = crate::montage::perf_work::advances();

        let p = playback(sections(SectionNext::Jump("a".into())), 0, 1.0);
        for i in 0..PLAYED {
            tick(&p, i);
        }
        crate::montage::perf_work::reset();
        for i in from..PLAYED {
            tick(&p, i);
        }
        let jumps = crate::montage::perf_work::jumps();

        assert!(
            advances < 200 && jumps < 80,
            "resim of {WINDOW} steps after {PLAYED}: {advances} RateCurve::advance calls, {jumps} section-link follows"
        );
    }

    /// A cold query of step `n` matches the step a sequential play produced. Both orders are the
    /// lockstep contract (the purity test covers one playback object).
    #[test]
    fn perf_g_cold_query_matches_sequential_play() {
        let cases = [(false, false), (true, false), (false, true), (true, true)];
        for (curve, jump) in cases {
            let build = || {
                let mut m = if jump {
                    sections(SectionNext::Jump("a".into()))
                } else {
                    Montage::single("vault", "full_body", "walk")
                };
                if curve {
                    m.rate_curve = Some(RateCurve {
                        keys: vec![(0.0, 1.0), (1.0, 3.0)],
                        interpolation: RateInterpolation::Linear,
                    });
                }
                playback(m, 0, 1.0)
            };
            let sequential = build();
            let played: Vec<_> = (0..80).map(|i| sequential.root_motion_of_step(i)).collect();
            let cold = build();
            for i in 0..80 {
                assert_eq!(
                    cold.root_motion_of_step(i),
                    played[i as usize],
                    "curve={curve} jump={jump} step={i}"
                );
            }
        }
    }

    /// Root motion off windows and a montage without root motion move nothing.
    #[test]
    fn root_motion_flags_apply() {
        let mut m = Montage::single("vault", "full_body", "walk");
        m.root_motion = false;
        assert_eq!(playback(m, 0, 1.0).root_motion_of_step(0), None);
        let m = Montage::single("vault", "full_body", "walk");
        let p = playback(m, 0, 0.0);
        assert_eq!(p.root_motion_of_step(0), None);
        assert_eq!(p.cursor_after(100).time, 0.0);
    }

    #[test]
    fn a_huge_finite_step_stops_instead_of_spinning() {
        let clip = walker();
        let montage = sections(SectionNext::Jump("a".into()));
        let layout = montage.layout(|_| Some(clip.clone())).expect("layout");
        let p = MontagePlayback::new(Arc::new(montage), Arc::new(layout), 0, 0.0, 1.0, 1.0e7_f32);
        let cursor = p.cursor_after(1);
        assert!(!cursor.ended, "the walk stopped mid-play, not at the end");
        let again = p.cursor_after(1);
        assert_eq!(cursor, again, "the stopped cursor is deterministic");
        let motion = p.root_motion_of_step(0).expect("motion");
        assert!(motion.0.is_finite() && motion.1.is_finite(), "finite motion");
    }

    #[test]
    fn late_step_keeps_f64_phase_and_moves() {
        let p = playback(sections(SectionNext::Jump("a".into())), 0, 1.0);
        // 2^24: `steps as f32` and `(steps + 1) as f32` are the same number, so the f32
        // amount collapses to zero and the tick stops moving.
        let steps = 16_777_216u32;
        let elapsed = f64::from(steps) * f64::from(STEP);
        {
            let mut cache = p.lock();
            cache.cursor_step = Some(steps);
            cache.cursor = MontageCursor {
                section: 0,
                time: (elapsed % 0.5) as f32,
                ended: false,
            };
        }
        let late = p.cursor_after(steps + 1);
        let expected = ((elapsed + f64::from(STEP)) % 0.5) as f32;
        assert!(
            (late.time - expected).abs() < 1e-5,
            "got {} want {expected}",
            late.time
        );
        assert!(!late.ended, "a self-jump is still playing");
        let (translation, _) = p.root_motion_of_step(steps).expect("motion");
        assert!(translation.z < -0.02, "step must move, got {translation:?}");
    }
}
