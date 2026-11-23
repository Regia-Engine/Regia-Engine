// SPDX-License-Identifier: MPL-2.0
//! The take recorder: play the game (or PIE) and capture what happened into a `.seq.ron` v3
//! sequence (ADR-2637, book `agents/sequence-take-recorder.md`).
//!
//! A recording picks **bindings** (a scene guid, an entity, or the active camera) and, for each,
//! **channels**:
//!
//! | Channel | Tracks it writes (target = the binding name) |
//! |---|---|
//! | `transform` | `transform.position` (Vec3, metres; world metres for a root, as the player applies it), `transform.rotation` (Vec3, Euler degrees `[pitch, yaw, roll]`, unwrapped), `transform.scale` (Vec3) |
//! | `camera` | the same position and rotation plus `camera.fov` (Float, degrees); a camera cut becomes a step in the keys and a marker |
//! | `animation` | a `Clip` track of sections: the clip the binding's animator plays most and its clip time |
//! | `property:<path>` | one keyed track of a [`PropertyReaders`] path (`visible`, `light.intensity`, ...) or a reflected field `reflect:<Component>.<field.path>` |
//!
//! and, for the whole take, the **audio cues** gameplay announces with a [`TakeCue`] message (one
//! `Audio` section each). The [`TakeRecorder`] resource samples every binding once per gameplay
//! frame in `PostUpdate`, after the sequencer applied its tracks and physics synced its bodies and
//! before transforms propagate, on the same whole-nanosecond gameplay clock a
//! [`super::SequencePlayer`] runs on. Under fixed stepping (`play.step`, PIE stepping, render-out,
//! tests at a manual delta) that is one sample per fixed step.
//!
//! Stopping turns the samples into keys with no new track kinds and no format bump: linear keys
//! reduced per channel by the Ramer-Douglas-Peucker polyline simplification against a stated
//! error ([`TakeTolerance`]), so a still object writes two keys and not one per frame, then
//! checked against the engine's own sampler at every recorded instant (a key is added where the
//! check fails). Camera cuts end a segment with a [`super::Ease::Step`] key so playback cuts
//! too. A take is written as `<name>_take<N>.seq.ron` next to the source sequence
//! ([`save_take`]); the source gains a sub-sequence track that plays the take, so recording into
//! an existing sequence adds a section and never overwrites a track.
//!
//! A binding that a playing sequence drives (one of its resolved targets, or the camera its shot
//! list holds) is left out of the take unless [`TakeConfig::include_driven`] is set: recording a
//! take while the sequence plays captures the game, not the sequence's own keys.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component as PathComponent, Path, PathBuf};

use regia_animation::Animator;
use regia_animation::debug::NodeDebugState;
use regia_app::prelude::*;
use regia_core::math::{EulerRot, Quat};
use regia_ecs::reflect::GetPath;
use regia_render::{Camera, DirectionalLight, PointLight, SpotLight};
use regia_scene::Transform;
use regia_scene::components::{EntityGuid, Visibility};
use regia_scene::streaming::OriginOffset;

use super::asset::{
    AudioSection, AudioSpatial, BoolKey, Channel, ClipSection, Ease, MAX_SECTIONS, Marker,
    SEQUENCE_FORMAT_VERSION, ScalarKey, Sequence, SoundSource, SubSection, Track, Vec3Key,
};
use super::player::{Binding, SequencePlayer, SequencerSet, nanos_to_seconds};
use super::sample::ChannelValue;

/// Most gameplay frames one take records (an hour at 60 Hz). The recorder stops sampling there
/// ([`TakeStatus::Full`]); stopping still writes what it has.
pub const MAX_TAKE_SAMPLES: usize = 216_000;

/// Most bindings one take records.
pub const MAX_TAKE_BINDINGS: usize = 256;

/// Most channels one binding records.
pub const MAX_TAKE_CHANNELS: usize = 32;

/// Most audio cues one take records; later cues are counted in [`TakeRecorder::dropped_cues`].
pub const MAX_TAKE_CUES: usize = 4096;

/// Highest take number [`save_take`] writes (`<name>_take9999.seq.ron`).
pub const MAX_TAKE_NUMBER: u32 = 9999;

/// Longest countdown, in gameplay frames.
pub const MAX_COUNTDOWN: u32 = 100_000;

/// Samples per Ramer-Douglas-Peucker window. The simplification is quadratic in the worst case,
/// so a long channel is reduced window by window, each window's ends kept as keys: at most one
/// key per window more than a whole-channel pass, and a bounded cost per window.
const REDUCE_WINDOW: usize = 2048;

/// The file name suffix of a take, before its number.
pub const TAKE_SUFFIX: &str = "_take";

/// The extension of a sequence file.
const SEQ_EXTENSION: &str = ".seq.ron";

/// The error a take may make, per channel kind, when its keys are played back (ADR-2637).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TakeTolerance {
    /// Metres of position error (distance).
    pub position: f32,
    /// Degrees of rotation error (the angle between the recorded and the replayed rotation).
    /// Each Euler component keeps a third of it, which bounds the angle.
    pub rotation: f32,
    /// Error of each scale component.
    pub scale: f32,
    /// Error of each component of a property track (and of `camera.fov`, in degrees).
    pub property: f32,
    /// Seconds of clip-time error before an animation section is split.
    pub clip_time: f32,
}

impl Default for TakeTolerance {
    fn default() -> Self {
        Self {
            position: 1.0e-3,
            rotation: 0.1,
            scale: 1.0e-3,
            property: 1.0e-4,
            clip_time: 1.0 / 120.0,
        }
    }
}

impl TakeTolerance {
    /// Every field finite and in `(0, 1000]`.
    pub fn validate(&self) -> Result<(), TakeError> {
        for (name, value) in [
            ("position", self.position),
            ("rotation", self.rotation),
            ("scale", self.scale),
            ("property", self.property),
            ("clip_time", self.clip_time),
        ] {
            if !(value.is_finite() && value > 0.0 && value <= 1000.0) {
                return Err(TakeError::BadTolerance(format!(
                    "tolerance `{name}` {value} must be in (0, 1000]"
                )));
            }
        }
        Ok(())
    }
}

/// Take recorder errors, each with a stable lowercase agent code ([`TakeError::code`]).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TakeError {
    /// A take is already counting down or recording.
    #[error("a take is already recording; stop it first")]
    Busy,
    /// A stopped take whose files could not be written is kept: save it (stop again with a
    /// path) or drop it (cancel) before recording another.
    #[error("a stopped take is waiting to be saved; save or cancel it first")]
    Unsaved,
    /// Nothing is recording.
    #[error("no take is recording")]
    NotRecording,
    /// The recording names no binding and no audio cues.
    #[error("a take needs at least one binding or audio cues")]
    NoBindings,
    /// Over one of the take's limits.
    #[error("{0}")]
    TooMany(String),
    /// A binding is malformed (an empty or duplicate name, no channels).
    #[error("{0}")]
    BadBinding(String),
    /// A channel name does not parse, or does not fit its binding.
    #[error("{0}")]
    BadChannel(String),
    /// A tolerance is out of range.
    #[error("{0}")]
    BadTolerance(String),
    /// A path is not a project-relative `.seq.ron`.
    #[error("{0}")]
    BadPath(String),
    /// The take recorded nothing to write (stopped before the first sample, or every binding was
    /// left out).
    #[error("the take recorded nothing: {0}")]
    Empty(String),
    /// The source sequence exists and does not load.
    #[error("the source sequence does not load: {0}")]
    SourceInvalid(String),
    /// The take or the updated source would not validate (a bug; reported, never written).
    #[error("the take does not validate: {0}")]
    Invalid(String),
    /// Every take number up to [`MAX_TAKE_NUMBER`] is taken.
    #[error("every take number up to {MAX_TAKE_NUMBER} is used next to `{0}`")]
    NumbersExhausted(String),
    /// Reading or writing a file failed.
    #[error("take file error: {0}")]
    Io(String),
}

impl TakeError {
    /// The lowercase code agents see (`take_busy`, `take_unsaved`, `take_not_recording`, `take_no_bindings`,
    /// `take_too_many`, `take_bad_binding`, `take_bad_channel`, `take_bad_tolerance`,
    /// `take_bad_path`, `take_empty`, `take_source_invalid`, `take_invalid`,
    /// `take_numbers_exhausted`, `take_io`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Busy => "take_busy",
            Self::Unsaved => "take_unsaved",
            Self::NotRecording => "take_not_recording",
            Self::NoBindings => "take_no_bindings",
            Self::TooMany(_) => "take_too_many",
            Self::BadBinding(_) => "take_bad_binding",
            Self::BadChannel(_) => "take_bad_channel",
            Self::BadTolerance(_) => "take_bad_tolerance",
            Self::BadPath(_) => "take_bad_path",
            Self::Empty(_) => "take_empty",
            Self::SourceInvalid(_) => "take_source_invalid",
            Self::Invalid(_) => "take_invalid",
            Self::NumbersExhausted(_) => "take_numbers_exhausted",
            Self::Io(_) => "take_io",
        }
    }
}

