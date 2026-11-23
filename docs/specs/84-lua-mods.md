# 84 — Embedded Lua for mods (`regia_script_lua`)

Status: v1 (ADR-1751, accepted 2026-10-30 by the lead on the owner's delegation), with state in save games and hot
reload (§§ 11-12, ADR-1943). Master plan § 12.3, rows B1-B6 and L3.

## 1. Purpose

Gameplay in Regia Engine is Rust. This crate lets a game accept **mods written in Lua 5.1** from players: small
scripts that react to game events, read and move named entities, spawn their own entities and send events back to
the game. Mods are untrusted code, so every mod runs in its own sandboxed Lua state with an instruction budget per
call, a memory cap, no filesystem, network, process or clock access, and error isolation (a failing mod is disabled;
the game and the other mods keep running).

The crate is **optional**: the facade feature `regia/lua` (off by default) compiles it. Without the feature no Lua
code is built and nothing in the engine changes.

## 2. Crate and API shape

`crates/regia_script_lua`, layer 3 (Gameplay), depends on `regia_core`, `regia_ecs`, `regia_app`, `regia_scene`,
`regia_agent` (for the `AgentExtension`) and `regia_framework` (for the save section), plus `mlua` 0.12.2 (`lua51`,
`vendored`, `send`) and `postcard`.
`#![forbid(unsafe_code)]`.

| Item | What it is |
|---|---|
| `LuaLimits` | Host budgets: `instruction_budget` (per call, default 1,000,000), `memory_limit_bytes` (per state, default 16 MiB), `hook_interval` (default 1,000 instructions), `max_mod_bytes` (all files of a mod, default 4 MiB), `max_log_lines` (default 256), `max_spawns_per_call`, `max_emits_per_call`, `max_emit_bytes_per_call`, `max_call_millis`, and the saved-state caps of § 11. A manifest may ask for less, never more: a smaller instruction request also lowers that mod's hook interval to match; memory is floored at 64 KiB when the host cap allows it (a host cap under 64 KiB is kept; a request is never raised above the host cap) and at what an empty state needs. |
| `ModFiles` | A mod's files: relative path → source text (`BTreeMap`). `ModFiles::from_dir(path, max_bytes)` reads a directory on desktop (`.lua` files and `luamod.ron`, no symlinks followed out of it); `ModFiles::from_map` builds one in memory (tests, packed mods, Android). |
| `LuaModManifest` | `luamod.ron` (§ 3). |
| `LuaMod` | One mod: manifest, files, its Lua state, status (`Active` or `Disabled { code, message }`), counters (calls, last and peak instructions per call, memory used and peak), log ring, error ring (last 32). |
| `LuaMods` | Resource: every mod sorted by id, the host seed, limits, the incoming event queue and the outgoing (emitted) queue. `load`, `restart`, `get`, `iter`, `fire(name, ScriptValue)`, `drain_emitted()`, `call_update(world, dt)`, `call_tick(world, dt)`, `run_chunk(world, mod, chunk)`, `call_function(world, mod, function, arg)` (a non-disabling call with a return value, ADR-7268), `save_state(world)`, `restore_state(bytes, world)`, `peek_state(id, world)`, `reload(id, files, world)`, `poll_files(world)`, `set_watch(frames)`, `tick_watch(world)`. |
| `ModWorld` | Trait for world access (find by name, alive, position, set position, spawn, despawn, name, `find_except`, the same find past given entities, and `owned`, the entities a mod spawned, default none). Implemented for `regia_ecs` `World` through `EcsModWorld`; tests use their own. |
| `ScriptValue` | A plain value crossing the boundary: `Nil`, `Bool`, `Number(f64)`, `String`, `Vec3`, `Entity(u64)`, `List`, `Map(BTreeMap)`; caps: depth 16, 4,096 items, 64 KiB per string. Serde-serializable (agent JSON). |
| `LuaErrorCode` | The lowercase protocol codes (§ 4). |
| `LuaModsPlugin` | Adds `LuaMods`, runs `on_update` in `Update`, `on_tick` in `FixedUpdate`, delivers queued events after the tick, registers the agent methods and the save section. `with_watch(frames)` turns the file watch on. |
| `StateTree`, `ModState`, `LuaStateFile` | The plain-data tree of `regia.state` and the `luastate` format (§ 11). |
| `LUA_LICENSE_NOTICE` | The Lua 5.1.5 copyright and MIT text for a game's credits. |

### Lua side (`regia` table, `regia.api_version == 1`)

| Lua | Meaning |
|---|---|
| `function on_start()`, `on_update(dt)`, `on_tick(dt)` | Optional globals the host calls (once after load; every frame; every fixed step). |
| `regia.on(name, fn)` | Subscribe `fn(value)` to game event `name`. |
| `regia.emit(name, value, target?)` | Queue an event for the game (`LuaMods::drain_emitted`). `target` is an entity handle the mod holds; it is recorded in `EmittedEvent::target` (spec 102 § 4.4 amendment, ADR-7268), and the umbrella crate's logic bridge routes the event into that entity's logic-graph inbox (no target: entities marked `LogicModHost` for the mod). |
| `regia.log(...)`, `print(...)` | Mod log ring and `tracing` `info`. |
| `regia.find(name)` | Entity handle of the first entity with that `Name`, or `nil`. Two results for one entity are the same userdata. |
| `regia.spawn(name, pos)` | Spawn a named entity at `pos` (a `vec3`); the mod owns it. |
| `e:alive()`, `e:name()`, `e:position()`, `e:set_position(v)`, `e:despawn()`, `e:id()` | Stale-safe handle methods: a dead or reused entity answers `false`/`nil`. `e:id()` is `"{index}v{generation}"` (low 32 bits, `v`, high 32 bits); the agent `entity` field stays the decimal bits. `despawn` works only on the mod's own entities. |
| `regia.vec3(x, y, z)` | Vector value: `.x .y .z`, `+ - * /` (scalar on either side for `*`), unary `-`, `==`, `:length()`, `:dot(v)`, `:cross(v)`, `:normalized()`. |
| `regia.random([m [, n]])`, `math.random`, `math.randomseed(s)` | Deterministic PCG32 per state (§ 6). |
| `require(name)` | A module from the mod's own files. |
| `regia.state` | The mod's plain-data state: a table that goes into save games and across hot reloads (§ 11). |
| `regia.state_version` | The version of that table's shape; a whole number from 1, absent means 1. |
| `function on_save()` | Optional: runs before the state is read, to copy live data into `regia.state`. |
| `function on_load(info)` | Optional: runs after a load or a reload instead of `on_start`. `info` has `reason` (`"save"`, `"reload"`), `state` (`"carried"`, `"migrated"`, `"reset"`), `from_version`, `to_version`. |
| `function on_migrate(old, from, to)` | Optional: turns an old state into the new shape when the versions differ; returns the new table, or nothing when it edited `old` in place. |

## 3. Files and formats

A mod is a directory (or an in-memory map) with `luamod.ron` and `.lua` files:

```ron
(
    format_version: 1,
    id: "example.lights",       // [a-z0-9_.-], 1..64 chars; mods run in id order
    name: "Blinking lights",
    version: "1.0.0",
    entry: "main.lua",          // relative path inside the mod
    api_version: 1,             // the `regia` table version the mod was written for
    instruction_budget: None,   // optional; clamped to the host's LuaLimits
    memory_limit_bytes: None,   // optional; clamped
)
```

- Format name `luamod`, `format_version` 1, `deny_unknown_fields`. A file with a newer version (or version 0), an
  unknown `api_version`, a bad id, an entry outside the mod or not ending in `.lua`, or a `name` or `version` over
  256 bytes is refused with `lua_manifest`. Ids starting with `agent.` are reserved (`lua.run`'s throwaway state
  owns its entities as `agent.scratch`) and refused.
- The mod manager (`ModsVfs`, § 13) reads only `version` from the `luamod.ron` at a mod's root, as the mod's version text in
  a multiplayer session's content manifest (spec 77 § 4.1, ADR-7259); a mod without one has empty version text.
- **Migration note.** v1 is the first version: there is no migrator. The format is registered with
  `cargo xtask check-formats` (`luamod_schema_fingerprint`, `LUAMOD_FORMAT_VERSION`, `luamod_migration_sources`);
  a v2 bumps the version, adds a v1 → v2 step in the crate and its `regia upgrade-project` registration.
- `.lua` files are text. Precompiled chunks are refused (`lua_syntax`).

## 4. Agent-protocol surface (`book/agents/lua-mods.md`)

Registered on the `AgentExtension` by `LuaModsPlugin`; capability marker `lua_mods`.

| Method | Perm | Params | Result |
|---|---|---|---|
| `lua.list` | Read | — | `{ plugin, seed, limits, watch_every_frames, emitted_held, emitted_dropped, last_save?, last_restore?, kept_state, mods: [{ id, name, version, status, code?, message?, calls, last_instructions, peak_instructions, memory_bytes, peak_memory_bytes, instruction_budget, memory_limit_bytes, subscriptions, reloads, failed_reloads, entity_cap, entity_cap_refused, last_reload? }] }` |
| `lua.run` | EditScene, mutating | `chunk` (string), `mod`? | `{ ok, results?, error?: { code, message }, instructions, memory_bytes, log }`. Without `mod`, a fresh sandbox state with the host limits, dropped after the call — its spawns go with it, so nothing a sandbox call leaves persists in the world. With `mod`, inside that mod's state (it must be active; a budget error disables it like any other call; a call refused because the mod is disabled never ran and reports 0 instructions and 0 bytes). |
| `lua.globals` | Read | `mod`, `path`? (dotted path of a table, e.g. `config`; every segment, the last included, must name a table, else `lua_bad_value`) | `{ mod, path, entries: [{ key, type, value? }] }` sorted by key; scalars carry `value`, tables carry `len`. An error result still carries `mod`, `path` and `entries`. |
| `lua.errors` | Read | `mod`? | `{ errors: [{ mod, code, message, call }] }` |
| `lua.restart` | EditScene, mutating | `mod` | `{ mod, status }`: rebuilds the state from the mod's files and calls `on_start`. |
| `lua.reload` | EditScene, mutating | `mod`, `files`? (`{ path: text }`) | `{ mod, ok, stage, state?, from_version?, to_version?, note?, error?: { code, message }, status, version_before, version_after }`: hot reload (§ 12). Without `files`, reads the mod's directory. |
| `lua.state` | Read | `mod`, `tree`? | `{ mod, ok, state_version?, tables?, items?, bytes?, entities?, tree?, error? }`: what the mod would save now, or why not. Raw reads; no Lua runs. |
| `lua.watch` | EditScene, mutating | `every_frames`? | `{ every_frames, mods }`: reads or sets the file watch (§ 12); 0 is off. |

Script outcomes come back in results with these codes: `lua_syntax`, `lua_runtime`, `lua_instruction_budget`,
`lua_memory`, `lua_bad_value`, `lua_require`, `lua_disabled`, `lua_manifest`, `lua_io`, `lua_too_large`,
`lua_unknown_mod`, `lua_state`, `lua_entity_cap` (§ 13). Protocol errors: `F0002` (bad params), `F0003` (no such mod).
`lua.entity_cap` (§ 13) and the script-component methods (§ 14) are registered under the same capability.

## 5. Sandbox rules

As ADR-1751 § 3-5: one state per mod; base (minus `dofile`, `loadfile`, `load`, `loadstring`, `module`, `gcinfo`,
`newproxy`; `collectgarbage("count")` only, and `collectgarbage()` with no argument answers the same count
instead of collecting), `string` (minus `dump`), `table`, `math`, `coroutine`; no `io`, `os`,
`package`, `debug`. `string.find`, `match`, `gmatch`, `gsub` and `gfind` are the step-budgeted port of ADR-1957 § 2,
not the C functions. `pcall`, `xpcall` and `coroutine.resume` re-raise budget and memory errors. Any error disables
the mod and lands in its error ring and `tracing` at `warn`. The `pcall` and `xpcall` wrappers run the protected
function through a closure that packs its results into one table: Lua 5.1.5's own `pcall`/`xpcall` push their
status without a stack check, so a function returning about 20 or more values (or a `pcall` recursion) overruns
the C frame (an API-check abort in debug builds). `coroutine.resume` checks its stack and is wrapped as is.

Budgets see host work too (ADR-1943 § 13, ADR-1957 § 2). `string.find`, `match`, `gmatch` and `gsub` are a Rust port of
Lua's matcher (ADR-1957's; its `gmatch` iterator lives in Lua, ADR-1958) that charges the instruction budget (4 steps per
instruction, depth 200), because the C matcher never returns to the
hook and backtracks exponentially on a short pattern; a differential test holds it to the C library. `string.gfind`, Lua
5.1's compat name for `gmatch`, is the same function. `gsub`'s result is capped at the state's memory limit. `table.sort`
without a comparator sorts with a Lua one, so each comparison is instructions the hook counts: Lua 5.1 picks its pivot by
a fixed rule, so an input can be built that takes it n^2 / 4 comparisons in C (ADR-1958). `string.rep` of an empty string
returns at once: Lua 5.1 loops `n` times over nothing, unseen by the hook and the memory cap. Every world call costs 16
instructions; `regia.find` 64 more, plus one per 8 of `ModWorld::find_cost` before it runs and one per entity
`take_scanned` reports after. A world with a name index answers a find cost of 1 and one probe. A world without
one still answers its entity count, so that pre-charge follows the world's size. One event delivery pass
is one call for each mod: its handlers share one budget and one set of emit and spawn caps, with at most 256 handlers per
event. Emits are capped in bytes per call (`max_emit_bytes_per_call`, default 1 MiB). `max_call_millis` (default 1,000; 0
is off) is a watchdog in the same hook, for work no count can see: one `..` of two huge strings is one instruction. It
reports `lua_instruction_budget`. Timing decides when it fires, but only for a call far past any frame budget. In a
delivery pass it adds up the mod's own handlers and nothing between them, so a slow mod cannot trip it for the mods after
it.

