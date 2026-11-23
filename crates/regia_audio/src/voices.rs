// SPDX-License-Identifier: MPL-2.0
//! Voice management: the registry of playing sounds, per-bus activity and virtualisation
//! (`docs/specs/16-audio.md` § 3).
//!
//! Every live sound is a [`VoiceEntry`]. Real entries own their kira handle; virtual entries
//! have given the handle up (they are too far away or the voice budget is exhausted) but keep a
//! time cursor so they can revive in place. [`VoiceManager`] is pure state — starting, stopping
//! and reviving kira handles is executed by [`crate::Audio`], which owns the backend.

use std::collections::HashMap;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Entry examinations performed by voice lookup and by one reconcile pass.
/// Compiled for unit tests only; release builds do not touch it.
#[cfg(test)]
static LOOKUP_PROBES: AtomicU64 = AtomicU64::new(0);

fn note_lookup(examined: u64) {
    #[cfg(test)]
    if examined > 0 {
        LOOKUP_PROBES.fetch_add(examined, Ordering::Relaxed);
    }
    #[cfg(not(test))]
    let _ = examined;
}

/// How many voice entries lookups have examined since the last [`reset_lookup_probes`].
#[cfg(test)]
#[must_use]
pub fn lookup_probes() -> u64 {
    LOOKUP_PROBES.load(Ordering::Relaxed)
}

/// Zeroes [`lookup_probes`].
#[cfg(test)]
pub fn reset_lookup_probes() {
    LOOKUP_PROBES.store(0, Ordering::Relaxed);
}

use regia_ecs::prelude::*;

use crate::sound::{PlaybackParams, SoundAsset};
use crate::spatial::gain_to_db;

/// Identifies one playing voice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SoundInstanceId(pub u64);

/// What a voice is for: drives ducking activity keys, stealing and debugging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceKind {
    /// A fire-and-forget one-shot (`play` / `play_at` / cue pick).
    OneShot,
    /// A looping sound attached to an entity's [`crate::spatial::AudioEmitter`].
    Emitter(Entity),
    /// A music stem or track.
    Music,
    /// An announcer voice line.
    Announcer,
    /// Voice-chat playback (exempt from the announcer duck).
    VoiceChat,
    /// One layer of a parametric patch.
    PatchLayer,
}

impl VoiceKind {
    /// The ducking activity key this voice contributes to (`"Voice"`, `"Announcer"`, or the bus).
    fn activity_key<'a>(&self, bus: &'a str) -> &'a str {
        match self {
            Self::Announcer => "Announcer",
            Self::VoiceChat => "Voice",
            _ => bus,
        }
    }
}

/// One voice scored for a reconcile pass. Reused so the budget decision does not scan by id.
struct ScoredVoice {
    score: f32,
    id: SoundInstanceId,
    real: bool,
    inaudible: bool,
    stopping: bool,
}

/// One playing (or virtualised) voice.
pub struct VoiceEntry {
    /// Unique id of this voice.
    pub id: SoundInstanceId,
    /// What the voice is for.
    pub kind: VoiceKind,
    /// Bus it plays on (name).
    pub bus: Arc<str>,
    /// Cue name when this voice was started by a cue pick (for concurrency limits and stealing).
    pub cue: Option<Arc<str>>,
    /// The sound, kept so a virtualised voice can be revived.
    pub asset: SoundAsset,
    /// The parameters it was started with (revive re-uses them).
    pub params: PlaybackParams,
    /// Volume without spatialisation, dB.
    pub base_volume_db: f32,
    /// Spatial amplitude multiplier 0..1 (1 for 2D voices).
    pub gain: f32,
    /// Higher survives the voice budget better (announcer > music > sfx).
    pub priority: f32,
    /// The live kira handle; `None` while virtualised.
    pub instance: Option<crate::instance::SoundInstance>,
    /// The one-off filtered child track a filtered cue plays on, held so kira cannot remove the
    /// (`persist_until_sounds_finish`) track before the sound lands; dropped with the entry.
    pub filter_track: Option<kira::track::TrackHandle>,
    /// Playback cursor while virtualised, seconds.
    pub virtual_cursor_secs: f64,
    /// Age of the voice, seconds (steal-oldest ordering).
    pub age_secs: f64,
    /// True while the entry has been asked to stop (fade-out in progress).
    pub stopping: bool,
    /// True while the host paused the voice (`Audio::pause_voice`): a replay (emitter track
    /// rebuild) or a revive re-applies the pause, so an internal rebuild never un-pauses it.
    pub paused: bool,
    /// True while the gameplay pause holds the voice (`Audio::set_game_paused`, ADR-10105): like
    /// [`Self::paused`], but the pause owns it and resumes it when the game unpauses.
    pub held: bool,
}

