// SPDX-License-Identifier: MPL-2.0
//! The [`Audio`] resource: the game-facing facade over the mixer (`docs/specs/16-audio.md`).
//!
//! One resource owns the output backend, the buses, snapshots, ducking, the voice manager
//! (virtualisation), cues, music, patches, the announcer and voice capture/playback. Gameplay
//! queues one-shots through [`Audio::play`]/[`Audio::play_at`]/[`Audio::play_cue`]; the drain
//! system hands them to the backend, and the `AudioSync` tick advances everything else.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kira::{
    effect::filter::{FilterBuilder, FilterHandle, FilterMode},
    track::{TrackBuilder, TrackHandle},
};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;

use crate::announcer::{AnnounceOutcome, Announcer, AnnouncerBank};
use crate::backend::{OutputBackend, PlayedOneShot};
use crate::bus::{BusLayout, Buses, tween};
use crate::clip::SoundClip;
use crate::cue::{CueSelector, SoundCue, StealPolicy};
use crate::ducking::DuckEngine;
use crate::error::AudioError;
use crate::instance::SoundInstance;
use crate::music::MusicSystem;
use crate::patch::{
    AudioPatch, LayerOutputs, PatchLayerKind, PatchLoop, SmoothedParam, crossfade_weights_into, layer_outputs,
};
use crate::settings::{AudioSettings, default_voice_limit};
use crate::snapshot::SnapshotStack;
use crate::sound::{LoopRegion, PlayData, PlaybackParams, SoundAsset};
use crate::spatial::{AudioEmitter, ListenerFrame, db_to_gain, gain_to_db};
use crate::voices::{SoundInstanceId, VoiceEntry, VoiceKind, VoiceManager, voice_priority};

mod emitters;
mod layout;
mod mix;
mod persistent;

pub use emitters::{EmitterStatus, EmitterUpdate};
pub use mix::BusStatus;

/// One bus's blending state (snapshot fades) and last applied volume.
#[derive(Clone, Copy, Debug)]
struct BusBlend {
    current_db: f32,
    target_db: f32,
    rate_db_per_sec: f32,
    last_applied_db: f32,
    muted: bool,
}

/// A queued one-shot (drained once per frame).
struct PlayRequest {
    asset: SoundAsset,
    params: PlaybackParams,
    position: Option<Vec3>,
    kind: VoiceKind,
    cue: Option<Arc<str>>,
    emitter: Option<AudioEmitter>,
    /// Propagation settings (ground coupling, path class); `None` = a plain world sound.
    propagation: Option<crate::coupling::AudioPropagation>,
    /// A cue's authored low-pass, Hz: each path voice of a positional play plays on a one-off
    /// filtered child track of its bus (spec 16 § 3, ADR-5926). `None` = unfiltered.
    filter_cutoff_hz: Option<f32>,
}

/// Runtime state of one registered cue.
struct CueRuntime {
    selector: CueSelector,
    cooldown_until_secs: f64,
}

/// A cue pick ready to play: the selected asset and composed params, plus what committing the
/// play needs (cooldown, caption) and what rolling it back needs (the selector before the
/// pick). Produced by `Audio::prepare_cue`.
struct CuePick {
    key: Arc<str>,
    asset: SoundAsset,
    params: PlaybackParams,
    filter_cutoff_hz: Option<f32>,
    caption: Option<crate::cue::CueCaptionDef>,
    cooldown: Duration,
    /// The cue's selector before this pick (no-repeat window, shuffle bag, sequential cursor):
    /// restored when the play fails or is inaudible, so the pick is not consumed.
    selector_before: CueSelector,
    /// The voice the steal policy chose when the cue is at `max_concurrent`. Stopped only once
    /// the new play has started (a refused, failed or inaudible play steals nothing).
    steal_victim: Option<SoundInstanceId>,
}

/// Where a cue play sounds: flat (2D), at a world position, or attached to an entity's emitter
/// (spec 16 § 3: `play_at(cue, …)` / `play_on(entity, cue)`).
enum CuePlace {
    /// 2D, centred, on the cue's bus (`play_cue`).
    Flat,
    /// A one-shot at a world position with emitter spatial parameters (`play_cue_at`).
    At { position: Vec3, emitter: AudioEmitter },
    /// Attached to an entity's emitter, following it every frame (`play_cue_on`).
    On { entity: Entity, emitter: AudioEmitter },
}

/// One spawned parametric patch.
struct PatchPlayer {
    id: u64,
    patch: AudioPatch,
    params: HashMap<String, SmoothedParam>,
    layers: Vec<PatchLayerRuntime>,
    /// The bus the layers play on.
    bus: String,
    /// Seconds of ticks since the spawn (one-shot cooldowns).
    clock_secs: f64,
}

/// One layer of a spawned patch.
struct PatchLayerRuntime {
    kind: PatchLayerKind,
    loops: Vec<PatchLoop>,
    /// The layer's filtered child track under the patch bus: its loops, grains and one-shots
    /// play on it (dropping it removes the track once they have faded). `None` without
    /// `filter_curves`.
    track: Option<TrackHandle>,
    filter: Option<FilterHandle>,
    applied: LayerOutputs,
    /// A one-shot layer's trigger state (format v2).
    shot: Option<patch_layers::OneShotRuntime>,
    /// A granular layer's voice (format v2).
    granular: Option<crate::granular::GranularHandle>,
}

/// The audio resource.
#[derive(Resource)]
pub struct Audio {
    backend: OutputBackend,
    buses: Buses,
    bus_layout: BusLayout,
    bus_mix: HashMap<String, BusBlend>,
    user_base_db: HashMap<String, f32>,
    snapshots: SnapshotStack,
    duck: DuckEngine,
    duck_rules: Vec<crate::ducking::DuckRule>,
    pub(crate) voices: VoiceManager,
    /// Host-owned loops that survive an output suspend, keyed by their stable voice id.
    persistent_loops: HashSet<SoundInstanceId>,
    queue: Vec<PlayRequest>,
    played_log: Vec<PlayedOneShot>,
    /// Entries the `played_log` cap has dropped so far: a mark (`played_log_mark`) counts
    /// these too, so a truncate back to it stays right across a half-drain.
    played_log_dropped: usize,
    played_count: u64,
    cues: HashMap<Arc<str>, SoundCue>,
    cue_runtime: HashMap<Arc<str>, CueRuntime>,
    cue_captions: Vec<crate::cue::CueCaptionPlay>,
    emitters: HashMap<Entity, emitters::EmitterState>,
    listeners: Vec<(Option<Entity>, ListenerFrame)>,
    hrir: emitters::HrirState,
    reverb: emitters::ReverbState,
    zone_field: crate::audio_zone::ZoneField,
    /// The installed rock field, when it routes sound itself (cooked portals, W-25).
    sound_paths: Option<std::sync::Arc<dyn crate::audio_zone::RockField>>,
    music: MusicSystem,
    patches: Vec<PatchPlayer>,
    next_patch_id: u64,
    announcer: Announcer,
    announcer_banks: HashMap<String, AnnouncerBank>,
    announcer_voice: Option<SoundInstanceId>,
    capture: Option<crate::voice::AudioCapture>,
    voice_playback: Option<crate::voice::VoiceStreamHandle>,
    /// Media feeds playing on the bus named `Voice` (`play_media_feed`). Counted as the `"Voice"`
    /// duck key while the handle is not stopped, including while paused (specs/16 § 1).
    voice_bus_feeds: Vec<crate::media_feed::MediaStreamHandle>,
    now_secs: f64,
    /// [`Self::begin_mixdown`]: the next [`Self::tick`] carries a wall-clock delta and is dropped.
    skip_next_tick: bool,
    /// The gameplay pause (ADR-10105).
    game_pause: game_pause::GamePauseState,
    suspend_count: u64,
    resume_count: u64,
    throttle_suspend_count: u64,
    /// Test fault: the next this-many `resume_output` calls fail as if no device opened.
    #[cfg(test)]
    failing_resumes: u32,
    /// True between a [`Audio::suspend_output`] and the [`Audio::resume_output`] that re-opened
    /// the output: the mixer the voices played on is gone (device) or about to be rebuilt.
    suspended: bool,
    rng: regia_core::math::Rng,
    /// The medium, the listener's contact and the path voices (`audio_propagation.rs`).
    propagation: propagation_impl::PropagationState,
    /// Streaming underrun watch (`audio/streams.rs`, ADR-872).
    streams: streams::StreamWatch,
    /// The render-out mixdown running on its own offline mixer (`audio/mixdown.rs`, ADR-1520).
    mixdown: Option<mixdown::MixdownState>,
    /// A resume that arrived while a mixdown held the bus tree on its own mixer: applied by
    /// `end_mixdown` on the restored output. Resuming mid-take would rebuild the buses on the
    /// mixdown's mixer and drop the render-out stem taps.
    resume_after_mixdown: bool,
    /// The sample rate of the output the settings opened (the stream watch's rate outside a
    /// mixdown).
    output_sample_rate: u32,
    /// Entities in the current [`Audio::sync_emitter_frames`] call. Cleared and refilled so the
    /// set is not allocated every frame.
    sync_live: HashSet<Entity>,
    /// Crossfade weights of the patch layer being ticked.
    patch_weights: Vec<(usize, f32)>,
    /// Key-index order for [`crossfade_weights_into`](crate::patch::crossfade_weights_into).
    patch_order: Vec<usize>,
    /// Previous ground-interface name while a propagation solve folds the material chain.
    ground_name_scratch: String,
    /// Occlusion rays for the current [`Audio::sync_emitter_frames`] call. Cleared and refilled
    /// so a steady in-range emitter does not allocate a new vector every frame.
    occlusion_rays: Vec<crate::occlusion::OcclusionRay>,
    /// Impulse responses, the tier and the zone IR (`audio/convolution.rs`, ADR-2721).
    convolution: convolution::ConvolutionState,
    /// Time-stretched voices (`audio/stretch.rs`, ADR-2721).
    stretch: stretch::StretchState,
    /// Playing `.ambience.ron` files (`audio/ambience_files.rs`, ADR-2721).
    ambience_files: ambience_files::AmbienceFiles,
    /// The filters of low-pass voices (`audio/lowpass_voice.rs`, ADR-3197).
    lowpass: lowpass_voice::LowpassVoices,
}

/// The cue picker's seed: at creation, and again when a render-out mixdown starts (ADR-1520).
pub(crate) const CUE_SEED: u64 = 0x5EED_0A11;

impl Audio {
    /// The most recent one-shot plays `played()` keeps: a diagnostics tail for tests and
    /// tools, not a session-long history. Past the cap the oldest half is dropped at once, so
    /// the amortized cost per play stays constant.
    const PLAYED_LOG_CAP: usize = 4096;
    /// Creates the audio resource for `settings`: opens the backend, builds the buses. Never
    /// panics and never fails — without a device the offline backend is used, and a rejected
    /// layout falls back to no buses (plays fail with [`AudioError::UnknownBus`]).
    ///
    /// Games normally get this from [`AudioPlugin`](crate::AudioPlugin); constructing it
    /// directly is for tools and tests that drive the mixer without an app.
    pub fn new(settings: &AudioSettings) -> Self {
        // An absurdly low offline rate would build zero-length reverb and delay lines in kira.
        let floored;
        let settings = if settings.sample_rate < crate::settings::MIN_OFFLINE_SAMPLE_RATE {
            tracing::warn!(
                target: "regia_audio",
                "offline sample rate {} Hz is too low; using {} Hz",
                settings.sample_rate,
                crate::settings::MIN_OFFLINE_SAMPLE_RATE
            );
            floored = AudioSettings {
                sample_rate: crate::settings::MIN_OFFLINE_SAMPLE_RATE,
                ..settings.clone()
            };
            &floored
        } else {
            settings
        };
        let mut backend = OutputBackend::open(settings);
        let conv = crate::bus::ConvolutionContext {
            sample_rate: settings.sample_rate,
            ..crate::bus::ConvolutionContext::default()
        };
        let buses = backend
            .build_buses(&settings.buses, &conv)
            .unwrap_or_else(|error| {
                tracing::error!(target: "regia_audio", "bus layout rejected: {error}");
                Buses::build(&BusLayout::default(), &mut crate::bus::NoHost)
                    .unwrap_or_else(|_| unreachable!("an empty layout never touches the host"))
            });
        let mut bus_mix = HashMap::new();
        let mut user_base_db = HashMap::new();
        for name in buses.names() {
            let base = buses.base_volume_db(name).unwrap_or(0.0);
            user_base_db.insert(name.to_string(), base);
            // A bus the layout starts muted stays muted until the host unmutes it.
            let muted = settings.buses.buses.iter().any(|b| b.name == name && b.mute);
            bus_mix.insert(
                name.to_string(),
                BusBlend {
                    current_db: base,
                    target_db: base,
                    rate_db_per_sec: f32::INFINITY,
                    last_applied_db: f32::NAN,
                    muted,
                },
            );
        }
        let threshold_gain = db_to_gain(settings.virtual_threshold_db);
        Self {
            voices: VoiceManager::new(
                settings.voice_limit.unwrap_or_else(default_voice_limit),
                threshold_gain,
            ),
            persistent_loops: HashSet::new(),
            duck: DuckEngine::new(&settings.duck_rules),
            duck_rules: settings.duck_rules.clone(),
            bus_layout: settings.buses.clone(),
            backend,
            buses,
            bus_mix,
            user_base_db,
            snapshots: {
                let mut stack = SnapshotStack::default();
                for snapshot in &settings.buses.snapshots {
                    stack.register(snapshot.clone());
                }
                stack
            },
            queue: Vec::new(),
            played_log: Vec::new(),
            played_log_dropped: 0,
            played_count: 0,
            cues: HashMap::new(),
            cue_runtime: HashMap::new(),
            cue_captions: Vec::new(),
            emitters: HashMap::new(),
            listeners: vec![(None, ListenerFrame::default())],
            hrir: emitters::HrirState::default(),
            reverb: emitters::ReverbState::default(),
            zone_field: crate::audio_zone::ZoneField::default(),
            sound_paths: None,
            music: MusicSystem::new(),
            patches: Vec::new(),
            next_patch_id: 1,
            announcer: Announcer::new(),
            announcer_banks: HashMap::new(),
            announcer_voice: None,
            capture: None,
            voice_playback: None,
            voice_bus_feeds: Vec::new(),
            now_secs: 0.0,
            skip_next_tick: false,
            game_pause: game_pause::GamePauseState::default(),
            suspend_count: 0,
            resume_count: 0,
            throttle_suspend_count: 0,
            #[cfg(test)]
            failing_resumes: 0,
            suspended: false,
            rng: regia_core::math::Rng::new(CUE_SEED),
            propagation: propagation_impl::PropagationState::default(),
            streams: streams::StreamWatch::new(settings.sample_rate),
            mixdown: None,
            resume_after_mixdown: false,
            output_sample_rate: settings.sample_rate,
            sync_live: HashSet::new(),
            patch_weights: Vec::new(),
            patch_order: Vec::new(),
            ground_name_scratch: String::new(),
            occlusion_rays: Vec::new(),
            convolution: convolution::ConvolutionState::default(),
            stretch: stretch::StretchState::default(),
            ambience_files: ambience_files::AmbienceFiles::default(),
            lowpass: lowpass_voice::LowpassVoices::default(),
        }
    }

    // --------------------------------------------------------------- one-shots (M3 slice API)

    /// Queues `clip` as a non-positional one-shot on `Master`.
    pub fn play(&mut self, clip: &SoundClip) -> Result<(), AudioError> {
        self.play_with(clip, None)
    }

    /// Queues `clip` as a one-shot at `position` (world space, metres); distance gain and
    /// panning are applied when it starts.
    pub fn play_at(&mut self, clip: &SoundClip, position: Vec3) -> Result<(), AudioError> {
        self.play_with(clip, Some(position))
    }

    fn play_with(&mut self, clip: &SoundClip, position: Option<Vec3>) -> Result<(), AudioError> {
        if clip.is_empty() {
            return Err(AudioError::EmptyClip);
        }
        self.queue.push(PlayRequest {
            asset: SoundAsset::from_clip(clip),
            params: PlaybackParams::default(),
            position,
            kind: VoiceKind::OneShot,
            cue: None,
            emitter: None,
            propagation: None,
            filter_cutoff_hz: None,
        });
        Ok(())
    }

