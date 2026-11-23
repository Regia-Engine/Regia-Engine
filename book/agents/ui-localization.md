# UI localization pipeline and subtitles (agents)

Guide topic: `agent.guide {"topic": "ui-localization"}`. ADR-1570. Extends the Fluent `.ftl` localization of
`regia_ui::localization`. Translator PO and XLIFF exchange is `regia loc export` / `regia loc import`
([user guide](../user-guide/localization.md)).

The pipeline is pure logic over in-memory sources, so tools, tests and the editor run it the same way. Reading the
files and any command-line wiring are the caller's job.

## Gather: the key manifest

`regia_ui::loc_manifest::gather(&[GatherSource])` scans `.ftl` files, UI assets and scenes and returns a `Gathered`
(its `manifest` is the `KeyManifest`, plus the parsed `messages` and the `diagnostics`). The manifest holds every key, where each locale defines it (path, line, column) and where UI assets and scenes use it.

- In `.ui.ron` and scene text, a string literal is a reference when it starts with `loc:` (`"loc:menu-start"`) or is the
  argument of `Loc(...)`. Comments (`//`, nested `/* */`) are skipped; raw strings and escapes work.
- In `.ftl` text, a message at the start of a line is a key. Terms (`-brand`) and attributes are not keys.
- Sources are processed in path order, so the same inputs give the same manifest and the same JSON.
- Problems come back as diagnostics with stable codes: `ftl_syntax`, `duplicate_key`, `invalid_key`,
  `unterminated_string`, `unterminated_comment`, `empty_locale`.
- `KeyManifest::undefined_keys()` lists keys used but defined nowhere; `unused_keys()` lists keys defined but never
  used (a hint only: keys built at run time look unused). `to_json()` and `to_text()` write the manifest.

## Compile: tables, fallback chains, missing-key report

`loc_compile::compile(&gathered, &CompileOptions::new("en"))` builds one `CompiledTable` per locale. Each entry keeps
the verbatim Fluent source, the locale it came from and where it is defined. A locale falls back through its shorter
tags (`zh-Hant-TW`, `zh-Hant`, `zh`) and ends at the default locale; `CompileOptions::with_chain` gives a locale an
explicit list instead. The last definition of a key inside a locale wins.

`LocalizationReport` lists per locale the keys it lacks (`missing`), the keys no locale in its chain can show
(`unresolved`), `$variables` that differ from the default locale's text (`placeholder_mismatches`) and the coverage
share. `unknown_chain_locales` (only present when not empty) names locales an explicit chain lists that have no `.ftl`
source, usually a typo. `is_clean()` is false when anything is unresolved, undefined or mismatched. `CompileOutput::to_localization`
turns the tables into a runtime `Localization`. Errors: `no_locales`, `unknown_default_locale`.

## Debug locales

- **Pseudo** (`en-XA`, `loc_pseudo::pseudo_table`): ascii letters become accented look-alikes, text grows by a ratio
  (default 30 %, at most 200 %) and is wrapped in `«` `»`. Fluent syntax is kept: ids, placeables, variant keys and
  attribute names are not touched, so the table still loads. The tag `en-XA` gives it English plural rules.
- **Key display** (`en-XD`, `loc_pseudo::key_display_table`): every key shows as `⟪key⟫`, with its `$variables`
  (`⟪hello name=Zoé⟫`). Keys that are used but undefined show too.
- `debug_locales(&output, "en", &PseudoOptions::default())` returns both as `(locale, ftl)` pairs for
  `CompileOutput::to_localization`.

## Subtitle queue

`regia_ui::subtitles::SubtitleQueue` (a resource, inserted by `UiPlugin`). A `SubtitleLine` has a speaker, text or a
localization key, a start time, a duration and a priority. A line is active from `start` to `start + duration`. At most
`max_concurrent` active lines show: highest priority first, then earlier start, then push order. Shown lines list in
display order (earlier start first). The queue has no clock: `advance_to(now)` takes the time and returns
`SubtitleEvent`s (`Shown`, or `Hidden` with `expired`, `preempted` or `removed`). Time cannot go backwards. Expired lines
are dropped. The game must call `advance_to` from its own clock; `UiPlugin` does not tick the queue.

