// SPDX-License-Identifier: MPL-2.0
//! The render-job file (`.render_job.ron`): a whole sequence-driven render-out request (takes,
//! cuts, output settings, the shot list and sequence it follows), versioned and validated
//! (`docs/formats/render-job-ron.md`, ADR-1616).
//!
//! `render.out_job` and `regia render --job` read it. The engine resolves the takes against the
//! named `.shots.ron` and `.seq.ron` and renders them in one pass from sequence time 0
//! (book `agents/render-jobs.md`). This module is only the data and its checks; nothing here
//! touches a world, a file or a clock.
//!
//! ```
//! use regia_agent::render_job::{RenderJob, TakeRange};
//!
//! let job = RenderJob::from_ron(r#"(
//!     version: 1,
//!     name: "trailer",
//!     shots: Some("cinematics/trailer.shots.ron"),
//!     out: "renders/trailer",
//!     output: (width: Some(1920), height: Some(1080), fps: Some(24)),
//!     takes: [
//!         (name: "opening", range: Shots(first: "wide", last: "close")),
//!         (name: "tail", range: Seconds(start: 12.0, end: 15.5)),
//!     ],
//! )"#).expect("valid job");
//! assert_eq!(job.takes.len(), 2);
//! assert!(matches!(job.takes[1].range, TakeRange::Seconds { .. }));
//! ```

use regia_core::version::{self, VersionStep};
use serde::{Deserialize, Serialize};

/// Current `.render_job.ron` version (v2: the `bindings` table, ADR-1880; v3: `output.encode`, the
/// encoder settings of format `webm`, ADR-3369; v4: the `product` block, ADR-2757).
pub const RENDER_JOB_FORMAT_VERSION: u32 = 4;

/// Most entries of a job's `bindings` table.
pub const MAX_JOB_BINDINGS: usize = 1024;

/// Most takes one job may list.
pub const MAX_TAKES: usize = 256;

/// Most explicit cuts one job may list (the render-out `cuts` limit).
pub const MAX_JOB_CUTS: usize = 4096;

/// Most timed picks or view keys a job's `product` block may list.
pub const MAX_PRODUCT_EVENTS: usize = 4096;

/// The default output path template: a folder and a file stem named after the take.
pub const DEFAULT_PATH: &str = "{take}/{take}";

/// Render-job errors.
#[derive(Debug, thiserror::Error)]
pub enum RenderJobError {
    /// The file is a newer version than this build reads.
    #[error("render job version {found} is newer than supported version {expected}; upgrade the engine")]
    Version {
        /// Version in the file.
        found: u32,
        /// Version this build reads.
        expected: u32,
    },
    /// RON syntax or shape error.
    #[error("render job parse error: {0}")]
    Parse(String),
    /// Semantic error (no takes, a duplicate name, an empty range).
    #[error("invalid render job: {0}")]
    Invalid(String),
    /// A migration step failed.
    #[error("render job migration failed: {0}")]
    Migration(String),
}

impl RenderJobError {
    /// The agent-protocol error code of this error (`book/agents/render-jobs.md`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Version { .. } => "render_job_version",
            Self::Parse(_) | Self::Migration(_) => "render_job_parse",
            Self::Invalid(_) => "render_job_invalid",
        }
    }
}

/// The audio settings of a job (`render.out_start` `audio`, ADR-1520). Every `None` is the
/// render-out default.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioSettings {
    /// Mixdown sample rate, Hz.
    #[serde(default)]
    pub sample_rate: Option<u32>,
    /// `"pcm16"`, `"pcm24"` or `"float32"`.
    #[serde(default)]
    pub format: Option<String>,
    /// Buses recorded to their own WAV.
    #[serde(default)]
    pub stems: Vec<String>,
    /// `"render"`, `"output"` or `"sim"`.
    #[serde(default)]
    pub timeline: Option<String>,
}

/// One key of a job's time map (`render.out_start` `time_map`, ADR-1519). `frame` is a frame of
/// the job's timeline (global, from sequence time 0).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimeMapKey {
    /// Timeline frame the scale is reached at.
    pub frame: u64,
    /// Simulation time per unit of video time (1 = real time).
    pub scale: f64,
    /// `"smooth"` (default), `"linear"` or `"hold"`.
    #[serde(default)]
    pub ease: Option<String>,
}

