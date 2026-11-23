// SPDX-License-Identifier: MPL-2.0
//! The dialogue line player (spec 36 § 7.3, ADR-1882): plays one `.dialogue.ron` line. It selects the
//! variant for the speaker and listeners, resolves the transcript and the display text through
//! [`Localization`] with the voices' grammatical arguments, resolves the sound through the audio locale
//! chain (localized variants, spec 36 § 6), starts it on the voice player, and shows the subtitle on the
//! caption board (spec 56) as one caption whose pieces are the line's cues.
//!
//! Reconciliation with spec 56 (ADR-1882): the subtitles of a line go to the built caption presenter
//! (`regia_ui::captions`), which already does priority, wrapping, pages and layout; no second presenter
//! is built.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use regia_app::caption::{
    CaptionId, CaptionIds, CaptionKind, CaptionPiece, CaptionSource, CaptionStart, CaptionStop,
    CaptionStyleHint, CaptionText, CaptionTime,
};
use regia_app::prelude::*;
use regia_asset::dialogue::{
    DialogueAsset, DialogueFormatError, SubtitleTrack, TimedCue, VoiceAsset, expand_cues, parse_dialogue,
    parse_subtitles, parse_voice, subtitles_sidecar,
};
use regia_core::math::Vec3;
use regia_platform::{Vfs, VfsPath};
use regia_ui::captions::CaptionClock;
use regia_ui::localization::Localization;

use crate::conversation::{ConversationSettings, VoicePlayer, assets_host_root, with_voice_player};

/// Sound file extensions the library records as known sounds.
const SOUND_EXTENSIONS: [&str; 5] = ["ogg", "wav", "flac", "mp3", "opus"];

/// Where a package keeps its [`DialogueIndex`], under `assets://`.
pub const DIALOGUE_INDEX_PATH: &str = "loc/dialogue_index.ron";

/// The dialogue files of a package, written by `regia package`: an APK cannot be walked for them.
/// Derived data, rebuilt by every package; no migrator.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct DialogueIndex {
    /// Always 1.
    pub format_version: u32,
    /// The `.dialogue.ron`, `.voice.ron` and `.subs.ron` files, `assets/`-relative.
    pub files: Vec<String>,
    /// The sounds those dialogues name and their localized variants, `assets/`-relative.
    pub sounds: Vec<String>,
}

/// Everything the player reads: dialogues by name, voices by id, cue sets by sidecar path, and the
/// sound paths that exist (for localized-variant resolution).
#[derive(Resource, Debug, Clone, Default)]
pub struct DialogueLibrary {
    /// Dialogues by name.
    pub dialogues: BTreeMap<String, DialogueAsset>,
    /// Voices by id.
    pub voices: BTreeMap<String, VoiceAsset>,
    /// Cue sets by their `assets/`-relative `.subs.ron` path.
    pub subtitles: BTreeMap<String, SubtitleTrack>,
    /// `assets/`-relative sound paths found (base and `localized/<L>/` variants).
    pub sounds: BTreeSet<String>,
    /// Files that did not parse (`path:line:col: message`).
    pub errors: Vec<String>,
}

impl DialogueLibrary {
    /// Reads every `.dialogue.ron`, `.voice.ron` and `.subs.ron` under `assets` (recursively, sorted),
    /// and records sound files. A file that does not parse, or that repeats a dialogue name or voice
    /// id an earlier file defined, is listed in `errors` and skipped.
    #[must_use]
    pub fn load_dir(assets: &Path) -> Self {
        let mut files = Vec::new();
        collect(assets, assets, &mut files);
        files.sort();
        let mut library = Self::default();
        for rel in files {
            let lower = rel.to_ascii_lowercase();
            let is_sound = lower
                .rsplit_once('.')
                .is_some_and(|(_, ext)| SOUND_EXTENSIONS.contains(&ext));
            if is_sound {
                library.sounds.insert(rel);
                continue;
            }
            let Some(kind) = file_kind(&rel) else {
                continue;
            };
            match std::fs::read_to_string(assets.join(&rel)) {
                Ok(text) => library.insert_or_note(kind, &rel, &text),
                Err(err) => library.errors.push(format!("{rel}: {err}")),
            }
        }
        library
    }