    /// Queues `asset` with full playback parameters (2D: gain 1, centred).
    pub fn play_asset(&mut self, asset: &SoundAsset, params: PlaybackParams) -> Result<(), AudioError> {
        self.queue.push(PlayRequest {
            asset: asset.clone(),
            params,
            position: None,
            kind: VoiceKind::OneShot,
            cue: None,
            emitter: None,
            propagation: None,
            filter_cutoff_hz: None,
        });
        Ok(())
    }

    /// Starts `asset` now (2D, no queue) and returns its voice id, for hosts that stop, pause or
    /// retune the sounds they start ([`Audio::stop_voice`], [`Audio::pause_voice`], ...).
    /// [`Audio::play_asset`] queues the same play for the next drain and returns no id.
    pub fn start_asset(
        &mut self,
        asset: &SoundAsset,
        params: PlaybackParams,
    ) -> Result<SoundInstanceId, AudioError> {
        let mut params = params;
        let bus = resolved_params_bus(&mut params, Some(asset));
        let instance = self.play_on_bus(&bus, asset, &params)?;
        let id = self.register_voice(
            asset.clone(),
            params,
            VoiceKind::OneShot,
            None,
            1.0,
            instance,
            None,
        );
        self.played_count += 1;
        Ok(id)
    }

    /// [`Audio::play_asset_at`] started now, for hosts that follow or stop the voice. `Ok(None)`
    /// when no path reaches the listener (out of range, airless), so nothing plays.
    pub fn start_asset_at(
        &mut self,
        asset: &SoundAsset,
        params: PlaybackParams,
        position: Vec3,
        emitter: &AudioEmitter,
    ) -> Result<Option<SoundInstanceId>, AudioError> {
        let id = self.start_request(PlayRequest {
            asset: asset.clone(),
            params,
            position: Some(position),
            kind: VoiceKind::OneShot,
            cue: None,
            emitter: Some(emitter.clone()),
            propagation: None,
            filter_cutoff_hz: None,
        })?;
        self.played_count += 1;
        Ok(id)
    }

    /// Queues `asset` at a world position with emitter parameters (distance gain, panning, cone).
    pub fn play_asset_at(
        &mut self,
        asset: &SoundAsset,
        params: PlaybackParams,
        position: Vec3,
        emitter: &AudioEmitter,
    ) -> Result<(), AudioError> {
        self.queue.push(PlayRequest {
            asset: asset.clone(),
            params,
            position: Some(position),
            kind: VoiceKind::OneShot,
            cue: None,
            emitter: Some(emitter.clone()),
            propagation: None,
            filter_cutoff_hz: None,
        });
        Ok(())
    }

    /// Drains the queued one-shots into the backend. Called by the drain system once per frame;
    /// tools and tests driving [`Audio`] without an app call it directly.
    pub fn drain(&mut self) {
        let _span = tracing::info_span!("audio_drain", queued = self.queue.len()).entered();
        let _mem = regia_core::memory::enter(regia_core::memory::Subsystem::Audio);
        let mut accepted = 0;
        let queue: Vec<PlayRequest> = std::mem::take(&mut self.queue);
        for request in queue {
            let label = request.asset.label().to_string();
            match self.start_request(request) {
                Ok(_) => accepted += 1,
                Err(error) => {
                    tracing::warn!(target: "regia_audio", sound = %label, "one-shot not started: {error}");
                }
            }
        }
        self.played_count += accepted;
        if accepted > 0 {
            tracing::debug!(target: "regia_audio", accepted, "one-shots started");
        }
    }

    /// Starts one drained request (spatialisation for positional plays happens here).
    fn start_request(&mut self, request: PlayRequest) -> Result<Option<SoundInstanceId>, AudioError> {
        let _span = tracing::info_span!("audio_start_voice").entered();
        // Positional sounds travel through the level's medium (airborne and ground paths).
        if request.position.is_some() {
            return self.start_propagated(request);
        }
        let mut params = request.params;
        let bus = resolved_params_bus(&mut params, Some(&request.asset));
        let instance = self.play_on_bus(&bus, &request.asset, &params)?;
        Ok(Some(self.register_voice(
            request.asset,
            params,
            request.kind,
            request.cue,
            1.0,
            instance,
            None,
        )))
    }

    /// A position in the played log that survives the cap's half-drain: every entry ever
    /// logged up to now, dropped ones included. [`Self::truncate_played_log`] goes back to it.
    pub(crate) fn played_log_mark(&self) -> usize {
        self.played_log_dropped + self.played_log.len()
    }

    /// Drops every played entry logged after `mark` (from [`Self::played_log_mark`]), even when
    /// the cap drained the log in between: a request's own entries are always the newest, and
    /// the half-drain never reaches them (it keeps at least half the cap).
    pub(crate) fn truncate_played_log(&mut self, mark: usize) {
        self.played_log
            .truncate(mark.saturating_sub(self.played_log_dropped));
    }

    /// Registers a started voice in the manager.
    #[allow(clippy::too_many_arguments)]
    fn register_voice(
        &mut self,
        asset: SoundAsset,
        params: PlaybackParams,
        kind: VoiceKind,
        cue: Option<Arc<str>>,
        gain: f32,
        mut instance: SoundInstance,
        record_position: Option<Vec3>,
    ) -> SoundInstanceId {
        // A voice the mixer has not rendered yet can stop without a declick fade (ADR-544).
        instance.watch_render_clock(self.backend.limiter().render_clock());
        let base_volume_db = params.volume_db;
        let start_offset_secs = params.start_offset_secs;
        let sample_rate = asset.sample_rate();
        if kind == VoiceKind::OneShot {
            self.played_log.push(PlayedOneShot {
                clip_len: (asset.duration_secs() * f64::from(sample_rate)) as usize,
                sample_rate,
                position: record_position,
            });
            if self.played_log.len() > Self::PLAYED_LOG_CAP {
                // A diagnostics tail for tests and tools, not a session-long history: drop
                // the oldest half at once so the amortized cost per play stays constant. The
                // drop count keeps marks taken before it valid (`truncate_played_log`).
                self.played_log.drain(..Self::PLAYED_LOG_CAP / 2);
                self.played_log_dropped += Self::PLAYED_LOG_CAP / 2;
            }
        }
        let id = self.voices.push(VoiceEntry {
            id: SoundInstanceId(0),
            kind,
            bus: Arc::from(params.bus.as_deref().unwrap_or("Master")),
            cue,
            asset,
            params,
            base_volume_db,
            gain,
            priority: priority_for(kind),
            instance: Some(instance),
            filter_track: None,
            virtual_cursor_secs: start_offset_secs,
            age_secs: 0.0,
            stopping: false,
            paused: false,
            held: false,
        });
        // The spatial gain must be audible from the very first sample: apply it to the live
        // handle now (per-frame updates for attached emitters then continue from the same
        // `effective_volume_db` formula in `sync_emitters`).
        // NaN too (a NaN rolloff upstream): it is silence, not the authored volume.
        if (gain.is_nan() || gain < 1.0)
            && let Some(entry) = self.voices.get_mut(id)
        {
            let volume_db = entry.effective_volume_db();
            if let Some(instance) = entry.instance.as_mut() {
                instance.set_volume_db(volume_db, Duration::ZERO);
            }
        }
        // A voice that starts on a frozen bus during a gameplay pause is held at once (ADR-10105).
        self.hold_new_voice(id);
        // The voice budget is enforced the moment a voice starts, not on the next tick: a burst
        // of one-shots within one frame must not exceed the limit even transiently (spec § 3).
        while let Some(victim) = self.voices.over_budget_victim() {
            if let Some(entry) = self.voices.get_mut(victim) {
                entry.virtualise();
            }
        }
        id
    }

    /// Plays `asset` on its bus, returning the live handle.
    fn play_on_bus(
        &mut self,
        bus: &str,
        asset: &SoundAsset,
        params: &PlaybackParams,
    ) -> Result<SoundInstance, AudioError> {
        play_data_on(&mut self.buses, bus, asset.play_data(params)?)
    }

    /// Plays `asset` on a one-off low-pass child track under `bus` (the patch-layer pattern).
    /// The returned track handle must be kept (on the voice) until the sound finishes: kira removes a
    /// persisting track dropped while its arena is still empty. Falls back to the plain bus when
    /// the bus cannot host a child track.
    fn play_filtered(
        &mut self,
        bus: &str,
        asset: &SoundAsset,
        params: &PlaybackParams,
        cutoff_hz: f32,
    ) -> Result<(SoundInstance, Option<TrackHandle>), AudioError> {
        match self.buses.add_child_track(bus, low_pass_track(cutoff_hz)) {
            Some(mut track) => {
                let instance = play_data_on_track(&mut track, asset.play_data(params)?)?;
                Ok((instance, Some(track)))
            }
            None => {
                if !self.buses.contains(bus) {
                    return Err(AudioError::UnknownBus(bus.into()));
                }
                tracing::warn!(target: "regia_audio", "cue filter track unavailable on `{bus}`, playing unfiltered");
                Ok((self.play_on_bus(bus, asset, params)?, None))
            }
        }
    }

    // --------------------------------------------------------------- buses / snapshots / ducking

    /// Sets a bus volume directly (user sliders); snapshot-independent base.
    ///
    /// # Errors
    ///
    /// [`AudioError::UnknownBus`] for a bus the layout does not define; [`AudioError::Invalid`]
    /// for a non-finite `db` (NaN or an infinity would poison the bus blend and every voice
    /// on the bus; mute the bus for silence). Either way the bus keeps its volume.
    pub fn set_bus_volume(&mut self, bus: &str, db: f32, fade: Duration) -> Result<(), AudioError> {
        let Some(blend) = self.bus_mix.get_mut(bus) else {
            return Err(AudioError::UnknownBus(bus.into()));
        };
        if !db.is_finite() {
            return Err(AudioError::Invalid(format!(
                "bus `{bus}` volume must be a finite dB value, got {db}"
            )));
        }
        self.user_base_db.insert(bus.to_string(), db);
        blend.target_db = self.snapshots.target_volume_db(bus, db);
        blend.rate_db_per_sec = rate_for(blend.current_db, blend.target_db, fade.as_secs_f32());
        Ok(())
    }

    /// The bus's current volume target (dB).
    pub fn bus_volume(&self, bus: &str) -> Result<f32, AudioError> {
        self.bus_mix
            .get(bus)
            .map(|b| b.target_db)
            .ok_or_else(|| AudioError::UnknownBus(bus.into()))
    }

    /// Mutes or unmutes a bus.
    pub fn set_bus_mute(&mut self, bus: &str, mute: bool) -> Result<(), AudioError> {
        let Some(blend) = self.bus_mix.get_mut(bus) else {
            return Err(AudioError::UnknownBus(bus.into()));
        };
        blend.muted = mute;
        Ok(())
    }

    /// Whether a bus is muted (`None` when the layout does not define it).
    pub fn bus_muted(&self, bus: &str) -> Option<bool> {
        self.bus_mix.get(bus).map(|b| b.muted)
    }

    /// Applies the user's settings-screen volumes and mutes (spec § 1: chosen buses as
    /// sliders), blended in over `fade`. Buses the layout does not define are skipped; this
    /// composes with snapshots (it is the snapshot-independent base).
    pub fn apply_user_settings(&mut self, settings: &crate::settings::AudioUserSettings, fade: Duration) {
        for (bus, db) in &settings.bus_volumes {
            if let Err(error) = self.set_bus_volume(bus, *db, fade) {
                tracing::warn!(target: "regia_audio", "user settings for bus `{bus}` skipped: {error}");
            }
        }
        // Mute state follows the list exactly: buses not in `muted` are unmuted.
        let names: Vec<String> = self.bus_mix.keys().cloned().collect();
        for bus in names {
            let _ = self.set_bus_mute(&bus, settings.muted.contains(&bus));
        }
    }

    /// Registers a snapshot.
    pub fn register_snapshot(&mut self, snapshot: crate::snapshot::AudioSnapshot) {
        self.snapshots.register(snapshot);
    }

    /// Pushes a snapshot, blending bus volumes over `fade`
    /// (`audio.push_snapshot("Underwater", 0.5)`).
    pub fn push_snapshot(&mut self, name: &str, fade: Duration) -> Result<(), AudioError> {
        self.snapshots.push(name)?;
        self.retarget_blends(fade);
        self.retarget_filters(fade);
        self.retarget_sends(fade);
        Ok(())
    }

    /// Pops the top snapshot, blending back over `fade`.
    pub fn pop_snapshot(&mut self, fade: Duration) -> Result<(), AudioError> {
        self.snapshots.pop()?;
        self.retarget_blends(fade);
        self.retarget_filters(fade);
        self.retarget_sends(fade);
        Ok(())
    }

    /// Points every bus filter at the active snapshot's cutoff, or at the layout's authored
    /// cutoff when the snapshot does not name the bus.
    fn retarget_filters(&mut self, fade: Duration) {
        let names: Vec<String> = self.bus_mix.keys().cloned().collect();
        for bus in names {
            match self.snapshots.target_filter_cutoff(&bus) {
                Some(cutoff) => {
                    self.buses.set_filter_cutoffs(&bus, cutoff, fade);
                }
                None => {
                    self.buses.restore_filter_cutoffs(&bus, fade);
                }
            }
        }
    }

    /// The active snapshot's name.
    pub fn active_snapshot(&self) -> Option<&str> {
        self.snapshots.active()
    }

    fn retarget_blends(&mut self, fade: Duration) {
        let fade_secs = fade.as_secs_f32();
        let names: Vec<String> = self.bus_mix.keys().cloned().collect();
        for bus in names {
            let base = self.user_base_db.get(&bus).copied().unwrap_or(0.0);
            let Some(blend) = self.bus_mix.get_mut(&bus) else {
                continue;
            };
            blend.target_db = self.snapshots.target_volume_db(&bus, base);
            blend.rate_db_per_sec = rate_for(blend.current_db, blend.target_db, fade_secs);
        }
    }

    // --------------------------------------------------------------- cues

    /// Registers a cue for [`Audio::play_cue`].
    pub fn register_cue(&mut self, cue: SoundCue) {
        self.cue_runtime.insert(
            Arc::from(cue.name.as_str()),
            CueRuntime {
                selector: CueSelector::new(&cue),
                cooldown_until_secs: 0.0,
            },
        );
        self.cues.insert(Arc::from(cue.name.as_str()), cue);
    }

    /// Plays a registered cue: cooldown, variation limits and steal policy apply. Returns the
    /// started voice, or `None` when the cue refused to play (cooldown, limit with
    /// [`StealPolicy::None`], unknown name is an error).
    pub fn play_cue(&mut self, name: &str) -> Result<Option<SoundInstanceId>, AudioError> {
        self.play_cue_inner(name, None)
    }

    /// Like [`Audio::play_cue`], but the pick and the pitch/volume variation are drawn from a
    /// generator seeded with `seed` (and the cue name) instead of the shared audio generator, so
    /// the same seed with the same cue history replays the same variation. Derive the seed from
    /// the gameplay event with [`crate::cue_event_seed`]; never from a clock.
    pub fn play_cue_seeded(&mut self, name: &str, seed: u64) -> Result<Option<SoundInstanceId>, AudioError> {
        self.play_cue_inner(name, Some(seed))
    }

    /// [`Audio::play_cue_seeded`] with the seed derived from an event id and sim tick.
    pub fn play_cue_event(
        &mut self,
        name: &str,
        event_id: u64,
        tick: u64,
    ) -> Result<Option<SoundInstanceId>, AudioError> {
        self.play_cue_inner(name, Some(crate::cue::cue_event_seed(event_id, tick)))
    }

    fn play_cue_inner(
        &mut self,
        name: &str,
        seed: Option<u64>,
    ) -> Result<Option<SoundInstanceId>, AudioError> {
        self.play_cue_placed(name, seed, CuePlace::Flat)
    }

