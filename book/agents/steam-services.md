# Steam leaderboards, ownership and timeline (agents)

Leaderboards, app and DLC ownership, and game-recording timeline markers (spec 24 § 2a, ADR-2340). The
methods are registered by `regia_steam::SteamPlugin` on the `AgentExtension`; the leaderboard methods also
need the framework's `Leaderboard` resource, which `regia_steam::SteamLeaderboardPlugin` adds.

## Leaderboards

The game and the agent drive the same object: `regia_framework::leaderboard::Leaderboard`. A game never touches
the SDK:

```rust,ignore
app.add_plugins((SteamPlugin::new(AppId(480)), SteamLeaderboardPlugin));

fn finish_lap(mut boards: ResMut<Leaderboard>, lap: Res<LapTimer>) {
    boards.declare(LeaderboardSpec::new("fastest_lap").ascending().display(LeaderboardDisplay::TimeMilliseconds)).ok();
    boards.submit("fastest_lap", lap.millis, &[]).ok();
    boards.request("fastest_lap", LeaderboardRange::AroundUser { before: 4, after: 5 }).ok();
}
// Results: LeaderboardMessage::{Found, Missing, Uploaded, Entries, UgcAttached, Failed}, and boards.entries("fastest_lap").
```

| Service | `service` | Where |
|---|---|---|
| Steam (ISteamUserStats) | `steam` | `SteamLeaderboardPlugin` attaches it once Steam is ready, and detaches it when Steam goes away. `MockSteam` backs it with an in-memory store in tests. |
| Local (in memory) | `local` | `LeaderboardPlugin::local()`: offline play, dev, tests. Deterministic. |
| None | `none` | The default of `LeaderboardPlugin`: every operation fails with `leaderboard_unavailable`. |

Every call is queued. A board is found by name once (find-only by default; `create: true` uses Steam's
find-or-create). Answers come back on later frames: step a frame or `wait_until` on `steam.leaderboard_entries`.

| Method | Params | Result |
|---|---|---|
| `steam.leaderboard_find` | `name`, `create?`, `sort?`, `display?`, `method?` | `{ service, board }` |
| `steam.leaderboard_upload` | `name`, `score`, `details?`, `method?` | `{ service, board }` |
| `steam.leaderboard_download` | `name`, `range?`, `start?`, `end?`, `before?`, `after?` | `{ service, board, range }` |
| `steam.leaderboard_attach_ugc` | `name`, `ugc` | `{ service, board }` |
| `steam.leaderboard_entries` | `name` | `{ service, local_user, board, range, entries: [{ user, rank, score, details, ugc }] }` |
| `steam.leaderboard_list` | — | `{ plugin, service, pending, boards: [board] }` |

- `board` is `{ name, lookup, handle, sort, display, method, create, entry_count, pending, last_upload, attached_ugc, error }`.
  `lookup` is `not_started`, `pending`, `found`, `missing` (no such board and `create` was false) or `failed`.
  `error` is the last failure on the board, `{ code, message }`, with one of the codes below.
- `sort`: `ascending` (lowest first: lap times) or `descending` (highest first, the default). `display`: `numeric`
  (default), `time_seconds` or `time_milliseconds`. Both apply only when the board is created; an existing board
  keeps its own, and `board.sort`/`board.display` show the board's real values once found.
- `method`: `keep_best` (default; a worse score leaves the entry alone and `last_upload.changed` is false) or
  `force_update` (always replaces). `find` sets the board's default; `upload` may override it per call.
- `last_upload` is `{ score, changed, rank_new, rank_previous }`; `rank_previous` is `null` when the user had no
  entry.
- `range`: `global` (ranks `start..=end`, 1-based; defaults 1 and `start + 9`), `around_user` (`before` rows above
  and `after` below the local user; defaults 5 and 5; no rows when the user has no entry) or `friends` (the user
  and their friends, with global ranks). At most 1000 rows per download.