/// The output settings of a job: every `render.out_start` setting except `out`, `name`,
/// `frames` and `cuts`, which the job derives. Every `None` (or empty list) is the render-out
/// default, and the strings are the protocol's spellings, checked by the same parser.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OutputSettings {
    /// Delivered width, pixels (default 1920).
    #[serde(default)]
    pub width: Option<u32>,
    /// Delivered height, pixels (default 1080).
    #[serde(default)]
    pub height: Option<u32>,
    /// Frame rate numerator (default 30).
    #[serde(default)]
    pub fps: Option<u32>,
    /// Frame rate denominator (default 1; 1001 for 29.97).
    #[serde(default)]
    pub fps_den: Option<u32>,
    /// Supersampling factor (default 1).
    #[serde(default)]
    pub supersample: Option<u32>,
    /// `"png"` (default), `"raw"`, `"exr"`, `"webm"`, `"ffv1"`, `"prores"` or `"dnxhr"`.
    #[serde(default)]
    pub format: Option<String>,
    /// Draw the game UI (default true).
    #[serde(default)]
    pub hud: Option<bool>,
    /// `"auto"`, `"landscape"` or `"portrait"`.
    #[serde(default)]
    pub layout: Option<String>,
    /// `"neutral"` (default) or `"applied"`.
    #[serde(default)]
    pub calibration: Option<String>,
    /// Frames stepped before timeline frame 0.
    #[serde(default)]
    pub warmup_frames: Option<u64>,
    /// The last warm-up frames rendered at the delivered size (ADR-1449).
    #[serde(default)]
    pub settle_frames: Option<u64>,
    /// Shutter subframes per frame (ADR-1404).
    #[serde(default)]
    pub subframes: Option<u32>,
    /// Shutter angle, degrees.
    #[serde(default)]
    pub shutter_angle: Option<f64>,
    /// 8 (default) or 16.
    #[serde(default)]
    pub bit_depth: Option<u32>,
    /// `"fast"` (default), `"balanced"` or `"best"`.
    #[serde(default)]
    pub png_compression: Option<String>,
    /// Frame writer threads (0 or absent: automatic).
    #[serde(default)]
    pub writers: Option<u32>,
    /// AOVs written next to each frame (`"depth"`, `"id"`, `"motion"`).
    #[serde(default)]
    pub aovs: Vec<String>,
    /// `"exr"` or `"raw"`.
    #[serde(default)]
    pub aov_format: Option<String>,
    /// The WAV mixdown of the timeline (absent: no audio).
    #[serde(default)]
    pub audio: Option<AudioSettings>,
    /// Simulation-time scale per timeline frame range (slow motion).
    #[serde(default)]
    pub time_map: Vec<TimeMapKey>,
    /// Encoder settings of format `"webm"` (v3, ADR-3369): quality or bitrate, speed, keyframe
    /// interval, audio bitrate, threads. Absent: the encoder defaults.
    #[serde(default)]
    pub encode: Option<regia_media::encode::EncodeSettings>,
}

/// The time range of a take on the job's timeline. Every range is half-open: it holds its start
/// and not its end.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TakeRange {
    /// The whole shot list (else the whole sequence).
    Whole,
    /// Timeline frames `start..end`.
    Frames {
        /// First frame.
        start: u64,
        /// Frame after the last.
        end: u64,
    },
    /// Sequence seconds `start..end`.
    Seconds {
        /// Start time, seconds.
        start: f64,
        /// End time, seconds.
        end: f64,
    },
    /// From marker `from` to marker `to` of the `.seq.ron`.
    Markers {
        /// The marker the take starts at.
        from: String,
        /// The marker the take ends at.
        to: String,
    },
    /// From the start of shot `first` to the end of shot `last` of the `.shots.ron`.
    Shots {
        /// The first shot.
        first: String,
        /// The last shot (may be `first`).
        last: String,
    },
}

/// One take: a range rendered into its own folder and file stem.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Take {
    /// Take name (`{take}`): 1..=64 letters, digits, `_`, `-` or `.`, not leading `.`; unique.
    pub name: String,
    /// Its range.
    pub range: TakeRange,
    /// Output path template of this take (default: the job's `path`).
    #[serde(default)]
    pub path: Option<String>,
}

/// A cut the job adds to the shot boundaries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CutAt {
    /// At this timeline frame.
    Frame(u64),
    /// At the first frame at or after this sequence time, seconds.
    Seconds(f64),
    /// At the first frame at or after this `.seq.ron` marker.
    Marker(String),
}

/// What a sequence target binds to in a job's `bindings` table (v2).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum JobBinding {
    /// The entity with this `Name` (its scene guid when it has one; the lowest entity id when
    /// several share the name).
    Name(String),
    /// The entity with this scene guid (the `EntityGuid` text form).
    Guid(String),
}

/// The value of a timed pick: a label, or the labels of a multi group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PickValue {
    /// One label.
    One(String),
    /// The labels of a multi group (an empty list clears it).
    Many(Vec<String>),
}

/// A timed `setPick` of a product job. `at` is a time on the job's timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OptionChange {
    /// When: the pick applies before this frame draws.
    pub at: CutAt,
    /// The option group.
    pub group: String,
    /// The value.
    pub value: PickValue,
    /// Find the sold build that keeps this pick and changes the fewest other groups, as `setPick`'s
    /// `adjust` does. Without it a pick the snapshot does not sell refuses the job.
    #[serde(default)]
    pub adjust: bool,
}

/// A timed `setView` of a product job: what it leaves out stays at the key before it. The numbers are the
/// player's own, and they must stay inside the recipe's `view_range`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProductViewKey {
    /// When the view reaches this key.
    pub at: CutAt,
    /// Tilt in degrees: x about the up axis, y about the right axis.
    #[serde(default)]
    pub tilt: Option<[f32; 2]>,
    /// Turn about the front axis, degrees.
    #[serde(default)]
    pub spin: Option<f32>,
    /// Offset of the look-at point in the front plane, mm.
    #[serde(default)]
    pub pan: Option<[f32; 2]>,
    /// Zoom factor; 1 is the default framing.
    #[serde(default)]
    pub zoom: Option<f32>,
    /// `"smooth"` (default), `"linear"` or `"hold"`: how the view moves into this key.
    #[serde(default)]
    pub ease: Option<String>,
}

/// A job's product (v4, ADR-2757): frames come from the product viewer's raster renderer, with the player's
/// looks, instead of the running game's view. No path tracing, no world.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProductCapture {
    /// The product package folder (`regia.viewer_package/1`), confined like `out`.
    pub package: String,
    /// A `jewelry.build/1` file of this product, confined like `out`: the picks and the view at frame 0
    /// (absent: the product's first sold picks and its default view).
    #[serde(default)]
    pub build: Option<String>,
    /// The option-change track: timed picks.
    #[serde(default)]
    pub options: Vec<OptionChange>,
    /// A camera path in the player's terms. The job's `shots` is the other way to give one, in mm.
    #[serde(default)]
    pub view: Vec<ProductViewKey>,
}

