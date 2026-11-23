// SPDX-License-Identifier: MPL-2.0
//! Audio tracks (`.seq.ron` v2, ADR-1880): one `regia_audio` voice per sound section the
//! player's clock is inside.
//!
//! Each frame, after the tracks apply, every playing [`SequencePlayer`]'s sound sections are
//! compared with the voices it runs ([`SequenceVoices`], keyed by the section's path in the
//! tree): a section the clock entered starts a voice at the right offset into the sound (so a
//! start mid-section, a seek or a render-out pre-roll lands where the clock is); a section the
//! clock left stops its voice; the fade gain follows the section's fades. A jump of the clock (a
//! seek, a restart, a loop wrap, an external clock moving far from the frame's step) restarts
//! every voice at its new offset, and an edit that changes a running section's sound, bus or
//! timing restarts that voice. A player that pauses, stops, finishes, despawns or holds its audio
//! ([`SequencePlayer::hold_audio`]) stops its voices, and so does a paused game clock (they come
//! back at the clock's offset). A voice runs on the mixer's clock: the game's time scale does not
//! bend it. Without an `Audio` resource nothing plays.
//!
//! **Time scale (v3, ADR-2405).** A voice's time scale is its section's
//! [`time_scale`](super::AudioSection::time_scale) times the speeds of the sub-sequence sections
//! above it ([`super::SoundSample::rate`]). Voices follow it by varispeed, like tape: at a time
//! scale of 2 the sound plays twice as fast and an octave higher, and at 0.5 half as fast and an
//! octave lower (the voice's pitch ratio is the time scale). A v5 section may keep its pitch
//! instead ([`TimeScaleMode::PreservePitch`](super::TimeScaleMode), below).
//!
//! **Spatial sections (v3, ADR-2405).** A section with [`spatial`](super::AudioSection::spatial)
//! plays from an entity (a binding the player resolves, followed every frame through its
//! `GlobalTransform`) or a fixed position, with its own attenuation. Its gain and pan come from
//! `regia_audio`'s emitter spatialization ([`regia_audio::spatialize`] with an
//! [`AudioEmitter`] built by [`spatial_emitter`]) against the nearest listener the audio sync
//! placed, every frame; see [`spatial_frame`]. The voice itself is a plain voice on the section's
//! bus. The sync keeps its v2 place in the schedule (after the tracks apply), so it reads the
//! bound entity's latest propagated `GlobalTransform`: a moving emitter can trail by one frame.
//!
//! **Doppler (v5, ADR-3197).** With [`doppler`](super::AudioSpatial::doppler) above 0 the
//! spatialization gets the emitter's and the listener's velocity, and the voice's pitch ratio is
//! the time scale times the Doppler ratio (clamped to [`super::DOPPLER_RATIO_RANGE`]). The
//! emitter's velocity is the section's sampled motion on the sequence clock: for a binding with
//! a `transform.position` track, the track's slope (a central difference of the sequence sampled
//! [`MOTION_PROBE_SECS`] either side of the clock, through the bound entity's parent transform);
//! for a binding moved by something else, its world position's change over the frame's step; a
//! fixed position does not move. The listener's velocity is its position's change over the
//! step. A clock jump (a seek, a loop wrap, a restart) restarts the voice with no velocity, and a
//! listener that moves faster than [`LISTENER_CUT_SPEED`] in one step (a camera cut) counts as
//! still, so neither throws the pitch. Under a fixed step (a render-out) every number is a pure
//! function of the clock.
//!
//! **Occlusion (v5, ADR-3197).** With [`occlusion`](super::AudioSpatial::occlusion) the section
//! asks `regia_audio`'s occlusion query: its rays go on [`AudioOcclusion`] under the owner
//! `"sequencer"` (keyed by the bound entity, or a proxy entity for a fixed position), the provider
//! answers them like the emitters', and the voice plays on a low-pass voice
//! ([`Audio::start_asset_lowpass`]) whose cutoff and gain follow the occlusion tuning, smoothed
//! over the frame's step (the render clock in a render-out). A clock jump starts from the
//! reported amount. Propagation paths and binaural rendering do not apply to sequence voices.
//!
//! **Pitch-preserving sections (v5, ADR-3197).** A section with
//! [`TimeScaleMode::PreservePitch`](super::TimeScaleMode) plays its whole range of the sound
//! stretched by `regia_audio`'s WSOLA ([`regia_audio::stretch_asset`]) at the voice rate,
//! rendered ahead into [`SequenceStretches`] (a byte-bounded cache) and played as a plain voice
//! at pitch 1 (times Doppler) from `(position - offset) / rate` into the stretch. A seek therefore
//! lands on exactly the samples playing to that point gave, and a render-out reads it like any
//! decoded sound. Ranges up to [`STRETCH_INLINE_SECS`] of sound render on the frame that needs
//! them; longer ones render on the async compute pool, from [`STRETCH_LOOKAHEAD_SECS`] before the
//! clock reaches them, and the voice starts (at the clock's offset) when its stretch is ready. A
//! running render-out renders every stretch on the frame, so its mix does not depend on worker
//! timing. A voice rate of exactly 1 plays the sound itself (bit for bit the varispeed voice);
//! a rate outside [`super::PRESERVE_PITCH_RANGE`] plays by varispeed and is reported
//! (`apply_failed`, reason `stretch_rate: ...`).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use regia_app::prelude::*;
use regia_audio::{
    Audio, AudioEmitter, AudioOcclusion, DECLICK_FADE, EmitterFrame, ListenerFrame, OcclusionRay,
    PlaybackParams, RolloffCurve, SoundAsset, SoundAssets, SoundInstanceId, SpatialFrame, gain_to_db,
    nearest_listener, spatialize, stretch_asset,
};
use regia_core::math::{Quat, Vec3, det_log10};
use regia_scene::{GlobalTransform, Transform};

