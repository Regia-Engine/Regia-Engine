// SPDX-License-Identifier: MPL-2.0
//! The pure half of render-out: the request, its validation, the fixed-dt timeline, the
//! supersample downscale and the frame hash. Nothing here touches the world, the GPU, the disk or
//! a clock, so everything is unit-tested.

use std::path::PathBuf;
use std::time::Duration;

use regia_render::HudLayout;
use serde_json::Value;

use super::accumulate::Depth;
use super::aov::AovRequest;
use super::audio::AudioRequest;
use super::shutter::{DEFAULT_SHUTTER_DEG, MAX_SUBFRAMES, Shutter};
use super::time_map::TimeMap;
use crate::protocol::RpcError;

/// Longest output side, pixels.
pub const MAX_OUTPUT_SIDE: u32 = 8192;
/// Most pixels a single (supersampled) render may have: 400 MB of RGBA8.
pub const MAX_RENDER_PIXELS: u64 = 100_000_000;
/// Largest supersampling factor.
pub const MAX_SUPERSAMPLE: u32 = 4;
/// Most frames one job renders.
pub const MAX_FRAMES: u64 = 1_000_000;
/// Most frames the job steps before frame 0.
pub const MAX_WARMUP: u64 = 10_000;

/// How the frames leave the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// One PNG per frame (`<name>_000000.png`), 8 bit.
    Png,
    /// FFV1 version 3 in Matroska, lossless RGB, through an `ffmpeg` pipe.
    Ffv1,
    /// ProRes 4444 in QuickTime, through an `ffmpeg` pipe.
    ProRes,
    /// DNxHR 444 in QuickTime, through an `ffmpeg` pipe.
    DnxHr,
    /// One uncompressed file per frame (`<name>_000000.raw`): tightly packed RGBA, 8 bit or
    /// 16 bit little-endian, for an external encoder (the manifest records the layout).
    Raw,
    /// One OpenEXR file per frame (`<name>_000000.exr`): scene-linear RGB in `f32`, before
    /// exposure, bloom and the tonemap (ADR-1521).
    Exr,
    /// AV1 video and the audio mixdown as Opus in WebM (`<name>.webm`), encoded in-process with no
    /// external tool (`regia_media::encode`, feature `media-encode`, ADR-3369). Settings: `encode`.
    Webm,
}

/// How hard the PNG encoder works on each frame. The pixels (and so the frame hash) are the same
/// at every level; only the file size and the time to write it change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PngLevel {
    /// Fast deflate: several times quicker than `Best`, files a little larger (the default).
    Fast,
    /// The `png` crate's balanced setting.
    Balanced,
    /// The smallest files the encoder makes, at a much higher cost per frame.
    Best,
}

impl PngLevel {
    /// Parses the `png_compression` parameter.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "fast" => Some(Self::Fast),
            "balanced" => Some(Self::Balanced),
            "best" => Some(Self::Best),
            _ => None,
        }
    }

    /// Stable name (protocol, manifest).
    pub fn name(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::Best => "best",
        }
    }
}

/// Most writer threads a request may ask for.
pub const MAX_WRITERS: u64 = 16;

/// The writer threads `render.out` uses when the request does not say: half the cores, 1..=8.
pub fn default_writers(cores: usize) -> usize {
    (cores / 2).clamp(1, 8)
}

impl Format {
    /// Parses the `format` parameter.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "png" => Some(Self::Png),
            "ffv1" => Some(Self::Ffv1),
            "prores" | "prores4444" => Some(Self::ProRes),
            "dnxhr" | "dnxhr444" => Some(Self::DnxHr),
            "raw" => Some(Self::Raw),
            "exr" => Some(Self::Exr),
            "webm" | "av1" => Some(Self::Webm),
            _ => None,
        }
    }

    /// Stable name (protocol, manifest).
    pub fn name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Ffv1 => "ffv1",
            Self::ProRes => "prores",
            Self::DnxHr => "dnxhr",
            Self::Raw => "raw",
            Self::Exr => "exr",
            Self::Webm => "webm",
        }
    }

    /// True when the format is encoded in-process (`webm`, ADR-3369).
    pub fn is_encoded(self) -> bool {
        matches!(self, Self::Webm)
    }

    /// True when the format needs an external `ffmpeg`.
    pub fn needs_ffmpeg(self) -> bool {
        matches!(self, Self::Ffv1 | Self::ProRes | Self::DnxHr)
    }

    /// True when the format is one file per frame (written by the writer pool).
    pub fn is_frame_files(self) -> bool {
        matches!(self, Self::Png | Self::Raw | Self::Exr)
    }

    /// File extension of the movie container.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Raw => "raw",
            Self::Exr => "exr",
            Self::Ffv1 => "mkv",
            Self::ProRes | Self::DnxHr => "mov",
            Self::Webm => "webm",
        }
    }
}

/// A validated `render.out_start` request.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// Delivered width, pixels.
    pub width: u32,
    /// Delivered height, pixels.
    pub height: u32,
    /// Frame rate numerator.
    pub fps_num: u32,
    /// Frame rate denominator (1001 for 29.97).
    pub fps_den: u32,
    /// Frames to deliver.
    pub frames: u64,
    /// Supersampling factor (render at this many times the width and height, then average).
    pub supersample: u32,
    /// Output format.
    pub format: Format,
    /// Output directory (created).
    pub out_dir: PathBuf,
    /// The project root `out_dir` was confined to; answers show paths relative to it.
    pub root: Option<PathBuf>,
    /// File stem of the frames and the movie.
    pub name: String,
    /// Draw the game UI.
    pub hud: bool,
    /// HUD layout.
    pub layout: HudLayout,
    /// Frames (0-based, within the job) that start with a camera cut. Frame 0 always does, unless
    /// [`Self::settle`] moves that cut to the first settle frame.
    pub cuts: Vec<u64>,
    /// Frames stepped, not delivered, before frame 0 (lets spawns and streaming settle).
    pub warmup: u64,
    /// The last this many of the [`Self::warmup`] frames render at the delivered size after the
    /// job's camera cut, so frame 0 has a temporal history (TAA, hashed alpha, fog, exposure) of
    /// that many frames instead of none (ADR-1449). 0 (the default): frame 0 cuts as before.
    pub settle: u64,
    /// Skip the player's display calibration (the default: a render-out is not a screen).
    pub neutral: bool,
    /// Subframes per delivered frame and the shutter angle (motion blur, ADR-1404).
    pub shutter: Shutter,
    /// Bits per channel of the delivered frames (16 = the deep readout, PNG only; ADR-1404).
    pub bit_depth: Depth,
    /// PNG deflate level (format png only; ADR-1412).
    pub png_level: PngLevel,
    /// Writer threads encoding and writing frame files; 0 = automatic ([`default_writers`]).
    pub writers: usize,
    /// Simulation time per delivered frame, per frame range (slow motion, speed ramps; ADR-1519,
    /// ADR-1524). Empty: every frame covers one [`Self::step`].
    pub time_map: TimeMap,
    /// The WAV mixdown and stems written beside the frames (ADR-1520); `None` without `audio`.
    pub audio: Option<AudioRequest>,
    /// Depth, id and motion files written next to each frame (ADR-1521). Empty: none.
    pub aovs: AovRequest,
    /// Record the wall time of each delivered frame's render (`timings`, ADR-2444); off by default.
    pub timings: bool,
    /// Encoder settings of format `webm` (`encode`, ADR-3369); the defaults for other formats.
    pub encode: regia_media::encode::EncodeSettings,
}

/// An encoder refusal as `F0002` with `data.reason` = the encoder's code (`encode_odd_size`,
/// `media_encode_unavailable`, ...; `book/agents/video-encode.md`).
pub(crate) fn encode_refused(e: &regia_media::encode::EncodeError) -> RpcError {
    let mut error = RpcError::new(
        crate::protocol::INVALID_PARAMS,
        "F0002",
        e.message.clone(),
        "see book/agents/video-encode.md for the encode settings and their limits",
    );
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), Value::from(e.code));
    }
    error
}

/// Most `cuts` entries a render-out request may carry.
pub const MAX_CUTS: usize = 4096;

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError::invalid_params(message)
}

fn u64_param(params: &Value, key: &str, default: Option<u64>, min: u64, max: u64) -> Result<u64, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => default.ok_or_else(|| invalid(format!("`{key}` is required"))),
        Some(v) => v
            .as_u64()
            .filter(|n| (min..=max).contains(n))
            .ok_or_else(|| invalid(format!("`{key}` must be an integer in {min}..={max}"))),
    }
}