/// What one binding records.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TakeChannel {
    /// Position, rotation and scale.
    Transform,
    /// Position, rotation and field of view, with camera cuts.
    Camera,
    /// The clip the binding's animator plays most, and its clip time.
    Animation,
    /// One property path: a [`PropertyReaders`] path or `reflect:<Component>.<field.path>`.
    Property(String),
}

impl TakeChannel {
    /// The name agents use: `transform`, `camera`, `animation` or `property:<path>`.
    #[must_use]
    pub fn name(&self) -> String {
        match self {
            Self::Transform => "transform".into(),
            Self::Camera => "camera".into(),
            Self::Animation => "animation".into(),
            Self::Property(path) => format!("property:{path}"),
        }
    }

    /// Parses [`name`](Self::name); a bare `reflect:...` path is a property too.
    pub fn parse(name: &str) -> Result<Self, TakeError> {
        let property = |path: &str| {
            if path.is_empty() || path.len() > 256 {
                Err(TakeError::BadChannel(format!(
                    "channel `{name}`: the property path must be 1..=256 bytes"
                )))
            } else {
                Ok(Self::Property(path.to_string()))
            }
        };
        match name {
            "transform" => Ok(Self::Transform),
            "camera" => Ok(Self::Camera),
            "animation" => Ok(Self::Animation),
            _ => {
                if let Some(path) = name.strip_prefix("property:") {
                    property(path)
                } else if name.starts_with(REFLECT_PREFIX) {
                    property(name)
                } else {
                    Err(TakeError::BadChannel(format!(
                        "unknown channel `{name}`: use transform, camera, animation or property:<path>"
                    )))
                }
            }
        }
    }
}

/// What a take binding records from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TakeTarget {
    /// An entity, by scene guid or directly (as a [`super::SequenceBindings`] entry binds it).
    Binding(Binding),
    /// Whichever camera renders: the highest `order`, ties to the lowest entity id. A change of
    /// camera is a camera cut.
    ActiveCamera,
}

/// One binding of a take: the target name its tracks get, what it records from, and which
/// channels.
#[derive(Clone, Debug, PartialEq)]
pub struct TakeBinding {
    /// The track target name (bind it the same way to play the take back).
    pub name: String,
    /// What it records from.
    pub target: TakeTarget,
    /// The channels it records.
    pub channels: Vec<TakeChannel>,
}

/// What a recording captures and how its keys are reduced.
#[derive(Clone, Debug, PartialEq)]
pub struct TakeConfig {
    /// The bindings.
    pub bindings: Vec<TakeBinding>,
    /// `true` records [`TakeCue`] messages as audio sections.
    pub audio_cues: bool,
    /// Gameplay frames to wait after the start before the first sample (the editor's countdown).
    pub countdown: u32,
    /// `true` also records bindings a playing sequence drives.
    pub include_driven: bool,
    /// The key reduction's error bounds.
    pub tolerance: TakeTolerance,
    /// A camera that moves more than this many metres in one frame has cut (`None`: only a change
    /// of camera or a shot change of the sequence holding it is a cut).
    pub cut_distance: Option<f32>,
    /// The take sequence's display name (`save_take` sets it to the file stem).
    pub name: String,
    /// The take sequence's authoring frame rate.
    pub fps: f32,
}

impl Default for TakeConfig {
    fn default() -> Self {
        Self {
            bindings: Vec::new(),
            audio_cues: false,
            countdown: 0,
            include_driven: false,
            tolerance: TakeTolerance::default(),
            cut_distance: None,
            name: "take".into(),
            fps: 30.0,
        }
    }
}

impl TakeConfig {
    /// Checks the limits, names and tolerances.
    pub fn validate(&self) -> Result<(), TakeError> {
        if self.bindings.is_empty() && !self.audio_cues {
            return Err(TakeError::NoBindings);
        }
        if self.bindings.len() > MAX_TAKE_BINDINGS {
            return Err(TakeError::TooMany(format!(
                "{} bindings; the limit is {MAX_TAKE_BINDINGS}",
                self.bindings.len()
            )));
        }
        if self.countdown > MAX_COUNTDOWN {
            return Err(TakeError::TooMany(format!(
                "countdown {}; the limit is {MAX_COUNTDOWN} frames",
                self.countdown
            )));
        }
        if !(self.fps.is_finite() && self.fps > 0.0 && self.fps <= 1000.0) {
            return Err(TakeError::BadTolerance(format!(
                "fps {} must be in (0, 1000]",
                self.fps
            )));
        }
        if let Some(d) = self.cut_distance
            && !(d.is_finite() && d > 0.0)
        {
            return Err(TakeError::BadTolerance(format!(
                "cut_distance {d} must be more than zero"
            )));
        }
        self.tolerance.validate()?;
        let mut names = BTreeSet::new();
        for binding in &self.bindings {
            let name = &binding.name;
            if name.is_empty() || name.len() > 256 || name.contains('/') {
                return Err(TakeError::BadBinding(format!(
                    "binding name `{name}` must be 1..=256 bytes without `/`"
                )));
            }
            if !names.insert(name.as_str()) {
                return Err(TakeError::BadBinding(format!("duplicate binding name `{name}`")));
            }
            if binding.channels.is_empty() {
                return Err(TakeError::BadBinding(format!(
                    "binding `{name}` records no channel"
                )));
            }
            if binding.channels.len() > MAX_TAKE_CHANNELS {
                return Err(TakeError::TooMany(format!(
                    "binding `{name}` has {} channels; the limit is {MAX_TAKE_CHANNELS}",
                    binding.channels.len()
                )));
            }
            let unique: BTreeSet<&TakeChannel> = binding.channels.iter().collect();
            if unique.len() != binding.channels.len() {
                return Err(TakeError::BadChannel(format!(
                    "binding `{name}` names a channel twice"
                )));
            }
            if binding.channels.contains(&TakeChannel::Transform)
                && binding.channels.contains(&TakeChannel::Camera)
            {
                return Err(TakeError::BadChannel(format!(
                    "binding `{name}`: transform and camera both write the transform; pick one"
                )));
            }
        }
        Ok(())
    }
}

/// An audio cue gameplay fired, for the take recorder. Send it next to playing the sound (the
/// recorder cannot hear the mixer): while a take records with audio cues, each becomes an `Audio`
/// section at the frame's take time.
#[derive(Message, Clone, Debug, PartialEq)]
pub struct TakeCue {
    /// The sound's name in the audio library (what the `Audio` section plays back).
    pub sound: String,
    /// Seconds the section lasts (the sound's length, or how long it should play).
    pub length: f32,
    /// Volume, dB.
    pub volume_db: f32,
    /// Bus (`None`: `Master`).
    pub bus: Option<String>,
    /// Where it plays from in 3D (`None`: flat).
    pub source: Option<SoundSource>,
}

impl TakeCue {
    /// A flat cue of `sound` lasting `length` seconds at 0 dB on the master bus.
    #[must_use]
    pub fn new(sound: impl Into<String>, length: f32) -> Self {
        Self {
            sound: sound.into(),
            length,
            volume_db: 0.0,
            bus: None,
            source: None,
        }
    }
}

/// Where a recording is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TakeStatus {
    /// Nothing recording.
    #[default]
    Idle,
    /// Waiting out the countdown.
    Countdown,
    /// Sampling every gameplay frame.
    Recording,
    /// Reached [`MAX_TAKE_SAMPLES`]: no more samples; stop writes the take.
    Full,
}

impl TakeStatus {
    /// The lowercase name agents see (`idle`, `countdown`, `recording`, `full`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Countdown => "countdown",
            Self::Recording => "recording",
            Self::Full => "full",
        }
    }

    /// `true` while counting down, recording or full (a take to stop).
    #[must_use]
    pub fn is_active(self) -> bool {
        !matches!(self, Self::Idle)
    }
}

/// Reads one property of an entity for the take recorder: the inverse of a
/// [`super::PropertyApplier`].
pub type PropertyReader = fn(&World, Entity) -> Result<ChannelValue, String>;

/// The property readers, by property path: what a `property:<path>` channel samples. The
/// built-ins mirror the built-in appliers ([`super::PropertyAppliers`]); a host that registers an
/// applier for its own path registers the reader beside it. Paths starting `reflect:` need no
/// reader: they read the reflected field.
#[derive(Resource, Debug, Clone)]
pub struct PropertyReaders {
    readers: HashMap<String, PropertyReader>,
}

impl Default for PropertyReaders {
    fn default() -> Self {
        let mut readers = Self {
            readers: HashMap::new(),
        };
        readers.register("transform.position", read_position);
        readers.register("transform.rotation", read_rotation);
        readers.register("transform.scale", read_scale);
        readers.register("visible", read_visible);
        readers.register("light.intensity", read_light_intensity);
        readers.register("light.color", read_light_color);
        readers.register("camera.fov", read_camera_fov);
        readers
    }
}