- `details` are up to 64 game-defined `i32` values per entry. `user`, `ugc`, `handle` and `local_user` are 64-bit
  ids written as decimal strings; `ugc` is read from a string or an integer.
- `steam.leaderboard_attach_ugc` attaches a shared cloud file's UGC handle to the local user's entry (the entry must
  exist: `no_entry`). With the real client this answers `ugc_unsupported`: the `steamworks` 0.13 bindings do not
  expose the call, and downloaded rows carry `ugc: null` (ADR-2340). The mock and the local service support it.

## Ownership

| Method | Params | Result |
|---|---|---|
| `steam.owns_app` | `app_id` | `{ app_id, subscribed, installed, dlc_installed, install_dir }` |

`subscribed` is ownership (`BIsSubscribedApp`: a related game or a demo), `dlc_installed` is "owns this game's DLC
and has it installed" (`BIsDlcInstalled`), `installed` is `BIsAppInstalled` (not proof of ownership), and
`install_dir` is where the app is or would be installed (`null` when Steam returns nothing). Games call
`Steam::owns_app`, `Steam::dlc_installed`, `Steam::app_installed`, `Steam::app_install_dir` or
`Steam::app_ownership`.

## Timeline markers

Steam's game recording keeps a timeline; the game colors it by mode, describes the current state and marks events
the player may want to clip.

| Method | Params | Result |
|---|---|---|
| `steam.timeline_mode` | `mode` | `{ mode }` |
| `steam.timeline_state` | `description?`, `delta?` | `{ state }` |
| `steam.timeline_event` | `title`, `icon?`, `description?`, `priority?`, `start_offset?`, `duration?`, `clip?` | `{ events }` |
| `steam.timeline` | — | `{ plugin, running, mode, state, events, last_event, refused }` |

- `mode`: `playing`, `staging`, `menus` or `loading_screen`.
- `steam.timeline_state` sets the state text from `delta` seconds from now (0..=3600, default 0); an absent or
  empty `description` clears it.
- `steam.timeline_event`: `title` and `icon` are 1..=255 bytes (`icon` defaults to `steam_marker`; Steam lists its
  built-in icon names in the Steamworks timeline documentation), `description` 0..=1024 bytes, `priority` 0..=1000,
  `start_offset` -3600..=3600 seconds (negative marks the past), `duration` 0..=3600 seconds (0 is an instant),
  `clip` `none`, `standard` (default) or `featured`.
- Steam has no read back: `steam.timeline` reports what the engine last sent (`events` is a count, `refused` counts
  calls made while Steam was unavailable).

## Error codes

Errors are `F0002` with `data.reason`:

| Code | When |
|---|---|
| `bad_value` | A parameter is missing, has the wrong type, or names an unknown `sort`, `display`, `method`, `range`, `mode` or `clip`. |
| `leaderboard_not_running` | The app has no `Leaderboard` resource (add `SteamLeaderboardPlugin`). |
| `leaderboard_unavailable` | No leaderboard service (Steam not running); recorded on the board. |
| `name_invalid` | A board name is empty, longer than 128 bytes or holds a NUL. |
| `leaderboard_not_found` | `steam.leaderboard_entries` on an undeclared board; on a board, Steam has no board of that name. |
| `details_too_long` | More than 64 detail values. |
| `range_invalid` | `start` is 0, `end < start`, or more than 1000 rows. |
| `no_entry` | Attaching UGC while the local user has no entry (recorded on the board). |
| `ugc_unsupported` | The service cannot attach UGC (the real client, see above; recorded on the board). |
| `leaderboard_platform` | Steam reported a failure (network, rate limit); recorded on the board. |
| `steam_unavailable` | Steam is not running, or the app has no `SteamPlugin` (ownership and timeline methods). |
| `steam_rejected` | A timeline value is out of its limits. |
| `app_id_invalid` | `app_id` is 0. |

## Example

Against the mock (the integration test `crates/regia_steam/tests/steam_services.rs` runs this) or a real Steam
client (`regia agent call <method> '<params json>'`):

