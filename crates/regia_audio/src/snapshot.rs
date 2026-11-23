// SPDX-License-Identifier: MPL-2.0
//! Snapshots: named bus-volume states with blend times (`docs/specs/16-audio.md` § 1).

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

/// One send level a snapshot sets: the send of `bus` into the send bus `send` (`audio-buses` v1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnapshotSend {
    /// The sending bus.
    pub bus: String,
    /// The send bus it feeds.
    pub send: String,
    /// The send level, dB.
    pub volume_db: f32,
}

/// A named mixer state: bus volumes (absolute dB), optional filter cutoffs, mutes and send
/// levels, and the blend time a mixer applies it with. Buses the snapshot does not mention fall
/// back to the layout's base volume, mute and send levels. The maps are ordered, so a saved
/// `config/audio.ron` is deterministic.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioSnapshot {
    /// Snapshot name (`"Paused"`, `"Underwater"`).
    pub name: String,
    /// Absolute bus volumes in dB.
    #[serde(default)]
    pub bus_volumes: BTreeMap<String, f32>,
    /// Optional filter cutoffs per bus (applied to every low/high-pass on that bus), Hz.
    #[serde(default)]
    pub filter_cutoffs: BTreeMap<String, f64>,
    /// Mute overrides per bus (`true` mutes, `false` unmutes a bus the host muted).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bus_mutes: BTreeMap<String, bool>,
    /// Send levels the snapshot sets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub send_volumes: Vec<SnapshotSend>,
    /// The blend time a mixer applies the snapshot with when the caller names none
    /// ([`crate::Audio::apply_snapshot`]), seconds (0 = at once).
    #[serde(default)]
    pub blend_secs: f32,
}

impl AudioSnapshot {
    /// A snapshot named `name` with no overrides (everything at layout base).
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Overrides one bus mute.
    pub fn with_bus_mute(mut self, bus: impl Into<String>, mute: bool) -> Self {
        self.bus_mutes.insert(bus.into(), mute);
        self
    }

    /// Overrides one send level.
    pub fn with_send_volume(mut self, bus: impl Into<String>, send: impl Into<String>, db: f32) -> Self {
        self.send_volumes.push(SnapshotSend {
            bus: bus.into(),
            send: send.into(),
            volume_db: db,
        });
        self
    }

    /// Sets the blend time, seconds.
    pub fn with_blend_secs(mut self, secs: f32) -> Self {
        self.blend_secs = secs;
        self
    }

    /// Overrides one bus volume.
    pub fn with_bus_volume(mut self, bus: impl Into<String>, db: f32) -> Self {
        self.bus_volumes.insert(bus.into(), db);
        self
    }

    /// Overrides one bus's filter cutoffs.
    pub fn with_filter_cutoff(mut self, bus: impl Into<String>, hz: f64) -> Self {
        self.filter_cutoffs.insert(bus.into(), hz);
        self
    }
}

/// The stack of pushed snapshots; the top one is active.
#[derive(Default)]
pub struct SnapshotStack {
    snapshots: HashMap<String, AudioSnapshot>,
    stack: Vec<String>,
}

impl SnapshotStack {
    /// Registers a snapshot (replaces same-named).
    pub fn register(&mut self, snapshot: AudioSnapshot) {
        self.snapshots.insert(snapshot.name.clone(), snapshot);
    }

    /// Pushes a snapshot on the stack (blending is applied by the caller with the fade time).
    /// `Err` when the name is not registered.
    pub fn push(&mut self, name: &str) -> Result<(), crate::error::AudioError> {
        if !self.snapshots.contains_key(name) {
            return Err(crate::error::AudioError::UnknownSnapshot(name.into()));
        }
        self.stack.retain(|n| n != name);
        self.stack.push(name.to_string());
        Ok(())
    }

    /// Pops the top snapshot. `Err` when the stack is empty.
    pub fn pop(&mut self) -> Result<String, crate::error::AudioError> {
        self.stack
            .pop()
            .ok_or_else(|| crate::error::AudioError::UnknownSnapshot("the snapshot stack is empty".into()))
    }

