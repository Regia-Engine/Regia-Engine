// SPDX-License-Identifier: MPL-2.0
//! Demo recording for ECS games (`book/agents/demo.md`, ADR-1569): a self-describing file around
//! the input stream of [`crate::input_recording`], with periodic keyframes so a viewer can jump
//! to any tick without replaying from the start.
//!
//! # What a demo holds
//!
//! A [`DemoRecording`] is a [`DemoHeader`] (engine version, build hash, content hash, RNG seed,
//! level, tick rate, step count, keyframe interval), the tracked action names, one input row per
//! fixed step (the same values an [`InputRecording`] carries) and a list of [`DemoKeyframe`]s.
//! A keyframe is `(tick, state hash, optional snapshot)`: the hash names the simulation state
//! *before* step `tick` runs, so the keyframe at tick 0 is the initial state and the keyframe at
//! `step_count` is the final state. Since `format_version` 2 a demo also keeps the input
//! timestamps of [`InputRecording`] (per step, microseconds since recording started): analysis
//! data that never drives a replay (see [`crate::input_recording`], "Timestamps").
//!
//! # The simulation seam
//!
//! The recorder and the player never touch a world. A game plugs its simulation in through
//! [`DemoSim`]: `step` (one fixed step from one input row), `state_hash`, and `snapshot` /
//! `restore` (opaque bytes the game defines). An ECS game does not have to write one:
//! [`crate::demo_world`] implements the seam for a whole world (every entity marked
//! `DemoTracked` plus the registered demo resources, ADR-1878). A keyframe without a snapshot
//! still verifies a replay but cannot be a seek start.
//!
//! # Seeking
//!
//! [`DemoRecording::seek`] restores the nearest snapshot keyframe at or before the target tick,
//! checks the restored state against the keyframe's hash, then replays the recorded inputs up to
//! the target. Every keyframe passed on the way is checked too, so a replay that diverges (a
//! different build, a changed content file, a nondeterministic system) is reported as
//! [`DemoError::Diverged`] at the first keyframe where it happens instead of silently showing
//! the wrong state. A seek to tick `t` ends in exactly the state a straight run reaches at `t`;
//! a test pins that for every tick of a demo.
//!
//! # File format
//!
//! RON text through serde with a `format_version` in the header (currently
//! [`DEMO_FORMAT_VERSION`]), followed by the end marker line `// regia-demo-end`. The marker is
//! how a cut-off file is told apart from a corrupt one: a file without it is
//! [`DemoError::Truncated`], and the header's `step_count` must equal the number of input rows.
//! [`DemoRecording::save`] writes through a temp file and a rename, so a reader never sees half a
//! demo. Version 1 files (no `timestamps`) still load: [`DemoRecording::from_ron`] migrates them
//! to the current version with empty timestamps, and `regia upgrade-project` rewrites them.
//!
//! The same recording also has a compact binary form, the `.frec` match record
//! ([`crate::input_recording`], "Match records"); [`DemoRecording::load_any`] reads either.

use std::fmt::Write as _;
use std::path::Path;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use serde::{Deserialize, Serialize};

use crate::input_recording::InputRecording;
use crate::state_hash::{snapshot_hash, world_snapshot, world_snapshot_where};

/// The current demo file `format_version` (2: input timestamps, ADR-1878).
pub const DEMO_FORMAT_VERSION: u32 = 2;
/// The oldest demo `format_version` this build reads (migrated on load).
pub const DEMO_OLDEST_FORMAT_VERSION: u32 = 1;
/// The file extension of a RON demo.
pub const DEMO_EXTENSION: &str = "regiademo";
/// Largest demo file [`DemoRecording::load`] reads.
pub const MAX_DEMO_BYTES: u64 = 256 * 1024 * 1024;
/// The last line of every complete demo file.
pub const DEMO_END_MARKER: &str = "// regia-demo-end";

/// Why a demo could not be written, read, or replayed. [`DemoError::code`] gives the stable
/// agent-protocol code.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DemoError {
    /// The file does not end with the end marker: it was cut off (a crash while writing, a
    /// partial copy).
    #[error("demo file is truncated (no end marker after {bytes} bytes)")]
    Truncated {
        /// Bytes that were present.
        bytes: usize,
    },
    /// The file is larger than [`MAX_DEMO_BYTES`].
    #[error("demo is {size} bytes, over the {max} byte limit")]
    TooLarge {
        /// File size.
        size: u64,
        /// The limit.
        max: u64,
    },
    /// The text is not valid RON for a demo.
    #[error("demo does not parse: {0}")]
    Parse(String),
    /// The header's `format_version` is not one this build reads.
    #[error("demo format_version {0} is not supported (this build reads versions 1 and 2)")]
    UnsupportedVersion(u32),
    /// The demo parses but contradicts itself.
    #[error("demo is invalid: {0}")]
    Invalid(String),
    /// Reading or writing the file failed.
    #[error("demo file IO: {0}")]
    Io(String),
    /// An input row does not have one value per tracked action.
    #[error("input row has {got} values, the demo tracks {expected} actions")]
    InputWidth {
        /// Values supplied.
        got: usize,
        /// Tracked actions.
        expected: usize,
    },
    /// The requested tick is past the end of the demo.
    #[error("tick {tick} is past the end of the demo ({steps} steps)")]
    TickOutOfRange {
        /// The requested tick.
        tick: u64,
        /// Steps in the demo.
        steps: u64,
    },
    /// No keyframe at or before the target carries a snapshot to start from.
    #[error("no snapshot keyframe at or before tick {tick}")]
    NoKeyframe {
        /// The requested tick.
        tick: u64,
    },
    /// The simulation could not restore a snapshot.
    #[error("simulation could not restore the keyframe at tick {tick}: {reason}")]
    Restore {
        /// Keyframe tick.
        tick: u64,
        /// The simulation's reason.
        reason: String,
    },
    /// A binary `.frec` record failed its integrity check (bad magic or checksum).
    #[error("match record is corrupt: {0}")]
    Corrupt(String),
    /// The simulation's state hash differs from the recorded one at a keyframe.
    #[error("replay diverged at tick {tick}: recorded hash {expected:016x}, replayed {actual:016x}")]
    Diverged {
        /// Keyframe tick where the hashes first differ.
        tick: u64,
        /// The hash stored in the demo.
        expected: u64,
        /// The hash the replay produced.
        actual: u64,
    },
}

impl DemoError {
    /// A stable snake_case code for the agent protocol.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Truncated { .. } => "truncated",
            Self::TooLarge { .. } => "too_large",
            Self::Parse(_) => "parse",
            Self::UnsupportedVersion(_) => "unsupported_version",
            Self::Invalid(_) => "invalid",
            Self::Io(_) => "io",
            Self::InputWidth { .. } => "input_width",
            Self::TickOutOfRange { .. } => "tick_out_of_range",
            Self::NoKeyframe { .. } => "no_keyframe",
            Self::Restore { .. } => "restore_failed",
            Self::Diverged { .. } => "diverged",
            Self::Corrupt(_) => "corrupt",
        }
    }
}

/// The simulation a demo drives. Implemented by the game (or a test double); the demo code only
/// calls it through this seam.
pub trait DemoSim {
    /// Runs one fixed step from one input row (one value per tracked action, in demo order).
    fn step(&mut self, inputs: &[Vec3]);
    /// A hash of the simulation state: equal states hash equal on every platform and run.
    fn state_hash(&self) -> u64;
    /// Opaque bytes [`DemoSim::restore`] can rebuild this exact state from; empty for a
    /// simulation that cannot snapshot (its keyframes then carry hashes only).
    fn snapshot(&self) -> Vec<u8>;
    /// Replaces the state with a snapshot taken by [`DemoSim::snapshot`].
    ///
    /// # Errors
    /// A reason string when the bytes are not a snapshot this simulation can load.
    fn restore(&mut self, snapshot: &[u8]) -> Result<(), String>;
}

