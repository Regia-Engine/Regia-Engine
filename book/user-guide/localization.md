# Localization

<!-- index: Localization -->
<!-- index: Fluent -->
<!-- index: Locale -->
<!-- index: Pseudo-localization -->
<!-- index: Plural rules -->
<!-- index: PO file -->
<!-- index: XLIFF -->

Regia's UI text comes from [Project Fluent](https://projectfluent.org/) files, one directory per locale. This page
walks through the workflow: write keys in content and code, gather them into the locale files, translate, validate,
test layout with pseudo-localization, and switch locales at runtime. The design is in
[content data](../../docs/specs/content-data.md) § 1. Exchange files, dialogue recordings, and subtitle
cues are [spec 36](../../docs/specs/36-localization-exchange-dialogue-subtitles.md). The line player
and caption board are [spec 56](../../docs/specs/56-dialogue-and-subtitles.md) and
[Dialogue](dialogue.md). Agents use the [localization methods](../agents/localization.md).

## Files

```
assets/localization/
  localization.ron        # optional: source locale, locales, fallbacks, gather roots
  en/main.ftl             # every .ftl in a locale directory is combined (sorted by name)
  en/gathered.ftl         # written by `regia loc gather`
  fr/main.ftl
  fr-CA/main.ftl
  ru/main.ftl
```

`localization.ron`:

```ron
(
    format_version: 1,
    source: "en",
    locales: ["en", "fr", "fr-CA", "ru"],
    fallbacks: {},
    gather: (roots: ["assets", "src"], exclude: ["assets/localization"]),
)
```

`source` is the locale keys are written in and the last fallback of every chain. `locales` is what the gather tool
writes; when empty, every locale directory counts. `fallbacks` adds edges tried before a locale's parents (for
example `"pt-BR": ["pt-PT"]`).

## Worked example: a main menu in three locales

### 1. Name the text

In a `.ui.ron` layout, a text span is `Loc("key")`:

```ron
(
    version: 2,
    root: (
        name: "main_menu",
        children: [
            (name: "start", widget: button, text: (spans: [Loc("menu-start")])),
            (name: "saves", text: (spans: [Loc("menu-saves")])),
            (name: "quit", widget: button, text: (spans: [Loc("menu-quit")])),
        ],
    ),
)
```

In any other `.ron` asset or scene, write `"loc:menu-start"` in a string field. In Rust, use the `loc!` macro (the
key must be a string literal, so the gather tool can find it):

```rust
use regia_ui::{loc, Localization};

fn saves_label(localization: &Localization, count: usize) -> String {
    loc!("menu-saves", count = count).resolve(localization)
}
```

### 2. Gather

```sh
regia loc gather            # or --dry-run to only print the plan
```

The gather tool reads every `.ron` and `.rs` file under the gather roots and appends the keys a locale lacks to
`assets/localization/<locale>/gathered.ftl`. A symlinked file is read through the link; a symlinked folder is not
followed (it could loop) and is named in a warning. The source locale gets the key as placeholder text
(`menu-quit = menu-quit`); other locales get a copy of the source message under `# TODO translate`. Existing
messages are never rewritten.

### 3. Translate

`en/main.ftl`:

```ftl
menu-start = Start Game
menu-quit = Quit
menu-saves = { $count ->
    [one] One saved game
   *[other] { $count } saved games
}
```

`fr/main.ftl` (French treats 0 and 1 alike):

```ftl
menu-start = Commencer
menu-saves = { $count ->
    [one] { $count } partie sauvegardée
   *[other] { $count } parties sauvegardées
}
```

`ru/main.ftl` (Russian has four forms: 1 and 21 are `one`, 2-4 and 22-24 are `few`, 5-20 are `many`, fractions are
`other`):

```ftl
menu-start = Начать игру
menu-quit = Выход
menu-saves = { $count ->
    [one] { $count } сохранение
    [few] { $count } сохранения
    [many] { $count } сохранений
   *[other] { $count } сохранения
}
```