use super::asset::{
    AudioSpatial, DOPPLER_RATIO_RANGE, PRESERVE_PITCH_RANGE, SoundRolloff, SoundSource, TimeScaleMode,
};
use super::sample::ChannelValue;

use super::player::{ClockSource, PlaybackStatus, SequenceIssue, SequencePlayer, SequencerSet};

/// Sequence seconds either side of the clock a bound emitter's position track is sampled at for
/// its Doppler velocity (a central difference; one-sided at the ends of the sequence).
pub const MOTION_PROBE_SECS: f64 = 0.005;

/// A listener that moves faster than this (m/s) over one step jumped (a camera cut): Doppler
/// counts it as still on that step.
pub const LISTENER_CUT_SPEED: f32 = 120.0;

/// Most bytes of stretched audio [`SequenceStretches`] keeps (8 bytes a stereo frame: about 11
/// minutes at 48 kHz); the least recently used stretch goes first past it.
pub const STRETCH_CACHE_BYTES: usize = 256 << 20;

/// Sequence seconds ahead of the clock the player looks for pitch-preserving sections to render.
pub const STRETCH_LOOKAHEAD_SECS: f64 = 3.0;

/// A pitch-preserving section whose range of sound is at most this long (seconds) renders on the
/// frame that needs it; a longer one renders on the async compute pool. One WSOLA pass costs about
/// 70 ms of one core per second of output on a loaded build host (ADR-3197), so this keeps an
/// on-frame render under about 18 ms.
pub const STRETCH_INLINE_SECS: f64 = 0.25;

/// The occlusion owner sequence voices queue their rays under ([`AudioOcclusion::set_extra_rays_for`]).
pub const OCCLUSION_OWNER: &str = "sequencer";

/// How far an external clock may stray from the frame's step, on top of one step, before the
/// voices count as out of sync.
const JUMP_TOLERANCE: Duration = Duration::from_millis(50);

/// How far (seconds) a section's mapping from the clock to the sound may move before its voice
/// no longer plays what the section says.
const ANCHOR_TOLERANCE: f64 = 1.0e-3;

/// One running voice of a sound section.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SequenceVoice {
    /// The `regia_audio` voice.
    pub id: SoundInstanceId,
    /// The volume it was last set to, dB (section volume plus the fade gain, plus the spatial
    /// gain in dB for a spatial section, plus the occlusion gain for an occluding one).
    pub volume_db: f32,
    /// The stereo pan it was last set to, `-1..=1` (0 for a flat section, v3).
    pub pan: f32,
    /// The spatial gain of the last frame, `0..=1` (1 for a flat section, v3).
    pub spatial_gain: f32,
    /// Where a spatial section emitted from on the last frame, world metres (`None` for a flat
    /// section, v3).
    pub position: Option<[f32; 3]>,
    /// The pitch ratio it was last set to: the voice rate times Doppler for a varispeed voice,
    /// Doppler alone for a stretched one (v5).
    pub pitch: f32,
    /// The Doppler ratio of the last frame (1 when Doppler is off or the section is flat, v5).
    pub doppler: f32,
    /// The emitter velocity Doppler used on the last frame, m/s (`None` without Doppler, v5).
    pub velocity: Option<[f32; 3]>,
    /// The smoothed occlusion amount, `0..=1` (0 when the section does not occlude, v5).
    pub occlusion: f32,
    /// The low-pass cutoff it was last set to, Hz (`None` when the section does not occlude, v5).
    pub cutoff_hz: Option<f32>,
    /// Plays a pitch-preserving stretch rendered ahead (v5).
    pub stretched: bool,
}

/// What a voice was started from: an edit that changes any of these restarts it.
#[derive(Clone, Debug, PartialEq)]
struct VoiceKey {
    sound: String,
    bus: Option<String>,
    rate: f64,
    spatial: bool,
    stretched: bool,
    occluding: bool,
}

/// A running voice and what it was started from.
#[derive(Debug)]
struct RunningVoice {
    voice: SequenceVoice,
    key: VoiceKey,
    /// The sound position at clock zero: constant while the section keeps its timing.
    anchor: f64,
    /// The emitter's world position on the last frame (the moved-by-something-else velocity).
    last_position: Option<Vec3>,
}

#[derive(Debug, Default)]
struct PlayerVoices {
    jumps: u64,
    clock_ns: u64,
    voices: BTreeMap<Vec<usize>, RunningVoice>,
    /// Occlusion keys of the fixed-position sections in effect, by section path.
    proxies: BTreeMap<Vec<usize>, Entity>,
}

/// The voices sequence players run, per player entity and section path
/// ([`super::SoundSample::path`]). A resource the [`super::SequencerPlugin`] keeps.
#[derive(Resource, Debug, Default)]
pub struct SequenceVoices {
    players: BTreeMap<Entity, PlayerVoices>,
    started: u64,
    stopped: u64,
    /// The listeners' positions on the last frame (their Doppler velocity).
    listener_positions: Vec<Vec3>,
    /// Proxy entities whose sections ended, for the sync to despawn.
    retired: Vec<Entity>,
}

impl SequenceVoices {
    /// The voices `player` runs, by section path.
    #[must_use]
    pub fn voices(&self, player: Entity) -> Vec<(Vec<usize>, SequenceVoice)> {
        self.players
            .get(&player)
            .map(|p| p.voices.iter().map(|(k, v)| (k.clone(), v.voice)).collect())
            .unwrap_or_default()
    }

    /// Voices started so far.
    #[must_use]
    pub fn started(&self) -> u64 {
        self.started
    }