impl VoiceEntry {
    /// Stops the live handle (if any) and keeps its playback position as the virtual cursor, so
    /// the voice revives in place instead of restarting.
    pub fn virtualise(&mut self) {
        if let Some(mut instance) = self.instance.take() {
            self.virtual_cursor_secs = instance.position().max(0.0);
            instance.stop(Duration::ZERO);
        }
    }

    /// The volume the voice should play at right now, in dB: its base volume, plus the asset's
    /// default volume and loudness gain (spec 16 § 2, ADR-1855; the same offsets its start data
    /// carried), plus the spatial gain. Every per-frame update sends this, so a positional or
    /// emitter voice keeps the asset's level (ADR-5926).
    pub fn effective_volume_db(&self) -> f32 {
        self.base_volume_db + self.asset.play_volume_offset_db() + gain_to_db(self.gain.clamp(0.0, 1.0))
    }

    /// The pitch ratio the voice plays at before Doppler: its own pitch times the asset's
    /// default pitch (spec 16 § 2), the same product its start data carried (ADR-5926).
    pub fn effective_pitch(&self) -> f32 {
        self.params.pitch * self.asset.default_pitch()
    }

    /// True when the voice is silent enough to virtualise (below the audibility threshold).
    pub fn inaudible(&self, threshold_gain: f32) -> bool {
        // A NaN gain (a NaN transform upstream) is silent at the mixer: it must not hold a voice.
        self.gain.is_nan() || self.gain <= threshold_gain || self.stopping
    }

    /// Sort score for the voice budget: higher keeps the voice. A NaN gain scores as silence
    /// (`total_cmp` would otherwise rank it above every real voice).
    fn keep_score(&self) -> f32 {
        let gain = if self.gain.is_nan() { 0.0 } else { self.gain };
        self.priority * 1_000.0 + gain
    }
}

/// Priority presets (spec § 3a priorities, applied to voices).
pub mod voice_priority {
    /// Ambient filler.
    pub const LOW: f32 = 0.0;
    /// Ordinary gameplay SFX.
    pub const NORMAL: f32 = 1.0;
    /// Music stems and announcers.
    pub const HIGH: f32 = 2.0;
    /// Duck-relevant voice chat and critical lines.
    pub const CRITICAL: f32 = 3.0;
}

/// Actions the reconciler wants performed on kira handles.
#[derive(Default, Debug)]
pub struct ReconcileActions {
    /// Voices to virtualise (their handles should be stopped and dropped).
    pub virtualise: Vec<SoundInstanceId>,
    /// Virtual voices to bring back to life (play again, seek to the cursor, short fade-in).
    pub revive: Vec<SoundInstanceId>,
}

/// The registry of live voices and the virtualisation policy.
pub struct VoiceManager {
    /// All live voices, real and virtual.
    pub entries: Vec<VoiceEntry>,
    /// Slot of each id. Rebuilt when `entries` shrinks. A public edit of `entries` that
    /// disagrees with this map falls back to a scan.
    index: HashMap<SoundInstanceId, usize>,
    /// Real-voice counts by activity key. Keys stay after they hit zero so the next frame
    /// does not allocate them again.
    activity: HashMap<String, u32>,
    /// Reconcile scratch. Capacity sticks across frames.
    ranked: Vec<ScoredVoice>,
    /// Voices considered for virtualising or reviving, best or worst first.
    pick: Vec<(f32, SoundInstanceId)>,
    next_id: u64,
    limit: usize,
    threshold_gain: f32,
}

