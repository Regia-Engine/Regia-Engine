// SPDX-License-Identifier: MPL-2.0
//! The conversation runner (`docs/specs/56-dialogue-and-subtitles.md` §§ 2.5, 3.2).
//!
//! The runner is a plain state machine: [`ConversationRunner::tick`] takes the clock and a
//! [`RunnerEnv`] (text, clips, voice playback) and appends [`RunnerOutput`]s; the plugin turns
//! those into ECS messages. Nothing here reads a clock, a random number or a hash order, so two
//! runs with the same calls give the same outputs.

use std::collections::{BTreeMap, BTreeSet};

use regia_app::caption::{
    CaptionId, CaptionIds, CaptionKind, CaptionPiece, CaptionSource, CaptionStart, CaptionStyleHint,
    CaptionText, CaptionTime,
};
use regia_app::prelude::*;
use regia_ui::captions::grapheme_count;

use super::ConversationError;
use super::asset::{ConversationAsset, ConversationSettings, Speaker, SpeakerSet, Step};
use super::voice::{DurationBasis, VoiceContext, plan_duration, resolve_voice};

/// A sub-cue offset for a locale whose clip differs in length from the source (`rescale_cues`, spec
/// 56 § 2.4): `offset * clip / source`, rounded down. The authored offset when either length is
/// unknown.
fn rescale_offset(offset_us: u64, clip_us: Option<u64>, source_us: Option<u64>) -> u64 {
    match (clip_us, source_us) {
        (Some(clip), Some(source)) if source > 0 => {
            let scaled = u128::from(offset_us) * u128::from(clip) / u128::from(source);
            u64::try_from(scaled).unwrap_or(u64::MAX)
        }
        _ => offset_us,
    }
}

/// The id of one conversation run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunId(pub u64);

/// What to do when the channel already has a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BusyPolicy {
    /// Refuse with `ChannelBusy`.
    #[default]
    Fail,
    /// Stop the current run and start the new one.
    Replace,
    /// Start the new run when the current one finishes.
    Queue,
}

/// A request to play a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayRequest {
    /// The conversation id.
    pub conversation: String,
    /// The locale; `None` uses the active one.
    pub locale: Option<String>,
    /// Start at this label.
    pub from: Option<String>,
    /// The channel (default `main`).
    pub channel: String,
    /// What to do on a busy channel.
    pub busy: BusyPolicy,
}

impl PlayRequest {
    /// A request on the `main` channel.
    pub fn new(conversation: impl Into<String>) -> Self {
        Self {
            conversation: conversation.into(),
            locale: None,
            from: None,
            channel: "main".to_string(),
            busy: BusyPolicy::Fail,
        }
    }
}

/// A one-off caption (`conversation.say`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SayRequest {
    /// `<conversation>#<line>` to say an authored line.
    pub line: Option<String>,
    /// Literal text to show instead.
    pub text: Option<String>,
    /// The speaker id.
    pub speaker: Option<String>,
    /// The caption kind (default speech).
    pub kind: Option<CaptionKind>,
    /// The planned length in milliseconds.
    pub duration_ms: Option<u32>,
    /// The priority.
    pub priority: Option<u8>,
    /// A clip asset path to play.
    pub voice: Option<String>,
    /// The locale.
    pub locale: Option<String>,
}

/// The longest text `say` accepts, in graphemes.
pub const MAX_SAY_GRAPHEMES: usize = 512;

/// Answers a condition name. `None` means "unknown".
pub trait ConditionSource: Send + Sync {
    /// Evaluates a name.
    fn eval(&self, name: &str) -> Option<bool>;
}

/// The built-in condition source: a name that is present is true, an absent one is unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlagSet(pub BTreeSet<String>);

impl FlagSet {
    /// Sets or clears a flag.
    pub fn set(&mut self, name: &str, on: bool) {
        if on {
            self.0.insert(name.to_string());
        } else {
            self.0.remove(name);
        }
    }
}

impl ConditionSource for FlagSet {
    fn eval(&self, name: &str) -> Option<bool> {
        self.0.contains(name).then_some(true)
    }
}

/// One offered choice option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceView {
    /// The option's index among the offered options.
    pub index: usize,
    /// The resolved text.
    pub text: String,
}

