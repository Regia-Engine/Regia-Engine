// SPDX-License-Identifier: MPL-2.0
//! Input recording and playback (`docs/06-testing-and-ci.md` § 6): a deterministic way to
//! capture a player's action values per fixed step (plus the RNG seed) and replay them.
//!
//! [`InputRecording`] is the recorded stream; [`InputRecorder`] is the live recorder/player
//! resource. While **Recording**, `record_input_step` snapshots each tracked action's value on
//! every [`FixedUpdate`]; while **Playing**, `playback_inject_step` feeds the recorded values
//! back through [`regia_input::InputInjection`] and `playback_input_step` advances one recorded
//! step per fixed step.
//!
//! # Playback timing
//!
//! Step `k` of a replay sees exactly the action values step `k` of the recording saw.
//! `playback_inject_step` runs in [`FixedUpdate`] before
//! [`InputSet::Evaluation`](regia_input::InputSet::Evaluation) and holds the values of the
//! next recorded step for the fixed step about to be evaluated, and `playback_input_step`
//! releases them and advances the cursor after that step's evaluation — one recorded step per
//! fixed step, however many steps the frame runs. Before ADR-867 the values were injected
//! from [`FixedUpdate`] for the *next* frame, so a replay ran one step late: harmless for a
//! purely input-driven game, a divergence for anything that also moves on its own (gravity,
//! bots, timers).
//!
//! The mapping is exact for the fixed steps: a frame that runs no fixed step records nothing
//! and replays nothing (the zero-step-frame fallback evaluation reads the devices, not a
//! recorded step); only edge-triggered phases read across such a frame by frame-level readers
//! can differ.
//!
//! Together with a fixed timestep and a seeded RNG (the recording carries the seed), two runs of
//! the same recording produce identical results — the mechanism behind the `samples/arena`
//! "recorded bot match replays to the same final score" acceptance check.
//!
//! # Timestamps
//!
//! Each step also records when its raw input events reached the engine
//! ([`InputTimestamp`](regia_platform::InputTimestamp), via
//! [`InputTiming`](regia_platform::InputTiming)), in microseconds since recording started. They
//! are for analysis (input latency, gesture velocity, "what did the player press when"), not for
//! driving playback: a replay is indexed by step, so it is frame-identical however the original
//! events were spread in time. During playback [`InputRecorder::playback_timestamps`] exposes the
//! recorded stamps of the step just played.
//!
//! # Match records (`.frec`)
//!
//! A `.frec` file is the compact binary form of a recording with everything a viewer needs to
//! replay and seek it (RR-7, ADR-1878): the identity header of [`DemoHeader`] (engine version,
//! build hash, content hash, seed, level, tick rate), the tracked actions, one input row and one
//! timestamp list per fixed step, and periodic keyframes, each with the state hash the replay is
//! checked against and an optional snapshot to seek from. It carries the same data as a RON
//! `.regiademo` ([`DemoRecording`]); [`encode_frec`] and [`decode_frec`] convert, and
//! [`InputRecording::to_frec`] writes one straight from this recorder's stream.
//!
//! Layout (all integers little-endian):
//!
//! ```text
//! magic b"RFRC" | format_version u32 | payload_len u64 | payload (postcard) | crc32 u32
//! ```
//!
//! The CRC-32 covers the payload. A file shorter than its declared payload is
//! [`DemoError::Truncated`]; a bad magic or checksum is [`DemoError::Corrupt`]; a newer
//! `format_version` is [`DemoError::UnsupportedVersion`]. [`save_frec`] writes through a temp file
//! and a rename.

use std::path::Path;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_input::InputInjection;
use serde::{Deserialize, Serialize};

use crate::demo::{DemoError, DemoHeader, DemoKeyframe, DemoRecording};

/// The first four bytes of every `.frec` match record.
pub const FREC_MAGIC: &[u8; 4] = b"RFRC";
/// The current `.frec` `format_version`.
pub const FREC_FORMAT_VERSION: u32 = 1;
/// The file extension of a match record.
pub const FREC_EXTENSION: &str = "frec";
/// Largest `.frec` file [`load_frec`] reads (the same cap as a RON demo).
pub const MAX_FREC_BYTES: u64 = crate::demo::MAX_DEMO_BYTES;
/// Magic, version and payload length.
const FREC_HEAD_BYTES: usize = 16;

/// The postcard payload of a `.frec` file.
#[derive(Serialize, Deserialize)]
struct FrecPayload {
    header: DemoHeader,
    actions: Vec<String>,
    steps: Vec<Vec<[f32; 3]>>,
    timestamps: Vec<Vec<u64>>,
    keyframes: Vec<FrecKeyframe>,
}

/// A keyframe with its snapshot as raw bytes (the RON form spells them in hex).
#[derive(Serialize, Deserialize)]
struct FrecKeyframe {
    tick: u64,
    state_hash: u64,
    snapshot: Vec<u8>,
}

/// The serde field-list fingerprint of a `.frec` payload (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn frec_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<FrecPayload>(),
        regia_core::schema::serde_field_names::<DemoHeader>(),
        regia_core::schema::serde_field_names::<FrecKeyframe>(),
    ])
}

