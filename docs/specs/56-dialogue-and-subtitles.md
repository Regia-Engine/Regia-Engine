# Spec 56: Conversations (dialogue), captions and subtitles

Status: built, ADR-1882 records the reconciliation with spec 36 (ADR-1579). Target rung: v0.3.x (b) for the asset, runtime player, presenter and agent
surface; v1.0 for the studio exports and the full validator. Crates: `regia_app` (shared caption messages),
`regia_framework` (conversation asset, runner, voice resolution), `regia_ui` (caption board and presenter),
`regia_audio` (sound-cue captions), `regia_cli` (`regia loc` extensions, and the `.convc` cook inside `regia package`,
ADR-1882). Conformance audit: ADR-5783 (2026-11-10).
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: `regia_ui::subtitles::SubtitleQueue` and agent `ui.subtitles` (ADR-1570, integ/ext2) are implemented, spec records gaps (§ 1.1).
Source area: runtime-media/runtime-media-subtitles, developer-tools/developer-tools-localization.

## 1. Purpose and scope

A game with spoken lines needs four things the engine does not yet have: an asset that says "who says what,
and which recording carries it in each language"; a small conversation runner; an on-screen presenter that
shows the text for the right time, wrapped and styled well enough to be read (and usable by players who need
captions); and hand-off files for the people who record. This spec adds them on top of the localization
workflow that already exists.

**Baseline (already built, not re-specified):** the Fluent runtime (`regia_ui::localization`, free function
`fallback_chain(locale, loaded, extra, source)`), `LocaleChanged`, plural table, `loc!`/`Loc(...)` keys,
`regia loc gather|validate|pseudo|plural`, the `loc_*` validator codes and `assets/localization/localization.ron`
(`docs/specs/content-data.md` § 1, `book/user-guide/localization.md`), the `SafeAreaPadding` component and the
accessibility tree (`A11yRole::Status`, `Politeness`) of `regia_ui`, the sound-cue asset (`regia_audio::cue`,
`CUE_VERSION` 2, `docs/specs/16-audio.md` § 2) and the `Voice` audio bus.

### 1.1 Relationship to other specs (read before building)

- **Built (integ/ext2, ADR-1570, implemented, spec records gaps):** `regia_ui::subtitles::SubtitleQueue` is a
  small scheduler: `SubtitleLine { speaker, text, key, args, start: f64, duration: f64, priority: i32 }`,
  `max_concurrent`, `advance_to(now)`, events; agent `ui.subtitles` (`list/push/advance/cancel/clear`); errors
  `subtitle_*`. It has no pages, wrapping, options, kinds, overlap policies or layout. This spec does **not**
  change it or its agent method. The new presenter is a separate type, `regia_ui::captions::CaptionBoard`, and an
  adapter (`impl From<&SubtitleLine> for CaptionStart` with `kind: Speech`, times converted to microseconds) lets
  a game route the old queue into the board. Names here are chosen not to collide: board/caption/conversation,
  never `SubtitleQueue`.
- **Spec 36 (localization exchange, dialogue, subtitles) overlaps.** The two were written in parallel; the lead
  reconciles them (open item for the lead, `reviews/r6.md`). This spec's boundary, written so both can be built:
  spec 36 owns PO exchange (`regia loc export|import`, its § 3), shipping bundles, locale detection, localized
  asset variants, and its `.dialogue.ron`/`.voice.ron`/`.subs.ron` per-line sound-variant assets and
  `SubtitleManager`. This spec owns the scripted **conversation** (`.conv.ron`, steps, choices, conditions), the
  clip-per-locale resolution, caption timing/reading-speed/pagination/overlap, `CaptionOptions`, sound captions and the
  voice-over script / report. To avoid collisions this spec uses the format name `conversation`, `conv_*`
  validator codes, `conversation.*` and `captions.*` agent methods, and `regia_ui::captions`. A line's text key
  flows through spec 36's exchange unchanged (it is a Fluent key in `.ftl`); this spec adds no PO code.
- Spec 51 (choosers) may supply a condition source (§ 3.2); spec 50 (video) may send `CaptionStart` (§ 3.1).

Everything below is **new** unless marked baseline.

Not in scope: speech recognition or synthesis, lip-sync, a visual conversation-graph editor (§ 11), a quest
or game-state system, PO exchange (spec 36), and the translation-management service itself.
Streams embedded in video containers are a video spec concern; this spec only defines the shared message
(§ 3.1) a video player can send.

## 2. Regia terms and model

- **Speaker**: a named voice identity (`guard`, `narrator`). Defined once per project in
  `assets/conversations/speakers.ron` and referenced by id. Carries a display-name key (a Fluent key), a color, and
  default `voiced: bool`.
- **Conversation line**: the unit of spoken text. Identity is `(conversation id, line id)`, written `intro#guard_greets`.
  Fields: `speaker`, `text` (a Fluent message key; the words live in `.ftl`, so every baseline tool applies),
  `note` (context for translators and the cast, free text), `emotion` (optional free tag), `tags` (optional
  list), `voice` (per-locale clip map, § 2.2), `duration` (§ 2.3), `cues` (optional timed sub-cues, § 2.4),
  `priority` override, and `max_lines` override.
- **Conversation**: an ordered list of lines and `steps` (§ 2.5), authored in `assets/conversations/<id>.conv.ron`.
- **Voice clip reference**: an asset path to a `Sound` (`docs/specs/16`) for one line and one locale.
- **Caption cue** (runtime message, § 3.1): "show this text, from `start` to `end`, for this source". Dialogue
  lines, sound cues carrying a caption, and timed cue events all become caption cues. The presenter knows
  nothing else.
- **Subtitle**: a caption cue whose kind is `Speech` or `Narration`. **Sound caption**: kind `Sound`
  (non-speech: `[door slams]`, `[tense music]`). Both are *captions*; "caption mode" (§ 2.8) is the option that
  shows sound captions and always shows who is speaking.
- **Clock**: the caption *board* times rows on a `CaptionClock`: the fixed-step simulation time by default,
  following the audio device clock (frames rendered / sample rate) when one exists, so text and sound hold
  together. The conversation *runner* never reads the device clock: it ticks on fixed-step simulation time
  (`ConversationClock`, advanced once per `FixedUpdate`), so lines, events and choices land on the same fixed
  steps on a live device, a replay and a headless run (ADR-4756). Caption stamps stay on the caption clock: an
  unvoiced line is stamped with the caption clock's now, a voiced one with its device start (ADR-5572). The
  runner's clock only moves forward, by the simulation clock's forward steps, so a reset of the simulation clock
  neither re-fires nor freezes a line. Never wall time (§ 7).

