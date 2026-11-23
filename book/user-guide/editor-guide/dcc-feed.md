# DCC Feed

The **DCC Feed** tab controls the live scene feed: a content tool (a modeling or animation
application with a feed add-on) streams meshes, transforms and clips into the running editor, where
they appear as ordinary entities marked as feed-owned. When you are happy with the result you
**bake** the items into normal `.glb` assets. The feed is a development channel: it exists only in
builds with the `dcc-feed` feature (the editor), and it is off until the project turns it on.
Depth: [DCC scene feed (agents)](../../agents/dcc-feed.md).

## Open it

The tab is part of the default layout (the group under the viewport); **View ▸ Reset layout**
brings it back when closed. An agent uses `editor.layout_apply {"open": "dcc_feed"}`. The command
palette has no `open` command for this tab. In a build without the feed the tab reads "The DCC feed
is not in this build."

## Stream a scene and bake it

1. Turn the feed on in `Regia.toml` with a `[dcc_feed]` section (`enabled = true` and a `listen`
   list; the agent page lists every key). An absent section means off.
2. Open the tab and check **Listening**. The addresses the editor listens on appear under the
   header. If `allow_remote` is on, a warning says the protocol has no TLS: use an SSH tunnel or a
   VPN.
3. Connect the content tool. Over a network transport it needs the token: tick **Show token** to
   read it, or **Rotate token** to replace it.
4. A client whose name is not in `auto_accept` appears under **Sessions** as pending. Press
   **Accept** (tick **Remember this client** first to save its name for next time) or **Reject**. An
   active session has **Disconnect**.
5. Items appear in the **Items** grid: key, kind, state (declared, active, stalled, detached,
   baked or removed), frames applied, and seconds since the last frame. In the Outliner a feed-owned
   entity carries a **FEED** badge (**FEED stalled** or **FEED detached** when its feed stopped).
6. To bake, type a folder under `assets/` in the field beside **Items**. Optionally set **Scale**
   (0 keeps the scale the client sent), **LODs** (0 generates none) and **Collision**. Press **Bake
   all**, or **Bake...** on one item. Skeleton and generic items have no **Bake...** button. Only
   one bake runs at a time. The panel then shows a "Last bake" line with the job's state and its file count.
7. To capture a session, type a path in the first field of the record row (the hint reads
   `take.rfdrec`) and press **Record**, then **Stop**. To play a take back, type its path in the
   second field and press **Replay**; a `done / total` counter shows progress.

| Action | Control / key | Agent method |
|---|---|---|
| Start or stop listening | **Listening** | `dcc_feed.listen` |
| Read status, sessions, items | the panel | `dcc_feed.status`, `dcc_feed.items` |
| Replace the token | **Rotate token** | `dcc_feed.rotate_token` |
| Accept a pending client | **Accept** | `dcc_feed.accept` |
| Reject or disconnect a session | **Reject**, **Disconnect** | `dcc_feed.disconnect` |
| Bake items to assets | **Bake all**, **Bake...** | `dcc_feed.bake` (`dry_run` plans without writing) |
| Read a bake | the "Last bake" line | `dcc_feed.bake_status` |
| Record a session | **Record**, **Stop** | `dcc_feed.record` |
| Replay a recording | **Replay** | `dcc_feed.replay` |

## Keyboard

The tab has no shortcuts. Its boxes, fields and buttons take focus with Tab.

## Known limits

- A bake replaces the live feed entities with ordinary content that uses the new assets (the
  panel does not offer the agent method's `replace_live: false`).
- Windows has no named pipe yet: use loopback TCP or WebSocket with the token.
- Morph targets, curves, USD and two-way sync are not delivered yet.

See also: [Content browser](content-browser.md), [Outliner](outliner.md).