```text
steam.leaderboard_find     {"name":"replays","create":true,"method":"force_update"}
steam.leaderboard_upload   {"name":"replays","score":4200,"details":[1,2,3]}
steam.leaderboard_download {"name":"replays","range":"global","start":1,"end":10}
wait_until steam.leaderboard_entries {"name":"replays"} board.pending == 0
  -> entries: [{ user: "<your id>", rank: 1, score: 4200, details: [1,2,3], ugc: null }]
steam.owns_app             {"app_id":480}
  -> { subscribed: true, installed: true, dlc_installed: false, install_dir: ".../Spacewar" }
steam.timeline_mode        {"mode":"playing"}
steam.timeline_event       {"title":"Boss down","icon":"steam_achievement","clip":"featured","start_offset":-10,"duration":10}
steam.timeline             {}
  -> { mode: "playing", events: 1, last_event: { title: "Boss down", ... } }
```

The crate example runs the game side on the mock with no SDK: `cargo run -p regia_steam --example steam_leaderboard`.

A real-client run needs the owner's Steam login and an app id with a leaderboard; the release notes of v0.1.28
(steam-services) list the exact command.

<!-- depth-2:method-reference -->

These calls were sent to a headless editor, which does not register the steam methods, so each answer is unknown method (`F0008`). A process that registers them and has no Steam resource answers success with `plugin` false instead.


## Call reference

Parameters, results and error codes for every method on this page. Types and defaults come from the schema and handlers; limits include the contract. Error codes are in `data.code`; names explicitly labeled `data.reason` are additional handler reasons. Every method can also return `F0001`, `F0002`, `F0004`, `F0007` and `F0012`. The server can return `F0005`, `F0006`, `request_too_large`, or `F0008` when the method is not registered; see [protocol](protocol.md).

Unless a lower or explicit method limit is listed, the dispatcher accepts at most 65,536 items in a top-level array parameter and 1,048,576 UTF-8 bytes in a string parameter. A limit violation returns `F0002`.

The request in each fence is the call that was sent with `regia agent call`.
The line under it is the host's answer.
A second fence tagged `json,ignore` is an error answer. Large fields are elided with `...`. A fence tagged `json,ignore` is a real call on a host or plugin method that the runtime schema does not list.


### `steam.leaderboard_attach_ugc`

Queue attaching a shared UGC file handle to the local user's entry (the entry must exist).

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `name` | string | yes | the board name (1..=128 bytes) |
| `ugc` | string | yes | the 64-bit UGC handle, decimal string or integer |

Result: `{ service, board }`.

Errors besides the universal codes: `F0002`. `data.reason`: `bad_value`, `leaderboard_not_running`, `leaderboard_unavailable`, `name_invalid`, `leaderboard_not_found`, `no_entry`, `ugc_unsupported`, `leaderboard_platform`.