`fr-CA/main.ftl` only overrides what differs: `menu-start = Démarrer la partie`.

At runtime, with `fr-CA` selected: `menu-start` is "Démarrer la partie", `menu-saves` with 2 is "2 parties
sauvegardées" (from `fr`), and `menu-quit` is "Quit" (from `en`: `fr` has no translation yet).

### 4. Validate

```sh
regia loc validate          # --strict makes warnings fail; --json for CI and agents
```

For this example it reports `loc_untranslated` for `menu-quit` in `fr` and `fr-CA`. Other checks: keys missing from
a whole fallback chain, keys nothing uses, keys only one locale has, a translation that uses a `$variable` the source
does not pass, a plural select without a category the locale needs (a Russian `menu-saves` with only `[one]` and
`*[other]`), a variant the locale never selects (`[few]` in English), duplicate keys and parse errors. Every
finding names the file and line. The exit status is 1 when the check fails.

To check plural rules by hand:

```sh
regia loc plural ru 1 3 5 21 1.5
# ru (cardinal): one, few, many, other
#   1 -> one
#   3 -> few
#   5 -> many
#   21 -> one
#   1.5 -> other
```

### 5. Test the layout with pseudo-localization

```sh
regia loc pseudo            # writes assets/localization/en-XA/ from the source locale
                            # (--to names another folder; an existing locale needs --force)
```

Select `en-XA` in the game (or turn on pseudo mode at runtime, below). "Start Game" becomes `[Šŧáŕŧ Ĝáɱé~~~~]`:
accented so hard-coded English stands out, about 40 % longer so tight layouts overflow now rather than in German,
and bracketed so clipped text is visible.

### 6. Switch at runtime

```rust
use regia::prelude::*;
use regia_ui::{LocaleChanged, Localization};

fn language_menu(mut localization: ResMut<Localization>) {
    // fr-CA when loaded, else its nearest loaded parent (fr).
    localization.set_locale("fr-CA");
}

fn on_locale_changed(mut changed: MessageReader<LocaleChanged>) {
    for change in changed.read() {
        info!("language {} -> {}", change.from, change.to);
    }
}
```

Every text re-resolves on the next frame; `LocaleChanged` arrives the frame after the switch. The start-up locale is
chosen as the next section describes. `Localization::set_pseudo(true)`
turns on pseudo mode for every locale without writing files.

## Choosing the language at start-up

The game starts in the first of these that matches a locale you ship (a tag matches exactly, then through its
parents, ignoring case: `pt-br` finds `pt-BR`, `fr-CA` finds `fr`):

1. the `--locale <tag>` launch flag (`mygame --locale ja`);
2. the `REGIA_LOCALE` environment variable;
3. the language saved in the player's settings;
4. the language the player picked for the game in its Steam properties (when running under Steam);
5. the operating system's language;
6. your project's source locale.

A player who picks a language in your language menu has the choice saved, so it wins next time. To build the menu,
give every locale a message with its own name, which shows the same in every language:

```ftl
# assets/localization/de/main.ftl
locale-name = Deutsch
```

Send `PlayerSetLocale("de".into())` from the menu button. `regia loc validate` warns about a locale that lacks
`locale-name` once any locale has one, and about fallback edges in `localization.ron` that form a cycle. The
agent method `localization.status` says which source chose the language.

Text in Japanese, Korean and Traditional Chinese uses the matching regional letter forms when the system has such a
font installed; the bundled font has Simplified Chinese forms for all of them.

## Plural rules

