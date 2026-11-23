# Server world persistence

A dedicated server that keeps its world across restarts: persistent co-op and survival servers (ADR-3354). The
server saves the world through the save framework on a tick schedule, on a player leaving, before a level change and on
a graceful shutdown; on start it restores the newest save that decodes before any client gets in; and every player
gets their own kept state back when they join again. Guide topic `net-server-persistence`. The host itself is on
[net dedicated server](net-dedicated-server.md); the save flow and its slots on [save flow](save-flow.md).

## Turning it on

`server.toml` (format_version 4; `[persistence]` arrived in version 2, so a version 1 file has no such section and
persistence is off; `regia upgrade-project` rewrites older files to version 4):

```toml
format_version = 4

[persistence]
enabled = true                    # keep the world across restarts
dir = "saves"                     # relative to the server directory; one folder per level
autosave_ticks = 18000            # host ticks between autosaves (18000 = 5 min at 60 Hz); 0 = off
keep = 5                          # saves kept per level
save_on_shutdown = true           # after the drain of a graceful shutdown
save_on_level_change = true       # the level being left, before a travel
player_identity = "platform"      # or "platform_or_name"
```

| Key | Default | Valid | Reload |
|---|---|---|---|
| `persistence.enabled` | `false` | bool | restart |
| `persistence.dir` | `"saves"` | a non-empty path inside the server directory | restart |
| `persistence.autosave_ticks` | 18000 | host ticks, 0 = no autosave | restart |
| `persistence.keep` | 5 | 1..=1000 | restart |
| `persistence.save_on_shutdown` | `true` | bool | restart |
| `persistence.save_on_level_change` | `true` | bool | restart |
| `persistence.player_identity` | `"platform"` | `"platform"` or `"platform_or_name"` | restart |

The game adds `regia_framework::server_persistence::ServerPersistencePlugin` to its server app (`regia::run` in a
`server` build adds it already). What the world save holds is what the save framework saves: entities marked
`Saveable`, resources registered with `register_saveable_resource`, and save sections. Mark the entities that make up
the world (built structures, containers, placed items, the day counter) and nothing else.

## When it saves

- **Autosave** every `autosave_ticks` host ticks. The count is the host's tick count (the tick the host runs, idle
  slowdown ticks included), never the wall clock: after `n` ticks there were `n / autosave_ticks` autosaves.
- **A player left**: at the end of the tick in which one or more players left (one save for all of them), so a
  player's state is on disk when they are gone. A tick that autosaves anyway does not save twice.
- **Before a level change** (`save_on_level_change`): the level being left goes into its own folder.
- **Graceful shutdown** (`save_on_shutdown`): after the drain, when the players are gone (their leave hooks ran).
- **On demand**: console `save`, agent `server.save_now`.

A crash, `kill -9` or a power cut saves nothing: the world comes back as of the last save.

Each save is `<server dir>/<dir>/<level folder>/world-<sequence>.save`, in the save framework's container (magic,
version, payload, CRC-32). The level folder is the level path made file-name safe plus a hash of the exact path. The
sequence counts up across every level's folder and across restarts. A save is written to a temp file of its own,
fsynced, renamed over the target and the directory fsynced, so a crash mid-write leaves the previous saves intact.
Then the oldest saves beyond `keep` in that folder are removed.

## When it restores

- **Start**: after the first level loaded and before any session update, so no client is in yet. The newest save of
  that level's folder that decodes is applied (`Saveable` entities replaced, resources and sections restored).
- **Level change**: after the new level loaded, its folder's newest good save.

A save that fails to decode (truncated, torn, bad checksum, unreadable, too large) is renamed `*.save.corrupt`, warning
`persistence_save_corrupt` names it, and the next older save is tried. Set-aside files are never removed by the
rotation and their numbers are never reused. When the folder holds saves and **none** decodes, the start fails with
exit code 4 and `persistence_no_good_save`, and the files are left as they are: a supervisor restarting the server does
not silently start a fresh world over a world it could not read. Move the folder away (or restore a backup) to start
fresh. After a level change (not a start) the same case sets the files aside and the level starts fresh.

## Players

<!-- ignore-reason: a fragment of a server's App setup; Inventory is the game's own component type -->
```rust,ignore
app.add_plugins(ServerPersistencePlugin)
    .register_player_persistent::<Inventory>()
    .register_player_persistent::<Transform>();
```

A component registered with `register_player_persistent` is kept per player when it sits on the player's `NetPlayer`
entity or on the pawn the game rules spawned for that player (`NetRulesJoined::pawn`). It is captured when the player
leaves and at every save (players in the game too), and written back once when that player joins again (a travel or a
reconnect resume does not restore twice), after the rules' join hook spawned the pawn.

