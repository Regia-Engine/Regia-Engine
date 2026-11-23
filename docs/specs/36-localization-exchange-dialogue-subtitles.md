# Spec 36: Localization exchange, dialogue and subtitles

Status: built, ADR-1882 records where the build differs (ADR-1577). Plan item G5 (`BUILD-PLAN-1027.md` § 3), rung v0.3.x (b) / v1.0. Behavior written from
the public-sources spec set (ADR-1526); set areas `developer-tools` (localization) and `runtime-media` (subtitles);
the text-handling file of `runtime-core` was not read, per the ADR scrub list.
Reviewed: integ/specs (ADR-1585), 2026-10-28. Built baseline: text localization on main (ADR-1466) and the
pipeline and queue on `integ/ext2` (ADR-1570); both described below.

**Delta spec.** Regia already has a Fluent text workflow. This page adds translator exchange, shipping bundles,
locale preferences with a separate audio locale, localized non-text assets, dialogue assets and the subtitle sets
layer. It stands alone: the baseline it builds on is summarized here.

**Baseline (implemented).**

- Main, `regia_ui` (ADR-1466): `Localization` resource (`locale()`, `set_locale`, `chain`, `resolve(key, args)`,
  `resolve_in`, `has`, `set_pseudo`, `validate`), `LocaleChanged { from, to, generation }` message, `load_localization`
  (reads `assets://localization/<locale>/*.ftl` and `localization.ron` v1 `(format_version, source, locales,
  fallbacks, gather)`; selects the system locale at startup through `regia_platform::system_locale()`, which exists on
  Linux, Windows, macOS and Android), `loc_gather` (`gather_project`, text scan for `Loc("k")`, `"loc:k"` and
  `loc!("k")`, writes `gathered.ftl` with `# TODO translate` placeholders), `loc_check` (`loc_variable_mismatch`,
  `loc_plural_missing`, `loc_duplicate_key`, ...), `pseudo`, `plural` (CLDR). Files of one locale merge in name order.
  CLI `regia loc gather|validate|pseudo|plural`; agent `localization.status|set_locale|resolve|set_pseudo|validate|
  plural`. `regia_framework::settings::GameSettings` has `language` (BCP 47, empty = platform default) and
  `accessibility { subtitles: bool, text_scale, ... }`. `regia_asset::csv` (RFC 4180 `parse`, `write_record`,
  `to_string`) and the asset registry (`RegistryEntry.tags: BTreeMap<String, String>`) exist.
- `integ/ext2`, `regia_ui` (ADR-1570): `loc_gather::gather(&[GatherSource])` (key manifest), `loc_compile` (per-locale
  `CompiledTable` with `is_native(key)`, `to_ftl`, `LocalizationReport`), `loc_pseudo` (locale `en-XA`, key display
  locale `en-XD` showing `⟪key⟫`), `subtitles::{SubtitleLine, SubtitleQueue}` (priority, then earlier start, then push
  order; `max_concurrent` visible lines; `advance_to(now)` with `Shown`/`Hidden` events), agent `ui.localization`,
  `ui.subtitles`. That ADR deliberately left PO/gettext out for lack of a dependency; § 3 needs none.

## 1. Scope in one table

| # | Item | Crate | New file or format |
|---|---|---|---|
| L1 | Translator exchange (PO export/import) | `regia_cli`, `regia_ui` | PO files (external format); `imported.ftl` |
| L2 | Shipping bundles (`regia loc compile`) | `regia_cli`, `regia_asset_pipeline` | `loc/<locale>.ftl` + `loc/index.ron` v1 in paks |
| L3 | Key display toggle on `Localization` | `regia_ui` | none |
| L4 | Locale preferences, separate audio locale (detection exists) | `regia_ui`, `regia_framework::settings` | `GameSettings` fields (additive) |
| L5 | Localized asset variants | `regia_asset` | path convention |
| D1 | Voices and dialogue assets, line player | `regia_asset::dialogue` (types, parse, select), `regia_framework::dialogue` (player) | `.voice.ron` v1, `.dialogue.ron` v1 |
| D2 | Recording sheets | `regia_cli` | CSV (RFC 4180) |
| S1 | Cue sets over the subtitle queue | `regia_ui::subtitles`, `regia_asset::dialogue` | `.subs.ron` v1 |
| S2 | Settings, accessibility, agent and validators | all of the above | none |

Order of building: L3, L1, L2, L4, L5 (small, text pipeline), then D1, S1, D2, S2. Out of scope: § 12.

## 2. Terms

| Term | Meaning |
|---|---|
| Locale | A BCP 47 tag (`fr-CA`); resolution walks specific → general → source (existing fallback chain) |
| Text locale / audio locale | What written text uses / what spoken dialogue and localized audio use. Default equal; a player may split them |
| Exchange file | A gettext PO file a translator edits in a standard tool and the project re-imports |
| Bundle | The files shipped for one locale in a pak |
| Voice | The identity of a speaker or listener with grammatical hints for text selection |
| Dialogue line | One spoken line: speaker, listeners, text key, per-context sound variants |
| Cue | One displayed subtitle entry: text and times in seconds from the start of its line |
| Subtitle set | A prioritized group of cues for one sound; `SubtitleSets` expands it into queue lines and shows the highest-priority active set |

## 3. L1: PO exchange

Gettext PO is a plain, widely supported translator format; Regia's source of truth stays Fluent. Code: pure module
`regia_ui::loc_po` (no dependency, no file IO): `PoFile { header: Vec<(String, String)>, entries: Vec<PoEntry> }`,
`PoEntry { ctxt: String, msgid: String, msgstr: String, comments: Vec<String>, refs: Vec<String>, flags:
Vec<String> }`, `fn write_po(&PoFile) -> String`, `fn read_po(&str) -> Result<PoFile, PoError>`, `fn plan_import(&PoFile,
&ImportContext) -> ImportPlan` (the accepted `.ftl` text plus an `ImportReport`). `regia_cli::commands::loc` does the
file reading and writing. The entry layout (`msgctxt` = identity, `msgid` = source text, `msgstr` = translation) is
the common gettext convention for keyed messages.

