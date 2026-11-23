// SPDX-License-Identifier: MPL-2.0
//! Occlusion (`docs/specs/16-audio.md` § 3, Tier 2): geometry between a listener and an emitter
//! muffles the emitter (low-pass) and quietens it.
//!
//! The audio crate does not know about physics. Each frame the `AudioSync` system queues, within
//! a per-frame ray budget and round-robin over the occluding emitters, one
//! [`OcclusionRay`] per emitter on the [`AudioOcclusion`] resource. An *occlusion provider* —
//! `regia::DefaultPlugins` installs one that casts the rays through `regia_physics` — answers
//! them with [`AudioOcclusion::report`] before the next sync. The audio side smooths every
//! emitter toward its reported amount ([`OcclusionTuning::smoothing_secs`]), so a budget that
//! visits an emitter only every few frames still moves it without steps.
//!
//! Without a provider nothing is ever reported and every emitter stays unoccluded. Tests and
//! games with their own geometry query call [`AudioOcclusion::report`] themselves.

use std::collections::{HashMap, HashSet};

use regia_core::math::Vec3;
use regia_ecs::prelude::*;

/// One occlusion query: is anything between `from` (the listener) and `to` (the emitter)?
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OcclusionRay {
    /// The emitter entity the answer is for.
    pub emitter: Entity,
    /// The listener entity hearing it (`None` for the default origin listener).
    pub listener: Option<Entity>,
    /// Ray start: the listener position, world space.
    pub from: Vec3,
    /// Ray end: the emitter position, world space.
    pub to: Vec3,
}

/// Distance to the target under which [`OcclusionTuning::smooth`] lands on it. The smoothed
/// values are an occlusion amount (0..1) and metres of rock: 0.01 occlusion is 0.15 dB and a
/// 3 % cutoff change at the default tuning, 1 cm of rock 0.025 dB (and the rock march is only
/// exact to its 1 m step), so the last step is inaudible.
const SMOOTH_SETTLE: f32 = 0.01;

/// How occlusion sounds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OcclusionTuning {
    /// Volume change of a fully occluded emitter, dB (negative).
    pub occluded_gain_db: f32,
    /// Low-pass cutoff of a fully occluded emitter, Hz (log-interpolated from 20 kHz).
    pub occluded_cutoff_hz: f32,
    /// Time constant of the smoothing toward the reported amount, seconds.
    pub smoothing_secs: f32,
    /// Occlusion added by each blocking obstacle: `1 − (1 − per_obstacle)^obstacles`.
    pub per_obstacle: f32,
}

impl Default for OcclusionTuning {
    fn default() -> Self {
        Self {
            occluded_gain_db: -15.0,
            occluded_cutoff_hz: 700.0,
            smoothing_secs: 0.12,
            per_obstacle: 0.6,
        }
    }
}

/// `x` clamped to 0..1, with NaN as 0.
fn unit(x: f32) -> f32 {
    if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) }
}

impl OcclusionTuning {
    /// Occlusion amount (0..1) for `obstacles` blocking bodies between listener and emitter.
    pub fn amount_for_obstacles(&self, obstacles: usize) -> f32 {
        if obstacles == 0 {
            return 0.0;
        }
        let open = (1.0 - unit(self.per_obstacle)).powi(obstacles.min(16) as i32);
        1.0 - open
    }

    /// Volume factor (amplitude) for an occlusion amount. A NaN amount or tuning leaves the
    /// emitter unoccluded rather than handing NaN to the mixer.
    pub fn gain(&self, amount: f32) -> f32 {
        let db = self.occluded_gain_db * unit(amount);
        if db.is_nan() {
            // Broken tuning (a NaN scene value): unoccluded, not silence.
            return 1.0;
        }
        let gain = crate::spatial::db_to_gain(db);
        if gain.is_finite() { gain } else { 1.0 }
    }

    /// Low-pass cutoff (Hz) for an occlusion amount: 20 kHz open, `occluded_cutoff_hz` closed,
    /// interpolated in log-frequency (perceptually even). Always finite: a NaN cutoff would turn
    /// the emitter track's filter state into NaN for good.
    pub fn cutoff_hz(&self, amount: f32) -> f64 {
        let open = 20_000.0f32.ln();
        let closed_hz = if self.occluded_cutoff_hz.is_nan() {
            20_000.0
        } else {
            self.occluded_cutoff_hz.clamp(20.0, 20_000.0)
        };
        let closed = closed_hz.ln();
        f64::from((open + (closed - open) * unit(amount)).exp())
    }

