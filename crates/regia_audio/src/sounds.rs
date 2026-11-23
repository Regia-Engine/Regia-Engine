// SPDX-License-Identifier: MPL-2.0
//! The [`SoundAssets`] resource: the sounds entity emitters play (`docs/specs/16-audio.md` § 3).
//!
//! An [`AudioEmitter`](crate::AudioEmitter) names its sound by [`Handle`] (runtime code) or by
//! name (scenes, the editor, the agent protocol). Both resolve here. Games register sounds once
//! at load time:
//!
//! ```
//! # use regia_audio::{SoundAssets, SoundAsset, SoundClip};
//! let mut sounds = SoundAssets::default();
//! let clip = SoundClip::new(vec![0.1; 480], 48_000).expect("a valid clip");
//! let hum = sounds.add("generator_hum", SoundAsset::from_clip(&clip));
//! assert_eq!(sounds.handle("generator_hum"), Some(hum));
//! ```
//!
//! A sound registered from a file ([`SoundAssets::add_file`]) reloads live in dev builds
//! (`docs/specs/12-assets.md` § 8): when the file changes on disk
//! ([`regia_asset::SourceAssetChanged`]), [`reload_sound_files`] decodes it again and replaces
//! the sound in place — its handle stays valid, the next play uses the new sound, voices already
//! playing finish the old one. A file that no longer decodes keeps the previous sound.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_asset::{AssetId, Assets, Handle};
use regia_ecs::prelude::*;

use crate::error::AudioError;
use crate::loopgen::LoopGenCache;
use crate::loudness_table::LoudnessTable;
use crate::sound::SoundAsset;

/// Named, handle-addressed storage of the sounds emitters play.
#[derive(Resource, Default)]
pub struct SoundAssets {
    assets: Assets<SoundAsset>,
    names: BTreeMap<String, Handle<SoundAsset>>,
    /// Canonical source path → names registered from it ([`Self::add_file`]).
    files: BTreeMap<PathBuf, Vec<String>>,
    /// Live reloads per name.
    revisions: BTreeMap<String, u64>,
    /// Rendered procedural loops (`.loopgen.ron`), keyed by file content.
    loopgens: LoopGenCache,
    /// Sound name → content hash of the loop generator it was registered from.
    loopgen_names: BTreeMap<String, u64>,
    /// The active loudness table (ADR-1855): file sounds play at its gains. `None` = off.
    loudness: Option<LoudnessTable>,
    /// The loudness record of every sound registered from a pak ([`Self::add_packed`]), by asset path.
    packed: BTreeMap<String, crate::loudness_table::SoundLoudness>,
}

fn file_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

impl SoundAssets {
    /// Registers `asset` under `name` and returns its handle. Re-adding a name replaces the
    /// sound in place: the name and every existing handle resolve to the new sound.
    pub fn add(&mut self, name: impl Into<String>, asset: SoundAsset) -> Handle<SoundAsset> {
        let name = name.into();
        let id = AssetId::sub_asset(AssetId::NIL, &name);
        let handle = self.assets.insert(id, asset);
        self.loopgen_names.remove(&name);
        self.names.insert(name, handle.clone());
        handle
    }

    /// The sound behind `handle` (`None` for a stale or foreign handle).
    pub fn get(&self, handle: &Handle<SoundAsset>) -> Option<&SoundAsset> {
        self.assets.get(handle)
    }

    /// The handle of the sound registered as `name`.
    pub fn handle(&self, name: &str) -> Option<Handle<SoundAsset>> {
        self.names.get(name).cloned()
    }

    /// The sound registered as `name`.
    pub fn by_name(&self, name: &str) -> Option<&SoundAsset> {
        self.handle(name).and_then(|handle| self.assets.get(&handle))
    }

    /// The name a handle was registered under (tools and the agent protocol).
    pub fn name_of(&self, handle: &Handle<SoundAsset>) -> Option<&str> {
        self.names
            .iter()
            .find(|(_, h)| *h == handle)
            .map(|(name, _)| name.as_str())
    }