    /// The index `regia package` writes for this library, which was loaded from `assets`.
    #[must_use]
    pub fn index(&self, assets: &Path) -> DialogueIndex {
        let mut files = Vec::new();
        collect(assets, assets, &mut files);
        files.retain(|rel| file_kind(rel).is_some());
        files.sort();
        let named: BTreeSet<&str> = self
            .dialogues
            .values()
            .flat_map(|d| &d.lines)
            .flat_map(|l| &l.variants)
            .map(|v| v.sound.as_str())
            .collect();
        let sounds = self
            .sounds
            .iter()
            .filter(|s| {
                let base = regia_asset::localized::localized_of(s).map_or(s.as_str(), |(_, base)| base);
                named.contains(base)
            })
            .cloned()
            .collect();
        DialogueIndex {
            format_version: 1,
            files,
            sounds,
        }
    }

    /// Loads what the package index at [`DIALOGUE_INDEX_PATH`] lists, through the VFS. `None` when there
    /// is no readable index.
    #[must_use]
    pub fn load_index(vfs: &Vfs) -> Option<Self> {
        let read = |rel: &str| {
            let bytes = vfs
                .read_sync(&VfsPath::parse(&format!("assets://{rel}")).ok()?)
                .ok()?;
            Some(String::from_utf8_lossy(&bytes).into_owned())
        };
        let index: DialogueIndex = ron::from_str(&read(DIALOGUE_INDEX_PATH)?).ok()?;
        let mut library = Self {
            sounds: index.sounds.into_iter().collect(),
            ..Self::default()
        };
        for rel in index.files {
            let Some(kind) = file_kind(&rel) else {
                continue;
            };
            match read(&rel) {
                Some(text) => library.insert_or_note(kind, &rel, &text),
                None => library.errors.push(format!("{rel}: missing from the package")),
            }
        }
        Some(library)
    }

    fn insert_or_note(&mut self, kind: FileKind, rel: &str, text: &str) {
        if let Err(err) = self.insert_text(kind, rel, text) {
            self.errors.push(err.to_string());
        }
    }

    /// Parses and adds one file. A dialogue whose name, or a voice whose id, is already in the
    /// library is not added: the first file keeps it and a `duplicate ...` line goes to `errors`
    /// (`dialogue_duplicate_id`).
    fn insert_text(&mut self, kind: FileKind, rel: &str, text: &str) -> Result<(), DialogueFormatError> {
        match kind {
            FileKind::Dialogue => {
                let d = parse_dialogue(rel, text)?;
                if self.dialogues.contains_key(&d.name) {
                    self.errors.push(format!(
                        "{rel}: {DUPLICATE_DIALOGUE_NAME} `{}`: another file defines it; this one is skipped",
                        d.name
                    ));
                } else {
                    self.dialogues.insert(d.name.clone(), d);
                }
            }
            FileKind::Voice => {
                let v = parse_voice(rel, text)?;
                if self.voices.contains_key(&v.id) {
                    self.errors.push(format!(
                        "{rel}: {DUPLICATE_VOICE_ID} `{}`: another file defines it; this one is skipped",
                        v.id
                    ));
                } else {
                    self.voices.insert(v.id.clone(), v);
                }
            }
            FileKind::Subtitles => {
                let t = parse_subtitles(rel, text)?;
                self.subtitles.insert(rel.to_owned(), t);
            }
        }
        Ok(())
    }
}

/// The `errors` text of a dialogue name loaded twice.
const DUPLICATE_DIALOGUE_NAME: &str = "duplicate dialogue name";
/// The `errors` text of a voice id loaded twice.
const DUPLICATE_VOICE_ID: &str = "duplicate voice id";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Dialogue,
    Voice,
    Subtitles,
}

