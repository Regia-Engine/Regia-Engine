# Net

<!-- index: Net panel -->
<!-- index: Play in editor -->
<!-- index: Listen server -->
<!-- index: Dedicated server -->

Panel id: `net`. The **Net** tab sets how play-in-editor hosts a session, then shows the clients and the bandwidth of that session. It does not open a match on the public network. Depth: [Play-in-editor](play-in-editor.md), [PIE with several clients and net stats](../../agents/pie-net.md).

The tab sits in the default layout's bottom group. **View ▸ Reset layout** brings a closed tab back. An agent opens it with `editor.layout_apply {"open": "net"}`.

## Start a session

1. Choose **Mode**: `offline` (no session; play as usual), `listen` (this process is the server and a player) or `dedicated` (this process is a dedicated server).
2. Set **Clients** from 0 to 16. In one process those clients join in memory over the loopback transport. No socket is opened.
3. Choose **Conditions**: `off`, `lan`, `good`, `average`, `bad` or `terrible`. The same preset and seed report the same numbers on every run. The session clock advances 16 ms per editor frame.
4. Leave **Separate processes** off for the in-memory clients, or turn it on to start real processes (below).
5. Press **▶ Play** on the play toolbar (Alt+P). Stop ends the session. Pause holds the session clock.

With a game crate open, a mode other than `offline` and at least one client, the play world is the replication server and each client gets its own world. The main viewport keeps showing the server. **Client worlds** picks a client, and **Focus input on this client** sends keys to that client while the pointer is over its view.

**Overlay lines** adds the plain-text form of the connection rows under the table. Each connection row shows state, round-trip time, jitter, loss, and bytes and packets per second. The bandwidth table is per message kind.

## Separate processes

With **Separate processes** checked, play starts the server and the clients as their own processes on loopback, the same shape as `regia server --clients n`. **dedicated** starts a server process and one process per client. **listen** hosts on a loopback UDP port in the editor's play session and the client processes join it.

The binaries live in `build/server-dev/bin/` (`<game>-server` and `<game>-client`). Run `regia server --clients 1` once so those binaries exist. The editor does not build them.

**Show in a Game tab** opens that process in a dock tab. Those tabs use panel id `game_view` and are described on [Play-in-editor](play-in-editor.md#game-tabs).

## Actions

| Action | Control | Agent method |
|---|---|---|
| Mode, client count, conditions, separate processes | The controls at the top of the tab | `editor.pie_net_config` |
| Start or stop with the play toolbar | **▶ Play**, **⏹ Stop** | `editor.play`, `editor.stop` |
| Step the session clock | while paused, or from an agent | `editor.pie_net_run` (`action: "step"`) |
| Read clients and bandwidth | The tables | `editor.pie_net_state`, `editor.pie_net_clients` |
| Separate-process pictures | **Show in a Game tab** | `editor.pie_processes`, `editor.pie_process_view` |

## Keyboard

The tab's own controls are focusable (Tab, then Space or Enter). Play and Stop keep the toolbar keys on [Play-in-editor](play-in-editor.md).

## Known limits

- In-memory clients share one process and open no socket. They exercise handshake, join, streams and the conditioner. They are not a match between two machines.
- Separate processes use loopback addresses chosen at start. They are not a proof of a relay, a lobby, or a two-account online session.
- At most 16 clients. A failed client world unloads the client worlds already built; the play world stays as the game made it.
- A step longer than the links' silence timeout drops nobody: clients step as many frames as the play world did.

## See also

[Play-in-editor](play-in-editor.md), [Play window](play-window.md), [Networking](../networking.md), [Editor guide](index.md).
