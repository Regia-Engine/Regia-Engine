// SPDX-License-Identifier: MPL-2.0
//! Conversations (dialogue) for the caption presenter
//! (`docs/specs/56-dialogue-and-subtitles.md`, ADR-1697).
//!
//! - [`asset`]: the `.conv.ron` asset, `speakers.ron`, `conversation.ron`, `cue_events.ron`.
//! - [`voice`]: clip-per-locale resolution and line durations (pure).
//! - [`runner`]: the step interpreter ([`ConversationRunner`]), conditions and choices.
//! - [`validate`](mod@validate): the `conv_*` validator shared by `regia loc validate` and
//!   `conversation.validate`.
//! - [`ConversationPlugin`]: runs the runner after the fixed step and turns its outputs into
//!   `CaptionStart` / `CaptionStop` and the `Conversation*` messages.
//!
//! Layering: `regia_audio` and `regia_ui` do not name each other; this module (layer 3) is where
//! text and audio meet. Voice playback goes through the [`VoicePlayer`] trait so the runner runs
//! headless with [`FrameVoicePlayer`], whose clock is a frame counter.

pub mod asset;
pub mod export;
pub mod gather;
pub mod runner;
pub mod ui_bridge;
pub mod validate;
pub mod voice;
// The cooked `.convc` (spec 56 § 4.3, ADR-1882).
pub mod cooked;

#[cfg(test)]
use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_app::caption::{CaptionIds, CaptionStart, CaptionStop};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_platform::{Vfs, VfsPath};
use regia_ui::captions::{CaptionBoard, CaptionClock, CaptionDeviceClock, CueEventMap, Rgb, SpeakerStyle};
use regia_ui::localization::{Localization, fallback_chain};

pub use asset::{
    CONVERSATION_FORMAT_VERSION, CaptionFields, ChoiceOption, ClipPolicy, ConversationAsset,
    ConversationLine, ConversationSettings, DurationSource, Speaker, SpeakerSet, Step, SubCue, VoiceFallback,
    VoiceSpec, conversation_migration_sources, conversation_schema_fingerprint,
    cue_events_schema_fingerprint, parse_conversation, parse_cue_events, parse_settings, parse_speakers,
    settings_schema_fingerprint, speakers_schema_fingerprint,
};
pub use runner::{
    BusyPolicy, ChoiceView, ConditionSource, ConversationRunner, FinishReason, FlagSet, PlayRequest,
    PlayStarted, RunId, RunInfo, RunnerEnv, RunnerOutput, SayRequest, VoiceStart,
};
pub use validate::{Issue, Report, Severity, ValidateEnv, ValidateInput, validate};
pub use voice::{DurationBasis, VoiceContext, VoiceResolution, plan_duration, resolve_voice};

/// Errors of the conversation system. `code()` is the agent error code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConversationError {
    /// No conversation (or line) with that id.
    #[error("conversation `{0}` not found")]
    NotFound(String),
    /// The locale is not loaded.
    #[error("locale `{0}` is not loaded")]
    LocaleNotLoaded(String),
    /// A label does not exist.
    #[error("label `{0}` not found")]
    LabelNotFound(String),
    /// The channel already has a run.
    #[error("channel `{0}` is busy")]
    ChannelBusy(String),
    /// A run took too many non-blocking steps in one frame.
    #[error("the run took more non-blocking steps than max_steps_per_frame in one frame")]
    StepBudget,
    /// `choose` on a run that is not waiting for a choice.
    #[error("the run is not waiting for a choice")]
    NoChoicePending,
    /// The choice index is not one of the offered options.
    #[error("the choice index is out of range")]
    ChoiceOutOfRange,
    /// The text of `say` is longer than 512 graphemes.
    #[error("text is longer than 512 graphemes")]
    TextTooLong,
    /// A file could not be parsed or loaded (`path:line:col: message`).
    #[error("{0}")]
    Asset(String),
    /// No run with that id.
    #[error("run {0} not found")]
    RunNotFound(u64),
    /// No speaker with that id.
    #[error("speaker `{0}` not found")]
    SpeakerNotFound(String),
    /// No line with that reference.
    #[error("line `{0}` not found")]
    LineNotFound(String),
    /// A parameter is missing or malformed.
    #[error("{0}")]
    InvalidParams(String),
}