/// The `.render_job.ron` document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RenderJob {
    /// Format version ([`RENDER_JOB_FORMAT_VERSION`]).
    pub version: u32,
    /// Job name (`{job}`, the manifest and audio file stem; empty: `job`).
    #[serde(default)]
    pub name: String,
    /// The `.shots.ron` the job plays (project-relative).
    #[serde(default)]
    pub shots: Option<String>,
    /// The `.seq.ron` whose markers the job may name (project-relative). With `bindings` the job
    /// also plays it.
    #[serde(default)]
    pub sequence: Option<String>,
    /// The binding table (v2): each `sequence` target name and the entity it binds to. When it is
    /// not empty the job plays `sequence` itself: it creates an external-clock sequence player
    /// with these bindings for the run (its sub-sequences loaded from their project paths) and
    /// removes it at the end.
    #[serde(default)]
    pub bindings: std::collections::BTreeMap<String, JobBinding>,
    /// Output directory of the job (confined like `render.out_start` `out`).
    pub out: String,
    /// Output path template under `out` (default [`DEFAULT_PATH`]).
    #[serde(default = "default_path")]
    pub path: String,
    /// Size, rate, format and the other render-out settings.
    #[serde(default)]
    pub output: OutputSettings,
    /// The takes, at least one.
    pub takes: Vec<Take>,
    /// Cuts added to the shot boundaries.
    #[serde(default)]
    pub cuts: Vec<CutAt>,
    /// Refuse the job when more than this many timeline frames would be rendered and written to
    /// no take (the pre-roll cost of takes that start after 0).
    #[serde(default)]
    pub max_preroll_frames: Option<u64>,
    /// Render a product instead of the running game's view (v4, ADR-2757).
    #[serde(default)]
    pub product: Option<ProductCapture>,
}

fn default_path() -> String {
    DEFAULT_PATH.to_string()
}

/// A take or job name usable in a path: 1..=64 letters, digits, `_`, `-` or `.`, not leading `.`.
#[must_use]
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        // A take stem becomes a folder and a movie: `NUL` blocks on the device on Windows.
        && regia_core::project_path::check_relative(name).is_ok()
}

impl RenderJob {
    /// Parses a `.render_job.ron`: version check, migration, validation.
    pub fn from_ron(text: &str) -> Result<Self, RenderJobError> {
        let found = version::peek_ron_version(text).map_err(RenderJobError::Parse)?;
        if found > RENDER_JOB_FORMAT_VERSION {
            return Err(RenderJobError::Version {
                found,
                expected: RENDER_JOB_FORMAT_VERSION,
            });
        }
        let mut job: Self = ron::from_str(text).map_err(|e| RenderJobError::Parse(e.to_string()))?;
        job.migrate_to_latest()?;
        job.validate()?;
        Ok(job)
    }

