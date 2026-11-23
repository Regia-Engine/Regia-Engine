// SPDX-License-Identifier: MPL-2.0
//! The edit-world preview's sound and video sections (ADR-8940, ADR-8591,
//! `book/agents/seq-curve-panel.md`).
//!
//! **Sound.** Every flat and spatial sound section the playhead is inside plays one voice on the
//! editor's audio output: the edit world's `regia_audio::Audio` when it has one, else the preview
//! output the asset viewer and the content browser play sounds on
//! (`crate::asset_viewer::with_preview_audio`). Never a play-in-editor world: the preview only
//! ever touches the edit world it is given. A section the playhead enters starts its voice at the
//! section offset (`SoundSample::position`); playing on (the timeline's transport ticks, marked by
//! [`mark_play_step`]) keeps it; any other move of the playhead (a scrub, a seek, a loop wrap)
//! restarts it at the new offset; leaving the section, an edit of its sound, bus, rate, timing,
//! time-scale mode or occlusion, and Stop stop it. A forward transport tick plays on however long it
//! is (a slow frame); a loop wrap is a move. Reverse play is silent: its first tick stops the voices
//! and the later ones start none ([`mark_reverse_step`]). A voice whose sound has ended is stopped
//! and listed as not playing (`past_end`). The transport stopping (Pause, transport Stop, a
//! non-looping play reaching its end) silences every voice ([`hold_sounds`]); they stay silent while
//! the playhead stays put and start again at the playhead's offset on Play or a move. The sound
//! comes from the edit world's `SoundAssets` by name, else from the project file of that name.
//!
//! **Spatial sections** play from their bound emitter (the entity's world pose) or their fixed
//! position, panned and attenuated by the runtime's own spatialization
//! (`regia_framework::sequencer::audio::spatial_frame_moving`) against the output's nearest
//! listener, or, when the output has none (the edit world runs no listener), against the editor
//! camera (its eye, facing its forward; `crate::viewport::EditorCamera`).
//!
//! **Doppler** (a section's `doppler` above 0) uses the runtime's numbers: the emitter's velocity
//! is the slope of its binding's `transform.position` track on the sequence clock (a central
//! difference over `MOTION_PROBE_SECS`, through the bound entity's parent transform,
//! `regia_framework::sequencer::audio::world_velocity`), else, while the transport plays on, its
//! world position's change over the tick; the listener's velocity is the camera eye's change over a
//! forward tick (a jump faster than `LISTENER_CUT_SPEED` counts as still); a scrub or a seek hears
//! no motion. The voice's pitch is the section rate (1 for a stretched section) times the Doppler
//! ratio, clamped like the runtime's.
//!
//! **Occlusion** (a section's `occlusion`) asks the same question as the runtime's physics
//! occlusion provider (`regia::audio_occlusion`): how many distinct solid colliders stand between
//! the listener and the emitter (the emitter's and the listener's own bodies and their ancestors
//! never occlude, nor do sensors, at most `OCCLUSION_OBSTACLE_CAP`), within the section's
//! `max_distance`. The edit world runs no physics, so the preview casts its rays through a physics
//! world of its own that mirrors the edit world's `Collider`s at their world poses (rebuilt when a
//! collider or its pose changes, built only while an occluding section plays). The amount, the
//! voice's volume change and its low-pass cutoff come from the tuning (`AudioOcclusion`'s when the
//! edit world has one, else the default), like the runtime's voice; a voice starts at the amount
//! and a forward tick smooths toward it over the tick.
//!
//! **Pitch-preserving sections** (`TimeScaleMode::PreservePitch`, a rate other than 1 inside
//! `PRESERVE_PITCH_RANGE`) play the same stretch the runtime and the render-out play:
//! `regia_audio::stretch_asset` over the section's whole range of the sound at the voice rate,
//! played at pitch 1 (times Doppler) from `(position - offset) / rate` into it. Ranges up to
//! `STRETCH_INLINE_SECS` of sound render on the apply that needs them; longer ones render off the UI
//! thread on the async compute pool (from `STRETCH_LOOKAHEAD_SECS` before the playhead reaches them
//! while playing, or when the playhead lands in them), into a byte-bounded cache
//! (`STRETCH_CACHE_BYTES`, least recently used out first). Until its stretch is ready a section is
//! listed as not playing with `stretch_pending`, and the apply after the render finishes starts it
//! at the playhead's offset. A rate outside the range plays by varispeed and is listed with
//! `stretch_rate`, as the runtime reports it.
//!
//! **Video.** A video section whose target is a bound entity's own `VideoPlayer` shows the picture
//! of the section's source time on that entity: the preview opens the section's source in the
//! entity's `VideoPlayerState` (the runtime session, which is never reflected, saved or copied) with
//! `regia_media`'s blocking clock, and each apply repositions (`PlayerCore::reposition`, back or
//! more than `FORWARD_SEEK` ahead) and decodes frame-exact to the source time
//! (`PlayerCore::show_at`, the same half-millisecond rule as the runtime). The authored
//! `VideoPlayer` component is never written (ADR-7847), so an undo snapshot, a prefab, a duplicate,
//! a level save, an autosave or Play only ever see the authored player, and a save or Play does not
//! close and re-open the preview. Leaving the section and Stop close the session (a fresh state, or
//! none when the entity had none).
//!
//! **Fullscreen and mesh targets** run `regia_media`'s frame publisher
//! (`regia_media::publish_frames_once`) over sessions the preview keeps in a world of its own, the
//! stage (never the edit world's entities, never a play-in-editor world), only while such a section
//! is in effect. A fullscreen section's published picture is drawn over the viewport
//! ([`preview_overlay_frame`], with its fit and layer); a mesh (`screen`) section binds the picture
//! into the bound mesh's material slot (`regia_media::VideoMaterial` on a stand-in, the material
//! handed to the edit world's `MeshRenderer`), and the mesh gets its own material back when the
//! section ends and on Stop. `MeshRenderer` is never saved or reflected, so a save or an undo
//! snapshot never sees the bound material. UI image targets are listed with `target_not_previewed`
//! and a `message`: they show in the running game's UI (the edit world draws no game UI).

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_audio::{
    Audio, AudioOcclusion, DECLICK_FADE, ListenerFrame, OcclusionTuning, PlaybackParams, SoundAsset,
    SoundAssets, SoundInstanceId, nearest_listener, stretch_asset,
};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::*;
use regia_framework::sequencer::audio::{spatial_frame_moving, world_velocity};
use regia_framework::sequencer::video::{FORWARD_SEEK, media_time_ns};
use regia_framework::sequencer::{
    AudioSpatial, LISTENER_CUT_SPEED, PRESERVE_PITCH_RANGE, STRETCH_CACHE_BYTES, STRETCH_INLINE_SECS,
    SoundSource, VideoFit, VideoScreenSlot, VideoTarget, fade_db,
};
use regia_media::player::ClockInput;
use regia_media::plugin::{MediaFiles, MediaSettings, VisibilityPolicy, open_player_pooled};
use regia_media::{
    ClockMode, Fit, LoopMode, MediaFrameSource, PlayState, VideoFullscreen, VideoMaterial, VideoPlayer,
    VideoPlayerState,
};
use regia_render::{MeshRenderer, VideoFrameSource, VideoSlot};
use regia_scene::prelude::{GlobalTransform, Transform};
use serde_json::{Value, json};

/// Most pictures one apply decodes for one video section (the runtime's frame-exact budget).
pub const VIDEO_BUDGET: u32 = 20_000;

/// How far (seconds) a section's mapping from the clock to the sound may move before its voice
/// no longer plays what the section says (the runtime's tolerance).
const ANCHOR_TOLERANCE: f64 = 1.0e-3;

