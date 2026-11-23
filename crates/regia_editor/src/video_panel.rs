// SPDX-License-Identifier: MPL-2.0
//! The Video panel (`docs/specs/50-video-playback.md`, ADR-2720): opens a `.webm`/`.mkv` or `.video.ron`
//! asset, shows the decoded picture, plays, pauses, scrubs and steps single frames, lists the stream
//! (codec, resolution, frame rate, duration, audio tracks, caption and subtitle tracks with their cues),
//! reports the decode counters and the decode pool, and edits the boot movie list of `Regia.toml`.
//!
//! The panel drives a `regia_media::PlayerCore` on the `Lockstep` clock through
//! `regia_media::plugin::open_player_pooled`, the path a game's `VideoPlayer` takes, so what the panel
//! shows is what the runtime decodes. Pictures reach egui through the CPU frame path
//! (`VideoPlayerState::frame_rgba`, the reference YUV to RGBA8 conversion). Nothing here changes a file
//! format: the boot list is written back as the existing `[boot] movies` array of `Regia.toml`
//! (`regia_framework::boot_movies`), and the other keys of the manifest are kept.
//!
//! Agent surface: `editor.video_open`, `editor.video_seek`, `editor.video_state`
//! (`book/agents/video-panel.md`). Errors are `video_panel_*` codes: the panel's own
//! (`video_panel_no_project`, `video_panel_bad_path`, `video_panel_not_open`, `video_panel_bad_index`,
//! `video_panel_no_manifest`, `video_panel_manifest_invalid`, `video_panel_io_failed`) and the media
//! error codes prefixed (`video_panel_source_not_found`, `video_panel_container_invalid`, ...).

use std::path::Path;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_framework::boot_movies::BootConfig;
use regia_media::container::{MediaInfo, TrackKind};
use regia_media::player::ClockInput;
use regia_media::plugin::{MediaFiles, MediaSettings, VideoPlayer, VideoPlayerState, open_player_pooled};
use regia_media::{ClockMode, LoopMode, MediaError, PlayState, SeekMode, TextCue, VideoStats};
use serde_json::{Value, json};

use crate::data_panels::{PanelError, write_atomic};
use crate::viewport_agent::Handler;

const HINT: &str = "see book/agents/video-panel.md";

/// The longest single playback step, so a stalled UI frame does not jump the picture.
const MAX_STEP_NS: u64 = 100_000_000;

/// The file suffixes the panel opens (project-relative paths).
pub const VIDEO_SUFFIXES: &[&str] = &[".webm", ".mkv", ".video.ron"];

/// Whether `path` names an asset the Video panel opens.
#[must_use]
pub fn is_video_asset(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    VIDEO_SUFFIXES.iter().any(|s| lower.ends_with(s))
}

fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(&format!("video_panel_{code}"), message)
}

fn media_err(e: &MediaError) -> PanelError {
    err(e.code(), e.to_string())
}

// ---------------------------------------------------------------------------------------------
// Stream information
// ---------------------------------------------------------------------------------------------

/// One audio track of the open file.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioSummary {
    /// Index in the file's track list.
    pub index: u32,
    /// Matroska codec id (`A_OPUS`, ...).
    pub codec: String,
    /// Language tag.
    pub language: String,
    /// Channel count (0 when not declared).
    pub channels: u8,
    /// Sample rate in Hz (0 when not declared).
    pub sample_rate: u32,
    /// Flagged as the default track.
    pub default: bool,
}

/// One caption or subtitle track of the open file with the cues read so far.
#[derive(Clone, Debug, PartialEq)]
pub struct TextSummary {
    /// Index in the file's track list.
    pub index: u32,
    /// Matroska codec id (`S_TEXT/UTF8`, ...).
    pub codec: String,
    /// Language tag.
    pub language: String,
    /// Flagged as the default track.
    pub default: bool,
    /// The track's cues, in start order.
    pub cues: Vec<TextCue>,
}

/// What the Video panel shows about the open file.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamSummary {
    /// Container family (`webm`, `matroska`).
    pub container: String,
    /// Video codec id.
    pub codec: String,
    /// Picture width.
    pub width: u32,
    /// Picture height.
    pub height: u32,
    /// Nominal frame rate.
    pub fps: Option<f64>,
    /// Declared duration in seconds.
    pub duration_s: Option<f64>,
    /// Frames at the nominal rate (`None` without a duration or a constant rate).
    pub total_frames: Option<u64>,
    /// Audio tracks.
    pub audio: Vec<AudioSummary>,
    /// Caption and subtitle tracks.
    pub text: Vec<TextSummary>,
    /// Chapter markers `(name, seconds)`.
    pub markers: Vec<(String, f64)>,
}