impl ConversationError {
    /// The lowercase error code of the agent protocol.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "conversation_not_found",
            Self::LocaleNotLoaded(_) => "locale_not_loaded",
            Self::LabelNotFound(_) => "label_not_found",
            Self::ChannelBusy(_) => "channel_busy",
            Self::StepBudget => "step_budget",
            Self::NoChoicePending => "no_choice_pending",
            Self::ChoiceOutOfRange => "choice_out_of_range",
            Self::TextTooLong => "text_too_long",
            Self::Asset(_) => "asset_error",
            Self::RunNotFound(_) => "run_not_found",
            Self::SpeakerNotFound(_) => "speaker_not_found",
            Self::LineNotFound(_) => "line_not_found",
            Self::InvalidParams(_) => "invalid_params",
        }
    }
}

/// The audio file extensions the project scan treats as voice clips.
pub const CLIP_EXTENSIONS: [&str; 4] = ["ogg", "wav", "flac", "mp3"];

/// Everything loaded from `assets/conversations/` plus the clips found under `assets/`.
#[derive(Resource, Debug, Default, Clone)]
pub struct ConversationLibrary {
    /// The conversations by id.
    pub conversations: BTreeMap<String, ConversationAsset>,
    /// The speakers.
    pub speakers: SpeakerSet,
    /// Project settings.
    pub settings: ConversationSettings,
    /// Clip asset path (relative to `assets/`) to its length in microseconds (`None`: the header
    /// could not be read).
    pub clips: BTreeMap<String, Option<u64>>,
    /// Cue event name to caption (`cue_events.ron`).
    pub cue_events: BTreeMap<String, regia_ui::captions::CueEventDef>,
}

/// The length of a clip file in microseconds, read from the container header.
pub fn clip_duration_us(path: &Path) -> Option<u64> {
    let asset = regia_audio::SoundAsset::from_file_streaming(path.to_string_lossy().into_owned()).ok()?;
    let secs = asset.duration_secs();
    (secs.is_finite() && secs >= 0.0).then(|| (secs * 1_000_000.0).round() as u64)
}

fn read(path: &Path) -> Result<Option<String>, ConversationError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ConversationError::Asset(format!("{}: {e}", path.display()))),
    }
}

fn collect_clips(dir: &Path, root: &Path, out: &mut BTreeMap<String, Option<u64>>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_clips(&path, root, out);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| CLIP_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
            && let Ok(relative) = path.strip_prefix(root)
        {
            let key = relative.to_string_lossy().replace('\\', "/");
            out.insert(key, clip_duration_us(&path));
        }
    }
}

impl ConversationLibrary {
    /// Loads a project's `assets/` directory: every `*.conv.ron`, `speakers.ron`, `conversation.ron`
    /// under `conversations/`, and the audio clips anywhere under `assets/`. Missing files give
    /// defaults; a malformed file is an error naming `path:line:col`.
    pub fn load_assets(assets: &Path) -> Result<Self, ConversationError> {
        let dir = assets.join(asset::CONVERSATIONS_DIR);
        let mut library = Self::default();
        if let Some(text) = read(&dir.join(asset::SPEAKERS_FILE))? {
            library.speakers = parse_speakers(asset::SPEAKERS_FILE, &text)?;
        }
        if let Some(text) = read(&dir.join(asset::SETTINGS_FILE))? {
            library.settings = parse_settings(asset::SETTINGS_FILE, &text)?;
        }
        if let Some(text) = read(&dir.join(asset::CUE_EVENTS_FILE))? {
            library.cue_events = parse_cue_events(asset::CUE_EVENTS_FILE, &text)?.events;
        }
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut paths: Vec<_> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.ends_with(".conv.ron"))
                })
                .collect();
            paths.sort();
            for path in paths {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if let Some(text) = read(&path)? {
                    let conv = parse_conversation(&name, &text)?;
                    library.conversations.insert(conv.id.clone(), conv);
                }
            }
        }
        collect_clips(assets, assets, &mut library.clips);
        Ok(library)
    }

    /// Loads `assets://conversations` through the VFS, for a mount that is no plain directory (the
    /// Android APK): the same files as [`Self::load_assets`] plus the cooked `<id>.convc`, whose
    /// tables name the clips. A missing directory gives defaults.
    pub fn load_vfs(vfs: &Vfs) -> Result<Self, ConversationError> {
        let read = |name: &str| {
            VfsPath::parse(&format!("assets://{}/{name}", asset::CONVERSATIONS_DIR))
                .ok()
                .and_then(|path| vfs.read_sync(&path).ok())
        };
        let text = |name: &str| read(name).map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
        let mut library = Self::default();
        if let Some(text) = text(asset::SPEAKERS_FILE) {
            library.speakers = parse_speakers(asset::SPEAKERS_FILE, &text)?;
        }
        if let Some(text) = text(asset::SETTINGS_FILE) {
            library.settings = parse_settings(asset::SETTINGS_FILE, &text)?;
        }
        if let Some(text) = text(asset::CUE_EVENTS_FILE) {
            library.cue_events = parse_cue_events(asset::CUE_EVENTS_FILE, &text)?.events;
        }
        let entries = VfsPath::parse(&format!("assets://{}", asset::CONVERSATIONS_DIR))
            .ok()
            .and_then(|dir| vfs.list_dir(&dir).ok())
            .unwrap_or_default();
        for entry in &entries {
            if entry.name.ends_with(".conv.ron")
                && let Some(text) = text(&entry.name)
            {
                let conv = parse_conversation(&entry.name, &text)?;
                library.conversations.insert(conv.id.clone(), conv);
            }
        }
        // Cooked files second: a source with the same id wins, as in `load_assets` + `load_cooked`.
        for entry in &entries {
            if entry.name.ends_with(&format!(".{}", cooked::CONVC_EXTENSION))
                && let Some(bytes) = read(&entry.name)
            {
                library.add_cooked(&bytes)?;
            }
        }
        Ok(library)
    }
}