/// Physics updates the occlusion mirror runs after its colliders spawn (the collider sync, then a
/// step that fills the query structure; `regia_physics::WorldQueries`'s own example).
const MIRROR_UPDATES: usize = 6;

/// One sound section in effect at the previewed time, owned.
#[derive(Clone, Debug)]
pub(super) struct SoundJob {
    pub(super) path: Vec<usize>,
    pub(super) track: String,
    pub(super) sound: String,
    pub(super) position: f64,
    pub(super) rate: f64,
    pub(super) volume_db: f32,
    pub(super) fade: f32,
    pub(super) bus: Option<String>,
    pub(super) spatial: Option<AudioSpatial>,
    pub(super) emitter: Option<String>,
    /// The section keeps its pitch (`TimeScaleMode::PreservePitch`).
    pub(super) preserve: bool,
    /// Seconds into the sound at the section's start.
    pub(super) section_offset: f64,
    /// Seconds of the sound the section plays from start to end.
    pub(super) section_source_secs: f64,
    /// The slope of the emitter binding's `transform.position` track at the clock, m per sequence
    /// second (`None` without such a track, or with Doppler off).
    pub(super) slope: Option<Vec3>,
}

/// A pitch-preserving section the playhead reaches within the lookahead: its stretch renders
/// ahead.
#[derive(Clone, Debug)]
pub(super) struct StretchAhead {
    pub(super) sound: String,
    pub(super) section_offset: f64,
    pub(super) section_source_secs: f64,
    pub(super) rate: f64,
}

/// One video section in effect at the previewed time, owned.
#[derive(Clone, Debug)]
pub(super) struct VideoJob {
    pub(super) path: Vec<usize>,
    pub(super) track: String,
    pub(super) video: String,
    pub(super) target: VideoTarget,
    pub(super) binding: Option<String>,
    pub(super) position: f64,
    pub(super) looping: bool,
}

/// What a voice was started from: a change restarts it.
#[derive(Clone, Debug, PartialEq)]
struct VoiceKey {
    sound: String,
    bus: Option<String>,
    rate: f64,
    spatial: bool,
    stretched: bool,
    occluding: bool,
}

#[derive(Debug)]
struct PreviewVoice {
    id: SoundInstanceId,
    key: VoiceKey,
    /// Where the sound ends, in section positions (seconds into the sound): a voice whose
    /// position passes it has ended.
    duration: f64,
    /// The sound position at clock zero: constant while the section keeps its timing.
    anchor: f64,
    volume_db: f32,
    pan: f32,
    pitch: f32,
    occlusion: f32,
    cutoff_hz: Option<f32>,
    /// The emitter's place on the last apply (a velocity source while playing on).
    last_position: Option<Vec3>,
}

#[derive(Debug)]
struct PreviewVideo {
    entity: Entity,
    source: String,
    /// The bound entity had a `VideoPlayerState` before the preview opened its session in one.
    had_state: bool,
    /// The media time last shown, nanoseconds.
    last_ns: u64,
}

/// A fullscreen or mesh section's session on the stage.
#[derive(Debug)]
struct StagedVideo {
    /// The session (its entity is on the stage).
    session: PreviewVideo,
    /// `fullscreen` or `screen`.
    kind: &'static str,
    /// The fullscreen layer and fit.
    fullscreen: Option<(i32, VideoFit)>,
    /// The bound mesh: the edit entity, its own material, the stand-in on the stage, and the
    /// material last handed to the mesh.
    mesh: Option<StagedMesh>,
}

#[derive(Debug)]
struct StagedMesh {
    entity: Entity,
    own: Arc<regia_render::CpuMaterial>,
    stand_in: Entity,
    bound: Option<Arc<regia_render::CpuMaterial>>,
}

/// The preview's sound and video state.
#[derive(Debug, Default)]
pub(super) struct PreviewMedia {
    voices: BTreeMap<Vec<usize>, PreviewVoice>,
    videos: BTreeMap<Vec<usize>, PreviewVideo>,
    /// Fullscreen and mesh sections' sessions, on the stage.
    staged: BTreeMap<Vec<usize>, StagedVideo>,
    /// The world the staged sessions live in (`None` while none runs).
    stage: Option<World>,
    /// Voices started so far.
    pub(super) started: u64,
    /// Voices stopped so far.
    pub(super) stopped: u64,
    /// Video players the preview opened (took over) so far this session.
    pub(super) video_opens: u64,
    /// Sounds read from project files, by name (`None`: unreadable, not tried again).
    files: BTreeMap<String, Option<SoundAsset>>,
    /// The pitch-preserving sections' stretches.
    pub(super) stretches: PreviewStretches,
    /// The occlusion rays' physics world (`None` until an occluding section plays).
    occlusion: Option<OcclusionMirror>,
    /// The editor camera's eye on the last apply (the listener's velocity while playing on).
    last_eye: Option<Vec3>,
    /// The next apply is a transport tick ([`mark_play_step`], [`mark_reverse_step`]).
    pub(super) play_step: Option<PlayStep>,
    /// The transport stopped (Pause, Stop, the end of a non-looping play): the voices are
    /// silenced and stay so through re-applies at the same time, until a transport tick or a
    /// move of the playhead ([`hold_sounds`]).
    pub(super) held: bool,
    /// The time the sounds were last synced at.
    last_time: Option<f64>,
    /// The sound sections of the last apply, for the state method.
    pub(super) sounds_json: Vec<Value>,
    /// The video sections of the last apply, for the state method.
    pub(super) videos_json: Vec<Value>,
}

impl PreviewMedia {
    /// A stretch rendering off the UI thread has finished: the next apply must run even when the
    /// playhead did not move (it starts the waiting section).
    pub(super) fn stretch_finished(&self) -> bool {
        self.stretches.any_finished()
    }

    /// The stretch cache's counters and the occlusion mirror's builds, for the state method.
    pub(super) fn stats_json(&self) -> Value {
        json!({
            "stretches": {
                "rendered": self.stretches.rendered,
                "inline": self.stretches.inline,
                "pending": self.stretches.pending(),
            },
            "occlusion_mirror_builds": self.occlusion.as_ref().map_or(0, |m| m.builds),
        })
    }
}

/// Which way a transport tick moved the clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlayStep {
    /// Play: voices inside their sections play on.
    Forward,
    /// Reverse play: silent.
    Reverse,
}

// ---- stretches ----

/// Identifies one stretch: the sound, the range of it and the rate (the runtime's key).
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

type StretchTask = regia_core::tasks::Task<Result<SoundAsset, String>>;

/// Where a stretch stands.
enum StretchState {
    Ready(SoundAsset),
    Pending,
    Failed(String),
}

/// The preview's cache of pitch-preserving section stretches (the runtime's rules: short ranges
/// render on the apply, longer ones on the async compute pool; bounded by `STRETCH_CACHE_BYTES`).
#[derive(Default)]
pub(super) struct PreviewStretches {
    ready: BTreeMap<StretchKey, (SoundAsset, usize, u64)>,
    pending: BTreeMap<StretchKey, StretchTask>,
    failures: BTreeMap<StretchKey, String>,
    bytes: usize,
    clock: u64,
    /// Stretches rendered so far.
    pub(super) rendered: u64,
    /// Of those, rendered on the apply that needed them.
    pub(super) inline: u64,
    /// A render from the pool was collected outside an apply ([`Self::finish_pending`]): the next
    /// apply runs even with the playhead still.
    fresh: bool,
}

impl std::fmt::Debug for PreviewStretches {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewStretches")
            .field("ready", &self.ready.len())
            .field("pending", &self.pending.len())
            .field("bytes", &self.bytes)
            .field("rendered", &self.rendered)
            .finish()
    }
}