    /// One smoothing step from `current` toward `target` over `dt` seconds. A step that ends
    /// within 0.01 of the target lands on it. A NaN never sticks: a NaN `current` restarts from
    /// the target, a NaN `target` holds the current value, and a NaN time constant or step
    /// behaves like no smoothing and no step.
    pub fn smooth(&self, current: f32, target: f32, dt: f32) -> f32 {
        let target = match (target.is_nan(), current.is_nan()) {
            (false, _) => target,
            (true, false) => current,
            (true, true) => 0.0,
        };
        if current.is_nan() {
            return target;
        }
        if dt.is_nan() || dt <= 0.0 {
            return current;
        }
        if self.smoothing_secs.is_nan() || self.smoothing_secs <= 1e-4 {
            return target;
        }
        let k = 1.0 - (-dt / self.smoothing_secs).exp();
        let next = current + (target - current) * k;
        // An exponential approach never lands; within an inaudible step it does, so a cleared
        // report reads 0 again instead of a tail of millimetres.
        if (target - next).abs() <= SMOOTH_SETTLE {
            target
        } else {
            next
        }
    }
}

/// The occlusion exchange between the audio sync and an occlusion provider.
#[derive(Resource, Debug)]
pub struct AudioOcclusion {
    /// Rays queued per frame at most (the per-emitter update budget is spread round-robin).
    pub rays_per_frame: usize,
    /// How occlusion sounds.
    pub tuning: OcclusionTuning,
    /// How rock thickness sounds (reported with [`AudioOcclusion::report_thickness`]).
    pub rock: crate::audio_zone::RockTuning,
    /// Rays queued by the last `AudioSync`, for the provider to answer.
    pending: Vec<OcclusionRay>,
    /// Copies of the queued rays for the rock-thickness answerer (the provider drains `pending`).
    pending_rock: Vec<OcclusionRay>,
    /// Latest reported amount per emitter, 0..1.
    reported: HashMap<Entity, f32>,
    /// Latest reported rock thickness per emitter, metres.
    thickness: HashMap<Entity, f32>,
    /// Round-robin cursor over the occluding emitters.
    cursor: usize,
    /// Rays answered so far (diagnostics).
    answered: u64,
    /// Rays of sources that are not sound emitters (voice chat), queued with the emitters' rays.
    extra: Vec<OcclusionRay>,
    /// Emitters listed this frame. Cleared and refilled so a steady crowd does not allocate a set.
    listed: HashSet<Entity>,
    /// Emitter rays plus `extra` and `keyed_extra`, reused when non-emitter rays are present.
    merged: Vec<OcclusionRay>,
    /// Rays of other non-emitter sources by owner (sequence voices, ADR-3197), queued after
    /// `extra` in owner order.
    keyed_extra: std::collections::BTreeMap<String, Vec<OcclusionRay>>,
}

impl Default for AudioOcclusion {
    fn default() -> Self {
        Self {
            rays_per_frame: 16,
            tuning: OcclusionTuning::default(),
            rock: crate::audio_zone::RockTuning::default(),
            pending: Vec::new(),
            pending_rock: Vec::new(),
            reported: HashMap::new(),
            thickness: HashMap::new(),
            cursor: 0,
            answered: 0,
            extra: Vec::new(),
            listed: HashSet::new(),
            merged: Vec::new(),
            keyed_extra: std::collections::BTreeMap::new(),
        }
    }
}

impl AudioOcclusion {
    /// Replaces the rays of sources that are not sound emitters (voice chat speakers). The next
    /// [`queue`](Self::queue) adds them to the emitters' rays.
    pub fn set_extra_rays(&mut self, rays: Vec<OcclusionRay>) {
        self.extra = rays;
    }

    /// Replaces the rays one non-emitter `owner` asks about (the sequencer's spatial sound
    /// sections use `"sequencer"`, ADR-3197); an empty list removes the owner. The next
    /// [`queue`](Self::queue) adds every owner's rays, in owner order, after the
    /// [`set_extra_rays`](Self::set_extra_rays) rays, so owners never replace each other's.
    pub fn set_extra_rays_for(&mut self, owner: &str, rays: Vec<OcclusionRay>) {
        if rays.is_empty() {
            self.keyed_extra.remove(owner);
        } else {
            self.keyed_extra.insert(owner.to_string(), rays);
        }
    }

