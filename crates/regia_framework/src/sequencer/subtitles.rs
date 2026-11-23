// SPDX-License-Identifier: MPL-2.0
//! Subtitles for sound sections (ADR-3334, spec 56): a voice line a sequence plays shows its
//! subtitle on the caption board, like a line the dialogue player plays.
//!
//! No format change. Each frame, after the tracks apply, every playing or paused
//! [`SequencePlayer`]'s sound sections are looked up in the [`DialogueLibrary`]: a section whose
//! sound is the sound of a dialogue line's variant (its path, its audio-locale resolution, or its
//! file stem) belongs to that line, and the line's cue set (inline cues, else the `.subs.ron`
//! sidecar for the active text locale) is followed along the section's playback position. The
//! position is a pure function of the player's clock ([`super::SoundSample::position`]), so the
//! section's start offset, its time scale and the speeds and loops of the sub-sequence sections
//! above it are already in it; nothing here reads the audio device.
//!
//! One set is queued per running cue (`regia_ui::captions::queue_set`), so a locale change swaps
//! the text at the next cue, and the board's own priority, overlap, wrapping and the player's
//! caption options apply. A cue lasts `length / rate` sequence seconds (a time scale of 0.5
//! stretches it to twice as long). A set follows the position with `sync_set`: a seek into a cue
//! shows that cue's remaining time, a pause holds the cue, and a loop that wraps back re-bases it.
//! A player that stops, finishes or despawns clears its sets; a section the clock left clears its
//! own.
//!
//! Opt-out: [`SequenceSubtitleSettings::opt_out_tracks`] (track names), the track-name suffix
//! [`NO_SUBTITLES_SUFFIX`] and [`SequenceSubtitleSettings::opt_out_sounds`]. During a render-out
//! no subtitles are requested unless [`SequenceSubtitleSettings::burn_in`] is set (off by
//! default), so a rendered video carries them only when the job asks.

use std::collections::{BTreeMap, BTreeSet};

use regia_app::caption::CaptionId;
use regia_app::prelude::*;
use regia_asset::dialogue::{Cue, TimedCue};
use regia_audio::SoundAssets;
use regia_ui::captions::{SetRequest, SubtitleKind, SyncOutcome, queue_set, stop_owner, stop_set, sync_set};
use regia_ui::localization::Localization;
use regia_ui::output::UiOutput;

use super::player::{PlaybackStatus, SequencePlayer, SequencerSet};
use crate::dialogue::{DialogueLibrary, DialogueRequest, ResolvedLine, resolve_line};

/// A sound track whose name ends with this suffix shows no subtitles.
pub const NO_SUBTITLES_SUFFIX: &str = ".nosubs";

/// What sequence subtitles do. A resource the [`super::SequencerPlugin`] keeps.
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct SequenceSubtitleSettings {
    /// Request subtitles at all (default `true`).
    pub enabled: bool,
    /// Request subtitles while a render-out renders (default `false`: a rendered video carries
    /// them only when the job asks).
    pub burn_in: bool,
    /// Sound tracks (by name) that show no subtitles.
    pub opt_out_tracks: BTreeSet<String>,
    /// Sounds (by library name) that show no subtitles.
    pub opt_out_sounds: BTreeSet<String>,
}

impl Default for SequenceSubtitleSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            burn_in: false,
            opt_out_tracks: BTreeSet::new(),
            opt_out_sounds: BTreeSet::new(),
        }
    }
}

impl SequenceSubtitleSettings {
    fn opted_out(&self, track: &str, sound: &str) -> bool {
        track.ends_with(NO_SUBTITLES_SUFFIX)
            || self.opt_out_tracks.contains(track)
            || self.opt_out_sounds.contains(sound)
    }
}

/// One cue showing now for a sound section.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveSubtitle {
    /// The section's path in the sequence tree ([`super::SoundSample::path`]).
    pub path: Vec<usize>,
    /// The section's sound.
    pub sound: String,
    /// The dialogue the sound belongs to.
    pub dialogue: String,
    /// The line.
    pub line: String,
    /// The speaker voice id.
    pub speaker: String,
    /// The cue's index in the line's cue list.
    pub cue: usize,
    /// The cue's text in the locale it was requested in.
    pub text: String,
    /// The locale of `text`.
    pub locale: String,
    /// The cue's start and end, seconds into the sound.
    pub sound_span: [f64; 2],
    /// The caption set on the board (`None` when its priority is 0).
    pub caption: Option<CaptionId>,
}