/// The state hash of a reflected ECS world: [`snapshot_hash`] of [`world_snapshot`]. Resources are
/// not part of it (see [`crate::state_hash`] for adding them).
///
/// It sweeps every reflected component, so it is only for worlds that hold simulation state alone.
/// In a real app a `Transform` the camera writes from the frame delta differs between a recording at
/// 144 fps and a replay at 60 even when the simulation is perfect: use [`world_sim_state_hash`].
#[must_use]
pub fn world_state_hash(world: &World) -> u64 {
    snapshot_hash(&world_snapshot(world))
}

/// The state hash of the components `keep` accepts (by short type name), the hash a [`DemoSim`] of a
/// real app wants: name the gameplay components and leave out anything written at frame rate.
#[must_use]
pub fn world_sim_state_hash(world: &World, keep: impl Fn(&str) -> bool) -> u64 {
    snapshot_hash(&world_snapshot_where(world, keep))
}

/// What a demo says about where it came from. Fill the identity fields from the build; replay
/// code can compare them with [`DemoHeader::mismatches`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DemoHeader {
    /// The file's `format_version` ([`DEMO_FORMAT_VERSION`]).
    pub format_version: u32,
    /// The engine version that recorded it (`CARGO_PKG_VERSION` of this crate by default).
    pub engine_version: String,
    /// An identifier of the game build (a commit hash, an ABI hash); empty when unknown.
    pub build_hash: String,
    /// A hash of the content the level loaded (assets, scene); empty when unknown.
    pub content_hash: String,
    /// The RNG seed the simulation was seeded with.
    pub seed: u64,
    /// The level or scene name.
    pub level: String,
    /// Fixed steps per second.
    pub tick_rate_hz: u32,
    /// Input rows in the file (set by [`DemoRecorder::finish`], checked on load).
    pub step_count: u64,
    /// Steps between keyframes (0: only the keyframes at tick 0 and the end).
    pub keyframe_interval: u32,
}

impl DemoHeader {
    /// A header for `level` at `tick_rate_hz`, under `seed`, with a snapshot keyframe every
    /// `keyframe_interval` steps. The engine version is this build's; build and content hashes
    /// start empty (set them with [`DemoHeader::with_build`] and [`DemoHeader::with_content`]).
    #[must_use]
    pub fn new(level: impl Into<String>, seed: u64, tick_rate_hz: u32, keyframe_interval: u32) -> Self {
        Self {
            format_version: DEMO_FORMAT_VERSION,
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
            build_hash: String::new(),
            content_hash: String::new(),
            seed,
            level: level.into(),
            tick_rate_hz,
            step_count: 0,
            keyframe_interval,
        }
    }

    /// Sets the game build identifier.
    #[must_use]
    pub fn with_build(mut self, build_hash: impl Into<String>) -> Self {
        self.build_hash = build_hash.into();
        self
    }

    /// Sets the content hash.
    #[must_use]
    pub fn with_content(mut self, content_hash: impl Into<String>) -> Self {
        self.content_hash = content_hash.into();
        self
    }

    /// Marks the demo as recorded in a session that stopped being reproducible (ADR-9850 (b)):
    /// each tag (`code:source`, `SessionDeterminism::tags`) not already present is appended to
    /// `build_hash` as `!<tag>`. The field is free text, so the frozen `.frec` and RON shapes stay
    /// as they are; [`Self::nondeterministic`] reads the tags back and [`Self::build_id`] the
    /// build without them.
    pub fn mark_nondeterministic(&mut self, tags: &[String]) {
        for tag in tags {
            let tag = tag.replace('!', "_");
            if !self.nondeterministic().contains(&tag.as_str()) {
                self.build_hash.push('!');
                self.build_hash.push_str(&tag);
            }
        }
    }

    /// The nondeterminism tags [`Self::mark_nondeterministic`] recorded (empty for a demo of a
    /// reproducible session).
    #[must_use]
    pub fn nondeterministic(&self) -> Vec<&str> {
        self.build_hash
            .split('!')
            .skip(1)
            .filter(|tag| !tag.is_empty())
            .collect()
    }

    /// `build_hash` without the nondeterminism tags.
    #[must_use]
    pub fn build_id(&self) -> &str {
        self.build_hash.split('!').next().unwrap_or_default()
    }

    /// The identity fields (`engine_version`, `build_hash`, `content_hash`, `level`, `seed`,
    /// `tick_rate_hz`) in which this header differs from `current`, as field names. An empty
    /// `build_hash` or `content_hash` on either side means "unknown" and is not a mismatch.
    /// `build_hash` compares [`Self::build_id`]s; a demo marked nondeterministic (either side)
    /// adds `nondeterministic`, since its keyframe hashes need not repeat (ADR-9850 (b)).
    #[must_use]
    pub fn mismatches(&self, current: &DemoHeader) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.engine_version != current.engine_version {
            out.push("engine_version");
        }
        let known_differ = |a: &str, b: &str| !a.is_empty() && !b.is_empty() && a != b;
        if known_differ(self.build_id(), current.build_id()) {
            out.push("build_hash");
        }
        if known_differ(&self.content_hash, &current.content_hash) {
            out.push("content_hash");
        }
        if self.level != current.level {
            out.push("level");
        }
        if self.seed != current.seed {
            out.push("seed");
        }
        if self.tick_rate_hz != current.tick_rate_hz {
            out.push("tick_rate_hz");
        }
        if !self.nondeterministic().is_empty() || !current.nondeterministic().is_empty() {
            out.push("nondeterministic");
        }
        out
    }
}

/// A point in the demo a seek can start from or a replay can be checked against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DemoKeyframe {
    /// The state is the one *before* step `tick` runs.
    pub tick: u64,
    /// The simulation's [`DemoSim::state_hash`] at that point.
    pub state_hash: u64,
    /// [`DemoSim::snapshot`] bytes as lowercase hex; empty when the simulation gave none.
    pub snapshot_hex: String,
}

impl DemoKeyframe {
    /// Whether a seek can start from this keyframe.
    #[must_use]
    pub fn has_snapshot(&self) -> bool {
        !self.snapshot_hex.is_empty()
    }
}

/// Where a seek started and what it did; see [`DemoRecording::seek`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeekReport {
    /// The keyframe the seek restored.
    pub keyframe_tick: u64,
    /// Fixed steps replayed from that keyframe.
    pub steps_replayed: u64,
    /// The tick the simulation is now at (the requested one).
    pub tick: u64,
    /// The simulation's state hash at `tick`.
    pub state_hash: u64,
    /// Recorded hashes compared on the way (the restored keyframe plus later keyframes passed,
    /// and the keyframe at `tick` itself when there is one).
    pub keyframes_checked: u32,
}

/// A complete demo: header, input rows, keyframes.
#[derive(Debug, Clone, PartialEq)]
pub struct DemoRecording {
    /// Identity and settings.
    pub header: DemoHeader,
    /// Tracked action names, in input-row order.
    pub actions: Vec<String>,
    /// One row per fixed step; each row parallels [`DemoRecording::actions`].
    pub steps: Vec<Vec<Vec3>>,
    /// Keyframes in strictly ascending tick order.
    pub keyframes: Vec<DemoKeyframe>,
    /// Per step (parallel to [`DemoRecording::steps`]), the arrival times of the raw input events
    /// that step consumed, in microseconds since recording started
    /// ([`InputRecording::timestamps`]); empty when the recording has none (version 1 files,
    /// simulations driven without a platform layer). Analysis data: never used by a replay.
    pub timestamps: Vec<Vec<u64>>,
}