/// `.frec` is at its first version: no migration steps.
#[must_use]
pub fn frec_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Encodes `demo` as `.frec` bytes.
///
/// # Errors
/// [`DemoError::Invalid`] (and the other errors of [`DemoRecording::validate`]) for an
/// inconsistent demo, or [`DemoError::Parse`] if postcard cannot write it.
pub fn encode_frec(demo: &DemoRecording) -> Result<Vec<u8>, DemoError> {
    demo.validate()?;
    let mut keyframes = Vec::with_capacity(demo.keyframes.len());
    for key in &demo.keyframes {
        keyframes.push(FrecKeyframe {
            tick: key.tick,
            state_hash: key.state_hash,
            snapshot: crate::demo::decode_hex(&key.snapshot_hex).ok_or_else(|| {
                DemoError::Invalid(format!("keyframe {} snapshot is not valid hex", key.tick))
            })?,
        });
    }
    let payload = FrecPayload {
        header: demo.header.clone(),
        actions: demo.actions.clone(),
        steps: demo
            .steps
            .iter()
            .map(|row| row.iter().map(|v| v.to_array()).collect())
            .collect(),
        timestamps: demo.timestamps.clone(),
        keyframes,
    };
    let body = postcard::to_allocvec(&payload).map_err(|e| DemoError::Parse(e.to_string()))?;
    let mut out = Vec::with_capacity(FREC_HEAD_BYTES + body.len() + 4);
    out.extend_from_slice(FREC_MAGIC);
    out.extend_from_slice(&FREC_FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&(body.len() as u64).to_le_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(&crate::save_game::crc32(&body).to_le_bytes());
    Ok(out)
}

/// Decodes `.frec` bytes into a demo.
///
/// # Errors
/// [`DemoError::Truncated`] for a file shorter than its header or declared payload,
/// [`DemoError::Corrupt`] for a bad magic, trailing bytes or a checksum mismatch,
/// [`DemoError::UnsupportedVersion`] for a newer `format_version`, [`DemoError::Parse`] for a
/// payload postcard cannot read, and the errors of [`DemoRecording::validate`].
pub fn decode_frec(bytes: &[u8]) -> Result<DemoRecording, DemoError> {
    let truncated = || DemoError::Truncated { bytes: bytes.len() };
    let Some((head, rest)) = bytes.split_first_chunk::<FREC_HEAD_BYTES>() else {
        // A prefix of the magic is a cut-off record; anything else is not a record at all.
        return if FREC_MAGIC.starts_with(&bytes[..bytes.len().min(4)]) {
            Err(truncated())
        } else {
            Err(DemoError::Corrupt("not a .frec file (bad magic)".to_string()))
        };
    };
    let (magic, head) = head.split_at(4);
    if magic != FREC_MAGIC {
        return Err(DemoError::Corrupt("not a .frec file (bad magic)".to_string()));
    }
    let (version, len) = head.split_at(4);
    let version = u32::from_le_bytes(version.try_into().map_err(|_| truncated())?);
    if version == 0 || version > FREC_FORMAT_VERSION {
        return Err(DemoError::UnsupportedVersion(version));
    }
    let len = u64::from_le_bytes(len.try_into().map_err(|_| truncated())?);
    let len = usize::try_from(len).map_err(|_| DemoError::TooLarge {
        size: len,
        max: MAX_FREC_BYTES,
    })?;
    if rest.len() < len.saturating_add(4) {
        return Err(truncated());
    }
    if rest.len() > len + 4 {
        return Err(DemoError::Corrupt(format!(
            "{} bytes after the checksum",
            rest.len() - len - 4
        )));
    }
    let (body, crc) = rest.split_at(len);
    let stored = u32::from_le_bytes(crc.try_into().map_err(|_| truncated())?);
    let computed = crate::save_game::crc32(body);
    if stored != computed {
        return Err(DemoError::Corrupt(format!(
            "checksum mismatch: stored {stored:08x}, computed {computed:08x}"
        )));
    }
    let payload: FrecPayload = postcard::from_bytes(body).map_err(|e| DemoError::Parse(e.to_string()))?;
    let demo = DemoRecording {
        header: payload.header,
        actions: payload.actions,
        steps: payload
            .steps
            .into_iter()
            .map(|row| row.into_iter().map(Vec3::from_array).collect())
            .collect(),
        keyframes: payload
            .keyframes
            .into_iter()
            .map(|key| DemoKeyframe {
                tick: key.tick,
                state_hash: key.state_hash,
                snapshot_hex: crate::demo::encode_hex(&key.snapshot),
            })
            .collect(),
        timestamps: payload.timestamps,
    };
    demo.validate()?;
    Ok(demo)
}

/// Writes `demo` to `path` as a `.frec` match record, through a temp file and a rename.
///
/// # Errors
/// The errors of [`encode_frec`], or [`DemoError::Io`].
pub fn save_frec(demo: &DemoRecording, path: &Path) -> Result<(), DemoError> {
    let bytes = encode_frec(demo)?;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    std::fs::write(&tmp, bytes).map_err(|e| DemoError::Io(e.to_string()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        DemoError::Io(e.to_string())
    })
}

/// Reads a `.frec` match record (at most [`MAX_FREC_BYTES`]).
///
/// # Errors
/// [`DemoError::Io`], [`DemoError::TooLarge`], or any error of [`decode_frec`].
pub fn load_frec(path: &Path) -> Result<DemoRecording, DemoError> {
    let size = std::fs::metadata(path)
        .map_err(|e| DemoError::Io(e.to_string()))?
        .len();
    if size > MAX_FREC_BYTES {
        return Err(DemoError::TooLarge {
            size,
            max: MAX_FREC_BYTES,
        });
    }
    let bytes = std::fs::read(path).map_err(|e| DemoError::Io(e.to_string()))?;
    decode_frec(&bytes)
}

/// A recorded input stream: the RNG seed and, per fixed step, the value of every tracked action
/// (player 0).
#[derive(Resource, Debug, Clone, Default)]
pub struct InputRecording {
    /// The RNG seed the recording was made under (replay must seed identically).
    pub seed: u64,
    /// Tracked action names, in recording order.
    pub actions: Vec<String>,
    /// One `Vec<Vec3>` per fixed step; each inner vector parallels [`InputRecording::actions`].
    pub frames: Vec<Vec<Vec3>>,
    /// Per fixed step (parallel to [`InputRecording::frames`]), the arrival times of the raw input
    /// events the step's frame consumed, in microseconds since recording started. A frame's events
    /// go to its first fixed step; later steps of the same frame, and frames without input, record
    /// an empty list. Recordings made before timestamps existed have none (`timestamps` empty).
    pub timestamps: Vec<Vec<u64>>,
}

impl InputRecording {
    /// Number of fixed steps recorded.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Whether no steps were recorded.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// This stream as `.frec` bytes under `header` (its seed is taken from the recording) with
    /// `keyframes` (empty for an input-only record: it replays from the start but cannot seek).
    ///
    /// # Errors
    /// The errors of [`encode_frec`] (keyframes past the end or out of order are
    /// [`DemoError::Invalid`]).
    pub fn to_frec(&self, header: DemoHeader, keyframes: Vec<DemoKeyframe>) -> Result<Vec<u8>, DemoError> {
        let mut demo = DemoRecording::from_input_recording(header, self);
        demo.keyframes = keyframes;
        encode_frec(&demo)
    }
}

/// What the recorder is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecorderMode {
    /// Not recording or playing.
    #[default]
    Idle,
    /// Capturing action values into [`InputRecorder::recording`].
    Recording,
    /// Feeding a recording back through [`InputInjection`].
    Playing,
}