impl VoiceManager {
    /// A manager allowing `limit` real voices, virtualising anything quieter than
    /// `threshold_gain` amplitude.
    pub fn new(limit: usize, threshold_gain: f32) -> Self {
        Self {
            entries: Vec::new(),
            index: HashMap::new(),
            activity: HashMap::new(),
            ranked: Vec::new(),
            pick: Vec::new(),
            next_id: 1,
            limit: limit.max(1),
            threshold_gain,
        }
    }

    /// Drops every voice and its index. Activity keys stay, at zero.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.index.clear();
        for count in self.activity.values_mut() {
            *count = 0;
        }
    }

    /// Keeps persistent ownership after every mixer handle has been detached.
    pub(crate) fn retain_for_output_resume(&mut self, ids: &std::collections::HashSet<SoundInstanceId>) {
        self.entries
            .retain(|entry| ids.contains(&entry.id) && !entry.stopping);
        self.reindex();
        for count in self.activity.values_mut() {
            *count = 0;
        }
    }

    /// The activity counts from the last [`Self::tick_state`].
    pub(crate) fn activity(&self) -> &HashMap<String, u32> {
        &self.activity
    }

    /// Adds one to `key`, inserting it the first time.
    pub(crate) fn bump_activity(&mut self, key: &str) {
        self.bump_activity_by(key, 1);
    }

    /// Raises `key`'s activity count by `n` (no key allocation when the key is already there).
    pub(crate) fn bump_activity_by(&mut self, key: &str, n: u32) {
        if let Some(count) = self.activity.get_mut(key) {
            *count += n;
        } else {
            self.activity.insert(key.to_string(), n);
        }
    }

    /// The maximum number of real voices.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Registers a new voice and returns its id.
    pub fn push(&mut self, mut entry: VoiceEntry) -> SoundInstanceId {
        let id = SoundInstanceId(self.next_id);
        self.next_id += 1;
        entry.id = id;
        let slot = self.entries.len();
        self.entries.push(entry);
        self.index.insert(id, slot);
        id
    }

    /// Removes a voice outright, returning its entry. Used when a request that already
    /// registered voices fails: the rollback stops and drops them.
    pub fn remove(&mut self, id: SoundInstanceId) -> Option<VoiceEntry> {
        let index = self.entries.iter().position(|e| e.id == id)?;
        Some(self.entries.swap_remove(index))
    }

    /// The worst-scoring real voice to virtualise when the budget is over-committed, or `None`
    /// while within the limit. Called when a voice starts so a burst inside one frame cannot
    /// exceed the limit even transiently.
    pub fn over_budget_victim(&self) -> Option<SoundInstanceId> {
        let real = self.entries.iter().filter(|e| e.instance.is_some()).count();
        if real <= self.limit {
            return None;
        }
        self.entries
            .iter()
            .filter(|e| e.instance.is_some())
            .min_by(|a, b| a.keep_score().total_cmp(&b.keep_score()))
            .map(|e| e.id)
    }

    /// How many voices currently own a kira handle.
    pub fn real_count(&self) -> usize {
        self.entries.iter().filter(|e| e.instance.is_some()).count()
    }

    /// How many voices are virtualised.
    pub fn virtual_count(&self) -> usize {
        self.entries.iter().filter(|e| e.instance.is_none()).count()
    }

    /// A voice by id.
    pub fn get(&self, id: SoundInstanceId) -> Option<&VoiceEntry> {
        if let Some(&slot) = self.index.get(&id)
            && self.entries.get(slot).is_some_and(|entry| entry.id == id)
        {
            note_lookup(1);
            return Some(&self.entries[slot]);
        }
        let mut seen = 0u64;
        let found = self.entries.iter().find(|entry| {
            seen += 1;
            entry.id == id
        });
        note_lookup(if found.is_some() {
            seen
        } else {
            self.entries.len() as u64
        });
        found
    }

    /// A voice by id, mutably.
    pub fn get_mut(&mut self, id: SoundInstanceId) -> Option<&mut VoiceEntry> {
        if let Some(&slot) = self.index.get(&id)
            && self.entries.get(slot).is_some_and(|entry| entry.id == id)
        {
            note_lookup(1);
            return Some(&mut self.entries[slot]);
        }
        let len = self.entries.len() as u64;
        let mut seen = 0u64;
        let found = self.entries.iter_mut().find(|entry| {
            seen += 1;
            entry.id == id
        });
        note_lookup(if found.is_some() { seen } else { len });
        found
    }

    fn reindex(&mut self) {
        self.index.clear();
        for (slot, entry) in self.entries.iter().enumerate() {
            self.index.insert(entry.id, slot);
        }
    }

    /// Removes finished voices, advances ages and (unpaused) virtual cursors by `dt`, and returns the
    /// per-source activity counts for ducking.
    ///
    /// The returned map is a clone of the counts `tick_state` keeps. The frame path
    /// uses `tick_state` so a steady mix does not clone it.
    pub fn tick(&mut self, dt: f64) -> HashMap<String, u32> {
        self.tick_state(dt);
        self.activity.clone()
    }

    /// [`Self::tick`] without cloning the activity map.
    pub(crate) fn tick_state(&mut self, dt: f64) {
        for entry in &mut self.entries {
            // A voice the gameplay pause holds does not age either (ADR-10105).
            if entry.held {
                continue;
            }
            entry.age_secs += dt;
            // A paused voice holds its place while virtual too (`pause_voice`: never heard again
            // until `resume_voice`), so it is neither pruned nor revived later in the sound.
            if entry.instance.is_none() && !entry.paused {
                // The cursor is in asset time: playback consumes `effective_pitch` asset
                // seconds per wall second (params.pitch × the asset's default pitch, the
                // rate the live handle plays at, ADR-5926). A NaN or negative rate
                // advances nothing.
                entry.virtual_cursor_secs += dt * f64::from(entry.effective_pitch()).max(0.0);
            }
        }
        let before = self.entries.len();
        self.entries.retain(|entry| {
            if entry.stopping {
                let stopped = entry.instance.as_ref().is_none_or(|i| i.is_stopped());
                return !stopped;
            }
            match entry.instance.as_ref() {
                Some(instance) => !instance.is_stopped(),
                None => {
                    // Virtual: survives until its (non-looping) content runs out. An empty sound
                    // never loops (`checked_loop_region`), so it never survives on its loop.
                    (entry.params.loop_region.is_some() && entry.asset.duration_secs() != 0.0)
                        || entry.virtual_cursor_secs < entry.asset.duration_secs()
                }
            }
        });
        if self.entries.len() != before {
            self.reindex();
        }

        let VoiceManager {
            entries, activity, ..
        } = self;
        for count in activity.values_mut() {
            *count = 0;
        }
        for entry in entries.iter() {
            if entry.instance.is_none() {
                continue;
            }
            let key = entry.kind.activity_key(&entry.bus);
            if let Some(count) = activity.get_mut(key) {
                *count += 1;
            } else {
                activity.insert(key.to_string(), 1);
            }
        }
    }

    /// Decides which voices to virtualise and which to revive. Pure: no kira calls.
    ///
    /// Scores each voice once. It does not look voices up again while it sorts.
    pub fn reconcile(&mut self) -> ReconcileActions {
        let mut actions = ReconcileActions::default();
        let threshold = self.threshold_gain;
        note_lookup(self.entries.len() as u64);
        {
            let VoiceManager { entries, ranked, .. } = self;
            ranked.clear();
            for entry in entries.iter() {
                ranked.push(ScoredVoice {
                    score: entry.keep_score(),
                    id: entry.id,
                    real: entry.instance.is_some(),
                    inaudible: entry.inaudible(threshold),
                    stopping: entry.stopping,
                });
            }
        }

        let mut kept = 0usize;
        for voice in &self.ranked {
            if voice.real && voice.inaudible {
                actions.virtualise.push(voice.id);
            } else if voice.real {
                kept += 1;
            }
        }

        if kept > self.limit {
            self.pick.clear();
            for voice in &self.ranked {
                if voice.real && !voice.inaudible {
                    self.pick.push((voice.score, voice.id));
                }
            }
            self.pick.sort_by(|a, b| a.0.total_cmp(&b.0));
            let excess = kept - self.limit;
            for (_, id) in self.pick.iter().take(excess) {
                actions.virtualise.push(*id);
            }
            kept -= excess;
        }

        let budget = self.limit.saturating_sub(kept);
        if budget > 0 {
            self.pick.clear();
            for voice in &self.ranked {
                if !voice.real && !voice.inaudible && !voice.stopping {
                    self.pick.push((voice.score, voice.id));
                }
            }
            if !self.pick.is_empty() {
                self.pick.sort_by(|a, b| b.0.total_cmp(&a.0));
                for (_, id) in self.pick.iter().take(budget) {
                    actions.revive.push(*id);
                }
            }
        }

        actions
    }

    /// Counts live (real) voices started from cue `cue`.
    pub fn cue_active(&self, cue: &str) -> usize {
        self.entries
            .iter()
            .filter(|e| e.instance.is_some() && e.cue.as_deref() == Some(cue))
            .count()
    }

    /// The id of the oldest live voice of cue `cue` (steal policy `Oldest`).
    pub fn oldest_of_cue(&self, cue: &str) -> Option<SoundInstanceId> {
        self.entries
            .iter()
            .filter(|e| e.cue.as_deref() == Some(cue) && e.instance.is_some())
            .max_by(|a, b| a.age_secs.total_cmp(&b.age_secs))
            .map(|e| e.id)
    }

    /// The id of the quietest live voice of cue `cue` (steal policy `Quietest`).
    pub fn quietest_of_cue(&self, cue: &str) -> Option<SoundInstanceId> {
        self.entries
            .iter()
            .filter(|e| e.cue.as_deref() == Some(cue) && e.instance.is_some())
            .min_by(|a, b| a.effective_volume_db().total_cmp(&b.effective_volume_db()))
            .map(|e| e.id)
    }

    /// Every registered voice, real and virtual, oldest first.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &VoiceEntry> {
        self.entries.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::SoundClip;
    use crate::sound::SoundAsset;

    fn asset(duration_secs: f64) -> SoundAsset {
        let samples = (duration_secs * 48_000.0) as usize;
        let clip = SoundClip::new(vec![0.2; samples.max(1)], 48_000).expect("clip");
        SoundAsset::from_clip(&clip)
    }

    fn entry(id: u64, gain: f32) -> VoiceEntry {
        VoiceEntry {
            id: SoundInstanceId(id),
            kind: VoiceKind::OneShot,
            bus: "Sfx".into(),
            cue: None,
            asset: asset(10.0),
            params: PlaybackParams::default(),
            base_volume_db: 0.0,
            gain,
            priority: voice_priority::NORMAL,
            instance: None,
            filter_track: None,
            virtual_cursor_secs: 0.0,
            age_secs: 0.0,
            stopping: false,
            paused: false,
            held: false,
        }
    }

    #[test]
    fn virtual_cursor_advances_at_the_effective_pitch() {
        // The real playback rate is params.pitch × the asset's default pitch (ADR-5926): a
        // virtualised voice of a 0.5×-pitched asset must advance its cursor at half the wall
        // rate, or it is pruned (and revives) a factor too early.
        let mut manager = VoiceManager::new(64, 0.0);
        let mut pitched = entry(1, 1.0);
        pitched.asset =
            SoundAsset::from_clip(&crate::clip::SoundClip::new(vec![0.2; 480_000], 48_000).expect("clip"))
                .with_default_pitch(0.5);
        pitched.params.pitch = 1.5;
        manager.push(pitched);
        let _ = manager.tick(1.0);
        assert!(
            (manager.entries[0].virtual_cursor_secs - 0.75).abs() < 1e-9,
            "cursor advanced at 1.5 × 0.5: {}",
            manager.entries[0].virtual_cursor_secs
        );
    }

    /// Rev-kimi-28 (audio2-k finding 6): a paused voice that virtualises keeps its place. Its
    /// virtual cursor advanced while paused, so it was pruned once the cursor passed its length
    /// (or revived later in the sound), against `pause_voice`'s "never heard again until
    /// `resume_voice`".
    #[test]
    fn a_paused_virtual_voice_keeps_its_cursor() {
        let mut manager = VoiceManager::new(64, 0.0);
        let mut paused = entry(1, 1.0);
        paused.paused = true;
        paused.virtual_cursor_secs = 2.0;
        manager.push(paused);
        for _ in 0..20 {
            let _ = manager.tick(1.0);
        }
        let entry = manager
            .get(SoundInstanceId(1))
            .expect("a paused voice is not pruned");
        assert_eq!(entry.virtual_cursor_secs, 2.0, "the cursor holds while paused");
        assert_eq!(entry.age_secs, 20.0, "its age still counts");
    }

    #[test]
    fn below_threshold_entries_never_revive() {
        // Kira handles cannot be constructed without a manager, so the pure paths are tested
        // here and real-handle behaviour in `tests/acceptance.rs` through the offline backend.
        let mut manager = VoiceManager::new(64, 0.01);
        manager.push(entry(1, 0.0));
        manager.push(entry(2, 0.001));
        let actions = manager.reconcile();
        assert!(actions.revive.is_empty(), "inaudible voices never revive");
        assert!(actions.virtualise.is_empty(), "already virtual");
    }

    #[test]
    fn finished_voices_are_pruned_by_cursor() {
        let mut manager = VoiceManager::new(64, 0.01);
        // A short one-shot that runs out while virtual.
        manager.push(entry(1, 1.0));
        manager.entries[0].virtual_cursor_secs = 20.0; // past its 10 s duration
        let _ = manager.tick(0.016);
        assert!(manager.entries.is_empty(), "the finished one-shot is pruned");

        // Looping virtual voices survive.
        let mut looping = entry(2, 1.0);
        looping.params.loop_region = Some(crate::sound::LoopRegion {
            start_secs: 0.0,
            end_secs: None,
        });
        manager.push(looping);
        let _ = manager.tick(0.016);
        assert_eq!(manager.entries.len(), 1);
    }

    #[test]
    fn virtual_cursor_advances_at_the_playback_rate() {
        let mut manager = VoiceManager::new(64, 0.01);
        let mut slow = entry(1, 1.0);
        slow.params.pitch = 0.5;
        let mut fast = entry(2, 1.0);
        fast.params.pitch = 2.0;
        let mut nan = entry(3, 1.0);
        nan.params.pitch = f32::NAN;
        manager.push(slow);
        manager.push(fast);
        manager.push(nan);
        let _ = manager.tick(2.0);
        let cursor = |m: &VoiceManager, id: u64| m.get(SoundInstanceId(id)).map(|e| e.virtual_cursor_secs);
        assert_eq!(cursor(&manager, 1), Some(1.0));
        assert_eq!(cursor(&manager, 2), Some(4.0));
        assert_eq!(cursor(&manager, 3), Some(0.0));
    }

    #[test]
    fn slow_virtual_one_shot_outlives_wall_time_duration() {
        let mut manager = VoiceManager::new(64, 0.01);
        let mut slow = entry(1, 1.0); // 10 s asset
        slow.params.pitch = 0.5;
        manager.push(slow);
        let _ = manager.tick(12.0); // 6 asset seconds consumed
        assert_eq!(manager.entries.len(), 1, "half-rate playback is not done at 12 s");
        let _ = manager.tick(10.0); // 11 asset seconds
        assert!(manager.entries.is_empty());
    }

    #[test]
    fn effective_volume_combines_base_and_gain() {
        let mut e = entry(1, 0.5);
        e.base_volume_db = -6.0;
        assert!(
            (e.effective_volume_db() - (-12.0206)).abs() < 0.01,
            "-6 dB + half amplitude (≈ -6.02 dB), got {}",
            e.effective_volume_db()
        );
        e.gain = 0.0;
        assert!(e.effective_volume_db() < -99.0);
        assert!(e.inaudible(0.01));
        e.gain = 0.5;
        e.stopping = true;
        assert!(e.inaudible(0.01), "stopping voices are virtualization candidates");
    }

    #[test]
    fn voice_lookup_stays_linear_in_virtual_loops() {
        let mut manager = VoiceManager::new(64, 0.01);
        let mut ids = Vec::new();
        for i in 0..48 {
            let mut voice = entry(i, 1.0);
            voice.params.loop_region = Some(crate::sound::LoopRegion {
                start_secs: 0.0,
                end_secs: None,
            });
            ids.push(manager.push(voice));
        }
        reset_lookup_probes();
        for id in ids {
            assert!(manager.get(id).is_some());
        }
        let probes = lookup_probes();
        assert!(
            probes <= 48 * 2,
            "48 lookups examined {probes} entries; an index examines one slot each"
        );

        let mut crowded = VoiceManager::new(8, 0.0);
        for i in 0..64 {
            let mut voice = entry(i, 1.0);
            voice.params.loop_region = Some(crate::sound::LoopRegion {
                start_secs: 0.0,
                end_secs: None,
            });
            crowded.push(voice);
        }
        reset_lookup_probes();
        let actions = crowded.reconcile();
        assert_eq!(actions.revive.len(), 8, "the budget revives eight voices");
        let probes = lookup_probes();
        assert!(
            probes <= 64 * 8,
            "reconciling 64 virtual voices examined {probes} entries"
        );
    }

    #[test]
    fn activity_keys_separate_announcer_and_voice_chat_from_buses() {
        assert_eq!(VoiceKind::Announcer.activity_key("Voice"), "Announcer");
        assert_eq!(VoiceKind::VoiceChat.activity_key("Voice"), "Voice");
        assert_eq!(VoiceKind::OneShot.activity_key("World"), "World");
        assert_eq!(VoiceKind::Emitter(Entity::PLACEHOLDER).activity_key("Ui"), "Ui");
    }

    #[test]
    fn cue_queries_need_real_voices() {
        // Without a backend there are no real voices, so steal queries find nothing; the
        // with-handles behaviour (oldest/quietest of a cue) is covered by the integration tests.
        let mut manager = VoiceManager::new(64, 0.01);
        let mut a = entry(1, 0.9);
        a.cue = Some("hit".into());
        a.age_secs = 5.0;
        manager.push(a);
        assert_eq!(manager.oldest_of_cue("hit"), None);
        assert_eq!(manager.quietest_of_cue("hit"), None);
        assert_eq!(manager.cue_active("hit"), 0);
    }

    #[test]
    fn a_nan_gain_is_inaudible_and_scores_as_silence() {
        let nan = entry(1, f32::NAN);
        assert!(nan.inaudible(0.01), "NaN never holds a real voice");
        assert_eq!(nan.keep_score(), entry(2, 0.0).keep_score());
        assert!(nan.keep_score() < entry(3, 0.5).keep_score());
        // A NaN voice never revives either.
        let mut manager = VoiceManager::new(64, 0.01);
        manager.push(nan);
        assert!(manager.reconcile().revive.is_empty());
    }
}