fn file_kind(rel: &str) -> Option<FileKind> {
    let lower = rel.to_ascii_lowercase();
    if lower.ends_with(".dialogue.ron") {
        Some(FileKind::Dialogue)
    } else if lower.ends_with(".voice.ron") {
        Some(FileKind::Voice)
    } else if lower.ends_with(".subs.ron") {
        Some(FileKind::Subtitles)
    } else {
        None
    }
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect(root, &path, out),
            Ok(t) if t.is_file() => {
                if let Ok(rel) = path.strip_prefix(root) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
            _ => {}
        }
    }
}

/// Why a line could not be resolved or played. [`DialogueError::code`] is the agent error code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DialogueError {
    /// No dialogue with that name.
    #[error("no dialogue `{0}`")]
    UnknownAsset(String),
    /// The dialogue has no line with that id.
    #[error("dialogue `{0}` has no line `{1}`")]
    UnknownLine(String, String),
    /// A speaker or listener has no `.voice.ron`.
    #[error("no voice `{0}`")]
    UnknownVoice(String),
    /// No variant fits the speaker and listeners.
    #[error("line `{0}` has no variant for speaker `{1}`")]
    NoVariant(String, String),
    /// No playback with that handle.
    #[error("no dialogue playback {0}")]
    BadHandle(u64),
}

impl DialogueError {
    /// `dialogue_unknown_asset`, `dialogue_unknown_line`, `dialogue_unknown_voice`,
    /// `dialogue_no_variant` or `dialogue_bad_handle`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownAsset(_) => "dialogue_unknown_asset",
            Self::UnknownLine(..) => "dialogue_unknown_line",
            Self::UnknownVoice(_) => "dialogue_unknown_voice",
            Self::NoVariant(..) => "dialogue_no_variant",
            Self::BadHandle(_) => "dialogue_bad_handle",
        }
    }
}

/// What to play. [`DialogueRequest::new`] sets priority 1.
#[derive(Debug, Clone, PartialEq)]
pub struct DialogueRequest {
    /// The dialogue name.
    pub dialogue: String,
    /// The line id.
    pub line: String,
    /// The speaker voice id; `None` uses the line's speaker.
    pub speaker: Option<String>,
    /// The listener voice ids; `None` uses the line's listeners.
    pub listeners: Option<Vec<String>>,
    /// Caption priority (higher wins); 0 shows no subtitle.
    pub priority: u8,
    /// A world position: the voice plays spatially there, on the Voice bus.
    pub position: Option<Vec3>,
}

impl DialogueRequest {
    /// The line's own speaker and listeners, priority 1, no position.
    #[must_use]
    pub fn new(dialogue: impl Into<String>, line: impl Into<String>) -> Self {
        Self {
            dialogue: dialogue.into(),
            line: line.into(),
            speaker: None,
            listeners: None,
            priority: 1,
            position: None,
        }
    }
}

impl Default for DialogueRequest {
    fn default() -> Self {
        Self::new("", "")
    }
}

/// A resolved line: what [`play_line`] would play.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedLine {
    /// The chosen variant's index in the line.
    pub variant: usize,
    /// The variant's base sound path.
    pub sound: String,
    /// The sound path after localized-variant resolution (audio chain).
    pub resolved_sound: String,
    /// The transcript in the text locale.
    pub text: String,
    /// The subtitle text (the display key's text, else the transcript).
    pub display_text: String,
    /// The Fluent arguments passed (`speaker`, `speaker_gender`, ...).
    pub args: Vec<(String, String)>,
    /// The speaker voice id.
    pub speaker: String,
    /// The listener voice ids.
    pub listeners: Vec<String>,
    /// The cues (inline, else the sidecar), expanded with the sound length unknown (0).
    pub cues: Vec<TimedCue>,
    /// The cue set's priority when the sidecar gave one.
    pub track_priority: Option<i32>,
    /// The sidecar cue set keeps its author-placed line breaks.
    pub keep_breaks: bool,
    /// The sidecar cue set shows a long cue one line at a time (spec 36 § 8.1).
    pub one_at_a_time: bool,
    /// The text locale the texts were resolved in.
    pub locale: String,
}

fn text_of(loc: Option<&Localization>, key: &str, args: &[(String, String)]) -> String {
    loc.map_or_else(|| key.to_owned(), |l| l.resolve(key, args))
}