    /// Voices stopped so far.
    #[must_use]
    pub fn stopped(&self) -> u64 {
        self.stopped
    }

    /// `true` when no player runs a voice.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.players.values().all(|p| p.voices.is_empty())
    }

    /// The entity `player`'s occluding section at `path` asks the occlusion query about: the
    /// bound entity of a binding source, a proxy entity for a fixed position (v5). `None` when
    /// the section is not in effect or does not occlude.
    #[must_use]
    pub fn occlusion_key(&self, player: Entity, path: &[usize]) -> Option<Entity> {
        self.players.get(&player)?.proxies.get(path).copied()
    }

    fn stop_all(&mut self, audio: &mut Audio, player: Entity) {
        if let Some(entry) = self.players.get_mut(&player) {
            for voice in std::mem::take(&mut entry.voices).into_values() {
                audio.stop_voice(voice.voice.id, DECLICK_FADE);
                self.stopped += 1;
            }
        }
    }

    fn forget(&mut self, player: Entity) {
        if let Some(entry) = self.players.remove(&player) {
            self.retired.extend(entry.proxies.into_values());
        }
    }
}

/// Identifies one stretch: the sound, the range of it and the rate.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct StretchKey {
    sound: String,
    offset_bits: u64,
    source_bits: u64,
    rate_bits: u64,
}

impl StretchKey {
    fn new(sound: &str, offset: f64, source_secs: f64, rate: f64) -> Self {
        Self {
            sound: sound.to_string(),
            offset_bits: offset.to_bits(),
            source_bits: source_secs.to_bits(),
            rate_bits: rate.to_bits(),
        }
    }
}

struct CachedStretch {
    asset: SoundAsset,
    bytes: usize,
    used: u64,
}

type StretchTask = regia_core::tasks::Task<Result<SoundAsset, String>>;

/// Where a stretch stands.
enum StretchState {
    Ready(SoundAsset),
    Pending,
    Failed(String),
}

/// Counters of [`SequenceStretches`] (`sequencer.spatial_voices` reports them).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct StretchStats {
    /// Stretches held.
    pub entries: usize,
    /// Bytes they hold (8 a stereo frame).
    pub bytes: usize,
    /// Stretches rendering on the async compute pool.
    pub pending: usize,
    /// Stretches rendered so far (on the frame or on the pool).
    pub rendered: u64,
    /// Of those, rendered on the frame that needed them.
    pub inline: u64,
    /// Stretches dropped to stay under [`STRETCH_CACHE_BYTES`].
    pub evicted: u64,
    /// Stretches that failed (each reported once).
    pub failed: u64,
}

/// The cache of pitch-preserving section stretches (v5, ADR-3197): rendered ahead, played as
/// plain voices, bounded by [`STRETCH_CACHE_BYTES`] (least recently used out first). A resource
/// the [`super::SequencerPlugin`] keeps.
#[derive(Resource, Default)]
pub struct SequenceStretches {
    ready: BTreeMap<StretchKey, CachedStretch>,
    pending: BTreeMap<StretchKey, StretchTask>,
    failures: BTreeMap<StretchKey, String>,
    bytes: usize,
    clock: u64,
    stats: StretchStats,
}

impl std::fmt::Debug for SequenceStretches {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SequenceStretches")
            .field("stats", &self.stats())
            .finish()
    }
}

impl SequenceStretches {
    /// The cache's counters now.
    #[must_use]
    pub fn stats(&self) -> StretchStats {
        StretchStats {
            entries: self.ready.len(),
            bytes: self.bytes,
            pending: self.pending.len(),
            ..self.stats
        }
    }

    /// Drops every stretch (a running render is abandoned).
    pub fn clear(&mut self) {
        self.ready.clear();
        self.pending.clear();
        self.failures.clear();
        self.bytes = 0;
    }

    /// Collects the stretches the pool finished.
    fn poll(&mut self) {
        let done: Vec<StretchKey> = self
            .pending
            .iter()
            .filter(|(_, task)| task.is_finished())
            .map(|(key, _)| key.clone())
            .collect();
        for key in done {
            if let Some(task) = self.pending.remove(&key) {
                let result = regia_core::tasks::block_on(task);
                self.finish(key, result);
            }
        }
    }

    fn finish(&mut self, key: StretchKey, result: Result<SoundAsset, String>) {
        match result {
            Ok(asset) => {
                self.stats.rendered += 1;
                let bytes = (asset.duration_secs() * f64::from(asset.sample_rate())) as usize * 8;
                self.clock += 1;
                self.bytes += bytes;
                self.ready.insert(
                    key,
                    CachedStretch {
                        asset,
                        bytes,
                        used: self.clock,
                    },
                );
                self.evict();
            }
            Err(reason) => {
                self.stats.failed += 1;
                self.failures.insert(key, reason);
            }
        }
    }

    /// Drops the least recently used stretches until the cache fits (the newest always stays).
    fn evict(&mut self) {
        while self.bytes > STRETCH_CACHE_BYTES && self.ready.len() > 1 {
            let Some(oldest) = self
                .ready
                .iter()
                .min_by_key(|(_, c)| c.used)
                .map(|(key, _)| key.clone())
            else {
                return;
            };
            if let Some(gone) = self.ready.remove(&oldest) {
                self.bytes -= gone.bytes;
                self.stats.evicted += 1;
            }
        }
    }