/// The `assets://` root on the host file system: the assets beside a packaged game's executable or a
/// dev project's `assets/`. `None` when it is no plain directory (the Android APK) or without a VFS.
pub(crate) fn assets_host_root(vfs: Option<&Vfs>) -> Option<PathBuf> {
    vfs?.host_path(&VfsPath::parse("assets://").ok()?)
}

#[cfg(test)]
thread_local! {
    static VOICE_DEBUG_POLLS: Cell<u64> = const { Cell::new(0) };
}

/// How many times dialogue code formatted a playback state on this thread.
///
/// The steady caption path uses `voice_playing` and does not increment this.
#[cfg(test)]
pub(crate) fn voice_debug_polls() -> u64 {
    VOICE_DEBUG_POLLS.with(|count| count.get())
}

/// Plays voice clips for the runner.
pub trait VoicePlayer: Send + Sync {
    /// Starts a clip on the Voice bus.
    fn play(&mut self, clip: &str, duration_us: Option<u64>) -> Option<VoiceStart>;
    /// Starts a clip at a world position. A player without spatial output plays it flat.
    fn play_at(&mut self, clip: &str, duration_us: Option<u64>, position: Vec3) -> Option<VoiceStart> {
        let _ = position;
        self.play(clip, duration_us)
    }
    /// Whether a started voice is still playing.
    fn playing(&self, handle: u64) -> bool;
    /// Stops a voice with a fade.
    fn stop(&mut self, handle: u64, fade_ms: u32);
    /// The caption clock now in microseconds, when this player presents on a clock other than
    /// the runner's (the engine audio's sample clock). The runner stamps an unvoiced line's
    /// caption with it so the caption board shows the line for its planned length (corr-framework-a
    /// F4). The default, `None`, stamps with the runner's clock.
    fn caption_now_us(&self) -> Option<u64> {
        None
    }
}

/// A voice player that plays nothing: lines run on their planned length.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullVoicePlayer;

impl VoicePlayer for NullVoicePlayer {
    fn play(&mut self, _clip: &str, _duration_us: Option<u64>) -> Option<VoiceStart> {
        None
    }
    fn playing(&self, _handle: u64) -> bool {
        false
    }
    fn stop(&mut self, _handle: u64, _fade_ms: u32) {}
}

/// A deterministic voice player whose clock is a frame counter (`frames * 1_000_000 /
/// sample_rate`, integer, rounded down). Headless tests and offline runs use it.
#[derive(Debug, Clone)]
pub struct FrameVoicePlayer {
    /// Frames rendered so far.
    pub frames: u64,
    /// Sample rate in Hz.
    pub sample_rate: u64,
    next: u64,
    ends: BTreeMap<u64, u64>,
    /// Clips played, in order (handle, path).
    pub played: Vec<(u64, String)>,
    /// The positions of spatial plays, in order (handle, position).
    pub positions: Vec<(u64, Vec3)>,
}

impl FrameVoicePlayer {
    /// A player at frame 0.
    pub fn new(sample_rate: u32) -> Self {
        Self {
            frames: 0,
            sample_rate: u64::from(sample_rate.max(1)),
            next: 0,
            ends: BTreeMap::new(),
            played: Vec::new(),
            positions: Vec::new(),
        }
    }

    /// The audio clock in microseconds.
    pub fn clock_us(&self) -> u64 {
        self.frames.saturating_mul(1_000_000) / self.sample_rate
    }
}