impl StreamSummary {
    /// Reads the track list of `info`; `cues` are the text cues the session has demuxed so far.
    #[must_use]
    pub fn from_info(info: &MediaInfo, cues: &[TextCue]) -> Self {
        let video = info.video_track();
        let (width, height) = video
            .and_then(|t| t.video.as_ref())
            .map_or((0, 0), |v| (v.display_width, v.display_height));
        let total_frames = match (
            info.duration_ns.filter(|d| *d > 0),
            video.and_then(|t| t.default_duration_ns).filter(|d| *d > 0),
        ) {
            (Some(clip), Some(frame)) => Some(clip.div_ceil(frame)),
            _ => None,
        };
        Self {
            container: format!("{:?}", info.container).to_ascii_lowercase(),
            codec: video.map(|t| t.codec_id.clone()).unwrap_or_default(),
            width,
            height,
            fps: video.and_then(|t| t.frame_rate),
            duration_s: info.duration_ns.map(|d| d as f64 / 1e9),
            total_frames,
            audio: info
                .tracks
                .iter()
                .filter(|t| t.kind == TrackKind::Audio)
                .map(|t| AudioSummary {
                    index: t.index,
                    codec: t.codec_id.clone(),
                    language: t.language.clone(),
                    channels: t.audio.as_ref().map_or(0, |a| a.channels),
                    sample_rate: t.audio.as_ref().map_or(0, |a| a.sample_rate.round() as u32),
                    default: t.default,
                })
                .collect(),
            text: info
                .tracks
                .iter()
                .filter(|t| t.kind == TrackKind::Text)
                .map(|t| TextSummary {
                    index: t.index,
                    codec: t.codec_id.clone(),
                    language: t.language.clone(),
                    default: t.default,
                    cues: cues.iter().filter(|c| c.track == t.index).cloned().collect(),
                })
                .collect(),
            markers: info
                .markers
                .iter()
                .map(|m| (m.name.clone(), m.time_ns as f64 / 1e9))
                .collect(),
        }
    }

