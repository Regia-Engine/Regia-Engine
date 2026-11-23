# Play window

Play-in-editor normally shows the game inside the **Viewport** tab. The play window shows it in a
second operating-system window instead, titled `Regia Engine Play`, with no editor overlays and at a
resolution you choose, so you can see the game at a phone or tablet size while the editor stays open.
It is not a dock tab: it is a mode of the play toolbar ([Play-in-editor](play-in-editor.md)). Depth:
[PIE modes and editor jobs](../../agents/pie-modes.md).

The window needs a window system. A host with none (the headless editor, CI) does not show the
toolbar's play window controls, and `editor.play` with `window: true` answers `play_window_unavailable`.

## Open it

- Click **⧉ Play in window** on the play toolbar. Closing the play window, or pressing **⏹ Stop**,
  ends play; the edit world was never touched.
- Agents call `editor.play` with `{"window": true}` and read the result with `editor.play_state`.

## Walk-through: check a level at phone size

1. Click the **⛶** button next to **⧉ Play in window** (it reads **⛶ Window size** until you pick a
   size) and choose **Phone 9:16 portrait (1080 × 1920)**. The choice is saved in the editor
   preferences as `play.resolution`.
2. Click **⧉ Play in window**. The game opens in the `Regia Engine Play` window. The picture is scaled to
   fit the window with its aspect ratio kept and black bars around it; the game's own window reports
   1080 × 1920, so its UI lays out as on that screen.
3. Click in the window to capture the mouse (the cursor is confined and hidden). A click on a point of
   the picture reaches the same point of the play world; a press or the wheel over the black bars
   reaches nothing. Press **Shift+F1** to release the mouse without stopping play.
4. While the window is open the **Viewport** tab does not draw the play world: it shows a note naming the
   window and its size. The **Outliner** and **Inspector** still show the play world, so you can inspect
   entities as the game runs.
5. Close the window (or press **⏹ Stop**) to end play. The window's size and position are remembered for
   the next one.

To play with two to four local players, open the **Play** menu, then **Players**, and pick a count
while play is stopped. With two or more players the play window shows one pane per player: two side
by side, three as one on top and two below, four in quarters.

## Actions

| Action | Control / key | Agent method |
|---|---|---|
| Play in the separate window | **⧉ Play in window** (toolbar) | `editor.play` with `window: true` |
| Choose the play resolution | **⛶** menu: a preset or a **Custom** width and height with **Use** | `editor.prefs_set` `play.resolution` (and `play.resolution_width`, `play.resolution_height`); `editor.play` `preset` or `size` for one session |
| Run with no local player | **▶ Simulate** (toolbar, **Play** menu) | `editor.play` with `mode: "simulate"` |
| Choose the local player count | **Play ▸ Players** (1 to 4, only while stopped) | `editor.play` with `players` |
| Copy the play state of the selection back to the level | **⤓ Keep changes** (enabled while a session runs) | `editor.keep_simulation_changes` |
| Release a captured mouse | Shift+F1 in the play window or the possessed viewport | none |
| Stop play and close the window | **⏹ Stop**, Escape in the editor window, or close the play window | `editor.stop` |
| Read the window's state | none | `editor.play_state` (`play_window`: `open`, `size`, `render_size`, `picture`, `panes`, `counters`) |

## Resolution presets

The **⛶** menu lists these presets, in this order, and **Custom**. Each side of a custom size must be
from 16 to 16384 pixels.

| Preset id | Size |
|---|---|
| `window` | the window's own size, no bars |
| `hd_720` | 1280 × 720 |
| `hd_1080` | 1920 × 1080 |
| `phone_9_16_portrait` | 1080 × 1920 |
| `phone_9_19_5_portrait` | 1080 × 2340 |
| `phone_9_19_5_landscape` | 2340 × 1080 |
| `tablet_3_4_portrait` | 1536 × 2048 |
| `tablet_3_4_landscape` | 2048 × 1536 |
| `custom` | `play.resolution_width` × `play.resolution_height` |

A change made in the menu while the window is open applies at once.

## Keyboard

Shift+F1 is the only key the play window handles for itself: it frees a captured mouse and never
reaches the game. Every other key, mouse and wheel event in the play window goes to the play world only,
and the editor window's events never reach the play world. Alt+P (play) and Escape (stop) work in the
editor window as on the [play toolbar](play-in-editor.md#keyboard-and-accessibility-notes).

## Known limits

- On Wayland a window cannot read or set its own position, so the window system places the play window
  and the remembered position is not used there.
- If the window system refuses to open the window, play goes on in the viewport and
  `editor.play_state` reports the reason in `play_window.error`.
- `editor.play` with `preset` or `size` but without `window: true` is refused
  (`play_resolution_needs_window`): the viewport always renders at its own size.
- The mouse writes one cursor in play pixels; routing the pointer to one player is the game's job
  (gamepads are per player).
- A level or game with no cameras of its own gets a stand-in camera for each local player in split screen;
  a game that authors its own cameras controls how the panes look.

## See also

[PIE modes and editor jobs](../../agents/pie-modes.md) (every `editor.play` option, the preset list,
error reasons), [PIE with several clients and net stats](../../agents/pie-net.md),
[Editor preferences](../../agents/editor-preferences.md), [Play-in-editor](play-in-editor.md).