    /// Serializes to RON with a trailing newline. Re-saving an unchanged job is byte-identical.
    pub fn to_ron(&self) -> Result<String, RenderJobError> {
        let mut text = ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| RenderJobError::Parse(e.to_string()))?;
        text.push('\n');
        Ok(text)
    }

    /// Rejects a newer version and runs the migration chain on an older one
    /// (`docs/specs/12-assets.md` § 4a).
    pub fn migrate_to_latest(&mut self) -> Result<u32, RenderJobError> {
        if self.version > RENDER_JOB_FORMAT_VERSION {
            return Err(RenderJobError::Version {
                found: self.version,
                expected: RENDER_JOB_FORMAT_VERSION,
            });
        }
        if self.version < RENDER_JOB_FORMAT_VERSION {
            self.version = version::migrate(
                self,
                self.version,
                render_job_migrations(),
                RENDER_JOB_FORMAT_VERSION,
            )
            .map_err(|e| RenderJobError::Migration(e.to_string()))?;
        }
        Ok(self.version)
    }

    /// The job name used for `{job}` and the manifest (`job` when the file leaves it empty).
    #[must_use]
    pub fn job_name(&self) -> &str {
        if self.name.is_empty() { "job" } else { &self.name }
    }

    /// Checks what needs no project: a valid job name, a non-empty `out`, 1..=[`MAX_TAKES`] takes
    /// with unique valid names, non-empty and finite ranges, named markers and shots, at most
    /// [`MAX_JOB_CUTS`] finite cuts. Markers and shots are resolved against their files later.
    pub fn validate(&self) -> Result<(), RenderJobError> {
        let invalid = |m: String| Err(RenderJobError::Invalid(m));
        if !is_valid_name(self.job_name()) {
            return invalid(format!(
                "job name `{}` must be 1..=64 letters, digits, `_`, `-` or `.` and not start with `.`",
                self.name
            ));
        }
        if self.out.is_empty() {
            return invalid("`out` (the job's output directory) is empty".into());
        }
        if self.takes.is_empty() {
            return invalid("a job needs at least one take".into());
        }
        if self.takes.len() > MAX_TAKES {
            return invalid(format!(
                "the job has {} takes; the limit is {MAX_TAKES}",
                self.takes.len()
            ));
        }
        if self.cuts.len() > MAX_JOB_CUTS {
            return invalid(format!(
                "the job has {} cuts; the limit is {MAX_JOB_CUTS}",
                self.cuts.len()
            ));
        }
        for (field, path) in [("shots", &self.shots), ("sequence", &self.sequence)] {
            if path.as_ref().is_some_and(String::is_empty) {
                return invalid(format!("`{field}` is an empty path"));
            }
        }
        if !self.bindings.is_empty() && self.sequence.is_none() {
            return invalid("`bindings` binds the targets of `sequence`, and the job names none".into());
        }
        if self.bindings.len() > MAX_JOB_BINDINGS {
            return invalid(format!(
                "the job has {} bindings; the limit is {MAX_JOB_BINDINGS}",
                self.bindings.len()
            ));
        }
        for (target, binding) in &self.bindings {
            if target.is_empty() {
                return invalid("a binding has an empty target name".into());
            }
            match binding {
                JobBinding::Name(name) if name.is_empty() => {
                    return invalid(format!("binding `{target}`: the entity name is empty"));
                }
                JobBinding::Guid(text) if text.parse::<regia_core::id::Guid>().is_err() => {
                    return invalid(format!("binding `{target}`: `{text}` is not a guid"));
                }
                _ => {}
            }
        }
        let mut names = std::collections::BTreeSet::new();
        for take in &self.takes {
            if !is_valid_name(&take.name) {
                return invalid(format!(
                    "take name `{}` must be 1..=64 letters, digits, `_`, `-` or `.` and not start with `.`",
                    take.name
                ));
            }
            if !names.insert(take.name.as_str()) {
                return invalid(format!("duplicate take name `{}`", take.name));
            }
            take.range
                .validate()
                .map_err(|m| RenderJobError::Invalid(format!("take `{}`: {m}", take.name)))?;
        }
        for cut in &self.cuts {
            match cut {
                CutAt::Seconds(t) if !(t.is_finite() && *t >= 0.0) => {
                    return invalid(format!("cut at {t} s must be a finite time of zero or more"));
                }
                CutAt::Marker(name) if name.is_empty() => {
                    return invalid("a cut names an empty marker".into());
                }
                _ => {}
            }
        }
        if let Some(product) = &self.product {
            self.validate_product(product)?;
        }
        Ok(())
    }

    /// The checks of a product job: its paths, its timed picks and view keys, and the settings a raster frame of
    /// the lean renderer has no meaning for.
    fn validate_product(&self, product: &ProductCapture) -> Result<(), RenderJobError> {
        let invalid = |m: String| Err(RenderJobError::Invalid(m));
        if product.package.is_empty() {
            return invalid("`product.package` is an empty path".into());
        }
        if product.build.as_ref().is_some_and(String::is_empty) {
            return invalid("`product.build` is an empty path".into());
        }
        if !self.bindings.is_empty() {
            return invalid(
                "`bindings` plays a sequence in the world, and a product job renders no world".into(),
            );
        }
        if self.shots.is_some() && !product.view.is_empty() {
            return invalid("give the camera path as `shots` or as `product.view`, not both".into());
        }
        for (name, count) in [("options", product.options.len()), ("view", product.view.len())] {
            if count > MAX_PRODUCT_EVENTS {
                return invalid(format!(
                    "`product.{name}` has {count} entries; the limit is {MAX_PRODUCT_EVENTS}"
                ));
            }
        }
        let bad_time = |at: &CutAt, what: &str| match at {
            CutAt::Seconds(t) if !(t.is_finite() && *t >= 0.0) => {
                Some(format!("{what} at {t} s must be a finite time of zero or more"))
            }
            CutAt::Marker(name) if name.is_empty() => Some(format!("{what} names an empty marker")),
            _ => None,
        };
        for change in &product.options {
            if let Some(message) = bad_time(&change.at, "a timed pick") {
                return invalid(message);
            }
            if change.group.is_empty() {
                return invalid("a timed pick has an empty group".into());
            }
        }
        for key in &product.view {
            if let Some(message) = bad_time(&key.at, "a view key") {
                return invalid(message);
            }
            let finite = key.tilt.is_none_or(|v| v.iter().all(|n| n.is_finite()))
                && key.spin.is_none_or(f32::is_finite)
                && key.pan.is_none_or(|v| v.iter().all(|n| n.is_finite()))
                && key.zoom.is_none_or(f32::is_finite);
            if !finite {
                return invalid("a view key has a number that is not finite".into());
            }
            if let Some(ease) = &key.ease
                && !matches!(ease.as_str(), "smooth" | "linear" | "hold")
            {
                return invalid(format!(
                    "a view key's ease `{ease}` must be smooth, linear or hold"
                ));
            }
        }
        let output = &self.output;
        let refused: [(&str, bool); 8] = [
            ("subframes above 1", output.subframes.is_some_and(|n| n > 1)),
            ("bit_depth 16", output.bit_depth.is_some_and(|n| n != 8)),
            ("format exr", output.format.as_deref() == Some("exr")),
            ("aovs", !output.aovs.is_empty()),
            ("audio", output.audio.is_some()),
            ("time_map", !output.time_map.is_empty()),
            ("warmup_frames", output.warmup_frames.is_some_and(|n| n > 0)),
            ("settle_frames", output.settle_frames.is_some_and(|n| n > 0)),
        ];
        if let Some((name, _)) = refused.iter().find(|(_, set)| *set) {
            return invalid(format!(
                "a product job renders display frames of the raster viewer with no history: {name} does not apply"
            ));
        }
        Ok(())
    }
}

