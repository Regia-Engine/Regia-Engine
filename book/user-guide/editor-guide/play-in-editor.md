# Play-in-editor

<!-- index: Play in editor -->

The play toolbar above the dock runs the level inside the editor. Play copies the edit world
into a fresh play world, so nothing you do while playing changes the level; Stop drops the play
world and the edit world is exactly as before. The **Net** tab (panel id `net`, [Net](net.md)) plays with a listen server or a
dedicated server and several clients. A separate process shown in the dock uses panel id `game_view` ([Game tabs](#game-tabs)). Walk-through: [Your first scene in the editor
§ 5](../../tutorials/editor-first-scene.md#step-5-play-in-the-editor). Depth: [Editor
surface](../../agents/editor.md), [PIE with several clients and net stats](../../agents/pie-net.md),
[Hot reload](../hot-reload.md).

| Action | Control / key | Agent method |
|---|---|---|
| Play | **▶ Play**, Alt+P (when no session runs and no text field has focus) | `editor.play` |
| Pause | **⏸ Pause** | `editor.pause` (`play.pause` in the play world) |
| Resume | **▶ Resume** while paused | `editor.resume` |
| Step one frame | **⏭ Step** (while paused) | `editor.step` (`frames` for more) |
| Stop | **⏹ Stop**, Escape (when no panel uses Escape) | `editor.stop` |
| Session state | The status text on the toolbar | `editor.play_state` |
| Possess or eject the game camera | **Possess (F8)** in the viewport toolbar, F8 | `editor.set_viewport` (`possessed`) |
| Compile the game and hot-reload it | **⚙ Compile**, Ctrl+Alt+F11 | `editor.code_status` (reads the build state) |
| Static entities that moved | none | `editor.static_move_warnings` |
| Several clients, listen or dedicated server | **Net** tab | `editor.pie_net_config`, `editor.pie_net_run`, `editor.pie_net_state`, `editor.pie_net_clients`, `editor.pie_processes` |
| Show a separate process in a Game tab (pause, step, speed, mute, input, pop out) | **Net** tab ▸ **Show in a Game tab**; in the tab, click or Tab to the picture, Shift+F1 to leave | `editor.pie_process_view` |

While a session runs, the runtime `play.*` methods (`play.wait_until`, `play.expect`,
`play.inject_input`, ...) act on the play world ([Agent protocol](../agent-protocol.md)).

## Keyboard and accessibility notes

Play, Stop and possession have keys; Pause, Resume and Step are focusable toolbar buttons. Before
this guide's review the toolbar had no resume: while paused, **⏸ Pause** stayed enabled and did
nothing and **▶ Play** started a new session. The toolbar now shows **▶ Resume** while paused
([Editor accessibility](accessibility.md), finding E-1).

The separate play window is [Play window](play-window.md). Several clients are [Net](net.md).

## Game tabs

With **Separate processes** checked in the Net tab, each running process (the dedicated server and every
client) has a **Show in a Game tab** button. The tab shows that process's pictures with the frame number and the
latency, and a toolbar: **Pause** / **Resume**, **Step** (one frame of game time), **Slower** / **Faster**,
**Mute**, **Pop out** (a client starts again in its own window) and **Stop process**. Every control is a
focusable button or checkbox. Click the picture, or reach it with Tab, to send keys and the mouse to that process
only (Tab and the arrows go to the game too); **Shift+F1** gives the keyboard back to the editor. Closing the tab
leaves the process running. Details: [PIE with several clients and net stats § Game
tabs](../../agents/pie-net.md#game-tabs).
