# Font subsetting

A font file holds far more glyphs than a game draws. When Regia cooks and packages your project, each TrueType font
under `assets/` keeps the outlines of the characters your shipped locales use and loses the rest (ADR-2697). You change
nothing in the game: it still reads `assets://fonts/ui.ttf` and passes the bytes to `TextSystem::load_font`.

Two things stay the same as in the source font: every glyph keeps its id, and `cmap`, `GSUB`, `GPOS`, `GDEF` and `hmtx`
are byte for byte identical. So text made of kept characters shapes to the same glyphs and advances, with the same
ligatures, joining and mark placement. Only `glyf`, `loca` and `gvar` shrink. The `DSIG` table is dropped, because its
signature covers bytes that changed.

## The one thing to watch

**A character outside the set still maps to its glyph, and that glyph is now empty.** It draws nothing, with its normal
advance, and no fallback font is asked. This is what you want for text you ship and wrong for text you do not know
ahead of time. If a font draws player names, chat or typed input, give it `subset: off`. If it draws a few characters
no locale file holds, such as non-ASCII literals in your code or a `.ui.ron`, list them in `extra_chars`.

A placeable's value is text the cook never sees. Digits and ASCII are kept by default, but a value with other characters,
or one that needs a composed or mirrored form no locale text reaches, also belongs in `extra_chars`.

## What is kept

- Every character of the shipped locales: all `.ftl` files under `assets/localization/<locale>/`, which include what
  `regia loc gather` wrote. Shipped locales are `locales` in `localization.ron`, or every locale directory when that
  list is empty, plus the source locale and their fallbacks. The read fails when the source locale has no files.
- Every glyph Regia's shaper picks for each distinct grapheme cluster of that text in this font, which covers what it does
  before reaching the font:
  composing accents, mirroring brackets in right-to-left text, splitting Thai and Khmer vowels.
- ASCII, including digits, for numbers formatted at run time (`keep_ascii`).
- The punctuation the engine and shapers add themselves, such as the ellipsis of an overflowing label and the dotted
  circle of a broken cluster (`keep_punctuation`).
- The accented letters the pseudo-locale draws, in `auto` mode, so a pseudo-localized build has no holes (`keep_pseudo`).
- `extra_chars`, and any glyph those reach: all `GSUB` substitutions whose inputs are kept, whether or not a feature
  names the lookup, `cmap` variation-selector glyphs, and composite components. Glyph 0 stays.

## Settings

In the font's `.meta`, the `settings` string (RON). `regia cook` creates the `.meta` the first time it sees the font.

```ron
(
    format_version: 1,
    id: "…",
    importer: "font",
    settings: "(subset: locales([\"fr\", \"de\"]), extra_chars: \"©®\")",
)
```

| Field | Default | Meaning |
|---|---|---|
| `subset` | `auto` | `auto`: the shipped locales. `off`: ship the font whole. `locales([...])`: exactly these locales. |
| `extra_chars` | empty | More characters to keep. |
| `keep_ascii` | `true` | Keep printable ASCII. |
| `keep_punctuation` | `true` | Keep the engine and shaper punctuation. |
| `keep_pseudo` | `true` | In `auto`, keep the pseudo-locale's letters. |

Settings that do not parse (`font_subset_bad_settings`) and a locale the project does not have
(`font_subset_unknown_locale`) fail the cook: a typo must not quietly change what ships.

## Fonts that ship whole

These warn in the cook and ship unchanged:

| Code | Why |
|---|---|
| `font_subset_cff_kept` | CFF or CFF2 outlines; the writer only rewrites `glyf`. |
| `font_subset_collection_kept` | A `.ttc` or `.otc` collection. |
| `font_subset_color_kept` | A `COLR` font: its color layers are glyphs nothing else refers to. |
| `font_subset_aat_kept` | A `morx` font: the shaper applies `morx` instead of `GSUB`. |
| `font_subset_cmap_kept` | A character map in a format the reader does not follow (0, 6, 10, 13 or MacRoman). |
| `font_subset_no_outlines_kept` | No `glyf` table, such as a bitmap color font. |
| `font_subset_no_locales_kept` | The project has no localization, or it does not read, so nothing says which characters it uses. |
| `font_subset_unreadable_kept` | The file does not read as a font. It ships as it is, as it always did. |

## The cook summary and the package

`regia cook` prints one line per font and a total, and writes the same numbers to `font_subsets.json` in the cooked
cache:

```
  font assets/fonts/ui.ttf: 411640 -> 96812 bytes, 314828 saved, 372 of 2937 glyphs keep their outline (en, fr)
cook: 1 font(s), 1 subset: 411640 -> 96812 bytes
```

The character set and the shaped glyphs are part of the cook key, so a translation that reaches a new character or
glyph recooks the font and one that reuses known ones does not. Fonts stay out of paks: `regia package` puts the subset
in place of the raw copy in the package's `assets/`, and prints `package: N font(s), M subset: before -> after bytes`. A
package can skip its cook when the paks are fresh, so it derives the subset again from the current font and locales,
which gives the same bytes the cook reported.

## `regia assets font-subset`

```
regia assets font-subset <font> [--project DIR] [--locales fr,de] [--chars TEXT] [--out FILE] [--json]
```

Reports what subsetting one font would do, without cooking; `--out` writes the subset, and nothing is written without
it. `--locales` replaces the font's `.meta` choice and `--chars` adds characters. A file that does not read fails with
its code: `font_subset_not_a_font`, `font_subset_truncated`, `font_subset_malformed` or `font_subset_too_complex`.

## Agent: `assets.font_subset`

`{ font? }`, permission `read`. What the last cook did, read from `font_subsets.json` in the cooked cache: nothing is
subset here, and a packaged game has no report.

```json
{ "method": "assets.font_subset", "params": { "font": "assets/fonts/ui.ttf" } }
```

Returns `format_version`, `totals` (`fonts`, `subset`, `bytes_before`, `bytes_after`) and `fonts`, each with `path`,
`mode` (`auto`, `off`, `locales`), `subset`, `code` (a kept-whole code above, or null), `glyphs_total`, `glyphs_kept`,
`bytes_before`, `bytes_after`, `locales` (where the character set came from) and `missing`: per locale, the characters
its text uses that this font does not map. A character missing from one font is often fine, since another font of the
chain may draw it; `localization.glyph_coverage` answers for the whole chain.

Errors: `font_subset_bad_param` (`font` is not a non-empty string), `font_subset_no_report` (no cook yet),
`font_subset_unknown_font` (the last cook has no such font), `font_subset_bad_report` (the report does not read or is
from a newer build), `F0016` (no asset server).

## Fuzzing

`import.font_subset` is in `fuzz.targets` in the editor: it subsets the bytes it is given and reads an accepted result
back. Damaged fonts refuse with one of the codes above and never panic.

## Limits

- Bitmap strikes (`CBDT`, `sbix`, `EBDT`) and `SVG ` documents stay as they are, so a font with both outlines and
  bitmaps only sheds its outlines.
- Hinting programs and per-glyph side tables (`hdmx`, `VDMX`, `LTSH`) stay whole.
- The bundled default fonts in the engine binary are not subset: this covers fonts in your project's `assets/`.