## `ui.localization`

`ui.localization { action? }` (`read` permission). `report` (the default) answers the loaded locales' coverage against
all keys any locale defines:

```json
{ "method": "ui.localization" }
→ { "selected": "en", "fallback": "en", "total_keys": 3,
    "locales": [ { "locale": "en", "defined": 3, "coverage": 1.0, "missing_count": 0, "missing": [] },
                 { "locale": "fr", "defined": 1, "coverage": 0.33, "missing_count": 2, "missing": ["b", "c"] } ],
    "debug_locales": { "pseudo": "en-XA", "keys": "en-XD" } }
{ "method": "ui.localization", "params": { "action": "pseudo", "text": "Start game", "expansion": 0.5 } }
→ { "text": "«Šţáŕţ ĝáɱé~~~~~»", "input_chars": 10, "output_chars": 17 }
```

`missing` lists at most 50 keys (the count is exact). Pseudo takes `text` (required, at most 64 KiB), `expansion`,
`accents` and `brackets`. Error: `no_localization` (the project has no `.ftl` files).

## `ui.subtitles`

`ui.subtitles { action?, ... }` (`play` permission). Actions: `list` (default, answers `applied: false`), `push`
(`speaker`, `text` and/or `key` with `args`, `duration` required, `start` defaults to the queue's `now`, `priority`),
`advance` (`to` or `by`, exactly one; answers `events`), `cancel` (`id`), `clear` and `limit` (`max_concurrent`, 1 to 64).
Every answer is the queue after the call: `now`, `max_concurrent`, `count`, `queued` (push order, with `visible` and the
resolved `text`) and `visible` (ids in display order).

```json
{ "method": "ui.subtitles", "params": { "action": "push", "speaker": "Ana", "text": "Hello", "duration": 2.0 } }
{ "method": "ui.subtitles", "params": { "action": "advance", "by": 0.5 } }
→ { "now": 0.5, "visible": [1], "events": [ { "event": "shown", "id": 1 } ], "...": "..." }
```

Errors (lowercase codes in `error.data.code`): `subtitle_empty_text`, `subtitle_bad_start`,
`subtitle_bad_duration`, `subtitle_bad_time` (time went backwards or is not finite), `subtitle_queue_full` (256 lines)
and `unknown_subtitle` (cancel of an id that is gone).

## Recipes

- *Which labels overflow in German?* Gather and compile, load `debug_locales`, switch with `ui.set_locale
  {"locale": "en-XA"}` and screenshot.
- *Which key is this label?* Switch to `en-XD` and read the label.
- *Does a loud alarm cut off dialog?* Push both lines with different priorities and `max_concurrent` 1, advance past
  both starts and read `visible`.

## `ui.format_preview`

`ui.format_preview { kind?, locale?, value?, unit?, items?, options? }` (`read` permission) formats a value the way
Fluent's `NUMBER()` and `DATETIME()` show it in localized text (`regia_ui::loc_format`, ADR-2698). `kind` is `number`
(default), `date`, `time`, `datetime`, `relative`, `list` or `locales`; `locale` defaults to the selected locale (else `en`).
`options` takes the Fluent names: `style`, `currency`, `currencyDisplay`, `useGrouping`, `minimumIntegerDigits`,
`minimumFractionDigits`, `maximumFractionDigits`, `minimumSignificantDigits`, `maximumSignificantDigits`, `roundingMode`,
`signDisplay`, `numberingSystem`; `dateStyle`, `timeStyle`, `hour12`, `hourCycle`, `timeZone` (a fixed offset such as
`+02:00`; there is no time-zone database); `numeric` (`always`/`auto`) for `relative`; `type` (`conjunction`/`disjunction`)
for `list`. Unknown names are listed in `ignored_options`, as Fluent ignores them.

```json
{ "method": "ui.format_preview", "params": { "locale": "fr-FR", "value": 1234.5, "options": { "style": "currency", "currency": "EUR" } } }
→ { "kind": "number", "locale": "fr-FR", "chain": ["fr-FR", "fr", "root"], "text": "1 234,50 €",
    "escaped": "1\\u{202f}234,50\\u{a0}€", "ignored_options": [] }
{ "method": "ui.format_preview", "params": { "kind": "date", "locale": "ja-JP", "value": "2026-11-05", "options": { "dateStyle": "long" } } }
→ { "text": "2026年11月5日", "...": "..." }
{ "method": "ui.format_preview", "params": { "kind": "relative", "locale": "ru", "value": 5, "unit": "day" } }
→ { "text": "через 5 дней", "...": "..." }
{ "method": "ui.format_preview", "params": { "kind": "list", "locale": "en-US", "items": ["a", "b", "c"] } }
→ { "text": "a, b, and c", "...": "..." }
```

`value` is a number or text (`"NaN"`, `"inf"`) for numbers and relative times, and unix seconds or ISO 8601 text for dates
(unix seconds show at `timeZone`, else at `Localization::utc_offset_seconds`). `escaped` spells no-break spaces and bidi
marks as `\u{..}`. `chain` is the data lookup order; `kind: "locales"` lists every tag the table has. Native digits are
opt-in (`numberingSystem: "native"`); the pseudo locale `en-XA` formats as English and pseudo mode never changes a
formatted value.

Errors (lowercase codes in `error.data.code`): `format_bad_option` (an option value the formatter does not accept, such as
`maximumFractionDigits: 500`, a currency style without `currency`, or an IANA zone name), `format_bad_value` (text that
is not a number, a date that does not exist, NaN or infinity as a relative time) and `format_bad_pattern` (a malformed
pattern in the engine's table; report it). In a message the same failures render Fluent's error value (`NUMBER()`).

## Thai, Lao, Khmer and Myanmar line breaking

These scripts (Unicode UAX #14 line-break class SA) write words without spaces, so the shaper's
word wrap has no break opportunities of its own and used to fall back to mid-word glyph breaks.
The engine runs ICU's dictionary/LSTM segmentation over SA runs and wraps at the word boundaries
it finds — in UI text, captions and subtitles alike — and word-wise caret movement (Ctrl+arrows,
double-click) steps over the same words. Carets, selection and hit testing still index the
original string: the break opportunities are invisible zero-width markers removed from every
reported offset. Texts without SA characters shape exactly as before. Segmentation is cached
with the shaped text (an LRU keyed by the string), never re-run per frame.

Bundled fonts cover Thai (Noto Sans Thai). Lao, Khmer and Myanmar wrap correctly but need a font
to draw: a game shipping those locales loads Noto Sans Lao / Noto Sans Khmer / Noto Sans Myanmar
(OFL-1.1) as project fonts, or the text reports `missing_glyphs`.

## `ui.text_breaks`

```
{ "method": "ui.text_breaks", "params": { "text": "ฉันกินข้าว" } }
→ { "text": "ฉันกินข้าว", "locale": null, "breaks": [3, 6], "words": ["ฉัน", "กิน", "ข้าว"] }
```

`ui.text_breaks {text, locale?}` reports the dictionary word boundaries a string wraps at:
`breaks` holds the character offsets between words (positions count characters, not bytes, like
`ui.text_state`), `words` holds the words. Text that contains an SA character lists every break
the wrapper is given for it, so its non-SA part shows its UAX #14 breaks too (`"hello ฉันกิน"`
answers `breaks: [6, 9]`, `words: ["hello ", "ฉัน", "กิน"]`). The result is empty (`breaks: []`,
`words: []`) for text without an SA character — Latin, CJK and spaced scripts follow their normal UAX #14 rules,
which `ui.text_shape` exercises end to end. `locale` is optional and informational: segmentation
follows the script of the text's own characters, so any BCP 47 tag is accepted and echoed; it
selects nothing yet. The same segmentation drives wrapping and caret movement, so a translation
that splits badly here will wrap the same way on screen (ADR-3323).

Errors (lowercase codes in `error.data.code`): `ui_text_missing` (no `text`), `bad_text` (`text`
is not a string), `text_too_long` (over 4096 characters), `bad_locale` (`locale` is not a
string).