    /// Plays a registered cue as a one-shot at a world position with emitter spatial parameters
    /// (distance gain, panning, cone) through the level's propagation medium — spec 16 § 3,
    /// `play_at(cue, …)`. Cooldown, variation limits and steal policy apply as in
    /// [`Audio::play_cue`] (a steal happens only once this play started), and so does the cue's
    /// authored low-pass. A play that takes the airborne and the ground path counts once toward
    /// the cue's limit. Returns the started voice, or `None` when the cue refused (cooldown,
    /// limit with [`StealPolicy::None`]) or nothing audible reaches the listener (out of range,
    /// airless); a refused, failed or inaudible play does not consume the cue's pick.
    pub fn play_cue_at(
        &mut self,
        name: &str,
        position: Vec3,
        emitter: &AudioEmitter,
    ) -> Result<Option<SoundInstanceId>, AudioError> {
        self.play_cue_placed(
            name,
            None,
            CuePlace::At {
                position,
                emitter: emitter.clone(),
            },
        )
    }

    /// [`Audio::play_cue_at`] with a deterministic seed (see [`Audio::play_cue_seeded`]).
    pub fn play_cue_seeded_at(
        &mut self,
        name: &str,
        seed: u64,
        position: Vec3,
        emitter: &AudioEmitter,
    ) -> Result<Option<SoundInstanceId>, AudioError> {
        self.play_cue_placed(
            name,
            Some(seed),
            CuePlace::At {
                position,
                emitter: emitter.clone(),
            },
        )
    }

    /// [`Audio::play_cue_seeded_at`] with the seed derived from an event id and sim tick.
    pub fn play_cue_event_at(
        &mut self,
        name: &str,
        event_id: u64,
        tick: u64,
        position: Vec3,
        emitter: &AudioEmitter,
    ) -> Result<Option<SoundInstanceId>, AudioError> {
        self.play_cue_placed(
            name,
            Some(crate::cue::cue_event_seed(event_id, tick)),
            CuePlace::At {
                position,
                emitter: emitter.clone(),
            },
        )
    }

    /// Plays a registered cue attached to an entity's emitter: the pick follows the entity every
    /// frame — spec 16 § 3, `play_on(entity, cue)` — spatialised by `emitter`. The cue's
    /// variation (pitch/volume), bus, limits and authored low-pass apply; the low-pass track
    /// sits under the emitter's own track, so the emitter's occlusion filter still applies on
    /// top (ADR-5977). Returns the started voice, or `None` when the cue refused.
    pub fn play_cue_on(
        &mut self,
        name: &str,
        entity: Entity,
        emitter: &AudioEmitter,
    ) -> Result<Option<SoundInstanceId>, AudioError> {
        self.play_cue_placed(
            name,
            None,
            CuePlace::On {
                entity,
                emitter: emitter.clone(),
            },
        )
    }

    /// [`Audio::play_cue_on`] with a deterministic seed (see [`Audio::play_cue_seeded`]).
    pub fn play_cue_seeded_on(
        &mut self,
        name: &str,
        seed: u64,
        entity: Entity,
        emitter: &AudioEmitter,
    ) -> Result<Option<SoundInstanceId>, AudioError> {
        self.play_cue_placed(
            name,
            Some(seed),
            CuePlace::On {
                entity,
                emitter: emitter.clone(),
            },
        )
    }

    /// [`Audio::play_cue_seeded_on`] with the seed derived from an event id and sim tick.
    pub fn play_cue_event_on(
        &mut self,
        name: &str,
        event_id: u64,
        tick: u64,
        entity: Entity,
        emitter: &AudioEmitter,
    ) -> Result<Option<SoundInstanceId>, AudioError> {
        self.play_cue_placed(
            name,
            Some(crate::cue::cue_event_seed(event_id, tick)),
            CuePlace::On {
                entity,
                emitter: emitter.clone(),
            },
        )
    }

    /// Shared cue-play path: pick a variation (cooldown, limit and steal policy apply), play it
    /// where `place` directs, then commit the cooldown/caption. Returns the started voice, or
    /// `None` when the cue refused or (for a positional play) nothing is audible.
    fn play_cue_placed(
        &mut self,
        name: &str,
        seed: Option<u64>,
        place: CuePlace,
    ) -> Result<Option<SoundInstanceId>, AudioError> {
        let Some(pick) = self.prepare_cue(name, seed)? else {
            return Ok(None);
        };
        let duration_secs = pick.asset.duration_secs();
        let played = self.play_cue_pick(&pick, place);
        match played {
            Ok(Some(id)) => {
                // Steal only now: the new play started and is audible. A refused, failed or
                // out-of-range play leaves the voice it would have replaced alone.
                if let Some(victim) = pick.steal_victim {
                    self.stop_cue_play(victim, Duration::from_millis(10));
                }
                self.commit_cue_play(&pick, id, name, duration_secs);
                Ok(Some(id))
            }
            // The pick did not lead to a voice (the play failed, or a positional play reached
            // nothing audible): restore the selector so the entry is not consumed (sequential
            // cues would otherwise skip it forever; integ-v048c's rollback, on every place).
            Ok(None) | Err(_) => {
                if let Some(runtime) = self.cue_runtime.get_mut(&pick.key) {
                    runtime.selector = pick.selector_before.clone();
                }
                played
            }
        }
    }

    /// Plays a prepared pick where `place` directs (no bookkeeping: see `play_cue_placed`).
    fn play_cue_pick(
        &mut self,
        pick: &CuePick,
        place: CuePlace,
    ) -> Result<Option<SoundInstanceId>, AudioError> {
        Ok(match place {
            CuePlace::Flat => {
                let mut params = pick.params.clone();
                let bus = resolved_params_bus(&mut params, Some(&pick.asset));
                let (instance, filter_track) = match pick.filter_cutoff_hz {
                    None => (self.play_on_bus(&bus, &pick.asset, &params)?, None),
                    Some(cutoff) => self.play_filtered(&bus, &pick.asset, &params, cutoff)?,
                };
                let id = self.register_voice(
                    pick.asset.clone(),
                    params,
                    VoiceKind::OneShot,
                    Some(pick.key.clone()),
                    1.0,
                    instance,
                    None,
                );
                if let Some(entry) = self.voices.get_mut(id) {
                    entry.filter_track = filter_track;
                }
                Some(id)
            }
            CuePlace::At { position, emitter } => {
                let request = PlayRequest {
                    asset: pick.asset.clone(),
                    params: pick.params.clone(),
                    position: Some(position),
                    kind: VoiceKind::OneShot,
                    cue: Some(pick.key.clone()),
                    emitter: Some(emitter),
                    propagation: None,
                    // The cue's authored low-pass applies to a positional play too (ADR-5926).
                    filter_cutoff_hz: pick.filter_cutoff_hz,
                };
                // Positional plays travel through the medium (airborne and ground paths).
                self.start_request(request)?
            }
            CuePlace::On { entity, emitter } => {
                // The cue's authored low-pass applies to an attached play too, on a sub-track of
                // the emitter's track so occlusion still stacks on it (ADR-5977).
                let id = self.start_emitter_voice(
                    entity,
                    pick.asset.clone(),
                    &emitter,
                    pick.params.clone(),
                    pick.filter_cutoff_hz,
                )?;
                if let Some(entry) = self.voices.get_mut(id) {
                    entry.cue = Some(pick.key.clone());
                }
                Some(id)
            }
        })
    }

    /// Live plays of cue `name`: its voices with a live handle, where a positional play's second
    /// path voice counts only while its play's first voice is not live (one play, one count,
    /// whether it took the airborne path, the ground path or both).
    fn cue_plays_active(&self, name: &str) -> usize {
        self.voices
            .iter()
            .filter(|entry| entry.instance.is_some() && entry.cue.as_deref() == Some(name))
            .filter(|entry| {
                !self
                    .path_primary(entry.id)
                    .and_then(|primary| self.voices.get(primary))
                    .is_some_and(|primary| primary.instance.is_some())
            })
            .count()
    }

    /// Stops the cue play `victim` belongs to: the voice and the other path voice of the same
    /// positional play, if any.
    fn stop_cue_play(&mut self, victim: SoundInstanceId, fade: Duration) {
        let primary = self.path_primary(victim).unwrap_or(victim);
        let companion = self
            .voices
            .iter()
            .find(|entry| entry.id != primary && self.path_primary(entry.id) == Some(primary))
            .map(|entry| entry.id);
        self.stop_voice(primary, fade);
        if let Some(companion) = companion {
            self.stop_voice(companion, fade);
        }
    }