impl PropertyReaders {
    /// Registers (or replaces) the reader of one property path.
    pub fn register(&mut self, property: impl Into<String>, reader: PropertyReader) {
        self.readers.insert(property.into(), reader);
    }

    /// The reader of one property path.
    #[must_use]
    pub fn get(&self, property: &str) -> Option<PropertyReader> {
        self.readers.get(property).copied()
    }

    /// Every registered path, sorted.
    #[must_use]
    pub fn paths(&self) -> Vec<&str> {
        let mut paths: Vec<&str> = self.readers.keys().map(String::as_str).collect();
        paths.sort_unstable();
        paths
    }
}

fn transform(world: &World, entity: Entity) -> Result<&Transform, String> {
    world
        .get::<Transform>(entity)
        .ok_or_else(|| "the target has no Transform component".to_string())
}

/// The translation as the `transform.position` applier writes it: world metres for a root (the
/// floating origin added back), parent-relative for a child.
fn position_of(world: &World, entity: Entity, transform: &Transform) -> [f32; 3] {
    let local = transform.translation.to_array();
    if world.get::<ChildOf>(entity).is_some() {
        return local;
    }
    match world.get_resource::<OriginOffset>() {
        Some(offset) => [
            (f64::from(local[0]) + offset.0[0]) as f32,
            (f64::from(local[1]) + offset.0[1]) as f32,
            (f64::from(local[2]) + offset.0[2]) as f32,
        ],
        None => local,
    }
}

/// A rotation as the `transform.rotation` applier reads it: Euler degrees `[pitch x, yaw y,
/// roll z]`, applied yaw then pitch then roll.
#[must_use]
pub fn rotation_to_euler_degrees(rotation: Quat) -> [f32; 3] {
    let (yaw, pitch, roll) = rotation.normalize().to_euler(EulerRot::YXZ);
    [pitch.to_degrees(), yaw.to_degrees(), roll.to_degrees()]
}

/// The inverse of [`rotation_to_euler_degrees`] (what the applier does).
#[must_use]
pub fn euler_degrees_to_rotation(euler: [f32; 3]) -> Quat {
    let [pitch, yaw, roll] = euler;
    Quat::from_euler(
        EulerRot::YXZ,
        yaw.to_radians(),
        pitch.to_radians(),
        roll.to_radians(),
    )
}

/// Each component moved by whole turns to lie within half a turn of `previous`, so a rotation
/// that crosses ±180° keeps going instead of interpolating the long way back.
#[must_use]
pub fn unwrap_degrees(euler: [f32; 3], previous: [f32; 3]) -> [f32; 3] {
    let mut out = euler;
    for (value, prev) in out.iter_mut().zip(previous) {
        let turns = ((prev - *value) / 360.0).round();
        *value += turns * 360.0;
    }
    out
}

fn read_position(world: &World, entity: Entity) -> Result<ChannelValue, String> {
    let t = transform(world, entity)?;
    Ok(ChannelValue::Vec3(position_of(world, entity, t)))
}

fn read_rotation(world: &World, entity: Entity) -> Result<ChannelValue, String> {
    Ok(ChannelValue::Vec3(rotation_to_euler_degrees(
        transform(world, entity)?.rotation,
    )))
}

fn read_scale(world: &World, entity: Entity) -> Result<ChannelValue, String> {
    Ok(ChannelValue::Vec3(transform(world, entity)?.scale.to_array()))
}

fn read_visible(world: &World, entity: Entity) -> Result<ChannelValue, String> {
    // No Visibility component shows the entity (the applier inserts one only when it writes).
    let visible = world
        .get::<Visibility>(entity)
        .is_none_or(|v| *v != Visibility::Hidden);
    Ok(ChannelValue::Bool(visible))
}

fn read_light_intensity(world: &World, entity: Entity) -> Result<ChannelValue, String> {
    if let Some(light) = world.get::<PointLight>(entity) {
        Ok(ChannelValue::Float(light.intensity))
    } else if let Some(light) = world.get::<SpotLight>(entity) {
        Ok(ChannelValue::Float(light.intensity))
    } else if let Some(light) = world.get::<DirectionalLight>(entity) {
        Ok(ChannelValue::Float(light.illuminance))
    } else {
        Err("the target has no light component".into())
    }
}

fn read_light_color(world: &World, entity: Entity) -> Result<ChannelValue, String> {
    if let Some(light) = world.get::<PointLight>(entity) {
        Ok(ChannelValue::Vec3(light.color))
    } else if let Some(light) = world.get::<SpotLight>(entity) {
        Ok(ChannelValue::Vec3(light.color))
    } else if let Some(light) = world.get::<DirectionalLight>(entity) {
        Ok(ChannelValue::Vec3(light.color))
    } else {
        Err("the target has no light component".into())
    }
}

fn read_camera_fov(world: &World, entity: Entity) -> Result<ChannelValue, String> {
    world
        .get::<Camera>(entity)
        .map(|c| ChannelValue::Float(c.fov_y.to_degrees()))
        .ok_or_else(|| "the target has no Camera component".into())
}

/// The prefix of a reflected property path: `reflect:<Component>.<field.path>`.
pub const REFLECT_PREFIX: &str = "reflect:";

/// Splits `reflect:<Component>.<field.path>` (the component by short or full type path, which
/// has no `.`).
fn split_reflected(property: &str) -> Result<(&str, &str), String> {
    let rest = property
        .strip_prefix(REFLECT_PREFIX)
        .ok_or_else(|| format!("`{property}` is not a reflect: path"))?;
    match rest.split_once('.') {
        Some((component, path)) if !component.is_empty() && !path.is_empty() => Ok((component, path)),
        _ => Err(format!("`{property}` must be reflect:<Component>.<field.path>")),
    }
}

/// Reads a reflected field (`reflect:<Component>.<field.path>`) as a channel value: an `f32` or
/// `f64` reads as Float, a `bool` as Bool, a `Vec3` or `[f32; 3]` as Vec3.
pub fn read_reflected(world: &World, entity: Entity, property: &str) -> Result<ChannelValue, String> {
    let (component, path) = split_reflected(property)?;
    let registry = world
        .get_resource::<AppTypeRegistry>()
        .ok_or_else(|| "no type registry".to_string())?
        .clone();
    let registry = registry.read();
    let registration = registry
        .get_with_short_type_path(component)
        .or_else(|| regia_core::brand::lookup_type_path(component, |p| registry.get_with_type_path(p)))
        .ok_or_else(|| format!("{component} is not a registered type"))?;
    let reflect = registration
        .data::<ReflectComponent>()
        .ok_or_else(|| format!("{component} is not a reflected component"))?;
    let entity_ref = world
        .get_entity(entity)
        .map_err(|_| "the target entity does not exist".to_string())?;
    let value = reflect
        .reflect(entity_ref)
        .ok_or_else(|| format!("the target has no {component}"))?;
    let field = value
        .reflect_path(path)
        .map_err(|e| format!("no field `{path}` in {component}: {e}"))?;
    if let Some(v) = field.try_downcast_ref::<f32>() {
        Ok(ChannelValue::Float(*v))
    } else if let Some(v) = field.try_downcast_ref::<f64>() {
        Ok(ChannelValue::Float(*v as f32))
    } else if let Some(v) = field.try_downcast_ref::<bool>() {
        Ok(ChannelValue::Bool(*v))
    } else if let Some(v) = field.try_downcast_ref::<regia_core::math::Vec3>() {
        Ok(ChannelValue::Vec3(v.to_array()))
    } else if let Some(v) = field.try_downcast_ref::<[f32; 3]>() {
        Ok(ChannelValue::Vec3(*v))
    } else {
        Err(format!(
            "`{component}.{path}` is not an f32, f64, bool, Vec3 or [f32; 3]"
        ))
    }
}

/// Writes a channel value into a reflected field (`reflect:<Component>.<field.path>`): the
/// sequence player's fallback for a `reflect:` property with no registered applier, so a
/// recorded reflected property plays back.
pub fn apply_reflected(
    world: &mut World,
    entity: Entity,
    property: &str,
    value: ChannelValue,
) -> Result<(), String> {
    let (component, path) = split_reflected(property)?;
    let registry = world
        .get_resource::<AppTypeRegistry>()
        .ok_or_else(|| "no type registry".to_string())?
        .clone();
    let registry = registry.read();
    let registration = registry
        .get_with_short_type_path(component)
        .or_else(|| regia_core::brand::lookup_type_path(component, |p| registry.get_with_type_path(p)))
        .ok_or_else(|| format!("{component} is not a registered type"))?;
    let reflect = registration
        .data::<ReflectComponent>()
        .ok_or_else(|| format!("{component} is not a reflected component"))?;
    let mut entity_mut = world
        .get_entity_mut(entity)
        .map_err(|_| "the target entity does not exist".to_string())?;
    let mut reflected = reflect
        .reflect_mut(&mut entity_mut)
        .ok_or_else(|| format!("the target has no {component}"))?;
    let field = reflected
        .as_reflect_mut()
        .reflect_path_mut(path)
        .map_err(|e| format!("no field `{path}` in {component}: {e}"))?;
    let wrong = || format!("`{component}.{path}` does not take a {} value", value_kind(value));
    match value {
        ChannelValue::Float(v) => {
            if let Some(slot) = field.try_downcast_mut::<f32>() {
                *slot = v;
            } else if let Some(slot) = field.try_downcast_mut::<f64>() {
                *slot = f64::from(v);
            } else {
                return Err(wrong());
            }
        }
        ChannelValue::Bool(v) => {
            *field.try_downcast_mut::<bool>().ok_or_else(wrong)? = v;
        }
        ChannelValue::Vec3(v) => {
            if let Some(slot) = field.try_downcast_mut::<regia_core::math::Vec3>() {
                *slot = regia_core::math::Vec3::from_array(v);
            } else if let Some(slot) = field.try_downcast_mut::<[f32; 3]>() {
                *slot = v;
            } else {
                return Err(wrong());
            }
        }
    }
    Ok(())
}