    /// As JSON (the `info` object of `editor.video_state`).
    #[must_use]
    pub fn json(&self) -> Value {
        json!({
            "container": self.container,
            "codec": self.codec,
            "width": self.width,
            "height": self.height,
            "fps": self.fps,
            "duration_s": self.duration_s,
            "total_frames": self.total_frames,
            "audio": self.audio.iter().map(|a| json!({
                "index": a.index, "codec": a.codec, "language": a.language,
                "channels": a.channels, "sample_rate": a.sample_rate, "default": a.default,
            })).collect::<Vec<_>>(),
            "text": self.text.iter().map(|t| json!({
                "index": t.index, "codec": t.codec, "language": t.language, "default": t.default,
                "cues": t.cues.iter().map(|c| json!({
                    "start_s": c.start_ns as f64 / 1e9,
                    "end_s": c.end_ns.map(|e| e as f64 / 1e9),
                    "text": c.text,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "markers": self.markers.iter().map(|(n, t)| json!({ "name": n, "time_s": t })).collect::<Vec<_>>(),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The boot movie list
// ---------------------------------------------------------------------------------------------

/// Where the `skip_video` binding comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipBinding {
    /// The project's `config/input.ron` binds the action.
    Project,
    /// Nothing binds it: `BootMoviesPlugin` adds Escape, Space, Enter and the gamepad South and Start.
    Default,
    /// `config/input.ron` exists but cannot be read, so the binding is unknown.
    Unreadable,
}

impl SkipBinding {
    /// The stable name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Default => "default",
            Self::Unreadable => "unreadable",
        }
    }

    /// Whether a press of a bound key will skip a boot movie.
    #[must_use]
    pub fn ok(self) -> bool {
        !matches!(self, Self::Unreadable)
    }

    /// Reads `root/config/input.ron`.
    #[must_use]
    pub fn check(root: &Path) -> Self {
        let path = root.join("config/input.ron");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::Default,
            Err(_) => return Self::Unreadable,
        };
        match regia_input::InputConfig::from_ron(&text) {
            Ok(config) => {
                let bound = config.contexts.values().any(|c| {
                    c.bindings
                        .iter()
                        .any(|b| b.action == regia_input::SKIP_VIDEO_ACTION)
                });
                if bound { Self::Project } else { Self::Default }
            }
            Err(_) => Self::Unreadable,
        }
    }
}

/// The result of checking one boot movie entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryCheck {
    /// The entry as written in `Regia.toml`.
    pub path: String,
    /// The media error code when the entry does not open (`source_not_found`, ...); `None` when it does.
    pub problem: Option<String>,
}

/// The `[boot]` table as the panel edits it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BootListModel {
    /// Movies in play order.
    pub movies: Vec<String>,
    /// Seconds before a movie may be skipped (`None` = the engine default).
    pub skippable_after: Option<f64>,
    saved: BootConfig,
}

impl BootListModel {
    /// Reads the `[boot]` table of the project's `Regia.toml` (an absent manifest or table is an empty list).
    ///
    /// # Errors
    /// `video_panel_manifest_invalid` when the manifest does not parse.
    pub fn load(root: &Path) -> Result<Self, PanelError> {
        let config = BootConfig::load(root).map_err(|e| err("manifest_invalid", e))?;
        Ok(Self {
            movies: config.movies.clone(),
            skippable_after: config.skippable_after,
            saved: config,
        })
    }

    /// Whether the list differs from what the manifest holds.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.movies != self.saved.movies || self.skippable_after != self.saved.skippable_after
    }

    /// Moves the entry at `from` to position `to`.
    ///
    /// # Errors
    /// `video_panel_bad_index` when either index is out of range.
    pub fn reorder(&mut self, from: usize, to: usize) -> Result<(), PanelError> {
        if from >= self.movies.len() || to >= self.movies.len() {
            return Err(err(
                "bad_index",
                format!("indices {from} -> {to} outside a list of {}", self.movies.len()),
            ));
        }
        let item = self.movies.remove(from);
        self.movies.insert(to, item);
        Ok(())
    }

    /// Appends `path` to the list.
    ///
    /// # Errors
    /// `video_panel_bad_path` when it is not a video asset path.
    pub fn add(&mut self, path: &str) -> Result<(), PanelError> {
        // The shared project-path rule: `..` and `.` drop, a drive prefix (`C:`), an absolute
        // path and a Windows device name (`CON.webm`) are refused, while a name that merely
        // contains `..` (`a..b.webm`) is a name.
        if !is_video_asset(path) || regia_core::project_path::check_relative(path).is_err() {
            return Err(err(
                "bad_path",
                format!("`{path}` must be a relative .webm, .mkv or .video.ron path inside the project"),
            ));
        }
        self.movies.push(path.to_owned());
        Ok(())
    }

    /// Removes the entry at `index`.
    ///
    /// # Errors
    /// `video_panel_bad_index` when out of range.
    pub fn remove(&mut self, index: usize) -> Result<(), PanelError> {
        if index >= self.movies.len() {
            return Err(err("bad_index", format!("no entry {index}")));
        }
        self.movies.remove(index);
        Ok(())
    }

    /// Opens every entry under `root` far enough to say whether it plays.
    #[must_use]
    pub fn check_entries(&self, root: &Path) -> Vec<EntryCheck> {
        let files = MediaFiles::with_root(root);
        self.movies
            .iter()
            .map(|path| {
                let problem = files
                    .resolve(path)
                    .and_then(|(file, _)| files.open(&file).map(|_| ()))
                    .err()
                    .map(|e| e.code().to_owned());
                EntryCheck {
                    path: path.clone(),
                    problem,
                }
            })
            .collect()
    }

    /// Writes the list into the `[boot]` table of `root`'s `Regia.toml`, keeping every other key. A list that
    /// did not change leaves the file's bytes alone. Returns whether the file was written.
    ///
    /// # Errors
    /// `video_panel_no_manifest`, `video_panel_manifest_invalid` or `video_panel_io_failed`.
    pub fn save(&mut self, root: &Path) -> Result<bool, PanelError> {
        if !self.dirty() {
            return Ok(false);
        }
        let path = regia_core::brand::project_manifest(root)
            .ok_or_else(|| err("no_manifest", "the project has no Regia.toml"))?;
        let text = std::fs::read_to_string(&path)
            .map_err(|e| err("io_failed", format!("{}: {e}", path.display())))?;
        let mut doc: toml::Table =
            toml::from_str(&text).map_err(|e| err("manifest_invalid", format!("{}: {e}", path.display())))?;
        let boot = doc
            .entry("boot")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let toml::Value::Table(boot) = boot else {
            return Err(err("manifest_invalid", "`boot` in Regia.toml is not a table"));
        };
        if self.movies.is_empty() {
            boot.remove("movies");
        } else {
            boot.insert(
                "movies".into(),
                toml::Value::Array(self.movies.iter().cloned().map(toml::Value::String).collect()),
            );
        }
        match self.skippable_after {
            Some(s) => {
                boot.insert("skippable_after".into(), toml::Value::Float(s));
            }
            None => {
                boot.remove("skippable_after");
            }
        }
        let out = toml::to_string_pretty(&doc).map_err(|e| err("manifest_invalid", e.to_string()))?;
        write_atomic(&path, &out, "video_panel")?;
        self.saved = BootConfig {
            movies: self.movies.clone(),
            skippable_after: self.skippable_after,
        };
        Ok(true)
    }

    /// As JSON (the `boot` object of `editor.video_state`).
    #[must_use]
    pub fn json(&self, root: Option<&Path>) -> Value {
        let checks = root.map(|r| self.check_entries(r));
        let skip = root.map(SkipBinding::check);
        json!({
            "movies": self.movies.iter().enumerate().map(|(i, m)| json!({
                "index": i,
                "path": m,
                "problem": checks.as_ref().and_then(|c| c.get(i)).and_then(|c| c.problem.clone()),
            })).collect::<Vec<_>>(),
            "skippable_after": self.skippable_after,
            "dirty": self.dirty(),
            "skip_binding": skip.map(SkipBinding::name),
            "skip_binding_ok": skip.map(SkipBinding::ok),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The panel
// ---------------------------------------------------------------------------------------------

/// Decode pool use: sessions holding a decoder against `MediaSettings::max_decoders`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolUse {
    /// Sessions holding a decoder, the panel's included.
    pub in_use: usize,
    /// The pool size.
    pub max: usize,
}

/// The Video panel's state (a world resource).
#[derive(Resource)]
pub struct VideoPanel {
    /// The path text box.
    pub path_input: String,
    state: VideoPlayerState,
    source: Option<String>,
    error: Option<PanelError>,
    generation: u64,
    others_decoding: usize,
    max_decoders: usize,
    texture: Option<(u64, u64, egui::TextureHandle)>,
    /// The boot movie list, once loaded.
    pub boot: Option<BootListModel>,
    /// The boot list entry typed into the add box.
    pub boot_input: String,
    boot_error: Option<PanelError>,
}

impl Default for VideoPanel {
    fn default() -> Self {
        Self {
            path_input: String::new(),
            state: VideoPlayerState::default(),
            source: None,
            error: None,
            generation: 0,
            others_decoding: 0,
            max_decoders: MediaSettings::default().max_decoders,
            texture: None,
            boot: None,
            boot_input: String::new(),
            boot_error: None,
        }
    }
}

impl VideoPanel {
    /// The source path of the open file.
    #[must_use]
    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// The error of the last open or transport command, until the next success.
    #[must_use]
    pub fn error(&self) -> Option<&PanelError> {
        self.error.as_ref()
    }

    /// The stream information of the open file, with the text cues demuxed so far.
    #[must_use]
    pub fn summary(&self) -> Option<StreamSummary> {
        let core = self.state.core();
        let cues = core
            .cue_store()
            .map(regia_media::CueStore::cues)
            .unwrap_or_default();
        core.info().map(|i| StreamSummary::from_info(i, &cues))
    }

    /// The player state (read the presented picture through `core().presented()`).
    #[must_use]
    pub fn state(&self) -> &VideoPlayerState {
        &self.state
    }

    /// Whether a file is open and playable.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.source.is_some() && self.error.is_none() && self.state.core().info().is_some()
    }

    /// The machine state.
    #[must_use]
    pub fn play_state(&self) -> PlayState {
        self.state.core().state()
    }

    /// Index of the picture on screen.
    #[must_use]
    pub fn frame_index(&self) -> Option<u64> {
        self.state.core().frame_index()
    }

    /// Frames at the nominal rate, when known.
    #[must_use]
    pub fn total_frames(&self) -> Option<u64> {
        self.state
            .core()
            .info()
            .and_then(|i| StreamSummary::from_info(i, &[]).total_frames)
    }

    /// The player counters.
    #[must_use]
    pub fn stats(&self) -> VideoStats {
        self.state.core().stats()
    }

    /// Decode pool use, this panel's session included.
    #[must_use]
    pub fn pool(&self) -> PoolUse {
        PoolUse {
            in_use: self.others_decoding + usize::from(self.state.core().holds_decoder()),
            max: self.max_decoders.max(1),
        }
    }

    /// Opens `source` (a project path) from `root`. `others` are the sessions elsewhere in the world:
    /// `(open players, players holding a decoder)`. An asset that cannot play leaves the panel in an error
    /// state ([`Self::error`]) and returns it.
    ///
    /// # Errors
    /// `video_panel_bad_path`, or the media error code prefixed `video_panel_`.
    pub fn open(
        &mut self,
        root: &Path,
        source: &str,
        settings: &MediaSettings,
        others: (usize, usize),
    ) -> Result<(), PanelError> {
        let _span = tracing::debug_span!("video_panel_open").entered();
        self.state.core_mut().close();
        self.state = VideoPlayerState::default();
        self.source = Some(source.to_owned());
        self.texture = None;
        self.generation += 1;
        self.others_decoding = others.1;
        self.max_decoders = settings.max_decoders;
        self.path_input = source.to_owned();
        // The shared project-path rule (ADR-7566): `Path::is_absolute` let `/etc/x.webm` and `\x.webm`
        // through on Windows, where joining them to the project root names a file on its drive.
        let confined = regia_core::project_path::check_relative(source).is_ok() && is_video_asset(source);
        if !confined {
            let e = err(
                "bad_path",
                format!("`{source}` must be a relative .webm, .mkv or .video.ron path inside the project"),
            );
            self.error = Some(e.clone());
            return Err(e);
        }
        let files = MediaFiles::with_root(root);
        let player = VideoPlayer {
            source: source.to_owned(),
            autoplay: false,
            loop_mode: LoopMode::Off,
            clock: ClockMode::Lockstep,
            ..VideoPlayer::default()
        };
        open_player_pooled(
            Entity::PLACEHOLDER,
            &player,
            &mut self.state,
            &files,
            settings,
            others,
            None,
        );
        let failure = {
            let core = self.state.core();
            (core.state() == PlayState::Failed || core.info().is_none()).then(|| {
                core.last_error()
                    .map_or_else(|| err("open_failed", format!("{source} did not open")), media_err)
            })
        };
        if let Some(e) = failure {
            self.error = Some(e.clone());
            return Err(e);
        }
        self.error = None;
        Ok(())
    }

    fn require_open(&self) -> Result<(), PanelError> {
        if self.is_open() {
            Ok(())
        } else {
            Err(self
                .error
                .clone()
                .unwrap_or_else(|| err("not_open", "no video is open; open one first")))
        }
    }

    /// A transport error is returned to the caller; only a failed open puts the panel in the error state.
    fn record<T>(&mut self, result: Result<T, MediaError>) -> Result<T, PanelError> {
        result.map_err(|e| media_err(&e))
    }

    /// Starts or resumes playback.
    ///
    /// # Errors
    /// `video_panel_not_open`, or the player's error (`video_panel_bad_state`).
    pub fn play(&mut self) -> Result<(), PanelError> {
        self.require_open()?;
        let r = self.state.core_mut().play();
        self.record(r)
    }

    /// Pauses playback.
    ///
    /// # Errors
    /// `video_panel_not_open`, or the player's error.
    pub fn pause(&mut self) -> Result<(), PanelError> {
        self.require_open()?;
        let r = self.state.core_mut().pause();
        self.record(r)
    }

    /// Advances playback by this frame's `dt_ns` (a UI frame while playing).
    ///
    /// The session stays on the lockstep clock, so a seek finishes inside the call. The advance
    /// itself is [`ClockInput::Elapsed`]: [`ClockInput::Step`] multiplies the step count by the
    /// delta of this call, and a variable UI frame would jump the playhead and then rewind it.
    /// `dt_ns` is clamped to 100 ms.
    pub fn tick(&mut self, dt_ns: u64) {
        let _span = tracing::debug_span!("video_panel_tick").entered();
        if self.is_open() {
            self.state.core_mut().tick(ClockInput::Elapsed {
                dt_ns: dt_ns.clamp(1, MAX_STEP_NS),
            });
        }
    }

    /// Seeks to frame `frame` (accurate), pausing there. A frame past the last one clamps to the last.
    /// Returns the frame index now shown.
    ///
    /// # Errors
    /// `video_panel_not_open`, or the player's error.
    pub fn seek_frame(&mut self, frame: u64) -> Result<u64, PanelError> {
        self.require_open()?;
        if matches!(self.play_state(), PlayState::Playing | PlayState::Buffering) {
            let r = self.state.core_mut().pause();
            self.record(r)?;
        }
        let frame = match self.total_frames() {
            Some(total) => frame.min(total.saturating_sub(1)),
            None => frame,
        };
        let r = self.state.core_mut().seek_frame(frame, SeekMode::Accurate);
        self.record(r)?;
        Ok(self.frame_index().unwrap_or(frame))
    }

    /// Seeks to `seconds` (negative clamps to 0, past the end to the last picture).
    ///
    /// # Errors
    /// `video_panel_not_open`, or the player's error.
    pub fn seek_time(&mut self, seconds: f64) -> Result<Option<u64>, PanelError> {
        self.require_open()?;
        let r = self.state.core_mut().seek_time(seconds, SeekMode::Accurate);
        self.record(r)?;
        Ok(self.frame_index())
    }

    /// Steps `delta` frames (negative steps back) and pauses. Stepping past either end stays on the end.
    ///
    /// # Errors
    /// `video_panel_not_open`, or the player's error.
    pub fn step(&mut self, delta: i64) -> Result<Option<u64>, PanelError> {
        self.require_open()?;
        let now = self.frame_index().unwrap_or(0);
        if delta >= 0 {
            if self.play_state() == PlayState::Playing {
                let r = self.state.core_mut().pause();
                self.record(r)?;
            }
            let room = self
                .total_frames()
                .map(|total| total.saturating_sub(now.saturating_add(1)));
            let n = u32::try_from(delta).unwrap_or(u32::MAX);
            let n = match room {
                Some(left) => n.min(u32::try_from(left).unwrap_or(u32::MAX)),
                None => n,
            };
            if n > 0 {
                let r = self.state.core_mut().step_frames(n);
                self.record(r)?;
            }
            if self.play_state() == PlayState::Ended {
                let hold = self.frame_index().unwrap_or(now);
                self.seek_frame(hold)?;
            }
        } else {
            self.seek_frame(now.saturating_sub(delta.unsigned_abs()))?;
        }
        Ok(self.frame_index())
    }

    /// Reads the boot movie list from the project.
    pub fn reload_boot(&mut self, root: &Path) {
        match BootListModel::load(root) {
            Ok(b) => {
                self.boot = Some(b);
                self.boot_error = None;
            }
            Err(e) => {
                self.boot = None;
                self.boot_error = Some(e);
            }
        }
    }

    fn texture_for(&mut self, ctx: &egui::Context) -> Option<egui::TextureHandle> {
        let _span = tracing::debug_span!("video_panel_frame").entered();
        let count = self.state.core().stats().frames_presented;
        if let Some((generation, seen, handle)) = &self.texture
            && *generation == self.generation
            && *seen == count
        {
            return Some(handle.clone());
        }
        let (w, h, rgba) = self.state.frame_rgba();
        let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
        let handle = ctx.load_texture("video-panel", image, egui::TextureOptions::LINEAR);
        self.texture = Some((self.generation, count, handle.clone()));
        Some(handle)
    }

    /// The `editor.video_state` answer.
    #[must_use]
    pub fn json(&self, root: Option<&Path>) -> Value {
        // Each accessor locks the core on its own: never hold a guard across one.
        let stats = self.stats();
        let pool = self.pool();
        let (name, frame, position, duration) = {
            let core = self.state.core();
            (
                core.state().name(),
                core.frame_index(),
                core.position(),
                core.duration(),
            )
        };
        json!({
            "open": self.is_open(),
            "source": self.source,
            "state": if self.error.is_some() { "error" } else { name },
            "error": self.error.as_ref().map(PanelError::json),
            "frame": frame,
            "total_frames": self.total_frames(),
            "position_s": position,
            "duration_s": duration,
            "info": self.summary().as_ref().map(StreamSummary::json),
            "stats": {
                "decode_ms": stats.decode_ms_ema,
                "frames_presented": stats.frames_presented,
                "frames_dropped": stats.frames_dropped,
                "frames_repeated": stats.frames_repeated,
                "frames_corrupt": stats.frames_corrupt,
                "packets_read": stats.packets_read,
                "queue_depth": stats.queue_depth,
                "decode_too_slow": stats.decode_too_slow,
            },
            "pool": { "in_use": pool.in_use, "max": pool.max },
            "boot": self.boot.as_ref().map(|b| b.json(root)),
            "boot_error": self.boot_error.as_ref().map(PanelError::json),
        })
    }
}

/// Sessions elsewhere in the world: `(open players, players holding a decoder)`.
fn others(world: &mut World) -> (usize, usize) {
    let mut open = 0;
    let mut decoding = 0;
    let mut query = world.query::<&VideoPlayerState>();
    for state in query.iter(world) {
        let core = state.core();
        if core.state() != PlayState::Closed {
            open += 1;
        }
        if core.holds_decoder() {
            decoding += 1;
        }
    }
    (open, decoding)
}

fn settings_of(world: &World) -> MediaSettings {
    world.get_resource::<MediaSettings>().cloned().unwrap_or_default()
}

/// Opens `path` in the panel (created on first use). The panel keeps the error state on failure.
///
/// # Errors
/// `video_panel_no_project`, or [`VideoPanel::open`]'s errors.
pub fn open_video(world: &mut World, path: &str) -> Result<(), PanelError> {
    let settings = settings_of(world);
    let counts = others(world);
    let root = crate::mesh_source::project_root(world);
    let mut panel = world.remove_resource::<VideoPanel>().unwrap_or_default();
    let result = match root {
        Some(root) => {
            if panel.boot.is_none() {
                panel.reload_boot(&root);
            }
            panel.open(&root, path, &settings, counts)
        }
        None => Err(err("no_project", "no project is open; open one first")),
    };
    world.insert_resource(panel);
    result
}

// ---------------------------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------------------------

fn clock_text(seconds: f64) -> String {
    let total = seconds.max(0.0);
    format!("{:02}:{:05.2}", (total / 60.0) as u64, total % 60.0)
}

/// Draws the panel.
pub fn video_panel(ui: &mut egui::Ui, world: &mut World) {
    let root = crate::mesh_source::project_root(world);
    let mut panel = world.remove_resource::<VideoPanel>().unwrap_or_default();
    let settings = settings_of(world);
    let counts = others(world);
    if panel.boot.is_none()
        && panel.boot_error.is_none()
        && let Some(root) = &root
    {
        panel.reload_boot(root);
    }
    let mut open: Option<String> = None;
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut panel.path_input).hint_text("movies/intro.webm"));
        if ui.button("Open").clicked() {
            open = Some(panel.path_input.clone());
        }
    });
    if let Some(path) = open {
        match &root {
            Some(root) => {
                let _ = panel.open(root, &path, &settings, counts);
            }
            None => panel.error = Some(err("no_project", "no project is open; open one first")),
        }
    }
    if let Some(e) = panel.error.clone() {
        e.draw(ui);
    }
    if panel.is_open() {
        transport_ui(ui, &mut panel);
        if let Some(tex) = panel.texture_for(ui.ctx()) {
            let size = tex.size_vec2();
            let avail = ui.available_width().clamp(64.0, 640.0);
            let scale = (avail / size.x.max(1.0)).min(1.0);
            ui.image(egui::load::SizedTexture::new(tex.id(), size * scale));
        }
        info_ui(ui, &panel);
        stats_ui(ui, &panel);
    }
    ui.separator();
    boot_ui(ui, &mut panel, root.as_deref());
    world.insert_resource(panel);
}

fn transport_ui(ui: &mut egui::Ui, panel: &mut VideoPanel) {
    let playing = panel.play_state() == PlayState::Playing;
    if playing {
        let dt = ui.ctx().input(|i| i.stable_dt);
        panel.tick((f64::from(dt) * 1e9) as u64);
        ui.ctx().request_repaint();
    }
    let total = panel.total_frames();
    let frame = panel.frame_index().unwrap_or(0);
    ui.horizontal(|ui| {
        if ui.button("⏮").on_hover_text("Previous frame").clicked() {
            let _ = panel.step(-1);
        }
        let label = if playing { "Pause" } else { "Play" };
        if ui.button(label).clicked() {
            let _ = if playing { panel.pause() } else { panel.play() };
        }
        if ui.button("⏭").on_hover_text("Next frame").clicked() {
            let _ = panel.step(1);
        }
        let mut scrub = frame;
        let last = total.map_or(frame, |t| t.saturating_sub(1)).max(1);
        if ui
            .add(egui::Slider::new(&mut scrub, 0..=last).text("frame"))
            .changed()
        {
            let _ = panel.seek_frame(scrub);
        }
        let position = panel.state.core().position();
        let duration = panel.state.core().duration();
        ui.monospace(format!(
            "{} / {}",
            clock_text(position),
            duration.map_or_else(|| "--".to_owned(), clock_text)
        ));
    });
}

fn info_ui(ui: &mut egui::Ui, panel: &VideoPanel) {
    let Some(s) = panel.summary() else {
        return;
    };
    egui::CollapsingHeader::new("Stream")
        .default_open(true)
        .show(ui, |ui| {
            ui.label(format!(
                "{} {}x{}{}{} ({})",
                s.codec,
                s.width,
                s.height,
                s.fps.map_or_else(String::new, |f| format!(" @ {f:.3} fps")),
                s.duration_s.map_or_else(String::new, |d| format!(", {d:.2} s")),
                s.container
            ));
            for a in &s.audio {
                ui.label(format!(
                    "audio {}: {} {} ch {} Hz [{}]{}",
                    a.index,
                    a.codec,
                    a.channels,
                    a.sample_rate,
                    a.language,
                    if a.default { " default" } else { "" }
                ));
            }
            for t in &s.text {
                egui::CollapsingHeader::new(format!(
                    "text {}: {} [{}] {} cues",
                    t.index,
                    t.codec,
                    t.language,
                    t.cues.len()
                ))
                .id_salt(("video-text", t.index))
                .show(ui, |ui| {
                    // Only the cues in view are laid out (ADR-7306): a film has thousands.
                    let row_height = ui.spacing().interact_size.y;
                    egui::ScrollArea::vertical()
                        .id_salt(("video-text-cues", t.index))
                        .max_height(240.0)
                        .show_rows(ui, row_height, t.cues.len(), |ui, range| {
                            for c in &t.cues[range] {
                                ui.add(
                                    egui::Label::new(format!(
                                        "{} {}",
                                        clock_text(c.start_ns as f64 / 1e9),
                                        c.text
                                    ))
                                    .truncate(),
                                );
                            }
                        });
                });
            }
        });
}

fn stats_ui(ui: &mut egui::Ui, panel: &VideoPanel) {
    let s = panel.stats();
    let pool = panel.pool();
    ui.label(format!(
        "decode {:.2} ms, presented {}, dropped {}, corrupt {}, decode pool {}/{}",
        s.decode_ms_ema, s.frames_presented, s.frames_dropped, s.frames_corrupt, pool.in_use, pool.max
    ));
}

fn boot_ui(ui: &mut egui::Ui, panel: &mut VideoPanel, root: Option<&Path>) {
    ui.strong("Boot movies (Regia.toml [boot])");
    if let Some(e) = panel.boot_error.clone() {
        e.draw(ui);
    }
    let checks = match (&panel.boot, root) {
        (Some(b), Some(r)) => b.check_entries(r),
        _ => Vec::new(),
    };
    let mut action: Option<(usize, usize)> = None;
    let mut remove: Option<usize> = None;
    let mut add: Option<String> = None;
    let mut save = false;
    let mut message: Option<PanelError> = None;
    if let Some(boot) = &panel.boot {
        for (i, movie) in boot.movies.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("{}.", i + 1));
                ui.label(movie);
                if let Some(code) = checks.get(i).and_then(|c| c.problem.as_ref()) {
                    ui.colored_label(egui::Color32::LIGHT_RED, "Does not pass the check")
                        .on_hover_text(format!("Agent error code: {code}"));
                }
                if ui.add_enabled(i > 0, egui::Button::new("Up")).clicked() {
                    action = Some((i, i - 1));
                }
                if ui
                    .add_enabled(i + 1 < boot.movies.len(), egui::Button::new("Down"))
                    .clicked()
                {
                    action = Some((i, i + 1));
                }
                if ui.small_button("Remove").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(r) = root {
            let skip = SkipBinding::check(r);
            let text = match skip {
                SkipBinding::Project => "skip_video: bound by config/input.ron",
                SkipBinding::Default => {
                    "skip_video: default keys (Escape, Space, Enter, gamepad South and Start)"
                }
                SkipBinding::Unreadable => "skip_video: config/input.ron cannot be read",
            };
            if skip.ok() {
                ui.weak(text);
            } else {
                ui.colored_label(egui::Color32::LIGHT_RED, text);
            }
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut panel.boot_input).hint_text("movies/studio.video.ron"));
            if ui.button("Add").clicked() && !panel.boot_input.trim().is_empty() {
                add = Some(panel.boot_input.trim().to_owned());
            }
            if ui.add_enabled(boot.dirty(), egui::Button::new("Save")).clicked() {
                save = true;
            }
        });
    } else if root.is_none() {
        ui.weak("No project is open.");
    }
    if let Some(boot) = panel.boot.as_mut() {
        if let Some((from, to)) = action
            && let Err(e) = boot.reorder(from, to)
        {
            message = Some(e);
        }
        if let Some(i) = remove
            && let Err(e) = boot.remove(i)
        {
            message = Some(e);
        }
        if let Some(path) = add {
            match boot.add(&path) {
                Ok(()) => panel.boot_input.clear(),
                Err(e) => message = Some(e),
            }
        }
        if save
            && let Some(r) = root
            && let Err(e) = boot.save(r)
        {
            message = Some(e);
        }
    }
    if message.is_some() {
        panel.boot_error = message;
    }
}