### 2.1 Speaker set

`speakers.ron` (format `conversation-speakers`, v1): `(format_version: 1, speakers: { "guard": (name_key: "speaker-guard",
color: "#F2C14E", voiced: true, side: Start) })` (ADR-5783). `color` is a text tint for the speaker label and optionally the text (option
`speaker_colors`). A speaker without `name_key` shows its id. `side` (`Start`, default, or `End`) puts the label
before the text (the reading-start side) or after it (as built, ADR-7237: `Label: text` or `text — Label`
in logical order, so the shaper's direction places it; `captions.state` reports `label_side`).

### 2.2 Voice resolution

Resolving the clip of a line for locale `L` walks the baseline fallback chain for `L`
(`regia_ui::localization::fallback_chain`, `content-data.md` § 1.3; the runner passes the chain in, so voice resolution is a pure function), then stops at the first locale `M` for which a
clip is known:

1. the line's explicit `voice[M]` path if present;
2. else the project template `voice_path` (in `assets/conversations/conversation.ron`) with `{locale}`, `{conversation}`,
   `{line}`, `{speaker}` and `{index}` (zero-padded line ordinal) substituted, if that asset exists.

Result `VoiceResolution { clip: Option<String>, locale: Option<Locale>, fallback: bool }`. `fallback` is
true when `M != L`. Project policy `voice_fallback` (default `Source`): `Source` plays the fallback clip with
the subtitle in `L`; `Silent` plays nothing and shows the subtitle for its estimated duration. A line whose
speaker has `voiced: false`, or whose line sets `voice: None`, never resolves a clip and never reports a
missing clip. Subtitle text always resolves in `L` through the normal Fluent chain, independent of the clip
locale.

### 2.3 Duration source

`duration` is an enum, evaluated when the line starts:

| Variant | Meaning |
|---|---|
| `Clip` (default when voiced) | The resolved clip's length. Cooked into the dialogue's per-locale table so the runtime need not open the clip first (§ 4.3); falls back to `Estimate` if no clip resolved. |
| `Fixed(ms)` | Authored length. |
| `Estimate` | `ceil(graphemes / estimate_cps)` seconds, floored at `min_display` (§ 2.7). Default for unvoiced lines. |

When the actual audio ends earlier or later than the planned duration (stream hiccup), the audio stop message
(§ 3.1) ends the cue; the presenter still holds it to `min_display` (§ 2.7).

### 2.4 Sub-cues

`cues: [(at_ms, text)]` splits a line into timed pieces. `text` is a Fluent key (for translation) and
`at_ms` an offset from the line start, in ascending order. Piece `k` is shown from `at_ms[k]` until
`at_ms[k+1]`, the last until the line ends. Without `cues` the line is one piece. Cue offsets are language
independent, so a translator must keep the same piece count; the validator checks it (§ 6.3). A locale
whose clip differs in length scales offsets by `clip_len(L) / clip_len(source)` when
`rescale_cues: true` (default false: authored offsets win).

### 2.5 Steps (conversations)

Lines run in file order unless `steps` is present. A step is one of:

| Step | Effect |
|---|---|
| `Line(id)` | Play that line, then continue. |
| `Label(name)` | Jump target; no effect. |
| `Goto(label)` | Continue at the label. |
| `If { cond, then, else }` | Evaluate `cond` (a condition name, § 3.4); jump to `then` or `else` label. |
| `Choice { prompt?, options: [(text, cond?, goto)] }` | Offer the options whose `cond` is absent or true; wait for a choice (`ConversationRunner::choose`, `conversation.choose`). If none are available, take the first option with no `cond`, else end. |
| `Event(name)` | Emit `ConversationEvent { run, name }` for game code. |
| `End` | Finish the run. |

There are no variables, loops with counters or expressions: a `Goto` cycle is legal (idle chatter) but a run
that executes more than `max_steps_per_frame` (default 64) non-blocking steps in one frame stops with
`ConversationError::StepBudget` (infinite-loop guard). Conditions are looked up, never computed here, which keeps
the format reviewable; richer logic is Rust game code reacting to `Event`.

### 2.6 Caption board

The presenter holds a bounded list of **entries** (`max_entries`, default 8 including waiting ones). An entry:
`id`, `source`, `speaker`, `kind`, `text`, `priority: u8`, `start_us`, `end_us`, `pages`, and state.

| State | Entered when | Leaves when |
|---|---|---|
| `Pending` | cue accepted with `start_us` in the future | clock reaches `start_us` |
| `Waiting` | start reached but no visible slot free (policy `Queue`), or lower priority than every visible row | a slot frees, or `end_us + max_wait` passes (then `Dropped`) |
| `Visible` | has a slot | `max(end_us, start_us + min_display)` passes, or cancelled/evicted |
| `Done` | normal end | removed next frame |
| `Dropped` | evicted, expired while waiting, or options filter it out | removed next frame (counted) |

**Overlap policy** (`overlap`, project default `Stack`): `Stack` shows up to `max_visible` (default 3) rows
newest at the bottom; a new higher-priority cue evicts the lowest-priority, then oldest visible row when full
(a row that already met `min_display` is evicted first). `ReplaceSameSpeaker` first ends any visible row of
the same speaker and kind `Speech`. `Queue` shows one row at a time in order of `(priority desc, start asc,
id asc)`. Sound captions rank below speech at equal priority and never evict speech. Ordering is a total
order, so ties never depend on hash order.

### 2.7 Timing and reading speed

- `min_display_ms` (default 1000) and a script-aware speed floor: the hold time of a piece is at least
  `graphemes / reading_cps` seconds, where `reading_cps` is an option (default 17 for alphabetic scripts,
  taken as the generally cited adult figure for English captions; a separate default of 9 for Han/Kana/Hangul, a conservative Regia placeholder, see the decisions in § 12). Graphemes are UAX #29 clusters, so combining marks and emoji sequences count once; spaces
  count as one.
- Cap on extension: a piece is not held past the start of the next piece of the same speaker, and not more
  than `max_extend_ms` (default 2000) past its planned end. As built (ADR-7237): the next piece is the
  earliest later-starting caption of the same speaker that is pending, waiting or visible; the cap never
  ends a row before its planned end or `min_display_ms` (the same floor a stop keeps).
- Pagination: if the wrapped text exceeds `max_lines`, the text is split at the best UAX #14 break near the
  proportional point into **pages**; planned time divides in proportion to graphemes, each page keeping the
  speed floor where the budget allows. If the budget cannot satisfy the floor, pages are still shown for the
  budgeted time and the entry is counted in `captions.state.underrun` (and the validator warns at author time).

### 2.8 Layout and options

Text is wrapped to a block of width `min(max_width_frac * safe_width, max_width_px)` (defaults 0.62 and
1400 logical px), at most `max_lines` lines (default 2, 3 when `text_scale >= 1.5`), centered (or aligned to
the reading start side for RTL scripts, see below), anchored to the bottom of the safe area plus
`bottom_offset`.

Line breaking uses UAX #14 break opportunities as exposed by the text shaper; within an entry, a greedy fill
is followed by a balancing pass that moves a break earlier when the last line holds under a third of the
first line and no prohibited break results. `keep_breaks: true` on a line (or a literal newline in a
translator-supplied message when the option `honor_newlines` is on) uses author breaks verbatim, with
overflow still wrapping. As built (ADR-7237): `honor_newlines` is a field of the board policy
(`CaptionPolicy`, default off) set by the game like `bottom_offset`, not a player option and not a
`conversation.ron` field; a `Key` piece whose resolved text holds a newline turns `keep_breaks` on for its
caption, and the game-literal pieces of that caption are flattened first. CJK text has no spaces, so any ideographic break opportunity is legal except where
UAX #14 forbids it (closing punctuation never begins a line, opening brackets never end one).

Bidirectional text uses the shaper's UAX #9 support (baseline: `cosmic-text`). The paragraph direction comes
from the locale (`ar`, `he`, `fa`, `ur` are RTL) not from the first character; the speaker label sits on the
reading-start side; digits and embedded Latin names keep their own direction.

`CaptionOptions` (resource, persisted in the user settings file, reflected):

| Option | Range / default | Notes |
|---|---|---|
| `enabled` | bool, `true` | Off drops `Speech` and `Narration` rows pushed while it is off (counted as `Dropped`), never `Sound`; rows already queued or shown are hidden and keep their timing (spec 36 § 8.4 (4), ADR-5993). |
| `caption_mode` | bool, `false` | On: sound captions shown, speaker label forced, `speaker_labels` = `Always`. |
| `text_scale` | 0.75..=2.0 step 0.05, 1.0 | Multiplies the base size (1/24 of the safe height at 1.0). |
| `background` | `None / Box / Outline`, `Box` | `Box` is a rectangle behind each row; `Outline` a stroke around glyphs. |
| `background_opacity` | 0.0..=1.0, 0.6 | At 0 with `Box`, the presenter uses `Outline` so text stays legible. |
| `speaker_labels` | `Off / OnChange / Always`, `OnChange` | `OnChange`: label when the speaker differs from the previous visible row. |
| `speaker_colors` | bool, `true` | Color never carries meaning alone: the label is also shown when it is the only cue and `caption_mode` is on. |
| `text_color` | preset or `#RRGGBB`, white | Presets include a high-contrast yellow-on-black. |
| `position` | `Bottom / Top`, `Bottom` | |
| `reading_cps` | 8..=40, locale default | Player-visible "slower display" control. |

Contrast: the effective text/background pair must meet 4.5:1 (WCAG 2.x, success criterion 1.4.3, computed
with the published relative-luminance formula against the background with opacity applied over black); a
speaker color that does not meet it is replaced by the neutral text color and `captions.state` lists the
speaker in `contrast_overrides`. The validator reports it at author time too.

Safe area: the block rectangle is the viewport minus the larger of the platform inset
(`SafeAreaPadding`, which on Android covers display cutouts and gesture/navigation bars) and a margin of 5%
of each side (title-safe margin), minus `bottom_offset` (default 0; a game moves captions above its own HUD).
The block rectangle is recomputed on resize, rotation and inset change; wrapping reruns, but an entry's
pages are *not* recomputed mid-display (a resize reflows within the current page only).

## 3. API shape in Rust

Layering (`docs/02-architecture.md`): `regia_audio` and `regia_ui` are both layer 2 and must not depend
on each other, so they communicate only through plain-data messages in `regia_app` (layer 0a; it re-exports
`bevy_ecs`, so `#[derive(Message)]` works there, which `regia_core` cannot do) and are wired together by
`regia_framework` (layer 3), which depends on both and therefore hosts everything that needs text *and* audio at
once: the conversation asset, runner and voice resolution. `cargo xtask check-layers` must pass. The editor and
importer stay out of runtime crates.

### 3.1 `regia_app::caption` (messages, no logic)

```rust
pub type SpeakerId = String; pub type Locale = String; pub type LocKey = String; // plain strings, as in regia_ui::localization
pub struct CaptionId(pub u64);                     // allocated by CaptionIds::next(), deterministic counter
pub enum CaptionKind { Speech, Narration, Sound }
pub struct CaptionStyleHint { pub speaker: Option<SpeakerId>, pub emphasis: bool }

#[derive(Message, Clone)]
pub struct CaptionStart {
    pub id: CaptionId, pub source: CaptionSource,   // Dialogue{run,line} | SoundCue(asset) | Video | Game(String)
    pub kind: CaptionKind, pub locale: Locale,
    pub pieces: Vec<CaptionPiece>,                  // (offset_us, CaptionText::Literal(String) | Key{key, args})
    pub speaker: Option<SpeakerId>, pub priority: u8,
    pub start: CaptionTime,                         // Now | AtMicros(u64)
    pub planned_us: u64, pub keep_breaks: bool, pub hint: CaptionStyleHint,
}
#[derive(Message, Clone)] pub struct CaptionStop { pub id: CaptionId }          // audio stopped / cancelled
#[derive(Message, Clone)] pub struct CaptionCueEvent { pub name: String, pub at_us: u64 } // timed cue event
```

`CaptionCueEvent` is the generic hook for sequencer markers (`17`, glossary "Marker") and animation events:
the project maps `name` to a Fluent key in `assets/conversations/cue_events.ron` (`(format_version: 1, events:
{ "door_slam": (key: "cap-door-slam", kind: Sound, planned_ms: 1500) })`, ADR-5783); an unmapped name is ignored.

### 3.2 `regia_framework::conversation` (layer 3: needs both audio and text)

```rust
pub struct ConversationAsset { pub id: String, pub lines: Vec<ConversationLine>, pub steps: Vec<Step>, pub notes: String, .. }
pub struct ConversationLine { pub id: String, pub speaker: SpeakerId, pub text: LocKey, pub note: String,
    pub emotion: Option<String>, pub tags: Vec<String>, pub voice: VoiceSpec, pub duration: DurationSource,
    pub cues: Vec<SubCue>, pub priority: Option<u8>, pub keep_breaks: bool }
pub enum VoiceSpec { Auto, None, Explicit(BTreeMap<Locale, String>) }
pub fn resolve_voice(line: &ConversationLine, ctx: &VoiceContext, locale: &Locale) -> VoiceResolution;

pub struct ConversationRunner;       // resource: channels -> runs
impl ConversationRunner {
    pub fn play(&mut self, req: PlayRequest) -> Result<RunId, ConversationError>;
    pub fn say(&mut self, req: SayRequest) -> Result<CaptionId, ConversationError>;
    pub fn choose(&mut self, run: RunId, index: usize) -> Result<(), ConversationError>;
    pub fn stop(&mut self, run: RunId, fade_ms: u32);  pub fn skip(&mut self, run: RunId);
    pub fn state(&self) -> Vec<RunInfo>;
}
pub trait ConditionSource: Send + Sync { fn eval(&self, name: &str) -> Option<bool>; }  // None = unknown
pub struct FlagSet(BTreeSet<String>);                // built-in source: name present = true
pub enum ConversationError { NotFound(String), LocaleNotLoaded(Locale), LabelNotFound(String), ChannelBusy(String),
    StepBudget, NoChoicePending, ChoiceOutOfRange, TextTooLong, Asset(String) }
pub struct ConversationPlugin;    // runner system in PostUpdate on ConversationClock (fixed-step sim time)
```

Messages out: `ConversationEvent { run, name }`, `ConversationChoiceRequested { run, options: Vec<ChoiceView> }`,
`ConversationFinished { run, reason: Finished | Stopped | Error }`; plus the `CaptionStart/Stop` of § 3.1. The
runner (which resolves line text through `Localization` itself, so its pieces are `Literal`) starts the voice through the existing audio play path (a `SoundInstanceId` from `regia_audio::VoiceManager`) on the `Voice` bus and sends `CaptionStart` with
`start: AtMicros(audio_clock_at_start)` read from the same call, so text and sound begin on the same
clock sample. A line ends when the runner's clock reaches the planned end (`start + planned_us`, the cooked
integer clip length for `DurationSource::Clip`), never on the mixer's playback state: the mixer renders on the
device thread, and a live run, a replay and a machine with a different buffer size would otherwise end the line
on different steps (ADR-4756). At the planned end the runner sends `CaptionStop` for a voiced line (the audio stop of
§ 2.3); an unvoiced line's caption ends at the same planned end on the board, which keeps its reading-speed hold
(ADR-5783). Stopping a run early fades the voice as before.

