// SPDX-License-Identifier: MPL-2.0
//! Audio: buses, snapshots, ducking, cues, spatialisation, virtualisation, streaming music with
//! crossfades and stems, parametric patches, announcers and voice capture
//! (`docs/specs/16-audio.md`).
//!
//! The [`Audio`] resource is the facade; [`AudioPlugin`] wires it into the frame loop. Two output
//! backends exist: the default device through kira (cpal), and an offline renderer used for
//! headless runs, CI and deterministic tests — the whole mixer graph can be rendered frame by
//! frame and asserted on.
//!
//! # How the pieces fit
//!
//! Sounds ([`SoundClip`], [`SoundAsset`]) are played either directly through [`Audio::play`] or
//! through randomised containers ([`SoundCue`]) and parametric generators ([`AudioPatch`]).
//! Everything routes into named [`Buses`] with per-bus effects and side-chain [`DuckEngine`]
//! ducking; [`AudioSnapshot`]/[`SnapshotStack`] fade whole bus states, [`MusicSystem`] streams
//! tracks with crossfades and stems, and [`AudioCapture`] captures/plays voice. Spatial placement
//! ([`AudioEmitter`]/[`AudioListener`], from each entity's `GlobalTransform`) applies distance
//! attenuation, panning, cones and Doppler ([`spatialize`]); emitters with a sound in
//! [`SoundAssets`] play on spawn and stop on despawn, physics occlusion muffles them
//! ([`AudioOcclusion`]), [`AudioReverbZone`]s make rooms reverberate, and headphone listeners
//! can opt into binaural rendering ([`hrtf`]). Sound travels through the level's [`AudioMedium`]
//! (Earth air, Mars CO₂, or none) and, for emitters with [`AudioPropagation::ground_coupling`],
//! through the ground to a listener whose [`ListenerCoupling`] touches it ([`propagate`]).
//! Gameplay reads the [`Audio`] resource; the drain system hands queued one-shots to the backend
//! each frame.
//!
//! See `book/user-guide/audio.md` (buses, cues, patches, spatial audio).
//!
//! # Example
//!
//! ```
//! # use regia_app::App;
//! # use regia_audio::{AudioPlugin, AudioSettings, SoundClip};
//! let mut app = App::new();
//! app.insert_resource(AudioSettings { headless: true, ..Default::default() });
//! app.add_plugins(AudioPlugin);
//! let clip = SoundClip::new(vec![0.25; 16], 48_000).expect("a valid clip");
//! # let _ = (app, clip);
//! ```

mod announcer;
mod audio;
mod audio_zone;
mod backend;
mod bus;
pub mod bus_meter;
mod clip;
mod convolution;
mod coupling;
mod cue;
mod cues;
mod ducking;
mod error;
// Sound over the cooked world geometry's portals, rock and section reverb (W-25, ADR-8416).
mod geom_sound;
mod ground;
pub mod hrtf;
mod instance;
mod limiter;
mod loopgen;
mod loudness;
mod medium;
mod music;
mod occlusion;
mod patch;
mod path;
mod plugin;
mod propagation;
mod propagation_sync;
mod render_stats;
mod reverb_zone;
mod settings;
mod snapshot;
mod sound;
mod sounds;
mod spatial;
mod timestretch;
mod voice;
pub mod voice_chat;
mod voices;
// The video player's PCM feed and audio clock (specs/50 § 3.3, ADR-1670).
pub mod media_feed;
// Loudness at import: the per-sound record, the policy and `loudness.ron` (ADR-1855).
mod loudness_table;
// Ambience that matches the picture: wind/weather bed, point spots, surface footsteps (ADR-2442).
mod ambience;
// Vehicle audio: engine layers, tires, wind, shifts and the voice budget (ADR-3271).
mod vehicle_audio;
// Cue points stored in a WAV file (RIFF `cue ` and `labl`), ADR-4632.
pub mod wav_cues;
// The cooked sound: source bytes plus loudness record, one pak entry (ADR-2788).
pub mod cooked_sound;
// Whole-file decoding with a size and duration cap (threat-model finding 30, ADR-4922).
mod decode;
// Granular synthesis for the granular patch layer (`.audiopatch.ron` v2, ADR-7226).
pub mod granular;
// Patch parameters bound to reflected fields (`.audiopatch.ron` v2, ADR-7226).
mod patch_binding;
// The live program-loudness meter on the master output (audio-loudness § 6, ADR-7227).
pub mod program_loudness;

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) mod test_support;