    /// Everything up to the pick: unknown name, empty cue, cooldown, max-concurrent (the steal
    /// victim is chosen here and stopped by the caller once the play started). Returns `None`
    /// when the play is refused, the pick otherwise.
    fn prepare_cue(&mut self, name: &str, seed: Option<u64>) -> Result<Option<CuePick>, AudioError> {
        let key: Arc<str> = Arc::from(name);
        if !self.cues.contains_key(&key) {
            return Err(AudioError::UnknownEntry(name.into()));
        }
        // A cue built in Rust may have no entries; there is nothing to pick.
        if self.cues[&key].entries.is_empty() {
            return Ok(None);
        }
        if self.now_secs < self.cue_runtime.get(&key).map_or(0.0, |r| r.cooldown_until_secs) {
            return Ok(None);
        }
        let (max_concurrent, steal, cooldown) = {
            let cue = &self.cues[&key];
            (cue.max_concurrent, cue.steal, cue.cooldown)
        };
        let mut steal_victim = None;
        if let Some(max) = max_concurrent
            && self.cue_plays_active(name) >= max
        {
            steal_victim = match steal {
                StealPolicy::None => return Ok(None),
                StealPolicy::Oldest => self.voices.oldest_of_cue(name),
                StealPolicy::Quietest => self.voices.quietest_of_cue(name),
            };
        }

        let mut seeded = seed.map(|seed| {
            // One stream per cue name so two cues fired by the same event vary independently.
            let stream = name.bytes().fold(0xCBF2_9CE4_8422_2325_u64, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3)
            });
            regia_core::math::Rng::with_stream(seed, stream)
        });
        let (asset, params, selector_before) = {
            let runtime = &mut self.cue_runtime.get_mut(&key).expect("checked above");
            // The pick commits the selector (no-repeat window, shuffle bag, sequential cursor);
            // a play that fails afterwards must not consume it, so it can be rolled back.
            let selector_before = runtime.selector.clone();
            let cue = &self.cues[&key];
            let rng = seeded.as_mut().unwrap_or(&mut self.rng);
            let pick = runtime.selector.pick(rng);
            let mut params = PlaybackParams {
                bus: cue.bus.clone(),
                volume_db: cue.volume_db,
                ..PlaybackParams::default()
            };
            runtime.selector.vary(&mut params, rng);
            let asset = cue.entries[pick].asset.clone();
            (asset, params, selector_before)
        };
        let filter_cutoff_hz = self.cues[&key].effective_filter_cutoff_hz();
        let caption = self.cues[&key].caption.clone();
        Ok(Some(CuePick {
            key,
            asset,
            params,
            filter_cutoff_hz,
            caption,
            cooldown,
            selector_before,
            steal_victim,
        }))
    }

    /// Finishes a started cue play: commits the cooldown only now (a rejected or inaudible play
    /// must not silence the cue for the window), counts the play and queues its caption once
    /// (ADR-1697).
    fn commit_cue_play(&mut self, pick: &CuePick, id: SoundInstanceId, name: &str, duration_secs: f64) {
        if !pick.cooldown.is_zero()
            && let Some(runtime) = self.cue_runtime.get_mut(&pick.key)
        {
            runtime.cooldown_until_secs = self.now_secs + pick.cooldown.as_secs_f64();
        }
        self.played_count += 1;
        if let Some(caption) = pick.caption.clone() {
            self.cue_captions.push(crate::cue::CueCaptionPlay {
                cue: name.to_string(),
                caption,
                instance: id,
                duration_secs,
            });
        }
    }

    /// Takes the cue plays that carry a caption since the last call, oldest first. A game (or
    /// `regia_framework::conversation`) turns each into a caption message.
    pub fn take_cue_captions(&mut self) -> Vec<crate::cue::CueCaptionPlay> {
        std::mem::take(&mut self.cue_captions)
    }

    /// Stops a voice (fade) and marks it for removal.
    pub fn stop_voice(&mut self, id: SoundInstanceId, fade: Duration) {
        if let Some(entry) = self.voices.get_mut(id) {
            entry.stopping = true;
            if let Some(mut instance) = entry.instance.take() {
                instance.stop(fade);
            }
        }
    }

    /// Pauses a voice, fading to silence over `fade` (spec § 2: the `SoundInstance` handle
    /// API, addressed by the id returned from the play/cue calls). The pause survives the
    /// internal replays that rebuild an emitter's track and a virtualise/revive round trip:
    /// they re-apply it, so the voice is never heard again until [`Audio::resume_voice`].
    pub fn pause_voice(&mut self, id: SoundInstanceId, fade: Duration) {
        if let Some(entry) = self.voices.get_mut(id) {
            entry.paused = true;
            if let Some(instance) = entry.instance.as_mut() {
                instance.pause(fade);
            }
        }
    }

    /// Resumes a paused voice, fading in over `fade`.
    pub fn resume_voice(&mut self, id: SoundInstanceId, fade: Duration) {
        if let Some(entry) = self.voices.get_mut(id) {
            entry.paused = false;
            if let Some(instance) = entry.instance.as_mut() {
                instance.resume(fade);
            }
        }
    }

    /// Retimes a voice's volume in dB (e.g. a live tween on a cue pick).
    pub fn set_voice_volume_db(&mut self, id: SoundInstanceId, db: f32, tween_secs: f32) {
        let db = self.retain_persistent_volume(id, db);
        if let Some(entry) = self.voices.get_mut(id)
            && let Some(instance) = entry.instance.as_mut()
        {
            instance.set_volume_db(db, crate::sound::tween_from_secs(tween_secs));
        }
    }

    /// Retimes a voice's playback rate (1 = original).
    pub fn set_voice_pitch(&mut self, id: SoundInstanceId, ratio: f32, tween_secs: f32) {
        let ratio = self.retain_persistent_pitch(id, ratio);
        if let Some(entry) = self.voices.get_mut(id)
            && let Some(instance) = entry.instance.as_mut()
        {
            instance.set_pitch(ratio, crate::sound::tween_from_secs(tween_secs));
        }
    }

    /// Retimes a voice's stereo pan (−1 hard left .. 1 hard right).
    pub fn set_voice_pan(&mut self, id: SoundInstanceId, pan: f32, tween_secs: f32) {
        let pan = self.retain_persistent_pan(id, pan);
        if let Some(entry) = self.voices.get_mut(id)
            && let Some(instance) = entry.instance.as_mut()
        {
            instance.set_pan(pan, crate::sound::tween_from_secs(tween_secs));
        }
    }

    /// Jumps a voice to `secs` into the sound.
    pub fn seek_voice(&mut self, id: SoundInstanceId, secs: f64) {
        let secs = self.retain_persistent_seek(id, secs);
        if let Some(entry) = self.voices.get_mut(id)
            && let Some(instance) = entry.instance.as_mut()
        {
            instance.seek(secs);
        }
    }

    /// The voice's playback state (`None` when the id is dead or the voice is virtualised).
    pub fn voice_state(&self, id: SoundInstanceId) -> Option<kira::sound::PlaybackState> {
        self.voices
            .get(id)
            .and_then(|e| e.instance.as_ref())
            .map(crate::instance::SoundInstance::state)
    }

    /// Whether the voice should keep a caption up.
    ///
    /// True while a playback state is present and is not stopped, so a paused voice still counts.
    /// A missing id and a virtualised voice (no playback state) do not.
    #[must_use]
    pub fn voice_playing(&self, id: SoundInstanceId) -> bool {
        self.voice_state(id)
            .is_some_and(|state| state != kira::sound::PlaybackState::Stopped)
    }

    /// `true` while the voice is live, real or virtual (a virtualised voice has no
    /// [`Audio::voice_state`] but is still tracked and comes back when it is heard again).
    pub fn voice_alive(&self, id: SoundInstanceId) -> bool {
        self.voices.get(id).is_some()
    }

    /// The voice's playback position in seconds (`None` when dead or virtualised).
    pub fn voice_position(&self, id: SoundInstanceId) -> Option<f64> {
        self.voices
            .get(id)
            .and_then(|e| e.instance.as_ref())
            .map(crate::instance::SoundInstance::position)
    }

    // --------------------------------------------------------------- patches

    /// Spawns a parametric patch; returns its id. Output recovery restarts its layers with
    /// the same id and parameter state. While the mixer is down (suspended, and no render-out
    /// take is running), playback waits for resume. A render-out take still builds layers,
    /// including when the device output is suspended.
    ///
    /// # Errors
    /// Returns [`AudioError::UnknownBus`] when the patch bus is not in the layout, including
    /// while the mixer is down. Other layer errors match a live spawn.
    pub fn spawn_patch(&mut self, patch: AudioPatch) -> Result<u64, AudioError> {
        let mut params = HashMap::new();
        for def in patch.def.parameters.clone() {
            params.insert(def.name.clone(), SmoothedParam::new(def));
        }
        // Stored even when layers wait for resume, so one-shot ticks and recovery
        // use the same bus `build_patch_layers` would.
        let bus = patch.def.bus.clone().unwrap_or_else(|| "Sfx".to_string());
        let layers = if self.mixer_down() {
            if !self.buses.contains(&bus) {
                return Err(AudioError::UnknownBus(bus));
            }
            Vec::new()
        } else {
            self.build_patch_layers(&patch, &params)?
        };
        let id = self.next_patch_id;
        self.next_patch_id += 1;
        self.patches.push(PatchPlayer {
            id,
            patch,
            params,
            layers,
            bus,
            clock_secs: 0.0,
        });
        Ok(id)
    }

    /// Builds new mixer handles from a patch's retained parameter state.
    fn build_patch_layers(
        &mut self,
        patch: &AudioPatch,
        params: &HashMap<String, SmoothedParam>,
    ) -> Result<Vec<PatchLayerRuntime>, AudioError> {
        let bus = patch.def.bus.clone().unwrap_or_else(|| "Sfx".to_string());
        let mut layers = Vec::new();
        for layer in &patch.def.layers {
            let kind = match layer.kind() {
                Ok(kind) => kind,
                Err(error) => {
                    stop_patch_loops(&mut [], &mut layers);
                    return Err(error);
                }
            };
            let initial = layer_outputs(layer, params);
            let looping = PlaybackParams {
                bus: Some(bus.clone()),
                loop_region: Some(LoopRegion {
                    start_secs: 0.0,
                    end_secs: None,
                }),
                // Use the retained parameter state from the first sample, including recovery.
                volume_db: initial.volume_db,
                pitch: initial.pitch,
                ..PlaybackParams::default()
            };
            // A filtered layer plays on its own child track under the bus, carrying the low-pass
            // `filter_curves` drive (the emitter pattern): bus volume, ducking and snapshots
            // still apply, and the cutoff shapes what is heard.
            let (mut track, filter) = if layer.filter_curves.is_empty() {
                (None, None)
            } else {
                // Persist until the loops finish, so a despawn's fade-out is heard in full.
                let mut builder = TrackBuilder::new().persist_until_sounds_finish(true);
                let filter = builder.add_effect(
                    FilterBuilder::new()
                        .mode(FilterMode::LowPass)
                        .cutoff(initial.cutoff_hz.unwrap_or(20_000.0))
                        .resonance(0.0),
                );
                match self.buses.add_child_track(&bus, builder) {
                    Some(track) => (Some(track), Some(filter)),
                    None => {
                        if !self.buses.contains(&bus) {
                            return Err(AudioError::UnknownBus(bus.clone()));
                        }
                        tracing::warn!(
                            target: "regia_audio",
                            "patch layer `{}`: no filter track, playing unfiltered", layer.name
                        );
                        (None, None)
                    }
                }
            };
            let mut loops = Vec::new();
            let mut shot = None;
            let mut granular = None;
            if kind == PatchLayerKind::OneShot {
                shot = layer
                    .one_shot
                    .as_ref()
                    .map(|def| patch_layers::OneShotRuntime::new(def, params));
            } else if kind == PatchLayerKind::Granular {
                if let (Some(def), Some(asset)) = (&layer.granular, patch.layer_sounds.get(&layer.name)) {
                    let started = self.start_granular(asset, def, &initial, params, track.as_mut(), &bus);
                    match started {
                        Ok(handle) => granular = Some(handle),
                        Err(error) => {
                            stop_patch_loops(&mut loops, &mut layers);
                            return Err(error);
                        }
                    }
                }
            } else if layer.keys.is_empty() {
                let mut start =
                    |asset: &SoundAsset, params: &PlaybackParams| -> Result<SoundInstance, AudioError> {
                        match track.as_mut() {
                            Some(track) => play_data_on_track(track, asset.play_data(params)?),
                            None => play_data_on(&mut self.buses, &bus, asset.play_data(params)?),
                        }
                    };
                if let Some(asset) = patch.layer_sounds.get(&layer.name) {
                    let instance = match start(asset, &looping) {
                        Ok(instance) => instance,
                        Err(error) => {
                            stop_patch_loops(&mut loops, &mut layers);
                            return Err(error);
                        }
                    };
                    loops.push(PatchLoop {
                        instance,
                        volume_offset_db: asset.play_volume_offset_db(),
                        pitch_multiplier: asset.default_pitch(),
                        applied_db: looping.volume_db,
                        applied_pitch: looping.pitch,
                    });
                }
            } else {
                let mut start =
                    |asset: &SoundAsset, params: &PlaybackParams| -> Result<SoundInstance, AudioError> {
                        match track.as_mut() {
                            Some(track) => play_data_on_track(track, asset.play_data(params)?),
                            None => play_data_on(&mut self.buses, &bus, asset.play_data(params)?),
                        }
                    };
                let mut weights = Vec::new();
                let mut order = Vec::new();
                if let Some(param) = layer.parameter.as_ref().and_then(|name| params.get(name)) {
                    crossfade_weights_into(&layer.keys, param.value, &mut order, &mut weights);
                }
                for (index, key) in layer.keys.iter().enumerate() {
                    if let Some(asset) = patch.key_sounds.get(&(layer.name.clone(), index)) {
                        let mut key_params = looping.clone();
                        let weight = weights.iter().find(|(i, _)| *i == index).map_or(0.0, |(_, w)| *w);
                        key_params.volume_db = if weight <= 1e-4 {
                            -100.0
                        } else {
                            initial.volume_db + key.gain_db + gain_to_db(weight)
                        };
                        key_params.pitch = initial.pitch * key.pitch;
                        let instance = match start(asset, &key_params) {
                            Ok(instance) => instance,
                            Err(error) => {
                                stop_patch_loops(&mut loops, &mut layers);
                                return Err(error);
                            }
                        };
                        loops.push(PatchLoop {
                            instance,
                            volume_offset_db: asset.play_volume_offset_db(),
                            pitch_multiplier: asset.default_pitch(),
                            applied_db: key_params.volume_db,
                            applied_pitch: key_params.pitch,
                        });
                    }
                }
            }
            // The track was built at the initial cutoff: the first tick only retargets on change.
            let applied_cutoff = initial.cutoff_hz.filter(|_| filter.is_some());
            layers.push(PatchLayerRuntime {
                kind,
                loops,
                track,
                filter,
                applied: LayerOutputs {
                    cutoff_hz: applied_cutoff,
                    ..LayerOutputs::default()
                },
                shot,
                granular,
            });
        }
        Ok(layers)
    }

    /// Sets a patch parameter (slewed over the following ticks; no zipper noise).
    pub fn patch_set_param(&mut self, id: u64, name: &str, value: f32) -> Result<(), AudioError> {
        let Some(player) = self.patches.iter_mut().find(|p| p.id == id) else {
            return Err(AudioError::UnknownEntry(format!("patch {id}")));
        };
        match player.params.get_mut(name) {
            Some(param) => param.set_target(value),
            None => return Err(AudioError::Invalid(format!("patch has no parameter `{name}`"))),
        }
        Ok(())
    }

    /// Stops and removes a patch.
    pub fn despawn_patch(&mut self, id: u64) {
        let mut doomed: Vec<&mut PatchPlayer> =
            self.patches.iter_mut().filter(|player| player.id == id).collect();
        for player in &mut doomed {
            for layer in &mut player.layers {
                for voice in &mut layer.loops {
                    voice.instance.stop(Duration::from_millis(50));
                }
                if let Some(granular) = &layer.granular {
                    granular.stop();
                }
            }
        }
        self.patches.retain(|player| player.id != id);
    }

    fn tick_patches(&mut self, dt: f64) {
        if self.mixer_down() {
            return;
        }
        let micro = Duration::from_millis((dt * 2_000.0).clamp(1.0, 50.0) as u64);
        let Audio {
            patches,
            patch_weights,
            patch_order,
            buses,
            ..
        } = self;
        for player in patches.iter_mut() {
            for param in player.params.values_mut() {
                param.tick(dt);
            }
            if dt.is_finite() && dt > 0.0 {
                player.clock_secs += dt;
            }
            let PatchPlayer {
                patch,
                params,
                layers,
                bus,
                clock_secs,
                ..
            } = player;
            for (layer_def, layer) in patch.def.layers.iter().zip(layers.iter_mut()) {
                let out = layer_outputs(layer_def, params);
                match layer.kind {
                    PatchLayerKind::OneShot => {
                        let mut ctx = patch_layers::ShotContext {
                            buses: &mut *buses,
                            bus: bus.as_str(),
                            sounds: &patch.key_sounds,
                            clock: *clock_secs,
                        };
                        patch_layers::tick_one_shot(&mut ctx, layer_def, layer, params, &out);
                        patch_layers::retarget_filter(layer, &out, micro);
                        continue;
                    }
                    PatchLayerKind::Granular => {
                        patch_layers::tick_granular(layer_def, layer, params, &out);
                        patch_layers::retarget_filter(layer, &out, micro);
                        continue;
                    }
                    PatchLayerKind::Loop | PatchLayerKind::Keys => {}
                }
                patch_weights.clear();
                if let Some(param_name) = &layer_def.parameter
                    && let Some(param) = params.get(param_name)
                {
                    crossfade_weights_into(&layer_def.keys, param.value, patch_order, patch_weights);
                }
                if layer_def.keys.is_empty() {
                    if let Some(voice) = layer.loops.first_mut() {
                        if (voice.applied_db - out.volume_db).abs() > 0.05 {
                            voice
                                .instance
                                .set_volume_db(out.volume_db + voice.volume_offset_db, micro);
                            voice.applied_db = out.volume_db;
                        }
                        if (voice.applied_pitch - out.pitch).abs() > 0.005 {
                            voice
                                .instance
                                .set_pitch(out.pitch * voice.pitch_multiplier, micro);
                            voice.applied_pitch = out.pitch;
                        }
                    }
                } else {
                    for (loop_index, voice) in layer.loops.iter_mut().enumerate() {
                        let weight = patch_weights
                            .iter()
                            .find(|(i, _)| *i == loop_index)
                            .map_or(0.0f32, |(_, w)| *w);
                        let key_db = layer_def.keys.get(loop_index).map_or(0.0, |k| k.gain_db);
                        let db = if weight <= 1e-4 {
                            -100.0
                        } else {
                            out.volume_db + key_db + gain_to_db(weight)
                        };
                        if (voice.applied_db - db).abs() > 0.05 {
                            voice.instance.set_volume_db(db + voice.volume_offset_db, micro);
                            voice.applied_db = db;
                        }
                        let key_pitch = layer_def.keys.get(loop_index).map_or(1.0, |k| k.pitch);
                        let pitch = out.pitch * key_pitch;
                        if (voice.applied_pitch - pitch).abs() > 0.005 {
                            voice.instance.set_pitch(pitch * voice.pitch_multiplier, micro);
                            voice.applied_pitch = pitch;
                        }
                    }
                }
                if let Some(cutoff) = out.cutoff_hz
                    && let Some(filter) = layer.filter.as_mut()
                    && layer.applied.cutoff_hz != Some(cutoff)
                {
                    filter.set_cutoff(cutoff, tween(micro));
                    layer.applied.cutoff_hz = Some(cutoff);
                }
            }
        }
    }

    // --------------------------------------------------------------- announcer

    /// Registers a bank of interchangeable announcer lines under `id`.
    pub fn register_announcer_bank(&mut self, id: impl Into<String>, bank: AnnouncerBank) {
        let id = id.into();
        self.announcer.set_bank_priority(&id, bank.priority);
        self.announcer_banks.insert(id, bank);
    }

    /// Announces a line: rotation picks the variation, pre-emption and the low-priority queue
    /// follow the spec rules, and the game buses duck while the line plays.
    pub fn announce(&mut self, id: &str) -> AnnounceOutcome {
        let outcome = self.announcer.decide(id);
        match outcome {
            AnnounceOutcome::Started | AnnounceOutcome::Preempted => {
                if outcome == AnnounceOutcome::Preempted
                    && let Some(old) = self.announcer_voice.take()
                {
                    self.stop_voice(old, Duration::from_millis(100));
                }
                self.start_announcer_line(id);
            }
            AnnounceOutcome::Queued => self.announcer.note_queued(id),
            AnnounceOutcome::Rejected => {}
        }
        outcome
    }

    /// The queued low-priority line, if any.
    pub fn announcer_queued(&self) -> Option<&str> {
        self.announcer.queued()
    }

    fn start_announcer_line(&mut self, id: &str) {
        let pick = self
            .announcer_banks
            .get_mut(id)
            .and_then(|bank| bank.pick(&mut self.rng));
        let asset = pick.and_then(|index| {
            self.announcer_banks
                .get(id)
                .and_then(|bank| bank.sounds.get(index).cloned())
        });
        let Some(asset) = asset else {
            // A pre-empted line is already stopped: release the announcer state (and with it the
            // duck) instead of waiting for a voice that will never exist.
            self.finish_announcer_line();
            return;
        };
        self.announcer.note_started(id);
        let params = PlaybackParams {
            bus: Some("Voice".into()),
            ..PlaybackParams::default()
        };
        match self.play_on_bus("Voice", &asset, &params) {
            Ok(instance) => {
                let id = self.register_voice(asset, params, VoiceKind::Announcer, None, 1.0, instance, None);
                self.announcer_voice = Some(id);
            }
            Err(error) => {
                tracing::warn!(target: "regia_audio", "announcer line `{id}` failed to start: {error}");
                self.finish_announcer_line();
            }
        }
    }

    /// Ends the announcer's current line (when one is recorded) and starts the queued
    /// low-priority line, if any. A no-op while the announcer is idle.
    fn finish_announcer_line(&mut self) {
        if !self.announcer.is_active() {
            return;
        }
        if let Some(queued) = self.announcer.note_finished() {
            self.start_announcer_line(&queued);
        }
    }

    // --------------------------------------------------------------- voice capture / loopback

    /// Starts voice capture. Android: the `RECORD_AUDIO` permission must already be granted by
    /// the platform layer (Regia Engine never prompts on its own).
    pub fn start_capture(&mut self, settings: crate::voice::VoiceCaptureSettings) -> Result<(), AudioError> {
        self.capture = Some(crate::voice::AudioCapture::start(&settings)?);
        Ok(())
    }

    /// Stops voice capture and releases the microphone.
    pub fn stop_capture(&mut self) {
        self.capture = None;
    }

    /// Drains captured frames (push-to-talk gate open) and, when loopback is running, plays them
    /// back through the [`crate::voice::VoiceStreamData`] sound (the spec's `VoiceStream` source
    /// type). Returns the frames drained (for the game
    /// to encode and send).
    pub fn tick_voice(&mut self, dt: f64) -> Vec<f32> {
        let Some(capture) = self.capture.as_mut() else {
            return Vec::new();
        };
        let frames = capture.drain_talking(dt);
        if let Some(handle) = self.voice_playback.as_ref()
            && !frames.is_empty()
        {
            // Timestamping here measures the pipeline's added latency: from drain (the earliest
            // the game could act on the frames) to first render. The capture callback → drain
            // delay is at most one frame and lives in the ring buffer.
            let chunk: Arc<[f32]> = frames.clone().into();
            handle.push(chunk, Some(Instant::now()));
        }
        frames
    }

    /// Starts (or restarts) the voice loopback stream on the Voice bus; `prebuffer_ms` is the
    /// jitter-buffer target.
    pub fn start_voice_loopback(&mut self, prebuffer_ms: u32) -> Result<(), AudioError> {
        let capture_rate = self
            .capture
            .as_ref()
            .map_or(48_000, crate::voice::AudioCapture::sample_rate);
        let data = crate::voice::VoiceStreamData::new(prebuffer_ms, capture_rate);
        let early = data.early_handle();
        let bus = "Voice".to_string();
        let track = self
            .buses
            .track(&bus)
            .ok_or_else(|| AudioError::UnknownBus(bus.clone()))?;
        track.play(data).map_err(crate::sound::play_error)?;
        self.voice_playback = Some(early);
        Ok(())
    }

    /// Pushes decoded voice frames into the loopback stream (the game's own transport decodes
    /// remote voice chat and hands the frames here). `captured_at` timestamps the chunk for the
    /// latency statistics.
    pub fn push_voice_frames(&mut self, frames: Arc<[f32]>, captured_at: Option<std::time::Instant>) {
        if let Some(handle) = self.voice_playback.as_ref() {
            handle.push(frames, captured_at);
        }
    }

    /// Stops the voice loopback stream.
    pub fn stop_voice_loopback(&mut self) {
        if let Some(handle) = self.voice_playback.take() {
            handle.stop();
        }
    }

    /// The voice loopback statistics, when running.
    pub fn voice_stats(&self) -> Option<crate::voice::VoiceStreamSnapshot> {
        self.voice_playback.as_ref().map(|h| h.stats())
    }

    /// Places the voice loopback stream in the stereo field (spec 16 § 3b): distance gain, pan
    /// and an optional low-pass cutoff, or [`crate::voice_chat::VoicePlacement::Center`] for the
    /// flat default. A game with an anchored entity does not call this — the `AudioSync` pass
    /// follows [`crate::voice::VoiceStreamAnchor`] each frame. No stream running: ignored.
    pub fn set_voice_stream_placement(&mut self, placement: crate::voice_chat::VoicePlacement) {
        if let Some(handle) = self.voice_playback.as_ref() {
            handle.set_placement(placement);
        }
    }

    /// The loopback stream's current placement, when a stream runs.
    #[must_use]
    pub fn voice_stream_placement(&self) -> Option<crate::voice_chat::VoicePlacement> {
        self.voice_playback.as_ref().map(|handle| handle.placement())
    }

    /// The capture level `(rms, peak)` in dBFS, when capturing.
    pub fn capture_level_db(&self) -> Option<(f32, f32)> {
        self.capture.as_ref().map(crate::voice::AudioCapture::level_db)
    }

    /// The capture device name actually opened (empty when not capturing).
    pub fn capture_device(&self) -> &str {
        self.capture
            .as_ref()
            .map_or("", crate::voice::AudioCapture::device)
    }

    /// The capture sample rate frames arrive at (48 000 when not capturing).
    pub fn capture_sample_rate(&self) -> u32 {
        self.capture
            .as_ref()
            .map_or(48_000, crate::voice::AudioCapture::sample_rate)
    }

    /// The capture session, if its stream is actually open. A suspended session (the microphone
    /// released) does not count: the four state accessors below read through this so they
    /// report "not capturing" for the duration of an output suspend.
    fn live_capture(&self) -> Option<&crate::voice::AudioCapture> {
        self.capture.as_ref().filter(|capture| capture.is_running())
    }

    /// True while a capture stream is open (a microphone is held). False while the output is
    /// suspended — the microphone is released for the duration — so a `!is_capturing()` restart
    /// gate reads correctly across an Android suspend.
    pub fn is_capturing(&self) -> bool {
        self.live_capture().is_some()
    }

    /// The channel count of the open capture stream (0 when not capturing, including while the
    /// output is suspended).
    pub fn capture_channels(&self) -> u16 {
        self.live_capture()
            .map_or(0, crate::voice::AudioCapture::channels)
    }

    /// Frames dropped by the capture ring since it started (0 when not capturing, including
    /// while the output is suspended; the count carries across restarts).
    pub fn capture_dropped_frames(&self) -> u64 {
        self.live_capture()
            .map_or(0, crate::voice::AudioCapture::dropped_frames)
    }

    /// True while the push-to-talk gate is open (false when not capturing, including while the
    /// output is suspended; the gate itself is kept across the suspend).
    pub fn is_talking(&self) -> bool {
        self.live_capture()
            .is_some_and(crate::voice::AudioCapture::is_talking)
    }

    /// True while the VAD hears speech, when capturing.
    pub fn voice_active(&self) -> bool {
        self.capture
            .as_ref()
            .is_some_and(crate::voice::AudioCapture::is_voice_active)
    }

    /// Sets the push-to-talk gate.
    pub fn set_talking(&mut self, talking: bool) {
        if let Some(capture) = self.capture.as_mut() {
            capture.set_talking(talking);
        }
    }

    // --------------------------------------------------------------- lifecycle / backend

    /// The device mixer is down: output is suspended and no render-out take is mixing.
    /// A take keeps its own offline mixer, so patch layers and persistent loops still play
    /// there when the device output is suspended.
    fn mixer_down(&self) -> bool {
        self.suspended && self.mixdown.is_none()
    }

    /// Tears the device output stream down (Android `Suspended`) and releases live mixer handles.
    /// Patches and [`Self::start_persistent_loop`] voices retain their ownership for recovery;
    /// other voices end. Ducking activity and real voice-budget slots are released at once.
    /// A render-out take that is already running keeps its patch layers and persistent-loop
    /// voices: the take mixes on its own offline mixer, which this call does not tear down.
    /// Calling it again while suspended is a no-op; counters count only real transitions.
    pub fn suspend_output(&mut self) {
        if self.suspended {
            return;
        }
        self.suspended = true;
        self.suspend_count += 1;
        regia_core::telemetry::add(regia_core::telemetry::Counter::AudioSuspends, 1);
        // A running take mixes on its own offline mixer. Dropping its layers here would
        // silence audio the take is still rendering. Device teardown does not stop that mixer.
        if self.mixdown.is_none() {
            self.suspend_persistent_voices();
        }
        self.backend.suspend();
        if let Some(capture) = self.capture.as_mut() {
            capture.stop();
        }
    }

    /// Re-opens the output stream (Android `Resumed`) and rebuilds the bus tracks with the
    /// current bus state (host volumes, mutes, snapshot blend and filter cutoffs). Restarts
    /// spawned patches at their retained parameter values and persistent host loops at their
    /// frozen cursors, with the same ids. Transient sounds on the old tracks are discarded.
    ///
    /// Idempotent: on a live output (never suspended, or already resumed) this does nothing and
    /// returns `true`, so a host resume and the engine's own resume in the same frame rebuild
    /// once. Returns `false` when the device or bus tree could not be re-opened (the next call
    /// retries), or while a mixdown defers recovery until it ends.
    pub fn resume_output(&mut self) -> bool {
        if !self.suspended {
            return true;
        }
        if self.mixdown.is_some() {
            // A render-out mixdown holds the bus tree on its own offline mixer; resuming now
            // would rebuild the buses there and drop the take's stem taps. Defer to
            // `end_mixdown`, which applies it on the restored output.
            self.resume_after_mixdown = true;
            return false;
        }
        #[cfg(test)]
        if self.failing_resumes > 0 {
            self.failing_resumes -= 1;
            return false;
        }
        if !self.backend.resume() {
            return false;
        }
        // Anything started while suspended played on the old tracks, which the rebuild drops.
        self.suspend_persistent_voices();
        let conv = self.convolution_context(self.output_sample_rate);
        match self.backend.build_buses(&self.bus_layout, &conv) {
            Ok(buses) => self.buses = buses,
            Err(error) => {
                tracing::error!(target: "regia_audio", "bus rebuild failed: {error}");
                return false;
            }
        }
        self.suspended = false;
        self.resume_count += 1;
        if let Some(capture) = self.capture.as_mut()
            && let Err(error) = capture.resume()
        {
            tracing::warn!(target: "regia_audio", "capture did not resume: {error}");
        }
        self.reapply_bus_state();
        // The emitter tracks died with the old mixer: autoplaying emitters restart on the
        // next sync, reverb zones re-apply.
        self.reset_emitters_after_rebuild();
        self.restart_persistent_voices();
        true
    }

    /// True between [`Audio::suspend_output`] and the [`Audio::resume_output`] that re-opened
    /// the output (also while a resume keeps failing to find a device).
    pub fn output_suspended(&self) -> bool {
        self.suspended
    }

    /// Ends every voice: stops the live handles and forgets every entry, real and virtual, and
    /// stops the voice loopback stream. kira only reports `Stopped` from its renderer, so a voice
    /// whose mixer or track is torn down would otherwise count as live (ducking activity, budget
    /// slot) forever; the loopback's `running` flag likewise only clears on an explicit stop.
    fn release_voices(&mut self) {
        self.persistent_loops.clear();
        for player in &mut self.patches {
            stop_patch_loops(&mut [], &mut player.layers);
        }
        self.patches.clear();
        self.stop_voice_loopback();
        self.music.output_lost();
        let released = self.voices.entries.len();
        for entry in &mut self.voices.entries {
            if let Some(mut instance) = entry.instance.take() {
                instance.stop(Duration::ZERO);
            }
        }
        self.voices.clear();
        if released > 0 {
            tracing::debug!(target: "regia_audio", released, "voices released with the output");
        }
    }

    /// Pushes the current bus state onto freshly built bus tracks (which start at the layout's
    /// defaults): the composed volume or mute of every bus and the active snapshot's filter
    /// cutoffs. Ducking re-applies on the next tick.
    fn reapply_bus_state(&mut self) {
        let names: Vec<String> = self.bus_mix.keys().cloned().collect();
        for bus in &names {
            let muted = self.muted_now(bus);
            let Some(blend) = self.bus_mix.get_mut(bus) else {
                continue;
            };
            let db = if muted { -100.0 } else { blend.current_db };
            self.buses.set_volume(bus, db, Duration::ZERO);
            blend.last_applied_db = db;
            if let Some(cutoff) = self.snapshots.target_filter_cutoff(bus) {
                self.buses.set_filter_cutoffs(bus, cutoff, Duration::ZERO);
            }
        }
    }

    /// Test fault: the next `count` resumes fail as if the device could not be opened (a fake
    /// output that comes back late, e.g. a Bluetooth headset at `Resumed`).
    #[cfg(test)]
    pub(crate) fn fail_next_resumes(&mut self, count: u32) {
        self.failing_resumes = count;
    }

    /// How many suspend/resume cycles this resource has seen.
    pub fn suspend_count(&self) -> u64 {
        self.suspend_count
    }

    /// How many of the [`Audio::suspend_count`] suspends the engine's output gate made because the
    /// frame loop was throttled (sustained slow frames, see `AudioThrottle`), not because every
    /// window was hidden.
    pub fn throttle_suspend_count(&self) -> u64 {
        self.throttle_suspend_count
    }

    /// Counts a slow-frame suspend (the output gate, after a real `suspend_output` transition).
    pub(crate) fn note_throttle_suspend(&mut self) {
        self.throttle_suspend_count += 1;
        regia_core::telemetry::add(regia_core::telemetry::Counter::AudioThrottleSuspends, 1);
    }

    /// How many times the output stream was (re)opened after a suspend (a resume on a live
    /// output does not count). `resume_count() == suspend_count()` while the output is live.
    pub fn resume_count(&self) -> u64 {
        self.resume_count
    }

    /// True when running on the offline backend (no output device).
    pub fn is_headless(&self) -> bool {
        self.backend.is_offline()
    }

    /// Drains output-stream diagnostics (device backends only): cpal stream errors such as device
    /// changes and disconnections. kira's cpal backend restarts the stream itself when the
    /// default device changes or dies, keeping the renderer — and with it all playing sounds —
    /// alive (spec § 4); the surfaced errors are for logging and tooling. Empty on the offline
    /// backend.
    pub fn take_output_errors(&mut self) -> Vec<String> {
        self.backend.take_output_errors()
    }

    /// The output's sample clock, the time base of what is being heard. `None` before the first
    /// rendered block, while suspended and during a render-out mixdown.
    pub fn sample_clock(&self) -> Option<crate::backend::SampleClock> {
        self.backend.sample_clock()
    }

    /// Renders `frames` stereo frames (offline backend only; used by tests and tooling).
    pub fn render_offline(&mut self, frames: usize) -> Result<Vec<f32>, AudioError> {
        let out = self.backend.render_offline(frames)?;
        self.note_rendered_frames(frames);
        Ok(out)
    }

    /// How many one-shots the backend accepted so far; monotonic.
    pub fn played_count(&self) -> u64 {
        self.played_count
    }

    /// The one-shots recorded, oldest first (kept on every backend; tests read it). The log
    /// holds at most the most recent 4096 plays (`PLAYED_LOG_CAP`): past the cap the oldest
    /// half is dropped, so a long session does not grow it without bound.
    pub fn played(&self) -> &[PlayedOneShot] {
        &self.played_log
    }

    /// Number of real (non-virtualised) voices right now.
    pub fn real_voice_count(&self) -> usize {
        self.voices.real_count()
    }

    /// Number of virtualised voices right now.
    pub fn virtual_voice_count(&self) -> usize {
        self.voices.virtual_count()
    }

    /// Live voices by kind name (`"one_shot"`, `"emitter"`, ...), for tools and debugging.
    pub fn voice_kinds(&self) -> Vec<(&'static str, usize)> {
        use VoiceKind::*;
        let mut counts = [
            ("one_shot", 0usize),
            ("emitter", 0),
            ("music", 0),
            ("announcer", 0),
            ("voice_chat", 0),
            ("patch_layer", 0),
        ];
        for entry in &self.voices.entries {
            match entry.kind {
                OneShot => counts[0].1 += 1,
                Emitter(_) => counts[1].1 += 1,
                Music => counts[2].1 += 1,
                Announcer => counts[3].1 += 1,
                VoiceChat => counts[4].1 += 1,
                PatchLayer => counts[5].1 += 1,
            }
        }
        counts.into_iter().filter(|(_, n)| *n > 0).collect()
    }

    /// The voice limit in effect.
    pub fn voice_limit(&self) -> usize {
        self.voices.limit()
    }

    // --------------------------------------------------------------- the tick

    /// Advances every audio subsystem by `dt` seconds. Called by the `AudioSync` system; tests
    /// call it directly with a fixed step.
    #[tracing::instrument(target = "regia_audio", skip_all, level = "trace")]
    pub fn tick(&mut self, dt: f64) {
        // First: a job started on a paused clock ticks with 0 here, and that is still its first tick.
        if std::mem::take(&mut self.skip_next_tick) {
            return;
        }
        // NaN too: a NaN step would reach `Duration::from_secs_f64` below, which panics.
        if dt.is_nan() || dt <= 0.0 {
            return;
        }
        self.now_secs += dt;
        // Streams: did one run dry since the last tick? (Before voice bookkeeping moves them.)
        self.watch_streams(dt);

        // Music: crossfade tails, transitions, beats and bar-boundary intensity changes.
        self.tick_music(dt);
        // Convolution: free retired slots, follow the mixer's rate, retry a busy zone IR.
        self.tick_convolution();
        self.tick_stretch();

        // Voices: aging, pruning, activity.
        self.voices.tick_state(if self.mixer_down() { 0.0 } else { dt });
        self.persistent_loops.retain(|id| self.voices.get(*id).is_some());
        // Propagation: contact fade, ground-arrival rumbles, path bookkeeping (world sound: a
        // zero step while the gameplay pause is on, ADR-10105).
        self.tick_propagation(self.world_step(dt));

        // Announcer line finished? Start the queued one.
        let announcer_done = self.announcer_voice.is_some_and(|voice| {
            self.voices
                .get(voice)
                .is_none_or(|entry| entry.instance.is_none())
        });
        if announcer_done {
            self.announcer_voice = None;
            if let Some(queued) = self.announcer.note_finished() {
                self.start_announcer_line(&queued);
            }
        }
        if self.announcer.is_active() {
            self.voices.bump_activity("Announcer");
        }
        if self.voice_playback.as_ref().is_some_and(|h| h.stats().running) {
            self.voices.bump_activity("Voice");
        }
        // A voice-chat playback feed (and any other feed on the bus named `Voice`) raises the
        // same key. Capture loopback above is a separate path. The match is the layout's exact
        // name, so a feed that resolved to another bus does not duck.
        self.voice_bus_feeds.retain(|handle| !handle.is_stopped());
        if !self.voice_bus_feeds.is_empty() {
            let feeds = u32::try_from(self.voice_bus_feeds.len()).unwrap_or(u32::MAX);
            self.voices.bump_activity_by("Voice", feeds);
        }

        // Ducking + bus volume composition (snapshot blend eased by BusBlend).
        // The activity map is borrowed only for the advance; bus commands are collected when
        // the 0.05 dB gate fires so a steady mix does not clone bus names.
        self.duck.advance(dt, &self.duck_rules, self.voices.activity());
        let mut pending_buses: Vec<(String, f32)> = Vec::new();
        {
            let offsets = self.duck.offsets();
            let snapshots = &self.snapshots;
            for (bus, blend) in &mut self.bus_mix {
                let distance = blend.target_db - blend.current_db;
                let step = distance.abs().min(blend.rate_db_per_sec * dt as f32);
                if distance > 0.0 {
                    blend.current_db += step;
                } else {
                    blend.current_db -= step;
                }
                let duck_offset = offsets.get(bus).copied().unwrap_or(0.0);
                // An active snapshot's mute override beats the host mute (`audio-buses` v1).
                let muted = snapshots.target_mute(bus).unwrap_or(blend.muted);
                let final_db = if muted {
                    -100.0
                } else {
                    blend.current_db + duck_offset
                };
                // `last_applied_db` starts as NaN (nothing applied yet): NaN arithmetic is never
                // `> 0.05`, so the first application must be checked for explicitly.
                if blend.last_applied_db.is_nan() || (blend.last_applied_db - final_db).abs() > 0.05 {
                    blend.last_applied_db = final_db;
                    pending_buses.push((bus.clone(), final_db));
                }
            }
        }
        let fade = Duration::from_secs_f64((dt * 2.0).clamp(0.001, 0.05));
        for (bus, final_db) in &pending_buses {
            self.buses.set_volume(bus, *final_db, fade);
        }

        // Streaming decoders report failures out of band; surface them instead of going silent.
        for entry in &mut self.voices.entries {
            if let Some(instance) = entry.instance.as_mut()
                && let Some(error) = instance.pop_error()
            {
                tracing::warn!(
                    target: "regia_audio",
                    sound = %entry.asset.label(),
                    "streaming decoder error: {error}"
                );
            }
        }

        // Virtualisation.
        let actions = if self.mixer_down() {
            crate::voices::ReconcileActions::default()
        } else {
            self.voices.reconcile()
        };
        for id in actions.virtualise {
            if let Some(entry) = self.voices.get_mut(id) {
                entry.virtualise();
            }
        }
        for id in actions.revive {
            self.revive_voice(id);
        }

        // Patches (world sound: a zero step while the gameplay pause is on).
        self.tick_patches(self.world_step(dt));
    }

    /// Revives a virtualised voice from its cursor.
    fn revive_voice(&mut self, id: SoundInstanceId) {
        let Some(entry) = self.voices.get_mut(id) else {
            return;
        };
        let mut params = entry.params.clone();
        params.start_offset_secs = entry.virtual_cursor_secs.max(0.0);
        params.fade_in = Some(Duration::from_millis(30));
        let asset = entry.asset.clone();
        let bus = entry.bus.clone();
        let played = asset
            .play_data(&params)
            .and_then(|data| self.play_revived(id, &bus, data));
        match played {
            Ok(mut instance) => {
                instance.watch_render_clock(self.backend.limiter().render_clock());
                if let Some(entry) = self.voices.get_mut(id) {
                    entry.instance = Some(instance);
                    entry.virtual_cursor_secs = 0.0;
                    if entry.paused || entry.held {
                        // A pause the host asked for survives the revive: re-apply it at
                        // once (a fresh handle has not been heard, so no declick fade).
                        if let Some(instance) = entry.instance.as_mut() {
                            instance.pause(Duration::ZERO);
                        }
                    }
                }
                self.forget_sent_frame(id);
            }
            Err(error) => tracing::warn!(target: "regia_audio", "voice {id:?} could not revive: {error}"),
        }
    }
}

