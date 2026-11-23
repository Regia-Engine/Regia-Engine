# Localization (agents)

ADR-1466, `docs/specs/content-data.md` § 1. The runtime side of the localization workflow: the selected
locale and its fallback chain, a locale switch, resolving one key, pseudo mode, the validator over the loaded
locales, and the CLDR plural table. Gathering keys from content and code is a command-line step
(`regia loc gather`, [user guide](../user-guide/localization.md)); the agent methods work on what the running
app loaded from `assets/localization/<locale>/*.ftl`.

## Fallback chain

A key resolves in the first locale of the chain that has it:

1. the locale itself (`fr-CA`);
2. its extra fallbacks from `assets/localization/localization.ron` (`fallbacks: { "fr-CA": ["fr-FR"] }`), each
   followed by its own chain;
3. its parents, by dropping the last subtag (`fr-CA` → `fr`);
4. the source locale (`source`, default `en`).

Only loaded locales appear in a chain. A key missing from the whole chain shows `⟦key⟧` in dev builds and the bare
key in shipping builds.

## Start-up order

At start-up the locale comes from the first source that yields a loaded locale, in this fixed order
(ADR-1618). Each value is matched exactly first, then by its parents and script/region equivalents, ignoring
case (`pt-br` finds `pt-BR`, `zh-TW` finds `zh-Hant`, `ja_JP.UTF-8` is cleaned to `ja-JP`):