    /// The stretch of `key`, starting its render when it is not there: on the frame when `inline`
    /// (or the range is short), else on the async compute pool.
    fn get(&mut self, key: &StretchKey, asset: &SoundAsset, inline: bool) -> StretchState {
        self.clock += 1;
        if let Some(cached) = self.ready.get_mut(key) {
            cached.used = self.clock;
            return StretchState::Ready(cached.asset.clone());
        }
        if let Some(reason) = self.failures.get(key) {
            return StretchState::Failed(reason.clone());
        }
        let offset = f64::from_bits(key.offset_bits);
        let source = f64::from_bits(key.source_bits);
        let rate = f64::from_bits(key.rate_bits) as f32;
        let short = source <= STRETCH_INLINE_SECS;
        if inline || short {
            // A render-out never waits on a worker: an unfinished pool render is dropped.
            self.pending.remove(key);
            let result = stretch_asset(asset, offset, source, rate).map_err(|e| e.to_string());
            self.stats.inline += 1;
            self.finish(key.clone(), result);
            return match self.ready.get(key) {
                Some(cached) => StretchState::Ready(cached.asset.clone()),
                None => StretchState::Failed(self.failures.get(key).cloned().unwrap_or_default()),
            };
        }
        if !self.pending.contains_key(key) {
            let asset = asset.clone();
            let task = regia_core::tasks::spawn(async move {
                stretch_asset(&asset, offset, source, rate).map_err(|e| e.to_string())
            });
            self.pending.insert(key.clone(), task);
        }
        StretchState::Pending
    }
}

/// `true` when the clock moved other than by this frame's step.
fn clock_jumped(from_ns: u64, to_ns: u64, step: Duration) -> bool {
    let moved = i128::from(to_ns) - i128::from(from_ns);
    let step_ns = step.as_nanos() as i128;
    (moved - step_ns).abs() > step_ns + JUMP_TOLERANCE.as_nanos() as i128
}

/// The gain of a fade value `0..=1`, dB (silence floors at -80 dB). The `log10` is the pinned
/// series, so a replayed mix is bit-identical across platforms (spec 35 § 12.13 class).
#[must_use]
pub fn fade_db(fade: f32) -> f32 {
    if fade >= 1.0 {
        0.0
    } else {
        20.0 * det_log10(fade.max(1.0e-4))
    }
}

/// [`regia_audio::gain_to_db`](regia_audio::spatial::gain_to_db)'s semantics (0 maps to the −100
/// dB floor) with the pinned `log10` series, so the sequencer's voice volumes are bit-identical
/// across platforms. The audio crate's own helper stays the rendering-side reference.
fn det_gain_to_db(gain: f32) -> f32 {
    // NaN must never reach a mixer volume.
    if gain.is_nan() || gain <= 1.0e-5 {
        -100.0
    } else {
        20.0 * det_log10(gain)
    }
}

/// The emitter parameters of a spatial section: its attenuation, blend and Doppler factor
/// (v5), no cone, no air absorption (what [`spatial_frame`] spatializes with). Occlusion is
/// applied by the sequencer's own query, not by these parameters.
#[must_use]
pub fn spatial_emitter(spatial: &AudioSpatial) -> AudioEmitter {
    AudioEmitter {
        autoplay: false,
        looping: false,
        min_distance: spatial.min_distance,
        max_distance: spatial.max_distance,
        rolloff: match spatial.rolloff {
            SoundRolloff::Inverse => RolloffCurve::Inverse,
            SoundRolloff::Linear => RolloffCurve::Linear,
            SoundRolloff::Log(db_per_doubling) => RolloffCurve::Log { db_per_doubling },
        },
        spatial_blend: spatial.spatial_blend,
        doppler_factor: if spatial.doppler.is_finite() {
            spatial.doppler.max(0.0)
        } else {
            0.0
        },
        air_absorption: false,
        occlusion: false,
        ..AudioEmitter::default()
    }
}

/// The gain and pan of a spatial section emitting from `position` facing `rotation` (−Z), heard
/// by the nearest of `listeners` (with none, a listener at the origin facing −Z): the emitter
/// spatialization of `regia_audio` with [`spatial_emitter`]'s parameters, the emitter still.
/// Pure. [`spatial_frame_moving`] adds the velocity Doppler needs.
#[must_use]
pub fn spatial_frame(
    spatial: &AudioSpatial,
    position: Vec3,
    rotation: Quat,
    listeners: &[ListenerFrame],
) -> SpatialFrame {
    spatial_frame_moving(spatial, position, rotation, Vec3::ZERO, listeners)
}

/// [`spatial_frame`] for an emitter moving at `velocity` (m/s) against listeners whose
/// [`ListenerFrame::velocity`] is set: its `pitch_ratio` is the Doppler ratio, clamped to
/// [`DOPPLER_RATIO_RANGE`] (v5). Pure.
#[must_use]
pub fn spatial_frame_moving(
    spatial: &AudioSpatial,
    position: Vec3,
    rotation: Quat,
    velocity: Vec3,
    listeners: &[ListenerFrame],
) -> SpatialFrame {
    let listener = nearest_listener(listeners, position)
        .and_then(|i| listeners.get(i).copied())
        .unwrap_or_else(|| ListenerFrame::looking_at(Vec3::ZERO, Vec3::NEG_Z));
    let emitter = EmitterFrame {
        position,
        velocity: if velocity.is_finite() {
            velocity
        } else {
            Vec3::ZERO
        },
        forward: rotation * Vec3::NEG_Z,
    };
    let mut frame = spatialize(&listener, &emitter, &spatial_emitter(spatial));
    let (lo, hi) = DOPPLER_RATIO_RANGE;
    frame.pitch_ratio = if frame.pitch_ratio.is_finite() {
        frame.pitch_ratio.clamp(lo, hi)
    } else {
        1.0
    };
    frame
}