**Conditions** (§ 2.5): `eval` is asked for the name in `If`/`Choice`. The runner holds a chain of sources
queried in order; the first `Some` wins; all `None` is treated as false and logged once per name. The
built-in `FlagSet` source is always present. A later spec (51, written in parallel) may add a source that
evaluates tag expressions or a chooser; this spec only requires that such a source implements the trait and is
registered with `ConversationRunner::add_source`. Nothing here depends on it.

**Sound captions**: `SoundCue` (`.cue.ron`) gains optional `caption: Option<CaptionDef { key: LocKey, kind,
priority, planned_ms: Option }>`. `CUE_VERSION` goes 2 -> 3 with the field defaulting to `None` (`#[serde(default)]`); every v2
file loads unchanged, and `regia upgrade-project` gets a v2 -> v3 step beside `migrate_cue` (it stamps `version: 3` the way the v1 -> v2 step stamps 2; `docs/specs/12` § 4a). Because `regia_audio` cannot resolve Fluent text (layer rule), the cue sends `CaptionText::Key`; the caption board resolves keys when it accepts the message. Playing such a cue sends `CaptionStart { kind: Sound, source: SoundCue(..) }` once
per play (not per loop iteration) and `CaptionStop` when it stops.

### 3.3 `regia_ui::captions`