/// Stops the loops of a patch whose spawn failed part-way: the current layer's `loops` and every
/// finished layer, so no orphaned voice keeps playing without an id.
fn stop_patch_loops(loops: &mut [PatchLoop], layers: &mut [PatchLayerRuntime]) {
    let started = loops
        .iter_mut()
        .chain(layers.iter_mut().flat_map(|layer| layer.loops.iter_mut()));
    for patch_loop in started {
        patch_loop.instance.stop(Duration::ZERO);
    }
    for granular in layers.iter().filter_map(|layer| layer.granular.as_ref()) {
        granular.stop();
    }
}

/// The one-off low-pass child track a filtered cue voice plays on (flat, positional and attached
/// plays; an attached voice's is a sub-track of its emitter's track, ADR-5977):
/// it persists until its sounds finish, so the voice holds its handle until then.
fn low_pass_track(cutoff_hz: f32) -> TrackBuilder {
    let mut builder = TrackBuilder::new().persist_until_sounds_finish(true);
    let _filter = builder.add_effect(
        FilterBuilder::new()
            .mode(FilterMode::LowPass)
            .cutoff(f64::from(cutoff_hz))
            .resonance(0.0),
    );
    builder
}

/// Plays prepared data through a bus track.
fn play_data_on(buses: &mut Buses, bus: &str, data: PlayData) -> Result<SoundInstance, AudioError> {
    let track = buses
        .track(bus)
        .ok_or_else(|| AudioError::UnknownBus(bus.into()))?;
    match data {
        PlayData::Static(data) => Ok(SoundInstance::from_static(
            track.play(data).map_err(crate::sound::play_error)?,
        )),
        PlayData::Streaming(data) => Ok(SoundInstance::from_streaming(
            track.play(data).map_err(crate::sound::play_error)?,
        )),
    }
}