fn value_kind(value: ChannelValue) -> &'static str {
    match value {
        ChannelValue::Float(_) => "Float",
        ChannelValue::Vec3(_) => "Vec3",
        ChannelValue::Bool(_) => "Bool",
    }
}

/// How the error between a recorded and a replayed value is measured.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ErrorMetric {
    /// Euclidean distance (positions).
    Distance(f32),
    /// The largest component difference (Euler angles, scales, properties).
    Component(f32),
}

impl ErrorMetric {
    fn tolerance(self) -> f32 {
        match self {
            Self::Distance(t) | Self::Component(t) => t,
        }
    }

    fn error<const N: usize>(self, a: &[f32; N], b: &[f64; N]) -> f64 {
        match self {
            Self::Distance(_) => a
                .iter()
                .zip(b)
                .map(|(a, b)| (f64::from(*a) - b).powi(2))
                .sum::<f64>()
                .sqrt(),
            Self::Component(_) => a
                .iter()
                .zip(b)
                .map(|(a, b)| (f64::from(*a) - b).abs())
                .fold(0.0, f64::max),
        }
    }
}

/// The indices of the samples a linear polyline through them needs so that every sample lies
/// within `metric`'s tolerance of it: the Ramer-Douglas-Peucker simplification (Ramer 1972;
/// Douglas and Peucker 1973) with the error measured at each sample's own time (the vertical, not
/// the perpendicular, distance: what playback at that instant shows), run window by window
/// (`REDUCE_WINDOW`). The first and last samples are always kept; a constant channel keeps just
/// those two. `times` must be strictly increasing and as long as `values`.
#[must_use]
pub fn reduce_samples<const N: usize>(times: &[f32], values: &[[f32; N]], metric: ErrorMetric) -> Vec<usize> {
    let n = times.len().min(values.len());
    if n <= 2 {
        return (0..n).collect();
    }
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;
    let mut start = 0;
    while start < n - 1 {
        let end = (start + REDUCE_WINDOW).min(n - 1);
        keep[end] = true;
        rdp(times, values, start, end, metric, &mut keep);
        start = end;
    }
    keep.iter()
        .enumerate()
        .filter_map(|(i, k)| k.then_some(i))
        .collect()
}

/// Ramer-Douglas-Peucker over `[lo, hi]`, iteratively (no recursion depth to overflow).
fn rdp<const N: usize>(
    times: &[f32],
    values: &[[f32; N]],
    lo: usize,
    hi: usize,
    metric: ErrorMetric,
    keep: &mut [bool],
) {
    let tolerance = f64::from(metric.tolerance());
    let mut stack = vec![(lo, hi)];
    while let Some((a, b)) = stack.pop() {
        if b <= a + 1 {
            continue;
        }
        let (ta, tb) = (f64::from(times[a]), f64::from(times[b]));
        let span = tb - ta;
        let mut worst = (0.0, a);
        for i in a + 1..b {
            let u = if span > 0.0 {
                (f64::from(times[i]) - ta) / span
            } else {
                0.0
            };
            let mut line = [0.0f64; N];
            for (c, slot) in line.iter_mut().enumerate() {
                let (va, vb) = (f64::from(values[a][c]), f64::from(values[b][c]));
                *slot = va + (vb - va) * u;
            }
            let error = metric.error(&values[i], &line);
            if error > worst.0 {
                worst = (error, i);
            }
        }
        if worst.0 > tolerance {
            keep[worst.1] = true;
            stack.push((a, worst.1));
            stack.push((worst.1, b));
        }
    }
}

/// One recorded keyed channel, before reduction.
#[derive(Clone, Debug, Default)]
struct Samples<V> {
    times: Vec<u64>,
    values: Vec<V>,
}

impl<V> Samples<V> {
    fn push(&mut self, time: u64, value: V) {
        self.times.push(time);
        self.values.push(value);
    }

    fn len(&self) -> usize {
        self.times.len()
    }
}

/// The recorded samples of one track.
#[derive(Clone, Debug)]
enum RecordedTrack {
    Float(Samples<[f32; 1]>, ErrorMetric),
    Vec3(Samples<[f32; 3]>, ErrorMetric),
    Bool(Samples<bool>),
}

impl RecordedTrack {
    fn len(&self) -> usize {
        match self {
            Self::Float(s, _) => s.len(),
            Self::Vec3(s, _) => s.len(),
            Self::Bool(s) => s.len(),
        }
    }

    /// Adds one sample; a non-finite value (which no key may hold) or one of another kind is
    /// dropped with the reason.
    fn push(&mut self, time: u64, value: ChannelValue) -> Result<(), &'static str> {
        let finite = match value {
            ChannelValue::Float(v) => v.is_finite(),
            ChannelValue::Vec3(v) => v.iter().all(|c| c.is_finite()),
            ChannelValue::Bool(_) => true,
        };
        if !finite {
            return Err("is not finite; the sample is dropped");
        }
        match (self, value) {
            (Self::Float(s, _), ChannelValue::Float(v)) => s.push(time, [v]),
            (Self::Vec3(s, _), ChannelValue::Vec3(v)) => s.push(time, v),
            (Self::Bool(s), ChannelValue::Bool(v)) => s.push(time, v),
            _ => return Err("changed kind mid-take; the sample is dropped"),
        }
        Ok(())
    }
}

/// Seconds of a take time, as a key time.
fn key_time(nanos: u64) -> f32 {
    nanos_to_seconds(nanos) as f32
}

/// Times as key seconds, dropping a sample whose `f32` time does not advance (only possible on a
/// take far longer than [`MAX_TAKE_SAMPLES`] steps of a microsecond).
fn key_times<V: Copy>(samples: &Samples<V>) -> (Vec<f32>, Vec<V>, Vec<u64>) {
    let mut times = Vec::with_capacity(samples.len());
    let mut values = Vec::with_capacity(samples.len());
    let mut nanos = Vec::with_capacity(samples.len());
    for (t, v) in samples.times.iter().zip(&samples.values) {
        let s = key_time(*t);
        if times.last().is_some_and(|last| s <= *last) {
            continue;
        }
        times.push(s);
        values.push(*v);
        nanos.push(*t);
    }
    (times, values, nanos)
}

/// Builds a keyed channel from times, values, the kept sample indices and the ones that step.
type BuildKeys<const N: usize> = fn(&[f32], &[[f32; N]], &[usize], &BTreeSet<usize>) -> Channel;

/// Reduced keys of one keyed channel: segments split at `cuts` (take times where the value jumps
/// without blending), each reduced on its own, the last key of a segment before a cut stepping.
/// Every recorded sample is then checked against the engine's sampler, and a key is added where
/// the replay misses the tolerance, so the bound holds as played and not only on paper.
fn reduce_channel<const N: usize>(
    samples: &Samples<[f32; N]>,
    cuts: &[u64],
    metric: ErrorMetric,
    build: BuildKeys<N>,
) -> Channel {
    let (times, values, nanos) = key_times(samples);
    let n = times.len();
    // The first sample index at or after each cut, past the first sample.
    let mut starts: BTreeSet<usize> = cuts
        .iter()
        .map(|cut| nanos.partition_point(|t| t < cut))
        .filter(|&i| i > 0 && i < n)
        .collect();
    starts.insert(n);
    let mut kept = BTreeSet::new();
    let mut steps = BTreeSet::new();
    let mut from = 0;
    for &to in &starts {
        if to <= from {
            continue;
        }
        for i in reduce_samples(&times[from..to], &values[from..to], metric) {
            kept.insert(from + i);
        }
        if to < n {
            steps.insert(to - 1);
        }
        from = to;
    }
    let tolerance = f64::from(metric.tolerance());
    loop {
        let keys: Vec<usize> = kept.iter().copied().collect();
        let channel = build(&times, &values, &keys, &steps);
        let mut missed = false;
        for i in 0..n {
            if kept.contains(&i) {
                continue;
            }
            let played = played_components::<N>(&channel, times[i]);
            if metric.error(&values[i], &played) > tolerance {
                kept.insert(i);
                missed = true;
            }
        }
        if !missed {
            return channel;
        }
    }
}