/// A file stem that cannot escape the output directory.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        && !name.starts_with('.')
        && regia_core::project_path::check_relative(name).is_ok()
}

/// The directories `out` may live under: the project `root`, the system temp dir and the
/// `agent.toml` `output_roots`. Each is listed as given and canonicalized.
fn allowed_roots(root: &std::path::Path, extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = vec![root.to_path_buf(), std::env::temp_dir()];
    roots.extend(extra.iter().cloned());
    // Not a network or device form: canonicalizing `\\\\server\\share` connects to it, and the list
    // may come from a cloned `agent.toml`.
    let canonical: Vec<PathBuf> = roots
        .iter()
        .filter(|r| !regia_core::project_path::is_remote_form(&r.to_string_lossy()))
        .filter_map(|r| r.canonicalize().ok())
        .collect();
    roots.extend(canonical);
    roots
}

/// Resolves a caller-supplied output directory under the allowed output roots (see
/// [`allowed_roots`]): relative paths are joined to the project `root`, absolute ones must lie under
/// an allowed root, `..` and NUL are refused, and a symlink that leads out of every allowed root
/// (checked on the deepest existing ancestor) is refused. `.` is the project root itself. A refusal
/// is `F0002` with `data.reason` `path_outside_project` and the broken rule in `data.path_reason`
/// (`book/agents/safety.md`, Request paths).
pub(crate) fn confine_out_dir(
    root: &std::path::Path,
    extra_roots: &[PathBuf],
    out: &std::path::Path,
) -> Result<PathBuf, RpcError> {
    let resolved = resolve_under_roots(root, extra_roots, out).map_err(|e| {
        let mut refused = crate::req_path::refused_with(
            "`out` must be relative to the project root or an absolute path under an allowed output \
             root (project root, system temp dir, or `output_roots` in \
             .regia/agent.toml), without `..`",
            e.code(),
            "use a path relative to the project, or list the directory in `output_roots`; see \
             book/agents/safety.md (Request paths)",
        );
        crate::req_path::name_allowed_roots(&mut refused, OUTPUT_ROOT_KINDS);
        refused
    })?;
    // ADR-2821: `out` can come from a job file the guard on the request's parameters never saw.
    if crate::golden_tools::under_goldens(root, &resolved) {
        return Err(crate::golden_tools::golden_path_denied(
            &out.display().to_string(),
        ));
    }
    Ok(resolved)
}

/// The kinds of directory an output path may lie under (`data.allowed_roots` of a refusal).
pub(crate) const OUTPUT_ROOT_KINDS: &[&str] = &["project", "temp_dir", "output_roots"];

/// The output-root rule without its error text (see [`confine_out_dir`]): `.` is the project
/// `root`, a relative path joins it, an absolute one must lie under the project root, the system
/// temp dir or one of `extra_roots`; `..`, NUL and symlinks leading out are
/// refused. Creates nothing.
pub(crate) fn resolve_under_roots(
    root: &std::path::Path,
    extra_roots: &[PathBuf],
    path: &std::path::Path,
) -> Result<PathBuf, regia_core::project_path::ProjectPathError> {
    use std::path::Component;
    if path.components().next().is_some() && path.components().all(|c| matches!(c, Component::CurDir)) {
        return Ok(root.join(path));
    }
    regia_core::project_path::resolve_in_roots(&allowed_roots(root, extra_roots), &path.to_string_lossy())
}

impl Request {
    /// Confines `out_dir` to the allowed output roots (see [`confine_out_dir`]).
    pub(crate) fn confine_to(
        &mut self,
        root: &std::path::Path,
        extra_roots: &[PathBuf],
    ) -> Result<(), RpcError> {
        self.out_dir = confine_out_dir(root, extra_roots, &self.out_dir)?;
        self.root = Some(root.to_path_buf());
        Ok(())
    }

    /// `message` with the absolute project root replaced by `.`, for error text.
    pub(crate) fn scrub(&self, message: &str) -> String {
        match &self.root {
            Some(root) => message.replace(&root.display().to_string(), "."),
            None => message.to_string(),
        }
    }

    /// `path` as shown to the caller: relative to the project root when it lies under it, so no
    /// absolute host path leaves the process. A relative answer is `/`-separated on every host
    /// (Windows would otherwise mix the request's `/` with its own `\`).
    pub(crate) fn shown(&self, path: &std::path::Path) -> String {
        match self.root.as_deref().and_then(|root| path.strip_prefix(root).ok()) {
            Some(rel) if rel.as_os_str().is_empty() => ".".to_string(),
            Some(rel) if cfg!(windows) => rel.display().to_string().replace('\\', "/"),
            Some(rel) => rel.display().to_string(),
            None => path.display().to_string(),
        }
    }