impl VoicePlayer for FrameVoicePlayer {
    fn play(&mut self, clip: &str, duration_us: Option<u64>) -> Option<VoiceStart> {
        self.next += 1;
        let handle = self.next;
        let length_frames = duration_us.map_or(0, |us| us.saturating_mul(self.sample_rate) / 1_000_000);
        self.ends
            .insert(handle, self.frames.saturating_add(length_frames));
        self.played.push((handle, clip.to_string()));
        Some(VoiceStart {
            handle,
            start_us: self.clock_us(),
            duration_us,
        })
    }
    fn play_at(&mut self, clip: &str, duration_us: Option<u64>, position: Vec3) -> Option<VoiceStart> {
        let start = self.play(clip, duration_us)?;
        self.positions.push((start.handle, position));
        Some(start)
    }
    fn playing(&self, handle: u64) -> bool {
        self.ends.get(&handle).is_some_and(|&end| self.frames < end)
    }
    fn stop(&mut self, handle: u64, _fade_ms: u32) {
        self.ends.remove(&handle);
    }
}

/// Plays clips on the `Voice` bus of the engine's [`regia_audio::Audio`] resource. Clips are read
/// from `assets_dir`, or through `vfs` when they are not there (the Android APK). `CaptionStart` is stamped with the output's sample clock
/// at the call, read through `device`, so text and sound begin on one clock sample; `clock_us` is
/// the fallback when the output has no clock yet.
pub struct AudioVoicePlayer<'a> {
    /// The engine audio.
    pub audio: &'a mut regia_audio::Audio,
    /// Where clip paths are resolved.
    pub assets_dir: &'a Path,
    /// The asset file system, for clips that are not plain files.
    pub vfs: Option<&'a Vfs>,
    /// The caption clock now, in microseconds.
    pub clock_us: u64,
    /// How the output's sample clock maps on to the caption clock.
    pub device: CaptionDeviceClock,
}

impl AudioVoicePlayer<'_> {
    fn load(&self, clip: &str) -> Result<regia_audio::SoundAsset, regia_audio::AudioError> {
        let path = self.assets_dir.join(clip);
        let packed = self.vfs.filter(|_| !path.is_file()).and_then(|vfs| {
            vfs.read_sync(&VfsPath::parse(&format!("assets://{clip}")).ok()?)
                .ok()
        });
        match packed {
            Some(bytes) => regia_audio::SoundAsset::streaming_from_bytes(Arc::new(bytes.to_vec()), clip),
            None => regia_audio::SoundAsset::from_file_streaming(path.to_string_lossy().into_owned()),
        }
    }

    fn start(&mut self, clip: &str, duration_us: Option<u64>, position: Option<Vec3>) -> Option<VoiceStart> {
        let asset = match self.load(clip) {
            Ok(asset) => asset,
            Err(error) => {
                tracing::warn!(%error, clip, "conversation: cannot load the voice clip");
                return None;
            }
        };
        let params = regia_audio::PlaybackParams::on_bus("Voice");
        let started = match position {
            None => self.audio.start_asset(&asset, params).map(Some),
            Some(at) => {
                let emitter = regia_audio::AudioEmitter::default();
                self.audio.start_asset_at(&asset, params, at, &emitter)
            }
        };
        match started {
            Ok(id) => Some(VoiceStart {
                handle: id?.0,
                start_us: self
                    .audio
                    .sample_clock()
                    .and_then(|clock| self.device.to_caption(clock.micros()))
                    .map_or(self.clock_us, |us| us.max(self.clock_us)),
                duration_us: duration_us.or_else(|| {
                    let secs = asset.duration_secs();
                    (secs.is_finite() && secs >= 0.0).then(|| (secs * 1_000_000.0).round() as u64)
                }),
            }),
            Err(error) => {
                tracing::warn!(%error, clip, "conversation: cannot start the voice clip");
                None
            }
        }
    }
}

impl VoicePlayer for AudioVoicePlayer<'_> {
    fn play(&mut self, clip: &str, duration_us: Option<u64>) -> Option<VoiceStart> {
        self.start(clip, duration_us, None)
    }
    fn play_at(&mut self, clip: &str, duration_us: Option<u64>, position: Vec3) -> Option<VoiceStart> {
        self.start(clip, duration_us, Some(position))
    }
    fn playing(&self, handle: u64) -> bool {
        self.audio.voice_playing(regia_audio::SoundInstanceId(handle))
    }
    fn caption_now_us(&self) -> Option<u64> {
        Some(self.clock_us)
    }
    fn stop(&mut self, handle: u64, fade_ms: u32) {
        self.audio.stop_voice(
            regia_audio::SoundInstanceId(handle),
            std::time::Duration::from_millis(u64::from(fade_ms)),
        );
    }
}