/// Why a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    /// It reached `End` or ran out of steps.
    Finished,
    /// It was stopped.
    Stopped,
    /// It failed (a missing line, a missing label, the step budget).
    Error,
}

/// What the runner produced.
#[derive(Debug, Clone, PartialEq)]
pub enum RunnerOutput {
    /// Send this `CaptionStart`.
    Caption(CaptionStart),
    /// Send this `CaptionStop`.
    CaptionStop(CaptionId),
    /// An `Event` step ran.
    Event {
        /// The run.
        run: RunId,
        /// The event name.
        name: String,
    },
    /// A `Choice` step is waiting.
    ChoiceRequested {
        /// The run.
        run: RunId,
        /// The prompt, resolved.
        prompt: Option<String>,
        /// The offered options.
        options: Vec<ChoiceView>,
    },
    /// A run ended.
    Finished {
        /// The run.
        run: RunId,
        /// Why.
        reason: FinishReason,
        /// The error text for `Error`.
        detail: Option<String>,
    },
    /// Stop the voice instance (a stopped or skipped line).
    StopVoice {
        /// The voice handle from [`VoiceStart`].
        handle: u64,
        /// Fade length in milliseconds.
        fade_ms: u32,
    },
}

/// A started voice clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceStart {
    /// A handle to ask about later.
    pub handle: u64,
    /// The audio clock at the call, in microseconds.
    pub start_us: u64,
    /// The clip length if known.
    pub duration_us: Option<u64>,
}

/// What the runner needs from the world.
pub trait RunnerEnv {
    /// The conversation with that id.
    fn conversation(&self, id: &str) -> Option<&ConversationAsset>;
    /// Project settings.
    fn settings(&self) -> &ConversationSettings;
    /// The speaker set.
    fn speakers(&self) -> &SpeakerSet;
    /// Whether a locale is loaded.
    fn locale_loaded(&self, locale: &str) -> bool;
    /// The active locale.
    fn active_locale(&self) -> String;
    /// The fallback chain of a locale, starting with itself.
    fn chain(&self, locale: &str) -> Vec<String>;
    /// Text of a key in a locale, falling back along the chain.
    fn text(&self, locale: &str, key: &str) -> Option<String>;
    /// Whether a clip asset exists.
    fn clip_exists(&self, path: &str) -> bool;
    /// The length of a clip in microseconds, from the cooked table or its header.
    fn clip_duration_us(&self, path: &str) -> Option<u64>;
    /// Starts a voice clip on the Voice bus.
    fn play_voice(&mut self, clip: &str) -> Option<VoiceStart>;
    /// Whether a started voice is still playing.
    ///
    /// The runner never gates a line's end on this: line ends are the planned end on the runner's
    /// clock, so a live device, a replay and a headless run agree (audit G finding 1, ADR-4756).
    /// Implementations keep it for presentation queries (tests, the agent).
    fn voice_playing(&self, handle: u64) -> bool;
    /// The caption clock now in microseconds, when the caption presenter runs on a clock other
    /// than the runner's (the engine audio's sample clock, ADR-4756). An unvoiced caption is
    /// stamped with it, so the board shows it for its planned length whatever the offset between
    /// the two clocks; the runner's deadlines stay on its own clock (corr-framework-a F4). The
    /// default, `None`, stamps with the runner's clock.
    fn caption_now_us(&self) -> Option<u64> {
        None
    }
}

#[derive(Debug, Clone)]
enum RunState {
    /// Executing steps.
    Running,
    /// A line is showing.
    Line {
        caption: CaptionId,
        ends_us: u64,
        voice: Option<u64>,
    },
    /// A choice is offered; each entry is the goto label.
    Choice(Vec<String>),
    Done,
}

#[derive(Debug, Clone)]
struct Run {
    id: RunId,
    channel: String,
    conversation: String,
    locale: String,
    pc: usize,
    state: RunState,
    /// Cached effective steps. Rebuilt when `stamp_asset` changes.
    steps: Vec<Step>,
    /// Label indexes built from `steps`.
    labels: BTreeMap<String, usize>,
    /// `stamp_asset` of the cached steps. `0` with `cached == false` means the cache is empty.
    stamp: u64,
    cached: bool,
}

