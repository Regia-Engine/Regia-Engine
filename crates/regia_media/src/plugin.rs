// SPDX-License-Identifier: MPL-2.0
//! The ECS surface (specs/50 § 3.4): the [`VideoPlayer`] component, its runtime
//! [`VideoPlayerState`], the [`MediaFiles`] resolver, [`MediaSettings`], [`VideoMessage`]s, and
//! [`MediaPlugin`] with its systems (open in `PreUpdate`, clock and frame selection in `Update`,
//! playlist advance after the clock, events in `PostUpdate`, sorted by entity bits). The command functions at the bottom are the data
//! layer of the `video.*` agent methods (`regia_agent::video`).

use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::list::ListCursor;
use crate::source::Preload;

use regia_app::prelude::{App, Plugin, PostUpdate, PreUpdate, Time, Update};
use regia_audio::media_feed::{
    AudioClock, MEDIA_FEED_RATES, MediaAudioRegistry, MediaFeedEntry, PcmFeed, PcmProducer,
};
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::container::{self, MediaInfo};
use crate::error::MediaError;
use crate::isolate::{DecodeCaps, SLOW_DECODE_FRAMES};
use crate::player::{ClockInput, ClockMode, LoopMode, PlayState, PlayerCore, SeekMode, VideoEvent};
use crate::reference::{Region, picture_to_rgba8, rgba_hash};
use crate::session::{AudioChoice, AudioSink, FrameFeed, Session, SessionOptions, WorkerFeed};
use crate::source::{VideoSource, platform_key};

/// When a player off screen keeps decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect, Serialize, Deserialize)]
pub enum VisibilityPolicy {
    /// Always decode.
    Always,
    /// Stop decoding while invisible; the clock keeps running.
    #[default]
    PauseDecode,
    /// Stop decoding and the clock while invisible.
    PauseAll,
}

/// Whether a player's surface is seen this frame (specs/50 § 2.7). The host, a culling system or an
/// agent writes it; a player without one counts as visible. With
/// [`VisibilityPolicy::PauseDecode`] an invisible player stops decoding (the clock runs on), with
/// [`VisibilityPolicy::PauseAll`] its clock stops too; fullscreen players always decode.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Reflect)]
#[reflect(Component)]
pub struct VideoVisible(pub bool);

impl Default for VideoVisible {
    fn default() -> Self {
        Self(true)
    }
}

/// One playback session on an entity (specs/50 § 3.2). `source` is a project path to a `.video.ron`
/// or directly to a `.webm`/`.mkv` file, resolved through [`MediaFiles`]. Changing `source` reopens;
/// changing `rate`, `loop_mode` or `clock` applies to the running session.
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component)]
#[require(VideoPlayerState)]
pub struct VideoPlayer {
    /// `.video.ron` or media file path.
    #[reflect(@regia_ecs::attrs::AssetFilter("*.video.ron, *.webm, *.mkv"))]
    pub source: String,
    /// Start playing once the first picture is ready.
    pub autoplay: bool,
    /// Forward rate, `0.25..=4.0`.
    pub rate: f32,
    /// Linear volume.
    pub volume: f32,
    /// Loop policy.
    pub loop_mode: LoopMode,
    /// Clock mode.
    pub clock: ClockMode,
    /// Initial seek in seconds. On a `.videolist.ron` it applies to the first entry the list opens;
    /// later entries start at 0.
    pub phase_offset: f64,
    /// Audio track index. `None` uses the default rule (the flagged track, else the language
    /// match, else the first). It does not mute. `video.select_track` with `index: null` mutes
    /// until a later index select; that flag lives on [`VideoPlayerState`], not here.
    pub audio_track: Option<u32>,
    /// Off-screen policy.
    pub visibility: VisibilityPolicy,
    /// Stored and unused. An open past [`MediaSettings::max_decoders`] is refused with
    /// `limit_exceeded` (ADR-2410). Players do not queue.
    pub priority: i32,
    /// Stored and unused. The mixer applies [`VideoPlayer::volume`] and does not pan from the
    /// entity position.
    pub spatial: bool,
}

impl Default for VideoPlayer {
    fn default() -> Self {
        Self {
            source: String::new(),
            autoplay: true,
            rate: 1.0,
            volume: 1.0,
            loop_mode: LoopMode::Off,
            clock: ClockMode::Game,
            phase_offset: 0.0,
            audio_track: None,
            visibility: VisibilityPolicy::PauseDecode,
            priority: 0,
            spatial: false,
        }
    }
}

impl VideoPlayer {
    /// A player for `source` with defaults.
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            ..Self::default()
        }
    }
}

/// Runtime state of a player (never persisted).
#[derive(Component, Debug)]
pub struct VideoPlayerState {
    core: Mutex<PlayerCore>,
    opened: Option<String>,
    pub(crate) applied: Option<(f32, LoopMode, ClockMode)>,
    clear_color: [f32; 4],
    external_time_ns: Option<u64>,
    rgba: Option<(u64, u32, u32, Vec<u8>)>,
    /// `video.select_track` with a null index. `None` on [`VideoPlayer::audio_track`] still means
    /// the default track while this is false.
    audio_disabled: bool,
    playlist: Option<PlaylistRun>,
    list_finished: bool,
    /// The `.video.ron` or media file open now. For a playlist this is the current entry;
    /// [`Self::opened`] stays the `.videolist.ron` path so a source change is the only reopen.
    playing: Option<String>,
}

/// One `.videolist.ron` walk. The component `source` stays the list path.
#[derive(Debug)]
struct PlaylistRun {
    entries: Vec<String>,
    cursor: ListCursor,
}

impl Default for VideoPlayerState {
    fn default() -> Self {
        Self {
            core: Mutex::default(),
            opened: None,
            applied: None,
            clear_color: [0.0, 0.0, 0.0, 1.0],
            external_time_ns: None,
            rgba: None,
            audio_disabled: false,
            playlist: None,
            list_finished: false,
            playing: None,
        }
    }
}

impl VideoPlayerState {
    /// The player core (exclusive access).
    pub fn core_mut(&mut self) -> &mut PlayerCore {
        self.core.get_mut().unwrap_or_else(PoisonError::into_inner)
    }

    /// The player core (shared access).
    pub fn core(&self) -> MutexGuard<'_, PlayerCore> {
        self.core.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The source path of the open session (the component `source`, including a `.videolist.ron`).
    pub fn opened(&self) -> Option<&str> {
        self.opened.as_deref()
    }

    /// The file or `.video.ron` playing now. On a playlist this is the current entry.
    pub fn playing(&self) -> Option<&str> {
        self.playing.as_deref()
    }

    /// Whether `video.select_track` with a null index has muted this player.
    pub fn audio_disabled(&self) -> bool {
        self.audio_disabled
    }

    /// Mute (`true`, `select_track` index null) or follow [`VideoPlayer::audio_track`] again (`false`).
    /// A later open keeps the flag: muting survives a source change until an index select clears it.
    pub fn set_audio_disabled(&mut self, disabled: bool) {
        self.audio_disabled = disabled;
    }

    /// The clear color shown while no picture is presented (the `.video.ron` `clear_color`, black
    /// by default), linear RGBA as written in the file.
    pub fn clear_color(&self) -> [f32; 4] {
        self.clear_color
    }

    /// Sets the media time for the `External` clock.
    pub fn set_external_time(&mut self, seconds: f64) {
        self.external_time_ns = Some(if seconds.is_finite() && seconds > 0.0 {
            (seconds * 1e9) as u64
        } else {
            0
        });
    }

    /// The presented picture as RGBA8 (CPU reference conversion, cached per presented frame), or the
    /// clear color as a 1x1 image while nothing is presented.
    pub fn frame_rgba(&mut self) -> (u32, u32, Vec<u8>) {
        let clear = self
            .clear_color
            .map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8);
        let presented_count = self.core_mut().stats().frames_presented;
        let core = self.core.get_mut().unwrap_or_else(PoisonError::into_inner);
        let Some(p) = core.presented() else {
            return (1, 1, clear.to_vec());
        };
        if let Some((count, w, h, rgba)) = &self.rgba
            && *count == presented_count
        {
            return (*w, *h, rgba.clone());
        }
        let rgba = picture_to_rgba8(p);
        let (w, h) = (p.width, p.height);
        self.rgba = Some((presented_count, w, h, rgba.clone()));
        (w, h, rgba)
    }
}