    /// Every registered name, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.names.keys().map(String::as_str)
    }

    /// Decodes the sound file at `path` (fully, as a static sound) and registers it under `name`.
    /// The sound reloads live when the file changes in a dev build ([`reload_sound_files`]).
    ///
    /// # Errors
    ///
    /// The file could not be read or decoded.
    pub fn add_file(
        &mut self,
        name: impl Into<String>,
        path: impl AsRef<Path>,
    ) -> Result<Handle<SoundAsset>, AudioError> {
        let key = file_key(path.as_ref());
        let asset = self.with_table_gain(SoundAsset::from_file_static(&key)?, &key);
        let name = name.into();
        let handle = self.add(name.clone(), asset);
        for names in self.files.values_mut() {
            names.retain(|n| *n != name);
        }
        self.files.retain(|_, names| !names.is_empty());
        self.files.entry(key).or_default().push(name);
        Ok(handle)
    }

    /// Registers the sound a pak holds: `payload` is the body of a cooked sound entry
    /// (`regia_audio::CookedSound`, [`crate::cooked_sound`]), `key` its asset path (`assets/sfx/door.wav`),
    /// the key a loudness table uses. The audio decodes from the entry's bytes: no loose file is read.
    /// Its record joins [`Self::packed_loudness_table`], and the active loudness table's gain applies
    /// as it does to a file sound.
    ///
    /// # Errors
    ///
    /// The payload is not a cooked sound ([`AudioError::Invalid`], the message carries the
    /// [`crate::cooked_sound::CookedSoundError`] code), its audio does not decode
    /// ([`AudioError::LoadFailed`]), or decoding it whole would pass the default
    /// [`crate::SoundDecodeLimits`] ([`AudioError::DecodeLimit`], ADR-4922).
    pub fn add_packed(
        &mut self,
        name: impl Into<String>,
        key: &str,
        payload: &[u8],
    ) -> Result<Handle<SoundAsset>, AudioError> {
        self.add_packed_with_limits(name, key, payload, &crate::SoundDecodeLimits::default())
    }

    /// [`Self::add_packed`] with explicit decode limits (ADR-4922). The public path uses the defaults.
    pub(crate) fn add_packed_with_limits(
        &mut self,
        name: impl Into<String>,
        key: &str,
        payload: &[u8],
        limits: &crate::SoundDecodeLimits,
    ) -> Result<Handle<SoundAsset>, AudioError> {
        let _span = tracing::debug_span!(target: "regia_audio", "add_packed_sound").entered();
        let cooked = crate::cooked_sound::CookedSound::from_bytes(payload)
            .map_err(|e| AudioError::Invalid(format!("{}: {key}: {e}", e.code())))?;
        let data = crate::decode::decode_static_bytes(cooked.data, key, limits)?;
        let path = PathBuf::from(key);
        let asset = self.with_table_gain(SoundAsset::from_static_data(data), &path);
        let name = name.into();
        let handle = self.add(name.clone(), asset);
        for names in self.files.values_mut() {
            names.retain(|n| *n != name);
        }
        self.files.retain(|_, names| !names.is_empty());
        self.files.entry(path).or_default().push(name);
        self.packed.insert(key.to_owned(), cooked.loudness);
        Ok(handle)
    }

    /// [`Self::add_packed`] through the asset server: the pak (or the dev cache) entry for `path`.
    ///
    /// # Errors
    ///
    /// [`AudioError::Invalid`] when the manifest has no such path, the entry does not read or is not a
    /// cooked sound; otherwise as [`Self::add_packed`].
    pub fn add_packed_from_assets(
        &mut self,
        name: impl Into<String>,
        server: &mut regia_asset::AssetServer,
        path: &str,
    ) -> Result<Handle<SoundAsset>, AudioError> {
        let missing = |why: String| AudioError::Invalid(format!("cooked_sound_missing: {path}: {why}"));
        let id = server
            .id_for_path(path)
            .ok_or_else(|| missing("not in the asset manifest".to_owned()))?;
        let (bytes, _) = server.read_cooked(id).map_err(|e| missing(e.to_string()))?;
        let (header, payload) =
            regia_asset::decode_cooked_file(&bytes).map_err(|e| missing(e.to_string()))?;
        if header.type_tag != regia_core::id::TypePathHash::of(crate::cooked_sound::COOKED_SOUND_TYPE) {
            return Err(missing("the entry is not a cooked sound".to_owned()));
        }
        self.add_packed(name, path, payload)
    }

    /// A loudness table over the records of every packed sound registered so far (the default
    /// policy; each record already holds the gain the cook's policy gave it). Hand it to
    /// [`Self::set_loudness_normalization`] to play packed sounds at their normalized loudness; a
    /// sound with no record in the table plays at 0 dB, as for a loose file with no table entry.
    #[must_use]
    pub fn packed_loudness_table(&self) -> LoudnessTable {
        LoudnessTable {
            entries: self.packed.clone(),
            ..LoudnessTable::default()
        }
    }

    /// Re-decodes the file at `path` for every name registered from it, replacing each sound in
    /// place (handles stay valid). Returns how many sounds were replaced (0 when no sound came
    /// from that file).
    ///
    /// # Errors
    ///
    /// The new file does not decode; the previous sounds stay.
    pub fn reload_file(&mut self, path: impl AsRef<Path>) -> Result<usize, AudioError> {
        let key = file_key(path.as_ref());
        let Some(names) = self.files.get(&key).cloned() else {
            return Ok(0);
        };
        let asset = self.with_table_gain(SoundAsset::from_file_static(&key)?, &key);
        for name in &names {
            let id = AssetId::sub_asset(AssetId::NIL, name);
            let handle = self.assets.insert(id, asset.clone());
            self.names.insert(name.clone(), handle);
            *self.revisions.entry(name.clone()).or_insert(0) += 1;
        }
        Ok(names.len())
    }

    /// `asset` with the active loudness table's gain for the file at `key` (0 dB without a
    /// table or an entry).
    fn with_table_gain(&self, asset: SoundAsset, key: &Path) -> SoundAsset {
        let gain = self
            .loudness
            .as_ref()
            .and_then(|table| table.gain_db_for(&key.to_string_lossy()))
            .unwrap_or(0.0);
        asset.with_loudness_gain_db(gain)
    }

    /// The loudness normalization switch (`docs/specs/95-loudness-importers.md` § 2.5):
    /// `Some(table)` gives every sound registered from a file ([`Self::add_file`]) the table's
    /// gain for that file, and every later `add_file`/`reload_file` too; `None` returns them to
    /// 0 dB. Sounds not registered from a file keep their own gain. Handles stay valid (each
    /// sound is replaced in place). Returns how many sounds changed gain.
    pub fn set_loudness_normalization(&mut self, table: Option<LoudnessTable>) -> usize {
        let _span = tracing::debug_span!(target: "regia_audio", "set_loudness_normalization").entered();
        self.loudness = table;
        let mut changed = 0;
        let files: Vec<(PathBuf, Vec<String>)> =
            self.files.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (key, names) in files {
            for name in names {
                let Some(handle) = self.names.get(&name).cloned() else {
                    continue;
                };
                let Some(asset) = self.assets.get(&handle).cloned() else {
                    continue;
                };
                let before = asset.loudness_gain_db();
                let asset = self.with_table_gain(asset, &key);
                if asset.loudness_gain_db() != before {
                    changed += 1;
                    let id = AssetId::sub_asset(AssetId::NIL, &name);
                    let handle = self.assets.insert(id, asset);
                    self.names.insert(name, handle);
                }
            }
        }
        changed
    }

    /// The active loudness table (`None` when normalization is off).
    pub fn loudness_normalization(&self) -> Option<&LoudnessTable> {
        self.loudness.as_ref()
    }

    /// How many times the sound registered as `name` was reloaded from its file.
    pub fn revision(&self, name: &str) -> u64 {
        self.revisions.get(name).copied().unwrap_or(0)
    }

    /// Renders the `.loopgen.ron` text (a cache hit for identical text) and registers the loop
    /// under `name`.
    ///
    /// # Errors
    ///
    /// The text is not a valid loop generator; the error names the key path.
    pub fn add_loopgen(
        &mut self,
        name: impl Into<String>,
        text: &str,
    ) -> Result<Handle<SoundAsset>, AudioError> {
        let asset = self.loopgens.sound_asset(text)?;
        let name = name.into();
        let hash = LoopGenCache::content_hash(text);
        let handle = self.add(name.clone(), asset);
        self.loopgen_names.insert(name, hash);
        Ok(handle)
    }

    /// Reads the `.loopgen.ron` file at `path` and registers its loop under `name`
    /// ([`Self::add_loopgen`]). Loop generators are not hot-reloaded in v1.
    ///
    /// # Errors
    ///
    /// The file could not be read, or is not a valid loop generator.
    pub fn add_loopgen_file(
        &mut self,
        name: impl Into<String>,
        path: impl AsRef<Path>,
    ) -> Result<Handle<SoundAsset>, AudioError> {
        let path = path.as_ref();
        let text =
            std::fs::read_to_string(path).map_err(|e| AudioError::load(path.display().to_string(), e))?;
        self.add_loopgen(name, &text)
    }

    /// The loop-generator cache (renders, hit and miss counts).
    pub fn loopgens(&self) -> &LoopGenCache {
        &self.loopgens
    }

    /// Sound names registered from a loop generator, with the content hash of their file.
    pub fn loopgen_names(&self) -> impl Iterator<Item = (&str, u64)> {
        self.loopgen_names.iter().map(|(n, h)| (n.as_str(), *h))
    }

    /// Number of registered sounds.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// True when no sound is registered.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// `Update`: re-decodes sound files that changed on disk ([`SoundAssets::reload_file`]) and
