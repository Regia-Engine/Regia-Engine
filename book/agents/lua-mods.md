# Lua mods (`lua.list`, `lua.run`, `lua.globals`, `lua.errors`, `lua.restart`, `lua.reload`, `lua.state`, `lua.watch`, `lua.entity_cap`, `lua.authority`)

A game can accept mods written in **Lua 5.1**: small scripts that react to game events, read and move named
entities, spawn their own entities and send events back to the game. Each mod runs in its own sandboxed Lua
state with an instruction budget per call and a memory cap. A mod that fails is disabled; the game and the other
mods keep running.

Lua is optional. It lives in `regia_script_lua` behind the `lua` feature of `regia` (off by default), which is
the one place the Lua C runtime is compiled in (ADR-1751, an exception to the Rust-only rule). Spec:
`docs/specs/84-lua-mods.md`.

## Turning it on

```toml
# the game's Cargo.toml
regia = { version = "*", features = ["lua"] }
```

```rust,ignore
use regia::lua::{LuaLimits, LuaModsPlugin, ModFiles};

let mods = vec![ModFiles::from_dir(std::path::Path::new("mods/lights"), 4 * 1024 * 1024)?];
app.add_plugins(LuaModsPlugin::new(mods).with_seed(level_seed).with_limits(LuaLimits::default()));
```

The plugin loads the mods at `Startup` (in id order), calls `on_update(dt)` every frame and `on_tick(dt)` every
fixed step, then delivers queued events. The game sends events with `LuaMods::fire(name, value)` and reads what
mods sent with `LuaMods::drain_emitted()`. On Android, read the files through the game's own asset reads and
pass `ModFiles::from_map`.

## A mod

A directory with `luamod.ron` and `.lua` files:

```ron
(
    format_version: 1,
    id: "example.lights",
    name: "Blinking lights",
    version: "1.0.0",
    entry: "main.lua",
    api_version: 1,
    instruction_budget: None,
    memory_limit_bytes: None,
)
```

```lua
-- main.lua
local lamp
function on_start()
  lamp = regia.spawn("blink_lamp", regia.vec3(0, 3, 0))
  regia.on("door_opened", function(v) regia.emit("lamp_toggled", { by = v.door }) end)
end
function on_tick(dt)
  if lamp:alive() then
    lamp:set_position(lamp:position() + regia.vec3(0, math.random() * 0.01, 0))
  end
end
```

`luamod.ron` is format `luamod`, version 1. Unknown fields are refused, as are a newer `format_version`, an
`api_version` other than 1, an id outside `[a-z0-9_.-]` (1 to 64 characters) and an entry outside the mod or not
ending in `.lua`. Version 1
is the first version, so there is no migrator yet.

## What a mod can call

