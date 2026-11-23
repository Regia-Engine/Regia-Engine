# Dialogue lines and subtitles

Spec 36 §§ 7-10, ADR-1882. A **dialogue** (`.dialogue.ron`) is a list of spoken lines with context-specific
recordings; a **voice** (`.voice.ron`) is a speaker or listener with grammatical hints; a **cue set** (`<sound>.subs.ron`)
times the subtitle of one sound. The line player (`regia_framework::dialogue`, in `DefaultPlugins` with the `ui`
feature) plays a line's sound on the Voice bus and shows its subtitle on the caption board of
[conversations and captions](conversation.md), so `captions.state` is where an agent reads what is on screen.

Scripted conversations with steps and choices are the other system ([conversation.md](conversation.md)); a
dialogue is one line at a time, chosen by context.

## Files

```ron
// assets/dialogue/ranger.voice.ron
(format_version: 1, id: "ranger", name_key: "voice-ranger-name", gender: Feminine, number: Singular)

// assets/dialogue/ranger_intro.dialogue.ron
(
    format_version: 1,
    name: "ranger_intro",
    lines: [
        (id: "greeting", speaker: "ranger", listeners: ["player"],
         note: "Said when the player first arrives at the gate.",
         display_key: Some("dlg-ranger_intro-greeting-short"),
         variants: [
            (sound: "audio/dialogue/ranger_greeting.ogg"),
            (speaker: Some("ranger"), listener: Some("child"), sound: "audio/dialogue/ranger_greeting_child.ogg"),
         ]),
    ],
)

// assets/audio/dialogue/ranger_greeting.subs.ron (the sound's sidecar)
(format_version: 1, priority: 1, cues: [
    (at: 0.0, until: Some(2.2), text_key: Some("dlg-ranger_intro-greeting")),
    (at: 2.4, text: Some("[laughs]")),
])
```

- Ids (voice, dialogue name, line) are lowercase `[a-z0-9_]`, 1 to 48 characters. Unknown fields are errors; parse
  errors name `file:line:col`.
- `text_key` defaults to `dlg-<dialogue>-<line>`; `display_key`, when set, is what the subtitle shows (the transcript
  still goes to translators and recording sheets).
- **Variant selection:** a speaker match scores 2, a listener match 1, a field left `None` 0; a variant naming a
  speaker or listener that does not match is not a candidate. The highest score wins, ties go to the earlier variant.
- **Sound:** the variant's `sound` resolves along the audio locale chain: `assets/localized/<L>/<sound>` for each
  locale of the chain, then the base path.
- **Cues:** inline `cues` win; else the sound's `.subs.ron` sidecar (`a.ogg` → `a.subs.ron`, itself localized along the
  text chain). `at` inclusive, `until` exclusive, `until: None` runs to the next cue's start (or the sound's end). A
  cue set with `priority: 0` is never shown.
- **Fluent arguments:** `$speaker`, `$listener` (display names), `$speaker_gender`, `$speaker_number`,
  `$listener_gender`, `$listener_number` (`neutral|feminine|masculine|common|other`, `singular|plural`), so a
  translation can inflect: `{ $speaker_gender -> [feminine] ... *[other] ... }`.
- Texts are resolved when the line starts: a locale switch mid line leaves the running subtitle alone; the next line
  uses the new locale.

## Methods

| Method | Permission | Params | Result |
|---|---|---|---|
| `dialogue.list` | read | `{ dialogue? }` | `{ dialogues: [{ name, lines: [{ id, speaker, listeners, text_key, display_key, mature, variants }] }], voices, subtitle_sets, errors }` |
| `dialogue.resolve` | read | `{ dialogue, line, speaker?, listeners? }` | `{ variant, sound, resolved_sound, text, display_text, args, speaker, listeners, locale, cues }` |
| `dialogue.play` | play | `{ dialogue, line, speaker?, listeners?, priority?, position? }` | `{ handle, sound, caption, start_us, end_us }` |
| `dialogue.stop` | play | `{ handle?, fade_ms? }` | `{ stopped }` (absent handle: every line) |
| `subtitle_sets.status` | read | `{}` | `{ enabled, mode, active: [{ id, priority, owner, elapsed }], current: { lines, page, speaker } \| null }` |
| `subtitle_sets.queue` | play | `{ cues, duration, priority?, start_delay?, keep_breaks?, one_at_a_time?, kind?, speaker?, owner? }` | `{ id }` (`null` for priority 0) |
| `subtitle_sets.stop` | play | `{ id? }` | `{ stopped }` (absent id: every set) |
| `subtitle_sets.set_enabled` | play | `{ on }` | `{ enabled }` |
| `subtitle_sets.sync` | play | `{ id, playhead }` | `{ outcome: "in_sync" \| "rebased" \| "finished" }` |

A line ends `Finished` when the caption clock reaches its planned end and its voice stopped, or `Interrupted` by
`dialogue.stop`, by its voice being cut well before its end (stolen or stopped from outside), or by a level unloading;
all of them end its caption (`DialogueEvent` messages for game code). The planned length is the sound's length when the
voice player knows it, else the cues' end or a reading-time estimate. `position` plays the voice spatially there, on the
Voice bus.

`position` must contain three coordinates that remain finite as `f32`. An oversized coordinate
answers `F0002` before starting the voice or its caption.