// ---------------------------------------------------------------------------------------------
// Agent surface
// ---------------------------------------------------------------------------------------------

fn state_json(world: &mut World) -> Value {
    let root = crate::mesh_source::project_root(world);
    match world.get_resource::<VideoPanel>() {
        Some(panel) => panel.json(root.as_deref()),
        None => VideoPanel::default().json(root.as_deref()),
    }
}

const BOOT_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "move",
        kind: "array",
        required: false,
        description: "[from, to]: move one entry (the panel's Up and Down).",
    },
    ParamSchema {
        name: "remove",
        kind: "number",
        required: false,
        description: "Remove the entry at this index.",
    },
    ParamSchema {
        name: "add",
        kind: "string",
        required: false,
        description: "Append this project-relative .webm, .mkv or .video.ron.",
    },
    ParamSchema {
        name: "save",
        kind: "boolean",
        required: false,
        description: "Write the list into Regia.toml (default false: edit the panel's list only).",
    },
];

const OPEN_PARAMS: &[ParamSchema] = &[ParamSchema {
    name: "path",
    kind: "string",
    required: true,
    description: "Project-relative .webm, .mkv or .video.ron to open in the Video panel.",
}];

const SEEK_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "frame",
        kind: "integer",
        required: false,
        description: "Seek to this frame (clamped to the last frame) and pause there.",
    },
    ParamSchema {
        name: "time",
        kind: "number",
        required: false,
        description: "Seek to this time in seconds (negative = 0, past the end = the last picture).",
    },
    ParamSchema {
        name: "step",
        kind: "integer",
        required: false,
        description: "Step this many frames (negative steps back) and pause.",
    },
    ParamSchema {
        name: "transport",
        kind: "string",
        required: false,
        description: "play or pause, applied after the seek.",
    },
    ParamSchema {
        name: "ticks",
        kind: "integer",
        required: false,
        description: "Advance a playing video by this many 1/60 s lockstep steps (0..=3600).",
    },
];