1. the `--locale <tag>` launch flag;
2. the `REGIA_LOCALE` environment variable (tests and CI);
3. the saved settings language (`GameSettings::language`), when not empty;
4. the Steam per-game language, when Steam is connected (the language the player picked in the game's Steam
   properties, mapped from Steam's names by a fixed table: `japanese` → `ja`, `schinese` → `zh-Hans`, ...);
5. the OS locale;
6. the project source locale.

A value no loaded locale matches is skipped (and listed in `startup.skipped`). A player's choice in a language
menu (the `PlayerSetLocale` message) is saved to the settings language; an agent switch is saved only with
`persist: true`. The font-fallback locale follows the selected locale, so `ja` text gets Japanese Han forms on a
desktop that has a Japanese face installed, whatever the OS language is. Each locale should define a
`locale-name` message with its own name (`locale-name = Deutsch`) for language menus.

## Plurals

Plural variants (`[one]`, `[few]`, `[many]`, ...) are chosen by Regia's CLDR table, `regia_ui::plural`, for en,
fr, de, es, it, pt, nl, sv, nb, nn, no, ru, uk, be, pl, cs, ar, he, ja, zh, ko, tr and the languages that share their rules. When a
locale loads, each plural select is routed through the `REGIA_PLURAL` function, which reads the table. Exact-number
variants (`[0]`) still win over categories. The plural data bundled with the Fluent runtime reads Arabic
`few`/`many` from the integer digits (103 would show the `other` form), and reads Belarusian `few` from the
whole integer (22 would show `other`; the table says `few`). The table uses the CLDR rules instead.
Norwegian Bokmål, Norwegian and Nynorsk use `n = 1`, so `1.0` is `one`.

## Methods

### `localization.status`

Read-only, no parameters. Returns `selected`, `source` (the last fallback), `available` (loaded locales,
sorted), `chain` (the selected locale's chain), `fallbacks` (extra edges), `pseudo` and `generation` (bumped by
each switch, fallback change or pseudo toggle; texts re-resolve when it moves). Three more fields
(ADR-1618):

- `startup`: how the start-up locale was chosen (`null` when the app loaded no localization at start-up):
  `source` is the winning source (`flag`, `environment`, `settings`, `steam`, `os` or `project`), `raw` its value
  as it came in, `selected` the loaded locale it matched, `skipped` the sources that had a value no loaded locale
  matched before the winner (`[{ source, raw }]`).
- `names`: each loaded locale's own name for a language menu (its `locale-name` message), `null` where a locale
  does not define one.
- `font_locale`: the locale the text system uses for font fallback (it follows `selected`; `null` without a text
  system).

```json
{ "method": "localization.status", "params": {} }
```

### `localization.set_locale`

Tags use the startup matching rules: normalize underscores and POSIX suffixes, ignore ASCII case,
then try the exact tag, script/region equivalents and parents against loaded locales.

`{ locale, persist? }`. Selects the locale, or its nearest loaded parent (`fr-CA` selects `fr` when only `fr` is loaded).
A switch made through this method is an agent-driven switch and is not saved. With `persist: true` it also stores
the resolved loaded tag in the saved settings language (`GameSettings::language`), as a player's choice in a language menu is
stored; the next start-up then picks it from the settings source. Returns the status plus `previous`, `changed`,
`persisted`, `saved_language` and `message_next_frame`: game code receives one `LocaleChanged
{ from, to, generation }` message on the next frame's `Update` (none when the selection did not change). Wait for
it with `play.step`.

```json
{ "method": "localization.set_locale", "params": { "locale": "fr-CA" } }
```

### `localization.resolve`

`{ key, args?, locale? }`. Resolves one key through the selected locale's chain, or through `locale`'s chain when
given. With pseudo mode on, a call that omits `locale` returns the accented, padded and bracketed text
(`[Šŧáŕŧ~~]` for `Start`). A call that passes `locale` still accents and pads (`Šŧáŕŧ~~`) and does not add the
brackets: that path formats through `resolve_in`, which runs the bundle transform and does not wrap. `pseudo` in
the reply is whether the mode is on, either way. `args` is an object of strings, numbers or booleans; a number
selects plural variants. Returns `key`, `text` (`null` when missing), `missing`, `provider` (the locale that had
the message), `pseudo` and `selected`.

```json
{ "method": "localization.resolve", "params": { "key": "menu-saves", "args": { "count": 3 } } }
```

### `localization.set_pseudo`

`{ on }`. Pseudo mode replaces each letter with an accented look-alike (`a` → `á`), pads each run of text by about
40 % with `~`, and brackets messages resolved for the selected locale: `[Šŧáŕŧ Ĝáɱé~~~~]`. `localization.resolve`
with a `locale` still accents and pads, and does not add the brackets. Placeables keep their real values. Text
that shows without accents is hard-coded; text that loses its `]` is clipped. Returns the status plus `changed`.

### `localization.validate`

`{ strict? }`. Runs the validator over the loaded locales with this app's source locale and fallback edges.
Returns `ok` (no errors, or with `strict` no warnings either), `errors`, `warnings`, one summary per locale
(`locale`, `keys`, `missing`, `untranslated`, `extra`, `chain`) and the findings (`code`, `severity`, `locale`,
`key`, `line`, `message`). Finding codes:

| Code | Severity | Meaning |
|---|---|---|
| `loc_parse_error` | error | An `.ftl` file does not parse |
| `loc_duplicate_key` | warning | A key is defined twice in one locale (the later one wins) |
| `loc_missing_key` | error | A key is missing from the locale's whole fallback chain |
| `loc_untranslated` | warning | A source key is missing from the locale itself; a fallback shows instead |
| `loc_unused_key` | warning | A source key no content or code names (only `regia loc validate`, which gathers) |
| `loc_extra_key` | warning | The locale has a key the source locale does not |
| `loc_variable_mismatch` | error / warning | The translation uses a `$variable` the source does not pass (error) or drops one it uses (warning) |
| `loc_plural_missing` | warning | A plural select lacks a category the locale uses (Russian without `[few]`) |
| `loc_plural_unreachable` | warning | A variant names a category the locale never selects (`[few]` in English) |
| `loc_key_set_differs` | info | Summary: the locale's key set differs from the source's |
| `loc_no_self_name` | warning | Some locale defines `locale-name` (its own name for language menus) and this one does not |
| `loc_fallback_cycle` | warning | The extra fallback edges lead back to a locale on their own path (`pt` → `es` → `pt`) |
| `loc_glyph_missing` | error | A message has a character no font covers (not in this method: `regia loc check --glyphs` and `localization.glyph_coverage` report it) |

### `localization.plural`

`{ locale, numbers }`. No app state needed. Returns the locale's `categories` and each number's `category`.
Pass numbers as strings to keep trailing zeros: in English `"1.0"` is `other`, `1` is `one`.

```json
{ "method": "localization.plural", "params": { "locale": "ru", "numbers": [1, 3, 5, "1.5"] } }
```

### `localization.glyph_coverage`

`{ locale?, pseudo?, charset? }`, permission `read` (ADR-2550). The tofu-box check for the running app: which messages
hold a character that no font of the app's text system covers. The chain is every face the app's `TextSystem` holds
(the bundled Inter and Noto subsets, project fonts loaded with `load_font` and, on a desktop, the system fonts), merged
by family; an app without a text system is checked against the bundled set. With `locale` only that locale (or its
nearest loaded parent) is reported; the pseudo-locale (the source text pseudo-localized in memory, tag `en-XA`) is
always included unless `pseudo` is `false`. With `charset: true` each locale row carries its visible characters in code
point order, for font subsetting.

```json
{ "method": "localization.glyph_coverage", "params": { "locale": "fr", "charset": true } }
```

Returns `source`, `fonts` (the chain's family names), `ok` (no findings), `locales` (`locale`, `characters`,
`missing_messages`, `charset?`), `findings` (`code` is `loc_glyph_missing`, `locale`, `key` (`key`, `key.attribute` or
`-term`), `line` in that locale's text, `message` naming the characters) and `truncated` (more than 500 findings).
Plural and select variants are scanned one by one, each with its own line. Errors: `loc_not_loaded`, `bad_locale`,
`loc_unknown_locale`, `bad_flag`. The command-line equivalent, which also reads project fonts from `assets/fonts/`
and writes character-set files, is `regia loc check --glyphs` ([user guide](../user-guide/localization.md)). The cook
subsets project fonts to these character sets: [Font subsetting](font-subset.md).

## Translator exchange formats

`regia loc export --format po|xliff` writes the file a translator gets and `regia loc import` reads either back
(detected from the content). Both carry the same entries and go through the same import checks; the XLIFF 2.0 form
protects Fluent placeables as inline `<ph canDelete="no">` codes, and a translation that deletes one is skipped with
`loc_xliff_placeholder_missing`. The other codes are `loc_xliff_unknown_code`, `loc_xliff_unsupported_inline`,
`loc_xliff_incomplete_segment`, `loc_xliff_bad_unit` (units), and `loc_xliff_syntax`, `loc_xliff_version`,
`loc_xliff_doctype`, `loc_xliff_entity`, `loc_xliff_limit`, `loc_xliff_bad_header` (the file). Details are in the
[user guide](../user-guide/localization.md).

## Errors

| Code | When |
|---|---|
| `loc_not_loaded` | The app has no localization (no `.ftl` files, or no UI plugin) |
| `bad_locale` | `locale` is missing or not a string |
| `loc_unknown_locale` | Neither the locale nor any parent of it is loaded |
| `bad_key` | `key` is missing or empty |
| `bad_args` | `args` is not an object of strings, numbers and booleans |
| `bad_flag` | `on`, `strict`, `persist`, `pseudo` or `charset` is not a boolean |
| `no_settings` | `persist: true` but the app has no saved settings (no framework `SettingsPlugin`) |
| `loc_unknown_plural_locale` | The plural table does not know the language |
| `bad_number` | A number is not a plain decimal (`1e3`, `NaN`) |

## Checking a translation

1. `localization.validate { strict: true }` and fix every finding with a `line`.
2. For each shipped locale, `localization.set_locale`, `play.step`, `seeing.screenshot` the menus.
3. `localization.set_pseudo { on: true }`, screenshot again: every visible string should be accented and
   bracketed, and no bracket should be cut off.

## Key display, coverage, audio locale and bundles (spec 36, ADR-1882)

| Method | Permission | Params | Result |
|---|---|---|---|
| `localization.set_key_display` | play | `{ on }` | status plus `on`, `changed` |
| `localization.coverage` | read | `{ locale? }` | `{ source, origin, locales: [{ locale, messages, translated, untranslated, placeholders, stale, percent }] }` |
| `localization.set_audio_locale` | play | `{ locale?, persist? }` | `{ text, audio, override, chain, previous, changed, persisted, message_next_frame }` |

- **Key display.** While on, every text shows its key as `⟪key⟫` (`⟪key n=3⟫` with arguments). It wins over pseudo mode;
  `localization.status` reports `key_display`.
- **Coverage.** `messages` counts the source locale's messages that have a value; `translated` the ones the locale
  has a real translation for (a `# TODO translate` placeholder is not one: it counts under `placeholders`); `stale`
  the translations whose recorded source hash (`# regia-source:` in `imported.ftl`) no longer matches the source
  text. `percent` is rounded to one decimal; a locale that is a parent of the one asked for answers for it
  (`ru-RU` gives `ru`).
- **Audio locale.** Spoken dialogue and localized sounds follow the audio locale; by default it follows the text locale.
  `set_audio_locale { locale: "ja" }` splits it (the tag needs no text loaded); without `locale` it follows the text
  again. `AudioLocaleChanged { from, to, generation }` goes out the next frame when the effective audio locale moved
  (also when the text moves while audio follows it). `persist: true` stores the tag in
  `GameSettings::audio_language` (`""` = follow). `localization.status` has `audio: { locale, override, chain,
  generation }`.
- **Origin.** `localization.status` `origin` is `files` (development: `assets/localization/<locale>/*.ftl`) or
  `bundle` (a packaged build: `loc/index.ron` and the bundles it lists, hash-checked on load). The status key `source`
  stays the source locale, so the spec's `source: files|bundle` is reported as `origin`.
- **Localized assets.** `assets/localized/<locale>/<path>` replaces `<path>` for that locale. Sounds, sound cues,
  voices, dialogues and conversations follow the audio chain, everything else the text chain. After a switch the UI
  plugin re-resolves every path handed out by `AssetServer::resolve_localized` and sends a `LocalizedAssetChanged {
  rel, group, from, to }` message per moved asset; the owner loads the new path. A variant whose cooked type differs
  from its base is never chosen.

Errors: `loc_not_loaded`, `bad_flag`, `bad_locale`, `loc_unknown_locale`, `no_settings`.

## Translation status and text fit (ADR-3359)

| Method | Permission | Params | Result |
|---|---|---|---|
| `loc.status` | read | `{ locale? }` | `{ source, locales: [{ locale, messages, translated, untranslated, placeholders, stale, percent }] }` |
| `loc.check_fit` | read | `{ size?: string, document?: string }` | `{ ok, findings: [{ document, node, key, locale, size, kind, measured, box, excess }], nodes_checked }` |

- **Status.** `loc.status` reports per loaded locale how much of the source locale's messages it translates:
  `translated` (a `# TODO translate` placeholder is not a translation; those count under `placeholders`), and
  `stale` — translations whose recorded source hash no longer matches the source text. The missing-key and
  placeholder-mismatch counts need the project's gather scan over content and code, so `regia loc status` (which
  runs it) stays the CI entry point with `--fail-under`.
- **Check fit.** `loc.check_fit` lays out every text node of the app's `.ui.ron` documents with every loaded locale
  through the runtime layout (same shaping and box model, headless, no GPU) and reports each text that overflows or
  clips its content box: `kind` is `width_overflow` or `height_overflow`, with the measured extent, the box and the
  excess in physical px. `ellipsis` and `scale_to_fit` nodes are silent (the engine adapts the text). Params: `size`
  one viewport `WxH` (default 1920x1080 and 1280x720), `document` one `.ui.ron` path relative to the working
  directory, or an absolute path under the project, the temp directory or an output root
  (default every `.ui.ron` under `assets/`). A path with `..`, or one that resolves through a symlink outside those
  roots, is `F0002` `path_outside_project` and is not read. The `assets/` walk skips symlinks. The CLI twin is `regia loc check-fit`, which also accepts
  several `--size` values and project-relative `--document` paths and exits 1 on any finding.

Errors: `loc_not_loaded`, `loc_unknown_locale` (`loc.status`), `fit_bad_size` (a `size` that is not `WxH`),
`fit_unknown_document` (the `document` cannot be read), `fit_bad_document` (it does not parse), `fit_no_documents`
(no `.ui.ron` under `assets/` and no `document` given), `fit_failed` (the layout engine refused a document; report
as an engine bug). Agent error codes are lowercase ascii with underscores; `size` of the wrong JSON type is refused
with JSON-RPC `invalid_params`. A `document` outside the allowed roots is `path_outside_project` (`F0002`).
