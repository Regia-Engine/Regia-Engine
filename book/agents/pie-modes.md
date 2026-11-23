# Play-in-editor modes and editor jobs

Spec 19 § 1, § 4 and § 7 (ADR-7205). What play-in-editor offers beyond Play, Pause, Step and Stop
([Editor surface](editor.md#play-in-editor)), and how the editor's long jobs are cancelled.

## Play or Simulate

`editor.play` takes options:

| Param | Values | What it does |
|---|---|---|
| `mode` | `play` (default), `simulate` | **Simulate** runs the play world (physics, AI, the game's systems and scripts) with no local player spawned or possessed. The editor camera stays free. |
| `players` | `1`..`8` (default `1`) | `1` leaves joining to the game. `2` or more joins that many local players through the game's `PlayerManager` (inserted when the game has none), one split-screen viewport each: the play window and a possessed viewport composite [one pane per player](#split-screen-players). Simulate ignores it. |
| `window` | `false`, `true` | `true` plays in a separate window: see [The play window](#the-play-window). A host with no window system (the headless editor, CI) answers `play_window_unavailable`. |
| `preset` | a [play resolution](#play-resolution) id | With `window: true`: the size the play world renders at for this session (default: the `play.resolution` preference). |
| `size` | `[width, height]`, 16..=16384 each | With `window: true`: a custom play resolution for this session. Not with `preset`. |

Physics in play follows the game (ADR-7251). With a game crate open, the play world has physics when the
game's `register` adds `PhysicsPlugin` (the shipped templates do), exactly as in the shipped game; a game
without it gets no physics in Play or Simulate. Without a game crate, both modes add the physics plugin so
authored rigid bodies fall. Either way the physics spawn-order tracking starts before the level is spawned
(ADR-9180), so a game's `PhysicsPlugin` builds over a level that already holds colliders. A plugin the
play world already has (`ScenePlugin`, for one) is skipped when the game adds it again, with a warning
logged under target `regia_app`.

Simulate caps the play world's local players at 0 (`MaxLocalPlayers(0)` and a `PlayerManager` capped at 0)
before the game's `on_load`. A join press or a game's own `spawn_player_static` then gets `None`. A local
player the game spawned anyway is removed with its controller. Server bots (`spawn_bot_static`) are not
local players and still join.

`editor.play_state` reports:

- `kind`: `play`, `simulate`, or `null` when stopped.
- `players` and `viewports` (`[{ player, rect: [x, y, w, h] }]`, normalized): the play world's
  `PlayerManager` layout. Two players sit side by side, three as one on top and two below, four in quarters.
- `window`: whether the session asked for the play window; `play_window` reports the window itself.
- `panic_backtrace`: see [Game panics](#game-panics).
- `kept`: see [Keep simulation changes](#keep-simulation-changes).

```json
{"jsonrpc":"2.0","id":1,"method":"editor.play","params":{"mode":"simulate"}}
{"jsonrpc":"2.0","id":2,"method":"editor.play_state","params":{}}
```

In the editor window, the toolbar has **▶ Simulate** next to **▶ Play**, and **⧉ Play in window** where the
host has a window system.

## The play window

`editor.play { window: true }` (or the toolbar's **⧉ Play in window**) plays in a second OS window titled
`Regia Engine Play` (ADR-7272). Its answer names where play shows: `window: "play_window"`, or
`"viewport"` without the option. The window shows the play world through the game's camera (Play) or the
editor camera (Simulate), with no editor overlays. It has its own GPU surface on the editor's device.

- **Input.** Its keyboard, mouse, wheel and focus events go to the play world only, as the engine's input
  messages a game's `InputPlugin` reads. The play world gets a primary window entity sized like the window.
  The editor window's events never reach the play world, and the play window's never reach the editor's
  panels or shortcuts.
- **Mouse.** A click in the window captures the mouse. **Shift+F1** releases it and never reaches the game.
  Focus loss releases it too. While the session plays here, a click on the editor viewport (which keeps
  showing the play world) captures nothing on the editor window — this window owns the mouse.
- **Close.** Closing the window ends play. The edit world was never touched. Stopping play another way
  (toolbar, Esc in the editor window, `editor.stop`) closes the window.
- **Resize and surface loss.** A resize reconfigures only the window's surface. A lost surface is made again
  on the same window and device. The device and the editor's other GPU resources stay.
- **Size and position.** The editor preferences `play.window_width`, `play.window_height`,
  `play.window_placed`, `play.window_x` and `play.window_y` hold them, written when the window closes and
  when the editor exits with the window open, so the next play window opens where the last one was.
  Wayland does not let a window read or set its position, so there the window system places it.
- **One render per frame.** While the play window is open, the viewport does not draw the play world: it
  shows a note naming the window and its resolution (ADR-7283). Play renders once per editor frame, in the
  window. The Outliner and Inspector still show the play world. With two or more split-screen `players`, the
  window composites one pane per player (see [Split-screen players](#split-screen-players)).
- **Hidden editor window.** A compositor stops the redraws of an editor window that the play window
  covers. When no editor frame ran for 100 ms, the play window's own redraw runs the editor frame, so play
  keeps stepping.

### Play resolution

The play world can render at a size of its own instead of the window's (ADR-7283). The picture is scaled to
fit the window with its aspect ratio kept, centered, with black bars around it. The game's primary window
reports the chosen size, so its UI lays out as on that screen. Cursor positions are mapped back into the play
world's pixels: a click on a point of the picture reaches that point of the play world. A press or the wheel
over the bars reaches nothing and does not capture the mouse. Resizing the window moves the bars; the
game's window size stays.

| Preset | Size |
|---|---|
| `window` (default) | the window's own size (no bars) |
| `hd_720` | 1280 × 720 |
| `hd_1080` | 1920 × 1080 (also a 9:16 phone held landscape) |
| `phone_9_16_portrait` | 1080 × 1920 |
| `phone_9_19_5_portrait` | 1080 × 2340 |
| `phone_9_19_5_landscape` | 2340 × 1080 |
| `tablet_3_4_portrait` | 1536 × 2048 |
| `tablet_3_4_landscape` | 2048 × 1536 |
| `custom` | `play.resolution_width` × `play.resolution_height` |

The toolbar's **⛶** menu next to **⧉ Play in window** picks a preset or a custom size and stores it in the
editor preferences (`play.resolution`, see [Editor preferences](editor-preferences.md)). A change while the
window is open applies at once: the game's window takes the new size and gets a `WindowResized` message.
`editor.play`'s `preset` or `size` overrides the preference for one session (that session keeps its size). The answer reports the session's resolution:

```json
{"jsonrpc":"2.0","id":1,"method":"editor.play","params":{"window":true,"preset":"phone_9_16_portrait"}}
```

```json
{ "queued": true, "mode": "play", "players": 1, "window": "play_window",
  "resolution": { "preset": "phone_9_16_portrait", "size": [1080, 1920] }, "hint": "..." }
```

A size no preset has reports `preset: "custom"`; the window's own size reports `size: null`. The viewport
always renders at its own size, so `preset` or `size` without `window: true` is
`play_resolution_needs_window`.

`editor.play_state` reports the window as `play_window`. `size` is the OS window's, `render_size` the play
world's, and `picture` the `[x, y, width, height]` rect the picture fills in the window. A closed window
reports the resolution the next one opens with.

```json
{ "open": true, "title": "Regia Engine Play", "size": [1280, 720], "position": null, "captured": false,
  "error": null, "resolution": { "preset": "phone_9_16_portrait", "size": [1080, 1920] },
  "render_size": [1080, 1920], "picture": [437, 0, 405, 720],
  "counters": { "windows_opened": 1, "windows_closed": 0, "surface_configures": 0, "surface_recreations": 0,
    "frames_presented": 240, "frames_skipped": 0 } }
```

`frames_presented` counts the play window's drawn frames. `frames_skipped` counts frames with no surface
texture: lost (the surface is made again and play keeps going), occluded or timed out. A surface that
cannot be made again puts the reason in `error`; the next lost frame tries again, and success clears it.

If the window system refuses the window, play goes on in the viewport and `error` says why.

```json
{"jsonrpc":"2.0","id":1,"method":"editor.play","params":{"window":true}}
{"jsonrpc":"2.0","id":2,"method":"editor.play_state","params":{}}
```

### Split-screen players

With `players` 2 or more (ADR-7395), the play window composites one pane per local player — each player's
window view drawn into that player's viewport rect of the play-resolution picture, exactly as the game's
own window draws them. Two players sit side by side, three as one on top and two below, four in quarters
(the player manager's layout). A possessed viewport pane composites the same picture, so viewport play shows
the split too; the viewport's P1/P2 outlines label the panes.

- **Cameras.** The engine leaves split-screen presentation to the game (the game copies each player's rect
  onto its own cameras). A play world with **no** window cameras — a game-less level, or a game that spawns
  none — gets a PIE stand-in camera per local player (`PiePlayerCamera`): welded to the player's
  controller, at the player's rect, drawn in player order. The controllers PIE joined are stood 4 m apart so
  the panes differ; a controller the game joined or placed keeps its transform. A game or level that
  authored a camera owns presentation: PIE adds nothing, and whatever window cameras exist composite in
  their own rects and order.
- **When PIE decides (ADR-7466).** The stand-ins are kept every frame in `PostUpdate`, after the game's
  `Startup`, `Update` and observers ran: a camera the game spawns in `Startup` (as the arena sample does)
  means no stand-in ever appears; a camera the game spawns later replaces them; a player who leaves loses
  its stand-in and a player joined at runtime gets one. The stand-ins count in `editor.play_state`'s
  `entities`, kept current as they come and go: a departed player's stand-in, the stand-ins a late game
  camera replaces and a runtime join's stand-in all move the count (the players and controllers the game
  joins or removes after start are the game's own entities and do not). A possessed pane composites whenever it has more than one window view, so a one-player game
  with two window cameras also composites them in their rects.
- **Resolution and input.** The [play resolution](#play-resolution) and the letterbox work as with one
  player: the panes tile the picture, so a window point maps to the same play pixel, and a press in the
  bars reaches nothing. The mouse still writes one cursor in play pixels; per-player pointer routing is the
  game's (gamepads are per player).
- **Status.** `editor.play_state` reports the session's `players` and `viewports` (normalized rects), and
  `play_window.players` with `play_window.panes: [{ player, rect: [x, y, w, h] }]` — the split the open
  window shows, in window pixels (empty when no window is open or there is no split). The pane rects are
  the ones the composite draws (`regia_render::pixel_viewport` on the play target, mapped into the
  letterboxed picture), so on an odd size such as 1279 x 719 two halves read `[0, 0, 640, 719]` and
  `[639, 0, 640, 719]`.

```json
{"jsonrpc":"2.0","id":1,"method":"editor.play","params":{"window":true,"players":2}}
{"jsonrpc":"2.0","id":2,"method":"editor.play_state","params":{}}
```

```json
{ "open": true, "size": [1280, 720], "resolution": { "preset": "window", "size": null },
  "render_size": [1280, 720], "picture": [0, 0, 1280, 720], "players": 2,
  "panes": [ { "player": 0, "rect": [0, 0, 640, 720] }, { "player": 1, "rect": [640, 0, 640, 720] } ],
  "counters": { "windows_opened": 1, "windows_closed": 0, "surface_configures": 0, "surface_recreations": 0,
    "frames_presented": 240, "frames_skipped": 0 } }
```

A player the game joins at runtime gets a stand-in welded to its controller, but PIE never stands that
controller apart: only the controllers PIE joined at play start are moved (ADR-7466), and a runtime join is
the game's. A runtime-joined controller keeps the transform the game gave it, so a game that leaves it at
the identity transform shows the same view in its pane as any other player whose controller stands at the
origin (player 1's, when PIE joined it); place the controller when you join the player, or spawn a camera
for it.

## Keep simulation changes

`editor.keep_simulation_changes { guids? }` copies the play-world state of the selected entities, or of
`guids`, back to the edit world as **one undo step**. It needs a running session; without one it answers
`not_playing`. The play world is a copy spawned by guid, so a play entity and its edit counterpart share an
`EntityGuid`. For each one, every reflected component the edit entity already has is compared with the play
value. Each component that differs is written. Components only the play world has (physics handles,
velocities, a game's runtime state) are not added. `GlobalTransform`, `PreviousGlobalTransform`, hierarchy links, the guid, the level
membership and the name are never copied.

The request is applied on the next editor frame. `editor.play_state`'s `kept` then reports `{ entities,
components, copied, missing, undo_step }` (`copied`: the type path of each copied component), or `{ error: "keep_nothing_selected" }` when nothing was selected and no
`guids` were given. `editor.undo` reverts the whole keep. The toolbar's **⤓ Keep changes** does the same for
the play world's selection.

## Input capture

A click on a possessed play view (F8) or in [the play window](#the-play-window) captures the mouse: the cursor is confined and hidden, as the game
expects. **Shift+F1** releases it without stopping play. Stopping play or the window losing focus releases it
too. There is no agent method for this; agents send input through `editor.send_key` and the play methods.

## The EditorOnly save filter

Entities marked `regia_editor::editor_only::EditorOnly`, or `GltfEditorOnly` (a glTF hierarchy spawned with
`editor_only: true`), are left out of every saved level, together with their descendants. That covers
`editor.save_level`, the autosave, the level-directory writer and the glTF export. Cooks read the saved level,
so they never see them. The edit world keeps them.

## Game panics

A panic in game code during play stops the session (spec 19 § 7). The Output Log gets one `regia_editor`
error record with the message, then `backtrace:` and the frames, up to 80 lines. The editor binaries also
log every panic under the target `panic`. `editor.play_state` keeps `panic` and `panic_backtrace` after the
session ended, until the next play starts. `logs.read { contains: "PIE panic" }` finds the record.

## Cancelling the import and the game build

`editor.import { sources, folder?, async: true }` answers `{ job: "import:<n>" }` at once and imports on a
worker thread. Extension importers still run first, on the editor thread. `editor.cancel_job { id }` stops it.
The worker checks the cancel before each file, before the cook and after it. A cancelled import removes every
file it had copied, with its `.meta`, so no partial asset stays in `assets/`. The job then ends `cancelled`
with `{ cancelled: true, imported: [] }`. A cancel that arrives after the worker's last check cannot stop
the import any more: the files stay and the job ends `done` with `cancelled: false`, so the job's state, its
result and `assets/` always agree. Poll `agent.job_status` for the end. The Content Browser's **Import…** button runs the same job and shows
**Cancel import** while it runs.

`editor.cancel_job { id: "hot:<n>" }` cancels an `agent.hot_reload` job and stops its game build. Cargo is
killed and nothing is copied into `.regia/hot/`. The job ends `cancelled` with `build_stopped`, and the old
code keeps running. `editor.cancel_job { id: "build" }` stops a build the toolbar, Ctrl+Alt+F11 or a source
change started (`requested: false` when none runs). `agent.job_cancel` reaches the same jobs.
It also cancels an import-settings write (`import_settings:<n>`, [Import settings panel](import-settings.md)) and a
navigation preview bake (`nav_preview:<n>`, [Viewport flags](viewport-flags.md)), the worker jobs of ADR-7664.

| Error reason | When |
|---|---|
| `play_mode_unknown` | `editor.play`'s `mode` is neither `play` nor `simulate`. |
| `play_players_out_of_range` | `players` is not a whole number 1..=8. |
| `play_window_unavailable` | `window: true` on a host with no window system (the headless editor, CI). Nothing starts. |
| `play_resolution_invalid` | `preset` is not a [play resolution](#play-resolution) id, `size` is not `[width, height]` with sides 16..=16384, or both were given. Checked before the window system, so every host names it. |
| `play_resolution_needs_window` | `preset` or `size` without `window: true`. |
| `not_playing` | `editor.keep_simulation_changes` without a running session. |
| `keep_nothing_selected` | The keep found no selection and no `guids` (reported in `kept`). |
| `job_not_found` | `editor.cancel_job` for an id the editor did not start. |
| `import_failed` | The async import's worker could not start, or the import failed before copying. |

See also [PIE with several clients and net stats](pie-net.md) and [Editor surface](editor.md).

## Start options, speed and mute

ADR-7829. Play can start somewhere other than the level's player start, and a running session can be slowed down,
sped up and muted. None of this touches the edit world: the play world is a copy.

`editor.play` takes four more options:

| Param | Values | What it does |
|---|---|---|
| `start` | `player_start` (default), `camera`, `entity` | **player_start**: the game's spawn rules pick the level's player start. **camera**: a play-only player start is placed at the editor camera's pose (and every level start loses its marker in the copy), so the player spawns where the camera is; a play world that has no player after its first frame (a game with no player, or Simulate) gets its window cameras placed at that pose instead. **entity**: only the player start named by `entity` keeps its marker, so the spawn rules can only pick it. Default: the toolbar's choice. |
| `entity` | a player start's entity guid | With `start: "entity"`. |
| `time_scale` | `0.1`..=`4` (default `1`) | The session's starting gameplay speed (the play world's `Time::time_scale`). Pause stays `editor.pause`; `0` is refused. |
| `mute` | `false`, `true` | Start with the play world's audio muted. |

`editor.play_controls { time_scale?, mute? }` changes a running session: the speed (0.1 to 4) and the play world's
mute. Mute silences the root bus of the play world's own audio (a game's `regia_audio::Audio`); the editor's own
sounds (the asset preview) keep playing. A game that builds its audio later, or unmutes its root bus itself, is
muted again while the mute is on, and turning the mute off restores the game's own root-bus state. Both reset when
play stops: the next session starts at real speed, unmuted, unless its `editor.play` says otherwise.

`editor.play_state` adds `time_scale` (0 while stopped), `muted` and `start` (`{ kind, entity? }`, `null` while
stopped).

Error codes (in `data.reason`): `play_start_unknown` (another `start`), `play_start_not_player_start` (`start:
"entity"` without an `entity` that is a player start of the open level), `play_time_scale_out_of_range` (outside
0.1..=4), and `not_playing` from `editor.play_controls` with no session.

```json
{"jsonrpc":"2.0","id":1,"method":"editor.play","params":{"start":"camera","time_scale":0.5,"mute":true}}
{"jsonrpc":"2.0","id":2,"method":"editor.play_controls","params":{"time_scale":2}}
```

In the editor window, the play toolbar's **⚑** menu chooses the start (From player start, From editor camera, and
one entry per player start when the level has several); Play, Simulate and Alt+P use it. While a session runs the
toolbar shows a **Speed** slider and a **Mute** toggle. All of them are reachable with Tab and work with Space or
Enter.

## Play-mode tint

While a session runs, the editor borders the play viewport and tints the play toolbar with the theme's
`accents.play_mode` color (ADR-7829), so edits made in play are obvious. With PIE client worlds the color is the
viewed role's: `accents.play_server` for the server, `accents.play_client_1` to `_4` for clients (client 5 uses
client 1's color again). Change the colors with `editor.theme_set` (`{ "accents": { "play_mode": "#rrggbb" } }`);
turn the tint off with the `play.tint` preference (`editor.prefs_set { key: "play.tint", value: false }`). The
border is 3 points wide and scales with `appearance.ui_scale`.

## Device simulation

`editor.play { window: true, device: { preset, landscape, touch, gamepad, safe_area_outline } }` (ADR-7829) simulates a device in the
play window. `device: null` turns it off. The toolbar's **📱** menu (next to the play resolution menu) sets the
same things, and changes apply to an open play window at once.

| Field | Values | What it does |
|---|---|---|
| `preset` | `phone`, `handheld`, `null` | The play world renders at that device's screen (it wins over `preset`/`size`), and the game gets its safe-area insets as `regia_platform::SafeArea`, as a device build reads them from the OS. The screens and insets are the UI designer's own presets (`phone` is the designer's 1080 × 2400 with a 110 px top inset and a 60 px bottom inset; `handheld` is 1280 × 800 with none), so a layout checked in the designer plays the same. |
| `landscape` | `false`, `true` | Turns the device a quarter turn: the cutout edge goes to the left. |
| `touch` | `false`, `true` | Mouse to touch: a left press, drag and release in the picture become one touch (`TouchInput`, id 0) instead of mouse input. Hold Ctrl at the press for a second touch (id 1) that mirrors the first around the press point, so dragging away spreads two fingers (pinch out) and dragging back closes them. The window does not capture the mouse for touches. |
| `gamepad` | `false`, `true` | A virtual pad over the picture: a direction pad bottom left, four face buttons bottom right, Select and Start bottom center. A left press on one presses that button on a virtual pad until the release (`GamepadButtonChanged`, device `0x5649525450414400`); the pad's first press announces it with `GamepadConnected`. Presses elsewhere go on as touches or mouse input. |

`editor.play_state.device` reports `{ preset, landscape, touch, gamepad, size, safe_area: [left, top, right,
bottom], cutout: [x, y, w, h], pad_buttons: [{ button, rect: [x, y, w, h] }] }` in play pixels. The cutout is
derived from the deepest inset: centered on that edge, a third of the edge long, 60 % of the inset deep.

Error codes: `play_device_preset_unknown`, `play_device_needs_window` (a `device` without `window: true`).

The composed play window draws the device outline, cutout and virtual pad over the game
picture (ADR-8205). `safe_area_outline: true` draws the safe-area boundary; the default
is false. The toolbar's **Safe-area outline** toggle changes it while playing. These
markings never enter the game's render target or its captures. Colors come from the
editor's theme tokens; strokes, pad buttons and labels follow `appearance.ui_scale`.
The pad clusters fit at most twice their base size to stay inside the picture
without overlap. `pad_buttons` reports the actual scaled hit rectangles in play
pixels while the window is open, including when no device preset is selected.

With the play window's mouse uncaptured (Shift+F1 releases it), Tab/Shift+Tab selects
the next/previous pad button. Space or Enter holds that button until release; Escape
releases it and leaves the pad. A focus ring names the current button visually.
Captured gameplay keeps its normal keyboard input. Losing focus, disabling the pad,
turning the device or stopping play releases held input. Pointer clicks and keyboard
activation use the same input path. The pad supports one held button at a time.

`editor.play_controls { device: { safe_area_outline?, button?, pressed? } }` changes
the live outline or presses/releases a pad button without restarting play. Supply
`button` and `pressed` together; names are `dpad_up`, `dpad_down`, `dpad_left`,
`dpad_right`, `north`, `south`, `west`, `east`, `select`, `start`. The virtual gamepad
must be enabled. Outline changes are immediate view state; pad transitions apply on
the next window sync. At most 128 transitions can wait for a sync. Duplicate presses
of the held button do nothing; pressing another releases the previous one first.
A release of a different button does nothing. Queued input is discarded on restart.

`editor.play_state.device` also reports `safe_area_outline`, `held_button` and
`focused_button` (the latter two are names or null). Error codes: `not_playing` if
play has stopped, `play_device_needs_window` if the play window is not open, and
`invalid_params` for a malformed control, an unknown button, a disabled pad or a full
input queue. Invalid compound requests apply nothing.

```json
{"jsonrpc":"2.0","id":1,"method":"editor.play","params":{"window":true,"device":{"preset":"phone","gamepad":true,"safe_area_outline":true}}}
{"jsonrpc":"2.0","id":2,"method":"editor.play_state","params":{}}
{"jsonrpc":"2.0","id":3,"method":"editor.play_controls","params":{"device":{"button":"south","pressed":true}}}
{"jsonrpc":"2.0","id":4,"method":"editor.play_state","params":{}}
{"jsonrpc":"2.0","id":5,"method":"editor.play_controls","params":{"device":{"button":"south","pressed":false,"safe_area_outline":false}}}
```

Wait for `play_window.open` before the live control and poll `held_button` after
its queued transition. The dispatcher regression exercises that sequence; the
composed-window pixel check uses lavapipe and compares source and destination
textures directly, without golden images.