fn mix_stamp(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn stamp_step(mut hash: u64, step: &Step) -> u64 {
    match step {
        Step::Line(id) => {
            hash = mix_stamp(hash, b"L");
            mix_stamp(hash, id.as_bytes())
        }
        Step::Label(id) => {
            hash = mix_stamp(hash, b"A");
            mix_stamp(hash, id.as_bytes())
        }
        Step::Goto(id) => {
            hash = mix_stamp(hash, b"G");
            mix_stamp(hash, id.as_bytes())
        }
        Step::If { cond, then, else_ } => {
            hash = mix_stamp(hash, b"I");
            hash = mix_stamp(hash, cond.as_bytes());
            hash = mix_stamp(hash, then.as_bytes());
            mix_stamp(hash, else_.as_bytes())
        }
        Step::Event(name) => {
            hash = mix_stamp(hash, b"E");
            mix_stamp(hash, name.as_bytes())
        }
        Step::End => mix_stamp(hash, b"Z"),
        Step::Choice { prompt, options } => {
            hash = mix_stamp(hash, b"C");
            if let Some(prompt) = prompt {
                hash = mix_stamp(hash, prompt.as_bytes());
            }
            for option in options {
                hash = mix_stamp(hash, option.text.as_bytes());
                if let Some(cond) = &option.cond {
                    hash = mix_stamp(hash, cond.as_bytes());
                }
                hash = mix_stamp(hash, option.goto.as_bytes());
            }
            hash
        }
    }
}

/// A stamp of the steps the runner would execute. Empty authored steps hash the line ids, because
/// that is the list `effective_steps` builds. This does not call `effective_steps`.
fn stamp_asset(asset: &ConversationAsset) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    if asset.steps.is_empty() {
        hash = mix_stamp(hash, b"lines");
        for line in &asset.lines {
            hash = mix_stamp(hash, line.id.as_bytes());
            hash = mix_stamp(hash, b"\0");
        }
        hash
    } else {
        hash = mix_stamp(hash, b"steps");
        for step in &asset.steps {
            hash = stamp_step(hash, step);
        }
        hash
    }
}

/// A queued request waiting for its channel.
#[derive(Debug, Clone)]
struct Queued {
    request: PlayRequest,
}

/// A summary of one run (`RunnerState`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunInfo {
    /// The run.
    pub run: RunId,
    /// Its channel.
    pub channel: String,
    /// Its conversation.
    pub conversation: String,
    /// `running`, `line`, `choice` or `done`.
    pub state: &'static str,
    /// The showing caption, if any.
    pub caption: Option<CaptionId>,
}

/// Result of [`ConversationRunner::play`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayStarted {
    /// The run, or `None` when the request was queued.
    pub run: Option<RunId>,
    /// The locale in use.
    pub locale: String,
}

/// Runs conversations.
#[derive(Resource, Default)]
pub struct ConversationRunner {
    runs: BTreeMap<RunId, Run>,
    queued: Vec<Queued>,
    next_run: u64,
    /// The built-in flag source.
    pub flags: FlagSet,
    sources: Vec<Box<dyn ConditionSource>>,
    warned: BTreeSet<String>,
    pending: Vec<RunnerOutput>,
}

impl std::fmt::Debug for ConversationRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConversationRunner")
            .field("runs", &self.runs.len())
            .field("queued", &self.queued.len())
            .finish()
    }
}

impl ConversationRunner {
    /// Adds a condition source, queried after the built-in flags in the order added.
    pub fn add_source(&mut self, source: Box<dyn ConditionSource>) {
        self.sources.push(source);
    }

    fn eval(&mut self, name: &str) -> bool {
        // The chain is the built-in flags first (a present name is true, an absent one is
        // unknown), then the added sources in order; the first answer wins.
        if let Some(value) = self.flags.eval(name) {
            return value;
        }
        for source in &self.sources {
            if let Some(value) = source.eval(name) {
                return value;
            }
        }
        if self.warned.insert(name.to_string()) {
            tracing::warn!(
                condition = name,
                "conversation condition unknown to every source; treated as false"
            );
        }
        false
    }