fn voice_args(
    library: &DialogueLibrary,
    loc: Option<&Localization>,
    role: &str,
    id: &str,
    args: &mut Vec<(String, String)>,
) -> Result<(), DialogueError> {
    let voice = library
        .voices
        .get(id)
        .ok_or_else(|| DialogueError::UnknownVoice(id.to_owned()))?;
    args.push((role.to_owned(), text_of(loc, &voice.name_key, &[])));
    args.push((format!("{role}_gender"), voice.gender.as_str().to_owned()));
    args.push((format!("{role}_number"), voice.number.as_str().to_owned()));
    Ok(())
}

/// Resolves a request without playing it (`dialogue.resolve`).
///
/// # Errors
///
/// An unknown dialogue, line or voice, or no fitting variant.
pub fn resolve_line(
    library: &DialogueLibrary,
    loc: Option<&Localization>,
    req: &DialogueRequest,
) -> Result<ResolvedLine, DialogueError> {
    let dialogue = library
        .dialogues
        .get(&req.dialogue)
        .ok_or_else(|| DialogueError::UnknownAsset(req.dialogue.clone()))?;
    let line = dialogue
        .line(&req.line)
        .ok_or_else(|| DialogueError::UnknownLine(req.dialogue.clone(), req.line.clone()))?;
    let speaker = req.speaker.clone().unwrap_or_else(|| line.speaker.clone());
    let listeners = req.listeners.clone().unwrap_or_else(|| line.listeners.clone());
    let mut args = Vec::new();
    voice_args(library, loc, "speaker", &speaker, &mut args)?;
    for listener in &listeners {
        library
            .voices
            .get(listener)
            .ok_or_else(|| DialogueError::UnknownVoice(listener.clone()))?;
    }
    if let Some(first) = listeners.first() {
        voice_args(library, loc, "listener", first, &mut args)?;
    }
    let listener_refs: Vec<&str> = listeners.iter().map(String::as_str).collect();
    let variant_index = line
        .select_index(&speaker, &listener_refs)
        .ok_or_else(|| DialogueError::NoVariant(req.line.clone(), speaker.clone()))?;
    let variant = &line.variants[variant_index];
    let audio_chain = loc.map(Localization::audio_chain).unwrap_or_default();
    let resolved_sound = regia_asset::localized::resolve_localized_with(&variant.sound, &audio_chain, |p| {
        library.sounds.contains(p).then_some("sound")
    });
    let text_chain = loc.map(|l| l.chain(l.locale())).unwrap_or_default();
    let (cues, track_priority, keep_breaks, one_at_a_time) = match &variant.cues {
        Some(inline) => (expand_cues(inline, 0), None, false, false),
        None => {
            let sidecar = regia_asset::localized::resolve_localized_with(
                &subtitles_sidecar(&variant.sound),
                &text_chain,
                |p| library.subtitles.contains_key(p).then_some("subtitles"),
            );
            match library.subtitles.get(&sidecar) {
                Some(track) => (
                    expand_cues(&track.cues, 0),
                    Some(track.priority),
                    track.keep_breaks,
                    track.one_at_a_time,
                ),
                None => (Vec::new(), None, false, false),
            }
        }
    };
    let text = text_of(loc, &line.text_key(&dialogue.name), &args);
    let display_text = text_of(loc, &line.display_key(&dialogue.name), &args);
    Ok(ResolvedLine {
        variant: variant_index,
        sound: variant.sound.clone(),
        resolved_sound,
        text,
        display_text,
        args,
        speaker,
        listeners,
        cues,
        track_priority,
        keep_breaks,
        one_at_a_time,
        locale: loc.map_or_else(|| "en".to_owned(), |l| l.locale().to_owned()),
    })
}

/// A playing line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DialogueHandle(pub u64);

/// What happened to a playing line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogueEventKind {
    /// The line started.
    Started,
    /// The line played to its end (uncut).
    Finished,
    /// The line was stopped before its end.
    Interrupted,
}

/// A dialogue line started, finished or was interrupted (spec 36 § 7.3).
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct DialogueEvent {
    /// The playback.
    pub handle: DialogueHandle,
    /// The dialogue name.
    pub dialogue: String,
    /// The line id.
    pub line: String,
    /// What happened.
    pub kind: DialogueEventKind,
}