const STATE: &str = "{ open, source, state, error?: { code, message }, frame, total_frames, position_s, duration_s, \
    info: { container, codec, width, height, fps, duration_s, total_frames, audio: [{ index, codec, language, \
    channels, sample_rate, default }], text: [{ index, codec, language, default, cues: [{ start_s, end_s, text }] }], \
    markers }, stats: { decode_ms, frames_presented, frames_dropped, frames_repeated, frames_corrupt, packets_read, \
    queue_depth, decode_too_slow }, pool: { in_use, max }, boot: { movies: [{ index, path, problem }], \
    skippable_after, dirty, skip_binding, skip_binding_ok } }";

fn video_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`path` (string) is required"))?;
    open_video(world, path).map_err(|e| e.rpc(HINT))?;
    Ok(state_json(world))
}

fn video_seek(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let frame = params.get("frame").and_then(Value::as_u64);
    let time = params.get("time").and_then(Value::as_f64);
    let step = params.get("step").and_then(Value::as_i64);
    let transport = params.get("transport").and_then(Value::as_str);
    let ticks = params.get("ticks").and_then(Value::as_u64);
    if frame.is_none() && time.is_none() && step.is_none() && transport.is_none() && ticks.is_none() {
        return Err(RpcError::invalid_params(
            "give one of `frame`, `time`, `step`, `transport` or `ticks`",
        ));
    }
    if [frame.is_some(), time.is_some(), step.is_some()]
        .iter()
        .filter(|b| **b)
        .count()
        > 1
    {
        return Err(RpcError::invalid_params(
            "`frame`, `time` and `step` are exclusive",
        ));
    }
    if !matches!(transport, None | Some("play" | "pause")) {
        return Err(RpcError::invalid_params("`transport` is `play` or `pause`"));
    }
    if ticks.is_some_and(|t| t > 3600) {
        return Err(RpcError::invalid_params("`ticks` is 0..=3600"));
    }
    let mut panel = world.remove_resource::<VideoPanel>().unwrap_or_default();
    let result = (|| -> Result<(), PanelError> {
        if let Some(f) = frame {
            panel.seek_frame(f)?;
        } else if let Some(t) = time {
            panel.seek_time(t)?;
        } else if let Some(s) = step {
            panel.step(s)?;
        }
        match transport {
            Some("play") => panel.play()?,
            Some("pause") => panel.pause()?,
            _ => {}
        }
        for _ in 0..ticks.unwrap_or(0) {
            panel.tick(16_666_667);
        }
        Ok(())
    })();
    world.insert_resource(panel);
    result.map_err(|e| e.rpc(HINT))?;
    Ok(state_json(world))
}

