# UI theme sheet

The `ui_gallery` sample has a **Theme sheet** page (lane D3 of `docs/specs/ui-default-design.md`): every
existing widget (button, primary button, toggle, slider, progress bar, text input, dropdown, tab bar) in every
style state (normal, hover, pressed, focused, selected, error, disabled) and every size (`sm`, `md`, `lg`), so
one screenshot shows the whole look of the active theme. It is the baseline the default-design lanes are
compared against.

## Opening the page

- Gamepad or keyboard navigation: focus the **Theme sheet** toolbar button and confirm. It is a normal
  focusable button, so no mouse is needed.
- Key `5`, or `ui.click { "name": "gallery.page.theme_sheet" }` (see [ui](ui.md)).
- `T` (or the **Toggle theme** button) switches between the two built-in schemes.

## Reading it

Cells are static: they carry the state marker components and a `WidgetSize`, and are not interactable or
focusable. Each cell's `NodeName` is `sheet.<widget>.<size>.<state>`, for example `sheet.primary.lg.disabled`
or `sheet.toggle.sm.selected`. `ui.node_state` on a cell reports its markers; `Themed` holds the resolved look.

Take the page with `seeing.screenshot` at 1600x1500 or larger (the page is not scrolled).

## Goldens

`samples/ui_gallery/tests/theme_sheet.rs` renders the page in both schemes offscreen and compares against
`tests/goldens/theme_sheet_dark.png` and `theme_sheet_light.png` (lavapipe-blessed). Those goldens are not
blessed in the repository yet: until they are, the test checks that the capture is not one flat color, prints a
`golden ... is not blessed yet` line and skips the comparison. Bless with
`REGIA_BLESS=1 cargo test -p ui_gallery --test theme_sheet`; the reason in the commit message is
"baseline before default-design lanes". When the default look changes (lane D6) the goldens are re-blessed
as an intended change.
