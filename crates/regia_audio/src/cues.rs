// SPDX-License-Identifier: MPL-2.0
//! The [`CueAssets`] resource: the sound cues entity emitters play (`docs/specs/16-audio.md` § 3).
//!
//! An [`AudioEmitter`](crate::AudioEmitter) names its cue by [`Handle`] (runtime code) or by name
//! (scenes, the editor, the agent protocol); both resolve here. This mirrors
//! [`SoundAssets`](crate::SoundAssets) for cues: an emitter that plays a [`SoundCue`] (rather than
//! a single sound) picks one of the cue's variations when it starts or its cue changes.

use std::collections::BTreeMap;

use regia_asset::{AssetId, Assets, Handle};
use regia_ecs::prelude::*;

use crate::cue::SoundCue;

/// Named, handle-addressed storage of the sound cues emitters play.
///
/// # Example
///
/// ```
/// # use regia_audio::{CueAssets, SoundCue, CueEntry, SoundAsset, SoundClip};
/// let mut cues = CueAssets::default();
/// let clip = SoundClip::new(vec![0.1; 480], 48_000).expect("a valid clip");
/// let hit = SoundCue::new("hit", vec![CueEntry { asset: SoundAsset::from_clip(&clip), weight: 1.0 }]);
/// let handle = cues.add("hit", hit);
/// assert_eq!(cues.handle("hit"), Some(handle));
/// assert!(cues.by_name("hit").is_some());
/// ```
#[derive(Resource, Default)]
pub struct CueAssets {
    assets: Assets<SoundCue>,
    names: BTreeMap<String, Handle<SoundCue>>,
}

impl CueAssets {
    /// Registers `cue` under `name` and returns its handle. Re-adding a name replaces the cue in
    /// place: the name and every existing handle resolve to the new cue.
    pub fn add(&mut self, name: impl Into<String>, cue: SoundCue) -> Handle<SoundCue> {
        let name = name.into();
        let id = AssetId::sub_asset(AssetId::NIL, &name);
        let handle = self.assets.insert(id, cue);
        self.names.insert(name, handle.clone());
        handle
    }

    /// The cue behind `handle` (`None` for a stale or foreign handle).
    pub fn get(&self, handle: &Handle<SoundCue>) -> Option<&SoundCue> {
        self.assets.get(handle)
    }

    /// The handle of the cue registered as `name`.
    pub fn handle(&self, name: &str) -> Option<Handle<SoundCue>> {
        self.names.get(name).cloned()
    }

    /// The cue registered as `name`.
    pub fn by_name(&self, name: &str) -> Option<&SoundCue> {
        self.handle(name).and_then(|handle| self.assets.get(&handle))
    }

    /// The name a handle was registered under (tools and the agent protocol).
    pub fn name_of(&self, handle: &Handle<SoundCue>) -> Option<&str> {
        self.names
            .iter()
            .find(|(_, h)| *h == handle)
            .map(|(name, _)| name.as_str())
    }

    /// Every registered name, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.names.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CueEntry, SoundAsset, SoundClip};

    fn cue(name: &str) -> SoundCue {
        let clip = SoundClip::new(vec![0.1; 480], 48_000).expect("clip");
        SoundCue::new(
            name,
            vec![CueEntry {
                asset: SoundAsset::from_clip(&clip),
                weight: 1.0,
            }],
        )
    }

    #[test]
    fn add_resolve_and_replace_round_trip() {
        let mut cues = CueAssets::default();
        let handle = cues.add("hit", cue("hit"));
        assert_eq!(cues.handle("hit"), Some(handle.clone()));
        assert_eq!(cues.name_of(&handle), Some("hit"));
        assert!(cues.by_name("hit").is_some());
        assert!(cues.get(&handle).is_some());
        assert_eq!(cues.names().collect::<Vec<_>>(), vec!["hit"]);
        // Re-adding replaces in place: the old handle and the name resolve to the NEW cue
        // (marked by its volume), not the old one (rev-kimi-10 finding 12).
        let mut louder = cue("hit");
        louder.volume_db = -3.0;
        cues.add("hit", louder);
        assert_eq!(
            cues.get(&handle).map(|c| c.volume_db),
            Some(-3.0),
            "the old handle resolves to the new cue after a replace"
        );
        assert_eq!(
            cues.by_name("hit").map(|c| c.volume_db),
            Some(-3.0),
            "the name too"
        );
        // Unknown names and stale handles resolve to nothing.
        assert!(cues.by_name("nope").is_none());
        assert!(cues.names().count() == 1);
    }
}