```rust
pub struct CaptionPlugin;                    // reads CaptionStart/Stop/CueEvent; PostUpdate: queue then layout; UI root
pub struct CaptionBoard { /* bounded Vec<Entry>, no allocation per frame in steady state */ }
impl CaptionBoard {
    pub fn push(&mut self, c: CaptionStart, now_us: u64, opts: &CaptionOptions);
    pub fn stop(&mut self, id: CaptionId, now_us: u64);
    pub fn advance(&mut self, now_us: u64, opts: &CaptionOptions, layout: &mut dyn BlockLayout);
    pub fn snapshot(&self) -> CaptionSnapshot;     // what captions.state returns
}
pub trait TextMeasure { fn width(&mut self, text: &str, size_px: f32, rtl: bool) -> f32; }  // fake in tests
pub fn wrap(text: &str, width: f32, max_lines: usize, rtl: bool, m: &mut dyn TextMeasure) -> Wrapped; // pure
pub fn paginate(text: &str, planned_us: u64, ..) -> Vec<Page>;                                    // pure
pub fn effective_colors(opts: &CaptionOptions, speaker_color: Rgba) -> (Rgba, Rgba, bool);        // pure
```

`wrap`, `paginate`, `effective_colors` and `CaptionBoard::advance` are pure over their inputs and take the
clock as an argument, so every rule of § 2 is testable with a fake `TextMeasure` (monospace, CJK full-width)
and no GPU. The plugin spawns one `UiRoot` ("CaptionBlock", above gameplay UI, below system menus) with row
child nodes reusing the pooled `UiNode` text nodes, accessible role `Status` with polite politeness
(baseline `A11yRole`), so screen readers can be told of new captions (`a11y` live region), and honors
`CaptionOptions` changes the same frame.

### 3.4 Plugins and reflection

`CaptionOptions`, `Speaker`, `ConversationAsset` and all enums derive `Reflect + Serialize + Deserialize`
(`Reflect` registered so the inspector and `set_property` work). `CaptionBoard` and `ConversationRunner` are
resources, not reflected. `regia_framework` adds both plugins to its default group; they are inert when no
dialogue asset is played and no caption arrives.

## 4. Files and formats

All new formats (`conversation`, `conversation-speakers`, `conversation-cue-events`, `conversation-settings`, cooked `convc`; the names `cargo xtask check-formats` registers, ADR-5783) start at `format_version: 1`; the lead registers them with `cargo xtask check-formats --update`. The existing `cue` format moves to v3 (§ 3.2). A change that breaks a v1 reader bumps the version and ships a
migrator registered with the asset migration list (`docs/specs/12-assets.md` § 4a); `regia upgrade-project`
runs it. Additive optional fields do not bump the version.