/// World position and rotation of an entity, as the audio sync reads it: the `GlobalTransform`,
/// or the local `Transform` while the global is identity (not propagated yet).
fn entity_pose(global: Option<&GlobalTransform>, local: Option<&Transform>) -> (Vec3, Quat) {
    let unpropagated = |g: &GlobalTransform| {
        *g == GlobalTransform::IDENTITY && local.is_some_and(|l| *l != Transform::IDENTITY)
    };
    match (global, local) {
        (Some(global), _) if !unpropagated(global) => {
            let (_, rotation, translation) = global.0.to_scale_rotation_translation();
            (translation, rotation.normalize())
        }
        (_, Some(local)) => (local.translation, local.rotation),
        _ => (Vec3::ZERO, Quat::IDENTITY),
    }
}

/// A local velocity of an entity in world space: through its parent's transform (the global
/// times the inverse local), or as is while unparented or not propagated. Public so the editor's
/// sequence preview turns a bound emitter's track slope into a Doppler velocity the same way
/// (ADR-8591).
#[must_use]
pub fn world_velocity(global: Option<&GlobalTransform>, local: Option<&Transform>, velocity: Vec3) -> Vec3 {
    match (global, local) {
        (Some(global), Some(local)) if *global != GlobalTransform::IDENTITY => {
            let parent = global.0 * local.compute_affine().inverse();
            let world = parent.transform_vector3(velocity);
            if world.is_finite() { world } else { velocity }
        }
        _ => velocity,
    }
}

/// The `transform.position` value the sequence gives `target` at `time` (`None` without such a
/// track).
fn track_position(player: &SequencePlayer, time: f64, target: &str) -> Option<Vec3> {
    let frame = match player.library() {
        Some(library) => player.sequence().sample_with(time, library),
        None => player.sequence().sample(time),
    };
    frame.values.iter().find_map(|v| match v.value {
        ChannelValue::Vec3(p) if v.target == target && v.property == "transform.position" => {
            Some(Vec3::from_array(p))
        }
        _ => None,
    })
}

/// The slope of each name's `transform.position` track at the player's clock, m per sequence
/// second (a central difference over [`MOTION_PROBE_SECS`], one-sided at the ends). Names with
/// no such track are missing.
fn track_velocities(player: &SequencePlayer, names: &BTreeSet<&str>) -> BTreeMap<String, Vec3> {
    let mut out = BTreeMap::new();
    if names.is_empty() {
        return out;
    }
    let t = player.time();
    let a = (t - MOTION_PROBE_SECS).max(0.0);
    let b = (t + MOTION_PROBE_SECS).min(player.duration().max(t));
    if b <= a {
        return out;
    }
    for name in names {
        if let (Some(pa), Some(pb)) = (track_position(player, a, name), track_position(player, b, name)) {
            out.insert((*name).to_string(), (pb - pa) / (b - a) as f32);
        }
    }
    out
}

/// `true` when the player's sequence or a sequence of its library has a pitch-preserving sound
/// section (what the render-ahead looks for).
fn has_preserve_pitch(player: &SequencePlayer) -> bool {
    let preserves = |sequence: &super::Sequence| {
        sequence.tracks.iter().any(|t| match &t.keys {
            super::Channel::Audio(sections) => sections
                .iter()
                .any(|s| s.time_scale_mode == TimeScaleMode::PreservePitch),
            _ => false,
        })
    };
    preserves(player.sequence())
        || player.library().is_some_and(|library| {
            library
                .names()
                .filter_map(|name| library.get(name))
                .any(preserves)
        })
}

/// Where one spatial section is this frame.
#[derive(Clone, Copy, Debug)]
struct Placement {
    position: Vec3,
    frame: SpatialFrame,
    /// The emitter velocity Doppler used (`None` with Doppler off).
    velocity: Option<Vec3>,
    /// The occlusion key and the listener's place and entity (occluding sections only).
    occlusion: Option<(Entity, Vec3, Option<Entity>)>,
}

/// Plays one section: the asset and offset of a stretched or plain voice, or why not yet.
enum Source {
    Play {
        asset: SoundAsset,
        offset: f64,
        stretched: bool,
    },
    Wait,
    Skip,
}

/// The mixer, the sound library and the occlusion exchange, as the audio sync reads them.
type MixerParams<'w> = (
    Option<ResMut<'w, Audio>>,
    Option<Res<'w, SoundAssets>>,
    Option<ResMut<'w, AudioOcclusion>>,
);