/// An override for the voice player. Left empty, the runner plays through the engine's `Audio`
/// resource when there is one and plays nothing otherwise (lines run on their planned length).
#[derive(Resource, Default)]
pub struct ConversationVoice(pub Option<Box<dyn VoicePlayer>>);

/// Runs `f` with the voice player in force: the [`ConversationVoice`] override, else the engine
/// audio, else [`NullVoicePlayer`].
pub fn with_voice_player<R>(
    world: &mut World,
    clock_us: u64,
    f: impl FnOnce(&mut dyn VoicePlayer) -> R,
) -> R {
    let device = world
        .get_resource::<CaptionDeviceClock>()
        .copied()
        .unwrap_or_default();
    world.resource_scope(|world, mut voice: Mut<ConversationVoice>| {
        if let Some(player) = voice.0.as_mut() {
            return f(player.as_mut());
        }
        if !world.contains_resource::<regia_audio::Audio>() {
            return f(&mut NullVoicePlayer);
        }
        world.resource_scope(|world, mut audio: Mut<regia_audio::Audio>| {
            let vfs = world.get_resource::<Vfs>();
            let assets_dir = assets_host_root(vfs).unwrap_or_else(|| PathBuf::from("assets"));
            f(&mut AudioVoicePlayer {
                audio: &mut audio,
                assets_dir: &assets_dir,
                vfs,
                clock_us,
                device,
            })
        })
    })
}

/// Sent when a `Choice` step offers options.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ConversationChoiceRequested {
    /// The run.
    pub run: RunId,
    /// The resolved prompt.
    pub prompt: Option<String>,
    /// The offered options.
    pub options: Vec<ChoiceView>,
}

/// Sent by an `Event` step.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ConversationEvent {
    /// The run.
    pub run: RunId,
    /// The event name.
    pub name: String,
}

/// Sent when a run ends.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ConversationFinished {
    /// The run.
    pub run: RunId,
    /// Why.
    pub reason: FinishReason,
}

/// The world as the runner sees it.
pub struct WorldEnv<'a> {
    /// The loaded library.
    pub library: &'a ConversationLibrary,
    /// The localization, if the UI plugin is installed.
    pub loc: Option<&'a Localization>,
    /// The voice player.
    pub voice: &'a mut dyn VoicePlayer,
}

impl RunnerEnv for WorldEnv<'_> {
    fn conversation(&self, id: &str) -> Option<&ConversationAsset> {
        self.library.conversations.get(id)
    }
    fn settings(&self) -> &ConversationSettings {
        &self.library.settings
    }
    fn speakers(&self) -> &SpeakerSet {
        &self.library.speakers
    }
    fn locale_loaded(&self, locale: &str) -> bool {
        self.loc.is_none_or(|l| l.source_text(locale).is_some())
    }
    fn active_locale(&self) -> String {
        self.loc
            .map_or_else(|| "en".to_string(), |l| l.locale().to_string())
    }
    fn chain(&self, locale: &str) -> Vec<String> {
        match self.loc {
            Some(l) => l.chain(locale),
            None => vec![locale.to_string()],
        }
    }
    fn text(&self, locale: &str, key: &str) -> Option<String> {
        let loc = self.loc?;
        self.chain(locale)
            .iter()
            .find_map(|l| loc.resolve_in(l, key, &[]))
    }
    fn clip_exists(&self, path: &str) -> bool {
        self.library.clips.contains_key(path)
    }
    fn clip_duration_us(&self, path: &str) -> Option<u64> {
        self.library.clips.get(path).copied().flatten()
    }
    fn play_voice(&mut self, clip: &str) -> Option<VoiceStart> {
        let duration = self.library.clips.get(clip).copied().flatten();
        self.voice.play(clip, duration)
    }
    fn voice_playing(&self, handle: u64) -> bool {
        self.voice.playing(handle)
    }
    fn caption_now_us(&self) -> Option<u64> {
        self.voice.caption_now_us()
    }
}

/// A [`ValidateEnv`] over a library and a localization.
pub struct LibraryValidateEnv<'a> {
    /// The library (clips).
    pub library: &'a ConversationLibrary,
    /// The localization.
    pub loc: &'a Localization,
}