impl TakeRange {
    /// Checks the range on its own: frames and seconds non-empty and finite, names non-empty.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Whole => Ok(()),
            Self::Frames { start, end } => {
                if end <= start {
                    Err(format!("frames {start}..{end} is empty"))
                } else {
                    Ok(())
                }
            }
            Self::Seconds { start, end } => {
                if !(start.is_finite() && end.is_finite() && *start >= 0.0) {
                    Err(format!(
                        "seconds {start}..{end} must be finite and start at zero or later"
                    ))
                } else if end <= start {
                    Err(format!("seconds {start}..{end} is empty"))
                } else {
                    Ok(())
                }
            }
            Self::Markers { from, to } => {
                if from.is_empty() || to.is_empty() {
                    Err("a marker range names an empty marker".into())
                } else {
                    Ok(())
                }
            }
            Self::Shots { first, last } => {
                if first.is_empty() || last.is_empty() {
                    Err("a shot range names an empty shot".into())
                } else {
                    Ok(())
                }
            }
        }
    }
}

/// The `.render_job.ron` migration chain. v1 → v2 (ADR-1880) only added the optional
/// `bindings` table, v2 → v3 (ADR-3369) only the optional `output.encode` and v3 → v4 (ADR-2757)
/// only the optional `product` block, so an older job parses as the current version unchanged and
/// each step only bumps the version. (The product block was v3 on the jewelry line before it met
/// ADR-3369's v3; both fields are optional, so a v3 file of either line loads and saves as v4.)
fn render_job_migrations() -> &'static [VersionStep<RenderJob>] {
    &[
        VersionStep {
            from: 1,
            to: 2,
            migrate: |_| Ok(()),
        },
        VersionStep {
            from: 2,
            to: 3,
            migrate: |_| Ok(()),
        },
        VersionStep {
            from: 3,
            to: 4,
            migrate: |_| Ok(()),
        },
    ]
}

/// The `from` versions of the `.render_job.ron` migration steps (for `cargo xtask check-formats`).
#[must_use]
pub fn render_job_migration_sources() -> Vec<u32> {
    render_job_migrations().iter().map(|s| s.from).collect()
}

/// The serde shape fingerprint of `.render_job.ron` (hashed by `cargo xtask check-formats`): the
/// field list of every struct in the tree, then the variant spellings of every enum.
#[must_use]
pub fn render_job_schema_fingerprint() -> String {
    use regia_core::schema::{serde_field_names, serde_variant_names};
    regia_core::schema::join_field_lists(&[
        serde_field_names::<RenderJob>(),
        serde_field_names::<OutputSettings>(),
        serde_field_names::<AudioSettings>(),
        serde_field_names::<regia_media::encode::EncodeSettings>(),
        serde_field_names::<TimeMapKey>(),
        serde_field_names::<Take>(),
        serde_variant_names::<TakeRange>(),
        serde_variant_names::<CutAt>(),
        serde_variant_names::<JobBinding>(),
        serde_field_names::<ProductCapture>(),
        serde_field_names::<OptionChange>(),
        serde_field_names::<ProductViewKey>(),
        serde_variant_names::<PickValue>(),
    ])
}