fn video_state(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(state_json(world))
}

/// `editor.video_boot { move?, remove?, add?, save? }`: the panel's boot list Up / Down,
/// Remove, Add and Save (ADR-7307), applied in that order to the project's list.
fn video_boot(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = crate::mesh_source::project_root(world)
        .ok_or_else(|| err("no_manifest", "no project is open").rpc(HINT))?;
    let index = |v: &Value, what: &str| {
        v.as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| err("bad_index", format!("`{what}` must be a list index")).rpc(HINT))
    };
    let mv = match params.get("move").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => {
            let pair = v
                .as_array()
                .filter(|a| a.len() == 2)
                .ok_or_else(|| err("bad_index", "`move` must be [from, to]").rpc(HINT))?;
            Some((index(&pair[0], "move")?, index(&pair[1], "move")?))
        }
    };
    let remove = match params.get("remove").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(index(v, "remove")?),
    };
    let add = match params.get("add").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| err("bad_path", "`add` must be a path").rpc(HINT))?
                .to_owned(),
        ),
    };
    let save = params.get("save").and_then(Value::as_bool).unwrap_or(false);
    let mut panel = world.remove_resource::<VideoPanel>().unwrap_or_default();
    if panel.boot.is_none() {
        panel.reload_boot(&root);
    }
    let result = (|| {
        let boot = panel
            .boot
            .as_mut()
            .ok_or_else(|| err("manifest_invalid", "the boot list could not be read"))?;
        if let Some((from, to)) = mv {
            boot.reorder(from, to)?;
        }
        if let Some(i) = remove {
            boot.remove(i)?;
        }
        if let Some(path) = &add {
            boot.add(path)?;
        }
        if save {
            boot.save(&root)?;
        }
        Ok::<(), PanelError>(())
    })();
    world.insert_resource(panel);
    result.map_err(|e| e.rpc(HINT))?;
    Ok(state_json(world))
}