impl ValidateEnv for LibraryValidateEnv<'_> {
    fn has_text_in(&self, locale: &str, key: &str) -> bool {
        self.loc.has_in(locale, key)
    }
    fn text(&self, locale: &str, key: &str) -> Option<String> {
        self.chain(locale)
            .iter()
            .find_map(|l| self.loc.resolve_in(l, key, &[]))
    }
    fn chain(&self, locale: &str) -> Vec<String> {
        self.loc.chain(locale)
    }
    fn clip_exists(&self, path: &str) -> bool {
        self.library.clips.contains_key(path)
    }
    fn clip_duration_us(&self, path: &str) -> Option<u64> {
        self.library.clips.get(path).copied().flatten()
    }
}

/// Runs `f` with the world and the [`Localization`] (taken out of the world for the call), if the
/// app has one.
pub fn with_localization<R>(world: &mut World, f: impl FnOnce(&mut World, Option<&Localization>) -> R) -> R {
    if world.contains_resource::<Localization>() {
        world.resource_scope(|world, loc: Mut<Localization>| f(world, Some(&loc)))
    } else {
        f(world, None)
    }
}

/// `PostUpdate`, before the caption board: gives it the output's sample clock (spec 56 § 2). This
/// is the caption *presentation* clock; the conversation runner does not read it (it ticks on
/// [`ConversationClock`], audit G finding 2, ADR-4756).
pub fn feed_caption_device_clock(
    audio: Option<Res<regia_audio::Audio>>,
    clock: Option<ResMut<CaptionDeviceClock>>,
) {
    if let Some(mut clock) = clock {
        clock.device_us = audio
            .and_then(|audio| audio.sample_clock())
            .map(regia_audio::SampleClock::micros);
    }
}

/// The conversation runner's clock in microseconds: fixed-step simulation time, advanced one
/// `FixedUpdate` at a time. The runner (line timing, events, choices) ticks on this clock so a
/// live device, a replay and a headless run emit the same step sequence; the audio device sample
/// clock stays with the caption board as a presentation clock (audit G finding 2, ADR-4756).
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ConversationClock(pub u64);

/// `FixedUpdate`: advances [`ConversationClock`] by how far the simulation clock
/// (`Time::elapsed`, in microseconds) moved since the last step, and never moves it backward.
/// The first step takes the simulation clock itself (or keeps a clock already ahead of it). A
/// step where the simulation clock jumped back (`Time::set_clocks`, which a render-out job calls
/// at its start) adds nothing, and later steps advance from there, so a reset neither re-fires a
/// line nor freezes the runner until the new clock passes the old one (corr-framework-a F5).
/// Without a [`Time`] resource the clock does not advance; the runner then falls back to the
/// caption clock (a bare app without the time plugin).
pub fn advance_conversation_clock(
    time: Option<Res<Time>>,
    mut clock: ResMut<ConversationClock>,
    mut last_sim_us: Local<Option<u64>>,
) {
    if let Some(time) = time {
        // Differences of one conversion of the simulation clock telescope back to it. Summing each
        // step's truncated microseconds instead drops the leftover nanoseconds (667 ns of a
        // 1/60 s step) and drifts off `Time`.
        let now = u64::try_from(time.elapsed().as_micros()).unwrap_or(u64::MAX);
        clock.0 = match *last_sim_us {
            None => clock.0.max(now),
            Some(previous) => clock.0.saturating_add(now.saturating_sub(previous)),
        };
        *last_sim_us = Some(now);
    }
}

/// The runner system: `PostUpdate`, before the caption board (`regia_ui::captions`). It takes the
/// world because the voice player may borrow the engine's `Audio` resource. Ticks on
/// [`ConversationClock`] (fixed-step simulation time) whenever the world has a [`Time`] resource —
/// always, for an `App` (`App::new` installs one; the caption-clock branch is an unreachable
/// defensive default, see [`advance_conversation_clock`]).
pub fn conversation_system(world: &mut World) {
    if !world
        .get_resource::<ConversationRunner>()
        .is_some_and(ConversationRunner::has_work)
    {
        return;
    }
    // `App::new` always installs `Time`, so the caption-clock branch is an unreachable defensive
    // default for a bare `World` (see `advance_conversation_clock`).
    let caption_clock = world.get_resource::<CaptionClock>().map_or(0, |c| c.0);
    let clock = if world.contains_resource::<Time>() {
        world.get_resource::<ConversationClock>().map_or(0, |c| c.0)
    } else {
        caption_clock
    };
    let outputs = world.resource_scope(|world, mut runner: Mut<ConversationRunner>| {
        world.resource_scope(|world, mut ids: Mut<CaptionIds>| {
            world.resource_scope(|world, library: Mut<ConversationLibrary>| {
                with_localization(world, |world, loc| {
                    // The voice player stamps captions on the caption clock (its `clock_us`); the
                    // runner's deadlines stay on `clock` (corr-framework-a F4).
                    with_voice_player(world, caption_clock, |voice| {
                        let mut env = WorldEnv {
                            library: &library,
                            loc,
                            voice,
                        };
                        runner.tick(clock, &mut env, &mut ids)
                    })
                })
            })
        })
    });
    for output in outputs {
        match output {
            RunnerOutput::Caption(c) => {
                world.write_message(c);
            }
            RunnerOutput::CaptionStop(id) => {
                world.write_message(CaptionStop { id });
            }
            RunnerOutput::Event { run, name } => {
                world.write_message(ConversationEvent { run, name });
            }
            RunnerOutput::ChoiceRequested { run, prompt, options } => {
                world.write_message(ConversationChoiceRequested { run, prompt, options });
            }
            RunnerOutput::Finished { run, reason, .. } => {
                world.write_message(ConversationFinished { run, reason });
            }
            RunnerOutput::StopVoice { handle, fade_ms } => {
                with_voice_player(world, caption_clock, |voice| voice.stop(handle, fade_ms));
            }
        }
    }
}