/// One playing line.
#[derive(Debug, Clone, PartialEq)]
pub struct Playing {
    /// The dialogue name.
    pub dialogue: String,
    /// The line id.
    pub line: String,
    /// The caption showing its subtitle (`None` when the priority is 0).
    pub caption: Option<CaptionId>,
    /// The voice player's handle, when a sound started.
    pub voice: Option<u64>,
    /// Caption clock at the start, microseconds.
    pub start_us: u64,
    /// Planned end on the caption clock, microseconds.
    pub end_us: u64,
    /// `end_us` comes from the sound's own length, so a voice that stops well before it was cut.
    pub end_is_exact: bool,
    /// The resolved sound path.
    pub sound: String,
    /// The requested position.
    pub position: Option<Vec3>,
}

/// The lines playing now, by handle.
#[derive(Resource, Debug, Default)]
pub struct DialoguePlayback {
    next: u64,
    /// Playing lines by handle.
    pub active: BTreeMap<DialogueHandle, Playing>,
    /// Events for the next [`dialogue_system`] run (sent as messages).
    pending: Vec<DialogueEvent>,
}

fn caption_pieces(resolved: &ResolvedLine, loc: Option<&Localization>) -> Vec<CaptionPiece> {
    if resolved.cues.is_empty() {
        return vec![CaptionPiece::literal(0, resolved.display_text.clone())];
    }
    // Texts are resolved now: a locale switch mid line leaves the running subtitle alone.
    regia_ui::captions::cue_pieces(&resolved.cues, |cue| {
        CaptionText::Literal(match (&cue.text_key, &cue.text) {
            (Some(key), _) => text_of(loc, key, &resolved.args),
            (None, raw) => raw.clone().unwrap_or_default(),
        })
    })
}

/// Plays a line: selects the variant, starts the sound on the voice player (the
/// `ConversationVoice` override, else the engine audio), shows the subtitle on the caption board and
/// sends [`DialogueEvent`] `Started` on the next [`dialogue_system`] run.
///
/// # Errors
///
/// What [`resolve_line`] returns; a world without a [`DialogueLibrary`] reports
/// [`DialogueError::UnknownAsset`].
pub fn play_line(world: &mut World, req: &DialogueRequest) -> Result<DialogueHandle, DialogueError> {
    let _span = tracing::debug_span!("dialogue_play_line").entered();
    world.get_resource_or_init::<crate::conversation::ConversationVoice>();
    let resolved = {
        let library = world
            .get_resource::<DialogueLibrary>()
            .ok_or_else(|| DialogueError::UnknownAsset(req.dialogue.clone()))?;
        resolve_line(library, world.get_resource::<Localization>(), req)?
    };
    let pieces = caption_pieces(&resolved, world.get_resource::<Localization>());
    let clock = line_clock(world);
    let start = with_voice_player(world, clock, |voice: &mut dyn VoicePlayer| match req.position {
        Some(at) => voice.play_at(&resolved.resolved_sound, None, at),
        None => voice.play(&resolved.resolved_sound, None),
    });
    let cue_end = resolved.cues.iter().map(|c| c.end_us).max().unwrap_or(0);
    let sound_us = start.and_then(|s| s.duration_us).filter(|d| *d > 0);
    let duration = sound_us.unwrap_or_else(|| {
        cue_end.max(crate::conversation::voice::estimate_us(
            &resolved.display_text,
            &ConversationSettings::default(),
        ))
    });
    let start_us = start.map_or(clock, |s| s.start_us);
    // The request decides the priority; a cue set with priority 0 is never shown (spec 36 § 8.1).
    let priority = if resolved.track_priority == Some(0) {
        0
    } else {
        req.priority
    };
    let caption = (priority > 0).then(|| {
        let id = world.get_resource_or_init::<CaptionIds>().next_id();
        world.write_message(CaptionStart {
            id,
            source: CaptionSource::Game(format!("dialogue/{}#{}", req.dialogue, req.line)),
            kind: CaptionKind::Speech,
            locale: resolved.locale.clone(),
            pieces,
            speaker: Some(resolved.speaker.clone()),
            priority,
            start: CaptionTime::AtMicros(start_us),
            planned_us: duration,
            keep_breaks: resolved.keep_breaks,
            hint: CaptionStyleHint {
                one_at_a_time: resolved.one_at_a_time,
                ..CaptionStyleHint::default()
            },
        });
        id
    });
    let mut playback = world.get_resource_or_init::<DialoguePlayback>();
    playback.next += 1;
    let handle = DialogueHandle(playback.next);
    playback.active.insert(
        handle,
        Playing {
            dialogue: req.dialogue.clone(),
            line: req.line.clone(),
            caption,
            voice: start.map(|s| s.handle),
            start_us,
            end_us: start_us.saturating_add(duration),
            end_is_exact: sound_us.is_some(),
            sound: resolved.resolved_sound.clone(),
            position: req.position,
        },
    );
    playback.pending.push(DialogueEvent {
        handle,
        dialogue: req.dialogue.clone(),
        line: req.line.clone(),
        kind: DialogueEventKind::Started,
    });
    Ok(handle)
}