fn played_components<const N: usize>(channel: &Channel, time: f32) -> [f64; N] {
    let mut out = [0.0f64; N];
    match channel.sample(time) {
        ChannelValue::Float(v) => {
            if let Some(slot) = out.first_mut() {
                *slot = f64::from(v);
            }
        }
        ChannelValue::Vec3(v) => {
            for (slot, v) in out.iter_mut().zip(v) {
                *slot = f64::from(v);
            }
        }
        ChannelValue::Bool(_) => {}
    }
    out
}

fn build_float(times: &[f32], values: &[[f32; 1]], keys: &[usize], steps: &BTreeSet<usize>) -> Channel {
    Channel::Float(
        keys.iter()
            .map(|&i| ScalarKey::new(times[i], values[i][0], ease_of(i, steps)))
            .collect(),
    )
}

fn build_vec3(times: &[f32], values: &[[f32; 3]], keys: &[usize], steps: &BTreeSet<usize>) -> Channel {
    Channel::Vec3(
        keys.iter()
            .map(|&i| Vec3Key::new(times[i], values[i], ease_of(i, steps)))
            .collect(),
    )
}

fn ease_of(i: usize, steps: &BTreeSet<usize>) -> Ease {
    if steps.contains(&i) {
        Ease::Step
    } else {
        Ease::Linear
    }
}

/// Bool keys: the first sample, every change, and the last sample (two keys when it never
/// changes).
fn reduce_bool(samples: &Samples<bool>) -> Channel {
    let (times, values, _) = key_times(samples);
    let mut keys: Vec<BoolKey> = Vec::new();
    for (i, (&time, &value)) in times.iter().zip(&values).enumerate() {
        let changed = keys.last().is_none_or(|k| k.value != value);
        if changed || i + 1 == times.len() {
            keys.push(BoolKey { time, value });
        }
    }
    Channel::Bool(keys)
}

/// One animation sample: the dominant clip and its time.
#[derive(Clone, Debug, PartialEq)]
struct ClipFrame {
    time: u64,
    clip: String,
    clip_time: f32,
}

/// Slowest section speed: a held clip time plays at this rate, and the clip-time check splits
/// the section once the drift passes the tolerance.
const MIN_CLIP_SPEED: f32 = 1.0e-3;

/// Clip sections from animation samples: a new section starts where the clip changes or its time
/// leaves the line the section predicts (offset plus speed times elapsed, the speed taken from
/// the section's first two samples) by more than `tolerance` seconds, a wrap or a restart
/// included. Each section ends where the next starts; the last one a frame after its last sample.
fn clip_sections(frames: &[ClipFrame], tolerance: f32, frame: f32) -> Vec<ClipSection> {
    let mut sections = Vec::new();
    let mut i = 0;
    while i < frames.len() {
        let first = &frames[i];
        let t0 = nanos_to_seconds(first.time);
        let speed = frames
            .get(i + 1)
            .filter(|next| next.clip == first.clip)
            .map_or(1.0, |next| {
                let dt = nanos_to_seconds(next.time) - t0;
                if dt > 0.0 {
                    ((f64::from(next.clip_time) - f64::from(first.clip_time)) / dt) as f32
                } else {
                    1.0
                }
            })
            .clamp(MIN_CLIP_SPEED, 1000.0);
        let mut j = i + 1;
        while let Some(next) = frames.get(j) {
            let predicted =
                f64::from(first.clip_time) + f64::from(speed) * (nanos_to_seconds(next.time) - t0);
            if next.clip != first.clip || (f64::from(next.clip_time) - predicted).abs() > f64::from(tolerance)
            {
                break;
            }
            j += 1;
        }
        let start = key_time(first.time);
        let end = match frames.get(j) {
            Some(next) => key_time(next.time),
            None => key_time(frames[j - 1].time) + frame,
        };
        sections.push(ClipSection {
            clip: first.clip.clone(),
            start,
            end: end.max(start + 1.0e-3),
            offset: first.clip_time,
            speed,
            looping: false,
            blend_in: 0.0,
            blend_out: 0.0,
            weight: 1.0,
        });
        i = j;
    }
    sections
}

/// The dominant clip of an animator: the active clip node with the most weight (graph animators;
/// a single-clip animator reports none).
fn dominant_clip(animator: &Animator) -> Option<(String, f32)> {
    let snapshot = animator.debug_snapshot()?;
    let mut best: Option<(f32, String, f32)> = None;
    for node in &snapshot.nodes {
        if let NodeDebugState::Clip { clip, time, .. } = &node.state
            && node.active
            && best.as_ref().is_none_or(|(w, ..)| node.weight > *w)
        {
            best = Some((node.weight, clip.clone(), *time));
        }
    }
    best.map(|(_, clip, time)| (clip, time))
}

/// One binding's recording.
#[derive(Clone, Debug)]
struct BindingRecording {
    spec: TakeBinding,
    resolved: Option<Entity>,
    /// Why the binding is left out of the take (`driven`), once decided.
    excluded: Option<&'static str>,
    /// Tracks by property path.
    tracks: BTreeMap<String, RecordedTrack>,
    clips: Vec<ClipFrame>,
    /// Take times where the camera cut.
    cuts: Vec<u64>,
    last_camera: Option<Entity>,
    last_position: Option<[f32; 3]>,
    /// The shot-change count of the player holding the camera, as last seen.
    last_shot_changes: Option<(Entity, u64)>,
    frames: u64,
    missing: u64,
}

impl BindingRecording {
    fn new(spec: TakeBinding) -> Self {
        Self {
            spec,
            resolved: None,
            excluded: None,
            tracks: BTreeMap::new(),
            clips: Vec::new(),
            cuts: Vec::new(),
            last_camera: None,
            last_position: None,
            last_shot_changes: None,
            frames: 0,
            missing: 0,
        }
    }

    fn track(&mut self, property: &str, make: impl FnOnce() -> RecordedTrack) -> &mut RecordedTrack {
        self.tracks.entry(property.to_string()).or_insert_with(make)
    }

    fn last_vec3(&self, property: &str) -> Option<[f32; 3]> {
        match self.tracks.get(property)? {
            RecordedTrack::Vec3(s, _) => s.values.last().copied(),
            _ => None,
        }
    }
}

/// A binding's state, for the agent and the editor.
#[derive(Clone, Debug, PartialEq)]
pub struct TakeBindingState {
    /// The binding name.
    pub name: String,
    /// The entity it resolved to on the last sample.
    pub entity: Option<Entity>,
    /// The channels it records.
    pub channels: Vec<String>,
    /// Frames it was sampled on.
    pub frames: u64,
    /// Frames it did not resolve on.
    pub missing: u64,
    /// Why it is left out (`driven`), if it is.
    pub excluded: Option<&'static str>,
    /// Camera cuts so far.
    pub cuts: usize,
}

/// A stopped take: the sequence and what went into it.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedTake {
    /// The take, a valid `.seq.ron` v3 sequence (times from zero).
    pub sequence: Sequence,
    /// Gameplay frames sampled.
    pub frames: u64,
    /// Take seconds from the first to the last sample.
    pub duration: f64,
    /// `(track name, samples, keys or sections)` of every written track.
    pub tracks: Vec<(String, usize, usize)>,
    /// `(binding, reason)` of every binding left out.
    pub excluded: Vec<(String, String)>,
    /// Audio cues dropped past [`MAX_TAKE_CUES`].
    pub dropped_cues: u64,
    /// Problems met while sampling (a property that would not read), once each.
    pub issues: Vec<String>,
}

/// The take recorder: one recording at a time. Start it with [`start`](Self::start); the
/// recorder's systems sample it every gameplay frame; [`stop`](Self::stop) returns the take.
#[derive(Resource, Debug, Default)]
pub struct TakeRecorder {
    config: Option<TakeConfig>,
    status: TakeStatus,
    countdown_left: u32,
    clock_ns: u64,
    started: bool,
    frames: u64,
    bindings: Vec<BindingRecording>,
    cues: Vec<(u64, TakeCue)>,
    pending_cues: Vec<TakeCue>,
    dropped_cues: u64,
    issues: BTreeSet<String>,
    takes: u64,
    unsaved: Option<RecordedTake>,
}

impl TakeRecorder {
    /// Starts a recording: counting down when `config.countdown` is above zero, else sampling
    /// from the next recorder pass (this frame's when called before `PostUpdate`).
    pub fn start(&mut self, config: TakeConfig) -> Result<(), TakeError> {
        if self.status.is_active() {
            return Err(TakeError::Busy);
        }
        if self.unsaved.is_some() {
            return Err(TakeError::Unsaved);
        }
        config.validate()?;
        self.bindings = config
            .bindings
            .iter()
            .cloned()
            .map(BindingRecording::new)
            .collect();
        self.status = if config.countdown > 0 {
            TakeStatus::Countdown
        } else {
            TakeStatus::Recording
        };
        self.countdown_left = config.countdown;
        self.clock_ns = 0;
        self.started = false;
        self.frames = 0;
        self.cues.clear();
        self.pending_cues.clear();
        self.dropped_cues = 0;
        self.issues.clear();
        self.config = Some(config);
        Ok(())
    }