/// Starts, retunes and stops the voices of every player's sound sections.
#[tracing::instrument(skip_all, target = "regia_framework", name = "sequencer_audio")]
fn sync_audio(
    mut commands: Commands,
    time: Res<Time>,
    mut players: Query<(Entity, &mut SequencePlayer)>,
    poses: Query<(Option<&GlobalTransform>, Option<&Transform>)>,
    (audio, sounds, mut occlusion): MixerParams<'_>,
    mut stretches: ResMut<SequenceStretches>,
    mut voices: ResMut<SequenceVoices>,
) {
    let Some(mut audio) = audio else {
        let ids: Vec<Entity> = voices.players.keys().copied().collect();
        for entity in ids {
            voices.forget(entity);
        }
        for proxy in std::mem::take(&mut voices.retired) {
            commands.entity(proxy).try_despawn();
        }
        return;
    };
    stretches.poll();
    let rendering = audio.mixdown_active();
    let step = time.delta();
    let step_secs = step.as_secs_f32();
    // Players that are gone stop their voices.
    let gone: Vec<Entity> = voices
        .players
        .keys()
        .copied()
        .filter(|&entity| !players.contains(entity))
        .collect();
    for entity in gone {
        voices.stop_all(&mut audio, entity);
        voices.forget(entity);
    }
    // The listeners and their velocity over the step (a camera cut counts as still).
    let listed: Vec<(Option<Entity>, ListenerFrame)> = audio.listeners().to_vec();
    let mut listeners: Vec<ListenerFrame> = listed.iter().map(|(_, l)| *l).collect();
    let same_listeners = voices.listener_positions.len() == listeners.len();
    for (i, listener) in listeners.iter_mut().enumerate() {
        let moved = match voices.listener_positions.get(i) {
            Some(last) if same_listeners && step_secs > 0.0 => (listener.position - *last) / step_secs,
            _ => Vec3::ZERO,
        };
        listener.velocity = if moved.is_finite() && moved.length() <= LISTENER_CUT_SPEED {
            moved
        } else {
            Vec3::ZERO
        };
    }
    voices.listener_positions = listeners.iter().map(|l| l.position).collect();
    let tuning = occlusion.as_deref().map(|o| o.tuning).unwrap_or_default();
    let mut rays: Vec<OcclusionRay> = Vec::new();
    let mut order: Vec<Entity> = players.iter().map(|(entity, _)| entity).collect();
    order.sort_unstable_by_key(|entity| entity.to_bits());
    for entity in order {
        let Ok((_, mut player)) = players.get_mut(entity) else {
            continue;
        };
        let frozen = player.clock == ClockSource::Gameplay && step.is_zero();
        if player.status() != PlaybackStatus::Playing || player.hold_audio || frozen {
            voices.stop_all(&mut audio, entity);
            continue;
        }
        let jumps = player.jumps();
        let clock_ns = player.time_nanos();
        let jumped = voices
            .players
            .get(&entity)
            .is_some_and(|p| p.jumps != jumps || clock_jumped(p.clock_ns, clock_ns, step));
        if jumped {
            voices.stop_all(&mut audio, entity);
        }
        // A jumped frame hears no listener motion.
        let heard: Vec<ListenerFrame> = if jumped {
            listeners
                .iter()
                .map(|l| ListenerFrame {
                    velocity: Vec3::ZERO,
                    ..*l
                })
                .collect()
        } else {
            listeners.clone()
        };
        let mut issues = Vec::new();
        {
            let frame = player.frame();
            let SequenceVoices {
                players: by_player,
                started,
                stopped,
                retired,
                ..
            } = &mut *voices;
            let entry = by_player.entry(entity).or_default();
            entry.jumps = jumps;
            entry.clock_ns = clock_ns;
            let clock = player.time();
            let wanted: BTreeMap<&[usize], &super::SoundSample<'_>> =
                frame.sounds.iter().map(|s| (s.path.as_slice(), s)).collect();
            // The track slopes of the bound emitters Doppler follows.
            let tracked: BTreeSet<&str> = wanted
                .values()
                .filter(|s| s.spatial.is_some_and(|p| p.doppler > 0.0))
                .filter_map(|s| s.emitter)
                .collect();
            let slopes = track_velocities(&player, &tracked);
            // Fixed-position occluding sections keep a proxy entity as their occlusion key.
            let occluding_fixed: BTreeSet<&[usize]> = wanted
                .iter()
                .filter(|(_, s)| {
                    s.spatial
                        .is_some_and(|p| p.occlusion && matches!(p.source, SoundSource::Position(_)))
                })
                .map(|(path, _)| *path)
                .collect();
            entry.proxies.retain(|path, proxy| {
                let keep = occluding_fixed.contains(path.as_slice());
                if !keep {
                    retired.push(*proxy);
                }
                keep
            });
            for path in &occluding_fixed {
                if !entry.proxies.contains_key(*path) {
                    entry.proxies.insert(path.to_vec(), commands.spawn_empty().id());
                }
            }
            // Where each spatial section emits from this frame, and how it sounds there.
            let mut placed: BTreeMap<&[usize], Placement> = BTreeMap::new();
            for (path, sound) in &wanted {
                let Some(spatial) = sound.spatial else {
                    continue;
                };
                let (position, rotation, bound, pose) = match &spatial.source {
                    SoundSource::Position(p) => (Vec3::from_array(*p), Quat::IDENTITY, None, None),
                    SoundSource::Binding(_) => {
                        let bound = sound
                            .emitter
                            .and_then(|name| player.target_entities().get(name).copied());
                        match bound.and_then(|e| poses.get(e).ok()) {
                            Some((global, local)) => {
                                let (position, rotation) = entity_pose(global, local);
                                (position, rotation, bound, Some((global, local)))
                            }
                            // Unbound (reported as an unbound target): at the listener.
                            None => (
                                nearest_listener(&listeners, Vec3::ZERO)
                                    .and_then(|i| listeners.get(i))
                                    .map_or(Vec3::ZERO, |l| l.position),
                                Quat::IDENTITY,
                                None,
                                None,
                            ),
                        }
                    }
                };
                // A jumped frame has no last position (its voices restarted), so only the track slope
                // gives it a velocity: a seek lands on the ratio playing through gives.
                let velocity = (spatial.doppler > 0.0).then(|| {
                    let slope = sound.emitter.and_then(|name| slopes.get(name).copied());
                    match (slope, pose) {
                        (Some(slope), Some((global, local))) => world_velocity(global, local, slope),
                        (Some(slope), None) => slope,
                        (None, _) if bound.is_some() && step_secs > 0.0 => entry
                            .voices
                            .get(*path)
                            .and_then(|r| r.last_position)
                            .map_or(Vec3::ZERO, |last| (position - last) / step_secs),
                        _ => Vec3::ZERO,
                    }
                });
                let heard_frame = spatial_frame_moving(
                    spatial,
                    position,
                    rotation,
                    velocity.unwrap_or(Vec3::ZERO),
                    &heard,
                );
                let occlusion_key = if spatial.occlusion {
                    match &spatial.source {
                        SoundSource::Position(_) => entry.proxies.get(*path).copied(),
                        SoundSource::Binding(_) => bound,
                    }
                } else {
                    None
                };
                let listener_index = nearest_listener(&listeners, position);
                let occlusion = occlusion_key.map(|key| {
                    let at = listener_index
                        .and_then(|i| listeners.get(i))
                        .map_or(Vec3::ZERO, |l| l.position);
                    let listener_entity = listener_index.and_then(|i| listed.get(i)).and_then(|(e, _)| *e);
                    (key, at, listener_entity)
                });
                placed.insert(
                    *path,
                    Placement {
                        position,
                        frame: heard_frame,
                        velocity,
                        occlusion,
                    },
                );
            }
            // What each wanted section plays: its key, and the stretch for a pitch-preserving one.
            let key_of = |sound: &super::SoundSample<'_>| -> (VoiceKey, bool) {
                let (lo, hi) = PRESERVE_PITCH_RANGE;
                let preserve = sound.time_scale_mode == TimeScaleMode::PreservePitch;
                let in_range = (f64::from(lo)..=f64::from(hi)).contains(&sound.rate);
                let key = VoiceKey {
                    sound: sound.sound.to_string(),
                    bus: sound.bus.map(str::to_string),
                    rate: sound.rate,
                    spatial: sound.spatial.is_some(),
                    stretched: preserve && in_range && sound.rate != 1.0,
                    occluding: sound.spatial.is_some_and(|p| p.occlusion),
                };
                (key, preserve && !in_range)
            };
            entry.voices.retain(|path, running| {
                let keep = wanted.get(path.as_slice()).is_some_and(|sound| {
                    running.key == key_of(sound).0
                        && (running.anchor - (sound.position - clock * sound.rate)).abs() < ANCHOR_TOLERANCE
                });
                if !keep {
                    audio.stop_voice(running.voice.id, DECLICK_FADE);
                    *stopped += 1;
                }
                keep
            });
            for (path, sound) in &wanted {
                let place = placed.get(path).copied();
                let (key, out_of_range) = key_of(sound);
                if out_of_range {
                    issues.push(SequenceIssue::ApplyFailed {
                        track: sound.track.to_string(),
                        reason: format!(
                            "stretch_rate: section `{}` plays at rate {} outside {}..={}; varispeed instead",
                            sound.sound, sound.rate, PRESERVE_PITCH_RANGE.0, PRESERVE_PITCH_RANGE.1
                        ),
                    });
                }
                let (pan, spatial_gain) = place.map_or((0.0, 1.0), |p| (p.frame.pan, p.frame.gain));
                let position = place.map(|p| p.position.to_array());
                let doppler = place.map_or(1.0, |p| p.frame.pitch_ratio);
                let pitch = if key.stretched {
                    doppler
                } else {
                    (sound.rate as f32) * doppler
                };
                // Occlusion: queue this frame's ray, and read the latest report.
                let reported = match place.and_then(|p| p.occlusion) {
                    Some((key_entity, from, listener)) => {
                        let max = sound.spatial.map_or(0.0, |s| s.max_distance);
                        if (place.map_or(Vec3::ZERO, |p| p.position) - from).length() < max {
                            rays.push(OcclusionRay {
                                emitter: key_entity,
                                listener,
                                from,
                                to: place.map_or(Vec3::ZERO, |p| p.position),
                            });
                        }
                        Some(occlusion.as_deref().map_or(0.0, |o| o.reported(key_entity)))
                    }
                    None => None,
                };
                let amount = match (reported, entry.voices.get(*path)) {
                    (Some(target), Some(running)) => {
                        tuning.smooth(running.voice.occlusion, target, step_secs)
                    }
                    (Some(target), None) => target,
                    (None, _) => 0.0,
                };
                let cutoff = reported.map(|_| tuning.cutoff_hz(amount) as f32);
                let occlusion_db = if reported.is_some() {
                    gain_to_db(tuning.gain(amount))
                } else {
                    0.0
                };
                let db = sound.volume_db
                    + fade_db(sound.fade)
                    + if place.is_some() {
                        det_gain_to_db(spatial_gain)
                    } else {
                        0.0
                    }
                    + occlusion_db;
                if let Some(running) = entry.voices.get_mut(*path) {
                    if (running.voice.volume_db - db).abs() > 0.01 {
                        audio.set_voice_volume_db(running.voice.id, db, 0.0);
                        running.voice.volume_db = db;
                    }
                    if place.is_some() && (running.voice.pan - pan).abs() > 1.0e-3 {
                        audio.set_voice_pan(running.voice.id, pan, 0.0);
                        running.voice.pan = pan;
                    }
                    if (running.voice.pitch - pitch).abs() > 1.0e-6 {
                        // Over the step: the pitch glides with the motion instead of stepping.
                        audio.set_voice_pitch(running.voice.id, pitch, step_secs);
                        running.voice.pitch = pitch;
                    }
                    if let (Some(hz), Some(last)) = (cutoff, running.voice.cutoff_hz)
                        && (hz - last).abs() > last * 0.01
                    {
                        audio.set_voice_lowpass(running.voice.id, hz, step_secs);
                        running.voice.cutoff_hz = Some(hz);
                    }
                    running.voice.spatial_gain = spatial_gain;
                    running.voice.position = position;
                    running.voice.doppler = doppler;
                    running.voice.velocity = place.and_then(|p| p.velocity).map(|v| v.to_array());
                    running.voice.occlusion = amount;
                    running.last_position = place.map(|p| p.position);
                    continue;
                }
                let Some(asset) = sounds.as_deref().and_then(|s| s.by_name(sound.sound)) else {
                    issues.push(SequenceIssue::UnknownSound {
                        track: sound.track.to_string(),
                        sound: sound.sound.to_string(),
                    });
                    continue;
                };
                let source = if key.stretched {
                    let stretch_key = StretchKey::new(
                        sound.sound,
                        sound.section_offset,
                        sound.section_source_secs,
                        sound.rate,
                    );
                    match stretches.get(&stretch_key, asset, rendering) {
                        StretchState::Ready(stretched) => Source::Play {
                            offset: (sound.position - sound.section_offset) / sound.rate,
                            asset: stretched,
                            stretched: true,
                        },
                        StretchState::Pending => Source::Wait,
                        StretchState::Failed(reason) => {
                            issues.push(SequenceIssue::ApplyFailed {
                                track: sound.track.to_string(),
                                reason,
                            });
                            Source::Skip
                        }
                    }
                } else {
                    Source::Play {
                        asset: asset.clone(),
                        offset: sound.position,
                        stretched: false,
                    }
                };
                let Source::Play {
                    asset,
                    offset,
                    stretched,
                } = source
                else {
                    continue;
                };
                if offset >= asset.duration_secs() {
                    continue;
                }
                let params = PlaybackParams {
                    bus: sound.bus.map(str::to_string),
                    volume_db: db,
                    pitch,
                    panning: place.map(|_| pan),
                    start_offset_secs: offset,
                    ..PlaybackParams::default()
                };
                let started_voice = match cutoff {
                    Some(hz) => audio.start_asset_lowpass(&asset, params, hz),
                    None => audio.start_asset(&asset, params),
                };
                match started_voice {
                    Ok(id) => {
                        entry.voices.insert(
                            path.to_vec(),
                            RunningVoice {
                                voice: SequenceVoice {
                                    id,
                                    volume_db: db,
                                    pan,
                                    spatial_gain,
                                    position,
                                    pitch,
                                    doppler,
                                    velocity: place.and_then(|p| p.velocity).map(|v| v.to_array()),
                                    occlusion: amount,
                                    cutoff_hz: cutoff,
                                    stretched,
                                },
                                key,
                                anchor: sound.position - clock * sound.rate,
                                last_position: place.map(|p| p.position),
                            },
                        );
                        *started += 1;
                    }
                    Err(error) => issues.push(SequenceIssue::ApplyFailed {
                        track: sound.track.to_string(),
                        reason: error.to_string(),
                    }),
                }
            }
            // Render ahead: the pitch-preserving sections the clock reaches within the lookahead.
            if !rendering && has_preserve_pitch(&player) {
                let ahead = clock + STRETCH_LOOKAHEAD_SECS;
                if ahead < player.duration() {
                    let later = match player.library() {
                        Some(library) => player.sequence().sample_with(ahead, library),
                        None => player.sequence().sample(ahead),
                    };
                    for sound in &later.sounds {
                        let (key, _) = key_of(sound);
                        if !key.stretched {
                            continue;
                        }
                        if let Some(asset) = sounds.as_deref().and_then(|s| s.by_name(sound.sound)) {
                            let stretch_key = StretchKey::new(
                                sound.sound,
                                sound.section_offset,
                                sound.section_source_secs,
                                sound.rate,
                            );
                            let _ = stretches.get(&stretch_key, asset, false);
                        }
                    }
                }
            }
        }
        for issue in issues {
            player.report(issue);
        }
    }
    if let Some(occlusion) = occlusion.as_deref_mut() {
        occlusion.set_extra_rays_for(OCCLUSION_OWNER, rays);
    }
    for proxy in std::mem::take(&mut voices.retired) {
        commands.entity(proxy).try_despawn();
    }
}