/// Stops one playing line (`Some(handle)`) or all (`None`) with a voice fade: the subtitle ends and
/// [`DialogueEventKind::Interrupted`] goes out. Returns how many stopped.
///
/// # Errors
///
/// [`DialogueError::BadHandle`] for a handle that is not playing.
pub fn stop_line(
    world: &mut World,
    handle: Option<DialogueHandle>,
    fade_ms: u32,
) -> Result<usize, DialogueError> {
    let stopped: Vec<(DialogueHandle, Playing)> = {
        let Some(mut playback) = world.get_resource_mut::<DialoguePlayback>() else {
            return match handle {
                Some(h) => Err(DialogueError::BadHandle(h.0)),
                None => Ok(0),
            };
        };
        match handle {
            Some(h) => {
                let playing = playback.active.remove(&h).ok_or(DialogueError::BadHandle(h.0))?;
                vec![(h, playing)]
            }
            None => std::mem::take(&mut playback.active).into_iter().collect(),
        }
    };
    let clock = line_clock(world);
    world.get_resource_or_init::<crate::conversation::ConversationVoice>();
    for (handle, playing) in &stopped {
        if let Some(voice) = playing.voice {
            with_voice_player(world, clock, |player| player.stop(voice, fade_ms));
        }
        if let Some(id) = playing.caption {
            world.write_message(CaptionStop { id });
        }
        world
            .resource_mut::<DialoguePlayback>()
            .pending
            .push(DialogueEvent {
                handle: *handle,
                dialogue: playing.dialogue.clone(),
                line: playing.line.clone(),
                kind: DialogueEventKind::Interrupted,
            });
    }
    Ok(stopped.len())
}

/// The clock lines are timed on, microseconds: the caption clock while the caption presenter
/// keeps one ([`CaptionClock`]), else the simulation time of [`Time`] (the caption clock's own
/// source when no audio device leads it), else 0. Without the presenter a line still reaches its
/// end, and a voice that plays to its end is not mistaken for a cut one.
fn line_clock(world: &World) -> u64 {
    if let Some(clock) = world.get_resource::<CaptionClock>() {
        return clock.0;
    }
    world.get_resource::<Time>().map_or(0, |t| {
        u64::try_from(t.sim_elapsed().as_micros()).unwrap_or(u64::MAX)
    })
}

/// How far before its known end a voice must stop to count as cut: the clocks of the mixer and the
/// frame differ by a block or a frame at a natural end.
const CUT_MARGIN_US: u64 = 200_000;