    /// Drops the recording without writing anything; while idle, drops a take kept by
    /// [`keep_unsaved`](Self::keep_unsaved) instead.
    pub fn cancel(&mut self) -> Result<(), TakeError> {
        if !self.status.is_active() {
            return match self.unsaved.take() {
                Some(_) => Ok(()),
                None => Err(TakeError::NotRecording),
            };
        }
        self.reset();
        Ok(())
    }

    fn reset(&mut self) {
        self.status = TakeStatus::Idle;
        self.config = None;
        self.bindings.clear();
        self.cues.clear();
        self.pending_cues.clear();
    }

    /// Stops the recording and builds the take. The recorder is idle afterwards whatever the
    /// result, so a refused take never blocks the next recording.
    pub fn stop(&mut self) -> Result<RecordedTake, TakeError> {
        if !self.status.is_active() {
            return Err(TakeError::NotRecording);
        }
        let result = self.build();
        self.reset();
        if result.is_ok() {
            self.takes += 1;
        }
        result
    }

    /// Keeps a stopped take whose files could not be written, so a later save can retry
    /// (the agent's `sequence.record_stop` does this when [`save_take`] fails).
    pub fn keep_unsaved(&mut self, take: RecordedTake) {
        self.unsaved = Some(take);
    }

    /// Takes back the take kept by [`keep_unsaved`](Self::keep_unsaved).
    pub fn take_unsaved(&mut self) -> Option<RecordedTake> {
        self.unsaved.take()
    }

    /// `true` while a stopped take waits to be saved.
    #[must_use]
    pub fn has_unsaved(&self) -> bool {
        self.unsaved.is_some()
    }

    /// Where the recording is.
    #[must_use]
    pub fn status(&self) -> TakeStatus {
        self.status
    }

    /// Countdown frames left.
    #[must_use]
    pub fn countdown_left(&self) -> u32 {
        self.countdown_left
    }

    /// Gameplay frames sampled so far.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Take seconds so far.
    #[must_use]
    pub fn duration(&self) -> f64 {
        nanos_to_seconds(self.clock_ns)
    }

    /// Takes stopped successfully since the recorder was created.
    #[must_use]
    pub fn takes(&self) -> u64 {
        self.takes
    }

    /// Audio cues recorded so far.
    #[must_use]
    pub fn cues(&self) -> usize {
        self.cues.len()
    }

    /// Audio cues dropped past [`MAX_TAKE_CUES`].
    #[must_use]
    pub fn dropped_cues(&self) -> u64 {
        self.dropped_cues
    }

    /// The running recording's settings.
    #[must_use]
    pub fn config(&self) -> Option<&TakeConfig> {
        self.config.as_ref()
    }

    /// Problems met so far, once each.
    pub fn issues(&self) -> impl Iterator<Item = &str> {
        self.issues.iter().map(String::as_str)
    }

    /// Every binding's state.
    #[must_use]
    pub fn binding_states(&self) -> Vec<TakeBindingState> {
        self.bindings
            .iter()
            .map(|b| TakeBindingState {
                name: b.spec.name.clone(),
                entity: b.resolved,
                channels: b.spec.channels.iter().map(TakeChannel::name).collect(),
                frames: b.frames,
                missing: b.missing,
                excluded: b.excluded,
                cuts: b.cuts.len(),
            })
            .collect()
    }

    fn wants_frame(&self) -> bool {
        matches!(self.status, TakeStatus::Countdown | TakeStatus::Recording)
    }

    /// One gameplay frame: counts down, or advances the take clock by `delta_ns` and samples
    /// every binding. Frames that do not advance gameplay time (paused) are skipped after the
    /// first sample, so take times strictly increase.
    fn frame(&mut self, world: &World, delta_ns: u64) {
        if self.status == TakeStatus::Countdown {
            self.pending_cues.clear();
            if delta_ns == 0 {
                return;
            }
            // `countdown` whole gameplay frames go by unsampled; the next one is the first sample.
            if self.countdown_left > 0 {
                self.countdown_left -= 1;
                return;
            }
            self.status = TakeStatus::Recording;
        }
        if self.status != TakeStatus::Recording {
            return;
        }
        if self.started && delta_ns == 0 {
            return;
        }
        if self.frames as usize >= MAX_TAKE_SAMPLES {
            self.status = TakeStatus::Full;
            self.pending_cues.clear();
            return;
        }
        if self.started {
            self.clock_ns = self.clock_ns.saturating_add(delta_ns);
        }
        self.started = true;
        self.frames += 1;
        let now = self.clock_ns;
        for cue in std::mem::take(&mut self.pending_cues) {
            if self.cues.len() < MAX_TAKE_CUES {
                self.cues.push((now, cue));
            } else {
                self.dropped_cues += 1;
            }
        }
        let Some(config) = self.config.as_ref() else {
            return;
        };
        let driven = driven_entities(world);
        let readers = world.get_resource::<PropertyReaders>();
        let mut issues = Vec::new();
        for binding in &mut self.bindings {
            sample_binding(world, binding, config, &driven, readers, now, &mut issues);
        }
        self.issues.extend(issues);
    }

    fn build(&self) -> Result<RecordedTake, TakeError> {
        let config = self.config.as_ref().ok_or(TakeError::NotRecording)?;
        if self.frames == 0 {
            return Err(TakeError::Empty(
                "stopped before the first sample (still counting down, or no gameplay frame ran)".into(),
            ));
        }
        let mut tracks = Vec::new();
        let mut markers = Vec::new();
        let mut summary = Vec::new();
        let mut excluded = Vec::new();
        for binding in &self.bindings {
            let name = &binding.spec.name;
            if let Some(reason) = binding.excluded {
                excluded.push((name.clone(), reason.to_string()));
                continue;
            }
            for (property, recorded) in &binding.tracks {
                let channel = match recorded {
                    RecordedTrack::Float(s, metric) => reduce_channel(s, &binding.cuts, *metric, build_float),
                    RecordedTrack::Vec3(s, metric) => reduce_channel(s, &binding.cuts, *metric, build_vec3),
                    RecordedTrack::Bool(s) => reduce_bool(s),
                };
                if channel.is_empty() {
                    continue;
                }
                let track = format!("{name}/{property}");
                summary.push((track.clone(), recorded.len(), channel.len()));
                tracks.push(Track {
                    name: track,
                    target: name.clone(),
                    property: property.clone(),
                    keys: channel,
                    enabled: true,
                    locked: false,
                });
            }
            if !binding.clips.is_empty() {
                let sections = clip_sections(&binding.clips, config.tolerance.clip_time, 1.0 / config.fps);
                for (part, chunk) in sections.chunks(MAX_SECTIONS).enumerate() {
                    let track = if part == 0 {
                        format!("{name}/clip")
                    } else {
                        format!("{name}/clip_{}", part + 1)
                    };
                    summary.push((track.clone(), binding.clips.len(), chunk.len()));
                    tracks.push(Track {
                        name: track,
                        target: name.clone(),
                        property: String::new(),
                        keys: Channel::Clip(chunk.to_vec()),
                        enabled: true,
                        locked: false,
                    });
                }
            }
            for (k, &cut) in binding.cuts.iter().enumerate() {
                markers.push(Marker {
                    name: format!("{name}/cut_{}", k + 1),
                    time: key_time(cut),
                    payload: "camera_cut".into(),
                });
            }
        }
        if !self.cues.is_empty() {
            let sections: Vec<AudioSection> = self
                .cues
                .iter()
                .map(|(time, cue)| {
                    let start = key_time(*time);
                    let length = if cue.length.is_finite() && cue.length > 0.0 {
                        cue.length
                    } else {
                        1.0 / config.fps
                    };
                    let end = (start + length).max(start.next_up());
                    let mut section = AudioSection::new(cue.sound.clone(), start, end);
                    if cue.volume_db.is_finite() {
                        section.volume_db = cue.volume_db.min(24.0);
                    }
                    section.bus = cue.bus.clone().filter(|b| !b.is_empty());
                    section.spatial = cue
                        .source
                        .clone()
                        .filter(|source| match source {
                            SoundSource::Binding(name) => !name.is_empty(),
                            SoundSource::Position(p) => p.iter().all(|v| v.is_finite()),
                        })
                        .map(AudioSpatial::new);
                    section
                })
                .filter(|s| !s.sound.is_empty())
                .collect();
            for (part, chunk) in sections.chunks(MAX_SECTIONS).enumerate() {
                let track = if part == 0 {
                    "audio".to_string()
                } else {
                    format!("audio_{}", part + 1)
                };
                summary.push((track.clone(), chunk.len(), chunk.len()));
                tracks.push(Track {
                    name: track,
                    target: String::new(),
                    property: String::new(),
                    keys: Channel::Audio(chunk.to_vec()),
                    enabled: true,
                    locked: false,
                });
            }
        }
        if tracks.is_empty() {
            return Err(TakeError::Empty(if excluded.is_empty() {
                "no binding resolved and no audio cue fired".into()
            } else {
                "every binding was left out (a playing sequence drives it; pass include_driven)".into()
            }));
        }
        // A property path may collide with another track's name (`property:clip` beside
        // `animation`): later duplicates get a `#k` suffix.
        let mut seen = BTreeSet::new();
        for track in &mut tracks {
            let base = track.name.clone();
            let mut k = 2;
            while !seen.insert(track.name.clone()) {
                track.name = format!("{base}#{k}");
                k += 1;
            }
        }
        for (entry, track) in summary.iter_mut().zip(&tracks) {
            entry.0.clone_from(&track.name);
        }
        markers.sort_by(|a, b| a.time.total_cmp(&b.time).then_with(|| a.name.cmp(&b.name)));
        let sequence = Sequence {
            version: SEQUENCE_FORMAT_VERSION,
            name: config.name.clone(),
            fps: config.fps,
            tracks,
            markers,
            ..Sequence::default()
        };
        sequence
            .validate()
            .map_err(|e| TakeError::Invalid(e.to_string()))?;
        Ok(RecordedTake {
            sequence,
            frames: self.frames,
            duration: nanos_to_seconds(self.clock_ns),
            tracks: summary,
            excluded,
            dropped_cues: self.dropped_cues,
            issues: self.issues.iter().cloned().collect(),
        })
    }
}