/// Reader trait object for media files.
pub trait MediaRead: Read + Seek + Send {}
impl<T: Read + Seek + Send> MediaRead for T {}

/// Resolves media paths: in-memory files first (tests, agents, generated clips), then files under the
/// project root. Paths must be relative and must not leave the root, through a symlink either.
#[derive(Resource, Debug, Default, Clone)]
pub struct MediaFiles {
    root: Option<PathBuf>,
    memory: BTreeMap<String, Arc<[u8]>>,
}

impl MediaFiles {
    /// A resolver rooted at the project directory.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
            memory: BTreeMap::new(),
        }
    }

    /// The project root files resolve under, if set.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Sets the project root (in-memory files stay).
    pub fn set_root(&mut self, root: impl Into<PathBuf>) {
        self.root = Some(root.into());
    }

    /// Registers an in-memory file.
    pub fn insert(&mut self, path: impl Into<String>, bytes: impl Into<Arc<[u8]>>) {
        self.memory.insert(path.into(), bytes.into());
    }

    /// The project file `path` names, through the shared path rule
    /// (`regia_core::project_path::resolve_read`, ADR-4922): relative, no `..`, drive, UNC or
    /// device name on any host, and no symlink out of the root. A refused path and a missing file
    /// are both `SourceNotFound`, so a probe learns nothing about files outside the project.
    fn checked(&self, path: &str) -> Result<PathBuf, MediaError> {
        let root = self
            .root
            .as_ref()
            .ok_or_else(|| MediaError::SourceNotFound(path.to_string()))?;
        let joined = regia_core::project_path::resolve_read(root, path)
            .map_err(|_| MediaError::SourceNotFound(path.to_string()))?;
        // Open the verified real path: on Windows it is the `\\?\` extended-length form, the only
        // form that opens past MAX_PATH (media audit G 4).
        Ok(joined.canonicalize().unwrap_or(joined))
    }

    /// Opens a file for reading.
    pub fn open(&self, path: &str) -> Result<Box<dyn MediaRead>, MediaError> {
        if let Some(bytes) = self.memory.get(path) {
            return Ok(Box::new(std::io::Cursor::new(bytes.clone())));
        }
        let full = self.checked(path)?;
        let file = std::fs::File::open(&full).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => MediaError::SourceNotFound(path.to_string()),
            _ => MediaError::io(e),
        })?;
        Ok(Box::new(std::io::BufReader::new(file)))
    }

    /// Reads a whole small text file (`.video.ron`).
    pub fn read_text(&self, path: &str) -> Result<String, MediaError> {
        let mut reader = self.open(path)?;
        let mut text = String::new();
        reader
            .by_ref()
            .take(1024 * 1024)
            .read_to_string(&mut text)
            .map_err(MediaError::io)?;
        Ok(text)
    }

    /// Resolves a player source to the media file path and its `.video.ron` (if any).
    pub fn resolve(&self, source: &str) -> Result<(String, Option<VideoSource>), MediaError> {
        if source.ends_with(".video.ron") {
            // A refused `.video.ron` (a newer `format_version`, bad RON) names the file (spec 50 § 4).
            let def = VideoSource::from_ron(&self.read_text(source)?).map_err(|e| match e {
                MediaError::ContainerInvalid(m) => MediaError::ContainerInvalid(format!("{source}: {m}")),
                other => other,
            })?;
            let file = def.file_for(platform_key()).to_string();
            container::check_extension(&file)?;
            Ok((file, Some(def)))
        } else {
            container::check_extension(source)?;
            Ok((source.to_string(), None))
        }
    }
}

/// Limits and defaults (specs/50 § 2.4).
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct MediaSettings {
    /// Decoder caps.
    pub caps: DecodeCaps,
    /// The `Lockstep` fixed step in nanoseconds (1/60 s by default).
    pub lockstep_dt_ns: u64,
    /// Most open players.
    pub max_players: usize,
    /// Most players decoding at once (the decode pool, specs/50 § 2.4): 4 on desktop, 2 on Android.
    /// Opening, playing, seeking and buffering players decode; ready, paused, ended and
    /// decode-paused ones do not. An open while the pool is full is refused with `limit_exceeded`
    /// (ADR-2410); the check runs at open.
    pub max_decoders: usize,
    /// React to `regia_platform::ThermalState` (specs/50 § 9): moderate halves the presented rate
    /// of non-fullscreen players, severe and above also pauses `PauseDecode` players. On by default.
    pub thermal_throttle: bool,
    /// Decoded frame queue per player (4, max 8).
    pub queue_frames: usize,
    /// Language for audio track selection. The default is the operating-system locale
    /// (`regia_platform::system_locale`, for example `en-US`), or `eng` when that locale is
    /// missing, `C`, or `POSIX`. Matching is [`crate::container::languages_match`], not a raw
    /// string compare, so `en-US` still selects an `eng` track.
    pub language: String,
    /// Decode into a ring bounded by frames and bytes (`crate::ring::RingFeed`, ADR-2044) instead of
    /// the frame-count queue. `None` (the default) keeps the frame-count queue; `VideoFramePlugin`
    /// turns the ring on.
    pub ring_budget: Option<crate::ring::RingBudget>,
}

impl Default for MediaSettings {
    fn default() -> Self {
        Self {
            caps: DecodeCaps::for_platform(),
            lockstep_dt_ns: 16_666_667,
            max_players: 8,
            max_decoders: if cfg!(target_os = "android") { 2 } else { 4 },
            thermal_throttle: true,
            queue_frames: 4,
            language: language_or_eng(regia_platform::system_locale().as_deref()),
            ring_budget: None,
        }
    }
}

/// A player notification as an ECS message.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct VideoMessage {
    /// The player entity.
    pub entity: Entity,
    /// What happened.
    pub event: VideoEvent,
}

/// The last 256 events, for agents (`video.status` and the event subscriptions).
#[derive(Resource, Debug, Default)]
pub struct VideoRegistry {
    log: VecDeque<(u64, Entity, VideoEvent)>,
    next_seq: u64,
}

impl VideoRegistry {
    /// Recent events, oldest first, with their sequence numbers.
    pub fn recent(&self) -> impl Iterator<Item = &(u64, Entity, VideoEvent)> {
        self.log.iter()
    }

    fn push(&mut self, entity: Entity, event: VideoEvent) {
        if self.log.len() >= 256 {
            self.log.pop_front();
        }
        self.log.push_back((self.next_seq, entity, event));
        self.next_seq += 1;
    }
}

/// Registers the player systems, resources and messages.
#[derive(Debug, Default)]
pub struct MediaPlugin;

impl Plugin for MediaPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<VideoPlayer>()
            .register_type::<crate::present::VideoFullscreen>()
            .register_type::<VideoVisible>()
            .init_resource::<MediaSettings>()
            .init_resource::<VideoRegistry>()
            .init_resource::<VideoCaptionRoutes>()
            .add_message::<VideoMessage>()
            .add_message::<crate::boot::SkipVideo>()
            .add_systems(PreUpdate, (crate::boot::run_boot_movies, open_players).chain())
            .add_systems(
                Update,
                (apply_visibility, tick_players, advance_playlists).chain(),
            )
            .add_systems(PostUpdate, (emit_video_events, route_cues_to_captions).chain());
        if !app.world().contains_resource::<MediaFiles>() {
            app.insert_resource(MediaFiles::default());
        }
    }
}

/// The decode thread's end of a player's PCM feed. Samples that do not fit the ring are counted
/// and warned about once per session instead of vanishing (deep-zai-media finding 3).
struct PcmSink {
    producer: PcmProducer,
    dropped: u64,
}

impl PcmSink {
    fn new(producer: PcmProducer) -> Self {
        Self { producer, dropped: 0 }
    }
}

impl AudioSink for PcmSink {
    fn push(&mut self, _pts_ns: i64, samples: &[f32]) {
        let kept = self.producer.push(samples);
        if kept < samples.len() {
            if self.dropped == 0 {
                tracing::warn!(
                    target: "regia_media",
                    dropped = samples.len() - kept,
                    "video audio feed full: decoded audio dropped"
                );
            }
            self.dropped = self.dropped.saturating_add((samples.len() - kept) as u64);
        }
    }

    fn flush(&mut self) {
        self.producer.clear();
    }
}