```json,ignore
steam.leaderboard_attach_ugc {"name": "example", "ugc": "example"}
→ { "message": "unknown method steam.leaderboard_attach_ugc", "code": -32601, "data": { "code": "F0008", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 19099 chars).

<!-- capture:steam.leaderboard_attach_ugc -->

### `steam.leaderboard_download`

Queue a download of global ranks, rows around the local user, or friends; read rows with steam.leaderboard_entries.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `name` | string | yes | the board name (1..=128 bytes) |
| `range` | string | no | global (default), around_user or friends |
| `start` | integer | no | global: first rank, 1-based (default 1) |
| `end` | integer | no | global: last rank (default start + 9) |
| `before` | integer | no | around_user: rows above the user (default 5) |
| `after` | integer | no | around_user: rows below the user (default 5) |

Result: `{ service, board, range }`.

Errors besides the universal codes: `F0002`. `data.reason`: `bad_value`, `leaderboard_not_running`, `leaderboard_unavailable`, `name_invalid`, `leaderboard_not_found`, `range_invalid`, `leaderboard_platform`.

```json,ignore
steam.leaderboard_download {"name": "example"}
→ { "message": "unknown method steam.leaderboard_download", "code": -32601, "data": { "code": "F0008", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 19099 chars).

<!-- capture:steam.leaderboard_download -->

### `steam.leaderboard_entries`

One board's state: lookup, handle, last upload outcome, last downloaded rows, pending work, last error.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `name` | string | yes | the board name (1..=128 bytes) |

Result: `{ service, local_user, board, range, entries: [{ user, rank, score, details, ugc }] }`.

Errors besides the universal codes: `F0002`. `data.reason`: `bad_value`, `leaderboard_not_running`, `leaderboard_not_found`.

```json,ignore
steam.leaderboard_entries {"name": "example"}
→ { "message": "unknown method steam.leaderboard_entries", "code": -32601, "data": { "code": "F0008", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 19099 chars).

<!-- capture:steam.leaderboard_entries -->

### `steam.leaderboard_find`

Declare a leaderboard and look it up (create it when `create`); answers arrive on later frames.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `name` | string | yes | the board name (1..=128 bytes) |
| `create` | boolean | no | create the board when missing (default false) |
| `sort` | string | no | ascending or descending (default) when created |
| `display` | string | no | numeric (default), time_seconds or time_milliseconds when created |
| `method` | string | no | keep_best (default) or force_update for later uploads |

Result: `{ service, board }`.

Errors besides the universal codes: `F0002`. `data.reason`: `bad_value`, `leaderboard_not_running`, `leaderboard_unavailable`, `name_invalid`, `leaderboard_not_found`, `leaderboard_platform`.

```json,ignore
steam.leaderboard_find {"name": "example"}
→ { "message": "unknown method steam.leaderboard_find", "code": -32601, "data": { "code": "F0008", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 19099 chars).

<!-- capture:steam.leaderboard_find -->

### `steam.leaderboard_list`

Every declared leaderboard with its lookup state and pending work.

*Read, does not mutate.*

No parameters.

Result: `{ plugin, service, pending, boards: [{ name, lookup, handle, sort, display, method, entry_count, pending, error }] }`.

Errors besides the universal codes: `F0002`.

No `Leaderboard` resource: success with `plugin` false.

```json,ignore
steam.leaderboard_list {}
→ { "message": "unknown method steam.leaderboard_list", "code": -32601, "data": { "code": "F0008", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 19099 chars).

<!-- capture:steam.leaderboard_list -->

### `steam.leaderboard_upload`

Queue the local user's score upload (keep_best or force_update); read the outcome with steam.leaderboard_entries.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `name` | string | yes | the board name (1..=128 bytes) |
| `score` | integer | yes | the score (i32) |
| `details` | array | no | up to 64 game-defined i32 values |
| `method` | string | no | keep_best or force_update (default: the board's declared method) |

Omitted details sends an empty array; supplied details contain at most 64 signed 32-bit integers.

Result: `{ service, board }`.

Errors besides the universal codes: `F0002`. `data.reason`: `bad_value`, `leaderboard_not_running`, `leaderboard_unavailable`, `name_invalid`, `leaderboard_not_found`, `details_too_long`, `leaderboard_platform`.

```json,ignore
steam.leaderboard_upload {"name": "example", "score": 1}
→ { "message": "unknown method steam.leaderboard_upload", "code": -32601, "data": { "code": "F0008", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 19099 chars).

<!-- capture:steam.leaderboard_upload -->

### `steam.owns_app`

App or DLC ownership for the signed-in user: subscribed, installed, dlc_installed, install_dir.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `app_id` | integer | yes | the Steam app id of the game, demo or DLC |

Result: `{ app_id, subscribed, installed, dlc_installed, install_dir }`.

Errors besides the universal codes: `F0002`. `data.reason`: `bad_value`, `app_id_invalid`, `steam_unavailable`, `steam_rejected`.

```json,ignore
steam.owns_app {"app_id": 1}
→ { "message": "unknown method steam.owns_app", "code": -32601, "data": { "code": "F0008", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 19099 chars).

<!-- capture:steam.owns_app -->

### `steam.timeline`

What the engine last sent to the game-recording timeline (Steam has no read back).

*Read, does not mutate.*

No parameters.

Result: `{ plugin, running, mode, state, events, last_event, refused }`.

No error codes beyond the universal set.

No Steam plugin: success with `plugin` false. `events` counts markers the engine sent; `refused` counts calls made while Steam was unavailable.

```json,ignore
steam.timeline {}
→ { "message": "unknown method steam.timeline", "code": -32601, "data": { "code": "F0008", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 19099 chars).

<!-- capture:steam.timeline -->

### `steam.timeline_event`

Mark an instant or ranged event on the game-recording timeline.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `title` | string | yes | 1..=255 bytes |
| `icon` | string | no | Steam icon name (default steam_marker) |
| `description` | string | no | 0..=1024 bytes |
| `priority` | integer | no | 0..=1000 (default 0) |
| `start_offset` | number | no | seconds from now, -3600..=3600; negative marks the past (default 0) |
| `duration` | number | no | seconds, 0..=3600; 0 is an instant (default 0) |
| `clip` | string | no | none, standard (default) or featured |

Omitted description is empty; icon defaults to steam_marker and priority to 0.

Result: `{ events }`.

Errors besides the universal codes: `F0002`. `data.reason`: `bad_value`, `steam_unavailable`, `steam_rejected`.

```json,ignore
steam.timeline_event {"title": "example"}
→ { "message": "unknown method steam.timeline_event", "code": -32601, "data": { "code": "F0008", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 19099 chars).

<!-- capture:steam.timeline_event -->

### `steam.timeline_mode`

Set the game-recording timeline's game mode (bar color).

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `mode` | string | yes | playing, staging, menus or loading_screen |

Result: `{ mode }`.

Errors besides the universal codes: `F0002`. `data.reason`: `bad_value`, `steam_unavailable`, `steam_rejected`.

```json,ignore
steam.timeline_mode {"mode": "example"}
→ { "message": "unknown method steam.timeline_mode", "code": -32601, "data": { "code": "F0008", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 19099 chars).

<!-- capture:steam.timeline_mode -->

### `steam.timeline_state`

Describe the game state on the timeline from `delta` seconds from now; no description clears it.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `description` | string | no | 1..=1024 bytes; absent or empty clears the state |
| `delta` | number | no | seconds from now, 0..=3600 (default 0) |

Result: `{ state }`.

Errors besides the universal codes: `F0002`. `data.reason`: `bad_value`, `steam_unavailable`, `steam_rejected`.

```json,ignore
steam.timeline_state {}
→ { "message": "unknown method steam.timeline_state", "code": -32601, "data": { "code": "F0008", "hint": ... } }
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.
Elided: `data.hint` (string 19099 chars).

<!-- capture:steam.timeline_state -->

## Workflow: post a score and mark a moment on the timeline

1. `steam.leaderboard_list`. `plugin` false means there is no `Leaderboard` resource.
2. `steam.leaderboard_find` with the board name and create true when the game may create it. Poll `steam.leaderboard_list` until the board lookup completes; inspect its error before uploading. `name_invalid` means an empty name, more than 128 bytes, or a NUL.
3. `steam.leaderboard_upload` with score and up to 64 details, then poll the board in `steam.leaderboard_list` until its pending work completes.
4. `steam.leaderboard_download` with global range, start and end (at most 1000 rows; start is 1-based). Poll pending in `steam.leaderboard_list`, then read `steam.leaderboard_entries`; entries returns the downloaded rows and does not issue a fresh download.
5. `steam.timeline_mode` `playing`, `steam.timeline_event` for the moment, and `steam.timeline` to read what the engine last sent. `steam_unavailable` means Steam is not running.

<!-- /depth-2:method-reference -->
