# Steam Workshop (agents) (`steam.workshop_status`, `steam.workshop_upload`, `steam.workshop_subscribe`, `steam.workshop_unsubscribe`)

`regia_steam::SteamWorkshopPlugin` (ADR-2418, spec 24 § 2) uploads mods to the Steam Workshop, subscribes to items,
and mounts every installed item at [`mods://`](mods.md) as mod `workshop.<item id>` at the end of the load order. An
item update is installed again and remounted in the same place; an unsubscribed item is unmounted. A folder holding
nothing but one `.fpak` mounts as that pak. Items Steam installed in an earlier session mount on the first frame.
Workshop items keep a fixed order among themselves: a newly installed item goes before the first mounted Workshop item
with a larger id, so the same subscriptions give the same load order (and hash) whatever order Steam installed them
in; the game's own mods and any order it set stay as they are. An update that cannot be mounted leaves the item
unmounted (its old files are gone), with the reason in `items[].error`. The real service rescans the subscribed items
every 120 frames: an update Steam installed, or an item subscribed or unsubscribed through the overlay, is picked up
there, and a missing download is requested once. Every 16th scan (1,920 frames) it also re-reads each known install's
timestamp, so an update that started and finished between two scans is still picked up. Mounting walks and hashes the item's files on the frame the install
lands (one time per install, not per frame). Mounted items count in the `mods://` content-set hash, and the plugin keeps the lobby key `mods.content_set` of a
lobby this player owns equal to it, so players only meet lobbies with the same mods in the same order.

Add it after `SteamPlugin`:

<!-- ignore-reason: needs the `steam` feature, and `mock` is the test's `MockWorkshop` -->
```rust,ignore
app.add_plugins(SteamPlugin::new(AppId(480)));
app.add_plugins(SteamWorkshopPlugin::default());                // the real Workshop (feature `steamworks`)
app.add_plugins(SteamWorkshopPlugin::with_service(mock));       // or a MockWorkshop in tests
```

The service is `steam` (ISteamUGC, with the `steamworks` feature, once Steam is ready), `mock`
(`regia_steam::MockWorkshop`: an in-memory Workshop that installs items into a folder of its own) or `unavailable`.
`manual_mount()` reports installed items without mounting them; `without_lobby_sync()` leaves the lobby key to the
game.

## Methods

| Method | Params | Result |
|---|---|---|
| `steam.workshop_status` | — | `{ plugin, service, auto_mount, agent_upload, items: [{ item, subscribed, installed, downloading, needs_update, folder, mod_id, mounted, installs, error }], uploads: [{ request, state, item?, needs_legal_agreement?, code?, message? }] }` |
| `steam.workshop_upload` | `title`, `content_dir`, `item`?, `description`?, `preview`?, `tags`?, `visibility`?, `change_note`?, `metadata`? | `{ plugin, request, service }` |
| `steam.workshop_subscribe` | `item` | `{ plugin, item, mod_id }` |
| `steam.workshop_unsubscribe` | `item` | `{ plugin, item }` |

Item ids are decimal strings (an integer is accepted too). Calls do not wait for Steam: step a frame (or
`wait_until`), then read `steam.workshop_status` and `mods.list`. Without the plugin every method answers
`{ "plugin": false }`, but only after its params pass: a missing or malformed param, or an upload path outside the
allowed roots (`path_outside_project`, below), is still an error.

**Uploads publish outside the machine.** `steam.workshop_upload` needs the `tools` permission (not in the default
profile; a project grants it in `.regia/agent.toml`), and the real Workshop also needs the game's opt-in,
`SteamWorkshopPlugin::allow_agent_upload()`; the mock always takes uploads. Before anything is sent the content folder
is mounted like a `mods://` mod (no links, valid names, not empty). `content_dir` and `preview` are confined: they must
resolve, links followed, under the project root, the system temp dir or an explicit output root (else `F0002` with
`data.reason` `path_outside_project`). Unless `metadata` is given, the item metadata is
`regia.mods.content=<content hash>`: the hash a subscriber's mount computes for the same files. `visibility` is
`public`, `friends_only`, `private` (the default) or `unlisted`. Limits: title 1 to 128 bytes, description 8,000,
metadata 5,000, at most 32 tags of 1 to 255 bytes without commas. A new item whose `needs_legal_agreement` is true stays
hidden until the uploader accepts the Workshop legal agreement on Steam.

```text
steam.workshop_subscribe  {"item":"3301234567"}
(step a frame)
steam.workshop_status     {}
→ { "plugin": true, "service": "steam", "items": [{ "item": "3301234567", "subscribed": true, "installed": true,
    "mod_id": "workshop.3301234567", "mounted": true, "installs": 1, ... }], ... }
mods.read                 {"path":"mods://data/units.ron"}
```

## Error codes

Refusals carry the code in `data.code`; a bad parameter is `F0002`.

| Code | Meaning |
|---|---|
| `workshop_unavailable` | No Workshop: Steam is not running, the `steamworks` feature is off, or no service is attached. |
| `workshop_invalid` | A parameter broke a limit (title, description, tags, metadata, a missing preview or folder, an empty folder). |
| `workshop_content` | The content folder breaks a `mods://` rule (a link, a bad name, too large). |
| `workshop_unknown_item` | The item is not on the Workshop, or (unsubscribe) not subscribed. |
| `workshop_platform` | Steam refused or failed the call (the message carries Steam's result). |
| `workshop_upload_not_allowed` | The game did not add `SteamWorkshopPlugin::allow_agent_upload()`. |

A failed upload or subscribe that Steam reports later is not an error of the call: it shows in
`steam.workshop_status` (`uploads[].state: "failed"` with its code, `items[].error`).
Unsubscribe of an item that is not subscribed is a refusal on the call (`workshop_unknown_item`),
on the mock and on the real client. The real client reads the subscription bit before it calls
Steam. A real-client unsubscribe that Steam's callback then refuses is also not an error of the
call: `items[].error` is text beginning `unsubscribe failed: Steam:` (the `workshop_platform` message). Upload rows
are the status entries that carry `code`.

## The real Workshop

The real path is compiled with `regia_steam`'s `steamworks` feature and is checked by hand with Steam running and
signed in (an app whose Workshop is enabled; `STEAM_APP_ID` picks it, 480 by default). The real-client test lists the
subscribed items always, uploads only when `REGIA_WORKSHOP_UPLOAD_DIR` is set (a new private item), and subscribes,
waits for the install and reads the mounted files only when `REGIA_WORKSHOP_ITEM` is set. The owner's run, from the
repository root:

```sh
# 1. upload a small folder as a private item; note the printed item id
STEAM_SDK_LOCATION=$HOME/sdk/steamworks REGIA_WORKSHOP_UPLOAD_DIR=/path/to/test-mod \
  cargo test -p regia_steam --features steamworks --test real_client -- --nocapture
# 2. from a second account (or the same one), subscribe and mount it
STEAM_SDK_LOCATION=$HOME/sdk/steamworks REGIA_WORKSHOP_ITEM=<item id> \
  cargo test -p regia_steam --features steamworks --test real_client -- --nocapture
```

In a game, the same through the agent: `SteamWorkshopPlugin::default().allow_agent_upload()` and the `tools`
permission, `steam.workshop_upload`, then `steam.workshop_subscribe` from a second account, `mods.list` shows
`workshop.<item>` and `mods.read` returns its files.