/// Every entity a playing sequence drives this frame: its resolved targets and the camera its
/// shot list holds.
fn driven_entities(world: &World) -> BTreeMap<Entity, Entity> {
    let mut driven = BTreeMap::new();
    let Some(mut query) = world.try_query::<(Entity, &SequencePlayer)>() else {
        return driven;
    };
    for (entity, player) in query.iter(world) {
        if !player.is_active() {
            continue;
        }
        for &target in player.target_entities().values() {
            driven.insert(target, entity);
        }
        if let Some((camera, _)) = player.held_camera() {
            driven.insert(camera, entity);
        }
    }
    driven
}

/// Resolves a binding against the world: a live entity, the entity carrying a guid, or the
/// active camera.
fn resolve(world: &World, target: TakeTarget, cached: Option<Entity>) -> Option<Entity> {
    match target {
        TakeTarget::Binding(Binding::Entity(entity)) => world.get_entity(entity).ok().map(|_| entity),
        TakeTarget::Binding(Binding::Guid(guid)) => {
            if let Some(entity) = cached
                && world.get::<EntityGuid>(entity).is_some_and(|g| g.0 == guid)
            {
                return Some(entity);
            }
            let mut query = world.try_query::<(Entity, &EntityGuid)>()?;
            query
                .iter(world)
                .find(|(_, g)| g.0 == guid)
                .map(|(entity, _)| entity)
        }
        TakeTarget::ActiveCamera => {
            let mut query = world.try_query::<(Entity, &Camera)>()?;
            // Spawn order breaks an `order` tie, as the shot player picks (ADR-1347).
            crate::shots::player::pick_camera(
                query.iter(world).map(|(e, c)| (e, c.order)),
                world.get_resource::<regia_ecs::spawn_order::SpawnOrder<Camera>>(),
            )
        }
    }
}

fn sample_binding(
    world: &World,
    binding: &mut BindingRecording,
    config: &TakeConfig,
    driven: &BTreeMap<Entity, Entity>,
    readers: Option<&PropertyReaders>,
    now: u64,
    issues: &mut Vec<String>,
) {
    if binding.excluded.is_some() {
        return;
    }
    let entity = resolve(world, binding.spec.target, binding.resolved);
    binding.resolved = entity;
    let Some(entity) = entity else {
        binding.missing += 1;
        return;
    };
    if !config.include_driven && driven.contains_key(&entity) {
        // Left out for the whole take: half a track of game and half of the sequence's own keys
        // would be neither.
        binding.excluded = Some("driven");
        binding.tracks.clear();
        binding.clips.clear();
        binding.cuts.clear();
        return;
    }
    binding.frames += 1;
    let tolerance = config.tolerance;
    // Taken out for the pass and put back after it, so a frame clones nothing.
    let name = std::mem::take(&mut binding.spec.name);
    let channels = std::mem::take(&mut binding.spec.channels);
    for channel in &channels {
        match channel {
            TakeChannel::Transform | TakeChannel::Camera => {
                let Some(t) = world.get::<Transform>(entity) else {
                    issues.push(format!("binding `{name}`: no Transform component"));
                    continue;
                };
                let position = position_of(world, entity, t);
                let mut euler = rotation_to_euler_degrees(t.rotation);
                if let Some(previous) = binding.last_vec3("transform.rotation") {
                    euler = unwrap_degrees(euler, previous);
                }
                let scale = t.scale.to_array();
                if *channel == TakeChannel::Camera {
                    let cut = camera_cut(world, binding, entity, position, config.cut_distance, driven);
                    if cut && binding.frames > 1 {
                        binding.cuts.push(now);
                    }
                    binding.last_position = Some(position);
                }
                let pushed = binding
                    .track("transform.position", || {
                        RecordedTrack::Vec3(Samples::default(), ErrorMetric::Distance(tolerance.position))
                    })
                    .push(now, ChannelValue::Vec3(position));
                report(issues, &name, "transform.position", pushed);
                let pushed = binding
                    .track("transform.rotation", || {
                        RecordedTrack::Vec3(
                            Samples::default(),
                            ErrorMetric::Component(tolerance.rotation / 3.0),
                        )
                    })
                    .push(now, ChannelValue::Vec3(euler));
                report(issues, &name, "transform.rotation", pushed);
                if *channel == TakeChannel::Transform {
                    let pushed = binding
                        .track("transform.scale", || {
                            RecordedTrack::Vec3(Samples::default(), ErrorMetric::Component(tolerance.scale))
                        })
                        .push(now, ChannelValue::Vec3(scale));
                    report(issues, &name, "transform.scale", pushed);
                } else if let Some(camera) = world.get::<Camera>(entity) {
                    let pushed = binding
                        .track("camera.fov", || {
                            RecordedTrack::Float(Samples::default(), ErrorMetric::Component(tolerance.property))
                        })
                        .push(now, ChannelValue::Float(camera.fov_y.to_degrees()));
                    report(issues, &name, "camera.fov", pushed);
                }
            }
            TakeChannel::Animation => match world.get::<Animator>(entity) {
                Some(animator) => match dominant_clip(animator) {
                    Some((clip, clip_time)) if clip_time.is_finite() && !clip.is_empty() => binding.clips.push(ClipFrame {
                        time: now,
                        clip,
                        clip_time,
                    }),
                    _ => issues.push(format!(
                        "binding `{name}`: the animator reports no clip (a single-clip animator, or not evaluated yet)"
                    )),
                },
                None => issues.push(format!("binding `{name}`: no Animator component")),
            },
            TakeChannel::Property(path) => {
                let value = match readers.and_then(|r| r.get(path)) {
                    Some(reader) => reader(world, entity),
                    None if path.starts_with(REFLECT_PREFIX) => read_reflected(world, entity, path),
                    None => Err(format!(
                        "no property reader for `{path}` (register one in PropertyReaders, or use reflect:<Component>.<field>)"
                    )),
                };
                match value {
                    Ok(value) => {
                        let metric = ErrorMetric::Component(tolerance.property);
                        let track = binding.track(path, || match value {
                            ChannelValue::Float(_) => RecordedTrack::Float(Samples::default(), metric),
                            ChannelValue::Vec3(_) => RecordedTrack::Vec3(Samples::default(), metric),
                            ChannelValue::Bool(_) => RecordedTrack::Bool(Samples::default()),
                        });
                        let pushed = track.push(now, value);
                        report(issues, &name, path, pushed);
                    }
                    Err(reason) => issues.push(format!("binding `{name}`: property `{path}`: {reason}")),
                }
            }
        }
    }
    binding.spec.name = name;
    binding.spec.channels = channels;
}

/// Notes a dropped sample in the take's issues.
fn report(issues: &mut Vec<String>, name: &str, property: &str, pushed: Result<(), &'static str>) {
    if let Err(why) = pushed {
        issues.push(format!("binding `{name}`: `{property}` {why}"));
    }
}

/// `true` when the camera cut since the last sample: another camera took over, the sequence
/// holding it changed shot, or it jumped further than `cut_distance`.
fn camera_cut(
    world: &World,
    binding: &mut BindingRecording,
    entity: Entity,
    position: [f32; 3],
    cut_distance: Option<f32>,
    driven: &BTreeMap<Entity, Entity>,
) -> bool {
    let mut cut = binding.last_camera.is_some_and(|last| last != entity);
    binding.last_camera = Some(entity);
    let holder = driven.get(&entity).and_then(|&player| {
        world
            .get::<SequencePlayer>(player)
            .map(|p| (player, p.shot_changes()))
    });
    if let (Some((player, changes)), Some((last_player, last_changes))) = (holder, binding.last_shot_changes)
        && player == last_player
        && changes != last_changes
    {
        cut = true;
    }
    binding.last_shot_changes = holder;
    if let (Some(limit), Some(last)) = (cut_distance, binding.last_position) {
        let jump = position
            .iter()
            .zip(last)
            .map(|(a, b)| (a - b).powi(2))
            .sum::<f32>()
            .sqrt();
        if jump > limit {
            cut = true;
        }
    }
    cut
}