/// Turns the cue plays that carry a caption into `CaptionStart` messages (kind from the cue, text
/// a Fluent key the board resolves) and ends each caption when its sound does (`CaptionStop`).
pub fn cue_caption_system(
    audio: Option<ResMut<regia_audio::Audio>>,
    mut ids: ResMut<CaptionIds>,
    loc: Option<Res<Localization>>,
    mut starts: MessageWriter<CaptionStart>,
    mut stops: MessageWriter<CaptionStop>,
    mut live: Local<Vec<(regia_audio::SoundInstanceId, regia_app::caption::CaptionId)>>,
) {
    let Some(mut audio) = audio else {
        return;
    };
    let _span = tracing::debug_span!(target: "regia_framework", "cue_captions").entered();
    for play in audio.take_cue_captions() {
        let planned_us = match play.caption.planned_ms {
            Some(ms) => u64::from(ms) * 1000,
            None if play.duration_secs.is_finite() && play.duration_secs >= 0.0 => {
                (play.duration_secs * 1_000_000.0).round() as u64
            }
            None => 2_000_000,
        };
        let id = ids.next_id();
        starts.write(CaptionStart {
            id,
            source: regia_app::caption::CaptionSource::SoundCue(play.cue.clone()),
            kind: match play.caption.kind {
                regia_audio::CueCaptionKind::Sound => regia_app::caption::CaptionKind::Sound,
                regia_audio::CueCaptionKind::Speech => regia_app::caption::CaptionKind::Speech,
                regia_audio::CueCaptionKind::Narration => regia_app::caption::CaptionKind::Narration,
            },
            locale: loc
                .as_deref()
                .map_or_else(|| "en".to_string(), |l| l.locale().to_string()),
            pieces: vec![regia_app::caption::CaptionPiece {
                offset_us: 0,
                text: regia_app::caption::CaptionText::Key {
                    key: play.caption.key,
                    args: Vec::new(),
                },
            }],
            speaker: None,
            priority: play.caption.priority,
            start: regia_app::caption::CaptionTime::Now,
            planned_us,
            keep_breaks: false,
            hint: regia_app::caption::CaptionStyleHint::default(),
        });
        live.push((play.instance, id));
    }
    live.retain(|&(instance, caption)| {
        let playing = audio.voice_playing(instance);
        if !playing {
            stops.write(CaptionStop { id: caption });
        }
        playing
    });
}

/// Tells the caption board how each speaker is shown (localized name, label color) whenever the
/// library or the localization changes.
pub fn sync_speakers(
    library: Res<ConversationLibrary>,
    loc: Option<Res<Localization>>,
    board: Option<ResMut<CaptionBoard>>,
    mut seen: Local<(u64, bool)>,
) {
    let Some(mut board) = board else {
        return;
    };
    let generation = loc.as_ref().map_or(0, |l| l.generation.0);
    if !library.is_changed() && *seen == (generation, true) {
        return;
    }
    *seen = (generation, true);
    for (id, speaker) in &library.speakers.speakers {
        let name = speaker
            .name_key
            .as_deref()
            .and_then(|key| loc.as_deref().map(|l| l.resolve(key, &[])))
            .unwrap_or_else(|| id.clone());
        board.set_speaker(
            id.clone(),
            SpeakerStyle {
                name,
                color: speaker.color.as_deref().and_then(Rgb::from_hex),
                side: match speaker.side {
                    asset::LabelSide::Start => regia_ui::captions::LabelSide::Start,
                    asset::LabelSide::End => regia_ui::captions::LabelSide::End,
                },
            },
        );
    }
}