/// Audio frames a player's PCM ring holds: half a second for the output's own lead, plus the audio
/// of the pictures the decode thread may be ahead of the clock (`queue_frames` queued, one being
/// sent, one held by the player) at the clip's frame period, at most 4 s. A clip that declares no
/// frame period (no `DefaultDuration`) gets the 4 s ring, since its frame rate is unknown until it
/// plays and a low-rate clip sized for 30 fps overflowed (KI-AU-028, ADR-10070). A 500 ms ring
/// dropped audio at frame rates under about 8 fps (deep-zai-media finding 3).
///
/// The ring is also capped at [`PCM_RING_MAX_BYTES`] of samples (ADR-7635): the rate, the channel
/// count and the frame period all come from the file, so without the cap a few-KB file (768 kHz,
/// 8 channels, a 1 s video frame period) allocated about 98 MB per player. Every 48 kHz layout up
/// to 8 channels stays under the cap, so real content keeps its ring.
pub(crate) fn pcm_ring_frames(rate: u32, channels: u8, frame_ns: Option<u64>, queue_frames: usize) -> usize {
    let rate = u128::from(rate.max(1));
    let pictures = queue_frames.clamp(1, 8) as u128 + 2;
    // No declared period: the longest ring (the 4 s cap below), never a 30 fps guess (KI-AU-028).
    let frame_ns = u128::from(frame_ns.filter(|d| *d > 0).unwrap_or(u64::MAX));
    let ahead = pictures.saturating_mul(frame_ns).saturating_mul(rate) / 1_000_000_000;
    let sample_bytes = std::mem::size_of::<f32>() * usize::from(channels.max(1));
    let byte_cap = (PCM_RING_MAX_BYTES / sample_bytes) as u128;
    (rate / 2 + ahead).min(rate * 4).min(byte_cap).max(1) as usize
}

/// The most sample memory one player's PCM ring may allocate (ADR-7635): 8 MiB, which holds 4 s of
/// 48 kHz 8-channel float audio with room to spare. At the default `max_players` of 8 the rings of
/// hostile files total at most 64 MiB.
pub(crate) const PCM_RING_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Opens `player.source` into `state` (closing any previous session). Applies the open-player
/// limit only; [`open_player_pooled`] also applies the decode pool.
pub fn open_player(
    entity: Entity,
    player: &VideoPlayer,
    state: &mut VideoPlayerState,
    files: &MediaFiles,
    settings: &MediaSettings,
    open_players: usize,
    audio: Option<&mut MediaAudioRegistry>,
) {
    open_player_pooled(entity, player, state, files, settings, (open_players, 0), audio);
}

/// [`open_player`] with the decode pool: `counts` is (open players, sessions holding a decoder),
/// both without this player. A session past [`MediaSettings::max_decoders`] fails to open with
/// `limit_exceeded` ("decode pool full").
pub fn open_player_pooled(
    entity: Entity,
    player: &VideoPlayer,
    state: &mut VideoPlayerState,
    files: &MediaFiles,
    settings: &MediaSettings,
    counts: (usize, usize),
    audio: Option<&mut MediaAudioRegistry>,
) {
    let _span = tracing::debug_span!("video_open").entered();
    let (open_players, decoding) = counts;
    state.opened = Some(player.source.clone());
    state.applied = Some((player.rate, player.loop_mode, player.clock));
    state.rgba = None;
    state.playlist = None;
    state.list_finished = false;
    state.playing = None;
    if open_players >= settings.max_players {
        let err = MediaError::LimitExceeded(format!("more than {} open players", settings.max_players));
        state.core_mut().fail_open(err);
        return;
    }
    if decoding >= settings.max_decoders.max(1) {
        let err = MediaError::LimitExceeded(format!(
            "decode pool full: {} decoders in use (MediaSettings::max_decoders)",
            settings.max_decoders.max(1)
        ));
        state.core_mut().fail_open(err);
        return;
    }
    if player.source.ends_with(".videolist.ron") {
        match files
            .read_text(&player.source)
            .and_then(|text| crate::source::VideoList::from_ron(&text))
        {
            Ok(list) => {
                let cursor = ListCursor::new(list.entries.len(), list.loop_list, list.shuffle);
                state.playlist = Some(PlaylistRun {
                    entries: list.entries,
                    cursor,
                });
                let mut audio = audio;
                open_playlist_entries(entity, player, state, files, settings, &mut audio, true);
            }
            Err(e) => state.core_mut().fail_open(e),
        }
        return;
    }
    if let Err(e) = open_clip(
        entity,
        player,
        state,
        files,
        settings,
        audio,
        (&player.source, player.phase_offset),
    ) {
        state.core_mut().fail_open(e);
    }
}

/// A non-finite volume poisons the mixer (`sample * volume` stays non-finite). Playback uses `1.0`.
fn finite_volume(volume: f32) -> f32 {
    if volume.is_finite() { volume } else { 1.0 }
}

/// Volume and loop from a `.video.ron` when the component is still at its defaults (`volume == 1.0`,
/// [`LoopMode::Off`]). An explicit `1.0` or `Off` cannot override the asset: those are the defaults,
/// and the component has no separate "authored" flag. A non-finite asset volume does not replace the
/// component, and a non-finite result is `1.0`: `NaN` is not clamped by the mixer and would silence
/// the bus for the whole clip.
pub(crate) fn asset_playback(
    player_volume: f32,
    player_loop: LoopMode,
    def: &VideoSource,
) -> (f32, LoopMode) {
    let volume = if player_volume == 1.0 && def.volume.is_finite() {
        def.volume
    } else {
        finite_volume(player_volume)
    };
    let volume = if volume.is_finite() { volume } else { 1.0 };
    let loop_mode = if player_loop == LoopMode::Off {
        def.loop_mode
    } else {
        player_loop
    };
    (volume, loop_mode)
}

/// Bytes [`Preload::WholeFile`] will read. 64 MiB, or 32 MiB on Android.
pub(crate) fn whole_file_limit() -> usize {
    if cfg!(target_os = "android") {
        32 * 1024 * 1024
    } else {
        64 * 1024 * 1024
    }
}

/// Reads `reader` into memory when `len <= limit`. The length is checked before the buffer grows,
/// and only that many bytes are read, so a reader whose later `read` returns more than `seek`
/// reported cannot allocate past the length that passed the cap.
pub(crate) fn read_whole_file(mut reader: impl Read + Seek, limit: usize) -> Result<Vec<u8>, MediaError> {
    let len64 = reader.seek(std::io::SeekFrom::End(0)).map_err(MediaError::io)?;
    reader.seek(std::io::SeekFrom::Start(0)).map_err(MediaError::io)?;
    if len64 > limit as u64 {
        return Err(MediaError::LimitExceeded(format!(
            "whole-file preload is {len64} bytes (max {limit})"
        )));
    }
    let Ok(len) = usize::try_from(len64) else {
        return Err(MediaError::LimitExceeded(format!(
            "whole-file preload is {len64} bytes (max {limit})"
        )));
    };
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).map_err(MediaError::io)?;
    Ok(buf)
}

/// `locale` when it names a language, otherwise `eng`. `C`, `POSIX` and `und` are not languages.
pub(crate) fn language_or_eng(locale: Option<&str>) -> String {
    let Some(tag) = locale.map(str::trim).filter(|tag| !tag.is_empty()) else {
        return "eng".to_string();
    };
    let primary = tag.split(['-', '_']).next().unwrap_or("");
    if primary.eq_ignore_ascii_case("c")
        || primary.eq_ignore_ascii_case("posix")
        || primary.eq_ignore_ascii_case("und")
    {
        "eng".to_string()
    } else {
        tag.to_string()
    }
}

/// Where the new clip's PCM feed goes, decided before the decode thread is spawned. The registry
/// is updated only after that spawn succeeds, so a failed open leaves the clip that is still open
/// (and its mixer stream) alone.
enum PendingAudio {
    /// No audio registry in this world.
    Untouched,
    /// Replace this owner's feed.
    Register(MediaFeedEntry),
    /// The new clip has no audio: drop the previous feed once the open has succeeded.
    Clear,
}

