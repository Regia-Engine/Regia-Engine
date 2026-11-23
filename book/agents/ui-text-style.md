# UI text effects (agents)

ADR-273 T07, ADR-2045. Code: `regia_ui::text_effects`, the new fields of `regia_ui::text::TextStyle`.

A text span can now carry:

- `letter_spacing`: extra space after every character, in em (tracking). 0 keeps the font's own spacing.
- `weight`: an explicit weight from 1 to 1000 (400 regular, 700 bold). It overrides `bold`; a face without that weight
  falls back to the nearest one the font stack has (the bundled Inter ships regular and bold).
- `underline` and `strikethrough`: lines placed from the font's own metrics. Rich-text markup writes them with `<u>` and
  `<s>`. Both draw after the glyphs, in the span's color.
- `features`: OpenType feature settings such as `"smcp"` (small caps), `"tnum"` (tabular figures) or `"liga=0"`
  (ligatures off). A face without the feature ignores it.
- `effects`: up to four outline or shadow layers drawn under the glyphs. A layer is `{ color, width, blur, offset }`
  in UI px. `width` dilates the glyph (an outline); `offset` moves the layer (a shadow); `blur` softens it (with
  `width` 0 it is a Gaussian blur with σ = blur / 2, like CSS `text-shadow`; with `width` above 0 it widens the
  outline's edge into a glow). Width and blur clamp at 8 physical px. Layers stack front to back: the first sits right
  under the glyphs. Every layer of a text block draws before any of its glyphs, so an outline never covers the
  neighboring letter. Color emoji draw without effects.

Everything is off by default: a span without these settings shapes and draws exactly as before. `.ui.ron` v4
(ADR-2045) added `letter_spacing`, `weight`, `underline`, `strikethrough`, `features` and `effects` as optional
text-style fields, so a file can carry them (current version 9; older files migrate on load). `sdf` is not a
`.ui.ron` field.

The drawn layers use one more kind of the UI overlay shader (kind 7). It reads the glyph's own atlas rect only, so
neighbors in the atlas never bleed in. Its per-pixel math is mirrored on the CPU by
`regia_ui::text_effects::effect_coverage`, which the tests check.

## Distance-field text

A node marked `regia_ui::sdf_text::SdfText` (or `ui.text_style { sdf: true }`) draws its monochrome glyphs from signed
distance fields (ADR-273 L07): each glyph is rasterized once at 48 px, turned into an exact Euclidean distance field
with a 6 px margin, stored in the normal glyph atlas under its own key, and drawn at any size by overlay kind 9, which
turns the sampled distance into coverage at the screen scale. One field serves every size, so text that is drawn
large, or whose size animates, stays sharp without a new bitmap per size. Small body text looks slightly softer than
the hinted bitmaps, so leave it off there. Color emoji keep the bitmap path, and text effects do not apply to
distance-field glyphs. `regia_ui::sdf_text::sdf_coverage` is the CPU model of the shader.

## Methods

- `ui.text_style { entity, span?, letter_spacing?, weight?, underline?, strikethrough?, features?, effects?, sdf? }`: with
  only `entity` it reads every span's settings and the last shape: `{ glyphs, size, decorations: [{ rect, span, kind }] }`.
  Shaping runs at the end of the frame, so an edit shows in the read after the next frame. The other parameters edit
  every span, or only `span`. `weight: null` goes back to the bold flag. A failed call changes nothing. Mutating;
  permission `edit_scene`.

```json
{"method": "ui.text_style", "params": {"entity": 4294967310,
  "effects": [{"color": "#000000", "width": 2}, {"color": [0, 0, 0, 0.6], "offset": [3, 3], "blur": 4}]}}
```

`sdf` switches distance-field drawing for the whole node. Reply: `{ entity, changed, sdf, spans: [{ letter_spacing, weight, bold, underline, strikethrough, features, effects }], shaped }`.

## Errors

| Code | Meaning |
|---|---|
| `bad_entity` | `entity` is not entity bits |
| `unknown_entity` | the entity does not exist |
| `no_text` | the node has no `Text` |
| `bad_span` | `span` is past the text's spans |
| `bad_param` | `letter_spacing` (-1 to 4), `weight` (1 to 1000), `underline`, `strikethrough` or `sdf` is malformed |
| `bad_feature` | a feature is not a four-character tag with an optional `=value`, or there are more than 16 |
| `bad_effect` | an effect is malformed, out of range (width and blur 0 to 8, offset within 256) or there are more than 4 |
| `bad_color` | an effect color is not `#RRGGBB[AA]` or `[r, g, b, a]` |

## Showcase

The `ui_gallery` sample's Parity page (key 6, or the "Parity" toolbar button; node `gallery.page.parity`) shows an
outline, a blurred shadow, a stacked outline with a glow, underline and strikethrough markup, letter spacing with an
explicit weight, tabular figures and 48 px distance-field text (nodes `gallery.parity.*`).