    /// Takes the queued rays (the provider answers each with [`AudioOcclusion::report`]).
    ///
    /// The returned `Vec` owns the buffer, so the next [`queue`](Self::queue) starts from an
    /// empty allocation. A provider that runs every frame should use
    /// [`take_pending_into`](Self::take_pending_into) and keep the buffer.
    pub fn take_pending(&mut self) -> Vec<OcclusionRay> {
        std::mem::take(&mut self.pending)
    }

    /// Moves the queued rays into `out` (cleared first) and keeps this queue's buffer.
    ///
    /// `out` keeps the capacity it grows to. The queue's buffer stays here, empty, so the
    /// next [`queue`](Self::queue) does not allocate when the ray count does not grow.
    pub fn take_pending_into(&mut self, out: &mut Vec<OcclusionRay>) {
        out.clear();
        out.append(&mut self.pending);
    }

    /// The queued rays, without taking them.
    pub fn pending(&self) -> &[OcclusionRay] {
        &self.pending
    }

    /// Reports `emitter`'s occlusion amount (0 = clear line of sight, 1 = fully occluded).
    pub fn report(&mut self, emitter: Entity, amount: f32) {
        let amount = if amount.is_finite() {
            amount.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.reported.insert(emitter, amount);
        self.answered += 1;
    }

    /// Reports an obstacle count, converted with [`OcclusionTuning::amount_for_obstacles`].
    pub fn report_obstacles(&mut self, emitter: Entity, obstacles: usize) {
        let amount = self.tuning.amount_for_obstacles(obstacles);
        self.report(emitter, amount);
    }

    /// The latest reported amount for `emitter` (0 when never reported).
    pub fn reported(&self, emitter: Entity) -> f32 {
        self.reported.get(&emitter).copied().unwrap_or(0.0)
    }

    /// Reports the metres of rock between `emitter` and its listener.
    pub fn report_thickness(&mut self, emitter: Entity, metres: f32) {
        let metres = if metres.is_finite() { metres.max(0.0) } else { 0.0 };
        self.thickness.insert(emitter, metres);
    }

    /// Forgets every reported rock thickness (the rock field was removed).
    pub fn clear_thickness(&mut self) {
        self.thickness.clear();
    }

    /// The latest reported rock thickness for `emitter`, metres (0 when never reported).
    pub fn reported_thickness(&self, emitter: Entity) -> f32 {
        self.thickness.get(&emitter).copied().unwrap_or(0.0)
    }

    /// Takes the copies of the queued rays for the rock-thickness answerer.
    pub fn take_pending_rock(&mut self) -> Vec<OcclusionRay> {
        std::mem::take(&mut self.pending_rock)
    }

    /// How many answers were reported so far.
    pub fn answered(&self) -> u64 {
        self.answered
    }

    /// Queues this frame's rays: up to [`AudioOcclusion::rays_per_frame`] of `candidates`
    /// (emitters that occlude and are within range of their listener), continuing round-robin
    /// from where the previous frame stopped. Forgets reports of emitters no longer listed.
    pub fn queue(&mut self, candidates: &[OcclusionRay]) {
        let use_extra = !self.extra.is_empty() || !self.keyed_extra.is_empty();
        if use_extra {
            self.merged.clear();
            self.merged.extend_from_slice(candidates);
            self.merged.extend_from_slice(&self.extra);
            for rays in self.keyed_extra.values() {
                self.merged.extend_from_slice(rays);
            }
        }
        self.listed.clear();
        if use_extra {
            self.listed.extend(self.merged.iter().map(|ray| ray.emitter));
        } else {
            self.listed.extend(candidates.iter().map(|ray| ray.emitter));
        }
        self.pending.clear();
        self.pending_rock.clear();
        // A set, not a scan of `candidates` per report: a few hundred occluding emitters would
        // otherwise cost a quadratic number of comparisons every frame. The set is kept on
        // `self` so the next frame refills it without allocating.
        let listed = &self.listed;
        self.reported.retain(|entity, _| listed.contains(entity));
        self.thickness.retain(|entity, _| listed.contains(entity));
        let len = if use_extra {
            self.merged.len()
        } else {
            candidates.len()
        };
        if len == 0 || self.rays_per_frame == 0 {
            return;
        }
        let count = self.rays_per_frame.min(len);
        let start = self.cursor % len;
        for i in 0..count {
            let ray = if use_extra {
                self.merged[(start + i) % len]
            } else {
                candidates[(start + i) % len]
            };
            self.pending.push(ray);
        }
        self.cursor = (start + count) % len;
        self.pending_rock.extend_from_slice(&self.pending);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ecs::ecs::entity::EntityIndex;

    fn entity(i: u32) -> Entity {
        Entity::from_index(EntityIndex::from_raw_u32(i).expect("index"))
    }

    fn ray(i: u32) -> OcclusionRay {
        OcclusionRay {
            emitter: entity(i),
            listener: None,
            from: Vec3::ZERO,
            to: Vec3::X * i as f32,
        }
    }

    #[test]
    fn the_budget_visits_every_emitter_round_robin() {
        let mut occ = AudioOcclusion {
            rays_per_frame: 3,
            ..Default::default()
        };
        let candidates: Vec<OcclusionRay> = (1..=7).map(ray).collect();
        let mut seen = Vec::new();
        for _ in 0..3 {
            occ.queue(&candidates);
            assert!(occ.pending().len() <= 3, "never more than the budget");
            seen.extend(occ.take_pending().iter().map(|r| r.emitter));
        }
        // 9 rays over 7 emitters: every emitter was visited at least once.
        for i in 1..=7 {
            assert!(seen.contains(&entity(i)), "emitter {i} starved");
        }
    }

    /// Owner-keyed extra rays (ADR-3197) join the queue after the voice-chat extras, keep their
    /// reports alive, and never replace another owner's rays.
    #[test]
    fn keyed_extra_rays_join_the_queue_beside_the_voice_chat_rays() {
        let mut occ = AudioOcclusion {
            rays_per_frame: 16,
            ..Default::default()
        };
        occ.set_extra_rays(vec![ray(2)]);
        occ.set_extra_rays_for("sequencer", vec![ray(3), ray(4)]);
        occ.set_extra_rays_for("other", vec![ray(5)]);
        occ.report(entity(3), 1.0);
        occ.queue(&[ray(1)]);
        let queued: Vec<Entity> = occ.take_pending().iter().map(|r| r.emitter).collect();
        assert_eq!(queued, [entity(1), entity(2), entity(5), entity(3), entity(4)]);
        assert_eq!(occ.reported(entity(3)), 1.0, "a listed key keeps its report");
        // Clearing one owner leaves the others; its keys' reports go with the next queue.
        occ.set_extra_rays_for("sequencer", Vec::new());
        occ.queue(&[]);
        let queued: Vec<Entity> = occ.take_pending().iter().map(|r| r.emitter).collect();
        assert_eq!(queued, [entity(2), entity(5)]);
        assert_eq!(occ.reported(entity(3)), 0.0);
    }

    #[test]
    fn obstacles_compound_and_tuning_maps_amounts() {
        let t = OcclusionTuning::default();
        assert_eq!(t.amount_for_obstacles(0), 0.0);
        let one = t.amount_for_obstacles(1);
        let two = t.amount_for_obstacles(2);
        assert!((one - 0.6).abs() < 1e-6 && two > one && two < 1.0);
        assert!((t.gain(0.0) - 1.0).abs() < 1e-6);
        assert!((t.gain(1.0) - crate::spatial::db_to_gain(-15.0)).abs() < 1e-6);
        assert!((t.cutoff_hz(0.0) - 20_000.0).abs() < 1.0);
        assert!((t.cutoff_hz(1.0) - 700.0).abs() < 1.0);
        let half = t.cutoff_hz(0.5);
        assert!(half > 700.0 && half < 20_000.0);
    }

    #[test]
    fn smoothing_converges_without_overshoot() {
        let t = OcclusionTuning::default();
        let mut x = 0.0;
        let mut prev = x;
        for _ in 0..60 {
            x = t.smooth(x, 1.0, 1.0 / 60.0);
            assert!(x >= prev && x <= 1.0);
            prev = x;
        }
        assert!(x > 0.99, "{x}");
    }

    #[test]
    fn smoothing_lands_on_the_target() {
        // 10 m of rock cleared: one second at 60 Hz ends exactly at 0, not on a decaying tail.
        let t = OcclusionTuning::default();
        let mut x = 10.0;
        for _ in 0..60 {
            x = t.smooth(x, 0.0, 1.0 / 60.0);
        }
        assert_eq!(x, 0.0);
        // Further than the settle distance, a step still moves only part of the way.
        assert!(t.smooth(0.0, 1.0, 1.0 / 60.0) < 0.5);
    }

    #[test]
    fn nan_tuning_and_inputs_never_reach_the_mixer() {
        let broken = OcclusionTuning {
            occluded_gain_db: f32::NAN,
            occluded_cutoff_hz: f32::NAN,
            smoothing_secs: f32::NAN,
            per_obstacle: f32::NAN,
        };
        for amount in [0.0, 0.5, 1.0, f32::NAN] {
            assert!(broken.gain(amount).is_finite(), "gain at {amount}");
            assert!(broken.cutoff_hz(amount).is_finite(), "cutoff at {amount}");
        }
        assert_eq!(broken.amount_for_obstacles(3), 0.0);
        let t = OcclusionTuning::default();
        assert_eq!(t.gain(f32::NAN), 1.0, "a NaN amount is unoccluded");
        assert!((t.cutoff_hz(f32::NAN) - 20_000.0).abs() < 1.0);
        // NaN never sticks in the smoothed state.
        assert_eq!(t.smooth(f32::NAN, 0.5, 1.0 / 60.0), 0.5);
        assert_eq!(t.smooth(0.3, f32::NAN, 1.0 / 60.0), 0.3);
        assert_eq!(t.smooth(f32::NAN, f32::NAN, 1.0 / 60.0), 0.0);
        assert_eq!(t.smooth(0.3, 1.0, f32::NAN), 0.3, "a NaN step does not move");
        assert_eq!(
            broken.smooth(0.3, 1.0, 1.0 / 60.0),
            1.0,
            "a NaN time constant is instant"
        );
        let mut x = f32::NAN;
        for _ in 0..10 {
            x = broken.smooth(x, 0.25, 1.0 / 60.0);
            assert!(x.is_finite());
        }
    }

    #[test]
    fn queueing_many_emitters_keeps_exactly_the_listed_reports() {
        let mut occ = AudioOcclusion {
            rays_per_frame: 16,
            ..Default::default()
        };
        let all: Vec<OcclusionRay> = (1..=600).map(ray).collect();
        for r in &all {
            occ.report(r.emitter, 0.5);
            occ.report_thickness(r.emitter, 2.0);
        }
        // Every other emitter leaves range.
        let kept: Vec<OcclusionRay> = all
            .iter()
            .copied()
            .filter(|r| (r.to.x as u32).is_multiple_of(2))
            .collect();
        occ.queue(&kept);
        for r in &all {
            let listed = (r.to.x as u32).is_multiple_of(2);
            assert_eq!(occ.reported(r.emitter), if listed { 0.5 } else { 0.0 });
            assert_eq!(occ.reported_thickness(r.emitter), if listed { 2.0 } else { 0.0 });
        }
        assert_eq!(occ.pending().len(), 16);
        // The cursor keeps its place across a shrinking candidate list (300 → 7).
        let few: Vec<OcclusionRay> = kept.iter().copied().take(7).collect();
        let mut seen = HashSet::new();
        for _ in 0..3 {
            occ.queue(&few);
            seen.extend(occ.take_pending().iter().map(|r| r.emitter));
        }
        assert_eq!(seen.len(), 7, "every remaining emitter is visited");
    }

    #[test]
    fn take_pending_into_keeps_both_capacities() {
        let mut occ = AudioOcclusion {
            rays_per_frame: 4,
            ..Default::default()
        };
        let candidates: Vec<OcclusionRay> = (1..=4).map(ray).collect();
        let mut out = Vec::new();
        occ.queue(&candidates);
        occ.take_pending_into(&mut out);
        assert_eq!(out.len(), 4);
        // The queue kept its own buffer, so the next fill does not start from nothing.
        occ.queue(&candidates);
        assert!(occ.pending.capacity() >= 4, "the queue kept a buffer");
        let queued_cap = occ.pending.capacity();
        let out_cap = out.capacity();
        occ.take_pending_into(&mut out);
        assert_eq!(out.len(), 4);
        assert!(out.capacity() >= out_cap);
        assert!(occ.pending.capacity() >= queued_cap.min(out_cap));
        // A third frame still answers the same four rays and does not shrink.
        occ.queue(&candidates);
        occ.take_pending_into(&mut out);
        assert_eq!(
            out.iter().map(|r| r.emitter).collect::<Vec<_>>(),
            (1..=4).map(entity).collect::<Vec<_>>()
        );
        assert!(out.capacity() >= 4);
        assert!(occ.pending.capacity() >= 4);
    }

    #[test]
    fn reports_of_vanished_emitters_are_forgotten() {
        let mut occ = AudioOcclusion::default();
        occ.report(entity(1), 0.8);
        occ.report(entity(2), f32::NAN);
        assert_eq!(occ.reported(entity(2)), 0.0, "NaN is treated as clear");
        occ.queue(&[ray(2)]);
        assert_eq!(occ.reported(entity(1)), 0.0, "emitter 1 is gone");
    }
}