/// Opens `clip` (a `.video.ron` or media file, and the seconds to seek to) into `state`. Does not
/// touch the playlist. `playing` and `clear_color` change only after the file is open: a later
/// playlist entry that fails to open must not replace the clip that is still on screen.
fn open_clip(
    entity: Entity,
    player: &VideoPlayer,
    state: &mut VideoPlayerState,
    files: &MediaFiles,
    settings: &MediaSettings,
    audio: Option<&mut MediaAudioRegistry>,
    clip: (&str, f64),
) -> Result<(), MediaError> {
    let (source, phase_offset) = clip;
    let (file, def) = files.resolve(source)?;
    let (volume, loop_mode) = match &def {
        Some(d) => asset_playback(player.volume, player.loop_mode, d),
        None => (finite_volume(player.volume), player.loop_mode),
    };
    let clear = def.as_ref().map(|d| d.clear_color);
    let preload = def.as_ref().map(|d| d.preload).unwrap_or(Preload::Stream);
    let reader: Box<dyn MediaRead> = if preload == Preload::WholeFile {
        let bytes = read_whole_file(files.open(&file)?, whole_file_limit())?;
        Box::new(std::io::Cursor::new(bytes))
    } else {
        files.open(&file)?
    };
    let disabled = state.audio_disabled;
    let clock = if disabled && player.clock == ClockMode::Audio {
        ClockMode::Game
    } else {
        player.clock
    };
    let choice = if disabled {
        AudioChoice::None
    } else {
        match player.audio_track {
            Some(i) => AudioChoice::Index(i),
            None => AudioChoice::Default,
        }
    };
    let opts = SessionOptions {
        caps: settings.caps,
        audio: choice,
        language: settings.language.clone(),
    };
    let mut session = Session::open(reader, &opts, None)?;
    let cues = session.has_text().then(crate::cues::CueStore::default);
    session.set_cue_store(cues.clone());
    let have_registry = audio.is_some();
    let pending = if !have_registry {
        PendingAudio::Untouched
    } else if let Some((channels, rate)) = session.audio_format() {
        // A channel count outside 1..=8 gets no feed: the clip plays silent (fix-audio1 finding 8).
        let frame_ns = session.info().video_track().and_then(|t| t.default_duration_ns);
        let ring_frames = pcm_ring_frames(rate, channels, frame_ns, settings.queue_frames);
        if let Some((producer, consumer)) = PcmFeed::channel(channels, ring_frames) {
            session.set_audio_sink(Some(Box::new(PcmSink::new(producer))));
            let bus = def.map_or_else(|| "sfx".to_string(), |d| d.audio_bus);
            PendingAudio::Register(MediaFeedEntry {
                owner: entity.to_bits(),
                bus,
                consumer,
                volume,
                sample_rate: rate,
            })
        } else {
            tracing::warn!(target: "regia_media", channels, "audio feed refused: channel count outside 1..=8");
            PendingAudio::Clear
        }
    } else {
        PendingAudio::Clear
    };
    let info = session.info().clone();
    let feed: Box<dyn FrameFeed> = if clock == ClockMode::Lockstep {
        Box::new(session)
    } else if let Some(budget) = settings.ring_budget {
        Box::new(crate::ring::RingFeed::spawn(
            session,
            crate::ring::RingBudget {
                max_frames: budget.max_frames.min(settings.queue_frames.max(1)),
                ..budget
            },
        )?)
    } else {
        Box::new(WorkerFeed::spawn(session, settings.queue_frames)?)
    };
    if let Some(registry) = audio {
        match pending {
            PendingAudio::Register(entry) => registry.register(entry),
            PendingAudio::Clear => registry.unregister(entity.to_bits()),
            PendingAudio::Untouched => {}
        }
    }
    {
        let core = state.core_mut();
        // A rate of `0.0` is pause (spec 50 § 2.2): the open waits in Ready. A rate outside
        // `0.25..=4.0` keeps the previous rate, with the same warning a later property edit gives.
        let paused_by_rate = player.rate == 0.0;
        if !paused_by_rate && core.set_rate(player.rate).is_err() {
            tracing::warn!(rate = player.rate, "video rate outside 0.25..=4.0 ignored");
        }
        let autoplay = player.autoplay && !paused_by_rate;
        core.open(feed, info, clock, loop_mode, autoplay, phase_offset);
        if let Some(store) = cues {
            core.set_cues(store);
        }
    }
    if let Some(color) = clear {
        state.clear_color = color;
    }
    state.playing = Some(source.to_string());
    Ok(())
}

/// Opens the next playlist entry, skipping entries that fail. `initial` is the open of the list
/// itself: an empty list fails the player, and only those entries seek to
/// [`VideoPlayer::phase_offset`] (later entries start at 0). A later end with no entry left emits
/// `ListEnded` once.
fn open_playlist_entries(
    entity: Entity,
    player: &VideoPlayer,
    state: &mut VideoPlayerState,
    files: &MediaFiles,
    settings: &MediaSettings,
    audio: &mut Option<&mut MediaAudioRegistry>,
    initial: bool,
) {
    loop {
        let path = {
            let Some(run) = state.playlist.as_mut() else {
                return;
            };
            run.cursor
                .next_entry()
                .and_then(|index| run.entries.get(index).cloned())
        };
        let Some(path) = path else {
            if initial {
                state.core_mut().fail_open(MediaError::invalid("empty playlist"));
            } else if !state.list_finished {
                state.list_finished = true;
                state.core_mut().note_list_ended();
            }
            return;
        };
        match open_clip(
            entity,
            player,
            state,
            files,
            settings,
            audio.as_deref_mut(),
            (&path, if initial { player.phase_offset } else { 0.0 }),
        ) {
            Ok(()) => {
                if let Some(run) = state.playlist.as_mut() {
                    run.cursor.opened();
                }
                return;
            }
            Err(e) => {
                state.core_mut().note_open_failed(&e);
                let all = state.playlist.as_mut().is_some_and(|run| run.cursor.failed());
                if all {
                    state.core_mut().fail_playlist(e);
                    return;
                }
            }
        }
    }
}

/// `Update`, after [`tick_players`]: an ended playlist entry opens the next one. The decode pool
/// is not checked again for this player; the slot it already held is the one the next entry uses.
pub(crate) fn advance_playlists(
    mut players: Query<(Entity, &VideoPlayer, &mut VideoPlayerState)>,
    files: Option<Res<MediaFiles>>,
    settings: Res<MediaSettings>,
    mut audio: Option<ResMut<MediaAudioRegistry>>,
) {
    let default_files = MediaFiles::default();
    let files = files.as_deref().unwrap_or(&default_files);
    for (entity, player, mut state) in &mut players {
        if state.playlist.is_none() || state.list_finished {
            continue;
        }
        if state.core().state() != PlayState::Ended {
            continue;
        }
        if state.opened.as_deref() != Some(player.source.as_str()) {
            continue;
        }
        let mut slot = audio.as_deref_mut();
        open_playlist_entries(entity, player, &mut state, files, &settings, &mut slot, false);
    }
}

pub(crate) fn open_players(
    mut players: Query<(Entity, Ref<VideoPlayer>, &mut VideoPlayerState)>,
    files: Option<Res<MediaFiles>>,
    settings: Res<MediaSettings>,
    mut audio: Option<ResMut<MediaAudioRegistry>>,
) {
    let default_files = MediaFiles::default();
    let files = files.as_deref().unwrap_or(&default_files);
    let mut open = players
        .iter()
        .filter(|(_, _, s)| !matches!(s.core().state(), PlayState::Closed | PlayState::Failed))
        .count();
    let mut decoding = players
        .iter()
        .filter(|(_, _, s)| s.core().holds_decoder())
        .count();
    for (entity, player, mut state) in &mut players {
        if player.source.is_empty() {
            continue;
        }
        if state.opened.as_deref() != Some(player.source.as_str()) {
            let was_open = !matches!(state.core().state(), PlayState::Closed | PlayState::Failed);
            let held = state.core().holds_decoder();
            open_player_pooled(
                entity,
                &player,
                &mut state,
                files,
                &settings,
                (open - usize::from(was_open), decoding - usize::from(held)),
                audio.as_deref_mut(),
            );
            if !was_open && !matches!(state.core().state(), PlayState::Closed | PlayState::Failed) {
                open += 1;
            }
            let holds = state.core().holds_decoder();
            decoding = decoding - usize::from(held) + usize::from(holds);
            continue;
        }
        let wanted = (player.rate, player.loop_mode, player.clock);
        if player.is_changed() && state.applied != Some(wanted) {
            let loop_changed = state.applied.is_none_or(|(_, mode, _)| mode != player.loop_mode);
            state.applied = Some(wanted);
            let disabled = state.audio_disabled;
            let clock = if disabled && player.clock == ClockMode::Audio {
                ClockMode::Game
            } else {
                player.clock
            };
            let core = state.core_mut();
            if apply_rate_edit(core, player.rate).is_err() {
                tracing::warn!(rate = player.rate, "video rate outside 0.25..=4.0 ignored");
            }
            // A `.video.ron` loop applies only while the component stays `Off`. Changing `loop_mode`
            // on the component replaces it, including a change back to `Off`.
            if loop_changed {
                core.set_loop(player.loop_mode);
            }
            if core.clock() != clock {
                core.set_clock(clock);
            }
        }
    }
}