Plurals follow the Unicode CLDR rules for en, fr, de, es, it, pt, nl, sv, ru, uk, pl, cs, ar, he, ja, zh, ko, tr
and the languages that share their rules. Exact numbers (`[0] No saved games`) are matched before categories.
`regia loc plural <locale> <numbers>...` lists a locale's categories. Ordinals ("1st, 2nd, 3rd") follow the CLDR ordinal
table: `regia loc plural --kind ordinal en 1 2 3 11` prints `one, two, few, other`, and a select over
`NUMBER($n, type: "ordinal")` picks the ordinal form.

## Translator exchange (PO and XLIFF files)

Fluent stays the source of truth. To hand a locale to a translator who uses a standard gettext tool:

```
regia loc export --locale fr [--out loc-export/fr.po] [--only untranslated|stale|all]
regia loc import loc-export/fr.po [--locale fr] [--dry-run] [--strict]
```

`export` writes one entry per message of the source locale: `msgctxt` is the key, `msgid` the source pattern exactly
as written in Fluent (placeables and selects stay, so translators see and keep them) and `msgstr` the current
translation (empty when there is none). Comments carry the variables the message passes (`variables: $count`), your
`# note: ...` comment from above the source message, the plural categories the target locale needs, and the places
the key is used; every entry is flagged `fluent-format`. The default output is `.regia/loc/<locale>.po`. Output is
sorted by key and never wrapped, so a diff against an earlier export shows only real changes. Attributes and terms
are not exported (the command says how many messages had attributes and how many terms it left out; with `--json`,
`skipped_attributes` and `skipped_terms`).

`import` reads the returned file. It refuses a file whose `Language` header differs from `--locale`
(`loc_po_language_mismatch`). For each entry with a translation it skips the entry when the key does not exist
(`loc_po_unknown_key`), the source text changed since the export (`loc_po_stale`), the entry is marked `fuzzy`
(`loc_po_fuzzy`), the text does not parse as Fluent (`loc_po_bad_pattern`), or it drops a variable the source passes
(`loc_variable_mismatch`). A missing plural category is a warning and the entry is imported
(`loc_plural_missing`). Accepted entries go to `assets/localization/<locale>/imported.ftl` (sorted by key, regenerated
whole from earlier imports plus this file, each message under a `# regia-source:` hash of the text it was translated
against, which is how `--only stale` finds translations to redo), and a matching `# TODO translate` placeholder in
`gathered.ftl` is removed. Hand-written files win over `imported.ftl`. Importing the same file twice leaves the
files byte for byte the same; `--dry-run` prints the report and writes nothing; `--strict` makes stale and fuzzy
entries fail the import (exit 1).

**Translation memory and the glossary.** When the project already translated a source string, `export` offers it:
untranslated entries carry a `tm-exact:` or `tm-near (NN%):` comment with the earlier translation, and an exact match
also fills `msgstr` and flags the entry `regia-review` (review it before approving — it is a suggestion, not a
translation). Until it is approved the import skips it as `loc_po_fuzzy`: in PO the entry is also `fuzzy`, which
your PO editor clears when you approve it; in XLIFF its segment state is `initial`, which your CAT tool moves on
when you confirm it. `assets/localization/glossary.locglossary.ron` (optional, format `glossary` v1) lists product terms and
their approved translation per locale:

```ron
(format_version: 1, terms: [
    (term: "Save", translations: { "de": "Speichern", "fr": "Enregistrer" }, note: "verb, not noun"),
])
```

`regia loc validate` warns (`loc_glossary_mismatch`) when a message uses a term but its translation in that locale
does not use the approved translation. See `docs/formats/glossary-locglossary-ron.md`.

### XLIFF 2.0

Most translation vendors and CAT tools take XLIFF rather than PO. The same commands speak it:

```
regia loc export --locale fr --format xliff [--out loc-export/fr.xlf] [--only untranslated|stale|all]
regia loc import loc-export/fr.xlf [--locale fr] [--dry-run] [--strict]
```