/// Plays prepared data through an explicit track handle.
fn play_data_on_track(track: &mut TrackHandle, data: PlayData) -> Result<SoundInstance, AudioError> {
    match data {
        PlayData::Static(data) => Ok(SoundInstance::from_static(
            track.play(data).map_err(crate::sound::play_error)?,
        )),
        PlayData::Streaming(data) => Ok(SoundInstance::from_streaming(
            track.play(data).map_err(crate::sound::play_error)?,
        )),
    }
}

/// The resolved bus name of playback parameters.
/// The bus a play routes to: the params' bus, else the asset's default bus (spec 16 § 2), else
/// `Master`.
fn bus_of(params: &PlaybackParams, asset: Option<&SoundAsset>) -> String {
    params
        .bus
        .clone()
        .or_else(|| asset.and_then(|asset| asset.default_bus().map(str::to_string)))
        .unwrap_or_else(|| "Master".into())
}

/// Resolves the play's bus ([`bus_of`]) and records it in `params.bus`, so the voice's
/// bookkeeping (`VoiceEntry::bus`, revive, ducking) names the track it actually plays on — the
/// asset's default bus included. Returns the resolved bus.
fn resolved_params_bus(params: &mut PlaybackParams, asset: Option<&SoundAsset>) -> String {
    let bus = bus_of(params, asset);
    params.bus = Some(bus.clone());
    bus
}

fn priority_for(kind: VoiceKind) -> f32 {
    match kind {
        VoiceKind::OneShot | VoiceKind::Emitter(_) | VoiceKind::PatchLayer => voice_priority::NORMAL,
        VoiceKind::Music | VoiceKind::Announcer => voice_priority::HIGH,
        VoiceKind::VoiceChat => voice_priority::CRITICAL,
    }
}

fn rate_for(from: f32, to: f32, fade_secs: f32) -> f32 {
    if fade_secs <= 1e-4 {
        f32::INFINITY
    } else {
        (to - from).abs() / fade_secs
    }
}

#[cfg(test)]
mod compile_checks {
    use super::*;

    #[test]
    fn audio_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Audio>();
    }
}

#[cfg(test)]
mod capture_state_tests {
    use super::*;
    use crate::voice::VoiceCaptureSettings;

    fn headless() -> Audio {
        Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        })
    }

    /// A headless audio with a live capture session whose stream is closed — the state
    /// `Audio::suspend_output` leaves, reproducible on a host with no microphone. The session
    /// asks for a device name that cannot match, so a resume attempt fails deterministically
    /// even on a host that has a real input.
    fn audio_with_suspended_capture() -> Audio {
        let mut audio = headless();
        let settings = VoiceCaptureSettings {
            device: Some("__regia_test_no_such_device__".into()),
            ..VoiceCaptureSettings::default()
        };
        audio.capture = Some(crate::voice::AudioCapture::stopped_for_test(&settings));
        audio
    }

    #[test]
    fn accessors_report_the_not_capturing_defaults() {
        let audio = headless();
        assert!(!audio.is_capturing());
        assert_eq!(audio.capture_channels(), 0);
        assert_eq!(audio.capture_dropped_frames(), 0);
        assert!(!audio.is_talking());
    }

    #[test]
    fn suspended_capture_reads_as_not_capturing() {
        let mut audio = audio_with_suspended_capture();
        audio.suspend_output();
        assert!(audio.output_suspended());
        // The microphone is released for the suspend: the accessors must read exactly like
        // "not capturing", not report the stale pre-suspend session state (ADR-4870).
        assert!(!audio.is_capturing(), "no microphone is held while suspended");
        assert_eq!(audio.capture_channels(), 0);
        assert_eq!(audio.capture_dropped_frames(), 0);
        assert!(!audio.is_talking());
    }

    #[test]
    fn a_failed_resume_keeps_the_accessors_at_not_capturing() {
        let mut audio = audio_with_suspended_capture();
        audio.suspend_output();
        // The headless host has no input device, so the capture cannot re-open; the output
        // itself resumes. The accessors must keep reading "not capturing" until the
        // microphone really comes back.
        assert!(audio.resume_output(), "the offline output resumes");
        assert!(!audio.output_suspended());
        assert!(!audio.is_capturing());
        assert_eq!(audio.capture_channels(), 0);
        assert_eq!(audio.capture_dropped_frames(), 0);
        assert!(!audio.is_talking());
    }

    #[test]
    fn suspend_while_suspended_is_a_no_op() {
        let mut audio = audio_with_suspended_capture();
        audio.suspend_output();
        let once = (audio.suspend_count(), audio.resume_count());
        audio.suspend_output();
        assert_eq!(
            (audio.suspend_count(), audio.resume_count()),
            once,
            "the counters only count real transitions"
        );
        assert!(!audio.is_capturing());
    }
}

#[cfg(test)]
mod asset_default_tests {
    use super::*;

    fn headless() -> Audio {
        Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        })
    }

    fn tone(len: usize) -> SoundAsset {
        let clip = crate::SoundClip::new(vec![0.1; len], 48_000).expect("clip");
        SoundAsset::from_clip(&clip)
    }

    /// Spec 16 § 2: the sound asset's default bus routes a play whose params name no bus; an
    /// explicit params bus still wins; an asset without a default routes to `Master`.
    #[test]
    fn spec16_section2_the_assets_default_bus_routes_a_bus_less_play() {
        let mut audio = headless();
        let asset = tone(4_800).with_default_bus("Sfx");
        let id = audio
            .start_asset(&asset, PlaybackParams::default())
            .expect("plays");
        assert_eq!(audio.voices.get(id).expect("voice").bus.as_ref(), "Sfx");
        let id = audio
            .start_asset(&asset, PlaybackParams::on_bus("Music"))
            .expect("plays");
        assert_eq!(audio.voices.get(id).expect("voice").bus.as_ref(), "Music");
        let plain = tone(4_800);
        let id = audio
            .start_asset(&plain, PlaybackParams::default())
            .expect("plays");
        assert_eq!(audio.voices.get(id).expect("voice").bus.as_ref(), "Master");
    }
}

/// Spec 16 § 3: a cue plays in 3D — `play_at(cue, …)` at a position, `play_on(entity, cue)`
/// attached to an entity — with the cue's variation, bus, limits and name intact.
#[cfg(test)]
mod cue_place_tests {
    use super::*;
    use crate::spatial::AudioEmitter;
    use regia_core::math::Vec3;