The memory cap sees the Lua heap only, and Lua strings are interned: a table of a million references to one long string
costs a mod a few MiB. So nothing that copies a mod's strings to the Rust heap may scale with references (ADR-1958).
`gmatch` iterators keep their subject in Lua. `regia.log` copies what the line keeps. A value crossing the boundary holds at
most `MAX_VALUE_BYTES` (1 MiB, ADR-1957) of strings; `lua.run`'s results share one such allowance, and a result past it is
described as `"<type>"`. `lua.globals` reads values only for the entries it returns. Events wait for
`LuaMods::drain_emitted` up to `MAX_EMITTED_HELD` (65,536) and `MAX_EMITTED_HELD_BYTES` (64 MiB), oldest dropped first, with
one warning; `lua.list` reports `emitted_held` and `emitted_dropped`.

## 6. Determinism

- The PRNG is `regia_core::math::Rng` (PCG32) seeded with `LuaMods::seed` and stream = FNV-1a 64 of the mod id;
  `math.randomseed(s)` reseeds from `s` (integer part) with the same stream. Same seed, same mods, same events →
  same results on every platform.
- Mods load in id order; per frame each active mod's `on_update` runs in id order; per fixed step `on_tick` then the
  queued events, each event to every subscribed mod in id order and subscription order.