`import` tells the two formats apart by content, so one command reads either. The XLIFF file has the same entries as
the PO file, in the same order, with the same stale (`loc_po_stale`), fuzzy, unknown-key, parse and variable checks
(it is read into the PO entries and goes through the same import step), so importing the same translations from PO
or from XLIFF writes the same `imported.ftl` bytes. One `<unit>` per message (`id` and `name` are the key), a
`<segment>` with `<source>` and `<target>` (`state="translated"`, or `initial` when empty), the variables, your
`# note:` comment, the plural categories and the places the key is used as `<note>`s. A unit marked
`rg:fuzzy="yes"` (the `urn:regia:xliff:1` namespace) is the equivalent of a PO `fuzzy` flag.

Placeholders are protected. Every Fluent placeable (`{ $name }`, `{ -brand }`, `{ NUMBER($n) }`) is an inline
`<ph canDelete="no" equiv="{ $name }">`, which translator tools show as one opaque tag. A translator can move a tag
or use it twice, not edit it: on import each tag is replaced by the source's own text for its `id`, whatever the
file says. A translation that deletes a protected tag is skipped with `loc_xliff_placeholder_missing`, one that
invents a tag with `loc_xliff_unknown_code`, and one with inline elements Regia does not take (`<pc>`, `<sc>`,
`<ec>`) with `loc_xliff_unsupported_inline`. The structure of a select (`{ $count ->`, each `[one]` and `*[other]`
key, the closing brace) is also a tag, deletable (`canDelete="yes"`) so a language can have other plural
categories; the variant texts between them are translatable, and `loc_plural_missing` and
`loc_variable_mismatch` judge the result. A unit split into several segments by a tool is joined; a unit with only
some segments translated is skipped (`loc_xliff_incomplete_segment`).

The reader is strict: XLIFF 2.x only (`loc_xliff_version`; 1.2 files are refused), no DTD or custom entities
(`loc_xliff_doctype`, `loc_xliff_entity`), malformed XML is `loc_xliff_syntax`, and a file past the limits (32 MiB,
24 levels deep, 2,000,000 nodes, 32 attributes per element, 1 MiB per text) is `loc_xliff_limit`. A file whose
`trgLang` differs from `--locale` is refused like a PO `Language` mismatch (`loc_po_language_mismatch`).

The writer has one check of its own: `regia loc export --format xliff` fails, writing nothing, when a key, source or
translation holds a character XML 1.0 cannot carry (a control character other than tab, line feed and carriage
return, or U+FFFE / U+FFFF). The error names the code `loc_xliff_bad_char`, the message key and the code point (`U+XXXX`);
remove the character and export again.

## Checking that the fonts can draw every translated character

A translation can be correct and still show boxes ("tofu") because no shipped font has a glyph for one of its
characters. `regia loc check --glyphs` (the same command as `regia loc validate`) finds that before players do:

```
regia loc check --glyphs [--font extra.ttf] [--charset-out build/charsets] [--json]
```

For every locale it reads every message's displayed text (text, all plural and select variants, attributes, terms,
string literals) and checks each visible character against the font chain: the fonts under your `assets/fonts/`
(`.ttf`, `.otf`, `.ttc`, `.otc`, in path order), then each `--font`, then the fonts Regia bundles (Inter plus the
Noto subsets for Arabic, Hebrew, Devanagari, Thai, Chinese, Korean and color emoji). The pseudo-locale (`en-XA`,
made in memory from the source locale unless you wrote one) is checked too. Each message with an uncovered
character is one `loc_glyph_missing` error with the message id and `file:line` of the text, naming the characters
(`U+A66E 'ꙮ'`), so it fails the check like any error. Spaces, control characters and invisible format characters
(joiners, direction marks, variation selectors) are not checked. The chain is what ships on every platform: a
desktop would find some characters in a system font at run time, Android has none, so a character only a desktop
font draws is reported on purpose.