/// A section's line, found once per section.
#[derive(Debug)]
struct LineInfo {
    request: DialogueRequest,
    dialogue: String,
    line: String,
    speaker: String,
    /// Cues in microseconds of the sound; the last one ends with the sound.
    cues: Vec<TimedCue>,
}

#[derive(Debug)]
struct Running {
    cue: usize,
    set: Option<CaptionId>,
    text: String,
    locale: String,
}

#[derive(Debug)]
struct Entry {
    sound: String,
    rate: f64,
    /// `None`: the sound belongs to no line (looked up once).
    line: Option<LineInfo>,
    running: Option<Running>,
}

/// The cues showing for each player's sound sections. A resource the [`super::SequencerPlugin`]
/// keeps.
#[derive(Resource, Debug, Default)]
pub struct SequenceSubtitles {
    players: BTreeMap<Entity, BTreeMap<Vec<usize>, Entry>>,
    requested: u64,
}

impl SequenceSubtitles {
    /// The cues `player` shows now, by section path.
    #[must_use]
    pub fn active(&self, player: Entity) -> Vec<ActiveSubtitle> {
        let Some(entries) = self.players.get(&player) else {
            return Vec::new();
        };
        entries
            .iter()
            .filter_map(|(path, entry)| {
                let (line, running) = (entry.line.as_ref()?, entry.running.as_ref()?);
                let cue = line.cues.get(running.cue)?;
                Some(ActiveSubtitle {
                    path: path.clone(),
                    sound: entry.sound.clone(),
                    dialogue: line.dialogue.clone(),
                    line: line.line.clone(),
                    speaker: line.speaker.clone(),
                    cue: running.cue,
                    text: running.text.clone(),
                    locale: running.locale.clone(),
                    sound_span: [cue.start_us as f64 / 1.0e6, cue.end_us as f64 / 1.0e6],
                    caption: running.set,
                })
            })
            .collect()
    }

    /// Cue sets requested so far.
    #[must_use]
    pub fn requested(&self) -> u64 {
        self.requested
    }

    /// `true` when no player shows a cue.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.players
            .values()
            .all(|entries| entries.values().all(|e| e.running.is_none()))
    }
}

/// The index of the cue running at `position_us`, if any. Overlapping cues resolve to the
/// later one, the same cue `cue_at` shows.
#[must_use]
pub fn active_cue(cues: &[TimedCue], position_us: u64) -> Option<usize> {
    cues.iter()
        .rposition(|c| c.start_us <= position_us && position_us < c.end_us)
}