    /// Starts a conversation. Errors: `NotFound`, `LocaleNotLoaded`, `LabelNotFound`,
    /// `ChannelBusy`.
    pub fn play(&mut self, req: PlayRequest, env: &dyn RunnerEnv) -> Result<PlayStarted, ConversationError> {
        let asset = env
            .conversation(&req.conversation)
            .ok_or_else(|| ConversationError::NotFound(req.conversation.clone()))?;
        let locale = req.locale.clone().unwrap_or_else(|| env.active_locale());
        if !env.locale_loaded(&locale) {
            return Err(ConversationError::LocaleNotLoaded(locale));
        }
        let pc = match &req.from {
            Some(label) => *asset
                .labels()
                .get(label)
                .ok_or_else(|| ConversationError::LabelNotFound(label.clone()))?,
            None => 0,
        };
        let busy = self
            .runs
            .values()
            .find(|r| r.channel == req.channel && !matches!(r.state, RunState::Done))
            .map(|r| r.id);
        if let Some(existing) = busy {
            match req.busy {
                BusyPolicy::Fail => return Err(ConversationError::ChannelBusy(req.channel)),
                BusyPolicy::Queue => {
                    self.queued.push(Queued { request: req });
                    return Ok(PlayStarted { run: None, locale });
                }
                BusyPolicy::Replace => self.stop(existing, 0),
            }
        }
        self.next_run += 1;
        let id = RunId(self.next_run);
        self.runs.insert(
            id,
            Run {
                id,
                channel: req.channel,
                conversation: req.conversation,
                locale: locale.clone(),
                pc,
                state: RunState::Running,
                steps: Vec::new(),
                labels: BTreeMap::new(),
                stamp: 0,
                cached: false,
            },
        );
        Ok(PlayStarted {
            run: Some(id),
            locale,
        })
    }

    /// Picks an offered option. Errors: `RunNotFound`-style `NotFound`, `NoChoicePending`,
    /// `ChoiceOutOfRange`.
    pub fn choose(&mut self, run: RunId, index: usize, env: &dyn RunnerEnv) -> Result<(), ConversationError> {
        let entry = self
            .runs
            .get_mut(&run)
            .ok_or(ConversationError::RunNotFound(run.0))?;
        let RunState::Choice(gotos) = &entry.state else {
            return Err(ConversationError::NoChoicePending);
        };
        let label = gotos
            .get(index)
            .cloned()
            .ok_or(ConversationError::ChoiceOutOfRange)?;
        let target = env
            .conversation(&entry.conversation)
            .and_then(|a| a.labels().get(&label).copied())
            .ok_or(ConversationError::LabelNotFound(label))?;
        entry.pc = target;
        entry.state = RunState::Running;
        Ok(())
    }

    /// Stops a run: its caption ends, its voice fades and `Finished { Stopped }` is emitted on the
    /// next tick.
    pub fn stop(&mut self, run: RunId, fade_ms: u32) {
        let Some(entry) = self.runs.get_mut(&run) else {
            return;
        };
        if let RunState::Line { caption, voice, .. } = entry.state {
            self.pending.push(RunnerOutput::CaptionStop(caption));
            if let Some(handle) = voice {
                self.pending.push(RunnerOutput::StopVoice { handle, fade_ms });
            }
        }
        entry.state = RunState::Done;
        self.pending.push(RunnerOutput::Finished {
            run,
            reason: FinishReason::Stopped,
            detail: None,
        });
    }

    /// Ends the showing line at once and continues with the next step.
    pub fn skip(&mut self, run: RunId) {
        let Some(entry) = self.runs.get_mut(&run) else {
            return;
        };
        if let RunState::Line { caption, voice, .. } = entry.state {
            self.pending.push(RunnerOutput::CaptionStop(caption));
            if let Some(handle) = voice {
                self.pending.push(RunnerOutput::StopVoice { handle, fade_ms: 0 });
            }
            entry.state = RunState::Running;
        }
    }

    /// True when a run, a queued request or an unsent output exists.
    pub fn has_work(&self) -> bool {
        !self.runs.is_empty() || !self.queued.is_empty() || !self.pending.is_empty()
    }