/// Applies a component `rate` edit to a running player. `Err` means the rate is outside
/// `0.25..=4.0` and was ignored (the caller warns). A rate of `0.0` is pause (spec 50 § 2.2), a
/// legal value: a player that is not playing (closed, ended, failed) has nothing to pause, so its
/// `bad_state` is not an error here (it used to be warned as an out-of-range rate).
fn apply_rate_edit(core: &mut PlayerCore, rate: f32) -> Result<(), MediaError> {
    if rate == 0.0 {
        let _ = core.pause();
        return Ok(());
    }
    core.set_rate(rate)
}

/// Thermal step of the decode policy: 0 none, 1 half rate, 2 also pause off-screen-able players.
fn thermal_step(state: Option<&regia_platform::ThermalState>) -> u8 {
    use regia_platform::ThermalStatus as S;
    match state.map(|t| t.status) {
        None | Some(S::None | S::Light) => 0,
        Some(S::Moderate) => 1,
        Some(_) => 2,
    }
}

/// `Update`, before the clock: applies [`VideoVisible`] under each player's [`VisibilityPolicy`],
/// and the thermal throttle (specs/50 § 9). Fullscreen players always decode; under severe thermal
/// pressure a fullscreen player keeps decoding and logs once.
pub(crate) fn apply_visibility(
    mut players: Query<(
        &VideoPlayer,
        &mut VideoPlayerState,
        Option<&VideoVisible>,
        Option<&crate::present::VideoFullscreen>,
    )>,
    settings: Res<MediaSettings>,
    thermal: Option<Res<regia_platform::ThermalState>>,
    mut warned: Local<bool>,
) {
    let step = if settings.thermal_throttle {
        thermal_step(thermal.as_deref())
    } else {
        0
    };
    if step < 2 {
        *warned = false;
    }
    for (player, mut state, visible, full) in &mut players {
        let core = state.core_mut();
        if full.is_some() {
            core.set_decode_paused(false, false);
            core.set_half_rate(false);
            if step >= 2 && !*warned && core.holds_decoder() {
                *warned = true;
                tracing::warn!(target: "regia_media", "thermal pressure: fullscreen video keeps decoding");
            }
            continue;
        }
        let seen = visible.is_none_or(|v| v.0);
        let (pause, freeze) = match player.visibility {
            VisibilityPolicy::Always => (false, false),
            VisibilityPolicy::PauseDecode => (!seen || step >= 2, false),
            VisibilityPolicy::PauseAll => (!seen, !seen),
        };
        core.set_decode_paused(pause, freeze);
        core.set_half_rate(step >= 1);
    }
}

/// What one player's clock can read on one tick: the inputs of [`ClockSources::input`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ClockSources {
    /// The player's clock mode.
    pub mode: ClockMode,
    /// The player's playback rate.
    pub rate: f32,
    /// The fixed lockstep step (`MediaSettings::lockstep_dt_ns`).
    pub lockstep_dt_ns: u64,
    /// This frame's game-time delta.
    pub game_dt_ns: u64,
    /// The externally driven time, when a host sets one.
    pub external_ns: Option<u64>,
    /// The player's position (the `External` fallback).
    pub position_ns: i64,
    /// The player's own audio stream: frames the output mixed and the mix rate.
    pub stream: Option<(u64, u32)>,
    /// The global audio clock: frames consumed and the sample rate.
    pub global_clock: Option<(u64, u32)>,
}

impl ClockSources {
    /// The `ClockInput` for one tick (spec 50 § 2.5). Outside the audible rates the
    /// audio feed is muted, so an audio-clocked player follows game time until the rate
    /// returns. Otherwise the player's own stream (the frames the output mixed) wins,
    /// then the global audio clock, then game time. Pure so the clock-choice rules are
    /// unit-testable without real time on the worker path.
    pub(crate) fn input(&self) -> ClockInput {
        match self.mode {
            ClockMode::Lockstep => ClockInput::Step {
                dt_ns: self.lockstep_dt_ns,
            },
            ClockMode::Game => ClockInput::Elapsed {
                dt_ns: self.game_dt_ns,
            },
            ClockMode::External => ClockInput::Time {
                t_ns: self.external_ns.unwrap_or(self.position_ns.max(0) as u64),
            },
            ClockMode::Audio if !MEDIA_FEED_RATES.contains(&self.rate) => ClockInput::Elapsed {
                dt_ns: self.game_dt_ns,
            },
            ClockMode::Audio => match (self.stream, self.global_clock) {
                (Some((frames, sample_rate)), _) | (None, Some((frames, sample_rate))) => {
                    ClockInput::Audio { frames, sample_rate }
                }
                (None, None) => ClockInput::Elapsed {
                    dt_ns: self.game_dt_ns,
                },
            },
        }
    }
}

pub(crate) fn tick_players(
    mut players: Query<(Entity, &mut VideoPlayerState)>,
    settings: Res<MediaSettings>,
    time: Option<Res<Time>>,
    clock: Option<Res<AudioClock>>,
    streams: Option<Res<MediaAudioRegistry>>,
) {
    let _span = tracing::trace_span!("media_update").entered();
    let dt_ns = time
        .as_deref()
        .map_or(0, |t| t.delta().as_nanos().min(u128::from(u64::MAX)) as u64);
    // The gameplay pause holds every player's clock, audio and lockstep included (ADR-10105).
    let game_paused = time.as_deref().is_some_and(|t| t.paused);
    for (entity, mut state) in &mut players {
        let external = state.external_time_ns;
        let stream = streams.as_deref().and_then(|r| r.stream(entity.to_bits()));
        let core = state.core_mut();
        core.set_game_held(game_paused);
        if let Some(s) = stream {
            core.set_underruns(s.underruns());
        }
        let input = ClockSources {
            mode: core.clock(),
            rate: core.rate(),
            lockstep_dt_ns: settings.lockstep_dt_ns,
            game_dt_ns: dt_ns,
            external_ns: external,
            position_ns: core.position_ns(),
            stream: stream.map(|s| (s.frames(), s.mix_rate())),
            global_clock: clock.as_deref().map(|c| (c.frames(), c.sample_rate())),
        }
        .input();
        core.tick(input);
        if let Some(s) = stream {
            // The transport reaches the audio feed (concurrency audit 11-03 audio H2, M3): a
            // player that is not playing holds its stream, and the stream plays at its rate.
            s.set_paused(core.state() != PlayState::Playing || core.game_held());
            s.set_rate(core.rate());
        }
        if core.take_slow_warning() {
            let decode_ms = core.stats().decode_ms_ema;
            tracing::warn!(
                clip = state.opened().unwrap_or_default(),
                decode_ms,
                "video decode slower than the frame rate for {SLOW_DECODE_FRAMES} pictures in a row"
            );
        }
    }
}

/// Whether a player in `state` owns an open session (and so possibly a registered audio feed).
fn has_live_session(state: PlayState) -> bool {
    !matches!(state, PlayState::Closed | PlayState::Failed)
}

pub(crate) fn emit_video_events(
    mut players: Query<(Entity, &mut VideoPlayerState)>,
    mut writer: MessageWriter<VideoMessage>,
    mut registry: ResMut<VideoRegistry>,
    mut streams: Option<ResMut<MediaAudioRegistry>>,
) {
    let mut all: Vec<(Entity, Vec<VideoEvent>, bool)> = players
        .iter_mut()
        .map(|(e, mut s)| {
            let core = s.core_mut();
            let events = core.drain_events();
            (e, events, !has_live_session(core.state()))
        })
        .filter(|(_, ev, _)| !ev.is_empty())
        .collect();
    all.sort_by_key(|(e, _, _)| e.to_bits());
    for (entity, events, no_session) in all {
        for event in events {
            // A reopen closes the old session and registers the new feed in the same frame: the old
            // session's `Closed` must not tear the new feed down.
            if event == VideoEvent::Closed
                && no_session
                && let Some(streams) = streams.as_deref_mut()
            {
                streams.unregister(entity.to_bits());
            }
            registry.push(entity, event.clone());
            writer.write(VideoMessage { entity, event });
        }
    }
}