**Cue sets.** `subtitle_sets.queue` puts timed cues on the caption board without a sound, with the same cue model as a
`.subs.ron`: `cues` is `[{ at, until?, text_key | text }]` in seconds, `duration` the seconds the set lives. Larger
`priority` wins; priority 0 is never shown. `kind: "caption"` is sound-effect text, shown only in caption mode. `mode` in
the status is `off`, `dialogue` or `all`. A gap between cues shows nothing: the entry stays `Visible` in `captions.state`
with an empty line, and no row is drawn. Rust: `regia_ui::captions::{queue_set, stop_set, stop_all_sets, stop_owner,
sync_set}`.

**Turning subtitles off.** `subtitle_sets.set_enabled { on: false }` sets the caption option `enabled` (spec 56): the
board hides speech and narration rows, including rows already on screen, so nothing is shown, but their sets still
advance and finish at their end (spec 36 § 8.4 (4), ADR-5993): turned back on mid-line, the rest of the line and the
set's later cues show. A set queued while off is dropped at once (spec 56 § 2.8). Sound captions follow `caption_mode`
the same way.

**One line at a time.** `one_at_a_time: true` (on `subtitle_sets.queue`, `SetRequest`, or in a `.subs.ron` sidecar that
the dialogue player reads) pages a long cue one line per page, shown in sequence over the cue's time, instead of
stacking up to the board's line limit. `captions.state` reports `one_at_a_time` on the entry. Off by default.

**Following a voice playhead.** `subtitle_sets.sync { id, playhead }` sets a live set's time to a voice position in
seconds of cue time (`start_delay` counts as passed). When the set's own time is within 40 ms of the playhead nothing
changes (`in_sync`), so a caller may sync every frame. Otherwise the set is sent again from the playhead under the same
id (`rebased`): cues that ended are dropped, the running cue keeps its remaining time, and a set that was showing keeps
its row ahead of waiting captions. A playhead at or past the set's duration stops it (`finished`). Rust:
`regia_ui::captions::sync_set` returns `SyncOutcome`.

Example (a voice that started 2.5 s before the set was queued):

```json
{"jsonrpc":"2.0","id":1,"method":"subtitle_sets.queue","params":{"cues":[{"at":0.0,"until":1.0,"text":"One"},{"at":2.0,"text":"Two"}],"duration":4.0}}
{"jsonrpc":"2.0","id":2,"method":"subtitle_sets.sync","params":{"id":1,"playhead":2.5}}
{"jsonrpc":"2.0","id":3,"method":"subtitle_sets.status","params":{}}
```

The second call answers `{ "outcome": "rebased" }`; after the next frame the status shows `current.lines` `["Two"]`
with the same id in `active`.

## Errors

Lowercase codes in `error.data.code`: `dialogue_unavailable` (no `DialoguePlugin`), `dialogue_unknown_asset`,
`dialogue_unknown_line`, `dialogue_unknown_voice` (a speaker or listener without `.voice.ron`), `dialogue_no_variant`,
`dialogue_bad_handle`; `subtitle_sets_unavailable` (no `CaptionPlugin`), `subtitle_sets_bad_cues`,
`subtitle_sets_unknown_id` (also from `subtitle_sets.sync`), `subtitle_sets_bad_playhead` (a missing, non-number,
negative or non-finite playhead).

## Settings

`accessibility.subtitle_mode` is `Off`, `Dialogue` or `All` (`None` derives from `subtitles`); `GameSettings::captions`
holds the caption options. A first run with the OS captions setting on (Android: the captioning setting; Windows: Show
Sounds) starts in `All`; otherwise `Dialogue`. `captions.state` reports the OS value as `system_captions` (`null` where
the platform has no such setting).

## Recording sheets

`regia loc dialogue-sheet export --locale fr --csv sheet.csv` writes one row per line: `line` (`<dialogue>#<line>`),
`speaker`, `listeners` (`;`-separated), `spoken_text`, `display_text`, `recording` (the expected localized path of the
first variant when it exists, else empty), `note`, then `source_text` and `source_display` (the source patterns, so
import can find stale rows). `regia loc dialogue-sheet import --locale fr --csv sheet.csv [--dry-run] [--strict]` reads
the edited texts into `assets/localization/fr/imported.ftl` through the same checks as `regia loc import` and warns
`loc_sheet_missing_recording` for recordings that do not exist. With `--strict` the sheet import is all or nothing: a
stale or fuzzy row fails it and no project file is written (`regia loc import --strict` writes the accepted entries and
then fails).

## Differences from spec 36 (ADR-1882)

- Sets run on the caption board, so `subtitle_sets.*` share its layout and options. Spec 36's area (90 % width, 3 lines)
  does not exist: the board's layout applies. A voiced line from the dialogue player stays on the sound through the
  shared sample clock; `subtitle_sets.sync` (ADR-2137) re-bases a cue set on a playhead the caller reads.
- `one_at_a_time` pages a long cue one line per page (ADR-2137).
- `subtitle_style` is `CaptionOptions`, persisted as `GameSettings::captions`.

## Validation

The spec 36 § 11 checks run in three places with the same codes: `regia loc validate` (as `loc` findings), `regia assets validate` / `regia cook` / `regia package` and the editor's save trigger (as the `dialogue_assets` validator of spec 32; see [Asset validators](asset-validators.md)). Codes: `dialogue_duplicate_line`, `dialogue_unknown_voice`, `dialogue_missing_text`, `dialogue_missing_sound`, `subtitle_past_end` (warning) and `dialogue_parse_error`. One implementation, `regia_framework::dialogue::validate_dialogues`, serves all three.