/// The serde shape of a demo file (inputs as plain arrays).
#[derive(Serialize, Deserialize)]
struct DemoFile {
    header: DemoHeader,
    actions: Vec<String>,
    steps: Vec<Vec<[f32; 3]>>,
    keyframes: Vec<DemoKeyframe>,
    /// Version 2; absent in version 1 files.
    #[serde(default)]
    timestamps: Vec<Vec<u64>>,
}

/// The serde field-list fingerprint of a `.regiademo` file (hashed by `cargo xtask
/// check-formats`): the file root, its header and its keyframe.
#[must_use]
pub fn demo_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<DemoFile>(),
        regia_core::schema::serde_field_names::<DemoHeader>(),
        regia_core::schema::serde_field_names::<DemoKeyframe>(),
    ])
}

/// The `from` versions of the demo migration steps (`DemoRecording::from_ron` reads version 1 and
/// writes the current version), for `cargo xtask check-formats`.
#[must_use]
pub fn demo_migration_sources() -> Vec<u32> {
    (DEMO_OLDEST_FORMAT_VERSION..DEMO_FORMAT_VERSION).collect()
}

/// The `format_version` of a `.regiademo` file's text, for `regia upgrade-project`: `Ok(None)`
/// when the text is not a demo at all.
///
/// # Errors
/// A reason when the text is a demo whose header cannot be read.
pub fn demo_file_version(text: &str) -> Result<Option<u32>, String> {
    if !text.trim_end().ends_with(DEMO_END_MARKER) {
        return Ok(None);
    }
    ron::from_str::<VersionPeek>(text)
        .map(|peek| Some(peek.header.format_version))
        .map_err(|e| e.to_string())
}

/// Just enough of a demo file to read its version.
#[derive(Deserialize)]
struct VersionPeek {
    header: VersionOnly,
}

#[derive(Deserialize)]
struct VersionOnly {
    format_version: u32,
}