/// The civil date (`YYYY-MM-DD`, proleptic Gregorian) of the day `days` days after
/// 1970-01-01, for the `{date}` token. The days-to-civil conversion of H. Hinnant,
/// "chrono-Compatible Low-Level Date Algorithms" (public domain description; original code
/// here). The engine never calls a clock: the caller passes the day (`regia render --job` uses
/// the UTC day of its own clock).
#[must_use]
pub fn utc_date_from_unix_days(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

/// True for a `YYYY-MM-DD` string with a month 01..=12 and a day 01..=31 (the `{date}` value).
#[must_use]
pub fn is_valid_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| {
        bytes[range.clone()].iter().all(u8::is_ascii_digit).then(|| {
            bytes[range]
                .iter()
                .fold(0_u32, |n, b| n * 10 + u32::from(b - b'0'))
        })
    };
    matches!(
        (digits(0..4), digits(5..7), digits(8..10)),
        (Some(_), Some(1..=12), Some(1..=31))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renderout audit M6: a take stem becomes a folder and a movie file, and on Windows `NUL` opens
    /// the device; the job name was refused through `valid_name`, take stems were not.
    #[test]
    fn a_windows_device_name_is_not_a_take_name() {
        for bad in ["NUL", "nul", "CON", "COM1", "lpt9", "aux.mov"] {
            assert!(!is_valid_name(bad), "{bad}");
        }
        for good in ["take_01", "nullable", "console", "COM", "a.b"] {
            assert!(is_valid_name(good), "{good}");
        }
    }

    fn minimal() -> &'static str {
        r#"(version: 1, out: "renders", takes: [(name: "a", range: Frames(start: 0, end: 10))])"#
    }

    #[test]
    fn a_minimal_job_parses_with_the_defaults() {
        let job = RenderJob::from_ron(minimal()).expect("valid");
        assert_eq!(job.job_name(), "job");
        assert_eq!(job.path, DEFAULT_PATH);
        assert_eq!(job.output, OutputSettings::default());
        assert!(job.cuts.is_empty() && job.shots.is_none() && job.sequence.is_none());
        assert_eq!(job.max_preroll_frames, None);
    }

    #[test]
    fn a_full_job_round_trips_byte_identically() {
        let job = RenderJob {
            version: RENDER_JOB_FORMAT_VERSION,
            name: "trailer".into(),
            shots: Some("cine/trailer.shots.ron".into()),
            sequence: Some("cine/trailer.seq.ron".into()),
            bindings: [("hero".to_string(), JobBinding::Name("Hero".into()))].into(),
            out: "renders/trailer".into(),
            path: "{date}/{take}/{job}_{take}_{frame}".into(),
            output: OutputSettings {
                width: Some(1280),
                height: Some(720),
                fps: Some(24000),
                fps_den: Some(1001),
                supersample: Some(2),
                format: Some("png".into()),
                hud: Some(false),
                layout: Some("landscape".into()),
                calibration: Some("neutral".into()),
                warmup_frames: Some(8),
                settle_frames: Some(2),
                subframes: Some(4),
                shutter_angle: Some(180.0),
                bit_depth: Some(8),
                png_compression: Some("balanced".into()),
                writers: Some(2),
                aovs: vec!["depth".into()],
                aov_format: Some("exr".into()),
                audio: Some(AudioSettings {
                    sample_rate: Some(48_000),
                    format: Some("pcm24".into()),
                    stems: vec!["music".into()],
                    timeline: Some("render".into()),
                }),
                time_map: vec![TimeMapKey {
                    frame: 10,
                    scale: 0.25,
                    ease: Some("linear".into()),
                }],
                encode: Some(regia_media::encode::EncodeSettings {
                    quality: 40,
                    keyframe_interval: 48,
                    ..regia_media::encode::EncodeSettings::default()
                }),
            },
            takes: vec![
                Take {
                    name: "whole".into(),
                    range: TakeRange::Whole,
                    path: None,
                },
                Take {
                    name: "f".into(),
                    range: TakeRange::Frames { start: 3, end: 9 },
                    path: Some("{take}".into()),
                },
                Take {
                    name: "s".into(),
                    range: TakeRange::Seconds { start: 0.5, end: 2.0 },
                    path: None,
                },
                Take {
                    name: "m".into(),
                    range: TakeRange::Markers {
                        from: "boom".into(),
                        to: "end".into(),
                    },
                    path: None,
                },
                Take {
                    name: "sh".into(),
                    range: TakeRange::Shots {
                        first: "wide".into(),
                        last: "close".into(),
                    },
                    path: None,
                },
            ],
            cuts: vec![
                CutAt::Frame(12),
                CutAt::Seconds(1.5),
                CutAt::Marker("boom".into()),
            ],
            max_preroll_frames: Some(600),
            product: None,
        };
        let text = job.to_ron().expect("serializes");
        let back = RenderJob::from_ron(&text).expect("parses back");
        assert_eq!(back, job);
        assert_eq!(
            back.to_ron().expect("serializes"),
            text,
            "re-save is byte-identical"
        );
    }

    #[test]
    fn a_newer_version_is_refused_before_the_shape_is_read() {
        // Version 5 with a field v4 does not know: the version check runs first.
        let error =
            RenderJob::from_ron(r#"(version: 5, out: "r", takes: [], future_field: 1)"#).expect_err("newer");
        assert!(
            matches!(
                error,
                RenderJobError::Version {
                    found: 5,
                    expected: 4
                }
            ),
            "{error}"
        );
        assert_eq!(error.code(), "render_job_version");
    }

    #[test]
    fn shape_and_semantic_errors_name_the_problem() {
        let cases = [
            (r#"(version: 1, out: "r", takes: [])"#, "at least one take"),
            (
                r#"(version: 1, out: "", takes: [(name: "a", range: Whole)])"#,
                "`out`",
            ),
            (
                r#"(version: 1, out: "r", takes: [(name: "a", range: Whole), (name: "a", range: Whole)])"#,
                "duplicate take name `a`",
            ),
            (
                r#"(version: 1, out: "r", takes: [(name: "../x", range: Whole)])"#,
                "take name `../x`",
            ),
            (
                r#"(version: 1, out: "r", takes: [(name: "a", range: Frames(start: 5, end: 5))])"#,
                "take `a`: frames 5..5 is empty",
            ),
            (
                r#"(version: 1, out: "r", takes: [(name: "a", range: Seconds(start: 2.0, end: 1.0))])"#,
                "take `a`: seconds 2..1 is empty",
            ),
            (
                r#"(version: 1, out: "r", takes: [(name: "a", range: Seconds(start: -1.0, end: 1.0))])"#,
                "start at zero or later",
            ),
            (
                r#"(version: 1, out: "r", takes: [(name: "a", range: Markers(from: "", to: "b"))])"#,
                "empty marker",
            ),
            (
                r#"(version: 1, out: "r", takes: [(name: "a", range: Shots(first: "x", last: ""))])"#,
                "empty shot",
            ),
            (
                r#"(version: 1, out: "r", shots: Some(""), takes: [(name: "a", range: Whole)])"#,
                "`shots` is an empty path",
            ),
            (
                r#"(version: 1, name: ".hidden", out: "r", takes: [(name: "a", range: Whole)])"#,
                "job name",
            ),
        ];
        for (text, needle) in cases {
            let error = RenderJob::from_ron(text).expect_err(text);
            assert_eq!(error.code(), "render_job_invalid", "{text}: {error}");
            assert!(error.to_string().contains(needle), "{text}: {error}");
        }
        let error = RenderJob::from_ron(r#"(version: 1, out: "r", takes: [(name: "a", range: Later)])"#)
            .expect_err("unknown range kind");
        assert_eq!(error.code(), "render_job_parse", "{error}");
    }

    #[test]
    fn too_many_takes_or_cuts_are_refused() {
        let take = |i: usize| Take {
            name: format!("t{i}"),
            range: TakeRange::Whole,
            path: None,
        };
        let mut job = RenderJob::from_ron(minimal()).expect("valid");
        job.takes = (0..=MAX_TAKES).map(take).collect();
        assert!(
            job.validate()
                .is_err_and(|e| e.to_string().contains("limit is 256"))
        );
        job.takes = vec![take(0)];
        job.cuts = (0..=MAX_JOB_CUTS as u64).map(CutAt::Frame).collect();
        assert!(
            job.validate()
                .is_err_and(|e| e.to_string().contains("limit is 4096"))
        );
        job.cuts = vec![CutAt::Seconds(f64::NAN)];
        assert!(job.validate().is_err());
    }

    #[test]
    fn the_fingerprint_lists_every_struct_and_enum() {
        let print = render_job_schema_fingerprint();
        for needle in [
            "max_preroll_frames",
            "png_compression",
            "sample_rate",
            "scale",
            "range",
            "Whole",
            "Frames",
            "Seconds",
            "Markers",
            "Shots",
            "Frame",
            "Marker",
        ] {
            assert!(print.contains(needle), "{needle} missing from {print}");
        }
        assert!(
            print.contains("bindings") && print.contains("Name,Guid"),
            "{print}"
        );
        assert!(print.contains("keyframe_interval"), "v3 encode settings: {print}");
        assert!(
            print.contains("package,build,options,view") && print.contains("One,Many"),
            "{print}"
        );
        assert_eq!(
            render_job_migration_sources(),
            vec![1, 2, 3],
            "v1 -> v2 -> v3 -> v4"
        );
    }

    #[test]
    fn a_v1_job_migrates_to_the_current_version_and_the_binding_table_is_checked() {
        // v1 migrates through v2 and v3 to the current version (4 since ADR-2757: `product`).
        let job = RenderJob::from_ron(minimal()).expect("a v1 job loads");
        assert_eq!(job.version, RENDER_JOB_FORMAT_VERSION);
        assert_eq!(job.version, 4);
        assert!(job.bindings.is_empty() && job.output.encode.is_none());
        assert!(job.product.is_none());
        let saved = job.to_ron().expect("saves");
        assert!(saved.contains("version: 4"), "{saved}");
        assert_eq!(RenderJob::from_ron(&saved).expect("re-parses"), job);

        let bound = RenderJob::from_ron(
            r#"(version: 2, out: "r", sequence: Some("cine/a.seq.ron"),
                bindings: {"hero": Name("Hero"), "lamp": Guid("00000000-0000-0000-0000-000000000042")},
                takes: [(name: "a", range: Whole)])"#,
        )
        .expect("bindings parse");
        assert_eq!(bound.bindings.len(), 2);
        assert_eq!(bound.bindings["hero"], JobBinding::Name("Hero".into()));
        let text = bound.to_ron().expect("saves");
        assert_eq!(
            RenderJob::from_ron(&text).expect("again").to_ron().expect("ser"),
            text
        );

        let cases = [
            (
                r#"(version: 2, out: "r", bindings: {"a": Name("A")}, takes: [(name: "a", range: Whole)])"#,
                "names none",
            ),
            (
                r#"(version: 2, out: "r", sequence: Some("s"), bindings: {"": Name("A")}, takes: [(name: "a", range: Whole)])"#,
                "empty target",
            ),
            (
                r#"(version: 2, out: "r", sequence: Some("s"), bindings: {"a": Name("")}, takes: [(name: "a", range: Whole)])"#,
                "entity name is empty",
            ),
            (
                r#"(version: 2, out: "r", sequence: Some("s"), bindings: {"a": Guid("nope")}, takes: [(name: "a", range: Whole)])"#,
                "not a guid",
            ),
        ];
        for (text, needle) in cases {
            let error = RenderJob::from_ron(text).expect_err(text);
            assert_eq!(error.code(), "render_job_invalid", "{text}: {error}");
            assert!(error.to_string().contains(needle), "{text}: {error}");
        }
        let mut many = bound;
        many.bindings = (0..=MAX_JOB_BINDINGS)
            .map(|i| (format!("t{i}"), JobBinding::Name("x".into())))
            .collect();
        assert!(
            many.validate()
                .is_err_and(|e| e.to_string().contains("limit is 1024"))
        );
    }

    const PRODUCT: &str = r#"(version: 3, name: "swap", out: "r",
        output: (width: Some(1080), height: Some(1920), fps: Some(30), supersample: Some(2)),
        takes: [(name: "all", range: Seconds(start: 0.0, end: 4.0))],
        product: Some((
            package: "products/a",
            build: Some("capture/a.build.json"),
            options: [
                (at: Seconds(1.0), group: "Material", value: One("Rose")),
                (at: Frame(60), group: "Finish", value: Many(["Satin"]), adjust: true),
            ],
            view: [(at: Seconds(2.0), tilt: Some((12.0, -7.0)), zoom: Some(1.3), ease: Some("linear"))],
        )),
    )"#;

    #[test]
    fn a_product_job_parses_and_saves_back_the_same() {
        let job = RenderJob::from_ron(PRODUCT).expect("a product job");
        let product = job.product.as_ref().expect("the block");
        assert_eq!(product.package, "products/a");
        assert_eq!(product.options.len(), 2);
        assert_eq!(product.options[1].value, PickValue::Many(vec!["Satin".into()]));
        assert!(product.options[1].adjust && !product.options[0].adjust);
        assert_eq!(product.view[0].tilt, Some([12.0, -7.0]));
        assert_eq!(product.view[0].spin, None);
        let text = job.to_ron().expect("saves");
        let again = RenderJob::from_ron(&text).expect("re-parses");
        assert_eq!(again, job);
        assert_eq!(
            again.to_ron().expect("saves"),
            text,
            "byte-identical on a re-save"
        );
    }

    #[test]
    fn a_v2_job_loads_at_v4_with_no_product() {
        let job = RenderJob::from_ron(
            r#"(version: 2, out: "r", takes: [(name: "a", range: Frames(start: 0, end: 2))])"#,
        )
        .expect("a v2 job loads");
        assert_eq!(job.version, 4);
        assert_eq!(job.product, None);
    }

    #[test]
    fn a_product_block_with_what_a_raster_frame_cannot_do_is_refused() {
        let with = |output: &str, block: &str, extra: &str| {
            format!(
                r#"(version: 3, out: "r", output: ({output}), {extra} takes: [(name: "a", range: Frames(start: 0, end: 2))],
                    product: Some((package: "p", {block})))"#
            )
        };
        let cases = [
            (with("subframes: Some(4)", "", ""), "subframes"),
            (with("bit_depth: Some(16)", "", ""), "bit_depth"),
            (with(r#"format: Some("exr")"#, "", ""), "format exr"),
            (with(r#"aovs: ["depth"]"#, "", ""), "aovs"),
            (with("audio: Some(())", "", ""), "audio"),
            (with("time_map: [(frame: 0, scale: 1.0)]", "", ""), "time_map"),
            (with("warmup_frames: Some(3)", "", ""), "warmup_frames"),
            (with("settle_frames: Some(3)", "", ""), "settle_frames"),
            (
                with("", "", r#"sequence: Some("s"), bindings: {"a": Name("A")},"#),
                "bindings",
            ),
            (
                with(
                    "",
                    "view: [(at: Frame(0), zoom: Some(1.5))]",
                    r#"shots: Some("s"),"#,
                ),
                "not both",
            ),
            (
                with("", r#"options: [(at: Frame(0), group: "", value: One("x"))]"#, ""),
                "empty group",
            ),
            (
                with(
                    "",
                    r#"options: [(at: Seconds(-1.0), group: "g", value: One("x"))]"#,
                    "",
                ),
                "finite time",
            ),
            (
                with(
                    "",
                    r#"options: [(at: Marker(""), group: "g", value: One("x"))]"#,
                    "",
                ),
                "empty marker",
            ),
            (
                with("", r#"view: [(at: Frame(0), ease: Some("cubic"))]"#, ""),
                "ease",
            ),
            (with("", r#"build: Some("")"#, ""), "empty path"),
        ];
        for (text, needle) in cases {
            let error = RenderJob::from_ron(&text).expect_err(&text);
            assert_eq!(error.code(), "render_job_invalid", "{text}: {error}");
            assert!(error.to_string().contains(needle), "{text}: {error}");
        }
        let mut job = RenderJob::from_ron(PRODUCT).expect("valid");
        job.product.as_mut().expect("block").package.clear();
        assert!(
            job.validate()
                .is_err_and(|e| e.to_string().contains("empty path"))
        );
        let mut job = RenderJob::from_ron(PRODUCT).expect("valid");
        job.product.as_mut().expect("block").view[0].zoom = Some(f32::NAN);
        assert!(
            job.validate()
                .is_err_and(|e| e.to_string().contains("not finite"))
        );
        let mut job = RenderJob::from_ron(PRODUCT).expect("valid");
        let change = job.product.as_ref().expect("block").options[0].clone();
        job.product.as_mut().expect("block").options = vec![change; MAX_PRODUCT_EVENTS + 1];
        assert!(
            job.validate()
                .is_err_and(|e| e.to_string().contains("limit is 4096"))
        );
    }

    #[test]
    fn days_to_civil_dates_are_exact_across_eras_and_leap_days() {
        assert_eq!(utc_date_from_unix_days(0), "1970-01-01");
        assert_eq!(utc_date_from_unix_days(-1), "1969-12-31");
        assert_eq!(utc_date_from_unix_days(11_016), "2000-02-29");
        assert_eq!(utc_date_from_unix_days(11_017), "2000-03-01");
        assert_eq!(utc_date_from_unix_days(20_754), "2026-10-28");
        assert_eq!(utc_date_from_unix_days(-719_468), "0000-03-01");
        assert_eq!(utc_date_from_unix_days(47_540), "2100-02-28");
        assert_eq!(
            utc_date_from_unix_days(47_541),
            "2100-03-01",
            "2100 is not a leap year"
        );
    }

    #[test]
    fn the_documented_example_parses() {
        let text = include_str!("../../../docs/formats/render-job.example.ron");
        let job = RenderJob::from_ron(text).expect("docs/formats/render-job.example.ron parses");
        assert_eq!(job.job_name(), "trailer");
        assert_eq!(job.takes.len(), 3);
        assert_eq!(job.cuts.len(), 2);
        assert_eq!(job.output.audio.as_ref().map(|a| a.stems.len()), Some(1));
        assert_eq!(job.bindings.len(), 2, "the v2 binding table");
    }

    #[test]
    fn dates_are_checked_by_shape() {
        assert!(is_valid_date("2026-10-28"));
        for bad in [
            "2026-13-01",
            "2026-00-10",
            "2026-10-32",
            "26-10-28",
            "2026/10/28",
            "2026-1a-01",
            "",
        ] {
            assert!(!is_valid_date(bad), "{bad}");
        }
    }
}
