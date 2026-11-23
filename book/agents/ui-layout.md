# UI layout edge cases (agents)

ADR-518. How `regia_ui` lays out the cases that are easy to get wrong: right-to-left mirroring, wrapping, grid
tracks, min/max sizes, aspect ratios, safe areas, and style values that make no sense. Layout is taffy (CSS
flexbox and grid); the rules below are where Regia Engine's style differs from plain CSS or picks one reading of it.

## Checking layout from an agent

`ui.query` returns each node's `rect` (`[x, y, width, height]` in physical pixels) and its `layout_dir` (`ltr` or
`rtl`, the direction it lays out its children in). To check a layout, query the nodes and compare rects; to check
mirroring, switch the locale and query again.

```json
{"method": "ui.query", "params": {"name": "inventory_row"}}
```

The reply has `"layout_dir": "rtl"` for a node under an Arabic or Hebrew locale (or under an explicit `rtl`), and
the rects of its children run from the right edge.

## Right to left

- A node's `text_dir` is `auto`, `ltr` or `rtl`. `auto` inherits the parent's direction; a root with `auto` takes
  the active locale's direction (ar, he, fa, ur, ps, sd, yi, dv, ug, ckb are right to left). So an `ltr` panel
  inside a right-to-left screen keeps left-to-right children, and everything under it that says `auto` does too.
- In a right-to-left node, `row` runs from the right edge, `rowreverse` runs from the left, `justify_content:
  start` packs to the right, grid columns are placed right to left, and in a `column` the cross-axis `start` is the
  right edge.
- The left and right values of `margin`, `padding`, `border` and `inset` are start and end: in a right-to-left
  node they swap sides. `padding: (16, 0, 0, 0)` pads the right edge under an RTL locale.
- Safe-area insets do not swap: a notch on the left stays on the left.
- Changing a node's `text_dir` re-lays out every `auto` node below it on the next frame.

## Alignment

`justify_content: start` and `end` are flex-relative: they pack toward where the first item goes. A `rowreverse`
row with `start` hugs the right edge (left in RTL), like `flex-start` in CSS.

## Wrapping

With `flex_wrap: true`, items fill a line until the next one (plus the gap) would cross the content edge, then
start a new line. Items never shrink by default (`flex_shrink` is 0 unless set), so an item wider than the
container sits alone on its line and overflows it. `gap` is `[horizontal, vertical]`.

## Grid tracks

- `Px(n)` and `Percent(p)` tracks are fixed: content wider than the track overflows it instead of widening it
  (CSS `100px` / `50%`).
- `Fr(n)` shares the free space, but never shrinks below its content (CSS `minmax(auto, 1fr)`).
- Items are placed in reading order, filling each row of columns before the next row.
- A grid whose height is fixed stretches its auto rows to fill it; give the grid an `auto` height to keep rows at
  their content height.

## Min, max and aspect ratio

- `min_size` beats `max_size` when they conflict, on both axes.
- With `aspect_ratio` (`width / height`) and one axis given, the other axis is derived from it.

## Safe areas

A root with `SafeAreaPadding` adds the platform's safe-area insets to its own padding. Percentage padding on such
a root still counts (it resolves against the window width first). Insets that add up to more than the window
(a bogus platform report) are scaled down to fit it, so the root never grows past the window.

## Values layout refuses

Layout must not produce NaN or infinite rects from user content. Before taffy sees a style:

| Value | Becomes |
|---|---|
| a non-finite length, or a negative `size`, `min_size`, `max_size`, `padding`, `border`, `gap` or `flex_basis` | `Auto` (the property's initial value: no padding, no gap, no limit) |
| a negative margin or inset | kept (they are legal) |
| a length beyond ±1,000,000 physical px, a percentage beyond ±10,000 % | clamped |
| a non-finite or negative `flex_grow` / `flex_shrink` | 0 |
| an `aspect_ratio` that is not finite and positive | none |
| a grid track that is non-finite or negative | an `Auto` track |
| a non-finite or negative window size or safe-area inset | 0 |

These rules are checked by seeded property tests over random style trees (`crates/regia_ui/tests/layout_edge_cases.rs`):
random hostile trees always produce finite rects with non-negative sizes, and every random flex or grid tree laid
out under an RTL locale is the mirror image of its LTR layout.

## Not covered yet

- Scrolling: a right-to-left scroll view still measures its overflow toward the right, and the scroll offset
  still moves content to the left.
- Text inside a node shapes by its own bidi rules; `text_dir` changes box layout, not glyph order.