/// Caption ids of the text cues on screen, by (player entity bits, track, start ns).
#[derive(Resource, Debug, Default)]
pub struct VideoCaptionRoutes {
    live: BTreeMap<(u64, u32, i64), regia_app::caption::CaptionId>,
    sent: u64,
}

impl VideoCaptionRoutes {
    /// Captions started so far.
    pub fn sent(&self) -> u64 {
        self.sent
    }

    /// Captions on the board now.
    pub fn live(&self) -> usize {
        self.live.len()
    }
}

/// `PostUpdate`, after the events: routes text cues to the caption board (specs/50 § 2.8, spec 56)
/// as `regia_app::caption` messages: `CaptionStart` (source `Video`, kind `Speech`, the track's
/// language, the cue's length as the planned time) on `TextCue`, `CaptionStop` on `TextCueEnd`.
/// Does nothing unless the caption messages are registered (`regia_ui::captions::CaptionPlugin`).
pub(crate) fn route_cues_to_captions(
    mut messages: MessageReader<VideoMessage>,
    players: Query<&VideoPlayerState>,
    mut routes: ResMut<VideoCaptionRoutes>,
    ids: Option<ResMut<regia_app::caption::CaptionIds>>,
    starts: Option<ResMut<Messages<regia_app::caption::CaptionStart>>>,
    stops: Option<ResMut<Messages<regia_app::caption::CaptionStop>>>,
) {
    use regia_app::caption::{
        CaptionKind, CaptionPiece, CaptionSource, CaptionStart, CaptionStop, CaptionStyleHint, CaptionTime,
    };
    let (Some(mut ids), Some(mut starts), Some(mut stops)) = (ids, starts, stops) else {
        messages.clear();
        return;
    };
    for m in messages.read() {
        let owner = m.entity.to_bits();
        match &m.event {
            VideoEvent::TextCue {
                track,
                start,
                end,
                text,
            } => {
                let start_ns = (start * 1e9).round() as i64;
                let id = ids.next_id();
                let locale = players
                    .get(m.entity)
                    .ok()
                    .and_then(|s| {
                        s.core()
                            .info()
                            .and_then(|i| i.tracks.iter().find(|t| t.index == *track))
                            .map(|t| t.language.clone())
                    })
                    .unwrap_or_else(|| "und".to_string());
                let planned_us = end.map_or(0, |e| ((e - start).max(0.0) * 1e6).round() as u64);
                if let Some(old) = routes.live.insert((owner, *track, start_ns), id) {
                    stops.write(CaptionStop { id: old });
                }
                routes.sent += 1;
                starts.write(CaptionStart {
                    id,
                    source: CaptionSource::Video,
                    kind: CaptionKind::Speech,
                    locale,
                    pieces: vec![CaptionPiece::literal(0, text.clone())],
                    speaker: None,
                    priority: 0,
                    start: CaptionTime::Now,
                    planned_us,
                    keep_breaks: true,
                    hint: CaptionStyleHint::default(),
                });
            }
            VideoEvent::TextCueEnd { track, start } => {
                let start_ns = (start * 1e9).round() as i64;
                if let Some(id) = routes.live.remove(&(owner, *track, start_ns)) {
                    stops.write(CaptionStop { id });
                }
            }
            _ => {}
        }
    }
}

// ---- command layer for the agent protocol -------------------------------------------------------

/// Finds a player by entity bits (decimal) or `Name`.
pub fn find_player(world: &mut World, key: &str) -> Result<Entity, MediaError> {
    let mut q = world.query::<(Entity, &VideoPlayer, Option<&Name>)>();
    let bits = key.parse::<u64>().ok();
    q.iter(world)
        .find(|(e, _, name)| Some(e.to_bits()) == bits || name.is_some_and(|n| n.as_str() == key))
        .map(|(e, _, _)| e)
        .ok_or_else(|| MediaError::PlayerNotFound(key.to_string()))
}

/// Runs `f` on a player's core.
pub fn with_core<T>(
    world: &mut World,
    entity: Entity,
    f: impl FnOnce(&mut PlayerCore) -> Result<T, MediaError>,
) -> Result<T, MediaError> {
    let mut state = world
        .get_mut::<VideoPlayerState>(entity)
        .ok_or_else(|| MediaError::PlayerNotFound(entity.to_bits().to_string()))?;
    f(state.core_mut())
}

/// A status snapshot (`video.status`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayerStatus {
    /// Entity bits.
    pub player: u64,
    /// Source path.
    pub source: String,
    /// State name.
    pub state: String,
    /// Position in seconds.
    pub position: f64,
    /// Duration in seconds.
    pub duration: Option<f64>,
    /// Presented frame index.
    pub frame_index: Option<u64>,
    /// Rate.
    pub rate: f32,
    /// Loop policy.
    pub loop_mode: LoopMode,
    /// Wraps so far.
    pub loops: u32,
    /// Clock mode name.
    pub clock: String,
    /// Selected audio track.
    pub audio_track: Option<u32>,
    /// Counters.
    pub stats: crate::player::VideoStats,
    /// Last error code.
    pub last_error: Option<String>,
    /// Decoding paused by visibility or thermal policy (ADR-2410).
    pub decode_paused: bool,
    /// Thermal half-rate presentation is on.
    pub half_rate: bool,
    /// Text cues read so far.
    pub text_cues: usize,
    /// Text cues on screen now.
    pub active_cues: usize,
}

/// Status of every player, by entity bits.
pub fn statuses(world: &mut World) -> Vec<PlayerStatus> {
    let mut q = world.query::<(Entity, &VideoPlayer, &VideoPlayerState)>();
    let mut out: Vec<PlayerStatus> = q
        .iter(world)
        .map(|(e, p, s)| {
            let core = s.core();
            PlayerStatus {
                player: e.to_bits(),
                source: p.source.clone(),
                state: core.state().name().to_string(),
                position: core.position(),
                duration: core.duration(),
                frame_index: core.frame_index(),
                rate: core.rate(),
                loop_mode: core.loop_mode(),
                loops: core.loops_done(),
                clock: core.clock().name().to_string(),
                audio_track: p.audio_track,
                stats: core.stats(),
                last_error: core.last_error().map(|e| e.code().to_string()),
                decode_paused: core.decode_paused(),
                half_rate: core.half_rate(),
                text_cues: core.cue_store().map_or(0, crate::cues::CueStore::len),
                active_cues: core.active_cues().len(),
            }
        })
        .collect();
    out.sort_by_key(|s| s.player);
    out
}

/// `video.frame_hash`: FNV-1a of the presented frame's RGBA8 (CPU reference).
pub fn frame_hash(
    world: &mut World,
    entity: Entity,
    region: Option<Region>,
) -> Result<(u64, u32, u32, f64), MediaError> {
    let mut state = world
        .get_mut::<VideoPlayerState>(entity)
        .ok_or_else(|| MediaError::PlayerNotFound(entity.to_bits().to_string()))?;
    let (w, h, rgba) = state.frame_rgba();
    let time = state
        .core_mut()
        .presented()
        .map_or(0.0, |p| p.pts_ns as f64 / 1e9);
    Ok((rgba_hash(&rgba, w, h, region), w, h, time))
}

/// `video.open`: sets the source (and options) and opens now.
pub fn command_open(world: &mut World, entity: Entity, player: VideoPlayer) -> Result<PlayState, MediaError> {
    let files = world.get_resource::<MediaFiles>().cloned().unwrap_or_default();
    let settings = world.get_resource::<MediaSettings>().cloned().unwrap_or_default();
    let open = statuses(world)
        .iter()
        .filter(|s| s.player != entity.to_bits() && s.state != "closed" && s.state != "failed")
        .count();
    let decoding = {
        let mut q = world.query::<(Entity, &VideoPlayerState)>();
        q.iter(world)
            .filter(|(e, s)| *e != entity && s.core().holds_decoder())
            .count()
    };
    let mut registry = world.remove_resource::<MediaAudioRegistry>();
    let result = match world.get_entity_mut(entity) {
        Ok(mut e) => {
            e.insert(player.clone());
            match e.get_mut::<VideoPlayerState>() {
                Some(mut state) => {
                    open_player_pooled(
                        entity,
                        &player,
                        &mut state,
                        &files,
                        &settings,
                        (open, decoding),
                        registry.as_mut(),
                    );
                    Ok(state.core_mut().state())
                }
                None => Err(MediaError::PlayerNotFound(entity.to_bits().to_string())),
            }
        }
        Err(_) => Err(MediaError::PlayerNotFound(entity.to_bits().to_string())),
    };
    if let Some(r) = registry {
        world.insert_resource(r);
    }
    result
}