impl PreviewStretches {
    /// Stretches rendering on the pool.
    pub(super) fn pending(&self) -> usize {
        self.pending.len()
    }

    fn any_finished(&self) -> bool {
        self.fresh || self.pending.values().any(StretchTask::is_finished)
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

    /// Waits for every stretch rendering on the pool (tests: a join, not a sleep).
    #[cfg(test)]
    pub(super) fn finish_pending(&mut self) {
        for (key, task) in std::mem::take(&mut self.pending) {
            let result = regia_core::tasks::block_on(task);
            self.finish(key, result);
            self.fresh = true;
        }
    }

    fn finish(&mut self, key: StretchKey, result: Result<SoundAsset, String>) {
        match result {
            Ok(asset) => {
                self.rendered += 1;
                let bytes = (asset.duration_secs() * f64::from(asset.sample_rate())) as usize * 8;
                self.clock += 1;
                self.bytes += bytes;
                self.ready.insert(key, (asset, bytes, self.clock));
                while self.bytes > STRETCH_CACHE_BYTES && self.ready.len() > 1 {
                    let Some(oldest) = self.ready.iter().min_by_key(|(_, c)| c.2).map(|(k, _)| k.clone())
                    else {
                        break;
                    };
                    if let Some(gone) = self.ready.remove(&oldest) {
                        self.bytes -= gone.1;
                    }
                }
            }
            Err(reason) => {
                self.failures.insert(key, reason);
            }
        }
    }

    /// The stretch of `key`, starting its render when it is not there.
    fn get(&mut self, key: &StretchKey, asset: &SoundAsset) -> StretchState {
        self.clock += 1;
        if let Some(cached) = self.ready.get_mut(key) {
            cached.2 = self.clock;
            return StretchState::Ready(cached.0.clone());
        }
        if let Some(reason) = self.failures.get(key) {
            return StretchState::Failed(reason.clone());
        }
        let offset = f64::from_bits(key.offset_bits);
        let source = f64::from_bits(key.source_bits);
        let rate = f64::from_bits(key.rate_bits) as f32;
        if source <= STRETCH_INLINE_SECS {
            let _span = tracing::debug_span!(target: "regia_editor", "seq_preview_stretch").entered();
            self.pending.remove(key);
            let result = stretch_asset(asset, offset, source, rate).map_err(|e| e.to_string());
            self.inline += 1;
            self.finish(key.clone(), result);
            return match self.ready.get(key) {
                Some(cached) => StretchState::Ready(cached.0.clone()),
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

/// Whether a section at `rate` plays a stretch (the runtime's rule).
fn stretches(preserve: bool, rate: f64) -> (bool, bool) {
    let (lo, hi) = PRESERVE_PITCH_RANGE;
    let in_range = (f64::from(lo)..=f64::from(hi)).contains(&rate);
    (preserve && in_range && rate != 1.0, preserve && !in_range)
}

/// `true` when `sequence` has a pitch-preserving sound section (what the render-ahead looks for).
pub(super) fn has_preserve_pitch(sequence: &regia_framework::sequencer::Sequence) -> bool {
    sequence.tracks.iter().any(|t| match &t.keys {
        regia_framework::sequencer::Channel::Audio(sections) => sections
            .iter()
            .any(|s| s.time_scale_mode == regia_framework::sequencer::TimeScaleMode::PreservePitch),
        regia_framework::sequencer::Channel::Sub(_) => true,
        _ => false,
    })
}

/// The slope of `target`'s `transform.position` track at `time`, m per sequence second: the
/// runtime's central difference over `MOTION_PROBE_SECS` (one-sided at the ends of the sequence).
/// `None` without such a track.
pub(super) fn track_slope(
    sequence: &regia_framework::sequencer::Sequence,
    library: &regia_framework::sequencer::SequenceLibrary,
    time: f64,
    target: &str,
) -> Option<Vec3> {
    use regia_framework::sequencer::{ChannelValue, MOTION_PROBE_SECS};
    let a = (time - MOTION_PROBE_SECS).max(0.0);
    let b = (time + MOTION_PROBE_SECS).min(sequence.duration().max(time));
    if b <= a {
        return None;
    }
    let at = |t: f64| {
        sequence
            .sample_with(t, library)
            .values
            .iter()
            .find_map(|v| match v.value {
                ChannelValue::Vec3(p) if v.target == target && v.property == "transform.position" => {
                    Some(Vec3::from_array(p))
                }
                _ => None,
            })
    };
    let (pa, pb) = (at(a)?, at(b)?);
    Some((pb - pa) / (b - a) as f32)
}

// ---- occlusion ----

/// A physics world mirroring the edit world's colliders, for the occlusion rays.
struct OcclusionMirror {
    physics: regia_physics::PhysicsWorld,
    layers: Option<regia_physics::PhysicsLayers>,
    /// The colliders it was built from (entity, collider, scale/rotation/translation).
    colliders: Vec<(Entity, regia_physics::Collider, [f32; 10])>,
    /// Edit entity to mirror entity.
    to_mirror: BTreeMap<Entity, Entity>,
    builds: u64,
}

impl std::fmt::Debug for OcclusionMirror {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OcclusionMirror")
            .field("colliders", &self.colliders.len())
            .field("builds", &self.builds)
            .finish()
    }
}

/// The edit world's colliders at their world poses, in entity order.
fn edit_colliders(world: &mut World) -> Vec<(Entity, regia_physics::Collider, [f32; 10])> {
    let mut query = world.query::<(
        Entity,
        &regia_physics::Collider,
        Option<&GlobalTransform>,
        Option<&Transform>,
    )>();
    let mut out: Vec<(Entity, regia_physics::Collider, [f32; 10])> = query
        .iter(world)
        .map(|(entity, collider, global, local)| {
            let (scale, rotation, translation) = match (global, local) {
                (Some(g), l)
                    if *g != GlobalTransform::IDENTITY || l.is_none_or(|l| *l == Transform::IDENTITY) =>
                {
                    g.0.to_scale_rotation_translation()
                }
                (_, Some(l)) => (l.scale, l.rotation, l.translation),
                _ => (Vec3::ONE, Quat::IDENTITY, Vec3::ZERO),
            };
            let r = rotation.normalize();
            let pose = [
                scale.x,
                scale.y,
                scale.z,
                r.x,
                r.y,
                r.z,
                r.w,
                translation.x,
                translation.y,
                translation.z,
            ];
            (entity, collider.clone(), pose)
        })
        .collect();
    out.sort_by_key(|(entity, _, _)| entity.to_bits());
    out
}

/// Brings the mirror up to date with the edit world's colliders (rebuilt only on a change).
fn ensure_mirror(world: &mut World, media: &mut PreviewMedia) {
    let colliders = edit_colliders(world);
    let layers = world.get_resource::<regia_physics::PhysicsLayers>().cloned();
    if let Some(mirror) = media.occlusion.as_mut()
        && mirror.colliders == colliders
    {
        mirror.layers = layers;
        return;
    }
    let _span = tracing::debug_span!(target: "regia_editor", "seq_preview_occlusion_mirror").entered();
    use regia_app::prelude::{App, FixedTime, TimeUpdateStrategy};
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_nanos(16_666_667),
    ));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins(regia_physics::PhysicsPlugin);
    let mut to_mirror = BTreeMap::new();
    for (entity, collider, p) in &colliders {
        let transform = Transform {
            translation: Vec3::new(p[7], p[8], p[9]),
            rotation: Quat::from_xyzw(p[3], p[4], p[5], p[6]),
            scale: Vec3::new(p[0], p[1], p[2]),
        };
        let mirror = app
            .world_mut()
            .spawn((
                transform,
                regia_physics::RigidBody::static_body(),
                collider.clone(),
            ))
            .id();
        to_mirror.insert(*entity, mirror);
    }
    for _ in 0..MIRROR_UPDATES {
        app.update();
    }
    let builds = media.occlusion.as_ref().map_or(0, |m| m.builds) + 1;
    media.occlusion = app
        .world_mut()
        .remove_resource::<regia_physics::PhysicsWorld>()
        .map(|physics| OcclusionMirror {
            physics,
            layers,
            colliders,
            to_mirror,
            builds,
        });
}

/// `entity` and its ancestors in the edit world, as mirror entities.
fn mirrored_ancestors(world: &World, mirror: &OcclusionMirror, entity: Entity, out: &mut Vec<Entity>) {
    let mut current = Some(entity);
    for _ in 0..64 {
        let Some(e) = current else {
            return;
        };
        if let Some(m) = mirror.to_mirror.get(&e) {
            out.push(*m);
        }
        current = world.get::<ChildOf>(e).map(ChildOf::parent);
    }
}

/// Distinct solid colliders between `from` and `to` (the runtime provider's count), ignoring the
/// mirrors of `ignore`.
fn obstacles(mirror: &OcclusionMirror, from: Vec3, to: Vec3, ignore: &[Entity]) -> usize {
    let span = to - from;
    let distance = span.length();
    if distance < 1e-3 || !distance.is_finite() {
        return 0;
    }
    let queries = regia_physics::WorldQueries {
        world: &mirror.physics,
        layers: mirror.layers.as_ref(),
    };
    let mut hits: Vec<Entity> = Vec::new();
    let mut exclude: Vec<Entity> = Vec::new();
    while hits.len() < regia::audio_occlusion::OCCLUSION_OBSTACLE_CAP {
        exclude.clear();
        exclude.extend_from_slice(ignore);
        exclude.extend_from_slice(&hits);
        let filter = regia_physics::QueryFilter::new()
            .exclude_sensors()
            .exclude_entities(&exclude);
        match queries.raycast(from, span, distance, filter) {
            Some(hit) if !hits.contains(&hit.entity) => hits.push(hit.entity),
            _ => break,
        }
    }
    hits.len()
}

// ---- sound ----

/// Runs `f` on the editor's audio output: the edit world's `Audio`, else the preview output.
fn with_output<R>(world: &mut World, f: impl FnOnce(&mut Audio) -> R) -> R {
    match world.get_resource_mut::<Audio>() {
        Some(mut audio) => f(&mut audio),
        None => crate::asset_viewer::with_preview_audio(f),
    }
}

/// World position and facing of an entity: its `GlobalTransform`, or its `Transform` while the
/// global is still identity (the edit world propagates only some frames).
fn entity_pose(world: &World, entity: Entity) -> Option<(Vec3, Quat)> {
    let local = world.get::<Transform>(entity);
    let global = world.get::<GlobalTransform>(entity);
    match (global, local) {
        (Some(g), l) if *g != GlobalTransform::IDENTITY || l.is_none_or(|l| *l == Transform::IDENTITY) => {
            let (_, rotation, translation) = g.0.to_scale_rotation_translation();
            Some((translation, rotation.normalize()))
        }
        (_, Some(l)) => Some((l.translation, l.rotation)),
        _ => None,
    }
}

/// The sound `name`: the edit world's `SoundAssets`, else the project file of that name.
fn sound_asset(world: &World, media: &mut PreviewMedia, name: &str) -> Option<SoundAsset> {
    if let Some(asset) = world.get_resource::<SoundAssets>().and_then(|s| s.by_name(name)) {
        return Some(asset.clone());
    }
    if let Some(cached) = media.files.get(name) {
        return cached.clone();
    }
    let loaded = crate::seq_curve_panel::project_root(world)
        .map(|root| root.join(name))
        .filter(|file| file.is_file())
        .and_then(|file| match SoundAsset::from_file_static(&file) {
            Ok(asset) => Some(asset),
            Err(error) => {
                tracing::debug!(target: "regia_editor", "sequence preview: sound {name}: {error}");
                None
            }
        });
    media.files.insert(name.to_string(), loaded.clone());
    loaded
}

/// Marks the next preview apply as a forward transport tick (playing on): voices inside their
/// sections keep playing instead of restarting, however long the tick (a slow frame). The
/// timeline's `tick` calls it before it moves the clock forward.
pub fn mark_play_step(world: &mut World) {
    mark_step(world, PlayStep::Forward);
}

/// Marks the next preview apply as a reverse transport tick: the preview's sounds are silent
/// while the transport runs backward (a voice cannot play backward; restarting it on every tick
/// would stutter). The timeline's `tick` calls it before it moves the clock backward.
pub fn mark_reverse_step(world: &mut World) {
    mark_step(world, PlayStep::Reverse);
}

fn mark_step(world: &mut World, step: PlayStep) {
    if let Some(mut authoring) = world.get_resource_mut::<super::SeqAuthoring>()
        && authoring.preview.is_some()
    {
        authoring.media.play_step = Some(step);
    }
}

/// Silences the preview's voices because the transport stopped (Pause, Stop, a non-looping play
/// reaching its end; sprint-w9-rev-v06-lanes § 1 finding 1). They stay silent while the preview
/// re-applies at the same time (an edit, the state call, a save's lift); the next transport tick
/// (Play) or move of the playhead starts them again at the playhead's offset. The timeline's
/// transport calls it.
pub fn hold_sounds(world: &mut World) {
    let Some(mut media) = world
        .get_resource_mut::<super::SeqAuthoring>()
        .filter(|a| a.preview.is_some())
        .map(|mut a| std::mem::take(&mut a.media))
    else {
        return;
    };
    let last_time = media.last_time;
    stop_sounds(world, &mut media);
    // Kept: a re-apply at the same time is not a move.
    media.last_time = last_time;
    media.held = true;
    media.play_step = None;
    if let Some(mut authoring) = world.get_resource_mut::<super::SeqAuthoring>() {
        authoring.media = media;
    }
}

/// Where one section's voice emits from this apply and how it is heard there.
#[derive(Clone, Copy, Debug)]
struct Heard {
    position: Vec3,
    pan: f32,
    gain: f32,
    doppler: f32,
    velocity: Option<Vec3>,
    /// The occlusion target amount (`None` when the section does not occlude).
    occlusion: Option<f32>,
}

/// Starts, keeps and stops the voices of `jobs` at sequence time `time` (see the module docs);
/// `ahead` are the pitch-preserving sections to render ahead.
pub(super) fn sync_sounds(
    world: &mut World,
    player: Entity,
    time: f64,
    jobs: Vec<SoundJob>,
    ahead: Vec<StretchAhead>,
    media: &mut PreviewMedia,
) {
    let play_step = media.play_step.take();
    // A forward tick plays on whatever its length; a backward move under Play is a loop wrap.
    let continuing = play_step == Some(PlayStep::Forward) && media.last_time.is_some_and(|l| time >= l);
    // The tick's length (0 for anything but playing on): what velocities and smoothing step by.
    let step = if continuing {
        media.last_time.map_or(0.0, |l| time - l)
    } else {
        0.0
    };
    let moved = media.last_time != Some(time);
    media.last_time = Some(time);
    if media.held {
        if play_step.is_none() && !moved {
            // The transport is stopped: nothing plays until it runs or the playhead moves.
            media.sounds_json.clear();
            return;
        }
        media.held = false;
    }
    if play_step == Some(PlayStep::Reverse) {
        // Reverse play is silent: stop what plays and hold (a re-apply at the same time, such as
        // an edit or the state call, starts nothing); the next forward tick or move starts again.
        let last_time = media.last_time;
        stop_sounds(world, media);
        media.last_time = last_time;
        media.held = true;
        return;
    }
    media.stretches.poll();
    media.stretches.fresh = false;
    for later in ahead {
        if let Some(asset) = sound_asset(world, media, &later.sound) {
            let key = StretchKey::new(
                &later.sound,
                later.section_offset,
                later.section_source_secs,
                later.rate,
            );
            let _ = media.stretches.get(&key, &asset);
        }
    }
    if jobs.is_empty() && media.voices.is_empty() {
        media.sounds_json.clear();
        return;
    }
    let _span = tracing::debug_span!(target: "regia_editor", "seq_preview_sounds").entered();
    // The listeners: the output's, else the editor camera (with its velocity while playing on).
    let output: Vec<(Option<Entity>, ListenerFrame)> = with_output(world, |audio| audio.listeners().to_vec());
    // The output always holds at least one listener: with none set it is the default one at the
    // origin (`Audio::set_listeners`), which the camera replaces.
    let unset = output
        .iter()
        .all(|(entity, frame)| entity.is_none() && *frame == ListenerFrame::default());
    let camera = world
        .get_resource::<crate::viewport::EditorCamera>()
        .map(|c| (c.eye(), c.forward()));
    let (listed, eye): (Vec<(Option<Entity>, ListenerFrame)>, Option<Vec3>) = match camera {
        Some((eye, forward)) if unset => {
            let mut frame = ListenerFrame::looking_at(eye, forward);
            if step > 0.0
                && let Some(last) = media.last_eye
            {
                let v = (eye - last) / step as f32;
                if v.is_finite() && v.length() <= LISTENER_CUT_SPEED {
                    frame.velocity = v;
                }
            }
            (vec![(None, frame)], Some(eye))
        }
        _ => (output, None),
    };
    media.last_eye = eye;
    let listeners: Vec<ListenerFrame> = listed.iter().map(|(_, l)| *l).collect();
    let tuning: OcclusionTuning = world
        .get_resource::<AudioOcclusion>()
        .map(|o| o.tuning)
        .unwrap_or_default();
    if jobs
        .iter()
        .any(|j| j.spatial.as_ref().is_some_and(|s| s.occlusion))
    {
        ensure_mirror(world, media);
    }
    // Where each spatial section emits from, and how it is heard.
    let mut heard: Vec<Option<Heard>> = Vec::with_capacity(jobs.len());
    for job in &jobs {
        let Some(spatial) = job.spatial.as_ref() else {
            heard.push(None);
            continue;
        };
        let bound = match &spatial.source {
            SoundSource::Binding(_) => job
                .emitter
                .as_deref()
                .and_then(|name| super::resolve_binding(world, player, name)),
            SoundSource::Position(_) => None,
        };
        let (position, rotation) = match &spatial.source {
            SoundSource::Position(p) => (Vec3::from_array(*p), Quat::IDENTITY),
            SoundSource::Binding(_) => bound
                .and_then(|e| entity_pose(world, e))
                .unwrap_or((Vec3::ZERO, Quat::IDENTITY)),
        };
        let velocity = (spatial.doppler > 0.0).then(|| match (job.slope, bound) {
            (Some(slope), Some(e)) => {
                world_velocity(world.get::<GlobalTransform>(e), world.get::<Transform>(e), slope)
            }
            (Some(slope), None) => slope,
            (None, Some(_)) if step > 0.0 => media
                .voices
                .get(&job.path)
                .and_then(|v| v.last_position)
                .map_or(Vec3::ZERO, |last| (position - last) / step as f32),
            _ => Vec3::ZERO,
        });
        let frame = spatial_frame_moving(
            spatial,
            position,
            rotation,
            velocity.unwrap_or(Vec3::ZERO),
            &listeners,
        );
        let occlusion = spatial.occlusion.then(|| {
            let index = nearest_listener(&listeners, position);
            let from = index
                .and_then(|i| listeners.get(i))
                .map_or(Vec3::ZERO, |l| l.position);
            let Some(mirror) = media.occlusion.as_ref() else {
                return 0.0;
            };
            if (position - from).length() >= spatial.max_distance {
                return 0.0;
            }
            let mut ignore = Vec::new();
            if let Some(e) = bound {
                mirrored_ancestors(world, mirror, e, &mut ignore);
            }
            if let Some(listener) = index.and_then(|i| listed.get(i)).and_then(|(e, _)| *e) {
                mirrored_ancestors(world, mirror, listener, &mut ignore);
            }
            tuning.amount_for_obstacles(obstacles(mirror, from, position, &ignore))
        });
        heard.push(Some(Heard {
            position,
            pan: frame.pan,
            gain: frame.gain,
            doppler: frame.pitch_ratio,
            velocity,
            occlusion,
        }));
    }
    let key_of = |job: &SoundJob| VoiceKey {
        sound: job.sound.clone(),
        bus: job.bus.clone(),
        rate: job.rate,
        spatial: job.spatial.is_some(),
        stretched: stretches(job.preserve, job.rate).0,
        occluding: job.spatial.as_ref().is_some_and(|s| s.occlusion),
    };
    // Voices whose section ended, changed, or whose clock jumped stop; the rest play on.
    let ended: Vec<Vec<usize>> = media
        .voices
        .iter()
        .filter(|(path, voice)| {
            let keep = jobs.iter().any(|job| {
                &job.path == *path
                    && voice.key == key_of(job)
                    && (voice.anchor - (job.position - time * job.rate)).abs() < ANCHOR_TOLERANCE
            }) && (continuing || !moved);
            !keep
        })
        .map(|(path, _)| path.clone())
        .collect();
    let ended: Vec<SoundInstanceId> = ended
        .iter()
        .filter_map(|path| media.voices.remove(path))
        .map(|voice| voice.id)
        .collect();
    // Only the sections that start a voice now need their sound (and their stretch).
    enum Start {
        Play {
            asset: SoundAsset,
            offset: f64,
            /// Where the sound ends, in section positions.
            end: f64,
        },
        Pending,
        Failed(String),
        Missing,
        Running,
    }
    let starts: Vec<Start> = jobs
        .iter()
        .map(|job| {
            if media.voices.contains_key(&job.path) {
                return Start::Running;
            }
            let Some(asset) = sound_asset(world, media, &job.sound) else {
                return Start::Missing;
            };
            if !stretches(job.preserve, job.rate).0 {
                return Start::Play {
                    end: asset.duration_secs(),
                    offset: job.position,
                    asset,
                };
            }
            let key = StretchKey::new(&job.sound, job.section_offset, job.section_source_secs, job.rate);
            match media.stretches.get(&key, &asset) {
                StretchState::Ready(stretched) => Start::Play {
                    end: job.section_offset + stretched.duration_secs() * job.rate,
                    offset: (job.position - job.section_offset) / job.rate,
                    asset: stretched,
                },
                StretchState::Pending => Start::Pending,
                StretchState::Failed(reason) => Start::Failed(reason),
            }
        })
        .collect();
    let mut sounds_json = Vec::with_capacity(jobs.len());
    with_output(world, |audio| {
        for id in ended {
            audio.stop_voice(id, DECLICK_FADE);
            media.stopped += 1;
        }
        for ((job, heard), start) in jobs.iter().zip(heard).zip(starts) {
            let key = key_of(job);
            let out_of_range = stretches(job.preserve, job.rate).1;
            let (pan, gain, doppler) = heard.map_or((0.0, 1.0, 1.0), |h| (h.pan, h.gain, h.doppler));
            let pitch = if key.stretched {
                doppler
            } else {
                job.rate as f32 * doppler
            };
            let amount = match (heard.and_then(|h| h.occlusion), media.voices.get(&job.path)) {
                (Some(target), Some(running)) => tuning.smooth(running.occlusion, target, step as f32),
                (Some(target), None) => target,
                (None, _) => 0.0,
            };
            let occluding = heard.is_some_and(|h| h.occlusion.is_some());
            let cutoff = occluding.then(|| tuning.cutoff_hz(amount) as f32);
            let occlusion_db = if occluding {
                regia_audio::gain_to_db(tuning.gain(amount))
            } else {
                0.0
            };
            let db = job.volume_db
                + fade_db(job.fade)
                + if heard.is_some() {
                    regia_audio::gain_to_db(gain).max(-100.0)
                } else {
                    0.0
                }
                + occlusion_db;
            let mut error: Option<String> = out_of_range.then(|| "stretch_rate".to_string());
            if media
                .voices
                .get(&job.path)
                .is_some_and(|voice| job.position >= voice.duration)
                && let Some(voice) = media.voices.remove(&job.path)
            {
                // The sound has ended inside its section: not playing any more.
                audio.stop_voice(voice.id, DECLICK_FADE);
                media.stopped += 1;
                error = Some("past_end".to_string());
            } else if let Some(voice) = media.voices.get_mut(&job.path) {
                if (voice.volume_db - db).abs() > 0.01 {
                    audio.set_voice_volume_db(voice.id, db, 0.0);
                    voice.volume_db = db;
                }
                if heard.is_some() && (voice.pan - pan).abs() > 1.0e-3 {
                    audio.set_voice_pan(voice.id, pan, 0.0);
                    voice.pan = pan;
                }
                if (voice.pitch - pitch).abs() > 1.0e-6 {
                    // Over the tick: the pitch glides with the motion instead of stepping.
                    audio.set_voice_pitch(voice.id, pitch, step as f32);
                    voice.pitch = pitch;
                }
                if let (Some(hz), Some(last)) = (cutoff, voice.cutoff_hz)
                    && (hz - last).abs() > last * 0.01
                {
                    audio.set_voice_lowpass(voice.id, hz, step as f32);
                    voice.cutoff_hz = Some(hz);
                }
                voice.occlusion = amount;
                voice.last_position = heard.map(|h| h.position);
            } else {
                match start {
                    Start::Missing => error = Some("unknown_sound".to_string()),
                    Start::Pending => error = Some("stretch_pending".to_string()),
                    Start::Failed(reason) => error = Some(reason),
                    Start::Running => {}
                    Start::Play { end, .. } if job.position >= end => {
                        error = Some("past_end".to_string());
                    }
                    Start::Play { asset, offset, end } => {
                        let params = PlaybackParams {
                            bus: job.bus.clone(),
                            volume_db: db,
                            pitch,
                            panning: heard.map(|_| pan),
                            start_offset_secs: offset.max(0.0),
                            ..PlaybackParams::default()
                        };
                        let started = match cutoff {
                            Some(hz) => audio.start_asset_lowpass(&asset, params, hz),
                            None => audio.start_asset(&asset, params),
                        };
                        match started {
                            Ok(id) => {
                                media.voices.insert(
                                    job.path.clone(),
                                    PreviewVoice {
                                        id,
                                        key: key.clone(),
                                        duration: end,
                                        anchor: job.position - time * job.rate,
                                        volume_db: db,
                                        pan,
                                        pitch,
                                        occlusion: amount,
                                        cutoff_hz: cutoff,
                                        last_position: heard.map(|h| h.position),
                                    },
                                );
                                media.started += 1;
                            }
                            Err(e) => error = Some(e.to_string()),
                        }
                    }
                }
            }
            sounds_json.push(json!({
                "path": job.path,
                "track": job.track,
                "sound": job.sound,
                "position": job.position,
                "spatial": job.spatial.is_some(),
                "emitter": heard.map(|h| h.position.to_array()),
                "volume_db": db,
                "pan": pan,
                "pitch": pitch,
                "doppler": doppler,
                "velocity": heard.and_then(|h| h.velocity).map(|v| v.to_array()),
                "occlusion": amount,
                "cutoff_hz": cutoff,
                "stretched": key.stretched,
                "playing": media.voices.contains_key(&job.path),
                "error": error,
            }));
        }
    });
    media.sounds_json = sounds_json;
}

/// Stops every voice the preview runs.
pub(super) fn stop_sounds(world: &mut World, media: &mut PreviewMedia) {
    media.last_time = None;
    media.last_eye = None;
    media.sounds_json.clear();
    if media.voices.is_empty() {
        return;
    }
    let voices = std::mem::take(&mut media.voices);
    with_output(world, |audio| {
        for voice in voices.into_values() {
            audio.stop_voice(voice.id, DECLICK_FADE);
            media.stopped += 1;
        }
    });
}

// ---- video ----

/// The media files the preview opens videos through: the edit world's `MediaFiles`, else the
/// project root's.
fn media_files(world: &World) -> Option<MediaFiles> {
    world
        .get_resource::<MediaFiles>()
        .cloned()
        .or_else(|| crate::seq_curve_panel::project_root(world).map(MediaFiles::with_root))
}

/// Closes the preview's session on its entity: a fresh `VideoPlayerState`, or none when the entity
/// had none. The authored `VideoPlayer` was never touched.
fn release_video(world: &mut World, running: PreviewVideo) {
    let entity = running.entity;
    if let Some(mut state) = world.get_mut::<VideoPlayerState>(entity) {
        state.core_mut().close();
    }
    if let Ok(mut e) = world.get_entity_mut(entity) {
        if running.had_state {
            e.insert(VideoPlayerState::default());
        } else {
            e.remove::<VideoPlayerState>();
        }
    }
}

/// The session settings of a preview session: blocking for its first decode, silent (the sound
/// sections are previewed), no loop, always visible.
fn session_player(source: &str) -> VideoPlayer {
    VideoPlayer {
        source: source.to_string(),
        autoplay: false,
        // Blocking session: `open_player_pooled` decodes on this thread for this clock.
        clock: ClockMode::Lockstep,
        loop_mode: LoopMode::Off,
        // The section's own sound is not previewed (the sound sections are).
        volume: 0.0,
        visibility: VisibilityPolicy::Always,
        ..VideoPlayer::default()
    }
}

/// Opens `source` in a fresh session for `entity`, decoded on the time the preview writes.
/// `Err` carries the media error code (nothing is left open).
fn open_session(
    files: &MediaFiles,
    settings: &MediaSettings,
    entity: Entity,
    source: &str,
) -> Result<VideoPlayerState, String> {
    let _span = tracing::debug_span!(target: "regia_editor", "seq_preview_video_open").entered();
    let mut state = VideoPlayerState::default();
    open_player_pooled(
        entity,
        &session_player(source),
        &mut state,
        files,
        settings,
        (0, 0),
        None,
    );
    let error = state.core().last_error().map(|e| e.code().to_string());
    if let Some(code) = error {
        tracing::debug!(target: "regia_editor", "sequence preview: video {source} did not open: {code}");
        state.core_mut().close();
        return Err(code);
    }
    state.core_mut().set_clock(ClockMode::External);
    Ok(state)
}

/// Opens `job`'s source in a session on the bound entity's `VideoPlayerState` (the authored
/// `VideoPlayer` stays as it is). `Err` carries the media error code (nothing is left open).
fn open_video(world: &mut World, entity: Entity, job: &VideoJob) -> Result<PreviewVideo, String> {
    let Some(files) = media_files(world) else {
        return Err("no_project".to_string());
    };
    if world.get_entity(entity).is_err() {
        return Err("target_gone".to_string());
    }
    let settings = world.get_resource::<MediaSettings>().cloned().unwrap_or_default();
    let running = PreviewVideo {
        entity,
        source: job.video.clone(),
        had_state: world.get::<VideoPlayerState>(entity).is_some(),
        last_ns: 0,
    };
    let state = open_session(&files, &settings, entity, &job.video)?;
    world.entity_mut(entity).insert(state);
    Ok(running)
}

/// Shows the picture of `job`'s source time. Returns the frame index and presentation time, or
/// the media error code.
fn show_video(
    world: &mut World,
    running: &mut PreviewVideo,
    job: &VideoJob,
) -> Result<(Option<u64>, Option<i64>), String> {
    let Some(mut state) = world.get_mut::<VideoPlayerState>(running.entity) else {
        return Err("target_gone".to_string());
    };
    let core = state.core_mut();
    if core.state() == PlayState::Opening {
        core.tick(ClockInput::Time { t_ns: 0 });
    }
    if core.state() == PlayState::Failed {
        return Err(core
            .last_error()
            .map_or("decode_failed", |e| e.code())
            .to_string());
    }
    let t_ns = media_time_ns(job.position, job.looping, core.duration());
    let moved = t_ns as f64 / 1e9 - running.last_ns as f64 / 1e9;
    if moved < 0.0 || moved > FORWARD_SEEK {
        core.reposition(t_ns);
    }
    running.last_ns = t_ns;
    core.show_at(t_ns, true, VIDEO_BUDGET);
    let shown = (core.frame_index(), core.presented().map(|p| p.pts_ns));
    state.set_external_time(t_ns as f64 / 1e9);
    Ok(shown)
}

fn media_fit(fit: VideoFit) -> Fit {
    match fit {
        VideoFit::Letterbox => Fit::Letterbox,
        VideoFit::Fill => Fit::Fill,
        VideoFit::Stretch => Fit::Stretch,
    }
}

fn video_slot(slot: VideoScreenSlot) -> VideoSlot {
    match slot {
        VideoScreenSlot::Albedo => VideoSlot::Albedo,
        VideoScreenSlot::Emissive => VideoSlot::Emissive,
    }
}

/// Opens `job`'s source in a session on the stage: a fullscreen layer, or a stand-in carrying the
/// bound mesh's renderer with a `VideoMaterial` on the session.
fn open_staged(
    world: &mut World,
    media: &mut PreviewMedia,
    job: &VideoJob,
    mesh: Option<Entity>,
) -> Result<StagedVideo, String> {
    let Some(files) = media_files(world) else {
        return Err("no_project".to_string());
    };
    let settings = world.get_resource::<MediaSettings>().cloned().unwrap_or_default();
    let own = match mesh {
        Some(entity) => Some(
            world
                .get::<MeshRenderer>(entity)
                .cloned()
                .ok_or_else(|| "no_mesh".to_string())?,
        ),
        None => None,
    };
    let stage = media.stage.get_or_insert_with(World::new);
    let session = stage.spawn(session_player(&job.video)).id();
    let state = match open_session(&files, &settings, session, &job.video) {
        Ok(state) => state,
        Err(code) => {
            let _ = stage.despawn(session);
            return Err(code);
        }
    };
    stage.entity_mut(session).insert(state);
    let mut staged = StagedVideo {
        session: PreviewVideo {
            entity: session,
            source: job.video.clone(),
            had_state: false,
            last_ns: 0,
        },
        kind: job.target.kind(),
        fullscreen: None,
        mesh: None,
    };
    match (&job.target, mesh.zip(own)) {
        (VideoTarget::Fullscreen { layer, fit }, _) => {
            stage.entity_mut(session).insert(VideoFullscreen {
                fit: media_fit(*fit),
                layer: *layer,
                skippable: false,
            });
            staged.fullscreen = Some((*layer, *fit));
        }
        (VideoTarget::Screen { slot, .. }, Some((entity, renderer))) => {
            let own = renderer.material.clone();
            let stand_in = stage
                .spawn((
                    renderer,
                    VideoMaterial {
                        player: session,
                        slot: video_slot(*slot),
                    },
                ))
                .id();
            staged.mesh = Some(StagedMesh {
                entity,
                own,
                stand_in,
                bound: None,
            });
        }
        _ => {}
    }
    Ok(staged)
}

/// Closes a staged session; a mesh gets its own material back (when it still shows ours).
fn release_staged(world: &mut World, stage: Option<&mut World>, staged: StagedVideo) {
    if let Some(stage) = stage {
        if let Some(mut state) = stage.get_mut::<VideoPlayerState>(staged.session.entity) {
            state.core_mut().close();
        }
        let _ = stage.despawn(staged.session.entity);
        if let Some(mesh) = &staged.mesh {
            let _ = stage.despawn(mesh.stand_in);
        }
    }
    if let Some(mesh) = staged.mesh
        && let Some(mut renderer) = world.get_mut::<MeshRenderer>(mesh.entity)
        && mesh
            .bound
            .as_ref()
            .is_some_and(|b| Arc::ptr_eq(b, &renderer.material))
    {
        renderer.material = mesh.own;
    }
}

/// Opens, drives and gives back the targets of `jobs` (see the module docs).
pub(super) fn sync_videos(world: &mut World, player: Entity, jobs: Vec<VideoJob>, media: &mut PreviewMedia) {
    if jobs.is_empty() && media.videos.is_empty() && media.staged.is_empty() {
        media.videos_json.clear();
        return;
    }
    let _span = tracing::debug_span!(target: "regia_editor", "seq_preview_videos").entered();
    // Which entity each section shows on; two sections on one entity: the later one has it.
    let mut wanted: Vec<(VideoJob, Option<Entity>, Option<&'static str>)> = Vec::new();
    for job in jobs {
        let entity = job
            .binding
            .as_deref()
            .and_then(|name| super::resolve_binding(world, player, name));
        let problem = match (&job.target, entity) {
            (VideoTarget::Player(_) | VideoTarget::Screen { .. }, Some(_)) => None,
            (VideoTarget::Player(_) | VideoTarget::Screen { .. }, None) => Some("unbound"),
            (VideoTarget::Fullscreen { .. }, _) => None,
            (VideoTarget::UiImage(_), _) => Some("target_not_previewed"),
        };
        if problem.is_none() && entity.is_some() {
            wanted.retain(|(_, e, p)| !(p.is_none() && *e == entity));
        }
        wanted.push((job, entity, problem));
    }
    let staged_target = |job: &VideoJob| {
        matches!(
            job.target,
            VideoTarget::Fullscreen { .. } | VideoTarget::Screen { .. }
        )
    };
    // Sections that ended or now point elsewhere give their player back.
    let stale: Vec<Vec<usize>> = media
        .videos
        .iter()
        .filter(|(path, running)| {
            !wanted.iter().any(|(job, entity, problem)| {
                problem.is_none()
                    && !staged_target(job)
                    && &&job.path == path
                    && job.video == running.source
                    && *entity == Some(running.entity)
            })
        })
        .map(|(path, _)| path.clone())
        .collect();
    for path in stale {
        if let Some(running) = media.videos.remove(&path) {
            release_video(world, running);
        }
    }
    let stale: Vec<Vec<usize>> = media
        .staged
        .iter()
        .filter(|(path, staged)| {
            !wanted.iter().any(|(job, entity, problem)| {
                problem.is_none()
                    && staged_target(job)
                    && &&job.path == path
                    && job.video == staged.session.source
                    && job.target.kind() == staged.kind
                    && staged.fullscreen.is_none_or(
                        |f| matches!(job.target, VideoTarget::Fullscreen { layer, fit } if (layer, fit) == f),
                    )
                    && staged.mesh.as_ref().map(|m| m.entity) == entity.filter(|_| staged.mesh.is_some())
            })
        })
        .map(|(path, _)| path.clone())
        .collect();
    for path in stale {
        if let Some(staged) = media.staged.remove(&path) {
            release_staged(world, media.stage.as_mut(), staged);
        }
    }
    let mut out = Vec::with_capacity(wanted.len());
    let mut published: Vec<(usize, Vec<usize>)> = Vec::new();
    for (job, entity, problem) in wanted {
        let mut shown = (None, None);
        let mut error = problem.map(str::to_string);
        if problem.is_none() && staged_target(&job) {
            if !media.staged.contains_key(&job.path) {
                media.video_opens += 1;
                let mesh = entity.filter(|_| matches!(job.target, VideoTarget::Screen { .. }));
                match open_staged(world, media, &job, mesh) {
                    Ok(staged) => {
                        media.staged.insert(job.path.clone(), staged);
                    }
                    Err(code) => error = Some(code),
                }
            }
            if let (Some(staged), Some(stage)) = (media.staged.get_mut(&job.path), media.stage.as_mut()) {
                match show_video(stage, &mut staged.session, &job) {
                    Ok(s) => shown = s,
                    Err(code) => error = Some(code),
                }
                published.push((out.len(), job.path.clone()));
            }
        } else if let (None, Some(entity)) = (problem, entity) {
            if !media.videos.contains_key(&job.path) {
                media.video_opens += 1;
                match open_video(world, entity, &job) {
                    Ok(running) => {
                        media.videos.insert(job.path.clone(), running);
                    }
                    Err(code) => error = Some(code),
                }
            }
            if let Some(running) = media.videos.get_mut(&job.path) {
                match show_video(world, running, &job) {
                    Ok(s) => shown = s,
                    Err(code) => error = Some(code),
                }
            }
        }
        let message = (problem == Some("target_not_previewed")).then_some(
            "a UI image target shows in the running game's UI; the edit world draws no game UI (preview it with Play)",
        );
        out.push(json!({
            "path": job.path,
            "track": job.track,
            "video": job.video,
            "target": job.target.kind(),
            "entity": entity.map(Entity::to_bits),
            "source_time": job.position,
            "frame": shown.0,
            "pts_ns": shown.1,
            "error": error,
            "message": message,
        }));
    }
    publish_staged(world, media, &published, &mut out);
    media.videos_json = out;
}

/// Runs the frame publisher over the stage, hands each mesh section's bound material to its mesh,
/// and writes each staged section's publish counter into `out`.
fn publish_staged(
    world: &mut World,
    media: &mut PreviewMedia,
    published: &[(usize, Vec<usize>)],
    out: &mut [Value],
) {
    let Some(stage) = media.stage.as_mut() else {
        return;
    };
    if media.staged.is_empty() {
        media.stage = None;
        return;
    }
    let _span = tracing::debug_span!(target: "regia_editor", "seq_preview_video_publish").entered();
    if let Err(error) = regia_media::publish_frames_once(stage) {
        tracing::debug!(target: "regia_editor", "sequence preview: publish: {error}");
    }
    let statuses = stage
        .get_resource::<MediaFrameSource>()
        .map(MediaFrameSource::statuses)
        .unwrap_or_default();
    for staged in media.staged.values_mut() {
        let Some(mesh) = staged.mesh.as_mut() else {
            continue;
        };
        let Some(material) = stage
            .get::<MeshRenderer>(mesh.stand_in)
            .map(|r| r.material.clone())
        else {
            continue;
        };
        if Arc::ptr_eq(&material, &mesh.own) {
            continue;
        }
        if let Some(mut renderer) = world.get_mut::<MeshRenderer>(mesh.entity)
            && !Arc::ptr_eq(&renderer.material, &material)
        {
            renderer.material = material.clone();
        }
        mesh.bound = Some(material);
    }
    for (index, path) in published {
        let Some(staged) = media.staged.get(path) else {
            continue;
        };
        let id = staged.session.entity.to_bits();
        let publishes = statuses
            .iter()
            .find(|s| s.player == id)
            .map_or(0, |s| s.publishes);
        if let Some(Value::Object(entry)) = out.get_mut(*index) {
            entry.insert("publishes".into(), json!(publishes));
        }
    }
}

/// Closes every preview video session (Stop, the end of the preview).
pub(super) fn release_videos(world: &mut World, media: &mut PreviewMedia) {
    media.videos_json.clear();
    for running in std::mem::take(&mut media.videos).into_values() {
        release_video(world, running);
    }
    for staged in std::mem::take(&mut media.staged).into_values() {
        release_staged(world, media.stage.as_mut(), staged);
    }
    media.stage = None;
}

/// The picture a fullscreen preview section shows, for the viewport's overlay.
#[derive(Clone, Debug)]
pub struct PreviewFrame {
    /// Publish serial (changes with each new picture).
    pub serial: u64,
    /// Picture width, pixels.
    pub width: u32,
    /// Picture height, pixels.
    pub height: u32,
    /// sRGB RGBA8 pixels, row-major.
    pub rgba: Arc<[u8]>,
    /// How the picture fills the viewport.
    pub fit: VideoFit,
    /// Order among fullscreen sections (the highest is drawn).
    pub layer: i32,
}

/// The top fullscreen section's published picture while the sequence preview shows one (`None`
/// otherwise): the viewport draws it over the scene.
#[must_use]
pub fn preview_overlay_frame(world: &World) -> Option<PreviewFrame> {
    let media = &world.get_resource::<super::SeqAuthoring>()?.media;
    let stage = media.stage.as_ref()?;
    let source = stage.get_resource::<MediaFrameSource>()?;
    media
        .staged
        .values()
        .filter_map(|s| s.fullscreen.map(|(layer, fit)| (s.session.entity, layer, fit)))
        .max_by_key(|(_, layer, _)| *layer)
        .and_then(|(entity, layer, fit)| {
            let (serial, width, height, rgba) = source.rgba(entity.to_bits())?;
            Some(PreviewFrame {
                serial,
                width,
                height,
                rgba,
                fit,
                layer,
            })
        })
}

/// Draws the sequence preview's fullscreen picture over a pane's `rect` (letterboxed, filled or
/// stretched as the section says, like the runtime's fullscreen layer). The texture is uploaded
/// once per published picture.
pub fn paint_preview_overlay(ui: &egui::Ui, rect: egui::Rect, world: &World) {
    let Some(frame) = preview_overlay_frame(world) else {
        return;
    };
    let (w, h) = (frame.width as usize, frame.height as usize);
    if w == 0 || h == 0 || frame.rgba.len() != w * h * 4 {
        return;
    }
    let id = egui::Id::new("seq-preview-fullscreen");
    let cached = ui
        .ctx()
        .data_mut(|d| d.get_temp::<(u64, egui::TextureHandle)>(id))
        .filter(|(serial, _)| *serial == frame.serial)
        .map(|(_, texture)| texture);
    let texture = match cached {
        Some(texture) => texture,
        None => {
            let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &frame.rgba);
            let texture =
                ui.ctx()
                    .load_texture("seq-preview-fullscreen", image, egui::TextureOptions::LINEAR);
            ui.ctx()
                .data_mut(|d| d.insert_temp(id, (frame.serial, texture.clone())));
            texture
        }
    };
    let fit = regia_media::present::fit_rect(
        frame.width as f32 / frame.height as f32,
        rect.width(),
        rect.height(),
        media_fit(frame.fit),
    );
    let at = egui::Rect::from_min_size(rect.min + egui::vec2(fit.x, fit.y), egui::vec2(fit.w, fit.h));
    let uv = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0));
    ui.painter()
        .with_clip_rect(rect)
        .image(texture.id(), at, uv, egui::Color32::WHITE);
}