/// The live record/playback state (§ 06 § 6).
#[derive(Resource, Debug, Default)]
pub struct InputRecorder {
    /// Current mode.
    pub mode: RecorderMode,
    /// The recording being built (Recording) or played (Playing).
    pub recording: InputRecording,
    /// Next frame index to play (Playing).
    pub cursor: usize,
    /// When recording started (the zero of [`InputRecording::timestamps`]).
    started_at: regia_platform::InputTimestamp,
    /// The app frame whose input stamps were last recorded (one frame's stamps go to one step).
    stamped_frame: Option<u64>,
}

impl InputRecorder {
    /// Begins recording player-0 actions under `seed`. Action names are read from the
    /// [`InputConfig`](regia_input::InputConfig) resource (every declared action, sorted) so a
    /// game does not need to list them; pass [`InputRecorder::record_with`] to track a subset.
    pub fn record(&mut self, world: &World, seed: u64) {
        let mut actions: Vec<String> = world
            .get_resource::<regia_input::InputConfig>()
            .map(|config| config.actions.keys().cloned().collect())
            .unwrap_or_default();
        actions.sort();
        self.record_with(seed, actions);
    }

    /// Begins recording exactly `actions` under `seed`.
    pub fn record_with(&mut self, seed: u64, actions: Vec<String>) {
        self.recording = InputRecording {
            seed,
            actions,
            frames: Vec::new(),
            timestamps: Vec::new(),
        };
        self.cursor = 0;
        self.started_at = regia_platform::InputTimestamp::now();
        self.stamped_frame = None;
        self.mode = RecorderMode::Recording;
    }

    /// Begins playing `recording`. No-op if the recording has no frames.
    pub fn play(&mut self, recording: InputRecording) {
        if recording.is_empty() {
            return;
        }
        self.recording = recording;
        self.cursor = 0;
        self.mode = RecorderMode::Playing;
    }

    /// The recorded input stamps (µs since recording started) of the step played last (it stays
    /// readable after playback ends); empty before the first step, while recording, or for
    /// recordings without timestamps.
    pub fn playback_timestamps(&self) -> &[u64] {
        self.cursor
            .checked_sub(1)
            .and_then(|step| self.recording.timestamps.get(step))
            .map_or(&[], Vec::as_slice)
    }

    /// Stops recording/playing and clears the recording.
    pub fn stop(&mut self) {
        self.mode = RecorderMode::Idle;
    }
}

/// The record/playback step in `FixedUpdate`. A deterministic game orders its own fixed-step
/// systems after this set, so a replay sees the same actions on the same step.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct InputRecordingSet;

/// Installs the recorder resource and the record/playback systems (§ 06 § 6): playback holds
/// each recorded step's values around that step's evaluation; recording snapshots the values
/// the evaluation produced.
pub struct InputRecordingPlugin;