impl DemoRecording {
    /// Number of fixed steps.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.steps.len() as u64
    }

    /// Whether the demo has no steps.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// The demo's duration in seconds at its tick rate (0 for a zero tick rate).
    #[must_use]
    pub fn duration_secs(&self) -> f64 {
        if self.header.tick_rate_hz == 0 {
            0.0
        } else {
            self.len() as f64 / f64::from(self.header.tick_rate_hz)
        }
    }

    /// Wraps an [`InputRecording`] (what [`crate::input_recording::InputRecorder`] captures) as a
    /// demo without keyframes. `header.seed` is taken from the recording; rows shorter than the
    /// action list (a frame with no action layer) are padded with zeros.
    #[must_use]
    pub fn from_input_recording(mut header: DemoHeader, input: &InputRecording) -> Self {
        header.seed = input.seed;
        header.step_count = input.len() as u64;
        let width = input.actions.len();
        let steps = input
            .frames
            .iter()
            .map(|row| {
                let mut row = row.clone();
                row.resize(width, Vec3::ZERO);
                row
            })
            .collect();
        Self {
            header,
            actions: input.actions.clone(),
            steps,
            keyframes: Vec::new(),
            timestamps: Self::stamps_for(input),
        }
    }

    /// The recording's timestamps, one list per step (padded or cut to the step count); empty when
    /// it has none.
    fn stamps_for(input: &InputRecording) -> Vec<Vec<u64>> {
        if input.timestamps.is_empty() {
            return Vec::new();
        }
        let mut stamps = input.timestamps.clone();
        stamps.resize(input.len(), Vec::new());
        stamps
    }

    /// The input stream as an [`InputRecording`], for
    /// [`InputRecorder::play`](crate::input_recording::InputRecorder::play), timestamps included.
    #[must_use]
    pub fn to_input_recording(&self) -> InputRecording {
        InputRecording {
            seed: self.header.seed,
            actions: self.actions.clone(),
            frames: self.steps.clone(),
            timestamps: self.timestamps.clone(),
        }
    }

    /// The recorded input timestamps of step `tick` (empty when the demo has none).
    #[must_use]
    pub fn timestamps_at(&self, tick: u64) -> &[u64] {
        usize::try_from(tick)
            .ok()
            .and_then(|index| self.timestamps.get(index))
            .map_or(&[], Vec::as_slice)
    }

    /// Checks the demo's internal consistency.
    ///
    /// # Errors
    /// [`DemoError::UnsupportedVersion`] or [`DemoError::Invalid`] naming the first problem: a
    /// zero tick rate, a `step_count` that is not the number of rows, a row of the wrong width or
    /// with a non-finite value, a keyframe past the end, keyframe ticks that do not strictly
    /// ascend, snapshot text that is not hex, or timestamp rows that do not match the steps.
    pub fn validate(&self) -> Result<(), DemoError> {
        let invalid = |reason: String| Err(DemoError::Invalid(reason));
        if self.header.format_version != DEMO_FORMAT_VERSION {
            return Err(DemoError::UnsupportedVersion(self.header.format_version));
        }
        if self.header.tick_rate_hz == 0 {
            return invalid("tick_rate_hz is 0".to_string());
        }
        if self.header.step_count != self.len() {
            return invalid(format!(
                "header says {} steps, the file has {}",
                self.header.step_count,
                self.len()
            ));
        }
        for (index, row) in self.steps.iter().enumerate() {
            if row.len() != self.actions.len() {
                return invalid(format!(
                    "step {index} has {} values for {} actions",
                    row.len(),
                    self.actions.len()
                ));
            }
            if row.iter().any(|v| !v.is_finite()) {
                return invalid(format!("step {index} has a non-finite input value"));
            }
        }
        if !self.timestamps.is_empty() && self.timestamps.len() != self.steps.len() {
            return invalid(format!(
                "{} timestamp rows for {} steps",
                self.timestamps.len(),
                self.steps.len()
            ));
        }
        let mut previous: Option<u64> = None;
        for key in &self.keyframes {
            if key.tick > self.len() {
                return invalid(format!(
                    "keyframe tick {} is past the end ({})",
                    key.tick,
                    self.len()
                ));
            }
            if previous.is_some_and(|p| key.tick <= p) {
                return invalid(format!("keyframe ticks do not ascend at {}", key.tick));
            }
            previous = Some(key.tick);
            if decode_hex(&key.snapshot_hex).is_none() {
                return invalid(format!("keyframe {} snapshot is not valid hex", key.tick));
            }
        }
        Ok(())
    }

    /// The demo as RON text with the end marker.
    ///
    /// # Errors
    /// [`DemoError::Invalid`] when the demo fails [`DemoRecording::validate`], or
    /// [`DemoError::Parse`] if RON cannot write it.
    pub fn to_ron(&self) -> Result<String, DemoError> {
        self.validate()?;
        let file = DemoFile {
            header: self.header.clone(),
            actions: self.actions.clone(),
            steps: self
                .steps
                .iter()
                .map(|row| row.iter().map(|v| v.to_array()).collect())
                .collect(),
            keyframes: self.keyframes.clone(),
            timestamps: self.timestamps.clone(),
        };
        let mut text = ron::to_string(&file).map_err(|e| DemoError::Parse(e.to_string()))?;
        text.push('\n');
        text.push_str(DEMO_END_MARKER);
        text.push('\n');
        Ok(text)
    }

    /// Reads a demo from RON text. A version 1 file is migrated: it loads with
    /// `format_version` set to the current version and no timestamps.
    ///
    /// # Errors
    /// [`DemoError::Truncated`] without the end marker, [`DemoError::UnsupportedVersion`] for a
    /// `format_version` this build does not read, [`DemoError::Parse`] for malformed RON, and
    /// [`DemoError::Invalid`] from [`DemoRecording::validate`].
    pub fn from_ron(text: &str) -> Result<Self, DemoError> {
        if !text.trim_end().ends_with(DEMO_END_MARKER) {
            return Err(DemoError::Truncated { bytes: text.len() });
        }
        let peek: VersionPeek = ron::from_str(text).map_err(|e| DemoError::Parse(e.to_string()))?;
        let version = peek.header.format_version;
        if !(DEMO_OLDEST_FORMAT_VERSION..=DEMO_FORMAT_VERSION).contains(&version) {
            return Err(DemoError::UnsupportedVersion(version));
        }
        let mut file: DemoFile = ron::from_str(text).map_err(|e| DemoError::Parse(e.to_string()))?;
        if version == 1 {
            // v1 -> v2: timestamps did not exist; a v1 file that carries the field is not v1.
            if !file.timestamps.is_empty() {
                return Err(DemoError::Invalid(
                    "a format_version 1 demo carries timestamps (a version 2 field)".to_string(),
                ));
            }
            file.header.format_version = DEMO_FORMAT_VERSION;
        }
        Self::from_file(file)
    }

    fn from_file(file: DemoFile) -> Result<Self, DemoError> {
        let demo = Self {
            header: file.header,
            actions: file.actions,
            steps: file
                .steps
                .into_iter()
                .map(|row| row.into_iter().map(Vec3::from_array).collect())
                .collect(),
            keyframes: file.keyframes,
            timestamps: file.timestamps,
        };
        demo.validate()?;
        Ok(demo)
    }

    /// Reads a demo in either form: a binary `.frec` match record (recognized by its magic) or a
    /// RON `.regiademo`.
    ///
    /// # Errors
    /// The errors of [`DemoRecording::load`] or [`crate::input_recording::load_frec`].
    pub fn load_any(path: &Path) -> Result<Self, DemoError> {
        let mut magic = [0u8; 4];
        let is_frec = std::fs::File::open(path)
            .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut magic))
            .is_ok()
            && &magic == crate::input_recording::FREC_MAGIC;
        if is_frec {
            crate::input_recording::load_frec(path)
        } else {
            Self::load(path)
        }
    }

    /// Writes the demo to `path` through a temp file and a rename.
    ///
    /// # Errors
    /// The errors of [`DemoRecording::to_ron`], or [`DemoError::Io`].
    pub fn save(&self, path: &Path) -> Result<(), DemoError> {
        let text = self.to_ron()?;
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{}.tmp", std::process::id()));
        let tmp = path.with_file_name(name);
        std::fs::write(&tmp, text).map_err(|e| DemoError::Io(e.to_string()))?;
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            DemoError::Io(e.to_string())
        })
    }

    /// Reads a demo file (at most [`MAX_DEMO_BYTES`]).
    ///
    /// Only regular files are read (a FIFO, device or directory is [`DemoError::Io`]: `/dev/zero`
    /// reports size 0 and would read without end), through one descriptor whose read is bounded
    /// by [`MAX_DEMO_BYTES`] even if the file grows after the size check.
    ///
    /// # Errors
    /// [`DemoError::Io`], [`DemoError::TooLarge`], or any error of [`DemoRecording::from_ron`].
    pub fn load(path: &Path) -> Result<Self, DemoError> {
        use std::io::Read as _;
        let not_regular = || DemoError::Io("not a regular file".into());
        // Checked before `open` too: opening a FIFO for reading blocks until a writer appears.
        if !std::fs::metadata(path)
            .map_err(|e| DemoError::Io(e.to_string()))?
            .is_file()
        {
            return Err(not_regular());
        }
        let file = std::fs::File::open(path).map_err(|e| DemoError::Io(e.to_string()))?;
        let meta = file.metadata().map_err(|e| DemoError::Io(e.to_string()))?;
        if !meta.is_file() {
            return Err(not_regular());
        }
        let too_large = |size| DemoError::TooLarge {
            size,
            max: MAX_DEMO_BYTES,
        };
        if meta.len() > MAX_DEMO_BYTES {
            return Err(too_large(meta.len()));
        }
        let mut bytes = Vec::new();
        // One byte past the cap tells "exactly at the cap" from "grew past it".
        file.take(MAX_DEMO_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| DemoError::Io(e.to_string()))?;
        if bytes.len() as u64 > MAX_DEMO_BYTES {
            return Err(too_large(bytes.len() as u64));
        }
        let text = String::from_utf8(bytes).map_err(|e| DemoError::Parse(e.to_string()))?;
        Self::from_ron(&text)
    }

    /// The keyframe a seek to `tick` would start from: the latest keyframe at or before `tick`
    /// that carries a snapshot.
    #[must_use]
    pub fn nearest_keyframe(&self, tick: u64) -> Option<&DemoKeyframe> {
        let end = self.keyframes.partition_point(|key| key.tick <= tick);
        self.keyframes[..end].iter().rev().find(|key| key.has_snapshot())
    }

    /// Puts `sim` at `tick`: restores the nearest snapshot keyframe, then replays inputs (see the
    /// module docs, "Seeking").
    ///
    /// # Errors
    /// [`DemoError::TickOutOfRange`] past the end, [`DemoError::NoKeyframe`],
    /// [`DemoError::Restore`], or [`DemoError::Diverged`].
    #[tracing::instrument(skip_all, target = "regia_framework", name = "demo_seek")]
    pub fn seek(&self, sim: &mut dyn DemoSim, tick: u64) -> Result<SeekReport, DemoError> {
        if tick > self.len() {
            return Err(DemoError::TickOutOfRange {
                tick,
                steps: self.len(),
            });
        }
        let start = self
            .nearest_keyframe(tick)
            .ok_or(DemoError::NoKeyframe { tick })?;
        self.run(sim, start, tick)
    }

    /// Replays the whole demo from its first snapshot keyframe to the end, checking every
    /// keyframe hash. Use it to validate a demo against the current build.
    ///
    /// # Errors
    /// As [`DemoRecording::seek`] to the final tick.
    pub fn verify(&self, sim: &mut dyn DemoSim) -> Result<SeekReport, DemoError> {
        let end = self.len();
        let start = self
            .keyframes
            .iter()
            .find(|key| key.has_snapshot())
            .ok_or(DemoError::NoKeyframe { tick: 0 })?;
        self.run(sim, start, end)
    }

    fn run(&self, sim: &mut dyn DemoSim, start: &DemoKeyframe, target: u64) -> Result<SeekReport, DemoError> {
        // A recording built in memory (its fields are public) can hold a keyframe past its steps;
        // `validate` refuses that for loaded files, and `verify` must not count steps backwards.
        if start.tick > target {
            return Err(DemoError::TickOutOfRange {
                tick: start.tick,
                steps: self.len(),
            });
        }
        let snapshot = decode_hex(&start.snapshot_hex).ok_or_else(|| {
            DemoError::Invalid(format!("keyframe {} snapshot is not valid hex", start.tick))
        })?;
        sim.restore(&snapshot).map_err(|reason| DemoError::Restore {
            tick: start.tick,
            reason,
        })?;
        let mut checked = 0u32;
        let mut check = |sim: &dyn DemoSim, key: &DemoKeyframe| {
            let actual = sim.state_hash();
            checked += 1;
            if actual == key.state_hash {
                Ok(())
            } else {
                Err(DemoError::Diverged {
                    tick: key.tick,
                    expected: key.state_hash,
                    actual,
                })
            }
        };
        check(&*sim, start)?;
        // Keyframes strictly after the start, up to and including the target.
        let first = self.keyframes.partition_point(|key| key.tick <= start.tick);
        let mut upcoming = self.keyframes[first..]
            .iter()
            .take_while(|key| key.tick <= target)
            .peekable();
        let mut tick = start.tick;
        while tick < target {
            let row = self.steps.get(tick as usize).map_or(&[][..], Vec::as_slice);
            sim.step(row);
            tick += 1;
            while let Some(key) = upcoming.next_if(|key| key.tick == tick) {
                check(&*sim, key)?;
            }
        }
        Ok(SeekReport {
            keyframe_tick: start.tick,
            steps_replayed: target - start.tick,
            tick: target,
            state_hash: sim.state_hash(),
            keyframes_checked: checked,
        })
    }
}