    /// The runs, in id order.
    pub fn state(&self) -> Vec<RunInfo> {
        self.runs
            .values()
            .map(|r| RunInfo {
                run: r.id,
                channel: r.channel.clone(),
                conversation: r.conversation.clone(),
                state: match r.state {
                    RunState::Running => "running",
                    RunState::Line { .. } => "line",
                    RunState::Choice(_) => "choice",
                    RunState::Done => "done",
                },
                caption: match r.state {
                    RunState::Line { caption, .. } => Some(caption),
                    _ => None,
                },
            })
            .collect()
    }

    /// Shows a one-off caption now. Errors: `NotFound` (line or speaker), `TextTooLong`,
    /// `Asset("invalid_params")` when neither `line` nor `text` is given.
    pub fn say(
        &mut self,
        req: SayRequest,
        now_us: u64,
        env: &mut dyn RunnerEnv,
        ids: &mut CaptionIds,
    ) -> Result<CaptionStart, ConversationError> {
        let locale = req.locale.clone().unwrap_or_else(|| env.active_locale());
        if !env.locale_loaded(&locale) {
            return Err(ConversationError::LocaleNotLoaded(locale));
        }
        if let Some(speaker) = &req.speaker
            && !env.speakers().speakers.contains_key(speaker)
        {
            return Err(ConversationError::SpeakerNotFound(speaker.clone()));
        }
        let start = if let Some(reference) = &req.line {
            let (conv, line_id) = reference.split_once('#').ok_or_else(|| {
                ConversationError::InvalidParams("`line` must be `<conversation>#<line>`".into())
            })?;
            let run = RunId(0);
            self.start_line_caption(run, conv, line_id, &locale, now_us, env, ids)?
                .0
        } else if let Some(text) = &req.text {
            if grapheme_count(text) > MAX_SAY_GRAPHEMES {
                return Err(ConversationError::TextTooLong);
            }
            let settings = env.settings().clone();
            let (planned, voice_start) = match &req.voice {
                Some(clip) => {
                    let start = env.play_voice(clip);
                    let len = start
                        .and_then(|s| s.duration_us)
                        .or_else(|| env.clip_duration_us(clip));
                    (
                        len.unwrap_or_else(|| super::voice::estimate_us(text, &settings)),
                        start,
                    )
                }
                None => (super::voice::estimate_us(text, &settings), None),
            };
            let planned = req.duration_ms.map_or(planned, |ms| u64::from(ms) * 1000);
            CaptionStart {
                id: ids.next_id(),
                source: CaptionSource::Game("say".to_string()),
                kind: req.kind.unwrap_or(CaptionKind::Speech),
                locale: locale.clone(),
                pieces: vec![CaptionPiece::literal(0, text.clone())],
                speaker: req.speaker.clone(),
                priority: req.priority.unwrap_or(settings.default_priority),
                start: CaptionTime::AtMicros(
                    voice_start.map_or_else(|| env.caption_now_us().unwrap_or(now_us), |v| v.start_us),
                ),
                planned_us: planned,
                keep_breaks: false,
                hint: CaptionStyleHint::default(),
            }
        } else {
            return Err(ConversationError::InvalidParams("give `line` or `text`".into()));
        };
        let mut start = start;
        if let Some(priority) = req.priority {
            start.priority = priority;
        }
        if let Some(kind) = req.kind {
            start.kind = kind;
        }
        if let Some(ms) = req.duration_ms {
            start.planned_us = u64::from(ms) * 1000;
        }
        self.pending.push(RunnerOutput::Caption(start.clone()));
        Ok(start)
    }