impl Plugin for InputRecordingPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InputRecorder>()
            // The injection holds this step's recorded values for the evaluation that is about
            // to run: per fixed step, not per frame (a hitch frame runs several steps and each
            // must play its own recorded step).
            .add_systems(
                FixedUpdate,
                playback_inject_step.before(regia_input::InputSet::Evaluation),
            )
            .add_systems(
                FixedUpdate,
                (record_input_step, playback_input_step)
                    .chain()
                    .in_set(InputRecordingSet)
                    // Action evaluation advanced into `FixedUpdate` (ADR-4768): recording must
                    // see this step's evaluated values, not last step's.
                    .after(regia_input::InputSet::Evaluation),
            );
    }
}

/// `FixedUpdate`: while recording, snapshots every tracked action's current value.
#[tracing::instrument(skip_all, target = "regia_framework", name = "input_record")]
fn record_input_step(world: &mut World) {
    let Some(mut recorder) = world.remove_resource::<InputRecorder>() else {
        return;
    };
    if recorder.mode != RecorderMode::Recording {
        world.insert_resource(recorder);
        return;
    }
    let names = recorder.recording.actions.clone();
    let frame: Vec<Vec3> = match world.get_resource::<regia_input::Actions>() {
        Some(actions) => names.iter().map(|name| actions.value(name)).collect(),
        None => Vec::new(), // no action layer: nothing to record
    };
    recorder.recording.frames.push(frame);
    // This frame's input stamps go to its first fixed step only.
    let app_frame = world.get_resource::<Time>().map(|t| t.frame_count());
    let stamps = if app_frame.is_some() && recorder.stamped_frame == app_frame {
        Vec::new()
    } else {
        recorder.stamped_frame = app_frame;
        let start = recorder.started_at.since_epoch();
        world
            .get_resource::<regia_platform::InputTiming>()
            .map(|timing| {
                timing
                    .events
                    .iter()
                    .map(|stamp| {
                        u64::try_from(stamp.since_epoch().saturating_sub(start).as_micros())
                            .unwrap_or(u64::MAX)
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    recorder.recording.timestamps.push(stamps);
    world.insert_resource(recorder);
}

/// `FixedUpdate` (before [`InputSet::Evaluation`](regia_input::InputSet::Evaluation)): while
/// playing, holds the values of the next recorded step for the fixed step about to be
/// evaluated, so the step that consumes them sees what the recording saw on the same step
/// (module docs, "Playback timing"). Holding — instead of the old frame-windowed schedule —
/// keys the injection to the *step*: a frame that runs three fixed steps holds three different
/// values in turn, and none of the recording's steps is skipped (fixrev review, finding 2).
#[tracing::instrument(skip_all, target = "regia_framework", name = "input_playback_inject")]
fn playback_inject_step(recorder: Option<Res<InputRecorder>>, injection: Option<ResMut<InputInjection>>) {
    let (Some(recorder), Some(mut injection)) = (recorder, injection) else {
        return;
    };
    if recorder.mode != RecorderMode::Playing {
        return;
    }
    let Some(frame) = recorder.recording.frames.get(recorder.cursor) else {
        return;
    };
    for (index, name) in recorder.recording.actions.iter().enumerate() {
        let value = frame.get(index).copied().unwrap_or(Vec3::ZERO);
        injection.hold(name.clone(), value);
    }
}

/// `FixedUpdate`: while playing, releases the values this step held (its evaluation consumed
/// them; the actions read their devices again until the next step holds the next recorded
/// values), consumes one recorded step and advances the cursor. Ends (returns to
/// [`RecorderMode::Idle`]) after the final step.
#[tracing::instrument(skip_all, target = "regia_framework", name = "input_playback")]
fn playback_input_step(world: &mut World) {
    let Some(mut recorder) = world.remove_resource::<InputRecorder>() else {
        return;
    };
    if recorder.mode != RecorderMode::Playing {
        world.insert_resource(recorder);
        return;
    }
    if let Some(mut injection) = world.get_resource_mut::<InputInjection>() {
        for name in &recorder.recording.actions {
            injection.release(name);
        }
    }
    if recorder.cursor < recorder.recording.frames.len() {
        recorder.cursor += 1;
    }
    if recorder.cursor >= recorder.recording.frames.len() {
        recorder.mode = RecorderMode::Idle;
    }
    world.insert_resource(recorder);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins((
            regia_input::InputPlugin::new(
                regia_input::InputConfig::new()
                    .action("Move", regia_input::ActionKind::Axis2D)
                    .action("Fire", regia_input::ActionKind::Button),
            ),
            InputRecordingPlugin,
        ));
        enable_fixed_step(&mut app, 60.0);
        app
    }

    /// Injects a `Move` value for one frame.
    fn inject(app: &mut App, action: &str, value: Vec3) {
        // `Time.frame_count` has not advanced for the frame we are about to run; schedule at the
        // *next* frame so the value is visible to the frame's `evaluate_actions`.
        let frame = app.world().resource::<Time>().frame_count() + 1;
        app.world_mut()
            .resource_mut::<InputInjection>()
            .schedule(action.to_string(), value, frame, 1);
    }

    /// Hold latches change trigger phases, not the recorded action values or binary encoding.
    #[test]
    fn independent_hold_bindings_keep_recordings_byte_identical_and_replay_values() {
        use regia_input::{ActionKind, Actions, Binding, InputConfig, InputPlugin, InputSource, Trigger};
        use regia_platform::{ButtonState, InputTimestamp, KeyboardInput};

        fn hold_app(hold: bool) -> App {
            let mut config = InputConfig::new().action("Charge", ActionKind::Button);
            for key in ["KeyA", "KeyB"] {
                let binding = Binding::new("Charge", InputSource::key(key));
                config = config.binding(if hold {
                    binding.with_trigger(Trigger::Hold(0.2))
                } else {
                    binding
                });
            }
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, InputPlugin::new(config), InputRecordingPlugin));
            enable_fixed_step(&mut app, 60.0);
            app
        }

        let record = |hold| {
            let mut app = hold_app(hold);
            app.world_mut()
                .resource_mut::<InputRecorder>()
                .record_with(21, vec!["Charge".into()]);
            let mut fires = 0;
            for step in 0..80 {
                let edge = match step {
                    0 => Some(("KeyA", ButtonState::Pressed)),
                    30 => Some(("KeyB", ButtonState::Pressed)),
                    60 => Some(("KeyB", ButtonState::Released)),
                    65 => Some(("KeyA", ButtonState::Released)),
                    _ => None,
                };
                if let Some((key, state)) = edge {
                    app.world_mut().write_message(KeyboardInput {
                        window: Entity::PLACEHOLDER,
                        physical_key: key.into(),
                        logical_key: key.into(),
                        state,
                        repeat: false,
                        timestamp: InputTimestamp::ZERO,
                    });
                }
                app.update();
                fires += u32::from(app.world().resource::<Actions>().triggered("Charge"));
            }
            if hold {
                assert_eq!(fires, 2, "each independently held binding fires");
            }
            let recording = &app.world().resource::<InputRecorder>().recording;
            recording
                .to_frec(DemoHeader::new("hold-fixture", 0, 60, 0), Vec::new())
                .expect("encode")
        };
        let bytes = record(true);
        assert_eq!(
            bytes,
            record(true),
            "repeated fixed input produces identical bytes"
        );
        assert_eq!(
            bytes,
            record(false),
            "Hold latches do not change the recorded value stream"
        );
        let recording = decode_frec(&bytes).expect("decode").to_input_recording();
        let expected = recording.frames.clone();
        let mut replay = hold_app(true);
        replay.world_mut().resource_mut::<InputRecorder>().play(recording);
        for row in expected {
            replay.update();
            assert_eq!(replay.world().resource::<Actions>().value("Charge"), row[0]);
        }
        assert_eq!(
            replay.world().resource::<InputRecorder>().mode,
            RecorderMode::Idle
        );
    }

    #[test]
    fn recording_captures_action_values_per_step() {
        let mut app = app();
        app.world_mut()
            .resource_mut::<InputRecorder>()
            .record_with(42, vec!["Move".to_string(), "Fire".to_string()]);

        inject(&mut app, "Move", Vec3::new(1.0, 0.0, 0.0));
        app.update();
        inject(&mut app, "Move", Vec3::new(0.0, 1.0, 0.0));
        inject(&mut app, "Fire", Vec3::X);
        app.update();
        app.world_mut().resource_mut::<InputRecorder>().stop();

        let recording = app.world().resource::<InputRecorder>().recording.clone();
        assert_eq!(recording.seed, 42);
        assert_eq!(recording.actions, vec!["Move".to_string(), "Fire".to_string()]);
        assert_eq!(recording.frames.len(), 2, "two fixed steps recorded");
        assert_eq!(recording.frames[0], vec![Vec3::new(1.0, 0.0, 0.0), Vec3::ZERO]);
        assert_eq!(recording.frames[1], vec![Vec3::new(0.0, 1.0, 0.0), Vec3::X]);
    }

    #[test]
    fn playback_schedules_recorded_values_then_goes_idle() {
        let mut app = app();
        let recording = InputRecording {
            seed: 7,
            actions: vec!["Fire".to_string()],
            frames: vec![vec![Vec3::X], vec![Vec3::ZERO]],
            timestamps: vec![vec![5], vec![]],
        };

        app.world_mut().resource_mut::<InputRecorder>().play(recording);
        app.update();
        assert_eq!(
            app.world().resource::<InputRecorder>().mode,
            RecorderMode::Playing
        );
        app.update();
        assert_eq!(
            app.world().resource::<InputRecorder>().mode,
            RecorderMode::Idle,
            "playback ends after the last frame"
        );
    }

    #[test]
    fn playback_injects_recorded_values_into_actions() {
        let mut recorded = app();
        recorded
            .world_mut()
            .resource_mut::<InputRecorder>()
            .record_with(3, vec!["Move".to_string()]);
        inject(&mut recorded, "Move", Vec3::new(0.25, -0.5, 0.0));
        recorded.update();
        recorded.world_mut().resource_mut::<InputRecorder>().stop();
        let recording = recorded.world().resource::<InputRecorder>().recording.clone();
        assert_eq!(recording.frames, vec![vec![Vec3::new(0.25, -0.5, 0.0)]]);

        let mut replay = app();
        replay.world_mut().resource_mut::<InputRecorder>().play(recording);
        replay.update();
        assert_eq!(
            replay.world().resource::<regia_input::Actions>().value("Move"),
            Vec3::new(0.25, -0.5, 0.0),
            "the first replayed frame's evaluation receives the first recorded step (ADR-867)"
        );
        assert_eq!(
            replay.world().resource::<InputRecorder>().mode,
            RecorderMode::Idle,
            "a one-step recording ends after one frame"
        );
        replay.update();
        assert_eq!(
            replay.world().resource::<regia_input::Actions>().value("Move"),
            Vec3::ZERO,
            "nothing is injected after the recording ends"
        );
    }

    /// What a fixed step saw: (frame, Move, Fire just pressed, steps run so far).
    #[derive(Resource, Default)]
    struct StepTrace(Vec<(u64, Vec3, bool, u32)>);

    /// A fixed-step "game" that reads the actions after the recorder and also counts its own steps
    /// (the part of a game that moves without input).
    fn trace_step(time: Res<Time>, actions: Res<regia_input::Actions>, mut trace: ResMut<StepTrace>) {
        let steps = u32::try_from(trace.0.len()).unwrap_or(u32::MAX);
        trace.0.push((
            time.frame_count(),
            actions.value("Move"),
            actions.just_pressed("Fire"),
            steps,
        ));
    }

    fn traced_app() -> App {
        let mut app = app();
        app.init_resource::<StepTrace>()
            .add_systems(FixedUpdate, trace_step.after(InputRecordingSet));
        app
    }

    #[test]
    fn replay_steps_see_the_recorded_steps_values_and_edges() {
        const STEPS: u64 = 40;
        let mut recorded = traced_app();
        recorded
            .world_mut()
            .resource_mut::<InputRecorder>()
            .record_with(5, vec!["Fire".to_string(), "Move".to_string()]);
        for step in 0..STEPS {
            let x = (step % 7) as f32 * 0.125 - 0.375;
            inject(&mut recorded, "Move", Vec3::new(x, 0.5, 0.0));
            // Fire pressed for two frames out of five: a press edge every five frames.
            if step % 5 < 2 {
                inject(&mut recorded, "Fire", Vec3::X);
            }
            recorded.update();
        }
        recorded.world_mut().resource_mut::<InputRecorder>().stop();
        let recording = recorded.world().resource::<InputRecorder>().recording.clone();
        let original = std::mem::take(&mut recorded.world_mut().resource_mut::<StepTrace>().0);
        assert_eq!(original.len(), STEPS as usize);
        assert!(
            original.iter().filter(|(_, _, fire, _)| *fire).count() >= 8,
            "the recording has press edges: {original:?}"
        );

        let mut replay = traced_app();
        replay.world_mut().resource_mut::<InputRecorder>().play(recording);
        for _ in 0..STEPS {
            replay.update();
        }
        assert_eq!(
            replay.world().resource::<InputRecorder>().mode,
            RecorderMode::Idle,
            "playback ends on the last recorded step, not one frame later"
        );
        let replayed = std::mem::take(&mut replay.world_mut().resource_mut::<StepTrace>().0);
        assert_eq!(
            replayed, original,
            "every replayed step sees the recorded step's values and press edges on the same frame"
        );
    }

    #[test]
    fn playing_an_empty_recording_is_a_no_op() {
        let mut app = app();
        app.world_mut()
            .resource_mut::<InputRecorder>()
            .play(InputRecording::default());
        assert_eq!(app.world().resource::<InputRecorder>().mode, RecorderMode::Idle);
    }

    #[test]
    fn record_reads_action_names_from_the_action_layer() {
        let mut app = app();
        let mut recorder = app
            .world_mut()
            .remove_resource::<InputRecorder>()
            .expect("recorder");
        recorder.record(app.world(), 9);
        app.world_mut().insert_resource(recorder);
        let recording = app.world().resource::<InputRecorder>().recording.clone();
        // Both declared actions are tracked, sorted by name.
        assert_eq!(recording.actions, vec!["Fire".to_string(), "Move".to_string()]);
        assert_eq!(recording.seed, 9);
    }

    /// A sim for match-record tests: a sum and a step count.
    #[derive(Default)]
    struct Sum(i64, u64);

    impl crate::demo::DemoSim for Sum {
        fn step(&mut self, inputs: &[Vec3]) {
            self.0 += inputs.first().map_or(0, |v| (v.x * 8.0) as i64);
            self.1 += 1;
        }
        fn state_hash(&self) -> u64 {
            crate::state_hash::fnv1a(format!("{}/{}", self.0, self.1).as_bytes())
        }
        fn snapshot(&self) -> Vec<u8> {
            [self.0.to_le_bytes(), self.1.to_le_bytes()].concat()
        }
        fn restore(&mut self, snapshot: &[u8]) -> Result<(), String> {
            let (chunks, rest) = snapshot.as_chunks::<8>();
            let ([a, b], []) = (chunks, rest) else {
                return Err("expected 16 bytes".to_string());
            };
            self.0 = i64::from_le_bytes(*a);
            self.1 = u64::from_le_bytes(*b);
            Ok(())
        }
    }

    fn match_demo(steps: u64) -> DemoRecording {
        let mut sim = Sum::default();
        let header = DemoHeader::new("dock", 11, 60, 4)
            .with_build("b1")
            .with_content("c1");
        let mut rec = crate::demo::DemoRecorder::begin(header, vec!["Move".to_string()], &sim);
        for k in 0..steps {
            let row = [Vec3::new((k % 5) as f32 * 0.25, 0.0, 0.0)];
            rec.record_step_stamped(&mut sim, &row, &[k * 16_667])
                .expect("record");
        }
        rec.finish(&sim)
    }

    #[test]
    fn frec_round_trips_header_inputs_timestamps_and_keyframes() {
        let demo = match_demo(19);
        let bytes = encode_frec(&demo).expect("encode");
        assert_eq!(&bytes[..4], FREC_MAGIC);
        assert_eq!(bytes[4..8], FREC_FORMAT_VERSION.to_le_bytes());
        let back = decode_frec(&bytes).expect("decode");
        assert_eq!(back, demo, "every field survives");
        assert_eq!(back.header.engine_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            (back.header.build_hash.as_str(), back.header.content_hash.as_str()),
            ("b1", "c1")
        );
        assert_eq!(back.timestamps_at(3), &[3 * 16_667]);
        // Every keyframe carries its hash, and a seek on the decoded record is exact.
        assert_eq!(back.keyframes.len(), 6, "0, 4, 8, 12, 16 and the end (19)");
        let report = back.seek(&mut Sum::default(), 14).expect("seek");
        assert_eq!((report.keyframe_tick, report.steps_replayed), (12, 2));
        // Binary is smaller than the RON text for the same recording.
        assert!(bytes.len() < demo.to_ron().expect("ron").len());
    }

    #[test]
    fn frec_encoding_is_deterministic() {
        assert_eq!(encode_frec(&match_demo(9)), encode_frec(&match_demo(9)));
    }

    #[test]
    fn every_truncation_of_a_frec_is_truncated() {
        let bytes = encode_frec(&match_demo(7)).expect("encode");
        for cut in 0..bytes.len() {
            let err = decode_frec(&bytes[..cut]).expect_err("cut");
            assert_eq!(err.code(), "truncated", "cut at {cut}: {err}");
        }
    }

    #[test]
    fn frec_integrity_errors_are_named() {
        let bytes = encode_frec(&match_demo(7)).expect("encode");
        let mut flipped = bytes.clone();
        let middle = FREC_HEAD_BYTES + 3;
        flipped[middle] ^= 0x40;
        assert_eq!(decode_frec(&flipped).expect_err("crc").code(), "corrupt");
        let mut magic = bytes.clone();
        magic[0] = b'X';
        assert_eq!(decode_frec(&magic).expect_err("magic").code(), "corrupt");
        assert_eq!(decode_frec(b"PK\x03\x04").expect_err("zip").code(), "corrupt");
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(decode_frec(&trailing).expect_err("trailing").code(), "corrupt");
        let mut future = bytes.clone();
        future[4..8].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(decode_frec(&future), Err(DemoError::UnsupportedVersion(2)));
        let mut zero = bytes;
        zero[4..8].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(decode_frec(&zero), Err(DemoError::UnsupportedVersion(0)));
    }

    #[test]
    fn a_huge_declared_payload_is_truncated_not_an_allocation() {
        let mut bytes = encode_frec(&match_demo(2)).expect("encode");
        bytes[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        let err = decode_frec(&bytes).expect_err("len");
        assert!(matches!(err.code(), "truncated" | "too_large"), "{err}");
    }

    #[test]
    fn an_inconsistent_demo_is_never_encoded() {
        let mut demo = match_demo(3);
        demo.header.step_count = 9;
        assert_eq!(encode_frec(&demo).expect_err("invalid").code(), "invalid");
    }

    #[test]
    fn a_recorded_input_stream_writes_a_frec_and_plays_back() {
        let mut recorded = app();
        recorded
            .world_mut()
            .resource_mut::<InputRecorder>()
            .record_with(21, vec!["Move".to_string()]);
        for i in 0..5 {
            inject(&mut recorded, "Move", Vec3::new(i as f32 * 0.5, 0.0, 0.0));
            recorded.update();
        }
        recorded.world_mut().resource_mut::<InputRecorder>().stop();
        let recording = recorded.world().resource::<InputRecorder>().recording.clone();
        assert_eq!(recording.timestamps.len(), 5, "one stamp list per step");
        let bytes = recording
            .to_frec(DemoHeader::new("arena", 0, 60, 0), Vec::new())
            .expect("frec");
        let demo = decode_frec(&bytes).expect("decode");
        assert_eq!(demo.header.seed, 21, "the seed comes from the recording");
        let back = demo.to_input_recording();
        assert_eq!(back.frames, recording.frames);
        assert_eq!(back.timestamps, recording.timestamps);
        // Keyframes past the end are refused.
        let late = vec![DemoKeyframe {
            tick: 9,
            state_hash: 0,
            snapshot_hex: String::new(),
        }];
        assert_eq!(
            recording
                .to_frec(DemoHeader::new("arena", 0, 60, 0), late)
                .expect_err("late")
                .code(),
            "invalid"
        );
    }

    #[test]
    fn frec_files_save_load_and_load_any_tells_the_forms_apart() {
        let demo = match_demo(10);
        let dir = regia_test_utils::unique_temp_dir("frec-files");
        let frec = dir.join("m.frec");
        let ron = dir.join("m.regiademo");
        save_frec(&demo, &frec).expect("save frec");
        demo.save(&ron).expect("save ron");
        assert_eq!(load_frec(&frec).expect("load"), demo);
        assert_eq!(DemoRecording::load_any(&frec).expect("any frec"), demo);
        assert_eq!(DemoRecording::load_any(&ron).expect("any ron"), demo);
        assert_eq!(load_frec(&ron).expect_err("ron is not frec").code(), "corrupt");
        assert_eq!(
            load_frec(&dir.join("none.frec")).expect_err("missing").code(),
            "io"
        );
        assert_eq!(
            std::fs::read_dir(&dir).expect("dir").count(),
            2,
            "no temp files left"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn frec_fingerprint_names_the_payload_fields() {
        let fingerprint = frec_schema_fingerprint();
        for field in [
            "timestamps",
            "keyframes",
            "build_hash",
            "content_hash",
            "snapshot",
        ] {
            assert!(fingerprint.contains(field), "{field}: {fingerprint}");
        }
        assert!(frec_migration_sources().is_empty());
    }

    /// Each fixed step of a replay sees the value recorded on the same step even when one frame
    /// runs several fixed steps (a 50 ms hitch at 60 Hz runs three): the playback injection is
    /// per step, so the three steps of one frame play three recorded steps. Before the fix the
    /// injection was frame-windowed — every step of the frame evaluated step `k`'s values and
    /// the cursor advanced past `k+1` and `k+2`, which were never played (fixrev review,
    /// finding 2).
    #[test]
    fn playback_matches_steps_across_a_multi_step_frame() {
        #[derive(Resource, Default)]
        struct Seen(Vec<Vec3>);
        fn observe(actions: Res<regia_input::Actions>, mut seen: ResMut<Seen>) {
            seen.0.push(actions.value("Move"));
        }
        let traced = |steps_per_frame: u32| {
            let mut app = app();
            // Derive the frame delta from the real step duration: 1/60 s is not representable
            // in nanoseconds (16,666,667 ns), so a hand-written "3/60 s" frame is 1 ns short
            // of three steps and the third step lands in the next frame.
            let step = FixedTime::default().step();
            app.insert_resource(TimeUpdateStrategy::ManualDuration(
                step.mul_f64(f64::from(steps_per_frame)),
            ));
            let mut fixed = FixedTime::default();
            fixed.max_substeps = 5;
            app.insert_resource(fixed);
            app.init_resource::<Seen>()
                .add_systems(FixedUpdate, observe.after(InputRecordingSet));
            app
        };

        let values: Vec<Vec3> = (0..6).map(|i| Vec3::new(i as f32 * 0.25, -0.5, 0.0)).collect();
        let mut recorded = traced(1);
        recorded
            .world_mut()
            .resource_mut::<InputRecorder>()
            .record_with(5, vec!["Move".to_string()]);
        for value in &values {
            inject(&mut recorded, "Move", *value);
            recorded.update();
        }
        recorded.world_mut().resource_mut::<InputRecorder>().stop();
        let recording = recorded.world().resource::<InputRecorder>().recording.clone();
        let recorded_seen = recorded.world().resource::<Seen>().0.clone();
        assert_eq!(recorded_seen, values, "one fixed step per frame while recording");

        // The replay runs two frames of three steps each: a hitch from the start.
        let mut replay = traced(3);
        replay.world_mut().resource_mut::<InputRecorder>().play(recording);
        replay.update();
        replay.update();
        assert_eq!(
            replay.world().resource::<Seen>().0,
            recorded_seen,
            "every recorded step is played exactly once, on its own fixed step"
        );
        assert_eq!(
            replay.world().resource::<InputRecorder>().mode,
            RecorderMode::Idle,
            "playback ends on the last recorded step"
        );
    }

    /// Each fixed step of a replay sees the value recorded on the same step, for a recording whose
    /// values change every step.
    #[test]
    fn playback_matches_the_recording_step_for_step() {
        #[derive(Resource, Default)]
        struct Seen(Vec<Vec3>);
        fn observe(actions: Res<regia_input::Actions>, mut seen: ResMut<Seen>) {
            seen.0.push(actions.value("Move"));
        }
        let traced = || {
            let mut app = app();
            app.init_resource::<Seen>()
                .add_systems(FixedUpdate, observe.after(InputRecordingSet));
            app
        };
        let values: Vec<Vec3> = (0..6).map(|i| Vec3::new(i as f32 * 0.25, -0.5, 0.0)).collect();
        let mut recorded = traced();
        recorded
            .world_mut()
            .resource_mut::<InputRecorder>()
            .record_with(5, vec!["Move".to_string()]);
        for value in &values {
            inject(&mut recorded, "Move", *value);
            recorded.update();
        }
        recorded.world_mut().resource_mut::<InputRecorder>().stop();
        let recording = recorded.world().resource::<InputRecorder>().recording.clone();
        let recorded_seen = recorded.world().resource::<Seen>().0.clone();
        assert_eq!(recorded_seen, values, "one fixed step per frame while recording");

        let mut replay = traced();
        replay.world_mut().resource_mut::<InputRecorder>().play(recording);
        for _ in 0..values.len() {
            replay.update();
        }
        assert_eq!(
            replay.world().resource::<Seen>().0,
            recorded_seen,
            "step k of the replay sees step k of the recording"
        );
        assert_eq!(
            replay.world().resource::<InputRecorder>().mode,
            RecorderMode::Idle
        );
    }
}