- No clock, no `os`. The instruction budget counts VM instructions and charged host work (matcher steps, world
  operations, entities a `regia.find` walked), not time. The one exception is the watchdog (§ 5), which only ever ends
  a call that has run for a second. `regia.find` of a shared name answers the lowest entity index, whatever the
  iteration order.
- One delivery pass shares one budget per mod (ADR-1943 § 13, stricter than ADR-1957 § 1's sixteen calls), so a mod
  that overruns it does so at the same event on every run.
- Not deterministic (documented on the guide page): `tostring` of tables, functions and userdata; `pairs` over
  tables keyed by tables, functions or userdata; the last digit of `tostring(number)` across C libraries.

## 7. Tests and acceptance

Unit and integration tests in the crate (no GPU, no goldens):
1. Sandbox: `io`, `os`, `debug`, `package`, `load`, `loadstring`, `dofile`, `loadfile`, `string.dump` are absent;
   a bytecode chunk is refused; `require` reaches only the mod's files and refuses `..`.
2. Budgets: an infinite loop stops with `lua_instruction_budget`; the same inside `pcall`, `xpcall` and a
   coroutine; a growing table stops with `lua_memory`; the mod is disabled and the next mod still runs.
3. Isolation: a runtime error disables only that mod; `restart` brings it back.
4. Determinism: two hosts with the same seed produce the same random sequence and the same emitted events; a
   different mod id gives a different stream.
5. Host API: stale entity handles answer `nil`/`false` after despawn and after slot reuse; a mod cannot despawn a
   game entity; `vec3` arithmetic; `ScriptValue` caps.
6. Manifest: v1 loads; v2, unknown fields, bad ids and escaping entries are refused.
7. ECS: the plugin calls `on_tick`/`on_update` in an `App`, events round-trip, agent methods answer.
8. Fuzz-style: generated chunks and value trees through the binding layer never panic.
9. Saves (`tests/saves.rs`, `tests/ecs.rs`, `state.rs`): state crosses into a new host; a loaded game goes on exactly
   like the run it was saved from; every plain shape, identity of shared tables and binary strings; non-plain values
   refused with their path; `on_migrate`, in place, reset, a failing one; a local holding `regia.state` stays the state;
   caps per mod, in total, at a memory-capped state, lower on load, a save exactly at the total cap reads back; a mod's
   failure stays with it and a failing `on_save` keeps its state; kept entries; damaged and newer blobs; hostile bytes
   never panic the decoder; through `SaveGame`, the save flow and an older save without the section.
10. Reload (`tests/reload.rs`, `tests/ecs.rs`): state carried, handlers replaced, random numbers go on, also on a reset
    and untouched by a rollback; each stage that can fail rolls back with the world as it was; a failing entry chunk
    never runs the old `on_save`, and a failing `on_save` does not stop the reload; a despawn waits for the commit and
    `regia.find` sees past it; a disabled mod returns; a fixed-frame watch reloads once, reports a bad save once and
    recovers.

Acceptance: the tests pass on Linux; `cargo ndk -t arm64-v8a check -p regia_script_lua` and
`cargo ndk -t arm64-v8a check -p regia --features lua` build; the Windows check is the lead's
(`cargo xwin check -p regia_script_lua --target x86_64-pc-windows-msvc`).

## 8. Platforms

Linux, Windows (MSVC and clang-cl) and Android aarch64: the Lua C sources compile with `cc` and the platform's C
compiler; nothing platform-specific is used (no `io`, `os`, `package`). macOS: builds like Linux (testing only).
`ModFiles::from_dir` uses `std::fs` and is meant for desktop; Android games pass `ModFiles::from_map` from their
own VFS reads until `mods://` (master plan A3) exists.

## 9. Performance budget

- State creation and sandbox setup: target under 1 ms per mod; memory about 30-60 KiB per empty state.
- Per call overhead (scoped world dispatcher, hook bookkeeping): target under 10 µs.
- The count hook costs one Rust call per 1,000 instructions.
- `call_update`/`call_tick` carry `tracing` spans (`lua_mods.update`, `lua_mods.tick`, per mod
  `lua_mod.call`). The default budget (1 M instructions) is roughly 5-20 ms of interpreter time per call on a
  desktop core: a ceiling for a runaway mod, not a target. Games set a lower budget for per-frame hooks.
- Measured 2026-10-31 at the head of ADR-1958 (the build server under load, the workspace's optimized dev profile, `NoWorld`, one
  throwaway run, not a CI check): load and sandbox setup 0.19 ms per mod; empty state 38 KiB; an empty `on_update` call
  2.0 µs. A small state: save 4 µs, restore 0.20 ms, hot reload 0.21 ms per mod. A state of 10,000 records in 560 KB: save
  26 ms, restore 66 ms, reload 94 ms, most of it the entry chunk building those records again. Neither runs per frame.
  `string.find` 0.43 µs a call, one `gmatch` step 0.9 µs. `table.sort` of random numbers costs 3 instructions per
  comparison: 10,000 items take 428,000 instructions and about 9 ms, and about 20,000 fit in the default budget.
  `tracing` spans: `lua_mods.save`, `lua_mods.restore`, `lua_mods.reload`, `lua_mods.watch`, and `lua_mods.events`
  for an event delivery pass.

## 10. Out of scope (later rungs)

Luau as a second backend (new ADR); case-insensitive lookup (A4; `mods://` mounts are § 13); a mod SDK or type
definitions; a child-process sandbox. Saved references to
entities (a handle in `regia.state`; § 11); saving components a game added to a mod's entity; saving coroutines,
closures and handler lists.

## 11. State in save games (ADR-1943)

**What is saved.** `regia.state`, a table the host creates and a mod fills (or replaces), with `regia.state_version`;
the mod's random number generator, so a loaded game goes on with the same numbers; and the entities the mod owns, as
name and position. Plain data only: numbers, booleans, strings (bytes, exactly), `vec3`s, and tables of them keyed by
numbers, strings or booleans. A `vec3` is three numbers with no host object behind it, so it counts as plain. Functions,
coroutines, entity handles and tables with a metatable are refused with `lua_bad_value` and the path of the offender
(`regia.state.player.weapon: a function cannot be saved`). A handle's entity bits mean nothing after a load: save the
entity's name and `regia.find` it in `on_load`. Shared tables and cycles survive: the tree is a flat list of tables, so
two references to one table are one table again after a load. What is not saved: locals, closures, handler lists,
coroutines, the log and the counters. The entry chunk builds those again.

**Size limits** (`LuaLimits`, enforced on save and on load): `max_state_bytes` 1 MiB per mod, `max_state_items` 262,144
tables plus entries per mod, `max_state_entities` 4,096 per mod (also checked by `regia.spawn`, which refuses a spawn past it with `lua_too_large`;
at the defaults the entity cap of § 13, also 4,096 and checked first, refuses it with `lua_entity_cap`, so a mod meets
`lua_too_large` only when a host sets `max_entities_per_mod` above `max_state_entities`), `max_state_total_bytes` 8 MiB for all mods. Tables
nest at most 128 levels (`MAX_STATE_DEPTH`); a cycle is not nesting. Past a cap the mod is left out of the save with
`lua_too_large`.

**Saving** (`LuaMods::save_state`): for each mod in id order, `on_save` (a mod call under its budget; an error disables
the mod as usual), then the raw read. The read runs no Lua: no metamethod fires and the instruction budget is not
involved; its cost is bounded by the caps. A mod whose state cannot be read, or is over a cap, is left out and reported
on its line of `SaveReport` (`lua.list` `last_save`, and `lua.errors` with call `save`). A failing `on_save` is reported
there too, but the state saves as it stood: a bug in a hook must not cost the player the mod's data. Neither fails the
save. A disabled mod saves what it has, or the entry it was restored from when a restore failed. Owned entities the
save leaves out — their `Name` or `Transform` was gone, or they were not restorable (a non-finite position, an
overlong name) — are warned once each per save (one `warn` per entity at every save, nothing is remembered between saves)
and counted on the line (`skipped_entities`).

**Loading** (`LuaMods::restore_state`): every mod is rebuilt in a fresh Lua state and its entry chunk runs. Then, with a
saved entry:
1. `regia.state` is filled from the entry (same version), or the entry is passed to `on_migrate` (versions differ), or
   `regia.state` stays as the entry chunk set it when the mod has no `on_migrate` (`reset`). It stays the same table the
   entry chunk saw, emptied and refilled, so a `local s = regia.state` at the top of a chunk stays the state; only a table
   with a metatable is replaced. During `on_migrate`, `regia.state` is what the entry chunk set up and `old` is the saved
   state; what it returns, or `old` when it returns nothing, then goes into that same table.
2. The random number generator is restored.
3. The entities the mod owned before the load go.
4. With `on_load`: the saved entities are spawned and `on_load(info)` runs. Handles from before the load are gone, so
   `on_load` finds what it needs by name. The saved list is the mod's entities: what the entry chunk or `on_migrate`
   spawned during this load is despawned first (ADR-2080). On a hot reload with `on_load`, the previous version's
   entities stay and what the new version's entry chunk or `on_migrate` spawned goes.
5. Without `on_load`: `on_start` runs. A mod that knows nothing of saves restarts cleanly: it spawns what it spawned the
   first time. Only `regia.state` came back, and it ignores that.

A mod with no entry starts fresh (`on_start`). So does every mod when the save has no Lua section at all (a save from
before the mods) or the section cannot be read (`RestoreReport::error`, `lua_state`). Events queued or emitted before
the load are dropped: they belong to the abandoned timeline, and events fired but not delivered at save time are not
saved. A mod that fails during a load is disabled and its entry kept, unless the entry is corrupt in a way no version of
the mod could load (no root table, a NaN key, one key twice in a table under Lua's key equality, a reference to a table
it does not hold; ADR-2080): that entry is dropped, and the next save holds what the disabled mod has. An entry for a
mod that is not running is kept
too and written back by the next save, so a player who turns a mod off for a while loses nothing; `LuaMods::load` adopts
it when the mod comes back. A revived mod that re-declares a component name another mod holds is refused like a load
(`lua_bad_value` on its `RestoreReport` line): the name is not registered, the mod stays disabled, its entities go and
its saved entry is kept.

**Where it lives.** `LuaModsPlugin` registers the save section `regia_script_lua::LuaState`
(`regia_framework::save_game::SaveSections`). A section writes its bytes with the whole world in hand before the
entities are serialized and reads them after the world is restored, on every path through `SaveGame` (save, encode,
load, decode, the save flow, the autosave). It is stored as one more `SavedResource`, so the save format stays at
version 1 (`docs/formats/save-format.md`).

**Format `luastate`, `format_version` 1** (registered with `cargo xtask check-formats`): magic `RLST`, version (`u32`
LE), postcard `LuaStateFile { mods: [ModState { id, mod_version, state_version, rng, tree: StateTree { tables },
entities }] }`. Nothing about it recurses when decoded, and each entry is checked (`ModState::check`) before use.
Two levels of version:
- `regia.state_version` is the mod's own schema; `on_migrate` is its migrator, and it runs in the new code under the
  budget. Its result must still be plain data.
- `format_version` is the container. **Migration note.** v1 is the first version, so no step exists; a v2 bumps
  `LUASTATE_FORMAT_VERSION`, adds a v1 → v2 step in `state::decode` and lists it in `luastate_migration_sources`. A newer
  file is refused (`lua_state`) and the mods start fresh. The container is only ever inside a save file, so there is
  nothing for `regia upgrade-project` to rewrite.

## 12. Hot reload (ADR-1943)

`LuaMods::reload(id, files, world)` replaces a mod's code while the game runs. `files` is the new file set; `None`
reads the directory the mod was loaded from. The trigger is `lua.reload`, a game's own call, or the file watch:
`LuaMods::set_watch(frames)` (or `LuaModsPlugin::with_watch`, `lua.watch`) checks directory-backed mods every `frames`
frames and reloads those whose text changed. The check compares file size, modification time and file identity
(the inode) and reads bodies only when those differ: a rename-based save that keeps the size and the modification
time is still seen (ADR-5895). Off by default; the check runs on the game thread, so it is for
development. Files a reload refused are not tried again until they change, so a broken save reports once.

The new version is built beside the old one. Stages (`ReloadOutcome::stage`):

| Stage | What runs | Failure means |
|---|---|---|
| `files` | Size cap, manifest, same id, the entry exists. | Bad files. |
| `compile` | Every `.lua` file compiles, not only the ones the entry requires at once. | A syntax error, named by file and line. |
| `entry` | The new version's entry chunk, under its budget. | An error, or a budget. |
| `snapshot` | The old version's `on_save` (not when it is disabled), then the raw read of its state. | Nothing: see below. |
| `state` | `regia.state` crosses over, through `on_migrate` when `regia.state_version` changed. | The migrator failed or left non-plain data. |
| `start` | `on_load({ reason = "reload", … })`; without `on_load` the mod restarts: its entities go and `on_start` runs. | An error, or a budget. |
| `done` | Nothing: the stage a successful reload reports. | — |

The old version's `on_save` runs only once the new entry chunk has, so a new version that cannot start leaves the old
one untouched. A failing `on_save` is not a failed reload, or a bug in it could only be fixed by restarting the game: the
state is read as it stood, `note` says so, and `lua.errors` has it with call `on_save`.

**Rollback.** Any failure drops the new state and leaves the old version running as it was: its state, its random
numbers, its handlers, its entities. World writes during a reload go through a journal (`JournalWorld`): spawns are
undone, moves are undone newest first, and a despawn of an entity from before the reload is held until the reload
commits (the mod already sees it as dead, and `regia.find` looks past it). Events the new version emitted are dropped
with it, and so are those of the old version's `on_save`. The one thing a failure at the `state` or `start` stage cannot
take back is what that `on_save` wrote into `regia.state`, which it writes on every save anyway. The failure lands in the
mod's error ring with call `reload`, in `failed_reloads` and in `last_reload`. It does not disable the old version. A
failed reload of a disabled mod leaves it disabled; a good one brings it back.

**State across a reload** (`ReloadOutcome::state`): `carried` when the versions match, `migrated` through `on_migrate`,
`reset` when they differ with no `on_migrate`, or when the old state could not be read (not plain data; `note` says why,
and the reload still goes through: a developer fixing that very bug must be able to reload). `regia.state` stays the
table the entry chunk saw, as on a load (§ 11). The random number generator carries over, also on a `reset`. Budgets hold
for the new version: instruction budget per call, the memory cap on its state.

## 13. `mods://` and the entity cap (ADR-2418)

**`mods://`** is `regia_asset::mods_vfs::ModsVfs`: mod folders, `.fpak` archives and in-memory maps mounted in a declared
load order (first = base; a later mod's file wins at the same path; every override is a reported `ModConflict`).
Read-only and confined: relative paths only (absolute, drive letters and `..` are `mods_path_escape`), no symbolic
link or reparse point below a mod root at the mount or on a read (`mods_symlink`), only the files indexed at the
mount, each read checked against the size and BLAKE3 recorded then (`mods_changed`). Each mod has a content hash over
its sorted file listing (independent of its id and source kind) and the set has a content-set hash over those in load
order; lobbies match on it under the data key `mods.content_set`. The byte layouts are in the module docs and pinned by
tests. `ModFiles::from_mods_vfs(vfs, id, max_bytes)` builds a Lua mod's files from its mount, on every platform.
Agent: `mods.list`, `mods.mount`, `mods.unmount`, `mods.order`, `mods.hash`, `mods.read` (`book/agents/mods.md`).

**Entity cap.** `LuaLimits::max_entities_per_mod` (default 4,096) is the most entities one mod may own at once (spawned
and not despawned; other mods' entities do not count). `regia.spawn` past it fails with `lua_entity_cap` (catchable;
uncaught it disables the mod like any error), before the save cap's `lua_too_large`. `LuaMods::set_entity_cap(id, cap)`
lowers it for one mod, never above the host's, and the cap holds across restarts, reloads (the new entry chunk runs
under it) and loads. `lua.entity_cap { mod, cap? }` reads or sets it; `lua.list` shows `entity_cap` and
`entity_cap_refused`. Test: `tests/entity_cap.rs`.

## 14. Script components (ADR-3356)

A mod declares a **script component** once, in its entry chunk or a required module:

```lua
regia.component("mover", {
    props = {
        speed  = { type = "number", default = 2.0, min = 0.0, max = 20.0, step = 0.1 },
        active = { type = "bool",   default = true },
        label  = { type = "string", default = "mover", multiline = false },
        offset = { type = "vec3",   default = regia.vec3(0, 1, 0) },
        tint   = { type = "color",  default = { 1.0, 0.5, 0.25, 1.0 } },
        target = { type = "entity" },                     -- default: no reference
        mesh   = { type = "asset",   filter = "mesh" },
        mode   = { type = "enum",    options = { "walk", "run" }, default = "walk" },
    },
    on_update = function(e, dt) local p = e:props("mover") if p.active then
        e:set_position(e:position() + p.offset * (p.speed * dt)) end end,
})
```

- **Types.** `number` (`min`/`max` clamp, `step`), `bool`, `string` (`multiline`), `vec3`, `color`
  (stored as a list of four linear channels), `entity` (entity bits or none), `asset` (a project
  asset path; `filter` is the inspector's `@AssetFilter` syntax) and `enum` (a non-empty `options`
  list). Every write is validated and clamped against the declaration (`lua_bad_value` on a
  mismatch). Names are 1–64 bytes of lowercase ascii, digits and `_`; a mod declares at most 64
  components of 1 to 64 properties each (a declaration without a `props` table, or with an empty
  one, is `lua_bad_value`). A Lua table keeps no declaration order, so the properties are listed
  sorted by name (inspector, `lua.components`, `e:props`) on every load (ADR-5517); component names are global across every loaded mod
  (a second declaration fails the load, restart, reload or save-restore, which rolls the new version's world
  writes back and does not register the name).
- **Entity handles.** `e:add(name)` attaches the component with every property at its default
  (one script component per entity), `e:has(name)`, `e:remove(name)`, `e:props(name)` reads the
  values as a plain table, `e:set_prop(name, key, value)` writes one. A mod may only touch
  components it declared itself. All of it goes through the `ModWorld` seam: a trial reload's
  component writes are journaled and roll back with its spawns and moves.
- **Per-entity hook.** An `on_update(entity, dt)` in the declaration runs once per frame per entity
  carrying the component, in declaration order. All of a mod's passes in one update are one call as
  far as the budgets go: they share one instruction budget, one watchdog and the emit and spawn
  caps, so a mod cannot multiply its budget by declaring many components (ADR-5870; the global
  `on_update` is its own call). An empty pass makes no call and does not touch the instruction
  budget or the counters. A pass that has entities charges that shared budget 64, plus one
  instruction per 8 entities the lookup looked at (`ModWorld::component_cost`: 1 with the component
  index, the world's entity count without one, like `regia.find`'s pre-charge), plus one per entity
  in the pass (ADR-5816). An error disables the mod like any call.
- **Persistence.** Values live in `LuaComponent { decl, props }`, a reflected component
  (`BTreeMap<String, LuaValue>`) saved in scenes by the reflection codec like any other component
  (no scene-format version change). The editor draws it in the inspector from the declaration and
  loads a project's `mods/<id>/` on open with the directory watch on.
- **Reload.** After every successful load, hot reload, restart and save-restore, stored values are
  reconciled against the declaration: values of properties still declared survive (designer edits
  included), new properties get their defaults, removed ones go, a property whose type changed
  falls back to its default.
- **Agent.** `lua.components`, `lua.props.get`, `lua.props.set`, `lua.props.list`
  (`book/agents/lua-component-props.md`).

`regia.state` is unchanged; component values are *not* part of `luastate` (a restore re-attaches
only what the mod recreates in `on_load`).

## 15. Mod ids bound to their mount (ADR-7881, amended by ADR-7951 and ADR-7992)

The `luamod.ron` `id` is a claim. Each mod is bound to its mount (`ModFiles::mount`): `mods:<mount id>` for files read
from `mods://` (Workshop: `mods:workshop.<item id>`), `dir:<canonical-path BLAKE3 hash>` for a local directory, what
`ModFiles::with_mount` set (`<scheme>:<name>`, at most 256 bytes, no NUL), and none for `ModFiles::from_map`.
Canonical path bytes distinguish same-named directories in different parents and avoid lossy Unicode conversion.
This mount identity is machine/path-specific. The v1 *save key* remains the manifest id.

- A load whose manifest id another loaded mod holds from another mount, or whose mount another mod holds, is refused
  with `lua_id_taken`, logged as a warning. The second claim makes the manifest id contested this session. The same
  files loaded twice stay `lua_manifest` (already loaded). `LuaModsPlugin` loads the claimants of one id in mount order
  (no mount, `dir:`, `mods:`).
- Frozen `luastate` v1 entries retain manifest ids, with unchanged fields and version. Mount keys must never be
  written into entry ids. Mount binding on disk requires a separately approved format version and migration.
- A Workshop mount (`mods:workshop.<item id>`, the id `regia_steam::workshop` builds) never receives a v1 manifest
  entry, even when it is the only claimant. A local `mods:` folder or `.fpak` archive — the same `from_mods_vfs`
  loader, and the only loader on Android — restores by manifest id, as does a `dir:` mount. A contested id also
  receives no entry. Both load-before-restore and restore-before-load hold a Workshop or contested entry under its
  original manifest id (ADR-7992). There is no separate trust flag; the Workshop mount id is the signal.
- Held entries win over a claimant's current capture in every save. Their bytes are reserved first. A live mod that
  does not fit in what remains is left out with `lua_too_large` on its own line, and the save continues. The save
  returns empty bytes only when the held entries alone pass `max_state_total_bytes`, and keeps them for a retry.
  Callers must retain the previous save on that failure. The plugin registers a required save section and propagates
  that failure through `SaveGame` before it replaces file/cloud storage. An uncontested local claimant, including a
  local `mods:` mount, can restore the preserved entry.
- A restored entry whose id contains `:` is not a v1 manifest id. It is quarantined with a warning and left out of
  the save, so one such entry does not fail later game saves.
- Limitation: v1 has no mount ownership field, so even the original Workshop item cannot safely restore its entry.
  Local state is still per manifest id; simultaneous duplicate manifest claims remain refused.
- A hot reload keeps the mod's mount whatever files it is given.
- `lua.list` reports `mount?`, the v1 manifest `save_key`, `contested_ids` and `held_state`.

## 15. Host and presentation runtimes, host commands, snapshots (ADR-8466)

- **Sides.** `ScriptAuthority::{Host, Presentation}` on `LuaMods` (`with_authority`) and `LuaModsPlugin`
  (`with_authority`); default `Host`. `regia.authority` is `"host"` or `"presentation"`.
- **Host commands.** Host states only get `regia.command(name, value?) → true | false, reason`. Reasons:
  `unknown_command` (name not in `HostCommandRules::names`; the game registers them), `bad_value` (not plain data, or
  a non-finite number), `too_large` (`max_command_bytes`, default 4,096), `queue_full` (`max_pending_per_mod`,
  default 32; a setting, never a constant). A refusal is counted, never raises, never disables. The queue is per mod;
  `LuaMods::drain_host_commands` takes it in mod id order then queue order (deterministic: no clock, no hash order).
  A failing call drops the commands it queued. Restart and hot reload carry undrained commands ahead of the new
  version's; a save restore clears them, like the event queues. The engine never applies a command: the game maps
  each onto its own command path (and may refuse it there).
- **Presentation.** No `regia.command` in the table at all, so no script, `pcall` or stored function reaches the
  queue (`presentation_script_cannot_enqueue_host_commands`).
- **Snapshots.** `LuaMods::publish_snapshot(world, ScriptValue) → Result<bool>`: refused over `MAX_SNAPSHOT_BYTES`
  (16 MiB, `lua_too_large`) or with a non-finite number (`lua_bad_value`). An equal snapshot is `Ok(false)`: no hook,
  no rebuild. A changed one bumps the generation and calls every active mod's `on_snapshot(snapshot)` in id order;
  `regia.snapshot()` returns the same table. Each mod builds its table at most once per generation, charged to the
  current call (64 instructions plus one per 256 bytes); writes to it stay in that mod.
- **Agent.** `lua.authority` (read): side, names, caps, waiting commands (up to 64, not drained), per-mod refusals and
  snapshot builds (`book/agents/lua-mods.md`).
- **Tests.** `crates/regia_script_lua/tests/authority.rs`; `agent::tests::the_authority_method_reports_the_queue_without_draining_it`.