### 4.1 `assets/conversations/<id>.conv.ron` (format `conversation`)

```ron
(
    format_version: 1,
    id: "intro",
    notes: "Gate scene. Guard is tired, not hostile.",
    lines: [
        (id: "greet", speaker: "guard", text: "conv-intro-greet", emotion: Some("weary"),
         note: "Said while waving the player in.",
         voice: Auto, duration: Clip, tags: ["gate"]),
        (id: "warn", speaker: "guard", text: "conv-intro-warn", voice: Explicit({"en": "audio/vo/en/gate_warn.ogg"}),
         duration: Fixed(2600), cues: [(0, "conv-intro-warn-a"), (1300, "conv-intro-warn-b")]),
    ],
    steps: [ Line("greet"), If(cond: "met_captain", then: "short", else: "long"),
             Label("long"), Line("warn"), End, Label("short"), End ],
)
```

`text` keys are baseline Fluent keys; the baseline gather finds them because they are `Loc`-style strings in
a `.ron` file (§ 1.1 of `content-data.md`), so `regia loc gather` adds them to every locale without change.
Project settings: `assets/conversations/conversation.ron` (`voice_path`, `voice_fallback`, `estimate_cps`,
`overlap`, `max_visible`, `min_display_ms`, `max_extend_ms`, `reading_cps` per script); this file is format `conversation-settings`, v1. Its caption fields (`overlap`, `max_visible`, `min_display_ms`, `max_extend_ms`, `reading_cps`, `reading_cps_cjk`) are written into the caption board's policy whenever they change and into a board inserted later; a file with any non-default caption field owns all six, one with all six at their defaults leaves a game-set policy alone; other board fields stay as the game set them (ADR-5783, ADR-5936). Unknown fields, including `key_prefix`, are rejected.

### 4.2 Other source files

`speakers.ron` (§ 2.1) and `cue_events.ron` (§ 3.1) are RON structs with `format_version: 1` that hold the map under
`speakers` and `events` (ADR-5783). Unknown fields
are an error with the path and line (house convention).

### 4.3 Cook output

`regia package` (`regia_cli`, ADR-1882 (i)) cooks each conversation into `<id>.convc` (postcard, `format_version` first): lines, steps,
resolved text keys, and a per-locale table `{ line -> (clip path, duration_us, fallback) }` built with the
same resolution as § 2.2 against the staged locales; clip durations are read from the cooked sound headers
(frames / sample rate, integer). A clip listed but absent from the pak is a cook error `conv_missing_clip`
only when the locale is in the project's staged locale list and policy is `require`; otherwise a warning. Text is
not cooked into the file (it stays in the `.ftl` locale data), so a string fix never re-cooks audio tables.

## 5. Agent-protocol surface (`book/agents/conversation.md`)

Error codes are lowercase ascii/digits/underscore. Access: `read` observes, `play` changes runtime state
(`docs/specs/23-agent-native.md`). All methods answer `error: "plugin_missing"` (no crash) if the plugin
is absent.

| Method | Access | Params | Result | Errors |
|---|---|---|---|---|
| `conversation.play` | play | `conversation`, `locale?`, `from?` (label), `channel?` ("main"), `busy?` (`fail`/`replace`/`queue`, default `fail`) | `{run, queued, locale}` | `conversation_not_found`, `locale_not_loaded`, `label_not_found`, `channel_busy` |
| `conversation.say` | play | `line` (`<id>#<line>`) **or** `text`; `speaker?`, `kind?`, `duration_ms?`, `priority?`, `voice?` (asset path), `locale?` | `{caption, start_us, planned_end_us, pieces}` | `line_not_found`, `invalid_params`, `text_too_long` (>512 graphemes), `speaker_not_found` |
| `conversation.choose` | play | `run`, `index` | `{run, state}` | `run_not_found`, `no_choice_pending`, `choice_out_of_range` |
| `conversation.stop` | play | `run`, `fade_ms?` | `{stopped}` | `run_not_found` |
| `captions.state` | read | `include_layout?` (default true) | below | none |
| `captions.set_options` | play | any subset of § 2.8 options; `persist?` | `{options, changed[]}` | `invalid_option` (unknown name), `out_of_range` (with `min`/`max`), `invalid_color` |
| `conversation.validate` | read | `conversation?` (all if absent), `locale?`, `strict?` | `{issues:[{code, severity, conversation, line, locale, message, fix}], counts}` | `conversation_not_found`, `locale_not_loaded` |

`conversation.play` does not start a line, so the result has no `first_caption`, `voice_locale` or
`fallback`. `queued` is true and `run` is null when `busy` is `queue` and the channel is already in use;
otherwise `queued` is false and `run` is the new run id.

`captions.state` result: `{clock_us, options, entries:[{id, source, kind, speaker, state, priority,
start_us, end_us, hold_until_us, page, pages, text, lines[], rect:{x,y,w,h}, label?}], queue_len,
dropped_total, underrun_total, contrast_overrides[], block_rect, safe_rect}`. `rect` and `lines` are
logical pixels after wrapping, so an agent can (a) assert text and line count without an image, and (b) crop
a `screenshot` of the window to `rect` and compare it to a golden or inspect it. The block root entity is
named `CaptionBlock`, queryable with `query_entities`; the existing `step` method advances the clock by
whole fixed steps, `wait_until` can poll `captions.state` (for example `entries[0].state == "Done"`), and
`expect` checks line counts. Verification recipe in the guide: set `locale`, `conversation.say` a long line,
`step` N, `captions.state`, `screenshot`, then `captions.set_options { text_scale: 1.5 }` and repeat.

`regia loc` additions are CLI-only (§ 6), and `conversation.validate` runs the
same validator in-process for the project on disk.

Guide page: `book/agents/conversation.md` (task pages: "make a conversation", "verify a subtitle layout",
"export a voice script"). `book/COVERAGE.md` rows (one per method), for example:
`| conversation.play | conversation_play | [conversation](agents/conversation.md) |`, and likewise `conversation.say`,
`conversation.choose`, `conversation.stop`, `captions.state`, `captions.set_options`, `conversation.validate`.
`cargo xtask agent-coverage` must pass with every method having a schema row, contract and guide topic.

## 6. Translation, recording and validation workflow