`--charset-out DIR` (also on `regia loc gather`) writes `<DIR>/<locale>.txt`: the visible characters the locale uses,
in code point order on one line, the form font subsetting tools read (`pyftsubset --text-file=en.txt`). Regia also
subsets your fonts itself when it cooks and packages them, to the characters the shipped locales use: see
[Font subsetting](../agents/font-subset.md). At run time `localization.glyph_coverage` answers the same question for the running app
(see the [agent guide](../agents/localization.md)).

Key display for testers: `Localization::set_key_display(true)` makes every resolved text show `⟪key⟫` (with
`name=value` arguments), so you can see which element uses which key; it wins over pseudo mode.

## Shipping bundles

`regia loc compile [--locales fr,de] [--out DIR] [--strict]` validates the project's locales (an error finding stops
it and writes nothing; `--strict` also stops on warnings) and writes, under `<out>/loc/` (default `build/loc/`), one
`<locale>.ftl` per shipped locale and `index.ron` (`format_version`, `source`, a list of `(tag, file, messages, hash)`
and the fallback edges). The shipped set is the locales you list (default: `locales` of `localization.ron`, else every
locale found) plus the locales their fallback chains need, and the source locale. Compile fails when the source
locale has no `.ftl` files. Each bundle merges the locale's files in runtime order, drops comments, leaves out
`# TODO translate` placeholders (so a player sees the source text through the fallback chain rather than an
untranslated copy), keeps Fluent terms, and sorts messages and terms by key, so compiling twice gives identical bytes.

When validation stops the compile, the command lists each error and warning as `severity code file:line: message`
(`--json` adds `findings` with `file` and `line`), so you can open the file at the line. The editor's Localization
panel has a **Compile** button that runs the same step and shows each finding as a link, and a **context** column for
each key's translator note (`# note:` comments above the message, carried into PO and XLIFF exports); the toolbar's
**Preview language** picks the language the editor viewport and play show. See
[Editor Localization panel (agents)](../agents/editor-localization.md).

`regia package` does the same for the package: the bundles go to `assets/loc/` in the staged build and the authoring
`.ftl` files stay out (a localization error fails the package). At start-up a build that has `assets/loc/index.ron`
loads the bundles (`Localization::load_bundles`, each file checked against its hash) instead of reading `.ftl` files;
`localization.status` says `origin: "bundle"`.

## Audio language and localized assets

The spoken-audio language may differ from the text language: `GameSettings::audio_language` (empty follows the text
language) or `Localization::set_audio_locale`. A non-text asset (a texture with words, a recording) is localized by
path: `assets/localized/<locale>/<path>` replaces `assets/<path>` for that locale. Sounds, cues, voices and dialogues
follow the audio language; everything else the text language. `AssetServer::resolve_localized(path, group)` gives the
path to load; after a language switch, a `LocalizedAssetChanged` message names each asset whose path moved.

## Dialogue and voice-over