    /// Builds the `CaptionStart` of an authored line and starts its voice. Returns the caption and
    /// the voice handle.
    #[allow(clippy::too_many_arguments)]
    fn start_line_caption(
        &self,
        run: RunId,
        conversation: &str,
        line_id: &str,
        locale: &str,
        now_us: u64,
        env: &mut dyn RunnerEnv,
        ids: &mut CaptionIds,
    ) -> Result<(CaptionStart, Option<u64>), ConversationError> {
        let asset = env
            .conversation(conversation)
            .ok_or_else(|| ConversationError::NotFound(conversation.to_string()))?
            .clone();
        let line = asset
            .line(line_id)
            .ok_or_else(|| ConversationError::LineNotFound(format!("{conversation}#{line_id}")))?
            .clone();
        let settings = env.settings().clone();
        let speaker: Speaker = env
            .speakers()
            .speakers
            .get(&line.speaker)
            .cloned()
            .unwrap_or_default();
        let text = env.text(locale, &line.text).unwrap_or_else(|| line.text.clone());
        let chain = env.chain(locale);
        let index = asset.line_index(line_id).unwrap_or(0);
        let (resolution, source_clip) = {
            let env_ref: &dyn RunnerEnv = &*env;
            let exists = |path: &str| env_ref.clip_exists(path);
            let ctx = VoiceContext {
                conversation,
                index,
                speaker_voiced: speaker.voiced,
                template: Some(settings.voice_path.as_str()),
                fallback_policy: settings.voice_fallback,
                chain: &chain,
                exists: &exists,
            };
            let resolution = resolve_voice(&line, &ctx, locale);
            // The chain ends at the source locale; its clip is what the cue offsets were timed to.
            let source_clip = chain
                .split_last()
                .filter(|_| settings.rescale_cues && !line.cues.is_empty())
                .and_then(|(source, _)| {
                    let source_ctx = VoiceContext {
                        chain: std::slice::from_ref(source),
                        ..ctx
                    };
                    resolve_voice(&line, &source_ctx, source).clip
                });
            (resolution, source_clip)
        };
        let clip_us = resolution.clip.as_deref().and_then(|c| env.clip_duration_us(c));
        let source_us = source_clip.as_deref().and_then(|c| env.clip_duration_us(c));
        let (mut planned, basis) = plan_duration(line.duration, clip_us, &text, &settings);
        let mut voice = None;
        let mut start_us = env.caption_now_us().unwrap_or(now_us);
        if let Some(clip) = resolution.clip.as_deref()
            && let Some(started) = env.play_voice(clip)
        {
            voice = Some(started);
            start_us = started.start_us;
            if basis == DurationBasis::Clip {
                // The cooked integer length wins over the length the mixer reports (a float header
                // value, rounded): both must plan the same end on every run (audit G finding 1,
                // ADR-4756).
                planned = clip_us.or(started.duration_us).unwrap_or(planned);
            }
        }
        let mut pieces = Vec::new();
        if line.cues.is_empty() {
            pieces.push(CaptionPiece {
                offset_us: 0,
                text: CaptionText::Literal(text),
            });
        } else {
            for cue in &line.cues {
                let piece = env.text(locale, &cue.1).unwrap_or_else(|| cue.1.clone());
                let offset_us = u64::from(cue.0) * 1000;
                pieces.push(CaptionPiece::literal(
                    rescale_offset(offset_us, clip_us, source_us),
                    piece,
                ));
            }
        }
        let kind = if speaker.narration {
            CaptionKind::Narration
        } else {
            CaptionKind::Speech
        };
        let caption = CaptionStart {
            id: ids.next_id(),
            source: CaptionSource::Dialogue {
                run: run.0,
                line: format!("{conversation}#{line_id}"),
            },
            kind,
            locale: locale.to_string(),
            pieces,
            speaker: Some(line.speaker.clone()),
            priority: line.priority.unwrap_or(settings.default_priority),
            start: CaptionTime::AtMicros(start_us),
            planned_us: planned,
            keep_breaks: line.keep_breaks,
            hint: CaptionStyleHint::default(),
        };
        Ok((caption, voice.map(|v| v.handle)))
    }

    /// Advances every run to `now_us` and returns what they produced, in a fixed order (runs by
    /// id, outputs in the order they happened).
    pub fn tick(&mut self, now_us: u64, env: &mut dyn RunnerEnv, ids: &mut CaptionIds) -> Vec<RunnerOutput> {
        let _span = tracing::trace_span!("dialogue_runner").entered();
        let mut out = std::mem::take(&mut self.pending);
        let run_ids: Vec<RunId> = self.runs.keys().copied().collect();
        for id in run_ids {
            self.tick_run(id, now_us, env, ids, &mut out);
        }
        // Remove finished runs, then start queued requests whose channel is free.
        self.runs.retain(|_, r| !matches!(r.state, RunState::Done));
        let mut i = 0;
        while i < self.queued.len() {
            let channel = self.queued[i].request.channel.clone();
            let busy = self.runs.values().any(|r| r.channel == channel);
            if busy {
                i += 1;
                continue;
            }
            let request = self.queued.remove(i).request;
            if let Err(error) = self.play(request, &*env) {
                tracing::warn!(%error, "queued conversation could not start");
            }
        }
        out
    }