    /// Parses and validates the parameters of `render.out_start`.
    pub fn parse(params: &Value) -> Result<Self, RpcError> {
        // A capture preset fills in the keys the request left out (ADR-2449).
        let merged = super::capture::with_preset(params)?;
        let params = merged.as_ref();
        let width = u64_param(params, "width", Some(1920), 1, u64::from(MAX_OUTPUT_SIDE))? as u32;
        let height = u64_param(params, "height", Some(1080), 1, u64::from(MAX_OUTPUT_SIDE))? as u32;
        let fps_num = u64_param(params, "fps", Some(30), 1, 1_000_000).map_err(|_| {
            invalid("`fps` must be an integer in 1..=1000000 (29.97 is fps 30000 with fps_den 1001)")
        })? as u32;
        let fps_den = u64_param(params, "fps_den", Some(1), 1, 100_000)? as u32;
        if u64::from(fps_num) > 1000 * u64::from(fps_den) {
            return Err(invalid("the frame rate is above 1000 fps"));
        }
        let rate = f64::from(fps_num) / f64::from(fps_den);
        let frames = match (params.get("frames"), params.get("duration_s")) {
            (Some(v), _) if !v.is_null() => u64_param(params, "frames", None, 1, MAX_FRAMES)?,
            (_, Some(d)) if !d.is_null() => {
                let seconds = d
                    .as_f64()
                    .filter(|s| s.is_finite() && *s > 0.0)
                    .ok_or_else(|| invalid("`duration_s` must be a positive number"))?;
                let frames = (seconds * rate).round() as u64;
                if !(1..=MAX_FRAMES).contains(&frames) {
                    return Err(invalid(format!(
                        "`duration_s` gives {frames} frames, not 1..={MAX_FRAMES}"
                    )));
                }
                frames
            }
            _ => return Err(invalid("give `frames` or `duration_s`")),
        };
        let supersample = u64_param(params, "supersample", Some(1), 1, u64::from(MAX_SUPERSAMPLE))? as u32;
        let render_pixels = u64::from(width) * u64::from(height) * u64::from(supersample).pow(2);
        if render_pixels > MAX_RENDER_PIXELS {
            return Err(invalid(format!(
                "{width}x{height} at supersample {supersample} renders {render_pixels} pixels; the limit is {MAX_RENDER_PIXELS}"
            )));
        }
        let format = match crate::params::opt_str(params, "format")? {
            None => Format::Png,
            Some(text) => Format::parse(text).ok_or_else(|| {
                invalid(format!(
                    "unknown format {text:?} (png, raw, exr, webm, ffv1, prores, dnxhr)"
                ))
            })?,
        };
        let out_dir = crate::params::opt_str(params, "out")?
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| invalid("`out` (an output directory) is required"))?;
        let name = crate::params::opt_str(params, "name")?
            .unwrap_or("frame")
            .to_string();
        if !valid_name(&name) {
            return Err(invalid(
                "`name` must be 1..=64 letters, digits, `_`, `-` or `.` and not start with `.`",
            ));
        }
        let layout = match crate::params::opt_str(params, "layout")? {
            None => HudLayout::Auto,
            Some(text) => HudLayout::parse(text).ok_or_else(|| {
                invalid(format!(
                    "unknown layout {text:?} (\"auto\", \"landscape\" or \"portrait\")"
                ))
            })?,
        };
        let hud = match params.get("hud") {
            None | Some(Value::Null) => true,
            Some(v) => v.as_bool().ok_or_else(|| invalid("`hud` must be a boolean"))?,
        };
        let neutral = match crate::params::opt_str(params, "calibration")? {
            None | Some("neutral") => true,
            Some("applied") => false,
            Some(other) => {
                return Err(invalid(format!(
                    "unknown calibration {other:?} (\"neutral\" or \"applied\")"
                )));
            }
        };
        let mut cuts = Vec::new();
        match params.get("cuts") {
            None | Some(Value::Null) => {}
            Some(Value::Array(list)) => {
                if list.len() > MAX_CUTS {
                    return Err(invalid(format!(
                        "`cuts` has {} entries; the limit is {MAX_CUTS}",
                        list.len()
                    )));
                }
                for entry in list {
                    let frame = entry.as_u64().filter(|f| *f < frames).ok_or_else(|| {
                        invalid(format!("`cuts` entries must be frame numbers below {frames}"))
                    })?;
                    cuts.push(frame);
                }
            }
            Some(_) => return Err(invalid("`cuts` must be an array of frame numbers")),
        }
        cuts.sort_unstable();
        cuts.dedup();
        let warmup = u64_param(params, "warmup_frames", Some(0), 0, MAX_WARMUP)?;
        let settle = u64_param(params, "settle_frames", Some(0), 0, MAX_WARMUP)?;
        if settle > warmup {
            return Err(invalid(format!(
                "`settle_frames` ({settle}) must not exceed `warmup_frames` ({warmup}): the settle frames are the last warm-up frames"
            )));
        }
        let subframes = u64_param(params, "subframes", Some(1), 1, u64::from(MAX_SUBFRAMES))? as u32;
        let angle = match params.get("shutter_angle") {
            None | Some(Value::Null) => DEFAULT_SHUTTER_DEG,
            Some(v) => v
                .as_f64()
                .ok_or_else(|| invalid("`shutter_angle` must be a number of degrees in (0, 360]"))?,
        };
        let shutter = Shutter::new(subframes, angle)
            .ok_or_else(|| invalid("`shutter_angle` must be a number of degrees in (0, 360]"))?;
        let bit_depth = match u64_param(params, "bit_depth", Some(8), 8, 16)? {
            8 => Depth::Eight,
            16 => Depth::Sixteen,
            _ => return Err(invalid("`bit_depth` must be 8 or 16")),
        };
        if bit_depth == Depth::Sixteen {
            if !format.is_frame_files() {
                return Err(invalid(
                    "`bit_depth` 16 writes 16-bit PNGs or raw frames; the movie formats take 8-bit frames (use format \"png\" or \"raw\")",
                ));
            }
            if !neutral {
                return Err(invalid(
                    "`bit_depth` 16 renders before the display calibration; use calibration \"neutral\"",
                ));
            }
        }
        let png_level = match params.get("png_compression") {
            None | Some(Value::Null) => PngLevel::Fast,
            Some(v) => v.as_str().and_then(PngLevel::parse).ok_or_else(|| {
                invalid("`png_compression` must be \"fast\" (default), \"balanced\" or \"best\"")
            })?,
        };
        let writers = u64_param(params, "writers", Some(0), 0, MAX_WRITERS)? as usize;
        let time_map = TimeMap::parse(params.get("time_map"), MAX_FRAMES).map_err(invalid)?;
        if format == Format::Exr {
            if bit_depth == Depth::Sixteen {
                return Err(invalid(
                    "format \"exr\" writes 32-bit float scene-linear frames; drop `bit_depth` 16",
                ));
            }
            if !neutral {
                return Err(invalid(
                    "format \"exr\" is scene-linear, before the display calibration; use calibration \"neutral\"",
                ));
            }
        }
        let aovs = AovRequest::parse(params)?;
        if aovs.is_on() && !format.is_frame_files() {
            return Err(invalid(
                "`aovs` are written by the frame writers: use format \"png\", \"raw\" or \"exr\" (encode a movie from the frames later)",
            ));
        }
        let audio = AudioRequest::parse(params.get("audio")).map_err(invalid)?;
        let timings = match params.get("timings") {
            None | Some(Value::Null) => false,
            Some(v) => v
                .as_bool()
                .ok_or_else(|| invalid("`timings` must be a boolean"))?,
        };
        let encode = match params.get("encode") {
            None | Some(Value::Null) => regia_media::encode::EncodeSettings::default(),
            Some(_) if format != Format::Webm => {
                return Err(invalid(
                    "`encode` sets the encoder of format \"webm\"; drop it or use format \"webm\"",
                ));
            }
            Some(value) => {
                regia_media::encode::EncodeSettings::from_json(value).map_err(|e| encode_refused(&e))?
            }
        };
        if format == Format::Webm {
            // The writer in encode_sink is gated on this crate's feature. Cargo may enable
            // regia_media's encoder through another workspace member without enabling it here.
            if !cfg!(feature = "media-encode") {
                return Err(encode_refused(&regia_media::encode::EncodeError::unavailable()));
            }
            regia_media::encode::check_picture(width, height, fps_num, fps_den)
                .map_err(|e| encode_refused(&e))?;
            if let Some(audio) = &audio
                && !regia_media::encode::AUDIO_RATES.contains(&audio.sample_rate)
            {
                return Err(invalid(format!(
                    "format \"webm\" encodes the mixdown as Opus, which takes 8000, 12000, 16000, 24000 or 48000 Hz; `audio.sample_rate` is {}",
                    audio.sample_rate
                )));
            }
        }
        let request = Self {
            width,
            height,
            fps_num,
            fps_den,
            frames,
            supersample,
            format,
            out_dir,
            root: None,
            name,
            hud,
            layout,
            cuts,
            warmup,
            settle,
            neutral,
            shutter,
            bit_depth,
            png_level,
            writers,
            time_map,
            audio,
            aovs,
            timings,
            encode,
        };
        if let Some(audio) = &request.audio {
            super::audio::check_wav_size(&request, audio).map_err(invalid)?;
        }
        Ok(request)
    }

    /// True when a delivered frame is an accumulation of several renders or a 16-bit frame (the
    /// ADR-1404 path); false for the classic one-render 8-bit frame.
    pub fn accumulates(&self) -> bool {
        self.shutter.is_on() || self.bit_depth != Depth::Eight
    }

    /// Width of the image the GPU renders.
    pub fn render_width(&self) -> u32 {
        self.width * self.supersample
    }

    /// Height of the image the GPU renders.
    pub fn render_height(&self) -> u32 {
        self.height * self.supersample
    }

    /// The fixed game-time step of every frame: `fps_den / fps_num` seconds, rounded to the
    /// nanosecond. The same value feeds every frame, so the timeline never depends on the wall
    /// clock or on how long a frame took to render.
    pub fn step(&self) -> Duration {
        let nanos = (1_000_000_000_u128 * u128::from(self.fps_den) + u128::from(self.fps_num) / 2)
            / u128::from(self.fps_num);
        Duration::from_nanos(nanos as u64)
    }

    /// Game (render-clock) time of delivered frame `index` after frame 0: `index` steps. This is
    /// the output's own timeline (`game_time_s`); a [`Self::time_map`] never changes it.
    pub fn frame_time(&self, index: u64) -> Duration {
        let step = self.step();
        Duration::from_nanos((step.as_nanos() as u64).saturating_mul(index))
    }

    /// Simulation time of delivered frame `index` after frame 0 (`sim_time_s`): `index` steps, or
    /// with a [`Self::time_map`] the sum of the simulation steps of the frames before it
    /// (ADR-1524). The job keeps the same sum as it goes (`Job::sim_ns`); this is the reference
    /// the tests check it against.
    #[cfg(test)]
    pub fn sim_time(&self, index: u64) -> Duration {
        Duration::from_nanos(self.time_map.sim_ns(self.step_ns(), index))
    }

    /// [`Self::step`] in whole nanoseconds.
    pub fn step_ns(&self) -> u64 {
        self.step().as_nanos() as u64
    }

    /// Simulation time delivered frame `index` covers, nanoseconds (the simulation clock's step
    /// from it to the next frame): one step scaled by the [`Self::time_map`].
    pub fn sim_step_ns(&self, index: u64) -> u64 {
        self.time_map.frame_step_ns(self.step_ns(), index)
    }

    /// Simulation time each warm-up frame, and the step into frame 0, covers, nanoseconds.
    pub fn pre_sim_step_ns(&self) -> u64 {
        self.time_map.pre_step_ns(self.step_ns())
    }

    /// The longest simulation time one app frame of this job advances, nanoseconds: the longest
    /// frame step, doubled with the shutter on (the step into a frame's first subframe can add up
    /// to one more frame).
    pub fn peak_sim_step_ns(&self) -> u64 {
        let peak = self.time_map.peak_step_ns(self.step_ns(), self.frames);
        if self.shutter.is_on() {
            peak.saturating_mul(2)
        } else {
            peak
        }
    }

    /// True when `index` starts with a camera cut. With [`Self::settle`] frames the job's opening
    /// cut is at the first of them, so frame 0 cuts only when `cuts` lists it.
    pub fn is_cut(&self, index: u64) -> bool {
        (index == 0 && self.settle == 0) || self.cuts.binary_search(&index).is_ok()
    }

    /// True when the warm-up frame with `left` warm-up frames still to go (counting itself) is a
    /// settle frame: rendered at the delivered size.
    pub fn is_settle(&self, left: u64) -> bool {
        left <= self.settle
    }

    /// The file a frame of a frame-file format (PNG, raw or EXR) is written to.
    pub fn frame_path(&self, index: u64) -> PathBuf {
        let extension = match self.format {
            Format::Raw => "raw",
            Format::Exr => "exr",
            _ => "png",
        };
        self.out_dir.join(format!("{}_{index:06}.{extension}", self.name))
    }

    /// Every file of delivered frame `index`: its AOV files, then the frame (frame-file formats).
    pub fn frame_files(&self, index: u64) -> Vec<PathBuf> {
        let mut files = self.aovs.paths(&self.out_dir, &self.name, index);
        files.push(self.frame_path(index));
        files
    }

    /// The writer threads this request uses (the request's, else [`default_writers`] of the
    /// machine's cores).
    pub fn writer_threads(&self) -> usize {
        if self.writers > 0 {
            return self.writers;
        }
        default_writers(std::thread::available_parallelism().map_or(1, usize::from))
    }

    /// The movie file of a video format.
    pub fn movie_path(&self) -> PathBuf {
        self.out_dir
            .join(format!("{}.{}", self.name, self.format.extension()))
    }
}