/// records the result in [`regia_asset::AssetWatchStatus`].
pub fn reload_sound_files(
    mut changes: MessageReader<regia_asset::SourceAssetChanged>,
    mut sounds: ResMut<SoundAssets>,
    mut status: Option<ResMut<regia_asset::AssetWatchStatus>>,
    assets: Option<Res<regia_asset::AssetServer>>,
) {
    for change in changes.read() {
        if change.kind != regia_asset::SourceKind::Audio {
            continue;
        }
        let started = std::time::Instant::now();
        let outcome = match sounds.reload_file(&change.path) {
            Ok(0) => continue,
            Ok(_) => regia_asset::WatchOutcome::Reloaded,
            Err(err) => {
                tracing::error!(target: "regia_audio", "{err} (keeping the previous sound)");
                regia_asset::WatchOutcome::Error(err.to_string())
            }
        };
        let shown = match assets.as_deref() {
            Some(server) => regia_asset::display_path(server.project_root(), &change.path),
            None => change.path.display().to_string(),
        };
        if let Some(status) = status.as_deref_mut() {
            let line = status.record(regia_asset::WatchEntry {
                path: shown,
                kind: regia_asset::SourceKind::Audio,
                millis: started.elapsed().as_millis() as u64,
                outcome,
            });
            tracing::info!(target: "regia_audio", "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::SoundClip;

    fn asset(value: f32) -> SoundAsset {
        SoundAsset::from_clip(&SoundClip::new(vec![value; 64], 48_000).expect("clip"))
    }

    #[test]
    fn names_and_handles_resolve_to_the_same_sound() {
        let mut sounds = SoundAssets::default();
        let a = sounds.add("a", asset(0.1));
        let b = sounds.add("b", asset(0.2));
        assert_ne!(a, b);
        assert_eq!(sounds.get(&a), sounds.by_name("a"));
        assert_eq!(sounds.name_of(&b), Some("b"));
        assert_eq!(sounds.names().collect::<Vec<_>>(), vec!["a", "b"]);
        assert!(sounds.by_name("missing").is_none());
    }

    #[test]
    fn re_adding_a_name_replaces_the_sound() {
        let mut sounds = SoundAssets::default();
        let short = SoundAsset::from_clip(&SoundClip::new(vec![0.1; 64], 48_000).expect("clip"));
        let long = SoundAsset::from_clip(&SoundClip::new(vec![0.3; 128], 48_000).expect("clip"));
        let long_secs = long.duration_secs();
        let first = sounds.add("hum", short);
        let second = sounds.add("hum", long);
        assert_eq!(sounds.len(), 1);
        assert_eq!(sounds.handle("hum"), Some(second.clone()));
        // Re-adding replaces in place: the same handle resolves, and to the new (longer) sound.
        assert_eq!(first, second);
        let got = sounds.get(&first).expect("the handle still resolves");
        assert_eq!(got.duration_secs(), long_secs);
        assert_eq!(
            sounds.by_name("hum").map(SoundAsset::duration_secs),
            Some(long_secs)
        );
    }

    /// 16-bit mono PCM. `frames` samples, a quiet ramp.
    fn pcm_wav(rate: u32, frames: u32) -> Vec<u8> {
        let data_len = frames * 2;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            out.extend_from_slice(&((i % 2000) as i16).to_le_bytes());
        }
        out
    }

    fn cooked_loudness() -> crate::loudness_table::SoundLoudness {
        crate::loudness_table::SoundLoudness {
            integrated_lufs: Some(-20.0),
            loudness_range_lu: 0.0,
            true_peak_db: Some(-6.0),
            short_clip: true,
            dual_mono: true,
            sample_rate: 8_000,
            frames: 100,
            normalize: false,
            target_lufs: -23.0,
            gain_db: 0.0,
            peak_limited: false,
        }
    }

    #[test]
    fn packed_audio_stops_at_the_decode_limit() {
        let payload = crate::cooked_sound::CookedSound {
            loudness: cooked_loudness(),
            data: pcm_wav(8_000, 100),
        }
        .to_bytes()
        .expect("the payload encodes");
        let tight = crate::SoundDecodeLimits {
            max_secs: 0.005,
            ..crate::SoundDecodeLimits::default()
        };
        let mut sounds = SoundAssets::default();
        let error = sounds
            .add_packed_with_limits("hit", "hit.wav", &payload, &tight)
            .expect_err("100 frames is past a 40-frame cap");
        match error {
            AudioError::DecodeLimit {
                max_frames,
                sample_rate,
                ..
            } => {
                assert_eq!(max_frames, 40);
                assert_eq!(sample_rate, 8_000);
            }
            other => panic!("expected DecodeLimit, got {other:?}"),
        }
        let within = crate::SoundDecodeLimits {
            max_secs: 1.0,
            ..crate::SoundDecodeLimits::default()
        };
        sounds
            .add_packed_with_limits("hit", "hit.wav", &payload, &within)
            .expect("100 frames is inside a 1 second cap");
        assert!(sounds.by_name("hit").is_some());
    }
}