The key is the session's stable player identity (spec 77 § 4): the authenticator's platform id (`steam:<id>`).
Split-screen players of one account are `<id>#<index>` from index 1. With `player_identity = "platform_or_name"` a
player without a platform id is `name:<name>`; anyone can join under any name, so use it only on password-protected or
LAN servers. Under `"platform"` a player without a platform id keeps nothing (`players_unidentified` counts them).

The records travel inside the world save as the section `regia_framework::server_persistence::players`
([`docs/formats/server-players.md`](../../docs/formats/server-players.md), format `server-players` v1), so a world and
its players are one atomic file. Each record carries the sequence of the save it went into. At start the records of
every other level's newest good save are merged in too, and the newest record per player wins, so a player who last
played on another level of the rotation keeps their state.

`ServerPersistence::player_record(identity)` gives a game the kept record (for state it applies itself, such as a
pawn it spawns later).

## Operator console

| Command | What it does |
|---|---|
| `save` | Saves now; prints the file, size, entities, players and tick. |
| `persistence` | Enabled, level, tick, next autosave tick, next slot, saves by reason, players kept, the level's slots and warnings. |
| `restore <slot> confirm` | Rolls the world back to slot `<slot>` of the current level. Without `confirm` it prints `confirmation_required`. |

`help` lists them after the built-in commands. Games add their own commands the same way
(`regia_net::server_host::HostGameHooks::add_command`).

## Agent methods

| Method | Params | Result |
|---|---|---|
| `server.persistence_status` | none | `{ enabled, settings, root, level, tick, next_autosave_tick, next_sequence, counts: { autosave, shutdown, level_change, player_left, manual, failed }, last_save, last_restore, saves: [{ sequence, path, bytes }], corrupt: [path], players_kept, players_restored, players_unidentified, warnings: [{ code, message }] }` |
| `server.save_now` | none | `{ reason, sequence, level, tick, path, bytes, entities, players, removed }` |
| `server.restore` | `slot` (integer, required), `confirm: true` (required) | `{ restored: true, level, sequence, path, saved_tick, entities, fell_back, players }` |

`server.restore` replaces the world and every kept player record with the slot's; players in the game take their
saved state. The world is untouched when the slot does not decode.

Errors are `F0002` with `data.reason`:

| Code | Meaning |
|---|---|
| `server_persistence_missing` | The app has no `ServerPersistencePlugin`. |
| `persistence_disabled` | `[persistence] enabled = false`, or the host has not started. |
| `persistence_write_failed` | The save could not be written (disk full, permissions); the previous saves are untouched. |
| `persistence_encode_failed` | The save framework could not encode the world. |
| `persistence_slot_unknown` | No save with that sequence in the current level's folder. |
| `persistence_save_corrupt` | The slot does not decode; also the warning code of a start that fell back past a bad save. |
| `confirmation_required` | `server.restore` without `confirm: true`. |

Warning codes in `server.persistence_status`: `persistence_save_corrupt`, `persistence_no_good_save`,
`persistence_write_failed`, `persistence_rotation_failed`, `persistence_player_section_unreadable`.

## Verify it

1. Start the server with `--agent`, call `server.persistence_status`: `enabled: true`, `last_restore.sequence: null` on
   a fresh folder.
2. `wait_until` `tick >= autosave_ticks`, then `expect` `counts.autosave == 1` and `saves[0].sequence == 1`.
3. Stop the process (no shutdown) and start it again: `last_restore.sequence == 1`, `last_restore.saved_tick ==
   autosave_ticks`.
4. Truncate the newest save and restart: `last_restore.fell_back` names a `*.save.corrupt` file and `warnings` has
   `persistence_save_corrupt`.

`crates/regia_framework/tests/server_persistence.rs` runs all of this in process (test clock, local transport,
simulated clients).

## How other systems plug in

- **Streamed cells.** Cell changes kept by world streaming (spec 78 § 10.6) travel as their own save section; the
  world save carries every registered section, so a server with streamed levels keeps them with no extra work.
- **Entity references.** Components that hold `Entity` fields need the save framework's entity remapping before they
  are marked `Saveable` or registered as player-persistent.

## Limits

- Restore replaces `Saveable` entities: entities the level spawned that are not `Saveable` stay as the level made
  them.
- Player state is read from the player entity and the pawn named at join; state on other owned entities is the game's
  to mirror onto one of those, or to read from `player_record`.
- One world per level folder; two server processes must not share a `[persistence] dir`.
