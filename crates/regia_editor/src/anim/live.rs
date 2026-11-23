// SPDX-License-Identifier: MPL-2.0
//! Live parameter writes (spec addendum § 11.1, P-201): while play-in-editor runs, the graph
//! panel's Parameters section writes to the watched animator in the play world.
//!
//! A write is a debug write: it goes straight to the running `Animator`'s parameter set, it is
//! never an undo entry and never touches the graph document (nothing is saved), and the record of
//! what was written is cleared when play stops. Writes are queued here by the panel and by
//! `editor.anim_param { live_value }`, and applied by [`super::sync_pie_debug`] (the one place that
//! sees the edit world and the play world together) to the animator it watches.

use std::collections::BTreeMap;

use regia_animation::Animator;
use regia_animation::params::ParamValue;
use regia_ecs::prelude::*;

/// Queued and applied live writes of one play session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LiveParams {
    /// Writes waiting for the next sync, in order (the watched animator gets them all).
    pub queued: Vec<(String, ParamValue)>,
    /// The last value written to each parameter this session (the panel's "Live" badge).
    pub written: BTreeMap<String, ParamValue>,
    /// How many writes reached an animator this session.
    pub applied: u64,
    /// The egui frame of the panel's last write per parameter (one write per painted frame).
    panel_frames: BTreeMap<String, u64>,
}

impl LiveParams {
    /// Queues a write for the watched animator.
    pub fn queue(&mut self, name: &str, value: ParamValue) {
        self.queued.push((name.to_string(), value));
    }

    /// Queues the Parameters panel's write in egui frame `frame`, once per parameter and frame:
    /// a discarded second pass of the frame sees the same Fire click and would run a trigger twice
    /// (review sprint-w12-rev-g7 #5). Returns whether it queued.
    pub fn queue_from_panel(&mut self, name: &str, value: ParamValue, frame: u64) -> bool {
        if self.panel_frames.get(name) == Some(&frame) {
            return false;
        }
        self.panel_frames.insert(name.to_string(), frame);
        self.queue(name, value);
        true
    }

    /// Forgets the session: nothing queued, nothing written (play stopped).
    pub fn clear(&mut self) {
        self.queued.clear();
        self.written.clear();
        self.applied = 0;
        self.panel_frames.clear();
    }

    /// Applies the queued writes to the animator of `entity` (bits) in `play`. Writes stay
    /// queued while no animator is watched yet; returns how many were applied.
    pub fn apply(&mut self, play: &mut World, entity: u64) -> usize {
        if self.queued.is_empty() {
            return 0;
        }
        let Some(entity) = crate::command::entity_from_bits(entity) else {
            return 0;
        };
        let Some(mut animator) = play.get_mut::<Animator>(entity) else {
            return 0;
        };
        let count = self.queued.len();
        for (name, value) in self.queued.drain(..) {
            animator.set(&name, value);
            self.written.insert(name, value);
        }
        self.applied += count as u64;
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_wait_for_an_animator_then_land_and_clear_on_stop() {
        let mut play = World::new();
        let entity = play
            .spawn(Animator::single(
                regia_animation::clip::AnimationClip::empty("idle", 1.0),
                true,
            ))
            .id();
        let mut live = LiveParams::default();
        live.queue("Speed", ParamValue::Float(2.5));
        // No animator at these bits: nothing is applied, the write waits.
        assert_eq!(live.apply(&mut play, u64::MAX), 0);
        assert_eq!(live.queued.len(), 1);
        assert_eq!(live.apply(&mut play, entity.to_bits()), 1);
        let animator = play.get::<Animator>(entity).expect("animator");
        assert_eq!(animator.parameters.float("Speed"), 2.5);
        assert_eq!(live.written.get("Speed"), Some(&ParamValue::Float(2.5)));
        assert_eq!(live.applied, 1);
        live.clear();
        assert_eq!(live, LiveParams::default());
    }

    /// Review sprint-w12-rev-g7 #5: one Fire click queues one trigger, though egui's discarded
    /// second pass of the frame sees the click again.
    #[test]
    fn a_panel_trigger_queues_once_per_egui_frame() {
        let mut live = LiveParams::default();
        assert!(live.queue_from_panel("Jump", ParamValue::Trigger, 5));
        assert!(
            !live.queue_from_panel("Jump", ParamValue::Trigger, 5),
            "the second pass"
        );
        assert!(
            live.queue_from_panel("Speed", ParamValue::Float(1.0), 5),
            "another parameter"
        );
        assert!(
            live.queue_from_panel("Jump", ParamValue::Trigger, 6),
            "the next frame"
        );
        let names: Vec<&str> = live.queued.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["Jump", "Speed", "Jump"]);
        live.clear();
        assert_eq!(live, LiveParams::default());
    }
}