/// The Video panel's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.video_open",
                tool: "video_open",
                description: "Open a video asset (.webm, .mkv, .video.ron) in the Video panel and answer its state: \
                    stream info, decode counters, pool use and the boot movie list. A missing or corrupt file leaves \
                    the panel in an error state and fails with a video_panel_* code.",
                params: OPEN_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::Assets,
            },
            video_open as Handler,
        ),
        (
            MethodSchema {
                name: "editor.video_seek",
                tool: "video_seek",
                description: "Move the Video panel's playhead: seek to a frame or time, step frames, or play, pause \
                    and advance a playing video by lockstep ticks. Answers the state.",
                params: SEEK_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::Assets,
            },
            video_seek as Handler,
        ),
        (
            MethodSchema {
                name: "editor.video_boot",
                tool: "video_boot",
                description: "Edit the boot movie list as the Video panel does (ADR-7307): `move` [from, to], \
                    `remove` an index, `add` a project-relative .webm, .mkv or .video.ron, then `save` it into \
                    the `[boot] movies` array of Regia.toml (other keys kept). Applied in that order; answers \
                    the state. Errors video_panel_bad_index, video_panel_bad_path, video_panel_no_manifest, \
                    video_panel_manifest_invalid, video_panel_io_failed.",
                params: BOOT_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::Assets,
            },
            video_boot as Handler,
        ),
        (
            MethodSchema {
                name: "editor.video_state",
                tool: "video_state",
                description: "The Video panel: the open file, play state, frame, stream info, decode stats, decode \
                    pool use and the boot movie list with its skip_video binding check.",
                params: &[],
                result: STATE,
                mutating: false,
                permission: Permission::Read,
            },
            video_state as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;