Conversation line text and sub-cue text are ordinary Fluent keys, so `regia loc gather|validate` and spec 36's
PO exchange (`regia loc export|import`, spec 36 § 3) handle them with no code here. Three small hooks make that
work: (1) the conversation loader registers every `text` and sub-cue key with the gather scan (the same hook
spec 36 uses for its dialogue assets; if that hook is not yet built, the builder adds the `.conv.ron` keys to the
gather pass directly); (2) each line's `note`, `emotion` and speaker are written above the message in the
gathered `.ftl` as a Fluent comment (`# note: ...`), which spec 36's exporter already carries into PO `#.`
comments; (3) `max_chars_hint` (derived: `reading_cps * planned seconds`, see § 6.3 `conv_line_too_long`) is added
to that comment as `# budget: 38 chars`. There is no PO code, no source-hash comment and no `conversation.ftl` in
this spec: staleness detection is spec 36's.

All commands below are deterministic, write atomically (temp file + rename), support `--dry-run` where they
write, and exit non-zero on error.

**6.1 Voice-over script.** `regia loc export-vo --locale <L> [--conversation <id>...] --out <file.csv>`. RFC 4180
CSV, UTF-8 without BOM (`--bom` adds one for spreadsheet imports), CRLF line ends, fields quoted when they
contain comma, quote or newline. One row per voiced line in dialogue order. Columns, in order: `conversation`,
`line`, `speaker`, `speaker_name` (resolved in `L`), `locale`, `text` (resolved in `L`, falling back to the
source with `fallback_text=true` in the next column), `fallback_text`, `note`, `emotion`, `tags`
(semicolon-joined), `chars`, `max_ms` (the line's planned length for `Fixed`, or the source clip length, so a
studio can match timing), `filename` (the file name, i.e. last path component, of the `voice_path` substitution: the name the recording must have; ADR-5783), `status`
(`recorded` / `missing` / `fallback`). Rows are stable for equal input (sorted by dialogue id, then file order),
so successive exports diff cleanly; a re-export never reorders existing lines.

**6.2 Missing-VO report.** `regia loc vo-report [--locale <L>...] [--format text|csv|json]` lists per locale
and line: `ok` (own clip), `fallback` (uses another locale's clip), `missing` (voiced line, no clip anywhere in
the chain), `orphan` (a clip under `voice_path` that no line names). Summary counts per locale and a coverage
percentage (`ok` / voiced lines). Exit is non-zero with `--require` when any `missing` exists.

**6.3 Validator.** `regia loc validate-conv` runs the dialogue checks (ADR-1697 (4); `regia loc validate` and `gather`
scan the conversation keys), and `conversation.validate` runs the same checks in-process; the baseline `loc_*` codes still
cover text keys. New codes:

| Code | Meaning | Severity |
|---|---|---|
| `conv_missing_clip` | Voiced line has no clip in `L`'s chain (own or fallback) | error if `require`, else warning |
| `conv_clip_fallback` | Line uses another locale's clip | info |
| `conv_line_too_long` | Text exceeds the budget for its duration: `graphemes / duration > reading_cps`, or text wraps to more than `max_lines` at the reference layout (1920x1080 logical, scale 1.0) | warning (error with `--strict`) |
| `conv_untranslated` | Line text key absent from `L` itself (a fallback supplies it) | warning |
| `conv_cue_count_mismatch` | A locale's sub-cue pieces differ in count from the source | error |
| `conv_unknown_speaker` | `speaker` not in `speakers.ron` | error |
| `conv_dup_line` | Two lines share an id in one dialogue | error |
| `conv_bad_label` | `Goto`/`If`/`Choice` names a label that does not exist; duplicate label | error |
| `conv_unreachable` | A line or step no path from the start can reach | warning |
| `conv_no_exit` | A reachable cycle with no `End`, `Choice` or blocking `Line` (infinite non-blocking loop) | error |
| `conv_speaker_contrast` | Speaker color fails 4.5:1 against the default background | warning |
| `conv_clip_missing_header` | Clip exists but its duration cannot be read | error |

Each issue has `code`, `severity`, conversation, line, locale, message and a `fix` hint.

## 7. Determinism notes

- The queue is a pure function of `(entries, now_us, options, measure)`. `now_us` comes from the fixed step
  (`step_count * step_us`) or the audio frame counter (`frames * 1_000_000 / sample_rate`, integer, rounded
  down); wall-clock time never enters. Two runs with the same input messages and the same `step` calls produce
  identical `captions.state` (bit-exact for times, ids, states, page splits and text).
- `CaptionId`s come from one counter incremented in message-processing order; message order is the ECS
  schedule order, fixed by system sets (conversation runner before caption board, same frame).
- Randomness: none in this feature. (A dialogue variation, if added later, must use a seeded `Rng` stream
  named per run.)
- Tolerance-based: pixel rects depend on the font, so layout assertions in tests use the fake monospace
  measure (exact); with the real shaper they compare line count and text per line (exact) and `rect` within
  1 px. Screenshots are never asserted bit-exact across GPUs.
- Condition evaluation order is source-chain order; `Choice` options keep file order.

## 8. Tests and acceptance

Each item is a numbered test or check; items marked [GPU] need a window and a device, everything else runs on
`cargo test` without one.

1. Parse: golden `.conv.ron` round-trips; unknown field, bad version and duplicate line id give path+line errors.
2. Voice resolution: with chain `pt-BR -> pt-PT -> pt -> en`, own clip wins; missing own clip uses `pt-PT`
   with `fallback=true`; `voiced: false` and `voice: None` never resolve; `Silent` policy yields no clip.
3. Duration: `Clip` uses the cooked table; `Estimate` uses `ceil(graphemes/estimate_cps)` and the `min_display`
   floor; `Fixed` is exact; missing clip degrades to `Estimate`.
4. Steps: `If` takes `then` for a true source, `else` for false and for unknown; the first `Some` in the source
   chain wins; `Choice` filters by `cond`; a `Goto` loop stops at `max_steps_per_frame`; `Event` is emitted once.
5. Queue timing: with a scripted clock, a cue with `start_us` in the future is `Pending` then `Visible` on the
   exact fixed step; a 400 ms cue holding 40 graphemes stays visible until `grapheme/reading_cps`; the hold never
   exceeds `max_extend_ms`; a `CaptionStop` before `min_display` still holds to `min_display`.
6. Overlap: `Stack` evicts lowest priority then oldest; `ReplaceSameSpeaker` replaces only speech rows of that
   speaker; `Queue` shows one at a time in `(priority, start, id)` order; sound captions never evict speech;
   a total order test shuffles input and obtains the same snapshot.
7. Wrap (fake measure): UAX #14 breaks at spaces and hyphens only where allowed; CJK breaks between ideographs
   and never before closing punctuation or after opening brackets; RTL paragraphs set direction from the
   locale; `max_lines` triggers pagination; page time splits by grapheme proportion; pages fit.