    fn tick_run(
        &mut self,
        id: RunId,
        now_us: u64,
        env: &mut dyn RunnerEnv,
        ids: &mut CaptionIds,
        out: &mut Vec<RunnerOutput>,
    ) {
        // A line in progress: wait for it.
        if let Some(run) = self.runs.get(&id)
            && let RunState::Line {
                caption,
                ends_us,
                voice,
            } = run.state
        {
            // The planned end on the runner's clock ends the line (audit G finding 1, ADR-4756):
            // the mixer's playback state follows the audio callback on the device thread, so it
            // ends the line one step early or late between a live run, a replay and a machine with
            // a different buffer size. Mixer state stays a presentation concern (fades).
            if now_us < ends_us {
                return;
            }
            if voice.is_some() {
                out.push(RunnerOutput::CaptionStop(caption));
            }
            if let Some(run) = self.runs.get_mut(&id) {
                run.state = RunState::Running;
            }
        }
        let budget = env.settings().max_steps_per_frame;
        let mut taken = 0;
        loop {
            if !self.refresh_run_steps(id, env, out) {
                return;
            }
            let next = {
                let Some(run) = self.runs.get(&id) else {
                    return;
                };
                if !matches!(run.state, RunState::Running) {
                    return;
                }
                let pc = run.pc;
                run.steps.get(pc).cloned().map(|step| (pc, step))
            };
            let Some((pc, step)) = next else {
                self.finish(id, FinishReason::Finished, None, out);
                return;
            };
            if taken >= budget {
                self.finish(
                    id,
                    FinishReason::Error,
                    Some(ConversationError::StepBudget.to_string()),
                    out,
                );
                return;
            }
            taken += 1;
            match step {
                Step::Label(_) => self.set_pc(id, pc + 1),
                Step::Goto(label) => match self.label_index(id, &label) {
                    Some(target) => self.set_pc(id, target),
                    None => {
                        self.finish(
                            id,
                            FinishReason::Error,
                            Some(format!("label `{label}` not found")),
                            out,
                        );
                        return;
                    }
                },
                Step::If { cond, then, else_ } => {
                    let label = if self.eval(&cond) { then } else { else_ };
                    match self.label_index(id, &label) {
                        Some(target) => self.set_pc(id, target),
                        None => {
                            self.finish(
                                id,
                                FinishReason::Error,
                                Some(format!("label `{label}` not found")),
                                out,
                            );
                            return;
                        }
                    }
                }
                Step::Event(name) => {
                    out.push(RunnerOutput::Event { run: id, name });
                    self.set_pc(id, pc + 1);
                }
                Step::End => {
                    self.finish(id, FinishReason::Finished, None, out);
                    return;
                }
                Step::Choice { prompt, options } => {
                    let locale = self
                        .runs
                        .get(&id)
                        .map(|run| run.locale.clone())
                        .unwrap_or_default();
                    let offered: Vec<_> = options
                        .iter()
                        .filter(|o| o.cond.as_ref().is_none_or(|c| self.eval(c)))
                        .cloned()
                        .collect();
                    if offered.is_empty() {
                        match options.iter().find(|o| o.cond.is_none()) {
                            Some(first) => match self.label_index(id, &first.goto) {
                                Some(target) => self.set_pc(id, target),
                                None => {
                                    self.finish(
                                        id,
                                        FinishReason::Error,
                                        Some(format!("label `{}` not found", first.goto)),
                                        out,
                                    );
                                    return;
                                }
                            },
                            None => {
                                self.finish(id, FinishReason::Finished, None, out);
                                return;
                            }
                        }
                        continue;
                    }
                    let views = offered
                        .iter()
                        .enumerate()
                        .map(|(index, o)| ChoiceView {
                            index,
                            text: env.text(&locale, &o.text).unwrap_or_else(|| o.text.clone()),
                        })
                        .collect();
                    let prompt = prompt.map(|k| env.text(&locale, &k).unwrap_or(k));
                    out.push(RunnerOutput::ChoiceRequested {
                        run: id,
                        prompt,
                        options: views,
                    });
                    let gotos = offered.into_iter().map(|o| o.goto).collect();
                    if let Some(run) = self.runs.get_mut(&id) {
                        run.state = RunState::Choice(gotos);
                        run.pc = pc + 1;
                    }
                    return;
                }
                Step::Line(line_id) => {
                    let (conversation, locale) = self
                        .runs
                        .get(&id)
                        .map(|run| (run.conversation.clone(), run.locale.clone()))
                        .unwrap_or_default();
                    match self.start_line_caption(id, &conversation, &line_id, &locale, now_us, env, ids) {
                        Ok((caption, voice)) => {
                            // The caption may be stamped on the device sample clock. The runner's
                            // deadline stays on its own clock (ADR-4756): a device that is already
                            // ahead must not hold the next line for that lead as well.
                            let ends_us = now_us.saturating_add(caption.planned_us);
                            let caption_id = caption.id;
                            out.push(RunnerOutput::Caption(caption));
                            if let Some(run) = self.runs.get_mut(&id) {
                                run.state = RunState::Line {
                                    caption: caption_id,
                                    ends_us,
                                    voice,
                                };
                                run.pc = pc + 1;
                            }
                        }
                        Err(error) => {
                            self.finish(id, FinishReason::Error, Some(error.to_string()), out);
                        }
                    }
                    return;
                }
            }
        }
    }