```
regia loc export --locale fr [--out loc-export/fr.po] [--only untranslated|stale|all]   # default all, out under .regia/loc/
regia loc import loc-export/fr.po [--locale fr] [--dry-run] [--strict]
```

**Export** writes one PO entry per Fluent message of the source locale:

- `msgctxt` = the message key (stable identity). `msgid` = the source message's value pattern exactly as written in
  Fluent syntax (placeables like `{ $name }` and selects kept verbatim, so translators see and keep them).
  `msgstr` = the locale's current translation pattern, empty when untranslated.
- Entry comments: `#.` lines with the variables the source passes (`variables: $count, $name`), a translator note
  from a Fluent comment immediately above the source message (`# note: ...` lines carry through), and for plural
  selects the CLDR categories the target locale needs (`plural categories for fr: one, other`); `#:` source
  references from the gather scan (`file:line`). Flag `#, fluent-format` on every entry.
- Header: `Project-Id-Version` (project name and version), `Language` (the target tag), `MIME-Version`,
  `Content-Type: text/plain; charset=UTF-8`, `X-Regia-Source-Locale`, `X-Regia-Export-Version: 1`.
- `--only stale` includes entries whose stored translation was made against a different source text (§ import).
- Fluent attributes and terms are not exported: gather does not collect them (built behavior, ADR-1466/1570). Export
  skips them and says so once.
- Output is deterministic: entries sorted by key, UTF-8, `\n` newlines, lines wrapped never (no msgid wrapping), so a
  diff against a previous export shows only real changes. The writer escapes `\\`, `\"`, `\n`, `\t` and nothing else.

**Import** reads a PO file (the `read_po` above) and for each entry with a non-empty `msgstr`:

1. Reject the file if `Language` is missing or differs from `--locale`, with `loc_po_language_mismatch`.
2. Look up the key in the source locale. An entry whose key does not exist is skipped with `loc_po_unknown_key`.
3. **Stale detection:** if the entry's `msgid` differs from the current source pattern, or the entry is flagged
   `fuzzy`, it is skipped and reported (`loc_po_stale`, `loc_po_fuzzy`); with `--strict` those fail the import.
4. Parse `msgstr` as a Fluent pattern; a parse failure skips the entry (`loc_po_bad_pattern`, with the parser
   message and line within the entry).
5. Run the existing translation checks against it (`loc_variable_mismatch`, `loc_plural_missing`); errors skip the
   entry, warnings import it and are listed.
6. Write accepted entries as Fluent messages to `assets/localization/<locale>/imported.ftl` (sorted by key,
   regenerated whole, never merged by hand), and delete a matching `# TODO translate` placeholder from `gathered.ftl`
   (only entries carrying that marker; any other text is untouched). Hand-written files win over `imported.ftl`
   (the existing merge rule: later file name wins, `imported` sorts before `main`; a duplicate across files stays a
   `loc_duplicate_key` warning).

`--dry-run` prints the same report and writes nothing. The report counts imported, skipped (by code), warnings.
Import is idempotent: importing the same file twice leaves the same files byte-for-byte. Agent equivalents are
read-only (`localization.coverage`, § 10); exchange files are a CLI/editor workflow.

Acceptance: round-trip (export, fill in `msgstr`, import) reproduces the messages including a plural select and a
placeable; a changed source text makes the old entry stale and skipped; a translation dropping `$count` is an error;
CRLF input, a BOM, multi-line `msgstr` continuation lines and escapes are read correctly; output ordering is stable.


### 3.1 XLIFF 2.0 and glyph coverage (ADR-2550)

`regia loc export --format xliff` writes the same entries as the PO file as XLIFF 2.0 (one `<unit>` per message, Fluent
placeables as protected `<ph canDelete="no">` inline codes); `regia loc import` reads PO or XLIFF through the same
`plan_import` checks, so both give identical `imported.ftl` bytes. `regia loc check --glyphs` reports every message
with a character no font of the shipped chain covers (`loc_glyph_missing`, message id and `file:line`, all plural
variants, the pseudo-locale included) and can write per-locale character sets for font subsetting tools. Agent:
`localization.glyph_coverage`. Details: `book/user-guide/localization.md`. The cook subsets project fonts to the shipped
locales' character sets itself (spec 12 § 7, ADR-2697).

### 3.2 Translation memory and glossary (ADR-3359)

`assets/localization/glossary.locglossary.ron` (format `glossary` v1) lists product terms with their approved
translation per locale; `regia loc validate` warns `loc_glossary_mismatch` when a translated message uses a term
but not its approved translation (word-boundary matching). The translation memory is derived: on `regia loc export`
(PO and XLIFF), an untranslated entry whose source pattern was translated before gets a `tm-exact`/`tm-near` note;
an exact match pre-fills the translation with the `regia-review` flag (XLIFF `rg:review`, round-tripped) and stays
unconfirmed (PO `fuzzy`, XLIFF segment state `initial`), so the import skips it until it is approved (ADR-5577). Both formats
build from the same `PoFile`, so the notes reach both. Details: `book/user-guide/localization.md`,
`docs/formats/glossary-locglossary-ron.md`.

## 4. L2: Shipping bundles (`regia loc compile`)

