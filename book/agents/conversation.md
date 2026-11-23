# Conversations, captions and subtitles (agents)

Guide topic: `agent.guide {"topic": "conversation"}`. ADR-1697, spec 56. Builds on the Fluent localization
(`ui-localization`) and the older subtitle queue (`ui.subtitles`, unchanged).

A **conversation** (`assets/conversations/<id>.conv.ron`) says who speaks, which Fluent key holds the words, which
recording carries the line in each language, and the order. A **caption** is "show this text from `start` for
`planned` microseconds"; dialogue lines, sound cues and your own code all send captions to the **caption board**,
which decides what is visible, wraps and paginates the text, holds each row for its reading time and lays rows out in
the safe area.

All times are integer microseconds. The conversation *runner* ticks on fixed-step simulation time
(`ConversationClock`, advanced once per `FixedUpdate`): line ends, events and choices land on the same fixed steps on
a live device, a replay and a headless run, and a voiced line ends at its planned end (the cooked clip length), never
on the mixer's playback state (ADR-4756). The caption *board* times its rows on the caption clock: fixed-step
simulation time, following the audio frame clock while a device runs, never wall time. Captions are stamped on the
caption clock (an unvoiced line at the caption clock's now, ADR-5572), so a row shows for its planned length even when
the two clocks differ. A jump back of the simulation clock (a render-out job starts at 0) does not stop the runner's
clock: it keeps counting forward. Advance both with the existing `play.step` method.

## Files

| File | Format | What |
|---|---|---|
| `assets/conversations/<id>.conv.ron` | `conversation` v1 | lines (speaker, text key, voice, duration, sub-cues, note) and steps |
| `assets/conversations/speakers.ron` | `conversation-speakers` v1 | speaker id, name key, label color, `voiced` |
| `assets/conversations/conversation.ron` | `conversation-settings` v1 | `voice_path` template, fallback and overlap policy, reading speed (the caption fields reach the board at run time) |
| `assets/conversations/cue_events.ron` | `conversation-cue-events` v1 | named cue event to caption key |

Unknown fields and a `format_version` newer than the build are errors that name `path:line:col`. Steps: `Line`,
`Label`, `Goto`, `If { cond, then, else }`, `Choice { prompt?, options }`, `Event`, `End`. Conditions are looked up by
name (the built-in flag set, then sources the game registers); there are no variables or expressions. A run that takes
more than `max_steps_per_frame` (64) non-blocking steps in one frame stops with an error.

## Methods

| Method | Permission | Params | Result |
|---|---|---|---|
| `conversation.play` | play | `conversation`, `locale?`, `from?`, `channel?`, `busy?` (`fail`/`replace`/`queue`) | `{run, queued, locale}` |
| `conversation.say` | play | `line` (`<conversation>#<line>`) or `text`, `speaker?`, `kind?`, `duration_ms?`, `priority?`, `voice?`, `locale?` | `{caption, start_us, planned_end_us, pieces}` |
| `conversation.choose` | play | `run`, `index` | `{run, state}` |
| `conversation.stop` | play | `run`, `fade_ms?` | `{stopped}` |
| `conversation.validate` | read | `conversation?`, `locale?`, `strict?` | `{issues, counts}` |
| `captions.state` | read | `include_layout?` | clock, options, entries, totals, rectangles |
| `captions.set_options` | play | any option, `persist?` | `{options, changed, persisted}` |

`captions.state` entries carry `state` (`Pending`, `Waiting`, `Visible`, `Done`, `Dropped`), `priority`, `start_us`,
`end_us`, `hold_until_us`, `page`/`pages`, the page `text`, wrapped `lines`, `rect` (logical pixels), `label`,
`label_side` (`Start`: the label comes before the text, on the reading-start side; `End`: after it; from `side` in
`speakers.ron`) and `rtl`. `hold_until_us` already includes the cap at the next piece of the same speaker: a row held
for reading speed ends when that speaker's next caption starts (never before its planned end or `min_display_ms`;
ADR-7237). A game that sets `CaptionPolicy::honor_newlines` keeps the line breaks of translated (Fluent) messages,
visible in `lines` (ADR-7237).
`rect` and `lines` let an agent assert text and line count without an image, or crop a `screenshot` to `rect`.

Options: `enabled`, `caption_mode` (sound captions on, speaker always labeled), `text_scale` (0.75 to 2.0 in steps of
0.05), `background` (`None`/`Box`/`Outline`), `background_opacity` (0 to 1; `Box` at 0 draws an outline), `speaker_labels`
(`Off`/`OnChange`/`Always`), `speaker_colors`, `text_color` (`white`, `yellow`, `#RRGGBB`), `position` (`Bottom`/`Top`),
`reading_cps` (8 to 40, `null` for the locale default). A speaker color below 4.5:1 against the caption background is
replaced by the text color and listed in `contrast_overrides`. `persist: true` also stores the options in the user
settings file (`GameSettings::captions`, and the caption mode `accessibility.subtitle_mode` from `enabled` and
`caption_mode`; ADR-1882); without saved settings it fails with `no_settings` and changes nothing. A bad `persist`
value is `bad_flag`.