    /// Refills the run's step cache when the asset's stamp changed. Returns false when the run
    /// stopped (the conversation asset is gone, or the run itself is gone).
    fn refresh_run_steps(&mut self, id: RunId, env: &dyn RunnerEnv, out: &mut Vec<RunnerOutput>) -> bool {
        enum Refresh {
            Ready,
            Missing(String),
            Fill(u64, Vec<Step>, BTreeMap<String, usize>),
        }
        let refresh = {
            let Some(run) = self.runs.get(&id) else {
                return false;
            };
            if !matches!(run.state, RunState::Running) {
                return true;
            }
            match env.conversation(&run.conversation) {
                None => Refresh::Missing(run.conversation.clone()),
                Some(asset) => {
                    let stamp = stamp_asset(asset);
                    if run.cached && run.stamp == stamp {
                        Refresh::Ready
                    } else {
                        let steps = asset.effective_steps();
                        let mut labels = BTreeMap::new();
                        for (index, step) in steps.iter().enumerate() {
                            if let Step::Label(name) = step {
                                labels.entry(name.clone()).or_insert(index);
                            }
                        }
                        Refresh::Fill(stamp, steps, labels)
                    }
                }
            }
        };
        match refresh {
            Refresh::Ready => true,
            Refresh::Missing(name) => {
                self.finish(
                    id,
                    FinishReason::Error,
                    Some(format!("conversation `{name}` is gone")),
                    out,
                );
                false
            }
            Refresh::Fill(stamp, steps, labels) => {
                if let Some(run) = self.runs.get_mut(&id) {
                    run.steps = steps;
                    run.labels = labels;
                    run.stamp = stamp;
                    run.cached = true;
                }
                true
            }
        }
    }

    fn label_index(&self, id: RunId, label: &str) -> Option<usize> {
        self.runs.get(&id)?.labels.get(label).copied()
    }

    fn set_pc(&mut self, id: RunId, pc: usize) {
        if let Some(run) = self.runs.get_mut(&id) {
            run.pc = pc;
        }
    }

    fn finish(
        &mut self,
        id: RunId,
        reason: FinishReason,
        detail: Option<String>,
        out: &mut Vec<RunnerOutput>,
    ) {
        if let Some(run) = self.runs.get_mut(&id) {
            run.state = RunState::Done;
        }
        out.push(RunnerOutput::Finished {
            run: id,
            reason,
            detail,
        });
    }
}