/// Builds a [`DemoRecording`] while a simulation runs.
#[derive(Debug)]
pub struct DemoRecorder {
    header: DemoHeader,
    actions: Vec<String>,
    steps: Vec<Vec<Vec3>>,
    keyframes: Vec<DemoKeyframe>,
    /// Empty until the first [`DemoRecorder::record_step_stamped`]; then one row per step.
    timestamps: Vec<Vec<u64>>,
}

impl DemoRecorder {
    /// Starts a demo of `sim` tracking `actions`; captures the keyframe at tick 0.
    #[must_use]
    pub fn begin(header: DemoHeader, actions: Vec<String>, sim: &dyn DemoSim) -> Self {
        let mut recorder = Self {
            header,
            actions,
            steps: Vec::new(),
            keyframes: Vec::new(),
            timestamps: Vec::new(),
        };
        recorder.capture(sim);
        recorder
    }

    /// Steps recorded so far.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.steps.len() as u64
    }

    /// Whether no step has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Runs one step of `sim` on `inputs` and records it; adds a keyframe when the step count
    /// reaches a multiple of the header's `keyframe_interval`.
    ///
    /// # Errors
    /// [`DemoError::InputWidth`] when `inputs` is not one value per tracked action (nothing is
    /// recorded and `sim` does not step).
    pub fn record_step(&mut self, sim: &mut dyn DemoSim, inputs: &[Vec3]) -> Result<(), DemoError> {
        if inputs.len() != self.actions.len() {
            return Err(DemoError::InputWidth {
                got: inputs.len(),
                expected: self.actions.len(),
            });
        }
        sim.step(inputs);
        self.steps.push(inputs.to_vec());
        if !self.timestamps.is_empty() {
            self.timestamps.push(Vec::new());
        }
        let interval = u64::from(self.header.keyframe_interval);
        if interval != 0 && self.len().is_multiple_of(interval) {
            self.capture(sim);
        }
        Ok(())
    }

    /// [`DemoRecorder::record_step`] that also keeps the input timestamps the step consumed
    /// (microseconds since recording started). Steps recorded before the first stamped one get
    /// empty lists.
    ///
    /// # Errors
    /// As [`DemoRecorder::record_step`].
    pub fn record_step_stamped(
        &mut self,
        sim: &mut dyn DemoSim,
        inputs: &[Vec3],
        stamps: &[u64],
    ) -> Result<(), DemoError> {
        let before = self.steps.len();
        self.record_step(sim, inputs)?;
        self.timestamps.resize(before, Vec::new());
        self.timestamps.push(stamps.to_vec());
        Ok(())
    }

    /// Ends the demo: adds the final keyframe (unless the last step already made one) and fills
    /// in the step count.
    #[must_use]
    pub fn finish(mut self, sim: &dyn DemoSim) -> DemoRecording {
        if self.keyframes.last().is_none_or(|key| key.tick != self.len()) {
            self.capture(sim);
        }
        self.header.step_count = self.len();
        DemoRecording {
            header: self.header,
            actions: self.actions,
            steps: self.steps,
            keyframes: self.keyframes,
            timestamps: self.timestamps,
        }
    }

    fn capture(&mut self, sim: &dyn DemoSim) {
        self.keyframes.push(DemoKeyframe {
            tick: self.steps.len() as u64,
            state_hash: sim.state_hash(),
            snapshot_hex: encode_hex(&sim.snapshot()),
        });
    }
}

pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `None` for odd length or a non-hex digit.
pub(crate) fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let (pairs, _) = bytes.as_chunks::<2>();
    pairs
        .iter()
        .map(|pair| Some((digit(pair[0])? << 4) | digit(pair[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use regia_ecs::reflect::AppTypeRegistry;
    use regia_scene::components::Name;

    use super::*;

    /// A tiny deterministic simulation: a position integrating the first input, plus an LCG that
    /// folds in the second, so state depends on the whole input history.
    #[derive(Debug, Clone, Default)]
    struct Toy {
        x: i64,
        rng: u64,
        steps: u64,
        fail_restore: bool,
        /// Added to `x` every step: a "different build" for divergence tests.
        drift: i64,
    }

    impl Toy {
        fn seeded(seed: u64) -> Self {
            Self {
                rng: seed,
                ..Self::default()
            }
        }
    }

    impl DemoSim for Toy {
        fn step(&mut self, inputs: &[Vec3]) {
            let dx = inputs.first().map_or(0.0, |v| v.x);
            self.x += (dx * 100.0) as i64 + self.drift;
            let fold = inputs.get(1).map_or(0.0, |v| v.x);
            self.rng = self
                .rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407 ^ (fold.to_bits() as u64));
            self.steps += 1;
        }
        fn state_hash(&self) -> u64 {
            crate::state_hash::fnv1a(format!("{}/{}/{}", self.x, self.rng, self.steps).as_bytes())
        }
        fn snapshot(&self) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&self.x.to_le_bytes());
            out.extend_from_slice(&self.rng.to_le_bytes());
            out.extend_from_slice(&self.steps.to_le_bytes());
            out
        }
        fn restore(&mut self, snapshot: &[u8]) -> Result<(), String> {
            if self.fail_restore {
                return Err("refused".to_string());
            }
            let (chunks, rest) = snapshot.as_chunks::<8>();
            let [x, rng, steps] = chunks else {
                return Err(format!("expected 24 bytes, got {}", snapshot.len()));
            };
            if !rest.is_empty() {
                return Err("trailing bytes".to_string());
            }
            self.x = i64::from_le_bytes(*x);
            self.rng = u64::from_le_bytes(*rng);
            self.steps = u64::from_le_bytes(*steps);
            Ok(())
        }
    }

    fn actions() -> Vec<String> {
        vec!["Move".to_string(), "Fire".to_string()]
    }

    /// Deterministic input for step `k`.
    fn input(k: u64) -> [Vec3; 2] {
        [
            Vec3::new(((k * 7) % 11) as f32 * 0.25 - 1.0, 0.0, 0.0),
            Vec3::new((k % 3) as f32, 0.0, 0.0),
        ]
    }

    fn record(steps: u64, interval: u32) -> (DemoRecording, Toy) {
        let mut sim = Toy::seeded(99);
        let header = DemoHeader::new("arena", 99, 60, interval)
            .with_build("abc123")
            .with_content("c0ffee");
        let mut rec = DemoRecorder::begin(header, actions(), &sim);
        for k in 0..steps {
            rec.record_step(&mut sim, &input(k)).expect("record");
        }
        let demo = rec.finish(&sim);
        (demo, sim)
    }

    #[test]
    fn recorder_makes_keyframes_at_the_interval_and_the_end() {
        let (demo, sim) = record(25, 10);
        let ticks: Vec<u64> = demo.keyframes.iter().map(|k| k.tick).collect();
        assert_eq!(ticks, vec![0, 10, 20, 25]);
        assert_eq!(demo.header.step_count, 25);
        assert_eq!(demo.keyframes[3].state_hash, sim.state_hash());
        assert!(demo.keyframes.iter().all(DemoKeyframe::has_snapshot));
        assert_eq!(demo.validate(), Ok(()));
    }

    #[test]
    fn no_duplicate_final_keyframe_when_the_last_step_made_one() {
        let (demo, _) = record(20, 10);
        let ticks: Vec<u64> = demo.keyframes.iter().map(|k| k.tick).collect();
        assert_eq!(ticks, vec![0, 10, 20]);
    }

    #[test]
    fn interval_zero_keeps_only_the_start_and_end_keyframes() {
        let (demo, _) = record(7, 0);
        let ticks: Vec<u64> = demo.keyframes.iter().map(|k| k.tick).collect();
        assert_eq!(ticks, vec![0, 7]);
    }

    #[test]
    fn an_empty_demo_has_one_keyframe_and_verifies() {
        let (demo, _) = record(0, 10);
        assert!(demo.is_empty());
        assert_eq!(demo.keyframes.len(), 1);
        let report = demo.verify(&mut Toy::default()).expect("verify");
        assert_eq!(report.steps_replayed, 0);
    }

    #[test]
    fn wrong_input_width_is_rejected_and_does_not_step() {
        let mut sim = Toy::seeded(1);
        let mut rec = DemoRecorder::begin(DemoHeader::new("l", 1, 60, 4), actions(), &sim);
        let err = rec.record_step(&mut sim, &[Vec3::X]).expect_err("width");
        assert_eq!(err, DemoError::InputWidth { got: 1, expected: 2 });
        assert_eq!(err.code(), "input_width");
        assert_eq!(sim.steps, 0);
        assert!(rec.is_empty());
    }

    #[test]
    fn recording_twice_gives_identical_demos() {
        let (a, _) = record(40, 8);
        let (b, _) = record(40, 8);
        assert_eq!(a, b, "same inputs and seed: byte-identical demo");
        assert_eq!(a.to_ron(), b.to_ron());
    }

    #[test]
    fn seek_is_exact_for_every_tick() {
        let (demo, _) = record(37, 8);
        // Straight-run reference hashes, one per tick.
        let mut straight = Toy::seeded(99);
        let mut hashes = vec![straight.state_hash()];
        for k in 0..37 {
            straight.step(&input(k));
            hashes.push(straight.state_hash());
        }
        for tick in 0..=37u64 {
            // A fresh simulation with unrelated state: restore must overwrite it all.
            let mut sim = Toy::seeded(12345);
            let report = demo.seek(&mut sim, tick).expect("seek");
            assert_eq!(report.tick, tick);
            assert_eq!(report.state_hash, hashes[tick as usize], "tick {tick}");
            assert_eq!(sim.state_hash(), hashes[tick as usize], "tick {tick}");
            assert_eq!(sim.steps, tick);
        }
    }

    #[test]
    fn seek_starts_from_the_nearest_keyframe() {
        let (demo, _) = record(37, 8);
        let mut sim = Toy::default();
        let report = demo.seek(&mut sim, 19).expect("seek");
        assert_eq!(report.keyframe_tick, 16);
        assert_eq!(report.steps_replayed, 3);
        // Landing exactly on a keyframe replays nothing.
        let report = demo.seek(&mut sim, 24).expect("seek");
        assert_eq!((report.keyframe_tick, report.steps_replayed), (24, 0));
        // The tail after the last interval keyframe starts from it; the end keyframe is exact.
        let report = demo.seek(&mut sim, 37).expect("seek");
        assert_eq!((report.keyframe_tick, report.steps_replayed), (37, 0));
        let report = demo.seek(&mut sim, 36).expect("seek");
        assert_eq!((report.keyframe_tick, report.steps_replayed), (32, 4));
    }

    #[test]
    fn seek_checks_the_keyframes_it_passes() {
        let (mut demo, _) = record(37, 8);
        // Only the keyframe at 0 can start a seek; 8, 16 and 24 are hash-only and get crossed.
        for key in &mut demo.keyframes[1..4] {
            key.snapshot_hex.clear();
        }
        let mut sim = Toy::default();
        // Start 0, then keyframes 8, 16, 24 are crossed: 4 hashes compared (start + 3).
        let report = demo.seek(&mut sim, 25).expect("seek");
        assert_eq!(report.keyframes_checked, 4);
    }

    #[test]
    fn seeking_backwards_works() {
        let (demo, _) = record(30, 8);
        let mut sim = Toy::default();
        let late = demo.seek(&mut sim, 28).expect("seek").state_hash;
        let early = demo.seek(&mut sim, 3).expect("seek").state_hash;
        assert_ne!(late, early);
        assert_eq!(demo.seek(&mut sim, 28).expect("seek").state_hash, late);
    }

    #[test]
    fn seek_past_the_end_is_an_error() {
        let (demo, _) = record(10, 4);
        let err = demo.seek(&mut Toy::default(), 11).expect_err("range");
        assert_eq!(err, DemoError::TickOutOfRange { tick: 11, steps: 10 });
        assert_eq!(err.code(), "tick_out_of_range");
    }

    #[test]
    fn verify_refuses_a_first_snapshot_past_the_steps_instead_of_underflowing() {
        // Steps trimmed after recording: the first snapshot keyframe (8) is past the end (5).
        let (mut demo, _) = record(10, 4);
        for key in &mut demo.keyframes[..2] {
            key.snapshot_hex.clear();
        }
        demo.steps.truncate(5);
        assert_eq!(demo.len(), 5);
        let err = demo
            .verify(&mut Toy::default())
            .expect_err("keyframe past the end");
        assert_eq!(err, DemoError::TickOutOfRange { tick: 8, steps: 5 });
        assert_eq!(err.code(), "tick_out_of_range");
    }

    #[test]
    fn seek_without_a_snapshot_keyframe_is_an_error() {
        let (mut demo, _) = record(10, 4);
        for key in &mut demo.keyframes {
            key.snapshot_hex.clear();
        }
        assert!(demo.nearest_keyframe(5).is_none());
        let err = demo.seek(&mut Toy::default(), 5).expect_err("no keyframe");
        assert_eq!(err.code(), "no_keyframe");
        assert_eq!(
            demo.verify(&mut Toy::default()).expect_err("none").code(),
            "no_keyframe"
        );
    }

    #[test]
    fn hash_only_keyframes_are_skipped_as_seek_starts_but_still_verify() {
        let (mut demo, _) = record(20, 5);
        // Drop the snapshot of the keyframe at tick 10: a seek to 12 falls back to tick 5.
        demo.keyframes[2].snapshot_hex.clear();
        assert_eq!(demo.nearest_keyframe(12).map(|k| k.tick), Some(5));
        let report = demo.seek(&mut Toy::default(), 12).expect("seek");
        assert_eq!(report.keyframe_tick, 5);
        // The hash-only keyframe at 10 is still compared on the way.
        assert_eq!(report.keyframes_checked, 2);
    }

    #[test]
    fn a_failing_restore_is_reported_with_its_reason() {
        let (demo, _) = record(10, 4);
        let mut sim = Toy {
            fail_restore: true,
            ..Toy::default()
        };
        let err = demo.seek(&mut sim, 6).expect_err("restore");
        assert_eq!(
            err,
            DemoError::Restore {
                tick: 4,
                reason: "refused".to_string()
            }
        );
        assert_eq!(err.code(), "restore_failed");
    }

    #[test]
    fn a_tampered_keyframe_hash_is_caught_on_restore() {
        let (mut demo, _) = record(20, 5);
        demo.keyframes[1].state_hash ^= 1;
        let err = demo.seek(&mut Toy::default(), 7).expect_err("diverged");
        match err {
            DemoError::Diverged {
                tick,
                expected,
                actual,
            } => {
                assert_eq!(tick, 5);
                assert_eq!(expected ^ actual, 1);
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn a_different_build_diverges_at_the_first_keyframe_after_the_difference() {
        let (demo, _) = record(30, 5);
        // A "build" whose step drifts: replaying from the start snapshot diverges at tick 5.
        let mut sim = Toy {
            drift: 1,
            ..Toy::default()
        };
        let err = demo.verify(&mut sim).expect_err("diverge");
        assert!(matches!(err, DemoError::Diverged { tick: 5, .. }), "{err:?}");
        assert_eq!(err.code(), "diverged");
        // The same build verifies.
        let report = demo.verify(&mut Toy::default()).expect("verify");
        assert_eq!(report.tick, 30);
        assert_eq!(report.keyframes_checked, 7);
        assert_eq!(
            report.state_hash,
            demo.keyframes.last().map(|k| k.state_hash).unwrap_or(0)
        );
    }

    #[test]
    fn ron_round_trip_is_lossless() {
        let (demo, _) = record(33, 8);
        let text = demo.to_ron().expect("write");
        assert!(text.trim_end().ends_with(DEMO_END_MARKER));
        let back = DemoRecording::from_ron(&text).expect("read");
        assert_eq!(back, demo, "inputs, keyframes and header survive exactly");
        // And a seek on the reloaded demo still lands on the recorded final hash.
        let mut sim = Toy::default();
        let report = back.seek(&mut sim, 33).expect("seek");
        assert_eq!(
            Some(report.state_hash),
            back.keyframes.last().map(|k| k.state_hash)
        );
    }

    #[test]
    fn save_and_load_round_trip_through_a_file() {
        let (demo, _) = record(12, 4);
        let dir = regia_test_utils::unique_temp_dir("demo-save");
        let path = dir.join("match.regiademo");
        demo.save(&path).expect("save");
        assert_eq!(DemoRecording::load(&path).expect("load"), demo);
        // No temp file is left next to it.
        let leftovers = std::fs::read_dir(&dir).expect("dir").count();
        assert_eq!(leftovers, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_truncation_of_a_file_is_an_error_and_never_a_panic() {
        let (demo, _) = record(9, 3);
        let text = demo.to_ron().expect("write");
        for cut in (0..text.trim_end().len()).filter(|c| text.is_char_boundary(*c)) {
            let err = DemoRecording::from_ron(&text[..cut]).expect_err("truncated");
            assert_eq!(err.code(), "truncated", "cut at {cut}");
        }
        // Dropping only the trailing newline is still a complete file.
        assert!(DemoRecording::from_ron(text.trim_end()).is_ok());
    }

    #[test]
    fn a_truncated_file_on_disk_reports_truncated() {
        let (demo, _) = record(9, 3);
        let text = demo.to_ron().expect("write");
        let dir = regia_test_utils::unique_temp_dir("demo-trunc");
        let path = dir.join("cut.regiademo");
        std::fs::write(&path, &text[..text.len() / 2]).expect("write");
        let err = DemoRecording::load(&path).expect_err("truncated");
        assert!(matches!(err, DemoError::Truncated { .. }), "{err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// audit-agent-sec (High): `metadata().len()` of `/dev/zero` is 0, so the size cap passed and
    /// `fs::read` grew without end; a directory or device is refused as not a regular file.
    #[test]
    fn only_regular_files_are_read() {
        let dir = regia_test_utils::unique_temp_dir("demo-not-regular");
        let err = DemoRecording::load(&dir).expect_err("directory");
        assert_eq!(err, DemoError::Io("not a regular file".into()));
        #[cfg(unix)]
        {
            let err = DemoRecording::load(Path::new("/dev/zero")).expect_err("device");
            assert_eq!(err, DemoError::Io("not a regular file".into()));
            // A symlink to a regular demo file still loads (the target is what is checked).
            let (demo, _) = record(4, 2);
            let real = dir.join("real.regiademo");
            demo.save(&real).expect("save");
            std::os::unix::fs::symlink(&real, dir.join("link.regiademo")).expect("symlink");
            assert_eq!(
                DemoRecording::load(&dir.join("link.regiademo")).expect("through a link"),
                demo
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_an_io_error() {
        let dir = regia_test_utils::unique_temp_dir("demo-missing");
        let err = DemoRecording::load(&dir.join("nope.regiademo")).expect_err("io");
        assert_eq!(err.code(), "io");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_content_is_a_parse_error_not_truncation() {
        let text = format!("this is not a demo\n{DEMO_END_MARKER}\n");
        assert_eq!(DemoRecording::from_ron(&text).expect_err("parse").code(), "parse");
    }

    #[test]
    fn a_future_format_version_is_named_not_misparsed() {
        let (demo, _) = record(4, 2);
        let text = demo
            .to_ron()
            .expect("write")
            .replacen("format_version:2", "format_version:3", 1);
        assert!(text.contains("format_version:3"), "{text}");
        assert_eq!(
            DemoRecording::from_ron(&text).expect_err("version"),
            DemoError::UnsupportedVersion(3)
        );
    }

    #[test]
    fn a_step_count_that_disagrees_with_the_rows_is_invalid() {
        let (demo, _) = record(6, 3);
        let text = demo
            .to_ron()
            .expect("write")
            .replacen("step_count:6", "step_count:7", 1);
        assert!(text.contains("step_count:7"), "{text}");
        assert_eq!(
            DemoRecording::from_ron(&text).expect_err("count").code(),
            "invalid"
        );
    }

    #[test]
    fn validate_catches_each_inconsistency() {
        let (good, _) = record(6, 3);
        let mut zero_rate = good.clone();
        zero_rate.header.tick_rate_hz = 0;
        let mut wide = good.clone();
        wide.steps[2].push(Vec3::ZERO);
        let mut nan = good.clone();
        nan.steps[1][0] = Vec3::new(f32::NAN, 0.0, 0.0);
        let mut late = good.clone();
        late.keyframes.last_mut().expect("keyframe").tick = 99;
        let mut unordered = good.clone();
        unordered.keyframes.swap(0, 1);
        let mut bad_hex = good.clone();
        bad_hex.keyframes[0].snapshot_hex = "xyz".to_string();
        let mut odd_hex = good.clone();
        odd_hex.keyframes[0].snapshot_hex = "abc".to_string();
        for (name, demo) in [
            ("zero rate", zero_rate),
            ("wide row", wide),
            ("nan input", nan),
            ("keyframe past end", late),
            ("unordered keyframes", unordered),
            ("bad hex", bad_hex),
            ("odd hex", odd_hex),
        ] {
            assert_eq!(demo.validate().expect_err(name).code(), "invalid", "{name}");
            assert!(demo.to_ron().is_err(), "{name}: an invalid demo is never written");
        }
        let mut old = good;
        old.header.format_version = 0;
        assert_eq!(old.validate(), Err(DemoError::UnsupportedVersion(0)));
    }

    #[test]
    fn header_mismatches_name_the_differing_identity_fields() {
        let base = DemoHeader::new("arena", 1, 60, 8)
            .with_build("a")
            .with_content("x");
        assert!(base.mismatches(&base.clone()).is_empty());
        let mut other = base.clone();
        other.engine_version = "9.9.9".to_string();
        other.build_hash = "b".to_string();
        other.content_hash = "y".to_string();
        other.level = "dock".to_string();
        other.seed = 2;
        other.tick_rate_hz = 30;
        assert_eq!(
            base.mismatches(&other),
            vec![
                "engine_version",
                "build_hash",
                "content_hash",
                "level",
                "seed",
                "tick_rate_hz"
            ]
        );
        // An unknown (empty) hash on either side is not a mismatch.
        let unknown = DemoHeader::new("arena", 1, 60, 8);
        assert!(base.mismatches(&unknown).is_empty());
        assert!(unknown.mismatches(&base).is_empty());
    }

    #[test]
    fn engine_version_defaults_to_this_build() {
        assert_eq!(
            DemoHeader::new("l", 0, 60, 0).engine_version,
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn input_recording_converts_both_ways() {
        let input = InputRecording {
            seed: 77,
            actions: actions(),
            frames: vec![vec![Vec3::X, Vec3::Y], vec![Vec3::Z]],
            timestamps: vec![vec![3], vec![]],
        };
        let demo = DemoRecording::from_input_recording(DemoHeader::new("l", 0, 60, 4), &input);
        assert_eq!(demo.header.seed, 77, "the seed comes from the recording");
        assert_eq!(demo.header.step_count, 2);
        assert_eq!(demo.steps[1], vec![Vec3::Z, Vec3::ZERO], "short rows are padded");
        assert_eq!(demo.validate(), Ok(()));
        let back = demo.to_input_recording();
        assert_eq!(back.seed, 77);
        assert_eq!(back.actions, input.actions);
        assert_eq!(back.frames[0], input.frames[0]);
        assert_eq!(
            back.timestamps, input.timestamps,
            "timestamps survive (format v2)"
        );
        assert_eq!(demo.timestamps_at(0), &[3]);
        assert!(demo.timestamps_at(1).is_empty());
        assert!(demo.timestamps_at(99).is_empty());
    }

    #[test]
    fn short_timestamp_lists_are_padded_to_the_step_count() {
        let input = InputRecording {
            seed: 1,
            actions: vec!["Move".to_string()],
            frames: vec![vec![Vec3::X], vec![Vec3::Y], vec![Vec3::Z]],
            timestamps: vec![vec![10, 20]],
        };
        let demo = DemoRecording::from_input_recording(DemoHeader::new("l", 0, 60, 0), &input);
        assert_eq!(demo.timestamps, vec![vec![10, 20], vec![], vec![]]);
        assert_eq!(demo.validate(), Ok(()));
        let none = InputRecording {
            timestamps: Vec::new(),
            ..input
        };
        let demo = DemoRecording::from_input_recording(DemoHeader::new("l", 0, 60, 0), &none);
        assert!(demo.timestamps.is_empty(), "no timestamps stay none");
    }

    #[test]
    fn stamped_steps_round_trip_through_ron() {
        let mut sim = Toy::seeded(4);
        let mut rec = DemoRecorder::begin(DemoHeader::new("l", 4, 60, 3), actions(), &sim);
        rec.record_step(&mut sim, &input(0)).expect("plain step");
        for k in 1..6u64 {
            rec.record_step_stamped(&mut sim, &input(k), &[k * 1000, k * 1000 + 7])
                .expect("stamped");
        }
        let demo = rec.finish(&sim);
        assert_eq!(demo.timestamps.len(), 6, "one row per step");
        assert!(demo.timestamps[0].is_empty(), "the unstamped first step");
        assert_eq!(demo.timestamps[5], vec![5000, 5007]);
        let back = DemoRecording::from_ron(&demo.to_ron().expect("write")).expect("read");
        assert_eq!(back, demo);
        // Timestamps never change a replay.
        let report = back.verify(&mut Toy::default()).expect("verify");
        assert_eq!(report.tick, 6);
    }

    #[test]
    fn mismatched_timestamp_rows_are_invalid() {
        let (mut demo, _) = record(4, 2);
        demo.timestamps = vec![vec![1]; 3];
        assert_eq!(demo.validate().expect_err("rows").code(), "invalid");
    }

    /// The v1 text of a demo (no `timestamps` field): what ADR-1569 builds wrote.
    fn v1_text(demo: &DemoRecording) -> String {
        let text = demo.to_ron().expect("write");
        let text = text.replacen("format_version:2", "format_version:1", 1);
        let cut = text.find(",timestamps:").expect("timestamps field");
        let end = text[cut..].find(')').map(|i| cut + i).expect("close");
        // `timestamps:[]` (empty) is the last field before the closing parenthesis.
        format!("{}{}", &text[..cut], &text[end..])
    }

    #[test]
    fn a_version_1_file_migrates_to_the_current_version() {
        let (demo, _) = record(9, 3);
        assert!(demo.timestamps.is_empty());
        let old = v1_text(&demo);
        assert!(
            old.contains("format_version:1") && !old.contains("timestamps"),
            "{old}"
        );
        assert_eq!(demo_file_version(&old), Ok(Some(1)));
        let back = DemoRecording::from_ron(&old).expect("v1 loads");
        assert_eq!(back.header.format_version, DEMO_FORMAT_VERSION);
        assert_eq!(back, demo, "a v1 file loads as the same recording");
        // Upgrading is load + save: the result is the current version.
        let upgraded = back.to_ron().expect("write");
        assert_eq!(demo_file_version(&upgraded), Ok(Some(DEMO_FORMAT_VERSION)));
        assert_eq!(demo_migration_sources(), vec![1]);
    }

    #[test]
    fn a_version_1_file_with_timestamps_is_invalid() {
        let (mut demo, _) = record(2, 0);
        demo.timestamps = vec![vec![1], vec![2]];
        let text = demo
            .to_ron()
            .expect("write")
            .replacen("format_version:2", "format_version:1", 1);
        assert_eq!(DemoRecording::from_ron(&text).expect_err("v1").code(), "invalid");
    }

    #[test]
    fn demo_file_version_ignores_other_files() {
        assert_eq!(demo_file_version("(not:a demo)"), Ok(None));
        assert!(demo_file_version(&format!("junk\n{DEMO_END_MARKER}")).is_err());
        assert!(!demo_schema_fingerprint().is_empty());
    }

    #[test]
    fn duration_follows_the_tick_rate() {
        let (demo, _) = record(120, 30);
        assert!((demo.duration_secs() - 2.0).abs() < 1e-12);
    }

    #[test]
    fn hex_round_trips_and_rejects_bad_text() {
        let bytes = [0u8, 1, 0x7f, 0x80, 0xff];
        assert_eq!(decode_hex(&encode_hex(&bytes)), Some(bytes.to_vec()));
        assert_eq!(decode_hex(""), Some(Vec::new()));
        assert_eq!(decode_hex("0"), None);
        assert_eq!(decode_hex("0g"), None);
        assert_eq!(decode_hex("0F"), None, "uppercase is not what the writer emits");
    }

    #[derive(Component, Reflect, Default)]
    #[reflect(Component)]
    struct Hp(u32);

    #[test]
    fn world_sim_state_hash_ignores_what_keep_leaves_out() {
        let mut world = World::new();
        world.init_resource::<AppTypeRegistry>();
        world.resource::<AppTypeRegistry>().write().register::<Name>();
        world
            .resource::<AppTypeRegistry>()
            .write()
            .register::<regia_scene::Transform>();
        let camera = world
            .spawn((
                Name::new("camera"),
                regia_scene::Transform::from_translation(Vec3::ZERO),
            ))
            .id();
        let keep = |name: &str| name != "Transform";
        let one = world_sim_state_hash(&world, keep);
        let all = world_state_hash(&world);
        world
            .entity_mut(camera)
            .insert(regia_scene::Transform::from_translation(Vec3::X));
        assert_eq!(one, world_sim_state_hash(&world, keep), "a presentation write");
        assert_ne!(all, world_state_hash(&world), "the unfiltered hash saw it");
    }

    #[test]
    fn world_state_hash_tracks_world_changes() {
        let mut world = World::new();
        world.init_resource::<AppTypeRegistry>();
        world.resource::<AppTypeRegistry>().write().register::<Hp>();
        let entity = world.spawn((Name::new("a"), Hp(10))).id();
        let one = world_state_hash(&world);
        assert_eq!(one, world_state_hash(&world), "stable for an unchanged world");
        if let Some(mut hp) = world.get_mut::<Hp>(entity) {
            hp.0 = 9;
        }
        assert_ne!(one, world_state_hash(&world));
    }
}