fn stem(path: &str) -> &str {
    let name = path.rsplit_once('/').map_or(path, |(_, n)| n);
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// The request that plays `sound`'s line: the first dialogue line (in dialogue and line order)
/// with a variant whose sound is `sound`, its audio-locale resolution, or has file stem `sound`.
#[must_use]
pub fn line_for_sound(
    library: &DialogueLibrary,
    loc: Option<&Localization>,
    sound: &str,
) -> Option<DialogueRequest> {
    let chain = loc.map(Localization::audio_chain).unwrap_or_default();
    for dialogue in library.dialogues.values() {
        for line in &dialogue.lines {
            for variant in &line.variants {
                let resolved = regia_asset::localized::resolve_localized_with(&variant.sound, &chain, |p| {
                    library.sounds.contains(p).then_some("sound")
                });
                if variant.sound == sound || resolved == sound || stem(&variant.sound) == sound {
                    let mut request = DialogueRequest::new(dialogue.name.clone(), line.id.clone());
                    request.speaker = variant.speaker.clone();
                    request.listeners = variant.listener.clone().map(|l| vec![l]);
                    return Some(request);
                }
            }
        }
    }
    None
}

/// The cue texts of `resolved` in time order, microseconds into the sound, the last cue ending at
/// `sound_us`.
fn finish_cues(resolved: &ResolvedLine, sound_us: u64) -> Vec<TimedCue> {
    let mut cues = resolved.cues.clone();
    if let Some(last) = cues.last_mut()
        && last.end_us <= last.start_us
    {
        last.end_us = sound_us.max(last.start_us.saturating_add(1_000_000));
    }
    cues.retain(|c| c.end_us > c.start_us);
    cues
}

fn cue_text(cue: &TimedCue, resolved: &ResolvedLine, loc: Option<&Localization>) -> String {
    match (&cue.text_key, &cue.text) {
        (Some(key), _) => loc.map_or_else(|| key.clone(), |l| l.resolve(key, &resolved.args)),
        (None, raw) => raw.clone().unwrap_or_default(),
    }
}

/// What one frame wants of one sound section.
struct Want {
    path: Vec<usize>,
    sound: String,
    position: f64,
    rate: f64,
    remaining: f64,
}

fn render_suppressed(world: &World, settings: &SequenceSubtitleSettings) -> bool {
    !settings.enabled
        || (!settings.burn_in
            && world
                .get_resource::<UiOutput>()
                .is_some_and(|o| o.forced.is_some()))
}

/// `PostUpdate`, after the tracks apply: requests, follows and stops the cue sets of the sound
/// sections.
#[tracing::instrument(skip_all, target = "regia_framework", name = "sequencer_subtitles")]
fn sync_subtitles(world: &mut World) {
    let settings = world
        .get_resource::<SequenceSubtitleSettings>()
        .cloned()
        .unwrap_or_default();
    let suppressed = render_suppressed(world, &settings);
    // What each live player wants this frame. A stopped, waiting or finished player wants nothing.
    let mut wants: BTreeMap<Entity, Vec<Want>> = BTreeMap::new();
    let mut query = world.query::<(Entity, &SequencePlayer)>();
    for (entity, player) in query.iter(world) {
        if suppressed || !matches!(player.status(), PlaybackStatus::Playing | PlaybackStatus::Paused) {
            continue;
        }
        let frame = player.frame();
        let list = frame
            .sounds
            .iter()
            .filter(|s| !settings.opted_out(s.track, s.sound))
            .map(|s| Want {
                path: s.path.clone(),
                sound: s.sound.to_owned(),
                position: s.position,
                rate: s.rate,
                remaining: s.remaining,
            })
            .collect();
        wants.insert(entity, list);
    }
    let Some(mut state) = world.remove_resource::<SequenceSubtitles>() else {
        return;
    };
    // Players that stopped, finished or are gone clear all their sets.
    let leaving: Vec<Entity> = state
        .players
        .keys()
        .copied()
        .filter(|e| !wants.contains_key(e))
        .collect();
    for entity in leaving {
        state.players.remove(&entity);
        stop_owner(world, entity);
    }
    for (entity, list) in wants {
        let mut entries = state.players.remove(&entity).unwrap_or_default();
        let wanted: BTreeSet<&[usize]> = list.iter().map(|w| w.path.as_slice()).collect();
        let gone: Vec<Vec<usize>> = entries
            .keys()
            .filter(|p| !wanted.contains(p.as_slice()))
            .cloned()
            .collect();
        for path in gone {
            if let Some(set) = entries.remove(&path).and_then(|e| e.running).and_then(|r| r.set) {
                stop_set(world, set);
            }
        }
        for want in &list {
            follow(world, &mut state.requested, entity, &mut entries, want);
        }
        if !entries.is_empty() {
            state.players.insert(entity, entries);
        }
    }
    world.insert_resource(state);
}

fn follow(
    world: &mut World,
    requested: &mut u64,
    owner: Entity,
    entries: &mut BTreeMap<Vec<usize>, Entry>,
    want: &Want,
) {
    let rate = if want.rate.is_finite() && want.rate > 0.0 {
        want.rate
    } else {
        1.0
    };
    // A section whose sound or rate changed is a new section.
    if entries
        .get(&want.path)
        .is_some_and(|e| e.sound != want.sound || (e.rate - rate).abs() > f64::EPSILON)
        && let Some(set) = entries
            .remove(&want.path)
            .and_then(|e| e.running)
            .and_then(|r| r.set)
    {
        stop_set(world, set);
    }
    let entry = entries.entry(want.path.clone()).or_insert_with(|| Entry {
        sound: want.sound.clone(),
        rate,
        line: find_line(world, want),
        running: None,
    });
    let Some(line) = entry.line.as_ref() else {
        return;
    };
    let position_us = (want.position.max(0.0) * 1.0e6).round() as u64;
    let wanted_cue = active_cue(&line.cues, position_us);
    let running_cue = entry.running.as_ref().map(|r| r.cue);
    if wanted_cue != running_cue {
        if let Some(set) = entry.running.take().and_then(|r| r.set) {
            stop_set(world, set);
        }
        let Some(index) = wanted_cue else {
            return;
        };
        let Some(running) = start_cue(world, owner, line, index, rate) else {
            return;
        };
        *requested += 1;
        entry.running = Some(running);
    }
    let (Some(running), Some(cue)) = (entry.running.as_mut(), wanted_cue.and_then(|i| line.cues.get(i)))
    else {
        return;
    };
    if let Some(set) = running.set {
        let playhead = ((want.position - cue.start_us as f64 / 1.0e6) / rate).max(0.0);
        match sync_set(world, set, playhead as f32) {
            Ok(Some(SyncOutcome::InSync | SyncOutcome::Rebased)) => {}
            // The board is done with the set (or the playhead left it): the cue stays "running"
            // until the clock leaves it, and is not asked for again.
            Ok(Some(SyncOutcome::Finished) | None) | Err(_) => running.set = None,
        }
    }
}

fn find_line(world: &World, want: &Want) -> Option<LineInfo> {
    let library = world.get_resource::<DialogueLibrary>()?;
    let loc = world.get_resource::<Localization>();
    let request = line_for_sound(library, loc, &want.sound)?;
    let resolved = resolve_line(library, loc, &request).ok()?;
    let asset_secs = world
        .get_resource::<SoundAssets>()
        .and_then(|s| s.by_name(&want.sound))
        .map(|a| a.duration_secs());
    let sound_secs = asset_secs.unwrap_or(want.position + want.remaining * want.rate);
    let sound_us = (sound_secs.max(0.0) * 1.0e6).round() as u64;
    let cues = finish_cues(&resolved, sound_us);
    Some(LineInfo {
        dialogue: request.dialogue.clone(),
        line: request.line.clone(),
        speaker: resolved.speaker,
        request,
        cues,
    })
}

/// Queues the one-cue set of cue `index`, texts resolved now in the active locale.
fn start_cue(world: &mut World, owner: Entity, line: &LineInfo, index: usize, rate: f64) -> Option<Running> {
    let cue = line.cues.get(index)?;
    let resolved = {
        let library = world.get_resource::<DialogueLibrary>()?;
        resolve_line(library, world.get_resource::<Localization>(), &line.request).ok()?
    };
    let text = cue_text(cue, &resolved, world.get_resource::<Localization>());
    let length = (cue.end_us - cue.start_us) as f64 / 1.0e6 / rate;
    let req = SetRequest {
        cues: vec![Cue {
            at: 0.0,
            until: Some(length as f32),
            text_key: None,
            text: Some(text.clone()),
        }],
        priority: resolved.track_priority.unwrap_or(1),
        duration: length as f32,
        start_delay: 0.0,
        keep_breaks: resolved.keep_breaks,
        one_at_a_time: resolved.one_at_a_time,
        kind: SubtitleKind::Dialogue,
        speaker: Some(resolved.speaker.clone()),
        owner: Some(owner),
    };
    let set = queue_set(world, &req).ok().flatten();
    Some(Running {
        cue: index,
        set,
        text,
        locale: resolved.locale,
    })
}

/// Adds the settings, the state and the sync (`PostUpdate`, after [`SequencerSet::Apply`]).
pub(crate) fn install(app: &mut App) {
    use regia_app::caption::{CaptionStart, CaptionStop};
    app.init_resource::<SequenceSubtitleSettings>()
        .init_resource::<SequenceSubtitles>()
        .add_message::<CaptionStart>()
        .add_message::<CaptionStop>()
        .add_systems(
            PostUpdate,
            sync_subtitles
                .after(SequencerSet::Apply)
                .run_if(resource_exists::<DialogueLibrary>),
        );
}

#[cfg(test)]
mod tests;