| Lua | What it does |
|---|---|
| `on_start()`, `on_update(dt)`, `on_tick(dt)` | Optional globals the host calls. |
| `regia.on(name, fn)` | Subscribe `fn(value)` to a game event. |
| `regia.emit(name, value, target?)` | Send an event to the game (at most 256 per call). `target` (an entity handle) aims it at that entity: with the `regia` `logic` feature the event lands in the entity's logic-graph inbox ([Logic graphs](logic-graph.md#integrations-phase-3)) — only the *name* crosses; the `value` is dropped there by design (ADR-7268 addendum), so hand values to a graph via `LuaMods::call_function` / `LogicLuaUnrouted` from the game side. |
| `regia.log(...)`, `print(...)` | The mod's log ring and the engine log. |
| `regia.find(name)` | The entity with that `Name` (among several, the same one every time), or `nil`. |
| `regia.spawn(name, pos)` | Spawn a named entity (at most 64 per call); the mod owns it. |
| `e:alive()`, `e:name()`, `e:position()`, `e:set_position(v)`, `e:despawn()`, `e:id()` | Entity handle methods. `e:id()` is `"{index}v{generation}"` (the low 32 bits, `v`, the high 32 bits) and is not a liveness test (use `e:alive()`). Lua 5.1 numbers cannot hold every entity's bits, so the decimal bits stay the agent `entity` integer. A handle to a despawned entity, or to a slot a new entity reuses, answers `false`/`nil`. `despawn` works only on the mod's own entities. |
| `regia.vec3(x, y, z)` | A vector: `.x .y .z`, `+ - * /` (a number on either side of `*`), unary `-`, `==`, `:length()`, `:dot(v)`, `:cross(v)`, `:normalized()`. |
| `math.random`, `math.randomseed`, `regia.random` | Deterministic random numbers (below). |
| `require(name)` | A module from the mod's own files: `require("lib.util")` loads `lib/util.lua`. |
| `regia.state`, `regia.state_version` | The mod's saved state and the version of its shape (below). |
| `on_save()`, `on_load(info)`, `on_migrate(old, from, to)` | Optional globals for saves and hot reload (below). |
| `regia.authority`, `regia.command(name, value?)`, `regia.snapshot()`, `on_snapshot(s)` | The side the mod runs on (`"host"` or `"presentation"`), a request for the game (host only; `true` or `false, reason`), and the state the game published ([below](#host-and-presentation-runtimes-adr-8466)). |

Event values are plain data: `nil`, booleans, numbers, strings, `vec3`s, entity handles, and tables of them,
at most 16 levels deep, 4,096 items and 64 KiB per string. A function or coroutine in a value fails with
`lua_bad_value`.

## The sandbox

- Loaded: the base library, `string`, `table`, `math`, `coroutine`. Not loaded: `io`, `os`, `package`, `debug`.
- Removed: `dofile`, `loadfile`, `load`, `loadstring`, `module`, `gcinfo`, `newproxy`, `string.dump`;
  `collectgarbage` answers `"count"` only. Precompiled chunks are refused (`lua_syntax`).
- **Instruction budget**: 1,000,000 VM instructions per call by default, checked every 1,000 instructions.
  **Memory cap**: 16 MiB per mod by default. A manifest may ask for less, never more.
- `pcall`, `xpcall` and `coroutine.resume` cannot catch a budget or memory error: the call ends and the mod is
  disabled.
- Any error disables that mod (status `disabled`, with its code). `lua.restart` builds it again from its files.
- Host work counts. `string.find`, `match`, `gmatch` (also as `gfind`) and `gsub` run in a matcher that charges the
  instruction budget, so a hostile pattern fails with `lua_instruction_budget`; they answer like Lua's own, except that a
  pattern with more than 200 nested items is refused (`pattern too complex`). Every comparison of a `table.sort` counts,
  with or without a comparator, so one call sorts up to about 20,000 items at the default budget.
  World calls cost 16 instructions each, and `regia.find` more: 64, plus one per 8 entities in the world, plus one per
  named entity it walks. Find once and keep the handle; handles stay valid.
  One round of event delivery is one call: all of a mod's handlers share one budget, and an event takes at most 256
  handlers per mod. The same holds for script components: in one update, all of a mod's component `on_update` passes
  share one budget (the global `on_update` is its own call). A value sent with `regia.emit` holds at most
  4,096 items, strings of at most 64 KiB, and 1 MiB of strings in all (`MAX_VALUE_BYTES`); a call emits at most 1 MiB
  of values (`max_emit_bytes_per_call`).
- Events wait for `LuaMods::drain_emitted()`. A game that never drains them keeps the latest 65,536 events or 64 MiB,
  whichever is less: the oldest go first, with one warning, and `lua.list` counts them in `emitted_dropped`.
- Known limit: other C library calls and string concatenation run to their end between budget checks. Their cost
  is bounded by the memory cap, so a call's C work is at most the budget times the state's size.
- A one-second watchdog (`max_call_millis`, 0 turns it off) ends a call the instruction count cannot see, such as a
  loop concatenating megabytes. It reports `lua_instruction_budget`. It depends on time, but a mod that fits its budget
  never gets near it. In one round of event delivery it counts the mod's own handlers, so a slow mod cannot trip it for
  another.
- Mod ids starting with `agent.` are reserved. `name` and `version` are at most 256 bytes. `format_version` 0 and an
  `entry` that does not end in `.lua` are refused.

## Determinism

`math.random` is the engine's PCG32, seeded from the host seed (`with_seed`) with the mod id as the stream:
same seed, same mods, same events, same numbers on every platform. There is no clock. Mods run in id order;
event handlers in subscription order. Not deterministic: `tostring` of tables and functions (addresses),
`pairs` over tables keyed by tables, functions or userdata, and the last digit of `tostring(number)` across
C libraries. Gameplay that must replay belongs in `on_tick`.

## State in save games

Put what must survive a save in `regia.state`. The plugin adds it to every save game: `SaveGame::save`, the save flow,
the autosave and the agent's `save.*` methods all carry it, as one section of the file.

```lua
regia.state_version = 2
regia.state.score = 0              -- defaults; a load replaces what is in the table

local lamp

function on_start()                -- a new game
  lamp = regia.spawn("blink_lamp", regia.vec3(0, 3, 0))
end

function on_load(info)             -- after a load or a hot reload, instead of on_start
  lamp = regia.find("blink_lamp")  -- entities come back, handles do not
end

function on_migrate(old, from, to) -- a state from another version
  if from < 2 then old.score = old.points or 0; old.points = nil end
  -- return a new table, or nothing when you edited `old`
end

regia.on("door_opened", function(v) regia.state.score = regia.state.score + 1 end)
```

- **`regia.state` stays one table.** A load and a reload empty it and fill it again, so `local s = regia.state` at the
  top of a chunk is fine.
- **Plain data only**: numbers, booleans, strings, `vec3`s and tables of them, keyed by numbers, strings or booleans.
  Shared tables and cycles survive. A function, a coroutine, an entity handle or a table with a metatable is refused
  with `lua_bad_value` and its path, and that mod is left out of the save; the save goes on. Save an entity's name and
  find it in `on_load`.
- **Limits**: 1 MiB per mod, 262,144 tables plus entries per mod, 4,096 entities per mod, 8 MiB for all mods (the
  `LuaLimits` fields `max_state_bytes`, `max_state_items`, `max_state_entities`, `max_state_total_bytes`); tables nest
  at most 128 levels. Past a limit: `lua_too_large`. The entity limit is checked when the mod spawns, so a mod
  cannot grow past what a save keeps and lose its whole entry: `regia.spawn` past `max_state_entities` (default
  4,096) owned entities fails (catch it with `pcall`, or despawn first). At the default limits the
  [entity cap](#entity-cap-adr-2418) (also 4,096, checked first) refuses it with `lua_entity_cap`; `lua_too_large`
  comes only when a host sets `max_entities_per_mod` above `max_state_entities`.
- **Also saved**: the mod's random numbers, so a loaded game rolls what the original would have, and its entities, by
  name and position. Not saved: locals, closures, coroutines, subscriptions. Subscribe at the top of the chunk: it runs
  on every load. A component the game added to a mod's entity does not come back.
- **`on_load` is the opt-in.** A mod that defines it gets its entities and `on_load(info)`. A mod that does not
  restarts on a load: its entities go and `on_start` runs, so a mod written without saves in mind keeps working.
  With `on_load`, spawn in `on_start`, not at the top of the chunk: the chunk runs on every load.
- **`on_save` is optional.** It copies live data into `regia.state` before a save. If it fails, the mod is disabled like
  on any failed call, and its state saves as it stood. `last_save` on `lua.list` names the error.
- `info.reason` is `"save"` or `"reload"`; `info.state` is `"carried"`, `"migrated"` or `"reset"`; `info.from_version`
  and `info.to_version` are set when the versions differ.
- **Versions.** Change `regia.state_version` when the shape of `regia.state` changes, and write `on_migrate`. With no
  `on_migrate`, a state from another version is dropped and the mod keeps what its chunk set up (`reset`). A failing
  `on_migrate` during a load disables that mod and keeps its saved entry for the next save.
- A mod the save does not know starts fresh, and so does every mod when the save has no Lua section. The entry of a mod
  that is turned off stays in the save and comes back with the mod. Events fired but not delivered are not saved; a load
  drops them.

## Mod ids and mounts (ADR-7881, amended by ADR-7951 and ADR-7992)

The `luamod.ron` id is a claim. Mount identity prevents two loaded sources from claiming the same id. Frozen
luastate v1 saves keep manifest ids; they have no field proving which mount owns an entry.

| Files from | Mount (`lua.list` `mount`) |
|---|---|
| `ModFiles::from_mods_vfs(vfs, id, ..)` (Workshop: `workshop.<item id>`) | `mods:<id>`, e.g. `mods:workshop.31337` |
| `ModFiles::from_dir(dir, ..)` | `dir:<canonical-path BLAKE3 hash>`; same-named folders in different parents differ |
| `ModFiles::from_map(..)` | none, unless the game binds it with `.with_mount("<scheme>:<name>")` |

`lua.list` `save_key` is the manifest id used by v1. Directory mount identity is machine/path-specific; it is not
written into v1 saves.

- **One claimant per id.** A mod whose id another loaded mod already holds, from another mount (or whose mount
  another mod holds), is refused with `lua_id_taken`, logged as a warning; `lua.list` `contested_ids` lists the id.
  The same mod loaded twice stays `lua_manifest` (already loaded). At `Startup` claimants load in mount order:
  no mount, then `dir:`, then `mods:`.
- **Saves.** A Workshop mount (`mods:workshop.<item id>`) starts fresh from a v1 save, even when loaded alone.
  Its manifest entry is held back, as is an entry with a contested id (`lua.list` `held_state`). Both restore
  orders enforce this rule. A local `mods:` folder, a `.fpak` archive, and a `dir:` mount restore by manifest id.
  A save preserves a held entry, including its manifest id and values, instead of the Workshop claimant's live
  capture. An uncontested local claimant can restore it later.
- **Full saves.** Held bytes are counted first. A live mod that does not fit beside them is left out with
  `lua_too_large` on its own line, and the save continues. The entire save returns empty bytes only when the held
  entries alone pass the cap. The plugin propagates that failure through its required save section: the game save
  fails before replacing the previous file or cloud save. Direct `save_state` callers must also keep the previous
  save on that failure. The host retains the held entries.
- **Bad restored ids.** An entry whose id contains `:` is quarantined with a warning and left out. It does not
  fail the rest of the save. v1 mod ids never contain `:`.
- **Hot reload keeps the mount**: `lua.reload` with `files` replaces the code, not the mod's identity.
- **Known limit.** v1 cannot prove mount ownership, so it cannot safely restore even the original Workshop item's
  entry. Persistent mount binding requires a separately approved version and migration. Local state remains keyed
  by manifest id, and simultaneous duplicate manifest claims remain refused.

## Hot reload

Change a mod while the game runs. `lua.reload` (or `LuaMods::reload`) builds the new version beside the old one and
moves `regia.state` across, through `on_migrate` when the version changed. **Any failure rolls back**: the old version
keeps running as it was, with its state, handlers and entities, and whatever the new code did to the world is undone.
A reload that fails names the stage and the error.

| Stage | What runs |
|---|---|
| `files` | Size cap, manifest, same id, the entry exists. |
| `compile` | Every `.lua` file, including modules nothing has required yet. |
| `entry` | The new version's entry chunk. |
| `snapshot` | The old version's `on_save`, then the read of its state. |
| `state` | `regia.state` crosses over, with `on_migrate` if the version changed. |
| `start` | `on_load`, or the restart of a mod without one. |
| `done` | The new version runs. |

`state` in the answer is `carried`, `migrated` or `reset`. A state that is not plain data cannot cross: the reload goes
through with `reset` and a `note` naming the value. A failing `on_save` does not stop a reload either, so the reload that
fixes it can go through: the state is read as it stood, and `note` and `lua.errors` (call `on_save`) say so. If a later
stage fails, what that `on_save` wrote into `regia.state` stays there. A disabled mod comes back with a good reload and stays disabled after
a bad one. A mod with `on_load` keeps its entities; a mod without restarts, with its state.

For reload on save in development, `LuaModsPlugin::with_watch(30)` or `lua.watch` looks at directory-backed mods every 30
frames and reloads those whose text changed. Off by default. Mods from `ModFiles::from_map` have no directory: pass their
`files` to `lua.reload`. Passing `files` for a directory mod takes it off the watch for the rest of the process: a later `lua.reload` without `files` answers `lua_io`.

```json,ignore
{ "method": "lua.reload", "params": { "mod": "example.lights" } }
→ { "plugin": true, "mod": "example.lights", "ok": true, "stage": "done", "state": "carried", "status": "active", "version_before": "1.0.0", "version_after": "1.0.1" }

{ "method": "lua.reload", "params": { "mod": "example.lights" } }
→ { "plugin": true, "mod": "example.lights", "ok": false, "stage": "compile", "error": { "code": "lua_syntax", "message": "…example.lights/main.lua:12: 'end' expected near <eof>" }, "status": "active", "version_before": "1.0.0", "version_after": "1.0.0" }
```

Check a run the way the book's other agent pages do: reload, then `lua.list` (`reloads`, `failed_reloads`, `last_reload`)
and `lua.errors` (call `reload`). To see what a save would hold, use `lua.state`.

```json,ignore
{ "method": "lua.state", "params": { "mod": "example.lights", "tree": true } }
→ { "plugin": true, "mod": "example.lights", "ok": true, "state_version": 2, "tables": 1, "items": 2, "bytes": 96, "entities": 1, "tree": { "score": 3.0 } }
```

## Agent methods

| Method | Params | Result |
|---|---|---|
| `lua.list` | — | `{ plugin, seed, limits, watch_every_frames, emitted_held, emitted_dropped, last_save?, last_restore?, kept_state, contested_ids, held_state, mods: [{ id, name, version, status, code?, message?, calls, last_instructions, peak_instructions, memory_bytes, peak_memory_bytes, instruction_budget, memory_limit_bytes, subscriptions, reloads, failed_reloads, entity_cap, entity_cap_refused, mount?, save_key, last_reload? }] }` |
| `lua.run` | `chunk`, `mod`? | `{ plugin, ok, results, error?: { code, message }, instructions, memory_bytes, log }` |
| `lua.globals` | `mod`, `path`? | `{ plugin, mod, path, entries: [{ key, type, value?, len? }] }` |
| `lua.errors` | `mod`? | `{ plugin, errors: [{ mod, call, code, message }] }` |
| `lua.restart` | `mod` | `{ plugin, mod, status, error? }` |
| `lua.reload` | `mod`, `files`? (`{ path: text }`) | `{ plugin, mod, ok, stage, state?, from_version?, to_version?, note?, error?: { code, message }, status, version_before, version_after }` |
| `lua.state` | `mod`, `tree`? | `{ plugin, mod, ok, state_version?, tables?, items?, bytes?, entities?, tree?, error?: { code, message } }` |
| `lua.watch` | `every_frames`? | `{ plugin, every_frames, mods }` |
| `lua.entity_cap` | `mod`, `cap`? (a number, or `null` for the host's) | `{ plugin, mod, entity_cap, host_cap, override, owned, refused }` |
| `lua.authority` | — | `{ plugin, authority, commands, max_pending_per_mod, max_command_bytes, pending, listed: [{ mod, name, value }], snapshot_generation, mods: [{ id, authority, pending, refused, snapshot_builds }] }` ([host and presentation](#host-and-presentation-runtimes-adr-8466)) |

`lua.run` (permission `edit_scene`) runs a chunk (Lua source text, at most 64 KiB, `MAX_CHUNK_BYTES`; longer is
`F0002`) against the live world, in a fresh sandbox dropped after the call (its spawns go with it: nothing a
sandbox call leaves persists in the world), or inside a mod's state when `mod` is given (an error there disables the
mod; a call refused because the mod is already disabled never ran, so it reports 0 instructions and 0 bytes).
Non-plain return values come
back as `"<function>"`-style strings. `lua.globals` lists at most 512 entries (the lexicographically smallest keys; the rest are not shown). `lua.globals` and `lua.state` read raw table fields only: no metamethod and no
Lua code runs. `lua.reload` and `lua.watch` need `edit_scene` too. In `lua.state`, `tree` shows sequences as arrays,
other tables as objects, and a table already shown, or one past 16 levels of depth (not printed), as `{ "ref": index }`. `lua.list` `last_save` and `last_restore` hold
one line per mod with what happened, and `kept_state` the saved entries of mods that are not running. Every method
answers `{ "plugin": false }` when the app has no `LuaMods`. `agent.hello` lists the
capability marker `lua_mods` when the plugin is installed.

```json,ignore
{ "method": "lua.run", "params": { "chunk": "local e = regia.find('crate') return e and e:position()" } }
→ { "plugin": true, "ok": true, "results": [{ "vec3": [0.0, 1.0, 0.0] }], "instructions": 0, "memory_bytes": 41230, "log": [] }
```

`instructions` counts in steps of 1,000 (the budget check interval) plus the host work charged, so a short chunk
reports 0.

## Error codes

Script outcomes come back inside results (`error.code`, a mod's `code`):

| Code | Meaning |
|---|---|
| `lua_syntax` | The chunk did not compile, or was precompiled bytecode. |
| `lua_runtime` | A Lua runtime error, or a per-call count cap (spawns, emits) was passed. |
| `lua_instruction_budget` | The call ran past its instruction budget. |
| `lua_memory` | The mod's state hit its memory cap. |
| `lua_bad_value` | A value crossing the boundary, or in `regia.state`, was not plain data or broke a size cap (including the emit byte cap per call). |
| `lua_require` | `require` named a module outside the mod, a missing one, or one that requires itself. |
| `lua_disabled` | The mod is disabled by an earlier error. |
| `lua_manifest` | `luamod.ron` is missing, malformed, newer, has a bad id, or its entry is missing. |
| `lua_io` | Reading the mod's files failed. |
| `lua_too_large` | The mod's files, or its saved state, are larger than the host allows. |
| `lua_unknown_mod` | No mod has that id (the methods answer `F0003` instead). |
| `lua_id_taken` | Another mount already holds the id the mod's `luamod.ron` claims, or the mod's mount (below): the mod is not loaded. |
| `lua_state` | Saved mod state is corrupt or newer than this build. |
| `lua_entity_cap` | `regia.spawn` past the mod's entity cap (ADR-2418). |

Bad params answer `F0002`; an unknown `mod` answers `F0003`.

## Licence

Lua 5.1.5 is MIT licensed (Copyright (C) 1994-2012 Lua.org, PUC-Rio). `regia::lua::LUA_LICENSE_NOTICE` holds the
text for a credits screen.

## Entity cap (ADR-2418)

A mod may own at most `LuaLimits::max_entities_per_mod` entities at once (default 4,096, the most a save keeps): the
entities it spawned and has not despawned. `regia.spawn` past the cap raises an error with the code `lua_entity_cap`;
under `pcall` the mod sees it and goes on, uncaught it disables the mod like any other error. A despawn makes room
again. Another mod's entities never count. `LuaMods::set_entity_cap(id, Some(n))` (or `lua.entity_cap { mod, cap }`)
lowers one mod's cap, never above the host's; `None` (`cap: null`) goes back to the host's. The cap holds across
restarts, hot reloads (the new version's entry chunk already runs under it) and loads. `lua.list` shows each mod's
`entity_cap` and `entity_cap_refused`, the spawns it refused.

```text
lua.entity_cap     {"mod":"example.lights","cap":64}
→ { "plugin": true, "mod": "example.lights", "entity_cap": 64, "host_cap": 4096, "override": 64, "owned": 12, "refused": 0 }
```

While a save loads or a mod hot-reloads, the entities of the previous incarnation do not count until the start stage,
because they go (or the new version takes them over) before the mod starts: a mod at its cap whose entry chunk spawns
still loads and reloads. The entities a save restores are spawned as saved, without the cap check (a save holds at most
`max_state_entities`). `lua.restart` removes the entities of the run it replaces before `on_start` runs (also when
`on_start` then fails), so they do not count against the new run.

The cap check runs before the save cap (`max_state_entities`, `lua_too_large`); with both at their defaults the entity
cap is the one a mod meets.

## Host and presentation runtimes (ADR-8466)

A networked game runs its mods on two sides. `LuaMods::with_authority(ScriptAuthority::Host)` (the default; a
single-player game is its own authority) or `ScriptAuthority::Presentation` (a client), or
`LuaModsPlugin::with_authority(..)`, picks the side before the mods load. `regia.authority` is `"host"` or
`"presentation"`.

- **Host.** `regia.command(name, value?)` asks the game to do something. It returns `true` when the command is
  queued, or `false` and a reason: `unknown_command` (the game did not register that name), `bad_value` (a function,
  a coroutine or a non-finite number in the value), `too_large` (over `max_command_bytes`, default 4,096), or
  `queue_full` (the mod already has `max_pending_per_mod` commands waiting, default 32). A refusal never raises an
  error and never disables the mod; `lua.authority` counts it. The game registers its names and caps with
  `HostCommandRules` (`LuaMods::with_host_commands`, `LuaModsPlugin::with_host_commands`) and takes the queue with
  `LuaMods::drain_host_commands()`: mod id order, then the order each mod queued them. It applies each through its
  own command path, as it would a player's; the engine never acts on a command. A call that fails drops the
  commands it queued. Commands nobody drained survive a restart and a hot reload; loading a save clears them.
- **Presentation.** There is no `regia.command`: calling it is a script error, and a script function stored under
  that name reaches nothing. Hooks, events (`regia.emit`), the local world and `regia.snapshot()` work as on the
  host.

The game publishes what scripts may read with `LuaMods::publish_snapshot(world, value)` (plain data, at most
16 MiB, finite numbers). A changed snapshot calls every active mod's `on_snapshot(snapshot)` in id order, and
`regia.snapshot()` returns the same table (nil before the first snapshot). The table is built once per snapshot in
each mod (the build is charged to that call's budget); publishing an equal snapshot again runs no hook and rebuilds
nothing (`snapshot_builds` in `lua.authority` does not move). A mod's writes to its table stay in that mod until the
next snapshot; the host never reads them back.

```lua
-- host side: queue an order every second snapshot
function on_snapshot(s)
  if s.tick % 2 == 0 then
    local ok, why = regia.command("order", { unit = s.leader, to = 3 })
    if not ok then regia.log("order refused: " .. why) end
  end
end
```

`lua.authority` (permission `read`) lists the side, the registered names, the caps, up to 64 waiting commands (in
drain order, not drained), and per mod its side, waiting commands, refusals and snapshot builds:

```json,ignore
{ "method": "lua.authority", "params": {} }
→ { "plugin": true, "authority": "host", "commands": ["order"], "max_pending_per_mod": 32, "max_command_bytes": 4096, "pending": 1, "listed": [{ "mod": "example.ai", "name": "order", "value": { "to": 3.0, "unit": 7.0 } }], "snapshot_generation": 12, "mods": [{ "id": "example.ai", "authority": "host", "pending": 1, "refused": 0, "snapshot_builds": 6 }] }
```

Requirements came from the owner's game's script bridge (requirements only, nothing copied; `docs/provenance.md`).
