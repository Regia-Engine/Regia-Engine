# UI text shaping (agents)

UI series L13, ADR-865. `regia_ui::text` shapes every `Text` with cosmic-text (advanced shaping, bidi, per-script
font fallback). This page covers the edge cases it handles, what the shaped result reports, and `ui.text_shape`.

## What the shaper guarantees

- **Ellipsis.** `TextOverflow::Ellipsis` on unwrapped text (`WrapMode::None`) wider than its box ends the line with
  "…" on the line's visual end: the right for left-to-right lines, the left for right-to-left lines. Truncation
  works on whole clusters in logical order, so a ligature, a ZWJ emoji or a letter with its marks is never split.
  Mixed-direction lines stay inside the box. Wrapped text keeps all its lines (the ellipsis does not apply to it).
- **Emoji presentation.** A character followed by U+FE0F (VS16), with its skin tone, keycap and ZWJ continuations,
  uses the color emoji face. Without the selector, or with U+FE0E, the default face keeps characters it covers
  (Inter has a text heart, warning sign, arrows and sun).
- **Mixed scripts in one span.** Arabic letters, and the neutrals between them, use the joining Naskh face. Latin
  words in the same span keep the default face. Before, any Arabic letter moved the whole span to the Arabic face.
- **Line breaks.** CR LF, a lone CR, NEL (U+0085), LINE SEPARATOR (U+2028) and PARAGRAPH SEPARATOR (U+2029) each
  break the line once. A CR at the end of one span plus an LF at the start of the next is one break.
- **Invisible controls.** Zero-width space and joiners, bidi marks, embeddings and isolates, the soft hyphen, word
  joiner, variation selectors, the BOM and tag characters never draw a box, even when no face maps them.
- **Line breaking.** Breaks follow UAX #14: CJK text breaks between ideographs but never before closing
  punctuation (、。」）) or after opening punctuation (「（). A no-break space holds two words together; a
  zero-width space is a break opportunity.
- **Color edits.** Changing a span's `color` recolors the shaped glyphs on the next frame without reshaping.
  Before, a color edit did not show until something else forced a reshape.

## What a shaped text reports

`ShapedText` gains:

| Field | Meaning |
|---|---|
| `lines` | One `ShapedLine` per laid-out line: `paragraph`, byte `range` in that paragraph, `first_glyph`, `glyph_count`, `width`, `baseline`, `rtl`, `ellipsized`. |
| `glyph_spans` | The span each glyph came from (same length as `glyphs`). |
| `missing_glyphs` | Glyphs drawn as a `.notdef` box because no loaded face covers the character. |
| `colors_digest` | The span colors the glyph colors were taken from. |

`regia_ui::text::shaped_string(text, loc)` is the exact string that was shaped (spans resolved, line breaks
normalized), and `text_paragraphs` splits it the way `ShapedLine::paragraph` counts.

## Method

`ui.text_shape` is read-only (permission `read`).

- `{ entity }`: a text node's current shape, the one on screen. `node` is an alias of `entity`.
- `{ text, font_px?, width?, wrap?, overflow?, family?, bold? }`: shapes a string now, touching no node. `font_px`
  defaults to 18 and `width` to unlimited (both in UI px, at most 512 and 16384). `wrap` is `word` (default),
  `glyph` or `none`. `overflow` is `visible` (default), `clip`, `ellipsis` or `scale_to_fit`. `text` is at most
  4096 characters.

Result: `{ text, size: [w, h], lines: [{ text, rtl, width, baseline, glyphs, ellipsized }], glyphs, missing_glyphs,
truncated, fonts, entity?, scale? }`. `fonts` lists the faces the glyphs came from, in first-use order.

Entity reports require the current content/locale digest written by the UI shaping pass. Hosts
that attach `TextSystem::shape` results manually must run the public `regia_ui::text::shape_text`
system before querying the entity. The regular UI plugin runs that system each frame.

```json
{"method": "ui.text_shape", "params": {"text": "Démarrer la partie", "width": 120, "wrap": "none", "overflow": "ellipsis"}}
{"method": "ui.text_shape", "params": {"entity": 4294967321}}
```

Typical checks: a translated label fits its button when `truncated` is false and there is one line; a script has a
face when `missing_glyphs` is 0.

## Errors

| Code | Meaning |
|---|---|
| `ui_text_missing` | neither `entity` nor `text` was given |
| `bad_text` | `text` is not a string |
| `text_too_long` | `text` has more than 4096 characters |
| `bad_font_px` | `font_px` is not a number above 0 and at most 512 |
| `bad_width` | `width` is not a number above 0 and at most 16384 |
| `bad_wrap` | `wrap` is not `word`, `glyph` or `none` |
| `bad_overflow` | `overflow` is not `visible`, `clip`, `ellipsis` or `scale_to_fit` |
| `bad_family` | `family` is not a non-empty string |
| `bad_bold` | `bold` is not a boolean |
| `bad_entity` | `entity` is not a node id |
| `unknown_entity` | the entity does not exist |
| `not_a_text` | the entity has no `Text` |
| `text_not_shaped` | the node has not been shaped yet, or its text/locale changed since shaping; step a frame and ask again |
| `ui_not_installed` | the app has no UI text system |

## Known limits

- The bundled emoji subset has no skin-tone modifiers and no keycap combining mark: those draw as a missing glyph
  (counted in `missing_glyphs`) unless a system emoji font is loaded.
- Ellipsis applies to unwrapped text only; a wrapped paragraph in a box that is too short keeps every line.
- The bundled faces cover Latin (Inter), Arabic, Hebrew, Devanagari, Thai, Simplified Chinese, Korean and color
  emoji (Noto subsets); other scripts (Lao, Khmer, Myanmar, ...) need a project font, and on Android (no system
  fonts) they show as missing glyphs.