/// Gives the caption board the project's caption settings from `conversation.ron` (`overlap`,
/// `max_visible`, `min_display_ms`, `max_extend_ms`, reading speeds; spec 56 § 4.1) whenever they
/// change, and to a board that is inserted (or re-created) later. A settings file with any
/// non-default caption field owns all six fields; one whose six fields are all default never
/// overwrites a policy the game set on the board. Fields the settings file does not own
/// (`max_lines`, `bottom_offset`, ...) are left alone.
pub fn sync_caption_policy(
    library: Res<ConversationLibrary>,
    board: Option<ResMut<CaptionBoard>>,
    mut applied: Local<Option<CaptionFields>>,
) {
    let Some(mut board) = board else {
        return;
    };
    let added = board.is_added();
    if !added && !library.is_changed() {
        return;
    }
    let fields = library.settings.caption_fields();
    // A new board starts from its own policy: compare against the defaults again, so non-default
    // settings reach it and default ones leave a game-set policy alone (rev-kimi-14 finding 4,
    // ADR-5936).
    if added {
        *applied = None;
    }
    let last = applied.get_or_insert_with(|| ConversationSettings::default().caption_fields());
    if *last == fields {
        return;
    }
    *last = fields;
    library.settings.apply_caption_policy(&mut board.policy);
}

/// Dev runs: when the working directory has `assets/conversations/`, loads the library from it
/// unless a game already filled it. A packaged build gets its conversations from the cooked
/// `.convc` files (not built yet), so this does nothing there.
pub fn load_dev_library(
    mut library: ResMut<ConversationLibrary>,
    cue_events: Option<ResMut<CueEventMap>>,
    vfs: Option<Res<Vfs>>,
) {
    if !library.conversations.is_empty() {
        return;
    }
    let root = assets_host_root(vfs.as_deref());
    let loaded = match (vfs.as_deref(), root) {
        (Some(vfs), None) => ConversationLibrary::load_vfs(vfs),
        (_, root) => {
            let assets = root.unwrap_or_else(|| PathBuf::from("assets"));
            if !assets.join(asset::CONVERSATIONS_DIR).is_dir() {
                return;
            }
            ConversationLibrary::load_assets(&assets).map(|mut loaded| {
                // A packaged build ships `<id>.convc` instead of the `.conv.ron` sources (ADR-1882).
                if let Err(error) = loaded.load_cooked(&assets.join(asset::CONVERSATIONS_DIR)) {
                    tracing::warn!(%error, "conversations: a cooked conversation could not be read");
                }
                loaded
            })
        }
    };
    match loaded {
        Ok(loaded) => {
            if let Some(mut map) = cue_events {
                map.0 = loaded.cue_events.clone();
            }
            *library = loaded;
        }
        Err(error) => tracing::warn!(%error, "conversations: could not load the conversations"),
    }
}

/// Adds the library, runner, voice player and messages. Inert while no conversation plays.
#[derive(Debug, Default, Clone, Copy)]
pub struct ConversationPlugin;

impl Plugin for ConversationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ConversationLibrary>()
            .init_resource::<ConversationRunner>()
            .init_resource::<ConversationVoice>()
            .init_resource::<ConversationClock>()
            .init_resource::<CaptionIds>()
            .register_type::<ConversationAsset>()
            .register_type::<Speaker>()
            .register_type::<ConversationSettings>()
            // The caption messages too, so the runner works with or without the presenter.
            .add_message::<CaptionStart>()
            .add_message::<CaptionStop>()
            .add_message::<ConversationEvent>()
            .add_message::<ConversationChoiceRequested>()
            .add_message::<ConversationFinished>()
            .add_systems(Startup, load_dev_library)
            .add_systems(FixedUpdate, advance_conversation_clock)
            .add_systems(
                PostUpdate,
                (
                    feed_caption_device_clock,
                    sync_speakers,
                    sync_caption_policy,
                    cue_caption_system,
                    conversation_system,
                )
                    .chain()
                    .before(regia_ui::captions::caption_system),
            );
    }
}

/// The fallback chain used when only the loaded locale set is known (re-export for tools).
pub fn chain_for(locale: &str, loaded: &std::collections::BTreeSet<String>, source: &str) -> Vec<String> {
    fallback_chain(locale, loaded, &BTreeMap::new(), source)
}

#[cfg(test)]
mod tests;