pub use ambience::{
    AMBIENCE_VERSION, AmbienceCall, AmbienceDef, AmbienceError, AmbienceOneShot, AmbiencePlayer,
    MAX_AMBIENCE_DISTANCE_M, MAX_AMBIENCE_INTERVAL_SECS, MIN_AMBIENCE_INTERVAL_SECS,
    ambience_migration_sources, ambience_schema_fingerprint,
};
pub use ambience::{
    AmbienceBed, AmbienceInputs, AmbienceSpotState, AmbientCall, AmbientSpot, AmbientSpotKind, BedDrive,
    BedLayer, BedLayerState, FOOTSTEP_LOG_LEN, FootstepBank, FootstepEvent, FootstepPlay, FootstepSet,
    SpotHearing, SpotSchedule, TEST_TONE_PREFIX, ambience_bed_sync, ambience_file_sync, ambient_spot_sync,
    call_position, foliage_flutter, next_interval, pick_variant, spot_hearing, stop_bed_voices,
    test_tone_loopgens,
};
pub use announcer::{AnnounceOutcome, AnnouncePriority, Announcer, AnnouncerBank};
pub use audio::BusStatus;
pub use audio::{AmbienceFileInfo, ConvolutionReport, ImpulseResponseInfo, TimeStretchVoiceInfo};
pub use audio::{
    Audio, EmitterStatus, EmitterUpdate, GroundHaptics, GroundRumble, HAPTIC_GAIN, HAPTIC_THRESHOLD,
    send_music_beats,
};
pub use audio::{MAX_MIXDOWN_STEMS, MIXDOWN_SAMPLE_RATES, MixdownBlock, MixdownSettings};
pub use audio::{PatchInfo, PatchLayerInfo, PatchParamInfo};
// The gameplay pause of the mixer (ADR-10105).
pub use audio::{DEFAULT_PAUSE_EXEMPT_BUSES, GAME_PAUSE_FADE};
pub use audio_zone::{
    ArmedZone, AudioRockField, AudioZonePortals, FieldRoute, FnRockField, PlacedZone, PortalRoute, RockField,
    RockTuning, ZoneField, ZoneHearing, ZonePortal, rock_thickness, scaled_range,
};
pub use backend::{OfflineBackend, OfflineSettings, PlayedOneShot, SampleClock};
pub use bus::SendTap;
pub use bus::{
    AUDIO_BUSES_FORMAT_VERSION, BUS_EFFECT_KINDS, audio_buses_migration_sources,
    bus_layout_schema_fingerprint, migrate_audio_buses_ron,
};
pub use bus::{
    BusDef, BusEffect, BusFilterHandle, BusLayout, BusSend, Buses, NoHost, REVERB_BUS, SILENT_SEND_DB,
    SendBusDef, TrackHost,
};
pub use bus::{MAX_BUS_DB, MUTE_DB, ZONE_IR_BUS};
pub use clip::{SoundClip, SoundClipError};
pub use convolution::{
    CONVOLUTION_PARTITION, ConvolutionEffectBuilder, ConvolutionError, ConvolutionHandle, ConvolutionKernel,
    ConvolutionSlot, ConvolutionStats, ConvolutionTier, Convolver, DEFAULT_IR_CROSSFADE_SECS,
    ImpulseResponse, MAX_IR_CROSSFADE_SECS, MAX_PREDELAY_SECS, direct_convolution, resample_sinc,
};
pub use coupling::{
    AudioInteriorVolume, AudioPropagation, COUPLING_FADE_SECS, CouplingFilter, EmitterCategory,
    INTERIOR_RADIATION, InteriorFrame, ListenerContact, ListenerCoupling, PathClass, WALL_TRANSMISSION,
};
pub use cue::{
    CUE_VERSION, CueCaptionDef, CueCaptionKind, CueCaptionPlay, CueEntry, CuePlayMode, CueSelector,
    CueSoundDef, SoundCue, SoundCueDef, StealPolicy, cue_event_seed, cue_migration_sources,
    cue_schema_fingerprint,
};
pub use cues::CueAssets;
pub use decode::{MAX_DECODED_SOUND_BYTES, MAX_DECODED_SOUND_SECS, MAX_SOUND_SAMPLE_RATE, SoundDecodeLimits};
pub use decode::{SoundHeader, probe_sound_header};
pub use ducking::{DuckEngine, DuckRule, default_duck_rules};
pub use error::AudioError;
pub use geom_sound::GeomSoundField;
pub use ground::{
    CODA_REFERENCE_HZ, GroundArrival, GroundMaterial, HAPTIC_HZ, MAX_CODA_SECS, MIN_CODA_SECS, Spreading,
    ground_arrival, interface_transmission,
};
pub use instance::DECLICK_FADE;
pub use instance::SoundInstance;
pub use limiter::{MasterLimiter, OutputMeter};
pub use loopgen::{
    FilterMode, LOOPGEN_SUFFIX, LOOPGEN_VERSION, LoopGenCache, LoopGenDef, LoopGenError, LoopGenInfo,
    LoopMixInput, LoopNode, LoopNodeDef, LoopStats, MAX_LENGTH_SECS, MAX_NODES, NoiseColor, OnePoleMode,
    OscShape, RenderedLoop, is_loopgen_path, loopgen_migration_sources, loopgen_schema_fingerprint,
};
pub use loudness::{
    ABSOLUTE_GATE_LUFS, GATING_BLOCK_SECS, GATING_HOP_SECS, LOUDNESS_OFFSET_DB, Loudness, LoudnessError,
    LoudnessMeter, MAX_NORMALIZE_GAIN_DB, MOMENTARY_SECS, REFERENCE_LUFS, RELATIVE_GATE_LU, SHORT_TERM_SECS,
    measure_loudness, normalize_gain, true_peak_oversampling,
};
pub use loudness::{FIVE_ONE_WEIGHTS, channel_weights};
pub use loudness_table::measure_sound_bytes_with_limits;
pub use loudness_table::{
    DEFAULT_TRUE_PEAK_CEILING_DB, DUAL_MONO_OFFSET_LU, LOUDNESS_TABLE_FILE, LOUDNESS_TABLE_VERSION,
    LoudnessPolicy, LoudnessTable, LoudnessTableError, SoundLoudness, loudness_table_migration_sources,
    loudness_table_schema_fingerprint, measure_sound, measure_sound_bytes,
};
pub use medium::{
    AudioMedium, EARTH_ABSORPTION_DB_PER_M_KHZ2, MARS_ABSORPTION_DB_PER_M_KHZ2, MARS_LEVEL_OFFSET_DB,
    MARS_SPEED_OF_SOUND, MAX_CUTOFF_HZ, MIN_CUTOFF_HZ, MediumKind, absorption_cutoff_hz, doppler_ratio_in,
    doppler_ratio_moving_in,
};
pub use music::{
    BarGate, BeatGrid, MUSIC_ANY_TRACK, MUSIC_SCHEDULE_LEAD_SECS, MUSIC_VERSION, MusicAsset, MusicBeat,
    MusicDef, MusicPending, MusicSchedule, MusicSegment, MusicSegmentDef, MusicState, MusicStem,
    MusicStinger, MusicStingerDef, MusicSync, MusicSystem, MusicTransitionDef, MusicTransitionRequest,
    MusicTransitionRule, StemDef, music_migration_sources, music_schema_fingerprint,
};
pub use occlusion::{AudioOcclusion, OcclusionRay, OcclusionTuning};
pub use patch::{
    AudioPatch, AudioPatchDef, MAX_FIRES_PER_TICK, PATCH_VERSION, PatchBindingDef, PatchCurveDef,
    PatchGranularDef, PatchKeyDef, PatchLayerDef, PatchLayerKind, PatchOneShotDef, PatchParamDef,
    PatchTriggerDef, SmoothedParam, builtin_migrations, granular_controls, patch_migration_sources,
    patch_schema_fingerprint,
};
pub use patch_binding::{AudioPatchSource, PatchBindingState, patch_binding_sync, reflected_f32};
pub use plugin::{
    AudioPlugin, AudioSyncState, AudioThrottle, audio_lifecycle, audio_sync, drain_one_shots, placed_zones,
};
pub use plugin::{AudioResumeRetry, RESUME_RETRY_ATTEMPTS, RESUME_RETRY_FRAMES, audio_resume_retry};
pub use program_loudness::ProgramLoudness;
pub use propagation::{
    AUDIBLE_GAIN, GROUND_PAN_SCALE, PathFrame, Propagation, PropagationContext, propagate,
    step_coupling_weight,
};
pub use propagation_sync::{audio_propagation_sync, ground_haptics};
pub use render_stats::{CLIP_LEVEL, RenderStats};
pub use reverb_zone::{
    AudioReverbZone, PresetParams, ReverbMix, ReverbPreset, ReverbZoneShape, reverb_mix, zone_ir,
};
pub use settings::{AudioSettings, AudioUserSettings, SETTINGS_BUSES, default_voice_limit};
pub use snapshot::{AudioSnapshot, SnapshotSend, SnapshotStack};
pub use sound::{LoopRegion, PlaybackParams, SoundAsset, SoundSource, StreamStats, rejected_value_count};
pub use sounds::{SoundAssets, reload_sound_files};
pub use spatial::{
    AudioActiveCamera, AudioCameraPose, AudioEmitter, AudioListener, AudioListenerFollow, EmitterFrame,
    ListenerFrame, MotionTracker, RolloffCurve, SPEED_OF_SOUND, SpatialFrame, absorption_cutoff, cone_gain,
    db_to_gain, direction_pan, distance_gain, distance_gain_faded, doppler_ratio, doppler_ratio_moving,
    gain_to_db, nearest_listener, spatialize,
};
pub use timestretch::{
    MAX_STRETCH_ASSET_FRAMES, MAX_STRETCH_RATE, MIN_STRETCH_RATE, TimeStretchError, TimeStretchHandle,
    TimeStretchSoundData, WSOLA_HOP, WSOLA_TOLERANCE, WSOLA_WINDOW, check_rate, stretch_asset, time_stretch,
};
pub use vehicle_audio::{
    BumpSoundDef, EmitterPlacementDef, EngineLayerDef, EngineSoundDef, LIMITER_SMOOTHING_SECS,
    MAX_AUDIO_WHEELS, MAX_ENGINE_LAYERS, ONE_SHOT_CAPACITY, PLACEHOLDER_PREFIX, REDUCED_SWITCH_RATIO,
    RevLimiterDef, RollingSoundDef, ShiftSoundDef, SmoothingDef, SquealSoundDef, TireSoundDef,
    VEHICLE_AUDIO_SUFFIX, VEHICLE_AUDIO_VERSION, VOICE_START_AMPLITUDE, VOICE_STOP_AMPLITUDE,
    VOICE_STOP_FADE, VehicleAudioDef, VehicleAudioError, VehicleAudioInputs, VehicleAudioModel,
    VehicleAudioParams, VehicleEmitterEntities, VehicleEmitterSlot, VehicleOneShot, VehicleOneShotKind,
    VehicleSoundBank, VehicleVoiceMode, VehicleVoiceOut, VehicleVoiceRole, VehicleVoiceSet, WheelAudioInput,
    WhineSoundDef, WindSoundDef, engine_crossfade, load_target, one_pole_coefficient, placeholder_sound,
    squeal_amount, vehicle_audio_migration_sources, vehicle_audio_schema_fingerprint,
};
pub use voice::{
    AudioCapture, VadSettings, VoiceActivity, VoiceCaptureSettings, VoiceChunk, VoiceStreamAnchor,
    VoiceStreamData, VoiceStreamHandle, VoiceStreamSnapshot, VoiceStreamStats, stream_placement,
};
pub use voices::{SoundInstanceId, VoiceEntry, VoiceKind, VoiceManager, voice_priority};