    fn headless() -> Audio {
        Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        })
    }

    fn tone(len: usize) -> SoundAsset {
        let clip = crate::SoundClip::new(vec![0.2; len], 48_000).expect("clip");
        SoundAsset::from_clip(&clip)
    }

    fn single_cue(name: &str) -> crate::cue::SoundCue {
        crate::cue::SoundCue::new(
            name,
            vec![crate::cue::CueEntry {
                asset: tone(4_800),
                weight: 1.0,
            }],
        )
    }

    #[test]
    fn play_cue_at_registers_a_spatial_one_shot_under_the_cue_name() {
        let mut audio = headless();
        audio.register_cue(single_cue("hit"));
        let emitter = AudioEmitter::default();
        let id = audio
            .play_cue_at("hit", Vec3::new(3.0, 0.0, 0.0), &emitter)
            .expect("plays")
            .expect("audible");
        let entry = audio.voices.get(id).expect("voice");
        assert_eq!(entry.kind, VoiceKind::OneShot, "a positional cue is a one-shot");
        assert_eq!(entry.cue.as_deref(), Some("hit"), "tracked under the cue name");
        assert!(entry.gain < 1.0, "distance gain applied: {}", entry.gain);
        assert_eq!(audio.played_count(), 1);
    }

    #[test]
    fn play_cue_at_out_of_range_is_silent_and_not_counted() {
        let mut audio = headless();
        audio.register_cue(single_cue("hit"));
        let emitter = AudioEmitter::default();
        // 1 km away, max_distance 50 m: nothing audible reaches the listener.
        let id = audio
            .play_cue_at("hit", Vec3::new(1_000.0, 0.0, 0.0), &emitter)
            .expect("plays");
        assert_eq!(id, None, "no path is audible");
        assert_eq!(audio.played_count(), 0, "an inaudible play is not counted");
    }

    #[test]
    fn play_cue_at_applies_the_cooldown() {
        let mut audio = headless();
        let mut cue = single_cue("hit");
        cue.cooldown = Duration::from_millis(100);
        audio.register_cue(cue);
        let emitter = AudioEmitter::default();
        assert!(
            audio
                .play_cue_at("hit", Vec3::ZERO, &emitter)
                .expect("plays")
                .is_some()
        );
        assert!(
            audio
                .play_cue_at("hit", Vec3::ZERO, &emitter)
                .expect("plays")
                .is_none(),
            "cooldown"
        );
        audio.tick(0.2);
        assert!(
            audio
                .play_cue_at("hit", Vec3::ZERO, &emitter)
                .expect("plays")
                .is_some(),
            "after"
        );
    }

    #[test]
    fn play_cue_seeded_at_replays_the_same_variation() {
        let mut audio = headless();
        let mut cue = single_cue("vary");
        cue.pitch_range = (0.5, 2.0);
        audio.register_cue(cue);
        let emitter = AudioEmitter::default();
        let seed = crate::cue::cue_event_seed(7, 100);
        let pitch_of = |audio: &Audio, id: SoundInstanceId| audio.voices.get(id).expect("voice").params.pitch;
        let a = audio
            .play_cue_seeded_at("vary", seed, Vec3::ZERO, &emitter)
            .expect("plays")
            .expect("audible");
        let b = audio
            .play_cue_seeded_at("vary", seed, Vec3::ZERO, &emitter)
            .expect("plays")
            .expect("audible");
        assert_eq!(pitch_of(&audio, a), pitch_of(&audio, b), "same seed, same pitch");
        // And the variation actually applied (rev-kimi-10 finding 10): the pick's pitch is a
        // draw inside the cue's range, not the neutral 1.0, and another seed draws another one.
        let pitch = pitch_of(&audio, a);
        assert!((0.5..=2.0).contains(&pitch), "inside the cue's range: {pitch}");
        assert_ne!(pitch, 1.0, "the variation applied, not the neutral pitch");
        let c = audio
            .play_cue_seeded_at("vary", crate::cue::cue_event_seed(8, 100), Vec3::ZERO, &emitter)
            .expect("plays")
            .expect("audible");
        assert_ne!(pitch_of(&audio, c), pitch, "another seed, another variation");
    }

    #[test]
    fn play_cue_on_attaches_an_emitter_voice_that_follows_the_entity() {
        let mut audio = headless();
        audio.register_cue(single_cue("hit"));
        let entity = Entity::from_raw_u32(9).expect("entity");
        let emitter = AudioEmitter::default();
        let id = audio
            .play_cue_on("hit", entity, &emitter)
            .expect("plays")
            .expect("started");
        let entry = audio.voices.get(id).expect("voice");
        assert_eq!(entry.kind, VoiceKind::Emitter(entity), "attached to the entity");
        assert_eq!(entry.cue.as_deref(), Some("hit"));
        // The emitter reports the live voice.
        assert_eq!(
            audio
                .emitter_status()
                .iter()
                .find(|s| s.entity == entity)
                .map(|s| s.voices),
            Some(1)
        );
    }

    #[test]
    fn play_cue_on_keeps_the_cue_pitch_variation() {
        let mut audio = headless();
        let mut cue = single_cue("vary");
        cue.pitch_range = (1.5, 1.5);
        audio.register_cue(cue);
        let entity = Entity::from_raw_u32(5).expect("entity");
        let emitter = AudioEmitter::default();
        let id = audio
            .play_cue_on("vary", entity, &emitter)
            .expect("plays")
            .expect("started");
        // The attached voice keeps the cue's locked 1.5 pitch in its params (Doppler composes on
        // the live handle, not in params).
        assert_eq!(audio.voices.get(id).expect("voice").params.pitch, 1.5);
    }

    #[test]
    fn an_unknown_cue_in_3d_is_an_error() {
        let mut audio = headless();
        let emitter = AudioEmitter::default();
        assert!(matches!(
            audio.play_cue_at("nope", Vec3::ZERO, &emitter),
            Err(AudioError::UnknownEntry(_))
        ));
        assert!(matches!(
            audio.play_cue_on("nope", Entity::from_raw_u32(1).expect("e"), &emitter),
            Err(AudioError::UnknownEntry(_))
        ));
    }

    /// A two-entry sequential cue whose entries tell apart by length (4 800 / 4 900 frames).
    fn sequential_cue(name: &str) -> crate::cue::SoundCue {
        let mut cue = crate::cue::SoundCue::new(
            name,
            [4_800, 4_900]
                .into_iter()
                .map(|len| crate::cue::CueEntry {
                    asset: tone(len),
                    weight: 1.0,
                })
                .collect(),
        );
        cue.mode = crate::cue::CuePlayMode::Sequential;
        cue.max_concurrent = None;
        cue
    }

    fn frames_of(audio: &Audio, id: SoundInstanceId) -> usize {
        (audio.voices.get(id).expect("voice").asset.duration_secs() * 48_000.0).round() as usize
    }

    /// integ-v048c's selector rollback on the 3D places (rev-kimi-10 finding 4): a `play_cue_at`
    /// that reaches nothing audible, and a `play_cue_at` / `play_cue_on` that fails, consume no
    /// pick: the sequential cue plays entry 0 next.
    #[test]
    fn a_failed_or_inaudible_3d_cue_play_leaves_the_selector_unchanged() {
        let mut audio = headless();
        audio.register_cue(sequential_cue("seq"));
        let emitter = AudioEmitter::default();
        // Inaudible: 1 km away, nothing reaches the listener.
        let far = audio
            .play_cue_at("seq", Vec3::new(1_000.0, 0.0, 0.0), &emitter)
            .expect("no error");
        assert_eq!(far, None, "out of range");
        // Failed: the cue routes to a bus that does not exist.
        if let Some(cue) = audio.cues.get_mut("seq") {
            cue.bus = Some("no_such_bus".into());
        }
        assert!(
            audio
                .play_cue_at("seq", Vec3::new(2.0, 0.0, 0.0), &emitter)
                .is_err(),
            "unknown bus, positional"
        );
        let entity = Entity::from_raw_u32(21).expect("entity");
        assert!(
            audio.play_cue_on("seq", entity, &emitter).is_err(),
            "unknown bus, attached"
        );
        if let Some(cue) = audio.cues.get_mut("seq") {
            cue.bus = None;
        }
        let first = audio
            .play_cue_at("seq", Vec3::new(2.0, 0.0, 0.0), &emitter)
            .expect("plays")
            .expect("audible");
        assert_eq!(frames_of(&audio, first), 4_800, "entry 0 was not consumed");
        let second = audio
            .play_cue_on("seq", entity, &emitter)
            .expect("plays")
            .expect("started");
        assert_eq!(
            frames_of(&audio, second),
            4_900,
            "a played pick advances the cursor"
        );
    }

    /// Rev-kimi-10 finding 5: the steal waits until the new play is known audible. A far
    /// positional play that reaches nothing keeps the near voice it would have replaced.
    #[test]
    fn an_inaudible_positional_play_steals_nothing() {
        let mut audio = headless();
        let mut cue = single_cue("shot");
        cue.max_concurrent = Some(1);
        cue.steal = StealPolicy::Oldest;
        audio.register_cue(cue);
        let emitter = AudioEmitter::default();
        let near = audio
            .play_cue_at("shot", Vec3::new(2.0, 0.0, 0.0), &emitter)
            .expect("plays")
            .expect("audible");
        let far = audio
            .play_cue_at("shot", Vec3::new(1_000.0, 0.0, 0.0), &emitter)
            .expect("no error");
        assert_eq!(far, None, "nothing audible");
        let entry = audio
            .voices
            .get(near)
            .expect("the near voice is still registered");
        assert!(
            entry.instance.is_some() && !entry.stopping,
            "the near voice was not stolen by an inaudible play"
        );
        // An audible play does steal it.
        let next = audio
            .play_cue_at("shot", Vec3::new(3.0, 0.0, 0.0), &emitter)
            .expect("plays")
            .expect("audible");
        assert_ne!(next, near);
        assert!(
            audio.voices.get(near).is_none_or(|e| e.stopping),
            "an audible play steals the oldest"
        );
    }

    /// Rev-kimi-10 finding 5: a positional play that takes both the airborne and the ground path
    /// registers two voices under the cue, but it is one play: it counts once toward
    /// `max_concurrent`, and a steal stops both voices.
    #[test]
    fn a_two_path_positional_play_counts_once_toward_max_concurrent() {
        let mut audio = headless();
        let mut medium = crate::AudioMedium::earth();
        medium.ground = "bedrock".into();
        audio.set_medium(medium);
        audio.set_listener(crate::ListenerFrame::looking_at(Vec3::ZERO, Vec3::X));
        let mut cue = single_cue("boom");
        cue.max_concurrent = Some(2);
        cue.steal = StealPolicy::None;
        audio.register_cue(cue);
        // One explosion through the medium, tagged with the cue (as `play_cue_at` tags it).
        let loud = AudioEmitter {
            min_distance: 30.0,
            max_distance: 5_000.0,
            ..AudioEmitter::default()
        };
        let first = audio
            .start_request(PlayRequest {
                asset: tone(48_000),
                params: PlaybackParams::on_bus("World"),
                position: Some(Vec3::new(300.0, 0.0, 0.0)),
                kind: VoiceKind::OneShot,
                cue: Some(Arc::from("boom")),
                emitter: Some(loud),
                propagation: Some(crate::coupling::AudioPropagation::for_category(
                    crate::coupling::EmitterCategory::Explosion,
                )),
                filter_cutoff_hz: None,
            })
            .expect("plays")
            .expect("audible");
        let tagged: Vec<SoundInstanceId> = audio
            .voices
            .iter()
            .filter(|e| e.cue.as_deref() == Some("boom") && e.instance.is_some())
            .map(|e| e.id)
            .collect();
        assert_eq!(tagged.len(), 2, "the play took both paths: {tagged:?}");
        assert_eq!(audio.cue_plays_active("boom"), 1, "two path voices, one play");
        assert_eq!(audio.played_log.len(), 1, "one played entry per request");
        // With max 2 and no steal, a second play still fits.
        assert!(
            audio.play_cue("boom").expect("cue").is_some(),
            "the two-path play counted once, so the limit of 2 is not reached"
        );
        assert!(audio.play_cue("boom").expect("cue").is_none(), "now it is");
        // Stealing the play stops both of its voices.
        audio.stop_cue_play(first, Duration::ZERO);
        for id in tagged {
            assert!(
                audio.voices.get(id).is_none_or(|e| e.stopping),
                "voice {id:?} of the stolen play stopped"
            );
        }
    }

    /// Rev-kimi-10 finding 3: `play_cue_at` keeps the cue's authored low-pass (each path voice
    /// holds its one-off filtered track, like a flat play's).
    #[test]
    fn play_cue_at_keeps_the_cues_low_pass_track() {
        let mut audio = headless();
        let mut cue = single_cue("muffled");
        cue.filter_cutoff_hz = Some(400.0);
        audio.register_cue(cue);
        audio.register_cue(single_cue("open"));
        let emitter = AudioEmitter::default();
        let id = audio
            .play_cue_at("muffled", Vec3::new(2.0, 0.0, 0.0), &emitter)
            .expect("plays")
            .expect("audible");
        assert!(
            audio.voices.get(id).expect("voice").filter_track.is_some(),
            "the positional voice plays on the cue's low-pass track"
        );
        let id = audio
            .play_cue_at("open", Vec3::new(2.0, 0.0, 0.0), &emitter)
            .expect("plays")
            .expect("audible");
        assert!(
            audio.voices.get(id).expect("voice").filter_track.is_none(),
            "an unfiltered cue needs no track"
        );
    }

    /// Rev-kimi-10 finding 6: a cue emitter's voice composes the emitter's `volume_db` with the
    /// cue's level, at start and on every sync.
    #[test]
    fn a_cue_emitter_applies_the_emitters_volume() {
        let mut audio = headless();
        let mut cues = crate::CueAssets::default();
        let mut cue = single_cue("steps");
        cue.volume_db = -3.0;
        cues.add("steps", cue);
        let entity = Entity::from_raw_u32(4).expect("entity");
        let mut update = EmitterUpdate {
            entity,
            emitter: AudioEmitter {
                cue_name: "steps".into(),
                volume_db: -20.0,
                ..AudioEmitter::default()
            },
            frame: crate::spatial::EmitterFrame::default(),
        };
        let base_of = |audio: &Audio| {
            audio
                .voices
                .iter()
                .find(|e| e.kind == VoiceKind::Emitter(entity))
                .map(|e| e.base_volume_db)
        };
        audio.sync_emitter_frames(std::slice::from_ref(&update), 1.0 / 60.0, None, Some(&cues), None);
        assert_eq!(base_of(&audio), Some(-23.0), "cue −3 dB plus emitter −20 dB");
        update.emitter.volume_db = -6.0;
        audio.sync_emitter_frames(std::slice::from_ref(&update), 1.0 / 60.0, None, Some(&cues), None);
        assert_eq!(
            base_of(&audio),
            Some(-9.0),
            "a changed emitter volume applies to the cue voice"
        );
    }

    #[test]
    fn an_emitter_with_a_cue_autoplays_a_variation() {
        let mut audio = headless();
        let mut cues = crate::CueAssets::default();
        cues.add("steps", single_cue("steps"));
        let entity = Entity::from_raw_u32(3).expect("entity");
        let emitter = AudioEmitter {
            cue_name: "steps".into(),
            looping: false,
            ..AudioEmitter::default()
        };
        let update = EmitterUpdate {
            entity,
            emitter: emitter.clone(),
            frame: crate::spatial::EmitterFrame {
                position: Vec3::ZERO,
                ..Default::default()
            },
        };
        audio.sync_emitter_frames(&[update], 1.0 / 60.0, None, Some(&cues), None);
        let status = audio
            .emitter_status()
            .into_iter()
            .find(|s| s.entity == entity)
            .expect("the emitter is tracked");
        assert_eq!(status.voices, 1, "the cue autoplayed one variation");
        let live_emitter_voices = |audio: &Audio| -> Vec<(SoundInstanceId, f64)> {
            audio
                .voices
                .iter()
                .filter(|e| e.kind == VoiceKind::Emitter(entity) && e.instance.is_some() && !e.stopping)
                .map(|e| (e.id, e.asset.duration_secs()))
                .collect()
        };
        let cue_voice = live_emitter_voices(&audio);
        assert_eq!(cue_voice.len(), 1, "one live cue voice: {cue_voice:?}");
        // Switching to a sound drops the cue voice and plays the sound (rev-kimi-10 finding 10:
        // the switch really goes to a sound). The sound is twice the cue entry's length, so the
        // live voice tells which one plays.
        let mut sounds = crate::SoundAssets::default();
        sounds.add("hum", tone(9_600));
        let sound_emitter = AudioEmitter {
            sound_name: "hum".into(),
            cue_name: "steps".into(),
            ..AudioEmitter::default()
        };
        let update = EmitterUpdate {
            entity,
            emitter: sound_emitter,
            frame: crate::spatial::EmitterFrame::default(),
        };
        audio.sync_emitter_frames(&[update], 1.0 / 60.0, Some(&sounds), Some(&cues), None);
        let now = live_emitter_voices(&audio);
        assert_eq!(now.len(), 1, "only the sound plays: {now:?}");
        assert_ne!(now[0].0, cue_voice[0].0, "the cue voice was dropped");
        assert!(
            (now[0].1 - 0.2).abs() < 1e-9,
            "the live voice is the 9 600-frame sound, not the 4 800-frame cue entry: {now:?}"
        );
        // A silent emitter stops the sound too.
        let silent = AudioEmitter {
            autoplay: false,
            ..AudioEmitter::default()
        };
        let update = EmitterUpdate {
            entity,
            emitter: silent,
            frame: crate::spatial::EmitterFrame::default(),
        };
        audio.sync_emitter_frames(&[update], 1.0 / 60.0, Some(&sounds), Some(&cues), None);
        assert_eq!(audio.real_voice_count(), 0, "a silent emitter stops the voice");
    }

    #[test]
    fn an_emitter_with_an_empty_cue_plays_nothing() {
        // `SoundCue::new("x", vec![])` is accepted by `CueAssets::add` (the rustdoc example in
        // cue.rs builds one); the flat play path guards it (`play_cue_with_no_entries_plays_nothing`),
        // so the emitter autoplay path must not index `entries[0]` either.
        let mut audio = headless();
        let mut cues = crate::CueAssets::default();
        cues.add("empty", crate::cue::SoundCue::new("empty", vec![]));
        let entity = Entity::from_raw_u32(4).expect("entity");
        let emitter = AudioEmitter {
            cue_name: "empty".into(),
            ..AudioEmitter::default()
        };
        let update = EmitterUpdate {
            entity,
            emitter: emitter.clone(),
            frame: crate::spatial::EmitterFrame::default(),
        };
        for _ in 0..2 {
            audio.sync_emitter_frames(std::slice::from_ref(&update), 1.0 / 60.0, None, Some(&cues), None);
            assert_eq!(audio.real_voice_count(), 0, "an empty cue autoplays nothing");
        }
    }

    /// Deep-zai-audio-mix finding 7: `pause_voice` is public spec surface; the pause must
    /// survive the internal replays that rebuild an emitter's track (here: a bus change) and a
    /// virtualise/revive round trip.
    #[test]
    fn a_paused_emitter_voice_stays_paused_across_rebuilds() {
        use kira::sound::PlaybackState;
        let mut audio = headless();
        let mut sounds = crate::SoundAssets::default();
        sounds.add("hum", tone(9_600));
        let entity = Entity::from_raw_u32(5).expect("entity");
        let base = EmitterUpdate {
            entity,
            emitter: AudioEmitter {
                sound_name: "hum".into(),
                ..AudioEmitter::default()
            },
            frame: crate::spatial::EmitterFrame::default(),
        };
        audio.sync_emitter_frames(std::slice::from_ref(&base), 1.0 / 60.0, Some(&sounds), None, None);
        // Render a block: kira only advances a handle's playback state while the mixer runs.
        let _ = audio.render_offline(480);
        let id = audio
            .voices
            .iter()
            .find(|e| e.kind == VoiceKind::Emitter(entity))
            .map(|e| e.id)
            .expect("the emitter autoplayed a voice");
        audio.pause_voice(id, Duration::ZERO);
        let _ = audio.render_offline(960);
        let entry = audio.voices.get(id).expect("voice");
        assert!(entry.paused, "pause_voice records the pause on the entry");
        assert_eq!(
            entry.instance.as_ref().expect("live").state(),
            PlaybackState::Paused,
            "the live handle paused"
        );

        // A bus change rebuilds the emitter's track and replays every voice onto it.
        let moved = EmitterUpdate {
            emitter: AudioEmitter {
                bus: Some("Sfx".into()),
                ..base.emitter.clone()
            },
            ..base
        };
        audio.sync_emitter_frames(&[moved], 1.0 / 60.0, Some(&sounds), None, None);
        let _ = audio.render_offline(960);
        let entry = audio.voices.get(id).expect("the voice moved, id kept");
        assert!(
            entry.paused,
            "the rebuild replay keeps the pause (the old behaviour un-paused it)"
        );
        assert_eq!(
            entry.instance.as_ref().expect("replayed").state(),
            PlaybackState::Paused,
            "the replayed handle is paused, not playing"
        );

        // A virtualise/revive round trip (the voice fell out of the budget, then came back):
        // the revive must not play either.
        audio.voices.get_mut(id).expect("voice").virtualise();
        assert!(audio.voices.get(id).expect("voice").instance.is_none());
        audio.revive_voice(id);
        let _ = audio.render_offline(960);
        let entry = audio.voices.get(id).expect("voice");
        assert!(entry.paused, "the revive re-applies the pause");
        assert_eq!(
            entry.instance.as_ref().expect("revived").state(),
            PlaybackState::Paused,
            "the revived handle is paused, not playing"
        );

        // `resume_voice` clears it.
        audio.resume_voice(id, Duration::ZERO);
        assert!(!audio.voices.get(id).expect("voice").paused);
    }

    /// Rev-kimi-28 (audio2-k item 3): the `played_log` cap must not break "one played entry
    /// per request" for a two-path positional play. With the log one entry short of its cap,
    /// the airborne entry fills it and the ground entry overflowed it: the half-drain ran
    /// inside the request, so the truncate back to the saved length did nothing and the
    /// request logged two entries.
    #[test]
    fn a_two_path_play_logs_once_across_the_log_cap() {
        let mut audio = headless();
        let mut medium = crate::AudioMedium::earth();
        medium.ground = "bedrock".into();
        audio.set_medium(medium);
        audio.set_listener(crate::ListenerFrame::looking_at(Vec3::ZERO, Vec3::X));
        let filler = PlayedOneShot {
            clip_len: 1,
            sample_rate: 48_000,
            position: None,
        };
        audio.played_log = vec![filler; Audio::PLAYED_LOG_CAP - 1];
        let position = Vec3::new(300.0, 0.0, 0.0);
        audio
            .start_request(PlayRequest {
                asset: tone(48_000),
                params: PlaybackParams::on_bus("World"),
                position: Some(position),
                kind: VoiceKind::OneShot,
                cue: None,
                emitter: Some(AudioEmitter {
                    min_distance: 30.0,
                    max_distance: 5_000.0,
                    ..AudioEmitter::default()
                }),
                propagation: Some(crate::coupling::AudioPropagation::for_category(
                    crate::coupling::EmitterCategory::Explosion,
                )),
                filter_cutoff_hz: None,
            })
            .expect("plays")
            .expect("audible");
        assert_eq!(
            audio.voices.iter().filter(|e| e.instance.is_some()).count(),
            2,
            "the play took both paths"
        );
        let logged = audio
            .played()
            .iter()
            .filter(|p| p.position == Some(position))
            .count();
        assert_eq!(logged, 1, "one played entry per request, at the cap too");
        assert!(audio.played().len() <= Audio::PLAYED_LOG_CAP);
    }

    /// Deep-zai-audio-mix finding 10: `played_log` is a diagnostics tail for tests and tools;
    /// a long one-shot-heavy session must not grow it without bound.
    #[test]
    fn played_log_keeps_only_the_recent_tail() {
        let mut audio = headless();
        let total = Audio::PLAYED_LOG_CAP + 100;
        for i in 0..total {
            // Micro-offsets keep every play inaudible-distance-wise identical (full gain) while
            // the recorded position identifies the play. Each voice is stopped at once and
            // reaped every few plays: kira caps the concurrent sounds, and the log — what
            // this test exercises — keeps the entry regardless of the voice's life.
            let at = Vec3::new(i as f32 * 1e-6, 0.0, 0.0);
            let id = audio
                .start_request(PlayRequest {
                    asset: tone(48),
                    params: PlaybackParams::on_bus("World"),
                    position: Some(at),
                    kind: VoiceKind::OneShot,
                    cue: None,
                    emitter: None,
                    propagation: None,
                    filter_cutoff_hz: None,
                })
                .expect("plays")
                .expect("audible at {at:?}");
            audio.stop_voice(id, Duration::ZERO);
            if i % 16 == 15 {
                audio.tick(1.0 / 60.0);
                let _ = audio.render_offline(960);
            }
        }
        let log = audio.played();
        assert!(
            log.len() <= Audio::PLAYED_LOG_CAP,
            "the log is capped: {} entries",
            log.len()
        );
        // The cap drops the oldest half each time it fills: after `total` plays the log holds
        // the tail, in order.
        let expected_len = total - Audio::PLAYED_LOG_CAP / 2;
        assert_eq!(log.len(), expected_len, "one half-drain happened");
        let first_kept = (total - log.len()) as f32 * 1e-6;
        assert_eq!(
            log[0].position,
            Some(Vec3::new(first_kept, 0.0, 0.0)),
            "the oldest retained entry"
        );
        assert_eq!(
            log[log.len() - 1].position,
            Some(Vec3::new((total - 1) as f32 * 1e-6, 0.0, 0.0)),
            "the newest entry is the last play"
        );
    }

    /// A `hz` sine cue lasting `secs`, with the authored low-pass `cutoff`.
    fn sine_cue(name: &str, hz: f32, secs: f64, cutoff: Option<f32>) -> crate::cue::SoundCue {
        let len = (secs * 48_000.0) as usize;
        let samples: Vec<f32> = (0..len)
            .map(|i| (i as f32 / 48_000.0 * hz * std::f32::consts::TAU).sin() * 0.5)
            .collect();
        let clip = crate::SoundClip::new(samples, 48_000).expect("clip");
        let mut cue = crate::cue::SoundCue::new(
            name,
            vec![crate::cue::CueEntry {
                asset: SoundAsset::from_clip(&clip),
                weight: 1.0,
            }],
        );
        cue.max_concurrent = None;
        cue.filter_cutoff_hz = cutoff;
        cue
    }

    /// RMS of the second half of `frames` 60 Hz blocks of offline output.
    fn tail_rms(audio: &mut Audio, frames: usize) -> f32 {
        let mut out = Vec::new();
        for _ in 0..frames {
            out.extend(audio.render_offline(800).expect("offline render"));
        }
        let tail = &out[out.len() / 2..];
        (tail.iter().map(|s| s * s).sum::<f32>() / tail.len().max(1) as f32).sqrt()
    }

    /// Fixrev6 (review sprint-rev-claude-22-k finding 6, ADR-5977): `play_cue_on` holds the
    /// cue's low-pass track on the voice, like the flat and positional plays.
    #[test]
    fn play_cue_on_holds_the_cues_low_pass_track() {
        let mut audio = headless();
        let mut cue = single_cue("muffled");
        cue.filter_cutoff_hz = Some(400.0);
        audio.register_cue(cue);
        audio.register_cue(single_cue("open"));
        let emitter = AudioEmitter::default();
        let entity = Entity::from_raw_u32(31).expect("entity");
        let id = audio
            .play_cue_on("muffled", entity, &emitter)
            .expect("plays")
            .expect("started");
        assert!(
            audio.voices.get(id).expect("voice").filter_track.is_some(),
            "the attached voice plays on the cue's low-pass track"
        );
        let id = audio
            .play_cue_on("open", entity, &emitter)
            .expect("plays")
            .expect("started");
        assert!(
            audio.voices.get(id).expect("voice").filter_track.is_none(),
            "an unfiltered cue needs no track"
        );
    }

    /// ADR-5977: a filtered cue voice that is virtualised and revived plays on its held low-pass
    /// track again — filtered (not the bare bus or emitter track) and filtered once (no second
    /// filter stacked on the first) — for the flat, positional and attached places alike.
    #[test]
    fn a_revived_filtered_cue_voice_keeps_its_low_pass_once() {
        let entity = Entity::from_raw_u32(32).expect("entity");
        let emitter = AudioEmitter::default();
        let play = |audio: &mut Audio, place: usize| -> SoundInstanceId {
            match place {
                0 => audio.play_cue("whine"),
                1 => audio.play_cue_at("whine", Vec3::new(2.0, 0.0, 0.0), &emitter),
                _ => audio.play_cue_on("whine", entity, &emitter),
            }
            .expect("plays")
            .expect("started")
        };
        for (place, name) in ["flat", "at", "on"].into_iter().enumerate() {
            let level = |cutoff: Option<f32>, revive: bool| {
                let mut audio = headless();
                audio.register_cue(sine_cue("whine", 6_000.0, 2.0, cutoff));
                let id = play(&mut audio, place);
                let _ = tail_rms(&mut audio, 3);
                if revive {
                    audio.voices.get_mut(id).expect("voice").virtualise();
                    assert!(audio.voices.get(id).expect("voice").instance.is_none());
                    audio.revive_voice(id);
                    assert!(
                        audio.voices.get(id).expect("voice").instance.is_some(),
                        "{name}: the voice revived"
                    );
                }
                tail_rms(&mut audio, 8)
            };
            let open = level(None, true);
            let fresh = level(Some(300.0), false);
            let revived = level(Some(300.0), true);
            assert!(open > 1e-3, "{name}: control audible: {open}");
            assert!(
                revived < open * 0.2,
                "{name}: the revived voice is still low-passed: {revived} vs {open}"
            );
            assert!(
                revived > fresh * 0.25,
                "{name}: the revived voice is filtered once, not twice: {revived} vs {fresh}"
            );
        }
    }
}