Development reads `.ftl` files directly. A shipped build needs only the locales it ships, free of authoring noise.
The bundle writer builds on `loc_compile` (ADR-1570): per locale it keeps the entries for which
`CompiledTable::is_native(key)` holds, so inherited fallback text is not duplicated.

- `regia loc compile [--locales fr,de]` (default: `localization.ron` `locales`) runs `regia loc validate` (`--strict`
  when asked) for those locales (errors fail the compile), then writes `loc/<locale>.ftl` and `loc/index.ron` under the cook
  output: `index.ron` = `(format_version: 1, source: "en", locales: [(tag: "fr", file: "loc/fr.ftl", messages: N,
  hash: "...")], fallbacks: {...})` with a content hash (blake3 through the existing asset hash helper; no new dependency).
- Each bundle file is the locale's messages **normalized**: files merged in the runtime order, comments dropped,
  `# TODO translate` placeholder entries **omitted** (so the fallback chain shows the source text rather than an
  untranslated copy), messages sorted by key, one message per block, deterministic bytes.
- Packaging includes bundles only for the configured shipping locales plus every locale their fallback chains
  need (the source locale always). `Localization::load_bundles(&index, read)` loads from a pak (`read` is a closure that returns a file's bytes); `localization.status`
  reports `source: "files" | "bundle"`.
- `localization.ron` (v1) already carries `locales`; no format change. The bundle index is a derived artifact, so
  there is no frozen-format migrator: a version it does not read is rebuilt by the next compile.

Acceptance: compile twice → identical bytes; a placeholder-only translation resolves to the source via fallback in
a bundled run; an unlisted locale is absent from the pak; compile fails on an error finding and passes on warnings
unless `--strict`.

## 5. L3 and L4: key display, locale detection, audio locale

**Key display (L3).** `Localization::set_key_display(bool)` mirrors `set_pseudo`: while on, `resolve` returns the
`⟪key⟫` form (`⟪key n=3⟫` when arguments were passed; same function as `loc_pseudo::key_display`, ADR-1570) for every
key, so a tester sees which element uses which key. If both are on, key display wins. A missing key already resolves
to a visible marker (built); keep it. Agent: `localization.set_key_display { on }`; `localization.status` reports it.

**Preferences (L4).** `GameSettings.language` (built; empty = follow the platform) is the text locale. Add
`GameSettings.audio_language: String` with `#[serde(default)]` (empty = follow the text locale), following the
pattern of earlier additive settings fields ("missing in older files"): old files load unchanged, so no version bump
or migrator is needed; the lead still lists the field under Upgrade for struct-literal users.
`Localization::set_audio_locale(tag)` and `Localization::audio_chain()` give the fallback chain for audio. A text
change keeps sending `LocaleChanged` unchanged. An audio change sends a new message `AudioLocaleChanged { from, to,
generation }` (a new type, so the existing struct and every reader stay as they are).

**System detection (implemented).** `regia_platform::system_locale() -> Option<String>` exists on all platforms
(Linux: `LC_ALL`, `LC_MESSAGES`, `LANG`; Windows: user default locale name; Android: the JVM default locale;
macOS: preferred language) and `load_localization` already picks the exact tag, then its language part. Gaps this
spec closes: (a) start-up order becomes explicit `--locale` flag, then `GameSettings.language`, then
`system_locale()`, then the project `source`, through one function `choose_startup_locale(flag, setting,
system, available)` that tests call with injected values; (b) the pure converter behind the Linux reading
(`fr_CA.UTF-8` to `fr-CA`; `C`, `POSIX` and empty give none) is exposed as `regia_platform::locale_from_env(get)`
for tests; (c) the audio locale uses the same order with `audio_language` in place of `language`. No new
environment variable is introduced; tests never read the real environment.

Acceptance: key display on/off; `fr_CA.UTF-8` → `fr-CA`, `C` → none; `choose_startup_locale` with each source present
or absent (injected); audio follows text until set, then splits; clearing the audio override re-follows; both preferences
survive a settings save/load; an old settings file without `audio_language` loads.

## 6. L5: Localized asset variants

Non-text content (a texture with baked words, a voice recording) is localized by path: for a project asset
`assets/<rel>`, a locale `L` may provide `assets/localized/L/<rel>`.

- `regia_asset::localized_candidates(rel, chain) -> Vec<String>` (new, pure): for each locale of the chain in order,
  the localized path; then the base path last. `AssetServer::resolve_localized(rel, group) -> path` (new): the first
  candidate present in the manifest/registry wins. `LocaleGroup::{Text, Audio}`: `Text` uses the text chain; `Audio`
  the audio chain (`regia_asset` holds the chains as plain `Vec<String>` set by the UI/audio plugins; it does not
  depend on `regia_ui`).
  Which group a type belongs to: sounds, voices, dialogue → `Audio`; everything else → `Text`.
- The cook treats `assets/localized/**` like other assets (each is a normal manifest entry) and the registry adds the tags
  `localized=<locale>` and `localized_of=<rel>` to its `tags` map; the content registry queries and spec 32's `localized-twin`
  validator use those tags.
- A locale switch while running re-resolves localized assets on the next load; already loaded assets are replaced
  when `reload_localized()` is called (the UI and audio plugins call it on `LocaleChanged`); textures swap through
  the existing asset hot-reload path, no restart needed.
- Variants must have the same cooked type as their base; a mismatch is a validator error, and the loader ignores a
  mismatched variant (it never loads a failing variant at runtime; it falls back to the base).

Acceptance: chain order; fallback to base when the locale has no variant; a type-mismatched variant ignored; switching
locale swaps a texture without restart (headless: the resolved path changes and a reload event is raised).

## 7. D1: Voices and dialogue assets

### 7.1 `.voice.ron` (format name `voice`, v1)

`(format_version: 1, id: "ranger", name_key: "voice-ranger-name", gender: Feminine, number: Singular)`.
`gender`: `Neutral | Feminine | Masculine | Common | Other`; `number`: `Singular | Plural`. Ids are unique, lowercase
`[a-z0-9_]`, 1..=48 chars. The grammatical hints exist so translators can select forms:
the dialogue player passes `$speaker_gender`, `$speaker_number`, `$listener_gender`, `$listener_number` (strings:
`neutral|feminine|masculine|common|other`, `singular|plural`) and `$speaker`, `$listener` (display names) as Fluent
arguments, so a French line can inflect with `{ $speaker_gender -> [feminine] ... *[other] ... }`.

### 7.2 `.dialogue.ron` (format name `dialogue`, v1)

```ron
(
    format_version: 1,
    name: "ranger_intro",
    lines: [
        (id: "greeting", speaker: "ranger", listeners: ["player"],
         note: "Said when the player first arrives at the gate.",
         text_key: Some("dlg-ranger_intro-greeting"), display_key: None, mature: false,
         variants: [
            (speaker: None, listener: None, sound: "audio/dialogue/ranger_greeting.ogg", cues: None),
            (speaker: Some("ranger"), listener: Some("child"), sound: "audio/dialogue/ranger_greeting_child.ogg", cues: None),
         ]),
    ],
)
```

- `text_key`: the Fluent key of the spoken text (the transcript). Default when omitted: `dlg-<name>-<line id>`
  (declared in the loader, validated unique). `display_key`: when set, the subtitle shows this key's text instead
  of the transcript (a shortened or altered display text); the transcript still goes to translators and recording
  sheets. Both keys must reach `regia loc gather`. Built gather scans text for `Loc("k")`, `"loc:k"` and `loc!("k")`; the
  dialogue keys are plain strings, so the CLI loads each `.dialogue.ron`, collects its keys and passes them to the
  gather planner as extra uses (a new function `loc_gather::plan_with_extra(.., extra: &[(String, KeyUse)])`, additive).
- `note` is a translator/cast note carried into the exchange file as a PO comment. `mature` is a boolean content
  marker surfaced to settings (a game can hide marked lines or voices); Regia attaches no policy to it.
- `variants`: context-specific sounds. `sound` is a base asset path resolved through L5 with the **audio** chain.
- **Selection** `DialogueLine::select(speaker: &str, listeners: &[&str]) -> Option<&Variant>`: the best match by
  score (speaker matches 2, listener matches 1, a variant with `None` for a field matches that field with 0 and
  never loses on it), highest score wins, ties by file order; a variant that names a speaker/listener that does not
  match is not a candidate. No candidate → `None`; the player then plays nothing and reports `dialogue_no_variant`.
- Sound durations come from the cooked sound; cues (§ 8) may be given inline per variant or in the sound's `.subs.ron`
  sidecar (the inline value wins).

### 7.3 Playing a line

`regia_framework::dialogue::play_line(world, DialogueRequest { dialogue, line, speaker, listeners, priority, position:
Option<Vec3>, bus }) -> DialogueHandle`: selects the variant, resolves the text through `Localization` with the
voice arguments, starts the audio (spatial when `position` is set), and queues the subtitle set (§ 8) tied to the
voice instance. Stopping the voice, finishing it, or unloading the world stops the subtitle set; `DialogueEvent::{
Started, Finished, Interrupted }` are messages (`Finished` only for uncut completion). The handle exposes
`stop(blend)`. The line plays on the Voice bus; music ducking comes from the existing rules in
`regia_audio::ducking`, with no new audio rules here. The player lives in `regia_framework` because it needs audio,
localization and subtitles together (`regia_ui` does not depend on `regia_audio`).

Acceptance: variant selection table (all combinations of speaker/listener presence, tie by order); grammatical
arguments reach the Fluent message (an English `{ $speaker_gender -> [feminine] her *[other] their }` test);
`display_key` overrides on screen only; unknown voice/line errors; `Finished` vs `Interrupted`; a locale switch mid
line does not change the running subtitle text (new lines use the new locale).

## 8. S1: Subtitles

### 8.1 Cue data (`<sound>.subs.ron`, format name `subtitles`, v1, and inline in dialogue variants)

`(format_version: 1, priority: 1, keep_breaks: false, one_at_a_time: false, cues: [(at: 0.0, until: Some(2.2),
text_key: Some("dlg-ranger_intro-greeting")), (at: 2.4, until: None, text: Some("Raw text (not localizable)"))])`
(optional fields are written `Some(..)`, as everywhere in RON; ADR-5778)

- `at` seconds from the start of the sound, `until` the end (None = until the next cue starts, or the sound ends
  for the last cue). `at` values are non-decreasing and ≥ 0; `until >= at`; `text_key` or `text` (exactly one).
- `priority: i32` ≥ 0 (matches `SubtitleLine.priority`; 0 means the set is never shown). `keep_breaks`: author-placed
  line breaks (`\n` in the text) are kept and no automatic breaks are added. `one_at_a_time`: the entries of a long
  cue are presented in sequence instead of stacked (long entries are split to fit, shown in sequence over the cue's
  time).
- Cues are looked up for a sound by the sidecar name (`foo.ogg` → `foo.subs.ron`), localized through L5 (the
  `Text` chain, since cue text is text).

### 8.2 Cue sets over the queue (`regia_ui::subtitles`)

The queue (`SubtitleQueue`, `SubtitleLine { speaker, text, key, args, start, duration, priority }`, ADR-1570,
implemented on `integ/ext2`) decides what is visible from timed lines. This spec adds the layer above it that
understands a sound's cue list: a `SubtitleSets` resource that owns a `SubtitleQueue` configured with
`max_concurrent = 1` (one set's text at a time).

```rust
pub struct SubtitleSets { /* resource: queue + sets + layout cache */ }
pub struct SetRequest {
    pub cues: Vec<Cue>,               // text already resolved (or key plus args)
    pub priority: i32,                // > 0 to show; larger wins
    pub duration: f32,                // seconds the set lives (the sound's length)
    pub start_delay: f32,             // seconds before the first cue counts
    pub keep_breaks: bool, pub one_at_a_time: bool,
    pub kind: SubtitleKind,           // Dialogue | Caption
    pub speaker: Option<String>,      // display name shown before the text when enabled
    pub owner: Option<Entity>,        // for stop-by-owner and later movie subtitles
}
impl SubtitleSets {
    pub fn queue(&mut self, req: SetRequest) -> SetId;
    pub fn stop(&mut self, id: SetId) -> bool;
    pub fn stop_all(&mut self);
    pub fn stop_owner(&mut self, owner: Entity) -> usize;
    pub fn is_active(&self) -> bool;
    pub fn advance(&mut self, dt: f32);                    // fixed-step clock, drives the queue
    pub fn sync(&mut self, id: SetId, playhead: f32);      // set the set's time to the voice playhead
    pub fn current(&self) -> Option<CurrentSubtitle>;      // the chosen set's visible text, as laid-out lines
    pub fn set_enabled(&mut self, on: bool); pub fn enabled(&self) -> bool;
}
```

Behavior:

- **Expansion.** `queue` turns each cue into one `SubtitleLine` whose `start` is the set's base time plus
  `start_delay + at` and whose `duration` runs to `until` (or the next cue's start, or the set's end for the last
  cue); every line carries the set's `priority`. `sync(id, playhead)` re-bases the set (cancel its lines, push them
  again from the new base; lines that ended before `now` are dropped and a line already running starts at `now`
  with its remaining duration, since the queue requires `start ≥ 0` and time never goes backwards), so the queue
  itself needs no seek.
- A set lives from `queue` until `elapsed ≥ start_delay + duration`, `stop`, or its owner being stopped. Finished
  sets are discarded the frame they finish.
- Selection is the queue's rule: highest `priority` first, ties to the earlier start, then to the line pushed first;
  priority 0 lines are never queued; a preempted line returns while still active. `Caption` sets (sound-effect
  text) are queued only when the player's mode is `All` (§ 9).
- The visible cue at set time `t` (after `start_delay`) is the line active at `t`: `at` inclusive, `until`
  exclusive; a gap between cues shows nothing.
- **Layout (`current()` returns text lines; the UI draws them):** the subtitle area is the bottom-center of the
  safe region: width 90 % of the viewport width (never wider than 1280 logical px), at most `max_lines` (default 3)
  lines, with a bottom margin of 7 % of the height. Automatic wrapping breaks at word boundaries using the UI text
  measurer at the current text scale (CJK text breaks between characters); `keep_breaks` keeps `\n`. When text needs
  more than `max_lines`, it is split into **pages** of at most `max_lines`, shown consecutively with the cue's time
  divided in proportion to each page's character count (minimum 1 second per page unless the cue is shorter). The
  layout is cached per (text, width, scale, locale) and recomputed when the viewport width changes between frames;
  a width change mid-set reflows without restarting the set (the page index is kept by character offset). These
  defaults are design choices, not copies of any standard; broadcaster guidance (about two lines, 32 to 42
  characters per line) informs them.
- Right-to-left locales: lines are right-aligned and order follows the text layout engine's bidi result; the area
  is mirrored only for alignment, not position.
- `speaker` is drawn as `Name: ` before the first line when the setting is on.
- Neither the queue nor the sets read a clock: `advance`, `sync` and the injected measurer are the only inputs. A
  system `tick_subtitles` (FixedUpdate) calls `advance(dt)`; the dialogue player calls `sync` each frame (the system
  that ticks the queue from game time was not built by ADR-1570; this spec asks for it).

### 8.3 Drawing

The `regia_ui` runtime draws the current subtitle in a screen-space overlay layer above the HUD and below modal
menus, using theme tokens (a `subtitle` style: font, size, text color, outline or plate color, plate opacity). It
honors the UI text scale and the high-contrast setting from the accessibility work (`book/agents/ui-a11y.md`), and
is announced to the accessibility tree as a live region with polite priority (`regia_ui::a11y`). No draw call when nothing is shown.

As built (ADR-2137): `sync` is `regia_ui::captions::sync_set(world, id, playhead)` and the agent method
`subtitle_sets.sync`; the set's caption is sent again from the playhead under the same id and the board replaces the
entry in place (a visible set keeps its row). A playhead within 40 ms (`SYNC_TOLERANCE_US`) of the set's own time
changes nothing, so a per-frame caller re-lays nothing. `one_at_a_time` pages a long cue one line per page
(`CaptionStyleHint::one_at_a_time`).

As built (ADR-7208): the theme `subtitle` style is three token slots read by `captions::sync_caption_rows`:
`subtitle.text` (text color while the player keeps the default white; a color the player picks wins),
`subtitle.plate` (the box plate color; its alpha multiplies the player's box opacity) and `subtitle.plate-opacity` (the
lowest box opacity). `regia-dark`, and so every built-in theme, sets white text on a black plate (the previous look,
bit for bit); the high-contrast themes (`regia-high-contrast`, `regia-light-high-contrast`) set the plate opacity to
1, so turning high contrast on swaps the captions to an opaque plate. Test:
`crates/regia_ui/tests/theme_slots.rs` `the_caption_theme_swaps_under_high_contrast`.

As built (ADR-9340): the font, size and outline of the `subtitle` style are four more token slots. `subtitle.font`
names the caption font family (rows are measured and drawn in it; unset = the UI font); `subtitle.size` multiplies the
caption size on top of the player's `text_scale` (the board lays rows out at `safe height / 24 * text_scale * size`,
clamped to 0.25..=4); the `Outline` background (also `Box` at opacity 0) draws a `subtitle.outline` stroke
`subtitle.outline-width` UI px wide around the glyphs (one ADR-2045 text effect layer) instead of a plate.
`regia-dark` sets size 1, a black outline and width 2. `ui.theme_get` reports the style as `caption`. Tests:
`theme_slots.rs` `a_caption_size_token_changes_the_glyph_size`, `an_outline_caption_adds_outline_quads_to_the_draw_list`.