/// Collects this frame's [`TakeCue`] messages for the recorder.
fn collect_take_cues(mut reader: MessageReader<TakeCue>, mut recorder: ResMut<TakeRecorder>) {
    let wanted =
        recorder.status == TakeStatus::Recording && recorder.config.as_ref().is_some_and(|c| c.audio_cues);
    for cue in reader.read() {
        if wanted {
            recorder.pending_cues.push(cue.clone());
        }
    }
}

/// Samples the recording once per gameplay frame.
fn record_take(world: &mut World) {
    let _span = tracing::info_span!(target: "regia_framework", "sequencer_record_take").entered();
    let delta_ns = world.get_resource::<Time>().map_or(0, |time| {
        u64::try_from(time.delta().as_nanos()).unwrap_or(u64::MAX)
    });
    world.resource_scope(|world, mut recorder: Mut<TakeRecorder>| {
        recorder.frame(world, delta_ns);
    });
}

fn recording(recorder: Option<Res<TakeRecorder>>) -> bool {
    recorder.is_some_and(|r| r.wants_frame())
}

fn cues_pending(cues: Option<Res<Messages<TakeCue>>>) -> bool {
    cues.is_some_and(|c| !c.is_empty())
}

/// Installs the take recorder: the [`TakeRecorder`] and [`PropertyReaders`] resources, the
/// [`TakeCue`] message, and the two `PostUpdate` systems (both idle until a take starts). The
/// [`super::SequencerPlugin`] calls this.
pub fn install(app: &mut App) {
    if app.world().contains_resource::<TakeRecorder>() {
        return;
    }
    app.init_resource::<TakeRecorder>()
        .init_resource::<PropertyReaders>()
        .add_message::<TakeCue>()
        .add_systems(
            PostUpdate,
            (
                // Whenever cues are pending, recording or not, so a cue sent before a take
                // started never lands in it.
                collect_take_cues.run_if(cues_pending),
                record_take.run_if(recording),
            )
                .chain()
                .after(SequencerSet::Apply)
                .after(PostUpdateSet::PhysicsSync)
                .after(PostUpdateSet::AnimationApply)
                // Cameras are written by their rigs, the shot and sequence players and the
                // director before they are sampled.
                .after(crate::camera::CameraSet::Director)
                .after(crate::shots::player::ShotDriveSet)
                .before(PostUpdateSet::TransformPropagate),
        );
}

/// The files [`save_take`] wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TakeFiles {
    /// The take, project-relative (`cinematics/intro_take3.seq.ron`): also its name in a
    /// sequence library and in the source's sub-sequence section.
    pub take: String,
    /// The take number.
    pub number: u32,
    /// The source sequence, project-relative.
    pub source: String,
    /// The source's new sub-sequence track (`take3`).
    pub section_track: String,
    /// `true` when the source did not exist and was created.
    pub created_source: bool,
}

/// Checks a project-relative `.seq.ron` path (plain components, `/`-separated) and splits it into
/// its directory part (with a trailing `/`, or empty) and its stem.
pub fn split_source(source: &str) -> Result<(String, String), TakeError> {
    let bad = || {
        TakeError::BadPath(format!(
            "`{source}` is not a project-relative .seq.ron path (plain components, no `..`)"
        ))
    };
    if source.is_empty() || source.len() > 1024 || source.contains('\\') {
        return Err(bad());
    }
    if !Path::new(source)
        .components()
        .all(|c| matches!(c, PathComponent::Normal(_)))
    {
        return Err(bad());
    }
    let (dir, file) = match source.rsplit_once('/') {
        Some((dir, file)) => (format!("{dir}/"), file),
        None => (String::new(), source),
    };
    let stem = file.strip_suffix(SEQ_EXTENSION).ok_or_else(bad)?;
    if stem.is_empty() {
        return Err(bad());
    }
    Ok((dir, stem.to_string()))
}

/// The project-relative path of take `number` of `source`: `<dir>/<stem>_take<number>.seq.ron`.
pub fn take_path(source: &str, number: u32) -> Result<String, TakeError> {
    let (dir, stem) = split_source(source)?;
    Ok(format!("{dir}{stem}{TAKE_SUFFIX}{number}{SEQ_EXTENSION}"))
}

/// Adds a sub-sequence track to `source` that plays `take` (a library name) from `at` seconds for
/// `length` seconds, named `take<number>` (or `take<number>_<k>` when that name is taken), and
/// returns the track name. Nothing already in `source` changes.
pub fn link_take(source: &mut Sequence, take: &str, number: u32, at: f32, length: f32) -> String {
    let base = format!("take{number}");
    let mut name = base.clone();
    let mut k = 2;
    while source.tracks.iter().any(|t| t.name == name) {
        name = format!("{base}_{k}");
        k += 1;
    }
    let at = if at.is_finite() { at.max(0.0) } else { 0.0 };
    let length = if length.is_finite() && length > 1.0e-3 {
        length
    } else {
        1.0e-3
    };
    source.tracks.push(Track {
        name: name.clone(),
        target: String::new(),
        property: String::new(),
        keys: Channel::Sub(vec![SubSection::new(take, at, at + length)]),
        enabled: true,
        locked: false,
    });
    name
}

/// Writes `text` to `path` through a temporary file and a rename, so a reader never sees half a
/// file.
fn write_atomic(path: &Path, text: &str) -> Result<(), TakeError> {
    let io = |e: std::io::Error| TakeError::Io(format!("{}: {e}", path.display()));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| TakeError::BadPath(format!("{} has no file name", path.display())))?;
    let tmp = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io(e)
    })
}

/// Writes a take next to its source sequence and links it in (ADR-2637):
///
/// - the take goes to `<dir>/<stem>_take<N>.seq.ron` under `root`, `N` the lowest free number
///   from 1 (the name is reserved with a create-new open before it is written, so two recorders
///   never share a number); its display name becomes `<stem>_take<N>`;
/// - the source (`root`/`source`) gains a sub-sequence track `take<N>` playing the take from `at`
///   seconds ([`link_take`]): recording into an existing sequence adds a section and overwrites
///   nothing. A missing source is created holding just that track.
///
/// Both files are written atomically. A source that does not load is refused before anything is
/// written ([`TakeError::SourceInvalid`]).
pub fn save_take(root: &Path, source: &str, take: &Sequence, at: f32) -> Result<TakeFiles, TakeError> {
    let (_, stem) = split_source(source)?;
    let source_path: PathBuf = root.join(source);
    let mut source_sequence = match std::fs::read_to_string(&source_path) {
        Ok(text) => Some(Sequence::from_ron(&text).map_err(|e| TakeError::SourceInvalid(e.to_string()))?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(TakeError::Io(format!("{}: {e}", source_path.display()))),
    };
    if let Some(dir) = source_path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| TakeError::Io(format!("{}: {e}", dir.display())))?;
    }
    let mut reserved = None;
    for number in 1..=MAX_TAKE_NUMBER {
        let rel = take_path(source, number)?;
        let path = root.join(&rel);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => {
                reserved = Some((number, rel, path));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(TakeError::Io(format!("{}: {e}", path.display()))),
        }
    }
    let (number, take_rel, take_abs) =
        reserved.ok_or_else(|| TakeError::NumbersExhausted(source.to_string()))?;
    let release = |e: TakeError| {
        let _ = std::fs::remove_file(&take_abs);
        e
    };
    let mut take = take.clone();
    take.name = format!("{stem}{TAKE_SUFFIX}{number}");
    take.validate()
        .map_err(|e| release(TakeError::Invalid(e.to_string())))?;
    let take_text = take
        .to_ron()
        .map_err(|e| release(TakeError::Invalid(e.to_string())))?;
    let created_source = source_sequence.is_none();
    let mut updated = source_sequence.take().unwrap_or_else(|| Sequence {
        name: stem.clone(),
        fps: take.fps,
        ..Sequence::default()
    });
    let length = take.duration() as f32;
    let section_track = link_take(&mut updated, &take_rel, number, at, length.max(1.0 / take.fps));
    updated
        .validate()
        .map_err(|e| release(TakeError::Invalid(e.to_string())))?;
    let source_text = updated
        .to_ron()
        .map_err(|e| release(TakeError::Invalid(e.to_string())))?;
    write_atomic(&take_abs, &take_text).map_err(release)?;
    // A take the source does not link would be an orphan: remove it, and the caller keeps the take.
    write_atomic(&source_path, &source_text).map_err(release)?;
    Ok(TakeFiles {
        take: take_rel,
        number,
        source: source.to_string(),
        section_track,
        created_source,
    })
}

#[cfg(test)]
mod tests;