/// `video.select_track`: reopens `player` with its new track choice at
/// [`VideoPlayer::phase_offset`] (the caller points it at the current position for the call and
/// restores the game's initial seek after, so the component keeps its documented meaning).
///
/// A running `.videolist.ron` (the same list still open, an entry playing) reopens only the entry
/// that plays now and keeps the list's place. Anything else is [`command_open`]. A failed reopen
/// fails the player, as [`command_open`] does.
pub fn command_reopen_tracks(
    world: &mut World,
    entity: Entity,
    player: VideoPlayer,
) -> Result<PlayState, MediaError> {
    let entry = world.get::<VideoPlayerState>(entity).and_then(|state| {
        let same_list = state.playlist.is_some() && state.opened.as_deref() == Some(player.source.as_str());
        if same_list { state.playing.clone() } else { None }
    });
    let Some(entry) = entry else {
        return command_open(world, entity, player);
    };
    let files = world.get_resource::<MediaFiles>().cloned().unwrap_or_default();
    let settings = world.get_resource::<MediaSettings>().cloned().unwrap_or_default();
    let mut registry = world.remove_resource::<MediaAudioRegistry>();
    let result = match world.get_entity_mut(entity) {
        Ok(mut e) => {
            e.insert(player.clone());
            match e.get_mut::<VideoPlayerState>() {
                Some(mut state) => {
                    state.applied = Some((player.rate, player.loop_mode, player.clock));
                    state.rgba = None;
                    if let Err(err) = open_clip(
                        entity,
                        &player,
                        &mut state,
                        &files,
                        &settings,
                        registry.as_mut(),
                        (&entry, player.phase_offset),
                    ) {
                        state.core_mut().fail_open(err);
                    }
                    Ok(state.core_mut().state())
                }
                None => Err(MediaError::PlayerNotFound(entity.to_bits().to_string())),
            }
        }
        Err(_) => Err(MediaError::PlayerNotFound(entity.to_bits().to_string())),
    };
    if let Some(r) = registry {
        world.insert_resource(r);
    }
    result
}

/// `video.seek` by time or frame.
pub fn command_seek(
    world: &mut World,
    entity: Entity,
    time: Option<f64>,
    frame: Option<u64>,
    mode: SeekMode,
) -> Result<PlayState, MediaError> {
    with_core(world, entity, |c| {
        match (time, frame) {
            (_, Some(f)) => c.seek_frame(f, mode)?,
            (Some(t), None) => c.seek_time(t, mode)?,
            (None, None) => c.seek_time(0.0, mode)?,
        }
        Ok(c.state())
    })
}

