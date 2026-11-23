# Viewport resolution preview (`editor.viewport_resolution_preview`)

ADR-7744. Code: `regia_editor::resolution_preview`. The user's view is [Editor](../user-guide/editor.md).

The edit viewport can frame the picture the play window would have at a chosen play resolution. The toolbar's
**Resolution** toggle turns it on; its combo picks a preset. The perspective pane then dims the bars outside the frame,
outlines the frame, draws the action-safe (5 % in from each side) and title-safe (10 %) guides and labels the size.

The frame is the play window's own rect: `resolution_preview::frame_rect` calls `play_window::Letterbox::fit`, the
function the play window uses for the rect its picture lands in, so the overlay and the window agree to the pixel at
any pane size. The resolutions are the `play.resolution` presets (`window`, `hd_720`, `hd_1080`, `phone_9_16_portrait`,
`phone_9_19_5_portrait`, `phone_9_19_5_landscape`, `tablet_3_4_portrait`, `tablet_3_4_landscape`) or a custom size.
It is editor view state: no undo step, nothing saved, and play-in-editor does not read it.

```json
{ "method": "editor.viewport_resolution_preview", "params": { "preset": "phone_9_16_portrait", "pane": [800, 600] } }
```

```json
{ "on": true, "preset": "phone_9_16_portrait", "size": [1080, 1920], "pane": [800, 600],
  "rect": [231, 0, 338, 600], "safe_rects": [[243, 30, 314, 540], [298, 60, 202, 480]] }
```

| Param | Meaning |
|---|---|
| `preset` | A preset id above. Turns the preview on. |
| `size` | `[width, height]`, 16 to 16384 each. Turns the preview on. |
| `off` | `true` turns the preview off; the chosen resolution is kept. |
| `pane` | `[width, height]` of a pane in pixels: the reply adds the frame `rect` and `safe_rects` (`[x, y, width, height]` in pane pixels) for it. |

No params reads the state. Error codes (`data.code`): `viewport_resolution_invalid` (unknown preset, a size that is
not two whole numbers or is out of range, a bad `pane`), `viewport_resolution_conflict` (more than one of `preset`,
`size` and `off`). The toolbar toggle is the registered action `viewport.resolution_preview` (`editor.actions { panel:
"viewport.toolbar" }`).