    /// The name of the active (top) snapshot, or `None` when at the base state.
    pub fn active(&self) -> Option<&str> {
        self.stack.last().map(String::as_str)
    }

    /// The active snapshot, or `None` at the base state.
    pub fn active_snapshot(&self) -> Option<&AudioSnapshot> {
        self.active().and_then(|name| self.snapshots.get(name))
    }

    /// The target volume of `bus`: the snapshot's override or `base_db`.
    pub fn target_volume_db(&self, bus: &str, base_db: f32) -> f32 {
        self.active_snapshot()
            .and_then(|s| s.bus_volumes.get(bus).copied())
            .unwrap_or(base_db)
    }

    /// The mute of `bus`, if the active snapshot overrides it.
    pub fn target_mute(&self, bus: &str) -> Option<bool> {
        self.active_snapshot().and_then(|s| s.bus_mutes.get(bus).copied())
    }

    /// The level of `bus`'s send into `send`, if the active snapshot sets it.
    pub fn target_send(&self, bus: &str, send: &str) -> Option<f32> {
        self.active_snapshot().and_then(|s| {
            s.send_volumes
                .iter()
                .find(|x| x.bus == bus && x.send == send)
                .map(|x| x.volume_db)
        })
    }

    /// The registered snapshot `name`.
    pub fn get(&self, name: &str) -> Option<&AudioSnapshot> {
        self.snapshots.get(name)
    }

    /// The target filter cutoff of `bus`, if the active snapshot overrides it.
    pub fn target_filter_cutoff(&self, bus: &str) -> Option<f64> {
        self.active_snapshot()
            .and_then(|s| s.filter_cutoffs.get(bus).copied())
    }

    /// Depth of the stack.
    pub fn depth(&self) -> usize {
        self.stack.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AudioError;

    #[test]
    fn push_and_pop_change_the_active_target() {
        let mut stack = SnapshotStack::default();
        stack.register(AudioSnapshot::new("Paused").with_bus_volume("Music", -12.0));
        stack.register(AudioSnapshot::new("Underwater").with_bus_volume("World", -6.0));

        // Base state: layout volumes.
        assert_eq!(stack.active(), None);
        assert_eq!(stack.target_volume_db("Music", -3.0), -3.0);

        stack.push("Paused").expect("registered");
        assert_eq!(stack.active(), Some("Paused"));
        assert_eq!(stack.target_volume_db("Music", -3.0), -12.0);
        assert_eq!(
            stack.target_volume_db("Sfx", 0.0),
            0.0,
            "unmentioned buses stay at base"
        );

        stack.push("Underwater").expect("registered");
        assert_eq!(stack.active(), Some("Underwater"));
        assert_eq!(stack.target_volume_db("World", 0.0), -6.0);
        assert_eq!(stack.target_volume_db("Music", -3.0), -3.0, "Paused is masked");

        assert_eq!(stack.pop().expect("pop"), "Underwater");
        assert_eq!(stack.active(), Some("Paused"));
        assert_eq!(stack.pop().expect("pop"), "Paused");
        assert_eq!(stack.active(), None);
    }

    #[test]
    fn unknown_snapshots_and_empty_pops_are_errors() {
        let mut stack = SnapshotStack::default();
        assert_eq!(
            stack.push("Ghost"),
            Err(AudioError::UnknownSnapshot("Ghost".into()))
        );
        assert!(matches!(stack.pop(), Err(AudioError::UnknownSnapshot(_))));
    }

    #[test]
    fn pushing_the_same_snapshot_moves_it_to_the_top() {
        let mut stack = SnapshotStack::default();
        stack.register(AudioSnapshot::new("A"));
        stack.register(AudioSnapshot::new("B"));
        stack.push("A").expect("registered");
        stack.push("B").expect("registered");
        stack.push("A").expect("registered");
        assert_eq!(stack.depth(), 2);
        assert_eq!(stack.active(), Some("A"));
    }
}