/// The small size warm-up frames render at: they only have to advance the simulations.
pub fn warmup_size(width: u32, height: u32) -> (u32, u32) {
    ((width / 8).max(16), (height / 8).max(16))
}

/// FNV-1a, 64 bit, over `bytes`; the frame hash.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Folds a frame hash into a sequence hash (order matters).
pub fn fold_hash(sequence: u64, frame: u64) -> u64 {
    fnv1a(&[sequence.to_le_bytes(), frame.to_le_bytes()].concat())
}

/// sRGB code value to linear light, in 20-bit fixed point.
fn srgb_to_linear_fixed(code: u8) -> u32 {
    let c = f64::from(code) / 255.0;
    let linear = if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    };
    (linear * f64::from(1_u32 << 20)).round() as u32
}

/// Averages `factor`×`factor` blocks of an sRGB-encoded RGBA8 image in linear light (alpha
/// linearly). `width` and `height` are the delivered size; `rgba` is `factor` times larger in
/// each direction. After the 256-entry table is built the arithmetic is integer only, so the
/// result is bit-identical on every platform.
pub fn downsample(rgba: &[u8], width: u32, height: u32, factor: u32) -> Vec<u8> {
    if factor <= 1 {
        return rgba.to_vec();
    }
    let linear: Vec<u32> = (0..=255u8).map(srgb_to_linear_fixed).collect();
    // The code value whose linear light is nearest to an average lies between two table
    // entries when the average is under their midpoint: `twice[i]` is that midpoint doubled.
    let twice: Vec<u64> = linear
        .windows(2)
        .map(|w| u64::from(w[0]) + u64::from(w[1]))
        .collect();
    let (w, h, f) = (width as usize, height as usize, factor as usize);
    let src_row = w * f * 4;
    let count = (f * f) as u64;
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let mut sums = [0u64; 4];
            for dy in 0..f {
                let row = (y * f + dy) * src_row;
                for dx in 0..f {
                    let at = row + (x * f + dx) * 4;
                    for (c, sum) in sums.iter_mut().take(3).enumerate() {
                        *sum += u64::from(linear[rgba[at + c] as usize]);
                    }
                    sums[3] += u64::from(rgba[at + 3]);
                }
            }
            let dst = (y * w + x) * 4;
            for (c, sum) in sums.iter().take(3).enumerate() {
                // Number of table midpoints the average has passed = the nearest code value.
                let code = twice.partition_point(|mid| 2 * *sum > mid * count);
                out[dst + c] = code as u8;
            }
            out[dst + 3] = ((sums[3] + count / 2) / count) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(params: Value) -> Result<Request, RpcError> {
        Request::parse(&params)
    }

    #[test]
    fn filesystem_roots_and_device_names_are_refused() {
        let root = regia_test_utils::unique_temp_dir("render-out-fs-root");
        let outside = if cfg!(windows) {
            "C:\\Windows\\regia-capture"
        } else {
            "/usr/share/regia-capture"
        };
        assert!(resolve_under_roots(&root, &[PathBuf::from("/")], std::path::Path::new(outside)).is_err());
        std::fs::remove_dir_all(root).expect("cleanup");
        for bad in [
            "CON", "con", "Nul.x", "COM1", "COM0", "lpt9", "aux.tar", "PRN", "name.", "CONIN$",
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
        for ok in ["COM10", "console", "shot", "lpt"] {
            assert!(valid_name(ok), "{ok}");
        }
    }

    #[test]
    fn answers_show_project_relative_paths() {
        let root = std::env::temp_dir().join("regia-shown-root");
        let mut r = parse(json!({ "out": "renders/a", "frames": 2 })).expect("request");
        r.confine_to(&root, &[]).expect("confined");
        assert_eq!(r.shown(&r.out_dir), "renders/a");
        assert_eq!(r.shown(&r.out_dir.join("f.png")), "renders/a/f.png");
        let message = format!("{}: denied", r.out_dir.display());
        assert!(!r.scrub(&message).contains("regia-shown-root"));
    }

    #[test]
    fn cuts_are_capped() {
        let cuts: Vec<u64> = (0..=MAX_CUTS as u64).collect();
        assert!(parse(json!({ "out": "o", "frames": 10_000, "cuts": cuts })).is_err());
        assert!(parse(json!({ "out": "o", "frames": 10_000, "cuts": [1, 2, 3] })).is_ok());
    }

    /// ADR-2821: `out` from a job file never passed the dispatcher's guard.
    #[test]
    fn out_dir_cannot_be_under_a_goldens_directory() {
        let root = regia_test_utils::unique_temp_dir("confine-goldens");
        for out in [
            std::path::PathBuf::from("crates/demo/goldens"),
            std::path::PathBuf::from("goldens/a"),
            root.join("crates/demo/GOLDENS/x"),
        ] {
            let error = confine_out_dir(&root, &[], &out).expect_err("refused");
            let reason = error.data.map(|d| d["reason"].clone());
            assert_eq!(
                reason,
                Some(serde_json::json!("golden_path_denied")),
                "{}",
                out.display()
            );
        }
        assert!(confine_out_dir(&root, &[], std::path::Path::new("renders/golden_hour")).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn out_dir_is_confined_to_the_allowed_roots() {
        let root = std::env::temp_dir().join(format!("regia-confine-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("mkdir");
        let ok = confine_out_dir(&root, &[], std::path::Path::new("renders/a")).expect("relative");
        assert_eq!(ok, root.join("renders/a"));
        assert!(confine_out_dir(&root, &[], &root.join("b")).is_ok());
        // An absolute directory under the system temp dir is allowed, even beside the project.
        let beside = std::env::temp_dir().join(format!("regia-beside-{}", std::process::id()));
        assert!(confine_out_dir(&root, &[], &beside).is_ok());
        assert!(confine_out_dir(&root, &[], std::path::Path::new("../x")).is_err());
        assert!(confine_out_dir(&root, &[], std::path::Path::new("a/../../x")).is_err());
        assert!(confine_out_dir(&root, &[], &beside.join("..").join("x")).is_err());
        #[cfg(unix)]
        {
            assert!(confine_out_dir(&root, &[], std::path::Path::new("/usr/share/regia")).is_err());
            let link = root.join("escape");
            std::os::unix::fs::symlink("/usr", &link).expect("symlink");
            assert!(confine_out_dir(&root, &[], std::path::Path::new("escape/sub")).is_err());
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The `path_reason` of a refused `out`, after checking the rest of the documented refusal.
    fn refusal(root: &std::path::Path, out: &str) -> String {
        let error = confine_out_dir(root, &[], std::path::Path::new(out)).expect_err(out);
        assert_eq!(error.code, crate::protocol::INVALID_PARAMS, "{out}");
        assert!(error.message.starts_with("`out` must be relative"), "{out}");
        assert!(!error.message.contains(&root.display().to_string()), "{out}");
        let data = error.data.expect("data");
        assert_eq!(data["code"], "F0002", "{out}");
        assert_eq!(data["reason"], "path_outside_project", "{out}");
        assert!(
            data["hint"].as_str().is_some_and(|h| h.contains("safety.md")),
            "{out}"
        );
        data["path_reason"].as_str().expect("path_reason").to_string()
    }

    #[test]
    fn every_refused_out_shape_carries_the_documented_reason() {
        // `book/agents/safety.md`, Request paths: the same reasons on every host.
        let base = regia_test_utils::unique_temp_dir("render-out-shapes");
        let root = base.join("project");
        std::fs::create_dir_all(&root).expect("root");
        for (out, reason) in [
            // `..`, in a relative and in an absolute path.
            ("../x", "path_parent"),
            ("a/../../x", "path_parent"),
            ("a\\..\\..\\x", "path_parent"),
            ("a/.. /x", "path_parent"),
            // Drive letters, relative to a drive or not.
            ("C:x", "path_prefix"),
            ("C:\\x", "path_prefix"),
            ("c:/x", "path_prefix"),
            // NUL.
            ("a\0b", "path_nul"),
            // Device names.
            ("NUL", "path_device"),
            ("renders/COM1.txt", "path_device"),
        ] {
            #[cfg(windows)]
            if out.len() > 1 && out.as_bytes()[1] == b':' && out.as_bytes()[2] != b'x' {
                continue; // an absolute path on Windows: `path_escapes`, below.
            }
            assert_eq!(refusal(&root, out), reason, "{out}");
        }
        let beside = base.join("..").join("x").display().to_string();
        assert_eq!(refusal(&root, &beside), "path_parent", "{beside}");
        let nul = format!("{}/a\0b", root.display());
        assert_eq!(refusal(&root, &nul), "path_nul");
        // UNC: a leading `\\` is absolute on Windows and under no root there, so it is refused as a
        // network form before the filesystem is touched (`book/agents/safety.md`; ADR-7566);
        // elsewhere it is the same refusal by shape.
        let unc = refusal(&root, "\\\\server\\share\\x");
        assert_eq!(
            unc,
            if cfg!(windows) {
                "path_remote"
            } else {
                "path_absolute"
            }
        );
        let rooted = refusal(&root, "\\x");
        assert_eq!(rooted, "path_absolute");
        #[cfg(unix)]
        {
            // Absolute, under no allowed root.
            assert_eq!(refusal(&root, "/usr/share/regia"), "path_escapes");
            // A symlink inside the project leading out, then one left dangling.
            std::os::unix::fs::symlink("/usr", root.join("escape")).expect("symlink");
            assert_eq!(refusal(&root, "escape/sub"), "path_escapes");
            std::os::unix::fs::symlink(base.join("nowhere"), root.join("dangling")).expect("symlink");
            assert_eq!(refusal(&root, "dangling/sub"), "path_escapes");
        }
        // The project root itself and paths under it stay fine.
        assert_eq!(
            confine_out_dir(&root, &[], std::path::Path::new(".")).ok(),
            Some(root.join("."))
        );
        assert!(confine_out_dir(&root, &[], std::path::Path::new("./renders/a")).is_ok());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_relative_out_resolves_against_the_project_root_not_the_working_dir() {
        let root = regia_test_utils::unique_temp_dir("render-out-relative");
        let cwd = std::env::current_dir().expect("cwd");
        assert_ne!(root, cwd, "the project root is not the process working dir");
        let mut r = parse(json!({ "out": "renders/take1", "frames": 4 })).expect("request");
        r.confine_to(&root, &[]).expect("relative out");
        assert_eq!(r.out_dir, root.join("renders/take1"));
        assert_ne!(r.out_dir, cwd.join("renders/take1"));
        assert_eq!(r.shown(&r.out_dir), "renders/take1");
        // Cwd is not an implicit root. Test the root list even if this checkout lives in temp.
        let roots = allowed_roots(&root, &[]);
        let temp = std::env::temp_dir();
        if cwd != temp && temp.canonicalize().ok().as_ref() != Some(&cwd) {
            assert!(!roots.contains(&cwd), "cwd must require explicit configuration");
        }
        // The `output_roots` rules are unchanged: `..` is still refused.
        let mut escaping = parse(json!({ "out": "../x", "frames": 4 })).expect("request");
        assert!(escaping.confine_to(&root, &[]).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn extra_output_roots_extend_the_allowed_list() {
        let root = std::env::temp_dir().join(format!("regia-confine-x-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("mkdir");
        #[cfg(unix)]
        {
            // The extra root must exist (it is canonicalized); the out dir beneath it need not.
            let farm = std::path::PathBuf::from("/usr/share");
            let out = farm.join("regia-job1");
            assert!(confine_out_dir(&root, &[], &out).is_err());
            assert!(confine_out_dir(&root, &[farm], &out).is_ok());
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn defaults_and_the_required_fields() {
        let r = parse(json!({ "out": "/tmp/x", "frames": 48 })).expect("request");
        assert_eq!((r.width, r.height, r.fps_num, r.fps_den), (1920, 1080, 30, 1));
        assert_eq!(
            (r.supersample, r.format, r.hud, r.neutral),
            (1, Format::Png, true, true)
        );
        assert_eq!(r.layout, HudLayout::Auto);
        assert!(parse(json!({ "frames": 4 })).is_err(), "no out");
        assert!(
            parse(json!({ "out": "/tmp/x" })).is_err(),
            "no frames or duration"
        );
        let r = parse(json!({ "out": "/tmp/x", "duration_s": 2.5, "fps": 24 })).expect("duration");
        assert_eq!(r.frames, 60);
    }

    #[test]
    fn limits_and_names_are_enforced() {
        let base = |extra: Value| {
            let mut v = json!({ "out": "/tmp/x", "frames": 4 });
            v.as_object_mut()
                .expect("json object")
                .extend(extra.as_object().expect("json object").clone());
            parse(v)
        };
        assert!(base(json!({ "width": 0 })).is_err());
        assert!(base(json!({ "width": 8193 })).is_err());
        assert!(base(json!({ "supersample": 5 })).is_err());
        assert!(
            base(json!({ "width": 7680, "height": 4320, "supersample": 4 })).is_err(),
            "pixel limit"
        );
        assert!(base(json!({ "width": 3840, "height": 2160, "supersample": 3 })).is_ok());
        assert!(base(json!({ "format": "gif" })).is_err());
        assert!(base(json!({ "name": "../escape" })).is_err());
        assert!(base(json!({ "name": ".hidden" })).is_err());
        assert!(base(json!({ "layout": "square" })).is_err());
        assert!(base(json!({ "fps": 2.5 })).is_err());
        assert!(base(json!({ "cuts": [4] })).is_err(), "cut past the last frame");
        assert!(base(json!({ "cuts": "x" })).is_err());
    }

    #[test]
    fn cuts_are_sorted_deduplicated_and_frame_zero_always_cuts() {
        let r = parse(json!({ "out": "/o", "frames": 100, "cuts": [50, 20, 50] })).expect("request");
        assert_eq!(r.cuts, [20, 50]);
        assert!(r.is_cut(0) && r.is_cut(20) && r.is_cut(50));
        assert!(!r.is_cut(1) && !r.is_cut(49));
    }

    /// ADR-1449: settle frames are the last warm-up frames; the job's opening cut moves from
    /// frame 0 to the first of them, and a listed cut still cuts.
    #[test]
    fn settle_frames_move_the_opening_cut_into_the_warm_up() {
        let r = parse(json!({ "out": "/o", "frames": 4, "warmup_frames": 10, "settle_frames": 3 }))
            .expect("request");
        assert_eq!((r.warmup, r.settle), (10, 3));
        assert!(!r.is_cut(0), "frame 0 keeps the settle frames' history");
        // `left` counts the warm-up frames still to go, this one included.
        assert!(!r.is_settle(10) && !r.is_settle(4));
        assert!(r.is_settle(3) && r.is_settle(2) && r.is_settle(1));
        let listed = parse(json!({
            "out": "/o", "frames": 4, "warmup_frames": 10, "settle_frames": 3, "cuts": [0, 2]
        }))
        .expect("request");
        assert!(listed.is_cut(0) && listed.is_cut(2) && !listed.is_cut(1));
        let plain = parse(json!({ "out": "/o", "frames": 4, "warmup_frames": 10 })).expect("request");
        assert_eq!(plain.settle, 0);
        assert!(plain.is_cut(0) && !plain.is_settle(1));
    }

    #[test]
    fn the_timeline_is_fixed_and_never_reads_a_clock() {
        let r = parse(json!({ "out": "/o", "frames": 10, "fps": 60 })).expect("request");
        assert_eq!(r.step(), Duration::from_nanos(16_666_667));
        assert_eq!(r.frame_time(0), Duration::ZERO);
        assert_eq!(r.frame_time(3), Duration::from_nanos(50_000_001));
        let ntsc =
            parse(json!({ "out": "/o", "frames": 10, "fps": 30000, "fps_den": 1001 })).expect("request");
        assert_eq!(ntsc.step(), Duration::from_nanos(33_366_667));
        // The same request always gives the same timeline.
        assert_eq!(ntsc.step(), ntsc.clone().step());
    }

    #[test]
    fn warm_up_frames_render_at_an_eighth_of_the_size() {
        // A windowed host not stepping the particles twice is the VFX layer's once-per-frame
        // step now (ADR-2687, `regia_vfx` runtime tests).
        assert_eq!(warmup_size(1920, 1080), (240, 135));
        assert_eq!(warmup_size(64, 64), (16, 16));
    }

    #[test]
    fn paths_follow_the_name() {
        let r = parse(json!({ "out": "/o", "frames": 2, "name": "shot_a", "format": "prores" }))
            .expect("request");
        assert_eq!(r.frame_path(7), PathBuf::from("/o/shot_a_000007.png"));
        assert_eq!(r.movie_path(), PathBuf::from("/o/shot_a.mov"));
        assert_eq!((r.render_width(), r.render_height()), (1920, 1080));
    }

    #[test]
    fn hashes_depend_on_content_and_order() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_ne!(fnv1a(b"ab"), fnv1a(b"ba"));
        let (a, b) = (fnv1a(b"one"), fnv1a(b"two"));
        assert_ne!(fold_hash(fold_hash(0, a), b), fold_hash(fold_hash(0, b), a));
    }

    #[test]
    fn downsampling_averages_in_linear_light() {
        // 2x2 block of black and white pixels: linear average of 0 and 1 is 0.5 linear = code 188.
        let mut rgba = Vec::new();
        for value in [0u8, 255, 255, 0] {
            rgba.extend_from_slice(&[value, value, value, 255]);
        }
        let out = downsample(&rgba, 1, 1, 2);
        assert_eq!(out, [188, 188, 188, 255]);
        // Uniform input is returned unchanged (every code survives the round trip).
        for code in [0u8, 1, 17, 128, 254, 255] {
            let block: Vec<u8> = (0..4).flat_map(|_| [code, code, code, 200]).collect();
            assert_eq!(
                downsample(&block, 1, 1, 2),
                [code, code, code, 200],
                "code {code}"
            );
        }
        assert_eq!(downsample(&rgba, 2, 2, 1), rgba, "factor 1 is a copy");
    }

    #[test]
    fn every_numeric_parameter_rejects_wrong_types_and_ranges() {
        let base = |extra: Value| {
            let mut v = json!({ "out": "/tmp/x", "frames": 4 });
            v.as_object_mut()
                .expect("json object")
                .extend(extra.as_object().expect("json object").clone());
            parse(v)
        };
        for (extra, why) in [
            (json!({ "width": "100" }), "string width"),
            (json!({ "height": -1 }), "negative height"),
            (json!({ "height": 0 }), "zero height"),
            (json!({ "width": 1.5 }), "fractional width"),
            (json!({ "fps": 0 }), "zero fps"),
            (json!({ "fps": 1_000_001 }), "fps above 1e6"),
            (json!({ "fps": 2000 }), "fps above 1000 with den 1"),
            (json!({ "fps": 1001 }), "fps 1001 is above 1000"),
            (json!({ "fps": 30000, "fps_den": 1 }), "30000/1 is above 1000 fps"),
            (json!({ "fps_den": 0 }), "zero fps_den"),
            (json!({ "fps_den": 100_001 }), "fps_den above 1e5"),
            (json!({ "supersample": 0 }), "zero supersample"),
            (json!({ "frames": 0 }), "zero frames"),
            (json!({ "frames": 1_000_001 }), "frames above MAX_FRAMES"),
            (json!({ "frames": -2 }), "negative frames"),
            (json!({ "frames": "4" }), "string frames"),
            (json!({ "warmup_frames": 10_001 }), "warmup above max"),
            (json!({ "warmup_frames": -1 }), "negative warmup"),
            (json!({ "settle_frames": 1 }), "settle without warm-up"),
            (
                json!({ "warmup_frames": 8, "settle_frames": 9 }),
                "settle above warmup",
            ),
            (
                json!({ "warmup_frames": 8, "settle_frames": -1 }),
                "negative settle",
            ),
            (
                json!({ "warmup_frames": 8, "settle_frames": 1.5 }),
                "fractional settle",
            ),
            (json!({ "hud": "yes" }), "non-boolean hud"),
            (json!({ "calibration": "gamma" }), "unknown calibration"),
            (json!({ "cuts": [-1] }), "negative cut"),
            (json!({ "cuts": [1.5] }), "fractional cut"),
            (json!({ "cuts": ["a"] }), "string cut"),
            (json!({ "name": "" }), "empty name"),
            (json!({ "name": "a/b" }), "slash in name"),
            (json!({ "name": "a b" }), "space in name"),
            (json!({ "name": "x".repeat(65) }), "name too long"),
            (json!({ "out": "" }), "empty out"),
            (json!({ "out": 5 }), "numeric out"),
        ] {
            let err = base(extra).expect_err(why);
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{why}");
            if why == "fps 1001 is above 1000" || why == "30000/1 is above 1000 fps" {
                assert!(
                    err.message.contains("the frame rate is above 1000 fps"),
                    "{why}: {}",
                    err.message
                );
            }
        }
        for (extra, why) in [
            (json!({ "frames": 1_000_000 }), "frames at MAX_FRAMES"),
            (json!({ "warmup_frames": 10_000 }), "warmup at max"),
            (
                json!({ "warmup_frames": 10_000, "settle_frames": 10_000 }),
                "settle at max",
            ),
            (json!({ "fps": 1000 }), "fps exactly 1000"),
            (
                json!({ "fps": 30000, "fps_den": 1001 }),
                "29.97 is under 1000 fps",
            ),
            (json!({ "name": "x".repeat(64) }), "name at 64"),
            (json!({ "name": "a.b-c_d" }), "name charset"),
            (json!({ "cuts": [] }), "empty cuts"),
            (json!({ "cuts": null, "hud": null }), "nulls use defaults"),
            (json!({ "calibration": "applied" }), "applied calibration"),
            (json!({ "layout": "portrait" }), "portrait"),
            (json!({ "format": "dnxhr" }), "dnxhr"),
        ] {
            base(extra).unwrap_or_else(|e| panic!("{why}: {e:?}"));
        }
    }

    #[test]
    fn duration_s_edge_cases() {
        let d = |v: Value| parse(json!({ "out": "/o", "fps": 30, "duration_s": v }));
        assert_eq!(d(json!(1.0)).expect("1 s").frames, 30);
        assert!(d(json!(0.0)).is_err(), "zero");
        assert!(d(json!(-1.0)).is_err(), "negative");
        assert!(d(json!("2")).is_err(), "string");
        assert!(
            d(json!(1e300)).is_err(),
            "absurd duration saturates and is rejected"
        );
        assert!(d(json!(0.001)).is_err(), "rounds to zero frames");
        // `frames` wins over `duration_s`; a null `frames` falls back to the duration.
        let both = parse(json!({ "out": "/o", "frames": 3, "duration_s": 10.0 })).expect("both");
        assert_eq!(both.frames, 3);
        let fallback = parse(json!({ "out": "/o", "frames": null, "duration_s": 1.0 })).expect("null frames");
        assert_eq!(fallback.frames, 30);
        assert!(parse(json!({ "out": "/o", "frames": null, "duration_s": null })).is_err());
    }

    #[test]
    fn parsing_is_pure_and_deterministic() {
        let params = json!({ "out": "/o", "frames": 90, "cuts": [30, 10, 60], "fps": 24, "supersample": 2 });
        assert_eq!(parse(params.clone()).expect("a"), parse(params).expect("b"));
    }

    #[test]
    fn step_is_never_zero_and_never_overflows_at_the_limits() {
        for (num, den) in [
            (1u64, 1u64),
            (1000, 1),
            (1_000_000, 1000),
            (1, 1),
            (30_000, 1001),
            (1000, 100_000),
        ] {
            if let Ok(r) = parse(json!({ "out": "/o", "frames": 2, "fps": num, "fps_den": den })) {
                assert!(r.step() > Duration::ZERO, "{num}/{den}");
                assert!(r.frame_time(MAX_FRAMES) >= r.step());
            }
        }
        let r = parse(json!({ "out": "/o", "frames": 2 })).expect("request");
        assert_eq!(
            r.frame_time(u64::MAX),
            Duration::from_nanos(u64::MAX),
            "saturates"
        );
    }

    #[test]
    fn format_names_round_trip() {
        for name in ["png", "raw", "exr", "ffv1", "prores", "dnxhr"] {
            let format = Format::parse(name).expect(name);
            assert_eq!(format.name(), name);
        }
        assert!(Format::parse("PNG ").is_none());
        assert!(!Format::Png.needs_ffmpeg() && !Format::Raw.needs_ffmpeg());
        assert!(
            Format::Png.is_frame_files() && Format::Raw.is_frame_files() && !Format::Ffv1.is_frame_files()
        );
        assert!(Format::Ffv1.needs_ffmpeg() && Format::ProRes.needs_ffmpeg() && Format::DnxHr.needs_ffmpeg());
    }

    #[test]
    fn png_level_writers_and_raw_are_validated_and_default_to_fast_automatic() {
        let r = parse(json!({ "out": "/o", "frames": 4 })).expect("request");
        assert_eq!((r.png_level, r.writers), (PngLevel::Fast, 0));
        for (name, level) in [
            ("fast", PngLevel::Fast),
            ("balanced", PngLevel::Balanced),
            ("best", PngLevel::Best),
        ] {
            let r = parse(json!({ "out": "/o", "frames": 4, "png_compression": name })).expect(name);
            assert_eq!(r.png_level, level);
            assert_eq!(level.name(), name);
        }
        for bad in [
            json!({ "png_compression": "max" }),
            json!({ "png_compression": 3 }),
            json!({ "writers": 17 }),
            json!({ "writers": -1 }),
        ] {
            let mut params = json!({ "out": "/o", "frames": 4 });
            params
                .as_object_mut()
                .expect("object")
                .extend(bad.as_object().expect("object").clone());
            assert!(parse(params).is_err(), "{bad}");
        }
        let raw = parse(json!({ "out": "/o", "frames": 4, "format": "raw", "bit_depth": 16, "writers": 3 }))
            .expect("raw16");
        assert_eq!((raw.writers, raw.writer_threads()), (3, 3));
        assert_eq!(raw.frame_path(7), PathBuf::from("/o/frame_000007.raw"));
        assert!(parse(json!({ "out": "/o", "frames": 4, "format": "ffv1", "bit_depth": 16 })).is_err());
    }

    #[test]
    fn automatic_writers_are_half_the_cores_between_one_and_eight() {
        let counts: Vec<usize> = [1, 2, 3, 8, 16, 32, 64]
            .iter()
            .map(|c| default_writers(*c))
            .collect();
        assert_eq!(counts, [1, 1, 1, 4, 8, 8, 8]);
        assert_eq!(default_writers(0), 1);
    }

    #[test]
    fn the_shutter_and_the_bit_depth_default_to_the_classic_frame() {
        let r = parse(json!({ "out": "/o", "frames": 4 })).expect("request");
        assert_eq!(r.shutter, Shutter::OFF);
        assert_eq!(r.bit_depth, Depth::Eight);
        assert!(!r.accumulates(), "the classic path, byte for byte");
        let blur = parse(json!({ "out": "/o", "frames": 4, "subframes": 8 })).expect("blur");
        assert_eq!(blur.shutter, Shutter::new(8, 180.0).expect("shutter"));
        assert!(blur.accumulates());
        let wide = parse(json!({ "out": "/o", "frames": 4, "subframes": 4, "shutter_angle": 270.5 }))
            .expect("angle");
        assert_eq!(wide.shutter.angle_millideg, 270_500);
        let deep = parse(json!({ "out": "/o", "frames": 4, "bit_depth": 16 })).expect("deep");
        assert_eq!(deep.bit_depth, Depth::Sixteen);
        assert!(deep.accumulates());
        // A lone angle without subframes is accepted and changes nothing.
        let lone = parse(json!({ "out": "/o", "frames": 4, "shutter_angle": 90 })).expect("lone angle");
        assert!(!lone.accumulates());
    }

    #[test]
    fn the_shutter_and_the_bit_depth_are_validated() {
        let base = |extra: Value| {
            let mut v = json!({ "out": "/tmp/x", "frames": 4 });
            v.as_object_mut()
                .expect("json object")
                .extend(extra.as_object().expect("json object").clone());
            parse(v)
        };
        for (extra, why) in [
            (json!({ "subframes": 0 }), "zero subframes"),
            (json!({ "subframes": 65 }), "subframes above 64"),
            (json!({ "subframes": 2.5 }), "fractional subframes"),
            (json!({ "subframes": "8" }), "string subframes"),
            (json!({ "subframes": 4, "shutter_angle": 0 }), "closed shutter"),
            (json!({ "subframes": 4, "shutter_angle": -10 }), "negative angle"),
            (json!({ "subframes": 4, "shutter_angle": 361 }), "angle above 360"),
            (json!({ "subframes": 4, "shutter_angle": "180" }), "string angle"),
            (json!({ "bit_depth": 10 }), "10-bit"),
            (json!({ "bit_depth": 12 }), "12-bit"),
            (json!({ "bit_depth": 32 }), "32-bit"),
            (json!({ "bit_depth": "16" }), "string bit depth"),
            (json!({ "bit_depth": 16, "format": "ffv1" }), "16-bit movie"),
            (json!({ "bit_depth": 16, "format": "prores" }), "16-bit prores"),
            (
                json!({ "bit_depth": 16, "calibration": "applied" }),
                "16-bit with the 8-bit calibration",
            ),
        ] {
            let err = base(extra).expect_err(why);
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{why}");
        }
        for (extra, why) in [
            (json!({ "subframes": 64, "shutter_angle": 360 }), "the limits"),
            (json!({ "subframes": 1 }), "one subframe"),
            (
                json!({ "subframes": 2, "shutter_angle": 0.5 }),
                "a sliver of a shutter",
            ),
            (json!({ "bit_depth": 8, "format": "ffv1" }), "8-bit movie"),
            (
                json!({ "bit_depth": 16, "subframes": 8, "supersample": 2 }),
                "everything at once",
            ),
            (
                json!({ "subframes": null, "shutter_angle": null, "bit_depth": null }),
                "nulls use defaults",
            ),
        ] {
            base(extra).unwrap_or_else(|e| panic!("{why}: {e:?}"));
        }
    }

    #[test]
    fn the_time_map_scales_the_simulation_timeline_and_nothing_else() {
        let plain = parse(json!({ "out": "/o", "frames": 10, "fps": 25 })).expect("plain");
        assert!(plain.time_map.is_identity());
        assert_eq!(plain.sim_step_ns(3), 40_000_000);
        assert_eq!(plain.pre_sim_step_ns(), 40_000_000);
        assert_eq!(plain.peak_sim_step_ns(), 40_000_000);
        let slow = parse(json!({
            "out": "/o", "frames": 10, "fps": 25, "time_map": [{ "frame": 0, "scale": 0.5 }]
        }))
        .expect("slow");
        // The output timeline is unchanged; the simulation covers half of it.
        assert_eq!(slow.step(), plain.step());
        assert_eq!(slow.sim_step_ns(3), 20_000_000);
        assert_eq!(slow.pre_sim_step_ns(), 20_000_000);
        assert_eq!(slow.sim_time(4), Duration::from_millis(80));
        // The render clock (`game_time_s`) is the output's own: the map never moves it.
        assert_eq!(slow.frame_time(4), plain.frame_time(4));
        assert_eq!(plain.sim_time(7), plain.frame_time(7), "no map: one clock");
        // A 25 fps job at half speed walks the simulation clock as a 50 fps job does.
        let fifty = parse(json!({ "out": "/o", "frames": 10, "fps": 50 })).expect("fifty");
        for i in 0..10 {
            assert_eq!(slow.sim_time(i), fifty.sim_time(i), "frame {i}");
        }
        let blurred = parse(json!({
            "out": "/o", "frames": 10, "fps": 25, "subframes": 4,
            "time_map": [{ "frame": 0, "scale": 3 }]
        }))
        .expect("blurred");
        assert_eq!(
            blurred.peak_sim_step_ns(),
            240_000_000,
            "shutter doubles the bound"
        );
        assert_eq!(blurred.shutter.subframes, 4, "the shutter is its own parameter");
        for bad in [
            json!({ "time_map": { "frame": 0, "scale": 1 } }),
            json!({ "time_map": [{ "frame": 0, "scale": 101 }] }),
            json!({ "time_map": [{ "frame": 2_000_000, "scale": 1 }] }),
        ] {
            let mut params = json!({ "out": "/o", "frames": 4 });
            params
                .as_object_mut()
                .expect("object")
                .extend(bad.as_object().expect("object").clone());
            let err = parse(params).expect_err("bad time map");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        }
    }

    #[test]
    fn downsampling_keeps_the_layout() {
        // 4x2 source -> 2x1 at factor 2: left block red, right block blue.
        let mut rgba = Vec::new();
        for _row in 0..2 {
            for x in 0..4 {
                rgba.extend_from_slice(if x < 2 {
                    &[255, 0, 0, 255]
                } else {
                    &[0, 0, 255, 255]
                });
            }
        }
        assert_eq!(downsample(&rgba, 2, 1, 2), [255, 0, 0, 255, 0, 0, 255, 255]);
    }

    #[test]
    fn exr_frames_are_scene_linear_files_and_refuse_display_options() {
        let r = parse(json!({ "out": "/o", "frames": 2, "format": "exr", "name": "hdr" })).expect("exr");
        assert_eq!(r.format, Format::Exr);
        assert!(r.format.is_frame_files() && !r.format.needs_ffmpeg());
        assert_eq!(r.frame_path(3), std::path::Path::new("/o").join("hdr_000003.exr"));
        for bad in [
            json!({ "out": "/o", "frames": 2, "format": "exr", "bit_depth": 16 }),
            json!({ "out": "/o", "frames": 2, "format": "exr", "calibration": "applied" }),
        ] {
            assert_eq!(
                parse(bad).expect_err("refused").code,
                crate::protocol::INVALID_PARAMS
            );
        }
    }

    #[test]
    fn aovs_need_frame_files_and_name_every_file_of_a_frame() {
        let r = parse(json!({ "out": "/o", "frames": 2, "aovs": ["motion", "depth"], "name": "s" }))
            .expect("aovs");
        let dir = std::path::Path::new("/o");
        assert_eq!(
            r.frame_files(1),
            [
                dir.join("s_depth_000001.exr"),
                dir.join("s_motion_000001.exr"),
                dir.join("s_000001.png")
            ]
        );
        let raw =
            parse(json!({ "out": "/o", "frames": 2, "aovs": ["id"], "aov_format": "raw", "format": "raw" }))
                .expect("raw aovs");
        assert_eq!(
            raw.frame_files(0),
            [dir.join("frame_id_000000.raw"), dir.join("frame_000000.raw")]
        );
        assert_eq!(
            parse(json!({ "out": "/o", "frames": 2 }))
                .expect("none")
                .frame_files(0)
                .len(),
            1
        );
        for bad in [
            json!({ "out": "/o", "frames": 2, "aovs": ["depth"], "format": "ffv1" }),
            json!({ "out": "/o", "frames": 2, "aovs": ["albedo"] }),
            json!({ "out": "/o", "frames": 2, "aov_format": "exr" }),
        ] {
            assert_eq!(
                parse(bad).expect_err("refused").code,
                crate::protocol::INVALID_PARAMS
            );
        }
    }

    /// The `data.reason` of a refused request.
    #[cfg(feature = "media-encode")]
    fn reason(params: serde_json::Value) -> Option<String> {
        Request::parse(&params)
            .err()
            .and_then(|e| e.data)
            .and_then(|d| d.get("reason").and_then(|r| r.as_str().map(str::to_string)))
    }

    #[test]
    fn format_webm_parses_and_names_itself() {
        assert_eq!(Format::parse("webm"), Some(Format::Webm));
        assert_eq!(Format::parse("av1"), Some(Format::Webm));
        assert_eq!(Format::Webm.name(), "webm");
        assert_eq!(Format::Webm.extension(), "webm");
        assert!(Format::Webm.is_encoded() && !Format::Webm.needs_ffmpeg() && !Format::Webm.is_frame_files());
        assert!(!Format::Png.is_encoded() && !Format::Ffv1.is_encoded());
        // `encode` belongs to format webm only.
        let png = Request::parse(&serde_json::json!({ "out": "/tmp/x", "frames": 2, "encode": {} }));
        assert!(png.is_err_and(|e| e.message.contains("webm")));
    }

    #[cfg(not(feature = "media-encode"))]
    #[test]
    fn without_the_encoder_format_webm_is_media_encode_unavailable() {
        // This must also refuse when regia_media/media-encode is enabled independently:
        // the agent's writer is still a stub without regia_agent/media-encode.
        let error = Request::parse(&serde_json::json!({
            "out": "/tmp/x", "frames": 2, "format": "webm"
        }))
        .expect_err("the agent has no encoder writer");
        assert_eq!(error.code, crate::protocol::INVALID_PARAMS);
        assert_eq!(
            error.data.expect("encoder reason")["reason"],
            "media_encode_unavailable"
        );
    }

    #[cfg(feature = "media-encode")]
    #[test]
    fn with_the_encoder_format_webm_is_available() {
        assert!(regia_media::encode::available());
        let request = Request::parse(&serde_json::json!({
            "out": "/tmp/x", "frames": 2, "format": "webm"
        }))
        .expect("the linked media crate has an encoder");
        assert_eq!(request.format, Format::Webm);
        assert_eq!(request.encode, regia_media::encode::EncodeSettings::default());
        assert!(request.movie_path().ends_with("frame.webm"));
    }

    #[cfg(feature = "media-encode")]
    #[test]
    fn webm_requests_carry_their_settings_and_hostile_ones_are_refused() {
        let ok = Request::parse(&serde_json::json!({
            "out": "/tmp/x", "frames": 2, "format": "webm", "width": 64, "height": 32,
            "encode": { "quality": 40, "keyframe_interval": 12, "threads": 1 }
        }))
        .expect("webm request");
        assert_eq!(
            (ok.encode.quality, ok.encode.keyframe_interval, ok.encode.threads),
            (40, 12, 1)
        );
        assert!(ok.movie_path().ends_with("frame.webm"));
        let r = |extra: serde_json::Value| {
            let mut params = serde_json::json!({ "out": "/tmp/x", "frames": 2, "format": "webm", "width": 64, "height": 32 });
            params
                .as_object_mut()
                .expect("object")
                .extend(extra.as_object().expect("object").clone());
            reason(params)
        };
        assert_eq!(
            r(serde_json::json!({ "width": 63 })).as_deref(),
            Some("encode_odd_size")
        );
        assert_eq!(
            r(serde_json::json!({ "height": 8 })).as_deref(),
            Some("encode_bad_size")
        );
        assert_eq!(
            r(serde_json::json!({ "encode": { "keyframe_interval": 1_000_000 } })).as_deref(),
            Some("encode_bad_keyframe_interval")
        );
        assert_eq!(
            r(serde_json::json!({ "encode": { "speed": 99 } })).as_deref(),
            Some("encode_bad_speed")
        );
        assert_eq!(
            r(serde_json::json!({ "encode": { "nope": 1 } })).as_deref(),
            Some("encode_bad_input")
        );
        assert_eq!(
            r(serde_json::json!({ "fps": 500 })).as_deref(),
            Some("encode_bad_rate")
        );
        // The mixdown must be at a rate Opus takes; movies take no 16-bit frames or AOVs.
        let bad_rate = Request::parse(&serde_json::json!({
            "out": "/tmp/x", "frames": 2, "format": "webm", "width": 64, "height": 32, "audio": { "sample_rate": 44100 }
        }));
        assert!(bad_rate.is_err_and(|e| e.message.contains("Opus")));
        let deep = Request::parse(&serde_json::json!({
            "out": "/tmp/x", "frames": 2, "format": "webm", "width": 64, "height": 32, "bit_depth": 16
        }));
        assert!(deep.is_err());
    }
}