8. Options: every option's range is enforced; `background_opacity: 0` with `Box` falls back to `Outline`;
   a speaker color with contrast < 4.5:1 is replaced and listed in `contrast_overrides`; `enabled: false` drops
   speech, keeps sound captions; `caption_mode` forces labels.
9. Safe area: with an inset of (24, 0, 0, 48) px the block rectangle sits above 48 px; a resize recomputes the
   rect and wrapping; a page already shown is not repaginated.
10. Gather hook: a fixture `.conv.ron` makes `regia loc gather` add every line and sub-cue key to a locale's
    `gathered.ftl` with the `# note:` / `# budget:` comments; a second gather changes no byte.
11. CSV: fixture matches RFC 4180 quoting (embedded comma, quote, newline); column order and row order fixed;
    `--bom` prefix is exactly EF BB BF; the filename column equals the file name of the `voice_path` substitution.
12. Reports/validator: a fixture project with one missing clip, one fallback, one orphan, one over-long line,
    one cue-count mismatch, one dead label, one infinite loop yields exactly those codes and counts; `--strict`
    promotes warnings; exit codes as specified.
13. Agent protocol (headless, offline audio backend `OfflineBackend`): `conversation.say` -> `step` x N ->
    `captions.state` shows the entry `Visible` then `Done`; `conversation.play` on a busy channel returns
    `channel_busy`; `captions.set_options` rejects `text_scale: 3`; `conversation.validate` returns the same
    issues as the CLI; the no-plugin degradation answers `plugin_missing`. `cargo xtask agent-coverage` and
    `check-layers` pass.
14. Audio coupling: with the offline backend, `CaptionStart.start` equals the audio frame counter at
    `play` and the line ends at the planned end on the runner's clock (the cooked clip length), whatever the
    mixer reports (frame-counter assertion, no sleeps).
15. [GPU] Screenshot of a 3-line RTL and a CJK subtitle at scales 1.0 and 2.0 on 1280x720 and a 720x1600
    portrait Android viewport; crops to `rect` look right to the reviewer. Golden images are a TODO for the
    lead and are never created by the builder.

## 9. Platform notes

- **Linux / Windows**: only difference is the title-safe margin default (5%) and the settings file location.
- **Android aarch64**: the safe area comes from the platform inset (cutouts, gesture bar) via the baseline
  `SafeAreaPadding` path; rotation and fold changes re-lay out. Fonts: the bundled CJK/Arabic fallback must be
  staged (the staged-locale font list in `Regia.toml`) or those scripts show empty boxes; the validator
  warns when a locale in `locales` has no font covering its script. Touch: captions are non-interactive
  (they never take input focus). TalkBack announcement of each new caption uses the live-region path with
  polite politeness; check on device.
- **macOS** (testing only): same as Linux; nothing macOS-specific.
- **Dependencies**: none new in runtime crates. Shaping and bidi use `cosmic-text`; grapheme counting uses
  `unicode-segmentation` (both baseline in `docs/03`). UAX #14 break opportunities: if `cosmic-text` does not
  expose them for the needed cases, proposed `unicode-linebreak` 0.1 (MIT) in `regia_ui`; needs a `docs/03`
  row and `cargo deny check`. CSV is written by hand (a small, specified format; spec 36 offers `regia_asset::csv` for the same purpose, reuse it when it exists). No PO code here. Audio
  clip duration reads reuse the existing decoder (`symphonia` baseline).

## 10. Performance budget (min-spec machine class)

- `caption_board` (tracing span): `advance` for <= 8 entries: < 0.02 ms per frame, no allocation in steady
  state (pooled strings and row nodes).
- `caption_layout`: wrap + paginate one entry of <= 200 graphemes: < 0.3 ms, run only when text, options,
  locale or block rectangle change (cached otherwise); at most 2 entries re-wrapped per frame (rest next frame).
- `dialogue_runner`: < 0.02 ms per frame for 4 runs; line start (voice resolve + CaptionStart) < 0.2 ms.
- Memory: presenter < 256 KiB resident including glyph rows; a 5,000-line dialogue table (cooked) < 1 MiB.
- Validator on 10,000 lines x 12 locales: < 5 s on the desktop dev machine (offline tool; not a runtime budget).

## 11. Out of scope / follow-ups

Visual conversation-graph editor; lip-sync and facial animation hooks; player-facing dialogue UI widgets
(choice wheel); gender/number voice variants (the engine picks one clip per line and locale; variants need
content-side separate lines); video-embedded subtitle streams (message hook defined, decoder elsewhere);
live translation; per-speaker spatial placement of captions; karaoke-style word highlighting (WebVTT/TTML
import and export is built: `regia loc cues export|import`, ADR-3359); condition source backed by
spec 51 choosers (optional, § 3.2).

## 12. Provenance and open questions