/// `video.probe` on a source path (no player needed).
pub fn probe_source(world: &World, source: &str) -> Result<MediaInfo, MediaError> {
    let files = world.get_resource::<MediaFiles>().cloned().unwrap_or_default();
    let (file, _) = files.resolve(source)?;
    container::probe(files.open(&file)?)
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    /// An audio-clocked player at `rate` with an own stream and a global clock.
    fn audio_sources(rate: f32, stream: Option<(u64, u32)>, global: Option<(u64, u32)>) -> ClockSources {
        ClockSources {
            mode: ClockMode::Audio,
            rate,
            lockstep_dt_ns: 0,
            game_dt_ns: 16_666_666,
            external_ns: None,
            position_ns: 0,
            stream,
            global_clock: global,
        }
    }

    #[test]
    fn audio_clock_follows_game_time_outside_the_audible_rates() {
        // Spec 50 § 2.5: outside 0.5..=2.0 the feed is muted and the clock follows
        // game time, even when a stream exists.
        for rate in [0.25_f32, 0.49, 2.01, 4.0] {
            assert_eq!(
                audio_sources(rate, Some((48000, 48_000)), Some((96000, 48_000))).input(),
                ClockInput::Elapsed { dt_ns: 16_666_666 },
                "rate {rate}"
            );
        }
    }

    #[test]
    fn audio_clock_prefers_the_own_stream_then_the_global_clock() {
        // An audible rate: the player's own stream (the frames the output mixed) wins.
        assert_eq!(
            audio_sources(1.0, Some((48000, 48_000)), Some((96000, 48_000))).input(),
            ClockInput::Audio {
                frames: 48000,
                sample_rate: 48_000,
            }
        );
        // No own stream: the global audio clock is the fallback.
        assert_eq!(
            audio_sources(1.0, None, Some((96000, 48_000))).input(),
            ClockInput::Audio {
                frames: 96000,
                sample_rate: 48_000,
            }
        );
        // Neither: the player follows game time.
        assert_eq!(
            audio_sources(1.0, None, None).input(),
            ClockInput::Elapsed { dt_ns: 16_666_666 }
        );
    }

    /// Review rev-kimi-13 (19-50 finding 6): a component rate edit to `0.0` is pause (spec 50
    /// § 2.2), a legal value. On a player that is not playing (closed, ended, failed) there is
    /// nothing to pause; the edit must not be reported as a rate outside `0.25..=4.0`.
    #[test]
    fn a_rate_edit_to_zero_is_never_an_out_of_range_rate() {
        let mut closed = PlayerCore::new();
        assert_eq!(closed.state(), PlayState::Closed);
        assert!(
            apply_rate_edit(&mut closed, 0.0).is_ok(),
            "rate 0.0 on a closed player is a legal pause, not an out-of-range rate"
        );
        assert_eq!(closed.state(), PlayState::Closed, "nothing opened or played");
        // Out-of-range rates still report, whatever the state.
        for rate in [0.1_f32, 4.5, -1.0, f32::NAN] {
            assert!(apply_rate_edit(&mut closed, rate).is_err(), "rate {rate}");
        }
        assert!(apply_rate_edit(&mut closed, 2.0).is_ok());
        assert_eq!(closed.rate(), 2.0);
    }

    #[test]
    fn asset_defaults_apply_only_at_the_component_defaults() {
        let def = VideoSource {
            volume: 0.25,
            loop_mode: LoopMode::Loop,
            ..VideoSource::new("a.webm")
        };
        assert_eq!(asset_playback(1.0, LoopMode::Off, &def), (0.25, LoopMode::Loop));
        assert_eq!(asset_playback(0.5, LoopMode::Off, &def), (0.5, LoopMode::Loop));
        assert_eq!(
            asset_playback(1.0, LoopMode::Count(2), &def),
            (0.25, LoopMode::Count(2))
        );
    }

    #[test]
    fn whole_file_read_refuses_over_the_cap_before_allocating_it() {
        let cap = if cfg!(target_os = "android") { 32 } else { 64 } * 1024 * 1024;
        assert_eq!(whole_file_limit(), cap);
        let small = read_whole_file(std::io::Cursor::new(vec![1, 2, 3, 4]), 8).expect("under");
        assert_eq!(small, vec![1, 2, 3, 4]);
        let err = read_whole_file(std::io::Cursor::new(vec![0; 10]), 8).expect_err("over");
        assert_eq!(err.code(), "limit_exceeded");
        assert!(err.to_string().contains("max 8"));
    }

    #[test]
    fn language_falls_back_to_eng_for_missing_c_and_posix() {
        assert_eq!(language_or_eng(None), "eng");
        assert_eq!(language_or_eng(Some("")), "eng");
        assert_eq!(language_or_eng(Some("C")), "eng");
        assert_eq!(language_or_eng(Some("POSIX")), "eng");
        assert_eq!(language_or_eng(Some("und")), "eng");
        assert_eq!(language_or_eng(Some("en-US")), "en-US");
        assert_eq!(language_or_eng(Some("fra")), "fra");
    }

    #[test]
    fn asset_volume_nan_does_not_pass_through() {
        let def = VideoSource {
            volume: f32::NAN,
            ..VideoSource::new("a.webm")
        };
        let (volume, _) = asset_playback(1.0, LoopMode::Off, &def);
        assert!(volume.is_finite(), "asset NaN volume reached playback: {volume}");
    }

    /// A reader whose `seek(End)` reports a short length, then whose `read` yields a longer body.
    struct ShortClaim {
        claimed: u64,
        pos: u64,
        body: Vec<u8>,
    }

    impl std::io::Read for ShortClaim {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.body.len() as u64 {
                return Ok(0);
            }
            let start = self.pos as usize;
            let n = buf.len().min(self.body.len() - start);
            buf[..n].copy_from_slice(&self.body[start..start + n]);
            self.pos += n as u64;
            Ok(n)
        }
    }

    impl std::io::Seek for ShortClaim {
        fn seek(&mut self, from: std::io::SeekFrom) -> std::io::Result<u64> {
            let next = match from {
                std::io::SeekFrom::Start(pos) => pos,
                std::io::SeekFrom::End(delta) => {
                    if delta >= 0 {
                        self.claimed.saturating_add(delta as u64)
                    } else {
                        self.claimed.saturating_sub(delta.unsigned_abs())
                    }
                }
                std::io::SeekFrom::Current(delta) => {
                    if delta >= 0 {
                        self.pos.saturating_add(delta as u64)
                    } else {
                        self.pos.saturating_sub(delta.unsigned_abs())
                    }
                }
            };
            self.pos = next;
            Ok(self.pos)
        }
    }

    #[test]
    fn whole_file_read_stops_at_the_length_it_checked() {
        let body = vec![9u8; 100];
        let got = read_whole_file(
            ShortClaim {
                claimed: 4,
                pos: 0,
                body,
            },
            8,
        )
        .expect("claimed length is under the cap");
        assert!(
            got.len() <= 8,
            "read {} bytes after a length check of 4 (cap 8)",
            got.len()
        );
    }

    #[test]
    fn non_finite_volume_does_not_reach_playback() {
        let nan_asset = VideoSource {
            volume: f32::NAN,
            ..VideoSource::new("a.webm")
        };
        let (volume, loop_mode) = asset_playback(1.0, LoopMode::Off, &nan_asset);
        assert!(volume.is_finite(), "asset NaN volume reached playback: {volume}");
        assert_eq!(loop_mode, LoopMode::Off);
        let inf_asset = VideoSource {
            volume: f32::INFINITY,
            loop_mode: LoopMode::Loop,
            ..VideoSource::new("a.webm")
        };
        let (volume, loop_mode) = asset_playback(1.0, LoopMode::Off, &inf_asset);
        assert!(volume.is_finite(), "asset Inf volume reached playback: {volume}");
        assert_eq!(loop_mode, LoopMode::Loop);
        let (volume, _) = asset_playback(f32::NAN, LoopMode::Off, &VideoSource::new("a.webm"));
        assert!(
            volume.is_finite(),
            "component NaN volume reached playback: {volume}"
        );
        let (volume, _) = asset_playback(f32::NEG_INFINITY, LoopMode::Off, &VideoSource::new("a.webm"));
        assert!(
            volume.is_finite(),
            "component -Inf volume reached playback: {volume}"
        );
        let quiet = VideoSource {
            volume: 0.25,
            loop_mode: LoopMode::Loop,
            ..VideoSource::new("a.webm")
        };
        assert_eq!(asset_playback(1.0, LoopMode::Off, &quiet).0, 0.25);
        assert_eq!(asset_playback(0.5, LoopMode::Off, &quiet).0, 0.5);
    }

    /// Deep-zai-media finding 3: the ring covers half a second plus the decode-ahead window.
    #[test]
    fn pcm_ring_covers_the_decode_ahead_window() {
        // 30 fps, queue 4: 0.5 s + 6 x 33.3 ms = 700 ms at 48 kHz.
        assert_eq!(pcm_ring_frames(48_000, 2, Some(33_333_333), 4), 24_000 + 9_599);
        // 4 fps, queue 4: 0.5 s + 6 x 250 ms = 2 s.
        assert_eq!(pcm_ring_frames(48_000, 2, Some(250_000_000), 4), 96_000);
        // Queue sizes clamp to 1..=8 as the decode queue does.
        assert_eq!(
            pcm_ring_frames(48_000, 2, Some(33_333_333), 0),
            pcm_ring_frames(48_000, 2, Some(33_333_333), 1)
        );
        assert_eq!(
            pcm_ring_frames(48_000, 2, Some(33_333_333), 99),
            pcm_ring_frames(48_000, 2, Some(33_333_333), 8)
        );
        // KI-AU-028 (ADR-10070): no declared period is the 4 s ring, not a 30 fps guess, so a 2 fps
        // clip without `DefaultDuration` (0.5 s + 6 x 500 ms = 3.5 s ahead) fits.
        assert_eq!(pcm_ring_frames(48_000, 2, None, 4), 4 * 48_000);
        assert!(pcm_ring_frames(48_000, 2, None, 4) >= pcm_ring_frames(48_000, 2, Some(500_000_000), 4));
        assert_eq!(
            pcm_ring_frames(48_000, 2, None, 0),
            pcm_ring_frames(48_000, 2, None, 8)
        );
        // A hostile frame period is capped at 4 s; a zero rate still gets a ring.
        assert_eq!(pcm_ring_frames(48_000, 2, Some(i64::MAX as u64), 8), 4 * 48_000);
        assert_eq!(pcm_ring_frames(0, 2, Some(40_000_000), 4), 1);
    }

    /// ADR-7635 (v0.5.1 review M1): the file sets the rate, the channel count and the frame
    /// period, so the ring is capped by bytes too. Without the cap, a 768 kHz 8-channel track with
    /// a 1 s video frame period sized a 4 s ring: 768000 x 4 x 8 samples of 4 bytes, about 98 MB
    /// per player.
    #[test]
    fn pcm_ring_allocation_is_capped_by_bytes() {
        let bytes = |rate, channels: u8, frame_ns, queue| {
            pcm_ring_frames(rate, channels, frame_ns, queue) * usize::from(channels) * 4
        };
        // The worst case the container accepts: the 768 kHz maximum rate, 8 channels, the
        // largest queue and a frame period of 1 s or more.
        for frame_ns in [Some(1_000_000_000), Some(10_000_000_000), Some(u64::MAX)] {
            let worst = bytes(768_000, 8, frame_ns, 8);
            assert!(
                worst <= PCM_RING_MAX_BYTES,
                "a 768 kHz 8-channel ring allocates {worst} bytes, over the {PCM_RING_MAX_BYTES} cap"
            );
        }
        for channels in 1..=8u8 {
            for rate in [44_100, 96_000, 192_000, 384_000, 768_000] {
                for frame_ns in [None, Some(16_666_667), Some(250_000_000), Some(2_000_000_000)] {
                    let b = bytes(rate, channels, frame_ns, 8);
                    assert!(b <= PCM_RING_MAX_BYTES, "{rate} Hz x {channels}: {b} bytes");
                }
            }
        }
        // The cap is a ceiling, not a target: the capped ring still holds every whole frame
        // that fits (no rounding to zero for wide layouts).
        assert_eq!(
            pcm_ring_frames(768_000, 8, Some(1_000_000_000), 8),
            PCM_RING_MAX_BYTES / (8 * 4)
        );
    }

    /// ADR-7635: real content keeps today's ring. 48 kHz stereo at 24, 30 and 60 fps with the
    /// default queue of 4 gets 0.5 s plus six pictures of audio, the same sizes as before the cap;
    /// every 48 kHz layout up to 8 channels at a 4 fps slideshow keeps its 2 s, and at the 4 s
    /// ceiling 7.1 still fits.
    #[test]
    fn pcm_ring_size_is_unchanged_for_real_content() {
        // 24 fps: 6 x 41.667 ms = 250 ms; 30 fps: 6 x 33.3 ms (integer floor); 60 fps: 100 ms.
        assert_eq!(pcm_ring_frames(48_000, 2, Some(41_666_667), 4), 24_000 + 12_000);
        assert_eq!(pcm_ring_frames(48_000, 2, Some(33_333_333), 4), 24_000 + 9_599);
        assert_eq!(pcm_ring_frames(48_000, 2, Some(16_666_667), 4), 24_000 + 4_800);
        // 44.1 kHz stereo at 30 fps, and the undeclared-period default: the 4 s ring since
        // KI-AU-028 (ADR-10070; it was a 30 fps guess, 24_000 + 9_599, which a low frame rate
        // clip without `DefaultDuration` overflowed).
        assert_eq!(pcm_ring_frames(44_100, 2, Some(33_333_333), 4), 22_050 + 8_819);
        assert_eq!(pcm_ring_frames(48_000, 2, None, 4), 4 * 48_000);
        for channels in 1..=8u8 {
            assert_eq!(pcm_ring_frames(48_000, channels, Some(250_000_000), 4), 96_000);
            assert_eq!(
                pcm_ring_frames(48_000, channels, Some(1_000_000_000), 8),
                4 * 48_000
            );
        }
    }
}