Spoken lines live in `assets/conversations/<id>.conv.ron` and take their words from ordinary Fluent keys, so everything above
applies to them. `regia loc gather` adds every line and sub-cue key to each locale's `gathered.ftl` with comments for the
translator and the cast (`# speaker:`, `# note:`, `# emotion:`) and a length hint (`# budget: 34 chars`, the reading speed times
the line's length). `regia loc export` and `import` carry those comments to and from the PO file.

- `regia loc export-vo --locale de --out vo-de.csv` writes the recording script for a locale (RFC 4180 CSV, one row per voiced
  line, with the file name each recording must have). Add `--bom` for spreadsheets that need a byte order mark.
- `regia loc vo-report` lists which voiced lines have a clip in each locale: `ok`, `fallback` (another locale's clip),
  `missing`, and `orphan` clips no line names. `--require` exits non-zero when a clip is missing.
- `regia loc validate-conv` checks the conversations themselves: clips, reading speed and wrapping, sub-cue counts across
  locales, unknown speakers, dead labels, unreachable lines and loops that never wait.

How captions look and how to drive them from an agent: [Conversations, captions and subtitles](../agents/conversation.md).

Single dialogue lines chosen by speaker and listener (`.dialogue.ron`, `.voice.ron`, `.subs.ron`) and their recording
sheets: [Dialogue](dialogue.md).

## Numbers, dates and lists in text

Write `NUMBER()` and `DATETIME()` in a message to show a value the way the player's language writes it:

```ftl
score = Score: { NUMBER($points) }
price = { NUMBER($cost, style: "currency", currency: "EUR") }
saved = Saved { DATETIME($when, dateStyle: "long", timeStyle: "short") }
items = { NUMBER($n) ->
    [one] { NUMBER($n) } item
   *[other] { NUMBER($n) } items
}
```

`1234.5` shows as `1,234.5` in English, `1.234,5` in German, `1 234,5` in French (with a narrow no-break space) and
`12,34,567` style grouping in Hindi. The options are the usual Fluent ones: `style` (`decimal`, `percent`, `currency`),
`currency` (ISO code), `currencyDisplay` (`symbol`, `narrowSymbol`, `code`), `useGrouping`, `minimumFractionDigits`,
`maximumFractionDigits`, significant digits, `roundingMode` (default `halfExpand`: 2.5 → 3), `signDisplay` and
`numberingSystem: "native"` for Arabic-Indic, Devanagari or Thai digits. `DATETIME()` takes `dateStyle` and `timeStyle`
(`short`, `medium`, `long`, `full`), `hour12`, `hourCycle` and `timeZone`.

Pass dates as unix seconds or ISO 8601 text (`2026-11-05T14:30:00+01:00`). Regia has no time-zone database: tell it the
player's UTC offset with `Localization::set_utc_offset_seconds`, and unix seconds show at that offset. A plain `{ $n }`
still prints the number as given.

From Rust, `regia_ui::loc_format` has the same formatting directly, plus relative times ("in 3 days",
`format_relative`) and lists ("a, b, and c", `format_list`). A wrong option shows `NUMBER()` in the text instead of
crashing; `ui.format_preview` ([agents](../agents/ui-localization.md#uiformat_preview)) shows what a value will look like.

## How far along is each language?

`regia loc status` counts, per locale, the source messages and how many are translated, untranslated (with how many
still carry a `# TODO translate` placeholder), missing nowhere in the fallback chain (including keys the gather scan
found in content and code), stale (translations recorded against an older source text, via the `# regia-source:` hash
in `imported.ftl`) and placeholder-mismatched (variables added or dropped against the source), with the percent:

```
regia loc status                     # one line per locale
regia loc status --json              # machine output for agents and CI
regia loc status --fail-under 95     # exit 1 while any locale is under 95% translated
```

Stale counts only exist where a hash was recorded (imports); hand-written translations are never stale. The agent
twin is `loc.status` ([agents](../agents/localization.md)).

## Does the translation fit its box?

`regia loc check-fit` lays out every text node of every `.ui.ron` document with every locale — the pseudo-locale
included — through the runtime layout (the same shaping and box model, headless, no GPU) and reports each text that
overflows or clips its box:

```
regia loc check-fit                                  # 1920x1080 and 1280x720
regia loc check-fit --size 1920x1080 --size 800x480  # handheld panels too
regia loc check-fit --locale de --json
```

Each finding names the document, the node's path (`root/row[2]/label`), the key, the locale, the viewport size and the
measured excess in px; a German word too long for its button, a CJK string that wraps taller than its label, an RTL
line clipped by a fixed width. `overflow: ellipsis` and `overflow: scale_to_fit` are silent — the engine adapts those.
The command exits 1 when anything overflows, so it runs in CI next to `regia loc validate`. The agent twin is
`loc.check_fit { size?, document? }`.

## Subtitle cues in WebVTT and TTML (spec 56 § 11)

A `.subs.ron` cue track goes to a subtitle vendor as WebVTT or TTML and comes back the same way:

```
regia loc cues export --cues assets/audio/ranger_intro.subs.ron --format webvtt
regia loc cues export --cues assets/audio/ranger_intro.subs.ron --format ttml --locale fr
regia loc cues import ranger_intro.vtt             # writes ranger_intro.subs.ron next to it
```

`export` resolves `text_key` cues in the chosen locale (raw-text cues go as written); `import` writes a raw-text
track (a timed-text file carries display text, not Fluent keys). Times round trip to the millisecond; a last cue
with an open end (`until: None`) exports with `end == start` and imports back open. Both readers are bounded and
refuse malformed files with coded errors (`webvtt_header`, `webvtt_timestamp`, `webvtt_cue_order`, `webvtt_limit`,
`webvtt_text`, `ttml_root`, `ttml_time`, `ttml_cue_order`, `ttml_limit`, `ttml_text`, `ttml_syntax`).

The WebVTT reader skips the header's metadata lines (`Kind: captions`, `Language: en`) and accepts CRLF, LF or a
lone CR as the line end. Cue text is plain text in the track: on import, character references (`&amp;`, `&lt;`,
`&#x2019;`, ...) resolve and the WebVTT cue tags (`<i>`, `<b>`, `<c.class>`, `<v Speaker>`, ...) are dropped; on
export, `&`, `<` and `>` are written as references, a tab or carriage return is written as a numeric reference
(`&#9;`, `&#13;`) so it reads back exactly, and an empty line inside a cue is written as one space (an empty
line would end the cue). A line containing `-->` after the cue's timing line starts the next cue (the W3C
"collect a block" step), so a separator line padded with spaces or a tab does not merge two cues into one.
The TTML reader resolves the XML character references (including `&#9;` / `&#13;` to a tab / carriage return),
and the source layout of a `<p>`
(line breaks and indentation) is not caption text: only `<br/>` breaks a line.

## Length budgets

`cargo xtask loc-length` fails when a message's pseudo-localized form is longer than a budget you wrote down
(ADR-4564). It does not cook, and it is not a `regia` subcommand.

```sh
cargo xtask loc-length --ftl assets/localization/en/main.ftl --budget loc-budgets.txt
```

`loc-budgets.txt` is one `key = max_chars` line per message you care about. Blank lines and `#` comments are
ignored. The defaults are those of `regia_ui::loc_pseudo::PseudoOptions` (not the 40 % of `regia loc pseudo`): 30%
longer, accented letters, and brackets.
`--expansion`, `--no-accents` and `--no-brackets` change those.

The length is the number of characters in the pseudo-localized message block, including the id line, continuation
lines and attributes. A length equal to the budget fits. A message with no budget line is skipped and listed
(`skipped <key>`), which does not fail the run. A budget key that is not in the file fails with
`loc_budget_missing`. When one or more keys exceed, the command exits 1 and prints `key`, the pseudo-localized
length and the budget, one per line, sorted by key. It reads only the two files.

## The Localization panel in the editor

The editor's **Localization** tab (in the bottom group) shows every key against every locale, with the source text first.
Cells are green when translated, amber when stale (the source text changed since the translation was made), purple
when only the gather tool's `# TODO translate` copy is there, and red when missing; a translation much longer than its
source has an orange outline. Pick a filter (missing, untranslated, stale, long), a locale, or type in the search box;
each locale's progress bar counts real translations.

Click a cell, type the new text below the table and press **Apply**. Only that entry of the `.ftl` file changes (its
attributes and comments stay), the file is written atomically, and Edit ▸ Undo puts the old bytes back. **Gather** does
what `regia loc gather` does, **Find usages** lists the UI files, scenes, conversations and code that name the selected
key, and **Pseudo (viewport)** shows the game's text pseudo-localized while you work. Agents use the same functions
through `editor.loc_*` ([agents](../agents/editor-localization.md)).