/// `PostUpdate`, before the caption board: a line whose planned end passed and whose voice stopped is
/// finished (its subtitle ends, [`DialogueEventKind::Finished`]), one whose voice stopped well before
/// its end was cut ([`DialogueEventKind::Interrupted`]); pending events go out as messages.
pub fn dialogue_system(world: &mut World) {
    let Some(playback) = world.get_resource::<DialoguePlayback>() else {
        return;
    };
    if playback.active.is_empty() && playback.pending.is_empty() {
        return;
    }
    let _span = tracing::debug_span!("dialogue_update").entered();
    let clock = line_clock(world);
    let lines: Vec<(DialogueHandle, Option<u64>, u64, bool)> = playback
        .active
        .iter()
        .map(|(h, p)| (*h, p.voice, p.end_us, p.end_is_exact))
        .collect();
    world.get_resource_or_init::<crate::conversation::ConversationVoice>();
    for (handle, voice, end_us, end_is_exact) in lines {
        let still_playing =
            voice.is_some_and(|v| with_voice_player(world, clock, |player| player.playing(v)));
        if still_playing {
            continue;
        }
        let reached_end = clock >= end_us;
        // A voice gone well before its own end was stopped or stolen, not finished.
        let cut = voice.is_some() && end_is_exact && clock.saturating_add(CUT_MARGIN_US) < end_us;
        if !reached_end && !cut {
            continue;
        }
        let Some(playing) = world.resource_mut::<DialoguePlayback>().active.remove(&handle) else {
            continue;
        };
        if let Some(id) = playing.caption {
            world.write_message(CaptionStop { id });
        }
        world
            .resource_mut::<DialoguePlayback>()
            .pending
            .push(DialogueEvent {
                handle,
                dialogue: playing.dialogue,
                line: playing.line,
                kind: if cut {
                    DialogueEventKind::Interrupted
                } else {
                    DialogueEventKind::Finished
                },
            });
    }
    let pending = std::mem::take(&mut world.resource_mut::<DialoguePlayback>().pending);
    for event in pending {
        world.write_message(event);
    }
}

/// A dialogue validator finding (spec 36 § 11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogueIssue {
    /// `dialogue_parse_error`, `dialogue_duplicate_line`, `dialogue_duplicate_id` (a second file with a
    /// dialogue name or voice id already loaded), `dialogue_unknown_voice`, `dialogue_missing_text`,
    /// `dialogue_missing_sound` or `subtitle_past_end`.
    pub code: &'static str,
    /// An error (else a warning).
    pub error: bool,
    /// `<dialogue>#<line>`, or the file for parse errors.
    pub subject: String,
    /// The locale, for per-locale findings.
    pub locale: Option<String>,
    /// What is wrong.
    pub message: String,
}

/// What [`validate_dialogues`] asks the project.
pub struct DialogueValidateEnv<'a> {
    /// Whether the source locale defines a key.
    pub has_source_key: &'a dyn Fn(&str) -> bool,
    /// The shipping locales' audio fallback chains, `(locale, chain)`.
    pub audio_chains: &'a [(String, Vec<String>)],
    /// A sound's length in microseconds, when known.
    pub sound_us: &'a dyn Fn(&str) -> Option<u64>,
}