| Area | Algorithm/behavior | Public source |
|---|---|---|
| Timed cue model | Cue = text + start/end on a media clock; cue stacking and ordering | W3C WebVTT: The Web Video Text Tracks Format (https://www.w3.org/TR/webvtt1/); W3C TTML 2 (https://www.w3.org/TR/ttml2/) |
| Line breaking | Break opportunities, CJK prohibited line-start/end classes | Unicode Standard Annex #14, Line Breaking Algorithm (https://www.unicode.org/reports/tr14/) |
| Grapheme counting, bidi | Extended grapheme clusters; paragraph direction and mixed-direction text | Unicode UAX #29 (https://www.unicode.org/reports/tr29/); UAX #9 (https://www.unicode.org/reports/tr9/) |
| Reading speed and line count | Characters-per-second ceilings, two-line captions, balanced breaks, minimum display time | BBC Subtitle Guidelines (https://bbc.github.io/subtitle-guidelines/); Netflix Timed Text Style Guide, general requirements (https://partnerhelp.netflixstudios.com/hc/en-us/articles/215758617) |
| Contrast | Relative luminance and 4.5:1 ratio | W3C WCAG 2.2, success criterion 1.4.3 (https://www.w3.org/TR/WCAG22/#contrast-minimum) |
| Studio script | CSV quoting and line ends | RFC 4180 (https://www.rfc-editor.org/rfc/rfc4180) |

Provenance row for this spec is in the table above (a row for the shared `docs/provenance.md` exists as ADR-1579).
Behavior notes taken from the public documentation concern only the feature areas (voice per culture with
fallback, duration/priority/wrap options, a recording script); structure, names and defaults here are Regia's own.

### Decisions (ADR-1585)

- Reading-speed defaults: 17 characters per second for alphabetic scripts (the figure public caption guidance cites
  for adult English captions). For Han/Kana/Hangul the default is 9, a conservative Regia placeholder that is
  *not* taken from a cited page; it is overridable per script in `conversation.ron`, and the validator only warns.
  Before v1.0 the owner or a localization reviewer should replace it with a cited figure (not an owner decision
  about money or legal risk; a content-quality task).
- Shared messages live in `regia_app::caption` (see § 3), not `regia_core` (no ECS there).
- Conversation text stays in `.ftl` (not in `.conv.ron`), so the baseline tools apply; the split across two
  files is accepted.
- `conversation.choose` and `conversation.stop` stay: a conversation with `Choice` cannot be driven without them.
- `CUE_VERSION` 2 -> 3 with a migrator and `regia upgrade-project` support.
- No new dependency is required; `unicode-linebreak` is an optional fallback needing a docs/03 row, an ADR and
  `cargo deny check` (MIT/Apache-2.0 licence acceptable; pure Rust).
- Both new crates modules respect layers: nothing in `regia_audio` or `regia_ui` names the other.

### Owner decisions

None.

## Sequence sound sections (ADR-3334)

A sequencer sound section whose sound is a dialogue line's variant sound requests that line's cues, one
one-cue set at a time, through `regia_ui::captions::queue_set` and `sync_set` (owner = the player entity).
The cue is picked from the section's playback position, so a seek shows the cue at the new position, a time
scale of `r` makes a cue last `length / r` sequence seconds, and a pause holds the cue. The text is resolved
when the cue starts: a locale change shows at the next cue. During a render-out nothing is requested unless
`SequenceSubtitleSettings::burn_in` is set. Opt-out: track name suffix `.nosubs` or the settings lists.

### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>

## 13. Conversation graph editor (ADR-3298)

The follow-up named in § 11. The editor's **Conversation** panel (`regia_editor::conversation_panel`) opens one
`.conv.ron` and shows it as a graph; the agent surface is `editor.conversation_open`, `_graph`, `_edit`, `_save` and
`_preview_step` (`book/agents/conversation-editor.md`). The `conversation` format (§ 4.1) is unchanged.

- **Graph.** A block node holds `Label`, `Line` and `Event` steps; a block starts at step 0, at every `Label` and after
  every leaving step (`Goto`, `If`, `Choice`, `End`), and a `Goto` ends its block. `If`, `Choice` and `End` are nodes
  of their own. Edges: `next` (fall-through), `goto`, `then`/`else`, `option` (one per option). A jump to a missing
  label is an edge without a target. Node ids are names (`label:<l>`, `line:<id>`, `<kind>:<n>`).
- **Layout.** Layered: layer = breadth-first depth from the entry (unreached nodes walk on below), order in a layer by
  the parents' barycenter, ties by step order. It reads no text. Dragged positions are kept in the editor-side sidecar
  `<project>/.regia/conversation_layout/<file>.layout.ron` (`format_version: 1`), never in the asset.
- **Edits and undo.** Ops: `add_line`, `add_step`, `delete_step`, `move_step`, `set_line`, `set_prompt`, `add_option`,
  `set_option`, `delete_option`, `move_option`, `set_cond`, `set_event`, `retarget`, `rename_label`; a batch applies
  all or nothing as one undo step. Every edit writes `asset::to_ron`; opening keeps the bytes, so open-and-save and
  edit-and-undo are byte-identical. A file that does not parse opens read-only with its `path:line:col` error.
- **Findings.** The § 6.3 validator on the open asset (all project locales and clips) plus `conv_empty_choice` (a
  `Choice` with no options; error), each drawn on its node and listed; a click centres the node.
- **Preview.** From any node, with the runner's step rules (§ 2.5), a mocked condition table, choice picking and the
  line's voice clip played on request. Lines do not wait for time.

Acceptance (headless editor tests, no goldens): the fixture's nodes and edges; positions stable when a line's text
changes; each op plus undo restores identical bytes; a label rename updates every jump; each seeded fault is found on
its node; the preview follows choices and conditions; open-and-save is byte-identical; a malformed asset opens
read-only with the parse error.

## 12. Dialogue UI widgets (ADR-3234)

- **Choice widgets** (`regia_ui::choice`): a list or a wheel over `ChoiceOption { label, disabled_reason }`. Inputs: up/down/next/prev
  step to the next enabled option with wrap; left/right step on a wheel (reversed in right-to-left locales); a stick selects the
  sector by angle; pointer move focuses an enabled option, press or touch picks it; a number key picks that option. Disabled
  options are skipped, can be hovered and show their reason, and answer a press with a `Rejected` event. Focus lands on the default
  option (timer default) or the first enabled one on open. The timer counts fixed steps; the pick fires on the step the count
  reaches zero. The list mirrors in right-to-left locales: the marker sits at the right edge and the text aligns to the right.
  Result: the `ChoicePicked { widget, index (None = cancel), source }` message.
- **Karaoke**: an off-by-default setting. The words of the visible page get timings by splitting the page window in proportion to
  estimated syllables (§ karaoke module docs); sub-cues are separate pieces so each starts a word. The highlight is a color of
  the spoken prefix of the already wrapped lines; it never changes the text, so wrapping and RTL are identical.
  The syllable estimate (ADR-5959, dependency-free): vowel groups (`a e i o u y`, accented Latin vowels), and for words spelled
  in ASCII letters these English rules: `y` before a vowel is a consonant (`yes`, `player`); a final `e` after a consonant is
  silent (`one`, `five`, `there`) except consonant + `le` (`table`); a final `ed` is silent unless after `t`/`d` (`jumped`;
  `wanted`); a final `es` is silent unless after `s x z c g ch sh` (`makes`; `boxes`, `wishes`); a word-final `ia`/`io` after a
  consonant other than `c s t g` is two syllables (`radio`, `media`). A silent ending is dropped only when another vowel group
  remains. Words with no vowel (digits, `hmm`) weigh one unit per three characters; ideographs weigh one per grapheme. It is an
  estimate for even pacing, not a dictionary: authored per-word timings (sub-cues) always win.
- **Anchored captions**: a setting (`anchored` or `bottom_band`). A caption follows a world point or entity, projected with the
  host's camera; it is placed above the anchor, clamped into the safe area; an anchor outside the safe area gets the box at the
  nearest edge and an arrow; boxes never overlap (later ids slide by a box height); a missing or behind-camera anchor and a lack
  of room fall back to the bottom band.
- **Bridge**: `ConversationChoiceBridgePlugin` shows the widget for a `Choice` step and calls `choose` with the pick.