### Errors

Lowercase codes in `error.data.code`: `plugin_missing` (no caption or conversation plugin: every method answers it
instead of failing), `conversation_not_found`, `locale_not_loaded`, `label_not_found`, `channel_busy`,
`line_not_found`, `speaker_not_found`, `text_too_long` (more than 512 graphemes), `run_not_found`, `no_choice_pending`,
`choice_out_of_range`, `invalid_option` (unknown option name), `out_of_range` (`data.min`, `data.max`), `invalid_color`.

## Recipes

### Make a conversation

1. Write `assets/conversations/speakers.ron` and `intro.conv.ron` (see the spec's § 4.1 example).
2. `regia loc gather` adds every line and sub-cue key to each locale's `gathered.ftl` with `# speaker:`, `# note:`,
   `# emotion:` and `# budget: N chars` comments; write the words there.
3. `conversation.validate` (or `regia loc validate-conv`) until it is clean, then `conversation.play`.

### Verify a subtitle layout

```json
{ "method": "ui.set_locale", "params": { "locale": "en" } }
{ "method": "conversation.say", "params": { "text": "A long line that will need to wrap onto a second row.", "duration_ms": 2000 } }
{ "method": "play.step", "params": { "frames": 2 } }
{ "method": "captions.state" }
→ { "entries": [ { "state": "Visible", "lines": ["A long line that will need to wrap", "onto a second row."], "rect": { "...": "..." } } ], "...": "..." }
{ "method": "captions.set_options", "params": { "text_scale": 1.5 } }
{ "method": "play.step", "params": { "frames": 1 } }
{ "method": "captions.state" }
```

`wait_until` can poll `captions.state` (`entries[0].state == "Done"`); `expect` checks line counts.

### Export a voice script and check recordings

- `regia loc export-vo --locale de --out vo-de.csv [--conversation intro] [--bom]` writes RFC 4180 CSV (UTF-8, CRLF),
  one row per voiced line in dialogue order: `conversation, line, speaker, speaker_name, locale, text, fallback_text,
  note, emotion, tags, chars, max_ms, filename, status`.
- `regia loc vo-report [--locale de] [--format text|csv|json] [--require]` lists per locale `ok`, `fallback`, `missing`
  and `orphan` lines with a coverage percentage; `--require` exits non-zero when any clip is missing.
- `regia loc validate-conv [--strict] [--conversation id] [--locale l]` prints the `conv_*` issues
  (`conv_missing_clip`, `conv_clip_fallback`, `conv_line_too_long`, `conv_untranslated`, `conv_cue_count_mismatch`,
  `conv_unknown_speaker`, `conv_dup_line`, `conv_bad_label`, `conv_unreachable`, `conv_no_exit`,
  `conv_speaker_contrast`, `conv_clip_missing_header`); exit 1 on an error.

## Sound captions

A sound cue (`.cue.ron`, format v3) may carry `caption: Some((key: "cap-door", kind: Sound, priority: 0, planned_ms: Some(1500)))`.
Each play of the cue sends one sound caption (not one per loop iteration) and ends it when the sound ends; sound captions show
only while `caption_mode` is on. v1 and v2 cue files load unchanged; `regia upgrade-project` re-stamps them as v3.

## On screen

`CaptionPlugin` keeps one `UiRoot` named `CaptionBlock` (sort order 10000, accessibility role `Status`, polite) with an
absolutely positioned child per visible row (`CaptionRow`), each with a background and a text child. Query it with
`query_entities`. Nothing is spawned until the first caption arrives.

## Packaged builds, clip timing and the clock

`regia package` cooks each conversation into `conversations/<id>.convc` (v1, postcard: the conversation and, per shipped
locale, each line's clip, length and fallback flag; ADR-1882) and leaves the `.conv.ron` source out; a clip missing in a
shipped locale fails the package with `conv_missing_clip` when `clip_policy` is `Require` and warns otherwise. A packaged
build loads the cooked files. With `rescale_cues: true` in `conversation.ron`, sub-cue offsets scale by
`clip_len(locale) / clip_len(source)` when both clips are known; by default the authored offsets win. Layout measures with the UI text shaper when the app has a `TextSystem`, and with a width model
(0.5 em per narrow grapheme, 1 em per wide one) otherwise (headless tests). Voice clips play from `assets/` through the engine `Audio` resource on the `Voice` bus.

**The clock.** While the audio output runs, the caption clock follows its sample clock (`frames * 1_000_000 / rate`,
`regia_audio::Audio::sample_clock`) and a voiced line's `CaptionStart` is stamped with the frame counter read when the
sound started, so text and sound begin on one sample. `captions.state` reports `clock_source: "device"`. Without a
running output (headless, no device, suspended, a render-out mixdown) it reports `"fixed"` and runs on fixed-step time.
A paused game holds the clock on either source, and the switch between them never jumps. Before any frame is rendered
an offline backend has no clock, so headless `step` runs stay deterministic; a test that calls `render_offline` gets
the frame counter.