#[cfg(test)]
mod seeded_cue_tests {
    use super::*;
    use crate::cue::{CueEntry, SoundCue};

    fn tone(len: usize) -> SoundAsset {
        let clip = crate::SoundClip::new(vec![0.1; len], 48_000).expect("clip");
        SoundAsset::from_clip(&clip)
    }

    fn audio_with_cues() -> Audio {
        let mut audio = Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        });
        for name in ["hit", "other"] {
            let entries = (0..5)
                .map(|i| CueEntry {
                    asset: tone(4_800 + i * 100),
                    weight: 1.0,
                })
                .collect();
            let mut cue = SoundCue::new(name, entries);
            cue.pitch_range = (0.8, 1.2);
            cue.volume_range_db = (-6.0, 0.0);
            audio.register_cue(cue);
        }
        audio
    }

    /// (clip length, pitch bits, volume bits) of the voice `id`.
    fn picked(audio: &Audio, id: SoundInstanceId) -> (usize, u32, u32) {
        let entry = audio.voices.get(id).expect("voice");
        (
            (entry.asset.duration_secs() * 48_000.0).round() as usize,
            entry.params.pitch.to_bits(),
            entry.params.volume_db.to_bits(),
        )
    }

    /// Plays both seeded APIs for a series of events and returns what each voice drew.
    fn run(names: &[&str]) -> Vec<(usize, u32, u32)> {
        let mut audio = audio_with_cues();
        let mut out = Vec::new();
        for event in 0..24u64 {
            for name in names {
                let a = audio
                    .play_cue_seeded(name, cue_seed(event))
                    .expect("cue")
                    .expect("played");
                out.push(picked(&audio, a));
                let b = audio
                    .play_cue_event(name, event, 1000 + event)
                    .expect("cue")
                    .expect("played");
                out.push(picked(&audio, b));
            }
        }
        out
    }

    fn cue_seed(event: u64) -> u64 {
        crate::cue::cue_event_seed(event, 7)
    }

    #[test]
    fn seeded_cue_apis_repeat_across_runs_and_differ_across_cue_names() {
        assert_eq!(
            run(&["hit"]),
            run(&["hit"]),
            "same seeds replay the same variation"
        );
        assert_eq!(run(&["other"]), run(&["other"]));
        assert_ne!(run(&["hit"]), run(&["other"]), "cue name is part of the stream");
    }

    /// A play that fails (here: an unknown bus) must not commit the pick: the sequential cue
    /// must offer the same entry again once the bus is fixed, not skip it (correctness audit
    /// finding 4).
    #[test]
    fn a_failed_play_does_not_advance_the_cue_selector() {
        let mut audio = audio_with_cues();
        let mut cue = SoundCue::new(
            "seq",
            vec![
                CueEntry {
                    asset: tone(4_800),
                    weight: 1.0,
                },
                CueEntry {
                    asset: tone(4_900),
                    weight: 1.0,
                },
            ],
        );
        cue.mode = crate::cue::CuePlayMode::Sequential;
        cue.bus = Some("no_such_bus".into());
        audio.register_cue(cue);
        // The pick happens before the bus lookup, and the play fails.
        assert!(audio.play_cue("seq").is_err(), "unknown bus");
        assert!(
            audio.voices.entries.is_empty(),
            "the failed play started no voice"
        );
        // Fix the bus: entry 0 must play — the failed pick was rolled back.
        if let Some(cue) = audio.cues.get_mut("seq") {
            cue.bus = Some("Master".into());
        }
        let first = audio.play_cue("seq").expect("cue").expect("played");
        assert_eq!(
            picked(&audio, first).0,
            4_800,
            "entry 0 plays first, not skipped by the failed attempt"
        );
        let second = audio.play_cue("seq").expect("cue").expect("played");
        assert_eq!(
            picked(&audio, second).0,
            4_900,
            "the successful plays advance the sequential cursor"
        );
    }

    #[test]
    fn filtered_cue_voice_keeps_its_track_alive() {
        let mut audio = audio_with_cues();
        let mut cue = SoundCue::new(
            "dull",
            vec![CueEntry {
                asset: tone(4_800),
                weight: 1.0,
            }],
        );
        cue.filter_cutoff_hz = Some(1_000.0);
        audio.register_cue(cue);
        let id = audio.play_cue("dull").expect("cue").expect("played");
        let entry = audio.voices.get(id).expect("voice");
        assert!(entry.filter_track.is_some(), "the voice owns the filter track");
        let plain = audio.play_cue("hit").expect("cue").expect("played");
        assert!(audio.voices.get(plain).expect("voice").filter_track.is_none());
    }

    #[test]
    fn filtered_cue_on_an_unknown_bus_is_an_error_and_plays_nothing() {
        let mut audio = audio_with_cues();
        let entry = || CueEntry {
            asset: tone(4_800),
            weight: 1.0,
        };
        let mut missing = SoundCue::new("dull_missing", vec![entry()]);
        missing.filter_cutoff_hz = Some(1_000.0);
        missing.bus = Some("Missing".into());
        audio.register_cue(missing);
        let mut master = SoundCue::new("dull_master", vec![entry()]);
        master.filter_cutoff_hz = Some(1_000.0);
        master.bus = Some("Master".into());
        audio.register_cue(master);

        let before = audio.played_count();
        assert!(matches!(
            audio.play_cue("dull_missing"),
            Err(AudioError::UnknownBus(bus)) if bus == "Missing"
        ));
        assert_eq!(audio.played_count(), before, "a refused play is not counted");
        assert_eq!(audio.real_voice_count(), 0, "no voice was registered");

        assert!(matches!(audio.play_cue("dull_master"), Ok(Some(_))));
        assert_eq!(audio.played_count(), before + 1);
    }
}

#[path = "audio_propagation.rs"]
mod propagation_impl;
pub use propagation_impl::{GroundHaptics, GroundRumble, HAPTIC_GAIN, HAPTIC_THRESHOLD};

/// The streaming underrun watch (ADR-872).
mod streams;

mod mixdown;
pub(crate) use mixdown::StemTap;
pub use music::send_music_beats;

/// A video player's PCM feed on a bus (ADR-1670).
mod media;
// The music calls on the facade (ADR-2517).
mod music;

mod convolution;
pub use convolution::{ConvolutionReport, ImpulseResponseInfo};

mod stretch;
pub use stretch::TimeStretchVoiceInfo;

mod ambience_files;
pub use ambience_files::AmbienceFileInfo;

// Low-pass voices for occluded sequence sections (ADR-3197).
mod lowpass_voice;

// One-shot and granular patch layers, the patch state for tools (`.audiopatch.ron` v2, ADR-7226).
mod patch_layers;

// The gameplay pause of the mixer (ADR-10105).
mod game_pause;
pub use game_pause::{DEFAULT_PAUSE_EXEMPT_BUSES, GAME_PAUSE_FADE};
pub use mixdown::{MAX_MIXDOWN_STEMS, MIXDOWN_SAMPLE_RATES, MixdownBlock, MixdownSettings};
pub use patch_layers::{PatchInfo, PatchLayerInfo, PatchParamInfo};
