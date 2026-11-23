# UI glyph atlas (agents)

ADR-8832; spec `docs/specs/18-runtime-ui.md` § 2. The runtime UI rasterizes glyphs on demand into atlas pages:
`max_pages` square pages of `page_size` pixels for monochrome glyphs (R8) and as many for color glyphs (RGBA8),
shelf-packed. A glyph is keyed by face, size, weight and sub-pixel position, so the same letter at two sizes or two
sub-pixel offsets takes two slots.

## Read the atlas

`ui.glyph_atlas` (read-only, no parameters):

```json
{ "method": "ui.glyph_atlas" }
→ { "page_size": 512, "max_pages": 8, "mono_pages": 3, "color_pages": 1, "glyphs": 412,
    "wipes": 0, "thrash": 0, "thrash_frames": 0 }
```

- `mono_pages`, `color_pages`: pages in use per kind (at most `max_pages` each).
- `glyphs`: glyphs cached across both kinds.
- `wipes`: pages recycled so far. When every page of a kind is full, the least recently used page is cleared and
  its glyphs re-rasterize when next drawn. A few wipes after a screen change are normal.
- `thrash`: glyphs that were not drawn because every page of their kind held glyphs the same frame had already
  drawn. The UI extract pins each page it draws from until the frame ends, so a page is never recycled under quads
  that still point into it (the glyph would otherwise show another glyph's bitmap for a frame). A glyph that cannot
  fit is refused instead: it is missing from that frame. `thrash_frames` counts the frames that refused at least one
  glyph; the engine logs a `regia_ui` warning on the first such frame and then at every power of two.

`thrash` above 0 means one screen needs more distinct glyphs than the pages hold: many sizes, a dense CJK screen at
several sizes, or many sub-pixel positions. Fewer sizes, or `SdfText` for large text, bring it back to 0.

Errors: `ui_not_installed` (`F0013`): the app has no UI renderer (`regia_ui::UiPlugin`).

## Check it

```bash
cargo test -p regia_ui --lib atlas
cargo test -p regia_agent --lib ui_glyph_atlas
```
