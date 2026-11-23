// SPDX-License-Identifier: MPL-2.0
//! Embedded Lua 5.1 for mods (`docs/specs/84-lua-mods.md`, ADR-1751).
//!
//! Optional: the umbrella crate's `lua` feature (`regia = { features = ["lua"] }`) re-exports this
//! crate as `regia::lua`. Without it no Lua is compiled.
//!
//! Every mod runs in its own sandboxed Lua state:
//!
//! - **No host access by default.** No `io`, `os`, `package` or `debug`; no `load`, `loadstring`,
//!   `dofile`, `loadfile` or `string.dump`; precompiled chunks are refused. `require` reads only the
//!   mod's own files ([`ModFiles`]).
//! - **Budgets.** An instruction budget per call and a memory cap per state ([`LuaLimits`]). A
//!   budget error cannot be caught by the script (`pcall`, `xpcall` and `coroutine.resume`
//!   re-raise it). Host work is charged to the budget: pattern matching (the `string` functions
//!   run in a budgeted matcher), `regia.find`, and event delivery, one budget per pass. A
//!   millisecond watchdog is the last resort for work no count can see.
//! - **Error isolation.** Any error disables that mod only ([`ModStatus::Disabled`]); the game and
//!   the other mods keep running. [`LuaMods::restart`] rebuilds it.
//! - **Determinism.** `math.random` is the engine's PCG32 seeded from the host seed and the mod id;
//!   there is no clock. Mods run in id order.
//!
//! The host API is the `regia` table (API version [`API_VERSION`]): entity handles that go stale
//! safely, `vec3` values, game events in both directions. World access goes through [`ModWorld`]
//! ([`EcsModWorld`] for the ECS). [`LuaModsPlugin`] wires it into an app and registers the
//! `lua.*` agent methods (`book/agents/lua-mods.md`).
//!
//! A host runs on one side of a networked game ([`ScriptAuthority`], ADR-8466). On the host side
//! `regia.command(name, value)` queues a request in a bounded per-mod queue the game drains with
//! [`LuaMods::drain_host_commands`]; a presentation (client) side has no `regia.command`. The game
//! publishes what scripts read with [`LuaMods::publish_snapshot`] (`regia.snapshot()` and the
//! `on_snapshot` hook); an unchanged snapshot rebuilds nothing.
//!
//! `regia.state` is a mod's plain-data state. It goes into save games
//! ([`LuaMods::save_state`], format `luastate`, a section of the save file) and across a hot
//! reload ([`LuaMods::reload`], which rolls back on any failure), through the mod's `on_migrate`
//! when its `regia.state_version` changed.
//!
//! ```
//! use regia_script_lua::{LuaLimits, LuaMods, ModFiles, NoWorld};
//!
//! let files = ModFiles::from_map([
//!     (
//!         "luamod.ron",
//!         r#"(format_version: 1, id: "demo", name: "Demo", version: "1.0.0", entry: "main.lua", api_version: 1)"#,
//!     ),
//!     ("main.lua", "function on_tick(dt) regia.emit('ticked', dt) end"),
//! ]);
//! let mut mods = LuaMods::new(7, LuaLimits::default());
//! mods.load(files, &mut NoWorld).expect("the mod loads");
//! mods.call_tick(&mut NoWorld, 0.5);
//! let events = mods.drain_emitted();
//! assert_eq!(events[0].name, "ticked");
//! ```

#![forbid(unsafe_code)]

mod agent;
mod authority;
mod components;
mod ecs;
mod error;
mod files;
/// The Lua host's fuzz targets (ADR-2084), added to the agent's fuzz registry by [`LuaModsPlugin`].
pub mod fuzz;
mod host;
mod manifest;
mod pattern;
mod sandbox;
mod state;
mod value;
mod world;

pub use authority::{
    DEFAULT_MAX_COMMAND_BYTES, DEFAULT_MAX_PENDING_COMMANDS, HostCommand, HostCommandRules,
    MAX_COMMAND_NAME_BYTES, MAX_SNAPSHOT_BYTES, ScriptAuthority,
};
pub use components::{
    ComponentDecl, LuaComponent, LuaValue, MAX_DECL_BYTES, MAX_DECLS_PER_MOD, MAX_ENUM_OPTION_BYTES,
    MAX_ENUM_OPTIONS, MAX_NAME_BYTES, MAX_PROPS_PER_DECL, PropDecl, PropType,
};
pub use ecs::{EcsModWorld, LuaModsPlugin, LuaSpawned};
pub use error::{LuaErrorCode, ModError};
pub use files::{FilesError, ModFiles, mod_file_body_reads, reset_mod_file_body_reads};
pub use host::{
    EmittedEvent, GlobalEntry, LuaMod, LuaMods, MAX_CHUNK_BYTES, MAX_EMITTED_HELD, MAX_EMITTED_HELD_BYTES,
    MAX_ERRORS_KEPT, MAX_HANDLERS_PER_EVENT, ModCounters, ModRestoreLine, ModSaveLine, ModStatus,
    ReloadOutcome, ReloadStage, RestoreReport, RunOutcome, SaveReport,
};
pub use manifest::{
    LUAMOD_FILE, LUAMOD_FORMAT_VERSION, LuaModManifest, ManifestError, luamod_migration_sources,
    luamod_schema_fingerprint,
};
pub use sandbox::{DEFAULT_MAX_ENTITIES_PER_MOD, LuaLimits};
pub use state::{
    LUA_STATE_SECTION, LUASTATE_FORMAT_VERSION, LUASTATE_MAGIC, LuaStateFile, MAX_STATE_DEPTH, ModState,
    SavedModEntity, StateCarry, StateKey, StateTree, StateValue, decode as decode_luastate,
    luastate_migration_sources, luastate_schema_fingerprint,
};
pub use value::{
    MAX_DEPTH, MAX_ITEMS, MAX_STRING_BYTES, MAX_VALUE_BYTES, ScriptValue, reset_vec3_userdata_creates,
    vec3_userdata_creates,
};
pub use world::{ModWorld, NoWorld};

/// The version of the `regia` table mods are written against (`luamod.ron` `api_version`).
pub const API_VERSION: u32 = 1;

/// The Lua 5.1.5 copyright and licence text, for a game's credits or notices screen (ADR-1751 § 10).
pub const LUA_LICENSE_NOTICE: &str = "Lua 5.1.5\n\
Copyright (C) 1994-2012 Lua.org, PUC-Rio.\n\
\n\
Permission is hereby granted, free of charge, to any person obtaining a copy of this software and \
associated documentation files (the \"Software\"), to deal in the Software without restriction, \
including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, \
and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so, \
subject to the following conditions:\n\
\n\
The above copyright notice and this permission notice shall be included in all copies or substantial \
portions of the Software.\n\
\n\
THE SOFTWARE IS PROVIDED \"AS IS\", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT \
LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN \
NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, \
WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE \
SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.\n";

/// Longest mount name `ModFiles::with_mount` takes (ADR-7881).
pub use files::MAX_MOUNT_BYTES;