### 8.4 Acceptance

1. Priority: the larger value shows; 0 hides; ties earliest; an interrupted lower set resumes at its own time.
2. Cue timing: boundaries (`at` inclusive, `until` exclusive), gaps, `until: None`, `start_delay`, `sync` jumping
   forward and backward (the re-based lines give the same visible text as an uninterrupted run).
3. Wrapping: width wrap, `keep_breaks` keeps `\n`, `one_at_a_time` sequencing, pages when more than `max_lines`
   with proportional timing and the 1 s minimum, CJK break between characters, a width change reflows keeping the
   page by character offset.
4. `stop`, `stop_all`, `stop_owner`, `is_active`, `set_enabled(false)` (nothing shown, sets still advance and finish).
5. `Caption` hidden unless the setting is `All`; speaker label setting.
6. Determinism: a recorded sequence of (queue, advance, sync) calls reproduces the same visible text per frame.

## 9. S2: Settings and accessibility

Built: `GameSettings.accessibility { subtitles: bool, text_scale, reduced_motion, high_contrast, ... }`. The new
fields are additive with `#[serde(default)]` (the repo's pattern for older files), so no version bump or migrator is
needed:

- `accessibility.subtitle_mode: Option<SubtitleMode>` (`Off | Dialogue | All`); `None` (default, and in older files)
  derives from `subtitles`: `true` is `Dialogue`, `false` is `Off`. When `Some`, it wins and `subtitles` is kept in
  sync (`Off` writes `false`).
- `GameSettings.subtitle_style: SubtitleStyle { plate_opacity: f32 (0..1, default 0.6), speaker_names: bool
  (default true), max_lines: u8 (1..=4, default 3) }`.
- Text scale reuses `accessibility.text_scale` (the UI clamps it to 0.5..3.0); no second scale exists.

A first run follows the system captions preference where the platform reports one (Android, Windows); otherwise
`Dialogue`. The settings screen template gets a subtitle panel (follow-up in the UI lane).

## 10. Agent surface and CLI

Page `book/agents/dialogue-subtitles.md` (topic `dialogue-subtitles`, plus additions to `book/agents/localization.md`
for `localization.coverage`, `set_key_display`, `set_audio_locale`). The built `ui.localization` and `ui.subtitles`
(`book/agents/ui-localization.md`, queue level) stay; the `subtitle_sets.*` methods below sit one level above them; `book/COVERAGE.md` rows. User guide additions to
`book/user-guide/localization.md` (exchange and bundle workflow) and a new `book/user-guide/dialogue.md`.

| Method | Permission | Params | Result |
|---|---|---|---|
| `localization.coverage` | read | `{ locale? }` | per locale: messages, translated, untranslated (placeholder), stale-vs-source count, percent |
| `localization.set_key_display` | play | `{ on }` | `{ on }` |
| `localization.set_audio_locale` | play | `{ locale? }` | `{ text, audio, chain }` (`locale` absent clears) |
| `dialogue.list` | read | `{ dialogue? }` | dialogues, lines (id, speaker, keys, variant count), voices |
| `dialogue.resolve` | read | `{ dialogue, line, speaker?, listeners? }` | `{ variant, sound, text, display_text, args }` |
| `dialogue.play` | play | `{ dialogue, line, speaker?, listeners?, position? }` | `{ handle }` |
| `dialogue.stop` | play | `{ handle? }` | `{ stopped }` (absent = all) |
| `subtitle_sets.status` | read | `{}` | `{ enabled, mode, active: [{id, priority, owner, elapsed}], current: { lines, page, speaker } | null }` |
| `subtitle_sets.queue` | play | `{ cues, priority, duration, ... }` | `{ id }` |
| `subtitle_sets.stop` | play | `{ id? }` | `{ stopped }` (absent = all) |
| `subtitle_sets.set_enabled` | play | `{ on }` | `{ enabled }` |

Errors (lowercase codes in the book page): `loc_not_loaded` (existing), `bad_locale` (existing),
`dialogue_unknown_asset`, `dialogue_unknown_line`, `dialogue_unknown_voice`, `dialogue_no_variant`,
`dialogue_bad_handle`, `subtitle_sets_bad_cues`, `subtitle_sets_unknown_id`, `subtitle_sets_unavailable`, `dialogue_unavailable`.
`regia loc` gains `export`, `import`, `compile`, `dialogue-sheet export|import` (below). `regia loc validate` gains the
dialogue checks.

**Recording sheets (D2).** `regia loc dialogue-sheet export --locale fr --csv sheet.csv` writes one row per line with
columns `line,speaker,listeners,spoken_text,display_text,recording,note` (RFC 4180 via `regia_asset::csv`); the
`recording` is the expected localized path (`assets/localized/fr/...`) for the first (default) variant, empty when
unrecorded. `import --csv sheet.csv` reads edited `spoken_text`/`display_text` back into `imported.ftl` through the
same checks as L1 (stale detection compares to the source) and reports `recording` entries that do not exist.
Native lines are sheet rows for the source locale too.

## 11. Validators (contributed to spec 32's framework)

`dialogue_assets` (registry name; spec 32 validators are lowercase `[a-z0-9_]`): `dialogue_missing_sound` (a variant's sound is
absent for a shipping audio locale and its base is absent), `dialogue_unknown_voice`, `dialogue_missing_text` (a key absent from
the source locale), `dialogue_duplicate_line`, `dialogue_duplicate_id` (a second file repeats a dialogue name or a voice id; the first
file by path is kept, the other skipped), `dialogue_parse_error` (a file that does not parse), `subtitle_past_end` (a cue
starts after the sound's known duration; warning). Built in ADR-2755: `regia_asset_pipeline::dialogue_validator` runs
`regia_framework::dialogue::validate_dialogues` once and reports its issues with these codes, so `regia assets validate`, the
editor and the save trigger report them. (The codes were first spelled `validate_dialogue_*` here; the code and the book use the
names above.) `loc_*` codes from the content-data spec stay as they are.

## 12. Determinism, performance, platforms, out of scope

- All selection, timing and layout results are pure functions of their inputs plus the injected clock and measurer;
  no wall clock, no environment access in tests.
- Budgets: `SubtitleSets` update with 16 active sets ≤ 20 µs; layout of a 200-character cue ≤ 100 µs on a cache
  miss and ≈ 0 on a hit; `regia loc export` of 20,000 messages ≤ 1 s, `import` ≤ 1.5 s; compile of 10 locales × 20,000
  messages ≤ 3 s. `tracing` spans: `subtitles_update`, `subtitle_layout`.
- Linux, Windows, Android aarch64: pure Rust; locale detection per platform in `regia_platform` behind the seam;
  tools are desktop-only.
- **Out of scope:** machine translation or crowd-platform variants of PO (only the plain format; XLIFF 2.0 is a second wire format since ADR-2550), number/date/
  currency formatting by locale beyond Fluent's built-ins, text-to-speech, lip sync, subtitle tracks inside video
  files (belongs to video playback, spec 50; `SubtitleRequest.owner` is the hook), voice recording tools, content
  rating policy, character-level bidirectional editing, per-line audio ducking rules, replication of dialogue.

## 13. Format and migration summary

New formats: `voice` v1, `dialogue` v1, `subtitles` v1 (all `format_version: 1`, unknown fields rejected). Derived
artifacts without migrators: bundle index (`loc/index.ron`, rebuilt by the next compile). External: PO, CSV.
Additive changes to an existing frozen format: `GameSettings` gains `audio_language`, `subtitle_style` and
`accessibility.subtitle_mode`, all `#[serde(default)]` (older files load unchanged; no migrator needed, per the
pattern earlier settings fields used). Host-visible API: new message `AudioLocaleChanged`; new struct fields on
`GameSettings` and `AccessibilitySettings` (break struct literals; list under Upgrade); `localization.ron` v1
unchanged. The lead registers the three new formats in `check-formats` when they freeze and runs `cargo xtask
check-formats --update` and `cargo xtask manual`.

## 14. Provenance

Public pages on localization workflows and the gettext PO file format (GNU gettext manual, public); BCP 47 and RFC
4647 lookup (already cited in ADR-1466); Unicode CLDR plural rules (already cited); public broadcaster subtitle
guidelines (line length, line count, reading speed) inform defaults only; public user documentation of dialogue
assets, voices and the subtitle priority/queue behavior (behavior only). Set areas: `developer-tools-localization`,
`runtime-media-subtitles`. Row in `docs/provenance.md` (ADR-1577); this section is the spec's own record. Format and
field names are Regia's.

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

## 15. Decisions and owner questions

- Decision (ADR-1585): `msgid` carries the Fluent pattern verbatim (placeables and selects visible); expanding
  selects into PO plural forms would lose variable and select fidelity.
- Decision (ADR-1585): the subtitle layer is `SubtitleSets` over the built `SubtitleQueue` (`max_concurrent = 1`);
  the queue is not replaced.
- Decision (ADR-1585): a second message `AudioLocaleChanged` instead of a `scope` field on `LocaleChanged`.
- Decision (ADR-1585): settings additions are additive serde-default fields on `GameSettings`.
- Decision (ADR-1585): page timing (proportional, 1 s minimum) and a 90 % / 1280 px area are Regia defaults.
- Decision (ADR-1585): no new dependency (own PO reader and writer, CSV from `regia_asset::csv`); no `unsafe`.
- Legacy per-language file layouts are deliberately not carried over.
- Owner decisions: none.

## 16. Builder notes

Size L; build in the order of § 1. Spec 32 receives the validators in § 11 (spec 32's `localized-twin` uses the L5
tags). The subtitle sets depend on the `SubtitleQueue` from ADR-1570, which is on `integ/ext2` and not yet on main;
build after that merge, or merge it first. Note for the lead: `regia_ui::loc_gather` exists on main with a different
API than the module of the same name on `integ/ext2` (ADR-1570); that conflict is outside this spec. Spec 56 also
covers dialogue and subtitles: reconcile with it before building D1/S1 (this spec's formats win unless the lead
decides otherwise). Book pages: `book/agents/dialogue-subtitles.md`, `book/user-guide/dialogue.md`; COVERAGE rows.

## 17. Culture formatting (UI L04, ADR-2698)

`regia_ui::loc_format` formats values for localized text; its data table resolves each field along `locale_chain`
(`pt-BR` → `pt` → `root`, explicit CLDR parents such as `zh-TW` → `zh-Hant` → `root`).

- Numbers: `format_number(value, locale, &NumberOptions)`; options are the ECMA-402/Fluent names (`style`
  decimal|percent|currency, `currency`, `currencyDisplay` symbol|narrowSymbol|code, `useGrouping` auto|always|min2|false,
  `minimumIntegerDigits`, `minimum`/`maximumFractionDigits` (0..=100), `minimum`/`maximumSignificantDigits` (1..=21),
  `roundingMode` (nine ECMA-402 modes, default `halfExpand`, applied to the shortest round-trip decimal of the `f64`),
  `signDisplay`, `numberingSystem` latn|native). NaN and ±∞ use the locale's symbols; any magnitude prints in full.
- Dates: `CivilDateTime` (wall time plus optional UTC offset; `from_unix`, `parse_iso`, `to_iso`), `format_date`,
  `format_time`, `format_datetime` with `dateStyle`/`timeStyle` short|medium|long|full, `hour12`, `hourCycle`, `timeZone`
  (fixed offsets only: there is no time-zone database), Gregorian only.
- `format_relative(value, unit, locale, numeric always|auto, &NumberOptions)`; nouns by `crate::plural` categories.
- `format_list(items, locale, And|Or)`.
- Fluent: `NUMBER()` and `DATETIME()` on every bundle. `DATETIME($d)` takes unix seconds (shown at `timeZone`, else
  `Localization::utc_offset_seconds`) or ISO 8601 text. Errors render Fluent's error value (`NUMBER()`).
- Pseudo-localization leaves formatted values alone; native digits are opt-in.
- Agent: `ui.format_preview` (errors `format_bad_option`, `format_bad_value`, `format_bad_pattern`).

## 13. Thai, Lao, Khmer and Myanmar line breaking (ADR-3323)

Thai, Lao, Khmer and Myanmar write words without spaces: UAX #14 gives their letters line-break
class SA and defines no break opportunities without dictionary segmentation. The engine segments
every SA run with ICU's line segmenter (`icu_segmenter`, compiled dictionaries for all four
scripts plus LSTM where available; Unicode License v3, ~420 KiB per release binary) and wraps at
the word boundaries it finds. The break opportunities reach the shaper as invisible zero-width
markers; `ShapedText` line ranges, `CaretMap` carets, selection and hit testing index the
original string through per-layout offset maps, and texts without SA characters shape
byte-identically to before.

- UI text (`specs/18` § 2), captions/subtitles (§ 8 above, through `captions::wrap`) and
  word-wise caret movement in text inputs (Ctrl+arrows, double-click) all use the one shared
  segmentation (`regia_ui::line_break_sa`), cached with the shaped text (a 32-entry LRU keyed by
  the string; a real segmentation logs a `ui_sa_segment` span and never runs per frame).
- A word wider than the block still splits between grapheme clusters, as for any over-long word.
- Agent: `ui.text_breaks {text, locale?}` reports the word boundaries (character offsets and the
  words) that the UI, captions and caret movement use; `locale` is informational (segmentation
  follows the text's own script). Book page `agents/ui-localization.md`.
- Fonts: the bundled set covers Thai (Noto Sans Thai). Lao, Khmer and Myanmar wrap correctly but
  draw tofu without a project font: the subsets those locales need are Noto Sans Lao, Noto Sans
  Khmer and Noto Sans Myanmar (OFL-1.1); `ui.text_shape`'s `missing_glyphs` reports the gap.

## 18. As built (ADR-5778)

Conformance audit 2026-11-10. ADR-1882 is the main record of where the build differs; ADR-1618 (start-up locale
order), ADR-1668, ADR-1697, ADR-2137, ADR-4954 and ADR-5778 add to it.

- Turning subtitles off (`CaptionOptions::enabled`, `subtitle_sets.set_enabled`) takes speech rows off the screen,
  including rows already shown, and § 8.4 (4) holds: the sets still advance and finish, and rows turned back on
  before their end show again (ADR-5993 reverts ADR-5778's "their sets end"). A row pushed while off is dropped
  (spec 56 § 2.8). `caption_mode` off hides sound captions the same way.
- PO export counts the terms it leaves out (`ExportOutput::skipped_terms`); `loc_po_bad_pattern` names the line
  inside the translation; dialogue keys get `#:` references on export.
- `plan_with_extra` is `Gathered::add_key` through `loc_sheet::add_dialogue_keys`; `DialogueRequest` has no `bus` and
  a `u8` priority; lines stop through `stop_line`; audio follows `audio_language`, else the text locale; with
  `keep_breaks` an over-wide line still wraps.
- Open: theme
  `subtitle` tokens and high contrast in captions (§ 8.3); the 200-character layout miss budget (ADR-1882 (o)).

## 19. As planned (ADR-7752, UI parity)

Planned for v0.6.0; nothing here is built at the time of writing. Built by job 3246 (`w12-loc-tables-finish`); every
item ships with an agent method, a `book/agents` page and a `book/COVERAGE.md` row.

- **Compile step.** A panel action and `regia loc compile` produce the runtime string tables from the project's
  localization files, through the compile code `regia_ui` already has (`loc_compile.rs`), which no caller uses yet.
  `editor.loc_compile` (planned).
- **Translator comments.** A comment per key, shown as a context column in the Localization panel and exported to PO
  (as extracted comments) and XLIFF (as notes); import keeps them. `editor.loc_set` takes `comment`.
- **Preview language.** A picker for the viewport and play that previews a project locale without changing the user's
  setting; the preview is view state and is never saved. `editor.loc_preview` (planned).
- **Table exchange.** CSV and JSON import and export for data tables and curve tables (spec 51, `content-data.md`), planned first and
  applied as one undo step, with the same refusal rules as the PO and XLIFF exchange.
  `editor.tables_panel_export`, `editor.tables_panel_import` (planned).
- The editor's own UI language stays English in v0.6.0: translating editor chrome is deferred and needs its own spec
  (ADR-7752).