/// Runs the audio sync while a player exists or a voice still runs.
fn audio_wanted(players: Query<(), With<SequencePlayer>>, voices: Res<SequenceVoices>) -> bool {
    !players.is_empty() || !voices.is_empty()
}

/// Adds [`SequenceVoices`], [`SequenceStretches`] and the audio sync (`PostUpdate`, after
/// [`SequencerSet::Apply`]).
pub(crate) fn install(app: &mut App) {
    app.init_resource::<SequenceVoices>()
        .init_resource::<SequenceStretches>()
        .add_systems(
            PostUpdate,
            sync_audio.after(SequencerSet::Apply).run_if(audio_wanted),
        );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin: the fade gain and the spatial gain use the pinned `log10` series, so a replayed mix
    /// is bit-identical across platforms (a platform libm's `log10` used to decide the last digit
    /// of the voice volume).
    #[test]
    fn fade_db_and_spatial_gain_db_are_pinned_across_platforms() {
        assert_eq!(fade_db(0.5).to_bits(), 0xc0c0_a8c2);
        assert_eq!(fade_db(1.0), 0.0);
        assert_eq!(fade_db(2.0), 0.0, "a fade above 1 is unity");
        assert!((fade_db(0.0) - -80.0).abs() < 1.0e-3, "{}", fade_db(0.0));
        // Same semantics as `regia_audio::gain_to_db` (0 and NaN floor at -100 dB), pinned bits.
        assert_eq!(det_gain_to_db(0.0), -100.0);
        assert_eq!(det_gain_to_db(f32::NAN), -100.0);
        assert_eq!(det_gain_to_db(1.0), 0.0);
        assert_eq!(det_gain_to_db(1.0e-5), -100.0);
        assert_eq!(det_gain_to_db(0.25).to_bits(), 0xc140_a8c2);
        // The pair agrees with the audio crate's helper to well under a millibel.
        assert!(
            (det_gain_to_db(0.25) - regia_audio::gain_to_db(0.25)).abs() < 1.0e-3,
            "{}",
            det_gain_to_db(0.25)
        );
    }
}