/// The `dialogue-assets` checks (spec 36 § 11) over a loaded library, in a stable order.
#[must_use]
pub fn validate_dialogues(library: &DialogueLibrary, env: &DialogueValidateEnv<'_>) -> Vec<DialogueIssue> {
    let mut out = Vec::new();
    for error in &library.errors {
        let code = if error.contains("duplicate line id") {
            "dialogue_duplicate_line"
        } else if error.contains(DUPLICATE_DIALOGUE_NAME) || error.contains(DUPLICATE_VOICE_ID) {
            "dialogue_duplicate_id"
        } else {
            "dialogue_parse_error"
        };
        out.push(DialogueIssue {
            code,
            error: true,
            subject: error.split(':').next().unwrap_or_default().to_owned(),
            locale: None,
            message: error.clone(),
        });
    }
    for (name, dialogue) in &library.dialogues {
        for line in &dialogue.lines {
            let subject = format!("{name}#{}", line.id);
            let mut voices: Vec<&str> = vec![line.speaker.as_str()];
            voices.extend(line.listeners.iter().map(String::as_str));
            for v in &line.variants {
                voices.extend(v.speaker.as_deref());
                voices.extend(v.listener.as_deref());
            }
            voices.sort_unstable();
            voices.dedup();
            for voice in voices.into_iter().filter(|v| !library.voices.contains_key(*v)) {
                out.push(DialogueIssue {
                    code: "dialogue_unknown_voice",
                    error: true,
                    subject: subject.clone(),
                    locale: None,
                    message: format!("`{subject}` names the voice `{voice}`, which has no .voice.ron"),
                });
            }
            let mut keys = vec![line.text_key(name)];
            keys.extend(line.display_key.clone());
            for key in keys.into_iter().filter(|k| !(env.has_source_key)(k)) {
                out.push(DialogueIssue {
                    code: "dialogue_missing_text",
                    error: true,
                    subject: subject.clone(),
                    locale: None,
                    message: format!("`{subject}` uses `{key}`, which the source locale does not define"),
                });
            }
            for variant in &line.variants {
                for (locale, chain) in env.audio_chains {
                    let found = regia_asset::localized::localized_candidates(&variant.sound, chain)
                        .iter()
                        .any(|c| library.sounds.contains(c));
                    if !found {
                        out.push(DialogueIssue {
                            code: "dialogue_missing_sound",
                            error: true,
                            subject: subject.clone(),
                            locale: Some(locale.clone()),
                            message: format!(
                                "`{}` has no recording for `{locale}` and no base file",
                                variant.sound
                            ),
                        });
                    }
                }
                let cues = match &variant.cues {
                    Some(inline) => Some(inline.as_slice()),
                    None => library
                        .subtitles
                        .get(&subtitles_sidecar(&variant.sound))
                        .map(|t| t.cues.as_slice()),
                };
                if let (Some(cues), Some(length)) = (cues, (env.sound_us)(&variant.sound)) {
                    for cue in cues
                        .iter()
                        .filter(|c| f64::from(c.at) * 1_000_000.0 > length as f64)
                    {
                        out.push(DialogueIssue {
                            code: "subtitle_past_end",
                            error: false,
                            subject: subject.clone(),
                            locale: None,
                            message: format!(
                                "a cue of `{}` starts at {} s, after the sound's end ({} s)",
                                variant.sound,
                                cue.at,
                                length as f64 / 1_000_000.0
                            ),
                        });
                    }
                }
            }
        }
    }
    out
}

/// `OnLevelUnloading`: a line does not outlive its level (spec 36 § 7.3).
fn stop_lines_on_unload(world: &mut World) {
    if let Err(error) = stop_line(world, None, 0) {
        tracing::warn!(target: "regia_framework", "dialogue: {error}");
    }
}

/// `Startup`: loads the dialogue library: a package's index, else a walk of the assets directory.
fn load_dev_library(world: &mut World) {
    if world
        .get_resource::<DialogueLibrary>()
        .is_some_and(|l| !l.dialogues.is_empty())
    {
        return;
    }
    let vfs = world.get_resource::<Vfs>();
    let library = match vfs.and_then(DialogueLibrary::load_index) {
        Some(library) => library,
        None => {
            let assets = assets_host_root(vfs).unwrap_or_else(|| "assets".into());
            if !assets.is_dir() {
                return;
            }
            DialogueLibrary::load_dir(&assets)
        }
    };
    for error in &library.errors {
        tracing::warn!(target: "regia_framework", "dialogue: {error}");
    }
    world.insert_resource(library);
}

/// The dialogue player: [`DialogueLibrary`], [`DialoguePlayback`], [`DialogueEvent`] and
/// [`dialogue_system`]. Works with or without the caption presenter and the audio plugin: without
/// the presenter, lines are timed on the simulation clock of [`Time`].
pub struct DialoguePlugin;

impl Plugin for DialoguePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DialogueLibrary>()
            .init_resource::<DialoguePlayback>()
            .init_resource::<CaptionIds>()
            .init_resource::<crate::conversation::ConversationVoice>()
            .add_message::<CaptionStart>()
            .add_message::<CaptionStop>()
            .add_message::<DialogueEvent>()
            .add_systems(Startup, load_dev_library)
            .add_systems(OnLevelUnloading, stop_lines_on_unload)
            .add_systems(
                PostUpdate,
                dialogue_system.before(regia_ui::captions::caption_system),
            );
    }
}

#[cfg(test)]
mod tests;