// Pure patch evaluation for hosts that inspect the same laws as playback (ADR-8052).
pub use patch::{LayerOutputs, layer_outputs};
pub use spatial::polyline;

/// Common imports for code that plays audio.
pub mod prelude {
    pub use crate::{
        AnnounceOutcome, AnnouncePriority, AnnouncerBank, Audio, AudioCapture, AudioEmitter,
        AudioInteriorVolume, AudioListener, AudioListenerFollow, AudioMedium, AudioOcclusion, AudioPatch,
        AudioPatchDef, AudioPlugin, AudioPropagation, AudioReverbZone, AudioSettings, AudioSnapshot,
        CueEntry, DuckRule, EmitterCategory, EmitterFrame, ListenerContact, ListenerCoupling, ListenerFrame,
        LoopRegion, MusicAsset, MusicBeat, MusicDef, MusicStem, MusicSync, PatchCurveDef, PatchKeyDef,
        PatchLayerDef, PatchParamDef, PlaybackParams, ReverbZoneShape, RolloffCurve, SoundAsset, SoundAssets,
        SoundClip, SoundCue, SoundCueDef, SoundInstanceId, SpatialFrame, VadSettings, VoiceCaptureSettings,
        drain_one_shots, spatialize,
    };
    pub use regia_core::math::Vec3;
}

// Contact sounds: impacts, scrapes, rolls and breaks from physics material pairs (ADR-3270).
pub mod impacts;
pub use impacts::{
    ContactLoop, IMPACT_BANK_FORMAT_VERSION, ImpactBank, ImpactBankDef, ImpactBankError, ImpactBudget,
    ImpactContact, ImpactCurves, ImpactPair, ImpactPlay, ImpactSet, ImpactSource, LoopLevel, SetKey,
};

// Output stall watchdog: a device output whose sample clock stopped is reopened (ADR-8830).
pub mod stall;
pub use stall::{AudioStallWatchdog, StallAction, StallTarget, audio_stall_watchdog};
